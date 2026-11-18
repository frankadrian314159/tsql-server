//! Graph tables: `AS NODE` and `AS EDGE`, the ids, edge constraints, `MATCH`, and `SHORTEST_PATH`.
mod common;
use common::*;
use tsql_server::Connection;

fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
}

fn sorted(c: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let mut r = rows(c, sql);
    r.sort();
    r
}

/// People and who knows whom, and where they live:
///
/// ```text
/// Alice -> Bob -> Carol -> Dave        Alice lives in Oslo, Bob in Rome, Dave in Oslo
/// Alice -> Erin -> Dave
/// Frank (alone)
/// ```
fn social() -> Connection {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE Person (ID INT PRIMARY KEY, name VARCHAR(20) NOT NULL) AS NODE
CREATE TABLE City (name VARCHAR(20)) AS NODE
CREATE TABLE Knows (since INT) AS EDGE
CREATE TABLE LivesIn AS EDGE
GO
INSERT Person VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol'), (4, 'Dave'), (5, 'Erin'), (6, 'Frank')
INSERT City VALUES ('Oslo'), ('Rome')
GO
INSERT Knows ($from_id, $to_id, since) SELECT a.$node_id, b.$node_id, x.s FROM (VALUES (1, 2, 2010), (2, 3, 2012), (3, 4, 2015), (1, 5, 2011), (5, 4, 2013)) x (f, t, s) JOIN Person a ON a.ID = x.f JOIN Person b ON b.ID = x.t
INSERT LivesIn ($from_id, $to_id) SELECT p.$node_id, c.$node_id FROM Person p JOIN City c ON (p.name IN ('Alice', 'Dave') AND c.name = 'Oslo') OR (p.name = 'Bob' AND c.name = 'Rome')
GO",
    );
    c
}

#[test]
fn node_and_edge_tables_have_their_ids() {
    let mut c = social();
    // a node's id is JSON text naming its table and its number in it; the numbers are the table's own
    assert_eq!(rows(&mut c, "SELECT name, $node_id FROM Person ORDER BY ID").first().unwrap(), &["Alice", "{\"type\":\"node\",\"schema\":\"dbo\",\"table\":\"Person\",\"id\":0}"]);
    assert_eq!(scalar(&mut c, "SELECT $node_id FROM City WHERE name = 'Rome'"), "{\"type\":\"node\",\"schema\":\"dbo\",\"table\":\"City\",\"id\":1}");
    assert_eq!(scalar(&mut c, "SELECT COUNT(DISTINCT $node_id) FROM Person"), "6");
    // an edge has an id of its own, and the ids of the nodes it joins
    let e = rows(&mut c, "SELECT $edge_id, $from_id, $to_id, since FROM Knows WHERE since = 2010");
    assert_eq!(e[0][0], "{\"type\":\"edge\",\"schema\":\"dbo\",\"table\":\"Knows\",\"id\":0}");
    assert_eq!(e[0][1], scalar(&mut c, "SELECT $node_id FROM Person WHERE name = 'Alice'"));
    assert_eq!(e[0][2], scalar(&mut c, "SELECT $node_id FROM Person WHERE name = 'Bob'"));
    // the id functions
    assert_eq!(scalar(&mut c, "SELECT OBJECT_ID_FROM_NODE_ID($node_id) FROM Person WHERE ID = 3"), scalar(&mut c, "SELECT OBJECT_ID('Person')"));
    assert_eq!(scalar(&mut c, "SELECT GRAPH_ID_FROM_NODE_ID($node_id) FROM Person WHERE ID = 3"), "2");
    assert_eq!(scalar(&mut c, "SELECT NODE_ID_FROM_PARTS(OBJECT_ID('Person'), 2)"), scalar(&mut c, "SELECT $node_id FROM Person WHERE ID = 3"));
    assert_eq!(scalar(&mut c, "SELECT GRAPH_ID_FROM_EDGE_ID($edge_id) FROM Knows WHERE since = 2015"), "2");
    assert_eq!(scalar(&mut c, "SELECT OBJECT_ID_FROM_EDGE_ID($edge_id) FROM Knows WHERE since = 2015"), scalar(&mut c, "SELECT OBJECT_ID('Knows')"));
    assert_eq!(scalar(&mut c, "SELECT EDGE_ID_FROM_PARTS(OBJECT_ID('Knows'), 2)"), scalar(&mut c, "SELECT $edge_id FROM Knows WHERE since = 2015"));
    assert_eq!(scalar(&mut c, "SELECT ISNULL(NODE_ID_FROM_PARTS(OBJECT_ID('Knows'), 2), 'none')"), "none");
    // the pseudo columns cannot be written; the edge's ends are required
    assert_eq!(err_no(&mut c, "UPDATE Person SET $node_id = 'x'"), 271);
    assert_eq!(err_no(&mut c, "INSERT Person ($node_id, ID, name) VALUES ('x', 9, 'z')"), 271);
    assert_eq!(err_no(&mut c, "INSERT Knows (since) VALUES (1)"), 515);
    assert_eq!(err_no(&mut c, "INSERT Knows ($from_id, $to_id) VALUES ('nope', 'nope')"), 13919);
    assert_eq!(err_no(&mut c, "INSERT Knows ($from_id, $to_id) SELECT $edge_id, $edge_id FROM Knows"), 13919);
    // SELECT * shows the node id and not the hidden counter; the user's own IDENTITY is not disturbed by it
    assert_eq!(rows(&mut c, "SELECT * FROM City WHERE name = 'Oslo'")[0][1], "Oslo");
    exec(&mut c, "CREATE TABLE Item (n INT IDENTITY(10, 5), label VARCHAR(5)) AS NODE");
    exec(&mut c, "INSERT Item (label) VALUES ('a'), ('b'), ('c')");
    assert_eq!(rows(&mut c, "SELECT n, label, GRAPH_ID_FROM_NODE_ID($node_id) FROM Item ORDER BY n"), table(&[&["10", "a", "0"], &["15", "b", "1"], &["20", "c", "2"]]));
}

