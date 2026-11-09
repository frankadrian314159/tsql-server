//! Catalog introspection for tools built on the engine (the shell, the GUI).
//!
//! These read the live catalog directly, so they work without permission checks and without
//! parsing `sys.*` output. They describe the connection's current database.
use crate::catalog::{Database, Object, Table};
use crate::Connection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
    Procedure,
    Function,
    Sequence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectEntry {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
}

impl ObjectEntry {
    /// `schema.name`, as typed in SQL.
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub name: String,
    /// The type as written in a column definition, e.g. `nvarchar(50)` or `decimal(10,2)`.
    pub ty: String,
    pub nullable: bool,
    /// `(seed, increment)` of an `IDENTITY` column.
    pub identity: Option<(i64, i64)>,
    pub has_default: bool,
    /// Source text of the `DEFAULT` expression.
    pub default_text: Option<String>,
    pub default_name: Option<String>,
    pub computed: bool,
    /// Source text of a computed column's formula.
    pub computed_text: Option<String>,
    pub primary_key: bool,
}

#[derive(Clone, Debug)]
pub struct CheckInfo {
    pub name: String,
    pub text: String,
    pub enabled: bool,
    /// The column of a column-level constraint.
    pub column: Option<String>,
}

#[derive(Clone, Debug)]
pub struct IndexInfo {
    pub name: String,
    /// Column names with `true` for descending.
    pub columns: Vec<(String, bool)>,
    pub unique: bool,
    pub primary_key: bool,
    /// Declared as a constraint (drop it with `ALTER TABLE ... DROP CONSTRAINT`) rather than
    /// created with `CREATE INDEX` (drop it with `DROP INDEX`).
    pub constraint: bool,
    /// `INCLUDE (...)` columns.
    pub include: Vec<String>,
    /// The `WHERE` clause of a filtered index.
    pub filter: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Clone, Debug)]
pub struct TableInfo {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    /// Primary key and unique constraints first, then the other indexes.
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// Names of the check constraints.
    pub checks: Vec<String>,
    /// The check constraints with their expressions.
    pub check_defs: Vec<CheckInfo>,
    pub triggers: Vec<String>,
    /// Slots in the heap: live rows plus deleted rows not yet collected. Cheap, approximate.
    pub approx_rows: usize,
}

impl TableInfo {
    /// `[schema].[name]`, safe to splice into SQL.
    pub fn quoted(&self) -> String {
        format!("{}.{}", crate::tools::bracket(&self.schema), crate::tools::bracket(&self.name))
    }
    pub fn primary_key(&self) -> Vec<&ColumnInfo> {
        self.columns.iter().filter(|c| c.primary_key).collect()
    }
}

