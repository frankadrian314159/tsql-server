mod common;
use common::*;

#[test]
fn integer_arithmetic() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT 7 / 2"), "3");
    assert_eq!(scalar(&mut c, "SELECT -7 / 2"), "-3");
    assert_eq!(scalar(&mut c, "SELECT -7 % 3"), "-1");
    assert_eq!(scalar(&mut c, "SELECT 2147483647 + 0"), "2147483647");
    assert_eq!(err_no(&mut c, "SELECT 2147483647 + 1"), 8115);
    assert_eq!(scalar(&mut c, "SELECT CAST(2147483647 AS bigint) + 1"), "2147483648");
    assert_eq!(err_no(&mut c, "SELECT 1/0"), 8134);
    assert_eq!(err_no(&mut c, "SELECT 1%0"), 8134);
    assert_eq!(scalar(&mut c, "SELECT 5 & 3"), "1");
    assert_eq!(scalar(&mut c, "SELECT 5 | 3"), "7");
    assert_eq!(scalar(&mut c, "SELECT 5 ^ 3"), "6");
    assert_eq!(scalar(&mut c, "SELECT ~5"), "-6");
}

#[test]
fn decimal_arithmetic_follows_sql_server_scale_rules() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT 5/2.0"), "2.500000");
    assert_eq!(scalar(&mut c, "SELECT 1.0/3.0"), "0.333333");
    assert_eq!(scalar(&mut c, "SELECT 1.5 + 2.25"), "3.75");
    assert_eq!(scalar(&mut c, "SELECT 1.5 * 2.25"), "3.375");
    assert_eq!(scalar(&mut c, "SELECT CAST(10.555 AS decimal(5,2))"), "10.56");
    assert_eq!(scalar(&mut c, "SELECT CAST(-10.555 AS decimal(5,2))"), "-10.56");
    assert_eq!(err_no(&mut c, "SELECT CAST(1000 AS decimal(3,0))"), 8115);
    assert_eq!(scalar(&mut c, "SELECT 7.5 % 2"), "1.5");
    assert_eq!(scalar(&mut c, "SELECT 12345678901234567890123456789012345678 + 0"), "12345678901234567890123456789012345678");
}

#[test]
fn float_behaviour() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT 1.5e0 + 1"), "2.5");
    assert_eq!(err_no(&mut c, "SELECT 1.0e0 / 0"), 8134);
    assert_eq!(scalar(&mut c, "SELECT CAST(CAST(1.5 AS float) AS varchar(20))"), "1.5");
    assert_eq!(scalar(&mut c, "SELECT CAST(123456789.0e0 AS varchar(30))"), "1.23457e+008");
}

#[test]
fn integer_cast_ranges() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST(255 AS tinyint)"), "255");
    assert_eq!(err_no(&mut c, "SELECT CAST(256 AS tinyint)"), 220);
    assert_eq!(err_no(&mut c, "SELECT CAST(-1 AS tinyint)"), 220);
    assert_eq!(err_no(&mut c, "SELECT CAST(40000 AS smallint)"), 220);
    assert_eq!(scalar(&mut c, "SELECT CAST(3.99 AS int)"), "3");
    assert_eq!(scalar(&mut c, "SELECT CAST(-3.99 AS int)"), "-3");
    assert_eq!(scalar(&mut c, "SELECT CAST('42' AS int)"), "42");
    assert_eq!(scalar(&mut c, "SELECT CAST(' 42 ' AS int)"), "42");
    assert_eq!(err_no(&mut c, "SELECT CAST('abc' AS int)"), 245);
    assert_eq!(err_no(&mut c, "SELECT CAST('1.5' AS int)"), 245);
    assert_eq!(err_no(&mut c, "SELECT CAST(3000000000 AS int)"), 8115);
    let e = err(&mut c, "SELECT CAST('abc' AS int)");
    assert_eq!(e.message, "Conversion failed when converting the varchar value 'abc' to data type int.");
}

