//! Writing SQL Server backups (`BACKUP DATABASE ... WITH SQL_SERVER_FORMAT`) and reading them back with `RESTORE DATABASE`:
//! a database restored from its own backup must have the same tables, columns, rows, indexes, constraints and modules.
//!
//! The first test builds a database with every kind of column the writer handles. The second (slow, `--ignored`) restores
//! the sample `resources/AdventureWorks2025.bak`, backs the result up in this form, restores that and compares everything;
//! it does nothing when the sample is absent.
mod common;
use common::*;
use std::collections::BTreeMap;
use tsql_server::Connection;

/// A text that describes a database: its tables (columns, indexes, constraints), the rows of every table as a count and a
/// checksum, and the text of its modules.
fn describe(c: &mut Connection, db: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    exec(c, &format!("USE [{}]", db));
    let tables = rows(c, "SELECT s.name, t.name FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id ORDER BY 1, 2");
    for t in &tables {
        let full = format!("[{}].[{}]", t[0], t[1]);
        let cols = rows(c, &format!("SELECT c.column_id, c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, c.is_computed FROM sys.columns c JOIN sys.types ty ON ty.user_type_id = c.user_type_id WHERE c.object_id = OBJECT_ID('{}') ORDER BY c.column_id", full));
        let mut d = format!("columns {:?}\n", cols);
        let ix = rows(c, &format!("SELECT i.name, i.type, i.is_unique, i.is_primary_key, i.is_unique_constraint FROM sys.indexes i WHERE i.object_id = OBJECT_ID('{}') AND i.type > 0 ORDER BY i.name", full));
        d.push_str(&format!("indexes {:?}\n", ix));
        let ixc = rows(c, &format!("SELECT i.name, ic.key_ordinal, col.name, ic.is_descending_key, ic.is_included_column FROM sys.index_columns ic JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id WHERE ic.object_id = OBJECT_ID('{}') ORDER BY i.name, ic.key_ordinal, col.name", full));
        d.push_str(&format!("index columns {:?}\n", ixc));
        let defs = rows(c, &format!("SELECT d.name, col.name, d.definition FROM sys.default_constraints d JOIN sys.columns col ON col.object_id = d.parent_object_id AND col.column_id = d.parent_column_id WHERE d.parent_object_id = OBJECT_ID('{}') ORDER BY 1", full));
        d.push_str(&format!("defaults {:?}\n", defs));
        let chk = rows(c, &format!("SELECT name, definition FROM sys.check_constraints WHERE parent_object_id = OBJECT_ID('{}') ORDER BY 1", full));
        d.push_str(&format!("checks {:?}\n", chk));
        let fks = rows(c, &format!("SELECT f.name, OBJECT_NAME(f.referenced_object_id), f.delete_referential_action_desc, f.update_referential_action_desc FROM sys.foreign_keys f WHERE f.parent_object_id = OBJECT_ID('{}') ORDER BY 1", full));
        d.push_str(&format!("foreign keys {:?}\n", fks));
        let ident = rows(c, &format!("SELECT IDENT_CURRENT('{}')", full));
        d.push_str(&format!("identity {:?}\n", ident));
        // the rows: a count and an order-independent checksum of their text
        let r = exec(c, &format!("SELECT * FROM {}", full));
        let rs = r.resultsets();
        let (mut n, mut sum) = (0u64, 0u64);
        if let Some(rs) = rs.first() {
            for row in &rs.rows {
                let text: String = row.iter().map(|v| format!("{:?}", v)).collect::<Vec<_>>().join("\u{1}");
                n += 1;
                let mut h = 0xcbf29ce484222325u64;
                for b in text.bytes() {
                    h = (h ^ b as u64).wrapping_mul(0x100000001b3);
                }
                sum = sum.wrapping_add(h);
            }
        }
        d.push_str(&format!("rows {} checksum {:016x}\n", n, sum));
        out.insert(format!("table {}", full), d);
    }
    // objects that are not in a table: schemas, triggers, full-text objects, XML and spatial indexes, computed columns, key columns
    let extras: [(&str, &str); 10] = [
        ("schemas", "SELECT name FROM sys.schemas ORDER BY name"),
        ("triggers", "SELECT name, OBJECT_NAME(parent_id), is_disabled, is_instead_of_trigger FROM sys.triggers ORDER BY 1"),
        ("fulltext catalogs", "SELECT name, is_default, is_accent_sensitivity_on FROM sys.fulltext_catalogs ORDER BY 1"),
        ("fulltext indexes", "SELECT OBJECT_NAME(i.object_id), i.unique_index_id, i.change_tracking_state_desc, i.is_enabled, (SELECT name FROM sys.fulltext_catalogs k WHERE k.fulltext_catalog_id = i.fulltext_catalog_id) FROM sys.fulltext_indexes i ORDER BY 1"),
        ("fulltext columns", "SELECT OBJECT_NAME(object_id), column_id, language_id FROM sys.fulltext_index_columns ORDER BY 1, 2"),
        ("xml indexes", "SELECT OBJECT_NAME(object_id), name, xml_index_type, secondary_type, using_xml_index_id FROM sys.xml_indexes ORDER BY 1, 2"),
        ("spatial indexes", "SELECT OBJECT_NAME(object_id), name, spatial_index_type_desc FROM sys.spatial_indexes ORDER BY 1, 2"),
        ("computed columns", "SELECT OBJECT_NAME(object_id), name, definition, is_persisted FROM sys.computed_columns ORDER BY 1, 2"),
        ("foreign key columns", "SELECT f.name, pc.name, rc.name FROM sys.foreign_key_columns fc JOIN sys.foreign_keys f ON f.object_id = fc.constraint_object_id JOIN sys.columns pc ON pc.object_id = fc.parent_object_id AND pc.column_id = fc.parent_column_id JOIN sys.columns rc ON rc.object_id = fc.referenced_object_id AND rc.column_id = fc.referenced_column_id ORDER BY 1, fc.constraint_column_id"),
        ("key constraints", "SELECT name, type FROM sys.key_constraints ORDER BY 1"),
    ];
    for (what, sql) in extras {
        out.insert(what.to_string(), format!("{:?}", rows(c, sql)));
    }
    for m in rows(c, "SELECT s.name, o.name, o.type, m.definition FROM sys.sql_modules m JOIN sys.objects o ON o.object_id = m.object_id JOIN sys.schemas s ON s.schema_id = o.schema_id ORDER BY 1, 2") {
        out.insert(format!("module {}.{} {}", m[0], m[1], m[2]), m[3].clone());
    }
    out
}

