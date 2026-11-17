# Developing the server: where to change what

A guide for someone about to change the code. It says where each kind of change goes, what else has to change with it, how to
test it, and how to find out what the engine is doing. The overview of the design is in the [top-level README](../README.md#architecture);
the subsystems are described in the other files of this folder (see the [index](README.md)). This file does not repeat them: it
points at them.

## Building, running, testing

```text
cargo build --release                  # the library, the shell (tsql), the TDS server (tsql-serve), tsql-bakinfo
cargo test                             # the integration tests in tests/*.rs and the unit tests in src/
cargo test --workspace                 # the same, plus the cluster crates in crates/parallel/
cargo test --features gui              # also the desktop app's headless tests
cargo run --release --example bench    # the micro workloads (see performance.md)
```

Three modes run the same suite against other configurations, and a change that touches storage or execution should be run in all of
them (the tests are unchanged; the server they get is different, see `tests/common/mod.rs`):

| Mode | Set | What it gives the tests |
| --- | --- | --- |
| default | nothing | an in-memory server |
| paged | `TSQL_TEST_PAGED=1` | a disk-backed server with an 8-page buffer pool, so pages are evicted all the time |
| spill | `TSQL_TEST_SPILL=1` | a tiny work-memory budget, so every query that can use the streaming executor does, and sorts, groups and joins spill to files |

The cluster tests build and run shard nodes in the test process. If a cluster is running from the same `target/` directory its
binaries are locked on Windows; build the tests elsewhere with `CARGO_TARGET_DIR=some-other-dir`.

Cargo features: `gui` (the desktop app and its dependencies), `prof` (span timing, below), `fast-alloc` (mimalloc), and
`kerberos-windows` / `kerberos-linux` / `kerberos-macos` (see [authentication.md](authentication.md)).

## Where things are

The root crate is the single-node server (`src/`). The cluster is a workspace beside it (`crates/parallel/`).

| You are changing | Look in |
| --- | --- |
| the syntax of a statement or an expression | `src/lexer.rs`, `src/parser.rs`, `src/ast.rs` |
| what a statement does | `src/engine/mod.rs` (`exec_inner`, the dispatcher) and the file for its kind: `ddl.rs`, `dml.rs`, `query.rs`, `txn.rs`, `admin.rs`, `backup.rs`, ... |
| the value of an expression, a built-in function | `src/engine/expr.rs` (the evaluator, `Engine::ev`), `src/engine/funcs.rs` (`Engine::builtin`, the functions) |
| a data type, a conversion, a comparison | `src/types.rs` (`DataType`, `Value`), `src/convert.rs` (`CAST`, `compare`, `arith`), `src/decimal.rs` |
| how a query is answered | `src/engine/query.rs` (the general executor), `src/engine/stream.rs` (the streaming executor), `src/engine/optimizer.rs` (access paths, pushdown), `src/engine/joins.rs` and `hashjoin.rs` (joins), `src/engine/stats.rs` (column statistics), `src/engine/aggacc.rs` (folded aggregates), `src/engine/par.rs` (parallelism), `src/rewrite/` (subquery rewrites) |
| tables, rows, indexes, locks | `src/storage/`, `src/catalog.rs`, `src/coord.rs` (sessions, the lock and commit coordination) |
| the log, checkpoints, recovery | `src/persist.rs` |
| permissions, logins, masking, row-level security, audit | `src/security.rs`, `src/mask.rs`, `src/engine/rls.rs`, `src/audit.rs`, `src/engine/auditing.rs` |
| `sys.*` views, `sp_*` procedures | `src/engine/sysviews.rs` (`Engine::sysview`), `src/engine/sysprocs.rs` |
| the TDS server | `src/tds/` (`mod.rs` the connection, `rpc.rs`, `wire.rs` the encoding, `cursor.rs`, `bulk.rs`, `mars.rs`, `tls.rs`, `auth.rs`) |
| backups | `src/engine/backup.rs`, `src/engine/bakimport.rs`, `src/engine/bakexport.rs`; the SQL Server formats in `src/mtf.rs`, `src/mssqlbak.rs`, `src/sqlbak.rs`, `src/sqlbak_write.rs`, `src/xpress.rs`, `src/binxml.rs` |
| xml, spatial, hierarchyid, full-text | `src/xmlpath.rs`, `src/xsd.rs`, `src/spatial.rs` and `src/engine/spatialfn.rs`, `src/hierarchyid.rs`, `src/fulltext.rs` |
| the shell and the desktop app | `src/main.rs`, `src/gui/` (`backend.rs` is the seam between the app and a connection: embedded, or remote over TDS in `remote.rs`) |
| catalog introspection and helpers shared by the front ends | `src/inspect.rs`, `src/tools.rs` |
| the cluster | `crates/parallel/` (below) |

A statement's path through the engine, for orientation: the text is lexed and parsed into `Stmt`s (`parser::parse`); `Engine::exec_stmt`
wraps each with transaction and error handling and `exec_inner` matches on the variant; a `SELECT` goes to `exec_query`, which tries the
rewrites (`decorrelated`, `fold_scalar_subqueries`), then `try_stream_select` (the streaming executor, for big tables, joins, grouping,
ordering) and otherwise `exec_select`, which evaluates the `FROM` (`eval_from_list`, `scan_table_leaf`, `run_cluster` for joins), filters,
groups, sorts and projects. Every scan of a table goes through `scan_each` / `scan_rows_flt` in `src/engine/txn.rs`, which applies the
locking and visibility rules.

## Adding things

### A built-in scalar function

1. Add an arm to the `match name` in `Engine::builtin` in `src/engine/funcs.rs`. The name is upper case. `need(name, &a, n)` checks the
   argument count; return `Value::Null` for a NULL argument unless the function says otherwise; errors are `SqlError::new(number, message)`
   with SQL Server's number and wording.
2. If it is deterministic and reads nothing but its arguments, add its name to the list of functions that are safe to run on worker threads
   in `src/engine/par.rs` (`par_safe`), or a query that uses it will not run in parallel. If it passes the collation of its first argument
   through (string functions), add it to `COLLATION_PASSING` in `src/engine/expr.rs`.
3. A function the optimizer must understand (it changes what an index can answer) is not a built-in's job: see `column_preds` and
   `choose_access` in `src/engine/optimizer.rs`.
4. Test it in `tests/functions.rs`, with the value SQL Server gives. Add it to the function list in the top-level README's *T-SQL coverage*.
5. In a cluster, a function is evaluated by the shards that run the statement, so nothing more is needed unless it depends on a session
   or a sequence (see `has_window_or_session` in `src/rewrite/astutil.rs`: such an expression keeps a statement from being split over shards).

### An aggregate

`is_agg_name` in `src/engine/query.rs` names them and the general executor computes them from a group's values (`agg_over`). To let the
streaming executor fold it one value at a time, add it to `foldable` and `AggAcc` in `src/engine/aggacc.rs`, including `merge` if the
parallel executor may combine partial states (it does so only for an argument that is certainly an exact number, `exact_number`: a float
sum depends on the order of its additions). For the cluster, `src/` of the control crate splits an aggregate into a part for each shard and
a part that combines them (`plan.rs`, `AggInfo`).

### A statement

1. `src/ast.rs`: a `Stmt` variant (and the structs it needs).
2. `src/parser.rs`: recognise it in `Parser::statement` (keywords are matched by `is_kw` / `peek_kw`); add a helper method for a
   statement with more than a few clauses. Add its leading word to `STMT_START` near the top of the parser if a statement must be recognised as starting a new
   statement where the previous one has no terminator.
3. `src/engine/mod.rs`: an arm in `exec_inner` that calls a function in the file for its kind. Statements that are allowed in a
   database that is being restored are listed at the top of `exec_inner`.
4. Permissions: if it needs one, check it where the object is resolved (see how `check_select_permission` and the DDL checks are called).
5. If it is DDL it may fire DDL triggers (`src/engine/ddltrig.rs` knows the event names), and a change it makes to a persistent
   database has to reach the write-ahead log (`src/persist.rs`, `WalRecord`): look at how a neighbouring statement does both.
6. The cluster: `ClusterSession::exec_stmt` in `crates/parallel/control/src/lib.rs` decides what the control node does with each statement.
   One it does not know is refused with error 50100 (*not supported by the distributed engine yet*); add an arm that routes it (to one
   shard, to all, or planned like a query) when it should work there.
7. Tests in the file for its area, and a line in the docs (the file for the subject; the top-level README if it widens what T-SQL is
   covered).

### A data type

`DataType` and `Value` in `src/types.rs` (the runtime value is serialised with serde into the log and snapshots, so adding a variant
changes the on-disk format: read [storage.md](storage.md) first); `src/convert.rs` for casts, comparison and arithmetic with SQL Server's
result-type rules; the parser's type names; `src/tds/wire.rs` for how the TDS protocol encodes and decodes it (and `src/tds/bulk.rs`,
`cursor.rs`); `src/tools.rs` and `src/gui/` for how it is shown; `crates/parallel/shared-ces/src/arrowconv.rs` for how a shard sends it to
the control node; `src/engine/bakimport.rs` and `src/sqlbak.rs` for how a restored SQL Server database stores it.

