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
//! The group's log is kept short by **snapshots**: when a replica has applied `snapshot_every` entries since its last one it
//! forces its engine's log to disk, exports the engine's whole state (`Server::export_state`) and lets Raft drop the log up to
//! that point. A replica that has fallen behind the start of the leader's log is sent the leader's snapshot and installs it
//! into its engine (`Server::install_state`). The **membership** of the group can change while it serves: a replica is added
//! as a learner, caught up, and promoted; a replica is removed (`add_replica`, `remove_replica`).
//!
//! Requests are served by the leader only, once a majority has recently heard from it (a lease) and it has applied
//! everything the group committed; a request to another replica is refused with the leader's address.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tsql_raft::{Conf, ConfError, Config, Entry, FileStorage, Member, Msg, NodeId, Raft, Role, Snapshot, Storage};
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
    /// this replica's own `host:port` (what the others use to reach it); needed to change the membership
    pub addr: String,
    /// this replica joins a group that exists: it takes no part in elections until the group has added it, and `peers` lists
    /// replicas it can reach to hear from the leader
    pub join: bool,
    /// take a snapshot and cut the log after this many applied entries (0: never)
    pub snapshot_every: u64,
}

impl ReplicaConfig {
    pub fn new(node_id: NodeId, peers: Vec<(NodeId, String)>, dir: impl Into<PathBuf>) -> ReplicaConfig {
        ReplicaConfig { node_id, peers, dir: dir.into(), tick_ms: 20, election_ticks: 10, heartbeat_ticks: 2, sync: true, addr: String::new(), join: false, snapshot_every: 5000 }
    }
}

enum Item {
    Entry(Entry),
    Snapshot(Snapshot),
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
    /// the commit timestamp of the last record applied (the engine has published it as its clock when it has caught up)
    applied_ts: u64,
    stop: bool,
}

