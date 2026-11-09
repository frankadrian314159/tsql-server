# Collations

A collation says how strings compare: whether case and accents matter. The default, `SQL_Latin1_General_CP1_CI_AS`, ignores
case and trailing spaces and distinguishes accents. Any other collation can be chosen per database, per column, or per
expression:

```sql
CREATE DATABASE shop COLLATE Latin1_General_CS_AS;                  -- default for new columns and for literals
CREATE TABLE cust (name VARCHAR(40) COLLATE Latin1_General_CI_AI);   -- per column
ALTER TABLE cust ALTER COLUMN name VARCHAR(40) COLLATE Latin1_General_BIN2 NOT NULL;
SELECT * FROM cust WHERE name COLLATE Latin1_General_CS_AS = 'Smith';   -- per expression
SELECT name, collation_name FROM sys.columns;  -- also INFORMATION_SCHEMA.COLUMNS, DATABASEPROPERTYEX, sys.fn_helpcollations()
```

## Names

`<family>_<CI|CS>_<AI|AS>[_KS][_WS][_SC][_UTF8]`, `<family>_BIN` and `<family>_BIN2`. The family is any of the usual Windows or
SQL families (`Latin1_General`, `Latin1_General_100`, `SQL_Latin1_General_CP1`, `French`, `Turkish`, ...); an unknown name is
error 448. What is **implemented** is the sensitivity: `CI`/`CS` (case), `AI`/`AS` (accents), and binary (code point order). What
is not: a language's own alphabetical order (Spanish ñ after n, Danish å after z, German phone-book expansions) and kana/width
sensitivity (`_KS`, `_WS` are accepted and ignored). Letters sort by the code point of their unaccented, case-folded form,
lower case before upper case in the `CS` collations, and `BIN`/`BIN2` both compare code points. Trailing spaces never count.

## What a collation is applied to

Everywhere strings are compared, ordered or matched: `=`, `<>`, `<`, `>`, `BETWEEN`, `IN`, `LIKE` (including `[a-c]`
ranges), `CASE`, `ORDER BY`, `GROUP BY`, `DISTINCT`, `COUNT(DISTINCT ...)`, `MIN`/`MAX`, `UNION`/`INTERSECT`/`EXCEPT`, hash
and index joins, unique and primary keys, foreign keys, and index seeks and range scans. Views, derived tables, `SELECT INTO`
and the pseudo-tables of triggers carry their columns' collations.

Which collation a comparison uses follows SQL Server's rules: an explicit `COLLATE` wins; otherwise the collation of a column
involved; literals and variables take the other side's (or the current database's default when both sides are literals). Two
different column collations that no `COLLATE` reconciles are error 468. A string function such as `UPPER`, `LTRIM`, `SUBSTRING`,
`CONCAT` or `COALESCE` keeps the collation of its argument.

## How it works

`src/collation.rs` turns a string into a **sort key**: a string that the built-in comparison orders and equates exactly as the
collation would (`letters \u1 accents \u1 case`, so letters decide first, accents next, case last). The engine compares or
hashes keys instead of strings wherever a non-default collation is involved, so the default collation costs nothing and the
others work in every operator, including the streaming and spilling executors. An index on a column with a non-default
collation stores the keys, so its order and uniqueness follow the collation; index bounds and key-range locks are converted the
same way. Changing a column's collation rebuilds its indexes.

## Limits

* Collation of a subquery's result, of `CASE` results and of function results other than those listed is not derived; such an
  expression compares with the built-in rules unless a `COLLATE` or the other operand says otherwise.
* `PARTITION BY` / `ORDER BY` inside `OVER (...)`, `STRING_AGG ... WITHIN GROUP`, `CHARINDEX`, `PATINDEX`, `REPLACE` and
  `STRING_SPLIT` use the built-in (case-insensitive) rules.
* `IN (subquery)` and `= ANY (subquery)` use the left operand's collation only.
* Changing a database's collation (`ALTER DATABASE ... COLLATE`) affects only objects created afterwards; existing columns keep
  the collation they were created with, as in SQL Server.
* Setting a non-default collation changes the on-disk format of its indexes: data directories written by an older build do
  not open.
