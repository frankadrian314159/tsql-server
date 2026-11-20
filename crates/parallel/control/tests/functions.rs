//! Stored functions in a cluster: one that reads no table is on the control node and on every shard (a query that calls it still runs on the
//! shards); one that reads tables is on the control node only, and a query that calls it (directly, through a view, or through another
//! function) is gathered there and is exact.
use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{BatchResult, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

struct Harness {
    _rt: tokio::runtime::Runtime,
    shards: Vec<RunningShard>,
    cfg: ClusterConfig,
    cluster: Arc<Cluster>,
}

fn start(n: usize, state: Option<&std::path::Path>) -> Harness {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut shards = vec![];
    for i in 0..n {
        shards.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
    }
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = match state {
        Some(p) => Cluster::open(p, &cfg).unwrap(),
        None => Cluster::connect(&cfg).unwrap(),
    };
    Harness { _rt: rt, shards, cfg, cluster }
}

fn ok(r: &BatchResult, what: &str) {
    assert!(r.first_error().is_none(), "{}: {:?}", what, r.first_error());
}

fn run(s: &mut ClusterSession, sql: &str) {
    let r = s.execute(sql);
    ok(&r, sql);
}

fn rows(s: &mut ClusterSession, sql: &str) -> Vec<Vec<String>> {
    let r = s.execute(sql);
    ok(&r, sql);
    r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(display).collect()).collect()).unwrap_or_default()
}

fn one(s: &mut ClusterSession, sql: &str) -> String {
    rows(s, sql).first().and_then(|r| r.first()).cloned().unwrap_or_default()
}

fn error(s: &mut ClusterSession, sql: &str) -> i32 {
    s.execute(sql).first_error().map(|e| e.number).unwrap_or(0)
}

fn plan(s: &ClusterSession) -> String {
    s.last_plan().join(" | ")
}

fn fill(s: &mut ClusterSession, table: &str) {
    run(s, &format!("CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(20), qty INT)", table));
    let values: Vec<String> = (1..=300).map(|i| format!("({}, 'n{}', {})", i, i, i % 7)).collect();
    run(s, &format!("INSERT INTO {} VALUES {}", table, values.join(", ")));
}

/// Whether a shard has the function (asked of the shard's own engine, in `db`).
fn shard_has(sh: &RunningShard, db: &str, func: &str) -> bool {
    let mut c = sh.server.connect();
    let _ = c.execute(&format!("USE {}", db));
    c.execute(&format!("SELECT {}(1)", func)).first_error().is_none()
}

const TOTAL: &str = "903"; // the sum of i % 7 over 1..=300 (43 cycles of 21)

#[test]
fn a_function_that_reads_no_table_is_on_the_shards_and_one_that_does_is_not() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.dbl(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END");
    run(&mut s, "CREATE FUNCTION dbo.total_qty(@unused INT) RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    for sh in &h.shards {
        assert!(shard_has(sh, "master", "dbo.dbl"), "the shards have the function that reads no table");
        assert!(!shard_has(sh, "master", "dbo.total_qty"), "and not the one that reads one");
    }
    // the pure one runs on the shards, in a scan that is scattered
    assert_eq!(one(&mut s, "SELECT SUM(dbo.dbl(qty)) FROM items"), "1806");
    assert!(plan(&s).contains("scatter") || plan(&s).contains("route"), "a query that calls only pure functions runs on the shards: {}", plan(&s));
    assert_eq!(one(&mut s, "SELECT dbo.dbl(qty) FROM items WHERE id = 3"), "6");
    // the one that reads tables is exact (over every shard's rows) and runs on the control node
    assert_eq!(one(&mut s, "SELECT dbo.total_qty(0)"), TOTAL);
    assert!(plan(&s).contains("calls a function that reads tables"), "{}", plan(&s));
    assert_eq!(rows(&mut s, "SELECT id, dbo.total_qty(0) FROM items WHERE id <= 2 ORDER BY id"), vec![vec!["1".to_string(), TOTAL.into()], vec!["2".to_string(), TOTAL.into()]]);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items WHERE qty * 200 > dbo.total_qty(0)"), "86");
    // in a variable
    run(&mut s, "DECLARE @x INT = dbo.total_qty(0)");
    assert_eq!(one(&mut s, "DECLARE @y INT; SET @y = dbo.total_qty(0); SELECT @y"), TOTAL);
}

