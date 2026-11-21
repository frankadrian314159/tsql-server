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
//! UNIQUE constraints that do not contain the shard key, views, functions, triggers, ALTER TABLE, and DML with joins,
//! subqueries, OUTPUT or TOP. A statement that fails on some shards after succeeding on others (a multi-shard
//! `UPDATE`) leaves the shards that succeeded changed: atomic cross-shard transactions need the two-phase commit of a later
//! phase.
pub use tsql_server::rewrite::astutil;
pub mod advisor;
pub mod autoscale;
pub mod client;
pub mod cursors;
pub mod dbname;
pub mod ddltrig;
pub mod dmlfuncs;
pub mod execproc;
pub mod deadlock;
pub use tsql_server::rewrite::decorrelate;
pub mod fk;
pub mod funcs;
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
pub mod procs;
pub mod rebalance;
pub mod renames;
pub mod split;
pub mod replicas;
pub mod restore;
pub mod rules;
pub mod triggers;
pub mod uniq;
pub mod updfrom;
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
    /// a column the control node gives and the user does not see (the `$graph_id` of a graph table): not in a `*`, not in the values of an
    /// `INSERT` that names no columns, but part of the row when rows are copied
    #[serde(default)]
    pub hidden: bool,
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
    /// global secondary indexes (`DISTRIBUTE TABLE t INDEX BY col`): see `GlobalIndex`
    #[serde(default)]
    pub indexes: Vec<GlobalIndex>,
    /// a node or edge table: the control node numbers its rows (`$graph_id`), as it numbers an IDENTITY column
    #[serde(default)]
    pub graph: Option<GraphMeta>,
    /// the `CONNECTION` constraints of an edge table (the shards check what an edge may connect; what becomes of the edges at a deleted node is
    /// the control node's to do, as the foreign keys are)
    #[serde(default)]
    pub edge_cons: Vec<EdgeCons>,
    /// unique keys that do not contain the shard key: the control node checks them across the shards (see uniq.rs)
    #[serde(default)]
    pub weak_uniques: Vec<uniq::WeakUnique>,
}

impl DistTable {
    /// What a read of the rows of the table that are to be copied selects: `*`, except for a graph table, whose `*` leaves out the hidden
    /// `$graph_id`: every column, by name, in the order of `cols` (so that a position in `cols` is a position in the row).
    pub(crate) fn copy_items(&self) -> Vec<SelectItem> {
        if self.graph.is_some() {
            self.cols.iter().map(|c| SelectItem::Expr { e: Expr::Col(vec![c.name.clone()]), alias: None }).collect()
        } else {
            vec![SelectItem::Star(None)]
        }
    }

    /// The columns of a copied row that an `INSERT` can give, with their positions: `None` when a row is inserted whole, as it is (every
    /// table but a graph table, whose rows have generated columns an `INSERT` cannot be given).
    pub(crate) fn insertable(&self) -> Option<Vec<(usize, String)>> {
        self.graph.as_ref().map(|_| self.cols.iter().enumerate().filter(|(_, c)| !c.computed).map(|(i, c)| (i, c.name.clone())).collect())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphMeta {
    pub edge: bool,
    /// the next `$graph_id` to hand out
    pub next: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeCons {
    pub name: String,
    /// the ids of the node tables an edge may go from and to
    pub pairs: Vec<(String, String)>,
    pub cascade: bool,
}

/// A global secondary index of a sharded table: a table of its own, `(gv, gk)`, sharded by `gv` (the indexed value), whose rows say
/// "a row with this value has this shard key". A lookup by the indexed column reads it (one shard) and asks only the shards that own
/// the keys it lists. It only ever grows: a row deleted or moved leaves its entry behind, which costs a visit to a shard that has no
/// such row and never gives a wrong answer; every row inserted gets its entry, in the same transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlobalIndex {
    /// the position of the indexed column in the table
    pub col: usize,
    /// the index table's name
    pub table: Name,
    /// how the indexed value is hashed (the index table's own shard key class)
    pub class: KeyClass,
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
    /// per table, the lock of its unique keys (see uniq.rs)
    uniq_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    mirror: Server,
    tables: Mutex<HashMap<String, DistTable>>,
    /// held while the mirror's copies of tables are loaded and queried
    gather: Mutex<()>,
    /// what the queries had to move between shards or gather (see `advisor.rs`)
    advisor: advisor::Log,
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
    /// the procedures that run on one shard (by `dbo.name`): see `procs.rs`
    proc_routes: Mutex<HashMap<String, procs::ProcRoute>>,
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
    /// the databases the cluster has besides `master` (`CREATE DATABASE`), in the case they were made in; see `dbname.rs`
    dbs: Mutex<Vec<String>>,
    /// databases that were dropped (lower case): a session still in one goes back to `master` (see dbname.rs)
    dropped_dbs: Mutex<std::collections::HashSet<String>>,
    /// the database the last statement of the schema log was made in (the log says `USE` when it changes)
    logged_db: Mutex<String>,
    /// the stored functions (by id) as their `CREATE FUNCTION` statements: see `funcs.rs`
    funcs: Mutex<HashMap<String, Stmt>>,
    /// the functions that are on the shards (the ones that read no table)
    funcs_on_shards: Mutex<std::collections::BTreeSet<String>>,
    /// the statement (as JSON) each of them was made from there, to see that a changed one is made again
    funcs_placed: Mutex<HashMap<String, String>>,
    /// the triggers of the tables, in the order they were made: kept and fired by the control node (see `triggers.rs`)
    triggers: Mutex<Vec<(String, Stmt)>>,
    /// the ids of the triggers that are disabled
    triggers_off: Mutex<std::collections::HashSet<String>>,
    /// the DDL triggers of the databases (see ddltrig.rs)
    ddl_triggers: Mutex<Vec<ddltrig::DdlTrig>>,
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
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 8)).enable_all().build().map_err(|e| SqlError::new(50000, e.to_string()))?;
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
            uniq_locks: Mutex::new(HashMap::new()),
            mirror: Server::new(),
            tables: Mutex::new(HashMap::new()),
            gather: Mutex::new(()),
            advisor: Default::default(),
            next_session: AtomicU64::new(fresh_session_base()),
            broadcast_limit: AtomicU64::new(100_000),
            in_list_limit: AtomicU64::new(20_000),
            in_join_min: AtomicU64::new(100),
            lock_timeout_ms: AtomicU64::new(5000),
            views: Mutex::new(HashMap::new()),
            proc_routes: Mutex::new(HashMap::new()),
            fks: Mutex::new(vec![]),
            pending,
            preparing,
            next_gid: AtomicU64::new(fresh_session_base()),
            fault: Mutex::new(None),
            deadlock_check_ms: AtomicU64::new(200),
            victims: Mutex::new(Default::default()),
            deadlocks: AtomicU64::new(0),
            dbs: Default::default(),
            dropped_dbs: Default::default(),
            logged_db: Mutex::new("master".into()),
            funcs: Default::default(),
            funcs_on_shards: Default::default(),
            funcs_placed: Default::default(),
            triggers: Default::default(),
            triggers_off: Default::default(),
            ddl_triggers: Default::default(),
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

