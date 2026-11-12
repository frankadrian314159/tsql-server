# The TDS server

`tsql-serve` exposes a [`Server`](../src/lib.rs) over TCP using the Tabular Data Stream protocol (version 7.4), so
SQL Server client libraries can connect:

```text
cargo run --release --bin tsql-serve -- [DATA_DIR] [--port 1433] [--bind 127.0.0.1] [--sa-password PW] [--pool-pages N]
```

* Without `DATA_DIR` the database is in memory. With it the database is durable exactly as in the shell.
* The `sa` login has no password until you give one (`--sa-password` or the `TSQL_SA_PASSWORD` environment variable,
  applied with `ALTER LOGIN` at start-up). A login without a password cannot connect over the network. Create other
  logins with `CREATE LOGIN` from a first connection as `sa`.
* The default bind address is loopback because the connection is **not encrypted** (see below).
* Only one process can open a data directory, so the TDS server and the shell cannot share one; connect to the
  server with `sqlcmd`/ADO.NET instead, or use the shell for local work while the server is stopped.

Connection strings need encryption off, e.g. `Server=127.0.0.1,1433;User Id=sa;Password=...;Encrypt=False`
(`sqlcmd -S 127.0.0.1,1433 -U sa -P ... -No`, `EncryptionLevel::NotSupported` in tiberius).

From Rust: `tsql_server::tds::Listener::bind(server.clone(), "127.0.0.1:0", Config::default())` starts the listener
on its own threads (one per client) and returns a handle with `local_addr()`, `wait()` and `shutdown()`.

## What is supported

