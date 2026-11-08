//! Paged storage: data bigger than the buffer pool, eviction, spilling, recovery and space reuse.
mod common;
use common::*;
use tsql_server::{Server, StorageOptions};

fn pad() -> &'static str {
    &"x".repeat(150).leak()[..]
}

fn open(dir: &std::path::Path, pool: usize) -> Server {
    Server::open_with(dir, StorageOptions { pool_pages: pool, temp_pool_pages: 16 }).expect("open data directory")
}

/// `CREATE TABLE big` plus `n` rows: (id, id % 100, 150-byte pad).
fn load_big(c: &mut tsql_server::Connection, n: i32) {
    exec(c, "CREATE TABLE big (id INT PRIMARY KEY, grp INT, pad VARCHAR(200))");
    exec(c, "CREATE INDEX ix_grp ON big(grp)");
    let mut i = 0;
    while i < n {
        let hi = (i + 500).min(n);
        let vals: Vec<String> = (i..hi).map(|k| format!("({}, {}, '{}')", k, k % 100, pad())).collect();
        exec(c, &format!("INSERT big VALUES {}", vals.join(",")));
        i = hi;
    }
}

#[test]
fn a_table_much_larger_than_the_buffer_pool_works() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path(), 64); // 512 KiB of cache
    let mut c = s.connect();
    load_big(&mut c, 20_000);

    let (st, _) = s.storage_stats();
    assert!(st.evictions > 0 && st.writes > 0, "{:?}", st);
    assert!(st.pages_cached <= 64 + 8, "the pool grew past its limit: {:?}", st);
    assert!(st.file_blocks > 64 * 5, "the data should be several times the cache: {:?}", st);

    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "20000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 7"), "200");
    assert_eq!(scalar(&mut c, "SELECT pad FROM big WHERE id = 12345"), pad());
    assert!(c.last_plan().iter().any(|l| l.contains("index seek")), "{:?}", c.last_plan());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE id BETWEEN 100 AND 199"), "100");
    assert_eq!(rows(&mut c, "SELECT TOP 3 id FROM big ORDER BY id DESC"), vec![vec!["19999"], vec!["19998"], vec!["19997"]]);
    assert_eq!(scalar(&mut c, "SELECT SUM(CAST(id AS BIGINT)) FROM big"), "199990000");

    // changes through the cache
    exec(&mut c, "UPDATE big SET grp = 1000 WHERE id < 1000");
    exec(&mut c, "DELETE big WHERE grp = 3");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "19810");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 1000"), "1000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 3"), "0");
    // the same answers without the optimizer's index paths
    exec(&mut c, "SET OPTIMIZER OFF");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 1000"), "1000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 3"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE id BETWEEN 100 AND 199"), "100");
}

#[test]
fn a_big_database_reopens_cold_and_reads_pages_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path(), 64);
        let mut c = s.connect();
        load_big(&mut c, 10_000);
    } // clean shutdown: checkpoint
    let s = open(dir.path(), 64);
    let (st0, _) = s.storage_stats();
    assert_eq!(st0.pages_cached, 0, "nothing is loaded until it is needed");
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT pad FROM big WHERE id = 4242"), pad());
    let (st1, _) = s.storage_stats();
    assert!(st1.reads > 0 && st1.reads < 40, "a point lookup reads a handful of pages, read {}", st1.reads);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "10000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 99"), "100");
    let (st2, _) = s.storage_stats();
    assert!(st2.pages_cached <= 64 + 8, "{:?}", st2);
    // the pool statistics are visible from SQL too
    let r = rows(&mut c, "SELECT store, pool_pages FROM sys.dm_os_buffer_pool ORDER BY store");
    assert_eq!(r[0], vec!["data", "64"]);
}