#[test]
fn try_cast_and_try_convert() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT TRY_CAST('abc' AS int)"), "NULL");
    assert_eq!(scalar(&mut c, "SELECT TRY_CAST('12' AS int)"), "12");
    assert_eq!(scalar(&mut c, "SELECT TRY_CONVERT(date, '2020-13-45')"), "NULL");
    assert_eq!(scalar(&mut c, "SELECT TRY_CONVERT(date, '2020-02-29')"), "2020-02-29");
    assert_eq!(scalar(&mut c, "SELECT TRY_CAST(300 AS tinyint)"), "NULL");
    assert_eq!(scalar(&mut c, "SELECT TRY_PARSE('3.5' AS decimal(5,2))"), "3.50");
}

#[test]
fn bit_conversions() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST(5 AS bit)"), "1");
    assert_eq!(scalar(&mut c, "SELECT CAST(0 AS bit)"), "0");
    assert_eq!(scalar(&mut c, "SELECT CAST('true' AS bit)"), "1");
    assert_eq!(scalar(&mut c, "SELECT CAST('FALSE' AS bit)"), "0");
    assert_eq!(err_no(&mut c, "SELECT CAST('maybe' AS bit)"), 245);
}

#[test]
fn string_types_and_padding() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST('abcdef' AS varchar(3))"), "abc");
    assert_eq!(scalar(&mut c, "SELECT '[' + CAST('ab' AS char(5)) + ']'"), "[ab   ]");
    assert_eq!(scalar(&mut c, "SELECT '[' + CAST('ab' AS varchar(5)) + ']'"), "[ab]");
    assert_eq!(scalar(&mut c, "SELECT 'a' + 'b'"), "ab");
    assert_eq!(scalar(&mut c, "SELECT 'a' + NULL"), "NULL");
    assert_eq!(scalar(&mut c, "SELECT '1' + 1"), "2");
    assert_eq!(err_no(&mut c, "SELECT 'a' + 1"), 245);
    assert_eq!(scalar(&mut c, "SELECT 'it''s'"), "it's");
    assert_eq!(scalar(&mut c, "SELECT N'héllo'"), "héllo");
    assert_eq!(scalar(&mut c, "SELECT CAST(12.50 AS varchar(10))"), "12.50");
    assert_eq!(scalar(&mut c, "SELECT CAST(NULL AS varchar(10))"), "NULL");
}

#[test]
fn string_truncation_error_on_insert() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (s VARCHAR(3))");
    let e = err(&mut c, "INSERT t VALUES ('abcd')");
    assert_eq!(e.number, 2628);
    assert!(e.message.contains("would be truncated"), "{}", e.message);
    exec(&mut c, "INSERT t VALUES ('abc')");
}

#[test]
fn binary_types() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT 0x1F"), "0x1F");
    assert_eq!(scalar(&mut c, "SELECT CAST(255 AS varbinary(4))"), "0x000000FF");
    assert_eq!(scalar(&mut c, "SELECT CAST('abc' AS varbinary(10))"), "0x616263");
    assert_eq!(scalar(&mut c, "SELECT CAST(0x616263 AS varchar(10))"), "abc");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varchar(10), 0x616263, 2)"), "616263");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varbinary(10), '0x616263', 1)"), "0x616263");
    assert_eq!(scalar(&mut c, "SELECT DATALENGTH(0x010203)"), "3");
    assert_eq!(scalar(&mut c, "SELECT CAST(0x00000010 AS int)"), "16");
}

#[test]
fn date_conversions_and_formats() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-02-29' AS date)"), "2020-02-29");
    assert_eq!(err_no(&mut c, "SELECT CAST('2019-02-29' AS date)"), 241);
    assert_eq!(scalar(&mut c, "SELECT CAST('20200229' AS date)"), "2020-02-29");
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-02-29 13:45:10.123' AS datetime)"), "2020-02-29 13:45:10.123");
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-02-29 13:45:10.123456' AS datetime2)"), "2020-02-29 13:45:10.1234560");
    assert_eq!(scalar(&mut c, "SELECT CAST('13:45:10' AS time)"), "13:45:10.0000000");
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-02-29' AS datetime)"), "2020-02-29 00:00:00.000");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varchar(20), CAST('2020-02-29 13:45:10' AS datetime), 120)"), "2020-02-29 13:45:10");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varchar(20), CAST('2020-02-29' AS datetime), 101)"), "02/29/2020");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varchar(20), CAST('2020-02-29' AS datetime), 103)"), "29/02/2020");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varchar(20), CAST('2020-02-29' AS datetime), 112)"), "20200229");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varchar(30), CAST('2020-02-29 13:45:10' AS datetime), 126)"), "2020-02-29T13:45:10.000");
    assert_eq!(scalar(&mut c, "SELECT CAST(CAST('2020-02-29 13:45:00' AS datetime) AS varchar(30))"), "Feb 29 2020  1:45PM");
    assert_eq!(scalar(&mut c, "SELECT CAST(CAST('2020-02-29' AS date) AS varchar(30))"), "2020-02-29");
}

