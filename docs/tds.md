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
| RPC | `sp_executesql`, `sp_prepare`, `sp_execute`, `sp_prepexec`, `sp_unprepare` (by id or name), and calls to stored procedures by name, with typed parameters, OUTPUT parameters (`RETURNVALUE`) and the return status (`RETURNSTATUS`); `sp_reset_connection` rolls back an open transaction |
| Transaction manager | begin (with isolation level), commit, rollback, save point; transactions begun by SQL text are announced to the client with ENVCHANGE too |
| Results | multiple result sets, row counts (`DONE_COUNT`), `PRINT` and low-severity messages as INFO, errors with number, severity, state, procedure |
| Attention | acknowledged with `DONE_ATTN` (a running statement is not interrupted: the engine finishes it first) |
| Environment changes | database (`USE`), packet size, language, collation, transaction descriptors |
| Types | bit, tinyint … bigint, decimal/numeric, money, smallmoney, float, real, char/varchar/text, nchar/nvarchar/ntext, binary/varbinary/image, timestamp, date, time, datetime, smalldatetime, datetime2, datetimeoffset, uniqueidentifier, xml; `(max)` types use partially length-prefixed values |

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
```

How the listener answers the client's pre-login `ENCRYPTION` request:

| client asks for | listener (optional) | listener (`--force-encryption`) |
|---|---|---|
| off | the login is encrypted, the rest of the session is plain | whole session encrypted |
| on / required | whole session encrypted | whole session encrypted |
| not supported | plain session | refused |

TLS 1.2 and 1.3 are supported (rustls). As in TDS, the handshake travels inside pre-login packets and the records
follow as they are; with "login only" encryption TLS is dropped again after the Login7 message. From Rust:
`Config::with_tls(TlsConfig::from_pem_files(cert, key)?, Encryption::Required)`.

## Not supported

* Integrated (Windows/Kerberos/NTLM) and Azure AD authentication.
* Bulk load (`bcp`, `SqlBulkCopy`), server-side cursors (`sp_cursor*`, used by some JDBC settings), MARS,
  query notifications, table-valued parameters, and `sql_variant` and `hierarchyid`/spatial types on the wire.
* Session options sent in the Login7 flags are ignored; `varchar` parameters are always read as code page 1252.
* The return status of `sp_executesql` is the value of a `RETURN n` inside the statement (0 when there is none).

## Tests

`tests/tds.rs` drives the listener with [tiberius](https://crates.io/crates/tiberius), an independent client
(types, parameters, errors, transactions between connections, permissions, large results, concurrent clients).
`tests/tds_raw.rs` speaks the protocol by hand for what high-level clients rarely expose: OUTPUT parameters and return
status, prepared statements, transaction-manager requests, attention, bad logins and garbage input. The encoders and
decoders also have unit tests (`src/tds/wire.rs`). The listener was also checked by hand with
`System.Data.SqlClient` (parameters, transactions, a stored procedure with OUTPUT and return value, errors).
