//! UPDATE ... FROM and DELETE ... FROM over the cluster: the join is read by the control node and applied to the target by key.
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
    ok(&s.execute(sql), sql);
}

fn rows(s: &mut ClusterSession, sql: &str) -> Vec<Vec<String>> {
    let r = s.execute(sql);
    ok(&r, sql);
    r.resultsets().last().map(|rs| rs.rows.iter().map(|row| row.iter().map(display).collect()).collect()).unwrap_or_default()
}

fn one(s: &mut ClusterSession, sql: &str) -> String {
    rows(s, sql).first().and_then(|r| r.first()).cloned().unwrap_or_default()
}

fn setup(s: &mut ClusterSession) {
    run(s, "CREATE TABLE tgt (id INT PRIMARY KEY, k INT, n INT, note VARCHAR(10) NULL)");
    run(s, "CREATE TABLE src (k INT PRIMARY KEY, v INT, label VARCHAR(10))");
    run(s, "INSERT tgt VALUES (1,10,0,NULL),(2,20,0,NULL),(3,30,0,NULL),(4,40,0,NULL),(5,10,0,NULL),(6,99,0,NULL)");
    run(s, "INSERT src VALUES (10,100,'ten'),(20,200,'twenty'),(30,300,'thirty'),(50,500,'fifty')");
}

#[test]
fn an_update_takes_values_from_rows_it_joins() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "UPDATE tgt SET n = src.v, note = src.label FROM src WHERE tgt.k = src.k");
    assert_eq!(
        rows(&mut s, "SELECT id, n, note FROM tgt ORDER BY id"),
        vec![
            vec!["1".to_string(), "100".into(), "ten".into()],
            vec!["2".into(), "200".into(), "twenty".into()],
            vec!["3".into(), "300".into(), "thirty".into()],
            vec!["4".into(), "0".into(), "NULL".into()],
            vec!["5".into(), "100".into(), "ten".into()],
            vec!["6".into(), "0".into(), "NULL".into()],
        ]
    );
}

#[test]
fn the_target_may_be_an_alias_and_the_set_may_add_to_the_column() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "UPDATE t SET n += s.v + 1 FROM tgt t JOIN src s ON t.k = s.k WHERE s.v > 100");
    assert_eq!(rows(&mut s, "SELECT id, n FROM tgt ORDER BY id").concat(), vec!["1", "0", "2", "201", "3", "301", "4", "0", "5", "0", "6", "0"]);
}

#[test]
fn a_delete_removes_the_rows_that_join() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "DELETE tgt FROM tgt JOIN src ON tgt.k = src.k WHERE src.v < 250");
    assert_eq!(rows(&mut s, "SELECT id FROM tgt ORDER BY id").concat(), vec!["3", "4", "6"]);
    run(&mut s, "DELETE FROM tgt FROM src WHERE tgt.k = src.k");
    assert_eq!(rows(&mut s, "SELECT id FROM tgt ORDER BY id").concat(), vec!["4", "6"]);
}

#[test]
fn nothing_matching_changes_nothing_and_a_table_without_a_key_is_changed_by_its_columns() {
    let h = start(3);
    let mut s = h.cluster.session();
    setup(&mut s);
    run(&mut s, "UPDATE tgt SET n = 7 FROM src WHERE tgt.k = src.k AND src.v > 10000");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM tgt WHERE n = 7"), "0");
    run(&mut s, "CREATE TABLE nokey (a INT, b INT)");
    run(&mut s, "INSERT nokey VALUES (1, 1), (2, 2), (3, 3)");
    run(&mut s, "UPDATE nokey SET b = src.v FROM src WHERE nokey.a * 10 = src.k");
    assert_eq!(rows(&mut s, "SELECT a, b FROM nokey ORDER BY a").concat(), vec!["1", "100", "2", "200", "3", "300"]);
}

#[test]
fn reporting_services_upgrade_statement_runs() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE ChunkSegmentMapping (ChunkId UNIQUEIDENTIFIER NOT NULL, SegmentId UNIQUEIDENTIFIER NOT NULL, LogicalByteCount INT NULL, ActualByteCount INT NULL)");
    run(&mut s, "CREATE TABLE Segment (SegmentId UNIQUEIDENTIFIER NOT NULL PRIMARY KEY, LogicalByteCount INT NULL, ActualByteCount INT NULL)");
    run(&mut s, "DECLARE @a UNIQUEIDENTIFIER = NEWID(), @b UNIQUEIDENTIFIER = NEWID() INSERT Segment VALUES (@a, 5, 6), (@b, 7, 8) INSERT ChunkSegmentMapping VALUES (NEWID(), @a, NULL, NULL), (NEWID(), @b, NULL, NULL), (NEWID(), NEWID(), NULL, NULL)");
    run(&mut s, "if exists (select 1 from sys.columns c where c.object_id = object_id('Segment') and c.name = 'LogicalByteCount')\r\nbegin     \r\n    exec sp_executesql N'update ChunkSegmentMapping\r\n    set LogicalByteCount = S.LogicalByteCount,\r\n\t    ActualByteCount = S.ActualByteCount\r\n    from Segment S\r\n    where ChunkSegmentMapping.SegmentId = S.SegmentId'\r\nend");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM ChunkSegmentMapping WHERE LogicalByteCount IS NOT NULL"), "2");
    assert_eq!(one(&mut s, "SELECT SUM(ActualByteCount) FROM ChunkSegmentMapping"), "14");
}

