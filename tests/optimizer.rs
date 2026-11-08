//! The optimizer must never change an answer, and it must make the choices it claims to.
mod common;
use common::*;
use tsql_server::Connection;

fn build(c: &mut Connection) {
    script(
        c,
        "CREATE TABLE cust (id INT PRIMARY KEY, name VARCHAR(20), region VARCHAR(5) NULL)
CREATE TABLE ord (id INT PRIMARY KEY, cust_id INT NULL, total INT NULL, status VARCHAR(5))
CREATE TABLE line (id INT IDENTITY PRIMARY KEY, ord_id INT, qty INT, sku VARCHAR(10))
CREATE INDEX ix_ord_cust ON ord (cust_id)
CREATE INDEX ix_ord_total ON ord (total)
CREATE INDEX ix_line_ord ON line (ord_id)
CREATE INDEX ix_line_sku_qty ON line (sku, qty)
",
    );
    // customers: some with NULL region, duplicate names
    let mut sql = String::from("INSERT cust VALUES ");
    for i in 1..=40 {
        let region = if i % 7 == 0 { "NULL".to_string() } else { format!("'r{}'", i % 4) };
        sql.push_str(&format!("({}, 'c{}', {}),", i, i % 10, region));
    }
    sql.pop();
    exec(c, &sql);
    let mut sql = String::from("INSERT ord VALUES ");
    for i in 1..=200 {
        let cust = if i % 13 == 0 { "NULL".to_string() } else { ((i * 7) % 45 + 1).to_string() }; // some dangling customers
        let total = if i % 11 == 0 { "NULL".to_string() } else { ((i * 37) % 50).to_string() };
        sql.push_str(&format!("({}, {}, {}, '{}'),", i, cust, total, if i % 3 == 0 { "open" } else { "done" }));
    }
    sql.pop();
    exec(c, &sql);
    let mut sql = String::from("INSERT line (ord_id, qty, sku) VALUES ");
    for i in 1..=600 {
        sql.push_str(&format!("({}, {}, 'sku{}'),", (i * 3) % 210 + 1, i % 5, i % 9));
    }
    sql.pop();
    exec(c, &sql);
}

fn run(c: &mut Connection, sql: &str, on: bool) -> Vec<Vec<String>> {
    exec(c, &format!("SET OPTIMIZER {}", if on { "ON" } else { "OFF" }));
    rows(c, sql)
}

/// Same answer with the optimizer on and off. Ordered queries must also agree on row order.
fn same(c: &mut Connection, sql: &str, ordered: bool) {
    let mut a = run(c, sql, true);
    let mut b = run(c, sql, false);
    exec(c, "SET OPTIMIZER ON");
    if !ordered {
        a.sort();
        b.sort();
    }
    assert_eq!(a, b, "optimizer changed the result of:\n{}", sql);
}