#[test]
fn match_is_a_pattern_over_edges() {
    let mut c = social();
    // one step, both directions
    assert_eq!(sorted(&mut c, "SELECT a.name, b.name FROM Person a, Knows k, Person b WHERE MATCH(a-(k)->b)"), table(&[&["Alice", "Bob"], &["Alice", "Erin"], &["Bob", "Carol"], &["Carol", "Dave"], &["Erin", "Dave"]]));
    assert_eq!(sorted(&mut c, "SELECT a.name, b.name FROM Person a, Knows k, Person b WHERE MATCH(a<-(k)-b)"), table(&[&["Bob", "Alice"], &["Carol", "Bob"], &["Dave", "Carol"], &["Dave", "Erin"], &["Erin", "Alice"]]));
    // a chain: friends of friends
    assert_eq!(sorted(&mut c, "SELECT a.name, c.name FROM Person a, Knows k1, Person b, Knows k2, Person c WHERE MATCH(a-(k1)->b-(k2)->c)"), table(&[&["Alice", "Carol"], &["Alice", "Dave"], &["Bob", "Dave"]]));
    // a pattern with another condition, an edge attribute, and a pattern across two kinds of edge
    assert_eq!(sorted(&mut c, "SELECT b.name FROM Person a, Knows k, Person b WHERE MATCH(a-(k)->b) AND a.name = 'Alice' AND k.since > 2010"), table(&[&["Erin"]]));
    assert_eq!(sorted(&mut c, "SELECT a.name, c.name FROM Person a, Knows k, Person b, LivesIn l, City c WHERE MATCH(a-(k)->b-(l)->c)"), table(&[&["Alice", "Rome"], &["Carol", "Oslo"], &["Erin", "Oslo"]]));
    // several patterns, ANDed: people who know someone who lives in the city they live in
    assert_eq!(sorted(&mut c, "SELECT DISTINCT a.name FROM Person a, Knows k, Person b, LivesIn l1, LivesIn l2, City c WHERE MATCH(a-(k)->b AND a-(l1)->c AND b-(l2)->c)"), table(&[]));
    assert_eq!(sorted(&mut c, "SELECT DISTINCT c.name FROM Person a, Knows k, Person b, LivesIn l1, LivesIn l2, City c WHERE MATCH(a-(k)->b AND a-(l1)->c) AND b.name = 'Bob'"), table(&[&["Oslo"]]));
    // aggregates over a pattern
    assert_eq!(sorted(&mut c, "SELECT a.name, COUNT(*) FROM Person a, Knows k, Person b WHERE MATCH(a-(k)->b) GROUP BY a.name"), table(&[&["Alice", "2"], &["Bob", "1"], &["Carol", "1"], &["Erin", "1"]]));
    // people nobody knows and who know nobody
    assert_eq!(sorted(&mut c, "SELECT name FROM Person p WHERE NOT EXISTS (SELECT 1 FROM Knows k WHERE k.$to_id = p.$node_id OR k.$from_id = p.$node_id)"), table(&[&["Frank"]]));
}

