//! INSTEAD OF triggers on views.
//!
//! An `INSERT`, `UPDATE` or `DELETE` whose target is a view with an INSTEAD OF trigger for that statement changes nothing itself:
//! it works out the rows the statement would have written to the view and runs the trigger with them as `inserted` and
//! `deleted`, shaped as the view's rows (its columns, with the types the view gives them). So a view that cannot be written
//! through (a join, an aggregate, a `UNION`) can be, by a trigger that writes its base tables.
//!
//! * `INSERT`: `inserted` holds the rows of the source, matched to the view's columns by the column list; the columns the list
//!   leaves out, and `DEFAULT`, are NULL (a view has no defaults). `deleted` is empty.
//! * `DELETE`: `deleted` holds the rows of the view that the `WHERE` (and the `FROM`, when there is one) select, each once.
//! * `UPDATE`: `deleted` holds those rows as they are, `inserted` the same rows with the `SET` applied; `UPDATE(col)` is true
//!   for the columns the `SET` names.
//!
//! The rows of the view are read with the rights of its owner, as a read through the view is, so the statement needs only the
//! permission for itself (`INSERT`, `UPDATE`, `DELETE`) on the view. An `OUTPUT` clause returns the rows as if the statement had
//! been carried out (as in SQL Server). A statement inside the trigger on its own view is not sent to the trigger again: it goes
//! to the view's base table when the view can be written through, and is an error otherwise.
//!
//! The rows an `UPDATE` or `DELETE` reaches are found by reading the view, numbering its rows, and running the statement's `FROM` /
//! `WHERE` over those numbered rows (as a `VALUES` list where the statement names the view), so a row joined to several rows of
//! the `FROM` is still taken once. `WHERE CURRENT OF` and `SET @variable = ...` are not handled here (the statement then runs
//! as if the view had no trigger).
use super::query::Rowset;
use super::*;
use crate::catalog::{Column, Table, Trigger, ViewDef};
use crate::rewrite::astutil::simple_query;

/// The column a view's rows are numbered by in the `VALUES` list that stands for the view.
const ROWNUM: &str = "__view_row";

impl<'a> Engine<'a> {
    /// The view a DML target names and its INSTEAD OF trigger for `event`, when it has one that may fire now. A CTE of that
    /// name hides the view; a one-part target that is an alias of the `FROM` names what the alias stands for. Returns the view's
    /// location and name, the view, the trigger, and the alias the statement knows the view by.
    #[allow(clippy::type_complexity)]
    fn view_instead(&self, target: &Target, ctes: &[Cte], from: &[TableRef], event: &str) -> Option<(Loc, Vec<String>, ViewDef, Arc<Trigger>, String)> {
        let Target::Table(written) = target else { return None };
        let mut name = written.clone();
        if name.len() == 1 {
            let l = name[0].to_lowercase();
            if ctes.iter().any(|c| c.name.eq_ignore_ascii_case(&l)) || self.ses.cte_stack.iter().any(|m| m.contains_key(&l)) {
                return None;
            }
            if let Some(real) = super::dml::find_alias(from, &name[0]) {
                name = real;
            }
        }
        let loc = self.locate(&name);
        let Some(Object::View(v)) = self.object_ref(&loc) else { return None };
        let trg = v.triggers.iter().find(|g| !g.disabled && g.instead && g.events.iter().any(|e| e == event)).cloned()?;
        // (while this trigger runs, a statement on the view itself is the view's own)
        let key = obj_key(&v.schema, &v.name);
        if self.ses.trigger_stack.iter().any(|c| c.instead_of && c.event == event && c.table.key() == key) {
            return None;
        }
        let alias = written.last().cloned().unwrap_or_default();
        Some((loc, name, v.clone(), trg, alias))
    }

    /// The rows of view `v` (`top0`: none, only its columns), read with the rights of the view's owner.
    fn view_rowset(&mut self, loc: &Loc, v: &ViewDef, top0: bool) -> Result<Rowset> {
        let owner = match loc {
            Loc::Db(d, _) => self.schema_owner_of(d, &v.schema),
            _ => String::new(),
        };
        let top = top0.then(|| Top { e: Expr::Lit(Value::Int(0)), percent: false, ties: false });
        let q = simple_query(Select {
            distinct: false,
            top,
            items: vec![SelectItem::Star(None)],
            into: None,
            from: vec![TableRef::Derived { query: Box::new((*v.query).clone()), alias: Some(v.name.clone()), cols: v.cols.clone() }],
            where_: None,
            group: None,
            having: None,
        });
        let saved = std::mem::take(&mut self.ses.cte_stack);
        self.ses.module_owner.push(owner);
        let r = self.exec_query(&q, None);
        self.ses.module_owner.pop();
        self.ses.cte_stack = saved;
        r
    }

