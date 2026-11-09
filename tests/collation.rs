//! Collations: case, accent and binary sensitivity in comparisons, sorting, grouping, joins, indexes and constraints.
mod common;
use common::*;
use tsql_server::Server;

fn words(c: &mut tsql_server::Connection, sql: &str) -> Vec<String> {
    rows(c, sql).into_iter().map(|r| r[0].clone()).collect()
}

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE cs (id INT PRIMARY KEY, w VARCHAR(20) COLLATE Latin1_General_CS_AS);
         CREATE TABLE ci (id INT PRIMARY KEY, w VARCHAR(20));
         INSERT cs VALUES (1,'apple'),(2,'Apple'),(3,'APPLE'),(4,'banana'),(5,'Banana'),(6,'cherry'),(7,'apple');
         INSERT ci SELECT id, w FROM cs;",
    );
}

#[test]
fn case_sensitive_columns_compare_sort_and_group_by_case() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w = 'apple' ORDER BY id"), ["1", "7"]);
    assert_eq!(words(&mut c, "SELECT id FROM ci WHERE w = 'apple' ORDER BY id"), ["1", "2", "3", "7"], "the default collation ignores case");
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w <> 'apple' AND w LIKE 'A%' ORDER BY id"), ["2", "3"]);
    // sorted: all spellings of a word together, lower case before upper case
    assert_eq!(words(&mut c, "SELECT w FROM cs ORDER BY w, id"), ["apple", "apple", "Apple", "APPLE", "banana", "Banana", "cherry"]);
    assert_eq!(words(&mut c, "SELECT w FROM cs ORDER BY w DESC, id"), ["cherry", "Banana", "banana", "APPLE", "Apple", "apple", "apple"]);
    // grouping, DISTINCT and COUNT(DISTINCT) see the spellings as different values
    assert_eq!(rows(&mut c, "SELECT w, COUNT(*) FROM cs GROUP BY w ORDER BY w"), [["apple", "2"], ["Apple", "1"], ["APPLE", "1"], ["banana", "1"], ["Banana", "1"], ["cherry", "1"]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM (SELECT DISTINCT w FROM cs) d"), "6");
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT w) FROM cs"), "6");
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT w) FROM ci"), "3");
    // MIN / MAX use the collation (lower case sorts first)
    assert_eq!(scalar(&mut c, "SELECT MIN(w) FROM cs WHERE id IN (1, 2, 3)"), "apple");
    assert_eq!(scalar(&mut c, "SELECT MAX(w) FROM cs WHERE id IN (1, 2, 3)"), "APPLE");
    // IN, BETWEEN, LIKE
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w IN ('Apple', 'cherry') ORDER BY id"), ["2", "6"]);
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w BETWEEN 'banana' AND 'cherry' ORDER BY id"), ["4", "5", "6"]);
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w LIKE 'A%' ORDER BY id"), ["2", "3"]);
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w LIKE 'a%' ORDER BY id"), ["1", "7"]);
    assert_eq!(words(&mut c, "SELECT id FROM ci WHERE w LIKE 'a%' ORDER BY id"), ["1", "2", "3", "7"]);
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w LIKE '[A-B]anana' ORDER BY id"), ["5"]);
}

#[test]
fn collate_in_an_expression_overrides_the_columns_collation() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(words(&mut c, "SELECT id FROM ci WHERE w COLLATE Latin1_General_CS_AS = 'Apple'"), ["2"]);
    assert_eq!(words(&mut c, "SELECT id FROM cs WHERE w COLLATE Latin1_General_CI_AS = 'apple' ORDER BY id"), ["1", "2", "3", "7"]);
    assert_eq!(words(&mut c, "SELECT w FROM ci ORDER BY w COLLATE Latin1_General_CS_AS, id")[..3], ["apple", "apple", "Apple"]);
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'a' COLLATE Latin1_General_CS_AS = 'A' THEN 'same' ELSE 'different' END"), "different");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'a' = 'A' THEN 'same' ELSE 'different' END"), "same");
    assert_eq!(err(&mut c, "SELECT 1 WHERE 'a' COLLATE No_Such_Collation = 'a'").number, 448);
    // two columns whose collations differ cannot be compared without saying which one wins
    script(&mut c, "CREATE TABLE bin (id INT, w VARCHAR(20) COLLATE Latin1_General_BIN2);");
    assert_eq!(err(&mut c, "SELECT * FROM cs JOIN bin ON cs.w = bin.w").number, 468);
    exec(&mut c, "SELECT * FROM cs JOIN bin ON cs.w = bin.w COLLATE Latin1_General_CS_AS");
}

