//! `RESTORE DATABASE ... FROM DISK = 'x.bak'` for a native SQL Server backup.
//!
//! The backup is read with [`crate::sqlbak`]; its tables and rows are built in a scratch in-memory server with ordinary
//! `CREATE TABLE` and `INSERT` statements, and the scratch database is written as a backup of this server's own format
//! (`BACKUP DATABASE`). The restore then runs on that file as for any backup of this server, so everything that restore
//! does (permissions, `REPLACE`, `NORECOVERY`, durability, undo on failure) applies unchanged and nothing needs to be built
//! twice.
use crate::sqlbak::{Backup, ColMeta, ModuleMeta, TableMeta};
use std::collections::HashMap;
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
    /// PRIMARY KEY and UNIQUE constraints, other indexes, FOREIGN KEY, DEFAULT and CHECK constraints that were created
    pub keys: usize,
    pub indexes: usize,
    pub xml_indexes: usize,
    pub spatial_indexes: usize,
    pub fulltext_catalogs: usize,
    pub fulltext_indexes: usize,
    pub foreign_keys: usize,
    pub defaults: usize,
    pub checks: usize,
    pub views: usize,
    pub procedures: usize,
    pub functions: usize,
    pub triggers: usize,
    /// what could not be created, and why
    pub problems: Vec<String>,
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
        DataType::HierarchyId => "hierarchyid".into(),
        DataType::Geometry => "geometry".into(),
        DataType::Geography => "geography".into(),
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
    // (table id, column id) of the computed columns that were created
    let mut computed_created: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();

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
    // functions first: computed columns, defaults and checks may call them. Those that cannot be created yet (they read
    // tables that are not there) are tried again when the rows are in.
    let modules = bak.modules()?;
    let is_function = |m: &&ModuleMeta| matches!(&m.kind, b"FN" | b"IF" | b"TF");
    let mut early_failed: Vec<&ModuleMeta> = vec![];
    for m in modules.iter().filter(is_function) {
        if let Some(def) = &m.definition {
            if conn.execute(def).first_error().is_some() {
                early_failed.push(m);
            } else {
                report.functions += 1;
            }
        }
    }
    for t in &tables {
        let data = bak.rows(t)?;
        let tn = format!("{}.{}", t.schema, t.name);
        for (c, n) in &data.skipped {
            report.skipped.push((tn.clone(), c.clone(), *n));
        }
        // computed columns come in their place among the others. When the table cannot be built with them (an expression that
        // this server cannot evaluate, found out when the rows go in) it is built again without them.
        let wants_computed = t.cols.iter().any(|c| c.computed && c.definition.is_some());
        let mut with_computed = wants_computed;
        let mut loaded = load_table(&mut conn, t, &data, with_computed);
        if let (Err(e), true) = (&loaded, wants_computed) {
            report.problems.push(format!("the computed columns of {} were not created: {}", tn, e));
            let _ = conn.execute(&format!("DROP TABLE {}", qname(t)));
            with_computed = false;
            loaded = load_table(&mut conn, t, &data, false);
        }
        loaded?;
        for c in t.cols.iter().filter(|c| c.computed) {
            if with_computed && c.definition.is_some() {
                computed_created.insert((t.id, c.colid));
            } else {
                report.computed.push((tn.clone(), c.name.clone()));
            }
        }
        report.tables += 1;
        report.rows += data.rows.len() as u64;
    }
    let catalogs = create_fulltext_catalogs(&mut conn, &mut bak, &mut report);
    create_keys(&mut conn, &tables, &computed_created, &catalogs, &mut report);
    create_modules(&mut conn, &early_failed, &mut report);
    let of_kind = |k: &[&[u8; 2]]| -> Vec<&ModuleMeta> { modules.iter().filter(|m| k.contains(&&m.kind)).collect() };
    // functions that no definition was found for (encrypted) or that are not T-SQL
    for m in modules.iter().filter(is_function).filter(|m| m.definition.is_none()) {
        report.problems.push(format!("function {}.{} was not created: its text is not in the backup (encrypted?)", m.schema, m.name));
    }
    create_modules(&mut conn, &of_kind(&[b"V "]), &mut report);
    create_modules(&mut conn, &of_kind(&[b"P "]), &mut report);
    create_modules(&mut conn, &of_kind(&[b"TR"]), &mut report);
    for m in modules.iter().filter(|m| matches!(&m.kind, b"AF" | b"FS" | b"FT" | b"PC" | b"TA")) {
        report.problems.push(format!("CLR object {}.{} was not created: CLR is not supported", m.schema, m.name));
    }
    let path = out.replace('\'', "''");
    run(&mut conn, &format!("BACKUP DATABASE {} TO DISK = N'{}' WITH INIT", quote(&db), path))?;
    Ok(report)
}

