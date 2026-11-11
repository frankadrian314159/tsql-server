//! The database inside a native SQL Server backup: tables, columns and rows. These tests use the sample backup
//! `resources/AdventureWorks2025.bak` (not part of the repository) and do nothing when it is absent.
use tsql_server::mssqlbak::Expanded;
use tsql_server::sqlbak::{Backup, TableMeta};
use tsql_server::types::Value;

fn open() -> Option<Backup<std::io::BufReader<std::fs::File>>> {
    let f = std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/AdventureWorks2025.bak")).ok()?;
    let ex = Expanded::open(std::io::BufReader::new(f)).unwrap().unwrap();
    Some(Backup::open(ex).unwrap())
}

fn table<'a>(ts: &'a [TableMeta], schema: &str, name: &str) -> &'a TableMeta {
    ts.iter().find(|t| t.schema == schema && t.name == name).unwrap_or_else(|| panic!("no table {}.{}", schema, name))
}

fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => panic!("not a string: {:?}", other),
    }
}

#[test]
fn the_database_and_its_tables_are_found() {
    let Some(mut b) = open() else { return };
    assert_eq!(b.db_name, "AdventureWorks2025");
    let ts = b.tables().unwrap();
    assert_eq!(ts.len(), 73);
    let p = table(&ts, "Person", "Person");
    assert_eq!(p.cols.len(), 13);
    assert_eq!(p.cols[0].name, "BusinessEntityID");
    assert_eq!(p.rows, 19972);
    let a = table(&ts, "Person", "AddressType");
    assert_eq!(a.cols[0].identity, Some((1, 1, 6)), "seed, increment and last value of the identity column");
    // computed columns are known but not stored
    let d = table(&ts, "Sales", "SalesOrderDetail");
    assert!(d.cols.iter().find(|c| c.name == "LineTotal").unwrap().computed);
}

#[test]
fn every_table_has_as_many_rows_as_the_catalog_says() {
    let Some(mut b) = open() else { return };
    let ts = b.tables().unwrap();
    let mut total = 0;
    for t in &ts {
        let r = b.rows(t).unwrap();
        assert_eq!(r.rows.len() as u64, t.rows, "{}.{}", t.schema, t.name);
        total += r.rows.len();
    }
    assert_eq!(total, 760_167);
}

#[test]
fn rows_have_their_values() {
    let Some(mut b) = open() else { return };
    let ts = b.tables().unwrap();
    // Production.Culture: nchar key, nvarchar name, datetime
    let r = b.rows(table(&ts, "Production", "Culture")).unwrap();
    assert_eq!(r.rows.len(), 8);
    assert_eq!(text(&r.rows[1][0]).trim_end(), "ar");
    assert_eq!(text(&r.rows[1][1]), "Arabic");
    // HumanResources.Employee: bit columns after a computed one, dates, a varchar-less row
    let r = b.rows(table(&ts, "HumanResources", "Employee")).unwrap();
    let e = &r.rows[0];
    let names: Vec<&str> = table(&ts, "HumanResources", "Employee").cols.iter().filter(|c| !c.computed).map(|c| c.name.as_str()).collect();
    let at = |n: &str| &e[names.iter().position(|x| *x == n).unwrap()];
    assert_eq!(text(at("JobTitle")), "Chief Executive Officer");
    assert_eq!(text(at("LoginID")), "adventure-works\\ken0");
    assert!(matches!(at("SalariedFlag"), Value::Bit(true)));
    assert_eq!(at("BirthDate").to_string(), "1969-01-29");
    // Production.Product: money, NULL decimals, two bit columns, datetime
    let r = b.rows(table(&ts, "Production", "Product")).unwrap();
    assert_eq!(text(&r.rows[0][1]), "Adjustable Race");
    assert_eq!(r.rows[2][3].to_string(), "1", "BB Ball Bearing is made in house (MakeFlag)");
    // Sales.SalesOrderHeader: the first order
    let t = table(&ts, "Sales", "SalesOrderHeader");
    let r = b.rows(t).unwrap();
    let names: Vec<&str> = t.cols.iter().filter(|c| !c.computed).map(|c| c.name.as_str()).collect();
    let row = r.rows.iter().find(|row| matches!(row[0], Value::Int(43659))).unwrap();
    assert_eq!(row[names.iter().position(|x| *x == "SubTotal").unwrap()].to_string(), "20565.6206");
    assert_eq!(row[names.iter().position(|x| *x == "OrderDate").unwrap()].to_string(), "2022-05-30 00:00:00.000");
}