    /// The table the trigger sees as `inserted` and `deleted`: the view's columns, with no storage.
    fn view_table(v: &ViewDef, rs: &Rowset) -> Table {
        let columns = rs
            .schema
            .cols
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                // (a column that is only NULL has no type of its own; SQL Server makes it an int)
                ty: c.ty.clone().unwrap_or(DataType::Int),
                nullable: true,
                identity: None,
                default: None,
                default_text: None,
                default_name: None,
                computed: None,
                computed_text: None,
                user_type: None,
                xml_schema: None,
                collation: c.coll.as_ref().map(|k| k.name.clone()),
                mask: None,
                hidden: false,
            })
            .collect();
        let mut t = Table::detached(&v.db, &v.schema, &v.name, columns);
        t.object_id = v.object_id;
        t
    }

    // ------------------------------------------------------------ INSERT
    /// An `INSERT` into a view with an INSTEAD OF INSERT trigger. False when the target is not such a view.
    pub(super) fn insert_view_instead(&mut self, ins: &Insert) -> Result<bool> {
        let Some((loc, name, v, trg, _)) = self.view_instead(&ins.target, &ins.ctes, &[], "INSERT") else { return Ok(false) };
        self.check_dml_permission(&loc, &name, "INSERT")?;
        let shape = self.view_rowset(&loc, &v, true)?;
        let t = Self::view_table(&v, &shape);
        let ncols = t.columns.len();
        let idx: Vec<usize> = match &ins.cols {
            Some(cs) => {
                let mut idx = vec![];
                for c in cs {
                    let i = t.col_index(c).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", c)))?;
                    if idx.contains(&i) {
                        return Err(SqlError::new(264, format!("The column name '{}' is specified more than once in the SET clause or column list of an INSERT. A column cannot be assigned more than one value in the same clause. Modify the clause to make sure that a column is updated only once. If this statement updates or inserts columns into a view, column aliasing can conceal the duplication in your code.", c)));
                    }
                    idx.push(i);
                }
                idx
            }
            None => (0..ncols).collect(),
        };
        let (src_rows, src_ncols) = self.with_ctes(&ins.ctes, |s| s.insert_source(ins))?;
        let default_values = matches!(ins.src, InsertSrc::DefaultValues);
        if !default_values && !src_rows.is_empty() && src_ncols != idx.len() {
            let listed = ins.cols.is_some();
            return Err(match (listed, src_ncols > idx.len()) {
                (true, true) => SqlError::new(110, "There are fewer columns in the INSERT statement than values specified in the VALUES clause. The number of values in the VALUES clause must match the number of columns specified in the INSERT statement."),
                (true, false) => SqlError::new(109, "There are more columns in the INSERT statement than values specified in the VALUES clause. The number of values in the VALUES clause must match the number of columns specified in the INSERT statement."),
                _ => SqlError::new(213, "Column name or number of supplied values does not match table definition."),
            });
        }
        let mut inserted: Vec<Row> = Vec::with_capacity(src_rows.len());
        for sr in src_rows {
            let mut row: Row = vec![Value::Null; ncols];
            for (k, val) in sr.into_iter().enumerate() {
                if let (Some(&ci), Some(val)) = (idx.get(k), val) {
                    row[ci] = crate::convert::cast(&val, &t.columns[ci].ty, None)?;
                }
            }
            inserted.push(row);
        }
        if let Some(o) = &ins.output {
            self.emit_output(o, &t, &inserted, &[], None)?;
        }
        self.run_trigger(&trg, &t, "INSERT", inserted, vec![], &vec![true; ncols])?;
        self.ses.rowcount = 0;
        Ok(true)
    }

    // ------------------------------------------------------------ UPDATE
    /// An `UPDATE` of a view with an INSTEAD OF UPDATE trigger. False when the target is not such a view (or the statement is one
    /// this does not handle, see the module comment).
    pub(super) fn update_view_instead(&mut self, u: &Update) -> Result<bool> {
        if u.current_of.is_some() || u.sets.iter().any(|s| matches!(s, SetItem::Var { .. })) {
            return Ok(false);
        }
        let Some((loc, name, v, trg, alias)) = self.view_instead(&u.target, &u.ctes, &u.from, "UPDATE") else { return Ok(false) };
        self.check_dml_permission(&loc, &name, "UPDATE")?;
        self.with_ctes(&u.ctes, |s| s.update_view_inner(u, &loc, &v, &trg, &alias))?;
        Ok(true)
    }

    fn update_view_inner(&mut self, u: &Update, loc: &Loc, v: &ViewDef, trg: &Arc<Trigger>, alias: &str) -> Result<()> {
        let rs = self.view_rowset(loc, v, false)?;
        let t = Self::view_table(v, &rs);
        let rows: Vec<Row> = rs.rows.iter().cloned().collect();
        let ncols = t.columns.len();
        let mut upd = vec![false; ncols];
        let mut sets: Vec<(usize, Expr)> = vec![];
        for s in &u.sets {
            let SetItem::Col { parts, op, e } = s else { continue };
            let c = parts.last().map(|s| s.as_str()).unwrap_or("");
            let ci = t.col_index(c).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", c)))?;
            if upd[ci] {
                return Err(SqlError::new(264, format!("The column name '{}' is specified more than once in the SET clause or column list of an INSERT. A column cannot be assigned more than one value in the same clause. Modify the clause to make sure that a column is updated only once. If this statement updates or inserts columns into a view, column aliasing can conceal the duplication in your code.", t.columns[ci].name)));
            }
            upd[ci] = true;
            let value = match op {
                // `SET c += e` is `c = c + e`, with `c` the view's value
                Some(o) => Expr::Binary(*o, Box::new(Expr::Col(vec![alias.to_string(), t.columns[ci].name.clone()])), Box::new(e.clone())),
                None => e.clone(),
            };
            sets.push((ci, value));
        }
        let exprs: Vec<Expr> = sets.iter().map(|(_, e)| e.clone()).collect();
        let mut matched = self.view_matches(loc, &t, &rows, alias, &u.from, &u.where_, &exprs)?;
        self.view_limit(&u.top, &mut matched)?;
        let mut deleted = Vec::with_capacity(matched.len());
        let mut inserted = Vec::with_capacity(matched.len());
        for (i, vals) in matched {
            let old = rows[i].clone();
            let mut new = old.clone();
            for ((ci, _), val) in sets.iter().zip(vals) {
                new[*ci] = crate::convert::cast(&val, &t.columns[*ci].ty, None)?;
            }
            deleted.push(old);
            inserted.push(new);
        }
        if let Some(o) = &u.output {
            self.emit_output(o, &t, &inserted, &deleted, None)?;
        }
        self.run_trigger(trg, &t, "UPDATE", inserted, deleted, &upd)?;
        self.ses.rowcount = 0;
        Ok(())
    }

    // ------------------------------------------------------------ DELETE
    /// A `DELETE` from a view with an INSTEAD OF DELETE trigger. False when the target is not such a view.
    pub(super) fn delete_view_instead(&mut self, d: &Delete) -> Result<bool> {
        if d.current_of.is_some() {
            return Ok(false);
        }
        let Some((loc, name, v, trg, alias)) = self.view_instead(&d.target, &d.ctes, &d.from, "DELETE") else { return Ok(false) };
        self.check_dml_permission(&loc, &name, "DELETE")?;
        self.with_ctes(&d.ctes, |s| {
            let rs = s.view_rowset(&loc, &v, false)?;
            let t = Self::view_table(&v, &rs);
            let rows: Vec<Row> = rs.rows.iter().cloned().collect();
            let mut matched = s.view_matches(&loc, &t, &rows, &alias, &d.from, &d.where_, &[])?;
            s.view_limit(&d.top, &mut matched)?;
            let deleted: Vec<Row> = matched.into_iter().map(|(i, _)| rows[i].clone()).collect();
            if let Some(o) = &d.output {
                s.emit_output(o, &t, &[], &deleted, None)?;
            }
            let ncols = t.columns.len();
            s.run_trigger(&trg, &t, "DELETE", vec![], deleted, &vec![true; ncols])?;
            s.ses.rowcount = 0;
            Ok(())
        })?;
        Ok(true)
    }

    /// `TOP` and `SET ROWCOUNT` on the rows an UPDATE or DELETE reaches.
    fn view_limit(&mut self, top: &Option<(Expr, bool)>, matched: &mut Vec<(usize, Vec<Value>)>) -> Result<()> {
        if let Some((e, pct)) = top {
            let v = self.ev(e, &super::query::Env::empty())?;
            let n = if *pct {
                let p = crate::convert::num_to_f64(crate::convert::as_num(&v).unwrap_or(crate::convert::Num::I(0)));
                (matched.len() as f64 * p / 100.0).ceil() as usize
            } else {
                v.as_i64().unwrap_or(0).max(0) as usize
            };
            matched.truncate(n);
        }
        if self.ses.rowcount_limit > 0 {
            matched.truncate(self.ses.rowcount_limit as usize);
        }
        Ok(())
    }

    /// The rows of the view (`rows`, numbered by position) that an UPDATE or DELETE with this `FROM` and `WHERE` reaches, each once,
    /// in the order found, with the values of `exprs` (the `SET` expressions) for each.
    #[allow(clippy::too_many_arguments)]
    fn view_matches(&mut self, loc: &Loc, t: &Table, rows: &[Row], alias: &str, from: &[TableRef], where_: &Option<Expr>, exprs: &[Expr]) -> Result<Vec<(usize, Vec<Value>)>> {
        if rows.is_empty() {
            return Ok(vec![]);
        }
        let mut cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
        cols.push(ROWNUM.to_string());
        let values = |alias: &str| TableRef::Values {
            rows: rows.iter().enumerate().map(|(i, r)| r.iter().cloned().map(Expr::Lit).chain(std::iter::once(Expr::Lit(Value::BigInt(i as i64)))).collect()).collect(),
            alias: Some(alias.to_string()),
            cols: Some(cols.clone()),
        };
        // the view stands where the statement names it; with no FROM, alone
        let mut from = from.to_vec();
        let mut vref = alias.to_string();
        let placed = self.place_view(&from, loc, alias, &mut vref);
        match placed {
            Some((i, path)) => {
                let replaced = replace_at(&from[i], &path, &values(&vref));
                from[i] = replaced;
            }
            None => from.push(values(&vref)),
        }
        let mut items = vec![SelectItem::Expr { e: Expr::Col(vec![vref.clone(), ROWNUM.to_string()]), alias: None }];
        items.extend(exprs.iter().map(|e| SelectItem::Expr { e: e.clone(), alias: None }));
        let q = simple_query(Select { distinct: false, top: None, items, into: None, from, where_: where_.clone(), group: None, having: None });
        let rs = self.exec_query(&q, None)?;
        let mut seen = vec![false; rows.len()];
        let mut out = vec![];
        for r in rs.rows.iter() {
            let Some(i) = r.first().and_then(|v| v.as_i64()).map(|i| i as usize) else { continue };
            if i < rows.len() && !seen[i] {
                seen[i] = true;
                out.push((i, r[1..].to_vec()));
            }
        }
        Ok(out)
    }

    /// Where the view is named in a `FROM`: the index of the top-level item and the path to the reference inside it (left / right
    /// of each join). `vref` is set to the name the statement knows the view by there.
    fn place_view(&self, from: &[TableRef], loc: &Loc, alias: &str, vref: &mut String) -> Option<(usize, Vec<bool>)> {
        let want = format!("{:?}", loc);
        fn find(e: &Engine<'_>, t: &TableRef, want: &str, alias: &str, path: &mut Vec<bool>, vref: &mut String) -> bool {
            match t {
                TableRef::Table { name, alias: a, .. } => {
                    let by_alias = a.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(alias));
                    if by_alias || (a.is_none() && format!("{:?}", e.locate(name)) == want) {
                        *vref = a.clone().unwrap_or_else(|| name.last().cloned().unwrap_or_default());
                        return true;
                    }
                    false
                }
                TableRef::Join { left, right, .. } => {
                    path.push(false);
                    if find(e, left, want, alias, path, vref) {
                        return true;
                    }
                    path.pop();
                    path.push(true);
                    if find(e, right, want, alias, path, vref) {
                        return true;
                    }
                    path.pop();
                    false
                }
                _ => false,
            }
        }
        for (i, t) in from.iter().enumerate() {
            let mut path = vec![];
            if find(self, t, &want, alias, &mut path, vref) {
                return Some((i, path));
            }
        }
        None
    }
}

