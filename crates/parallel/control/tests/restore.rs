//! RESTORE DATABASE into a cluster: a backup taken on one server is restored into a cluster, its rows going straight to the
//! shards (the control node's engine gets the schema only), and the cluster then answers as the source database does.
use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{BatchResult, Connection, Event, Server, Value};
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

fn rows_of(r: &BatchResult) -> Vec<Vec<String>> {
    r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(display).collect()).collect()).unwrap_or_default()
}

fn messages(r: &BatchResult) -> Vec<String> {
    r.events.iter().filter_map(|e| if let Event::Message(m) = e { Some(m.clone()) } else { None }).collect()
}

fn count(c: &mut Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap()[0][0].clone() {
        Value::Int(n) => n as i64,
        Value::BigInt(n) => n,
        other => panic!("{:?}", other),
    }
}

fn backup_path(name: &str) -> String {
    std::env::temp_dir().join(format!("tsql-restore-test-{}-{}.bak", std::process::id(), name)).to_string_lossy().to_string()
}

/// A database on one server, backed up to `path`: 4 regions, 200 customers (with a case-sensitive name and a masked email),
/// 10,050 orders (an identity key, a default, a computed column, a check, a foreign key and an index), a view and a procedure.
/// Returns the connection to it, which is the oracle the cluster is compared with.
fn source(server: &Server, path: &str, extra: &str) -> Connection {
    let mut c = server.connect();
    let script = format!(
        "CREATE DATABASE shop;
USE shop;
CREATE TABLE regions (code CHAR(2) PRIMARY KEY, name VARCHAR(20) NOT NULL);
INSERT regions VALUES ('NA', 'North America'), ('EU', 'Europe'), ('AS', 'Asia'), ('SA', 'South America');
CREATE TABLE customers (id INT PRIMARY KEY, name NVARCHAR(40) COLLATE Latin1_General_CS_AS NOT NULL, region CHAR(2) NOT NULL REFERENCES regions (code),
    email VARCHAR(60) MASKED WITH (FUNCTION = 'email()') NULL);
CREATE TABLE d (n INT);
INSERT d VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9);
INSERT customers SELECT a.n * 10 + b.n + 1, CASE WHEN a.n % 2 = 0 THEN N'Ann' ELSE N'ann' END, CASE b.n % 4 WHEN 0 THEN 'NA' WHEN 1 THEN 'EU' WHEN 2 THEN 'AS' ELSE 'SA' END,
    CONCAT('c', a.n * 10 + b.n + 1, '@example.com') FROM d a, d b;
INSERT customers SELECT 100 + a.n * 10 + b.n + 1, N'Bo', 'EU', NULL FROM d a, d b;
CREATE TABLE orders (id INT IDENTITY(1,1) PRIMARY KEY, customer_id INT NOT NULL REFERENCES customers (id), amount DECIMAL(10,2) NOT NULL,
    note VARCHAR(20) NULL CONSTRAINT df_note DEFAULT ('none'), doubled AS amount * 2, CONSTRAINT ck_amount CHECK (amount >= 0));
CREATE INDEX ix_orders_customer ON orders (customer_id);
INSERT orders (customer_id, amount) SELECT (a.n * 1000 + b.n * 100 + e.n * 10 + f.n) % 200 + 1, (a.n * 7 + b.n * 3 + e.n + f.n) * 1.25
    FROM d a, d b, d e, d f;
INSERT orders (customer_id, amount, note) SELECT TOP (50) id, 2.5, 'late' FROM customers ORDER BY id;
DROP TABLE d;
GO
CREATE VIEW v_totals AS SELECT customer_id, COUNT(*) AS n, SUM(amount) AS total FROM orders GROUP BY customer_id;
GO
CREATE PROCEDURE p_count AS SELECT COUNT(*) FROM orders;
GO
{}
BACKUP DATABASE shop TO DISK = '{}';",
        extra, path
    );
    for r in c.execute_script(&script) {
        ok(&r, "the source database");
    }
    c
}

/// The rows of `table` on each shard.
fn shard_counts(h: &Harness, table: &str) -> Vec<i64> {
    h.shards.iter().map(|s| count(&mut s.server.connect(), &format!("SELECT COUNT(*) FROM {}", table))).collect()
}

const SAME: [&str; 8] = [
    "SELECT COUNT(*), SUM(amount), SUM(doubled), MIN(id), MAX(id) FROM orders",
    "SELECT note, COUNT(*) FROM orders GROUP BY note ORDER BY note",
    "SELECT c.region, COUNT(*) AS n, SUM(o.amount) AS total FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region",
    "SELECT r.name, COUNT(*) FROM customers c JOIN regions r ON r.code = c.region GROUP BY r.name ORDER BY r.name",
    "SELECT TOP (5) customer_id, n, total FROM v_totals ORDER BY total DESC, customer_id",
    // the column's case-sensitive collation came along
    "SELECT COUNT(*) FROM customers WHERE name = N'Ann'",
    "SELECT id, amount, note, doubled FROM orders WHERE customer_id = 17 ORDER BY id",
    "SELECT * FROM regions ORDER BY code",
];

