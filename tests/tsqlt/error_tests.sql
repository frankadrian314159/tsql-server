-- Errors, TRY/CATCH, THROW, RAISERROR, constraints, transactions and savepoints.
EXEC tSQLt.NewTestClass 'ErrorTests';


GO

CREATE PROCEDURE ErrorTests.[test TRY CATCH exposes number severity state and message]
AS
BEGIN
    DECLARE @n INT, @s INT, @st INT, @m NVARCHAR(400);
    BEGIN TRY
        SELECT 1 / 0;
    END TRY
    BEGIN CATCH
        SELECT @n = ERROR_NUMBER(), @s = ERROR_SEVERITY(), @st = ERROR_STATE(), @m = ERROR_MESSAGE();
    END CATCH;
    EXEC tSQLt.AssertEquals 8134, @n;
    EXEC tSQLt.AssertEquals 16, @s;
    EXEC tSQLt.AssertEquals 1, @st;
    EXEC tSQLt.AssertEqualsString N'Divide by zero error encountered.', @m;
END;


GO

CREATE PROCEDURE ErrorTests.[test ERROR functions are NULL outside a CATCH block]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = ERROR_NUMBER();
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = ERROR_MESSAGE();
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
END;


GO

CREATE PROCEDURE ErrorTests.[test ERROR_PROCEDURE names the procedure that raised the error]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.err_raiser AS SELECT 1 / 0');
    DECLARE @p NVARCHAR(200);
    BEGIN TRY
        EXEC dbo.err_raiser;
    END TRY
    BEGIN CATCH
        SET @p = ERROR_PROCEDURE();
    END CATCH;
    EXEC tSQLt.AssertEqualsString N'err_raiser', @p;
END;


GO

CREATE PROCEDURE ErrorTests.[test THROW with no arguments re-raises the original error]
AS
BEGIN
    DECLARE @n INT;
    BEGIN TRY
        BEGIN TRY
            SELECT 1 / 0;
        END TRY
        BEGIN CATCH
            THROW;
        END CATCH;
    END TRY
    BEGIN CATCH
        SET @n = ERROR_NUMBER();
    END CATCH;
    EXEC tSQLt.AssertEquals 8134, @n;
END;


GO

CREATE PROCEDURE ErrorTests.[test THROW with no arguments outside a CATCH block is an error]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 10704;
    THROW;
END;


GO

CREATE PROCEDURE ErrorTests.[test THROW needs a user error number of 50000 or more]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 35100;
    THROW 49999, 'too low', 1;
END;


GO

CREATE PROCEDURE ErrorTests.[test THROW raises severity 16 with the given state]
AS
BEGIN
    DECLARE @n INT, @s INT, @st INT, @m NVARCHAR(100);
    BEGIN TRY
        THROW 52001, N'custom', 9;
    END TRY
    BEGIN CATCH
        SELECT @n = ERROR_NUMBER(), @s = ERROR_SEVERITY(), @st = ERROR_STATE(), @m = ERROR_MESSAGE();
    END CATCH;
    EXEC tSQLt.AssertEquals 52001, @n;
    EXEC tSQLt.AssertEquals 16, @s;
    EXEC tSQLt.AssertEquals 9, @st;
    EXEC tSQLt.AssertEqualsString N'custom', @m;
END;


GO

CREATE PROCEDURE ErrorTests.[test RAISERROR formats its arguments]
AS
BEGIN
    DECLARE @m NVARCHAR(400);
    BEGIN TRY
        RAISERROR('a=%d b=%s c=%u d=%i', 16, 1, -5, 'text', 7, 8);
    END TRY
    BEGIN CATCH
        SET @m = ERROR_MESSAGE();
    END CATCH;
    EXEC tSQLt.AssertEqualsString N'a=-5 b=text c=7 d=8', @m;
END;


GO

CREATE PROCEDURE ErrorTests.[test RAISERROR with a percent sign and a doubled percent]
AS
BEGIN
    DECLARE @m NVARCHAR(400);
    BEGIN TRY
        RAISERROR('100%% of %d', 16, 1, 3);
    END TRY
    BEGIN CATCH
        SET @m = ERROR_MESSAGE();
    END CATCH;
    EXEC tSQLt.AssertEqualsString N'100% of 3', @m;
END;


GO

