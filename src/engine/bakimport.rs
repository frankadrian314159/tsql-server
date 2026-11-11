//! `RESTORE DATABASE ... FROM DISK = 'x.bak'` for a native SQL Server backup.
//!
//! The backup is read with [`crate::sqlbak`]; its tables and rows are built in a scratch in-memory server with ordinary
//! `CREATE TABLE` and `INSERT` statements, and the scratch database is written as a backup of this server's own format
//! (`BACKUP DATABASE`). The restore then runs on that file as for any backup of this server, so everything that restore
//! does (permissions, `REPLACE`, `NORECOVERY`, durability, undo on failure) applies unchanged and nothing needs to be built
//! twice.
use crate::sqlbak::{Backup, ColMeta, TableMeta};
use crate::types::{DataType, DtKind, Value};
use crate::{mssqlbak, Server};
use chrono::Timelike;

/// What an import did, and what it could not bring over.
#[derive(Debug, Default)]
pub struct Report {
    pub database: String,
    pub tables: usize,
    pub rows: u64,
    /// (table, column, number of values left NULL because they are not decoded)
    pub skipped: Vec<(String, String, u64)>,
    /// computed columns, which are not created
    pub computed: Vec<(String, String)>,
}

fn quote(n: &str) -> String {
    format!("[{}]", n.replace(']', "]]"))
}

fn qname(t: &TableMeta) -> String {
    format!("{}.{}", quote(&t.schema), quote(&t.name))
}

fn size(n: i32) -> String {
    if n < 0 {
        "max".to_string()
    } else {
        n.to_string()
    }
}

fn type_sql(ty: &DataType) -> String {
    match ty {
        DataType::Bit => "bit".into(),
        DataType::TinyInt => "tinyint".into(),
        DataType::SmallInt => "smallint".into(),
        DataType::Int => "int".into(),
        DataType::BigInt => "bigint".into(),
        DataType::Decimal(p, s) => format!("decimal({},{})", p, s),
        DataType::Money => "money".into(),
        DataType::SmallMoney => "smallmoney".into(),
        DataType::Float => "float".into(),
        DataType::Real => "real".into(),
        DataType::Char(n) => format!("char({})", n),
        DataType::VarChar(n) => format!("varchar({})", size(*n)),
        DataType::NChar(n) => format!("nchar({})", n),
        DataType::NVarChar(n) => format!("nvarchar({})", size(*n)),
        DataType::Text => "text".into(),
        DataType::NText => "ntext".into(),
        DataType::Binary(n) => format!("binary({})", n),
        DataType::VarBinary(n) => format!("varbinary({})", size(*n)),
        DataType::Image => "image".into(),
        // a row version cannot be inserted into, so it is kept as the 8 bytes it is
        DataType::Timestamp => "binary(8)".into(),
        DataType::Date => "date".into(),
        DataType::Time(s) => format!("time({})", s),
        DataType::DateTime => "datetime".into(),
        DataType::SmallDateTime => "smalldatetime".into(),
        DataType::DateTime2(s) => format!("datetime2({})", s),
        DataType::DateTimeOffset(s) => format!("datetimeoffset({})", s),
        DataType::Guid => "uniqueidentifier".into(),
        DataType::Xml => "xml".into(),
        DataType::Variant => "sql_variant".into(),
        DataType::Other(n) => n.clone(),
    }
}

fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bit(b) => (*b as i32).to_string(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::Dec(d) => {
            let neg = d.m < 0;
            let digits = d.m.unsigned_abs().to_string();
            let s = d.s as usize;
            let body = if s == 0 {
                digits
            } else {
                let padded = format!("{:0>width$}", digits, width = s + 1);
                format!("{}.{}", &padded[..padded.len() - s], &padded[padded.len() - s..])
            };
            if neg {
                format!("-{}", body)
            } else {
                body
            }
        }
        Value::Float(f) => format!("{:e}", f),
        Value::Str(s) => format!("N'{}'", s.replace('\'', "''")),
        Value::Bin(b) => format!("0x{}", b.iter().map(|x| format!("{:02X}", x)).collect::<String>()),
        Value::Date(d) => format!("'{}'", d.format("%Y-%m-%d")),
        Value::Time(t) => format!("'{}.{:07}'", t.format("%H:%M:%S"), t.nanosecond() / 100),
        Value::DateTime(dt, DtKind::DateTime) => format!("'{}'", dt.format("%Y-%m-%dT%H:%M:%S%.3f")),
        Value::DateTime(dt, DtKind::SmallDateTime) => format!("'{}'", dt.format("%Y-%m-%dT%H:%M:%S")),
        Value::DateTime(dt, DtKind::DateTime2) => format!("'{}.{:07}'", dt.format("%Y-%m-%d %H:%M:%S"), dt.nanosecond() / 100),
        Value::Dto(d) => format!("'{}.{:07} {}'", d.format("%Y-%m-%d %H:%M:%S"), d.nanosecond() / 100, d.format("%:z")),
        Value::Guid(g) => format!("'{}'", g.to_string().to_uppercase()),
    }
}

