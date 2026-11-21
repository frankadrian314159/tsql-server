//! Schema changes inside a transaction: they commit what the transaction did before them, are made, and the transaction goes on.
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

fn one(s: &mut ClusterSession, sql: &str) -> String {
    let r = s.execute(sql);
    ok(&r, sql);
    r.resultsets().last().and_then(|rs| rs.rows.first()).and_then(|row| row.first()).map(display).unwrap_or_default()
}

fn error(s: &mut ClusterSession, sql: &str) -> i32 {
    s.execute(sql).first_error().map(|e| e.number).unwrap_or(0)
}

#[test]
fn a_transaction_may_change_the_schema_and_the_data_and_commit() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "BEGIN TRAN");
    run(&mut s, "CREATE TABLE a (id INT PRIMARY KEY, x INT NULL)");
    run(&mut s, "ALTER TABLE a ADD y INT NULL");
    run(&mut s, "CREATE INDEX ix_x ON a (x)");
    run(&mut s, "INSERT a (id, x, y) VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3), (4, 4, 4)");
    run(&mut s, "COMMIT");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM a"), "4");
    assert_eq!(one(&mut s, "SELECT SUM(y) FROM a"), "10");
}

#[test]
fn a_rollback_undoes_the_data_but_not_the_schema_change() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "BEGIN TRAN");
    run(&mut s, "CREATE TABLE b (id INT PRIMARY KEY)");
    run(&mut s, "INSERT b VALUES (1), (2), (3)");
    run(&mut s, "ROLLBACK");
    // (the table is there, as the change was made at once; the rows the transaction wrote are not)
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM b"), "0");
}

#[test]
fn a_migration_script_that_tests_and_alters_inside_a_transaction_works() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE seg (id INT NOT NULL, content VARBINARY(100) NULL)");
    run(&mut s, "INSERT seg VALUES (1, 0x01), (2, 0x0102)");
    // (Reporting Services' upgrade: `begin tran`, then the changes a version needs, each guarded by a look at the catalog)
    run(&mut s, "begin tran");
    run(&mut s, "if not exists (select * from sys.columns c where c.object_id = object_id('seg') and c.name = 'n') begin alter table seg add n int null end");
    run(&mut s, "alter table seg with nocheck add constraint [PK_seg] primary key nonclustered (id)");
    run(&mut s, "create unique nonclustered index [UQ_seg] on [dbo].[seg] ([id], [n])");
    run(&mut s, "commit tran");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.columns WHERE object_id = OBJECT_ID('seg') AND name = 'n'"), "1");
    assert_eq!(error(&mut s, "INSERT seg VALUES (1, 0x03, 1)"), 2627);
}

#[test]
fn truncate_is_still_refused_in_a_transaction() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE c (id INT PRIMARY KEY)");
    run(&mut s, "BEGIN TRAN");
    assert_eq!(error(&mut s, "TRUNCATE TABLE c"), 50100);
    run(&mut s, "ROLLBACK");
}

#[test]
fn an_identity_column_may_be_added_to_an_empty_table_and_is_numbered_by_the_control_node() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE seg (id UNIQUEIDENTIFIER NOT NULL, v INT NULL)");
    // (Reporting Services' upgrade: `add SegmentedChunkId bigint identity(1,1) not null` to a table that has no rows yet)
    run(&mut s, "ALTER TABLE seg ADD sid BIGINT IDENTITY(1,1) NOT NULL");
    assert_eq!(error(&mut s, "ALTER TABLE seg ADD sid2 INT IDENTITY(1,1) NOT NULL"), 2744);
    run(&mut s, "INSERT seg (id, v) VALUES (NEWID(), 1), (NEWID(), 2), (NEWID(), 3), (NEWID(), 4)");
    assert_eq!(one(&mut s, "SELECT MIN(sid) FROM seg"), "1");
    assert_eq!(one(&mut s, "SELECT MAX(sid) FROM seg"), "4");
    assert_eq!(one(&mut s, "SELECT COUNT(DISTINCT sid) FROM seg"), "4");
    // a table that has rows would need them numbered: refused
    run(&mut s, "CREATE TABLE full_t (a INT NOT NULL)");
    run(&mut s, "INSERT full_t VALUES (1)");
    assert_eq!(error(&mut s, "ALTER TABLE full_t ADD n INT IDENTITY(1,1) NOT NULL"), 50100);
}