/// A column list for an index or key, or `None` when a column is not one that was created.
fn column_list(t: &TableMeta, cols: &[u32], computed: &std::collections::HashSet<(u32, u32)>) -> Option<Vec<String>> {
    cols.iter().map(|id| t.cols.iter().find(|c| c.colid == *id && (!c.computed || computed.contains(&(t.id, c.colid)))).map(|c| quote(&c.name))).collect()
}

/// Run one statement that adds a key, index or constraint; what fails is reported, not fatal.
fn attempt(conn: &mut crate::Connection, sql: &str, what: String, problems: &mut Vec<String>) -> bool {
    match conn.execute(sql).first_error() {
        None => true,
        Some(e) => {
            problems.push(format!("{} was not created: {}", what, e));
            false
        }
    }
}

/// The full-text catalogs of the backup, created; the result maps the id of each catalog that was created to its name.
fn create_fulltext_catalogs<R: std::io::Read + std::io::Seek>(conn: &mut crate::Connection, bak: &mut Backup<R>, report: &mut Report) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    match bak.ft_catalogs() {
        Ok(cats) => {
            for c in cats {
                let sql = format!("CREATE FULLTEXT CATALOG {}{}{}", quote(&c.name), if c.accent_sensitive { "" } else { " WITH ACCENT_SENSITIVITY = OFF" }, if c.is_default { " AS DEFAULT" } else { "" });
                if attempt(conn, &sql, format!("full-text catalog {}", c.name), &mut report.problems) {
                    report.fulltext_catalogs += 1;
                    out.insert(c.id, c.name);
                }
            }
        }
        Err(e) => report.problems.push(format!("the full-text catalogs could not be read: {}", e)),
    }
    out
}

