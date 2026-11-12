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
pub mod parser;
pub mod persist;
pub mod prof;
pub mod security;
pub mod sqlbak;
pub mod storage;
pub mod tds;
pub mod tools;
pub mod types;
pub mod xmlpath;
pub mod fulltext;
pub mod hierarchyid;
pub mod spatial;
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
}

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
        Server { shared: Arc::new(Shared { state: RwLock::new(ServerState::new()), crashed: Default::default() }) }
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
        Ok(Server { shared: Arc::new(Shared { state: RwLock::new(state), crashed: Default::default() }) })
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
        self.session_for("sa")
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
        Connection { server: self.clone(), session }
    }
}

/// A session against a [`Server`].
pub struct Connection {
    server: Server,
    session: SessionState,
}

impl Connection {
    /// Execute a single batch (no `GO` handling).
    pub fn execute(&mut self, sql: &str) -> BatchResult {
        crate::span!("execute");
        let shared = &self.server.shared;
        let guard = SrvGuard::new(&shared.state);
        let mut eng = Engine::new(guard, &mut self.session);
        eng.run_batch(sql);
        BatchResult { events: eng.out }
    }

    /// Execute statements that are already parsed as one batch (a driver that builds statements itself, like the bulk load
    /// of the TDS listener, avoids the text).
    pub fn execute_stmts(&mut self, stmts: &[crate::ast::Stmt]) -> BatchResult {
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
