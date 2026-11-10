# System-versioned (temporal) tables

```sql
CREATE TABLE emp (
    id  INT PRIMARY KEY,
    sal INT,
    vf  DATETIME2 GENERATED ALWAYS AS ROW START NOT NULL,   -- add HIDDEN to keep it out of SELECT *
    vt  DATETIME2 GENERATED ALWAYS AS ROW END   NOT NULL,
    PERIOD FOR SYSTEM_TIME (vf, vt)
) WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = dbo.emp_hist));

SELECT * FROM emp FOR SYSTEM_TIME AS OF '2026-01-01T00:00:00';
SELECT * FROM emp FOR SYSTEM_TIME BETWEEN @a AND @b;   -- also FROM .. TO, CONTAINED IN (a, b), ALL
ALTER TABLE emp SET (SYSTEM_VERSIONING = OFF);          -- and = ON (HISTORY_TABLE = ...) again
```

Every `UPDATE` or `DELETE` copies the old row, with its period closed at the statement's UTC time, into the history
table (created when it does not exist, named `MSSQL_TemporalHistoryFor_<object id>` when no name is given). Inserts and
updates stamp the new row's period start with that time and its end with `9999-12-31 23:59:59.9999999`. History rows
are written in the same transaction, so a rollback removes them. `FOR SYSTEM_TIME` reads the table and its history
together and keeps the rows whose period matches (`AS OF t`: start <= t < end; `FROM a TO b`: start < b and end > a;
`BETWEEN a AND b`: start <= b and end > a; `CONTAINED IN (a, b)`: start >= a and end <= b).

Rules enforced (SQL Server's error numbers): the history table cannot be inserted into, updated or deleted from
(13559 / 13561 / 13560); neither table can be dropped or truncated, nor can columns be added, dropped or altered,
while versioning is on (13552); `FOR SYSTEM_TIME` on an ordinary table is 13544. An existing table can be named as the
history table when its columns match.

Differences: the stamped time is the statement's start time (SQL Server uses the transaction's start); explicit values
for the period columns are overwritten rather than rejected; there is no `HISTORY_RETENTION_PERIOD` clean-up; schema
changes need versioning switched off first; `sys.tables` has no `temporal_type` column.