/// Indexes, keys and constraints, once all rows are in: primary and unique keys and indexes, then defaults and checks, then
/// the foreign keys (which need the keys they refer to).
fn create_keys(conn: &mut crate::Connection, tables: &[TableMeta], computed: &std::collections::HashSet<(u32, u32)>, catalogs: &HashMap<u32, String>, report: &mut Report) {
    let by_id: HashMap<u32, &TableMeta> = tables.iter().map(|t| (t.id, t)).collect();
    for t in tables {
        let tn = format!("{}.{}", t.schema, t.name);
        for ix in &t.indexes {
            let key_ids: Vec<u32> = ix.keys.iter().map(|k| k.0).collect();
            let (Some(keys), Some(inc)) = (column_list(t, &key_ids, computed), column_list(t, &ix.included, computed)) else {
                report.problems.push(format!("index {} on {} was not created: it refers to a column that was not created", ix.name, tn));
                continue;
            };
            if keys.is_empty() {
                continue;
            }
            let keys: Vec<String> = keys.iter().zip(&ix.keys).map(|(k, (_, desc))| if *desc { format!("{} DESC", k) } else { k.clone() }).collect();
            let cl = if ix.clustered { "CLUSTERED" } else { "NONCLUSTERED" };
            if ix.primary || ix.unique_constraint {
                let kind = if ix.primary { "PRIMARY KEY" } else { "UNIQUE" };
                let sql = format!("ALTER TABLE {} ADD CONSTRAINT {} {} {} ({})", qname(t), quote(&ix.name), kind, cl, keys.join(", "));
                if attempt(conn, &sql, format!("{} {} on {}", kind, ix.name, tn), &mut report.problems) {
                    report.keys += 1;
                }
            } else {
                let include = if inc.is_empty() { String::new() } else { format!(" INCLUDE ({})", inc.join(", ")) };
                let sql = format!("CREATE {}{} INDEX {} ON {} ({}){}", if ix.unique { "UNIQUE " } else { "" }, cl, quote(&ix.name), qname(t), keys.join(", "), include);
                if attempt(conn, &sql, format!("index {} on {}", ix.name, tn), &mut report.problems) {
                    report.indexes += 1;
                }
            }
        }
        // XML indexes need the primary key, so they come after the keys
        for x in &t.xml_indexes {
            let Some(col) = column_list(t, &[x.column], computed) else {
                report.problems.push(format!("XML index {} on {} was not created: its column was not created", x.name, tn));
                continue;
            };
            let sql = if x.primary {
                format!("CREATE PRIMARY XML INDEX {} ON {} ({})", quote(&x.name), qname(t), col[0])
            } else {
                // a secondary index is built on the primary index of its column
                let Some(p) = t.xml_indexes.iter().find(|p| p.primary && p.column == x.column) else {
                    report.problems.push(format!("XML index {} on {} was not created: there is no primary XML index on its column", x.name, tn));
                    continue;
                };
                let kind = match x.secondary {
                    Some('P') => "PATH",
                    Some('R') => "PROPERTY",
                    _ => "VALUE",
                };
                format!("CREATE XML INDEX {} ON {} ({}) USING XML INDEX {} FOR {}", quote(&x.name), qname(t), col[0], quote(&p.name), kind)
            };
            if attempt(conn, &sql, format!("XML index {} on {}", x.name, tn), &mut report.problems) {
                report.xml_indexes += 1;
            }
        }
        // spatial indexes need the primary key. The backup's tessellation settings are not read (the sample has no spatial index to
        // learn them from): a geography index gets the defaults, a geometry index the bounding box of the whole globe.
        for (name, column) in &t.spatial_indexes {
            let Some(col) = column_list(t, &[*column], computed) else {
                report.problems.push(format!("spatial index {} on {} was not created: its column was not created", name, tn));
                continue;
            };
            let geography = t.cols.iter().any(|c| c.colid == *column && matches!(c.ty, DataType::Geography));
            let with = if geography { "" } else { " WITH (BOUNDING_BOX = (-180, -90, 180, 90))" };
            let sql = format!("CREATE SPATIAL INDEX {} ON {} ({}){}", quote(name), qname(t), col[0], with);
            if attempt(conn, &sql, format!("spatial index {} on {}", name, tn), &mut report.problems) {
                report.spatial_indexes += 1;
                report.problems.push(format!("spatial index {} on {} was created with default tessellation settings (the backup's are not read)", name, tn));
            }
        }
        // the full-text index comes after the key it is built on
        if let Some(f) = &t.fulltext {
            let key = t.indexes.iter().find(|i| i.id == f.key_index).map(|i| i.name.clone());
            let cols: Option<Vec<String>> = f.cols.iter().map(|(c, lang)| column_list(t, &[*c], computed).map(|n| format!("{} LANGUAGE {}", n[0], lang))).collect();
            match (key, cols) {
                (Some(key), Some(cols)) if !cols.is_empty() => {
                    let catalog = catalogs.get(&f.catalog).map(|c| format!(" ON {}", quote(c))).unwrap_or_default();
                    let sql = format!("CREATE FULLTEXT INDEX ON {} ({}) KEY INDEX {}{}", qname(t), cols.join(", "), quote(&key), catalog);
                    if attempt(conn, &sql, format!("full-text index on {}", tn), &mut report.problems) {
                        report.fulltext_indexes += 1;
                    }
                }
                _ => report.problems.push(format!("full-text index on {} was not created: its key index or columns were not found", tn)),
            }
        }
        for (name, ty) in &t.other_indexes {
            let kind = match ty {
                5 | 6 => "columnstore index",
                7 => "hash index",
                _ => "index of a kind that is not known",
            };
            report.problems.push(format!("{} {} on {} was not created: this kind of index is not supported", kind, name, tn));
        }
        for d in &t.defaults {
            let Some(col) = column_list(t, &[d.column], computed) else {
                report.problems.push(format!("default {} on {} was not created: its column was not created", d.name, tn));
                continue;
            };
            let sql = format!("ALTER TABLE {} ADD CONSTRAINT {} DEFAULT {} FOR {}", qname(t), quote(&d.name), d.definition, col[0]);
            if attempt(conn, &sql, format!("default {} on {}", d.name, tn), &mut report.problems) {
                report.defaults += 1;
            }
        }
        for c in &t.checks {
            let sql = format!("ALTER TABLE {} ADD CONSTRAINT {} CHECK {}", qname(t), quote(&c.name), c.definition);
            if attempt(conn, &sql, format!("check {} on {}", c.name, tn), &mut report.problems) {
                report.checks += 1;
            }
        }
    }
    for t in tables {
        let tn = format!("{}.{}", t.schema, t.name);
        for fk in &t.foreign_keys {
            let Some(parent) = by_id.get(&fk.ref_table) else {
                report.problems.push(format!("foreign key {} on {} was not created: the table it refers to is not in the backup", fk.name, tn));
                continue;
            };
            let child_ids: Vec<u32> = fk.columns.iter().map(|c| c.0).collect();
            let parent_ids: Vec<u32> = fk.columns.iter().map(|c| c.1).collect();
            let (Some(cc), Some(pc)) = (column_list(t, &child_ids, computed), column_list(parent, &parent_ids, computed)) else {
                report.problems.push(format!("foreign key {} on {} was not created: it refers to a column that was not created", fk.name, tn));
                continue;
            };
            if fk.unknown_flags != 0 {
                report.problems.push(format!("foreign key {} on {} has options (status {:#x}) that are not understood and were left out", fk.name, tn, fk.unknown_flags));
            }
            let sql = format!(
                "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){}",
                qname(t),
                quote(&fk.name),
                cc.join(", "),
                qname(parent),
                pc.join(", "),
                if fk.cascade_delete { " ON DELETE CASCADE" } else { "" }
            );
            if attempt(conn, &sql, format!("foreign key {} on {}", fk.name, tn), &mut report.problems) {
                report.foreign_keys += 1;
            }
        }
    }
}