    /// Reserve `count` `$graph_id`s of a node or edge table: the first of them (they go up by one). Saved with the control state.
    pub(crate) fn take_graph_ids(&self, id: &str, count: usize) -> Option<i64> {
        let first = {
            let mut t = self.tables.lock().unwrap_or_else(|e| e.into_inner());
            let g = t.get_mut(id)?.graph.as_mut()?;
            let first = g.next;
            g.next += count as i64;
            first
        };
        self.save_state();
        Some(first)
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
        ClusterSession { cluster: self.clone(), id: self.next_session.fetch_add(1, Ordering::Relaxed), mirror: self.mirror.connect_internal(), plan: vec![], temp_counter: 0, own_temps: vec![], txn: std::cell::RefCell::new(None), stmt_shards: std::cell::Cell::new(0), commit_seq: 0, vars: HashMap::new(), local_tables: HashMap::new(), tvars: HashMap::new(), identity_insert: Default::default(), last_identity: None, create_dist: None, cur_db: "master".into(), capture: None, trigger_stack: vec![], returning: false, catch_stack: vec![], proc_depth: 0, return_value: None, loop_ctl: 0, temp_creates: HashMap::new(), mirror_temps: Default::default(), uniq_guards: vec![], eventdata: vec![], ddl_rolled_back: false, no_ddl_triggers: false, batch_text: String::new(), cursors: HashMap::new(), fetch_status: 0 }
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
    /// The temporary tables (`#t`) this session made in the cluster: dropped when it ends.
    own_temps: Vec<String>,
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
    /// how the table that `create_table` is about to make is spread, when a RESTORE chooses it (taken by `create_table`)
    create_dist: Option<restore::DistChoice>,
    /// the table variables of the session (by lower-case name) with their columns and rows: a query that reads one has its
    /// rows sent to the shards as a temporary table (see `nested.rs`). (The engine keeps variables for one batch only, so the
    /// session keeps these itself.)
    tvars: HashMap<String, ResultSet>,
    /// the database the session is in (`USE`; the front end tells it before each batch): the tables its statements name are those
    /// of this database, see `dbname.rs`
    cur_db: String,
    /// the rows the statement being run for the triggers of its table changed (see `triggers.rs`)
    capture: Option<triggers::Capture>,
    /// the triggers being run, innermost last: the table and whether it is an INSTEAD OF trigger
    trigger_stack: Vec<(String, bool)>,
    /// a `RETURN` has been run: the statements around it stop (see `exec_flow`)
    returning: bool,
    /// the errors being handled by the `CATCH` blocks that are running, innermost last (what `ERROR_NUMBER()` and `THROW;` read)
    catch_stack: Vec<SqlError>,
    /// procedures that are running, one inside the other (see `execproc.rs`)
    proc_depth: usize,
    /// the value of the `RETURN` that ended the procedure that is running
    return_value: Option<Value>,
    /// `BREAK` (1) or `CONTINUE` (2) has been run: the statements around it stop, up to the `WHILE`
    loop_ctl: u8,
    /// the `CREATE TABLE` of each temporary table the session materialized (by id), and those of them that the control node's engine has too
    /// (it is given one only when a query that it runs reads it: see `refresh_mirror`)
    temp_creates: HashMap<String, String>,
    mirror_temps: std::collections::HashSet<String>,
    /// the locks of unique keys this session holds, until its transaction (or statement) ends
    uniq_guards: Vec<(String, tokio::sync::OwnedMutexGuard<()>)>,
    /// the XML of the events whose DDL triggers are running (`EVENTDATA()`), the innermost last
    eventdata: Vec<String>,
    /// a `ROLLBACK` ran in a DDL trigger (see ddltrig.rs)
    ddl_rolled_back: bool,
    /// no DDL triggers run (a restore makes its objects without them)
    no_ddl_triggers: bool,
    /// the text of the batch being run (the `CommandText` of an event)
    batch_text: String,
    /// the cursors this batch declared (see cursors.rs), and what `@@FETCH_STATUS` says
    cursors: HashMap<String, cursors::CursorState>,
    fetch_status: i32,
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

pub(crate) fn stmt_kind(st: &Stmt) -> String {
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
pub(crate) fn pin_shard(q: &Query, tables: &HashMap<String, DistTable>, cluster: &Cluster) -> Option<usize> {
    let QueryBody::Select(sel) = &q.body else { return None };
    let w = sel.where_.as_ref()?;
    let mut refs: Vec<(String, DistTable)> = vec![];
    // (the conditions of inner joins count as conditions of the WHERE; an outer join is not looked into)
    let mut conds: Vec<&Expr> = vec![w];
    fn collect<'a>(t: &'a TableRef, tables: &HashMap<String, DistTable>, refs: &mut Vec<(String, DistTable)>, conds: &mut Vec<&'a Expr>) -> Option<()> {
        match t {
            TableRef::Table { name, alias, .. } => {
                let dt = tables.get(&table_id(name))?;
                refs.push((alias.clone().or_else(|| name.last().cloned())?.to_lowercase(), dt.clone()));
                Some(())
            }
            TableRef::Join { kind: JoinKind::Inner | JoinKind::Cross, left, right, on } => {
                collect(left, tables, refs, conds)?;
                collect(right, tables, refs, conds)?;
                if let Some(c) = on {
                    conds.push(c);
                }
                Some(())
            }
            _ => None,
        }
    }
    for t in &sel.from {
        collect(t, tables, &mut refs, &mut conds)?;
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
    for c in conds {
        conjuncts(c, &mut cs);
    }
    // does `e` name the shard key column of table `i` (unambiguously)?
    let names_key = |e: &Expr, i: usize| -> bool {
        let (alias, dt) = &refs[i];
        let Dist::Sharded { key, .. } = &dt.dist else { return false };
        let key_name = dt.cols[*key].name.to_lowercase();
        let others = refs.iter().filter(|(a, _)| a != alias).any(|(_, o)| o.cols.iter().any(|c| c.name.eq_ignore_ascii_case(&key_name)));
        match e {
            Expr::Col(p) if p.last().map(|s| s.to_lowercase()) == Some(key_name.clone()) => match p.len() {
                1 => !others,
                n => p[n - 2].to_lowercase() == *alias,
            },
            _ => false,
        }
    };
    // the shard each sharded table is fixed to: by a literal on its key, then by an equality of its key with that of a table that is
    let mut shard: Vec<Option<usize>> = vec![None; refs.len()];
    for (i, (_, dt)) in refs.iter().enumerate() {
        let Dist::Sharded { class, .. } = &dt.dist else { continue };
        for c in &cs {
            if let Expr::Binary(BinOp::Eq, a, b) = c {
                let lit = match (&**a, &**b) {
                    (x, Expr::Lit(v)) if names_key(x, i) => Some(v),
                    (Expr::Lit(v), x) if names_key(x, i) => Some(v),
                    _ => None,
                };
                if let Some(h) = lit.and_then(|v| key_hash(v, *class)) {
                    shard[i] = Some(cluster.shard_of_hash(h));
                    break;
                }
            }
        }
    }
    for _ in 0..refs.len() {
        let mut changed = false;
        for i in 0..refs.len() {
            if shard[i].is_some() || !matches!(refs[i].1.dist, Dist::Sharded { .. }) {
                continue;
            }
            for c in &cs {
                if let Expr::Binary(BinOp::Eq, a, b) = c {
                    for j in 0..refs.len() {
                        let (Dist::Sharded { class: ci, .. }, Dist::Sharded { class: cj, .. }) = (&refs[i].1.dist, &refs[j].1.dist) else { continue };
                        if j != i && shard[j].is_some() && ci == cj && ((names_key(a, i) && names_key(b, j)) || (names_key(b, i) && names_key(a, j))) {
                            shard[i] = shard[j];
                            changed = true;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut pinned: Option<usize> = None;
    for (i, (_, dt)) in refs.iter().enumerate() {
        if !matches!(dt.dist, Dist::Sharded { .. }) {
            continue;
        }
        let s = shard[i]?;
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
    /// `DISTRIBUTE TABLE name INDEX BY column`: a global secondary index
    Index(Name, String),
    /// `DISTRIBUTE PROCEDURE name BY @parameter`: the procedure runs on the shard of the parameter's value (see `procs.rs`)
    Proc(Name, String),
}

fn parse_distribute(sql: &str) -> Option<Result<DistCmd, SqlError>> {
    let t = sql.trim().trim_end_matches(';').trim();
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.len() < 3 || !words[0].eq_ignore_ascii_case("DISTRIBUTE") || !(words[1].eq_ignore_ascii_case("TABLE") || words[1].eq_ignore_ascii_case("PROCEDURE") || words[1].eq_ignore_ascii_case("PROC")) {
        return None;
    }
    let clean = |s: &str| s.trim_matches(|c| c == '[' || c == ']' || c == '"').to_string();
    let name: Name = words[2].split('.').map(clean).collect();
    if !words[1].eq_ignore_ascii_case("TABLE") {
        return Some(if words.len() == 5 && words[3].eq_ignore_ascii_case("BY") && words[4].starts_with('@') {
            Ok(DistCmd::Proc(name, words[4].to_string()))
        } else {
            Err(SqlError::new(102, "Incorrect syntax near DISTRIBUTE. Expected: DISTRIBUTE PROCEDURE name BY @parameter"))
        });
    }
    Some(if words.len() == 4 && words[3].eq_ignore_ascii_case("REPLICATED") {
        Ok(DistCmd::Replicated(name))
    } else if words.len() == 5 && words[3].eq_ignore_ascii_case("BY") {
        Ok(DistCmd::By(name, clean(words[4])))
    } else if words.len() == 6 && words[3].eq_ignore_ascii_case("INDEX") && words[4].eq_ignore_ascii_case("BY") {
        Ok(DistCmd::Index(name, clean(words[5])))
    } else {
        Err(SqlError::new(102, "Incorrect syntax near DISTRIBUTE. Expected: DISTRIBUTE TABLE name BY column | DISTRIBUTE TABLE name REPLICATED | DISTRIBUTE TABLE name INDEX BY column"))
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
        tsql_server::span!("ctl_execute");
        self.plan.clear();
        self.batch_text = sql.to_string();
        self.cursors.clear();
        self.leave_dropped_database();
        front::note_xact_state(sql);
        let cl = self.cluster.clone();
        let mut events = vec![];
        // ADVISE DISTRIBUTION [RESET]: what the queries had to move or gather so far, and the distribution that would have spared it
        {
            let words: Vec<String> = sql.trim().trim_end_matches(';').split_whitespace().map(|w| w.to_lowercase()).collect();
            if words.len() >= 2 && words[0] == "advise" && words[1] == "distribution" {
                if words.len() == 3 && words[2] == "reset" {
                    cl.advisor.clear();
                    return BatchResult { events: vec![Event::Message("The record of moved tables is cleared.".into())] };
                }
                let rs = advisor::advise(&cl.advisor, &cl.tables(), cl.broadcast_limit.load(Ordering::Relaxed));
                return BatchResult { events: vec![Event::Rows(rs)] };
            }
        }
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
        let parsed = {
            tsql_server::span!("ctl_parse");
            tsql_server::parser::parse(sql)
        };
        match parsed {
            Ok(stmts) => {
                let r = self.run_stmts(&stmts);
                // TSQL_PLAN_TRACE=1: one line per statement with its first words and how the cluster ran it (to see which statements
                // of a workload are gathered, joined or scattered)
                if std::env::var_os("TSQL_PLAN_TRACE").is_some() {
                    let head: String = sql.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
                    eprintln!("plan: {} => {}", head, self.plan.join(" | "));
                }
                r
            }
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
            let done = cl.rt.block_on(self.exec_stmt(st, &mut events));
            if self.txn.borrow().is_none() {
                self.uniq_guards.clear();
            }
            if done.is_ok() && self.returning {
                // (a RETURN ends the batch)
                self.returning = false;
                self.return_value = None;
                break;
            }
            self.loop_ctl = 0;
            if let Err(e) = done {
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
        if self.txn.borrow().is_none() {
            self.uniq_guards.clear();
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
        tsql_server::span!("ctl_select_assign");
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
        tsql_server::span!("ctl_ensure_begun");
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

    /// `work` for `shard`, with the start of the transaction in front of it when the transaction has not been there yet: the shard
    /// begins and runs the statement in one request (a request of its own for the `BEGIN` costs a round trip to every shard a
    /// transaction touches). Work that is text goes behind a `BEGIN` of its own, as it may be a statement that must start its batch.
    async fn prepared(&self, shard: usize, work: Work) -> Result<Work, SqlError> {
        self.stmt_shards.set(self.stmt_shards.get().max(1));
        let begins = match self.txn.borrow().as_ref() {
            None => false,
            Some(t) => !t.shards.contains(&shard),
        };
        if !begins {
            return Ok(work);
        }
        match work {
            Work::Stmts(mut v) => {
                let ms = self.cluster.lock_timeout_ms.load(Ordering::Relaxed);
                let mut pre = tsql_server::parser::parse(&format!("SET LOCK_TIMEOUT {}; BEGIN TRANSACTION", ms)).map_err(|e| SqlError::new(50000, e.to_string()))?;
                if let Some(t) = self.txn.borrow_mut().as_mut() {
                    t.shards.insert(shard);
                }
                pre.append(&mut v);
                Ok(Work::Stmts(pre))
            }
            other => {
                self.ensure_begun(&[shard]).await?;
                Ok(other)
            }
        }
    }

    async fn fan_run(&self, shards: &[usize], work: impl Fn(usize) -> Work) -> Result<Vec<RunReply>, SqlError> {
        tsql_server::span!("ctl_fan_run");
        self.stmt_shards.set(self.stmt_shards.get().max(shards.len()));
        let mut works = vec![];
        for &i in shards {
            works.push(self.prepared(i, work(i)).await?);
        }
        let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
        let futs: Vec<_> = clients.iter().zip(works).map(|(c, w)| c.run(self.id, w)).collect();
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
        tsql_server::span!("ctl_fan_query");
        self.stmt_shards.set(self.stmt_shards.get().max(shards.len()));
        let mut works = vec![];
        for &i in shards {
            works.push(self.prepared(i, work(i)).await?);
        }
        let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
        let futs: Vec<_> = clients.iter().zip(works).map(|(c, w)| c.query(self.id, w)).collect();
        join_all(futs).await.into_iter().collect()
    }

    /// Run a statement on the mirror first (which checks it), then on every shard.
    async fn broadcast(&mut self, st: &Stmt) -> Result<(), SqlError> {
        // A schema change in a transaction: the control node's engine, the schema log and the shards' copies are not one transaction, so it cannot be
        // undone with the rest. What the transaction has done so far is committed first (as a DDL statement commits in some other systems; it also
        // lets go of the locks the schema change would wait for), the change is made, and the client goes on in a new transaction of the same depth:
        // a `ROLLBACK` undoes what came after the last schema change.
        let depth = self.txn.borrow().as_ref().map(|t| t.depth);
        let Some(depth) = depth else { return self.broadcast_now(st).await };
        if let Some(t) = self.txn.borrow_mut().as_mut() {
            t.depth = 1;
        }
        let mut none = vec![];
        let committed = self.commit_txn(&mut none).await;
        let r = match committed {
            Ok(()) => self.broadcast_now(st).await,
            Err(e) => Err(e),
        };
        *self.txn.borrow_mut() = Some(TxnState { shards: Default::default(), depth, doomed: false, wrote: Default::default(), savepoints: vec![] });
        r
    }

    async fn broadcast_now(&mut self, st: &Stmt) -> Result<(), SqlError> {
        // (the mirror takes the rows of a graph table as the shards do, with the `$graph_id` they were given, so it makes the table in their form)
        let for_mirror = if matches!(st, Stmt::CreateTable { defs, .. } if defs.iter().any(|d| matches!(d, TableDef::Graph { .. }))) { shard_form(st) } else { st.clone() };
        self.sync_mirror_db()?;
        merge::check(&self.mirror.execute_stmts(std::slice::from_ref(&for_mirror)))?;
        let shards = self.all_shards();
        let on_shards = shard_form(st);
        let work = self.db_work(on_shards);
        self.fan_run(&shards, |_| work.clone()).await?;
        self.plan.push(format!("broadcast {} to {} shards", stmt_kind(st), shards.len()));
        Ok(())
    }

    // ------------------------------------------------------------------ statements

    async fn exec_stmt(&mut self, st: &Stmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        tsql_server::span!("ctl_exec_stmt");
        // (the tables the statement names are in the database the session is in: see `dbname.rs`)
        let qualified = self.qualify(st);
        let qualified = self.qualify_functions(&qualified);
        // (a DEFAULT or CHECK that calls a function that reads sharded tables is the control node's: see rules.rs)
        let (qualified, mut control_rules) = self.split_control_rules(&qualified);
        let st = &qualified;
        self.sync_mirror_db()?;
        // (the DDL triggers of the database run before a statement that raises an event: see ddltrig.rs)
        self.fire_ddl_triggers(st, events).await?;
        match st {
            Stmt::CreateTable { .. } | Stmt::AlterTable { .. } => self.refuse_reading_functions(st, "a computed column")?,
            _ => {}
        }
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
            // (in a DDL trigger there is no transaction to end: the statement the trigger is for is stopped, see ddltrig.rs)
            Stmt::Rollback(None) if !self.eventdata.is_empty() && self.txn.borrow().is_none() => {
                self.ddl_rolled_back = true;
                return Ok(());
            }
            Stmt::Rollback(None) => return self.rollback_txn().await,
            Stmt::SaveTran(name) => return self.save_savepoint(name).await,
            _ => {}
        }
        if let Some(t) = self.txn.borrow().as_ref() {
            // (a transaction that can only be rolled back still lets a batch read, run its control of flow, and report: a CATCH block does)
            let harmless = matches!(st, Stmt::Block(_) | Stmt::If { .. } | Stmt::While { .. } | Stmt::Try { .. } | Stmt::Print(_) | Stmt::Throw(_) | Stmt::RaiseError { .. } | Stmt::Return(_) | Stmt::Break | Stmt::Continue | Stmt::Declare(_) | Stmt::Set { .. } | Stmt::SetOpts(_) | Stmt::Noop)
                || matches!(st, Stmt::Query(q) if !matches!(&q.body, QueryBody::Select(s) if s.into.is_some()));
            if t.doomed && !harmless {
                return Err(SqlError::new(3930, "The current transaction cannot be committed and cannot support operations that write to the log file. Roll back the transaction."));
            }
            // (a schema change inside a transaction commits what came before it: see `broadcast`)
            if matches!(st, Stmt::Truncate(_)) {
                return Err(unsupported("TRUNCATE inside a transaction"));
            }
        }
        if let Stmt::Exec(ex) = st {
            if self.exec_executesql(ex, events).await? {
                return Ok(());
            }
            if !self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).is_empty() && self.exec_routed_proc(ex, events).await? {
                return Ok(());
            }
            // (any other procedure of the cluster runs here, on the control node)
            if self.exec_proc(ex, events).await? {
                return Ok(());
            }
        }
        if let Stmt::ExecStr(parts) = st {
            return self.exec_string(parts, events).await;
        }
        // control of flow: a batch that mixes the cluster's tables with BEGIN ... END, IF and WHILE (SSMS's connect batches do) runs its
        // statements here, one after the other, each as it would be alone
        if matches!(st, Stmt::Block(_) | Stmt::If { .. } | Stmt::While { .. } | Stmt::Try { .. }) {
            return self.exec_flow(st, events).await;
        }
        if self.exec_variable_stmt(st).await? {
            return Ok(());
        }
        // (the variables a cursor's query names are those of the moment it is opened, see cursors.rs)
        let bound = if matches!(st, Stmt::DeclareCursor { .. }) { st.clone() } else { self.bind_vars(st)? };
        let st = &bound;
        match st {
            Stmt::CreateTable { name, defs, versioning } => {
                let r = self.create_table(st, name, defs, versioning.is_some()).await;
                // (a temporary table is the session's: it is dropped when the session ends)
                if let (Ok(()), Some(n)) = (&r, name.last().filter(|n| n.starts_with('#'))) {
                    self.own_temps.push(n.clone());
                }
                if r.is_ok() {
                    // (a function that named this table before it was there reads a sharded one now: refused when a procedure that runs on
                    // the shards calls it, and the table not made)
                    if let Some((p, f)) = self.cluster.stale_procedure() {
                        let id = name.last().cloned().unwrap_or_default();
                        let drop = Stmt::Drop { what: "TABLE".into(), names: vec![name.clone()], if_exists: true };
                        let mut none = vec![];
                        let _ = Box::pin(self.exec_stmt(&drop, &mut none)).await;
                        return Err(SqlError::new(50105, format!("CREATE TABLE {} would make the function {}, which the procedure {} calls on the shards, read a sharded table, and each shard would answer from its slice. Drop or change the procedure first.", id, f, p)));
                    }
                    self.replace_functions_after_table_change().await?;
                    self.add_control_rules(name, std::mem::take(&mut control_rules))?;
                }
                r
            }
            Stmt::AlterTable { name, action } => {
                // (a check of the control node's that is dropped by name)
                if let AlterAction::DropConstraint(names, if_exists) = action {
                    let rest = self.take_control_checks(name, names);
                    if rest.len() < names.len() {
                        if rest.is_empty() {
                            return Ok(());
                        }
                        let st2 = Stmt::AlterTable { name: name.clone(), action: AlterAction::DropConstraint(rest, *if_exists) };
                        if let Stmt::AlterTable { name: n2, action: a2 } = &st2 {
                            let r = self.alter_table(&st2, n2, a2).await;
                            if r.is_ok() {
                                self.refresh_weak_uniques(n2);
                            }
                            return r;
                        }
                    }
                }
                self.verify_control_checks(name, &control_rules).await?;
                let nothing_left = matches!(action, AlterAction::AddDefs(d) if d.is_empty());
                let r = if nothing_left { Ok(()) } else { self.alter_table(st, name, action).await };
                if r.is_ok() {
                    self.add_control_rules(name, std::mem::take(&mut control_rules))?;
                    self.refresh_weak_uniques(name);
                }
                r
            }
            Stmt::Use(db) => self.use_database(db),
            Stmt::CreateDatabase(name, _, _) => self.create_database(st, name).await,
            Stmt::Drop { what, names, if_exists } if what.eq_ignore_ascii_case("DATABASE") => self.drop_database(st, names, *if_exists).await,
            Stmt::Print(e) => {
                let b = self.bind_expr(e)?;
                let v = self.eval_scalar(&b).await?;
                events.push(Event::Message(tsql_server::convert::display(&v)));
                Ok(())
            }
            Stmt::Throw(Some((n, m, s))) => {
                let (n, m, s) = (self.bind_expr(n)?, self.bind_expr(m)?, self.bind_expr(s)?);
                let number = self.eval_scalar(&n).await?.as_i64().unwrap_or(50000) as i32;
                let message = tsql_server::convert::display(&self.eval_scalar(&m).await?);
                let state = self.eval_scalar(&s).await?.as_i64().unwrap_or(1) as u8;
                Err(SqlError::with(number, 16, state, message))
            }
            Stmt::RaiseError { args, .. } => {
                let mut vals = vec![];
                for a in args {
                    let b = self.bind_expr(a)?;
                    vals.push(self.eval_scalar(&b).await?);
                }
                let first = vals.first().cloned().unwrap_or(Value::Null);
                let (number, mut text) = match &first {
                    Value::Str(s) => (50000, s.clone()),
                    other => (other.as_i64().unwrap_or(50000) as i32, String::new()),
                };
                let severity = vals.get(1).and_then(|v| v.as_i64()).unwrap_or(16) as u8;
                let state = vals.get(2).and_then(|v| v.as_i64()).unwrap_or(1) as u8;
                // (%s and %d take the arguments after the state, in order)
                let mut rest = vals.iter().skip(3);
                let mut out = String::new();
                let mut chars = text.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '%' {
                        match chars.peek() {
                            Some('s') | Some('d') | Some('i') | Some('u') => {
                                chars.next();
                                out.push_str(&rest.next().map(tsql_server::convert::display).unwrap_or_default());
                            }
                            Some('%') => {
                                chars.next();
                                out.push('%');
                            }
                            _ => out.push('%'),
                        }
                    } else {
                        out.push(c);
                    }
                }
                text = out;
                if severity <= 10 {
                    events.push(Event::Message(text));
                    Ok(())
                } else {
                    Err(SqlError::with(number, severity, state, text))
                }
            }
            Stmt::Throw(None) => match self.catch_stack.last() {
                Some(e) => Err(e.clone()),
                None => Err(SqlError::new(10704, "To rethrow an error, a THROW statement must be used inside a CATCH block. Insert the THROW statement inside a CATCH block, or insert the argument values for this THROW statement.")),
            },
            Stmt::Return(None) => {
                self.returning = true;
                Ok(())
            }
            Stmt::Return(Some(e)) => {
                let b = self.bind_expr(e)?;
                self.return_value = Some(self.eval_scalar(&b).await?);
                self.returning = true;
                Ok(())
            }
            Stmt::Break => {
                self.loop_ctl = 1;
                Ok(())
            }
            Stmt::Continue => {
                self.loop_ctl = 2;
                Ok(())
            }
            Stmt::Noop => Ok(()),
            Stmt::DeclareCursor { .. } | Stmt::CursorOp { .. } | Stmt::Fetch { .. } => self.exec_cursor(st).await,
            // (sp_rename of an index or a column of a table of the cluster: the shards rename it too, see renames.rs)
            Stmt::Exec(ex) if self.is_cluster_rename(ex) => self.exec_rename(st, ex).await,
            Stmt::Exec(ex) if renames::is_security_proc(ex) => {
                merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
                self.log_schema(st);
                Ok(())
            }
            Stmt::CreateTrigger { .. } => self.create_trigger(st),
            Stmt::Drop { what, names, if_exists } if what.eq_ignore_ascii_case("TRIGGER") => self.drop_triggers(st, names, *if_exists),
            Stmt::TriggerAdmin(a) => self.trigger_admin(a),
            Stmt::CreateFunction(cf) => self.create_function(st, cf).await,
            Stmt::Drop { what, names, if_exists } if what.eq_ignore_ascii_case("FUNCTION") => self.drop_functions(st, names, *if_exists).await,
            Stmt::CreateProc { name, .. } => {
                // (a procedure is made on every shard, which runs it; a changed one must be declared again)
                self.broadcast(st).await?;
                self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).remove(&table_id(name));
                self.log_schema(st);
                Ok(())
            }
            // (users, roles and permissions are the control node's engine's, which authenticates the clients and holds the schema of every table; a
            // `GRANT` on a table of the cluster is recorded there. Logins carry passwords and the session's identity changes with `EXECUTE AS`: neither is
            // written to the schema log. The rest is, so that a control node that starts again has it)
            Stmt::Security(sec) => {
                merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
                if !matches!(&**sec, SecStmt::CreateLogin { .. } | SecStmt::AlterLogin { .. } | SecStmt::ExecuteAs { .. } | SecStmt::Revert) {
                    self.log_schema(st);
                }
                self.plan.push("security statement: kept by the control node's engine".into());
                Ok(())
            }
            // (a full-text catalog or index is a declaration, which the shards and the control node's engine keep: a search scans the rows)
            Stmt::FullText(_) => {
                self.broadcast(st).await?;
                self.log_schema(st);
                Ok(())
            }
            // (a user-defined type is made on every shard and on the control node's engine: the columns and parameters that use it are there)
            Stmt::CreateType { .. } => {
                self.broadcast(st).await?;
                self.log_schema(st);
                Ok(())
            }
            Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("TYPE") => {
                self.broadcast(st).await?;
                self.log_schema(st);
                Ok(())
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("PROCEDURE") || what.eq_ignore_ascii_case("PROC") => {
                self.broadcast(st).await?;
                for n in names {
                    self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).remove(&table_id(n));
                }
                self.log_schema(st);
                Ok(())
            }
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
                self.log_schema(st);
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
                self.log_schema(st);
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
                // (the index tables of a table that is dropped go with it)
                let mut dropped: Vec<Name> = names.clone();
                for n in names {
                    if let Some(t) = self.table_by_id(&table_id(n)) {
                        dropped.extend(t.indexes.iter().map(|i| i.table.clone()));
                    }
                }
                let st = &Stmt::Drop { what: "TABLE".into(), names: dropped.clone(), if_exists: matches!(st, Stmt::Drop { if_exists: true, .. }) };
                self.broadcast(st).await?;
                let mut t = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
                for n in &dropped {
                    t.remove(&table_id(n));
                }
                drop(t);
                {
                    let ids: Vec<String> = names.iter().map(|n| table_id(n)).collect();
                    self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).retain(|f| !ids.contains(&f.child) && !ids.contains(&f.parent));
                }
                {
                    let ids: Vec<String> = names.iter().map(|n| table_id(n)).collect();
                    self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner()).retain(|(_, s)| !matches!(s, Stmt::CreateTrigger { table, .. } if ids.contains(&table_id(table))));
                }
                self.log_schema(st);
                self.replace_functions_after_table_change().await?;
                Ok(())
            }
            Stmt::CreateIndex { name: ix_name, table, unique, cols, xml, spatial, filter, .. } => {
                // (an XML or spatial index is a declaration, which the shards and the control node's engine keep: it changes no answer)
                let _ = (xml, spatial);
                let t = self.table(table)?;
                let mut weak = false;
                if let (true, Dist::Sharded { key, .. }) = (*unique, &t.dist) {
                    if !cols.iter().any(|(c, _)| c.eq_ignore_ascii_case(&t.cols[*key].name)) {
                        if filter.is_some() {
                            return Err(unsupported(&format!("a filtered unique index that does not contain the shard key ({})", t.cols[*key].name)));
                        }
                        // (the rows there are checked against it, across the shards: see uniq.rs)
                        self.lock_unique(&t.id).await?;
                        let names: Vec<String> = cols.iter().map(|(c, _)| c.clone()).collect();
                        self.verify_unique_existing(table, &names, ix_name).await?;
                        weak = true;
                    }
                }
                self.broadcast(st).await?;
                self.log_schema(st);
                if weak {
                    self.refresh_weak_uniques(table);
                }
                Ok(())
            }
            Stmt::DropIndex { names, .. } => {
                self.broadcast(st).await?;
                self.log_schema(st);
                for (_, t) in names.iter() {
                    if let Some(t) = t {
                        self.refresh_weak_uniques(t);
                    }
                }
                Ok(())
            }
            Stmt::AlterIndex { .. } => {
                self.broadcast(st).await?;
                self.log_schema(st);
                Ok(())
            }
            Stmt::SetOpts(opts) => {
                // IDENTITY_INSERT is about the numbers the control node hands out: the shards' tables have no identity
                let mut rest: Vec<SetOpt> = vec![];
                for o in opts {
                    match o {
                        SetOpt::IdentityInsert(name, on) => {
                            let id = self.qid(name);
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
            Stmt::Restore(r) if r.kind == RestoreKind::Database => self.restore_database(r, events).await,
            // (the control node's own copy of the cluster's database holds no rows: a backup of it would be empty)
            Stmt::Backup(_) => Err(unsupported("BACKUP of the cluster's database (its rows are on the shards)")),
            // any other procedure is the control node's engine's (a system procedure; a name that is nothing is its error 2812)
            Stmt::Exec(ex) if !ex.name_is_var => {
                let res = self.mirror.execute_stmts(std::slice::from_ref(st));
                merge::check(&res)?;
                events.extend(res.events);
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
        let mut identity: Option<(usize, i64, i64)> = None;
        let mut graph: Option<bool> = None;
        let mut edge_cons: Vec<EdgeCons> = vec![];
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
                    let type_name = self.base_type_name(cd.ty.as_ref());
                    cols.push(ColInfo { name: cd.name.clone(), computed: cd.computed.is_some(), type_name, hidden: false });
                }
                TableDef::PrimaryKey { cols: c, .. } => pk = c.iter().map(|(n, _)| n.clone()).collect(),
                TableDef::Period { .. } => return Err(unsupported("system-versioned tables")),
                // (the control node numbers the rows of a graph table, `$graph_id`: the shards would each number them from 0)
                TableDef::Graph { edge, .. } => graph = Some(*edge),
                TableDef::EdgeConstraint { name: n, pairs, cascade } => {
                    edge_cons.push(EdgeCons { name: n.clone().unwrap_or_else(|| format!("EC__{}", name.last().cloned().unwrap_or_default())), pairs: pairs.iter().map(|(f, t)| (table_id(f), table_id(t))).collect(), cascade: *cascade });
                }
                _ => {}
            }
        }
        // a graph table's own columns come first, as the engine puts them (see graph_columns in the engine)
        let mut graph_meta: Option<GraphMeta> = None;
        if let Some(edge) = graph {
            let col = |n: &str, computed: bool, hidden: bool| ColInfo { name: n.to_string(), computed, type_name: if n == "$graph_id" { "bigint".to_string() } else { "nvarchar".to_string() }, hidden };
            let mut own = vec![col("$graph_id", false, true), col(if edge { "$edge_id" } else { "$node_id" }, true, false)];
            if edge {
                own.push(col("$from_id", false, false));
                own.push(col("$to_id", false, false));
            }
            if let Some(i) = identity.as_mut() {
                i.0 += own.len();
            }
            own.extend(cols);
            cols = own;
            graph_meta = Some(GraphMeta { edge, next: 0 });
        }
        // foreign keys are the control node's to enforce (the shards and its own engine are not given them)
        let fks = self.declared_fks(name, defs, &pk)?;
        let stripped = without_fks(st);
        let st = &stripped;
        // the shard key: the first column of the primary key, else the first column (a RESTORE may name another, or make the
        // table replicated: see restore.rs)
        let choice = self.create_dist.take();
        let dist = if choice == Some(restore::DistChoice::Replicated) {
            Dist::Replicated
        } else {
            let named = matches!(&choice, Some(restore::DistChoice::By(_)));
            let preferred = match &choice {
                Some(restore::DistChoice::By(c)) => c.clone(),
                _ => pk.first().cloned().or_else(|| cols.iter().find(|c| !c.computed && !c.hidden).or_else(|| cols.iter().find(|c| !c.computed)).map(|c| c.name.clone())).ok_or_else(|| SqlError::new(1113, "A table needs at least one column."))?,
            };
            let usable = |n: &str| cols.iter().find(|c| c.name.eq_ignore_ascii_case(n)).is_some_and(|c| KeyClass::of_type_name(&c.type_name).is_some());
            // (a column that was asked for is the key, or an error; the default one, the first column or the first of the primary key, is the key when the
            // cluster can shard on its type, else the first column it can: a table of columns of no such type is replicated)
            let key_name = if named || usable(&preferred) { Some(preferred) } else { cols.iter().find(|c| !c.computed && !c.hidden && KeyClass::of_type_name(&c.type_name).is_some()).map(|c| c.name.clone()) };
            match key_name {
                None => {
                    self.plan.push("no column of the table can be a shard key: it is replicated".into());
                    Dist::Replicated
                }
                Some(key_name) => {
                    let key = cols.iter().position(|c| c.name.eq_ignore_ascii_case(&key_name)).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table.", key_name)))?;
                    let class = KeyClass::of_type_name(&cols[key].type_name)
                        .ok_or_else(|| unsupported(&format!("the shard key {} of type {} (use an integer, character or uniqueidentifier first key column, or DISTRIBUTE TABLE ... REPLICATED)", key_name, cols[key].type_name)))?;
                    // (a unique key without the shard key is checked across the shards by the control node: see uniq.rs; a filtered one is refused)
                    Dist::Sharded { key, class }
                }
            }
        };
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
        self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), DistTable { id, name: name.clone(), cols, dist, version: 0, loaded: None, rows: None, identity: identity.map(|(col, next, inc)| Identity { col, next, inc }), indexes: vec![], graph: graph_meta, edge_cons, weak_uniques: vec![] });
        self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).extend(fks);
        self.log_schema(st);
        self.refresh_weak_uniques(name);
        Ok(())
    }

    /// The name of the type of a column as the cluster classifies shard keys by it: a user-defined alias type is its base type.
    fn base_type_name(&self, ty: Option<&TypeRef>) -> String {
        let mut name = ty.and_then(|t| t.parts.last()).map(|s| s.to_lowercase()).unwrap_or_default();
        let log = self.cluster.schema_log.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..8 {
            let base = log.iter().rev().find_map(|st| match st {
                Stmt::CreateType { name: n, base: Some(b), .. } if n.last().is_some_and(|l| l.eq_ignore_ascii_case(&name)) => b.parts.last().map(|s| s.to_lowercase()),
                _ => None,
            });
            match base {
                Some(b) => name = b,
                None => break,
            }
        }
        name
    }

    /// `BEGIN ... END`, `IF ... ELSE` and `WHILE`: the condition is worked out by the cluster (it may read the cluster's tables), and the
    /// statements of the branch it takes run through the cluster as they would alone. Not here: `BREAK`, `CONTINUE`, `RETURN`, `TRY ... CATCH`.
    async fn exec_flow(&mut self, st: &Stmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        match st {
            Stmt::Block(stmts) => {
                for s in stmts {
                    Box::pin(self.exec_stmt(s, events)).await?;
                    if self.returning || self.loop_ctl != 0 {
                        break;
                    }
                }
                Ok(())
            }
            Stmt::If { cond, then, els } => {
                if self.condition(cond).await? {
                    Box::pin(self.exec_stmt(then, events)).await
                } else if let Some(e) = els {
                    Box::pin(self.exec_stmt(e, events)).await
                } else {
                    Ok(())
                }
            }
            Stmt::While { cond, body } => {
                let mut rounds = 0usize;
                while self.condition(cond).await? {
                    Box::pin(self.exec_stmt(body, events)).await?;
                    if self.returning {
                        break;
                    }
                    match self.loop_ctl {
                        1 => {
                            self.loop_ctl = 0;
                            break;
                        }
                        2 => self.loop_ctl = 0,
                        _ => {}
                    }
                    rounds += 1;
                    if rounds >= 1_000_000 {
                        return Err(unsupported("a WHILE loop of more than a million rounds"));
                    }
                }
                Ok(())
            }
            Stmt::Try { body, catch } => self.exec_try(body, catch, events).await,
            other => Err(unsupported(&format!("the statement {}", stmt_kind(other)))),
        }
    }

    /// `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH`. An error of the `TRY` block (severity below 20) runs the `CATCH` block, in which
    /// `ERROR_NUMBER()`, `ERROR_MESSAGE()`, `ERROR_SEVERITY()`, `ERROR_STATE()`, `ERROR_PROCEDURE()`, `XACT_STATE()` and `THROW;` tell and
    /// repeat the error. A statement that fails inside a shard is undone by the shard; one that had changed several shards, or that failed
    /// because a shard did, leaves the transaction half done, so the transaction can only be rolled back (`XACT_STATE() = -1`: the `CATCH`
    /// block may read, but a write is error 3930), as in SQL Server. A deadlock victim (1205) is not caught: its transaction is already rolled back.
    async fn exec_try(&mut self, body: &[Stmt], catch: &[Stmt], events: &mut Vec<Event>) -> Result<(), SqlError> {
        let outer_shards = self.stmt_shards.get();
        let trigger_depth = self.trigger_stack.len();
        let mut caught: Option<SqlError> = None;
        for s in body {
            self.stmt_shards.set(0);
            match Box::pin(self.exec_stmt(s, events)).await {
                Ok(()) => {
                    if self.returning || self.loop_ctl != 0 {
                        break;
                    }
                }
                Err(e) => {
                    if e.severity >= 20 || e.number == 1205 || e.number == 596 {
                        return Err(e);
                    }
                    let fatal = matches!(e.number, 1222 | 40613 | 3314 | 50200 | 50210);
                    if self.stmt_shards.get() > 1 || fatal {
                        if let Some(t) = self.txn.borrow_mut().as_mut() {
                            t.doomed = true;
                        }
                    }
                    caught = Some(e);
                    break;
                }
            }
        }
        self.stmt_shards.set(outer_shards);
        let Some(mut e) = caught else { return Ok(()) };
        self.trigger_stack.truncate(trigger_depth);
        self.capture = None;
        self.returning = false;
        e.abort_batch = false;
        self.catch_stack.push(e);
        let mut r = Ok(());
        for s in catch {
            r = Box::pin(self.exec_stmt(s, events)).await;
            if r.is_err() || self.returning || self.loop_ctl != 0 {
                break;
            }
        }
        self.catch_stack.pop();
        r
    }

    /// Whether a condition holds (a NULL does not).
    async fn condition(&mut self, cond: &Expr) -> Result<bool, SqlError> {
        let bound = self.bind_expr(cond)?;
        let case = Expr::Case { operand: None, whens: vec![(bound, Expr::Lit(Value::Int(1)))], els: Some(Box::new(Expr::Lit(Value::Int(0)))) };
        Ok(matches!(self.eval_scalar(&case).await?, Value::Int(1)))
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
        // an IDENTITY column added to an empty table: (its name, seed, increment), numbered by the control node as any identity column is
        let mut new_identity: Option<(String, i64, i64)> = None;
        match action {
            AlterAction::AddDefs(defs) => {
                for d in defs {
                    match d {
                        TableDef::Column(cd) => {
                            if let Some((seed, inc)) = &cd.identity {
                                if t.identity.is_some() {
                                    return Err(SqlError::new(2744, format!("Multiple identity columns specified for table '{}'. Only one identity column per table is allowed.", name.last().cloned().unwrap_or_default())));
                                }
                                if !self.table_is_empty(&t).await? {
                                    return Err(unsupported("adding an IDENTITY column to a table that has rows (the rows would need numbers)"));
                                }
                                let seed = self.eval_scalar(seed).await?.as_i64().unwrap_or(1);
                                let inc = self.eval_scalar(inc).await?.as_i64().unwrap_or(1);
                                new_identity = Some((cd.name.clone(), seed, inc));
                            }
                            if cd.pk.is_some() {
                                return Err(unsupported("adding a primary key"));
                            }
                            // (a column added with UNIQUE has no value in the rows that are there: only an empty table can take it)
                            if cd.unique.is_some() && !self.table_is_empty(&t).await? {
                                return Err(unsupported("adding a UNIQUE column to a table that has rows"));
                            }
                        }
                        // (a primary key made later: one with the shard key is the shards' to keep, as any key is; one without is checked across the shards
                        // by the control node, against the rows that are there first: see uniq.rs)
                        TableDef::PrimaryKey { cols, name: pname } => {
                            if t.cols.iter().any(|c| c.computed) && cols.iter().any(|(n, _)| t.cols.iter().any(|c| c.computed && c.name.eq_ignore_ascii_case(n))) {
                                return Err(unsupported("a primary key on a computed column"));
                            }
                            if !cols.iter().any(|(c, _)| is(c, &key_col)) && key_col.is_some() {
                                self.lock_unique(&table_id(name)).await?;
                                let names: Vec<String> = cols.iter().map(|(c, _)| c.clone()).collect();
                                self.verify_unique_existing(name, &names, pname.as_deref().unwrap_or("PK")).await?;
                            }
                        }
                        TableDef::Unique { cols, name: uname, .. } => {
                            if !cols.iter().any(|(c, _)| is(c, &key_col)) && key_col.is_some() {
                                // (the rows there are checked against it, across the shards: see uniq.rs)
                                self.lock_unique(&table_id(name)).await?;
                                let names: Vec<String> = cols.iter().map(|(c, _)| c.clone()).collect();
                                self.verify_unique_existing(name, &names, uname.as_deref().unwrap_or("UQ")).await?;
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
        // the edge constraints of an edge table, as they are now
        {
            let mut tables = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = tables.get_mut(&t.id) {
                match action {
                    AlterAction::AddDefs(defs) => {
                        for d in defs {
                            if let TableDef::EdgeConstraint { name: n, pairs, cascade } = d {
                                e.edge_cons.push(EdgeCons { name: n.clone().unwrap_or_else(|| format!("EC__{}", name.last().cloned().unwrap_or_default())), pairs: pairs.iter().map(|(f, to)| (table_id(f), table_id(to))).collect(), cascade: *cascade });
                            }
                        }
                    }
                    AlterAction::DropConstraint(names, _) => e.edge_cons.retain(|c| !names.iter().any(|n| n.eq_ignore_ascii_case(&c.name))),
                    _ => {}
                }
            }
        }
        // the columns as the control node's engine now has them
        let shown = format!("[{}].[{}]", if name.len() >= 2 { name[name.len() - 2].clone() } else { "dbo".to_string() }, name.last().cloned().unwrap_or_default());
        if let Some(info) = self.mirror.table_info(&shown) {
            let cols: Vec<ColInfo> = info.columns.iter().map(|c| ColInfo { name: c.name.clone(), computed: c.computed, type_name: c.ty.split('(').next().unwrap_or("").trim().to_lowercase(), hidden: t.graph.is_some() && c.name == "$graph_id" }).collect();
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
                if let Some((n, seed, inc)) = &new_identity {
                    if let Some(p) = cols.iter().position(|c| c.name.eq_ignore_ascii_case(n)) {
                        e.identity = Some(Identity { col: p, next: *seed, inc: *inc });
                    }
                }
                e.cols = cols;
                e.version += 1;
                e.loaded = None;
                e.rows = None;
            }
        }
        self.log_schema(st);
        Ok(())
    }

    /// Whether a table has no rows on any shard.
    async fn table_is_empty(&mut self, t: &DistTable) -> Result<bool, SqlError> {
        // (it only reads: a statement that fails after it has not changed several shards)
        let shards_before = self.stmt_shards.get();
        let count = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: Expr::Func(Box::new(FuncCall { name: "COUNT".into(), schema: None, parts: vec![], args: vec![], distinct: false, star: true, over: None, within: None })), alias: None }], into: None, from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }], where_: None, group: None, having: None });
        let shards = self.all_shards();
        let counts = self.fan_query(&shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(count.clone()))])).await;
        self.stmt_shards.set(shards_before);
        let counts = counts?;
        Ok(counts.iter().all(|r| r.rows.first().map(|x| matches!(x[0], Value::Int(0))).unwrap_or(true)))
    }

    /// `DISTRIBUTE PROCEDURE p BY @param`: check the procedure body against the declaration (see `procs.rs`) and route its calls.
    fn distribute_proc(&mut self, name: Name, param: String) -> Result<(), SqlError> {
        let id = table_id(&name);
        let created = {
            let log = self.cluster.schema_log.lock().unwrap_or_else(|e| e.into_inner());
            log.iter().rev().find_map(|st| match st {
                Stmt::CreateProc { name: n, params, body, .. } if table_id(n) == id => Some((params.clone(), body.clone())),
                _ => None,
            })
        };
        let Some((params, body)) = created else { return Err(SqlError::new(2812, format!("Could not find stored procedure '{}'.", name.join(".")))) };
        let tables = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let lookup = |i: &str| tables.get(i).cloned();
        let route = procs::validate(&params, &param, &body, &lookup, &self.cluster).map_err(|why| SqlError::new(50104, format!("The procedure {} cannot run on one shard: {}.", name.join("."), why)))?;
        // (a procedure that runs on a shard can call only the functions that are there: not one that reads sharded tables)
        if let Some((_, f)) = self.cluster.procedure_calling_reader(Some(&id), false) {
            return Err(SqlError::new(50104, format!("The procedure {} cannot run on one shard: it calls the function {}, which reads sharded tables, and a shard would answer from its slice.", name.join("."), f)));
        }
        self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).insert(id, route);
        self.cluster.save_state();
        Ok(())
    }

    /// An EXEC of a procedure that runs on one shard: the arguments are evaluated, and the call is sent to the shard that owns the
    /// value of the key parameter (inside the session transaction, when there is one). False when the procedure is not one of those.
    async fn exec_routed_proc(&mut self, ex: &ExecStmt, events: &mut Vec<Event>) -> Result<bool, SqlError> {
        tsql_server::span!("ctl_exec_routed_proc");
        if ex.name_is_var {
            return Ok(false);
        }
        let id = table_id(&ex.name);
        let Some(route) = self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned() else { return Ok(false) };
        if ex.retvar.is_some() || ex.args.iter().any(|a| a.out) {
            return Err(unsupported("a procedure call that takes a return value or an OUTPUT parameter"));
        }
        let params = {
            let log = self.cluster.schema_log.lock().unwrap_or_else(|e| e.into_inner());
            log.iter().rev().find_map(|st| match st {
                Stmt::CreateProc { name: n, params, .. } if table_id(n) == id => Some(params.clone()),
                _ => None,
            })
        };
        let Some(params) = params else { return Ok(false) };
        // the arguments as values: by name, or by position
        let mut args = ex.args.clone();
        let mut key: Option<Value> = None;
        for (i, a) in args.iter_mut().enumerate() {
            let b = self.bind_expr(&a.e)?;
            let v = self.eval_scalar(&b).await?;
            let is_key = match &a.name {
                Some(n) => n.trim_start_matches('@').eq_ignore_ascii_case(params[route.param].name.trim_start_matches('@')),
                None => i == route.param,
            };
            if is_key {
                key = Some(v.clone());
            }
            a.e = Expr::Lit(v);
        }
        let Some(key) = key else { return Err(SqlError::new(201, format!("Procedure '{}' expects parameter '{}', which was not supplied.", ex.name.join("."), params[route.param].name))) };
        let h = key_hash(&key, route.class).ok_or_else(|| SqlError::new(245, format!("The value {} cannot be used as the shard key of {}.", tsql_server::convert::display(&key), ex.name.join("."))))?;
        let shard = self.cluster.shard_of_hash(h);
        let st = Stmt::Exec(Box::new(ExecStmt { name: ex.name.clone(), name_is_var: false, args, retvar: None }));
        self.plan.push(format!("procedure {} runs on shard {}", ex.name.join("."), shard));
        let work = self.prepared(shard, Work::Stmts(vec![st])).await?;
        if route.rows {
            // (every result set the procedure returns; there may be none, when it ends before it returns any, as New-Order does when it
            // rolls back)
            for rs in self.cluster.shard(shard).run_sets(self.id, work).await? {
                events.push(Event::Rows(rs));
            }
        } else {
            let reply = self.cluster.shard(shard).run(self.id, work).await?;
            if let Some(e) = reply.first_error() {
                return Err(e.to_sql());
            }
        }
        for t in &route.writes {
            self.bump(t);
        }
        Ok(true)
    }

    /// `DISTRIBUTE TABLE t INDEX BY col`: make the global index table (sharded by the value, indexed on it) and register it.
    async fn distribute_index(&mut self, name: Name, column: String) -> Result<(), SqlError> {
        let mut t = self.table(&name)?;
        let Dist::Sharded { key, class: key_class } = t.dist.clone() else { return Err(unsupported("a global index on a replicated table")) };
        let pos = t.cols.iter().position(|x| x.name.eq_ignore_ascii_case(&column)).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", column)))?;
        if pos == key {
            return Err(SqlError::new(50103, "The shard key needs no global index: a condition on it already routes."));
        }
        let class = KeyClass::of_type_name(&t.cols[pos].type_name).ok_or_else(|| unsupported(&format!("a global index on a column of type {}", t.cols[pos].type_name)))?;
        if t.indexes.iter().any(|i| i.col == pos) {
            return Err(SqlError::new(1913, format!("The operation failed because an index or statistics with name '{}' already exists.", column)));
        }
        // an index can only be started on an empty table (it would have to be built from the rows otherwise)
        let count = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: Expr::Func(Box::new(FuncCall { name: "COUNT".into(), schema: None, parts: vec![], args: vec![], distinct: false, star: true, over: None, within: None })), alias: None }], into: None, from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }], where_: None, group: None, having: None });
        let shards = self.all_shards();
        let counts = self.fan_query(&shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(count.clone()))])).await?;
        if counts.iter().any(|r| r.rows.first().map(|x| !matches!(x[0], Value::Int(0))).unwrap_or(false)) {
            return Err(SqlError::new(50101, format!("DISTRIBUTE TABLE ... INDEX needs an empty table; {} has rows.", name.join("."))));
        }
        // the column types are the base table's own
        let find = |c: &str| -> Option<ColumnDef> {
            let log = self.cluster.schema_log.lock().unwrap_or_else(|e| e.into_inner());
            log.iter().find_map(|st| match st {
                Stmt::CreateTable { name: n, defs, .. } if table_id(n) == t.id => defs.iter().find_map(|d| match d {
                    TableDef::Column(cd) if cd.name.eq_ignore_ascii_case(c) => Some(cd.clone()),
                    _ => None,
                }),
                _ => None,
            })
        };
        let (Some(vcol), Some(kcol)) = (find(&t.cols[pos].name), find(&t.cols[key].name)) else { return Err(unsupported("a global index on a table whose definition is not known")) };
        let plain = |name: &str, from: &ColumnDef| TableDef::Column(ColumnDef { name: name.to_string(), ty: from.ty.clone(), nullable: Some(true), identity: None, default: None, default_text: None, default_name: None, computed: None, computed_text: None, pk: None, unique: None, checks: vec![], fk: None, collation: from.collation.clone(), mask: None, generated: None });
        let idx_name = format!("gsi_{}_{}", t.name.last().cloned().unwrap_or_default(), t.cols[pos].name);
        let idx_table: Name = vec![idx_name.clone()];
        let mut none = vec![];
        let create = Stmt::CreateTable { name: idx_table.clone(), defs: vec![plain("gv", &vcol), plain("gk", &kcol)], versioning: None };
        self.exec_stmt(&create, &mut none).await?;
        let ix = Stmt::CreateIndex { name: format!("ix_{}", idx_name), table: idx_table.clone(), cols: vec![("gv".to_string(), false)], unique: false, include: vec![], filter: None, xml: None, spatial: None, columnstore: None };
        self.exec_stmt(&ix, &mut none).await?;
        let _ = key_class;
        t.indexes.push(GlobalIndex { col: pos, table: idx_table, class });
        self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).insert(t.id.clone(), t);
        self.cluster.save_state();
        Ok(())
    }

    async fn distribute(&mut self, cmd: DistCmd) -> Result<(), SqlError> {
        // (the table or procedure is the one of the database the session is in)
        let cmd = match cmd {
            DistCmd::By(n, c) => DistCmd::By(self.qn(&n), c),
            DistCmd::Replicated(n) => DistCmd::Replicated(self.qn(&n)),
            DistCmd::Index(n, c) => DistCmd::Index(self.qn(&n), c),
            DistCmd::Proc(n, p) => DistCmd::Proc(self.qn(&n), p),
        };
        self.sync_mirror_db()?;
        if let DistCmd::Index(n, c) = cmd {
            return self.distribute_index(n, c).await;
        }
        if let DistCmd::Proc(n, p) = cmd {
            return self.distribute_proc(n, p);
        }
        let (name, by) = match &cmd {
            DistCmd::By(n, c) => (n.clone(), Some(c.clone())),
            DistCmd::Replicated(n) => (n.clone(), None),
            DistCmd::Index(..) | DistCmd::Proc(..) => unreachable!(),
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
        let id = t.id.clone();
        let before = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), t);
        // (a procedure runs on the shards, and so does a function it calls, only while that function reads replicated tables: refuse a
        // change that would leave it calling one over slices)
        if let Some((p, f)) = self.cluster.stale_procedure() {
            if let Some(old) = before {
                self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), old);
            }
            return Err(SqlError::new(50105, format!("DISTRIBUTE TABLE {} would make the function {}, which the procedure {} calls on the shards, read a sharded table, and each shard would answer from its slice. Drop or change the procedure (or the function) first.", name.join("."), f, p)));
        }
        self.cluster.save_state();
        // (a function that reads the table runs on the shards only while the table is replicated)
        self.replace_functions_after_table_change().await?;
        Ok(())
    }

    async fn eval_scalar(&mut self, e: &Expr) -> Result<Value, SqlError> {
        tsql_server::span!("ctl_eval_scalar");
        if let Expr::Lit(v) = e {
            return Ok(v.clone());
        }
        let q = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: e.clone(), alias: None }], into: None, from: vec![], where_: None, group: None, having: None });
        if has_subquery(e) || self.reading_function_tables(e).is_some() || self.calls_function_reading_tables(e) {
            // a subquery may read distributed tables (and so may a function): the cluster answers it
            let rs = self.run_query(&q).await?;
            return rs.rows.first().and_then(|r| r.first()).cloned().ok_or_else(|| SqlError::new(50000, "an expression returned no value"));
        }
        let res = self.mirror.execute_stmts(&[Stmt::Query(Box::new(q))]);
        merge::check(&res)?;
        res.resultsets().first().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().ok_or_else(|| SqlError::new(50000, "an expression returned no value"))
    }

    /// An INSERT. A table with global indexes gets its entries in the same transaction as its rows (one of its own when the session
    /// has none), since they are on other shards.
    async fn insert(&mut self, ins: &Insert, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let indexed = match &ins.target {
            Target::Table(name) => self.table(name).map(|t| !t.indexes.is_empty()).unwrap_or(false),
            _ => false,
        };
        if !indexed {
            return self.insert_values(ins, events).await;
        }
        let own_txn = self.txn.borrow().is_none();
        if own_txn {
            *self.txn.borrow_mut() = Some(TxnState { shards: Default::default(), depth: 1, doomed: false, wrote: Default::default(), savepoints: vec![] });
        }
        let out = self.insert_values(ins, events).await;
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

    /// The rows of the first result set of `EXEC`, for `INSERT ... EXEC`: a `sp_executesql` of a literal `SELECT` is that query, run on the
    /// cluster; anything else (the system procedures SSMS and the drivers fill their temporary tables from) is run by the control node's own
    /// engine. A procedure that is the cluster's (made with `CREATE PROCEDURE`) is not run here.
    async fn exec_rows(&mut self, exec: &Stmt) -> Result<Vec<Vec<Value>>, SqlError> {
        if let Stmt::Exec(ex) = exec {
            if let Some(inner) = self.executesql_text(ex) {
                if let Ok(stmts) = tsql_server::parser::parse(&inner) {
                    if let [Stmt::Query(q)] = stmts.as_slice() {
                        return Ok(self.run_query(q).await?.rows);
                    }
                }
            }
        }
        let res = self.mirror.execute_stmts(std::slice::from_ref(exec));
        merge::check(&res)?;
        Ok(first_rows(res)?.rows)
    }

    async fn insert_values(&mut self, ins: &Insert, events: &mut Vec<Event>) -> Result<(), SqlError> {
        tsql_server::span!("ctl_insert_values");
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
        // (`WITH c AS (...) INSERT INTO t SELECT ... FROM c`: the common table expressions are those of the SELECT)
        if !ins.ctes.is_empty() && ins.output.is_none() && ins.top.is_none() {
            if let InsertSrc::Query(q) = &ins.src {
                let mut q2 = (**q).clone();
                let mut ctes = ins.ctes.clone();
                ctes.extend(q2.ctes.drain(..));
                q2.ctes = ctes;
                let ins2 = Insert { ctes: vec![], src: InsertSrc::Query(Box::new(q2)), ..ins.clone() };
                return Box::pin(self.insert_values(&ins2, events)).await;
            }
        }
        let t = self.table(name)?;
        if ins.output.is_some() || ins.top.is_some() || !ins.ctes.is_empty() {
            return Err(unsupported("INSERT with OUTPUT, TOP or CTEs"));
        }
        if self.triggers_apply(&t.id, triggers::Ev::Insert) {
            return self.dml_with_triggers(&t, triggers::Ev::Insert, Stmt::Insert(Box::new(ins.clone())), events).await;
        }
        let rows: Vec<Vec<Expr>> = match &ins.src {
            InsertSrc::Values(v) => {
                // (a call of a function that reads sharded tables is evaluated here, and the shards get its value)
                let mut rows = v.clone();
                for r in rows.iter_mut() {
                    for cell in r.iter_mut() {
                        if let Some(e2) = self.fold_reading_calls(cell, None, None).await? {
                            *cell = e2;
                        }
                    }
                }
                self.refuse_reading_functions(&rows, "an INSERT")?;
                rows
            }
            InsertSrc::Query(q) => {
                let rs = self.run_query(q).await?;
                rs.rows.into_iter().map(|r| r.into_iter().map(Expr::Lit).collect()).collect()
            }
            InsertSrc::DefaultValues => return Err(unsupported("INSERT ... DEFAULT VALUES")),
            InsertSrc::Exec(ex) => {
                // the rows the procedure returns (see `exec_rows`)
                self.exec_rows(ex).await?.into_iter().map(|r| r.into_iter().map(Expr::Lit).collect()).collect()
            }
        };
        let n = self.cluster.shard_count();
        let ident_col = t.identity.as_ref().map(|i| t.cols[i.col].name.clone());
        let given: Option<Vec<String>> = ins.cols.clone();
        // without a column list the values are for the columns that are not computed (and not the identity column, unless
        // IDENTITY_INSERT is on)
        let mut names: Vec<String> = given.clone().unwrap_or_else(|| {
            t.cols.iter().filter(|c| !c.computed && !c.hidden && !(ident_col.as_deref() == Some(c.name.as_str()) && !self.identity_insert.contains(&t.id))).map(|c| c.name.clone()).collect()
        });
        if let Some(bad) = rows.iter().find(|r| r.len() != names.len()) {
            return Err(SqlError::new(213, format!("Column name or number of supplied values does not match table definition ({} values for {} columns).", bad.len(), names.len())));
        }
        let mut rows = rows;
        // a DEFAULT of the control node's (see rules.rs): for a column the INSERT leaves out, or gives as DEFAULT
        for (col, e) in self.cluster.default_rules(&t.id) {
            let pos = names.iter().position(|c| c.eq_ignore_ascii_case(&col));
            let needed = match pos {
                None => true,
                Some(p) => rows.iter().any(|r| matches!(r[p], Expr::Default)),
            };
            if !needed || t.cols.iter().find(|c| c.name.eq_ignore_ascii_case(&col)).is_none() {
                continue;
            }
            let b = self.bind_expr(&e)?;
            let v = self.eval_scalar(&b).await?;
            match pos {
                None => {
                    for r in rows.iter_mut() {
                        r.push(nested::lit(&v));
                    }
                    names.push(col);
                }
                Some(p) => {
                    for r in rows.iter_mut() {
                        if matches!(r[p], Expr::Default) {
                            r[p] = nested::lit(&v);
                        }
                    }
                }
            }
        }
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
        // a graph table: the control node numbers the rows (`$graph_id`), the same numbers whichever shard a row goes to
        if t.graph.is_some() {
            let explicit = names.iter().any(|c| c.eq_ignore_ascii_case("$graph_id"));
            if explicit && !self.identity_insert.contains(&t.id) {
                return Err(SqlError::new(544, format!("Cannot insert explicit value for identity column in table '{}' when IDENTITY_INSERT is set to OFF.", t.name.last().cloned().unwrap_or_default())));
            }
            if !explicit {
                if let Some(first) = self.cluster.take_graph_ids(&t.id, rows.len()) {
                    for (k, r) in rows.iter_mut().enumerate() {
                        r.push(Expr::Lit(Value::BigInt(first + k as i64)));
                    }
                    names.push("$graph_id".to_string());
                }
            }
        }
        if !self.cluster.fks_of_child(&t.id).is_empty() {
            self.fk_check_insert(&t, &names, &rows).await?;
        }
        // (a unique key without the shard key is checked across the shards: see uniq.rs)
        if !t.weak_uniques.is_empty() {
            self.check_unique_insert(&t, &names, &rows).await?;
        }
        let cols_for_shards: Option<Vec<String>> = if ident_col.is_some() || given.is_some() || t.graph.is_some() { Some(names.clone()) } else { None };
        let make = |rows: Vec<Vec<Expr>>| Stmt::Insert(Box::new(Insert { ctes: vec![], target: ins.target.clone(), cols: cols_for_shards.clone(), output: None, src: InsertSrc::Values(rows), top: None }));
        let count: i64;
        match &t.dist {
            Dist::Replicated => {
                let shards = self.all_shards();
                let st = make(rows);
                if self.capturing(&t.id) {
                    let with_rows = triggers::with_output(st.clone());
                    let results = self.fan_query(&shards, |_| Work::Stmts(vec![with_rows.clone()])).await?;
                    count = results.first().map(|r| r.rows.len() as i64).unwrap_or(0);
                    self.capture_rows(results, true);
                } else {
                    let replies = self.fan_run(&shards, |_| Work::Stmts(vec![st.clone()])).await?;
                    count = replies.first().map(|r| r.row_count()).unwrap_or(0);
                }
                self.plan.push(format!("insert into replicated table on {} shards", n));
            }
            Dist::Sharded { key, class } => {
                let key_pos = names.iter().position(|c| c.eq_ignore_ascii_case(&t.cols[*key].name)).ok_or_else(|| SqlError::new(50102, format!("The shard key column {} must be given a value.", t.cols[*key].name)))?;
                let mut per_shard: Vec<Vec<Vec<Expr>>> = vec![vec![]; n];
                // (the entries of the global indexes: per index, the (value, key) pairs)
                let slots: Vec<Option<usize>> = t.indexes.iter().map(|ix| names.iter().position(|c| c.eq_ignore_ascii_case(&t.cols[ix.col].name))).collect();
                let mut entries: Vec<Vec<(Value, Value)>> = vec![vec![]; t.indexes.len()];
                for r in rows {
                    let v = self.eval_scalar(&r[key_pos]).await?;
                    let h = key_hash(&v, *class).ok_or_else(|| SqlError::new(245, format!("The value {} cannot be used as the shard key {}.", tsql_server::convert::display(&v), t.cols[*key].name)))?;
                    for (i, slot) in slots.iter().enumerate() {
                        if let Some(p) = slot {
                            let gv = self.eval_scalar(&r[*p]).await?;
                            // (a NULL is never found by an equality, so it needs no entry)
                            if !gv.is_null() {
                                entries[i].push((gv, v.clone()));
                            }
                        }
                    }
                    per_shard[self.cluster.shard_of_hash(h)].push(r);
                }
                let used: Vec<usize> = (0..n).filter(|i| !per_shard[*i].is_empty()).collect();
                let stmts: Vec<Stmt> = per_shard.into_iter().map(|rows| make(rows)).collect();
                if self.capturing(&t.id) {
                    let with_rows: Vec<Stmt> = stmts.iter().cloned().map(triggers::with_output).collect();
                    let results = self.fan_query(&used, |i| Work::Stmts(vec![with_rows[i].clone()])).await?;
                    count = results.iter().map(|r| r.rows.len() as i64).sum();
                    self.capture_rows(results, false);
                } else {
                    let replies = self.fan_run(&used, |i| Work::Stmts(vec![stmts[i].clone()])).await?;
                    count = replies.iter().map(|r| r.row_count()).sum();
                }
                self.plan.push(format!("insert split over shards {:?}", used));
                for (i, e) in entries.into_iter().enumerate() {
                    if !e.is_empty() {
                        self.index_insert(&t.indexes[i], e).await?;
                    }
                }
            }
        }
        self.bump(&t.id);
        events.push(Event::RowCount(count));
        Ok(())
    }

    /// Add entries `(value, key)` to a global index: each goes to the shard that owns the hash of its value.
    async fn index_insert(&mut self, ix: &GlobalIndex, mut entries: Vec<(Value, Value)>) -> Result<(), SqlError> {
        let n = self.cluster.shard_count();
        entries.sort_by(|a, b| format!("{:?}{:?}", a.0, a.1).cmp(&format!("{:?}{:?}", b.0, b.1)));
        entries.dedup_by(|a, b| format!("{:?}{:?}", a.0, a.1) == format!("{:?}{:?}", b.0, b.1));
        let mut per_shard: Vec<Vec<Vec<Expr>>> = vec![vec![]; n];
        for (gv, gk) in entries {
            let h = key_hash(&gv, ix.class).ok_or_else(|| SqlError::new(245, format!("The value {} cannot be used in a global index.", tsql_server::convert::display(&gv))))?;
            per_shard[self.cluster.shard_of_hash(h)].push(vec![Expr::Lit(gv), Expr::Lit(gk)]);
        }
        let used: Vec<usize> = (0..n).filter(|i| !per_shard[*i].is_empty()).collect();
        let stmts: Vec<Stmt> = per_shard.into_iter().map(|rows| Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(ix.table.clone()), cols: Some(vec!["gv".into(), "gk".into()]), output: None, src: InsertSrc::Values(rows), top: None }))).collect();
        self.fan_run(&used, |i| Work::Stmts(vec![stmts[i].clone()])).await?;
        Ok(())
    }

    /// The shards that can hold rows matching `where_`: those the shard key fixes, else (when a global index of the table has its
    /// column fixed by `col = literal` or `col IN (literals)`) the shards that own the keys the index lists for those values, else all.
    pub(crate) async fn pruned_shards(&mut self, t: &DistTable, where_: Option<&Expr>) -> Result<Vec<usize>, SqlError> {
        let by_key = prune_shards(where_, t, &self.cluster);
        if by_key.len() < self.cluster.shard_count() || t.indexes.is_empty() {
            return Ok(by_key);
        }
        let Some(w) = where_ else { return Ok(by_key) };
        let Dist::Sharded { class: key_class, .. } = &t.dist else { return Ok(by_key) };
        let mut cs = vec![];
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
        conjuncts(w, &mut cs);
        for ix in &t.indexes {
            let cname = &t.cols[ix.col].name;
            let is_col = |e: &Expr| matches!(e, Expr::Col(p) if p.last().map(|s| s.eq_ignore_ascii_case(cname)).unwrap_or(false));
            let mut values: Option<Vec<Value>> = None;
            for c in &cs {
                match c {
                    Expr::Binary(BinOp::Eq, a, b) => match (&**a, &**b) {
                        (x, Expr::Lit(v)) | (Expr::Lit(v), x) if is_col(x) && !v.is_null() => values = Some(vec![v.clone()]),
                        _ => {}
                    },
                    Expr::InList(x, list, false) if is_col(x) && list.iter().all(|i| matches!(i, Expr::Lit(v) if !v.is_null())) => {
                        values = Some(list.iter().filter_map(|i| if let Expr::Lit(v) = i { Some(v.clone()) } else { None }).collect());
                    }
                    _ => {}
                }
                if values.is_some() {
                    break;
                }
            }
            let Some(values) = values else { continue };
            // the keys the index lists for these values (one shard of the index table when there is one value)
            let cond = if values.len() == 1 {
                Expr::Binary(BinOp::Eq, Box::new(col("gv")), Box::new(Expr::Lit(values[0].clone())))
            } else {
                Expr::InList(Box::new(col("gv")), values.iter().cloned().map(Expr::Lit).collect(), false)
            };
            let q = simple_query(Select { distinct: true, top: None, items: vec![SelectItem::Expr { e: col("gk"), alias: None }], into: None, from: vec![TableRef::Table { name: ix.table.clone(), alias: None, hints: vec![] }], where_: Some(cond), group: None, having: None });
            let rs = Box::pin(self.run_query(&q)).await?;
            let mut shards: Vec<usize> = vec![];
            for r in &rs.rows {
                match key_hash(&r[0], *key_class) {
                    Some(h) => shards.push(self.cluster.shard_of_hash(h)),
                    None => return Ok(by_key),
                }
            }
            shards.sort();
            shards.dedup();
            if shards.is_empty() {
                // (nothing has the value: any shard gives the empty answer with the right columns)
                shards.push(0);
            }
            self.plan.push(format!("global index on {}: shards {:?}", cname, shards));
            return Ok(shards);
        }
        Ok(by_key)
    }

    /// The common checks of a single-table UPDATE or DELETE; the shards to send it to.
    async fn dml_shards(&mut self, t: &DistTable, where_: Option<&Expr>) -> Result<Vec<usize>, SqlError> {
        tsql_server::span!("ctl_dml_shards");
        match t.dist {
            Dist::Replicated => Ok(self.all_shards()),
            Dist::Sharded { .. } => self.pruned_shards(t, where_).await,
        }
    }

    async fn run_dml(&mut self, t: &DistTable, st: Stmt, shards: Vec<usize>, events: &mut Vec<Event>) -> Result<(), SqlError> {
        tsql_server::span!("ctl_run_dml");
        if self.capturing(&t.id) {
            // (the rows the shards changed come back, for the triggers of the table)
            let with_rows = triggers::with_output(st.clone());
            let results = self.fan_query(&shards, |_| Work::Stmts(vec![with_rows.clone()])).await?;
            let replicated = matches!(t.dist, Dist::Replicated);
            let count: i64 = if replicated { results.first().map(|r| r.rows.len() as i64).unwrap_or(0) } else { results.iter().map(|r| r.rows.len() as i64).sum() };
            self.capture_rows(results, replicated);
            self.plan.push(format!("{} on shards {:?}, with its rows kept for the triggers", stmt_kind(&st).to_lowercase(), shards));
            self.bump(&t.id);
            events.push(Event::RowCount(count));
            return Ok(());
        }
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
        tsql_server::span!("ctl_update");
        let Target::Table(name) = &u.target else { return Err(unsupported("UPDATE of a table variable")) };
        // (an UPDATE that joins another table: the control node reads the join and applies it by key, see updfrom.rs)
        if !u.from.is_empty() && u.output.is_none() && u.top.is_none() && u.current_of.is_none() && u.ctes.is_empty() {
            return self.update_from(u, events).await;
        }
        let t = self.table(name)?;
        if !u.from.is_empty() || u.output.is_some() || u.top.is_some() || u.current_of.is_some() || !u.ctes.is_empty() {
            return Err(unsupported("UPDATE with FROM, OUTPUT, TOP, CURRENT OF or CTEs"));
        }
        // (the calls of functions that read sharded tables are evaluated here, and the shards get their values: see dmlfuncs.rs)
        if let Some(u2) = self.fold_update(u).await? {
            return Box::pin(self.update(&u2, events)).await;
        }
        if self.triggers_apply(&t.id, triggers::Ev::Update) {
            return self.dml_with_triggers(&t, triggers::Ev::Update, Stmt::Update(Box::new(u.clone())), events).await;
        }
        let mut exprs: Vec<&Expr> = u.where_.iter().collect();
        let mut moves_key = false;
        for s in &u.sets {
            match s {
                SetItem::Col { parts, e, .. } => {
                    let col = parts.last().cloned().unwrap_or_default();
                    if t.graph.is_some() && col.eq_ignore_ascii_case("$graph_id") {
                        return Err(SqlError::new(8102, format!("Cannot update identity column '{}'.", col)));
                    }
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
                    // (the entry for the new value would be missing, and a lookup by it would not find the row)
                    if t.indexes.iter().any(|ix| t.cols[ix.col].name.eq_ignore_ascii_case(&col)) {
                        return Err(unsupported(&format!("UPDATE of the globally indexed column {} (delete and insert the row instead)", col)));
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
                // (a subquery that depends on the row: the control node reads the rows and their new values as a query, which the cluster can run, and applies
                // them by key: see updfrom.rs)
                _ => self.update_from(u, events).await,
            };
        }
        self.refuse_reading_functions(u, "an UPDATE")?;
        self.fk_check_update(&t, u).await?;
        if !moves_key && !t.weak_uniques.is_empty() {
            self.check_unique_update(&t, u).await?;
        }
        if moves_key {
            return self.update_moving_key(&t, u, events).await;
        }
        let shards = self.dml_shards(&t, u.where_.as_ref()).await?;
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
        let shards = self.dml_shards(t, u.where_.as_ref()).await?;
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
        tsql_server::span!("ctl_delete");
        let Target::Table(name) = &d.target else { return Err(unsupported("DELETE from a table variable")) };
        // (a DELETE that joins another table: see updfrom.rs)
        if !d.from.is_empty() && d.output.is_none() && d.top.is_none() && d.current_of.is_none() && d.ctes.is_empty() {
            return self.delete_from(d, events).await;
        }
        let t = self.table(name)?;
        if !d.from.is_empty() || d.output.is_some() || d.top.is_some() || d.current_of.is_some() || !d.ctes.is_empty() {
            return Err(unsupported("DELETE with FROM, OUTPUT, TOP, CURRENT OF or CTEs"));
        }
        if let Some(d2) = self.fold_delete(d).await? {
            return Box::pin(self.delete(&d2, events)).await;
        }
        if self.triggers_apply(&t.id, triggers::Ev::Delete) {
            return self.dml_with_triggers(&t, triggers::Ev::Delete, Stmt::Delete(Box::new(d.clone())), events).await;
        }
        if let Some(w) = &d.where_ {
            if has_window_or_session(w) {
                return Err(unsupported("variables in DELETE"));
            }
            if has_subquery(w) {
                return match self.resolve_stmt(&Stmt::Delete(Box::new(d.clone()))).await? {
                    Some(Stmt::Delete(d2)) if !d2.where_.as_ref().map(has_subquery).unwrap_or(false) => Box::pin(self.delete(&d2, events)).await,
                    _ => self.delete_from(d, events).await,
                };
            }
        }
        self.refuse_reading_functions(d, "a DELETE")?;
        self.fk_before_delete(&t, d.where_.as_ref(), events).await?;
        self.edges_before_node_delete(&t, d.where_.as_ref(), events).await?;
        let shards = self.dml_shards(&t, d.where_.as_ref()).await?;
        self.run_dml(&t, Stmt::Delete(Box::new(d.clone())), shards, events).await
    }

    /// Deleting nodes: the edges at them, in the edge tables whose `CONNECTION` constraints name the node's table, are deleted too with
    /// `ON DELETE CASCADE`, and stop the delete (error 547) without it, as in the engine (see graph.rs there). Each shard checks only
    /// the edges it holds, so the control node does it for all of them. (The cascade is a statement of its own, before the delete of the
    /// nodes: inside a transaction they are one.)
    async fn edges_before_node_delete(&mut self, t: &DistTable, where_: Option<&Expr>, events: &mut Vec<Event>) -> Result<(), SqlError> {
        if !matches!(&t.graph, Some(g) if !g.edge) {
            return Ok(());
        }
        let edge_tables: Vec<(DistTable, bool)> = {
            let all = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
            all.values()
                .filter_map(|e| {
                    let mine: Vec<&EdgeCons> = e.edge_cons.iter().filter(|c| c.pairs.iter().any(|(f, to)| *f == t.id || *to == t.id)).collect();
                    (!mine.is_empty()).then(|| (e.clone(), mine.iter().any(|c| c.cascade)))
                })
                .collect()
        };
        if edge_tables.is_empty() {
            return Ok(());
        }
        let ids_q = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: col("$node_id"), alias: None }], into: None, from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }], where_: where_.cloned(), group: None, having: None });
        let ids: Vec<Expr> = self.run_query(&ids_q).await?.rows.into_iter().filter_map(|r| r.into_iter().next()).filter(|v| !v.is_null()).map(Expr::Lit).collect();
        if ids.is_empty() {
            return Ok(());
        }
        // (what stops the delete first, so that nothing has been cascaded when it does)
        let at = |c: &str| Expr::InList(Box::new(col(c)), ids.clone(), false);
        let cond = || Expr::Or(Box::new(at("$from_id")), Box::new(at("$to_id")));
        for (e, _) in edge_tables.iter().filter(|(_, cascade)| !cascade) {
            let found_q = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: col("$graph_id"), alias: None }], into: None, from: vec![TableRef::Table { name: e.name.clone(), alias: None, hints: vec![] }], where_: Some(cond()), group: None, having: None });
            if !self.run_query(&found_q).await?.rows.is_empty() {
                return Err(SqlError::new(547, format!("The DELETE statement conflicted with the EDGE REFERENCE constraint \"{}\". The conflict occurred in table \"{}\".", e.edge_cons[0].name, e.name.join("."))));
            }
        }
        for (e, _) in edge_tables.iter().filter(|(_, cascade)| *cascade) {
            let del = Delete { ctes: vec![], target: Target::Table(e.name.clone()), from: vec![], where_: Some(cond()), output: None, top: None, current_of: None };
            let mut quiet = vec![];
            Box::pin(self.delete(&del, &mut quiet)).await?;
            self.plan.push(format!("ON DELETE CASCADE: the edges at the deleted nodes deleted from {}", e.id));
        }
        let _ = events;
        Ok(())
    }

    // ------------------------------------------------------------------ queries

    async fn run_query_flat(&mut self, q: &Query) -> Result<ResultSet, SqlError> {
        tsql_server::span!("ctl_run_query_flat");
        let ids = referenced_tables(q);
        let tables: Vec<DistTable> = ids.iter().filter_map(|i| self.table_by_id(i)).collect();
        if tables.is_empty() {
            // (a function it calls may read a replicated table, which every shard holds and the control node's own copy does not)
            if self.calls_function_reading_tables(q) {
                self.plan.push("direct: it calls functions that read replicated tables, on shard 0".into());
                return self.direct(0, q).await;
            }
            self.plan.push("local: reads no distributed table".into());
            return self.mirror_query(q);
        }
        // a shortest path is worked out over the whole graph (see graph.rs in the engine): the tables are copied to the control node's engine
        if serde_json::to_string(q).map(|s| s.contains("__SHORTEST_PATH")).unwrap_or(false) {
            self.plan.push("gather: a shortest path is worked out over the whole graph, on the control node".into());
            return self.gather_whole(q, &tables).await;
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
            // (the rest of the tables broadcast to the shards that hold the co-located ones, TPC-H Q21)
            if let Some(rs) = self.try_join(q, &tables, true).await? {
                return Ok(rs);
            }
        }
        // (one table read once: a table joined with itself is two row sources, and a condition on the key of one says nothing
        // about the other)
        if tables.len() == 1 && !has_nested_query(q) && matches!(&q.body, QueryBody::Select(s) if matches!(s.from.as_slice(), [TableRef::Table { .. }])) {
            let t = &tables[0];
            let shards = self.pruned_shards(t, query_where(q)).await?;
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
            if let Some(rs) = self.try_join(q, &tables, false).await? {
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
    /// `nested`: the query has subqueries in its `WHERE`; the tables moved are then broadcast (never repartitioned), and the query goes
    /// on only when the shards can evaluate the subqueries against what they hold once those copies are in place (the tables that
    /// stay where they are are co-located with each other, and each subquery is co-located with them or reads replicated tables).
    async fn try_join(&mut self, q: &Query, tables: &[DistTable], nested: bool) -> Result<Option<ResultSet>, SqlError> {
        tsql_server::span!("ctl_try_join");
        let map: HashMap<String, DistTable> = tables.iter().map(|t| (t.id.clone(), t.clone())).collect();
        let limit = self.cluster.broadcast_limit.load(Ordering::Relaxed);
        let n = self.cluster.shard_count();
        let sid = self.id;
        let mut counter = self.temp_counter;
        // (the row counts only decide between moving tables; a join that is co-located needs none: planned first with every table
        // taken as too big to move, and only when that moves something are the tables counted, which costs a round trip to every shard
        // each time a table has changed)
        let unknown: HashMap<String, u64> = tables.iter().filter(|t| !matches!(t.dist, Dist::Replicated)).map(|t| (t.id.clone(), u64::MAX / 4)).collect();
        let first = joins::plan_join(q, &map, &unknown, limit, n, nested, &mut || {
            counter += 1;
            format!("#dpj_{}_{}", sid, counter)
        });
        let plan = match first {
            Some(jp) if jp.preps.is_empty() => Some(jp),
            _ => {
                let mut rows = HashMap::new();
                for t in tables {
                    if !matches!(t.dist, Dist::Replicated) {
                        let n = self.table_rows(t).await?;
                        rows.insert(t.id.clone(), n);
                    }
                }
                joins::plan_join(q, &map, &rows, limit, n, nested, &mut || {
                    counter += 1;
                    format!("#dpj_{}_{}", sid, counter)
                })
            }
        };
        self.temp_counter = counter;
        let Some(mut jp) = plan else { return Ok(None) };
        let shape = if nested {
            let replicated_copies = |preps: &[joins::PrepSpec]| -> HashMap<String, DistTable> {
                preps
                    .iter()
                    .map(|p| {
                        let mut t = p.table.clone();
                        t.dist = Dist::Replicated;
                        t.id = table_id(std::slice::from_ref(&p.temp));
                        (t.id.clone(), t)
                    })
                    .collect()
            };
            let mut copies = replicated_copies(&jp.preps);
            let mut ok = {
                let this = &*self;
                let lookup = |id: &str| copies.get(id).cloned().or_else(|| this.table_by_id(id));
                pushdown::pushdown_ok(&jp.query, &lookup, false)
            };
            if !ok {
                // a subquery over a table that is sharded on another key: when that table is small it is copied to every shard too
                let mut sizes: HashMap<String, u64> = HashMap::new();
                for t in tables.iter().filter(|t| matches!(t.dist, Dist::Sharded { .. })) {
                    sizes.insert(t.id.clone(), self.table_rows(t).await?);
                }
                let mut counter = self.temp_counter;
                let extra = {
                    let this = &*self;
                    let lookup = |id: &str| copies.get(id).cloned().or_else(|| this.table_by_id(id));
                    pushdown::with_small_tables_copied(&jp.query, &lookup, &|id| sizes.get(id).copied(), limit, &mut || {
                        counter += 1;
                        format!("#dpj_{}_{}", sid, counter)
                    })
                };
                self.temp_counter = counter;
                if let Some((q2, more)) = extra {
                    jp.query = q2;
                    jp.preps.extend(more);
                    jp.description = format!("{}", jp.preps.iter().map(|p| format!("{} {}", if p.repartition.is_some() { "repartition" } else { "broadcast" }, p.table.id)).collect::<Vec<_>>().join(", "));
                    copies = replicated_copies(&jp.preps);
                    let this = &*self;
                    let lookup = |id: &str| copies.get(id).cloned().or_else(|| this.table_by_id(id));
                    ok = pushdown::pushdown_ok(&jp.query, &lookup, false);
                }
            }
            if !ok {
                return Ok(None);
            }
            plan::plan_scatter_sub(&jp.query)
        } else {
            plan::plan_scatter_multi(&jp.query)
        };
        let Some(shape) = shape else { return Ok(None) };
        let mut shards = self.all_shards();
        // (a condition `col = literal` on a column with a global index limits a co-located join to the shards that hold such rows: the
        // other sharded tables' partners of a row are on its shard; a column that another table of the query has too is not used)
        if jp.preps.is_empty() {
            for t in tables {
                let ambiguous = t.indexes.iter().any(|ix| tables.iter().any(|o| o.id != t.id && o.cols.iter().any(|c| c.name.eq_ignore_ascii_case(&t.cols[ix.col].name))));
                if t.indexes.is_empty() || ambiguous {
                    continue;
                }
                let found = self.pruned_shards(t, query_where(q)).await?;
                if found.len() < shards.len() {
                    shards = found;
                    break;
                }
            }
        }
        let mut made = vec![];
        let mut result = Ok(());
        for p in &jp.preps {
            let rows = self.table_by_id(&p.table.id).and_then(|t| t.rows.map(|r| r.1)).unwrap_or(0);
            let col: Vec<String> = p.repartition.as_ref().map(|(c, _)| p.table.cols[*c].name.clone()).into_iter().collect();
            self.cluster.advisor.record(&p.table.id, if p.repartition.is_some() { "repartition" } else { "broadcast" }, rows, &col);
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
        tsql_server::span!("ctl_try_mpp");
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
            items: p.table.copy_items(),
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
            items: p.table.copy_items(),
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
        // a subquery that looks the copy up for each row of the outer query seeks it by an index instead of reading it each time
        if !p.index_on.is_empty() {
            let keys: Vec<(String, bool)> = p.index_on.iter().map(|c| (c.clone(), false)).collect();
            let ix = Stmt::CreateIndex { name: format!("ix_{}", p.temp.trim_start_matches('#')), table: vec![p.temp.clone()], cols: keys, unique: false, include: vec![], filter: None, xml: None, spatial: None, columnstore: None };
            self.fan_run(&shards, |_| Work::Stmts(vec![ix.clone()])).await?;
        }
        Ok(())
    }

    fn mirror_query(&mut self, q: &Query) -> Result<ResultSet, SqlError> {
        let res = self.mirror.execute_stmts(&[Stmt::Query(Box::new(q.clone()))]);
        merge::check(&res)?;
        first_rows(res)
    }

    async fn direct(&mut self, shard: usize, q: &Query) -> Result<ResultSet, SqlError> {
        tsql_server::span!("ctl_direct");
        let work = self.prepared(shard, Work::Stmts(vec![Stmt::Query(Box::new(q.clone()))])).await?;
        self.cluster.shard(shard).query(self.id, work).await
    }

    async fn scatter(&mut self, shape: Shape, shards: &[usize]) -> Result<ResultSet, SqlError> {
        tsql_server::span!("ctl_scatter");
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
        // (a plain ordering and TOP is merged here; the rest, and whatever might order differently from the engine, goes through it)
        let rows = match &shape {
            Shape::Row(p) => match p.merge_rows(&columns, rows) {
                Ok(done) => {
                    let n = done.first().map(|r| r.len()).unwrap_or(columns.len().min(types.len()));
                    let nvisible = if done.is_empty() { p.visible_columns(columns.len()) } else { n };
                    let types = merge::widen(&types[..nvisible], &done);
                    return Ok(ResultSet { columns: columns[..nvisible].to_vec(), types, rows: done });
                }
                Err(rows) => rows,
            },
            Shape::Agg(_) => rows,
        };
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
        // (a temporary table of the session, such as `inserted` of a trigger, is made in the control node's engine when it is first needed there)
        if let Some(create) = self.temp_creates.get(&t.id).cloned() {
            if !self.mirror_temps.contains(&t.id) {
                let stmts = tsql_server::parser::parse(&create)?;
                merge::check(&self.mirror.execute_stmts(&stmts))?;
                self.mirror_temps.insert(t.id.clone());
            }
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
        // (the mirror's table has the IDENTITY the control node numbers by, and the copy carries the numbers the rows were given)
        let identity = t.identity.is_some();
        if identity {
            merge::check(&self.mirror.execute_stmts(&[Stmt::SetOpts(vec![SetOpt::IdentityInsert(t.name.clone(), true)])]))?;
        }
        let loaded: Result<(), SqlError> = (|| {
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
            Ok(())
        })();
        if identity {
            let _ = self.mirror.execute_stmts(&[Stmt::SetOpts(vec![SetOpt::IdentityInsert(t.name.clone(), false)])]);
        }
        loaded?;
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
            // the temporary tables of the session (made in the cluster by a statement that needed them there) go with it
            if !self.own_temps.is_empty() {
                let mut s = cl.session();
                for t in std::mem::take(&mut self.own_temps) {
                    let _ = s.execute(&format!("DROP TABLE IF EXISTS [{}]", t));
                }
            }
        }
    }
}
