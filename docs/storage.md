# Storage and durability

## Layout of a data directory

```
data/
  data.db        the page file: table heaps and indexes, 8 KiB pages
  snapshot.bin   the latest checkpoint: catalog, security, and the page table of data.db
  wal.log        write-ahead log: one frame per committed transaction since the checkpoint
  temp.db        spill file for temporary tables (recreated at every start, never recovered)
  lock           held with an OS file lock; a second process cannot open the directory (error 5120)
```

`Server::open(dir)` (or `Server::open_with(dir, StorageOptions { pool_pages, .. })`) creates the directory
if needed, loads `snapshot.bin`, replays `wal.log` and returns a server whose commits are durable.
`Server::new()` is the in-memory variant used for tests and embedding; it runs the same storage engine with a
page store that has no file and never evicts. The shell takes `-D dir` and `--pool-pages N`.

## Pages, the buffer pool and the page file

All table data lives in **B+trees made of 8 KiB pages** (`src/storage/page.rs`):

* a table's **heap** is a B+tree from row id to the row's *slot* (its chain of versions, oldest first);
* every primary key, unique constraint and index is a B+tree from `(key columns, row id)` to nothing, ordered
  with SQL Server's comparison rules (case-insensitive strings, NULL first);
* leaves are doubly linked and the root page of a tree never moves. A delete that leaves a leaf under a quarter full
  merges it into a neighbour under the same parent when the two fit in three quarters of a page; an inner node left
  with a single child is replaced by that child, and a root left with one child takes over the child's contents, so
  a tree whose rows were all deleted is a single empty page again (see *Reclaiming space* below).

A version chain whose serialised form exceeds 1 KiB is stored in **overflow pages**, so rows of any size
(`nvarchar(max)` values of hundreds of kilobytes) work. Index keys are limited to 1700 bytes (error 1946, as in
SQL Server).

The **buffer pool** caches decoded pages (`pool_pages` of them, default 16,384 = 128 MiB; clock replacement).
When it is full the least recently used page that no operation is using is written back (if modified) and dropped.
The pool is made of one **cache per tree**, each behind its own lock: every page belongs to exactly one tree, so an
operation takes the lock of its own tree and nothing else, and work on different trees (different tables, or a table
and its indexes) goes on at the same time. The trees share the size of the pool (a tree that is over it evicts one of
its own pages, or one of another tree that is not busy) and the page table, which is behind a small mutex held for
one step at a time (choosing a block, looking one up); a page is read from and written to the file by position, outside
that mutex, so reads and writes of different pages overlap. Within one tree operations still take turns (a
checkpoint holds every tree, in a fixed order). The format of the files is the same as before.
A server therefore needs memory for the pool, the page table (8 bytes per page, about 0.1 % of the data size) and
the catalog, **not** for the data. A cold start reads nothing: pages are loaded when a query touches them.
`sys.dm_os_buffer_pool` and `Server::storage_stats()` report cache size, page reads/writes, hits and evictions
for the database and temporary stores.

What still has to fit in memory is the *working set of one statement*: the rows it scans and keeps, the rows it
returns, the rows a `JOIN`/`GROUP BY`/`ORDER BY` operator materialises, and the changes of one `INSERT/UPDATE/DELETE`.
Pushed-down conditions are applied while a table is read, so a selective query over a huge table only keeps the
matching rows, and index seeks and early-stopping ordered reads touch only the pages they need. The undo log of an
open transaction (a few dozen bytes per changed row) is also in memory.

## Reclaiming space

* **Merging.** Deletes merge sparse leaves as described above, so the pages of deleted rows are returned to the free
  list (`pages_live` falls) instead of staying half empty.
* **Compaction.** At each checkpoint, if at least a quarter of `data.db` is free blocks, live pages from the end of the
  file are copied into free blocks nearer its start (at most 4,096 pages, 32 MiB, per checkpoint). A moved page gets a
  new, unprotected block and its old block is released only after the checkpoint that no longer refers to it, so the
  shadow-paging rule (a checkpointed block is never overwritten) still holds and a crash at any point recovers from
  the previous checkpoint plus the log.
