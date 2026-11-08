mod common;
use common::*;

#[test]
fn insert_forms() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT, b VARCHAR(10) DEFAULT 'dflt', c INT NULL)");
    exec(&mut c, "INSERT t (a) VALUES (1)");
    exec(&mut c, "INSERT t VALUES (2, 'x', 3), (3, DEFAULT, NULL)");
    exec(&mut c, "INSERT t (a, b) SELECT a + 10, b FROM t WHERE a < 3");
    exec(&mut c, "INSERT INTO t DEFAULT VALUES");
    assert_eq!(rows(&mut c, "SELECT a, b, c FROM t ORDER BY a"), vec![
        vec!["NULL", "dflt", "NULL"], vec!["1", "dflt", "NULL"], vec!["2", "x", "3"], vec!["3", "dflt", "NULL"], vec!["11", "dflt", "NULL"], vec!["12", "x", "NULL"]]);
    assert_eq!(err_no(&mut c, "INSERT t VALUES (1)"), 213);
    assert_eq!(err_no(&mut c, "INSERT t (a, b) VALUES (1)"), 109);
    assert_eq!(err_no(&mut c, "INSERT t (a) VALUES (1, 2)"), 110);
    assert_eq!(err_no(&mut c, "INSERT t (zzz) VALUES (1)"), 207);
    assert_eq!(err_no(&mut c, "INSERT nope VALUES (1)"), 208);
}

#[test]
fn insert_is_atomic_per_statement() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT PRIMARY KEY)");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (1),(2),(1)"), 2627);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "0");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (1),(NULL)"), 515);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "0");
}

#[test]
fn identity_columns() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT IDENTITY(10, 5) PRIMARY KEY, v INT)");
    exec(&mut c, "INSERT t (v) VALUES (1),(2)");
    assert_eq!(rows(&mut c, "SELECT id FROM t ORDER BY id"), vec![vec!["10"], vec!["15"]]);
    assert_eq!(scalar(&mut c, "SELECT SCOPE_IDENTITY()"), "15");
    assert_eq!(scalar(&mut c, "SELECT @@IDENTITY"), "15");
    assert_eq!(scalar(&mut c, "SELECT IDENT_CURRENT('t')"), "15");
    assert_eq!(err_no(&mut c, "INSERT t (id, v) VALUES (1, 1)"), 544);
    exec(&mut c, "SET IDENTITY_INSERT t ON");
    exec(&mut c, "INSERT t (id, v) VALUES (100, 3)");
    exec(&mut c, "SET IDENTITY_INSERT t OFF");
    exec(&mut c, "INSERT t (v) VALUES (4)");
    assert_eq!(scalar(&mut c, "SELECT MAX(id) FROM t"), "105");
    assert_eq!(err_no(&mut c, "UPDATE t SET id = 5"), 8102);
    exec(&mut c, "DELETE FROM t");
    exec(&mut c, "INSERT t (v) VALUES (5)");
    assert_eq!(scalar(&mut c, "SELECT id FROM t"), "110");
    exec(&mut c, "TRUNCATE TABLE t");
    exec(&mut c, "INSERT t (v) VALUES (6)");
    assert_eq!(scalar(&mut c, "SELECT id FROM t"), "10");
    exec(&mut c, "DBCC CHECKIDENT('t', RESEED, 99)");
    exec(&mut c, "INSERT t (v) VALUES (7)");
    assert_eq!(scalar(&mut c, "SELECT MAX(id) FROM t"), "104");
}

#[test]
fn update_forms() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE a (id INT PRIMARY KEY, v INT, tag VARCHAR(5))");
    exec(&mut c, "CREATE TABLE b (id INT, nv INT)");
    exec(&mut c, "INSERT a VALUES (1,10,'x'),(2,20,'y'),(3,30,'z')");
    exec(&mut c, "INSERT b VALUES (1, 100), (3, 300)");
    exec(&mut c, "UPDATE a SET v = v + 1 WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT v FROM a WHERE id = 1"), "11");
    exec(&mut c, "UPDATE a SET v = b.nv FROM a JOIN b ON a.id = b.id");
    assert_eq!(rows(&mut c, "SELECT v FROM a ORDER BY id"), vec![vec!["100"], vec!["20"], vec!["300"]]);
    exec(&mut c, "UPDATE x SET x.tag = 'q' FROM a x WHERE x.id = 2");
    assert_eq!(scalar(&mut c, "SELECT tag FROM a WHERE id = 2"), "q");
    exec(&mut c, "UPDATE TOP (1) a SET tag = 'top'");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a WHERE tag = 'top'"), "1");
    // swap semantics: both RHS use old values
    exec(&mut c, "CREATE TABLE s (x INT, y INT)");
    exec(&mut c, "INSERT s VALUES (1, 2)");
    exec(&mut c, "UPDATE s SET x = y, y = x");
    assert_eq!(rows(&mut c, "SELECT x, y FROM s"), vec![vec!["2", "1"]]);
    exec(&mut c, "UPDATE a SET v += 5, tag = NULL WHERE id = 3");
    assert_eq!(rows(&mut c, "SELECT v, tag FROM a WHERE id = 3"), vec![vec!["305", "NULL"]]);
    // variable assignment inside UPDATE
    exec(&mut c, "DECLARE @sum INT = 0; UPDATE a SET @sum = @sum + v; SELECT @sum");
    assert_eq!(err_no(&mut c, "UPDATE a SET id = 1 WHERE id = 2"), 2627);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM a"), vec![vec!["3"]]);
}