fn run(conn: &mut crate::Connection, sql: &str) -> Result<(), String> {
    let r = conn.execute(sql);
    match r.first_error() {
        Some(e) => Err(format!("{} (while running: {})", e, sql.chars().take(200).collect::<String>())),
        None => Ok(()),
    }
}

/// Read the SQL Server backup at `src` and write its database as a backup of this server's format at `out`.
pub fn convert(src: &str, out: &str) -> Result<Report, String> {
    let f = std::fs::File::open(src).map_err(|e| format!("cannot open {}: {}", src, e))?;
    let ex = mssqlbak::Expanded::open(std::io::BufReader::new(f)).map_err(|e| e.to_string())??;
    let mut bak = Backup::open(ex)?;
    let db = bak.db_name.clone();
    let tables: Vec<TableMeta> = bak.tables()?.into_iter().filter(|t| t.schema != "sys" && !t.cols.is_empty()).collect();
    let mut report = Report { database: db.clone(), ..Report::default() };

    let scratch = Server::new();
    let mut conn = scratch.connect();
    run(&mut conn, &format!("CREATE DATABASE {}", quote(&db)))?;
    run(&mut conn, &format!("USE {}", quote(&db)))?;
    let mut schemas: Vec<&str> = tables.iter().map(|t| t.schema.as_str()).filter(|s| !s.eq_ignore_ascii_case("dbo")).collect();
    schemas.sort_unstable();
    schemas.dedup();
    for s in schemas {
        run(&mut conn, &format!("CREATE SCHEMA {}", quote(s)))?;
    }
    for t in &tables {
        let data = bak.rows(t)?;
        let stored: Vec<&ColMeta> = t.cols.iter().filter(|c| !c.computed).collect();
        for c in t.cols.iter().filter(|c| c.computed) {
            report.computed.push((format!("{}.{}", t.schema, t.name), c.name.clone()));
        }
        for (c, n) in &data.skipped {
            report.skipped.push((format!("{}.{}", t.schema, t.name), c.clone(), *n));
        }
        // a column with values that could not be read must be able to hold NULL
        let defs: Vec<String> = stored
            .iter()
            .map(|c| {
                let nullable = c.nullable || data.skipped.contains_key(&c.name);
                let identity = c.identity.map(|(seed, inc, _)| format!(" IDENTITY({},{})", seed, inc)).unwrap_or_default();
                format!("{} {}{}{}", quote(&c.name), type_sql(&c.ty), identity, if nullable { " NULL" } else { " NOT NULL" })
            })
            .collect();
        run(&mut conn, &format!("CREATE TABLE {} ({})", qname(t), defs.join(", ")))?;
        if !data.rows.is_empty() {
            let has_identity = stored.iter().any(|c| c.identity.is_some());
            let cols = stored.iter().map(|c| quote(&c.name)).collect::<Vec<_>>().join(", ");
            if has_identity {
                run(&mut conn, &format!("SET IDENTITY_INSERT {} ON", qname(t)))?;
            }
            for chunk in data.rows.chunks(500) {
                let values: Vec<String> = chunk.iter().map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", "))).collect();
                run(&mut conn, &format!("INSERT INTO {} ({}) VALUES {}", qname(t), cols, values.join(", ")))?;
            }
            if has_identity {
                run(&mut conn, &format!("SET IDENTITY_INSERT {} OFF", qname(t)))?;
            }
        }
        report.tables += 1;
        report.rows += data.rows.len() as u64;
    }
    let path = out.replace('\'', "''");
    run(&mut conn, &format!("BACKUP DATABASE {} TO DISK = N'{}' WITH INIT", quote(&db), path))?;
    Ok(report)
}
