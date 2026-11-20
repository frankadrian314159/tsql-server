//! Elasticity: the control node's persistent state, adding and removing shards, moving data between them, and the
//! telemetry and advice that say when to.
//!
//! A key belongs to one of [`SLOTS`] hash slots and each slot is owned by one shard (the *slot map*). Growing or shrinking
//! the cluster moves slots: for each sharded table the rows of the moving slots are copied to the new owner, the slot map
//! is switched, and the old copies are deleted. Statements hold a read lock (`Cluster::migration`) while they run and a
//! move holds it for writing, so no statement sees a half-moved table; a move is therefore a pause, not an outage of
//! queries elsewhere in the process. The orchestrator that starts and stops machines is outside this crate: it calls
//! [`Cluster::add_shard`] once a shard process is up, and [`Cluster::remove_last_shard`] before it stops one, guided by
//! [`advise`].
use crate::astutil::*;
use crate::client::ShardClient;
use crate::{Cluster, ClusterSession, Dist, DistTable};
use futures::future::join_all;
pub use tsql_shared_ces::metrics::NodeMetrics;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};
use tsql_server::ast::*;
use tsql_server::{Server, SqlError, Value};
use tsql_shared_ces::route::{key_hash, slot_of, SLOTS};
use tsql_shared_ces::wire::Work;
use tsql_shared_ces::ClusterConfig;

#[derive(Serialize, Deserialize)]
struct State {
    pending: Vec<(u64, Vec<usize>)>,
    shard_addrs: Vec<String>,
    slots: Vec<u32>,
    tables: Vec<DistTable>,
    schema_log: Vec<Stmt>,
    views: Vec<(String, Stmt)>,
    fks: Vec<crate::fk::FkDef>,
    proc_routes: Vec<(String, crate::procs::ProcRoute)>,
}

impl Cluster {
    /// Open a cluster whose control state (shards, slot map, tables, schema) is kept in `state_path`: if the file exists
    /// the cluster is restored from it (and `config` is not consulted), else it is created from `config` and the file is
    /// written. The control node can then be restarted without losing track of the data on the shards.
    pub fn open(state_path: impl Into<PathBuf>, config: &ClusterConfig) -> Result<Arc<Cluster>, SqlError> {
        Cluster::open_inner(state_path.into(), config, false)
    }

    /// Another front end for a cluster that is running: a control node that reads the catalogue (shards, slot map, tables) from the
    /// state file another control node keeps, and serves sessions of its own over the same shards. It does not write the state
    /// file, and it does not settle the prepared transactions the shards hold (they may be live ones of the other front ends).
    /// Use it for more clients (several processes, or machines, each with a control node of its own, so that no one control
    /// node's single engine and planner is the limit); schema changes, elasticity and the identity counters stay with the
    /// control node that owns the state. Deadlocks across front ends are found by each one's detector (it sees all waits on the
    /// shards) or end at the lock timeout.
    pub fn attach(state_path: impl Into<PathBuf>) -> Result<Arc<Cluster>, SqlError> {
        Cluster::open_inner(state_path.into(), &ClusterConfig::default(), true)
    }

