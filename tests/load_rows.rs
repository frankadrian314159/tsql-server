//! `Connection::load_rows`: appending rows to a table without the machinery of an INSERT (no transaction, locks, undo or checks),
//! for a copy of data that has been checked already. It must give the same table an INSERT would; where it cannot be used
//! (a durable server, a session in a transaction, an identity column) it is an INSERT.
use tsql_server::{Server, Value};

fn rows(c: &mut tsql_server::Connection, sql: &str) -> Vec<Vec<String>> {
    let r = c.execute(sql);
    assert!(r.first_error().is_none(), "{}: {:?}", sql, r.first_error());
    r.resultsets().first().map(|rs| rs.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect()).unwrap_or_default()
}

fn names(t: &str) -> Vec<String> {
    vec![t.to_string()]
}

#[test]
fn loaded_rows_are_the_rows_an_insert_would_make() {
    let server = Server::new();
    let mut a = server.connect();
    for ddl in [
        "CREATE TABLE ins (id INT PRIMARY KEY, name CHAR(8), amount DECIMAL(10,2), note VARCHAR(20) NULL, ts DATETIME NULL)",
        "CREATE TABLE ld (id INT PRIMARY KEY, name CHAR(8), amount DECIMAL(10,2), note VARCHAR(20) NULL, ts DATETIME NULL)",
        "CREATE INDEX ix_ins ON ins (name)",
        "CREATE INDEX ix_ld ON ld (name)",
    ] {
        assert!(a.execute(ddl).first_error().is_none(), "{}", ddl);
    }
    // the values come typed as another engine (a shard) made them: an integer for a decimal column, a short string for a CHAR(8)
    let vals: Vec<Vec<Value>> = (0..500).map(|i| vec![Value::Int(i), Value::Str(format!("n{}", i % 9)), Value::Int(i * 3), if i % 7 == 0 { Value::Null } else { Value::Str(format!("note {}", i)) }, Value::Null]).collect();
    let n = a.load_rows(&names("ld"), None, vals.clone()).unwrap();
    assert_eq!(n, 500);
    for chunk in vals.chunks(100) {
        let literal = chunk.iter().map(|r| format!("({}, '{}', {}, {}, NULL)", r[0], r[1], r[2], if let Value::Str(s) = &r[3] { format!("'{}'", s) } else { "NULL".into() })).collect::<Vec<_>>().join(",");
        assert!(a.execute(&format!("INSERT ins VALUES {}", literal)).first_error().is_none());
    }
    // the same rows, the same types (CHAR is padded, the decimal has its scale), the same answers through the indexes
    for q in [
        "SELECT id, name, amount, note FROM {} ORDER BY id",
        "SELECT COUNT(*), SUM(amount), MIN(name), MAX(note), SUM(CASE WHEN note IS NULL THEN 1 ELSE 0 END) FROM {}",
        "SELECT id FROM {} WHERE name = 'n3' ORDER BY id",
        "SELECT id, DATALENGTH(name), name + '|' FROM {} WHERE id < 5 ORDER BY id",
        "SELECT name, COUNT(*) FROM {} GROUP BY name ORDER BY name",
        "SELECT id FROM {} WHERE id BETWEEN 100 AND 110 ORDER BY id",
    ] {
        assert_eq!(rows(&mut a, &q.replace("{}", "ld")), rows(&mut a, &q.replace("{}", "ins")), "{}", q);
    }
    // later changes work on the loaded rows as on any
    assert!(a.execute("UPDATE ld SET amount = amount + 1 WHERE id < 10").first_error().is_none());
    assert!(a.execute("DELETE FROM ld WHERE id >= 490").first_error().is_none());
    assert!(a.execute("INSERT ld VALUES (1000, 'x', 1, NULL, NULL)").first_error().is_none());
    assert_eq!(rows(&mut a, "SELECT COUNT(*), SUM(amount) FROM ld"), vec![vec!["491".to_string(), rows(&mut a, "SELECT SUM(amount) + 10 + 1 - (SELECT SUM(amount) FROM ins WHERE id >= 490) FROM ins")[0][0].clone()]]);
    // another session sees them (they are committed rows)
    let mut b = server.connect();
    assert_eq!(rows(&mut b, "SELECT COUNT(*) FROM ld")[0][0], "491");
}

