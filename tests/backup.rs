//! BACKUP / RESTORE: full and log backups, point-in-time recovery, verification, and behaviour under load.
mod common;
use common::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tsql_server::{Connection, Server};

struct Env {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    bak: std::path::PathBuf,
    server: Server,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let bak = dir.path().join("bak");
    std::fs::create_dir_all(&bak).unwrap();
    let server = Server::open(&data).unwrap();
    Env { _dir: dir, data, bak, server }
}

impl Env {
    fn file(&self, name: &str) -> String {
        self.bak.join(name).to_string_lossy().replace('\'', "''")
    }
}

fn sales(c: &mut Connection) {
    script(
        c,
        "CREATE DATABASE shop;
         USE shop;
         CREATE TABLE cust (id INT IDENTITY PRIMARY KEY, name VARCHAR(40) NOT NULL UNIQUE);
         CREATE TABLE ord (id INT PRIMARY KEY, cust_id INT NOT NULL REFERENCES cust(id) ON DELETE CASCADE, total DECIMAL(10,2));
         CREATE INDEX ix_ord_total ON ord(total);
         CREATE VIEW big_orders AS SELECT id, total FROM ord WHERE total > 100;
         CREATE SEQUENCE order_seq START WITH 1000;
         CREATE USER shopper WITHOUT LOGIN;
         GRANT SELECT ON big_orders TO shopper;
         CREATE PROCEDURE add_cust @n VARCHAR(40) AS BEGIN INSERT cust (name) VALUES (@n); END;",
    );
    let mut sql = String::from("USE shop;\n");
    for i in 0..40 {
        sql.push_str(&format!("INSERT cust (name) VALUES ('customer {}');\n", i));
    }
    for i in 1..=120 {
        sql.push_str(&format!("INSERT ord VALUES ({}, {}, {}.50);\n", i, (i % 40) + 1, i));
    }
    sql.push_str("DECLARE @x BIGINT = NEXT VALUE FOR order_seq; SET @x = NEXT VALUE FOR order_seq;");
    script(c, &sql);
}

#[test]
fn a_full_backup_restores_everything_under_another_name() {
    let e = env();
    let mut c = e.server.connect();
    sales(&mut c);
    let f = e.file("shop.bak");
    let r = exec(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}' WITH INIT, NAME = 'shop full'", f));
    assert!(r.messages().iter().any(|m| m.contains("successfully processed 160 rows")), "{:?}", r.messages());
    exec(&mut c, &format!("RESTORE DATABASE shop2 FROM DISK = '{}'", f));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shop2.dbo.cust"), "40");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shop2.dbo.ord"), "120");
    assert_eq!(scalar(&mut c, "SELECT SUM(total) FROM shop2.dbo.ord"), scalar(&mut c, "SELECT SUM(total) FROM shop.dbo.ord"));
    exec(&mut c, "USE shop2");
    // views, procedures, sequences, identity values, constraints, indexes and permissions came with it
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM big_orders"), "21");
    exec(&mut c, "EXEC add_cust 'new customer'");
    assert_eq!(scalar(&mut c, "SELECT id FROM cust WHERE name = 'new customer'"), "41");
    assert_eq!(scalar(&mut c, "SELECT NEXT VALUE FOR order_seq"), "1002");
    assert_eq!(err(&mut c, "INSERT cust (name) VALUES ('customer 3')").number, 2627);
    assert_eq!(err(&mut c, "INSERT ord VALUES (999, 12345, 1)").number, 547);
    exec(&mut c, "DELETE cust WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE cust_id = 1"), "0", "the cascading foreign key survived");
    assert!(scalar(&mut c, "SELECT index_id FROM sys.indexes WHERE name = 'ix_ord_total'").parse::<i32>().is_ok());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.database_principals WHERE name = 'shopper'"), "1");
    // the original is untouched, and the two databases have separate rows
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shop.dbo.cust"), "40");
    // lock identities of the copy do not collide with the original's
    exec(&mut c, "USE shop");
    exec(&mut c, "BEGIN TRAN; UPDATE ord SET total = total + 1 WHERE id = 1");
    let mut other = e.server.connect();
    exec(&mut other, "USE shop2; SET LOCK_TIMEOUT 200");
    assert_eq!(scalar(&mut other, "SELECT total FROM ord WHERE id = 1"), "1.50", "a lock in one database must not block the other");
    exec(&mut c, "ROLLBACK");
}

