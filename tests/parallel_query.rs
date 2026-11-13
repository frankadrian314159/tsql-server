//! Intra-query parallelism: a query that is shared out to worker threads (see `src/engine/par.rs`) returns what it returns
//! when it runs on one thread (`OPTION (MAXDOP 1)`): the same rows, in the same order, the same errors.
mod common;
use common::*;
use std::sync::Once;
use tsql_server::engine::par::parallel_batches;

/// Four workers and a small minimum size, whatever the machine is (this test program is its own process).
fn force() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("TSQL_PARALLEL_FORCE_DOP", "4");
        std::env::set_var("TSQL_PARALLEL_MIN_SLOTS", "1000");
    });
}

/// The tests count batches with a counter that every test moves, so they run one at a time.
fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn big() -> tsql_server::Connection {
    force();
    let mut c = conn();
    exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, grp INT, name VARCHAR(20), amount DECIMAL(12,2), f FLOAT, d DATE)");
    exec(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 30000) INSERT big SELECT i, i % 97, 'row' + CAST(i AS VARCHAR(10)), i * 1.25, i / 7.0, DATEADD(DAY, i % 365, '2020-01-01') FROM n OPTION (MAXRECURSION 0)");
    c
}

/// The rows of a query, as text, in the order they came.
fn run(c: &mut tsql_server::Connection, sql: &str) -> Vec<String> {
    rows(c, sql).into_iter().map(|r| r.join("|")).collect()
}

/// `sql` gives the same rows (in the same order) in parallel and with MAXDOP 1; `expect_parallel` says whether it was shared out.
fn same(c: &mut tsql_server::Connection, sql: &str, expect_parallel: bool) {
    let before = parallel_batches();
    let par = run(c, sql);
    let used = parallel_batches() > before;
    let serial = run(c, &format!("{} OPTION (MAXDOP 1)", sql));
    assert_eq!(par, serial, "{}", sql);
    assert_eq!(used, expect_parallel, "{}: ran in parallel: {}", sql, used);
}

#[test]
fn scans_with_conditions_are_shared_out() {
    let _one = one_at_a_time();
    let mut c = big();
    same(&mut c, "SELECT id, name FROM big WHERE grp = 5 AND amount > 1000", true);
    same(&mut c, "SELECT id FROM big WHERE name LIKE 'row1%' AND id % 3 = 0", true);
    same(&mut c, "SELECT id, d FROM big WHERE d >= '2020-06-01' AND d < '2020-07-01' AND UPPER(name) LIKE 'ROW2%'", true);
    same(&mut c, "SELECT id FROM big WHERE CASE WHEN grp > 50 THEN amount ELSE -amount END > 20000", true);
    same(&mut c, "SELECT COUNT(*) FROM big WHERE grp IN (1, 2, 3) OR f BETWEEN 100 AND 200", true);
}

#[test]
fn grouped_aggregates_and_sorted_queries_are_shared_out() {
    let _one = one_at_a_time();
    let mut c = big();
    same(&mut c, "SELECT grp, COUNT(*), SUM(amount), MIN(name), MAX(id), AVG(amount) FROM big WHERE id > 100 GROUP BY grp ORDER BY grp", true);
    // a float sum is added in the order of the rows, so it is the same as the serial one to the last digit
    same(&mut c, "SELECT grp, SUM(f), AVG(f) FROM big GROUP BY grp ORDER BY grp", true);
    same(&mut c, "SELECT grp % 5 AS k, SUM(amount * 2 + 1) FROM big WHERE name <> 'x' GROUP BY grp % 5 ORDER BY k", true);
    same(&mut c, "SELECT TOP 10 id, amount FROM big WHERE grp = 7 ORDER BY amount DESC, id", true);
    same(&mut c, "SELECT id, name, amount * 2 FROM big WHERE grp < 3 ORDER BY name, id", true);
    same(&mut c, "SELECT DISTINCT grp FROM big WHERE id % 2 = 0 ORDER BY grp", true);
}

