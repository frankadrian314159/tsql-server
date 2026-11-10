# DDL triggers, LOGON triggers and the xml methods

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

`xmlcol.value('path', 'type')`, `.exist('path')` and `.query('path')`, on columns, variables and expressions such as
`EVENTDATA()`. The path language is a small subset of XPath: `/a/b`, `//b`, `*`, `@attr`, `text()`, positions `[n]` and
`[last()]`, equality predicates `[@a='v']` and `[child='v']`, a wrapping `(...)[n]` and `data(...)`; namespaces are
ignored. `.value` returns the first match's text converted to the type (NULL if none, error 9400 for malformed xml, 2225
for an unsupported path), `.exist` is 1 or 0, `.query` the matching nodes as xml text. Not supported: FLWOR,
other XQuery functions, `.nodes()`, `.modify()`, `OPENXML`, typed xml and XML indexes.
