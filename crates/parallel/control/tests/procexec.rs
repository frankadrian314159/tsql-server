//! `EXEC` on the control node: procedures that are not routed to a shard run here, so EXEC works in a batch, in a procedure and in a trigger.
use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{BatchResult, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

struct Harness {
    _rt: tokio::runtime::Runtime,
    _shards: Vec<RunningShard>,
    cluster: Arc<Cluster>,
}

fn start(n: usize) -> Harness {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut shards = vec![];
    for i in 0..n {
        shards.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
    }
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = Cluster::connect(&cfg).unwrap();
    Harness { _rt: rt, _shards: shards, cluster }
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

fn setup(s: &mut ClusterSession) {
    run(s, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(20), qty INT)");
    run(s, "CREATE TABLE audit (item INT PRIMARY KEY, action VARCHAR(100), qty INT)");
    run(s, "INSERT items VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30), (4, 'd', 40), (5, 'e', 50), (6, 'f', 60)");
}

#[test]
fn a_procedure_that_reads_sharded_tables_runs_here_with_parameters_defaults_and_a_return_value() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE PROCEDURE total_over @min INT, @extra INT = 5 AS BEGIN DECLARE @t INT; SELECT @t = SUM(qty) FROM items WHERE qty > @min; SELECT @t + @extra AS total; RETURN @t END");
    // the answer is over all the rows, not a shard's slice
    assert!(s.wants("EXEC total_over 20"));
    assert_eq!(one(&mut s, "EXEC total_over 20"), "185");
    assert_eq!(one(&mut s, "EXEC total_over @extra = 0, @min = 20"), "180");
    // the return value and the variables of the caller
    run(&mut s, "DECLARE @rc INT; EXEC @rc = total_over 40; INSERT audit VALUES (1, 'rc ' + CAST(@rc AS VARCHAR(10)), 0)");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 1"), "rc 110");
    // the arguments are checked
    assert_eq!(error(&mut s, "EXEC total_over"), 201);
    assert_eq!(error(&mut s, "EXEC total_over 1, 2, 3"), 8144);
    assert_eq!(error(&mut s, "EXEC total_over @nope = 1"), 8145);
    // a string is converted to the parameter's type
    assert_eq!(one(&mut s, "EXEC total_over '20'"), "185");
}

#[test]
fn output_parameters_nested_calls_loops_and_catching_work_in_a_procedure() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE PROCEDURE count_big @min INT, @n INT OUTPUT AS SELECT @n = COUNT(*) FROM items WHERE qty >= @min");
    run(&mut s, "DECLARE @c INT; EXEC count_big 30, @c OUTPUT; INSERT audit VALUES (10, 'big ' + CAST(@c AS VARCHAR(10)), 0)");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 10"), "big 4");
    // a procedure that calls another, loops with BREAK and CONTINUE, and writes
    run(&mut s, "CREATE PROCEDURE note @k INT, @what VARCHAR(50) AS INSERT audit VALUES (@k, @what, 0)");
    run(&mut s, "CREATE PROCEDURE walk AS BEGIN DECLARE @i INT = 0; WHILE 1 = 1 BEGIN SET @i = @i + 1; IF @i = 2 CONTINUE; IF @i > 4 BREAK; DECLARE @w VARCHAR(50) = 'step ' + CAST(@i AS VARCHAR(10)); EXEC note @i, @w END END");
    run(&mut s, "EXEC walk");
    assert_eq!(rows(&mut s, "SELECT item, action FROM audit WHERE item < 10 ORDER BY item"), vec![vec!["1".to_string(), "step 1".to_string()], vec!["3".to_string(), "step 3".to_string()], vec!["4".to_string(), "step 4".to_string()]]);
    // TRY ... CATCH in a procedure, and the error that leaves it
    run(&mut s, "CREATE PROCEDURE safe_insert @k INT AS BEGIN BEGIN TRY INSERT items VALUES (@k, 'x', 1) END TRY BEGIN CATCH RETURN -1 END CATCH RETURN 0 END");
    run(&mut s, "DECLARE @r1 INT, @r2 INT; EXEC @r1 = safe_insert 1; EXEC @r2 = safe_insert 100; INSERT audit VALUES (20, CAST(@r1 AS VARCHAR(5)) + ' ' + CAST(@r2 AS VARCHAR(5)), 0)");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 20"), "-1 0");
    run(&mut s, "CREATE PROCEDURE boom AS THROW 51000, 'boom', 1");
    assert_eq!(error(&mut s, "EXEC boom"), 51000);
    // the caller's variables are not the procedure's
    run(&mut s, "CREATE PROCEDURE peek AS SELECT @secret");
    assert_ne!(error(&mut s, "DECLARE @secret INT = 5; EXEC peek"), 0);
}

