//! System-versioned temporal tables: history capture, FOR SYSTEM_TIME reads, guards.
mod common;
use common::*;

const CREATE: &str = "CREATE TABLE emp (
    id INT PRIMARY KEY,
    name VARCHAR(20),
    sal INT,
    vf DATETIME2 GENERATED ALWAYS AS ROW START NOT NULL,
    vt DATETIME2 GENERATED ALWAYS AS ROW END NOT NULL,
    PERIOD FOR SYSTEM_TIME (vf, vt)
) WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = dbo.emp_hist))";

#[test]
fn updates_and_deletes_are_copied_to_the_history_table() {
    let mut c = conn();
    exec(&mut c, CREATE);
    exec(&mut c, "INSERT emp (id, name, sal) VALUES (1, 'ann', 100), (2, 'bob', 200)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp_hist"), "0");
    std::thread::sleep(std::time::Duration::from_millis(20));
    exec(&mut c, "UPDATE emp SET sal = 150 WHERE id = 1");
    exec(&mut c, "DELETE emp WHERE id = 2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp_hist"), "2");
    assert_eq!(scalar(&mut c, "SELECT sal FROM emp_hist WHERE id = 1"), "100");
    assert_eq!(scalar(&mut c, "SELECT sal FROM emp_hist WHERE id = 2"), "200");
    // the closed period of a history row ends where the new row's period starts
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp_hist h JOIN emp e ON e.id = h.id WHERE h.vt = e.vf"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN vt = '9999-12-31 23:59:59.9999999' THEN 'open' ELSE 'closed' END FROM emp WHERE id = 1"), "open");
}

#[test]
fn for_system_time_reads_the_past() {
    let mut c = conn();
    exec(&mut c, CREATE);
    exec(&mut c, "INSERT emp (id, name, sal) VALUES (1, 'ann', 100)");
    std::thread::sleep(std::time::Duration::from_millis(30));
    let t1 = scalar(&mut c, "SELECT CONVERT(VARCHAR(40), SYSUTCDATETIME(), 126)");
    std::thread::sleep(std::time::Duration::from_millis(30));
    exec(&mut c, "UPDATE emp SET sal = 150 WHERE id = 1");
    std::thread::sleep(std::time::Duration::from_millis(30));
    let t2 = scalar(&mut c, "SELECT CONVERT(VARCHAR(40), SYSUTCDATETIME(), 126)");
    std::thread::sleep(std::time::Duration::from_millis(30));
    exec(&mut c, "UPDATE emp SET sal = 175 WHERE id = 1");
    assert_eq!(scalar(&mut c, &format!("SELECT sal FROM emp FOR SYSTEM_TIME AS OF '{}'", t1)), "100");
    assert_eq!(scalar(&mut c, &format!("SELECT sal FROM emp FOR SYSTEM_TIME AS OF '{}' AS e", t2)), "150");
    assert_eq!(scalar(&mut c, "SELECT sal FROM emp"), "175");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp FOR SYSTEM_TIME ALL"), "3");
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM emp FOR SYSTEM_TIME BETWEEN '{}' AND '{}'", t1, t2)), "2");
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM emp FOR SYSTEM_TIME FROM '{}' TO '{}'", t1, t2)), "2");
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM emp FOR SYSTEM_TIME CONTAINED IN ('{}', '{}')", t1, "9999-12-31 23:59:59.9999999")), "2");
    // columns are usable through the alias
    assert_eq!(scalar(&mut c, "SELECT MIN(e.sal) FROM emp FOR SYSTEM_TIME ALL AS e"), "100");
}

#[test]
fn hidden_period_columns_stay_out_of_select_star() {
    let mut c = conn();
    exec(
        &mut c,
        "CREATE TABLE h (id INT PRIMARY KEY, v INT,
           s DATETIME2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL,
           e DATETIME2 GENERATED ALWAYS AS ROW END HIDDEN NOT NULL,
           PERIOD FOR SYSTEM_TIME (s, e)) WITH (SYSTEM_VERSIONING = ON)",
    );
    exec(&mut c, "INSERT h VALUES (1, 10)");
    assert_eq!(rows(&mut c, "SELECT * FROM h")[0].len(), 2);
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN s IS NULL THEN 0 ELSE 1 END FROM h"), "1");
}

