-- Data types, conversions, NULL logic, collation, and the built-in string, numeric, date and hashing functions.
EXEC tSQLt.NewTestClass 'TypeTests';

GO

CREATE PROCEDURE TypeTests.[test integer division truncates and a decimal operand makes the result decimal]
AS
BEGIN
    DECLARE @a INT = 10 / 4, @b DECIMAL(12,6) = 10 / 4.0, @c INT = -7 / 2, @d INT = -7 % 3, @e INT = 7 % -3;
    EXEC tSQLt.AssertEquals 2, @a;
    EXEC tSQLt.AssertEquals 2.5, @b;
    EXEC tSQLt.AssertEquals -3, @c;
    EXEC tSQLt.AssertEquals -1, @d;
    EXEC tSQLt.AssertEquals 1, @e;
END;

GO

CREATE PROCEDURE TypeTests.[test decimal arithmetic gives the documented scale]
AS
BEGIN
    DECLARE @s INT, @m DECIMAL(20,4) = CAST(1.5 AS DECIMAL(10,2)) * CAST(2.25 AS DECIMAL(10,2));
    SET @s = CAST(SQL_VARIANT_PROPERTY(CAST(1.5 AS DECIMAL(10,2)) * CAST(2.25 AS DECIMAL(10,2)), 'Scale') AS INT);
    EXEC tSQLt.AssertEquals 4, @s;
    EXEC tSQLt.AssertEquals 3.375, @m;
END;
GO
CREATE PROCEDURE TypeTests.[test casting a decimal to an integer truncates and ROUND rounds half away from zero]
AS
BEGIN
    DECLARE @a INT = CAST(1.9 AS INT), @b INT = CAST(-1.9 AS INT), @c DECIMAL(5,1) = ROUND(2.5, 0), @d DECIMAL(5,1) = ROUND(-2.5, 0), @e DECIMAL(6,2) = ROUND(1.2345, 2), @f DECIMAL(6,0) = ROUND(1234.5, -2);
    EXEC tSQLt.AssertEquals 1, @a;
    EXEC tSQLt.AssertEquals -1, @b;
    EXEC tSQLt.AssertEquals 3.0, @c;
    EXEC tSQLt.AssertEquals -3.0, @d;
    EXEC tSQLt.AssertEquals 1.23, @e;
    EXEC tSQLt.AssertEquals 1200, @f;
END;

GO

CREATE PROCEDURE TypeTests.[test FLOAT is approximate and DECIMAL is exact]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @f1 FLOAT = 0.1, @f2 FLOAT = 0.2, @f3 FLOAT = 0.3, @d1 DECIMAL(5,2) = 0.1, @d2 DECIMAL(5,2) = 0.2, @d3 DECIMAL(5,2) = 0.3;
    SET @tsqlt_v = CASE WHEN @f1 + @f2 = @f3 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN @d1 + @d2 = @d3 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test the result type follows data type precedence]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_v = CAST(SQL_VARIANT_PROPERTY(1 + 1, 'BaseType') AS VARCHAR(20));
    EXEC tSQLt.AssertEquals 'int', @tsqlt_v;
    SET @tsqlt_v = CAST(SQL_VARIANT_PROPERTY(CAST(1 AS INT) + CAST(1 AS BIGINT), 'BaseType') AS VARCHAR(20));
    EXEC tSQLt.AssertEquals 'bigint', @tsqlt_v;
    SET @tsqlt_v = CAST(SQL_VARIANT_PROPERTY(1 + 1.5, 'BaseType') AS VARCHAR(20));
    EXEC tSQLt.AssertEquals 'numeric', @tsqlt_v;
    SET @tsqlt_v = CAST(SQL_VARIANT_PROPERTY(1 + 1.5e0, 'BaseType') AS VARCHAR(20));
    EXEC tSQLt.AssertEquals 'float', @tsqlt_v;
    SET @tsqlt_v = 1 + '2';
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_s = '1' + '2';
    EXEC tSQLt.AssertEqualsString '12', @tsqlt_s;
END;

GO