#[test]
fn delete_forms() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE a (id INT, v INT)");
    exec(&mut c, "INSERT a VALUES (1,1),(2,2),(3,3),(4,4)");
    exec(&mut c, "DELETE FROM a WHERE id = 1");
    exec(&mut c, "DELETE TOP (1) FROM a");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a"), "2");
    exec(&mut c, "CREATE TABLE d (id INT)");
    exec(&mut c, "INSERT d VALUES (3)");
    exec(&mut c, "DELETE a FROM a JOIN d ON a.id = d.id");
    assert_eq!(rows(&mut c, "SELECT id FROM a"), vec![vec!["4"]]);
    let r = exec(&mut c, "DELETE FROM a OUTPUT deleted.id");
    assert_eq!(r.resultsets()[0].rows[0][0].to_string(), "4");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM a"), "0");
}

#[test]
fn rowcount_reporting() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE a (id INT)");
    exec(&mut c, "INSERT a VALUES (1),(2),(3)");
    assert_eq!(scalar(&mut c, "SELECT @@ROWCOUNT"), "3");
    exec(&mut c, "UPDATE a SET id = id + 1 WHERE id > 1");
    assert_eq!(scalar(&mut c, "SELECT @@ROWCOUNT"), "2");
    exec(&mut c, "DELETE a WHERE id = 99");
    assert_eq!(scalar(&mut c, "SELECT @@ROWCOUNT"), "0");
    let r = exec(&mut c, "INSERT a VALUES (7)");
    assert!(r.events.iter().any(|e| matches!(e, tsql_server::Event::RowCount(1))));
    let r = exec(&mut c, "SET NOCOUNT ON; INSERT a VALUES (8)");
    assert!(!r.events.iter().any(|e| matches!(e, tsql_server::Event::RowCount(_))));
}

#[test]
fn primary_key_unique_and_checks() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT CONSTRAINT pk_t PRIMARY KEY, email VARCHAR(30) CONSTRAINT uq_e UNIQUE, age INT CONSTRAINT ck_age CHECK (age BETWEEN 0 AND 150))");
    exec(&mut c, "INSERT t VALUES (1, 'a@x', 30)");
    let e = err(&mut c, "INSERT t VALUES (1, 'b@x', 30)");
    assert_eq!(e.number, 2627);
    assert_eq!(e.message, "Violation of PRIMARY KEY constraint 'pk_t'. Cannot insert duplicate key in object 'dbo.t'. The duplicate key value is (1).");
    let e = err(&mut c, "INSERT t VALUES (2, 'a@x', 30)");
    assert_eq!(e.message, "Violation of UNIQUE KEY constraint 'uq_e'. Cannot insert duplicate key in object 'dbo.t'. The duplicate key value is (a@x).");
    let e = err(&mut c, "INSERT t VALUES (3, 'c@x', 200)");
    assert_eq!(e.number, 547);
    assert!(e.message.contains("CHECK constraint \"ck_age\""), "{}", e.message);
    // NULLs: only one allowed in UNIQUE column in SQL Server
    exec(&mut c, "INSERT t VALUES (4, NULL, NULL)");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (5, NULL, 1)"), 2627);
    // NULL in check passes
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "2");
    // case-insensitive uniqueness
    assert_eq!(err_no(&mut c, "INSERT t VALUES (6, 'A@X', 1)"), 2627);
}

#[test]
fn composite_keys_and_unique_index() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT, b INT, c INT, PRIMARY KEY (a, b))");
    exec(&mut c, "INSERT t VALUES (1,1,1),(1,2,2)");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (1,1,9)"), 2627);
    exec(&mut c, "CREATE UNIQUE INDEX ux ON t (c)");
    let e = err(&mut c, "INSERT t VALUES (2,2,1)");
    assert_eq!(e.number, 2601);
    assert!(e.message.contains("unique index 'ux'"), "{}", e.message);
    assert_eq!(err_no(&mut c, "CREATE UNIQUE INDEX ux2 ON t (a)"), 1505);
    exec(&mut c, "DROP INDEX ux ON t");
    exec(&mut c, "INSERT t VALUES (2,2,1)");
}