/// Create a table and put its rows in.
fn load_table(conn: &mut crate::Connection, t: &TableMeta, data: &crate::sqlbak::Rows, with_computed: bool) -> Result<(), String> {
    // a column with values that could not be read must be able to hold NULL
    let defs: Vec<String> = t
        .cols
        .iter()
        .filter_map(|c| {
            if c.computed {
                let expr = c.definition.as_ref().filter(|_| with_computed)?;
                return Some(format!("{} AS {}{}", quote(&c.name), expr, if c.persisted { " PERSISTED" } else { "" }));
            }
            let nullable = c.nullable || data.skipped.contains_key(&c.name);
            let identity = c.identity.map(|(seed, inc, _)| format!(" IDENTITY({},{})", seed, inc)).unwrap_or_default();
            Some(format!("{} {}{}{}", quote(&c.name), type_sql(&c.ty), identity, if nullable { " NULL" } else { " NOT NULL" }))
        })
        .collect();
    run(conn, &format!("CREATE TABLE {} ({})", qname(t), defs.join(", ")))?;
    if data.rows.is_empty() {
        return Ok(());
    }
    let stored: Vec<&ColMeta> = t.cols.iter().filter(|c| !c.computed).collect();
    let has_identity = stored.iter().any(|c| c.identity.is_some());
    let cols = stored.iter().map(|c| quote(&c.name)).collect::<Vec<_>>().join(", ");
    if has_identity {
        run(conn, &format!("SET IDENTITY_INSERT {} ON", qname(t)))?;
    }
    for chunk in data.rows.chunks(500) {
        let values: Vec<String> = chunk.iter().map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", "))).collect();
        run(conn, &format!("INSERT INTO {} ({}) VALUES {}", qname(t), cols, values.join(", ")))?;
    }
    if has_identity {
        run(conn, &format!("SET IDENTITY_INSERT {} OFF", qname(t)))?;
    }
    Ok(())
}

