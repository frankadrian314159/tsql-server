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
> implemented (see [Limitations](#limitations)). The TDS wire protocol
> is implemented (`tsql-serve`, see [docs/tds.md](docs/tds.md)) for SQL-authenticated connections (optionally encrypted with TLS), so
> ADO.NET and tiberius can connect; so can the Rust API and the bundled shell.

## What it has

| Area | Summary | Details |
| --- | --- | --- |
| **Durable, paged storage** | Tables and indexes are B+trees of 8 KiB pages in a page file behind a buffer pool, so a database can be far larger than memory. Every commit is written to a checksummed write-ahead log and fsynced before it returns; shadow paging makes checkpoints atomic and lets any page be evicted at any time; recovery replays the log and discards a torn tail. | [docs/storage.md](docs/storage.md) |
| **Transactions & isolation** | `READ UNCOMMITTED`, `READ COMMITTED` (locking), `REPEATABLE READ`, `SERIALIZABLE`, `SNAPSHOT`, `READ_COMMITTED_SNAPSHOT`; row and table locks with intent modes, blocking, lock timeouts (1222), deadlock detection (1205). | [docs/concurrency.md](docs/concurrency.md) |
| **Backup & recovery** | `BACKUP DATABASE` (a consistent snapshot while others write), `BACKUP LOG`, `RESTORE DATABASE` / `LOG` with `NORECOVERY`, `STOPAT` point-in-time recovery, `VERIFYONLY`, `HEADERONLY`, recovery models. | [docs/backup.md](docs/backup.md) |
| **Collations** | `COLLATE` on databases, columns and expressions: case-, accent- and binary-sensitive comparison, ordering, grouping, joins, indexes and unique keys. | [docs/collations.md](docs/collations.md) |
| **Administration** | `WAITFOR`, `KILL`, `sp_who`/`sp_who2`, `sp_configure`/`RECONFIGURE`, table hints (`NOLOCK`, `UPDLOCK`, `READPAST`, `NOWAIT`, ...), `OPTION` hints, a per-connection parse cache, `sys.dm_exec_sessions`/`requests`/`query_stats`/`cached_plans`, `BULK INSERT`, SQL login password policy and lockout, DDL and LOGON triggers with `EVENTDATA()` ([docs/triggers.md](docs/triggers.md)), SQL Server Audit ([docs/security.md](docs/security.md#sql-server-audit)). | [docs/administration.md](docs/administration.md) |
| **Security** | Logins with salted password hashes, database users, roles (fixed and custom), `GRANT` / `DENY` / `REVOKE`, ownership chaining, `EXECUTE AS`, permission errors with SQL Server's numbers and text. | [docs/security.md](docs/security.md) |
| **Query optimizer** | Predicate pushdown, index selection (seek / range / IN-list), cost-based join ordering, index nested-loop and hash joins, ordered index reads that stop early for `TOP … ORDER BY`, `SET SHOWPLAN_TEXT`. | [docs/optimizer.md](docs/optimizer.md) |
| **Temporal tables** | `PERIOD FOR SYSTEM_TIME`, `SYSTEM_VERSIONING = ON` with an automatic or named history table, `FOR SYSTEM_TIME AS OF / FROM..TO / BETWEEN / CONTAINED IN / ALL`, `HIDDEN` period columns. | [docs/temporal.md](docs/temporal.md) |
| **Performance** | A benchmark (`cargo run --release --example bench`) and a span profiler (`--features prof`) to find hot spots; index seeks for correlated subqueries, in-place B+tree updates, no global catalog lock for reads, per-session bookkeeping and a reader-writer page store so read throughput scales with cores. | [docs/performance.md](docs/performance.md) |
| **Benchmarks and tests in T-SQL** | TPC-H (22 queries) and a TPC-DS subset, TPC-C and a reduced TPC-E (concurrent terminals with the benchmark's consistency conditions), a SQLsmith-style fuzzer with differential, ternary-logic and NoREC oracles, and about 195 tSQLt-style unit tests written in T-SQL (`cargo run --release --example tpc`, `--example tpc_oltp`, `--example sqlsmith`, `cargo test --test tsqlt`). | [docs/benchmarks.md](docs/benchmarks.md), [docs/tsqlt.md](docs/tsqlt.md) |
| **Shell and desktop app** | `tsql`: an interactive shell with `GO` batches, grids, dot-commands (`.tables`, `.schema`, `.import`, `.export`, `.plan`, `.storage`, …) and script/exit-code support. `tsql-gui`: an egui app with a navigation pane, editable datasheets, a table designer, a query editor with plans, CSV import/export, transactions and live storage statistics. | [docs/shell-and-gui.md](docs/shell-and-gui.md) |
| **TDS server** | `tsql-serve` listens on a TCP port and speaks TDS 7.4: SQL logins, batches, `sp_executesql`, prepared statements and procedure calls with OUTPUT parameters and return status, transactions, attention, all the data types. Checked against `System.Data.SqlClient` and tiberius. TLS supported. | [docs/tds.md](docs/tds.md) |
| **T-SQL language** | Queries, DML, DDL, procedural code, cursors, dynamic SQL, JSON, windows, CTEs, MERGE, triggers, … | [Coverage](#t-sql-coverage) |

## Quick start

```text
cargo build --release
cargo run --release --bin tsql -- -D ./data           # durable shell on ./data (created if missing)
cargo run --release --bin tsql -- -D ./data --pool-pages 4096   # cache 4096 pages = 32 MiB
cargo run --release --bin tsql                         # in-memory shell (nothing is written)
cargo run --release --bin tsql-serve -- ./data --sa-password "Str0ng!pw"   # TDS server on 127.0.0.1:1433
cargo run --release --features gui --bin tsql-gui      # desktop app (add a data directory to keep your data)
cargo run --bin tsql -- -D ./data -Q "SELECT @@VERSION"
cargo run --bin tsql -- -D ./data -U ann -P secret     # log in as a SQL login
cargo test                                             # ~200 tests
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
| `inspect`, `tools` | Catalog introspection (`table_info`, `objects`, …) and helpers for front ends: result grids, CSV import/export, SQL literals, table scripts |
| `tds/`, `serve.rs` | The TDS 7.4 listener (`tds::Listener`) and the `tsql-serve` program: packets, login, batches, RPC, transaction-manager requests, token streams, data type encoding |
| `main.rs`, `gui/` | The `tsql` shell, and the optional `tsql-gui` desktop app (cargo feature `gui`) |
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
* Integer arithmetic overflows raise error 8115. A decimal division's scale comes from the values' scales, not from SQL Server's rule for the operands' declared precision, so it can have fewer decimals.
* A runtime error ends the current statement and the batch continues, unless inside `TRY`, under `XACT_ABORT ON`, or the
  error is batch-aborting (`THROW`, syntax errors, deadlock victims, …). A failed statement inside a transaction undoes
  only that statement.
* Autocommit: a statement outside `BEGIN TRAN` is its own transaction under the session's isolation level.
* Local temporary tables created in a stored procedure or a dynamic batch (`EXEC (...)`, `sp_executesql`) are dropped when it ends, and the `SET` options it changed (`NOCOUNT`, `DATEFIRST`, `LANGUAGE`, `XACT_ABORT`, ...) return to the caller's.
* Expressions nested more than 100 levels deep, or chains of more than 700 terms, are refused with error 8632 instead of overflowing the stack.
* Without an `ORDER BY`, row order is whatever the chosen access path produces (as in SQL Server).

## Limitations

Things that are **not** implemented, so you are not surprised:

* **TDS: integrated authentication, bulk load, server cursors, MARS** — see [docs/tds.md](docs/tds.md). TLS is
  supported (a self-signed certificate is generated unless you give one; `--force-encryption` makes it mandatory).
* **Memory use** — stored data (heaps, indexes, version chains, large values) live in a paged file and are cached by a
  bounded buffer pool, so a database may be much larger than RAM. What must still fit in memory is the working set of
  one statement: single-table `ORDER BY`, `DISTINCT`, `TOP n` and `GROUP BY`, and joins of base tables (a streaming hash
  join that spills to partition files, grace-hash style), stream and spill to temporary files, and the rows of such a
  `SELECT` are delivered to a TDS client as they are produced; so are right and full joins, window functions that share
  one `OVER` clause (one partition at a time), set operations (external sort/merge) and `SELECT`s over a derived table or
  CTE (their buffers spill to a file); scalar aggregates, `APPLY`, views, recursive CTEs and a derived table that is
  itself joined are still materialised;
  plus the catalog, the page table (~0.1 % of the data size) and the undo log of open transactions.
  Sparse leaves are merged after deletes and the page file is compacted and truncated at checkpoints. A checkpoint is only taken
  when no transaction is open, so a transaction left open indefinitely keeps the log growing. See
  [docs/storage.md](docs/storage.md).
* **Lock granularity** — row locks and table locks with intent modes; no page locks or lock escalation. `SERIALIZABLE`
  prevents phantoms with shared key-range locks on the index ranges it reads (a table lock for a scan with no usable
  index; see [docs/concurrency.md](docs/concurrency.md)). Statements from different sessions run in parallel: the catalog is shared by queries and DML, writers latch
  only the key values they check or change, commits are atomic and share log forces (group commit); see the latching
  section of that document.
* **Permissions** — object-, schema- and database-level permissions; **column-level** `SELECT` / `UPDATE` permissions (see [docs/security.md](docs/security.md));
  security statements (`CREATE LOGIN`, `GRANT`, …) take effect immediately and are not rolled back with a transaction;
  **dynamic data masking** and **row-level security** (filter and block predicates); **password policy and lockout**, **SQL Server Audit**, DDL and LOGON triggers; no certificates, signatures, Always Encrypted, contained databases or Windows authentication.
* **Optimizer** — heuristic and statistics-on-demand (distinct counts come from the indexes, exact for small ones and
  sampled for large ones);
  no histograms on unindexed columns, `OPTION (HASH JOIN | LOOP JOIN | FORCE ORDER)` and table hints are honoured (see
  [docs/administration.md](docs/administration.md)), outer joins are not reordered, a correlated subquery is executed per outer row (an uncorrelated one once per statement)
  rather than being rewritten to a semi-join or a grouped join, and only parsed batches (not execution plans) are cached.
* **Triggers on views** — `INSTEAD OF` triggers on views are not supported (`AFTER` triggers on tables are; a view that maps onto one table is updatable, with `WITH CHECK OPTION`).
* **Tools** — the shell and the desktop app use one session each (see [docs/shell-and-gui.md](docs/shell-and-gui.md)).
* **XML / spatial / hierarchy** — `xml` is stored as text; `.value()`, `.exist()` and `.query()` handle simple XPath only, with no `.nodes()`, `.modify()` or `OPENXML` (see [docs/triggers.md](docs/triggers.md));
  `geography`, `geometry`, `hierarchyid` are not functional. No full-text search, Service Broker, CLR, SQL Agent,
  linked servers, `OPENROWSET(BULK)`, graph tables, columnstore, partitioning, replication, 
  backup compression (full, differential and log backups, restore and point-in-time recovery exist; see [docs/backup.md](docs/backup.md)).
* **Collations** — case, accent and binary sensitivity (`CI`/`CS`, `AI`/`AS`, `BIN`/`BIN2`) per database, column and expression, in
  comparisons, sorting, grouping, joins, indexes and constraints; language-specific alphabetical orders and kana/width
  sensitivity are not implemented (see [docs/collations.md](docs/collations.md)). `RANGE` frames with numeric offsets treat
  the bound as `CURRENT ROW`.
* Cultures other than `en-US` for `FORMAT`/`PARSE`; `CHECKSUM`/`BINARY_CHECKSUM` values differ from SQL Server's.

## Development

```text
cargo test                # integration tests in tests/*.rs
cargo clippy              # lint
```

Tests are grouped by area: `datatypes`, `queries`, `dml`, `programmability`, `functions`, `ddl_catalog`, `views_dml`,
`isolation` (real threads), `persistence` (crash simulation, torn log tails), `tools` (introspection, CSV, scripts), `cli` (the shell as a process), `tds` (a tiberius client against the listener), `tds_raw` (protocol-level RPC, prepared statements, transaction-manager requests), `paging` (data far larger than the buffer
pool, eviction of uncommitted pages, crash during a checkpoint, overflow rows, space reuse), `security`, `optimizer`
(results with the optimizer on and off must be identical, plus plan assertions). Unit tests in `src/storage` check the
B+tree against a `BTreeMap` while the cache is forced to evict. `TSQL_TEST_PAGED=1 cargo test` runs the whole suite
against a disk-backed server with an 8-page cache. `cargo test --features gui` also runs the GUI's headless tests
(views are driven without a window). Expected values encode real SQL Server behaviour.

## License

MIT — see `LICENSE`.
