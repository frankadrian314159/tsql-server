//! `UPDATE t SET ... FROM other WHERE ...` and `DELETE t FROM t JOIN other ON ...`: a data change that joins another table. The shards each hold a
//! slice of the tables, so the control node does what a shard cannot:
//!
//! 1. it reads the join as a query, over the cluster (the target's key columns, and for an `UPDATE` the new value of every column the `SET`
//!    names, worked out by the engine for each joined row);
//! 2. it applies the changes to the target by key, in chunks, as ordinary data changes (`UPDATE ... SET c = CASE WHEN key = k1 THEN v1 ... END
//!    WHERE key = k1 OR ...`, `DELETE ... WHERE key = k1 OR ...`): they go to the shards that hold the rows, run the triggers and the checks of the
//!    table, and are the transaction's.
//!
//! The key is the table's primary key, else every column that is not computed. When several rows of the join match one row of the target, the
//! first is applied (SQL Server applies an arbitrary one). The rows the join returns are held by the control node: more than `LIMIT` are refused.
use super::*;

/// The most rows of a join one such statement changes.
const LIMIT: usize = 200_000;
/// The rows changed by one statement sent on.
const CHUNK: usize = 100;

fn table_label(name: &Name) -> String {
    name.last().cloned().unwrap_or_default()
}

/// The table a statement names as its target: the target itself, or (when the target is an alias of the `FROM`) the table with that alias.
fn real_target(target: &Name, from: &[TableRef]) -> Name {
    fn find(tr: &TableRef, alias: &str) -> Option<Name> {
        match tr {
            TableRef::Table { name, alias: Some(a), .. } if a.eq_ignore_ascii_case(alias) => Some(name.clone()),
            TableRef::Join { left, right, .. } => find(left, alias).or_else(|| find(right, alias)),
            _ => None,
        }
    }
    if target.len() == 1 {
        if let Some(n) = from.iter().find_map(|f| find(f, &target[0])) {
            return n;
        }
    }
    target.clone()
}

impl ClusterSession {
    /// The columns that identify a row of `t`: its primary key, else every column that is not computed.
    fn row_key(&self, t: &DistTable) -> Vec<String> {
        let schema = if t.name.len() >= 2 { t.name[t.name.len() - 2].clone() } else { "dbo".to_string() };
        let shown = format!("[{}].[{}]", schema, t.name.last().cloned().unwrap_or_default());
        let pk: Vec<String> = self.mirror.table_info(&shown).and_then(|i| i.indexes.into_iter().find(|i| i.primary_key)).map(|i| i.columns.into_iter().map(|(c, _)| c).collect()).unwrap_or_default();
        if !pk.is_empty() {
            return pk;
        }
        t.cols.iter().filter(|c| !c.computed && !c.hidden).map(|c| c.name.clone()).collect()
    }

    /// The rows of the join: the key of the target's row, then `extra` (what is wanted of the joined row).
    async fn join_rows(&mut self, target: &Name, label: &str, from: &[TableRef], where_: Option<&Expr>, key: &[String], extra: Vec<Expr>) -> Result<Vec<Vec<Value>>, SqlError> {
        let aliased = from.iter().any(|f| fn_has_alias(f, label));
        let mut refs: Vec<TableRef> = vec![];
        if !aliased {
            refs.push(TableRef::Table { name: target.clone(), alias: None, hints: vec![] });
        }
        refs.extend(from.iter().cloned());
        let mut items: Vec<SelectItem> = key.iter().map(|c| SelectItem::Expr { e: Expr::Col(vec![label.to_string(), c.clone()]), alias: None }).collect();
        items.extend(extra.into_iter().map(|e| SelectItem::Expr { e, alias: None }));
        let q = simple_query(Select { distinct: false, top: None, items, into: None, from: refs, where_: where_.cloned(), group: None, having: None });
        let rows = self.run_query(&q).await?.rows;
        if rows.len() > LIMIT {
            return Err(SqlError::new(50100, format!("This statement joins more than {} rows: the control node cannot apply it.", LIMIT)));
        }
        Ok(rows)
    }

