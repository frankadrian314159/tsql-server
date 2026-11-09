//! Filtered indexes (`CREATE INDEX ... WHERE`) and included columns (`INCLUDE (...)`).
mod common;
use common::*;
use tsql_server::Server;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE ord (id INT PRIMARY KEY, cust INT, status VARCHAR(10), amount INT, note VARCHAR(20) NULL);",
    );
    let mut sql = String::new();
    for i in 0..400 {
        let st = if i % 10 == 0 { "open" } else { "done" };
        let note = if i % 4 == 0 { "NULL".to_string() } else { format!("'n{}'", i) };
        sql.push_str(&format!("INSERT ord VALUES ({}, {}, '{}', {}, {});\n", i, i % 25, st, i * 3, note));
    }
    script(c, &sql);
}

#[test]
fn a_filtered_index_holds_only_matching_rows_and_serves_queries_that_imply_its_filter() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "CREATE INDEX ix_open ON ord (cust) WHERE status = 'open'");
    // a query that implies the filter may use the index, and returns the same rows as a scan
    let q = "SELECT id FROM ord WHERE status = 'open' AND cust = 5 ORDER BY id";
    let with = rows(&mut c, q);
    assert!(c.last_plan().join(" ").contains("ix_open"), "{:?}", c.last_plan());
    let scan = rows(&mut c, "SELECT id FROM ord WITH (FORCESCAN) WHERE status = 'open' AND cust = 5 ORDER BY id");
    assert_eq!(with, scan);
    assert_eq!(with.len(), 8);
    // a query that does not imply it must not use it (rows with other statuses are missing from the index)
    let all = rows(&mut c, "SELECT COUNT(*) FROM ord WHERE cust = 5");
    assert!(!c.last_plan().join(" ").contains("ix_open"), "{:?}", c.last_plan());
    assert_eq!(all, [["16"]]);
    // rows that change status move in and out of the index
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE status = 'open' AND cust = 1"), "0");
    exec(&mut c, "UPDATE ord SET status = 'open' WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE status = 'open' AND cust = 1"), "1");
    assert_eq!(scalar(&mut c, "SELECT id FROM ord WHERE status = 'open' AND cust = 1"), "1");
    exec(&mut c, "UPDATE ord SET status = 'done' WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE status = 'open' AND cust = 1"), "0");
}

#[test]
fn a_filtered_unique_index_constrains_only_its_rows() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE acct (id INT PRIMARY KEY, email VARCHAR(30) NULL, active BIT NOT NULL);");
    exec(&mut c, "CREATE UNIQUE INDEX ux_email ON acct (email) WHERE email IS NOT NULL AND active = 1");
    exec(&mut c, "INSERT acct VALUES (1, 'a@x', 1), (2, NULL, 1), (3, NULL, 1), (4, 'a@x', 0), (5, 'a@x', 0)");
    assert_eq!(err(&mut c, "INSERT acct VALUES (6, 'a@x', 1)").number, 2601, "a second active row with the address");
    exec(&mut c, "INSERT acct VALUES (7, 'b@x', 1)");
    // deactivating the owner frees the address; reactivating a duplicate is refused
    exec(&mut c, "UPDATE acct SET active = 0 WHERE id = 1");
    exec(&mut c, "UPDATE acct SET active = 1 WHERE id = 4");
    assert_eq!(err(&mut c, "UPDATE acct SET active = 1 WHERE id = 5").number, 2601);
    // building a unique filtered index over existing data ignores rows outside the filter
    script(&mut c, "CREATE TABLE t2 (v INT, flag INT); INSERT t2 VALUES (1, 1), (1, 0), (1, 0), (2, 1);");
    exec(&mut c, "CREATE UNIQUE INDEX u2 ON t2 (v) WHERE flag = 1");
    assert_eq!(err(&mut c, "CREATE UNIQUE INDEX u3 ON t2 (v)").number, 1505);
}

