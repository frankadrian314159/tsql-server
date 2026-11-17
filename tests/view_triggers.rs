//! INSTEAD OF triggers on views: the statement on the view runs the trigger with the rows it would have written, shaped as the
//! view's rows, and changes nothing itself.
mod common;
use common::*;
use tsql_server::Connection;

/// A view over a join (which cannot be written through without a trigger), and a log the triggers write to.
fn shop() -> Connection {
    let mut c = conn();
    script(
        &mut c,
        "
CREATE TABLE cust (id INT PRIMARY KEY, name VARCHAR(20))
CREATE TABLE ord (id INT PRIMARY KEY, cust_id INT, amount INT NULL)
CREATE TABLE log (n INT IDENTITY PRIMARY KEY, msg VARCHAR(100))
INSERT cust VALUES (1, 'ann'), (2, 'bob')
INSERT ord VALUES (10, 1, 5), (11, 1, 7), (12, 2, 9)
GO
CREATE VIEW v_ord AS SELECT o.id, c.name, o.amount FROM ord o JOIN cust c ON c.id = o.cust_id
GO
",
    );
    c
}

fn orders(c: &mut Connection) -> Vec<Vec<String>> {
    rows(c, "SELECT id, cust_id, ISNULL(CAST(amount AS VARCHAR(10)), 'NULL') FROM ord ORDER BY id")
}

fn log(c: &mut Connection) -> Vec<String> {
    rows(c, "SELECT msg FROM log ORDER BY n").into_iter().map(|r| r[0].clone()).collect()
}

fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
}

#[test]
fn an_insert_into_a_view_runs_its_instead_of_trigger() {
    let mut c = shop();
    script(
        &mut c,
        "CREATE TRIGGER v_ord_ins ON v_ord INSTEAD OF INSERT AS
BEGIN
    INSERT ord SELECT i.id, cu.id, i.amount FROM inserted i JOIN cust cu ON cu.name = i.name
    INSERT log (msg) SELECT CONCAT('ins ', id, ' ', name, ' ', ISNULL(CAST(amount AS VARCHAR(10)), 'NULL')) FROM inserted
END
GO",
    );
    exec(&mut c, "INSERT v_ord VALUES (13, 'bob', 4)");
    // a column list in another order, and a column it leaves out (NULL: a view has no defaults)
    exec(&mut c, "INSERT v_ord (name, id) VALUES ('ann', 14)");
    // from a query, several rows
    exec(&mut c, "INSERT v_ord SELECT id + 100, name, amount FROM v_ord WHERE id IN (10, 11)");
    assert_eq!(
        orders(&mut c),
        table(&[&["10", "1", "5"], &["11", "1", "7"], &["12", "2", "9"], &["13", "2", "4"], &["14", "1", "NULL"], &["110", "1", "5"], &["111", "1", "7"]])
    );
    assert_eq!(log(&mut c)[..2], ["ins 13 bob 4", "ins 14 ann NULL"]);
    // the column list is checked against the view's columns
    assert_eq!(err_no(&mut c, "INSERT v_ord (id, nope) VALUES (1, 2)"), 207);
    assert_eq!(err_no(&mut c, "INSERT v_ord VALUES (1, 'ann')"), 213);
    // OUTPUT gives the rows as if the insert had been carried out
    let out = rows(&mut c, "INSERT v_ord OUTPUT inserted.id, inserted.name VALUES (15, 'ann', 1)");
    assert_eq!(out, table(&[&["15", "ann"]]));
}

#[test]
fn an_update_of_a_view_hands_old_and_new_rows_to_the_trigger() {
    let mut c = shop();
    script(
        &mut c,
        "CREATE TRIGGER v_ord_upd ON v_ord INSTEAD OF UPDATE AS
BEGIN
    UPDATE o SET amount = i.amount FROM ord o JOIN inserted i ON i.id = o.id
    INSERT log (msg) SELECT CONCAT('upd ', d.id, ' ', d.amount, '->', i.amount, CASE WHEN UPDATE(name) THEN ' name' ELSE '' END)
        FROM deleted d JOIN inserted i ON i.id = d.id
END
GO",
    );
    exec(&mut c, "UPDATE v_ord SET amount = amount + 1 WHERE name = 'ann'");
    assert_eq!(orders(&mut c), table(&[&["10", "1", "6"], &["11", "1", "8"], &["12", "2", "9"]]));
    let mut l = log(&mut c);
    l.sort();
    assert_eq!(l, ["upd 10 5->6", "upd 11 7->8"]);
    // a compound assignment, and UPDATE(col) for a column the SET names
    exec(&mut c, "UPDATE v_ord SET amount += 10, name = name WHERE id = 12");
    assert_eq!(orders(&mut c)[2], ["12", "2", "19"]);
    assert_eq!(log(&mut c).last().unwrap(), "upd 12 9->19 name");
    // a FROM that names the view by an alias, joined to a row that matches twice: each view row is updated once
    exec(&mut c, "UPDATE v SET amount = 0 FROM v_ord v JOIN ord o2 ON o2.cust_id = 1 WHERE v.name = 'ann'");
    assert_eq!(orders(&mut c), table(&[&["10", "1", "0"], &["11", "1", "0"], &["12", "2", "19"]]));
    assert_eq!(log(&mut c).len(), 5, "two rows, not four");
    // nothing matched: the trigger runs with no rows
    exec(&mut c, "UPDATE v_ord SET amount = 1 WHERE id = 999");
    assert_eq!(log(&mut c).len(), 5);
}

