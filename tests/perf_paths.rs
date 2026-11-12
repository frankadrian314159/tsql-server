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

// ---------------------------------------------------------------------------------------- decision-support paths

/// Two servers with the same data: one that runs everything in memory, one with a tiny work-memory budget that
/// sends every eligible query through the streaming and spilling executors.
fn twin(setup: &str) -> (tsql_server::Connection, tsql_server::Connection) {
    let a = tsql_server::Server::new();
    let b = tsql_server::Server::new();
    b.set_work_mem(2048, 0, None);
    let (mut ca, mut cb) = (a.connect(), b.connect());
    script(&mut ca, setup);
    script(&mut cb, setup);
    std::mem::forget(a);
    std::mem::forget(b);
    (ca, cb)
}

/// An `INSERT` of `n` rows into `table`, each made from its number `n` by the expressions in `select` (a recursive
/// CTE makes the numbers).
fn fill(table: &str, n: usize, select: &str) -> String {
    format!("WITH nums(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < {n}) INSERT {table} SELECT {select} FROM nums OPTION (MAXRECURSION 0)")
}

fn sales_setup() -> String {
    format!(
        "CREATE TABLE s (id INT PRIMARY KEY, g INT, h VARCHAR(5), q INT, p DECIMAL(10,2), f FLOAT);
         CREATE INDEX ix_s_g ON s (g);
         {};
         UPDATE s SET q = NULL WHERE id % 11 = 0;",
        fill("s", 4000, "n, n % 7, CHAR(65 + n % 3), n % 13, (n % 100) * 1.25, n * 0.5")
    )
}

#[test]
fn aggregates_folded_as_rows_arrive_agree_with_the_ones_worked_out_from_kept_rows() {
    let (mut mem, mut spill) = twin(&sales_setup());
    for sql in [
        "SELECT g, h, COUNT(*), COUNT(q), SUM(q), AVG(q), MIN(q), MAX(q), SUM(p), AVG(p), MIN(h), MAX(h), AVG(f) FROM s GROUP BY g, h ORDER BY g, h",
        "SELECT g, SUM(p) / COUNT(*) AS m, SUM(q) * 2 + 1 FROM s GROUP BY g HAVING COUNT(*) > 100 AND MAX(q) >= 10 ORDER BY MAX(p) DESC, g",
        "SELECT h, COUNT_BIG(*), SUM(CAST(q AS BIGINT)), AVG(CAST(p AS FLOAT)) FROM s WHERE id > 100 GROUP BY h ORDER BY h",
        "SELECT g FROM s GROUP BY g HAVING SUM(q) IS NULL OR MIN(p) = 0 ORDER BY g",
        "SELECT TOP 3 g, SUM(p) AS tot FROM s GROUP BY g ORDER BY tot DESC",
        "SELECT g, COUNT(DISTINCT q), SUM(q) FROM s GROUP BY g ORDER BY g",
        "SELECT g, STDEV(q), STRING_AGG(h, ',') FROM s WHERE id < 50 GROUP BY g ORDER BY g",
    ] {
        assert_eq!(rows(&mut mem, sql), rows(&mut spill, sql), "{sql}");
    }
    // with a tiny budget the groups are sorted and spilled; with room for them they are folded in a hash table
    rows(&mut spill, "SELECT g, SUM(p) FROM s GROUP BY g");
    assert!(spill.last_plan().iter().any(|l| l.contains("group by sort")), "{:?}", spill.last_plan());
    let roomy = tsql_server::Server::new();
    roomy.set_work_mem(256 << 20, 0, None);
    let mut r = roomy.connect();
    script(&mut r, &sales_setup());
    assert_eq!(rows(&mut r, "SELECT g, SUM(p), AVG(q) FROM s GROUP BY g ORDER BY g"), rows(&mut mem, "SELECT g, SUM(p), AVG(q) FROM s GROUP BY g ORDER BY g"));
    assert!(r.last_plan().iter().any(|l| l.contains("group by hash")), "{:?}", r.last_plan());
}

#[test]
fn a_group_that_having_throws_away_never_raises_its_aggregate_error() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE t (g INT, v INT); INSERT t VALUES (1, 2147483647), (1, 2147483647), (2, 5), (2, 6);");
    // the sum of group 1 overflows; HAVING keeps only group 2, so the overflow is never asked for
    assert_eq!(rows(&mut c, "SELECT g, SUM(v) FROM t GROUP BY g HAVING MIN(v) < 100"), vec![vec!["2".to_string(), "11".to_string()]]);
    assert_eq!(err_no(&mut c, "SELECT g, SUM(v) FROM t GROUP BY g"), 8115);
}