#[test]
fn accent_insensitive_and_binary_collations() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE ai (id INT PRIMARY KEY, w NVARCHAR(20) COLLATE Latin1_General_CI_AI);
         CREATE TABLE ac (id INT PRIMARY KEY, w NVARCHAR(20));
         CREATE TABLE bn (id INT PRIMARY KEY, w NVARCHAR(20) COLLATE Latin1_General_BIN2);
         INSERT ai VALUES (1, N'résumé'), (2, N'RESUME'), (3, N'resumé'), (4, N'other');
         INSERT ac SELECT id, w FROM ai;
         INSERT bn VALUES (1, N'a'), (2, N'B'), (3, N'b'), (4, N'A');",
    );
    assert_eq!(words(&mut c, "SELECT id FROM ai WHERE w = N'resume' ORDER BY id"), ["1", "2", "3"]);
    assert_eq!(words(&mut c, "SELECT id FROM ac WHERE w = N'resume' ORDER BY id"), ["2"], "the default collation is accent sensitive");
    assert_eq!(words(&mut c, "SELECT id FROM ai WHERE w LIKE N'res%' ORDER BY id"), ["1", "2", "3"]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT w) FROM ai"), "2");
    // binary: code point order (upper case before lower case), no folding at all
    assert_eq!(words(&mut c, "SELECT w FROM bn ORDER BY w"), ["A", "B", "a", "b"]);
    assert_eq!(words(&mut c, "SELECT id FROM bn WHERE w = N'b'"), ["3"]);
    assert_eq!(words(&mut c, "SELECT id FROM bn WHERE w > N'B' ORDER BY id"), ["1", "3"]);
}

#[test]
fn indexes_and_unique_constraints_follow_the_collation() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE u (id INT IDENTITY, name VARCHAR(30) COLLATE Latin1_General_CS_AS NOT NULL, CONSTRAINT uq UNIQUE (name));
         CREATE INDEX ix_name ON u(name);
         INSERT u (name) VALUES ('Smith'), ('smith'), ('SMITH'), ('Jones');",
    );
    // case-sensitive uniqueness: three spellings coexist, an exact repeat does not
    assert_eq!(err(&mut c, "INSERT u (name) VALUES ('smith')").number, 2627);
    exec(&mut c, "INSERT u (name) VALUES ('sMith')");
    // an index seek finds exactly the equal key, a range scan walks the collation's order
    assert_eq!(words(&mut c, "SELECT id FROM u WHERE name = 'smith'"), ["2"]);
    assert!(c.last_plan().join("\n").contains("index seek"), "{:?}", c.last_plan());
    assert_eq!(words(&mut c, "SELECT name FROM u WHERE name >= 'smith' ORDER BY name"), ["smith", "sMith", "Smith", "SMITH"]);
    assert_eq!(words(&mut c, "SELECT TOP 3 name FROM u ORDER BY name"), ["Jones", "smith", "sMith"]);
    // updating a key to another spelling is allowed, to an existing one is not
    exec(&mut c, "UPDATE u SET name = 'SmItH' WHERE name = 'sMith'");
    assert_eq!(err(&mut c, "UPDATE u SET name = 'smith' WHERE name = 'SmItH'").number, 2627);
    // the same table in a case-insensitive column refuses the second spelling
    script(&mut c, "CREATE TABLE u2 (name VARCHAR(30) PRIMARY KEY); INSERT u2 VALUES ('Smith');");
    assert_eq!(err(&mut c, "INSERT u2 VALUES ('smith')").number, 2627);
    // a unique index cannot be built over duplicates in its collation, but can over spellings that differ in it
    script(&mut c, "CREATE TABLE d (w VARCHAR(10) COLLATE Latin1_General_CS_AS); INSERT d VALUES ('a'), ('A');");
    exec(&mut c, "CREATE UNIQUE INDEX ux ON d(w)");
    script(&mut c, "CREATE TABLE d2 (w VARCHAR(10)); INSERT d2 VALUES ('a'), ('A');");
    assert_eq!(err(&mut c, "CREATE UNIQUE INDEX ux2 ON d2(w)").number, 1505);
}

