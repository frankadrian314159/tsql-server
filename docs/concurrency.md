# Transactions, isolation levels and locking

Statements from different sessions run in parallel (see *Latching and parallel statements* at the end). A statement
that needs a lock held by another transaction **waits** until it is released; it holds no latch and no catalog guard while
it waits, so other sessions keep running.

## Autocommit and explicit transactions

A statement outside `BEGIN TRAN` is its own transaction, run under the session's isolation level
(`SET TRANSACTION ISOLATION LEVEL …`). `BEGIN TRAN` / `COMMIT` / `ROLLBACK` / `SAVE TRAN` nest and behave
like SQL Server (`@@TRANCOUNT`, error 3902/3903). If a statement fails inside a transaction only that
statement is undone; `SET XACT_ABORT ON` rolls the whole transaction back instead.

## Locks

| Resource | Modes |
| --- | --- |
| table | `IS`, `IX`, `S`, `U`, `SIX`, `X` |
| row | `S`, `U`, `X` |

Standard multi-granularity compatibility applies (`IS`/`IX` are compatible with each other, `X` with nothing,
`U` with `S` but not with `U`). Writers take `IX` on the table and `X` on each changed row; scans for
`UPDATE`/`DELETE` take `U` on matched rows and convert to `X`. DDL takes a schema-modification (`X`) lock on the
table, so it waits for every transaction that has the table locked.

## Isolation levels

| Level | Reads | Prevents |
| --- | --- | --- |
| `READ UNCOMMITTED` | newest version, even uncommitted; no locks | nothing |
| `READ COMMITTED` (default) | committed data; waits on rows another transaction is changing; no read locks kept | dirty reads |
| `REPEATABLE READ` | as above, plus shared row locks held to the end of the transaction | dirty and non-repeatable reads |
| `SERIALIZABLE` | as above, plus shared **key-range locks** on the index ranges it reads (a shared table lock for a scan with no usable index), held to the end | also phantoms |
| `SNAPSHOT` | the database as of the transaction's start; never waits; first-updater-wins | all of the above |
| `READ_COMMITTED_SNAPSHOT` (database option) | each statement sees the last committed state at its start; readers never wait | dirty reads |

Enable the snapshot levels with `ALTER DATABASE x SET ALLOW_SNAPSHOT_ISOLATION ON` and
`ALTER DATABASE x SET READ_COMMITTED_SNAPSHOT ON`. Using `SNAPSHOT` without it is error 3952. A snapshot
transaction that updates a row changed since it began is aborted with error 3960.

A scan waits only on rows it actually reads (a table scan reads the heap in chunks of 512 rows and applies the query's
pushed-down conditions as it goes, so memory is spent only on matching rows). A seek through an index touches just the matching rows, so a
writer holding row 1 does not block a reader of row 3; a table scan, which must examine row 1, does.

## Waiting, timeouts and deadlocks

* `SET LOCK_TIMEOUT n` (milliseconds; `0` = do not wait; default `-1` = wait forever). Expiry is error 1222.
* Waits form a graph. When a wait would close a cycle the session that detects it is chosen as the deadlock
  victim: its transaction is rolled back and it receives error 1205 ("…deadlocked on lock resources…"),
  the rest of its batch is abandoned, and the others proceed. Retry the transaction.
* Dropping a `Connection` rolls back its open transaction and releases its locks.

`SELECT * FROM sys.dm_tran_locks` lists the locks currently held.

## Differences from SQL Server

There is no lock escalation and no page locks. Statements of one session never run in parallel with each other (no
parallel query plans); statements of different sessions do (see Latching).

### Key-range locks

A `SERIALIZABLE` read that uses an index (an equality seek, `IN` list, or range on the first key column) takes an
intention-shared lock on the table, shared row locks on the rows it reads, and a shared **key-range lock** on the exact
range of that index it covered: a seek on `id = 25` locks the point `[25, 25]` even if no row has that key, so the gap is
protected; `id BETWEEN 15 AND 35` locks `[15, 35]`. A range is a pair of key *prefixes* compared with SQL Server's
ordering, so a seek on the first column of a two-column index locks every key that starts with that value.

Readers never conflict with readers. A writer that is about to add a row, or to change a row's indexed columns,
checks the new key in every index of the table against the ranges held by other transactions and waits (like any lock
wait, with the deadlock detection and `LOCK_TIMEOUT` of the other locks) if it falls inside one; deleting or changing a
row that was read is already stopped by that row's shared lock. Everything outside the ranges proceeds: other
transactions can insert, update and delete rows elsewhere in the table. A scan with no usable index has no range to lock,
so it takes a shared lock on the whole table, as SQL Server does for a scan of a heap. The ranges are listed in
`sys.dm_tran_locks` as `KEY_RANGE` / `RangeS_S`.

Differences from SQL Server: a range covers exactly the interval read (SQL Server locks each key and the gap before
it, so it can lock slightly more at the edges); a read that stops early (`TOP n` over an ordered index read) locks its
whole requested range, not only the part it reached; `RangeX-X`, `RangeI-N` and `RangeS-U` modes are not modelled
(writers check and wait instead of holding insert-range locks).

## Latching and parallel statements

Logical locks (above) protect transactions from each other. *Latches* protect the server's data structures while a
statement runs. Statements from different sessions run **in parallel**; what they share is protected as follows
(outermost latch first):