* **Truncation.** After a checkpoint is durable, free blocks at the end of the file are cut off (`set_len`), so a
  database that shrank gets a smaller file within a couple of checkpoints (`StoreStats::{merges, moved, truncated}`).
  Small files (under 256 blocks) are left alone.

## Working memory and spilling to disk

A single-table `SELECT` that sorts, groups, de-duplicates or limits, and any join of base tables (see below), runs in a
**streaming executor** (`src/engine/stream.rs`) once a table in it has at least `stream_min_slots` heap slots
(default 20,000): the table is read a chunk at a time and each qualifying row goes into a sink instead of into a vector
that is then copied several times.

| Clause | Sink | Memory |
| --- | --- | --- |
| `ORDER BY` | external merge sort (`src/engine/spill.rs`): sorted runs of at most `work_mem` bytes are written to temporary files and merged (fan-in 48, multi-pass) | `work_mem` |
| `TOP n` / `OFFSET .. FETCH` with `ORDER BY` | the best `offset + n` rows are kept (up to 200,000), everything else is discarded as it is read | O(n) |
| `TOP n` without `ORDER BY` | the scan stops after `n` rows | O(n) |
| `DISTINCT` | rows are sorted by identity (spilling), the first arrival of each is kept, then the requested order is restored | `work_mem` |
| `GROUP BY` (plain expressions) | rows are sorted by group key (spilling); each group is aggregated, filtered by `HAVING` and projected as soon as it is complete, then its rows are dropped; groups keep first-appearance order unless `ORDER BY` says otherwise | `work_mem` + the largest group |

### Joins: streaming hash join with a grace fallback

A `FROM` clause that is a left-deep chain of base tables joined with `INNER`, `CROSS` (as a comma list with `WHERE`
equalities) or `LEFT` joins, with at least one equality between each new table and the tables before it, is joined by
`src/engine/hashjoin.rs`. The biggest table (for inner joins; the first one when there is a `LEFT JOIN`) is *probed*: it
is read a chunk at a time and each row is pushed through the join stages into the same sinks as above. Every other table
is a *build* side with a share of `work_mem`:

* if its rows fit, they are hashed in memory by the join key (and the output order is the probe order, as in the general
  executor);
* if they do not, the stage becomes a **grace hash join**: the rows already hashed and the rest of the build side are
  written to 32 partition files by the hash of the join key, probe rows are partitioned the same way, and when the probe
  input is exhausted each partition pair is joined on its own. A partition whose build side still exceeds the budget
  (skewed keys) is joined in several passes, reading the build partition in chunks that fit and the probe partition once
  per chunk, so memory is bounded however skewed the data is. Left joins keep a flag per probe row across the passes.

Rows with a NULL key never match (they appear only null-extended in a left join). The plan says how each stage ran:
`streaming hash join [n probe rows read]: scan f: ... -> scan d [rows]: ...; grace hash, 32 partition(s) spilled`.
A big table joined to a handful of rows through one of its indexes is left to the general executor, whose index
nested-loop join is the better plan there. `INNER`, `LEFT`, `RIGHT` and `FULL` joins stream (a right or full join keeps
a matched flag per build row, and emits the unmatched ones, with NULLs for the probe columns, when the probe side is
exhausted; build rows with a NULL key are kept in a file for the same purpose). `APPLY`, views and joins without an
equality use the general executor.

### Spilling rowsets

A result that feeds another operator (an operand of `UNION`/`INTERSECT`/`EXCEPT`, a derived table, a CTE body) is
collected in a `RowsRef` (`spill.rs`): rows stay in memory until they exceed the budget, then move to a temporary file.
Code written for in-memory rows keeps working, because dereferencing a spilled rowset reads it back once; code that can
handle one row at a time (the streaming executor, the set operations) reads the file sequentially instead.