#[test]
fn a_delete_from_a_view_hands_the_rows_to_the_trigger() {
    let mut c = shop();
    exec(&mut c, "CREATE TRIGGER v_ord_del ON v_ord INSTEAD OF DELETE AS DELETE ord WHERE id IN (SELECT id FROM deleted)");
    exec(&mut c, "DELETE v_ord WHERE name = 'bob'");
    assert_eq!(orders(&mut c), table(&[&["10", "1", "5"], &["11", "1", "7"]]));
    exec(&mut c, "DELETE TOP (1) FROM v_ord");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord"), "1");
    let out = rows(&mut c, "DELETE v_ord OUTPUT deleted.name");
    assert_eq!(out, table(&[&["ann"]]));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord"), "0");
}

#[test]
fn view_triggers_follow_the_rules_of_ddl() {
    let mut c = shop();
    // a view takes INSTEAD OF triggers only, one per statement
    assert_eq!(err_no(&mut c, "CREATE TRIGGER a ON v_ord AFTER INSERT AS SELECT 1"), 8197);
    exec(&mut c, "CREATE TRIGGER t1 ON v_ord INSTEAD OF INSERT AS INSERT ord SELECT id, 1, amount FROM inserted");
    assert_eq!(err_no(&mut c, "CREATE TRIGGER t2 ON v_ord INSTEAD OF INSERT, DELETE AS SELECT 1"), 2111);
    exec(&mut c, "CREATE OR ALTER TRIGGER t1 ON v_ord INSTEAD OF INSERT AS INSERT ord SELECT id, 2, amount FROM inserted");
    assert_eq!(
        rows(&mut c, "SELECT name, CAST(is_instead_of_trigger AS INT), CASE WHEN parent_id = OBJECT_ID('v_ord') THEN 'view' ELSE 'other' END FROM sys.triggers"),
        table(&[&["t1", "1", "view"]])
    );
    exec(&mut c, "INSERT v_ord VALUES (20, 'x', 1)");
    assert_eq!(scalar(&mut c, "SELECT cust_id FROM ord WHERE id = 20"), "2", "the altered body runs");
    // a disabled trigger does not fire: the view, a join, cannot be written through
    exec(&mut c, "DISABLE TRIGGER t1 ON v_ord");
    assert!(c.run("INSERT v_ord VALUES (21, 'x', 1)").is_err());
    exec(&mut c, "ENABLE TRIGGER t1 ON v_ord");
    exec(&mut c, "INSERT v_ord VALUES (21, 'x', 1)");
    // ALTER VIEW keeps the trigger; DROP TRIGGER and DROP VIEW remove it
    exec(&mut c, "ALTER VIEW v_ord AS SELECT o.id, c.name, o.amount FROM ord o JOIN cust c ON c.id = o.cust_id WHERE o.amount IS NOT NULL");
    exec(&mut c, "INSERT v_ord VALUES (22, 'x', 1)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE id >= 20"), "3");
    exec(&mut c, "DROP TRIGGER t1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.triggers"), "0");
    exec(&mut c, "CREATE TRIGGER t3 ON v_ord INSTEAD OF DELETE AS SELECT 1");
    exec(&mut c, "DROP VIEW v_ord");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.triggers"), "0");
}

#[test]
fn a_statement_on_the_view_inside_its_own_trigger_goes_to_the_base_table() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE b (id INT PRIMARY KEY, v INT)
GO
CREATE VIEW vb AS SELECT id, v FROM b
GO
CREATE TRIGGER vb_ins ON vb INSTEAD OF INSERT AS INSERT vb SELECT id, v * 10 FROM inserted
GO",
    );
    exec(&mut c, "INSERT vb VALUES (1, 2), (2, 3)");
    assert_eq!(rows(&mut c, "SELECT id, v FROM b ORDER BY id"), table(&[&["1", "20"], &["2", "30"]]));
    // and a rollback of the statement's transaction undoes what the trigger wrote
    exec(&mut c, "BEGIN TRAN; INSERT vb VALUES (3, 4); ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM b"), "2");
}

#[test]
fn update_of_a_column_is_not_checked_against_the_from_of_its_query() {
    // UPDATE(name) names a column of the trigger's table; `deleted` and `inserted` both have `name`, and joining them must not make it ambiguous
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20))
CREATE TABLE lg (msg VARCHAR(100))
INSERT t VALUES (1, 'ann')
GO
CREATE TRIGGER tg ON t AFTER UPDATE AS INSERT lg SELECT CONCAT('u ', d.id, CASE WHEN UPDATE(name) THEN ' name' ELSE '' END) FROM deleted d JOIN inserted i ON i.id = d.id
GO",
    );
    exec(&mut c, "UPDATE t SET name = 'x'");
    assert_eq!(scalar(&mut c, "SELECT msg FROM lg"), "u 1 name");
}
