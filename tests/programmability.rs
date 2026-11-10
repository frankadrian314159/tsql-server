mod common;
use common::*;

#[test]
fn variables_and_assignment() {
    let mut c = conn();
    assert_eq!(rows(&mut c, "DECLARE @a INT = 5, @b VARCHAR(10) = 'hi'; SET @a = @a * 2; SELECT @a, @b"), vec![vec!["10", "hi"]]);
    assert_eq!(rows(&mut c, "DECLARE @a INT; SELECT @a"), vec![vec!["NULL"]]);
    assert_eq!(rows(&mut c, "DECLARE @s VARCHAR(3) = 'abcdef'; SELECT @s"), vec![vec!["abc"]]);
    assert_eq!(rows(&mut c, "DECLARE @d DECIMAL(5,2) = 1.239; SELECT @d"), vec![vec!["1.24"]]);
    assert_eq!(rows(&mut c, "DECLARE @n INT = 1; SET @n += 4; SET @n *= 3; SELECT @n"), vec![vec!["15"]]);
    assert_eq!(err_no(&mut c, "DECLARE @a INT; DECLARE @a INT"), 134);
    assert_eq!(err_no(&mut c, "SET @nope = 1"), 137);
    // variables do not outlive the batch
    exec(&mut c, "DECLARE @gone INT = 1");
    assert_eq!(err_no(&mut c, "SELECT @gone"), 137);
}

#[test]
fn control_flow() {
    let mut c = conn();
    assert_eq!(rows(&mut c, "DECLARE @x INT = 5; IF @x > 3 SELECT 'big' ELSE SELECT 'small'"), vec![vec!["big"]]);
    assert_eq!(rows(&mut c, "DECLARE @x INT = 1; IF @x > 3 SELECT 'big' ELSE SELECT 'small'"), vec![vec!["small"]]);
    assert_eq!(rows(&mut c, "DECLARE @i INT = 0, @s INT = 0; WHILE @i < 10 BEGIN SET @i += 1; IF @i % 2 = 0 CONTINUE; SET @s += @i; IF @i >= 7 BREAK END; SELECT @s, @i"), vec![vec!["16", "7"]]);
    assert_eq!(rows(&mut c, "DECLARE @i INT = 0; WHILE 1=1 BEGIN SET @i += 1; IF @i = 3 BREAK END; SELECT @i"), vec![vec!["3"]]);
    // nested blocks and GOTO
    assert_eq!(rows(&mut c, "DECLARE @n INT = 0; start: SET @n += 1; IF @n < 3 GOTO start; SELECT @n"), vec![vec!["3"]]);
    // RETURN ends the batch early
    let r = exec(&mut c, "SELECT 1; RETURN; SELECT 2");
    assert_eq!(r.resultsets().len(), 1);
    // IF EXISTS pattern
    exec(&mut c, "CREATE TABLE t (a INT)");
    assert_eq!(rows(&mut c, "IF EXISTS (SELECT 1 FROM t) SELECT 'has' ELSE SELECT 'empty'"), vec![vec!["empty"]]);
    assert_eq!(rows(&mut c, "IF OBJECT_ID('t', 'U') IS NOT NULL SELECT 'exists'"), vec![vec!["exists"]]);
    assert_eq!(rows(&mut c, "IF OBJECT_ID('nope') IS NULL SELECT 'missing'"), vec![vec!["missing"]]);
}

#[test]
fn print_and_raiserror() {
    let mut c = conn();
    assert_eq!(msgs(&mut c, "PRINT 'hello'"), vec!["hello"]);
    assert_eq!(msgs(&mut c, "PRINT 1 + 1"), vec!["2"]);
    assert_eq!(msgs(&mut c, "DECLARE @n INT = 7; PRINT 'n=' + CAST(@n AS VARCHAR(5))"), vec!["n=7"]);
    assert_eq!(msgs(&mut c, "RAISERROR('info %d %s', 0, 1, 42, 'x')"), vec!["info 42 x"]);
    let e = err(&mut c, "RAISERROR('bad %s', 16, 1, 'thing')");
    assert_eq!(e.number, 50000);
    assert_eq!(e.severity, 16);
    assert_eq!(e.message, "bad thing");
    // RAISERROR does not abort the batch
    let r = c.execute("RAISERROR('e', 16, 1); PRINT 'after'");
    assert_eq!(r.errors().len(), 1);
    assert_eq!(r.messages(), vec!["after"]);
}

