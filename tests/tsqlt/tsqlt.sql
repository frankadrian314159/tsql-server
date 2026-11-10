-- A tSQLt-compatible unit-testing framework written in T-SQL.
--
-- The real tSQLt (https://tsqlt.org) needs CLR assemblies for parts of its implementation, which this server does
-- not have. This file provides the part of its public API that test suites use: test classes (schemas whose
-- procedures named test* are the test cases, with an optional SetUp), Run / RunAll / RunTestClass, the Assert*
-- procedures, ExpectException / ExpectNoException, FakeTable and SpyProcedure. Each test runs inside a transaction
-- that is rolled back, so FakeTable and SpyProcedure (which rename and replace objects) leave nothing behind.
--
-- A test fails (as opposed to erroring) when an assertion raises error 50000 with state 10, as in tSQLt.
CREATE SCHEMA tSQLt;
GO
CREATE TABLE tSQLt.TestClasses (Name NVARCHAR(300) NOT NULL PRIMARY KEY);
GO
CREATE TABLE tSQLt.TestResult (
    Id INT IDENTITY(1,1) PRIMARY KEY,
    Class NVARCHAR(300),
    TestCase NVARCHAR(300),
    Name NVARCHAR(600),
    Result NVARCHAR(20),
    Msg NVARCHAR(MAX),
    TestStartTime DATETIME,
    TestEndTime DATETIME
);
GO
CREATE TABLE tSQLt.Private_Expectation (
    Id INT NOT NULL PRIMARY KEY,
    ExpectException BIT NOT NULL,
    Msg NVARCHAR(MAX) NULL,
    Pattern NVARCHAR(MAX) NULL,
    Number INT NULL,
    Severity INT NULL,
    State INT NULL
);
GO
CREATE PROCEDURE tSQLt.Fail @Message NVARCHAR(MAX) = ''
AS
BEGIN
    RAISERROR('%s', 16, 10, @Message);
END;
GO
CREATE PROCEDURE tSQLt.NewTestClass @ClassName NVARCHAR(300)
AS
BEGIN
    EXEC tSQLt.DropClass @ClassName;
    EXEC('CREATE SCHEMA ' + @ClassName);
    INSERT INTO tSQLt.TestClasses (Name) VALUES (@ClassName);
END;
GO
CREATE PROCEDURE tSQLt.DropClass @ClassName NVARCHAR(300)
AS
BEGIN
    DECLARE @schema NVARCHAR(300) = PARSENAME(@ClassName, 1);
    IF SCHEMA_ID(@schema) IS NULL RETURN 0;
    DECLARE @objs TABLE (Id INT IDENTITY(1,1), Ddl NVARCHAR(MAX));
    INSERT INTO @objs (Ddl)
        SELECT 'DROP ' + CASE o.type WHEN 'P' THEN 'PROCEDURE' WHEN 'V' THEN 'VIEW' WHEN 'U' THEN 'TABLE' WHEN 'FN' THEN 'FUNCTION' WHEN 'IF' THEN 'FUNCTION' WHEN 'TF' THEN 'FUNCTION' END
               + ' ' + QUOTENAME(s.name) + '.' + QUOTENAME(o.name)
        FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id
        WHERE s.name = @schema AND o.type IN ('P', 'V', 'U', 'FN', 'IF', 'TF');
    DECLARE @i INT = 1, @n INT = (SELECT COUNT(*) FROM @objs), @ddl NVARCHAR(MAX);
    WHILE @i <= @n
    BEGIN
        SELECT @ddl = Ddl FROM @objs WHERE Id = @i;
        EXEC(@ddl);
        SET @i = @i + 1;
    END;
    EXEC('DROP SCHEMA ' + @schema);
    DELETE FROM tSQLt.TestClasses WHERE Name = @schema;
END;
GO
-- Run one test case (class, procedure name) in its own transaction.
CREATE PROCEDURE tSQLt.Private_RunTest @Class NVARCHAR(300), @TestProc NVARCHAR(300)
AS
BEGIN
    DECLARE @full NVARCHAR(700) = QUOTENAME(@Class) + '.' + QUOTENAME(@TestProc);
    DECLARE @setup NVARCHAR(700) = QUOTENAME(@Class) + '.' + QUOTENAME('SetUp');
    DECLARE @result NVARCHAR(20) = 'Success', @msg NVARCHAR(MAX) = '', @start DATETIME = GETDATE();
    DECLARE @en INT, @es INT, @est INT, @em NVARCHAR(MAX);
    DELETE FROM tSQLt.Private_Expectation;
    BEGIN TRAN;
    BEGIN TRY
        IF OBJECT_ID(@setup, 'P') IS NOT NULL EXEC @setup;
        EXEC @full;
        IF EXISTS (SELECT 1 FROM tSQLt.Private_Expectation WHERE ExpectException = 1)
        BEGIN
            SET @result = 'Failure';
            SET @msg = 'Expected an error to be raised, but none was.';
        END;
    END TRY
    BEGIN CATCH
        SET @en = ERROR_NUMBER();
        SET @es = ERROR_SEVERITY();
        SET @est = ERROR_STATE();
        SET @em = ERROR_MESSAGE();
        IF EXISTS (SELECT 1 FROM tSQLt.Private_Expectation WHERE ExpectException = 1)
        BEGIN
            DECLARE @why NVARCHAR(MAX) = '';
            SELECT @why = @why
                + CASE WHEN Msg IS NOT NULL AND ISNULL(@em, '') <> Msg THEN 'Expected message <' + Msg + '> but was <' + ISNULL(@em, 'NULL') + '>. ' ELSE '' END
                + CASE WHEN Pattern IS NOT NULL AND ISNULL(@em, '') NOT LIKE Pattern THEN 'Expected message like <' + Pattern + '> but was <' + ISNULL(@em, 'NULL') + '>. ' ELSE '' END
                + CASE WHEN Number IS NOT NULL AND @en <> Number THEN 'Expected error number ' + CAST(Number AS NVARCHAR(20)) + ' but was ' + CAST(@en AS NVARCHAR(20)) + '. ' ELSE '' END
                + CASE WHEN Severity IS NOT NULL AND @es <> Severity THEN 'Expected severity ' + CAST(Severity AS NVARCHAR(20)) + ' but was ' + CAST(@es AS NVARCHAR(20)) + '. ' ELSE '' END
                + CASE WHEN State IS NOT NULL AND @est <> State THEN 'Expected state ' + CAST(State AS NVARCHAR(20)) + ' but was ' + CAST(@est AS NVARCHAR(20)) + '. ' ELSE '' END
            FROM tSQLt.Private_Expectation WHERE ExpectException = 1;
            IF @why <> '' BEGIN SET @result = 'Failure'; SET @msg = @why; END;
        END
        ELSE IF @en = 50000 AND @est = 10
        BEGIN
            SET @result = 'Failure';
            SET @msg = @em;
        END
        ELSE
        BEGIN
            SET @result = 'Error';
            SET @msg = @em + ' [Error ' + CAST(@en AS NVARCHAR(20)) + ', Severity ' + CAST(@es AS NVARCHAR(20)) + ', State ' + CAST(@est AS NVARCHAR(20)) + ']';
        END;
    END CATCH;
    IF @@TRANCOUNT > 0 ROLLBACK TRAN;
    INSERT INTO tSQLt.TestResult (Class, TestCase, Name, Result, Msg, TestStartTime, TestEndTime)
    VALUES (@Class, @TestProc, @full, @result, @msg, @start, GETDATE());
END;
GO
CREATE PROCEDURE tSQLt.RunTestClass @TestClassName NVARCHAR(300)
AS
BEGIN
    DECLARE @tests TABLE (Id INT IDENTITY(1,1), Proc_ NVARCHAR(300));
    INSERT INTO @tests (Proc_)
        SELECT p.name FROM sys.procedures p JOIN sys.schemas s ON s.schema_id = p.schema_id
        WHERE s.name = @TestClassName AND p.name LIKE 'test%' ORDER BY p.name;
    DECLARE @i INT = 1, @n INT = (SELECT COUNT(*) FROM @tests), @p NVARCHAR(300);
    WHILE @i <= @n
    BEGIN
        SELECT @p = Proc_ FROM @tests WHERE Id = @i;
        EXEC tSQLt.Private_RunTest @TestClassName, @p;
        SET @i = @i + 1;
    END;
END;
GO
CREATE PROCEDURE tSQLt.RunAll
AS
BEGIN
    DELETE FROM tSQLt.TestResult;
    DECLARE @classes TABLE (Id INT IDENTITY(1,1), Name NVARCHAR(300));
    INSERT INTO @classes (Name) SELECT Name FROM tSQLt.TestClasses ORDER BY Name;
    DECLARE @i INT = 1, @n INT = (SELECT COUNT(*) FROM @classes), @c NVARCHAR(300);
    WHILE @i <= @n
    BEGIN
        SELECT @c = Name FROM @classes WHERE Id = @i;
        EXEC tSQLt.RunTestClass @c;
        SET @i = @i + 1;
    END;
END;
GO
-- tSQLt.Run: a test class, a single test ('Class.test name'), or everything
CREATE PROCEDURE tSQLt.Run @TestName NVARCHAR(MAX) = NULL
AS
BEGIN
    IF @TestName IS NULL
    BEGIN
        EXEC tSQLt.RunAll;
        RETURN 0;
    END;
    DELETE FROM tSQLt.TestResult;
    IF EXISTS (SELECT 1 FROM tSQLt.TestClasses WHERE Name = PARSENAME(@TestName, 1) AND PARSENAME(@TestName, 2) IS NULL)
    BEGIN
        EXEC tSQLt.RunTestClass @TestName;
        RETURN 0;
    END;
    DECLARE @class NVARCHAR(300) = PARSENAME(@TestName, 2), @proc NVARCHAR(300) = PARSENAME(@TestName, 1);
    EXEC tSQLt.Private_RunTest @class, @proc;
END;
GO
-- ---------------------------------------------------------------- assertions
CREATE PROCEDURE tSQLt.AssertEquals @Expected SQL_VARIANT, @Actual SQL_VARIANT, @Message NVARCHAR(MAX) = ''
AS
BEGIN
    IF (@Expected = @Actual) OR (@Actual IS NULL AND @Expected IS NULL) RETURN 0;
    DECLARE @msg NVARCHAR(MAX) = 'Expected: <' + ISNULL(CAST(@Expected AS NVARCHAR(MAX)), 'NULL') + '> but was: <' + ISNULL(CAST(@Actual AS NVARCHAR(MAX)), 'NULL') + '>';
    IF @Message <> '' SET @msg = @Message + ' ' + @msg;
    RAISERROR('%s', 16, 10, @msg);
END;
GO
CREATE PROCEDURE tSQLt.AssertNotEquals @Expected SQL_VARIANT, @Actual SQL_VARIANT, @Message NVARCHAR(MAX) = ''
AS
BEGIN
    IF (@Expected <> @Actual) OR (@Actual IS NULL AND @Expected IS NOT NULL) OR (@Actual IS NOT NULL AND @Expected IS NULL) RETURN 0;
    DECLARE @msg NVARCHAR(MAX) = 'Expected actual value to not be <' + ISNULL(CAST(@Expected AS NVARCHAR(MAX)), 'NULL') + '>';
    IF @Message <> '' SET @msg = @Message + ' ' + @msg;
    RAISERROR('%s', 16, 10, @msg);
END;
GO
CREATE PROCEDURE tSQLt.AssertEqualsString @Expected NVARCHAR(MAX), @Actual NVARCHAR(MAX), @Message NVARCHAR(MAX) = ''
AS
BEGIN
    IF (@Expected = @Actual) OR (@Actual IS NULL AND @Expected IS NULL) RETURN 0;
    DECLARE @msg NVARCHAR(MAX) = 'Expected: <' + ISNULL(@Expected, 'NULL') + '> but was: <' + ISNULL(@Actual, 'NULL') + '>';
    IF @Message <> '' SET @msg = @Message + ' ' + @msg;
    RAISERROR('%s', 16, 10, @msg);
END;
GO
CREATE PROCEDURE tSQLt.AssertLike @ExpectedPattern NVARCHAR(MAX), @Actual NVARCHAR(MAX), @Message NVARCHAR(MAX) = ''
AS
BEGIN
    IF @Actual LIKE @ExpectedPattern RETURN 0;
    DECLARE @msg NVARCHAR(MAX) = 'Expected: <' + ISNULL(@ExpectedPattern, 'NULL') + '> but was: <' + ISNULL(@Actual, 'NULL') + '>';
    IF @Message <> '' SET @msg = @Message + ' ' + @msg;
    RAISERROR('%s', 16, 10, @msg);
END;
GO
CREATE PROCEDURE tSQLt.AssertObjectExists @ObjectName NVARCHAR(MAX), @Message NVARCHAR(MAX) = ''
AS
BEGIN
    IF OBJECT_ID(@ObjectName) IS NOT NULL RETURN 0;
    DECLARE @msg NVARCHAR(MAX) = 'Object <' + @ObjectName + '> does not exist';
    IF @Message <> '' SET @msg = @Message + ' ' + @msg;
    RAISERROR('%s', 16, 10, @msg);
END;
GO
CREATE PROCEDURE tSQLt.AssertEmptyTable @TableName NVARCHAR(MAX), @Message NVARCHAR(MAX) = ''
AS
BEGIN
    DECLARE @n INT, @sql NVARCHAR(MAX) = 'SELECT @c = COUNT(*) FROM ' + @TableName;
    EXEC sp_executesql @sql, N'@c INT OUTPUT', @c = @n OUTPUT;
    IF @n = 0 RETURN 0;
    DECLARE @msg NVARCHAR(MAX) = 'Table <' + @TableName + '> is not empty (' + CAST(@n AS NVARCHAR(20)) + ' rows)';
    IF @Message <> '' SET @msg = @Message + ' ' + @msg;
    RAISERROR('%s', 16, 10, @msg);
END;
GO
-- the two tables (or temporary tables) must hold the same rows; duplicates count
CREATE PROCEDURE tSQLt.AssertEqualsTable @Expected NVARCHAR(MAX), @Actual NVARCHAR(MAX), @FailMsg NVARCHAR(MAX) = ''
AS
BEGIN
    DECLARE @a INT, @b INT, @n1 INT, @n2 INT, @sql NVARCHAR(MAX);
    SET @sql = 'SELECT @r = COUNT(*) FROM (SELECT * FROM ' + @Expected + ' EXCEPT SELECT * FROM ' + @Actual + ') x';
    EXEC sp_executesql @sql, N'@r INT OUTPUT', @r = @a OUTPUT;
    SET @sql = 'SELECT @r = COUNT(*) FROM (SELECT * FROM ' + @Actual + ' EXCEPT SELECT * FROM ' + @Expected + ') x';
    EXEC sp_executesql @sql, N'@r INT OUTPUT', @r = @b OUTPUT;
    SET @sql = 'SELECT @r = COUNT(*) FROM ' + @Expected;
    EXEC sp_executesql @sql, N'@r INT OUTPUT', @r = @n1 OUTPUT;
    SET @sql = 'SELECT @r = COUNT(*) FROM ' + @Actual;
    EXEC sp_executesql @sql, N'@r INT OUTPUT', @r = @n2 OUTPUT;
    IF @a = 0 AND @b = 0 AND @n1 = @n2 RETURN 0;
    DECLARE @msg NVARCHAR(MAX) = 'Unexpected/missing resultset rows: ' + CAST(@a AS NVARCHAR(20)) + ' expected row(s) missing, ' + CAST(@b AS NVARCHAR(20)) + ' unexpected row(s), ' + CAST(@n1 AS NVARCHAR(20)) + ' expected in all and ' + CAST(@n2 AS NVARCHAR(20)) + ' actual';
    IF @FailMsg <> '' SET @msg = @FailMsg + ' ' + @msg;
    RAISERROR('%s', 16, 10, @msg);
END;
GO
-- ------------------------------------------------------------- expectations
CREATE PROCEDURE tSQLt.ExpectException
    @ExpectedMessage NVARCHAR(MAX) = NULL, @ExpectedSeverity INT = NULL, @ExpectedState INT = NULL,
    @Message NVARCHAR(MAX) = NULL, @ExpectedMessagePattern NVARCHAR(MAX) = NULL, @ExpectedErrorNumber INT = NULL
AS
BEGIN
    DELETE FROM tSQLt.Private_Expectation;
    INSERT INTO tSQLt.Private_Expectation (Id, ExpectException, Msg, Pattern, Number, Severity, State)
    VALUES (1, 1, ISNULL(@ExpectedMessage, NULL), @ExpectedMessagePattern, @ExpectedErrorNumber, @ExpectedSeverity, @ExpectedState);
END;
GO
CREATE PROCEDURE tSQLt.ExpectNoException @Message NVARCHAR(MAX) = NULL
AS
BEGIN
    DELETE FROM tSQLt.Private_Expectation;
    INSERT INTO tSQLt.Private_Expectation (Id, ExpectException) VALUES (1, 0);
END;
GO
-- --------------------------------------------------------- isolation helpers
-- Replace a table by an empty copy with the same columns and no constraints; the test's rollback restores the original.
CREATE PROCEDURE tSQLt.FakeTable @TableName NVARCHAR(MAX), @SchemaName NVARCHAR(MAX) = NULL
AS
BEGIN
    DECLARE @sch NVARCHAR(300) = ISNULL(@SchemaName, ISNULL(PARSENAME(@TableName, 2), 'dbo')), @tab NVARCHAR(300) = PARSENAME(@TableName, 1);
    DECLARE @old NVARCHAR(300) = @tab + '_tSQLt_Original';
    DECLARE @sql NVARCHAR(MAX) = N'EXEC sp_rename ''' + @sch + '.' + @tab + ''', ''' + @old + '''';
    EXEC(@sql);
    SET @sql = 'SELECT * INTO ' + QUOTENAME(@sch) + '.' + QUOTENAME(@tab) + ' FROM ' + QUOTENAME(@sch) + '.' + QUOTENAME(@old) + ' WHERE 1 = 0';
    EXEC(@sql);
END;
GO
-- Replace a procedure by one that records its arguments in <name>_SpyProcedureLog and runs @CommandToExecute.
CREATE PROCEDURE tSQLt.SpyProcedure @ProcedureName NVARCHAR(MAX), @CommandToExecute NVARCHAR(MAX) = NULL
AS
BEGIN
    DECLARE @sch NVARCHAR(300) = ISNULL(PARSENAME(@ProcedureName, 2), 'dbo'), @proc NVARCHAR(300) = PARSENAME(@ProcedureName, 1);
    DECLARE @oid INT = OBJECT_ID(@ProcedureName, 'P');
    IF @oid IS NULL RAISERROR('Cannot use SpyProcedure on <%s> because the procedure does not exist', 16, 10, @ProcedureName);
    DECLARE @params TABLE (Id INT, Name NVARCHAR(300), Decl NVARCHAR(300), IsOut BIT);
    INSERT INTO @params (Id, Name, Decl, IsOut)
        SELECT pa.parameter_id, pa.name,
               TYPE_NAME(pa.user_type_id) + CASE WHEN TYPE_NAME(pa.user_type_id) IN ('varchar', 'char', 'varbinary', 'binary') THEN '(' + CASE WHEN pa.max_length = -1 THEN 'MAX' ELSE CAST(pa.max_length AS VARCHAR(10)) END + ')'
                                                 WHEN TYPE_NAME(pa.user_type_id) IN ('nvarchar', 'nchar') THEN '(' + CASE WHEN pa.max_length = -1 THEN 'MAX' ELSE CAST(pa.max_length / 2 AS VARCHAR(10)) END + ')'
                                                 WHEN TYPE_NAME(pa.user_type_id) IN ('decimal', 'numeric') THEN '(' + CAST(pa.precision AS VARCHAR(10)) + ',' + CAST(pa.scale AS VARCHAR(10)) + ')'
                                                 ELSE '' END,
               pa.is_output
        FROM sys.parameters pa WHERE pa.object_id = @oid;
    DECLARE @old NVARCHAR(300) = @proc + '_tSQLt_Original', @log NVARCHAR(600) = QUOTENAME(@sch) + '.' + QUOTENAME(@proc + '_SpyProcedureLog');
    DECLARE @sql NVARCHAR(MAX), @decl NVARCHAR(MAX) = '', @cols NVARCHAR(MAX) = '', @vals NVARCHAR(MAX) = '';
    SELECT @decl = @decl + CASE WHEN @decl = '' THEN '' ELSE ', ' END + Name + ' ' + Decl + CASE WHEN IsOut = 1 THEN ' = NULL OUTPUT' ELSE ' = NULL' END,
           @cols = @cols + ', ' + QUOTENAME(SUBSTRING(Name, 2, 300)) + ' ' + Decl + ' NULL',
           @vals = @vals + CASE WHEN @vals = '' THEN '' ELSE ', ' END + Name
    FROM @params ORDER BY Id;
    SET @sql = 'EXEC sp_rename ''' + @sch + '.' + @proc + ''', ''' + @old + '''';
    EXEC(@sql);
    SET @sql = 'CREATE TABLE ' + @log + ' (_id_ INT IDENTITY(1,1) PRIMARY KEY' + @cols + ')';
    EXEC(@sql);
    SET @sql = 'CREATE PROCEDURE ' + QUOTENAME(@sch) + '.' + QUOTENAME(@proc) + CASE WHEN @decl = '' THEN '' ELSE ' ' + @decl END + ' AS BEGIN '
             + 'INSERT INTO ' + @log + CASE WHEN @vals = '' THEN ' DEFAULT VALUES' ELSE ' (' + REPLACE(REPLACE(@vals, '@', ''), ', ', ', ') + ') VALUES (' + @vals + ')' END + '; '
             + ISNULL(@CommandToExecute, '') + ' END';
    EXEC(@sql);
END;
GO
