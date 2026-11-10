# tSQLt-style unit tests in T-SQL

[tSQLt](https://tsqlt.org) is the usual unit-testing framework for T-SQL: tests are stored procedures in a *test class*
(a schema), run in a transaction that is rolled back, and use assertion procedures. The real tSQLt cannot be installed
here, because parts of it are CLR assemblies, which this server does not have (and its installer script depends on
SQL Server internals). `tests/tsqlt/tsqlt.sql` is a compatible subset written in T-SQL, and the other files in that
directory are test suites that use it. `cargo test --test tsqlt` loads the framework and a suite into a fresh server,
runs `tSQLt.RunAll` and requires every test to succeed; a failing test is reported with its name and message.

The point is to check the server against T-SQL's documented behaviour using T-SQL itself, and the suites are written so
that they would also run on a real SQL Server (arguments of `EXEC` are variables and constants, not expressions).

## The framework

| | |
| --- | --- |
| `tSQLt.NewTestClass 'Name'` / `DropClass` | create or drop a test class (a schema); procedures named `test...` in it are test cases, an optional `SetUp` runs before each |
| `tSQLt.Run 'Class'` / `'Class.test name'` / `RunTestClass` / `RunAll` | run tests; results are in `tSQLt.TestResult` (`Class`, `TestCase`, `Result` = `Success`, `Failure` or `Error`, `Msg`) |
| `AssertEquals`, `AssertNotEquals` | `sql_variant` arguments, with NULL equal to NULL, and the message `Expected: <x> but was: <y>` |
| `AssertEqualsString`, `AssertLike` | text |
| `AssertEqualsTable` | two tables or temp tables hold the same rows, duplicates counted |
| `AssertEmptyTable`, `AssertObjectExists`, `Fail` | |
| `ExpectException`, `ExpectNoException` | the next error must (not) happen; matches message, message pattern, error number, severity and state |
| `FakeTable` | replaces a table by an empty copy with the same columns and no constraints (rename + `SELECT ... INTO`) |
| `SpyProcedure` | replaces a procedure by one that records its arguments in `<name>_SpyProcedureLog` and optionally runs a command |

A test fails when an assertion raises error 50000 with state 10 (as tSQLt does) and errors when anything else is raised.
Each test runs inside a transaction that is rolled back, which is why `FakeTable` and `SpyProcedure` need DDL (`sp_rename`,
`CREATE`) to be transactional, and why the suites double as a check that it is.

## The suites

| File | Tests | Covers |
| --- | ---: | --- |
| `framework_tests.sql` | 21 | the framework itself: every assertion, expectation, `FakeTable`, `SpyProcedure`, `SetUp`, isolation between tests |
| `dynamic_sql_tests.sql` | 40 | `EXEC (string)` and `sp_executesql`: parameters, output parameters, defaults, missing parameters, variable and temp-table scoping, `QUOTENAME`, injection (a parameter neutralises it, concatenation does not, `QUOTENAME` protects an identifier), nesting, statements over 4,000 characters, errors caught by the caller, `USE` and `SET` inside, permissions of the caller, dynamic `PIVOT`, shape changes between calls |
| `error_tests.sql` | 47 | `TRY` / `CATCH`, `ERROR_*()`, `THROW`, `RAISERROR` formatting, every constraint error with its number and severity, statement atomicity, triggers raising errors, conversion and overflow errors, name resolution errors, procedure call errors, `@@TRANCOUNT`, savepoints, transactional DDL, `XACT_ABORT`, `XACT_STATE` |
| `programmability_tests.sql` | 47 | table variables, temp tables, cursors, loops, `CASE` / `IIF` / `CHOOSE` / `GOTO`, scalar, inline and multi-statement functions, recursive CTEs, windows, `PIVOT` / `UNPIVOT`, `MERGE` and `OUTPUT`, identity and sequences, computed columns, triggers, views (updatable, `WITH CHECK OPTION`), filtered unique indexes, table types, alias types, `STRING_SPLIT` / `STRING_AGG` / `FOR XML` / JSON, `TOP WITH TIES`, `OFFSET`, set operators with NULLs, `ROLLUP` / `CUBE`, `SELECT` assignment, procedure nesting limit, synonyms, catalog views |
| `type_tests.sql` | 41 | integer and decimal arithmetic, rounding, `FLOAT` against `DECIMAL`, type precedence, NULL logic, aggregates and NULLs, collation (trailing spaces, case, `COLLATE`), string, numeric, bit, date and hash functions, `CONVERT` styles, `DATEDIFF` boundaries, `DATEFIRST`, `datetime` rounding, binary and hex conversions, `HASHBYTES` digests, `uniqueidentifier`, `sql_variant`, `MONEY`, Unicode, `FORMAT` |

## Known differences that the suites leave out

These are behaviours of SQL Server that the engine does not reproduce, so no test asserts them:

* **`INSTEAD OF` triggers on views** are not supported (`CREATE TRIGGER ... ON view` fails with error 8197). Views that map
  onto one table are updatable by rewriting the statement, with `WITH CHECK OPTION` enforced.
* **Declared types of some values.** `SQL_VARIANT_PROPERTY` reports `decimal` for a decimal from a column, variable or cast and
  `numeric` for a literal or `CAST(.. AS NUMERIC(..))`, but a `NUMERIC` *column* or *variable* is reported as `decimal` (the two are one type here).
* **Expression limits.** Expressions nested more than 100 levels deep, or chains of more than 700 terms (`a OR b OR ...`),
  are refused with error 8632 (25 and 60 in a debug build, whose frames are much larger); SQL Server accepts deeper ones.
  Debug-build tests run on 64 MB thread stacks (`.cargo/config.toml`) so that procedure nesting to the limit of 32 works.
