//! The helpers behind the shell and the GUI: catalog introspection, grids, CSV, scripts.
mod common;
use common::*;
use tsql_server::inspect::ObjectKind;
use tsql_server::tools;

fn demo() -> tsql_server::Connection {
    let mut c = conn();
    script(
        &mut c,
        "
CREATE TABLE dbo.cust (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(50) NOT NULL, email VARCHAR(80) NULL DEFAULT 'none', CONSTRAINT uq_email UNIQUE (email))
GO
CREATE TABLE dbo.ord (id INT PRIMARY KEY, cust_id INT NOT NULL, amt DECIMAL(10,2) NULL, CONSTRAINT ck_amt CHECK (amt > 0), CONSTRAINT fk_ord_cust FOREIGN KEY (cust_id) REFERENCES dbo.cust(id) ON DELETE CASCADE)
GO
CREATE INDEX ix_ord_amt ON dbo.ord (amt DESC, cust_id)
GO
CREATE VIEW dbo.big_orders AS SELECT id, amt FROM dbo.ord WHERE amt > 100
GO
CREATE PROCEDURE dbo.hello AS SELECT 1
GO
CREATE FUNCTION dbo.twice(@x INT) RETURNS INT AS BEGIN RETURN @x * 2 END
GO
CREATE SEQUENCE dbo.seq1
GO
",
    );
    c
}

#[test]
fn table_info_describes_columns_keys_indexes_and_foreign_keys() {
    let c = demo();
    let t = c.table_info("cust").unwrap();
    assert_eq!((t.schema.as_str(), t.name.as_str()), ("dbo", "cust"));
    assert_eq!(t.quoted(), "[dbo].[cust]");
    let cols: Vec<(&str, &str, bool)> = t.columns.iter().map(|c| (c.name.as_str(), c.ty.as_str(), c.nullable)).collect();
    assert_eq!(cols, vec![("id", "int", false), ("name", "nvarchar(50)", false), ("email", "varchar(80)", true)]);
    assert_eq!(t.columns[0].identity, Some((1, 1)));
    assert!(t.columns[0].primary_key && t.columns[2].has_default);
    assert_eq!(t.indexes.iter().filter(|i| i.primary_key).count(), 1);
    assert!(t.indexes.iter().any(|i| i.name == "uq_email" && i.unique));

    // schema-qualified, bracketed and case-insensitive names all resolve
    for n in ["dbo.ORD", "[dbo].[ord]", "ord"] {
        assert!(c.table_info(n).is_some(), "{n}");
    }
    let o = c.table_info("ord").unwrap();
    let ix = o.indexes.iter().find(|i| i.name == "ix_ord_amt").unwrap();
    assert_eq!(ix.columns, vec![("amt".to_string(), true), ("cust_id".to_string(), false)]);
    assert!(!ix.unique);
    let fk = &o.foreign_keys[0];
    assert_eq!(fk.name, "fk_ord_cust");
    assert_eq!(fk.columns, vec!["cust_id".to_string()]);
    assert_eq!(fk.ref_columns, vec!["id".to_string()]);
    assert_eq!(fk.ref_table, "dbo.cust");
    assert_eq!(o.checks, vec!["ck_amt".to_string()]);
    assert!(c.table_info("nope").is_none());
    assert!(c.table_info("big_orders").is_none(), "a view is not a table");
}

#[test]
fn objects_and_definitions() {
    let c = demo();
    let objs = c.objects();
    let kinds = |k: ObjectKind| objs.iter().filter(|o| o.kind == k).map(|o| o.qualified()).collect::<Vec<_>>();
    assert_eq!(kinds(ObjectKind::Table), vec!["dbo.cust", "dbo.ord"]);
    assert_eq!(kinds(ObjectKind::View), vec!["dbo.big_orders"]);
    assert_eq!(kinds(ObjectKind::Procedure), vec!["dbo.hello"]);
    assert_eq!(kinds(ObjectKind::Function), vec!["dbo.twice"]);
    assert_eq!(kinds(ObjectKind::Sequence), vec!["dbo.seq1"]);
    assert!(c.definition("big_orders").unwrap().contains("amt > 100"));
    assert!(c.definition("dbo.hello").unwrap().to_uppercase().contains("PROCEDURE"));
    assert!(c.definition("cust").is_none());
    assert!(c.databases().contains(&"master".to_string()));
    assert_eq!(c.login(), "sa");
}

