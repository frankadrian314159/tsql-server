//! tsql-server: an in-memory clone of Microsoft SQL Server with a T-SQL engine.
//!
//! ```
//! use tsql_server::Server;
//! let server = Server::new();
//! let mut conn = server.connect();
//! let res = conn.execute("SELECT 1 + 1 AS two");
//! assert_eq!(res.resultsets()[0].rows[0][0].to_string(), "2");
//! ```
pub mod ast;
pub mod audit;
pub mod binxml;
pub mod catalog;
pub mod convert;
pub mod collation;
pub mod coord;
pub mod decimal;
pub mod engine;
pub mod error;
pub mod inspect;
pub mod lexer;
pub mod mask;
pub mod mssqlbak;
pub mod mtf;
pub mod mtfwrite;
pub mod parser;
pub mod persist;
pub mod prof;
pub mod security;
pub mod sqlbak;
pub mod sqlbak_write;
pub mod storage;
pub mod tds;
pub mod tools;
pub mod types;
pub mod xmlpath;
pub mod fulltext;
pub mod hierarchyid;
pub mod spatial;
pub mod rewrite;
pub mod script;
pub mod xsd;
pub mod xpress;

use catalog::ServerState;
use engine::{Engine, SessionState, SrvGuard};
use std::sync::{Arc, RwLock};

pub use engine::{BatchResult, Event, ResultSet, Streamed};
pub use error::SqlError;
pub use persist::StorageOptions;
pub use storage::StoreStats;
pub use types::{DataType, Value};

struct Shared {
    state: RwLock<ServerState>,
    crashed: std::sync::atomic::AtomicBool,
    /// makes the interceptor of each new connection (see [`Interceptor`])
    interceptor: std::sync::Mutex<Option<std::sync::Arc<InterceptorFactory>>>,
}

/// A hook that can take over the batches of a connection. An interceptor sees the text of every batch the connection is
/// asked to run; it either runs the batch itself and reports what happened to `sink` (returning true), or declines (returning
/// false) and the engine runs the batch as usual. The distributed edition uses it to serve clients (over TDS) from a control
/// node whose own engine only holds the schema: the interceptor sends the batches that touch distributed tables to the
/// cluster. A server without an interceptor behaves as it always has.
pub trait Interceptor: Send {
    fn handle(&mut self, sql: &str, sink: &mut dyn FnMut(Streamed) -> bool) -> bool;
    /// The same for statements that are already parsed (what the bulk load of the TDS listener runs); declining is the default.
    fn handle_stmts(&mut self, _stmts: &[crate::ast::Stmt], _sink: &mut dyn FnMut(Streamed) -> bool) -> bool {
        false
    }
    /// Before a batch is handled: the session's temporary tables (`#name`, lower case) that it needs the interceptor to take over,
    /// because the batch is one it will run itself. The connection reads each (its `CREATE TABLE` and its rows), drops it and gives it
    /// to [`Interceptor::adopt_temp`]. None by default.
    fn temps_wanted(&mut self, _sql: &str) -> Vec<String> {
        vec![]
    }
    /// The database the connection is in, told before each batch is handled (a `USE` that the engine ran changes it): a cluster's
    /// control node names the tables of the batch by it. Nothing by default.
    fn set_database(&mut self, _db: &str) -> Option<String> {
        None
    }
    /// After the interceptor handled a batch: the database the connection is in now, when the batch changed it (a batch the interceptor
    /// ran that has a `USE` in it: the engine did not see the `USE`). None by default.
    fn database_after(&mut self) -> Option<String> {
        None
    }
    /// A temporary table of the session handed over (see [`Interceptor::temps_wanted`]): `create` is its `CREATE TABLE`, `rows` its rows.
    fn adopt_temp(&mut self, _name: &str, _create: &str, _rows: Vec<Vec<Value>>) -> Result<(), SqlError> {
        Ok(())
    }
}

/// Makes the interceptor of a new connection.
pub type InterceptorFactory = dyn Fn() -> Box<dyn Interceptor> + Send + Sync;

