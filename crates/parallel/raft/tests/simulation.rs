//! Raft under a hostile network: random message loss, reordering and delay, partitions, and nodes that crash and restart
//! from what they stored; with log compaction and snapshots, and with changes of membership. After every step the safety
//! properties are checked (at most one leader per term; no two nodes commit different entries at one index; the state machine
//! of every node, whether it applied the entries or installed a snapshot, is the same as every other node's at the same
//! index; committed entries are delivered in order and never retracted); at the end the network heals and the group must
//! elect a leader, converge, and commit new commands.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use tsql_raft::*;

#[derive(Clone)]
struct Shared(Rc<RefCell<MemStorage>>);
impl Storage for Shared {
    fn hard_state(&self) -> (u64, Option<u64>) {
        self.0.borrow().hard_state()
    }
    fn set_hard_state(&mut self, t: u64, v: Option<u64>) {
        self.0.borrow_mut().set_hard_state(t, v)
    }
    fn last_index(&self) -> u64 {
        self.0.borrow().last_index()
    }
    fn term_at(&self, i: u64) -> Option<u64> {
        self.0.borrow().term_at(i)
    }
    fn entries(&self, a: u64, b: u64) -> Vec<Entry> {
        self.0.borrow().entries(a, b)
    }
    fn append(&mut self, e: &[Entry]) {
        self.0.borrow_mut().append(e)
    }
    fn snap_meta(&self) -> Option<SnapMeta> {
        self.0.borrow().snap_meta()
    }
    fn snap_data(&self) -> Vec<u8> {
        self.0.borrow().snap_data()
    }
    fn compact(&mut self, m: SnapMeta, d: Vec<u8>) {
        self.0.borrow_mut().compact(m, d)
    }
    fn install_snapshot(&mut self, m: SnapMeta, d: Vec<u8>) {
        self.0.borrow_mut().install_snapshot(m, d)
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn chance(&mut self, per_mille: u64) -> bool {
        self.next() % 1000 < per_mille
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The replicated state machine of the simulation: how many entries it applied and a hash of all of them, so that two
/// machines that applied the same entries (or installed a snapshot of such a machine) are equal.
#[derive(Clone, Default, Debug, PartialEq)]
struct Machine {
    applied: u64,
    hash: u64,
}

impl Machine {
    fn apply(&mut self, e: &Entry) {
        let mut x = self.hash ^ e.index.wrapping_mul(0x9E3779B97F4A7C15) ^ e.term.rotate_left(17);
        for b in &e.data {
            x = x.wrapping_mul(0x100000001B3) ^ *b as u64;
        }
        if e.conf.is_some() {
            x ^= 0xC0FFEE;
        }
        self.hash = x.rotate_left(5) ^ (x >> 3);
        self.applied = e.index;
    }
    fn encode(&self) -> Vec<u8> {
        let mut v = self.applied.to_le_bytes().to_vec();
        v.extend_from_slice(&self.hash.to_le_bytes());
        v
    }
    fn decode(b: &[u8]) -> Machine {
        Machine { applied: u64::from_le_bytes(b[..8].try_into().unwrap()), hash: u64::from_le_bytes(b[8..16].try_into().unwrap()) }
    }
}

struct Sim {
    n: usize,
    /// the nodes that are voters at the start; the others are spares that join later
    initial: usize,
    stores: Vec<Shared>,
    nodes: Vec<Option<Raft<Shared>>>,
    machines: Vec<Machine>,
    net: VecDeque<(u64, u64, Msg)>,
    groups: Vec<usize>,
    chosen: HashMap<u64, Entry>,
    /// the state of the machine after each index, as the first node to reach it computed it
    states: HashMap<u64, u64>,
    leaders: HashMap<u64, u64>,
    rng: Rng,
    next_cmd: u32,
    proposing: bool,
    snapshots_installed: u64,
    compactions: u64,
}

impl Sim {
    fn new(n: usize, seed: u64) -> Sim {
        Sim::with_spares(n, 0, seed)
    }

    fn with_spares(initial: usize, spares: usize, seed: u64) -> Sim {
        let n = initial + spares;
        let stores: Vec<Shared> = (0..n).map(|_| Shared(Rc::new(RefCell::new(MemStorage::new())))).collect();
        let mut s = Sim {
            n,
            initial,
            stores,
            nodes: vec![],
            machines: vec![Machine::default(); n],
            net: VecDeque::new(),
            groups: vec![0; n],
            chosen: HashMap::new(),
            states: HashMap::new(),
            leaders: HashMap::new(),
            rng: Rng(seed | 1),
            next_cmd: 0,
            proposing: true,
            snapshots_installed: 0,
            compactions: 0,
        };
        for i in 0..n {
            let node = s.make(i, seed);
            s.nodes.push(Some(node));
        }
        s
    }

    fn make(&self, i: usize, seed: u64) -> Raft<Shared> {
        let join = i >= self.initial;
        let peers = if join { vec![] } else { (0..self.initial as u64).filter(|p| *p != i as u64).collect() };
        Raft::new(Config { id: i as u64, peers, election_ticks: 10, heartbeat_ticks: 2, seed: seed.wrapping_add(i as u64 * 7919), join }, self.stores[i].clone())
    }

    /// A node that starts again knows what its snapshot holds (the state machine is at least that far, by the contract of
    /// `compact`), and is given the log after it again.
    fn restart(&mut self, i: usize, seed: u64) {
        self.machines[i] = match self.stores[i].snap_meta() {
            Some(m) => Machine::decode(&self.stores[i].snap_data()).tap(|d| assert_eq!(d.applied, m.index)),
            None => Machine::default(),
        };
        self.nodes[i] = Some(self.make(i, seed));
    }

    fn collect(&mut self, i: usize) {
        let Some(node) = self.nodes[i].as_mut() else { return };
        let r = node.ready();
        for (to, m) in r.messages {
            self.net.push_back((i as u64, to, m));
        }
        if let Some(snap) = r.snapshot {
            let m = Machine::decode(&snap.data);
            assert_eq!(m.applied, snap.index, "a snapshot says what index it is of");
            assert!(snap.index > self.machines[i].applied, "node {} was given a snapshot of {} but is at {}", i, snap.index, self.machines[i].applied);
            match self.states.get(&snap.index) {
                Some(h) => assert_eq!(*h, m.hash, "node {} installed a snapshot of index {} that differs from what was applied", i, snap.index),
                None => {
                    self.states.insert(snap.index, m.hash);
                }
            }
            self.machines[i] = m;
            self.snapshots_installed += 1;
        }
        for e in r.committed {
            // in order, once
            let prev = self.machines[i].applied;
            assert_eq!(e.index, prev + 1, "node {} was given commit {} after {}", i, e.index, prev);
            // no two nodes commit different entries at an index
            match self.chosen.get(&e.index) {
                Some(c) => assert_eq!(*c, e, "nodes disagree about the entry at index {}", e.index),
                None => {
                    self.chosen.insert(e.index, e.clone());
                }
            }
            self.machines[i].apply(&e);
            match self.states.get(&e.index) {
                Some(h) => assert_eq!(*h, self.machines[i].hash, "node {} has a different state after index {}", i, e.index),
                None => {
                    self.states.insert(e.index, self.machines[i].hash);
                }
            }
        }
        let node = self.nodes[i].as_ref().unwrap();
        if node.is_leader() {
            let t = node.term();
            match self.leaders.get(&t) {
                Some(l) => assert_eq!(*l, i as u64, "two leaders in term {}", t),
                None => {
                    self.leaders.insert(t, i as u64);
                }
            }
        }
    }

    fn connected(&self, a: u64, b: u64) -> bool {
        self.groups[a as usize] == self.groups[b as usize]
    }

    /// Take a snapshot of node `i`'s state machine and let it drop the log up to it.
    fn compact(&mut self, i: usize) {
        let m = self.machines[i].clone();
        if let Some(nd) = self.nodes[i].as_mut() {
            if nd.compact(m.applied, m.encode()) {
                self.compactions += 1;
            }
        }
    }

    fn step(&mut self, chaos: bool) {
        for i in 0..self.n {
            if let Some(nd) = self.nodes[i].as_mut() {
                nd.tick();
            }
            self.collect(i);
        }
        // deliver (a snapshot of) the messages in flight, in random order
        let mut batch: Vec<_> = self.net.drain(..).collect();
        for k in (1..batch.len()).rev() {
            let j = self.rng.below(k + 1);
            batch.swap(k, j);
        }
        for (from, to, m) in batch {
            if chaos && self.rng.chance(60) {
                continue; // lost
            }
            if chaos && self.rng.chance(150) {
                self.net.push_back((from, to, m)); // delayed
                continue;
            }
            if !self.connected(from, to) {
                continue;
            }
            if let Some(nd) = self.nodes[to as usize].as_mut() {
                nd.step(from, m);
            }
            self.collect(to as usize);
        }
        if chaos {
            if self.rng.chance(15) {
                let i = self.rng.below(self.n);
                if self.nodes[i].is_some() {
                    self.nodes[i] = None; // crash: volatile state is gone, the storage stays
                }
            }
            if self.rng.chance(40) {
                let i = self.rng.below(self.n);
                if self.nodes[i].is_none() {
                    let seed = self.rng.next();
                    self.restart(i, seed);
                }
            }
            if self.rng.chance(8) {
                let cut = 1 + self.rng.below(self.n - 1);
                for g in self.groups.iter_mut() {
                    *g = 0;
                }
                let mut moved = HashSet::new();
                while moved.len() < cut {
                    moved.insert(self.rng.below(self.n));
                }
                for m in moved {
                    self.groups[m] = 1;
                }
            }
            if self.rng.chance(20) {
                self.groups = vec![0; self.n];
            }
        }
        // nodes take snapshots now and then (what a state machine does when its log has grown)
        if self.rng.chance(25) {
            let i = self.rng.below(self.n);
            let start = self.nodes[i].as_ref().map(|nd| nd.snapshot_index()).unwrap_or(0);
            if self.machines[i].applied > start + 3 {
                self.compact(i);
            }
        }
        // clients propose to whoever says it is the leader
        if self.proposing && self.rng.chance(300) {
            if let Some(i) = self.leader() {
                self.next_cmd += 1;
                let cmd = self.next_cmd.to_le_bytes().to_vec();
                let _ = self.nodes[i].as_mut().unwrap().propose(cmd);
                self.collect(i);
            }
        }
    }

    fn leader(&self) -> Option<usize> {
        (0..self.n).find(|i| self.nodes[*i].as_ref().map(|n| n.is_leader()).unwrap_or(false))
    }

    /// The leader changes the membership: add a spare as a learner, promote a caught-up learner, or remove a voter.
    fn change_membership(&mut self) {
        let Some(l) = self.leader() else { return };
        let (conf, last, progress) = {
            let nd = self.nodes[l].as_ref().unwrap();
            (nd.conf().clone(), nd.last_index(), nd.progress())
        };
        let next = match self.rng.below(3) {
            0 => {
                let spare = (0..self.n as u64).find(|id| !conf.is_voter(*id) && !conf.is_learner(*id));
                spare.map(|id| conf.with_learner(Member { id, addr: format!("n{}", id) }))
            }
            1 => conf.learners.iter().find(|m| progress.iter().any(|(p, matched, _)| *p == m.id && *matched + 3 >= last)).map(|m| conf.with_promoted(m.id)),
            _ => {
                if conf.voters.len() >= 4 {
                    let victim = conf.voters[self.rng.below(conf.voters.len())].id;
                    Some(conf.without(victim))
                } else {
                    None
                }
            }
        };
        if let Some(c) = next {
            let _ = self.nodes[l].as_mut().unwrap().propose_conf(c, vec![]);
            self.collect(l);
        }
    }
}

trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}
impl<T> Tap for T {}

fn heal(s: &mut Sim) {
    s.proposing = false;
    for i in 0..s.n {
        if s.nodes[i].is_none() {
            let sd = s.rng.next();
            s.restart(i, sd);
        }
    }
    s.groups = vec![0; s.n];
    for _ in 0..500 {
        s.step(false);
    }
}

fn run(n: usize, seed: u64, steps: usize) {
    let mut s = Sim::new(n, seed);
    for _ in 0..steps {
        s.step(true);
    }
    heal(&mut s);
    let leader = s.leader().unwrap_or_else(|| panic!("seed {}: no leader after the network healed", seed));
    // a new command commits on every node
    let before = s.chosen.len();
    s.next_cmd += 1;
    let cmd = s.next_cmd.to_le_bytes().to_vec();
    s.nodes[leader].as_mut().unwrap().propose(cmd.clone()).unwrap();
    s.collect(leader);
    for _ in 0..200 {
        s.step(false);
    }
    assert!(s.chosen.len() > before, "seed {}: the new command did not commit", seed);
    // every node ends in the same state (restarted nodes were given the log again, or a snapshot)
    let top = s.machines.iter().map(|m| m.applied).max().unwrap();
    for i in 0..n {
        assert_eq!(s.machines[i], s.machines[0], "seed {}: node {} did not converge", seed, i);
    }
    assert_eq!(s.machines[0].applied, top);
    assert!(s.chosen.values().any(|e| e.data == cmd));
}

#[test]
fn three_nodes_under_chaos() {
    let mut compactions = 0;
    let mut installs = 0;
    for seed in 1..=40 {
        let mut s = Sim::new(3, seed);
        for _ in 0..1500 {
            s.step(true);
        }
        compactions += s.compactions;
        installs += s.snapshots_installed;
        drop(s);
        run(3, seed, 1500);
    }
    assert!(compactions > 100, "the runs must compact the log ({} times)", compactions);
    assert!(installs > 0, "the runs must send snapshots to followers that fell behind the log ({} installs)", installs);
}

#[test]
fn five_nodes_under_chaos() {
    for seed in 100..=130 {
        run(5, seed, 2000);
    }
}

/// Three voters and two spares; the leader adds spares as learners, promotes them, and removes voters, while the network
/// loses and reorders messages, partitions, and nodes crash and restart.
fn run_membership(seed: u64, steps: usize) -> Sim {
    let mut s = Sim::with_spares(3, 2, seed);
    for k in 0..steps {
        s.step(true);
        if k % 40 == 7 && s.rng.chance(700) {
            s.change_membership();
        }
    }
    heal(&mut s);
    let leader = s.leader().unwrap_or_else(|| panic!("seed {}: no leader after the network healed", seed));
    let conf = s.nodes[leader].as_ref().unwrap().conf().clone();
    assert!(conf.is_voter(leader as u64), "seed {}: the leader {} is not a voter of {:?}", seed, leader, conf);
    // the group as it now is takes a command and every member has it
    let before = s.chosen.len();
    s.next_cmd += 1;
    let cmd = s.next_cmd.to_le_bytes().to_vec();
    s.nodes[leader].as_mut().unwrap().propose(cmd.clone()).unwrap();
    s.collect(leader);
    for _ in 0..300 {
        s.step(false);
    }
    assert!(s.chosen.len() > before, "seed {}: the new command did not commit under {:?}", seed, conf);
    for id in conf.ids() {
        assert_eq!(s.machines[id as usize], s.machines[leader], "seed {}: member {} of {:?} did not converge", seed, id, conf);
    }
    s
}

#[test]
fn membership_changes_under_chaos() {
    let mut changes = 0;
    for seed in 200..=260 {
        let s = run_membership(seed, 2500);
        changes += s.chosen.values().filter(|e| e.conf.is_some()).count();
    }
    assert!(changes > 100, "the runs must change the membership ({} configuration entries)", changes);
}

#[test]
fn a_single_node_leads_itself() {
    let mut s = Sim::new(1, 5);
    for _ in 0..50 {
        s.step(false);
    }
    let l = s.leader().expect("a group of one elects itself");
    s.nodes[l].as_mut().unwrap().propose(vec![9]).unwrap();
    s.collect(l);
    assert!(s.chosen.values().any(|e| e.data == vec![9]));
}

#[test]
fn a_follower_refuses_commands_and_says_who_leads() {
    let mut s = Sim::new(3, 11);
    for _ in 0..100 {
        s.step(false);
    }
    let l = s.leader().unwrap();
    let f = (0..3).find(|i| *i != l).unwrap();
    let e = s.nodes[f].as_mut().unwrap().propose(vec![1]).unwrap_err();
    assert_eq!(e.leader, Some(l as u64));
}

fn commit_some(s: &mut Sim, count: usize) {
    for _ in 0..count {
        let l = s.leader().expect("a leader");
        s.next_cmd += 1;
        s.nodes[l].as_mut().unwrap().propose(s.next_cmd.to_le_bytes().to_vec()).unwrap();
        s.collect(l);
        for _ in 0..3 {
            s.step(false);
        }
    }
}

#[test]
fn a_follower_that_fell_behind_the_log_is_caught_up_with_a_snapshot() {
    let mut s = Sim::new(3, 21);
    s.proposing = false;
    for _ in 0..100 {
        s.step(false);
    }
    let l = s.leader().unwrap();
    let lost = (0..3).find(|i| *i != l).unwrap();
    // one node is cut off while the others commit a lot, and then drop the log
    s.groups[lost] = 1;
    commit_some(&mut s, 300);
    for i in 0..3 {
        if i != lost {
            s.compact(i);
            assert!(s.nodes[i].as_ref().unwrap().snapshot_index() >= 290);
            assert!(s.nodes[i].as_ref().unwrap().log_len() < 20, "the log was dropped");
        }
    }
    assert_eq!(s.snapshots_installed, 0);
    s.groups = vec![0; 3];
    for _ in 0..200 {
        s.step(false);
    }
    assert!(s.snapshots_installed >= 1, "the follower is given the snapshot");
    assert_eq!(s.machines[lost], s.machines[l], "and ends in the leader's state");
    // it follows the log from there
    commit_some(&mut s, 5);
    for _ in 0..30 {
        s.step(false);
    }
    assert_eq!(s.machines[lost], s.machines[s.leader().unwrap()]);
    assert!(s.nodes[lost].as_ref().unwrap().log_len() < 20, "it never held the dropped entries");
}

#[test]
fn a_snapshot_larger_than_a_chunk_arrives_whole() {
    let mut s = Sim::new(3, 22);
    s.proposing = false;
    for _ in 0..100 {
        s.step(false);
    }
    let l = s.leader().unwrap();
    let lost = (0..3).find(|i| *i != l).unwrap();
    s.groups[lost] = 1;
    commit_some(&mut s, 20);
    // a state machine with a lot of state: the snapshot is much bigger than one message
    let mut big = s.machines[l].encode();
    big.extend(std::iter::repeat(7u8).take(1_000_000));
    let applied = s.machines[l].applied;
    for i in 0..3 {
        if i != lost {
            // (the simulation's machine only reads the first 16 bytes; the rest is ballast that must arrive intact)
            assert!(s.nodes[i].as_mut().unwrap().compact(applied, big.clone()));
        }
    }
    s.groups = vec![0; 3];
    for _ in 0..300 {
        s.step(false);
    }
    assert!(s.snapshots_installed >= 1);
    assert_eq!(s.stores[lost].snap_data().len(), big.len());
    assert!(s.stores[lost].snap_data() == big, "every byte of the snapshot arrived, in order");
}

#[test]
fn a_node_is_added_promoted_and_a_voter_removed_without_disturbing_the_group() {
    let mut s = Sim::with_spares(3, 2, 31);
    s.proposing = false;
    for _ in 0..100 {
        s.step(false);
    }
    commit_some(&mut s, 30);
    let l = s.leader().unwrap();
    // add node 3 as a learner: it gets the log, but is not counted
    let c = s.nodes[l].as_ref().unwrap().conf().with_learner(Member { id: 3, addr: "n3".into() });
    s.nodes[l].as_mut().unwrap().propose_conf(c, vec![]).unwrap();
    s.collect(l);
    for _ in 0..100 {
        s.step(false);
    }
    assert_eq!(s.machines[3], s.machines[l], "the learner is caught up");
    assert!(s.nodes[l].as_ref().unwrap().conf().is_learner(3));
    // a second change is refused until the first is committed ... and by then it is
    let c = s.nodes[l].as_ref().unwrap().conf().with_promoted(3);
    s.nodes[l].as_mut().unwrap().propose_conf(c, vec![]).unwrap();
    s.collect(l);
    for _ in 0..100 {
        s.step(false);
    }
    assert!(s.nodes[l].as_ref().unwrap().conf().is_voter(3));
    // two voters cannot be removed at once
    let conf = s.nodes[l].as_ref().unwrap().conf().clone();
    let two_gone = conf.without(conf.voters[0].id).without(conf.voters[1].id);
    assert_eq!(s.nodes[l].as_mut().unwrap().propose_conf(two_gone, vec![]), Err(ConfError::TooMany));
    // remove a follower: the others carry on and the removed node does not depose anybody, although it keeps timing out
    let gone = conf.voters.iter().map(|m| m.id).find(|id| *id != l as u64).unwrap();
    s.nodes[l].as_mut().unwrap().propose_conf(conf.without(gone), vec![]).unwrap();
    s.collect(l);
    for _ in 0..100 {
        s.step(false);
    }
    let term = s.nodes[l].as_ref().unwrap().term();
    assert!(!s.nodes[l].as_ref().unwrap().conf().is_voter(gone));
    for _ in 0..600 {
        s.step(false);
    }
    assert_eq!(s.leader(), Some(l));
    assert_eq!(s.nodes[l].as_ref().unwrap().term(), term, "a removed node must not raise the term");
    commit_some(&mut s, 5);
    // remove the leader itself: it hands over, and the rest elects another
    let conf = s.nodes[l].as_ref().unwrap().conf().clone();
    s.nodes[l].as_mut().unwrap().propose_conf(conf.without(l as u64), vec![]).unwrap();
    s.collect(l);
    for _ in 0..400 {
        s.step(false);
    }
    let new_leader = s.leader().expect("a new leader");
    assert_ne!(new_leader, l);
    assert!(s.nodes[new_leader].as_ref().unwrap().conf().is_voter(new_leader as u64));
    commit_some(&mut s, 5);
}

#[test]
fn only_one_membership_change_can_be_pending() {
    let mut s = Sim::with_spares(3, 2, 41);
    s.proposing = false;
    for _ in 0..100 {
        s.step(false);
    }
    let l = s.leader().unwrap();
    let c = s.nodes[l].as_ref().unwrap().conf().with_learner(Member { id: 3, addr: String::new() });
    s.nodes[l].as_mut().unwrap().propose_conf(c.clone(), vec![]).unwrap();
    let c2 = c.with_learner(Member { id: 4, addr: String::new() });
    assert_eq!(s.nodes[l].as_mut().unwrap().propose_conf(c2, vec![]), Err(ConfError::Busy));
}

#[test]
fn the_file_log_survives_a_restart_and_a_torn_tail() {
    let dir = std::env::temp_dir().join(format!("tsql_raft_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let e = |term, index, v: u8| Entry { term, index, data: vec![v], conf: None };
    {
        let mut st = FileStorage::open(&dir, false).unwrap();
        st.set_hard_state(3, Some(2));
        st.append(&[e(1, 1, 1), e(1, 2, 2), e(2, 3, 3)]);
        // a conflicting suffix replaces what followed index 1
        st.append(&[e(3, 2, 22)]);
        assert_eq!(st.last_index(), 2);
    }
    // a crash in the middle of writing a frame
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("raft.log")).unwrap();
        f.write_all(&[200, 0, 0, 0, 1, 2, 3]).unwrap();
    }
    let st = FileStorage::open(&dir, false).unwrap();
    assert_eq!(st.hard_state(), (3, Some(2)));
    assert_eq!(st.last_index(), 2);
    assert_eq!(st.entries(1, 3), vec![e(1, 1, 1), e(3, 2, 22)]);
    drop(st);
    let mut st = FileStorage::open(&dir, false).unwrap();
    st.append(&[e(3, 3, 33)]);
    drop(st);
    assert_eq!(FileStorage::open(&dir, false).unwrap().last_index(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_file_log_keeps_its_snapshot_and_the_entries_after_it() {
    let dir = std::env::temp_dir().join(format!("tsql_raft_snap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let e = |term, index, v: u8| Entry { term, index, data: vec![v], conf: None };
    let conf = Conf { voters: vec![Member { id: 1, addr: "a".into() }], learners: vec![] };
    {
        let mut st = FileStorage::open(&dir, false).unwrap();
        st.append(&[e(1, 1, 1), e(1, 2, 2), e(2, 3, 3), e(2, 4, 4)]);
        st.compact(SnapMeta { index: 2, term: 1, conf: conf.clone() }, vec![9, 9, 9]);
        assert_eq!(st.last_index(), 4);
        assert_eq!(st.term_at(2), Some(1), "the snapshot's own index keeps its term");
        assert_eq!(st.term_at(1), None, "what the snapshot replaced is gone");
        assert_eq!(st.entries(1, 5), vec![e(2, 3, 3), e(2, 4, 4)]);
        st.append(&[e(2, 5, 5)]);
    }
    let mut st = FileStorage::open(&dir, false).unwrap();
    assert_eq!(st.snap_meta(), Some(SnapMeta { index: 2, term: 1, conf: conf.clone() }));
    assert_eq!(st.snap_data(), vec![9, 9, 9]);
    assert_eq!(st.last_index(), 5);
    assert_eq!(st.entries(3, 6), vec![e(2, 3, 3), e(2, 4, 4), e(2, 5, 5)]);
    // a snapshot from the leader replaces the whole log
    st.install_snapshot(SnapMeta { index: 10, term: 4, conf: conf.clone() }, vec![1]);
    assert_eq!(st.last_index(), 10);
    assert!(st.entries(1, 20).is_empty());
    st.append(&[e(4, 11, 11)]);
    drop(st);
    let st = FileStorage::open(&dir, false).unwrap();
    assert_eq!(st.snap_meta().unwrap().index, 10);
    assert_eq!(st.last_index(), 11);
    assert_eq!(st.term_at(10), Some(4));
    // a crash between writing the snapshot and cutting the log leaves entries the snapshot holds: they are dropped on open
    drop(st);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("raft.log")).unwrap();
        // (a frame for an old entry, index 3, written after the snapshot of index 10 as a half-done rewrite would leave it)
        let body = postcard::to_allocvec(&e(2, 3, 3)).unwrap();
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        frame.extend_from_slice(&body);
        f.write_all(&frame).unwrap();
    }
    let st = FileStorage::open(&dir, false).unwrap();
    assert_eq!(st.last_index(), 11, "an entry before the snapshot is ignored");
    let _ = std::fs::remove_dir_all(&dir);
}