| Feature | Notes |
| --- | --- |
| Pre-login, Login7 | SQL authentication (`connect_as`); the login's default database or the `database` in the connection string is selected; failures return 18456 (or 4060 for a missing database) |
| SQL batch | the text runs as one engine batch (`GO` is a client-side separator and is not recognised) |
| RPC | `sp_executesql` (its return status is the value of a `RETURN n` inside the statement), `sp_prepare`, `sp_execute`, `sp_prepexec`, `sp_unprepare` (by id or name), and calls to stored procedures by name, with typed parameters, OUTPUT parameters (`RETURNVALUE`) and the return status (`RETURNSTATUS`); `sp_reset_connection` rolls back an open transaction |
| Transaction manager | begin (with isolation level), commit, rollback, save point; transactions begun by SQL text are announced to the client with ENVCHANGE too |
| Results | multiple result sets, row counts (`DONE_COUNT`), `PRINT` and low-severity messages as INFO, errors with number, severity, state, procedure |
| Attention | acknowledged with `DONE_ATTN` (a running statement is not interrupted: the engine finishes it first) |
| Environment changes | database (`USE`), packet size, language, collation, transaction descriptors |
| Bulk load | `INSERT BULK` and the bulk-load message (`bcp`, `SqlBulkCopy`, the JDBC `SQLServerBulkCopy`, tiberius `bulk_insert`); see [Bulk load](#bulk-load) |
| Server cursors | `sp_cursoropen`, `sp_cursorprepare`, `sp_cursorexecute`, `sp_cursorprepexec`, `sp_cursorfetch`, `sp_cursoroption`, `sp_cursorclose`, `sp_cursorunprepare`; see [Server cursors](#server-cursors) |
| Azure AD, Kerberos | opt-in: access-token logins (`--aad-tenant`, `--aad-jwks`) and Kerberos logins (`--kerberos`, a build with `kerberos-windows`, `kerberos-linux` or `kerberos-macos`); see [authentication.md](authentication.md) |
| MARS | a client that asks for Multiple Active Result Sets (`MultipleActiveResultSets=True`) gets it; see [MARS](#mars) |
| Metadata queries | `SET FMTONLY ON` (describes a result without running it), `sp_tablecollations_100`, a procedure call without `EXEC` at the start of a batch |
| Types | bit, tinyint … bigint, decimal/numeric, money, smallmoney, float, real, char/varchar/text, nchar/nvarchar/ntext, binary/varbinary/image, timestamp, date, time, datetime, smalldatetime, datetime2, datetimeoffset, uniqueidentifier, xml, `sql_variant` (sent with its base type), `hierarchyid`, `geometry` and `geography` (as CLR user-defined types: the client gets the type name and the serialized bytes), and the legacy `text`, `ntext` and `image` (as the old text-pointer types); `(max)` types use partially length-prefixed values |
| Login options | the ODBC and OLE DB flags of Login7 make the session start with `SET ANSI_DEFAULTS ON`; `@@OPTIONS` follows the `SET` options. `char`, `varchar` and `text` parameters are read in the code page of the collation they carry (Windows code pages through `encoding_rs`, except 437 and 850, which read as 1252) |

## How it works

* The engine executes T-SQL text, so a typed RPC is translated into a short batch: parameter values become literals
  (dates in ISO 8601 so no precision is lost), OUTPUT parameters live in `@__oN` variables and the return status in
  `@__rc`, and a final `SELECT` reads them back. That result set is removed from the reply and sent as
  `RETURNVALUE` / `RETURNSTATUS` tokens.
* Result column types come from the engine's result metadata. When the engine only estimates a type (a literal or an
  expression), the column is widened to fit the longest value, and a `varchar` column whose text cannot be
  represented in code page 1252 is sent as `nvarchar`.
* Character columns carry the collation `SQL_Latin1_General_CP1_CI_AS`; `varchar` parameters are decoded as code
  page 1252.
* Each connection is served by its own thread and owns one engine session, so isolation levels, locks, blocking
  and deadlocks behave as they do between two shells. A panic in a connection thread closes its socket.
* Packets above the negotiated size are split; a message is limited to 256 MiB; a client that connects and sends
  nothing is dropped after 30 seconds, and an unrecognised packet type closes the connection.

## Encryption (TLS)

`tsql-serve` offers TLS by default, with a self-signed certificate generated at start-up (as SQL Server does when no
certificate is installed). Clients must then trust the server certificate (`TrustServerCertificate=True`,
`Encrypt=Optional`, tiberius `trust_cert()`), or you supply a real one:

```text
tsql-serve data --tls-cert server.pem --tls-key server.key      # PEM certificate chain and private key
tsql-serve data --force-encryption                             # refuse clients that cannot use TLS
tsql-serve data --no-tls                                       # do not offer TLS
tsql-serve data --tls13                                        # offer TLS 1.3 as well as 1.2
tsql-serve data --no-mars                                      # do not grant MARS
```

How the listener answers the client's pre-login `ENCRYPTION` request:

| client asks for | listener (optional) | listener (`--force-encryption`) |
|---|---|---|
| off | the login is encrypted, the rest of the session is plain | whole session encrypted |
| on / required | whole session encrypted | whole session encrypted |
| not supported | plain session | refused |

TLS 1.2 is offered by default (rustls). `--tls13` (or `TlsConfig::tls13(true)`) offers TLS 1.3 as well; a client that supports it then gets it (tiberius' rustls does). It is opt-in because several TDS 7.x clients cannot do it: the handshake is wrapped in pre-login packets, and the Microsoft JDBC driver (checked with 12.8 on JDK 25) sends the last flight of a TLS 1.3 handshake unwrapped and is dropped, while `System.Data.SqlClient` on Windows simply negotiates 1.2. As in TDS, the handshake travels inside pre-login packets and the records
follow as they are; with "login only" encryption TLS is dropped again after the Login7 message. From Rust:
`Config::with_tls(TlsConfig::from_pem_files(cert, key)?, Encryption::Required)`.

## Bulk load

`INSERT BULK table (columns) [WITH (...)]` names the target; the data message that follows (a COLMETADATA token, one ROW or
NBCROW token per row, a DONE token) is decoded as its packets arrive, however large it is, and the rows are inserted in chunks
of 5,000 rows (or 16 MiB). All the chunks of a load run in one transaction: the client's, if it has one open, otherwise one
the load begins and commits (or rolls back on the first error), so a failed load stores nothing. Values for identity columns
are not stored unless `KEEP_IDENTITY` is given; computed columns and `timestamp` columns are skipped. **Difference from SQL
Server:** constraints and triggers always apply (SQL Server skips them unless `CHECK_CONSTRAINTS` and `FIRE_TRIGGERS` are
given). The metadata calls the drivers make first (`SET FMTONLY ON SELECT * FROM t`, `sp_tablecollations_100`) are served.
A client that cancels a load part-way gets the rows rolled back and an attention acknowledgement.

Checked with tiberius, `System.Data.SqlClient.SqlBulkCopy` (data tables, readers, column mappings, internal transactions
that roll back) and the Microsoft JDBC `SQLServerBulkCopy`.

## Server cursors

The OLE DB, ODBC and JDBC drivers open server cursors (`sp_cursoropen` and friends) for scrollable result sets. A cursor is a
**snapshot**: opening it runs the statement and keeps its rows, and `sp_cursorfetch` pages through them (`FIRST`, `NEXT`,
`PREV`, `LAST`, `ABSOLUTE`, `RELATIVE`, `REFRESH`, `INFO`, with any number of rows). So a keyset or dynamic cursor is
converted to a static one and an updatable cursor to `READ_ONLY`, which the protocol allows (the call returns the type and
concurrency in force, the client sees them and the JDBC driver reports `TYPE_SCROLL_INSENSITIVE` or `CONCUR_READ_ONLY`).
Forward-only and fast-forward cursors refuse backward fetches; `AUTO_FETCH` and `AUTO_CLOSE` are honoured; positioned
updates and deletes (`sp_cursor`) are refused with error 16929. A fetched row ends with a hidden `rowstat` column, which
the Microsoft JDBC driver skips (the position of that column was settled by testing against this driver; it has not been
compared with a real SQL Server). Checked with the Microsoft JDBC driver (insensitive, sensitive and updatable result
sets, `absolute`, `relative`, `last`, `previous`, forward-only with a fetch size, prepared statements with parameters);
not checked with the OLE DB and ODBC drivers of Windows, which speak only TDS 7.1 and cannot connect.

## MARS

A client that asks for MARS in its pre-login message gets it (`tsql-serve --no-mars`, or `Config::mars = false`, turns it off).
After the login every packet travels inside a frame of the Session Multiplex Protocol (MS-SMP): the client opens logical
sessions (SYN), each carrying its own requests and replies, with a sequence number and a window per session. The sessions
share the connection's one engine state, as in SQL Server: the transaction, temporary tables, SET options.

**How it differs from SQL Server:** the engine runs one statement at a time, so requests are executed one after another in
the order they arrive, never in parallel, and the reply to each is queued for its session and sent as the client's window
allows, interleaved with the replies of the other sessions. A result is therefore held in memory while the client reads
another session (a reader over millions of rows that the client does not drain costs that much memory), and a statement
does not start until the one before it has finished running, though the client need not have read its result. The usual use
(read one result while issuing other commands, nested readers, several commands in flight) works and does not deadlock.
This is a change to the transport only: `src/tds/mars.rs` sits between the socket and the packet code, and a connection that
does not use MARS is unchanged. Checked with `System.Data.SqlClient` (`MultipleActiveResultSets=True`: a 6,000-row reader
kept open while a scalar command, DDL, DML and a second reader run; a transaction shared by the sessions; hundreds of
commands in a row) and by hand-written frames (`tests/tds_raw.rs`).

## Not supported

* Group-based logins and NTLM-only domains; see [authentication.md](authentication.md) for what Azure AD and Kerberos do cover.
* Query notifications and table-valued parameters (the engine has no table types).
* Keyset and dynamic cursors that see later changes, and updatable server cursors.
* `ANSI_NULLS OFF`, `QUOTED_IDENTIFIER OFF` and the like: the SET options are recorded (`@@OPTIONS`) but the engine always
  behaves as with them ON.
* `varchar` result columns are sent in code page 1252 (a value the code page lacks makes the column `nvarchar`).

## Tests

`tests/tds.rs` drives the listener with [tiberius](https://crates.io/crates/tiberius), an independent client
(types, parameters, errors, transactions between connections, permissions, large results, concurrent clients).
`tests/tds_raw.rs` speaks the protocol by hand for what high-level clients rarely expose: OUTPUT parameters and return
status, prepared statements, server cursors, MARS frames, transaction-manager requests, attention, bad logins and garbage input.
`tests/tds_bulk.rs` covers bulk load, `SET FMTONLY` and `sp_tablecollations_100`; `tests/tds_tls.rs` covers TLS 1.2 and 1.3. The encoders and
decoders also have unit tests (`src/tds/wire.rs`). The listener was also checked by hand with
`System.Data.SqlClient` (parameters, transactions, a stored procedure with OUTPUT and return value, errors, `SqlBulkCopy`) and with the Microsoft JDBC driver 12.8 (scrollable result sets, prepared cursors, bulk copy, `sql_variant`, `text`).

## Clients

The Microsoft drivers (JDBC, ODBC, `System.Data.SqlClient`) and tiberius work. **jTDS** (DBeaver's "old driver") does not: it sends a LOGIN7 packet without the PRELOGIN handshake and speaks TDS 7.0/7.1, so the server closes the connection. When a connection is dropped before login completes, `tsql-serve` prints the reason on stderr (`tsql-serve: closing a connection during login: ...` or `login handshake failed: ...`).

To see what a client sends, set `TSQL_TDS_TRACE=1` before starting `tsql-serve`: it then logs on stderr each login step, every request packet, the SQL text of each batch and RPC (as a quoted string, so stray characters show), and each reply size.

GUI clients such as DBeaver, SSMS and Azure Data Studio build their navigators from the `sys` catalog views. Those they commonly use are provided (`sys.schemas`, `objects` / `all_objects`, `tables`, `views`, `columns`, `types`, `indexes`, `index_columns`, `foreign_keys`, `foreign_key_columns`, `default_constraints`, `check_constraints`, `computed_columns`, `key_constraints`, `synonyms`, `sql_modules`, `partitions`, `dm_db_partition_stats`, `database_files`, `database_principals`, ... and the `INFORMATION_SCHEMA` views), as are `OBJECT_DEFINITION`, `OBJECT_ID` (also for triggers), `SERVERPROPERTY` and `sp_help`. `sys.extended_properties, sys.computed_columns` exists but is always empty (descriptions are not kept). If a client reports `Invalid object name 'sys.something'`, that view is missing: tell us which.