#[test]
fn a_correlated_subquery_in_an_update_or_delete_is_read_as_a_query_and_applied_by_key() {
    let h = start(3);
    let mut s = h.cluster.session();
    // (Reporting Services' CleanOrphanedPolicies)
    run(&mut s, "CREATE TABLE Policies (PolicyID INT PRIMARY KEY, PolicyFlag INT NOT NULL)");
    run(&mut s, "CREATE TABLE Cat (ItemID INT PRIMARY KEY, PolicyID INT NULL)");
    run(&mut s, "INSERT Policies VALUES (1,0),(2,0),(3,0),(4,1),(5,0)");
    run(&mut s, "INSERT Cat VALUES (10,1),(11,3)");
    run(&mut s, "DELETE Policies WHERE Policies.PolicyFlag = 0 AND NOT EXISTS (SELECT ItemID FROM Cat WHERE Cat.PolicyID = Policies.PolicyID)");
    assert_eq!(rows(&mut s, "SELECT PolicyID FROM Policies ORDER BY PolicyID").concat(), vec!["1", "3", "4"]);
    run(&mut s, "UPDATE Policies SET PolicyFlag = (SELECT COUNT(*) FROM Cat WHERE Cat.PolicyID = Policies.PolicyID) WHERE EXISTS (SELECT 1 FROM Cat WHERE Cat.PolicyID = Policies.PolicyID)");
    assert_eq!(rows(&mut s, "SELECT PolicyID, PolicyFlag FROM Policies ORDER BY PolicyID").concat(), vec!["1", "1", "3", "1", "4", "1"]);
}

#[test]
fn a_unique_column_may_be_added_to_an_empty_table_and_a_refused_statement_does_not_spoil_the_transaction() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE Datasource (id INT NOT NULL)");
    run(&mut s, "CREATE TABLE hist (v VARCHAR(10))");
    // (Reporting Services' upgrade: inside a transaction, a unique identity column)
    run(&mut s, "BEGIN TRAN; ALTER TABLE Datasource ADD DSIDNum BIGINT IDENTITY(1,1) UNIQUE; INSERT hist VALUES ('236'); COMMIT");
    run(&mut s, "INSERT Datasource (id) VALUES (1), (2), (3)");
    assert_eq!(one(&mut s, "SELECT MAX(DSIDNum) FROM Datasource"), "3");
    // a table that has rows cannot take a unique column; the statement fails and the transaction goes on
    run(&mut s, "BEGIN TRAN");
    let r = s.execute("ALTER TABLE Datasource ADD another INT UNIQUE");
    assert_eq!(r.first_error().map(|e| e.number), Some(50100));
    run(&mut s, "INSERT hist VALUES ('still')");
    run(&mut s, "COMMIT");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM hist"), "2");
}

#[test]
fn a_table_whose_first_column_cannot_be_a_shard_key_is_sharded_on_another_or_replicated() {
    let h = start(3);
    let mut s = h.cluster.session();
    // (Reporting Services' ProductInfoHistory: no primary key, a datetime first)
    run(&mut s, "CREATE TABLE [dbo].[ProductInfoHistory] ([DateTime] datetime default Current_Timestamp, [DbSchemaHash] varchar(128) NOT NULL, [Sku] varchar(25) NOT NULL)");
    run(&mut s, "ALTER TABLE [dbo].[ProductInfoHistory] ADD CONSTRAINT IX_ProductInfoHistory_DateTime UNIQUE(DateTime)");
    run(&mut s, "INSERT ProductInfoHistory (DbSchemaHash, Sku) VALUES ('a', 'x')");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM ProductInfoHistory"), "1");
    // a table of columns the cluster cannot shard on at all is replicated
    run(&mut s, "CREATE TABLE stamps (t datetime, flag bit)");
    run(&mut s, "INSERT stamps VALUES (GETDATE(), 1), (GETDATE(), 0)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM stamps"), "2");
    // a key that is asked for is still refused when it cannot be one
    assert!(s.execute("DISTRIBUTE TABLE stamps BY t").first_error().is_some());
}

#[test]
fn an_insert_select_may_start_with_common_table_expressions() {
    let h = start(3);
    let mut s = h.cluster.session();
    // (Reporting Services' UpdatePBIDefinition)
    run(&mut s, "CREATE TABLE ext (ItemId INT NOT NULL, ContentType VARCHAR(30) NOT NULL, Content INT NULL, PRIMARY KEY (ItemId, ContentType))");
    run(&mut s, "CREATE TABLE ds (ItemId INT PRIMARY KEY)");
    run(&mut s, "INSERT ds VALUES (1), (2), (3)");
    run(&mut s, "INSERT ext VALUES (1, 'CatalogItem', 10), (2, 'CatalogItem', 20), (2, 'Def', 99), (3, 'CatalogItem', 30)");
    run(&mut s, "WITH Defined AS (SELECT ItemId FROM ext WHERE ContentType = 'Def'), Missing AS (SELECT d.ItemId FROM ds d LEFT JOIN Defined ON d.ItemId = Defined.ItemId WHERE Defined.ItemId IS NULL) INSERT INTO ext (ItemId, ContentType, Content) SELECT ItemId, 'Def', Content FROM ext WHERE ItemId IN (SELECT ItemId FROM Missing) AND ContentType = 'CatalogItem'");
    assert_eq!(rows(&mut s, "SELECT ItemId, Content FROM ext WHERE ContentType = 'Def' ORDER BY ItemId").concat(), vec!["1", "10", "2", "99", "3", "30"]);
}