#[test]
fn optimizer_never_changes_answers() {
    let mut c = conn();
    build(&mut c);
    let unordered = [
        "SELECT * FROM ord WHERE cust_id = 5",
        "SELECT * FROM ord WHERE cust_id IN (3, 5, 99)",
        "SELECT * FROM ord WHERE total > 40",
        "SELECT * FROM ord WHERE total BETWEEN 10 AND 12",
        "SELECT * FROM ord WHERE total >= 49 OR total IS NULL",
        "SELECT * FROM ord WHERE cust_id = 5 AND total > 10",
        "SELECT * FROM ord WHERE cust_id IS NULL",
        "SELECT * FROM ord WHERE status = 'OPEN'",
        "SELECT * FROM ord WHERE '5' = cust_id",
        "SELECT * FROM ord WHERE cust_id = 5.5",
        "SELECT * FROM ord WHERE cust_id = NULL",
        "SELECT COUNT(*) FROM ord WHERE id = 7",
        "SELECT o.id, c.name FROM ord o JOIN cust c ON o.cust_id = c.id",
        "SELECT o.id, c.name FROM ord o JOIN cust c ON o.cust_id = c.id WHERE c.region = 'r1'",
        "SELECT o.id, c.name FROM ord o, cust c WHERE o.cust_id = c.id AND o.total > 30",
        "SELECT * FROM cust c, ord o WHERE c.id = o.cust_id AND c.name = 'c3'",
        "SELECT c.name, l.qty FROM cust c JOIN ord o ON o.cust_id = c.id JOIN line l ON l.ord_id = o.id WHERE c.name = 'c3'",
        "SELECT * FROM line l JOIN ord o ON l.ord_id = o.id JOIN cust c ON o.cust_id = c.id WHERE l.sku = 'sku3' AND l.qty = 2",
        "SELECT c.id, o.id FROM cust c LEFT JOIN ord o ON o.cust_id = c.id",
        "SELECT c.id, o.id FROM cust c LEFT JOIN ord o ON o.cust_id = c.id WHERE o.id IS NULL",
        "SELECT c.id, o.id FROM cust c LEFT JOIN ord o ON o.cust_id = c.id WHERE c.region = 'r1'",
        "SELECT c.id, o.id FROM cust c LEFT JOIN ord o ON o.cust_id = c.id WHERE o.total > 40",
        "SELECT c.id, o.id FROM cust c RIGHT JOIN ord o ON o.cust_id = c.id WHERE o.total > 40",
        "SELECT c.id, o.id FROM cust c FULL JOIN ord o ON o.cust_id = c.id",
        "SELECT a.id, b.id FROM ord a JOIN ord b ON a.cust_id = b.cust_id AND a.id < b.id WHERE a.total = 10",
        "SELECT c.name, COUNT(*) FROM cust c JOIN ord o ON o.cust_id = c.id GROUP BY c.name",
        "SELECT c.name, SUM(o.total) FROM cust c JOIN ord o ON o.cust_id = c.id WHERE o.status = 'open' GROUP BY c.name HAVING SUM(o.total) > 20",
        "SELECT * FROM ord WHERE cust_id IN (SELECT id FROM cust WHERE region = 'r2')",
        "SELECT * FROM ord o WHERE EXISTS (SELECT 1 FROM line l WHERE l.ord_id = o.id AND l.qty = 4)",
        "SELECT * FROM cust c WHERE NOT EXISTS (SELECT 1 FROM ord o WHERE o.cust_id = c.id)",
        "SELECT o.id FROM ord o CROSS APPLY (SELECT TOP 1 l.qty FROM line l WHERE l.ord_id = o.id ORDER BY l.qty DESC) x WHERE o.total > 45",
        "SELECT * FROM (SELECT cust_id, SUM(total) s FROM ord GROUP BY cust_id) t JOIN cust c ON c.id = t.cust_id",
        "SELECT * FROM ord WHERE total = 10 AND cust_id = 4 AND status = 'done'",
        "SELECT * FROM line WHERE sku = 'sku3'",
        "SELECT * FROM line WHERE sku = 'sku3' AND qty > 2",
        "SELECT * FROM line WHERE sku > 'sku6'",
        "SELECT * FROM cust c JOIN ord o ON o.cust_id = c.id JOIN line l ON l.ord_id = o.id",
        "SELECT x.id FROM cust c JOIN ord x ON c.id = x.cust_id WHERE c.id = 5 AND x.total > 0",
        "SELECT DISTINCT cust_id FROM ord WHERE total > 10",
        "SELECT c.id FROM cust c WHERE c.id = (SELECT MAX(cust_id) FROM ord)",
    ];
    for q in unordered {
        same(&mut c, q, false);
    }
    let ordered = [
        "SELECT TOP 5 * FROM ord ORDER BY total",
        "SELECT TOP 5 * FROM ord ORDER BY total DESC",
        "SELECT TOP 10 id FROM ord ORDER BY id",
        "SELECT TOP 10 id FROM ord ORDER BY id DESC",
        "SELECT TOP 5 id FROM ord WHERE total > 20 ORDER BY id",
        "SELECT TOP 5 id FROM ord WHERE status = 'open' ORDER BY id",
        "SELECT id, total FROM ord ORDER BY total, id OFFSET 5 ROWS FETCH NEXT 4 ROWS ONLY",
        "SELECT TOP 7 * FROM line ORDER BY sku, qty",
        "SELECT TOP 7 * FROM line ORDER BY sku",
        "SELECT TOP 3 * FROM ord WHERE cust_id = 5 ORDER BY id",
        "SELECT id FROM ord ORDER BY id",
        "SELECT TOP 5 WITH TIES total FROM ord ORDER BY total",
        "SELECT TOP 10 PERCENT id FROM ord ORDER BY id",
        "SELECT TOP 4 c.name, o.total FROM cust c JOIN ord o ON o.cust_id = c.id ORDER BY o.total DESC, c.name, o.id",
    ];
    for q in ordered {
        same(&mut c, q, true);
    }
}

