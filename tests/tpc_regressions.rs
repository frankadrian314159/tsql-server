//! Problems found by running the TPC-H and TPC-DS queries (see `examples/tpc`), each pinned by a small test:
//! uncorrelated subqueries were re-run for every outer row, the common part of an OR of ANDs hid join conditions,
//! a derived table could not start with a parenthesised query, filters on a CTE reference were applied after the
//! join, and the join order put a many-to-many join ahead of a key join.
mod common;
use common::*;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE t (id INT PRIMARY KEY, grp INT, v INT);
         CREATE TABLE u (id INT PRIMARY KEY, w INT);
         INSERT u VALUES (1, 10), (2, 20), (3, 30), (4, NULL);
         DECLARE @i INT = 1;
         BEGIN TRAN;
         WHILE @i <= 200 BEGIN INSERT t VALUES (@i, @i % 5, @i); SET @i = @i + 1; END;
         COMMIT;",
    );
}

#[test]
fn uncorrelated_subqueries_give_the_same_answers_as_when_they_ran_per_row() {
    let mut c = conn();
    setup(&mut c);
    // scalar, IN, NOT IN (with a NULL in the subquery), EXISTS, ANY/ALL
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE v > (SELECT AVG(v) FROM t)"), "100");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp IN (SELECT id FROM u WHERE id < 3)"), "80");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp NOT IN (SELECT w FROM u)"), "0", "NOT IN over a set holding NULL is never true");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp NOT IN (SELECT id FROM u WHERE id < 3)"), "120");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE w > 25)"), "200");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE w > 99)"), "200");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp < ALL (SELECT id FROM u WHERE id > 3)"), "160");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE grp = ANY (SELECT id FROM u)"), "160");
}

#[test]
fn correlated_subqueries_still_run_once_per_outer_row() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE v > (SELECT AVG(v) FROM t t2 WHERE t2.grp = t.grp)"), "100");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.id = t.grp)"), "160");
    // a correlated reference that is only reached on some inner rows must not make the subquery look uncorrelated
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.id = 2 AND u.w = t.grp * 10)"), "40");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE (SELECT MAX(w) FROM u WHERE u.id <= t.grp) = 20"), "40");
    // an unknown column is still an error, not a silent "uncorrelated"
    assert_eq!(err_no(&mut c, "SELECT COUNT(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.nope = t.grp)"), 207);
}

#[test]
fn a_cached_subquery_is_not_reused_across_statements_loop_iterations_or_conditions() {
    let mut c = conn();
    setup(&mut c);
    // the same statement text in a loop, with a variable that changes the subquery's result each time
    script(
        &mut c,
        "CREATE TABLE r (i INT, n INT);
         DECLARE @i INT = 1;
         WHILE @i <= 3 BEGIN
             INSERT r SELECT @i, COUNT(*) FROM t WHERE grp IN (SELECT id FROM u WHERE id <= @i);
             SET @i = @i + 1;
         END;",
    );
    assert_eq!(rows(&mut c, "SELECT i, n FROM r ORDER BY i"), [["1", "40"], ["2", "80"], ["3", "120"]]);
    // a WHILE condition containing a subquery sees the table change from one iteration to the next
    script(&mut c, "CREATE TABLE q (x INT); INSERT q VALUES (1), (2), (3); WHILE (SELECT COUNT(*) FROM q) > 0 BEGIN DELETE TOP (1) FROM q; END;");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM q"), "0");
    // an IF condition, then a change, then the same condition again
    script(&mut c, "INSERT q VALUES (1);");
    assert_eq!(scalar(&mut c, "IF (SELECT COUNT(*) FROM q) = 1 SELECT 'one' ELSE SELECT 'other'"), "one");
    exec(&mut c, "INSERT q VALUES (2)");
    assert_eq!(scalar(&mut c, "IF (SELECT COUNT(*) FROM q) = 1 SELECT 'one' ELSE SELECT 'other'"), "other");
    // inside a function called per row, each call is its own statement
    script(&mut c, "CREATE FUNCTION dbo.cnt() RETURNS INT AS BEGIN RETURN (SELECT COUNT(*) FROM q); END;");
    assert_eq!(rows(&mut c, "SELECT dbo.cnt(), (SELECT COUNT(*) FROM q) FROM u WHERE id = 1"), [["2", "2"]]);
    // DML whose subquery depends on rows the statement itself changes: evaluated against the state at its start
    exec(&mut c, "UPDATE t SET v = v + (SELECT COUNT(*) FROM t WHERE grp = 0) WHERE grp = 1");
    assert_eq!(scalar(&mut c, "SELECT v FROM t WHERE id = 1"), "41");
}

