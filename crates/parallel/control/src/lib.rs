//! The control node of the distributed tsql-server.
//!
//! A control node speaks to a set of shard nodes (each a complete single-node engine, see the `tsql-storage` crate) and
//! makes them look like one server: a [`ClusterSession`] takes T-SQL, and routes, splits, scatters and merges it.
//!
//! How a statement is run (`ClusterSession::last_plan` says which way each went):
//! * **DDL** (`CREATE`/`DROP TABLE`, indexes, `SET`) runs on every shard and on the control node's own *mirror* engine,
//!   which therefore always knows the schema.
//! * **`INSERT`** is split by the shard key of each row (a hash of the key column, fixed for the life of the cluster) and
//!   each shard gets its rows in one statement; a *replicated* table's rows go to every shard.
//! * **`UPDATE` / `DELETE`** of one table go to the shards that can hold the rows (one, when the `WHERE` fixes the key)
//!   and the counts are added.
//! * **`SELECT`**: a query that fixes the key goes to one shard whole; a scan or aggregate over one sharded table is
//!   *scattered*: every shard runs a shard query over its rows and the control node merges the results with its own engine
//!   (two-phase aggregation, see `plan.rs`); everything else (joins, subqueries, windows, ...) *gathers*: the tables
//!   involved are copied into the mirror (kept until they change) and the query runs there. The gather path is the
//!   correctness baseline: it is slow for big tables but exact.
//!
//! What is not supported yet (an error says so): transactions that span statements, IDENTITY columns, foreign keys and
//! UNIQUE constraints that do not contain the shard key, views, procedures, triggers, ALTER TABLE, and DML with joins,
//! subqueries, OUTPUT or TOP. A statement that fails on some shards after succeeding on others (a multi-shard
//! `UPDATE`) leaves the shards that succeeded changed: atomic cross-shard transactions need the two-phase commit of a later
//! phase.
pub mod astutil;
pub mod client;
pub mod deadlock;
pub mod front;
pub mod joins;
pub mod merge;
pub mod plan;
pub mod rebalance;
pub mod replicas;
pub mod txn;

