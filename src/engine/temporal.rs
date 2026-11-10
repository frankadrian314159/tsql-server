//! System-versioned (temporal) tables.
//!
//! A temporal table has a pair of `datetime2` period columns (`GENERATED ALWAYS AS ROW START / ROW END`) and a
//! history table of the same shape. Every statement that updates or deletes rows copies the old row image, closed
//! at the statement's UTC time, into the history table; every statement that inserts or updates rows stamps the
//! new image with that time as its period start (and `9999-12-31 23:59:59.9999999` as its end). `FOR SYSTEM_TIME`
//! reads the current table and the history table together and keeps the rows whose period matches.
//!
//! Differences from SQL Server: the time stamped is the statement's start time (SQL Server uses the transaction's
//! begin time); explicit values for the period columns are overwritten instead of rejected; the history table has
//! no retention policy; and schema changes to a versioned table are refused (turn versioning off first).
use super::query::{Env, Rowset};
use super::*;
use crate::convert;
use crate::storage::Row;
use chrono::NaiveDateTime;

/// The end of every current row's period.
pub fn period_max() -> NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(9999, 12, 31).unwrap().and_hms_nano_opt(23, 59, 59, 999_999_900).unwrap()
}

/// Now in UTC at `datetime2(7)` precision.
pub fn utc_now() -> NaiveDateTime {
    use chrono::Timelike;
    let n = chrono::Utc::now().naive_utc();
    n.with_nanosecond(n.nanosecond() / 100 * 100).unwrap_or(n)
}

fn dt(v: &Value) -> Option<NaiveDateTime> {
    match v {
        Value::DateTime(d, _) => Some(*d),
        _ => None,
    }
}

impl<'a> Engine<'a> {
    /// Column positions of the period, for a table with one.
    pub fn period_cols(t: &Table) -> Option<(usize, usize)> {
        let p = t.temporal.as_ref()?;
        Some((t.col_index(&p.start)?, t.col_index(&p.end)?))
    }

    /// Does `t` currently record history (a system-versioned table)?
    pub fn is_versioned(t: &Table) -> bool {
        t.temporal.as_ref().is_some_and(|p| p.history.is_some())
    }

    /// Is `t` the history table of a system-versioned table?
    pub fn is_history(t: &Table) -> bool {
        t.temporal.as_ref().is_some_and(|p| p.history_for.is_some())
    }

    /// Refuse a statement that would break the temporal link (`what` names it, e.g. "Drop table").
    pub fn temporal_guard(&self, t: &Table, what: &str) -> Result<()> {
        if Self::is_versioned(t) || Self::is_history(t) {
            return Err(SqlError::new(13552, format!("{} operation failed on table '{}.{}.{}' because it is not supported operation on system-versioned temporal tables.", what, t.db, t.schema, t.name)));
        }
        Ok(())
    }

    /// Stamp the period of every new row image written by one statement.
    pub fn stamp_period(t: &Table, ch: &mut super::dml::Changes, now: NaiveDateTime) {
        let Some((s, e)) = Self::period_cols(t) else { return };
        if Self::is_history(t) {
            return;
        }
        for (_, row) in ch.updates.iter_mut() {
            row[s] = Value::DateTime(now, DtKind::DateTime2);
            row[e] = Value::DateTime(period_max(), DtKind::DateTime2);
        }
        for row in ch.inserts.iter_mut() {
            row[s] = Value::DateTime(now, DtKind::DateTime2);
            row[e] = Value::DateTime(period_max(), DtKind::DateTime2);
        }
    }

    /// Give a row that has no period yet its (placeholder) values, so NOT NULL checks pass before it is stamped.
    pub fn fill_period(t: &Table, row: &mut Row) {
        if let Some((s, e)) = Self::period_cols(t) {
            if row[s].is_null() {
                row[s] = Value::DateTime(utc_now(), DtKind::DateTime2);
            }
            if row[e].is_null() {
                row[e] = Value::DateTime(period_max(), DtKind::DateTime2);
            }
        }
    }

    /// Copy the old images of a statement's updated and deleted rows into the history table.
    pub fn write_history(&mut self, t: &Table, applied: &super::dml::Applied, now: NaiveDateTime) -> Result<()> {
        let (Some((s, e)), Some((hs, hn))) = (Self::period_cols(t), t.temporal.as_ref().and_then(|p| p.history.clone())) else { return Ok(()) };
        let mut rows: Vec<Row> = vec![];
        for old in applied.old_updated.iter().chain(applied.deleted.iter()) {
            // a row created by this very statement time has no history yet
            if dt(&old[s]).is_some_and(|st| st >= now) {
                continue;
            }
            let mut r = old.clone();
            r[e] = Value::DateTime(now, DtKind::DateTime2);
            rows.push(r);
        }
        if rows.is_empty() {
            return Ok(());
        }
        let hloc = Loc::Db(t.db.to_lowercase(), format!("{}.{}", hs, hn).to_lowercase());
        let mut h = self.checkout_table(&hloc).ok_or_else(|| SqlError::new(13542, format!("Cannot find the history table '{}.{}'.", hs, hn)))?;
        let r = self.apply_changes(&hloc, &mut h, super::dml::Changes { updates: vec![], deletes: vec![], inserts: rows }, &[], "HISTORY");
        self.checkin_table(&hloc, h);
        r.map(|_| ())
    }

