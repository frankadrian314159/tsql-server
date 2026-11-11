# XML Schema collections and typed xml

`CREATE XML SCHEMA COLLECTION [schema.]name AS 'xsd text' | @variable` compiles one or more schema documents (they may be
written one after another in the text), and `ALTER XML SCHEMA COLLECTION name ADD 'xsd text'` adds more (the whole collection is
compiled again, so a document may refer to the namespaces of another). `DROP XML SCHEMA COLLECTION [IF EXISTS] name` removes
one that no column uses (6906). A schema that cannot be compiled is error 6905 with the reason; a collection that is not there is
6903 and one that exists is 6904.

A column declared `xml(DOCUMENT sc)` or `xml(CONTENT sc)` (`xml(sc)` is CONTENT) is **validated on every `INSERT` and
`UPDATE`**: DOCUMENT needs exactly one root element and no text outside it, CONTENT allows any number of elements and text, and
every top-level element must be declared by the collection. A value that is not well-formed is error 9400. A column can be typed in `CREATE
TABLE` and `ALTER TABLE ... ADD`, and the type is kept by scripts, backups of this server's format and restores of them.

`sys.xml_schema_collections`, `sys.xml_schema_namespaces` and `XML_SCHEMA_NAMESPACE(schema, collection [, namespace])` (the
original text of the schemas, not a reconstruction) are there.

## The validator (`src/xsd.rs`)

**Supported**: elements (global, local, `ref`, `nillable`, `default`, `fixed`, `minOccurs`, `maxOccurs`), attributes (`use`,
`default`, `fixed`, `ref`), complex types with `sequence`, `choice`, `all`, `group` references, `any` and `anyAttribute`
(`processContents` strict, lax and skip), mixed content, `simpleContent` and `complexContent` by extension and by restriction,
`attributeGroup`, simple types by restriction (every facet), `list` and `union`, the built-in types (their lexical forms are
checked and the range of the integer types is enforced), target namespaces, `elementFormDefault`, `attributeFormDefault`,
`xsi:nil` and `xsi:type`, and several schemas in one collection.

**Not supported**: `redefine`, substitution groups, identity constraints (`key`, `keyref`, `unique` are read and ignored), the check
that a restriction restricts its base or that a content model is deterministic, `ID`/`IDREF` uniqueness, and the full lexical rules of `QName`.
Patterns use the syntax of the `regex` crate: XSD's `\i`, `\c` and `\p{IsBlock}` make the schema fail to compile. `import` and `include` do not fetch anything: what a schema
needs must be in the collection. The range facets compare numbers as numbers and dates and times by their text (right within one
format; time zones are not taken into account).

**Typed values.** Typed columns are validated, nothing more: `.value()`, `.exist()` and the other methods read a typed column
as they read an untyped one (text, with their own conversions), and typed `xml` variables and parameters accept any xml.

**Errors.** Validation stops at the first error. The numbers follow those of SQL Server's messages where they are remembered (6913
no declaration for an element, 6914 for an attribute, 6926 a value that is not valid for its type, 6965 content that does not fit the
content model, with the expected elements and the one found, 6909 a missing attribute, 6910 a nil problem) and have not been compared with
a real server; the wording follows `XML Validation: ... Location: /*:a[1]/*:b[2]`.

## Backups from SQL Server

A SQL Server backup holds a schema collection as compiled components (`sysxmlcomponent` and its neighbours), not as XSD text. They
are not read: a restore creates the typed columns of AdventureWorks as untyped `xml` columns with their values (see
[backup.md](backup.md)).
