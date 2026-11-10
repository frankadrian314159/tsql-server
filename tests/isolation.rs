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

// ---------------------------------------------------------------- key-range locks (SERIALIZABLE)
fn ranges() -> (Server, Connection, Connection) {
    let (s, mut a, b) = two();
    script(&mut a, "CREATE TABLE kr (id INT PRIMARY KEY, grp INT, v INT)\nGO\nCREATE INDEX ix_grp ON kr (grp)\nGO\nINSERT kr VALUES (10, 1, 0), (20, 1, 0), (30, 2, 0), (40, 2, 0), (50, 3, 0)");
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    (s, a, b)
}

#[test]
fn an_index_range_read_blocks_inserts_into_the_range_only() {
    let (_s, mut a, mut b) = ranges();
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kr WHERE id BETWEEN 15 AND 35"), "2");
    assert!(a.last_plan().join("\n").contains("index"), "the read must use the primary key range: {:?}", a.last_plan());
    // outside the range nothing blocks: inserts, updates and deletes of other rows go through
    exec(&mut b, "INSERT kr VALUES (5, 9, 0)");
    exec(&mut b, "INSERT kr VALUES (45, 9, 0)");
    exec(&mut b, "UPDATE kr SET v = 1 WHERE id = 50");
    exec(&mut b, "DELETE kr WHERE id = 40");
    // inside the range an insert is a phantom and must wait
    let ins = Async::start(b, "INSERT kr VALUES (25, 9, 0)");
    assert!(ins.blocked(), "a phantom insert into a locked key range must wait");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kr WHERE id BETWEEN 15 AND 35"), "2", "the range still reads the same");
    exec(&mut a, "COMMIT");
    let (mut b, r) = ins.finish();
    assert!(r.errors().is_empty());
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM kr WHERE id BETWEEN 15 AND 35"), "3");
}

#[test]
fn a_seek_that_finds_nothing_still_protects_the_gap() {
    let (_s, mut a, mut b) = ranges();
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kr WHERE id = 25"), "0");
    exec(&mut b, "INSERT kr VALUES (26, 9, 0)");
    let ins = Async::start(b, "INSERT kr VALUES (25, 9, 0)");
    assert!(ins.blocked(), "inserting the very key that was looked up and not found is a phantom");
    exec(&mut a, "ROLLBACK");
    let (_b, r) = ins.finish();
    assert!(r.errors().is_empty());
}

#[test]
fn an_update_that_moves_a_row_into_the_range_waits() {
    let (_s, mut a, b) = ranges();
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kr WHERE grp = 2"), "2");
    let mv = Async::start(b, "UPDATE kr SET grp = 2 WHERE id = 50");
    assert!(mv.blocked(), "changing a row's key into a locked range is a phantom");
    exec(&mut a, "COMMIT");
    let (mut b, r) = mv.finish();
    assert!(r.errors().is_empty());
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM kr WHERE grp = 2"), "3");
}

#[test]
fn reads_without_a_usable_index_still_lock_the_whole_table() {
    let (_s, mut a, b) = ranges();
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kr WHERE v = 0"), "5");
    let ins = Async::start(b, "INSERT kr VALUES (999, 9, 0)");
    assert!(ins.blocked(), "a scan has no key range, so it locks the table");
    exec(&mut a, "COMMIT");
    ins.finish().1.errors().is_empty().then_some(()).expect("insert after commit");
}