* **Set operations** run on external sorts once an input has spilled or the inputs would not fit the budget (plan:
  `Union by external sort -> n rows`): `UNION` sorts by row identity and keeps each identity's first arrival, then puts
  the survivors back into arrival order; `INTERSECT`/`EXCEPT` sort both inputs by identity and merge them. `UNION ALL`
  concatenates the two files.
* **Derived tables and CTEs** over big tables stream into such a buffer, and a `SELECT` that sorts, groups,
  de-duplicates, limits or evaluates window functions over a derived table or CTE reads its source from the buffer
  (plan: `streaming rowset source`).
* **Window functions** in the select list run in the streaming executor when they all share one `OVER` clause and the
  query has no `GROUP BY`: rows are sorted externally by partition key and the window's order, and one partition at a
  time is evaluated (plan: `window by sort (n spilled run(s))`). A single partition is held in memory, so
  `OVER (ORDER BY x)` without `PARTITION BY` is one partition holding the whole input.

### Streamed results

`Connection::execute_streaming(sql, sink)` reports output as it is produced (`Streamed::Event` for messages, errors and
row counts, and `Header`, `Row`..., `End` for the rows of a plain top-level `SELECT` that the streaming executor ran),
so the result set is never held in memory: rows leave the final sort or merge one at a time. `tsql-serve` uses it for
every batch and RPC, writing rows into TDS packets and sending them as they fill. The column types of expressions are
not declared, so they are guessed from the first 64 rows (held back until then); the TDS layer sends such string
columns as `nvarchar(max)` so later, wider or non-code-page values are not lost. If the consumer goes away the
statement stops (error 64) and its spill files are removed. `SELECT ... INTO`, `TOP ... PERCENT`/`WITH TIES`, results
captured by `INSERT ... EXEC`, set operations and queries whose rows are read from a CTE deliver the whole result set
(`Event::Rows`).

Results are identical to the general executor's (the tests run both and compare, and `TSQL_TEST_SPILL=1 cargo test`
runs the whole suite with the streaming executor forced on and a 2 KiB budget). Ties keep the order rows were read in,
like a stable in-memory sort. Run files are named `tsql-spill-<pid>-<n>.run`, live in the system temporary directory
(`TSQL_SPILL_DIR` or `Server::set_work_mem` choose another) and are deleted when the statement ends, including when it
fails. `Server::set_work_mem(bytes, stream_min_slots, dir)` changes the budget; the plan (`last_plan()`,
`SET SHOWPLAN_TEXT`-style output) says `streaming scan ... external sort (n spilled run(s))`.

Everything else still materialises: scalar aggregates (one group holding every row), `ROLLUP`/`CUBE`, window
functions with different `OVER` clauses or inside `ORDER BY`, views, `APPLY`, recursive CTEs, a derived table that is
itself joined to something, and results that are captured rather than delivered (a spilled set-operation or
derived-table result is loaded back when one of those reads it). So the bound for the streamed shapes is `work_mem` plus the largest group, not a
constant for every query. A query whose ordered index read already provides the order (and can stop early) keeps using
that instead of a sort, unless its whole result is being streamed to a client.

## Crash safety without a write-ahead rule: shadow paging

Every page has a logical id, which pages and trees refer to, and a physical block in `data.db`; the mapping (the
page table) is in memory and is part of every checkpoint. **A block that belongs to the last checkpoint is never
overwritten**: when such a page is written again (because it was evicted or the checkpoint flushes it) it goes to
a fresh block, and the old block becomes free only after the next checkpoint is durable. So `data.db` always holds
a complete, consistent database as of the last checkpoint, plus whatever was written since, which recovery ignores.

That is what lets the buffer pool evict *any* page at *any* time, including pages holding the uncommitted changes of
a running transaction, without logging undo information: after a crash the server starts from the checkpoint and
replays the redo log, so nothing uncommitted can reappear. A transaction may be larger than the cache.

## Rows, versions and indexes