#[test]
fn restoring_over_an_existing_database_needs_replace_and_undoes_later_changes() {
    let e = env();
    let mut c = e.server.connect();
    sales(&mut c);
    let f = e.file("a.bak");
    exec(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}'", f));
    exec(&mut c, "USE shop; DELETE ord WHERE id <= 60; DROP VIEW big_orders; INSERT cust (name) VALUES ('later')");
    exec(&mut c, "USE master");
    assert_eq!(err(&mut c, &format!("RESTORE DATABASE shop FROM DISK = '{}'", f)).number, 3159);
    exec(&mut c, &format!("RESTORE DATABASE shop FROM DISK = '{}' WITH REPLACE", f));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shop.dbo.ord"), "120");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shop.dbo.cust WHERE name = 'later'"), "0");
    exec(&mut c, "USE shop");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.views WHERE name = 'big_orders'"), "1");
}

#[test]
fn a_restored_database_survives_a_crash() {
    let e = env();
    let f = e.file("d.bak");
    {
        let mut c = e.server.connect();
        sales(&mut c);
        exec(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}'", f));
        exec(&mut c, &format!("RESTORE DATABASE shopcopy FROM DISK = '{}'", f));
        exec(&mut c, "INSERT shopcopy.dbo.cust (name) VALUES ('after restore')");
    }
    e.server.clone().simulate_crash();
    drop(e.server);
    let s = Server::open(&e.data).unwrap();
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shopcopy.dbo.ord"), "120");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM shopcopy.dbo.cust"), "41");
}