CREATE PROCEDURE TypeTests.[test NULL in comparisons and logic]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CASE WHEN NULL = NULL THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN NULL <> 1 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN NULL IS NULL THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN NULL OR 1 = 1 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN NULL AND 1 = 1 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN NOT (NULL = 1) THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test COALESCE ISNULL NULLIF and arithmetic with NULL]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_v = COALESCE(NULL, NULL, 3, 4);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = ISNULL(NULL, 5);
    EXEC tSQLt.AssertEquals 5, @tsqlt_v;
    SET @tsqlt_v = NULLIF(4, 4);
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = NULLIF(4, 5);
    EXEC tSQLt.AssertEquals 4, @tsqlt_v;
    SET @tsqlt_v = 1 + NULL;
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = NULL * 0;
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_s = ISNULL(CAST(NULL AS VARCHAR(2)) + 'x', 'ab');
    EXEC tSQLt.AssertEqualsString 'ab', @tsqlt_s;
END;

GO

CREATE PROCEDURE TypeTests.[test aggregates ignore NULLs and an empty SUM is NULL]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.ag (v INT);
    INSERT dbo.ag VALUES (1), (NULL), (3);
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.ag);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(v) FROM dbo.ag);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT SUM(v) FROM dbo.ag);
    EXEC tSQLt.AssertEquals 4, @tsqlt_v;
    SET @tsqlt_v = (SELECT AVG(v) FROM dbo.ag);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT SUM(v) FROM dbo.ag WHERE v > 100);
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(v) FROM dbo.ag WHERE v > 100);
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = (SELECT MAX(v) FROM dbo.ag);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test AVG of integers is an integer]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.ag (v INT);
    INSERT dbo.ag VALUES (1), (2);
    SET @tsqlt_v = (SELECT AVG(v) FROM dbo.ag);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT AVG(CAST(v AS DECIMAL(10,2))) FROM dbo.ag);
    EXEC tSQLt.AssertEquals 1.5, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test string comparison ignores trailing spaces and case with the default collation]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CASE WHEN 'a' = 'a   ' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'a' = 'A' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN ' a' = 'a' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'a' = 'A' COLLATE Latin1_General_CS_AS THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'abc' < 'abd' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test LEN ignores trailing spaces and DATALENGTH does not]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = LEN('abc   ');
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = DATALENGTH('abc   ');
    EXEC tSQLt.AssertEquals 6, @tsqlt_v;
    DECLARE @n NVARCHAR(10) = N'abc';
    SET @tsqlt_v = DATALENGTH(@n);
    EXEC tSQLt.AssertEquals 6, @tsqlt_v;
    SET @tsqlt_v = LEN('');
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = LEN(NULL);
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test string functions]
AS
BEGIN
    DECLARE @tsqlt_s NVARCHAR(MAX);
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_s = SUBSTRING('abcdef', 2, 3);
    EXEC tSQLt.AssertEqualsString 'bcd', @tsqlt_s;
    SET @tsqlt_s = SUBSTRING('abcdef', 6, 10);
    EXEC tSQLt.AssertEqualsString 'f', @tsqlt_s;
    SET @tsqlt_s = SUBSTRING('abc', 10, 2);
    EXEC tSQLt.AssertEqualsString '', @tsqlt_s;
    SET @tsqlt_v = CHARINDEX('c', 'abcabc');
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = CHARINDEX('c', 'abcabc', 4);
    EXEC tSQLt.AssertEquals 6, @tsqlt_v;
    SET @tsqlt_v = CHARINDEX('z', 'abc');
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = PATINDEX('%c_e%', 'abcde');
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_s = STUFF('abcdef', 3, 2, 'XX');
    EXEC tSQLt.AssertEqualsString 'abXXef', @tsqlt_s;
    SET @tsqlt_s = REVERSE('abcdef');
    EXEC tSQLt.AssertEqualsString 'fedcba', @tsqlt_s;
    SET @tsqlt_s = REPLACE('a b c', ' ', '-');
    EXEC tSQLt.AssertEqualsString 'a-b-c', @tsqlt_s;
    SET @tsqlt_s = REPLICATE('abc', 2);
    EXEC tSQLt.AssertEqualsString 'abcabc', @tsqlt_s;
    SET @tsqlt_s = LEFT('abcd', 2);
    EXEC tSQLt.AssertEqualsString 'ab', @tsqlt_s;
    SET @tsqlt_s = RIGHT('abcd', 2);
    EXEC tSQLt.AssertEqualsString 'cd', @tsqlt_s;
    SET @tsqlt_s = UPPER('aBc');
    EXEC tSQLt.AssertEqualsString 'ABC', @tsqlt_s;
    SET @tsqlt_s = LOWER('aBc');
    EXEC tSQLt.AssertEqualsString 'abc', @tsqlt_s;
    SET @tsqlt_s = 'a' + SPACE(2) + 'b';
    EXEC tSQLt.AssertEqualsString 'a  b', @tsqlt_s;
