//! DDL triggers (database and server scope), LOGON triggers, EVENTDATA(), ENABLE / DISABLE TRIGGER.
mod common;
use common::*;
use tsql_server::Server;

#[test]
fn a_database_trigger_can_block_ddl() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE keep (id INT)");
    script(
        &mut c,
        "CREATE TRIGGER no_drops ON DATABASE FOR DROP_TABLE, ALTER_TABLE AS
BEGIN
    ROLLBACK;
    THROW 50001, 'Tables cannot be dropped or altered here.', 1;
END
",
    );
    assert_eq!(err_no(&mut c, "DROP TABLE keep"), 50001);
    assert_eq!(err_no(&mut c, "ALTER TABLE keep ADD x INT"), 50001);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE name = 'keep'"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.columns c JOIN sys.tables t ON t.object_id = c.object_id WHERE t.name = 'keep'"), "1");
    // creating is not one of the events
    exec(&mut c, "CREATE TABLE other (id INT)");
    // a trigger that only rolls back, with no error, aborts with 3609
    exec(&mut c, "DROP TRIGGER no_drops ON DATABASE");
    script(&mut c, "CREATE TRIGGER rb ON DATABASE FOR DROP_TABLE AS ROLLBACK\n");
    assert_eq!(err_no(&mut c, "DROP TABLE other"), 3609);
    exec(&mut c, "DROP TRIGGER rb ON DATABASE");
    exec(&mut c, "DROP TABLE other");
}

#[test]
fn eventdata_describes_the_event() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE ddl_log (ev VARCHAR(50), obj VARCHAR(50), typ VARCHAR(20), who VARCHAR(50), cmd NVARCHAR(400))");
    script(
        &mut c,
        "CREATE TRIGGER log_ddl ON DATABASE FOR DDL_TABLE_EVENTS, CREATE_VIEW AS
    INSERT ddl_log VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'varchar(50)'),
        EVENTDATA().value('(/EVENT_INSTANCE/ObjectName)[1]', 'varchar(50)'),
        EVENTDATA().value('(/EVENT_INSTANCE/ObjectType)[1]', 'varchar(20)'),
        EVENTDATA().value('(/EVENT_INSTANCE/LoginName)[1]', 'varchar(50)'),
        EVENTDATA().value('(/EVENT_INSTANCE/TSQLCommand/CommandText)[1]', 'nvarchar(400)'))
",
    );
    exec(&mut c, "CREATE TABLE a (x INT)");
    exec(&mut c, "ALTER TABLE a ADD y INT");
    exec(&mut c, "CREATE VIEW v AS SELECT x FROM a");
    exec(&mut c, "DROP VIEW v");
    exec(&mut c, "DROP TABLE a");
    assert_eq!(
        rows(&mut c, "SELECT ev, obj, typ, who FROM ddl_log ORDER BY (SELECT NULL)"),
        vec![vec!["CREATE_TABLE", "a", "TABLE", "sa"], vec!["ALTER_TABLE", "a", "TABLE", "sa"], vec!["CREATE_VIEW", "v", "VIEW", "sa"], vec!["DROP_TABLE", "a", "TABLE", "sa"]]
    );
    assert_eq!(scalar(&mut c, "SELECT cmd FROM ddl_log WHERE ev = 'CREATE_TABLE'"), "CREATE TABLE a (x INT)");
    // EVENTDATA() outside a trigger is NULL
    assert_eq!(scalar(&mut c, "SELECT EVENTDATA()"), "NULL");
}

#[test]
fn enable_disable_drop_and_errors() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT)");
    script(&mut c, "CREATE TRIGGER t1 ON DATABASE FOR CREATE_TABLE AS THROW 50002, 'no new tables', 1\n");
    assert_eq!(err_no(&mut c, "CREATE TABLE u (id INT)"), 50002);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE name = 'u'"), "0");
    exec(&mut c, "DISABLE TRIGGER t1 ON DATABASE");
    exec(&mut c, "CREATE TABLE u (id INT)");
    exec(&mut c, "ENABLE TRIGGER ALL ON DATABASE");
    assert_eq!(err_no(&mut c, "CREATE TABLE w (id INT)"), 50002);
    assert_eq!(err_no(&mut c, "DISABLE TRIGGER nope ON DATABASE"), 4947);
    assert_eq!(err_no(&mut c, "DROP TRIGGER nope ON DATABASE"), 3701);
    exec(&mut c, "DROP TRIGGER IF EXISTS nope ON DATABASE");
    assert_eq!(err_no(&mut c, "CREATE TRIGGER t1 ON DATABASE FOR CREATE_TABLE AS PRINT 1"), 2714);
    assert_eq!(err_no(&mut c, "CREATE TRIGGER bad ON DATABASE FOR MAKE_COFFEE AS PRINT 1"), 1082);
    assert_eq!(err_no(&mut c, "CREATE TRIGGER bad ON DATABASE FOR LOGON AS PRINT 1"), 1082);
    exec(&mut c, "CREATE OR ALTER TRIGGER t1 ON DATABASE FOR DROP_TABLE AS PRINT 'dropped'");
    exec(&mut c, "CREATE TABLE w (id INT)");
    exec(&mut c, "DROP TRIGGER t1 ON DATABASE");
    exec(&mut c, "DROP TABLE w");
}

