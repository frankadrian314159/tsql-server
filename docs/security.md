# Security

## Principals

* **Logins** authenticate to the server: `CREATE LOGIN name WITH PASSWORD = '…'`, `ALTER LOGIN … ENABLE | DISABLE |
  WITH PASSWORD`, `DROP LOGIN`. Passwords are stored as salted SHA-256 hashes. `Server::connect()` is a trusted
  in-process session as the built-in `sa` login, which is a member of `sysadmin` and bypasses every check;
  `Server::connect_as(login, password)` authenticates (failure is error 18456).
* **Server roles**: `sysadmin` and `dbcreator` (`ALTER SERVER ROLE … ADD MEMBER`).
* **Users** map a login into a database: `CREATE USER u FOR LOGIN l`. `dbo` (the owner) is unrestricted. A login
  with no user in a database cannot `USE` it (error 916); `master` and `tempdb` admit everyone as `guest`.
* **Roles**: the fixed database roles `db_owner`, `db_datareader`, `db_datawriter`, `db_ddladmin`,
  `db_denydatareader`, `db_denydatawriter`, `db_securityadmin`, `db_accessadmin`, `public` (everyone), and your own
  (`CREATE ROLE`, `ALTER ROLE r ADD MEMBER u`, `sp_addrolemember`).

## Permissions

`GRANT | DENY | REVOKE perm [, …] [ON securable] TO | FROM principal [, …] [WITH GRANT OPTION]`.

* Securables: an object (`table`, `view`, procedure, function, sequence), `SCHEMA::name`, or the database
  (no `ON` clause).
* Permissions: `SELECT INSERT UPDATE DELETE EXECUTE REFERENCES ALTER CONTROL` on objects; `CREATE TABLE`,
  `CREATE VIEW`, `CREATE PROCEDURE`, `CREATE FUNCTION`, `CREATE SCHEMA`, `CREATE TYPE`, `CREATE SEQUENCE` and the object
  permissions at database level.
* Resolution: members of `db_owner` and the schema owner are allowed; otherwise a `DENY` anywhere in the user's role
  closure and the securable's scope chain (object → schema → database) wins over any `GRANT`; `CONTROL` implies
  everything; the fixed roles add their implied grants/denies. Anything else is denied.
* Failures are error 229 (`The SELECT permission was denied on the object 'emp', database 'master', schema 'dbo'.`),
  262 for `CREATE …` in the database, 916 for database access, 15247 for administrative statements, 4902 for
  `ALTER`/`DROP` on objects the caller cannot alter.

## What is checked

Reading a table or view (`SELECT`), `INSERT` / `UPDATE` / `DELETE` / `MERGE` on the target, `TRUNCATE` (`ALTER`),
`EXEC` of a procedure and calling a scalar function (`EXECUTE`), reading a table-valued function (`SELECT`),
all `CREATE`/`ALTER`/`DROP` DDL, `CREATE DATABASE` (`sysadmin` or `dbcreator`), and `USE`.

### Ownership chaining

Inside a view, procedure, function or trigger, access to objects with the **same owner** (the owner of the schema)
is not checked again, so `GRANT EXECUTE` on a procedure lets a user modify tables they cannot touch directly.
The chain is broken by dynamic SQL (`EXEC('…')`, `sp_executesql`), where the caller's own permissions apply, and by
objects in schemas with a different owner.

### Impersonation

`EXECUTE AS USER = 'u'` / `EXECUTE AS LOGIN = 'l'` and `REVERT` (allowed for `sysadmin` and `db_owner`).
`USER_NAME()`, `SUSER_SNAME()`, `ORIGINAL_LOGIN()`, `IS_MEMBER()`, `IS_SRVROLEMEMBER()` and
`HAS_PERMS_BY_NAME()` report the effective identity. Catalog views: `sys.server_principals`,
`sys.database_principals`, `sys.database_permissions`, `sys.database_role_members`.

## Column-level permissions

`GRANT | DENY | REVOKE SELECT | UPDATE | INSERT | REFERENCES (col, ...) ON table TO ...` (also `ON table (col, ...)`)
are stored per column. They follow SQL Server's rules: a column `DENY` and any table-, schema- or database-level `DENY`
win; a table-level `GRANT` or a column `GRANT` allows.