#[test]
fn foreign_keys_and_cascades() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE p (id INT PRIMARY KEY)");
    exec(&mut c, "CREATE TABLE ch (id INT PRIMARY KEY, pid INT, CONSTRAINT fk_ch FOREIGN KEY (pid) REFERENCES p(id) ON DELETE CASCADE ON UPDATE CASCADE)");
    exec(&mut c, "CREATE TABLE sn (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE SET NULL)");
    exec(&mut c, "CREATE TABLE na (id INT PRIMARY KEY, pid INT REFERENCES p(id))");
    exec(&mut c, "INSERT p VALUES (1),(2),(3)");
    exec(&mut c, "INSERT ch VALUES (10,1),(11,1),(12,2)");
    exec(&mut c, "INSERT sn VALUES (20,2)");
    exec(&mut c, "INSERT na VALUES (30,3)");
    let e = err(&mut c, "INSERT ch VALUES (13, 99)");
    assert_eq!(e.number, 547);
    assert!(e.message.contains("FOREIGN KEY constraint \"fk_ch\""), "{}", e.message);
    exec(&mut c, "UPDATE p SET id = 100 WHERE id = 1");
    assert_eq!(rows(&mut c, "SELECT pid FROM ch WHERE id = 10"), vec![vec!["100"]]);
    exec(&mut c, "DELETE p WHERE id = 2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ch"), "2");
    assert_eq!(rows(&mut c, "SELECT pid FROM sn"), vec![vec!["NULL"]]);
    let e = err(&mut c, "DELETE p WHERE id = 3");
    assert_eq!(e.number, 547);
    assert!(e.message.contains("REFERENCE constraint"), "{}", e.message);
    assert_eq!(err_no(&mut c, "DROP TABLE p"), 3726);
    assert_eq!(err_no(&mut c, "TRUNCATE TABLE p"), 4712);
    NULLS_ALLOWED(&mut c);
}

#[allow(non_snake_case)]
fn NULLS_ALLOWED(c: &mut tsql_server::Connection) {
    exec(c, "INSERT na VALUES (31, NULL)");
}

#[test]
fn self_referencing_fk() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE e (id INT PRIMARY KEY, boss INT REFERENCES e(id) ON DELETE CASCADE)");
    exec(&mut c, "INSERT e VALUES (1, NULL)");
    exec(&mut c, "INSERT e VALUES (2, 1), (3, 2), (4, 3)");
    assert_eq!(err_no(&mut c, "INSERT e VALUES (5, 99)"), 547);
    exec(&mut c, "DELETE e WHERE id = 2");
    assert_eq!(rows(&mut c, "SELECT id FROM e"), vec![vec!["1"]]);
}

#[test]
fn computed_and_default_columns() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT, b INT, total AS a + b, d DATETIME DEFAULT GETDATE(), flag BIT NOT NULL DEFAULT 0)");
    exec(&mut c, "INSERT t (a, b) VALUES (2, 3)");
    assert_eq!(rows(&mut c, "SELECT total, flag, CASE WHEN d IS NOT NULL THEN 1 END FROM t"), vec![vec!["5", "0", "1"]]);
    exec(&mut c, "UPDATE t SET a = 10");
    assert_eq!(scalar(&mut c, "SELECT total FROM t"), "13");
    assert_eq!(err_no(&mut c, "INSERT t (a, b, total) VALUES (1, 1, 1)"), 271);
}

#[test]
fn output_clause() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT IDENTITY, v VARCHAR(5))");
    let r = exec(&mut c, "INSERT t (v) OUTPUT inserted.id, inserted.v VALUES ('a'), ('b')");
    assert_eq!(r.resultsets()[0].rows.len(), 2);
    assert_eq!(r.resultsets()[0].rows[1][0].to_string(), "2");
    exec(&mut c, "CREATE TABLE log (id INT, old_v VARCHAR(5), new_v VARCHAR(5))");
    exec(&mut c, "UPDATE t SET v = 'z' OUTPUT inserted.id, deleted.v, inserted.v INTO log");
    assert_eq!(rows(&mut c, "SELECT old_v, new_v FROM log ORDER BY id"), vec![vec!["a", "z"], vec!["b", "z"]]);
    let r = exec(&mut c, "DELETE FROM t OUTPUT deleted.*");
    assert_eq!(r.resultsets()[0].rows.len(), 2);
}