#[test]
fn table_valued_functions_and_functions_that_call_functions() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.pair(@x INT) RETURNS TABLE AS RETURN (SELECT @x AS a, @x * 2 AS b)");
    run(&mut s, "CREATE FUNCTION dbo.big(@min INT) RETURNS TABLE AS RETURN (SELECT id, qty FROM dbo.items WHERE qty >= @min)");
    run(&mut s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    run(&mut s, "CREATE FUNCTION dbo.twice_total() RETURNS INT AS BEGIN RETURN dbo.total_qty() * 2 END");
    run(&mut s, "CREATE FUNCTION dbo.add_one(@x INT) RETURNS INT AS BEGIN RETURN @x + 1 END");
    run(&mut s, "CREATE FUNCTION dbo.add_two(@x INT) RETURNS INT AS BEGIN RETURN dbo.add_one(dbo.add_one(@x)) END");
    // a table-valued function that reads nothing is on the shards; one that reads a table is not
    for sh in &h.shards {
        assert!(shard_has(sh, "master", "dbo.add_two"), "a function that calls only pure functions is pure");
        assert!(!shard_has(sh, "master", "dbo.twice_total"), "one that calls a function that reads tables reads them");
    }
    assert_eq!(rows(&mut s, "SELECT a, b FROM dbo.pair(4)"), vec![vec!["4".to_string(), "8".into()]]);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM dbo.big(6)"), "43");
    assert_eq!(one(&mut s, "SELECT SUM(b.qty) FROM items i JOIN dbo.big(6) b ON b.id = i.id"), "258");
    assert!(plan(&s).contains("calls a function that reads tables"), "{}", plan(&s));
    assert_eq!(one(&mut s, "SELECT dbo.twice_total()"), "1806");
    assert_eq!(one(&mut s, "SELECT SUM(dbo.add_two(qty)) FROM items"), "1503");
    assert!(plan(&s).contains("scatter") || plan(&s).contains("route"), "a query that calls only pure functions runs on the shards: {}", plan(&s));
}

#[test]
fn a_view_that_calls_a_function_that_reads_tables_is_gathered_too() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    run(&mut s, "CREATE VIEW dbo.v AS SELECT id, qty, dbo.total_qty() AS t FROM dbo.items");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM dbo.v WHERE t = 903"), "300");
    assert!(plan(&s).contains("calls a function that reads tables"), "{}", plan(&s));
    assert_eq!(one(&mut s, "SELECT MAX(t) FROM dbo.v"), TOTAL);
}

#[test]
fn what_would_run_on_a_shard_cannot_call_a_function_that_reads_tables() {
    let h = start(2, None);
    let mut s = h.cluster.session();
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    // (a call in INSERT, UPDATE and DELETE is evaluated by the control node: see funcdml.rs)
    // (a procedure runs on the control node, which has the function: see procexec.rs)
    run(&mut s, "CREATE PROCEDURE dbo.p AS SELECT dbo.total_qty()");
    assert_eq!(error(&mut s, "CREATE TABLE dbo.c (a INT PRIMARY KEY, b AS dbo.total_qty())"), 50100);
    // nothing was changed by the refused statements
    assert_eq!(one(&mut s, "SELECT SUM(qty) FROM items"), TOTAL);
    // the pure ones are fine in all of them
    run(&mut s, "CREATE FUNCTION dbo.dbl(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END");
    run(&mut s, "UPDATE items SET qty = dbo.dbl(qty) WHERE id <= 10");
    run(&mut s, "CREATE PROCEDURE dbo.p2 AS SELECT SUM(dbo.dbl(qty)) FROM dbo.items");
}

#[test]
fn a_function_changed_or_dropped_moves_between_the_shards_and_the_control_node() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.f(@x INT) RETURNS INT AS BEGIN RETURN @x + 1 END");
    assert!(h.shards.iter().all(|sh| shard_has(sh, "master", "dbo.f")));
    assert_eq!(one(&mut s, "SELECT dbo.f(1)"), "2");
    // changed to one that reads a table: it leaves the shards, and the answer is still exact
    run(&mut s, "CREATE OR ALTER FUNCTION dbo.f(@x INT) RETURNS INT AS BEGIN RETURN @x + (SELECT COUNT(*) FROM dbo.items) END");
    assert!(h.shards.iter().all(|sh| !shard_has(sh, "master", "dbo.f")));
    assert_eq!(one(&mut s, "SELECT dbo.f(1)"), "301");
    // and back
    run(&mut s, "CREATE OR ALTER FUNCTION dbo.f(@x INT) RETURNS INT AS BEGIN RETURN @x + 10 END");
    assert!(h.shards.iter().all(|sh| shard_has(sh, "master", "dbo.f")));
    assert_eq!(one(&mut s, "SELECT SUM(dbo.f(qty)) FROM items"), "3903");
    run(&mut s, "DROP FUNCTION dbo.f");
    assert!(h.shards.iter().all(|sh| !shard_has(sh, "master", "dbo.f")));
    assert_eq!(error(&mut s, "SELECT dbo.f(1)"), 4121);
    assert_eq!(error(&mut s, "DROP FUNCTION dbo.f"), 3701);
    run(&mut s, "DROP FUNCTION IF EXISTS dbo.f");
}

