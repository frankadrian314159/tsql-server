# The distributed edition (cluster)

The single-node server in this repository's root crate runs the same engine on several machines in the distributed edition
(`crates/parallel/`): a **control node** that clients talk to, and **shard nodes** that each hold a slice of every table,
optionally replicated for availability. It is a Cargo workspace beside the root package (`cargo build` / `cargo test` at
the root still build only the single-node server; add `--workspace`, or `-p tsql-control` and so on, for the cluster).

A few small, general hooks were added to the single-node engine for it (they do nothing unless used, see
[Engine hooks](#engine-hooks)); everything else is new code in the workspace.

## Parts

| Crate | Role |
| --- | --- |
| `tsql-storage` (binary `tsql-shard`) | A shard node: a complete single-node engine (storage, write-ahead log, locks, optimizer, nothing reimplemented) behind an **Arrow Flight** gRPC service; optionally one replica of a Raft group. |
| `tsql-control` (binary `tsql-cluster`) | The control node: routing, planning, merging, joins, transactions, elasticity, and a TDS front end. `tsql-cluster` is its shell and server. |
| `tsql-raft` | A Raft consensus core (election, log replication, commit, leader lease, file-backed log), a pure state machine tested under a simulated hostile network. |
| `tsql-shared-ces` | What the nodes share: configuration, wire messages, `ResultSet` to Arrow conversion, the key hash and slot function. |

A separate compute tier (`execute`, `shared-ce`, `shared-es` in the original plan) was not needed: shards do their own
computing, joins shuffle rows from shard to shard, and no compute node was introduced.

## Running a cluster

```
# one shard, three replicas (each its own data directory; --id is the shard, --replica the member of its group)
tsql-shard --id 0 --listen 10.0.0.1:50051 --data /data/s0r1 --replica 1 --peers 2=10.0.0.2:50051,3=10.0.0.3:50051
tsql-shard --id 0 --listen 10.0.0.2:50051 --data /data/s0r2 --replica 2 --peers 1=10.0.0.1:50051,3=10.0.0.3:50051
tsql-shard --id 0 --listen 10.0.0.3:50051 --data /data/s0r3 --replica 3 --peers 1=10.0.0.1:50051,2=10.0.0.2:50051
# an unreplicated shard
tsql-shard --id 1 --listen 10.0.0.4:50051 [--data DIR]
# the cluster file: a shard's replicas are listed in one address, separated by commas
echo '{"shards":[{"id":0,"addr":"10.0.0.1:50051,10.0.0.2:50051,10.0.0.3:50051"},{"id":1,"addr":"10.0.0.4:50051"}]}' > cluster.json
tsql-cluster --config cluster.json --state cluster.state --listen 1433 --sa-password PW      # serve SQL Server clients
tsql-cluster --config cluster.json --state cluster.state --plan < script.sql               # or run a script (batches end at GO)
```

`--state` keeps the control node's state (shards, slot map, tables, schema, unfinished commits) in a file: a control node
started on an existing file picks the cluster up where it was and ignores `--config`. From Rust: `Cluster::connect(&config)` or
`Cluster::open(state_file, &config)`, then `cluster.session()` and `session.execute(sql)`; `session.last_plan()` says how each
statement ran. `cargo run --release -p tsql-control --example tpch_cluster` runs TPC-H on a cluster and compares every
answer with a single node; `--example tpcds_cluster` does the same for the 24 TPC-DS-style queries (and `oltp_cluster` runs TPC-C and the reduced TPC-E, `bench_cluster` the engine micro-benchmark and `sqlsmith_cluster` the fuzzer, each against one engine; see *Benchmarks on a cluster*) (dimensions replicated, fact
tables sharded on the item), and its table shows how each query ran (`split`, `set`, `correlated`, `join`, `scatter-gather`,
`gather`): none gathers.

## How a statement runs

* **DDL** (`CREATE`/`DROP TABLE`, `ALTER TABLE`, indexes, `SET`) runs on the control node's own *mirror* engine first (which
  checks it and keeps the schema), then on every shard; a table that cannot be created everywhere is removed again.
  `ALTER TABLE` may add, change (not the shard key or an identity column) and drop columns and constraints; adding a primary key
  or a `UNIQUE` constraint without the shard key is refused. **Views** are kept by the control node (the shards hold none): a
  query that reads a view is rewritten around its definition, as a derived table (see subqueries below); writes through a view
  are refused. **`IDENTITY` columns** are numbered by the control node (a counter per table in its state, so the numbers do not
  depend on the shard a row lands on, and survive a restart); the shards' tables have no identity of their own.
  `SET IDENTITY_INSERT`, `SCOPE_IDENTITY()` and `@@IDENTITY` work. **Foreign keys** are enforced by the control node, see
  [Foreign keys](#foreign-keys).
* **Distribution.** A table is *sharded* by default, on the first column of its primary key (else its first column), by an
  integer, character or `uniqueidentifier` key. `DISTRIBUTE TABLE t BY column` picks another column and `DISTRIBUTE TABLE t
  REPLICATED` puts the same rows on every shard (small reference tables); both need an empty table. A key belongs to one of
  1024 hash *slots* and each slot to one shard (the slot map).
* **`INSERT`** is split by the key of each row; every shard gets its rows in one statement.
* **`UPDATE` / `DELETE`** of one table go to the shards that can hold the rows (one when the `WHERE` fixes the key); counts
  are added. A condition may have subqueries that do not depend on the row (they are evaluated first, as for `SELECT`). An
  `UPDATE` that changes the shard key reads the new rows, deletes the old ones and inserts the new ones in one transaction.
* **`SELECT`**:
  * *route*: the `WHERE` fixes the key, so the whole query goes to one shard;
  * *scatter-gather*: a scan or aggregate over one sharded table. Each shard runs a shard query; the control node merges the
    results with its own engine (two-phase aggregation: partial `SUM`/`COUNT`/`MIN`/`MAX`, `AVG` as `SUM` and `COUNT`;
    `HAVING`, `ORDER BY`, `TOP`, `DISTINCT` and expressions over aggregates are applied by the merge), so collation and every
    expression behave as on one node;
  * *join over shards*: joins of base tables are run shard by shard when the rows can meet (`joins.rs`). **Co-located**: the
    sharded tables are joined on their shard keys. **Broadcast**: a small table (at most 100,000 rows, `set_broadcast_limit`)
    is copied to every shard as a session temporary table, with the conditions on it applied while copying; replicated tables
    need no copy. **Repartition** (shuffle): a table is re-spread over the shards by its join column with the hash the other
    table's shard key uses. The planner picks which tables stay put so as to move the fewest rows. A copy (broadcast or
    repartition) is made by the **shuffle**: the control node asks every shard to run the scan and send each row to the shard
    that needs it (action `shuffle`; the shards send the rows to each other with the action `load`, 5,000 rows to a message), so
    the rows never pass through the control node. An outer join is planned
    only when its preserved side is partitioned over the shards; full joins and joins on expressions are gathered;
  * *subqueries, CTEs, derived tables and views* (`nested.rs`): the parts that do not depend on the outer query are evaluated
    first, each by the cluster itself, and the outer query is rewritten around their results: a scalar subquery becomes its
    value, `EXISTS` a true or false condition, `x IN (SELECT ...)` a list (up to 20,000 values), and a CTE, derived table, view
    or table variable becomes a small *replicated* temporary table on every shard (up to the broadcast limit) that the rest of
    the query joins like any replicated table. The rewritten query then goes through the planning above. A correlated subquery
    (it does not bind alone), a recursive CTE, `ANY`/`ALL`, and a result too big to list or broadcast are not rewritten;
  * *correlated subqueries* (`decorrelate.rs`): the common shapes are rewritten as joins first, so that their parts do not
    depend on the outer rows. A scalar aggregate subquery in a comparison of the `WHERE`
    (`x > (SELECT AVG(y) FROM t WHERE t.k = o.k)`, also under arithmetic) becomes a join with a grouped derived table (a row
    with no match gets NULL, which fails the comparison, as the join dropping it does; `COUNT` is left alone, it gives 0 for no
    rows). `EXISTS (... WHERE t.k = o.k ..)` becomes `o.k IN (SELECT t.k ..)`, and `NOT EXISTS` the same with `NOT IN`, written
    so that a NULL key counts as no match. Only equalities between a column of the subquery and a qualified column of the outer
    query are recognised; a rewrite that does not bind is undone and the statement runs as it was;
  * *set operations* (`split.rs`): `UNION`, `INTERSECT` and `EXCEPT` (also nested, with `ALL`, `TOP` in a branch, `ORDER BY`
    over the result) run each branch as a query of its own, over the shards, and combine the answers on the control node;
  * *windows, `ROLLUP`/`CUBE`/`GROUPING SETS` and `DISTINCT` aggregates* (`plan_split`, `split.rs`): an *inner* query returns
    the finest grain (the group expressions of all grouping sets, plus the distinct argument, with partial aggregates; or the
    columns a windowed row query reads) and is run over the shards like any query; the control node's engine then runs the
    *outer* query over its answer: the grouping sets re-aggregate the partials, a `COUNT(DISTINCT x)` counts the `x` column,
    and windows (also over aggregates, `SUM(SUM(x)) OVER (..)`) see the aggregated rows. Only one distinct expression per
    query is supported (more gather);
  * *gather*: everything else (a correlated subquery that is not an equality, `STDEV` and the like, more than one distinct
    expression, recursive CTEs, ...): the tables are copied into the control node's mirror (kept until a change
    bumps their version) and the query runs there. This is the correctness baseline, always exact, running at single-node
    speed after a one-time copy.