impl<'a> Engine<'a> {
    // ------------------------------------------------------------ DDL
    /// `CREATE [OR ALTER] TRIGGER .. ON view INSTEAD OF ..`: the trigger is kept with the view. False when `tloc` is not a view.
    /// A view takes INSTEAD OF triggers only (an AFTER trigger is error 8197, as in SQL Server), and one per statement (2111).
    pub(super) fn create_view_trigger(&mut self, tloc: &Loc, table: &[String], trg: Arc<Trigger>, or_alter: bool) -> Result<bool> {
        let Loc::Db(vdb, vkey) = tloc else { return Ok(false) };
        let Some(Object::View(v)) = self.object_ref(tloc) else { return Ok(false) };
        let v = v.clone();
        let display = table.join(".");
        if !trg.instead {
            return Err(SqlError::new(8197, format!("The object '{}' does not exist or is invalid for this operation.", display)));
        }
        self.check_alter(tloc, table.last().map(|s| s.as_str()).unwrap_or(""))?;
        if !or_alter && v.triggers.iter().any(|x| x.name.eq_ignore_ascii_case(&trg.name)) {
            return Err(self.object_exists_err(&trg.name));
        }
        for ev in &trg.events {
            if v.triggers.iter().any(|x| !x.name.eq_ignore_ascii_case(&trg.name) && x.instead && x.events.contains(ev)) {
                return Err(SqlError::new(2111, format!("Cannot CREATE trigger '{}' on view '{}' because an INSTEAD OF {} trigger already exists on this object.", trg.name, display, ev)));
            }
        }
        self.ensure_tx()?;
        self.undo_object(vdb, vkey);
        if let Some(Object::View(v)) = self.srv.cat_mut().databases.get_mut(vdb).and_then(|d| d.objects.get_mut(vkey)) {
            v.triggers.retain(|x| !x.name.eq_ignore_ascii_case(&trg.name));
            v.triggers.push(trg);
        }
        Ok(true)
    }

