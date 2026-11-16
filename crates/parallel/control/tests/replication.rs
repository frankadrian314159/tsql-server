//! A replicated shard: three replicas (durable engines, Raft logs on disk, Arrow Flight between them) behind one control
//! node. Writes go through the leader and are complete when a majority has them; the leader can die and the data and the
//! service survive; a replica that was down catches up; without a majority nothing is served.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{Server, Value};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn_on, ReplicaConfig, RunningShard, ShardNode};

/// The groups of replicas of these tests keep time with timeouts of a few hundred milliseconds, so they take turns: several
/// groups at once on a loaded machine would hold elections that have nothing to do with what a test is checking.
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Group {
    _turn: std::sync::MutexGuard<'static, ()>,
    rt: tokio::runtime::Runtime,
    dir: PathBuf,
    addrs: Vec<String>,
    nodes: Vec<Option<RunningShard>>,
    /// the log is cut after this many applied entries (0: never)
    snapshot_every: u64,
    /// nodes that joined an existing group (started with `join`)
    joined: Vec<usize>,
}

fn unique_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("tsql_repl_{}_{}_{}", tag, std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    d
}

impl Group {
    fn start(n: usize, tag: &str) -> Group {
        Group::start_with(n, tag, 0)
    }

    fn start_with(n: usize, tag: &str, snapshot_every: u64) -> Group {
        let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = unique_dir(tag);
        let listeners: Vec<tokio::net::TcpListener> = (0..n).map(|_| rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap()).collect();
        let addrs: Vec<String> = listeners.iter().map(|l| l.local_addr().unwrap().to_string()).collect();
        let mut g = Group { _turn: turn, rt, dir, addrs, nodes: (0..n).map(|_| None).collect(), snapshot_every, joined: vec![] };
        for (i, l) in listeners.into_iter().enumerate() {
            g.start_node(i, l);
        }
        g
    }

    fn start_node(&mut self, i: usize, listener: tokio::net::TcpListener) {
        let peers: Vec<(u64, String)> = (0..self.addrs.len()).filter(|j| *j != i).map(|j| (j as u64 + 1, self.addrs[j].clone())).collect();
        let mut cfg = ReplicaConfig::new(i as u64 + 1, peers, self.dir.join(format!("raft{}", i)));
        cfg.tick_ms = 10;
        cfg.election_ticks = 30;
        cfg.heartbeat_ticks = 3;
        cfg.sync = false;
        cfg.addr = self.addrs[i].clone();
        cfg.snapshot_every = self.snapshot_every;
        cfg.join = self.joined.contains(&i);
        let server = Server::open(self.dir.join(format!("data{}", i))).unwrap();
        let running = self.rt.block_on(async {
            let node = ShardNode::replicated(0, server, cfg).unwrap();
            spawn_on(node, listener).await.unwrap()
        });
        self.nodes[i] = Some(running);
    }

    /// Stop a replica for good (its files stay).
    fn kill(&mut self, i: usize) {
        if let Some(n) = self.nodes[i].take() {
            self.rt.block_on(n.shutdown());
            std::thread::sleep(Duration::from_millis(150)); // the engine's directory lock is released when the threads let go
        }
    }

    /// Start one more replica that is not yet part of the group (`tsql-shard --join`); returns its index (its node id is the
    /// index plus one) and its address.
    fn start_joiner(&mut self) -> (usize, String) {
        let l = self.rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let i = self.addrs.len();
        // it knows where the group's replicas are, to answer the leader
        self.addrs.push(addr.clone());
        self.nodes.push(None);
        self.joined.push(i);
        self.start_node(i, l);
        (i, addr)
    }

    fn restart(&mut self, i: usize) {
        let l = self.rt.block_on(tokio::net::TcpListener::bind(self.addrs[i].as_str())).unwrap();
        self.start_node(i, l);
    }

    fn leader(&self) -> Option<usize> {
        (0..self.nodes.len()).find(|i| self.nodes[*i].as_ref().and_then(|n| n.replica.as_ref()).map(|r| r.can_serve()).unwrap_or(false))
    }

