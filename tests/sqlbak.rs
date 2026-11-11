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
    // long XML stored off the row
    assert_eq!(r.skipped.get("XmlEvent"), None);
    let long = r.rows.iter().map(|row| row[ci].to_string()).max_by_key(|x| x.len()).unwrap();
    assert!(long.len() > 8000 && long.starts_with("<EVENT_INSTANCE>") && long.ends_with("</EVENT_INSTANCE>"));
    // typed XML with comments, processing instructions, declared namespaces, decimal attributes and dates
    let t = table(&ts, "Production", "ProductModel");
    let r = b.rows(t).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    let col = |n: &str| t.cols.iter().filter(|c| !c.computed).position(|c| c.name == n).unwrap();
    assert!(r.rows.iter().any(|row| row[col("Instructions")].to_string().contains("LaborHours=\"2.5\"")));
    assert!(r.rows.iter().any(|row| row[col("CatalogDescription")].to_string().contains("<p1:ProductDescription")));
    let t = table(&ts, "Production", "Illustration");
    let r = b.rows(t).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    assert!(r.rows.iter().any(|row| row[1].to_string().contains("<!--") || row[1].to_string().contains("<?xml-stylesheet")));
    let r = b.rows(table(&ts, "HumanResources", "JobCandidate")).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
}

#[test]
fn indexes_keys_and_constraints_are_read() {
    let Some(mut b) = open() else { return };
    let ts = b.tables().unwrap();
    let count = |f: &dyn Fn(&TableMeta) -> usize| ts.iter().map(f).sum::<usize>();
    assert_eq!(count(&|t| t.indexes.iter().filter(|i| i.primary).count()), 71);
    assert_eq!(count(&|t| t.indexes.iter().filter(|i| i.unique_constraint).count()), 1);
    assert_eq!(count(&|t| t.indexes.len()), 165);
    assert_eq!(count(&|t| t.xml_indexes.len()), 8);
    assert_eq!(count(&|t| t.other_indexes.len()), 0);
    // XML indexes: the primary ones first, the secondary ones with the letter of their kind
    let p = table(&ts, "Person", "Person");
    assert_eq!(p.xml_indexes.len(), 5);
    assert!(p.xml_indexes[..2].iter().all(|x| x.primary) && p.xml_indexes[2..].iter().all(|x| !x.primary));
    let mut kinds: Vec<char> = p.xml_indexes.iter().filter_map(|x| x.secondary).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec!['P', 'R', 'V']);
    let demo = p.cols.iter().find(|c| c.name == "Demographics").unwrap().colid;
    assert!(p.xml_indexes.iter().filter(|x| !x.primary).all(|x| x.column == demo));
    assert_eq!(count(&|t| t.foreign_keys.len()), 90);
    assert_eq!(count(&|t| t.defaults.len()), 152);
    assert_eq!(count(&|t| t.checks.len()), 89);
    // key columns in order, INCLUDE columns
    let t = table(&ts, "Production", "ProductReview");
    let ix = t.indexes.iter().find(|i| i.name == "IX_ProductReview_ProductID_Name").unwrap();
    assert!(!ix.clustered && !ix.unique && ix.keys == vec![(2, false), (3, false)] && ix.included == vec![7]);
    let d = table(&ts, "Sales", "SalesOrderDetail");
    let pk = d.indexes.iter().find(|i| i.primary).unwrap();
    assert!(pk.clustered && pk.unique && pk.keys == vec![(1, false), (2, false)]);
    // foreign keys: columns in key order, referenced table, cascade
    let fk = d.foreign_keys.iter().find(|f| f.name == "FK_SalesOrderDetail_SpecialOfferProduct_SpecialOfferIDProductID").unwrap();
    let sop = table(&ts, "Sales", "SpecialOfferProduct");
    assert_eq!(fk.ref_table, sop.id);
    assert_eq!(fk.columns, vec![(6, 1), (5, 2)], "(SpecialOfferID, ProductID) of the detail against those of the special offer");
    assert!(!fk.cascade_delete);
    assert!(d.foreign_keys.iter().any(|f| f.name == "FK_SalesOrderDetail_SalesOrderHeader_SalesOrderID" && f.cascade_delete));
    // definitions
    assert!(d.defaults.iter().any(|c| c.name == "DF_SalesOrderDetail_ModifiedDate" && c.column == 11 && c.definition == "(getdate())"));
    assert!(d.checks.iter().any(|c| c.name == "CK_SalesOrderDetail_OrderQty" && c.definition == "([OrderQty]>(0))"));
    // a computed column has its expression
    let line = d.cols.iter().find(|c| c.name == "LineTotal").unwrap();
    assert!(line.computed && !line.persisted && line.definition.as_deref().unwrap().contains("[UnitPrice]"));
}

