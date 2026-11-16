//! A small Raft consensus core.
//!
//! [`Raft`] is a pure state machine: it is driven by [`Raft::tick`] (the passage of time), [`Raft::step`] (a message from a
//! peer) and [`Raft::propose`] (a command from a client), and it hands back what to send and what has been committed with
//! [`Raft::ready`]. It does no I/O of its own except through its [`Storage`] (the log, the vote and the snapshot, which must
//! be durable before a message that depends on them leaves), so it can be tested deterministically with a simulated network
//! (see `tests/simulation.rs`) and driven by real sockets and timers in production.
//!
//! What it implements:
//! * leader election with randomized timeouts, log replication with fast back-off, commitment of entries by a majority (a
//!   leader commits only entries of its own term by counting, as the Raft paper requires, and starts its term with an empty
//!   entry for that reason), a leader lease for reads, and a file-backed log;
//! * **log compaction and snapshots**: the application takes a snapshot of its state machine at an index it has applied and
//!   hands it to [`Raft::compact`]; the log up to that index is dropped. A follower that is behind the start of the leader's
//!   log is sent the snapshot (in chunks) and hands it to its own state machine through [`Ready::snapshot`];
//! * **membership changes**, one voter at a time (the single-server change of the Raft dissertation): a configuration is a
//!   log entry that takes effect when it is appended; a new member joins as a non-voting *learner* that is caught up first
//!   and is then promoted. Only one change can be uncommitted at a time, and none before the leader has committed an entry
//!   of its term;
//! * a vote is refused while a leader has been heard from recently (so a node that was removed, or cut off, and keeps timing
//!   out cannot depose a working leader by raising the term), and votes only count from the voters of the configuration.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub mod storage;
pub use storage::{FileStorage, MemStorage, SnapMeta, Storage};

pub type NodeId = u64;

/// A member of the group: its id, and an address the application can use to reach it (opaque to the protocol).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Member {
    pub id: NodeId,
    pub addr: String,
}

/// The membership of the group: the **voters** (they elect leaders and count towards a majority) and the **learners** (they
/// receive the log but do not vote).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Conf {
    pub voters: Vec<Member>,
    pub learners: Vec<Member>,
}

impl Conf {
    pub fn is_voter(&self, id: NodeId) -> bool {
        self.voters.iter().any(|m| m.id == id)
    }
    pub fn is_learner(&self, id: NodeId) -> bool {
        self.learners.iter().any(|m| m.id == id)
    }
    pub fn voter_ids(&self) -> Vec<NodeId> {
        self.voters.iter().map(|m| m.id).collect()
    }
    /// Every member, voter or learner.
    pub fn ids(&self) -> Vec<NodeId> {
        self.voters.iter().chain(self.learners.iter()).map(|m| m.id).collect()
    }
    pub fn addr_of(&self, id: NodeId) -> Option<&str> {
        self.voters.iter().chain(self.learners.iter()).find(|m| m.id == id).map(|m| m.addr.as_str())
    }
    /// How many voters make a majority.
    pub fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }
    /// This configuration with `m` as a learner (a voter's entry is left alone).
    pub fn with_learner(&self, m: Member) -> Conf {
        let mut c = self.clone();
        if !c.is_voter(m.id) && !c.is_learner(m.id) {
            c.learners.push(m);
        }
        c
    }
    /// This configuration with the learner `id` made a voter.
    pub fn with_promoted(&self, id: NodeId) -> Conf {
        let mut c = self.clone();
        if let Some(i) = c.learners.iter().position(|m| m.id == id) {
            let m = c.learners.remove(i);
            c.voters.push(m);
        }
        c
    }
    /// This configuration without `id` (a voter or a learner).
    pub fn without(&self, id: NodeId) -> Conf {
        let mut c = self.clone();
        c.voters.retain(|m| m.id != id);
        c.learners.retain(|m| m.id != id);
        c
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub term: u64,
    pub index: u64,
    /// the command; empty for the entry a new leader starts its term with
    pub data: Vec<u8>,
    /// set on a membership change: the configuration that takes effect when the entry is appended
    pub conf: Option<Conf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    /// A candidate asks for a vote.
    Vote { term: u64, last_index: u64, last_term: u64 },
    VoteReply { term: u64, granted: bool },
    /// The leader replicates entries (none: a heartbeat) and says how far the log is committed.
    Append { term: u64, prev_index: u64, prev_term: u64, entries: Vec<Entry>, commit: u64 },
    /// `match_index`: how far the follower's log matches the leader's (on success), or the end of its log (a hint on failure).
    AppendReply { term: u64, success: bool, match_index: u64 },
    /// The leader sends its snapshot (state as of `index`, an entry of term `last_term`) in chunks, for a follower whose log
    /// ends before the leader's log begins. `total` is the size of `data` when it is whole.
    Snapshot { term: u64, index: u64, last_term: u64, conf: Conf, offset: u64, total: u64, data: Vec<u8> },
    /// The follower has the snapshot up to `next_offset` (`done`: all of it, and installed, or it already had what it holds).
    SnapshotReply { term: u64, index: u64, next_offset: u64, done: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub id: NodeId,
    /// the other members of the group, as voters (when the log holds no configuration yet)
    pub peers: Vec<NodeId>,
    /// ticks without hearing from a leader before an election starts (randomized between min and 2*min)
    pub election_ticks: u32,
    pub heartbeat_ticks: u32,
    /// seeds the randomization of the election timeout (so a simulation is repeatable)
    pub seed: u64,
    /// this node is joining a group that already exists: it starts with no configuration (so it never starts an election)
    /// and learns the group from the leader
    pub join: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct NotLeader {
    pub leader: Option<NodeId>,
}

/// Why a membership change was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfError {
    NotLeader(Option<NodeId>),
    /// the leader has not yet committed an entry of its term, or the previous change is not committed: try again shortly
    Busy,
    /// more than one voter would be added or removed at once
    TooMany,
}

/// A snapshot to install into the state machine (a follower that was sent one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub index: u64,
    pub term: u64,
    pub data: Vec<u8>,
}

