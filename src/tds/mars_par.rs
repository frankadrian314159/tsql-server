//! Parallel MARS: the requests of a MARS connection that only read run at the same time.
//!
//! The engine runs the statements of one session one at a time, so the logical sessions of a MARS connection (see `mars.rs`)
//! would take turns. SQL Server's own MARS is cooperative: one request runs while the others wait, except that a `SELECT`
//! can be put aside while the client is not reading it. This server goes further where it can do so safely: a request that
//! **only reads** (a batch of `SELECT`s, or `sp_executesql` / `sp_execute` of one, with no OUTPUT parameter) runs on a worker
//! thread, on a *fork* of the session ([`crate::Connection::fork_reader`]: same login, database, temporary tables and settings,
//! its own statement state), while the connection's thread carries on reading frames and delivering finished replies. Reads
//! of the logical sessions therefore overlap each other, and a long query on one does not hold up a short one on another.
//!
//! What keeps it correct:
//! * **Anything else is exclusive.** A request that is not a plain read (DML, DDL, `EXEC`, `SET`, transaction control, a cursor,
//!   a bulk load) first waits until the workers have finished, then runs on the connection's own session as before, so the
//!   changes of one request are complete before the next starts, and the session's own state (variables of earlier batches,
//!   the transaction, `SET` options, temporary tables) is only changed by one request at a time.
//! * **A transaction stays serial.** While the session has a transaction open, every request runs on the session itself, one
//!   after the other (the statements of a transaction see its changes and hold its locks).
//! * **What a fork cannot see is not run on one**: statements that read or change the session (`@@` variables, `NEXT VALUE FOR`,
//!   `OPENXML`, the last identity value), take locks to the end of a transaction (table hints such as `UPDLOCK`, `HOLDLOCK`) or
//!   assign variables are exclusive.
//! * Replies of a logical session stay in order; the replies of different sessions are interleaved as they finish, within the
//!   windows of the client (see `mars.rs`).
use super::*;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// How many requests run on workers at once.
const MAX_WORKERS: usize = 16;

/// Whether `sql` is a batch of queries that read and have no other effect on the session or the database.
pub fn read_only_sql(sql: &str) -> bool {
    use crate::ast::{QueryBody, SelectItem, Stmt};
    let up = sql.to_uppercase();
    const SESSION: &[&str] = &["@@", "NEXT VALUE FOR", "OPENXML", "IDENTITY", "UPDLOCK", "XLOCK", "HOLDLOCK", "SERIALIZABLE", "REPEATABLEREAD", "TABLOCK", "READPAST", " INTO ", "CHANGETABLE", "FOR SYSTEM_TIME"];
    if SESSION.iter().any(|w| up.contains(w)) {
        return false;
    }
    fn body_ok(b: &QueryBody) -> bool {
        match b {
            QueryBody::Select(s) => s.into.is_none() && !s.items.iter().any(|i| matches!(i, SelectItem::Assign { .. })),
            QueryBody::SetOp { left, right, .. } => body_ok(left) && body_ok(right),
            QueryBody::Paren(q) => body_ok(&q.body) && q.ctes.iter().all(|c| body_ok(&c.query.body)),
        }
    }
    match crate::parser::parse(sql) {
        Ok(stmts) => !stmts.is_empty() && stmts.iter().all(|s| matches!(s, Stmt::Query(q) if body_ok(&q.body) && q.ctes.iter().all(|c| body_ok(&c.query.body)))),
        Err(_) => false,
    }
}

/// A request run on a worker: the reply, whole.
type Job = Box<dyn FnOnce(&mut Connection) -> Vec<u8> + Send>;

/// The work of a request, if it can run on a fork of the session.
fn plan_job(kind: u8, payload: &[u8], prepared: &HashMap<i32, (Option<String>, String)>) -> Option<Job> {
    match kind {
        T_BATCH => {
            let mut rd = Rd::new(payload);
            rd.all_headers().ok()?;
            let sql = rd.utf16(rd.left() / 2).ok()?;
            if bulk::parse_insert_bulk(&sql).is_some() || !read_only_sql(&sql) {
                return None;
            }
            Some(Box::new(move |conn: &mut Connection| {
                let mut out = vec![];
                let mut live = Live::new(Reply::Batch);
                conn.execute_streaming(&sql, &mut |item| live.on(item, &mut out, None));
                done(&mut out, 0xFD, if live.errored { DONE_ERROR } else { 0 }, 0);
                out
            }))
        }
        T_RPC => {
            let calls = rpc::parse(payload).ok()?;
            let [call] = &calls[..] else { return None };
            if call.params.iter().any(|p| p.out) {
                return None;
            }
            let ps = &call.params;
            let id = match &call.proc {
                Proc::Id(i) => *i,
                Proc::Name(n) => match n.trim().trim_start_matches("master..").trim_start_matches("dbo.").to_ascii_lowercase().as_str() {
                    "sp_executesql" => rpc::SP_EXECUTESQL,
                    "sp_execute" => rpc::SP_EXECUTE,
                    _ => return None,
                },
            };
            let plan = match id {
                rpc::SP_EXECUTESQL => {
                    let stmt = rpc::text_param(ps, 0, "@stmt").ok()??;
                    let decl = rpc::text_param(ps, 1, "@params").unwrap_or(None);
                    if !read_only_sql(&stmt) {
                        return None;
                    }
                    let args: Vec<usize> = (2..ps.len()).collect();
                    rpc::plan_executesql(&stmt, decl.as_deref(), ps, &args).ok()?
                }
                rpc::SP_EXECUTE => {
                    let Some(Value::Int(h)) = ps.first().map(|p| &p.val) else { return None };
                    let (decl, stmt) = prepared.get(h)?.clone();
                    if !read_only_sql(&stmt) {
                        return None;
                    }
                    let args: Vec<usize> = (1..ps.len()).collect();
                    rpc::plan_executesql(&stmt, decl.as_deref(), ps, &args).ok()?
                }
                _ => return None,
            };
            let calls = calls;
            Some(Box::new(move |conn: &mut Connection| {
                let mut out = vec![];
                let errored = run_plan_on(conn, None, &plan, &calls[0].params, &mut out, None);
                done(&mut out, 0xFE, if errored { DONE_ERROR } else { 0 }, 0);
                out
            }))
        }
        _ => None,
    }
}

