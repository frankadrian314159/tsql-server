//! The fast paths of the engine (see docs/performance.md) against what they replace: batched row writes and commits,
//! the cached foreign-key facts, the plan text limit, the cached planning of correlated subqueries and the byte tokenizer.
mod common;
use common::*;

/// A table with an index on a column whose values repeat, filled with `n` rows.
fn filled(c: &mut tsql_server::Connection, n: usize) {
    script(c, "CREATE TABLE t (id INT PRIMARY KEY, grp INT, name VARCHAR(30), val FLOAT); CREATE INDEX ix_grp ON t (grp); CREATE UNIQUE INDEX ux_name ON t (name);");
    let mut i = 0;
    while i < n {
        let mut sql = String::from("INSERT t VALUES ");
        for k in i..(i + 500).min(n) {
            if k > i {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, 'n{}', {}.5)", k, k % 17, k, k));
        }
        exec(c, &sql);
        i += 500;
    }
}

#[test]
fn a_statement_that_writes_many_rows_leaves_every_index_right() {
    let mut c = conn();
    filled(&mut c, 3000);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "3000");
    // every index agrees with the table: seeks through each of them find the rows a scan finds
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 5"), scalar(&mut c, "SELECT COUNT(*) FROM t WITH (INDEX(0)) WHERE grp + 0 = 5"));
    assert_eq!(scalar(&mut c, "SELECT id FROM t WHERE name = 'n1234'"), "1234");
    assert!(c.last_plan().join("\n").contains("index seek"), "{:?}", c.last_plan());
    // update the indexed column of every row: the old index entries go, the new ones come
    exec(&mut c, "UPDATE t SET grp = grp + 100, name = 'x' + name");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 5"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 105"), scalar(&mut c, "SELECT COUNT(*) FROM t WITH (INDEX(0)) WHERE grp + 0 = 105"));
    assert_eq!(scalar(&mut c, "SELECT id FROM t WHERE name = 'xn77'"), "77");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE name = 'n77'"), "0");
    // a delete of a quarter of them, then of the rest by key range
    exec(&mut c, "DELETE FROM t WHERE id % 4 = 0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "2250");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE name = 'xn8'"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE name = 'xn9'"), "1");
    exec(&mut c, "DELETE FROM t WHERE id >= 1000");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "750");
    // a duplicate in the unique index is refused and changes nothing
    assert_eq!(err_no(&mut c, "UPDATE t SET name = 'xn1' WHERE id = 3"), 2601);
    assert_eq!(scalar(&mut c, "SELECT name FROM t WHERE id = 3"), "xn3");
}

#[test]
fn rolling_back_a_big_statement_restores_the_rows_and_indexes() {
    let mut c = conn();
    filled(&mut c, 1500);
    exec(&mut c, "BEGIN TRAN");
    exec(&mut c, "UPDATE t SET grp = 999, name = 'gone' + name");
    exec(&mut c, "DELETE FROM t WHERE id > 1000");
    exec(&mut c, "INSERT t VALUES (5000, 1, 'extra', 1)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 999"), "1001");
    exec(&mut c, "ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "1500");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 999"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE name = 'n1400'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE name = 'extra'"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 3"), scalar(&mut c, "SELECT COUNT(*) FROM t WITH (INDEX(0)) WHERE grp + 0 = 3"));
}

#[test]
fn many_updates_of_one_row_in_a_transaction_commit_one_version() {
    let mut c = conn();
    filled(&mut c, 100);
    exec(&mut c, "BEGIN TRAN");
    for _ in 0..5 {
        exec(&mut c, "UPDATE t SET grp = grp + 1 WHERE id < 50");
    }
    exec(&mut c, "COMMIT");
    assert_eq!(scalar(&mut c, "SELECT grp FROM t WHERE id = 3"), "8");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 8"), scalar(&mut c, "SELECT COUNT(*) FROM t WITH (INDEX(0)) WHERE grp + 0 = 8"));
    // the old versions are collected: nothing else is reading, so each row holds one
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = 3"), scalar(&mut c, "SELECT COUNT(*) FROM t WITH (INDEX(0)) WHERE grp + 0 = 3"));
}

#[test]
fn foreign_key_facts_follow_the_catalog() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE p (id INT PRIMARY KEY); CREATE TABLE ch (id INT PRIMARY KEY, pid INT);");
    exec(&mut c, "INSERT p VALUES (1), (2)");
    exec(&mut c, "INSERT ch VALUES (10, 1)");
    // these statements learn that nothing refers to p
    exec(&mut c, "DELETE FROM p WHERE id = 2");
    exec(&mut c, "INSERT p VALUES (2)");
    // a foreign key added afterwards is enforced at once
    exec(&mut c, "ALTER TABLE ch ADD CONSTRAINT fk_ch FOREIGN KEY (pid) REFERENCES p (id)");
    assert_eq!(err_no(&mut c, "DELETE FROM p WHERE id = 1"), 547);
    assert_eq!(err_no(&mut c, "INSERT ch VALUES (11, 99)"), 547);
    // and one that is dropped no longer is
    exec(&mut c, "ALTER TABLE ch DROP CONSTRAINT fk_ch");
    exec(&mut c, "DELETE FROM p WHERE id = 1");
    exec(&mut c, "INSERT ch VALUES (11, 99)");
    // a cascading key changes what a delete of the parent does
    exec(&mut c, "DELETE FROM ch");
    exec(&mut c, "INSERT p VALUES (1)");
    exec(&mut c, "INSERT ch VALUES (10, 1), (11, 1), (12, 2)");
    exec(&mut c, "ALTER TABLE ch ADD CONSTRAINT fk_ch2 FOREIGN KEY (pid) REFERENCES p (id) ON DELETE CASCADE");
    exec(&mut c, "DELETE FROM p WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ch"), "1");
    // dropping the table that refers to p lets p be truncated again
    assert_eq!(err_no(&mut c, "TRUNCATE TABLE p"), 4712);
    exec(&mut c, "DROP TABLE ch");
    exec(&mut c, "TRUNCATE TABLE p");
}

