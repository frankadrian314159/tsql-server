//! Cursors over the cluster's tables, in a batch and in a trigger: DECLARE, OPEN, FETCH ... INTO, CLOSE, DEALLOCATE and @@FETCH_STATUS.
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

fn error(s: &mut ClusterSession, sql: &str) -> i32 {
    s.execute(sql).first_error().map(|e| e.number).unwrap_or(0)
}

fn items(s: &mut ClusterSession) {
    run(s, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(10), qty INT)");
    run(s, "INSERT items VALUES (1,'a',10),(2,'b',20),(3,'c',30),(4,'d',40),(5,'e',50)");
}

#[test]
fn a_cursor_walks_rows_of_sharded_tables_in_a_loop() {
    let h = start(3);
    let mut s = h.cluster.session();
    items(&mut s);
    let sql = "DECLARE @id INT, @sum INT = 0, @names VARCHAR(100) = ''
        DECLARE c CURSOR FOR SELECT id, name FROM items ORDER BY id
        OPEN c
        FETCH NEXT FROM c INTO @id, @names
        WHILE @@FETCH_STATUS = 0
        BEGIN
            SET @sum = @sum + @id
            FETCH NEXT FROM c INTO @id, @names
        END
        CLOSE c
        DEALLOCATE c
        SELECT @sum, @id, @@FETCH_STATUS";
    // (the last FETCH found no row: the variables keep the last row's values and the status is -1)
    assert_eq!(rows(&mut s, sql), vec![vec!["15".to_string(), "5".into(), "-1".into()]]);
}

#[test]
fn fetch_goes_in_every_direction_and_off_either_end() {
    let h = start(3);
    let mut s = h.cluster.session();
    items(&mut s);
    let decl = "DECLARE @id INT; DECLARE c CURSOR FOR SELECT id FROM items ORDER BY id; OPEN c;";
    let at = |s: &mut ClusterSession, f: &str| one(s, &format!("{} {} SELECT ISNULL(CAST(@id AS VARCHAR(5)), '-') + '/' + CAST(@@FETCH_STATUS AS VARCHAR(3))", decl, f));
    assert_eq!(at(&mut s, "FETCH LAST FROM c INTO @id;"), "5/0");
    assert_eq!(at(&mut s, "FETCH FIRST FROM c INTO @id;"), "1/0");
    assert_eq!(at(&mut s, "FETCH ABSOLUTE 3 FROM c INTO @id;"), "3/0");
    assert_eq!(at(&mut s, "FETCH ABSOLUTE -1 FROM c INTO @id;"), "5/0");
    assert_eq!(at(&mut s, "FETCH ABSOLUTE 9 FROM c INTO @id;"), "-/-1");
    assert_eq!(at(&mut s, "FETCH NEXT FROM c INTO @id; FETCH NEXT FROM c INTO @id; FETCH PRIOR FROM c INTO @id;"), "1/0");
    assert_eq!(at(&mut s, "FETCH ABSOLUTE 2 FROM c INTO @id; FETCH RELATIVE 2 FROM c INTO @id;"), "4/0");
    assert_eq!(at(&mut s, "FETCH PRIOR FROM c INTO @id;"), "-/-1");
}

#[test]
fn a_cursor_is_a_snapshot_taken_when_it_is_opened_and_uses_the_variables_of_that_moment() {
    let h = start(3);
    let mut s = h.cluster.session();
    items(&mut s);
    let sql = "DECLARE @min INT = 3, @id INT, @n INT = 0
        DECLARE c CURSOR FOR SELECT id FROM items WHERE id >= @min
        SET @min = 5
        OPEN c
        DELETE items WHERE id = 4
        FETCH NEXT FROM c INTO @id
        WHILE @@FETCH_STATUS = 0 BEGIN SET @n = @n + 1; FETCH NEXT FROM c INTO @id END
        CLOSE c; DEALLOCATE c
        SELECT @n";
    // (the query ran with @min = 5 at the OPEN: one row; the delete after it is not seen either way)
    assert_eq!(one(&mut s, sql), "1");
}

#[test]
fn cursor_errors_are_the_engines() {
    let h = start(3);
    let mut s = h.cluster.session();
    items(&mut s);
    assert_eq!(error(&mut s, "DECLARE @i INT; FETCH NEXT FROM nope INTO @i"), 16916);
    assert_eq!(error(&mut s, "DECLARE @i INT; DECLARE c CURSOR FOR SELECT id FROM items; FETCH NEXT FROM c INTO @i"), 16917);
    assert_eq!(error(&mut s, "DECLARE c CURSOR FOR SELECT id FROM items; DECLARE c CURSOR FOR SELECT id FROM items"), 16905);
}

#[test]
fn a_trigger_may_walk_the_rows_it_was_fired_for_with_a_cursor() {
    let h = start(3);
    let mut s = h.cluster.session();
    items(&mut s);
    run(&mut s, "CREATE TABLE gone (n INT IDENTITY(1,1) PRIMARY KEY, id INT)");
    // (the shape of the trigger of Reporting Services' Schedule table)
    run(
        &mut s,
        "CREATE TRIGGER items_gone ON items AFTER DELETE AS
         DECLARE id_cursor CURSOR FOR SELECT id FROM deleted
         OPEN id_cursor
         DECLARE @next_id INT
         FETCH NEXT FROM id_cursor INTO @next_id
         WHILE (@@FETCH_STATUS <> -1)
         BEGIN
             IF (@@FETCH_STATUS <> -2)
             BEGIN
                 INSERT gone (id) VALUES (@next_id)
             END
             FETCH NEXT FROM id_cursor INTO @next_id
         END
         CLOSE id_cursor
         DEALLOCATE id_cursor",
    );
    run(&mut s, "DELETE items WHERE id IN (2, 4, 5)");
    assert_eq!(rows(&mut s, "SELECT id FROM gone ORDER BY id").concat(), vec!["2".to_string(), "4".into(), "5".into()]);
    // the trigger fires again, with its cursor declared afresh
    run(&mut s, "DELETE items WHERE id = 1");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM gone"), "4");
}
