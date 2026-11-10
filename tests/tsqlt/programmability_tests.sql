-- Procedural T-SQL and the programmability features: variables, temp tables, cursors, functions, triggers, CTEs,
-- windows, MERGE/OUTPUT, identity and sequences, views, table types.
EXEC tSQLt.NewTestClass 'ProgrammabilityTests';


GO

CREATE PROCEDURE ProgrammabilityTests.[test a table variable is not rolled back by ROLLBACK to a savepoint]
AS
BEGIN
    DECLARE @t TABLE (a INT);
    SAVE TRAN s;
    INSERT @t VALUES (1);
    ROLLBACK TRAN s;
    DECLARE @n INT = (SELECT COUNT(*) FROM @t);
    EXEC tSQLt.AssertEquals 1, @n;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a temp table is rolled back with its transaction]
AS
BEGIN
    CREATE TABLE #t (a INT);
    SAVE TRAN s;
    INSERT #t VALUES (1);
    ROLLBACK TRAN s;
    DECLARE @n INT = (SELECT COUNT(*) FROM #t);
    EXEC tSQLt.AssertEquals 0, @n;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a temp table of the caller is visible in a called procedure]
AS
BEGIN
    CREATE TABLE #outer (a INT);
    INSERT #outer VALUES (4), (5);
    EXEC('CREATE PROCEDURE dbo.p_sees AS SELECT COUNT(*) AS n FROM #outer');
    CREATE TABLE #r (n INT);
    INSERT #r EXEC dbo.p_sees;
    DECLARE @n INT = (SELECT n FROM #r);
    EXEC tSQLt.AssertEquals 2, @n;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a temp table made in a procedure is gone after it returns]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    EXEC('CREATE PROCEDURE dbo.p_makes AS BEGIN CREATE TABLE #inner (a INT); INSERT #inner VALUES (1); END');
    EXEC dbo.p_makes;
    SET @tsqlt_v = OBJECT_ID('tempdb..#inner');
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test SELECT INTO creates a table with the column types]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.src (id INT, name VARCHAR(20), amt DECIMAL(9,2));
    INSERT dbo.src VALUES (1, 'a', 1.50);
    SELECT * INTO dbo.copy FROM dbo.src;
    SET @tsqlt_v = (SELECT TYPE_NAME(system_type_id) FROM sys.columns WHERE object_id = OBJECT_ID('dbo.copy') AND name = 'name');
    EXEC tSQLt.AssertEquals 'varchar', @tsqlt_v;
    SET @tsqlt_v = (SELECT scale FROM sys.columns WHERE object_id = OBJECT_ID('dbo.copy') AND name = 'amt');
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.copy);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a fast-forward cursor loop visits every row in order]
AS
BEGIN
    CREATE TABLE dbo.cur (id INT, v VARCHAR(5));
    INSERT dbo.cur VALUES (3, 'c'), (1, 'a'), (2, 'b');
    DECLARE @id INT, @v VARCHAR(5), @all VARCHAR(20) = '';
    DECLARE c CURSOR FAST_FORWARD FOR SELECT id, v FROM dbo.cur ORDER BY id;
    OPEN c;
    FETCH NEXT FROM c INTO @id, @v;
    WHILE @@FETCH_STATUS = 0
    BEGIN
        SET @all = @all + @v + CAST(@id AS VARCHAR(5));
        FETCH NEXT FROM c INTO @id, @v;
    END;
    CLOSE c;
    DEALLOCATE c;
    EXEC tSQLt.AssertEqualsString 'a1b2c3', @all;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a scrollable cursor moves in both directions]
AS
BEGIN
    CREATE TABLE dbo.cur (id INT);
    INSERT dbo.cur VALUES (10), (20), (30), (40);
    DECLARE @a INT, @b INT, @c INT, @d INT;
    DECLARE c CURSOR SCROLL STATIC FOR SELECT id FROM dbo.cur ORDER BY id;
    OPEN c;
    FETCH LAST FROM c INTO @a;
    FETCH PRIOR FROM c INTO @b;
    FETCH FIRST FROM c INTO @c;
    FETCH ABSOLUTE 3 FROM c INTO @d;
    CLOSE c;
    DEALLOCATE c;
    EXEC tSQLt.AssertEquals 40, @a;
    EXEC tSQLt.AssertEquals 30, @b;
    EXEC tSQLt.AssertEquals 10, @c;
    EXEC tSQLt.AssertEquals 30, @d;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test FETCH_STATUS is -1 after the last row]
