//! A shard that is replicated: several nodes hold the same data and elect a leader (Raft); a commit on the leader is
//! complete when a majority of the replicas have the record.
//!
//! The unit of replication is the engine's own redo record (`WalRecord`): what a transaction wrote, as the write-ahead log
//! would record it. A transaction on the leader runs on the leader's engine as it would on one node (locks, isolation,
//! constraints, triggers); when it commits, the engine hands its record to this replica (`Replicator::propose`, under the
//! commit lock, so records enter the group's log in commit order), waits until the group has committed it
//! (`Replicator::wait`) and only then makes the transaction visible and releases its locks. The other replicas apply the
//! records of the log to their engines (`Server::apply_replicated`) in order. Nothing is re-executed, so replicas cannot
//! diverge through non-deterministic functions or lock timing.
//!
//! Requests are served by the leader only, once a majority has recently heard from it (a lease) and it has applied
//! everything the group committed; a request to another replica is refused with the leader's address.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tsql_raft::{Config, Entry, FileStorage, Msg, NodeId, Raft, Role};
use tsql_server::persist::{Replicator, WalRecord};
use tsql_server::Server;

/// What the group's log carries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReplCmd {
    /// A committed transaction.
    Record(WalRecord),
}

/// A Raft message on the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub from: NodeId,
    pub msg: Msg,
}

#[derive(Clone, Debug)]
pub struct ReplicaConfig {
    pub node_id: NodeId,
    /// the other replicas: id and `host:port` of their Flight service
    pub peers: Vec<(NodeId, String)>,
    /// where this replica keeps its Raft log (the engine's own data directory is given separately)
    pub dir: PathBuf,
    pub tick_ms: u64,
    pub election_ticks: u32,
    pub heartbeat_ticks: u32,
    /// force the Raft log to disk at every write (tests may turn it off)
    pub sync: bool,
}

impl ReplicaConfig {
    pub fn new(node_id: NodeId, peers: Vec<(NodeId, String)>, dir: impl Into<PathBuf>) -> ReplicaConfig {
        ReplicaConfig { node_id, peers, dir: dir.into(), tick_ms: 20, election_ticks: 10, heartbeat_ticks: 2, sync: true }
    }
}

#[derive(Default)]
struct Shared {
    /// proposals of this node: raft index -> commit timestamp of the record
    proposals: HashMap<u64, u64>,
    /// the outcome of a proposal once its entry has been applied
    results: HashMap<u64, Result<(), String>>,
    /// timestamps of records this node's own engine committed (and has applied itself)
    local_ts: HashSet<u64>,
    applied_index: u64,
    stop: bool,
}

pub struct Replica {
    pub node_id: NodeId,
    raft: Mutex<Raft<FileStorage>>,
    inbox: Mutex<VecDeque<(NodeId, Msg)>>,
    apply_queue: Mutex<VecDeque<Entry>>,
    outbox: tokio::sync::mpsc::UnboundedSender<(NodeId, Msg)>,
    server: Arc<Server>,
    shared: Mutex<Shared>,
    cv: Condvar,
    kick: Condvar,
    kick_lock: Mutex<bool>,
    tick: Duration,
    peers: Vec<(NodeId, String)>,
}

/// Who the replica is, as seen by the group.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplicaState {
    pub role: Role,
    pub term: u64,
    pub leader: Option<NodeId>,
    pub commit: u64,
    pub applied: u64,
}

impl Replica {
    /// Start a replica: open its Raft log, make `server` a replica of the group, and run the protocol on a thread. Must be
    /// called inside a tokio runtime (the transport uses it).
    pub fn start(cfg: ReplicaConfig, server: Arc<Server>) -> std::io::Result<Arc<Replica>> {
        let storage = FileStorage::open(&cfg.dir, cfg.sync)?;
        let raft = Raft::new(Config { id: cfg.node_id, peers: cfg.peers.iter().map(|p| p.0).collect(), election_ticks: cfg.election_ticks, heartbeat_ticks: cfg.heartbeat_ticks, seed: cfg.node_id.wrapping_mul(2654435761) ^ std::process::id() as u64 }, storage);
        let (otx, orx) = tokio::sync::mpsc::unbounded_channel();
        let replica = Arc::new(Replica {
            node_id: cfg.node_id,
            raft: Mutex::new(raft),
            inbox: Mutex::new(VecDeque::new()),
            apply_queue: Mutex::new(VecDeque::new()),
            outbox: otx,
            server: server.clone(),
            shared: Mutex::new(Shared::default()),
            cv: Condvar::new(),
            kick: Condvar::new(),
            kick_lock: Mutex::new(false),
            tick: Duration::from_millis(cfg.tick_ms),
            peers: cfg.peers.clone(),
        });
        server.set_replicator(Some(Arc::new(ReplHandle(replica.clone()))));
        crate::transport::spawn_sender(cfg.node_id, cfg.peers.clone(), orx);
        let r2 = replica.clone();
        std::thread::Builder::new().name(format!("raft-{}", cfg.node_id)).spawn(move || r2.run()).expect("spawn the raft thread");
        Ok(replica)
    }

