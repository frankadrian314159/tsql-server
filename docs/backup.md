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
file 3241. Backups are not compatible with SQL Server's `.bak` format (reading those is in progress, see the end of this page). Server-level objects (logins, server roles) are not
part of a database backup, as in SQL Server. A full backup copies rows, not pages, so its size follows the live data;
restore writes the restored rows through the redo log, so it takes about as long as loading that data. There is no
backup compression and no tail-log backup (`WITH NORECOVERY` on `BACKUP LOG`).

## Native SQL Server backups (`.bak`): work in progress

Reading the `.bak` files of a real SQL Server is being built in phases; reading only, no writer is planned. This server's own
format (above) stays the default for `BACKUP` and for `RESTORE` of files that start with `TSQLBAK1`; a SQL Server backup
is recognised by its first bytes, so `RESTORE` can tell the formats apart. The work is checked against a real backup, a
SQL Server 2025 `AdventureWorks2025.bak` (50 MB, compressed), which is not part of the repository: put it in `resources/`
and the tests that use it run.

| Phase | State |
| --- | --- |
| 1. The container: expand the compressed chunks and walk the Microsoft Tape Format stream; `tsql-bakinfo` | done |
| 2. The database image: 8 KiB pages, system tables, rows, the types of the columns | in progress (the layouts below are verified) |
| 3. Rebuilding the database in this server (`RESTORE` of such a file) | not started |
| 4. Pages changed while the backup ran (needs the log), differential and log backups | not started |

### What a recent backup looks like

```text
0x0000  "MSSQLBAK" header page (4 KiB)       replaces the TAPE block of Microsoft Tape Format (MTF)
0x1000  SFMB block (4 KiB)
0x2000  chunk, chunk, ...                    the stream, compressed in pieces of at most 64 KiB
        SFMB blocks, 16-byte footer
```

A chunk is a 24-byte header (`0x21`, the expanded size in units of 512 bytes, the compressed length), 4 bytes (probably a
checksum), and an **Xpress Huffman** chunk ([MS-XCA] 2.2; `src/xpress.rs`): a 256-byte table of 512 four-bit code lengths
and a bit stream of 16-bit words. One detail differs from what the specification suggests at first reading, and is verified
here: the extra length byte of a long match is read *before* its distance bits. A 50 MB file expands in under a second.

The expanded chunks joined form an MTF stream (`src/mtf.rs`): `SSET` (data set name, user, SQL Server version), `VOLB`,
`MSCI` (stream `MQCI`: the configuration, with the database name, file groups and the logical and physical names of the
files), `MSDA` blocks (stream `MQDA`: the pages of a file; `APAD`, `CSUM` and `SPAD` pad and check), `MSTL` (the log,
`MQTL`) and `MSLS`. The `MQDA` stream of the data file is a two-byte prefix and then **only the allocated extents**, as 8 KiB
pages in file order; the page number of each is in its own header (offset 32), so there are gaps that the stream does not mark.

### What is known about the pages

* Page 0 is the file header (type 15), page 1 PFS, 2 GAM, 3 SGAM, page 9 the boot page. The boot page holds the first page of
  `sysallocunits` (offset 0x264; page 20 in the sample), whose records (11 columns, no `dbfragid`) list every allocation
  unit with its first page, root and IAM page.
* A record starts with two status bytes and the length of its fixed part; then the fixed columns in column order, the column
  count, the null bitmap, the number of variable columns and their **end offsets counted from the start of the record**, and
  the variable data. `sysschobjs` (objects), `syscolpars` (columns), `sysidxstats`, `sysiscols`, `sysrowsets`, `sysobjvalues`
  decode with the fixed lengths implied by the `pminlen` field of their pages (checked: the sample has 3,457 objects, 73 of
  them user tables).
* Some runs of pages (a few hundred of 25,600 in the sample) look like noise: their first 32 bytes are random but the page
  id at offset 32 is right. What they are is not yet known (they sit between text pages of one object); rows stored there
  cannot be read until it is understood.

### The inspector

`cargo run --release --bin tsql-bakinfo -- file.bak [--hex 64] [--all] [--extract out.mtf]` describes a backup of either
form: the container (header words, chunks, compression ratio), then every block and stream (software, dates, sizes,
compression and encryption flags), and reports damage (a header checksum that does not match, a file cut short).
`--extract` writes the expanded stream so it can be examined with other tools. It reads the file only. Until phase 3 is
done, `RESTORE` of such a file fails with error 3241 and says so.