#[test]
fn edge_constraints_say_what_an_edge_may_join() {
    let mut c = social();
    exec(&mut c, "CREATE TABLE Likes (rating INT, CONSTRAINT EC_Likes CONNECTION (Person TO City, Person TO Person)) AS EDGE");
    exec(&mut c, "INSERT Likes ($from_id, $to_id, rating) SELECT p.$node_id, c.$node_id, 5 FROM Person p, City c WHERE p.name = 'Alice' AND c.name = 'Rome'");
    exec(&mut c, "INSERT Likes ($from_id, $to_id, rating) SELECT p.$node_id, q.$node_id, 3 FROM Person p, Person q WHERE p.name = 'Alice' AND q.name = 'Bob'");
    // City to Person is not among the pairs
    assert_eq!(err_no(&mut c, "INSERT Likes ($from_id, $to_id) SELECT c.$node_id, p.$node_id FROM Person p, City c WHERE p.name = 'Alice' AND c.name = 'Rome'"), 547);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM Likes"), "2");
    // the catalog
    assert_eq!(rows(&mut c, "SELECT name, is_node, is_edge FROM sys.tables WHERE is_node = 1 OR is_edge = 1 ORDER BY name"), table(&[&["City", "1", "0"], &["Knows", "0", "1"], &["Likes", "0", "1"], &["LivesIn", "0", "1"], &["Person", "1", "0"]]));
    assert_eq!(rows(&mut c, "SELECT name, graph_type_desc FROM sys.columns WHERE object_id = OBJECT_ID('Knows') AND graph_type IS NOT NULL ORDER BY column_id"), table(&[&["$graph_id", "GRAPH_ID"], &["$edge_id", "GRAPH_ID_COMPUTED"], &["$from_id", "GRAPH_FROM_ID_COMPUTED"], &["$to_id", "GRAPH_TO_ID_COMPUTED"]]));
    assert_eq!(rows(&mut c, "SELECT name, delete_referential_action_desc FROM sys.edge_constraints"), table(&[&["EC_Likes", "NO_ACTION"]]));
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.edge_constraint_clauses"), "2");
    // an edge constraint goes only on an edge table; a node table has no use for one
    assert_eq!(err_no(&mut c, "ALTER TABLE Person ADD CONSTRAINT EC_x CONNECTION (Person TO Person)"), 13923);
    assert_eq!(err_no(&mut c, "CREATE TABLE E2 (x INT, CONSTRAINT EC_2 CONNECTION (Person TO Nowhere)) AS EDGE"), 13925);
    // a constraint added later, and dropped
    exec(&mut c, "ALTER TABLE Knows ADD CONSTRAINT EC_Knows CONNECTION (Person TO Person)");
    assert_eq!(err_no(&mut c, "INSERT Knows ($from_id, $to_id) SELECT p.$node_id, c.$node_id FROM Person p, City c WHERE p.name = 'Alice' AND c.name = 'Rome'"), 547);
    exec(&mut c, "ALTER TABLE Knows DROP CONSTRAINT EC_Knows");
    exec(&mut c, "INSERT Knows ($from_id, $to_id) SELECT p.$node_id, c.$node_id FROM Person p, City c WHERE p.name = 'Alice' AND c.name = 'Rome'");
    // the table's own columns cannot take the graph's names, and the graph's cannot be dropped
    assert_eq!(err_no(&mut c, "CREATE TABLE Bad ($x INT) AS NODE"), 13920);
    assert_eq!(err_no(&mut c, "ALTER TABLE Person DROP COLUMN $node_id"), 13922);
}

