//! Differences from SQL Server that were found and closed: EXEC arguments, stable schema ids, RANGE frames,
//! declared types of values (N'..' literals, decimal precision, SQL_VARIANT_PROPERTY).
mod common;
use common::*;

#[test]
fn exec_arguments_are_constants_variables_or_default_not_expressions() {
    let mut c = conn();
    script(&mut c, "CREATE PROCEDURE dbo.p @a INT = 1, @b VARCHAR(10) = 'x' AS SELECT @a, @b;");
    exec(&mut c, "DECLARE @v INT = 5; EXEC dbo.p @v, 'y'");
    exec(&mut c, "EXEC dbo.p -3, N'unicode'");
    exec(&mut c, "EXEC dbo.p @b = 'z', @a = 2");
    exec(&mut c, "EXEC dbo.p DEFAULT, DEFAULT");
    assert_eq!(scalar(&mut c, "EXEC dbo.p 7"), "7");
    for bad in ["EXEC dbo.p 1 + 1", "EXEC dbo.p ABS(-1)", "EXEC dbo.p GETDATE()", "EXEC dbo.p @a = LEN('x')", "EXEC dbo.p 'a' + 'b'", "EXEC dbo.p CAST(1 AS INT)", "EXEC dbo.p (SELECT 1)"] {
        assert_eq!(err_no(&mut c, bad), 102, "{}", bad);
    }
    // EXEC (string) still takes a concatenation, and sp_executesql its constants and variables
    assert_eq!(scalar(&mut c, "DECLARE @n NVARCHAR(20) = N'SELECT'; EXEC (@n + ' 4')"), "4");
    assert_eq!(scalar(&mut c, "DECLARE @x INT = 3; EXEC sp_executesql N'SELECT @p * 2', N'@p INT', @p = @x"), "6");
}

#[test]
fn schema_ids_are_stable_and_agree_between_schema_id_and_sys_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let (id_b, id_c);
    {
        let s = tsql_server::Server::open(dir.path()).unwrap();
        let mut c = s.connect();
        exec(&mut c, "CREATE SCHEMA bbb");
        id_b = scalar(&mut c, "SELECT SCHEMA_ID('bbb')");
        exec(&mut c, "CREATE SCHEMA ccc");
        id_c = scalar(&mut c, "SELECT SCHEMA_ID('ccc')");
        // a schema that sorts first must not renumber the others
        exec(&mut c, "CREATE SCHEMA aaa");
        assert_eq!(scalar(&mut c, "SELECT SCHEMA_ID('bbb')"), id_b);
        assert_eq!(scalar(&mut c, "SELECT SCHEMA_ID('ccc')"), id_c);
        let id_a = scalar(&mut c, "SELECT SCHEMA_ID('aaa')");
        assert!(id_a != id_b && id_a != id_c);
        assert_eq!(scalar(&mut c, "SELECT schema_id FROM sys.schemas WHERE name = 'aaa'"), id_a);
        assert_eq!(scalar(&mut c, "SELECT SCHEMA_NAME(SCHEMA_ID('bbb'))"), "bbb");
        assert_eq!(scalar(&mut c, "SELECT SCHEMA_ID('dbo')"), "1");
        // dropping one does not renumber the rest either
        exec(&mut c, "DROP SCHEMA bbb");
        assert_eq!(scalar(&mut c, "SELECT SCHEMA_ID('ccc')"), id_c);
        exec(&mut c, "CREATE SCHEMA ddd");
    }
    let s = tsql_server::Server::open(dir.path()).unwrap();
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT SCHEMA_ID('ccc')"), id_c, "ids survive a restart");
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT schema_id) FROM sys.schemas"), scalar(&mut c, "SELECT COUNT(*) FROM sys.schemas"));
}

#[test]
fn range_frames_accept_only_unbounded_and_current_row() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE w (x INT); INSERT w VALUES (1), (2), (2), (5);");
    assert_eq!(err_no(&mut c, "SELECT SUM(x) OVER (ORDER BY x RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w"), 4194);
    assert_eq!(err_no(&mut c, "SELECT SUM(x) OVER (ORDER BY x RANGE BETWEEN CURRENT ROW AND 2 FOLLOWING) FROM w"), 4194);
    assert_eq!(err_no(&mut c, "SELECT SUM(x) OVER (ORDER BY x RANGE 3 PRECEDING) FROM w"), 4194);
    // the supported forms, and ROWS with offsets, are unchanged
    assert_eq!(scalar(&mut c, "SELECT MAX(s) FROM (SELECT SUM(x) OVER (ORDER BY x RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) s FROM w) q"), "10");
    assert_eq!(scalar(&mut c, "SELECT MAX(s) FROM (SELECT SUM(x) OVER (ORDER BY x RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) s FROM w) q"), "10");
    assert_eq!(scalar(&mut c, "SELECT MAX(s) FROM (SELECT SUM(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) s FROM w) q"), "7");
}

