-- Dynamic SQL: EXEC(string), sp_executesql, scoping, injection, quoting, nesting, errors.
EXEC tSQLt.NewTestClass 'DynamicSqlTests';


GO

CREATE PROCEDURE DynamicSqlTests.[test EXEC of a string returns its result set]
AS
BEGIN
    CREATE TABLE #r (x INT);
    INSERT #r EXEC('SELECT 42');
    DECLARE @a INT = (SELECT x FROM #r);
    EXEC tSQLt.AssertEquals 42, @a;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test sp_executesql passes an input parameter]
AS
BEGIN
    CREATE TABLE #r (x INT);
    INSERT #r EXEC sp_executesql N'SELECT @p + 1', N'@p INT', @p = 41;
    DECLARE @a INT = (SELECT x FROM #r);
    EXEC tSQLt.AssertEquals 42, @a;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test sp_executesql returns an output parameter]
AS
BEGIN
    DECLARE @out INT;
    EXEC sp_executesql N'SET @o = @i * 2', N'@i INT, @o INT OUTPUT', @i = 21, @o = @out OUTPUT;
    EXEC tSQLt.AssertEquals 42, @out;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test sp_executesql with several parameters of different types]
AS
BEGIN
    DECLARE @s NVARCHAR(50);
    EXEC sp_executesql N'SET @r = @a + N''-'' + CAST(@b AS NVARCHAR(10)) + N''-'' + CONVERT(NVARCHAR(10), @c, 23)', N'@a NVARCHAR(10), @b INT, @c DATE, @r NVARCHAR(50) OUTPUT', @a = N'x', @b = 7, @c = '2024-02-29', @r = @s OUTPUT;
    EXEC tSQLt.AssertEqualsString N'x-7-2024-02-29', @s;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test an output parameter keeps its old value when the statement does not set it]
AS
BEGIN
    DECLARE @out INT = 5;
    EXEC sp_executesql N'SELECT 1', N'@o INT OUTPUT', @o = @out OUTPUT;
    EXEC tSQLt.AssertEquals 5, @out;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test local variables of the caller are not visible inside dynamic SQL]
AS
BEGIN
    DECLARE @x INT = 1;
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 137;
    EXEC('SELECT @x');
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test variables declared in dynamic SQL are gone afterwards]
AS
BEGIN
    EXEC('DECLARE @inner INT = 5');
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 137;
    SELECT @inner;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a temp table of the caller is visible to dynamic SQL]
AS
BEGIN
    CREATE TABLE #o (a INT);
    EXEC('INSERT #o VALUES (7)');
    DECLARE @a INT = (SELECT a FROM #o);
    EXEC tSQLt.AssertEquals 7, @a;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a temp table made in dynamic SQL is dropped when it ends]
AS
BEGIN
    EXEC('CREATE TABLE #made (a INT); INSERT #made VALUES (1)');
    DECLARE @id INT = OBJECT_ID('tempdb..#made');
    EXEC tSQLt.AssertEquals NULL, @id;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test QUOTENAME doubles a closing bracket]
AS
BEGIN
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_s = QUOTENAME(N'a]b');
    EXEC tSQLt.AssertEqualsString N'[a]]b]', @tsqlt_s;
    SET @tsqlt_s = QUOTENAME(N'plain');
    EXEC tSQLt.AssertEqualsString N'[plain]', @tsqlt_s;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test QUOTENAME with a quote character doubles the quote]
AS
BEGIN
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_s = QUOTENAME(N'it''s', '''');
    EXEC tSQLt.AssertEqualsString N'''it''''s''', @tsqlt_s;
    SET @tsqlt_s = QUOTENAME(N'a', '"');
    EXEC tSQLt.AssertEqualsString N'"a"', @tsqlt_s;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a parameter neutralises an injection attempt]
