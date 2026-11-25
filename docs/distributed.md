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

First make the certificates with which the nodes know one another (see [Security](#security)), and copy to each node
`ca.pem` and its own `.pem` and `.key`:

```
tsql-cluster --make-certs certs control s0r1 s0r2 s0r3 s1
```

```
# one shard, three replicas (each its own data directory; --id is the shard, --replica the member of its group)
tsql-shard --id 0 --listen 10.0.0.1:50051 --data /data/s0r1 --replica 1 --peers 2=10.0.0.2:50051,3=10.0.0.3:50051 \
    --node-ca ca.pem --node-cert s0r1.pem --node-key s0r1.key
tsql-shard --id 0 --listen 10.0.0.2:50051 --data /data/s0r2 --replica 2 --peers 1=10.0.0.1:50051,3=10.0.0.3:50051 \
    --node-ca ca.pem --node-cert s0r2.pem --node-key s0r2.key
tsql-shard --id 0 --listen 10.0.0.3:50051 --data /data/s0r3 --replica 3 --peers 1=10.0.0.1:50051,2=10.0.0.2:50051 \
    --node-ca ca.pem --node-cert s0r3.pem --node-key s0r3.key
# an unreplicated shard
tsql-shard --id 1 --listen 10.0.0.4:50051 --data /data/s1 \
    --node-ca ca.pem --node-cert s1.pem --node-key s1.key
# the cluster file: a shard's replicas are listed in one address, separated by commas
echo '{"shards":[{"id":0,"addr":"10.0.0.1:50051,10.0.0.2:50051,10.0.0.3:50051"},{"id":1,"addr":"10.0.0.4:50051"}]}' > cluster.json
# serve SQL Server clients (on this machine; see Security to serve others)
tsql-cluster --config cluster.json --state cluster.state --listen 1433 --sa-password PW \
    --node-ca ca.pem --node-cert control.pem --node-key control.key
# or run a script (batches end at GO)
tsql-cluster --config cluster.json --state cluster.state --plan \
    --node-ca ca.pem --node-cert control.pem --node-key control.key < script.sql
```

`--state` keeps the control node's state (shards, slot map, tables, schema, unfinished commits) in a file: a control node
started on an existing file picks the cluster up where it was and ignores `--config`. From Rust:
`Cluster::connect_with(&config, tls)` or `Cluster::open_with(state_file, &config, tls)`, where `tls` is a
`NodeTls::from_files(..)` (`connect` and `open` use unprotected connections, to shards started with `--insecure`), then `cluster.session()` and `session.execute(sql)`; `session.last_plan()` says how each
statement ran. `cargo run --release -p tsql-control --example tpch_cluster` runs TPC-H on a cluster and compares every
answer with a single node; `--example tpcds_cluster` does the same for the 24 TPC-DS-style queries (and `oltp_cluster` runs TPC-C and the reduced TPC-E, `bench_cluster` the engine micro-benchmark and `sqlsmith_cluster` the fuzzer, each against one engine; see *Benchmarks on a cluster*) (dimensions replicated, fact
tables sharded on the item), and its table shows how each query ran (`split`, `set`, `correlated`, `join`, `scatter-gather`,
`gather`): none gathers.

## Security

All connections between nodes (control node to shard, shard to shard, and replica to replica) use **mutual TLS**: both
ends show a certificate signed by the cluster's certificate authority, and refuse the connection otherwise. `tsql-shard`
and `tsql-cluster` do not start without `--node-ca`, `--node-cert` and `--node-key`. `--insecure` starts them without
protection, with a warning: then anyone who reaches a shard's port can run any T-SQL on it as `sa`, which includes
reading and writing files through `BACKUP` and `BULK INSERT`. The two kinds of node cannot be mixed.

```
tsql-cluster --make-certs certs control shard0 shard1     # ca.pem, ca.key, and NAME.pem and NAME.key for each name
tsql-cluster --make-certs certs shard2                    # later: another node, signed by the same authority
```

* **Guard `ca.key`.** It is only needed to make certificates; do not copy it to the nodes. Whoever holds it, or any
  certificate it signed with its key, has that access to every shard.
* Every certificate of the authority gives the same access, and none can be revoked: if a key leaks, make a new
  authority in an empty directory and give every node a new certificate. Nodes may share one certificate (an image that
  `deploy/gcp/scale-up.sh` starts shards from carries it).
* Key files are readable only by their owner on Unix. On Windows they take the folder's permissions: restrict them.
* Made certificates expire on 1 January ten years after the year they were made. Replace them before then and restart
  the nodes.
* **Your own certificates** work too (PEM files). The authority must sign nothing but this cluster's nodes. Every node's
  certificate needs the DNS name `tsql-node` (callers check for that name, not for the host, so a node can move) and, if
  it restricts key usage, both server and client authentication.

**Clients** reach the control node over TDS, whose TLS is set separately and works as in `tsql-serve`
([tds.md](tds.md#encryption-tls)): TLS is offered with a self-signed certificate made at start-up, or with
`--tls-cert FILE --tls-key FILE`; `--force-encryption`, `--tls13` and `--no-tls`. To listen on an address that other
machines can reach (`--bind`), `tsql-cluster` needs `--force-encryption`, or `--insecure-clients`. Clients then have to
encrypt, and either trust the certificate (`TrustServerCertificate=True`; `--encrypt` in `tsql-gui`) or be given one
they can verify. A login without a password cannot connect over the network.

The `--metrics-listen` pages are plain HTTP without authentication.

## How a statement runs

* **DDL** (`CREATE`/`DROP TABLE`, `ALTER TABLE`, indexes, `SET`) runs on the control node's own *mirror* engine first (which
  checks it and keeps the schema), then on every shard; a table that cannot be created everywhere is removed again.
  `ALTER TABLE` may add, change (not the shard key or an identity column) and drop columns and constraints; adding a primary key
  or a `UNIQUE` constraint is allowed (see *Unique keys across shards*: one without the shard key is checked across the shards, against the rows
  that are there first), but a column that is a primary key or `UNIQUE` as it is added, and an `IDENTITY` column, are refused. **Views** are kept by the control node (the shards hold none): a
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
  * *procedures that run on one shard* (a procedure that is not declared like this runs on the control node: see *Stored procedures*;
    `CREATE PROCEDURE` is made on every shard; `DISTRIBUTE PROCEDURE name BY @param` declares that
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

## Stored functions

`CREATE [OR ALTER] FUNCTION` (scalar, inline and multi-statement table-valued) and `DROP FUNCTION` work in a cluster, in any of its
databases. The control node's engine (the *mirror*) checks a function and keeps it; the registry of functions is rebuilt from the schema log
when a control node starts again, so no file format changes. Where else a function lives depends on what it reads:

* A function that **runs on the shards** (it reads no table of the cluster, or only *replicated* ones, which every shard holds in full; it
  reads no view; and the functions it calls are the same) is made on every shard too, as a procedure is, so a query that calls it still
  runs on the shards: `SELECT SUM(dbo.dbl(qty)) FROM items` is scattered, and the function may be used in `INSERT`, `UPDATE`, `DELETE`,
  in a procedure, and in a computed column, a default or a `CHECK`. A query that names no table but calls a function that reads a
  replicated one (`SELECT dbo.region_name('EU')`) runs on shard 0, since the control node's own copy of a table has no rows. A shard that
  joins is given these functions.
* A function that **reads a sharded table** (or a view) lives on the control node only, because a shard holds a slice of each such table
  and would answer from its slice. A query that calls one, directly, through a view, or through another function, is *gathered*: every
  table it reaches (the function's too) is copied into the control node and the whole query runs there. The answer is exact, at
  single-node speed, and the copies of tables are kept until a change bumps their version. The plan says `gather: the query calls a
  function that reads tables`. An `INSERT`, `UPDATE` or `DELETE`, a default and a `CHECK` that calls one is run by the control node, which
  evaluates the calls and sends the shards their values (see *Functions that read sharded tables, in data changes*); a computed column that
  calls one is refused with error 50100.

Which kind a function is is worked out from what it reads *now*: a table made after the function, or dropped, or distributed
(`DISTRIBUTE TABLE`, which needs an empty table) moves the functions that read it between the shards and the control node. A stored
procedure that runs on one shard (a routed one, see *Procedures that run on one shard*) can call only the functions that are there, and
nothing is allowed to strand one: `CREATE TABLE`
(of a table a function named before it was there), `DISTRIBUTE TABLE` and `CREATE OR ALTER FUNCTION` are refused (error 50105, naming the
procedure and the function) when they would take from the shards a function that a procedure calls, and nothing is changed; drop the
procedure first. `DISTRIBUTE PROCEDURE` is refused (error 50104) for a procedure that calls a function that reads sharded tables. A procedure
that is not routed runs on the control node and may call any function. A statement that calls a function is checked every time it runs.
A restore treats a function that reads any table of the backup as one that reads sharded tables when it decides which computed columns
to keep, even when the table ends up replicated (the function itself is then put on the shards, but its computed column is left out).

A function that is changed (`CREATE OR ALTER`) moves between the two kinds as what it reads changes: one that begins to read a table leaves the
shards, one that stops is put on them. A call is given the database of its function (`db.schema.f(...)`, which the engine runs in that
database, as SQL Server does), since a shard's connection is in whichever database it was last sent to; from another database a function is
called by that name (`SELECT shop.dbo.dbl(21)`). Dropping a database drops its functions. Triggers are not supported.

### Functions that read sharded tables, in data changes

A shard cannot call a function that reads sharded tables (it would answer from its slice), so the control node does what the shards cannot:

* **`INSERT ... VALUES`, `UPDATE`, `DELETE` with a call whose arguments are constants or variables** (`VALUES (1, dbo.total())`,
  `SET qty = dbo.total()`, `WHERE qty > dbo.total() / 2`): the call is evaluated first, over the tables as they are before the statement,
  and the statement goes to the shards with its value. `INSERT ... SELECT` runs its `SELECT` as any query that calls such a function does.
* **A call over the columns of the table being changed** (`SET rnk = dbo.rank_of(qty)`, `WHERE dbo.rank_of(qty) >= 9`): the control node
  reads the distinct values of the arguments among the rows concerned (the rows the `WHERE` lets through, for the `SET`; compared as bytes, so
  `'a'` and `'A'` are two values, and NULL is one), calls the function once for each, and the statement gets a `CASE` that looks the value
  up. At most 5000 distinct values; more is refused (50100). A row that appears between the reading and the writing, with values that
  were not read, raises a divide-by-zero error instead of getting a wrong value: run the statement again. The plan says
  `function f evaluated first on the control node`.
* **`DEFAULT` and `CHECK` that call such a function** are not given to the shards or to the control node's engine: the control node keeps
  them as *rules* of the table. A `CHECK` is verified after each `INSERT` or `UPDATE` over the rows it wrote, in the statement's
  transaction; a row for which the condition is false undoes the statement with error 547 (`ALTER TABLE ... ADD CHECK` verifies the rows
  that are there, `DROP CONSTRAINT name` drops it). A `DEFAULT` is evaluated, once for the statement, when an `INSERT` leaves the column
  out or says `DEFAULT` for it. A rule is a hidden trigger (`ck$table$name`, `df$table$column`): logged with the schema, found again when
  the control node starts, dropped with the table, not listed by `DISABLE TRIGGER ALL`, and it gives the table the limits of a table with
  triggers (an `UPDATE` of the shard key is refused). The constraint is not in `sys.check_constraints` or `sys.default_constraints`.
* **A computed column** that calls one is still refused: it is evaluated whenever a row is read, on the shard that holds it.

## UPDATE ... FROM and DELETE ... FROM

A data change that joins another table (`UPDATE t SET c = s.v FROM s WHERE t.k = s.k`, `UPDATE x SET ... FROM t x JOIN s ON ...`, `DELETE t FROM t JOIN s ON ...`) is run by the control node: it reads the join as a query over the cluster (the key of the target's rows and, for an `UPDATE`, the new value of each column the `SET` names, worked out by the engine for every joined row), then applies the changes to the target by key, 100 rows at a time, as ordinary data changes: they go to the shards that hold the rows, fire the triggers and the checks of the table, and are part of the transaction. The key is the primary key, else every column that is not computed. When several rows of the join match one row of the target the first is applied (SQL Server applies an arbitrary one). The join is held by the control node: more than 200 000 rows are refused (50100). `OUTPUT`, `TOP` and CTEs in such a statement are still refused.

## Cursors

`DECLARE name CURSOR FOR query`, `OPEN`, `FETCH [NEXT | PRIOR | FIRST | LAST | ABSOLUTE n | RELATIVE n] FROM name INTO @v, ...`, `CLOSE`, `DEALLOCATE` and
`@@FETCH_STATUS` work over the cluster's tables in a batch, a procedure the control node runs and the body of a trigger (`FETCH` from `deleted` or
`inserted` included). A cursor is a **static snapshot**: `OPEN` runs the query, with the variables it names as they are at that moment, and
keeps the rows; changes made afterwards are not seen. `FETCH` must have `INTO` (one that would return the row as a result set is refused, 50100).
A cursor belongs to the batch that declared it, and a trigger's to the trigger. The errors are the engine's: 16916 (no such cursor), 16917 (not
open), 16905 (declared twice).

## DDL triggers

`CREATE | ALTER | DROP TRIGGER name ON DATABASE FOR events AS ...` and `ENABLE | DISABLE TRIGGER name | ALL ON DATABASE` work in a cluster. The control
node keeps the triggers of each database (logged with the schema, so they are there when it starts again) and runs their bodies itself, since a
body reads and writes the cluster's tables, which only it can do. The events are those of the DDL the cluster runs for tables, indexes, views,
procedures, functions, triggers, schemas and types (`CREATE_TABLE`, `DROP_TABLE`, `DDL_TABLE_EVENTS`, `DDL_DATABASE_LEVEL_EVENTS`, ...); an unknown event
is error 1082. `EVENTDATA()` gives the event's XML (`EventType`, `PostTime`, `SPID`, `ServerName`, `LoginName`, `UserName`, `DatabaseName`,
`SchemaName`, `ObjectName`, `ObjectType`, `TSQLCommand/CommandText`), read with `.value()`.

* **A trigger runs before the statement**, not after it as on a single server: the statement changes the shards and the control node's engine,
  which cannot be taken back together. An error stops the statement, and so does `ROLLBACK` followed by `THROW` or `RAISERROR`, which is how a
  trigger refuses `DROP_TABLE` or `ALTER_TABLE`. A `ROLLBACK` alone ends the batch with error 3609, as it does there.
* A statement that fails after its triggers ran leaves what they wrote (an audit row, say), and a body that selects from the catalog sees the object
  as it was before the statement.
* A trigger does not run for DDL its own body runs, nor for what a `RESTORE` makes.
* **Not supported:** `ON ALL SERVER` and `LOGON` triggers (error 50100). A DDL trigger is not in the control node's engine, so `sys.triggers`
  does not list it.

## User-defined types

`CREATE TYPE name FROM base [NOT] NULL` (an alias type) and `CREATE TYPE name AS TABLE (...)` (a table type) are made on the control node's
engine and on every shard, logged with the schema (so a control node that starts again, and a new shard, have them), and dropped with
`DROP TYPE`. A column, a variable, a `CAST` and a parameter may then be of the type, on the shards and on the control node. A column of an alias
type may be the shard key (it is classified by its base type). A table type is a table variable's type and the type of a table-valued parameter
(`READONLY`) of a procedure the control node runs. A `RESTORE` makes the types of the backup before its tables, procedures and functions, and
no longer says that it left them out. As on a single server (this engine does not refuse to drop a type that a column uses), `DROP TYPE` does
not check for users of the type. Types are made outside a transaction.

## XML and spatial indexes

`CREATE [PRIMARY] XML INDEX` and `CREATE SPATIAL INDEX` are declarations on a single server (they are checked and kept in the catalog, and change no
result or plan: see [optimizer.md](optimizer.md) and [spatial.md](spatial.md)). A cluster keeps them the same way: the statement runs on the
control node's engine, which applies the rules (one primary XML index per column, the names shared with the table's other indexes, the
primary key a table needs), then on every shard, and is logged with the schema. `DROP INDEX` drops them (a primary XML index takes the
secondary ones built on it), the catalog views show them, and a `RESTORE` makes them after the table's rows, the primary XML indexes first.

## Full-text catalogs and indexes

`CREATE | ALTER | DROP FULLTEXT CATALOG` and `... FULLTEXT INDEX` are declarations on a single server: a search tokenises the text of the rows it
looks at (see [triggers.md](triggers.md)). A cluster keeps them as it keeps XML indexes: the control node's engine applies the rules
(errors 7608 to 7615: the key index must be a unique, non-nullable, single-column index, one full-text index per table, a catalog with indexes is
not dropped), then every shard runs the statement, and it is logged with the schema. The key index may be a unique key that does not contain the
shard key (see *Unique keys across shards*). `CONTAINS` and `FREETEXT` are evaluated by the shards over their own rows; a query that reads
`CONTAINSTABLE` or `FREETEXTTABLE` reads the table they name, which the control node gathers as it does for any query it runs itself. As on a
single server, a predicate on a table that has no full-text index is not an error. Catalogs are made outside a transaction here, as are all schema
changes. A `RESTORE` makes the catalogs of the backup first, and the full-text index of each table after its unique key index.

## Schema changes inside a transaction

`CREATE`, `ALTER` and `DROP` of tables, indexes, views, procedures, functions, triggers and types work after `BEGIN TRAN` (Reporting Services' upgrade scripts
do the whole migration in one transaction). The control node's engine, the schema log and the shards' own copies are not one transaction, so such a change cannot be
undone together with the rest. **A schema change commits what the transaction has done so far, is made, and the client goes on in a new transaction of the same depth**
(`@@TRANCOUNT` is as it was): a `ROLLBACK` undoes what came after the last schema change, not the change nor what came before it. This also lets go of the locks the
transaction held, which the change would otherwise wait for. `TRUNCATE` inside a transaction is still refused (50100).

## UPDATE ... FROM and DELETE ... FROM

A data change that joins another table (`UPDATE t SET c = s.v FROM s WHERE t.k = s.k`, `UPDATE x SET ... FROM t x JOIN s ON ...`, `DELETE t FROM t JOIN s ON ...`) is run by the control node: it reads the join as a query over the cluster (the key of the target's rows and, for an `UPDATE`, the new value of each column the `SET` names, worked out by the engine for every joined row), then applies the changes to the target by key, 100 rows at a time, as ordinary data changes: they go to the shards that hold the rows, fire the triggers and the checks of the table, and are part of the transaction. The key is the primary key, else every column that is not computed. When several rows of the join match one row of the target the first is applied (SQL Server applies an arbitrary one). The join is held by the control node: more than 200 000 rows are refused (50100). `OUTPUT`, `TOP` and CTEs in such a statement are still refused.

## Cursors

`DECLARE name CURSOR FOR query`, `OPEN`, `FETCH [NEXT | PRIOR | FIRST | LAST | ABSOLUTE n | RELATIVE n] FROM name INTO @v, ...`, `CLOSE`, `DEALLOCATE` and
`@@FETCH_STATUS` work over the cluster's tables in a batch, a procedure the control node runs and the body of a trigger (`FETCH` from `deleted` or
`inserted` included). A cursor is a **static snapshot**: `OPEN` runs the query, with the variables it names as they are at that moment, and
keeps the rows; changes made afterwards are not seen. `FETCH` must have `INTO` (one that would return the row as a result set is refused, 50100).
A cursor belongs to the batch that declared it, and a trigger's to the trigger. The errors are the engine's: 16916 (no such cursor), 16917 (not
open), 16905 (declared twice).

## DDL triggers

`CREATE | ALTER | DROP TRIGGER name ON DATABASE FOR events AS ...` and `ENABLE | DISABLE TRIGGER name | ALL ON DATABASE` work in a cluster. The control
node keeps the triggers of each database (logged with the schema, so they are there when it starts again) and runs their bodies itself, since a
body reads and writes the cluster's tables, which only it can do. The events are those of the DDL the cluster runs for tables, indexes, views,
procedures, functions, triggers, schemas and types (`CREATE_TABLE`, `DROP_TABLE`, `DDL_TABLE_EVENTS`, `DDL_DATABASE_LEVEL_EVENTS`, ...); an unknown event
is error 1082. `EVENTDATA()` gives the event's XML (`EventType`, `PostTime`, `SPID`, `ServerName`, `LoginName`, `UserName`, `DatabaseName`,
`SchemaName`, `ObjectName`, `ObjectType`, `TSQLCommand/CommandText`), read with `.value()`.

* **A trigger runs before the statement**, not after it as on a single server: the statement changes the shards and the control node's engine,
  which cannot be taken back together. An error stops the statement, and so does `ROLLBACK` followed by `THROW` or `RAISERROR`, which is how a
  trigger refuses `DROP_TABLE` or `ALTER_TABLE`. A `ROLLBACK` alone ends the batch with error 3609, as it does there.
* A statement that fails after its triggers ran leaves what they wrote (an audit row, say), and a body that selects from the catalog sees the object
  as it was before the statement.
* A trigger does not run for DDL its own body runs, nor for what a `RESTORE` makes.
* **Not supported:** `ON ALL SERVER` and `LOGON` triggers (error 50100). A DDL trigger is not in the control node's engine, so `sys.triggers`
  does not list it.

## User-defined types

`CREATE TYPE name FROM base [NOT] NULL` (an alias type) and `CREATE TYPE name AS TABLE (...)` (a table type) are made on the control node's
engine and on every shard, logged with the schema (so a control node that starts again, and a new shard, have them), and dropped with
`DROP TYPE`. A column, a variable, a `CAST` and a parameter may then be of the type, on the shards and on the control node. A column of an alias
type may be the shard key (it is classified by its base type). A table type is a table variable's type and the type of a table-valued parameter
(`READONLY`) of a procedure the control node runs. A `RESTORE` makes the types of the backup before its tables, procedures and functions, and
no longer says that it left them out. As on a single server (this engine does not refuse to drop a type that a column uses), `DROP TYPE` does
not check for users of the type. Types are made outside a transaction.

## XML and spatial indexes

`CREATE [PRIMARY] XML INDEX` and `CREATE SPATIAL INDEX` are declarations on a single server (they are checked and kept in the catalog, and change no
result or plan: see [optimizer.md](optimizer.md) and [spatial.md](spatial.md)). A cluster keeps them the same way: the statement runs on the
control node's engine, which applies the rules (one primary XML index per column, the names shared with the table's other indexes, the
primary key a table needs), then on every shard, and is logged with the schema. `DROP INDEX` drops them (a primary XML index takes the
secondary ones built on it), the catalog views show them, and a `RESTORE` makes them after the table's rows, the primary XML indexes first.

## Full-text catalogs and indexes

`CREATE | ALTER | DROP FULLTEXT CATALOG` and `... FULLTEXT INDEX` are declarations on a single server: a search tokenises the text of the rows it
looks at (see [triggers.md](triggers.md)). A cluster keeps them as it keeps XML indexes: the control node's engine applies the rules
(errors 7608 to 7615: the key index must be a unique, non-nullable, single-column index, one full-text index per table, a catalog with indexes is
not dropped), then every shard runs the statement, and it is logged with the schema. The key index may be a unique key that does not contain the
shard key (see *Unique keys across shards*). `CONTAINS` and `FREETEXT` are evaluated by the shards over their own rows; a query that reads
`CONTAINSTABLE` or `FREETEXTTABLE` reads the table they name, which the control node gathers as it does for any query it runs itself. As on a
single server, a predicate on a table that has no full-text index is not an error. Catalogs are made outside a transaction here, as are all schema
changes. A `RESTORE` makes the catalogs of the backup first, and the full-text index of each table after its unique key index.

## Schema changes inside a transaction

`CREATE`, `ALTER` and `DROP` of tables, indexes, views, procedures, functions, triggers and types work after `BEGIN TRAN` (Reporting Services' upgrade scripts do the whole migration in one transaction). The control node's engine, the schema log and the shards' own copies are not one transaction, so such a change cannot be undone together: **it is made at once, and a `ROLLBACK` does not undo it** (the rows the transaction wrote are undone, as always). On the shards it runs in a session of its own, apart from the transaction's, so that it does not take the transaction's locks; an `ALTER TABLE` of a table the transaction has written waits for the lock timeout. `TRUNCATE` inside a transaction is still refused (50100).

## Unique keys across shards

A shard can keep a unique key unique only among its own rows, so a `PRIMARY KEY`, `UNIQUE` constraint or `UNIQUE` index whose columns do not
include the shard key is also checked by the control node (the shards keep their own copy of the index too). The keys are read from the control
node's engine after every change of the table, so the names of constraints the engine made up (`UQ__t__...`) are known, and
`DROP INDEX` and `DROP CONSTRAINT` forget them.

* An **`INSERT`** looks for the new values among the rows the shards hold, and among the new rows themselves; a duplicate is error 2627
  (a constraint) or 2601 (an index), worded as the engine words them. Values are compared as a unique index compares them: a NULL equals
  a NULL (one is allowed), case and trailing spaces do not count (the default collation).
* An **`UPDATE`** that sets a column of the key reads the values the rows would have (the engine evaluates the `SET`), and looks for them among
  the rows the statement does not change and among themselves, so a statement may swap values between its own rows. One that moves the
  shard key is checked as the `INSERT` it becomes, after its delete. A statement that changes a key in more than 200 000 rows is refused (50100).
* **`CREATE UNIQUE INDEX`** and **`ALTER TABLE ... ADD UNIQUE`** verify the rows that are there (error 1505 for a duplicate).
* **Concurrent sessions.** The control node keeps a lock of the unique keys of each table. A session takes it before it looks for a value
  and holds it until its transaction ends (until the statement ends, with none), so the next session finds what the first wrote. Sessions
  that write such a table take turns; one that waits more than 10 s gets error 1222. Reads are not affected.
* **Not checked:** a filtered unique index that lacks the shard key (refused when it is made), a key column that an `INSERT` leaves out or
  gives `DEFAULT` (its value is not known to the control node), and the rows a `RESTORE` puts in (they come from a table that was unique
  as a whole). Only the control node keeps these keys: a write that does not come through it is not checked.

## Stored procedures

A procedure made with `CREATE PROCEDURE` is made on every shard and on the control node. `EXEC` of one that is declared with
`DISTRIBUTE PROCEDURE` goes to the shard of its key (see *Procedures that run on one shard*). A call with no `OUTPUT` parameter, return
value or table-valued argument also declares an undeclared procedure by itself, on the first parameter that passes the same check, unless it
calls a function that reads sharded tables. `EXEC` of any other procedure of the cluster
runs on the control node: its body is run statement by statement by the session's own executor, so what it reads and writes goes through
the cluster like any batch (a query over sharded tables is scattered, gathered or moved as it would be alone, and DML keeps its triggers,
foreign keys and global indexes). That is what makes `EXEC` work anywhere a statement does: in a batch, in a procedure, in a trigger body.

The body runs in a scope of its own, in the database the procedure was made in: the parameters (by name or by position, with defaults, a string
converted to the parameter's type), `OUTPUT` parameters copied back to the caller's variables, `RETURN value` into `EXEC @rc = ...`, table-valued
parameters (`READONLY`), `IF`, `WHILE` with `BREAK` and `CONTINUE`, `TRY ... CATCH` and `THROW`, nested calls to 32 levels (error 217), and
`EXEC('text')` (a batch of its own, which does not see the caller's variables). The caller's variables are not visible inside. A name that is not
a procedure of the cluster (a system procedure) is the control node's engine's, and an unknown one is its error 2812. Not supported: a routed
procedure with an `OUTPUT` parameter or a return value, cursors, `EXECUTE AS`.

## Triggers

`CREATE [OR ALTER] TRIGGER`, `DROP TRIGGER`, `ENABLE | DISABLE TRIGGER` work for the triggers of tables (`AFTER` and `INSTEAD OF`, for `INSERT`,
`UPDATE` and `DELETE`, in any database). They are the control node's: kept in a registry that is rebuilt from the schema log, and made
neither on the control node's engine nor on the shards, which would fire them on the wrong rows (a shard holds a slice of each table, and a
trigger that reads or writes another table has to reach every shard). The control node fires them around the statements it runs:

* the statement runs inside a transaction (the client's, or one of its own), with `OUTPUT` added on the shards so that the control node gets the
  rows that were inserted and deleted;
* an `AFTER` trigger then runs once for the statement, as T-SQL of the session: `inserted` and `deleted` are replicated temporary tables
  holding those rows, and what the body reads or writes goes through the cluster, in the same transaction, so a trigger may change any table;
  an error, or a `ROLLBACK`, undoes the statement and everything the trigger did (a `ROLLBACK` ends the batch with error 3609, as in SQL Server);
* an `INSTEAD OF` trigger runs in place of the statement: `deleted` holds the rows it would delete or change, `inserted` the rows it would
  insert (or the changed rows). Only one `INSTEAD OF` trigger per event (error 2111).

As in SQL Server: a trigger is not fired again by what it does to its own table (direct recursion is off), an `INSTEAD OF` trigger's own
statement on its table is done for real (and fires its `AFTER` triggers), triggers nest to 32 levels (error 217), a statement that changes no row
fires nothing, and `UPDATE(column)` tells whether the statement set the column. A trigger body may use data changes, queries, variables,
`IF`, `WHILE`, `BEGIN ... END`, `TRY ... CATCH`, `EXEC` (of any procedure of the cluster), cursors, `PRINT`, `THROW`, `RAISERROR`, `RETURN` and `ROLLBACK`; one that
uses anything else (`TRUNCATE`, ...) is refused when it is made, not when it fires. Not supported: triggers on views, `ALL SERVER` and logon triggers (database DDL triggers are: see *DDL triggers*), `COLUMNS_UPDATED()`,
`INSERT ... EXEC` into a table with an `INSTEAD OF` trigger, an `UPDATE` of the shard key of a table that has triggers, and a statement that
changes more rows than a statement can broadcast (the limit `broadcast_limit` sets) on a table that has triggers. Rows loaded by `INSERT BULK`
and by a restore fire nothing. Row counts and results of a trigger body are not sent to the client.

## TRY ... CATCH

The control node runs `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` in batches that use the cluster's tables and in trigger bodies. An error of
the `TRY` block (severity below 20) runs the `CATCH` block, where `ERROR_NUMBER()`, `ERROR_MESSAGE()`, `ERROR_SEVERITY()`, `ERROR_STATE()`,
`ERROR_PROCEDURE()` and `THROW;` (which repeats the error; error 10704 outside a `CATCH` block) work, and `XACT_STATE()` tells the transaction's
state (0 none, 1 usable, -1 doomed). A statement that fails inside one shard is undone by that shard, so the transaction goes on. A statement that had
changed several shards when it failed, and any error that ends a trigger, leaves the transaction half done, so it can only be rolled back
(`XACT_STATE() = -1`): the `CATCH` block may read and `ROLLBACK`, and a write is error 3930. A deadlock victim (1205) is not caught: its transaction is
rolled back everywhere before the batch ends. `ERROR_LINE()` is always 1.

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
...` (`--addr` if its address differs from `--listen`) and its own certificate flags, then `Cluster::add_replica(shard, R, addr)`: the leader adds it as a
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
running (`tsql-shard --insecure` on other machines: the examples connect without TLS), otherwise `--shards N` starts
them in the process.

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

## Restoring a backup into a cluster

```sql
RESTORE DATABASE shop FROM DISK = '/backups/shop.bak'
    WITH DISTRIBUTION = N'dbo.orders BY customer_id; dbo.currencies REPLICATED', REPLACE;
```

`RESTORE DATABASE` sent to the control node restores the backup into the cluster: the tables are made through the cluster (as
`CREATE TABLE` makes them) and the rows go to the shards that own them **as the backup is read**, a batch at a time with the
`load` action (`crates/parallel/control/src/restore.rs`). The control node never holds the database: its own engine gets the
schema, as for every distributed table. A backup of this server and a SQL Server `.bak` both work (a `.bak` is first converted
to this server's format in a temporary file, as a single-node restore does).

* **How each table is spread.** `WITH DISTRIBUTION = N'table BY column; table REPLICATED; ...'` says it (names with or without a
  schema and brackets, entries separated by `;` or `,`). A table it does not name is **replicated when it has at most 10,000
  rows** and otherwise sharded on the first column of its primary key, as `CREATE TABLE` does. A shard key must be of a type the
  cluster hashes (integer, character, `uniqueidentifier`) and be in every primary key and unique constraint of its table;
  otherwise the restore is refused before anything is made, and says to name another key or `REPLICATED`.
* **The order.** The backup is checked (every checksum) and each table's rows counted; then the schemas (on the control node and
  every shard), the tables with their keys, defaults, computed columns, collations, masks and check constraints, the foreign
  keys (while the tables are empty, so the tables can come in any order), the rows, the other indexes, the identity counters
  (they go on from the source's), and last the views and procedures in the order they were made. The rows are written as
  committed rows (a durable or replicated shard takes ordinary `INSERT`s, which its log and its replicas keep). The foreign keys
  are not checked again for the restored rows (they come from a consistent database), and on an in-memory shard neither are the
  check constraints.
* **DDL triggers** of the database are restored last, so that nothing the restore makes runs them, and the disabled ones stay disabled (see
  *DDL triggers*).
* **User-defined types** (`CREATE TYPE ... FROM` and `... AS TABLE`) are restored first, and the columns of the restored tables, and the parameters
  of the procedures, keep them (see *User-defined types*).
* **Unique keys that do not contain the shard key** are restored and kept unique across the shards by the control node (see *Unique
  keys across shards*); AdventureWorks has such keys on `rowguid` and on business keys. A *filtered* one cannot be checked that way:
  it is refused with the rest, and `WITH CONTINUE_AFTER_ERROR` makes it an ordinary index and says so. A `DISTRIBUTION` may shard a
  table on a column outside its primary key: the primary key is then checked across the shards as well.
* **Stored functions** are restored (see *Stored functions*): the ones that read no table of the backup are made first, on the shards
  too, so a computed column may call them (`Sales.Customer.AccountNumber` of AdventureWorks calls `dbo.ufnLeadingZeros`, which is
  restored with its column); the ones that read tables are made after the tables, on the control node only. A **computed column that
  calls a function that reads tables** is left out with its indexes, with a note, under `CONTINUE_AFTER_ERROR`.
* **Triggers** of tables are restored (see *Triggers*), disabled ones disabled, after the tables and rows (loading the rows fires nothing).
  One whose body the control node cannot run (see *Triggers*), and a trigger of a view, is refused with the rest.
* **What a cluster cannot hold is refused** before anything is made: sequences, synonyms,
  system-versioned and graph tables, columnstore indexes, xml typed by a schema collection, foreign keys to other databases,
  security policies. `WITH CONTINUE_AFTER_ERROR` restores the rest and says what it left out (as messages
  beginning `Not restored:`), including views and procedures that could not be made without what was left out. Never restored,
  with a message: the database's users, roles and permissions (the
  database keeps its own), and disabled foreign keys (the cluster enforces every foreign key it knows). XML and spatial indexes
  and full-text catalogs and indexes are restored (the catalogs before the tables, the indexes after them).
* **The database.** The objects go into the cluster's database of the name the statement gives (`RESTORE DATABASE advw ...` makes
  `advw` on the control node and on every shard if the cluster does not have it; see *Databases*), and the session is in the database
  it was in. If any of the names in it is taken, the restore is error 3159; `WITH REPLACE` drops the objects of those names first. A
  failure drops everything the restore made, and the database too when the restore made it. Only a full backup restores, `WITH RECOVERY`: `NORECOVERY`, `STANDBY`, `STOPAT`, and log and differential backups
  are refused, as is a restore inside a transaction (574). While it runs, other sessions can see the tables filling.
* **`BACKUP DATABASE` of the cluster's database is refused** (error 50100): the control node's engine holds none of its rows, so the
  backup would be empty.

## Databases

A cluster has the databases `CREATE DATABASE` makes, besides `master`. `CREATE DATABASE name` is made on the control node's engine and on
every shard (so `sys.databases` and SSMS's Databases node list it), `DROP DATABASE name` drops it from all of them with the tables, views
and procedures the cluster holds in it (a database that the session that drops it is in cannot be dropped: 3702; another session that is in it, a client's pooled connection say, goes back to `master` at its next batch instead of failing on a database that is gone), and `USE name` is the engine's own: the
control node is told which database a client is in before each batch.

A table is named by its database, schema and name, and a table of another database is named in full (`shop.dbo.items`). A statement of a
session in a database other than `master` has the tables it names given the database before the cluster plans it, so everything after
that (the plan, the SQL the shards run, the catalog's ids) names a table in full; `CREATE SCHEMA`, `CREATE VIEW` and `CREATE PROCEDURE`,
which are about the whole database, are sent to the shards behind a `USE`. The id of a table has the database in it (`shop.dbo.items`),
except that of a table of `master`, which has none (`dbo.items`), so a state file of a control node made before databases existed reads
as it was: its tables are in `master`. The log of the schema that a control node (or a new shard) replays says `USE` when the database
changes, and the `CREATE DATABASE`s are in it.

What is the same in every database: the shards, the slots and the sharding of each table. What differs from SQL Server: a cluster has
no cross-database transactions beyond what a transaction already does (every table is reached through its shards), and a database has no
settings of its own (files, collation, recovery model): `CREATE DATABASE` ignores them, except that the folders and names of its `FILENAME`s are checked on the control node first, as a single server checks them (the paths are not serialized into the schema log, which a restart replays).

## Temporary tables of a client

A `CREATE TABLE #t` that a client sends to the control node (and `SELECT ... INTO #t`) makes the table in the **control node's own engine**, as
in a single server: it can have any column types and indexes (a unique index over columns that are not a shard key, `IGNORE_DUP_KEY`, `sysname`
columns), and a batch that reads it with the catalog views (SSMS scripts a table and discovers dependencies that way) runs in that engine.

A statement that needs the temporary table together with a table of the cluster is another matter: the shards cannot read the control
node's tables. When such a batch arrives, the control node **hands the temporary table over** first: it reads its `CREATE TABLE` and its rows from
the client's session, drops it there, makes it in the cluster as a *replicated* table (up to the broadcast limit, on every shard) and loads the
rows. From then on it is the cluster's, as a table made with `CREATE TABLE` while a cluster table was in the batch always was (see
`temps_wanted` and `adopt_temp` of the `Interceptor`, `crates/parallel/control/src/front.rs`). The temporary tables the cluster has made for a
session are dropped when the session ends.

Not done: a table that the engine made inside a procedure or `EXEC(...)` (the control node does not see the statement), and a temporary table
with a feature the cluster does not take (it is refused when it is handed over, with the cluster's message). `CREATE UNIQUE INDEX ... WITH
IGNORE_DUP_KEY` is accepted by the engine and does not skip duplicate rows: it raises the duplicate key error, as `CREATE UNIQUE INDEX` does.

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

* **Every node is trusted fully.** A certificate of the cluster's authority gives full access to every shard, and
  certificates cannot be revoked one by one (see [Security](#security)).

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
* **Refused** (error 50100 says so): filtered unique indexes that do not contain the shard key, stored
  triggers (a function is supported, see *Stored functions*; a procedure is made on every shard and can be declared to run on one,
  see *Procedures that run on one shard*), temporal
  tables, `OUTPUT` or `TOP` in DML, `INSERT ... DEFAULT VALUES`/`EXEC`, correlated
  subqueries in DML, the T-SQL `BULK INSERT` statement (`INSERT BULK`, the protocol's bulk
  load, works), `BACKUP` of the cluster's database, and restoring anything but a full backup (see *Restoring a backup into a
  cluster*). Views cannot be written through.
* **Foreign keys and identity values are the control node's**: the checks are not atomic across sessions (see Foreign keys),
  and a control node that loses its state file loses the identity counters and the foreign keys with it.
* **Gather moves whole tables** (queries with a correlated subquery that is not an equality, `STDEV`, recursive CTEs and the
  like run at single-node speed on the control node), a CTE, view or derived table bigger than the broadcast limit makes the
  whole query gather, results are collected on a shard before they are sent, and the control node merges in memory. The outer
  query of a split runs on the control node: a window over a very large aggregated result, or the rollup of a very fine grain,
  is as big as that result. A join of three tables that need different shuffles is still gathered.
* Elasticity moves slots one pair of shards at a time under a pause, and only the last shard can be removed.
