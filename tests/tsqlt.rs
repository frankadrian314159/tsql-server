//! A tSQLt-style unit-test suite written in T-SQL and run on the server itself.
//!
//! `tests/tsqlt/tsqlt.sql` is a small tSQLt-compatible framework (test classes, assertions, ExpectException,
//! FakeTable, SpyProcedure) written in T-SQL, since the real tSQLt needs CLR. The other files in that directory are
//! test classes; each is loaded into a fresh server, `tSQLt.RunAll` runs them, and every test must succeed.
//! A failing test is reported with its name and message.
mod common;
use common::*;

fn run_suite(name: &str, suite: &str) {
    let mut c = conn();
    script(&mut c, include_str!("tsqlt/tsqlt.sql"));
    script(&mut c, suite);
    exec(&mut c, "EXEC tSQLt.RunAll");
    let total = rows(&mut c, "SELECT COUNT(*) FROM tSQLt.TestResult")[0][0].parse::<i64>().unwrap();
    assert!(total > 0, "{}: no tests ran", name);
    let bad = rows(&mut c, "SELECT Class + '.' + TestCase, Result, Msg FROM tSQLt.TestResult WHERE Result <> 'Success' ORDER BY Id");
    if !bad.is_empty() {
        let list: Vec<String> = bad.iter().map(|r| format!("  [{}] {}: {}", r[1], r[0], r[2])).collect();
        panic!("{}: {} of {} tests did not succeed:\n{}", name, bad.len(), total, list.join("\n"));
    }
    println!("{}: {} tests succeeded", name, total);
}

#[test]
fn the_framework_itself() {
    run_suite("framework", include_str!("tsqlt/framework_tests.sql"));
}

#[test]
fn dynamic_sql() {
    run_suite("dynamic sql", include_str!("tsqlt/dynamic_sql_tests.sql"));
}

#[test]
fn errors_and_transactions() {
    run_suite("errors and transactions", include_str!("tsqlt/error_tests.sql"));
}

#[test]
fn programmability() {
    run_suite("programmability", include_str!("tsqlt/programmability_tests.sql"));
}

#[test]
fn data_types_and_functions() {
    run_suite("data types and functions", include_str!("tsqlt/type_tests.sql"));
}
