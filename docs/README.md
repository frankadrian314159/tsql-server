# Documentation

This folder describes how the server behaves and how it is built. The [top-level README](../README.md) is the overview (what the
project is, how to run it, what T-SQL it covers, what it does not); the files here go into one subject each. This page says
which file to open for what, and in what order to read them.

## Where to start

| If you want to... | Read |
| --- | --- |
| try the server, or connect a client to it | [shell-and-gui.md](shell-and-gui.md), then [tds.md](tds.md) |
| run it for other people (logins, permissions, hints, configuration) | [administration.md](administration.md), [security.md](security.md), [authentication.md](authentication.md) |
| back it up, restore it, or load a SQL Server `.bak` | [backup.md](backup.md) |
| know what an isolation level or a lock does here | [concurrency.md](concurrency.md) |
| understand why a query is slow, or make it faster | [optimizer.md](optimizer.md), then [performance.md](performance.md) |
| run it on several machines | [distributed.md](distributed.md) |
| understand how data reaches the disk and survives a crash | [storage.md](storage.md) |
| find out whether a T-SQL feature is there, and how it differs from SQL Server | the feature files below, and the *Limitations* section of the [top-level README](../README.md#limitations) |
| change the code | [devel.md](devel.md), then the file for the subject you are changing |
| check the engine against something, or look for bugs | [benchmarks.md](benchmarks.md), [tsqlt.md](tsqlt.md) |

## The files

### Running and using the server

| File | What it covers |
| --- | --- |
| [shell-and-gui.md](shell-and-gui.md) | `tsql`, the interactive shell (`GO` batches, dot-commands, scripts and exit codes), and `tsql-gui`, the desktop app: navigation pane, datasheets, table designer, query editor with plans, CSV and bulk import, backup import and export, and its modes (in-process, or connected to a server over TDS, a cluster's control node included). |
| [tds.md](tds.md) | `tsql-serve` and the TDS 7.4 protocol: what clients can do (batches, RPC, transactions, bulk load, server cursors, table-valued parameters, MARS), TLS, how a connection is served, what is not supported, and which client libraries have been tried. |
| [administration.md](administration.md) | Sessions and `KILL`, `WAITFOR`, `sp_configure`, table and query hints, the plan cache and statistics, `BULK INSERT`. |
| [security.md](security.md) | Logins, users and roles, password policy and lockout, permissions and what is checked, column permissions, dynamic data masking, row-level security, SQL Server Audit. |
| [authentication.md](authentication.md) | Logins other than SQL logins: Azure AD access tokens and Kerberos; how to turn them on, what they need, what is not covered. |
| [backup.md](backup.md) | Full, differential and log backups, point-in-time restore, the native format and its limits, reading SQL Server `.bak` files, and writing them (`WITH SQL_SERVER_FORMAT`). |

### Features of the language

| File | What it covers |
| --- | --- |
| [collations.md](collations.md) | How strings compare: naming, where a collation applies (database, column, expression), how it is implemented, limits. |
| [temporal.md](temporal.md) | System-versioned tables: period columns, the history table, `FOR SYSTEM_TIME` queries. |
| [hierarchyid.md](hierarchyid.md) | The `hierarchyid` type and its methods; values from restored SQL Server databases are used as they are. |
| [spatial.md](spatial.md) | `geometry` and `geography`: shapes, methods, the binary form, spatial indexes, what is not supported. |
| [xml-schema.md](xml-schema.md) | `CREATE XML SCHEMA COLLECTION`, typed `xml`, the validator, and what happens to typed columns in a SQL Server backup. |
| [triggers.md](triggers.md) | DDL triggers, `LOGON` triggers, `ENABLE`/`DISABLE TRIGGER`, the xml methods and `OPENXML`, full-text search. |
| [tsqlt.md](tsqlt.md) | A tSQLt-compatible unit-test framework written in T-SQL (the real one needs CLR), the suites that run on it, and where it differs. |

### How the engine works

| File | What it covers |
| --- | --- |
| [concurrency.md](concurrency.md) | Transactions, locks (row, table and key-range), isolation levels including snapshot, waiting, timeouts and deadlocks, differences from SQL Server, intra-query parallelism, latching, the commit protocol. |
| [storage.md](storage.md) | Pages, the buffer pool and the page file, shadow paging, the write-ahead log and recovery, the undo log, rows, versions and indexes, working memory and spilling to disk, prepared transactions, snapshots of the whole state, the layout of a data directory. |
| [optimizer.md](optimizer.md) | How a query is planned: access paths, pushdown, join order and method, subquery rewrites (correlated subqueries become joins), column statistics, how to see a plan, filtered and XML indexes. |

### For people changing the code

| File | What it covers |
| --- | --- |
| [devel.md](devel.md) | Where to change what: a map of the source, how to add a function, an aggregate, a statement, a data type, a system view or an error, the rules for optimizer, storage and cluster changes, how the tests are organised and run (three modes), and how to see what the engine is doing. |

### Speed, benchmarks and the cluster

| File | What it covers |
| --- | --- |
| [performance.md](performance.md) | How to measure, then a history of what was found and fixed, round by round (locking, group commit, buffer pools, TPC-H/TPC-DS and OLTP work, subqueries and statistics, parallel joins, what a row costs), with the numbers and what did not help. |
| [benchmarks.md](benchmarks.md) | The workload programs (TPC-H/TPC-DS-style queries, TPC-C/TPC-E-style OLTP, a SQLsmith-style fuzzer): how to run them, what they check, the bugs they found, what they do not cover. They are not the audited benchmarks; their numbers are for comparing this engine with itself. |
| [distributed.md](distributed.md) | The cluster: control node and shard nodes, running one, how a statement is routed, planned and merged, transactions across shards, elasticity, replication, operating it, benchmarks on it, foreign keys, SQL Server clients, limits, and the distribution advisor. |

## Reading paths

* **New to the project:** the [top-level README](../README.md), [shell-and-gui.md](shell-and-gui.md), then whichever feature file matches
  what you are trying to run.
* **Operating a server:** [tds.md](tds.md), [security.md](security.md), [administration.md](administration.md), [backup.md](backup.md);
  [storage.md](storage.md) for what is on disk and what a crash leaves.
* **Working on performance:** [optimizer.md](optimizer.md) (the plans and what shapes them), then [performance.md](performance.md)
  (*Measuring* first; the sixth round says how its numbers were compared: binaries run back to back on a machine doing nothing else),
  then [benchmarks.md](benchmarks.md) for the programs to run.
* **Changing the code for the first time:** [devel.md](devel.md) (the map and the rules), then the file for the subject, then the tests of
  that area.
* **Working on correctness or concurrency:** [concurrency.md](concurrency.md), [storage.md](storage.md), and [benchmarks.md](benchmarks.md)
  (the fuzzer and the consistency checks of the OLTP programs).
* **Working on the cluster:** [distributed.md](distributed.md), then the cluster rows of [performance.md](performance.md).

## Conventions

* Each file says what is **not** supported in a section of its own (*Limits*, *Not supported*, *What is not covered*) or beside the
  feature. Where behaviour differs from SQL Server on purpose, the file says so; the top-level README's *Behaviour notes* and
  *Limitations* sections collect the main ones.
* Code is named by its path (`src/engine/optimizer.rs`, `crates/parallel/control/src/joins.rs`); the documentation of a subsystem and its
  source are meant to be read together.
* Numbers in [performance.md](performance.md) and [distributed.md](distributed.md) are measurements on the machines named there; they are
  there to show what a change did, not to promise a figure.
* The tests are the specification that the files describe: `tests/*.rs` for the engine, `crates/parallel/*/tests/` for the cluster. The test
  commands, and the modes the suite can run in (`TSQL_TEST_PAGED`, `TSQL_TEST_SPILL`), are under *Development* in the top-level README
  (the spill mode is described in [storage.md](storage.md)).

## Keeping this folder current

* A change to behaviour changes the file that describes it, in the same change; a new subject gets a new file.
* A new file is added to the tables above, in the group it belongs to, with a line on what it covers.
* A file that grows a long history (as [performance.md](performance.md) has) keeps its current state at the top or in a *Where the time
  goes now* section, and the history below it.