### A `sys.*` view or an `sp_*` procedure

A view is an arm in `Engine::sysview` (`src/engine/sysviews.rs`) that builds a `Rowset` from the catalog; give each column a type
(`rs.schema.cols[i].ty`) when a client will see it. A system procedure is an arm in `src/engine/sysprocs.rs`; it emits result sets with
`emit_rowset` and returns its status. SQL Server clients and tools call many of these, so match the column names and order exactly.

### An error

`SqlError::new(number, message)` takes SQL Server's number and message text; `SqlError::with(number, severity, state, message)` sets
the other two. Tests read errors by number (`err_no` in `tests/common/mod.rs`), and the number is what clients branch on: use the real one.

### A change to the optimizer

Plans must never change an answer. The rule of `tests/optimizer.rs` and `tests/rewrites.rs` is that a query gives the same rows with
`SET OPTIMIZER ON` and `OFF`; add your query shape to one of them, and assert the plan text for the choice you claim (`plan(...)`,
`c.last_plan()`). A new optimization is worth nothing until it is shown to engage (a statistics feature once never ran because of a scan
that found nothing, and only a test of the estimates showed it) and measured on a quiet machine, back to back with the binary before it
([performance.md](performance.md), *Measuring* and the sixth round). Read [optimizer.md](optimizer.md) before changing the optimizer, and
`src/engine/par.rs`'s header before changing what runs on worker threads.