#[test]
fn log_backups_restore_to_a_point_in_time() {
    let e = env();
    let mut c = e.server.connect();
    exec(&mut c, "CREATE DATABASE ledger");
    script(&mut c, "USE ledger; CREATE TABLE entries (id INT PRIMARY KEY, memo VARCHAR(30));");
    exec(&mut c, "INSERT entries VALUES (1, 'before the backup')");
    let (full, log1, log2) = (e.file("ledger.bak"), e.file("ledger1.trn"), e.file("ledger2.trn"));
    exec(&mut c, &format!("BACKUP DATABASE ledger TO DISK = '{}'", full));
    for i in 2..=5 {
        exec(&mut c, &format!("INSERT entries VALUES ({}, 'batch one')", i));
    }
    exec(&mut c, &format!("BACKUP LOG ledger TO DISK = '{}'", log1));
    for i in 6..=7 {
        exec(&mut c, &format!("INSERT entries VALUES ({}, 'batch two')", i));
    }
    thread::sleep(std::time::Duration::from_millis(60));
    let t = scalar(&mut c, "SELECT CONVERT(VARCHAR(30), GETDATE(), 121)");
    thread::sleep(std::time::Duration::from_millis(60));
    for i in 8..=10 {
        exec(&mut c, &format!("INSERT entries VALUES ({}, 'batch three')", i));
    }
    exec(&mut c, "UPDATE entries SET memo = 'changed' WHERE id = 1; DELETE entries WHERE id = 2");
    // a checkpoint in the middle moves the log into the archive: nothing may be lost
    e.server.checkpoint();
    exec(&mut c, "INSERT entries VALUES (11, 'batch four')");
    exec(&mut c, &format!("BACKUP LOG ledger TO DISK = '{}'", log2));
    // disaster: restore the full backup, then the first log, then the second up to the chosen time
    exec(&mut c, "DROP DATABASE ledger");
    exec(&mut c, &format!("RESTORE DATABASE ledger FROM DISK = '{}' WITH NORECOVERY", full));
    assert_eq!(scalar(&mut c, "SELECT state_desc FROM sys.databases WHERE name = 'ledger'"), "RESTORING");
    assert_eq!(err(&mut c, "SELECT COUNT(*) FROM ledger.dbo.entries").number, 927, "a restoring database cannot be read");
    assert_eq!(err(&mut c, "USE ledger").number, 927);
    exec(&mut c, &format!("RESTORE LOG ledger FROM DISK = '{}' WITH NORECOVERY", log1));
    exec(&mut c, &format!("RESTORE LOG ledger FROM DISK = '{}' WITH STOPAT = '{}', RECOVERY", log2, t));
    assert_eq!(scalar(&mut c, "SELECT state_desc FROM sys.databases WHERE name = 'ledger'"), "ONLINE");
    let ids = rows(&mut c, "SELECT id FROM ledger.dbo.entries ORDER BY id");
    let ids: Vec<&str> = ids.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(ids, ["1", "2", "3", "4", "5", "6", "7"], "everything up to the chosen time, nothing after it");
    assert_eq!(scalar(&mut c, "SELECT memo FROM ledger.dbo.entries WHERE id = 1"), "before the backup");
    // and without STOPAT the whole log is applied
    exec(&mut c, "DROP DATABASE ledger");
    exec(&mut c, &format!("RESTORE DATABASE ledger FROM DISK = '{}' WITH NORECOVERY", full));
    exec(&mut c, &format!("RESTORE LOG ledger FROM DISK = '{}' WITH NORECOVERY", log1));
    exec(&mut c, &format!("RESTORE LOG ledger FROM DISK = '{}' WITH RECOVERY", log2));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ledger.dbo.entries"), "10");
    assert_eq!(scalar(&mut c, "SELECT memo FROM ledger.dbo.entries WHERE id = 1"), "changed");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ledger.dbo.entries WHERE id = 2"), "0");
}

#[test]
fn the_log_chain_is_enforced() {
    let e = env();
    let mut c = e.server.connect();
    exec(&mut c, "CREATE DATABASE chain");
    script(&mut c, "USE chain; CREATE TABLE t (id INT PRIMARY KEY);");
    let (full, l1, l2, l3) = (e.file("c.bak"), e.file("c1.trn"), e.file("c2.trn"), e.file("c3.trn"));
    assert_eq!(err(&mut c, &format!("BACKUP LOG chain TO DISK = '{}'", l1)).number, 4214, "no full backup yet");
    exec(&mut c, &format!("BACKUP DATABASE chain TO DISK = '{}'", full));
    for (i, f) in [&l1, &l2, &l3].into_iter().enumerate() {
        exec(&mut c, &format!("INSERT chain.dbo.t VALUES ({})", i));
        exec(&mut c, &format!("BACKUP LOG chain TO DISK = '{}'", f));
    }
    exec(&mut c, "DROP DATABASE chain");
    exec(&mut c, &format!("RESTORE DATABASE chain FROM DISK = '{}' WITH NORECOVERY", full));
    assert_eq!(err(&mut c, &format!("RESTORE LOG chain FROM DISK = '{}'", l2)).number, 4305, "the first log backup is missing");
    exec(&mut c, &format!("RESTORE LOG chain FROM DISK = '{}' WITH NORECOVERY", l1));
    exec(&mut c, &format!("RESTORE LOG chain FROM DISK = '{}' WITH NORECOVERY", l1)); // an overlapping backup is harmless
    exec(&mut c, &format!("RESTORE LOG chain FROM DISK = '{}' WITH NORECOVERY", l2));
    exec(&mut c, &format!("RESTORE LOG chain FROM DISK = '{}' WITH RECOVERY", l3));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM chain.dbo.t"), "3");
    // an online database cannot be rolled forward
    assert_eq!(err(&mut c, &format!("RESTORE LOG chain FROM DISK = '{}'", l1)).number, 3117);
    // SIMPLE recovery breaks the chain
    exec(&mut c, "ALTER DATABASE chain SET RECOVERY SIMPLE");
    assert_eq!(scalar(&mut c, "SELECT recovery_model_desc FROM sys.databases WHERE name = 'chain'"), "SIMPLE");
    assert_eq!(err(&mut c, &format!("BACKUP LOG chain TO DISK = '{}'", l1)).number, 4208);
    exec(&mut c, "ALTER DATABASE chain SET RECOVERY FULL");
    assert_eq!(err(&mut c, &format!("BACKUP LOG chain TO DISK = '{}'", l1)).number, 4214, "a new full backup is needed after SIMPLE");
}