fn plan(c: &mut Connection, sql: &str) -> String {
    exec(c, sql);
    c.last_plan().join("\n")
}

#[test]
fn equality_predicates_use_an_index_seek() {
    let mut c = conn();
    build(&mut c);
    let p = plan(&mut c, "SELECT * FROM ord WHERE cust_id = 5");
    assert!(p.contains("index seek via ix_ord_cust"), "{}", p);
    let p = plan(&mut c, "SELECT * FROM ord WHERE id = 5");
    assert!(p.contains("index seek via PK"), "{}", p);
    let p = plan(&mut c, "SELECT * FROM ord WHERE total BETWEEN 10 AND 12");
    assert!(p.contains("index range scan via ix_ord_total"), "{}", p);
    let p = plan(&mut c, "SELECT * FROM ord WHERE status = 'open'");
    assert!(p.contains("full scan"), "unindexed column: {}", p);
    let p = plan(&mut c, "SELECT * FROM line WHERE sku = 'sku3' AND qty = 2");
    assert!(p.contains("ix_line_sku_qty (1 key(s))"), "{}", p);
    // a predicate that would need a lossy conversion must not use the index
    let p = plan(&mut c, "SELECT * FROM ord WHERE cust_id = 5.5");
    assert!(p.contains("full scan"), "{}", p);
    let p = plan(&mut c, "SELECT * FROM ord WHERE cust_id = 5 OR total = 7");
    assert!(p.contains("full scan"), "OR is not narrowed: {}", p);
}

#[test]
fn the_selective_table_is_joined_first_and_found_by_index() {
    let mut c = conn();
    build(&mut c);
    let p = plan(&mut c, "SELECT COUNT(*) FROM line l JOIN ord o ON o.id = l.ord_id JOIN cust c ON c.id = o.cust_id WHERE c.id = 7");
    assert!(p.contains("join order: c -> o -> l") || p.contains("join order: c ->"), "{}", p);
    assert!(p.contains("(reordered from the query's order)"), "{}", p);
    assert!(p.contains("index nested-loop"), "{}", p);
    // the answer is the same as the unoptimized execution
    same(&mut c, "SELECT COUNT(*) FROM line l JOIN ord o ON o.id = l.ord_id JOIN cust c ON c.id = o.cust_id WHERE c.id = 7", false);
    exec(&mut c, "SET OPTIMIZER OFF");
    let p = plan(&mut c, "SELECT COUNT(*) FROM line l JOIN ord o ON o.id = l.ord_id JOIN cust c ON c.id = o.cust_id WHERE c.id = 7");
    assert!(!p.contains("reordered"), "{}", p);
    assert!(p.contains("full scan"), "{}", p);
}

#[test]
fn comma_joins_use_where_conditions_as_join_keys() {
    let mut c = conn();
    build(&mut c);
    let p = plan(&mut c, "SELECT COUNT(*) FROM ord o, cust c WHERE o.cust_id = c.id");
    assert!(p.contains("hash join") || p.contains("index nested-loop"), "no cross product expected: {}", p);
    assert!(!p.contains("cross join"), "{}", p);
}

#[test]
fn conditions_are_pushed_to_each_table_but_not_through_outer_joins() {
    let mut c = conn();
    build(&mut c);
    let p = plan(&mut c, "SELECT c.id FROM cust c LEFT JOIN ord o ON o.cust_id = c.id WHERE c.id = 3");
    assert!(p.contains("scan c") && p.contains("index seek via PK"), "preserved side is narrowed: {}", p);
    let p = plan(&mut c, "SELECT c.id FROM cust c LEFT JOIN ord o ON o.cust_id = c.id WHERE o.total = 10");
    assert!(!p.contains("scan o [200 rows]: index"), "null-supplying side must not be narrowed before the join: {}", p);
    same(&mut c, "SELECT c.id, o.id FROM cust c LEFT JOIN ord o ON o.cust_id = c.id WHERE o.total = 10", false);
}

