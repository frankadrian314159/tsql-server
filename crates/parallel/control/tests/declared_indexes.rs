//! XML and spatial indexes in a cluster: declarations, kept by the shards and the control node's engine, logged, and restored.
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

fn one(s: &mut ClusterSession, sql: &str) -> String {
    let r = s.execute(sql);
    ok(&r, sql);
    r.resultsets().first().and_then(|rs| rs.rows.first()).and_then(|row| row.first()).map(display).unwrap_or_default()
}

fn error(s: &mut ClusterSession, sql: &str) -> i32 {
    s.execute(sql).first_error().map(|e| e.number).unwrap_or(0)
}

#[test]
fn xml_indexes_are_made_everywhere_and_follow_the_engines_rules() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE doc (id INT PRIMARY KEY, body XML)");
    run(&mut s, "INSERT doc VALUES (1, '<a><b>x</b></a>'), (2, '<a><b>y</b></a>'), (3, '<a/>'), (4, '<a><b>z</b></a>')");
    run(&mut s, "CREATE PRIMARY XML INDEX pxml_body ON doc (body)");
    run(&mut s, "CREATE XML INDEX xp_body ON doc (body) USING XML INDEX pxml_body FOR PATH");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.xml_indexes"), "2");
    // the engine's own rules are the cluster's: one primary index per column, names shared with the table's other indexes
    assert_eq!(error(&mut s, "CREATE PRIMARY XML INDEX pxml_2 ON doc (body)"), 6324);
    // queries on the xml column go on working
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM doc WHERE body.exist('/a/b') = 1"), "3");
    // dropping the primary index takes the secondary one with it
    run(&mut s, "DROP INDEX pxml_body ON doc");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.xml_indexes"), "0");
}

#[test]
fn spatial_indexes_are_made_everywhere() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE places (id INT PRIMARY KEY, g GEOMETRY)");
    run(&mut s, "INSERT places VALUES (1, geometry::STGeomFromText('POINT(1 1)', 0)), (2, geometry::STGeomFromText('POINT(5 5)', 0)), (3, geometry::STGeomFromText('POINT(9 9)', 0))");
    run(&mut s, "CREATE SPATIAL INDEX sx ON places (g) USING GEOMETRY_GRID WITH (BOUNDING_BOX = (0, 0, 10, 10))");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.spatial_indexes"), "1");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM places WHERE g.STDistance(geometry::STGeomFromText('POINT(1 1)', 0)) < 2"), "1");
    run(&mut s, "DROP INDEX sx ON places");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.spatial_indexes"), "0");
}

#[test]
fn declared_indexes_are_kept_when_the_control_node_starts_again() {
    let h = start(2);
    let state = std::env::temp_dir().join(format!("tsql-declidx-test-{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    {
        let cluster = Cluster::open(&state, &cfg).unwrap();
        let mut s = cluster.session();
        run(&mut s, "CREATE TABLE doc (id INT PRIMARY KEY, body XML)");
        run(&mut s, "CREATE PRIMARY XML INDEX pxml_body ON doc (body)");
    }
    let cluster = Cluster::open(&state, &cfg).unwrap();
    let mut s = cluster.session();
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.xml_indexes"), "1");
    let _ = std::fs::remove_file(&state);
}
