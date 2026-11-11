//! The xml methods `.value()`, `.exist()` and `.query()` over simple paths.
mod common;
use common::*;

#[test]
fn value_exist_query_on_variables_and_columns() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE d (id INT, doc XML)");
    exec(&mut c, "INSERT d VALUES (1, '<order id=\"7\"><cust>ann</cust><line q=\"2\">pen</line><line q=\"5\">ink</line></order>')");
    assert_eq!(scalar(&mut c, "SELECT doc.value('(/order/cust)[1]', 'varchar(20)') FROM d"), "ann");
    assert_eq!(scalar(&mut c, "SELECT doc.value('(/order/@id)[1]', 'int') + 1 FROM d"), "8");
    assert_eq!(scalar(&mut c, "SELECT doc.value('(/order/line/@q)[2]', 'int') FROM d"), "5");
    assert_eq!(scalar(&mut c, "SELECT doc.value('/order/line[@q=\"5\"]', 'varchar(10)') FROM d"), "ink");
    assert_eq!(scalar(&mut c, "SELECT doc.exist('/order/line[@q=\"2\"]') FROM d"), "1");
    assert_eq!(scalar(&mut c, "SELECT doc.exist('/order/missing') FROM d"), "0");
    assert_eq!(scalar(&mut c, "SELECT doc.query('/order/cust') FROM d"), "<cust>ann</cust>");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM d WHERE doc.exist('/order/line') = 1"), "1");
    assert_eq!(scalar(&mut c, "DECLARE @x XML = '<a><b>5</b></a>'; SELECT @x.value('(/a/b)[1]', 'int') * 2"), "10");
    assert_eq!(scalar(&mut c, "SELECT doc.value('(/order/none)[1]', 'int') FROM d"), "NULL");
    assert_eq!(err_no(&mut c, "SELECT CAST('<a>' AS XML).value('(/a)[1]', 'int')"), 9400);
}

#[test]
fn nodes_shreds_xml_into_rows() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE d (id INT, doc XML)");
    exec(&mut c, "INSERT d VALUES (1, '<order id=\"7\"><line q=\"2\">pen</line><line q=\"5\">ink</line></order>'), (2, '<order id=\"8\"><line q=\"1\">cap</line></order>'), (3, NULL)");
    // CROSS APPLY a column's nodes; relative paths in the methods of a node start at the node
    assert_eq!(
        rows(&mut c, "SELECT d.id, l.ref.value('@q', 'int'), l.ref.value('(.)[1]', 'varchar(10)') FROM d CROSS APPLY d.doc.nodes('/order/line') AS l(ref) ORDER BY d.id, 2"),
        vec![vec!["1", "2", "pen"], vec!["1", "5", "ink"], vec!["2", "1", "cap"]]
    );
    // OUTER APPLY keeps the row without nodes
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM d OUTER APPLY d.doc.nodes('/order/line') AS l(ref)"), "4");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM d CROSS APPLY doc.nodes('/order/line') AS l(ref)"), "3");
    // a node is xml
    assert_eq!(rows(&mut c, "SELECT l.ref.query('.') FROM d CROSS APPLY d.doc.nodes('/order/line[@q=\"5\"]') AS l(ref)"), vec![vec!["<line q=\"5\">ink</line>"]]);
    assert_eq!(scalar(&mut c, "SELECT l.ref.exist('@q') FROM d CROSS APPLY d.doc.nodes('/order/line[1]') AS l(ref) WHERE d.id = 2"), "1");
    // nodes of a variable, and nodes of a node
    assert_eq!(
        rows(&mut c, "DECLARE @x XML = '<r><g n=\"a\"><i>1</i><i>2</i></g><g n=\"b\"><i>3</i></g></r>'; SELECT g.ref.value('@n', 'char(1)'), i.ref.value('.', 'int') FROM @x.nodes('/r/g') AS g(ref) CROSS APPLY g.ref.nodes('i') AS i(ref) ORDER BY 2"),
        vec![vec!["a", "1"], vec!["a", "2"], vec!["b", "3"]]
    );
    // namespaces: a prolog is accepted, prefixes are matched by their local part
    assert_eq!(
        rows(&mut c, "DECLARE @x XML = '<p:r xmlns:p=\"urn:p\"><p:v>5</p:v><p:v>6</p:v></p:r>'; SELECT n.ref.value('declare namespace q=\"urn:p\"; (q:v)[1]', 'int') FROM @x.nodes('declare namespace q=\"urn:p\"; /q:r') AS n(ref)"),
        vec![vec!["5"]]
    );
    // sql:variable and sql:column in a path
    assert_eq!(scalar(&mut c, "DECLARE @q INT = 5; SELECT COUNT(*) FROM d CROSS APPLY d.doc.nodes('/order/line[@q = sql:variable(\"@q\")]') AS l(ref)"), "1");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM d WHERE d.doc.exist('/order[@id = sql:column(\"d.id\") + 6]') = 1"), "2");
    // nodes() needs an alias with a column, and elements
    assert_eq!(err_no(&mut c, "SELECT * FROM d CROSS APPLY d.doc.nodes('/order/line/@q') AS l(ref)"), 2225);
}