#[test]
fn functions_of_a_database_of_the_cluster() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE DATABASE shop");
    run(&mut s, "USE shop");
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.dbl(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END");
    run(&mut s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    assert_eq!(one(&mut s, "SELECT SUM(dbo.dbl(qty)) FROM items"), "1806");
    assert_eq!(one(&mut s, "SELECT dbo.total_qty()"), TOTAL);
    for sh in &h.shards {
        assert!(shard_has(sh, "shop", "dbo.dbl"));
        assert!(!shard_has(sh, "shop", "dbo.total_qty"));
        assert!(!shard_has(sh, "master", "dbo.dbl"), "master did not get it");
    }
    // from master, the function is named with its database
    run(&mut s, "USE master");
    assert_eq!(one(&mut s, "SELECT shop.dbo.dbl(21)"), "42");
    assert_eq!(one(&mut s, "SELECT shop.dbo.total_qty()"), TOTAL);
    // the database dropped takes them with it
    run(&mut s, "DROP DATABASE shop");
    assert_eq!(error(&mut s, "SELECT shop.dbo.dbl(21)"), 4121);
}

#[test]
fn a_control_node_that_starts_again_and_a_shard_that_joins_have_the_functions() {
    let dir = std::env::temp_dir().join(format!("tsql-functions-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("cluster.state");
    let mut h = start(2, Some(&state));
    {
        let mut s = h.cluster.session();
        fill(&mut s, "items");
        run(&mut s, "CREATE FUNCTION dbo.dbl(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END");
        run(&mut s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    }
    // a second control node over the same shards, from the state file
    let again = Cluster::open(&state, &h.cfg).unwrap();
    {
        let mut s = again.session();
        assert_eq!(one(&mut s, "SELECT SUM(dbo.dbl(qty)) FROM items"), "1806");
        assert_eq!(one(&mut s, "SELECT dbo.total_qty()"), TOTAL);
        assert!(!plan(&s).is_empty());
    }
    // a shard joins: it is given the function that reads no table, not the other
    let s3 = h._rt.block_on(spawn(ShardNode::new(2, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap();
    let addr = s3.addr.to_string();
    h.shards.push(s3);
    assert_eq!(again.add_shard(&addr).unwrap(), 2);
    assert!(shard_has(&h.shards[2], "master", "dbo.dbl"));
    assert!(!shard_has(&h.shards[2], "master", "dbo.total_qty"));
    let mut s = again.session();
    assert_eq!(one(&mut s, "SELECT SUM(dbo.dbl(qty)) FROM items"), "1806");
    assert_eq!(one(&mut s, "SELECT dbo.total_qty()"), TOTAL);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_table_with_an_identity_column_is_gathered_into_the_control_node() {
    // (a query the shards cannot split, STDEV here, is run on the control node over a copy of the table, which carries the numbers its
    // rows were given; the same goes for a function that reads such a table)
    let h = start(2, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE t (id INT IDENTITY(1,1) PRIMARY KEY, v INT)");
    run(&mut s, "INSERT t (v) VALUES (1), (2), (3), (4)");
    assert_eq!(one(&mut s, "SELECT CAST(STDEV(v) * 1000 AS INT) FROM t"), "1290");
    assert!(plan(&s).contains("gather"), "{}", plan(&s));
    run(&mut s, "CREATE FUNCTION dbo.sum_v() RETURNS INT AS BEGIN RETURN (SELECT SUM(v) FROM dbo.t) END");
    assert_eq!(one(&mut s, "SELECT dbo.sum_v()"), "10");
    assert_eq!(rows(&mut s, "SELECT MAX(id) FROM t"), vec![vec!["4".to_string()]]);
}

#[test]
fn the_front_end_hands_the_cluster_a_batch_that_calls_a_function_that_reads_tables() {
    // (the control node's own engine has none of the rows: it must not answer such a batch itself, even when no table is named)
    let h = start(2, None);
    let mut s = h.cluster.session();
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    run(&mut s, "CREATE FUNCTION dbo.dbl(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END");
    assert!(s.wants("SELECT dbo.total_qty()"));
    assert!(s.wants("DECLARE @t INT = dbo.total_qty(); SELECT @t"));
    assert!(s.wants("CREATE FUNCTION dbo.g() RETURNS INT AS BEGIN RETURN 1 END"));
    assert!(s.wants("DROP FUNCTION dbo.dbl"));
    // one that reads no table, with no table named, is the engine's own to answer
    assert!(!s.wants("SELECT dbo.dbl(4)"));
}

#[test]
fn a_function_that_reads_only_replicated_tables_is_on_the_shards() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE regions (code CHAR(2) PRIMARY KEY, name VARCHAR(20))");
    run(&mut s, "DISTRIBUTE TABLE regions REPLICATED");
    run(&mut s, "INSERT regions VALUES ('NA', 'North America'), ('EU', 'Europe')");
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.region_name(@c CHAR(2)) RETURNS VARCHAR(20) AS BEGIN RETURN (SELECT name FROM dbo.regions WHERE code = @c) END");
    run(&mut s, "CREATE FUNCTION dbo.item_count() RETURNS INT AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.items) END");
    run(&mut s, "CREATE FUNCTION dbo.region_or_count(@c CHAR(2)) RETURNS VARCHAR(30) AS BEGIN RETURN ISNULL(dbo.region_name(@c), CAST(dbo.item_count() AS VARCHAR(30))) END");
    for sh in &h.shards {
        assert!(shard_has_text(sh, "master", "SELECT dbo.region_name('EU')"), "a function that reads a replicated table is on the shards");
        assert!(!shard_has_text(sh, "master", "SELECT dbo.item_count()"), "one that reads a sharded table is not");
        assert!(!shard_has_text(sh, "master", "SELECT dbo.region_or_count('EU')"), "nor one that calls it");
    }
    // the replicated one runs in the shards' scan: the answer is exact and the plan is a scatter, not a gather
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items WHERE dbo.region_name(CASE WHEN id % 2 = 0 THEN 'EU' ELSE 'NA' END) = 'Europe'"), "150");
    assert!(plan(&s).contains("scatter") || plan(&s).contains("route"), "{}", plan(&s));
    assert_eq!(one(&mut s, "SELECT dbo.region_name('NA')"), "North America");
    assert!(s.wants("SELECT dbo.region_name('NA')"), "the control node's own copy of the table has no rows: the cluster answers");
    assert_eq!(one(&mut s, "DECLARE @n VARCHAR(20) = dbo.region_name('EU'); SELECT @n"), "Europe");
    // the one that reads a sharded table is gathered, and the one that calls it too
    assert_eq!(one(&mut s, "SELECT dbo.region_or_count('XX')"), "300");
    assert!(plan(&s).contains("calls a function that reads tables"), "{}", plan(&s));
}

/// Whether a shard's own engine can run a statement.
fn shard_has_text(sh: &RunningShard, db: &str, sql: &str) -> bool {
    let mut c = sh.server.connect();
    let _ = c.execute(&format!("USE {}", db));
    c.execute(sql).first_error().is_none()
}

#[test]
fn a_change_that_would_leave_a_procedure_calling_a_function_over_slices_is_refused() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE regions (code CHAR(2) PRIMARY KEY, name VARCHAR(20))");
    run(&mut s, "DISTRIBUTE TABLE regions REPLICATED");
    fill(&mut s, "items");
    run(&mut s, "CREATE FUNCTION dbo.region_name(@c CHAR(2)) RETURNS VARCHAR(20) AS BEGIN RETURN (SELECT name FROM dbo.regions WHERE code = @c) END");
    run(&mut s, "CREATE PROCEDURE dbo.p (@id INT) AS SELECT id, dbo.region_name('EU') FROM dbo.items WHERE id = @id");
    run(&mut s, "DISTRIBUTE PROCEDURE dbo.p BY @id");
    run(&mut s, "INSERT regions VALUES ('EU', 'Europe')");
    assert_eq!(rows(&mut s, "EXEC dbo.p 5"), vec![vec!["5".to_string(), "Europe".into()]]);
    // (the table has rows now, so empty it: only an empty table can be re-spread)
    run(&mut s, "DELETE FROM regions");
    let r = s.execute("DISTRIBUTE TABLE regions BY code");
    let e = r.first_error().expect("refused");
    assert_eq!(e.number, 50105);
    assert!(e.message.contains("dbo.p") && e.message.contains("dbo.region_name"), "{}", e.message);
    // nothing changed: the table is replicated still, and the procedure works
    run(&mut s, "INSERT regions VALUES ('EU', 'Europe')");
    assert_eq!(rows(&mut s, "EXEC dbo.p 7"), vec![vec!["7".to_string(), "Europe".into()]]);
    // with the procedure gone the table may be re-spread, and the function leaves the shards
    run(&mut s, "DROP PROCEDURE dbo.p");
    run(&mut s, "DELETE FROM regions");
    run(&mut s, "DISTRIBUTE TABLE regions BY code");
    assert!(h.shards.iter().all(|sh| !shard_has_text(sh, "master", "SELECT dbo.region_name('EU')")));
    run(&mut s, "INSERT regions VALUES ('EU', 'Europe')");
    assert_eq!(one(&mut s, "SELECT dbo.region_name('EU')"), "Europe");
    assert!(plan(&s).contains("calls a function that reads tables"), "{}", plan(&s));
}

#[test]
fn a_table_made_or_a_function_changed_under_a_procedure_that_runs_on_the_shards_is_refused() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    fill(&mut s, "items");
    // the function names a table that is not there yet: it reads none, as far as the cluster knows, so it is on the shards
    run(&mut s, "CREATE FUNCTION dbo.later_count() RETURNS INT AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.later) END");
    assert!(h.shards.iter().all(|sh| shard_has_text(sh, "master", "SELECT 1 WHERE OBJECT_ID('dbo.later_count') IS NOT NULL")));
    run(&mut s, "CREATE PROCEDURE dbo.q (@id INT) AS SELECT id, dbo.later_count() FROM dbo.items WHERE id = @id");
    run(&mut s, "DISTRIBUTE PROCEDURE dbo.q BY @id");
    // making the table (sharded) would leave the procedure calling the function over slices: refused, and the table is not made
    assert_eq!(error(&mut s, "CREATE TABLE later (id INT PRIMARY KEY, v INT)"), 50105);
    assert_eq!(error(&mut s, "INSERT later VALUES (1, 1)"), 208);
    // a changed function is refused the same way, and stays as it was
    run(&mut s, "CREATE FUNCTION dbo.f(@x INT) RETURNS INT AS BEGIN RETURN @x + 1 END");
    run(&mut s, "CREATE PROCEDURE dbo.r (@id INT) AS SELECT id, dbo.f(qty) FROM dbo.items WHERE id = @id");
    run(&mut s, "DISTRIBUTE PROCEDURE dbo.r BY @id");
    assert_eq!(error(&mut s, "CREATE OR ALTER FUNCTION dbo.f(@x INT) RETURNS INT AS BEGIN RETURN @x + (SELECT COUNT(*) FROM dbo.items) END"), 50105);
    assert_eq!(one(&mut s, "SELECT dbo.f(1)"), "2");
    assert!(h.shards.iter().all(|sh| shard_has_text(sh, "master", "SELECT dbo.f(1)")));
    assert_eq!(rows(&mut s, "EXEC dbo.r 5"), vec![vec!["5".to_string(), "6".into()]]);
    // with the procedures dropped, both go through and the functions that read tables are on the control node
    run(&mut s, "DROP PROCEDURE dbo.q");
    run(&mut s, "DROP PROCEDURE dbo.r");
    run(&mut s, "CREATE TABLE later (id INT PRIMARY KEY, v INT)");
    run(&mut s, "INSERT later VALUES (1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)");
    run(&mut s, "CREATE OR ALTER FUNCTION dbo.f(@x INT) RETURNS INT AS BEGIN RETURN @x + (SELECT COUNT(*) FROM dbo.items) END");
    assert!(h.shards.iter().all(|sh| !shard_has_text(sh, "master", "SELECT dbo.f(1)")));
    assert_eq!(one(&mut s, "SELECT dbo.later_count()"), "6");
    assert_eq!(one(&mut s, "SELECT dbo.f(1)"), "301");
}
