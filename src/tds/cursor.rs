//! Server cursors over TDS: `sp_cursoropen`, `sp_cursorprepare`, `sp_cursorexecute`, `sp_cursorprepexec`,
//! `sp_cursorfetch`, `sp_cursoroption`, `sp_cursorclose`, `sp_cursorunprepare` and `sp_cursor`, which the OLE DB, ODBC
//! and JDBC drivers use for scrollable result sets.
//!
//! Opening a cursor runs the statement and keeps its rows; fetches page through them. A cursor of the kind a client asks
//! for a *sensitive* result set (KEYSET, or DYNAMIC, which is served as keyset) over one table with a key is a **keyset
//! cursor**: it keeps the key of each row, the membership is fixed at open, and every fetch reads the rows of the block again
//! by key, so it shows the changes other sessions and the cursor itself have made (a row deleted since is flagged
//! `rowstat` 2). Such a cursor can be **updatable**: `sp_cursor` updates, deletes, inserts and refreshes the row at a
//! position, with an optimistic check (the row must be as the cursor last fetched it, else error 16934). Any other cursor
//! (STATIC, or a statement that is not a plain read of one table that returns its key) is a snapshot and read-only; the
//! open call returns the type and concurrency actually in force, as the protocol allows. A DYNAMIC cursor does not see rows
//! inserted since it opened.
use super::*;
use rpc::Param;
use std::collections::HashMap;

/// Handles of cursors count up from here (SQL Server's are large numbers as well).
const FIRST_HANDLE: i32 = 180_150_000;

const SCROLL_KEYSET: i32 = 1;
const SCROLL_DYNAMIC: i32 = 2;
const SCROLL_FORWARD_ONLY: i32 = 4;
const SCROLL_STATIC: i32 = 8;
const SCROLL_FAST_FORWARD: i32 = 0x10;
const SCROLL_AUTO_FETCH: i32 = 0x2000;
const SCROLL_AUTO_CLOSE: i32 = 0x4000;
const SCROLL_CHECK_ACCEPTED: i32 = 0x8000;
const SCROLL_FORWARD_ACCEPTABLE: i32 = 0x40000;
const SCROLL_STATIC_ACCEPTABLE: i32 = 0x80000;

const CC_READ_ONLY: i32 = 1;

const FETCH_FIRST: i32 = 0x01;
const FETCH_NEXT: i32 = 0x02;
const FETCH_PREV: i32 = 0x04;
const FETCH_LAST: i32 = 0x08;
const FETCH_ABSOLUTE: i32 = 0x10;
const FETCH_RELATIVE: i32 = 0x20;
const FETCH_REFRESH: i32 = 0x80;
const FETCH_INFO: i32 = 0x100;
const FETCH_PREV_NOADJUST: i32 = 0x200;

/// The rows `sp_cursorfetch` returns when the client does not say how many.
const DEFAULT_FETCH_ROWS: i32 = 20;

/// What a keyset cursor knows about the table it reads.
struct Keyset {
    /// `[schema].[table]`
    table: String,
    /// The table's name in parts (schema, table), for the TABNAME token.
    table_parts: Vec<String>,
    /// The result columns, bracketed.
    cols: Vec<String>,
    /// The key columns, as positions in the result columns.
    key: Vec<usize>,
    /// The position among the key columns of an identity column (a row inserted without it is found by `@@IDENTITY`).
    identity_key: Option<usize>,
    /// The cursor accepts `sp_cursor` changes.
    updatable: bool,
    /// Rows that were found deleted (or deleted through the cursor).
    deleted: Vec<bool>,
}

pub struct Cursor {
    keyset: Option<Keyset>,
    names: Vec<String>,
    wires: Vec<Wire>,
    rows: Vec<Vec<Value>>,
    /// First and last row (1-based) of the block fetched last; both 0 before the first row, both rows+1 after the last.
    start: usize,
    last: usize,
    /// The first row (1-based) of the block the last fetch returned: the row numbers of `sp_cursor` count from it.
    block_start: usize,
    forward_only: bool,
    auto_close: bool,
}

fn int_param(ps: &[Param], i: usize, default: i32) -> i32 {
    match ps.get(i).map(|p| &p.val) {
        Some(Value::Null) | None => default,
        Some(v) => convert_i32(v).unwrap_or(default),
    }
}