/// What to do after a step: messages to send, a snapshot to install, and entries that became committed (in order, each
/// exactly once; a snapshot, when there is one, comes before the entries that follow it).
#[derive(Default, Debug)]
pub struct Ready {
    pub messages: Vec<(NodeId, Msg)>,
    pub committed: Vec<Entry>,
    pub snapshot: Option<Snapshot>,
    /// the membership, when it changed since the last call
    pub conf: Option<Conf>,
}

/// Bytes of a snapshot per message.
const CHUNK: usize = 256 * 1024;

struct SnapIn {
    index: u64,
    total: u64,
    data: Vec<u8>,
}

pub struct Raft<S: Storage> {
    cfg: Config,
    st: S,
    role: Role,
    term: u64,
    vote: Option<NodeId>,
    leader: Option<NodeId>,
    commit: u64,
    delivered: u64,
    /// the membership now (the last configuration in the log, appended or not yet committed) and where it came from
    conf: Conf,
    conf_index: u64,
    conf_dirty: bool,
    next_index: HashMap<NodeId, u64>,
    match_index: HashMap<NodeId, u64>,
    votes: HashSet<NodeId>,
    election_elapsed: u32,
    election_timeout: u32,
    heartbeat_elapsed: u32,
    now: u64,
    /// the tick at which each peer last answered us as leader (for the lease)
    acked: HashMap<NodeId, u64>,
    out: Vec<(NodeId, Msg)>,
    rng: u64,
    /// snapshots being sent: the offset each peer needs next (and the snapshot's index)
    snap_send: HashMap<NodeId, (u64, u64)>,
    snap_cache: Option<(u64, Arc<Vec<u8>>)>,
    snap_in: Option<SnapIn>,
    snap_ready: Option<Snapshot>,
}

