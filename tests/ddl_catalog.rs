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

#[test]
fn alter_table_with_check_or_nocheck_add_constraint() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE chunk (id INT NOT NULL, v INT NULL)");
    exec(&mut c, "ALTER TABLE [dbo].[chunk] WITH NOCHECK ADD CONSTRAINT [PK_chunk] PRIMARY KEY NONCLUSTERED ([id]) ON [PRIMARY]");
    exec(&mut c, "INSERT chunk VALUES (1, 1)");
    assert_eq!(err_no(&mut c, "INSERT chunk VALUES (1, 2)"), 2627);
    exec(&mut c, "ALTER TABLE chunk WITH CHECK ADD CONSTRAINT ck_v CHECK (v < 100)");
    assert_eq!(err_no(&mut c, "INSERT chunk VALUES (2, 500)"), 547);
    // (the forms that were there are as they were)
    exec(&mut c, "ALTER TABLE chunk NOCHECK CONSTRAINT ck_v");
    exec(&mut c, "ALTER TABLE chunk WITH CHECK CHECK CONSTRAINT ck_v");
}

#[test]
fn create_table_accepts_a_comma_before_the_closing_parenthesis_and_textimage_on() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE [dbo].[SecData](\r\n    [SecDataID] uniqueidentifier NOT NULL,\r\n    [XmlDescription] [ntext] NOT NULL,  \r\n    [NtSecDescPrimary] [image] NOT NULL,\r\n    [NtSecDescSecondary] [ntext] NULL,\r\n)  ON [PRIMARY] TEXTIMAGE_ON[PRIMARY]");
    exec(&mut c, "INSERT SecData VALUES (NEWID(), 'x', 0x01, NULL)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM SecData"), "1");
}

#[test]
fn table_hints_on_the_target_of_a_data_change() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE sch (id INT PRIMARY KEY, flag INT NULL)");
    exec(&mut c, "INSERT INTO sch WITH (TABLOCK) VALUES (1, 1), (2, 1)");
    exec(&mut c, "UPDATE [sch] WITH (tablock, xlock) SET [flag] = NULL");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sch WHERE flag IS NULL"), "2");
    exec(&mut c, "DELETE FROM sch WITH (ROWLOCK) WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sch"), "1");
}

#[test]
fn the_text_pointer_statements_are_read_so_that_a_procedure_with_one_can_be_made() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE chunk (id INT PRIMARY KEY, content IMAGE NULL)");
    // (Reporting Services' WriteChunkPortion)
    exec(
        &mut c,
        "CREATE PROCEDURE WriteChunkPortion @ptr binary(16), @perm bit, @idx int = NULL, @del int = NULL, @Content image AS\r\nIF @perm != 0 BEGIN\r\n    UPDATETEXT chunk.Content @ptr @idx @del @Content\r\nEND ELSE BEGIN\r\n    UPDATETEXT [master].dbo.chunk.Content @ptr @idx @del WITH LOG @Content\r\nEND",
    );
    // the statement itself says it is not supported
    assert_eq!(err_no(&mut c, "DECLARE @p binary(16) = 0x00; EXEC WriteChunkPortion @p, 1, 0, 0, 0x01"), 50100);
    exec(&mut c, "SELECT 1");
}

#[test]
fn sp_rename_renames_an_index() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE ren (id INT PRIMARY KEY, a INT, b INT)");
    exec(&mut c, "CREATE UNIQUE INDEX [IX_old] ON ren (a, b)");
    exec(&mut c, "EXEC sp_rename 'ren.IX_old', 'UQ_new', 'INDEX'");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID('ren') AND name = 'UQ_new'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID('ren') AND name = 'IX_old'"), "0");
    // the index still enforces what it did, under its new name
    exec(&mut c, "INSERT ren VALUES (1, 1, 1)");
    assert_eq!(err_no(&mut c, "INSERT ren VALUES (2, 1, 1)"), 2601);
    assert_eq!(err_no(&mut c, "EXEC sp_rename 'ren.nope', 'x', 'INDEX'"), 15225);
    exec(&mut c, "CREATE INDEX [IX_two] ON ren (b)");
    assert_eq!(err_no(&mut c, "EXEC sp_rename 'ren.IX_two', 'UQ_new', 'INDEX'"), 15338);
    exec(&mut c, "DROP INDEX UQ_new ON ren");
}

#[test]
fn create_index_with_drop_existing_replaces_the_index() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE snap (id INT PRIMARY KEY, a INT, b INT, m VARCHAR(10))");
    exec(&mut c, "CREATE INDEX [IX_clean] ON [dbo].[snap] (a, b)");
    // (Reporting Services' upgrade script)
    exec(&mut c, "CREATE INDEX [IX_clean] ON [dbo].[snap]([a], [b]) INCLUDE ([m])\r\nWITH (DROP_EXISTING = ON)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID('snap') AND name = 'IX_clean'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.index_columns ic JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id WHERE i.name = 'IX_clean' AND ic.is_included_column = 1"), "1");
    // with no index of the name it just makes one
    exec(&mut c, "CREATE INDEX [IX_new] ON [dbo].[snap] (b) WITH (DROP_EXISTING = ON)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID('snap') AND name = 'IX_new'"), "1");
    // without the option an existing name is still an error
    assert_eq!(err_no(&mut c, "CREATE INDEX [IX_new] ON [dbo].[snap] (b)"), 1913);
}