#[test]
fn evicted_uncommitted_pages_never_reach_the_recovered_database() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path(), 16); // 128 KiB: a transaction this size cannot stay in the cache
        let mut c = s.connect();
        load_big(&mut c, 3_000);
        s.checkpoint();
        exec(&mut c, "INSERT big VALUES (5000, 1, 'after the checkpoint')");
        exec(&mut c, "UPDATE big SET pad = 'changed' WHERE id = 10");
        let mut c2 = s.connect();
        exec(&mut c2, "BEGIN TRAN");
        let vals: Vec<String> = (10_000..16_000).map(|k| format!("({}, {}, '{}')", k, k % 100, pad())).collect();
        for chunk in vals.chunks(500) {
            exec(&mut c2, &format!("INSERT big VALUES {}", chunk.join(",")));
        }
        exec(&mut c2, "UPDATE big SET pad = 'dirty' WHERE id < 1500");
        exec(&mut c2, "DELETE big WHERE id BETWEEN 2000 AND 2500");
        let (st, _) = s.storage_stats();
        assert!(st.writes > 100, "the uncommitted work should have been written back to the page file: {:?}", st);
        drop(c2);
        drop(c);
        s.simulate_crash();
    }
    let s = open(dir.path(), 16);
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "3001");
    assert_eq!(scalar(&mut c, "SELECT pad FROM big WHERE id = 10"), "changed");
    assert_eq!(scalar(&mut c, "SELECT pad FROM big WHERE id = 1499"), pad());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE id BETWEEN 2000 AND 2500"), "501");
    assert_eq!(scalar(&mut c, "SELECT pad FROM big WHERE id = 5000"), "after the checkpoint");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE id >= 10000"), "0");
    // the recovered store is usable: more work, another restart
    exec(&mut c, "INSERT big VALUES (7000, 7, 'again')");
    drop(c);
    drop(s);
    let s = open(dir.path(), 16);
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "3002");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 7"), "31");
}

#[test]
fn rolling_back_a_transaction_larger_than_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path(), 16);
    let mut c = s.connect();
    load_big(&mut c, 1_000);
    exec(&mut c, "BEGIN TRAN");
    let vals: Vec<String> = (2_000..8_000).map(|k| format!("({}, {}, '{}')", k, k % 100, pad())).collect();
    for chunk in vals.chunks(500) {
        exec(&mut c, &format!("INSERT big VALUES {}", chunk.join(",")));
    }
    exec(&mut c, "UPDATE big SET grp = 5000 WHERE id < 500");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "7000");
    exec(&mut c, "ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "1000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 5000"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 5"), "10");
    exec(&mut c, "SET OPTIMIZER OFF");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 5"), "10");
}

#[test]
fn rows_larger_than_a_page_are_stored_in_overflow_pages() {
    let dir = tempfile::tempdir().unwrap();
    let big = "ab".repeat(150_000); // 300,000 characters
    {
        let s = open(dir.path(), 16);
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE docs (id INT PRIMARY KEY, body NVARCHAR(MAX), note VARCHAR(20))");
        for i in 0..6 {
            exec(&mut c, &format!("INSERT docs VALUES ({}, REPLICATE(CAST(N'ab' AS NVARCHAR(MAX)), 150000), 'n{}')", i, i));
        }
        exec(&mut c, "UPDATE docs SET body = body + N'!' WHERE id = 2");
        exec(&mut c, "DELETE docs WHERE id = 3");
        assert_eq!(scalar(&mut c, "SELECT LEN(body) FROM docs WHERE id = 2"), "300001");
        s.simulate_crash();
    }
    let s = open(dir.path(), 16);
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM docs"), "5");
    assert_eq!(scalar(&mut c, "SELECT LEN(body) FROM docs WHERE id = 2"), "300001");
    assert_eq!(scalar(&mut c, "SELECT LEN(body) FROM docs WHERE id = 4"), "300000");
    assert_eq!(scalar(&mut c, "SELECT RIGHT(body, 3) FROM docs WHERE id = 2"), "ab!");
    assert_eq!(scalar(&mut c, "SELECT SUM(LEN(body)) FROM docs"), "1500001");
    let _ = big;
}

#[test]
fn dropped_tables_give_their_space_back() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path(), 64);
    let mut c = s.connect();
    load_big(&mut c, 8_000);
    s.checkpoint();
    let (full, _) = s.storage_stats();
    exec(&mut c, "DROP TABLE big");
    s.checkpoint();
    let (empty, _) = s.storage_stats();
    assert!(empty.free_blocks as u64 > full.file_blocks / 2, "dropping the table should release its blocks: before {:?}, after {:?}", full, empty);
    load_big(&mut c, 8_000);
    s.checkpoint();
    let (again, _) = s.storage_stats();
    assert!(again.file_blocks <= full.file_blocks + full.file_blocks / 4, "the file should be reused, not grown: {:?} -> {:?}", full, again);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "8000");
}

