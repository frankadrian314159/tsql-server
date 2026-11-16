//! The engine micro-benchmark (`bench`) on a cluster: the same table, rows and workloads (inserts, point lookups, scans,
//! aggregates, joins, sorts, windows, set operations, updates, deletes, transactions, concurrent sessions) run on one engine and
//! on a cluster, and for each workload the table shows both times, how the cluster ran it, and whether the answers agree.
//!
//!   cargo run --release -p tsql-control --example bench_cluster -- [rows] [--shards 3] [--config cluster.json] [--state FILE]
//!
//! With `--config` the shard nodes are already running (`tsql-shard` on other machines) and only the control node is in this
//! process. Left out, because a cluster has no procedures or T-SQL loops: the stored-procedure, `sp_executesql` and `WHILE`
//! workloads of the single-node benchmark. The exit status is non-zero when an answer differs.
use std::time::Instant;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::{Connection, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, ShardNode};

fn rows_of(r: &tsql_server::BatchResult) -> Result<Vec<Vec<String>>, String> {
    if let Some(e) = r.first_error() {
        return Err(format!("{}", e));
    }
    Ok(r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect()).unwrap_or_default())
}

fn close(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 0.011 + 1e-9 * x.abs().max(y.abs()),
        _ => false,
    }
}

/// None when the answers agree (rows compared as sets: the order of equal sort keys may differ).
fn differ(a: &[Vec<String>], b: &[Vec<String>]) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("{} rows against {}", a.len(), b.len()));
    }
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    a.sort();
    b.sort();
    for (x, y) in a.iter().zip(&b) {
        if x.len() != y.len() || x.iter().zip(y).any(|(u, v)| u != v && !close(u, v)) {
            return Some(format!("{:?} against {:?}", x, y));
        }
    }
    None
}

struct Bench {
    one: Connection,
    cs: ClusterSession,
    problems: u32,
    total: (f64, f64),
}

impl Bench {
    fn plan(&self) -> String {
        self.cs.last_plan().iter().map(|p| p.split_whitespace().next().unwrap_or("").trim_end_matches(':').to_string()).collect::<Vec<_>>().join(",")
    }

    fn line(&mut self, name: &str, a: f64, b: f64, note: &str) {
        self.total.0 += a;
        self.total.1 += b;
        println!("{:<40} {:>9.3} {:>9.3}  {}", name, a, b, note);
    }

    /// Run a statement on both and compare.
    fn both(&mut self, name: &str, sql: &str) {
        let t = Instant::now();
        let a = self.one.execute(sql);
        let ta = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let b = self.cs.execute(sql);
        let tb = t.elapsed().as_secs_f64();
        let verdict = match (rows_of(&a), rows_of(&b)) {
            (Ok(x), Ok(y)) => match differ(&x, &y) {
                None => "same".to_string(),
                Some(d) => {
                    self.problems += 1;
                    format!("DIFFERENT: {}", d)
                }
            },
            (Err(x), Err(y)) if x == y => "same error".to_string(),
            (x, y) => {
                self.problems += 1;
                format!("DIFFERENT: {:?} against {:?}", x.err(), y.err())
            }
        };
        let note = format!("{:<44} {}", self.plan(), verdict);
        self.line(name, ta, tb, &note);
    }