CREATE PROCEDURE ErrorTests.[test RAISERROR returns number 50000 and the given severity and state]
AS
BEGIN
    DECLARE @n INT, @s INT, @st INT;
    BEGIN TRY
        RAISERROR('x', 11, 4);
    END TRY
    BEGIN CATCH
        SELECT @n = ERROR_NUMBER(), @s = ERROR_SEVERITY(), @st = ERROR_STATE();
    END CATCH;
    EXEC tSQLt.AssertEquals 50000, @n;
    EXEC tSQLt.AssertEquals 11, @s;
    EXEC tSQLt.AssertEquals 4, @st;
END;


GO

CREATE PROCEDURE ErrorTests.[test RAISERROR with severity 10 is informational and not caught]
AS
BEGIN
    DECLARE @x INT = 0;
    BEGIN TRY
        RAISERROR('just a message', 10, 1);
        SET @x = 1;
    END TRY
    BEGIN CATCH
        SET @x = 2;
    END CATCH;
    EXEC tSQLt.AssertEquals 1, @x;
END;


GO

CREATE PROCEDURE ErrorTests.[test the CATCH block is skipped when nothing fails]
AS
BEGIN
    DECLARE @x INT = 0;
    BEGIN TRY
        SET @x = 1;
    END TRY
    BEGIN CATCH
        SET @x = 99;
    END CATCH;
    EXEC tSQLt.AssertEquals 1, @x;
END;


GO

CREATE PROCEDURE ErrorTests.[test statements after the failing one in a TRY block do not run]
AS
BEGIN
    DECLARE @x INT = 0;
    BEGIN TRY
        SELECT 1 / 0;
        SET @x = 1;
    END TRY
    BEGIN CATCH
        SET @x = @x + 10;
    END CATCH;
    EXEC tSQLt.AssertEquals 10, @x;
END;


GO

CREATE PROCEDURE ErrorTests.[test nested TRY CATCH where the inner handler fails]
AS
BEGIN
    DECLARE @m NVARCHAR(100);
    BEGIN TRY
        BEGIN TRY
            THROW 51000, 'inner', 1;
        END TRY
        BEGIN CATCH
            THROW 51001, 'from the inner handler', 2;
        END CATCH;
    END TRY
    BEGIN CATCH
        SET @m = ERROR_MESSAGE();
    END CATCH;
    EXEC tSQLt.AssertEqualsString N'from the inner handler', @m;
END;


GO

CREATE PROCEDURE ErrorTests.[test an error in a loop body can be handled and the loop continues]
AS
BEGIN
    DECLARE @i INT = 1, @ok INT = 0, @bad INT = 0;
    WHILE @i <= 6
    BEGIN
        BEGIN TRY
            IF @i % 3 = 0 SELECT 1 / 0;
            SET @ok = @ok + 1;
        END TRY
        BEGIN CATCH
            SET @bad = @bad + 1;
        END CATCH;
        SET @i = @i + 1;
    END;
    EXEC tSQLt.AssertEquals 4, @ok;
    EXEC tSQLt.AssertEquals 2, @bad;
END;


GO

CREATE PROCEDURE ErrorTests.[test a primary key violation is error 2627]
AS
BEGIN
    CREATE TABLE dbo.e_pk (id INT PRIMARY KEY);
    INSERT dbo.e_pk VALUES (1);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 2627, @ExpectedMessagePattern = 'Violation of PRIMARY KEY constraint%', @ExpectedSeverity = 14;
    INSERT dbo.e_pk VALUES (1);
END;


GO

CREATE PROCEDURE ErrorTests.[test a unique constraint violation is error 2627]
AS
BEGIN
    CREATE TABLE dbo.e_uq (id INT, v INT, CONSTRAINT uq_v UNIQUE (v));
    INSERT dbo.e_uq VALUES (1, 5);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 2627, @ExpectedMessagePattern = 'Violation of UNIQUE KEY constraint ''uq_v''%';
    INSERT dbo.e_uq VALUES (2, 5);
END;


GO

CREATE PROCEDURE ErrorTests.[test a unique index violation is error 2601]
AS
BEGIN
    CREATE TABLE dbo.e_ui (id INT, v INT);
    CREATE UNIQUE INDEX ux_v ON dbo.e_ui (v);
    INSERT dbo.e_ui VALUES (1, 5);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 2601;
    INSERT dbo.e_ui VALUES (2, 5);
END;


GO