#[test]
fn filters_are_validated_and_listed_in_the_catalog() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(err(&mut c, "CREATE INDEX bad1 ON ord (cust) WHERE amount + 1 > 5").number, 10735);
    assert_eq!(err(&mut c, "CREATE INDEX bad2 ON ord (cust) WHERE status = 'a' OR status = 'b'").number, 10735);
    assert_eq!(err(&mut c, "CREATE INDEX bad3 ON ord (cust) WHERE nosuch = 1").number, 10735);
    exec(&mut c, "CREATE INDEX ix_f ON ord (cust) INCLUDE (status, amount) WHERE amount >= 100 AND note IS NOT NULL AND status IN ('open', 'done')");
    let r = rows(&mut c, "SELECT has_filter, filter_definition FROM sys.indexes WHERE name = 'ix_f'");
    assert_eq!(r[0][0], "1");
    assert!(r[0][1].contains("amount >= 100"), "{:?}", r);
    let cols = rows(&mut c, "SELECT ic.is_included_column, c.name FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id WHERE i.name = 'ix_f' ORDER BY ic.is_included_column, c.name");
    assert_eq!(cols, [["0", "cust"], ["1", "amount"], ["1", "status"]]);
    assert_eq!(scalar(&mut c, "SELECT has_filter FROM sys.indexes WHERE name LIKE 'PK%'"), "0");
}

#[test]
fn range_filters_are_implied_by_stronger_conditions() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "CREATE INDEX ix_big ON ord (cust) WHERE amount > 500");
    let used = |c: &tsql_server::Connection| c.last_plan().join(" ").contains("ix_big");
    let n = scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE cust = 3 AND amount > 600");
    assert!(used(&c), "amount > 600 implies amount > 500: {:?}", c.last_plan());
    assert_eq!(n, scalar(&mut c, "SELECT COUNT(*) FROM ord WITH (FORCESCAN) WHERE cust = 3 AND amount > 600"));
    scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE cust = 3 AND amount >= 500");
    assert!(!used(&c), "amount >= 500 does not imply amount > 500");
    scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE cust = 3 AND amount = 900");
    assert!(used(&c), "{:?}", c.last_plan());
    scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE cust = 3 AND amount < 900");
    assert!(!used(&c));
}

#[test]
fn foreign_keys_and_joins_never_depend_on_a_filtered_index() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE p (id INT PRIMARY KEY, code INT NOT NULL, live BIT NOT NULL);
         CREATE UNIQUE INDEX uq_live ON p (code) WHERE live = 1;
         CREATE TABLE ch (id INT PRIMARY KEY, pid INT REFERENCES p(id));
         CREATE INDEX ix_ch_live ON ch (pid) WHERE id > 1000;
         INSERT p VALUES (1, 10, 0), (2, 10, 1);
         INSERT ch VALUES (1, 1), (2, 2);",
    );
    assert_eq!(err(&mut c, "INSERT ch VALUES (3, 99)").number, 547);
    assert_eq!(err(&mut c, "DELETE p WHERE id = 1").number, 547, "the child row is found even though the filtered index lacks it");
    assert_eq!(rows(&mut c, "SELECT ch.id, p.code FROM ch JOIN p ON ch.pid = p.id ORDER BY ch.id"), [["1", "10"], ["2", "10"]]);
}

#[test]
fn filtered_indexes_survive_a_restart_and_keep_their_rows_right() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Server::open(dir.path().join("d")).unwrap();
        let mut c = s.connect();
        script(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, k INT, on_ BIT); CREATE UNIQUE INDEX u ON t (k) WHERE on_ = 1; INSERT t VALUES (1, 5, 1), (2, 5, 0), (3, 6, 1);");
    }
    let s = Server::open(dir.path().join("d")).unwrap();
    let mut c = s.connect();
    assert_eq!(err(&mut c, "INSERT t VALUES (4, 5, 1)").number, 2601);
    exec(&mut c, "INSERT t VALUES (5, 5, 0)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE on_ = 1 AND k = 5"), "1");
    s.checkpoint();
    assert_eq!(err(&mut c, "UPDATE t SET on_ = 1 WHERE id = 2").number, 2601, "ids 1 and 2 would both be live with k = 5");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE on_ = 1 AND k = 5"), "1");
}

#[test]
fn schema_scripts_keep_include_columns_and_filters() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "CREATE INDEX ix_s ON ord (cust) INCLUDE (amount) WHERE status = 'open'");
    let script_text = tsql_server::tools::script_table(&c.table_info("ord").unwrap());
    assert!(script_text.contains("INCLUDE ([amount])") && script_text.contains("WHERE status = 'open'"), "{}", script_text);
}
