//! SQL Server Audit: server audits, server and database audit specifications, audit files.
mod common;
use common::*;
use tsql_server::Server;

fn audit_dir() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().to_string_lossy().replace('\'', "''");
    (d, p)
}

#[test]
fn database_audit_specification_records_access_to_a_table() {
    let (_d, dir) = audit_dir();
    let s = server();
    let mut c = s.connect();
    script(
        &mut c,
        &format!(
            "CREATE TABLE emp (id INT, salary INT)
INSERT emp VALUES (1, 100)
CREATE LOGIN ann WITH PASSWORD = 'x'
CREATE USER ann FOR LOGIN ann
GRANT SELECT ON emp TO ann
CREATE SERVER AUDIT a1 TO FILE (FILEPATH = '{dir}') WITH (QUEUE_DELAY = 1000, ON_FAILURE = CONTINUE)
ALTER SERVER AUDIT a1 WITH (STATE = ON)
CREATE DATABASE AUDIT SPECIFICATION s1 FOR SERVER AUDIT a1 ADD (SELECT, UPDATE ON OBJECT::dbo.emp BY public) WITH (STATE = ON)
"
        ),
    );
    exec(&mut c, "SELECT * FROM emp");
    exec(&mut c, "UPDATE emp SET salary = 200");
    exec(&mut c, "INSERT emp VALUES (2, 50)"); // not audited
    let mut ann = s.connect_as("ann", "x").unwrap();
    exec(&mut ann, "SELECT id FROM emp WHERE id IN (SELECT id FROM emp)");
    assert_eq!(err_no(&mut ann, "UPDATE emp SET salary = 0"), 229, "ann has no UPDATE permission");
    let got = rows(
        &mut c,
        &format!("SELECT action_id, succeeded, server_principal_name, database_principal_name, schema_name, object_name FROM sys.fn_get_audit_file('{dir}', DEFAULT, DEFAULT) ORDER BY sequence_number"),
    );
    assert_eq!(
        got,
        vec![
            vec!["SL", "1", "sa", "dbo", "dbo", "emp"],
            vec!["UP", "1", "sa", "dbo", "dbo", "emp"],
            vec!["SL", "1", "ann", "ann", "dbo", "emp"],
            vec!["UP", "0", "ann", "ann", "dbo", "emp"],
        ],
        "one record per statement and object, with the outcome"
    );
    assert!(scalar(&mut c, &format!("SELECT statement FROM sys.fn_get_audit_file('{dir}', DEFAULT, DEFAULT) WHERE action_id = 'UP' AND succeeded = 1")).contains("UPDATE emp SET salary = 200"));
    // disabling the audit stops the records
    exec(&mut c, "ALTER SERVER AUDIT a1 WITH (STATE = OFF)");
    exec(&mut c, "SELECT * FROM emp");
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM sys.fn_get_audit_file('{dir}', DEFAULT, DEFAULT)")), "4");
}

#[test]
fn only_the_named_principals_and_objects_are_audited() {
    let s = server();
    let mut c = s.connect();
    script(
        &mut c,
        "CREATE TABLE a (x INT)
CREATE TABLE b (x INT)
CREATE LOGIN u1 WITH PASSWORD = 'x'
CREATE LOGIN u2 WITH PASSWORD = 'x'
CREATE USER u1 FOR LOGIN u1
CREATE USER u2 FOR LOGIN u2
GRANT SELECT ON a TO public
GRANT SELECT ON b TO public
CREATE SERVER AUDIT a1 TO APPLICATION_LOG WITH (STATE = ON)
CREATE DATABASE AUDIT SPECIFICATION s1 FOR SERVER AUDIT a1 ADD (SELECT ON dbo.a BY u1) WITH (STATE = ON)
",
    );
    let mut u1 = s.connect_as("u1", "x").unwrap();
    let mut u2 = s.connect_as("u2", "x").unwrap();
    exec(&mut u1, "SELECT * FROM a");
    exec(&mut u1, "SELECT * FROM b");
    exec(&mut u2, "SELECT * FROM a");
    assert_eq!(rows(&mut c, "SELECT database_principal_name, object_name FROM sys.dm_audit_log"), vec![vec!["u1", "a"]]);
    // a schema- or database-wide action covers everything below it
    exec(&mut c, "ALTER DATABASE AUDIT SPECIFICATION s1 ADD (SELECT ON SCHEMA::dbo BY public)");
    exec(&mut u2, "SELECT * FROM b");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.dm_audit_log WHERE database_principal_name = 'u2'"), "1");
    exec(&mut c, "ALTER DATABASE AUDIT SPECIFICATION s1 WITH (STATE = OFF)");
    exec(&mut u2, "SELECT * FROM b");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.dm_audit_log WHERE database_principal_name = 'u2'"), "1");
}

