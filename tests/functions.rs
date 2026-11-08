mod common;
use common::*;

fn check(c: &mut tsql_server::Connection, cases: &[(&str, &str)]) {
    for (sql, expected) in cases {
        let got = scalar(c, &format!("SELECT {}", sql));
        assert_eq!(&got, expected, "SELECT {}", sql);
    }
}

#[test]
fn string_functions() {
    let mut c = conn();
    check(&mut c, &[
        ("LEN('abc')", "3"),
        ("LEN('abc   ')", "3"),
        ("LEN(NULL)", "NULL"),
        ("DATALENGTH('abc')", "3"),
        ("LEFT('abcdef', 2)", "ab"),
        ("RIGHT('abcdef', 2)", "ef"),
        ("LEFT('ab', 10)", "ab"),
        ("SUBSTRING('abcdef', 2, 3)", "bcd"),
        ("SUBSTRING('abcdef', 0, 3)", "ab"),
        ("SUBSTRING('abcdef', -1, 4)", "ab"),
        ("SUBSTRING('abcdef', 5, 100)", "ef"),
        ("CHARINDEX('c', 'abcabc')", "3"),
        ("CHARINDEX('c', 'abcabc', 4)", "6"),
        ("CHARINDEX('z', 'abc')", "0"),
        ("CHARINDEX('B', 'abc')", "2"),
        ("PATINDEX('%c_e%', 'abcdef')", "3"),
        ("PATINDEX('%z%', 'abc')", "0"),
        ("REPLACE('aXbxc', 'x', '-')", "a-b-c"),
        ("REPLACE('abc', '', 'z')", "abc"),
        ("STUFF('abcdef', 2, 3, '123')", "a123ef"),
        ("STUFF('abc', 5, 1, 'x')", "NULL"),
        ("REPLICATE('ab', 3)", "ababab"),
        ("REVERSE('abc')", "cba"),
        ("UPPER('aBc')", "ABC"),
        ("LOWER('aBc')", "abc"),
        ("LTRIM('  a ')", "a "),
        ("RTRIM('  a ') + '|'", "  a|"),
        ("TRIM('  a  ') + '|'", "a|"),
        ("TRIM('xx' FROM 'xxaxx')", "a"),
        ("'[' + SPACE(3) + ']'", "[   ]"),
        ("STR(123.456, 8, 2)", "  123.46"),
        ("STR(5)", "         5"),
        ("STR(1234.5, 3)", "***"),
        ("CONCAT('a', NULL, 'b', 1)", "ab1"),
        ("CONCAT_WS('-', 'a', NULL, 'b')", "a-b"),
        ("ASCII('A')", "65"),
        ("CHAR(66)", "B"),
        ("UNICODE(N'A')", "65"),
        ("NCHAR(67)", "C"),
        ("QUOTENAME('a]b')", "[a]]b]"),
        ("QUOTENAME('it''s', '''')", "'it''s'"),
        ("SOUNDEX('Robert')", "R163"),
        ("SOUNDEX('Rupert')", "R163"),
        ("DIFFERENCE('Robert', 'Rupert')", "4"),
        ("TRANSLATE('2*[3+4]', '[]', '()')", "2*(3+4)"),
        ("STRING_ESCAPE('a\"b', 'json')", "a\\\"b"),
        ("PARSENAME('a.b.c', 1)", "c"),
        ("PARSENAME('a.b.c', 3)", "a"),
        ("REVERSE(NULL)", "NULL"),
    ]);
    assert_eq!(err_no(&mut c, "SELECT LEFT('abc', -1)"), 536);
}

