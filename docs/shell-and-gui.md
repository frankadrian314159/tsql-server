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
cargo run --release --features gui --bin tsql-gui -- [DATA_DIR] [--pool-pages N]
```

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
* **File** — open a data folder (or start a new in-memory database), import CSV into a new or existing table, export
  the current grid. **Database** — begin/commit/roll back, isolation level, checkpoint, storage statistics (buffer pool
  and page file, live), locks held, load the sample database. The status bar always shows an open transaction, and
  closing or switching databases with one open asks whether to commit or roll back.

The app uses one session, so it shows the effect of isolation levels and locks held by its own open transaction but
cannot demonstrate blocking between sessions; use two shells or the library for that. A script run from the Query tab
executes on a worker thread (the session moves to it and comes back with the results), so the window stays responsive,
shows the elapsed time, and keeps the transaction and isolation level of the one session. While it runs the rest of
the app (menus, navigation, other tabs) is disabled, because they would need the same session; there is no cancel
button yet, since the engine cannot interrupt a running statement. Datasheet edits and the designer run on the UI
thread (they are short statements). Only one process can have a data folder open.