#[test]
fn log_backups_survive_a_crash_and_restart() {
    let e = env();
    let (full, log) = (e.file("r.bak"), e.file("r.trn"));
    {
        let mut c = e.server.connect();
        exec(&mut c, "CREATE DATABASE rr");
        script(&mut c, "USE rr; CREATE TABLE t (id INT PRIMARY KEY);");
        exec(&mut c, &format!("BACKUP DATABASE rr TO DISK = '{}'", full));
        exec(&mut c, "INSERT rr.dbo.t VALUES (1)");
        e.server.checkpoint();
        exec(&mut c, "INSERT rr.dbo.t VALUES (2)");
    }
    e.server.clone().simulate_crash();
    drop(e.server);
    let s = Server::open(&e.data).unwrap();
    let mut c = s.connect();
    exec(&mut c, "INSERT rr.dbo.t VALUES (3)");
    exec(&mut c, &format!("BACKUP LOG rr TO DISK = '{}'", log));
    exec(&mut c, "DROP DATABASE rr");
    exec(&mut c, &format!("RESTORE DATABASE rr FROM DISK = '{}' WITH NORECOVERY", full));
    exec(&mut c, &format!("RESTORE LOG rr FROM DISK = '{}' WITH RECOVERY", log));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM rr.dbo.t"), "3");
}

#[test]
fn verification_and_header_information() {
    let e = env();
    let mut c = e.server.connect();
    sales(&mut c);
    let f = e.file("v.bak");
    exec(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}' WITH NAME = 'nightly', DESCRIPTION = 'test run'", f));
    let r = exec(&mut c, &format!("RESTORE VERIFYONLY FROM DISK = '{}'", f));
    assert!(r.messages().iter().any(|m| m.contains("is valid")));
    let h = rows(&mut c, &format!("RESTORE HEADERONLY FROM DISK = '{}'", f));
    assert_eq!(h[0][0], "nightly");
    assert_eq!(h[0][2], "1");
    assert_eq!(h[0][4], "shop");
    assert_eq!(h[0][9], "FULL");
    // damage: one flipped byte, a truncated file, something else entirely
    let mut bytes = std::fs::read(e.bak.join("v.bak")).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(e.bak.join("flip.bak"), &bytes).unwrap();
    assert_eq!(err(&mut c, &format!("RESTORE VERIFYONLY FROM DISK = '{}'", e.file("flip.bak"))).number, 3043);
    std::fs::write(e.bak.join("cut.bak"), &bytes[..bytes.len() / 3]).unwrap();
    assert_eq!(err(&mut c, &format!("RESTORE VERIFYONLY FROM DISK = '{}'", e.file("cut.bak"))).number, 3241);
    std::fs::write(e.bak.join("junk.bak"), b"not a backup at all").unwrap();
    assert_eq!(err(&mut c, &format!("RESTORE DATABASE x FROM DISK = '{}'", e.file("junk.bak"))).number, 3241);
    assert_eq!(err(&mut c, &format!("RESTORE VERIFYONLY FROM DISK = '{}'", e.file("missing.bak"))).number, 3201);
    // a damaged backup restores nothing: no database appears
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.databases WHERE name = 'x'"), "0");
}

