//! OPENXML with sp_xml_preparedocument / sp_xml_removedocument.
mod common;
use common::*;

const DOC: &str = "<ROOT>\
  <Customer CustomerID=\"VINET\" ContactName=\"Paul Henriot\">\
    <Order OrderID=\"10248\" CustomerID=\"VINET\" OrderDate=\"1996-07-04\">\
      <OrderDetail ProductID=\"11\" Quantity=\"12\"/>\
      <OrderDetail ProductID=\"42\" Quantity=\"10\"/>\
    </Order>\
  </Customer>\
  <Customer CustomerID=\"LILAS\" ContactName=\"Carlos Gonzalez\">\
    <Order OrderID=\"10283\" CustomerID=\"LILAS\" OrderDate=\"1996-08-16\">\
      <OrderDetail ProductID=\"72\" Quantity=\"3\"/>\
    </Order>\
  </Customer>\
  <Item><Name>pen</Name><Price>1.50</Price></Item>\
  <Item><Name>ink</Name><Price>7.25</Price></Item>\
</ROOT>";

fn run(c: &mut tsql_server::Connection, body: &str) -> Vec<Vec<String>> {
    rows(
        c,
        &format!(
            "DECLARE @h INT, @x NVARCHAR(MAX) = N'{}';
             EXEC sp_xml_preparedocument @h OUTPUT, @x;
             {}
             EXEC sp_xml_removedocument @h;",
            DOC, body
        ),
    )
}

#[test]
fn attribute_centric_mapping_with_a_with_clause() {
    let mut c = conn();
    let r = run(&mut c, "SELECT * FROM OPENXML(@h, '/ROOT/Customer', 1) WITH (CustomerID VARCHAR(10), ContactName VARCHAR(20)) ORDER BY CustomerID;");
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["LILAS|Carlos Gonzalez", "VINET|Paul Henriot"]);
    // the default (flags 0) is attribute-centric too; typed columns; a pattern with the parent axis
    let r = run(&mut c, "SELECT OrderID, CustomerID, OrderDate, ProdID, Qty FROM OPENXML(@h, '/ROOT/Customer/Order/OrderDetail') WITH (OrderID INT '../@OrderID', CustomerID VARCHAR(10) '../@CustomerID', OrderDate DATE '../@OrderDate', ProdID INT '@ProductID', Qty INT '@Quantity') ORDER BY ProdID;");
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["10248|VINET|1996-07-04|11|12", "10248|VINET|1996-07-04|42|10", "10283|LILAS|1996-08-16|72|3"]);
}

#[test]
fn element_centric_mapping_and_flags() {
    let mut c = conn();
    let r = run(&mut c, "SELECT * FROM OPENXML(@h, '/ROOT/Item', 2) WITH (Name VARCHAR(10), Price DECIMAL(6,2)) ORDER BY Name;");
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["ink|7.25", "pen|1.50"]);
    // a column that is not there is NULL; flags 3 read an attribute, then an element
    let r = run(&mut c, "SELECT * FROM OPENXML(@h, '/ROOT/Item', 3) WITH (Name VARCHAR(10), Missing INT) ORDER BY Name;");
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["ink|NULL", "pen|NULL"]);
    // text() and '.' patterns
    let r = run(&mut c, "SELECT * FROM OPENXML(@h, '/ROOT/Item/Name') WITH (v VARCHAR(10) 'text()', w VARCHAR(10) '.') ORDER BY v;");
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["ink|ink", "pen|pen"]);
}

#[test]
fn with_the_name_of_a_table() {
    let mut c = conn();
    exec(&mut c, "CREATE TABLE cust (CustomerID VARCHAR(10), ContactName VARCHAR(20))");
    let r = run(&mut c, "SELECT * FROM OPENXML(@h, '/ROOT/Customer', 1) WITH cust ORDER BY CustomerID;");
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["LILAS|Carlos Gonzalez", "VINET|Paul Henriot"]);
    // used for an INSERT ... SELECT
    exec(
        &mut c,
        &format!(
            "DECLARE @h INT, @x NVARCHAR(MAX) = N'{}';
             EXEC sp_xml_preparedocument @h OUTPUT, @x;
             INSERT INTO cust SELECT * FROM OPENXML(@h, '/ROOT/Customer', 1) WITH cust;
             EXEC sp_xml_removedocument @h;",
            DOC
        ),
    );
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM cust"), "2");
}

#[test]
fn the_edge_table_when_there_is_no_with_clause() {
    let mut c = conn();
    let r = rows(
        &mut c,
        "DECLARE @h INT;
         EXEC sp_xml_preparedocument @h OUTPUT, N'<a x=\"1\"><b>hi</b></a>';
         SELECT nodetype, localname, text FROM OPENXML(@h, '/a') ORDER BY id;
         EXEC sp_xml_removedocument @h;",
    );
    assert_eq!(r.iter().map(|x| x.join("|")).collect::<Vec<_>>(), vec!["1|a|NULL", "2|x|NULL", "3|#text|1", "1|b|NULL", "3|#text|hi"]);
}

#[test]
fn handles_and_documents_are_checked() {
    let mut c = conn();
    assert_eq!(err_no(&mut c, "DECLARE @h INT; EXEC sp_xml_preparedocument @h OUTPUT, N'<a><b></a>'"), 6603);
    assert_eq!(err_no(&mut c, "SELECT * FROM OPENXML(99, '/a') WITH (x INT '@x')"), 6602);
    assert_eq!(err_no(&mut c, "EXEC sp_xml_removedocument 99"), 6602);
    exec(&mut c, "DECLARE @h INT; EXEC sp_xml_preparedocument @h OUTPUT, N'<a/>'; EXEC sp_xml_removedocument @h");
}