#[test]
fn math_functions() {
    let mut c = conn();
    check(&mut c, &[
        ("ABS(-5)", "5"),
        ("ABS(-5.5)", "5.5"),
        ("CEILING(1.2)", "2"),
        ("CEILING(-1.2)", "-1"),
        ("FLOOR(1.8)", "1"),
        ("FLOOR(-1.2)", "-2"),
        ("ROUND(1.255, 2)", "1.260"),
        ("ROUND(1.254, 2)", "1.250"),
        ("ROUND(1.255, 2, 1)", "1.250"),
        ("ROUND(150, -2)", "200"),
        ("ROUND(CAST(2.5 AS float), 0)", "3"),
        ("SIGN(-3)", "-1"),
        ("SIGN(0)", "0"),
        ("SIGN(4.5)", "1.0"),
        ("SQRT(16)", "4"),
        ("SQUARE(5)", "25"),
        ("POWER(2, 10)", "1024"),
        ("POWER(2.0, 3)", "8.0"),
        ("EXP(0)", "1"),
        ("LOG(1)", "0"),
        ("LOG10(1000)", "3"),
        ("LOG(8, 2)", "3"),
        ("PI()", "3.141592653589793"),
        ("DEGREES(PI())", "180"),
        ("RADIANS(180)", "3.141592653589793"),
        ("SIN(0)", "0"),
        ("COS(0)", "1"),
        ("ATN2(1, 1)", "0.7853981633974483"),
        ("GREATEST(1, 5, 3)", "5"),
        ("LEAST(4, 2, 9)", "2"),
        ("10 % 3", "1"),
    ]);
    assert_eq!(err_no(&mut c, "SELECT SQRT(-1)"), 3623);
    assert_eq!(err_no(&mut c, "SELECT LOG(0)"), 3623);
    let r = scalar(&mut c, "SELECT RAND()");
    assert!(r.parse::<f64>().unwrap() >= 0.0);
    assert_eq!(scalar(&mut c, "SELECT RAND(5)"), scalar(&mut c, "SELECT RAND(5)"));
}

