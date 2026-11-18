//! Columnstore indexes: `CREATE [NON]CLUSTERED COLUMNSTORE INDEX`, `ALTER INDEX .. REBUILD | REORGANIZE | DISABLE`, the catalog views,
//! and the reads that go through the row groups. Every query is run against a table with the index and an identical table without it,
//! and must give the same rows, whatever has been written to them since.
mod common;
use common::*;
use std::sync::Once;
use tsql_server::Connection;

static INIT: Once = Once::new();

/// With `TSQL_TEST_SPILL` the work memory is so small that grouped queries do not fit and run again without batch mode: their answers are
/// still checked, their plans are not.
fn spilling() -> bool {
    std::env::var("TSQL_TEST_SPILL").is_ok()
}

/// Row groups of 1,000 rows (the real size is 1,048,576), before anything reads the setting.
fn small_groups() {
    INIT.call_once(|| {
        std::env::set_var("TSQL_COLUMNSTORE_ROWGROUP", "1000");
    });
}

const COLS: &str = "id INT NOT NULL PRIMARY KEY, grp INT NOT NULL, amt DECIMAL(10,2) NOT NULL, f FLOAT NULL, name VARCHAR(20) NOT NULL, d DATE NOT NULL, note VARCHAR(10) NULL, big BIGINT NOT NULL";

/// `cs` (with a columnstore index) and `hp` (without), both holding 6,000 rows of the same values.
fn pair(index: &str) -> Connection {
    small_groups();
    let mut c = conn();
    exec(&mut c, &format!("CREATE TABLE cs ({COLS})"));
    exec(&mut c, &format!("CREATE TABLE hp ({COLS})"));
    exec(
        &mut c,
        "DECLARE @i INT = 1; WHILE @i <= 6000 BEGIN
            INSERT hp VALUES (@i, @i % 13, (@i % 977) * 1.25, CASE WHEN @i % 7 = 0 THEN NULL ELSE @i / 4.0 END, CONCAT('n', @i % 50), DATEADD(day, @i % 400, '2020-01-01'), CASE WHEN @i % 5 = 0 THEN NULL ELSE 'x' END, @i * 100000);
            SET @i += 1 END",
    );
    exec(&mut c, "INSERT cs SELECT * FROM hp");
    exec(&mut c, index);
    c
}

fn sorted(c: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let mut r = rows(c, sql);
    r.sort();
    r
}

/// The same statement over both tables gives the same rows.
fn same(c: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let a = sorted(c, &sql.replace("{t}", "cs"));
    let b = sorted(c, &sql.replace("{t}", "hp"));
    assert_eq!(a, b, "{sql}");
    a
}

fn plan_of(c: &mut Connection, sql: &str) -> String {
    exec(c, sql);
    c.last_plan().join("\n")
}

const QUERIES: &[&str] = &[
    "SELECT * FROM {t}",
    "SELECT id, amt FROM {t} WHERE grp = 3",
    "SELECT id FROM {t} WHERE id BETWEEN 1500 AND 1600",
    "SELECT id FROM {t} WHERE id > 5990 OR id < 5",
    "SELECT id, d FROM {t} WHERE d >= '2020-06-01' AND d < '2020-06-10'",
    "SELECT id FROM {t} WHERE amt > 1000.5",
    "SELECT id FROM {t} WHERE f IS NULL",
    "SELECT id FROM {t} WHERE f IS NOT NULL AND f > 1400",
    "SELECT id FROM {t} WHERE note IS NULL AND grp IN (1, 2, 3)",
    "SELECT id FROM {t} WHERE name = 'n7'",
    "SELECT id FROM {t} WHERE name LIKE 'n1%' AND grp <> 4",
    "SELECT id FROM {t} WHERE big >= 500000000 AND big <= 500500000",
    "SELECT COUNT(*), SUM(amt), MIN(d), MAX(name), AVG(amt), COUNT(f), SUM(big) FROM {t}",
    "SELECT grp, COUNT(*), SUM(amt), MIN(id), MAX(id) FROM {t} GROUP BY grp",
    "SELECT grp, SUM(f), COUNT(note) FROM {t} WHERE id % 3 = 0 GROUP BY grp",
    "SELECT name, COUNT(*) FROM {t} WHERE d > '2020-03-01' GROUP BY name HAVING COUNT(*) > 100",
    "SELECT TOP 5 id, amt FROM {t} ORDER BY amt DESC, id",
    "SELECT COUNT(*) FROM {t} WHERE id < 0",
    "SELECT a.id, b.grp FROM {t} a JOIN hp b ON b.id = a.id WHERE a.grp = 5 AND b.amt > 600",
    "SELECT id FROM {t} WHERE id IN (SELECT id FROM hp WHERE grp = 2) AND amt < 100",
];

