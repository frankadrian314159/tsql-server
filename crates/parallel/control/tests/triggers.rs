//! Triggers in a cluster: the control node keeps them and fires them around the statements that change a table, in the statement's
//! transaction, over the rows the shards changed (`inserted` and `deleted`).
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

/// items (the table with triggers) and audit (what the triggers write), both sharded, over a few shards.
fn setup(s: &mut ClusterSession) {
    run(s, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(20), qty INT)");
    run(s, "CREATE TABLE audit (item INT PRIMARY KEY, action VARCHAR(10), qty INT)");
}

fn audit(s: &mut ClusterSession) -> Vec<Vec<String>> {
    rows(s, "SELECT item, action, qty FROM audit ORDER BY item")
}

fn strings(v: &[&[&str]]) -> Vec<Vec<String>> {
    v.iter().map(|r| r.iter().map(|x| x.to_string()).collect()).collect()
}

#[test]
fn after_triggers_see_the_rows_of_the_whole_statement_across_the_shards() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE TABLE batches (id INT IDENTITY(1,1) PRIMARY KEY, n INT)");
    run(&mut s, "CREATE TRIGGER trg_ins ON items AFTER INSERT AS BEGIN INSERT audit SELECT id, 'ins', qty FROM inserted; INSERT batches (n) SELECT COUNT(*) FROM inserted END");
    run(&mut s, "CREATE TRIGGER trg_upd ON items AFTER UPDATE AS BEGIN DELETE FROM audit WHERE item IN (SELECT id FROM inserted); INSERT audit SELECT id, 'upd', qty FROM inserted END");
    run(&mut s, "CREATE TRIGGER trg_del ON items AFTER DELETE AS BEGIN DELETE FROM audit WHERE item IN (SELECT id FROM deleted); INSERT audit SELECT id, 'del', qty FROM deleted END");
    // one statement, rows on every shard: the trigger runs once and sees them all
    run(&mut s, "INSERT items VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30), (4, 'd', 40), (5, 'e', 50), (6, 'f', 60)");
    assert_eq!(audit(&mut s).len(), 6);
    assert_eq!(rows(&mut s, "SELECT n FROM batches"), strings(&[&["6"]]));
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE action = 'ins'"), "6");
    // an update: inserted has the new rows and deleted the old ones
    run(&mut s, "UPDATE items SET qty = qty + 1 WHERE id <= 3");
    assert_eq!(rows(&mut s, "SELECT item, action, qty FROM audit WHERE action = 'upd' ORDER BY item"), strings(&[&["1", "upd", "11"], &["2", "upd", "21"], &["3", "upd", "31"]]));
    // a delete
    run(&mut s, "DELETE FROM items WHERE id >= 5");
    assert_eq!(rows(&mut s, "SELECT item, action, qty FROM audit WHERE action = 'del' ORDER BY item"), strings(&[&["5", "del", "50"], &["6", "del", "60"]]));
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "4");
    // a statement that changes no row fires nothing
    run(&mut s, "UPDATE items SET qty = 0 WHERE id = 999");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit"), "6");
}

#[test]
fn an_error_in_a_trigger_undoes_the_statement_and_what_the_trigger_did() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    run(&mut s, "CREATE TRIGGER trg_bad ON items AFTER INSERT AS BEGIN INSERT audit SELECT id, 'ins', qty FROM inserted; INSERT audit SELECT 1 / 0, 'x', 1 END");
    assert_eq!(error(&mut s, "INSERT items VALUES (2, 'b', 20), (3, 'c', 30)"), 8134);
    // neither the rows, nor the audit rows the trigger made before it failed
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit"), "0");
    // in a transaction of the client's own the statement is undone as far as the error: the transaction is still the client's to end
    run(&mut s, "DROP TRIGGER trg_bad");
    run(&mut s, "INSERT items VALUES (2, 'b', 20)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "2");
}

#[test]
fn a_rollback_in_a_trigger_ends_the_transaction_and_the_batch() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE TRIGGER trg_veto ON items AFTER INSERT, UPDATE AS BEGIN IF EXISTS (SELECT 1 FROM inserted WHERE qty < 0) ROLLBACK TRANSACTION END");
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    assert_eq!(error(&mut s, "INSERT items VALUES (2, 'b', -5)"), 3609);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    assert_eq!(error(&mut s, "UPDATE items SET qty = -1 WHERE id = 1"), 3609);
    assert_eq!(one(&mut s, "SELECT qty FROM items WHERE id = 1"), "10");
}

