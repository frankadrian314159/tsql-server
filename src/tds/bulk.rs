//! Bulk load (`bcp`, `SqlBulkCopy`, tiberius' `bulk_insert`).
//!
//! The client first sends `INSERT BULK table (col type, ...) [WITH (...)]` as an ordinary batch, which only names the target
//! and its options, then a message of type 7 whose payload is a COLMETADATA token (the columns and their types), one ROW or
//! NBCROW token per row, and a DONE token. The payload can be far bigger than memory, so it is decoded as the packets
//! arrive and the rows are inserted in chunks, all inside one transaction (the client's, or one the load opens): either every
//! row is stored or none is. Constraints and triggers always apply (SQL Server skips them unless `CHECK_CONSTRAINTS` and
//! `FIRE_TRIGGERS` are given).
use super::*;
use crate::ast::{Expr, Insert, InsertSrc, Stmt, Target};

/// What `INSERT BULK` announced.
pub struct BulkTarget {
    pub table: Vec<String>,
    pub keep_identity: bool,
}

/// The rows are inserted this many at a time.
const CHUNK_ROWS: usize = 5000;
const CHUNK_BYTES: usize = 16 << 20;

/// The parts of an object name written `[a].[b]`, `a.b` or `"a"."b"`.
fn name_parts(text: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in text.trim().chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '[') => quote = Some(']'),
            (None, '"') => quote = Some('"'),
            (None, '.') => {
                parts.push(std::mem::take(&mut cur));
            }
            (None, c) if c.is_whitespace() => {}
            (None, c) => cur.push(c),
        }
    }
    parts.push(cur);
    parts.retain(|p| !p.is_empty());
    parts
}