impl<S: Storage> Raft<S> {
    pub fn new(cfg: Config, st: S) -> Raft<S> {
        let (term, vote) = st.hard_state();
        let snap_index = st.snap_meta().map(|m| m.index).unwrap_or(0);
        let mut r = Raft {
            rng: cfg.seed.wrapping_mul(0x9E3779B97F4A7C15) | 1,
            conf: Conf::default(),
            conf_index: 0,
            conf_dirty: true,
            cfg,
            st,
            role: Role::Follower,
            term,
            vote,
            leader: None,
            // what the snapshot covers was applied by the state machine before the snapshot was taken
            commit: snap_index,
            delivered: snap_index,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            votes: HashSet::new(),
            election_elapsed: 0,
            election_timeout: 0,
            heartbeat_elapsed: 0,
            now: 0,
            acked: HashMap::new(),
            out: vec![],
            snap_send: HashMap::new(),
            snap_cache: None,
            snap_in: None,
            snap_ready: None,
        };
        r.recompute_conf();
        r.reset_election_timer();
        r
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn reset_election_timer(&mut self) {
        self.election_elapsed = 0;
        self.election_timeout = self.cfg.election_ticks + (self.next_rand() % self.cfg.election_ticks.max(1) as u64) as u32;
    }

    pub fn id(&self) -> NodeId {
        self.cfg.id
    }
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn term(&self) -> u64 {
        self.term
    }
    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }
    /// The leader this node believes in (itself, if it is the leader).
    pub fn leader(&self) -> Option<NodeId> {
        self.leader
    }
    pub fn commit_index(&self) -> u64 {
        self.commit
    }
    pub fn last_index(&self) -> u64 {
        self.st.last_index()
    }
    /// The index of the snapshot the log starts after (0: the log has never been compacted).
    pub fn snapshot_index(&self) -> u64 {
        self.st.snap_meta().map(|m| m.index).unwrap_or(0)
    }
    /// The entries the log holds (those not replaced by a snapshot).
    pub fn log_len(&self) -> u64 {
        self.st.last_index() - self.snapshot_index()
    }
    /// The membership now.
    pub fn conf(&self) -> &Conf {
        &self.conf
    }
    /// Whether the log is committed up to an entry of the current term: a new leader cannot serve requests safely before
    /// the empty entry that starts its term is committed (until then it may not know everything that was committed).
    pub fn has_committed_in_term(&self) -> bool {
        self.commit > 0 && self.st.term_at(self.commit) == Some(self.term)
    }

    /// For each peer, how far the leader knows its log matches and the next index it will send (a leader only).
    pub fn progress(&self) -> Vec<(NodeId, u64, u64)> {
        self.peers().iter().map(|p| (*p, *self.match_index.get(p).unwrap_or(&0), *self.next_index.get(p).unwrap_or(&0))).collect()
    }

    pub fn storage(&self) -> &S {
        &self.st
    }

    /// The other members, voters and learners.
    fn peers(&self) -> Vec<NodeId> {
        self.conf.ids().into_iter().filter(|p| *p != self.cfg.id).collect()
    }

    fn is_voter(&self) -> bool {
        self.conf.is_voter(self.cfg.id)
    }

    /// The configuration the first entries of the log stand on, before any configuration entry.
    fn initial_conf(&self) -> Conf {
        if self.cfg.join {
            return Conf::default();
        }
        let mut v: Vec<Member> = self.cfg.peers.iter().map(|p| Member { id: *p, addr: String::new() }).collect();
        v.push(Member { id: self.cfg.id, addr: String::new() });
        v.sort_by_key(|m| m.id);
        Conf { voters: v, learners: vec![] }
    }

    /// The configuration in force at `index`: the last one at or before it in the log, else the snapshot's, else the first.
    fn conf_at(&self, index: u64) -> (Conf, u64) {
        let snap = self.st.snap_meta();
        let first = snap.as_ref().map(|m| m.index).unwrap_or(0) + 1;
        let mut i = index.min(self.st.last_index());
        while i >= first {
            if let Some(e) = self.st.entries(i, i + 1).into_iter().next() {
                if let Some(c) = e.conf {
                    return (c, i);
                }
            }
            i -= 1;
        }
        match snap {
            Some(m) => (m.conf, m.index),
            None => (self.initial_conf(), 0),
        }
    }

    /// Take the membership from the log again (after a start, or after a conflicting suffix was cut off).
    fn recompute_conf(&mut self) {
        let (c, i) = self.conf_at(self.st.last_index());
        if c != self.conf || i != self.conf_index {
            self.conf_dirty = true;
        }
        self.conf = c;
        self.conf_index = i;
    }

    fn set_conf(&mut self, c: Conf, index: u64) {
        if c != self.conf {
            self.conf_dirty = true;
        }
        self.conf = c;
        self.conf_index = index;
    }

