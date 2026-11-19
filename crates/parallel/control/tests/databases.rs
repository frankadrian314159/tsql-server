//! Databases in a cluster: `CREATE DATABASE`, `USE`, tables named by their database, `DROP DATABASE`, and a control node that starts
//! again from its state file knowing them.
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

fn fill(s: &mut ClusterSession, table: &str) {
    run(s, &format!("CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(20), qty INT)", table));
    let values: Vec<String> = (1..=300).map(|i| format!("({}, 'n{}', {})", i, i, i % 7)).collect();
    run(s, &format!("INSERT INTO {} VALUES {}", table, values.join(", ")));
}

#[test]
fn a_table_made_in_a_database_is_in_it_and_not_in_master() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE DATABASE shop");
    run(&mut s, "USE shop");
    fill(&mut s, "items");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "300");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM dbo.items WHERE qty = 3"), "43");
    assert_eq!(one(&mut s, "SELECT SUM(qty) FROM shop.dbo.items"), "903");
    // the rows are on the shards, in the database
    let mut sh = 0;
    for t in h.cluster.tables() {
        if t.id == "shop.dbo.items" {
            sh += 1;
            assert_eq!(t.name, vec!["shop".to_string(), "dbo".to_string(), "items".to_string()]);
        }
        assert_ne!(t.id, "dbo.items", "the table is not one of master's");
    }
    assert_eq!(sh, 1);
    // master has no such table; the same name can be made there too
    run(&mut s, "USE master");
    assert_eq!(error(&mut s, "SELECT COUNT(*) FROM items"), 208);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM shop.dbo.items"), "300");
    run(&mut s, "CREATE TABLE items (id INT PRIMARY KEY, other INT)");
    run(&mut s, "INSERT INTO items VALUES (1, 10), (2, 20)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "2");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM shop.dbo.items"), "300");
    run(&mut s, "USE shop");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "300");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM master.dbo.items"), "2");
}

#[test]
fn a_query_joins_tables_of_two_databases_and_a_schema_belongs_to_its_database() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE DATABASE a");
    run(&mut s, "CREATE DATABASE b");
    run(&mut s, "USE a");
    fill(&mut s, "t");
    run(&mut s, "USE b");
    fill(&mut s, "t");
    run(&mut s, "UPDATE t SET qty = qty + 100");
    assert_eq!(one(&mut s, "SELECT SUM(qty) FROM a.dbo.t"), "903");
    assert_eq!(one(&mut s, "SELECT SUM(qty) FROM b.dbo.t"), "30903");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM a.dbo.t x JOIN b.dbo.t y ON x.id = y.id WHERE y.qty - x.qty = 100"), "300");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM t x JOIN a.dbo.t y ON x.id = y.id WHERE x.qty - y.qty = 100"), "300");
    run(&mut s, "DELETE FROM t WHERE id > 10");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM t"), "10");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM a.dbo.t"), "300");
}

#[test]
fn a_view_and_an_index_of_a_database_and_dropping_the_database() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE DATABASE shop");
    run(&mut s, "USE shop");
    fill(&mut s, "items");
    run(&mut s, "CREATE INDEX ix_qty ON items (qty)");
    run(&mut s, "CREATE VIEW v_big AS SELECT id, qty FROM items WHERE qty >= 5");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM v_big"), "86");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM shop.dbo.v_big"), "86");
    // a database in use cannot be dropped; once it is left, it goes with everything in it
    assert_eq!(error(&mut s, "DROP DATABASE shop"), 3702);
    run(&mut s, "USE master");
    run(&mut s, "DROP DATABASE shop");
    assert!(h.cluster.tables().iter().all(|t| !t.id.starts_with("shop.")));
    assert_eq!(error(&mut s, "USE shop"), 911);
    assert_eq!(error(&mut s, "SELECT COUNT(*) FROM shop.dbo.items"), 208);
    // the name can be used again, for a database with nothing of the old one in it
    run(&mut s, "CREATE DATABASE shop");
    run(&mut s, "USE shop");
    assert_eq!(error(&mut s, "SELECT COUNT(*) FROM items"), 208);
    assert_eq!(error(&mut s, "CREATE DATABASE master"), 1801);
    assert_eq!(error(&mut s, "DROP DATABASE nope"), 3701);
}

#[test]
fn a_control_node_that_starts_again_has_its_databases_and_their_tables() {
    let dir = std::env::temp_dir().join(format!("tsql-databases-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("cluster.state");
    let h = start(3, Some(&state));
    {
        let mut s = h.cluster.session();
        run(&mut s, "CREATE DATABASE shop");
        run(&mut s, "CREATE DATABASE other");
        run(&mut s, "USE shop");
        fill(&mut s, "items");
        run(&mut s, "CREATE VIEW v_all AS SELECT id FROM items");
        run(&mut s, "USE other");
        run(&mut s, "CREATE TABLE t (a INT PRIMARY KEY)");
        run(&mut s, "USE master");
        run(&mut s, "CREATE TABLE m (a INT PRIMARY KEY)");
    }
    // a second control node over the same shards, started from the state file
    let again = Cluster::open(&state, &h.cfg).unwrap();
    let mut s = again.session();
    run(&mut s, "USE shop");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM items"), "300");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM v_all"), "300");
    run(&mut s, "USE other");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM t"), "0");
    run(&mut s, "USE master");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM m"), "0");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM shop.dbo.items"), "300");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a database of the cluster does, against a database of one server: the same statements, the same answers.