#[test]
fn queries_give_the_rows_the_heap_gives() {
    let mut c = pair("CREATE CLUSTERED COLUMNSTORE INDEX cci ON cs");
    for q in QUERIES {
        same(&mut c, q);
    }
    let p = plan_of(&mut c, "SELECT id, amt FROM cs WHERE grp = 3");
    assert!(p.contains("columnstore scan of cs"), "{p}");
    let p = plan_of(&mut c, "SELECT id, amt FROM hp WHERE grp = 3");
    assert!(!p.contains("columnstore"), "{p}");
}

#[test]
fn a_nonclustered_columnstore_serves_the_queries_it_covers() {
    let mut c = pair("CREATE NONCLUSTERED COLUMNSTORE INDEX ncci ON cs (id, grp, amt, d)");
    for q in QUERIES {
        same(&mut c, q);
    }
    let p = plan_of(&mut c, "SELECT grp, SUM(amt) FROM cs WHERE d > '2020-03-01' GROUP BY grp");
    assert!(p.contains("columnstore scan of cs"), "{p}");
    // a query of a column the index does not hold reads the heap
    let p = plan_of(&mut c, "SELECT name FROM cs WHERE grp = 3");
    assert!(!p.contains("columnstore"), "{p}");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM cs WHERE grp = 3"), scalar(&mut c, "SELECT COUNT(*) FROM hp WHERE grp = 3"));
}

#[test]
fn row_groups_are_skipped_by_their_bounds() {
    let mut c = pair("CREATE CLUSTERED COLUMNSTORE INDEX cci ON cs");
    // ids are in order: only the last of the six groups holds ids above 5500
    let p = plan_of(&mut c, "SELECT id FROM cs WHERE big > 550000000");
    assert!(p.contains("1 of 6 row group(s) read (5 skipped"), "{p}");
    let p = plan_of(&mut c, "SELECT id FROM cs WHERE big = 300000");
    assert!(p.contains("1 of 6 row group(s) read (5 skipped"), "{p}");
    let p = plan_of(&mut c, "SELECT id FROM cs WHERE big = 99999");
    assert!(p.contains("0 of 6 row group(s) read (6 skipped"), "{p}");
    let p = plan_of(&mut c, "SELECT id FROM cs WHERE note IS NULL AND big < 1000000");
    assert!(p.contains("1 of 6"), "{p}");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM cs WHERE big > 550000000"), "500");
}

/// Run `sql` over both tables (`{t}` is `cs`, then `hp`).
fn both(c: &mut Connection, sql: &str) {
    exec(c, &sql.replace("{t}", "cs"));
    exec(c, &sql.replace("{t}", "hp"));
}

fn all_the_same(c: &mut Connection) {
    // (under the tiny work memory of the spill mode every grouping and sort is spilled to files: a third of the queries is enough)
    for q in QUERIES.iter().step_by(if spilling() { 3 } else { 1 }) {
        same(c, q);
    }
}

fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
}