    /// Whether this node is the leader and a majority has answered it recently: while it holds, no other leader can have
    /// been elected (an election needs a majority that heard nothing from this one for a full election timeout), so a read
    /// it serves is not stale.
    pub fn lease_valid(&self) -> bool {
        if self.role != Role::Leader {
            return false;
        }
        let window = self.cfg.election_ticks as u64;
        let me = self.is_voter() as usize;
        let fresh = self.conf.voters.iter().filter(|m| m.id != self.cfg.id && self.acked.get(&m.id).map(|t| self.now.saturating_sub(*t) < window).unwrap_or(false)).count() + me;
        fresh >= self.conf.quorum()
    }

    fn become_follower(&mut self, term: u64, leader: Option<NodeId>) {
        if term > self.term {
            self.term = term;
            self.vote = None;
            self.st.set_hard_state(self.term, self.vote);
        }
        self.role = Role::Follower;
        self.leader = leader;
        self.votes.clear();
        self.reset_election_timer();
    }

    fn become_candidate(&mut self) {
        self.term += 1;
        self.vote = Some(self.cfg.id);
        self.st.set_hard_state(self.term, self.vote);
        self.role = Role::Candidate;
        self.leader = None;
        self.votes.clear();
        self.votes.insert(self.cfg.id);
        self.reset_election_timer();
        if self.votes.len() >= self.conf.quorum() {
            self.become_leader();
            return;
        }
        let last_index = self.st.last_index();
        let last_term = self.st.term_at(last_index).unwrap_or(0);
        for p in self.conf.voter_ids() {
            if p != self.cfg.id {
                self.out.push((p, Msg::Vote { term: self.term, last_index, last_term }));
            }
        }
    }

    fn become_leader(&mut self) {
        self.role = Role::Leader;
        self.leader = Some(self.cfg.id);
        let next = self.st.last_index() + 1;
        self.next_index = self.peers().into_iter().map(|p| (p, next)).collect();
        self.match_index = self.peers().into_iter().map(|p| (p, 0)).collect();
        self.acked.clear();
        self.snap_send.clear();
        self.heartbeat_elapsed = 0;
        // an entry of the new term: committing it commits everything before it
        let e = Entry { term: self.term, index: next, data: vec![], conf: None };
        self.st.append(&[e]);
        self.broadcast_append();
        self.advance_commit();
    }

    /// Time passes by one tick.
    pub fn tick(&mut self) {
        self.now += 1;
        match self.role {
            Role::Leader => {
                self.heartbeat_elapsed += 1;
                if self.heartbeat_elapsed >= self.cfg.heartbeat_ticks {
                    self.heartbeat_elapsed = 0;
                    self.broadcast_append();
                }
            }
            _ => {
                self.election_elapsed += 1;
                if self.election_elapsed >= self.election_timeout {
                    if self.is_voter() {
                        self.become_candidate();
                    } else {
                        // a learner, a node not (yet) in the group, or one that was removed: it waits for a leader
                        self.reset_election_timer();
                    }
                }
            }
        }
    }

    /// A client command: appended to the log and replicated. Returns its index; it is committed when it appears in a later
    /// [`Ready::committed`] (it may never be, if leadership is lost first).
    pub fn propose(&mut self, data: Vec<u8>) -> Result<u64, NotLeader> {
        if self.role != Role::Leader {
            return Err(NotLeader { leader: self.leader });
        }
        let index = self.st.last_index() + 1;
        self.st.append(&[Entry { term: self.term, index, data, conf: None }]);
        self.broadcast_append();
        self.advance_commit();
        Ok(index)
    }

    /// Change the membership to `conf`: it differs from the current one by at most one voter added or removed (learners may
    /// change freely). `data` goes with the entry for the application. The change takes effect on this node now and on the
    /// others as they append the entry. Returns the entry's index.
    pub fn propose_conf(&mut self, conf: Conf, data: Vec<u8>) -> Result<u64, ConfError> {
        if self.role != Role::Leader {
            return Err(ConfError::NotLeader(self.leader));
        }
        if !self.has_committed_in_term() || self.conf_index > self.commit {
            return Err(ConfError::Busy);
        }
        let old: HashSet<NodeId> = self.conf.voter_ids().into_iter().collect();
        let new: HashSet<NodeId> = conf.voter_ids().into_iter().collect();
        if old.symmetric_difference(&new).count() > 1 {
            return Err(ConfError::TooMany);
        }
        let index = self.st.last_index() + 1;
        self.st.append(&[Entry { term: self.term, index, data, conf: Some(conf.clone()) }]);
        self.set_conf(conf, index);
        for p in self.peers() {
            self.next_index.entry(p).or_insert(index);
            self.match_index.entry(p).or_insert(0);
        }
        self.broadcast_append();
        self.advance_commit();
        Ok(index)
    }

