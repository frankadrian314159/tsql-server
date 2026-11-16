//! Two-phase commit under failure: durable shards that crash and restart between their vote and the commit, and a control
//! node that stops after or before it decided. A vote is durable (a `Prepare` record in the shard's log), so a participant
//! that restarted after voting still commits, and one whose coordinator never decided is rolled back.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::Server;
use tsql_shared_ces::route::{key_hash, KeyClass};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn_on, RunningShard, ShardNode};

struct Shards {
    rt: Arc<tokio::runtime::Runtime>,
    dir: PathBuf,
    addrs: Vec<String>,
    nodes: Arc<Mutex<Vec<Option<RunningShard>>>>,
}

fn unique_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("tsql_2pc_{}_{}_{}", tag, std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn start_node(rt: &tokio::runtime::Runtime, dir: &std::path::Path, i: usize, listener: tokio::net::TcpListener) -> RunningShard {
    let server = Server::open(dir.join(format!("data{}", i))).unwrap();
    rt.block_on(async { spawn_on(ShardNode::new(i as u32, server), listener).await.unwrap() })
}

impl Shards {
    fn start(n: usize, tag: &str) -> Shards {
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let dir = unique_dir(tag);
        let listeners: Vec<tokio::net::TcpListener> = (0..n).map(|_| rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap()).collect();
        let addrs: Vec<String> = listeners.iter().map(|l| l.local_addr().unwrap().to_string()).collect();
        let nodes = listeners.into_iter().enumerate().map(|(i, l)| Some(start_node(&rt, &dir, i, l))).collect();
        Shards { rt, dir, addrs, nodes: Arc::new(Mutex::new(nodes)) }
    }

    fn config(&self) -> ClusterConfig {
        ClusterConfig { shards: self.addrs.iter().enumerate().map(|(i, a)| ShardInfo { id: i as u32, addr: a.clone() }).collect() }
    }

    /// Stop shard `i` (its open sessions and transactions go with it) and start it again on its files: what a crash and a
    /// restart of the process do. Runs on its own thread so that it can be called from inside the control node.
    fn restart_fn(&self) -> impl Fn(usize) + Send + Sync + 'static {
        let (rt, nodes, dir, addrs) = (self.rt.clone(), self.nodes.clone(), self.dir.clone(), self.addrs.clone());
        move |i| {
            let (rt, nodes, dir, addr) = (rt.clone(), nodes.clone(), dir.clone(), addrs[i].clone());
            std::thread::spawn(move || {
                let old = nodes.lock().unwrap()[i].take();
                if let Some(n) = old {
                    rt.block_on(n.shutdown());
                }
                std::thread::sleep(Duration::from_millis(200)); // the engine's directory lock is released with its threads
                let l = rt.block_on(tokio::net::TcpListener::bind(addr.as_str())).unwrap();
                let running = start_node(&rt, &dir, i, l);
                nodes.lock().unwrap()[i] = Some(running);
            })
            .join()
            .unwrap();
        }
    }

    fn server(&self, i: usize) -> Arc<Server> {
        self.nodes.lock().unwrap()[i].as_ref().unwrap().server.clone()
    }
}