impl Drop for Shared {
    fn drop(&mut self) {
        // clean shutdown: fold the log into a checkpoint (skipped when simulating a crash)
        let crashed = self.crashed.load(std::sync::atomic::Ordering::SeqCst);
        if let Ok(g) = self.state.write() {
            if !crashed && g.core.no_active() {
                let taken = g.persist().take();
                if let Some(mut p) = taken {
                    let _ = p.checkpoint(&g);
                }
            }
            // dropping the tables must not walk the page files
            g.store.set_closing();
            g.temp_store.set_closing();
        }
    }
}

/// A server instance holding all databases. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Server {
    shared: Arc<Shared>,
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}

impl Server {
    /// A server whose data lives only in memory (nothing is written to disk).
    pub fn new() -> Server {
        Server { shared: Arc::new(Shared { state: RwLock::new(ServerState::new()), crashed: Default::default(), interceptor: Default::default() }) }
    }

    /// Open (or create) a server whose data lives in `dir`. Committed work survives crashes:
    /// every commit is written to a checksummed write-ahead log before it returns, and the log is
    /// folded into an atomic checkpoint periodically and on a clean shutdown.
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<Server, SqlError> {
        Server::open_with(dir, StorageOptions::default())
    }

    /// Like [`Server::open`] with explicit storage settings (buffer pool size).
    pub fn open_with(dir: impl AsRef<std::path::Path>, opts: StorageOptions) -> Result<Server, SqlError> {
        let (p, mut state) = persist::Persist::open(dir.as_ref(), opts)?;
        state.persist = std::sync::Mutex::new(Some(p));
        state.core.audit.set_active(state.security.audits.iter().any(|a| a.enabled));
        Ok(Server { shared: Arc::new(Shared { state: RwLock::new(state), crashed: Default::default(), interceptor: Default::default() }) })
    }

    /// Make this server one replica of a group: every transaction that commits proposes its redo record to `r` first (see
    /// [`persist::Replicator`]). `None` makes it an ordinary server again. A replica needs a persistent server
    /// ([`Server::open`]).
    pub fn set_replicator(&self, r: Option<Arc<dyn persist::Replicator>>) {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        *g.replicator.lock().unwrap_or_else(|e| e.into_inner()) = r;
    }

    /// The commit timestamp the data is current to.
    pub fn applied_ts(&self) -> u64 {
        self.shared.state.read().unwrap_or_else(|e| e.into_inner()).core.clock()
    }

    /// Apply a record that another replica committed (a follower applying the group's log): it is written to this
    /// server's log and its changes are made, as when the log is replayed after a crash. Returns false when the record
    /// is already applied (its timestamp is not past the data's), so applying a log twice is harmless.
    pub fn apply_replicated(&self, rec: &persist::WalRecord) -> Result<bool, SqlError> {
        let mut g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        if rec.ts <= g.core.clock() {
            return Ok(false);
        }
        if let Some(p) = g.persist().as_mut() {
            p.append(rec.ts, rec.next_object_id, rec.next_rowversion, rec.ops.clone(), false)?;
        }
        persist::apply_record(&mut g, rec);
        g.epoch += 1;
        Ok(true)
    }

    /// A record that a session of this server proposed (its changes are made by the session itself, not applied from the
    /// log) has been committed by the replica group: record what it does to the set of prepared transactions, which the
    /// session leaves to the log. `live`: the transaction is still open on a session here.
    pub fn note_prepared(&self, rec: &persist::WalRecord, live: bool) {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        let mut p = g.prepared.lock().unwrap_or_else(|e| e.into_inner());
        for op in &rec.ops {
            match op {
                persist::LogOp::Prepare { gid, ops } => {
                    p.entry(*gid).or_insert_with(|| persist::PreparedTxn { ops: ops.clone(), live });
                }
                persist::LogOp::Resolve { gid } => {
                    p.remove(gid);
                }
                _ => {}
            }
        }
    }