    fn wait_leader(&self) -> usize {
        let t = Instant::now();
        loop {
            if let Some(l) = self.leader() {
                return l;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "no leader elected");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn cluster(&self) -> Arc<Cluster> {
        Cluster::connect(&ClusterConfig { shards: vec![ShardInfo { id: 0, addr: self.addrs.join(",") }] }).unwrap()
    }

    /// The rows a replica's own engine holds (read directly, not through the leader).
    fn local_rows(&self, i: usize, sql: &str) -> Vec<Vec<String>> {
        let mut c = self.nodes[i].as_ref().unwrap().server.connect();
        c.query(sql).unwrap().into_iter().map(|r| r.iter().map(display).collect()).collect()
    }

    /// Wait until every live replica's engine has the same answer as `expect` to the query.
    fn converge(&self, sql: &str, expect: &[Vec<String>]) {
        let t = Instant::now();
        loop {
            let ok = (0..self.nodes.len()).filter(|i| self.nodes[*i].is_some()).all(|i| self.local_rows(i, sql) == expect);
            if ok {
                return;
            }
            assert!(t.elapsed() < Duration::from_secs(15), "the replicas did not converge on {:?}: {:?}", expect, (0..self.nodes.len()).filter(|i| self.nodes[*i].is_some()).map(|i| self.local_rows(i, sql)).collect::<Vec<_>>());
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        for i in 0..self.nodes.len() {
            self.kill(i);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn exec_ok(s: &mut ClusterSession, sql: &str) {
    let r = s.execute(sql);
    assert!(r.first_error().is_none(), "{}: {:?}", sql, r.first_error());
}

fn rows(s: &mut ClusterSession, sql: &str) -> Vec<Vec<String>> {
    s.query(sql).unwrap().into_iter().map(|r| r.iter().map(display).collect()).collect()
}

#[test]
fn a_replicated_shard_serves_through_its_leader_and_the_replicas_converge() {
    let g = Group::start(3, "conv");
    g.wait_leader();
    let cluster = g.cluster();
    let mut s = cluster.session();
    exec_ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10), v DECIMAL(8,2))");
    exec_ok(&mut s, "INSERT INTO t VALUES (1,'a',1.50),(2,'b',2.50),(3,'c',3.50)");
    exec_ok(&mut s, "UPDATE t SET v = v * 2 WHERE id <= 2");
    exec_ok(&mut s, "DELETE FROM t WHERE id = 3");
    let expect = vec![vec!["1".to_string(), "a".to_string(), "3.00".to_string()], vec!["2".to_string(), "b".to_string(), "5.00".to_string()]];
    assert_eq!(rows(&mut s, "SELECT id, name, v FROM t ORDER BY id"), expect);
    // every replica's own engine has the same rows (the followers applied the leader's records)
    g.converge("SELECT id, name, v FROM t ORDER BY id", &expect);
}

#[test]
fn the_service_and_the_data_survive_the_death_of_the_leader() {
    let mut g = Group::start(3, "failover");
    let first = g.wait_leader();
    let cluster = g.cluster();
    let mut s = cluster.session();
    exec_ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    exec_ok(&mut s, "INSERT INTO t VALUES (1,10),(2,20),(3,30)");
    g.converge("SELECT COUNT(*) FROM t", &[vec!["3".to_string()]]);
    g.kill(first);
    // the control node finds the new leader by itself; nothing that was acknowledged is lost
    let mut s2 = cluster.session();
    assert_eq!(rows(&mut s2, "SELECT SUM(v) FROM t"), vec![vec!["60".to_string()]]);
    exec_ok(&mut s2, "INSERT INTO t VALUES (4,40)");
    exec_ok(&mut s2, "UPDATE t SET v = v + 1 WHERE id = 1");
    let expect = vec![vec!["1".to_string(), "11".to_string()], vec!["2".to_string(), "20".to_string()], vec!["3".to_string(), "30".to_string()], vec!["4".to_string(), "40".to_string()]];
    assert_eq!(rows(&mut s2, "SELECT id, v FROM t ORDER BY id"), expect);
    assert_ne!(g.wait_leader(), first);
    // the replica that was down comes back (from its files) and catches up with what it missed
    g.restart(first);
    g.converge("SELECT id, v FROM t ORDER BY id", &expect);
    // and it can lead again: kill the others one at a time and the data is still there
    let leader_now = g.wait_leader();
    g.kill(leader_now);
    let mut s3 = cluster.session();
    assert_eq!(rows(&mut s3, "SELECT COUNT(*) FROM t"), vec![vec!["4".to_string()]]);
}

#[test]
fn without_a_majority_nothing_is_served_and_nothing_is_lost() {
    let mut g = Group::start(3, "quorum");
    let l = g.wait_leader();
    let cluster = g.cluster();
    let mut s = cluster.session();
    exec_ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    exec_ok(&mut s, "INSERT INTO t VALUES (1,1)");
    g.converge("SELECT COUNT(*) FROM t", &[vec!["1".to_string()]]);
    // lose two of three
    let others: Vec<usize> = (0..3).filter(|i| *i != l).collect();
    g.kill(others[0]);
    g.kill(others[1]);
    std::thread::sleep(Duration::from_millis(500));
    // the lone leader's lease runs out: it neither writes nor reads
    let mut s2 = cluster.session();
    let w = s2.execute("INSERT INTO t VALUES (2,2)");
    assert!(w.first_error().is_some(), "a write without a majority must fail");
    assert!(s2.query("SELECT COUNT(*) FROM t").is_err(), "a read without a majority must be refused");
    // the majority returns: service resumes, and the failed write left nothing behind
    g.restart(others[0]);
    g.restart(others[1]);
    g.wait_leader();
    let mut s3 = cluster.session();
    assert_eq!(rows(&mut s3, "SELECT COUNT(*) FROM t"), vec![vec!["1".to_string()]]);
    exec_ok(&mut s3, "INSERT INTO t VALUES (3,3)");
    g.converge("SELECT id FROM t ORDER BY id", &[vec!["1".to_string()], vec!["3".to_string()]]);
    let _ = Value::Null;
}

#[test]
fn a_prepared_transaction_survives_the_loss_of_the_leader_and_commits_on_the_new_one() {
    let mut g = Group::start(3, "prepared");
    let first = g.wait_leader();
    let cluster = g.cluster();
    let mut s = cluster.session();
    exec_ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    exec_ok(&mut s, "INSERT INTO t VALUES (1,10)");
    g.converge("SELECT COUNT(*) FROM t", &[vec!["1".to_string()]]);
    // a transaction votes on the leader (whoever leads now): its Prepare record goes through the group's log like a commit does
    let first = g.wait_leader();
    {
        let srv = g.nodes[first].as_ref().unwrap().server.clone();
        let mut c = srv.connect();
        assert!(c.execute("BEGIN TRANSACTION").first_error().is_none());
        assert!(c.execute("INSERT INTO t VALUES (2,20)").first_error().is_none());
        assert!(c.prepare_transaction(5).unwrap());
    }
    g.kill(first);
    // the new leader holds the vote as in doubt, and refuses statements until it is settled
    let second = g.wait_leader();
    assert_ne!(second, first);
    let srv2 = g.nodes[second].as_ref().unwrap().server.clone();
    assert_eq!(srv2.in_doubt(), vec![5]);
    // the coordinator decided to commit: the new leader finishes it, and the settlement is replicated
    assert!(srv2.resolve_prepared(5, true).unwrap());
    let expect = vec![vec!["1".to_string(), "10".to_string()], vec!["2".to_string(), "20".to_string()]];
    g.converge("SELECT id, v FROM t ORDER BY id", &expect);
    // the old leader comes back: it had the vote in its own log, and learns the outcome from the group's
    g.restart(first);
    g.converge("SELECT id, v FROM t ORDER BY id", &expect);
    let t = Instant::now();
    while !g.nodes[first].as_ref().unwrap().server.in_doubt().is_empty() {
        assert!(t.elapsed() < Duration::from_secs(10), "the old leader still holds the transaction in doubt");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut s2 = cluster.session();
    assert_eq!(rows(&mut s2, "SELECT SUM(v) FROM t"), vec![vec!["30".to_string()]]);
}

fn counts(g: &Group, i: usize) -> (u64, u64) {
    g.nodes[i].as_ref().unwrap().replica.as_ref().unwrap().snapshot_counts()
}

fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_log_is_cut_by_snapshots_and_a_replica_that_was_down_is_brought_up_by_one() {
    let mut g = Group::start_with(3, "snap", 20);
    let leader = g.wait_leader();
    let cluster = g.cluster();
    let mut s = cluster.session();
    exec_ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(20))");
    let down = (0..3).find(|i| *i != leader).unwrap();
    for id in 1..=60 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({}, 'row{}')", id, id));
    }
    g.converge("SELECT COUNT(*) FROM t", &[vec!["60".to_string()]]);
    // the replicas that are up cut their logs
    for i in 0..3 {
        wait_for("a snapshot", 10, || counts(&g, i).0 >= 1);
    }
    g.kill(down);
    // the others go on a long way, and cut their logs past what the replica that is down has
    for id in 61..=160 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({}, 'row{}')", id, id));
    }
    let up: Vec<usize> = (0..3).filter(|i| *i != down).collect();
    for &i in &up {
        wait_for("the log to be cut", 10, || g.nodes[i].as_ref().unwrap().replica.as_ref().unwrap().log_len() < 40);
    }
    // it returns, finds the leader's log starts after its own end, and is given a snapshot
    g.restart(down);
    g.converge("SELECT COUNT(*) FROM t", &[vec!["160".to_string()]]);
    wait_for("the snapshot to be installed", 10, || counts(&g, down).1 >= 1);
    g.converge("SELECT MIN(id), MAX(id) FROM t", &[vec!["1".to_string(), "160".to_string()]]);
    // the replica that was brought up by snapshot serves correctly when it has to lead: stop the others
    for &i in &up {
        g.kill(i);
    }
    g.restart(up[0]);
    g.wait_leader();
    let mut s2 = cluster.session();
    assert_eq!(rows(&mut s2, "SELECT COUNT(*), SUM(id) FROM t"), vec![vec!["160".to_string(), "12880".to_string()]]);
    exec_ok(&mut s2, "INSERT INTO t VALUES (161, 'after')");
    assert_eq!(rows(&mut s2, "SELECT v FROM t WHERE id = 161"), vec![vec!["after".to_string()]]);
}

#[test]
fn a_replica_is_added_and_another_removed_while_the_shard_serves() {
    let mut g = Group::start_with(3, "member", 25);
    g.wait_leader();
    let cluster = g.cluster();
    let mut s = cluster.session();
    exec_ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    for id in 1..=80 {
        exec_ok(&mut s, &format!("INSERT INTO t VALUES ({}, {})", id, id * 10));
    }
    // a fourth replica is started on an empty directory and the group takes it in: it is sent the snapshot (the log was cut
    // long ago), then the entries after it, and becomes a voter
    let (j, addr) = g.start_joiner();
    cluster.add_replica(0, j as u64 + 1, &addr).unwrap();
    let members = cluster.replicas(0).unwrap();
    assert_eq!(members.len(), 4, "{:?}", members);
    assert!(members.iter().all(|m| m.2), "every member is a voter: {:?}", members);
    assert!(members.iter().any(|m| m.1 == addr));
    exec_ok(&mut s, "INSERT INTO t VALUES (81, 810)");
    g.converge("SELECT COUNT(*), SUM(v) FROM t", &[vec!["81".to_string(), "33210".to_string()]]);
    assert!(counts(&g, j).1 >= 1, "the new replica was sent a snapshot");
    // the original leader dies: the shard still has a majority (three of four), and the new replica takes part
    let l = g.wait_leader();
    g.kill(l);
    let mut s2 = cluster.session();
    exec_ok(&mut s2, "INSERT INTO t VALUES (82, 820)");
    // the dead replica is removed from the group: three voters remain, and the control node follows
    let dead_addr = g.addrs[l].clone();
    cluster.remove_replica(0, &dead_addr).unwrap();
    let members = cluster.replicas(0).unwrap();
    assert_eq!(members.len(), 3, "{:?}", members);
    assert!(members.iter().all(|m| m.1 != dead_addr));
    // with three voters one more can be lost
    let l2 = g.wait_leader();
    let other = (0..4).find(|i| *i != l && *i != l2 && g.nodes[*i].is_some()).unwrap();
    g.kill(other);
    let mut s3 = cluster.session();
    exec_ok(&mut s3, "INSERT INTO t VALUES (83, 830)");
    assert_eq!(rows(&mut s3, "SELECT COUNT(*) FROM t"), vec![vec!["83".to_string()]]);
}

#[test]
fn the_leader_can_be_removed_from_its_group() {
    let mut g = Group::start_with(3, "selfremove", 0);
    let first = g.wait_leader();
    let cluster = g.cluster();
    let mut s = cluster.session();
    exec_ok(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");
    exec_ok(&mut s, "INSERT INTO t VALUES (1)");
    // add a fourth replica, then remove the leader
    let (j, addr) = g.start_joiner();
    cluster.add_replica(0, j as u64 + 1, &addr).unwrap();
    let leader_addr = g.addrs[first].clone();
    cluster.remove_replica(0, &leader_addr).unwrap();
    let t = Instant::now();
    let l2 = loop {
        if let Some(l) = g.leader() {
            if l != first {
                break l;
            }
        }
        assert!(t.elapsed() < Duration::from_secs(15), "no new leader after the leader removed itself");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(l2, first);
    let mut s2 = cluster.session();
    exec_ok(&mut s2, "INSERT INTO t VALUES (2)");
    assert_eq!(rows(&mut s2, "SELECT COUNT(*) FROM t"), vec![vec!["2".to_string()]]);
    // the removed replica is still running but is no longer asked: it does not disturb the group
    // (the Raft simulation checks that its term never rises; here, loaded machines may hold an election of their own)
    std::thread::sleep(Duration::from_millis(800));
    assert!(!g.nodes[first].as_ref().unwrap().replica.as_ref().unwrap().can_serve(), "the removed replica does not lead");
    let mut s3 = cluster.session();
    exec_ok(&mut s3, "INSERT INTO t VALUES (3)");
    assert_eq!(rows(&mut s3, "SELECT COUNT(*) FROM t"), vec![vec!["3".to_string()]]);
}
