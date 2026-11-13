//! `BACKUP DATABASE db TO DISK = 'x.bak' WITH SQL_SERVER_FORMAT`: a backup in the form of a SQL Server `.bak`.
//!
//! The catalog of the database is described in the types of [`crate::sqlbak`] (the ones the reader fills in), the rows are
//! read from a snapshot, and [`crate::sqlbak_write`] and [`crate::mtfwrite`] write the pages and the container. Reading the
//! file back with `RESTORE DATABASE` gives the same tables, columns, constraints, indexes, rows and modules. What is not
//! written is what the reader does not read either (see docs/backup.md): sequences, synonyms, user-defined types, users and
//! permissions, XML schema collections, row-level security, and the settings of filtered, spatial and XML indexes.
use super::*;
use crate::catalog::{Column, Database, Object, Table};
use crate::sqlbak::{ColMeta, Constraint, ForeignKey, FtCatalogMeta, FullTextMeta, IndexMeta, ModuleMeta, TableMeta, XmlIndexMeta};
use crate::sqlbak_write::{write_pages, Image, Pager, RowSource};
use crate::storage::{ReadView, ViewMode};
use crate::types::{DataType, Value};
use super::backup::Snapshot;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;

/// `(xtype, utype, length, precision, scale)` of a column's type as a backup records it.
fn type_numbers(ty: &DataType) -> (u8, u32, i16, u8, u8) {
    use crate::types::MAX;
    let n = |v: i32| if v == MAX { -1 } else { v as i16 };
    let time_len = |s: u8| if s <= 2 { 3 } else if s <= 4 { 4 } else { 5 };
    match ty {
        DataType::Bit => (104, 104, 1, 1, 0),
        DataType::TinyInt => (48, 48, 1, 3, 0),
        DataType::SmallInt => (52, 52, 2, 5, 0),
        DataType::Int => (56, 56, 4, 10, 0),
        DataType::BigInt => (127, 127, 8, 19, 0),
        DataType::Decimal(p, s) => (106, 106, if *p <= 9 { 5 } else if *p <= 19 { 9 } else if *p <= 28 { 13 } else { 17 }, *p, *s),
        DataType::Money => (60, 60, 8, 19, 4),
        DataType::SmallMoney => (122, 122, 4, 10, 4),
        DataType::Float => (62, 62, 8, 53, 0),
        DataType::Real => (59, 59, 4, 24, 0),
        DataType::Char(l) => (175, 175, *l as i16, 0, 0),
        DataType::VarChar(l) => (167, 167, n(*l), 0, 0),
        DataType::NChar(l) => (239, 239, (*l * 2) as i16, 0, 0),
        DataType::NVarChar(l) => (231, 231, if *l == MAX { -1 } else { (*l * 2) as i16 }, 0, 0),
        DataType::Text => (35, 35, 16, 0, 0),
        DataType::NText => (99, 99, 16, 0, 0),
        DataType::Binary(l) => (173, 173, *l as i16, 0, 0),
        DataType::VarBinary(l) => (165, 165, n(*l), 0, 0),
        DataType::Image => (34, 34, 16, 0, 0),
        DataType::Timestamp => (189, 189, 8, 0, 0),
        DataType::Date => (40, 40, 3, 10, 0),
        DataType::Time(s) => (41, 41, time_len(*s), 0, *s),
        DataType::DateTime => (61, 61, 8, 23, 3),
        DataType::SmallDateTime => (58, 58, 4, 16, 0),
        DataType::DateTime2(s) => (42, 42, time_len(*s) + 3, 0, *s),
        DataType::DateTimeOffset(s) => (43, 43, time_len(*s) + 5, 0, *s),
        DataType::Guid => (36, 36, 16, 0, 0),
        DataType::Xml => (241, 241, -1, 0, 0),
        DataType::Variant => (98, 98, 8016, 0, 0),
        DataType::HierarchyId => (240, 128, 892, 0, 0),
        DataType::Geometry => (240, 129, -1, 0, 0),
        DataType::Geography => (240, 130, -1, 0, 0),
        DataType::Other(_) => (165, 165, -1, 0, 0),
    }
}

fn col_meta(i: usize, c: &Column, last: Option<i64>) -> ColMeta {
    let (xtype, utype, length, prec, scale) = type_numbers(&c.ty);
    ColMeta {
        colid: i as u32 + 1,
        name: c.name.clone(),
        xtype,
        utype,
        length,
        prec,
        scale,
        status: 0,
        ty: c.ty.clone(),
        nullable: c.nullable,
        identity: c.identity.map(|(seed, inc)| (seed, inc, last.unwrap_or(seed - inc))),
        computed: c.computed.is_some(),
        definition: c.computed_text.clone(),
        persisted: false,
    }
}