fn compare(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) {
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let mut bad = vec![];
    for k in keys {
        if a.get(k) != b.get(k) {
            let (x, y) = (a.get(k).map(|s| s.as_str()).unwrap_or("(missing)"), b.get(k).map(|s| s.as_str()).unwrap_or("(missing)"));
            // the first line that differs, cut around the first differing character
            let (lx, ly) = x.lines().zip(y.lines()).find(|(p, q)| p != q).unwrap_or((x, y));
            let at = lx.chars().zip(ly.chars()).position(|(p, q)| p != q).unwrap_or(0).saturating_sub(60);
            let cut = |l: &str| l.chars().skip(at).take(240).collect::<String>();
            bad.push(format!("{}:
  original: ...{}
  restored: ...{}", k, cut(lx), cut(ly)));
        }
    }
    assert!(bad.is_empty(), "{} difference(s):
{}", bad.len(), bad.iter().take(8).cloned().collect::<Vec<_>>().join("
"));
}

#[test]
fn a_database_of_every_kind_of_column_survives_a_sql_server_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("types.bak");
    let path = path.to_string_lossy().replace('\\', "/");
    let mut c = conn();
    exec(&mut c, "CREATE DATABASE src");
    exec(&mut c, "USE src");
    exec(&mut c, "CREATE SCHEMA hr");
    exec(
        &mut c,
        "CREATE TABLE dbo.everything (
            id INT IDENTITY(10, 5) NOT NULL CONSTRAINT PK_everything PRIMARY KEY CLUSTERED,
            b BIT NOT NULL CONSTRAINT DF_everything_b DEFAULT (1), b2 BIT NULL, ti TINYINT NULL, si SMALLINT NULL, bi BIGINT NULL,
            d DECIMAL(12, 3) NULL, d2 DECIMAL(30, 10) NULL, m MONEY NULL, sm SMALLMONEY NULL, f FLOAT NULL, r REAL NULL,
            c CHAR(5) NULL, vc VARCHAR(30) NULL, nc NCHAR(4) NULL, nvc NVARCHAR(40) NULL, bn BINARY(4) NULL, vb VARBINARY(50) NULL,
            g UNIQUEIDENTIFIER NULL, dt DATETIME NULL, sdt SMALLDATETIME NULL, dd DATE NULL, tm TIME(3) NULL, dt2 DATETIME2(5) NULL,
            dto DATETIMEOFFSET(2) NULL, calc AS (id * 2), notes NVARCHAR(MAX) NULL, blob VARBINARY(MAX) NULL, x XML NULL,
            tx TEXT NULL, ntx NTEXT NULL, img IMAGE NULL, h HIERARCHYID NULL, v SQL_VARIANT NULL,
            CONSTRAINT CK_everything_ti CHECK (ti IS NULL OR ti < 200))",
    );
    exec(&mut c, "CREATE INDEX IX_everything_vc ON dbo.everything (vc DESC, ti) INCLUDE (si)");
    exec(&mut c, "CREATE TABLE hr.child (n INT NOT NULL, parent INT NOT NULL CONSTRAINT FK_child_parent FOREIGN KEY REFERENCES dbo.everything (id) ON DELETE CASCADE, label NVARCHAR(20) NULL CONSTRAINT UQ_child_label UNIQUE)");
    exec(&mut c, "CREATE TABLE dbo.heap (a INT, b VARCHAR(10))");
    exec(&mut c, "CREATE TABLE dbo.empty (a INT NOT NULL PRIMARY KEY)");
    exec(&mut c, "INSERT dbo.everything (b, b2, ti, si, bi, d, d2, m, sm, f, r, c, vc, nc, nvc, bn, vb, g, dt, sdt, dd, tm, dt2, dto, notes, blob, x, tx, ntx, img, h, v) VALUES (0, 1, 7, -300, 9000000000, 12345.678, 12345678901234567890.0123456789, 922.5025, -214.75, 3.141592653589793, 2.5, 'abc', 'caf\u{e9} \u{201c}x\u{201d}', N'\u{4e16}\u{754c}', N'Unicode \u{2603} text', 0x01020304, 0xDEADBEEF, '6F9619FF-8B86-D011-B42D-00C04FC964FF', '2024-03-04T05:06:07.123', '2024-03-04T05:07:00', '2024-03-04', '05:06:07.123', '2024-03-04T05:06:07.12345', '2024-03-04T05:06:07.12+02:00', N'short note', 0x00FF, '<a x=\"1\"><b>t &amp; u</b><!--c--><?p d?></a>', 'old text', N'old unicode text', 0x424344, hierarchyid::Parse('/1/2/'), CAST(42 AS INT))");
    exec(&mut c, "INSERT dbo.everything (b) VALUES (1)");
    exec(&mut c, "INSERT dbo.everything (b, vc, notes, blob, x, tx, v) VALUES (1, REPLICATE('v', 30), REPLICATE(CAST(N'\u{e9}\u{4e16}' AS NVARCHAR(MAX)), 20000), CAST(REPLICATE(CAST('blob' AS VARCHAR(MAX)), 30000) AS VARBINARY(MAX)), '<doc/>', REPLICATE(CAST('t' AS VARCHAR(MAX)), 50000), N'a variant')");
    // (a filtered index would come back without its filter, as the reader of SQL Server backups does not read filters)
    exec(&mut c, "UPDATE dbo.everything SET g = NEWID() WHERE g IS NULL");
    exec(&mut c, "CREATE UNIQUE INDEX IX_everything_g ON dbo.everything (g)");
    exec(&mut c, "INSERT hr.child VALUES (1, 10, N'one'), (2, 10, N'two'), (3, 15, NULL)");
    exec(&mut c, "INSERT dbo.heap VALUES (1, 'x'), (NULL, NULL), (3, 'zzz')");
    exec(&mut c, "CREATE VIEW dbo.v_big AS SELECT id, b FROM dbo.everything WHERE b = 1");
    exec(&mut c, "CREATE PROCEDURE dbo.p_count AS SELECT COUNT(*) FROM dbo.heap");
    exec(&mut c, "CREATE FUNCTION dbo.f_twice (@n INT) RETURNS INT AS BEGIN RETURN @n * 2 END");
    exec(&mut c, "CREATE TRIGGER dbo.tr_heap ON dbo.heap AFTER INSERT AS BEGIN SET NOCOUNT ON END");
    let before = describe(&mut c, "src");
    assert_eq!(before.keys().filter(|k| k.starts_with("table")).count(), 4);
    exec(&mut c, "USE master");
    exec(&mut c, &format!("BACKUP DATABASE src TO DISK = '{}' WITH SQL_SERVER_FORMAT", path));
    // the file is a Microsoft Tape Format file, as an uncompressed SQL Server backup is
    let head = std::fs::read(&path).unwrap();
    assert_eq!(&head[..4], b"TAPE");
    exec(&mut c, &format!("RESTORE VERIFYONLY FROM DISK = '{}'", path));
    exec(&mut c, &format!("RESTORE DATABASE dst FROM DISK = '{}'", path));
    let after = describe(&mut c, "dst");
    compare(&before, &after);
    // the restored database is usable
    exec(&mut c, "USE dst");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM dbo.everything"), "3");
    assert_eq!(scalar(&mut c, "SELECT dbo.f_twice(21)"), "42");
    // a second round: the restored database backs up and restores again to the same state
    exec(&mut c, "USE master");
    let path2 = format!("{}2", path);
    exec(&mut c, &format!("BACKUP DATABASE dst TO DISK = '{}' WITH SQL_SERVER_FORMAT", path2));
    exec(&mut c, &format!("RESTORE DATABASE dst2 FROM DISK = '{}'", path2));
    compare(&before, &describe(&mut c, "dst2"));
}

#[test]
fn the_native_format_is_still_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.bak").to_string_lossy().replace('\\', "/");
    let mut c = conn();
    exec(&mut c, "CREATE DATABASE nat");
    exec(&mut c, "USE nat");
    exec(&mut c, "CREATE TABLE t (a INT)");
    exec(&mut c, "INSERT t VALUES (1)");
    exec(&mut c, "USE master");
    exec(&mut c, &format!("BACKUP DATABASE nat TO DISK = '{}'", path));
    assert_eq!(&std::fs::read(&path).unwrap()[..8], b"TSQLBAK1");
    let e = err(&mut c, &format!("BACKUP LOG nat TO DISK = '{}' WITH SQL_SERVER_FORMAT", path));
    assert!(e.message.contains("SQL_SERVER_FORMAT"), "{}", e);
}

