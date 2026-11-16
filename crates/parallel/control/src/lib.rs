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
pub mod autoscale;
pub mod client;
pub mod deadlock;
pub mod decorrelate;
pub mod fk;
pub mod flatten;
pub mod gather;
pub mod front;
pub mod joins;
pub mod merge;
pub mod mpp;
pub mod nested;
pub mod ops;
pub mod pushdown;
pub mod plan;
pub mod rebalance;
pub mod split;
pub mod replicas;
pub mod txn;

use astutil::*;
use client::ShardClient;
use fk::{without_fks, FkDef};
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
    /// the IDENTITY column the control node numbers (the shards' copies of the table have no identity of their own)
    pub identity: Option<Identity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    /// the position of the column
    pub col: usize,
    /// the next value to hand out
    pub next: i64,
    pub inc: i64,
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
    /// the longest list an `IN (SELECT ...)` is turned into (longer results become replicated temporary tables)
    in_list_limit: AtomicU64,
    in_join_min: AtomicU64,
    /// how long a statement of a transaction waits for a lock on a shard (ms); a deadlock between shards is broken by it
    lock_timeout_ms: AtomicU64,
    /// the views (by table id) as their `CREATE VIEW` statements: a query that reads one is rewritten around its definition
    /// (see `nested.rs`), since the shards hold no views
    views: Mutex<HashMap<String, Stmt>>,
    /// the foreign keys, which the shards do not enforce (see `fk.rs`)
    fks: Mutex<Vec<FkDef>>,
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
            in_list_limit: AtomicU64::new(20_000),
            in_join_min: AtomicU64::new(100),
            lock_timeout_ms: AtomicU64::new(5000),
            views: Mutex::new(HashMap::new()),
            fks: Mutex::new(vec![]),
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

    /// Reserve `count` identity values of a table: the first of them and the increment. Saved with the control state, so that a
    /// restart goes on from there.
    pub(crate) fn take_identity(&self, id: &str, count: usize) -> Option<(i64, i64)> {
        let out = {
            let mut t = self.tables.lock().unwrap_or_else(|e| e.into_inner());
            let ident = t.get_mut(id)?.identity.as_mut()?;
            let first = ident.next;
            ident.next += ident.inc * count as i64;
            (first, ident.inc)
        };
        self.save_state();
        Some(out)
    }

    /// An explicit value was inserted into an IDENTITY column: the numbers handed out from now on are past it.
    pub(crate) fn raise_identity(&self, id: &str, value: i64) {
        let changed = {
            let mut t = self.tables.lock().unwrap_or_else(|e| e.into_inner());
            match t.get_mut(id).and_then(|t| t.identity.as_mut()) {
                Some(i) if (i.inc > 0 && value >= i.next) || (i.inc < 0 && value <= i.next) => {
                    i.next = value + i.inc;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.save_state();
        }
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

    /// The shard that owns each hash slot.
    pub fn slot_map(&self) -> Vec<u32> {
        self.slots.read().unwrap_or_else(|e| e.into_inner()).clone()
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
    /// The longest list an `IN (SELECT ...)` is turned into; a longer result is made a replicated temporary table.
    pub fn set_in_list_limit(&self, rows: u64) {
        self.in_list_limit.store(rows, Ordering::Relaxed);
    }

    /// The shortest literal `IN` list that is joined as a replicated table instead (default 100); 0 or less than 2 is 2.
    pub fn set_in_join_min(&self, values: u64) {
        self.in_join_min.store(values.max(2), Ordering::Relaxed);
    }

    pub fn set_broadcast_limit(&self, rows: u64) {
        self.broadcast_limit.store(rows, Ordering::Relaxed);
    }

    pub fn session(self: &Arc<Self>) -> ClusterSession {
        ClusterSession { cluster: self.clone(), id: self.next_session.fetch_add(1, Ordering::Relaxed), mirror: self.mirror.connect_internal(), plan: vec![], temp_counter: 0, txn: std::cell::RefCell::new(None), stmt_shards: std::cell::Cell::new(0), commit_seq: 0, vars: HashMap::new(), local_tables: HashMap::new(), tvars: HashMap::new(), identity_insert: Default::default(), last_identity: None }
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
    /// the temporary tables of the statement being run (results of CTEs and derived tables, see `nested.rs`)
    local_tables: HashMap<String, DistTable>,
    /// the tables whose IDENTITY column may be given values (`SET IDENTITY_INSERT t ON`), by table id
    identity_insert: std::collections::HashSet<String>,
    /// the last identity value this session handed out (`SCOPE_IDENTITY()`, `@@IDENTITY`)
    last_identity: Option<i64>,
    /// the table variables of the session (by lower-case name) with their columns and rows: a query that reads one has its
    /// rows sent to the shards as a temporary table (see `nested.rs`). (The engine keeps variables for one batch only, so the
    /// session keeps these itself.)
    tvars: HashMap<String, ResultSet>,
}

/// A transaction of a control session: the shards that have begun it, and what it has changed.
struct TxnState {
    /// the savepoints, oldest first: the name and the shards that had begun the transaction when it was made
    savepoints: Vec<(String, std::collections::BTreeSet<usize>)>,
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

/// The variables a `SELECT @a = ..., @b = ...` assigns (with the operator of `+=` and the like), when every item is an
/// assignment.
fn assign_targets(q: &Query) -> Option<Vec<(String, Option<BinOp>)>> {
    let QueryBody::Select(s) = &q.body else { return None };
    if s.items.is_empty() || !s.items.iter().all(|i| matches!(i, SelectItem::Assign { .. })) {
        return None;
    }
    Some(s.items.iter().filter_map(|i| if let SelectItem::Assign { var, op, .. } = i { Some((var.clone(), *op)) } else { None }).collect())
}

fn exprs_have_subquery(u: &Update) -> bool {
    u.where_.iter().any(has_subquery) || u.sets.iter().any(|s| matches!(s, SetItem::Col { e, .. } if has_subquery(e)))
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

/// The one shard that holds every row a join of tables reads, when each sharded table's key is fixed by a `key = literal`
/// conjunct of the `WHERE` (a table's key column must be unambiguous: qualified by the table's name or alias, or not a column of
/// any other table in the `FROM`) and the literals all hash to the same shard. Only for plain `FROM a, b, ...` lists; the
/// replicated tables are on every shard.
fn pin_shard(q: &Query, tables: &HashMap<String, DistTable>, cluster: &Cluster) -> Option<usize> {
    let QueryBody::Select(sel) = &q.body else { return None };
    let w = sel.where_.as_ref()?;
    let mut refs: Vec<(String, DistTable)> = vec![];
    for t in &sel.from {
        let TableRef::Table { name, alias, .. } = t else { return None };
        let dt = tables.get(&table_id(name))?;
        refs.push((alias.clone().or_else(|| name.last().cloned())?.to_lowercase(), dt.clone()));
    }
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
    let mut cs = vec![];
    conjuncts(w, &mut cs);
    let mut pinned: Option<usize> = None;
    for (alias, dt) in &refs {
        let Dist::Sharded { key, class } = &dt.dist else { continue };
        let key_name = dt.cols[*key].name.to_lowercase();
        // the key column named only by this table
        let others = refs.iter().filter(|(a, _)| a != alias).any(|(_, o)| o.cols.iter().any(|c| c.name.eq_ignore_ascii_case(&key_name)));
        let names_key = |e: &Expr| match e {
            Expr::Col(p) if p.last().map(|s| s.to_lowercase()) == Some(key_name.clone()) => match p.len() {
                1 => !others,
                n => p[n - 2].to_lowercase() == *alias,
            },
            _ => false,
        };
        let mut shard: Option<usize> = None;
        for c in &cs {
            if let Expr::Binary(BinOp::Eq, a, b) = c {
                let lit = match (&**a, &**b) {
                    (x, Expr::Lit(v)) if names_key(x) => Some(v),
                    (Expr::Lit(v), x) if names_key(x) => Some(v),
                    _ => None,
                };
                if let Some(h) = lit.and_then(|v| key_hash(v, *class)) {
                    shard = Some(cluster.shard_of_hash(h));
                    break;
                }
            }
        }
        let s = shard?;
        if pinned.map(|p| p != s).unwrap_or(false) {
            return None;
        }
        pinned = Some(s);
    }
    pinned
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

    /// `SELECT @a = e1, @b = e2 FROM ...`: run the query for its values and assign the last row's to the variables (no row:
    /// they keep their values), as the engine does.
    async fn select_assign(&mut self, q: &Query, targets: Vec<(String, Option<BinOp>)>) -> Result<(), SqlError> {
        let mut plain = q.clone();
        if let QueryBody::Select(s) = &mut plain.body {
            s.items = s.items.iter().map(|i| if let SelectItem::Assign { e, .. } = i { SelectItem::Expr { e: e.clone(), alias: None } } else { i.clone() }).collect();
        }
        let rs = self.run_query(&plain).await?;
        let Some(row) = rs.rows.last() else { return Ok(()) };
        for ((var, op), v) in targets.into_iter().zip(row.iter()) {
            let key = var.to_lowercase();
            let new = match op {
                None => v.clone(),
                Some(op) => {
                    let old = self.vars.get(&key).cloned().unwrap_or(Value::Null);
                    self.eval_scalar(&Expr::Binary(op, Box::new(Expr::Lit(old)), Box::new(Expr::Lit(v.clone())))).await?
                }
            };
            self.vars.insert(key, new);
        }
        Ok(())
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
        if let Some(t) = self.local_tables.get(id) {
            return Some(t.clone());
        }
        self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
    }

    fn table(&self, name: &Name) -> Result<DistTable, SqlError> {
        if self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&table_id(name)) {
            return Err(unsupported("INSERT, UPDATE or DELETE through a view"));
        }
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
        let on_shards = shard_form(st);
        self.fan_run(&shards, |_| Work::Stmts(vec![on_shards.clone()])).await?;
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
                    None => *t = Some(TxnState { shards: Default::default(), depth: 1, doomed: false, wrote: Default::default(), savepoints: vec![] }),
                }
                return Ok(());
            }
            Stmt::Commit(_) => return self.commit_txn(events).await,
            Stmt::Rollback(Some(name)) => return self.rollback_to_savepoint(name).await,
            Stmt::Rollback(None) => return self.rollback_txn().await,
            Stmt::SaveTran(name) => return self.save_savepoint(name).await,
            _ => {}
        }
        if let Some(t) = self.txn.borrow().as_ref() {
            if t.doomed {
                return Err(SqlError::new(3930, "The current transaction cannot be committed and cannot support operations that write to the log file. Roll back the transaction."));
            }
            if matches!(st, Stmt::CreateTable { .. } | Stmt::AlterTable { .. } | Stmt::CreateView { .. } | Stmt::Drop { .. } | Stmt::CreateIndex { .. } | Stmt::DropIndex { .. } | Stmt::Truncate(_)) {
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
            Stmt::AlterTable { name, action } => self.alter_table(st, name, action).await,
            Stmt::CreateView { name, alter, or_alter, .. } => {
                let id = table_id(name);
                let known = self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id);
                if self.table_by_id(&id).is_some() || (known && !*alter && !*or_alter) {
                    return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", name.last().cloned().unwrap_or_default())));
                }
                if *alter && !known {
                    return Err(SqlError::new(208, format!("Invalid object name '{}'.", name.join("."))));
                }
                // the control node's own engine checks the definition (it holds the schema of every table) ...
                merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
                // ... and the control node keeps it: the shards hold no views
                self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).insert(id, st.clone());
                self.cluster.log_schema(st);
                Ok(())
            }
            Stmt::Drop { what, names, if_exists } if what.eq_ignore_ascii_case("VIEW") => {
                for n in names {
                    let id = table_id(n);
                    if !self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) && !*if_exists {
                        return Err(SqlError::new(3701, format!("Cannot drop the view '{}', because it does not exist or you do not have permission.", n.join("."))));
                    }
                }
                merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
                let mut v = self.cluster.views.lock().unwrap_or_else(|e| e.into_inner());
                for n in names {
                    v.remove(&table_id(n));
                }
                drop(v);
                self.cluster.log_schema(st);
                Ok(())
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("TABLE") => {
                for n in names {
                    let id = table_id(n);
                    // (a table may be dropped together with the tables that refer to it)
                    if self.cluster.fks_of_parent(&id).iter().any(|f| f.child != id && !names.iter().any(|m| table_id(m) == f.child)) {
                        return Err(SqlError::new(3726, format!("Could not drop object '{}' because it is referenced by a FOREIGN KEY constraint.", n.join("."))));
                    }
                }
                self.broadcast(st).await?;
                let mut t = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
                for n in names {
                    t.remove(&table_id(n));
                }
                drop(t);
                {
                    let ids: Vec<String> = names.iter().map(|n| table_id(n)).collect();
                    self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).retain(|f| !ids.contains(&f.child) && !ids.contains(&f.parent));
                }
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
            Stmt::SetOpts(opts) => {
                // IDENTITY_INSERT is about the numbers the control node hands out: the shards' tables have no identity
                let mut rest: Vec<SetOpt> = vec![];
                for o in opts {
                    match o {
                        SetOpt::IdentityInsert(name, on) => {
                            let id = table_id(name);
                            if self.table_by_id(&id).is_none() {
                                rest.push(o.clone());
                            } else if *on {
                                self.identity_insert.insert(id);
                            } else {
                                self.identity_insert.remove(&id);
                            }
                        }
                        other => rest.push(other.clone()),
                    }
                }
                if rest.is_empty() {
                    Ok(())
                } else {
                    self.broadcast(&Stmt::SetOpts(rest)).await
                }
            }
            Stmt::Truncate(Target::Table(name)) => {
                let t = self.table(name)?;
                if self.cluster.fks_of_parent(&t.id).iter().any(|f| f.child != t.id) {
                    return Err(SqlError::new(4712, format!("Cannot truncate table '{}' because it is being referenced by a FOREIGN KEY constraint.", name.join("."))));
                }
                self.broadcast(st).await?;
                self.bump(&t.id);
                Ok(())
            }
            Stmt::Insert(ins) => self.insert(ins, events).await,
            Stmt::Update(u) => self.update(u, events).await,
            Stmt::Delete(d) => self.delete(d, events).await,
            Stmt::Query(q) => {
                if let Some(assigns) = assign_targets(q) {
                    return self.select_assign(q, assigns).await;
                }
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
        let mut identity: Option<(usize, i64, i64)> = None;
        for d in defs {
            match d {
                TableDef::Column(cd) => {
                    if let Some((seed, inc)) = &cd.identity {
                        let seed = self.eval_scalar(seed).await?;
                        let inc = self.eval_scalar(inc).await?;
                        identity = Some((cols.len(), seed.as_i64().unwrap_or(1), inc.as_i64().unwrap_or(1)));
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
                TableDef::Period { .. } => return Err(unsupported("system-versioned tables")),
                _ => {}
            }
        }
        // foreign keys are the control node's to enforce (the shards and its own engine are not given them)
        let fks = self.declared_fks(name, defs, &pk)?;
        let stripped = without_fks(st);
        let st = &stripped;
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
        self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), DistTable { id, name: name.clone(), cols, dist: Dist::Sharded { key, class }, version: 0, loaded: None, rows: None, identity: identity.map(|(col, next, inc)| Identity { col, next, inc }) });
        self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).extend(fks);
        self.cluster.log_schema(st);
        Ok(())
    }

    /// `ALTER TABLE`: run on the control node's engine and on every shard, and keep the columns the control node knows. What
    /// would change how rows are placed or numbered (the shard key, an IDENTITY column, a primary key) is refused.
    async fn alter_table(&mut self, st: &Stmt, name: &Name, action: &AlterAction) -> Result<(), SqlError> {
        let t = self.table(name)?;
        let key_col = match &t.dist {
            Dist::Sharded { key, .. } => Some(t.cols[*key].name.clone()),
            Dist::Replicated => None,
        };
        let ident_col = t.identity.as_ref().map(|i| t.cols[i.col].name.clone());
        let is = |c: &str, of: &Option<String>| of.as_deref().map(|k| k.eq_ignore_ascii_case(c)).unwrap_or(false);
        match action {
            AlterAction::AddDefs(defs) => {
                for d in defs {
                    match d {
                        TableDef::Column(cd) => {
                            if cd.identity.is_some() {
                                return Err(unsupported("adding an IDENTITY column"));
                            }
                            if cd.pk.is_some() {
                                return Err(unsupported("adding a primary key"));
                            }
                            if cd.unique.is_some() {
                                return Err(unsupported("adding a UNIQUE column"));
                            }
                        }
                        TableDef::PrimaryKey { .. } => return Err(unsupported("adding a primary key")),
                        TableDef::Unique { cols, .. } => {
                            if !cols.iter().any(|(c, _)| is(c, &key_col)) && key_col.is_some() {
                                return Err(unsupported("a UNIQUE constraint that does not contain the shard key"));
                            }
                        }
                        TableDef::Period { .. } => return Err(unsupported("system-versioned tables")),
                        _ => {}
                    }
                }
            }
            AlterAction::DropColumn(cols, _) => {
                if cols.iter().any(|c| is(c, &key_col) || is(c, &ident_col)) {
                    return Err(unsupported("dropping the shard key or an IDENTITY column"));
                }
            }
            AlterAction::AlterColumn { col, .. } => {
                if is(col, &key_col) || is(col, &ident_col) {
                    return Err(unsupported("changing the shard key or an IDENTITY column"));
                }
            }
            AlterAction::SetVersioning(_) => return Err(unsupported("system-versioned tables")),
            AlterAction::DropConstraint(..) | AlterAction::AlterMask { .. } | AlterAction::CheckConstraint { .. } => {}
        }
        // foreign keys are the control node's: a new one is checked against the rows that exist, and none goes to the shards
        let mut st_owned = st.clone();
        let mut new_fks: Vec<FkDef> = vec![];
        let mut dropped_fks: Vec<String> = vec![];
        if let Stmt::AlterTable { action, .. } = &mut st_owned {
            match action {
                AlterAction::AddDefs(defs) => {
                    new_fks = self.declared_fks(name, defs, &[])?;
                    defs.retain(|d| !matches!(d, TableDef::ForeignKey(_)));
                    for d in defs.iter_mut() {
                        if let TableDef::Column(cd) = d {
                            cd.fk = None;
                        }
                    }
                    if defs.is_empty() {
                        for fk in &new_fks {
                            self.fk_validate_existing(fk).await?;
                        }
                        self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).extend(new_fks);
                        self.cluster.save_state();
                        return Ok(());
                    }
                }
                AlterAction::DropConstraint(names, _) => {
                    let known: Vec<String> = self.cluster.fks_of_child(&t.id).into_iter().map(|f| f.name).collect();
                    dropped_fks = names.iter().filter(|n| known.iter().any(|k| k.eq_ignore_ascii_case(n))).cloned().collect();
                    names.retain(|n| !dropped_fks.iter().any(|d| d.eq_ignore_ascii_case(n)));
                    if names.is_empty() {
                        self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).retain(|f| !(f.child == t.id && dropped_fks.iter().any(|d| d.eq_ignore_ascii_case(&f.name))));
                        self.cluster.save_state();
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        for fk in &new_fks {
            self.fk_validate_existing(fk).await?;
        }
        let st = &st_owned;
        self.broadcast(st).await?;
        self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).extend(new_fks);
        self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).retain(|f| !(f.child == t.id && dropped_fks.iter().any(|d| d.eq_ignore_ascii_case(&f.name))));
        // the columns as the control node's engine now has them
        let shown = format!("[{}].[{}]", if name.len() >= 2 { name[name.len() - 2].clone() } else { "dbo".to_string() }, name.last().cloned().unwrap_or_default());
        if let Some(info) = self.mirror.table_info(&shown) {
            let cols: Vec<ColInfo> = info.columns.iter().map(|c| ColInfo { name: c.name.clone(), computed: c.computed, type_name: c.ty.split('(').next().unwrap_or("").trim().to_lowercase() }).collect();
            let mut tables = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = tables.get_mut(&t.id) {
                if let (Dist::Sharded { key, .. }, Some(k)) = (&mut e.dist, &key_col) {
                    if let Some(p) = cols.iter().position(|c| c.name.eq_ignore_ascii_case(k)) {
                        *key = p;
                    }
                }
                if let (Some(i), Some(ic)) = (e.identity.as_mut(), &ident_col) {
                    if let Some(p) = cols.iter().position(|c| c.name.eq_ignore_ascii_case(ic)) {
                        i.col = p;
                    }
                }
                e.cols = cols;
                e.version += 1;
                e.loaded = None;
                e.rows = None;
            }
        }
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
        if has_subquery(e) {
            // a subquery may read distributed tables: the cluster answers it
            let rs = self.run_query(&q).await?;
            return rs.rows.first().and_then(|r| r.first()).cloned().ok_or_else(|| SqlError::new(50000, "an expression returned no value"));
        }
        let res = self.mirror.execute_stmts(&[Stmt::Query(Box::new(q))]);
        merge::check(&res)?;
        res.resultsets().first().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().ok_or_else(|| SqlError::new(50000, "an expression returned no value"))
    }

    async fn insert(&mut self, ins: &Insert, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let name = match &ins.target {
            Target::Table(name) => name,
            Target::Var(v) if self.tvars.contains_key(&v.to_lowercase()) => {
                let key = v.to_lowercase();
                let values: Vec<Vec<Value>> = match &ins.src {
                    InsertSrc::Values(rows) => {
                        let mut out = vec![];
                        for r in rows {
                            let mut vals = vec![];
                            for e in r {
                                let b = self.bind_expr(e)?;
                                vals.push(self.eval_scalar(&b).await?);
                            }
                            out.push(vals);
                        }
                        out
                    }
                    // (what it is filled from may be read from the shards)
                    InsertSrc::Query(q) => self.run_query(q).await?.rows,
                    _ => return Err(unsupported("this INSERT into a table variable")),
                };
                let tv = self.tvars.get_mut(&key).expect("a table variable");
                let ncols = tv.columns.len();
                // the columns the values are for: those named, or all in order
                let order: Vec<usize> = match &ins.cols {
                    Some(names) => names.iter().map(|n| tv.columns.iter().position(|c| c.eq_ignore_ascii_case(n)).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", n)))).collect::<Result<_, _>>()?,
                    None => (0..ncols).collect(),
                };
                for vals in values {
                    if vals.len() != order.len() {
                        return Err(SqlError::new(213, "Column name or number of supplied values does not match table definition."));
                    }
                    let mut row = vec![Value::Null; ncols];
                    for (v, &i) in vals.into_iter().zip(&order) {
                        row[i] = v;
                    }
                    tv.rows.push(row);
                }
                return Ok(());
            }
            Target::Var(_) => return Err(unsupported("INSERT into a table variable that is not declared in this batch")),
        };
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
        let ident_col = t.identity.as_ref().map(|i| t.cols[i.col].name.clone());
        let given: Option<Vec<String>> = ins.cols.clone();
        // without a column list the values are for the columns that are not computed (and not the identity column, unless
        // IDENTITY_INSERT is on)
        let mut names: Vec<String> = given.clone().unwrap_or_else(|| {
            t.cols.iter().filter(|c| !c.computed && !(ident_col.as_deref() == Some(c.name.as_str()) && !self.identity_insert.contains(&t.id))).map(|c| c.name.clone()).collect()
        });
        if let Some(bad) = rows.iter().find(|r| r.len() != names.len()) {
            return Err(SqlError::new(213, format!("Column name or number of supplied values does not match table definition ({} values for {} columns).", bad.len(), names.len())));
        }
        let mut rows = rows;
        if let Some(ic) = &ident_col {
            let explicit = names.iter().any(|c| c.eq_ignore_ascii_case(ic));
            if explicit && !self.identity_insert.contains(&t.id) {
                return Err(SqlError::new(544, format!("Cannot insert explicit value for identity column in table '{}' when IDENTITY_INSERT is set to OFF.", t.name.last().cloned().unwrap_or_default())));
            }
            if !explicit {
                // the control node numbers the rows: the same numbers whichever shard a row goes to
                if let Some((first, inc)) = self.cluster.take_identity(&t.id, rows.len()) {
                    let is_big = t.cols.iter().find(|c| c.name.eq_ignore_ascii_case(ic)).map(|c| c.type_name == "bigint").unwrap_or(false);
                    for (k, r) in rows.iter_mut().enumerate() {
                        let v = first + inc * k as i64;
                        r.push(Expr::Lit(if is_big { Value::BigInt(v) } else { Value::Int(v as i32) }));
                    }
                    names.push(ic.clone());
                    self.last_identity = Some(first + inc * (rows.len().saturating_sub(1)) as i64);
                }
            } else if let Some(pos) = names.iter().position(|c| c.eq_ignore_ascii_case(ic)) {
                // an explicit value moves the counter if it is past it (as the engine does)
                let mut top: Option<i64> = None;
                for r in &rows {
                    if let Expr::Lit(v) = &r[pos] {
                        if let Some(x) = v.as_i64() {
                            top = Some(top.map_or(x, |t| t.max(x)));
                        }
                    }
                }
                if let Some(top) = top {
                    self.cluster.raise_identity(&t.id, top);
                }
            }
        }
        if !self.cluster.fks_of_child(&t.id).is_empty() {
            self.fk_check_insert(&t, &names, &rows).await?;
        }
        let cols_for_shards: Option<Vec<String>> = if ident_col.is_some() || given.is_some() { Some(names.clone()) } else { None };
        let make = |rows: Vec<Vec<Expr>>| Stmt::Insert(Box::new(Insert { ctes: vec![], target: ins.target.clone(), cols: cols_for_shards.clone(), output: None, src: InsertSrc::Values(rows), top: None }));
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
        let mut moves_key = false;
        for s in &u.sets {
            match s {
                SetItem::Col { parts, e, .. } => {
                    let col = parts.last().cloned().unwrap_or_default();
                    if let Some(i) = &t.identity {
                        if t.cols[i.col].name.eq_ignore_ascii_case(&col) {
                            return Err(SqlError::new(8102, format!("Cannot update identity column '{}'.", col)));
                        }
                    }
                    if let Dist::Sharded { key, .. } = &t.dist {
                        if col.eq_ignore_ascii_case(&t.cols[*key].name) {
                            moves_key = true;
                        }
                    }
                    exprs.push(e);
                }
                SetItem::Var { .. } => return Err(unsupported("UPDATE that assigns variables")),
            }
        }
        if exprs.iter().any(|e| has_window_or_session(e)) {
            return Err(unsupported("variables or window functions in UPDATE"));
        }
        if exprs.iter().any(|e| has_subquery(e)) {
            // subqueries that do not depend on the row are evaluated first
            return match self.resolve_stmt(&Stmt::Update(Box::new(u.clone()))).await? {
                Some(Stmt::Update(u2)) if !exprs_have_subquery(&u2) => Box::pin(self.update(&u2, events)).await,
                _ => Err(unsupported("correlated subqueries in UPDATE")),
            };
        }
        self.fk_check_update(&t, u).await?;
        if moves_key {
            return self.update_moving_key(&t, u, events).await;
        }
        let shards = self.dml_shards(&t, u.where_.as_ref());
        self.run_dml(&t, Stmt::Update(Box::new(u.clone())), shards, events).await
    }

    /// An UPDATE that changes the shard key moves rows between shards: the new rows are read (the engine evaluates the new
    /// values), the old ones deleted and the new ones inserted, in one transaction.
    async fn update_moving_key(&mut self, t: &DistTable, u: &Update, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let own_txn = self.txn.borrow().is_none();
        if own_txn {
            *self.txn.borrow_mut() = Some(TxnState { shards: Default::default(), depth: 1, doomed: false, wrote: Default::default(), savepoints: vec![] });
        }
        let out = self.move_rows(t, u, events).await;
        if own_txn {
            match out {
                Ok(()) => self.commit_txn(events).await,
                Err(e) => {
                    let _ = self.rollback_txn().await;
                    Err(e)
                }
            }
        } else {
            out
        }
    }

    async fn move_rows(&mut self, t: &DistTable, u: &Update, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let cols: Vec<&ColInfo> = t.cols.iter().filter(|c| !c.computed).collect();
        // the new row: each column as it is, or the value it is set to
        let items: Vec<SelectItem> = cols
            .iter()
            .map(|c| {
                let set = u.sets.iter().find_map(|s| match s {
                    SetItem::Col { parts, e, .. } if parts.last().map(|p| p.eq_ignore_ascii_case(&c.name)).unwrap_or(false) => Some(e.clone()),
                    _ => None,
                });
                SelectItem::Expr { e: set.unwrap_or_else(|| col(&c.name)), alias: Some(c.name.clone()) }
            })
            .collect();
        let alias = match &u.target {
            Target::Table(n) => n.last().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        let read = simple_query(Select { distinct: false, top: None, items, into: None, from: vec![TableRef::Table { name: t.name.clone(), alias: Some(alias), hints: vec![] }], where_: u.where_.clone(), group: None, having: None });
        let rs = self.run_query(&read).await?;
        let del = Delete { ctes: vec![], target: u.target.clone(), from: vec![], where_: u.where_.clone(), output: None, top: None, current_of: None };
        let mut none = vec![];
        let shards = self.dml_shards(t, u.where_.as_ref());
        self.run_dml(t, Stmt::Delete(Box::new(del)), shards, &mut none).await?;
        let moved = rs.rows.len() as i64;
        if moved > 0 {
            let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
            let rows: Vec<Vec<Expr>> = rs.rows.into_iter().map(|r| r.into_iter().map(Expr::Lit).collect()).collect();
            let ins = Insert { ctes: vec![], target: u.target.clone(), cols: Some(names), output: None, src: InsertSrc::Values(rows), top: None };
            // (the identity column, if any, is given: the rows keep their numbers)
            let had = self.identity_insert.contains(&t.id);
            self.identity_insert.insert(t.id.clone());
            let r = self.insert(&ins, &mut none).await;
            if !had {
                self.identity_insert.remove(&t.id);
            }
            r?;
        }
        self.plan.push("update of the shard key: rows read, deleted and inserted again".into());
        events.push(Event::RowCount(moved));
        Ok(())
    }

    async fn delete(&mut self, d: &Delete, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let Target::Table(name) = &d.target else { return Err(unsupported("DELETE from a table variable")) };
        let t = self.table(name)?;
        if !d.from.is_empty() || d.output.is_some() || d.top.is_some() || d.current_of.is_some() || !d.ctes.is_empty() {
            return Err(unsupported("DELETE with FROM, OUTPUT, TOP, CURRENT OF or CTEs"));
        }
        if let Some(w) = &d.where_ {
            if has_window_or_session(w) {
                return Err(unsupported("variables in DELETE"));
            }
            if has_subquery(w) {
                return match self.resolve_stmt(&Stmt::Delete(Box::new(d.clone()))).await? {
                    Some(Stmt::Delete(d2)) if !d2.where_.as_ref().map(has_subquery).unwrap_or(false) => Box::pin(self.delete(&d2, events)).await,
                    _ => Err(unsupported("correlated subqueries in DELETE")),
                };
            }
        }
        self.fk_before_delete(&t, d.where_.as_ref(), events).await?;
        let shards = self.dml_shards(&t, d.where_.as_ref());
        self.run_dml(&t, Stmt::Delete(Box::new(d.clone())), shards, events).await
    }

    // ------------------------------------------------------------------ queries

    async fn run_query_flat(&mut self, q: &Query) -> Result<ResultSet, SqlError> {
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
        if let Some(rs) = self.run_setop(q).await? {
            return Ok(rs);
        }
        // subqueries that the shards evaluate themselves (over replicated tables, or co-located with the outer table): the whole
        // query runs shard by shard with them inside
        if has_nested_query(q) && tables.iter().any(|t| matches!(t.dist, Dist::Sharded { .. })) {
            if let Some(rs) = self.try_pushdown(q, false).await? {
                return Ok(rs);
            }
        }
        // (one table read once: a table joined with itself is two row sources, and a condition on the key of one says nothing
        // about the other)
        if tables.len() == 1 && !has_nested_query(q) && matches!(&q.body, QueryBody::Select(s) if matches!(s.from.as_slice(), [TableRef::Table { .. }])) {
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
            let map: HashMap<String, DistTable> = tables.iter().map(|t| (t.id.clone(), t.clone())).collect();
            if let Some(shard) = pin_shard(q, &map, &self.cluster) {
                self.plan.push(format!("route: every table's key fixes the shard ({})", shard));
                return self.direct(shard, q).await;
            }
            if let Some(rs) = self.try_join(q, &tables).await? {
                return Ok(rs);
            }
            if let Some(rs) = self.try_mpp(q, &tables).await? {
                return Ok(rs);
            }
        }
        if let Some(rs) = self.run_split(q).await? {
            return Ok(rs);
        }
        self.gather(q, &tables).await
    }

    /// Run a query whose `WHERE` subqueries the shards can evaluate (see `pushdown.rs`) as a scatter-gather with them inside.
    /// `need_colocated`: only when some subquery reads a sharded table (the case that would otherwise be gathered).
    pub(crate) async fn try_pushdown(&mut self, q: &Query, need_colocated: bool) -> Result<Option<ResultSet>, SqlError> {
        let ids = referenced_tables(q);
        let map: HashMap<String, DistTable> = ids.iter().filter_map(|i| self.table_by_id(i).map(|t| (i.clone(), t))).collect();
        let lookup = |id: &str| map.get(id).cloned();
        if !pushdown::pushdown_ok(q, &lookup, need_colocated) {
            return Ok(None);
        }
        let Some(shape) = plan::plan_scatter_sub(q) else { return Ok(None) };
        // (one table: the shards its WHERE can reach; several, co-located: all of them)
        let shards = match &q.body {
            QueryBody::Select(s) => match s.from.as_slice() {
                [TableRef::Table { name, .. }] => match map.get(&table_id(name)) {
                    Some(outer) => prune_shards(query_where(q), outer, &self.cluster),
                    None => return Ok(None),
                },
                _ => self.all_shards(),
            },
            _ => return Ok(None),
        };
        self.plan.push(format!("pushdown: the subqueries run on the shards {:?} with the rest of the query", shards));
        self.scatter(shape, &shards).await.map(Some)
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

    /// A join that needs the shuffle between its joins (`mpp.rs`): the stages move rows from shard to shard into session
    /// temporary tables, then the rewritten query runs shard by shard. None when the query is not of that kind.
    async fn try_mpp(&mut self, q: &Query, tables: &[DistTable]) -> Result<Option<ResultSet>, SqlError> {
        if tables.len() < 2 {
            return Ok(None);
        }
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
        let plan = mpp::plan_mpp(q, &map, &rows, limit, n, &mut || {
            counter += 1;
            format!("#dpm_{}_{}", sid, counter)
        });
        self.temp_counter = counter;
        let Some(plan) = plan else { return Ok(None) };
        let shards = self.all_shards();
        let slots = self.cluster.slot_map();
        let peers: Vec<String> = self.cluster.shard_list().iter().map(|s| s.addr.clone()).collect();
        let mut made: Vec<String> = vec![];
        let out: Result<ResultSet, SqlError> = async {
            for stage in &plan.stages {
                // the temporary table has the columns of the scan
                let mut create = stage.scan.clone();
                create.top = Some(Top { e: Expr::Lit(Value::Int(0)), percent: false, ties: false });
                create.into = Some(vec![stage.temp.clone()]);
                made.push(stage.temp.clone());
                let create = Stmt::Query(Box::new(simple_query(create)));
                self.fan_run(&shards, |_| Work::Stmts(vec![create.clone()])).await?;
                // every shard runs the scan and sends each row to the shard that needs it, straight there
                let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
                let futs: Vec<_> = clients
                    .iter()
                    .zip(&shards)
                    .map(|(c, &me)| c.shuffle(tsql_shared_ces::wire::ShuffleReq { session: self.id, scan: Stmt::Query(Box::new(simple_query(stage.scan.clone()))), key: stage.hash, table: stage.temp.clone(), slots: slots.clone(), peers: peers.clone(), me }))
                    .collect();
                for r in join_all(futs).await {
                    r?;
                }
            }
            let shape = plan::plan_scatter_multi(&plan.query).ok_or_else(|| SqlError::new(50000, "the query could not be rewritten for the cluster"))?;
            self.plan.push(format!("mpp: {} ({} stages)", plan.description, plan.stages.len()));
            self.scatter(shape, &shards).await
        }
        .await;
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
        // every shard scans its rows and sends each to the shard that needs it, straight there: the rows do not pass through
        // the control node (the transaction has begun on every shard: the create above did that)
        let slots = self.cluster.slot_map();
        let peers: Vec<String> = self.cluster.shard_list().iter().map(|s| s.addr.clone()).collect();
        let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
        let futs: Vec<_> = clients
            .iter()
            .zip(&shards)
            .map(|(c, &me)| c.shuffle(tsql_shared_ces::wire::ShuffleReq { session: self.id, scan: Stmt::Query(Box::new(scan.clone())), key: p.repartition, table: p.temp.clone(), slots: slots.clone(), peers: peers.clone(), me }))
            .collect();
        for r in join_all(futs).await {
            r?;
        }
        let _ = n;
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

    pub(crate) async fn refresh_mirror(&mut self, t: &DistTable) -> Result<(), SqlError> {
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
            let mut it = rs.rows.into_iter();
            loop {
                let chunk: Vec<Vec<Value>> = it.by_ref().take(20_000).collect();
                if chunk.is_empty() {
                    break;
                }
                self.mirror.load_rows(&t.name, Some(&cols), chunk)?;
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
