mod common;
use common::*;

fn setup(c: &mut tsql_server::Connection) {
    exec(c, "CREATE TABLE t (id INT PRIMARY KEY, grp INT, v VARCHAR(10) DEFAULT 'dflt')");
    exec(c, "INSERT t VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 1, 'c')");
    script(c, "CREATE VIEW g1 AS SELECT id, v FROM t WHERE grp = 1\nGO");
}

#[test]
fn update_through_a_simple_view_respects_its_filter() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "UPDATE g1 SET v = 'X'");
    assert_eq!(rows(&mut c, "SELECT id, v FROM t ORDER BY id"), vec![vec!["1", "X"], vec!["2", "b"], vec!["3", "X"]]);
    exec(&mut c, "UPDATE g1 SET v = 'only3' WHERE id = 3");
    assert_eq!(scalar(&mut c, "SELECT v FROM t WHERE id = 3"), "only3");
}

#[test]
fn delete_through_a_view() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "DELETE FROM g1 WHERE id = 3");
    exec(&mut c, "DELETE FROM g1 WHERE id = 2");
    assert_eq!(rows(&mut c, "SELECT id FROM t ORDER BY id"), vec![vec!["1"], vec!["2"]]);
}

#[test]
fn insert_through_a_view_uses_base_defaults() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "INSERT g1 (id, v) VALUES (4, 'new')");
    exec(&mut c, "INSERT g1 (id) VALUES (5)");
    assert_eq!(rows(&mut c, "SELECT id, v FROM t WHERE id >= 4 ORDER BY id"), vec![vec!["4", "new"], vec!["5", "dflt"]]);
}

#[test]
fn update_and_delete_through_a_cte() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "WITH x AS (SELECT * FROM t WHERE grp = 2) UPDATE x SET v = 'cte'");
    assert_eq!(scalar(&mut c, "SELECT v FROM t WHERE id = 2"), "cte");
    exec(&mut c, "WITH x AS (SELECT id FROM t WHERE grp = 1) DELETE FROM x");
    assert_eq!(rows(&mut c, "SELECT id FROM t"), vec![vec!["2"]]);
}

#[test]
fn self_referencing_dml_subqueries() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "DELETE FROM t WHERE id IN (SELECT id FROM t WHERE grp = 1)");
    assert_eq!(rows(&mut c, "SELECT id FROM t"), vec![vec!["2"]]);
    exec(&mut c, "INSERT INTO t (id, grp) SELECT id + 100, grp FROM t");
    exec(&mut c, "UPDATE t SET grp = (SELECT COUNT(*) FROM t)");
    assert_eq!(rows(&mut c, "SELECT DISTINCT grp FROM t"), vec![vec!["2"]]);
}