* **Transactions.** `BEGIN TRANSACTION` begins on a shard when a statement first uses it, on that shard's session for this
  control session, so locks and uncommitted changes are held there and the session reads its own writes. `COMMIT`: one shard,
  committed there; several shards, **two-phase commit** (see `txn.rs`): every participant makes its part durable (a `Prepare`
  record in its log, or its Raft group's log) and keeps the transaction open, the decision is persisted in the control state,
  then each participant commits, from its open transaction or, if it restarted since it voted, from its durable record.
  Isolation is the shard engines'. A statement that fails on several shards, or because a shard failed, dooms the transaction (error 3930; only
  `ROLLBACK` works), as the shards cannot be told apart from half done. Nested `BEGIN`/`COMMIT` count as in SQL Server. `SAVE TRANSACTION name` makes a savepoint on the shards that have begun the
  transaction; `ROLLBACK TRANSACTION name` takes them back to it, undoes everything on a shard that joined later, and clears a
  doomed state.
  A transaction that waits for a lock fails after `set_lock_timeout_ms` (default 5 s, error 1222). A **deadlock across shards**
  (A waits for B on one shard, B for A on another) is found by the control node: every 200 ms (`set_deadlock_check_ms`) it asks
  each shard who waits for whom, joins the graphs, and a cycle seen twice in a row is broken by killing the youngest member's
  wait; that session gets error 1205 and its transaction is rolled back on every shard, as SQL Server does (`deadlock.rs`).
