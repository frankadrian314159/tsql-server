# The shell and the desktop app

Both are thin front ends over the library; the code they share lives in `tsql_server::inspect` (catalog
introspection: `Connection::table_info`, `objects`, `definition`, `databases`, `transaction_count`) and
`tsql_server::tools` (result grids, CSV, SQL literals, table scripts), so the Rust API can do everything they do.

## `tsql` — the shell

```text
cargo run --release --bin tsql -- [DATA_DIR] [-D dir] [--pool-pages N] [-U login -P password] [-d database]
                                 [-Q sql]... [-i script.sql]...
```

Without a data directory the database lives in memory. `-Q` / `-i` (also `-c` / `-f`) run queries or script files
and exit with status 1 if any batch failed or a transaction was left open; 2 for bad arguments. With no `-Q`/`-i`
an interactive session starts (prompts only when stdin is a terminal, so it also works in pipes).

Type T-SQL and finish a batch with `GO` on its own line (`GO 5` runs it five times). The prompt shows the current
database, and `*` while a transaction is open. Results are printed as aligned grids (numbers right-aligned, long
values cut at 60 characters); `PRINT` output and errors (`Msg N, Level L, State S`) go where `sqlcmd` would put them
(errors on stderr).

| Command | |
| --- | --- |
| `.databases`, `.use NAME` | list databases; switch |
| `.tables`, `.views`, `.routines`, `.sequences` | list objects of the current database |
| `.columns T`, `.indexes T` | columns (type, nullability, identity, key); indexes, keys, foreign keys |
| `.schema [NAME]` | T-SQL that recreates a table (columns, keys, indexes, foreign keys) or a view/procedure/function (its text) |
| `.import [--new] FILE T` | load CSV: append to `T`, or create it with inferred types; all rows or none |
| `.export FILE SQL` | write a query's result as CSV |
| `.read FILE` | run a script |
| `.plan SQL` | the optimizer's plan, without running the statement |
| `.optimizer on\|off`, `.isolation LEVEL`, `.timer on\|off` | session settings |
| `.storage`, `.locks`, `.checkpoint` | buffer pool statistics, held locks, write a checkpoint |
| `.help`, `.quit` | |

`.schema` reproduces `DEFAULT` expressions, `CHECK` constraints (also disabled ones) and computed-column formulas from the
source text the engine keeps with them. CSV import maps an empty field to `NULL` in a nullable column
(and to an empty string in a `NOT NULL` text column); the header row names the columns, case-insensitively.

## `tsql-gui` — the desktop app

