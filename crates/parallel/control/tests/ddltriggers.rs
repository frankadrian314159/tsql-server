//! DDL triggers (ON DATABASE) in a cluster: run by the control node before the statement that raises the event.
use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{BatchResult, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

struct Harness {
    _rt: tokio::runtime::Runtime,
    shards: Vec<RunningShard>,
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
    Harness { _rt: rt, shards, cluster }
}

fn ok(r: &BatchResult, what: &str) {
    assert!(r.first_error().is_none(), "{}: {:?}", what, r.first_error());
}

fn run(s: &mut ClusterSession, sql: &str) {
    ok(&s.execute(sql), sql);
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

#[test]
fn a_trigger_that_refuses_a_statement_stops_it() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE keep (id INT PRIMARY KEY, v INT)");
    run(&mut s, "INSERT keep VALUES (1, 1), (2, 2), (3, 3)");
    run(&mut s, "CREATE TRIGGER no_drops ON DATABASE FOR DROP_TABLE AS BEGIN ROLLBACK; THROW 50001, 'Tables cannot be dropped here.', 1; END");
    assert_eq!(error(&mut s, "DROP TABLE keep"), 50001);
    // the table, and its rows on the shards, are still there
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM keep"), "3");
    // other statements are not for this trigger
    run(&mut s, "ALTER TABLE keep ADD w INT NULL");
    run(&mut s, "CREATE TABLE other (id INT PRIMARY KEY)");
    // disabled, it runs no more; enabled again, it does
    run(&mut s, "DISABLE TRIGGER no_drops ON DATABASE");
    run(&mut s, "DROP TABLE other");
    run(&mut s, "ENABLE TRIGGER no_drops ON DATABASE");
    assert_eq!(error(&mut s, "DROP TABLE keep"), 50001);
    // and a dropped trigger is gone
    run(&mut s, "DROP TRIGGER no_drops ON DATABASE");
    run(&mut s, "DROP TABLE keep");
    assert_eq!(error(&mut s, "CREATE TRIGGER x ON DATABASE FOR NO_SUCH_EVENT AS SELECT 1"), 1082);
    assert_eq!(error(&mut s, "DROP TRIGGER no_drops ON DATABASE"), 3701);
}

#[test]
fn a_rollback_alone_ends_the_batch_with_3609() {
    let h = start(2);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TRIGGER stop_all ON DATABASE FOR CREATE_TABLE AS ROLLBACK");
    assert_eq!(error(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)"), 3609);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.tables WHERE name = 't'"), "0");
}

#[test]
fn a_trigger_audits_with_eventdata_into_a_sharded_table() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE audit (n INT IDENTITY(1,1) PRIMARY KEY, what VARCHAR(40), obj VARCHAR(40), cmd VARCHAR(200))");
    run(&mut s, "CREATE TRIGGER audit_ddl ON DATABASE FOR DDL_TABLE_EVENTS AS INSERT audit (what, obj, cmd) VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'nvarchar(40)'), EVENTDATA().value('(/EVENT_INSTANCE/ObjectName)[1]', 'nvarchar(40)'), EVENTDATA().value('(/EVENT_INSTANCE/TSQLCommand/CommandText)[1]', 'nvarchar(200)'))");
    run(&mut s, "CREATE TABLE a (id INT PRIMARY KEY)");
    run(&mut s, "ALTER TABLE a ADD x INT NULL");
    run(&mut s, "DROP TABLE a");
    // (an index is not a table event)
    run(&mut s, "CREATE TABLE b (id INT PRIMARY KEY, c INT)");
    run(&mut s, "CREATE INDEX ix_c ON b (c)");
    let got = rows(&mut s, "SELECT what, obj FROM audit ORDER BY n");
    assert_eq!(
        got,
        vec![
            vec!["CREATE_TABLE".to_string(), "a".into()],
            vec!["ALTER_TABLE".into(), "a".into()],
            vec!["DROP_TABLE".into(), "a".into()],
            vec!["CREATE_TABLE".into(), "b".into()],
        ]
    );
    assert_eq!(one(&mut s, "SELECT cmd FROM audit WHERE what = 'CREATE_TABLE' AND obj = 'a'"), "CREATE TABLE a (id INT PRIMARY KEY)");
}

#[test]
fn triggers_are_per_database() {
    let h = start(2);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE audit (n INT IDENTITY(1,1) PRIMARY KEY, what VARCHAR(40))");
    run(&mut s, "CREATE TRIGGER t1 ON DATABASE FOR CREATE_TABLE AS INSERT audit (what) VALUES ('seen')");
    run(&mut s, "CREATE DATABASE other");
    run(&mut s, "USE other");
    // a trigger of the first database does not run in this one
    run(&mut s, "CREATE TABLE o (id INT PRIMARY KEY)");
    run(&mut s, "USE master");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit"), "0");
    run(&mut s, "CREATE TABLE m (id INT PRIMARY KEY)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit"), "1");
}

#[test]
fn triggers_are_kept_when_the_control_node_starts_again() {
    let h = start(2);
    let state = std::env::temp_dir().join(format!("tsql-ddltrig-test-{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    {
        let cluster = Cluster::open(&state, &cfg).unwrap();
        let mut s = cluster.session();
        run(&mut s, "CREATE TRIGGER no_drops ON DATABASE FOR DROP_TABLE AS BEGIN ROLLBACK; THROW 50001, 'no', 1; END");
        run(&mut s, "CREATE TRIGGER off ON DATABASE FOR CREATE_VIEW AS THROW 50002, 'off', 1");
        run(&mut s, "DISABLE TRIGGER off ON DATABASE");
        run(&mut s, "CREATE TABLE t (id INT PRIMARY KEY)");
    }
    let cluster = Cluster::open(&state, &cfg).unwrap();
    let mut s = cluster.session();
    assert_eq!(error(&mut s, "DROP TABLE t"), 50001);
    // (the disabled one is still disabled)
    run(&mut s, "CREATE VIEW v AS SELECT id FROM t");
    let _ = std::fs::remove_file(&state);
}

#[test]
fn server_and_logon_triggers_are_not_supported() {
    let h = start(2);
    let mut s = h.cluster.session();
    assert_eq!(error(&mut s, "CREATE TRIGGER s1 ON ALL SERVER FOR CREATE_DATABASE AS SELECT 1"), 50100);
}