    /// The whole committed state of the server, as bytes: what a replica group keeps as a snapshot of its state machine so
    /// that its log can be cut, and sends to a replica that has fallen behind the start of the log (see
    /// [`Server::install_state`]). Statements keep running while it is taken (it only reads what is committed, like a
    /// query), so it may include the changes of commits that are in progress: the record is stamped `ts`, the commit
    /// timestamp of the last log entry it is meant to contain, so that a replica that installs it applies the entries after
    /// that one again, which is harmless (a record sets rows and objects to the values it has). It cannot be taken while
    /// the caller holds up a commit that is waiting for the caller (use another thread).
    pub fn export_state_at(&self, ts: u64) -> Result<Vec<u8>, SqlError> {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        let mut rec = g.export_record();
        rec.ts = ts;
        postcard::to_allocvec(&rec).map_err(|e| SqlError::with(9002, 17, 1, format!("snapshot serialisation failed: {}", e)))
    }

    /// [`Server::export_state_at`] the current clock.
    pub fn export_state(&self) -> Result<Vec<u8>, SqlError> {
        self.export_state_at(self.applied_ts())
    }

    /// Replace the whole state of this server by one exported with [`Server::export_state`] (a replica that is being given
    /// a snapshot). It is written to this server's log first, so that a restart finds it.
    pub fn install_state(&self, data: &[u8]) -> Result<(), SqlError> {
        let rec: persist::WalRecord = postcard::from_bytes(data).map_err(|e| SqlError::with(9002, 17, 1, format!("bad snapshot: {}", e)))?;
        let mut g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        let full = g.install_record(&rec);
        if let Some(p) = g.persist().as_mut() {
            p.append(full.ts, full.next_object_id, full.next_rowversion, full.ops.clone(), true)?;
        }
        persist::apply_record(&mut g, &full);
        g.epoch += 1;
        Ok(())
    }

    /// Write every commit so far to the log file and force it to disk. A commit that a replica group has made durable in its
    /// own log is written to the engine's log without being forced (the group's log is the durable copy); the group forces
    /// the engine's log before it drops the entries (see [`Server::export_state`]).
    pub fn flush_log(&self) -> Result<(), SqlError> {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        let batch = match g.persist().as_mut() {
            Some(p) => p.take_batch(),
            None => return Ok(()),
        };
        batch.force().map(|_| ()).map_err(|e| SqlError::with(3314, 21, 1, format!("I/O error: {}", e)))
    }

    /// Who waits for whom for a lock right now, as sessions: (waiting session, the session it waits for), for the sessions
    /// whose transactions are known. The control node of the distributed edition joins the graphs of its shards to find the
    /// deadlocks that run through several of them.
    pub fn wait_graph(&self) -> Vec<(i32, i32)> {
        self.shared.state.read().unwrap_or_else(|e| e.into_inner()).core.wait_graph()
    }

    /// Ask session `spid` to stop (as `KILL` does): its statement ends with error 596 and its transaction is rolled back.
    pub fn kill_session(&self, spid: i32) {
        self.shared.state.read().unwrap_or_else(|e| e.into_inner()).core.kill_session(spid);
    }

    /// The prepared transactions (two-phase commit, see [`Connection::prepare_transaction`]) that are *in doubt*: they voted,
    /// and the process that held them open is gone (the server restarted, or their session ended), so only their log record
    /// remains. Each must be settled with [`Server::resolve_prepared`]; until then the data they would change is not safe to
    /// use, which is why a coordinator settles them before it lets statements run.
    pub fn in_doubt(&self) -> Vec<u64> {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        let p = g.prepared.lock().unwrap_or_else(|e| e.into_inner());
        p.iter().filter(|(_, t)| !t.live).map(|(k, _)| *k).collect()
    }

    /// Whether `gid` is prepared on this server, and whether its transaction is still open on a session (`Some(true)`).
    pub fn prepared_state(&self, gid: u64) -> Option<bool> {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        let p = g.prepared.lock().unwrap_or_else(|e| e.into_inner());
        p.get(&gid).map(|t| t.live)
    }

