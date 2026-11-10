# Performance

## Measuring

`cargo run --release --example bench [rows] [--durable] [workload filter...]` runs a fixed set of workloads (bulk and
single-row inserts, point lookups, scans, aggregates, sorts, joins, subqueries, windows, set operations, updates,
deletes, and the same lookups and updates from 1-8 threads) and prints the seconds each took.

To see where a statement spends its time, build with the span profiler:
`cargo run --release --features prof --example bench`. `span!("name")` (`src/prof.rs`) times the rest of the enclosing
block; the report lists calls, inclusive time and exclusive time (without nested spans) per span. Without the `prof`
feature the spans compile to nothing. Spans are in the statement pipeline (`parse`, `exec_stmt`, `exec_select`,
`scan_table_leaf`, `apply_changes_latched`, `write_versions`, `commit_tx`, ...); add one where you need a finer view.

## What was found and fixed

Measured on 100,000 rows, in memory (seconds; the first column is the engine before this work):

| Workload | Before | After |
| --- | ---: | ---: |
| `WHERE EXISTS (SELECT 1 FROM d WHERE d.id = t.grp ...)` over 100k outer rows | 38.35 | 1.14 |
| `(SELECT label FROM d WHERE d.id = t.grp)` in the select list, 2000 rows | 0.37 | 0.02 |
| insert 100,000 rows (500 per statement) | 1.38 | 0.97 |
| `UPDATE` every row (non-key column) | 1.68 | 0.91 |
| `UPDATE` every row (indexed column) | 2.18 | 1.39 |
| `GROUP BY` + `ORDER BY` | 0.51 | 0.19 |
| `GROUP BY` 1000 keys, 3 aggregates | 0.37 | 0.18 |
| `DISTINCT` | 0.37 | 0.17 |
| join + `GROUP BY` | 0.64 | 0.30 |
| window `ROW_NUMBER() OVER (PARTITION BY ...)` | 0.64 | 0.31 |
| four threads doing point lookups (statements/s) | 0.9x of one thread | 3.4x |

1. **Correlated predicates were never indexed.** In `EXISTS (SELECT ... FROM d WHERE d.id = t.grp)` the comparison
   mentions a column of the outer row, which the pushdown planner treated as unknown, so `d` was scanned in full for
   every outer row. A comparison between a local column and an outer column is now pushed down with the outer value as
   a constant (`plan_pushdown_outer`), which lets the access chooser seek on `d.id`.
2. **The catalog was taken exclusively by every statement.** A batch started with the catalog in exclusive mode and
   only dropped to shared mode once `exec_stmt` had run, and every statement ended with `yield_now()`. Sessions
   therefore queued behind each other even for `SELECT 1`. The guard now starts shared, and only a session that held the
   catalog exclusively yields. Per-statement bookkeeping that took a global lock (the kill check, the statement
   statistics) is lock-free or sharded.
3. **B+tree writes did redundant work.** `Tree::put` serialised the whole row version chain just to see whether it
   would overflow, `LeafEntry::new` serialised it again, and every insert added up the sizes of all entries of the
   leaf. Sizes are now computed once (with a counting serialiser) and the leaf size is kept up to date. Stamping a
   commit, collecting old versions and pushing a new version used to copy the whole slot out, change it and put it back
   (a second tree descent); they now change it in place (`Tree::modify`). An update that leaves an index key alone no
   longer rewrites the index entry.
4. **Sort comparators allocated.** The streaming executor's grouping, `DISTINCT` and window sorts compared two
   `Value::to_string()` results per comparison. They compare the strings in place now.
5. **Rows were copied needlessly.** The general executor copied every row of a scan once more before filtering; it now
   takes over the scan's rows.

Things that were measured and left alone: link-time optimisation and a single codegen unit (+1.5 % for a 2.5-minute
link), preallocating the log file (no change in commit latency; a durable commit is one `FlushFileBuffers`, about
0.5 ms on the test machine), and the cost of cloning a table definition per statement (made cheap with copy-on-write
fields, but small next to the rest).

6. **`sp_executesql` parsed its text on every call.** Both the statement and the `@params` declaration bypassed the parse
   cache, which is the shape every driver sends. Both (and `EXEC (...)`) are cached now: a parameterized lookup through
   `sp_executesql` went from 29 to 23 microseconds.

## Why there is no execution-plan cache

An execution-plan cache was measured before it was built. For a point lookup by primary key (about 20 microseconds from
`Connection::execute`) the parts a cache could remove are small and spread out: lexing 1.0 and parsing 2.2 (already
cached for repeated text), resolving the table and checking permissions 0.9, choosing the access path 0.6, building the
column schema 0.4, plan text 0.7, and about 4 of set-up in `exec_select` and `eval_from_list`; the rest is the work itself
(the seek and fetch 3.4) and per-statement session bookkeeping (about 3.5). Caching every one of those would save about
30 % of a lookup and nothing for scans, joins and aggregates, whose time is execution. It would also need a catalog
epoch checked on every use, because permissions, definitions and statistics all change plans, and the access path depends
on parameter values. That is more risk than the gain justifies; the cheap, safe part (parsed text) is cached.

## Where the time goes now

A point lookup by primary key costs about 20 microseconds from `Connection::execute`: parsing 5-10 (statements with
different literals are parsed afresh; identical text hits the parse cache), planning and scan setup 10, execution
a few. A correlated subquery costs about 11 microseconds per outer row, mostly the same per-query planning. Both would
need a cache of plans keyed by statement shape. A durable single-row commit costs one disk flush (about 0.5 ms);
sessions that commit together share it (group commit). Throughput flattens at 4-8 sessions: the page store's buffer pool
is one mutex.
