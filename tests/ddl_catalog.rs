mod common;
use common::*;

#[test]
fn alter_table_add_and_drop_columns() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, a INT)");
    exec(&mut c, "INSERT t VALUES (1, 10), (2, 20)");
    exec(&mut c, "ALTER TABLE t ADD b VARCHAR(5) NULL");
    assert_eq!(rows(&mut c, "SELECT b FROM t ORDER BY id"), vec![vec!["NULL"], vec!["NULL"]]);
    exec(&mut c, "ALTER TABLE t ADD c INT NOT NULL DEFAULT 7");
    assert_eq!(rows(&mut c, "SELECT c FROM t ORDER BY id"), vec![vec!["7"], vec!["7"]]);
    assert_eq!(err_no(&mut c, "ALTER TABLE t ADD d INT NOT NULL"), 4901);
    assert_eq!(err_no(&mut c, "ALTER TABLE t ADD a INT"), 2705);
    exec(&mut c, "ALTER TABLE t DROP COLUMN b");
    assert_eq!(rows(&mut c, "SELECT * FROM t ORDER BY id"), vec![vec!["1", "10", "7"], vec!["2", "20", "7"]]);
    assert_eq!(err_no(&mut c, "ALTER TABLE t DROP COLUMN id"), 5074);
    assert_eq!(err_no(&mut c, "ALTER TABLE t DROP COLUMN nope"), 4924);
    exec(&mut c, "ALTER TABLE t DROP COLUMN IF EXISTS nope");
    exec(&mut c, "INSERT t (id, a) VALUES (3, 30)");
    assert_eq!(scalar(&mut c, "SELECT c FROM t WHERE id = 3"), "7");
}

#[test]
fn alter_table_constraints() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE p (id INT PRIMARY KEY)");
    exec(&mut c, "CREATE TABLE t (id INT, pid INT, v INT)");
    exec(&mut c, "INSERT p VALUES (1)");
    exec(&mut c, "INSERT t VALUES (1, 1, 5), (2, 1, 6)");
    exec(&mut c, "ALTER TABLE t ADD CONSTRAINT pk_t PRIMARY KEY (id)");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (1, 1, 1)"), 2627);
    exec(&mut c, "ALTER TABLE t ADD CONSTRAINT fk_t FOREIGN KEY (pid) REFERENCES p(id)");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (3, 2, 1)"), 547);
    exec(&mut c, "ALTER TABLE t ADD CONSTRAINT ck_v CHECK (v > 0)");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (3, 1, -1)"), 547);
    assert_eq!(err_no(&mut c, "ALTER TABLE t ADD CONSTRAINT ck_bad CHECK (v > 100)"), 547);
    exec(&mut c, "ALTER TABLE t NOCHECK CONSTRAINT ck_v");
    exec(&mut c, "INSERT t VALUES (3, 1, -1)");
    exec(&mut c, "ALTER TABLE t DROP CONSTRAINT ck_v");
    exec(&mut c, "ALTER TABLE t DROP CONSTRAINT fk_t");
    exec(&mut c, "INSERT t VALUES (4, 999, 1)");
    assert_eq!(err_no(&mut c, "ALTER TABLE t DROP CONSTRAINT nope"), 3728);
    exec(&mut c, "ALTER TABLE t ADD CONSTRAINT df_v DEFAULT 42 FOR v");
    exec(&mut c, "INSERT t (id, pid) VALUES (5, 1)");
    assert_eq!(scalar(&mut c, "SELECT v FROM t WHERE id = 5"), "42");
    exec(&mut c, "ALTER TABLE t ALTER COLUMN v BIGINT NOT NULL");
    assert_eq!(scalar(&mut c, "SELECT SUM(v) FROM t"), "53");
}

#[test]
fn create_index_and_drop() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT, b INT, c INT)");
    exec(&mut c, "CREATE INDEX ix_a ON t (a)");
    exec(&mut c, "CREATE NONCLUSTERED INDEX ix_ab ON t (a DESC, b) INCLUDE (c)");
    // a column cannot be both a key column and an included one
    assert_eq!(err_no(&mut c, "CREATE INDEX ix_dup ON t (a) INCLUDE (a)"), 1909);
    exec(&mut c, "CREATE UNIQUE CLUSTERED INDEX ux ON t (b)");
    assert_eq!(err_no(&mut c, "CREATE INDEX ix_a ON t (a)"), 1913);
    assert_eq!(rows(&mut c, "SELECT name FROM sys.indexes WHERE object_id = OBJECT_ID('t') AND name IS NOT NULL ORDER BY name"), vec![vec!["ix_a"], vec!["ix_ab"], vec!["ux"]]);
    exec(&mut c, "DROP INDEX ix_a ON t");
    assert_eq!(err_no(&mut c, "DROP INDEX ix_a ON t"), 3703);
    exec(&mut c, "DROP INDEX IF EXISTS ix_a ON t");
}