1. **The catalog** (`ServerState`: databases, objects, security, global temporary tables) is behind a
   reader-writer lock held through `SrvGuard`. Queries, DML, variables, cursors and transaction control only read
   the catalog and share it. Anything that changes it (DDL, security statements, database options, `SELECT ... INTO`,
   undoing DDL, a checkpoint) takes it exclusively, from the start of the statement or (for the rare late case)
   at the point of the change (`SrvGuard::cat_mut`; set `TSQL_TRACE_UPGRADES=1` to print a backtrace whenever a
   statement has to upgrade). The guard is taken lazily and given up between top-level statements and while a
   statement waits for a lock, so DDL waits for running statements, not for idle sessions.
2. **Write latches** (`coord::Latches`). A write checks keys and constraints and then adds row versions, and
   that must not interleave with another session's where it could break a rule. `Engine::apply_changes` takes:
   * the table **shared** (so writers to one table run side by side) plus *stripes*, exclusive latches hashed from
     the key values the statement checks or changes: the keys of the unique indexes it writes, the foreign-key values
     it inserts (child side) and the referenced values it deletes or changes (parent side). Two inserts of one key,
     or a child insert beside the delete of its parent, land on the same stripe and take turns; everything else
     proceeds in parallel;
   * or, when the table is connected through foreign keys to a cascading action (`ON DELETE/UPDATE CASCADE`,
     `SET NULL`, `SET DEFAULT`), the whole connected set of tables **exclusively**, because the statement may change
     rows in any of them.

   Latches are taken in one ascending order (tables by id, then stripes) and are never held while waiting for a
   transaction, so they cannot deadlock. A SERIALIZABLE reader needs no latch: it registers its key range before it
   reads, and a writer checks the ranges *after* putting its new versions in place, so either the reader sees the
   uncommitted version (and waits for the writer) or the writer sees the range (and waits for the reader); a wait
   undoes the statement.
3. **Core** (`coord::Core`): the commit clock and id counters (atomics), the table of running transactions (in 16
   shards by transaction id), the lock table and the commit lock, each with its own short latch. While nobody holds a lock
   the lock table answers "is anything in my way?" and "release everything" without its latch, and a transaction that
   changed nothing skips the commit protocol. Each session owns its row of the session table and its query statistics. Waiting for a lock sleeps until one of the
   transactions waited for has ended (`Core::wait_for_end`); deadlock detection records the wait and looks for a
   cycle in one step (`LockTable::wait_and_check`).
4. **The page store** has one reader-writer latch per store (the buffer pool). Lookups, and scans of a store without a page
   file, hold it shared and so run in parallel; loading a page, changing a tree or evicting holds it exclusively.
   A locking read (`REPEATABLE READ`, `UPDLOCK`) reads rows before it locks them, so after locking it re-checks that none
   was committed since the statement began and runs the statement again if one was.
5. **The redo log** (`ServerState::persist`) is appended to under its own latch.

Identity columns and sequences are shared counters (`SharedCounter`): a statement works on a copy of the table and
advances the real counter atomically, so it does not need to write the table back to the catalog.

### Commit protocol

1. Under the commit lock the transaction takes the next commit timestamp and appends its redo record to the log, so
   the log is in timestamp order. The lock is held for the append only, not for the disk force.
2. **Group commit:** the session waits until its record is on disk. One waiting session (the leader) forces the log
   for every record written so far; sessions that arrive during the force are covered by the next one, so commits that
   overlap share forces (`Server::log_forces` counts them). A failed force fails every waiting commit.
3. The transaction stamps every row version it wrote with its timestamp.
4. The timestamp is published as the clock only when it *and every earlier timestamp* have been stamped
   (`Core::finish_commit_ts`), so a snapshot, which reads the clock when it starts, never sees half a commit, even
   when commits finish out of order.
5. The transaction leaves the active table, old versions no snapshot needs are collected, and its locks are released.

A snapshot transaction reads the clock and enters the active table in one step, so the version-collection horizon
can never pass a snapshot that is just starting.

### Lost updates

A statement reads rows before it locks them. After taking the exclusive lock on a row to update or delete, the
statement checks that nobody committed a change to it since the statement began; if somebody did, the statement is
run again (as after any wait for a lock). That makes `UPDATE t SET n = n + 1` from many sessions exact under
READ COMMITTED.

### Schema locks

DDL on a table takes an exclusive table lock (`Engine::ddl_lock`) and the catalog exclusively. It therefore waits for
statements that are running, and for open transactions that have written the table or hold read locks on it
(`REPEATABLE READ`, `SERIALIZABLE`); a `READ COMMITTED` reader that is between statements holds nothing, as in SQL
Server.

### Shared counters and the log

Identity values and sequence values are handed out atomically. The log record of a sequence value is written while
the counter is held, so the log lists values in the order they were handed out and recovery never repeats one.
Global temporary tables (`##t`) are locked like other tables.

### Limits

* Writers to one table overlap, but two writes that touch the same unique key or foreign-key value (or hash to the
  same stripe, one in 4096) take turns, and a cascading foreign-key group is written by one session at a time.
* Tables with a cascading foreign key are latched as a group, so unrelated writes to tables connected to it wait.
* Commit records are appended one at a time (the append is short; the disk force is shared).
* If forcing the log fails, the log may still hold the record of a commit that was rolled back in memory; the server
  reports the I/O error to every session waiting for that force.