/// Create views, procedures, functions or triggers from their text. An object may need another that comes later (a view of a
/// view), so what fails is tried again as long as a pass creates anything; what still fails is reported with its error.
fn create_modules(conn: &mut crate::Connection, mods: &[&ModuleMeta], report: &mut Report) {
    let mut pending: Vec<(&ModuleMeta, String)> = vec![];
    for m in mods {
        match &m.definition {
            Some(_) => pending.push((m, String::new())),
            None => report.problems.push(format!("{}.{} was not created: its text is not in the backup (encrypted?)", m.schema, m.name)),
        }
    }
    loop {
        let mut failed: Vec<(&ModuleMeta, String)> = vec![];
        let before = pending.len();
        for (m, _) in pending {
            match conn.execute(m.definition.as_deref().unwrap_or("")).first_error() {
                None => match &m.kind {
                    b"V " => report.views += 1,
                    b"P " => report.procedures += 1,
                    b"TR" => report.triggers += 1,
                    _ => report.functions += 1,
                },
                Some(e) => failed.push((m, e.to_string())),
            }
        }
        let progress = failed.len() < before;
        pending = failed;
        if pending.is_empty() || !progress {
            break;
        }
    }
    for (m, e) in pending {
        let what = match &m.kind {
            b"V " => "view",
            b"P " => "procedure",
            b"TR" => "trigger",
            _ => "function",
        };
        report.problems.push(format!("{} {}.{} was not created: {}", what, m.schema, m.name, e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlbak::Rows;
    use crate::types::DataType;

    fn col(id: u32, name: &str, xtype: u8, ty: DataType) -> ColMeta {
        ColMeta { colid: id, name: name.into(), xtype, utype: 0, length: 0, prec: 0, scale: 0, status: 0, ty, nullable: true, identity: None, computed: false, definition: None, persisted: false }
    }

    /// Values of the types that a backup stores in other forms (large objects, variants) go through the import as they do for
    /// any other type: the literals are accepted and read back unchanged.
    #[test]
    fn text_ntext_image_and_sql_variant_values_are_loaded() {
        let table = TableMeta {
            id: 1,
            schema: "dbo".into(),
            name: "t".into(),
            cols: vec![
                col(1, "id", 56, DataType::Int),
                col(2, "t", 35, DataType::Text),
                col(3, "n", 99, DataType::NText),
                col(4, "i", 34, DataType::Image),
                col(5, "v", 98, DataType::Variant),
            ],
            data: None,
            rows: 0,
            locs: Default::default(),
            indexes: vec![],
            xml_indexes: vec![],
            spatial_indexes: vec![],
            other_indexes: vec![],
            foreign_keys: vec![],
            defaults: vec![],
            checks: vec![],
            fulltext: None,
        };
        let long = "x".repeat(20_000);
        let rows = vec![
            vec![Value::Int(1), Value::Str("it's text".into()), Value::Str("ünïcode \u{4e2d}".into()), Value::Bin(vec![0, 1, 255]), Value::Int(42)],
            vec![Value::Int(2), Value::Str(long.clone()), Value::Null, Value::Null, Value::Str("a variant".into())],
            vec![Value::Int(3), Value::Null, Value::Str(String::new()), Value::Bin(vec![]), Value::Bit(true)],
            vec![Value::Int(4), Value::Null, Value::Null, Value::Null, Value::BigInt(1 << 40)],
            vec![Value::Int(5), Value::Null, Value::Null, Value::Null, Value::Bin(vec![9, 8])],
        ];
        let data = Rows { rows, skipped: Default::default() };
        let server = Server::new();
        let mut c = server.connect();
        load_table(&mut c, &table, &data, false).unwrap();
        let q = |c: &mut crate::Connection, sql: &str| -> Vec<String> { c.execute(sql).resultsets()[0].rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|")).collect() };
        assert_eq!(q(&mut c, "SELECT t, n FROM t WHERE id = 1"), vec!["it's text|ünïcode \u{4e2d}"]);
        assert_eq!(q(&mut c, "SELECT DATALENGTH(t) FROM t WHERE id = 2"), vec!["20000"]);
        assert_eq!(q(&mut c, "SELECT CONVERT(varchar(10), i, 1) FROM t WHERE id = 1"), vec!["0x0001FF"]);
        assert_eq!(q(&mut c, "SELECT CAST(v AS int) FROM t WHERE id = 1"), vec!["42"]);
        assert_eq!(q(&mut c, "SELECT CAST(v AS varchar(20)) FROM t WHERE id = 2"), vec!["a variant"]);
        assert_eq!(q(&mut c, "SELECT CAST(v AS bigint) FROM t WHERE id = 4"), vec![(1i64 << 40).to_string()]);
        assert_eq!(q(&mut c, "SELECT COUNT(*) FROM t WHERE t IS NULL"), vec!["3"]);
    }
}
