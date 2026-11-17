//! What the optimizer does to a query without changing what it gives: correlated subqueries rewritten as joins, scalar subqueries
//! that stand alone taken as constants, `IN (subquery)` tested with a hash, and row estimates taken from column statistics.
//! Every query is run with the optimizer on and off and must give the same rows.
mod common;
use common::*;

fn data() -> tsql_server::Connection {
    let mut c = conn();
    script(
        &mut c,
        "
CREATE TABLE dept (id INT PRIMARY KEY, name VARCHAR(20), region INT NULL)
GO
CREATE TABLE emp (id INT PRIMARY KEY, dept_id INT NULL, sal INT NULL, grade VARCHAR(5) NULL, boss INT NULL)
GO
INSERT dept VALUES (1,'a',1),(2,'b',1),(3,'c',2),(4,'d',NULL),(5,'e',2)
GO
INSERT emp VALUES (1,1,100,'x',NULL),(2,1,200,'x',1),(3,1,NULL,'y',1),(4,2,50,'y',NULL),(5,2,50,NULL,4),(6,3,300,'x',NULL),(7,NULL,10,'z',NULL),(8,9,70,'x',NULL),(9,3,300,'y',6),(10,1,200,'x',2)
GO
",
    );
    c
}

fn sorted(c: &mut tsql_server::Connection, sql: &str) -> Vec<Vec<String>> {
    let mut r = rows(c, sql);
    r.sort();
    r
}

/// The same answer with the optimizer on and off; returns it.
fn same(c: &mut tsql_server::Connection, sql: &str) -> Vec<Vec<String>> {
    exec(c, "SET OPTIMIZER ON");
    let on = sorted(c, sql);
    exec(c, "SET OPTIMIZER OFF");
    let off = sorted(c, sql);
    exec(c, "SET OPTIMIZER ON");
    assert_eq!(on, off, "{sql}");
    on
}

#[test]
fn correlated_subqueries_give_the_same_rows_as_before() {
    let mut c = data();
    // scalar aggregates compared with a column
    let r = same(&mut c, "SELECT id FROM emp e WHERE sal > (SELECT AVG(sal) FROM emp x WHERE x.dept_id = e.dept_id)");
    // (department 1 averages 166 over 100, 200 and 200, the NULL being ignored: the two 200s are above it)
    assert_eq!(r, vec![vec!["10".to_string()], vec!["2".to_string()]]);
    same(&mut c, "SELECT id FROM emp e WHERE sal = (SELECT MAX(sal) FROM emp x WHERE x.dept_id = e.dept_id)");
    same(&mut c, "SELECT id FROM emp e WHERE sal * 2 < (SELECT MIN(sal) FROM emp x WHERE x.dept_id = e.dept_id AND x.grade = 'x')");
    // a subquery with nothing to give is NULL, and a comparison with NULL keeps no row
    same(&mut c, "SELECT id FROM emp e WHERE sal >= (SELECT MIN(sal) FROM emp x WHERE x.dept_id = e.dept_id AND x.id > 100)");
    // EXISTS and NOT EXISTS, with NULL keys on both sides
    same(&mut c, "SELECT id FROM emp e WHERE EXISTS (SELECT * FROM dept d WHERE d.id = e.dept_id)");
    same(&mut c, "SELECT id FROM emp e WHERE NOT EXISTS (SELECT * FROM dept d WHERE d.id = e.dept_id)");
    same(&mut c, "SELECT id FROM emp e WHERE NOT EXISTS (SELECT * FROM emp b WHERE b.id = e.boss AND b.sal > 100)");
    same(&mut c, "SELECT name FROM dept d WHERE EXISTS (SELECT * FROM emp e WHERE e.dept_id = d.id AND e.sal IS NOT NULL) AND d.region = 1");
    // other conditions around the subquery, a join in the outer query, and a column unqualified in the subquery
    same(&mut c, "SELECT e.id, d.name FROM emp e, dept d WHERE e.dept_id = d.id AND e.sal > (SELECT AVG(sal) FROM emp x WHERE x.dept_id = e.dept_id) AND d.region = 1");
    same(&mut c, "SELECT id FROM emp e WHERE EXISTS (SELECT 1 FROM emp x WHERE x.boss = e.id AND sal > 60)");
    // SELECT * keeps its columns
    assert_eq!(same(&mut c, "SELECT * FROM dept d WHERE EXISTS (SELECT * FROM emp e WHERE e.dept_id = d.id)").len(), 3);
}