#[test]
fn the_common_part_of_an_or_of_ands_is_a_condition_of_its_own() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, x INT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, y INT);
         INSERT a VALUES (1, 1, 5), (2, 2, 6), (3, 3, NULL), (4, NULL, 5), (5, 1, 7);
         INSERT b VALUES (1, 1, 10), (2, 2, 20), (3, 3, 30), (4, NULL, 10), (5, 1, 40);",
    );
    // (a.k = b.k AND x = 5 AND y = 10) OR (a.k = b.k AND x = 6 AND y = 20) OR (a.k = b.k AND x = 7 AND y = 40)
    let q = "SELECT a.id, b.id FROM a, b WHERE (a.k = b.k AND a.x = 5 AND b.y = 10) OR (a.k = b.k AND a.x = 6 AND b.y = 20) OR (a.k = b.k AND a.x = 7 AND b.y = 40) ORDER BY a.id, b.id";
    assert_eq!(rows(&mut c, q), [["1", "1"], ["2", "2"], ["5", "5"]]);
    // the same without anything in common (no factoring possible) gives the answer written by hand
    let q2 = "SELECT a.id, b.id FROM a, b WHERE (a.k = b.k AND a.x = 5) OR (b.k = a.k AND b.y = 20) ORDER BY a.id, b.id";
    assert_eq!(rows(&mut c, q2), [["1", "1"], ["1", "5"], ["2", "2"]]);
    // three-valued logic: with NULLs the factored form must give what the original did
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE (k = 1 AND x = 5) OR (k = 1 AND x = 7)"), "2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE NOT ((k = 1 AND x = 5) OR (k = 1 AND x = 7))"), "2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE (k = 1 AND x > 5) OR (k = 1 AND x IS NULL)"), "1");
    // an OR where one branch is exactly the common part reduces to it
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE k = 1 OR (k = 1 AND x = 7)"), "2");
    // and the plan joins the tables on the shared condition instead of forming every pair
    let plan = c.explain(q).unwrap().join("\n").to_lowercase();
    assert!(plan.contains("hash join"), "{}", plan);
}

#[test]
fn a_derived_table_can_start_with_parenthesised_queries() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ((SELECT id FROM u) EXCEPT (SELECT id FROM u WHERE id > 2)) x"), "2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ((SELECT id FROM u) EXCEPT (SELECT id FROM u WHERE id = 1) EXCEPT (SELECT id FROM u WHERE id = 2)) x"), "2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ((SELECT id FROM u WHERE id < 3) UNION ALL (SELECT id FROM u WHERE id > 2)) AS x"), "4");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM (((SELECT id FROM u))) y"), "4");
    // a parenthesised join is still a join
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM (t JOIN u ON t.grp = u.id)"), "160");
}

