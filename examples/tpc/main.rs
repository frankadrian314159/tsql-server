//! TPC: the decision-support benchmarks, TPC-H and TPC-DS, run against the engine.
//!
//! `cargo run --release --example tpc [h|ds] [--sf 0.01] [--durable]`
//!
//! Each benchmark loads generated data, runs every query once (timing it), and checks what it can: queries that
//! fail are reported, and selected TPC-H answers are recomputed in Rust from the generated rows. The exit status is
//! non-zero when any query errors or any check fails.
mod ds;
mod h;
mod util;
use std::time::Instant;
use tsql_server::Server;
use util::*;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f64>().ok());
    let flag_s = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let sf = flag("--sf").unwrap_or(0.01);
    let durable = args.iter().any(|a| a == "--durable");
    let compare = args.iter().any(|a| a == "--compare");
    let only_h = args.iter().any(|a| a == "h");
    let only_ds = args.iter().any(|a| a == "ds");
    let dir = tempfile::tempdir().unwrap();
    let mut failures = 0;
    let mut total = 0.0;

    if !only_ds {
        println!("== TPC-H (scale factor {}) ==", sf);
        let server = if durable { Server::open(dir.path().join("h")).unwrap() } else { Server::new() };
        if let Some(mb) = flag("--work-mb") {
            server.set_work_mem((mb * 1024.0 * 1024.0) as usize, 20_000, None);
        }
        let mut c = server.connect();
        let t = Instant::now();
        let data = h::load(&mut c, sf);
        println!("loaded in {:.2}s", t.elapsed().as_secs_f64());
        // --sql "statement" (repeatable) runs ad hoc statements against the loaded data and prints their times and plans
        let adhoc: Vec<String> = args.iter().enumerate().filter(|(_, a)| *a == "--sql").filter_map(|(i, _)| args.get(i + 1).cloned()).collect();
        if !adhoc.is_empty() {
            for sql in adhoc {
                let (res, dt) = timed(&mut c, &sql);
                println!("{:.3}s {}", dt, if res.is_ok() { "ok" } else { "ERROR" });
                if let Ok(rows) = &res { println!("   {} row(s): {:?}", rows.len(), rows.iter().take(6).collect::<Vec<_>>()); } else if let Err(e) = &res { println!("   {}", e); }
                if let Ok(rows) = &res { for r in rows.iter().take(8) { println!("   row: {}", r.join(" | ")); } }
                if let Ok(rows) = &res { for r in rows.iter().take(12) { println!("   | {}", r.join(" | ")); } }
                for l in c.last_plan() {
                    println!("      plan: {}", l);
                }
            }
            tsql_server::prof::report();
            return;
        }
        // with --compare every query also runs on a second server holding the same data but a tiny work-memory budget,
        // which makes it use the streaming and spilling executors; the two answers must agree
        let mut other = if compare {
            let s2 = Server::new();
            s2.set_work_mem(256 << 10, 0, None);
            let mut c2 = s2.connect();
            h::load(&mut c2, sf);
            Some((s2, c2))
        } else {
            None
        };
        let mut report = Report::default();
        for (name, sql) in h::queries(sf) {
            if let Some(only) = flag_s("--query") {
                if !name.starts_with(&format!("{} ", only)) {
                    continue;
                }
            }
            let (res, dt) = timed(&mut c, &sql);
            if dt > 0.5 || args.iter().any(|a| a == "--plans") {
                for l in c.last_plan() {
                    println!("      plan: {}", l);
                }
            }
            if let (Some((_, c2)), Ok(rows)) = (other.as_mut(), res.as_ref()) {
                let (res2, dt2) = timed(c2, &sql);
                match res2 {
                    Err(e) => {
                        failures += 1;
                        println!("   COMPARE {}: the spilling server failed: {}", name, e);
                    }
                    Ok(rows2) => {
                        if let Some(why) = compare_rows(rows, &rows2) {
                            failures += 1;
                            println!("   COMPARE {}: results differ: {}", name, why);
                        } else {
                            println!("   compare {}: same ({:.3}s spilling)", name, dt2);
                        }
                    }
                }
            }
            match res {
                Err(e) => {
                    failures += 1;
                    report.add(&name, dt, 0, &format!("ERROR: {}", e));
                }
                Ok(rows) => match h::check(&data, &name, &rows) {
                    Some(Err(why)) => {
                        failures += 1;
                        report.add(&name, dt, rows.len(), &format!("WRONG: {}", why));
                    }
                    Some(Ok(())) => report.add(&name, dt, rows.len(), "verified"),
                    None => report.add(&name, dt, rows.len(), ""),
                },
            }
        }
        println!("TPC-H total {:.2}s", report.total());
        total += report.total();
    }

    if !only_h {
        println!("== TPC-DS (scale {}) ==", sf);
        let server = if durable { Server::open(dir.path().join("ds")).unwrap() } else { Server::new() };
        if let Some(mb) = flag("--work-mb") {
            server.set_work_mem((mb * 1024.0 * 1024.0) as usize, 0, None);
        }
        let mut c = server.connect();
        let t = Instant::now();
        let data = ds::load(&mut c, sf);
        let adhoc: Vec<String> = args.iter().enumerate().filter(|(_, a)| *a == "--sql").filter_map(|(i, _)| args.get(i + 1).cloned()).collect();
        if !adhoc.is_empty() {
            for sql in adhoc {
                let (res, dt) = timed(&mut c, &sql);
                println!("{:.3}s {}", dt, if res.is_ok() { "ok" } else { "ERROR" });
                if let Ok(rows) = &res { println!("   {} row(s): {:?}", rows.len(), rows.iter().take(6).collect::<Vec<_>>()); } else if let Err(e) = &res { println!("   {}", e); }
                if let Ok(rows) = &res { for r in rows.iter().take(8) { println!("   row: {}", r.join(" | ")); } }
                if let Ok(rows) = &res { for r in rows.iter().take(12) { println!("   | {}", r.join(" | ")); } }
                for l in c.last_plan() {
                    println!("      plan: {}", l);
                }
            }
            tsql_server::prof::report();
            return;
        }
        println!("loaded in {:.2}s", t.elapsed().as_secs_f64());
        // with --compare every query also runs on a second server holding the same data but a tiny work-memory budget,
        // which makes it use the streaming and spilling executors; the two answers must agree
        let mut other = if compare {
            let s2 = Server::new();
            s2.set_work_mem(256 << 10, 0, None);
            let mut c2 = s2.connect();
            ds::load(&mut c2, sf);
            Some((s2, c2))
        } else {
            None
        };
        let mut report = Report::default();
        for (name, sql) in ds::queries() {
            let (res, dt) = timed(&mut c, &sql);
            if dt > 0.5 || args.iter().any(|a| a == "--plans") {
                for l in c.last_plan() {
                    println!("      plan: {}", l);
                }
            }
            if let (Some((_, c2)), Ok(rows)) = (other.as_mut(), res.as_ref()) {
                let (res2, dt2) = timed(c2, &sql);
                match res2 {
                    Err(e) => {
                        failures += 1;
                        println!("   COMPARE {}: the spilling server failed: {}", name, e);
                    }
                    Ok(rows2) => {
                        if let Some(why) = compare_rows(rows, &rows2) {
                            failures += 1;
                            println!("   COMPARE {}: results differ: {}", name, why);
                        } else {
                            println!("   compare {}: same ({:.3}s spilling)", name, dt2);
                        }
                    }
                }
            }
            match res {
                Err(e) => {
                    failures += 1;
                    report.add(&name, dt, 0, &format!("ERROR: {}", e));
                }
                Ok(rows) => match ds::check(&data, &name, &rows) {
                    Some(Err(why)) => {
                        failures += 1;
                        report.add(&name, dt, rows.len(), &format!("WRONG: {}", why));
                    }
                    Some(Ok(())) => report.add(&name, dt, rows.len(), "verified"),
                    None => report.add(&name, dt, rows.len(), ""),
                },
            }
        }
        println!("TPC-DS total {:.2}s", report.total());
        total += report.total();
    }
    tsql_server::prof::report();
    println!("total {:.2}s, {} problem(s)", total, failures);
    if failures > 0 {
        std::process::exit(1);
    }
}