#[test]
fn dml_triggers_can_be_disabled() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT)");
    exec(&mut c, "CREATE TABLE n (c INT)");
    script(&mut c, "CREATE TRIGGER trg ON t AFTER INSERT AS INSERT n SELECT COUNT(*) FROM inserted\n");
    exec(&mut c, "INSERT t VALUES (1)");
    exec(&mut c, "DISABLE TRIGGER trg ON t");
    exec(&mut c, "INSERT t VALUES (2)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM n"), "1");
    exec(&mut c, "ENABLE TRIGGER trg ON t");
    exec(&mut c, "INSERT t VALUES (3)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM n"), "2");
    exec(&mut c, "DISABLE TRIGGER ALL ON t");
    exec(&mut c, "INSERT t VALUES (4)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM n"), "2");
    assert_eq!(err_no(&mut c, "DISABLE TRIGGER nope ON t"), 4947);
    assert_eq!(err_no(&mut c, "DISABLE TRIGGER trg ON missing"), 4902);
}

#[test]
fn server_triggers_see_every_database_and_login_events() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE DATABASE logdb");
    exec(&mut c, "CREATE TABLE logdb.dbo.log (ev VARCHAR(40), obj VARCHAR(40), db VARCHAR(40))");
    script(
        &mut c,
        "CREATE TRIGGER srv ON ALL SERVER FOR CREATE_DATABASE, DDL_LOGIN_EVENTS, CREATE_TABLE AS
    INSERT logdb.dbo.log VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'varchar(40)'),
        EVENTDATA().value('(/EVENT_INSTANCE/ObjectName)[1]', 'varchar(40)'),
        EVENTDATA().value('(/EVENT_INSTANCE/DatabaseName)[1]', 'varchar(40)'))
",
    );
    exec(&mut c, "CREATE DATABASE other");
    exec(&mut c, "CREATE LOGIN lg WITH PASSWORD = 'x'");
    exec(&mut c, "ALTER LOGIN lg DISABLE");
    exec(&mut c, "DROP LOGIN lg");
    exec(&mut c, "USE other");
    exec(&mut c, "CREATE TABLE tt (id INT)");
    exec(&mut c, "USE master");
    let log = rows(&mut c, "SELECT ev, obj, db FROM logdb.dbo.log ORDER BY (SELECT NULL)");
    assert_eq!(
        log,
        vec![
            vec!["CREATE_DATABASE", "other", "other"],
            vec!["CREATE_LOGIN", "lg", ""],
            vec!["ALTER_LOGIN", "lg", ""],
            vec!["DROP_LOGIN", "lg", ""],
            vec!["CREATE_TABLE", "tt", "other"]
        ]
    );
    // only sysadmin may create server triggers
    exec(&mut c, "CREATE LOGIN bob WITH PASSWORD = 'x'");
    let mut bob = s.connect_as("bob", "x").unwrap();
    assert_eq!(err_no(&mut bob, "CREATE TRIGGER x ON ALL SERVER FOR CREATE_DATABASE AS PRINT 1"), 262);
    assert_eq!(err_no(&mut bob, "CREATE TRIGGER x ON DATABASE FOR CREATE_TABLE AS PRINT 1"), 262);
    exec(&mut c, "DROP TRIGGER srv ON ALL SERVER");
    exec(&mut c, "CREATE DATABASE third");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM logdb.dbo.log"), "6", "bob's login creation was logged, the third database was not");
}