#[test]
fn filters_on_a_cte_reference_apply_before_the_join_and_per_reference() {
    let mut c = conn();
    setup(&mut c);
    let q = "WITH yt AS (SELECT grp k, v % 2 yr, SUM(v) tot FROM t GROUP BY grp, v % 2)
             SELECT a.k, a.tot, b.tot FROM yt a, yt b WHERE a.k = b.k AND a.yr = 0 AND b.yr = 1 AND b.tot > 0 ORDER BY a.k";
    let r = rows(&mut c, q);
    assert_eq!(r.len(), 5);
    let want: Vec<Vec<String>> = (0..5i64)
        .map(|g| {
            let even: i64 = (1..=200).filter(|v| v % 5 == g && v % 2 == 0).sum();
            let odd: i64 = (1..=200).filter(|v| v % 5 == g && v % 2 == 1).sum();
            vec![g.to_string(), even.to_string(), odd.to_string()]
        })
        .collect();
    assert_eq!(r, want);
    // a filter that mentions both references is still a join condition, not a per-leaf filter
    assert_eq!(scalar(&mut c, "WITH yt AS (SELECT grp k, SUM(v) tot FROM t GROUP BY grp) SELECT COUNT(*) FROM yt a, yt b WHERE a.tot < b.tot"), "10");
    // outer joins keep their rows: the filter is on the null-supplying side's output
    assert_eq!(scalar(&mut c, "WITH yt AS (SELECT grp k FROM t GROUP BY grp) SELECT COUNT(*) FROM u LEFT JOIN yt ON yt.k = u.id WHERE u.id > 1"), "3");
}

#[test]
fn a_key_join_is_placed_before_a_many_to_many_join() {
    // customers and suppliers in nations, orders by customer: joining customer to the rows so far through the shared
    // nation alone would multiply them, while orders joins on its key
    let s = server();
    s.set_work_mem(1 << 20, 0, None);
    let mut c = s.connect();
    script(
        &mut c,
        "CREATE TABLE li (okey INT, skey INT, amt INT);
         CREATE TABLE sup (skey INT PRIMARY KEY, nat INT);
         CREATE TABLE ord (okey INT PRIMARY KEY, ckey INT, d INT);
         CREATE TABLE cus (ckey INT PRIMARY KEY, nat INT);
         DECLARE @i INT = 1;
         BEGIN TRAN;
         WHILE @i <= 300 BEGIN
             INSERT li VALUES (@i % 60 + 1, @i % 10 + 1, @i);
             SET @i = @i + 1;
         END;
         SET @i = 1;
         WHILE @i <= 10 BEGIN INSERT sup VALUES (@i, @i % 3); SET @i = @i + 1; END;
         SET @i = 1;
         WHILE @i <= 60 BEGIN INSERT ord VALUES (@i, @i % 30 + 1, @i % 7); SET @i = @i + 1; END;
         SET @i = 1;
         WHILE @i <= 30 BEGIN INSERT cus VALUES (@i, @i % 3); SET @i = @i + 1; END;
         COMMIT;",
    );
    let q = "SELECT ord.d, COUNT(*), SUM(amt) FROM li, sup, ord, cus WHERE li.skey = sup.skey AND li.okey = ord.okey AND ord.ckey = cus.ckey AND cus.nat = sup.nat AND ord.d < 3 GROUP BY ord.d ORDER BY ord.d";
    let streamed = rows(&mut c, q);
    let plan = c.last_plan().join(" ");
    assert!(plan.contains("streaming hash join"), "{}", plan);
    let (o, cu) = (plan.find("scan ord").expect(&plan), plan.find("scan cus").expect(&plan));
    assert!(o < cu, "orders (a key join) should come before customer (a join on nation only): {}", plan);
    // the answer is the same as without streaming
    let s2 = server();
    let mut c2 = s2.connect();
    script(
        &mut c2,
        "CREATE TABLE li (okey INT, skey INT, amt INT);
         CREATE TABLE sup (skey INT PRIMARY KEY, nat INT);
         CREATE TABLE ord (okey INT PRIMARY KEY, ckey INT, d INT);
         CREATE TABLE cus (ckey INT PRIMARY KEY, nat INT);
         DECLARE @i INT = 1;
         BEGIN TRAN;
         WHILE @i <= 300 BEGIN
             INSERT li VALUES (@i % 60 + 1, @i % 10 + 1, @i);
             SET @i = @i + 1;
         END;
         SET @i = 1;
         WHILE @i <= 10 BEGIN INSERT sup VALUES (@i, @i % 3); SET @i = @i + 1; END;
         SET @i = 1;
         WHILE @i <= 60 BEGIN INSERT ord VALUES (@i, @i % 30 + 1, @i % 7); SET @i = @i + 1; END;
         SET @i = 1;
         WHILE @i <= 30 BEGIN INSERT cus VALUES (@i, @i % 3); SET @i = @i + 1; END;
         COMMIT;",
    );
    assert_eq!(rows(&mut c2, q), streamed);
}

