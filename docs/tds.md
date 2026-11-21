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
| Table-valued parameters | `TVP` parameters of procedures and `sp_executesql`; see [Table-valued parameters](#table-valued-parameters) |
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

The OLE DB, ODBC and JDBC drivers open server cursors (`sp_cursoropen` and friends) for scrollable result sets, and for result
sets that can be changed. `sp_cursorfetch` pages through the rows (`FIRST`, `NEXT`, `PREV`, `LAST`, `ABSOLUTE`, `RELATIVE`,
`REFRESH`, `INFO`, with any number of rows). A fetched row ends with a hidden `rowstat` column (1: fetched, 2: deleted), which the
Microsoft JDBC driver skips (its position was settled by testing against this driver; it has not been compared with a real SQL
Server). There are two kinds of cursor:

* **Keyset cursors** (asked for as KEYSET or DYNAMIC, or as FORWARD_ONLY with updatable concurrency) over a statement that is
  a plain read of one table (`SELECT columns | * FROM table [WHERE ...] [ORDER BY ...]`, no join, grouping, `DISTINCT` or
  expression columns) that returns the table's primary key (or a unique constraint of NOT NULL columns). The cursor keeps
  the key of each row: the membership is fixed when it opens, and **every fetch reads the rows of the block again by key**, so
  it shows what other sessions (and the cursor itself) changed; a row deleted since comes back with `rowstat` 2. Rows other
  sessions inserted are not in it, so a DYNAMIC cursor is served as a keyset one and the call says so. Such a cursor can
  be **updatable**: `sp_cursor` updates (operation 33), deletes (34), inserts (4) and refreshes (1) the row at a position. The
  update and delete check optimistically that the row is as the cursor last fetched it (error 16934 otherwise, 16933 for a row
  that is gone); an inserted row joins the cursor (its key is its given key columns, or the identity value it received). The
  result columns are flagged updatable and the browse information (`TABNAME`, `COLINFO`) is sent, which the JDBC driver needs.
* **Snapshot cursors**: anything else (STATIC, FAST_FORWARD, a read-only request that is not asked as keyset, a statement that
  is not a plain read of one table with its key). Opening it runs the statement and keeps the rows; it is read-only, and the call
  returns the type (static, or forward-only) and concurrency (`READ_ONLY`) in force, which the protocol allows.

Forward-only and fast-forward cursors refuse backward fetches; `AUTO_FETCH` and `AUTO_CLOSE` are honoured. Rows are read
again in the cursor's own session and transaction, so a keyset cursor inside a transaction sees that transaction's changes.
Checked with the Microsoft JDBC driver: insensitive, sensitive and updatable result sets (`updateRow`, `deleteRow`,
`insertRow`, `refreshRow`, `rowDeleted`, a change made by another connection, forward-only updatable, a grouped query staying
read-only), `absolute`, `relative`, `last`, `previous`, prepared statements with parameters (`scripts/jdbc/`), and by
hand-written requests (`tests/tds_raw.rs`). Not checked with the OLE DB and ODBC drivers of Windows, which speak only TDS 7.1
and cannot connect.

## Table-valued parameters

A parameter of type `TVP` (the `Structured` type of ADO.NET, `setStructured` of JDBC) is read with its type name and rows and
passed to the stored procedure or to `sp_executesql` (`@p dbo.MyType READONLY`) as a table variable of that type, filled with the
rows. The table type must exist (`CREATE TYPE dbo.MyType AS TABLE (...)`). The rows are inserted by a statement the server
builds, 500 to a statement; an empty or NULL table passes an empty table. A column flagged as default in the TVP metadata
is not treated specially (the client sends every column). `sp_executesql` accepts table-type parameters in its parameter list
in the engine too (`EXEC sp_executesql N'...', N'@p dbo.T READONLY', @p = @tablevar`), and an unnamed argument after a named
one (as the JDBC driver sends them) is bound by position. Checked with `System.Data.SqlClient` (a 1,200-row table with NULLs, a
stored procedure, a text command, an empty table) and the Microsoft JDBC driver (a call and an `EXEC`), and by hand-written
requests.

## Interceptors (the control node of a cluster)

`Server::set_interceptor` installs a factory of per-connection `Interceptor`s: for each batch a connection is asked to run, the interceptor either runs it and reports to the sink, or declines and the engine runs it as usual (`Server::connect_internal` makes a connection that is never intercepted). For pre-parsed statements (what bulk load runs) the interceptor has `handle_stmts`. The listener is unaware: the control node of the distributed edition serves its own engine over TDS and an interceptor sends the batches that touch distributed tables to the cluster (see [distributed.md](distributed.md)). Without an interceptor nothing changes.

## MARS

A client that asks for MARS in its pre-login message gets it (`tsql-serve --no-mars`, or `Config::mars = false`, turns it off).
After the login every packet travels inside a frame of the Session Multiplex Protocol (MS-SMP): the client opens logical
sessions (SYN), each carrying its own requests and replies, with a sequence number and a window per session. The sessions
share the connection's one engine state, as in SQL Server: the transaction, temporary tables, SET options.

**How it differs from SQL Server:** requests that **only read** (a batch of `SELECT`s, or `sp_executesql` / `sp_execute` of one,
with no OUTPUT parameter, no `@@` variable, sequence, `INTO` or locking hint, and no open transaction) run in parallel, each
on a worker thread over a fork of the connection's session (same login, database, temporary tables and settings), so a long
query on one logical session does not hold up a short one on another (`src/tds/mars_par.rs`). Any other request (DML, DDL,
`EXEC`, `SET`, transaction control, cursors, bulk load) waits for the running ones to finish, then runs on the session itself,
so it is complete before the next starts; while a transaction is open every request runs serially. Replies of one logical
session stay in order; those of different sessions are interleaved as they finish, within the client's windows. A reply is
held in memory until the client's window lets it go, so a reader over millions of rows that the client does not drain costs
that much memory. The usual use
(read one result while issuing other commands, nested readers, several commands in flight) works and does not deadlock.
This is mostly a change to the transport: `src/tds/mars.rs` sits between the socket and the packet code, and a connection that
does not use MARS is unchanged. Checked with `System.Data.SqlClient` (`MultipleActiveResultSets=True`: a 6,000-row reader
kept open while a scalar command, DDL, DML and a second reader run; a transaction shared by the sessions; hundreds of
commands in a row) and by hand-written frames (`tests/tds_raw.rs`).

## SSMS

SQL Server Management Studio connects and lists the databases. At connect and in Object Explorer it asks for many functions and
views (`SERVERPROPERTY` values that must not be NULL, `CONNECTIONPROPERTY`, `HAS_DBACCESS`, `sys.dm_os_host_info`,
`sys.availability_replicas`, `sys.database_mirroring` and others; those about features the server does not have are empty), runs
`INSERT ... EXEC` and `IF` batches, and sends a big statement as an `NTEXT` parameter (a length and the bytes, without the text pointer
a column value has). When a client fails against the server, set `TSQL_TDS_TRACE=1`: the server prints each request (the batch or
the RPC and the batch it becomes) and the errors, to its standard error.

Scripting a table and View Dependencies send long batches that need the catalog to look like SQL Server's: `sys.sql_expression_dependencies`
(from the definitions of views, procedures, functions and triggers), `sys.dm_sql_referencing_entities`, the property columns of `sys.tables`,
`sys.columns`, `sys.indexes` and `sys.databases`, `COLLATE DATABASE_DEFAULT`, `UPDATE ... FROM` a table that does not name the target,
and variables that are declared inside a block and used after it. A result column that SMO reads as a byte or a boolean must be sent as one:
`ISNULL(column, value)` has the type of the column, and arithmetic of an int with a binary literal (`(@@microsoftversion / 0x1000000) & 0xff`) is an int.
On a cluster, a client's temporary tables are handled as [distributed.md](distributed.md) says, and View Dependencies works.

The folders of Object Explorer (Tables, Views and their Columns, Synonyms, Programmability, Storage, Security, Memory Optimization Advisor,
Policy Management) each run a catalog query; one that names a column or view the server lacks fails and SSMS shows an empty folder with no message.
The columns of a view are found by running its query for no rows (once per connection and view text). `msdb.dbo.syspolicy_configuration`
exists with a new server's values; Policy-Based Management is not implemented (no policies). The Memory Optimization Advisor opens but cannot
continue: there is no In-Memory OLTP.
The folder picker behind New Database's file "..." button reads the server's drives from `sys.dm_os_enumerate_fixed_drives` (announced in `sys.all_objects`) and its folders with `xp_subdirs`, `xp_dirtree` and `xp_fileexist`, which list the server machine's own disks; `xp_fixeddrives` gives the older drive list. The Locate Folder dialog lists one level at a time with `sys.dm_os_enumerate_filesystem(path, pattern)` (`*` and `?` match; `level` is always 0) and checks a name with `sys.dm_os_file_exists(path)`; sysadmin only.
`CREATE DATABASE` accepts the script SSMS's New Database dialog writes (`CONTAINMENT`, `ON PRIMARY` and `FILEGROUP` file lists, `LOG ON`, `WITH LEDGER = OFF`); no file is created, since a database here lives in the catalog, but each `FILENAME` is held to what SQL Server demands: its folder must exist (else error 5133, "Directory lookup for the file ... failed") and nothing may be at the path already, a folder included (else 5170, "Cannot create file ... because it already exists"); sizes and filegroups are ignored, and the files of `FOR ATTACH` are not checked. (SQL Server follows each of these errors with 1802, "CREATE DATABASE failed"; this server sends only the first.)
The `ALTER DATABASE ... SET` statements the dialog follows with are accepted too: an option may carry a parenthesized list (`AUTO_CREATE_STATISTICS ON(INCREMENTAL = OFF)`), a bare value (`PAGE_VERIFY CHECKSUM`) or a unit (`TARGET_RECOVERY_TIME = 60 SECONDS`); only the options the engine has (recovery model, snapshot isolation, collation, ...) take effect.
The Delete dialog's "delete backup history" box calls `msdb.dbo.sp_delete_database_backuphistory`, which deletes the database's history.
The Restore dialog and the Databases node read the backup history of `msdb` (`backupset`, also as `msdb..backupset`, `backupmediafamily`, `backupmediaset`, `backupfile`, `backupfilegroup`, `restorehistory`, `restorefile`, `restorefilegroup`, `suspect_pages`). Each backup and restore is recorded there (see [backup.md](backup.md)); before the first backup the tables read as empty. `msdb.dbo.sp_delete_database_backuphistory` really deletes the history.
"Close existing connections" counts the database's connections in `master.dbo.sysprocesses` (a compatibility view of the sessions) and runs `ALTER DATABASE ... SET SINGLE_USER WITH ROLLBACK IMMEDIATE`, which is accepted (the option does not disconnect anyone).
The View Designer reads a view's text from `syscomments` (in pieces of 4000 characters, as SQL Server keeps it) and the table designer
reads a table's relationships with `EXEC sp_fkeys`; a designer that gets an error for one of them asks again, so that the cursor
flickers and nothing appears. On a cluster a system procedure (`sp_...`, `xp_...`) that is given the name of a table is the engine's, not
the cluster's: the name is a value, and the engine holds the schema of every table.

**Reporting Services** (SSRS) makes its catalog databases (`ReportServer`, `ReportServerTempDB`) with scripts that use the older parts of SQL Server, which the server and a cluster's control node take: `sp_addrole`/`sp_droprole`, `sp_tableoption`, `ALTER TABLE ... WITH NOCHECK ADD`, a comma before the `)` of `CREATE TABLE`, table hints on the target of `UPDATE`, `DELETE` and `INSERT`, the compatibility views `syscolumns` and `sysindexes`, cursors in triggers, and `GRANT` on the SQL Server Agent objects of `master` and `msdb` (accepted, with no effect: there is no Agent). `UPDATE t SET col.WRITE(expression, offset, length)` changes a part of a `varchar(max)`, `nvarchar(max)` or `varbinary(max)` value (a NULL offset appends, a NULL length goes to the end of the value, a NULL expression only deletes). `UPDATETEXT`, `READTEXT` and `WRITETEXT` are read, so that a procedure that has one can be made, and raise error 50100 when run: there are no text pointers, so the writes SSRS makes to its chunk tables with them do not work.

## Not supported

* Group-based logins and NTLM-only domains; see [authentication.md](authentication.md) for what Azure AD and Kerberos do cover.
* Query notifications.
* DYNAMIC cursors that see rows inserted by others (they are served as keyset cursors), and updatable cursors over joins, views, or statements that do not return a key.
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

GUI clients such as DBeaver, SSMS and Azure Data Studio build their navigators from the `sys` catalog views. Those they commonly use are provided (`sys.schemas`, `objects` / `all_objects`, `tables`, `views`, `columns`, `types`, `indexes`, `index_columns`, `foreign_keys`, `foreign_key_columns`, `default_constraints`, `check_constraints`, `computed_columns`, `key_constraints`, `synonyms`, `sql_modules`, `partitions`, `dm_db_partition_stats`, `database_files`, `database_principals`, ... and the `INFORMATION_SCHEMA` views: `TABLES`, `COLUMNS`, `VIEWS`, `ROUTINES`, `PARAMETERS`, `SCHEMATA`, `TABLE_CONSTRAINTS`, `KEY_COLUMN_USAGE`, `REFERENTIAL_CONSTRAINTS`, `CONSTRAINT_COLUMN_USAGE`, `CONSTRAINT_TABLE_USAGE`, `CHECK_CONSTRAINTS`, `VIEW_TABLE_USAGE`, `VIEW_COLUMN_USAGE`, `DOMAINS`, `SEQUENCES`; `ROUTINE_COLUMNS`, `TABLE_PRIVILEGES` and `COLUMN_PRIVILEGES` exist but are empty, and `VIEW_COLUMN_USAGE` counts a column when its name is a word of the view's text), as are `OBJECT_DEFINITION`, `OBJECT_ID` (also for triggers), `SERVERPROPERTY` and `sp_help`. `sys.extended_properties, sys.computed_columns` exists but is always empty (descriptions are not kept). If a client reports `Invalid object name 'sys.something'`, that view is missing: tell us which.
