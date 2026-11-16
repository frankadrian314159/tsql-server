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
}

impl Cluster {
    /// Open a cluster whose control state (shards, slot map, tables, schema) is kept in `state_path`: if the file exists
    /// the cluster is restored from it (and `config` is not consulted), else it is created from `config` and the file is
    /// written. The control node can then be restarted without losing track of the data on the shards.
    pub fn open(state_path: impl Into<PathBuf>, config: &ClusterConfig) -> Result<Arc<Cluster>, SqlError> {
        let path: PathBuf = state_path.into();
        let (addrs, slots, tables, log, pending) = match std::fs::read(&path) {
            Ok(bytes) => {
                let st: State = postcard::from_bytes(&bytes).map_err(|e| SqlError::new(50000, format!("{}: bad cluster state: {}", path.display(), e)))?;
                (st.shard_addrs, Some(st.slots), st.tables, st.schema_log, st.pending)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                config.validate().map_err(|e| SqlError::new(50000, e))?;
                (config.shards.iter().map(|s| s.addr.clone()).collect(), None, vec![], vec![], vec![])
            }
            Err(e) => return Err(SqlError::new(50000, format!("{}: {}", path.display(), e))),
        };
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().map_err(|e| SqlError::new(50000, e.to_string()))?;
        let shards = {
            let _g = rt.enter();
            addrs.iter().enumerate().map(|(i, a)| ShardClient::new(i as u32, a)).collect::<Result<Vec<_>, _>>()?
        };
        let n = shards.len();
        let slots = slots.unwrap_or_else(|| (0..SLOTS).map(|i| (i % n) as u32).collect());
        let mirror = Server::new();
        {
            // the mirror learns the schema again
            let mut c = mirror.connect_internal();
            for st in &log {
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
            state_path: Some(path),
            migration: RwLock::new(()),
            mirror,
            tables: Mutex::new(tables),
            gather: Mutex::new(()),
            next_session: AtomicU64::new(crate::fresh_session_base()),
            broadcast_limit: AtomicU64::new(100_000),
            lock_timeout_ms: AtomicU64::new(5000),
            pending: Mutex::new(pending),
        });
        cluster.save_state();
        // transactions that were decided to commit when the control node stopped are finished
        cluster.resolve_pending();
        Ok(cluster)
    }

    /// Record a successful CREATE / DROP of a table or index (so that a new shard can be given the schema) and save.
    pub(crate) fn log_schema(&self, st: &Stmt) {
        self.schema_log.lock().unwrap_or_else(|e| e.into_inner()).push(st.clone());
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
            let client = ShardClient::new(id as u32, addr)?;
            client.status().await?;
            s.cluster.shards.write().unwrap_or_else(|e| e.into_inner()).push(client);
            let r = async {
                let log = s.cluster.schema_log.lock().unwrap_or_else(|e| e.into_inner()).clone();
                for st in log {
                    s.fan_run(&[id], |_| Work::Stmts(vec![st.clone()])).await?;
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
                    Ok(st) => ShardTelemetry { id: i, reachable: true, slots: slots.get(i).copied().unwrap_or(0), sessions: st.sessions, lock_waits: st.lock_waits, log_forces: st.log_forces, rows },
                    Err(_) => ShardTelemetry { id: i, reachable: false, slots: slots.get(i).copied().unwrap_or(0), sessions: 0, lock_waits: 0, log_forces: 0, rows: 0 },
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
            items: vec![SelectItem::Star(None)],
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
        let values: Vec<Vec<Expr>> = rows.iter().map(|r| r.iter().cloned().map(Expr::Lit).collect()).collect();
        let ins = Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(t.name.clone()), cols: None, output: None, src: InsertSrc::Values(values), top: None }));
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
                items: vec![SelectItem::Star(None)],
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
}

/// When to grow or shrink.
#[derive(Clone, Debug)]
pub struct ScalePolicy {
    /// add a shard when the shards hold more rows than this on average
    pub max_rows_per_shard: u64,
    /// remove one when they hold fewer than this on average (and there is more than one)
    pub min_rows_per_shard: u64,
    /// rebalance when the busiest shard owns more than this multiple of the average number of slots
    pub max_slot_imbalance: f64,
}

impl Default for ScalePolicy {
    fn default() -> Self {
        ScalePolicy { max_rows_per_shard: 50_000_000, min_rows_per_shard: 1_000_000, max_slot_imbalance: 1.1 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScaleAdvice {
    AddShard { reason: String },
    RemoveShard { reason: String },
    Rebalance { reason: String },
    /// the shard does not answer: replace it (or restore it from its replica)
    Unreachable { shard: usize },
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
    let avg = total / n;
    let slots_avg = t.iter().map(|s| s.slots).sum::<usize>() as f64 / t.len() as f64;
    let slots_max = t.iter().map(|s| s.slots).max().unwrap_or(0) as f64;
    if slots_avg > 0.0 && slots_max / slots_avg > p.max_slot_imbalance {
        out.push(ScaleAdvice::Rebalance { reason: format!("the busiest shard owns {:.0} slots, {:.2} times the average", slots_max, slots_max / slots_avg) });
    }
    if avg > p.max_rows_per_shard {
        out.push(ScaleAdvice::AddShard { reason: format!("{} rows per shard on average (more than {})", avg, p.max_rows_per_shard) });
    } else if n > 1 && avg < p.min_rows_per_shard && out.is_empty() {
        out.push(ScaleAdvice::RemoveShard { reason: format!("{} rows per shard on average (fewer than {})", avg, p.min_rows_per_shard) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: usize, slots: usize, rows: u64, up: bool) -> ShardTelemetry {
        ShardTelemetry { id, reachable: up, slots, sessions: 0, lock_waits: 0, log_forces: 0, rows }
    }

    #[test]
    fn advice_follows_the_policy() {
        let p = ScalePolicy { max_rows_per_shard: 1000, min_rows_per_shard: 100, max_slot_imbalance: 1.1 };
        assert!(advise(&[t(0, 512, 500, true), t(1, 512, 500, true)], &p).is_empty());
        assert!(matches!(advise(&[t(0, 512, 5000, true), t(1, 512, 3000, true)], &p)[..], [ScaleAdvice::AddShard { .. }]));
        assert!(matches!(advise(&[t(0, 512, 10, true), t(1, 512, 20, true)], &p)[..], [ScaleAdvice::RemoveShard { .. }]));
        assert!(matches!(advise(&[t(0, 900, 500, true), t(1, 124, 500, true)], &p)[..], [ScaleAdvice::Rebalance { .. }]));
        assert_eq!(advise(&[t(0, 512, 500, true), t(1, 512, 0, false)], &p), vec![ScaleAdvice::Unreachable { shard: 1 }]);
        // one shard is never removed
        assert!(advise(&[t(0, 1024, 1, true)], &p).is_empty());
    }
}
