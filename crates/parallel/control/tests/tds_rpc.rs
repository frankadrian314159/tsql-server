//! What the control node does with the requests of a TDS client that are not plain batches: parameterized statements with
//! OUTPUT parameters, table-valued parameters, server cursors and bulk load. The rows live on the shards.
mod raw;
use raw::*;
use std::sync::Arc;
use tsql_control::Cluster;
use tsql_server::tds::wire::*;
use tsql_server::tds::{Config as ListenConfig, Listener};
use tsql_server::{Server, Value};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

struct Front {
    rt: tokio::runtime::Runtime,
    shards: Vec<RunningShard>,
    cluster: Arc<Cluster>,
    listener: Listener,
}

fn start(n: usize) -> Front {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut shards = vec![];
    for i in 0..n {
        shards.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
    }
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = Cluster::connect(&cfg).unwrap();
    cluster.enable_tds();
    let server = cluster.mirror_server();
    server.connect_internal().execute("ALTER LOGIN sa WITH PASSWORD = N'pw'");
    let listener = Listener::bind(server, "127.0.0.1:0", ListenConfig::default()).unwrap();
    Front { rt, shards, cluster, listener }
}

fn no_error(t: &[Tok]) {
    assert!(!t.iter().any(|t| matches!(t, Tok::Error { .. })), "{:?}", t);
}