    fn open_inner(path: PathBuf, config: &ClusterConfig, attach: bool) -> Result<Arc<Cluster>, SqlError> {
        let (addrs, slots, tables, log, pending, views, fks, routes) = match std::fs::read(&path) {
            Ok(bytes) => {
                let st: State = postcard::from_bytes(&bytes).map_err(|e| SqlError::new(50000, format!("{}: bad cluster state: {}", path.display(), e)))?;
                (st.shard_addrs, Some(st.slots), st.tables, st.schema_log, st.pending, st.views, st.fks, st.proc_routes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !attach => {
                config.validate().map_err(|e| SqlError::new(50000, e))?;
                (config.shards.iter().map(|s| s.addr.clone()).collect(), None, vec![], vec![], vec![], vec![], vec![], vec![])
            }
            Err(e) => return Err(SqlError::new(50000, format!("{}: {}", path.display(), e))),
        };
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 8)).enable_all().build().map_err(|e| SqlError::new(50000, e.to_string()))?;
        // (transactions decided by the control node that owns the state are its to finish)
        let pending = Arc::new(Mutex::new(if attach { vec![] } else { pending }));
        let preparing = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let verdict = crate::make_verdict(&pending, &preparing);
        let shards = {
            let _g = rt.enter();
            addrs.iter().enumerate().map(|(i, a)| ShardClient::new(i as u32, a).map(|c| c.with_verdict(verdict.clone()))).collect::<Result<Vec<_>, _>>()?
        };
        let n = shards.len();
        let slots = slots.unwrap_or_else(|| (0..SLOTS).map(|i| (i % n) as u32).collect());
        let (dbs, logged_db) = crate::dbname::databases_of_log(&log);
        let funcs_init = crate::funcs::functions_of_log(&log);
        let triggers_init = crate::triggers::triggers_of_log(&log);
        let log_for_ddl = log.clone();
        let mirror = Server::new();
        {
            // the mirror learns the schema again
            let mut c = mirror.connect_internal();
            for st in &log {
                // (the triggers are the control node's to fire: the engine here would fire them on the copies it is loaded with)
                if crate::triggers::is_trigger_log_stmt(st) {
                    continue;
                }
                let _ = c.execute_stmts(std::slice::from_ref(st));
            }
        }
        let tables: HashMap<String, DistTable> = tables
            .into_iter()
            .map(|mut t| {
                t.loaded = None;
                t.rows = None;
                (t.id.clone(), t)
            })
            .collect();
        let cluster = Arc::new(Cluster {
            rt,
            shards: RwLock::new(shards),
            slots: RwLock::new(slots),
            schema_log: Mutex::new(log),
            state_path: if attach { None } else { Some(path) },
            migration: RwLock::new(()),
            uniq_locks: Mutex::new(HashMap::new()),
            mirror,
            tables: Mutex::new(tables),
            gather: Mutex::new(()),
            advisor: Default::default(),
            next_session: AtomicU64::new(crate::fresh_session_base()),
            broadcast_limit: AtomicU64::new(100_000),
            in_list_limit: AtomicU64::new(20_000),
            in_join_min: AtomicU64::new(100),
            lock_timeout_ms: AtomicU64::new(5000),
            views: Mutex::new(views.into_iter().collect()),
            fks: Mutex::new(fks),
            proc_routes: Mutex::new(routes.into_iter().collect()),
            pending,
            preparing,
            next_gid: AtomicU64::new(crate::fresh_session_base()),
            fault: Mutex::new(None),
            deadlock_check_ms: AtomicU64::new(200),
            victims: Mutex::new(Default::default()),
            deadlocks: AtomicU64::new(0),
            dbs: Mutex::new(dbs),
            logged_db: Mutex::new(logged_db),
            funcs: Mutex::new(funcs_init),
            funcs_on_shards: Default::default(),
            funcs_placed: Default::default(),
            triggers: Mutex::new(triggers_init.0),
            triggers_off: Mutex::new(triggers_init.1),
            ddl_triggers: Mutex::new(crate::ddltrig::ddl_triggers_of_log(&log_for_ddl)),
        });
        {
            // (the shards of a cluster that was running hold the functions that read no table)
            let pure: std::collections::BTreeSet<String> = cluster.pure_functions().into_iter().map(|(i, _)| i).collect();
            *cluster.funcs_on_shards.lock().unwrap_or_else(|e| e.into_inner()) = pure;
        }
        cluster.start_deadlock_monitor();
        if attach {
            return Ok(cluster);
        }
        cluster.save_state();
        // transactions that were decided to commit when the control node stopped are finished, and the ones that were not
        // decided are rolled back (a shard that prepared one holds it until it is told)
        cluster.resolve_pending();
        cluster.settle_prepared();
        Ok(cluster)
    }

    /// Record a successful CREATE / DROP of a table or index (so that a new shard can be given the schema) and save.
    pub(crate) fn log_schema(&self, st: &Stmt) {
        self.log_schema_in("master", st);
    }