#[test]
fn a_table_created_in_another_database_is_seen_by_the_cached_facts() {
    let mut c = conn();
    exec(&mut c, "CREATE DATABASE fk1");
    exec(&mut c, "USE fk1");
    script(&mut c, "CREATE TABLE p (id INT PRIMARY KEY); INSERT p VALUES (1);");
    exec(&mut c, "DELETE FROM p WHERE id = 1");
    exec(&mut c, "INSERT p VALUES (1)");
    exec(&mut c, "CREATE TABLE ch (id INT PRIMARY KEY, pid INT REFERENCES p (id))");
    exec(&mut c, "INSERT ch VALUES (1, 1)");
    assert_eq!(err_no(&mut c, "DELETE FROM p WHERE id = 1"), 547);
    exec(&mut c, "DROP TABLE ch");
    exec(&mut c, "DELETE FROM p WHERE id = 1");
}

#[test]
fn the_plan_text_of_a_statement_stays_short() {
    let mut c = conn();
    filled(&mut c, 2000);
    script(&mut c, "CREATE TABLE d (id INT PRIMARY KEY, label VARCHAR(10)); INSERT d SELECT DISTINCT grp, 'l' FROM t;");
    // the subquery runs once per row of t: a line of plan for each would be thousands of lines
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp)"), "2000");
    let plan = c.last_plan();
    assert!(plan.len() <= tsql_server::engine::PLAN_LINES, "{} lines", plan.len());
    // the plan of an ordinary statement is whole
    scalar(&mut c, "SELECT name FROM t WHERE id = 7");
    assert!(c.last_plan().iter().any(|l| l.contains("index seek")), "{:?}", c.last_plan());
}

#[test]
fn correlated_subqueries_give_the_same_answers_however_the_outer_value_is_used() {
    let mut c = conn();
    filled(&mut c, 600);
    script(&mut c, "CREATE TABLE d (id INT PRIMARY KEY, label VARCHAR(10), lim INT); INSERT d SELECT g, 'l' + CAST(g AS VARCHAR(5)), g * 30 FROM (SELECT DISTINCT grp AS g FROM t) x;");
    let ref_count = |c: &mut tsql_server::Connection, sql: &str| scalar(c, sql);
    // the outer column on the right and on the left, with equality and with ranges
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp)"), "600");
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE t.grp = d.id)"), "600");
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.lim > t.id)"), ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE id < 480"));
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE t.id >= d.lim)"), ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE id >= 0"));
    // a condition on the subquery's own table beside the correlated one, and one that mentions both sides
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp AND d.lim >= 150)"), ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE grp >= 5"));
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp AND d.lim + t.id > 400)"), ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE grp * 30 + id > 400"));
    // a scalar subquery, parenthesised outer columns, and an outer column inside a function
    assert_eq!(scalar(&mut c, "SELECT (SELECT label FROM d WHERE d.id = (t.grp)) FROM t WHERE id = 20"), "l3");
    assert_eq!(scalar(&mut c, "SELECT (SELECT label FROM d WHERE d.id = ABS(t.grp)) FROM t WHERE id = 20"), "l3");
    assert_eq!(scalar(&mut c, "SELECT (SELECT MAX(lim) FROM d WHERE d.id <= t.grp) FROM t WHERE id = 20"), "90");
    // the same subquery text twice in one statement, and nested subqueries
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp) AND EXISTS (SELECT 1 FROM d WHERE d.id = t.grp AND d.lim > 0)"), ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE grp > 0"));
    assert_eq!(
        ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp AND EXISTS (SELECT 1 FROM t t2 WHERE t2.grp = d.id AND t2.id = t.id))"),
        "600"
    );
    // an outer value that is NULL matches nothing
    exec(&mut c, "UPDATE t SET grp = NULL WHERE id < 10");
    assert_eq!(ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp)"), "590");
    // an outer join: the condition on the null-supplying side must not be applied early
    assert_eq!(
        ref_count(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM d LEFT JOIN d d2 ON d2.id = d.id + 1000 WHERE d.id = t.grp AND d2.id IS NULL)"),
        "590"
    );
}

#[test]
fn the_byte_tokenizer_reads_unicode_names_and_literals() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE [caf\u{e9}] (\u{4f60}\u{597d} INT, [a b] NVARCHAR(20)); INSERT [caf\u{e9}] VALUES (1, N'na\u{ef}ve \u{1f600}');");
    assert_eq!(scalar(&mut c, "SELECT [a b] FROM [caf\u{e9}] WHERE \u{4f60}\u{597d} = 1"), "na\u{ef}ve \u{1f600}");
    assert_eq!(scalar(&mut c, "SELECT N'it''s' + N'\u{e9}'"), "it's\u{e9}");
    assert_eq!(scalar(&mut c, "SELECT 1 /* c\u{e9} /* nested */ */ + 2 -- tail \u{1f600}"), "3");
    assert_eq!(err_no(&mut c, "SELECT 'unclosed"), 105);
    assert_eq!(err_no(&mut c, "SELECT 1 /* open"), 113);
}
