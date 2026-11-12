#!/bin/bash
# A throw-away MIT Kerberos realm for testing Kerberos logins on Linux (also WSL): realm TSQL.TEST with the users alice and
# carol (password "userpw") and the service MSSQLSvc/localhost:14680, whose key goes to /root/mssql.keytab.
# Needs root and: apt-get install krb5-kdc krb5-admin-server krb5-user libkrb5-dev
# WARNING: it overwrites /etc/krb5.conf and creates a realm database; use a scratch machine or WSL distribution.
set -e
cat > /etc/krb5.conf <<'EOF'
[libdefaults]
 default_realm = TSQL.TEST
 dns_lookup_kdc = false
 dns_lookup_realm = false
 rdns = false
 forwardable = true

[realms]
 TSQL.TEST = {
  kdc = localhost:88
  admin_server = localhost
 }

[domain_realm]
 localhost = TSQL.TEST
EOF
if [ ! -f /var/lib/krb5kdc/principal ]; then
  kdb5_util create -s -P masterpw -r TSQL.TEST
fi
for p in alice carol MSSQLSvc/localhost:14680; do kadmin.local -q "delprinc -force $p" >/dev/null 2>&1 || true; done
kadmin.local -q "addprinc -pw userpw alice"
kadmin.local -q "addprinc -pw userpw carol"
kadmin.local -q "addprinc -randkey MSSQLSvc/localhost:14680"
rm -f /root/mssql.keytab
kadmin.local -q "ktadd -k /root/mssql.keytab MSSQLSvc/localhost:14680"
pgrep -x krb5kdc >/dev/null || { krb5kdc; sleep 1; }
echo "KDC running; get a ticket with: echo userpw | kinit alice"
