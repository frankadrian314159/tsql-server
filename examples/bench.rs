//! A small benchmark of the engine: `cargo run --release --example bench [rows] [--durable]`.
//!
//! It runs a fixed set of workloads (inserts, point lookups, scans, aggregates, joins, sorts, updates, deletes) and
//! prints the time of each, so a change that speeds up or slows down the engine shows up as a number.
use std::time::Instant;
use tsql_server::Server;

type Results = Vec<(String, f64, String)>;

fn run(results: &mut Results, only: &[String], name: &str, sql: &str, c: &mut tsql_server::Connection) {
    if !only.is_empty() && !only.iter().any(|o| name.contains(o.as_str())) {
        return;
    }
    let t = Instant::now();
    let r = c.execute(sql);
    let dt = t.elapsed().as_secs_f64();
    if let Some(e) = r.first_error() {
        panic!("{}: {}", name, e);
    }
    results.push((name.to_string(), dt, String::new()));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rows: usize = args.iter().find_map(|a| a.parse().ok()).unwrap_or(100_000);
    let durable = args.iter().any(|a| a == "--durable");
    let dir = tempfile::tempdir().unwrap();
    let server = if durable { Server::open(dir.path()).unwrap() } else { Server::new() };
    let mut c = server.connect();
    let mut only: Vec<String> = args.iter().filter(|a| !a.starts_with("--") && a.parse::<usize>().is_err()).cloned().collect();
    only.sort();
    let mut results: Results = vec![];
    run(&mut results, &[], "create tables", "CREATE TABLE t (id INT PRIMARY KEY, grp INT, name VARCHAR(40), val FLOAT, note NVARCHAR(60));\nCREATE TABLE d (id INT PRIMARY KEY, label VARCHAR(30));\nCREATE INDEX ix_grp ON t (grp);", &mut c);
    // multi-row inserts in batches of 500
    let t0 = Instant::now();
    let mut i = 0;
    while i < rows {
        let mut sql = String::from("INSERT t VALUES ");
        for k in i..(i + 500).min(rows) {
            if k > i {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, 'name{}', {}.5, N'note {}')", k, (k * 7919) % 1000, k % 977, (k * 13) % 997, k % 31));
        }
        let r = c.execute(&sql);
        if let Some(e) = r.first_error() {
            panic!("insert: {}", e);
        }
        i += 500;
    }
    results.push((format!("insert {} rows (500/stmt)", rows), t0.elapsed().as_secs_f64(), String::new()));
    run(&mut results, &[], "insert dim 1000 rows", &(0..1000).map(|k| format!("INSERT d VALUES ({}, 'label {}');", k, k)).collect::<String>(), &mut c);

    // single-row autocommit inserts
    let t0 = Instant::now();
    let n1 = 5000usize;
    for k in 0..n1 {
        let r = c.execute(&format!("INSERT t VALUES ({}, {}, 'x', 1.0, N'y')", rows + k, k % 1000));
        if let Some(e) = r.first_error() {
            panic!("insert1: {}", e);
        }
    }
    results.push((format!("insert {} single rows (autocommit)", n1), t0.elapsed().as_secs_f64(), String::new()));

    // the fixed cost of a statement
    let t0 = Instant::now();
    for _ in 0..20_000 {
        let r = c.execute("SELECT 1");
        if r.first_error().is_some() {
            panic!("select 1");
        }
    }
    results.push(("20000 x SELECT 1 (statement overhead)".into(), t0.elapsed().as_secs_f64(), String::new()));
    let t0 = Instant::now();
    for _ in 0..20_000 {
        let r = c.execute("SET NOCOUNT ON");
        if r.first_error().is_some() {
            panic!("set");
        }
    }
    results.push(("20000 x SET NOCOUNT ON".into(), t0.elapsed().as_secs_f64(), String::new()));

    // point lookups
    let t0 = Instant::now();
    let n2 = 20_000usize;
    for k in 0..n2 {
        let r = c.execute(&format!("SELECT name, val FROM t WHERE id = {}", (k * 31) % rows));
        if r.first_error().is_some() {
            panic!("lookup");
        }
    }
    results.push((format!("{} point lookups by PK", n2), t0.elapsed().as_secs_f64(), String::new()));

    // the same statement text repeated (parse cache) with a variable
    let t0 = Instant::now();
    c.execute("DECLARE @i INT = 0; DECLARE @s FLOAT = 0; WHILE @i < 20000 BEGIN SELECT @s = @s + val FROM t WHERE id = @i; SET @i = @i + 1; END");
    results.push(("20000 point lookups in a T-SQL loop".into(), t0.elapsed().as_secs_f64(), String::new()));

    // the statement shape real clients send: sp_executesql with parameters (same text every time)
    let t0 = Instant::now();
    for k in 0..20_000usize {
        let r = c.execute(&format!("EXEC sp_executesql N'SELECT name, val FROM t WHERE id = @id', N'@id INT', @id = {}", (k * 31) % rows));
        if let Some(e) = r.first_error() {
            panic!("executesql: {}", e);
        }
    }
    results.push(("20000 sp_executesql point lookups".into(), t0.elapsed().as_secs_f64(), String::new()));
    c.execute("CREATE PROCEDURE getrow @id INT AS SELECT name, val FROM t WHERE id = @id");
    let t0 = Instant::now();
    for k in 0..20_000usize {
        let r = c.execute(&format!("EXEC getrow {}", (k * 31) % rows));
        if let Some(e) = r.first_error() {
            panic!("proc: {}", e);
        }
    }
    results.push(("20000 stored procedure point lookups".into(), t0.elapsed().as_secs_f64(), String::new()));

    run(&mut results, &only, "count(*)", "SELECT COUNT(*) FROM t", &mut c);
    run(&mut results, &only, "full scan with filter", "SELECT COUNT(*) FROM t WHERE val > 500 AND name LIKE 'name1%'", &mut c);
    run(&mut results, &only, "index range (grp = 5)", "SELECT COUNT(*), SUM(val) FROM t WHERE grp = 5", &mut c);
    run(&mut results, &only, "group by + order by", "SELECT grp, COUNT(*) n, AVG(val) a FROM t GROUP BY grp ORDER BY n DESC, grp", &mut c);
    run(&mut results, &only, "group by 1000 keys, 3 aggs", "SELECT name, MIN(val), MAX(val), SUM(id) FROM t GROUP BY name", &mut c);
    run(&mut results, &only, "order by top 100", "SELECT TOP 100 id, name FROM t ORDER BY val DESC, id", &mut c);
    run(&mut results, &only, "order by all rows", "SELECT id, name, val FROM t ORDER BY name, id", &mut c);
    run(&mut results, &only, "distinct", "SELECT DISTINCT name, grp FROM t", &mut c);
    run(&mut results, &only, "join t-d (hash)", "SELECT COUNT(*) FROM t JOIN d ON t.grp = d.id", &mut c);
    run(&mut results, &only, "join + group", "SELECT d.label, COUNT(*) FROM t JOIN d ON t.grp = d.id GROUP BY d.label", &mut c);
    run(&mut results, &only, "self join on pk (index)", "SELECT COUNT(*) FROM t a JOIN t b ON a.id = b.id WHERE a.grp = 3", &mut c);
    run(&mut results, &only, "subquery in select list", "SELECT TOP 2000 id, (SELECT label FROM d WHERE d.id = t.grp) FROM t", &mut c);
    run(&mut results, &only, "exists subquery", "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp AND d.id < 500)", &mut c);
    run(&mut results, &only, "window row_number", "SELECT COUNT(*) FROM (SELECT ROW_NUMBER() OVER (PARTITION BY grp ORDER BY id) rn FROM t) x WHERE rn = 1", &mut c);
    run(&mut results, &only, "union all + distinct", "SELECT COUNT(*) FROM (SELECT name FROM t UNION SELECT name FROM t WHERE id < 1000) x", &mut c);
    run(&mut results, &only, "string functions", "SELECT COUNT(*) FROM t WHERE UPPER(name) = 'NAME5' OR LEN(note) > 6", &mut c);
    run(&mut results, &only, "cast/convert + arithmetic", "SELECT SUM(CAST(id AS BIGINT) * 2 + grp) FROM t", &mut c);

    // updates
    let t0 = Instant::now();
    let n3 = 5000usize;
    for k in 0..n3 {
        let r = c.execute(&format!("UPDATE t SET val = val + 1 WHERE id = {}", (k * 17) % rows));
        if r.first_error().is_some() {
            panic!("update");
        }
    }
    results.push((format!("{} single-row updates by PK", n3), t0.elapsed().as_secs_f64(), String::new()));
    run(&mut results, &only, "update all rows (non-key)", "UPDATE t SET val = val * 2", &mut c);
    run(&mut results, &only, "update all rows (indexed col)", "UPDATE t SET grp = grp + 1", &mut c);
    run(&mut results, &only, "delete a quarter", "DELETE FROM t WHERE id % 4 = 0", &mut c);
    run(&mut results, &only, "explicit transaction, 2000 inserts", &format!("BEGIN TRAN; {} COMMIT", (0..2000).map(|k| format!("INSERT d VALUES ({}, 'z');", 100_000 + k)).collect::<String>()), &mut c);

    // concurrent sessions: the same point lookups, and single-row updates of different rows, from several threads
    for threads in [1usize, 4, 8] {
        let per = 20_000usize;
        let t0 = Instant::now();
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                let mut c = server.connect();
                std::thread::spawn(move || {
                    for _ in 0..per {
                        let r = c.execute("SELECT 1");
                        assert!(r.first_error().is_none());
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let dt = t0.elapsed().as_secs_f64();
        results.push((format!("{} thread(s) x {} SELECT 1 ({:.0} stmt/s)", threads, per, (threads * per) as f64 / dt), dt, String::new()));
    }
    for threads in [1usize, 2, 4, 8] {
        let per = 10_000usize;
        let t0 = Instant::now();
        let hs: Vec<_> = (0..threads)
            .map(|th| {
                let mut c = server.connect();
                std::thread::spawn(move || {
                    for k in 0..per {
                        let r = c.execute(&format!("SELECT name, val FROM t WHERE id = {}", (k * 31 + th * 7) % rows));
                        assert!(r.first_error().is_none());
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let dt = t0.elapsed().as_secs_f64();
        results.push((format!("{} thread(s) x {} lookups ({:.0} stmt/s)", threads, per, (threads * per) as f64 / dt), dt, String::new()));
    }
    for threads in [1usize, 4] {
        let per = 2_000usize;
        let t0 = Instant::now();
        let hs: Vec<_> = (0..threads)
            .map(|th| {
                let mut c = server.connect();
                std::thread::spawn(move || {
                    for k in 0..per {
                        let r = c.execute(&format!("UPDATE t SET val = val + 1 WHERE id = {}", (k * 4 + th) % 20000 + 1));
                        assert!(r.first_error().is_none(), "{:?}", r.first_error());
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let dt = t0.elapsed().as_secs_f64();
        results.push((format!("{} thread(s) x {} updates ({:.0} stmt/s)", threads, per, (threads * per) as f64 / dt), dt, String::new()));
    }

    println!("{:<44} {:>10}", format!("workload ({} rows, {})", rows, if durable { "durable" } else { "in memory" }), "seconds");
    let total: f64 = results.iter().map(|r| r.1).sum();
    for (n, t, _) in &results {
        println!("{:<44} {:>10.3}", n, t);
    }
    println!("{:<44} {:>10.3}", "total", total);
    tsql_server::prof::report();
}

// The memory allocator of Windows is slow for the many small allocations of the engine; `--features fast-alloc` links mimalloc
// instead (about a third less time on the benchmark; see docs/performance.md). It is C code, so it is not on by default.
#[cfg(feature = "fast-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
