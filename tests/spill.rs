//! Sorts, DISTINCT and TOP n over tables much bigger than the working-memory budget: results must equal those of
//! the general (materialising) executor, spill files must be used and then removed.
mod common;
use common::*;
use tsql_server::{Connection, Server};

const ROWS: usize = 6000;

fn setup() -> (Server, Connection, tempfile::TempDir) {
    let s = common::server();
    let dir = tempfile::tempdir().unwrap();
    let mut c = s.connect();
    script(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, grp INT, name VARCHAR(40), val FLOAT, note NVARCHAR(100) NULL)");
    // deterministic pseudo-random data with plenty of ties in grp and duplicates in name
    let mut sql = String::new();
    for i in 0..ROWS {
        let g = (i * 7919) % 53;
        let n = (i * 31) % 211;
        sql.push_str(&format!("INSERT big VALUES ({}, {}, 'name{}', {}.5, {});\n", i, g, n, (i * 13) % 997, if i % 5 == 0 { "NULL".to_string() } else { format!("N'note {}'", i % 17) }));
    }
    script(&mut c, &sql);
    (s, c, dir)
}

/// Run `q` with the streaming executor forced on and a tiny budget, and again with it off; both must agree.
fn same(s: &Server, c: &mut Connection, dir: &std::path::Path, q: &str) -> Vec<Vec<String>> {
    s.set_work_mem(4096, 0, Some(dir.to_path_buf()));
    let streamed = rows(c, q);
    let plan = c.last_plan().join("\n");
    s.set_work_mem(64 << 20, usize::MAX, Some(dir.to_path_buf()));
    let general = rows(c, q);
    assert_eq!(streamed, general, "streaming and general executors differ for: {q}\nplan: {plan}");
    assert!(plan.contains("streaming scan"), "the streaming executor was not used for {q}: {plan}");
    streamed
}

fn spill_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("tsql-spill-")).count()
}

#[test]
fn order_by_spills_and_matches_the_general_executor() {
    let (s, mut c, dir) = setup();
    for q in [
        "SELECT id, grp, name FROM big ORDER BY grp, name",
        "SELECT id, grp FROM big ORDER BY grp DESC, id",
        "SELECT id, val FROM big WHERE grp < 20 ORDER BY val, id DESC",
        "SELECT name, note FROM big ORDER BY note, name",
        "SELECT id * 2 AS d, name FROM big ORDER BY d DESC",
        "SELECT id, grp FROM big ORDER BY 2, 1",
        "SELECT * FROM big WHERE grp = 5 OR name = 'name7' ORDER BY val DESC, id DESC",
    ] {
        let r = same(&s, &mut c, dir.path(), q);
        assert!(!r.is_empty(), "{q}");
    }
    // ties keep the order rows were read in, exactly as the stable in-memory sort does
    let r = same(&s, &mut c, dir.path(), "SELECT id FROM big ORDER BY grp");
    assert_eq!(r.len(), ROWS);
    assert_eq!(spill_files(dir.path()), 0, "spill files are removed after the statement");
}

