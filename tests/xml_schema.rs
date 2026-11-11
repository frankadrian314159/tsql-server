//! XML schema collections and typed xml columns (see src/xsd.rs and src/engine/xmlschema.rs).
mod common;
use common::*;

const SCHEMA: &str = r#"<xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:books" xmlns="urn:books" elementFormDefault="qualified">
  <xsd:element name="book">
    <xsd:complexType>
      <xsd:sequence>
        <xsd:element name="title" type="xsd:string"/>
        <xsd:element name="pages" type="xsd:positiveInteger" minOccurs="0"/>
        <xsd:element name="author" type="xsd:string" maxOccurs="unbounded"/>
      </xsd:sequence>
      <xsd:attribute name="isbn" type="xsd:string" use="required"/>
    </xsd:complexType>
  </xsd:element>
</xsd:schema>"#;

fn setup(c: &mut tsql_server::Connection) {
    exec(c, &format!("CREATE XML SCHEMA COLLECTION dbo.BookSchema AS N'{}'", SCHEMA.replace('\'', "''")));
    script(
        c,
        "CREATE TABLE books (id INT PRIMARY KEY, doc xml(DOCUMENT dbo.BookSchema) NULL, frag xml(CONTENT dbo.BookSchema) NULL, plain xml NULL);",
    );
}

const GOOD: &str = r#"<book xmlns="urn:books" isbn="1"><title>T</title><pages>12</pages><author>A</author><author>B</author></book>"#;