#[test]
fn server_specification_records_logins_and_changes() {
    let s = server();
    let mut c = s.connect();
    script(
        &mut c,
        "CREATE LOGIN bob WITH PASSWORD = 'secret'
CREATE SERVER AUDIT sec TO SECURITY_LOG
CREATE SERVER AUDIT SPECIFICATION sp FOR SERVER AUDIT sec
  ADD (FAILED_LOGIN_GROUP), ADD (SUCCESSFUL_LOGIN_GROUP), ADD (LOGOUT_GROUP), ADD (SERVER_PRINCIPAL_CHANGE_GROUP), ADD (DATABASE_CHANGE_GROUP), ADD (AUDIT_CHANGE_GROUP)
  WITH (STATE = ON)
ALTER SERVER AUDIT sec WITH (STATE = ON)
",
    );
    assert!(s.connect_as("bob", "wrong").is_err());
    {
        let _bob = s.connect_as("bob", "secret").unwrap();
    }
    exec(&mut c, "CREATE LOGIN carol WITH PASSWORD = 'x'");
    exec(&mut c, "CREATE DATABASE audited");
    exec(&mut c, "DROP LOGIN carol");
    let got = rows(&mut c, "SELECT action_id, succeeded, server_principal_name, target_server_principal_name, database_name FROM sys.dm_audit_log ORDER BY sequence_number");
    assert_eq!(
        got,
        vec![
            vec!["AUSC", "1", "sa", "", "master"], // enabling the audit is itself an audited change
            vec!["LGIF", "0", "bob", "", ""],
            vec!["LGIS", "1", "bob", "", ""],
            vec!["LGO", "1", "bob", "", ""],
            vec!["CR", "1", "sa", "carol", ""],
            vec!["CR", "1", "sa", "", "audited"],
            vec!["DR", "1", "sa", "carol", ""],
        ]
    );
    exec(&mut c, "ALTER SERVER AUDIT SPECIFICATION sp WITH (STATE = OFF)");
    exec(&mut c, "ALTER SERVER AUDIT SPECIFICATION sp DROP (DATABASE_CHANGE_GROUP) WITH (STATE = ON)");
    exec(&mut c, "CREATE DATABASE audited2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.dm_audit_log WHERE database_name = 'audited2'"), "0");
}

#[test]
fn ddl_and_permission_changes_and_filters() {
    let (_d, dir) = audit_dir();
    let s = server();
    let mut c = s.connect();
    script(
        &mut c,
        &format!(
            "CREATE LOGIN w WITH PASSWORD = 'x'
CREATE USER w FOR LOGIN w
CREATE SERVER AUDIT chg TO FILE (FILEPATH = '{dir}') WHERE object_name <> 'noisy' AND database_name = 'master'
ALTER SERVER AUDIT chg WITH (STATE = ON)
CREATE DATABASE AUDIT SPECIFICATION d1 FOR SERVER AUDIT chg
  ADD (SCHEMA_OBJECT_CHANGE_GROUP), ADD (DATABASE_PRINCIPAL_CHANGE_GROUP), ADD (DATABASE_PERMISSION_CHANGE_GROUP), ADD (SCHEMA_OBJECT_PERMISSION_CHANGE_GROUP)
  WITH (STATE = ON)
"
        ),
    );
    exec(&mut c, "CREATE TABLE t (id INT)");
    exec(&mut c, "CREATE TABLE noisy (id INT)");
    exec(&mut c, "ALTER TABLE t ADD n INT");
    exec(&mut c, "GRANT SELECT ON t TO w");
    exec(&mut c, "GRANT CREATE TABLE TO w");
    exec(&mut c, "CREATE USER x WITHOUT LOGIN");
    exec(&mut c, "DROP TABLE t");
    let got = rows(&mut c, &format!("SELECT action_id, object_name, class_type FROM sys.fn_get_audit_file('{dir}', DEFAULT, DEFAULT) ORDER BY sequence_number"));
    assert_eq!(
        got,
        vec![vec!["CR", "t", "U "], vec!["AL", "t", "U "], vec!["G", "t", "U "], vec!["G", "master", "DB"], vec!["CR", "x", "US"], vec!["DR", "t", "U "]],
        "the WHERE filter drops `noisy`"
    );
}