#[test]
fn unicode_literals_are_nvarchar_and_everything_that_used_them_still_works() {
    let mut c = conn();
    assert_eq!(rows(&mut c, "SELECT DATALENGTH(N'abc'), DATALENGTH('abc'), DATALENGTH(N''), DATALENGTH(N'ä€')"), [["6", "3", "0", "4"]]);
    assert_eq!(scalar(&mut c, "SELECT N'abc'"), "abc");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN N'a' = 'A' THEN 'eq' ELSE 'ne' END"), "eq");
    assert_eq!(scalar(&mut c, "SELECT N'a' + 'b' + N'c'"), "abc");
    assert_eq!(scalar(&mut c, "SELECT DATALENGTH(UPPER(N'ab')) "), "4");
    assert_eq!(scalar(&mut c, "SELECT DATALENGTH(N'ab' + 'c')"), "6");
    script(&mut c, "CREATE TABLE u (id INT PRIMARY KEY, name NVARCHAR(20) DEFAULT N'dflt', note VARCHAR(20)); CREATE INDEX ix_u_name ON u (name); INSERT u (id, note) VALUES (1, 'a'); INSERT u VALUES (2, N'ünï', 'b'), (3, N'x', 'c');");
    assert_eq!(scalar(&mut c, "SELECT name FROM u WHERE id = 1"), "dflt");
    assert_eq!(scalar(&mut c, "SELECT id FROM u WHERE name = N'ünï'"), "2");
    // a unicode literal is still a constant: the index on name is used
    let plan = c.explain("SELECT id FROM u WHERE name = N'x'").unwrap().join("\n").to_lowercase();
    assert!(plan.contains("seek") || plan.contains("ix_u_name"), "{}", plan);
    assert_eq!(scalar(&mut c, "SELECT id FROM u WHERE name = N'x'"), "3");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM u WHERE name LIKE N'%n%'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM u WHERE name IN (N'x', N'dflt')"), "2");
    assert_eq!(scalar(&mut c, "DECLARE @s NVARCHAR(10) = N'q'; SELECT @s + N'r'"), "qr");
    assert_eq!(scalar(&mut c, "SELECT LEN(N'tail   ')"), "4");
    exec(&mut c, "EXEC sp_executesql N'SELECT 1', N''");
    assert_eq!(scalar(&mut c, "SELECT UNICODE(N'€')"), "8364");
    // sets of names where a string is expected stay strings
    exec(&mut c, "SET LANGUAGE N'british'");
    exec(&mut c, "SET LANGUAGE us_english");
}

#[test]
fn decimal_division_takes_its_scale_from_the_declared_precision_of_the_operands() {
    let mut c = conn();
    // scale = max(6, s1 + p2 + 1); precision = p1 - s1 + s2 + scale (scale given up, down to 6, beyond 38 digits)
    let q = |c: &mut tsql_server::Connection, e: &str| scalar(c, &format!("SELECT {}", e));
    assert_eq!(q(&mut c, "CAST(1 AS DECIMAL(10,2)) / CAST(3 AS DECIMAL(10,2))"), "0.3333333333333");
    assert_eq!(q(&mut c, "1.0 / 3"), "0.333333333333");
    assert_eq!(q(&mut c, "1.0 / 3.0"), "0.333333");
    assert_eq!(q(&mut c, "10 / 4.0"), "2.500000");
    assert_eq!(q(&mut c, "CAST(10 AS DECIMAL(5,1)) / 4"), "2.500000000000");
    assert_eq!(q(&mut c, "CAST(1 AS DECIMAL(38,10)) / CAST(3 AS DECIMAL(38,10))"), "0.333333");
    assert_eq!(q(&mut c, "CAST(2 AS DECIMAL(4,2)) / CAST(3 AS DECIMAL(4,2))"), "0.6666667");
    // a column and a variable carry their declared types
    script(&mut c, "CREATE TABLE d (a DECIMAL(10,2), b DECIMAL(10,2), i INT, s SMALLINT); INSERT d VALUES (1.00, 3.00, 7, 3);");
    assert_eq!(scalar(&mut c, "SELECT a / b FROM d"), "0.3333333333333");
    assert_eq!(scalar(&mut c, "SELECT a / i FROM d"), "0.1428571428571");
    assert_eq!(scalar(&mut c, "SELECT a / s FROM d"), "0.33333333");
    assert_eq!(scalar(&mut c, "DECLARE @x DECIMAL(8,3) = 1, @y DECIMAL(8,3) = 7; SELECT @x / @y"), "0.142857142857");
    // nested arithmetic is typed by the same rules: (a * b) has p = 21, s = 4
    assert_eq!(scalar(&mut c, "SELECT (a * b) / 7 FROM d"), "0.428571428571429");
    assert_eq!(scalar(&mut c, "SELECT 1 / (a + b) FROM d"), "0.250000000000");
    // other operators and an integer division are untouched
    assert_eq!(q(&mut c, "7 / 2"), "3");
    assert_eq!(q(&mut c, "CAST(7 AS DECIMAL(10,2)) * 2"), "14.00");
    assert_eq!(q(&mut c, "CAST(7.5 AS DECIMAL(10,2)) + 1"), "8.50");
    assert_eq!(q(&mut c, "CAST(1 AS DECIMAL(10,2)) / 0.5"), "2.000000");
    assert_eq!(err_no(&mut c, "SELECT CAST(1 AS DECIMAL(10,2)) / 0"), 8134);
}