#[test]
fn throw_semantics() {
    let mut c = conn();
    let e = err(&mut c, "THROW 51000, 'custom', 2");
    assert_eq!((e.number, e.state, e.message.as_str()), (51000, 2, "custom"));
    assert_eq!(err_no(&mut c, "THROW 100, 'low', 1"), 35100);
    // THROW aborts the rest of the batch
    let r = c.execute("THROW 51000, 'x', 1; PRINT 'unreachable'");
    assert_eq!(r.errors().len(), 1);
    assert!(r.messages().is_empty());
    // rethrow in CATCH
    let e = err(&mut c, "BEGIN TRY THROW 51234, 'inner', 1 END TRY BEGIN CATCH THROW END CATCH");
    assert_eq!(e.number, 51234);
}

#[test]
fn try_catch() {
    let mut c = conn();
    assert_eq!(rows(&mut c, "BEGIN TRY SELECT 1/0 END TRY BEGIN CATCH SELECT ERROR_NUMBER(), ERROR_MESSAGE(), ERROR_SEVERITY() END CATCH"),
        vec![vec!["8134", "Divide by zero error encountered.", "16"]]);
    assert_eq!(rows(&mut c, "DECLARE @r INT = 0; BEGIN TRY SET @r = 1; SELECT CAST('x' AS INT); SET @r = 2 END TRY BEGIN CATCH SET @r = @r + 10 END CATCH; SELECT @r"), vec![vec!["11"]]);
    // nested
    assert_eq!(rows(&mut c, "BEGIN TRY BEGIN TRY SELECT 1/0 END TRY BEGIN CATCH THROW 50001, 'inner', 1 END CATCH END TRY BEGIN CATCH SELECT ERROR_MESSAGE() END CATCH"), vec![vec!["inner"]]);
    // no error: catch skipped
    assert_eq!(rows(&mut c, "BEGIN TRY SELECT 'ok' END TRY BEGIN CATCH SELECT 'caught' END CATCH"), vec![vec!["ok"]]);
    // ERROR_* outside CATCH are NULL
    assert_eq!(scalar(&mut c, "SELECT ERROR_NUMBER()"), "NULL");
    // constraint violation is catchable
    exec(&mut c, "CREATE TABLE u (a INT PRIMARY KEY)");
    exec(&mut c, "INSERT u VALUES (1)");
    assert_eq!(rows(&mut c, "BEGIN TRY INSERT u VALUES (1) END TRY BEGIN CATCH SELECT ERROR_NUMBER() END CATCH"), vec![vec!["2627"]]);
    // errors inside procedures are catchable by the caller
    exec(&mut c, "CREATE PROCEDURE boom AS SELECT 1/0");
    assert_eq!(rows(&mut c, "BEGIN TRY EXEC boom END TRY BEGIN CATCH SELECT ERROR_NUMBER() END CATCH"), vec![vec!["8134"]]);
}

#[test]
fn statement_terminating_errors_continue_the_batch() {
    let mut c = conn();
    let r = c.execute("SELECT 1/0; SELECT 'next'");
    assert_eq!(r.errors().len(), 1);
    assert_eq!(r.resultsets().len(), 1);
    assert_eq!(r.resultsets()[0].rows[0][0].to_string(), "next");
    // XACT_ABORT makes the batch stop
    let r = c.execute("SET XACT_ABORT ON; SELECT 1/0; SELECT 'next'");
    assert_eq!(r.errors().len(), 1);
    assert_eq!(r.resultsets().len(), 0);
    // @@ERROR after a failure
    c.execute("SET XACT_ABORT OFF");
    assert_eq!(rows(&mut c, "DECLARE @e INT; SELECT @e = 0; SET @e = @@ERROR; SELECT @e"), vec![vec!["0"]]);
}