#[test]
fn what_cannot_be_shared_out_runs_on_one_thread() {
    let _one = one_at_a_time();
    let mut c = big();
    // a subquery, a function of the user's, MAXDOP 1, and a small table
    exec(&mut c, "CREATE FUNCTION dbo.f_twice(@n INT) RETURNS INT AS BEGIN RETURN @n * 2 END");
    exec(&mut c, "CREATE TABLE small (id INT, g INT)");
    exec(&mut c, "INSERT small VALUES (1, 1), (2, 2)");
    // (an uncorrelated subquery is worked out first and becomes a constant, so it is not what stops a query being shared out)
    same(&mut c, "SELECT id FROM big WHERE EXISTS (SELECT 1 FROM small WHERE small.g = big.grp)", false);
    same(&mut c, "SELECT id FROM big WHERE dbo.f_twice(grp) = 10", false);
    // (a limit does not stop the scan from reading a batch ahead, but the rows are the same)
    same(&mut c, "SELECT TOP 5 id FROM big WHERE grp = 4", true);
    same(&mut c, "SELECT id FROM small WHERE g = 2", false);
    let before = parallel_batches();
    run(&mut c, "SELECT id FROM big WHERE grp = 5 OPTION (MAXDOP 1)");
    assert_eq!(parallel_batches(), before);
}

#[test]
fn an_error_in_a_row_is_raised_as_it_is_in_a_serial_run() {
    let _one = one_at_a_time();
    let mut c = big();
    // the first row that divides by zero is the same one, whichever thread reaches it
    let e = err(&mut c, "SELECT id FROM big WHERE 100 / (id - 20000) > 0");
    let e1 = err(&mut c, "SELECT id FROM big WHERE 100 / (id - 20000) > 0 OPTION (MAXDOP 1)");
    assert_eq!((e.number, e.message.clone()), (e1.number, e1.message.clone()), "{} / {}", e, e1);
    // rows after the ones that a TOP needs are not looked at
    let ok = run(&mut c, "SELECT TOP 3 id FROM big WHERE id < 10 ORDER BY id");
    assert_eq!(ok, vec!["1", "2", "3"]);
}

#[test]
fn readers_in_parallel_do_not_block_writers_or_ddl() {
    let _one = one_at_a_time();
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    force();
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, grp INT, amount DECIMAL(12,2))");
    exec(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 20000) INSERT big SELECT i, i % 50, i FROM n OPTION (MAXRECURSION 0)");
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = vec![];
    for _ in 0..3 {
        let mut r = s.connect();
        let stop = stop.clone();
        readers.push(std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                let res = r.execute("SELECT grp, SUM(amount) FROM big WHERE id > 10 GROUP BY grp");
                assert!(res.first_error().is_none(), "{:?}", res.first_error());
                n += 1;
            }
            n
        }));
    }
    // writers and DDL while the readers run (a deadlock between a waiting DDL statement and a worker would hang here)
    for i in 0..30 {
        exec(&mut c, &format!("UPDATE big SET amount = amount + 1 WHERE id = {}", i + 1));
        exec(&mut c, &format!("CREATE TABLE side{} (a INT)", i));
    }
    stop.store(true, Ordering::Relaxed);
    let total: i32 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0);
}

#[test]
#[ignore = "a measurement: cargo test --release --test parallel_query -- --ignored --nocapture"]
fn how_much_faster_is_a_parallel_scan() {
    let _one = one_at_a_time();
    std::env::remove_var("TSQL_PARALLEL_FORCE_DOP");
    let mut c = conn();
    exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, grp INT, name VARCHAR(20), amount DECIMAL(12,2), f FLOAT, d DATE)");
    exec(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 400000) INSERT big SELECT i, i % 97, 'row' + CAST(i AS VARCHAR(10)), i * 1.25, i / 7.0, DATEADD(DAY, i % 365, '2020-01-01') FROM n OPTION (MAXRECURSION 0)");
    eprintln!("processors: {}", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    for (what, sql) in [
        ("filter, 4 conditions", "SELECT COUNT(*) FROM big WHERE name LIKE 'row1%' AND UPPER(name) <> 'X' AND d > '2020-03-01' AND amount * 2 > 1000"),
        ("group by, expressions", "SELECT grp, SUM(amount * (1 + f)), COUNT(*), MAX(UPPER(name)) FROM big WHERE d > '2020-03-01' GROUP BY grp"),
        ("sorted, expressions", "SELECT TOP 20 id, amount * 2 + f FROM big WHERE name LIKE 'row%' ORDER BY amount * 2 + f DESC"),
    ] {
        let mut t = [0f64; 2];
        for (i, hint) in ["OPTION (MAXDOP 1)", ""].iter().enumerate() {
            let q = format!("{} {}", sql, hint);
            run(&mut c, &q); // warm up
            let start = std::time::Instant::now();
            for _ in 0..3 {
                run(&mut c, &q);
            }
            t[i] = start.elapsed().as_secs_f64() / 3.0;
        }
        eprintln!("{:<24} one thread {:>7.1} ms   parallel {:>7.1} ms   {:.2}x", what, t[0] * 1000.0, t[1] * 1000.0, t[0] / t[1]);
    }
}
