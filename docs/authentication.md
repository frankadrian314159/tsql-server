# Authentication beyond SQL logins

`tsql-serve` always accepts SQL logins (a name and a password). Two other kinds of login are available, both off until you
turn them on:

| | Who vouches for the client | Needs |
| --- | --- | --- |
| **Azure AD** (Entra ID) | a signed access token | a JWKS file with the tenant's signing keys |
| **Kerberos** (integrated authentication) | the Kerberos realm / Active Directory domain | a build with the feature for your platform |

Neither kind of identity has a password in this server. A login that may authenticate this way is created ahead of time,
as in SQL Server:

```sql
CREATE LOGIN [alice@contoso.com] FROM EXTERNAL PROVIDER;   -- Azure AD (type E in sys.server_principals)
CREATE LOGIN [CORP\dana] FROM WINDOWS;                     -- Kerberos (type U)
ALTER SERVER ROLE sysadmin ADD MEMBER [CORP\dana];         -- permissions are granted to the login as usual
```

A client that authenticates but has no such login is refused with error 18456, like a wrong password. An external login
cannot be used with a password, and a SQL login cannot be entered with a token or a ticket. Embedded Rust code can use
`Server::connect_external(&["CORP\\dana", "dana@CORP.EXAMPLE"])`, which takes the names an identity can be known by, most
specific first.

## Azure AD access tokens

```text
tsql-serve data --aad-tenant 11111111-2222-3333-4444-555555555555 --aad-jwks keys.json
```