#[test]
fn datetime_rounding() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-01-01 00:00:00.001' AS datetime)"), "2020-01-01 00:00:00.000");
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-01-01 00:00:00.002' AS datetime)"), "2020-01-01 00:00:00.003");
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-01-01 00:00:00.005' AS datetime)"), "2020-01-01 00:00:00.007");
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-01-01 10:20:40' AS smalldatetime)"), "2020-01-01 10:21:00");
    assert_eq!(err_no(&mut c, "SELECT CAST('1600-01-01' AS datetime)"), 242);
    assert_eq!(scalar(&mut c, "SELECT CAST('1600-01-01' AS datetime2)"), "1600-01-01 00:00:00.0000000");
}

#[test]
fn datetime_arithmetic() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-01-01' AS datetime) + 1.5"), "2020-01-02 12:00:00.000");
    assert_eq!(scalar(&mut c, "SELECT CAST('2020-01-10' AS datetime) - 10"), "2019-12-31 00:00:00.000");
    assert_eq!(err_no(&mut c, "SELECT CAST('2020-01-01' AS date) + 1"), 206);
}

#[test]
fn guid_type() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier)"), "6F9619FF-8B86-D011-B42D-00C04FC964FF");
    assert_eq!(err_no(&mut c, "SELECT CAST('nope' AS uniqueidentifier)"), 8169);
    assert_eq!(scalar(&mut c, "SELECT LEN(CAST(NEWID() AS varchar(36)))"), "36");
    assert_eq!(scalar(&mut c, "SELECT IIF(NEWID() = NEWID(), 1, 0)"), "0");
}

#[test]
fn money_types() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CAST(1234.5678 AS money)"), "1234.5678");
    assert_eq!(scalar(&mut c, "SELECT CAST(1234.56789 AS money)"), "1234.5679");
    assert_eq!(scalar(&mut c, "SELECT CAST('$1,234.50' AS money)"), "1234.5000");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(varchar(20), CAST(1234567.5 AS money), 1)"), "1,234,567.50");
}

#[test]
fn comparison_semantics() {
    let mut c = conn();
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'a' = 'A' THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'a ' = 'a' THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN '10' = 10 THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN NULL = NULL THEN 1 ELSE 0 END"), "0");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN NULL IS NULL THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 2 BETWEEN 1 AND 3 THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'b' > 'a' THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN CAST('2020-01-01' AS date) < '2020-06-01' THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 1 IN (2, NULL) THEN 1 WHEN 1 NOT IN (2, NULL) THEN 2 ELSE 3 END"), "3");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'abc' LIKE 'a%' THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'abc' LIKE '[a-c]_c' THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'abc' LIKE '%[^a-z]%' THEN 1 ELSE 0 END"), "0");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN '50%' LIKE '50!%' ESCAPE '!' THEN 1 ELSE 0 END"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 1 IS DISTINCT FROM NULL THEN 1 ELSE 0 END"), "1");
}

#[test]
fn user_defined_alias_types() {
    let mut c = conn();
    exec(&mut c, "CREATE TYPE Money2 FROM DECIMAL(10,2) NOT NULL");
    exec(&mut c, "CREATE TABLE t (m Money2)");
    exec(&mut c, "INSERT t VALUES (1.239)");
    assert_eq!(scalar(&mut c, "SELECT m FROM t"), "1.24");
    exec(&mut c, "DECLARE @x Money2 = 5; SELECT @x");
}
