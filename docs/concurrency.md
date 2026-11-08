# Transactions, isolation levels and locking

Statements from different sessions run one at a time, but sessions interleave at statement boundaries
and a statement that needs a lock held by another transaction **waits** until it is released. The
server mutex is released while waiting, so other sessions keep running.

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
| `SERIALIZABLE` | as above, with a shared table lock held to the end | also phantoms |
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

`SERIALIZABLE` uses a table-level shared lock instead of key-range locks, so it blocks more writers than SQL Server
would. There is no lock escalation, no page locks, and no parallel execution of statements.