### Storage and concurrency

These are the places where a small change breaks something far away. Read [storage.md](storage.md) and [concurrency.md](concurrency.md)
first; run the whole suite in the paged and spill modes; `tests/paging.rs`, `persistence.rs` (crash simulation), `isolation.rs` and
`parallel.rs` (real threads) are the ones that fail first. A change to the log record format needs a way to read the old records.

## The cluster

`crates/parallel/` is a workspace of four crates (see [distributed.md](distributed.md)):

| Crate | Where to change what |
| --- | --- |
| `control` (`tsql-control`, binary `tsql-cluster`) | `src/lib.rs`: `Cluster` and `ClusterSession`, how each statement is routed (`exec_stmt`), `run_query_flat` (the order in which ways of running a query are tried). `plan.rs`: a query over one table split into a part for each shard and a combining part. `joins.rs`, `mpp.rs`: joins over sharded tables (co-located, broadcast, repartitioned, in stages). `pushdown.rs`, `nested.rs`: subqueries. `gather.rs`: the fallback that copies tables to the control node. `txn.rs`: two-phase commit. `fk.rs`, `procs.rs`, `advisor.rs`, `rebalance.rs`, `autoscale.rs`, `front.rs` (the TDS front end) |
| `storage` (`tsql-storage`, binary `tsql-shard`) | a shard: the Arrow Flight service in `src/lib.rs`, the shard-to-shard row shuffle in `shuffle.rs`, replication in `replica.rs` |
| `raft` | the consensus core |
| `shared-ces` | what the nodes share: the wire messages (`wire.rs`), the key hash and slot function (`route.rs`), Arrow conversion, configuration |

