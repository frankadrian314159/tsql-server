//! Disk persistence: write-ahead log, checkpoints and crash recovery.
mod common;
use common::*;
use std::fs;
use std::io::Write;
use tsql_server::Server;

fn open(dir: &std::path::Path) -> Server {
    Server::open(dir).expect("open data directory")
}

#[test]
fn data_survives_a_clean_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT IDENTITY PRIMARY KEY, name NVARCHAR(20), amt DECIMAL(10,2), d DATETIME2, g UNIQUEIDENTIFIER, b VARBINARY(10))");
        exec(&mut c, "INSERT t (name, amt, d, g, b) VALUES (N'héllo', 12.50, '2020-02-29 13:45:10.1234567', '6F9619FF-8B86-D011-B42D-00C04FC964FF', 0xCAFE), (NULL, NULL, NULL, NULL, NULL)");
    }
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id, name, amt, d, g, b FROM t ORDER BY id"), vec![
        vec!["1", "héllo", "12.50", "2020-02-29 13:45:10.1234567", "6F9619FF-8B86-D011-B42D-00C04FC964FF", "0xCAFE"],
        vec!["2", "NULL", "NULL", "NULL", "NULL", "NULL"]]);
    // identity continues where it left off, indexes were rebuilt
    exec(&mut c, "INSERT t (name) VALUES ('x')");
    assert_eq!(scalar(&mut c, "SELECT MAX(id) FROM t"), "3");
    assert_eq!(err_no(&mut c, "INSERT t (id, name) VALUES (1, 'dup')"), 544);
    exec(&mut c, "SET IDENTITY_INSERT t ON");
    assert_eq!(err_no(&mut c, "INSERT t (id, name) VALUES (1, 'dup')"), 2627);
}

#[test]
fn committed_work_survives_a_crash_and_uncommitted_work_does_not() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
        exec(&mut c, "INSERT t VALUES (1, 10), (2, 20)");
        exec(&mut c, "UPDATE t SET v = 11 WHERE id = 1");
        exec(&mut c, "DELETE t WHERE id = 2");
        exec(&mut c, "INSERT t VALUES (3, 30)");
        exec(&mut c, "BEGIN TRAN; INSERT t VALUES (4, 40); UPDATE t SET v = 99 WHERE id = 1"); // never committed
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["1", "11"], vec!["3", "30"]]);
}

#[test]
fn rolled_back_work_is_not_logged() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY)");
        exec(&mut c, "INSERT t VALUES (1)");
        exec(&mut c, "BEGIN TRAN; INSERT t VALUES (2); CREATE TABLE ghost (x INT); ROLLBACK");
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id FROM t"), vec![vec!["1"]]);
    assert_eq!(err_no(&mut c, "SELECT * FROM ghost"), 208);
}

#[test]
fn a_torn_log_tail_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY)");
        exec(&mut c, "INSERT t VALUES (1)");
        exec(&mut c, "INSERT t VALUES (2)");
        drop(c);
        s.simulate_crash();
    }
    let wal = dir.path().join("wal.log");
    let len = fs::metadata(&wal).unwrap().len();
    // chop the final frame in half, as a crash in the middle of a write would
    let f = fs::OpenOptions::new().write(true).open(&wal).unwrap();
    f.set_len(len - 4).unwrap();
    drop(f);
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id FROM t ORDER BY id"), vec![vec!["1"]], "the half-written commit must vanish, earlier commits stay");
    exec(&mut c, "INSERT t VALUES (2)"); // the log is usable again
    drop(c);
    s.simulate_crash();
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id FROM t ORDER BY id"), vec![vec!["1"], vec!["2"]]);

    // garbage appended after good frames is also ignored
    drop(c);
    s.simulate_crash();
    let mut f = fs::OpenOptions::new().append(true).open(&wal).unwrap();
    f.write_all(&[0xFF; 37]).unwrap();
    drop(f);
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "2");
}

#[test]
fn checkpoint_plus_log_recovers_everything() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(10))");
        exec(&mut c, "INSERT t VALUES (1,'a'),(2,'b')");
        exec(&mut c, "CHECKPOINT");
        assert_eq!(fs::metadata(dir.path().join("wal.log")).unwrap().len(), 0, "the log is truncated by a checkpoint");
        exec(&mut c, "INSERT t VALUES (3,'c')");
        exec(&mut c, "UPDATE t SET v = 'B' WHERE id = 2");
        exec(&mut c, "DELETE t WHERE id = 1");
        exec(&mut c, "CREATE TABLE u (x INT)");
        exec(&mut c, "INSERT u VALUES (7)");
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["2", "B"], vec!["3", "c"]]);
    assert_eq!(scalar(&mut c, "SELECT x FROM u"), "7");
}

