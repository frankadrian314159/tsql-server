//! Operating a cluster: resource metrics reach the control node, a shard drains and stops gracefully, and the autoscaler grows
//! and shrinks the cluster by the thresholds of its policy.
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tsql_control::autoscale::{Action, Autoscaler, CommandProvisioner, Provisioner};
use tsql_control::rebalance::ScalePolicy;
use tsql_control::Cluster;
use tsql_server::Server;
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

fn config(shards: &[RunningShard]) -> ClusterConfig {
    ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() }
}

fn start_memory(rt: &tokio::runtime::Runtime, n: usize) -> Vec<RunningShard> {
    (0..n).map(|i| rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap()).collect()
}

fn ok(cl: &mut tsql_control::ClusterSession, sql: &str) {
    let r = cl.execute(sql);
    assert!(r.first_error().is_none(), "{}: {:?}", sql, r.first_error());
}

fn count(cl: &mut tsql_control::ClusterSession) -> String {
    let r = cl.execute("SELECT COUNT(*), SUM(v) FROM t");
    r.resultsets().first().map(|rs| rs.rows[0].iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")).unwrap_or_default()
}

#[test]
fn shard_metrics_reach_the_control_node_and_are_exported() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shards = start_memory(&rt, 2);
    let cluster = Cluster::connect(&config(&shards)).unwrap();
    let mut cl = cluster.session();
    ok(&mut cl, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    ok(&mut cl, "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3), (4, 4)");
    std::thread::sleep(Duration::from_millis(1200)); // a reading of the machine needs a second
    let before: Vec<_> = cluster.metrics().into_iter().map(|m| m.unwrap()).collect();
    assert_eq!(before.len(), 2);
    for m in &before {
        assert!(m.sample.cores >= 1 && m.sample.mem_total_bytes > 0 && m.sample.mem_process_bytes > 0, "{:?}", m);
        assert!(m.sample.cpu_machine_pct >= 0.0 && m.sample.cpu_machine_pct <= 100.0);
        assert!(!m.draining);
    }
    for _ in 0..10 {
        ok(&mut cl, "SELECT COUNT(*) FROM t");
    }
    let after: Vec<_> = cluster.metrics().into_iter().map(|m| m.unwrap()).collect();
    assert!(after.iter().map(|m| m.statements).sum::<u64>() >= before.iter().map(|m| m.statements).sum::<u64>() + 10, "the statement counter follows the work");
    assert!(after.iter().all(|m| m.sessions >= 1));
    let text = cluster.prometheus();
    for want in ["tsql_cpu_machine_percent{node=\"control\"}", "tsql_memory_used_percent{node=\"shard-1\"}", "tsql_disk_write_bytes_per_second{node=\"shard-0\"}", "tsql_network_receive_bytes_per_second", "tsql_shard_up{node=\"shard-0\"} 1", "tsql_slots{node=\"shard-1\"} 512"] {
        assert!(text.contains(want), "missing {} in\n{}", want, text);
    }
    // the telemetry the autoscaler reads carries them too
    assert!(cluster.telemetry().iter().all(|t| t.metrics.is_some()));
    // a shard that is gone is reported, not hidden
    let mut shards = shards;
    let last = shards.pop().unwrap();
    rt.block_on(last.shutdown());
    assert!(cluster.prometheus().contains("tsql_shard_up{node=\"shard-1\"} 0"));
    assert_eq!(cluster.wait_for_shards(Duration::from_millis(300)), Err(vec![1]));
    drop(cl);
    for s in shards {
        rt.block_on(s.shutdown());
    }
}

#[test]
fn a_draining_shard_lets_open_transactions_finish_and_refuses_new_work() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shard = rt.block_on(spawn(ShardNode::new(0, Server::open(dir.path()).unwrap()), "127.0.0.1:0".parse().unwrap())).unwrap();
    let cluster = Cluster::connect(&config(std::slice::from_ref(&shard))).unwrap();
    let mut a = cluster.session();
    let mut b = cluster.session();
    ok(&mut a, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    ok(&mut a, "INSERT INTO t VALUES (1, 10)");
    ok(&mut b, "SELECT COUNT(*) FROM t");
    ok(&mut a, "BEGIN TRANSACTION");
    ok(&mut a, "INSERT INTO t VALUES (2, 20)");

    let control = shard.control().clone();
    let drain = rt.spawn(async move { control.drain(Duration::from_secs(10)).await });
    std::thread::sleep(Duration::from_millis(400));
    assert!(shard.control().is_draining());
    // a session with no transaction is refused, with the shutdown error
    let r = b.execute("SELECT COUNT(*) FROM t");
    assert_eq!(r.first_error().map(|e| e.number), Some(50240), "{:?}", r.first_error());
    assert!(cluster.metrics()[0].as_ref().map(|m| m.draining).unwrap_or(false));
    assert!(!drain.is_finished(), "the drain waits for the open transaction");
    // the session with the transaction goes on and finishes it
    ok(&mut a, "INSERT INTO t VALUES (3, 30)");
    ok(&mut a, "COMMIT");
    let report = rt.block_on(drain).unwrap();
    assert_eq!((report.open_at_start, report.finished, report.rolled_back), (1, 1, 0), "{:?}", report);
    assert!(report.checkpointed);
    drop(a);
    drop(b);
    rt.block_on(shard.shutdown());

    // everything committed is there after a restart
    let server = Server::open(dir.path()).unwrap();
    let mut c = server.connect();
    let r = c.execute("SELECT COUNT(*), SUM(v) FROM t");
    assert_eq!(r.resultsets()[0].rows[0].iter().map(|v| v.to_string()).collect::<Vec<_>>(), vec!["3", "60"]);
}

#[test]
fn a_transaction_that_outlasts_the_grace_period_is_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shard = rt.block_on(spawn(ShardNode::new(0, Server::open(dir.path()).unwrap()), "127.0.0.1:0".parse().unwrap())).unwrap();
    let cluster = Cluster::connect(&config(std::slice::from_ref(&shard))).unwrap();
    let mut a = cluster.session();
    ok(&mut a, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    ok(&mut a, "INSERT INTO t VALUES (1, 10)");
    ok(&mut a, "BEGIN TRANSACTION");
    ok(&mut a, "INSERT INTO t VALUES (2, 20)");
    let t0 = std::time::Instant::now();
    let report = rt.block_on(shard.graceful_shutdown(Duration::from_millis(500)));
    assert!(t0.elapsed() >= Duration::from_millis(450), "the grace period was given");
    assert_eq!((report.open_at_start, report.finished, report.rolled_back), (1, 0, 1), "{:?}", report);
    drop(a);
    let server = Server::open(dir.path()).unwrap();
    let mut c = server.connect();
    let r = c.execute("SELECT COUNT(*) FROM t");
    assert_eq!(r.resultsets()[0].rows[0][0].to_string(), "1", "the open transaction did not commit");
}

/// A provisioner that starts and stops shard nodes in this process.
struct InProcess {
    rt: Arc<tokio::runtime::Runtime>,
    nodes: Arc<Mutex<Vec<RunningShard>>>,
    started: Arc<Mutex<u32>>,
}

impl Provisioner for InProcess {
    fn add_node(&mut self) -> Result<String, String> {
        let id = self.nodes.lock().unwrap().len() as u32;
        let s = self.rt.block_on(spawn(ShardNode::new(id, Server::new()), "127.0.0.1:0".parse().unwrap())).map_err(|e| e.to_string())?;
        let addr = s.addr.to_string();
        self.nodes.lock().unwrap().push(s);
        *self.started.lock().unwrap() += 1;
        Ok(addr)
    }
    fn remove_node(&mut self, addr: &str) -> Result<(), String> {
        let mut nodes = self.nodes.lock().unwrap();
        let i = nodes.iter().position(|n| n.addr.to_string() == addr).ok_or("no such node")?;
        let n = nodes.remove(i);
        self.rt.block_on(n.shutdown());
        Ok(())
    }
}

#[test]
fn the_autoscaler_grows_and_shrinks_the_cluster_by_its_thresholds() {
    let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
    let nodes = Arc::new(Mutex::new(start_memory(&rt, 2)));
    let cfg = config(&nodes.lock().unwrap());
    let cluster = Cluster::connect(&cfg).unwrap();
    let mut cl = cluster.session();
    ok(&mut cl, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    ok(&mut cl, &format!("INSERT INTO t VALUES {}", (1..=200).map(|i| format!("({}, {})", i, i)).collect::<Vec<_>>().join(",")));
    let expected = count(&mut cl);
    let started = Arc::new(Mutex::new(0));
    let mut scaler = Autoscaler::new(cluster.clone(), ScalePolicy::default(), Box::new(InProcess { rt: rt.clone(), nodes: nodes.clone(), started: started.clone() }));
    std::thread::sleep(Duration::from_millis(1200));

    // within the thresholds: nothing, however often it looks
    scaler.set_policy(ScalePolicy { sustain: 2, cooldown_secs: 0, cpu_high_pct: f64::MAX, mem_high_pct: f64::MAX, disk_high_bps: f64::MAX, net_high_bps: f64::MAX, cpu_low_pct: -1.0, max_rows_per_shard: u64::MAX, min_rows_per_shard: 0, ..Default::default() });
    for _ in 0..6 {
        let a = scaler.step();
        assert!(matches!(a, Action::Nothing(_)), "{:?}", a);
    }
    assert_eq!(cluster.shard_count(), 2);

    // memory above the limit (the limit is 0%: the machine always is): add a shard, but only after two readings in a row
    scaler.set_policy(ScalePolicy { mem_high_pct: 0.0, sustain: 2, cooldown_secs: 0, max_shards: 3, cpu_high_pct: f64::MAX, disk_high_bps: f64::MAX, net_high_bps: f64::MAX, cpu_low_pct: -1.0, max_rows_per_shard: u64::MAX, min_rows_per_shard: 0, ..Default::default() });
    assert!(matches!(scaler.step(), Action::Nothing(_)), "one reading is not a trend");
    let a = scaler.step();
    assert!(matches!(&a, Action::Added { shard: 2, .. }), "{:?}", a);
    assert_eq!(cluster.shard_count(), 3);
    assert_eq!(count(&mut cl), expected, "the rows follow the slots");
    assert_eq!(*started.lock().unwrap(), 1);
    // the most shards: no more, whatever the load
    for _ in 0..4 {
        assert!(matches!(scaler.step(), Action::Nothing(_)));
    }
    assert_eq!(cluster.shard_count(), 3);

    // idle processors (the limit is above 100%) and memory under its limit: take one away again
    scaler.set_policy(ScalePolicy { mem_high_pct: f64::MAX, sustain: 2, cooldown_secs: 0, min_shards: 2, cpu_high_pct: f64::MAX, disk_high_bps: f64::MAX, net_high_bps: f64::MAX, cpu_low_pct: 101.0, max_rows_per_shard: u64::MAX, min_rows_per_shard: 0, ..Default::default() });
    assert!(matches!(scaler.step(), Action::Nothing(_)));
    let a = scaler.step();
    assert!(matches!(&a, Action::Removed { .. }), "{:?}", a);
    assert_eq!(cluster.shard_count(), 2);
    assert_eq!(nodes.lock().unwrap().len(), 2, "the provisioner took the node away");
    assert_eq!(count(&mut cl), expected, "the rows came back to the others");
    // the least shards
    for _ in 0..4 {
        assert!(matches!(scaler.step(), Action::Nothing(_)));
    }
    assert_eq!(cluster.shard_count(), 2);

    // after a change it waits (the cooldown)
    scaler.set_policy(ScalePolicy { mem_high_pct: 0.0, sustain: 1, cooldown_secs: 0, max_shards: 4, cpu_high_pct: f64::MAX, disk_high_bps: f64::MAX, net_high_bps: f64::MAX, cpu_low_pct: -1.0, max_rows_per_shard: u64::MAX, min_rows_per_shard: 0, ..Default::default() });
    assert!(matches!(scaler.step(), Action::Added { .. }));
    let mut p = scaler.policy.clone();
    p.cooldown_secs = 3600;
    scaler.set_policy(p);
    let a = scaler.step();
    assert!(matches!(&a, Action::Nothing(why) if why.contains("cooling down")), "{:?}", a);
    drop(cl);
    drop(scaler);
    for n in nodes.lock().unwrap().drain(..) {
        rt.block_on(n.shutdown());
    }
}

#[test]
fn a_command_provisioner_reads_the_address_the_command_prints() {
    let mut p = CommandProvisioner { up: "echo starting && echo 10.1.2.3:50051".into(), down: "exit 0".into() };
    assert_eq!(p.add_node().unwrap(), "10.1.2.3:50051");
    assert!(p.remove_node("10.1.2.3:50051").is_ok());
    let mut bad = CommandProvisioner { up: "exit 3".into(), down: "exit 3".into() };
    assert!(bad.add_node().is_err() && bad.remove_node("x").is_err());
}