    /// Check the period definition of a table being built and record it.
    pub fn define_period(&mut self, t: &mut Table, defs: &[TableDef]) -> Result<()> {
        let mut period = None;
        for d in defs {
            if let TableDef::Period { start, end } = d {
                if period.is_some() {
                    return Err(SqlError::new(13509, format!("Table '{}' can have only one PERIOD FOR SYSTEM_TIME definition.", t.name)));
                }
                period = Some((start.clone(), end.clone()));
            }
        }
        let gen: Vec<(&ColumnDef, bool)> = defs.iter().filter_map(|d| if let TableDef::Column(c) = d { c.generated.map(|(st, _)| (c, st)) } else { None }).collect();
        let Some((s, e)) = period else {
            if let Some((c, _)) = gen.first() {
                return Err(SqlError::new(13503, format!("Column '{}' in table '{}' is declared GENERATED ALWAYS AS ROW START/END but the table has no PERIOD FOR SYSTEM_TIME.", c.name, t.name)));
            }
            return Ok(());
        };
        for (n, start) in [(&s, true), (&e, false)] {
            let ci = t.col_index(n).ok_or_else(|| SqlError::new(13508, format!("Period column '{}' does not exist in table '{}'.", n, t.name)))?;
            if !matches!(t.columns[ci].ty, DataType::DateTime2(_)) {
                return Err(SqlError::new(13510, format!("Period column '{}' must be of type datetime2.", n)));
            }
            if !gen.iter().any(|(c, st)| c.name.eq_ignore_ascii_case(n) && *st == start) {
                return Err(SqlError::new(13511, format!("Period column '{}' must be declared GENERATED ALWAYS AS ROW {}.", n, if start { "START" } else { "END" })));
            }
            t.columns[ci].nullable = false;
            t.columns[ci].hidden = gen.iter().any(|(c, _)| c.name.eq_ignore_ascii_case(n) && c.generated.is_some_and(|(_, h)| h));
        }
        t.temporal = Some(Temporal { start: t.columns[t.col_index(&s).unwrap()].name.clone(), end: t.columns[t.col_index(&e).unwrap()].name.clone(), history: None, history_for: None });
        Ok(())
    }

    /// Link `base` (a table not yet, or already, in the catalog of `db`) to a history table: the one named, created
    /// when it does not exist.
    pub fn attach_history(&mut self, db: &str, base: &mut Table, named: &Option<Name>) -> Result<()> {
        if base.is_temp || base.temporal.is_none() {
            return Err(SqlError::new(13506, format!("SYSTEM_VERSIONING requires table '{}' to have a PERIOD FOR SYSTEM_TIME.", base.name)));
        }
        let (hdb, hschema, hname) = match named {
            Some(n) => self.ddl_names(n),
            None => (db.to_string(), base.schema.clone(), format!("MSSQL_TemporalHistoryFor_{}", base.object_id)),
        };
        if hdb != db {
            return Err(SqlError::new(13550, "The history table must be in the same database as the temporal table."));
        }
        self.check_schema(&hdb, &hschema)?;
        let hkey = format!("{}.{}", hschema, hname).to_lowercase();
        if hkey == base.key() {
            return Err(SqlError::new(13506, "A table cannot be its own history table."));
        }
        let existing = self.srv.databases.get(&hdb).and_then(|d| d.objects.get(&hkey)).cloned();
        match existing {
            Some(Object::Table(h)) => {
                if h.temporal.is_some() {
                    return Err(SqlError::new(13549, format!("'{}.{}' is already part of system versioning and cannot be a history table.", h.schema, h.name)));
                }
                let same = h.columns.len() == base.columns.len() && h.columns.iter().zip(&base.columns).all(|(a, b)| a.name.eq_ignore_ascii_case(&b.name) && a.ty == b.ty);
                if !same {
                    return Err(SqlError::new(13547, format!("Setting SYSTEM_VERSIONING to ON failed because history table '{}' does not have the same columns as the temporal table.", h.name)));
                }
                let hloc = Loc::Db(hdb.clone(), hkey.clone());
                self.ensure_tx()?;
                self.undo_table(&hloc, &h);
                let mut h = self.take_table(&hloc).unwrap();
                h.temporal = Some(Temporal { start: base.temporal.as_ref().unwrap().start.clone(), end: base.temporal.as_ref().unwrap().end.clone(), history: None, history_for: Some(base.key()) });
                self.put_table(&hloc, h);
            }
            Some(_) => return Err(self.invalid_object(&[hname])),
            None => {
                self.check_create("CREATE TABLE", &hschema)?;
                let cols: Vec<Column> = base
                    .columns
                    .iter()
                    .map(|c| {
                        let mut c = c.clone();
                        c.identity = None;
                        c.default = None;
                        c.default_text = None;
                        c.default_name = None;
                        c.computed = None;
                        c.computed_text = None;
                        c.hidden = false;
                        c.mask = None;
                        c
                    })
                    .collect();
                let mut h = Table::new(&base.db, &hschema, &hname, cols, &self.srv.store.clone());
                h.object_id = self.srv.new_object_id();
                let p = base.temporal.as_ref().unwrap();
                h.temporal = Some(Temporal { start: p.start.clone(), end: p.end.clone(), history: None, history_for: Some(base.key()) });
                if let Some((s, e)) = Self::period_cols(&h) {
                    h.indexes.push(IndexDef { name: format!("ix_{}", hname), cols: vec![(e, false), (s, false)], unique: false, include: vec![], filter: None });
                    h.sync_indexes();
                }
                self.ensure_tx()?;
                self.undo_object(&hdb, &hkey);
                self.srv.cat_mut().databases.get_mut(&hdb).unwrap().objects.insert(hkey.clone(), Object::Table(h));
            }
        }
        base.temporal.as_mut().unwrap().history = Some((hschema, hname));
        Ok(())
    }

