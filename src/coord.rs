//! Coordination state that does not belong to the catalog: the commit clock, id counters, the table of running
//! transactions and the lock table.
//!
//! These live in a [`Core`] shared by every session and have their own latches, so they can be used without holding
//! the catalog lock (see `docs/concurrency.md`, "Latching"). Every method here takes `&self` and holds its latch only
//! for the duration of the call; nothing here ever calls back into the engine, so the latches cannot take part in a
//! deadlock.
//!
//! Latch order (outermost first): catalog, then the lock table, then `active`. A waiting session holds the lock
//! table while it tests `active`; the code that ends a transaction takes the two one after the other, never nested.
use crate::catalog::TxInfo;
use crate::storage::lock::{KeyRange, LockManager, Mode, Res};
use crate::storage::TxId;
use crate::types::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The lock manager behind its own latch, with a way to wait for other transactions to end.
///
/// A waiter calls [`LockTable::wait_until`] with a test that says whether the transactions it waits for have ended.
/// The test runs while the latch is held, and every release takes the latch before it wakes anyone, so a transaction
/// cannot end between "the test said no" and "the waiter went to sleep" (the wake-up would be lost).
#[derive(Default)]
pub struct LockTable {
    m: Mutex<LockManager>,
    cv: Condvar,
}

impl LockTable {
    /// Sleep until `done()` is true; returns true when `timeout` elapsed first.
    pub fn wait_until(&self, done: impl Fn() -> bool, timeout: Option<Duration>) -> bool {
        let mut g = lock(&self.m);
        let deadline = timeout.map(|d| std::time::Instant::now() + d);
        while !done() {
            match deadline {
                None => g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner()),
                Some(dl) => {
                    let now = std::time::Instant::now();
                    if now >= dl {
                        return true;
                    }
                    g = self.cv.wait_timeout(g, dl - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
        false
    }

    /// Wake every session waiting in [`LockTable::wait_until`] so it re-tests its condition.
    pub fn wake_all(&self) {
        let _g = lock(&self.m);
        self.cv.notify_all();
    }

    pub fn conflicts(&self, tx: TxId, res: &Res, mode: Mode) -> Vec<TxId> {
        lock(&self.m).conflicts(tx, res, mode)
    }
    pub fn acquire(&self, tx: TxId, res: &Res, mode: Mode) -> Result<(), Vec<TxId>> {
        lock(&self.m).acquire(tx, res, mode)
    }
    pub fn acquire_range(&self, r: KeyRange) {
        lock(&self.m).acquire_range(r)
    }
    pub fn has_ranges(&self) -> bool {
        lock(&self.m).has_ranges()
    }
    pub fn range_conflicts(&self, tx: TxId, oid: i32, idx: usize, key: &[Value]) -> Vec<TxId> {
        lock(&self.m).range_conflicts(tx, oid, idx, key)
    }
    /// Release everything `tx` holds and wake the sessions waiting for locks.
    pub fn release_all(&self, tx: TxId) {
        let mut g = lock(&self.m);
        g.release_all(tx);
        drop(g);
        self.cv.notify_all();
    }
    /// Record that `tx` waits for `blockers` and say whether that closes a cycle (one step, so two transactions
    /// that each wait for the other cannot both miss the cycle).
    pub fn wait_and_check(&self, tx: TxId, blockers: Vec<TxId>) -> bool {
        let mut g = lock(&self.m);
        g.set_wait(tx, blockers);
        let dead = g.deadlocked(tx);
        if dead {
            g.clear_wait(tx);
        }
        dead
    }
    pub fn clear_wait(&self, tx: TxId) {
        lock(&self.m).clear_wait(tx)
    }
    pub fn lock_count(&self, tx: TxId) -> usize {
        lock(&self.m).lock_count(tx)
    }
    pub fn snapshot(&self) -> Vec<(Res, TxId, Mode)> {
        lock(&self.m).snapshot()
    }
}

/// Per-table latches for the physical part of a write (checking keys and constraints, then adding row versions),
/// shared by readers that must not miss a key range being locked. They are not the transaction locks of
/// [`LockTable`]: they are held for part of one statement, never while waiting for a transaction, and are always
/// taken in ascending table-id order, so they cannot deadlock.
#[derive(Default)]
pub struct Latches {
    /// table id -> (exclusive holder?, shared holders)
    m: Mutex<std::collections::HashMap<i64, (bool, u32)>>,
    cv: Condvar,
}

impl Latches {
    fn acquire(&self, oid: i64, excl: bool) {
        let mut g = lock(&self.m);
        loop {
            let e = g.entry(oid).or_insert((false, 0));
            let free = if excl { !e.0 && e.1 == 0 } else { !e.0 };
            if free {
                if excl {
                    e.0 = true;
                } else {
                    e.1 += 1;
                }
                return;
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
    fn release(&self, oid: i64, excl: bool) {
        let mut g = lock(&self.m);
        if let Some(e) = g.get_mut(&oid) {
            if excl {
                e.0 = false;
            } else {
                e.1 = e.1.saturating_sub(1);
            }
            if !e.0 && e.1 == 0 {
                g.remove(&oid);
            }
        }
        drop(g);
        self.cv.notify_all();
    }
}

/// The latch stripe for a key value of a table: `kind` tells unique-index keys (by index number) and foreign-key
/// values apart. Different keys may share a stripe (that only costs parallelism), equal keys always do.
pub fn stripe(oid: i32, kind: u32, key: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (oid, kind, key).hash(&mut h);
    Core::STRIPE_BASE + (h.finish() % 4096) as i64
}

/// Latches held until dropped.
pub struct LatchSet {
    core: std::sync::Arc<Core>,
    held: Vec<(i64, bool)>,
}

impl Drop for LatchSet {
    fn drop(&mut self) {
        for (oid, excl) in self.held.iter().rev() {
            self.core.latches.release(*oid, *excl);
        }
    }
}

/// Commit timestamps in flight: a timestamp is allocated when a commit is logged and the clock only reaches it
/// when it and every earlier timestamp have been stamped.
#[derive(Default)]
struct Commits {
    last: u64,
    pending: std::collections::BTreeSet<u64>,
}

/// Group commit: sessions that have appended their commit record wait here for it to reach the disk; one of them
/// (the leader) forces the log for everybody whose record is already written.
#[derive(Default)]
struct GroupSync {
    synced: u64,
    leader: bool,
    failed: Option<String>,
}

/// What the server knows about one connected session (for `sp_who`, `sys.dm_exec_sessions`, `KILL`).
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub spid: i32,
    pub login: String,
    pub host: String,
    pub app: String,
    pub db: String,
    pub status: &'static str,
    pub isolation: i32,
    pub login_time: chrono::NaiveDateTime,
    pub last_batch: chrono::NaiveDateTime,
    pub batches: u64,
    /// The statement being run (first 200 characters).
    pub command: String,
}

/// One `sp_configure` option.
#[derive(Clone, Debug)]
pub struct ConfigOption {
    pub name: &'static str,
    pub minimum: i64,
    pub maximum: i64,
    pub config: i64,
    pub run: i64,
    pub dynamic: bool,
    pub advanced: bool,
    pub description: &'static str,
}

fn default_config() -> Vec<ConfigOption> {
    let o = |name, minimum, maximum, value, advanced, description| ConfigOption { name, minimum, maximum, config: value, run: value, dynamic: true, advanced, description };
    vec![
        o("show advanced options", 0, 1, 0, false, "Show advanced options"),
        o("max degree of parallelism", 0, 32767, 0, true, "maximum degree of parallelism"),
        o("cost threshold for parallelism", 0, 32767, 5, true, "cost threshold for parallelism"),
        o("max server memory (MB)", 128, 2147483647, 2147483647, true, "Maximum size of server memory (MB)"),
        o("min server memory (MB)", 0, 2147483647, 0, true, "Minimum size of server memory (MB)"),
        o("user connections", 0, 32767, 0, true, "Number of user connections allowed"),
        o("recovery interval (min)", 0, 32767, 0, true, "Maximum recovery interval in minutes"),
        o("remote access", 0, 1, 1, true, "Allow remote access"),
        o("backup compression default", 0, 1, 0, false, "Enable compression of backups by default"),
        o("default language", 0, 9999, 0, false, "default language"),
        o("lock timeout default (ms)", -1, 2147483647, -1, true, "Default LOCK_TIMEOUT for new sessions (ms)"),
        o("query governor cost limit", 0, 2147483647, 0, true, "Maximum estimated cost allowed by query governor"),
        o("audit log enabled", 0, 1, 1, true, "Keep the in-memory audit log"),
        o("login lockout threshold", 1, 1000, 5, false, "Failed logins (of a login with CHECK_POLICY) before the account is locked"),
        o("login lockout minutes", 1, 1440, 15, false, "How long a locked-out account stays locked"),
        o("password max age (days)", 0, 3650, 42, false, "Age at which a password of a login with CHECK_EXPIRATION expires (0 = never)"),
        o("default login check_policy", 0, 1, 0, false, "Create new logins with CHECK_POLICY ON unless they say otherwise"),
    ]
}

/// Per-statement-text execution statistics (`sys.dm_exec_query_stats`).
#[derive(Clone, Debug)]
pub struct QueryStat {
    pub text: String,
    pub executions: u64,
    pub total_us: u64,
    pub last_us: u64,
    pub max_us: u64,
    pub rows: u64,
    pub created: chrono::NaiveDateTime,
    pub last: chrono::NaiveDateTime,
}

/// Clock, counters, running transactions and locks.
const STAT_SHARDS: usize = 16;

pub struct Core {
    clock: AtomicU64,
    next_tx: AtomicU64,
    next_object_id: AtomicI32,
    next_rowversion: AtomicU64,
    next_spid: AtomicI32,
    active: Mutex<BTreeMap<TxId, TxInfo>>,
    commit: Mutex<()>,
    sessions: Mutex<BTreeMap<i32, SessionInfo>>,
    killed: Mutex<std::collections::HashSet<i32>>,
    /// Number of sessions asked to stop (so the common case, none, costs no lock).
    kill_pending: AtomicUsize,
    config: Mutex<Vec<ConfigOption>>,
    /// Statement statistics, in shards by statement hash so sessions do not queue on one lock.
    stats: Vec<Mutex<std::collections::HashMap<u64, QueryStat>>>,
    commits: Mutex<Commits>,
    group: Mutex<GroupSync>,
    group_cv: Condvar,
    forces: AtomicU64,
    latches: Latches,
    pub locks: LockTable,
    /// Where audit records are written.
    pub audit: crate::audit::AuditSink,
}

impl Default for Core {
    fn default() -> Self {
        Core::new()
    }
}

impl Core {
    pub fn new() -> Core {
        Core {
            clock: AtomicU64::new(1),
            next_tx: AtomicU64::new(1),
            next_object_id: AtomicI32::new(100_000),
            next_rowversion: AtomicU64::new(1),
            next_spid: AtomicI32::new(51),
            active: Mutex::new(BTreeMap::new()),
            commit: Mutex::new(()),
            sessions: Mutex::new(BTreeMap::new()),
            killed: Mutex::new(Default::default()),
            kill_pending: AtomicUsize::new(0),
            config: Mutex::new(default_config()),
            stats: (0..STAT_SHARDS).map(|_| Mutex::new(Default::default())).collect(),
            commits: Mutex::new(Commits::default()),
            group: Mutex::new(GroupSync::default()),
            group_cv: Condvar::new(),
            forces: AtomicU64::new(0),
            latches: Latches::default(),
            locks: LockTable::default(),
            audit: Default::default(),
        }
    }

    /// Latch tables (`(table id, exclusive)`), in ascending id order whatever order they are given in.
    pub fn latch(self: &std::sync::Arc<Self>, tables: &[(i32, bool)]) -> LatchSet {
        let keys: Vec<(i64, bool)> = tables.iter().map(|(o, e)| (*o as i64, *e)).collect();
        self.latch_keys(&keys)
    }

    /// First key of the stripe range: stripes (see [`stripe`]) sort after every table id, so latching a table and
    /// then stripes in one call is in order.
    pub const STRIPE_BASE: i64 = 1 << 40;

    /// Latch arbitrary keys (table ids or stripes), in ascending order whatever order they are given in.
    pub fn latch_keys(self: &std::sync::Arc<Self>, keys: &[(i64, bool)]) -> LatchSet {
        let mut t: Vec<(i64, bool)> = keys.to_vec();
        t.sort_unstable();
        // one latch per table, exclusive if any request for it is
        let mut v: Vec<(i64, bool)> = vec![];
        for (oid, excl) in t {
            match v.last_mut() {
                Some(l) if l.0 == oid => l.1 |= excl,
                _ => v.push((oid, excl)),
            }
        }
        for (oid, excl) in &v {
            self.latches.acquire(*oid, *excl);
        }
        LatchSet { core: self.clone(), held: v }
    }

    // ---- sessions
    pub fn register_session(&self, s: SessionInfo) {
        lock(&self.sessions).insert(s.spid, s);
    }
    pub fn unregister_session(&self, spid: i32) {
        lock(&self.sessions).remove(&spid);
        self.unkill(spid);
    }
    pub fn update_session(&self, spid: i32, f: impl FnOnce(&mut SessionInfo)) {
        if let Some(s) = lock(&self.sessions).get_mut(&spid) {
            f(s);
        }
    }
    pub fn session_list(&self) -> Vec<SessionInfo> {
        lock(&self.sessions).values().cloned().collect()
    }
    pub fn session_exists(&self, spid: i32) -> bool {
        lock(&self.sessions).contains_key(&spid)
    }
    /// Ask session `spid` to stop: its current statement ends with an error and its transaction rolls back.
    pub fn kill_session(&self, spid: i32) {
        {
            let mut k = lock(&self.killed);
            k.insert(spid);
            self.kill_pending.store(k.len(), Ordering::SeqCst);
        }
        // wake it if it waits for a lock
        self.locks.wake_all();
    }
    pub fn is_killed(&self, spid: i32) -> bool {
        self.kill_pending.load(Ordering::SeqCst) != 0 && lock(&self.killed).contains(&spid)
    }
    pub fn clear_kill(&self, spid: i32) {
        self.unkill(spid);
    }
    fn unkill(&self, spid: i32) {
        let mut k = lock(&self.killed);
        k.remove(&spid);
        self.kill_pending.store(k.len(), Ordering::SeqCst);
    }

    // ---- configuration (sp_configure)
    pub fn config_list(&self) -> Vec<ConfigOption> {
        lock(&self.config).clone()
    }
    pub fn config_get(&self, name: &str) -> Option<ConfigOption> {
        lock(&self.config).iter().find(|o| o.name.eq_ignore_ascii_case(name)).cloned()
    }
    pub fn config_set(&self, name: &str, v: i64) {
        if let Some(o) = lock(&self.config).iter_mut().find(|o| o.name.eq_ignore_ascii_case(name)) {
            o.config = v;
        }
    }
    /// RECONFIGURE: configured values become the running ones.
    pub fn config_apply(&self) {
        for o in lock(&self.config).iter_mut() {
            o.run = o.config;
        }
    }
    pub fn run_value(&self, name: &str) -> i64 {
        self.config_get(name).map(|o| o.run).unwrap_or(0)
    }

    // ---- statement statistics
    pub fn record_stat(&self, text: &str, us: u64, rows: u64) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let now = chrono::Local::now().naive_local();
        let mut m = lock(&self.stats[(h.finish() % STAT_SHARDS as u64) as usize]);
        if m.len() > 5000 / STAT_SHARDS && !m.contains_key(&h.finish()) {
            return;
        }
        let e = m.entry(h.finish()).or_insert_with(|| QueryStat { text: text.chars().take(4000).collect(), executions: 0, total_us: 0, last_us: 0, max_us: 0, rows: 0, created: now, last: now });
        e.executions += 1;
        e.total_us += us;
        e.last_us = us;
        e.max_us = e.max_us.max(us);
        e.rows += rows;
        e.last = now;
    }
    pub fn stats_list(&self) -> Vec<QueryStat> {
        self.stats.iter().flat_map(|m| lock(m).values().cloned().collect::<Vec<_>>()).collect()
    }
    pub fn clear_stats(&self) {
        for m in &self.stats {
            lock(m).clear();
        }
    }

    // ---- clock
    /// The timestamp of the newest commit.
    pub fn clock(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }
    /// Move the clock forward to `ts` (never back).
    pub fn advance_clock(&self, ts: u64) {
        self.clock.fetch_max(ts, Ordering::SeqCst);
    }

    // ---- counters
    pub fn next_spid(&self) -> i32 {
        self.next_spid.fetch_add(1, Ordering::SeqCst) + 1
    }
    pub fn object_id(&self) -> i32 {
        self.next_object_id.load(Ordering::SeqCst)
    }
    pub fn raise_object_id(&self, v: i32) {
        self.next_object_id.fetch_max(v, Ordering::SeqCst);
    }
    pub fn new_object_id(&self) -> i32 {
        self.next_object_id.fetch_add(1, Ordering::SeqCst) + 1
    }
    /// The newest rowversion handed out (`@@DBTS`).
    pub fn rowversion(&self) -> u64 {
        self.next_rowversion.load(Ordering::SeqCst)
    }
    pub fn raise_rowversion(&self, v: u64) {
        self.next_rowversion.fetch_max(v, Ordering::SeqCst);
    }
    pub fn new_rowversion(&self) -> u64 {
        self.next_rowversion.fetch_add(1, Ordering::SeqCst) + 1
    }

    // ---- running transactions
    /// Register a new transaction. A snapshot transaction reads the clock and enters the active table in one step
    /// (under the table's latch), so [`Core::horizon`] can never pass a snapshot that is just starting.
    /// Returns the id and the clock value at the start.
    pub fn begin(&self, snapshot: bool) -> (TxId, u64) {
        let mut a = lock(&self.active);
        let id = self.next_tx.fetch_add(1, Ordering::SeqCst);
        let start = self.clock();
        a.insert(id, TxInfo { snapshot_ts: if snapshot { Some(start) } else { None }, ddl: false });
        (id, start)
    }
    /// Take the next commit timestamp. Call with the commit lock held, so that the order of timestamps is the
    /// order of the log records written under it.
    pub fn alloc_commit_ts(&self) -> u64 {
        let mut c = lock(&self.commits);
        c.last = c.last.max(self.clock()) + 1;
        let ts = c.last;
        c.pending.insert(ts);
        ts
    }
    /// The commit with timestamp `ts` has stamped its versions (or gave up): publish every timestamp up to the
    /// first one still in flight as the clock, so a snapshot never sees half a commit.
    pub fn finish_commit_ts(&self, ts: u64) {
        let mut c = lock(&self.commits);
        c.pending.remove(&ts);
        let upto = c.pending.iter().next().map(|p| p - 1).unwrap_or(c.last);
        drop(c);
        self.advance_clock(upto);
    }
    /// Wait until the log is on disk up to record `lsn`. `sync` forces the log and returns the number of the last
    /// record it covered; only one session does it at a time, and the sessions that arrive while it does are
    /// covered by the next force, so a burst of commits costs few forces.
    pub fn sync_log(&self, lsn: u64, mut sync: impl FnMut() -> Result<u64, String>) -> Result<(), String> {
        let mut g = lock(&self.group);
        loop {
            if let Some(e) = &g.failed {
                return Err(e.clone());
            }
            if g.synced >= lsn {
                return Ok(());
            }
            if g.leader {
                g = self.group_cv.wait(g).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            g.leader = true;
            drop(g);
            self.forces.fetch_add(1, Ordering::SeqCst);
            let r = sync();
            g = lock(&self.group);
            g.leader = false;
            match r {
                Ok(upto) => g.synced = g.synced.max(upto),
                Err(e) => g.failed = Some(e),
            }
            self.group_cv.notify_all();
        }
    }
    /// How many times the log has been forced to disk for commits.
    pub fn log_forces(&self) -> u64 {
        self.forces.load(Ordering::SeqCst)
    }
    /// Held while a commit takes its timestamp and writes its log record: commits are logged one at a time, so the
    /// log order is the timestamp order.
    pub fn commit_lock(&self) -> MutexGuard<'_, ()> {
        lock(&self.commit)
    }
    pub fn end(&self, id: TxId) {
        lock(&self.active).remove(&id);
    }
    /// Note that transaction `id` has changed the catalog.
    pub fn mark_ddl(&self, id: TxId) {
        if let Some(t) = lock(&self.active).get_mut(&id) {
            t.ddl = true;
        }
    }
    /// Running transactions (other than `except`) that have changed the catalog and not finished.
    pub fn ddl_txs(&self, except: TxId) -> Vec<TxId> {
        lock(&self.active).iter().filter(|(id, t)| t.ddl && **id != except).map(|(id, _)| *id).collect()
    }
    /// Running transactions other than `except`.
    pub fn others_active(&self, except: TxId) -> Vec<TxId> {
        lock(&self.active).keys().copied().filter(|id| *id != except).collect()
    }
    pub fn no_active(&self) -> bool {
        lock(&self.active).is_empty()
    }
    pub fn is_active(&self, id: TxId) -> bool {
        lock(&self.active).contains_key(&id)
    }
    /// Wait until none of `txs` is running any more; returns true when `timeout` elapsed first. A transaction ends
    /// by leaving the active table and then releasing its locks, so the release that wakes this wait always
    /// follows the removal the test looks for.
    pub fn wait_for_end(&self, txs: &[TxId], timeout: Option<Duration>, spid: i32) -> bool {
        if txs.is_empty() {
            std::thread::sleep(Duration::from_millis(1));
            return false;
        }
        self.locks.wait_until(|| self.is_killed(spid) || !txs.iter().any(|t| self.is_active(*t)), timeout)
    }
    pub fn active_count(&self) -> usize {
        lock(&self.active).len()
    }
    pub fn active_ids(&self) -> Vec<TxId> {
        lock(&self.active).keys().copied().collect()
    }
    pub fn tx_snapshot(&self, id: TxId) -> Option<Option<u64>> {
        lock(&self.active).get(&id).map(|t| t.snapshot_ts)
    }
    /// Oldest timestamp any running snapshot still needs.
    pub fn horizon(&self) -> u64 {
        let a = lock(&self.active);
        a.values().filter_map(|t| t.snapshot_ts).min().unwrap_or_else(|| self.clock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    #[test]
    fn sessions_that_commit_together_share_forces() {
        let core = Arc::new(Core::new());
        let written = Arc::new(AtomicU64::new(0));
        let forced = Arc::new(AtomicU64::new(0));
        let mut hs = vec![];
        for _ in 0..16 {
            let (core, written, forced) = (core.clone(), written.clone(), forced.clone());
            hs.push(std::thread::spawn(move || {
                for _ in 0..10 {
                    let lsn = written.fetch_add(1, Ordering::SeqCst) + 1;
                    core.sync_log(lsn, || {
                        let upto = written.load(Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(2)); // a slow disk
                        forced.fetch_add(1, Ordering::SeqCst);
                        Ok(upto)
                    })
                    .unwrap();
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        let n = forced.load(Ordering::SeqCst);
        assert!(n < 160 / 2, "{} forces for 160 commits", n);
        assert_eq!(n, core.log_forces());
    }

    #[test]
    fn a_failed_force_fails_every_waiter() {
        let core = Core::new();
        assert!(core.sync_log(1, || Err("disk gone".to_string())).is_err());
        assert!(core.sync_log(2, || Ok(2)).is_err());
    }

    #[test]
    fn the_clock_waits_for_earlier_commits() {
        let core = Core::new();
        let (a, b) = (core.alloc_commit_ts(), core.alloc_commit_ts());
        assert_eq!((a + 1, core.clock()), (b, 1));
        core.finish_commit_ts(b);
        assert_eq!(core.clock(), 1, "the later commit must not be visible before the earlier one is stamped");
        core.finish_commit_ts(a);
        assert_eq!(core.clock(), b);
    }
}