    /// Settle an in-doubt prepared transaction: commit it (its changes are made, from its `Prepare` record) or abort it
    /// (nothing is made). The settlement is durable (and replicated, on a replica) before it returns. Returns false when
    /// `gid` is not prepared here (it was settled already, so this is safe to repeat). A transaction that is still open on a
    /// session is settled by that session (`COMMIT` / `ROLLBACK`), and this returns error 50211.
    pub fn resolve_prepared(&self, gid: u64, commit: bool) -> Result<bool, SqlError> {
        // the record is made durable under a shared lock (a replica's apply thread needs the exclusive one while we wait)
        let settled = {
            let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
            g.log_settlement(gid, commit, false)?
        };
        let Some((ts, rec)) = settled else { return Ok(false) };
        let mut g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        if commit {
            persist::apply_record(&mut g, &rec);
            g.epoch += 1;
        } else {
            g.prepared.get_mut().unwrap_or_else(|e| e.into_inner()).remove(&gid);
        }
        g.core.finish_commit_ts(ts);
        Ok(true)
    }

    /// Drop the server as if the process had been killed: no final checkpoint is written, so the
    /// next [`Server::open`] has to recover from the log. Intended for tests.
    #[doc(hidden)]
    pub fn simulate_crash(self) {
        self.shared.crashed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Force a checkpoint now (a no-op for in-memory servers or while transactions are open).
    pub fn checkpoint(&self) {
        let g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        if g.core.no_active() {
            if let Some(p) = g.persist().as_mut() {
                let _ = p.checkpoint(&g);
            }
        }
    }

    /// Write every dirty page to the page file but do not write a checkpoint, as if the process
    /// died halfway through a checkpoint. Intended for tests.
    #[doc(hidden)]
    pub fn flush_pages_only(&self) {
        let g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        let _ = g.store.prepare_checkpoint();
    }

    /// Set how much memory a sort or `DISTINCT` may use before spilling to temporary files in `dir` (the system
    /// temporary directory by default), and from how many heap slots a single-table query uses the streaming
    /// executor.
    pub fn set_work_mem(&self, bytes: usize, stream_min_slots: usize, dir: Option<std::path::PathBuf>) {
        let mut g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        g.work.bytes = bytes.max(1);
        g.work.stream_min_slots = stream_min_slots;
        if let Some(d) = dir {
            g.work.dir = d;
        }
    }

    /// Lock waits by the place in the engine that raised them: (source line, waits, time waited).
    pub fn lock_wait_sites(&self) -> Vec<(String, u64, std::time::Duration)> {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        g.core.lock_wait_sites().into_iter().map(|(s, n, ns)| (s, n, std::time::Duration::from_nanos(ns))).collect()
    }

    /// (how many times sessions waited for a lock holder, how long they waited in all) since the server started.
    pub fn lock_waits(&self) -> (u64, std::time::Duration) {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        let (n, ns) = g.core.lock_wait_totals();
        (n, std::time::Duration::from_nanos(ns))
    }

    /// How many times the redo log has been forced to disk for commits (sessions that commit together share a force).
    pub fn log_forces(&self) -> u64 {
        let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
        g.core.log_forces()
    }

    /// Buffer pool and page file statistics: `(persistent tables, temporary tables)`.
    pub fn storage_stats(&self) -> (StoreStats, StoreStats) {
        let g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        (g.store.stats(), g.temp_store.stats())
    }

    /// Open a trusted session as the built-in `sa` (sysadmin) login.
    pub fn connect(&self) -> Connection {
        let mut c = self.session_for("sa");
        c.interceptor = self.new_interceptor();
        c
    }

    /// Like [`Server::connect`] but the connection is never intercepted (for the interceptor's own use of the engine).
    pub fn connect_internal(&self) -> Connection {
        self.session_for("sa")
    }

    /// Install (or remove) the factory of the interceptors of the connections made from now on (see [`Interceptor`]).
    pub fn set_interceptor(&self, f: Option<std::sync::Arc<InterceptorFactory>>) {
        *self.shared.interceptor.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    fn new_interceptor(&self) -> Option<Box<dyn Interceptor>> {
        self.shared.interceptor.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|f| f())
    }

    /// Authenticate with a login name and password.
    pub fn connect_as(&self, login: &str, password: &str) -> Result<Connection, SqlError> {
        let outcome = {
            let mut g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
            let r = Self::check_login(&mut g, login, password);
            if let Err(e) = &r {
                if g.core.audit.is_active() {
                    engine::auditing::login_event(&g, "LOGIN FAILED", login, 0, &e.message);
                }
            }
            r
        };
        outcome?;
        let mut conn = self.session_for(login);
        {
            let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
            if g.core.audit.is_active() {
                engine::auditing::login_event(&g, "LOGIN SUCCEEDED", &conn.session.login, conn.session.spid, "Login succeeded for user");
            }
        }
        conn.run_logon_triggers()?;
        conn.interceptor = self.new_interceptor();
        Ok(conn)
    }

    /// Connect as a login that authenticates outside the server (`CREATE LOGIN ... FROM WINDOWS` or `FROM EXTERNAL
    /// PROVIDER`), once Kerberos or Azure AD has established who the client is. `names` are the forms the identity can take
    /// (`DOMAIN\user`, `user@REALM`, `user@contoso.com`), most specific first; the first that names an external login is
    /// used.
    pub fn connect_external(&self, names: &[String]) -> Result<Connection, SqlError> {
        let shown = names.first().cloned().unwrap_or_default();
        let login = {
            let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
            let found = names.iter().find_map(|n| g.security.logins.get(&n.to_lowercase()).filter(|l| l.external.is_some()).map(|l| l.name.clone()));
            let r = match found {
                Some(n) if g.security.logins.get(&n.to_lowercase()).is_some_and(|l| !l.disabled) => Ok(n),
                _ => Err(SqlError::with(18456, 14, 1, format!("Login failed for user '{}'.", shown))),
            };
            if let Err(e) = &r {
                if g.core.audit.is_active() {
                    engine::auditing::login_event(&g, "LOGIN FAILED", &shown, 0, &e.message);
                }
            }
            r?
        };
        let mut conn = self.session_for(&login);
        {
            let g = self.shared.state.read().unwrap_or_else(|e| e.into_inner());
            if g.core.audit.is_active() {
                engine::auditing::login_event(&g, "LOGIN SUCCEEDED", &conn.session.login, conn.session.spid, "Login succeeded for user");
            }
        }
        conn.run_logon_triggers()?;
        conn.interceptor = self.new_interceptor();
        Ok(conn)
    }

    fn check_login(g: &mut catalog::ServerState, login: &str, password: &str) -> Result<(), SqlError> {
        {
            let now = chrono::Local::now().naive_local();
            let (threshold, minutes, max_age) = (g.core.run_value("login lockout threshold").max(1) as u32, g.core.run_value("login lockout minutes").max(1), g.core.run_value("password max age (days)"));
            let failed = |why: &str| SqlError::with(18456, 14, 1, format!("Login failed for user '{}'.{}", login, why));
            let Some(l) = g.security.logins.get_mut(&login.to_lowercase()) else { return Err(failed("")) };
            if l.disabled {
                return Err(failed(""));
            }
            if l.is_locked(now) {
                return Err(SqlError::with(18486, 14, 1, format!("Login failed for user '{}' because the account is currently locked out. The system administrator can unlock it.", login)));
            }
            if !l.check_password(password) {
                if l.check_policy {
                    l.bad_count += 1;
                    if l.bad_count >= threshold {
                        l.locked_until = Some(now + chrono::Duration::minutes(minutes));
                        l.bad_count = 0;
                    }
                }
                return Err(failed(""));
            }
            if l.is_expired(now, max_age) {
                return Err(SqlError::with(18487, 14, 1, format!("Login failed for user '{}'. Reason: The password of the account has expired.", login)));
            }
            if l.must_change {
                return Err(SqlError::with(18488, 14, 1, format!("Login failed for user '{}'. Reason: The password of the account must be changed.", login)));
            }
            l.bad_count = 0;
        }
        Ok(())
    }

    fn session_for(&self, login: &str) -> Connection {
        let g = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
        let spid = g.core.next_spid();
        let mut session = SessionState::new(spid);
        let canonical = g.security.logins.get(&login.to_lowercase()).map(|l| l.name.clone()).unwrap_or_else(|| login.to_string());
        session.login = canonical.clone();
        if let Some(l) = g.security.logins.get(&login.to_lowercase()) {
            if g.databases.contains_key(&l.default_db.to_lowercase()) {
                session.db = g.databases[&l.default_db.to_lowercase()].name.clone();
            }
        }
        let now = chrono::Local::now().naive_local();
        session.info = Some(g.core.register_session(coord::SessionInfo {
            spid,
            login: session.login.clone(),
            host: session.host_name.clone(),
            app: session.app_name.clone(),
            db: session.db.clone(),
            status: "sleeping",
            isolation: session.isolation.level(),
            login_time: now,
            last_batch: now,
            last_batch_at: std::time::SystemTime::now(),
            batches: 0,
            command: String::new(),
        }));
        drop(g);
        Connection { server: self.clone(), session, interceptor: None }
    }
}

/// A session against a [`Server`].
pub struct Connection {
    server: Server,
    session: SessionState,
    interceptor: Option<Box<dyn Interceptor>>,
}

impl Connection {
    pub(crate) fn session_mut(&mut self) -> &mut SessionState {
        &mut self.session
    }