#[test]
fn joins_and_foreign_keys_use_the_collation() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE p (code VARCHAR(10) COLLATE Latin1_General_CS_AS PRIMARY KEY);
         CREATE TABLE ch (id INT PRIMARY KEY, code VARCHAR(10) COLLATE Latin1_General_CS_AS REFERENCES p(code));
         INSERT p VALUES ('ab'), ('AB');
         INSERT ch VALUES (1, 'ab'), (2, 'AB');",
    );
    assert_eq!(err(&mut c, "INSERT ch VALUES (3, 'Ab')").number, 547, "'Ab' is no parent in a case-sensitive key");
    assert_eq!(rows(&mut c, "SELECT ch.id, p.code FROM ch JOIN p ON ch.code = p.code ORDER BY ch.id"), [["1", "ab"], ["2", "AB"]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ch JOIN p ON ch.code = p.code COLLATE Latin1_General_CI_AS"), "4");
    assert_eq!(err(&mut c, "DELETE p WHERE code = 'ab'").number, 547, "a child row references 'ab'");
    exec(&mut c, "DELETE ch WHERE id = 1; DELETE p WHERE code = 'ab'");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM p"), "1");
    // UNION, EXCEPT and INTERSECT compare in the column's collation
    script(&mut c, "CREATE TABLE s1 (w VARCHAR(5) COLLATE Latin1_General_CS_AS); CREATE TABLE s2 (w VARCHAR(5) COLLATE Latin1_General_CS_AS); INSERT s1 VALUES ('x'), ('X'); INSERT s2 VALUES ('x');");
    assert_eq!(words(&mut c, "SELECT w FROM s1 UNION SELECT w FROM s2 ORDER BY w"), ["x", "X"]);
    assert_eq!(words(&mut c, "SELECT w FROM s1 EXCEPT SELECT w FROM s2"), ["X"]);
}

#[test]
fn the_database_default_collation_applies_to_new_columns_and_literals() {
    let mut c = conn();
    script(&mut c, "CREATE DATABASE casey COLLATE Latin1_General_CS_AS;");
    assert_eq!(scalar(&mut c, "SELECT collation_name FROM sys.databases WHERE name = 'casey'"), "Latin1_General_CS_AS");
    assert_eq!(scalar(&mut c, "SELECT collation_name FROM sys.databases WHERE name = 'master'"), "SQL_Latin1_General_CP1_CI_AS");
    script(&mut c, "USE casey; CREATE TABLE t (w VARCHAR(10)); INSERT t VALUES ('a'), ('A');");
    assert_eq!(scalar(&mut c, "SELECT collation_name FROM sys.columns WHERE name = 'w' AND object_id = OBJECT_ID('t')"), "Latin1_General_CS_AS");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE w = 'a'"), "1");
    assert_eq!(scalar(&mut c, "SELECT CASE WHEN 'a' = 'A' THEN 'same' ELSE 'different' END"), "different", "literals use the database's collation");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE w COLLATE SQL_Latin1_General_CP1_CI_AS = 'a'"), "2");
    // a column may still choose its own, and changing the database default leaves existing columns alone
    script(&mut c, "CREATE TABLE t2 (w VARCHAR(10) COLLATE SQL_Latin1_General_CP1_CI_AS); INSERT t2 VALUES ('a'), ('A');");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t2 WHERE w = 'a'"), "2");
    exec(&mut c, "ALTER DATABASE casey COLLATE SQL_Latin1_General_CP1_CI_AS");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t WHERE w = 'a'"), "1", "the existing column keeps the collation it was created with");
    script(&mut c, "CREATE TABLE t3 (w VARCHAR(10)); INSERT t3 VALUES ('a'), ('A');");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t3 WHERE w = 'a'"), "2");
    assert_eq!(err(&mut c, "CREATE DATABASE bad COLLATE Nonsense_CI_AS").number, 448);
    assert_eq!(err(&mut c, "CREATE TABLE t4 (w VARCHAR(5) COLLATE Nonsense_CI_AS)").number, 448);
}

#[test]
fn altering_a_columns_collation_rebuilds_its_indexes() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE a (w VARCHAR(10) NOT NULL); CREATE INDEX ix ON a(w); INSERT a VALUES ('x'), ('X'), ('y');");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE w = 'x'"), "2");
    exec(&mut c, "ALTER TABLE a ALTER COLUMN w VARCHAR(10) COLLATE Latin1_General_CS_AS NOT NULL");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE w = 'x'"), "1");
    assert!(c.last_plan().join("\n").contains("index seek"), "{:?}", c.last_plan());
    assert_eq!(words(&mut c, "SELECT w FROM a ORDER BY w"), ["x", "X", "y"]);
    exec(&mut c, "ALTER TABLE a ALTER COLUMN w VARCHAR(10) COLLATE SQL_Latin1_General_CP1_CI_AS NOT NULL");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE w = 'x'"), "2");
}

