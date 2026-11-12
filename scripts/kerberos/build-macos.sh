#!/bin/bash
# Build tsql-serve with Kerberos (GSS.framework) on macOS, and show how to run it.
#
# NOT BUILT OR TESTED: no Mac was available when this was written. The code it builds (src/tds/gssapi.rs) is the one that was
# tested on Linux against MIT Kerberos; macOS's GSS.framework exports the same GSSAPI functions. If the build fails, the
# by-hand declarations at the top of gssapi.rs are what to adjust.
#
# Needs: Xcode command line tools (xcode-select --install) and Rust (https://rustup.rs). Nothing else: GSS.framework is part
# of macOS.
set -e
cd "$(dirname "$0")/../.."
cargo build --release --features kerberos-macos
cat <<'EOF'

Built: target/release/tsql-serve

Run (the service key must be in a keytab that this user can read; create it with ktutil or take it from your KDC):
  target/release/tsql-serve ./data --kerberos \
      --kerberos-keytab /path/to/mssql.keytab \
      --kerberos-service MSSQLSvc/db.example.com:1433

Create the logins that may connect, e.g. in sqlcmd:
  CREATE LOGIN [EXAMPLE\alice] FROM WINDOWS;

Clients get a ticket with `kinit alice@EXAMPLE.COM` and connect with integrated security (sqlcmd -E, JDBC
integratedSecurity=true;authenticationScheme=JavaKerberos, the Microsoft ODBC driver with Trusted_Connection=yes).
EOF
