//! The hierarchyid type and its methods (see src/hierarchyid.rs).
mod common;
use common::*;

fn setup(c: &mut tsql_server::Connection) {
    script(
        c,
        "CREATE TABLE org (
             node hierarchyid NOT NULL PRIMARY KEY,
             level AS node.GetLevel(),
             name VARCHAR(20) NOT NULL);
         CREATE INDEX ix_org_level ON org (level, node);
         INSERT INTO org VALUES (hierarchyid::GetRoot(), 'ceo');
         INSERT INTO org VALUES (CAST('/1/' AS hierarchyid), 'cto');
         INSERT INTO org VALUES (hierarchyid::Parse('/2/'), 'cfo');
         INSERT INTO org VALUES (CAST('/1/1/' AS hierarchyid), 'dev lead');
         INSERT INTO org VALUES ('/1/2/', 'qa lead');
         INSERT INTO org VALUES ('/1/1/1/', 'dev');
         INSERT INTO org VALUES ('/3/', 'coo');",
    );
}

#[test]
fn nodes_are_stored_in_tree_order_and_print_as_paths() {
    let mut c = conn();
    setup(&mut c);
    // the order of the type is the order of the tree: a parent, then its children, then the next sibling
    assert_eq!(rows(&mut c, "SELECT name FROM org ORDER BY node").iter().map(|r| r[0].as_str()).collect::<Vec<_>>(), vec!["ceo", "cto", "dev lead", "dev", "qa lead", "cfo", "coo"]);
    assert_eq!(rows(&mut c, "SELECT node.ToString() FROM org ORDER BY node").iter().map(|r| r[0].as_str()).collect::<Vec<_>>(), vec!["/", "/1/", "/1/1/", "/1/1/1/", "/1/2/", "/2/", "/3/"]);
    // CAST to a string is the same text; the bytes are those of SQL Server
    assert_eq!(scalar(&mut c, "SELECT CAST(node AS VARCHAR(50)) FROM org WHERE name = 'dev'"), "/1/1/1/");
    assert_eq!(scalar(&mut c, "SELECT CONVERT(NVARCHAR(50), node) FROM org WHERE name = 'dev lead'"), "/1/1/");
    assert_eq!(scalar(&mut c, "SELECT CAST(node AS VARBINARY(10)) FROM org WHERE name = 'dev lead'"), "0x5AC0");
    assert_eq!(scalar(&mut c, "SELECT CAST(node AS VARBINARY(10)) FROM org WHERE name = 'cto'"), "0x58");
    assert_eq!(scalar(&mut c, "SELECT DATALENGTH(node) FROM org WHERE name = 'ceo'"), "0");
    // comparisons
    assert_eq!(scalar(&mut c, "SELECT name FROM org WHERE node = hierarchyid::Parse('/1/2/')"), "qa lead");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM org WHERE node > hierarchyid::Parse('/1/1/')"), "4");
    // the type name
    assert_eq!(scalar(&mut c, "SELECT TYPE_NAME(user_type_id) FROM sys.columns WHERE name = 'node' AND object_id = OBJECT_ID('org')"), "hierarchyid");
}

#[test]
fn level_ancestor_descendant_and_reparenting() {
    let mut c = conn();
    setup(&mut c);
    // the computed column and its index
    assert_eq!(rows(&mut c, "SELECT name, level FROM org ORDER BY level, node").iter().map(|r| format!("{}={}", r[0], r[1])).collect::<Vec<_>>(), vec!["ceo=0", "cto=1", "cfo=1", "coo=1", "dev lead=2", "qa lead=2", "dev=3"]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM org WHERE level = 1"), "3");
    assert_eq!(scalar(&mut c, "SELECT node.GetAncestor(1).ToString() FROM org WHERE name = 'dev'"), "/1/1/");
    assert_eq!(scalar(&mut c, "SELECT node.GetAncestor(3).ToString() FROM org WHERE name = 'dev'"), "/");
    assert_eq!(scalar(&mut c, "SELECT node.GetAncestor(4) FROM org WHERE name = 'dev'"), "NULL");
    // direct reports
    assert_eq!(rows(&mut c, "SELECT name FROM org WHERE node.GetAncestor(1) = hierarchyid::Parse('/1/') ORDER BY node").iter().map(|r| r[0].as_str()).collect::<Vec<_>>(), vec!["dev lead", "qa lead"]);
    // everyone below the cto, the cto included
    assert_eq!(rows(&mut c, "SELECT name FROM org WHERE node.IsDescendantOf(hierarchyid::Parse('/1/')) = 1 ORDER BY node").iter().map(|r| r[0].as_str()).collect::<Vec<_>>(), vec!["cto", "dev lead", "dev", "qa lead"]);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM org WHERE node.IsDescendantOf('/') = 1"), "7");
    // a new child after the last one, the usual pattern
    exec(
        &mut c,
        "DECLARE @parent hierarchyid = (SELECT node FROM org WHERE name = 'cto'), @last hierarchyid;
         SELECT @last = MAX(node) FROM org WHERE node.GetAncestor(1) = @parent;
         INSERT INTO org VALUES (@parent.GetDescendant(@last, NULL), 'sec');",
    );
    assert_eq!(scalar(&mut c, "SELECT node.ToString() FROM org WHERE name = 'sec'"), "/1/3/");
    // GetReparentedValue moves a subtree
    assert_eq!(scalar(&mut c, "SELECT node.GetReparentedValue(hierarchyid::Parse('/1/'), hierarchyid::Parse('/2/')).ToString() FROM org WHERE name = 'dev'"), "/2/1/1/");
    assert_eq!(scalar(&mut c, "SELECT node.GetReparentedValue(hierarchyid::Parse('/2/'), hierarchyid::Parse('/9/')) FROM org WHERE name = 'dev'"), "NULL");
    // NULL in, NULL out
    assert_eq!(scalar(&mut c, "SELECT CAST(NULL AS hierarchyid).GetLevel()"), "NULL");
}

#[test]
fn bad_values_and_what_is_not_supported_are_errors() {
    let mut c = conn();
    setup(&mut c);
    assert_eq!(err_no(&mut c, "SELECT hierarchyid::Parse('1/2')"), 6522);
    assert_eq!(err_no(&mut c, "SELECT CAST('/a/' AS hierarchyid)"), 6522);
    assert_eq!(err_no(&mut c, "INSERT INTO org VALUES ('/1/', 'twice')"), 2627, "the key is unique");
    assert_eq!(err_no(&mut c, "SELECT hierarchyid::Parse('/1.5/')"), 6522, "fractional labels");
    assert_eq!(err_no(&mut c, "SELECT hierarchyid::Parse('/-1/')"), 6522, "negative labels");
    assert_eq!(err_no(&mut c, "SELECT hierarchyid::Parse('/1/').GetDescendant(hierarchyid::Parse('/1/1/'), hierarchyid::Parse('/1/2/'))"), 6522, "no label between 1 and 2");
    assert_eq!(err_no(&mut c, "SELECT hierarchyid::Parse('/1/').GetDescendant(hierarchyid::Parse('/2/1/'), NULL)"), 6522, "not a child");
    assert_eq!(err_no(&mut c, "SELECT CAST(0xFF AS hierarchyid)"), 6522, "bytes that are not a node");
}