#[test]
fn a_subquery_in_a_join_condition_can_refer_to_the_joined_table() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE x (id INT PRIMARY KEY, a INT, b INT);
         CREATE TABLE y (id INT PRIMARY KEY, a INT, b INT);
         INSERT x VALUES (1, 1, 1), (2, 2, 3), (3, 3, 3), (4, NULL, 2);
         INSERT y VALUES (1, 1, 2), (2, 3, 3), (3, 3, 1), (4, NULL, NULL);",
    );
    let q = "SELECT q1.id, q2.id FROM x q1 INNER JOIN y q2 ON q1.b = q2.a AND q1.b NOT IN (SELECT s.b FROM y s WHERE s.b = q2.a) ORDER BY q1.id, q2.id";
    let on = rows(&mut c, q);
    exec(&mut c, "SET OPTIMIZER OFF");
    let off = rows(&mut c, q);
    assert_eq!(on, off);
    let q = "SELECT q1.id, q2.id FROM x q1 LEFT JOIN y q2 ON q1.b = q2.a AND EXISTS (SELECT 1 FROM y s WHERE s.a = q2.a AND s.id <> q2.id) ORDER BY q1.id, q2.id";
    exec(&mut c, "SET OPTIMIZER ON");
    let on = rows(&mut c, q);
    exec(&mut c, "SET OPTIMIZER OFF");
    assert_eq!(rows(&mut c, q), on);
}

#[test]
fn window_aggregates_are_incremental_and_still_right() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE w (id INT PRIMARY KEY, g INT, x INT, d DECIMAL(8,2));
         INSERT w VALUES (1, 1, 10, 1.50), (2, 1, 10, NULL), (3, 1, 20, 2.25), (4, 1, NULL, 3.00), (5, 2, 5, 0.10), (6, 2, 5, 0.20), (7, 2, 7, NULL);",
    );
    // running totals (RANGE is the default frame, so peers share a value), with ties and NULLs (which sort first)
    let r = rows(&mut c, "SELECT id, SUM(x) OVER (PARTITION BY g ORDER BY x) s, COUNT(x) OVER (PARTITION BY g ORDER BY x) cx, COUNT(*) OVER (PARTITION BY g ORDER BY x) cs, MIN(x) OVER (PARTITION BY g ORDER BY x) mn, MAX(x) OVER (PARTITION BY g ORDER BY x) mx, AVG(d) OVER (PARTITION BY g ORDER BY x) ad FROM w ORDER BY id");
    let want = [
        ("1", "20", "2", "3", "10", "10", 2.25),
        ("2", "20", "2", "3", "10", "10", 2.25),
        ("3", "40", "3", "4", "10", "20", 2.25),
        ("4", "NULL", "0", "1", "NULL", "NULL", 3.0),
        ("5", "10", "2", "2", "5", "5", 0.15),
        ("6", "10", "2", "2", "5", "5", 0.15),
        ("7", "17", "3", "3", "5", "7", 0.15),
    ];
    assert_eq!(r.len(), want.len());
    for (got, w) in r.iter().zip(want.iter()) {
        assert_eq!(got[..6], [w.0, w.1, w.2, w.3, w.4, w.5].map(String::from), "{:?}", got);
        assert!((got[6].parse::<f64>().unwrap() - w.6).abs() < 1e-6, "{:?}", got);
    }
    // explicit ROWS frames: growing, sliding and the whole partition
    let r = rows(&mut c, "SELECT id, SUM(x) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), SUM(x) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), SUM(x) OVER (PARTITION BY g) FROM w ORDER BY id");
    assert_eq!(
        r,
        [["1", "10", "10", "40"], ["2", "20", "20", "40"], ["3", "40", "30", "40"], ["4", "40", "20", "40"], ["5", "5", "5", "17"], ["6", "10", "10", "17"], ["7", "17", "12", "17"]].map(|x| x.map(String::from).to_vec()).to_vec()
    );
    // a large partition: the whole-partition total and a running total must not be quadratic
    script(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, x INT); DECLARE @i INT = 1; BEGIN TRAN; WHILE @i <= 6000 BEGIN INSERT big VALUES (@i, @i % 7); SET @i = @i + 1; END; COMMIT;");
    let t = std::time::Instant::now();
    assert_eq!(rows(&mut c, "SELECT MIN(s), MAX(s), MAX(r) FROM (SELECT SUM(x) OVER () s, SUM(x) OVER (ORDER BY id) r FROM big) q"), [["17998", "17998", "17998"]]);
    assert_eq!(rows(&mut c, "SELECT MAX(c) FROM (SELECT CUME_DIST() OVER (ORDER BY 1) c FROM big) q"), [["1"]]);
    assert!(t.elapsed().as_secs() < 20, "window aggregates over one big partition took {:?}", t.elapsed());
}