    /// Run the same function against each (the work is repeated statements, so the answers are not compared).
    fn repeat(&mut self, name: &str, n: usize, one: impl Fn(usize) -> String, check: bool) {
        let t = Instant::now();
        for k in 0..n {
            let r = self.one.execute(&one(k));
            assert!(r.first_error().is_none(), "{}: {:?}", name, r.first_error());
        }
        let ta = t.elapsed().as_secs_f64();
        let t = Instant::now();
        for k in 0..n {
            let r = self.cs.execute(&one(k));
            assert!(r.first_error().is_none(), "{}: {:?}", name, r.first_error());
        }
        let tb = t.elapsed().as_secs_f64();
        let note = format!("{:<44} {:.0} / {:.0} stmt/s", self.plan(), n as f64 / ta, n as f64 / tb);
        self.line(&format!("{} x {}", n, name), ta, tb, &note);
        if check {
            // the effect of the statements is the same
            let q = "SELECT COUNT(*), SUM(val), SUM(grp) FROM t";
            let (x, y) = (rows_of(&self.one.execute(q)).unwrap(), rows_of(&self.cs.execute(q)).unwrap());
            if let Some(d) = differ(&x, &y) {
                self.problems += 1;
                println!("   DIFFERENT after {}: {}", name, d);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rows: usize = args.iter().find_map(|a| a.parse().ok()).unwrap_or(20_000);
    let text = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let nshards = text("--shards").and_then(|v| v.parse().ok()).unwrap_or(3usize);

    let server = Server::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut local = vec![];
    let cfg = match text("--config") {
        Some(p) => ClusterConfig::load(&p).unwrap_or_else(|e| {
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
    let cluster = match text("--state") {
        Some(p) => Cluster::open(std::path::Path::new(&p), &cfg),
        None => Cluster::connect(&cfg),
    }
    .unwrap();
    let mut b = Bench { one: server.connect(), cs: cluster.session(), problems: 0, total: (0.0, 0.0) };
    for t in ["t", "d"] {
        let _ = b.cs.execute(&format!("DROP TABLE IF EXISTS {}", t));
    }
    println!("{:<40} {:>9} {:>9}  {}", format!("workload ({} rows, {} shards)", rows, cfg.shards.len()), "one", "cluster", "how the cluster ran it; answers");

    b.both("create tables", "CREATE TABLE t (id INT PRIMARY KEY, grp INT, name VARCHAR(40), val FLOAT, note NVARCHAR(60));\nCREATE TABLE d (id INT PRIMARY KEY, label VARCHAR(30));\nCREATE INDEX ix_grp ON t (grp);");
    let r = b.cs.execute("DISTRIBUTE TABLE d REPLICATED");
    assert!(r.first_error().is_none(), "{:?}", r.first_error());
    // multi-row inserts in batches of 500
    let batches: Vec<String> = (0..rows)
        .step_by(500)
        .map(|i| format!("INSERT t VALUES {}", (i..(i + 500).min(rows)).map(|k| format!("({}, {}, 'name{}', {}.5, N'note {}')", k, (k * 7919) % 1000, k % 977, (k * 13) % 997, k % 31)).collect::<Vec<_>>().join(",")))
        .collect();
    let t = Instant::now();
    for s in &batches {
        assert!(b.one.execute(s).first_error().is_none());
    }
    let ta = t.elapsed().as_secs_f64();
    let t = Instant::now();
    for s in &batches {
        let r = b.cs.execute(s);
        assert!(r.first_error().is_none(), "{:?}", r.first_error());
    }
    let tb = t.elapsed().as_secs_f64();
    b.line(&format!("insert {} rows (500/stmt)", rows), ta, tb, &format!("{:.0} / {:.0} rows/s", rows as f64 / ta, rows as f64 / tb));
    b.both("insert dim 1000 rows", &(0..1000).map(|k| format!("INSERT d VALUES ({}, 'label {}');", k, k)).collect::<String>());

    b.repeat("single-row autocommit inserts", 2000, |k| format!("INSERT t VALUES ({}, {}, 'x', 1.0, N'y')", rows + k, k % 1000), true);
    b.repeat("SELECT 1 (statement overhead)", 5000, |_| "SELECT 1".to_string(), false);
    b.repeat("point lookups by PK", 5000, |k| format!("SELECT name, val FROM t WHERE id = {}", (k * 31) % rows), false);

    for (name, sql) in [
        ("count(*)", "SELECT COUNT(*) FROM t"),
        ("full scan with filter", "SELECT COUNT(*) FROM t WHERE val > 500 AND name LIKE 'name1%'"),
        ("index range (grp = 5)", "SELECT COUNT(*), SUM(val) FROM t WHERE grp = 5"),
        ("group by + order by", "SELECT grp, COUNT(*) n, AVG(val) a FROM t GROUP BY grp ORDER BY n DESC, grp"),
        ("group by 1000 keys, 3 aggs", "SELECT name, MIN(val), MAX(val), SUM(id) FROM t GROUP BY name"),
        ("order by top 100", "SELECT TOP 100 id, name FROM t ORDER BY val DESC, id"),
        ("order by all rows", "SELECT id, name, val FROM t ORDER BY name, id"),
        ("distinct", "SELECT DISTINCT name, grp FROM t"),
        ("count distinct", "SELECT COUNT(DISTINCT grp), COUNT(DISTINCT name) FROM t"),
        ("join t-d (replicated)", "SELECT COUNT(*) FROM t JOIN d ON t.grp = d.id"),
        ("join + group", "SELECT d.label, COUNT(*) FROM t JOIN d ON t.grp = d.id GROUP BY d.label"),
        ("self join on pk (co-located)", "SELECT COUNT(*) FROM t a JOIN t b ON a.id = b.id WHERE a.grp = 3"),
        ("self join on grp (shuffle)", "SELECT COUNT(*) FROM t a JOIN t b ON a.grp = b.grp WHERE a.id < 50 AND b.id < 200"),
        ("subquery in select list", "SELECT TOP 2000 id, (SELECT label FROM d WHERE d.id = t.grp) FROM t ORDER BY id"),
        ("exists subquery (correlated)", "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp AND d.id < 500)"),
        ("scalar subquery (correlated)", "SELECT COUNT(*) FROM t WHERE val > (SELECT AVG(x.val) FROM t x WHERE x.grp = t.grp)"),
        ("window row_number", "SELECT COUNT(*) FROM (SELECT ROW_NUMBER() OVER (PARTITION BY grp ORDER BY id) rn FROM t) x WHERE rn = 1"),
        ("window over aggregates", "SELECT grp, COUNT(*) n, SUM(COUNT(*)) OVER () total, RANK() OVER (ORDER BY COUNT(*) DESC, grp) r FROM t GROUP BY grp"),
        ("rollup", "SELECT grp, name, COUNT(*) FROM t WHERE grp < 5 GROUP BY ROLLUP (grp, name)"),
        ("union + distinct", "SELECT COUNT(*) FROM (SELECT name FROM t UNION SELECT name FROM t WHERE id < 1000) x"),
        ("intersect", "SELECT COUNT(*) FROM (SELECT name FROM t WHERE grp < 500 INTERSECT SELECT name FROM t WHERE grp >= 500) x"),
        ("string functions", "SELECT COUNT(*) FROM t WHERE UPPER(name) = 'NAME5' OR LEN(note) > 6"),
        ("cast/convert + arithmetic", "SELECT SUM(CAST(id AS BIGINT) * 2 + grp) FROM t"),
    ] {
        b.both(name, sql);
    }

    b.repeat("single-row updates by PK", 2000, |k| format!("UPDATE t SET val = val + 1 WHERE id = {}", (k * 17) % rows), true);
    b.both("update all rows (non-key)", "UPDATE t SET val = val * 2");
    b.both("update all rows (indexed col)", "UPDATE t SET grp = grp + 1");
    b.both("delete a quarter", "DELETE FROM t WHERE id % 4 = 0");
    b.both("explicit transaction, 1000 inserts", &format!("BEGIN TRAN; {} COMMIT", (0..1000).map(|k| format!("INSERT d VALUES ({}, 'z');", 100_000 + k)).collect::<String>()));
    b.both("the data after all of that", "SELECT COUNT(*), SUM(val), SUM(grp), MIN(id), MAX(id) FROM t");

    // concurrent sessions of the cluster: point lookups, and single-row updates of different rows
    for threads in [1usize, 4, 8] {
        let per = 3000usize;
        let t = Instant::now();
        let hs: Vec<_> = (0..threads)
            .map(|th| {
                let mut s = cluster.session();
                std::thread::spawn(move || {
                    for k in 0..per {
                        let r = s.execute(&format!("SELECT name, val FROM t WHERE id = {}", (k * 31 + th * 7) % rows));
                        assert!(r.first_error().is_none());
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let dt = t.elapsed().as_secs_f64();
        b.line(&format!("{} session(s) x {} lookups", threads, per), 0.0, dt, &format!("{:.0} stmt/s", (threads * per) as f64 / dt));
    }
    for threads in [1usize, 4, 8] {
        let per = 1000usize;
        let t = Instant::now();
        let hs: Vec<_> = (0..threads)
            .map(|th| {
                let mut s = cluster.session();
                std::thread::spawn(move || {
                    for k in 0..per {
                        let r = s.execute(&format!("UPDATE t SET val = val + 1 WHERE id = {}", (k * 8 + th) * 4 + 1));
                        assert!(r.first_error().is_none(), "{:?}", r.first_error());
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let dt = t.elapsed().as_secs_f64();
        b.line(&format!("{} session(s) x {} updates", threads, per), 0.0, dt, &format!("{:.0} stmt/s", (threads * per) as f64 / dt));
    }
    println!("{:<40} {:>9.3} {:>9.3}  {} problem(s)", "total (without the concurrent sessions)", b.total.0, b.total.1, b.problems);
    let problems = b.problems;
    drop(b);
    for s in local {
        rt.block_on(s.shutdown());
    }
    std::process::exit(if problems == 0 { 0 } else { 1 });
}