pub struct Replica {
    pub node_id: NodeId,
    raft: Mutex<Raft<FileStorage>>,
    inbox: Mutex<VecDeque<(NodeId, Msg)>>,
    apply_queue: Mutex<VecDeque<Item>>,
    outbox: tokio::sync::mpsc::UnboundedSender<(NodeId, Msg)>,
    server: Arc<Server>,
    shared: Mutex<Shared>,
    cv: Condvar,
    kick: Condvar,
    kick_lock: Mutex<bool>,
    tick: Duration,
    /// where each replica can be reached (also read by the transport); grows and shrinks with the membership
    book: Arc<Mutex<HashMap<NodeId, String>>>,
    conf: Mutex<Conf>,
    addr: String,
    snapshot_every: u64,
    /// a snapshot is being taken (one at a time)
    snapshot_busy: std::sync::atomic::AtomicBool,
    snapshots_taken: std::sync::atomic::AtomicU64,
    snapshots_installed: std::sync::atomic::AtomicU64,
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

/// A member of the replica group: (id, address, voter).
pub type MemberInfo = (NodeId, String, bool);

impl Replica {
    /// Start a replica: open its Raft log, make `server` a replica of the group, and run the protocol on a thread. Must be
    /// called inside a tokio runtime (the transport uses it).
    pub fn start(cfg: ReplicaConfig, server: Arc<Server>) -> std::io::Result<Arc<Replica>> {
        let storage = FileStorage::open(&cfg.dir, cfg.sync)?;
        let peer_ids: Vec<NodeId> = if cfg.join { vec![] } else { cfg.peers.iter().map(|p| p.0).collect() };
        let raft = Raft::new(Config { id: cfg.node_id, peers: peer_ids, election_ticks: cfg.election_ticks, heartbeat_ticks: cfg.heartbeat_ticks, seed: cfg.node_id.wrapping_mul(2654435761) ^ std::process::id() as u64, join: cfg.join }, storage);
        let (otx, orx) = tokio::sync::mpsc::unbounded_channel();
        let book: Arc<Mutex<HashMap<NodeId, String>>> = Arc::new(Mutex::new(cfg.peers.iter().cloned().collect()));
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
            book: book.clone(),
            conf: Mutex::new(Conf::default()),
            addr: cfg.addr.clone(),
            snapshot_every: cfg.snapshot_every,
            snapshot_busy: Default::default(),
            snapshots_taken: Default::default(),
            snapshots_installed: Default::default(),
        });
        server.set_replicator(Some(Arc::new(ReplHandle(replica.clone()))));
        crate::transport::spawn_sender(cfg.node_id, book, orx);
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
        self.book.lock().unwrap_or_else(|e| e.into_inner()).get(&leader).cloned()
    }

    /// The members of the group as this replica knows them: (id, address, voter).
    pub fn members(&self) -> Vec<MemberInfo> {
        let conf = self.raft.lock().unwrap_or_else(|e| e.into_inner()).conf().clone();
        let book = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let addr = |m: &Member| if !m.addr.is_empty() { m.addr.clone() } else if m.id == self.node_id { self.addr.clone() } else { book.get(&m.id).cloned().unwrap_or_default() };
        conf.voters.iter().map(|m| (m.id, addr(m), true)).chain(conf.learners.iter().map(|m| (m.id, addr(m), false))).collect()
    }

    /// How many snapshots this replica has taken (and cut its log at) and installed (from the leader).
    pub fn snapshot_counts(&self) -> (u64, u64) {
        (self.snapshots_taken.load(std::sync::atomic::Ordering::Relaxed), self.snapshots_installed.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The number of entries the Raft log holds now (the rest are in the snapshot).
    pub fn log_len(&self) -> u64 {
        self.raft.lock().unwrap_or_else(|e| e.into_inner()).log_len()
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
        if let Some(conf) = ready.conf {
            {
                // the transport reaches the members through this book
                let mut b = self.book.lock().unwrap_or_else(|e| e.into_inner());
                for m in conf.voters.iter().chain(conf.learners.iter()) {
                    if !m.addr.is_empty() && m.id != self.node_id {
                        b.insert(m.id, m.addr.clone());
                    }
                }
            }
            *self.conf.lock().unwrap_or_else(|e| e.into_inner()) = conf;
        }
        for m in ready.messages {
            let _ = self.outbox.send(m);
        }
        let mut q = self.apply_queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = ready.snapshot {
            q.push_back(Item::Snapshot(s));
        }
        q.extend(ready.committed.into_iter().map(Item::Entry));
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
                // at most one tick per turn: a thread that was held up (waiting for the engine) must not make up the time it
                // lost in one go, after it has just read the leader's heartbeat, or its election timer would run out
                // although the leader is fine
                if last_tick.elapsed() >= self.tick {
                    last_tick = Instant::now();
                    raft.tick();
                }
                self.pump(&mut raft);
            }
            // apply what the group committed
            let mut applied_any = false;
            loop {
                let next = self.apply_queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let Some(item) = next else { break };
                self.apply(item);
                applied_any = true;
            }
            if applied_any {
                self.maybe_snapshot();
            }
            let wait = self.tick.saturating_sub(last_tick.elapsed()).max(Duration::from_millis(1));
            let g = self.kick_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut g = if *g { g } else { self.kick.wait_timeout(g, wait).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0) };
            *g = false;
        }
    }

    fn apply(&self, item: Item) {
        let e = match item {
            Item::Snapshot(s) => {
                // this node is a follower that fell behind: whatever it proposed when it led will not be applied here, and
                // the sessions that wait for it must let go of the engine before the snapshot can replace its state
                {
                    let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                    let stale: Vec<u64> = sh.proposals.keys().copied().collect();
                    for i in stale {
                        if let Some(ts) = sh.proposals.remove(&i) {
                            sh.local_ts.remove(&ts);
                        }
                        sh.results.insert(i, Err("the proposal was superseded by a snapshot".into()));
                    }
                }
                self.cv.notify_all();
                // a snapshot from the leader replaces the engine's state: everything up to its index is in it
                if let Err(err) = self.server.install_state(&s.data) {
                    eprintln!("replica {}: installing the snapshot of index {} failed: {}", self.node_id, s.index, err.message);
                }
                self.snapshots_installed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                sh.applied_index = s.index;
                sh.applied_ts = self.server.applied_ts();
                drop(sh);
                self.cv.notify_all();
                return;
            }
            Item::Entry(e) => e,
        };
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
                    sh.applied_ts = sh.applied_ts.max(rec.ts);
                    drop(sh);
                    if skip {
                        // the session that proposed it makes its changes itself; what it does to the prepared set is done here,
                        // so that a snapshot taken now has it
                        self.server.note_prepared(&rec, true);
                    } else {
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

    /// Once enough entries have been applied since the last snapshot, take one on a helper thread: force the engine's log to
    /// disk (it holds what the entries said), export the engine's state, and let Raft drop the log up to the entry the
    /// snapshot is of. It runs beside the apply thread, not on it: the export reads the engine while sessions that are
    /// completing a commit wait for the apply thread, so the apply thread must not wait for the engine.
    fn maybe_snapshot(self: &Arc<Self>) {
        if self.snapshot_every == 0 {
            return;
        }
        let (applied, ts) = {
            let sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (sh.applied_index, sh.applied_ts)
        };
        let from = self.raft.lock().unwrap_or_else(|e| e.into_inner()).snapshot_index();
        if applied < from + self.snapshot_every || self.snapshot_busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name(format!("raft-snapshot-{}", self.node_id)).spawn(move || {
            me.take_snapshot(applied, ts);
            me.snapshot_busy.store(false, std::sync::atomic::Ordering::SeqCst);
        });
        if spawned.is_err() {
            self.snapshot_busy.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A snapshot of the engine as of the entry `applied` (whose commit timestamp is `ts`).
    fn take_snapshot(&self, applied: u64, ts: u64) {
        // the engine publishes a commit's timestamp when it has made the commit's changes; the snapshot must have them all
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.server.applied_ts() < ts {
            if Instant::now() >= deadline || self.shared.lock().unwrap_or_else(|e| e.into_inner()).stop {
                return; // try again after the next entries
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // what the entries up to `applied` did is on disk in the engine's own log before the entries are forgotten
        if self.server.flush_log().is_err() {
            return;
        }
        let Ok(data) = self.server.export_state_at(ts) else { return };
        if self.raft.lock().unwrap_or_else(|e| e.into_inner()).compact(applied, data) {
            self.snapshots_taken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn wait_applied(&self, index: u64, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        while sh.applied_index < index {
            let now = Instant::now();
            if now >= deadline || sh.stop {
                return Err("timed out waiting for the group to commit the membership change".into());
            }
            sh = self.cv.wait_timeout(sh, deadline - now).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
        }
        Ok(())
    }

    /// The configuration to change from, with every member's address filled in (the first change of a group that started from
    /// its command line has to give the others the addresses they will need).
    fn conf_with_addrs(&self, raft: &Raft<FileStorage>) -> Conf {
        let mut c = raft.conf().clone();
        let book = self.book.lock().unwrap_or_else(|e| e.into_inner());
        for m in c.voters.iter_mut().chain(c.learners.iter_mut()) {
            if m.addr.is_empty() {
                m.addr = if m.id == self.node_id { self.addr.clone() } else { book.get(&m.id).cloned().unwrap_or_default() };
            }
        }
        c
    }

    /// Propose a new configuration (`change` maps the current one to it) and wait until the group has committed it. Leader only.
    fn change_conf(&self, timeout: Duration, change: impl Fn(Conf) -> Conf) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let proposed = {
                let mut raft = self.raft.lock().unwrap_or_else(|e| e.into_inner());
                if !raft.is_leader() {
                    return Err(format!("this replica is not the leader{}", raft.leader().map(|l| format!(" (node {} is)", l)).unwrap_or_default()));
                }
                let c = change(self.conf_with_addrs(&raft));
                let r = raft.propose_conf(c, vec![]);
                self.pump(&mut raft);
                r
            };
            self.wake();
            match proposed {
                Ok(index) => return self.wait_applied(index, deadline.saturating_duration_since(Instant::now())),
                Err(ConfError::Busy) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(ConfError::Busy) => return Err("a membership change is still in progress".into()),
                Err(ConfError::TooMany) => return Err("only one voter can be added or removed at a time".into()),
                Err(ConfError::NotLeader(l)) => return Err(format!("this replica is not the leader{}", l.map(|l| format!(" (node {} is)", l)).unwrap_or_default())),
            }
        }
    }

    /// Add the replica `id` (a `tsql-shard --join` that is up, at `addr`) to the group: it joins as a learner that is sent the
    /// log (or the snapshot), and once it has caught up it is made a voter. Leader only. Returns when the change is committed.
    pub fn add_replica(&self, id: NodeId, addr: &str, timeout: Duration) -> Result<(), String> {
        if self.addr.is_empty() {
            return Err("this replica was not told its own address (ReplicaConfig::addr)".into());
        }
        let deadline = Instant::now() + timeout;
        self.book.lock().unwrap_or_else(|e| e.into_inner()).insert(id, addr.to_string());
        let member = Member { id, addr: addr.to_string() };
        self.change_conf(timeout, |c| c.with_learner(member.clone()))?;
        // the learner receives the log; wait until it is close to the leader's
        loop {
            let caught_up = {
                let raft = self.raft.lock().unwrap_or_else(|e| e.into_inner());
                if !raft.is_leader() {
                    return Err("this replica lost leadership while the new replica was catching up".into());
                }
                let commit = raft.commit_index();
                raft.progress().iter().any(|(p, matched, _)| *p == id && *matched + 2 >= commit)
            };
            if caught_up {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("replica {} did not catch up in time (it stays a learner)", id));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.change_conf(deadline.saturating_duration_since(Instant::now()).max(Duration::from_secs(1)), |c| c.with_promoted(id))
    }

    /// Remove the replica `id` from the group (a voter or a learner; the leader may remove itself, and then hands over).
    pub fn remove_replica(&self, id: NodeId, timeout: Duration) -> Result<(), String> {
        let known = self.members().iter().any(|m| m.0 == id);
        if !known {
            return Ok(());
        }
        self.change_conf(timeout, |c| c.without(id))
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

#[allow(dead_code)]
fn _storage_is_used<S: Storage>(_: &S) {}
