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

## Limits

Column-level permissions are rejected (error 15477). Security statements are applied immediately and are not
undone by `ROLLBACK`. No certificates, row-level security, dynamic data masking or auditing.
