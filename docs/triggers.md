# DML, DDL and LOGON triggers, and the xml methods

```sql
CREATE TRIGGER no_drops ON DATABASE FOR DROP_TABLE, ALTER_TABLE AS
BEGIN
    ROLLBACK;
    THROW 50001, 'Tables cannot be dropped or altered here.', 1;
END

CREATE TRIGGER log_ddl ON ALL SERVER FOR DDL_DATABASE_LEVEL_EVENTS, CREATE_DATABASE AS
    INSERT audit.dbo.ddl_log VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'nvarchar(100)'), SYSTEM_USER);

CREATE TRIGGER gate ON ALL SERVER FOR LOGON AS
    IF ORIGINAL_LOGIN() = 'contractor' ROLLBACK;

DISABLE TRIGGER no_drops ON DATABASE;   ENABLE TRIGGER ALL ON ALL SERVER;   DROP TRIGGER gate ON ALL SERVER;
DISABLE TRIGGER trg ON dbo.t;           -- DML triggers too
```

## DML triggers

`CREATE [OR ALTER] TRIGGER name ON table { AFTER | FOR | INSTEAD OF } { INSERT | UPDATE | DELETE } [, ...] AS ...` runs once per
statement, in the statement's transaction, with the rows it changed as the `inserted` and `deleted` tables and `UPDATE(col)`
true for the columns an `UPDATE` sets. An `AFTER` trigger runs after the rows are written (and the constraints checked); an
`INSTEAD OF` trigger runs in place of the statement, which then changes nothing itself. A trigger runs with the rights of its
schema's owner, so the ownership chain carries into the tables it writes. Inside an `INSTEAD OF` trigger the same statement on
the same table goes to the table, not to the trigger again. `ROLLBACK` in a trigger ends the transaction and aborts the batch
(3609).

### INSTEAD OF triggers on views

```sql
CREATE VIEW dbo.order_lines AS
    SELECT o.id, c.name, o.amount FROM dbo.orders o JOIN dbo.customers c ON c.id = o.customer_id;
GO
CREATE TRIGGER dbo.order_lines_ins ON dbo.order_lines INSTEAD OF INSERT AS
    INSERT dbo.orders (id, customer_id, amount)
    SELECT i.id, c.id, i.amount FROM inserted i JOIN dbo.customers c ON c.name = i.name;
GO
INSERT dbo.order_lines VALUES (13, 'bob', 4);      -- the trigger writes dbo.orders
```

A view takes `INSTEAD OF` triggers, one per statement (`INSERT`, `UPDATE`, `DELETE`): with one, a view that cannot be written
through (a join, an aggregate, a `UNION`) can be, by a trigger that writes its base tables (`src/engine/viewtrig.rs`).

* **What the trigger sees.** `inserted` and `deleted` have the view's columns, with the types the view gives them. For an
  `INSERT`, `inserted` holds the source rows matched to the view's columns by the column list; a column the list leaves out, and
  `DEFAULT`, is NULL (a view has no defaults). For a `DELETE`, `deleted` holds the rows of the view that the `WHERE` (and the
  `FROM`, if any) select; for an `UPDATE`, `deleted` holds those rows and `inserted` the same rows with the `SET` applied
  (`SET c += 1` too), and `UPDATE(col)` is true for the columns the `SET` names. A view row that a `FROM` joins to several rows is
  in them once. `TOP` and `SET ROWCOUNT` limit the rows. The trigger runs even when no row matched.
* **Permissions.** The statement needs `INSERT`, `UPDATE` or `DELETE` permission on the view; the view's rows are read with the
  rights of its owner, as a `SELECT` through the view is.
* **`OUTPUT`** returns the rows as if the statement had been carried out, as in SQL Server.
* **A statement on the view inside its own trigger** goes to the view's base table when the view maps onto one table (as it does
  without a trigger), and is an error otherwise.
