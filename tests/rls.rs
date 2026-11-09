//! Row-level security: filter and block predicates in security policies.
mod common;
use common::*;
use tsql_server::{Connection, Server};

fn setup() -> (Server, Connection) {
    let s = server();
    let mut sa = s.connect();
    script(
        &mut sa,
        "CREATE TABLE orders (id INT PRIMARY KEY, owner VARCHAR(20), amount INT)
         INSERT orders VALUES (1, 'ann', 10), (2, 'ann', 20), (3, 'bob', 30), (4, 'bob', 40), (5, 'cat', 50)
         CREATE LOGIN ann WITH PASSWORD = 'a'
         CREATE LOGIN bob WITH PASSWORD = 'b'
         CREATE USER ann FOR LOGIN ann
         CREATE USER bob FOR LOGIN bob
         GRANT SELECT, INSERT, UPDATE, DELETE ON orders TO ann
         GRANT SELECT, INSERT, UPDATE, DELETE ON orders TO bob
GO
         CREATE FUNCTION dbo.only_mine(@owner VARCHAR(20)) RETURNS TABLE AS RETURN SELECT 1 AS ok WHERE @owner = USER_NAME() OR USER_NAME() = 'dbo'",
    );
    (s, sa)
}

fn user(s: &Server, name: &str, pw: &str) -> Connection {
    let mut c = s.connect_as(name, pw).unwrap();
    exec(&mut c, "USE master");
    c
}

#[test]
fn a_filter_predicate_hides_rows_from_reads_updates_and_deletes() {
    let (s, mut sa) = setup();
    exec(&mut sa, "CREATE SECURITY POLICY mine ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders WITH (STATE = ON)");
    let mut ann = user(&s, "ann", "a");
    assert_eq!(rows(&mut ann, "SELECT id FROM orders ORDER BY id"), [["1"], ["2"]]);
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "2");
    assert_eq!(scalar(&mut ann, "SELECT SUM(amount) FROM orders"), "30");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders WHERE owner = 'bob'"), "0", "other people's rows cannot be probed");
    // updates and deletes only reach visible rows
    exec(&mut ann, "UPDATE orders SET amount = 0");
    exec(&mut ann, "DELETE orders WHERE id = 3");
    assert_eq!(rows(&mut sa, "SELECT id, amount FROM orders ORDER BY id"), [["1", "0"], ["2", "0"], ["3", "30"], ["4", "40"], ["5", "50"]], "bob's rows are untouched");
    // joins, subqueries and index lookups go through the same filter
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders a JOIN orders b ON a.id = b.id"), "2");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders WHERE id IN (SELECT id FROM orders WHERE id > 0)"), "2");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders WHERE id = 4"), "0");
    let mut bob = user(&s, "bob", "b");
    assert_eq!(rows(&mut bob, "SELECT id FROM orders ORDER BY id"), [["3"], ["4"]]);
    // the predicate function sees dbo as everyone-allowed in this test
    assert_eq!(scalar(&mut sa, "SELECT COUNT(*) FROM orders"), "5");
}

#[test]
fn block_predicates_stop_writes_that_break_the_rule() {
    let (s, mut sa) = setup();
    exec(
        &mut sa,
        "CREATE SECURITY POLICY mine
            ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders,
            ADD BLOCK PREDICATE dbo.only_mine(owner) ON dbo.orders AFTER INSERT,
            ADD BLOCK PREDICATE dbo.only_mine(owner) ON dbo.orders AFTER UPDATE,
            ADD BLOCK PREDICATE dbo.only_mine(owner) ON dbo.orders BEFORE DELETE",
    );
    let mut ann = user(&s, "ann", "a");
    exec(&mut ann, "INSERT orders VALUES (10, 'ann', 1)");
    assert_eq!(err(&mut ann, "INSERT orders VALUES (11, 'bob', 1)").number, 33504, "ann cannot create a row she could not see");
    assert_eq!(err(&mut ann, "UPDATE orders SET owner = 'bob' WHERE id = 1").number, 33504, "or hand a row to someone else");
    exec(&mut ann, "UPDATE orders SET amount = 5 WHERE id = 1");
    exec(&mut ann, "DELETE orders WHERE id = 10");
    assert_eq!(scalar(&mut sa, "SELECT COUNT(*) FROM orders WHERE id IN (10, 11)"), "0");
    assert_eq!(scalar(&mut sa, "SELECT owner FROM orders WHERE id = 1"), "ann");
}