#[test]
fn transactions() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT)");
    exec(&mut c, "BEGIN TRAN; INSERT t VALUES (1); COMMIT");
    exec(&mut c, "BEGIN TRAN; INSERT t VALUES (2); ROLLBACK");
    assert_eq!(rows(&mut c, "SELECT a FROM t"), vec![vec!["1"]]);
    assert_eq!(scalar(&mut c, "SELECT @@TRANCOUNT"), "0");
    exec(&mut c, "BEGIN TRAN");
    assert_eq!(scalar(&mut c, "SELECT @@TRANCOUNT"), "1");
    exec(&mut c, "BEGIN TRAN");
    assert_eq!(scalar(&mut c, "SELECT @@TRANCOUNT"), "2");
    exec(&mut c, "INSERT t VALUES (3)");
    exec(&mut c, "COMMIT");
    assert_eq!(scalar(&mut c, "SELECT @@TRANCOUNT"), "1");
    exec(&mut c, "ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT @@TRANCOUNT"), "0");
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM t"), vec![vec!["1"]]);
    assert_eq!(err_no(&mut c, "COMMIT"), 3902);
    assert_eq!(err_no(&mut c, "ROLLBACK"), 3903);
}

#[test]
fn savepoints_and_ddl_rollback() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT)");
    exec(&mut c, "BEGIN TRAN; INSERT t VALUES (1); SAVE TRAN sp1; INSERT t VALUES (2); SAVE TRAN sp2; INSERT t VALUES (3); ROLLBACK TRAN sp1; INSERT t VALUES (4); COMMIT");
    assert_eq!(rows(&mut c, "SELECT a FROM t ORDER BY a"), vec![vec!["1"], vec!["4"]]);
    // DDL is transactional
    exec(&mut c, "BEGIN TRAN; CREATE TABLE gone (x INT); INSERT gone VALUES (1); ROLLBACK");
    assert_eq!(err_no(&mut c, "SELECT * FROM gone"), 208);
    exec(&mut c, "BEGIN TRAN; DROP TABLE t; ROLLBACK");
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM t"), vec![vec!["2"]]);
    // try/catch + xact_state pattern
    exec(&mut c, "BEGIN TRY BEGIN TRAN; INSERT t VALUES (9); SELECT 1/0; COMMIT END TRY BEGIN CATCH IF @@TRANCOUNT > 0 ROLLBACK END CATCH");
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM t"), vec![vec!["2"]]);
    assert_eq!(scalar(&mut c, "SELECT @@TRANCOUNT"), "0");
}

