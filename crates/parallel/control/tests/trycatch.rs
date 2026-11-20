//! TRY ... CATCH on the control node: in a batch and in a trigger body.
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

/// items and audit, both sharded.
fn setup(s: &mut ClusterSession) {
    run(s, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(20), qty INT)");
    run(s, "CREATE TABLE audit (item INT PRIMARY KEY, action VARCHAR(200), qty INT)");
}

const MANY: &str = "INSERT items VALUES (2, 'b', 1), (3, 'c', 1), (4, 'd', 1), (5, 'e', 1), (6, 'f', 1), (7, 'g', 1), (8, 'h', 1), (1, 'dup', 1)";

#[test]
fn an_error_in_the_try_block_runs_the_catch_block_and_the_batch_goes_on() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    run(&mut s, "BEGIN TRY INSERT items VALUES (1, 'dup', 1); INSERT audit VALUES (9, 'not reached', 0) END TRY BEGIN CATCH INSERT audit SELECT ERROR_NUMBER(), ERROR_MESSAGE(), ERROR_SEVERITY() END CATCH; INSERT audit VALUES (100, 'after', 0)");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 100"), "after");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE item = 9"), "0");
    assert!(one(&mut s, "SELECT action FROM audit WHERE item = 2627").contains("PRIMARY KEY"));
    assert_eq!(one(&mut s, "SELECT qty FROM audit WHERE item = 2627"), "14");
    // the failed statement changed nothing
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    // no error: the CATCH block does not run
    run(&mut s, "BEGIN TRY INSERT items VALUES (2, 'b', 20) END TRY BEGIN CATCH INSERT audit VALUES (3, 'wrong', 0) END CATCH");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE item = 3"), "0");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "2");
}

#[test]
fn throw_without_arguments_repeats_the_error_and_a_catch_block_may_raise_its_own() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    assert_eq!(error(&mut s, "BEGIN TRY INSERT items VALUES (1, 'dup', 1) END TRY BEGIN CATCH THROW END CATCH"), 2627);
    assert_eq!(error(&mut s, "SELECT COUNT(*) FROM items; THROW"), 10704);
    let r = s.execute("BEGIN TRY INSERT items VALUES (1, 'dup', 1) END TRY BEGIN CATCH RAISERROR('wrapped: %s', 16, 1, 'x') END CATCH");
    let e = r.first_error().expect("raised");
    assert_eq!((e.number, e.message.as_str()), (50000, "wrapped: x"));
    // an error of the CATCH block is not caught by its own TRY; an outer one catches it
    run(&mut s, "BEGIN TRY BEGIN TRY INSERT items VALUES (1, 'dup', 1) END TRY BEGIN CATCH THROW 51000, 'inner', 1 END CATCH END TRY BEGIN CATCH INSERT audit SELECT ERROR_NUMBER(), ERROR_MESSAGE(), 0 END CATCH");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 51000"), "inner");
}

#[test]
fn a_statement_that_fails_on_one_shard_leaves_the_transaction_usable_and_one_that_changed_several_dooms_it() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    // one row, one shard: the shard undid the statement, and the transaction goes on
    run(&mut s, "BEGIN TRANSACTION");
    run(&mut s, "BEGIN TRY INSERT items VALUES (1, 'dup', 1) END TRY BEGIN CATCH INSERT audit SELECT 1, 'state ' + CAST(XACT_STATE() AS VARCHAR(5)), 0 END CATCH");
    run(&mut s, "COMMIT");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 1"), "state 1");
    // many rows on several shards, the last of which fails: some shards have the rows already, so the transaction can only be rolled back
    run(&mut s, "BEGIN TRANSACTION");
    run(&mut s, &format!("BEGIN TRY {} END TRY BEGIN CATCH SELECT 1 END CATCH", MANY));
    assert_eq!(one(&mut s, "SELECT XACT_STATE()"), "-1");
    // reads are allowed, writes are not (3930), ROLLBACK is
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items WHERE id = 1"), "1");
    assert_eq!(error(&mut s, "INSERT audit VALUES (50, 'x', 0)"), 3930);
    run(&mut s, "ROLLBACK");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    assert_eq!(one(&mut s, "SELECT XACT_STATE()"), "0");
}

#[test]
fn a_catch_block_may_roll_back_a_doomed_transaction_and_return() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    run(&mut s, "BEGIN TRANSACTION");
    run(&mut s, &format!("BEGIN TRY {} END TRY BEGIN CATCH IF XACT_STATE() = -1 ROLLBACK TRANSACTION END CATCH", MANY));
    assert_eq!(one(&mut s, "SELECT XACT_STATE()"), "0");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    // RETURN in a TRY block ends the statements around it
    run(&mut s, "BEGIN TRY INSERT audit VALUES (1, 'one', 0); RETURN; INSERT audit VALUES (2, 'two', 0) END TRY BEGIN CATCH INSERT audit VALUES (3, 'three', 0) END CATCH");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit"), "1");
}

#[test]
fn a_trigger_body_may_catch_its_own_errors() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE TRIGGER trg_log ON items AFTER INSERT AS BEGIN BEGIN TRY INSERT audit SELECT id, 'ins', qty FROM inserted; INSERT audit SELECT 1 / 0, 'x', 1 END TRY BEGIN CATCH INSERT audit VALUES (1000, 'caught ' + CAST(ERROR_NUMBER() AS VARCHAR(10)), 0) END CATCH END");
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 1000"), "caught 8134");
    // an error the trigger does not catch ends the statement; in a transaction of the client's, the transaction can then only be rolled back
    run(&mut s, "DROP TRIGGER trg_log");
    run(&mut s, "CREATE TRIGGER trg_bad ON items AFTER INSERT AS INSERT audit SELECT 1 / 0, 'x', 1");
    run(&mut s, "BEGIN TRANSACTION");
    assert_eq!(error(&mut s, "INSERT items VALUES (2, 'b', 20)"), 8134);
    assert_eq!(one(&mut s, "SELECT XACT_STATE()"), "-1");
    run(&mut s, "ROLLBACK");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    // a catch in the batch around the statement does the same
    run(&mut s, "BEGIN TRY INSERT items VALUES (3, 'c', 30) END TRY BEGIN CATCH INSERT audit VALUES (2000, 'batch caught', 0) END CATCH");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    assert_eq!(one(&mut s, "SELECT action FROM audit WHERE item = 2000"), "batch caught");
}
