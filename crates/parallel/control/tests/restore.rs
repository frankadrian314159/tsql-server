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
    h.shards.iter().map(|s| count(&mut s.server.connect(), &format!("SELECT COUNT(*) FROM shop.dbo.{}", table))).collect()
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
    ok(&s.execute("USE shop"), "USE shop");
    let said = messages(&r).join("\n");
    assert!(said.contains("10254 row(s) in 3 table(s)") && said.contains("2 sharded and 1 replicated"), "{}", said);

    // the rows are on the shards, spread; the replicated table is whole on each; the control node holds none
    let orders = shard_counts(&h, "orders");
    assert_eq!(orders.iter().sum::<i64>(), 10_050);
    assert!(orders.iter().all(|&n| n > 2_000 && n < 5_000), "orders spread as {:?}", orders);
    assert_eq!(shard_counts(&h, "customers").iter().sum::<i64>(), 200);
    assert_eq!(shard_counts(&h, "regions"), vec![4, 4, 4]);
    let mut mirror = h.cluster.mirror_server().connect_internal();
    assert_eq!(count(&mut mirror, "SELECT COUNT(*) FROM shop.dbo.orders"), 0, "the control node's engine holds the schema only");
    assert!(!matches!(mirror.query("SELECT OBJECT_ID('shop.dbo.p_count')").unwrap()[0][0], Value::Null), "the procedure is there");

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
    let _src = source(&src_server, &path, "GO\nCREATE TRIGGER trg_regions ON regions AFTER INSERT AS TRUNCATE TABLE regions\nGO");
    let mut s = h.cluster.session();
    let restore = format!("RESTORE DATABASE shop FROM DISK = '{}'", path);
    // a trigger the control node cannot run: refused, and nothing is made
    let e = s.execute(&restore).first_error().cloned().expect("refused");
    assert_eq!(e.number, 50100);
    assert!(e.message.contains("trg_regions") && e.message.contains("CONTINUE_AFTER_ERROR"), "{}", e.message);
    assert_eq!(count(&mut h.cluster.mirror_server().connect_internal(), "SELECT COUNT(*) FROM sys.tables"), 0);
    // the rest, saying what was left out
    let r = s.execute(&format!("{} WITH CONTINUE_AFTER_ERROR", restore));
    ok(&r, "the restore without the trigger");
    ok(&s.execute("USE shop"), "USE shop");
    assert!(messages(&r).iter().any(|m| m.starts_with("Not restored: the trigger trg_regions")), "{:?}", messages(&r));
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM orders")), vec![vec!["10050".to_string()]]);
    // the names are taken now: REPLACE drops the cluster's objects first, and the rows are not doubled
    assert_eq!(s.execute(&format!("{} WITH CONTINUE_AFTER_ERROR", restore)).first_error().map(|e| e.number), Some(3159));
    ok(&s.execute(&format!("{} WITH CONTINUE_AFTER_ERROR, REPLACE", restore)), "the restore WITH REPLACE");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM orders")), vec![vec!["10050".to_string()]]);
    assert_eq!(shard_counts(&h, "regions"), vec![4, 4]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_computed_column_that_calls_a_function_that_reads_tables_is_left_out_with_a_note_and_a_unique_key_without_the_shard_key_is_kept() {
    let h = start(2);
    let path = backup_path("weak");
    let src_server = Server::new();
    let extra = "GO
CREATE FUNCTION dbo.acct (@x INT) RETURNS NVARCHAR(12) AS BEGIN RETURN N'AW' + CAST(@x AS NVARCHAR(10)) END
GO
CREATE FUNCTION dbo.nregions () RETURNS INT AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.regions) END
GO
        CREATE TABLE people (id INT PRIMARY KEY, email VARCHAR(40) NOT NULL, acct AS dbo.acct(id), nreg AS dbo.nregions());
        INSERT people (id, email) VALUES (1, 'a@x'), (2, 'b@x'), (3, 'c@x'), (4, 'd@x');
        CREATE UNIQUE INDEX ux_email ON people (email);
        CREATE INDEX ix_acct ON people (acct);
        CREATE INDEX ix_nreg ON people (nreg);
GO";
    let _src = source(&src_server, &path, extra);
    let mut s = h.cluster.session();
    let restore = format!("RESTORE DATABASE shop FROM DISK = '{}' WITH DISTRIBUTION = N'dbo.people BY id'", path);
    let r = s.execute(&restore);
    ok(&r, "the restore");
    ok(&s.execute("USE shop"), "USE shop");
    let said = messages(&r).join("
");
    assert!(!said.contains("ux_email"), "the unique index is kept, with no note: {}", said);
    // (the function that reads tables is on the control node only, so the column that calls it, and its index, are left out)
    assert!(said.contains("the computed column [nreg] of [dbo].[people]"), "{}", said);
    assert!(said.contains("the index ix_nreg of [dbo].[people]"), "{}", said);
    // (the one that reads none is on the shards, so its column and index are kept)
    assert!(!said.contains("the computed column [acct]"), "{}", said);
    // the rows are there, spread; the index on the e-mail is unique across the shards (it does not contain the shard key)
    assert_eq!(rows_of(&s.execute("SELECT id, email FROM people ORDER BY id")).len(), 4);
    assert_eq!(s.execute("INSERT people (id, email) VALUES (9, 'c@x')").first_error().map(|e| e.number), Some(2601));
    assert_eq!(shard_counts(&h, "people").iter().sum::<i64>(), 4);
    assert_eq!(rows_of(&s.execute("SELECT acct FROM people WHERE id = 3")), vec![vec!["AW3".to_string()]]);
    assert!(s.execute("SELECT nreg FROM people").first_error().is_some(), "the column that calls a function that reads tables is not there");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stored_functions_are_restored_the_ones_that_read_no_table_to_the_shards_too() {
    let h = start(2);
    let path = backup_path("funcs");
    let src_server = Server::new();
    let extra = "GO
CREATE FUNCTION dbo.twice (@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END
GO
CREATE FUNCTION dbo.order_count () RETURNS INT AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.orders) END
GO
CREATE FUNCTION dbo.big_orders (@min DECIMAL(10,2)) RETURNS TABLE AS RETURN (SELECT id, amount FROM dbo.orders WHERE amount >= @min)
GO
CREATE VIEW v_counted AS SELECT id, dbo.order_count() AS total FROM customers
GO";
    let oracle = source(&src_server, &path, extra);
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}'", path));
    ok(&r, "the restore");
    ok(&s.execute("USE shop"), "USE shop");
    let mut o = oracle;
    for q in [
        "SELECT dbo.twice(21)",
        "SELECT SUM(dbo.twice(id)) FROM customers",
        "SELECT dbo.order_count()",
        "SELECT COUNT(*) FROM dbo.big_orders(100)",
        "SELECT MAX(total) FROM v_counted",
        "SELECT TOP (3) c.id, dbo.order_count() FROM customers c ORDER BY c.id",
    ] {
        let want = o.query(q).unwrap().iter().map(|row| row.iter().map(display).collect::<Vec<_>>()).collect::<Vec<_>>();
        let r = s.execute(q);
        let got = rows_of(&r);
        assert_eq!(got, want, "{}: {:?}", q, r.first_error());
    }
    // the shards have the function that reads no table and not the others
    for sh in &h.shards {
        let mut c = sh.server.connect();
        assert!(c.execute("SELECT shop.dbo.twice(1)").first_error().is_none());
        assert!(c.execute("SELECT shop.dbo.order_count()").first_error().is_some());
    }
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
    ok(&s.execute("USE shop"), "USE shop");
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
    assert_eq!(number(&mut s, format!("BEGIN TRAN; {}", restore)), Some(574));
    let _ = s.execute("ROLLBACK");
    // nothing was made by any of them
    assert_eq!(count(&mut h.cluster.mirror_server().connect_internal(), "SELECT COUNT(*) FROM sys.tables"), 0);
    // a backup of the cluster's database would hold no rows: refused
    assert_eq!(number(&mut s, format!("BACKUP DATABASE master TO DISK = '{}.x'", path)), Some(50100));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_table_may_be_sharded_on_a_column_outside_its_primary_key_which_the_control_node_keeps_unique() {
    let h = start(3);
    let path = backup_path("pkout");
    let src_server = Server::new();
    let _src = source(&src_server, &path, "");
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}' WITH DISTRIBUTION = N'dbo.orders BY customer_id'", path));
    ok(&r, "the restore");
    ok(&s.execute("USE shop"), "USE shop");
    let n = rows_of(&s.execute("SELECT COUNT(*) FROM orders"))[0][0].clone();
    // an order number that exists is refused, wherever the new row would go
    ok(&s.execute("SET IDENTITY_INSERT orders ON"), "identity insert");
    for c in [1, 2, 3, 4, 5, 6] {
        let e = s.execute(&format!("INSERT orders (id, customer_id, amount) VALUES (5, {}, 1)", c)).first_error().map(|e| e.number);
        assert_eq!(e, Some(2627), "customer {}", c);
    }
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM orders"))[0][0], n);
    ok(&s.execute("SET IDENTITY_INSERT orders OFF"), "identity insert off");
    ok(&s.execute("INSERT orders (customer_id, amount) VALUES (3, 1)"), "an insert that is numbered by the control node");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn user_defined_types_are_restored_and_their_columns_keep_them() {
    let h = start(2);
    let path = backup_path("udt");
    let src_server = Server::new();
    let extra = "GO
CREATE SCHEMA hr;
GO
CREATE TYPE dbo.Phone FROM VARCHAR(20) NOT NULL;
CREATE TYPE hr.Badge FROM INT NULL;
CREATE TYPE dbo.IdList AS TABLE (id INT);
GO
CREATE TABLE hr.staff (id INT PRIMARY KEY, phone dbo.Phone, badge hr.Badge);
INSERT hr.staff VALUES (1, '555-1', 10), (2, '555-2', NULL), (3, '555-3', 30);
GO
CREATE PROCEDURE dbo.pick @ids dbo.IdList READONLY AS SELECT phone FROM hr.staff WHERE id IN (SELECT id FROM @ids) ORDER BY phone;
GO";
    let _src = source(&src_server, &path, extra);
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}'", path));
    ok(&r, "the restore");
    assert!(!messages(&r).join(" ").contains("user-defined type"), "no note about types: {:?}", messages(&r));
    ok(&s.execute("USE shop"), "USE shop");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM sys.types WHERE name IN ('Phone', 'Badge', 'IdList') AND is_user_defined = 1")), vec![vec!["3".to_string()]]);
    assert_eq!(rows_of(&s.execute("SELECT t.name FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id WHERE c.object_id = OBJECT_ID('hr.staff') AND c.name = 'phone'")), vec![vec!["Phone".to_string()]]);
    // the type's rule is the column's
    assert_eq!(s.execute("INSERT hr.staff VALUES (4, NULL, 1)").first_error().map(|e| e.number), Some(515));
    let r = s.execute("DECLARE @l dbo.IdList; INSERT @l VALUES (1), (3); EXEC dbo.pick @l");
    ok(&r, "the procedure with a table type");
    assert_eq!(rows_of(&r), vec![vec!["555-1".to_string()], vec!["555-3".to_string()]]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn xml_and_spatial_indexes_are_restored_the_primary_one_first() {
    let h = start(2);
    let path = backup_path("declidx");
    let src_server = Server::new();
    let extra = "GO
CREATE TABLE docs (id INT PRIMARY KEY, body XML);
INSERT docs VALUES (1, '<a><b>x</b></a>'), (2, '<a/>');
CREATE XML INDEX xp_body ON docs (body) USING XML INDEX pxml_body FOR PATH;
GO
CREATE TABLE places (id INT PRIMARY KEY, g GEOMETRY);
CREATE SPATIAL INDEX sx ON places (g) USING GEOMETRY_GRID WITH (BOUNDING_BOX = (0, 0, 10, 10));
GO";
    // (the primary index is made first by the source, so that the secondary one can be built on it)
    let extra = extra.replace("CREATE XML INDEX xp_body", "CREATE PRIMARY XML INDEX pxml_body ON docs (body);\nCREATE XML INDEX xp_body");
    let _src = source(&src_server, &path, &extra);
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}'", path));
    ok(&r, "the restore");
    let said = messages(&r).join(" ");
    assert!(!said.contains("XML index") && !said.contains("spatial index"), "no note about them: {}", said);
    ok(&s.execute("USE shop"), "USE shop");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM sys.xml_indexes")), vec![vec!["2".to_string()]]);
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM sys.spatial_indexes")), vec![vec!["1".to_string()]]);
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM docs")), vec![vec!["2".to_string()]]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn full_text_catalogs_and_indexes_are_restored_and_searches_work() {
    let h = start(2);
    let path = backup_path("ft");
    let src_server = Server::new();
    let extra = "GO
CREATE TABLE notes (id INT NOT NULL, code VARCHAR(10) NOT NULL, body NVARCHAR(100) NULL, CONSTRAINT pk_notes PRIMARY KEY (id), CONSTRAINT uq_code UNIQUE (code));
INSERT notes VALUES (1, 'a', 'the quick fox'), (2, 'b', 'a lazy dog'), (3, 'c', 'fox and dog'), (4, 'd', 'nothing');
GO
CREATE FULLTEXT CATALOG ftc AS DEFAULT;
GO
CREATE FULLTEXT INDEX ON notes (body) KEY INDEX uq_code ON ftc WITH (CHANGE_TRACKING = MANUAL);
GO";
    let _src = source(&src_server, &path, extra);
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}' WITH DISTRIBUTION = N'dbo.notes BY id'", path));
    ok(&r, "the restore");
    assert!(!messages(&r).join(" ").contains("full-text"), "no note about them: {:?}", messages(&r));
    ok(&s.execute("USE shop"), "USE shop");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM sys.fulltext_catalogs")), vec![vec!["1".to_string()]]);
    assert_eq!(rows_of(&s.execute("SELECT change_tracking_state_desc FROM sys.fulltext_indexes")), vec![vec!["MANUAL".to_string()]]);
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM notes WHERE CONTAINS(body, 'fox')")), vec![vec!["2".to_string()]]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn ddl_triggers_of_a_backup_are_restored_last_and_do_not_run_for_what_the_restore_makes() {
    let h = start(2);
    let path = backup_path("ddltrig");
    let src_server = Server::new();
    let extra = "GO
CREATE TABLE ddl_log (n INT IDENTITY(1,1) PRIMARY KEY, what VARCHAR(40));
GO
CREATE TRIGGER no_drops ON DATABASE FOR DROP_TABLE AS BEGIN ROLLBACK; THROW 50001, 'no drops', 1; END;
GO
CREATE TRIGGER log_creates ON DATABASE FOR CREATE_TABLE AS INSERT ddl_log (what) VALUES ('created');
GO
DISABLE TRIGGER log_creates ON DATABASE;
GO";
    let _src = source(&src_server, &path, extra);
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}'", path));
    ok(&r, "the restore");
    assert!(!messages(&r).join(" ").contains("DDL trigger"), "no note about them: {:?}", messages(&r));
    ok(&s.execute("USE shop"), "USE shop");
    // the one that refuses drops is there and runs
    assert_eq!(s.execute("DROP TABLE regions").first_error().map(|e| e.number), Some(50001));
    // the one that was disabled in the backup is disabled
    ok(&s.execute("CREATE TABLE fresh (id INT PRIMARY KEY)"), "create");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM ddl_log")), vec![vec!["0".to_string()]]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_triggers_of_a_backup_are_restored_and_fire_in_the_cluster() {
    let h = start(2);
    let path = backup_path("triggers");
    let src_server = Server::new();
    let extra = "GO
CREATE TABLE audit_log (id INT IDENTITY(1,1) PRIMARY KEY, order_id INT, amount DECIMAL(10,2))
GO
CREATE TRIGGER trg_orders ON orders AFTER INSERT AS INSERT audit_log (order_id, amount) SELECT id, amount FROM inserted
GO
CREATE TRIGGER trg_off ON customers AFTER DELETE AS INSERT audit_log (order_id, amount) SELECT id, 0 FROM deleted
GO
DISABLE TRIGGER trg_off ON customers
GO";
    let _src = source(&src_server, &path, extra);
    let mut s = h.cluster.session();
    let r = s.execute(&format!("RESTORE DATABASE shop FROM DISK = '{}'", path));
    ok(&r, "the restore");
    ok(&s.execute("USE shop"), "USE shop");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM audit_log")), vec![vec!["0".to_string()]], "the rows of the backup fired nothing");
    ok(&s.execute("INSERT orders (customer_id, amount) VALUES (1, 5.50), (2, 7.25)"), "an insert");
    assert_eq!(rows_of(&s.execute("SELECT amount FROM audit_log ORDER BY amount")), vec![vec!["5.50".to_string()], vec!["7.25".to_string()]]);
    // the disabled one stays disabled
    ok(&s.execute("INSERT customers VALUES (5000, N'Zed', 'EU', NULL)"), "a customer with no orders");
    ok(&s.execute("DELETE FROM customers WHERE id = 5000"), "a delete");
    assert_eq!(rows_of(&s.execute("SELECT COUNT(*) FROM audit_log")), vec![vec!["2".to_string()]]);
    let _ = std::fs::remove_file(&path);
}
