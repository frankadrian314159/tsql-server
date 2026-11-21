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

#[test]
fn creating_a_view_does_not_execute_query_over_table_rows() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE conv_test (id INT, s VARCHAR(50))");
    exec(&mut c, "INSERT INTO conv_test VALUES (1, 'not_a_number'), (2, '42')");
    // If the query were executed against existing table rows during CREATE VIEW,
    // row 1 ('not_a_number') would fail with conversion error 245.
    let r = c.execute("CREATE VIEW v_conv AS SELECT id, s, CAST(s AS INT) AS num FROM conv_test");
    assert!(r.errors().is_empty(), "CREATE VIEW should succeed without running the query over rows: {:?}", r.errors());

    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.views WHERE name = 'v_conv'"), "1");
}

#[test]
fn create_view_performance_benchmark() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE big_t (id INT PRIMARY KEY, grp INT, name VARCHAR(40), val FLOAT)");
    let mut i = 0;
    while i < 100_000 {
        let mut sql = String::from("INSERT INTO big_t VALUES ");
        for k in i..(i + 500).min(100_000) {
            if k > i {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, 'user{}', {}.5)", k, k % 100, k, k % 500));
        }
        let r = c.execute(&sql);
        assert!(r.errors().is_empty());
        i += 500;
    }

    // Time the CREATE VIEW statement:
    let t_view = std::time::Instant::now();
    let r_view = c.execute("CREATE VIEW v_speed_test AS SELECT id, grp, name, val * 2.0 AS doubled FROM big_t WHERE val > 50");
    let dur_view = t_view.elapsed();
    assert!(r_view.errors().is_empty());

    // Time running the query itself (which is what CREATE VIEW used to do):
    let t_query = std::time::Instant::now();
    let r_query = c.execute("SELECT id, grp, name, val * 2.0 AS doubled FROM big_t WHERE val > 50");
    let dur_query = t_query.elapsed();
    assert!(r_query.errors().is_empty());

    println!("\n=== CREATE VIEW Performance Benchmark (100,000 rows) ===");
    println!("CREATE VIEW duration:    {:?}", dur_view);
    println!("Running query duration:  {:?}", dur_query);
    println!("Speedup ratio:           {:.1}x faster", dur_query.as_secs_f64() / dur_view.as_secs_f64().max(1e-9));

    // CREATE VIEW should be significantly faster than executing the query over the rows:
    assert!(dur_query > dur_view, "Query execution should take longer than CREATE VIEW");
}