/// An expression in one pair of outer parentheses, as a backup keeps the text of a check constraint or a default.
fn parenthesized(text: &str) -> String {
    let t = text.trim();
    if t.starts_with('(') && t.ends_with(')') {
        // the first parenthesis must close at the end
        let mut depth = 0i32;
        let mut quoted = false;
        for (i, ch) in t.char_indices() {
            match ch {
                '\'' => quoted = !quoted,
                '(' if !quoted => depth += 1,
                ')' if !quoted => {
                    depth -= 1;
                    if depth == 0 && i + 1 < t.len() {
                        return format!("({})", t);
                    }
                }
                _ => {}
            }
        }
        return t.to_string();
    }
    format!("({})", t)
}

fn colid(t: &Table, name: &str) -> u32 {
    t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)).map(|i| i as u32 + 1).unwrap_or(0)
}

/// What the writer needs to know about a table, from the catalog.
fn table_meta(db: &Database, t: &Table, schema_ids: &HashMap<String, u32>) -> TableMeta {
    let last = t.identity_next.get().and_then(|n| t.columns.iter().find_map(|c| c.identity.map(|(_, inc)| n - inc)));
    let cols: Vec<ColMeta> = t.columns.iter().enumerate().map(|(i, c)| col_meta(i, c, last)).collect();
    // indexes: the clustered key (a primary key or unique constraint) is index 1, the others follow
    let mut indexes: Vec<IndexMeta> = vec![];
    let key_index = |k: &crate::catalog::KeyDef, id: u32, primary: bool| IndexMeta {
        id,
        name: k.name.clone(),
        clustered: k.clustered,
        unique: true,
        primary,
        unique_constraint: !primary && !k.is_index,
        keys: k.cols.iter().map(|c| (*c as u32 + 1, false)).collect(),
        included: k.include.iter().map(|c| *c as u32 + 1).collect(),
    };
    let mut keys: Vec<(&crate::catalog::KeyDef, bool)> = vec![];
    if let Some(pk) = &t.pk {
        keys.push((pk, true));
    }
    keys.extend(t.uniques.iter().map(|u| (u, false)));
    // the clustered one first
    keys.sort_by_key(|(k, _)| !k.clustered);
    let mut id = 1u32;
    for (k, primary) in &keys {
        indexes.push(key_index(k, id, *primary));
        id += 1;
    }
    for ix in t.indexes.iter() {
        indexes.push(IndexMeta {
            id,
            name: ix.name.clone(),
            clustered: false,
            unique: ix.unique,
            primary: false,
            unique_constraint: false,
            keys: ix.cols.iter().map(|(c, d)| (*c as u32 + 1, *d)).collect(),
            included: ix.include.iter().map(|c| *c as u32 + 1).collect(),
        });
        id += 1;
    }
    let mut defaults = vec![];
    for (i, c) in t.columns.iter().enumerate() {
        if let Some(text) = &c.default_text {
            let name = c.default_name.clone().unwrap_or_else(|| format!("DF__{}__{}__{:08X}", t.name, c.name, (t.object_id as u32).wrapping_mul(31).wrapping_add(i as u32)));
            // a backup keeps the expression of a default in parentheses; the engine keeps it without them
            defaults.push(Constraint { name, column: i as u32 + 1, definition: format!("({})", text) });
        }
    }
    let checks = t.checks.iter().map(|c| Constraint { name: c.name.clone(), column: c.col.as_ref().map(|n| colid(t, n)).unwrap_or(0), definition: parenthesized(&c.text) }).collect();
    let mut foreign_keys = vec![];
    for f in t.fks.iter() {
        if !f.ref_db.is_empty() && !f.ref_db.eq_ignore_ascii_case(&db.name) {
            continue;
        }
        let Some(Object::Table(rt)) = db.objects.get(&f.ref_key) else { continue };
        let remote: Vec<u32> = f.ref_cols.iter().map(|n| colid(rt, n)).collect();
        if remote.len() != f.cols.len() || remote.contains(&0) {
            continue;
        }
        foreign_keys.push(ForeignKey { name: f.name.clone(), columns: f.cols.iter().zip(remote).map(|(l, r)| (*l as u32 + 1, r)).collect(), ref_table: rt.object_id as u32, cascade_delete: f.on_delete.eq_ignore_ascii_case("CASCADE"), unknown_flags: 0 });
    }
    let xml_indexes = t.xml_indexes.iter().map(|x| XmlIndexMeta { name: x.name.clone(), column: x.col as u32 + 1, primary: x.primary, secondary: x.secondary }).collect();
    let spatial_indexes = t.spatial_indexes.iter().map(|s| (s.name.clone(), s.col as u32 + 1)).collect();
    let fulltext = t.fulltext.as_ref().map(|f| FullTextMeta {
        key_index: indexes.iter().find(|i| i.name.eq_ignore_ascii_case(&f.key_index)).map(|i| i.id).unwrap_or(1),
        catalog: f.catalog.as_ref().and_then(|c| db.fulltext_catalogs.iter().position(|x| x.name.eq_ignore_ascii_case(c))).map(|p| p as u32 + 1).unwrap_or(0),
        cols: f.cols.iter().map(|(c, l)| (*c as u32 + 1, *l)).collect(),
    });
    let _ = schema_ids;
    TableMeta {
        id: t.object_id as u32,
        schema: t.schema.clone(),
        name: t.name.clone(),
        cols,
        data: None,
        rows: 0,
        locs: HashMap::new(),
        indexes,
        xml_indexes,
        spatial_indexes,
        other_indexes: vec![],
        foreign_keys,
        defaults,
        checks,
        fulltext,
    }
}

