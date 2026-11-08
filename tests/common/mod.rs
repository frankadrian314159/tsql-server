#![allow(dead_code)]
use tsql_server::*;

/// A fresh server. With `TSQL_TEST_PAGED=1` it is a disk-backed server with a tiny buffer pool
/// (8 pages), so the whole suite doubles as an eviction stress test of the storage engine.
pub fn server() -> Server {
    if std::env::var("TSQL_TEST_PAGED").is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let s = Server::open_with(dir.path(), StorageOptions { pool_pages: 8, temp_pool_pages: 8 }).unwrap();
        // the directory lives as long as the process; the OS temp cleaner removes it
        std::mem::forget(dir);
        s
    } else {
        Server::new()
    }
}

pub fn conn() -> Connection {
    server().connect()
}

pub fn exec(c: &mut Connection, sql: &str) -> BatchResult {
    let r = c.execute(sql);
    if let Some(e) = r.first_error() {
        panic!("unexpected error running:\n{}\n=> {}", sql, e);
    }
    r
}

pub fn rows(c: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let r = exec(c, sql);
    match r.resultsets().first() {
        Some(rs) => rs.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect(),
        None => vec![],
    }
}

pub fn scalar(c: &mut Connection, sql: &str) -> String {
    let r = rows(c, sql);
    assert!(!r.is_empty(), "no rows for {}", sql);
    r[0][0].clone()
}

pub fn err(c: &mut Connection, sql: &str) -> SqlError {
    let r = c.execute(sql);
    match r.first_error() {
        Some(e) => e.clone(),
        None => panic!("expected an error from:\n{}", sql),
    }
}

pub fn err_no(c: &mut Connection, sql: &str) -> i32 {
    err(c, sql).number
}

pub fn msgs(c: &mut Connection, sql: &str) -> Vec<String> {
    let r = exec(c, sql);
    r.messages().iter().map(|s| s.to_string()).collect()
}

pub fn script(c: &mut Connection, sql: &str) {
    for r in c.execute_script(sql) {
        if let Some(e) = r.first_error() {
            panic!("script error: {}", e);
        }
    }
}
