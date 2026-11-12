#!/bin/bash
# End-to-end Kerberos test on Linux (also WSL), as root, after scripts/kerberos/setup-kdc.sh:
#   1. builds tsql-serve with --features kerberos-linux
#   2. starts it with the keytab, creates the login [TSQL\alice] FROM WINDOWS
#   3. logs in with the Microsoft JDBC driver (JavaKerberos) as alice (works), carol (no login: refused), and with no ticket
# Needs a JDK or JRE 11+ and the driver jar: JDBC_JAR=/path/to/mssql-jdbc.jar (default ./mssql-jdbc.jar), e.g. from
# https://repo1.maven.org/maven2/com/microsoft/sqlserver/mssql-jdbc/12.8.1.jre11/mssql-jdbc-12.8.1.jre11.jar
set -u
cd "$(dirname "$0")/../.."
JDBC_JAR=${JDBC_JAR:-./mssql-jdbc.jar}
PORT=14680
cargo build --bin tsql-serve --features kerberos-linux || exit 1
run() {   # run <ticket-user | none>
  kdestroy -A >/dev/null 2>&1
  [ "$1" != "none" ] && echo userpw | kinit "$1" >/dev/null
  rm -rf /tmp/kdata
  ./target/debug/tsql-serve /tmp/kdata --port $PORT --sa-password 'Str0ng!pw' --no-tls --kerberos \
    --kerberos-keytab /root/mssql.keytab --kerberos-service MSSQLSvc/localhost:$PORT > /tmp/ksrv.log 2>&1 &
  PID=$!
  sleep 3
  echo "== ticket for: $1"
  timeout 120 java -cp "$JDBC_JAR" scripts/kerberos/KerberosLogin.java $PORT 2>&1 | grep -v '^\s*at ' | cut -c1-200
  kill $PID 2>/dev/null
  wait $PID 2>/dev/null
}
run alice    # expect: kerberos login as TSQL\alice
run carol    # expect: Login failed for user 'TSQL\carol'
run none     # expect: Kerberos Login failed (no ticket)