A slot holds a chain of versions, oldest first. A version is stamped either with the id of the transaction that
wrote it (still uncommitted) or with its commit timestamp. Readers pick the version their isolation level calls for
(see [concurrency.md](concurrency.md)); a deleted row is a version with no row. Old versions are garbage-collected
when their transaction commits and no running snapshot can see them. Row ids are handed out in increasing order and
are not reused.

Table handles are cheap to clone and share their pages: a clone is another name for the same data. Pages are freed
when the last handle to a tree is dropped, which is how `DROP TABLE` (and its rollback) works: the undo log keeps
the dropped table alive until the transaction ends.

## The undo log

A transaction records what it did: row versions it pushed and catalog objects it replaced. Rollback (of a statement,
to a savepoint, or of the whole transaction) pops that log. DDL is part of it, so `CREATE TABLE`, `ALTER TABLE`,
`DROP …` inside a transaction roll back like DML.

## The write-ahead log

Commit is redo-only: nothing reaches the log until a transaction commits.

1. The committing transaction's changes are collected: the final image of every row it touched, identity counters,
   and the final state of every catalog object it created, altered or dropped. A table that was rewritten (a column
   was added or altered, `SELECT INTO`) is logged as a reset followed by all of its rows; one whose heap is unchanged
   (`CREATE INDEX`, a constraint) is logged as its new definition only and its indexes are rebuilt at recovery.
2. They are written as one frame `[length][CRC-32][postcard payload]` and `fsync`ed. This is the commit point; if it
   fails the transaction is rolled back and the error returned. If the page store has failed (an I/O error or a
   checksum failure on a page), commits are refused.
3. Only then are versions stamped committed and locks released.

Sequence values and security settings are *not* transactional (as in SQL Server) and are appended immediately
without waiting for the disk.

### Checkpoints

A checkpoint (1) writes every modified page to `data.db` and syncs it, (2) writes `snapshot.tmp` containing the
catalog, the security data and the page table, syncs it and atomically renames it over `snapshot.bin`, (3) releases
the blocks superseded since the last checkpoint, and (4) truncates the log. It is taken

* by the `CHECKPOINT` statement or `Server::checkpoint()`,
* automatically when the log exceeds 16 MiB,
* when the last handle to a server is dropped (clean shutdown).

A checkpoint only runs while no transaction is open, so it never captures uncommitted changes. A crash at any
point leaves either the old checkpoint (and the log) or the new one; the pages written for a checkpoint that never
completed are unreferenced garbage. Every page carries a checksum; a damaged one fails the statement that reads it
(error 823/824) and stops the store from accepting commits.

### Recovery

On open the server loads the checkpoint and its page table, works out which blocks are free (those no page refers
to), then replays every log frame whose sequence number is newer than the checkpoint. Replay stops at the first frame
that is short or whose checksum fails, which is a torn write from a crash; the log is truncated there so new commits
start at a clean boundary. A transaction that had not committed leaves nothing in the log, so recovery needs no undo
pass.

## What is and is not on disk

Durable: tables and rows, constraints, indexes, views, procedures, functions, triggers, sequences (including their
position), user types, schemas, databases, logins and their password hashes, users, roles and permissions, database
options.

Not durable: temporary tables, table variables, cursors, session state. Temporary tables use the same page engine
with their own, smaller pool (`temp_pool_pages`) and spill to `temp.db`, so they can also exceed memory.

## Limits

* Joins, window functions, scalar aggregates, set operations and result sets are not spilled (see above); sorts, `DISTINCT`, `TOP n` and `GROUP BY` over a single table are.
* The page table (about 8 bytes per page) and the catalog are memory resident and are rewritten at each checkpoint.
* Only leaves are merged: inner nodes are not rebalanced, so a tree can stay slightly taller than a freshly built one
  after heavy deletes, and there is no `ALTER INDEX … REBUILD`. The temporary-table file (`temp.db`) is not shrunk.
* A checkpoint needs a moment with no open transaction; a transaction left open indefinitely keeps the log growing.
* `ALTER TABLE` rewrites and `SELECT INTO` hold the table's rows in memory and log them as one commit record.
* Stored sizes follow this engine's own row format, not SQL Server's.