    /// As [`Cluster::log_schema`] for a statement made in database `db`: the log says `USE db` first when the database is not the one
    /// of the statement before (a log is replayed on the control node's engine and on a new shard, statement by statement).
    pub(crate) fn log_schema_in(&self, db: &str, st: &Stmt) {
        {
            let mut log = self.schema_log.lock().unwrap_or_else(|e| e.into_inner());
            let mut last = self.logged_db.lock().unwrap_or_else(|e| e.into_inner());
            // (these two name their databases themselves)
            if !matches!(st, Stmt::CreateDatabase(..)) && !matches!(st, Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("DATABASE")) && !last.eq_ignore_ascii_case(db) {
                log.push(Stmt::Use(db.to_string()));
                *last = db.to_string();
            }
            log.push(st.clone());
            match st {
                Stmt::CreateDatabase(name, _, _) => self.dbs.lock().unwrap_or_else(|e| e.into_inner()).push(name.clone()),
                Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("DATABASE") => {
                    let mut dbs = self.dbs.lock().unwrap_or_else(|e| e.into_inner());
                    dbs.retain(|d| !names.iter().any(|n| n.last().is_some_and(|x| x.eq_ignore_ascii_case(d))));
                    // (the log goes back to `master` for what follows: a dropped database cannot be `USE`d)
                    if names.iter().any(|n| n.last().is_some_and(|x| x.eq_ignore_ascii_case(&last))) {
                        *last = "master".into();
                        log.push(Stmt::Use("master".into()));
                    }
                }
                _ => {}
            }
        }
        self.save_state();
    }

    /// Write the control state, if the cluster has a state file (atomically: a temporary file, then a rename).
    pub(crate) fn save_state(&self) {
        let Some(path) = &self.state_path else { return };
        let st = State {
            pending: self.pending.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            shard_addrs: self.shard_list().iter().map(|s| s.addr.clone()).collect(),
            slots: self.slots.read().unwrap_or_else(|e| e.into_inner()).clone(),
            tables: self.tables.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect(),
            schema_log: self.schema_log.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            views: self.views.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            fks: self.fks.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            proc_routes: self.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
        if let Ok(bytes) = postcard::to_allocvec(&st) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// Add a shard (a `tsql-shard` that is up and empty): it is given the schema and the replicated tables, and slots are
    /// moved to it until the shards own about the same number. Returns its id.
    pub fn add_shard(self: &Arc<Self>, addr: &str) -> Result<usize, SqlError> {
        let _w = self.migration.write().unwrap_or_else(|e| e.into_inner());
        let mut s = self.session();
        let id = self.rt.block_on(async {
            let id = s.cluster.shard_count();
            let client = ShardClient::new(id as u32, addr)?.with_verdict(s.cluster.verdict_fn());
            client.status().await?;
            s.cluster.shards.write().unwrap_or_else(|e| e.into_inner()).push(client);
            let r = async {
                let log = s.cluster.schema_log.lock().unwrap_or_else(|e| e.into_inner()).clone();
                for st in log {
                    if matches!(&st, Stmt::CreateView { .. }) || matches!(&st, Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("VIEW")) {
                        continue;
                    }
                    // (the functions are made below, the ones that read no table; the log may name ones that are not on the shards)
                    if matches!(&st, Stmt::CreateFunction(_)) || matches!(&st, Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("FUNCTION")) {
                        continue;
                    }
                    if crate::triggers::is_trigger_log_stmt(&st) {
                        continue;
                    }
                    s.fan_run(&[id], |_| Work::Stmts(vec![shard_form(&st)])).await?;
                }
                for (fid, fst) in s.cluster.pure_functions() {
                    let work = s.cluster.function_work(&fid, crate::funcs::or_alter_pub(&fst), "master");
                    s.fan_run(&[id], |_| work.clone()).await?;
                }
                // a replicated table is the same on every shard
                let tables: Vec<DistTable> = s.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).values().filter(|t| matches!(t.dist, Dist::Replicated)).cloned().collect();
                for t in tables {
                    s.copy_rows(&t, 0, id).await?;
                }
                s.rebalance_async().await?;
                Ok::<(), SqlError>(())
            }
            .await;
            if let Err(e) = r {
                // the new shard takes no part: forget it
                s.cluster.shards.write().unwrap_or_else(|e| e.into_inner()).pop();
                return Err(e);
            }
            Ok(id)
        })?;
        self.save_state();
        Ok(id)
    }

    /// Move everything the last shard owns to the others and forget it (the shard process can then be stopped).
    pub fn remove_last_shard(self: &Arc<Self>) -> Result<(), SqlError> {
        let _w = self.migration.write().unwrap_or_else(|e| e.into_inner());
        let mut s = self.session();
        self.rt.block_on(s.drain_last_async())?;
        self.save_state();
        Ok(())
    }

    /// Even out the slots over the shards (after a failed or partial change); returns how many slots moved.
    pub fn rebalance(self: &Arc<Self>) -> Result<usize, SqlError> {
        let _w = self.migration.write().unwrap_or_else(|e| e.into_inner());
        let mut s = self.session();
        let moved = self.rt.block_on(s.rebalance_async())?;
        self.save_state();
        Ok(moved)
    }

    /// What each shard reports, and how many rows it holds of the sharded tables.
    pub fn telemetry(self: &Arc<Self>) -> Vec<ShardTelemetry> {
        let mut s = self.session();
        let slots = self.slot_counts();
        self.rt.block_on(async {
            let shards = s.cluster.shard_list();
            let tables: Vec<DistTable> = s.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).values().filter(|t| matches!(t.dist, Dist::Sharded { .. })).cloned().collect();
            let mut out = vec![];
            for (i, sh) in shards.iter().enumerate() {
                let status = sh.status().await;
                let mut rows = 0u64;
                if status.is_ok() {
                    for t in &tables {
                        if let Ok(rs) = s.fan_query(&[i], |_| Work::Stmts(vec![count_query(&t.name)])).await {
                            rows += rs.first().and_then(|r| r.rows.first()).and_then(|r| r.first()).map(as_u64).unwrap_or(0);
                        }
                    }
                }
                out.push(match status {
                    Ok(st) => ShardTelemetry { id: i, reachable: true, slots: slots.get(i).copied().unwrap_or(0), sessions: st.sessions, lock_waits: st.lock_waits, log_forces: st.log_forces, rows, metrics: sh.metrics().await.ok() },
                    Err(_) => ShardTelemetry { id: i, reachable: false, slots: slots.get(i).copied().unwrap_or(0), sessions: 0, lock_waits: 0, log_forces: 0, rows: 0, metrics: None },
                });
            }
            out
        })
    }
}