fn modules(db: &Database) -> Vec<ModuleMeta> {
    let mut out = vec![];
    for o in db.objects.values() {
        match o {
            Object::Table(t) => {
                for tr in &t.triggers {
                    out.push(ModuleMeta { id: tr.object_id as u32, schema: tr.schema.clone(), name: tr.name.clone(), kind: *b"TR", parent: t.object_id as u32, definition: Some(tr.text.clone()) });
                }
            }
            Object::View(v) => {
                out.push(ModuleMeta { id: v.object_id as u32, schema: v.schema.clone(), name: v.name.clone(), kind: *b"V ", parent: 0, definition: Some(v.text.clone()) });
                for tr in &v.triggers {
                    out.push(ModuleMeta { id: tr.object_id as u32, schema: tr.schema.clone(), name: tr.name.clone(), kind: *b"TR", parent: v.object_id as u32, definition: Some(tr.text.clone()) });
                }
            }
            Object::Proc(p) => out.push(ModuleMeta { id: p.object_id as u32, schema: p.schema.clone(), name: p.name.clone(), kind: *b"P ", parent: 0, definition: Some(p.text.clone()) }),
            Object::Func(f) => {
                let kind = match o.type_code() {
                    "IF" => *b"IF",
                    "TF" => *b"TF",
                    _ => *b"FN",
                };
                out.push(ModuleMeta { id: f.object_id as u32, schema: f.schema.clone(), name: f.name.clone(), kind, parent: 0, definition: Some(f.def.text.clone()) });
            }
            _ => {}
        }
    }
    for tr in &db.ddl_triggers {
        out.push(ModuleMeta { id: tr.object_id as u32, schema: "dbo".into(), name: tr.name.clone(), kind: *b"TR", parent: 0, definition: Some(tr.text.clone()) });
    }
    out.sort_by_key(|m| m.id);
    out
}

/// The rows of the tables of a snapshot.
struct SnapshotRows<'a> {
    view: ReadView,
    tables: HashMap<u32, &'a Table>,
}

impl RowSource for SnapshotRows<'_> {
    fn rows(&mut self, meta: &TableMeta, f: &mut dyn FnMut(&[Value]) -> std::result::Result<(), String>) -> std::result::Result<(), String> {
        let t = self.tables.get(&meta.id).ok_or("a table of the image is not in the catalog")?;
        let mut rows: Vec<Vec<Value>> = vec![];
        let mut after = None;
        loop {
            let chunk = t.data.scan_chunk(&self.view, after, 512, false);
            rows.extend(chunk.rows.into_iter().map(|(_, r)| r));
            if chunk.done || chunk.last.is_none() {
                break;
            }
            after = chunk.last;
        }
        // a clustered index keeps its rows in key order
        if let Some(ix) = meta.indexes.iter().find(|i| i.clustered) {
            let key: Vec<usize> = ix.keys.iter().map(|k| k.0 as usize - 1).collect();
            let desc: Vec<bool> = ix.keys.iter().map(|k| k.1).collect();
            rows.sort_by(|a, b| {
                let ka: Vec<Value> = key.iter().map(|i| a[*i].clone()).collect();
                let kb: Vec<Value> = key.iter().map(|i| b[*i].clone()).collect();
                super::query::cmp_keys(&ka, &kb, &desc)
            });
        }
        let stored: Vec<usize> = (0..t.columns.len()).filter(|i| t.columns[*i].computed.is_none()).collect();
        let mut buf: Vec<Value> = Vec::with_capacity(stored.len());
        for row in rows {
            buf.clear();
            buf.extend(stored.iter().map(|i| row.get(*i).cloned().unwrap_or(Value::Null)));
            f(&buf)?;
        }
        Ok(())
    }
}