#[test]
fn statements_are_checked() {
    let e = env();
    let mut c = e.server.connect();
    sales(&mut c);
    let f = e.file("s.bak");
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE nothere TO DISK = '{}'", f)).number, 911);
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE tempdb TO DISK = '{}'", f)).number, 3147);
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}' WITH DIFFERENTIAL", f)).number, 3035, "no full backup to be based on");
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}' WITH NOSUCHOPTION", f)).number, 155);
    assert_eq!(err(&mut c, "BACKUP DATABASE shop TO DISK = ''").number, 3201);
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}', DISK = '{}2'", f, f)).number, 3205);
    exec(&mut c, "BEGIN TRAN");
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE shop TO DISK = '{}'", f)).number, 574);
    exec(&mut c, "ROLLBACK");
    // permissions: only sysadmin (or a db_owner for BACKUP) may
    exec(&mut c, "CREATE LOGIN bob WITH PASSWORD = 'pw'; USE shop; CREATE USER bob FOR LOGIN bob;");
    let mut bob = e.server.connect_as("bob", "pw").unwrap();
    exec(&mut bob, "USE shop");
    assert_eq!(err(&mut bob, &format!("BACKUP DATABASE shop TO DISK = '{}'", f)).number, 262);
    exec(&mut c, "USE shop; ALTER ROLE db_owner ADD MEMBER bob");
    exec(&mut bob, &format!("BACKUP DATABASE shop TO DISK = '{}'", f));
    assert_eq!(err(&mut bob, &format!("RESTORE DATABASE shopx FROM DISK = '{}'", f)).number, 262, "restoring needs dbcreator or sysadmin");
    // a full backup file is not a log backup and the other way round
    exec(&mut c, "USE master");
    assert_eq!(err(&mut c, &format!("RESTORE LOG shop FROM DISK = '{}'", f)).number, 3117);
    exec(&mut c, &format!("RESTORE DATABASE shopy FROM DISK = '{}' WITH NORECOVERY", f));
    assert_eq!(err(&mut c, &format!("RESTORE LOG shopy FROM DISK = '{}'", f)).number, 3154);
}

#[test]
fn a_backup_taken_during_writes_is_a_consistent_snapshot() {
    let e = env();
    let mut c = e.server.connect();
    exec(&mut c, "CREATE DATABASE bank");
    script(&mut c, "USE bank; CREATE TABLE acct (id INT PRIMARY KEY, bal INT);");
    let mut sql = String::from("USE bank;\n");
    for i in 0..20 {
        sql.push_str(&format!("INSERT acct VALUES ({}, 100);\n", i));
    }
    script(&mut c, &sql);
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = vec![];
    for t in 0..3 {
        let mut conn = e.server.connect();
        exec(&mut conn, "USE bank");
        let stop = stop.clone();
        writers.push(thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::SeqCst) {
                i += 1;
                let (a, b) = ((t * 5 + i) % 20, (t * 7 + i * 3 + 1) % 20);
                if a == b {
                    continue;
                }
                let r = conn.execute(&format!("BEGIN TRAN; UPDATE acct SET bal = bal - 3 WHERE id = {}; UPDATE acct SET bal = bal + 3 WHERE id = {}; COMMIT", a, b));
                if let Some(er) = r.first_error() {
                    assert!(er.number == 1205, "{}", er);
                }
            }
        }));
    }
    for n in 0..3 {
        let f = e.file(&format!("b{}.bak", n));
        exec(&mut c, &format!("BACKUP DATABASE bank TO DISK = '{}'", f));
        exec(&mut c, &format!("RESTORE DATABASE bank_copy{} FROM DISK = '{}'", n, f));
        assert_eq!(scalar(&mut c, &format!("SELECT SUM(bal) FROM bank_copy{}.dbo.acct", n)), "2000", "the copy must hold a state in which no transfer is half done");
        assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM bank_copy{}.dbo.acct", n)), "20");
    }
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        w.join().unwrap();
    }
}

