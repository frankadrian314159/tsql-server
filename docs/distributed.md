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
  1024 hash *slots* and each slot to one shard (the slot map). **`ADVISE DISTRIBUTION`** (`advisor.rs`) says which tables to
  distribute differently: the control node counts, per table, the times it was copied to every shard (broadcast), re-spread by a
  column (repartition) or read into the control node (gather) for a query, and the statement lists them with the advice that would
  have spared the move: `DISTRIBUTE TABLE t REPLICATED` for a small table broadcast three times or more, `DISTRIBUTE TABLE t BY c` for a
  table re-spread by `c` (or compared with other tables on `c` in a gathered query) more often than by its key. The advice is not
  carried out (a table that has rows cannot be re-distributed in place: create it again, distribute it, load it), and nothing about how a
  query runs depends on it. `ADVISE DISTRIBUTION RESET` clears the counts.
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
  * *an outer join from a replicated table, aggregated* (`outer_agg_ok` in `joins.rs`): `customer LEFT JOIN orders ... COUNT(o_orderkey)
    GROUP BY c_custkey` (TPC-H Q13) puts the preserved rows that find no match on a shard in that shard's result, once per shard. That
    is harmless when every aggregate is `COUNT`, `SUM`, `MIN`, `MAX` or `AVG` of an expression that is NULL for a missing row
    (columns of the sharded side, literals, arithmetic and casts), without `DISTINCT`: the partial counts add up, and a missing
    row adds 0. The grouping must be by preserved-side columns, and a `WHERE` condition on the sharded side must be one a missing row
    cannot satisfy (a comparison, `BETWEEN`, `IN`, `LIKE`). `COUNT(*)`, `SUM(1)`, an aggregate over `CASE` or `ISNULL`, grouping by the
    sharded side and `IS NULL` anti-joins are gathered (the fuzzer found `SUM(1)` counting the unmatched row on every shard);
  * *staged joins* (`mpp.rs`): a join whose tables are sharded on different keys (TPC-H Q9: `lineitem` and `orders` by the order key,
    `partsupp` by the part key) is done in stages that move rows from shard to shard. The *group* of tables joined so far starts as
    the largest sharded table, partitioned by its shard key; the next table is added where it is when the shards can do it alone
    (replicated, co-located on the group's partition column, or small enough to broadcast), otherwise a *stage* runs a scan on
    every shard and sends each row to the shard that owns the hash of the next join column (the `shuffle` action, shard to shard,
    into a session temporary table), either the group (re-spread by the column that joins the next table) or the next table
    (re-spread by its join column), whichever moves fewer rows. When all tables are in, the query (its select list, `GROUP BY`
    and so on rewritten over the temporary tables) runs shard by shard as an ordinary scatter-gather with two-phase
    aggregation, and only the groups reach the control node. Inner and cross joins of base tables only; the conditions of the
    `WHERE` that mention one table are applied in the scan that moves it;
  * *a common table expression too big to broadcast*, joined to itself (TPC-DS Q11's `year_total`), is spread over the shards by the
    column it is joined on, each row to the shard that owns the hash of that column, exactly as a table sharded on it would be, and
    registered as such: the self-joins are then co-located and run shard by shard. Without a column to spread by it is gathered;
  * *derived tables merged* (`flatten.rs`): `SELECT ... FROM (SELECT ... FROM a, b, c WHERE ...) d GROUP BY ...`, where the derived
    table is a plain select-project-join of several tables (no `DISTINCT`, `TOP`, grouping, aggregate, window, set operation or
    subquery), is one query: its tables and conditions join the outer ones and each reference to one of its columns becomes
    the expression it is. Without it the join's whole result (rows, not groups) would be made at the control node first. A derived
    table over one table is left alone;
  * *subqueries the shards answer themselves* (`pushdown.rs`): a subquery in the `WHERE` of a query over one sharded table is left
    where it is, and the whole query runs shard by shard, when each shard can answer it from its own rows: it reads only replicated
    tables (this is also what a long `IN (SELECT ...)` becomes: a result over `set_in_list_limit` rows, 20,000 by default, is made a
    replicated temporary table, up to the broadcast limit, and the shards run `x IN (SELECT k FROM that table)`), or it is
    *co-located* with the outer table: it reads one sharded table and an equality in its `WHERE` ties that table's shard key to the
    outer table's (`EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND ...)` from `orders`, both sharded on the order
    key; rows that agree on the key are on one shard, so anything else the subquery asks of them can be asked there). The outer
    query may join several tables when its sharded ones are co-located (TPC-H Q4 is this case; before it was gathered, 18 times
    slower than one node). When the outer query also reads tables that are not co-located with those (TPC-H Q21: `supplier` and `nation` next to
    `lineitem` and `orders`), the join planner copies them to every shard first (broadcast only, never a re-spread: the copy is a replicated
    table for the check), and the query then runs shard by shard with the subqueries inside, when the shards can answer them against that. A
    subquery may join several sharded tables (TPC-C's consistency checks 10 and 12: `orders JOIN order_line` under `customer`) when each of
    them is tied, by an equality of shard keys in its `WHERE` or `ON`, to another of them or to the outer query's table, all in one chain: the
    rows that belong to an outer row then lie on its shard. A subquery that reads a table sharded on another key (`history`, by the customer's
    id, next to `customer` by its warehouse) has that table copied to every shard when it is small (`set_broadcast_limit`), indexed on the
    columns the subquery compares, and reads the copy;
  * *correlated subqueries* (`decorrelate.rs`, `decorrelate`; the single-node engine also uses the outer-join rewrites of `decorrelate_all`,
    which the control node does not): the common shapes are rewritten as joins first, so that their parts do not
    depend on the outer rows. A scalar aggregate subquery in a comparison of the `WHERE`
    (`x > (SELECT AVG(y) FROM t WHERE t.k = o.k)`, also under arithmetic) becomes a join with a grouped derived table (a row
    with no match gets NULL, which fails the comparison, as the join dropping it does; `COUNT` is left alone, it gives 0 for no
    rows). `EXISTS (... WHERE t.k = o.k ..)` becomes `o.k IN (SELECT t.k ..)`, and `NOT EXISTS` the same with `NOT IN`, written
    so that a NULL key counts as no match. Only equalities between a column of the subquery and a qualified column of the outer
    query are recognised (an unqualified column is placed by the tables' schemas); a rewrite that does not bind is undone and the
    statement runs as it was. The grouped table is *restricted to the keys the outer query can meet*: the conditions of the outer
    `WHERE` that mention the outer table of the key alone become `inner_key IN (SELECT DISTINCT outer_key FROM that_table WHERE ...)`
    inside it (`NOT EXISTS` gets the same restriction: TPC-H Q22 only looks at the orders of customers in its country codes). A literal
    `IN` list of at least `set_in_join_min` values (100 by default) at the top level of a `WHERE`, against a plain column, is made a
    replicated temporary table of its distinct values and *joined*: `x IN (...)` an inner join, `x NOT IN (...)` a left join that keeps
    the unmatched rows (a NULL probe dropped, as NOT IN does; a list holding a NULL, strings a case-insensitive collation could call equal,
    a bare `SELECT *` and a multi-table `FROM` for NOT IN are left as they are), so the shards probe a table where they would
    compare every row with every item (measured: 8,000 values 4.7 s as a list, 0.1 s as a join; NOT IN 23 s against 0.18 s). Each engine
    also tests a long list of integer literals through a hash set (one lookup per row), which covers lists the cluster does not rewrite.
    (TPC-H Q17 wants the average quantity of about twenty parts, not of all of them; this took it from 4 times slower than
    one node to faster than it);
  * *set operations* (`split.rs`): `UNION`, `INTERSECT` and `EXCEPT` (also nested, with `ALL`, `TOP` in a branch, `ORDER BY`
    over the result) run each branch as a query of its own, over the shards, and combine the answers on the control node;
  * *windows, `ROLLUP`/`CUBE`/`GROUPING SETS` and `DISTINCT` aggregates* (`plan_split`, `split.rs`): an *inner* query returns
    the finest grain (the group expressions of all grouping sets, plus the distinct argument, with partial aggregates; or the
    columns a windowed row query reads) and is run over the shards like any query; the control node's engine then runs the
    *outer* query over its answer: the grouping sets re-aggregate the partials, a `COUNT(DISTINCT x)` counts the `x` column,
    and windows (also over aggregates, `SUM(SUM(x)) OVER (..)`) see the aggregated rows. Only one distinct expression per
    query is supported (more gather);
  * *gather*: everything else (a correlated subquery that is not an equality, `STDEV` and the like, recursive CTEs, outer joins that
    the join planner cannot place, ...): the tables are copied into the control node's mirror and the query runs there. This is the
    correctness baseline, always exact, running at single-node speed. A table of at most the broadcast limit is copied whole and
    kept until a change bumps its version; a bigger one is copied for this query only, with just the columns the query mentions
    and the rows that pass the `WHERE` conditions that mention that table alone (when the query is a plain inner join), every
    shard answering its part at once. The rows go into the control node's engine through `Connection::load_rows`, which appends them as
    committed rows without the machinery of an `INSERT` (no transaction, row locks, undo, constraint or key checks; each value is
    still coerced to its column's type): about 2.5 times faster than inserting (about 430,000 rows a second into a heap table and
    290,000 into one with a primary key, against 190,000 and 100,000). A durable server, a session in a transaction, and a table with
    an identity column, a foreign key or an INSTEAD OF trigger take the ordinary `INSERT`. The shards load shuffled rows the same way.
    A per-query copy is made an index on the columns the query equates (`a.x = b.y`, in table order), built after the load, and is a
    table of the mirror under a name of its own (`dpg_...`) that is dropped afterwards, not a `#temporary` table: the engine does not
    seek into the indexes of a temporary table from a correlated subquery, and without the index TPC-C's consistency condition 10 (a
    correlated subquery joining `orders` and `order_line`) scanned the 180,000-row copy once per customer and did not finish.
  * *global secondary indexes* (`DISTRIBUTE TABLE t INDEX BY col`, on an empty sharded table): a lookup by a column that is not the shard
    key (`col = literal`, `col IN (literals)`, in a SELECT, UPDATE or DELETE, or in a co-located join when no other table of the query has a
    column of that name) asks only the shards that can hold such rows. The index is a table of its own, `gsi_<table>_<col>(gv, gk)`, sharded
    by the value and indexed on it; the control node reads it (one shard) and sends the query to the shards that own the keys it lists
    (plan line `global index on col: shards [..]`). It only grows: every inserted row gets its entry in the same transaction (one of its own
    when the session has none); a deleted or moved row leaves its entry, which costs a visit to a shard with no such row and never a wrong
    answer. It prunes only when the value is selective: the trades of one account are spread over every shard when the table is sharded
    by trade id, so an index on the account cannot help there (shard the table by account instead), while an index on the trade id
    finds one shard. Dropping the table drops its index tables. An UPDATE that sets the indexed column is refused (the entry for the
    new value would be missing): delete and insert the row instead, or do not index a column that is updated.
  * *a join whose sharded tables are all fixed to one shard* (by literals on their keys, or by equalities between their keys, in `WHERE`
    or the `ON` of inner joins) runs on that shard as a whole.
  * *procedures that run on one shard* (`CREATE PROCEDURE` is made on every shard; `DISTRIBUTE PROCEDURE name BY @param` declares that
    the parameter's value is the shard key of everything the procedure reads and writes): a call `EXEC name ...` is one request to the
    shard that owns that value, inside the session's transaction when there is one (so a transaction can call several, on several
    shards, and commit them together by two-phase commit). The declaration is *checked* against the body and refused (error 50104,
    naming the statement) unless every statement is of a kind the check understands (declarations, `SET`, `IF`, `WHILE`, blocks,
    `SELECT`, `INSERT ... VALUES`, `UPDATE`, `DELETE`, transaction control, `RETURN`, `PRINT`), no expression has a subquery, every
    sharded table a statement touches is fixed to the parameter by an equality on its shard key (the test that pins a join to a
    shard; a derived table in a `FROM` is a query of its own that has to be fixed too), every `INSERT` gives the key the parameter, a replicated table is only read, the parameter is never assigned, and no
    table written has an IDENTITY column, a foreign key or a global index (the control node does those, and a shard would skip
    them). A call returns every result set of the procedure, in order (none when it ends before returning one); OUTPUT parameters and
    return values are not carried. Changing the
    procedure drops the declaration.
  * *joins are planned without counting rows when they are co-located*: the planner first tries every table as too big to move, and
    only a join that then has to move something counts the tables (a round trip to every shard each time a table has changed).
    A scatter-gather that only orders and takes `TOP n` is merged in the control node itself (no `DISTINCT`, percent or ties, a literal
    count, sort keys that are not character strings, whose order the engine takes from the collation); the rest goes through the
    control node's engine.
  * *plain point statements* (`SELECT ... FROM t WHERE ...` with no subquery, view, CTE, grouping or long literal list, on a replicated
    table or one whose key the condition fixes) are routed without walking the query in its serialized form: that took a routed
    SELECT from 0.78 ms to 0.59 ms (a bare round trip to a shard is 0.33 ms, a statement through the shard client 0.5 ms).
    `TSQL_PLAN_TRACE=1` prints one line per statement with how the cluster ran it.
* **Transactions.** `BEGIN TRANSACTION` begins on a shard when a statement first uses it (the `BEGIN` travels in the same request as that
  statement), on that shard's session for this
  control session, so locks and uncommitted changes are held there and the session reads its own writes. `COMMIT`: one shard,
  committed there; several shards, **two-phase commit** (see `txn.rs`): every participant makes its part durable (a `Prepare`
  record in its log, or its Raft group's log) and keeps the transaction open, the decision is persisted in the control state,
  then each participant commits (all at once), from its open transaction or, if it restarted since it voted, from its durable record. When only one participant
  changed anything there is nothing to decide between shards, and the decision is not written (a control node that stops after that vote leaves a prepared
  transaction, which rolls back, as nothing has committed anywhere).
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

## Operating a cluster

**Starting.** Shards and the control node may be started together. `tsql-cluster --listen` waits up to `--wait-shards` seconds
(default 30) for every shard to answer, then serves; a shard that restarted with prepared transactions refuses statements until the
control node has settled them (see Transactions), which the control node does by itself. A durable shard (`--data DIR`) recovers
from its write-ahead log; one that stopped gracefully has a checkpoint and starts at once.

**Stopping.** `tsql-shard` stops gracefully on SIGTERM (what `systemctl stop` and a Compute Engine shutdown send) and on Ctrl-C:
it *drains* (`NodeControl::drain`). It refuses new work (error 50240, "shutting down") except from sessions that already have a
transaction open, which may go on and commit; it waits up to `--drain-secs` (default 15) for them; it rolls back what is still open
after that; it forces the log and writes a checkpoint; then it closes its listener and, for a replica, stops its Raft loop (the
others elect a new leader; there is no hand-over of leadership). A *prepared* two-phase-commit transaction is never rolled back by
a stop: it is durable, the shard keeps it in doubt after the restart, and its coordinator settles it. `tsql-cluster` stops on the
same signals: it stops serving, which closes the client sessions (their open transactions are rolled back on the shards), and writes
its state file; transactions decided but not yet confirmed everywhere stay in that file and are finished at the next start. The
shard prints what the drain did (`open`, `finished`, `rolled back`, `prepared left`, `checkpoint`). The systemd units in
`deploy/gcp/` set `KillSignal=SIGTERM` and a stop timeout longer than the drain.

**Metrics.** Every shard samples its machine once a second (`tsql_shared_ces::metrics`): processor use of the machine and of the
process, memory used (percent of the machine) and the process's resident memory, the process's disk reads and writes per second,
and the bytes received and sent per second on all network interfaces; plus the engine's counters: sessions, statements in flight and
run, lock waits and the time waited, log forces, whether it is draining. The control node reads them with the action `metrics`
(`Cluster::metrics`, also in `Cluster::telemetry`) and samples itself. `--metrics-listen HOST:PORT` on `tsql-shard` serves its own
numbers, and on `tsql-cluster` serves those of the control node and every shard with `tsql_shard_up` and `tsql_slots`, at `/metrics`
in the Prometheus text format (Google Cloud's Ops Agent and Managed Service for Prometheus read it) and `/health`. Where the bottleneck
is: the busiest resource of a shard against its limit (the `Bottleneck` advice below); lock waits and log forces say whether the
time goes to contention or to commits rather than to the machine.

**Autoscaling** (`autoscale.rs`, `tsql-cluster --autoscale`). The thresholds are those of a `ScalePolicy`:

| setting | default | meaning |
|---|---|---|
| `cpu_high_pct` | 75 | add a shard when the machines' processors average above this |
| `cpu_low_pct` | 20 | a shard may be removed when they average below this |
| `mem_high_pct` | 85 | add a shard when memory in use averages above this (percent of the machine) |
| `disk_high_bps` | 150 MB/s | add a shard when the processes' disk reads and writes average above this |
| `net_high_bps` | 500 MB/s | add a shard when the machines' network receive and send average above this |
| `sustain` | 6 readings | the change must be advised by each of this many readings in a row (a burst adds nothing) |
| `cooldown_secs` | 300 | wait this long after a change (moving data loads the machines) |
| `min_shards`, `max_shards` | 1, 64 | the bounds |
| `max_rows_per_shard`, `min_rows_per_shard` | 50 M, 1 M | the row thresholds (a grow or shrink on data volume) |
| `max_slot_imbalance` | 1.1 | rebalance when a shard owns more than this times the average slots |

The averages are over the shards; one shard above a limit is reported as a `Bottleneck` (with its resource, value and limit) and
does not by itself add a machine, since another shard does not help a hot key. A shard is removed only when the rows or the
processors say so *and* what remains would stay under 90% of every limit with the load of the one removed. A shard that does not
answer stops all changes. The cluster cannot make machines, so a `Provisioner` does: `--scale-up-cmd` starts a shard node and prints
its `host:port` last, `--scale-down-cmd` deletes the node at `$TSQL_SHARD_ADDR` after its data has been moved off (see
`deploy/gcp/scale-up.sh` and `scale-down.sh`, which use `gcloud`; the delete reaches the shard as SIGTERM, a graceful stop). The flags
are `--cpu-high --cpu-low --mem-high --disk-high-mb --net-high-mb --sustain --scale-interval --cooldown --min-shards --max-shards`.
Limits: shards are added at the end and only the last one can be removed; the shards it adds are single nodes (a replica group is
made by hand, see Replication); the numbers are starting points, to be set from a run of the benchmark on the machine type in use.

## Benchmarks on a cluster

The drivers of the single-node examples run against a cluster too, through a cluster session (`util::Sql` is implemented for
`Connection` and for `ClusterSession`); with `--config cluster.json` the control node connects to shard nodes that are already
running (`tsql-shard` on other machines), otherwise `--shards N` starts them in the process.

* `oltp_cluster [c|e]`: TPC-C and the reduced TPC-E. The drivers use no cluster procedures or sequences, so the transactions are
  **client-side**: the terminal sends `BEGIN`, each statement and `COMMIT` itself (`c.rs` / `e.rs` with `client = true`; the single-node
  `tpc_oltp --client` runs the same code on one engine, and without `--client` it still calls the procedures). The cluster is used as it
  is, with no tuning of the schema or the statements for it: TPC-C is sharded on the warehouse (the item table is replicated), TPC-E on
  each table's first key column (`security` replicated); transactions that touch two shards (history rows, remote warehouses) commit
  by two-phase commit; a deadlock victim is retried. The consistency conditions are checked before and after the run. `tests/oltp.rs`
  runs both for a moment.
* `bench_cluster`: the micro-benchmark's workloads (loops, procedures and `sp_executesql` left out) on one engine and the cluster,
  with both times, the plan and whether the answers agree, and concurrent sessions of the cluster.
* `sqlsmith_cluster`: the fuzzer's random queries on one engine and the cluster; answers, error numbers and the effect of data changes
  must agree (error 50100, a statement the cluster refuses on purpose, is counted). It found three bugs now fixed: a rewritten query
  that had to gather lost its temporary tables, a grand total over nothing gave NULL for `COUNT`, and a table joined with itself was
  routed to one shard when a key condition named one of its aliases.

**More than one control node.** One control node (its planner, its own engine and the sessions of every terminal) saturates first on small
machines: in the TPC-C run on ten 2-vCPU machines it was at 100% while the nine shards averaged 7%. `Cluster::attach(state_file)` makes
another *front end* over the same shards: it reads the catalogue from the state file of the control node that owns the schema and
serves sessions of its own; it does not write that file and does not settle the prepared transactions the shards hold (they may be live
ones of the other front ends), and schema changes, elasticity and identity counters stay with the owner. `oltp_cluster --frontends N`
divides the terminals over N attached control nodes in one process; for several machines, `--load-only --state S` on one, then
`--attach --state S --terminals K --first I*K --start-at EPOCH` on each client, then `--attach --check-only --state S`. Deadlocks that
span front ends are found by each front end's detector (it sees every wait on the shards) or end at the lock timeout.

A join whose tables' keys are all fixed by `key = literal` conditions to the same shard (TPC-C Stock-Level) runs on that shard alone.

## Graph tables

Node and edge tables (see [graph.md](graph.md)) work in a cluster. The control node numbers their rows, as it numbers `IDENTITY` values:
the `$graph_id` of every row comes from a counter in its state, and the shards (and its own engine) make the table in the form that takes
`$graph_id` as given. `MATCH` is joins, planned as any; `SHORTEST_PATH` gathers the graph to the control node; the control node
does what a deleted node means for the edges at it (`ON DELETE CASCADE`, `NO ACTION`), since the edges may be on other shards. Rows copied
between shards (a new shard, a rebalance, the copies a join makes) are copied with every column by name, the hidden ones included.

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
  functions and triggers (they would need an interpreter next to the data; a procedure is made on every shard and can be
  declared to run on one, see *Procedures that run on one shard*), `CREATE DATABASE`/`USE`, temporal
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