#[test]
fn modify_changes_xml_in_variables_and_columns() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE p (id INT PRIMARY KEY, demo XML NULL)");
    exec(&mut c, "INSERT p VALUES (1, '<S xmlns=\"urn:s\"><Total>10</Total></S>'), (2, '<S xmlns=\"urn:s\"><Year>1999</Year></S>'), (3, NULL)");
    exec(&mut c, "CREATE TABLE sales (pid INT, line DECIMAL(9,2))");
    exec(&mut c, "INSERT sales VALUES (1, 2.5), (1, 4)");
    // insert, with the namespace prolog of AdventureWorks, only where it is missing
    exec(&mut c, "UPDATE p SET demo.modify(N'declare default element namespace \"urn:s\"; insert <Total>0.00</Total> as first into (/S)[1]') WHERE demo.exist(N'declare default element namespace \"urn:s\"; /S/Total') <> 1");
    assert_eq!(scalar(&mut c, "SELECT CAST(demo AS NVARCHAR(200)) FROM p WHERE id = 2"), "<S xmlns=\"urn:s\"><Total>0.00</Total><Year>1999</Year></S>");
    assert_eq!(scalar(&mut c, "SELECT CAST(demo AS NVARCHAR(200)) FROM p WHERE id = 1"), "<S xmlns=\"urn:s\"><Total>10</Total></S>");
    assert_eq!(scalar(&mut c, "SELECT demo.exist('/S') FROM p WHERE id = 3"), "NULL", "NULL stays NULL");
    // replace value with the value of a column of the joined table (sql:column), as the AdventureWorks triggers do
    exec(&mut c, "UPDATE p SET demo.modify(N'declare default element namespace \"urn:s\"; replace value of (/S/Total)[1] with data(/S/Total)[1] + sql:column(\"s.line\")') FROM p JOIN sales s ON s.pid = p.id");
    assert_eq!(scalar(&mut c, "SELECT demo.value('declare default element namespace \"urn:s\"; (/S/Total)[1]', 'decimal(9,2)') FROM p WHERE id = 1"), "12.50", "an UPDATE ... FROM applies one of the joined rows");
    // variables: delete, insert an attribute, replace with a variable
    assert_eq!(
        scalar(&mut c, "DECLARE @x XML = '<a><b>1</b><c/></a>'; DECLARE @n INT = 9; SET @x.modify('delete /a/c'); SET @x.modify('insert attribute k {\"v\"} into (/a)[1]'); SET @x.modify('replace value of (/a/b/text())[1] with sql:variable(\"@n\")'); SELECT CAST(@x AS NVARCHAR(100))"),
        "<a k=\"v\"><b>9</b></a>"
    );
    // a bad statement is an error and the xml stays
    assert_eq!(err_no(&mut c, "DECLARE @x XML = '<a/>'; SET @x.modify('rename node /a as b')"), 2225);
    assert_eq!(err_no(&mut c, "DECLARE @x XML = '<a><b/><b/></a>'; SET @x.modify('insert <c/> into /a/b')"), 2225);
}