#[test]
fn writes_after_the_build_are_seen() {
    let mut c = pair("CREATE CLUSTERED COLUMNSTORE INDEX cci ON cs");
    both(&mut c, "UPDATE {t} SET amt = amt + 1 WHERE grp = 2");
    all_the_same(&mut c);
    both(&mut c, "DELETE {t} WHERE id % 11 = 0");
    all_the_same(&mut c);
    both(&mut c, "INSERT {t} VALUES (7001, 99, 1.5, NULL, 'new', '2022-02-02', NULL, 7)");
    both(&mut c, "INSERT {t} VALUES (7002, 99, 2.5, 3.5, 'new', '2022-02-03', 'y', 8)");
    all_the_same(&mut c);
    both(&mut c, "UPDATE {t} SET note = NULL, f = 9.5 WHERE id < 50");
    all_the_same(&mut c);
    both(&mut c, "UPDATE {t} SET id = id + 20000 WHERE id BETWEEN 100 AND 110");
    all_the_same(&mut c);
    // the index was read throughout
    let p = plan_of(&mut c, "SELECT id, amt FROM cs WHERE grp = 3");
    assert!(p.contains("columnstore scan of cs"), "{p}");
    assert_eq!(scalar(&mut c, "SELECT amt FROM cs WHERE big = 300000"), scalar(&mut c, "SELECT amt FROM hp WHERE big = 300000"));
}

#[test]
fn the_index_is_extended_and_rebuilt_as_the_table_changes() {
    let mut c = pair("CREATE CLUSTERED COLUMNSTORE INDEX cci ON cs");
    let groups = |c: &mut Connection| -> (i64, i64, i64) {
        let r = rows(c, "SELECT COUNT(CASE WHEN state = 3 THEN 1 END), ISNULL(SUM(CASE WHEN state = 3 THEN total_rows END), 0), ISNULL(SUM(deleted_rows), 0) FROM sys.column_store_row_groups WHERE object_id = OBJECT_ID('cs')");
        (r[0][0].parse().unwrap(), r[0][1].parse().unwrap(), r[0][2].parse().unwrap())
    };
    assert_eq!(groups(&mut c), (6, 6000, 0));
    // rows added after the build wait in the delta store, and are read from the heap
    exec(&mut c, "DECLARE @i INT = 7001; WHILE @i <= 7500 BEGIN INSERT cs VALUES (@i, @i % 13, 1.25, 1.5, 'z', '2023-01-01', NULL, @i); INSERT hp SELECT * FROM cs WHERE id = @i; SET @i += 1 END");
    let delta = rows(&mut c, "SELECT state_description, total_rows FROM sys.column_store_row_groups WHERE object_id = OBJECT_ID('cs') AND state = 1");
    assert_eq!(delta, table(&[&["OPEN", "500"]]));
    all_the_same(&mut c);
    // a delta store the size of a row group is compressed by the next read
    exec(&mut c, "DECLARE @i INT = 7501; WHILE @i <= 8100 BEGIN INSERT cs VALUES (@i, @i % 13, 1.25, 1.5, 'z', '2023-01-01', NULL, @i); INSERT hp SELECT * FROM cs WHERE id = @i; SET @i += 1 END");
    all_the_same(&mut c);
    let g = groups(&mut c);
    assert_eq!(g.1, 7100, "the rows after the build are in row groups now: {g:?}");
    assert!(g.0 >= 7, "{g:?}");
    // a fifth of the rows rewritten: built again, with nothing marked deleted
    both(&mut c, "UPDATE {t} SET amt = amt + 2 WHERE id % 3 = 0");
    assert!(groups(&mut c).2 > 2000, "{:?}", groups(&mut c));
    all_the_same(&mut c);
    assert_eq!(groups(&mut c).2, 0, "{:?}", groups(&mut c));
    exec(&mut c, "ALTER INDEX cci ON cs REORGANIZE");
    exec(&mut c, "ALTER INDEX ALL ON cs REBUILD");
    assert_eq!(groups(&mut c).1, 7100);
    all_the_same(&mut c);
}