fn split_name(name: &str) -> Vec<String> {
    // `[a].[b]`, `a.b` or `db.a.b`; brackets and double quotes are stripped
    let mut parts = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    for ch in name.trim().chars() {
        match ch {
            '[' | '"' if !quoted => quoted = true,
            ']' | '"' if quoted => quoted = false,
            '.' if !quoted => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

fn find_object<'a>(db: &'a Database, name: &str) -> Option<&'a Object> {
    let parts = split_name(name);
    let (schema, obj) = match parts.len() {
        1 => (None, parts[0].as_str()),
        2 => (Some(parts[0].as_str()), parts[1].as_str()),
        _ => (Some(parts[parts.len() - 2].as_str()), parts[parts.len() - 1].as_str()),
    };
    match schema {
        Some(s) => db.objects.get(&format!("{}.{}", s, obj).to_lowercase()),
        None => db.objects.get(&format!("dbo.{}", obj).to_lowercase()).or_else(|| db.objects.values().find(|o| o.name().eq_ignore_ascii_case(obj))),
    }
}

fn describe(db: &str, t: &Table) -> TableInfo {
    let pk_cols: Vec<usize> = t.pk.as_ref().map(|p| p.cols.clone()).unwrap_or_default();
    let columns = t
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| ColumnInfo {
            name: c.name.clone(),
            ty: c.ty.to_string(),
            nullable: c.nullable && !pk_cols.contains(&i),
            identity: c.identity,
            has_default: c.default.is_some(),
            default_text: c.default_text.clone(),
            default_name: c.default_name.clone(),
            computed: c.computed.is_some(),
            computed_text: c.computed_text.clone(),
            primary_key: pk_cols.contains(&i),
        })
        .collect();
    let col_name = |i: usize| t.columns.get(i).map(|c| c.name.clone()).unwrap_or_default();
    let mut indexes = vec![];
    if let Some(pk) = &t.pk {
        indexes.push(IndexInfo { name: pk.name.clone(), columns: pk.cols.iter().map(|&c| (col_name(c), false)).collect(), unique: true, primary_key: true, constraint: true, include: vec![], filter: None });
    }
    for u in &t.uniques {
        // a unique index also has an index definition, which knows the sort direction of its columns
        let desc = |pos: usize| t.indexes.iter().find(|i| i.name == u.name).and_then(|i| i.cols.get(pos)).map(|c| c.1).unwrap_or(false);
        indexes.push(IndexInfo { name: u.name.clone(), columns: u.cols.iter().enumerate().map(|(p, &c)| (col_name(c), desc(p))).collect(), unique: true, primary_key: false, constraint: !u.is_index, include: u.include.iter().map(|&c| col_name(c)).collect(), filter: u.filter.as_ref().map(|f| f.text.clone()) });
    }
    for i in &t.indexes {
        if !i.unique {
            indexes.push(IndexInfo { name: i.name.clone(), columns: i.cols.iter().map(|&(c, d)| (col_name(c), d)).collect(), unique: false, primary_key: false, constraint: false, include: i.include.iter().map(|&c| col_name(c)).collect(), filter: i.filter.as_ref().map(|f| f.text.clone()) });
        }
    }
    let foreign_keys = t
        .fks
        .iter()
        .map(|f| ForeignKeyInfo {
            name: f.name.clone(),
            columns: f.cols.iter().map(|&c| col_name(c)).collect(),
            ref_table: f.ref_key.clone(),
            ref_columns: f.ref_cols.clone(),
            on_delete: f.on_delete.clone(),
            on_update: f.on_update.clone(),
        })
        .collect();
    TableInfo {
        database: db.to_string(),
        schema: t.schema.clone(),
        name: t.name.clone(),
        columns,
        indexes,
        foreign_keys,
        checks: t.checks.iter().map(|c| c.name.clone()).collect(),
        check_defs: t.checks.iter().map(|c| CheckInfo { name: c.name.clone(), text: c.text.clone(), enabled: c.enabled, column: c.col.clone() }).collect(),
        triggers: t.triggers.iter().map(|t| t.name.clone()).collect(),
        approx_rows: t.data.slot_count(),
    }
}

impl Connection {
    fn with_db<T>(&self, f: impl FnOnce(&Database) -> T) -> Option<T> {
        let g = self.server.shared.state.read().unwrap_or_else(|e| e.into_inner());
        g.databases.get(&self.session.db.to_lowercase()).map(f)
    }

    /// Structure of a table in the current database (`name` may be `schema.name`, bracketed or not).
    pub fn table_info(&self, name: &str) -> Option<TableInfo> {
        let db = self.session.db.clone();
        self.with_db(|d| match find_object(d, name) {
            Some(Object::Table(t)) => Some(describe(&db, t)),
            _ => None,
        })
        .flatten()
    }

    /// Tables, views, procedures, functions and sequences of the current database.
    pub fn objects(&self) -> Vec<ObjectEntry> {
        self.with_db(|d| {
            let mut v: Vec<ObjectEntry> = d
                .objects
                .values()
                .map(|o| ObjectEntry {
                    schema: o.schema().to_string(),
                    name: o.name().to_string(),
                    kind: match o {
                        Object::Table(_) => ObjectKind::Table,
                        Object::View(_) => ObjectKind::View,
                        Object::Proc(_) => ObjectKind::Procedure,
                        Object::Func(_) => ObjectKind::Function,
                        Object::Seq(_) => ObjectKind::Sequence,
                    },
                })
                .collect();
            v.sort_by(|a, b| (a.schema.to_lowercase(), a.name.to_lowercase()).cmp(&(b.schema.to_lowercase(), b.name.to_lowercase())));
            v
        })
        .unwrap_or_default()
    }

    /// The `CREATE` text of a view, procedure or function.
    pub fn definition(&self, name: &str) -> Option<String> {
        self.with_db(|d| match find_object(d, name) {
            Some(Object::View(v)) => Some(v.text.clone()),
            Some(Object::Proc(p)) => Some(p.text.clone()),
            Some(Object::Func(f)) => Some(f.def.text.clone()),
            _ => None,
        })
        .flatten()
    }

    /// Names of all databases on the server.
    pub fn databases(&self) -> Vec<String> {
        let g = self.server.shared.state.read().unwrap_or_else(|e| e.into_inner());
        g.databases.values().map(|d| d.name.clone()).collect()
    }

    /// `@@TRANCOUNT`: how many `BEGIN TRAN`s are open.
    pub fn transaction_count(&self) -> i32 {
        self.session.trancount
    }

    /// The login this session is running as.
    pub fn login(&self) -> String {
        self.session.login.clone()
    }
}