fn as_u64(v: &Value) -> u64 {
    match v {
        Value::BigInt(n) => *n as u64,
        Value::Int(n) => *n as u64,
        _ => 0,
    }
}

fn count_query(name: &Name) -> Stmt {
    Stmt::Query(Box::new(simple_query(Select {
        distinct: false,
        top: None,
        items: vec![SelectItem::Expr { e: Expr::Func(Box::new(FuncCall { name: "COUNT_BIG".into(), schema: None, parts: vec![], args: vec![], distinct: false, star: true, over: None, within: None })), alias: None }],
        into: None,
        from: vec![TableRef::Table { name: name.clone(), alias: None, hints: vec![] }],
        where_: None,
        group: None,
        having: None,
    })))
}

impl ClusterSession {
    /// Copy every row of a table from one shard to another.
    pub(crate) async fn copy_rows(&mut self, t: &DistTable, from: usize, to: usize) -> Result<(), SqlError> {
        let scan = Stmt::Query(Box::new(simple_query(Select {
            distinct: false,
            top: None,
            items: t.copy_items(),
            into: None,
            from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }],
            where_: None,
            group: None,
            having: None,
        })));
        let rs = self.fan_query(&[from], |_| Work::Stmts(vec![scan.clone()])).await?;
        for chunk in rs[0].rows.chunks(500) {
            self.insert_rows(t, to, chunk).await?;
        }
        Ok(())
    }

    async fn insert_rows(&mut self, t: &DistTable, shard: usize, rows: &[Vec<Value>]) -> Result<(), SqlError> {
        // (a graph table's rows have columns that are generated: the others are given by name)
        let given = t.insertable();
        let values: Vec<Vec<Expr>> = rows
            .iter()
            .map(|r| match &given {
                Some(g) => g.iter().map(|(i, _)| Expr::Lit(r[*i].clone())).collect(),
                None => r.iter().cloned().map(Expr::Lit).collect(),
            })
            .collect();
        let cols = given.map(|g| g.into_iter().map(|(_, n)| n).collect::<Vec<String>>());
        let ins = Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(t.name.clone()), cols, output: None, src: InsertSrc::Values(values), top: None }));
        self.fan_run(&[shard], |_| Work::Stmts(vec![ins.clone()])).await?;
        Ok(())
    }

    /// Move the rows of the given slots of every sharded table from shard `from` to shard `to`, then give `to` the slots.
    async fn move_slots(&mut self, from: usize, to: usize, slots: &HashSet<usize>) -> Result<(), SqlError> {
        let tables: Vec<DistTable> = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).values().filter(|t| matches!(t.dist, Dist::Sharded { .. })).cloned().collect();
        let mut moved: Vec<(DistTable, Vec<Value>)> = vec![];
        let mut result = Ok(());
        for t in &tables {
            let Dist::Sharded { key, class } = &t.dist else { continue };
            let scan = Stmt::Query(Box::new(simple_query(Select {
                distinct: false,
                top: None,
                items: t.copy_items(),
                into: None,
                from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }],
                where_: None,
                group: None,
                having: None,
            })));
            let rs = match self.fan_query(&[from], |_| Work::Stmts(vec![scan.clone()])).await {
                Ok(r) => r,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            let mine: Vec<Vec<Value>> = rs[0].rows.iter().filter(|r| slots.contains(&slot_of(key_hash(&r[*key], *class).unwrap_or(0)))).cloned().collect();
            let mut keys: Vec<Value> = vec![];
            let mut seen = HashSet::new();
            for r in &mine {
                if seen.insert(format!("{:?}", r[*key])) {
                    keys.push(r[*key].clone());
                }
            }
            for chunk in mine.chunks(500) {
                if let Err(e) = self.insert_rows(t, to, chunk).await {
                    result = Err(e);
                    break;
                }
            }
            moved.push((t.clone(), keys));
            if result.is_err() {
                break;
            }
        }
        if let Err(e) = result {
            // undo: take the copies back out of the target
            for (t, keys) in &moved {
                let _ = self.delete_keys(t, to, keys).await;
            }
            return Err(e);
        }
        {
            let mut m = self.cluster.slots.write().unwrap_or_else(|e| e.into_inner());
            for s in slots {
                m[*s] = to as u32;
            }
        }
        for (t, keys) in &moved {
            self.delete_keys(t, from, keys).await?;
        }
        Ok(())
    }

    async fn delete_keys(&mut self, t: &DistTable, shard: usize, keys: &[Value]) -> Result<(), SqlError> {
        let Dist::Sharded { key, .. } = &t.dist else { return Ok(()) };
        for chunk in keys.chunks(500) {
            let del = Stmt::Delete(Box::new(Delete {
                ctes: vec![],
                target: Target::Table(t.name.clone()),
                output: None,
                from: vec![],
                where_: Some(Expr::InList(Box::new(col(&t.cols[*key].name)), chunk.iter().cloned().map(Expr::Lit).collect(), false)),
                current_of: None,
                top: None,
            }));
            self.fan_run(&[shard], |_| Work::Stmts(vec![del.clone()])).await?;
        }
        Ok(())
    }

    /// Deal the slots out so that the shards own about the same number; returns how many moved.
    pub(crate) async fn rebalance_async(&mut self) -> Result<usize, SqlError> {
        let n = self.cluster.shard_count();
        let owner: Vec<u32> = self.cluster.slots.read().unwrap_or_else(|e| e.into_inner()).clone();
        let target: Vec<usize> = (0..n).map(|i| SLOTS / n + if i < SLOTS % n { 1 } else { 0 }).collect();
        let mut owned: Vec<Vec<usize>> = vec![vec![]; n];
        for (slot, o) in owner.iter().enumerate() {
            owned[(*o as usize).min(n - 1)].push(slot);
        }
        let mut surplus: Vec<(usize, usize)> = vec![]; // (shard, slot)
        for (i, o) in owned.iter().enumerate() {
            for slot in o.iter().skip(target[i]) {
                surplus.push((i, *slot));
            }
        }
        let mut moves: HashMap<(usize, usize), HashSet<usize>> = HashMap::new();
        let mut it = surplus.into_iter();
        for (i, o) in owned.iter().enumerate() {
            for _ in o.len()..target[i] {
                if let Some((from, slot)) = it.next() {
                    moves.entry((from, i)).or_default().insert(slot);
                }
            }
        }
        let mut count = 0;
        let mut keys: Vec<(usize, usize)> = moves.keys().copied().collect();
        keys.sort();
        for k in keys {
            let slots = &moves[&k];
            count += slots.len();
            self.move_slots(k.0, k.1, slots).await?;
        }
        Ok(count)
    }

    pub(crate) async fn drain_last_async(&mut self) -> Result<(), SqlError> {
        let n = self.cluster.shard_count();
        if n < 2 {
            return Err(SqlError::new(50103, "The last shard cannot be removed from a cluster of one shard."));
        }
        let last = n - 1;
        let owner: Vec<u32> = self.cluster.slots.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut counts: Vec<usize> = vec![0; n - 1];
        for o in &owner {
            if (*o as usize) < n - 1 {
                counts[*o as usize] += 1;
            }
        }
        let mut moves: HashMap<usize, HashSet<usize>> = HashMap::new();
        for (slot, o) in owner.iter().enumerate() {
            if *o as usize == last {
                let to = (0..n - 1).min_by_key(|i| counts[*i]).unwrap_or(0);
                counts[to] += 1;
                moves.entry(to).or_default().insert(slot);
            }
        }
        let mut dests: Vec<usize> = moves.keys().copied().collect();
        dests.sort();
        for to in dests {
            self.move_slots(last, to, &moves[&to]).await?;
        }
        self.cluster.shards.write().unwrap_or_else(|e| e.into_inner()).pop();
        Ok(())
    }
}

