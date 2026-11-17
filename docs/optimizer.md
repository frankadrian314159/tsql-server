# The query optimizer

The optimizer decides *how* a query is answered: which index to read, where to apply each condition, in which
order to join tables and by which method. It never changes the answer (a test runs 54 query shapes with it on
and off and compares the results), only how long it takes. There is nothing to configure: no statistics to
gather and no plan to invalidate; what it needs is worked out from the tables and indexes as they are when
the query runs. Row counts are kept in the trees; distinct-value counts are read from an index exactly when it is
small (up to 20,000 entries) and extrapolated from 48 sampled leaf pages when it is large, so planning never reads a
big index in full. The design follows the optimizer in the sibling `accessor` project.

## What it does

1. **Pushdown.** A `WHERE` conjunct that reads only one table is applied while that table is scanned, before any
   join. For the null-supplying side of an outer join this would change the answer, so it is not done there.
2. **Index selection.** For each table the conditions (`=`, `IN`, `<`, `<=`, `>`, `>=`, `BETWEEN`, with constant
   operands) are matched against the table's indexes: the index with the longest equality prefix wins, a full
   match on a unique index beats everything, a range on the first column is the fallback. A constant is only used
   when it converts to the column's type *without loss* (`cust_id = 5.5` is not turned into `= 5`), and string and
   numeric columns are never compared across types. The executor re-checks every condition, so an index only has to
   return a superset of the matching rows.
3. **Join order.** Tables joined with commas or `INNER` / `CROSS JOIN` form a cluster. Each member's row count is
   estimated (after its pushed-down conditions, using index distinct counts), the smallest is joined first, and
   then the cheapest table that is connected to the rows so far by a join condition. Join conditions from `ON` and
   from `WHERE` are both used, so `FROM a, b WHERE a.x = b.y` is a join, not a cross product. The original column
   order is restored afterwards, so `SELECT *` is unaffected.
4. **Join methods.** If the next table has an index on its join columns and few rows have been joined so far, its
   index is probed with the distinct join keys (index nested-loop). Otherwise an equality join is a hash join, with
   the other conditions as a residual filter.
5. **Ordered reads.** For a single table, `ORDER BY` over exactly the columns of an index reads the index in order
   instead of sorting; with `TOP n` / `OFFSET … FETCH` and all conditions applied during the scan it also stops
   after the rows needed. Descending order is read backwards when the index is unique.
6. **Locking benefit.** Because seeks touch only the matching rows, a reader of one row does not wait for a writer
   of another (see [concurrency.md](concurrency.md)).

Outer joins keep their written order; `OR` conditions, functions of columns and subqueries are not used to choose
indexes; correlated subqueries are rewritten as joins where they can be (see the rewrites below) and otherwise run per outer row.

## Seeing the plan

```sql
SET SHOWPLAN_TEXT ON;          -- statements are described, not executed
SELECT * FROM ord o JOIN cust c ON c.id = o.cust_id WHERE c.id = 4;
SET SHOWPLAN_TEXT OFF;
```

```
SELECT
  |--join order: c -> o (reordered from the query's order)
  |--scan c [4 rows]: index seek via PK__cust__000186A2 (1 key(s)) (1 condition(s) pushed down) -> est 1 rows
  |--scan o [4 rows]: index seek via ix_ord_cust (0 key(s)) (0 condition(s) pushed down) -> est 4 rows
  |--join o: index nested-loop via ix_ord_cust; 0 rows in, 0 out
```

An index range scan is only chosen when it covers at most 15 % of the index (tables of fewer than 2,000 rows are not measured):
the fraction is estimated from one descent of the index (the position of each bound among the children of each node), and a
bigger range is read with a full scan, which costs far less per row. The number of distinct keys of an index is remembered
per prefix length while its size is within 5 %. In a `SELECT`, a correlated subquery is reused for outer rows with the same
values in the columns it refers to, and a scan that comes again with the same access path and conditions is answered from
the rows of the first time (the plan says `reused from an earlier scan`); see [performance.md](performance.md).

From Rust: `conn.explain(sql)` returns the same lines, and `conn.last_plan()` returns the decisions (with actual
row counts) of the statement that last ran (at most the first 256 lines: a subquery that runs once per outer row would otherwise
add a line for every row). `SET OPTIMIZER OFF` switches the optimizer off for the session (every
table is scanned in the written order), which is how the equivalence tests get their baseline.

## Filtered and included-column indexes

```sql
CREATE INDEX ix_open ON ord (cust) INCLUDE (amount) WHERE status = 'open';
CREATE UNIQUE INDEX ux_email ON acct (email) WHERE email IS NOT NULL AND active = 1;
```

A filtered index contains only the rows that satisfy its `WHERE` clause: a conjunction of `column <op> constant`,
`column IS [NOT] NULL` and `column IN (constants)` (anything else is error 10735). A *unique* filtered index constrains only
the rows it contains, and a row enters or leaves it as its columns change. The optimizer uses a filtered index only when the
query's own conditions imply the filter (`amount > 600` implies `amount > 500`; `amount >= 500` does not), and foreign keys, joins
and ordered reads never rely on one. `INCLUDE` columns are recorded (`sys.index_columns.is_included_column`, scripts) but, with
no covering-index reads, they change nothing at run time. `sys.indexes` shows `has_filter` and `filter_definition`.

## XML indexes

```sql
CREATE PRIMARY XML INDEX pxml_body ON doc (body);
CREATE XML INDEX xp_body ON doc (body) USING XML INDEX pxml_body FOR PATH;   -- FOR VALUE | PATH | PROPERTY
```

