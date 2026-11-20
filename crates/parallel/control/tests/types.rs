//! User-defined types (`CREATE TYPE`) in a cluster: made on the shards and the control node, used by columns, variables and procedures.
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
fn an_alias_type_is_a_column_type_on_every_shard() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TYPE dbo.Phone FROM VARCHAR(20) NOT NULL");
    run(&mut s, "CREATE TABLE contacts (id INT PRIMARY KEY, p dbo.Phone)");
    run(&mut s, "INSERT contacts VALUES (1,'555-1'),(2,'555-2'),(3,'555-3'),(4,'555-4'),(5,'555-5')");
    // the type's rule (NOT NULL) is the column's
    assert_eq!(error(&mut s, "INSERT contacts VALUES (6, NULL)"), 515);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM contacts WHERE p LIKE '555-%'"), "5");
    // a variable of the type, and a CAST to it
    run(&mut s, "DECLARE @p dbo.Phone = '555-9'; INSERT contacts VALUES (9, @p)");
    assert_eq!(one(&mut s, "SELECT CAST('x' AS dbo.Phone)"), "x");
    // the catalog shows the column with its type
    assert_eq!(one(&mut s, "SELECT t.name FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id WHERE c.object_id = OBJECT_ID('dbo.contacts') AND c.name = 'p'"), "Phone");
    // a dropped type is gone from every shard
    run(&mut s, "DROP TABLE contacts");
    run(&mut s, "DROP TYPE dbo.Phone");
    assert_eq!(error(&mut s, "CREATE TABLE c2 (id INT PRIMARY KEY, p dbo.Phone)"), 2715);
}

#[test]
fn a_column_of_an_alias_type_may_be_the_shard_key() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TYPE dbo.Code FROM INT NOT NULL");
    run(&mut s, "CREATE TABLE coded (c dbo.Code PRIMARY KEY, v VARCHAR(10))");
    run(&mut s, "INSERT coded VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')");
    assert_eq!(one(&mut s, "SELECT v FROM coded WHERE c = 3"), "c");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM coded"), "4");
}

#[test]
fn a_table_type_is_a_parameter_of_a_procedure_and_a_variable() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(10))");
    run(&mut s, "INSERT items VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')");
    run(&mut s, "CREATE TYPE dbo.IdList AS TABLE (id INT)");
    run(&mut s, "CREATE PROCEDURE dbo.pick @ids dbo.IdList READONLY AS SELECT name FROM items WHERE id IN (SELECT id FROM @ids) ORDER BY name");
    run(&mut s, "DECLARE @l dbo.IdList; INSERT @l VALUES (2), (4); EXEC dbo.pick @l");
    let r = s.execute("DECLARE @l dbo.IdList; INSERT @l VALUES (2), (4); EXEC dbo.pick @l");
    ok(&r, "exec with a table-valued parameter");
    let got: Vec<Vec<String>> = r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(display).collect()).collect()).unwrap_or_default();
    assert_eq!(got, vec![vec!["b".to_string()], vec!["d".to_string()]]);
}

#[test]
fn types_are_kept_when_the_control_node_starts_again() {
    let h = start(2);
    let state = std::env::temp_dir().join(format!("tsql-types-test-{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    {
        let cluster = Cluster::open(&state, &cfg).unwrap();
        let mut s = cluster.session();
        run(&mut s, "CREATE TYPE dbo.Phone FROM VARCHAR(20) NULL");
        run(&mut s, "CREATE TABLE contacts (id INT PRIMARY KEY, p dbo.Phone)");
        run(&mut s, "INSERT contacts VALUES (1, 'x'), (2, 'y')");
    }
    let cluster = Cluster::open(&state, &cfg).unwrap();
    let mut s = cluster.session();
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.types WHERE name = 'Phone'"), "1");
    run(&mut s, "INSERT contacts VALUES (3, 'z')");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM contacts"), "3");
    let _ = std::fs::remove_file(&state);
}