    /// Record that the application has applied everything up to `index` and holds a snapshot of its state at that point
    /// (`data`): the log up to it is dropped, and a follower that needs those entries is sent the snapshot instead. `index`
    /// must have been delivered by [`Raft::ready`]. The caller must have made the state durable (so that after a restart
    /// it holds at least what the snapshot holds) before it calls this. Returns false when `index` is not past the current
    /// snapshot or not yet applied.
    pub fn compact(&mut self, index: u64, data: Vec<u8>) -> bool {
        if index <= self.snapshot_index() || index > self.delivered || index > self.st.last_index() {
            return false;
        }
        let Some(term) = self.st.term_at(index) else { return false };
        let (conf, _) = self.conf_at(index);
        self.st.compact(SnapMeta { index, term, conf }, data);
        self.snap_cache = None;
        true
    }

    fn snapshot_arc(&mut self) -> Option<(SnapMeta, Arc<Vec<u8>>)> {
        let meta = self.st.snap_meta()?;
        match &self.snap_cache {
            Some((i, d)) if *i == meta.index => Some((meta, d.clone())),
            _ => {
                let d = Arc::new(self.st.snap_data());
                self.snap_cache = Some((meta.index, d.clone()));
                Some((meta, d))
            }
        }
    }

    fn send_snapshot_chunk(&mut self, to: NodeId) {
        let Some((meta, data)) = self.snapshot_arc() else { return };
        let offset = match self.snap_send.get(&to) {
            Some((idx, off)) if *idx == meta.index => *off,
            _ => 0,
        };
        self.snap_send.insert(to, (meta.index, offset));
        let end = (offset as usize + CHUNK).min(data.len());
        let chunk = data[(offset as usize).min(data.len())..end].to_vec();
        self.out.push((to, Msg::Snapshot { term: self.term, index: meta.index, last_term: meta.term, conf: meta.conf, offset, total: data.len() as u64, data: chunk }));
    }

    fn send_append(&mut self, to: NodeId) {
        let next = *self.next_index.get(&to).unwrap_or(&1);
        let first = self.snapshot_index() + 1;
        if next < first {
            // the entries the peer needs are gone from the log: it gets the snapshot
            self.send_snapshot_chunk(to);
            return;
        }
        self.snap_send.remove(&to);
        let prev_index = next - 1;
        let prev_term = self.st.term_at(prev_index).unwrap_or(0);
        let last = self.st.last_index();
        let entries = if next <= last { self.st.entries(next, (next + 256).min(last + 1)) } else { vec![] };
        self.out.push((to, Msg::Append { term: self.term, prev_index, prev_term, entries, commit: self.commit }));
    }

    fn broadcast_append(&mut self) {
        for p in self.peers() {
            self.send_append(p);
        }
    }

    fn advance_commit(&mut self) {
        // the highest index replicated on a majority of the voters, if it is of the current term
        let voters = self.conf.voter_ids();
        if voters.is_empty() {
            return;
        }
        let mut idx: Vec<u64> = voters.iter().map(|v| if *v == self.cfg.id { self.st.last_index() } else { *self.match_index.get(v).unwrap_or(&0) }).collect();
        idx.sort_unstable_by(|a, b| b.cmp(a));
        let n = idx[self.conf.quorum() - 1];
        if n > self.commit && self.st.term_at(n) == Some(self.term) {
            self.commit = n;
        }
        // a leader that is no longer a voter has seen the change that removed it committed: it hands over
        if self.role == Role::Leader && !self.is_voter() && self.conf_index <= self.commit && self.conf_index > 0 {
            let t = self.term;
            self.become_follower(t, None);
        }
    }

    /// Is this node inside the lease of a leader it has heard from recently (so that it must not help to depose it)?
    fn in_lease(&self) -> bool {
        match self.role {
            Role::Leader => self.lease_valid(),
            _ => self.leader.is_some() && self.election_elapsed < self.cfg.election_ticks,
        }
    }