* **Control plane and data plane.** Statements, errors and row counts are Flight *actions* and *tickets* whose bodies are the
  engine's syntax trees in postcard (the control node sends a rewritten query without a SQL printer); result sets are Arrow
  record batches (native columns for common kinds; decimals, dates, times and GUIDs travel as exact packed values).

## Elasticity

`Cluster::add_shard(addr)` gives a new (empty) shard the schema and the replicated tables, then moves slots to it until the
shards own about the same number; for each sharded table the rows of the moving slots are copied, the slot map switched,
and the old copies deleted. `remove_last_shard()` does the reverse. `rebalance()` evens the slots after a partial change. A
move holds a migration lock for writing that statements hold for reading, so it is a pause for statements, not a risk to
data; a failed copy is undone. `telemetry()` reports each shard's slots, sessions, lock waits, log forces and rows, and
`advise(&telemetry, &ScalePolicy)` says when to add a shard (too many rows per shard), remove one (too few), rebalance, or
replace an unreachable shard. Starting and stopping machines is the orchestrator's job; the cluster gives it these calls.

## Replication

A shard can be a group of replicas (one `--id`, several `--replica`s). They elect a leader with Raft (`tsql-raft`); only the
leader serves requests, once a majority has recently answered it (a lease) and it has applied everything the group committed;
any other replica answers with the leader's address and the control node follows it. The unit of replication is the engine's own
redo record, what the write-ahead log would hold for the transaction: a transaction runs on the leader's engine under its usual
locks and isolation; at commit its record goes into the group's log, and the commit completes (versions stamped, locks
released) when a majority has it; followers apply the records to their engines in order. Nothing is re-executed, so replicas
cannot diverge through non-deterministic functions or lock timing. A replica that was down catches up from its Raft log when
it returns; without a majority nothing is served and nothing is lost (checked by tests that kill the leader, restart it, and
take two of three replicas away).

**Snapshots and log cutting.** After `--snapshot-every` applied entries (default 5000) a replica forces its engine's log to disk,
exports the engine's whole state (`Server::export_state_at`: every database, its tables' rows, security, prepared transactions)
on a helper thread, and Raft drops the log up to that entry. A replica that has fallen behind the start of the leader's log (it
was down a long time, or is new) is sent the leader's snapshot in chunks and installs it into its engine (`install_state`, which
also writes it to the engine's log so a restart finds it). The snapshot is taken while statements run, so it may hold changes of
commits just after its index; the log's records set rows and objects to absolute values, so applying those entries again is
harmless. A snapshot is held in memory, so a very large shard needs the memory of its data for the moment of the export.