#[test]
fn a_backup_waits_for_a_transaction_that_is_changing_the_catalog() {
    let e = env();
    let mut a = e.server.connect();
    exec(&mut a, "CREATE DATABASE ddl");
    exec(&mut a, "USE ddl; BEGIN TRAN; CREATE TABLE half_done (x INT)");
    let mut b = e.server.connect();
    exec(&mut b, "SET LOCK_TIMEOUT 300");
    let f = e.file("ddl.bak");
    assert_eq!(err(&mut b, &format!("BACKUP DATABASE ddl TO DISK = '{}'", f)).number, 1222);
    exec(&mut a, "COMMIT");
    exec(&mut b, &format!("BACKUP DATABASE ddl TO DISK = '{}'", f));
    exec(&mut b, &format!("RESTORE DATABASE ddl2 FROM DISK = '{}'", f));
    exec(&mut b, "USE ddl2");
    assert_eq!(scalar(&mut b, "SELECT COUNT(*) FROM sys.tables WHERE name = 'half_done'"), "1");
}

#[test]
fn log_segments_are_kept_only_while_a_backup_needs_them() {
    let e = env();
    let mut c = e.server.connect();
    exec(&mut c, "CREATE DATABASE seg");
    script(&mut c, "USE seg; CREATE TABLE t (id INT PRIMARY KEY);");
    exec(&mut c, &format!("BACKUP DATABASE seg TO DISK = '{}'", e.file("seg.bak")));
    exec(&mut c, "INSERT seg.dbo.t VALUES (1)");
    e.server.checkpoint();
    exec(&mut c, "INSERT seg.dbo.t VALUES (2)");
    e.server.checkpoint();
    let archive = e.data.join("log-archive");
    let count = || std::fs::read_dir(&archive).map(|d| d.count()).unwrap_or(0);
    assert!(count() >= 2, "checkpoints keep the log while it has not been backed up");
    exec(&mut c, &format!("BACKUP LOG seg TO DISK = '{}'", e.file("seg.trn")));
    assert_eq!(count(), 0, "a log backup releases the segments it covers");
}

#[test]
fn differential_backups_restore_on_top_of_their_full_backup() {
    let e = env();
    let mut c = e.server.connect();
    exec(&mut c, "CREATE DATABASE dd");
    let mut sql = String::from("USE dd; CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(20)); CREATE TABLE stay (x INT);\n");
    for i in 0..300 {
        sql.push_str(&format!("INSERT t VALUES ({}, 'v{}');\n", i, i));
    }
    sql.push_str("INSERT stay VALUES (1), (2), (3);");
    script(&mut c, &sql);
    let (full, diff, log) = (e.file("dd.bak"), e.file("dd.dif"), e.file("dd.trn"));
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE dd TO DISK = '{}' WITH DIFFERENTIAL", diff)).number, 3035, "no full backup yet");
    exec(&mut c, &format!("BACKUP DATABASE dd TO DISK = '{}'", full));
    // changes after the full backup: updates, deletes, inserts, a new table, a dropped table
    exec(&mut c, "UPDATE dd.dbo.t SET v = 'changed' WHERE id < 10");
    exec(&mut c, "DELETE dd.dbo.t WHERE id >= 290");
    exec(&mut c, "INSERT dd.dbo.t VALUES (1000, 'new'), (1001, 'newer')");
    exec(&mut c, "USE dd; CREATE TABLE fresh (a INT); INSERT fresh VALUES (7); DROP TABLE stay");
    let r = exec(&mut c, &format!("BACKUP DATABASE dd TO DISK = '{}' WITH DIFFERENTIAL", diff));
    assert!(r.messages().iter().any(|m| m.contains("successfully processed 13 rows")), "only the changed rows are in it: {:?}", r.messages());
    assert!(std::fs::metadata(e.bak.join("dd.dif")).unwrap().len() < std::fs::metadata(e.bak.join("dd.bak")).unwrap().len(), "a differential is smaller than the full backup");
    let h = rows(&mut c, &format!("RESTORE HEADERONLY FROM DISK = '{}'", diff));
    assert_eq!(h[0][2], "5");
    // one more change that only a log backup covers
    exec(&mut c, "INSERT dd.dbo.t VALUES (2000, 'logged')");
    exec(&mut c, &format!("BACKUP LOG dd TO DISK = '{}'", log));
    // restore: full, then differential, then log
    exec(&mut c, "DROP DATABASE dd");
    assert_eq!(err(&mut c, &format!("RESTORE DATABASE dd FROM DISK = '{}'", diff)).number, 3136, "the database does not exist yet");
    exec(&mut c, &format!("RESTORE DATABASE dd FROM DISK = '{}' WITH NORECOVERY", full));
    exec(&mut c, &format!("RESTORE DATABASE dd FROM DISK = '{}' WITH NORECOVERY", diff));
    exec(&mut c, &format!("RESTORE LOG dd FROM DISK = '{}' WITH RECOVERY", log));
    let n = |c: &mut tsql_server::Connection, q: &str| scalar(c, q);
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM dd.dbo.t"), "293", "300 - 10 deleted + 2 new + 1 logged");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM dd.dbo.t WHERE v = 'changed'"), "10");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM dd.dbo.t WHERE id >= 290 AND id < 1000"), "0");
    assert_eq!(n(&mut c, "SELECT v FROM dd.dbo.t WHERE id = 2000"), "logged");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM dd.dbo.t WHERE id = 150 AND v = 'v150'"), "1", "untouched rows come from the full backup");
    assert_eq!(n(&mut c, "SELECT a FROM dd.dbo.fresh"), "7");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM dd.sys.tables WHERE name = 'stay'").len().to_string(), "1");
}