AS
BEGIN
    CREATE TABLE dbo.dyn_victim (name VARCHAR(50));
    INSERT dbo.dyn_victim VALUES ('keep');
    DECLARE @in NVARCHAR(100) = N'x''; DROP TABLE dbo.dyn_victim; --';
    DECLARE @n INT;
    EXEC sp_executesql N'SELECT @c = COUNT(*) FROM dbo.dyn_victim WHERE name = @name', N'@name NVARCHAR(100), @c INT OUTPUT', @name = @in, @c = @n OUTPUT;
    EXEC tSQLt.AssertEquals 0, @n;
    EXEC tSQLt.AssertObjectExists 'dbo.dyn_victim';
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test concatenation lets the same input run an extra statement]
AS
BEGIN
    CREATE TABLE dbo.dyn_victim (name VARCHAR(50));
    DECLARE @in NVARCHAR(100) = N'x''; DROP TABLE dbo.dyn_victim; --';
    DECLARE @sql NVARCHAR(MAX) = N'SELECT * FROM dbo.dyn_victim WHERE name = ''' + @in + N'''';
    EXEC(@sql);
    DECLARE @id INT = OBJECT_ID('dbo.dyn_victim');
    EXEC tSQLt.AssertEquals NULL, @id;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test QUOTENAME protects an identifier built from input]
AS
BEGIN
    CREATE TABLE dbo.dyn_victim (name VARCHAR(50));
    DECLARE @tab NVARCHAR(100) = N'dyn_victim]; DROP TABLE dbo.dyn_victim; --';
    DECLARE @sql NVARCHAR(MAX) = N'SELECT * FROM dbo.' + QUOTENAME(@tab);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 208;
    EXEC(@sql);
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test an unquoted identifier from input runs the extra statement]
AS
BEGIN
    CREATE TABLE dbo.dyn_victim (name VARCHAR(50));
    DECLARE @tab NVARCHAR(100) = N'dyn_victim; DROP TABLE dbo.dyn_victim; --';
    EXEC(N'SELECT * FROM dbo.' + @tab);
    DECLARE @id INT = OBJECT_ID('dbo.dyn_victim');
    EXEC tSQLt.AssertEquals NULL, @id;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a stray closing bracket in an identifier is a syntax error and runs nothing]
AS
BEGIN
    CREATE TABLE dbo.dyn_victim (name VARCHAR(50));
    DECLARE @tab NVARCHAR(100) = N'dyn_victim]; DROP TABLE dbo.dyn_victim; --';
    DECLARE @n INT = 0;
    BEGIN TRY
        EXEC(N'SELECT * FROM dbo.' + @tab);
    END TRY
    BEGIN CATCH
        SET @n = ERROR_NUMBER();
    END CATCH;
    EXEC tSQLt.AssertEquals 102, @n;
    EXEC tSQLt.AssertObjectExists 'dbo.dyn_victim';
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test EXEC of a procedure name held in a variable]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.dyn_p AS RETURN 5');
    DECLARE @p NVARCHAR(100) = N'dbo.dyn_p', @rc INT;
    EXEC @rc = @p;
    EXEC tSQLt.AssertEquals 5, @rc;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test dynamic SQL can create and then call a procedure]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.dyn_q @a INT, @b INT OUTPUT AS SET @b = @a * 3');
    DECLARE @r INT;
    EXEC dbo.dyn_q 4, @r OUTPUT;
    EXEC tSQLt.AssertEquals 12, @r;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test nested dynamic SQL]
AS
BEGIN
    CREATE TABLE #r (x INT);
    INSERT #r EXEC('EXEC(''SELECT 3'')');
    DECLARE @a INT = (SELECT x FROM #r);
    EXEC tSQLt.AssertEquals 3, @a;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test nested sp_executesql keeps parameters apart]
AS
BEGIN
    DECLARE @o INT;
    EXEC sp_executesql N'EXEC sp_executesql N''SET @inner = @x + 1'', N''@x INT, @inner INT OUTPUT'', @x = @p, @inner = @res OUTPUT', N'@p INT, @res INT OUTPUT', @p = 10, @res = @o OUTPUT;
    EXEC tSQLt.AssertEquals 11, @o;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a statement longer than 4000 characters]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @sql NVARCHAR(MAX) = N'SELECT @r = 1' + REPLICATE(CAST(N' ' AS NVARCHAR(MAX)), 5000) + N'+ 4000', @r INT;
    EXEC sp_executesql @sql, N'@r INT OUTPUT', @r = @r OUTPUT;
    EXEC tSQLt.AssertEquals 4001, @r;
    SET @tsqlt_v = CASE WHEN LEN(@sql) > 5000 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test ROWCOUNT after EXEC is that of the last statement inside it]
AS
BEGIN
    CREATE TABLE dbo.dyn_t (a INT);
    INSERT dbo.dyn_t VALUES (1), (2), (3);
    EXEC('DELETE FROM dbo.dyn_t');
    DECLARE @n INT = @@ROWCOUNT;
    EXEC tSQLt.AssertEquals 3, @n;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a parameter that cannot be converted raises 245]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 245;
    EXEC sp_executesql N'SELECT @p', N'@p INT', @p = 'abc';
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a declared parameter that is not supplied raises 8178]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 8178;
    EXEC sp_executesql N'SELECT @p', N'@p INT';
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test sp_executesql default value of a parameter]
AS
BEGIN
    CREATE TABLE #r (x INT);
    INSERT #r EXEC sp_executesql N'SELECT @p', N'@p INT = 5';
    DECLARE @a INT = (SELECT x FROM #r);
    EXEC tSQLt.AssertEquals 5, @a;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a syntax error in dynamic SQL is caught by the caller]
AS
BEGIN
    DECLARE @n INT = 0;
    BEGIN TRY
        EXEC('SELECT 1 +');
    END TRY
    BEGIN CATCH
        SET @n = ERROR_NUMBER();
    END CATCH;
    EXEC tSQLt.AssertEquals 102, @n;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a runtime error in dynamic SQL is caught by the caller]
AS
BEGIN
    DECLARE @n INT = 0, @m NVARCHAR(200);
    BEGIN TRY
        EXEC('SELECT 1 / 0');
    END TRY
    BEGIN CATCH
        SET @n = ERROR_NUMBER();
        SET @m = ERROR_MESSAGE();
    END CATCH;
    EXEC tSQLt.AssertEquals 8134, @n;
    EXEC tSQLt.AssertLike N'Divide by zero%', @m;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a batch with a transaction rolls back with a savepoint]
AS
BEGIN
    CREATE TABLE dbo.dyn_t (a INT);
    SAVE TRAN s1;
    EXEC('INSERT dbo.dyn_t VALUES (1), (2)');
    ROLLBACK TRAN s1;
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.dyn_t);
    EXEC tSQLt.AssertEquals 0, @n;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test USE inside dynamic SQL does not change the caller's database]
AS
BEGIN
    DECLARE @tsqlt_s NVARCHAR(MAX);
    DECLARE @before NVARCHAR(128) = DB_NAME();
    EXEC('USE master');
    SET @tsqlt_s = DB_NAME();
    EXEC tSQLt.AssertEqualsString @before, @tsqlt_s;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test SET DATEFIRST inside dynamic SQL ends with it]
AS
BEGIN
    DECLARE @before INT = @@DATEFIRST;
    EXEC('SET DATEFIRST 3');
    EXEC tSQLt.AssertEquals @before, @@DATEFIRST;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test dynamic SQL sees SET options of the caller]
AS
BEGIN
    SET DATEFIRST 1;
    CREATE TABLE #r (x INT);
    INSERT #r EXEC('SELECT @@DATEFIRST');
    DECLARE @a INT = (SELECT x FROM #r);
    EXEC tSQLt.AssertEquals 1, @a;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test building a column list with STRING_AGG and QUOTENAME]
AS
BEGIN
    CREATE TABLE dbo.dyn_wide (id INT, [a b] INT, [c]]d] INT);
    INSERT dbo.dyn_wide VALUES (1, 2, 3);
    DECLARE @cols NVARCHAR(MAX) = (SELECT STRING_AGG(QUOTENAME(c.name), ', ') WITHIN GROUP (ORDER BY c.column_id) FROM sys.columns c WHERE c.object_id = OBJECT_ID('dbo.dyn_wide'));
    EXEC tSQLt.AssertEqualsString N'[id], [a b], [c]]d]', @cols;
    CREATE TABLE #r (id INT, ab INT, cd INT);
    INSERT #r EXEC('SELECT ' + @cols + ' FROM dbo.dyn_wide');
    DECLARE @sum INT = (SELECT id + ab + cd FROM #r);
    EXEC tSQLt.AssertEquals 6, @sum;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a dynamic PIVOT built from the data]
AS
BEGIN
    CREATE TABLE dbo.dyn_sales (yr INT, q VARCHAR(2), amt INT);
    INSERT dbo.dyn_sales VALUES (2023, 'Q1', 10), (2023, 'Q2', 20), (2024, 'Q1', 5), (2024, 'Q2', 6), (2024, 'Q1', 1);
    DECLARE @cols NVARCHAR(MAX) = (SELECT STRING_AGG(QUOTENAME(q), ', ') WITHIN GROUP (ORDER BY q) FROM (SELECT DISTINCT q FROM dbo.dyn_sales) d);
    DECLARE @sql NVARCHAR(MAX) = N'SELECT yr, ' + @cols + N' FROM (SELECT yr, q, amt FROM dbo.dyn_sales) s PIVOT (SUM(amt) FOR q IN (' + @cols + N')) p';
    CREATE TABLE #actual (yr INT, Q1 INT, Q2 INT);
    INSERT #actual EXEC(@sql);
    CREATE TABLE #expected (yr INT, Q1 INT, Q2 INT);
    INSERT #expected VALUES (2023, 10, 20), (2024, 6, 6);
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test dynamic SQL runs with the permissions of the caller]
AS
BEGIN
    CREATE TABLE dbo.dyn_secret (a INT);
    CREATE USER dyn_limited WITHOUT LOGIN;
    EXECUTE AS USER = 'dyn_limited';
    DECLARE @n INT = 0;
    BEGIN TRY
        EXEC('SELECT * FROM dbo.dyn_secret');
    END TRY
    BEGIN CATCH
        SET @n = ERROR_NUMBER();
    END CATCH;
    REVERT;
    EXEC tSQLt.AssertEquals 229, @n;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test dynamic SQL with a unicode literal and an apostrophe]
AS
BEGIN
    DECLARE @v NVARCHAR(50) = N'O''Brien ' + NCHAR(228);
    DECLARE @sql NVARCHAR(MAX) = N'SELECT @r = LEN(@s)';
    DECLARE @r INT;
    EXEC sp_executesql @sql, N'@s NVARCHAR(50), @r INT OUTPUT', @s = @v, @r = @r OUTPUT;
    EXEC tSQLt.AssertEquals 9, @r;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test dynamic SQL inside a loop sees each iteration's value]
AS
BEGIN
    CREATE TABLE dbo.dyn_loop (i INT);
    DECLARE @i INT = 1;
    WHILE @i <= 5
    BEGIN
        DECLARE @sql NVARCHAR(100) = N'INSERT dbo.dyn_loop VALUES (' + CAST(@i * @i AS NVARCHAR(10)) + N')';
        EXEC(@sql);
        SET @i = @i + 1;
    END;
    DECLARE @sum INT = (SELECT SUM(i) FROM dbo.dyn_loop);
    EXEC tSQLt.AssertEquals 55, @sum;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test the same text with different values gives different answers every time]
AS
BEGIN
    DECLARE @a INT, @b INT;
    EXEC sp_executesql N'SELECT @r = @v * 2', N'@v INT, @r INT OUTPUT', @v = 3, @r = @a OUTPUT;
    EXEC sp_executesql N'SELECT @r = @v * 2', N'@v INT, @r INT OUTPUT', @v = 4, @r = @b OUTPUT;
    EXEC tSQLt.AssertEquals 6, @a;
    EXEC tSQLt.AssertEquals 8, @b;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test dynamic SQL notices a table that changed shape between calls]
AS
BEGIN
    CREATE TABLE dbo.dyn_shape (a INT);
    INSERT dbo.dyn_shape VALUES (1);
    DECLARE @r1 INT, @r2 INT;
    EXEC sp_executesql N'SELECT @r = COUNT(*) FROM dbo.dyn_shape', N'@r INT OUTPUT', @r = @r1 OUTPUT;
    DROP TABLE dbo.dyn_shape;
    CREATE TABLE dbo.dyn_shape (a INT, b INT);
    INSERT dbo.dyn_shape VALUES (1, 2), (3, 4);
    EXEC sp_executesql N'SELECT @r = COUNT(*) FROM dbo.dyn_shape', N'@r INT OUTPUT', @r = @r2 OUTPUT;
    EXEC tSQLt.AssertEquals 1, @r1;
    EXEC tSQLt.AssertEquals 2, @r2;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test an empty string is a valid dynamic batch]
AS
BEGIN
    EXEC('');
    EXEC sp_executesql N'';
    EXEC tSQLt.AssertEquals 1, 1;
END;


GO

CREATE PROCEDURE DynamicSqlTests.[test a NULL statement raises an error]
AS
BEGIN
    DECLARE @sql NVARCHAR(MAX) = NULL;
    EXEC tSQLt.ExpectException;
    EXEC sp_executesql @sql;
END;


GO
