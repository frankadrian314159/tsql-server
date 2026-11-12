# Benchmarks and stress tests

Four programs run industry-style workloads against the engine and check what they can. They are not the audited
benchmarks (their generators, parameters and drivers differ from the official kits, which are C programs and
specifications this repository does not contain), so their numbers are for comparing this engine with itself. What they
are good for is finding bugs: each of them found some (listed below).

| Program | Command | What it is |
| --- | --- | --- |
| **TPC** | `cargo run --release --example tpc [h\|ds] [--sf 0.01] [--compare] [--plans]` | TPC-H (all 22 queries) and a TPC-DS-style subset |
| **TPC-OLTP** | `cargo run --release --example tpc_oltp [c\|e] [--terminals 4] [--secs 10] [--warehouses 2] [--durable] [--pool PAGES]` | TPC-C and a reduced TPC-E |
| **SQLsmith-style fuzzer** | `cargo run --release --example sqlsmith [iterations] [--seed N]` | random queries and data changes, checked three ways |
| **tSQLt-style suites** | `cargo test --test tsqlt` | about 195 unit tests written in T-SQL, run on the server ([docs/tsqlt.md](tsqlt.md)) |

Each exits non-zero when a query fails, an answer is wrong, a consistency condition is violated or a transaction fails
for a reason other than being a deadlock victim (which is retried).

## TPC (`examples/tpc`)