AS
BEGIN
    CREATE TABLE dbo.cur (id INT);
    INSERT dbo.cur VALUES (1);
    DECLARE @x INT;
    DECLARE c CURSOR FOR SELECT id FROM dbo.cur;
    OPEN c;
    FETCH NEXT FROM c INTO @x;
    FETCH NEXT FROM c INTO @x;
    DECLARE @st INT = @@FETCH_STATUS;
    CLOSE c;
    DEALLOCATE c;
    EXEC tSQLt.AssertEquals -1, @st;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a static cursor does not see changes made after it opened]
AS
BEGIN
    CREATE TABLE dbo.cur (id INT);
    INSERT dbo.cur VALUES (1), (2);
    DECLARE @x INT, @n INT = 0;
    DECLARE c CURSOR STATIC FOR SELECT id FROM dbo.cur;
    OPEN c;
    INSERT dbo.cur VALUES (3), (4);
    FETCH NEXT FROM c INTO @x;
    WHILE @@FETCH_STATUS = 0
    BEGIN
        SET @n = @n + 1;
        FETCH NEXT FROM c INTO @x;
    END;
    CLOSE c;
    DEALLOCATE c;
    EXEC tSQLt.AssertEquals 2, @n;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test WHILE with BREAK and CONTINUE]
AS
BEGIN
    DECLARE @i INT = 0, @sum INT = 0;
    WHILE 1 = 1
    BEGIN
        SET @i = @i + 1;
        IF @i > 10 BREAK;
        IF @i % 2 = 0 CONTINUE;
        SET @sum = @sum + @i;
    END;
    EXEC tSQLt.AssertEquals 25, @sum;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test CASE IIF CHOOSE and GOTO]
AS
BEGIN
    DECLARE @tsqlt_s NVARCHAR(MAX);
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @n INT = 2, @r VARCHAR(10);
    SET @r = CASE @n WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END;
    EXEC tSQLt.AssertEqualsString 'two', @r;
    SET @tsqlt_s = IIF(@n > 1, 'yes', 'no');
    EXEC tSQLt.AssertEqualsString 'yes', @tsqlt_s;
    SET @tsqlt_s = CHOOSE(3, 'a', 'b', 'c');
    EXEC tSQLt.AssertEqualsString 'c', @tsqlt_s;
    SET @tsqlt_v = CHOOSE(4, 'a', 'b', 'c');
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    DECLARE @k INT = 0;
    skip:
    SET @k = @k + 1;
    IF @k < 3 GOTO skip;
    EXEC tSQLt.AssertEquals 3, @k;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a scalar function]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    EXEC('CREATE FUNCTION dbo.f_sq(@x INT) RETURNS INT AS BEGIN RETURN @x * @x; END');
    SET @tsqlt_v = dbo.f_sq(7);
    EXEC tSQLt.AssertEquals 49, @tsqlt_v;
    CREATE TABLE dbo.fn_t (a INT);
    INSERT dbo.fn_t VALUES (1), (2), (3);
    SET @tsqlt_v = (SELECT SUM(dbo.f_sq(a)) FROM dbo.fn_t);
    EXEC tSQLt.AssertEquals 14, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test an inline table-valued function with CROSS APPLY]