/// The workers of a connection and the replies they have finished.
pub struct Pool {
    tx: Sender<(u16, Vec<u8>)>,
    rx: Receiver<(u16, Vec<u8>)>,
    running: usize,
}

impl Pool {
    pub fn new() -> Pool {
        let (tx, rx) = channel();
        Pool { tx, rx, running: 0 }
    }

    fn deliver(&mut self, s: &mut Session, sid: u16, reply: Vec<u8>) -> io::Result<()> {
        self.running = self.running.saturating_sub(1);
        trace(&format!("MARS: session {} finished on a worker, reply {} bytes", sid, reply.len()));
        s.t.mars_reply(sid, &reply)
    }

    /// Deliver the replies that are ready.
    fn collect(&mut self, s: &mut Session) -> io::Result<()> {
        while let Ok((sid, reply)) = self.rx.try_recv() {
            self.deliver(s, sid, reply)?;
        }
        Ok(())
    }

    /// Wait for every worker to finish, delivering what they produce.
    pub fn drain(&mut self, s: &mut Session) -> io::Result<()> {
        while self.running > 0 {
            match self.rx.recv() {
                Ok((sid, reply)) => self.deliver(s, sid, reply)?,
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// The next request of any logical session. Replies of the workers go out while waiting. None: the connection is over.
    pub fn next_request(&mut self, s: &mut Session, probe: &TcpStream) -> Option<(u16, u8, u8, Vec<u8>)> {
        loop {
            self.collect(s).ok()?;
            if let Some(r) = s.t.mars_pop_ready() {
                return Some(r);
            }
            if self.running > 0 {
                // workers are busy: look for a finished reply or a frame every millisecond
                match self.rx.recv_timeout(Duration::from_millis(1)) {
                    Ok((sid, reply)) => {
                        self.deliver(s, sid, reply).ok()?;
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
                }
                if !poll_readable(probe, 1) {
                    continue;
                }
            } else if !wait_readable(probe, &mut s.conn) {
                return None;
            }
            s.t.mars_read_frame().ok()?;
        }
    }

    /// Run the request on a worker if it only reads and the session has no transaction; false if it must run on the session.
    pub fn try_run(&mut self, s: &Session, kind: u8, status: u8, payload: &[u8]) -> bool {
        if status & 0x08 != 0 || (kind != T_BATCH && kind != T_RPC) {
            return false;
        }
        let Some(job) = plan_job(kind, payload, &s.prepared) else { return false };
        if self.running >= MAX_WORKERS {
            return false;
        }
        let Some(mut fork) = s.conn.fork_reader() else { return false };
        let sid = s.t.current_session();
        let tx = self.tx.clone();
        self.running += 1;
        trace(&format!("MARS: session {} runs on a worker ({} running)", sid, self.running));
        std::thread::spawn(move || {
            let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut fork))) {
                Ok(r) => r,
                Err(_) => {
                    let mut out = vec![];
                    error_token(&mut out, &SqlError::new(8144, "The request could not be processed: an internal error."));
                    done(&mut out, 0xFD, DONE_ERROR, 0);
                    out
                }
            };
            let _ = tx.send((sid, reply));
        });
        true
    }
}

/// Whether the socket has something to read within `ms` milliseconds (or is closed).
fn poll_readable(tcp: &TcpStream, ms: u64) -> bool {
    let _ = tcp.set_read_timeout(Some(Duration::from_millis(ms)));
    let mut b = [0u8; 1];
    let r = match tcp.peek(&mut b) {
        Ok(_) => true,
        Err(e) => !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut),
    };
    let _ = tcp.set_read_timeout(None);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_reads_are_run_beside_others() {
        assert!(read_only_sql("SELECT 1"));
        assert!(read_only_sql("SELECT a, SUM(b) FROM t GROUP BY a; SELECT * FROM u"));
        assert!(read_only_sql("WITH c AS (SELECT 1 AS x) SELECT x FROM c UNION ALL SELECT 2"));
        for sql in ["INSERT t VALUES (1)", "SELECT 1; UPDATE t SET a = 1", "SELECT * INTO u FROM t", "DECLARE @x INT; SELECT @x", "SELECT @@ROWCOUNT", "SELECT NEXT VALUE FOR s", "SELECT * FROM t WITH (UPDLOCK)", "SELECT @v = a FROM t", "EXEC p", "SET NOCOUNT ON", "SELECT SCOPE_IDENTITY()", "", "SELEC 1"] {
            assert!(!read_only_sql(sql), "{}", sql);
        }
    }
}
