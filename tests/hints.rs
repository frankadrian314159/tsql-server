//! Table hints (NOLOCK, UPDLOCK, READPAST, NOWAIT, ...) and OPTION query hints.
mod common;
use common::*;

fn setup() -> (tsql_server::Server, tsql_server::Connection) {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE h (id INT PRIMARY KEY, v INT); INSERT h VALUES (1, 10), (2, 20), (3, 30)");
    (s, c)
}

#[test]
fn nolock_reads_uncommitted_rows_instead_of_waiting() {
    let (s, mut a) = setup();
    exec(&mut a, "BEGIN TRAN; UPDATE h SET v = 99 WHERE id = 1; INSERT h VALUES (4, 40)");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 200");
    assert_eq!(err(&mut b, "SELECT COUNT(*) FROM h").number, 1222, "a normal read waits for the writer");
    assert_eq!(scalar(&mut b, "SELECT v FROM h WITH (NOLOCK) WHERE id = 1"), "99");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM h WITH (NOLOCK)"), "4");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM h WITH (READUNCOMMITTED)"), "4");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM h AS x WITH (NOLOCK) JOIN h AS y WITH (NOLOCK) ON x.id = y.id"), "4");
    exec(&mut a, "ROLLBACK");
}

#[test]
fn readpast_skips_locked_rows() {
    let (s, mut a) = setup();
    exec(&mut a, "BEGIN TRAN; UPDATE h SET v = 99 WHERE id = 2");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 200");
    let r = rows(&mut b, "SELECT id FROM h WITH (READPAST) ORDER BY id");
    assert_eq!(r, [["1"], ["3"]]);
    exec(&mut a, "ROLLBACK");
}

#[test]
fn updlock_and_xlock_keep_the_rows_locked() {
    let (s, mut a) = setup();
    exec(&mut a, "BEGIN TRAN; SELECT v FROM h WITH (UPDLOCK) WHERE id = 1");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 200");
    assert_eq!(err(&mut b, "UPDATE h SET v = 0 WHERE id = 1").number, 1222);
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM h WITH (READPAST) WHERE id = 2"), "1", "other rows stay free");
    exec(&mut b, "UPDATE h SET v = 0 WHERE id = 2");
    exec(&mut a, "COMMIT");
    exec(&mut b, "UPDATE h SET v = 0 WHERE id = 1");
}

#[test]
fn tablock_and_holdlock_take_table_and_range_locks() {
    let (s, mut a) = setup();
    exec(&mut a, "BEGIN TRAN; SELECT COUNT(*) FROM h WITH (TABLOCKX)");
    let mut b = s.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 200");
    assert_eq!(err(&mut b, "SELECT COUNT(*) FROM h").number, 1222);
    exec(&mut a, "ROLLBACK");
    exec(&mut a, "BEGIN TRAN; SELECT id FROM h WITH (HOLDLOCK) WHERE id BETWEEN 1 AND 5");
    assert_eq!(err(&mut b, "INSERT h VALUES (5, 50)").number, 1222, "HOLDLOCK is SERIALIZABLE for this table: no phantoms");
    exec(&mut b, "INSERT h VALUES (9, 90)");
    exec(&mut a, "ROLLBACK");
}

#[test]
fn nowait_fails_at_once() {
    let (s, mut a) = setup();
    exec(&mut a, "BEGIN TRAN; UPDATE h SET v = 1 WHERE id = 1");
    let mut b = s.connect();
    let t = std::time::Instant::now();
    assert_eq!(err(&mut b, "SELECT * FROM h WITH (NOWAIT) WHERE id = 1").number, 1222);
    assert!(t.elapsed() < std::time::Duration::from_secs(1));
    exec(&mut a, "ROLLBACK");
}

#[test]
fn unknown_hints_are_rejected_and_harmless_ones_accepted() {
    let (_s, mut c) = setup();
    assert_eq!(err(&mut c, "SELECT * FROM h WITH (NOSUCHHINT)").number, 321);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM h WITH (ROWLOCK, INDEX(0), NOEXPAND)"), "3");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM h WITH (FORCESEEK)"), "3");
    exec(&mut c, "SELECT * FROM h WITH (FORCESCAN) WHERE id = 2");
    assert!(c.last_plan().join("\n").contains("full scan"), "{:?}", c.last_plan());
}

fn two_tables(c: &mut tsql_server::Connection) {
    exec(c, "CREATE TABLE big2 (id INT PRIMARY KEY, g INT); CREATE TABLE small2 (g INT PRIMARY KEY, label VARCHAR(5));");
    let mut sql = String::new();
    for i in 0..300 {
        sql.push_str(&format!("INSERT big2 VALUES ({}, {});\n", i, i % 10));
    }
    for g in 0..10 {
        sql.push_str(&format!("INSERT small2 VALUES ({}, 'g{}');\n", g, g));
    }
    script(c, &sql);
}

#[test]
fn query_hints_choose_the_join_method_and_order_without_changing_results() {
    let (_s, mut c) = setup();
    two_tables(&mut c);
    let q = "SELECT b.id, s.label FROM big2 b JOIN small2 s ON b.g = s.g";
    let base = rows(&mut c, &format!("{} ORDER BY b.id", q));
    for opt in ["HASH JOIN", "LOOP JOIN", "MERGE JOIN", "FORCE ORDER", "MAXDOP 1", "RECOMPILE", "HASH JOIN, FORCE ORDER"] {
        let got = rows(&mut c, &format!("{} ORDER BY b.id OPTION ({})", q, opt));
        assert_eq!(got, base, "OPTION ({})", opt);
    }
    rows(&mut c, &format!("{} OPTION (HASH JOIN)", q));
    assert!(c.last_plan().join("\n").contains("hash join"), "{:?}", c.last_plan());
    assert!(!c.last_plan().join("\n").contains("index nested-loop"), "{:?}", c.last_plan());
    rows(&mut c, "SELECT * FROM small2 s JOIN big2 b ON b.g = s.g OPTION (FORCE ORDER)");
    let plan = c.last_plan().join(" ");
    assert!(plan.contains("join order: s -> b") || (plan.find("scan s").is_some() && plan.find("scan s") < plan.find("scan b")), "{}", plan);
}