AS
BEGIN
    EXEC('CREATE FUNCTION dbo.f_rng(@n INT) RETURNS TABLE AS RETURN (SELECT TOP (@n) 1 AS one FROM sys.objects)');
    CREATE TABLE dbo.fn_t (id INT, n INT);
    INSERT dbo.fn_t VALUES (1, 1), (2, 0);
    DECLARE @c INT = (SELECT COUNT(*) FROM dbo.fn_t t CROSS APPLY dbo.f_rng(t.n) r);
    DECLARE @o INT = (SELECT COUNT(*) FROM dbo.fn_t t OUTER APPLY dbo.f_rng(t.n) r);
    EXEC tSQLt.AssertEquals 1, @c;
    EXEC tSQLt.AssertEquals 2, @o;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a multi-statement table-valued function]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    EXEC('CREATE FUNCTION dbo.f_tab(@n INT) RETURNS @t TABLE (i INT, sq INT) AS BEGIN DECLARE @i INT = 1; WHILE @i <= @n BEGIN INSERT @t VALUES (@i, @i * @i); SET @i = @i + 1; END; RETURN; END');
    SET @tsqlt_v = (SELECT SUM(sq) FROM dbo.f_tab(5));
    EXEC tSQLt.AssertEquals 55, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.f_tab(3));
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a recursive CTE counts and builds a hierarchy]
AS
BEGIN
    DECLARE @s INT = (SELECT SUM(n) FROM (SELECT 1 AS n UNION ALL SELECT 2) x);
    WITH nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 10)
    SELECT @s = SUM(n) FROM nums;
    EXEC tSQLt.AssertEquals 55, @s;
    CREATE TABLE dbo.emp (id INT, boss INT, name VARCHAR(10));
    INSERT dbo.emp VALUES (1, NULL, 'ceo'), (2, 1, 'vp'), (3, 2, 'dev'), (4, 2, 'qa'), (5, 1, 'cfo');
    DECLARE @depth INT;
    WITH t AS (SELECT id, boss, 0 AS lvl FROM dbo.emp WHERE boss IS NULL UNION ALL SELECT e.id, e.boss, t.lvl + 1 FROM dbo.emp e JOIN t ON e.boss = t.id)
    SELECT @depth = MAX(lvl) FROM t;
    EXEC tSQLt.AssertEquals 2, @depth;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a recursive CTE past MAXRECURSION is error 530]