/// What one shard reports.
#[derive(Clone, Debug, PartialEq)]
pub struct ShardTelemetry {
    pub id: usize,
    pub reachable: bool,
    /// hash slots it owns
    pub slots: usize,
    pub sessions: usize,
    pub lock_waits: u64,
    pub log_forces: u64,
    /// rows it holds of the sharded tables
    pub rows: u64,
    /// its resource use and engine counters (None: it did not answer, or is too old to know the action)
    pub metrics: Option<NodeMetrics>,
}

/// A resource that can be the limit of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Cpu,
    Memory,
    DiskIo,
    Network,
}

/// When to grow or shrink. The thresholds on resources are the autoscaling thresholds: the shards' averages over the whole
/// window the autoscaler watches (`sustain` readings) decide, never one reading.
#[derive(Clone, Debug)]
pub struct ScalePolicy {
    /// add a shard when the machines' processors are busier than this on average (percent)
    pub cpu_high_pct: f64,
    /// a shard may be removed when they are less busy than this (percent)
    pub cpu_low_pct: f64,
    /// add a shard when memory in use on the machines exceeds this (percent of the machine's memory)
    pub mem_high_pct: f64,
    /// add a shard when the processes read and write more than this per second on average (bytes; the default is about
    /// two thirds of what a 1 TB balanced persistent disk gives)
    pub disk_high_bps: f64,
    /// add a shard when the machines send and receive more than this per second on average (bytes)
    pub net_high_bps: f64,
    /// how many readings in a row must cross a threshold before the autoscaler acts
    pub sustain: usize,
    /// the least and the most shards the cluster may have
    pub min_shards: usize,
    pub max_shards: usize,
    /// seconds to wait after a change before the next one (moving data loads the machines, which must not read as demand)
    pub cooldown_secs: u64,
    /// add a shard when the shards hold more rows than this on average
    pub max_rows_per_shard: u64,
    /// remove one when they hold fewer than this on average (and there is more than one)
    pub min_rows_per_shard: u64,
    /// rebalance when the busiest shard owns more than this multiple of the average number of slots
    pub max_slot_imbalance: f64,
}