    /// `ENABLE | DISABLE TRIGGER names | ALL ON view`.
    pub(super) fn set_view_triggers_enabled(&mut self, tloc: &Loc, table: &[String], names: &[String], all: bool, enable: bool) -> Result<()> {
        let Loc::Db(vdb, vkey) = tloc else { return Ok(()) };
        let Some(Object::View(v)) = self.object_ref(tloc) else { return Ok(()) };
        let v = v.clone();
        self.check_alter(tloc, table.last().map(|s| s.as_str()).unwrap_or(""))?;
        if !all {
            for n in names {
                if !v.triggers.iter().any(|g| g.name.eq_ignore_ascii_case(n)) {
                    return Err(SqlError::new(4947, format!("Cannot find the trigger '{}', because it does not exist or you do not have permission.", n)));
                }
            }
        }
        self.ensure_tx()?;
        self.undo_object(vdb, vkey);
        let mut changed: Vec<Arc<Trigger>> = vec![];
        let Some(d) = self.srv.cat_mut().databases.get_mut(vdb) else { return Ok(()) };
        if let Some(Object::View(v)) = d.objects.get_mut(vkey) {
            for g in v.triggers.iter_mut() {
                if all || names.iter().any(|n| g.name.eq_ignore_ascii_case(n)) {
                    let mut ng = (**g).clone();
                    ng.disabled = !enable;
                    *g = Arc::new(ng);
                    changed.push(g.clone());
                }
            }
        }
        // (the database's list of triggers, which sys.triggers shows, holds the same triggers)
        for (_, (k, g)) in d.triggers.iter_mut() {
            if k == vkey {
                if let Some(c) = changed.iter().find(|c| c.name.eq_ignore_ascii_case(&g.name)) {
                    *g = c.clone();
                }
            }
        }
        Ok(())
    }
}

/// `t` with the table reference at `path` (left / right of each join, from the top) replaced by `with`.
fn replace_at(t: &TableRef, path: &[bool], with: &TableRef) -> TableRef {
    match (t, path.split_first()) {
        (_, None) => with.clone(),
        (TableRef::Join { kind, left, right, on }, Some((&go_right, rest))) => {
            if go_right {
                TableRef::Join { kind: *kind, left: left.clone(), right: Box::new(replace_at(right, rest, with)), on: on.clone() }
            } else {
                TableRef::Join { kind: *kind, left: Box::new(replace_at(left, rest, with)), right: right.clone(), on: on.clone() }
            }
        }
        _ => t.clone(),
    }
}
