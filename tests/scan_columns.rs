//! A scan of a big table copies only the columns the query names, and a query of nothing but aggregates is run by the streaming
//! executor as one group. Neither may change an answer: every query here is run with the pruning (the default) and without it
//! (`TSQL_NO_SCAN_PRUNE`), on a server that streams whatever it can, and must give the same rows.
mod common;
use common::*;
use std::sync::Mutex;

/// The tests change a process-wide variable.
static ENV: Mutex<()> = Mutex::new(());

fn data() -> tsql_server::Connection {
    let s = server();
    s.set_work_mem(256 << 20, 0, None);
    let mut c = s.connect();
    script(
        &mut c,
        "
CREATE TABLE big (id INT PRIMARY KEY, grp INT, name VARCHAR(30), amount DECIMAL(12,2), note VARCHAR(40), d DATE, doubled AS (amount * 2))
GO
WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 6000) INSERT big (id, grp, name, amount, note, d) SELECT i, i % 13, 'name' + CAST(i % 97 AS VARCHAR(5)), i * 1.5, CASE WHEN i % 7 = 0 THEN NULL ELSE 'note' + CAST(i AS VARCHAR(8)) END, DATEADD(DAY, i % 400, '2020-01-01') FROM n OPTION (MAXRECURSION 0)
GO
CREATE TABLE plain (id INT PRIMARY KEY, grp INT, name VARCHAR(30), amount DECIMAL(12,2), note VARCHAR(40), d DATE)
GO
INSERT plain SELECT id, grp, name, amount, note, d FROM big
GO
CREATE TABLE dim (g INT PRIMARY KEY, label VARCHAR(20), pad VARCHAR(20))
GO
WITH n AS (SELECT 0 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 12) INSERT dim SELECT i, 'g' + CAST(i AS VARCHAR(3)), 'pad' FROM n
GO
",
    );
    c
}

fn run(c: &mut tsql_server::Connection, sql: &str) -> Vec<Vec<String>> {
    let mut r = rows(c, sql);
    r.sort();
    r
}

/// The same rows with and without the pruning; returns them.
fn same(c: &mut tsql_server::Connection, sql: &str) -> Vec<Vec<String>> {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("TSQL_NO_SCAN_PRUNE");
    let pruned = run(c, sql);
    std::env::set_var("TSQL_NO_SCAN_PRUNE", "1");
    let whole = run(c, sql);
    std::env::remove_var("TSQL_NO_SCAN_PRUNE");
    assert_eq!(pruned, whole, "{sql}");
    pruned
}

#[test]
fn queries_over_big_tables_give_the_same_rows_with_and_without_the_pruning() {
    let mut c = data();
    for sql in [
        "SELECT grp, COUNT(*), SUM(amount) FROM plain GROUP BY grp",
        // a column used only to order, only to filter, only in a HAVING, only in an expression
        "SELECT grp, MAX(amount) FROM plain WHERE note IS NOT NULL GROUP BY grp HAVING MIN(d) < '2020-03-01'",
        "SELECT TOP 20 name FROM plain ORDER BY amount DESC, id",
        "SELECT TOP 20 id FROM plain ORDER BY note, id",
        "SELECT DISTINCT grp FROM plain WHERE d >= '2020-06-01' AND d < '2020-07-01'",
        "SELECT name, ROW_NUMBER() OVER (PARTITION BY grp ORDER BY amount) FROM plain WHERE id < 200",
        // joins: the columns of each side that the ON and the select list name
        "SELECT dim.label, COUNT(*), SUM(plain.amount) FROM plain JOIN dim ON dim.g = plain.grp GROUP BY dim.label",
        "SELECT p.id, d.label FROM plain p LEFT JOIN dim d ON d.g = p.grp AND d.pad = 'pad' WHERE p.id % 1000 = 0",
        // a correlated subquery names columns of the outer table inside itself
        "SELECT p.id FROM plain p WHERE p.amount > (SELECT AVG(q.amount) FROM plain q WHERE q.grp = p.grp AND q.name = p.name) AND p.id < 300",
        "SELECT COUNT(*) FROM plain p WHERE EXISTS (SELECT 1 FROM dim WHERE dim.g = p.grp AND dim.label <> p.name)",
        // every column (a *), a function of the whole row
        "SELECT * FROM plain WHERE id % 1500 = 0",
        "SELECT p.*, 1 FROM plain p WHERE id % 1500 = 0",
        "SELECT CHECKSUM(*) FROM plain WHERE id % 1500 = 0",
        // a table with a computed column is read whole
        "SELECT grp, SUM(doubled) FROM big GROUP BY grp",
        "SELECT id, doubled FROM big WHERE id % 1000 = 0",
        // aggregates and nothing else: one group
        "SELECT COUNT(*) FROM plain",
        "SELECT COUNT(*), SUM(amount), MIN(d), MAX(name), AVG(amount), COUNT(note), COUNT(DISTINCT grp) FROM plain",
        "SELECT SUM(amount * 2 + 1), MAX(LEN(name)) FROM plain WHERE grp = 3",
        "SELECT COUNT(*) + 1, MAX(id) - MIN(id) FROM plain WHERE note IS NULL",
        "SELECT 1 + COUNT(*) FROM plain WHERE id > 100",
    ] {
        same(&mut c, sql);
    }
}

#[test]
fn the_pruning_is_in_effect_and_says_so_in_the_plan() {
    let mut c = data();
    exec(&mut c, "SELECT grp, SUM(amount) FROM plain GROUP BY grp");
    assert!(c.last_plan().iter().any(|l| l.contains("copies 2 of its 6 columns")), "{:?}", c.last_plan());
    exec(&mut c, "SELECT * FROM plain WHERE id % 1500 = 0 ORDER BY note");
    assert!(!c.last_plan().iter().any(|l| l.contains("copies")), "{:?}", c.last_plan());
    exec(&mut c, "SELECT COUNT(*) FROM plain");
    assert!(c.last_plan().iter().any(|l| l.contains("group by hash (1 group(s))")), "{:?}", c.last_plan());
}

#[test]
fn an_aggregate_query_with_no_rows_still_gives_its_one_row() {
    let mut c = data();
    assert_eq!(same(&mut c, "SELECT COUNT(*), SUM(amount), MAX(name), COUNT(note) FROM plain WHERE id < 0"), vec![vec!["0".to_string(), "NULL".into(), "NULL".into(), "0".into()]]);
    assert_eq!(same(&mut c, "SELECT COUNT(*) FROM plain"), vec![vec!["6000".to_string()]]);
    // GROUP BY with no rows gives none
    assert!(same(&mut c, "SELECT grp, COUNT(*) FROM plain WHERE id < 0 GROUP BY grp").is_empty());
    // an aggregate that cannot be folded (a string aggregate, a DISTINCT one with an ORDER) keeps its answer
    assert_eq!(same(&mut c, "SELECT COUNT(DISTINCT grp) FROM plain"), vec![vec!["13".to_string()]]);
    // an error in an aggregate is still raised
    let r = c.execute("SELECT SUM(CAST(name AS INT)) FROM plain");
    assert!(r.first_error().is_some());
}