impl Default for ScalePolicy {
    fn default() -> Self {
        ScalePolicy {
            cpu_high_pct: 75.0,
            cpu_low_pct: 20.0,
            mem_high_pct: 85.0,
            disk_high_bps: 150e6,
            net_high_bps: 500e6,
            sustain: 6,
            min_shards: 1,
            max_shards: 64,
            cooldown_secs: 300,
            max_rows_per_shard: 50_000_000,
            min_rows_per_shard: 1_000_000,
            max_slot_imbalance: 1.1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScaleAdvice {
    AddShard { reason: String },
    RemoveShard { reason: String },
    Rebalance { reason: String },
    /// the shard does not answer: replace it (or restore it from its replica)
    Unreachable { shard: usize },
    /// a shard is above the limit of a resource (where the bottleneck is; adding shards helps when the average is above too)
    Bottleneck { shard: usize, resource: Resource, value: f64, limit: f64 },
}

/// The advice the telemetry and the policy give. Shrinking is advised only when everything is reachable and balanced
/// (a drain moves data and should not happen in an emergency).
pub fn advise(t: &[ShardTelemetry], p: &ScalePolicy) -> Vec<ScaleAdvice> {
    let mut out: Vec<ScaleAdvice> = t.iter().filter(|s| !s.reachable).map(|s| ScaleAdvice::Unreachable { shard: s.id }).collect();
    if !out.is_empty() || t.is_empty() {
        return out;
    }
    let n = t.len() as u64;
    let total: u64 = t.iter().map(|s| s.rows).sum();
    // resource use: the average over the shards decides whether to grow or shrink, and a single shard over a limit is a bottleneck
    let ms: Vec<(usize, &NodeMetrics)> = t.iter().filter_map(|s| s.metrics.as_ref().map(|m| (s.id, m))).collect();
    let avg = |f: &dyn Fn(&NodeMetrics) -> f64| if ms.is_empty() { 0.0 } else { ms.iter().map(|(_, m)| f(m)).sum::<f64>() / ms.len() as f64 };
    let limits: [(Resource, f64, Box<dyn Fn(&NodeMetrics) -> f64>); 4] = [
        (Resource::Cpu, p.cpu_high_pct, Box::new(|m| m.sample.cpu_machine_pct)),
        (Resource::Memory, p.mem_high_pct, Box::new(|m| m.sample.mem_used_pct())),
        (Resource::DiskIo, p.disk_high_bps, Box::new(|m| m.sample.disk_bps())),
        (Resource::Network, p.net_high_bps, Box::new(|m| m.sample.net_bps())),
    ];
    let mut hot: Vec<String> = vec![];
    for (res, limit, f) in &limits {
        for (id, m) in &ms {
            if f(m) > *limit {
                out.push(ScaleAdvice::Bottleneck { shard: *id, resource: *res, value: f(m), limit: *limit });
            }
        }
        if !ms.is_empty() && avg(f.as_ref()) > *limit {
            hot.push(format!("{:?} averages {:.1} over the shards (limit {:.1})", res, avg(f.as_ref()), limit));
        }
    }
    let avg_rows = total / n;
    let slots_avg = t.iter().map(|s| s.slots).sum::<usize>() as f64 / t.len() as f64;
    let slots_max = t.iter().map(|s| s.slots).max().unwrap_or(0) as f64;
    if slots_avg > 0.0 && slots_max / slots_avg > p.max_slot_imbalance {
        out.push(ScaleAdvice::Rebalance { reason: format!("the busiest shard owns {:.0} slots, {:.2} times the average", slots_max, slots_max / slots_avg) });
    }
    let growing = avg_rows > p.max_rows_per_shard || !hot.is_empty();
    if t.len() >= p.max_shards {
        // (the most shards: the bottlenecks above are all there is to say)
    } else if avg_rows > p.max_rows_per_shard {
        out.push(ScaleAdvice::AddShard { reason: format!("{} rows per shard on average (more than {})", avg_rows, p.max_rows_per_shard) });
    } else if !hot.is_empty() {
        out.push(ScaleAdvice::AddShard { reason: hot.join("; ") });
    }
    // shrinking: few rows, or idle processors, and what remains must stay under every limit with the load of the one removed
    let nf = t.len() as f64;
    let projected_ok = ms.is_empty() || limits.iter().all(|(_, limit, f)| avg(f.as_ref()) * nf / (nf - 1.0).max(1.0) < limit * 0.9) && avg_rows * n / (n - 1).max(1) <= p.max_rows_per_shard;
    let idle = !ms.is_empty() && avg(&|m| m.sample.cpu_machine_pct) < p.cpu_low_pct;
    if !growing && t.len() > p.min_shards.max(1) && !out.iter().any(|a| !matches!(a, ScaleAdvice::Bottleneck { .. })) && projected_ok && (avg_rows < p.min_rows_per_shard || idle) {
        let why = if avg_rows < p.min_rows_per_shard { format!("{} rows per shard on average (fewer than {})", avg_rows, p.min_rows_per_shard) } else { format!("the processors average {:.1}% (less than {}%)", avg(&|m| m.sample.cpu_machine_pct), p.cpu_low_pct) };
        out.push(ScaleAdvice::RemoveShard { reason: why });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: usize, slots: usize, rows: u64, up: bool) -> ShardTelemetry {
        ShardTelemetry { id, reachable: up, slots, sessions: 0, lock_waits: 0, log_forces: 0, rows, metrics: None }
    }

    fn busy(id: usize, cpu: f64, mem_used_pct: f64) -> ShardTelemetry {
        let mut m = NodeMetrics::default();
        m.sample.cpu_machine_pct = cpu;
        m.sample.mem_total_bytes = 1000;
        m.sample.mem_available_bytes = (1000.0 * (100.0 - mem_used_pct) / 100.0) as u64;
        ShardTelemetry { metrics: Some(m), ..t(id, 512, 100, true) }
    }

    #[test]
    fn advice_follows_the_policy() {
        let p = ScalePolicy { max_rows_per_shard: 1000, min_rows_per_shard: 100, max_slot_imbalance: 1.1, ..Default::default() };
        assert!(advise(&[t(0, 512, 500, true), t(1, 512, 500, true)], &p).is_empty());
        assert!(matches!(advise(&[t(0, 512, 5000, true), t(1, 512, 3000, true)], &p)[..], [ScaleAdvice::AddShard { .. }]));
        assert!(matches!(advise(&[t(0, 512, 10, true), t(1, 512, 20, true)], &p)[..], [ScaleAdvice::RemoveShard { .. }]));
        assert!(matches!(advise(&[t(0, 900, 500, true), t(1, 124, 500, true)], &p)[..], [ScaleAdvice::Rebalance { .. }]));
        assert_eq!(advise(&[t(0, 512, 500, true), t(1, 512, 0, false)], &p), vec![ScaleAdvice::Unreachable { shard: 1 }]);
        // one shard is never removed
        assert!(advise(&[t(0, 1024, 1, true)], &p).is_empty());
    }

    #[test]
    fn advice_follows_the_resource_thresholds() {
        let p = ScalePolicy { max_rows_per_shard: u64::MAX, min_rows_per_shard: 0, ..Default::default() };
        let adds = |a: &[ScaleAdvice]| a.iter().any(|x| matches!(x, ScaleAdvice::AddShard { .. }));
        let removes = |a: &[ScaleAdvice]| a.iter().any(|x| matches!(x, ScaleAdvice::RemoveShard { .. }));
        // busy processors on average: add; one hot shard among cool ones is a bottleneck, not a reason to grow
        let a = advise(&[busy(0, 90.0, 40.0), busy(1, 80.0, 40.0)], &p);
        assert!(adds(&a) && a.iter().any(|x| matches!(x, ScaleAdvice::Bottleneck { resource: Resource::Cpu, .. })), "{:?}", a);
        let a = advise(&[busy(0, 90.0, 40.0), busy(1, 30.0, 40.0), busy(2, 30.0, 40.0)], &p);
        assert!(!adds(&a) && a.iter().any(|x| matches!(x, ScaleAdvice::Bottleneck { shard: 0, resource: Resource::Cpu, .. })), "{:?}", a);
        // memory
        assert!(adds(&advise(&[busy(0, 10.0, 95.0), busy(1, 10.0, 90.0)], &p)));
        // idle: remove one, unless what remains would be above the limits with the load of the one removed
        assert!(removes(&advise(&[busy(0, 10.0, 20.0), busy(1, 10.0, 20.0), busy(2, 10.0, 20.0)], &p)));
        assert!(!removes(&advise(&[busy(0, 18.0, 70.0), busy(1, 18.0, 70.0)], &p)), "memory would be 140%");
        // the least and the most shards
        let p1 = ScalePolicy { min_shards: 2, ..p.clone() };
        assert!(!removes(&advise(&[busy(0, 10.0, 20.0), busy(1, 10.0, 20.0)], &p1)));
        let p2 = ScalePolicy { max_shards: 2, ..p.clone() };
        assert!(!adds(&advise(&[busy(0, 90.0, 40.0), busy(1, 90.0, 40.0)], &p2)));
    }
}
