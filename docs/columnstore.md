# Columnstore indexes

A columnstore index keeps a table's rows a second time as compressed *column segments* in *row groups*, so that a query that reads a few
columns of many rows, or adds them up, reads and works on those columns alone, a vector at a time. The syntax is SQL Server's;
how it is done is this server's own, and this page says both.

The short version: `CREATE [NONCLUSTERED | CLUSTERED] COLUMNSTORE INDEX` builds a **derived, in-memory copy** of the table in columns. The
heap (the row storage, with its B-tree indexes, its locks, its log and its backups) stays the table and the source of truth. The
columnstore is read in its place only where that gives exactly the rows the heap would give, and a query that cannot use it reads the
heap as it always did. It changes no result.

## Using it

```sql
CREATE CLUSTERED COLUMNSTORE INDEX cci ON sales;                      -- all the columns
CREATE NONCLUSTERED COLUMNSTORE INDEX ncci ON sales (day, store, qty, amount);
CREATE TABLE facts (day DATE, store INT, qty INT, amount DECIMAL(12,2),
                    INDEX cci CLUSTERED COLUMNSTORE);                 -- in the table definition
CREATE TABLE facts2 (day DATE, store INT, qty INT, INDEX n NONCLUSTERED COLUMNSTORE (day, qty));
ALTER TABLE sales ADD INDEX cci CLUSTERED COLUMNSTORE;

ALTER INDEX cci ON sales REBUILD;        -- build it again from the heap
ALTER INDEX cci ON sales REORGANIZE;     -- compress the rows added since (and rebuild if rows were changed)
ALTER INDEX ALL ON sales REBUILD;
ALTER INDEX cci ON sales DISABLE;        -- kept, not read; REBUILD enables it again
DROP INDEX cci ON sales;
```