/// `INSERT BULK`, if `sql` is such a statement.
pub fn parse_insert_bulk(sql: &str) -> Option<BulkTarget> {
    let t = sql.trim_start();
    let mut words = t.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("insert") || !words.next()?.eq_ignore_ascii_case("bulk") {
        return None;
    }
    // the text after the two words
    let rest = {
        let lower = t.to_ascii_lowercase();
        let i = lower.find("bulk")? + 4;
        &t[i..]
    };
    let open = rest.find('(').unwrap_or(rest.len());
    let table = name_parts(&rest[..open]);
    // the options follow the column list: the column list is the first balanced pair of parentheses
    let mut depth = 0i32;
    let mut end = rest.len();
    for (i, ch) in rest.char_indices().skip_while(|(i, _)| *i < open) {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let options = rest[end.min(rest.len())..].to_ascii_uppercase();
    Some(BulkTarget { table, keep_identity: options.contains("KEEP_IDENTITY") })
}

struct BulkCol {
    name: String,
    ti: Ti,
}

/// The state of one bulk load message while its packets arrive.
pub struct BulkLoad {
    cols: Vec<BulkCol>,
    rows: Vec<Vec<Value>>,
    bytes: usize,
    inserted: u64,
    error: Option<SqlError>,
    began: bool,
    own_tx: bool,
    /// Which of the columns of the stream are not stored (identity values without KEEP_IDENTITY, computed columns, rowversion).
    skip: Option<Vec<bool>>,
}

impl BulkLoad {
    pub fn new() -> BulkLoad {
        BulkLoad { cols: vec![], rows: vec![], bytes: 0, inserted: 0, error: None, began: false, own_tx: false, skip: None }
    }
}

fn is_incomplete(e: &str) -> bool {
    e == "unexpected end of message"
}

impl Session {
    /// The reply to `INSERT BULK`: check the table, remember the target for the data that follows.
    pub fn insert_bulk(&mut self, target: BulkTarget) -> Vec<u8> {
        let mut out = vec![];
        let shown: String = target.table.iter().map(|p| format!("[{}]", p)).collect::<Vec<_>>().join(".");
        if self.conn.table_info(&shown).is_none() {
            error_token(&mut out, &SqlError::new(208, format!("Invalid object name '{}'.", target.table.join("."))));
            return self.finish(out, true, Reply::Batch);
        }
        self.bulk = Some(target);
        self.finish(out, false, Reply::Batch)
    }

    /// Handle a bulk load message whose first packet is `first` (`eom`: it is also the last).
    pub fn bulk_load(&mut self, first: Vec<u8>, mut eom: bool) -> R<Vec<u8>> {
        let target = self.bulk.take();
        let mut st = BulkLoad::new();
        if target.is_none() {
            st.error = Some(SqlError::new(4004, "Bulk load data was received without an INSERT BULK statement."));
        }
        let mut buf = first;
        if tracing() {
            trace(&format!("bulk data starts: {}", buf.iter().take(300).map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")));
        }
        loop {
            if st.error.is_none() {
                if let Err(e) = self.bulk_parse(&mut buf, &mut st, eom, target.as_ref().unwrap()) {
                    st.error = Some(SqlError::new(4832, format!("Bulk load: {}.", e)));
                }
            } else {
                buf.clear();
            }
            if eom {
                break;
            }
            let (kind, status, data, last) = self.t.read_packet().map_err(|e| format!("the bulk load was cut off: {}", e))?;
            if kind == T_ATTN || status & 2 != 0 {
                // the client cancelled the load: nothing is kept
                if tracing() {
                    trace(&format!("bulk load cancelled after {} rows stored", st.inserted));
                }
                st.error = Some(SqlError::new(4004, "The bulk load was cancelled."));
                if st.began && st.own_tx {
                    self.conn.execute("ROLLBACK TRANSACTION");
                }
                let mut out = vec![];
                self.changes(true, st.own_tx, &mut out);
                done(&mut out, 0xFD, DONE_ATTN, 0);
                return Ok(out);
            }
            buf.extend_from_slice(&data);
            eom = last;
        }
        if let Some(tg) = &target {
            self.bulk_flush(&mut st, tg);
        }
        let errored = st.error.is_some();
        if st.began && st.own_tx {
            self.conn.execute(if errored { "ROLLBACK TRANSACTION" } else { "COMMIT TRANSACTION" });
        }
        let mut out = vec![];
        if let Some(e) = &st.error {
            error_token(&mut out, e);
        }
        self.changes(errored, errored && st.own_tx, &mut out);
        let mut status = if errored { DONE_ERROR } else { DONE_COUNT };
        if self.in_tx {
            status |= DONE_INXACT;
        }
        done(&mut out, 0xFD, status, st.inserted);
        Ok(out)
    }

    /// Decode the tokens in `buf` that are complete (all of them once `last`), leaving the rest.
    fn bulk_parse(&mut self, buf: &mut Vec<u8>, st: &mut BulkLoad, last: bool, target: &BulkTarget) -> Result<(), String> {
        let mut pos = 0;
        let mut prev = 0;
        while pos < buf.len() {
            prev = if pos > 0 { prev } else { 0 };
            let mut rd = Rd::new(&buf[pos..]);
            let token = rd.u8()?;
            let parsed: R<()> = (|| {
                match token {
                    0x81 => {
                        let n = rd.u16()?;
                        let mut cols = vec![];
                        if n != 0xFFFF {
                            for _ in 0..n {
                                rd.u32()?; // user type
                                rd.u16()?; // flags
                                let ti = read_typeinfo(&mut rd)?;
                                skip_table_name(&mut rd, &ti.w)?;
                                let name = rd.bvarchar()?;
                                cols.push(BulkCol { name, ti });
                            }
                        }
                        st.cols = cols;
                        st.skip = None;
                    }
                    0xD1 => {
                        let mut row = Vec::with_capacity(st.cols.len());
                        for c in &st.cols {
                            row.push(read_value(&mut rd, &c.ti)?);
                        }
                        st.bytes += rd.p;
                        st.rows.push(row);
                    }
                    0xD2 => {
                        let map = rd.take(st.cols.len().div_ceil(8))?.to_vec();
                        let mut row = Vec::with_capacity(st.cols.len());
                        for (i, c) in st.cols.iter().enumerate() {
                            if map[i / 8] & (1 << (i % 8)) != 0 {
                                row.push(Value::Null);
                            } else {
                                row.push(read_value(&mut rd, &c.ti)?);
                            }
                        }
                        st.bytes += rd.p;
                        st.rows.push(row);
                    }
                    0xFD | 0xFE | 0xFF => {
                        // status, command, row count; the count is 8 bytes, but some clients (ADO.NET) send 4 in the DONE that ends the data
                        let left = rd.left();
                        if last && left < 12 {
                            rd.take(left)?;
                        } else {
                            rd.take(12)?;
                        }
                    }
                    0xA9 => {
                        let n = rd.u16()? as usize;
                        rd.take(n)?;
                    }
                    other => {
                        let dump: String = buf[prev.min(pos)..(pos + 24).min(buf.len())].iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
                        return Err(format!("unexpected token 0x{:02X} in the bulk load data (after {} rows of {} columns, at byte {}: {})", other, st.inserted + st.rows.len() as u64, st.cols.len(), pos, dump));
                    }
                }
                Ok(())
            })();
            match parsed {
                Ok(()) => {
                    prev = pos;
                    pos += rd.p
                }
                Err(e) if is_incomplete(&e) && !last => break,
                Err(e) if is_incomplete(&e) => return Err(format!("the bulk load data ends in the middle of a row (after {} rows stored, {} pending; the row starts {})", st.inserted, st.rows.len(), buf[pos..].iter().take(60).map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" "))),
                Err(e) => return Err(e),
            }
            if st.rows.len() >= CHUNK_ROWS || st.bytes >= CHUNK_BYTES {
                self.bulk_flush(st, target);
                if st.error.is_some() {
                    break;
                }
            }
        }
        buf.drain(..pos);
        Ok(())
    }

    /// Insert the rows decoded so far.
    fn bulk_flush(&mut self, st: &mut BulkLoad, target: &BulkTarget) {
        if st.rows.is_empty() || st.error.is_some() {
            st.rows.clear();
            st.bytes = 0;
            return;
        }
        if !st.began {
            st.began = true;
            if self.conn.transaction_count() == 0 {
                st.own_tx = true;
                self.conn.execute("BEGIN TRANSACTION");
            }
        }
        let shown: String = target.table.iter().map(|p| format!("[{}]", p.replace(']', "]]"))).collect::<Vec<_>>().join(".");
        if st.skip.is_none() {
            // what is not stored: values for an identity column (unless KEEP_IDENTITY), computed columns, rowversion
            let info = self.conn.table_info(&shown);
            let skip = st
                .cols
                .iter()
                .map(|c| {
                    info.as_ref()
                        .and_then(|t| t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(&c.name)))
                        .map(|x| x.computed || (x.identity.is_some() && !target.keep_identity) || x.ty.eq_ignore_ascii_case("timestamp") || x.ty.eq_ignore_ascii_case("rowversion"))
                        .unwrap_or(false)
                })
                .collect();
            st.skip = Some(skip);
        }
        let skip = st.skip.clone().unwrap_or_default();
        let names: Vec<String> = st.cols.iter().zip(&skip).filter(|(_, s)| !**s).map(|(c, _)| c.name.clone()).collect();
        let n = st.rows.len();
        let rows: Vec<Vec<Expr>> = std::mem::take(&mut st.rows).into_iter().map(|r| r.into_iter().zip(&skip).filter(|(_, s)| !**s).map(|(v, _)| Expr::Lit(v)).collect()).collect();
        st.bytes = 0;
        if names.is_empty() {
            st.inserted += n as u64;
            return;
        }
        let ins = Insert { ctes: vec![], target: Target::Table(target.table.clone()), cols: Some(names), output: None, src: InsertSrc::Values(rows), top: None };
        let mut stmts: Vec<Stmt> = vec![];
        if target.keep_identity {
            if let Ok(s) = crate::parser::parse(&format!("SET IDENTITY_INSERT {} ON", shown)) {
                stmts.extend(s);
            }
        }
        stmts.push(Stmt::Insert(Box::new(ins)));
        if target.keep_identity {
            if let Ok(s) = crate::parser::parse(&format!("SET IDENTITY_INSERT {} OFF", shown)) {
                stmts.extend(s);
            }
        }
        let res = self.conn.execute_stmts(&stmts);
        match res.first_error() {
            Some(e) => st.error = Some(e.clone()),
            None => st.inserted += n as u64,
        }
    }
}
