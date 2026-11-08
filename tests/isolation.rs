//! Transaction isolation levels, locking, blocking and deadlock handling, using real threads.
mod common;
use common::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tsql_server::{BatchResult, Connection, Server};

/// A batch running on another thread.
struct Async {
    done: Arc<AtomicBool>,
    rx: Receiver<(Connection, BatchResult)>,
}

impl Async {
    fn start(mut c: Connection, sql: &str) -> Async {
        let sql = sql.to_string();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let (tx, rx) = channel();
        thread::spawn(move || {
            let r = c.execute(&sql);
            d2.store(true, Ordering::SeqCst);
            let _ = tx.send((c, r));
        });
        Async { done, rx }
    }
    /// True when the batch has not finished after a short wait.
    fn blocked(&self) -> bool {
        thread::sleep(Duration::from_millis(250));
        !self.done.load(Ordering::SeqCst)
    }
    fn finish(self) -> (Connection, BatchResult) {
        self.rx.recv_timeout(Duration::from_secs(10)).expect("batch never finished")
    }
}

fn two() -> (Server, Connection, Connection) {
    let s = common::server();
    let mut a = s.connect();
    exec(&mut a, "CREATE TABLE acct (id INT PRIMARY KEY, bal INT)");
    exec(&mut a, "INSERT acct VALUES (1, 100), (2, 200), (3, 300)");
    let b = s.connect();
    (s, a, b)
}

#[test]
fn rollback_of_one_session_keeps_the_others_committed_work() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 0 WHERE id = 1");
    exec(&mut b, "UPDATE acct SET bal = 999 WHERE id = 3");
    exec(&mut a, "ROLLBACK");
    assert_eq!(rows(&mut a, "SELECT bal FROM acct ORDER BY id"), vec![vec!["100"], vec!["200"], vec!["999"]]);
}

#[test]
fn read_uncommitted_sees_dirty_data() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 1 WHERE id = 1");
    exec(&mut b, "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "1");
    exec(&mut a, "ROLLBACK");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "100");
}

#[test]
fn read_committed_blocks_on_uncommitted_write_then_sees_the_commit() {
    let (_s, mut a, b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 111 WHERE id = 1");
    let rd = Async::start(b, "SELECT bal FROM acct WHERE id = 1");
    assert!(rd.blocked(), "a READ COMMITTED reader must wait for the writer");
    exec(&mut a, "COMMIT");
    let (_b, r) = rd.finish();
    assert_eq!(r.resultsets()[0].rows[0][0].to_string(), "111");
}

#[test]
fn read_committed_blocked_reader_sees_original_after_rollback() {
    let (_s, mut a, b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 111 WHERE id = 1");
    let rd = Async::start(b, "SELECT bal FROM acct WHERE id = 1");
    assert!(rd.blocked());
    exec(&mut a, "ROLLBACK");
    let (_b, r) = rd.finish();
    assert_eq!(r.resultsets()[0].rows[0][0].to_string(), "100");
}

#[test]
fn read_committed_allows_non_repeatable_reads() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "100");
    exec(&mut b, "UPDATE acct SET bal = 150 WHERE id = 1");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "150");
    exec(&mut a, "COMMIT");
}

#[test]
fn repeatable_read_blocks_writers_until_the_reader_finishes() {
    let (_s, mut a, b) = two();
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "100");
    let wr = Async::start(b, "UPDATE acct SET bal = 150 WHERE id = 1");
    assert!(wr.blocked(), "the writer must wait for the REPEATABLE READ shared lock");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "100");
    exec(&mut a, "COMMIT");
    let (mut b, r) = wr.finish();
    assert!(r.errors().is_empty());
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "150");
}

#[test]
fn repeatable_read_still_allows_phantoms_but_serializable_does_not() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM acct WHERE bal > 0"), "3");
    exec(&mut b, "INSERT acct VALUES (4, 400)");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM acct WHERE bal > 0"), "4", "phantom row appears under REPEATABLE READ");
    exec(&mut a, "COMMIT");

    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM acct WHERE bal > 0"), "4");
    let ins = Async::start(b, "INSERT acct VALUES (5, 500)");
    assert!(ins.blocked(), "SERIALIZABLE must block the phantom insert");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM acct WHERE bal > 0"), "4");
    exec(&mut a, "COMMIT");
    let (mut b, r) = ins.finish();
    assert!(r.errors().is_empty());
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM acct"), "5");
}