#[test]
fn stored_procedures() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT IDENTITY, v INT)");
    script(&mut c, "CREATE PROCEDURE add_row @v INT, @id INT OUTPUT AS BEGIN INSERT t (v) VALUES (@v); SET @id = SCOPE_IDENTITY(); RETURN 7 END
GO
CREATE PROCEDURE with_default @a INT = 10, @b INT = 20 AS SELECT @a + @b
GO
CREATE PROCEDURE lister AS SELECT v FROM t ORDER BY id
GO");
    assert_eq!(rows(&mut c, "DECLARE @id INT, @rc INT; EXEC @rc = add_row 42, @id OUTPUT; SELECT @id, @rc"), vec![vec!["1", "7"]]);
    assert_eq!(rows(&mut c, "DECLARE @id INT; EXEC add_row @v = 5, @id = @id OUTPUT; SELECT @id"), vec![vec!["2"]]);
    assert_eq!(rows(&mut c, "EXEC with_default"), vec![vec!["30"]]);
    assert_eq!(rows(&mut c, "EXEC with_default 1"), vec![vec!["21"]]);
    assert_eq!(rows(&mut c, "EXEC with_default @b = 1"), vec![vec!["11"]]);
    assert_eq!(rows(&mut c, "EXECUTE dbo.lister"), vec![vec!["42"], vec!["5"]]);
    assert_eq!(err_no(&mut c, "EXEC add_row"), 201);
    assert_eq!(err_no(&mut c, "EXEC nosuchproc"), 2812);
    assert_eq!(err_no(&mut c, "EXEC with_default @zzz = 1"), 8145);
    // INSERT ... EXEC
    exec(&mut c, "CREATE TABLE copy (v INT)");
    exec(&mut c, "INSERT copy EXEC lister");
    assert_eq!(rows(&mut c, "SELECT SUM(v) FROM copy"), vec![vec!["47"]]);
    // ALTER and DROP
    script(&mut c, "ALTER PROCEDURE with_default AS SELECT 'altered'
GO");
    assert_eq!(rows(&mut c, "EXEC with_default"), vec![vec!["altered"]]);
    exec(&mut c, "DROP PROCEDURE with_default");
    assert_eq!(err_no(&mut c, "EXEC with_default"), 2812);
    script(&mut c, "CREATE OR ALTER PROCEDURE oa AS SELECT 1
GO
CREATE OR ALTER PROCEDURE oa AS SELECT 2
GO");
    assert_eq!(rows(&mut c, "EXEC oa"), vec![vec!["2"]]);
}

#[test]
fn recursive_and_nested_procedures() {
    let mut c = conn();
    script(&mut c, "CREATE PROCEDURE fact @n INT, @r BIGINT OUTPUT AS BEGIN IF @n <= 1 SET @r = 1 ELSE BEGIN DECLARE @sub BIGINT; DECLARE @m INT = @n - 1; EXEC fact @m, @sub OUTPUT; SET @r = @n * @sub END END
GO");
    assert_eq!(rows(&mut c, "DECLARE @x BIGINT; EXEC fact 10, @x OUTPUT; SELECT @x"), vec![vec!["3628800"]]);
    script(&mut c, "CREATE PROCEDURE inf @n INT AS EXEC inf @n
GO");
    assert_eq!(err_no(&mut c, "EXEC inf 1"), 217);
}

#[test]
fn user_defined_functions() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT, b INT)");
    exec(&mut c, "INSERT t VALUES (1,2),(3,4)");
    script(&mut c, "CREATE FUNCTION dbo.add2 (@x INT, @y INT = 100) RETURNS INT AS BEGIN RETURN @x + @y END
GO
CREATE FUNCTION dbo.rowsfor (@min INT) RETURNS TABLE AS RETURN (SELECT a, b FROM t WHERE a >= @min)
GO
CREATE FUNCTION dbo.multi (@n INT) RETURNS @r TABLE (i INT, sq INT) AS BEGIN DECLARE @i INT = 1; WHILE @i <= @n BEGIN INSERT @r VALUES (@i, @i * @i); SET @i += 1 END; RETURN END
GO
CREATE FUNCTION dbo.cat (@n INT) RETURNS VARCHAR(100) AS BEGIN DECLARE @s VARCHAR(100) = ''; DECLARE @i INT = 1; WHILE @i <= @n BEGIN SET @s = @s + CAST(@i AS VARCHAR(3)); SET @i += 1 END; RETURN @s END
GO");
    assert_eq!(rows(&mut c, "SELECT dbo.add2(1, 2)"), vec![vec!["3"]]);
    assert_eq!(rows(&mut c, "SELECT dbo.add2(1, DEFAULT)"), vec![vec!["101"]]);
    assert_eq!(rows(&mut c, "SELECT dbo.add2(a, b) FROM t ORDER BY a"), vec![vec!["3"], vec!["7"]]);
    assert_eq!(rows(&mut c, "SELECT b FROM dbo.rowsfor(2)"), vec![vec!["4"]]);
    assert_eq!(rows(&mut c, "SELECT t.a, f.b FROM t CROSS APPLY dbo.rowsfor(t.a) f ORDER BY t.a"), vec![vec!["1", "2"], vec!["1", "4"], vec!["3", "4"]]);
    assert_eq!(rows(&mut c, "SELECT SUM(sq) FROM dbo.multi(4)"), vec![vec!["30"]]);
    assert_eq!(rows(&mut c, "SELECT dbo.cat(5)"), vec![vec!["12345"]]);
    assert_eq!(err_no(&mut c, "SELECT dbo.nofunc(1)"), 4121);
    exec(&mut c, "DROP FUNCTION dbo.add2");
    assert_eq!(err_no(&mut c, "SELECT dbo.add2(1,2)"), 4121);
}

#[test]
fn dynamic_sql() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT)");
    exec(&mut c, "EXEC('INSERT t VALUES (1)')");
    exec(&mut c, "DECLARE @sql NVARCHAR(100) = N'INSERT t VALUES (2)'; EXEC(@sql)");
    assert_eq!(rows(&mut c, "EXEC('SELECT COUNT(*) FROM ' + 't')"), vec![vec!["2"]]);
    assert_eq!(rows(&mut c, "EXEC sp_executesql N'SELECT @x + @y', N'@x INT, @y INT', 3, 4"), vec![vec!["7"]]);
    assert_eq!(rows(&mut c, "DECLARE @out INT; EXEC sp_executesql N'SELECT @o = COUNT(*) FROM t', N'@o INT OUTPUT', @o = @out OUTPUT; SELECT @out"), vec![vec!["2"]]);
    // RETURN in the statement is the return status
    assert_eq!(rows(&mut c, "DECLARE @rc INT; EXEC @rc = sp_executesql N'RETURN 42'; SELECT @rc"), vec![vec!["42"]]);
    assert_eq!(rows(&mut c, "DECLARE @rc INT; EXEC @rc = sp_executesql N'DECLARE @z INT = 1'; SELECT @rc"), vec![vec!["0"]]);
    assert_eq!(rows(&mut c, "DECLARE @rc INT, @o INT; EXEC @rc = sp_executesql N'SET @o = @i * 2; RETURN @o + 1', N'@i INT, @o INT OUTPUT', 5, @o OUTPUT; SELECT @rc, @o"), vec![vec!["11", "10"]]);
    // dynamic SQL cannot see outer variables
    assert_eq!(err_no(&mut c, "DECLARE @v INT = 1; EXEC('SELECT @v')"), 137);
    // but sees temp tables
    exec(&mut c, "CREATE TABLE #tmp (z INT)");
    exec(&mut c, "EXEC('INSERT #tmp VALUES (5)')");
    assert_eq!(rows(&mut c, "SELECT z FROM #tmp"), vec![vec!["5"]]);
    assert_eq!(err_no(&mut c, "EXEC('SELEC 1')"), 102);
}

#[test]
fn cursors() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT, name VARCHAR(5))");
    exec(&mut c, "INSERT t VALUES (1,'a'),(2,'b'),(3,'c')");
    let m = msgs(&mut c, "DECLARE cur CURSOR FOR SELECT id, name FROM t ORDER BY id;
        DECLARE @id INT, @n VARCHAR(5);
        OPEN cur;
        FETCH NEXT FROM cur INTO @id, @n;
        WHILE @@FETCH_STATUS = 0
        BEGIN
            PRINT CAST(@id AS VARCHAR(3)) + ':' + @n;
            FETCH NEXT FROM cur INTO @id, @n;
        END;
        CLOSE cur; DEALLOCATE cur;");
    assert_eq!(m, vec!["1:a", "2:b", "3:c"]);
    assert_eq!(rows(&mut c, "DECLARE c2 CURSOR SCROLL FOR SELECT id FROM t ORDER BY id; OPEN c2; FETCH LAST FROM c2; CLOSE c2; DEALLOCATE c2"), vec![vec!["3"]]);
    assert_eq!(rows(&mut c, "DECLARE c3 CURSOR SCROLL FOR SELECT id FROM t ORDER BY id; DECLARE @v INT; OPEN c3; FETCH ABSOLUTE 2 FROM c3 INTO @v; SELECT @v; CLOSE c3; DEALLOCATE c3"), vec![vec!["2"]]);
    assert_eq!(err_no(&mut c, "FETCH NEXT FROM nocursor"), 16916);
}