#[test]
fn update_column_says_whether_the_statement_set_it() {
    let h = start(2, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10), (2, 'b', 20)");
    run(&mut s, "CREATE TRIGGER trg_qty ON items AFTER UPDATE AS BEGIN IF UPDATE(qty) INSERT audit SELECT id, 'qty', qty FROM inserted END");
    run(&mut s, "UPDATE items SET name = 'z' WHERE id = 1");
    assert_eq!(audit(&mut s), Vec::<Vec<String>>::new());
    run(&mut s, "UPDATE items SET qty = 25 WHERE id = 2");
    assert_eq!(audit(&mut s), strings(&[&["2", "qty", "25"]]));
}

#[test]
fn an_instead_of_trigger_runs_in_place_of_the_statement() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30)");
    // a delete that only marks: the trigger's own UPDATE of the table is done for real
    run(&mut s, "CREATE TRIGGER trg_soft ON items INSTEAD OF DELETE AS BEGIN UPDATE items SET qty = -1 WHERE id IN (SELECT id FROM deleted) END");
    run(&mut s, "DELETE FROM items WHERE id <= 2");
    assert_eq!(rows(&mut s, "SELECT id, qty FROM items ORDER BY id"), strings(&[&["1", "-1"], &["2", "-1"], &["3", "30"]]));
    // an insert that is redirected, with the rows the statement would insert
    run(&mut s, "CREATE TRIGGER trg_redirect ON items INSTEAD OF INSERT AS BEGIN INSERT audit SELECT id, 'wanted', qty FROM inserted END");
    run(&mut s, "INSERT items (id, name, qty) VALUES (10, 'x', 100), (11, 'y', 110)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "3");
    assert_eq!(audit(&mut s), strings(&[&["10", "wanted", "100"], &["11", "wanted", "110"]]));
    // an update: deleted has the rows as they are, inserted as they would be
    run(&mut s, "CREATE TRIGGER trg_change ON items INSTEAD OF UPDATE AS BEGIN INSERT audit SELECT i.id, 'was ' + CAST(d.qty AS VARCHAR(10)), i.qty FROM inserted i JOIN deleted d ON d.id = i.id END");
    run(&mut s, "UPDATE items SET qty = qty * 2 WHERE id = 3");
    assert_eq!(one(&mut s, "SELECT qty FROM items WHERE id = 3"), "30");
    assert_eq!(rows(&mut s, "SELECT item, action, qty FROM audit WHERE item = 3"), strings(&[&["3", "was 30", "60"]]));
    // only one INSTEAD OF trigger for an event
    assert_eq!(error(&mut s, "CREATE TRIGGER trg_second ON items INSTEAD OF DELETE AS SELECT 1"), 2111);
}

#[test]
fn a_trigger_does_not_fire_again_for_what_it_does_to_its_own_table() {
    let h = start(2, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10), (2, 'b', 20)");
    // (direct recursion is off: this would not end if it fired for its own update)
    run(&mut s, "CREATE TRIGGER trg_touch ON items AFTER UPDATE AS BEGIN UPDATE items SET name = 'touched' WHERE id IN (SELECT id FROM inserted); INSERT audit SELECT id, 'seen', qty FROM inserted END");
    run(&mut s, "UPDATE items SET qty = 5 WHERE id = 1");
    assert_eq!(rows(&mut s, "SELECT id, name, qty FROM items WHERE id = 1"), strings(&[&["1", "touched", "5"]]));
    assert_eq!(audit(&mut s), strings(&[&["1", "seen", "5"]]));
}

#[test]
fn triggers_are_disabled_enabled_and_dropped_and_a_dropped_table_takes_its_triggers() {
    let h = start(2, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE TRIGGER trg_ins ON items AFTER INSERT AS INSERT audit SELECT id, 'ins', qty FROM inserted");
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    assert_eq!(audit(&mut s).len(), 1);
    run(&mut s, "DISABLE TRIGGER trg_ins ON items");
    run(&mut s, "INSERT items VALUES (2, 'b', 20)");
    assert_eq!(audit(&mut s).len(), 1);
    run(&mut s, "ENABLE TRIGGER trg_ins ON items");
    run(&mut s, "INSERT items VALUES (3, 'c', 30)");
    assert_eq!(audit(&mut s).len(), 2);
    assert_eq!(error(&mut s, "CREATE TRIGGER trg_ins ON items AFTER INSERT AS SELECT 1"), 2714);
    run(&mut s, "CREATE OR ALTER TRIGGER trg_ins ON items AFTER INSERT AS INSERT audit SELECT id, 'new', qty FROM inserted");
    run(&mut s, "INSERT items VALUES (4, 'd', 40)");
    assert_eq!(rows(&mut s, "SELECT action FROM audit WHERE item = 4"), strings(&[&["new"]]));
    run(&mut s, "DROP TRIGGER trg_ins");
    run(&mut s, "INSERT items VALUES (5, 'e', 50)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE item = 5"), "0");
    assert_eq!(error(&mut s, "DROP TRIGGER trg_ins"), 3701);
    run(&mut s, "DROP TRIGGER IF EXISTS trg_ins");
    // a trigger on a table that is not there, on a view, and the kinds that are not supported
    assert_eq!(error(&mut s, "CREATE TRIGGER trg_none ON nothing AFTER INSERT AS SELECT 1"), 208);
    run(&mut s, "CREATE VIEW dbo.v AS SELECT id FROM dbo.items");
    assert_eq!(error(&mut s, "CREATE TRIGGER trg_view ON dbo.v INSTEAD OF DELETE AS SELECT 1"), 50100);
    assert_eq!(error(&mut s, "CREATE TRIGGER trg_ddl ON DATABASE FOR CREATE_TABLE AS SELECT 1"), 50100);
    // a dropped table takes its triggers: a table of the same name again has none
    run(&mut s, "CREATE TRIGGER trg_again ON items AFTER INSERT AS INSERT audit SELECT id, 'ins', qty FROM inserted");
    run(&mut s, "DROP TABLE items");
    run(&mut s, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(20), qty INT)");
    run(&mut s, "INSERT items VALUES (9, 'z', 90)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE item = 9"), "0");
}

