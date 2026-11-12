//! What the OLTP benchmarks (examples/tpc_oltp) turned up: the scope of table hints, reads that wait for a writer where
//! they are, and the counters of lock waits.
mod common;
use common::*;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn setup(s: &tsql_server::Server) -> tsql_server::Connection {
    let mut c = s.connect();
    script(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, v INT); INSERT t VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50);");
    exec(&mut c, "CREATE PROCEDURE p_hint @id INT AS BEGIN DECLARE @v INT; SELECT @v = v FROM t WITH (UPDLOCK) WHERE id = @id; END");
    exec(&mut c, "CREATE PROCEDURE p_plain AS BEGIN SELECT SUM(v) AS s FROM t; END");
    c
}

#[test]
fn a_table_hint_does_not_outlive_the_statement_it_was_written_in() {
    let s = server();
    let mut a = setup(&s);
    // a procedure reads the table WITH (UPDLOCK); in a transaction of its own session, a later plain read in another
    // procedure must not lock the rows it reads (it used to inherit the hint and take exclusive locks on every row)
    exec(&mut a, "EXEC p_hint 1");
    exec(&mut a, "BEGIN TRAN");
    exec(&mut a, "EXEC p_hint 2");
    exec(&mut a, "EXEC p_plain");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 0");
    // row 2 is locked by the hint of p_hint, nothing else is
    assert_eq!(err_no(&mut b, "UPDATE t SET v = v + 1 WHERE id = 2"), 1222);
    exec(&mut b, "UPDATE t SET v = v + 1 WHERE id = 5");
    exec(&mut b, "UPDATE t SET v = v + 1 WHERE id = 3");
    exec(&mut a, "COMMIT");
    assert_eq!(scalar(&mut a, "SELECT SUM(v) FROM t"), "152");
}

#[test]
fn a_hint_in_the_calling_statement_comes_back_after_a_procedure_returns() {
    let s = server();
    let mut a = setup(&s);
    exec(&mut a, "CREATE PROCEDURE p_outer AS BEGIN SELECT v FROM t WITH (UPDLOCK) WHERE id = 1; EXEC p_plain; SELECT v FROM t WHERE id = 4; END");
    exec(&mut a, "BEGIN TRAN");
    exec(&mut a, "EXEC p_outer");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 0");
    assert_eq!(err_no(&mut b, "UPDATE t SET v = 0 WHERE id = 1"), 1222);
    exec(&mut b, "UPDATE t SET v = 0 WHERE id = 4");
    exec(&mut a, "COMMIT");
}

#[test]
fn a_select_under_read_committed_waits_for_a_writer_and_carries_on() {
    let s = server();
    let mut a = setup(&s);
    exec(&mut a, "BEGIN TRAN");
    exec(&mut a, "UPDATE t SET v = 1000 WHERE id = 3");
    let (tx, rx) = mpsc::channel();
    let mut b = s.connect();
    let h = thread::spawn(move || {
        let r = scalar(&mut b, "SELECT SUM(v) FROM t");
        tx.send(r).unwrap();
    });
    // the reader is held up by the uncommitted row
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    exec(&mut a, "COMMIT");
    // and gives the committed value when the writer is done
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "1120");
    h.join().unwrap();
    let (waits, _) = s.lock_waits();
    assert!(waits >= 1);
    assert!(s.lock_wait_sites().iter().any(|(site, n, _)| site.contains("txn.rs") && *n >= 1));
}

#[test]
fn a_read_that_waits_still_times_out_and_is_still_a_deadlock_victim() {
    let s = server();
    let mut a = setup(&s);
    exec(&mut a, "BEGIN TRAN");
    exec(&mut a, "UPDATE t SET v = 1 WHERE id = 1");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 200");
    assert_eq!(err_no(&mut b, "SELECT SUM(v) FROM t"), 1222);
    // a deadlock between a writer and a reader is found, and one of them gives way
    let mut c2 = s.connect();
    exec(&mut c2, "BEGIN TRAN");
    exec(&mut c2, "UPDATE t SET v = 2 WHERE id = 2");
    let h = thread::spawn(move || {
        // c2 waits for row 1 (held by a) ...
        let r = c2.execute("UPDATE t SET v = 3 WHERE id = 1");
        (c2, r.first_error().map(|e| e.number))
    });
    thread::sleep(Duration::from_millis(200));
    // ... and a reads the table, which needs row 2 (held by c2): a reader waits where it is, and the cycle is seen
    let r = a.execute("SELECT SUM(v) FROM t");
    let a_err = r.first_error().map(|e| e.number);
    let (mut c2, c2_err) = h.join().unwrap();
    assert!(a_err == Some(1205) || c2_err == Some(1205), "a: {:?}, c2: {:?}", a_err, c2_err);
    let _ = c2.execute("ROLLBACK");
    let _ = a.execute("ROLLBACK");
}