#[test]
fn a_database_of_the_cluster_answers_as_one_of_a_single_server_does() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    let server = Server::new();
    let mut o = server.connect();
    let both = |s: &mut ClusterSession, o: &mut tsql_server::Connection, sql: &str| {
        let a = s.execute(sql);
        let b = o.execute(sql);
        assert!(a.first_error().is_none(), "cluster: {}: {:?}", sql, a.first_error());
        assert!(b.first_error().is_none(), "single: {}: {:?}", sql, b.first_error());
    };
    for sql in ["CREATE DATABASE shop", "USE shop"] {
        both(&mut s, &mut o, sql);
    }
    both(&mut s, &mut o, "CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR(20) NOT NULL, region VARCHAR(5))");
    both(&mut s, &mut o, "CREATE TABLE orders (id INT IDENTITY(1,1) PRIMARY KEY, customer_id INT NOT NULL REFERENCES customers (id), amount DECIMAL(10,2), note VARCHAR(10) DEFAULT 'none')");
    both(&mut s, &mut o, "CREATE TABLE regions (code VARCHAR(5) PRIMARY KEY, label VARCHAR(20))");
    run(&mut s, "DISTRIBUTE TABLE regions REPLICATED");
    assert!(h.cluster.tables().iter().any(|t| t.id == "shop.dbo.regions" && matches!(t.dist, tsql_control::Dist::Replicated)), "regions is replicated");
    both(&mut s, &mut o, "INSERT INTO regions VALUES ('n', 'North'), ('s', 'South'), ('e', 'East')");
    let cust: Vec<String> = (1..=120).map(|i| format!("({}, 'c{}', '{}')", i, i, ["n", "s", "e"][i % 3])).collect();
    both(&mut s, &mut o, &format!("INSERT INTO customers VALUES {}", cust.join(", ")));
    let ord: Vec<String> = (1..=600).map(|i| format!("({}, {}.25)", i % 120 + 1, i)).collect();
    both(&mut s, &mut o, &format!("INSERT INTO orders (customer_id, amount) VALUES {}", ord.join(", ")));
    both(&mut s, &mut o, "CREATE INDEX ix_cust ON orders (customer_id)");
    both(&mut s, &mut o, "UPDATE orders SET note = 'big' WHERE amount > 500");
    both(&mut s, &mut o, "DELETE FROM orders WHERE customer_id = 7");
    both(&mut s, &mut o, "CREATE VIEW v_tot AS SELECT customer_id, COUNT(*) AS n, SUM(amount) AS total FROM orders GROUP BY customer_id");
    both(&mut s, &mut o, "ALTER TABLE customers ADD vip BIT NULL");
    both(&mut s, &mut o, "BEGIN TRANSACTION");
    both(&mut s, &mut o, "UPDATE customers SET vip = 1 WHERE id <= 10");
    both(&mut s, &mut o, "COMMIT");
    for q in [
        "SELECT COUNT(*), SUM(amount), MIN(id), MAX(id) FROM orders",
        "SELECT note, COUNT(*) FROM orders GROUP BY note ORDER BY note",
        "SELECT c.region, COUNT(*), SUM(o.amount) FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region",
        "SELECT r.label, COUNT(*) FROM customers c JOIN regions r ON r.code = c.region GROUP BY r.label ORDER BY r.label",
        "SELECT TOP (5) customer_id, n, total FROM v_tot ORDER BY total DESC, customer_id",
        "SELECT COUNT(*) FROM customers WHERE vip = 1",
        "SELECT COUNT(*) FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > 590)",
        "SELECT name FROM customers WHERE id = 42",
        "SELECT id, amount FROM orders WHERE customer_id = 9 ORDER BY id",
    ] {
        let a = rows(&mut s, q);
        let b: Vec<Vec<String>> = o.execute(q).resultsets().first().map(|rs| rs.rows.iter().map(|r| r.iter().map(display).collect()).collect()).unwrap_or_default();
        assert_eq!(a, b, "{}", q);
    }
    // the same table names in another database are other tables
    run(&mut s, "CREATE DATABASE other");
    run(&mut s, "USE other");
    run(&mut s, "CREATE TABLE customers (id INT PRIMARY KEY, v INT)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM customers"), "0");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM shop.dbo.customers"), "120");
    // TRUNCATE and DROP TABLE act on the table of the database the session is in
    run(&mut s, "USE shop");
    run(&mut s, "TRUNCATE TABLE orders");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM orders"), "0");
    run(&mut s, "DROP VIEW v_tot");
    run(&mut s, "DROP TABLE orders");
    assert_eq!(error(&mut s, "SELECT COUNT(*) FROM orders"), 208);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM other.dbo.customers"), "0");
}

#[test]
fn a_procedure_of_a_database_runs_on_the_shard_of_its_key() {
    let h = start(3, None);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE DATABASE shop");
    run(&mut s, "USE shop");
    fill(&mut s, "items");
    run(&mut s, "CREATE PROCEDURE p_get @id INT AS SELECT name FROM items WHERE id = @id");
    run(&mut s, "DISTRIBUTE PROCEDURE p_get BY @id");
    assert_eq!(one(&mut s, "EXEC p_get 9"), "n9");
    assert_eq!(one(&mut s, "EXEC shop.dbo.p_get 11"), "n11");
}