#[test]
fn a_transaction_reads_its_own_changes_and_a_rollback_undoes_them() {
    let mut c = pair("CREATE CLUSTERED COLUMNSTORE INDEX cci ON cs");
    exec(&mut c, "BEGIN TRAN");
    both(&mut c, "UPDATE {t} SET amt = 0 WHERE grp = 4");
    both(&mut c, "DELETE {t} WHERE id < 100");
    both(&mut c, "INSERT {t} VALUES (9000, 1, 1.0, 1.0, 'in tx', '2024-01-01', NULL, 1)");
    all_the_same(&mut c);
    exec(&mut c, "ROLLBACK");
    exec(&mut c, "BEGIN TRAN");
    both(&mut c, "UPDATE {t} SET amt = 1 WHERE grp = 5");
    exec(&mut c, "COMMIT");
    all_the_same(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM cs"), "6000");
}

#[test]
fn other_sessions_see_what_a_heap_scan_would_show_them() {
    small_groups();
    let s = server();
    let mut a = s.connect();
    let mut b = s.connect();
    exec(&mut a, "ALTER DATABASE master SET ALLOW_SNAPSHOT_ISOLATION ON");
    exec(&mut a, "CREATE TABLE cs (id INT PRIMARY KEY, v INT NOT NULL, w INT NOT NULL)");
    exec(&mut a, "DECLARE @i INT = 1; WHILE @i <= 4000 BEGIN INSERT cs VALUES (@i, @i % 10, @i); SET @i += 1 END");
    exec(&mut a, "CREATE CLUSTERED COLUMNSTORE INDEX cci ON cs");
    let before = scalar(&mut a, "SELECT SUM(w) FROM cs WHERE v = 3");
    exec(&mut b, "SET TRANSACTION ISOLATION LEVEL SNAPSHOT");
    exec(&mut b, "BEGIN TRAN");
    assert_eq!(scalar(&mut b, "SELECT SUM(w) FROM cs WHERE v = 3"), before);
    exec(&mut a, "UPDATE cs SET w = 0 WHERE v = 3");
    exec(&mut a, "DELETE cs WHERE id > 3990");
    exec(&mut a, "INSERT cs VALUES (5000, 3, 5)");
    // a snapshot started before the writes still sees the old rows, however it reads them
    assert_eq!(scalar(&mut b, "SELECT SUM(w) FROM cs WHERE v = 3"), before);
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM cs"), "4000");
    exec(&mut b, "COMMIT");
    // and a new one sees the new
    assert_eq!(scalar(&mut b, "SELECT SUM(w) FROM cs WHERE v = 3"), "5");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM cs"), "3991");
    // read uncommitted sees another session's changes before they are committed
    exec(&mut a, "BEGIN TRAN; UPDATE cs SET w = 7 WHERE id = 1");
    exec(&mut b, "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED");
    assert_eq!(scalar(&mut b, "SELECT w FROM cs WHERE id = 1"), "7");
    assert_eq!(scalar(&mut b, "SELECT SUM(w) FROM cs WHERE v = 1 AND id < 3"), "7");
    exec(&mut a, "ROLLBACK");
    assert_eq!(scalar(&mut b, "SELECT w FROM cs WHERE id = 1"), "1");
}

#[test]
fn the_syntax_and_the_catalog() {
    let mut c = pair("CREATE NONCLUSTERED COLUMNSTORE INDEX ncci ON cs (grp, amt) WITH (DROP_EXISTING = OFF, MAXDOP = 1)");
    let ix = |c: &mut Connection, t: &str| rows(c, &format!("SELECT name, type, type_desc, is_disabled FROM sys.indexes WHERE object_id = OBJECT_ID('{t}') ORDER BY index_id"));
    let v = ix(&mut c, "cs");
    assert_eq!(v.last().unwrap(), &["ncci", "6", "NONCLUSTERED COLUMNSTORE", "0"], "{v:?}");
    assert_eq!(
        rows(&mut c, "SELECT ic.column_id, c.name FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id WHERE i.name = 'ncci' ORDER BY ic.index_column_id"),
        table(&[&["2", "grp"], &["3", "amt"]])
    );
    // one columnstore index to a table
    assert_eq!(err_no(&mut c, "CREATE CLUSTERED COLUMNSTORE INDEX other ON cs"), 35339);
    assert_eq!(err_no(&mut c, "CREATE NONCLUSTERED COLUMNSTORE INDEX other ON hp (nope)"), 1911);
    // disabled: kept, not read; rebuilt: read again
    exec(&mut c, "ALTER INDEX ncci ON cs DISABLE");
    assert_eq!(ix(&mut c, "cs").last().unwrap()[3], "1");
    let p = plan_of(&mut c, "SELECT grp, SUM(amt) FROM cs GROUP BY grp");
    assert!(!p.contains("columnstore"), "{p}");
    exec(&mut c, "ALTER INDEX ncci ON cs REBUILD");
    assert_eq!(ix(&mut c, "cs").last().unwrap()[3], "0");
    let p = plan_of(&mut c, "SELECT grp, SUM(amt) FROM cs GROUP BY grp");
    assert!(p.contains("columnstore scan"), "{p}");
    // a column of the index cannot be dropped
    assert_eq!(err_no(&mut c, "ALTER TABLE cs DROP COLUMN amt"), 5074);
    exec(&mut c, "ALTER TABLE cs DROP COLUMN note");
    exec(&mut c, "ALTER TABLE hp DROP COLUMN note");
    assert_eq!(sorted(&mut c, "SELECT grp, SUM(amt) FROM cs GROUP BY grp"), sorted(&mut c, "SELECT grp, SUM(amt) FROM hp GROUP BY grp"));
    exec(&mut c, "DROP INDEX ncci ON cs");
    assert_eq!(ix(&mut c, "cs").len(), 1);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.column_store_row_groups"), "0");
    // ALTER INDEX on other indexes is accepted and does nothing
    exec(&mut c, "ALTER INDEX ALL ON cs REBUILD");
    assert_eq!(err_no(&mut c, "ALTER INDEX nope ON cs REBUILD"), 1911);
    // in the table definition
    exec(&mut c, "CREATE TABLE t1 (a INT, b VARCHAR(10), INDEX cc CLUSTERED COLUMNSTORE)");
    exec(&mut c, "CREATE TABLE t2 (a INT, b VARCHAR(10), c DATE, INDEX nc NONCLUSTERED COLUMNSTORE (a, c))");
    assert_eq!(ix(&mut c, "t1"), table(&[&["cc", "5", "CLUSTERED COLUMNSTORE", "0"]]));
    assert_eq!(ix(&mut c, "t2"), table(&[&["NULL", "0", "HEAP", "0"], &["nc", "6", "NONCLUSTERED COLUMNSTORE", "0"]]));
    // types a columnstore cannot hold
    exec(&mut c, "CREATE TABLE t3 (a INT, x XML, g geometry)");
    assert_eq!(err_no(&mut c, "CREATE CLUSTERED COLUMNSTORE INDEX c3 ON t3"), 35343);
    exec(&mut c, "CREATE NONCLUSTERED COLUMNSTORE INDEX c3 ON t3 (a)");
    // a table with a clustered columnstore index and a key: the key is nonclustered
    exec(&mut c, "CREATE CLUSTERED COLUMNSTORE INDEX c4 ON hp");
    let v = ix(&mut c, "hp");
    assert_eq!(v[0][1..3], ["5", "CLUSTERED COLUMNSTORE"], "{v:?}");
    assert!(v.iter().any(|r| r[2] == "NONCLUSTERED"), "{v:?}");
    assert!(!v.iter().any(|r| r[2] == "HEAP" || r[2] == "CLUSTERED"), "{v:?}");
}

/// Aggregate queries of the shape batch mode takes (the first ones), and some that it leaves to the rows.
const BATCH: &[&str] = &[
    "SELECT COUNT(*) FROM {t}",
    "SELECT COUNT(*), COUNT(f), COUNT(note), SUM(amt), SUM(big), SUM(grp), AVG(amt), AVG(grp), MIN(amt), MAX(amt), MIN(d), MAX(d), MIN(name), MAX(name), MIN(f), MAX(f), MIN(note), MAX(note) FROM {t}",
    "SELECT grp, COUNT(*), SUM(amt), MIN(d), MAX(name) FROM {t} GROUP BY grp",
    "SELECT grp, name, COUNT(*), SUM(big), AVG(amt) FROM {t} GROUP BY grp, name",
    "SELECT note, COUNT(*), SUM(amt) FROM {t} GROUP BY note",
    "SELECT d, COUNT(*), MAX(big) FROM {t} WHERE grp = 2 GROUP BY d",
    "SELECT grp, SUM(amt) FROM {t} WHERE id BETWEEN 100 AND 4000 AND d > '2020-03-01' AND note IS NOT NULL GROUP BY grp HAVING SUM(amt) > 1000",
    "SELECT TOP 3 grp, COUNT(*) c FROM {t} GROUP BY grp ORDER BY c DESC, grp",
    "SELECT grp, COUNT(*) FROM {t} WHERE big < 0 GROUP BY grp",
    "SELECT SUM(amt), COUNT(*), MIN(d) FROM {t} WHERE big < 0",
    "SELECT COUNT(*), SUM(big) FROM {t} WHERE grp IN (1, 5, 9) AND amt >= 100.5 AND amt < 900",
    // left to the rows
    "SELECT grp % 3, COUNT(*) FROM {t} GROUP BY grp % 3",
    "SELECT grp, AVG(CAST(amt AS FLOAT)) FROM {t} GROUP BY grp",
    "SELECT grp, SUM(f) FROM {t} GROUP BY grp",
    "SELECT name, COUNT(*) FROM {t} WHERE name LIKE 'n1%' GROUP BY name",
    "SELECT grp, COUNT(DISTINCT name) FROM {t} GROUP BY grp",
];

fn all_batch(c: &mut Connection) {
    for q in BATCH.iter().step_by(if spilling() { 3 } else { 1 }) {
        same(c, q);
    }
}

#[test]
fn batch_mode_gives_the_answers_the_rows_give() {
    let mut c = pair("CREATE CLUSTERED COLUMNSTORE INDEX cci ON cs");
    all_batch(&mut c);
    for q in &BATCH[..11] {
        let p = plan_of(&mut c, &q.replace("{t}", "cs"));
        assert!(spilling() || p.contains("batch mode"), "{q}\n{p}");
    }
    for q in &BATCH[11..] {
        let p = plan_of(&mut c, &q.replace("{t}", "cs"));
        assert!(!p.contains("batch mode"), "{q}\n{p}");
    }
    let p = plan_of(&mut c, "SELECT grp, SUM(amt) FROM hp GROUP BY grp");
    assert!(!p.contains("batch mode"), "{p}");
    // with the rows written since: the groups give what they have, the heap the rest
    both(&mut c, "UPDATE {t} SET amt = amt * 2, note = 'u' WHERE id % 7 = 0");
    all_batch(&mut c);
    both(&mut c, "DELETE {t} WHERE id % 13 = 0");
    both(&mut c, "INSERT {t} VALUES (8001, 1, 10.5, 1.5, 'zz', '2030-01-01', NULL, 5)");
    all_batch(&mut c);
    let p = plan_of(&mut c, "SELECT grp, SUM(amt) FROM cs GROUP BY grp");
    assert!(spilling() || p.contains("batch mode"), "{p}");
    // a nonclustered index serves it when it holds the columns
    let mut c = pair("CREATE NONCLUSTERED COLUMNSTORE INDEX n ON cs (id, grp, amt, d)");
    all_batch(&mut c);
    let p = plan_of(&mut c, "SELECT grp, SUM(amt), MAX(d) FROM cs WHERE d > '2020-05-01' GROUP BY grp");
    assert!(spilling() || p.contains("batch mode"), "{p}");
}

#[test]
fn batch_mode_leaves_the_sums_that_could_overflow_to_the_rows() {
    small_groups();
    let mut c = conn();
    exec(&mut c, "CREATE TABLE ov (id INT, g INT, v INT, w BIGINT, d DECIMAL(38,2))");
    exec(&mut c, "DECLARE @i INT = 1; WHILE @i <= 2500 BEGIN INSERT ov VALUES (@i, @i % 2, 2000000000, 9000000000000000000, 9999999999999999999999999999999999.99); SET @i += 1 END");
    exec(&mut c, "CREATE CLUSTERED COLUMNSTORE INDEX cc ON ov");
    for q in ["SELECT SUM(v) FROM ov", "SELECT g, SUM(v) FROM ov GROUP BY g", "SELECT SUM(w) FROM ov", "SELECT SUM(d) FROM ov", "SELECT g, AVG(v) FROM ov GROUP BY g"] {
        let e = err(&mut c, q);
        assert_eq!(e.number, 8115, "{q}");
    }
    // the sums that do fit are exact
    assert_eq!(scalar(&mut c, "SELECT SUM(CAST(v AS BIGINT)) FROM ov"), "5000000000000");
    assert_eq!(scalar(&mut c, "SELECT MAX(v) FROM ov"), "2000000000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ov WHERE v > 1"), "2500");
    // a few rows: nothing near the limit
    assert_eq!(scalar(&mut c, "SELECT SUM(v) FROM ov WHERE id < 2"), "2000000000");
    let p = plan_of(&mut c, "SELECT SUM(v) FROM ov WHERE id < 2");
    assert!(spilling() || p.contains("batch mode"), "{p}");
}