#[test]
fn transaction_count_follows_begin_and_commit() {
    let mut c = conn();
    assert_eq!(c.transaction_count(), 0);
    exec(&mut c, "BEGIN TRAN");
    exec(&mut c, "BEGIN TRAN");
    assert_eq!(c.transaction_count(), 2);
    exec(&mut c, "ROLLBACK");
    assert_eq!(c.transaction_count(), 0);
}

#[test]
fn grid_aligns_numbers_and_shows_nulls_and_row_counts() {
    let mut c = conn();
    let r = exec(&mut c, "SELECT 1 AS id, N'ab' AS name, CAST(2.50 AS DECIMAL(5,2)) AS amt UNION ALL SELECT 22, NULL, NULL");
    let g = tools::format_grid(r.resultsets()[0], 60);
    assert_eq!(g, "id │ name │  amt\n───┼──────┼─────\n 1 │ ab   │ 2.50\n22 │ NULL │ NULL\n(2 rows)\n");
    let r = exec(&mut c, "SELECT REPLICATE('x', 100) AS long_one, 1 AS n WHERE 1 = 0");
    assert_eq!(tools::format_grid(r.resultsets()[0], 60), "long_one │ n\n─────────┼──\n(0 rows)\n");
    let r = exec(&mut c, "SELECT REPLICATE('x', 100) AS v");
    let g = tools::format_grid(r.resultsets()[0], 10);
    assert!(g.contains("xxxxxxxxx…"), "{g}");
    let r = exec(&mut c, "SELECT 'a' + CHAR(10) + 'b' AS v");
    assert!(tools::format_grid(r.resultsets()[0], 60).contains("a⏎b"));
}

#[test]
fn csv_round_trips_quotes_commas_newlines_and_nulls() {
    let mut c = conn();
    let r = exec(&mut c, "SELECT 1 AS id, N'a,b' AS t UNION ALL SELECT 2, N'say \"hi\"' UNION ALL SELECT 3, N'two' + CHAR(13) + CHAR(10) + N'lines' UNION ALL SELECT 4, NULL");
    let csv = tools::to_csv(r.resultsets()[0]);
    assert!(csv.starts_with("id,t\r\n1,\"a,b\"\r\n2,\"say \"\"hi\"\"\"\r\n"), "{csv:?}");
    let rows = tools::parse_csv(&csv).unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[1], vec!["1", "a,b"]);
    assert_eq!(rows[2], vec!["2", "say \"hi\""]);
    // a line break inside a quoted field is kept exactly
    assert_eq!(rows[3], vec!["3", "two\r\nlines"]);
    assert_eq!(rows[4], vec!["4", ""]);
    assert!(tools::parse_csv("a,b\n\"unterminated").is_err());
    assert_eq!(tools::parse_csv("\u{feff}a,b\n\n1,2\n").unwrap(), vec![vec!["a", "b"], vec!["1", "2"]]);
    assert_eq!(tools::parse_csv("").unwrap().len(), 0);
}