AS
BEGIN
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 530;
    EXEC('WITH r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r) SELECT COUNT(*) FROM r OPTION (MAXRECURSION 20)');
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test window functions]
AS
BEGIN
    CREATE TABLE dbo.w (g VARCHAR(1), v INT);
    INSERT dbo.w VALUES ('a', 10), ('a', 20), ('a', 20), ('b', 5), ('b', 7);
    CREATE TABLE #actual (g VARCHAR(1), v INT, rn BIGINT, rk BIGINT, dr BIGINT, lg INT, run INT);
    INSERT #actual
    SELECT g, v, ROW_NUMBER() OVER (PARTITION BY g ORDER BY v), RANK() OVER (PARTITION BY g ORDER BY v), DENSE_RANK() OVER (PARTITION BY g ORDER BY v),
           LAG(v) OVER (PARTITION BY g ORDER BY v), SUM(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
    FROM dbo.w;
    CREATE TABLE #expected (g VARCHAR(1), v INT, rn BIGINT, rk BIGINT, dr BIGINT, lg INT, run INT);
    INSERT #expected VALUES ('a', 10, 1, 1, 1, NULL, 10), ('a', 20, 2, 2, 2, 10, 30), ('a', 20, 3, 2, 2, 20, 50), ('b', 5, 1, 1, 1, NULL, 5), ('b', 7, 2, 2, 2, 5, 12);
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test NTILE and FIRST_VALUE LAST_VALUE]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.w (v INT);
    INSERT dbo.w VALUES (1), (2), (3), (4), (5);
    SET @tsqlt_v = (SELECT MAX(t) FROM (SELECT NTILE(2) OVER (ORDER BY v) AS t FROM dbo.w) x);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT NTILE(2) OVER (ORDER BY v) AS t FROM dbo.w) x WHERE t = 1);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = (SELECT TOP 1 FIRST_VALUE(v) OVER (ORDER BY v) FROM dbo.w);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT TOP 1 LAST_VALUE(v) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM dbo.w);
    EXEC tSQLt.AssertEquals 5, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test PIVOT and UNPIVOT]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.pv (k VARCHAR(5), q VARCHAR(2), amt INT);
    INSERT dbo.pv VALUES ('x', 'Q1', 1), ('x', 'Q2', 2), ('y', 'Q1', 3);
    CREATE TABLE #actual (k VARCHAR(5), Q1 INT, Q2 INT);
    INSERT #actual SELECT k, Q1, Q2 FROM dbo.pv PIVOT (SUM(amt) FOR q IN (Q1, Q2)) p;
    CREATE TABLE #expected (k VARCHAR(5), Q1 INT, Q2 INT);
    INSERT #expected VALUES ('x', 1, 2), ('y', 3, NULL);
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
    CREATE TABLE dbo.up (k VARCHAR(5), a INT, b INT);
    INSERT dbo.up VALUES ('x', 1, 2), ('y', NULL, 4);
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.up UNPIVOT (v FOR c IN (a, b)) u);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test MERGE inserts updates and deletes and reports through OUTPUT]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.tgt (id INT PRIMARY KEY, v INT);
    CREATE TABLE dbo.src (id INT PRIMARY KEY, v INT);
    INSERT dbo.tgt VALUES (1, 10), (2, 20), (3, 30);
    INSERT dbo.src VALUES (2, 200), (3, 30), (4, 400);
    DECLARE @log TABLE (act VARCHAR(10), id INT);
    MERGE dbo.tgt AS t USING dbo.src AS s ON t.id = s.id
    WHEN MATCHED AND t.v <> s.v THEN UPDATE SET v = s.v
    WHEN NOT MATCHED BY TARGET THEN INSERT (id, v) VALUES (s.id, s.v)
    WHEN NOT MATCHED BY SOURCE THEN DELETE
    OUTPUT $action, COALESCE(inserted.id, deleted.id) INTO @log;
    CREATE TABLE #actual (id INT, v INT);
    INSERT #actual SELECT id, v FROM dbo.tgt;
    CREATE TABLE #expected (id INT, v INT);
    INSERT #expected VALUES (2, 200), (3, 30), (4, 400);
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
    SET @tsqlt_v = (SELECT COUNT(*) FROM @log WHERE act = 'INSERT');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM @log WHERE act = 'UPDATE');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM @log WHERE act = 'DELETE');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test OUTPUT of INSERT UPDATE and DELETE]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.o (id INT IDENTITY(1,1), v INT);
    DECLARE @ins TABLE (id INT, v INT), @upd TABLE (old INT, new INT), @del TABLE (id INT);
    INSERT dbo.o (v) OUTPUT inserted.id, inserted.v INTO @ins VALUES (10), (20);
    UPDATE dbo.o SET v = v + 1 OUTPUT deleted.v, inserted.v INTO @upd WHERE id = 1;
    DELETE dbo.o OUTPUT deleted.id INTO @del WHERE id = 2;
    SET @tsqlt_v = (SELECT COUNT(*) FROM @ins);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT old FROM @upd);
    EXEC tSQLt.AssertEquals 10, @tsqlt_v;
    SET @tsqlt_v = (SELECT new FROM @upd);
    EXEC tSQLt.AssertEquals 11, @tsqlt_v;
    SET @tsqlt_v = (SELECT id FROM @del);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test SCOPE_IDENTITY IDENT_CURRENT and IDENTITY_INSERT]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.idt (id INT IDENTITY(5, 10), v INT);
    INSERT dbo.idt (v) VALUES (1);
    SET @tsqlt_v = SCOPE_IDENTITY();
    EXEC tSQLt.AssertEquals 5, @tsqlt_v;
    INSERT dbo.idt (v) VALUES (2);
    SET @tsqlt_v = SCOPE_IDENTITY();
    EXEC tSQLt.AssertEquals 15, @tsqlt_v;
    SET @tsqlt_v = IDENT_CURRENT('dbo.idt');
    EXEC tSQLt.AssertEquals 15, @tsqlt_v;
    SET IDENTITY_INSERT dbo.idt ON;
    INSERT dbo.idt (id, v) VALUES (100, 3);
    SET IDENTITY_INSERT dbo.idt OFF;
    INSERT dbo.idt (v) VALUES (4);
    SET @tsqlt_v = SCOPE_IDENTITY();
    EXEC tSQLt.AssertEquals 110, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test IDENTITY_INSERT is an error when it is off]
