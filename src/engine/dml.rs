//! INSERT / UPDATE / DELETE / MERGE / TRUNCATE, constraints, foreign keys, triggers, OUTPUT.
use super::query::{ColMeta, Env, Found, Rowset, Schema};
use super::txn::tab_ref;
use super::*;
use crate::convert::{arith, coerce_store, key_of, to_str};
use crate::storage::lock::Mode;
use crate::storage::tx::Undo;
use crate::storage::{ReadView, RowId, Stamp, TxId};
use std::collections::{HashMap, HashSet};

pub struct Changes {
    pub updates: Vec<(RowId, Row)>,
    pub deletes: Vec<RowId>,
    pub inserts: Vec<Row>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deletes.is_empty() && self.inserts.is_empty()
    }
}

pub struct Applied {
    pub old_updated: Vec<Row>,
    pub new_updated: Vec<Row>,
    pub deleted: Vec<Row>,
    pub inserted: Vec<Row>,
}

pub fn table_schema(t: &Table, alias: &str) -> Schema {
    Schema::new(t.columns.iter().map(|c| ColMeta { qual: Some(alias.to_string()), name: c.name.clone(), ty: Some(c.ty.clone()), denied: None, coll: c.collation.as_deref().and_then(crate::collation::get), mask: c.mask.as_deref().and_then(|m| crate::mask::Mask::parse(m).ok()).map(Arc::new) }).collect())
}

fn fmt_key(vals: &[&Value]) -> String {
    vals.iter().map(|v| if v.is_null() { "<NULL>".to_string() } else { to_str(v) }).collect::<Vec<_>>().join(", ")
}

pub enum UpdPlan {
    Instead { trg: Arc<Trigger>, ins: Vec<Row>, del: Vec<Row>, upd_cols: Vec<bool> },
    Apply { updates: Vec<(RowId, Row)>, upd_cols: Vec<bool> },
}

/// A matched target row: id, current image, and (for UPDATE ... FROM) the joined row.
pub type Matched = (RowId, Row, Option<(Schema, Row)>);

impl<'a> Engine<'a> {
    // ------------------------------------------------------------ targets
    fn dml_target(&mut self, target: &Target, from: &[TableRef], perm: &str) -> Result<(Loc, Vec<String>)> {
        match target {
            Target::Var(v) => {
                if self.ses.frames.last().map(|f| f.tables.contains_key(v)).unwrap_or(false) {
                    Ok((Loc::Var(v.clone()), vec![v.clone()]))
                } else {
                    Err(SqlError::new(1087, format!("Must declare the table variable \"{}\".", v)))
                }
            }
            Target::Table(name) => {
                let mut name = name.clone();
                if name.len() == 1 && !from.is_empty() {
                    if let Some(real) = find_alias(from, &name[0]) {
                        name = real;
                    }
                }
                let loc = self.locate(&name);
                if self.table_ref(&loc).is_none() {
                    return Err(self.invalid_object(&name));
                }
                if !perm.is_empty() {
                    self.check_dml_permission(&loc, &name, perm)?;
                }
                Ok((loc, name))
            }
        }
    }

    fn with_ctes<T>(&mut self, ctes: &[Cte], f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if ctes.is_empty() {
            return f(self);
        }
        let mut m = std::collections::HashMap::new();
        for c in ctes {
            m.insert(c.name.to_lowercase(), CteEntry { query: c.query.clone(), cols: c.cols.clone(), working: None });
        }
        self.ses.cte_stack.push(m);
        let r = f(self);
        self.ses.cte_stack.pop();
        r
    }

    // ------------------------------------------------------------ INSERT
    pub fn exec_insert(&mut self, ins: &Insert) -> Result<()> {
        let mut ins = ins.clone();
        self.rewrite_insert_target(&mut ins);
        let ins = &ins;
        self.with_ctes(&ins.ctes, |s| s.exec_insert_inner(ins))
    }

    fn exec_insert_inner(&mut self, ins: &Insert) -> Result<()> {
        let (loc, name) = self.dml_target(&ins.target, &[], "")?;
        self.check_insert_permission(&loc, &name, ins)?;
        let mut src_rows: Vec<Vec<Option<Value>>> = vec![];
        let mut src_ncols = 0usize;
        match &ins.src {
            InsertSrc::Values(rows) => {
                let env = Env::empty();
                for r in rows {
                    let mut row = vec![];
                    for e in r {
                        if matches!(e, Expr::Default) {
                            row.push(None);
                        } else {
                            row.push(Some(self.ev(e, &env)?));
                        }
                    }
                    src_ncols = row.len();
                    src_rows.push(row);
                }
            }
            InsertSrc::Query(q) => {
                let rs = self.exec_query(q, None)?;
                src_ncols = rs.schema.cols.len();
                src_rows = rs.rows.iter().map(|r| r.iter().cloned().map(Some).collect()).collect();
            }
            InsertSrc::Exec(st) => {
                self.ses.capture.push(vec![]);
                let r = self.exec_stmt(st);
                let cap = self.ses.capture.pop().unwrap_or_default();
                match r {
                    Ok(()) => {}
                    Err(Interrupt::Error(e)) => return Err(e),
                    Err(_) => {}
                }
                if let Some(rs) = cap.into_iter().next() {
                    src_ncols = rs.columns.len();
                    src_rows = rs.rows.into_iter().map(|r| r.into_iter().map(Some).collect()).collect();
                }
            }
            InsertSrc::DefaultValues => {
                src_rows.push(vec![]);
            }
        }
        if let Some((e, pct)) = &ins.top {
            let n = self.top_count(e, *pct, src_rows.len())?;
            src_rows.truncate(n);
        }
        if self.ses.rowcount_limit > 0 {
            src_rows.truncate(self.ses.rowcount_limit as usize);
        }
        let done = self.insert_into_table(&loc, &ins.cols, src_rows, src_ncols, matches!(ins.src, InsertSrc::DefaultValues))?;
        let Some((applied, nrows, tcopy)) = done else {
            self.ses.rowcount = 0;
            return Ok(());
        };
        self.ses.rowcount = nrows as i64;
        let all = vec![true; tcopy.columns.len()];
        self.after_dml(&loc, &tcopy, "INSERT", &applied, &all, ins.output.as_ref(), nrows)?;
        Ok(())
    }

    fn top_count(&mut self, e: &Expr, pct: bool, n: usize) -> Result<usize> {
        let v = self.ev(e, &Env::empty())?;
        if pct {
            let p = convert::num_to_f64(convert::as_num(&v).unwrap_or(convert::Num::I(0)));
            Ok((n as f64 * p / 100.0).ceil() as usize)
        } else {
            Ok(v.as_i64().unwrap_or(0).max(0) as usize)
        }
    }

    #[allow(clippy::type_complexity)]
    fn insert_into_table(&mut self, loc: &Loc, cols: &Option<Vec<String>>, src_rows: Vec<Vec<Option<Value>>>, src_ncols: usize, default_values: bool) -> Result<Option<(Applied, usize, Table)>> {
        let mut t = self.table_ref(loc).cloned().ok_or_else(|| SqlError::new(208, "Invalid object name."))?;
        let t = &mut t;
        let ident_on = self.ses.identity_insert.contains(&format!("{:?}", loc));
        let col_idx: Vec<usize> = match cols {
            Some(cs) => {
                let mut v = vec![];
                for c in cs {
                    match t.col_index(c) {
                        Some(i) => {
                            if t.columns[i].computed.is_some() {
                                return Err(SqlError::new(271, format!("The column \"{}\" cannot be modified because it is either a computed column or is the result of a UNION operator.", c)));
                            }
                            if v.contains(&i) {
                                return Err(SqlError::new(264, format!("The column name '{}' is specified more than once in the SET clause or column list of an INSERT. A column cannot be assigned more than one value in the same clause. Modify the clause to make sure that a column is updated only once. If this statement updates or inserts columns into a view, column aliasing can conceal the duplication in your code.", c)));
                            }
                            v.push(i)
                        }
                        None => return Err(SqlError::new(207, format!("Invalid column name '{}'.", c))),
                    }
                }
                v
            }
            None => (0..t.columns.len())
                .filter(|&i| {
                    let c = &t.columns[i];
                    c.computed.is_none() && !matches!(c.ty, DataType::Timestamp) && (c.identity.is_none() || ident_on)
                })
                .collect(),
        };
        if !default_values {
            if src_ncols > col_idx.len() {
                return Err(SqlError::new(if cols.is_some() { 110 } else { 213 }, if cols.is_some() { "There are fewer columns in the INSERT statement than values specified in the VALUES clause. The number of values in the VALUES clause must match the number of columns specified in the INSERT statement.".to_string() } else { "Column name or number of supplied values does not match table definition.".to_string() }));
            }
            if src_ncols < col_idx.len() {
                return Err(SqlError::new(if cols.is_some() { 109 } else { 213 }, if cols.is_some() { "There are more columns in the INSERT statement than values specified in the VALUES clause. The number of values in the VALUES clause must match the number of columns specified in the INSERT statement.".to_string() } else { "Column name or number of supplied values does not match table definition.".to_string() }));
            }
        }
        let mut new_rows = Vec::with_capacity(src_rows.len());
        for sr in &src_rows {
            let row = self.build_row(t, &col_idx, sr, ident_on)?;
            new_rows.push(row);
        }
        let n = new_rows.len();
        if let Some(trg) = self.instead_trigger(t, "INSERT") {
            let ncols = t.columns.len();
            self.run_trigger(&trg, t, "INSERT", new_rows, vec![], &vec![true; ncols])?;
            return Ok(None);
        }
        let ncols = t.columns.len();
        let _ = t;
        let mut real = self.checkout_table(loc).ok_or_else(|| SqlError::new(208, "Invalid object name."))?;
        let r = self.apply_changes(loc, &mut real, Changes { updates: vec![], deletes: vec![], inserts: new_rows }, &vec![true; ncols], "INSERT");
        let tc = real.clone();
        self.checkin_table(loc, real);
        let ap = r?;
        Ok(Some((ap, n, tc)))
    }

    fn eval_default(&mut self, c: &Column) -> Result<Value> {
        match &c.default {
            Some(d) => {
                let v = self.ev(d, &Env::empty())?;
                convert::cast(&v, &c.ty, None)
            }
            None => Ok(Value::Null),
        }
    }