#[test]
fn a_trigger_may_exec_a_procedure_of_the_cluster_routed_or_not() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    // a routed one: it runs on the shard of its key
    run(&mut s, "CREATE PROCEDURE log_it @k INT AS INSERT audit (item, action, qty) VALUES (@k, 'logged', 0)");
    run(&mut s, "DISTRIBUTE PROCEDURE log_it BY @k");
    // one that is not: it reads every shard's rows
    run(&mut s, "CREATE PROCEDURE log_total @k INT AS INSERT audit SELECT @k + 1000, 'total ' + CAST((SELECT SUM(qty) FROM items) AS VARCHAR(20)), 0");
    run(&mut s, "CREATE TRIGGER trg_calls ON items AFTER DELETE AS BEGIN DECLARE @k INT; SELECT @k = MIN(id) FROM deleted; EXEC log_it @k; EXEC log_total @k END");
    run(&mut s, "DELETE FROM items WHERE id = 6");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 6"), "logged");
    // (the trigger runs after the delete: the 60 is gone)
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 1006"), "total 150");
    // an error in the procedure undoes the statement
    run(&mut s, "CREATE PROCEDURE log_bad AS THROW 51000, 'no', 1");
    run(&mut s, "DROP TRIGGER trg_calls");
    run(&mut s, "CREATE TRIGGER trg_bad ON items AFTER DELETE AS EXEC log_bad");
    assert_eq!(error(&mut s, "DELETE FROM items WHERE id = 5"), 51000);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items WHERE id = 5"), "1");
}

#[test]
fn a_procedure_runs_in_the_database_it_was_made_in_and_a_dropped_one_is_gone() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE DATABASE shop");
    run(&mut s, "USE shop");
    run(&mut s, "CREATE TABLE items (id INT PRIMARY KEY, qty INT)");
    run(&mut s, "INSERT items VALUES (1, 5), (2, 6), (3, 7)");
    run(&mut s, "CREATE PROCEDURE shop_total AS SELECT SUM(qty) FROM items");
    assert_eq!(one(&mut s, "EXEC shop_total"), "18");
    run(&mut s, "USE master");
    assert_eq!(one(&mut s, "EXEC shop.dbo.shop_total"), "18");
    run(&mut s, "USE shop");
    run(&mut s, "DROP PROCEDURE shop_total");
    assert_eq!(error(&mut s, "EXEC shop_total"), 2812);
    // EXEC of a string
    assert_eq!(one(&mut s, "EXEC('SELECT COUNT(*) FROM items')"), "3");
}

#[test]
fn a_procedure_may_call_a_function_that_reads_sharded_tables_and_runs_here_but_cannot_be_routed() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE FUNCTION dbo.total_qty() RETURNS INT AS BEGIN RETURN (SELECT SUM(qty) FROM dbo.items) END");
    run(&mut s, "CREATE PROCEDURE show_total AS SELECT dbo.total_qty() AS t");
    assert_eq!(one(&mut s, "EXEC show_total"), "210");
    // it cannot run on one shard, which would answer from its slice
    run(&mut s, "CREATE PROCEDURE routed_total @k INT AS SELECT dbo.total_qty() + @k AS t");
    assert_eq!(error(&mut s, "DISTRIBUTE PROCEDURE routed_total BY @k"), 50104);
    assert_eq!(one(&mut s, "EXEC routed_total 1"), "211");
    // a function that reads only replicated tables, or nothing, is everywhere: a routed procedure may call it
    run(&mut s, "CREATE FUNCTION dbo.dbl(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END");
    run(&mut s, "CREATE PROCEDURE log_dbl @k INT AS INSERT audit (item, action, qty) VALUES (@k, 'dbl', dbo.dbl(@k))");
    run(&mut s, "DISTRIBUTE PROCEDURE log_dbl BY @k");
    run(&mut s, "EXEC log_dbl 7");
    assert_eq!(one(&mut s, "SELECT qty FROM audit WHERE item = 7"), "14");
}

#[test]
fn a_function_that_reads_no_sharded_table_works_in_dml_defaults_and_checks() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE FUNCTION dbo.dbl(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END");
    run(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, v INT DEFAULT dbo.dbl(21), w AS dbo.dbl(id), CONSTRAINT ck CHECK (dbo.dbl(id) < 100))");
    run(&mut s, "INSERT t (id) VALUES (1), (2), (3)");
    run(&mut s, "INSERT t (id, v) VALUES (4, dbo.dbl(5))");
    run(&mut s, "UPDATE t SET v = dbo.dbl(v) WHERE dbo.dbl(id) > 4");
    run(&mut s, "DELETE FROM t WHERE dbo.dbl(id) = 2");
    assert_eq!(rows(&mut s, "SELECT id, v, w FROM t ORDER BY id"), vec![vec!["2".to_string(), "42".to_string(), "4".to_string()], vec!["3".to_string(), "84".to_string(), "6".to_string()], vec!["4".to_string(), "20".to_string(), "8".to_string()]]);
    assert_ne!(error(&mut s, "INSERT t (id) VALUES (60)"), 0);
}