    /// A second connection for the same login that can run statements that only read while this one is busy (what the
    /// requests of a MARS connection that are read-only do in parallel). `None` while a transaction is open: the statements of
    /// a transaction run one at a time.
    pub fn fork_reader(&self) -> Option<Connection> {
        if self.session.trancount > 0 || self.interceptor.is_some() {
            return None;
        }
        Some(Connection { server: self.server.clone(), session: self.session.fork(), interceptor: None })
    }

    /// Tell the interceptor which database the connection is in (see [`Interceptor::set_database`]).
    fn sync_interceptor_db(&mut self) {
        if let Some(i) = self.interceptor.as_mut() {
            // (the interceptor may say the connection's database is gone: it is in the one it names)
            if let Some(db) = i.set_database(&self.session.db) {
                self.session.db = db;
            }
        }
    }

    /// Give the interceptor the temporary tables of this session that the batch needs it to take over (see [`Interceptor::temps_wanted`]):
    /// the table is read, dropped here, and made by the interceptor from what was read. An error is returned for the batch.
    fn hand_over_temps(&mut self, sql: &str) -> Option<SqlError> {
        self.sync_interceptor_db();
        let names = self.interceptor.as_mut()?.temps_wanted(sql);
        for n in names {
            let (create, rows) = {
                let shared = &self.server.shared;
                let guard = SrvGuard::new(&shared.state);
                let mut eng = Engine::new(guard, &mut self.session);
                let Some(create) = eng.temp_table_create_sql(&n) else { continue };
                eng.run_batch(&format!("SELECT * FROM {}", n));
                let rows = eng.out.iter().find_map(|e| if let Event::Rows(rs) = e { Some(rs.rows.clone()) } else { None }).unwrap_or_default();
                (create, rows)
            };
            if let Err(e) = self.interceptor.as_mut()?.adopt_temp(&n, &create, rows) {
                return Some(e);
            }
            let shared = &self.server.shared;
            let guard = SrvGuard::new(&shared.state);
            let mut eng = Engine::new(guard, &mut self.session);
            eng.run_batch(&format!("DROP TABLE {}", n));
        }
        None
    }