#[test]
fn an_sp_rename_is_committed_with_its_statement_and_a_later_rollback_leaves_it() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE ExecutionLog (id INT PRIMARY KEY, v INT NULL)");
    exec(&mut c, "exec sp_rename 'dbo.ExecutionLog', 'ExecutionLog_Old'");
    exec(&mut c, "BEGIN TRANSACTION");
    exec(&mut c, "INSERT ExecutionLog_Old VALUES (1, 1)");
    exec(&mut c, "ROLLBACK TRANSACTION");
    // the rename stands, the insert is undone
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ExecutionLog_Old"), "0");
    exec(&mut c, "CREATE TABLE ExecutionLog (id INT PRIMARY KEY)");
    // inside a transaction it is the transaction's, as before
    exec(&mut c, "BEGIN TRANSACTION");
    exec(&mut c, "exec sp_rename 'dbo.ExecutionLog', 'ExecutionLog_Two'");
    exec(&mut c, "ROLLBACK TRANSACTION");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ExecutionLog"), "0");
}

#[test]
fn an_insert_column_list_may_name_a_column_with_a_qualifier() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE dst (a INT, mime VARCHAR(10))");
    exec(&mut c, "CREATE TABLE src (a INT, mime VARCHAR(10))");
    exec(&mut c, "INSERT src VALUES (1, 'x'), (2, 'y')");
    // (Reporting Services' CopyChunksOfType)
    exec(&mut c, "INSERT INTO dst (a, C.mime) SELECT C.a, C.mime FROM src C");
    assert_eq!(rows(&mut c, "SELECT a, mime FROM dst ORDER BY a"), vec![vec!["1", "x"], vec!["2", "y"]]);
}

#[test]
fn a_table_constraint_may_follow_the_last_column_with_no_comma() {
    let mut c = conn();
    // (Reporting Services' DBUpgradeHistory)
    exec(&mut c, "create table [dbo].[DBUpgradeHistory](\r\n   [UpgradeID] bigint identity(1,1) not null,\r\n   [DbVersion] nvarchar(25),\r\n   [User] nvarchar(128) default System_User,\r\n   [DateTime] datetime default Current_Timestamp\r\n   CONSTRAINT [PK_DBUpgradeHistory] PRIMARY KEY (UpgradeID DESC)\r\n   ) ON [Primary]");
    exec(&mut c, "INSERT [dbo].[DBUpgradeHistory] ([DbVersion]) VALUES ('100')");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM DBUpgradeHistory"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE name = 'PK_DBUpgradeHistory' AND is_primary_key = 1"), "1");
    // a column's own constraints are still its own
    exec(&mut c, "create table kk (a int CONSTRAINT pk_kk PRIMARY KEY, b int CONSTRAINT uq_kk UNIQUE, c int CONSTRAINT df_kk DEFAULT 5 CONSTRAINT ck_kk CHECK (c > 0) CONSTRAINT uq2 UNIQUE (a, b))");
    exec(&mut c, "INSERT kk (a, b) VALUES (1, 2)");
    assert_eq!(scalar(&mut c, "SELECT c FROM kk"), "5");
}

#[test]
fn sp_procedure_params_100_managed_describes_the_parameters_of_a_procedure_or_function() {
    let mut c = conn();
    exec(&mut c, "CREATE PROCEDURE dbo.p1 @a INT, @b VARCHAR(20) = 'x', @c DECIMAL(10,2) OUTPUT AS SELECT 1");
    exec(&mut c, "CREATE FUNCTION dbo.f1 (@PersonID INT) RETURNS TABLE AS RETURN SELECT @PersonID AS id");
    // (what the Reporting Services portal asks for a function of the database a data source names)
    let r = rows(&mut c, "EXEC [master].[sys].[sp_procedure_params_100_managed] @procedure_name = N'f1'");
    assert_eq!(r.iter().map(|x| format!("{}/{}/{}", x[0], x[2], x[11])).collect::<Vec<_>>(), vec!["@personid/1/int"]);
    let r = rows(&mut c, "EXEC sys.sp_procedure_params_100_managed N'p1'");
    assert_eq!(r.iter().map(|x| format!("{}/{}/{}/{}", x[0], x[1], x[2], x[11])).collect::<Vec<_>>(), vec!["@RETURN_VALUE/0/4/int", "@a/1/1/int", "@b/2/1/varchar", "@c/3/2/decimal"]);
    assert_eq!(r[2][3], "1", "@b has a default");
    assert_eq!(r[2][6], "20", "the length of a varchar");
    assert_eq!(r[3][7], "10", "the precision of a decimal");
    assert_eq!(rows(&mut c, "EXEC sys.sp_procedure_params_100_managed N'dbo.nope'").len(), 0);
}