END;

GO

CREATE PROCEDURE TypeTests.[test trimming functions]
AS
BEGIN
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_s = LTRIM('  a  ');
    EXEC tSQLt.AssertEqualsString 'a  ', @tsqlt_s;
    SET @tsqlt_s = RTRIM('  a  ');
    EXEC tSQLt.AssertEqualsString '  a', @tsqlt_s;
    SET @tsqlt_s = TRIM('  a  ');
    EXEC tSQLt.AssertEqualsString 'a', @tsqlt_s;
    SET @tsqlt_s = TRIM('xy' FROM 'xyaxy');
    EXEC tSQLt.AssertEqualsString 'a', @tsqlt_s;
END;

GO

CREATE PROCEDURE TypeTests.[test concatenation with NULL and CONCAT and CONCAT_WS]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_v = 'a' + NULL;
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_s = CONCAT('a', NULL, 'b');
    EXEC tSQLt.AssertEqualsString 'ab', @tsqlt_s;
    SET @tsqlt_s = CONCAT_WS(',', 'a', 'b', 'c');
    EXEC tSQLt.AssertEqualsString 'a,b,c', @tsqlt_s;
    SET @tsqlt_s = CONCAT_WS(',', 'a', NULL, 'c');
    EXEC tSQLt.AssertEqualsString 'a,c', @tsqlt_s;
END;

GO