    /// `ALTER TABLE t SET (SYSTEM_VERSIONING = ON | OFF ...)`
    pub fn set_versioning(&mut self, name: &[String], v: &Versioning) -> Result<()> {
        let loc = self.locate(name);
        let cur = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(name))?;
        self.check_alter(&loc, name.last().unwrap())?;
        self.ddl_lock(&loc, &cur)?;
        let Loc::Db(db, _) = &loc else { return Err(SqlError::new(13506, "System versioning is not supported on temporary tables or table variables.")) };
        let db = db.clone();
        if v.on {
            if Self::is_versioned(&cur) {
                return Ok(());
            }
            self.undo_table(&loc, &cur);
            let mut t = self.take_table(&loc).unwrap();
            let r = self.attach_history(&db, &mut t, &v.history);
            self.put_table(&loc, t);
            r
        } else {
            let Some((hs, hn)) = cur.temporal.as_ref().and_then(|p| p.history.clone()) else {
                return Err(SqlError::new(13591, format!("Table '{}' is not a system-versioned temporal table.", cur.name)));
            };
            self.undo_table(&loc, &cur);
            let hloc = Loc::Db(db.clone(), format!("{}.{}", hs, hn).to_lowercase());
            if let Some(h) = self.table_ref(&hloc).cloned() {
                self.undo_table(&hloc, &h);
                let mut h = self.take_table(&hloc).unwrap();
                h.temporal = None;
                self.put_table(&hloc, h);
            }
            let mut t = self.take_table(&loc).unwrap();
            t.temporal.as_mut().unwrap().history = None;
            self.put_table(&loc, t);
            Ok(())
        }
    }

    /// `FROM t FOR SYSTEM_TIME ...`: the current rows and the history rows whose period matches.
    pub fn eval_temporal(&mut self, name: &[String], alias: Option<&str>, clause: &TemporalClause) -> Result<Rowset> {
        let loc = self.locate(name);
        let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(name))?;
        let (Some((s, e)), Some((hs, hn))) = (Self::period_cols(&t), t.temporal.as_ref().and_then(|p| p.history.clone())) else {
            return Err(SqlError::new(13544, format!("Temporal FOR SYSTEM_TIME clause can only be used with system-versioned temporal tables. '{}' is not a system-versioned temporal table.", name.join("."))));
        };
        let q = alias.unwrap_or_else(|| name.last().unwrap()).to_string();
        let mut cur = self.eval_named_source(name, Some(&q))?;
        let hname = vec![t.db.clone(), hs, hn];
        let hist = self.eval_named_source(&hname, Some(&q))?;
        let mut bound = |x: &Option<Expr>| -> Result<NaiveDateTime> {
            let v = self.ev(x.as_ref().ok_or_else(|| SqlError::new(102, "Incorrect syntax near FOR SYSTEM_TIME."))?, &Env::empty())?;
            match convert::cast(&v, &DataType::DateTime2(7), None)? {
                Value::DateTime(d, _) => Ok(d),
                _ => Err(SqlError::new(13517, "The FOR SYSTEM_TIME bounds must be datetime2 values.")),
            }
        };
        let (a, b) = match clause.kind {
            TemporalKind::All => (NaiveDateTime::MIN, NaiveDateTime::MAX),
            TemporalKind::AsOf => (bound(&clause.a)?, NaiveDateTime::MIN),
            _ => (bound(&clause.a)?, bound(&clause.b)?),
        };
        let keep = |r: &Row| -> bool {
            let (Some(st), Some(en)) = (dt(&r[s]), dt(&r[e])) else { return false };
            match clause.kind {
                TemporalKind::All => true,
                TemporalKind::AsOf => st <= a && en > a,
                TemporalKind::FromTo => st < b && en > a,
                TemporalKind::Between => st <= b && en > a,
                TemporalKind::ContainedIn => st >= a && en <= b,
            }
        };
        let mut rows: Vec<Row> = cur.rows.iter().filter(|r| keep(r)).cloned().collect();
        rows.extend(hist.rows.iter().filter(|r| keep(r)).cloned());
        cur.rows = std::sync::Arc::new(rows);
        Ok(cur)
    }
}
