# The hierarchyid type

`hierarchyid` holds the position of a node in a tree: `/` is the root, `/1/` its first child, `/1/2/` the second child of that
(`src/hierarchyid.rs`). A value is `Value::Bin`, in the binary form SQL Server stores, so the values of a restored SQL Server
database (`Employee.OrganizationNode`) are used as they are.

```sql
CREATE TABLE org (node hierarchyid PRIMARY KEY, level AS node.GetLevel(), name VARCHAR(20));
INSERT INTO org VALUES (hierarchyid::GetRoot(), 'ceo'), ('/1/', 'cto'), (hierarchyid::Parse('/1/1/'), 'dev lead');

DECLARE @parent hierarchyid = (SELECT node FROM org WHERE name = 'cto'), @last hierarchyid;
SELECT @last = MAX(node) FROM org WHERE node.GetAncestor(1) = @parent;
INSERT INTO org VALUES (@parent.GetDescendant(@last, NULL), 'new');

SELECT name FROM org WHERE node.IsDescendantOf('/1/') = 1 ORDER BY node;   -- everyone below the cto, in tree order
```

**Methods:** `GetLevel()`, `ToString()`, `GetAncestor(n)`, `GetDescendant(child1, child2)`, `IsDescendantOf(parent)`,
`GetReparentedValue(old, new)`, and `hierarchyid::GetRoot()` and `hierarchyid::Parse(text)`. A string where a node is expected
(`IsDescendantOf('/1/')`, an `INSERT` into a hierarchyid column, `CAST('/1/' AS hierarchyid)`) is parsed. `CAST(node AS
varchar)` and `CONVERT` give the text form; a hierarchyid cast to `varbinary` gives the bytes. A `NULL` argument or receiver
gives `NULL`. A string that is not a node, and what is not supported (below), is error 6522. The type sorts and compares as the
tree is ordered, so `ORDER BY`, `=`, `<`, keys and indexes work on it; comparing a hierarchyid with a *string* is not converted
(write `node = hierarchyid::Parse('/1/')`). `Read`, `Write` and `IsNull`-style CLR methods are not there.

**The stored form.** A node is its labels one after another, as a prefix code, then zero bits up to a byte; the root is no bytes at
all. One label (a whole number) is a prefix that says how many bits follow, those bits, and a final `1`:

| prefix | bits | labels |
| --- | --- | --- |
| `01` | 2 | 0 – 3 |
| `100` | 2 | 4 – 7 |
| `101` | 3 | 8 – 15 |
| `110` | 8 | 16 – 271 |

`/1/` is `0x58`, `/1/1/` is `0x5AC0`, `/3/1/24/` is `0x7AF044`. This table was found from the 301 values of the AdventureWorks sample
(`Employee.OrganizationNode`, `Document.DocumentNode`, which use labels up to 30): all of them decode, every parent is
present, and the byte order is the tree order, which is why the bytes can be compared as they are.

**What is not supported**, because it is not in any backup that was available and cannot be worked out from the rest of the form,
so that it is an error and not a guess:

* labels above 271 (the next prefixes of SQL Server's table are not known here);
* negative labels;
* the fractional labels (`/1.1/`) that SQL Server uses to insert a node between two siblings that have no label between them. So
  `GetDescendant(child1, child2)` needs a whole label between `child1` and `child2` (it gives `child1 + 1`) and says so when there
  is none; `GetDescendant(NULL, NULL)` is label 1, `GetDescendant(c, NULL)` is the label after `c`, `GetDescendant(NULL, c)` the
  one before it. SQL Server may pick other labels where it has a choice.

A backup that contains such a node (not the sample) fails to read it: the value is an error where it is used.