#[test]
fn error_message_fidelity() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT NOT NULL, b VARCHAR(2))");
    assert_eq!(err(&mut c, "INSERT t (a) VALUES (NULL)").message, "Cannot insert the value NULL into column 'a', table 'master.dbo.t'; column does not allow nulls. INSERT fails.");
    assert_eq!(err(&mut c, "SELECT * FROM nothing").message, "Invalid object name 'nothing'.");
    assert_eq!(err(&mut c, "SELECT zzz FROM t").message, "Invalid column name 'zzz'.");
    assert_eq!(err(&mut c, "SELECT 1/0").message, "Divide by zero error encountered.");
    assert_eq!(err(&mut c, "SELECT @q").message, "Must declare the scalar variable \"@q\".");
    assert_eq!(err(&mut c, "INSERT t VALUES (1, 'abc')").message, "String or binary data would be truncated in table 'master.dbo.t', column 'b'. Truncated value: 'ab'.");
    assert_eq!(err(&mut c, "SELEC 1").message, "Incorrect syntax near 'SELEC'.");
    assert_eq!(err(&mut c, "CREATE TABLE t (x INT)").message, "There is already an object named 't' in the database.");
}

#[test]
fn use_and_databases() {
    let mut c = conn();
    exec(&mut c, "CREATE DATABASE shop");
    exec(&mut c, "USE shop");
    assert_eq!(scalar(&mut c, "SELECT DB_NAME()"), "shop");
    exec(&mut c, "CREATE TABLE items (id INT)");
    exec(&mut c, "INSERT items VALUES (1)");
    exec(&mut c, "USE master");
    assert_eq!(err_no(&mut c, "SELECT * FROM items"), 208);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shop.dbo.items"), "1");
    assert_eq!(err_no(&mut c, "USE nodb"), 911);
    exec(&mut c, "DROP DATABASE shop");
    assert_eq!(err_no(&mut c, "USE shop"), 911);
    assert_eq!(err_no(&mut c, "CREATE DATABASE master"), 1801);
}

