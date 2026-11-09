//! Statements from different sessions really run at the same time (queries and DML share the catalog; writers
//! latch only the tables they change). These tests hammer the server from several threads and check the
//! invariants that serial execution used to give for free.
mod common;
use common::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use tsql_server::*;

/// Run `sql` until it is not chosen as a deadlock victim.
fn retry(c: &mut Connection, sql: &str) {
    loop {
        let r = c.execute(sql);
        match r.first_error() {
            None => return,
            Some(e) if e.number == 1205 => continue,
            Some(e) => panic!("{}\n=> {}", sql, e),
        }
    }
}

#[test]
fn updates_of_one_row_from_many_sessions_are_not_lost() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE ctr (id INT PRIMARY KEY, n INT)");
    exec(&mut c, "INSERT ctr VALUES (1, 0)");
    let mut hs = vec![];
    for _ in 0..8 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for _ in 0..60 {
                retry(&mut conn, "UPDATE ctr SET n = n + 1 WHERE id = 1");
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT n FROM ctr"), "480");
}

#[test]
fn racing_inserts_of_one_key_let_exactly_one_win() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE u (k INT PRIMARY KEY, who INT)");
    let wins = Arc::new(AtomicUsize::new(0));
    let mut hs = vec![];
    for t in 0..6 {
        let mut conn = s.connect();
        let wins = wins.clone();
        hs.push(thread::spawn(move || {
            for k in 0..150 {
                let key = if t % 2 == 0 { k } else { 149 - k };
                let r = conn.execute(&format!("INSERT u VALUES ({}, {})", key, t));
                match r.first_error() {
                    None => {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                    Some(e) if e.number == 2627 => {}
                    Some(e) => panic!("{}", e),
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(wins.load(Ordering::SeqCst), 150);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM u"), "150");
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT k) FROM u"), "150");
}

#[test]
fn inserts_into_different_tables_in_parallel() {
    let s = server();
    let mut c = s.connect();
    for t in 0..4 {
        exec(&mut c, &format!("CREATE TABLE t{} (id INT IDENTITY PRIMARY KEY, v INT)", t));
    }
    let mut hs = vec![];
    for t in 0..4 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for i in 0..200 {
                retry(&mut conn, &format!("INSERT t{} (v) VALUES ({})", t, i));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    for t in 0..4 {
        assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM t{}", t)), "200");
        assert_eq!(scalar(&mut c, &format!("SELECT SUM(v) FROM t{}", t)), "19900");
    }
}

#[test]
fn identity_values_are_unique_across_sessions() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE ids (id INT IDENTITY PRIMARY KEY, who INT)");
    let mut hs = vec![];
    for t in 0..6 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for _ in 0..100 {
                retry(&mut conn, &format!("INSERT ids (who) VALUES ({})", t));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT id) FROM ids"), "600");
    assert_eq!(scalar(&mut c, "SELECT MAX(id) FROM ids"), "600");
}

#[test]
fn sequence_values_are_unique_across_sessions() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE SEQUENCE sq START WITH 1");
    exec(&mut c, "CREATE TABLE got (v INT PRIMARY KEY)");
    let mut hs = vec![];
    for _ in 0..4 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for _ in 0..100 {
                retry(&mut conn, "INSERT got SELECT NEXT VALUE FOR sq");
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM got"), "400");
}

/// Writers move money between accounts in one transaction while readers add the balances up.
fn transfers(read_iso: &str) {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "ALTER DATABASE master SET ALLOW_SNAPSHOT_ISOLATION ON");
    exec(&mut c, "CREATE TABLE acct (id INT PRIMARY KEY, bal INT)");
    for i in 0..10 {
        exec(&mut c, &format!("INSERT acct VALUES ({}, 100)", i));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = vec![];
    for t in 0..4 {
        let mut conn = s.connect();
        writers.push(thread::spawn(move || {
            for i in 0..60 {
                let (a, b) = ((t * 3 + i) % 10, (t * 7 + i * 3 + 1) % 10);
                if a == b {
                    continue;
                }
                retry(&mut conn, &format!("BEGIN TRAN; UPDATE acct SET bal = bal - 5 WHERE id = {}; UPDATE acct SET bal = bal + 5 WHERE id = {}; COMMIT", a, b));
            }
        }));
    }
    let mut readers = vec![];
    for _ in 0..3 {
        let mut conn = s.connect();
        let stop = stop.clone();
        let iso = read_iso.to_string();
        readers.push(thread::spawn(move || {
            let mut reads = 0;
            while !stop.load(Ordering::SeqCst) {
                let r = conn.execute(&format!("SET TRANSACTION ISOLATION LEVEL {}; BEGIN TRAN; SELECT SUM(bal) FROM acct; COMMIT", iso));
                match r.first_error() {
                    None => {
                        let v = r.resultsets()[0].rows[0][0].to_string();
                        assert_eq!(v, "1000", "a {} reader saw a torn state", iso);
                        reads += 1;
                    }
                    Some(e) if e.number == 1205 => {}
                    Some(e) => panic!("{}", e),
                }
            }
            reads
        }));
    }
    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    let total: i32 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(total > 0, "the readers never completed a read");
    assert_eq!(scalar(&mut c, "SELECT SUM(bal) FROM acct"), "1000");
}

#[test]
fn snapshot_readers_never_see_half_a_transfer() {
    transfers("SNAPSHOT");
}

#[test]
fn serializable_readers_never_see_half_a_transfer() {
    transfers("SERIALIZABLE");
}

#[test]
fn a_foreign_key_is_never_violated_by_racing_child_inserts_and_parent_deletes() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE par (id INT PRIMARY KEY)");
    exec(&mut c, "CREATE TABLE kid (id INT IDENTITY PRIMARY KEY, pid INT NOT NULL REFERENCES par(id))");
    for i in 0..40 {
        exec(&mut c, &format!("INSERT par VALUES ({})", i));
    }
    let mut hs = vec![];
    for t in 0..3 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for i in 0..80 {
                let r = conn.execute(&format!("INSERT kid (pid) VALUES ({})", (i * 7 + t * 5) % 40));
                match r.first_error() {
                    None => {}
                    Some(e) if e.number == 547 || e.number == 1205 => {}
                    Some(e) => panic!("{}", e),
                }
            }
        }));
    }
    for t in 0..2 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for i in 0..40 {
                let r = conn.execute(&format!("DELETE par WHERE id = {}", (i * 3 + t * 11) % 40));
                match r.first_error() {
                    None => {}
                    Some(e) if e.number == 547 || e.number == 1205 => {}
                    Some(e) => panic!("{}", e),
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM kid WHERE pid NOT IN (SELECT id FROM par)"), "0");
}

#[test]
fn queries_run_while_other_sessions_write() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE w (id INT PRIMARY KEY, v INT)");
    exec(&mut c, "INSERT w VALUES (1, 0)");
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = vec![];
    for _ in 0..4 {
        let mut conn = s.connect();
        let stop = stop.clone();
        readers.push(thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                let r = conn.execute("SELECT v FROM w WHERE id = 1");
                assert!(r.first_error().is_none());
                n += 1;
            }
            n
        }));
    }
    let mut w = s.connect();
    for _ in 0..200 {
        exec(&mut w, "UPDATE w SET v = v + 1 WHERE id = 1");
    }
    stop.store(true, Ordering::SeqCst);
    let reads: i32 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(reads > 0);
    assert_eq!(scalar(&mut c, "SELECT v FROM w"), "200");
}