    /// `UPDATE target SET ... FROM ... WHERE ...`.
    pub(crate) async fn update_from(&mut self, u: &Update, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let Target::Table(tname) = &u.target else { return Err(unsupported("UPDATE of a table variable with FROM")) };
        let real = real_target(tname, &u.from);
        let t = self.table(&real)?;
        let label = if real == *tname { table_label(tname) } else { tname[0].clone() };
        let key = self.row_key(&t);
        if key.is_empty() {
            return Err(unsupported("UPDATE ... FROM of a table with no columns to identify its rows"));
        }
        // the new value of each column the SET names, as the engine works it out for each joined row
        let mut set_cols: Vec<String> = vec![];
        let mut values: Vec<Expr> = vec![];
        for s in &u.sets {
            match s {
                SetItem::Col { parts, op, e } => {
                    let c = parts.last().cloned().unwrap_or_default();
                    let cur = Expr::Col(vec![label.clone(), c.clone()]);
                    values.push(match op {
                        Some(op) => Expr::Binary(*op, Box::new(cur), Box::new(e.clone())),
                        None => e.clone(),
                    });
                    set_cols.push(c);
                }
                SetItem::Var { .. } => return Err(unsupported("UPDATE ... FROM that assigns variables")),
            }
        }
        let rows = self.join_rows(&real, &label, &u.from, u.where_.as_ref(), &key, values).await?;
        // (one change for each row of the target: the first)
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut changes: Vec<(Vec<Value>, Vec<Value>)> = vec![];
        for r in rows {
            let (k, v) = r.split_at(key.len());
            let id = k.iter().map(|x| format!("{:?}", x)).collect::<Vec<_>>().join("\u{1}");
            if seen.insert(id) {
                changes.push((k.to_vec(), v.to_vec()));
            }
        }
        let mut total = 0i64;
        for chunk in changes.chunks(CHUNK) {
            let keys: Vec<Vec<Value>> = chunk.iter().map(|c| c.0.clone()).collect();
            let sets: Vec<SetItem> = set_cols
                .iter()
                .enumerate()
                .map(|(j, c)| {
                    let whens: Vec<(Expr, Expr)> = chunk.iter().map(|(k, v)| (Expr::Paren(Box::new(uniq::tuple_cond(&key, k))), nested::lit(&v[j]))).collect();
                    SetItem::Col { parts: vec![c.clone()], op: None, e: Expr::Case { operand: None, whens, els: Some(Box::new(col(c))) } }
                })
                .collect();
            let upd = Update { ctes: vec![], target: Target::Table(real.clone()), sets, output: None, from: vec![], where_: Some(uniq::any_tuple(&key, &keys)), current_of: None, top: None };
            let mut ev = vec![];
            Box::pin(self.update(&upd, &mut ev)).await?;
            total += ev.iter().filter_map(|e| if let Event::RowCount(n) = e { Some(*n) } else { None }).sum::<i64>();
        }
        self.plan.push(format!("UPDATE ... FROM: the join read by the control node ({} rows), applied to {} by key", changes.len(), real.join(".")));
        events.push(Event::RowCount(total));
        Ok(())
    }

    /// `DELETE target FROM ... WHERE ...`.
    pub(crate) async fn delete_from(&mut self, d: &Delete, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let Target::Table(tname) = &d.target else { return Err(unsupported("DELETE from a table variable with FROM")) };
        let real = real_target(tname, &d.from);
        let t = self.table(&real)?;
        let label = if real == *tname { table_label(tname) } else { tname[0].clone() };
        let key = self.row_key(&t);
        if key.is_empty() {
            return Err(unsupported("DELETE ... FROM of a table with no columns to identify its rows"));
        }
        let rows = self.join_rows(&real, &label, &d.from, d.where_.as_ref(), &key, vec![]).await?;
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut keys: Vec<Vec<Value>> = vec![];
        for r in rows {
            let id = r.iter().map(|x| format!("{:?}", x)).collect::<Vec<_>>().join("\u{1}");
            if seen.insert(id) {
                keys.push(r);
            }
        }
        let mut total = 0i64;
        for chunk in keys.chunks(CHUNK) {
            let del = Delete { ctes: vec![], target: Target::Table(real.clone()), output: None, from: vec![], where_: Some(uniq::any_tuple(&key, chunk)), current_of: None, top: None };
            let mut ev = vec![];
            Box::pin(self.delete(&del, &mut ev)).await?;
            total += ev.iter().filter_map(|e| if let Event::RowCount(n) = e { Some(*n) } else { None }).sum::<i64>();
        }
        self.plan.push(format!("DELETE ... FROM: the join read by the control node ({} rows), applied to {} by key", keys.len(), real.join(".")));
        events.push(Event::RowCount(total));
        Ok(())
    }
}

/// Whether the `FROM` already names the target, by its alias or (with none) by its own name.
fn fn_has_alias(tr: &TableRef, label: &str) -> bool {
    match tr {
        TableRef::Table { alias: Some(a), .. } => a.eq_ignore_ascii_case(label),
        TableRef::Table { alias: None, name, .. } => name.last().is_some_and(|n| n.eq_ignore_ascii_case(label)),
        TableRef::Join { left, right, .. } => fn_has_alias(left, label) || fn_has_alias(right, label),
        _ => false,
    }
}
