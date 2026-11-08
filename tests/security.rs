//! Logins, users, roles, GRANT / DENY / REVOKE, ownership chaining and impersonation.
mod common;
use common::*;
use tsql_server::{Connection, Server};

/// A server with a table, a view, a procedure and two ordinary users (ann, bob).
fn setup() -> (Server, Connection) {
    let s = common::server();
    let mut sa = s.connect();
    script(
        &mut sa,
        "CREATE TABLE emp (id INT PRIMARY KEY, name VARCHAR(20), salary INT)
INSERT emp VALUES (1, 'a', 100), (2, 'b', 200)
GO
CREATE VIEW emp_public AS SELECT id, name FROM emp
GO
CREATE PROCEDURE raise_all AS UPDATE emp SET salary = salary + 1
GO
CREATE PROCEDURE dyn AS EXEC('SELECT COUNT(*) FROM emp')
GO
CREATE LOGIN ann WITH PASSWORD = 'annpw'
CREATE LOGIN bob WITH PASSWORD = 'bobpw'
CREATE USER ann FOR LOGIN ann
CREATE USER bob FOR LOGIN bob
",
    );
    (s, sa)
}

fn as_user(s: &Server, login: &str) -> Connection {
    s.connect_as(login, &format!("{}pw", login)).expect("login")
}

#[test]
fn authentication() {
    let (s, mut sa) = setup();
    assert!(s.connect_as("ann", "annpw").is_ok());
    let e = s.connect_as("ann", "nope").err().unwrap();
    assert_eq!(e.number, 18456);
    assert_eq!(e.message, "Login failed for user 'ann'.");
    assert_eq!(s.connect_as("ghost", "x").err().unwrap().number, 18456);
    assert_eq!(s.connect_as("sa", "").err().unwrap().number, 18456, "sa has no password until one is set");
    exec(&mut sa, "ALTER LOGIN sa WITH PASSWORD = 'sapw'");
    assert!(s.connect_as("sa", "sapw").is_ok());
    exec(&mut sa, "ALTER LOGIN ann DISABLE");
    assert_eq!(s.connect_as("ann", "annpw").err().unwrap().number, 18456);
    exec(&mut sa, "ALTER LOGIN ann ENABLE");
    assert!(s.connect_as("ann", "annpw").is_ok());
    exec(&mut sa, "ALTER LOGIN bob WITH PASSWORD = 'new'");
    assert!(s.connect_as("bob", "bobpw").is_err());
    assert!(s.connect_as("bob", "new").is_ok());
    assert_eq!(err_no(&mut sa, "CREATE LOGIN ann WITH PASSWORD = 'x'"), 15025);
}

#[test]
fn users_have_no_rights_until_granted() {
    let (s, _sa) = setup();
    let mut ann = as_user(&s, "ann");
    let e = err(&mut ann, "SELECT * FROM emp");
    assert_eq!(e.number, 229);
    assert_eq!(e.message, "The SELECT permission was denied on the object 'emp', database 'master', schema 'dbo'.");
    assert_eq!(err_no(&mut ann, "INSERT emp VALUES (3, 'c', 1)"), 229);
    assert_eq!(err_no(&mut ann, "UPDATE emp SET salary = 0"), 229);
    assert_eq!(err_no(&mut ann, "DELETE emp"), 229);
    assert_eq!(err_no(&mut ann, "EXEC raise_all"), 229);
    assert_eq!(err_no(&mut ann, "TRUNCATE TABLE emp"), 229);
}

#[test]
fn grant_and_revoke_object_permissions() {
    let (s, mut sa) = setup();
    let mut ann = as_user(&s, "ann");
    exec(&mut sa, "GRANT SELECT ON emp TO ann");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM emp"), "2");
    assert_eq!(err_no(&mut ann, "UPDATE emp SET salary = 0"), 229);
    exec(&mut sa, "GRANT UPDATE, INSERT ON emp TO ann");
    exec(&mut ann, "UPDATE emp SET salary = 5 WHERE id = 1");
    exec(&mut ann, "INSERT emp VALUES (3, 'c', 3)");
    assert_eq!(err_no(&mut ann, "DELETE emp WHERE id = 3"), 229);
    exec(&mut sa, "REVOKE SELECT ON emp FROM ann");
    assert_eq!(err_no(&mut ann, "SELECT * FROM emp"), 229);
    exec(&mut ann, "UPDATE emp SET salary = 6 WHERE id = 1"); // still granted
    // another user is unaffected
    let mut bob = as_user(&s, "bob");
    assert_eq!(err_no(&mut bob, "SELECT * FROM emp"), 229);
}