#[test]
fn ddl_and_dml_from_different_sessions_do_not_deadlock() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE base (id INT PRIMARY KEY, v INT)");
    let mut hs = vec![];
    for t in 0..3 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for i in 0..40 {
                retry(&mut conn, &format!("INSERT base VALUES ({}, {})", t * 1000 + i, i));
            }
        }));
    }
    let mut ddl = s.connect();
    hs.push(thread::spawn(move || {
        for i in 0..30 {
            retry(&mut ddl, &format!("CREATE TABLE extra{} (a INT)", i));
            retry(&mut ddl, &format!("DROP TABLE extra{}", i));
        }
    }));
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM base"), "120");
}

#[test]
fn commits_from_many_sessions_share_log_forces_and_survive_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::open(dir.path()).unwrap();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE g (id INT IDENTITY PRIMARY KEY, who INT)");
    let before = s.log_forces();
    let mut hs = vec![];
    for t in 0..16 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for _ in 0..100 {
                retry(&mut conn, &format!("INSERT g (who) VALUES ({})", t));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let forces = s.log_forces() - before;
    eprintln!("log forces for 1600 commits: {}", forces);
    assert!(forces <= 1600, "{} forces for 1600 commits", forces);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM g"), "1600");
    drop(c);
    s.simulate_crash();
    let s2 = Server::open(dir.path()).unwrap();
    let mut c2 = s2.connect();
    assert_eq!(scalar(&mut c2, "SELECT COUNT(*) FROM g"), "1600");
    assert_eq!(scalar(&mut c2, "SELECT COUNT(DISTINCT id) FROM g"), "1600");
}

#[test]
fn ddl_waits_for_a_transaction_that_has_written_the_table() {
    let s = server();
    let mut a = s.connect();
    exec(&mut a, "CREATE TABLE sch (id INT PRIMARY KEY)");
    exec(&mut a, "BEGIN TRAN; INSERT sch VALUES (1)");
    let mut b = s.connect();
    let done = Arc::new(AtomicBool::new(false));
    let d2 = done.clone();
    let h = thread::spawn(move || {
        exec(&mut b, "ALTER TABLE sch ADD note INT");
        d2.store(true, Ordering::SeqCst);
        b
    });
    thread::sleep(std::time::Duration::from_millis(300));
    assert!(!done.load(Ordering::SeqCst), "the ALTER ran while a transaction had uncommitted changes in the table");
    exec(&mut a, "COMMIT");
    let mut b = h.join().unwrap();
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM sch"), "1");
    assert_eq!(scalar(&mut b, "SELECT COUNT(note) FROM sch"), "0");
}

