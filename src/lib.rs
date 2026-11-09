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
pub mod catalog;
pub mod convert;
pub mod decimal;
pub mod engine;
pub mod error;
pub mod inspect;
pub mod lexer;
pub mod parser;
pub mod persist;
pub mod security;
pub mod storage;
pub mod tds;
pub mod tools;
pub mod types;

use catalog::ServerState;
use engine::{Engine, SessionState, SrvGuard};
use std::sync::{Arc, Condvar, Mutex};

pub use engine::{BatchResult, Event, ResultSet};
pub use error::SqlError;
pub use persist::StorageOptions;
pub use storage::StoreStats;
pub use types::{DataType, Value};

struct Shared {
    state: Mutex<ServerState>,
    cv: Condvar,
    crashed: std::sync::atomic::AtomicBool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // clean shutdown: fold the log into a checkpoint (skipped when simulating a crash)
        let crashed = self.crashed.load(std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut g) = self.state.lock() {
            if !crashed && g.active.is_empty() {
                if let Some(mut p) = g.persist.take() {
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
        Server { shared: Arc::new(Shared { state: Mutex::new(ServerState::new()), cv: Condvar::new(), crashed: Default::default() }) }
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
        state.persist = Some(p);
        Ok(Server { shared: Arc::new(Shared { state: Mutex::new(state), cv: Condvar::new(), crashed: Default::default() }) })
    }

    /// Drop the server as if the process had been killed: no final checkpoint is written, so the
    /// next [`Server::open`] has to recover from the log. Intended for tests.
    #[doc(hidden)]
    pub fn simulate_crash(self) {
        self.shared.crashed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Force a checkpoint now (a no-op for in-memory servers or while transactions are open).
    pub fn checkpoint(&self) {
        let mut g = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if g.active.is_empty() {
            if let Some(mut p) = g.persist.take() {
                let _ = p.checkpoint(&g);
                g.persist = Some(p);
            }
        }
    }

    /// Write every dirty page to the page file but do not write a checkpoint, as if the process
    /// died halfway through a checkpoint. Intended for tests.
    #[doc(hidden)]
    pub fn flush_pages_only(&self) {
        let g = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let _ = g.store.prepare_checkpoint();
    }

    /// Buffer pool and page file statistics: `(persistent tables, temporary tables)`.
    pub fn storage_stats(&self) -> (StoreStats, StoreStats) {
        let g = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        (g.store.stats(), g.temp_store.stats())
    }

    /// Open a trusted session as the built-in `sa` (sysadmin) login.
    pub fn connect(&self) -> Connection {
        self.session_for("sa")
    }

    /// Authenticate with a login name and password.
    pub fn connect_as(&self, login: &str, password: &str) -> Result<Connection, SqlError> {
        {
            let g = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let ok = g.security.logins.get(&login.to_lowercase()).map(|l| !l.disabled && l.check_password(password)).unwrap_or(false);
            if !ok {
                return Err(SqlError::with(18456, 14, 1, format!("Login failed for user '{}'.", login)));
            }
        }
        Ok(self.session_for(login))
    }

    fn session_for(&self, login: &str) -> Connection {
        let mut g = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        g.next_spid += 1;
        let spid = g.next_spid;
        let mut session = SessionState::new(spid);
        let canonical = g.security.logins.get(&login.to_lowercase()).map(|l| l.name.clone()).unwrap_or_else(|| login.to_string());
        session.login = canonical.clone();
        if let Some(l) = g.security.logins.get(&login.to_lowercase()) {
            if g.databases.contains_key(&l.default_db.to_lowercase()) {
                session.db = g.databases[&l.default_db.to_lowercase()].name.clone();
            }
        }
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
        let shared = self.server.shared.clone();
        let guard = SrvGuard::lock(&shared.state, &shared.cv);
        let mut eng = Engine::new(guard, &mut self.session);
        eng.run_batch(sql);
        BatchResult { events: eng.out }
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

    pub fn current_database(&self) -> String {
        self.session.db.clone()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // roll back any open transaction and release its locks
        if self.session.tx.is_some() {
            let shared = self.server.shared.clone();
            let guard = SrvGuard::lock(&shared.state, &shared.cv);
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
