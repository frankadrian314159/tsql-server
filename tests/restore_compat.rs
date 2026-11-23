mod common;
use common::*;

#[test]
fn sysprocesses_has_nt_username_and_compat_columns() {
    let s = server();
    let mut c = s.connect();
    let res = exec(&mut c, "SELECT spid, nt_username, nt_domain, net_address, net_library, cpu, physical_io, memusage FROM sys.sysprocesses");
    assert!(res.first_error().is_none(), "sysprocesses query failed: {:?}", res.first_error());
    let rs = res.resultsets();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].columns, vec!["spid", "nt_username", "nt_domain", "net_address", "net_library", "cpu", "physical_io", "memusage"]);
}

#[test]
fn deferred_view_resolution_for_external_database() {
    let s = server();
    let mut c = s.connect();
    let res = exec(&mut c, "CREATE VIEW v_archive AS SELECT id, name FROM ARCHIVE_demo.dbo.archive_audit_event");
    assert!(res.first_error().is_none(), "CREATE VIEW with external database should succeed with deferred resolution: {:?}", res.first_error());

    // Cascading view referencing the deferred view also succeeds
    let res2 = exec(&mut c, "CREATE VIEW v_fhir AS SELECT id FROM v_archive");
    assert!(res2.first_error().is_none(), "cascading view should succeed: {:?}", res2.first_error());
}

#[test]
fn parser_aliases_with_keywords_before_equals() {
    let s = server();
    let mut c = s.connect();
    let res = exec(&mut c, "SELECT Date = 1, User = 'sa', Output = 'val', Type = 42");
    assert!(res.first_error().is_none(), "SELECT alias = expr failed: {:?}", res.first_error());
    let rs = res.resultsets();
    assert_eq!(rs[0].columns, vec!["Date", "User", "Output", "Type"]);
    assert_eq!(rs[0].rows[0][0].as_i64(), Some(1));
}

#[test]
fn table_hints_without_with() {
    let s = server();
    let mut c = s.connect();
    exec(&mut c, "CREATE TABLE t_hint (id INT PRIMARY KEY, val VARCHAR(10)); INSERT t_hint VALUES (1, 'test')");
    
    // Hint before alias without WITH
    let res1 = exec(&mut c, "SELECT a.val FROM t_hint (NOLOCK) a");
    assert!(res1.first_error().is_none(), "hint before alias: {:?}", res1.first_error());
    assert_eq!(scalar(&mut c, "SELECT a.val FROM t_hint (NOLOCK) a"), "test");

    // Hint after alias without WITH
    let res2 = exec(&mut c, "SELECT a.val FROM t_hint a (NOLOCK)");
    assert!(res2.first_error().is_none(), "hint after alias: {:?}", res2.first_error());
    assert_eq!(scalar(&mut c, "SELECT a.val FROM t_hint a (NOLOCK)"), "test");

    // Hint without alias without WITH
    let res3 = exec(&mut c, "SELECT val FROM t_hint (NOLOCK)");
    assert!(res3.first_error().is_none(), "hint without alias: {:?}", res3.first_error());
    assert_eq!(scalar(&mut c, "SELECT val FROM t_hint (NOLOCK)"), "test");
}

#[test]
fn proc_parameter_output_before_default() {
    let s = server();
    let mut c = s.connect();
    let sql = "CREATE PROCEDURE sp_output_test @val INT OUTPUT = 42 AS BEGIN SELECT @val = @val + 1; END";
    let res = exec(&mut c, sql);
    assert!(res.first_error().is_none(), "CREATE PROCEDURE with OUTPUT before default failed: {:?}", res.first_error());
}