#[test]
fn collations_travel_through_views_derived_tables_and_select_into() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "CREATE VIEW v AS SELECT id, w FROM cs");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM v WHERE w = 'apple'"), "2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM (SELECT w FROM cs) d WHERE d.w = 'apple'"), "2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM (SELECT UPPER(w) AS u FROM cs) d WHERE d.u = 'APPLE'"), "4", "UPPER keeps the column's collation but the values are now equal");
    exec(&mut c, "SELECT id, w INTO copy1 FROM cs");
    assert_eq!(scalar(&mut c, "SELECT collation_name FROM sys.columns WHERE name = 'w' AND object_id = OBJECT_ID('copy1')"), "Latin1_General_CS_AS");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM copy1 WHERE w = 'apple'"), "2");
    // a variable or literal takes the collation of the column it is compared with
    exec(&mut c, "DECLARE @x VARCHAR(10) = 'APPLE'; SELECT id FROM cs WHERE w = @x");
    assert_eq!(rows(&mut c, "DECLARE @x VARCHAR(10) = 'APPLE'; SELECT id FROM cs WHERE w = @x"), [["3"]]);
}

#[test]
fn the_streaming_executor_agrees_with_the_general_one_for_collated_columns() {
    let s = common::server();
    let dir = tempfile::tempdir().unwrap();
    let mut c = s.connect();
    script(&mut c, "CREATE TABLE big (id INT PRIMARY KEY, w VARCHAR(20) COLLATE Latin1_General_CS_AS, g VARCHAR(5) COLLATE Latin1_General_CS_AS);\nCREATE TABLE dim (g VARCHAR(5) COLLATE Latin1_General_CS_AS PRIMARY KEY, label VARCHAR(10));");
    let mut sql = String::new();
    for i in 0..3000 {
        let w = ["apple", "Apple", "APPLE", "banana", "Banana"][i % 5];
        let g = ["aa", "AA", "Aa", "bb"][i % 4];
        sql.push_str(&format!("INSERT big VALUES ({}, '{}{}', '{}');\n", i, w, i % 7, g));
    }
    sql.push_str("INSERT dim VALUES ('aa', 'lower'), ('AA', 'upper'), ('Aa', 'mixed'), ('bb', 'bees');");
    script(&mut c, &sql);
    for q in [
        "SELECT w, COUNT(*) AS n FROM big GROUP BY w ORDER BY w",
        "SELECT DISTINCT w FROM big ORDER BY w",
        "SELECT TOP 40 w, id FROM big ORDER BY w, id",
        "SELECT w, id FROM big ORDER BY w DESC, id",
        "SELECT b.g, d.label, COUNT(*) AS n FROM big b JOIN dim d ON b.g = d.g GROUP BY b.g, d.label ORDER BY b.g",
        "SELECT b.id, d.label FROM big b LEFT JOIN dim d ON b.g = d.g WHERE b.id < 50 ORDER BY b.id",
    ] {
        s.set_work_mem(4096, 0, Some(dir.path().to_path_buf()));
        let streamed = rows(&mut c, q);
        let plan = c.last_plan().join("\n");
        assert!(plan.contains("streaming"), "{q}: {plan}");
        s.set_work_mem(64 << 20, usize::MAX, Some(dir.path().to_path_buf()));
        let general = rows(&mut c, q);
        assert_eq!(streamed, general, "{q}");
    }
    // the case-sensitive groups really are separate
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT g) FROM big"), "4");
}