* **DDL.** An `AFTER` trigger on a view is error 8197, a second `INSTEAD OF` trigger for the same statement error 2111.
  `ALTER VIEW` keeps the view's triggers and `DROP VIEW` drops them; `DISABLE` / `ENABLE TRIGGER ... ON view` and `DROP TRIGGER`
  work as on tables; `sys.triggers` lists them with the view as `parent_id` and `is_instead_of_trigger` = 1.
* **Not handled.** `WHERE CURRENT OF` and `SET @variable = ...` in an `UPDATE` of such a view run as if the view had no trigger;
  `MERGE` into a view does not fire it. The rows an `UPDATE` or `DELETE` reaches are found by numbering the view's rows and running
  the statement's `FROM` and `WHERE` over them, so a comparison in the `WHERE` uses the default collation even where a column of
  the view has another one.

## DDL triggers

`CREATE [OR ALTER] TRIGGER name ON DATABASE | ALL SERVER FOR event [, ...] AS ...` (`AFTER` is a synonym of `FOR`). A
database trigger sees the events of its own database; a server trigger sees every database's events and the server-level
ones. It runs **after** the statement, in the statement's transaction and with the *caller's* permissions (the ownership
chain is not extended). Inside it `@@TRANCOUNT` is at least 1: an error, or a `ROLLBACK` (followed by a `THROW`, normally),
undoes the statement. A bare `ROLLBACK` ends the batch with error 3609, as for a DML trigger.

Events: `CREATE|ALTER|DROP` of `TABLE`, `VIEW`, `INDEX`, `PROCEDURE`, `FUNCTION`, `TRIGGER`, `SCHEMA`, `SEQUENCE`, `USER`,
`ROLE`; `CREATE_TYPE`, `DROP_TYPE`; `GRANT_DATABASE`, `DENY_DATABASE`, `REVOKE_DATABASE`; `ALTER_DATABASE`; and, server
scope only, `CREATE|ALTER|DROP_LOGIN`, `CREATE_DATABASE`, `DROP_DATABASE`, `LOGON`. Event groups: `DDL_TABLE_EVENTS`,
`DDL_VIEW_EVENTS`, `DDL_INDEX_EVENTS`, `DDL_TABLE_VIEW_EVENTS`, `DDL_PROCEDURE_EVENTS`, `DDL_FUNCTION_EVENTS`,
`DDL_TRIGGER_EVENTS`, `DDL_SCHEMA_EVENTS`, `DDL_TYPE_EVENTS`, `DDL_SEQUENCE_EVENTS`, `DDL_USER_EVENTS`, `DDL_ROLE_EVENTS`,
`DDL_GDR_DATABASE_EVENTS`, `DDL_LOGIN_EVENTS`, `DDL_DATABASE_EVENTS`, `DDL_DATABASE_LEVEL_EVENTS`,
`DDL_SERVER_LEVEL_EVENTS`. An unknown event is error 1082. Creating a server trigger needs `sysadmin`; a database trigger
needs `db_owner`.

`CREATE OR ALTER FUNCTION` raises `CREATE_FUNCTION` and `CREATE OR ALTER VIEW/PROC/TRIGGER` the `ALTER_` event.
`ALTER INDEX`, `TRUNCATE TABLE`, statistics, security policies and `BACKUP`/`RESTORE` raise no event.

### EVENTDATA()

Returns the event as xml text: `EventType`, `PostTime`, `SPID`, `ServerName`, `LoginName`, `UserName`, `DatabaseName`,
`SchemaName`, `ObjectName`, `ObjectType` and `TSQLCommand/CommandText` (the text of the whole batch that raised it, not
just the statement). NULL outside a trigger.

## LOGON triggers

Run after a login authenticates (`Server::connect_as`, so every TDS login), as that login, in an implicit transaction that
is committed afterwards. A `ROLLBACK` or an error refuses the login with error 17892 and the session is dropped. The trusted
in-process `Server::connect()` (the `sa` session of the shell, tests and the desktop app) never runs them, which is how a
faulty trigger is repaired. The logon trigger does not see the login's database or failed logins.