    pub fn build_row(&mut self, t: &mut Table, cols: &[usize], vals: &[Option<Value>], ident_on: bool) -> Result<Row> {
        let n = t.columns.len();
        let mut row = vec![Value::Null; n];
        let mut set = vec![false; n];
        let tname = t.display_name();
        for (k, &ci) in cols.iter().enumerate() {
            if let Some(Some(v)) = vals.get(k) {
                let col = &t.columns[ci];
                let nv = coerce_store(v, &col.ty, &tname, &col.name)?;
                if col.identity.is_some() {
                    if !ident_on {
                        return Err(SqlError::new(544, format!("Cannot insert explicit value for identity column in table '{}' when IDENTITY_INSERT is set to OFF.", t.name)));
                    }
                    if let (Some(x), Some((_, inc))) = (nv.as_i64(), col.identity) {
                        let _ = t.identity_next.update(|cur| {
                            let next = cur.unwrap_or(x);
                            Ok::<_, ()>(if (inc > 0 && x >= next) || (inc < 0 && x <= next) { (Some(x + inc), ()) } else { (cur, ()) })
                        });
                    }
                }
                row[ci] = nv;
                set[ci] = true;
            }
        }
        for ci in 0..n {
            if set[ci] {
                continue;
            }
            let col = t.columns[ci].clone();
            if col.computed.is_some() {
                continue;
            }
            if let Some((_, inc)) = col.identity {
                // take the value and advance the counter in one step, so sessions never get the same one
                let ty = col.ty.clone();
                let cur = t.identity_next.update(|cur| {
                    let cur = cur.unwrap_or(1);
                    let max_ok = match ty {
                        DataType::TinyInt => cur <= 255,
                        DataType::SmallInt => cur <= 32767 && cur >= -32768,
                        DataType::Int => cur <= i32::MAX as i64 && cur >= i32::MIN as i64,
                        _ => true,
                    };
                    if !max_ok {
                        return Err(SqlError::new(8115, format!("Arithmetic overflow error converting IDENTITY to data type {}.", ty)));
                    }
                    Ok((Some(cur + inc), cur))
                })?;
                row[ci] = convert::cast(&Value::BigInt(cur), &col.ty, None)?;
                self.ses.last_identity = Some(cur);
                continue;
            }
            if matches!(col.ty, DataType::Timestamp) {
                row[ci] = self.next_rowversion();
                continue;
            }
            if col.default.is_some() {
                let v = self.eval_default(&col)?;
                row[ci] = coerce_store(&v, &col.ty, &tname, &col.name)?;
            }
        }
        self.finish_row(t, &mut row, "INSERT")?;
        Ok(row)
    }

    fn next_rowversion(&mut self) -> Value {
        Value::Bin(self.srv.core.new_rowversion().to_be_bytes().to_vec())
    }

    /// Compute computed columns and enforce NOT NULL.
    fn finish_row(&mut self, t: &Table, row: &mut Row, verb: &str) -> Result<()> {
        if t.columns.iter().any(|c| c.computed.is_some()) {
            let schema = table_schema(t, &t.name);
            for ci in 0..t.columns.len() {
                if let Some(expr) = &t.columns[ci].computed {
                    let snapshot = row.clone();
                    let env = Env::new(&schema, &snapshot, None);
                    let v = self.ev(expr, &env)?;
                    row[ci] = convert::cast(&v, &t.columns[ci].ty, None)?;
                }
            }
        }
        for (ci, c) in t.columns.iter().enumerate() {
            if !c.nullable && row[ci].is_null() && c.computed.is_none() {
                return Err(SqlError::new(515, format!("Cannot insert the value NULL into column '{}', table '{}'; column does not allow nulls. {} fails.", c.name, t.display_name(), verb)));
            }
        }
        Ok(())
    }

    // --------------------------------------------------- apply & validate
    /// Validate and apply a set of row changes to `t` (which must be the table stored at `loc`,
    /// currently taken out of the catalog). Everything is checked before the first write.
    pub fn apply_changes(&mut self, loc: &Loc, t: &mut Table, ch: Changes, upd_cols: &[bool], verb: &str) -> Result<Applied> {
        if ch.is_empty() {
            return Ok(Applied { old_updated: vec![], new_updated: vec![], deleted: vec![], inserted: vec![] });
        }
        // The checks and writes below must not interleave with another session's where that could break a rule:
        //
        // * Most tables are latched shared (writers to one table run side by side) plus *stripes* for the key values
        //   the statement checks or changes: the keys of unique indexes it writes, and the foreign-key values it
        //   inserts (child side) or removes (parent side). Two inserts of one key, or a child insert beside the
        //   delete of its parent, hash to the same stripe and take turns; everything else proceeds in parallel.
        // * A table connected through foreign keys to a cascading action may have rows in any of those tables changed
        //   by this statement, so the whole connected set is latched exclusively (in table-id order).
        //
        // A SERIALIZABLE reader's key range is checked *after* the writes (see `apply_changes_latched`), not here.
        let latch = if matches!(loc, Loc::Db(..) | Loc::GTemp(_)) {
            let (mut oids, cascade) = self.fk_component(t);
            if cascade {
                oids.retain(|o| !self.ses.latched.contains(o));
                if oids.is_empty() {
                    None
                } else {
                    let set = self.srv.core.latch(&oids.iter().map(|o| (*o, true)).collect::<Vec<_>>());
                    self.ses.latched.extend(oids.iter().copied());
                    Some((set, oids))
                }
            } else if self.ses.latched.contains(&t.object_id) {
                None
            } else {
                let mut keys: Vec<(i64, bool)> = vec![(t.object_id as i64, false)];
                keys.extend(self.write_stripes(loc, t, &ch).into_iter().map(|s| (s, true)));
                Some((self.srv.core.latch_keys(&keys), vec![]))
            }
        } else {
            None
        };
        let r = self.apply_changes_latched(loc, t, ch, upd_cols, verb);
        if let Some((_set, oids)) = latch {
            self.ses.latched.retain(|o| !oids.contains(o));
        }
        r
    }