use astutil::*;
use client::ShardClient;
use futures::future::join_all;
use plan::Shape;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tsql_server::ast::*;
use tsql_server::{BatchResult, Connection, Event, ResultSet, Server, SqlError, Value};
use serde::{Deserialize, Serialize};
use tsql_shared_ces::route::{key_hash, slot_of, KeyClass, SLOTS};
use tsql_shared_ces::wire::{RunReply, ShardStatus, Work};
use tsql_shared_ces::ClusterConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColInfo {
    pub name: String,
    pub computed: bool,
    /// the lower-case name of the column's type
    pub type_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Dist {
    /// hash-partitioned on column `key` of the table
    Sharded { key: usize, class: KeyClass },
    /// the same rows on every shard (small reference tables)
    Replicated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DistTable {
    pub id: String,
    pub name: Name,
    pub cols: Vec<ColInfo>,
    pub dist: Dist,
    /// bumped by every change made through the control node
    pub version: u64,
    /// the version whose rows the mirror holds
    pub loaded: Option<u64>,
    /// (version, row count), for choosing how to run a join
    pub rows: Option<(u64, u64)>,
}

pub struct Cluster {
    rt: tokio::runtime::Runtime,
    shards: RwLock<Vec<ShardClient>>,
    /// which shard owns each hash slot
    slots: RwLock<Vec<u32>>,
    /// every successful CREATE / DROP of a table or index, in order: how a new shard (or a restarted control node) learns the
    /// schema
    schema_log: Mutex<Vec<Stmt>>,
    state_path: Option<std::path::PathBuf>,
    /// statements hold it for reading; moving slots between shards holds it for writing
    migration: RwLock<()>,
    mirror: Server,
    tables: Mutex<HashMap<String, DistTable>>,
    /// held while the mirror's copies of tables are loaded and queried
    gather: Mutex<()>,
    next_session: AtomicU64,
    /// the largest table (in rows) a join broadcasts to every shard
    broadcast_limit: AtomicU64,
    /// how long a statement of a transaction waits for a lock on a shard (ms); a deadlock between shards is broken by it
    lock_timeout_ms: AtomicU64,
    /// transactions whose commit was decided but not yet confirmed by every shard: (transaction id, shards still to finish)
    pending: Arc<Mutex<Vec<(u64, Vec<usize>)>>>,
    /// transactions of this control node that are being prepared right now (not yet decided)
    preparing: Arc<Mutex<std::collections::HashSet<u64>>>,
    /// the next distributed transaction id (unique over restarts, like session ids)
    next_gid: AtomicU64,
    /// for tests: called at named points of two-phase commit; returning true stops the commit there, as a crash would
    fault: Mutex<Option<FaultHook>>,
    /// how often the control node looks for deadlocks across shards (ms; 0: not at all), see `deadlock.rs`
    deadlock_check_ms: AtomicU64,
    /// sessions chosen as deadlock victims whose statement has not yet come back with the error
    victims: Mutex<std::collections::HashSet<u64>>,
    deadlocks: AtomicU64,
}

pub type FaultHook = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How a shard that finds a prepared transaction is told to settle it: committed when its commit was decided (it is in
/// `pending`), left alone while it is being prepared, aborted otherwise (presumed abort: the decision is recorded before any
/// commit is sent, so a transaction without one never committed anywhere).
fn make_verdict(pending: &Arc<Mutex<Vec<(u64, Vec<usize>)>>>, preparing: &Arc<Mutex<std::collections::HashSet<u64>>>) -> client::VerdictFn {
    let (p, g) = (pending.clone(), preparing.clone());
    Arc::new(move |gid| {
        if p.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(x, _)| *x == gid) {
            client::Verdict::Commit
        } else if g.lock().unwrap_or_else(|e| e.into_inner()).contains(&gid) {
            client::Verdict::Wait
        } else {
            client::Verdict::Abort
        }
    })
}

impl Cluster {
    pub fn connect(config: &ClusterConfig) -> Result<Arc<Cluster>, SqlError> {
        config.validate().map_err(|e| SqlError::new(50000, e))?;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().map_err(|e| SqlError::new(50000, e.to_string()))?;
        let pending = Arc::new(Mutex::new(vec![]));
        let preparing = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let verdict = make_verdict(&pending, &preparing);
        let shards = {
            let _g = rt.enter();
            config.shards.iter().map(|s| ShardClient::new(s.id, &s.addr).map(|c| c.with_verdict(verdict.clone()))).collect::<Result<Vec<_>, _>>()?
        };
        let n = shards.len();
        let cluster = Arc::new(Cluster {
            rt,
            shards: RwLock::new(shards),
            slots: RwLock::new((0..SLOTS).map(|i| (i % n) as u32).collect()),
            schema_log: Mutex::new(vec![]),
            state_path: None,
            migration: RwLock::new(()),
            mirror: Server::new(),
            tables: Mutex::new(HashMap::new()),
            gather: Mutex::new(()),
            next_session: AtomicU64::new(fresh_session_base()),
            broadcast_limit: AtomicU64::new(100_000),
            lock_timeout_ms: AtomicU64::new(5000),
            pending,
            preparing,
            next_gid: AtomicU64::new(fresh_session_base()),
            fault: Mutex::new(None),
            deadlock_check_ms: AtomicU64::new(200),
            victims: Mutex::new(Default::default()),
            deadlocks: AtomicU64::new(0),
        });
        cluster.start_deadlock_monitor();
        Ok(cluster)
    }

    /// For tests: a hook called at the points of two-phase commit (`"after_prepare"`: every participant has voted;
    /// `"after_decision"`: the decision is recorded, nothing is committed yet). It may do anything a failure would (stop a
    /// shard); returning true makes the commit stop there with error 50220, as if this control node had died.
    #[doc(hidden)]
    pub fn set_fault_hook(&self, hook: Option<FaultHook>) {
        *self.fault.lock().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    pub(crate) fn fault(&self, point: &str) -> bool {
        let h = self.fault.lock().unwrap_or_else(|e| e.into_inner()).clone();
        h.map(|h| h(point)).unwrap_or(false)
    }

    /// How this control node settles a prepared transaction a shard holds (see [`make_verdict`]).
    pub(crate) fn verdict_fn(&self) -> client::VerdictFn {
        make_verdict(&self.pending, &self.preparing)
    }

    /// How long (milliseconds) a statement of a transaction waits for a lock on a shard before it fails with error 1222. Two
    /// transactions that wait for each other on different shards are not seen by either shard's deadlock detector; the
    /// timeout breaks such a deadlock (the statement fails, and the transaction is rolled back, releasing its locks).
    pub fn set_lock_timeout_ms(&self, ms: u64) {
        self.lock_timeout_ms.store(ms, Ordering::Relaxed);
    }

    pub fn shard_count(&self) -> usize {
        self.shards.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub(crate) fn shard(&self, i: usize) -> ShardClient {
        self.shards.read().unwrap_or_else(|e| e.into_inner())[i].clone()
    }

    pub(crate) fn shard_list(&self) -> Vec<ShardClient> {
        self.shards.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The shard that owns the key with this hash (through the slot map).
    pub fn shard_of_hash(&self, h: u64) -> usize {
        self.slots.read().unwrap_or_else(|e| e.into_inner())[slot_of(h)] as usize
    }

    /// How many slots each shard owns.
    pub fn slot_counts(&self) -> Vec<usize> {
        let n = self.shard_count();
        let mut c = vec![0; n];
        for s in self.slots.read().unwrap_or_else(|e| e.into_inner()).iter() {
            if (*s as usize) < n {
                c[*s as usize] += 1;
            }
        }
        c
    }

    /// Tables of at most this many rows are broadcast to every shard to join with a sharded table (default 100,000); a
    /// bigger one is repartitioned instead.
    pub fn set_broadcast_limit(&self, rows: u64) {
        self.broadcast_limit.store(rows, Ordering::Relaxed);
    }

    pub fn session(self: &Arc<Self>) -> ClusterSession {
        ClusterSession { cluster: self.clone(), id: self.next_session.fetch_add(1, Ordering::Relaxed), mirror: self.mirror.connect_internal(), plan: vec![], temp_counter: 0, txn: std::cell::RefCell::new(None), stmt_shards: std::cell::Cell::new(0), commit_seq: 0, vars: HashMap::new() }
    }

    pub fn shard_status(&self) -> Vec<Result<ShardStatus, SqlError>> {
        self.rt.block_on(join_all(self.shard_list().iter().map(|s| s.status())))
    }

    /// The distributed tables the control node knows.
    pub fn tables(&self) -> Vec<DistTable> {
        let mut v: Vec<DistTable> = self.tables.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

pub struct ClusterSession {
    cluster: Arc<Cluster>,
    id: u64,
    mirror: Connection,
    plan: Vec<String>,
    temp_counter: u64,
    /// the open transaction, if any
    txn: std::cell::RefCell<Option<TxnState>>,
    /// the most shards one statement has used (a failed statement on several shards leaves a transaction half done)
    stmt_shards: std::cell::Cell<usize>,
    commit_seq: u64,
    /// the variables of the session (`DECLARE`, `SET`, the parameters of `sp_executesql`), see `front.rs`
    vars: HashMap<String, Value>,
}

/// A transaction of a control session: the shards that have begun it, and what it has changed.
struct TxnState {
    shards: std::collections::BTreeSet<usize>,
    depth: u32,
    doomed: bool,
    wrote: std::collections::HashSet<String>,
}

/// Session ids are unique over restarts of the control node (a shard may still hold the transaction of a session of an
/// earlier life): they start at the clock.
fn fresh_session_base() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(1) << 8
}

fn unsupported(what: &str) -> SqlError {
    SqlError::new(50100, format!("{} is not supported by the distributed engine yet.", what))
}

fn stmt_kind(st: &Stmt) -> String {
    let d = format!("{:?}", st);
    d.chars().take_while(|c| c.is_alphanumeric()).collect()
}

/// The shards that can hold rows matching `where_` on a table: one or several when the condition fixes the shard key
/// (`key = literal`, `key IN (literals)`), else all.
fn prune_shards(where_: Option<&Expr>, t: &DistTable, cluster: &Cluster) -> Vec<usize> {
    let n = cluster.shard_count();
    let all: Vec<usize> = (0..n).collect();
    let Dist::Sharded { key, class } = &t.dist else { return all };
    let Some(w) = where_ else { return all };
    let key_name = &t.cols[*key].name;
    fn conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::And(a, b) => {
                conjuncts(a, out);
                conjuncts(b, out);
            }
            Expr::Paren(a) => conjuncts(a, out),
            other => out.push(other),
        }
    }
    let is_key = |e: &Expr| matches!(e, Expr::Col(p) if p.last().map(|s| s.eq_ignore_ascii_case(key_name)).unwrap_or(false));
    let mut cs = vec![];
    conjuncts(w, &mut cs);
    let mut best: Option<Vec<usize>> = None;
    for c in cs {
        let shards: Option<Vec<usize>> = match c {
            Expr::Binary(BinOp::Eq, a, b) => match (&**a, &**b) {
                (x, Expr::Lit(v)) if is_key(x) => key_hash(v, *class).map(|h| vec![cluster.shard_of_hash(h)]),
                (Expr::Lit(v), x) if is_key(x) => key_hash(v, *class).map(|h| vec![cluster.shard_of_hash(h)]),
                _ => None,
            },
            Expr::InList(x, list, false) if is_key(x) => {
                let mut s: Vec<usize> = vec![];
                let mut ok = true;
                for it in list {
                    match it {
                        Expr::Lit(v) => match key_hash(v, *class) {
                            Some(h) => s.push(cluster.shard_of_hash(h)),
                            None => ok = false,
                        },
                        _ => ok = false,
                    }
                }
                s.sort();
                s.dedup();
                if ok {
                    Some(s)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(s) = shards {
            if best.as_ref().map(|b| s.len() < b.len()).unwrap_or(true) {
                best = Some(s);
            }
        }
    }
    best.unwrap_or(all)
}

fn query_where(q: &Query) -> Option<&Expr> {
    match &q.body {
        QueryBody::Select(s) => s.where_.as_ref(),
        _ => None,
    }
}

/// `DISTRIBUTE TABLE name BY column` / `DISTRIBUTE TABLE name REPLICATED`: choose how an empty table is spread.
enum DistCmd {
    By(Name, String),
    Replicated(Name),
}

fn parse_distribute(sql: &str) -> Option<Result<DistCmd, SqlError>> {
    let t = sql.trim().trim_end_matches(';').trim();
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.len() < 3 || !words[0].eq_ignore_ascii_case("DISTRIBUTE") || !words[1].eq_ignore_ascii_case("TABLE") {
        return None;
    }
    let clean = |s: &str| s.trim_matches(|c| c == '[' || c == ']' || c == '"').to_string();
    let name: Name = words[2].split('.').map(clean).collect();
    Some(if words.len() == 4 && words[3].eq_ignore_ascii_case("REPLICATED") {
        Ok(DistCmd::Replicated(name))
    } else if words.len() == 5 && words[3].eq_ignore_ascii_case("BY") {
        Ok(DistCmd::By(name, clean(words[4])))
    } else {
        Err(SqlError::new(102, "Incorrect syntax near DISTRIBUTE. Expected: DISTRIBUTE TABLE name BY column | DISTRIBUTE TABLE name REPLICATED"))
    })
}

impl ClusterSession {
    /// How the statements of the last `execute` were run.
    pub fn last_plan(&self) -> &[String] {
        &self.plan
    }

    pub fn cluster(&self) -> &Arc<Cluster> {
        &self.cluster
    }

    /// Run a batch; the result has the events of the engine (result sets, row counts, an error that stops the batch).
    pub fn execute(&mut self, sql: &str) -> BatchResult {
        self.plan.clear();
        let cl = self.cluster.clone();
        let mut events = vec![];
        if let Some(cmd) = parse_distribute(sql) {
            let _migration = cl.migration.read().unwrap_or_else(|e| e.into_inner());
            let r = match cmd {
                Ok(c) => cl.rt.block_on(self.distribute(c)),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                events.push(Event::Error(e));
            }
            return BatchResult { events };
        }
        match tsql_server::parser::parse(sql) {
            Ok(stmts) => self.run_stmts(&stmts),
            Err(e) => {
                events.push(Event::Error(e));
                BatchResult { events }
            }
        }
    }

    /// Run already-parsed statements (what `execute` does after parsing; also the way to insert rows without printing them
    /// as text first).
    pub fn execute_stmts(&mut self, stmts: &[Stmt]) -> BatchResult {
        self.plan.clear();
        self.run_stmts(stmts)
    }

    fn run_stmts(&mut self, stmts: &[Stmt]) -> BatchResult {
        let cl = self.cluster.clone();
        // (moving slots between shards waits for the statements that are running, and they for it)
        let _migration = cl.migration.read().unwrap_or_else(|e| e.into_inner());
        let mut events = vec![];
        for st in stmts {
            self.stmt_shards.set(0);
            if let Err(e) = cl.rt.block_on(self.exec_stmt(st, &mut events)) {
                // the victim of a deadlock across shards: its wait was ended on the shard, and the transaction is rolled back
                // everywhere, as SQL Server does (a deadlock inside one shard also comes back as 1205, with that shard's part
                // already rolled back)
                let mut e = e;
                if e.number == 1205 || (e.number == 596 && cl.victims.lock().unwrap_or_else(|x| x.into_inner()).remove(&self.id)) {
                    e = SqlError::with(1205, 13, 51, format!("Transaction (Process ID {}) was deadlocked on lock resources with another process and has been chosen as the deadlock victim. Rerun the transaction.", self.id));
                    e.abort_batch = true;
                    let tx = self.txn.borrow_mut().take();
                    if let Some(tx) = tx {
                        cl.rt.block_on(self.finish_rollback(&tx));
                    }
                    events.push(Event::Error(e));
                    break;
                }
                // a statement that failed after changing several shards, or because a shard failed, leaves the
                // transaction half done: it can only be rolled back
                let fatal = matches!(e.number, 1222 | 1205 | 40613 | 3314 | 50200 | 50210);
                if self.stmt_shards.get() > 1 || fatal {
                    if let Some(t) = self.txn.borrow_mut().as_mut() {
                        t.doomed = true;
                    }
                }
                events.push(Event::Error(e));
                break;
            }
        }
        BatchResult { events }
    }

    /// Whether a transaction is open.
    pub fn in_transaction(&self) -> bool {
        self.txn.borrow().is_some()
    }

    /// The rows of a query (the first result set).
    pub fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, SqlError> {
        let res = self.execute(sql);
        if let Some(e) = res.first_error() {
            return Err(e.clone());
        }
        Ok(res.resultsets().first().map(|r| r.rows.clone()).unwrap_or_default())
    }

    // ------------------------------------------------------------------ helpers

    fn table_by_id(&self, id: &str) -> Option<DistTable> {
        self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
    }

    fn table(&self, name: &Name) -> Result<DistTable, SqlError> {
        self.table_by_id(&table_id(name)).ok_or_else(|| SqlError::new(208, format!("Invalid object name '{}'.", name.join("."))))
    }

    fn bump(&self, id: &str) {
        if let Some(t) = self.txn.borrow_mut().as_mut() {
            t.wrote.insert(id.to_string());
        }
        if let Some(t) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(id) {
            t.version += 1;
        }
    }

    fn all_shards(&self) -> Vec<usize> {
        (0..self.cluster.shard_count()).collect()
    }

    /// In a transaction, begin it on each shard the statement is about to use for the first time.
    async fn ensure_begun(&self, shards: &[usize]) -> Result<(), SqlError> {
        self.stmt_shards.set(self.stmt_shards.get().max(shards.len()));
        let need: Vec<usize> = match self.txn.borrow().as_ref() {
            None => return Ok(()),
            Some(t) => shards.iter().copied().filter(|s| !t.shards.contains(s)).collect(),
        };
        if need.is_empty() {
            return Ok(());
        }
        let ms = self.cluster.lock_timeout_ms.load(Ordering::Relaxed);
        let clients: Vec<ShardClient> = need.iter().map(|&i| self.cluster.shard(i)).collect();
        let futs: Vec<_> = clients.iter().map(|c| c.run(self.id, Work::Sql(format!("SET LOCK_TIMEOUT {}; BEGIN TRANSACTION", ms)))).collect();
        for (r, s) in join_all(futs).await.into_iter().zip(&need) {
            let reply = r?;
            if let Some(e) = reply.first_error() {
                return Err(e.to_sql());
            }
            if let Some(t) = self.txn.borrow_mut().as_mut() {
                t.shards.insert(*s);
            }
        }
        Ok(())
    }

    async fn fan_run(&self, shards: &[usize], work: impl Fn(usize) -> Work) -> Result<Vec<RunReply>, SqlError> {
        self.ensure_begun(shards).await?;
        let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
        let futs: Vec<_> = clients.iter().zip(shards).map(|(c, &i)| c.run(self.id, work(i))).collect();
        let mut out = vec![];
        for r in join_all(futs).await {
            let reply = r?;
            if let Some(e) = reply.first_error() {
                return Err(e.to_sql());
            }
            out.push(reply);
        }
        Ok(out)
    }

    async fn fan_query(&self, shards: &[usize], work: impl Fn(usize) -> Work) -> Result<Vec<ResultSet>, SqlError> {
        self.ensure_begun(shards).await?;
        let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
        let futs: Vec<_> = clients.iter().zip(shards).map(|(c, &i)| c.query(self.id, work(i))).collect();
        join_all(futs).await.into_iter().collect()
    }

    /// Run a statement on the mirror first (which checks it), then on every shard.
    async fn broadcast(&mut self, st: &Stmt) -> Result<(), SqlError> {
        merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
        let shards = self.all_shards();
        self.fan_run(&shards, |_| Work::Stmts(vec![st.clone()])).await?;
        self.plan.push(format!("broadcast {} to {} shards", stmt_kind(st), shards.len()));
        Ok(())
    }

    // ------------------------------------------------------------------ statements

    async fn exec_stmt(&mut self, st: &Stmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        match st {
            Stmt::BeginTran(_) => {
                let mut t = self.txn.borrow_mut();
                match t.as_mut() {
                    Some(x) => x.depth += 1,
                    None => *t = Some(TxnState { shards: Default::default(), depth: 1, doomed: false, wrote: Default::default() }),
                }
                return Ok(());
            }
            Stmt::Commit(_) => return self.commit_txn(events).await,
            Stmt::Rollback(_) => return self.rollback_txn().await,
            Stmt::SaveTran(_) => return Err(unsupported("savepoints")),
            _ => {}
        }
        if let Some(t) = self.txn.borrow().as_ref() {
            if t.doomed {
                return Err(SqlError::new(3930, "The current transaction cannot be committed and cannot support operations that write to the log file. Roll back the transaction."));
            }
            if matches!(st, Stmt::CreateTable { .. } | Stmt::Drop { .. } | Stmt::CreateIndex { .. } | Stmt::DropIndex { .. } | Stmt::Truncate(_)) {
                return Err(unsupported("schema changes and TRUNCATE inside a transaction"));
            }
        }
        if let Stmt::Exec(ex) = st {
            if self.exec_executesql(ex, events).await? {
                return Ok(());
            }
        }
        if self.exec_variable_stmt(st).await? {
            return Ok(());
        }
        let bound = self.bind_vars(st)?;
        let st = &bound;
        match st {
            Stmt::CreateTable { name, defs, versioning } => self.create_table(st, name, defs, versioning.is_some()).await,
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("TABLE") => {
                self.broadcast(st).await?;
                let mut t = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
                for n in names {
                    t.remove(&table_id(n));
                }
                drop(t);
                self.cluster.log_schema(st);
                Ok(())
            }
            Stmt::CreateIndex { table, unique, cols, xml, spatial, .. } => {
                if xml.is_some() || spatial.is_some() {
                    return Err(unsupported("XML and spatial indexes"));
                }
                let t = self.table(table)?;
                if let (true, Dist::Sharded { key, .. }) = (*unique, &t.dist) {
                    if !cols.iter().any(|(c, _)| c.eq_ignore_ascii_case(&t.cols[*key].name)) {
                        return Err(unsupported(&format!("a unique index that does not contain the shard key ({})", t.cols[*key].name)));
                    }
                }
                self.broadcast(st).await?;
                self.cluster.log_schema(st);
                Ok(())
            }
            Stmt::DropIndex { .. } => {
                self.broadcast(st).await?;
                self.cluster.log_schema(st);
                Ok(())
            }
            Stmt::SetOpts(_) => self.broadcast(st).await,
            Stmt::Truncate(Target::Table(name)) => {
                let t = self.table(name)?;
                self.broadcast(st).await?;
                self.bump(&t.id);
                Ok(())
            }
            Stmt::Insert(ins) => self.insert(ins, events).await,
            Stmt::Update(u) => self.update(u, events).await,
            Stmt::Delete(d) => self.delete(d, events).await,
            Stmt::Query(q) => {
                let rs = self.run_query(q).await?;
                events.push(Event::Rows(rs));
                Ok(())
            }
            other => Err(unsupported(&format!("the statement {}", stmt_kind(other)))),
        }
    }

    async fn create_table(&mut self, st: &Stmt, name: &Name, defs: &[TableDef], versioned: bool) -> Result<(), SqlError> {
        if versioned {
            return Err(unsupported("system-versioned tables"));
        }
        let mut cols: Vec<ColInfo> = vec![];
        let mut pk: Vec<String> = vec![];
        let mut uniques: Vec<Vec<String>> = vec![];
        for d in defs {
            match d {
                TableDef::Column(cd) => {
                    if cd.identity.is_some() {
                        return Err(unsupported("IDENTITY columns (the shards would each hand out the same numbers)"));
                    }
                    if cd.fk.is_some() {
                        return Err(unsupported("FOREIGN KEY constraints"));
                    }
                    if cd.pk.is_some() {
                        pk = vec![cd.name.clone()];
                    }
                    if cd.unique.is_some() {
                        uniques.push(vec![cd.name.clone()]);
                    }
                    let type_name = cd.ty.as_ref().and_then(|t| t.parts.last()).map(|s| s.to_lowercase()).unwrap_or_default();
                    cols.push(ColInfo { name: cd.name.clone(), computed: cd.computed.is_some(), type_name });
                }
                TableDef::PrimaryKey { cols: c, .. } => pk = c.iter().map(|(n, _)| n.clone()).collect(),
                TableDef::Unique { cols: c, .. } => uniques.push(c.iter().map(|(n, _)| n.clone()).collect()),
                TableDef::Index { cols: c, unique: true, .. } => uniques.push(c.iter().map(|(n, _)| n.clone()).collect()),
                TableDef::ForeignKey(_) => return Err(unsupported("FOREIGN KEY constraints")),
                TableDef::Period { .. } => return Err(unsupported("system-versioned tables")),
                _ => {}
            }
        }
        // the shard key: the first column of the primary key, else the first column
        let key_name = pk.first().cloned().or_else(|| cols.iter().find(|c| !c.computed).map(|c| c.name.clone())).ok_or_else(|| SqlError::new(1113, "A table needs at least one column."))?;
        let key = cols.iter().position(|c| c.name.eq_ignore_ascii_case(&key_name)).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table.", key_name)))?;
        let class = KeyClass::of_type_name(&cols[key].type_name)
            .ok_or_else(|| unsupported(&format!("the shard key {} of type {} (use an integer, character or uniqueidentifier first key column, or DISTRIBUTE TABLE ... REPLICATED)", key_name, cols[key].type_name)))?;
        for u in &uniques {
            if !u.iter().any(|c| c.eq_ignore_ascii_case(&key_name)) {
                return Err(unsupported(&format!("a UNIQUE constraint that does not contain the shard key ({})", key_name)));
            }
        }
        if let Err(e) = self.broadcast(st).await {
            // the mirror (and perhaps some shards) made the table: take it back, so that nothing knows a table the
            // cluster does not have
            let undo = Stmt::Drop { what: "TABLE".into(), names: vec![name.clone()], if_exists: true };
            let _ = self.mirror.execute_stmts(std::slice::from_ref(&undo));
            let shards = self.all_shards();
            let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
            let futs: Vec<_> = clients.iter().map(|c| c.run(self.id, Work::Stmts(vec![undo.clone()]))).collect();
            let _ = join_all(futs).await;
            return Err(e);
        }
        let id = table_id(name);
        self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), DistTable { id, name: name.clone(), cols, dist: Dist::Sharded { key, class }, version: 0, loaded: None, rows: None });
        self.cluster.log_schema(st);
        Ok(())
    }

    async fn distribute(&mut self, cmd: DistCmd) -> Result<(), SqlError> {
        let (name, by) = match &cmd {
            DistCmd::By(n, c) => (n.clone(), Some(c.clone())),
            DistCmd::Replicated(n) => (n.clone(), None),
        };
        let mut t = self.table(&name)?;
        // only an empty table can change how it is spread
        let count = Query { ..simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: Expr::Func(Box::new(FuncCall { name: "COUNT".into(), schema: None, parts: vec![], args: vec![], distinct: false, star: true, over: None, within: None })), alias: None }], into: None, from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }], where_: None, group: None, having: None }) };
        let shards = self.all_shards();
        let counts = self.fan_query(&shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(count.clone()))])).await?;
        if counts.iter().any(|r| r.rows.first().map(|x| !matches!(x[0], Value::Int(0))).unwrap_or(false)) {
            return Err(SqlError::new(50101, format!("DISTRIBUTE TABLE needs an empty table; {} has rows.", name.join("."))));
        }
        t.dist = match by {
            None => Dist::Replicated,
            Some(c) => {
                let key = t.cols.iter().position(|x| x.name.eq_ignore_ascii_case(&c)).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", c)))?;
                let class = KeyClass::of_type_name(&t.cols[key].type_name).ok_or_else(|| unsupported(&format!("a shard key of type {}", t.cols[key].type_name)))?;
                Dist::Sharded { key, class }
            }
        };
        self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).insert(t.id.clone(), t);
        self.cluster.save_state();
        Ok(())
    }

    async fn eval_scalar(&mut self, e: &Expr) -> Result<Value, SqlError> {
        if let Expr::Lit(v) = e {
            return Ok(v.clone());
        }
        let q = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: e.clone(), alias: None }], into: None, from: vec![], where_: None, group: None, having: None });
        let res = self.mirror.execute_stmts(&[Stmt::Query(Box::new(q))]);
        merge::check(&res)?;
        res.resultsets().first().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().ok_or_else(|| SqlError::new(50000, "an expression returned no value"))
    }

    async fn insert(&mut self, ins: &Insert, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let Target::Table(name) = &ins.target else { return Err(unsupported("INSERT into a table variable")) };
        let t = self.table(name)?;
        if ins.output.is_some() || ins.top.is_some() || !ins.ctes.is_empty() {
            return Err(unsupported("INSERT with OUTPUT, TOP or CTEs"));
        }
        let rows: Vec<Vec<Expr>> = match &ins.src {
            InsertSrc::Values(v) => v.clone(),
            InsertSrc::Query(q) => {
                let rs = self.run_query(q).await?;
                rs.rows.into_iter().map(|r| r.into_iter().map(Expr::Lit).collect()).collect()
            }
            InsertSrc::DefaultValues => return Err(unsupported("INSERT ... DEFAULT VALUES")),
            InsertSrc::Exec(_) => return Err(unsupported("INSERT ... EXEC")),
        };
        let n = self.cluster.shard_count();
        let names: Vec<String> = ins.cols.clone().unwrap_or_else(|| t.cols.iter().filter(|c| !c.computed).map(|c| c.name.clone()).collect());
        if let Some(bad) = rows.iter().find(|r| r.len() != names.len()) {
            return Err(SqlError::new(213, format!("Column name or number of supplied values does not match table definition ({} values for {} columns).", bad.len(), names.len())));
        }
        let make = |rows: Vec<Vec<Expr>>| Stmt::Insert(Box::new(Insert { ctes: vec![], target: ins.target.clone(), cols: ins.cols.clone(), output: None, src: InsertSrc::Values(rows), top: None }));
        let count: i64;
        match &t.dist {
            Dist::Replicated => {
                let shards = self.all_shards();
                let st = make(rows);
                let replies = self.fan_run(&shards, |_| Work::Stmts(vec![st.clone()])).await?;
                count = replies.first().map(|r| r.row_count()).unwrap_or(0);
                self.plan.push(format!("insert into replicated table on {} shards", n));
            }
            Dist::Sharded { key, class } => {
                let key_pos = names.iter().position(|c| c.eq_ignore_ascii_case(&t.cols[*key].name)).ok_or_else(|| SqlError::new(50102, format!("The shard key column {} must be given a value.", t.cols[*key].name)))?;
                let mut per_shard: Vec<Vec<Vec<Expr>>> = vec![vec![]; n];
                for r in rows {
                    let v = self.eval_scalar(&r[key_pos]).await?;
                    let h = key_hash(&v, *class).ok_or_else(|| SqlError::new(245, format!("The value {} cannot be used as the shard key {}.", tsql_server::convert::display(&v), t.cols[*key].name)))?;
                    per_shard[self.cluster.shard_of_hash(h)].push(r);
                }
                let used: Vec<usize> = (0..n).filter(|i| !per_shard[*i].is_empty()).collect();
                let stmts: Vec<Stmt> = per_shard.into_iter().map(|rows| make(rows)).collect();
                let replies = self.fan_run(&used, |i| Work::Stmts(vec![stmts[i].clone()])).await?;
                count = replies.iter().map(|r| r.row_count()).sum();
                self.plan.push(format!("insert split over shards {:?}", used));
            }
        }
        self.bump(&t.id);
        events.push(Event::RowCount(count));
        Ok(())
    }

    /// The common checks of a single-table UPDATE or DELETE; the shards to send it to.
    fn dml_shards(&self, t: &DistTable, where_: Option<&Expr>) -> Vec<usize> {
        match t.dist {
            Dist::Replicated => self.all_shards(),
            Dist::Sharded { .. } => prune_shards(where_, t, &self.cluster),
        }
    }

    async fn run_dml(&mut self, t: &DistTable, st: Stmt, shards: Vec<usize>, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let replies = self.fan_run(&shards, |_| Work::Stmts(vec![st.clone()])).await?;
        let count = match t.dist {
            Dist::Replicated => replies.first().map(|r| r.row_count()).unwrap_or(0),
            Dist::Sharded { .. } => replies.iter().map(|r| r.row_count()).sum(),
        };
        self.plan.push(format!("{} on shards {:?}", stmt_kind(&st).to_lowercase(), shards));
        self.bump(&t.id);
        events.push(Event::RowCount(count));
        Ok(())
    }

    async fn update(&mut self, u: &Update, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let Target::Table(name) = &u.target else { return Err(unsupported("UPDATE of a table variable")) };
        let t = self.table(name)?;
        if !u.from.is_empty() || u.output.is_some() || u.top.is_some() || u.current_of.is_some() || !u.ctes.is_empty() {
            return Err(unsupported("UPDATE with FROM, OUTPUT, TOP, CURRENT OF or CTEs"));
        }
        let mut exprs: Vec<&Expr> = u.where_.iter().collect();
        for s in &u.sets {
            match s {
                SetItem::Col { parts, e, .. } => {
                    if let Dist::Sharded { key, .. } = &t.dist {
                        if parts.last().map(|p| p.eq_ignore_ascii_case(&t.cols[*key].name)).unwrap_or(false) {
                            return Err(unsupported("changing the shard key of a row"));
                        }
                    }
                    exprs.push(e);
                }
                SetItem::Var { .. } => return Err(unsupported("UPDATE that assigns variables")),
            }
        }
        if exprs.iter().any(|e| has_subquery(e) || has_window_or_session(e)) {
            return Err(unsupported("subqueries, variables or window functions in UPDATE"));
        }
        let shards = self.dml_shards(&t, u.where_.as_ref());
        self.run_dml(&t, Stmt::Update(Box::new(u.clone())), shards, events).await
    }

    async fn delete(&mut self, d: &Delete, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let Target::Table(name) = &d.target else { return Err(unsupported("DELETE from a table variable")) };
        let t = self.table(name)?;
        if !d.from.is_empty() || d.output.is_some() || d.top.is_some() || d.current_of.is_some() || !d.ctes.is_empty() {
            return Err(unsupported("DELETE with FROM, OUTPUT, TOP, CURRENT OF or CTEs"));
        }
        if let Some(w) = &d.where_ {
            if has_subquery(w) || has_window_or_session(w) {
                return Err(unsupported("subqueries or variables in DELETE"));
            }
        }
        let shards = self.dml_shards(&t, d.where_.as_ref());
        self.run_dml(&t, Stmt::Delete(Box::new(d.clone())), shards, events).await
    }

    // ------------------------------------------------------------------ queries

    async fn run_query(&mut self, q: &Query) -> Result<ResultSet, SqlError> {
        let ids = referenced_tables(q);
        let tables: Vec<DistTable> = ids.iter().filter_map(|i| self.table_by_id(i)).collect();
        if tables.is_empty() {
            self.plan.push("local: reads no distributed table".into());
            return self.mirror_query(q);
        }
        if tables.iter().all(|t| matches!(t.dist, Dist::Replicated)) {
            self.plan.push("direct: replicated tables, on shard 0".into());
            return self.direct(0, q).await;
        }
        if tables.len() == 1 && !has_nested_query(q) {
            let t = &tables[0];
            let shards = prune_shards(query_where(q), t, &self.cluster);
            if shards.len() == 1 {
                self.plan.push(format!("route: the key fixes the shard ({})", shards[0]));
                return self.direct(shards[0], q).await;
            }
            if let Some(shape) = plan_scatter_checked(q) {
                return self.scatter(shape, &shards).await;
            }
        }
        if !has_nested_query(q) {
            if let Some(rs) = self.try_join(q, &tables).await? {
                return Ok(rs);
            }
        }
        self.gather(q, &tables).await
    }

    /// The number of rows of a table (cached until the table changes).
    async fn table_rows(&mut self, t: &DistTable) -> Result<u64, SqlError> {
        let current = self.table_by_id(&t.id).unwrap_or_else(|| t.clone());
        if let Some((v, n)) = current.rows {
            if v == current.version {
                return Ok(n);
            }
        }
        let count = simple_query(Select {
            distinct: false,
            top: None,
            items: vec![SelectItem::Expr { e: Expr::Func(Box::new(FuncCall { name: "COUNT_BIG".into(), schema: None, parts: vec![], args: vec![], distinct: false, star: true, over: None, within: None })), alias: None }],
            into: None,
            from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }],
            where_: None,
            group: None,
            having: None,
        });
        let shards = if matches!(t.dist, Dist::Replicated) { vec![0] } else { self.all_shards() };
        let results = self.fan_query(&shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(count.clone()))])).await?;
        let n: u64 = results
            .iter()
            .map(|r| match r.rows.first().and_then(|x| x.first()) {
                Some(Value::BigInt(n)) => *n as u64,
                Some(Value::Int(n)) => *n as u64,
                _ => 0,
            })
            .sum();
        if let Some(e) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&t.id) {
            e.rows = Some((current.version, n));
        }
        Ok(n)
    }

    /// A join of several tables run shard by shard (co-located, broadcast or repartitioned); None when it cannot be.
    async fn try_join(&mut self, q: &Query, tables: &[DistTable]) -> Result<Option<ResultSet>, SqlError> {
        let map: HashMap<String, DistTable> = tables.iter().map(|t| (t.id.clone(), t.clone())).collect();
        let mut rows = HashMap::new();
        for t in tables {
            if !matches!(t.dist, Dist::Replicated) {
                let n = self.table_rows(t).await?;
                rows.insert(t.id.clone(), n);
            }
        }
        let limit = self.cluster.broadcast_limit.load(Ordering::Relaxed);
        let n = self.cluster.shard_count();
        let sid = self.id;
        let mut counter = self.temp_counter;
        let plan = joins::plan_join(q, &map, &rows, limit, n, &mut || {
            counter += 1;
            format!("#dpj_{}_{}", sid, counter)
        });
        self.temp_counter = counter;
        let Some(jp) = plan else { return Ok(None) };
        let Some(shape) = plan::plan_scatter_multi(&jp.query) else { return Ok(None) };
        let shards = self.all_shards();
        let mut made = vec![];
        let mut result = Ok(());
        for p in &jp.preps {
            made.push(p.temp.clone());
            if let Err(e) = self.prepare_copy(p).await {
                result = Err(e);
                break;
            }
        }
        let out = match result {
            Ok(()) => {
                self.plan.push(format!("join over shards: {}", jp.description));
                self.scatter(shape, &shards).await
            }
            Err(e) => Err(e),
        };
        // drop the copies whatever happened
        for t in made {
            let drop = Stmt::Drop { what: "TABLE".into(), names: vec![vec![t]], if_exists: true };
            let _ = self.fan_run(&shards, |_| Work::Stmts(vec![drop.clone()])).await;
        }
        out.map(Some)
    }

    /// Make the session temporary copy of a table on every shard: all its rows (broadcast) or the rows each shard owns by
    /// the hash of the join column (repartition).
    async fn prepare_copy(&mut self, p: &joins::PrepSpec) -> Result<(), SqlError> {
        let shards = self.all_shards();
        let n = shards.len();
        let scan = simple_query(Select {
            distinct: false,
            top: None,
            items: vec![SelectItem::Star(None)],
            into: None,
            from: vec![TableRef::Table { name: p.table.name.clone(), alias: Some(p.alias.clone()), hints: vec![] }],
            where_: p.filter.clone(),
            group: None,
            having: None,
        });
        let source = self.fan_query(&shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(scan.clone()))])).await?;
        // the temporary table has the columns of the table
        let create = Stmt::Query(Box::new(simple_query(Select {
            distinct: false,
            top: Some(Top { e: Expr::Lit(Value::Int(0)), percent: false, ties: false }),
            items: vec![SelectItem::Star(None)],
            into: Some(vec![p.temp.clone()]),
            from: vec![TableRef::Table { name: p.table.name.clone(), alias: None, hints: vec![] }],
            where_: None,
            group: None,
            having: None,
        })));
        self.fan_run(&shards, |_| Work::Stmts(vec![create.clone()])).await?;
        let mut per_shard: Vec<Vec<Vec<Value>>> = vec![vec![]; n];
        for rs in source {
            for row in rs.rows {
                match p.repartition {
                    None => {
                        for s in per_shard.iter_mut() {
                            s.push(row.clone());
                        }
                    }
                    Some((col, class)) => {
                        let h = key_hash(&row[col], class).unwrap_or(0);
                        per_shard[self.cluster.shard_of_hash(h)].push(row);
                    }
                }
            }
        }
        // (a broadcast sends the same rows to every shard, a thousand rows to a statement)
        let mut offset = 0;
        loop {
            let stmts: Vec<Option<Stmt>> = (0..n)
                .map(|i| {
                    let rows = &per_shard[i];
                    if offset >= rows.len() {
                        return None;
                    }
                    let chunk: Vec<Vec<Expr>> = rows[offset..(offset + 1000).min(rows.len())].iter().map(|r| r.iter().cloned().map(Expr::Lit).collect()).collect();
                    Some(Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(vec![p.temp.clone()]), cols: None, output: None, src: InsertSrc::Values(chunk), top: None })))
                })
                .collect();
            let active: Vec<usize> = (0..n).filter(|i| stmts[*i].is_some()).collect();
            if active.is_empty() {
                break;
            }
            self.fan_run(&active, |i| Work::Stmts(vec![stmts[i].clone().unwrap()])).await?;
            offset += 1000;
        }
        Ok(())
    }

    fn mirror_query(&mut self, q: &Query) -> Result<ResultSet, SqlError> {
        let res = self.mirror.execute_stmts(&[Stmt::Query(Box::new(q.clone()))]);
        merge::check(&res)?;
        first_rows(res)
    }

    async fn direct(&mut self, shard: usize, q: &Query) -> Result<ResultSet, SqlError> {
        self.ensure_begun(&[shard]).await?;
        self.cluster.shard(shard).query(self.id, Work::Stmts(vec![Stmt::Query(Box::new(q.clone()))])).await
    }

    async fn scatter(&mut self, shape: Shape, shards: &[usize]) -> Result<ResultSet, SqlError> {
        let (sq, kind) = match &shape {
            Shape::Row(p) => (p.shard_query.clone(), "rows"),
            Shape::Agg(p) => (p.shard_query.clone(), "two-phase aggregate"),
        };
        self.plan.push(format!("scatter-gather ({}) over shards {:?}", kind, shards));
        let results = self.fan_query(shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(sq.clone()))])).await?;
        // a column's type: from the first shard that has a value in it (a shard with no rows may only guess)
        let ncols = results[0].columns.len();
        let types = (0..ncols).map(|i| results.iter().find(|r| r.rows.iter().any(|row| !row[i].is_null())).unwrap_or(&results[0]).types[i].clone()).collect::<Vec<_>>();
        let columns = results[0].columns.clone();
        let rows: Vec<Vec<Value>> = results.into_iter().flat_map(|r| r.rows).collect();
        merge::run_final(&mut self.mirror, &columns, &types, rows, |cols, tys| match &shape {
            Shape::Row(p) => Some(p.final_query(merge::tmp_name(), cols)),
            Shape::Agg(p) => p.final_query(merge::tmp_name(), tys),
        })
    }

    /// Copy the tables to the mirror (unless it already has their current rows) and run the query there.
    async fn gather(&mut self, q: &Query, tables: &[DistTable]) -> Result<ResultSet, SqlError> {
        self.plan.push(format!("gather {} into the control node, run there", tables.iter().map(|t| t.id.clone()).collect::<Vec<_>>().join(", ")));
        let cl = self.cluster.clone();
        let _guard = cl.gather.lock().unwrap_or_else(|e| e.into_inner());
        for t in tables {
            self.refresh_mirror(t).await?;
        }
        let r = self.mirror_query(q);
        if self.txn.borrow().is_some() {
            // (the copies hold what this transaction sees, which others must not: they are not kept)
            for t in tables {
                if let Some(e) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&t.id) {
                    e.loaded = None;
                }
            }
        }
        r
    }

    async fn refresh_mirror(&mut self, t: &DistTable) -> Result<(), SqlError> {
        let current = self.table_by_id(&t.id).ok_or_else(|| SqlError::new(208, format!("Invalid object name '{}'.", t.name.join("."))))?;
        if current.loaded == Some(current.version) {
            return Ok(());
        }
        merge::check(&self.mirror.execute_stmts(&[Stmt::Truncate(Target::Table(t.name.clone()))]))?;
        let cols: Vec<String> = t.cols.iter().filter(|c| !c.computed).map(|c| c.name.clone()).collect();
        let scan = simple_query(Select {
            distinct: false,
            top: None,
            items: cols.iter().map(|c| SelectItem::Expr { e: col(c), alias: None }).collect(),
            into: None,
            from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }],
            where_: None,
            group: None,
            having: None,
        });
        let shards = if matches!(t.dist, Dist::Replicated) { vec![0] } else { self.all_shards() };
        let results = self.fan_query(&shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(scan.clone()))])).await?;
        for rs in results {
            for chunk in rs.rows.chunks(1000) {
                let rows: Vec<Vec<Expr>> = chunk.iter().map(|r| r.iter().cloned().map(Expr::Lit).collect()).collect();
                let ins = Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(t.name.clone()), cols: Some(cols.clone()), output: None, src: InsertSrc::Values(rows), top: None }));
                merge::check(&self.mirror.execute_stmts(&[ins]))?;
            }
        }
        if let Some(e) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&t.id) {
            e.loaded = Some(current.version);
        }
        Ok(())
    }
}

fn first_rows(res: BatchResult) -> Result<ResultSet, SqlError> {
    for ev in res.events {
        if let Event::Rows(rs) = ev {
            return Ok(rs);
        }
    }
    Err(SqlError::new(50000, "the statement returned no result set"))
}

fn plan_scatter_checked(q: &Query) -> Option<Shape> {
    plan::plan_scatter(q)
}

impl Drop for ClusterSession {
    fn drop(&mut self) {
        let cl = self.cluster.clone();
        let id = self.id;
        // (not from inside a runtime: the session is dropped by the thread that used it)
        if tokio::runtime::Handle::try_current().is_err() {
            cl.rt.block_on(join_all(cl.shard_list().iter().map(|s| s.close_session(id))));
        }
    }
}
