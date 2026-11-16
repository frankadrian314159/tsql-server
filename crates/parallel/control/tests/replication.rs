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

struct Group {
    rt: tokio::runtime::Runtime,
    dir: PathBuf,
    addrs: Vec<String>,
    nodes: Vec<Option<RunningShard>>,
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
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = unique_dir(tag);
        let listeners: Vec<tokio::net::TcpListener> = (0..n).map(|_| rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap()).collect();
        let addrs: Vec<String> = listeners.iter().map(|l| l.local_addr().unwrap().to_string()).collect();
        let mut g = Group { rt, dir, addrs, nodes: (0..n).map(|_| None).collect() };
        for (i, l) in listeners.into_iter().enumerate() {
            g.start_node(i, l);
        }
        g
    }

    fn start_node(&mut self, i: usize, listener: tokio::net::TcpListener) {
        let peers: Vec<(u64, String)> = (0..self.addrs.len()).filter(|j| *j != i).map(|j| (j as u64 + 1, self.addrs[j].clone())).collect();
        let mut cfg = ReplicaConfig::new(i as u64 + 1, peers, self.dir.join(format!("raft{}", i)));
        cfg.tick_ms = 10;
        cfg.sync = false;
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