#[test]
fn null_handling_functions() {
    let mut c = conn();
    check(&mut c, &[
        ("ISNULL(NULL, 'x')", "x"),
        ("ISNULL('a', 'x')", "a"),
        ("COALESCE(NULL, NULL, 3)", "3"),
        ("NULLIF(5, 5)", "NULL"),
        ("NULLIF(5, 6)", "5"),
        ("IIF(1 = 1, 'y', 'n')", "y"),
        ("IIF(NULL = 1, 'y', 'n')", "n"),
        ("CHOOSE(3, 'a', 'b', 'c')", "c"),
        ("CHOOSE(9, 'a')", "NULL"),
        ("CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END", "two"),
        ("CASE WHEN 1 = 2 THEN 'a' END", "NULL"),
    ]);
    // lazy evaluation
    assert_eq!(scalar(&mut c, "SELECT IIF(1 = 1, 'safe', CAST(1/0 AS VARCHAR(5)))"), "safe");
    assert_eq!(scalar(&mut c, "SELECT COALESCE(1, 1/0)"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 1 = 1 THEN 'ok' ELSE CAST(1/0 AS VARCHAR(5)) END"), "ok");
}

#[test]
fn date_functions() {
    let mut c = conn();
    check(&mut c, &[
        ("YEAR('2020-03-15')", "2020"),
        ("MONTH('2020-03-15')", "3"),
        ("DAY('2020-03-15')", "15"),
        ("DATEPART(year, '2020-03-15')", "2020"),
        ("DATEPART(quarter, '2020-03-15')", "1"),
        ("DATEPART(dayofyear, '2020-03-15')", "75"),
        ("DATEPART(weekday, '2020-03-15')", "1"),
        ("DATEPART(hour, '2020-03-15 13:45:10')", "13"),
        ("DATEPART(minute, '2020-03-15 13:45:10')", "45"),
        ("DATEPART(second, '2020-03-15 13:45:10')", "10"),
        ("DATEPART(millisecond, '2020-03-15 13:45:10.123')", "123"),
        ("DATEPART(iso_week, '2021-01-03')", "53"),
        ("DATEPART(week, '2020-01-01')", "1"),
        ("DATEPART(week, '2020-03-15')", "12"),
        ("DATENAME(month, '2020-03-15')", "March"),
        ("DATENAME(weekday, '2020-03-15')", "Sunday"),
        ("DATENAME(year, '2020-03-15')", "2020"),
        ("DATEADD(day, 10, '2020-03-15')", "2020-03-25 00:00:00.000"),
        ("DATEADD(month, 1, '2020-01-31')", "2020-02-29 00:00:00.000"),
        ("DATEADD(year, -1, CAST('2020-02-29' AS date))", "2019-02-28"),
        ("DATEADD(hour, 5, '2020-03-15 22:00:00')", "2020-03-16 03:00:00.000"),
        ("DATEADD(week, 1, CAST('2020-03-15' AS date))", "2020-03-22"),
        ("DATEADD(quarter, 1, CAST('2020-03-15' AS date))", "2020-06-15"),
        ("DATEDIFF(day, '2020-03-15', '2020-03-20')", "5"),
        ("DATEDIFF(day, '2020-03-20', '2020-03-15')", "-5"),
        ("DATEDIFF(month, '2020-01-31', '2020-02-01')", "1"),
        ("DATEDIFF(year, '2019-12-31', '2020-01-01')", "1"),
        ("DATEDIFF(hour, '2020-03-15 10:59', '2020-03-15 11:01')", "1"),
        ("DATEDIFF(minute, '2020-03-15 10:00:59', '2020-03-15 10:01:01')", "1"),
        ("DATEDIFF(week, '2020-03-14', '2020-03-15')", "1"),
        ("DATEDIFF(week, '2020-03-15', '2020-03-21')", "0"),
        ("DATEDIFF_BIG(second, '2020-03-15', '2020-03-16')", "86400"),
        ("EOMONTH('2020-02-10')", "2020-02-29"),
        ("EOMONTH('2020-02-10', 1)", "2020-03-31"),
        ("EOMONTH('2021-12-01')", "2021-12-31"),
        ("DATEFROMPARTS(2020, 3, 15)", "2020-03-15"),
        ("DATETIME2FROMPARTS(2020, 3, 15, 1, 2, 3, 4, 1)", "2020-03-15 01:02:03.4000000"),
        ("DATETRUNC(month, CAST('2020-03-15 10:20:30' AS datetime2))", "2020-03-01 00:00:00.0000000"),
        ("ISDATE('2020-02-30')", "0"),
        ("ISDATE('2020-02-28')", "1"),
        ("ISDATE('hello')", "0"),
        ("CAST(GETDATE() AS date) = CAST(SYSDATETIME() AS date)", "1"),
    ]);
    assert_eq!(err_no(&mut c, "SELECT DATEADD(hour, 1, CAST('2020-01-01' AS date))"), 9810);
    assert_eq!(err_no(&mut c, "SELECT DATEPART(foo, GETDATE())"), 155);
    assert_eq!(err_no(&mut c, "SELECT DATEFROMPARTS(2020, 13, 1)"), 289);
    assert_eq!(err_no(&mut c, "SELECT DATEDIFF(second, '1900-01-01', '2020-01-01')"), 535);
}

#[test]
fn datefirst_changes_weekday() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT @@DATEFIRST"), "7");
    assert_eq!(scalar(&mut c, "SELECT DATEPART(weekday, '2020-03-16')"), "2");
    exec(&mut c, "SET DATEFIRST 1");
    assert_eq!(scalar(&mut c, "SELECT DATEPART(weekday, '2020-03-16')"), "1");
}

#[test]
fn format_function() {
    let mut c = conn();
    check(&mut c, &[
        ("FORMAT(1234567.891, 'N2')", "1,234,567.89"),
        ("FORMAT(1234567.891, 'N0')", "1,234,568"),
        ("FORMAT(0.256, 'P1')", "25.6 %"),
        ("FORMAT(1234.5, 'C')", "$1,234.50"),
        ("FORMAT(255, 'X')", "FF"),
        ("FORMAT(42, 'D5')", "00042"),
        ("FORMAT(3.14159, 'F2')", "3.14"),
        ("FORMAT(1234.5, '#,##0.00')", "1,234.50"),
        ("FORMAT(5, '000')", "005"),
        ("FORMAT(CAST('2020-03-15 13:45:10' AS datetime), 'yyyy-MM-dd HH:mm:ss')", "2020-03-15 13:45:10"),
        ("FORMAT(CAST('2020-03-15' AS date), 'dd/MM/yyyy')", "15/03/2020"),
        ("FORMAT(CAST('2020-03-15' AS date), 'MMMM d, yyyy')", "March 15, 2020"),
        ("FORMAT(CAST('2020-03-15' AS date), 'dddd')", "Sunday"),
        ("FORMAT(CAST('2020-03-15 13:45:10' AS datetime), 'hh:mm tt')", "01:45 PM"),
        ("FORMAT(CAST('2020-03-15' AS date), 'd')", "03/15/2020"),
        ("FORMAT(NULL, 'N2')", "NULL"),
    ]);
}

#[test]
fn conversion_styles() {
    let mut c = conn();
    check(&mut c, &[
        ("CONVERT(varchar(10), CAST('2020-03-05' AS datetime), 110)", "03-05-2020"),
        ("CONVERT(varchar(10), CAST('2020-03-05' AS datetime), 111)", "2020/03/05"),
        ("CONVERT(varchar(11), CAST('2020-03-05' AS datetime), 106)", "05 Mar 2020"),
        ("CONVERT(varchar(12), CAST('2020-03-05' AS datetime), 107)", "Mar 05, 2020"),
        ("CONVERT(varchar(8), CAST('2020-03-05 07:08:09' AS datetime), 108)", "07:08:09"),
        ("CONVERT(varchar(30), CAST('2020-03-05 07:08:09.123' AS datetime), 121)", "2020-03-05 07:08:09.123"),
        ("CONVERT(varchar(10), CAST('2020-03-05' AS datetime), 1)", "03/05/20"),
        ("CONVERT(datetime, '03/05/2020', 101)", "2020-03-05 00:00:00.000"),
        ("CONVERT(int, '42')", "42"),
        ("CONVERT(decimal(5,1), '3.14')", "3.1"),
        ("CONVERT(varchar(5), 3.14159)", "3.141"),
    ]);
    assert_eq!(err_no(&mut c, "SELECT CONVERT(varchar(10), GETDATE(), 999)"), 281);
}

#[test]
fn json_functions() {
    let mut c = conn();
    let doc = "'{\"name\":\"Ann\",\"age\":30,\"tags\":[\"a\",\"b\"],\"addr\":{\"city\":\"X\"},\"ok\":true,\"nil\":null}'";
    check(&mut c, &[
        (&format!("JSON_VALUE({}, '$.name')", doc), "Ann"),
        (&format!("JSON_VALUE({}, '$.age')", doc), "30"),
        (&format!("JSON_VALUE({}, '$.tags[1]')", doc), "b"),
        (&format!("JSON_VALUE({}, '$.addr.city')", doc), "X"),
        (&format!("JSON_VALUE({}, '$.ok')", doc), "true"),
        (&format!("JSON_VALUE({}, '$.nil')", doc), "NULL"),
        (&format!("JSON_VALUE({}, '$.missing')", doc), "NULL"),
        (&format!("JSON_VALUE({}, '$.addr')", doc), "NULL"),
        (&format!("JSON_QUERY({}, '$.addr')", doc), "{\"city\":\"X\"}"),
        (&format!("JSON_QUERY({}, '$.tags')", doc), "[\"a\",\"b\"]"),
        (&format!("JSON_QUERY({}, '$.name')", doc), "NULL"),
        ("ISJSON('{\"a\":1}')", "1"),
        ("ISJSON('[1,2]')", "1"),
        ("ISJSON('nope')", "0"),
        ("ISJSON(NULL)", "NULL"),
        ("JSON_MODIFY('{\"a\":1}', '$.a', 2)", "{\"a\":2}"),
        ("JSON_MODIFY('{\"a\":1}', '$.b', 'x')", "{\"a\":1,\"b\":\"x\"}"),
        ("JSON_MODIFY('{\"a\":1,\"b\":2}', '$.a', NULL)", "{\"b\":2}"),
        ("JSON_MODIFY('{\"a\":[1]}', 'append $.a', 2)", "{\"a\":[1,2]}"),
        ("JSON_PATH_EXISTS('{\"a\":1}', '$.a')", "1"),
        ("JSON_PATH_EXISTS('{\"a\":1}', '$.z')", "0"),
    ]);
    assert_eq!(err_no(&mut c, "SELECT JSON_VALUE('{\"a\":1}', 'bad')"), 13607);
    assert_eq!(err_no(&mut c, "SELECT JSON_VALUE('not json', '$.a')"), 13609);
    assert_eq!(err_no(&mut c, "SELECT JSON_VALUE('{\"a\":1}', 'strict $.b')"), 13608);
}

#[test]
fn hashing_and_ids() {
    let mut c = conn();
    check(&mut c, &[
        ("CONVERT(varchar(64), HASHBYTES('MD5', 'abc'), 2)", "900150983CD24FB0D6963F7D28E17F72"),
        ("CONVERT(varchar(64), HASHBYTES('SHA1', 'abc'), 2)", "A9993E364706816ABA3E25717850C26C9CD0D89D"),
        ("CONVERT(varchar(64), HASHBYTES('SHA2_256', 'abc'), 2)", "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"),
        ("LEN(CONVERT(varchar(200), HASHBYTES('SHA2_512', 'abc'), 2))", "128"),
        ("CHECKSUM('a') = CHECKSUM('A')", "1"),
    ]);
}

#[test]
fn system_functions() {
    let mut c = conn();
    check(&mut c, &[
        ("DB_NAME()", "master"),
        ("DB_ID('tempdb')", "2"),
        ("SCHEMA_NAME()", "dbo"),
        ("USER_NAME()", "dbo"),
        ("@@SERVERNAME", "TSQL-SERVER"),
        ("@@TRANCOUNT", "0"),
        ("@@NESTLEVEL", "0"),
        ("LEFT(@@VERSION, 20)", "Microsoft SQL Server"),
        ("SERVERPROPERTY('ProductMajorVersion')", "16"),
        ("ISNUMERIC('12.5')", "1"),
        ("ISNUMERIC('abc')", "0"),
        ("ISNUMERIC('1e5')", "1"),
        ("APP_NAME()", "tsql-server"),
    ]);
    assert_eq!(scalar(&mut c, "SELECT @@SPID") > "0".to_string(), true);
    exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, n VARCHAR(10))");
    assert_eq!(scalar(&mut c, "SELECT OBJECT_NAME(OBJECT_ID('t'))"), "t");
    assert_eq!(scalar(&mut c, "SELECT COL_LENGTH('t', 'n')"), "10");
    assert_eq!(scalar(&mut c, "SELECT OBJECTPROPERTY(OBJECT_ID('t'), 'IsTable')"), "1");
    assert_eq!(scalar(&mut c, "SELECT COLUMNPROPERTY(OBJECT_ID('t'), 'id', 'AllowsNull')"), "0");
    assert_eq!(scalar(&mut c, "SELECT OBJECT_ID('nope')"), "NULL");
}