#[test]
#[ignore = "slow: restores the 760,000-row sample twice; run with --release -- --ignored"]
fn adventureworks_restored_backed_up_and_restored_again_is_identical() {
    let sample = concat!(env!("CARGO_MANIFEST_DIR"), "/resources/AdventureWorks2025.bak");
    if !std::path::Path::new(sample).exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mid = dir.path().join("aw_written.bak").to_string_lossy().replace('\\', "/");
    let mut c = conn();
    let t = std::time::Instant::now();
    exec(&mut c, &format!("RESTORE DATABASE aw1 FROM DISK = '{}'", sample.replace('\\', "/")));
    eprintln!("restored the sample in {:.1} s", t.elapsed().as_secs_f64());
    let t = std::time::Instant::now();
    exec(&mut c, &format!("BACKUP DATABASE aw1 TO DISK = '{}' WITH SQL_SERVER_FORMAT", mid));
    eprintln!("wrote a SQL Server backup of {} MB in {:.1} s", std::fs::metadata(&mid).unwrap().len() / 1_000_000, t.elapsed().as_secs_f64());
    let t = std::time::Instant::now();
    exec(&mut c, &format!("RESTORE DATABASE aw2 FROM DISK = '{}'", mid));
    eprintln!("restored it in {:.1} s", t.elapsed().as_secs_f64());
    let a = describe(&mut c, "aw1");
    let b = describe(&mut c, "aw2");
    eprintln!("compared {} tables and modules", a.len());
    assert!(a.len() > 100);
    compare(&a, &b);
}
