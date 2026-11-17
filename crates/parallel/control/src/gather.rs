//! The gather: running a query on the control node's own engine over copies of the tables it reads.
//!
//! It is the baseline for what the shards cannot run (always exact), so it is worth making cheap:
//!
//! * a **small table** (at most the broadcast limit in rows) is copied whole and kept until a change bumps its version, as before;
//! * a **big table** is copied *for this query only*, with just what the query needs: the columns it mentions (anywhere in it,
//!   subqueries included; a `*` makes all of them needed) and the rows that pass the conditions of its `WHERE` that mention that
//!   table alone (when the query is a plain join with no outer join, and the condition is one a missing row cannot satisfy:
//!   comparisons, `BETWEEN`, `IN` lists and `LIKE` combined by `AND`). The condition stays in the query; applying it to the copy
//!   as well only keeps rows that cannot matter from travelling. Every shard answers its part at the same time.
//!
//! Where the work is spread over the shards instead (staged joins, `mpp.rs`) nothing is gathered at all.
use crate::astutil::*;
use crate::joins::{build, conjuncts, resolve};
use crate::*;
use serde_json::Value as J;
use std::collections::{HashMap, HashSet};

/// The lower-case names of the columns a query mentions, or None when it has a `*`.
fn used_columns(v: &J, out: &mut HashSet<String>) -> bool {
    match v {
        J::Object(m) => {
            if m.contains_key("Star") || (m.len() == 1 && m.contains_key("Star")) {
                return false;
            }
            for (k, c) in m {
                if k == "Col" {
                    if let Some(last) = c.as_array().and_then(|a| a.last()).and_then(|x| x.as_str()) {
                        out.insert(last.to_lowercase());
                    }
                } else if !used_columns(c, out) {
                    return false;
                }
            }
            true
        }
        J::Array(a) => a.iter().all(|c| used_columns(c, out)),
        _ => true,
    }
}

/// Point every reference to the table with this id at `to` (keeping the name it was known by as its alias).
fn rename_by_id(v: &mut J, id: &str, to: &str) {
    match v {
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(J::Object(t)) = m.get_mut("Table") {
                    let parts: Vec<String> = t.get("name").and_then(|n| n.as_array()).map(|a| a.iter().filter_map(|p| p.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
                    if table_id(&parts) == id {
                        let last = parts.last().cloned().unwrap_or_default();
                        t.insert("name".into(), J::Array(vec![J::String(to.to_string())]));
                        if t.get("alias").map(|a| a.is_null()).unwrap_or(true) {
                            t.insert("alias".into(), J::String(last));
                        }
                        return;
                    }
                }
            }
            for c in m.values_mut() {
                rename_by_id(c, id, to);
            }
        }
        J::Array(a) => {
            for c in a {
                rename_by_id(c, id, to);
            }
        }
        _ => {}
    }
}

/// The names of the columns the query compares with another column for equality (`a.x = b.y`): the ones worth an index on a copy.
fn equated_columns(v: &J, out: &mut HashSet<String>) {
    match v {
        J::Object(m) => {
            if let Some(J::Array(a)) = m.get("Binary") {
                if a.len() == 3 && a[0].as_str() == Some("Eq") {
                    let name = |e: &J| e.get("Col").and_then(|c| c.as_array()).and_then(|c| c.last()).and_then(|n| n.as_str()).map(|n| n.to_lowercase());
                    if let (Some(x), Some(y)) = (name(&a[1]), name(&a[2])) {
                        out.insert(x);
                        out.insert(y);
                    }
                }
            }
            for c in m.values() {
                equated_columns(c, out);
            }
        }
        J::Array(a) => {
            for c in a {
                equated_columns(c, out);
            }
        }
        _ => {}
    }
}

/// A condition that a row which is not there cannot satisfy, so that dropping the rows that fail it early changes nothing.
pub(crate) fn pushable(e: &Expr) -> bool {
    let mut ok = true;
    walk_expr(e, &mut |x| match x {
        Expr::Col(_) | Expr::Lit(_) | Expr::Binary(..) | Expr::And(..) | Expr::Between(..) | Expr::InList(..) | Expr::Like(..) | Expr::Neg(_) | Expr::Paren(_) => {}
        _ => ok = false,
    });
    ok
}