Built with [egui](https://github.com/emilk/egui) and compiled only with the `gui` feature, so the engine and shell
need no GUI toolkit:

```text
cargo run --release --features gui --bin tsql-gui -- [DATA_DIR] [--pool-pages N] [--embedded]
cargo run --release --features gui --bin tsql-gui -- --connect HOST[:PORT] [-U LOGIN] [-P PASSWORD] [-d DATABASE] [--encrypt]
```

### Which server it works on

The app works on a server over TDS, like any client:

* `--connect HOST[:PORT]` (with `-U`, `-P`, `-d`, `--encrypt`) names the server: a cluster's control node (`tsql-cluster --listen 1433`), `tsql-serve`,
  or any server that speaks TDS. **File > Connect to server...** does the same from inside the app.
* Without it, the app looks at `127.0.0.1:1433`. When something listens there it connects to it (the login is `sa` and the
  password is in the environment variable `TSQL_PASSWORD`; `TSQL_LOGIN` names another login). If the login is refused, the connect window opens
  with the reason so it can be tried again.
* When nothing listens there, the app starts a single-node server **in its own process** on `127.0.0.1:1433` (a `DATA_DIR` keeps its data
  on disk), gives its `sa` login a random password that only the process knows, and connects to it. Other tools (the shell, DBeaver, a
  cluster driver) can use that port while the window is open; the server stops with the window.
* `--embedded` keeps the engine inside the window with no network, the way the app worked before (and **File > Open data folder...**
  and **New in-memory database** switch a window to an embedded engine).

Over the wire the navigation pane, the designer and the scripts come from the `sys` catalog views, so everything a server shows there
works; what the wire does not carry is left out: `PRINT` output and the row counts of statements, XML and spatial index details of a
table, the buffer-pool statistics of **Database > Storage statistics...** (a server's are on its `/metrics` endpoint), and a data folder
(a server has its own).

* **Navigation pane** — database selector, a filter box, and tables, views, procedures, functions and sequences.
  Double-click to open; right-click for *Design*, *Script*, *Select top 100*, *Show definition*, *Delete…*.
* **Datasheet** — browse a table or view (`TOP n` rows, a `WHERE` filter, click a header to sort, the total row
  count). A table with a primary key is editable: double-click a cell to edit (Enter saves, Esc cancels, empty =
  `NULL`), add a row from the *New row* section, delete the selected row. Changes are `UPDATE`/`INSERT`/`DELETE`
  statements keyed on the primary key, so constraint and trigger errors appear exactly as they would from SQL.
  Views and tables without a key are read-only.
* **Design** — columns, indexes, keys, foreign keys, checks, triggers; add or drop a column, create or drop an index,
  drop a foreign key, show the table's script or open it in a query tab.
* **Query** — an editor with Run (Ctrl+Enter or F5; runs the selection if there is one, otherwise everything, with `GO`
  separators), *Explain* (the optimizer's plan), a result grid per result set, a messages pane (`PRINT`, row counts,
  errors, timing), open/save `.sql` files, export the shown result as CSV.
* **File** — open a data folder (or start a new in-memory database), connect to a server, import CSV into a new or existing
  table, export the current grid, **bulk import**, and **import / export a backup**. **Database** — begin/commit/roll back, isolation level, checkpoint, storage statistics (buffer pool
  and page file, live), locks held, load the sample database. The status bar always shows an open transaction, and
  closing or switching databases with one open asks whether to commit or roll back.

### Bulk import

**File > Bulk import...** loads a delimited file of any size (comma, tab, semicolon or pipe separated) into a table. The file is read
a line at a time, never whole, and loaded in batches of up to 1000 rows per `INSERT` on the session's worker thread, so the window stays
responsive; a counter shows the rows so far and **Stop** ends the import. The first line names the columns (or, without it, the fields are the
table's columns in order). The table can be an existing one or a new one (the table is created from the first 1000 lines, with
inferred types). *All or nothing* puts the whole load in one transaction (an error, a bad value or **Stop** takes every row back; the
message names the record and column); without it each batch commits by itself and an error leaves the earlier batches in. The same
code is `tsql_server::tools::bulk_import`, which works on any `SqlSession` (an engine or a server over TDS).

### Backups

**File > Export backup (.bak)...** runs `BACKUP DATABASE` for the chosen database in one of two formats: this server's own
(`TSQLBAK1`, checksummed logical frames) or a **Microsoft SQL Server `.bak`** (`WITH SQL_SERVER_FORMAT`). **File > Import backup
(.bak)...** runs `RESTORE DATABASE` under the name given (optionally replacing an existing database); it reads either kind, which the server
tells from the first bytes of the file (the dialog shows what it found), and a backup taken by a real SQL Server restores too
([docs/backup.md](backup.md)). The file is read and written by the server: on a server over the network the path is the server's, and
there is no **Browse...** button then. Both run on the worker thread.

The app uses one session, so it shows the effect of isolation levels and locks held by its own open transaction but
cannot demonstrate blocking between sessions; use two shells or the library for that. A script run from the Query tab
executes on a worker thread (the session moves to it and comes back with the results), so the window stays responsive,
shows the elapsed time, and keeps the transaction and isolation level of the one session. While it runs the rest of
the app (menus, navigation, other tabs) is disabled, because they would need the same session; there is no cancel
button yet, since the engine cannot interrupt a running statement. Datasheet edits and the designer run on the UI
thread (they are short statements). Only one process can have a data folder open.