    /// Execute a single batch (no `GO` handling).
    pub fn execute(&mut self, sql: &str) -> BatchResult {
        crate::span!("execute");
        if let Some(e) = self.hand_over_temps(sql) {
            return BatchResult { events: vec![Event::Error(e)] };
        }
        if let Some(i) = self.interceptor.as_mut() {
            let mut events = vec![];
            let mut rows: Option<ResultSet> = None;
            let handled = i.handle(sql, &mut |item| {
                match item {
                    Streamed::Event(e) => events.push(e),
                    Streamed::Header { columns, types, .. } => rows = Some(ResultSet { columns, types, rows: vec![] }),
                    Streamed::Row(r) => {
                        if let Some(rs) = rows.as_mut() {
                            rs.rows.push(r);
                        }
                    }
                    Streamed::End(_) => {
                        if let Some(rs) = rows.take() {
                            events.push(Event::Rows(rs));
                        }
                    }
                }
                true
            });
            if handled {
                if let Some(db) = i.database_after() {
                    self.session.db = db;
                }
                return BatchResult { events };
            }
        }
        let shared = &self.server.shared;
        let guard = SrvGuard::new(&shared.state);
        let mut eng = Engine::new(guard, &mut self.session);
        eng.run_batch(sql);
        BatchResult { events: eng.out }
    }

