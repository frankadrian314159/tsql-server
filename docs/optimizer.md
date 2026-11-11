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
indexes; subqueries run per outer row.

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

From Rust: `conn.explain(sql)` returns the same lines, and `conn.last_plan()` returns the decisions (with actual
row counts) of the statement that last ran. `SET OPTIMIZER OFF` switches the optimizer off for the session (every
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
* **Common conjuncts of an OR are factored out** when the statement is parsed: `(a AND b) OR (a AND c)` is
  `a AND (b OR c)`, so a join condition repeated in each branch (TPC-H Q19) is a join condition.
* **A condition on a CTE, derived table or view reference is applied to its rows before they are joined.**
* **Join order prefers a unique-key join.** Among the tables that connect to those already placed, one joined on all the
  columns of one of its unique keys (so each row matches at most one) goes before one joined on a non-unique column
  that can multiply rows, whatever their sizes.
* **Window aggregates are accumulated as the frame moves**, and peer groups are found once, so a running total or a
  whole-partition aggregate is linear in the partition.