impl Drop for Shards {
    fn drop(&mut self) {
        let old: Vec<RunningShard> = self.nodes.lock().unwrap().iter_mut().filter_map(|n| n.take()).collect();
        for n in old {
            self.rt.block_on(n.shutdown());
        }
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rows(s: &mut ClusterSession, sql: &str) -> Vec<Vec<String>> {
    s.query(sql).unwrap().into_iter().map(|r| r.iter().map(display).collect()).collect()
}

fn ok(s: &mut ClusterSession, sql: &str) {
    let r = s.execute(sql);
    assert!(r.first_error().is_none(), "{}: {:?}", sql, r.first_error());
}

/// Two keys that live on different shards (of a cluster of two).
fn keys(c: &Cluster) -> (i32, i32) {
    let shard = |i: i32| c.shard_of_hash(key_hash(&tsql_server::Value::Int(i), KeyClass::Int).unwrap());
    ((1..200).find(|i| shard(*i) == 0).unwrap(), (1..200).find(|i| shard(*i) == 1).unwrap())
}

fn setup(c: &Arc<Cluster>) -> ClusterSession {
    let mut s = c.session();
    ok(&mut s, "CREATE TABLE acct (id INT PRIMARY KEY, bal INT)");
    let rows: Vec<String> = (1..=30).map(|i| format!("({}, 100)", i)).collect();
    ok(&mut s, &format!("INSERT INTO acct VALUES {}", rows.join(",")));
    s
}

fn transfer(s: &mut ClusterSession, k0: i32, k1: i32) {
    ok(s, &format!("BEGIN TRANSACTION; UPDATE acct SET bal = bal - 10 WHERE id = {}; UPDATE acct SET bal = bal + 10 WHERE id = {}", k0, k1));
}

fn balances(s: &mut ClusterSession, k0: i32, k1: i32) -> Vec<Vec<String>> {
    rows(s, &format!("SELECT id, bal FROM acct WHERE id IN ({}, {}) ORDER BY id", k0.min(k1), k0.max(k1)))
}

/// A participant that crashes and restarts after it voted (before the decision, or after it) still commits its part.
fn participant_restarts_after_voting(point: &'static str, tag: &str) {
    let sh = Shards::start(2, tag);
    let cluster = Cluster::connect(&sh.config()).unwrap();
    let mut s = setup(&cluster);
    let (k0, k1) = keys(&cluster);
    let restart = sh.restart_fn();
    cluster.set_fault_hook(Some(Arc::new(move |p| {
        if p == point {
            restart(1);
        }
        false
    })));
    transfer(&mut s, k0, k1);
    let r = s.execute("COMMIT");
    assert!(r.first_error().is_none(), "the commit must succeed although shard 1 restarted after voting: {:?}", r.first_error());
    cluster.set_fault_hook(None);
    assert_eq!(cluster.pending_transactions(), 0);
    assert!(sh.server(1).in_doubt().is_empty());
    // both halves of the transfer are there, from a session of the new control life and straight from the shards
    let mut s2 = cluster.session();
    let (a, b) = (if k0 < k1 { 90 } else { 110 }, if k0 < k1 { 110 } else { 90 });
    assert_eq!(balances(&mut s2, k0, k1), vec![vec![k0.min(k1).to_string(), a.to_string()], vec![k0.max(k1).to_string(), b.to_string()]]);
    let mut direct = sh.server(1).connect();
    assert_eq!(display(&direct.query(&format!("SELECT bal FROM acct WHERE id = {}", k1)).unwrap()[0][0]), "110");
}

#[test]
fn a_participant_that_restarts_between_its_vote_and_the_decision_still_commits() {
    participant_restarts_after_voting("after_prepare", "vote");
}

#[test]
fn a_participant_that_restarts_after_the_decision_still_commits() {
    participant_restarts_after_voting("after_decision", "decided");
}

#[test]
fn a_control_node_that_stops_after_deciding_finishes_the_commit_when_it_is_back() {
    let sh = Shards::start(2, "ctl_after");
    let state = sh.dir.with_extension("state");
    let _ = std::fs::remove_file(&state);
    let (k0, k1);
    {
        let cluster = Cluster::open(&state, &sh.config()).unwrap();
        let mut s = setup(&cluster);
        (k0, k1) = keys(&cluster);
        cluster.set_fault_hook(Some(Arc::new(|p| p == "after_decision")));
        transfer(&mut s, k0, k1);
        let r = s.execute("COMMIT");
        assert_eq!(r.first_error().map(|e| e.number), Some(50220));
        assert_eq!(cluster.pending_transactions(), 1);
        // the control node dies here: its sessions go, the shards keep the votes
    }
    // the shards refuse statements until the transaction is settled...
    let cluster = Cluster::open(&state, &ClusterConfig::default()).unwrap();
    assert_eq!(cluster.pending_transactions(), 0, "the commit was finished on the way up");
    let mut s = cluster.session();
    let (a, b) = (if k0 < k1 { 90 } else { 110 }, if k0 < k1 { 110 } else { 90 });
    assert_eq!(balances(&mut s, k0, k1), vec![vec![k0.min(k1).to_string(), a.to_string()], vec![k0.max(k1).to_string(), b.to_string()]]);
    for i in 0..2 {
        assert!(sh.server(i).in_doubt().is_empty());
    }
    let _ = std::fs::remove_file(&state);
}

#[test]
fn a_control_node_that_stops_before_deciding_rolls_the_transaction_back_when_it_is_back() {
    let sh = Shards::start(2, "ctl_before");
    let state = sh.dir.with_extension("state");
    let _ = std::fs::remove_file(&state);
    let (k0, k1);
    {
        let cluster = Cluster::open(&state, &sh.config()).unwrap();
        let mut s = setup(&cluster);
        (k0, k1) = keys(&cluster);
        cluster.set_fault_hook(Some(Arc::new(|p| p == "after_prepare")));
        transfer(&mut s, k0, k1);
        let r = s.execute("COMMIT");
        assert_eq!(r.first_error().map(|e| e.number), Some(50220));
        assert_eq!(cluster.pending_transactions(), 0, "nothing was decided");
    }
    let cluster = Cluster::open(&state, &ClusterConfig::default()).unwrap();
    let mut s = cluster.session();
    assert_eq!(balances(&mut s, k0, k1), vec![vec![k0.min(k1).to_string(), "100".into()], vec![k0.max(k1).to_string(), "100".into()]]);
    for i in 0..2 {
        assert!(sh.server(i).in_doubt().is_empty());
    }
    // the rows are writable again: nothing is left locked
    ok(&mut s, &format!("UPDATE acct SET bal = 0 WHERE id IN ({}, {})", k0, k1));
    let _ = std::fs::remove_file(&state);
}

#[test]
fn a_statement_that_meets_an_undecided_in_doubt_transaction_rolls_it_back_and_goes_on() {
    let sh = Shards::start(2, "stmt");
    let cluster = Cluster::connect(&sh.config()).unwrap();
    let mut s = setup(&cluster);
    let (k0, _k1) = keys(&cluster);
    // a participant voted for a transaction whose coordinator is gone and never decided it
    {
        let srv = sh.server(0);
        let mut c = srv.connect();
        c.execute("BEGIN TRANSACTION");
        let r = c.execute(&format!("UPDATE acct SET bal = 5 WHERE id = {}", k0));
        assert!(r.first_error().is_none());
        assert!(c.prepare_transaction(777).unwrap());
    }
    assert_eq!(sh.server(0).in_doubt(), vec![777]);
    // the next statement finds the shard refusing, settles the transaction (nobody decided it: presumed abort) and runs
    assert_eq!(rows(&mut s, &format!("SELECT bal FROM acct WHERE id = {}", k0)), vec![vec!["100"]]);
    assert!(sh.server(0).in_doubt().is_empty());
}

#[test]
fn a_failed_vote_aborts_every_participant_and_leaves_nothing_prepared() {
    let sh = Shards::start(2, "abort");
    let cluster = Cluster::connect(&sh.config()).unwrap();
    let mut s = setup(&cluster);
    let (k0, k1) = keys(&cluster);
    transfer(&mut s, k0, k1);
    // shard 1 loses its open transaction before it is asked to vote (it restarted): its vote fails, shard 0's is rolled back
    let restart = sh.restart_fn();
    restart(1);
    let r = s.execute("COMMIT");
    assert!(r.first_error().is_some(), "{:?}", r.first_error());
    assert!(!s.in_transaction());
    for i in 0..2 {
        assert!(sh.server(i).in_doubt().is_empty());
    }
    let mut s2 = cluster.session();
    assert_eq!(balances(&mut s2, k0, k1), vec![vec![k0.min(k1).to_string(), "100".into()], vec![k0.max(k1).to_string(), "100".into()]]);
    ok(&mut s2, &format!("UPDATE acct SET bal = 1 WHERE id IN ({}, {})", k0, k1)); // and nothing is locked
}
