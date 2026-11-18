# Graph tables

SQL Server's graph syntax works here: node tables and edge tables (`CREATE TABLE ... AS NODE | AS EDGE`), the `MATCH` predicate that
finds patterns of nodes and edges, `SHORTEST_PATH` with the aggregates over the steps of a path, and the functions on node and edge ids.
A graph table is an ordinary table with a few columns of its own, and a pattern is turned into joins, so everything else (indexes,
constraints, transactions, permissions, `OUTPUT`, backup) is what it is for any table.

```sql
CREATE TABLE Person (ID INT PRIMARY KEY, name VARCHAR(40)) AS NODE;
CREATE TABLE City   (name VARCHAR(40)) AS NODE;
CREATE TABLE Knows  (since INT) AS EDGE;
CREATE TABLE LivesIn (CONSTRAINT EC_LivesIn CONNECTION (Person TO City) ON DELETE CASCADE) AS EDGE;

INSERT Person VALUES (1, 'Alice'), (2, 'Bob');
INSERT Knows ($from_id, $to_id, since)
  SELECT a.$node_id, b.$node_id, 2010 FROM Person a, Person b WHERE a.name = 'Alice' AND b.name = 'Bob';

SELECT a.name, b.name FROM Person a, Knows k, Person b WHERE MATCH(a-(k)->b);          -- who knows whom
SELECT a.name, c.name FROM Person a, Knows k, Person b, LivesIn l, City c
 WHERE MATCH(a-(k)->b-(l)->c);                                                           -- where the people they know live
```

## Node and edge tables

`AS NODE` or `AS EDGE` follows the column list (an edge table may have no columns of its own: `CREATE TABLE LivesIn AS EDGE`). The table
has, besides its own columns:

| Column | In | What it is |
| --- | --- | --- |
| `$graph_id` (hidden) | both | a number from a counter of the table's own, starting at 0; not shown by `SELECT *`. A column of the table that is an `IDENTITY` is not disturbed by it. |
| `$node_id` | node | the id of the node, `{"type":"node","schema":"dbo","table":"Person","id":0}`, computed from `$graph_id` and kept with the row |
| `$edge_id` | edge | the same for the edge, `"type":"edge"` |
| `$from_id`, `$to_id` | edge | the `$node_id` of the nodes the edge goes from and to; `NOT NULL`, given by the `INSERT` |

`SELECT *` shows `$node_id` (or `$edge_id`, `$from_id`, `$to_id`) first, as SQL Server does. An `INSERT` into a node table takes the table's
own columns; into an edge table `$from_id` and `$to_id` first, then the table's own (or name them in a column list). The generated ids
cannot be written (error 271), `$from_id` and `$to_id` must be ids of nodes (13919), and a column of the table cannot be named with a
leading `$` (13920) or the graph's columns dropped (13922).

An id is text here: SQL Server keeps a pair of numbers and shows it as this JSON, and this server keeps the JSON, so ids compare as
strings. `NODE_ID_FROM_PARTS(object_id, graph_id)`, `OBJECT_ID_FROM_NODE_ID(id)`, `GRAPH_ID_FROM_NODE_ID(id)`, and `EDGE_ID_FROM_PARTS`,
`OBJECT_ID_FROM_EDGE_ID`, `GRAPH_ID_FROM_EDGE_ID` take an id apart and put one together (NULL for anything that is not one). A table
cannot be made a node or edge table after it is created (13921); a graph table cannot be a temporary table (13918). **A node table is
not renamed** with `sp_rename`: its ids name it.

### Edge constraints

`CONSTRAINT name CONNECTION (a TO b, c TO d, ...) [ON DELETE CASCADE | NO ACTION]` on an edge table (in `CREATE TABLE`, or
`ALTER TABLE ... ADD CONSTRAINT`, and `DROP CONSTRAINT`) lists the pairs of node tables its edges may join. An edge whose ends are of
another pair is refused (547). Without a constraint an edge may join any nodes.

A constraint also says what becomes of the edges at a node that is deleted: with `ON DELETE CASCADE` they are deleted too (at either
end); with the default, `NO ACTION`, an edge at the node stops the delete (547). An edge table with no constraint naming the node's
table is not asked, and its edges are left (dangling). A node table that a constraint names cannot be dropped (3726). A constraint
cannot be added to a table that is not an edge table, and cannot name the edge table itself, a table that is not a node table, or a
table that does not exist (13923 to 13925).

