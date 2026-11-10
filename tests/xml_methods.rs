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