#[test]
fn a_column_list_and_wrong_input_are_handled() {
    let server = Server::new();
    let mut c = server.connect();
    assert!(c.execute("CREATE TABLE t (a INT, b INT NULL, c VARCHAR(5) DEFAULT 'dflt')").first_error().is_none());
    // only some columns: the others take their defaults, as in an INSERT (here none has one: NULL)
    c.load_rows(&names("t"), Some(&["a".to_string(), "c".to_string()]), vec![vec![Value::Int(1), Value::Str("x".into())], vec![Value::Int(2), Value::Null]]).unwrap();
    assert_eq!(rows(&mut c, "SELECT a, b, c FROM t ORDER BY a"), vec![vec!["1", "NULL", "x"], vec!["2", "NULL", "NULL"]]);
    // a row with the wrong number of values, an unknown column, an unknown table: errors, and nothing is added
    assert!(c.load_rows(&names("t"), None, vec![vec![Value::Int(3)]]).is_err());
    assert!(c.load_rows(&names("t"), Some(&["nope".to_string()]), vec![vec![Value::Int(3)]]).is_err());
    assert!(c.load_rows(&names("missing"), None, vec![vec![Value::Int(3)]]).is_err());
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM t")[0][0], "2");
    // a value that does not fit its column is the error an INSERT gives
    let e = c.load_rows(&names("t"), Some(&["a".to_string()]), vec![vec![Value::Str("not a number".into())]]).unwrap_err();
    assert!(e.number != 0, "{:?}", e);
    // no rows at all
    assert_eq!(c.load_rows(&names("t"), None, vec![]).unwrap(), 0);
    // a temporary table
    assert!(c.execute("CREATE TABLE #tmp (k INT, v VARCHAR(10))").first_error().is_none());
    c.load_rows(&names("#tmp"), None, vec![vec![Value::Int(1), Value::Str("one".into())], vec![Value::Int(2), Value::Str("two".into())]]).unwrap();
    assert_eq!(rows(&mut c, "SELECT k, v FROM #tmp ORDER BY k"), vec![vec!["1", "one"], vec!["2", "two"]]);
}

#[test]
fn where_the_fast_path_cannot_be_used_it_is_an_insert() {
    let server = Server::new();
    let mut c = server.connect();
    // an identity column: the numbers are the engine's
    assert!(c.execute("CREATE TABLE idt (id INT IDENTITY(10, 5) PRIMARY KEY, v INT)").first_error().is_none());
    c.load_rows(&names("idt"), Some(&["v".to_string()]), vec![vec![Value::Int(1)], vec![Value::Int(2)]]).unwrap();
    assert_eq!(rows(&mut c, "SELECT id, v FROM idt ORDER BY id"), vec![vec!["10", "1"], vec!["15", "2"]]);
    // a key violated: the INSERT's error, as the ordinary path checks keys
    assert!(c.execute("CREATE TABLE pk (id INT PRIMARY KEY)").first_error().is_none());
    c.load_rows(&names("pk"), None, vec![vec![Value::Int(1)]]).unwrap();
    // (the fast path does not check: its input is a copy of rows that are unique already; in a transaction it is checked)
    assert!(c.execute("BEGIN TRANSACTION").first_error().is_none());
    let e = c.load_rows(&names("pk"), None, vec![vec![Value::Int(1)]]).unwrap_err();
    assert_eq!(e.number, 2627, "{:?}", e);
    // and what a transaction loads, it can roll back
    c.load_rows(&names("pk"), None, vec![vec![Value::Int(2)], vec![Value::Int(3)]]).unwrap();
    assert!(c.execute("ROLLBACK").first_error().is_none());
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM pk")[0][0], "1");
}

#[test]
fn a_durable_server_logs_what_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    {
        let server = Server::open(dir.path()).unwrap();
        let mut c = server.connect();
        assert!(c.execute("CREATE TABLE d (id INT PRIMARY KEY, v VARCHAR(10))").first_error().is_none());
        c.load_rows(&names("d"), None, (0..300).map(|i| vec![Value::Int(i), Value::Str(format!("v{}", i))]).collect()).unwrap();
        assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM d")[0][0], "300");
        // (no checkpoint: the rows are in the log only)
    }
    let server = Server::open(dir.path()).unwrap();
    let mut c = server.connect();
    assert_eq!(rows(&mut c, "SELECT COUNT(*), MAX(v) FROM d"), vec![vec!["300", "v99"]]);
}