CREATE PROCEDURE TypeTests.[test LIKE patterns wildcards sets and ESCAPE]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CASE WHEN 'abc' LIKE 'a%' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'abc' LIKE 'a_c' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'b' LIKE '[a-c]' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'd' LIKE '[a-c]' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'd' LIKE '[^a-c]' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN '50%' LIKE '50\%' ESCAPE '\' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN '500' LIKE '50\%' ESCAPE '\' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN 'a[b' LIKE 'a[[]b' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test ASCII CHAR UNICODE and NCHAR]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_s NVARCHAR(MAX);
    SET @tsqlt_v = ASCII('A');
    EXEC tSQLt.AssertEquals 65, @tsqlt_v;
    SET @tsqlt_s = CHAR(66);
    EXEC tSQLt.AssertEqualsString 'B', @tsqlt_s;
    SET @tsqlt_v = UNICODE(N'ä');
    EXEC tSQLt.AssertEquals 228, @tsqlt_v;
    SET @tsqlt_v = NCHAR(228);
    EXEC tSQLt.AssertEquals N'ä', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test numeric functions]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = ABS(-5);
    EXEC tSQLt.AssertEquals 5, @tsqlt_v;
    SET @tsqlt_v = SIGN(-9);
    EXEC tSQLt.AssertEquals -1, @tsqlt_v;
    SET @tsqlt_v = CEILING(2.1);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = FLOOR(2.9);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = CEILING(-2.9);
    EXEC tSQLt.AssertEquals -2, @tsqlt_v;
    SET @tsqlt_v = POWER(2, 3);
    EXEC tSQLt.AssertEquals 8, @tsqlt_v;
    SET @tsqlt_v = SQRT(9);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = SQUARE(5);
    EXEC tSQLt.AssertEquals 25, @tsqlt_v;
    SET @tsqlt_v = LOG(EXP(1));
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = ROUND(PI(), 2);
    EXEC tSQLt.AssertEquals 3.14, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN RAND(42) = RAND(42) THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test bitwise operators]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = 12 & 6;
    EXEC tSQLt.AssertEquals 4, @tsqlt_v;
    SET @tsqlt_v = 12 | 6;
    EXEC tSQLt.AssertEquals 14, @tsqlt_v;
    SET @tsqlt_v = 12 ^ 6;
    EXEC tSQLt.AssertEquals 10, @tsqlt_v;
    SET @tsqlt_v = ~12;
    EXEC tSQLt.AssertEquals -13, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test integer overflow limits]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CAST(2147483647 AS INT);
    EXEC tSQLt.AssertEquals 2147483647, @tsqlt_v;
    SET @tsqlt_v = CAST(2147483647 AS BIGINT) + 1;
    EXEC tSQLt.AssertEquals 2147483648, @tsqlt_v;
    SET @tsqlt_v = CAST(255 AS TINYINT);
    EXEC tSQLt.AssertEquals 255, @tsqlt_v;
    SET @tsqlt_v = CAST(-32768 AS SMALLINT);
    EXEC tSQLt.AssertEquals -32768, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test DATEADD and DATEDIFF count boundaries]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = DATEDIFF(DAY, '2024-01-01 23:59:00', '2024-01-02 00:01:00');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = DATEDIFF(YEAR, '2023-12-31', '2024-01-01');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = DATEDIFF(MONTH, '2024-01-31', '2024-02-01');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = DATEDIFF(MONTH, '2024-01-01', '2024-01-31');
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = DATEDIFF(MINUTE, '2024-01-01 10:00', '2024-01-01 12:00');
    EXEC tSQLt.AssertEquals 120, @tsqlt_v;
    SET @tsqlt_v = CONVERT(CHAR(10), DATEADD(MONTH, 1, '2024-01-31'), 23);
    EXEC tSQLt.AssertEquals '2024-02-29', @tsqlt_v;
    SET @tsqlt_v = CONVERT(CHAR(10), DATEADD(YEAR, -1, '2024-02-29'), 23);
    EXEC tSQLt.AssertEquals '2023-02-28', @tsqlt_v;
    SET @tsqlt_v = CONVERT(CHAR(10), DATEADD(DAY, 1, '2024-02-29'), 23);
    EXEC tSQLt.AssertEquals '2024-03-01', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test DATEPART DATENAME and EOMONTH]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_s NVARCHAR(MAX);
    DECLARE @d DATE = '2024-03-15';
    SET @tsqlt_v = DATEPART(YEAR, @d);
    EXEC tSQLt.AssertEquals 2024, @tsqlt_v;
    SET @tsqlt_v = DATEPART(MONTH, @d);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = DATEPART(DAY, @d);
    EXEC tSQLt.AssertEquals 15, @tsqlt_v;
    SET @tsqlt_v = DATEPART(WEEKDAY, @d);
    EXEC tSQLt.AssertEquals 6, @tsqlt_v;
    SET @tsqlt_v = DATEPART(DAYOFYEAR, @d);
    EXEC tSQLt.AssertEquals 75, @tsqlt_v;
    SET @tsqlt_v = DATEPART(QUARTER, @d);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_s = DATENAME(MONTH, @d);
    EXEC tSQLt.AssertEqualsString 'March', @tsqlt_s;
    SET @tsqlt_s = DATENAME(WEEKDAY, @d);
    EXEC tSQLt.AssertEqualsString 'Friday', @tsqlt_s;
    SET @tsqlt_v = CONVERT(CHAR(10), EOMONTH(@d), 23);
    EXEC tSQLt.AssertEquals '2024-03-31', @tsqlt_v;
    SET @tsqlt_v = CONVERT(CHAR(10), EOMONTH('2024-02-10'), 23);
    EXEC tSQLt.AssertEquals '2024-02-29', @tsqlt_v;
    SET @tsqlt_v = CONVERT(CHAR(10), EOMONTH('2023-02-10'), 23);
    EXEC tSQLt.AssertEquals '2023-02-28', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test DATEFIRST changes the weekday number]
