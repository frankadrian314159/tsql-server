//! T-SQL that recreates catalog objects, for restoring a backup somewhere other than this engine's catalog (a cluster's control
//! node runs it through the cluster, which places the tables on its shards).
//!
//! Unlike [`crate::tools::script_table`], which works from the summary in `inspect::TableInfo`, this works from the catalog
//! itself and keeps what changes the meaning of the data: column collations, masks, clustered and nonclustered keys, unique
//! indexes with their filters, and disabled check constraints. XML, spatial and full-text indexes, which only declare what a
//! query may use, are left out and named in [`TableScript::skipped`]. What it cannot recreate faithfully it names
//! ([`not_scriptable`]), so that the caller refuses the table rather than restore something different.
use crate::catalog::{Column, Table};
use crate::tools::bracket;
use crate::types::DataType;

/// The statements that recreate a table, in the order they can be run.
pub struct TableScript {
    /// `CREATE TABLE` with the columns (types, collations, masks, identity, defaults, computed columns), the primary key, the
    /// unique constraints and the check constraints.
    pub create: String,
    /// What follows at once, while the table is empty: `NOCHECK` of the check constraints that are disabled.
    pub after_create: Vec<String>,
    /// The foreign keys, once every table they refer to exists.
    pub foreign_keys: Vec<String>,
    /// The other indexes (unique indexes included), best built once the rows are in.
    pub indexes: Vec<String>,
    /// What is left out, as notes for the person restoring (a disabled foreign key, a full-text index).
    pub skipped: Vec<String>,
}

/// `[schema].[name]` of a table.
pub fn table_name(t: &Table) -> String {
    format!("{}.{}", bracket(&t.schema), bracket(&t.name))
}

/// Why a table cannot be recreated faithfully by [`table`], or None.
pub fn not_scriptable(t: &Table) -> Option<String> {
    if t.temporal.is_some() || t.columns.iter().any(|c| c.hidden) {
        return Some("it is a system-versioned (temporal) table".into());
    }
    if t.graph.is_some() {
        return Some("it is a graph (node or edge) table".into());
    }
    if t.columnstore.is_some() {
        return Some("it has a columnstore index".into());
    }
    if let Some(c) = t.columns.iter().find(|c| c.xml_schema.is_some()) {
        return Some(format!("its column {} is xml typed by a schema collection", c.name));
    }
    if let Some(f) = t.fks.iter().find(|f| !f.ref_db.is_empty() && !f.ref_db.eq_ignore_ascii_case(&t.db)) {
        return Some(format!("its foreign key {} refers to another database", f.name));
    }
    None
}

fn column(c: &Column) -> String {
    if let Some(f) = &c.computed_text {
        return format!("{} AS ({})", bracket(&c.name), f);
    }
    let mut l = format!("{} {}", bracket(&c.name), c.ty);
    if let Some(coll) = &c.collation {
        l.push_str(&format!(" COLLATE {}", coll));
    }
    if let Some(m) = &c.mask {
        l.push_str(&format!(" MASKED WITH (FUNCTION = '{}')", m.replace('\'', "''")));
    }
    if let Some((s, i)) = c.identity {
        l.push_str(&format!(" IDENTITY({},{})", s, i));
    }
    l.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if let Some(d) = &c.default_text {
        if let Some(n) = &c.default_name {
            l.push_str(&format!(" CONSTRAINT {}", bracket(n)));
        }
        l.push_str(&format!(" DEFAULT ({})", d));
    }
    l
}

/// The statements that recreate table `t` (see [`TableScript`]). Call [`not_scriptable`] first.
pub fn table(t: &Table) -> TableScript {
    table_sharded(t, None, &[])
}

/// The unique indexes and constraints of `t` (not its primary key) that do not contain column `key`: a cluster that shards the table
/// on `key` cannot check them across its shards.
pub fn uniques_without(t: &Table, key: usize) -> Vec<&str> {
    t.uniques.iter().filter(|u| !u.cols.contains(&key)).map(|u| u.name.as_str()).collect()
}