AS
BEGIN
    CREATE TABLE dbo.idt (id INT IDENTITY(1,1), v INT);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 544;
    INSERT dbo.idt (id, v) VALUES (7, 1);
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a rolled back insert still consumes an identity value]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.idt (id INT IDENTITY(1,1), v INT);
    SAVE TRAN s;
    INSERT dbo.idt (v) VALUES (1);
    ROLLBACK TRAN s;
    INSERT dbo.idt (v) VALUES (2);
    SET @tsqlt_v = SCOPE_IDENTITY();
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a sequence hands out increasing values]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE SEQUENCE dbo.sq START WITH 100 INCREMENT BY 5;
    DECLARE @a INT = NEXT VALUE FOR dbo.sq, @b INT = NEXT VALUE FOR dbo.sq;
    EXEC tSQLt.AssertEquals 100, @a;
    EXEC tSQLt.AssertEquals 105, @b;
    CREATE TABLE dbo.sqt (id INT DEFAULT (NEXT VALUE FOR dbo.sq), v INT);
    INSERT dbo.sqt (v) VALUES (1), (2);
    SET @tsqlt_v = (SELECT MIN(id) FROM dbo.sqt);
    EXEC tSQLt.AssertEquals 110, @tsqlt_v;
    SET @tsqlt_v = (SELECT MAX(id) FROM dbo.sqt);
    EXEC tSQLt.AssertEquals 115, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test computed columns and defaults]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.cc (a INT, b INT, s AS a + b, p AS a * b PERSISTED, d INT DEFAULT 7, t DATETIME DEFAULT GETDATE());
    INSERT dbo.cc (a, b) VALUES (3, 4);
    SET @tsqlt_v = (SELECT s FROM dbo.cc);
    EXEC tSQLt.AssertEquals 7, @tsqlt_v;
    SET @tsqlt_v = (SELECT p FROM dbo.cc);
    EXEC tSQLt.AssertEquals 12, @tsqlt_v;
    SET @tsqlt_v = (SELECT d FROM dbo.cc);
    EXEC tSQLt.AssertEquals 7, @tsqlt_v;
    UPDATE dbo.cc SET a = 10;
    SET @tsqlt_v = (SELECT s FROM dbo.cc);
    EXEC tSQLt.AssertEquals 14, @tsqlt_v;
    SET @tsqlt_v = (SELECT p FROM dbo.cc);
    EXEC tSQLt.AssertEquals 40, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test AFTER triggers see inserted and deleted]
AS
BEGIN
    CREATE TABLE dbo.tr (id INT PRIMARY KEY, v INT);
    CREATE TABLE dbo.audit_log (what VARCHAR(10), id INT, oldv INT, newv INT);
    EXEC('CREATE TRIGGER dbo.trg_tr ON dbo.tr AFTER INSERT, UPDATE, DELETE AS
          BEGIN
              INSERT dbo.audit_log SELECT ''ins'', i.id, NULL, i.v FROM inserted i WHERE NOT EXISTS (SELECT 1 FROM deleted);
              INSERT dbo.audit_log SELECT ''upd'', i.id, d.v, i.v FROM inserted i JOIN deleted d ON d.id = i.id;
              INSERT dbo.audit_log SELECT ''del'', d.id, d.v, NULL FROM deleted d WHERE NOT EXISTS (SELECT 1 FROM inserted);
          END');
    INSERT dbo.tr VALUES (1, 10), (2, 20);
    UPDATE dbo.tr SET v = v + 1 WHERE id = 1;
    DELETE dbo.tr WHERE id = 2;
    CREATE TABLE #actual (what VARCHAR(10), id INT, oldv INT, newv INT);
    INSERT #actual SELECT * FROM dbo.audit_log;
    CREATE TABLE #expected (what VARCHAR(10), id INT, oldv INT, newv INT);
    INSERT #expected VALUES ('ins', 1, NULL, 10), ('ins', 2, NULL, 20), ('upd', 1, 10, 11), ('del', 2, 20, NULL);
    EXEC tSQLt.AssertEqualsTable '#expected', '#actual';
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test UPDATE function in a trigger tells which columns were set]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.tr (id INT PRIMARY KEY, a INT, b INT);
    CREATE TABLE dbo.audit_log (what VARCHAR(10));
    EXEC('CREATE TRIGGER dbo.trg_cols ON dbo.tr AFTER UPDATE AS BEGIN IF UPDATE(a) INSERT dbo.audit_log VALUES (''a''); IF UPDATE(b) INSERT dbo.audit_log VALUES (''b''); END');
    INSERT dbo.tr VALUES (1, 1, 1);
    UPDATE dbo.tr SET a = 2;
    UPDATE dbo.tr SET b = 2;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.audit_log);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.audit_log WHERE what = 'a');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a view with CHECK OPTION rejects rows that would disappear]
