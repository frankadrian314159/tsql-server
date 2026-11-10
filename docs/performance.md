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

## Findings from the TPC benchmarks

Running TPC-H, TPC-DS and the fuzzer ([benchmarks.md](benchmarks.md)) found performance problems that the micro-benchmark
did not (times at scale factor 0.01):

| Query | Before | After | Cause |
| --- | ---: | ---: | --- |
| TPC-H Q18 | did not finish in 10 minutes | 0.38 s | an uncorrelated `IN (SELECT ... HAVING ...)` was re-run for every order |
| TPC-H Q19 | 160 s | 0.11 s | the join condition was repeated inside each branch of an `OR`, so lineitem was cross-joined with part |
| TPC-H Q5 | 1.2 s | 0.12 s | customer was joined on nation alone (many-to-many) before orders was joined on its key |
| TPC-H Q11, Q15, Q16 | 0.67, 1.6, 0.26 s | 0.004, 0.03, 0.013 s | uncorrelated scalar and `NOT IN` subqueries re-run per row |
| TPC-DS Q11 | 4.1 s | 0.64 s | `WHERE x.year = 2001` on a CTE reference was applied after the self-join of the CTE |
| `SUM(..) OVER ()` over 35,000 rows | over a minute | milliseconds | the frame was re-aggregated for every row |

The hash join also reads its key columns by position when they are plain columns (it used to evaluate an expression that
looked each column up by name for every row) and moves the probe row into its last output row instead of copying it.
All TPC-H and TPC-DS answers are the same on the default executors and on the spilling ones.

## Concurrency: what stopped sessions scaling

Even `SELECT 1`, which touches no data, stopped scaling at four sessions: eight sessions ran slower than four. The
cause was per-statement bookkeeping that every session funnelled through global locks. Measured on a 12-thread machine,
in memory (statements per second):

| Workload | 1 thread | 8 threads before | 8 threads after |
| --- | ---: | ---: | ---: |
| `SELECT 1` | 278k -> 717k | 371k | 3.0M |
| point lookup by primary key | 47k -> 56k | 120k | 261k |

1. **The session table.** Each statement updated the session's row in one table (`sys.dm_exec_sessions`) twice, under a
   global lock, after reading the time zone-aware clock and allocating strings. Each session now owns its row (its own
   lock), the time is a plain `SystemTime` converted when the view is read, and the command text is written into the
   existing buffer.
2. **Query statistics.** Every batch hashed its text and took a lock on one of 16 shards, so the same popular statement
   always hit the same shard. Each session records into its own buffer; `dm_exec_query_stats` adds the buffers of the
   running sessions to the totals kept from sessions that have ended.
3. **The active-transaction table** (every statement begins and ends an implicit transaction) is in 16 shards by
   transaction id. The oldest-snapshot calculation reads the clock before it looks at the shards, so a snapshot that is
   just starting cannot be passed.
4. **Read-only statements ran the whole commit protocol.** A query's implicit transaction took the commit lock,
   allocated a commit timestamp, advanced the clock and walked the lock table. A transaction that changed nothing now
   leaves the active table and is done.
5. **The lock manager is one latch.** While nobody holds a lock or key range and nobody is waiting, "is anything in my
   way?" and "release everything" answer from an atomic flag without taking it. A transaction that ends still wakes
   sleepers (DDL waits for open transactions), checked with a fence so no wake-up is lost.
6. **The page store was one mutex.** It is a reader-writer lock now. A lookup, and a scan of a store without a page
   file, read the cached pages with the pool held shared; the replacement bit is atomic and the hit counter is kept in
   per-thread slots so readers on different cores do not write the same cache line. Anything that must load a page or
   change the tree takes the pool exclusively as before, and a lookup that misses the cache retries that way.

Making commits and lock releases this much faster exposed a latent race in locking reads. `REPEATABLE READ` (and
`UPDLOCK`) read a chunk of rows and then lock them, so a commit that landed in between left the reader holding a stale
value under a lock it had just been granted: in a read-modify-write test, increments were lost in about 5 of 80 loaded
runs. After taking the locks a read now checks whether any of those rows was committed after the statement began and, if
so, runs the statement again with a fresh view (the rule `UPDATE` already used for its own rows).

Writes to the same store still queue on the pool (updates scale to about 2x at 4 sessions, limited by that and by the
catalog's per-table latches), as does any scan of a store with a page file.

## Where the time goes now

A point lookup by primary key costs about 20 microseconds from `Connection::execute`: parsing 5-10 (statements with
different literals are parsed afresh; identical text hits the parse cache), planning and scan setup 10, execution
a few. A correlated subquery costs about 11 microseconds per outer row, mostly the same per-query planning. Both would
need a cache of plans keyed by statement shape. A durable single-row commit costs one disk flush (about 0.5 ms);
sessions that commit together share it (group commit). Read throughput scales with the number of cores; writers to one store still take its page pool exclusively.