#[test]
fn order_by_reads_an_index_in_order_and_stops_early() {
    let mut c = conn();
    build(&mut c);
    let p = plan(&mut c, "SELECT TOP 3 id FROM ord ORDER BY id");
    assert!(p.contains("read in index order"), "{}", p);
    assert!(p.contains("-> 3 rows"), "only three rows should be read: {}", p);
    let p = plan(&mut c, "SELECT TOP 3 id FROM ord WHERE status = 'open' ORDER BY id");
    assert!(p.contains("read in index order") && p.contains("-> 3 rows"), "filter applied during the ordered scan: {}", p);
    let p = plan(&mut c, "SELECT TOP 3 id FROM ord ORDER BY status");
    assert!(!p.contains("read in index order"), "no index on status: {}", p);
    let p = plan(&mut c, "SELECT TOP 3 id FROM ord WHERE id IN (1,2,3,4,5) ORDER BY id");
    assert!(p.contains("index seek") || p.contains("range"), "{}", p);
    // descending reads the index backwards
    assert_eq!(rows(&mut c, "SELECT TOP 2 id FROM ord ORDER BY id DESC"), vec![vec!["200"], vec!["199"]]);
    // aggregates and DISTINCT are not eligible
    let p = plan(&mut c, "SELECT DISTINCT TOP 3 id FROM ord ORDER BY id");
    assert!(!p.contains("read in index order"), "{}", p);
}

#[test]
fn showplan_describes_without_running() {
    let mut c = conn();
    build(&mut c);
    let lines = c.explain("SELECT * FROM ord o JOIN cust cu ON cu.id = o.cust_id WHERE cu.id = 4").unwrap();
    let text = lines.join("\n");
    assert!(text.contains("SELECT"), "{}", text);
    assert!(text.contains("scan cu") && text.contains("index seek via PK"), "{}", text);
    assert!(text.contains("join order"), "{}", text);
    // SET SHOWPLAN_TEXT ON: statements are not executed
    exec(&mut c, "SET SHOWPLAN_TEXT ON");
    let r = exec(&mut c, "DELETE ord");
    assert!(r.resultsets()[0].rows[0][0].to_string().contains("not executed"));
    exec(&mut c, "SET SHOWPLAN_TEXT OFF");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord"), "200");
}

#[test]
fn indexed_lookups_are_fast_on_big_tables() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, g INT, pad VARCHAR(20))");
    exec(&mut c, "CREATE INDEX ix_g ON big (g)");
    exec(&mut c, "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 20000) INSERT big SELECT i, i % 1000, 'x' FROM n OPTION (MAXRECURSION 0)");
    let started = std::time::Instant::now();
    for i in 0..300 {
        let n = scalar(&mut c, &format!("SELECT COUNT(*) FROM big WHERE g = {}", i % 1000));
        assert_eq!(n, "20");
        assert_eq!(scalar(&mut c, &format!("SELECT pad FROM big WHERE id = {}", i + 1)), "x");
    }
    // 600 point lookups against 20 000 rows: full scans would take many seconds in a debug build
    assert!(started.elapsed().as_secs() < 8, "index lookups too slow: {:?}", started.elapsed());
    // a join driven by a handful of rows stays cheap too
    let started = std::time::Instant::now();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big a JOIN big b ON b.g = a.g WHERE a.id = 5"), "20");
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
}

#[test]
fn locking_reads_touch_only_rows_found_by_the_index() {
    // a writer holding one row must not block a reader that seeks a different row
    let s = common::server();
    let mut a = s.connect();
    let mut b = s.connect();
    exec(&mut a, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    exec(&mut a, "INSERT t VALUES (1,1),(2,2),(3,3)");
    exec(&mut a, "BEGIN TRAN; UPDATE t SET v = 10 WHERE id = 1");
    assert_eq!(scalar(&mut b, "SELECT v FROM t WHERE id = 3"), "3");
    exec(&mut b, "SET LOCK_TIMEOUT 100");
    assert_eq!(err_no(&mut b, "SELECT COUNT(*) FROM t"), 1222, "a full scan must wait for the locked row");
    exec(&mut a, "ROLLBACK");
}
