# Administration, hints and observability

## Sessions, `KILL`, `WAITFOR`

Every connection is registered with the server (`coord::Core`). `sp_who`, `sp_who2`, `sys.dm_exec_sessions` and
`sys.dm_exec_requests` list them (login, host, program, database, status `running`/`sleeping`, isolation level, last
batch time, batch count, current command).

```sql
KILL 57;                        -- sysadmin only; not your own session (6104); unknown id is 6106
WAITFOR DELAY '00:00:05';       -- really sleeps, holding no catalog guard (other sessions run), keeps the transaction's locks
WAITFOR TIME '23:30:00';        -- until that time of day (tomorrow if it has passed)
```

`KILL` ends the target's current statement with error 596 and rolls its transaction back. A session blocked on a lock or in
`WAITFOR` wakes at once; a session idle between statements is stopped by the TDS listener (it checks while waiting for the
client's next request), which rolls back and closes the connection. An embedded `Connection` that is idle keeps its
locks until its next statement, because nothing is listening on its behalf. `WAITFOR (RECEIVE ...)` is not supported.

## `sp_configure`

`EXEC sp_configure` lists options, `EXEC sp_configure 'name'` shows one (a unique prefix is enough), `EXEC sp_configure 'name', value`
sets it and `RECONFIGURE` makes it the running value (`config_value` vs `run_value`, also `sys.configurations`). Advanced options
are hidden until `'show advanced options'` is 1. The options are recorded and reported; they do not change engine behaviour
(there is no parallelism or memory governor to configure; memory use is set with `Server::set_work_mem`).

## Table hints and query hints

`FROM t WITH (hint, ...)` is parsed and applied per table:

| Hint | Effect |
|---|---|
| `NOLOCK`, `READUNCOMMITTED` | read uncommitted rows, take no locks and do not wait |
| `READCOMMITTED`, `REPEATABLEREAD`, `SERIALIZABLE`, `HOLDLOCK` | isolation level for this table (`HOLDLOCK` = `SERIALIZABLE`, with key-range locks) |
| `UPDLOCK`, `XLOCK` | rows read are locked exclusively and stay locked to the end of the transaction |
| `TABLOCK`, `TABLOCKX` | shared / exclusive table lock |
| `READPAST` | skip rows other transactions have uncommitted changes in |
| `NOWAIT` | a lock that cannot be granted at once is error 1222 |
| `FORCESCAN` | read the whole table (no index) |
| `ROWLOCK`, `PAGLOCK`, `NOEXPAND`, `INDEX(..)`, `FORCESEEK`, `KEEPIDENTITY`, ... | accepted, no effect |

An unknown hint is error 321. `OPTION (...)` accepts `HASH JOIN` and `MERGE JOIN` (no index nested-loop joins), `LOOP JOIN` (every
pair of rows is tested against the whole condition), `FORCE ORDER` (join in the written order), `MAXRECURSION n`, and accepts
`MAXDOP`, `RECOMPILE`, `FAST n`, `OPTIMIZE FOR`, ... without effect. Hints never change a query's result, only how it runs.

## Plan cache and statistics

The parsed form of every batch is cached per connection thread, keyed by the exact text (512 entries, least used evicted),
so a statement a client sends repeatedly is parsed once; `sys.dm_exec_cached_plans` lists this thread's entries with their use
counts and `DBCC FREEPROCCACHE` empties the cache. **Execution plans are not cached**: the optimizer's choices depend on the
data, and there is no parameter sniffing. `sys.dm_exec_query_stats` is server-wide: per batch text, execution count, total / last /
maximum elapsed microseconds, rows, first and last execution time.