#[test]
fn views_procedures_functions_and_triggers_are_read() {
    let Some(mut b) = open() else { return };
    let ms = b.modules().unwrap();
    let n = |k: &[u8; 2]| ms.iter().filter(|m| &m.kind == k).count();
    assert_eq!((n(b"V "), n(b"P "), n(b"FN"), n(b"TF"), n(b"TR")), (20, 10, 10, 1, 11));
    assert!(ms.iter().all(|m| m.schema != "sys" && m.definition.is_some()));
    let v = ms.iter().find(|m| m.name == "vEmployee").unwrap();
    assert_eq!(v.schema, "HumanResources");
    let d = v.definition.as_deref().unwrap();
    assert!(d.trim_start().starts_with("CREATE VIEW [HumanResources].[vEmployee]") && d.contains("FROM [HumanResources].[Employee] e"), "{}", d);
    // a trigger knows its table; a database trigger has none
    let t = ms.iter().find(|m| m.name == "iuPerson").unwrap();
    assert!(t.parent != 0 && t.definition.as_deref().unwrap().contains("AFTER INSERT, UPDATE"));
    assert_eq!(ms.iter().find(|m| m.name == "ddlDatabaseTriggerLog").unwrap().parent, 0);
    // text longer than a few kilobytes
    assert!(ms.iter().any(|m| m.definition.as_ref().is_some_and(|d| d.len() > 6000)));
}