    /// The latch stripes a write of `ch` to `t` needs (see `apply_changes`).
    fn write_stripes(&self, loc: &Loc, t: &Table, ch: &Changes) -> Vec<i64> {
        use crate::coord::stripe;
        let oid = t.object_id;
        let mut out: Vec<i64> = vec![];
        let images: Vec<&Row> = ch.updates.iter().map(|(_, r)| r).chain(ch.inserts.iter()).collect();
        for (ix, sp) in t.data.specs.iter().enumerate() {
            if sp.unique {
                for im in &images {
                    let key: Vec<Value> = sp.cols.iter().map(|&c| im[c].clone()).collect();
                    out.push(stripe(oid, 1 + ix as u32, &key_of(&sp.xform(&key))));
                }
            }
        }
        for fk in t.fks.iter().filter(|f| f.enabled) {
            let poid = if fk.ref_key == t.key() && fk.ref_db.eq_ignore_ascii_case(&t.db) {
                Some(oid)
            } else {
                self.table_ref(&Loc::Db(fk.ref_db.clone(), fk.ref_key.clone())).map(|p| p.object_id)
            };
            let Some(poid) = poid else { continue };
            for im in &images {
                let vals: Vec<Value> = fk.cols.iter().map(|&c| im[c].clone()).collect();
                if !vals.iter().any(|v| v.is_null()) {
                    out.push(stripe(poid, 1000, &t.key_string(&fk.cols, &vals)));
                }
            }
        }
        let view = self.row_view(loc);
        let olds: Vec<Row> = ch.updates.iter().map(|u| u.0).chain(ch.deletes.iter().copied()).filter_map(|rid| t.data.get(rid, &view)).collect();
        if !olds.is_empty() {
            for (_, _, fk) in self.referencing_fks(t) {
                let idx: Vec<usize> = fk.ref_cols.iter().filter_map(|c| t.col_index(c)).collect();
                if idx.len() != fk.ref_cols.len() {
                    continue;
                }
                for o in &olds {
                    let vals: Vec<Value> = idx.iter().map(|&c| o[c].clone()).collect();
                    if !vals.iter().any(|v| v.is_null()) {
                        out.push(stripe(oid, 1000, &t.key_string(&idx, &vals)));
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Ids of the tables connected to `t` by foreign keys, in either direction, transitively (including `t`), and
    /// whether any foreign key among them has a cascading action (`ON DELETE/UPDATE CASCADE | SET NULL | SET DEFAULT`).
    fn fk_component(&self, t: &Table) -> (Vec<i32>, bool) {
        use std::collections::{HashMap, VecDeque};
        type K = (String, String);
        let mut adj: HashMap<K, Vec<K>> = HashMap::new();
        let mut ids: HashMap<K, i32> = HashMap::new();
        let mut cascading: HashSet<K> = HashSet::new();
        for (dn, db) in &self.srv.databases {
            for (k, o) in &db.objects {
                if let Object::Table(c) = o {
                    let me: K = (dn.to_lowercase(), k.clone());
                    ids.insert(me.clone(), c.object_id);
                    for fk in &c.fks {
                        if fk.on_delete != "NO ACTION" || fk.on_update != "NO ACTION" {
                            cascading.insert(me.clone());
                        }
                        let parent: K = (fk.ref_db.to_lowercase(), fk.ref_key.clone());
                        adj.entry(me.clone()).or_default().push(parent.clone());
                        adj.entry(parent).or_default().push(me.clone());
                    }
                }
            }
        }
        let start: K = (t.db.to_lowercase(), t.key());
        let mut seen: HashSet<K> = HashSet::new();
        let mut q = VecDeque::new();
        seen.insert(start.clone());
        q.push_back(start);
        while let Some(k) = q.pop_front() {
            for n in adj.get(&k).into_iter().flatten() {
                if seen.insert(n.clone()) {
                    q.push_back(n.clone());
                }
            }
        }
        let cascade = seen.iter().any(|k| cascading.contains(k)) || t.fks.iter().any(|f| f.on_delete != "NO ACTION" || f.on_update != "NO ACTION");
        let mut out: Vec<i32> = seen.iter().filter_map(|k| ids.get(k).copied()).collect();
        if !out.contains(&t.object_id) {
            out.push(t.object_id);
        }
        out.sort_unstable();
        out.dedup();
        (out, cascade)
    }

    fn apply_changes_latched(&mut self, loc: &Loc, t: &mut Table, mut ch: Changes, upd_cols: &[bool], verb: &str) -> Result<Applied> {
        let persistent = matches!(loc, Loc::Db(..) | Loc::GTemp(_));
        let is_var = matches!(loc, Loc::Var(_));
        let mut applied = Applied { old_updated: vec![], new_updated: vec![], deleted: vec![], inserted: vec![] };
        if ch.is_empty() {
            return Ok(applied);
        }
        let tx = if is_var { 0 } else { self.ensure_tx()? };
        let view = self.row_view(loc);
        if persistent {
            self.lock_table(t.object_id, Mode::IX)?;
            let snap = self.ses.tx.as_ref().and_then(|x| x.snapshot_ts);
            let rids: Vec<RowId> = ch.updates.iter().map(|u| u.0).chain(ch.deletes.iter().copied()).collect();
            for rid in rids {
                if let Some(sn) = snap {
                    if t.data.latest_committed_ts(rid) > sn {
                        return Err(SqlError::new(3960, format!("Snapshot isolation transaction aborted due to update conflict. You cannot use snapshot isolation to access table '{}.{}' directly or indirectly in database '{}' to update, delete, or insert the row that has been modified or deleted by another transaction. Retry the transaction or change the isolation level for the update/delete statement.", t.schema, t.name, t.db)));
                    }
                }
                self.lock_row(t.object_id, rid, Mode::X)?;
                // The row was read before it was locked: if another transaction committed a change to it since this
                // statement began, the new row image was computed from stale data. Run the statement again.
                if t.data.latest_committed_ts(rid) > self.ses.stmt_ts {
                    return Err(self.blocked(vec![]));
                }
            }
        }
        // parent-side foreign keys (may add cascaded changes to this table)
        if !ch.deletes.is_empty() {
            self.fk_parent_delete(loc, t, &mut ch)?;
        }
        if !ch.updates.is_empty() {
            self.fk_parent_update(loc, t, &mut ch, upd_cols)?;
        }
        // child-side checks on new images
        {
            let mut images: Vec<&Row> = ch.updates.iter().map(|(_, r)| r).collect();
            images.extend(ch.inserts.iter());
            self.check_constraints(t, &images, verb)?;
        }
        self.check_fk_child(t, &ch, upd_cols, verb, view, tx)?;
        for image in ch.updates.iter().map(|(_, r)| r).chain(ch.inserts.iter()) {
            if let Some((name, n)) = t.data.oversized_key(image) {
                return Err(SqlError::new(1946, format!("Operation failed. The index entry of length {} bytes for the index '{}' exceeds the maximum length of {} bytes for nonclustered indexes.", n, name, crate::storage::MAX_KEY_BYTES)));
            }
        }
        let keys_touched = !ch.inserts.is_empty() || t.all_keys().iter().any(|k| k.cols.iter().any(|&c| upd_cols.get(c).copied().unwrap_or(true)))
            // a row can also enter a filtered unique index by changing a column of its filter
            || t.data.specs.iter().any(|sp| sp.unique && sp.filter.as_ref().is_some_and(|f| f.conds.iter().any(|c| upd_cols.get(c.col).copied().unwrap_or(true))));
        if keys_touched {
            self.check_unique(t, &ch, tx)?;
        }
        // row-level security: block predicates on the rows about to be written (new images) or changed (old images)
        // (only for tables that have a block predicate: reading every old row back would cost a lookup per row)
        if persistent && self.has_block_policy(loc) {
            use crate::catalog::PredKind;
            let olds_upd: Vec<Row> = ch.updates.iter().filter_map(|u| t.data.get(u.0, &view)).collect();
            let olds_del: Vec<Row> = ch.deletes.iter().filter_map(|rid| t.data.get(*rid, &view)).collect();
            let (old_upd, old_del): (Vec<&Row>, Vec<&Row>) = (olds_upd.iter().collect(), olds_del.iter().collect());
            let new_upd: Vec<&Row> = ch.updates.iter().map(|(_, r)| r).collect();
            let new_ins: Vec<&Row> = ch.inserts.iter().collect();
            self.rls_check(loc, t, PredKind::BlockBeforeUpdate, &old_upd)?;
            self.rls_check(loc, t, PredKind::BlockBeforeDelete, &old_del)?;
            self.rls_check(loc, t, PredKind::BlockAfterUpdate, &new_upd)?;
            self.rls_check(loc, t, PredKind::BlockAfterInsert, &new_ins)?;
        }
        // ---- writes
        let stamp = if is_var { Stamp::Ts(0) } else { Stamp::Tx(tx) };
        let tab = tab_ref(loc);
        let oid = t.object_id;
        let mut touched: Vec<RowId> = vec![];
        {
            let d = &*t.data;
            for (rid, row) in &ch.updates {
                if let Some(old) = d.get(*rid, &view) {
                    applied.old_updated.push(old);
                }
                applied.new_updated.push(row.clone());
                d.push_version(*rid, Some(row.clone()), stamp);
                touched.push(*rid);
            }
            let mut dels: Vec<RowId> = ch.deletes.clone();
            dels.sort_unstable();
            dels.dedup();
            for rid in dels {
                if let Some(old) = d.get(rid, &view) {
                    applied.deleted.push(old);
                }
                d.push_version(rid, None, stamp);
                touched.push(rid);
            }
            for row in &ch.inserts {
                let rid = d.alloc();
                d.push_version(rid, Some(row.clone()), stamp);
                applied.inserted.push(row.clone());
                touched.push(rid);
            }
        }
        if is_var {
            let d = &*t.data;
            for rid in touched {
                d.gc_slot(rid, u64::MAX);
            }
        } else {
            let n_new = ch.inserts.len();
            let first_new = touched.len() - n_new;
            for (i, rid) in touched.iter().enumerate() {
                if persistent && i >= first_new {
                    // fresh rows are exclusively ours
                    let _ = self.srv.core.locks.acquire(tx, &crate::storage::lock::Res::Row(oid, *rid), Mode::X);
                }
                if let Some(tab) = &tab {
                    self.push_undo(Undo::Row { tab: tab.clone(), rowid: *rid });
                }
            }
            // A SERIALIZABLE reader's key range must not gain a row (a phantom). This is checked after the new
            // versions are in place: a reader registers its range before it reads, so either it sees these
            // (uncommitted) versions and waits for us, or we see its range here and wait for it. A wait unwinds
            // the statement, which removes the versions again.
            if persistent {
                let mut images: Vec<&Row> = ch.updates.iter().map(|(_, r)| r).collect();
                images.extend(ch.inserts.iter());
                self.check_key_ranges(t, &images)?;
            }
        }
        Ok(applied)
    }

    fn key_error(t: &Table, name: &str, is_index: bool, vals: &[&Value]) -> SqlError {
        if is_index {
            SqlError::new(2601, format!("Cannot insert duplicate key row in object '{}.{}' with unique index '{}'. The duplicate key value is ({}).", t.schema, t.name, name, fmt_key(vals)))
        } else {
            let kind = if t.pk.as_ref().map(|p| p.name == name).unwrap_or(false) { "PRIMARY KEY" } else { "UNIQUE KEY" };
            SqlError::new(2627, format!("Violation of {} constraint '{}'. Cannot insert duplicate key in object '{}.{}'. The duplicate key value is ({}).", kind, name, t.schema, t.name, fmt_key(vals)))
        }
    }

    fn check_unique(&mut self, t: &Table, ch: &Changes, tx: TxId) -> Result<()> {
        let excluded: HashSet<RowId> = ch.updates.iter().map(|u| u.0).chain(ch.deletes.iter().copied()).collect();
        for (ix, sp) in t.data.specs.iter().enumerate() {
            if !sp.unique {
                continue;
            }
            let is_index = t.uniques.iter().any(|u| u.name == sp.name && u.is_index);
            let mut batch: HashSet<String> = HashSet::new();
            let updated_images = ch.updates.iter().map(|(_, r)| r);
            for image in updated_images.chain(ch.inserts.iter()) {
                if !sp.admits(image) {
                    continue; // a filtered unique index only constrains the rows it contains
                }
                let key: Vec<Value> = sp.cols.iter().map(|&c| image[c].clone()).collect();
                let kk = key_of(&sp.xform(&key));
                if !batch.insert(kk.clone()) {
                    let refs: Vec<&Value> = key.iter().collect();
                    return Err(Self::key_error(t, &sp.name, is_index, &refs));
                }
                for rid in t.data.seek_eq(ix, &key) {
                    if excluded.contains(&rid) {
                        continue;
                    }
                    if let Some(v) = t.data.last_version(rid) {
                        if let Stamp::Tx(w) = v.stamp {
                            if w != tx && tx != 0 {
                                return Err(self.blocked(vec![w]));
                            }
                        }
                        if let Some(r) = &v.row {
                            let existing: Vec<Value> = sp.cols.iter().map(|&c| r[c].clone()).collect();
                            if key_of(&sp.xform(&existing)) == kk {
                                let refs: Vec<&Value> = key.iter().collect();
                                return Err(Self::key_error(t, &sp.name, is_index, &refs));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn check_constraints(&mut self, t: &Table, rows: &[&Row], verb: &str) -> Result<()> {
        if t.checks.is_empty() {
            return Ok(());
        }
        let schema = table_schema(t, &t.name);
        for r in rows {
            for ck in &t.checks {
                if !ck.enabled {
                    continue;
                }
                let env = Env::new(&schema, r, None);
                let v = self.ev(&ck.expr, &env)?;
                if matches!(v, Value::Bit(false)) {
                    let colpart = ck.col.as_ref().map(|c| format!(", column '{}'", c)).unwrap_or_default();
                    return Err(SqlError::new(547, format!("The {} statement conflicted with the CHECK constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\"{}.", verb, ck.name, t.db, t.schema, t.name, colpart)));
                }
            }
        }
        Ok(())
    }

    fn check_fk_child(&mut self, t: &Table, ch: &Changes, upd_cols: &[bool], verb: &str, _view: ReadView, tx: TxId) -> Result<()> {
        for fk in &t.fks {
            if !fk.enabled {
                continue;
            }
            let relevant: Vec<&Row> = if fk.cols.iter().any(|&c| upd_cols.get(c).copied().unwrap_or(true)) {
                ch.updates.iter().map(|(_, r)| r).chain(ch.inserts.iter()).collect()
            } else {
                ch.inserts.iter().collect()
            };
            if relevant.is_empty() {
                continue;
            }
            let self_ref = fk.ref_key == t.key() && fk.ref_db.eq_ignore_ascii_case(&t.db);
            let ploc = Loc::Db(fk.ref_db.clone(), fk.ref_key.clone());
            // a parent that is itself being changed up the stack (a cascade) is checked by that statement
            if !self_ref && self.ses.checked_out.contains(&format!("{:?}", ploc)) {
                continue;
            }
            let ptab: Table = if self_ref {
                t.clone()
            } else {
                match self.table_ref(&ploc) {
                    Some(p) => p.clone(),
                    None => continue,
                }
            };
            let pview = if self_ref { ReadView::committed(tx) } else { self.row_view(&ploc) };
            let ref_idx: Vec<usize> = fk.ref_cols.iter().filter_map(|c| ptab.col_index(c)).collect();
            let spec_pos = ptab.data.specs.iter().position(|sp| sp.unique && sp.filter.is_none() && sp.cols.len() == ref_idx.len() && sp.cols.iter().all(|c| ref_idx.contains(c)));
            // keys from this batch (self references may point at rows inserted together)
            let mut batch: HashSet<String> = HashSet::new();
            if self_ref {
                for r in ch.updates.iter().map(|(_, r)| r).chain(ch.inserts.iter()) {
                    let vals: Vec<Value> = ref_idx.iter().map(|&c| r[c].clone()).collect();
                    batch.insert(ptab.key_string(&ref_idx, &vals));
                }
            }
            for r in relevant {
                let vals: Vec<Value> = fk.cols.iter().map(|&c| r[c].clone()).collect();
                if vals.iter().any(|v| v.is_null()) {
                    continue;
                }
                let mut found = batch.contains(&ptab.key_string(&ref_idx, &vals));
                if !found {
                    match spec_pos {
                        Some(sp) => {
                            let spec_cols = ptab.data.specs[sp].cols.clone();
                            let key: Vec<Value> = spec_cols.iter().map(|c| vals[ref_idx.iter().position(|x| x == c).unwrap()].clone()).collect();
                            for rid in ptab.data.seek_eq(sp, &key) {
                                if let Some(w) = ptab.data.foreign_writer(rid, tx) {
                                    if tx != 0 {
                                        return Err(self.blocked(vec![w]));
                                    }
                                }
                                if ptab.data.get(rid, &pview).is_some() {
                                    found = true;
                                    break;
                                }
                            }
                        }
                        None => {
                            let want = ptab.key_string(&ref_idx, &vals);
                            for (_, pr) in ptab.data.iter(pview).into_iter() {
                                let pv: Vec<Value> = ref_idx.iter().map(|&c| pr[c].clone()).collect();
                                if ptab.key_string(&ref_idx, &pv) == want {
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                if !found {
                    return Err(SqlError::new(547, format!("The {} statement conflicted with the FOREIGN KEY constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\", column '{}'.", verb, fk.name, t.db, ptab.schema, ptab.name, fk.ref_cols.first().cloned().unwrap_or_default())));
                }
            }
        }
        Ok(())
    }

    /// All (db, key, fk) referencing the given table (including self references).
    fn referencing_fks(&self, t: &Table) -> Vec<(String, String, FkDef)> {
        let mut out = vec![];
        let tk = t.key();
        for (dn, db) in &self.srv.databases {
            for (k, o) in &db.objects {
                if let Object::Table(c) = o {
                    for fk in &c.fks {
                        if fk.ref_key == tk && fk.ref_db.eq_ignore_ascii_case(&t.db) {
                            out.push((dn.clone(), k.clone(), fk.clone()));
                        }
                    }
                }
            }
        }
        // a temporary table is not in the catalog
        for fk in t.fks.iter().filter(|_| t.is_temp) {
            if fk.ref_key == tk && fk.ref_db.eq_ignore_ascii_case(&t.db) {
                out.push((t.db.to_lowercase(), tk.clone(), fk.clone()));
            }
        }
        out
    }

    /// Rows of `ctab` whose foreign-key columns equal one of `keys` (key values in fk.cols order).
    fn find_children(&mut self, cloc: &Loc, ctab: &Table, fk: &FkDef, keys: &[Vec<Value>]) -> Result<Vec<(RowId, Row)>> {
        let tx = self.cur_tx();
        let view = self.row_view(cloc);
        let mut out = vec![];
        if let Some(ix) = ctab.data.specs.iter().position(|s| s.cols == fk.cols && s.filter.is_none()) {
            let mut seen = HashSet::new();
            for k in keys {
                for rid in ctab.data.seek_eq(ix, k) {
                    if !seen.insert(rid) {
                        continue;
                    }
                    if let Some(w) = ctab.data.foreign_writer(rid, tx) {
                        return Err(self.blocked(vec![w]));
                    }
                    if let Some(r) = ctab.data.get(rid, &view) {
                        out.push((rid, r));
                    }
                }
            }
        } else {
            let want: HashSet<String> = keys.iter().map(|k| ctab.key_string(&fk.cols, k)).collect();
            for (rid, r) in self.scan_rows(cloc, ctab)? {
                let vals: Vec<Value> = fk.cols.iter().map(|&c| r[c].clone()).collect();
                if vals.iter().any(|v| v.is_null()) {
                    continue;
                }
                if want.contains(&ctab.key_string(&fk.cols, &vals)) {
                    out.push((rid, r));
                }
            }
        }
        Ok(out)
    }

    fn apply_child(&mut self, cloc: &Loc, ch: Changes, verb: &str) -> Result<()> {
        let mut c = match self.checkout_table(cloc) {
            Some(c) => c,
            None => return Ok(()), // cycle: already being modified further up
        };
        let n = c.columns.len();
        let r = self.apply_changes(cloc, &mut c, ch, &vec![true; n], verb);
        self.checkin_table(cloc, c);
        r.map(|_| ())
    }

    fn fk_parent_delete(&mut self, loc: &Loc, t: &mut Table, ch: &mut Changes) -> Result<()> {
        let refs = self.referencing_fks(t);
        if refs.is_empty() {
            return Ok(());
        }
        let tk = t.key();
        let view = self.row_view(loc);
        let mut pending: Vec<RowId> = ch.deletes.clone();
        let mut done: HashSet<RowId> = HashSet::new();
        while !pending.is_empty() {
            let batch: Vec<RowId> = pending.drain(..).filter(|i| done.insert(*i)).collect();
            if batch.is_empty() {
                break;
            }
            for (dn, ck, fk) in &refs {
                let ref_idx: Vec<usize> = fk.ref_cols.iter().filter_map(|c| t.col_index(c)).collect();
                let mut keys: Vec<Vec<Value>> = vec![];
                for &i in &batch {
                    if let Some(r) = t.data.get(i, &view) {
                        let vals: Vec<Value> = ref_idx.iter().map(|&c| r[c].clone()).collect();
                        if !vals.iter().any(|v| v.is_null()) {
                            keys.push(vals);
                        }
                    }
                }
                if keys.is_empty() {
                    continue;
                }
                let is_self = *ck == tk && dn.eq_ignore_ascii_case(&t.db);
                let cloc = Loc::Db(dn.clone(), ck.clone());
                if is_self {
                    let tclone = t.clone();
                    let hits = self.find_children(loc, &tclone, fk, &keys)?;
                    for (rid, row) in hits {
                        if ch.deletes.contains(&rid) {
                            continue;
                        }
                        match fk.on_delete.as_str() {
                            "CASCADE" => {
                                ch.deletes.push(rid);
                                pending.push(rid);
                            }
                            "SET NULL" | "SET DEFAULT" => {
                                let mut nr = row.clone();
                                for &c in &fk.cols {
                                    nr[c] = Value::Null;
                                }
                                ch.updates.retain(|(r, _)| *r != rid);
                                ch.updates.push((rid, nr));
                            }
                            _ => return Err(self.ref_violation("DELETE", fk, t)),
                        }
                    }
                    continue;
                }
                let ctab = match self.table_ref(&cloc) {
                    Some(c) => c.clone(),
                    None => continue,
                };
                let hits = self.find_children(&cloc, &ctab, fk, &keys)?;
                if hits.is_empty() {
                    continue;
                }
                match fk.on_delete.as_str() {
                    "CASCADE" => {
                        self.apply_child(&cloc, Changes { updates: vec![], deletes: hits.iter().map(|h| h.0).collect(), inserts: vec![] }, "DELETE")?;
                    }
                    "SET NULL" | "SET DEFAULT" => {
                        let ups = hits
                            .into_iter()
                            .map(|(rid, mut r)| {
                                for &c in &fk.cols {
                                    r[c] = Value::Null;
                                }
                                (rid, r)
                            })
                            .collect();
                        self.apply_child(&cloc, Changes { updates: ups, deletes: vec![], inserts: vec![] }, "UPDATE")?;
                    }
                    _ => return Err(self.ref_violation("DELETE", fk, &ctab)),
                }
            }
        }
        Ok(())
    }

    fn ref_violation(&self, verb: &str, fk: &FkDef, child: &Table) -> SqlError {
        let mut cname = child.name.clone();
        let mut cschema = child.schema.clone();
        let mut ccol = fk.cols.first().and_then(|&i| child.columns.get(i)).map(|c| c.name.clone()).unwrap_or_default();
        for db in self.srv.databases.values() {
            for o in db.objects.values() {
                if let Object::Table(c) = o {
                    if c.fks.iter().any(|f| f.name == fk.name) {
                        cname = c.name.clone();
                        cschema = c.schema.clone();
                        ccol = fk.cols.first().and_then(|&i| c.columns.get(i)).map(|c| c.name.clone()).unwrap_or_default();
                    }
                }
            }
        }
        let dbname = self.srv.databases.get(&fk.ref_db).map(|d| d.name.clone()).unwrap_or_else(|| fk.ref_db.clone());
        SqlError::new(547, format!("The {} statement conflicted with the REFERENCE constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\", column '{}'.", verb, fk.name, dbname, cschema, cname, ccol))
    }

    fn fk_parent_update(&mut self, loc: &Loc, t: &mut Table, ch: &mut Changes, upd_cols: &[bool]) -> Result<()> {
        let refs = self.referencing_fks(t);
        if refs.is_empty() {
            return Ok(());
        }
        let tk = t.key();
        let view = self.row_view(loc);
        for (dn, ck, fk) in &refs {
            let ref_idx: Vec<usize> = fk.ref_cols.iter().filter_map(|c| t.col_index(c)).collect();
            if !ref_idx.iter().any(|&c| upd_cols.get(c).copied().unwrap_or(true)) {
                continue;
            }
            let mut old_new: Vec<(Vec<Value>, Vec<Value>)> = vec![];
            for (i, new) in ch.updates.iter() {
                if let Some(old) = t.data.get(*i, &view) {
                    let ov: Vec<Value> = ref_idx.iter().map(|&c| old[c].clone()).collect();
                    let nv: Vec<Value> = ref_idx.iter().map(|&c| new[c].clone()).collect();
                    if t.key_string(&ref_idx, &ov) != t.key_string(&ref_idx, &nv) && !ov.iter().any(|v| v.is_null()) {
                        old_new.push((ov, nv));
                    }
                }
            }
            if old_new.is_empty() {
                continue;
            }
            let keys: Vec<Vec<Value>> = old_new.iter().map(|(o, _)| o.clone()).collect();
            let is_self = *ck == tk && dn.eq_ignore_ascii_case(&t.db);
            let cloc = Loc::Db(dn.clone(), ck.clone());
            let ctab = if is_self {
                t.clone()
            } else {
                match self.table_ref(&cloc) {
                    Some(c) => c.clone(),
                    None => continue,
                }
            };
            let hits = self.find_children(if is_self { loc } else { &cloc }, &ctab, fk, &keys)?;
            if hits.is_empty() {
                continue;
            }
            let map: HashMap<String, Vec<Value>> = old_new.iter().map(|(o, n)| (ctab.key_string(&fk.cols, o), n.clone())).collect();
            let mut child_updates = vec![];
            for (rid, mut r) in hits {
                let cur: Vec<Value> = fk.cols.iter().map(|&c| r[c].clone()).collect();
                let nv = map.get(&ctab.key_string(&fk.cols, &cur)).cloned().unwrap_or_default();
                match fk.on_update.as_str() {
                    "CASCADE" | "SET NULL" | "SET DEFAULT" => {
                        for (k, &c) in fk.cols.iter().enumerate() {
                            r[c] = if fk.on_update == "CASCADE" { nv[k].clone() } else { Value::Null };
                        }
                        child_updates.push((rid, r));
                    }
                    _ => return Err(SqlError::new(547, format!("The UPDATE statement conflicted with the REFERENCE constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\", column '{}'.", fk.name, ctab.db, ctab.schema, ctab.name, fk.cols.first().and_then(|&i| ctab.columns.get(i)).map(|c| c.name.clone()).unwrap_or_default()))),
                }
            }
            if is_self {
                for (rid, r) in child_updates {
                    ch.updates.retain(|(x, _)| *x != rid);
                    ch.updates.push((rid, r));
                }
            } else {
                self.apply_child(&cloc, Changes { updates: child_updates, deletes: vec![], inserts: vec![] }, "UPDATE")?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ triggers
    /// The INSTEAD OF trigger to run for this event, unless that trigger is already executing.
    pub fn instead_trigger(&self, t: &Table, event: &str) -> Option<Arc<Trigger>> {
        let trg = t.triggers.iter().find(|g| g.instead && g.events.iter().any(|e| e == event)).cloned()?;
        let key = t.key();
        if self.ses.trigger_stack.iter().any(|c| c.instead_of && c.event == event && c.table.key() == key) {
            return None;
        }
        Some(trg)
    }

    pub fn run_trigger(&mut self, trg: &Arc<Trigger>, t: &Table, event: &str, inserted: Vec<Row>, deleted: Vec<Row>, updated: &[bool]) -> Result<()> {
        if self.ses.nest_level >= 32 {
            return Err(SqlError::new(217, "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32)."));
        }
        let tc = TriggerCtx { table: t.clone(), inserted, deleted, event: event.to_string(), updated_cols: updated.to_vec(), instead_of: trg.instead };
        self.ses.trigger_stack.push(tc);
        self.ses.frames.push(Frame::default());
        self.ses.strict += 1;
        self.ses.nest_level += 1;
        let had_tx = self.ses.tx.is_some();
        let owner = self.schema_owner_of(&t.db.to_lowercase(), &t.schema);
        self.ses.module_owner.push(owner);
        let r = self.exec_stmts(&trg.body);
        self.ses.module_owner.pop();
        self.ses.nest_level -= 1;
        self.ses.strict -= 1;
        self.ses.frames.pop();
        self.ses.trigger_stack.pop();
        match r {
            Ok(()) | Err(Interrupt::Return) => {}
            Err(Interrupt::Error(e)) => return Err(e),
            Err(_) => {}
        }
        if had_tx && self.ses.tx.is_none() {
            let mut e = SqlError::new(3609, "The transaction ended in the trigger. The batch has been aborted.");
            e.abort_batch = true;
            return Err(e);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn after_dml(&mut self, _loc: &Loc, t: &Table, event: &str, ap: &Applied, upd_cols: &[bool], output: Option<&Output>, nrows: usize) -> Result<()> {
        if let Some(o) = output {
            let (ins, del): (Vec<Row>, Vec<Row>) = match event {
                "INSERT" => (ap.inserted.clone(), vec![]),
                "DELETE" => (vec![], ap.deleted.clone()),
                _ => (ap.new_updated.clone(), ap.old_updated.clone()),
            };
            self.emit_output(o, t, &ins, &del, None)?;
        }
        let trgs: Vec<Arc<Trigger>> = t.triggers.iter().filter(|g| !g.instead && g.events.iter().any(|e| e == event)).cloned().collect();
        if !trgs.is_empty() {
            let (ins, del) = match event {
                "INSERT" => (ap.inserted.clone(), vec![]),
                "DELETE" => (vec![], ap.deleted.clone()),
                _ => (ap.new_updated.clone(), ap.old_updated.clone()),
            };
            if !ins.is_empty() || !del.is_empty() {
                let upd: Vec<bool> = if event == "UPDATE" { upd_cols.to_vec() } else { vec![true; t.columns.len()] };
                for trg in trgs {
                    self.run_trigger(&trg, t, event, ins.clone(), del.clone(), &upd)?;
                }
            }
        }
        if !self.ses.nocount {
            self.emit(Event::RowCount(nrows as i64));
        }
        Ok(())
    }

    fn emit_output(&mut self, o: &Output, t: &Table, ins: &[Row], del: &[Row], action: Option<&[String]>) -> Result<()> {
        let ncols = t.columns.len();
        let mut cols: Vec<ColMeta> = t.columns.iter().map(|c| ColMeta { qual: Some("inserted".into()), name: c.name.clone(), ty: Some(c.ty.clone()), denied: None, coll: c.collation.as_deref().and_then(crate::collation::get), mask: None }).collect();
        cols.extend(t.columns.iter().map(|c| ColMeta { qual: Some("deleted".into()), name: c.name.clone(), ty: Some(c.ty.clone()), denied: None, coll: c.collation.as_deref().and_then(crate::collation::get), mask: None }));
        if action.is_some() {
            cols.push(ColMeta { qual: None, name: "$action".into(), ty: Some(DataType::NVarChar(10)), denied: None, coll: None, mask: None });
        }
        let schema = Schema::new(cols);
        let n = ins.len().max(del.len());
        let mut rows: Vec<Row> = vec![];
        for i in 0..n {
            let mut r: Row = if i < ins.len() && !ins[i].is_empty() { ins[i].clone() } else { vec![Value::Null; ncols] };
            r.extend(if i < del.len() && !del[i].is_empty() { del[i].clone() } else { vec![Value::Null; ncols] });
            if let Some(a) = action {
                r.push(Value::Str(a[i].clone()));
            }
            rows.push(r);
        }
        let rs = self.project_items(&o.items, &schema, &rows)?;
        match &o.into {
            None => {
                self.emit_rowset(rs);
            }
            Some(target) => {
                let (loc, _) = self.dml_target(target, &[], "INSERT")?;
                let src: Vec<Vec<Option<Value>>> = rs.rows.iter().map(|r| r.iter().cloned().map(Some).collect()).collect();
                let ncols = rs.schema.cols.len();
                self.insert_into_table(&loc, &o.cols, src, ncols, false)?;
            }
        }
        Ok(())
    }

    /// Evaluate select items over a fixed set of rows (OUTPUT clause).
    pub fn project_items(&mut self, items: &[SelectItem], schema: &Schema, rows: &[Row]) -> Result<Rowset> {
        let mut cols: Vec<(String, Option<&Expr>, Option<usize>, Option<DataType>)> = vec![];
        for it in items {
            match it {
                SelectItem::Star(q) => {
                    for (i, c) in schema.cols.iter().enumerate() {
                        let ok = match q {
                            None => c.qual.is_some(),
                            Some(qp) => c.qual.as_deref().map(|x| x.eq_ignore_ascii_case(&qp[qp.len() - 1])).unwrap_or(false),
                        };
                        if ok && c.name != "$action" {
                            cols.push((c.name.clone(), None, Some(i), c.ty.clone()));
                        }
                    }
                }
                SelectItem::Expr { e, alias } => {
                    let mut name = alias.clone().unwrap_or_default();
                    let mut ty = None;
                    if let Expr::Col(p) = e {
                        if let Found::One(i) = schema.find_parts(p) {
                            if alias.is_none() {
                                name = schema.cols[i].name.clone();
                            }
                            ty = schema.cols[i].ty.clone();
                        } else if alias.is_none() {
                            name = p[p.len() - 1].clone();
                        }
                    }
                    cols.push((name, Some(e), None, ty));
                }
                SelectItem::Assign { .. } => {}
            }
        }
        let mut out = vec![];
        for r in rows {
            let env = Env::new(schema, r, None);
            let mut row = vec![];
            for (_, e, idx, _) in &cols {
                if let Some(i) = idx {
                    row.push(r[*i].clone());
                } else {
                    row.push(self.ev(e.unwrap(), &env)?);
                }
            }
            out.push(row);
        }
        let sch = Schema::new(cols.iter().map(|c| ColMeta { qual: None, name: c.0.clone(), ty: c.3.clone(), denied: None, coll: None, mask: None }).collect());
        Ok(Rowset::new(sch, out))
    }

    // ------------------------------------------------------------ UPDATE
    pub fn exec_update(&mut self, u: &Update) -> Result<()> {
        let mut u = u.clone();
        if let (Target::Table(name), true) = (&u.target, u.from.is_empty()) {
            if let Some((base, filter, _)) = self.updatable_source(name, &u.ctes) {
                u.target = Target::Table(base);
                u.where_ = and_filter(&u.where_, filter);
            }
        }
        let u = &u;
        self.with_ctes(&u.ctes, |s| s.exec_update_inner(u))
    }

    /// Find the rows an UPDATE/DELETE affects, locking them for update.
    fn matched_rows(&mut self, loc: &Loc, t: &Table, tname_aliases: &[String], from: &[TableRef], where_: &Option<Expr>, current_of: &Option<String>) -> Result<Vec<Matched>> {
        let mut mine: Vec<Expr> = vec![];
        let mut access_alias = t.name.clone();
        let mut filter_here = true;
        if from.is_empty() {
            if let Some(w) = where_ {
                let mut conj = vec![];
                super::optimizer::split_conjuncts(w, &mut conj);
                mine = conj.into_iter().filter(|c| super::optimizer::is_pushable(c)).cloned().collect();
            }
        } else {
            filter_here = false;
            for (a, es) in self.plan_pushdown(from, where_) {
                if tname_aliases.iter().any(|n| n.eq_ignore_ascii_case(&a)) {
                    access_alias = a;
                    mine.extend(es);
                }
            }
        }
        let conj: Vec<&Expr> = mine.iter().collect();
        let ap = self.choose_access(t, &access_alias, &conj);
        let scanned = self.scan_rows_ap(loc, t, &ap)?;
        let tschema = table_schema(t, &t.name);
        let scanned = if filter_here { self.filter_pushed(&tschema, scanned, &mine)? } else { scanned };
        let mut out: Vec<Matched> = vec![];
        if let Some(cur) = current_of {
            let c = self.ses.cursors.get(cur).ok_or_else(|| SqlError::with(16916, 16, 1, format!("A cursor with the name '{}' does not exist.", cur)))?;
            if c.pos < 0 || c.pos as usize >= c.rows.len() {
                return Ok(out);
            }
            let k = key_of(&c.rows[c.pos as usize]);
            for (rid, r) in scanned {
                if key_of(&r) == k {
                    out.push((rid, r, None));
                    break;
                }
            }
        } else if from.is_empty() {
            let schema = table_schema(t, &t.name);
            for (rid, r) in scanned {
                let ok = match where_ {
                    None => true,
                    Some(w) => {
                        let env = Env::new(&schema, &r, None);
                        matches!(self.ev(w, &env)?, Value::Bit(true))
                    }
                };
                if ok {
                    out.push((rid, r, None));
                }
            }
        } else {
            let mut cur = self.eval_table_ref(&from[0], None)?;
            for tr in &from[1..] {
                let r = self.eval_table_ref(tr, None)?;
                cur = self.join_rowsets(JoinKind::Cross, cur, r, &None, None)?;
            }
            let tcols: Vec<usize> = (0..cur.schema.cols.len())
                .filter(|&i| cur.schema.cols[i].qual.as_deref().map(|q| tname_aliases.iter().any(|a| a.eq_ignore_ascii_case(q))).unwrap_or(false))
                .collect();
            if tcols.len() != t.columns.len() {
                return Err(SqlError::new(4104, "The multi-part identifier could not be bound: target table not found in FROM clause."));
            }
            let mut index: HashMap<String, Vec<usize>> = HashMap::new();
            for (n, (_, r)) in scanned.iter().enumerate() {
                index.entry(key_of(r)).or_default().push(n);
            }
            let mut seen: HashSet<usize> = HashSet::new();
            let rows = cur.rows.clone();
            for r in rows.iter() {
                if let Some(w) = where_ {
                    let env = Env::new(&cur.schema, r, None);
                    if !matches!(self.ev(w, &env)?, Value::Bit(true)) {
                        continue;
                    }
                }
                let trow: Row = tcols.iter().map(|&i| r[i].clone()).collect();
                if let Some(idxs) = index.get(&key_of(&trow)) {
                    for &n in idxs {
                        if seen.insert(n) {
                            out.push((scanned[n].0, scanned[n].1.clone(), Some((cur.schema.clone(), r.clone()))));
                        }
                    }
                }
            }
        }
        if matches!(loc, Loc::Db(..)) {
            for (rid, _, _) in &out {
                self.lock_row(t.object_id, *rid, Mode::U)?;
            }
            if !out.is_empty() {
                self.lock_table(t.object_id, Mode::IX)?;
            }
        }
        Ok(out)
    }

    /// UPDATE permission: table-wide, or on every column the statement assigns; in the column-level case the
    /// columns read by `WHERE` and by the assigned expressions need SELECT permission too.
    /// INSERT needs the INSERT permission on the table, or INSERT granted on every column the statement names.
    fn check_insert_permission(&mut self, loc: &Loc, name: &[String], ins: &Insert) -> Result<()> {
        let shown = name.last().map(|s| s.as_str()).unwrap_or("");
        let first = match self.check_object_perm(loc, "INSERT", shown) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let (Loc::Db(dbk, key), Some(cols)) = (loc, &ins.cols) else { return Err(first) };
        let Some(t) = self.table_ref(loc) else { return Err(first) };
        let (Some(user), Some(db)) = (self.db_user(dbk), self.srv.databases.get(dbk)) else { return Err(first) };
        if cols.is_empty() || !db.security.has_column_entries(&user, key) {
            return Err(first);
        }
        let schema = key.split('.').next().unwrap_or("dbo").to_string();
        for c in cols {
            let Some(i) = t.col_index(c) else { continue };
            let col = &t.columns[i].name;
            if !db.security.allowed_column(&user, "INSERT", &schema, key, col) {
                return Err(SqlError::with(230, 14, 1, format!("The INSERT permission was denied on the column '{}' of the object '{}', database '{}', schema '{}'.", col, t.name, db.name, schema)));
            }
        }
        Ok(())
    }

    /// DELETE needs SELECT on every column its WHERE clause reads (table-wide SELECT, or column grants).
    fn check_delete_reads(&mut self, loc: &Loc, t: &Table, d: &Delete) -> Result<()> {
        let Some(w) = &d.where_ else { return Ok(()) };
        let Loc::Db(dbk, key) = loc else { return Ok(()) };
        if self.check_object_perm(loc, "SELECT", &t.name).is_ok() {
            return Ok(());
        }
        let mut stack: Vec<&Expr> = vec![w];
        let mut read: Vec<String> = vec![];
        while let Some(e) = stack.pop() {
            if let Expr::Col(p) = e {
                read.push(p[p.len() - 1].clone());
            }
            super::query::children(e, &mut stack);
        }
        let schema = key.split('.').next().unwrap_or("dbo").to_string();
        if let (Some(user), Some(db)) = (self.db_user(dbk), self.srv.databases.get(dbk)) {
            for c in read {
                if let Some(i) = t.col_index(&c) {
                    let col = &t.columns[i].name;
                    if !db.security.allowed_column(&user, "SELECT", &schema, key, col) {
                        return Err(SqlError::with(230, 14, 1, format!("The SELECT permission was denied on the column '{}' of the object '{}', database '{}', schema '{}'.", col, t.name, db.name, schema)));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_update_permission(&mut self, loc: &Loc, name: &[String], t: &Table, u: &Update) -> Result<()> {
        let shown = name.last().map(|s| s.as_str()).unwrap_or("");
        if self.check_object_perm(loc, "UPDATE", shown).is_ok() {
            return Ok(());
        }
        let assigned: Vec<usize> = u
            .sets
            .iter()
            .filter_map(|s| if let SetItem::Col { parts, .. } = s { parts.last().and_then(|c| t.col_index(c)) } else { None })
            .collect();
        self.check_update_columns(loc, name, t, &assigned)?;
        // columns read by the statement
        let mut exprs: Vec<&Expr> = u.where_.iter().collect();
        for s in &u.sets {
            let (SetItem::Col { e, .. } | SetItem::Var { e, .. }) = s;
            exprs.push(e);
        }
        let mut stack = exprs;
        let mut read: Vec<String> = vec![];
        while let Some(e) = stack.pop() {
            if let Expr::Col(p) = e {
                read.push(p[p.len() - 1].clone());
            }
            super::query::children(e, &mut stack);
        }
        if let Loc::Db(dbk, key) = loc {
            let schema = key.split('.').next().unwrap_or("dbo").to_string();
            if let (Some(user), Some(db)) = (self.db_user(dbk), self.srv.databases.get(dbk)) {
                for c in read {
                    if let Some(i) = t.col_index(&c) {
                        let col = &t.columns[i].name;
                        if !db.security.allowed_column(&user, "SELECT", &schema, key, col) {
                            return Err(SqlError::with(230, 14, 1, format!("The SELECT permission was denied on the column '{}' of the object '{}', database '{}', schema '{}'.", col, t.name, db.name, schema)));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn exec_update_inner(&mut self, u: &Update) -> Result<()> {
        let (loc, name) = self.dml_target(&u.target, &u.from, "")?;
        let mut t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(&name))?;
        self.check_update_permission(&loc, &name, &t, u)?;
        let plan = self.update_core(&loc, &mut t, u, &name)?;
        match plan {
            UpdPlan::Instead { trg, ins, del, upd_cols } => {
                self.run_trigger(&trg, &t, "UPDATE", ins, del, &upd_cols)?;
                self.ses.rowcount = 0;
                Ok(())
            }
            UpdPlan::Apply { updates, upd_cols } => {
                drop(t);
                let n = updates.len();
                let mut real = self.checkout_table(&loc).ok_or_else(|| self.invalid_object(&name))?;
                let r = self.apply_changes(&loc, &mut real, Changes { updates, deletes: vec![], inserts: vec![] }, &upd_cols, "UPDATE");
                let tc = real.clone();
                self.checkin_table(&loc, real);
                let ap = r?;
                self.ses.rowcount = n as i64;
                self.after_dml(&loc, &tc, "UPDATE", &ap, &upd_cols, u.output.as_ref(), n)
            }
        }
    }

    fn update_core(&mut self, loc: &Loc, t: &mut Table, u: &Update, name: &[String]) -> Result<UpdPlan> {
        let mut aliases = vec![t.name.clone()];
        if let Target::Table(orig) = &u.target {
            aliases.push(orig.last().unwrap().clone());
        }
        aliases.push(name.last().unwrap().clone());
        let mut matched = self.matched_rows(loc, t, &aliases, &u.from, &u.where_, &u.current_of)?;
        if let Some((e, pct)) = &u.top {
            let n = self.top_count(e, *pct, matched.len())?;
            matched.truncate(n);
        }
        if self.ses.rowcount_limit > 0 {
            matched.truncate(self.ses.rowcount_limit as usize);
        }
        let schema = table_schema(t, &t.name);
        let tname = t.display_name();
        let mut upd_cols = vec![false; t.columns.len()];
        enum Tgt {
            Col(usize),
            Var(String),
        }
        let mut targets = vec![];
        for s in &u.sets {
            match s {
                SetItem::Col { parts, .. } => {
                    let cname = parts.last().unwrap();
                    let ci = t.col_index(cname).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", cname)))?;
                    if t.columns[ci].computed.is_some() {
                        return Err(SqlError::new(271, format!("The column \"{}\" cannot be modified because it is either a computed column or is the result of a UNION operator.", cname)));
                    }
                    if t.columns[ci].identity.is_some() {
                        return Err(SqlError::new(8102, format!("Cannot update identity column '{}'.", cname)));
                    }
                    upd_cols[ci] = true;
                    targets.push(Tgt::Col(ci));
                }
                SetItem::Var { var, .. } => targets.push(Tgt::Var(var.clone())),
            }
        }
        let mut updates = vec![];
        let mut old_images: Vec<Row> = vec![];
        let ts_cols: Vec<usize> = t.columns.iter().enumerate().filter(|(_, c)| matches!(c.ty, DataType::Timestamp)).map(|(i, _)| i).collect();
        for (rid, old, extra) in &matched {
            let mut new = old.clone();
            let (env_schema, env_row): (Schema, Row) = match extra {
                Some((s, r)) => (s.clone(), r.clone()),
                None => (schema.clone(), old.clone()),
            };
            let env = Env::new(&env_schema, &env_row, None);
            let mut vals = vec![];
            for (k, s) in u.sets.iter().enumerate() {
                let (op, e) = match s {
                    SetItem::Col { op, e, .. } | SetItem::Var { op, e, .. } => (op, e),
                };
                let mut v = if matches!(e, Expr::Default) {
                    match &targets[k] {
                        Tgt::Col(ci) => {
                            let c = t.columns[*ci].clone();
                            self.eval_default(&c)?
                        }
                        _ => Value::Null,
                    }
                } else {
                    self.ev(e, &env)?
                };
                if let Some(op) = op {
                    let cur = match &targets[k] {
                        Tgt::Col(ci) => old[*ci].clone(),
                        Tgt::Var(v) => self.get_var(v)?,
                    };
                    v = arith(*op, &cur, &v)?;
                }
                vals.push(v);
            }
            for (k, v) in vals.into_iter().enumerate() {
                match &targets[k] {
                    Tgt::Col(ci) => {
                        let col = &t.columns[*ci];
                        new[*ci] = coerce_store(&v, &col.ty, &tname, &col.name)?;
                    }
                    Tgt::Var(var) => self.set_var(var, v)?,
                }
            }
            for &c in &ts_cols {
                new[c] = self.next_rowversion();
            }
            self.finish_row(t, &mut new, "UPDATE")?;
            updates.push((*rid, new));
            old_images.push(old.clone());
        }
        if let Some(trg) = self.instead_trigger(t, "UPDATE") {
            let ins: Vec<Row> = updates.iter().map(|(_, r)| r.clone()).collect();
            return Ok(UpdPlan::Instead { trg, ins, del: old_images, upd_cols });
        }
        Ok(UpdPlan::Apply { updates, upd_cols })
    }

    // ------------------------------------------------------------ DELETE
    pub fn exec_delete(&mut self, d: &Delete) -> Result<()> {
        let mut d = d.clone();
        if let (Target::Table(name), true) = (&d.target, d.from.is_empty()) {
            if let Some((base, filter, _)) = self.updatable_source(name, &d.ctes) {
                d.target = Target::Table(base);
                d.where_ = and_filter(&d.where_, filter);
            }
        }
        let d = &d;
        self.with_ctes(&d.ctes, |s| s.exec_delete_inner(d))
    }

    fn exec_delete_inner(&mut self, d: &Delete) -> Result<()> {
        let (loc, name) = self.dml_target(&d.target, &d.from, "DELETE")?;
        let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(&name))?;
        self.check_delete_reads(&loc, &t, d)?;
        let mut aliases = vec![t.name.clone(), name.last().unwrap().clone()];
        if let Target::Table(orig) = &d.target {
            aliases.push(orig.last().unwrap().clone());
        }
        let mut matched = self.matched_rows(&loc, &t, &aliases, &d.from, &d.where_, &d.current_of)?;
        if let Some((e, pct)) = &d.top {
            let n = self.top_count(e, *pct, matched.len())?;
            matched.truncate(n);
        }
        if self.ses.rowcount_limit > 0 {
            matched.truncate(self.ses.rowcount_limit as usize);
        }
        let idxs: Vec<RowId> = matched.iter().map(|m| m.0).collect();
        if let Some(trg) = self.instead_trigger(&t, "DELETE") {
            let del: Vec<Row> = matched.iter().map(|m| m.1.clone()).collect();
            self.run_trigger(&trg, &t, "DELETE", vec![], del, &vec![true; t.columns.len()])?;
            self.ses.rowcount = 0;
            return Ok(());
        }
        let ncols = t.columns.len();
        drop(t); // release the snapshot's hold on the data
        let mut real = self.checkout_table(&loc).ok_or_else(|| self.invalid_object(&name))?;
        let r = self.apply_changes(&loc, &mut real, Changes { updates: vec![], deletes: idxs, inserts: vec![] }, &vec![true; ncols], "DELETE");
        let tc = real.clone();
        self.checkin_table(&loc, real);
        let ap = r?;
        let n = ap.deleted.len();
        self.ses.rowcount = n as i64;
        self.after_dml(&loc, &tc, "DELETE", &ap, &vec![true; ncols], d.output.as_ref(), n)
    }

    // ---------------------------------------------------------- TRUNCATE
    pub fn exec_truncate(&mut self, target: &Target) -> Result<()> {
        let (loc, name) = self.dml_target(target, &[], "ALTER")?;
        let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(&name))?;
        let refs = self.referencing_fks(&t);
        if refs.iter().any(|(_, ck, _)| *ck != t.key()) {
            return Err(SqlError::new(4712, format!("Cannot truncate table '{}' because it is being referenced by a FOREIGN KEY constraint.", name.join("."))));
        }
        if matches!(loc, Loc::Db(..)) {
            self.lock_table(t.object_id, Mode::X)?;
        }
        let rows = self.scan_rows(&loc, &t)?;
        drop(t);
        let mut real = self.checkout_table(&loc).ok_or_else(|| self.invalid_object(&name))?;
        if let Some(c) = real.identity_col() {
            real.identity_next.set(real.columns[c].identity.map(|(s, _)| s));
        }
        let ncols = real.columns.len();
        let ids: Vec<RowId> = rows.iter().map(|r| r.0).collect();
        // TRUNCATE does not check child rows (none can reference the table) and fires no triggers
        let saved_fks = std::mem::take(&mut real.fks);
        let r = self.apply_changes(&loc, &mut real, Changes { updates: vec![], deletes: ids, inserts: vec![] }, &vec![true; ncols], "DELETE");
        real.fks = saved_fks;
        self.checkin_table(&loc, real);
        r.map(|_| ())
    }

    // ------------------------------------------------------------- MERGE
    pub fn exec_merge(&mut self, m: &Merge) -> Result<()> {
        self.with_ctes(&m.ctes, |s| s.exec_merge_inner(m))
    }

    fn exec_merge_inner(&mut self, m: &Merge) -> Result<()> {
        let loc = self.locate(&m.target);
        let mut t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(&m.target))?;
        for w in &m.whens {
            let perm = match w.action {
                MergeAction::Insert { .. } => "INSERT",
                MergeAction::Update(_) => "UPDATE",
                MergeAction::Delete => "DELETE",
            };
            self.check_dml_permission(&loc, &m.target, perm)?;
        }
        let src = self.eval_table_ref(&m.source, None)?;
        let (ch, upd_cols, actions) = self.merge_core(&loc, &mut t, m, src)?;
        drop(t);
        let n = actions.len();
        let mut real = self.checkout_table(&loc).ok_or_else(|| self.invalid_object(&m.target))?;
        let r = self.apply_changes(&loc, &mut real, ch, &upd_cols, "MERGE");
        let tc = real.clone();
        self.checkin_table(&loc, real);
        let ap = r?;
        self.ses.rowcount = n as i64;
        if let Some(o) = &m.output {
            let ins: Vec<Row> = actions.iter().map(|a| a.1.clone()).collect();
            let del: Vec<Row> = actions.iter().map(|a| a.2.clone()).collect();
            let names: Vec<String> = actions.iter().map(|a| a.0.clone()).collect();
            self.emit_output(o, &tc, &ins, &del, Some(&names))?;
        }
        let evs: [(&str, Vec<Row>, Vec<Row>); 3] = [("INSERT", ap.inserted.clone(), vec![]), ("UPDATE", ap.new_updated.clone(), ap.old_updated.clone()), ("DELETE", vec![], ap.deleted.clone())];
        for (ev, ins, del) in evs {
            let trgs: Vec<Arc<Trigger>> = tc.triggers.iter().filter(|g| !g.instead && g.events.iter().any(|e| e == ev)).cloned().collect();
            if trgs.is_empty() || (ins.is_empty() && del.is_empty()) {
                continue;
            }
            for trg in trgs {
                self.run_trigger(&trg, &tc, ev, ins.clone(), del.clone(), &vec![true; tc.columns.len()])?;
            }
        }
        if !self.ses.nocount {
            self.emit(Event::RowCount(n as i64));
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn merge_core(&mut self, loc: &Loc, t: &mut Table, m: &Merge, src: Rowset) -> Result<(Changes, Vec<bool>, Vec<(String, Row, Row)>)> {
        let talias = m.target_alias.clone().unwrap_or_else(|| t.name.clone());
        let tschema = table_schema(t, &talias);
        let combined = tschema.concat(&src.schema);
        let ident_on = self.ses.identity_insert.contains(&format!("{:?}", loc));
        let tname = t.display_name();
        let old_rows: Vec<(RowId, Row)> = self.scan_rows(loc, t)?;
        let mut src_matches: Vec<Vec<usize>> = vec![];
        for sr in src.rows.iter() {
            let mut hits = vec![];
            for (ti, (_, tr)) in old_rows.iter().enumerate() {
                let mut row = tr.clone();
                row.extend(sr.iter().cloned());
                let env = Env::new(&combined, &row, None);
                if matches!(self.ev(&m.on, &env)?, Value::Bit(true)) {
                    hits.push(ti);
                }
            }
            src_matches.push(hits);
        }
        let mut target_hit = vec![false; old_rows.len()];
        let mut touched: HashSet<usize> = HashSet::new();
        let mut updates: Vec<(RowId, Row)> = vec![];
        let mut deletes: Vec<RowId> = vec![];
        let mut inserts: Vec<Row> = vec![];
        let mut actions: Vec<(String, Row, Row)> = vec![];
        let mut upd_cols = vec![false; t.columns.len()];
        let ts_cols: Vec<usize> = t.columns.iter().enumerate().filter(|(_, c)| matches!(c.ty, DataType::Timestamp)).map(|(i, _)| i).collect();
        let empty_row: Row = vec![];
        for (si, sr) in src.rows.iter().enumerate() {
            let hits = &src_matches[si];
            if hits.is_empty() {
                for w in m.whens.iter().filter(|w| w.kind == MergeKind::NotMatched) {
                    let env = Env::new(&src.schema, sr, None);
                    if let Some(c) = &w.cond {
                        if !matches!(self.ev(c, &env)?, Value::Bit(true)) {
                            continue;
                        }
                    }
                    if let MergeAction::Insert { cols, values } = &w.action {
                        let col_idx: Vec<usize> = match cols {
                            Some(cs) => cs.iter().map(|c| t.col_index(c).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", c)))).collect::<Result<_>>()?,
                            None => (0..t.columns.len()).filter(|&i| t.columns[i].computed.is_none() && t.columns[i].identity.is_none() && !matches!(t.columns[i].ty, DataType::Timestamp)).collect(),
                        };
                        let vals: Vec<Option<Value>> = match values {
                            Some(vs) => {
                                let mut o = vec![];
                                for e in vs {
                                    if matches!(e, Expr::Default) {
                                        o.push(None);
                                    } else {
                                        o.push(Some(self.ev(e, &env)?));
                                    }
                                }
                                o
                            }
                            None => vec![],
                        };
                        let row = self.build_row(t, &col_idx, &vals, ident_on)?;
                        actions.push(("INSERT".into(), row.clone(), empty_row.clone()));
                        inserts.push(row);
                    }
                    break;
                }
                continue;
            }
            for &ti in hits {
                target_hit[ti] = true;
                let (rid, orow) = &old_rows[ti];
                let mut crow = orow.clone();
                crow.extend(sr.iter().cloned());
                let env = Env::new(&combined, &crow, None);
                for w in m.whens.iter().filter(|w| w.kind == MergeKind::Matched) {
                    if let Some(c) = &w.cond {
                        if !matches!(self.ev(c, &env)?, Value::Bit(true)) {
                            continue;
                        }
                    }
                    if !touched.insert(ti) {
                        return Err(SqlError::new(8672, "The MERGE statement attempted to UPDATE or DELETE the same row more than once. A target row matched more than one source row. A MERGE statement cannot UPDATE/DELETE the same row of the target table multiple times. Refine the ON clause to ensure a target row matches at most one source row, or use the GROUP BY clause to group the source rows."));
                    }
                    match &w.action {
                        MergeAction::Delete => {
                            deletes.push(*rid);
                            actions.push(("DELETE".into(), empty_row.clone(), orow.clone()));
                        }
                        MergeAction::Update(sets) => {
                            let mut new = orow.clone();
                            let mut vals = vec![];
                            for s in sets {
                                if let SetItem::Col { parts, op, e } = s {
                                    let ci = t.col_index(parts.last().unwrap()).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", parts.last().unwrap())))?;
                                    let mut v = self.ev(e, &env)?;
                                    if let Some(op) = op {
                                        v = arith(*op, &orow[ci], &v)?;
                                    }
                                    vals.push((ci, v));
                                }
                            }
                            for (ci, v) in vals {
                                upd_cols[ci] = true;
                                new[ci] = coerce_store(&v, &t.columns[ci].ty, &tname, &t.columns[ci].name)?;
                            }
                            for &c in &ts_cols {
                                new[c] = self.next_rowversion();
                            }
                            self.finish_row(t, &mut new, "UPDATE")?;
                            actions.push(("UPDATE".into(), new.clone(), orow.clone()));
                            updates.push((*rid, new));
                        }
                        _ => {}
                    }
                    break;
                }
            }
        }
        for ti in 0..old_rows.len() {
            if target_hit[ti] {
                continue;
            }
            let (rid, orow) = &old_rows[ti];
            let env = Env::new(&tschema, orow, None);
            for w in m.whens.iter().filter(|w| w.kind == MergeKind::NotMatchedBySource) {
                if let Some(c) = &w.cond {
                    if !matches!(self.ev(c, &env)?, Value::Bit(true)) {
                        continue;
                    }
                }
                match &w.action {
                    MergeAction::Delete => {
                        deletes.push(*rid);
                        actions.push(("DELETE".into(), empty_row.clone(), orow.clone()));
                    }
                    MergeAction::Update(sets) => {
                        let mut new = orow.clone();
                        for s in sets {
                            if let SetItem::Col { parts, op, e } = s {
                                let ci = t.col_index(parts.last().unwrap()).ok_or_else(|| SqlError::new(207, "Invalid column name."))?;
                                let mut v = self.ev(e, &env)?;
                                if let Some(op) = op {
                                    v = arith(*op, &orow[ci], &v)?;
                                }
                                upd_cols[ci] = true;
                                new[ci] = coerce_store(&v, &t.columns[ci].ty, &tname, &t.columns[ci].name)?;
                            }
                        }
                        self.finish_row(t, &mut new, "UPDATE")?;
                        actions.push(("UPDATE".into(), new.clone(), orow.clone()));
                        updates.push((*rid, new));
                    }
                    _ => {}
                }
                break;
            }
        }
        Ok((Changes { updates, deletes, inserts }, upd_cols, actions))
    }
}

/// A view / CTE definition that maps one-to-one onto a base table (single table, plain columns,
/// no DISTINCT / TOP / GROUP BY / joins). Returns (base table name, filter, exposed column names).
fn simple_updatable(q: &Query) -> Option<(Vec<String>, Option<Expr>, Vec<String>)> {
    if let QueryBody::Select(sel) = &q.body {
        if sel.distinct || sel.top.is_some() || sel.group.is_some() || sel.having.is_some() || sel.into.is_some() || sel.from.len() != 1 {
            return None;
        }
        let name = match &sel.from[0] {
            TableRef::Table { name, .. } => name.clone(),
            _ => return None,
        };
        let mut cols = vec![];
        for it in &sel.items {
            match it {
                SelectItem::Expr { e: Expr::Col(p), alias } => {
                    let n = p.last().unwrap();
                    if let Some(a) = alias {
                        if !a.eq_ignore_ascii_case(n) {
                            return None;
                        }
                    }
                    cols.push(n.clone());
                }
                SelectItem::Star(_) => cols.push("*".to_string()),
                _ => return None,
            }
        }
        return Some((name, sel.where_.clone(), cols));
    }
    None
}

impl<'a> Engine<'a> {
    /// If `name` is a CTE or view that maps onto a single base table, return its base table,
    /// the filter predicate and the exposed columns.
    fn updatable_source(&self, name: &[String], ctes: &[Cte]) -> Option<(Vec<String>, Option<Expr>, Vec<String>)> {
        if name.len() == 1 {
            if let Some(c) = ctes.iter().find(|c| c.name.eq_ignore_ascii_case(&name[0])) {
                return simple_updatable(&c.query);
            }
        }
        let loc = self.locate(name);
        if let Some(Object::View(v)) = self.object_ref(&loc) {
            if v.triggers.is_empty() {
                return simple_updatable(&v.query);
            }
        }
        None
    }

    fn rewrite_insert_target(&self, ins: &mut Insert) {
        if let Target::Table(name) = &ins.target {
            if let Some((base, _filter, cols)) = self.updatable_source(name, &ins.ctes) {
                ins.target = Target::Table(base);
                if ins.cols.is_none() && !cols.iter().any(|c| c == "*") {
                    ins.cols = Some(cols);
                }
            }
        }
    }
}

fn and_filter(w: &Option<Expr>, f: Option<Expr>) -> Option<Expr> {
    match (w.clone(), f) {
        (Some(a), Some(b)) => Some(Expr::And(Box::new(Expr::Paren(Box::new(a))), Box::new(Expr::Paren(Box::new(b))))),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

fn find_alias(from: &[TableRef], alias: &str) -> Option<Vec<String>> {
    fn walk(t: &TableRef, alias: &str) -> Option<Vec<String>> {
        match t {
            TableRef::Table { name, alias: a, .. } => {
                if a.as_deref().map(|x| x.eq_ignore_ascii_case(alias)).unwrap_or(false) {
                    Some(name.clone())
                } else {
                    None
                }
            }
            TableRef::Join { left, right, .. } => walk(left, alias).or_else(|| walk(right, alias)),
            _ => None,
        }
    }
    from.iter().find_map(|t| walk(t, alias))
}