#[test]
fn collations_survive_a_restart_and_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Server::open(dir.path().join("data")).unwrap();
        let mut c = s.connect();
        script(&mut c, "CREATE DATABASE cdb COLLATE Latin1_General_CS_AS; USE cdb; CREATE TABLE t (id INT PRIMARY KEY, w VARCHAR(10) UNIQUE); INSERT t VALUES (1, 'a'), (2, 'A');");
        script(&mut c, "USE master; CREATE TABLE m (w VARCHAR(10) COLLATE Latin1_General_CS_AS PRIMARY KEY); INSERT m VALUES ('q'), ('Q');");
    }
    let s = Server::open(dir.path().join("data")).unwrap();
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM cdb.dbo.t WHERE w = 'a'"), "1");
    assert_eq!(err(&mut c, "INSERT cdb.dbo.t VALUES (3, 'a')").number, 2627);
    exec(&mut c, "INSERT cdb.dbo.t VALUES (3, 'b')");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM m WHERE w = 'q'"), "1");
    assert_eq!(err(&mut c, "INSERT m VALUES ('q')").number, 2627);
    // a checkpoint and a crash in between change nothing
    s.checkpoint();
    exec(&mut c, "INSERT m VALUES ('r')");
    drop(c);
    s.simulate_crash();
    let s = Server::open(dir.path().join("data")).unwrap();
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM m WHERE w = 'r'"), "1");
    assert_eq!(err(&mut c, "INSERT m VALUES ('Q')").number, 2627);
    // backup and restore carry the collations, including the index keys
    let f = dir.path().join("c.bak").to_string_lossy().replace('\'', "''");
    exec(&mut c, &format!("BACKUP DATABASE cdb TO DISK = '{}'", f));
    exec(&mut c, &format!("RESTORE DATABASE cdb2 FROM DISK = '{}'", f));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM cdb2.dbo.t WHERE w = 'a'"), "1");
    assert_eq!(err(&mut c, "INSERT cdb2.dbo.t VALUES (9, 'A')").number, 2627);
    exec(&mut c, "INSERT cdb2.dbo.t VALUES (9, 'z')");
    assert_eq!(scalar(&mut c, "SELECT collation_name FROM sys.databases WHERE name = 'cdb2'"), "Latin1_General_CS_AS");
}

#[test]
fn collation_metadata_functions_and_views() {
    let mut c = conn();
    script(&mut c, "CREATE DATABASE meta COLLATE French_CI_AI; USE meta; CREATE TABLE t (a VARCHAR(5), b INT, d NVARCHAR(5) COLLATE Latin1_General_BIN2);");
    assert_eq!(scalar(&mut c, "SELECT DATABASEPROPERTYEX('meta', 'Collation')"), "French_CI_AI");
    assert_eq!(scalar(&mut c, "SELECT DATABASEPROPERTYEX('master', 'Collation')"), "SQL_Latin1_General_CP1_CI_AS");
    assert_eq!(scalar(&mut c, "SELECT SERVERPROPERTY('Collation')"), "SQL_Latin1_General_CP1_CI_AS");
    assert_eq!(rows(&mut c, "SELECT COLUMN_NAME, COLLATION_NAME FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = 't' ORDER BY ORDINAL_POSITION"), [["a", "French_CI_AI"], ["b", "NULL"], ["d", "Latin1_General_BIN2"]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.fn_helpcollations() WHERE name = 'Latin1_General_CS_AS'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COLLATIONPROPERTY('Latin1_General_CS_AS', 'CodePage')"), "1252");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.fn_helpcollations() WHERE name LIKE '%BIN2'"), "6");
}