#[test]
fn sp_rename_of_an_index_or_a_column_is_made_on_the_shards_too() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE ren (id INT PRIMARY KEY, a INT, b INT)");
    run(&mut s, "INSERT ren VALUES (1,1,1),(2,2,2),(3,3,3),(4,4,4)");
    run(&mut s, "CREATE INDEX [IX_old] ON ren (a)");
    // (the front end gives the cluster an `sp_rename` of a table of the cluster, so that the shards rename too)
    assert!(s.wants("exec sp_rename 'ren.IX_old', 'IX_new', 'INDEX'"));
    run(&mut s, "exec sp_rename 'ren.IX_old', 'IX_new', 'INDEX'");
    // a later DROP INDEX by the new name reaches the shards, which would not know it if only the control node's engine had renamed it
    run(&mut s, "DROP INDEX [IX_new] ON ren");
    run(&mut s, "EXEC sp_rename 'ren.b', 'bee', 'COLUMN'");
    assert_eq!(one(&mut s, "SELECT SUM(bee) FROM ren"), "10");
    run(&mut s, "UPDATE ren SET bee = 0 WHERE id = 1");
    assert_eq!(one(&mut s, "SELECT SUM(bee) FROM ren"), "9");
    assert_eq!(error(&mut s, "EXEC sp_rename 'ren', 'ren2'"), 0);
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM ren2"), "4");
}

#[test]
fn sp_rename_in_a_database_other_than_master_reaches_the_shards() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE DATABASE other");
    run(&mut s, "USE other");
    run(&mut s, "CREATE TABLE ren (id INT PRIMARY KEY, a INT)");
    run(&mut s, "CREATE INDEX [IX_old] ON ren (a)");
    assert!(s.wants("exec sp_rename 'ren.IX_old', 'IX_new', 'INDEX'"));
    run(&mut s, "if exists (select 1 from sys.indexes where name = 'IX_old')\r\nbegin\r\n exec sp_rename 'ren.IX_old', 'IX_new', 'INDEX'\r\nend");
    run(&mut s, "DROP INDEX [IX_new] ON ren");
}

#[test]
fn a_schema_change_commits_what_came_before_it_and_a_rollback_undoes_only_what_follows() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE w (id INT PRIMARY KEY, v INT NULL)");
    run(&mut s, "BEGIN TRAN");
    run(&mut s, "INSERT w VALUES (1, 1), (2, 2), (3, 3)");
    // (it reads and writes the table first: the change needs the locks to be let go)
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM w"), "3");
    run(&mut s, "ALTER TABLE w ADD extra INT NULL");
    assert!(s.in_transaction(), "the client is still in a transaction");
    run(&mut s, "INSERT w (id, v, extra) VALUES (4, 4, 4)");
    run(&mut s, "ROLLBACK");
    // the rows before the change stay; the one after it is undone
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM w"), "3");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM sys.columns WHERE object_id = OBJECT_ID('w') AND name = 'extra'"), "1");
}

#[test]
fn a_table_of_the_cluster_may_be_renamed_with_its_rows_keys_and_triggers() {
    let h = start(3);
    let mut s = h.cluster.session();
    run(&mut s, "CREATE TABLE ExecutionLog (id INT PRIMARY KEY, v INT NULL)");
    run(&mut s, "INSERT ExecutionLog VALUES (1,1),(2,2),(3,3),(4,4)");
    run(&mut s, "CREATE TABLE audit (n INT IDENTITY(1,1) PRIMARY KEY, what VARCHAR(20))");
    run(&mut s, "CREATE TRIGGER trg_log ON ExecutionLog AFTER INSERT AS INSERT audit (what) SELECT 'ins' FROM inserted");
    // (Reporting Services upgrade: rename the old table, then make a new one of the name)
    assert!(s.wants("exec sp_rename 'dbo.ExecutionLog', 'ExecutionLog_Old'"));
    run(&mut s, "exec sp_rename 'dbo.ExecutionLog', 'ExecutionLog_Old'");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM ExecutionLog_Old"), "4");
    assert_eq!(error(&mut s, "SELECT COUNT(*) FROM ExecutionLog"), 208);
    // the key and the trigger went with it
    assert_eq!(error(&mut s, "INSERT ExecutionLog_Old VALUES (1, 9)"), 2627);
    run(&mut s, "INSERT ExecutionLog_Old VALUES (5, 5)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM audit"), "1");
    run(&mut s, "CREATE TABLE ExecutionLog (id INT PRIMARY KEY, v INT NULL, extra INT NULL)");
    run(&mut s, "INSERT ExecutionLog (id) VALUES (1)");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM ExecutionLog"), "1");
    // a name that is taken is refused
    assert_eq!(error(&mut s, "EXEC sp_rename 'ExecutionLog_Old', 'ExecutionLog'"), 15335);
}