#[test]
fn an_update_of_the_shard_key_of_a_table_with_triggers_is_refused() {
    let h = start(2, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    run(&mut s, "CREATE TRIGGER trg_upd ON items AFTER UPDATE AS SELECT 1");
    assert_eq!(error(&mut s, "UPDATE items SET id = 100 WHERE id = 1"), 50100);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items WHERE id = 1"), "1");
}

#[test]
fn the_triggers_of_a_database_and_of_a_control_node_that_starts_again() {
    let dir = std::env::temp_dir().join(format!("tsql-triggers-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("cluster.state");
    let h = start(2, Some(&state));
    {
        let mut s = h.cluster.session();
        run(&mut s, "CREATE DATABASE shop");
        run(&mut s, "USE shop");
        setup(&mut s);
        run(&mut s, "CREATE TRIGGER trg_ins ON items AFTER INSERT AS INSERT audit SELECT id, 'ins', qty FROM inserted");
        run(&mut s, "CREATE TRIGGER trg_off ON items AFTER DELETE AS BEGIN DELETE FROM audit WHERE item IN (SELECT id FROM deleted); INSERT audit SELECT id, 'del', qty FROM deleted END");
        run(&mut s, "DISABLE TRIGGER trg_off ON items");
        run(&mut s, "INSERT items VALUES (1, 'a', 10)");
        assert_eq!(audit(&mut s), strings(&[&["1", "ins", "10"]]));
    }
    // a second control node over the same shards, from the state file: it has the triggers, and what was disabled stays so
    let again = Cluster::open(&state, &h.cfg).unwrap();
    let mut s = again.session();
    run(&mut s, "USE shop");
    run(&mut s, "INSERT items VALUES (2, 'b', 20)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE action = 'ins'"), "2");
    run(&mut s, "DELETE FROM items WHERE id = 1");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE action = 'del'"), "0");
    run(&mut s, "ENABLE TRIGGER trg_off ON items");
    run(&mut s, "DELETE FROM items WHERE id = 2");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE action = 'del'"), "1");
    // a query that the shards cannot split is gathered into the control node, whose copies of the tables fire nothing
    assert_eq!(one(&mut s, "SELECT CAST(STDEV(qty) AS INT) FROM audit"), "7");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_trigger_body_may_raise_errors_and_return_and_one_the_control_node_cannot_run_is_refused() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "CREATE TRIGGER trg_v ON items AFTER INSERT AS BEGIN IF EXISTS (SELECT 1 FROM inserted WHERE qty < 0) BEGIN RAISERROR('negative %s', 16, 1, 'qty'); ROLLBACK TRANSACTION; RETURN END; INSERT audit SELECT id, 'ok', qty FROM inserted END");
    run(&mut s, "INSERT items VALUES (1, 'a', 10)");
    let r = s.execute("INSERT items VALUES (2, 'b', -1)");
    let e = r.first_error().expect("raised");
    assert_eq!((e.number, e.message.as_str()), (50000, "negative qty"));
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "1");
    assert_eq!(audit(&mut s), strings(&[&["1", "ok", "10"]]));
    // RETURN ends the body
    run(&mut s, "CREATE TRIGGER trg_r ON items AFTER DELETE AS BEGIN RETURN; INSERT audit SELECT id, 'del', qty FROM deleted END");
    run(&mut s, "DELETE FROM items WHERE id = 1");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit WHERE action = 'del'"), "0");
    // what the control node cannot run is refused when the trigger is made, not when it fires
    assert_eq!(error(&mut s, "CREATE TRIGGER trg_trunc ON items AFTER INSERT AS TRUNCATE TABLE audit"), 50100);
}