#[test]
fn merge_statement() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE tgt (id INT PRIMARY KEY, v VARCHAR(10))");
    exec(&mut c, "CREATE TABLE src (id INT, v VARCHAR(10))");
    exec(&mut c, "INSERT tgt VALUES (1,'old1'),(2,'old2'),(3,'old3')");
    exec(&mut c, "INSERT src VALUES (2,'new2'),(3,'new3'),(4,'new4')");
    let r = exec(&mut c, "MERGE tgt t USING src s ON t.id = s.id
        WHEN MATCHED AND s.id = 2 THEN UPDATE SET v = s.v
        WHEN MATCHED THEN DELETE
        WHEN NOT MATCHED BY TARGET THEN INSERT (id, v) VALUES (s.id, s.v)
        WHEN NOT MATCHED BY SOURCE THEN UPDATE SET v = 'orphan'
        OUTPUT $action, inserted.id, deleted.id;");
    assert_eq!(r.resultsets()[0].rows.len(), 4);
    assert_eq!(rows(&mut c, "SELECT id, v FROM tgt ORDER BY id"), vec![vec!["1", "orphan"], vec!["2", "new2"], vec!["4", "new4"]]);
    exec(&mut c, "INSERT src VALUES (2, 'dup')");
    assert_eq!(err_no(&mut c, "MERGE tgt t USING src s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v;"), 8672);
}

#[test]
fn triggers() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    exec(&mut c, "CREATE TABLE audit (op VARCHAR(10), id INT, old_v INT, new_v INT)");
    script(&mut c, "CREATE TRIGGER trg_ins ON t AFTER INSERT AS INSERT audit SELECT 'ins', id, NULL, v FROM inserted
GO
CREATE TRIGGER trg_upd ON t AFTER UPDATE AS INSERT audit SELECT 'upd', i.id, d.v, i.v FROM inserted i JOIN deleted d ON i.id = d.id
GO
CREATE TRIGGER trg_del ON t AFTER DELETE AS INSERT audit SELECT 'del', id, v, NULL FROM deleted
GO");
    exec(&mut c, "INSERT t VALUES (1, 10), (2, 20)");
    exec(&mut c, "UPDATE t SET v = v + 1 WHERE id = 1");
    exec(&mut c, "DELETE t WHERE id = 2");
    assert_eq!(rows(&mut c, "SELECT op, id, old_v, new_v FROM audit ORDER BY op, id"), vec![
        vec!["del", "2", "20", "NULL"], vec!["ins", "1", "NULL", "10"], vec!["ins", "2", "NULL", "20"], vec!["upd", "1", "10", "11"]]);
}

#[test]
fn trigger_rollback_and_instead_of() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT, v INT)");
    script(&mut c, "CREATE TRIGGER trg_chk ON t AFTER INSERT AS IF EXISTS (SELECT 1 FROM inserted WHERE v < 0) THROW 50001, 'negative!', 1
GO");
    exec(&mut c, "INSERT t VALUES (1, 1)");
    let e = err(&mut c, "INSERT t VALUES (2, -1)");
    assert_eq!(e.number, 50001);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "1");
    exec(&mut c, "CREATE TABLE s (id INT, v INT)");
    script(&mut c, "CREATE TRIGGER trg_inst ON s INSTEAD OF INSERT AS INSERT s SELECT 100 + id, v FROM inserted WHERE 1=0
GO");
    exec(&mut c, "INSERT s VALUES (1, 1)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM s"), "0");
}

#[test]
fn table_variables_and_temp_tables() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE #t (a INT, b INT)");
    exec(&mut c, "INSERT #t VALUES (1, 2)");
    assert_eq!(scalar(&mut c, "SELECT a + b FROM #t"), "3");
    assert_eq!(err_no(&mut c, "CREATE TABLE #t (a INT)"), 2714);
    exec(&mut c, "DROP TABLE #t");
    assert_eq!(err_no(&mut c, "SELECT * FROM #t"), 208);
    assert_eq!(rows(&mut c, "DECLARE @v TABLE (id INT PRIMARY KEY, s VARCHAR(5)); INSERT @v VALUES (1,'a'),(2,'b'); UPDATE @v SET s = 'z' WHERE id = 2; DELETE @v WHERE id = 1; SELECT * FROM @v"), vec![vec!["2", "z"]]);
    exec(&mut c, "CREATE TABLE ##g (a INT)");
    let mut c2 = {
        let _ = &c;
        conn()
    };
    let _ = &mut c2;
    exec(&mut c, "INSERT ##g VALUES (1)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ##g"), "1");
}

#[test]
fn truncate_and_drop() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (a INT)");
    exec(&mut c, "INSERT t VALUES (1),(2)");
    exec(&mut c, "TRUNCATE TABLE t");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "0");
    exec(&mut c, "DROP TABLE t");
    assert_eq!(err_no(&mut c, "DROP TABLE t"), 3701);
    exec(&mut c, "DROP TABLE IF EXISTS t");
}