#[test]
fn import_appends_rows_with_typed_literals_and_is_all_or_nothing() {
    let mut c = demo();
    let csv = "name,email\nAnn,ann@x.com\n\"O'Neil, Pat\",\nBob,bob@x.com\n";
    assert_eq!(tools::import_csv(&mut c, "cust", csv, false).unwrap(), 3);
    assert_eq!(rows(&mut c, "SELECT id, name, email FROM cust ORDER BY id"), vec![vec!["1", "Ann", "ann@x.com"], vec!["2", "O'Neil, Pat", "NULL"], vec!["3", "Bob", "bob@x.com"]]);
    // a bad record rolls the whole import back
    let bad = "id,cust_id,amt\n1,1,10.5\n2,1,not-a-number\n";
    let e = tools::import_csv(&mut c, "ord", bad, false).unwrap_err();
    assert!(e.message.contains("record 3") && e.message.contains("amt"), "{}", e.message);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord"), "0");
    assert_eq!(c.transaction_count(), 0);
    // constraint violations are reported and leave nothing behind
    let e = tools::import_csv(&mut c, "ord", "id,cust_id,amt\n1,99,5\n", false).unwrap_err();
    assert_eq!(e.number, 547);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord"), "0");
    assert_eq!(tools::import_csv(&mut c, "ord", "id,cust_id,amt\n1,1,10.5\n2,3,\n", false).unwrap(), 2);
    assert_eq!(rows(&mut c, "SELECT id, amt FROM ord ORDER BY id"), vec![vec!["1", "10.50"], vec!["2", "NULL"]]);
    assert_eq!(tools::import_csv(&mut c, "ord", "nope\n1\n", false).unwrap_err().number, 207);
    assert_eq!(tools::import_csv(&mut c, "missing", "a\n1\n", false).unwrap_err().number, 208);
    assert!(tools::import_csv(&mut c, "ord", "id,amt\n1\n", false).unwrap_err().message.contains("record 2"));
    // inside the caller's own transaction nothing is committed behind its back
    exec(&mut c, "BEGIN TRAN");
    tools::import_csv(&mut c, "ord", "id,cust_id\n10,1\n", false).unwrap();
    assert_eq!(c.transaction_count(), 1);
    exec(&mut c, "ROLLBACK");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM ord WHERE id = 10"), "0");
}

#[test]
fn import_can_create_the_table_and_infers_types() {
    let mut c = conn();
    let csv = "id,big,price,seen,at,flag,note\n1,3000000000,1.5,2024-01-31,2024-01-31 10:20:30,true,hello\n2,4,2,2024-02-01,2024-02-01 00:00,false,\n";
    assert_eq!(tools::import_csv(&mut c, "dbo.imp", csv, true).unwrap(), 2);
    let t = c.table_info("imp").unwrap();
    let types: Vec<&str> = t.columns.iter().map(|c| c.ty.as_str()).collect();
    assert_eq!(types, vec!["int", "bigint", "float", "date", "datetime2(7)", "nvarchar(10)", "nvarchar(10)"]);
    assert_eq!(rows(&mut c, "SELECT id, big, price, seen FROM imp ORDER BY id"), vec![vec!["1", "3000000000", "1.5", "2024-01-31"], vec!["2", "4", "2", "2024-02-01"]]);
    // creating over an existing table fails and imports nothing
    assert!(tools::import_csv(&mut c, "dbo.imp", csv, true).is_err());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM imp"), "2");
    // names with brackets and odd characters are quoted
    tools::import_csv(&mut c, "we ird", "a b,c]d\n1,2\n", true).unwrap();
    assert_eq!(scalar(&mut c, "SELECT [a b] + [c]]d] FROM [we ird]"), "3");
}