AS
BEGIN
    CREATE TABLE dbo.base (id INT PRIMARY KEY, v INT);
    EXEC('CREATE VIEW dbo.vw AS SELECT id, v FROM dbo.base WHERE v > 0 WITH CHECK OPTION');
    INSERT dbo.vw VALUES (1, 5);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 550;
    INSERT dbo.vw VALUES (2, -5);
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test an updatable view writes through to its table]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.base (id INT PRIMARY KEY, v INT);
    INSERT dbo.base VALUES (1, 1), (2, 2);
    EXEC('CREATE VIEW dbo.vw AS SELECT id, v FROM dbo.base WHERE id > 1');
    UPDATE dbo.vw SET v = 99;
    SET @tsqlt_v = (SELECT v FROM dbo.base WHERE id = 1);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT v FROM dbo.base WHERE id = 2);
    EXEC tSQLt.AssertEquals 99, @tsqlt_v;
    DELETE dbo.vw;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.base);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a filtered unique index allows duplicates outside its filter]
AS
BEGIN
    CREATE TABLE dbo.fi (id INT, code VARCHAR(5), active BIT);
    CREATE UNIQUE INDEX ux ON dbo.fi (code) WHERE active = 1;
    INSERT dbo.fi VALUES (1, 'a', 1), (2, 'a', 0), (3, 'a', 0);
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 2601;
    INSERT dbo.fi VALUES (4, 'a', 1);
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a table type is passed to a procedure as READONLY]
AS
BEGIN
    EXEC('CREATE TYPE dbo.IdList AS TABLE (id INT PRIMARY KEY)');
    EXEC('CREATE PROCEDURE dbo.p_sum @ids dbo.IdList READONLY, @total INT OUTPUT AS SELECT @total = SUM(id) FROM @ids');
    DECLARE @l dbo.IdList, @t INT;
    INSERT @l VALUES (1), (2), (3);
    EXEC dbo.p_sum @l, @t OUTPUT;
    EXEC tSQLt.AssertEquals 6, @t;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test an alias type behaves as its base type]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    EXEC('CREATE TYPE dbo.Money2 FROM DECIMAL(9,2) NOT NULL');
    CREATE TABLE dbo.at (m dbo.Money2);
    INSERT dbo.at VALUES (1.239);
    SET @tsqlt_v = (SELECT m FROM dbo.at);
    EXEC tSQLt.AssertEquals 1.24, @tsqlt_v;
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 515;
    INSERT dbo.at VALUES (NULL);
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test STRING_SPLIT STRING_AGG and FOR XML PATH]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_v = (SELECT COUNT(*) FROM STRING_SPLIT('a,b,c', ','));
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_s = (SELECT STRING_AGG(value, '|') WITHIN GROUP (ORDER BY value) FROM STRING_SPLIT('c,a,b', ','));
    EXEC tSQLt.AssertEqualsString N'a|b|c', @tsqlt_s;
    CREATE TABLE dbo.xs (v VARCHAR(5));
    INSERT dbo.xs VALUES ('x'), ('y'), ('z');
    SET @tsqlt_s = (SELECT (SELECT ',' + v FROM dbo.xs ORDER BY v FOR XML PATH('')));
    EXEC tSQLt.AssertEqualsString N',x,y,z', @tsqlt_s;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test JSON_VALUE OPENJSON and FOR JSON]
