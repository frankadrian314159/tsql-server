//! WAITFOR, KILL, sp_who, sp_configure, session and statistics views.
mod common;
use common::*;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn waitfor_really_waits() {
    let mut c = conn();
    let t = Instant::now();
    exec(&mut c, "WAITFOR DELAY '00:00:00.300'");
    assert!(t.elapsed() >= Duration::from_millis(280), "{:?}", t.elapsed());
}

#[test]
fn waitfor_time_that_is_close_waits_until_then() {
    let mut c = conn();
    let target = scalar(&mut c, "SELECT CONVERT(VARCHAR(12), DATEADD(MILLISECOND, 400, CAST(GETDATE() AS TIME)), 114)");
    let t = Instant::now();
    exec(&mut c, &format!("WAITFOR TIME '{}'", target));
    assert!(t.elapsed() >= Duration::from_millis(300) && t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(err(&mut c, "WAITFOR DELAY 'not a time'").number, 9815);
}

#[test]
fn other_sessions_run_during_a_waitfor() {
    let s = server();
    let mut a = s.connect();
    let mut b = s.connect();
    let h = thread::spawn(move || {
        let t = Instant::now();
        a.execute("WAITFOR DELAY '00:00:00.500'");
        t.elapsed()
    });
    thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    exec(&mut b, "SELECT 1");
    assert!(t.elapsed() < Duration::from_millis(300), "a waiting session must not block others");
    assert!(h.join().unwrap() >= Duration::from_millis(450));
}

#[test]
fn kill_ends_a_waiting_session() {
    let s = server();
    let mut victim = s.connect();
    let spid = victim.spid();
    let h = thread::spawn(move || {
        let t = Instant::now();
        let r = victim.execute("BEGIN TRAN; CREATE TABLE k (x INT); INSERT k VALUES (1); WAITFOR DELAY '00:00:30'; SELECT 1");
        (r.first_error().map(|e| e.number), t.elapsed(), victim)
    });
    thread::sleep(Duration::from_millis(200));
    let mut admin = s.connect();
    assert_eq!(scalar(&mut admin, &format!("SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE session_id = {}", spid)), "1");
    exec(&mut admin, &format!("KILL {}", spid));
    let (number, took, mut victim) = h.join().unwrap();
    assert_eq!(number, Some(596));
    assert!(took < Duration::from_secs(5), "{:?}", took);
    // the transaction was rolled back, the session can be used again
    assert_eq!(scalar(&mut victim, "SELECT @@TRANCOUNT"), "0");
    assert_eq!(scalar(&mut admin, "SELECT COUNT(*) FROM sys.tables WHERE name = 'k'"), "0");
}

#[test]
fn kill_frees_a_session_blocked_on_a_lock() {
    let s = server();
    let mut a = s.connect();
    exec(&mut a, "CREATE TABLE lk (id INT PRIMARY KEY); INSERT lk VALUES (1); BEGIN TRAN; UPDATE lk SET id = 2 WHERE id = 1");
    let mut b = s.connect();
    let spid = b.spid();
    let h = thread::spawn(move || b.execute("UPDATE lk SET id = 3 WHERE id = 1").first_error().map(|e| e.number));
    thread::sleep(Duration::from_millis(300));
    let mut admin = s.connect();
    exec(&mut admin, &format!("KILL {}", spid));
    assert_eq!(h.join().unwrap(), Some(596));
    exec(&mut a, "ROLLBACK");
}

#[test]
fn kill_is_checked() {
    let s = server();
    let mut a = s.connect();
    let me = a.spid();
    assert_eq!(err(&mut a, &format!("KILL {}", me)).number, 6104);
    assert_eq!(err(&mut a, "KILL 32000").number, 6106);
    exec(&mut a, "CREATE LOGIN pat WITH PASSWORD = 'x'");
    let mut pat = s.connect_as("pat", "x").unwrap();
    assert_eq!(err(&mut pat, &format!("KILL {}", me)).number, 300);
}

#[test]
fn sp_who_and_the_session_views_list_every_session() {
    let s = server();
    let mut a = s.connect();
    let b = s.connect();
    let r = rows(&mut a, "EXEC sp_who");
    assert_eq!(r.len(), 2);
    assert!(r.iter().any(|x| x[0] == b.spid().to_string()));
    let r2 = rows(&mut a, "EXEC sp_who2");
    assert_eq!(r2[0].len(), 9);
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM sys.dm_exec_sessions"), "2");
    drop(b);
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM sys.dm_exec_sessions"), "1", "a closed session disappears");
    assert_eq!(scalar(&mut a, "SELECT COUNT(*) FROM sys.dm_exec_requests"), "1");
}

#[test]
fn sp_configure_lists_sets_and_reconfigures() {
    let mut c = conn();
    let basic = rows(&mut c, "EXEC sp_configure");
    assert!(!basic.iter().any(|r| r[0] == "max degree of parallelism"), "advanced options are hidden");
    assert_eq!(err(&mut c, "EXEC sp_configure 'max degree of parallelism'").number, 15123);
    exec(&mut c, "EXEC sp_configure 'show advanced options', 1; RECONFIGURE");
    let adv = rows(&mut c, "EXEC sp_configure");
    assert!(adv.iter().any(|r| r[0] == "max degree of parallelism"));
    exec(&mut c, "EXEC sp_configure 'max degree of parallelism', 4");
    assert_eq!(rows(&mut c, "EXEC sp_configure 'max degree of parallelism'")[0][3..5], ["4", "0"], "configured but not yet running");
    exec(&mut c, "RECONFIGURE");
    assert_eq!(scalar(&mut c, "SELECT value_in_use FROM sys.configurations WHERE name = 'max degree of parallelism'"), "4");
    assert_eq!(err(&mut c, "EXEC sp_configure 'max degree of parallelism', 99999").number, 15129);
}

#[test]
fn query_statistics_count_executions() {
    let mut c = conn();
    exec(&mut c, "SELECT 4242 AS unique_marker");
    exec(&mut c, "SELECT 4242 AS unique_marker");
    let r = rows(&mut c, "SELECT execution_count, total_rows FROM sys.dm_exec_query_stats WHERE sql_text = 'SELECT 4242 AS unique_marker'");
    assert_eq!(r, [["2", "2"]]);
}

#[test]
fn repeated_batches_are_parsed_once_and_dbcc_empties_the_cache() {
    let mut c = conn();
    for _ in 0..3 {
        exec(&mut c, "SELECT 777 AS cached_marker");
    }
    let n = scalar(&mut c, "SELECT usecounts FROM sys.dm_exec_cached_plans WHERE text = 'SELECT 777 AS cached_marker'");
    assert_eq!(n, "3");
    exec(&mut c, "DBCC FREEPROCCACHE");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.dm_exec_cached_plans WHERE text = 'SELECT 777 AS cached_marker'"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.dm_exec_query_stats WHERE sql_text = 'SELECT 777 AS cached_marker'"), "0");
    // the cache never changes what a statement means: the same text under another session setting
    exec(&mut c, "SET DATEFIRST 1");
    let a = scalar(&mut c, "SELECT DATEPART(weekday, '2026-11-09')");
    exec(&mut c, "SET DATEFIRST 7");
    let b = scalar(&mut c, "SELECT DATEPART(weekday, '2026-11-09')");
    assert_ne!(a, b);
}
