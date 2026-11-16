//! The SQLsmith-style fuzzer against a cluster: random, valid-looking T-SQL queries (the generator of the single-node
//! `sqlsmith` example) run on one engine and on a cluster holding the same data, and the answers must agree. A statement the
//! cluster refuses on purpose (error 50100, "not supported in a cluster") is counted, not reported; anything else that differs
//! (an answer, an error number, success against failure, a panic) is a problem.
//!
//!   cargo run --release -p tsql-control --example sqlsmith_cluster -- [iterations] [--seed N] [--shards 3] [--broadcast N] [--verbose]
//!
//! `t1` and `t2` are sharded on `id`; `t3` is replicated (so queries mix sharded and replicated tables). Every 20 iterations
//! a data change (`UPDATE`, `DELETE`, `INSERT`) is applied to both and the tables must stay identical; every 100 the data is
//! made again. The exit status is non-zero when a problem was found.
#[allow(dead_code, unused_imports, unused_variables)]
#[path = "../../../../examples/sqlsmith.rs"]
mod smith;

use rand::{Rng, SeedableRng};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use tsql_control::{Cluster, ClusterSession};
use tsql_server::{Connection, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, ShardNode};

type Rows = Result<Vec<Vec<String>>, (i32, String)>;

fn rows_of(r: &tsql_server::BatchResult) -> Rows {
    if let Some(e) = r.first_error() {
        return Err((e.number, format!("{}", e)));
    }
    Ok(r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect()).unwrap_or_default())
}

fn guarded(f: impl FnOnce() -> tsql_server::BatchResult) -> Result<Rows, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Ok(rows_of(&r)),
        Err(p) => Err(format!("PANIC: {}", p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())),
    }
}

