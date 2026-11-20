//! Full-text catalogs and indexes in a cluster (declarations, kept everywhere), and searches over rows on several shards.
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

fn docs(s: &mut ClusterSession) {
    run(s, "CREATE TABLE docs (id INT NOT NULL, title VARCHAR(60) NOT NULL, body NVARCHAR(200) NULL, CONSTRAINT pk_docs PRIMARY KEY (id), CONSTRAINT uq_title UNIQUE (title))");
    run(s, "INSERT docs VALUES (1,'a','the quick brown fox'),(2,'b','the lazy dog'),(3,'c','a quick dog'),(4,'d','nothing here'),(5,'e','fox and dog'),(6,'f','lazy fox')");
}

#[test]
fn a_catalog_and_an_index_are_made_on_every_node_and_follow_the_engines_rules() {
    let h = start(3);
    let mut s = h.cluster.session();
    docs(&mut s);
    run(&mut s, "CREATE FULLTEXT CATALOG ftc AS DEFAULT");
    assert_eq!(error(&mut s, "CREATE FULLTEXT CATALOG ftc"), 7609);
    // the key is a unique, non-nullable, single-column index: here the primary key, which holds the shard key, and the unique title, which does not
    assert_eq!(error(&mut s, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX nope"), 7653);
    run(&mut s, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX uq_title ON ftc WITH (CHANGE_TRACKING = MANUAL)");
    assert_eq!(error(&mut s, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX pk_docs"), 7611);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.fulltext_indexes"), "1");
    assert_eq!(one(&mut s, "SELECT change_tracking_state_desc FROM sys.fulltext_indexes"), "MANUAL");
    run(&mut s, "ALTER FULLTEXT INDEX ON docs SET CHANGE_TRACKING AUTO");
    assert_eq!(one(&mut s, "SELECT change_tracking_state_desc FROM sys.fulltext_indexes"), "AUTO");
    assert_eq!(error(&mut s, "DROP FULLTEXT CATALOG ftc"), 7610);
    run(&mut s, "DROP FULLTEXT INDEX ON docs");
    run(&mut s, "DROP FULLTEXT CATALOG ftc");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.fulltext_catalogs"), "0");
}

#[test]
fn searches_find_rows_on_every_shard() {
    let h = start(3);
    let mut s = h.cluster.session();
    docs(&mut s);
    run(&mut s, "CREATE FULLTEXT CATALOG ftc AS DEFAULT");
    run(&mut s, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX pk_docs ON ftc");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM docs WHERE CONTAINS(body, 'fox')"), "3");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM docs WHERE CONTAINS(body, 'fox AND dog')"), "1");
    assert_eq!(rows(&mut s, "SELECT id FROM docs WHERE FREETEXT(body, 'lazy') ORDER BY id").concat(), vec!["2".to_string(), "6".into()]);
    let r = rows(&mut s, "SELECT [KEY] FROM CONTAINSTABLE(docs, body, 'dog') ORDER BY [KEY]");
    assert_eq!(r.concat(), vec!["2".to_string(), "3".into(), "5".into()]);
    let r = rows(&mut s, "SELECT d.id FROM docs d INNER JOIN CONTAINSTABLE(docs, body, 'quick') AS k ON d.id = k.[KEY] ORDER BY d.id");
    assert_eq!(r.concat(), vec!["1".to_string(), "3".into()]);
}

#[test]
fn full_text_declarations_are_kept_when_the_control_node_starts_again() {
    let h = start(2);
    let state = std::env::temp_dir().join(format!("tsql-ft-test-{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    {
        let cluster = Cluster::open(&state, &cfg).unwrap();
        let mut s = cluster.session();
        docs(&mut s);
        run(&mut s, "CREATE FULLTEXT CATALOG ftc AS DEFAULT");
        run(&mut s, "CREATE FULLTEXT INDEX ON docs (body) KEY INDEX pk_docs ON ftc");
    }
    let cluster = Cluster::open(&state, &cfg).unwrap();
    let mut s = cluster.session();
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.fulltext_indexes"), "1");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM docs WHERE CONTAINS(body, 'fox')"), "3");
    let _ = std::fs::remove_file(&state);
}