#[test]
fn a_backup_is_restored_into_the_shards_and_answers_as_the_source_does() {
    let h = start(3);
    let path = backup_path("full");
    let src_server = Server::new();
    let mut src = source(&src_server, &path, "");
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}' WITH DISTRIBUTION = N'dbo.customers BY id'", path));
    ok(&r, "the restore");
    let said = messages(&r).join("\n");
    assert!(said.contains("10254 row(s) in 3 table(s)") && said.contains("2 sharded and 1 replicated"), "{}", said);

    // the rows are on the shards, spread; the replicated table is whole on each; the control node holds none
    let orders = shard_counts(&h, "orders");
    assert_eq!(orders.iter().sum::<i64>(), 10_050);
    assert!(orders.iter().all(|&n| n > 2_000 && n < 5_000), "orders spread as {:?}", orders);
    assert_eq!(shard_counts(&h, "customers").iter().sum::<i64>(), 200);
    assert_eq!(shard_counts(&h, "regions"), vec![4, 4, 4]);
    let mut mirror = h.cluster.mirror_server().connect_internal();
    assert_eq!(count(&mut mirror, "SELECT COUNT(*) FROM orders"), 0, "the control node's engine holds the schema only");
    assert!(!matches!(mirror.query("SELECT OBJECT_ID('dbo.p_count')").unwrap()[0][0], Value::Null), "the procedure is there");

    // the same answers as the database the backup was taken of
    for q in SAME {
        let a = rows_of(&s.execute(q));
        let b = rows_of(&src.execute(q));
        assert_eq!(a, b, "{}", q);
    }
    // the identity counter goes on from where the source's was; the check, the foreign key and the default hold
    let add = "INSERT orders (customer_id, amount) VALUES (3, 1.00)";
    ok(&s.execute(add), "an insert into the cluster");
    ok(&src.execute(add), "an insert into the source");
    let q = "SELECT MAX(id), COUNT(*) FROM orders WHERE note = 'none'";
    assert_eq!(rows_of(&s.execute(q)), rows_of(&src.execute(q)));
    assert!(s.execute("INSERT orders (customer_id, amount) VALUES (3, -1)").first_error().is_some(), "the check constraint");
    assert!(s.execute("INSERT orders (customer_id, amount) VALUES (9999, 1)").first_error().is_some(), "the foreign key");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn what_a_cluster_cannot_hold_is_refused_unless_the_rest_is_wanted() {
    let h = start(2);
    let path = backup_path("refused");
    let src_server = Server::new();
    let _src = source(&src_server, &path, "GO\nCREATE FUNCTION dbo.twice (@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END\nGO");
    let mut s = h.cluster.session();
    let restore = format!("RESTORE DATABASE shop FROM DISK = '{}'", path);
    // a stored function: refused, and nothing is made
    let e = s.execute(&restore).first_error().cloned().expect("refused");
    assert_eq!(e.number, 50100);
    assert!(e.message.contains("dbo.twice") && e.message.contains("CONTINUE_AFTER_ERROR"), "{}", e.message);
    assert_eq!(count(&mut h.cluster.mirror_server().connect_internal(), "SELECT COUNT(*) FROM sys.tables"), 0);
    // the rest, saying what was left out
    let r = s.execute(&format!("{} WITH CONTINUE_AFTER_ERROR", restore));
    ok(&r, "the restore without the function");
    assert!(messages(&r).iter().any(|m| m.starts_with("Not restored: the function dbo.twice")), "{:?}", messages(&r));
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM orders")), vec![vec!["10050".to_string()]]);
    // the names are taken now: REPLACE drops the cluster's objects first, and the rows are not doubled
    assert_eq!(s.execute(&format!("{} WITH CONTINUE_AFTER_ERROR", restore)).first_error().map(|e| e.number), Some(3159));
    ok(&s.execute(&format!("{} WITH CONTINUE_AFTER_ERROR, REPLACE", restore)), "the restore WITH REPLACE");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM orders")), vec![vec!["10050".to_string()]]);
    assert_eq!(shard_counts(&h, "regions"), vec![4, 4]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_unique_key_without_the_shard_key_and_a_computed_column_that_calls_a_function_are_left_out_with_a_note() {
    let h = start(2);
    let path = backup_path("weak");
    let src_server = Server::new();
    let extra = "GO
CREATE FUNCTION dbo.acct (@x INT) RETURNS NVARCHAR(12) AS BEGIN RETURN N'AW' + CAST(@x AS NVARCHAR(10)) END
GO
        CREATE TABLE people (id INT PRIMARY KEY, email VARCHAR(40) NOT NULL, acct AS dbo.acct(id));
        INSERT people (id, email) VALUES (1, 'a@x'), (2, 'b@x'), (3, 'c@x'), (4, 'd@x');
        CREATE UNIQUE INDEX ux_email ON people (email);
        CREATE INDEX ix_acct ON people (acct);
GO";
    let _src = source(&src_server, &path, extra);
    let mut s = h.cluster.session();
    let restore = format!("RESTORE DATABASE shop FROM DISK = '{}' WITH DISTRIBUTION = N'dbo.people BY id'", path);
    // the unique index does not contain the shard key: refused with the rest, and nothing is made
    let e = s.execute(&restore).first_error().cloned().expect("refused");
    assert_eq!(e.number, 50100);
    assert_eq!(count(&mut h.cluster.mirror_server().connect_internal(), "SELECT COUNT(*) FROM sys.tables"), 0);
    let r = s.execute(&format!("{}, CONTINUE_AFTER_ERROR", restore));
    ok(&r, "the restore");
    let said = messages(&r).join("
");
    assert!(said.contains("the uniqueness of the unique index ux_email of [dbo].[people]"), "{}", said);
    assert!(said.contains("the computed column [acct] of [dbo].[people]"), "{}", said);
    assert!(said.contains("the index ix_acct of [dbo].[people]"), "{}", said);
    // the rows are there, spread, without the computed column; the index on the e-mail is an ordinary one
    assert_eq!(rows_of(&s.execute("SELECT id, email FROM people ORDER BY id")).len(), 4);
    assert_eq!(shard_counts(&h, "people").iter().sum::<i64>(), 4);
    assert!(s.execute("SELECT acct FROM people").first_error().is_some(), "the computed column is not there");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_restored_schema_and_its_tables_are_still_there_when_the_control_node_starts_again() {
    let h = start(2);
    let path = backup_path("again");
    let state = std::env::temp_dir().join(format!("tsql-restore-test-{}-again.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let src_server = Server::new();
    let _src = source(&src_server, &path, "GO
CREATE SCHEMA hr;
GO
CREATE TABLE hr.staff (id INT PRIMARY KEY, name VARCHAR(20));
INSERT hr.staff VALUES (1, 'a'), (2, 'b');
GO");
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    {
        let cluster = Cluster::open(&state, &cfg).unwrap();
        ok(&cluster.session().execute(&format!("RESTORE DATABASE shop FROM DISK = '{}'", path)), "the restore");
    }
    // a control node started again from the state file knows the schema, the table made in it and the rows on the shards
    let cluster = Cluster::open(&state, &cfg).unwrap();
    let mut s = cluster.session();
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM sys.tables WHERE name = 'staff'")), vec![vec!["1".to_string()]]);
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM hr.staff")), vec![vec!["2".to_string()]]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&state);
}

#[test]
fn options_a_cluster_cannot_honour_are_errors() {
    let h = start(2);
    let path = backup_path("options");
    let src_server = Server::new();
    let _src = source(&src_server, &path, "");
    let mut s = h.cluster.session();
    let restore = format!("RESTORE DATABASE shop FROM DISK = '{}'", path);
    let number = |s: &mut ClusterSession, sql: String| s.execute(&sql).first_error().map(|e| e.number);
    assert_eq!(number(&mut s, format!("{} WITH NORECOVERY", restore)), Some(50100));
    assert_eq!(number(&mut s, format!("{} WITH DISTRIBUTION = N'dbo.nothing REPLICATED'", restore)), Some(208));
    assert_eq!(number(&mut s, format!("{} WITH DISTRIBUTION = N'orders BY'", restore)), Some(102));
    // a shard key outside the primary key could not keep it unique
    assert_eq!(number(&mut s, format!("{} WITH DISTRIBUTION = N'orders BY customer_id'", restore)), Some(50100));
    assert_eq!(number(&mut s, format!("BEGIN TRAN; {}", restore)), Some(574));
    let _ = s.execute("ROLLBACK");
    // nothing was made by any of them
    assert_eq!(count(&mut h.cluster.mirror_server().connect_internal(), "SELECT COUNT(*) FROM sys.tables"), 0);
    // a backup of the cluster's database would hold no rows: refused
    assert_eq!(number(&mut s, format!("BACKUP DATABASE master TO DISK = '{}.x'", path)), Some(50100));
    let _ = std::fs::remove_file(&path);
}