#[test]
fn schema_objects_are_durable() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        script(&mut c, "CREATE TABLE p (id INT PRIMARY KEY)
CREATE TABLE ch (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE, CONSTRAINT ck CHECK (id > 0))
CREATE UNIQUE INDEX ux ON ch (pid, id)
CREATE VIEW v AS SELECT id FROM ch
GO
CREATE PROCEDURE addp @n INT AS INSERT p VALUES (@n)
GO
CREATE FUNCTION dbo.twice(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END
GO
CREATE TRIGGER trg ON p AFTER INSERT AS INSERT audit SELECT id FROM inserted
GO
CREATE TABLE audit (id INT)
CREATE SEQUENCE sq START WITH 100 INCREMENT BY 10
CREATE SCHEMA hr
CREATE TABLE hr.emp (x INT)
CREATE TYPE money2 FROM DECIMAL(10,2)
CREATE DATABASE other
");
        exec(&mut c, "INSERT p VALUES (1)");
        exec(&mut c, "INSERT ch VALUES (10, 1)");
        assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR sq"), "100");
        assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR sq"), "110");
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    let mut c = s.connect();
    exec(&mut c, "EXEC addp 2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM audit"), "2", "trigger survived the restart and fired again");
    assert_eq!(scalar(&mut c, "SELECT dbo.twice(21)"), "42");
    assert_eq!(scalar(&mut c, "SELECT id FROM v"), "10");
    assert_eq!(err_no(&mut c, "INSERT ch VALUES (11, 99)"), 547);
    assert_eq!(err_no(&mut c, "INSERT ch VALUES (-1, 1)"), 547);
    assert_eq!(err_no(&mut c, "INSERT ch VALUES (12, 1), (13, 1), (12, 1)"), 2627);
    exec(&mut c, "DELETE p WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ch"), "0", "cascade delete still works");
    assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR sq"), "120", "sequence kept its position");
    exec(&mut c, "INSERT hr.emp VALUES (1)");
    exec(&mut c, "DECLARE @m money2 = 1.5");
    exec(&mut c, "USE other");
}

#[test]
fn dropped_objects_stay_dropped() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE a (x INT)");
        exec(&mut c, "CREATE TABLE b (x INT)");
        exec(&mut c, "INSERT a VALUES (1)");
        exec(&mut c, "DROP TABLE a");
        exec(&mut c, "ALTER TABLE b ADD y INT NOT NULL DEFAULT 5");
        exec(&mut c, "CREATE DATABASE tmp");
        exec(&mut c, "DROP DATABASE tmp");
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(err_no(&mut c, "SELECT * FROM a"), 208);
    exec(&mut c, "INSERT b (x) VALUES (3)");
    assert_eq!(rows(&mut c, "SELECT x, y FROM b"), vec![vec!["3", "5"]]);
    assert_eq!(err_no(&mut c, "USE tmp"), 911);
}

#[test]
fn many_rows_and_many_commits() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, s VARCHAR(20))");
        exec(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 3000) INSERT big SELECT i, 'row' + CAST(i AS VARCHAR(10)) FROM n OPTION (MAXRECURSION 0)");
        for i in 0..50 {
            exec(&mut c, &format!("UPDATE big SET s = 'upd{}' WHERE id = {}", i, i + 1));
        }
        exec(&mut c, "DELETE big WHERE id > 2900");
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "2900");
    assert_eq!(scalar(&mut c, "SELECT s FROM big WHERE id = 50"), "upd49");
    assert_eq!(scalar(&mut c, "SELECT s FROM big WHERE id = 51"), "row51");
}

#[test]
fn a_data_directory_can_only_be_open_once() {
    let dir = tempfile::tempdir().unwrap();
    let _s = open(dir.path());
    let e = Server::open(dir.path()).err().expect("second open must fail");
    assert_eq!(e.number, 5120);
}

#[test]
fn a_corrupt_checkpoint_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (x INT)");
        exec(&mut c, "CHECKPOINT");
    }
    let snap = dir.path().join("snapshot.bin");
    let mut bytes = fs::read(&snap).unwrap();
    let n = bytes.len();
    bytes[n - 3] ^= 0xFF;
    fs::write(&snap, bytes).unwrap();
    assert_eq!(Server::open(dir.path()).err().expect("corrupt").number, 824);
}

#[test]
fn security_settings_are_durable() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path());
        let mut c = s.connect();
        exec(&mut c, "CREATE LOGIN ann WITH PASSWORD = 'pw1'");
        exec(&mut c, "CREATE TABLE secret (x INT)");
        exec(&mut c, "INSERT secret VALUES (42)");
        exec(&mut c, "CREATE USER ann FOR LOGIN ann");
        exec(&mut c, "GRANT SELECT ON secret TO ann");
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    assert!(s.connect_as("ann", "wrong").is_err());
    let mut a = s.connect_as("ann", "pw1").expect("password survived the restart");
    assert_eq!(scalar(&mut a, "SELECT x FROM secret"), "42");
    assert_eq!(err_no(&mut a, "DELETE secret"), 229);
}
