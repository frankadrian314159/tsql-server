//! The app's one database session, which can run a script on a worker thread so a long query does not freeze the window.
//!
//! The connection (a `Backend`) is moved to the worker while it runs and handed back with the results, so the transaction,
//! isolation level and locks stay those of a single session. While a script runs the session is *busy*: it cannot be
//! used (dereferencing it panics), so the UI must not touch it; the transaction count and login are cached for display.
use std::ops::{Deref, DerefMut};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use crate::backend::Backend;
use tsql_server::BatchResult;

type Conn = Box<dyn Backend>;

/// A finished background script.
pub struct Finished {
    pub results: Vec<BatchResult>,
    pub elapsed: Duration,
}

struct Job {
    rx: Receiver<(Conn, Finished)>,
    started: Instant,
}

pub struct Session {
    conn: Option<Conn>,
    job: Option<Job>,
    trancount: i32,
    login: String,
}

impl Session {
    pub fn new(conn: impl Backend + 'static) -> Session {
        let mut s = Session { conn: Some(Box::new(conn)), job: None, trancount: 0, login: String::new() };
        s.refresh_cache();
        s
    }

    fn refresh_cache(&mut self) {
        if let Some(c) = &self.conn {
            self.trancount = c.transaction_count();
            self.login = c.login();
        }
    }

    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    /// How long the running script has been going.
    pub fn running_for(&self) -> Option<Duration> {
        self.job.as_ref().map(|j| j.started.elapsed())
    }

    /// Open transactions on the session (the last known value while busy).
    pub fn transaction_count(&self) -> i32 {
        match &self.conn {
            Some(c) => c.transaction_count(),
            None => self.trancount,
        }
    }

    pub fn login(&self) -> String {
        match &self.conn {
            Some(c) => c.login(),
            None => self.login.clone(),
        }
    }

    /// Run a script (`GO`-separated) on a worker thread. `wake` is called when it finishes.
    pub fn start(&mut self, script: String, wake: impl FnOnce() + Send + 'static) {
        self.start_with(move |c| c.execute_script(&script), wake);
    }

    /// Run a job on the worker thread: any work on the connection that produces results (a bulk import, a backup).
    pub fn start_with(&mut self, job: impl FnOnce(&mut dyn Backend) -> Vec<BatchResult> + Send + 'static, wake: impl FnOnce() + Send + 'static) {
        self.refresh_cache();
        let mut conn = self.conn.take().expect("a script is already running");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let t = Instant::now();
            let results = job(&mut *conn);
            let _ = tx.send((conn, Finished { results, elapsed: t.elapsed() }));
            wake();
        });
        self.job = Some(Job { rx, started: Instant::now() });
    }

    /// The finished script, if there is one; the session is usable again afterwards.
    pub fn poll(&mut self) -> Option<Finished> {
        let job = self.job.as_ref()?;
        match job.rx.try_recv() {
            Ok((conn, fin)) => {
                self.conn = Some(conn);
                self.job = None;
                self.refresh_cache();
                Some(fin)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // the worker died (a panic): the connection is lost, so start a fresh one is the caller's job
                self.job = None;
                None
            }
        }
    }

    /// Block until the running script finishes (used by tests).
    #[allow(dead_code)]
    pub fn wait(&mut self) -> Option<Finished> {
        loop {
            if self.job.is_none() {
                return None;
            }
            if let Some(f) = self.poll() {
                return Some(f);
            }
            if self.job.is_none() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Replace the connection (after switching servers). Only valid while idle.
    pub fn replace(&mut self, conn: impl Backend + 'static) {
        self.conn = Some(Box::new(conn));
        self.job = None;
        self.refresh_cache();
    }

    /// Whether the connection was lost because the worker panicked.
    pub fn is_lost(&self) -> bool {
        self.conn.is_none() && self.job.is_none()
    }
}

impl Deref for Session {
    type Target = dyn Backend;
    fn deref(&self) -> &(dyn Backend + 'static) {
        &**self.conn.as_ref().expect("the session is busy running a script")
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut (dyn Backend + 'static) {
        &mut **self.conn.as_mut().expect("the session is busy running a script")
    }
}