#[test]
fn a_subquery_that_stands_alone_is_a_constant_and_in_subqueries_are_hashed() {
    let mut c = data();
    same(&mut c, "SELECT id FROM emp WHERE dept_id = (SELECT MAX(id) FROM dept WHERE region = 1)");
    same(&mut c, "SELECT id FROM emp WHERE sal > (SELECT AVG(sal) FROM emp)");
    same(&mut c, "SELECT id FROM emp WHERE sal < (SELECT MAX(sal) FROM emp WHERE id > 100)");
    // a subquery of more than one row is still the error it was
    for opt in ["ON", "OFF"] {
        exec(&mut c, &format!("SET OPTIMIZER {opt}"));
        let e = c.run("SELECT id FROM emp WHERE sal = (SELECT sal FROM emp WHERE dept_id = 1)").unwrap_err();
        assert_eq!(e.number, 512);
    }
    exec(&mut c, "SET OPTIMIZER ON");
    // IN (subquery) over many integers, with and without NULLs among them
    exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, k INT NULL)");
    exec(&mut c, "DECLARE @i INT = 1; WHILE @i <= 300 BEGIN INSERT big VALUES (@i, CASE WHEN @i % 50 = 0 THEN NULL ELSE @i % 120 END); SET @i += 1 END");
    for q in [
        "SELECT COUNT(*) FROM emp WHERE sal IN (SELECT k FROM big)",
        "SELECT COUNT(*) FROM emp WHERE sal NOT IN (SELECT k FROM big)",
        "SELECT COUNT(*) FROM emp WHERE sal NOT IN (SELECT k FROM big WHERE k IS NOT NULL)",
        "SELECT COUNT(*) FROM emp WHERE id IN (SELECT k FROM big WHERE k < 5) OR sal IN (SELECT k FROM big WHERE k > 100)",
    ] {
        same(&mut c, q);
    }
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp WHERE id IN (SELECT k FROM big WHERE k IS NOT NULL)"), "10");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM emp WHERE id NOT IN (SELECT k FROM big)"), "0", "a NULL among the values: NOT IN is never true");
}

#[test]
fn estimates_come_from_the_statistics_of_the_columns() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE s (id INT PRIMARY KEY, g INT NOT NULL, d DATE NOT NULL, tag VARCHAR(10) NOT NULL)");
    // 6000 rows: g takes 100 values (60 rows each), d is spread over 300 days, tag is 'rare' for 1% of the rows
    exec(&mut c, "DECLARE @i INT = 1; WHILE @i <= 6000 BEGIN INSERT s VALUES (@i, @i % 100, DATEADD(day, @i % 300, '2020-01-01'), CASE WHEN @i % 100 = 0 THEN 'rare' ELSE 'common' END); SET @i += 1 END");
    let est = |c: &mut tsql_server::Connection, sql: &str| -> f64 {
        let plan = c.explain(sql).unwrap().join("\n");
        let at = plan.find("est ").unwrap_or_else(|| panic!("no estimate in {plan}"));
        plan[at + 4..].split_whitespace().next().unwrap().parse().unwrap()
    };
    let eq = est(&mut c, "SELECT * FROM s WHERE g = 7");
    assert!((30.0..=120.0).contains(&eq), "g = 7 keeps about 60 of 6000 rows, not a third: {eq}");
    let rare = est(&mut c, "SELECT * FROM s WHERE tag = 'rare'");
    assert!((30.0..=120.0).contains(&rare), "a rare value is found among the most common ones: {rare}");
    let common = est(&mut c, "SELECT * FROM s WHERE tag = 'common'");
    assert!(common > 5000.0, "{common}");
    let range = est(&mut c, "SELECT * FROM s WHERE d < '2020-02-01'");
    assert!((300.0..=1000.0).contains(&range), "a month of ten: about 620 rows: {range}");
    let between = est(&mut c, "SELECT * FROM s WHERE g BETWEEN 10 AND 19");
    assert!((300.0..=1000.0).contains(&between), "{between}");
    let both = est(&mut c, "SELECT * FROM s WHERE g = 7 AND d < '2020-02-01'");
    assert!(both < eq, "a second condition keeps fewer rows: {both} against {eq}");
    // and the answers are the answers
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM s WHERE g = 7"), "60");
}

fn est_of(c: &mut tsql_server::Connection, sql: &str) -> f64 {
    let plan = c.explain(sql).unwrap().join("\n");
    let at = plan.find("est ").unwrap_or_else(|| panic!("no estimate in {plan}"));
    plan[at + 4..].split_whitespace().next().unwrap().parse().unwrap()
}

