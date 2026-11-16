//! Raft under a hostile network: random message loss, reordering and delay, partitions, and nodes that crash and restart
//! from what they stored. After every step the safety properties are checked (at most one leader per term; no two nodes
//! commit different entries at one index; committed entries are delivered in order and never retracted); at the end the
//! network heals and the group must elect a leader, converge, and commit new commands.
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

struct Sim {
    n: usize,
    stores: Vec<Shared>,
    nodes: Vec<Option<Raft<Shared>>>,
    /// what each node has been told is committed (rebuilt from the start after a restart)
    committed: Vec<Vec<Entry>>,
    net: VecDeque<(u64, u64, Msg)>,
    groups: Vec<usize>,
    chosen: HashMap<u64, Entry>,
    leaders: HashMap<u64, u64>,
    rng: Rng,
    next_cmd: u32,
    proposing: bool,
}

impl Sim {
    fn new(n: usize, seed: u64) -> Sim {
        let stores: Vec<Shared> = (0..n).map(|_| Shared(Rc::new(RefCell::new(MemStorage::new())))).collect();
        let mut s = Sim { n, stores, nodes: vec![], committed: vec![vec![]; n], net: VecDeque::new(), groups: vec![0; n], chosen: HashMap::new(), leaders: HashMap::new(), rng: Rng(seed | 1), next_cmd: 0, proposing: true };
        for i in 0..n {
            let node = s.make(i, seed);
            s.nodes.push(Some(node));
        }
        s
    }

    fn make(&self, i: usize, seed: u64) -> Raft<Shared> {
        let peers = (0..self.n as u64).filter(|p| *p != i as u64).collect();
        Raft::new(Config { id: i as u64, peers, election_ticks: 10, heartbeat_ticks: 2, seed: seed.wrapping_add(i as u64 * 7919) }, self.stores[i].clone())
    }

    fn collect(&mut self, i: usize) {
        let Some(node) = self.nodes[i].as_mut() else { return };
        let r = node.ready();
        for (to, m) in r.messages {
            self.net.push_back((i as u64, to, m));
        }
        for e in r.committed {
            // in order, once
            let prev = self.committed[i].last().map(|x| x.index).unwrap_or(0);
            assert_eq!(e.index, prev + 1, "node {} was given commit {} after {}", i, e.index, prev);
            // no two nodes commit different entries at an index
            match self.chosen.get(&e.index) {
                Some(c) => assert_eq!(*c, e, "nodes disagree about the entry at index {}", e.index),
                None => {
                    self.chosen.insert(e.index, e.clone());
                }
            }
            self.committed[i].push(e);
        }
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
                    self.committed[i].clear();
                }
            }
            if self.rng.chance(40) {
                let i = self.rng.below(self.n);
                if self.nodes[i].is_none() {
                    let seed = self.rng.next();
                    self.nodes[i] = Some(self.make(i, seed));
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
}

fn run(n: usize, seed: u64, steps: usize) {
    let mut s = Sim::new(n, seed);
    for _ in 0..steps {
        s.step(true);
    }
    // heal everything: restart the dead, reconnect, no more loss and no more commands
    s.proposing = false;
    for i in 0..n {
        if s.nodes[i].is_none() {
            let sd = s.rng.next();
            s.nodes[i] = Some(s.make(i, sd));
        }
    }
    s.groups = vec![0; n];
    for _ in 0..400 {
        s.step(false);
    }
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
    // every node ends with the same committed log (restarted nodes were given it all again)
    let longest = s.committed.iter().map(|c| c.len()).max().unwrap();
    for i in 0..n {
        assert_eq!(s.committed[i].len(), longest, "seed {}: node {} did not catch up", seed, i);
        assert_eq!(s.committed[i], s.committed[0], "seed {}: node {} has a different log", seed, i);
    }
    assert!(s.committed[0].iter().any(|e| e.data == cmd));
}

#[test]
fn three_nodes_under_chaos() {
    for seed in 1..=40 {
        run(3, seed, 1500);
    }
}

#[test]
fn five_nodes_under_chaos() {
    for seed in 100..=130 {
        run(5, seed, 2000);
    }
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
    assert!(s.committed[0].iter().any(|e| e.data == vec![9]));
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

#[test]
fn the_file_log_survives_a_restart_and_a_torn_tail() {
    let dir = std::env::temp_dir().join(format!("tsql_raft_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut st = FileStorage::open(&dir, false).unwrap();
        st.set_hard_state(3, Some(2));
        st.append(&[Entry { term: 1, index: 1, data: vec![1] }, Entry { term: 1, index: 2, data: vec![2] }, Entry { term: 2, index: 3, data: vec![3] }]);
        // a conflicting suffix replaces what followed index 1
        st.append(&[Entry { term: 3, index: 2, data: vec![22] }]);
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
    assert_eq!(st.entries(1, 3), vec![Entry { term: 1, index: 1, data: vec![1] }, Entry { term: 3, index: 2, data: vec![22] }]);
    drop(st);
    let mut st = FileStorage::open(&dir, false).unwrap();
    st.append(&[Entry { term: 3, index: 3, data: vec![33] }]);
    drop(st);
    assert_eq!(FileStorage::open(&dir, false).unwrap().last_index(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}