CREATE PROCEDURE ErrorTests.[test several NULLs are not unique violations but one NULL in a PK column is a NOT NULL violation]
AS
BEGIN
    CREATE TABLE dbo.e_nu (id INT PRIMARY KEY, v INT UNIQUE);
    INSERT dbo.e_nu VALUES (1, NULL);
    DECLARE @n INT = 0;
    BEGIN TRY
        INSERT dbo.e_nu VALUES (2, NULL);
    END TRY
    BEGIN CATCH
        SET @n = ERROR_NUMBER();
    END CATCH;
    -- SQL Server treats NULLs as equal in a unique constraint: the second NULL is a violation
    EXEC tSQLt.AssertEquals 2627, @n;
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 515;
    INSERT dbo.e_nu VALUES (NULL, 3);
END;


GO

CREATE PROCEDURE ErrorTests.[test a NOT NULL violation is error 515]
AS
BEGIN
    CREATE TABLE dbo.e_nn (id INT NOT NULL, v INT);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 515, @ExpectedMessagePattern = 'Cannot insert the value NULL into column ''id''%';
    INSERT dbo.e_nn (v) VALUES (1);
END;


GO

CREATE PROCEDURE ErrorTests.[test a foreign key violation on insert is error 547]
AS
BEGIN
    CREATE TABLE dbo.e_parent (id INT PRIMARY KEY);
    CREATE TABLE dbo.e_child (id INT PRIMARY KEY, p INT CONSTRAINT fk_p FOREIGN KEY REFERENCES dbo.e_parent (id));
    INSERT dbo.e_parent VALUES (1);
    INSERT dbo.e_child VALUES (1, 1);
    INSERT dbo.e_child VALUES (2, NULL);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 547, @ExpectedMessagePattern = 'The INSERT statement conflicted with the FOREIGN KEY constraint "fk_p"%';
    INSERT dbo.e_child VALUES (3, 99);
END;


GO

CREATE PROCEDURE ErrorTests.[test deleting a referenced row is error 547]
AS
BEGIN
    CREATE TABLE dbo.e_parent (id INT PRIMARY KEY);
    CREATE TABLE dbo.e_child (id INT PRIMARY KEY, p INT REFERENCES dbo.e_parent (id));
    INSERT dbo.e_parent VALUES (1), (2);
    INSERT dbo.e_child VALUES (1, 1);
    DELETE dbo.e_parent WHERE id = 2;
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 547, @ExpectedMessagePattern = 'The DELETE statement conflicted with the REFERENCE constraint%';
    DELETE dbo.e_parent WHERE id = 1;
END;


GO

CREATE PROCEDURE ErrorTests.[test cascading delete removes the children]
AS
BEGIN
    CREATE TABLE dbo.e_parent (id INT PRIMARY KEY);
    CREATE TABLE dbo.e_child (id INT PRIMARY KEY, p INT REFERENCES dbo.e_parent (id) ON DELETE CASCADE);
    INSERT dbo.e_parent VALUES (1), (2);
    INSERT dbo.e_child VALUES (1, 1), (2, 1), (3, 2);
    DELETE dbo.e_parent WHERE id = 1;
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.e_child);
    EXEC tSQLt.AssertEquals 1, @n;
END;


GO

CREATE PROCEDURE ErrorTests.[test a CHECK violation is error 547 and NULL passes the check]
AS
BEGIN
    CREATE TABLE dbo.e_ck (a INT CONSTRAINT ck_a CHECK (a > 0));
    INSERT dbo.e_ck VALUES (1), (NULL);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 547, @ExpectedMessagePattern = 'The INSERT statement conflicted with the CHECK constraint "ck_a"%';
    INSERT dbo.e_ck VALUES (0);
END;


GO

CREATE PROCEDURE ErrorTests.[test a failing multi-row INSERT inserts nothing]
AS
BEGIN
    CREATE TABLE dbo.e_atomic (id INT PRIMARY KEY);
    BEGIN TRY
        INSERT dbo.e_atomic VALUES (1), (2), (1);
    END TRY
    BEGIN CATCH
    END CATCH;
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.e_atomic);
    EXEC tSQLt.AssertEquals 0, @n;
END;


GO

CREATE PROCEDURE ErrorTests.[test a failing UPDATE changes no row]
AS
BEGIN
    CREATE TABLE dbo.e_upd (id INT PRIMARY KEY, a INT);
    INSERT dbo.e_upd VALUES (1, 5), (2, 3), (3, 7);
    BEGIN TRY
        UPDATE dbo.e_upd SET a = 100 / (a - 3);
    END TRY
    BEGIN CATCH
    END CATCH;
    DECLARE @s INT = (SELECT SUM(a) FROM dbo.e_upd);
    EXEC tSQLt.AssertEquals 15, @s;