#[test]
fn statements_and_errors() {
    let (_d, dir) = audit_dir();
    let mut c = conn();
    assert_eq!(err_no(&mut c, "CREATE SERVER AUDIT x TO FILE (MAXSIZE = 10 MB)"), 25706);
    exec(&mut c, &format!("CREATE SERVER AUDIT x TO FILE (FILEPATH = '{dir}', MAXSIZE = 10 MB, MAX_FILES = 3)"));
    assert_eq!(err_no(&mut c, &format!("CREATE SERVER AUDIT x TO FILE (FILEPATH = '{dir}')")), 15012);
    assert_eq!(err_no(&mut c, "CREATE SERVER AUDIT SPECIFICATION s FOR SERVER AUDIT nope ADD (LOGOUT_GROUP)"), 15151);
    assert_eq!(err_no(&mut c, "CREATE SERVER AUDIT SPECIFICATION s FOR SERVER AUDIT x ADD (SCHEMA_OBJECT_CHANGE_GROUP)"), 25707);
    assert_eq!(err_no(&mut c, "CREATE DATABASE AUDIT SPECIFICATION s FOR SERVER AUDIT x ADD (LOGOUT_GROUP)"), 25707);
    assert_eq!(err_no(&mut c, "CREATE DATABASE AUDIT SPECIFICATION s FOR SERVER AUDIT x ADD (MERGE ON dbo.t BY public)"), 25707);
    exec(&mut c, "ALTER SERVER AUDIT x WITH (STATE = ON)");
    assert_eq!(err_no(&mut c, "DROP SERVER AUDIT x"), 33071, "a running audit cannot be dropped");
    exec(&mut c, "ALTER SERVER AUDIT x WITH (STATE = OFF)");
    assert_eq!(rows(&mut c, "SELECT name, type_desc, is_state_enabled FROM sys.server_audits"), vec![vec!["x", "FILE", "0"]]);
    assert_eq!(rows(&mut c, "SELECT name, status_desc FROM sys.dm_server_audit_status"), vec![vec!["x", "STOPPED"]]);
    exec(&mut c, "CREATE SERVER AUDIT SPECIFICATION ss FOR SERVER AUDIT x ADD (LOGOUT_GROUP), ADD (DBCC_GROUP)");
    exec(&mut c, "CREATE DATABASE AUDIT SPECIFICATION ds FOR SERVER AUDIT x ADD (SELECT, INSERT ON dbo.t BY public), ADD (SCHEMA_OBJECT_ACCESS_GROUP)");
    assert_eq!(rows(&mut c, "SELECT audit_action_name FROM sys.server_audit_specification_details ORDER BY 1"), vec![vec!["DBCC_GROUP"], vec!["LOGOUT_GROUP"]]);
    assert_eq!(
        rows(&mut c, "SELECT audit_action_name, class_desc, object_name, audited_principal_name, is_group FROM sys.database_audit_specification_details ORDER BY 1"),
        vec![vec!["INSERT", "OBJECT_OR_COLUMN", "dbo.t", "public", "0"], vec!["SCHEMA_OBJECT_ACCESS_GROUP", "DATABASE", "NULL", "NULL", "1"], vec!["SELECT", "OBJECT_OR_COLUMN", "dbo.t", "public", "0"]]
    );
    assert_eq!(rows(&mut c, "SELECT name, is_state_enabled FROM sys.server_audit_specifications"), vec![vec!["ss", "0"]]);
    exec(&mut c, "DROP DATABASE AUDIT SPECIFICATION ds");
    exec(&mut c, "DROP SERVER AUDIT SPECIFICATION ss");
    assert_eq!(err_no(&mut c, "DROP SERVER AUDIT SPECIFICATION ss"), 15151);
    exec(&mut c, "DROP SERVER AUDIT x");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.server_audits"), "0");
    assert_eq!(err_no(&mut c, "DROP SERVER AUDIT x"), 15151);
}

#[test]
fn only_sysadmin_manages_server_audits_and_reads_files() {
    let s = server();
    let mut c = s.connect();
    script(&mut c, "CREATE LOGIN pat WITH PASSWORD = 'x'\nCREATE USER pat FOR LOGIN pat\n");
    let mut pat = s.connect_as("pat", "x").unwrap();
    assert_eq!(err_no(&mut pat, "CREATE SERVER AUDIT z TO APPLICATION_LOG"), 262);
    assert_eq!(err_no(&mut pat, "CREATE SERVER AUDIT SPECIFICATION z FOR SERVER AUDIT q ADD (LOGOUT_GROUP)"), 262);
    assert_eq!(err_no(&mut pat, "CREATE DATABASE AUDIT SPECIFICATION z FOR SERVER AUDIT q ADD (SCHEMA_OBJECT_ACCESS_GROUP)"), 262);
    assert_eq!(err_no(&mut pat, "SELECT * FROM sys.fn_get_audit_file('.', DEFAULT, DEFAULT)"), 262);
}

#[test]
fn definitions_and_files_survive_a_restart() {
    let (_d, dir) = audit_dir();
    let data = tempfile::tempdir().unwrap();
    {
        let s = Server::open(data.path()).unwrap();
        let mut c = s.connect();
        script(
            &mut c,
            &format!(
                "CREATE TABLE t (id INT)
CREATE SERVER AUDIT keep TO FILE (FILEPATH = '{dir}')
ALTER SERVER AUDIT keep WITH (STATE = ON)
CREATE DATABASE AUDIT SPECIFICATION ks FOR SERVER AUDIT keep ADD (SELECT ON dbo.t BY public) WITH (STATE = ON)
"
            ),
        );
        exec(&mut c, "SELECT * FROM t");
    }
    let s = Server::open(data.path()).unwrap();
    let mut c = s.connect();
    exec(&mut c, "SELECT * FROM t");
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM sys.fn_get_audit_file('{dir}', DEFAULT, DEFAULT)")), "2", "the audit was still running after the restart");
    assert_eq!(rows(&mut c, "SELECT name, is_state_enabled FROM sys.database_audit_specifications"), vec![vec!["ks", "1"]]);
}