* The client sends a token for the resource `https://database.windows.net/` (what `Authentication=Active Directory ...`
  modes, `SqlConnection.AccessToken`, tiberius' `AuthMethod::aad_token` and the JDBC `accessToken` property do). The server
  checks the RS256 signature against the key set, the issuer (`https://login.microsoftonline.com/<tenant>/v2.0` or
  `https://sts.windows.net/<tenant>/`), the audience (`--aad-audience URI`, repeatable; default
  `https://database.windows.net/`), and the lifetime (`exp`, `nbf`, five minutes of clock skew).
* The login is looked up by the `upn`, `preferred_username`, `unique_name`, `email`, `appid`, `azp` or `oid` claim, in that
  order of preference. Service principals therefore use their application id as the login name.
* **The signing keys are a file you supply**: download `https://login.microsoftonline.com/<tenant>/discovery/v2.0/keys` into
  `keys.json`. The server re-reads the file at every login, so a cron job that refreshes it follows Microsoft's key rotation.
  The server makes no outgoing connections of its own.
* Clients that fetch the token themselves after being told where (`ActiveDirectoryPassword`, `Interactive`, ...) are served
  too: the server answers the FEDAUTH login with the token service address and reads the token from the FEDAUTH token
  message. Not covered: group claims (a login is per user, not per group), and conditional-access or other policy that Azure
  SQL enforces on its side.
* Azure AD logins need an encrypted connection to be safe: use TLS (the default; see [tds.md](tds.md)).

Checked with tiberius and `System.Data.SqlClient` (`SqlConnection.AccessToken`, over TLS) using tokens signed with a throw-away
key (`tests/tds_auth.rs`: accepted; expired, not yet valid, wrong audience, wrong tenant, forged body, unknown user, SQL login
named in a token, no Azure AD configured). **Not** checked against a real Azure AD tenant, and the `ActiveDirectory*` flows
that fetch a token themselves are implemented from the protocol description only.

## Kerberos

Kerberos is compiled in only when you ask for it, one cargo feature per platform. Without the feature `--kerberos` is
refused at start-up, and a client asking for integrated authentication is told so.

| Platform | Build | Backend | Needs |
| --- | --- | --- | --- |
| Windows | `cargo build --release --features kerberos-windows` | SSPI (`Negotiate`) | nothing extra |
| Linux (also WSL) | `cargo build --release --features kerberos-linux` | GSSAPI (MIT `libgssapi_krb5`) | `libkrb5-dev` (Debian, Ubuntu) or `krb5-devel` (Fedora, RHEL) |
| macOS | `cargo build --release --features kerberos-macos` | `GSS.framework` | nothing extra |

Each feature builds only for its own platform (a mismatch is a compile error). Run with:

```text
tsql-serve data --kerberos [--kerberos-service MSSQLSvc/host.example.com:1433] [--kerberos-keytab FILE] [--allow-ntlm]
```

The identity reaches the engine as `DOMAIN\user` (the realm's first label for a `user@REALM.EXAMPLE` principal) or as the
whole principal, whichever names a login (`CREATE LOGIN [EXAMPLE\alice] FROM WINDOWS`).

### Windows

SSPI uses the machine's own configuration: run `tsql-serve` as a domain account (or Network Service), register the service
principal for it (`setspn -S MSSQLSvc/host.example.com:1433 DOMAIN\serviceaccount`) and clients connect with
`Integrated Security=SSPI`. `Negotiate` falls back to **NTLM** when Kerberos is not possible (a workgroup machine, a client
that cannot find the service principal); that is refused unless you pass `--allow-ntlm`, so that "Kerberos" means Kerberos.
`--kerberos-service` and `--kerberos-keytab` are not used.

Tested by hand on a Windows 11 machine in a workgroup: the full SSPI exchange runs with
`System.Data.SqlClient` (`Integrated Security=SSPI`) and logs in as the Windows user with `--allow-ntlm`, and is refused with
the message that NTLM is not Kerberos without it. **Real Kerberos was not tested on Windows**: there is no domain controller
here. The code path after the exchange is the same as the one tested under Linux.

### Linux and WSL

```text
sudo apt-get install libkrb5-dev                    # to build; clients and the service need libgssapi-krb5-2 at run time
cargo build --release --features kerberos-linux
tsql-serve data --kerberos --kerberos-keytab /etc/mssql.keytab --kerberos-service MSSQLSvc/db.example.com:1433
```

The keytab holds the key of the service principal (`ktadd -k /etc/mssql.keytab MSSQLSvc/db.example.com:1433` in `kadmin`, or
`ktpass` on a domain controller for Active Directory). Without `--kerberos-service` every principal in the keytab is accepted.
Clients get a ticket with `kinit` and connect with integrated security (`sqlcmd -E`, the Microsoft ODBC driver with
`Trusted_Connection=yes`, JDBC with `integratedSecurity=true;authenticationScheme=JavaKerberos`). Under WSL the same commands
apply; build in the Linux file system (not under `/mnt/c`) for speed.

Tested under WSL 2 (Ubuntu 24.04, MIT Kerberos 1.20, OpenJDK 21) with a local KDC and the Microsoft JDBC driver 12.8 as the
client: `sudo bash scripts/kerberos/setup-kdc.sh` makes the realm `TSQL.TEST` with users `alice` and `carol` and the service
`MSSQLSvc/localhost:14680`, and `sudo JDBC_JAR=mssql-jdbc.jar bash scripts/kerberos/test-linux.sh` builds with
`kerberos-linux`, starts the server and checks that **alice** logs in (as `TSQL\alice`, twice), **carol** (a valid ticket but
no login) is refused with 18456, and a client with **no ticket** cannot start the exchange. The unit and integration tests
(`cargo test --features kerberos-linux`) also pass under Linux.

### macOS

`kerberos-macos` links `GSS.framework`, which exports the same GSSAPI functions as MIT Kerberos, so it uses the same code as
Linux (`src/tds/gssapi.rs`). The commands are those of Linux with the macOS feature:

```text
cargo build --release --features kerberos-macos
tsql-serve data --kerberos --kerberos-keytab /etc/mssql.keytab --kerberos-service MSSQLSvc/db.example.com:1433
```

macOS keeps tickets in its own credential cache (`kinit` on the client, `klist` to look). **This was not built or tested**: no
Mac was available. Expect the first build to need small fixes (for instance, if the framework's header defines a type
differently from MIT's, the by-hand declarations in `gssapi.rs` are what to adjust).

## What is not covered

* Authorization from directory groups (`CREATE LOGIN [DOMAIN\Group] FROM WINDOWS` creates a login named so, but group
  membership is not read from the ticket, so it never matches a user).
* Delegation, channel binding and extended protection.
* Kerberos through the NTLM-only SSPI clients of Windows 7 and older, and TDS-level `Authentication=ActiveDirectoryIntegrated`
  (which obtains an Azure AD token with the Windows account: it works as an Azure AD token login, so the login must exist as
  an external login).