    /// Append rows to a table without the machinery of an `INSERT` (see `Engine::load_rows`): for a copy of data that has been
    /// checked already, in a table nobody else uses. A durable server, or a session in a transaction, takes the ordinary `INSERT`.
    /// `cols`: the columns the values are for, in order (None: every column that takes a value).
    pub fn load_rows(&mut self, table: &[String], cols: Option<&[String]>, rows: Vec<Vec<Value>>) -> Result<usize, SqlError> {
        let declined = {
            let shared = &self.server.shared;
            let guard = SrvGuard::new(&shared.state);
            let mut eng = Engine::new(guard, &mut self.session);
            eng.try_load_rows(table, cols, rows)?
        };
        match declined {
            Ok(n) => Ok(n),
            // the ordinary INSERT, with everything an INSERT does (and the statement's own commit, when there is no transaction)
            Err(rows) => {
                let n = rows.len();
                let values: Vec<Vec<crate::ast::Expr>> = rows.into_iter().map(|r| r.into_iter().map(crate::ast::Expr::Lit).collect()).collect();
                let ins = crate::ast::Stmt::Insert(Box::new(crate::ast::Insert { ctes: vec![], target: crate::ast::Target::Table(table.to_vec()), cols: cols.map(|c| c.to_vec()), output: None, src: crate::ast::InsertSrc::Values(values), top: None }));
                match self.execute_stmts(&[ins]).first_error() {
                    Some(e) => Err(e.clone()),
                    None => Ok(n),
                }
            }
        }
    }

    /// Execute statements that are already parsed as one batch (a driver that builds statements itself, like the bulk load
    /// of the TDS listener, avoids the text).
    pub fn execute_stmts(&mut self, stmts: &[crate::ast::Stmt]) -> BatchResult {
        self.sync_interceptor_db();
        if let Some(i) = self.interceptor.as_mut() {
            let mut events = vec![];
            let handled = i.handle_stmts(stmts, &mut |item| {
                if let Streamed::Event(e) = item {
                    events.push(e);
                }
                true
            });
            if handled {
                if let Some(db) = i.database_after() {
                    self.session.db = db;
                }
                return BatchResult { events };
            }
        }
        let shared = &self.server.shared;
        let guard = SrvGuard::new(&shared.state);
        let mut eng = Engine::new(guard, &mut self.session);
        eng.run_parsed("(bulk load)", stmts);
        BatchResult { events: eng.out }
    }

    /// Execute a single batch, reporting its output to `sink` as it is produced instead of collecting it: messages,
    /// errors and row counts as [`Streamed::Event`], and the rows of a plain `SELECT` over a big table or join one at
    /// a time (`Header`, `Row`..., `End`), so the result set is never held in memory. `sink` returns false when the
    /// consumer has gone away; the statement is then abandoned. Small results and every other kind of result set
    /// arrive whole, as `Event::Rows`.
    pub fn execute_streaming(&mut self, sql: &str, sink: &mut dyn FnMut(Streamed) -> bool) {
        if let Some(e) = self.hand_over_temps(sql) {
            sink(Streamed::Event(Event::Error(e)));
            return;
        }
        if let Some(i) = self.interceptor.as_mut() {
            if i.handle(sql, sink) {
                if let Some(db) = i.database_after() {
                    self.session.db = db;
                }
                return;
            }
        }
        let shared = &self.server.shared;
        let guard = SrvGuard::new(&shared.state);
        let mut eng = Engine::with_sink(guard, &mut self.session, sink);
        eng.run_batch(sql);
    }

    /// Execute a script containing `GO` batch separators.
    pub fn execute_script(&mut self, script: &str) -> Vec<BatchResult> {
        let mut out = vec![];
        for (batch, count) in lexer::split_batches(script) {
            for _ in 0..count {
                out.push(self.execute(&batch));
            }
        }
        out
    }

    /// Run a batch and return the first result set's rows, or the first error.
    pub fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, SqlError> {
        let res = self.execute(sql);
        if let Some(e) = res.first_error() {
            return Err(e.clone());
        }
        Ok(res.resultsets().first().map(|r| r.rows.clone()).unwrap_or_default())
    }

