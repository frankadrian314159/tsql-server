//! Two-phase commit support in the engine: a transaction can be prepared (its changes made durable while it stays open),
//! and a prepared transaction survives a crash as an in-doubt transaction that is settled later.
mod common;
use common::*;
use tsql_server::Server;

fn open(dir: &std::path::Path) -> Server {
    Server::open(dir).expect("open data directory")
}

fn setup(dir: &std::path::Path) -> Server {
    let s = open(dir);
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    exec(&mut c, "INSERT t VALUES (1, 10)");
    s
}

fn count(s: &Server) -> String {
    let mut c = s.connect();
    scalar(&mut c, "SELECT COUNT(*) FROM t")
}

#[test]
fn a_prepared_transaction_commits_and_stays_committed() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = setup(dir.path());
        let mut c = s.connect();
        exec(&mut c, "BEGIN TRAN");
        exec(&mut c, "INSERT t VALUES (2, 20)");
        exec(&mut c, "UPDATE t SET v = 11 WHERE id = 1");
        assert!(c.prepare_transaction(7).unwrap());
        assert_eq!(s.prepared_state(7), Some(true));
        assert!(s.in_doubt().is_empty(), "a transaction that is still open is not in doubt");
        // the session's own COMMIT settles it
        exec(&mut c, "COMMIT");
        assert_eq!(s.prepared_state(7), None);
        assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["1", "11"], vec!["2", "20"]]);
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    assert!(s.in_doubt().is_empty());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["1", "11"], vec!["2", "20"]]);
}

#[test]
fn a_prepared_transaction_that_rolls_back_stays_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = setup(dir.path());
        let mut c = s.connect();
        exec(&mut c, "BEGIN TRAN");
        exec(&mut c, "INSERT t VALUES (2, 20)");
        assert!(c.prepare_transaction(8).unwrap());
        exec(&mut c, "ROLLBACK");
        assert_eq!(s.prepared_state(8), None);
        assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "1");
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    assert!(s.in_doubt().is_empty(), "the rollback was logged, so recovery does not bring the transaction back");
    assert_eq!(count(&s), "1");
}

#[test]
fn a_crash_after_prepare_leaves_the_transaction_in_doubt_and_commit_finishes_it() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = setup(dir.path());
        let mut c = s.connect();
        exec(&mut c, "BEGIN TRAN");
        exec(&mut c, "INSERT t VALUES (2, 20)");
        exec(&mut c, "UPDATE t SET v = 11 WHERE id = 1");
        assert!(c.prepare_transaction(42).unwrap());
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    assert_eq!(s.in_doubt(), vec![42]);
    assert_eq!(s.prepared_state(42), Some(false));
    // nothing of it is visible until it is settled
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["1", "10"]]);
    assert!(s.resolve_prepared(42, true).unwrap());
    assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["1", "11"], vec!["2", "20"]]);
    assert!(s.in_doubt().is_empty());
    // settling again is harmless
    assert!(!s.resolve_prepared(42, true).unwrap());
    // and the settlement is itself durable
    drop(c);
    s.simulate_crash();
    let s = open(dir.path());
    assert!(s.in_doubt().is_empty());
    let mut c = s.connect();
    assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["1", "11"], vec!["2", "20"]]);
    exec(&mut c, "INSERT t VALUES (3, 30)"); // the table is writable again
}

#[test]
fn a_crash_after_prepare_and_an_abort_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = setup(dir.path());
        let mut c = s.connect();
        exec(&mut c, "BEGIN TRAN");
        exec(&mut c, "INSERT t VALUES (2, 20)");
        assert!(c.prepare_transaction(43).unwrap());
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path());
    assert_eq!(s.in_doubt(), vec![43]);
    assert!(s.resolve_prepared(43, false).unwrap());
    assert_eq!(count(&s), "1");
    s.simulate_crash();
    let s = open(dir.path());
    assert!(s.in_doubt().is_empty());
    assert_eq!(count(&s), "1");
}

#[test]
fn a_session_that_ends_after_prepare_leaves_the_transaction_in_doubt() {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(dir.path());
    let mut c = s.connect();
    exec(&mut c, "BEGIN TRAN");
    exec(&mut c, "INSERT t VALUES (2, 20)");
    assert!(c.prepare_transaction(5).unwrap());
    // a transaction that is open on a session is settled by the session
    assert_eq!(s.resolve_prepared(5, true).unwrap_err().number, 50211);
    drop(c);
    assert_eq!(s.in_doubt(), vec![5]);
    assert_eq!(count(&s), "1", "its locks are gone and its changes are not visible");
    assert!(s.resolve_prepared(5, true).unwrap());
    assert_eq!(count(&s), "2");
}

#[test]
fn a_clean_restart_keeps_in_doubt_transactions_and_the_log() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = setup(dir.path());
        let mut c = s.connect();
        exec(&mut c, "BEGIN TRAN");
        exec(&mut c, "INSERT t VALUES (2, 20)");
        assert!(c.prepare_transaction(9).unwrap());
        drop(c);
        s.checkpoint(); // a checkpoint must not fold the log away while a transaction is prepared
    }
    let s = open(dir.path());
    assert_eq!(s.in_doubt(), vec![9]);
    assert!(s.resolve_prepared(9, true).unwrap());
    assert_eq!(count(&s), "2");
    drop(s);
    let s = open(dir.path());
    assert!(s.in_doubt().is_empty());
    assert_eq!(count(&s), "2");
}

#[test]
fn a_transaction_that_changed_nothing_has_nothing_to_prepare() {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(dir.path());
    let mut c = s.connect();
    exec(&mut c, "BEGIN TRAN");
    exec(&mut c, "SELECT * FROM t");
    assert!(!c.prepare_transaction(1).unwrap());
    exec(&mut c, "COMMIT");
    // no transaction at all
    assert_eq!(c.prepare_transaction(2).unwrap_err().number, 3902);
}

#[test]
fn a_prepared_transaction_keeps_its_locks_until_it_is_settled() {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(dir.path());
    let mut c = s.connect();
    exec(&mut c, "BEGIN TRAN");
    exec(&mut c, "UPDATE t SET v = 11 WHERE id = 1");
    assert!(c.prepare_transaction(3).unwrap());
    let mut other = s.connect();
    exec(&mut other, "SET LOCK_TIMEOUT 200");
    assert_eq!(err_no(&mut other, "UPDATE t SET v = 99 WHERE id = 1"), 1222);
    exec(&mut c, "COMMIT");
    exec(&mut other, "UPDATE t SET v = 99 WHERE id = 1");
    assert_eq!(scalar(&mut other, "SELECT v FROM t WHERE id = 1"), "99");
}

#[test]
fn an_in_memory_server_can_prepare_too() {
    let s = Server::new();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY)");
    exec(&mut c, "BEGIN TRAN");
    exec(&mut c, "INSERT t VALUES (1)");
    assert!(c.prepare_transaction(1).unwrap());
    exec(&mut c, "COMMIT");
    assert!(s.in_doubt().is_empty());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "1");
}