#[test]
fn deny_overrides_grant() {
    let (s, mut sa) = setup();
    let mut ann = as_user(&s, "ann");
    exec(&mut sa, "CREATE ROLE readers");
    exec(&mut sa, "ALTER ROLE readers ADD MEMBER ann");
    exec(&mut sa, "GRANT SELECT ON emp TO readers");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM emp"), "2");
    exec(&mut sa, "DENY SELECT ON emp TO ann");
    assert_eq!(err_no(&mut ann, "SELECT * FROM emp"), 229, "a DENY on the user beats a GRANT through a role");
    exec(&mut sa, "REVOKE SELECT ON emp FROM ann");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM emp"), "2", "revoking the deny restores the role's grant");
}

#[test]
fn roles_and_membership() {
    let (s, mut sa) = setup();
    let mut ann = as_user(&s, "ann");
    exec(&mut sa, "ALTER ROLE db_datareader ADD MEMBER ann");
    assert_eq!(scalar(&mut ann, "SELECT COUNT(*) FROM emp"), "2");
    assert_eq!(err_no(&mut ann, "INSERT emp VALUES (9, 'z', 1)"), 229);
    exec(&mut sa, "ALTER ROLE db_datawriter ADD MEMBER ann");
    exec(&mut ann, "INSERT emp VALUES (9, 'z', 1)");
    exec(&mut sa, "ALTER ROLE db_datareader DROP MEMBER ann");
    assert_eq!(err_no(&mut ann, "SELECT * FROM emp"), 229);
    exec(&mut sa, "ALTER ROLE db_denydatawriter ADD MEMBER ann");
    assert_eq!(err_no(&mut ann, "INSERT emp VALUES (10, 'z', 1)"), 229, "db_denydatawriter beats db_datawriter");
    assert_eq!(scalar(&mut ann, "SELECT IS_MEMBER('db_datawriter')"), "1");
    assert_eq!(scalar(&mut ann, "SELECT IS_MEMBER('db_datareader')"), "0");
    exec(&mut sa, "EXEC sp_addrolemember 'db_owner', 'bob'");
    let mut bob = as_user(&s, "bob");
    assert_eq!(scalar(&mut bob, "SELECT COUNT(*) FROM emp"), "3", "db_owner can do everything in the database");
}

#[test]
fn schema_and_database_level_grants() {
    let (s, mut sa) = setup();
    exec(&mut sa, "CREATE SCHEMA hr");
    exec(&mut sa, "CREATE TABLE hr.a (x INT)");
    exec(&mut sa, "CREATE TABLE hr.b (x INT)");
    exec(&mut sa, "INSERT hr.a VALUES (1)");
    let mut ann = as_user(&s, "ann");
    assert_eq!(err_no(&mut ann, "SELECT * FROM hr.a"), 229);
    exec(&mut sa, "GRANT SELECT ON SCHEMA::hr TO ann");
    assert_eq!(scalar(&mut ann, "SELECT x FROM hr.a"), "1");
    exec(&mut ann, "SELECT * FROM hr.b");
    assert_eq!(err_no(&mut ann, "SELECT * FROM emp"), 229, "the schema grant covers hr only");
    exec(&mut sa, "DENY SELECT ON hr.b TO ann");
    assert_eq!(err_no(&mut ann, "SELECT * FROM hr.b"), 229);
    exec(&mut sa, "GRANT SELECT TO bob");
    let mut bob = as_user(&s, "bob");
    assert_eq!(scalar(&mut bob, "SELECT COUNT(*) FROM emp"), "2", "database-level SELECT covers every object");
    assert_eq!(scalar(&mut bob, "SELECT COUNT(*) FROM hr.a"), "1");
}

#[test]
fn control_implies_everything() {
    let (s, mut sa) = setup();
    exec(&mut sa, "GRANT CONTROL ON emp TO ann");
    let mut ann = as_user(&s, "ann");
    exec(&mut ann, "SELECT * FROM emp");
    exec(&mut ann, "UPDATE emp SET salary = 1");
    exec(&mut ann, "ALTER TABLE emp ADD extra INT NULL");
}

