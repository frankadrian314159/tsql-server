//! XML indexes. They are kept in the catalog and change no result: XPath runs over the stored text.
mod common;
use common::*;
use tsql_server::tools;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE doc (id INT PRIMARY KEY, body XML, other XML NULL, title VARCHAR(20));
         INSERT INTO doc VALUES (1, '<a><b>x</b></a>', NULL, 't1');
         INSERT INTO doc VALUES (2, '<a><b>y</b></a>', '<o/>', 't2');
         CREATE PRIMARY XML INDEX pxml_body ON doc (body);
         CREATE XML INDEX xv_body ON doc (body) USING XML INDEX pxml_body FOR VALUE;
         CREATE XML INDEX xp_body ON doc (body) USING XML INDEX pxml_body FOR PATH;
         CREATE XML INDEX xr_body ON doc (body) USING XML INDEX pxml_body FOR PROPERTY;
         CREATE PRIMARY XML INDEX pxml_other ON doc (other);",
    );
}

#[test]
fn xml_indexes_show_in_the_catalog_and_change_no_result() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(
        rows(&mut c, "SELECT name, type, type_desc FROM sys.indexes WHERE type = 3 ORDER BY index_id"),
        vec![vec!["pxml_body", "3", "XML"], vec!["xv_body", "3", "XML"], vec!["xp_body", "3", "XML"], vec!["xr_body", "3", "XML"], vec!["pxml_other", "3", "XML"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT name, xml_index_type_description, secondary_type, secondary_type_desc FROM sys.xml_indexes ORDER BY index_id"),
        vec![
            vec!["pxml_body", "PRIMARY_XML", "NULL", "NULL"],
            vec!["xv_body", "SECONDARY_XML", "V", "VALUE"],
            vec!["xp_body", "SECONDARY_XML", "P", "PATH"],
            vec!["xr_body", "SECONDARY_XML", "R", "PROPERTY"],
            vec!["pxml_other", "PRIMARY_XML", "NULL", "NULL"],
        ]
    );
    // a secondary index names the primary one it is built on
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.xml_indexes s JOIN sys.xml_indexes p ON s.using_xml_index_id = p.index_id WHERE p.name = 'pxml_body'"), "3");
    // the indexed column
    assert_eq!(scalar(&mut c, "SELECT c.name FROM sys.index_columns ic JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id WHERE i.name = 'xp_body'"), "body");
    // queries are as before
    assert_eq!(rows(&mut c, "SELECT id FROM doc WHERE body.exist('/a/b') = 1 ORDER BY id"), vec![vec!["1"], vec!["2"]]);
    assert_eq!(scalar(&mut c, "SELECT body.value('(/a/b)[1]', 'varchar(5)') FROM doc WHERE id = 2"), "y");
    exec(&mut c, "INSERT INTO doc VALUES (3, '<a><b>z</b></a>', NULL, 't3')");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM doc"), "3");
}

#[test]
fn an_xml_index_needs_a_primary_key_an_xml_column_and_a_primary_index_for_a_secondary() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE heap (x XML, n INT)");
    assert_eq!(err(&mut c, "CREATE PRIMARY XML INDEX p ON heap (x)").number, 6332);
    exec(&mut c, "CREATE TABLE doc (id INT PRIMARY KEY, body XML, n INT)");
    assert_eq!(err(&mut c, "CREATE PRIMARY XML INDEX p ON doc (n)").number, 6324, "not an xml column");
    assert_eq!(err(&mut c, "CREATE PRIMARY XML INDEX p ON doc (nope)").number, 1911);
    assert_eq!(err(&mut c, "CREATE XML INDEX s ON doc (body) USING XML INDEX nope FOR VALUE").number, 6324, "no primary index");
    exec(&mut c, "CREATE PRIMARY XML INDEX p ON doc (body)");
    assert_eq!(err(&mut c, "CREATE PRIMARY XML INDEX p2 ON doc (body)").number, 6324, "one primary per column");
    assert_eq!(err(&mut c, "CREATE XML INDEX p ON doc (body) USING XML INDEX p").number, 1913, "the name is taken");
    assert_eq!(err(&mut c, "CREATE INDEX p ON doc (n)").number, 1913, "also by an ordinary index");
}

