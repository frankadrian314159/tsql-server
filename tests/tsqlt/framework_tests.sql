-- Tests of the framework itself: assertions, expectations, FakeTable, SpyProcedure and test isolation.
EXEC tSQLt.NewTestClass 'FrameworkTests';


GO

CREATE PROCEDURE FrameworkTests.[test AssertEquals passes for equal values of the same type]
AS
BEGIN
    EXEC tSQLt.AssertEquals 5, 5;
    EXEC tSQLt.AssertEquals 'abc', 'abc';
    EXEC tSQLt.AssertEquals NULL, NULL;
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertEquals fails with expected and actual in the message]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedMessage = 'Expected: <5> but was: <6>', @ExpectedSeverity = 16, @ExpectedState = 10;
    EXEC tSQLt.AssertEquals 5, 6;
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertEquals fails when one side is NULL]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedMessage = 'Expected: <1> but was: <NULL>';
    EXEC tSQLt.AssertEquals 1, NULL;
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertNotEquals passes and fails]
AS
BEGIN
    EXEC tSQLt.AssertNotEquals 1, 2;
    EXEC tSQLt.AssertNotEquals 1, NULL;
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = 'Expected actual value to not be <3>%';
    EXEC tSQLt.AssertNotEquals 3, 3;
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertEqualsString compares text and NULLs]
AS
BEGIN
    EXEC tSQLt.AssertEqualsString N'abc', N'abc';
    EXEC tSQLt.AssertEqualsString NULL, NULL;
    EXEC tSQLt.ExpectException @ExpectedMessage = 'Expected: <abc> but was: <abd>';
    EXEC tSQLt.AssertEqualsString N'abc', N'abd';
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertLike matches a pattern]
AS
BEGIN
    EXEC tSQLt.AssertLike N'ab%', N'abcdef';
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = 'Expected: <x%> but was: <abc>';
    EXEC tSQLt.AssertLike N'x%', N'abc';
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertObjectExists finds tables and procedures]
AS
BEGIN
    EXEC tSQLt.AssertObjectExists 'tSQLt.TestResult';
    EXEC tSQLt.AssertObjectExists 'tSQLt.AssertEquals';
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = 'Object <dbo.nothing_here> does not exist';
    EXEC tSQLt.AssertObjectExists 'dbo.nothing_here';
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertEmptyTable]
AS
BEGIN
    CREATE TABLE dbo.fw_e (a INT);
    EXEC tSQLt.AssertEmptyTable 'dbo.fw_e';
    INSERT dbo.fw_e VALUES (1);
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = 'Table <dbo.fw_e> is not empty (1 rows)';
    EXEC tSQLt.AssertEmptyTable 'dbo.fw_e';
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertEqualsTable compares tables including duplicates]
AS
BEGIN
    CREATE TABLE #expected (a INT, b VARCHAR(5));
    CREATE TABLE #actual (a INT, b VARCHAR(5));
    INSERT #expected VALUES (1, 'x'), (2, 'y'), (2, 'y');
    INSERT #actual VALUES (2, 'y'), (1, 'x'), (2, 'y');
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
    DELETE TOP (1) FROM #actual WHERE a = 2;
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = 'Unexpected/missing resultset rows%';
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
END;


GO

CREATE PROCEDURE FrameworkTests.[test AssertEqualsTable sees a wrong value]
AS
BEGIN
    CREATE TABLE #e (a INT);
    CREATE TABLE #a (a INT);
    INSERT #e VALUES (1), (2);
    INSERT #a VALUES (1), (3);
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = '%1 expected row(s) missing, 1 unexpected row(s)%';
    EXEC tSQLt.AssertEqualsTable '#e', '#a';
END;


GO

CREATE PROCEDURE FrameworkTests.[test Fail raises a failure]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedMessage = 'on purpose', @ExpectedState = 10;
    EXEC tSQLt.Fail 'on purpose';
END;


GO

CREATE PROCEDURE FrameworkTests.[test ExpectException with an error number and a pattern]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 8134, @ExpectedMessagePattern = 'Divide by zero%';
    SELECT 1 / 0;