`WITH (...)` options (`MAXDOP`, `DROP_EXISTING`, `COMPRESSION_DELAY`, `DATA_COMPRESSION = COLUMNSTORE_ARCHIVE`, ...) and an `ORDER (...)`
clause are read and ignored. Rules, with the error numbers (SQL Server's where it has one for the case):

| Rule | Error |
| --- | --- |
| a table has one columnstore index | 35339 |
| a column of a clustered index may not be of `text`, `ntext`, `image`, `xml`, `sql_variant`, `geometry`, `geography`, `hierarchyid`, `timestamp` | 35343 |
| the same for the columns of a nonclustered one | 35343 |
| a nonclustered columnstore index needs a column list; a clustered one has none | 35306, 35305 |
| a columnstore index cannot be unique, filtered, or have `INCLUDE` columns | 35302 |
| an index name already used on the table; an unknown column; a column named twice | 1913, 1911, 1909 |
| dropping a column that a nonclustered columnstore index is of | 5074 (drop the index first) |
| `ALTER INDEX name ON t ...` for a name the table has no index of | 1911 |

`ALTER INDEX` on any other index is accepted and does nothing, as it always did: the other indexes are B-trees kept in order, with nothing
to defragment.

**Where this differs from SQL Server.** A *clustered* columnstore index does not replace the table's storage here: the table stays a heap
with its primary key as a B-tree, and the clustered index is the same derived copy as a nonclustered one over every column. The catalog
shows what SQL Server shows (`sys.indexes` has the clustered columnstore index as index 1, type 5, and the primary key as nonclustered; no
heap row). There are no partitions, no archive compression, no ordered or filtered columnstore index, and a columnstore index in a SQL
Server `.bak` is not read when it is imported (see [backup.md](backup.md)); create it again after the import.

## What is stored

Each *row group* holds up to 1,048,576 rows (as in SQL Server) of consecutive row ids. For each column of the index it has a *segment*:

| Segment | For | Kept |
| --- | --- | --- |
| integers | `bit`, `tinyint`, `smallint`, `int`, `bigint` | a vector of 64-bit values and the type |
| floats | `float`, `real` | a vector of 64-bit floats |
| decimals | `decimal`, `numeric`, `money`, `smallmoney` of one scale | a vector of the unscaled 128-bit values |
| dictionary | strings, binary, dates, times, `uniqueidentifier`, ... when the values repeat (fewer distinct values than half the rows) | the distinct values and, for each row, the position of its value |
| plain | the rest | the values |

A segment also has a bit per row for NULL, and, for types whose order does not depend on a collation (not strings), the smallest and the
largest value, which is what lets a scan skip a whole group.

The row groups are built from the heap, and only from rows that have exactly one version, committed: that version is what every reader
sees, as long as nothing writes to the row. The rest of the story is how a write is noticed.

* A write to a row inside the built range (`TableData::push_version`, a rollback's `pop_version`, log replay) puts the row id in a
  *dirty* set. A scan reads a dirty row from the heap, with the reader's own view of it.
* Rows that came after the build (row ids above the last one it saw) are the *delta store*: a scan reads them from the heap too.
* When the delta store has grown to a row group the next read compresses it into row groups of its own (what SQL Server's tuple mover
  does in the background; here it is done by the query that notices). When a fifth of the rows are dirty, the next read builds the index
  again. `REORGANIZE` and `REBUILD` do the same on request.
* The groups, the dirty set and the build are in the table's memory: nothing of them is logged or written to the page file. After a restart
  the definition is there (it is part of the catalog) and the row groups are built when a query that could use them first runs (for a
  table of a thousand rows or more), or by `ALTER INDEX ... REBUILD`.

A build reads the heap a few thousand slots at a time, without holding anything across them. Every write is noted while a build runs, so
a row written after the build read it stays marked; and `ALTER INDEX`, `DROP INDEX` and `DISABLE` discard what a build in progress makes.

## What reads it

A scan of the table reads the row groups instead of the heap when all of these hold:

* the isolation level holds no row locks: `READ UNCOMMITTED`, `READ COMMITTED` (with or without row versioning) or `SNAPSHOT`; not
  `REPEATABLE READ` or `SERIALIZABLE`, and not under a lock hint (`UPDLOCK`, `XLOCK`, `HOLDLOCK`, `READPAST`, ...);
* the index covers the columns the query reads (a clustered one always does; a nonclustered one when the `SELECT` names only its
  columns: a `*`, or any other column, reads the heap);
* the access path is a scan (a query that an index seek answers well still seeks: a point lookup on the primary key does not read row
  groups);
* a snapshot reader is not older than the newest row in the groups (an older one sees other versions of them, and reads the heap).

Then, per row group:

1. **Segment elimination.** A group whose bounds rule out a simple condition on a column (`col = 5`, `<`, `<=`, `>`, `>=`, `<>`,
   `BETWEEN`, `IN (...)`, `IS NULL`, `IS NOT NULL`; the other side a number, date or time literal) is skipped whole. The plan says how
   many.
2. **Vector conditions.** The same conditions are evaluated over the vectors (or once per distinct value of a dictionary, by the engine's
   own comparison), and only the rows that pass are made into rows. Conditions on strings are left to the row evaluation, which knows the
   column's collation; so is anything that is not a column against a literal.
3. **Only the columns the query reads** are made into the rows, the others are NULL, as in any pruned scan.

The query's whole `WHERE` is still evaluated on the rows that come out, so the vector conditions never have to be exact, only to keep
every row the `WHERE` would keep.

The plan of a query that read through the index says so:

```text
columnstore scan of sales: 3 of 12 row group(s) read (9 skipped by their bounds or conditions), 40 written row(s) and the rows after the build read from the heap
```

## Batch mode

A grouped query, or one of nothing but aggregates, over a table with a columnstore index can be worked out over the column vectors
without making rows at all:

```sql
SELECT store, COUNT(*), SUM(amount), MIN(day), MAX(day) FROM sales WHERE qty >= 2 AND day < '2025-01-01' GROUP BY store HAVING COUNT(*) > 10;
```

The conditions are the `WHERE`'s, all of them simple (above); the group columns are plain columns; the aggregates are `COUNT(*)`, `COUNT`,
`SUM`, `AVG`, `MIN`, `MAX` of a plain column (`SUM` and `AVG` of an exact number, not of a `float`: a float sum depends on the order of the
additions). The select list, `HAVING` and `ORDER BY` name no column that is not a group column or an aggregate's. Each row group is turned
into its groups with the state of every aggregate over its rows, several groups at a time on several threads; the states are merged in
the order of the row groups into the streaming executor's groups, which is how its parallel workers' parts are merged (see
[performance.md](performance.md)), so `HAVING`, `ORDER BY`, `TOP` and the select list are the ones the engine has. The plan has
`batch mode: the aggregates of N row group(s) computed over column vectors`.

What is worked out here is what a row-by-row fold gives, or it is not worked out here:

* a sum whose bound (the row count times the largest absolute value of the segment) could leave the type's range is left to the rows,
  which raise the overflow (error 8115) where a serial run would;
* `MIN` and `MAX` of strings are taken here only when no collation is in use (they compare by bytes); otherwise they are left to the rows;
* a group column that is a string under a collation, a segment of plain values, a condition the segment cannot decide: left to the rows;
* the rows written since the build, and the rows after it, are folded row by row as before, and merged with the rest.

A query that is not of this shape (an expression as a group key or an argument, `DISTINCT`, a join, a float sum, a window function)
still reads the row groups for its scan, with segment elimination and vector conditions, and is worked out as it always was.
`TSQL_NO_BATCH` turns batch mode off, for comparing.

A table with a columnstore index streams whatever its size (the streaming executor is otherwise for tables of 20,000 slots or more), so
that batch mode and the pruning of columns are available.

## How fast

TPC-H `lineitem` at scale factor 0.1 (600,046 rows, one row group; release build; `examples/tpc h --sf 0.1 --sql ...`, the same statements
before and after `CREATE CLUSTERED COLUMNSTORE INDEX`, which took 2.4 s):

| Query | Heap | Columnstore, batch mode |
| --- | --- | --- |
| `GROUP BY l_returnflag, l_linestatus`: `COUNT(*)`, `SUM` of two columns, `MIN` and `MAX` of the ship date | 0.37 s | 0.026 s |
| `COUNT(*)`, two sums, `WHERE l_discount BETWEEN .05 AND .07 AND l_quantity < 24` | 0.25 s | 0.014 s |
| `GROUP BY l_shipmode` with a ship-date range of a year | 0.33 s | 0.005 s |
| `SUM(l_quantity)` | 0.23 s | 0.002 s |

The heap figures are the engine's parallel streaming scan with the columns it needs copied; the columnstore ones touch the vectors of
the columns the query names and nothing else. With more than one row group the groups are worked out on several threads.

## Catalog views

* `sys.indexes`: the columnstore index, `type` 5 (`CLUSTERED COLUMNSTORE`) or 6 (`NONCLUSTERED COLUMNSTORE`); `is_disabled`.
* `sys.index_columns`: its columns.
* `sys.partitions`, `sys.dm_db_partition_stats`: `data_compression` 3 for a clustered columnstore index.
* `sys.column_store_row_groups` and `sys.dm_db_column_store_row_group_physical_stats`: a row for each compressed row group (state 3,
  `COMPRESSED`, with `total_rows`, `deleted_rows` = the rows of the group that have been written since, and `size_in_bytes`) and one for
  the delta store (state 1, `OPEN`) when there are rows after the build. There are none until the index has been built.
* `sys.column_store_segments`: a row for each column of each row group: `row_count`, `has_nulls`, `encoding_type` (1 numbers, 2 or 3 a
  dictionary of non-strings or of strings, 4 plain values), the bounds of whole numbers as `min_data_id` and `max_data_id`, and the size.

## Settings

| | |
| --- | --- |
| `TSQL_COLUMNSTORE_ROWGROUP` | rows in a full row group (default 1,048,576; the tests use 1,000) |
| `TSQL_COLUMNSTORE_MIN_ROWS` | slots a table needs before its index is built when first read (default 1,000; an index is always built by `CREATE` and `REBUILD`) |
| `TSQL_NO_BATCH` | no batch mode |

## In a cluster

The statements are sent to every shard and each shard builds and reads its own columnstore over its rows; batch mode runs in each shard's
engine, and the control node merges the shards' answers as for any aggregate.

## Not done

* The row groups are not persisted, paged or limited in memory: a table's columnstore is as big as its compressed columns, in memory.
* A single write marks its row dirty for as long as the group stands: the heap serves it until the next build, which a fifth of the rows
  written forces. There is no per-group rebuild.
* A read-committed scan that reaches a row at the moment another session is writing it, between the write and the mark, reads the
  committed version without waiting for the writer (what a row-versioning read does); the heap would have waited.
* Batch mode has no expressions (`SUM(a * b)`), no string conditions, no joins, no window functions.
* No bulk path writes row groups directly (rows enter the heap and are compressed by the next build or extension). A join reads the
  row groups of its tables as any scan does, but batch mode is for a query of one table.