#[test]
fn on_conditions_that_mention_only_the_first_table_are_not_dropped_by_the_streaming_join() {
    let s = server();
    s.set_work_mem(1 << 20, 0, None); // every join streams
    let mut c = s.connect();
    let s2 = server();
    let mut c2 = s2.connect();
    let sql = "CREATE TABLE a (id INT PRIMARY KEY, k INT, v INT);
               CREATE TABLE b (id INT PRIMARY KEY, k INT, w INT);
               CREATE TABLE d (id INT PRIMARY KEY, k INT);
               INSERT a VALUES (1, 1, 5), (2, 2, 6), (3, 3, 7), (4, 1, 8), (5, NULL, 9);
               INSERT b VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3), (4, NULL, 4);
               INSERT d VALUES (1, 1), (2, 2), (3, 9);";
    script(&mut c, sql);
    script(&mut c2, sql);
    for q in [
        // single-table conditions on the first table, inside ON
        "SELECT a.id, b.id FROM a INNER JOIN b ON a.k = b.k AND a.v > 5 ORDER BY 1, 2",
        "SELECT a.id, b.id FROM a INNER JOIN b ON a.k = b.k AND a.v > a.v ORDER BY 1, 2",
        "SELECT a.id, b.id FROM a INNER JOIN b ON a.k = b.k AND (a.v < 7 OR 4 < 1) ORDER BY 1, 2",
        "SELECT a.id, b.id, d.id FROM a INNER JOIN b ON a.k = b.k AND a.v <> 6 INNER JOIN d ON d.k = b.k AND a.v < 9 ORDER BY 1, 2, 3",
        // on a later table, and across several
        "SELECT a.id, b.id FROM a INNER JOIN b ON a.k = b.k AND b.w >= 2 ORDER BY 1, 2",
        "SELECT a.id, b.id, d.id FROM a INNER JOIN b ON a.k = b.k INNER JOIN d ON d.k = a.k AND b.w < 3 AND a.v > 5 ORDER BY 1, 2, 3",
        // a left join keeps its unmatched rows; a condition on the preserved side of an inner join still filters
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND b.w >= 2 ORDER BY 1, 2",
        "SELECT a.id, b.id, d.id FROM a INNER JOIN d ON d.k = a.k AND a.v < 8 LEFT JOIN b ON b.k = a.k ORDER BY 1, 2, 3",
        // an inner join's ON condition after an outer join belongs to the inner join, not to the outer join's stage
        "SELECT a.id, b.id, d.id FROM a FULL JOIN b ON a.k = b.k INNER JOIN d ON b.k = d.k AND 'a b' LIKE 'x' ORDER BY 1, 2, 3",
        "SELECT a.id, b.id, d.id FROM a FULL JOIN b ON a.k = b.k INNER JOIN d ON b.k = d.k AND b.w < b.w ORDER BY 1, 2, 3",
        "SELECT a.id, b.id, d.id FROM a FULL JOIN b ON a.k = b.k INNER JOIN d ON b.k = d.k AND b.w >= 2 ORDER BY 1, 2, 3",
        "SELECT a.id, b.id, d.id FROM a LEFT JOIN b ON a.k = b.k INNER JOIN d ON a.k = d.k AND a.v > 5 ORDER BY 1, 2, 3",
        "SELECT a.id, b.id, d.id FROM d RIGHT JOIN a ON a.k = d.k INNER JOIN b ON a.k = b.k AND d.id IS NULL ORDER BY 1, 2, 3",
    ] {
        let streamed = rows(&mut c, q);
        let plain = rows(&mut c2, q);
        assert_eq!(streamed, plain, "{}", q);
    }
    // and the answers are the ones expected, not just equal
    assert_eq!(rows(&mut c, "SELECT a.id, b.id FROM a INNER JOIN b ON a.k = b.k AND a.v > a.v"), Vec::<Vec<String>>::new());
    assert_eq!(rows(&mut c, "SELECT a.id, b.id FROM a INNER JOIN b ON a.k = b.k AND a.v > 5 ORDER BY 1, 2"), [["2", "2"], ["2", "3"], ["4", "1"]]);
}