/// 6000 rows: `g` (indexed) takes 10 values, 600 rows each; `flag` (not indexed) is 1 for 1% of the rows, independently of `g`.
fn flagged(c: &mut tsql_server::Connection) {
    exec(c, "CREATE TABLE r (id INT PRIMARY KEY, g INT NOT NULL, flag INT NOT NULL); CREATE INDEX ix_r_g ON r (g)");
    exec(c, "DECLARE @i INT = 1; WHILE @i <= 6000 BEGIN INSERT r VALUES (@i, @i % 10, CASE WHEN (@i / 10) % 100 = 0 THEN 1 ELSE 0 END); SET @i += 1 END");
}

#[test]
fn conditions_an_index_seek_does_not_answer_still_count_in_its_estimate() {
    let mut c = conn();
    flagged(&mut c);
    let seek = est_of(&mut c, "SELECT * FROM r WHERE g = 3");
    assert!((400.0..=800.0).contains(&seek), "the index alone: about 600 rows: {seek}");
    let p = c.explain("SELECT * FROM r WHERE g = 3 AND flag = 1").unwrap().join("\n");
    assert!(p.contains("index seek via ix_r_g"), "{p}");
    let both = est_of(&mut c, "SELECT * FROM r WHERE g = 3 AND flag = 1");
    assert!(both < seek / 4.0, "the unindexed condition keeps about 1% of what the seek finds: {both} against {seek}");
    // a second condition on the column the seek is on is not counted again
    let same_col = est_of(&mut c, "SELECT * FROM r WHERE g = 3 AND g <> 4");
    assert_eq!(same_col, seek, "a condition on the sought column");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM r WHERE g = 3 AND flag = 1"), "6");
}

#[test]
fn statistics_are_taken_again_after_updates_that_keep_the_row_count() {
    let mut c = conn();
    flagged(&mut c);
    let before = est_of(&mut c, "SELECT * FROM r WHERE flag = 1");
    assert!(before < 200.0, "1% of the rows: {before}");
    // half the rows change value, none are added or removed
    exec(&mut c, "UPDATE r SET flag = 1 WHERE g < 5");
    let after = est_of(&mut c, "SELECT * FROM r WHERE flag = 1");
    assert!(after > 1500.0, "about 3000 rows now, from a new sample: {after}");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM r WHERE flag = 1"), "3030");
}

#[test]
fn a_session_temporary_table_is_read_through_its_index() {
    let mut c = data();
    exec(&mut c, "SELECT * INTO #e FROM emp");
    exec(&mut c, "CREATE INDEX ix_e ON #e (dept_id)");
    // an equality on the indexed column seeks, for a constant and for each row of an outer query
    let plan = |c: &mut tsql_server::Connection, sql: &str| {
        exec(c, sql);
        c.last_plan().to_vec()
    };
    let p = plan(&mut c, "SELECT id FROM #e WHERE dept_id = 1");
    assert!(p.iter().any(|l| l.contains("index seek via ix_e")), "{p:?}");
    let sql = "SELECT d.id, ISNULL((SELECT SUM(e.sal) FROM #e e WHERE e.dept_id = d.id), -1) FROM dept d";
    let p = plan(&mut c, sql);
    assert!(p.iter().any(|l| l.contains("index seek via ix_e")), "{p:?}");
    // the same rows as the same query over the table, and as with the index out of the way
    let over_table = sorted(&mut c, "SELECT d.id, ISNULL((SELECT SUM(e.sal) FROM emp e WHERE e.dept_id = d.id), -1) FROM dept d");
    assert_eq!(sorted(&mut c, sql), over_table);
    assert_eq!(sorted(&mut c, "SELECT id FROM #e WHERE dept_id IN (1, 3) AND sal > 100"), sorted(&mut c, "SELECT id FROM emp WHERE dept_id IN (1, 3) AND sal > 100"));
    // a table variable too: the rows of its index are found by it
    exec(&mut c, "DECLARE @v TABLE (k INT PRIMARY KEY, v INT); INSERT @v VALUES (1, 10), (2, 20), (3, 30)");
    assert_eq!(rows(&mut c, "DECLARE @v TABLE (k INT PRIMARY KEY, v INT); INSERT @v VALUES (1, 10), (2, 20), (3, 30); SELECT v FROM @v WHERE k = 2"), vec![vec!["20".to_string()]]);
}