#[test]
fn the_sort_really_spills() {
    let (s, mut c, dir) = setup();
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    exec(&mut c, "SELECT id, name FROM big ORDER BY name, id");
    let plan = c.last_plan().join("\n");
    let runs: usize = plan.split("external sort (").nth(1).and_then(|r| r.split(' ').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
    assert!(runs > 10, "expected many spilled runs: {plan}");
    assert_eq!(spill_files(dir.path()), 0);
}

#[test]
fn top_n_keeps_only_the_best_rows() {
    let (s, mut c, dir) = setup();
    for q in [
        "SELECT TOP 10 id, grp FROM big ORDER BY grp DESC, id",
        "SELECT TOP 1 id FROM big ORDER BY val DESC, id",
        "SELECT TOP 0 id FROM big ORDER BY grp",
        "SELECT id FROM big ORDER BY grp, id OFFSET 100 ROWS FETCH NEXT 25 ROWS ONLY",
        "SELECT TOP 50 id FROM big WHERE grp > 10 ORDER BY name, id",
        "SELECT TOP 5 WITH TIES grp, id FROM big ORDER BY grp",
        "SELECT TOP 10 PERCENT id FROM big ORDER BY val DESC, id DESC",
        "SELECT TOP 7 id, name FROM big",
        "SELECT TOP 7 id FROM big WHERE grp = 3",
    ] {
        same(&s, &mut c, dir.path(), q);
    }
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    exec(&mut c, "SELECT TOP 10 id FROM big ORDER BY grp, id");
    assert!(c.last_plan().join("\n").contains("top-N in memory"));
    // a plain TOP stops reading early
    exec(&mut c, "SELECT TOP 7 id FROM big");
    assert!(c.last_plan().join("\n").contains("[7 rows read]") || c.last_plan().join("\n").contains("[512 rows read]"), "{:?}", c.last_plan());
}

#[test]
fn distinct_keeps_the_first_arrival_and_the_requested_order() {
    let (s, mut c, dir) = setup();
    for q in [
        "SELECT DISTINCT grp FROM big",
        "SELECT DISTINCT grp, name FROM big",
        "SELECT DISTINCT grp FROM big ORDER BY grp DESC",
        "SELECT DISTINCT name, note FROM big ORDER BY name, note",
        "SELECT DISTINCT TOP 20 grp FROM big ORDER BY grp",
        "SELECT DISTINCT note FROM big WHERE grp < 10",
    ] {
        same(&s, &mut c, dir.path(), q);
    }
    let r = same(&s, &mut c, dir.path(), "SELECT DISTINCT grp FROM big");
    assert_eq!(r.len(), 53);
    assert_eq!(spill_files(dir.path()), 0);
}

#[test]
fn errors_and_permissions_behave_the_same_while_streaming() {
    let (s, mut c, dir) = setup();
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    assert_eq!(c.execute("SELECT id FROM big ORDER BY 9").first_error().unwrap().number, 108);
    assert_eq!(c.execute("SELECT nosuch FROM big ORDER BY id").first_error().unwrap().number, 207);
    assert_eq!(c.execute("SELECT 1/(id - 5) FROM big ORDER BY id").first_error().unwrap().number, 8134);
    assert_eq!(spill_files(dir.path()), 0, "a failing statement removes its spill files");
    // column-level permissions still apply
    script(&mut c, "CREATE LOGIN u WITH PASSWORD = 'upw'\nCREATE USER u FOR LOGIN u\nGRANT SELECT (id, grp) ON big TO u");
    let mut u = s.connect_as("u", "upw").unwrap();
    assert_eq!(u.query("SELECT TOP 3 id FROM big ORDER BY grp, id").unwrap().len(), 3);
    assert_eq!(u.execute("SELECT id FROM big ORDER BY name").first_error().unwrap().number, 230);
    assert_eq!(u.execute("SELECT * FROM big ORDER BY id").first_error().unwrap().number, 230);
}

#[test]
fn small_tables_keep_using_the_general_executor() {
    let (s, mut c, dir) = setup();
    s.set_work_mem(4096, 1_000_000, Some(dir.path().to_path_buf()));
    exec(&mut c, "SELECT id FROM big ORDER BY grp");
    assert!(!c.last_plan().join("\n").contains("streaming"), "{:?}", c.last_plan());
}

#[test]
fn group_by_sorts_on_the_key_and_matches_the_general_executor() {
    let (s, mut c, dir) = setup();
    for q in [
        // no ORDER BY: groups come out in first-appearance order, as in the general executor
        "SELECT grp, COUNT(*), SUM(val), MIN(name), MAX(id) FROM big GROUP BY grp",
        "SELECT name, COUNT(*) AS c FROM big GROUP BY name HAVING COUNT(*) > 25",
        "SELECT note, COUNT(*) FROM big GROUP BY note",
        "SELECT grp % 5 AS m, COUNT(*), AVG(val) FROM big GROUP BY grp % 5",
        "SELECT grp, name, COUNT(*) FROM big WHERE id < 3000 GROUP BY grp, name HAVING MAX(id) > 2900 ORDER BY grp, name",
        "SELECT name, COUNT(*) c FROM big GROUP BY name ORDER BY c DESC, name",
        "SELECT TOP 5 grp, AVG(val) FROM big GROUP BY grp ORDER BY 2 DESC, 1",
        "SELECT DISTINCT COUNT(*) FROM big GROUP BY grp",
        "SELECT grp, STRING_AGG(name, ',') FROM big WHERE id < 200 GROUP BY grp ORDER BY grp",
        "SELECT grp, COUNT(DISTINCT name) FROM big GROUP BY grp ORDER BY grp",
        "SELECT grp FROM big GROUP BY grp ORDER BY COUNT(*) DESC, grp",
    ] {
        let r = same(&s, &mut c, dir.path(), q);
        assert!(!r.is_empty(), "{q}");
    }
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    exec(&mut c, "SELECT grp, COUNT(*) FROM big GROUP BY grp");
    let plan = c.last_plan().join("\n");
    assert!(plan.contains("group by sort (53 group(s)"), "{plan}");
    assert!(!plan.contains("0 spilled run(s))"), "the group sort should have spilled: {plan}");
    assert_eq!(spill_files(dir.path()), 0);
    // an error while aggregating cleans up too
    assert!(c.execute("SELECT grp, SUM(1/(id - 77)) FROM big GROUP BY grp").first_error().is_some());
    assert_eq!(spill_files(dir.path()), 0);
}

#[test]
fn unsupported_shapes_fall_back_to_the_general_executor() {
    let (s, mut c, dir) = setup();
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    for q in [
        "SELECT grp, COUNT(*) FROM big GROUP BY ROLLUP(grp)",
        "SELECT COUNT(*) FROM big",
        "SELECT grp, ROW_NUMBER() OVER (ORDER BY id) FROM big ORDER BY id",
        "SELECT a.id FROM big a JOIN big b ON a.id = b.id ORDER BY a.id",
    ] {
        exec(&mut c, q);
        assert!(!c.last_plan().join("\n").contains("streaming scan"), "{q}: {:?}", c.last_plan());
    }
}

// ------------------------------------------------------------------------------------------------ joins
fn join_setup() -> (Server, Connection, tempfile::TempDir) {
    let s = common::server();
    let dir = tempfile::tempdir().unwrap();
    let mut c = s.connect();
    script(&mut c, "CREATE TABLE reg (region INT PRIMARY KEY, rname VARCHAR(20));\nCREATE TABLE dim (id INT PRIMARY KEY, region INT, label VARCHAR(30));\nCREATE TABLE fact (id INT PRIMARY KEY, dim_id INT NULL, qty INT, tag VARCHAR(20));");
    let mut sql = String::new();
    for r in 0..12 {
        sql.push_str(&format!("INSERT reg VALUES ({}, 'region {}');\n", r, r));
    }
    for i in 0..300 {
        sql.push_str(&format!("INSERT dim VALUES ({}, {}, 'dim {}');\n", i, i % 15, i)); // regions 12..14 have no reg row
    }
    for i in 0..ROWS {
        // ~10% NULL keys, ~7% keys with no dimension row, the rest spread over the dimension
        let k = if i % 10 == 3 { "NULL".to_string() } else if i % 14 == 5 { format!("{}", 300 + i % 40) } else { format!("{}", (i * 37) % 300) };
        sql.push_str(&format!("INSERT fact VALUES ({}, {}, {}, 'tag{}');\n", i, k, (i * 11) % 50, i % 9));
    }
    script(&mut c, &sql);
    (s, c, dir)
}

/// Like `same`, for queries whose plan must show the streaming hash join. Without an ORDER BY the row order is not
/// defined, so both result sets are sorted before they are compared.
fn same_join(s: &Server, c: &mut Connection, dir: &std::path::Path, q: &str, budget: usize) -> (Vec<Vec<String>>, String) {
    s.set_work_mem(budget, 0, Some(dir.to_path_buf()));
    let mut streamed = rows(c, q);
    let plan = c.last_plan().join("\n");
    s.set_work_mem(64 << 20, usize::MAX, Some(dir.to_path_buf()));
    let mut general = rows(c, q);
    if !q.to_uppercase().contains("ORDER BY") {
        streamed.sort();
        general.sort();
    }
    assert_eq!(streamed, general, "streaming and general executors differ for: {q}\nplan: {plan}");
    assert!(plan.contains("streaming hash join"), "the streaming join was not used for {q}: {plan}");
    (streamed, plan)
}

#[test]
fn inner_and_left_joins_spill_and_match_the_general_executor() {
    let (s, mut c, dir) = join_setup();
    for q in [
        "SELECT f.id, d.label FROM fact f JOIN dim d ON f.dim_id = d.id ORDER BY f.id",
        "SELECT f.id, d.label, f.tag FROM fact f LEFT JOIN dim d ON f.dim_id = d.id ORDER BY f.id",
        "SELECT f.id, d.region FROM fact f JOIN dim d ON f.dim_id = d.id AND f.qty > d.region ORDER BY f.id",
        "SELECT f.id, d.region FROM fact f LEFT JOIN dim d ON f.dim_id = d.id AND f.qty > d.region ORDER BY f.id",
        "SELECT f.id, d.label FROM fact f, dim d WHERE f.dim_id = d.id AND d.region = 3 ORDER BY f.id",
        "SELECT f.id, d.label FROM fact f LEFT JOIN dim d ON f.dim_id = d.id WHERE d.label IS NULL ORDER BY f.id",
        "SELECT * FROM dim d JOIN fact f ON d.id = f.dim_id WHERE f.tag = 'tag4' ORDER BY f.id",
        "SELECT f.id, d.label FROM fact f JOIN dim d ON f.dim_id = d.id",
    ] {
        let (r, plan) = same_join(&s, &mut c, dir.path(), q, 4096);
        assert!(!r.is_empty(), "{q}");
        if !q.contains("region = 3") {
            assert!(plan.contains("grace hash"), "the build side should not fit in 4 KiB for {q}: {plan}");
        }
    }
    assert_eq!(spill_files(dir.path()), 0, "spill files are removed after the statement");
}

#[test]
fn three_way_joins_aggregates_and_top_n_over_a_streamed_join() {
    let (s, mut c, dir) = join_setup();
    for q in [
        "SELECT f.id, d.label, r.rname FROM fact f JOIN dim d ON f.dim_id = d.id JOIN reg r ON d.region = r.region ORDER BY f.id",
        "SELECT f.id, d.label, r.rname FROM fact f JOIN dim d ON f.dim_id = d.id LEFT JOIN reg r ON d.region = r.region ORDER BY f.id",
        "SELECT d.region, COUNT(*) AS n, SUM(f.qty) AS q FROM fact f JOIN dim d ON f.dim_id = d.id GROUP BY d.region ORDER BY d.region",
        "SELECT r.rname, COUNT(*) AS n FROM fact f JOIN dim d ON f.dim_id = d.id JOIN reg r ON d.region = r.region GROUP BY r.rname HAVING COUNT(*) > 10 ORDER BY r.rname",
        "SELECT TOP 7 f.id, d.label FROM fact f JOIN dim d ON f.dim_id = d.id ORDER BY f.qty DESC, f.id",
        "SELECT DISTINCT d.region, f.tag FROM fact f JOIN dim d ON f.dim_id = d.id ORDER BY d.region, f.tag",
        "SELECT f.id FROM fact f JOIN dim d ON f.dim_id = d.id ORDER BY f.id OFFSET 100 ROWS FETCH NEXT 25 ROWS ONLY",
    ] {
        let (r, _) = same_join(&s, &mut c, dir.path(), q, 4096);
        assert!(!r.is_empty(), "{q}");
    }
    assert_eq!(spill_files(dir.path()), 0);
}

#[test]
fn skewed_keys_are_joined_in_several_passes() {
    let s = common::server();
    let dir = tempfile::tempdir().unwrap();
    let mut c = s.connect();
    script(&mut c, "CREATE TABLE probe (id INT PRIMARY KEY, k INT);\nCREATE TABLE heavy (id INT PRIMARY KEY, k INT, pad VARCHAR(40));");
    let mut sql = String::new();
    // the bigger table is probed, the smaller one is built: here the build side (heavy) is skewed
    for i in 0..1600 {
        sql.push_str(&format!("INSERT probe VALUES ({}, {});
", i, if i < 30 { 1 } else { 1000 + i }));
    }
    for i in 0..1500 {
        // nearly every build row has key 1: one partition holds almost everything
        sql.push_str(&format!("INSERT heavy VALUES ({}, {}, 'padding padding {}');
", i, if i < 1400 { 1 } else { 2 + i % 3 }, i));
    }
    script(&mut c, &sql);
    let (r, plan) = same_join(&s, &mut c, dir.path(), "SELECT p.k, COUNT(*) AS n FROM probe p JOIN heavy h ON p.k = h.k GROUP BY p.k ORDER BY p.k", 8192);
    assert_eq!(r[0], vec!["1".to_string(), "42000".to_string()], "{plan}");
    assert!(plan.contains("extra pass"), "a partition far bigger than the budget needs several passes: {plan}");
    let (_, plan) = same_join(&s, &mut c, dir.path(), "SELECT p.id, h.id FROM probe p LEFT JOIN heavy h ON p.k = h.k AND h.id < 3 WHERE p.id < 40 ORDER BY p.id, h.id", 8192);
    let _ = plan;
    assert_eq!(spill_files(dir.path()), 0);
}

#[test]
fn a_small_build_side_stays_in_memory() {
    let (s, mut c, dir) = join_setup();
    let (_, plan) = same_join(&s, &mut c, dir.path(), "SELECT f.id, r.rname FROM fact f JOIN dim d ON f.dim_id = d.id JOIN reg r ON d.region = r.region ORDER BY f.id", 64 << 20);
    assert!(plan.contains("hash in memory") && !plan.contains("grace"), "{plan}");
}

#[test]
fn null_keys_never_match_and_permissions_still_apply_to_joined_tables() {
    let (s, mut c, dir) = join_setup();
    // NULL = NULL is not true, so rows with a NULL key only appear in the outer join, null-extended
    let (r, _) = same_join(&s, &mut c, dir.path(), "SELECT COUNT(*) AS n, COUNT(d.id) AS m FROM fact f LEFT JOIN dim d ON f.dim_id = d.id GROUP BY f.tag ORDER BY f.tag", 4096);
    assert!(r.iter().all(|x| x[0] != x[1]));
    // a login with SELECT on one table only cannot read the other through a streamed join
    script(&mut c, "CREATE LOGIN u1 WITH PASSWORD = 'pw1'; CREATE USER u1 FOR LOGIN u1; GRANT SELECT ON fact TO u1;");
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    let mut u = s.connect_as("u1", "pw1").unwrap();
    let e = err(&mut u, "SELECT f.id, d.label FROM fact f JOIN dim d ON f.dim_id = d.id ORDER BY f.id");
    assert_eq!(e.number, 229, "{}", e);
}

// ------------------------------------------------------------------------------------------------ streamed results
use tsql_server::{Event, Streamed};

/// Run `sql` with `execute_streaming`; returns (rows, number of Event::Rows seen, plan).
fn streamed(c: &mut Connection, sql: &str) -> (Vec<Vec<String>>, usize, Vec<String>) {
    let mut rows: Vec<Vec<String>> = vec![];
    let mut whole = 0;
    let mut open = false;
    let mut expected: Option<u64> = None;
    c.execute_streaming(sql, &mut |item| {
        match item {
            Streamed::Event(Event::Rows(r)) => {
                whole += 1;
                rows.extend(r.rows.iter().map(|x| x.iter().map(|v| v.to_string()).collect()));
            }
            Streamed::Event(Event::Error(e)) => panic!("{}", e),
            Streamed::Event(_) => {}
            Streamed::Header { .. } => {
                assert!(!open, "a result set started inside another");
                open = true;
            }
            Streamed::Row(r) => {
                assert!(open, "a row arrived outside a result set");
                rows.push(r.iter().map(|v| v.to_string()).collect());
            }
            Streamed::End(n) => {
                assert!(open);
                open = false;
                expected = Some(n);
            }
        }
        true
    });
    assert!(!open);
    if let Some(n) = expected {
        assert_eq!(n as usize, rows.len());
    }
    (rows, whole, c.last_plan())
}

#[test]
fn big_results_are_streamed_row_by_row_and_equal_the_collected_ones() {
    let (s, mut c, dir) = join_setup();
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    for q in [
        "SELECT id, dim_id, qty FROM fact ORDER BY qty DESC, id",
        "SELECT f.id, d.label FROM fact f JOIN dim d ON f.dim_id = d.id ORDER BY f.id",
        "SELECT f.id, d.label FROM fact f LEFT JOIN dim d ON f.dim_id = d.id",
        "SELECT DISTINCT tag FROM fact ORDER BY tag",
        "SELECT tag, COUNT(*) AS n, SUM(qty) AS q FROM fact GROUP BY tag ORDER BY tag",
        "SELECT TOP 10 id, qty FROM fact ORDER BY qty, id",
        "SELECT id FROM fact ORDER BY qty, id OFFSET 5 ROWS FETCH NEXT 12 ROWS ONLY",
        "SELECT id + 1 AS next_id, UPPER(tag) AS t FROM fact ORDER BY id",
    ] {
        let (got, whole, plan) = streamed(&mut c, q);
        assert_eq!(whole, 0, "{q} was delivered whole: {:?}", plan);
        assert!(plan.iter().any(|l| l.contains("rows streamed to the client")), "{q}: {:?}", plan);
        let mut want = rows(&mut c, q);
        let mut got = got;
        if !q.to_uppercase().contains("ORDER BY") {
            got.sort();
            want.sort();
        }
        assert_eq!(got, want, "{q}");
    }
    assert_eq!(spill_files(dir.path()), 0);
}

#[test]
fn rowcount_and_messages_keep_their_order_around_a_streamed_result() {
    let (s, mut c, dir) = join_setup();
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    let mut seen: Vec<String> = vec![];
    c.execute_streaming("PRINT 'before'; SELECT id FROM fact ORDER BY id; PRINT 'after'; SELECT @@ROWCOUNT AS n", &mut |item| {
        seen.push(match item {
            Streamed::Event(Event::Message(m)) => format!("msg:{}", m),
            Streamed::Event(Event::RowCount(n)) => format!("count:{}", n),
            Streamed::Event(Event::Rows(r)) => format!("rows:{}", r.rows.len()),
            Streamed::Event(_) => "other".into(),
            Streamed::Header { .. } => "header".into(),
            Streamed::Row(_) => return true,
            Streamed::End(n) => format!("end:{}", n),
        });
        true
    });
    assert_eq!(seen[0], "msg:before");
    assert_eq!(seen[1], "header");
    assert_eq!(seen[2], format!("end:{}", ROWS));
    assert_eq!(seen[3], format!("count:{}", ROWS));
    assert_eq!(seen[4], "msg:after");
    assert_eq!(seen[5], "rows:1");
}

#[test]
fn abandoning_a_streamed_result_stops_the_statement_and_cleans_up() {
    let (s, mut c, dir) = join_setup();
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    let mut n = 0;
    let mut errors = vec![];
    c.execute_streaming("SELECT f.id, d.label FROM fact f JOIN dim d ON f.dim_id = d.id ORDER BY f.id", &mut |item| {
        match item {
            Streamed::Row(_) => {
                n += 1;
                return n < 100; // the client goes away after 100 rows
            }
            Streamed::Event(Event::Error(e)) => errors.push(e.number),
            _ => {}
        }
        true
    });
    assert_eq!(n, 100);
    assert_eq!(errors, vec![64]);
    assert_eq!(spill_files(dir.path()), 0, "spill files are removed when the client vanishes");
    // the session is still usable
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM reg"), "12");
}

#[test]
fn results_that_are_not_streamable_still_arrive_whole() {
    let (s, mut c, dir) = join_setup();
    s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
    for q in ["SELECT COUNT(*) AS n FROM fact", "SELECT id FROM fact UNION SELECT id FROM dim ORDER BY 1", "SELECT TOP 5 PERCENT id FROM fact ORDER BY id", "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS r FROM dim"] {
        let (got, whole, _) = streamed(&mut c, q);
        assert_eq!(whole, 1, "{q}");
        assert_eq!(got, rows(&mut c, q), "{q}");
    }
}