impl Engine<'_> {
    /// Returns (rows written, tables written).
    pub(super) fn backup_sqlserver(&mut self, dbkey: &str, tmp: &str, name: String, description: String) -> Result<(u64, u64)> {
        let fail = |m: String| SqlError::with(3013, 16, 1, format!("BACKUP DATABASE is terminating abnormally. {}", m));
        let pending = self.srv.core.ddl_txs(self.cur_tx());
        if !pending.is_empty() {
            return Err(self.blocked(pending));
        }
        let core = self.srv.core.clone();
        let (snap_id, start) = core.begin(true);
        let _snap = Snapshot(core.clone(), snap_id);
        let view = ReadView { tx: snap_id, mode: ViewMode::Snapshot(start) };
        let db: Database = self.srv.databases[dbkey].clone();
        // schemas and their ids
        let mut schemas: Vec<(u32, String)> = vec![(1, "dbo".to_string())];
        let mut next_schema = 16_384u32;
        let mut ids: HashMap<String, u32> = HashMap::new();
        ids.insert("dbo".into(), 1);
        for (low, shown) in &db.schemas {
            if low == "dbo" {
                continue;
            }
            let id = db.schema_ids.get(low).map(|i| *i as u32).unwrap_or_else(|| {
                next_schema += 1;
                next_schema - 1
            });
            ids.insert(low.clone(), id);
            schemas.push((id, shown.clone()));
        }
        let mut tables: Vec<TableMeta> = vec![];
        let mut by_id: HashMap<u32, &Table> = HashMap::new();
        for o in db.objects.values() {
            if let Object::Table(t) = o {
                by_id.insert(t.object_id as u32, t);
            }
        }
        for o in db.objects.values() {
            if let Object::Table(t) = o {
                tables.push(table_meta(&db, t, &ids));
            }
        }
        tables.sort_by(|a, b| (a.schema.as_str(), a.name.as_str()).cmp(&(b.schema.as_str(), b.name.as_str())));
        let ft_catalogs = db.fulltext_catalogs.iter().enumerate().map(|(i, c)| FtCatalogMeta { id: i as u32 + 1, name: c.name.clone(), is_default: c.is_default, accent_sensitive: c.accent_sensitive }).collect();
        let img = Image { name: db.name.clone(), schemas, tables, modules: modules(&db), ft_catalogs };
        let pages_path = format!("{}.pages", tmp);
        let result = (|| -> std::result::Result<(u64, u64), String> {
            let mut pager = Pager::create(std::path::Path::new(&pages_path)).map_err(|e| e.to_string())?;
            let mut src = SnapshotRows { view, tables: by_id };
            let stats = write_pages(&img, &mut src, &mut pager)?;
            let (mut file, total) = pager.into_file();
            let when = chrono::Utc::now().naive_utc();
            let meta = crate::mtfwrite::Meta {
                database: db.name.clone(),
                name: if name.is_empty() { format!("{}-Full Database Backup", db.name) } else { name.clone() },
                description: description.clone(),
                user: "tsql-server".into(),
                machine: self.srv.server_name.clone(),
                when,
                config: crate::mtfwrite::config_stream(&db.name, &format!("{}.mdf", db.name), total),
            };
            let out = File::create(tmp).map_err(|e| e.to_string())?;
            let mut out = BufWriter::new(out);
            crate::mtfwrite::write_backup(&mut out, &meta, &mut file, total).map_err(|e| e.to_string())?;
            for (what, n) in &stats.lost {
                eprintln!("tsql-server: {} value(s) of {} are of a form that a SQL Server backup cannot hold here and were written as NULL", n, what);
            }
            Ok((stats.rows, stats.tables as u64))
        })();
        let _ = std::fs::remove_file(&pages_path);
        result.map_err(fail)
    }
}
