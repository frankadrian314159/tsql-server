//! A small Raft consensus core.
//!
//! [`Raft`] is a pure state machine: it is driven by [`Raft::tick`] (the passage of time), [`Raft::step`] (a message from a
//! peer) and [`Raft::propose`] (a command from a client), and it hands back what to send and what has been committed with
//! [`Raft::ready`]. It does no I/O of its own except through its [`Storage`] (the log and the vote, which must be durable
//! before a message that depends on them leaves), so it can be tested deterministically with a simulated network (see
//! `tests/simulation.rs`) and driven by real sockets and timers in production.
//!
//! What it implements: leader election with randomized timeouts, log replication with fast back-off, commitment of entries by
//! a majority (a leader commits only entries of its own term by counting, as the Raft paper requires, and starts its
//! term with an empty entry for that reason), a leader lease for reads, and a file-backed log. What it does not (yet): log
//! compaction and snapshots (the log only grows; a replica that is far behind is caught up from the log), and changes of
//! membership (the group is fixed).
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub mod storage;
pub use storage::{FileStorage, MemStorage, Storage};

pub type NodeId = u64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub term: u64,
    pub index: u64,
    /// the command; empty for the entry a new leader starts its term with
    pub data: Vec<u8>,
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
    /// the other members of the group
    pub peers: Vec<NodeId>,
    /// ticks without hearing from a leader before an election starts (randomized between min and 2*min)
    pub election_ticks: u32,
    pub heartbeat_ticks: u32,
    /// seeds the randomization of the election timeout (so a simulation is repeatable)
    pub seed: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct NotLeader {
    pub leader: Option<NodeId>,
}

/// What to do after a step: messages to send, and entries that became committed (in order, each exactly once).
#[derive(Default, Debug)]
pub struct Ready {
    pub messages: Vec<(NodeId, Msg)>,
    pub committed: Vec<Entry>,
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
}

impl<S: Storage> Raft<S> {
    pub fn new(cfg: Config, st: S) -> Raft<S> {
        let (term, vote) = st.hard_state();
        let mut r = Raft {
            rng: cfg.seed.wrapping_mul(0x9E3779B97F4A7C15) | 1,
            cfg,
            st,
            role: Role::Follower,
            term,
            vote,
            leader: None,
            commit: 0,
            delivered: 0,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            votes: HashSet::new(),
            election_elapsed: 0,
            election_timeout: 0,
            heartbeat_elapsed: 0,
            now: 0,
            acked: HashMap::new(),
            out: vec![],
        };
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
    /// Whether the log is committed up to an entry of the current term: a new leader cannot serve requests safely before
    /// the empty entry that starts its term is committed (until then it may not know everything that was committed).
    pub fn has_committed_in_term(&self) -> bool {
        self.commit > 0 && self.st.term_at(self.commit) == Some(self.term)
    }

    /// For each peer, how far the leader knows its log matches and the next index it will send (a leader only).
    pub fn progress(&self) -> Vec<(NodeId, u64, u64)> {
        self.cfg.peers.iter().map(|p| (*p, *self.match_index.get(p).unwrap_or(&0), *self.next_index.get(p).unwrap_or(&0))).collect()
    }

    pub fn storage(&self) -> &S {
        &self.st
    }

    fn quorum(&self) -> usize {
        (self.cfg.peers.len() + 1) / 2 + 1
    }

    /// Whether this node is the leader and a majority has answered it recently: while it holds, no other leader can have
    /// been elected (an election needs a majority that heard nothing from this one for a full election timeout), so a read
    /// it serves is not stale.
    pub fn lease_valid(&self) -> bool {
        if self.role != Role::Leader {
            return false;
        }
        let window = self.cfg.election_ticks as u64;
        let fresh = self.acked.values().filter(|t| self.now.saturating_sub(**t) < window).count() + 1;
        fresh >= self.quorum()
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
        if self.votes.len() >= self.quorum() {
            self.become_leader();
            return;
        }
        let last_index = self.st.last_index();
        let last_term = self.st.term_at(last_index).unwrap_or(0);
        for p in self.cfg.peers.clone() {
            self.out.push((p, Msg::Vote { term: self.term, last_index, last_term }));
        }
    }

    fn become_leader(&mut self) {
        self.role = Role::Leader;
        self.leader = Some(self.cfg.id);
        let next = self.st.last_index() + 1;
        self.next_index = self.cfg.peers.iter().map(|p| (*p, next)).collect();
        self.match_index = self.cfg.peers.iter().map(|p| (*p, 0)).collect();
        self.acked.clear();
        self.heartbeat_elapsed = 0;
        // an entry of the new term: committing it commits everything before it
        let e = Entry { term: self.term, index: next, data: vec![] };
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
                    self.become_candidate();
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
        self.st.append(&[Entry { term: self.term, index, data }]);
        self.broadcast_append();
        self.advance_commit();
        Ok(index)
    }

    fn send_append(&mut self, to: NodeId) {
        let next = *self.next_index.get(&to).unwrap_or(&1);
        let prev_index = next - 1;
        let prev_term = self.st.term_at(prev_index).unwrap_or(0);
        let last = self.st.last_index();
        let entries = if next <= last { self.st.entries(next, (next + 256).min(last + 1)) } else { vec![] };
        self.out.push((to, Msg::Append { term: self.term, prev_index, prev_term, entries, commit: self.commit }));
    }

    fn broadcast_append(&mut self) {
        for p in self.cfg.peers.clone() {
            self.send_append(p);
        }
    }

    fn advance_commit(&mut self) {
        // the highest index replicated on a majority, if it is of the current term
        let mut idx: Vec<u64> = self.cfg.peers.iter().map(|p| *self.match_index.get(p).unwrap_or(&0)).collect();
        idx.push(self.st.last_index());
        idx.sort_unstable_by(|a, b| b.cmp(a));
        let n = idx[self.quorum() - 1];
        if n > self.commit && self.st.term_at(n) == Some(self.term) {
            self.commit = n;
        }
    }

    /// A message from a peer.
    pub fn step(&mut self, from: NodeId, msg: Msg) {
        let term = match &msg {
            Msg::Vote { term, .. } | Msg::VoteReply { term, .. } | Msg::Append { term, .. } | Msg::AppendReply { term, .. } => *term,
        };
        if term > self.term {
            let leader = if matches!(msg, Msg::Append { .. }) { Some(from) } else { None };
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
                if self.role == Role::Candidate && term == self.term && granted {
                    self.votes.insert(from);
                    if self.votes.len() >= self.quorum() {
                        self.become_leader();
                    }
                }
            }
            Msg::Append { term, prev_index, prev_term, entries, commit } => {
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
                    self.st.append(&to_append); // truncates a conflicting suffix
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
                    if match_index < self.st.last_index() {
                        self.send_append(from);
                    }
                } else {
                    let next = (match_index + 1).max(1);
                    self.next_index.insert(from, next);
                    self.send_append(from);
                }
            }
        }
    }

    /// Take what is to be sent, and the entries committed since the last call.
    pub fn ready(&mut self) -> Ready {
        let messages = std::mem::take(&mut self.out);
        let mut committed = vec![];
        if self.commit > self.delivered {
            committed = self.st.entries(self.delivered + 1, self.commit + 1);
            self.delivered = self.commit;
        }
        Ready { messages, committed }
    }

    /// Entries up to this index have been applied by the caller already (after a restart, the caller's own state is as of
    /// this index): they will not be delivered again.
    pub fn set_applied(&mut self, index: u64) {
        if index > self.delivered {
            self.delivered = index.min(self.st.last_index());
        }
    }
}