#[test]
fn large_values_stored_off_the_row_are_read() {
    let Some(mut b) = open() else { return };
    let ts = b.tables().unwrap();
    // images: GIFs of up to tens of kilobytes spread over several pages
    let t = table(&ts, "Production", "ProductPhoto");
    let r = b.rows(t).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    let ci = t.cols.iter().filter(|c| !c.computed).position(|c| c.name == "LargePhoto").unwrap();
    let mut biggest = 0;
    for row in &r.rows {
        if let Value::Bin(g) = &row[ci] {
            assert_eq!(&g[..6], b"GIF89a");
            assert_eq!(*g.last().unwrap(), 0x3B, "a GIF ends with a trailer, so no fragment is missing");
            biggest = biggest.max(g.len());
        }
    }
    assert!(biggest > 16_000, "{}", biggest);
    // documents: word processor files
    let t = table(&ts, "Production", "Document");
    let r = b.rows(t).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    let ci = t.cols.iter().filter(|c| !c.computed).position(|c| c.name == "Document").unwrap();
    let fe = t.cols.iter().filter(|c| !c.computed).position(|c| c.name == "FileExtension").unwrap();
    assert!(r.rows.iter().any(|row| matches!(&row[ci], Value::Bin(d) if d.len() > 8000 && d.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]))));
    // folders have an empty extension (a trailing empty column is not stored), and are not NULL
    assert!(r.rows.iter().any(|row| matches!(&row[fe], Value::Str(s) if s.is_empty())));
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
    // keys, indexes and constraints work: a duplicate key, a missing parent, a check and a cascade
    let err = |c: &mut tsql_server::Connection, sql: &str| c.execute(sql).errors().len();
    assert_eq!(err(&mut c, "INSERT INTO aw.Person.AddressType (AddressTypeID, Name, rowguid, ModifiedDate) VALUES (1, 'x', NEWID(), GETDATE())"), 1, "PRIMARY KEY");
    assert_eq!(err(&mut c, "INSERT INTO aw.Sales.SalesOrderDetail (SalesOrderID, SalesOrderDetailID, OrderQty, ProductID, SpecialOfferID, UnitPrice) VALUES (999999999, 1, 1, 1, 1, 1)"), 1, "FOREIGN KEY");
    assert_eq!(err(&mut c, "UPDATE aw.Sales.SalesOrderDetail SET OrderQty = 0 WHERE SalesOrderDetailID = 1"), 1, "CHECK");
    assert!(c.execute("USE aw").errors().is_empty());
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE name = 'IX_ProductReview_ProductID_Name'"), "1");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.foreign_keys"), "90");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.xml_indexes"), "8");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.xml_indexes WHERE name = 'XMLPATH_Person_Demographics' AND secondary_type = 'P'"), "1");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.SalesOrderHeader WHERE SalesOrderID = 43659"), "1");
    assert_eq!(n(&mut c, "SELECT CAST(LineTotal AS decimal(19,4)) FROM aw.Sales.SalesOrderDetail WHERE SalesOrderID = 43659 AND SalesOrderDetailID = 1"), "2024.9940");
    let before = n(&mut c, "SELECT COUNT(*) FROM aw.Sales.SalesOrderDetail");
    assert_eq!(err(&mut c, "DELETE FROM aw.Sales.SalesOrderHeader WHERE SalesOrderID = 43659"), 0, "ON DELETE CASCADE");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.SalesOrderDetail WHERE SalesOrderID = 43659"), "0");
    assert!(before.parse::<i64>().unwrap() > 0);
    // functions, views, procedures and triggers
    assert_eq!(n(&mut c, "SELECT dbo.ufnLeadingZeros(42)"), "00000042");
    assert_eq!(n(&mut c, "SELECT AccountNumber FROM aw.Sales.Customer WHERE CustomerID = 1"), "AW00000001");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.HumanResources.vEmployee"), "290");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.views WHERE name = 'vSalesPerson'"), "1");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.triggers WHERE name = 'iWorkOrder'"), "1");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.procedures WHERE name = 'uspGetBillOfMaterials'"), "1");
    // hierarchyid columns, their computed levels and the indexes on them
    // (the first employee has no node in this sample, so no level)
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.HumanResources.Employee WHERE OrganizationNode IS NULL"), "1");
    assert_eq!(n(&mut c, "SELECT MIN(OrganizationLevel) FROM aw.HumanResources.Employee"), "1");
    assert_eq!(n(&mut c, "SELECT MAX(OrganizationLevel) FROM aw.HumanResources.Employee"), "4");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.HumanResources.Employee WHERE OrganizationNode.IsDescendantOf(hierarchyid::Parse('/3/')) = 1 AND OrganizationLevel = 1"), "1");
    assert_eq!(n(&mut c, "SELECT DocumentLevel FROM aw.Production.Document WHERE DocumentNode = hierarchyid::Parse('/1/2/')"), "2");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM sys.indexes WHERE name IN ('IX_Employee_OrganizationLevel_OrganizationNode', 'AK_Document_DocumentLevel_DocumentNode')"), "2");
    // views and a procedure that shred xml with .nodes() and search with full-text functions
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.vPersonDemographics"), n(&mut c, "SELECT COUNT(*) FROM aw.Person.Person WHERE Demographics IS NOT NULL"));
    assert!(n(&mut c, "SELECT COUNT(*) FROM aw.HumanResources.vJobCandidateEducation").parse::<i64>().unwrap() > 0);
    assert!(n(&mut c, "SELECT COUNT(*) FROM aw.Production.vProductModelInstructions").parse::<i64>().unwrap() > 0);
    assert!(c.execute("EXEC aw.dbo.uspSearchCandidateResumes N'Production'").errors().is_empty());
    // xml columns
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.Store WHERE Demographics IS NOT NULL"), "701");
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw.Sales.Store WHERE CAST(Demographics AS nvarchar(max)) LIKE '%<AnnualSales>800000</AnnualSales>%'"), "166");
    assert!(n(&mut c, "SELECT COUNT(*) FROM aw.Person.Person WHERE CAST(Demographics AS nvarchar(max)) LIKE '%<DateFirstPurchase>2003-09-01Z</DateFirstPurchase>%'").parse::<i64>().unwrap() > 0);
    // the native format of this server still works: back the restored database up and restore it under another name
    let tmp = std::env::temp_dir().join(format!("tsql-sqlbak-test-{}.bak", std::process::id()));
    let t = tmp.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    assert!(c.execute(&format!("BACKUP DATABASE aw TO DISK = '{}'", t)).errors().is_empty());
    assert!(c.execute(&format!("RESTORE DATABASE aw2 FROM DISK = '{}'", t)).errors().is_empty());
    // (the cascade test above deleted the 12 lines of order 43659)
    assert_eq!(n(&mut c, "SELECT COUNT(*) FROM aw2.Sales.SalesOrderDetail"), "121305");
    let _ = std::fs::remove_file(&tmp);
}