fn convert_i32(v: &Value) -> Option<i32> {
    match v {
        Value::Int(i) => Some(*i),
        Value::BigInt(i) => Some(*i as i32),
        Value::Bit(b) => Some(*b as i32),
        Value::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Write parameter `i` as a RETURNVALUE even when the client did not mark it OUTPUT.
fn ret_always(out: &mut Vec<u8>, ps: &[Param], i: usize, v: Value) {
    if let Some(p) = ps.get(i) {
        return_value(out, i as u16, &p.name, &p.ti.w, &v);
    }
}

/// Write the OUTPUT parameter `i` (when the client declared it one) as a RETURNVALUE.
fn ret(out: &mut Vec<u8>, ps: &[Param], i: usize, v: Value) {
    if let Some(p) = ps.get(i) {
        if p.out {
            return_value(out, i as u16, &p.name, &p.ti.w, &v);
        }
    }
}

impl Session {
    /// A cursor RPC (`id` is its system procedure number). Returns whether it failed.
    pub fn cursor_rpc(&mut self, id: u16, ps: &[Param], out: &mut Vec<u8>) -> R<bool> {
        match id {
            rpc::SP_CURSOROPEN => {
                let stmt = rpc::text_param(ps, 1, "@stmt")?.ok_or("@stmt must not be NULL")?;
                let decl = rpc::text_param(ps, 5, "@paramdef").unwrap_or(None);
                let args: Vec<usize> = (6..ps.len()).collect();
                self.cursor_open(&stmt, decl.as_deref(), ps, &args, [0, 2, 4], None, out)
            }
            rpc::SP_CURSORPREPARE => {
                let decl = rpc::text_param(ps, 1, "@params")?;
                let stmt = rpc::text_param(ps, 2, "@stmt")?.ok_or("@stmt must not be NULL")?;
                self.next_handle += 1;
                let handle = self.next_handle;
                self.prepared.insert(handle, (decl, stmt));
                out.push(0x79);
                out.extend_from_slice(&0i32.to_le_bytes());
                ret(out, ps, 0, Value::Int(handle));
                // the cursor type and concurrency in force are settled when it opens
                let scroll = int_param(ps, 4, SCROLL_STATIC);
                ret(out, ps, 4, Value::Int(settle_scroll(scroll)));
                ret(out, ps, 5, Value::Int(CC_READ_ONLY));
                Ok(false)
            }
            rpc::SP_CURSOREXECUTE => {
                let handle = int_param(ps, 0, 0);
                let (decl, stmt) = self.prepared.get(&handle).cloned().ok_or_else(|| format!("The prepared statement handle {} is not valid in this context.", handle))?;
                let args: Vec<usize> = (5..ps.len()).collect();
                self.cursor_open(&stmt, decl.as_deref(), ps, &args, [1, 2, 4], None, out)
            }
            rpc::SP_CURSORPREPEXEC => {
                let decl = rpc::text_param(ps, 2, "@params")?;
                let stmt = rpc::text_param(ps, 3, "@stmt")?.ok_or("@stmt must not be NULL")?;
                self.next_handle += 1;
                let handle = self.next_handle;
                self.prepared.insert(handle, (decl.clone(), stmt.clone()));
                let args: Vec<usize> = (7..ps.len()).collect();
                let errored = self.cursor_open(&stmt, decl.as_deref(), ps, &args, [1, 4, 6], Some(handle), out)?;
                if errored {
                    self.prepared.remove(&handle);
                }
                Ok(errored)
            }
            rpc::SP_CURSORFETCH => self.cursor_fetch(ps, out),
            rpc::SP_CURSOROPTION => {
                // text pointer and name options do not change anything here
                out.push(0x79);
                out.extend_from_slice(&0i32.to_le_bytes());
                Ok(false)
            }
            rpc::SP_CURSORCLOSE => {
                let h = int_param(ps, 0, 0);
                if self.cursors.remove(&h).is_none() {
                    error_token(out, &SqlError::new(16916, format!("A cursor with the name '{}' does not exist.", h)));
                    return Ok(true);
                }
                out.push(0x79);
                out.extend_from_slice(&0i32.to_le_bytes());
                Ok(false)
            }
            rpc::SP_CURSORUNPREPARE => {
                let h = int_param(ps, 0, 0);
                self.prepared.remove(&h);
                out.push(0x79);
                out.extend_from_slice(&0i32.to_le_bytes());
                Ok(false)
            }
            rpc::SP_CURSOR => self.cursor_change(ps, out),
            other => Err(format!("The remote procedure call (system procedure id {}) is not supported.", other)),
        }
    }

    /// Run `stmt` and keep its rows as a new cursor. `at` holds the indexes of the parameters that take the cursor handle,
    /// the cursor type (the concurrency follows it) and the row count; `prepared` is the handle `sp_cursorprepexec` returns.
    fn cursor_open(&mut self, stmt: &str, decl: Option<&str>, ps: &[Param], args: &[usize], at: [usize; 3], prepared: Option<i32>, out: &mut Vec<u8>) -> R<bool> {
        let [cursor_at, scroll_at, rows_at] = at;
        let cc_at = scroll_at + 1;
        let plan = rpc::plan_executesql(stmt, decl, ps, args)?;
        let res = self.conn.execute(&plan.sql);
        if let Some(e) = res.first_error() {
            error_token(out, e);
            return Ok(true);
        }
        let Some(rs) = res.resultsets().into_iter().find(|r| r.columns.first().map(|c| c != rpc::TRAILER).unwrap_or(false)).cloned() else {
            error_token(out, &SqlError::new(16945, "The cursor was not declared: the statement does not return a result set."));
            return Ok(true);
        };
        let scroll_req = int_param(ps, scroll_at, SCROLL_STATIC);
        let cc_req = int_param(ps, cc_at, CC_READ_ONLY);
        let kind = scroll_req & 0x1F;
        let wants_updates = cc_req & 0x0E != 0 && cc_req & CC_READ_ONLY == 0;
        // a cursor that must see changes (or be updated) is a keyset cursor when the statement allows it
        let mut keyset = if kind & SCROLL_FAST_FORWARD == 0 && (kind & (SCROLL_KEYSET | SCROLL_DYNAMIC) != 0 || (kind & SCROLL_FORWARD_ONLY != 0 && wants_updates)) { analyze(&self.conn, stmt, &rs.columns, &rs.rows) } else { None };
        let scroll = match &keyset {
            Some(_) if kind & SCROLL_FORWARD_ONLY != 0 => SCROLL_FORWARD_ONLY,
            Some(_) => SCROLL_KEYSET,
            None => settle_scroll(scroll_req),
        };
        if let Some(k) = keyset.as_mut() {
            k.updatable = wants_updates;
        }
        // an updatable cursor gives back the concurrency that was asked for (optimistic, with the flags the client set)
        let cc = if keyset.as_ref().is_some_and(|k| k.updatable) { cc_req } else { CC_READ_ONLY };
        let wires = wires_for(&rs);
        let n = rs.rows.len();
        self.next_cursor += 1;
        let handle = FIRST_HANDLE + self.next_cursor;
        let cur = Cursor { keyset, names: rs.columns.clone(), wires, rows: rs.rows, block_start: 0, start: 0, last: 0, forward_only: scroll & (SCROLL_FORWARD_ONLY | SCROLL_FAST_FORWARD) != 0, auto_close: scroll_req & SCROLL_AUTO_CLOSE != 0 };
        cursor_metadata(out, &cur);
        done(out, 0xFF, DONE_MORE, 0);
        let mut cur = cur;
        if let Some(k) = cur.keyset.as_mut() {
            k.deleted = vec![false; cur.rows.len()];
        }
        let mut live = true;
        if scroll_req & SCROLL_AUTO_FETCH != 0 {
            // the first block comes with the open call
            let want = int_param(ps, rows_at, DEFAULT_FETCH_ROWS);
            let want = if want > 0 { want } else { DEFAULT_FETCH_ROWS };
            fetch_block(&mut cur, FETCH_FIRST, 0, want, out);
            if cur.auto_close && cur.last >= cur.rows.len() {
                live = false;
            }
        }
        out.push(0x79);
        out.extend_from_slice(&0i32.to_le_bytes());
        // the OUTPUT parameters come back in parameter order: the prepared handle (when there is one) first
        if let Some(h) = prepared {
            ret(out, ps, 0, Value::Int(h));
        }
        ret(out, ps, cursor_at, Value::Int(if live { handle } else { 0 }));
        // the cursor type and concurrency in force are reported whether or not the client marked them OUTPUT (the Microsoft JDBC
        // driver does not, and reads them)
        ret_always(out, ps, scroll_at, Value::Int(scroll));
        ret_always(out, ps, cc_at, Value::Int(cc));
        ret(out, ps, rows_at, Value::Int(n as i32));
        if live {
            self.cursors.insert(handle, cur);
        }
        Ok(false)
    }

    fn cursor_fetch(&mut self, ps: &[Param], out: &mut Vec<u8>) -> R<bool> {
        let h = int_param(ps, 0, 0);
        let ftype = int_param(ps, 1, FETCH_NEXT);
        let rownum = int_param(ps, 2, 0);
        let nrows = int_param(ps, 3, DEFAULT_FETCH_ROWS);
        let Some(mut cur) = self.cursors.remove(&h) else {
            error_token(out, &SqlError::new(16916, format!("A cursor with the name '{}' does not exist.", h)));
            return Ok(true);
        };
        if cur.forward_only && ftype & (FETCH_PREV | FETCH_LAST | FETCH_ABSOLUTE | FETCH_RELATIVE | FETCH_PREV_NOADJUST) != 0 {
            error_token(out, &SqlError::new(16911, "Fetch type is not valid for a forward only cursor."));
            self.cursors.insert(h, cur);
            return Ok(true);
        }
        let (s, count) = position(&mut cur, ftype, rownum, nrows);
        // a keyset cursor reads the rows of the block again: it shows what has changed
        if cur.keyset.is_some() && count > 0 {
            refresh(&mut self.conn, &mut cur, (s - 1) as usize, count as usize);
        }
        let first = write_block(&cur, s, count, out);
        let total = cur.rows.len();
        let close = cur.auto_close && cur.last >= cur.rows.len() && cur.start > 0;
        out.push(0x79);
        out.extend_from_slice(&0i32.to_le_bytes());
        // the row number of the first row fetched and the number of rows in the cursor
        ret(out, ps, 2, Value::Int(first as i32));
        ret(out, ps, 3, Value::Int(total as i32));
        if !close {
            self.cursors.insert(h, cur);
        }
        Ok(false)
    }

    /// `sp_cursor`: a positioned change of the keyset cursor `ps[0]`.
    fn cursor_change(&mut self, ps: &[Param], out: &mut Vec<u8>) -> R<bool> {
        let h = int_param(ps, 0, 0);
        let op = int_param(ps, 1, 0);
        let rownum = int_param(ps, 2, 0);
        let Some(mut cur) = self.cursors.remove(&h) else {
            error_token(out, &SqlError::new(16916, format!("A cursor with the name '{}' does not exist.", h)));
            return Ok(true);
        };
        let r = self.cursor_change_inner(&mut cur, op, rownum, ps, out);
        self.cursors.insert(h, cur);
        match r {
            Ok(()) => {
                out.push(0x79);
                out.extend_from_slice(&0i32.to_le_bytes());
                Ok(false)
            }
            Err(e) => {
                error_token(out, &e);
                Ok(true)
            }
        }
    }

    fn cursor_change_inner(&mut self, cur: &mut Cursor, op: i32, rownum: i32, ps: &[Param], _out: &mut Vec<u8>) -> Result<(), SqlError> {
        if tracing() {
            trace(&format!("sp_cursor: operation {}, row {}, {} value(s)", op, rownum, ps.len().saturating_sub(4)));
        }
        let Some(ks) = cur.keyset.as_ref().filter(|k| k.updatable) else {
            return Err(SqlError::new(16929, "The cursor is READ ONLY: positioned updates, deletes and inserts are not supported."));
        };
        // the values to write: `@column = value` parameters, or the columns in order
        let sets: Vec<(String, String)> = ps
            .iter()
            .skip(4)
            .enumerate()
            .filter_map(|(j, p)| {
                let name = p.name.trim_start_matches('@');
                let col = if name.is_empty() { cur.names.get(j).cloned()? } else { cur.names.iter().find(|c| c.eq_ignore_ascii_case(name)).cloned().unwrap_or_else(|| name.to_string()) };
                Some((crate::tools::bracket(&col), rpc::literal(&p.val)))
            })
            .collect();
        let row_idx = |cur: &Cursor| -> Result<usize, SqlError> {
            let i = (cur.block_start.max(1) - 1) + (rownum.max(1) as usize - 1);
            if i < cur.rows.len() {
                Ok(i)
            } else {
                Err(SqlError::new(16933, "The cursor does not include the row requested."))
            }
        };
        match op {
            OP_REFRESH => {
                let i = row_idx(cur)?;
                refresh(&mut self.conn, cur, i, 1);
                Ok(())
            }
            OP_UPDATE | OP_DELETE => {
                let i = row_idx(cur)?;
                if ks.deleted[i] {
                    return Err(SqlError::new(16933, "The cursor does not include the row requested."));
                }
                // optimistic concurrency: the row must be as it was fetched
                match read_row(&mut self.conn, cur, ks, i) {
                    None => return Err(SqlError::new(16933, "The cursor does not include the row requested.")),
                    Some(now) if format!("{:?}", now) != format!("{:?}", cur.rows[i]) => return Err(SqlError::new(16934, "Optimistic concurrency check failed. The row was modified outside of this cursor.")),
                    Some(_) => {}
                }
                let cond = key_condition(cur, ks, &[i]);
                let sql = if op == OP_UPDATE {
                    if sets.is_empty() {
                        return Ok(());
                    }
                    format!("UPDATE {} SET {} WHERE {}", ks.table, sets.iter().map(|(c, v)| format!("{} = {}", c, v)).collect::<Vec<_>>().join(", "), cond)
                } else {
                    format!("DELETE FROM {} WHERE {}", ks.table, cond)
                };
                let res = self.conn.execute(&sql);
                if let Some(e) = res.first_error() {
                    return Err(e.clone());
                }
                if op == OP_DELETE {
                    cur.keyset.as_mut().unwrap().deleted[i] = true;
                } else {
                    refresh(&mut self.conn, cur, i, 1);
                }
                Ok(())
            }
            OP_INSERT => {
                let sql = if sets.is_empty() { format!("INSERT {} DEFAULT VALUES", ks.table) } else { format!("INSERT {} ({}) VALUES ({})", ks.table, sets.iter().map(|s| s.0.clone()).collect::<Vec<_>>().join(", "), sets.iter().map(|s| s.1.clone()).collect::<Vec<_>>().join(", ")) };
                // the key of the new row: given, or the identity value it received
                let given: Option<Vec<Value>> = ks.key.iter().map(|k| ps.iter().skip(4).enumerate().find(|(j, p)| { let n = p.name.trim_start_matches('@'); if n.is_empty() { cur.names.get(*j) == Some(&cur.names[*k]) } else { n.eq_ignore_ascii_case(&cur.names[*k]) } }).map(|(_, p)| p.val.clone())).collect();
                let res = self.conn.execute(&format!("{}; SELECT @@IDENTITY", sql));
                if let Some(e) = res.first_error() {
                    return Err(e.clone());
                }
                let identity = res.resultsets().last().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().filter(|v| !v.is_null());
                let key: Option<Vec<Value>> = match (given, &identity, ks.identity_key) {
                    (Some(k), _, _) => Some(k),
                    (None, Some(id), Some(pos)) if ks.key.len() == 1 && pos == 0 => Some(vec![id.clone()]),
                    _ => None,
                };
                if let Some(key) = key {
                    // read the new row and add it to the cursor
                    let mut probe = vec![Value::Null; cur.names.len()];
                    for (k, v) in ks.key.iter().zip(key) {
                        probe[*k] = v;
                    }
                    cur.rows.push(probe);
                    let i = cur.rows.len() - 1;
                    cur.keyset.as_mut().unwrap().deleted.push(false);
                    // (the identity value comes back as a decimal: read the row by its key rather than matching on the type)
                    let ks = cur.keyset.as_ref().unwrap();
                    match read_row(&mut self.conn, cur, ks, i) {
                        Some(row) => cur.rows[i] = row,
                        None => cur.keyset.as_mut().unwrap().deleted[i] = true,
                    }
                }
                Ok(())
            }
            other => Err(SqlError::new(16902, format!("The cursor operation {} is not supported.", other))),
        }
    }
}

const OP_REFRESH: i32 = 1;
const OP_INSERT: i32 = 4;
const OP_UPDATE: i32 = 33;
const OP_DELETE: i32 = 34;

/// Whether `stmt` is a plain read of one table that returns its key, so that its rows can be found again by key. The
/// result columns are `cols`, the rows `rows` (the key columns of an identity-free table need all be NOT NULL).
fn analyze(conn: &Connection, stmt: &str, cols: &[String], rows: &[Vec<Value>]) -> Option<Keyset> {
    let r = analyze_inner(conn, stmt, cols, rows);
    if tracing() {
        trace(&format!("keyset cursor over {:?}: {}", stmt, if r.is_some() { "yes" } else { "no (not a plain read of one table that returns its key)" }));
    }
    r
}

fn analyze_inner(conn: &Connection, stmt: &str, cols: &[String], rows: &[Vec<Value>]) -> Option<Keyset> {
    use crate::ast::{Expr, QueryBody, SelectItem, Stmt, TableRef};
    let stmts = crate::parser::parse(stmt).ok()?;
    let [Stmt::Query(q)] = &stmts[..] else { return None };
    if !q.ctes.is_empty() || q.for_clause.is_some() {
        return None;
    }
    let QueryBody::Select(sel) = &q.body else { return None };
    if sel.distinct || sel.group.is_some() || sel.having.is_some() || sel.into.is_some() || sel.from.len() != 1 {
        return None;
    }
    let TableRef::Table { name, .. } = &sel.from[0] else { return None };
    for item in &sel.items {
        match item {
            SelectItem::Star(None) => {}
            SelectItem::Expr { e: Expr::Col(parts), alias } if alias.as_ref().is_none_or(|a| parts.last().is_some_and(|p| p.eq_ignore_ascii_case(a))) => {}
            _ => return None,
        }
    }
    let info = conn.table_info(&name.iter().map(|p| crate::tools::bracket(p)).collect::<Vec<_>>().join("."))?;
    if !cols.iter().all(|c| info.columns.iter().any(|x| x.name.eq_ignore_ascii_case(c))) {
        return None;
    }
    // the key: the primary key, or a unique constraint or index of NOT NULL columns
    let not_null = |names: &[String]| names.iter().all(|n| info.columns.iter().any(|c| c.name.eq_ignore_ascii_case(n) && !c.nullable));
    let key_names: Vec<String> = match info.indexes.iter().find(|i| i.primary_key).or_else(|| info.indexes.iter().find(|i| i.unique && i.filter.is_none() && not_null(&i.columns.iter().map(|c| c.0.clone()).collect::<Vec<_>>()))) {
        Some(i) => i.columns.iter().map(|c| c.0.clone()).collect(),
        None => return None,
    };
    let key: Vec<usize> = key_names.iter().map(|k| cols.iter().position(|c| c.eq_ignore_ascii_case(k))).collect::<Option<Vec<_>>>()?;
    let identity_key = key_names.iter().position(|k| info.columns.iter().any(|c| c.name.eq_ignore_ascii_case(k) && c.identity.is_some()));
    let _ = rows;
    Some(Keyset { table_parts: vec![info.schema.clone(), info.name.clone()], table: info.quoted(), cols: cols.iter().map(|c| crate::tools::bracket(c)).collect(), key, identity_key, updatable: false, deleted: vec![] })
}

/// The condition that selects the rows `idx` of the cursor by key.
fn key_condition(cur: &Cursor, ks: &Keyset, idx: &[usize]) -> String {
    let one = |i: usize| ks.key.iter().map(|k| format!("{} = {}", ks.cols[*k], rpc::literal(&cur.rows[i][*k]))).collect::<Vec<_>>().join(" AND ");
    if ks.key.len() == 1 && idx.len() > 1 {
        let k = ks.key[0];
        return format!("{} IN ({})", ks.cols[k], idx.iter().map(|i| rpc::literal(&cur.rows[*i][k])).collect::<Vec<_>>().join(", "));
    }
    idx.iter().map(|i| format!("({})", one(*i))).collect::<Vec<_>>().join(" OR ")
}

fn key_text(row: &[Value], key: &[usize]) -> String {
    key.iter().map(|k| format!("{:?}", row[*k])).collect::<Vec<_>>().join("|")
}

/// The current values of row `i` of the cursor, read by key; None when the row is gone.
fn read_row(conn: &mut Connection, cur: &Cursor, ks: &Keyset, i: usize) -> Option<Vec<Value>> {
    let sql = format!("SELECT {} FROM {} WHERE {}", ks.cols.join(", "), ks.table, key_condition(cur, ks, &[i]));
    let res = conn.execute(&sql);
    if res.first_error().is_some() {
        return None;
    }
    res.resultsets().first().and_then(|r| r.rows.first().cloned())
}

/// Read rows `from..from + count` of a keyset cursor again: their values are the current ones, and a row that no longer
/// exists is marked deleted (it keeps its last values).
fn refresh(conn: &mut Connection, cur: &mut Cursor, from: usize, count: usize) {
    let Some(ks) = cur.keyset.as_ref() else { return };
    let idx: Vec<usize> = (from..(from + count).min(cur.rows.len())).filter(|i| !ks.deleted.get(*i).copied().unwrap_or(false)).collect();
    if idx.is_empty() {
        return;
    }
    let sql = format!("SELECT {} FROM {} WHERE {}", ks.cols.join(", "), ks.table, key_condition(cur, ks, &idx));
    let res = conn.execute(&sql);
    let Some(rs) = res.resultsets().first().filter(|_| res.first_error().is_none()).cloned().cloned() else { return };
    let ks = cur.keyset.as_ref().unwrap();
    let mut found: HashMap<String, Vec<Value>> = HashMap::new();
    for r in rs.rows {
        found.insert(key_text(&r, &ks.key), r);
    }
    let key = ks.key.clone();
    for i in idx {
        match found.remove(&key_text(&cur.rows[i], &key)) {
            Some(r) => cur.rows[i] = r,
            None => cur.keyset.as_mut().unwrap().deleted[i] = true,
        }
    }
}

/// The cursor type this server gives a request: keyset and dynamic cursors are static ones; a request for a forward-only
/// or fast-forward cursor is kept; when the client lists the types it accepts, one of those is chosen.
fn settle_scroll(requested: i32) -> i32 {
    let asked = requested & 0x1F;
    let acceptable = |t: i32| -> bool {
        if requested & SCROLL_CHECK_ACCEPTED == 0 {
            return true;
        }
        match t {
            SCROLL_STATIC => requested & SCROLL_STATIC_ACCEPTABLE != 0 || requested & 0x1F0000 == 0,
            _ => requested & SCROLL_FORWARD_ACCEPTABLE != 0 || requested & 0x1F0000 == 0,
        }
    };
    let want = if asked & SCROLL_FAST_FORWARD != 0 {
        SCROLL_FAST_FORWARD
    } else if asked & SCROLL_FORWARD_ONLY != 0 {
        SCROLL_FORWARD_ONLY
    } else if asked & (SCROLL_KEYSET | SCROLL_DYNAMIC | SCROLL_STATIC) != 0 {
        SCROLL_STATIC
    } else {
        SCROLL_STATIC
    };
    if acceptable(want) {
        want
    } else if acceptable(SCROLL_STATIC) {
        SCROLL_STATIC
    } else {
        SCROLL_FORWARD_ONLY
    }
}

/// The columns of a cursor's result set as the client sees them, in the open call and in every fetch: a hidden last
/// column, `rowstat` (the Microsoft JDBC driver skips the last column of a cursor's result set).
fn cursor_metadata(out: &mut Vec<u8>, cur: &Cursor) {
    let mut names = cur.names.clone();
    names.push("rowstat".to_string());
    let mut wires = cur.wires.clone();
    wires.push(Wire::Int(4));
    // the columns of an updatable cursor say so (the Microsoft JDBC driver refuses to update a column otherwise)
    let updatable = cur.keyset.as_ref().is_some_and(|k| k.updatable);
    let mut flags = vec![if updatable { 0x0005u16 } else { 0x0009 }; cur.names.len()];
    flags.push(0x0000);
    colmetadata_flags(out, &names, &wires, &flags);
    if let Some(ks) = &cur.keyset {
        // browse information, as SQL Server sends for a cursor: TABNAME (the table) and COLINFO (which table each column
        // comes from, which are key columns, and that rowstat is hidden)
        let mut t = vec![ks.table_parts.len() as u8];
        for p in &ks.table_parts {
            t.extend_from_slice(&(p.encode_utf16().count() as u16).to_le_bytes());
            t.extend(p.encode_utf16().flat_map(|u| u.to_le_bytes()));
        }
        out.push(0xA4);
        out.extend_from_slice(&(t.len() as u16).to_le_bytes());
        out.extend_from_slice(&t);
        let mut c = vec![];
        for i in 0..cur.names.len() {
            c.extend_from_slice(&[i as u8 + 1, 1, if ks.key.contains(&i) { 0x08 } else { 0 }]);
        }
        c.extend_from_slice(&[cur.names.len() as u8 + 1, 1, 0x10]);
        out.push(0xA5);
        out.extend_from_slice(&(c.len() as u16).to_le_bytes());
        out.extend_from_slice(&c);
    }
}

/// Fetch a block of rows from the cursor and write it as a result set: the cursor's columns and then `rowstat` (1: the row
/// was fetched). Returns the row number of the first row fetched and the cursor's row count.
fn fetch_block(cur: &mut Cursor, ftype: i32, rownum: i32, nrows: i32, out: &mut Vec<u8>) -> (usize, usize) {
    let (s, count) = position(cur, ftype, rownum, nrows);
    let first = write_block(cur, s, count, out);
    (first, cur.rows.len())
}

/// Where a fetch lands: the first row of the block (1-based; out of range means before the first or after the last row) and
/// the number of rows in it. The position of the cursor is moved to the block.
fn position(cur: &mut Cursor, ftype: i32, rownum: i32, nrows: i32) -> (i64, i64) {
    let n = cur.rows.len() as i64;
    let want = nrows.max(0) as i64;
    let start = cur.start as i64;
    let last = cur.last as i64;
    // the first row of the block (1-based; out of 1..=n means before the first or after the last row)
    let s: i64 = if ftype & FETCH_FIRST != 0 {
        1
    } else if ftype & FETCH_LAST != 0 {
        (n - want + 1).max(1)
    } else if ftype & FETCH_ABSOLUTE != 0 {
        if rownum >= 0 {
            rownum as i64
        } else {
            n + rownum as i64 + 1
        }
    } else if ftype & FETCH_RELATIVE != 0 {
        start + rownum as i64
    } else if ftype & (FETCH_PREV | FETCH_PREV_NOADJUST) != 0 {
        if start <= 1 {
            0
        } else {
            (start - want).max(1)
        }
    } else if ftype & (FETCH_REFRESH | FETCH_INFO) != 0 {
        start
    } else {
        // NEXT, and anything unknown
        (last + 1).min(n + 1)
    };
    // REFRESH with no row count re-reads the block fetched last (the Microsoft JDBC driver sends it after updating a row)
    let want = if ftype & FETCH_REFRESH != 0 && want == 0 { (last - start + 1).max(1) } else { want };
    let mut count = if (1..=n).contains(&s) && ftype & FETCH_INFO == 0 { want.min(n - s + 1) } else { 0 };
    if ftype & (FETCH_PREV | FETCH_PREV_NOADJUST) != 0 && s >= 1 {
        // the rows before the current block, but no more than the block asked for
        count = count.min(start - s);
    }
    if ftype & FETCH_INFO == 0 {
        if count > 0 {
            cur.start = s as usize;
            cur.last = (s + count - 1) as usize;
        } else if s < 1 {
            cur.start = 0;
            cur.last = 0;
        } else {
            cur.start = (n + 1) as usize;
            cur.last = (n + 1) as usize;
        }
    }
    if ftype & FETCH_INFO == 0 && count > 0 {
        cur.block_start = s as usize;
    }
    (s, count)
}

/// Write the block as a result set: the cursor's columns and then `rowstat` (1: the row was fetched, 2: it has been
/// deleted). Returns the row number of the first row.
fn write_block(cur: &Cursor, s: i64, count: i64, out: &mut Vec<u8>) -> usize {
    let n = cur.rows.len() as i64;
    cursor_metadata(out, cur);
    for i in 0..count {
        let idx = (s - 1 + i) as usize;
        let row = &cur.rows[idx];
        out.push(0xD1);
        for (c, w) in cur.wires.iter().enumerate() {
            write_value(out, w, row.get(c).unwrap_or(&Value::Null));
        }
        let gone = cur.keyset.as_ref().is_some_and(|k| k.deleted.get(idx).copied().unwrap_or(false));
        write_value(out, &Wire::Int(4), &Value::Int(if gone { 2 } else { 1 }));
    }
    done(out, 0xFF, DONE_MORE | DONE_COUNT, count as u64);
    if s >= 1 && s <= n {
        s as usize
    } else {
        0
    }
}