#[test]
fn read_committed_and_repeatable_read_take_no_ranges() {
    let (_s, mut a, mut b) = two();
    script(&mut a, "CREATE TABLE kq (id INT PRIMARY KEY, v INT)\nGO\nINSERT kq VALUES (1, 0), (9, 0)");
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    exec(&mut a, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kq WHERE id BETWEEN 2 AND 8"), "0");
    exec(&mut b, "INSERT kq VALUES (5, 0)");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kq WHERE id BETWEEN 2 AND 8"), "1", "phantoms are allowed at REPEATABLE READ");
    exec(&mut a, "COMMIT");
}

#[test]
fn key_ranges_are_released_at_commit_and_listed_in_the_lock_view() {
    let (_s, mut a, mut b) = ranges();
    exec(&mut a, "BEGIN TRAN");
    exec(&mut a, "SELECT * FROM kr WHERE id BETWEEN 15 AND 35");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM sys.dm_tran_locks WHERE resource_type = 'KEY_RANGE' AND request_mode = 'RangeS_S'"), "1");
    exec(&mut a, "COMMIT");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM sys.dm_tran_locks WHERE resource_type = 'KEY_RANGE'"), "0");
    exec(&mut b, "INSERT kr VALUES (25, 9, 0)");
}

#[test]
fn two_serializable_transactions_that_insert_into_each_others_ranges_deadlock() {
    let (s, mut a, b) = ranges();
    let mut b = b;
    exec(&mut b, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    exec(&mut a, "BEGIN TRAN");
    exec(&mut b, "BEGIN TRAN");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM kr WHERE id BETWEEN 11 AND 19"), "0");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM kr WHERE id BETWEEN 41 AND 49"), "0");
    let first = Async::start(a, "INSERT kr VALUES (45, 9, 0)");
    assert!(first.blocked());
    let second = Async::start(b, "INSERT kr VALUES (15, 9, 0)");
    thread::sleep(Duration::from_millis(400));
    // one of the two was chosen as the deadlock victim and the other could go on
    let (mut a, ra) = first.finish();
    let (mut b, rb) = second.finish();
    let victims = ra.errors().iter().chain(rb.errors().iter()).filter(|e| e.number == 1205).count();
    assert_eq!(victims, 1, "exactly one deadlock victim: {:?} / {:?}", ra.errors(), rb.errors());
    let _ = (a.execute("ROLLBACK"), b.execute("ROLLBACK"));
    drop(s);
}

#[test]
fn a_waiting_upgrade_is_not_starved_by_readers_that_keep_retaking_shared_locks() {
    // three transactions each read the row under REPEATABLE READ and then update it: every one holds a shared lock the
    // others' updates wait for, so they deadlock and the victims retry at once. Without a fair queue the readers' new
    // shared locks keep the waiting update out forever (this once took minutes and tens of thousands of victims).
    let s = common::server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE ctr (id INT PRIMARY KEY, n INT)");
    exec(&mut c, "INSERT ctr VALUES (1, 0)");
    let started = std::time::Instant::now();
    let victims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = vec![];
    for _ in 0..3 {
        let mut conn = s.connect();
        let victims = victims.clone();
        handles.push(thread::spawn(move || {
            let mut done = 0;
            while done < 5 {
                let r = conn.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; BEGIN TRAN; DECLARE @v INT; SELECT @v = n FROM ctr WHERE id = 1; UPDATE ctr SET n = @v + 1 WHERE id = 1; COMMIT");
                match r.first_error() {
                    None => done += 1,
                    Some(e) if e.number == 1205 => {
                        victims.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Some(e) => panic!("{}", e),
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT n FROM ctr"), "15");
    let v = victims.load(std::sync::atomic::Ordering::Relaxed);
    assert!(started.elapsed().as_secs() < 30 && v < 100_000, "took {:?} with {} deadlock victims", started.elapsed(), v);
}

#[test]
fn a_new_shared_lock_queues_behind_a_waiting_exclusive_request() {
    let s = common::server();
    let mut a = s.connect();
    exec(&mut a, "CREATE TABLE q (id INT PRIMARY KEY, v INT)");
    exec(&mut a, "INSERT q VALUES (1, 0)");
    exec(&mut a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; BEGIN TRAN");
    exec(&mut a, "SELECT v FROM q WHERE id = 1");
    // B wants to write the row A has read, and waits
    let mut b = s.connect();
    let writer = thread::spawn(move || {
        // (in an explicit transaction: a blocked autocommit statement rolls back and starts over, so it holds no place in the queue)
        exec(&mut b, "BEGIN TRAN; UPDATE q SET v = 1 WHERE id = 1; COMMIT");
        b
    });
    thread::sleep(std::time::Duration::from_millis(300));
    // C asks for a new shared lock on the same row: it queues behind B's request, so with a short lock timeout it fails
    let mut c = s.connect();
    exec(&mut c, "SET LOCK_TIMEOUT 200; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; BEGIN TRAN");
    assert_eq!(err_no(&mut c, "SELECT v FROM q WHERE id = 1"), 1222);
    exec(&mut c, "ROLLBACK");
    // a read committed reader is not affected (it takes no row lock), and A's own further reads still work
    let mut d = s.connect();
    assert_eq!(scalar(&mut d, "SELECT v FROM q WHERE id = 1"), "0");
    assert_eq!(scalar(&mut a, "SELECT v FROM q WHERE id = 1"), "0");
    // when A ends, B's update goes through, and then a new reader gets in
    exec(&mut a, "COMMIT");
    let mut b = writer.join().unwrap();
    exec(&mut c, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(scalar(&mut c, "SELECT v FROM q WHERE id = 1"), "1");
    drop(&mut b);
}