#[test]
fn the_history_table_and_the_link_are_protected() {
    let mut c = conn();
    exec(&mut c, CREATE);
    exec(&mut c, "INSERT emp (id, name, sal) VALUES (1, 'ann', 100)");
    exec(&mut c, "UPDATE emp SET sal = 1 WHERE id = 1");
    assert_eq!(err_no(&mut c, "DELETE emp_hist"), 13560);
    assert_eq!(err_no(&mut c, "UPDATE emp_hist SET sal = 5"), 13561);
    assert_eq!(err_no(&mut c, "INSERT emp_hist (id, name, sal, vf, vt) VALUES (9, 'x', 1, '2000-01-01', '2001-01-01')"), 13559);
    assert_eq!(err_no(&mut c, "DROP TABLE emp"), 13552);
    assert_eq!(err_no(&mut c, "DROP TABLE emp_hist"), 13552);
    assert_eq!(err_no(&mut c, "TRUNCATE TABLE emp"), 13552);
    assert_eq!(err_no(&mut c, "ALTER TABLE emp ADD extra INT"), 13552);
    assert_eq!(err_no(&mut c, "SELECT * FROM id_less FOR SYSTEM_TIME ALL"), 208);
    exec(&mut c, "CREATE TABLE plain (a INT)");
    assert_eq!(err_no(&mut c, "SELECT * FROM plain FOR SYSTEM_TIME ALL"), 13544);
}

#[test]
fn versioning_can_be_turned_off_and_on_again() {
    let mut c = conn();
    exec(&mut c, CREATE);
    exec(&mut c, "INSERT emp (id, name, sal) VALUES (1, 'ann', 100)");
    exec(&mut c, "UPDATE emp SET sal = 2 WHERE id = 1");
    exec(&mut c, "ALTER TABLE emp SET (SYSTEM_VERSIONING = OFF)");
    // now an ordinary table: no history is kept and the history table is ordinary too
    exec(&mut c, "UPDATE emp SET sal = 3 WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp_hist"), "1");
    exec(&mut c, "DELETE emp_hist");
    exec(&mut c, "ALTER TABLE emp SET (SYSTEM_VERSIONING = ON (HISTORY_TABLE = dbo.emp_hist))");
    exec(&mut c, "UPDATE emp SET sal = 4 WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT sal FROM emp_hist"), "3");
    exec(&mut c, "ALTER TABLE emp SET (SYSTEM_VERSIONING = OFF)");
    exec(&mut c, "DROP TABLE emp");
    exec(&mut c, "DROP TABLE emp_hist");
}

#[test]
fn a_rolled_back_change_leaves_no_history() {
    let mut c = conn();
    exec(&mut c, CREATE);
    exec(&mut c, "INSERT emp (id, name, sal) VALUES (1, 'ann', 100)");
    std::thread::sleep(std::time::Duration::from_millis(20));
    exec(&mut c, "BEGIN TRAN");
    exec(&mut c, "UPDATE emp SET sal = 999 WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp_hist"), "1");
    exec(&mut c, "ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp_hist"), "0");
    assert_eq!(scalar(&mut c, "SELECT sal FROM emp"), "100");
}

#[test]
fn an_unnamed_history_table_is_created() {
    let mut c = conn();
    exec(
        &mut c,
        "CREATE TABLE t (id INT, s DATETIME2 GENERATED ALWAYS AS ROW START NOT NULL, e DATETIME2 GENERATED ALWAYS AS ROW END NOT NULL,
          PERIOD FOR SYSTEM_TIME (s, e)) WITH (SYSTEM_VERSIONING = ON)",
    );
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE name LIKE 'MSSQL_TemporalHistoryFor_%'"), "1");
}