#[test]
fn a_cte_read_many_times_gives_the_same_rows_each_time() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE t (g INT, v INT); INSERT t VALUES (1,1),(1,2),(2,3),(2,4),(3,5);");
    // a correlated subquery over a CTE: the CTE is worked out once for the statement
    assert_eq!(
        rows(&mut c, "WITH tot AS (SELECT g, SUM(v) AS s FROM t GROUP BY g) SELECT a.g FROM tot a WHERE a.s > (SELECT AVG(b.s) FROM tot b WHERE b.g <= a.g) ORDER BY 1"),
        vec![vec!["2".to_string()]]
    );
    // one that cannot be repeated is worked out every time
    assert_eq!(scalar(&mut c, "WITH r AS (SELECT NEWID() AS x) SELECT COUNT(DISTINCT x) FROM (SELECT x FROM r UNION ALL SELECT x FROM r) u"), "2");
}

#[test]
fn a_range_that_takes_in_most_of_a_table_is_read_in_full() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE r (id INT PRIMARY KEY, d INT, pad VARCHAR(20)); CREATE INDEX ix_r_d ON r (d);");
    exec(&mut c, &fill("r", 6000, "n, n, 'x'"));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM r WHERE d <= 5900"), "5900");
    assert!(c.last_plan().iter().any(|l| l.contains("full scan")), "{:?}", c.last_plan());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM r WHERE d <= 300"), "300");
    assert!(c.last_plan().iter().any(|l| l.contains("index range")), "{:?}", c.last_plan());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM r WHERE d BETWEEN 100 AND 400"), "301");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM r WHERE d > 5990"), "10");
}

#[test]
fn correlated_subqueries_in_a_join_see_each_outer_row() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE cust (id INT PRIMARY KEY, demo INT);
         CREATE TABLE demo (id INT PRIMARY KEY, kind VARCHAR(5));
         CREATE TABLE sale (id INT PRIMARY KEY, cust INT, day INT);
         CREATE TABLE cal (day INT PRIMARY KEY, yr INT, mo INT);
         CREATE INDEX ix_sale_cust ON sale (cust);
         CREATE INDEX ix_sale_day ON sale (day);",
    );
    exec(&mut c, &fill("demo", 40, "n, CHAR(65 + n % 4)"));
    exec(&mut c, &fill("cust", 200, "n, n % 40 + 1"));
    exec(&mut c, &fill("cal", 480, "n, 2000 + n / 12 % 4, n % 12 + 1"));
    // sales of every seventh customer, on days that depend on the customer
    exec(&mut c, &fill("sale", 28, "n, n * 7, (n * 13) % 480 + 1"));
    let exists = "EXISTS (SELECT * FROM sale s, cal d WHERE c.id = s.cust AND s.day = d.day AND d.yr = 2001 AND d.mo BETWEEN 4 AND 6)";
    let by_join = rows(&mut c, "SELECT DISTINCT s.cust FROM sale s JOIN cal d ON s.day = d.day WHERE d.yr = 2001 AND d.mo BETWEEN 4 AND 6 ORDER BY 1");
    assert!(!by_join.is_empty());
    // alone, joined to another table, and counted
    assert_eq!(rows(&mut c, &format!("SELECT c.id FROM cust c WHERE {exists} ORDER BY 1")), by_join);
    assert_eq!(rows(&mut c, &format!("SELECT c.id FROM cust c, demo m WHERE m.id = c.demo AND {exists} ORDER BY 1")), by_join);
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM cust c, demo m WHERE m.id = c.demo AND {exists}")), by_join.len().to_string());
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM cust c WHERE NOT {exists}")), (200 - by_join.len()).to_string());
    // a scalar subquery on a column with few values is worked out once per value, and gives what it gives per row
    assert_eq!(
        scalar(&mut c, "SELECT COUNT(*) FROM cust c WHERE c.demo > (SELECT AVG(m.id) FROM demo m WHERE m.kind = (SELECT kind FROM demo WHERE id = c.demo))"),
        scalar(&mut c, "SELECT COUNT(*) FROM cust c, demo m0 WHERE m0.id = c.demo AND c.demo > (SELECT AVG(m.id) FROM demo m WHERE m.kind = m0.kind)")
    );
}

#[test]
fn a_subquery_that_reads_a_variable_uses_its_value() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE v (id INT PRIMARY KEY, k INT); INSERT v VALUES (1,1),(2,1),(3,1),(4,1);");
    assert_eq!(scalar(&mut c, "DECLARE @n INT = 2; SELECT COUNT(*) FROM v WHERE (SELECT COUNT(*) FROM v w WHERE w.k = v.k AND w.id <= @n) = 2"), "4");
    assert_eq!(scalar(&mut c, "DECLARE @n INT = 3; SELECT COUNT(*) FROM v WHERE (SELECT COUNT(*) FROM v w WHERE w.k = v.k AND w.id <= @n) = 2"), "0");
}