END;


GO

CREATE PROCEDURE ErrorTests.[test an error raised by a trigger undoes the statement]
AS
BEGIN
    CREATE TABLE dbo.e_trg (id INT PRIMARY KEY);
    EXEC('CREATE TRIGGER dbo.trg_e ON dbo.e_trg AFTER INSERT AS IF EXISTS (SELECT 1 FROM inserted WHERE id < 0) THROW 53000, ''negative id'', 1');
    INSERT dbo.e_trg VALUES (1);
    BEGIN TRY
        INSERT dbo.e_trg VALUES (2), (-1);
    END TRY
    BEGIN CATCH
    END CATCH;
    DECLARE @n INT = (SELECT COUNT(*) FROM dbo.e_trg);
    EXEC tSQLt.AssertEquals 1, @n;
END;


GO

CREATE PROCEDURE ErrorTests.[test a conversion failure is error 245]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 245, @ExpectedMessagePattern = 'Conversion failed when converting the varchar value ''abc'' to data type int.';
    SELECT CAST('abc' AS INT);
END;


GO

CREATE PROCEDURE ErrorTests.[test TRY_CAST and TRY_CONVERT return NULL instead of failing]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = TRY_CAST('abc' AS INT);
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = TRY_CONVERT(INT, '12x');
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = TRY_CAST('12' AS INT);
    EXEC tSQLt.AssertEquals 12, @tsqlt_v;
    SET @tsqlt_v = TRY_CAST('2024-13-45' AS DATE);
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
END;


GO

CREATE PROCEDURE ErrorTests.[test integer overflow is error 8115]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 8115, @ExpectedMessagePattern = 'Arithmetic overflow error converting expression to data type int.';
    SELECT 2147483647 + 1;
END;


GO

CREATE PROCEDURE ErrorTests.[test a value that does not fit TINYINT is error 220]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 220;
    SELECT CAST(300 AS TINYINT);
END;


GO

CREATE PROCEDURE ErrorTests.[test truncation of a string on insert is an error]
AS
BEGIN
    CREATE TABLE dbo.e_tr (s VARCHAR(3));
    EXEC tSQLt.ExpectException @ExpectedMessagePattern = 'String or binary data would be truncated%';
    INSERT dbo.e_tr VALUES ('abcd');
END;


GO

CREATE PROCEDURE ErrorTests.[test an invalid object name is error 208]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 208, @ExpectedMessagePattern = 'Invalid object name ''dbo.no_such_table''.';
    EXEC('SELECT * FROM dbo.no_such_table');
END;


GO

CREATE PROCEDURE ErrorTests.[test an invalid column name is error 207]
AS
BEGIN
    CREATE TABLE dbo.e_col (a INT);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 207, @ExpectedMessagePattern = 'Invalid column name ''b''.';
    EXEC('SELECT b FROM dbo.e_col');
END;


GO

CREATE PROCEDURE ErrorTests.[test an ambiguous column name is error 209]
AS
BEGIN
    CREATE TABLE dbo.e_a1 (id INT);
    CREATE TABLE dbo.e_a2 (id INT);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 209;
    EXEC('SELECT id FROM dbo.e_a1 JOIN dbo.e_a2 ON 1 = 1');
END;


GO

CREATE PROCEDURE ErrorTests.[test calling a procedure that does not exist is error 2812]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 2812;
    EXEC dbo.no_such_proc;
END;


GO

CREATE PROCEDURE ErrorTests.[test a missing required procedure parameter is error 201]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.e_need @a INT AS SELECT @a');
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 201;
    EXEC dbo.e_need;
END;


GO

CREATE PROCEDURE ErrorTests.[test too many arguments to a procedure is error 8144]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.e_two @a INT AS SELECT @a');
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 8144;
    EXEC dbo.e_two 1, 2;
END;


GO

CREATE PROCEDURE ErrorTests.[test an unknown procedure parameter name is error 8145]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.e_named @a INT AS SELECT @a');
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 8145;
    EXEC dbo.e_named @b = 1;
END;


GO