**Changing the replicas.** Start the new replica on an empty directory with `tsql-shard --id N --replica R --join --peers
...` (`--addr` if its address differs from `--listen`), then `Cluster::add_replica(shard, R, addr)`: the leader adds it as a
non-voting *learner*, sends it the snapshot and log, and when it has caught up makes it a voter (one voter changes at a time,
as in the Raft dissertation); `Cluster::remove_replica(shard, addr)` takes one out, the leader included, and the control node
and its state file follow (`replicas.rs`). A removed replica that keeps running cannot depose the group: a vote is refused
while a leader has been heard from recently, and from a node that is not a voter.

## Benchmarks on a cluster

The drivers of the single-node examples run against a cluster too, through a cluster session (`util::Sql` is implemented for
`Connection` and for `ClusterSession`); with `--config cluster.json` the control node connects to shard nodes that are already
running (`tsql-shard` on other machines), otherwise `--shards N` starts them in the process.

* `oltp_cluster [c|e]`: TPC-C and the reduced TPC-E. A cluster has no stored procedures or sequences, so the transactions are
  **client-side**: the terminal sends `BEGIN`, each statement and `COMMIT` itself (`c.rs` / `e.rs` with `client = true`; the single-node
  `tpc_oltp --client` runs the same code on one engine, and without `--client` it still calls the procedures). TPC-C is sharded on
  the warehouse (the item table is replicated), TPC-E on each table's first key column (`security` replicated); transactions that
  touch two shards (history rows, remote warehouses) commit by two-phase commit; a deadlock victim is retried. The consistency
  conditions are checked before and after the run. `tests/oltp.rs` runs both for a moment.
* `bench_cluster`: the micro-benchmark's workloads (loops, procedures and `sp_executesql` left out) on one engine and the cluster,
  with both times, the plan and whether the answers agree, and concurrent sessions of the cluster.
* `sqlsmith_cluster`: the fuzzer's random queries on one engine and the cluster; answers, error numbers and the effect of data changes
  must agree (error 50100, a statement the cluster refuses on purpose, is counted). It found three bugs now fixed: a rewritten query
  that had to gather lost its temporary tables, a grand total over nothing gave NULL for `COUNT`, and a table joined with itself was
  routed to one shard when a key condition named one of its aliases.

A join whose tables' keys are all fixed by `key = literal` conditions to the same shard (TPC-C Stock-Level) runs on that shard alone.

## Foreign keys

The shards cannot see each other's rows, so they are not given foreign keys; the control node keeps the definitions (in its
state) and checks every statement that could break one before sending it (`fk.rs`): an insert into a child table, or an update
of its key columns, needs the keys to exist in the parent (compared by the control node's engine, so collation applies); a
delete from a parent table, or an update of the columns a key refers to, looks for child rows that refer to the rows concerned:
`NO ACTION` makes that error 547, `ON DELETE CASCADE` deletes them (their own children in turn) and `ON DELETE SET NULL` sets
their key columns to null; `TRUNCATE` or `DROP` of a table that is referred to is refused. A key to the same table works. The
checks run inside the statement's transaction, so a transaction sees its own changes, but **nothing locks the rows the checks
read across shards**: two sessions at the same moment can still produce an orphan (a child inserted while its parent is
deleted). `ON UPDATE` actions other than `NO ACTION` and `SET DEFAULT` are not supported.

## Serving SQL Server clients

`tsql-cluster --listen PORT` serves the control node's own engine over TDS (logins, TLS options, prepared statements, MARS as in
`tsql-serve`). An interceptor on that engine sends the batches that touch distributed tables to the cluster, including
parameterized statements (`sp_executesql`, whose input parameters become session variables) and transactions, and declines the
rest (`SET`, `SELECT @@VERSION`, system views), which the engine runs itself. A batch that mentions a distributed table and is
not understood is an error, never an answer from the mirror's empty copy. Also supported: `OUTPUT` parameters of
`sp_executesql` (their values come back to the variables they were passed as; `SELECT @v = ...` and `SET @v = (SELECT ...)`
read the cluster), **table-valued parameters** and table variables (a query that reads one has its rows sent to the shards as
a temporary table), **server cursors** (static and keyset, updatable ones included: they run their statements through the
same interception) and **bulk load** (`INSERT BULK`: the rows are split over the shards like an `INSERT`, in a transaction
that a failed load rolls back). Checked with tiberius and a raw protocol client (`tests/tds_front.rs`, `tests/tds_rpc.rs`).