#[test]
fn dropping_the_primary_xml_index_drops_the_secondary_ones() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, "DROP INDEX xr_body ON doc");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.xml_indexes"), "4");
    exec(&mut c, "DROP INDEX pxml_body ON doc");
    assert_eq!(rows(&mut c, "SELECT name FROM sys.xml_indexes"), vec![vec!["pxml_other"]]);
    assert_eq!(err(&mut c, "DROP INDEX pxml_body ON doc").number, 3703);
    exec(&mut c, "DROP INDEX IF EXISTS pxml_body ON doc");
}

#[test]
fn the_xml_column_and_the_primary_key_stay_while_an_xml_index_needs_them() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(err(&mut c, "ALTER TABLE doc DROP COLUMN body").number, 5074);
    let pk = scalar(&mut c, "SELECT name FROM sys.indexes WHERE is_primary_key = 1");
    assert_eq!(err(&mut c, &format!("ALTER TABLE doc DROP CONSTRAINT {}", pk)).number, 6332);
    // a column before the xml one can go; the index follows its column
    exec(&mut c, "CREATE TABLE d2 (junk INT NULL, id INT PRIMARY KEY, body XML)");
    exec(&mut c, "CREATE PRIMARY XML INDEX p ON d2 (body)");
    exec(&mut c, "ALTER TABLE d2 DROP COLUMN junk");
    assert_eq!(scalar(&mut c, "SELECT c.name FROM sys.index_columns ic JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id WHERE i.name = 'p'"), "body");
    exec(&mut c, "DROP TABLE d2");
}

#[test]
fn scripts_and_backups_keep_xml_indexes() {
    let mut c = conn();
    setup(&mut c);
    let script_text = tools::script_table(&c.table_info("doc").unwrap());
    let p = script_text.find("CREATE PRIMARY XML INDEX [pxml_body] ON [dbo].[doc] ([body]);").unwrap_or_else(|| panic!("{script_text}"));
    let s = script_text.find("CREATE XML INDEX [xp_body] ON [dbo].[doc] ([body]) USING XML INDEX [pxml_body] FOR PATH;").unwrap_or_else(|| panic!("{script_text}"));
    assert!(p < s, "the primary index comes first");
    assert!(script_text.contains("USING XML INDEX [pxml_body] FOR PROPERTY;") && script_text.contains("USING XML INDEX [pxml_body] FOR VALUE;"), "{script_text}");
    // the script runs on a fresh server with the same result
    let mut d = conn();
    script(&mut d, &script_text);
    assert_eq!(scalar(&mut d, "SELECT COUNT(*) FROM sys.xml_indexes"), "5");
    // a backup brings them along
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("x.bak").to_string_lossy().replace('\\', "/");
    exec(&mut c, "CREATE DATABASE xdb");
    exec(&mut c, "USE xdb");
    script(&mut c, "CREATE TABLE t (id INT PRIMARY KEY, x XML); CREATE PRIMARY XML INDEX px ON t (x); CREATE XML INDEX sx ON t (x) USING XML INDEX px FOR PATH; INSERT INTO t VALUES (1, '<r/>');");
    exec(&mut c, &format!("BACKUP DATABASE xdb TO DISK = '{}' WITH INIT", f));
    exec(&mut c, &format!("RESTORE DATABASE xdb2 FROM DISK = '{}'", f));
    exec(&mut c, "USE xdb2");
    assert_eq!(rows(&mut c, "SELECT name, secondary_type FROM sys.xml_indexes ORDER BY index_id"), vec![vec!["px", "NULL"], vec!["sx", "P"]]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM t"), "1");
}