AS
BEGIN
    DECLARE @tsqlt_s NVARCHAR(MAX);
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @j NVARCHAR(MAX) = N'{"a":1,"b":{"c":"x"},"arr":[10,20,30]}';
    SET @tsqlt_s = JSON_VALUE(@j, '$.a');
    EXEC tSQLt.AssertEqualsString N'1', @tsqlt_s;
    SET @tsqlt_s = JSON_VALUE(@j, '$.b.c');
    EXEC tSQLt.AssertEqualsString N'x', @tsqlt_s;
    SET @tsqlt_s = JSON_VALUE(@j, '$.arr[1]');
    EXEC tSQLt.AssertEqualsString N'20', @tsqlt_s;
    SET @tsqlt_v = (SELECT COUNT(*) FROM OPENJSON(@j, '$.arr'));
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    CREATE TABLE dbo.js (id INT, name VARCHAR(5));
    INSERT dbo.js VALUES (1, 'a'), (2, 'b');
    SET @tsqlt_s = (SELECT id, name FROM dbo.js ORDER BY id FOR JSON PATH);
    EXEC tSQLt.AssertEqualsString N'[{"id":1,"name":"a"},{"id":2,"name":"b"}]', @tsqlt_s;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test TOP WITH TIES and OFFSET FETCH]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.tp (v INT);
    INSERT dbo.tp VALUES (1), (2), (2), (3), (4);
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT TOP (3) WITH TIES v FROM dbo.tp ORDER BY v DESC) x);
    EXEC tSQLt.AssertEquals 4, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT v FROM dbo.tp ORDER BY v OFFSET 1 ROWS FETCH NEXT 2 ROWS ONLY) x);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT MIN(v) FROM (SELECT v FROM dbo.tp ORDER BY v OFFSET 3 ROWS) x);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test set operators treat NULLs as equal]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT 1 AS a UNION SELECT NULL UNION SELECT NULL) x);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT 1 AS a UNION ALL SELECT NULL UNION ALL SELECT NULL) x);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT NULL AS a INTERSECT SELECT NULL) x);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT 1 AS a EXCEPT SELECT NULL) x);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test NOT IN with a NULL in the list selects nothing]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.ni (a INT);
    INSERT dbo.ni VALUES (1), (2), (3);
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.ni WHERE a NOT IN (1, NULL));
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.ni WHERE a NOT IN (1, 5));
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.ni n WHERE NOT EXISTS (SELECT 1 FROM (SELECT 1 AS x UNION ALL SELECT NULL) q WHERE q.x = n.a));
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test GROUP BY ROLLUP CUBE and GROUPING SETS]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.gb (a VARCHAR(1), b VARCHAR(1), n INT);
    INSERT dbo.gb VALUES ('x', 'p', 1), ('x', 'q', 2), ('y', 'p', 4);
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT a, b, SUM(n) s FROM dbo.gb GROUP BY ROLLUP (a, b)) r);
    EXEC tSQLt.AssertEquals 6, @tsqlt_v;
    SET @tsqlt_v = (SELECT SUM(s) FROM (SELECT a, b, SUM(n) s FROM dbo.gb GROUP BY ROLLUP (a, b)) r WHERE a IS NULL AND b IS NULL);
    EXEC tSQLt.AssertEquals 7, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT a, b, SUM(n) s FROM dbo.gb GROUP BY CUBE (a, b)) c);
    EXEC tSQLt.AssertEquals 8, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT a, b, SUM(n) s FROM dbo.gb GROUP BY GROUPING SETS ((a), (b), ())) g);
    EXEC tSQLt.AssertEquals 5, @tsqlt_v;
    SET @tsqlt_v = (SELECT GROUPING(a) FROM dbo.gb GROUP BY ROLLUP (a) HAVING GROUPING(a) = 1);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a SELECT assignment keeps the last row and keeps the old value when there is none]
AS
BEGIN
    CREATE TABLE dbo.sa (id INT, v INT);
    INSERT dbo.sa VALUES (1, 10), (2, 20), (3, 30);
    DECLARE @v INT = -1;
    SELECT @v = v FROM dbo.sa ORDER BY id;
    EXEC tSQLt.AssertEquals 30, @v;
    SELECT @v = v FROM dbo.sa WHERE id = 99;
    EXEC tSQLt.AssertEquals 30, @v;
    SET @v = (SELECT v FROM dbo.sa WHERE id = 99);
    EXEC tSQLt.AssertEquals NULL, @v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test ROWCOUNT after DML and SET NOCOUNT]
