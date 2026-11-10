//! TPC-OLTP: the transaction-processing benchmarks, TPC-C and a reduced TPC-E, run against the engine by several
//! concurrent terminals, followed by the benchmark's consistency checks.
//!
//! `cargo run --release --example tpc_oltp [c|e] [--terminals 4] [--secs 10] [--warehouses 2] [--durable] [--pool PAGES]`
//!
//! `--pool` runs on a disk-backed server with that many buffer-pool pages (a small number forces eviction while
//! transactions run).
//!
//! The exit status is non-zero when a transaction fails (other than as a deadlock victim, which is retried) or a
//! consistency condition is violated.
mod c;
mod e;
#[path = "../tpc/util.rs"]
mod util;
use std::sync::Arc;
use tsql_server::{Server, StorageOptions};
use util::*;

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
    let terminals = flag("--terminals").unwrap_or(4.0) as usize;
    let secs = flag("--secs").unwrap_or(10.0);
    let warehouses = flag("--warehouses").unwrap_or(2.0) as i64;
    let durable = args.iter().any(|a| a == "--durable") || flag("--pool").is_some();
    let pool = flag("--pool").map(|p| p as usize);
    let open = |path: std::path::PathBuf| match pool {
        Some(p) => Server::open_with(path, StorageOptions { pool_pages: p, temp_pool_pages: p }).unwrap(),
        None => Server::open(path).unwrap(),
    };
    let only_c = args.iter().any(|a| a == "c");
    let only_e = args.iter().any(|a| a == "e");
    let dir = tempfile::tempdir().unwrap();
    let mut problems = 0;

    if !only_e {
        println!("== TPC-C ({} warehouses, {} terminals, {}s) ==", warehouses, terminals, secs);
        let server = if durable { open(dir.path().join("c")) } else { Server::new() };
        let mut conn = server.connect();
        let cfg = Arc::new(c::Cfg { warehouses, customers: 300, items: 1000 });
        let t = std::time::Instant::now();
        c::load(&mut conn, &cfg);
        println!("loaded in {:.2}s", t.elapsed().as_secs_f64());
        // the initial database must satisfy the conditions too
        for (name, sql) in c::consistency() {
            let (res, _) = timed(&mut conn, sql);
            match res {
                Ok(rows) if rows[0][0] == "0" => {}
                other => {
                    problems += 1;
                    println!("   INITIAL STATE violates condition {}: {:?}", name, other);
                }
            }
        }
        let mut tally = c::drive(&server, cfg.clone(), terminals, secs);
        let names = ["New-Order", "Payment", "Order-Status", "Delivery", "Stock-Level"];
        println!("{:<14} {:>8} {:>7} {:>10} {:>10} {:>10}", "transaction", "done", "failed", "mean ms", "p90 ms", "max ms");
        for i in 0..5 {
            let mean = if tally.lat[i].is_empty() { 0.0 } else { tally.lat[i].iter().sum::<f64>() / tally.lat[i].len() as f64 };
            let (p90, max) = (pct(&mut tally.lat[i], 0.9), pct(&mut tally.lat[i], 1.0));
            println!("{:<14} {:>8} {:>7} {:>10.2} {:>10.2} {:>10.2}", names[i], tally.done[i], tally.failed[i], mean * 1e3, p90 * 1e3, max * 1e3);
            problems += tally.failed[i];
        }
        println!("New-Orders per minute (tpmC-like): {:.0}; deadlock retries: {}", tally.done[0] as f64 * 60.0 / secs, tally.retries);
        for e in &tally.errors {
            println!("   ERROR {}", e);
        }
        for (name, sql) in c::consistency() {
            let (res, dt) = timed(&mut conn, sql);
            if dt > 1.0 {
                for l in conn.last_plan().iter().take(12) {
                    println!("      plan: {}", l);
                }
            }
            match res {
                Ok(rows) if rows[0][0] == "0" => println!("   consistency {}: ok ({:.3}s)", name, dt),
                other => {
                    problems += 1;
                    println!("   CONSISTENCY VIOLATED {}: {:?}", name, other);
                }
            }
        }
    }

    if !only_c {
        println!("== TPC-E (reduced; {} terminals, {}s) ==", terminals, secs);
        let server = if durable { open(dir.path().join("e")) } else { Server::new() };
        let mut conn = server.connect();
        let cfg = Arc::new(e::Cfg { customers: 500, securities: 200, accounts_per_customer: 2 });
        let t = std::time::Instant::now();
        e::load(&mut conn, &cfg);
        println!("loaded in {:.2}s", t.elapsed().as_secs_f64());
        let mut tally = e::drive(&server, cfg.clone(), terminals, secs);
        println!("{:<18} {:>8} {:>7} {:>10} {:>10} {:>10}", "transaction", "done", "failed", "mean ms", "p90 ms", "max ms");
        for i in 0..e::NAMES.len() {
            let mean = if tally.lat[i].is_empty() { 0.0 } else { tally.lat[i].iter().sum::<f64>() / tally.lat[i].len() as f64 };
            let (p90, max) = (pct(&mut tally.lat[i], 0.9), pct(&mut tally.lat[i], 1.0));
            println!("{:<18} {:>8} {:>7} {:>10.2} {:>10.2} {:>10.2}", e::NAMES[i], tally.done[i], tally.failed[i], mean * 1e3, p90 * 1e3, max * 1e3);
            problems += tally.failed[i];
        }
        println!("Trade-Results per second (tpsE-like): {:.1}; deadlock retries: {}", tally.done[e::TRADE_RESULT] as f64 / secs, tally.retries);
        for er in &tally.errors {
            println!("   ERROR {}", er);
        }
        for (name, sql) in e::consistency() {
            let (res, dt) = timed(&mut conn, sql);
            if dt > 1.0 {
                for l in conn.last_plan().iter().take(12) {
                    println!("      plan: {}", l);
                }
            }
            match res {
                Ok(rows) if rows[0][0] == "0" => println!("   consistency {}: ok ({:.3}s)", name, dt),
                other => {
                    problems += 1;
                    println!("   CONSISTENCY VIOLATED {}: {:?}", name, other);
                }
            }
        }
    }
    println!("{} problem(s)", problems);
    if problems > 0 {
        std::process::exit(1);
    }
}