END;


GO

CREATE PROCEDURE FrameworkTests.[test ExpectException with severity and state from RAISERROR]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedMessage = 'custom 7', @ExpectedSeverity = 14, @ExpectedState = 3, @ExpectedErrorNumber = 50000;
    RAISERROR('custom %d', 14, 3, 7);
END;


GO

CREATE PROCEDURE FrameworkTests.[test ExpectException with THROW]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 51234, @ExpectedMessage = 'thrown', @ExpectedState = 4;
    THROW 51234, 'thrown', 4;
END;


GO

CREATE PROCEDURE FrameworkTests.[test FakeTable replaces a table with an empty one without constraints]
AS
BEGIN
    CREATE TABLE dbo.fw_real (id INT PRIMARY KEY, name VARCHAR(10) NOT NULL, ref INT);
    INSERT dbo.fw_real VALUES (1, 'a', 10);
    EXEC tSQLt.FakeTable 'dbo.fw_real';
    EXEC tSQLt.AssertEmptyTable 'dbo.fw_real';
    -- the primary key and NOT NULL are gone: duplicates and a missing name are accepted
    INSERT dbo.fw_real (id) VALUES (1);
    INSERT dbo.fw_real (id) VALUES (1);
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.fw_real);
    EXEC tSQLt.AssertEquals 2, @n;
END;


GO

CREATE PROCEDURE FrameworkTests.[test objects made by one test are gone for the next]
AS
BEGIN
    EXEC tSQLt.AssertEquals NULL, NULL;
    -- tests run in name order, so this runs after the FakeTable test and the one that creates fw_real
    DECLARE @id INT = OBJECT_ID('dbo.fw_real');
    EXEC tSQLt.AssertEquals NULL, @id;
    DECLARE @orig INT = OBJECT_ID('dbo.fw_real_tSQLt_Original');
    EXEC tSQLt.AssertEquals NULL, @orig;
END;


GO

CREATE PROCEDURE FrameworkTests.[test SpyProcedure records calls and their arguments]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.fw_target @a INT, @b VARCHAR(20), @c DECIMAL(8,2) = NULL AS SELECT ''real code ran''');
    EXEC tSQLt.SpyProcedure 'dbo.fw_target';
    EXEC dbo.fw_target 1, 'one', 1.50;
    EXEC dbo.fw_target @a = 2, @b = 'two';
    CREATE TABLE #expected (a INT, b VARCHAR(20), c DECIMAL(8,2));
    INSERT #expected VALUES (1, 'one', 1.50), (2, 'two', NULL);
    SELECT a, b, c INTO #actual FROM dbo.fw_target_SpyProcedureLog;
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
END;


GO

CREATE PROCEDURE FrameworkTests.[test SpyProcedure can run a command instead of the original]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.fw_target2 @x INT AS RETURN 99');
    EXEC tSQLt.SpyProcedure 'dbo.fw_target2', 'SET @x = 7';
    EXEC dbo.fw_target2 @x = 1;
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.fw_target2_SpyProcedureLog);
    EXEC tSQLt.AssertEquals 1, @n;
END;


GO

CREATE PROCEDURE FrameworkTests.[test SpyProcedure rejects a procedure that does not exist]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = 'Cannot use SpyProcedure on <dbo.fw_missing>%';
    EXEC tSQLt.SpyProcedure 'dbo.fw_missing';
END;


GO

CREATE PROCEDURE FrameworkTests.SetUp
AS
BEGIN
    CREATE TABLE dbo.fw_setup_ran (n INT);
    INSERT dbo.fw_setup_ran VALUES (1);
END;


GO

CREATE PROCEDURE FrameworkTests.[test SetUp ran before the test]
AS
BEGIN
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.fw_setup_ran);
    EXEC tSQLt.AssertEquals 1, @n;
END;


GO

CREATE PROCEDURE FrameworkTests.[test SetUp runs again for each test with a clean slate]
AS
BEGIN
    INSERT dbo.fw_setup_ran VALUES (2);
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.fw_setup_ran);
    EXEC tSQLt.AssertEquals 2, @n;
END;


GO