impl ClusterSession {
    /// Copy the tables to the control node's engine (whole and kept, or pruned for this query) and run the query there.
    pub(crate) async fn gather(&mut self, q: &Query, tables: &[DistTable]) -> Result<ResultSet, SqlError> {
        let cl = self.cluster.clone();
        let _guard = cl.gather.lock().unwrap_or_else(|e| e.into_inner());
        let small = self.cluster.broadcast_limit.load(Ordering::Relaxed);
        let mut qj = serde_json::to_value(q).map_err(|e| SqlError::new(50000, format!("the query could not be rewritten for the cluster: {}", e)))?;
        let mut used = HashSet::new();
        let star = !used_columns(&qj, &mut used);
        let mut eq_cols = HashSet::new();
        equated_columns(&qj, &mut eq_cols);
        for t in tables.iter().filter(|t| matches!(t.dist, Dist::Sharded { .. })) {
            let mine: Vec<String> = t.cols.iter().filter(|c| eq_cols.contains(&c.name.to_lowercase())).map(|c| c.name.clone()).collect();
            let rows = self.table_by_id(&t.id).and_then(|x| x.rows.map(|r| r.1)).unwrap_or(0);
            cl.advisor.record(&t.id, "gather", rows, &mine);
        }
        // the tables as a plain join, for the conditions that can be applied while copying (None: not a plain join)
        let map: HashMap<String, DistTable> = tables.iter().map(|t| (t.id.clone(), t.clone())).collect();
        let join = match &q.body {
            QueryBody::Select(sel) if !has_nested_query(q) => {
                let mut leaves = vec![];
                let mut on = vec![];
                let mut plain = true;
                for tr in &sel.from {
                    match build(tr, &map, &mut leaves, &mut on) {
                        Some(node) => {
                            fn inner_only(n: &crate::joins::Node) -> bool {
                                match n {
                                    crate::joins::Node::Leaf(_) => true,
                                    crate::joins::Node::Join(k, l, r) => matches!(k, JoinKind::Inner | JoinKind::Cross) && inner_only(l) && inner_only(r),
                                }
                            }
                            plain &= inner_only(&node);
                        }
                        None => plain = false,
                    }
                }
                if plain {
                    Some((leaves, sel.where_.clone()))
                } else {
                    None
                }
            }
            _ => None,
        };
        let t_start = std::time::Instant::now();
        let mut made: Vec<String> = vec![];
        let mut copies: Vec<String> = vec![];
        let result: Result<ResultSet, SqlError> = async {
            for t in tables {
                let current = self.table_by_id(&t.id).ok_or_else(|| SqlError::new(208, format!("Invalid object name '{}'.", t.name.join("."))))?;
                let cached = current.loaded == Some(current.version);
                let big = !cached && matches!(t.dist, Dist::Sharded { .. }) && self.table_rows(t).await? > small;
                if !big || star {
                    self.refresh_mirror(t).await?;
                    copies.push(format!("{} (whole{})", t.id, if cached { ", cached" } else { "" }));
                    continue;
                }
                // the columns the query mentions (a computed column, which the copy cannot hold, needs the whole table)
                let cols: Vec<&ColInfo> = t.cols.iter().filter(|c| used.contains(&c.name.to_lowercase())).collect();
                if cols.is_empty() || cols.iter().any(|c| c.computed) {
                    self.refresh_mirror(t).await?;
                    copies.push(format!("{} (whole)", t.id));
                    continue;
                }
                // the conditions of the WHERE that mention only this table, when it is read once
                let filter = join.as_ref().and_then(|(leaves, where_)| {
                    let mine: Vec<usize> = (0..leaves.len()).filter(|i| leaves[*i].table.id == t.id).collect();
                    let [l] = mine.as_slice() else { return None };
                    let mut fs: Vec<Expr> = vec![];
                    let mut cs = vec![];
                    conjuncts(where_.as_ref()?, &mut cs);
                    for c in cs {
                        let mut only = true;
                        let mut any = false;
                        walk_expr(c, &mut |x| {
                            if let Expr::Col(p) = x {
                                match resolve(leaves, p) {
                                    Some((li, _)) if li == *l => any = true,
                                    _ => only = false,
                                }
                            }
                        });
                        if only && any && pushable(c) {
                            fs.push(c.clone());
                        }
                    }
                    let alias = leaves[*l].alias.clone();
                    fs.into_iter().reduce(|a, b| Expr::And(Box::new(a), Box::new(b))).map(|f| (f, alias))
                });
                self.temp_counter += 1;
                // (a table of the control node's engine, not a #temporary one: the engine does not use the indexes of a temporary table to seek into
                // it from a correlated subquery, and the copy's index is what keeps such a query from scanning it once per row)
                let temp = format!("dpg_{}_{}", self.id, self.temp_counter);
                let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
                // the copy has the columns (and the types) of the table's own, in the control node's engine
                let create = simple_query(Select {
                    distinct: false,
                    top: None,
                    items: names.iter().map(|c| SelectItem::Expr { e: col(c), alias: None }).collect(),
                    into: Some(vec![temp.clone()]),
                    from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }],
                    where_: Some(Expr::Binary(BinOp::Eq, Box::new(Expr::Lit(Value::Int(1))), Box::new(Expr::Lit(Value::Int(0))))),
                    group: None,
                    having: None,
                });
                merge::check(&self.mirror.execute_stmts(&[Stmt::Query(Box::new(create))]))?;
                made.push(temp.clone());
                let scan = simple_query(Select {
                    distinct: false,
                    top: None,
                    items: names.iter().map(|c| SelectItem::Expr { e: col(c), alias: None }).collect(),
                    into: None,
                    from: vec![TableRef::Table { name: t.name.clone(), alias: filter.as_ref().map(|f| f.1.clone()), hints: vec![] }],
                    where_: filter.as_ref().map(|f| f.0.clone()),
                    group: None,
                    having: None,
                });
                let shards = self.all_shards();
                let t_pull = std::time::Instant::now();
                let results = self.fan_query(&shards, |_| Work::Stmts(vec![Stmt::Query(Box::new(scan.clone()))])).await?;
                let pull = t_pull.elapsed().as_secs_f64();
                let mut n = 0usize;
                let (mut convert, mut insert) = (0.0f64, 0.0f64);
                for rs in results {
                    n += rs.rows.len();
                    // (the control node's engine takes the rows directly, as committed rows: see `Connection::load_rows`)
                    let mut it = rs.rows.into_iter();
                    loop {
                        let chunk: Vec<Vec<Value>> = it.by_ref().take(20_000).collect();
                        if chunk.is_empty() {
                            break;
                        }
                        let t1 = std::time::Instant::now();
                        self.mirror.load_rows(std::slice::from_ref(&temp), None, chunk)?;
                        insert += t1.elapsed().as_secs_f64();
                    }
                }
                // the copy is a plain heap: an index on the columns the query equates, so that a join or a correlated lookup on the control
                // node seeks where it would otherwise scan the copy once per row
                if n > 1000 {
                    let keys: Vec<(String, bool)> = names.iter().filter(|c| eq_cols.contains(&c.to_lowercase())).map(|c| (c.clone(), false)).collect();
                    if !keys.is_empty() {
                        let ix = Stmt::CreateIndex { name: format!("ix_{}", temp.trim_start_matches('#')), table: vec![temp.clone()], cols: keys, unique: false, include: vec![], filter: None, xml: None, spatial: None };
                        merge::check(&self.mirror.execute_stmts(&[ix]))?;
                    }
                }
                let _ = &mut convert;
                if std::env::var("TSQL_GATHER_TRACE").is_ok() {
                    eprintln!("gather {}: {} rows; pulling from the shards {:.2}s, making statements {:.2}s, inserting {:.2}s", t.id, n, pull, convert, insert);
                }
                rename_by_id(&mut qj, &t.id, &temp);
                copies.push(format!("{} ({} of {} columns{}, {} rows)", t.id, names.len(), t.cols.len(), if filter.is_some() { ", filtered" } else { "" }, n));
            }
            let q2: Query = serde_json::from_value(qj.clone()).map_err(|e| SqlError::new(50000, format!("the query could not be rewritten for the cluster: {}", e)))?;
            let copy_secs = t_start.elapsed().as_secs_f64();
            let r = self.mirror_query(&q2);
            self.plan.push(format!("gather {} into the control node, run there (copying {:.2}s, running {:.2}s)", copies.join(", "), copy_secs, t_start.elapsed().as_secs_f64() - copy_secs));
            if self.txn.borrow().is_some() {
                // (the whole copies hold what this transaction sees, which others must not: they are not kept)
                for t in tables {
                    if let Some(e) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&t.id) {
                        e.loaded = None;
                    }
                }
            }
            r
        }
        .await;
        for t in made {
            let _ = self.mirror.execute(&format!("DROP TABLE IF EXISTS {}", t));
        }
        result
    }
}
