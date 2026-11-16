//! TPC-C and the reduced TPC-E on a cluster: the same schema, data, transaction mix and consistency checks as the single-node
//! `tpc_oltp` example, with every transaction run as statements sent by the terminal through a cluster session (the cluster has
//! no stored procedures or sequences, so the transaction logic is in the driver: `c.rs` / `e.rs` with `client = true`).
//!
//!   cargo run --release -p tsql-control --example oltp_cluster -- [c|e] [--shards 3] [--terminals 8] [--secs 10]
//!                                                                 [--warehouses 6] [--config cluster.json] [--state FILE]
//!
//! By default the shard nodes run in this process (Arrow Flight over TCP, `--shards` of them). With `--config` the control node
//! connects to shard nodes that are already running (`tsql-shard` on other machines): the driver then loads the data through the
//! control node and runs the terminals against it. Terminals are threads, each with a session of its own; the warehouse (TPC-C)
//! is the shard key, so a terminal's transactions stay on one shard except for the payments and new orders that touch another
//! warehouse and the history rows (keyed by customer), which commit across shards by two-phase commit.
//!
//! The exit status is non-zero when a transaction fails (other than as a deadlock victim, which is retried) or a consistency
//! condition is violated.
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

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn table(names: &[&str], done: &[u64], failed: &[u64], lat: &mut [Vec<f64>], problems: &mut u64) {
    println!("{:<18} {:>8} {:>7} {:>10} {:>10} {:>10}", "transaction", "done", "failed", "mean ms", "p90 ms", "max ms");
    for i in 0..names.len() {
        let mean = if lat[i].is_empty() { 0.0 } else { lat[i].iter().sum::<f64>() / lat[i].len() as f64 };
        let (p90, max) = (pct(&mut lat[i], 0.9), pct(&mut lat[i], 1.0));
        println!("{:<18} {:>8} {:>7} {:>10.2} {:>10.2} {:>10.2}", names[i], done[i], failed[i], mean * 1e3, p90 * 1e3, max * 1e3);
        *problems += failed[i];
    }
}

/// Every table either benchmark makes, so that a run starts from nothing (the two benchmarks both have a `customer` table, and a
/// cluster that has been used before has the tables of its last run).
fn drop_all(cs: &mut ClusterSession) {
    for t in [
        "warehouse", "district", "customer", "history", "orders", "new_order", "item", "stock", "order_line", "broker", "customer_account", "security", "last_trade", "trade", "trade_history", "holding", "holding_summary", "watch_list", "watch_item",
    ] {
        let _ = cs.execute(&format!("DROP TABLE IF EXISTS {}", t));
    }
}

fn consistency(cs: &mut ClusterSession, checks: Vec<(&'static str, &'static str)>, problems: &mut u64) {
    for (name, sql) in checks {
        let (res, dt) = timed(cs, sql);
        let how = cs.last_plan().iter().map(|p| p.split_whitespace().next().unwrap_or("").trim_end_matches(':').to_string()).collect::<Vec<_>>().join(",");
        match res {
            Ok(rows) if rows[0][0] == "0" => println!("   consistency {}: ok ({:.3}s; {})", name, dt, how),
            other => {
                *problems += 1;
                println!("   CONSISTENCY VIOLATED {}: {:?}", name, other);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
    let text = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let nshards = flag("--shards").unwrap_or(3.0) as usize;
    let terminals = flag("--terminals").unwrap_or(8.0) as usize;
    let secs = flag("--secs").unwrap_or(10.0);
    let warehouses = flag("--warehouses").unwrap_or((nshards * 2) as f64) as i64;
    let (only_c, only_e) = (args.iter().any(|a| a == "c"), args.iter().any(|a| a == "e"));

    // the shard nodes: in this process, or already running
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut local = vec![];
    let cfg = match text("--config") {
        Some(path) => ClusterConfig::load(&path).unwrap_or_else(|e| {
            eprintln!("{}", e);
            std::process::exit(2)
        }),
        None => {
            for i in 0..nshards {
                local.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
            }
            ClusterConfig { shards: local.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() }
        }
    };
    let nshards = cfg.shards.len();
    let cluster = match text("--state") {
        Some(p) => Cluster::open(std::path::Path::new(&p), &cfg),
        None => Cluster::connect(&cfg),
    }
    .unwrap();
    let mut problems = 0u64;

    if !only_e {
        println!("== TPC-C on {} shards ({} warehouses, {} terminals, {}s) ==", nshards, warehouses, terminals, secs);
        let mut cs = cluster.session();
        drop_all(&mut cs);
        let cfgc = Arc::new(c::Cfg { warehouses, customers: 300, items: 1000 });
        let t = std::time::Instant::now();
        c::load(&mut cs, &cfgc, true, true);
        println!("loaded in {:.2}s", t.elapsed().as_secs_f64());
        let mut initial = 0;
        consistency(&mut cs, c::consistency(), &mut initial);
        problems += initial;
        let cl = cluster.clone();
        let mut tally = c::drive(move || cl.session(), cfgc.clone(), terminals, secs, true);
        table(&["New-Order", "Payment", "Order-Status", "Delivery", "Stock-Level"], &tally.done, &tally.failed, &mut tally.lat, &mut problems);
        println!("New-Orders per minute (tpmC-like): {:.0}; deadlock retries: {}", tally.done[0] as f64 * 60.0 / secs, tally.retries);
        for er in &tally.errors {
            println!("   ERROR {}", er);
        }
        consistency(&mut cs, c::consistency(), &mut problems);
    }

    if !only_c {
        println!("== TPC-E (reduced) on {} shards ({} terminals, {}s) ==", nshards, terminals, secs);
        let mut cs = cluster.session();
        drop_all(&mut cs);
        let cfge = Arc::new(e::Cfg { customers: 500, securities: 200, accounts_per_customer: 2 });
        let t = std::time::Instant::now();
        e::load(&mut cs, &cfge, true, true);
        println!("loaded in {:.2}s", t.elapsed().as_secs_f64());
        let cl = cluster.clone();
        let mut tally = e::drive(move || cl.session(), cfge.clone(), terminals, secs, true);
        table(&e::NAMES, &tally.done, &tally.failed, &mut tally.lat, &mut problems);
        println!("Trade-Results per second (tpsE-like): {:.1}; deadlock retries: {}", tally.done[e::TRADE_RESULT] as f64 / secs, tally.retries);
        for er in &tally.errors {
            println!("   ERROR {}", er);
        }
        consistency(&mut cs, e::consistency(), &mut problems);
    }
    println!("{} problem(s)", problems);
    for s in local {
        rt.block_on(s.shutdown());
    }
    std::process::exit(if problems == 0 { 0 } else { 1 });
}