#[test]
fn sql_variant_property_reports_declared_types() {
    let mut c = conn();
    let p = |c: &mut tsql_server::Connection, e: &str, prop: &str| scalar(c, &format!("SELECT CAST(SQL_VARIANT_PROPERTY({}, '{}') AS VARCHAR(30))", e, prop));
    assert_eq!(p(&mut c, "N'abc'", "BaseType"), "nvarchar");
    assert_eq!(p(&mut c, "'abc'", "BaseType"), "varchar");
    assert_eq!(p(&mut c, "1.5", "BaseType"), "numeric");
    assert_eq!(p(&mut c, "CAST(1.5 AS DECIMAL(10,2))", "BaseType"), "decimal");
    assert_eq!(p(&mut c, "CAST(1.5 AS NUMERIC(10,2))", "BaseType"), "numeric");
    assert_eq!(p(&mut c, "CAST(1.5 AS DECIMAL(10,2))", "Precision"), "10");
    assert_eq!(p(&mut c, "CAST(1.5 AS DECIMAL(10,2))", "Scale"), "2");
    assert_eq!(p(&mut c, "CAST(1.5 AS DECIMAL(10,2)) * CAST(2.25 AS DECIMAL(10,2))", "Precision"), "21");
    assert_eq!(p(&mut c, "CAST(1.5 AS DECIMAL(10,2)) * CAST(2.25 AS DECIMAL(10,2))", "Scale"), "4");
    assert_eq!(p(&mut c, "CAST(1 AS DECIMAL(10,2)) / CAST(3 AS DECIMAL(10,2))", "Scale"), "13");
    assert_eq!(p(&mut c, "CAST(1 AS DECIMAL(10,2)) / CAST(3 AS DECIMAL(10,2))", "Precision"), "23");
    assert_eq!(p(&mut c, "42", "BaseType"), "int");
    assert_eq!(p(&mut c, "CAST(42 AS BIGINT)", "BaseType"), "bigint");
    assert_eq!(p(&mut c, "1.5e0", "BaseType"), "float");
    script(&mut c, "CREATE TABLE t (m DECIMAL(12,3), n NVARCHAR(5), v VARCHAR(5)); INSERT t VALUES (1.5, N'a', 'b');");
    let col = |c: &mut tsql_server::Connection, e: &str, prop: &str| scalar(c, &format!("SELECT CAST(SQL_VARIANT_PROPERTY({}, '{}') AS VARCHAR(30)) FROM t", e, prop));
    assert_eq!(col(&mut c, "m", "Precision"), "12");
    assert_eq!(col(&mut c, "m", "Scale"), "3");
    assert_eq!(col(&mut c, "m", "BaseType"), "decimal");
    assert_eq!(col(&mut c, "n", "BaseType"), "nvarchar");
    assert_eq!(col(&mut c, "v", "BaseType"), "varchar");
}

#[test]
fn exec_arguments_may_be_system_variables() {
    let mut c = conn();
    script(&mut c, "CREATE PROCEDURE dbo.q @a INT AS SELECT @a;");
    assert_eq!(scalar(&mut c, "SELECT 1; EXEC dbo.q @@ROWCOUNT"), "1");
    exec(&mut c, "EXEC dbo.q @@TRANCOUNT");
}