## Engine hooks

Added to the single-node crate, inert unless installed: **`Server::set_replicator`** (every commit proposes its redo record to a
`Replicator` under the commit lock, and waits for the group before completing) with **`Server::apply_replicated`** (a follower
applies a committed record, idempotently by commit timestamp); and **`Connection::prepare_transaction`** with **`Server::in_doubt`** / **`Server::resolve_prepared`** (two-phase commit: a
transaction makes its changes durable as a `Prepare` log record while it stays open; `COMMIT`/`ROLLBACK` settle it, and one
whose process died is settled by id, see [storage.md](storage.md)); and **`Server::set_interceptor`** (an `Interceptor` per
connection may take over a batch; `Server::connect_internal` is a connection that is never intercepted). The single-node test
suite passes unchanged in all its modes.

## Checked against one node

`crates/parallel/control/tests/cluster.rs` runs every statement on a real cluster (control node and Flight shards over TCP) and
on one engine: scans, `TOP`, `DISTINCT`, hidden sort keys, every aggregate with `GROUP BY` and `HAVING`; windows, grouping
sets, distinct aggregates, set operations and correlated subqueries run over the shards and not gathered; co-located, broadcast,
repartition, left and right joins and the shapes that must gather; DML and the caches following it; transactions (commit across
shards, rollback, read-your-own-writes, isolation, a dead participant, a deadlock broken by the timeout); `twophase.rs`
crashes shards between their vote and the commit and stops the control node before and after it decides; adding and removing a
shard under data; a control restart from its state file. `replication.rs` starts three durable replicas with Raft logs on disk and
kills the leader, restarts it, and removes the majority. `tests/simulation.rs` of `tsql-raft` runs 71 randomized schedules of
message loss, reordering, partitions and crashes, checking at every step that no term has two leaders and no two nodes commit
different entries at an index. `tds_front.rs` drives the control node with a TDS client. The `tpch_cluster` example does it for
the 22 TPC-H queries (with the small dimension tables replicated, orders and lineitem co-located, Q3, Q5, Q10, Q12, Q14 and
Q19 run over the shards and Q5 and Q19 beat one node on three shards in one process) and checks six answers against the
generated rows.

## Limits (an error says so where a statement is refused)

* **Two-phase commit and the state file**: a vote is durable on the shard (it survives a crash, a restart and, on a replicated
  shard, the loss of the leader), and a control node that stops after deciding finishes the commit when it is opened on its
  state file. The state file is the coordinator's one point of failure: a control node that loses it cannot say how its
  transactions were decided (the shards would roll them back). A prepared transaction that a shard holds in doubt keeps the
  shard from serving statements, and its log from being checkpointed, until it is settled; a participant that cannot be
  reached after the decision keeps its part (and refuses statements) until the control node reaches it again.
* **Deadlock detection is periodic** (every 200 ms, a cycle must be seen twice), the victim is the youngest session rather than
  the cheapest to roll back, and a wait that is not a cycle falls back on the lock timeout.
* **Snapshots are in memory and fuzzy** (see Replication); a replicated shard's transactions in flight at a failover are lost
  (the client sees an error and retries). Membership changes go through the shard's leader one voter at a time.
* **Refused** (error 50100 says so): `UNIQUE` constraints or unique indexes that do not contain the shard key, stored
  procedures, functions and triggers (they would need an interpreter next to the data), `CREATE DATABASE`/`USE`, temporal
  tables, DML with joins (`UPDATE ... FROM`), `OUTPUT` or `TOP` in DML, `INSERT ... DEFAULT VALUES`/`EXEC`, correlated
  subqueries in DML, and schema changes inside a transaction. Views cannot be written through.
* **Foreign keys and identity values are the control node's**: the checks are not atomic across sessions (see Foreign keys),
  and a control node that loses its state file loses the identity counters and the foreign keys with it.
* **Gather moves whole tables** (queries with a correlated subquery that is not an equality, `STDEV`, recursive CTEs and the
  like run at single-node speed on the control node), a CTE, view or derived table bigger than the broadcast limit makes the
  whole query gather, results are collected on a shard before they are sent, and the control node merges in memory. The outer
  query of a split runs on the control node: a window over a very large aggregated result, or the rollup of a very fine grain,
  is as big as that result. A join of three tables that need different shuffles is still gathered.
* Elasticity moves slots one pair of shards at a time under a pause, and only the last shard can be removed.