#[test]
fn sys_catalog_views() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE people (id INT IDENTITY PRIMARY KEY, name NVARCHAR(40) NOT NULL, born DATE NULL, score DECIMAL(8,3))");
    script(&mut c, "CREATE VIEW v_people AS SELECT name FROM people
GO
CREATE PROCEDURE sp_hello AS SELECT 1
GO
CREATE FUNCTION fn_one() RETURNS INT AS BEGIN RETURN 1 END
GO");
    assert_eq!(rows(&mut c, "SELECT name FROM sys.tables"), vec![vec!["people"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM sys.views"), vec![vec!["v_people"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM sys.procedures"), vec![vec!["sp_hello"]]);
    assert_eq!(rows(&mut c, "SELECT name, type FROM sys.objects ORDER BY name").len(), 5);
    assert_eq!(rows(&mut c, "SELECT c.name, c.is_nullable, c.is_identity FROM sys.columns c JOIN sys.tables t ON c.object_id = t.object_id WHERE t.name = 'people' ORDER BY c.column_id"), vec![
        vec!["id", "0", "1"], vec!["name", "0", "0"], vec!["born", "1", "0"], vec!["score", "1", "0"]]);
    assert_eq!(rows(&mut c, "SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE, CHARACTER_MAXIMUM_LENGTH FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = 'people' ORDER BY ORDINAL_POSITION"), vec![
        vec!["id", "int", "NO", "NULL"], vec!["name", "nvarchar", "NO", "40"], vec!["born", "date", "YES", "NULL"], vec!["score", "decimal", "YES", "NULL"]]);
    assert_eq!(rows(&mut c, "SELECT TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.TABLES ORDER BY TABLE_NAME"), vec![vec!["people", "BASE TABLE"], vec!["v_people", "VIEW"]]);
    assert_eq!(rows(&mut c, "SELECT CONSTRAINT_TYPE FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS WHERE TABLE_NAME = 'people'"), vec![vec!["PRIMARY KEY"]]);
    assert_eq!(rows(&mut c, "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM INFORMATION_SCHEMA.ROUTINES ORDER BY ROUTINE_NAME"), vec![vec!["fn_one", "FUNCTION"], vec!["sp_hello", "PROCEDURE"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM sys.schemas WHERE name = 'dbo'"), vec![vec!["dbo"]]);
    assert_eq!(rows(&mut c, "SELECT name FROM sys.databases WHERE name IN ('master','tempdb') ORDER BY name"), vec![vec!["master"], vec!["tempdb"]]);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM sys.identity_columns"), vec![vec!["1"]]);
    assert!(scalar(&mut c, "SELECT definition FROM sys.sql_modules m JOIN sys.views v ON m.object_id = v.object_id").contains("SELECT name FROM people"));
}

#[test]
fn system_procedures() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT, name VARCHAR(10))");
    script(&mut c, "CREATE VIEW vw AS SELECT id FROM t
GO");
    exec(&mut c, "EXEC sp_rename 't', 'renamed'");
    assert_eq!(err_no(&mut c, "SELECT * FROM t"), 208);
    exec(&mut c, "SELECT * FROM renamed");
    exec(&mut c, "EXEC sp_rename 'renamed.name', 'label', 'COLUMN'");
    exec(&mut c, "SELECT label FROM renamed");
    let r = exec(&mut c, "EXEC sp_helptext 'vw'");
    assert!(r.resultsets()[0].rows[0][0].to_string().contains("CREATE VIEW vw"));
    let r = exec(&mut c, "EXEC sp_tables");
    assert_eq!(r.resultsets()[0].rows.len(), 2);
    let r = exec(&mut c, "EXEC sp_columns 'renamed'");
    assert_eq!(r.resultsets()[0].rows.len(), 2);
}

#[test]
fn go_batches_and_scripts() {
    let mut c = conn();
    let results = c.execute_script("CREATE TABLE t (a INT)\nGO\nINSERT t VALUES (1)\nGO 3\nSELECT COUNT(*) FROM t\nGO");
    assert_eq!(results.len(), 5);
    assert_eq!(results.last().unwrap().resultsets()[0].rows[0][0].to_string(), "3");
    // GO inside a string or comment is not a separator
    let results = c.execute_script("SELECT 'a\nGO\nb' AS s\n/*\nGO\n*/\nGO");
    assert_eq!(results.len(), 1);
    assert!(results[0].errors().is_empty());
    // a parse error aborts only its own batch
    let results = c.execute_script("SELEC 1\nGO\nSELECT 2");
    assert_eq!(results[0].errors().len(), 1);
    assert_eq!(results[1].resultsets()[0].rows[0][0].to_string(), "2");
    // CREATE must start its own batch: statements after CREATE PROC belong to the proc
    let results = c.execute_script("CREATE PROCEDURE pp AS SELECT 1\nSELECT 2\nGO\nEXEC pp");
    assert_eq!(results[1].resultsets().len(), 2);
}

#[test]
fn comments_and_identifiers() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT /* inline */ 1 -- trailing\n"), "1");
    assert_eq!(scalar(&mut c, "SELECT /* nested /* comment */ still */ 2"), "2");
    exec(&mut c, "CREATE TABLE [my table] ([my col] INT, \"other col\" INT)");
    exec(&mut c, "INSERT [my table] ([my col], [other col]) VALUES (1, 2)");
    assert_eq!(rows(&mut c, "SELECT [my col] + [other col] AS [the sum] FROM [my table]"), vec![vec!["3"]]);
    exec(&mut c, "CREATE TABLE [select] ([from] INT)");
    exec(&mut c, "INSERT [select] VALUES (9)");
    assert_eq!(scalar(&mut c, "SELECT [from] FROM [select]"), "9");
    // names are case-insensitive
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM [MY TABLE]"), "1");
    assert_eq!(scalar(&mut c, "select CoUnT(*) fRoM [my table]"), "1");
    // dotted / bracketed names
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM [dbo].[my table]"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM master.dbo.[my table]"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM master..[my table]"), "1");
}

#[test]
fn multiple_sessions_share_a_server() {
    let server = common::server();
    let mut a = server.connect();
    let mut b = server.connect();
    exec(&mut a, "CREATE TABLE shared (v INT)");
    exec(&mut a, "INSERT shared VALUES (1)");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM shared"), "1");
    // temp tables are private to a session
    exec(&mut a, "CREATE TABLE #mine (x INT)");
    assert_eq!(err_no(&mut b, "SELECT * FROM #mine"), 208);
    // variables and USE are private too
    exec(&mut a, "CREATE DATABASE other");
    exec(&mut a, "USE other");
    assert_eq!(scalar(&mut b, "SELECT DB_NAME()"), "master");
    assert_ne!(scalar(&mut a, "SELECT @@SPID"), scalar(&mut b, "SELECT @@SPID"));
}

#[test]
fn table_valued_parameters() {
    let mut c = conn();
    exec(&mut c, "CREATE TYPE IdList AS TABLE (id INT PRIMARY KEY)");
    script(&mut c, "CREATE PROCEDURE sum_ids @ids IdList READONLY AS SELECT SUM(id) FROM @ids
GO");
    assert_eq!(rows(&mut c, "DECLARE @l IdList; INSERT @l VALUES (1),(2),(3); EXEC sum_ids @l"), vec![vec!["6"]]);
    assert_eq!(rows(&mut c, "DECLARE @l IdList; SELECT COUNT(*) FROM @l"), vec![vec!["0"]]);
}

#[test]
fn drop_if_exists_and_misc_ddl() {
    let mut c = conn();
    exec(&mut c, "DROP TABLE IF EXISTS nothing");
    exec(&mut c, "DROP VIEW IF EXISTS nothing");
    exec(&mut c, "DROP PROCEDURE IF EXISTS nothing");
    exec(&mut c, "DROP FUNCTION IF EXISTS nothing");
    exec(&mut c, "DROP SCHEMA IF EXISTS nothing");
    exec(&mut c, "CREATE TABLE a (x INT)");
    exec(&mut c, "CREATE TABLE b (x INT)");
    exec(&mut c, "DROP TABLE a, b");
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM sys.tables"), vec![vec!["0"]]);
    assert_eq!(err_no(&mut c, "DROP VIEW nothing"), 3701);
    exec(&mut c, "CREATE SCHEMA s1");
    exec(&mut c, "CREATE TABLE s1.t (x INT)");
    assert_eq!(err_no(&mut c, "DROP SCHEMA s1"), 3729);
    exec(&mut c, "DROP TABLE s1.t");
    exec(&mut c, "DROP SCHEMA s1");
    exec(&mut c, "CREATE TABLE g (x INT)");
    exec(&mut c, "GRANT SELECT ON g TO public");
    assert_eq!(err_no(&mut c, "GRANT SELECT ON nothing TO public"), 15151);
}

#[test]
fn performance_smoke() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, g INT, v VARCHAR(20))");
    exec(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 5000) INSERT big SELECT i, i % 50, 'row' + CAST(i AS VARCHAR(10)) FROM n OPTION (MAXRECURSION 0)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "5000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big a JOIN big b ON a.id = b.id"), "5000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT g) FROM big"), "50");
    assert_eq!(scalar(&mut c, "SELECT TOP 1 v FROM big ORDER BY id DESC"), "row5000");
    exec(&mut c, "DECLARE @i INT = 0; WHILE @i < 500 BEGIN INSERT big VALUES (10000 + @i, 0, 'loop'); SET @i += 1 END");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "5500");
    exec(&mut c, "UPDATE big SET g = g + 1 WHERE id % 2 = 0");
    exec(&mut c, "DELETE big WHERE g > 40");
}
