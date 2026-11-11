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
| 2. The database image: 8 KiB pages, system tables, the layout of records, rows, large objects, XML, indexes and constraints (`src/sqlbak.rs`) | done for the common types |
| 3. `RESTORE DATABASE x FROM DISK = 'file.bak'`, `RESTORE VERIFYONLY`, `RESTORE HEADERONLY` (`src/engine/bakimport.rs`) | tables, rows, computed columns, keys, indexes, defaults, checks, views, procedures, functions and triggers |
| 4. Pages changed while the backup ran (needs the log), differential and log backups | not started |

`RESTORE DATABASE` of a SQL Server backup reads the tables and rows, builds them in a scratch in-memory server with `CREATE
TABLE` and `INSERT`, adds keys, indexes and constraints once the rows are in, writes that database as a backup of this
server's own format and restores that file, so permissions, `REPLACE`, `NORECOVERY` and durability behave as for any restore.
The sample (760,167 rows in 71 tables) restores in about 15 seconds in a release build.

What comes over:

* schemas, tables with their column types, nullability and identity properties, and all rows, including large values
  (`varbinary(max)`, `nvarchar(max)`, long `xml`, images) and `xml` of the forms below;
* computed columns, with their expressions (`PERSISTED` where the backup stores the value in the row);
* views, procedures, functions (scalar, inline and multi-statement) and triggers, from the text of their `CREATE` statement,
  which a backup keeps as written. Functions are created first (computed columns and checks call them; one that reads
  tables is tried again once the rows are in), views, procedures and triggers after the rows, and what fails is tried
  again while a pass creates anything (a view of a view). Triggers are created after the rows are loaded, so loading does
  not fire them. In the sample, all 20 views, 10 procedures, 11 functions and 11 triggers (the database trigger included) are
  created, those that use the XML methods `.nodes()` and `.modify()` and full-text search among them;
* full-text catalogs and indexes: the catalog (`sysclsobjs` class 32: name, and its status for default and accent
  sensitivity, both set in the sample), and each index (`sysftinds`: the table and the number of its key index; `sysobjvalues`
  class 42: the indexed columns and their language ids; `syssingleobjrefs` class 39: the catalog). In the sample, the catalog
  and the 3 indexes (Document, JobCandidate, ProductReview) are created. Change tracking and the other flags of `sysftinds`
  are not decoded: the index is created with `CHANGE_TRACKING AUTO`, and the meaning of the status bits of a catalog is
  inferred from one sample;
* spatial indexes: created on their column with default tessellation settings (a geometry index gets the bounding box
  `(-180, -90, 180, 90)`), and a warning says so; the sample has no spatial index, so the form in which a backup keeps the
  settings is not known and they are not read;