#[test]
fn shortest_path_finds_the_ways_to_every_reachable_node() {
    let mut c = social();
    // from Alice to everyone she can reach: Dave by Erin or by Bob and Carol (Erin's is shorter)
    let paths = sorted(
        &mut c,
        "SELECT p.name, LAST_VALUE(q.name) WITHIN GROUP (GRAPH PATH) AS last, STRING_AGG(q.name, '->') WITHIN GROUP (GRAPH PATH) AS via, COUNT(q.name) WITHIN GROUP (GRAPH PATH) AS hops
         FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k
         WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+)) AND p.name = 'Alice'",
    );
    assert_eq!(paths, table(&[&["Alice", "Bob", "Bob", "1"], &["Alice", "Carol", "Bob->Carol", "2"], &["Alice", "Dave", "Erin->Dave", "2"], &["Alice", "Erin", "Erin", "1"]]));
    // the other way, and a limit on the length
    let back = sorted(
        &mut c,
        "SELECT p.name, LAST_VALUE(q.name) WITHIN GROUP (GRAPH PATH), COUNT(q.name) WITHIN GROUP (GRAPH PATH)
         FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k
         WHERE MATCH(SHORTEST_PATH(p(<-(k)-q)+)) AND p.name = 'Dave'",
    );
    assert_eq!(back, table(&[&["Dave", "Alice", "2"], &["Dave", "Bob", "2"], &["Dave", "Carol", "1"], &["Dave", "Erin", "1"]]));
    let near = sorted(
        &mut c,
        "SELECT LAST_VALUE(q.name) WITHIN GROUP (GRAPH PATH) FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q){1,1})) AND p.name = 'Alice'",
    );
    assert_eq!(near, table(&[&["Bob"], &["Erin"]]));
    // an aggregate of an edge attribute along the path, MIN, SUM and AVG, and LAST_NODE
    let sums = sorted(
        &mut c,
        "SELECT LAST_VALUE(q.name) WITHIN GROUP (GRAPH PATH), SUM(k.since) WITHIN GROUP (GRAPH PATH), MIN(k.since) WITHIN GROUP (GRAPH PATH), MAX(k.since) WITHIN GROUP (GRAPH PATH)
         FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+)) AND p.name = 'Alice'",
    );
    assert_eq!(sums, table(&[&["Bob", "2010", "2010", "2010"], &["Carol", "4022", "2010", "2012"], &["Dave", "4024", "2011", "2013"], &["Erin", "2011", "2011", "2011"]]));
    let last = sorted(&mut c, "SELECT LAST_NODE(q) FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+)) AND p.name = 'Alice'");
    assert_eq!(last, sorted(&mut c, "SELECT $node_id FROM Person WHERE name IN ('Bob', 'Carol', 'Dave', 'Erin')"));
}

#[test]
fn shortest_path_in_a_larger_query() {
    let mut c = social();
    // from every person: start columns with the path's, ordered, filtered by a derived table, with TOP
    let q = "SELECT p.name AS who, LAST_VALUE(q.name) WITHIN GROUP (GRAPH PATH) AS other, COUNT(q.name) WITHIN GROUP (GRAPH PATH) AS hops
             FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+))";
    assert_eq!(scalar(&mut c, &format!("SELECT COUNT(*) FROM ({q}) x")), "8");
    assert_eq!(rows(&mut c, &format!("SELECT who, other FROM ({q}) x WHERE other = 'Dave' AND hops = 1 ORDER BY who")), table(&[&["Carol", "Dave"], &["Erin", "Dave"]]));
    assert_eq!(rows(&mut c, &format!("SELECT TOP 2 who, COUNT(*) FROM ({q}) x GROUP BY who ORDER BY COUNT(*) DESC, who")), table(&[&["Alice", "4"], &["Bob", "2"]]));
    // nobody to reach from Frank
    assert_eq!(rows(&mut c, "SELECT LAST_VALUE(q.name) WITHIN GROUP (GRAPH PATH) FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+)) AND p.name = 'Frank'"), Vec::<Vec<String>>::new());
    // what is not supported says so
    assert_eq!(err_no(&mut c, "SELECT p.name FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+)) AND q.name = 'Bob'"), 13926);
    assert_eq!(err_no(&mut c, "SELECT p.name FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q-(k)->q)+))"), 13904);
}

