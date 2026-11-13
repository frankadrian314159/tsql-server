# JDBC checks of the TDS listener

Single-file programs for the Microsoft JDBC driver (`mssql-jdbc-12.8.1.jre11.jar`, from Maven Central), used to check server
cursors and table-valued parameters against an independent client. Start `tsql-serve DATA --port 14641 --no-tls --sa-password
'Str0ng!pw'`, then:

```text
java -cp mssql-jdbc.jar Cursors.java "jdbc:sqlserver://127.0.0.1:14641;user=sa;password=Str0ng!pw;encrypt=false"
java -cp mssql-jdbc.jar TableValuedParameters.java "jdbc:sqlserver://127.0.0.1:14641;user=sa;password=Str0ng!pw;encrypt=false"
```

Each prints what it did and ends with `done`; use a fresh data directory for each run (they create tables).