#[test]
fn views_and_procedures_use_ownership_chaining() {
    let (s, mut sa) = setup();
    let mut ann = as_user(&s, "ann");
    // permission on the view only: the table is reached through the owner's chain
    exec(&mut sa, "GRANT SELECT ON emp_public TO ann");
    assert_eq!(rows(&mut ann, "SELECT name FROM emp_public ORDER BY id"), vec![vec!["a"], vec!["b"]]);
    assert_eq!(err_no(&mut ann, "SELECT salary FROM emp"), 229, "the base table is still protected");
    // EXECUTE on a procedure lets the caller modify tables they cannot touch directly
    exec(&mut sa, "GRANT EXECUTE ON raise_all TO ann");
    exec(&mut ann, "EXEC raise_all");
    assert_eq!(scalar(&mut sa, "SELECT salary FROM emp WHERE id = 1"), "101");
    assert_eq!(err_no(&mut ann, "UPDATE emp SET salary = 0"), 229);
    // dynamic SQL breaks the chain: the caller's own rights apply
    exec(&mut sa, "GRANT EXECUTE ON dyn TO ann");
    assert_eq!(err_no(&mut ann, "EXEC dyn"), 229);
    exec(&mut sa, "GRANT SELECT ON emp TO ann");
    exec(&mut ann, "EXEC dyn");
}

#[test]
fn functions_and_triggers_follow_the_same_rules() {
    let (s, mut sa) = setup();
    script(
        &mut sa,
        "CREATE FUNCTION dbo.total() RETURNS INT AS BEGIN RETURN (SELECT SUM(salary) FROM emp) END
GO
CREATE FUNCTION dbo.names() RETURNS TABLE AS RETURN (SELECT name FROM emp)
GO
CREATE TABLE audit (n INT)
CREATE TRIGGER trg ON emp AFTER UPDATE AS INSERT audit VALUES (1)
GO",
    );
    let mut ann = as_user(&s, "ann");
    assert_eq!(err_no(&mut ann, "SELECT dbo.total()"), 229);
    exec(&mut sa, "GRANT EXECUTE ON dbo.total TO ann");
    assert_eq!(scalar(&mut ann, "SELECT dbo.total()"), "300");
    assert_eq!(err_no(&mut ann, "SELECT * FROM dbo.names()"), 229);
    exec(&mut sa, "GRANT SELECT ON dbo.names TO ann");
    exec(&mut ann, "SELECT * FROM dbo.names()");
    // the trigger writes to audit, which ann cannot touch, through the owner's chain
    exec(&mut sa, "GRANT UPDATE ON emp TO ann");
    exec(&mut ann, "UPDATE emp SET salary = salary + 1");
    assert_eq!(scalar(&mut sa, "SELECT COUNT(*) FROM audit"), "1");
}

#[test]
fn ddl_permissions() {
    let (s, mut sa) = setup();
    let mut ann = as_user(&s, "ann");
    let e = err(&mut ann, "CREATE TABLE mine (x INT)");
    assert_eq!(e.number, 262);
    assert_eq!(e.message, "CREATE TABLE permission denied in database 'master'.");
    exec(&mut sa, "GRANT CREATE TABLE TO ann");
    assert_eq!(err_no(&mut ann, "CREATE TABLE mine (x INT)"), 2760, "also needs ALTER on the target schema");
    exec(&mut sa, "CREATE SCHEMA ann_s AUTHORIZATION ann");
    exec(&mut ann, "CREATE TABLE ann_s.mine (x INT)");
    exec(&mut ann, "INSERT ann_s.mine VALUES (1)");
    assert_eq!(scalar(&mut ann, "SELECT x FROM ann_s.mine"), "1", "a schema owner owns its objects");
    exec(&mut ann, "DROP TABLE ann_s.mine");
    assert_eq!(err_no(&mut ann, "DROP TABLE emp"), 4902);
    assert_eq!(err_no(&mut ann, "ALTER TABLE emp ADD z INT"), 4902);
    assert_eq!(err_no(&mut ann, "CREATE LOGIN eve WITH PASSWORD = 'x'"), 15247);
    assert_eq!(err_no(&mut ann, "CREATE DATABASE nope"), 262);
    exec(&mut sa, "ALTER SERVER ROLE dbcreator ADD MEMBER ann");
    exec(&mut ann, "CREATE DATABASE okdb");
    exec(&mut sa, "ALTER ROLE db_ddladmin ADD MEMBER bob");
    let mut bob = as_user(&s, "bob");
    exec(&mut bob, "CREATE TABLE bobs (x INT)");
}