#[test]
fn deleting_a_node_follows_the_edge_constraints() {
    let mut c = conn();
    script(
        &mut c,
        "CREATE TABLE P (id INT PRIMARY KEY) AS NODE
CREATE TABLE Q (id INT PRIMARY KEY) AS NODE
CREATE TABLE R (id INT PRIMARY KEY) AS NODE
CREATE TABLE ToQ (CONSTRAINT EC_ToQ CONNECTION (P TO Q) ON DELETE CASCADE) AS EDGE
CREATE TABLE ToR (CONSTRAINT EC_ToR CONNECTION (P TO R)) AS EDGE
CREATE TABLE Loose AS EDGE
GO
INSERT P VALUES (1), (2)
INSERT Q VALUES (1)
INSERT R VALUES (1)
GO
INSERT ToQ ($from_id, $to_id) SELECT p.$node_id, q.$node_id FROM P p, Q q
INSERT ToR ($from_id, $to_id) SELECT p.$node_id, r.$node_id FROM P p, R r WHERE p.id = 1
INSERT Loose ($from_id, $to_id) SELECT p.$node_id, q.$node_id FROM P p, Q q
GO",
    );
    // ON DELETE CASCADE: the edges at the node go with it, at either end
    exec(&mut c, "DELETE Q");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ToQ"), "0");
    exec(&mut c, "INSERT Q VALUES (7)");
    exec(&mut c, "INSERT ToQ ($from_id, $to_id) SELECT p.$node_id, q.$node_id FROM P p, Q q");
    exec(&mut c, "DELETE P WHERE id = 2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ToQ"), "1");
    // NO ACTION: an edge at the node stops the delete, and a node with none is deleted
    assert_eq!(err_no(&mut c, "DELETE P WHERE id = 1"), 547);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM P"), "1");
    assert_eq!(err_no(&mut c, "DELETE R"), 547);
    exec(&mut c, "DELETE ToR");
    exec(&mut c, "DELETE R");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM R"), "0");
    exec(&mut c, "DELETE P");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM P"), "0");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ToQ"), "0");
    // an edge table with no constraint is not asked: its edges are left where they are
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM Loose"), "2");
    // a node table that a constraint names cannot be dropped, nor a table that is not an edge table given one
    assert_eq!(err_no(&mut c, "DROP TABLE Q"), 3726);
    exec(&mut c, "DROP TABLE ToQ");
    exec(&mut c, "DROP TABLE Q");
}

#[test]
fn graph_tables_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = tsql_server::Server::open(dir.path()).unwrap();
        let mut c = s.connect();
        script(
            &mut c,
            "CREATE TABLE N (id INT, label VARCHAR(5)) AS NODE
CREATE TABLE E (w INT, CONSTRAINT EC CONNECTION (N TO N) ON DELETE CASCADE) AS EDGE
GO
INSERT N VALUES (1, 'a'), (2, 'b'), (3, 'c')
INSERT E ($from_id, $to_id, w) SELECT x.$node_id, y.$node_id, x.id * 10 + y.id FROM N x, N y WHERE y.id = x.id + 1
GO",
        );
    }
    let s = tsql_server::Server::open(dir.path()).unwrap();
    let mut c = s.connect();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE is_node = 1 AND is_edge = 0"), "1");
    assert_eq!(rows(&mut c, "SELECT a.label, b.label, e.w FROM N a, E e, N b WHERE MATCH(a-(e)->b) ORDER BY e.w"), table(&[&["a", "b", "12"], &["b", "c", "23"]]));
    // the counters go on where they were, and the constraint is still there
    exec(&mut c, "INSERT N VALUES (4, 'd')");
    assert_eq!(scalar(&mut c, "SELECT GRAPH_ID_FROM_NODE_ID($node_id) FROM N WHERE id = 4"), "3");
    assert_eq!(err_no(&mut c, "INSERT E ($from_id, $to_id) SELECT $edge_id, $edge_id FROM E"), 13919);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM sys.edge_constraints"), "1");
    exec(&mut c, "DELETE N WHERE id = 2");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM E"), "0");
    exec(&mut c, "INSERT E ($from_id, $to_id, w) SELECT x.$node_id, y.$node_id, 1 FROM N x, N y WHERE x.id = 1 AND y.id = 4");
    // (a failed insert has used a number, as it does for an IDENTITY column)
    let id: i64 = scalar(&mut c, "SELECT GRAPH_ID_FROM_EDGE_ID($edge_id) FROM E").parse().unwrap();
    assert!(id >= 2, "{id}");
}
