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
* leaves are doubly linked, the root page of a tree never moves, and pages are never merged after deletes
  (a table that shrank a lot keeps its pages for reuse by later inserts; dropping it frees them all).

A version chain whose serialised form exceeds 1 KiB is stored in **overflow pages**, so rows of any size
(`nvarchar(max)` values of hundreds of kilobytes) work. Index keys are limited to 1700 bytes (error 1946, as in
SQL Server).

The **buffer pool** caches decoded pages (`pool_pages` of them, default 16,384 = 128 MiB; clock replacement).
When it is full the least recently used page that no operation is using is written back (if modified) and dropped.
A server therefore needs memory for the pool, the page table (8 bytes per page, about 0.1 % of the data size) and
the catalog, **not** for the data. A cold start reads nothing: pages are loaded when a query touches them.
`sys.dm_os_buffer_pool` and `Server::storage_stats()` report cache size, page reads/writes, hits and evictions
for the database and temporary stores.

What still has to fit in memory is the *working set of one statement*: the rows it scans and keeps, the rows it
returns, the rows a `JOIN`/`GROUP BY`/`ORDER BY` operator materialises, and the changes of one `INSERT/UPDATE/DELETE`.
Pushed-down conditions are applied while a table is read, so a selective query over a huge table only keeps the
matching rows, and index seeks and early-stopping ordered reads touch only the pages they need. The undo log of an
open transaction (a few dozen bytes per changed row) is also in memory.

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

* Statement working set must fit in memory (see above); there is no spill-to-disk for sorts, hash joins or aggregates.
* The page table (about 8 bytes per page) and the catalog are memory resident and are rewritten at each checkpoint.
* Pages are not merged or compacted after deletes; there is no `ALTER INDEX … REBUILD` to shrink a file, and the
  file never shrinks (freed blocks are reused).
* A checkpoint needs a moment with no open transaction; a transaction left open indefinitely keeps the log growing.
* `ALTER TABLE` rewrites and `SELECT INTO` hold the table's rows in memory and log them as one commit record.
* Stored sizes follow this engine's own row format, not SQL Server's.