**TPC-H.** The eight tables, a generator that follows the specification's cardinalities, key relationships and value
domains (`dbgen`'s random streams are not reproduced, so the answers are not the published validation answers), and the
22 queries in T-SQL with the specification's validation parameters. Six answers (Q1, Q6, Q12, Q13, Q14, Q18) are
recomputed in Rust from the generated rows and compared.

**TPC-DS.** A subset: the retail snowflake (date, time, item, customer, address and demographics dimensions; store,
catalog and web sales; store returns; inventory) and 24 queries in the style of the benchmark's, covering what the
99 queries use: star joins, correlated subqueries, windows over aggregates, `ROLLUP`, `INTERSECT` / `EXCEPT`,
year-over-year CTEs. It is not the full benchmark (24 tables, 99 query templates). Three answers (Q3, Q42, Q96) are
recomputed in Rust.

`--compare` loads the same data into a second server with a tiny work-memory budget, which sends every query through the
streaming and spilling executors, and requires the two servers to agree on every answer. `--plans` prints the optimizer's
plan for each query; a plan is also printed for any query over half a second. `--query Q5` runs one query. Built with
`--features prof` it ends with the span profile.

Scale factor 0.01 (60,000 lineitem rows), in memory, one thread:

| | Time |
| --- | ---: |
| TPC-H, 22 queries | 2.6 s (slowest: Q1 0.45 s, Q9 0.25 s, Q21 0.24 s) |
| TPC-DS subset, 24 queries | 3.0 s (slowest: Q1 0.76 s, Q11 0.64 s, Q15 0.57 s) |

## TPC-OLTP (`examples/tpc_oltp`)

**TPC-C.** The nine tables, the five transactions as T-SQL stored procedures (New-Order, Payment, Order-Status,
Delivery, Stock-Level; they use `UPDLOCK`, `ROW_NUMBER` for the customer-by-name lookup, loops and `RETURN` codes), the
specification's mix (45 / 43 / 4 / 4 / 4 %) with 1% New-Orders rolled back on purpose, 1% remote supply warehouses and
15% remote payments, run by several terminals (threads, one connection each) with no think time. It is scaled down: 300
customers per district and 1,000 items. After the run, the consistency conditions of clause 3.3 are checked
(1, 2, 3, 4, 5, 6, 8, 9, 10 and 12; the initial database must satisfy them too).

**TPC-E (reduced).** The brokerage schema (customers, accounts, brokers, securities, trades, trade history, holdings and
holding summaries, watch lists), nine transactions (Broker-Volume, Customer-Position, Market-Feed, Market-Watch,
Security-Detail, Trade-Lookup, Trade-Order, Trade-Result, Trade-Status) in the specification's read-heavy mix, with
Trade-Result completing the trades Trade-Order queued. It is not the full benchmark (33 tables, many more transaction
frames). Its checks tie the tables together: holding summaries equal the sum of holdings, broker trade counts and
commissions equal the completed trades, account balances equal the initial balance plus sells minus buys minus
commissions, and every trade has its history rows.

Table hints written `WITH (UPDLOCK)` in a procedure used to stay in force for the later statements of the session, which made the
read-only transactions of TPC-E lock every row they read (see [performance.md](performance.md)); with that fixed, 4 terminals
complete 328 trade results per second against 152 before and 159 for one terminal.

4 terminals, 5 seconds, 2 warehouses, in memory: 31,000 New-Orders per minute, one deadlock retry, all conditions hold;
TPC-E about 156 Trade-Results per second, all invariants hold. With `--terminals 16 --warehouses 1 --pool 64` (a disk-backed
server with a 64-page buffer pool, so pages are evicted while transactions run) nothing fails and every condition
still holds; throughput drops, as expected with 16 terminals on the 10 hot district rows of one warehouse.

## SQLsmith-style fuzzer (`examples/sqlsmith.rs`)

The real SQLsmith is a C++ program that generates queries for PostgreSQL. This is a generator in its spirit: random,
valid-looking T-SQL over three small tables (with NULLs, duplicates and indexes): joins of every kind, derived tables,
CTEs, correlated and uncorrelated subqueries (`EXISTS`, `IN`, scalar), set operations, `GROUP BY` with `ROLLUP` /
`CUBE`, windows with frames, `APPLY`, `TOP`, `STRING_AGG`, `CASE`, string, date and numeric functions, and every few
iterations an `UPDATE`, `DELETE` or `INSERT ... SELECT`. Three oracles:

1. **Crashes and hangs.** A panic, a statement over 60 seconds, or an error whose text names an internal failure.
2. **Logic oracles.** *Ternary logic partitioning* (the rows where a predicate is true, false and NULL add up to all the
   rows) and *NoREC* (the count of rows a `WHERE` keeps equals summing the predicate evaluated per row, which cannot use
   an index or pushdown).
3. **Differential execution.** The same statement on the default executors, in a session with the optimizer off, and on
   a server with a tiny work-memory budget; the answers and, after a data change, the contents of the tables must agree.

About 23,000 statements over several seeds are clean now.

## What they found

Fixed, each with a regression test (`tests/tpc_regressions.rs` unless noted):

* **Uncorrelated subqueries were re-run for every outer row**, which made TPC-H Q18 take minutes. They are cached for
  the statement (see [performance.md](performance.md)).
* **The common part of an OR of ANDs hid join conditions** (Q19 cross-joined 60,000 by 2,000 rows, 160 s). Such
  conditions are factored out when the statement is parsed.
* **A derived table could not start with a parenthesised query** (`((SELECT ..) EXCEPT (SELECT ..)) x`, TPC-DS Q87).
* **Filters on a CTE or derived table were applied after the join**, not before (Q11, 4.1 s to 0.6 s).
* **The join order put a many-to-many join before a key join** (Q5, 1.2 s to 0.12 s): a table that joins on a unique
  key is placed before one that can multiply rows.
* **Window aggregates were quadratic**: a whole-partition `SUM() OVER ()` or a running total recomputed its frame for
  every row (a 35,000-row partition hung the fuzzer); `RANGE` frames and `CUME_DIST` scanned peer groups per row. Frames
  are now accumulated incrementally and peer groups found once.
* **Wrong answers from the streaming join**, found by the fuzzer: a condition of an inner join's `ON` clause that names
  only the first table was dropped, and with outer joins an `ON` condition could land in the wrong stage (so a `FULL JOIN`
  kept rows an inner join's condition should remove). Also a subquery in an `ON` clause that refers to the joined
  table was evaluated too early.
* **Statements with absurd nesting crashed the process** (a stack overflow): 2,000-term chains of `+` or `OR`, hundreds of
  nested parentheses or subqueries. The parser now refuses them with error 8632, as SQL Server does.

From the tSQLt-style suites ([tsqlt.md](tsqlt.md)): `GO` inside a bracketed identifier containing an apostrophe, `SCHEMA_ID`
of user schemas, `sp_rename` not being transactional, `sys.parameters`, `TYPE_NAME` / `TYPE_ID`, `SQL_VARIANT_PROPERTY`,
`CREATE SYNONYM`, `WITH CHECK OPTION`, `NOT NULL` on alias types, `FOREIGN KEY REFERENCES` in a column constraint,
`SET LANGUAGE british`, `ERROR_PROCEDURE()`, the severity of error 2627, `DECLARE` inside a loop, `sp_executesql` ignoring
missing parameters and defaults, temp tables and `SET` options leaking out of procedures and dynamic SQL, `EXECUTE AS
USER` for a user without a login keeping the caller's permissions, `CAST(0xFF AS INT)`, `CAST(255 AS BINARY(2))`, and
`DATALENGTH` of Unicode strings.

## What is not covered

Compared with the real benchmarks: no official data generators or answer sets, no think and keying times, no
audited-run rules; TPC-DS has 24 of 99 queries and 15 of 24 tables, TPC-E a fraction of its schema. The fuzzer works on
small tables, so it exercises the planner and executors but not behaviour at memory-pressure scale, and its generator
does not yet cover DML that fails midway, `MERGE`, triggers or concurrent sessions (the concurrency tests are in
`tests/parallel.rs` and `tests/isolation.rs`; the OLTP benchmarks add invariants under concurrent load).