AS
BEGIN
    SET DATEFIRST 7;
    DECLARE @a INT = DATEPART(WEEKDAY, '2024-03-17');
    SET DATEFIRST 1;
    DECLARE @b INT = DATEPART(WEEKDAY, '2024-03-17');
    EXEC tSQLt.AssertEquals 1, @a;
    EXEC tSQLt.AssertEquals 7, @b;
END;

GO

CREATE PROCEDURE TypeTests.[test DATEFROMPARTS and its invalid arguments]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CONVERT(CHAR(10), DATEFROMPARTS(2024, 2, 29), 23);
    EXEC tSQLt.AssertEquals '2024-02-29', @tsqlt_v;
    EXEC tSQLt.ExpectException @ExpectedErrorNumber = 289;
    SELECT DATEFROMPARTS(2023, 2, 29);
END;

GO

CREATE PROCEDURE TypeTests.[test CONVERT date styles]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @d DATETIME = '2024-03-05 14:07:09';
    SET @tsqlt_v = CONVERT(VARCHAR(8), @d, 112);
    EXEC tSQLt.AssertEquals '20240305', @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(10), @d, 101);
    EXEC tSQLt.AssertEquals '03/05/2024', @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(10), @d, 103);
    EXEC tSQLt.AssertEquals '05/03/2024', @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(10), @d, 23);
    EXEC tSQLt.AssertEquals '2024-03-05', @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(19), @d, 120);
    EXEC tSQLt.AssertEquals '2024-03-05 14:07:09', @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(8), @d, 108);
    EXEC tSQLt.AssertEquals '14:07:09', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test date literals and ISDATE]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = ISDATE('2024-02-29');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = ISDATE('2023-02-29');
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = ISDATE('not a date');
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN CAST('2024-03-05' AS DATE) < CAST('2024-03-06' AS DATE) THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = DATEDIFF(DAY, CAST('2024-03-01' AS DATE), CAST('2024-03-08' AS DATETIME2));
    EXEC tSQLt.AssertEquals 7, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test time and datetime2 precision]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @t TIME(3) = '12:34:56.789', @d DATETIME2(7) = '2024-03-05 01:02:03.1234567';
    SET @tsqlt_v = CAST(@t AS VARCHAR(12));
    EXEC tSQLt.AssertEquals '12:34:56.789', @tsqlt_v;
    SET @tsqlt_v = CAST(@d AS VARCHAR(30));
    EXEC tSQLt.AssertEquals '2024-03-05 01:02:03.1234567', @tsqlt_v;
    SET @tsqlt_v = DATEPART(MILLISECOND, CAST('2024-03-05 01:02:03.123' AS DATETIME2(3)));
    EXEC tSQLt.AssertEquals 123, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test datetime keeps time in ticks of three milliseconds]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @d DATETIME = '2024-01-01 00:00:00.004', @e DATETIME = '2024-01-01 00:00:00.123', @f DATETIME = '2024-01-01 23:59:59.999';
    SET @tsqlt_v = DATEPART(MILLISECOND, @d);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
    SET @tsqlt_v = DATEPART(MILLISECOND, @e);
    EXEC tSQLt.AssertEquals 123, @tsqlt_v;
    SET @tsqlt_v = CONVERT(CHAR(10), @f, 23);
    EXEC tSQLt.AssertEquals '2024-01-02', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test BIT conversion]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CAST('true' AS BIT);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CAST('false' AS BIT);
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CAST(5 AS BIT);
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CAST(0 AS BIT);
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = CAST(NULL AS BIT);
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test binary and hex conversions]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @tsqlt_ve SQL_VARIANT;
    SET @tsqlt_v = CAST(0xFF AS INT);
    EXEC tSQLt.AssertEquals 255, @tsqlt_v;
    SET @tsqlt_ve = 0x00FF;
    SET @tsqlt_v = CAST(255 AS BINARY(2));
    EXEC tSQLt.AssertEquals @tsqlt_ve, @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(10), CONVERT(VARBINARY(10), 'abc'), 2);
    EXEC tSQLt.AssertEquals '616263', @tsqlt_v;
    SET @tsqlt_v = DATALENGTH(0x010203);
    EXEC tSQLt.AssertEquals 3, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test HASHBYTES gives the standard digests]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CONVERT(VARCHAR(40), HASHBYTES('MD5', 'abc'), 2);
    EXEC tSQLt.AssertEquals '900150983CD24FB0D6963F7D28E17F72', @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(60), HASHBYTES('SHA1', 'abc'), 2);
    EXEC tSQLt.AssertEquals 'A9993E364706816ABA3E25717850C26C9CD0D89D', @tsqlt_v;
    SET @tsqlt_v = CONVERT(VARCHAR(80), HASHBYTES('SHA2_256', 'abc'), 2);
    EXEC tSQLt.AssertEquals 'BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test uniqueidentifier values]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @a UNIQUEIDENTIFIER = NEWID(), @b UNIQUEIDENTIFIER = NEWID();
    SET @tsqlt_v = CASE WHEN @a = @b THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 0, @tsqlt_v;
    SET @tsqlt_v = LEN(CAST(@a AS VARCHAR(40)));
    EXEC tSQLt.AssertEquals 36, @tsqlt_v;
    DECLARE @g UNIQUEIDENTIFIER = '6F9619FF-8B86-D011-B42D-00C04FC964FF';
    SET @tsqlt_v = CAST(@g AS VARCHAR(40));
    EXEC tSQLt.AssertEquals '6F9619FF-8B86-D011-B42D-00C04FC964FF', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test SQL_VARIANT keeps the base type]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @v SQL_VARIANT = CAST(42 AS INT), @w SQL_VARIANT = 'text';
    SET @tsqlt_v = CAST(SQL_VARIANT_PROPERTY(@v, 'BaseType') AS VARCHAR(20));
    EXEC tSQLt.AssertEquals 'int', @tsqlt_v;
    SET @tsqlt_v = CAST(SQL_VARIANT_PROPERTY(@w, 'BaseType') AS VARCHAR(20));
    EXEC tSQLt.AssertEquals 'varchar', @tsqlt_v;
    SET @tsqlt_v = CAST(@v AS INT);
    EXEC tSQLt.AssertEquals 42, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test MONEY keeps four decimals and rounds]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @m MONEY = 1.23456;
    SET @tsqlt_v = CAST(@m AS DECIMAL(10,4));
    EXEC tSQLt.AssertEquals 1.2346, @tsqlt_v;
    DECLARE @n MONEY = 10 / 3;
    SET @tsqlt_v = CAST(@n AS DECIMAL(10,4));
    EXEC tSQLt.AssertEquals 3.0000, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test a string longer than its variable is silently truncated in an assignment]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @s VARCHAR(3) = 'abcdef';
    EXEC tSQLt.AssertEquals 'abc', @s;
    SET @tsqlt_v = CAST('abcdef' AS VARCHAR(3));
    EXEC tSQLt.AssertEquals 'abc', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test unicode literals need the N prefix]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    DECLARE @a NVARCHAR(10) = N'ä€', @b VARCHAR(10) = N'ä';
    SET @tsqlt_v = LEN(@a);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = UNICODE(SUBSTRING(@a, 2, 1));
    EXEC tSQLt.AssertEquals 8364, @tsqlt_v;
    SET @tsqlt_v = DATALENGTH(NCHAR(228));
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test ORDER BY follows the collation and sorts NULL first]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.ob (s VARCHAR(5));
    INSERT dbo.ob VALUES ('b'), ('A'), (NULL), ('a'), ('B');
    CREATE TABLE #actual (pos INT IDENTITY(1,1), s VARCHAR(5));
    INSERT #actual (s) SELECT s FROM dbo.ob ORDER BY s, CASE WHEN s COLLATE Latin1_General_CS_AS = s THEN 0 ELSE 1 END, s COLLATE Latin1_General_BIN2;
    SET @tsqlt_v = (SELECT s FROM #actual WHERE pos = 1);
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = (SELECT s FROM #actual WHERE pos = 2);
    EXEC tSQLt.AssertEquals 'A', @tsqlt_v;
    SET @tsqlt_v = (SELECT s FROM #actual WHERE pos = 3);
    EXEC tSQLt.AssertEquals 'a', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test a case-sensitive column collation]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.cs (s VARCHAR(5) COLLATE Latin1_General_CS_AS);
    INSERT dbo.cs VALUES ('a'), ('A');
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.cs WHERE s = 'a');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(DISTINCT s) FROM dbo.cs);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM dbo.cs WHERE s = 'A' COLLATE Latin1_General_CI_AS AND s = 'A');
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test GROUP BY and DISTINCT use the collation of the column]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.ci (s VARCHAR(5));
    INSERT dbo.ci VALUES ('a'), ('A'), ('b');
    SET @tsqlt_v = (SELECT COUNT(DISTINCT s) FROM dbo.ci);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
    SET @tsqlt_v = (SELECT COUNT(*) FROM (SELECT s FROM dbo.ci GROUP BY s) g);
    EXEC tSQLt.AssertEquals 2, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test FORMAT and a few conversions to strings]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = FORMAT(42, '0000');
    EXEC tSQLt.AssertEquals '0042', @tsqlt_v;
    SET @tsqlt_v = FORMAT(CAST('2024-03-05' AS DATE), 'yyyy-MM-dd');
    EXEC tSQLt.AssertEquals '2024-03-05', @tsqlt_v;
    SET @tsqlt_v = FORMAT(1234.5, 'N2', 'en-US');
    EXEC tSQLt.AssertEquals '1,234.50', @tsqlt_v;
    SET @tsqlt_v = CAST(12 AS VARCHAR(10));
    EXEC tSQLt.AssertEquals '12', @tsqlt_v;
    SET @tsqlt_v = CAST(CAST(1.5 AS DECIMAL(4,1)) AS VARCHAR(10));
    EXEC tSQLt.AssertEquals '1.5', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test TRY_PARSE-style date checks with TRY_CONVERT]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CONVERT(VARCHAR(10), TRY_CONVERT(DATE, '2024-03-05'), 23);
    EXEC tSQLt.AssertEquals '2024-03-05', @tsqlt_v;
    SET @tsqlt_v = TRY_CONVERT(DATE, '2024-02-30');
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = TRY_CONVERT(DECIMAL(5,2), '1e5x');
    EXEC tSQLt.AssertEquals NULL, @tsqlt_v;
    SET @tsqlt_v = TRY_CONVERT(DECIMAL(5,2), '12.5');
    EXEC tSQLt.AssertEquals 12.50, @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test identity-like numbering with ROW_NUMBER and no gaps]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    CREATE TABLE dbo.rn (v VARCHAR(5));
    INSERT dbo.rn VALUES ('c'), ('a'), ('b');
    SET @tsqlt_v = (SELECT v FROM (SELECT v, ROW_NUMBER() OVER (ORDER BY v) AS n FROM dbo.rn) x WHERE n = 2);
    EXEC tSQLt.AssertEquals 'b', @tsqlt_v;
END;

GO

CREATE PROCEDURE TypeTests.[test the system functions describe the session]
AS
BEGIN
    DECLARE @tsqlt_v SQL_VARIANT;
    SET @tsqlt_v = CASE WHEN @@SPID > 0 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN LEN(@@VERSION) > 10 THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN GETDATE() > '2020-01-01' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN SYSDATETIME() > '2020-01-01' THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = CASE WHEN DB_NAME() IS NOT NULL THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
    SET @tsqlt_v = USER_NAME();
    EXEC tSQLt.AssertEquals 'dbo', @tsqlt_v;
    SET @tsqlt_v = CASE WHEN @@SERVERNAME IS NOT NULL OR @@SERVERNAME IS NULL THEN 1 ELSE 0 END;
    EXEC tSQLt.AssertEquals 1, @tsqlt_v;
END;

GO
