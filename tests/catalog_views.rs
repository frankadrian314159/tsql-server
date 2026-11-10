//! Catalog views and functions that GUI clients (DBeaver, SSMS, Azure Data Studio) query to build their navigators.
mod common;
use common::*;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE dbo.p (id INT IDENTITY PRIMARY KEY, name VARCHAR(10) CONSTRAINT df_name DEFAULT 'x', code INT UNIQUE);
         CREATE TABLE dbo.k (id INT PRIMARY KEY, pid INT CONSTRAINT fk_k FOREIGN KEY REFERENCES dbo.p (id), pid2 INT CONSTRAINT fk_k2 REFERENCES dbo.p (id));
         CREATE INDEX ix_k ON dbo.k (pid, pid2 DESC) INCLUDE (id);
         CREATE VIEW dbo.v AS SELECT id FROM dbo.p;
         CREATE SYNONYM dbo.sn FOR dbo.p;
         INSERT dbo.p (name, code) VALUES ('a', 1), ('b', 2), ('c', 3);",
    );
}

#[test]
fn extended_properties_exists_and_is_empty() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM master.sys.extended_properties WHERE class = 3"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.extended_properties ep JOIN sys.schemas s ON ep.major_id = s.schema_id WHERE ep.name = 'MS_Description'"), "0");
}

#[test]
fn foreign_keys_have_their_own_ids_and_columns() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT object_id) FROM sys.foreign_keys"), "2");
    let r = rows(&mut c, "SELECT fk.name, fkc.constraint_column_id, fkc.parent_column_id, fkc.referenced_column_id FROM sys.foreign_keys fk JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id ORDER BY fk.name");
    assert_eq!(r, [["fk_k", "1", "2", "1"], ["fk_k2", "1", "3", "1"]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.foreign_key_columns fkc JOIN sys.tables t ON t.object_id = fkc.parent_object_id AND t.name = 'k' JOIN sys.tables r ON r.object_id = fkc.referenced_object_id AND r.name = 'p'"), "2");
}

#[test]
fn default_constraints_join_to_their_columns() {
    let mut c = conn();
    setup(&mut c);
    let r = rows(&mut c, "SELECT dc.name, c.name, dc.definition FROM sys.default_constraints dc JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id");
    assert_eq!(r, [["df_name", "name", "('x')"]]);
    assert_eq!(scalar(&mut c, "SELECT dc.name FROM sys.columns c JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id WHERE c.name = 'name'"), "df_name");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.columns c WHERE c.name = 'code' AND c.default_object_id = 0"), "1");
}

#[test]
fn synonyms_are_listed() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "SELECT name, base_object_name, type FROM sys.synonyms"), [["sn", "[dbo].[p]", "SN"]]);
}

#[test]
fn indexes_report_type_uniqueness_and_key_ordinals() {
    let mut c = conn();
    setup(&mut c);
    let r = rows(&mut c, "SELECT i.name, i.type, i.type_desc, i.is_unique, i.is_primary_key, i.is_unique_constraint FROM sys.indexes i WHERE i.object_id = OBJECT_ID('dbo.p') ORDER BY i.index_id");
    assert_eq!(r.len(), 2);
    assert_eq!((r[0][1].as_str(), r[0][2].as_str(), r[0][3].as_str(), r[0][4].as_str(), r[0][5].as_str()), ("1", "CLUSTERED", "1", "1", "0"));
    assert_eq!((r[1][1].as_str(), r[1][2].as_str(), r[1][3].as_str(), r[1][4].as_str(), r[1][5].as_str()), ("2", "NONCLUSTERED", "1", "0", "1"));
    let r = rows(&mut c, "SELECT ic.column_id, ic.key_ordinal, ic.is_descending_key, ic.is_included_column FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id WHERE i.name = 'ix_k' ORDER BY ic.is_included_column, ic.index_column_id");
    assert_eq!(r, [["2", "1", "0", "0"], ["3", "2", "1", "0"], ["1", "0", "0", "1"]]);
}

#[test]
fn objects_types_partitions_and_files() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.all_objects WHERE is_ms_shipped = 1"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE is_ms_shipped = 0"), "2");
    let r = rows(&mut c, "SELECT name, max_length, precision, scale, is_nullable FROM sys.types WHERE name IN ('int', 'varchar', 'decimal', 'datetime2') ORDER BY name");
    assert_eq!(r, [["datetime2", "8", "27", "7", "1"], ["decimal", "17", "18", "0", "1"], ["int", "4", "10", "0", "1"], ["varchar", "8000", "0", "0", "1"]]);
    assert_eq!(scalar(&mut c, "SELECT row_count FROM sys.dm_db_partition_stats WHERE object_id = OBJECT_ID('dbo.p')"), "3");
    assert_eq!(scalar(&mut c, "SELECT SUM(rows) FROM sys.partitions WHERE object_id = OBJECT_ID('dbo.p') AND index_id IN (0, 1)"), "3");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.database_files WHERE type_desc = 'ROWS'"), "1");
}

#[test]
fn object_definition_returns_the_text_of_modules() {
    let mut c = conn();
    setup(&mut c);
    script(&mut c, "CREATE PROCEDURE dbo.pr AS SELECT 1;\nGO\nCREATE FUNCTION dbo.fn() RETURNS INT AS BEGIN RETURN 1; END;\nGO\nCREATE TRIGGER dbo.tr ON dbo.p AFTER INSERT AS SELECT 1;");
    for (obj, needle) in [("dbo.v", "CREATE VIEW dbo.v"), ("dbo.pr", "CREATE PROCEDURE dbo.pr"), ("dbo.fn", "CREATE FUNCTION dbo.fn"), ("dbo.tr", "CREATE TRIGGER dbo.tr")] {
        let text = scalar(&mut c, &format!("SELECT OBJECT_DEFINITION(OBJECT_ID('{}'))", obj));
        assert!(text.contains(needle), "{}: {}", obj, text);
    }
    assert_eq!(scalar(&mut c, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.p'))"), "NULL");
    assert_eq!(scalar(&mut c, "SELECT OBJECT_DEFINITION(NULL)"), "NULL");
}

#[test]
fn sp_help_and_sp_columns_accept_schema_qualified_names() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(rows(&mut c, "EXEC sp_columns 'dbo.p'").len(), 3);
    assert_eq!(rows(&mut c, "EXEC sp_columns '[dbo].[p]'").len(), 3);
    assert_eq!(rows(&mut c, "EXEC sp_help 'dbo.p'").len(), 3);
}

#[test]
fn computed_columns_are_listed() {
    let mut c = conn();
    setup(&mut c);
    script(&mut c, "CREATE TABLE dbo.cc (a INT, b INT, s AS a + b)");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM master.sys.computed_columns WHERE object_id = OBJECT_ID('dbo.cc')"), "1");
    assert_eq!(scalar(&mut c, "SELECT name FROM sys.computed_columns WHERE object_id = OBJECT_ID('dbo.cc')"), "s");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.computed_columns WHERE object_id = OBJECT_ID('dbo.p')"), "0");
}
