//! `BULK INSERT` from delimited text files.
mod common;
use common::*;

fn file(name: &str, content: &[u8]) -> String {
    let dir = std::env::temp_dir().join(format!("tsql_bulk_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, content).unwrap();
    p.to_string_lossy().replace('\'', "''")
}

#[test]
fn tab_separated_default_format() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT, name VARCHAR(20), amt DECIMAL(8,2))");
    let f = file("tab.txt", b"1\talpha\t1.50\r\n2\tbeta\t2.25\r\n3\t\t\r\n");
    exec(&mut c, &format!("BULK INSERT t FROM '{}'", f));
    assert_eq!(rows(&mut c, "SELECT id, name, amt FROM t ORDER BY id"), vec![vec!["1", "alpha", "1.50"], vec!["2", "beta", "2.25"], vec!["3", "NULL", "NULL"]]);
}

#[test]
fn options_terminators_firstrow_lastrow() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT, b VARCHAR(10))");
    let f = file("pipe.txt", b"a|b;skip|me;1|x;2|y;3|z;4|w;");
    exec(&mut c, &format!("BULK INSERT t FROM '{}' WITH (FIELDTERMINATOR = '|', ROWTERMINATOR = ';', FIRSTROW = 3, LASTROW = 5, TABLOCK)", f));
    assert_eq!(rows(&mut c, "SELECT a, b FROM t ORDER BY a"), vec![vec!["1", "x"], vec!["2", "y"], vec!["3", "z"]]);
}

#[test]
fn csv_format_with_quotes_and_header() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT, note VARCHAR(40))");
    let f = file("q.csv", b"id,note\r\n1,\"hello, world\"\r\n2,\"say \"\"hi\"\"\"\r\n3,\"two\r\nlines\"\r\n");
    exec(&mut c, &format!("BULK INSERT t FROM '{}' WITH (FORMAT = 'CSV', FIRSTROW = 2)", f));
    assert_eq!(rows(&mut c, "SELECT id, note FROM t ORDER BY id"), vec![vec!["1", "hello, world"], vec!["2", "say \"hi\""], vec!["3", "two\r\nlines"]]);
}

#[test]
fn identity_defaults_and_keepidentity() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT IDENTITY(1,1), v VARCHAR(5), flag INT DEFAULT 7)");
    let f = file("two.csv", b"a,\nb,2\n");
    exec(&mut c, &format!("BULK INSERT t FROM '{}' WITH (FIELDTERMINATOR = ',')", f));
    assert_eq!(rows(&mut c, "SELECT id, v, flag FROM t ORDER BY id"), vec![vec!["1", "a", "7"], vec!["2", "b", "2"]]);
    let g = file("three.csv", b"10,c,3\n");
    exec(&mut c, &format!("BULK INSERT t FROM '{}' WITH (FIELDTERMINATOR = ',', KEEPIDENTITY)", g));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE id = 10"), "1");
    // KEEPNULLS: an empty field is NULL even where a default exists
    let h = file("nulls.csv", b"x,\n");
    exec(&mut c, &format!("BULK INSERT t FROM '{}' WITH (FIELDTERMINATOR = ',', KEEPNULLS)", h));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE v = 'x' AND flag IS NULL"), "1");
}

#[test]
fn errors_leave_nothing_behind() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    let bad_count = file("short.txt", b"1\t1\n2\n");
    assert_eq!(err_no(&mut c, &format!("BULK INSERT t FROM '{}'", bad_count)), 4832);
    let bad_value = file("conv.txt", b"1\t1\n2\tabc\n");
    assert!(err(&mut c, &format!("BULK INSERT t FROM '{}'", bad_value)).number != 0);
    let dup = file("dup.txt", b"1\t1\n1\t2\n");
    assert_eq!(err_no(&mut c, &format!("BULK INSERT t FROM '{}'", dup)), 2627);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "0");
    assert_eq!(err_no(&mut c, "BULK INSERT t FROM 'C:\\no\\such\\file.txt'"), 4861);
    assert_eq!(err_no(&mut c, &format!("BULK INSERT nope FROM '{}'", bad_count)), 208);
}

#[test]
fn utf16_files_and_permissions() {
    let srv = server();
    let mut c = srv.connect();
    exec(&mut c, "CREATE TABLE t (id INT, s NVARCHAR(20))");
    let mut data = vec![0xff, 0xfe];
    for u in "1\tcafé\n".encode_utf16() {
        data.extend_from_slice(&u.to_le_bytes());
    }
    let f = file("u16.txt", &data);
    exec(&mut c, &format!("BULK INSERT t FROM '{}'", f));
    assert_eq!(scalar(&mut c, "SELECT s FROM t"), "café");
    // ordinary users may not bulk load
    script(&mut c, "CREATE LOGIN bl WITH PASSWORD = 'x'
CREATE USER bl FOR LOGIN bl
GRANT INSERT ON t TO bl
");
    let mut bl = srv.connect_as("bl", "x").unwrap();
    assert_eq!(err_no(&mut bl, &format!("BULK INSERT t FROM '{}'", f)), 4834);
}