## `MATCH`

`MATCH(pattern [AND pattern]...)` is a predicate of the `WHERE` clause. A pattern is a node alias followed by steps, each an edge
alias and a node alias: `a-(e)->b` (the edge goes from `a` to `b`), `a<-(e)-b` (from `b` to `a`), chained: `a-(e1)->b-(e2)->c`. The aliases
are those of the `FROM` clause: node tables and edge tables written as any other table. The parser turns each step into
`e.$from_id = a.$node_id AND e.$to_id = b.$node_id`, so a pattern is a join like any other and is planned as one, with whatever indexes
the tables have, and a column of an edge (`k.since`) is a column like any other. `MATCH` cannot be negated or `OR`ed.

## `SHORTEST_PATH`

```sql
SELECT p.name,
       STRING_AGG(q.name, '->') WITHIN GROUP (GRAPH PATH)  AS via,
       LAST_VALUE(q.name)       WITHIN GROUP (GRAPH PATH)  AS last,
       COUNT(q.name)            WITHIN GROUP (GRAPH PATH)  AS hops
FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k
WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+)) AND p.name = 'Alice';
```

For each start node (here, each person named Alice) the shortest path to each node that can be reached from it by edges of `k`; one row
for each such path. The select list has columns of the start node, aggregates over the steps of the path (`STRING_AGG` in the order of
the steps, `LAST_VALUE`, `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, each `WITHIN GROUP (GRAPH PATH)`) and `LAST_NODE(q)` (the id of the path's
last node). The pattern's repeat is `+` (any length) or `{m,n}`; `<-(k)-` follows the edges the other way. `FOR PATH` after a table
marks it as a step of the path; it is read and the pattern says which aliases are steps.

How it is done: the graph is walked breadth first from each start node, which finds a shortest path to every node it reaches, and every path is
written down as rows, one for each step (the number of the path, of the step and the path's length, and the columns of the start node,
the edge and the node of the step). The query is rewritten to read those rows with `GROUP BY` the path, the path aggregates becoming
aggregates over the steps, and the rest of the statement (`ORDER BY`, `HAVING`, `TOP`, `DISTINCT`, being a derived table or a CTE) is
the engine's. Where there are several shortest paths to a node the one found first is given.

What is not supported (error 13926 or 13904 says so): more than one step in the repeated pattern (`(-(e1)->n1-(e2)->n2)+`),
conditions of the `WHERE` on the nodes and edges of the path (only the start node's), `GROUP BY` in the same `SELECT` (put the path query in a
derived table and group that), other tables in the `FROM`, and `SHORTEST_PATH` with `{m,n}` meaning "shortest with at least `m` steps":
the paths of at least `m` steps are taken from the shortest paths (a node reached in fewer is not given).

## Catalog

* `sys.tables`: `is_node`, `is_edge`.
* `sys.columns`: `graph_type` and `graph_type_desc` for the graph columns (`GRAPH_ID`, `GRAPH_ID_COMPUTED`, `GRAPH_FROM_ID_COMPUTED`,
  `GRAPH_TO_ID_COMPUTED`).
* `sys.edge_constraints` and `sys.edge_constraint_clauses`: the constraints and the pairs of tables they connect.

## Not done

* Graph tables are not in a cluster (the shards would each number a table's rows from 0; error 50100) and not in a SQL Server `.bak`
  that is imported (the tables come over as plain tables if their columns can be read).
* The ids are text, not SQL Server's binary pairs: a program that reads `$node_id` as `nvarchar` gets what SQL Server gives it; one that
  expects the internal columns by their real names (`graph_id_<hex>`, `$node_id_<hex>`) does not.
* `SHORTEST_PATH` is described above; `MATCH` over `LAST_NODE`, the arbitrary-length pattern with `+` and `{m,n}` of more than one step,
  and graph-aware plans (an adjacency index, a path search that does not read the whole edge table) are not there: the edge table and the
  node table of a `SHORTEST_PATH` are read whole.