fn load_both(one: &mut Connection, cs: &mut ClusterSession) {
    smith::load_with(
        &mut |sql| match one.execute(sql).first_error() {
            None => Ok(()),
            Some(e) => Err(format!("{:?}", e)),
        },
        99,
    );
    smith::load_with(
        &mut |sql| {
            if sql.starts_with("INSERT t3") {
                // (a table is spread as the cluster is told while it is empty)
                let r = cs.execute("DISTRIBUTE TABLE t3 REPLICATED");
                if let Some(e) = r.first_error() {
                    return Err(format!("{:?}", e));
                }
            }
            match cs.execute(sql).first_error() {
                None => Ok(()),
                Some(e) => Err(format!("{:?}", e)),
            }
        },
        99,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let iterations: u64 = args.iter().find_map(|a| a.parse().ok()).unwrap_or(800);
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
    let seed = flag("--seed").unwrap_or(1);
    let nshards = flag("--shards").unwrap_or(3) as usize;
    let verbose = args.iter().any(|a| a == "--verbose");

    let server = Server::new();
    let mut one = server.connect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut shards = vec![];
    for i in 0..nshards {
        shards.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
    }
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = Cluster::connect(&cfg).unwrap();
    // `--broadcast N`: no table over N rows is broadcast, so that joins have to shuffle (the staged joins of `mpp.rs`)
    if let Some(n) = flag("--broadcast") {
        cluster.set_broadcast_limit(n);
    }
    let mut cs = cluster.session();
    load_both(&mut one, &mut cs);

    let mut g = smith::Gen { rng: rand::rngs::StdRng::seed_from_u64(seed), tables: smith::tables(), counter: 0 };
    let (mut same_ok, mut same_err, mut unsupported, mut problems) = (0u64, 0u64, 0u64, 0u64);
    let mut plans: BTreeMap<String, u64> = Default::default();
    let mut report = |what: &str, sql: &str, detail: &str, problems: &mut u64| {
        *problems += 1;
        if *problems <= 25 {
            println!("PROBLEM ({}): {}\n   {}", what, detail, sql);
        }
    };
    for it in 0..iterations {
        let sql = g.query();
        if verbose && it % 100 == 0 {
            println!("[{}] {}", it, sql);
        }
        let a = match guarded(|| one.execute(&sql)) {
            Ok(r) => r,
            Err(p) => {
                report("crash (one engine)", &sql, &p, &mut problems);
                continue;
            }
        };
        let b = match guarded(|| cs.execute(&sql)) {
            Ok(r) => r,
            Err(p) => {
                report("crash (cluster)", &sql, &p, &mut problems);
                continue;
            }
        };
        let kind = cs.last_plan().iter().map(|p| p.split_whitespace().next().unwrap_or("").trim_end_matches(':').to_string()).find(|k| matches!(k.as_str(), "gather" | "mpp" | "split" | "set" | "scatter-gather" | "route" | "join" | "direct" | "local")).unwrap_or_else(|| "other".into());
        *plans.entry(kind).or_default() += 1;
        match (&a, &b) {
            (Ok(x), Ok(y)) => match smith::same(x, y) {
                Some(d) => report("different answers", &sql, &d, &mut problems),
                None => same_ok += 1,
            },
            (_, Err((50100, _))) => unsupported += 1,
            (Err((n1, _)), Err((n2, _))) if n1 == n2 => same_err += 1,
            (Ok(_), Err((n, m))) => report("different outcome", &sql, &format!("one engine succeeded, the cluster failed with {}: {}", n, m), &mut problems),
            (Err((n, m)), Ok(_)) => report("different outcome", &sql, &format!("one engine failed with {}: {}, the cluster succeeded", n, m), &mut problems),
            (Err((n1, m1)), Err((n2, m2))) => report("different errors", &sql, &format!("{}: {} against {}: {}", n1, m1, n2, m2), &mut problems),
        }
        // data changes, applied to both; the tables must stay identical
        if it % 20 == 7 {
            let ti = g.rng.gen_range(0..3);
            let t = g.tables[ti].name;
            let avail: Vec<smith::Avail> = g.tables[ti].cols.iter().map(|c| smith::Avail { alias: t.to_string(), name: c.name.to_string(), ty: c.ty }).collect();
            let p = g.pred(&avail, 1);
            let dml = match g.rng.gen_range(0..4) {
                0 => format!("UPDATE {t} SET a = {} WHERE {}", g.int_expr(&avail, 1), p, t = t),
                1 => format!("UPDATE {t} SET c = {}, d = {} WHERE {}", g.str_expr(&avail, 1), g.dec_expr(&avail, 1), p, t = t),
                2 => format!("DELETE FROM {} WHERE {}", t, p),
                _ => format!("INSERT INTO {t} (id, a, b, c, d, e) VALUES ({}, 1, 2, 'new', 3.5, '2021-01-01')", 1000 + it, t = t),
            };
            let x = guarded(|| one.execute(&dml));
            let y = guarded(|| cs.execute(&dml));
            match (&x, &y) {
                (Ok(Ok(_)), Ok(Ok(_))) => {}
                (Ok(Err((n1, _))), Ok(Err((n2, _)))) if n1 == n2 => {}
                (_, Ok(Err((50100, _)))) => {
                    // the cluster refused: put the two back in step by repeating nothing on the cluster, so redo the data
                    unsupported += 1;
                    for tn in ["t1", "t2", "t3"] {
                        let _ = one.execute(&format!("DROP TABLE {}", tn));
                        let _ = cs.execute(&format!("DROP TABLE {}", tn));
                    }
                    load_both(&mut one, &mut cs);
                }
                _ => report("different outcome of a data change", &dml, &format!("{:?} against {:?}", x.as_ref().map(|r| r.as_ref().map(|_| "ok")), y.as_ref().map(|r| r.as_ref().map(|_| "ok"))), &mut problems),
            }
            for tn in ["t1", "t2", "t3"] {
                let q = format!("SELECT * FROM {} ORDER BY id", tn);
                if let (Ok(Ok(x)), Ok(Ok(y))) = (guarded(|| one.execute(&q)), guarded(|| cs.execute(&q))) {
                    if let Some(d) = smith::same(&x, &y) {
                        report("tables diverged after a data change", &dml, &format!("{}: {}", tn, d), &mut problems);
                        // go on from the same data
                        for tn in ["t1", "t2", "t3"] {
                            let _ = one.execute(&format!("DROP TABLE {}", tn));
                            let _ = cs.execute(&format!("DROP TABLE {}", tn));
                        }
                        load_both(&mut one, &mut cs);
                        break;
                    }
                }
            }
        }
        if it % 100 == 99 {
            for tn in ["t1", "t2", "t3"] {
                let _ = one.execute(&format!("DROP TABLE {}", tn));
                let _ = cs.execute(&format!("DROP TABLE {}", tn));
            }
            load_both(&mut one, &mut cs);
        }
    }
    println!("{} queries: {} same answers, {} same errors, {} refused by the cluster on purpose; {} problem(s)", iterations, same_ok, same_err, unsupported, problems);
    println!("how the cluster ran them: {:?}", plans);
    for s in shards {
        rt.block_on(s.shutdown());
    }
    std::process::exit(if problems == 0 { 0 } else { 1 });
}