## ENABLE / DISABLE TRIGGER

`ENABLE | DISABLE TRIGGER name [, ...] | ALL ON DATABASE | ALL SERVER | table`. A disabled trigger stays in the catalog
(`sys.triggers.is_disabled`) and does not fire. `DROP TRIGGER name ON DATABASE | ALL SERVER` removes a DDL trigger.

## Catalog

`sys.triggers` (DML triggers, `parent_class` 1, and database DDL triggers, `parent_class` 0), `sys.trigger_events`,
`sys.server_triggers`, `sys.server_trigger_events`.

Server and database DDL triggers are applied immediately, logged and not undone by `ROLLBACK`, like the other security
and policy statements.

## The xml methods

`xmlcol.value('path', 'type')`, `.exist('path')`, `.query('path')`, `.nodes('path') AS t(c)` and `.modify('dml')`, on columns,
variables and expressions such as `EVENTDATA()` (`src/xmlpath.rs`). The document is parsed into a tree that keeps prefixes and
`xmlns` attributes, so a modified document is written out as it was. Names are matched by their local part; namespaces are not
resolved, and a `declare namespace p = "...";` or `declare default element namespace "...";` prolog is accepted and not used.

The query language is a subset of XQuery: absolute and relative paths (`/a/b`, `//b`, `a/b`, `.`, `..`), the steps `name`,
`p:name`, `*`, `@attr`, `@*`, `text()`, `node()`, `(path)[n]` followed by more steps, predicates (`[n]`, `[last()]`, comparisons
joined by `and` / `or`, `not()`, an existence test), numbers, strings, `+ - * div mod`, and `data`, `string`, `number`, `count`,
`sum`, `not`, `concat`, `contains`, `starts-with`, `string-length`, `empty`, `exists` and the `xs:` casts. `sql:column("t.c")` and
`sql:variable("@v")` are bound by the SQL parser to the column or variable. Not supported: FLWOR, `if`, written-out axes,
user functions; typed values (the validation of typed columns is in [xml-schema.md](xml-schema.md)).

* `.value` returns the first item's text converted to the type (NULL if none; error 9400 for malformed xml, 2225 for a query
  that is not supported), `.exist` is 1 or 0, `.query` the items as xml text.
* `.nodes('path')` is a table: one row per selected element, in a single `xml` column, used with `CROSS APPLY` or `OUTER APPLY`
  (`FROM t CROSS APPLY t.doc.nodes('/a/b') AS n(ref)`) or on a variable (`FROM @x.nodes(...) AS n(ref)`); `n.ref.nodes(...)`
  works on a node again.
* `.modify` is `insert <element/> | attribute n {e} | "text"` with `as first into`, `as last into`, `into`, `before` or `after`
  a target, `delete target`, and `replace value of target with expr`; `{ expr }` inside new content is evaluated. It is used
  as `UPDATE t SET col.modify('...')` and `SET @x.modify('...')`. The target of `insert` and `replace value of` must be one
  node. A `NULL` xml stays `NULL`.

**The nodes that `.nodes()` returns are xml text** (each is the element, with the namespace declarations that are in scope for it),
not positions in the document they came from. Two things follow. The parent of a node is not available (`..` from a node finds
nothing), and a relative path in a method of a node starts at the node, as in SQL Server, which is told from a relative path
on a document, which starts at the document, by a rule: a relative path that finds nothing at the document is tried again at
its single top-level element. The one case in which that differs from SQL Server is a relative path that SQL Server would
evaluate to nothing, and that this server does evaluate from the element.