#[test]
fn a_differential_needs_its_own_full_backup_as_the_base() {
    let e = env();
    let mut c = e.server.connect();
    exec(&mut c, "CREATE DATABASE d2");
    script(&mut c, "USE d2; CREATE TABLE t (id INT PRIMARY KEY); INSERT t VALUES (1);");
    let (f1, f2, df) = (e.file("f1.bak"), e.file("f2.bak"), e.file("df.dif"));
    exec(&mut c, &format!("BACKUP DATABASE d2 TO DISK = '{}'", f1));
    exec(&mut c, "INSERT d2.dbo.t VALUES (2)");
    exec(&mut c, &format!("BACKUP DATABASE d2 TO DISK = '{}'", f2));
    exec(&mut c, "INSERT d2.dbo.t VALUES (3)");
    exec(&mut c, &format!("BACKUP DATABASE d2 TO DISK = '{}' WITH DIFFERENTIAL", df));
    // a COPY_ONLY full backup does not move the base
    exec(&mut c, &format!("BACKUP DATABASE d2 TO DISK = '{}' WITH COPY_ONLY", e.file("copy.bak")));
    exec(&mut c, "DROP DATABASE d2");
    exec(&mut c, &format!("RESTORE DATABASE d2 FROM DISK = '{}' WITH NORECOVERY", f1));
    assert_eq!(err(&mut c, &format!("RESTORE DATABASE d2 FROM DISK = '{}'", df)).number, 3136, "the differential is based on the second full backup");
    exec(&mut c, "DROP DATABASE d2");
    exec(&mut c, &format!("RESTORE DATABASE d2 FROM DISK = '{}' WITH NORECOVERY", f2));
    exec(&mut c, &format!("RESTORE DATABASE d2 FROM DISK = '{}' WITH RECOVERY", df));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM d2.dbo.t"), "3");
    assert_eq!(err(&mut c, &format!("BACKUP LOG d2 TO DISK = '{}' WITH DIFFERENTIAL", e.file("x.trn"))).number, 155);
    assert_eq!(err(&mut c, &format!("BACKUP DATABASE d2 TO DISK = '{}' WITH DIFFERENTIAL, COPY_ONLY", e.file("y.dif"))).number, 155);
}
