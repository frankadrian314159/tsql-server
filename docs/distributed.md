# The distributed edition (cluster)

The single-node server in this repository's root crate runs the same engine on several machines in the distributed edition
(`crates/parallel/`): a **control node** that clients talk to, and **shard nodes** that each hold a slice of every table,
optionally replicated for availability. It is a Cargo workspace beside the root package (`cargo build` / `cargo test` at
the root still build only the single-node server; add `--workspace`, or `-p tsql-control` and so on, for the cluster).

Two small, general hooks were added to the single-node engine for it (both do nothing unless installed, see
[Engine hooks](#engine-hooks)); everything else is new code in the workspace.

## Parts

| Crate | Role |
| --- | --- |
| `tsql-storage` (binary `tsql-shard`) | A shard node: a complete single-node engine (storage, write-ahead log, locks, optimizer, nothing reimplemented) behind an **Arrow Flight** gRPC service; optionally one replica of a Raft group. |
| `tsql-control` (binary `tsql-cluster`) | The control node: routing, planning, merging, joins, transactions, elasticity, and a TDS front end. `tsql-cluster` is its shell and server. |
| `tsql-raft` | A Raft consensus core (election, log replication, commit, leader lease, file-backed log), a pure state machine tested under a simulated hostile network. |
| `tsql-shared-ces` | What the nodes share: configuration, wire messages, `ResultSet` to Arrow conversion, the key hash and slot function. |

A separate compute tier (`execute`, `shared-ce`, `shared-es` in the original plan) was not needed: shards do their own
computing, joins repartition through the control node, and no compute node was introduced.

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
answer with a single node.

## How a statement runs

* **DDL** (`CREATE`/`DROP TABLE`, indexes, `SET`) runs on the control node's own *mirror* engine first (which checks it and
  keeps the schema), then on every shard; a table that cannot be created everywhere is removed again.
* **Distribution.** A table is *sharded* by default, on the first column of its primary key (else its first column), by an
  integer, character or `uniqueidentifier` key. `DISTRIBUTE TABLE t BY column` picks another column and `DISTRIBUTE TABLE t
  REPLICATED` puts the same rows on every shard (small reference tables); both need an empty table. A key belongs to one of
  1024 hash *slots* and each slot to one shard (the slot map).
* **`INSERT`** is split by the key of each row; every shard gets its rows in one statement.
* **`UPDATE` / `DELETE`** of one table go to the shards that can hold the rows (one when the `WHERE` fixes the key); counts
  are added.
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
    table's shard key uses. The planner picks which tables stay put so as to move the fewest rows. An outer join is planned
    only when its preserved side is partitioned over the shards; full joins and joins on expressions are gathered;
  * *gather*: everything else (subqueries, CTEs, windows, `DISTINCT` aggregates, `ROLLUP`, set operations, ...): the tables
    are copied into the control node's mirror (kept until a change bumps their version) and the query runs there. This is the
    correctness baseline, always exact, running at single-node speed after a one-time copy.
* **Transactions.** `BEGIN TRANSACTION` begins on a shard when a statement first uses it, on that shard's session for this
  control session, so locks and uncommitted changes are held there and the session reads its own writes. `COMMIT`: one shard,
  committed there; several shards, **two-phase commit** with a persisted decision (see `txn.rs`). Isolation is the shard
  engines'. A statement that fails on several shards, or because a shard failed, dooms the transaction (error 3930; only
  `ROLLBACK` works), as the shards cannot be told apart from half done. Nested `BEGIN`/`COMMIT` count as in SQL Server.
  A transaction that waits for a lock fails after `set_lock_timeout_ms` (default 5 s, error 1222), which also breaks a deadlock
  between two shards, which neither shard's detector sees.
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

## Serving SQL Server clients

`tsql-cluster --listen PORT` serves the control node's own engine over TDS (logins, TLS options, prepared statements, MARS as in
`tsql-serve`). An interceptor on that engine sends the batches that touch distributed tables to the cluster, including
parameterized statements (`sp_executesql`, whose input parameters become session variables) and transactions, and declines the
rest (`SET`, `SELECT @@VERSION`, system views), which the engine runs itself. A batch that mentions a distributed table and is
not understood is an error, never an answer from the mirror's empty copy. Not supported through the front end: OUTPUT
parameters, server cursors, table-valued parameters and bulk load. Checked with tiberius (`tests/tds_front.rs`).

## Engine hooks

Added to the single-node crate, inert unless installed: **`Server::set_replicator`** (every commit proposes its redo record to a
`Replicator` under the commit lock, and waits for the group before completing) with **`Server::apply_replicated`** (a follower
applies a committed record, idempotently by commit timestamp); and **`Server::set_interceptor`** (an `Interceptor` per
connection may take over a batch; `Server::connect_internal` is a connection that is never intercepted). The single-node test
suite passes unchanged in all its modes.

## Checked against one node

`crates/parallel/control/tests/cluster.rs` runs every statement on a real cluster (control node and Flight shards over TCP) and
on one engine: scans, `TOP`, `DISTINCT`, hidden sort keys, every aggregate with `GROUP BY` and `HAVING`; co-located, broadcast,
repartition, left and right joins and the shapes that must gather; DML and the caches following it; transactions (commit across
shards, rollback, read-your-own-writes, isolation, a dead participant, a deadlock broken by the timeout); adding and removing a
shard under data; a control restart from its state file. `replication.rs` starts three durable replicas with Raft logs on disk and
kills the leader, restarts it, and removes the majority. `tests/simulation.rs` of `tsql-raft` runs 71 randomized schedules of
message loss, reordering, partitions and crashes, checking at every step that no term has two leaders and no two nodes commit
different entries at an index. `tds_front.rs` drives the control node with a TDS client. The `tpch_cluster` example does it for
the 22 TPC-H queries (with the small dimension tables replicated, orders and lineitem co-located, Q3, Q5, Q10, Q12, Q14 and
Q19 run over the shards and Q5 and Q19 beat one node on three shards in one process) and checks six answers against the
generated rows.

## Limits (an error says so where a statement is refused)

* **Two-phase commit has a window**: a participant that *loses its open transaction after it voted* (its process crashes, or a
  replicated shard fails over, between prepare and commit) takes its part of the transaction with it while the others commit; the
  commit reports error 50210 naming the shard. Closing it needs the participant's redo record made durable before it votes (it
  could ride the same replicated log) plus recovery of the in-doubt transaction's locks. A control node that stops after
  deciding to commit finishes the commit when it is opened on its state file (`resolve_pending`).
* **No global wait-for graph**: a cross-shard deadlock is broken by the lock timeout, not by detecting the cycle and choosing a
  victim (the engine's wait graph cannot yet be attributed to control sessions from outside).
* **Raft has no log compaction or snapshots** (the log grows; a far-behind replica is caught up from the log) and a fixed
  membership of replicas (changing a shard's replicas means a new shard and a move of its slots). A replicated shard's
  transactions in flight at a failover are lost (the client sees an error and retries).
* **Refused:** `IDENTITY` columns, foreign keys, `UNIQUE` constraints or unique indexes that do not contain the shard key,
  changing a shard key, views, procedures, triggers, `ALTER TABLE`, `CREATE DATABASE`/`USE`, temporal tables, DML with joins,
  subqueries, `OUTPUT` or `TOP`, savepoints, DDL inside a transaction.
* **Gather moves whole tables** (queries with subqueries or CTEs run at single-node speed on the control node), results are
  collected on a shard before they are sent, and the control node merges in memory.
* Elasticity moves slots one pair of shards at a time under a pause, and only the last shard can be removed.