Error 2225 carries the reason for a query or statement that is not supported or cannot be applied; SQL Server's own numbers
for these (2256, 2260 and others) are not used. XML indexes can be created but are only recorded (see
[optimizer.md](optimizer.md#xml-indexes)).

## OPENXML

`sp_xml_preparedocument @h OUTPUT, @xml` parses a document and gives its handle (per session; error 6603 if it is not
well-formed), `sp_xml_removedocument @h` frees it (6602 for a handle that is not valid). `OPENXML(@h, 'rowpattern' [, flags])`
then has one row for each node the pattern selects, with a `WITH (col type ['colpattern'], ...)` list, or `WITH tablename` (the
columns of that table, matched by name), or with no `WITH` the edge table (`id, parentid, nodetype, localname, prefix,
namespaceuri, datatype, prev, text`). A column with no pattern reads the attribute of its name (flags 0 and 1), the child
element (flags 2) or the attribute and then the element (flags 3); a pattern is a path from the row node (`'@id'`, `'../@id'`,
`'Name'`, `'text()'`, `'.'`). Not supported: the `@mp:` meta-properties, the namespace argument of
`sp_xml_preparedocument`, and `datatype`/`namespaceuri` in the edge table (they are NULL). The ids of the edge table are numbered
one after another; SQL Server leaves gaps in its numbering, so only the shape of the table is the same.

## Full-text search

`CONTAINS(col | (c1, c2) | *, 'condition' [, LANGUAGE n])`, `FREETEXT(...)`, `CONTAINSTABLE(table, cols, 'condition' [, LANGUAGE n]
[, top])` and `FREETEXTTABLE(...)` (`src/fulltext.rs`). The two tables have the columns `KEY` (the single-column key of the table) and
`RANK`. A text is tokenised and matched when a row is looked at, so a search is a scan. `*` is every text column of the table
(of every table in the row, in the predicate form).

`CREATE | ALTER | DROP FULLTEXT CATALOG` and `CREATE | ALTER | DROP FULLTEXT INDEX` (`src/engine/ftddl.rs`) are **declarations**: they
are checked and kept in the catalog (`sys.fulltext_catalogs`, `sys.fulltext_indexes`, `sys.fulltext_index_columns`) so that scripts
and backups that create them run, and they change no result. The checks: the key index must be a unique, non-nullable,
single-column index (7653), the columns must be character, binary or xml (7654), one index per table (7611), the catalog must
exist (7608) and not exist twice (7609), `ALTER` needs an index (7612), `ADD`/`DROP` of a column that is/is not in it (7613,
7614), at least one column stays (7615), a catalog with indexes is not dropped (7610), and a column or the key index of a
full-text index cannot be dropped. `LANGUAGE`, `CHANGE_TRACKING`, `STOPLIST` and population commands are read; only the
language and the tracking mode are kept. **A predicate on a table that has no full-text index is not an error here** (SQL
Server gives 7601), so the scan-based search stays usable without the declaration. The catalogs, unlike the indexes, are not
part of a transaction (a rolled-back `CREATE FULLTEXT CATALOG` stays). A restore from a SQL Server backup does not import
full-text indexes (see [backup.md](backup.md)); the numbers of the errors above are this server's, not SQL Server's.

A condition understands words (case does not matter), `"phrases"`, prefix terms (`"comp*"`), `AND`, `OR`, `AND NOT` (and `&`,
`|`, `&!`), parentheses, `NEAR` (`a NEAR b`, `a ~ b`, `NEAR((a, b), distance, TRUE)`), `FORMSOF(INFLECTIONAL, word)` (words with the
same plain English stem; irregular forms are not known), `FORMSOF(THESAURUS, word)` (no thesaurus: the word itself) and
`ISABOUT(term WEIGHT(w), ...)` (matches any term). A short English list of noise words is ignored, and a phrase skips over
them; a condition with only noise words is error 7619, a syntax error 7630. `FREETEXT` takes the words of its text, drops the noise
words and matches texts that have the stem of any of them. **The rank is a number from 1 to 1000 that grows with the number of
matches; it orders results and is not SQL Server's rank.** The language is read and not used.