#[test]
fn schema_changes_on_a_big_table_survive_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path(), 32);
        let mut c = s.connect();
        load_big(&mut c, 5_000);
        exec(&mut c, "ALTER TABLE big ADD flag INT NOT NULL DEFAULT 7");
        exec(&mut c, "CREATE INDEX ix_flag_id ON big(flag, id)");
        exec(&mut c, "UPDATE big SET flag = 8 WHERE grp < 10");
        s.simulate_crash();
    }
    let s = open(dir.path(), 32);
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "5000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE flag = 8"), "500");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE flag = 7"), "4500");
    assert_eq!(rows(&mut c, "SELECT TOP 2 id FROM big WHERE flag = 7 ORDER BY id"), vec![vec!["10"], vec!["11"]]);
    exec(&mut c, "SET OPTIMIZER OFF");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE flag = 8"), "500");
    assert_eq!(err_no(&mut c, "INSERT big (id, grp, pad) VALUES (1, 1, 'dup')"), 2627);
}

#[test]
fn temporary_tables_spill_to_their_own_file() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path(), 64);
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE #t (id INT PRIMARY KEY, pad VARCHAR(200))");
    for chunk in 0..10 {
        let vals: Vec<String> = (chunk * 500..chunk * 500 + 500).map(|k| format!("({}, '{}')", k, pad())).collect();
        exec(&mut c, &format!("INSERT #t VALUES {}", vals.join(",")));
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM #t WHERE id >= 4000"), "1000");
    let (data, temp) = s.storage_stats();
    assert!(temp.evictions > 0, "{:?}", temp);
    assert_eq!(data.pages_live, 0, "temporary tables must not use the database file");
    exec(&mut c, "DROP TABLE #t");
    let (_, temp2) = s.storage_stats();
    assert!(temp2.pages_live < 5, "{:?}", temp2);
}

#[test]
fn index_keys_longer_than_the_limit_are_rejected() {
    let c = &mut conn();
    exec(c, "CREATE TABLE k (id INT PRIMARY KEY, s VARCHAR(4000))");
    exec(c, "CREATE INDEX ix_s ON k(s)");
    exec(c, &format!("INSERT k VALUES (1, '{}')", "a".repeat(1000)));
    assert_eq!(err_no(c, &format!("INSERT k VALUES (2, '{}')", "a".repeat(2500))), 1946);
    assert_eq!(scalar(c, "SELECT COUNT(*) FROM k"), "1");
}

#[test]
fn the_paged_engine_is_used_by_in_memory_servers_too() {
    let s = Server::new();
    let mut c = s.connect();
    load_big(&mut c, 2_000);
    let (st, _) = s.storage_stats();
    assert_eq!(st.evictions, 0, "an in-memory server never evicts");
    assert!(st.pages_live > 20);
    exec(&mut c, "DROP TABLE big");
    let (st2, _) = s.storage_stats();
    assert!(st2.pages_live < 5, "dropping a table frees its pages: {:?}", st2);
}

#[test]
fn a_crash_halfway_through_a_checkpoint_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = open(dir.path(), 32);
        let mut c = s.connect();
        load_big(&mut c, 3_000);
        s.checkpoint();
        // work after the checkpoint: it is in the log, and its pages are about to be written
        exec(&mut c, "UPDATE big SET pad = 'v2' WHERE id % 2 = 0");
        exec(&mut c, "DELETE big WHERE id >= 2500");
        exec(&mut c, "INSERT big VALUES (9000, 9, 'new')");
        // every dirty page reaches the page file, but the process dies before the checkpoint file is replaced
        s.flush_pages_only();
        s.simulate_crash();
    }
    let s = open(dir.path(), 32);
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big"), "2501");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE pad = 'v2'"), "1250");
    assert_eq!(scalar(&mut c, "SELECT pad FROM big WHERE id = 9000"), "new");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE id >= 2500 AND id < 9000"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big WHERE grp = 4"), "25");
}