#[test]
fn logon_triggers_can_refuse_a_login() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE LOGIN ok WITH PASSWORD = 'x'");
    exec(&mut c, "CREATE LOGIN contractor WITH PASSWORD = 'x'");
    exec(&mut c, "CREATE TABLE logons (who VARCHAR(40))");
    exec(&mut c, "GRANT INSERT ON logons TO public");
    script(
        &mut c,
        "CREATE TRIGGER gate ON ALL SERVER FOR LOGON AS
BEGIN
    IF ORIGINAL_LOGIN() = 'contractor' ROLLBACK;
    ELSE INSERT master.dbo.logons VALUES (EVENTDATA().value('(/EVENT_INSTANCE/LoginName)[1]', 'varchar(40)'));
END
",
    );
    assert!(s.connect_as("ok", "x").is_ok());
    let e = s.connect_as("contractor", "x").err().unwrap();
    assert_eq!(e.number, 17892);
    assert_eq!(e.message, "Logon failed for login 'contractor' due to trigger execution.");
    assert_eq!(rows(&mut c, "SELECT who FROM logons"), vec![vec!["ok"]]);
    // a wrong password is refused before the trigger runs; the trusted in-process session is not subject to it
    assert_eq!(s.connect_as("ok", "bad").err().unwrap().number, 18456);
    exec(&mut c, "DISABLE TRIGGER gate ON ALL SERVER");
    assert!(s.connect_as("contractor", "x").is_ok());
    exec(&mut c, "ENABLE TRIGGER gate ON ALL SERVER");
    // an error in the trigger refuses everyone who authenticates
    exec(&mut c, "CREATE OR ALTER TRIGGER gate ON ALL SERVER FOR LOGON AS THROW 50010, 'closed', 1");
    assert_eq!(s.connect_as("ok", "x").err().unwrap().number, 17892);
    exec(&mut c, "DROP TRIGGER gate ON ALL SERVER");
    assert!(s.connect_as("ok", "x").is_ok());
}

#[test]
fn ddl_triggers_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Server::open(dir.path()).unwrap();
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT)");
        script(&mut c, "CREATE TRIGGER nd ON DATABASE FOR DROP_TABLE AS THROW 50003, 'kept', 1\n");
        script(&mut c, "CREATE TRIGGER sv ON ALL SERVER FOR CREATE_DATABASE AS THROW 50004, 'no databases', 1\n");
    }
    let s = Server::open(dir.path()).unwrap();
    let mut c = s.connect();
    assert_eq!(err_no(&mut c, "DROP TABLE t"), 50003);
    assert_eq!(err_no(&mut c, "CREATE DATABASE x"), 50004);
}

#[test]
fn dml_trigger_rollback_idiom_works_in_autocommit() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT)");
    script(&mut c, "CREATE TRIGGER chk ON t AFTER INSERT AS IF EXISTS (SELECT 1 FROM inserted WHERE id < 0) BEGIN ROLLBACK; THROW 50005, 'negative', 1; END\n");
    exec(&mut c, "INSERT t VALUES (1)");
    assert_eq!(err_no(&mut c, "INSERT t VALUES (-1)"), 50005);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "1");
}

#[test]
fn catalog_views_list_triggers() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE t (id INT)");
    script(&mut c, "CREATE TRIGGER dm ON t AFTER INSERT AS PRINT 1\nGO\nCREATE TRIGGER dd ON DATABASE FOR CREATE_TABLE, DROP_TABLE AS PRINT 1\nGO\nCREATE TRIGGER ss ON ALL SERVER FOR LOGON AS PRINT 1\n");
    exec(&mut c, "DISABLE TRIGGER dd ON DATABASE");
    assert_eq!(rows(&mut c, "SELECT name, parent_class, is_disabled FROM sys.triggers ORDER BY name"), vec![vec!["dd", "0", "1"], vec!["dm", "1", "0"]]);
    assert_eq!(rows(&mut c, "SELECT e.type_desc FROM sys.trigger_events e JOIN sys.triggers t ON t.object_id = e.object_id WHERE t.name = 'dd' ORDER BY 1"), vec![vec!["CREATE_TABLE"], vec!["DROP_TABLE"]]);
    assert_eq!(rows(&mut c, "SELECT name, is_disabled FROM sys.server_triggers"), vec![vec!["ss", "0"]]);
    assert_eq!(rows(&mut c, "SELECT type_desc FROM sys.server_trigger_events"), vec![vec!["LOGON"]]);
}