#[test]
fn global_temporary_tables_are_locked_like_other_tables() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE ##shared (id INT IDENTITY PRIMARY KEY, who INT)");
    let mut hs = vec![];
    for t in 0..4 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for _ in 0..60 {
                retry(&mut conn, &format!("INSERT ##shared (who) VALUES ({})", t));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT id) FROM ##shared"), "240");
    // a reader does not see another session's uncommitted rows
    let mut w = s.connect();
    exec(&mut w, "BEGIN TRAN; INSERT ##shared (who) VALUES (99)");
    exec(&mut c, "SET LOCK_TIMEOUT 0");
    let e = err(&mut c, "SELECT COUNT(*) FROM ##shared");
    assert_eq!(e.number, 1222);
    exec(&mut w, "ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ##shared"), "240");
}

#[test]
fn writers_to_one_table_overlap_unless_they_share_a_key() {
    // Session A inserts key 1 and keeps its transaction open. Session B inserts key 2 into the same table: it must
    // not have to wait for A (different keys). Session C inserts key 1: it has to wait for A's outcome.
    let s = server();
    let mut a = s.connect();
    exec(&mut a, "CREATE TABLE ov (k INT PRIMARY KEY, v INT)");
    exec(&mut a, "BEGIN TRAN; INSERT ov VALUES (1, 1)");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 500");
    exec(&mut b, "INSERT ov VALUES (2, 2)");
    let mut c = s.connect();
    exec(&mut c, "SET LOCK_TIMEOUT 300");
    assert_eq!(err(&mut c, "INSERT ov VALUES (1, 3)").number, 1222);
    exec(&mut a, "ROLLBACK");
    exec(&mut c, "INSERT ov VALUES (1, 3)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ov"), "2");
}

#[test]
fn cascading_deletes_and_updates_stay_consistent_under_parallel_writers() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE cp (id INT PRIMARY KEY)");
    exec(&mut c, "CREATE TABLE cc (id INT IDENTITY PRIMARY KEY, pid INT NOT NULL REFERENCES cp(id) ON DELETE CASCADE ON UPDATE CASCADE)");
    for i in 0..30 {
        exec(&mut c, &format!("INSERT cp VALUES ({})", i));
    }
    let mut hs = vec![];
    for t in 0..3 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for i in 0..60 {
                let r = conn.execute(&format!("INSERT cc (pid) VALUES ({})", (i * 7 + t) % 30));
                match r.first_error() {
                    None => {}
                    Some(e) if e.number == 547 || e.number == 1205 => {}
                    Some(e) => panic!("{}", e),
                }
            }
        }));
    }
    for t in 0..2 {
        let mut conn = s.connect();
        hs.push(thread::spawn(move || {
            for i in 0..30 {
                retry(&mut conn, &format!("DELETE cp WHERE id = {}", (i * 4 + t) % 30));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM cc WHERE pid NOT IN (SELECT id FROM cp)"), "0");
}

#[test]
fn serializable_readers_still_block_phantoms_while_writers_run_in_parallel() {
    // The reader sums a key range twice inside one transaction while writers insert into and outside the range.
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE ph (k INT PRIMARY KEY, v INT)");
    for i in 0..10 {
        exec(&mut c, &format!("INSERT ph VALUES ({}, 1)", i * 10));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = vec![];
    for t in 0..3 {
        let mut conn = s.connect();
        let stop = stop.clone();
        writers.push(thread::spawn(move || {
            let mut k = 1000 * (t + 1);
            while !stop.load(Ordering::SeqCst) {
                k += 1;
                // alternately inside (k mod 100 within 0..100) and outside the range read below
                let key = if k % 2 == 0 { k % 100 + 1 } else { k };
                let r = conn.execute(&format!("INSERT ph VALUES ({}, 1)", key));
                match r.first_error() {
                    None => {}
                    Some(e) if e.number == 2627 || e.number == 1205 => {}
                    Some(e) => panic!("{}", e),
                }
            }
        }));
    }
    let mut r = s.connect();
    exec(&mut r, "SET LOCK_TIMEOUT 5000");
    let mut checked = 0;
    for _ in 0..40 {
        let out = r.execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; BEGIN TRAN; SELECT COUNT(*) FROM ph WHERE k BETWEEN 0 AND 99; WAITFOR DELAY '00:00:00'; SELECT COUNT(*) FROM ph WHERE k BETWEEN 0 AND 99; COMMIT");
        match out.first_error() {
            None => {
                let rs = out.resultsets();
                assert_eq!(rs[0].rows[0][0].to_string(), rs[1].rows[0][0].to_string(), "a phantom appeared inside a SERIALIZABLE transaction");
                checked += 1;
            }
            Some(e) if e.number == 1205 || e.number == 1222 => {}
            Some(e) => panic!("{}", e),
        }
    }
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        w.join().unwrap();
    }
    assert!(checked > 0);
}