fn ints(t: &[Tok]) -> Vec<i64> {
    t.iter()
        .filter_map(|t| match t {
            Tok::Row(v) => match &v[0] {
                Value::Int(i) => Some(*i as i64),
                Value::BigInt(i) => Some(*i),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn ret(t: &[Tok], name: &str) -> Option<Value> {
    t.iter().find_map(|t| match t {
        Tok::Ret { name: n, val } if n == name => Some(val.clone()),
        _ => None,
    })
}

fn table(r: &mut Raw) {
    no_error(&r.batch("CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(20), v INT)"));
    let rows: Vec<String> = (1..=40).map(|i| format!("({}, N'n{}', {})", i, i, i * 10)).collect();
    no_error(&r.batch(&format!("INSERT INTO t VALUES {}", rows.join(","))));
}

#[test]
fn output_parameters_of_sp_executesql_come_back() {
    let f = start(3);
    let (mut r, _) = Raw::connect(&f.listener, "sa", "pw");
    table(&mut r);
    // an OUTPUT parameter assigned by SELECT, by SET with a subquery, and an input one next to it
    let t = r.rpc(
        Err(10),
        &[("", false, nvarchar("SELECT @n = COUNT(*), @s = SUM(v) FROM t WHERE id <= @k")), ("", false, nvarchar("@k int, @n int OUTPUT, @s bigint OUTPUT")), ("@k", false, int(10)), ("@n", true, null_int()), ("@s", true, null_int())],
    );
    no_error(&t);
    assert!(matches!(ret(&t, "@n"), Some(Value::Int(10))), "{:?}", t);
    assert!(matches!(ret(&t, "@s"), Some(Value::BigInt(550)) | Some(Value::Int(550))), "{:?}", t);
    let t = r.rpc(Err(10), &[("", false, nvarchar("SET @n = (SELECT COUNT(*) FROM t WHERE v > 100); SELECT @n AS echo")), ("", false, nvarchar("@n int OUTPUT")), ("@n", true, null_int())]);
    no_error(&t);
    assert!(matches!(ret(&t, "@n"), Some(Value::Int(30))), "{:?}", t);
    assert_eq!(ints(&t), vec![30]);
    drop(f);
}

#[test]
fn table_valued_parameters_join_with_distributed_tables() {
    let f = start(3);
    let (mut r, _) = Raw::connect(&f.listener, "sa", "pw");
    table(&mut r);
    no_error(&r.batch("CREATE TYPE dbo.IdList AS TABLE (id INT NOT NULL PRIMARY KEY, tag NVARCHAR(10) NULL)"));
    let cols = [Wire::Int(4), Wire::NVarChar(10)];
    let rows = vec![vec![Value::Int(3), Value::str("x")], vec![Value::Int(7), Value::Null], vec![Value::Int(99), Value::str("none")]];
    let t = r.rpc(Err(10), &[("", false, nvarchar("SELECT COUNT(*) AS n FROM t WHERE id IN (SELECT id FROM @ids)")), ("", false, nvarchar("@ids dbo.IdList READONLY")), ("@ids", false, tvp("dbo", "IdList", &cols, &rows))]);
    no_error(&t);
    assert_eq!(ints(&t), vec![2], "{:?}", t);
    let t = r.rpc(Err(10), &[("", false, nvarchar("SELECT t.v, i.tag FROM t JOIN @ids i ON i.id = t.id ORDER BY t.id")), ("", false, nvarchar("@ids dbo.IdList READONLY")), ("@ids", false, tvp("dbo", "IdList", &cols, &rows))]);
    no_error(&t);
    assert_eq!(ints(&t), vec![30, 70], "{:?}", t);
    drop(f);
}

#[test]
fn server_cursors_read_and_change_distributed_tables() {
    let f = start(3);
    let (mut r, _) = Raw::connect(&f.listener, "sa", "pw");
    table(&mut r);
    // a static cursor over a distributed table
    let open = |r: &mut Raw, scroll: i32, cc: i32, sql: &str| r.rpc(Err(2), &[("", true, null_int()), ("", false, nvarchar(sql)), ("", true, int(scroll)), ("", true, int(cc)), ("", true, null_int())]);
    let toks = open(&mut r, 8, 1, "SELECT id, v FROM t ORDER BY id");
    no_error(&toks);
    let rets = ret_ints(&toks);
    let cursor = rets[0];
    assert_eq!(rets[3], 40, "{:?}", toks);
    assert_eq!(fetched(&fetch(&mut r, cursor, 2, 0, 3)), vec![1, 2, 3]);
    assert_eq!(fetched(&fetch(&mut r, cursor, 8, 0, 2)), vec![39, 40]); // LAST
    assert_eq!(fetched(&fetch(&mut r, cursor, 0x10, 10, 1)), vec![10]); // ABSOLUTE 10
    no_error(&r.rpc(Err(9), &[("", false, int(cursor))]));
    // a keyset cursor, updatable: the changes go to the shards
    let toks = open(&mut r, 1, 4, "SELECT id, name FROM t ORDER BY id");
    no_error(&toks);
    let rets = ret_ints(&toks);
    assert_eq!(&rets[1..3], &[1, 4], "a keyset, optimistic cursor: {:?}", toks);
    let cursor = rets[0];
    let t = fetch(&mut r, cursor, 1, 0, 3);
    assert_eq!(ct_rows(&t).len(), 3, "{:?}", t);
    let t = change(&mut r, cursor, 33, 1, "t", &[("@name", nvarchar("changed"))]); // UPDATE the first row
    no_error(&t);
    assert!(t.iter().any(|t| matches!(t, Tok::Status(0))), "{:?}", t);
    let t = r.batch("SELECT name FROM t WHERE id = 1");
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(&v[0], Value::Str(s) if s == "changed"))), "{:?}", t);
    let t = change(&mut r, cursor, 34, 2, "t", &[]); // DELETE the second
    no_error(&t);
    assert_eq!(ints(&r.batch("SELECT COUNT(*) FROM t")), vec![39]);
    drop(f);
}

#[test]
fn table_variables_declared_in_a_batch_work_with_distributed_tables() {
    let f = start(3);
    let (mut r, _) = Raw::connect(&f.listener, "sa", "pw");
    table(&mut r);
    let t = r.batch("DECLARE @t TABLE (id INT, tag VARCHAR(5)); INSERT @t VALUES (3, 'a'), (7, 'b'); INSERT @t SELECT id, 'c' FROM t WHERE id > 38; SELECT t.v FROM t JOIN @t x ON x.id = t.id ORDER BY t.id");
    no_error(&t);
    assert_eq!(ints(&t), vec![30, 70, 390, 400], "{:?}", t);
    drop(f);
}
