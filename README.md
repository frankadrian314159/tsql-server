# tsql-server

A clone of Microsoft SQL Server written in Rust: a from-scratch T-SQL engine on top of a
durable, transactional storage layer with lock-based isolation levels, permission checking
and a cost-based query optimizer. It runs as a library (`Server` / `Connection`) and as a
`sqlcmd`-style shell (`tsql`).

It aims to reproduce SQL Server's *observable behaviour*: result values, data-type semantics,
error numbers and message text, `@@ROWCOUNT` / `@@ERROR` / `@@TRANCOUNT` bookkeeping,
statement-terminating vs batch-aborting errors, blocking and deadlocks, permission errors.

> **Scope note.** "All of T-SQL" is the size of a multi-year product. This project implements
> the language core and the commonly used surface faithfully and is explicit about what is not
> implemented (see [Limitations](#limitations)). The wire protocol (TDS) is **not** implemented:
> you talk to the engine through the Rust API or the bundled shell, not through ODBC/ADO.NET/SSMS.

## What it has

| Area | Summary | Details |
| --- | --- | --- |
| **Durable, paged storage** | Tables and indexes are B+trees of 8 KiB pages in a page file behind a buffer pool, so a database can be far larger than memory. Every commit is written to a checksummed write-ahead log and fsynced before it returns; shadow paging makes checkpoints atomic and lets any page be evicted at any time; recovery replays the log and discards a torn tail. | [docs/storage.md](docs/storage.md) |
| **Transactions & isolation** | `READ UNCOMMITTED`, `READ COMMITTED` (locking), `REPEATABLE READ`, `SERIALIZABLE`, `SNAPSHOT`, `READ_COMMITTED_SNAPSHOT`; row and table locks with intent modes, blocking, lock timeouts (1222), deadlock detection (1205). | [docs/concurrency.md](docs/concurrency.md) |
| **Security** | Logins with salted password hashes, database users, roles (fixed and custom), `GRANT` / `DENY` / `REVOKE`, ownership chaining, `EXECUTE AS`, permission errors with SQL Server's numbers and text. | [docs/security.md](docs/security.md) |
| **Query optimizer** | Predicate pushdown, index selection (seek / range / IN-list), cost-based join ordering, index nested-loop and hash joins, ordered index reads that stop early for `TOP … ORDER BY`, `SET SHOWPLAN_TEXT`. | [docs/optimizer.md](docs/optimizer.md) |
| **T-SQL language** | Queries, DML, DDL, procedural code, cursors, dynamic SQL, JSON, windows, CTEs, MERGE, triggers, … | [Coverage](#t-sql-coverage) |

## Quick start

```text
cargo build --release
cargo run --release --bin tsql -- -D ./data           # durable shell on ./data (created if missing)
cargo run --release --bin tsql -- -D ./data --pool-pages 4096   # cache 4096 pages = 32 MiB
cargo run --release --bin tsql                         # in-memory shell (nothing is written)
cargo run --bin tsql -- -D ./data -Q "SELECT @@VERSION"
cargo run --bin tsql -- -D ./data -U ann -P secret     # log in as a SQL login
cargo test                                             # ~165 tests
TSQL_TEST_PAGED=1 cargo test                           # same tests on a disk server with an 8-page cache
```

```text
1> CREATE TABLE emp (id INT IDENTITY PRIMARY KEY, name VARCHAR(50), salary DECIMAL(10,2))
2> INSERT emp (name, salary) VALUES ('Alice', 100.5), ('Bob', 90)
3> SELECT name, RANK() OVER (ORDER BY salary DESC) AS r FROM emp
4> GO
```

### Library use

```rust
use tsql_server::Server;

let server = Server::open("./data")?;     // durable; Server::new() is in-memory only
let mut conn = server.connect();          // trusted session as the built-in `sa` (sysadmin)

conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(20))");
let rows = conn.query("SELECT id, name FROM t ORDER BY id")?;   // first result set, or the first error

let res = conn.execute("SELECT 1/0; PRINT 'still runs'");       // BatchResult with ordered events
for ev in &res.events { /* Rows | Message | RowCount | Error */ }

conn.execute_script("CREATE TABLE a (x INT)\nGO\nINSERT a VALUES (1)\nGO 3");   // GO / GO n

let mut ann = server.connect_as("ann", "secret")?;              // authenticated session
let plan = conn.explain("SELECT * FROM t WHERE id = 5")?;       // like SET SHOWPLAN_TEXT ON
```

Sessions can be used from many threads at once (`Server` is `Clone`; move a `Connection` into each
thread). Statements from different sessions interleave at statement boundaries and block on locks
exactly as described in [docs/concurrency.md](docs/concurrency.md).

## Architecture

| Module | Responsibility |
| --- | --- |
| `lexer`, `parser`, `ast` | Tokenizer, recursive-descent parser, typed AST for every statement and expression form below |
| `types`, `decimal`, `convert` | `DataType`, runtime `Value`, 38-digit `Dec`, `CAST`/`CONVERT` with styles, SQL comparison and arithmetic with SQL Server's result-type rules |
| `storage` | Page file, buffer pool and B+tree (`page`); table heap with MVCC version chains and indexes on top of it; lock manager; transaction and undo-log types |
| `catalog` | Databases, schemas, tables, views, procedures, functions, triggers, sequences, user types |
| `persist` | Write-ahead log, checkpoints, crash recovery, data-directory locking |
| `security` | Logins, users, roles, permission resolution |
| `engine` | Sessions, batches, control flow, `TRY…CATCH`, transactions, procedures; query pipeline; DML with constraints / foreign keys / triggers; DDL; optimizer (`optimizer`, `joins`); lock-aware reads (`txn`); permission checks (`secexec`); built-in functions; `sys.*` views |

## T-SQL coverage

**Queries** — `SELECT` (`DISTINCT`, `TOP n [PERCENT] [WITH TIES]`, `OFFSET/FETCH`, `INTO`, variable assignment),
`INNER/LEFT/RIGHT/FULL/CROSS JOIN`, `CROSS/OUTER APPLY`, derived tables, `VALUES`, `GROUP BY` / `ROLLUP` / `CUBE` /
`GROUPING SETS`, `HAVING`, `UNION [ALL]` / `INTERSECT` / `EXCEPT`, correlated subqueries, `EXISTS`, `IN`, `ANY/SOME/ALL`,
`PIVOT`/`UNPIVOT`, CTEs including recursive with `MAXRECURSION`, window functions with `ROWS`/`RANGE` frames,
`FOR JSON PATH/AUTO`, `FOR XML PATH/RAW/AUTO`.

**DML** — `INSERT` (`VALUES`, `SELECT`, `EXEC`, `DEFAULT VALUES`), `UPDATE … FROM`, `DELETE … FROM`, `MERGE`
(all `WHEN` forms, `$action`), `TRUNCATE`, `OUTPUT [INTO]`, `TOP`, `SET ROWCOUNT`, identity columns
(`IDENTITY_INSERT`, `SCOPE_IDENTITY`, `DBCC CHECKIDENT`), computed columns, sequences, `rowversion`, DML through simple
views and CTEs.

**Constraints** — `PRIMARY KEY`, `UNIQUE` (NULLs compare equal), unique indexes, `CHECK`, `DEFAULT`, `NOT NULL`,
`FOREIGN KEY` with `NO ACTION` / `CASCADE` / `SET NULL` / `SET DEFAULT` (including self-referencing), `NOCHECK` /
`CHECK CONSTRAINT`. Keys are enforced through real B-tree indexes, so they are also visible to other transactions'
locks (an insert of a key being inserted by an uncommitted transaction waits).

**DDL** — `CREATE/ALTER/DROP` `TABLE`, `VIEW`, `PROCEDURE`, `FUNCTION` (scalar, inline TVF, multi-statement TVF),
`TRIGGER` (`AFTER` / `INSTEAD OF`), `INDEX`, `SCHEMA`, `DATABASE`, `TYPE`, `SEQUENCE`, `LOGIN`, `USER`, `ROLE`;
`CREATE OR ALTER`; `DROP … IF EXISTS`; `ALTER TABLE`; `ALTER DATABASE … SET ALLOW_SNAPSHOT_ISOLATION /
READ_COMMITTED_SNAPSHOT`; temp tables and table variables. DDL is transactional.

**Programmability** — variables, `IF/ELSE`, `WHILE`, `GOTO`, `TRY…CATCH` with `ERROR_*()`, `THROW`, `RAISERROR`,
`PRINT`, procedures (`OUTPUT`, defaults, return codes, table-valued parameters), `EXEC('…')`, `sp_executesql`,
cursors, `BEGIN TRAN` / `COMMIT` / `ROLLBACK` / `SAVE TRAN`, `SET XACT_ABORT`, `SET NOCOUNT`,
`SET TRANSACTION ISOLATION LEVEL`, `SET LOCK_TIMEOUT`, `USE`, `CHECKPOINT`, `GO [n]`.

**Data types** — `bit`, `tinyint`, `smallint`, `int`, `bigint`, `decimal/numeric(p,s)` (to 38 digits), `money`,
`smallmoney`, `float`, `real`, `char`, `varchar`, `nchar`, `nvarchar` (`max`), `text`, `ntext`, `binary`, `varbinary`,
`image`, `date`, `time(n)`, `datetime`, `smalldatetime`, `datetime2(n)`, `datetimeoffset`, `uniqueidentifier`, `xml`
(stored as text), `sql_variant`, `rowversion`, user-defined alias and table types.

**Built-in functions** (non-exhaustive) — strings (`LEN … STRING_AGG STRING_SPLIT FORMAT`), math, dates
(`DATEADD DATEDIFF DATEPART EOMONTH AT TIME ZONE …`), conversion (`CAST CONVERT TRY_CAST PARSE`), logical, aggregates,
window functions, JSON (`JSON_VALUE JSON_QUERY JSON_MODIFY OPENJSON ISJSON`), `HASHBYTES`, system functions
(`@@ROWCOUNT … OBJECT_ID DB_NAME USER_NAME IS_MEMBER HAS_PERMS_BY_NAME …`), table-valued `STRING_SPLIT`,
`GENERATE_SERIES`, `OPENJSON`; `sys.*` / `INFORMATION_SCHEMA.*` views (including `sys.dm_tran_locks`,
`sys.database_principals`, `sys.database_permissions`) and `sp_*` system procedures.

## Behaviour notes

* Strings compare case-insensitively and ignore trailing spaces (`SQL_Latin1_General_CP1_CI_AS`-like).
* Integer arithmetic overflows raise error 8115; decimal division follows SQL Server's scale rules.
* A runtime error ends the current statement and the batch continues, unless inside `TRY`, under `XACT_ABORT ON`, or the
  error is batch-aborting (`THROW`, syntax errors, deadlock victims, …). A failed statement inside a transaction undoes
  only that statement.
* Autocommit: a statement outside `BEGIN TRAN` is its own transaction under the session's isolation level.
* Without an `ORDER BY`, row order is whatever the chosen access path produces (as in SQL Server).

## Limitations

Things that are **not** implemented, so you are not surprised:

* **TDS wire protocol / network listener** — use the Rust API or the shell.
* **Memory use** — stored data (heaps, indexes, version chains, large values) live in a paged file and are cached by a
  bounded buffer pool, so a database may be much larger than RAM. What must still fit in memory is the working set of
  one statement (rows it scans and keeps, joins, sorts, aggregates and results are materialised; there is no spill
  for those operators), the catalog, the page table (~0.1 % of the data size) and the undo log of open transactions.
  Pages are not merged after deletes and the file never shrinks (freed space is reused). A checkpoint is only taken
  when no transaction is open, so a transaction left open indefinitely keeps the log growing. See
  [docs/storage.md](docs/storage.md).
* **Lock granularity** — row locks and table locks with intent modes; no page locks or lock escalation. `SERIALIZABLE`
  prevents phantoms with a table-level shared lock rather than key-range locks, so it blocks more writers than SQL
  Server would. Statements from different sessions are executed one at a time (interleaved at statement boundaries),
  not in parallel.
* **Permissions** — object-, schema- and database-level permissions; **column-level** permissions are rejected;
  security statements (`CREATE LOGIN`, `GRANT`, …) take effect immediately and are not rolled back with a transaction;
  no certificates, signatures, row-level security, dynamic data masking, auditing, or Windows authentication.
* **Optimizer** — heuristic and statistics-on-demand (distinct counts come from the indexes, exact for small ones and
  sampled for large ones);
  no histograms on unindexed columns, no hints (`OPTION (HASH JOIN)`, `FORCE ORDER`, …), outer joins are not reordered,
  subqueries are executed per row rather than rewritten to semi-joins, and there is no plan cache.
* **XML / spatial / hierarchy** — `xml` is stored as text with no XQuery (`.value()`, `.query()`, `OPENXML`);
  `geography`, `geometry`, `hierarchyid` are not functional. No full-text search, Service Broker, CLR, SQL Agent,
  linked servers, `BULK INSERT`, temporal tables, graph tables, columnstore, partitioning, replication, backup/restore
  statements (copy the data directory while the server is stopped instead).
* **Collations** — only the case/trailing-space-insensitive default; `COLLATE` clauses are parsed but do not change
  comparisons. `RANGE` frames with numeric offsets treat the bound as `CURRENT ROW`.
* Cultures other than `en-US` for `FORMAT`/`PARSE`; `CHECKSUM`/`BINARY_CHECKSUM` values differ from SQL Server's.

## Development

```text
cargo test                # integration tests in tests/*.rs
cargo clippy              # lint
```

Tests are grouped by area: `datatypes`, `queries`, `dml`, `programmability`, `functions`, `ddl_catalog`, `views_dml`,
`isolation` (real threads), `persistence` (crash simulation, torn log tails), `paging` (data far larger than the buffer
pool, eviction of uncommitted pages, crash during a checkpoint, overflow rows, space reuse), `security`, `optimizer`
(results with the optimizer on and off must be identical, plus plan assertions). Unit tests in `src/storage` check the
B+tree against a `BTreeMap` while the cache is forced to evict. `TSQL_TEST_PAGED=1 cargo test` runs the whole suite
against a disk-backed server with an 8-page cache. Expected values encode real SQL Server behaviour.

## License

MIT — see `LICENSE`.
