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
