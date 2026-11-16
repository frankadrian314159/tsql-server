//! The TPC-C and TPC-E drivers of `examples/tpc_oltp`, run on a cluster for a moment: every transaction is statements sent
//! by a terminal through a cluster session (client-side transactions), and the benchmarks' consistency conditions must hold
//! afterwards. The same drivers run on one engine in `tpc_oltp --client`.
#[path = "../../../../examples/tpc_oltp/c.rs"]
mod c;
#[path = "../../../../examples/tpc_oltp/e.rs"]
mod e;
#[path = "../../../../examples/tpc/util.rs"]
mod util;

use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::Server;
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, ShardNode};
use util::*;

impl Sql for ClusterSession {
    fn exec(&mut self, sql: &str) -> Result<Out, DbErr> {
        out_of(&self.execute(sql))
    }
}

fn start(n: usize) -> (tokio::runtime::Runtime, Vec<tsql_storage::RunningShard>, Arc<Cluster>) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shards: Vec<_> = (0..n).map(|i| rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap()).collect();
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = Cluster::connect(&cfg).unwrap();
    (rt, shards, cluster)
}

fn violations(cs: &mut ClusterSession, checks: Vec<(&'static str, &'static str)>) -> Vec<String> {
    let mut bad = vec![];
    for (name, sql) in checks {
        match timed(cs, sql).0 {
            Ok(rows) if rows[0][0] == "0" => {}
            other => bad.push(format!("{}: {:?}", name, other)),
        }
    }
    bad
}

#[test]
fn tpcc_runs_on_a_cluster_and_keeps_its_invariants() {
    let (rt, shards, cluster) = start(2);
    let mut cs = cluster.session();
    let cfg = Arc::new(c::Cfg { warehouses: 4, customers: 30, items: 100 });
    c::load(&mut cs, &cfg, true, true);
    assert!(violations(&mut cs, c::consistency()).is_empty(), "the initial state violates a condition");
    let cl = cluster.clone();
    let tally = c::drive(move || cl.session(), cfg, 4, 1.5, true);
    assert!(tally.failed.iter().all(|f| *f == 0), "{:?}", tally.errors);
    assert!(tally.done.iter().all(|d| *d > 0), "every transaction ran: {:?}", tally.done);
    let bad = violations(&mut cs, c::consistency());
    assert!(bad.is_empty(), "{:?}", bad);
    drop(cs);
    for s in shards {
        rt.block_on(s.shutdown());
    }
}

#[test]
fn tpce_runs_on_a_cluster_and_keeps_its_invariants() {
    let (rt, shards, cluster) = start(2);
    let mut cs = cluster.session();
    let cfg = Arc::new(e::Cfg { customers: 60, securities: 40, accounts_per_customer: 2 });
    e::load(&mut cs, &cfg, true, true);
    assert!(violations(&mut cs, e::consistency()).is_empty(), "the initial state violates a condition");
    let cl = cluster.clone();
    let tally = e::drive(move || cl.session(), cfg, 4, 1.5, true);
    assert!(tally.failed.iter().all(|f| *f == 0), "{:?}", tally.errors);
    assert!(tally.done[e::TRADE_RESULT] > 0, "trades completed: {:?}", tally.done);
    let bad = violations(&mut cs, e::consistency());
    assert!(bad.is_empty(), "{:?}", bad);
    drop(cs);
    for s in shards {
        rt.block_on(s.shutdown());
    }
}

/// The terminals spread over several control nodes (`Cluster::attach`), each with its own planner, engine and sessions, over the
/// same shards: the invariants hold, and the front ends do not disturb each other's transactions.
#[test]
fn tpcc_and_tpce_run_through_several_control_nodes() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shards: Vec<_> = (0..2).map(|i| rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap()).collect();
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let state = std::env::temp_dir().join(format!("tsql_oltp_state_{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let owner = Cluster::open(&state, &cfg).unwrap();
    let mut cs = owner.session();
    let c_cfg = Arc::new(c::Cfg { warehouses: 4, customers: 30, items: 100 });
    c::load(&mut cs, &c_cfg, true, true);
    // the other front ends read the catalogue from the file, and do not touch it (or settle what the shards hold prepared)
    let before = std::fs::read(&state).unwrap();
    let fronts: Vec<_> = (0..3).map(|_| Cluster::attach(&state).unwrap()).collect();
    assert_eq!(std::fs::read(&state).unwrap(), before, "attaching leaves the state file alone");
    let hs: Vec<_> = fronts
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (f, cfg) = (f.clone(), c_cfg.clone());
            std::thread::spawn(move || c::drive_with(move || f.session(), cfg, 2, 1.5, true, i * 2))
        })
        .collect();
    let mut tally = c::Tally::new();
    for h in hs {
        tally.merge(h.join().unwrap());
    }
    assert!(tally.failed.iter().all(|f| *f == 0), "{:?}", tally.errors);
    assert!(tally.done.iter().all(|d| *d > 0), "{:?}", tally.done);
    let bad = violations(&mut cs, c::consistency());
    assert!(bad.is_empty(), "{:?}", bad);

    // the same for the brokerage: the trades of each front end are in a range of their own
    let mut cs2 = owner.session();
    for t in ["warehouse", "district", "customer", "history", "orders", "new_order", "item", "stock", "order_line"] {
        let _ = cs2.execute(&format!("DROP TABLE IF EXISTS {}", t));
    }
    let e_cfg = Arc::new(e::Cfg { customers: 60, securities: 40, accounts_per_customer: 2 });
    e::load(&mut cs2, &e_cfg, true, true);
    let fronts: Vec<_> = (0..3).map(|_| Cluster::attach(&state).unwrap()).collect();
    let hs: Vec<_> = fronts
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (f, cfg) = (f.clone(), e_cfg.clone());
            std::thread::spawn(move || e::drive_with(move || f.session(), cfg, 2, 1.5, true, i * 2))
        })
        .collect();
    let mut failed = 0;
    let mut completed = 0;
    for h in hs {
        let t = h.join().unwrap();
        failed += t.failed.iter().sum::<u64>();
        completed += t.done[e::TRADE_RESULT];
    }
    assert_eq!(failed, 0);
    assert!(completed > 0);
    let bad = violations(&mut cs2, e::consistency());
    assert!(bad.is_empty(), "{:?}", bad);
    drop((cs, cs2, fronts, owner));
    for s in shards {
        rt.block_on(s.shutdown());
    }
    let _ = std::fs::remove_file(&state);
}
