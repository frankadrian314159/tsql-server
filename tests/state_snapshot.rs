//! The state of a server as one blob (`export_state`) and the way back (`install_state`): what a replica group keeps as the
//! snapshot of its state machine, so that its log can be cut and a replica that fell behind can be brought up to date.
mod common;
use common::*;
use tsql_server::Server;

fn populate(c: &mut tsql_server::Connection) {
    exec(c, "CREATE TABLE t (id INT IDENTITY PRIMARY KEY, name NVARCHAR(20), amt DECIMAL(10,2), d DATETIME2, g UNIQUEIDENTIFIER)");
    exec(c, "CREATE INDEX ix_name ON t (name)");
    exec(c, "INSERT t (name, amt, d, g) VALUES (N'héllo', 12.50, '2020-02-29 13:45:10.1234567', '6F9619FF-8B86-D011-B42D-00C04FC964FF'), (N'wörld', NULL, NULL, NULL), (N'x', 1.25, NULL, NULL)");
    exec(c, "DELETE t WHERE name = N'x'");
    exec(c, "CREATE SEQUENCE sq START WITH 100 INCREMENT BY 5");
    exec(c, "SELECT NEXT VALUE FOR sq");
    exec(c, "CREATE DATABASE other");
    exec(c, "CREATE TABLE other.dbo.u (k INT PRIMARY KEY, v VARCHAR(10))");
    exec(c, "INSERT other.dbo.u VALUES (1, 'one'), (2, 'two')");
}

fn same_everywhere(c: &mut tsql_server::Connection) {
    assert_eq!(
        rows(c, "SELECT id, name, amt, d, g FROM t ORDER BY id"),
        vec![vec!["1", "héllo", "12.50", "2020-02-29 13:45:10.1234567", "6F9619FF-8B86-D011-B42D-00C04FC964FF"], vec!["2", "wörld", "NULL", "NULL", "NULL"]]
    );
    assert_eq!(rows(c, "SELECT id FROM t WHERE name = N'wörld'"), vec![vec!["2"]], "the secondary index works");
    assert_eq!(rows(c, "SELECT id FROM t WHERE id = 2"), vec![vec!["2"]], "and so does the primary key");
    assert_eq!(rows(c, "SELECT k, v FROM other.dbo.u ORDER BY k"), vec![vec!["1", "one"], vec!["2", "two"]]);
    assert_eq!(err_no(c, "INSERT t (id, name) VALUES (1, 'dup')"), 544); // identity insert off
    assert_eq!(scalar(c, "SELECT NEXT VALUE FOR sq"), "105", "the sequence continues");
}

#[test]
fn a_server_can_be_rebuilt_from_its_exported_state() {
    let a = Server::new();
    {
        let mut c = a.connect();
        populate(&mut c);
    }
    let blob = a.export_state().unwrap();
    let b = Server::new();
    b.install_state(&blob).unwrap();
    let mut c = b.connect();
    same_everywhere(&mut c);
    // and the copy is a working server: new rows get the next identity value
    exec(&mut c, "INSERT t (name) VALUES (N'new')");
    assert_eq!(scalar(&mut c, "SELECT MAX(id) FROM t"), "4"); // 3 was used by the deleted row
}

#[test]
fn installing_replaces_what_the_server_had() {
    let a = Server::new();
    populate(&mut a.connect());
    let blob = a.export_state().unwrap();
    let b = Server::new();
    {
        let mut c = b.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, junk INT)");
        exec(&mut c, "INSERT t VALUES (99, 99)");
        exec(&mut c, "CREATE DATABASE stale");
        exec(&mut c, "CREATE TABLE stale.dbo.z (a INT)");
    }
    b.install_state(&blob).unwrap();
    let mut c = b.connect();
    same_everywhere(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.databases WHERE name = 'stale'"), "0", "a database the snapshot lacks is gone");
}

#[test]
fn an_installed_state_survives_a_crash() {
    let src = Server::new();
    populate(&mut src.connect());
    let blob = src.export_state().unwrap();
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Server::open(dir.path()).unwrap();
        {
            let mut c = s.connect();
            exec(&mut c, "CREATE TABLE old (a INT)");
        }
        s.install_state(&blob).unwrap();
        s.simulate_crash();
    }
    let s = Server::open(dir.path()).unwrap();
    let mut c = s.connect();
    same_everywhere(&mut c);
    assert_eq!(err_no(&mut c, "SELECT * FROM old"), 208);
}

#[test]
fn prepared_transactions_are_part_of_the_state() {
    let a = Server::new();
    {
        let mut c = a.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
        exec(&mut c, "INSERT t VALUES (1, 10)");
        exec(&mut c, "BEGIN TRAN");
        exec(&mut c, "INSERT t VALUES (2, 20)");
        assert!(c.prepare_transaction(77).unwrap());
        // the snapshot is taken while the transaction is open and prepared
        let blob = a.export_state().unwrap();
        let b = Server::new();
        b.install_state(&blob).unwrap();
        assert_eq!(b.in_doubt(), vec![77], "the copy holds the vote, in doubt");
        assert_eq!(scalar(&mut b.connect(), "SELECT COUNT(*) FROM t"), "1", "without its changes");
        assert!(b.resolve_prepared(77, true).unwrap());
        assert_eq!(scalar(&mut b.connect(), "SELECT COUNT(*) FROM t"), "2");
    }
}

#[test]
fn flushing_the_log_makes_replicated_commits_durable() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::open(dir.path()).unwrap();
    {
        let mut c = s.connect();
        exec(&mut c, "CREATE TABLE t (id INT PRIMARY KEY)");
        exec(&mut c, "INSERT t VALUES (1)");
    }
    s.flush_log().unwrap();
    s.simulate_crash();
    let s = Server::open(dir.path()).unwrap();
    assert_eq!(scalar(&mut s.connect(), "SELECT COUNT(*) FROM t"), "1");
    // an in-memory server has nothing to flush
    Server::new().flush_log().unwrap();
}
