//! Functions that read sharded tables, called in INSERT, UPDATE and DELETE: the control node evaluates the calls and the shards get their values.
use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{BatchResult, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

struct Harness {
    _rt: tokio::runtime::Runtime,
    _shards: Vec<RunningShard>,
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
    Harness { _rt: rt, _shards: shards, cfg, cluster }
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

/// items(id, name, qty, rnk): ids 1..=12 over the shards, quantities with some ties.
fn setup(s: &mut ClusterSession) {
    run(s, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(10), qty INT, rnk INT)");
    run(s, "INSERT items (id, name, qty) VALUES (1, 'a', 10), (2, 'A', 20), (3, 'b', 20), (4, 'c', 30), (5, 'd', 30), (6, 'e', 40), (7, 'f', 50), (8, 'g', 50), (9, 'h', 60), (10, 'i', 70), (11, 'j', 80), (12, 'k', 90)");
    run(s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    run(s, "CREATE FUNCTION dbo.rank_of(@q INT) RETURNS INT AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.items WHERE qty > @q) END");
}

#[test]
fn a_call_with_constant_arguments_is_evaluated_first_in_insert_update_and_delete() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    assert_eq!(one(&mut s, "SELECT dbo.total_qty()"), "550");
    run(&mut s, "INSERT items (id, name, qty) VALUES (100, 'x', dbo.total_qty()), (101, 'y', dbo.rank_of(50) + 1000)");
    assert_eq!(rows(&mut s, "SELECT id, qty FROM items WHERE id >= 100 ORDER BY id"), vec![vec!["100".to_string(), "550".to_string()], vec!["101".to_string(), "1004".to_string()]]);
    // (the table as it is before the statement: 550 + 550 + 1004)
    run(&mut s, "UPDATE items SET qty = dbo.total_qty() WHERE id = 1");
    assert_eq!(one(&mut s, "SELECT qty FROM items WHERE id = 1"), "2104");
    run(&mut s, "DECLARE @v INT = 9; UPDATE items SET rnk = dbo.rank_of(@v) WHERE id = 2");
    assert_eq!(one(&mut s, "SELECT rnk FROM items WHERE id = 2"), "14");
    run(&mut s, "DELETE FROM items WHERE qty > dbo.total_qty() / 2");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "13");
    // a TVF in a subquery was already evaluated first
    run(&mut s, "CREATE FUNCTION dbo.big(@min INT) RETURNS TABLE AS RETURN (SELECT id, qty FROM dbo.items WHERE qty >= @min)");
    run(&mut s, "UPDATE items SET rnk = (SELECT COUNT(*) FROM dbo.big(50)) WHERE id = 3");
    assert_eq!(one(&mut s, "SELECT rnk FROM items WHERE id = 3"), "8");
}

#[test]
fn a_call_over_the_columns_of_the_table_is_looked_up_for_each_distinct_value() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    // the value for each row is what the function gives for that row's quantity, over the table as it was before the statement
    let expect = rows(&mut s, "SELECT id, dbo.rank_of(qty) FROM items ORDER BY id");
    run(&mut s, "UPDATE items SET rnk = dbo.rank_of(qty)");
    assert_eq!(rows(&mut s, "SELECT id, rnk FROM items ORDER BY id"), expect);
    // in a condition
    run(&mut s, "DELETE FROM items WHERE dbo.rank_of(qty) >= 9");
    assert_eq!(rows(&mut s, "SELECT id FROM items ORDER BY id").len(), 9);
    assert_eq!(one(&mut s, "SELECT MIN(rnk) FROM items"), "0");
    // both, with the condition limiting the rows the values are looked up for
    run(&mut s, "UPDATE items SET rnk = dbo.rank_of(qty) + 100 WHERE dbo.rank_of(qty) < 3");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items WHERE rnk >= 100"), "3");
    // NULL arguments are a value of their own
    run(&mut s, "UPDATE items SET qty = NULL WHERE id = 4");
    run(&mut s, "UPDATE items SET rnk = dbo.rank_of(qty) WHERE id IN (4, 5)");
    assert_eq!(rows(&mut s, "SELECT id, rnk FROM items WHERE id IN (4, 5) ORDER BY id"), vec![vec!["4".to_string(), "0".to_string()], vec!["5".to_string(), "7".to_string()]]);
}

#[test]
fn values_that_differ_only_in_case_get_their_own_call() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE TABLE tags (id INT PRIMARY KEY, name VARCHAR(10), tag VARCHAR(30))");
    run(&mut s, "INSERT tags (id, name) VALUES (1, 'a'), (2, 'A'), (3, 'a')");
    run(&mut s, "CREATE FUNCTION dbo.tagged(@s VARCHAR(10)) RETURNS VARCHAR(30) AS BEGIN RETURN @s + '-' + CAST((SELECT COUNT(*) FROM dbo.items) AS VARCHAR(5)) END");
    run(&mut s, "UPDATE tags SET tag = dbo.tagged(name)");
    assert_eq!(rows(&mut s, "SELECT id, tag FROM tags ORDER BY id"), vec![vec!["1".to_string(), "a-12".to_string()], vec!["2".to_string(), "A-12".to_string()], vec!["3".to_string(), "a-12".to_string()]]);
}