#[test]
fn a_collation_on_a_string_group_keeps_batch_mode_exact() {
    small_groups();
    let mut c = conn();
    exec(&mut c, "CREATE TABLE ci (id INT, s VARCHAR(10) COLLATE Latin1_General_CI_AS, n INT)");
    exec(&mut c, "DECLARE @i INT = 1; WHILE @i <= 3000 BEGIN INSERT ci VALUES (@i, CASE WHEN @i % 2 = 0 THEN 'abc' ELSE 'ABC' END, @i); SET @i += 1 END");
    exec(&mut c, "CREATE CLUSTERED COLUMNSTORE INDEX cc ON ci");
    // 'abc' and 'ABC' are one group, as the heap groups them
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM (SELECT s FROM ci GROUP BY s) x"), vec![vec!["1".to_string()]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ci WHERE s = 'Abc'"), "3000");
    assert_eq!(scalar(&mut c, "SELECT MAX(s) FROM ci"), scalar(&mut c, "SELECT MAX(s) FROM (SELECT s FROM ci UNION ALL SELECT 'a') x WHERE s <> 'a'"));
}

#[test]
fn the_index_survives_a_restart_and_is_built_again_when_read() {
    small_groups();
    let dir = tempfile::tempdir().unwrap();
    {
        let s = tsql_server::Server::open(dir.path()).unwrap();
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, grp INT NOT NULL, v INT NOT NULL)");
        exec(&mut c, "DECLARE @i INT = 1; WHILE @i <= 3000 BEGIN INSERT t VALUES (@i, @i % 7, @i); SET @i += 1 END");
        exec(&mut c, "CREATE NONCLUSTERED COLUMNSTORE INDEX ncci ON t (grp, v)");
        assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.column_store_row_groups"), "3");
    }
    let s = tsql_server::Server::open(dir.path()).unwrap();
    let mut c = s.connect();
    // the definition is there; the row groups (in memory only) are not, until a query that can use them is run
    assert_eq!(scalar(&mut c, "SELECT type_desc FROM sys.indexes WHERE name = 'ncci'"), "NONCLUSTERED COLUMNSTORE");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.column_store_row_groups"), "0");
    let p = plan_of(&mut c, "SELECT grp, SUM(v), COUNT(*) FROM t GROUP BY grp");
    assert!(spilling() || p.contains("batch mode"), "{p}");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.column_store_row_groups"), "3");
    assert_eq!(scalar(&mut c, "SELECT SUM(v) FROM t"), "4501500");
    exec(&mut c, "DROP INDEX ncci ON t");
    drop(c);
    drop(s);
    let s = tsql_server::Server::open(dir.path()).unwrap();
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE type IN (5, 6)"), "0");
}