#[test]
fn typed_columns_accept_valid_documents_and_refuse_the_rest() {
    let mut c = conn();
    setup(&mut c);
    exec(&mut c, &format!("INSERT INTO books (id, doc) VALUES (1, '{}')", GOOD));
    assert_eq!(scalar(&mut c, "SELECT doc.value('declare namespace b=\"urn:books\"; (/b:book/b:title)[1]', 'varchar(10)') FROM books"), "T");
    // what is wrong, and the error that says so
    let bad = |c: &mut tsql_server::Connection, xml: &str| err_no(c, &format!("INSERT INTO books (id, doc) VALUES (2, '{}')", xml));
    assert_eq!(bad(&mut c, r#"<book xmlns="urn:books"><title>T</title><author>A</author></book>"#), 6909, "isbn is required");
    assert_eq!(bad(&mut c, r#"<book xmlns="urn:books" isbn="1"><title>T</title><pages>0</pages><author>A</author></book>"#), 6926, "pages is positive");
    assert_eq!(bad(&mut c, r#"<book xmlns="urn:books" isbn="1"><title>T</title></book>"#), 6965, "an author is required");
    assert_eq!(bad(&mut c, r#"<book xmlns="urn:books" isbn="1"><author>A</author><title>T</title></book>"#), 6965, "the order matters");
    assert_eq!(bad(&mut c, r#"<book isbn="1"><title>T</title><author>A</author></book>"#), 6913, "the namespace is part of the name");
    assert_eq!(bad(&mut c, r#"<book xmlns="urn:books" isbn="1" extra="x"><title>T</title><author>A</author></book>"#), 6914);
    assert_eq!(bad(&mut c, "<book"), 9400, "not well-formed");
    // a DOCUMENT has one root; CONTENT has any number of elements
    assert_eq!(bad(&mut c, &format!("{}{}", GOOD, GOOD)), 6965);
    exec(&mut c, &format!("INSERT INTO books (id, frag) VALUES (3, '{}{}')", GOOD, GOOD));
    // an update is checked too, and a column that is not typed accepts anything
    assert_eq!(err_no(&mut c, "UPDATE books SET doc = '<other/>' WHERE id = 1"), 6913);
    exec(&mut c, "UPDATE books SET plain = '<anything/>' WHERE id = 1");
    exec(&mut c, "UPDATE books SET doc = NULL WHERE id = 1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM books"), "2");
}

#[test]
fn collections_are_listed_extended_and_dropped() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(scalar(&mut c, "SELECT name FROM sys.xml_schema_collections"), "BookSchema");
    assert_eq!(scalar(&mut c, "SELECT n.name FROM sys.xml_schema_namespaces n JOIN sys.xml_schema_collections s ON s.xml_collection_id = n.xml_collection_id"), "urn:books");
    assert_eq!(scalar(&mut c, "SELECT CAST(XML_SCHEMA_NAMESPACE(N'dbo', N'BookSchema', N'urn:books') AS NVARCHAR(MAX)) LIKE N'%targetNamespace=\"urn:books\"%'"), "1");
    assert_eq!(scalar(&mut c, "SELECT XML_SCHEMA_NAMESPACE(N'dbo', N'BookSchema', N'urn:none')"), "NULL");
    // ALTER ... ADD brings in another schema
    exec(&mut c, "ALTER XML SCHEMA COLLECTION dbo.BookSchema ADD N'<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"note\" type=\"xs:string\"/></xs:schema>'");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.xml_schema_namespaces"), "2");
    exec(&mut c, "INSERT INTO books (id, frag) VALUES (1, '<note>hi</note>')");
    // errors
    assert_eq!(err_no(&mut c, "CREATE XML SCHEMA COLLECTION dbo.BookSchema AS N'<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"/>'"), 6904);
    assert_eq!(err_no(&mut c, "CREATE XML SCHEMA COLLECTION dbo.Bad AS N'<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"a\" type=\"nothere\"/></xs:schema>'"), 6905);
    assert_eq!(err_no(&mut c, "CREATE XML SCHEMA COLLECTION dbo.Bad AS N'not xml'"), 6905);
    assert_eq!(err_no(&mut c, "CREATE TABLE t (x xml(dbo.Nope))"), 6903);
    assert_eq!(err_no(&mut c, "ALTER XML SCHEMA COLLECTION dbo.Nope ADD N'<a/>'"), 6903);
    assert_eq!(err_no(&mut c, "DROP XML SCHEMA COLLECTION dbo.BookSchema"), 6906, "columns are typed with it");
    exec(&mut c, "DROP TABLE books");
    exec(&mut c, "DROP XML SCHEMA COLLECTION dbo.BookSchema");
    exec(&mut c, "DROP XML SCHEMA COLLECTION IF EXISTS dbo.BookSchema");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.xml_schema_collections"), "0");
}

#[test]
fn a_schema_from_a_variable_and_a_typed_column_added_later_and_a_backup() {
    let mut c = conn();
    exec(&mut c, "DECLARE @s nvarchar(max) = N'<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"n\" type=\"xs:int\"/></xs:schema>'; CREATE XML SCHEMA COLLECTION dbo.Nums AS @s");
    script(&mut c, "CREATE TABLE t (id INT PRIMARY KEY); ALTER TABLE t ADD v xml(DOCUMENT dbo.Nums) NULL;");
    exec(&mut c, "INSERT INTO t VALUES (1, '<n>5</n>')");
    assert_eq!(err_no(&mut c, "INSERT INTO t VALUES (2, '<n>five</n>')"), 6926);
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("x.bak").to_string_lossy().replace('\\', "/");
    exec(&mut c, "CREATE DATABASE xs1");
    exec(&mut c, "USE xs1");
    script(
        &mut c,
        "CREATE XML SCHEMA COLLECTION dbo.Nums AS N'<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"n\" type=\"xs:int\"/></xs:schema>';
         CREATE TABLE t (id INT PRIMARY KEY, v xml(DOCUMENT dbo.Nums));
         INSERT INTO t VALUES (1, '<n>7</n>');",
    );
    exec(&mut c, &format!("BACKUP DATABASE xs1 TO DISK = '{}' WITH INIT", f));
    exec(&mut c, &format!("RESTORE DATABASE xs2 FROM DISK = '{}'", f));
    exec(&mut c, "USE xs2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.xml_schema_collections"), "1");
    assert_eq!(err_no(&mut c, "INSERT INTO t VALUES (2, '<n>x</n>')"), 6926, "the restored column is still typed");
}

#[test]
fn the_script_of_a_table_keeps_the_type_of_a_typed_column() {
    use tsql_server::tools;
    let mut c = conn();
    setup(&mut c);
    let text = tools::script_table(&c.table_info("books").unwrap());
    assert!(text.contains("xml(DOCUMENT [dbo].[bookschema])") && text.contains("xml(CONTENT [dbo].[bookschema])"), "{text}");
}