    /// Run a batch, returning an error if any statement failed.
    pub fn run(&mut self, sql: &str) -> Result<BatchResult, SqlError> {
        let res = self.execute(sql);
        match res.first_error() {
            Some(e) => Err(e.clone()),
            None => Ok(res),
        }
    }

    /// The optimizer's decisions for the last statement that ran (access paths, join order and
    /// methods, with row counts).
    pub fn last_plan(&self) -> Vec<String> {
        self.session.plan.clone()
    }

    /// Describe how `sql` would be executed without running it (like `SET SHOWPLAN_TEXT ON`).
    pub fn explain(&mut self, sql: &str) -> Result<Vec<String>, SqlError> {
        self.session.showplan = true;
        let r = self.execute(sql);
        self.session.showplan = false;
        if let Some(e) = r.first_error() {
            return Err(e.clone());
        }
        Ok(r.resultsets().iter().flat_map(|rs| rs.rows.iter().map(|row| row[0].to_string())).collect())
    }

    /// Run the server's LOGON triggers; an error means the login is refused (the session is dropped by the caller).
    fn run_logon_triggers(&mut self) -> Result<(), SqlError> {
        {
            let g = self.server.shared.state.read().unwrap_or_else(|e| e.into_inner());
            if g.security.server_triggers.is_empty() {
                return Ok(());
            }
        }
        let shared = &self.server.shared;
        let guard = SrvGuard::new(&shared.state);
        let mut eng = Engine::new(guard, &mut self.session);
        eng.run_logon_triggers()
    }

    pub fn current_database(&self) -> String {
        self.session.db.clone()
    }

    /// Say how this connection came in, for `CONNECTIONPROPERTY` (the TDS server does, for each connection it accepts).
    pub fn set_connection_info(&mut self, info: engine::ConnInfo) {
        self.session.conn_info = Some(info);
    }
}

impl Connection {
    /// The session id (`@@SPID`).
    pub fn spid(&self) -> i32 {
        self.session.spid
    }

    /// Has another session asked this one to stop (`KILL`)? Listeners check this while a client is idle.
    pub fn is_killed(&self) -> bool {
        self.server.shared.state.read().unwrap_or_else(|e| e.into_inner()).core.is_killed(self.session.spid)
    }

    /// Two-phase commit, phase one: make this session's open transaction durable under `gid` (a log record, replicated on a
    /// replica) without committing it; it stays open with its locks. After this the server can finish the commit even if it
    /// restarts or fails over first: `COMMIT TRANSACTION` commits it, `ROLLBACK TRANSACTION` aborts it, and if the session
    /// ends or the process dies it is in doubt ([`Server::in_doubt`]) until [`Server::resolve_prepared`]. Returns false when
    /// the transaction changed nothing (there is nothing to wait for).
    pub fn prepare_transaction(&mut self, gid: u64) -> Result<bool, SqlError> {
        let shared = &self.server.shared;
        let guard = SrvGuard::new(&shared.state);
        let mut eng = Engine::new(guard, &mut self.session);
        eng.prepare_tx(gid)
    }

    /// Roll back whatever a killed session had open and forget the kill request.
    pub fn finish_kill(&mut self) {
        let shared = &self.server.shared;
        let guard = SrvGuard::new(&shared.state);
        let mut eng = Engine::new(guard, &mut self.session);
        eng.rollback_all();
        eng.srv.core.clear_kill(eng.ses.spid);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        {
            let g = self.server.shared.state.read().unwrap_or_else(|e| e.into_inner());
            g.core.unregister_session(self.session.spid);
            g.core.retire_stats(&self.session.stat_buf);
            if g.core.audit.is_active() {
                engine::auditing::login_event(&g, "LOGOUT", &self.session.login, self.session.spid, "");
            }
        }
        // roll back any open transaction and release its locks
        if self.session.tx.is_some() {
            let shared = &self.server.shared;
            let guard = SrvGuard::new(&shared.state);
            let mut eng = Engine::new(guard, &mut self.session);
            eng.rollback_all();
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", convert::display(self))
    }
}
