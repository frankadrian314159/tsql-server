//! The `tsql` shell, run as a real process.
use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], stdin: &str, cwd: &std::path::Path) -> (String, String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tsql")).args(args).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), out.status.code().unwrap_or(-1))
}

#[test]
fn batches_dot_commands_and_csv_work_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let session = "\
CREATE TABLE dbo.p (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(50) NOT NULL, amt DECIMAL(10,2) NULL)
GO
INSERT p (name, amt) VALUES (N'a,b', 1.5), (N'tab', 20)
GO
SELECT * FROM p
GO
.columns p
.schema p
.export out.csv SELECT name, amt FROM p
.import --new out.csv copy1
SELECT COUNT(*) AS n FROM copy1
GO 2
.tables
.plan SELECT * FROM p WHERE id = 2
.nonsense
";
    let (out, err, code) = run(&[], session, dir.path());
    assert!(out.contains("id │ name │   amt"), "{out}");
    assert!(out.contains(" 1 │ a,b  │  1.50"), "{out}");
    assert!(out.contains("primary key, identity(1,1)"), "{out}");
    assert!(out.contains("CREATE TABLE [dbo].[p]"), "{out}");
    assert!(out.contains("2 rows written to out.csv") && out.contains("2 rows imported"), "{out}");
    assert_eq!(out.matches("(1 row)").count(), 2, "GO 2 runs the batch twice: {out}");
    assert!(out.contains("dbo.copy1") && out.contains("index seek"), "{out}");
    assert!(err.contains("unknown command '.nonsense'"), "{err}");
    assert_eq!(code, 0);
    assert_eq!(std::fs::read_to_string(dir.path().join("out.csv")).unwrap(), "name,amt\r\n\"a,b\",1.50\r\ntab,20.00\r\n");
}

#[test]
fn queries_from_the_command_line_set_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let (out, _, code) = run(&["-Q", "SELECT 6 * 7 AS answer"], "", dir.path());
    assert!(out.contains("answer") && out.contains("42"), "{out}");
    assert_eq!(code, 0);
    let (_, err, code) = run(&["-Q", "SELECT 1/0"], "", dir.path());
    assert!(err.contains("Msg 8134") && err.contains("Divide by zero"), "{err}");
    assert_eq!(code, 1);
    let (_, _, code) = run(&["--bogus"], "", dir.path());
    assert_eq!(code, 2);
    // a script file, and an open transaction counts as a failure
    std::fs::write(dir.path().join("s.sql"), "CREATE TABLE t (a INT)\nGO\nBEGIN TRAN\nINSERT t VALUES (1)\nGO\n").unwrap();
    let (_, err, code) = run(&["-i", "s.sql"], "", dir.path());
    assert!(err.contains("transaction was still open"), "{err}");
    assert_eq!(code, 1);
}

#[test]
fn a_data_directory_keeps_its_data_between_runs() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("db");
    let d = data.to_str().unwrap();
    let (_, err, code) = run(&["-D", d, "-Q", "CREATE TABLE keep (a INT); INSERT keep VALUES (7)"], "", dir.path());
    assert_eq!(code, 0, "{err}");
    let (out, _, code) = run(&[d, "--pool-pages", "64"], "SELECT a FROM keep\nGO\n.checkpoint\n.storage\n", dir.path());
    assert_eq!(code, 0);
    assert!(out.contains("
7
") && out.contains("pool_pages") && out.contains("64"), "{out}");
}

#[test]
fn login_and_database_options_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let (_, err, code) = run(&["-U", "nobody", "-P", "x", "-Q", "SELECT 1"], "", dir.path());
    assert_eq!(code, 1);
    assert!(err.contains("Msg 18456"), "{err}");
    let (out, _, code) = run(&["-d", "tempdb", "-Q", "SELECT DB_NAME() AS db"], "", dir.path());
    assert_eq!(code, 0);
    assert!(out.contains("tempdb"), "{out}");
}
