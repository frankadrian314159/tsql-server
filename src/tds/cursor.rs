//! Server cursors over TDS: `sp_cursoropen`, `sp_cursorprepare`, `sp_cursorexecute`, `sp_cursorprepexec`,
//! `sp_cursorfetch`, `sp_cursoroption`, `sp_cursorclose`, `sp_cursorunprepare` and `sp_cursor`, which the OLE DB, ODBC
//! and JDBC drivers use for scrollable result sets.
//!
//! A cursor here is a snapshot: opening it runs the statement and keeps the rows, and fetches page through them. So
//! every cursor is a STATIC (or, if asked for forward-only or fast-forward, a forward-only) cursor over the rows as they
//! were when it opened, and read-only: a keyset or dynamic cursor is converted to a static one and an updatable one to
//! READ_ONLY, which the protocol allows (the open call returns the type and concurrency actually in force, and the
//! client checks them). Positioned updates through `sp_cursor` are refused.
use super::*;
use rpc::Param;

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

pub struct Cursor {
    names: Vec<String>,
    wires: Vec<Wire>,
    rows: Vec<Vec<Value>>,
    /// First and last row (1-based) of the block fetched last; both 0 before the first row, both rows+1 after the last.
    start: usize,
    last: usize,
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
            rpc::SP_CURSOR => {
                let h = int_param(ps, 0, 0);
                if !self.cursors.contains_key(&h) {
                    error_token(out, &SqlError::new(16916, format!("A cursor with the name '{}' does not exist.", h)));
                } else {
                    error_token(out, &SqlError::new(16929, "The cursor is READ ONLY: positioned updates, deletes and inserts are not supported."));
                }
                Ok(true)
            }
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
        let scroll = settle_scroll(scroll_req);
        let wires = wires_for(&rs);
        let n = rs.rows.len();
        self.next_cursor += 1;
        let handle = FIRST_HANDLE + self.next_cursor;
        let cur = Cursor { names: rs.columns.clone(), wires, rows: rs.rows, start: 0, last: 0, forward_only: scroll & (SCROLL_FORWARD_ONLY | SCROLL_FAST_FORWARD) != 0, auto_close: scroll_req & SCROLL_AUTO_CLOSE != 0 };
        cursor_metadata(out, &cur);
        done(out, 0xFF, DONE_MORE, 0);
        let mut cur = cur;
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
        ret(out, ps, scroll_at, Value::Int(scroll));
        ret(out, ps, cc_at, Value::Int(CC_READ_ONLY));
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
        let Some(cur) = self.cursors.get_mut(&h) else {
            error_token(out, &SqlError::new(16916, format!("A cursor with the name '{}' does not exist.", h)));
            return Ok(true);
        };
        if cur.forward_only && ftype & (FETCH_PREV | FETCH_LAST | FETCH_ABSOLUTE | FETCH_RELATIVE | FETCH_PREV_NOADJUST) != 0 {
            error_token(out, &SqlError::new(16911, "Fetch type is not valid for a forward only cursor."));
            return Ok(true);
        }
        let (first, total) = fetch_block(cur, ftype, rownum, nrows, out);
        let close = cur.auto_close && cur.last >= cur.rows.len() && cur.start > 0;
        out.push(0x79);
        out.extend_from_slice(&0i32.to_le_bytes());
        // the row number of the first row fetched and the number of rows in the cursor
        ret(out, ps, 2, Value::Int(first as i32));
        ret(out, ps, 3, Value::Int(total as i32));
        if close {
            self.cursors.remove(&h);
        }
        Ok(false)
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
    colmetadata(out, &names, &wires);
}

/// Fetch a block of rows from the cursor and write it as a result set: the cursor's columns and then `rowstat` (1: the row
/// was fetched). Returns the row number of the first row fetched and the cursor's row count.
fn fetch_block(cur: &mut Cursor, ftype: i32, rownum: i32, nrows: i32, out: &mut Vec<u8>) -> (usize, usize) {
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
    cursor_metadata(out, cur);
    for i in 0..count {
        let row = &cur.rows[(s - 1 + i) as usize];
        out.push(0xD1);
        for (c, w) in cur.wires.iter().enumerate() {
            write_value(out, w, row.get(c).unwrap_or(&Value::Null));
        }
        write_value(out, &Wire::Int(4), &Value::Int(1)); // rowstat: the row was fetched
    }
    done(out, 0xFF, DONE_MORE | DONE_COUNT, count as u64);
    (if s >= 1 && s <= n { s as usize } else { 0 }, n as usize)
}