* XML indexes, primary ones before the secondary ones built on them. This server records them in the catalog and does not
  build anything (see [optimizer.md](optimizer.md#xml-indexes)); in the sample, 8 of 8 are created;
* `text`, `ntext` and `image` values, and `sql_variant` values of the types that are decoded (see below);
* `PRIMARY KEY` and `UNIQUE` constraints, clustered and nonclustered indexes (unique, with `INCLUDE` columns), `FOREIGN KEY`
  constraints (`ON DELETE CASCADE` is read; see below), `DEFAULT` and `CHECK` constraints, with their names. In the sample:
  72 keys, 89 indexes, 90 foreign keys, 152 defaults and 89 checks, as many as the catalog lists.

What does not:

* user-defined types (columns of an alias type get the base type), sequences, users and permissions, and CLR objects;
* columnstore and hash indexes (the engine has no equivalent), and filtered indexes lose their filter (their definition is
  not read);
* XML schema collections and the typed xml columns that use them: the collections are stored as compiled components, not as
  XSD text, and are not read, so such columns come over as untyped `xml` (their values are read as before);
* referential actions other than `ON DELETE CASCADE` (the other bits of the status of a foreign key are not decoded, and
  a warning names a key that has any), descending key columns are read from a bit that no index of the sample uses
  (unverified);
* a computed column whose expression this server cannot create: the table is then created without its computed columns and
  a warning says why. `hierarchyid` columns import as `hierarchyid`, with their methods (see [datatypes.md](hierarchyid.md)),
  but a node with a label above 271, a negative label or a fractional label (`/1.1/`) is not read by this server;
* `sql_variant` values whose base type is `decimal`, `time`, `datetime2` or `datetimeoffset` (their properties have not been
  seen in a backup), and the typed XML types listed below, which come over as NULL with a warning that names the column and
  the count;
* the `MOVE` options are ignored.

Objects that use what the engine lacks fail to be created and are listed (see [triggers.md](triggers.md) for the XML methods
and full-text search, which this server now has in a reduced form). `WITH EXECUTE AS CALLER` and `NOT FOR REPLICATION`, which AdventureWorks uses, are
accepted by the parser and not applied (there is no replication, and the procedure runs as its caller).

Everything that is not created is listed in warnings (the first 30, then a count) with the reason.

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
* **The layout of a record is in `sysrscols`**, not in the order of the columns: per rowset and column it gives the offset in
  the record (variable-length columns have offsets from 0xFFFF down), the null bit (counted from 1) and, for bit columns,
  the bit within the shared byte. The columns are numbered the way the b-tree stores them: **the clustered key columns first,
  in key order, then the others by column id**, with computed columns left out; `sysiscols` gives the key (`intprop` is the
  column id, `tinyprop1` the position in the key). Bit columns of one table share bytes.
* Some runs of pages (a few hundred of 25,600 in the sample) look like noise: their first 32 bytes are random but the page
  id at offset 32 is right. What they are is not known; nothing in the sample needs them (every table has as many rows as
  its catalog says, and every large value is complete), and they are left out of the index of pages.
* **The column of a record is found by column id**: the second column of `sysrscols` (`rscolid`) is the id of `syscolpars`;
  `hbcolid` is only the rank in the b-tree. A rowset lists the stored columns: the persisted computed columns are in it, the
  others are not.
* **Indexes**: `sysidxstats` (type 1 clustered, 2 nonclustered, 0 a heap or the statistics of a column, 3 XML; status 0x8
  unique, 0x20 `PRIMARY KEY`, 0x40 `UNIQUE` constraint), with their columns in `sysiscols` (`subid` is the order in the index,
  `tinyprop1` the position in the key, status 0x10 an `INCLUDE` column, 0x4 descending).
* **Constraints**: the objects of `sysschobjs` of type `D ` (default), `C ` (check) and `F ` (foreign key) with the table in
  `pid` and, for defaults and column checks, the column in `intprop`. The text of a default or check is in `sysobjvalues`
  (class 1, `objid` the constraint) in 8-bit characters; the text of a computed column is class 2 with the table in `objid` and
  the column in `subobjid`. A foreign key's columns are in `syssingleobjrefs`: class 27 gives the referenced table, 28 and 29
  the columns of the key in the referencing and the referenced table (`depsubid` is the position). The status bit 0x4000 of a
  foreign key is `ON DELETE CASCADE`.
* **Large values** (`src/sqlbak.rs`, `lob`): a variable-length column whose end offset has the high bit set holds a pointer
  instead of the value. A pointer of type 4 has a 12-byte header and then 12-byte links (cumulative size, page, file, slot);
  each leads to a data fragment (type 3) of a text page (a record with a 14-byte header, the data after it, up to 8,040
  bytes) or to an internal node (type 2: 20 bytes of header, then 16-byte links). The data of the fragments in order is the
  value. The 16-byte pointer of `text`, `ntext` and `image` columns (type 0: blob id, page, file, slot) leads to a root record in
  a text page (type 5: 24 bytes of header, with the number of links at 16, then 12-byte links like those above), or to the data
  directly. It was checked on the 100-odd such pointers of the system tables of the sample (the XML schema components,
  object 145), not on a user table: the sample has no `text`, `ntext` or `image` column. A value that is not behind a pointer
  (the form of `text in row`) is not read. A trailing variable-length column
  of zero length is not stored at all, and is an empty value if its null bit is not set.
* **`sql_variant`**: the base type, a byte that is 1, the properties of the type, and the value in the form of the type.
  Seen in the system tables of the sample and decoded: `tinyint`, `int`, `bigint`, `bit` (no properties), `nvarchar` (maximum
  length in bytes as 2 bytes, then the 4-byte collation id, then UTF-16) and `varbinary` (maximum length, then the bytes);
  all 538 extended-property descriptions and the numbers of `sysobjvalues` decode. The other types that have no properties
  and the `char`, `varchar`, `nchar` and `binary` types are read by the same form, which is probable and not checked against a
  backup that has them; `decimal`, `time`, `datetime2` and `datetimeoffset` are left NULL.
* **XML indexes**: `sysidxstats` type 3 with ids from 256000; the byte after the type is 0 for a primary index and the letter
  of the kind (`V`, `P`, `R`) for a secondary one; the column is in `sysiscols`. A secondary index does not say which
  primary index it uses, and a column has only one, so it is the primary index of the same column.
* **Views, procedures, functions and triggers**: objects of `sysschobjs` of type `V `, `P `, `FN`, `IF`, `TF`, `TR` (and the
  CLR types `AF`, `FS`, `FT`, `PC`, `TA`) outside the `sys` and `INFORMATION_SCHEMA` schemas; a trigger's `pid` is its table (0
  for a database trigger). The whole `CREATE` statement, starting with a line break, is in `sysobjvalues` class 1 (`objid`
  the object, `subobjid` the procedure number) as 8-bit text, in pieces by `valnum` when long; an encrypted object has none.

### XML columns

The stored form of `xml` is SQL Server's binary XML, decoded by `src/binxml.rs` (the tokens are listed there). Untyped XML is
a plain token stream (names, qualified names, elements, attributes, text) and decodes completely: all 901 values of the
sample that are stored in the row came out as the XML they were (`<EVENT_INSTANCE><EventType>CREATE_TABLE</EventType>...`).
XML bound to a schema collection (typed XML) stores each value in the form of its type: strings and years as text, decimals and
the integer types derived from them as precision, scale, a sign byte (0 is negative) and a 16-byte mantissa, a real as `03` and
four bytes, a date as a `datetimeoffset` printed with `Z` or the offset. The sample's 19,972 `Person.Demographics` and 701
`Sales.Store.Demographics` values decode to the XML of AdventureWorks (negative `TotalPurchaseYTD` values, `2003-09-01Z`). **Other
type codes are not known** (`xs:boolean`, `dateTime`, `time`, `double`, `base64Binary`, lists, ...): one in a value makes
the whole value an error, which leaves it NULL with the warning instead of writing it wrongly. The type codes seen so far
are in `typed_value`; more can be added from a backup that uses them. Long documents are read as large values. Besides
elements and attributes, the decoder reads comments (`F3`), processing instructions (`F4`) and namespace declarations whose
whole name (`xmlns:p`) is one defined name; a type declaration (`EA`) may be followed by a name instead of a value, and the
value tokens (`87` decimal, `7C` date, `03` real) lead with a byte of their own, in attributes too.

### The inspector

`cargo run --release --bin tsql-bakinfo -- file.bak [--hex 64] [--all] [--extract out.mtf]` describes a backup of either
form: the container (header words, chunks, compression ratio), then every block and stream (software, dates, sizes,
compression and encryption flags), and reports damage (a header checksum that does not match, a file cut short).
`--extract` writes the expanded stream so it can be examined with other tools. It reads the file only. `--tables` lists the
tables of the database with their row counts.