#[test]
fn schemas_sequences_and_synonym_free_names() {
    let mut c = conn();
    exec(&mut c, "CREATE SCHEMA hr");
    exec(&mut c, "CREATE TABLE hr.emp (id INT)");
    exec(&mut c, "INSERT hr.emp VALUES (1)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM hr.emp"), "1");
    assert_eq!(err_no(&mut c, "SELECT * FROM emp"), 208);
    assert_eq!(err_no(&mut c, "CREATE TABLE nosuch.t (id INT)"), 2760);
    exec(&mut c, "CREATE SEQUENCE seq1 START WITH 10 INCREMENT BY 5");
    assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR seq1"), "10");
    assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR seq1"), "15");
    exec(&mut c, "CREATE TABLE s (id INT DEFAULT NEXT VALUE FOR seq1, v INT)");
    exec(&mut c, "INSERT s (v) VALUES (1)");
    assert_eq!(scalar(&mut c, "SELECT id FROM s"), "20");
    exec(&mut c, "ALTER SEQUENCE seq1 RESTART WITH 1");
    assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR seq1"), "1");
    exec(&mut c, "CREATE SEQUENCE cyc AS INT START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 2 CYCLE");
    assert_eq!(rows(&mut c, "SELECT NEXT VALUE FOR cyc, NEXT VALUE FOR cyc, NEXT VALUE FOR cyc"), vec![vec!["1", "2", "1"]]);
    exec(&mut c, "CREATE SEQUENCE lim START WITH 1 INCREMENT BY 1 MAXVALUE 1");
    exec(&mut c, "SELECT NEXT VALUE FOR lim");
    assert_eq!(err_no(&mut c, "SELECT NEXT VALUE FOR lim"), 11728);
    // sequences are not rolled back
    exec(&mut c, "BEGIN TRAN; SELECT NEXT VALUE FOR seq1; ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR seq1"), "11");
}

#[test]
fn views() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT, b INT)");
    exec(&mut c, "INSERT t VALUES (1,10),(2,20),(3,30)");
    script(&mut c, "CREATE VIEW big AS SELECT a, b FROM t WHERE b >= 20
GO
CREATE VIEW agg (total) AS SELECT SUM(b) FROM big
GO");
    assert_eq!(rows(&mut c, "SELECT a FROM big ORDER BY a"), vec![vec!["2"], vec!["3"]]);
    assert_eq!(rows(&mut c, "SELECT total FROM agg"), vec![vec!["50"]]);
    exec(&mut c, "INSERT t VALUES (4,40)");
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM big"), vec![vec!["3"]]);
    assert_eq!(err_no(&mut c, "CREATE VIEW broken AS SELECT * FROM nope"), 208);
    script(&mut c, "ALTER VIEW big AS SELECT a FROM t WHERE a > 3
GO");
    assert_eq!(rows(&mut c, "SELECT * FROM big"), vec![vec!["4"]]);
    exec(&mut c, "DROP VIEW big");
    assert_eq!(err_no(&mut c, "SELECT * FROM big"), 208);
}