/// As [`table`], for a table that is sharded on column `shard_key`: a unique index or constraint that does not contain it is made an
/// ordinary index (uniqueness across shards is not checked), and a note in `skipped` says so. A computed column whose expression
/// mentions one of `missing_functions` (lower case names of functions that will not be there) is left out, with a note.
pub fn table_sharded(t: &Table, shard_key: Option<usize>, missing_functions: &[String]) -> TableScript {
    let tq = table_name(t);
    let col = |i: usize| t.columns.get(i).map(|c| bracket(&c.name)).unwrap_or_default();
    let mut skipped = vec![];
    let left_out = |c: &crate::catalog::Column| c.computed.is_some() && c.computed_text.as_ref().is_some_and(|x| {
        let x = x.to_lowercase();
        missing_functions.iter().any(|f| x.contains(&format!("[{}]", f)) || x.contains(&format!("{}(", f)))
    });
    for c in t.columns.iter().filter(|c| left_out(c)) {
        skipped.push(format!("the computed column {} of {} (it calls a stored function)", bracket(&c.name), tq));
    }
    let gone: Vec<usize> = (0..t.columns.len()).filter(|&i| left_out(&t.columns[i])).collect();
    let mut lines: Vec<String> = t.columns.iter().filter(|c| !left_out(c)).map(|c| format!("    {}", column(c))).collect();
    let kind = |clustered: bool| if clustered { "CLUSTERED" } else { "NONCLUSTERED" };
    if let Some(pk) = &t.pk {
        let cols = pk.cols.iter().map(|&c| col(c)).collect::<Vec<_>>().join(", ");
        lines.push(format!("    CONSTRAINT {} PRIMARY KEY {} ({})", bracket(&pk.name), kind(pk.clustered), cols));
    }
    let mut indexes = vec![];
    for u in t.uniques.iter() {
        // (a unique index also has an index definition, which knows the sort direction of its columns)
        let desc = |pos: usize| t.indexes.iter().find(|i| i.name == u.name).and_then(|i| i.cols.get(pos)).map(|c| c.1).unwrap_or(false);
        let cols = u.cols.iter().enumerate().map(|(p, &c)| format!("{}{}", col(c), if desc(p) { " DESC" } else { "" })).collect::<Vec<_>>().join(", ");
        if u.cols.iter().chain(u.include.iter()).any(|c| gone.contains(c)) {
            skipped.push(format!("the {} {} of {} (it uses a computed column that was left out)", if u.is_index { "unique index" } else { "unique constraint" }, u.name, tq));
            continue;
        }
        if let Some(k) = shard_key.filter(|k| !u.cols.contains(k)) {
            let include = if u.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", u.include.iter().map(|&c| col(c)).collect::<Vec<_>>().join(", ")) };
            let filter = u.filter.as_ref().map(|f| format!(" WHERE {}", f.text)).unwrap_or_default();
            indexes.push(format!("CREATE INDEX {} ON {} ({}){}{}", bracket(&u.name), tq, cols, include, filter));
            skipped.push(format!("the uniqueness of the {} {} of {} (it does not contain the shard key {}, and a cluster checks uniqueness only within a shard; it is an ordinary index)", if u.is_index { "unique index" } else { "unique constraint" }, u.name, tq, col(k)));
        } else if u.is_index {
            let include = if u.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", u.include.iter().map(|&c| col(c)).collect::<Vec<_>>().join(", ")) };
            let filter = u.filter.as_ref().map(|f| format!(" WHERE {}", f.text)).unwrap_or_default();
            indexes.push(format!("CREATE UNIQUE {} INDEX {} ON {} ({}){}{}", kind(u.clustered), bracket(&u.name), tq, cols, include, filter));
        } else {
            lines.push(format!("    CONSTRAINT {} UNIQUE {} ({})", bracket(&u.name), kind(u.clustered), cols));
        }
    }
    let mut after_create = vec![];
    for ck in t.checks.iter() {
        lines.push(format!("    CONSTRAINT {} CHECK ({})", bracket(&ck.name), ck.text));
        if !ck.enabled {
            after_create.push(format!("ALTER TABLE {} NOCHECK CONSTRAINT {}", tq, bracket(&ck.name)));
        }
    }
    let create = format!("CREATE TABLE {} (\n{}\n)", tq, lines.join(",\n"));
    for i in t.indexes.iter().filter(|i| !i.unique) {
        if i.cols.iter().map(|c| &c.0).chain(i.include.iter()).any(|c| gone.contains(c)) {
            skipped.push(format!("the index {} of {} (it uses a computed column that was left out)", i.name, tq));
            continue;
        }
        let cols = i.cols.iter().map(|&(c, d)| format!("{}{}", col(c), if d { " DESC" } else { "" })).collect::<Vec<_>>().join(", ");
        let include = if i.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", i.include.iter().map(|&c| col(c)).collect::<Vec<_>>().join(", ")) };
        let filter = i.filter.as_ref().map(|f| format!(" WHERE {}", f.text)).unwrap_or_default();
        indexes.push(format!("CREATE INDEX {} ON {} ({}){}{}", bracket(&i.name), tq, cols, include, filter));
    }
    // (XML and spatial indexes are declarations only, which change no answer; a cluster does not take them)
    for x in &t.xml_indexes {
        skipped.push(format!("the XML index {} of {}", x.name, tq));
    }
    for x in &t.spatial_indexes {
        skipped.push(format!("the spatial index {} of {}", x.name, tq));
    }
    if t.fulltext.is_some() {
        skipped.push(format!("the full-text index of {} (full-text search scans the rows without one)", tq));
    }
    let mut foreign_keys = vec![];
    for f in t.fks.iter() {
        if !f.enabled {
            skipped.push(format!("the disabled foreign key {} of {} (the cluster enforces every foreign key it knows)", f.name, tq));
            continue;
        }
        let own = f.cols.iter().map(|&c| col(c)).collect::<Vec<_>>().join(", ");
        let theirs = f.ref_cols.iter().map(|c| bracket(c)).collect::<Vec<_>>().join(", ");
        let target = f.ref_key.split('.').map(bracket).collect::<Vec<_>>().join(".");
        let mut s = format!("ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})", tq, bracket(&f.name), own, target, theirs);
        if !f.on_delete.is_empty() && !f.on_delete.eq_ignore_ascii_case("NO ACTION") {
            s.push_str(&format!(" ON DELETE {}", f.on_delete));
        }
        if !f.on_update.is_empty() && !f.on_update.eq_ignore_ascii_case("NO ACTION") {
            s.push_str(&format!(" ON UPDATE {}", f.on_update));
        }
        foreign_keys.push(s);
    }
    TableScript { create, after_create, foreign_keys, indexes, skipped }
}

/// The type name of a column as the cluster classifies shard keys by it (`int`, `nvarchar`, ...).
pub fn type_name(ty: &DataType) -> String {
    let s = ty.to_string().to_lowercase();
    s.split(|c: char| c == '(' || c.is_whitespace()).next().unwrap_or("").to_string()
}

/// The statement that creates a view, procedure or function from its stored text, which begins at the `CREATE` or `ALTER` of
/// the statement that last defined it (`ALTER` becomes `CREATE`; `CREATE OR ALTER` stays as it is).
pub fn module(text: &str) -> String {
    let t = text.trim_start();
    if t.len() >= 5 && t[..5].eq_ignore_ascii_case("ALTER") && t[5..].starts_with(|c: char| c.is_whitespace()) {
        format!("CREATE{}", &t[5..])
    } else {
        t.to_string()
    }
}