#[test]
fn time_zone_functions() {
    let mut c = conn();
    check(&mut c, &[
        ("CAST(CAST('2020-07-01 12:00:00' AS datetime2) AT TIME ZONE 'UTC' AS varchar(40))", "2020-07-01 12:00:00.0000000 +00:00"),
        ("CAST(CAST('2020-07-01 12:00:00' AS datetime2) AT TIME ZONE 'Eastern Standard Time' AS varchar(40))", "2020-07-01 12:00:00.0000000 -04:00"),
        ("CAST(CAST('2020-01-01 12:00:00' AS datetime2) AT TIME ZONE 'Eastern Standard Time' AS varchar(40))", "2020-01-01 12:00:00.0000000 -05:00"),
        ("CAST((CAST('2020-07-01 12:00:00' AS datetime2) AT TIME ZONE 'UTC') AT TIME ZONE 'Tokyo Standard Time' AS varchar(40))", "2020-07-01 21:00:00.0000000 +09:00"),
        ("DATEPART(tzoffset, CAST('2020-01-01 00:00:00 +05:30' AS datetimeoffset))", "330"),
        ("CAST(SWITCHOFFSET(CAST('2020-01-01 12:00:00 +00:00' AS datetimeoffset), '+02:00') AS varchar(40))", "2020-01-01 14:00:00.0000000 +02:00"),
    ]);
    assert_eq!(err_no(&mut c, "SELECT GETDATE() AT TIME ZONE 'Nowhere'"), 9820);
}