#[test]
fn script_table_recreates_the_structure() {
    let c = demo();
    let cust = tools::script_table(&c.table_info("cust").unwrap());
    let ord = tools::script_table(&c.table_info("ord").unwrap());
    assert!(cust.contains("[id] int IDENTITY(1,1) NOT NULL"), "{cust}");
    assert!(cust.contains("CONSTRAINT [uq_email] UNIQUE ([email])"), "{cust}");
    assert!(cust.contains("DEFAULT ("), "{cust}");
    assert!(ord.contains("CREATE INDEX [ix_ord_amt] ON [dbo].[ord] ([amt] DESC, [cust_id]);"), "{ord}");
    assert!(ord.contains("FOREIGN KEY ([cust_id]) REFERENCES [dbo].[cust] ([id]) ON DELETE CASCADE"), "{ord}");
    assert!(ord.contains("ADD CONSTRAINT [ck_amt] CHECK ("), "{ord}");
    // the script runs on a fresh server and yields the same structure
    let mut d = conn();
    script(&mut d, &format!("{cust}\nGO\n{ord}\nGO"));
    for t in ["cust", "ord"] {
        let (a, b) = (c.table_info(t).unwrap(), d.table_info(t).unwrap());
        let cols = |t: &tsql_server::inspect::TableInfo| t.columns.iter().map(|c| (c.name.clone(), c.ty.clone(), c.nullable, c.identity)).collect::<Vec<_>>();
        assert_eq!(cols(&a), cols(&b), "{t}");
        assert_eq!(a.indexes.len(), b.indexes.len(), "{t}");
        assert_eq!(a.foreign_keys.len(), b.foreign_keys.len(), "{t}");
        let exprs = |t: &tsql_server::inspect::TableInfo| (t.columns.iter().map(|c| c.default_text.clone()).collect::<Vec<_>>(), t.check_defs.iter().map(|k| (k.name.clone(), k.text.clone())).collect::<Vec<_>>());
        assert_eq!(exprs(&a), exprs(&b), "{t}");
    }
}

#[test]
fn script_keeps_defaults_checks_and_computed_columns_working() {
    let mut c = conn();
    script(&mut c, "CREATE TABLE ex (id INT PRIMARY KEY, qty INT NOT NULL CONSTRAINT df_qty DEFAULT (1 + 2), price DECIMAL(8,2) NOT NULL DEFAULT 5, total AS (qty * price), CONSTRAINT ck_qty CHECK (qty BETWEEN 0 AND 100 AND id > 0));
GO
ALTER TABLE ex ADD note VARCHAR(5) DEFAULT 'n/a' CHECK (LEN(note) > 1);
GO
ALTER TABLE ex NOCHECK CONSTRAINT ck_qty;
GO");
    let sql = tools::script_table(&c.table_info("ex").unwrap());
    assert!(sql.contains("[total] AS (qty * price)"), "{sql}");
    assert!(sql.contains("CONSTRAINT [df_qty] DEFAULT (1 + 2)"), "{sql}");
    assert!(sql.contains("NOCHECK CONSTRAINT [ck_qty]"), "{sql}");
    let mut d = conn();
    script(&mut d, &format!("{sql}
GO"));
    exec(&mut d, "INSERT ex (id) VALUES (1)");
    assert_eq!(rows(&mut d, "SELECT qty, price, total, note FROM ex"), vec![vec!["3", "5.00", "15.00", "n/a"]]);
    assert!(d.execute("INSERT ex (id, note) VALUES (2, 'x')").first_error().is_some(), "column check must survive scripting");
}

#[test]
fn literals_read_back_as_the_same_values() {
    let mut c = conn();
    let r = exec(&mut c, "SELECT 5 AS a, CAST(1.50 AS DECIMAL(5,2)) AS b, N'it''s' AS c, 0xCAFE AS d, CAST('2020-02-29 13:45:10.123' AS DATETIME2(3)) AS e, NEWID() AS f, NULL AS g, CAST(1 AS BIT) AS h, 1.25E0 AS i");
    let rs = r.resultsets()[0];
    let lits: Vec<String> = rs.rows[0].iter().map(tools::sql_literal).collect();
    assert_eq!(lits[0], "5");
    assert_eq!(lits[2], "N'it''s'");
    assert_eq!(lits[3], "0xCAFE");
    assert_eq!(lits[6], "NULL");
    let sql = format!("SELECT {}", lits.iter().enumerate().map(|(i, l)| format!("{} AS c{}", l, i)).collect::<Vec<_>>().join(", "));
    let again = exec(&mut c, &sql);
    for (a, b) in rs.rows[0].iter().zip(again.resultsets()[0].rows[0].iter()) {
        assert_eq!(a.to_string(), b.to_string());
    }
}
