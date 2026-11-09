# Backup, restore and point-in-time recovery

```sql
BACKUP DATABASE shop TO DISK = 'C:\backups\shop.bak' WITH NAME = 'nightly';
BACKUP LOG shop TO DISK = 'C:\backups\shop_1.trn';

RESTORE DATABASE shop FROM DISK = 'C:\backups\shop.bak' WITH REPLACE, NORECOVERY;
RESTORE LOG shop FROM DISK = 'C:\backups\shop_1.trn' WITH STOPAT = '2026-11-09 14:30:00', RECOVERY;

RESTORE VERIFYONLY FROM DISK = '...';     -- checks every checksum
RESTORE HEADERONLY FROM DISK = '...';     -- name, type, database, times, recovery model

ALTER DATABASE shop SET RECOVERY FULL | SIMPLE;   -- BULK_LOGGED is treated as FULL
SELECT name, state_desc, recovery_model_desc FROM sys.databases;
```

## Full backups

`BACKUP DATABASE` writes a logical copy of one database as checksummed frames: the catalog (tables, indexes,
constraints, views, procedures, functions, sequences, triggers, types, schemas, users, roles and permissions) and every
committed row. It is taken from a **snapshot**: other sessions keep reading and writing while it runs, and the file holds
exactly the commits up to the moment the backup started. A backup holds the catalog shared (so no DDL can start) and
waits for transactions that have changed the catalog and not yet finished, so what it copies is a committed catalog. It
is written to `file.tmp` and renamed, so a failed backup leaves nothing behind.

`WITH COPY_ONLY` leaves the log chain alone. `NAME`, `DESCRIPTION`, `INIT`, `FORMAT`, `CHECKSUM`, `COMPRESSION`, `STATS`
and similar options are accepted (the file is always overwritten and always checksummed). `DIFFERENTIAL`, striped
backups (several `DISK`s), `STANDBY`, encryption and mark-based restore are not supported and are refused.

Who may: sysadmin, `db_owner` or `db_backupoperator` for `BACKUP`; sysadmin or `dbcreator` for `RESTORE`. Neither can be
used inside a user transaction (error 574). System databases cannot be backed up (tempdb) or restored over.

## Differential backups

`BACKUP DATABASE db TO DISK = '...' WITH DIFFERENTIAL` writes only what changed since the last full backup (not `COPY_ONLY`):
the catalog, the ids of the rows each table has, and every row changed since the base (all rows of a table whose column
definitions changed). Without a base it is error 3035. To restore, restore the full backup it is based on `WITH NORECOVERY`,
then the differential (`NORECOVERY` or `RECOVERY`), then any log backups; a differential applied to anything but its own base is
error 3136. `RESTORE HEADERONLY` shows backup type 5. A differential leaves the log chain alone, so log backups continue as usual.

## Log backups and the log chain

In the FULL recovery model (the default) a full backup starts a **log chain**. From then on the server keeps the redo
log: at each checkpoint the log is copied to `log-archive/` before it is truncated. `BACKUP LOG` writes the committed
records of that one database since the previous backup in the chain, and the archived segments it covers are deleted.
`ALTER DATABASE ... SET RECOVERY SIMPLE` breaks the chain (the log is truncated at checkpoints again); take a new full
backup after switching back to FULL. A log backup needs a durable server (`tsql-serve DATA_DIR`).

Without log backups the archive keeps growing, exactly as the transaction log of a FULL-recovery SQL Server database does.

## Restore

`RESTORE DATABASE name FROM DISK` rebuilds the database in new storage (object ids are renumbered, so a copy never
shares locks with the original) under the name you give, which may differ from the backed-up one. If the database exists
you must say `WITH REPLACE` (error 3159 otherwise) and no other transaction may be open. The whole file is verified
before anything is replaced. The restored database is written to the redo log as one transaction, so it is durable like
any other change.

`NORECOVERY` leaves the database in the `RESTORING` state: it cannot be used (error 927) until a restore finishes with
`RECOVERY`, which is the default. In between, apply log backups in order with `RESTORE LOG`:

* each log backup must start where the database has got to (error 4305 if one is missing); a backup that overlaps what
  was already applied is fine, its older records are skipped;
* `STOPAT = '<date time>'` (server local time) stops at the last commit written at or before that time: **point-in-time
  recovery**. Records are timestamped with the wall clock when they are written;
* `RESTORE LOG` on a database that is not restoring is error 3117.

## Format and limits

Backup files are `TSQLBAK1` + frames of `[length][crc32][postcard]`; a damaged frame is error 3043, a truncated or foreign
file 3241. Backups are not compatible with SQL Server's `.bak` format. Server-level objects (logins, server roles) are not
part of a database backup, as in SQL Server. A full backup copies rows, not pages, so its size follows the live data;
restore writes the restored rows through the redo log, so it takes about as long as loading that data. There is no
backup compression and no tail-log backup (`WITH NORECOVERY` on `BACKUP LOG`).