* **SELECT.** A user with SELECT on some columns can query the table, and a column they may not read fails with
  error 230 *where it is used*: in the select list, `*`, `WHERE`, `ORDER BY`, a join condition or an aggregate.
  Such a table is read with a plain scan, so an index seek or an ordered read cannot be used to test or order by
  a hidden column. A user with no readable column gets the object-level error 229.
* **UPDATE.** A table-wide UPDATE permission, or UPDATE on every column assigned; in the column-level case, the
  columns read by `WHERE` and by the new values also need SELECT.
* **INSERT.** A table-wide INSERT permission, or INSERT on every column of an explicit column list
  (`INSERT t (a, b) ...`; without a column list the whole table is meant, so table-wide INSERT is needed). A column
  that is not granted is error 230; a column `DENY` wins.
* **DELETE.** Needs the table-wide DELETE permission, and SELECT on every column its `WHERE` clause reads (table-wide, or
  column grants), since the outcome would reveal them.
* Sysadmins, the owner and ownership chains are not restricted. Views are checked as a whole (a view reading a
  hidden column of a table you do not own works through ownership chaining, as in SQL Server).

## Dynamic data masking

```sql
CREATE TABLE person (
    name  VARCHAR(30) MASKED WITH (FUNCTION = 'default()'),
    email VARCHAR(40) MASKED WITH (FUNCTION = 'email()'),
    phone VARCHAR(20) MASKED WITH (FUNCTION = 'partial(2, "XXX-XXX", 2)'),
    score INT         MASKED WITH (FUNCTION = 'random(1, 5)'));
ALTER TABLE person ALTER COLUMN name ADD MASKED WITH (FUNCTION = 'default()');
ALTER TABLE person ALTER COLUMN name DROP MASKED;
GRANT UNMASK TO ann;        -- db_owner and sysadmin see the data without being granted
```

Users without `UNMASK` get the disguised value when a masked column is returned by a query: `default()` gives `XXXX` (fewer
for short columns), `0`, `1900-01-01`, `0x00` by type; `email()` gives `aXXX@XXXX.com`; `partial(prefix, "pad", suffix)` keeps the
first and last characters and shows the padding between (only the padding when the parts would cover the value);
`random(a, b)` a random number in the range. NULL stays NULL. Masking is applied to **output** only, as in SQL Server: the
stored data, `WHERE`, joins and constraints see the real values (so a user can probe masked values with predicates), and
`SELECT INTO` / `INSERT ... SELECT` by a masked user copy the masked form. Expressions built from a masked column (`UPPER(name)`)
are not masked. Masks must fit the type (error 16003) and parse (16000). `sys.masked_columns` lists them.

## Row-level security

```sql
CREATE FUNCTION dbo.only_mine(@owner VARCHAR(20)) RETURNS TABLE
AS RETURN SELECT 1 AS ok WHERE @owner = USER_NAME();

CREATE SECURITY POLICY mine
    ADD FILTER PREDICATE dbo.only_mine(owner) ON dbo.orders,
    ADD BLOCK  PREDICATE dbo.only_mine(owner) ON dbo.orders AFTER INSERT,
    ADD BLOCK  PREDICATE dbo.only_mine(owner) ON dbo.orders BEFORE DELETE
    WITH (STATE = ON);
ALTER SECURITY POLICY mine WITH (STATE = OFF);   -- also ADD / DROP FILTER|BLOCK PREDICATE ...
DROP SECURITY POLICY mine;
```

A **filter** predicate (an inline table-valued function of columns of the target table) hides rows from every read,
and so from `UPDATE` and `DELETE`: only rows for which the function returns a row are seen, whether the table is read by a scan,
an index seek, a join or a subquery. **Block** predicates (`AFTER INSERT`, `AFTER UPDATE` on the new row, `BEFORE UPDATE` and
`BEFORE DELETE` on the existing row; none given means all four) make a violating write fail with error 33504. Predicates run in
the caller's context (`USER_NAME()`, `SESSION_CONTEXT`, `IS_MEMBER()`), need no permission on the function from the querying user,
and do not re-trigger the policy when they read the protected table. One filter predicate and one block predicate of each kind per
table (33265). Policies apply to **every** user, including `dbo` and sysadmin (write the function to let administrators
through); managing them needs sysadmin or `db_owner`. `sys.security_policies` and `sys.security_predicates` list them; they are
persisted, backed up and restored with the database.

## Limits

Security statements are applied immediately and are not undone by `ROLLBACK`. Not implemented: certificates and signed
modules, Always Encrypted, auditing, contained databases, password policy and Windows/Azure AD authentication.