#[test]
fn more_row_groups_than_threads_and_the_other_ways_to_name_the_index() {
    small_groups();
    let mut c = conn();
    exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, g INT NOT NULL, s VARCHAR(8) NOT NULL, v DECIMAL(12,2) NOT NULL, d DATE NOT NULL)");
    exec(&mut c, "CREATE TABLE ref (id INT PRIMARY KEY, g INT NOT NULL, s VARCHAR(8) NOT NULL, v DECIMAL(12,2) NOT NULL, d DATE NOT NULL)");
    let n = if spilling() { 4000 } else { 20000 };
    exec(&mut c, &format!("DECLARE @i INT = 1; WHILE @i <= {n} BEGIN INSERT ref VALUES (@i, @i % 37, CONCAT('s', @i % 11), (@i % 1000) / 4.0, DATEADD(day, @i % 900, '2018-01-01')); SET @i += 1 END"));
    exec(&mut c, "INSERT big SELECT * FROM ref");
    // the index added to a table that exists
    exec(&mut c, "ALTER TABLE big ADD INDEX cc CLUSTERED COLUMNSTORE");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.column_store_row_groups WHERE object_id = OBJECT_ID('big') AND state = 3"), (n / 1000).to_string());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.dm_db_column_store_row_group_physical_stats WHERE object_id = OBJECT_ID('big')"), (n / 1000).to_string());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.column_store_segments"), (n / 1000 * 5).to_string());
    for q in [
        "SELECT g, s, COUNT(*), SUM(v), MIN(d), MAX(d), AVG(v) FROM {t} GROUP BY g, s",
        "SELECT s, COUNT(*), SUM(v) FROM {t} WHERE d BETWEEN '2018-06-01' AND '2019-03-01' AND g <> 5 GROUP BY s",
        "SELECT COUNT(*), SUM(v), MAX(s) FROM {t} WHERE g IN (3, 4, 5)",
        "SELECT TOP 5 g, SUM(v) t FROM {t} GROUP BY g ORDER BY t DESC, g",
    ] {
        let a = sorted(&mut c, &q.replace("{t}", "big"));
        let b = sorted(&mut c, &q.replace("{t}", "ref"));
        assert_eq!(a, b, "{q}");
    }
    let p = plan_of(&mut c, "SELECT g, SUM(v) FROM big GROUP BY g");
    assert!(spilling() || p.contains(&format!("batch mode: the aggregates of {} computed", n / 1000)), "{p}");
    // the same answers on one thread
    exec(&mut c, "SELECT g, SUM(v) FROM big GROUP BY g OPTION (MAXDOP 1)");
    assert_eq!(sorted(&mut c, "SELECT g, SUM(v) FROM big GROUP BY g OPTION (MAXDOP 1)"), sorted(&mut c, "SELECT g, SUM(v) FROM ref GROUP BY g"));
}