    /// A message from a peer.
    pub fn step(&mut self, from: NodeId, msg: Msg) {
        let term = match &msg {
            Msg::Vote { term, .. } | Msg::VoteReply { term, .. } | Msg::Append { term, .. } | Msg::AppendReply { term, .. } | Msg::Snapshot { term, .. } | Msg::SnapshotReply { term, .. } => *term,
        };
        if let Msg::Vote { .. } = msg {
            // a node that is not a voter of the group we know cannot disturb it; nor can one that asks for a vote while we
            // have a leader (it was cut off, or removed, and does not know it)
            if !self.conf.voters.is_empty() && !self.conf.is_voter(from) {
                return;
            }
            if term > self.term && self.in_lease() {
                return;
            }
            if !self.is_voter() {
                return;
            }
        }
        if term > self.term {
            let leader = if matches!(msg, Msg::Append { .. } | Msg::Snapshot { .. }) { Some(from) } else { None };
            self.become_follower(term, leader);
        }
        match msg {
            Msg::Vote { term, last_index, last_term } => {
                let my_last = self.st.last_index();
                let my_last_term = self.st.term_at(my_last).unwrap_or(0);
                let up_to_date = last_term > my_last_term || (last_term == my_last_term && last_index >= my_last);
                let grant = term == self.term && up_to_date && (self.vote.is_none() || self.vote == Some(from));
                if grant {
                    self.vote = Some(from);
                    self.st.set_hard_state(self.term, self.vote);
                    self.reset_election_timer();
                }
                self.out.push((from, Msg::VoteReply { term: self.term, granted: grant }));
            }
            Msg::VoteReply { term, granted } => {
                if self.role == Role::Candidate && term == self.term && granted && self.conf.is_voter(from) {
                    self.votes.insert(from);
                    let n = self.votes.iter().filter(|v| self.conf.is_voter(**v)).count();
                    if n >= self.conf.quorum() {
                        self.become_leader();
                    }
                }
            }
            Msg::Append { term, mut prev_index, mut prev_term, entries, commit } => {
                if term < self.term {
                    self.out.push((from, Msg::AppendReply { term: self.term, success: false, match_index: 0 }));
                    return;
                }
                // a leader of this term: a candidate steps down
                if self.role != Role::Follower || self.leader != Some(from) {
                    self.role = Role::Follower;
                    self.leader = Some(from);
                    self.votes.clear();
                }
                self.reset_election_timer();
                // entries before our snapshot are in it: they match by definition (they are committed)
                let snap = self.snapshot_index();
                let mut entries = entries;
                if prev_index < snap {
                    entries.retain(|e| e.index > snap);
                    prev_index = snap;
                    prev_term = self.st.term_at(snap).unwrap_or(0);
                    if entries.first().map(|e| e.index != snap + 1).unwrap_or(false) {
                        // we cannot tell what comes between: ask for what follows our snapshot
                        self.out.push((from, Msg::AppendReply { term: self.term, success: false, match_index: snap }));
                        return;
                    }
                }
                let last = self.st.last_index();
                if prev_index > last || (prev_index > 0 && self.st.term_at(prev_index) != Some(prev_term)) {
                    // no match: tell the leader where our log ends so that it can back up quickly
                    let hint = last.min(prev_index.saturating_sub(1));
                    self.out.push((from, Msg::AppendReply { term: self.term, success: false, match_index: hint }));
                    return;
                }
                let mut to_append: Vec<Entry> = vec![];
                for e in entries.iter() {
                    match self.st.term_at(e.index) {
                        Some(t) if t == e.term => continue, // already have it
                        _ => to_append.push(e.clone()),
                    }
                }
                if !to_append.is_empty() {
                    let truncates = to_append[0].index <= self.st.last_index();
                    self.st.append(&to_append); // truncates a conflicting suffix
                    if truncates {
                        self.recompute_conf();
                    }
                    for e in &to_append {
                        if let Some(c) = &e.conf {
                            self.set_conf(c.clone(), e.index);
                        }
                    }
                }
                let matched = prev_index + entries.len() as u64;
                if commit > self.commit {
                    self.commit = commit.min(matched.max(self.commit)).min(self.st.last_index());
                }
                self.out.push((from, Msg::AppendReply { term: self.term, success: true, match_index: matched }));
            }
            Msg::AppendReply { term, success, match_index } => {
                if self.role != Role::Leader || term != self.term {
                    return;
                }
                self.acked.insert(from, self.now);
                if success {
                    let m = self.match_index.entry(from).or_insert(0);
                    if match_index > *m {
                        *m = match_index;
                    }
                    self.next_index.insert(from, match_index + 1);
                    self.advance_commit();
                    if self.role == Role::Leader && match_index < self.st.last_index() {
                        self.send_append(from);
                    }
                } else {
                    let next = (match_index + 1).max(1);
                    self.next_index.insert(from, next);
                    self.send_append(from);
                }
            }
            Msg::Snapshot { term, index, last_term, conf, offset, total, data } => {
                if term < self.term {
                    self.out.push((from, Msg::AppendReply { term: self.term, success: false, match_index: 0 }));
                    return;
                }
                if self.role != Role::Follower || self.leader != Some(from) {
                    self.role = Role::Follower;
                    self.leader = Some(from);
                    self.votes.clear();
                }
                self.reset_election_timer();
                // we already have everything the snapshot holds (committed, or in our log with the same term)
                if index <= self.commit || (index <= self.st.last_index() && self.st.term_at(index) == Some(last_term)) {
                    self.snap_in = None;
                    self.out.push((from, Msg::SnapshotReply { term: self.term, index, next_offset: total, done: true }));
                    return;
                }
                if offset == 0 {
                    self.snap_in = Some(SnapIn { index, total, data: Vec::new() });
                }
                let (cur, ok) = match &self.snap_in {
                    Some(s) if s.index == index && s.total == total => (s.data.len() as u64, true),
                    _ => (0, false),
                };
                if !ok || offset != cur {
                    // out of step (a lost or repeated chunk): say where to go on from
                    self.out.push((from, Msg::SnapshotReply { term: self.term, index, next_offset: if ok { cur } else { 0 }, done: false }));
                    return;
                }
                let s = self.snap_in.as_mut().expect("snapshot in progress");
                s.data.extend_from_slice(&data);
                let have = s.data.len() as u64;
                if have < total {
                    self.out.push((from, Msg::SnapshotReply { term: self.term, index, next_offset: have, done: false }));
                    return;
                }
                let s = self.snap_in.take().expect("snapshot complete");
                self.st.install_snapshot(SnapMeta { index, term: last_term, conf: conf.clone() }, s.data.clone());
                self.snap_cache = None;
                self.set_conf(conf, index);
                self.commit = self.commit.max(index);
                self.delivered = index;
                self.snap_ready = Some(Snapshot { index, term: last_term, data: s.data });
                self.out.push((from, Msg::SnapshotReply { term: self.term, index, next_offset: total, done: true }));
            }
            Msg::SnapshotReply { term, index, next_offset, done } => {
                if self.role != Role::Leader || term != self.term {
                    return;
                }
                self.acked.insert(from, self.now);
                if done {
                    self.snap_send.remove(&from);
                    let m = self.match_index.entry(from).or_insert(0);
                    if index > *m {
                        *m = index;
                    }
                    self.next_index.insert(from, index.max(*m) + 1);
                    self.advance_commit();
                    if self.role == Role::Leader {
                        self.send_append(from);
                    }
                } else if self.snap_send.get(&from).map(|(i, _)| *i) == Some(index) {
                    self.snap_send.insert(from, (index, next_offset));
                    self.send_snapshot_chunk(from);
                }
            }
        }
    }

    /// Take what is to be sent, the snapshot to install and the entries committed since the last call.
    pub fn ready(&mut self) -> Ready {
        let messages = std::mem::take(&mut self.out);
        let snapshot = self.snap_ready.take();
        let mut committed = vec![];
        if self.commit > self.delivered {
            committed = self.st.entries(self.delivered + 1, self.commit + 1);
            self.delivered = self.commit;
        }
        let conf = if self.conf_dirty {
            self.conf_dirty = false;
            Some(self.conf.clone())
        } else {
            None
        };
        Ready { messages, committed, snapshot, conf }
    }

    /// Entries up to this index have been applied by the caller already (after a restart, the caller's own state is as of
    /// this index): they will not be delivered again.
    pub fn set_applied(&mut self, index: u64) {
        if index > self.delivered {
            self.delivered = index.min(self.st.last_index());
        }
    }
}