#[test]
fn xml_columns_are_decoded_and_off_row_values_are_counted() {
    let Some(mut b) = open() else { return };
    let ts = b.tables().unwrap();
    // typed XML (bound to a schema collection): strings, decimals, a float, years
    let t = table(&ts, "Sales", "Store");
    let r = b.rows(t).unwrap();
    let ci = t.cols.iter().filter(|c| !c.computed).position(|c| c.name == "Demographics").unwrap();
    assert_eq!(r.skipped.get("Demographics"), None);
    let x = text(&r.rows[0][ci]);
    assert!(x.starts_with("<StoreSurvey xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/adventure-works/StoreSurvey\">"), "{}", x);
    assert!(x.contains("<AnnualSales>800000</AnnualSales>") && x.contains("<BankName>United Security</BankName>") && x.contains("<SquareFeet>21000</SquareFeet>"), "{}", x);
    // dates and negative decimals
    let t = table(&ts, "Person", "Person");
    let r = b.rows(t).unwrap();
    let ci = t.cols.iter().filter(|c| !c.computed).position(|c| c.name == "Demographics").unwrap();
    assert_eq!(r.skipped.get("Demographics"), None);
    let with_dates: Vec<String> = r.rows.iter().map(|row| row[ci].to_string()).filter(|x| x.contains("<BirthDate>")).collect();
    assert!(with_dates.len() > 15000);
    assert!(with_dates.iter().any(|x| x.contains("<DateFirstPurchase>2003-09-01Z</DateFirstPurchase><BirthDate>1961-02-23Z</BirthDate>")));
    assert!(with_dates.iter().any(|x| x.contains("<TotalPurchaseYTD>-28.99</TotalPurchaseYTD>")));
    // untyped XML
    let t = table(&ts, "dbo", "DatabaseLog");
    let r = b.rows(t).unwrap();
    let ci = t.cols.iter().filter(|c| !c.computed).position(|c| c.name == "XmlEvent").unwrap();
    assert!(text(&r.rows[0][ci]).starts_with("<EVENT_INSTANCE><EventType>CREATE_TABLE</EventType>"));
    // values stored off the row are not read yet: counted, and NULL
    assert_eq!(r.skipped.get("XmlEvent"), Some(&26));
    let r = b.rows(table(&ts, "Production", "ProductPhoto")).unwrap();
    assert_eq!(r.skipped.get("LargePhoto"), Some(&98));
}

/// Takes a couple of minutes in a debug build (it builds 760,000 rows through the SQL path): `cargo test --release --test sqlbak -- --ignored`.
#[test]
#[ignore]
fn restore_database_from_a_sql_server_backup() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/resources/AdventureWorks2025.bak");
    if !std::path::Path::new(path).exists() {
        return;
    }
    let server = tsql_server::Server::new();
    let mut c = server.connect();
    let r = c.execute(&format!("RESTORE VERIFYONLY FROM DISK = '{}'", path));
    assert!(r.errors().is_empty(), "{:?}", r.errors());
    let r = c.execute(&format!("RESTORE HEADERONLY FROM DISK = '{}'", path));
    let rs = r.resultsets();
    assert_eq!(rs[0].rows[0][0].to_string(), "AdventureWorks2025-Full Database Backup");
    assert_eq!(rs[0].rows[0][4].to_string(), "AdventureWorks2025");
    let r = c.execute(&format!("RESTORE DATABASE aw FROM DISK = '{}'", path));
    assert!(r.errors().is_empty(), "{:?}", r.errors());
    let n = |c: &mut tsql_server::Connection, sql: &str| c.execute(sql).resultsets()[0].rows[0][0].to_string();
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.SalesOrderDetail"), "121317");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Person.Person"), "19972");
    assert_eq!(n(&mut c, "SELECT LastName FROM aw.Person.Person WHERE BusinessEntityID = 1"), "Sánchez");
    assert_eq!(n(&mut c, "SELECT SUM(UnitPrice * OrderQty) FROM aw.Sales.SalesOrderDetail"), "110373889.3134");
    // dates, money, decimal, bit and identity columns
    assert_eq!(n(&mut c, "SELECT CONVERT(varchar(10), OrderDate, 120) FROM aw.Sales.SalesOrderHeader WHERE SalesOrderID = 43659"), "2022-05-30");
    assert_eq!(n(&mut c, "SELECT SubTotal FROM aw.Sales.SalesOrderHeader WHERE SalesOrderID = 43659"), "20565.6206");
    assert_eq!(n(&mut c, "SELECT SalariedFlag FROM aw.HumanResources.Employee WHERE BusinessEntityID = 1"), "1");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Production.BillOfMaterials WHERE ProductAssemblyID IS NULL"), n(&mut c, "SELECT COUNT(*) FROM aw.Production.BillOfMaterials WHERE ProductAssemblyID IS NULL AND ComponentID > 0"));
    // xml columns
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.Store WHERE Demographics IS NOT NULL"), "701");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.Store WHERE CAST(Demographics AS nvarchar(max)) LIKE '%<AnnualSales>800000</AnnualSales>%'"), "166");
    assert!(n(&mut c, "SELECT COUNT(*) FROM aw.Person.Person WHERE CAST(Demographics AS nvarchar(max)) LIKE '%<DateFirstPurchase>2003-09-01Z</DateFirstPurchase>%'").parse::<i64>().unwrap() > 0);
    // the native format of this server still works: back the restored database up and restore it under another name
    let tmp = std::env::temp_dir().join(format!("tsql-sqlbak-test-{}.bak", std::process::id()));
    let t = tmp.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    assert!(c.execute(&format!("BACKUP DATABASE aw TO DISK = '{}'", t)).errors().is_empty());
    assert!(c.execute(&format!("RESTORE DATABASE aw2 FROM DISK = '{}'", t)).errors().is_empty());
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw2.Sales.SalesOrderDetail"), "121317");
    let _ = std::fs::remove_file(&tmp);
}