#[test]
fn go_is_not_recognised_inside_a_bracketed_identifier_with_an_apostrophe() {
    // a stored procedure named like a sentence (as tSQLt test names are) may contain an apostrophe
    let batches = tsql_server::lexer::split_batches("CREATE PROCEDURE p.[it's a test]\nAS\nBEGIN\n  SELECT 1;\nEND;\nGO\nSELECT [a]]'b], \"q'r\" FROM t\nGO 2\nSELECT 'GO'\nGO\n");
    let texts: Vec<&str> = batches.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(texts.len(), 3, "{:?}", texts);
    assert!(texts[0].starts_with("CREATE PROCEDURE p.[it's a test]") && texts[0].ends_with("END;"));
    assert_eq!(batches[1].1, 2);
    assert!(texts[1].contains("[a]]'b]"));
    // and the server runs such a procedure
    let mut c = conn();
    script(&mut c, "CREATE PROCEDURE dbo.[it's a test] AS SELECT 7;\nGO\nEXEC dbo.[it's a test]");
}

#[test]
fn absurdly_deep_or_long_expressions_are_refused_not_fatal() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE d (a INT)");
    for (what, sql) in [
        ("a chain of 5000 additions", format!("SELECT 1{}", "+1".repeat(5000))),
        ("a chain of 5000 ORs", format!("SELECT COUNT(*) FROM d WHERE {}", (0..5000).map(|i| format!("a = {}", i)).collect::<Vec<_>>().join(" OR "))),
        ("a chain of 5000 ANDs", format!("SELECT COUNT(*) FROM d WHERE {}", (0..5000).map(|i| format!("a <> {}", i)).collect::<Vec<_>>().join(" AND "))),
        ("1000 nested parentheses", format!("SELECT {}1{}", "(".repeat(1000), ")".repeat(1000))),
        ("1000 nested subqueries", format!("SELECT {}1{}", "(SELECT ".repeat(1000), ")".repeat(1000))),
        ("1000 nested derived tables", format!("SELECT * FROM {}d{}", "(SELECT * FROM ".repeat(1000), ") x".repeat(1000))),
    ] {
        assert_eq!(err_no(&mut c, &sql), 8632, "{}", what);
    }
    // ordinary expressions of some size still work, and the connection is fine afterwards
    assert_eq!(scalar(&mut c, &format!("SELECT 1{}", "+1".repeat(20))), "21");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM d WHERE a = 1 OR a = 2 OR a = 3 OR a = 4 OR a = 5 OR a = 6 OR a = 7 OR a = 8 OR a = 9 OR a = 10 OR a = 11 OR a = 12"), "0");
    assert_eq!(scalar(&mut c, &format!("SELECT {}1{}", "(".repeat(8), ")".repeat(8))), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM d WHERE a IN (1, 2, 3) OR a = 4 AND a > 0"), "0");
}