#[test]
fn a_bare_block_predicate_covers_every_operation_and_before_update_checks_the_old_row() {
    let (s, mut sa) = setup();
    exec(&mut sa, "CREATE SECURITY POLICY blockall ADD BLOCK PREDICATE dbo.only_mine(owner) ON dbo.orders");
    let mut ann = user(&s, "ann", "a");
    // no filter: ann still sees everything, but can write only her own rows
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "5");
    assert_eq!(err(&mut ann, "UPDATE orders SET amount = 1 WHERE id = 3").number, 33504, "bob's row: the BEFORE UPDATE check fails");
    assert_eq!(err(&mut ann, "DELETE orders WHERE id = 4").number, 33504);
    assert_eq!(err(&mut ann, "INSERT orders VALUES (12, 'cat', 1)").number, 33504);
    exec(&mut ann, "UPDATE orders SET amount = 99 WHERE id = 2");
}

#[test]
fn policies_can_be_switched_altered_and_dropped() {
    let (s, mut sa) = setup();
    exec(&mut sa, "CREATE SECURITY POLICY mine ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders WITH (STATE = OFF)");
    let mut ann = user(&s, "ann", "a");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "5", "a disabled policy does nothing");
    exec(&mut sa, "ALTER SECURITY POLICY mine WITH (STATE = ON)");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "2");
    assert_eq!(scalar(&mut sa, "SELECT is_enabled FROM sys.security_policies WHERE name = 'mine'"), "1");
    let preds = rows(&mut sa, "SELECT predicate_type_desc, predicate_definition FROM sys.security_predicates");
    assert_eq!(preds, [["FILTER", "[dbo].[only_mine](owner)"]]);
    exec(&mut sa, "ALTER SECURITY POLICY mine DROP FILTER PREDICATE ON dbo.orders");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "5");
    exec(&mut sa, "ALTER SECURITY POLICY mine ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "2");
    exec(&mut sa, "DROP SECURITY POLICY mine");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "5");
    exec(&mut sa, "DROP SECURITY POLICY IF EXISTS mine");
    assert_eq!(err(&mut sa, "DROP SECURITY POLICY mine").number, 3701);
}

#[test]
fn policies_are_validated_and_only_admins_manage_them() {
    let (s, mut sa) = setup();
    assert_eq!(err(&mut sa, "CREATE SECURITY POLICY p ADD FILTER PREDICATE dbo.no_such(owner) ON dbo.orders").number, 33262);
    assert_eq!(err(&mut sa, "CREATE SECURITY POLICY p ADD FILTER PREDICATE dbo.only_mine(nope) ON dbo.orders").number, 207);
    assert_eq!(err(&mut sa, "CREATE SECURITY POLICY p ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.no_table").number, 208);
    exec(&mut sa, "CREATE SECURITY POLICY p ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders");
    assert_eq!(err(&mut sa, "CREATE SECURITY POLICY p2 ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders").number, 33265, "one filter predicate per table");
    assert_eq!(err(&mut sa, "CREATE SECURITY POLICY p ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders").number, 2714);
    let mut ann = user(&s, "ann", "a");
    assert_eq!(err(&mut ann, "DROP SECURITY POLICY p").number, 262);
    assert_eq!(err(&mut ann, "ALTER SECURITY POLICY p WITH (STATE = OFF)").number, 262);
}

#[test]
fn a_predicate_function_that_reads_its_own_table_does_not_recurse() {
    let (s, mut sa) = setup();
    exec(&mut sa, "CREATE FUNCTION dbo.same_owner_exists(@owner VARCHAR(20)) RETURNS TABLE AS RETURN SELECT 1 AS ok FROM dbo.orders o WHERE o.owner = @owner AND o.amount >= 0");
    exec(&mut sa, "CREATE SECURITY POLICY selfref ADD FILTER PREDICATE dbo.same_owner_exists(owner) ON dbo.orders");
    let mut ann = user(&s, "ann", "a");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM orders"), "5");
}

#[test]
fn policies_survive_a_restart_and_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Server::open(dir.path().join("d")).unwrap();
        let mut sa = s.connect();
        script(
            &mut sa,
            "CREATE TABLE t (id INT, who VARCHAR(10)); INSERT t VALUES (1, 'ann'), (2, 'bob');
             CREATE LOGIN ann WITH PASSWORD = 'a'; CREATE USER ann FOR LOGIN ann; GRANT SELECT ON t TO ann;
GO
             CREATE FUNCTION dbo.f(@w VARCHAR(10)) RETURNS TABLE AS RETURN SELECT 1 AS x WHERE @w = USER_NAME()
GO
             CREATE SECURITY POLICY pol ADD FILTER PREDICATE dbo.f(who) ON dbo.t",
        );
    }
    let s = Server::open(dir.path().join("d")).unwrap();
    let mut ann = s.connect_as("ann", "a").unwrap();
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM t"), "1");
    let mut sa = s.connect();
    let f = dir.path().join("b.bak").to_string_lossy().replace('\'', "''");
    exec(&mut sa, &format!("BACKUP DATABASE master TO DISK = '{}'", f));
    exec(&mut sa, &format!("RESTORE DATABASE copy FROM DISK = '{}'", f));
    assert_eq!(scalar(&mut sa, "SELECT COUNT(*) FROM copy.sys.security_policies"), "1");
}