An XML index here is **a catalog entry that changes no result and no plan**. SQL Server builds a shredded copy of the xml
column and answers `.value`, `.exist` and `.query` from it; this server evaluates the simple XPath it supports directly over
the stored text, and has no structure to speed up, so the index is recorded and nothing more. It exists so that scripts, backups
and `.bak` files of SQL Server that create XML indexes load, and so that the catalog and scripts show them. The rules of
SQL Server that decide whether a script is valid are kept: the table needs a primary key (error 6332), the column must be
`xml`, there is one primary XML index per column, a secondary one names the primary index it is built on (`USING XML INDEX`),
names are shared with the other indexes of the table (error 1913), dropping a primary XML index drops the secondary indexes built on
it, and the xml column and the primary key cannot be dropped while an XML index needs them. Apart from 6332, 1913 and 3703
the error numbers are this server's own (6324 for the XML index rules). `sys.indexes` shows them with `type` 3 and `type_desc`
`XML` (index ids from 256000), `sys.xml_indexes` gives the kind (`PRIMARY_XML`, `SECONDARY_XML` with `V`, `P` or `R`) and the
primary index of a secondary one, `sys.index_columns` the column, and the table scripts (`tools::script_table`) create them
after the other indexes. A real shredded index would be a change to the storage and the planner; it is not built.

## Rewrites found by running the TPC queries

* **Subqueries that do not use the outer row are evaluated once per statement.** Whether a subquery is correlated is
  found by running it once without the outer row: if that works, no outer column was needed and its rows are kept for
  the rest of the statement; if it fails because a column is unknown, it runs per row as before. The cache is dropped
  at the end of each statement and is off while an `IF` or `WHILE` condition is evaluated.
* **Correlated subqueries of the `WHERE` become joins** (`src/rewrite/decorrelate.rs`, shared with the cluster's control node, applied by
  `Engine::decorrelated`). A scalar aggregate compared with a column (`x > (SELECT AVG(y) FROM t WHERE t.k = o.k)`) becomes a join with a
  grouped derived table (a `COUNT` is left alone: it gives 0, not NULL, for no rows), and `EXISTS` / `NOT EXISTS` on one equality become `IN` /
  `NOT IN` over the subquery's keys, written so that a NULL key behaves as it did. The derived table is restricted to the keys of the outer
  rows that survive the outer query's conditions on that table. Only equalities between a column of the subquery and a qualified column of
  the outer query are recognised, only when every table is a known base table, and not for `SELECT *`; a rewritten query that does not run
  is run as it was written. `SET OPTIMIZER OFF` turns it off. The plan says `correlated subquery rewritten as a join`.
* **Correlated aggregate subqueries in the select list, and those with `COUNT`, become outer joins** (`decorrelate_all`, the
  single-node engine only). A scalar aggregate subquery anywhere in the select list or the `WHERE` (`(SELECT COUNT(*) FROM t WHERE
  t.k = o.k)`, inside `ISNULL`, a `CASE` or an `OR`) becomes the value of a grouped derived table `LEFT JOIN`ed to the item of the
  `FROM` its outer columns belong to; where no group matches, the value is what the aggregate gives over no rows (`COUNT` 0, the
  others NULL), so the answer is the subquery's. `EXISTS` on two or more equalities becomes a join with the distinct keys of the
  subquery, and `NOT EXISTS` an outer join with them that found no match. Left alone: a subquery whose key column leads an index of
  its table (run per outer row it is a seek, which beats aggregating the whole table when the outer rows are few), one in the select
  list of a grouped query, one that is not an aggregate (it may give more than one row, error 512), and one whose outer columns span
  several items of the `FROM`.
* **A scalar subquery that stands alone is a constant for the plan**: `d_month_seq = (SELECT MAX(..) ..)` is run once before the plan
  is made and its value put in the condition, which can then be pushed to the table, seek an index and count in the row estimate.
* **Statistics of the columns** (`src/engine/stats.rs`) give the row estimates of the conditions that no index answers (the optimizer used
  to guess that each keeps a third of the rows). A table of 2000 slots or more is sampled once (64 blocks of 64 slots spread over its row
  ids, `Server::stats`, kept until the table has changed in size by a fifth or more than 500 + a fifth of its rows have been written
  since the sample, so an `UPDATE` that leaves the row count alone makes them stale too): per column the share of NULLs, the distinct
  values, the most common values with their shares and an equi-depth histogram. `col = lit`, ranges, `BETWEEN`, `IN`, `IS NULL`,
  `LIKE 'prefix%'`, `AND`, `OR` and `NOT` have a selectivity read off them. When a table is read through an index, the conditions the
  index does not answer (on other columns) multiply its estimate by their selectivity, so `WHERE indexed = 1 AND other = 2` is estimated
  as fewer rows than `WHERE indexed = 1`. They order joins; they do not change an answer. Temporary tables, global temporary tables and
  table variables are sampled in the same way; their statistics are kept in the session, by the heap they were taken from (the server's
  are by page number, which the temporary store gives out again after a `DROP`), and go with the table.
* **`IN (subquery)` of integers is tested with a hash**, and the rows of a subquery that does not use the outer row are shared, not copied,
  for each outer row. The keys of hash joins, groups and `DISTINCT` (`convert::key_into`) are written without a temporary string per value.
* **Common conjuncts of an OR are factored out** when the statement is parsed: `(a AND b) OR (a AND c)` is
  `a AND (b OR c)`, so a join condition repeated in each branch (TPC-H Q19) is a join condition.
* **A condition on a CTE, derived table or view reference is applied to its rows before they are joined.**
* **Join order prefers a unique-key join.** Among the tables that connect to those already placed, one joined on all the
  columns of one of its unique keys (so each row matches at most one) goes before one joined on a non-unique column
  that can multiply rows, whatever their sizes.
* **Window aggregates are accumulated as the frame moves**, and peer groups are found once, so a running total or a
  whole-partition aggregate is linear in the partition.

