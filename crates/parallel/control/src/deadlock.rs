//! Deadlocks across shards.
//!
//! A shard's engine sees the waits among its own transactions and breaks a cycle among them (error 1205). It cannot see a
//! cycle that runs through two shards: transaction A waits for B on shard 0 while B waits for A on shard 1. The control node
//! can: every few hundred milliseconds it asks each shard who waits for whom (in control session ids, which are the shards'
//! session names too), puts the edges together, and looks for a cycle. A cycle that is seen twice in a row (so that edges read
//! at different moments do not make up a cycle that never existed) is broken by choosing a victim, the youngest session of the
//! cycle (the one that has done least, roughly), and killing its statement on the shards it waits on: its lock wait ends, the
//! shard rolls its part back, and the control session turns that into error 1205 and rolls the transaction back everywhere,
//! as SQL Server does for the victim of a deadlock. The others go on. The lock timeout remains as a last resort for waits
//! that are not cycles.
use crate::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// The groups of sessions that wait for each other in a cycle: the strongly connected components with more than one member
/// (or a session that waits for itself), each sorted.
pub fn cycles(edges: &[(u64, u64)]) -> Vec<Vec<u64>> {
    let mut adj: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    for (w, h) in edges {
        adj.entry(*w).or_default().insert(*h);
        adj.entry(*h).or_default();
    }
    let reach = |from: u64| -> HashSet<u64> {
        let mut seen = HashSet::new();
        let mut stack: Vec<u64> = adj[&from].iter().copied().collect();
        while let Some(n) = stack.pop() {
            if seen.insert(n) {
                stack.extend(adj[&n].iter().copied());
            }
        }
        seen
    };
    let reachable: BTreeMap<u64, HashSet<u64>> = adj.keys().map(|k| (*k, reach(*k))).collect();
    let mut out: Vec<Vec<u64>> = vec![];
    let mut done: HashSet<u64> = HashSet::new();
    for (n, r) in &reachable {
        if done.contains(n) || !r.contains(n) {
            continue;
        }
        // everything that reaches n and is reached from it
        let mut comp: Vec<u64> = r.iter().copied().filter(|m| reachable[m].contains(n)).collect();
        comp.push(*n);
        comp.sort_unstable();
        comp.dedup();
        done.extend(comp.iter().copied());
        out.push(comp);
    }
    out
}

impl Cluster {
    /// How often (milliseconds) the control node looks for deadlocks across shards; 0 turns the search off (the lock timeout
    /// then ends such a deadlock). Default 200.
    pub fn set_deadlock_check_ms(&self, ms: u64) {
        self.deadlock_check_ms.store(ms, Ordering::Relaxed);
    }

    /// How many deadlocks across shards have been broken.
    pub fn deadlocks_broken(&self) -> u64 {
        self.deadlocks.load(Ordering::Relaxed)
    }

    /// Who waits for whom, over all shards: (shard, waiter session, holder session).
    async fn wait_edges(&self) -> Vec<(usize, u64, u64)> {
        let shards = self.shard_list();
        let polls = join_all(shards.iter().map(|s| s.waits())).await;
        let mut v = vec![];
        for (i, p) in polls.into_iter().enumerate() {
            if let Ok(edges) = p {
                v.extend(edges.into_iter().map(|(w, h)| (i, w, h)));
            }
        }
        v
    }

    /// One look: the cycles of waits now, each with the shards on which its members wait.
    fn look_for_deadlocks(&self) -> Vec<(Vec<u64>, Vec<(usize, u64)>)> {
        let edges = self.rt.block_on(self.wait_edges());
        if edges.is_empty() {
            return vec![];
        }
        let plain: Vec<(u64, u64)> = edges.iter().map(|(_, w, h)| (*w, *h)).collect();
        cycles(&plain)
            .into_iter()
            .map(|c| {
                let waits_on: Vec<(usize, u64)> = edges.iter().filter(|(_, w, h)| c.contains(w) && c.contains(h)).map(|(s, w, _)| (*s, *w)).collect();
                (c, waits_on)
            })
            .collect()
    }

    /// Break a deadlock: kill the victim's wait on the shards where it waits.
    fn break_deadlock(&self, cycle: &[u64], waits_on: &[(usize, u64)]) {
        let victim = *cycle.iter().max().expect("a cycle has members");
        self.victims.lock().unwrap_or_else(|e| e.into_inner()).insert(victim);
        let targets: Vec<usize> = waits_on.iter().filter(|(_, w)| *w == victim).map(|(s, _)| *s).collect();
        let shards = self.shard_list();
        let clients: Vec<ShardClient> = targets.iter().filter_map(|&i| shards.get(i).cloned()).collect();
        self.rt.block_on(join_all(clients.iter().map(|c| c.kill(victim))));
        self.deadlocks.fetch_add(1, Ordering::Relaxed);
    }

    /// The watcher: a thread that looks for deadlocks until the cluster is dropped.
    pub(crate) fn start_deadlock_monitor(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("deadlock-monitor".into()).spawn(move || {
            let mut previous: HashSet<Vec<u64>> = HashSet::new();
            loop {
                let Some(c) = weak.upgrade() else { return };
                let ms = c.deadlock_check_ms.load(Ordering::Relaxed);
                if ms == 0 {
                    drop(c);
                    previous.clear();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    continue;
                }
                let found = c.look_for_deadlocks();
                let mut now: HashSet<Vec<u64>> = HashSet::new();
                for (cycle, waits_on) in &found {
                    if previous.contains(cycle) {
                        c.break_deadlock(cycle, waits_on);
                    } else {
                        now.insert(cycle.clone());
                    }
                }
                previous = now;
                drop(c);
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::cycles;

    #[test]
    fn finds_cycles_and_only_cycles() {
        assert!(cycles(&[]).is_empty());
        assert!(cycles(&[(1, 2), (2, 3)]).is_empty(), "a chain of waits is not a deadlock");
        assert_eq!(cycles(&[(1, 2), (2, 1)]), vec![vec![1, 2]]);
        assert_eq!(cycles(&[(1, 2), (2, 3), (3, 1), (4, 1)]), vec![vec![1, 2, 3]], "4 waits for the cycle but is not in it");
        assert_eq!(cycles(&[(1, 2), (2, 1), (5, 6), (6, 5)]), vec![vec![1, 2], vec![5, 6]]);
        assert_eq!(cycles(&[(7, 7)]), vec![vec![7]]);
    }
}