#[test]
fn insert_select_a_call_in_a_transaction_and_what_stays_refused() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE TABLE copy (id INT PRIMARY KEY, r INT)");
    run(&mut s, "BEGIN TRANSACTION");
    run(&mut s, "INSERT copy SELECT id, dbo.rank_of(qty) FROM items");
    run(&mut s, "UPDATE items SET rnk = dbo.rank_of(qty) WHERE id <= 3");
    run(&mut s, "ROLLBACK");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM copy"), "0");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items WHERE rnk IS NOT NULL"), "0");
    run(&mut s, "INSERT copy SELECT id, dbo.rank_of(qty) FROM items");
    assert_eq!(one(&mut s, "SELECT r FROM copy WHERE id = 12"), "0");
    // a computed column cannot be filled by the control node: it is evaluated when a row is read
    assert_eq!(error(&mut s, "CREATE TABLE dbo.c (a INT PRIMARY KEY, b AS dbo.total_qty())"), 50100);
}

fn limits(s: &mut ClusterSession) {
    run(s, "CREATE TABLE limits (id INT PRIMARY KEY, cap INT)");
    run(s, "INSERT limits VALUES (1, 100), (2, 40), (3, 70)");
    run(s, "CREATE FUNCTION dbo.cap() RETURNS INT AS BEGIN RETURN (SELECT MAX(cap) FROM dbo.limits) END");
}

#[test]
fn a_check_that_reads_sharded_tables_is_verified_by_the_control_node() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    limits(&mut s);
    run(&mut s, "CREATE TABLE stock (id INT PRIMARY KEY, qty INT CHECK (qty <= dbo.cap()), low INT, CONSTRAINT ck_low CHECK (low >= 0 AND low <= dbo.cap()))");
    run(&mut s, "INSERT stock VALUES (1, 50, 0), (2, 100, 5)");
    // a row that breaks it undoes the whole statement
    let r = s.execute("INSERT stock VALUES (3, 10, 0), (4, 101, 0)");
    let e = r.first_error().expect("the check");
    assert_eq!(e.number, 547);
    assert!(e.message.contains("INSERT statement conflicted with the CHECK constraint"), "{}", e.message);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM stock"), "2");
    assert_eq!(error(&mut s, "INSERT stock VALUES (5, 10, 200)"), 547);
    // an update
    let r = s.execute("UPDATE stock SET qty = 500 WHERE id = 1");
    assert!(r.first_error().is_some_and(|e| e.number == 547 && e.message.contains("UPDATE statement conflicted")), "{:?}", r.first_error());
    assert_eq!(one(&mut s, "SELECT qty FROM stock WHERE id = 1"), "50");
    // the limit moves, and so does what is allowed
    run(&mut s, "UPDATE limits SET cap = 500 WHERE id = 1");
    run(&mut s, "UPDATE stock SET qty = 500 WHERE id = 1");
    // a check added later is verified against the rows that are there
    assert_eq!(error(&mut s, "ALTER TABLE stock ADD CONSTRAINT ck_big CHECK (qty < dbo.cap() / 2)"), 547);
    run(&mut s, "ALTER TABLE stock ADD CONSTRAINT ck_ok CHECK (qty <= dbo.cap())");
    // ... and dropped by name
    run(&mut s, "ALTER TABLE stock DROP CONSTRAINT ck_low");
    run(&mut s, "INSERT stock VALUES (6, 1, 100000)");
    run(&mut s, "DISABLE TRIGGER ALL ON stock");
    assert_eq!(error(&mut s, "INSERT stock VALUES (7, 100000, 1)"), 547);
    // a table that is dropped takes its checks
    run(&mut s, "DROP TABLE stock");
    run(&mut s, "CREATE TABLE stock (id INT PRIMARY KEY, qty INT)");
    run(&mut s, "INSERT stock VALUES (1, 100000)");
}

#[test]
fn a_default_that_reads_sharded_tables_is_evaluated_by_the_control_node() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    limits(&mut s);
    run(&mut s, "CREATE TABLE logd (id INT PRIMARY KEY, v INT DEFAULT dbo.cap(), w INT)");
    run(&mut s, "INSERT logd (id) VALUES (1), (2)");
    run(&mut s, "INSERT logd (id, v) VALUES (3, DEFAULT), (4, 7)");
    run(&mut s, "INSERT logd VALUES (5, DEFAULT, 1)");
    assert_eq!(rows(&mut s, "SELECT id, v FROM logd ORDER BY id"), vec![vec!["1".to_string(), "100".to_string()], vec!["2".to_string(), "100".to_string()], vec!["3".to_string(), "100".to_string()], vec!["4".to_string(), "7".to_string()], vec!["5".to_string(), "100".to_string()]]);
    run(&mut s, "ALTER TABLE logd ADD x INT CONSTRAINT df_x DEFAULT dbo.cap() + 1");
    run(&mut s, "INSERT logd (id) VALUES (6)");
    assert_eq!(one(&mut s, "SELECT x FROM logd WHERE id = 6"), "101");
}

#[test]
fn the_rules_of_a_table_are_still_there_when_the_control_node_starts_again() {
    let dir = std::env::temp_dir().join(format!("tsql-rules-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("cluster.state");
    let h = start(2, Some(&state));
    {
        let mut s = h.cluster.session();
        limits(&mut s);
        run(&mut s, "CREATE TABLE stock (id INT PRIMARY KEY, qty INT CHECK (qty <= dbo.cap()), v INT DEFAULT dbo.cap())");
        run(&mut s, "INSERT stock (id, qty) VALUES (1, 10)");
    }
    let again = Cluster::open(&state, &h.cfg).unwrap();
    let mut s = again.session();
    assert_eq!(error(&mut s, "INSERT stock (id, qty) VALUES (2, 1000)"), 547);
    run(&mut s, "INSERT stock (id, qty) VALUES (3, 20)");
    assert_eq!(one(&mut s, "SELECT v FROM stock WHERE id = 3"), "100");
    let _ = std::fs::remove_dir_all(&dir);
}