    /// Stop the protocol thread (the node stops taking part in the group).
    pub fn stop(&self) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.wake();
        self.cv.notify_all();
        self.server.set_replicator(None);
    }

    fn wake(&self) {
        *self.kick_lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.kick.notify_all();
    }

    /// A message from a peer.
    pub fn deliver(&self, from: NodeId, msg: Msg) {
        self.inbox.lock().unwrap_or_else(|e| e.into_inner()).push_back((from, msg));
        self.wake();
    }

    pub fn state(&self) -> ReplicaState {
        let r = self.raft.lock().unwrap_or_else(|e| e.into_inner());
        ReplicaState { role: r.role(), term: r.term(), leader: r.leader(), commit: r.commit_index(), applied: self.shared.lock().unwrap_or_else(|e| e.into_inner()).applied_index }
    }

    /// The address of the leader this replica knows, if any.
    pub fn leader_addr(&self) -> Option<String> {
        let leader = self.raft.lock().unwrap_or_else(|e| e.into_inner()).leader()?;
        if leader == self.node_id {
            return None;
        }
        self.peers.iter().find(|p| p.0 == leader).map(|p| p.1.clone())
    }

    /// Whether this replica can serve requests now: it is the leader, a majority has answered it recently, and it has
    /// applied everything the group has committed.
    pub fn can_serve(&self) -> bool {
        let r = self.raft.lock().unwrap_or_else(|e| e.into_inner());
        if !(r.is_leader() && r.lease_valid() && r.has_committed_in_term()) {
            return false;
        }
        let commit = r.commit_index();
        drop(r);
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).applied_index >= commit
    }

    /// Take what Raft has to send and to apply (with the raft lock held, so the order of the entries is kept).
    fn pump(&self, raft: &mut Raft<FileStorage>) {
        let ready = raft.ready();
        for m in ready.messages {
            let _ = self.outbox.send(m);
        }
        if !ready.committed.is_empty() {
            self.apply_queue.lock().unwrap_or_else(|e| e.into_inner()).extend(ready.committed);
        }
    }

    fn run(self: Arc<Self>) {
        let mut last_tick = Instant::now();
        loop {
            if self.shared.lock().unwrap_or_else(|e| e.into_inner()).stop {
                return;
            }
            // messages from peers
            let msgs: Vec<(NodeId, Msg)> = self.inbox.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
            {
                let mut raft = self.raft.lock().unwrap_or_else(|e| e.into_inner());
                for (from, m) in msgs {
                    raft.step(from, m);
                }
                while last_tick.elapsed() >= self.tick {
                    last_tick += self.tick;
                    raft.tick();
                }
                self.pump(&mut raft);
            }
            // apply what the group committed
            loop {
                let next = self.apply_queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let Some(e) = next else { break };
                self.apply(e);
            }
            let wait = self.tick.saturating_sub(last_tick.elapsed()).max(Duration::from_millis(1));
            let g = self.kick_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut g = if *g { g } else { self.kick.wait_timeout(g, wait).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0) };
            *g = false;
        }
    }

    fn apply(&self, e: Entry) {
        if !e.data.is_empty() {
            match postcard::from_bytes::<ReplCmd>(&e.data) {
                Ok(ReplCmd::Record(rec)) => {
                    let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                    let mine = sh.local_ts.contains(&rec.ts);
                    match sh.proposals.remove(&e.index) {
                        Some(ts) if ts == rec.ts => {
                            sh.local_ts.remove(&rec.ts);
                            sh.results.insert(e.index, Ok(()));
                        }
                        Some(_) => {
                            sh.results.insert(e.index, Err("the proposal was superseded by another leader".into()));
                        }
                        None => {}
                    }
                    let skip = mine && sh.results.get(&e.index) == Some(&Ok(()));
                    drop(sh);
                    if !skip {
                        // a record of another replica (or of this node's earlier life): make its changes here
                        if let Err(err) = self.server.apply_replicated(&rec) {
                            eprintln!("replica {}: applying the record at index {} failed: {}", self.node_id, e.index, err.message);
                        }
                    }
                }
                Err(err) => eprintln!("replica {}: bad entry at index {}: {}", self.node_id, e.index, err),
            }
        }
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).applied_index = e.index;
        self.cv.notify_all();
    }
}

/// What the engine calls (see `tsql_server::persist::Replicator`).
struct ReplHandle(Arc<Replica>);

impl Replicator for ReplHandle {
    fn propose(&self, rec: &WalRecord) -> Result<u64, String> {
        let data = postcard::to_allocvec(&ReplCmd::Record(rec.clone())).map_err(|e| e.to_string())?;
        let mut raft = self.0.raft.lock().unwrap_or_else(|e| e.into_inner());
        let index = raft.propose(data).map_err(|e| format!("this replica is not the leader{}", e.leader.map(|l| format!(" (node {} is)", l)).unwrap_or_default()))?;
        {
            let mut sh = self.0.shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.proposals.insert(index, rec.ts);
            sh.local_ts.insert(rec.ts);
        }
        self.0.pump(&mut raft);
        Ok(index)
    }

    fn wait(&self, ticket: u64) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut sh = self.0.shared.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = sh.results.remove(&ticket) {
                return r;
            }
            let now = Instant::now();
            if now >= deadline || sh.stop {
                // forget the proposal: if the group commits it later, this node applies it like any other record
                if let Some(ts) = sh.proposals.remove(&ticket) {
                    sh.local_ts.remove(&ts);
                }
                return Err("timed out waiting for a majority of the replicas".into());
            }
            sh = self.0.cv.wait_timeout(sh, deadline - now).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
        }
    }
}