#[test]
fn impersonation() {
    let (s, mut sa) = setup();
    exec(&mut sa, "GRANT SELECT ON emp TO ann");
    assert_eq!(scalar(&mut sa, "SELECT USER_NAME()"), "dbo");
    exec(&mut sa, "EXECUTE AS USER = 'bob'");
    assert_eq!(scalar(&mut sa, "SELECT USER_NAME()"), "bob");
    assert_eq!(err_no(&mut sa, "SELECT * FROM emp"), 229);
    exec(&mut sa, "REVERT");
    exec(&mut sa, "EXECUTE AS USER = 'ann'");
    assert_eq!(scalar(&mut sa, "SELECT COUNT(*) FROM emp"), "2");
    assert_eq!(scalar(&mut sa, "SELECT ORIGINAL_LOGIN()"), "sa");
    exec(&mut sa, "REVERT");
    exec(&mut sa, "EXECUTE AS LOGIN = 'ann'");
    assert_eq!(scalar(&mut sa, "SELECT SUSER_SNAME()"), "ann");
    exec(&mut sa, "REVERT");
    assert_eq!(scalar(&mut sa, "SELECT SUSER_SNAME()"), "sa");
    let mut ann = as_user(&s, "ann");
    assert_eq!(err_no(&mut ann, "EXECUTE AS USER = 'bob'"), 15517, "only privileged principals may impersonate");
}

#[test]
fn database_access_requires_a_mapped_user() {
    let (s, mut sa) = setup();
    exec(&mut sa, "CREATE DATABASE shop");
    exec(&mut sa, "USE shop; CREATE TABLE t (x INT); INSERT t VALUES (1)");
    let mut ann = as_user(&s, "ann");
    let e = err(&mut ann, "USE shop");
    assert_eq!(e.number, 916);
    exec(&mut sa, "USE shop; CREATE USER ann FOR LOGIN ann; GRANT SELECT ON t TO ann");
    assert_eq!(scalar(&mut ann, "USE shop; SELECT x FROM t"), "1");
    assert_eq!(scalar(&mut ann, "SELECT DB_NAME()"), "shop");
}

#[test]
fn security_statement_errors() {
    let (_s, mut sa) = setup();
    assert_eq!(err_no(&mut sa, "CREATE USER ann FOR LOGIN ann"), 15023);
    assert_eq!(err_no(&mut sa, "CREATE USER x FOR LOGIN nobody"), 15007);
    assert_eq!(err_no(&mut sa, "GRANT SELECT ON emp TO ghost"), 15151);
    assert_eq!(err_no(&mut sa, "GRANT SELECT ON nothing TO ann"), 15151);
    assert_eq!(err_no(&mut sa, "ALTER ROLE nothing ADD MEMBER ann"), 15151);
    assert_eq!(err_no(&mut sa, "GRANT SELECT (name) ON emp TO ann"), 15477);
    assert_eq!(err_no(&mut sa, "DROP LOGIN sa"), 15405);
    exec(&mut sa, "DROP USER ann");
    exec(&mut sa, "DROP LOGIN ann");
    assert_eq!(err_no(&mut sa, "DROP USER ann"), 15151);
    exec(&mut sa, "DROP ROLE IF EXISTS nothing");
}

#[test]
fn permission_functions() {
    let (s, mut sa) = setup();
    exec(&mut sa, "GRANT SELECT ON emp TO ann");
    let mut ann = as_user(&s, "ann");
    assert_eq!(scalar(&mut ann, "SELECT HAS_PERMS_BY_NAME('emp', 'OBJECT', 'SELECT')"), "1");
    assert_eq!(scalar(&mut ann, "SELECT HAS_PERMS_BY_NAME('emp', 'OBJECT', 'DELETE')"), "0");
    assert_eq!(scalar(&mut ann, "SELECT IS_SRVROLEMEMBER('sysadmin')"), "0");
    assert_eq!(scalar(&mut sa, "SELECT IS_SRVROLEMEMBER('sysadmin')"), "1");
    assert_eq!(rows(&mut sa, "SELECT name FROM sys.database_principals WHERE name IN ('ann','bob') ORDER BY name"), vec![vec!["ann"], vec!["bob"]]);
    assert_eq!(rows(&mut sa, "SELECT name FROM sys.server_principals WHERE name = 'ann'"), vec![vec!["ann"]]);
    assert_eq!(rows(&mut sa, "SELECT permission_name, state_desc FROM sys.database_permissions p JOIN sys.database_principals u ON p.grantee_principal_id = u.principal_id WHERE u.name = 'ann'"), vec![vec!["SELECT", "GRANT"]]);
}