CREATE PROCEDURE ErrorTests.[test a procedure return value and the default of zero]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.e_rc1 AS RETURN 7');
    EXEC('CREATE PROCEDURE dbo.e_rc2 AS SELECT 1');
    DECLARE @a INT, @b INT;
    EXEC @a = dbo.e_rc1;
    EXEC @b = dbo.e_rc2;
    EXEC tSQLt.AssertEquals 7, @a;
    EXEC tSQLt.AssertEquals 0, @b;
END;


GO

CREATE PROCEDURE ErrorTests.[test TRANCOUNT counts nested transactions]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @start INT = @@TRANCOUNT;
    BEGIN TRAN;
    BEGIN TRAN;
    SET @tsqlt_v = @@TRANCOUNT - @start;
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    COMMIT;
    SET @tsqlt_v = @@TRANCOUNT - @start;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    COMMIT;
    SET @tsqlt_v = @@TRANCOUNT - @start;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
END;


GO

CREATE PROCEDURE ErrorTests.[test a savepoint rolls back part of a transaction]
AS
BEGIN
    CREATE TABLE dbo.e_sp (a INT);
    INSERT dbo.e_sp VALUES (1);
    SAVE TRAN sp1;
    INSERT dbo.e_sp VALUES (2);
    SAVE TRAN sp2;
    INSERT dbo.e_sp VALUES (3);
    ROLLBACK TRAN sp2;
    DECLARE @n1 INT = (SELECT COUNT(*) FROM dbo.e_sp);
    ROLLBACK TRAN sp1;
    DECLARE @n2 INT = (SELECT COUNT(*) FROM dbo.e_sp);
    EXEC tSQLt.AssertEquals 2, @n1;
    EXEC tSQLt.AssertEquals 1, @n2;
END;


GO

CREATE PROCEDURE ErrorTests.[test rolling back to an unknown savepoint is error 6401]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 6401;
    ROLLBACK TRAN no_such_savepoint;
END;


GO

CREATE PROCEDURE ErrorTests.[test DDL is transactional]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SAVE TRAN s;
    CREATE TABLE dbo.e_ddl (a INT);
    ALTER TABLE dbo.e_ddl ADD b INT;
    SET @tsqlt_v = (SELECT COUNT(*) FROM sys.columns WHERE object_id = OBJECT_ID('dbo.e_ddl'));
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    ROLLBACK TRAN s;
    SET @tsqlt_v = OBJECT_ID('dbo.e_ddl');
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
END;


GO

CREATE PROCEDURE ErrorTests.[test XACT_ABORT makes the transaction uncommittable after an error]
AS
BEGIN
    SET XACT_ABORT ON;
    DECLARE @st INT;
    BEGIN TRY
        BEGIN TRAN;
        SELECT 1 / 0;
        COMMIT;
    END TRY
    BEGIN CATCH
        SET @st = XACT_STATE();
        IF @@TRANCOUNT > 0 ROLLBACK;
    END CATCH;
    EXEC tSQLt.AssertEquals -1, @st;
END;


GO

CREATE PROCEDURE ErrorTests.[test XACT_STATE is 1 in an open transaction and 0 outside]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = XACT_STATE();
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE ErrorTests.[test an error inside a function call fails the statement that called it]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    EXEC('CREATE FUNCTION dbo.e_div(@x INT) RETURNS INT AS BEGIN RETURN 10 / @x; END');
    SET @tsqlt_v = dbo.e_div(2);
    EXEC tSQLt.AssertEquals 5, @tsqlt_v;
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 8134;
    SELECT dbo.e_div(0);
END;


GO

CREATE PROCEDURE ErrorTests.[test an error in a nested procedure reaches the outer CATCH]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.e_inner AS THROW 54000, ''deep'', 1');
    EXEC('CREATE PROCEDURE dbo.e_outer AS EXEC dbo.e_inner');
    DECLARE @n INT, @p NVARCHAR(100);
    BEGIN TRY
        EXEC dbo.e_outer;
    END TRY
    BEGIN CATCH
        SELECT @n = ERROR_NUMBER(), @p = ERROR_PROCEDURE();
    END CATCH;
    EXEC tSQLt.AssertEquals 54000, @n;
    EXEC tSQLt.AssertEqualsString N'e_inner', @p;
END;


GO

CREATE PROCEDURE ErrorTests.[test an error in the CATCH block itself propagates]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 8134;
    BEGIN TRY
        THROW 51000, 'first', 1;
    END TRY
    BEGIN CATCH
        SELECT 1 / 0;
    END CATCH;
END;


GO