AS
BEGIN
    CREATE TABLE dbo.rc (a INT);
    INSERT dbo.rc VALUES (1), (2), (3);
    UPDATE dbo.rc SET a = a + 1 WHERE a > 1;
    EXEC tSQLt.AssertEquals 2, @@ROWCOUNT;
    DELETE dbo.rc;
    EXEC tSQLt.AssertEquals 3, @@ROWCOUNT;
    SET NOCOUNT ON;
    INSERT dbo.rc VALUES (1);
    EXEC tSQLt.AssertEquals 1, @@ROWCOUNT;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test a stored procedure with OUTPUT parameters defaults and a result set]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    EXEC('CREATE PROCEDURE dbo.p_full @a INT, @b INT = 5, @sum INT OUTPUT AS BEGIN SET @sum = @a + @b; SELECT @a AS a, @b AS b; RETURN 3; END');
    CREATE TABLE #r (a INT, b INT);
    DECLARE @s INT, @rc INT;
    INSERT #r EXEC @rc = dbo.p_full 1, @sum = @s OUTPUT;
    EXEC tSQLt.AssertEquals 6, @s;
    EXEC tSQLt.AssertEquals 3, @rc;
    SET @tsqlt_v = (SELECT b FROM #r);
    EXEC tSQLt.AssertEquals 5, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test procedures can be nested and recurse]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.p_fact @n INT, @r BIGINT OUTPUT AS BEGIN IF @n <= 1 BEGIN SET @r = 1; RETURN; END; DECLARE @s BIGINT, @m INT = @n - 1; EXEC dbo.p_fact @m, @s OUTPUT; SET @r = @n * @s; END');
    DECLARE @f BIGINT;
    EXEC dbo.p_fact 10, @f OUTPUT;
    EXEC tSQLt.AssertEquals 3628800, @f;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test the nesting limit of procedures is 32]
AS
BEGIN
    EXEC('CREATE PROCEDURE dbo.p_deep @n INT AS BEGIN IF @n > 0 BEGIN DECLARE @m INT = @n - 1; EXEC dbo.p_deep @m; END END');
    EXEC dbo.p_deep 20;
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 217;
    EXEC dbo.p_deep 40;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test synonyms and schemas]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    EXEC('CREATE SCHEMA other');
    CREATE TABLE other.thing (a INT);
    INSERT other.thing VALUES (7);
    EXEC('CREATE SYNONYM dbo.syn FOR other.thing');
    SET @tsqlt_v = (SELECT a FROM dbo.syn);
    EXEC tSQLt.AssertEquals 7, @tsqlt_v;
    INSERT dbo.syn VALUES (8);
    SET @tsqlt_v = (SELECT COUNT(*) FROM other.thing);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test sys catalog views describe what was created]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.cat1 (id INT PRIMARY KEY, name VARCHAR(20) NOT NULL, d DATE NULL);
    CREATE INDEX ix_cat1_name ON dbo.cat1 (name);
    SET @tsqlt_v = (SELECT COUNT(*) FROM sys.columns WHERE object_id = OBJECT_ID('dbo.cat1'));
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = (SELECT is_nullable FROM sys.columns WHERE object_id = OBJECT_ID('dbo.cat1') AND name = 'name');
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = (SELECT is_nullable FROM sys.columns WHERE object_id = OBJECT_ID('dbo.cat1') AND name = 'd');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID('dbo.cat1') AND name = 'ix_cat1_name');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT RTRIM(type) FROM sys.objects WHERE object_id = OBJECT_ID('dbo.cat1'));
    EXEC tSQLt.AssertEquals 'U', @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = 'cat1');
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
END;


GO

CREATE PROCEDURE ProgrammabilityTests.[test sys parameters lists procedure parameters]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_s NVARCHAR(MAX);
    EXEC('CREATE PROCEDURE dbo.p_params @a INT, @b NVARCHAR(30) = N''x'', @c DECIMAL(9,3) OUTPUT AS SELECT 1');
    SET @tsqlt_v = (SELECT COUNT(*) FROM sys.parameters WHERE object_id = OBJECT_ID('dbo.p_params'));
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_s = (SELECT name FROM sys.parameters WHERE object_id = OBJECT_ID('dbo.p_params') AND parameter_id = 2);
    EXEC tSQLt.AssertEqualsString N'@b', @tsqlt_s;
    SET @tsqlt_v = (SELECT max_length FROM sys.parameters WHERE object_id = OBJECT_ID('dbo.p_params') AND name = '@b');
    EXEC tSQLt.AssertEquals 60, @tsqlt_v;
    SET @tsqlt_v = (SELECT has_default_value FROM sys.parameters WHERE object_id = OBJECT_ID('dbo.p_params') AND name = '@b');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT is_output FROM sys.parameters WHERE object_id = OBJECT_ID('dbo.p_params') AND name = '@c');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT scale FROM sys.parameters WHERE object_id = OBJECT_ID('dbo.p_params') AND name = '@c');
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
END;


GO