#[test]
fn writers_serialize_and_no_update_is_lost() {
    let (_s, mut a, b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = bal + 10 WHERE id = 1");
    let w = Async::start(b, "UPDATE acct SET bal = bal + 5 WHERE id = 1");
    assert!(w.blocked());
    exec(&mut a, "COMMIT");
    let (mut b, r) = w.finish();
    assert!(r.errors().is_empty());
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "115");
}

#[test]
fn different_rows_do_not_block_each_other() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 1 WHERE id = 1");
    exec(&mut b, "UPDATE acct SET bal = 2 WHERE id = 2");
    exec(&mut a, "COMMIT");
    assert_eq!(rows(&mut a, "SELECT bal FROM acct WHERE id < 3 ORDER BY id"), vec![vec!["1"], vec!["2"]]);
}

#[test]
fn deadlock_is_detected_and_one_victim_is_chosen() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 1 WHERE id = 1");
    exec(&mut b, "BEGIN TRAN; UPDATE acct SET bal = 2 WHERE id = 2");
    // a waits for b's row
    let pending = Async::start(a, "UPDATE acct SET bal = 11 WHERE id = 2");
    assert!(pending.blocked());
    // b now asks for a's row: a cycle
    let r = b.execute("UPDATE acct SET bal = 22 WHERE id = 1");
    let e = r.first_error().expect("the second waiter must be chosen as the deadlock victim");
    assert_eq!(e.number, 1205);
    assert!(e.message.contains("deadlocked"), "{}", e.message);
    // the victim's transaction is gone, so the first session proceeds
    let (mut a, r) = pending.finish();
    assert!(r.errors().is_empty(), "{:?}", r.errors());
    exec(&mut a, "COMMIT");
    assert_eq!(scalar(&mut b, "SELECT @@TRANCOUNT"), "0");
    assert_eq!(rows(&mut a, "SELECT id, bal FROM acct WHERE id < 3 ORDER BY id"), vec![vec!["1", "1"], vec!["2", "11"]]);
}

#[test]
fn lock_timeout_gives_error_1222() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 1 WHERE id = 1");
    exec(&mut b, "SET LOCK_TIMEOUT 150");
    let started = std::time::Instant::now();
    assert_eq!(err_no(&mut b, "SELECT bal FROM acct WHERE id = 1"), 1222);
    assert!(started.elapsed() >= Duration::from_millis(100));
    exec(&mut b, "SET LOCK_TIMEOUT 0");
    assert_eq!(err_no(&mut b, "UPDATE acct SET bal = 5 WHERE id = 1"), 1222);
    exec(&mut a, "ROLLBACK");
}

#[test]
fn snapshot_isolation_reads_a_consistent_view_without_blocking() {
    let (_s, mut a, mut b) = two();
    assert_eq!(err_no(&mut a, "SET TRANSACTION ISOLATION LEVEL SNAPSHOT; BEGIN TRAN"), 3952);
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
    exec(&mut a, "ALTER DATABASE master SET ALLOW_SNAPSHOT_ISOLATION ON");
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL SNAPSHOT");
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "100");
    exec(&mut b, "UPDATE acct SET bal = 777 WHERE id = 1");
    exec(&mut b, "INSERT acct VALUES (9, 9)");
    exec(&mut b, "DELETE acct WHERE id = 3");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "100", "snapshot must not see the update");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM acct"), "3", "snapshot must not see inserts or deletes");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 3"), "300");
    exec(&mut a, "COMMIT");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "777");
}

#[test]
fn snapshot_readers_do_not_block_on_writers() {
    let (_s, mut a, b) = two();
    exec(&mut a, "ALTER DATABASE master SET ALLOW_SNAPSHOT_ISOLATION ON");
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 5 WHERE id = 1");
    let mut b = b;
    exec(&mut b, "SET TRANSACTION ISOLATION LEVEL SNAPSHOT");
    exec(&mut b, "BEGIN TRAN");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "100");
    exec(&mut b, "COMMIT");
    exec(&mut a, "ROLLBACK");
}

#[test]
fn snapshot_update_conflict_aborts_the_transaction() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "ALTER DATABASE master SET ALLOW_SNAPSHOT_ISOLATION ON");
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL SNAPSHOT");
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "100");
    exec(&mut b, "UPDATE acct SET bal = 500 WHERE id = 1");
    let e = err(&mut a, "UPDATE acct SET bal = bal + 1 WHERE id = 1");
    assert_eq!(e.number, 3960);
    assert_eq!(scalar(&mut a, "SELECT @@TRANCOUNT"), "0");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "500");
}