A rule the cluster follows: **the shards run the real engine**. Anything a shard can do for itself should be sent to it as a statement, not
reimplemented in the control node; the control node's job is to decide where a statement runs and to combine the results. A new way of
running a query is a new `try_...` step in `run_query_flat` that returns `Ok(None)` when it does not apply, so the ways tried after it
still work, and the last one, the gather, is always exact.

Tests for the cluster are in `crates/parallel/control/tests/`. The `Pair` fixture in `cluster.rs` runs each statement on a cluster and on a
single-node oracle and compares the answers (`p.check(sql, ordered)` returns the plan lines: assert how the statement was run, for
example that it did not gather). Change a test that expects a gather only when the statement now runs better, and say so.

## Tests

* `tests/*.rs` are integration tests, one file per area (the list is in the top-level README under *Development*); `tests/common/mod.rs`
  has the helpers: `conn()` and `server()` (a fresh server in the current mode), `exec` (panics on an error), `rows` (a result as strings),
  `script` (batches separated by `GO`), `err_no`, `msgs`. Unit tests live beside the code in `src/`.
* Expected values encode what SQL Server really does. When the engine and SQL Server differ on purpose, the difference goes in the
  docs (the *Limits* or *Differences* section of the file for the subject) and the top-level README's *Behaviour notes*.
* A test that counts something process-wide (`parallel_batches`, an environment variable such as `TSQL_NO_SCAN_PRUNE`) takes a lock, so
  the tests of the file do not disturb each other.
* The benchmarks and the fuzzer (`examples/`, see [benchmarks.md](benchmarks.md)) are also tests: they compare answers (the cluster's with
  one node's, the optimizer's on and off) and found most of the hard bugs. Run the fuzzer after a change to the executor:
  `cargo run --release --example sqlsmith` (and, from `crates/parallel`, `cargo run --release -p tsql-control --example sqlsmith_cluster`
  for the cluster).

## Finding out what the engine is doing

| To see | Use |
| --- | --- |
| how a statement would run | the shell's `.plan SQL`; in Rust `Connection::last_plan()` after running it (a list of lines: the scans and how they were read, the join order and method, rewrites, parallelism, spills) |
| the plan with the optimizer out of the way | `SET OPTIMIZER OFF` (or `.optimizer off` in the shell), then compare |
| where the time goes in a statement | build with `--features prof` and read the span report (`cargo run --release --features prof --example tpc -- h --sf 0.1 --sql "..."` prints one); `crate::span!("name")` times the rest of a block, and costs nothing without the feature |
| how the cluster ran a statement | `ClusterSession::last_plan()`, or `TSQL_PLAN_TRACE=1` on the control node for one line per statement; `TSQL_GATHER_TRACE=1` for what a gather copied |
| the TDS conversation | `TSQL_TDS_TRACE=1` |
| a fresh parallel or spill configuration in a test | `TSQL_PARALLEL_FORCE_DOP`, `TSQL_PARALLEL_MIN_SLOTS`, `TSQL_SPILL_DIR` |

## Before a change is done

* The tests for it exist and pass, in the default mode and, for anything near storage or execution, in paged and spill (and the cluster
  tests for anything the control node sees).
* The documentation of the subject is updated in the same change, and `docs/README.md` if a file was added. The top-level README is
  updated when the change widens the T-SQL that is covered, changes how to run something, or changes a limit it lists.
* `cargo clippy` has nothing new to say.
* A performance change has numbers: before and after, from binaries run back to back, and a statement of what did not improve.