#[test]
fn read_committed_snapshot_makes_readers_non_blocking() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "ALTER DATABASE master SET READ_COMMITTED_SNAPSHOT ON");
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 5 WHERE id = 1");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "100", "reader sees the last committed value without waiting");
    exec(&mut a, "COMMIT");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "5");
}

#[test]
fn unique_key_insert_waits_for_a_conflicting_uncommitted_insert() {
    let (_s, mut a, b) = two();
    exec(&mut a, "BEGIN TRAN; INSERT acct VALUES (10, 1)");
    let ins = Async::start(b, "INSERT acct VALUES (10, 2)");
    assert!(ins.blocked());
    exec(&mut a, "ROLLBACK");
    let (mut b, r) = ins.finish();
    assert!(r.errors().is_empty(), "insert proceeds once the other transaction rolled back");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 10"), "2");

    let (_s, mut a, b) = two();
    exec(&mut a, "BEGIN TRAN; INSERT acct VALUES (10, 1)");
    let ins = Async::start(b, "INSERT acct VALUES (10, 2)");
    assert!(ins.blocked());
    exec(&mut a, "COMMIT");
    let (_b, r) = ins.finish();
    assert_eq!(r.first_error().unwrap().number, 2627);
}

#[test]
fn locks_are_released_when_a_connection_is_dropped() {
    let (s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 1 WHERE id = 1");
    drop(a);
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "100");
    let _ = s;
}

#[test]
fn xact_abort_rolls_the_transaction_back_on_error() {
    let (_s, mut a, mut b) = two();
    let r = a.execute("SET XACT_ABORT ON; BEGIN TRAN; UPDATE acct SET bal = 0 WHERE id = 1; SELECT 1/0; UPDATE acct SET bal = 0 WHERE id = 2");
    assert_eq!(r.errors().len(), 1);
    assert_eq!(scalar(&mut a, "SELECT @@TRANCOUNT"), "0");
    assert_eq!(rows(&mut b, "SELECT bal FROM acct WHERE id < 3 ORDER BY id"), vec![vec!["100"], vec!["200"]]);
}

#[test]
fn statement_failure_inside_a_transaction_undoes_only_that_statement() {
    let (_s, mut a, mut b) = two();
    exec(&mut a, "BEGIN TRAN; UPDATE acct SET bal = 1 WHERE id = 1");
    assert_eq!(err_no(&mut a, "INSERT acct VALUES (7, 7), (2, 2)"), 2627);
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM acct WHERE id = 7"), "0");
    assert_eq!(scalar(&mut a, "SELECT bal FROM acct WHERE id = 1"), "1");
    exec(&mut a, "COMMIT");
    assert_eq!(scalar(&mut b, "SELECT bal FROM acct WHERE id = 1"), "1");
}

#[test]
fn concurrent_counter_increments_are_not_lost() {
    let s = common::server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE ctr (id INT PRIMARY KEY, n INT)");
    exec(&mut c, "INSERT ctr VALUES (1, 0)");
    let mut handles = vec![];
    for _ in 0..4 {
        let mut conn = s.connect();
        handles.push(thread::spawn(move || {
            for _ in 0..25 {
                loop {
                    let r = conn.execute("UPDATE ctr SET n = n + 1 WHERE id = 1");
                    match r.first_error() {
                        None => break,
                        Some(e) if e.number == 1205 => continue,
                        Some(e) => panic!("{}", e),
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT n FROM ctr"), "100");
}

#[test]
fn read_modify_write_with_repeatable_read_deadlocks_are_resolved() {
    let s = common::server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE ctr (id INT PRIMARY KEY, n INT)");
    exec(&mut c, "INSERT ctr VALUES (1, 0)");
    let mut handles = vec![];
    for _ in 0..3 {
        let mut conn = s.connect();
        handles.push(thread::spawn(move || {
            let mut done = 0;
            while done < 5 {
                let r = conn.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; BEGIN TRAN; DECLARE @v INT; SELECT @v = n FROM ctr WHERE id = 1; UPDATE ctr SET n = @v + 1 WHERE id = 1; COMMIT");
                match r.first_error() {
                    None => done += 1,
                    Some(e) if e.number == 1205 => {} // deadlock victim: retry
                    Some(e) => panic!("{}", e),
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT n FROM ctr"), "15");
}
