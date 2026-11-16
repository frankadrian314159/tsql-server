//! Queries with subqueries, common table expressions and derived tables.
//!
//! Instead of copying every table such a query reads to the control node (the gather), the pieces that do not depend on the
//! outer query are evaluated first, each by the cluster itself (so they are scattered, joined and aggregated over the shards
//! as any query is), and the outer query is rewritten around their results:
//!
//! * a scalar subquery becomes its value, `EXISTS (...)` a true or false condition, `x IN (SELECT ...)` the list of values;
//! * a common table expression or a derived table becomes a small *replicated* temporary table, created on every shard for the
//!   statement (the outer query then joins it like any replicated table: no data moves for the join);
//!
//! and the rewritten query, now flat, goes through the usual planning (route, scatter-gather, join over shards). A piece that
//! cannot be evaluated alone because it refers to the outer query (a *correlated* subquery: it does not bind on its own), a
//! recursive CTE, `ANY`/`ALL`, and results too big to be broadcast or listed (see `set_broadcast_limit`) are not rewritten:
//! the whole statement is gathered as before, which is always exact.
use crate::*;
use serde_json::Value as J;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

/// The errors a subquery gives when it refers to a column of the query around it: evaluated alone, the name does not bind.
fn is_correlation_error(e: &SqlError) -> bool {
    matches!(e.number, 207 | 4104)
}

fn single_part(v: &J) -> Option<&str> {
    match v {
        J::Array(a) if a.len() == 1 => a[0].as_str(),
        _ => None,
    }
}

/// Whether a query (as JSON) reads a table with this one-part name.
fn mentions_table(v: &J, name: &str) -> bool {
    match v {
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(J::Object(t)) = m.get("Table") {
                    if t.get("name").and_then(single_part).map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false) {
                        return true;
                    }
                }
            }
            m.values().any(|c| mentions_table(c, name))
        }
        J::Array(a) => a.iter().any(|c| mentions_table(c, name)),
        _ => false,
    }
}

/// Point every reference to the table `from` at `to`, keeping the old name as the alias (so `from.col` still resolves).
fn rename_table(v: &mut J, from: &str, to: &str) {
    match v {
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(J::Object(t)) = m.get_mut("Table") {
                    if t.get("name").and_then(single_part).map(|n| n.eq_ignore_ascii_case(from)).unwrap_or(false) {
                        t.insert("name".into(), J::Array(vec![J::String(to.to_string())]));
                        if t.get("alias").map(|a| a.is_null()).unwrap_or(true) {
                            t.insert("alias".into(), J::String(from.to_string()));
                        }
                        return;
                    }
                }
            }
            for c in m.values_mut() {
                rename_table(c, from, to);
            }
        }
        J::Array(a) => {
            for c in a {
                rename_table(c, from, to);
            }
        }
        _ => {}
    }
}

/// The column a query joins the table (a common table expression) named `cte` to itself on: the most common name among the
/// `a.col = b.col` conditions between two references to it. None when it is not joined to itself.
fn join_column(query: &J, cte: &str) -> Option<String> {
    let q: Query = serde_json::from_value(query.clone()).ok()?;
    let QueryBody::Select(sel) = &q.body else { return None };
    let mut mine: Vec<String> = vec![];
    let mut on: Vec<Expr> = vec![];
    fn walk(t: &TableRef, cte: &str, mine: &mut Vec<String>, on: &mut Vec<Expr>) {
        match t {
            TableRef::Table { name, alias, .. } if name.len() == 1 && name[0].eq_ignore_ascii_case(cte) => mine.push(alias.clone().unwrap_or_else(|| name[0].clone()).to_lowercase()),
            TableRef::Join { left, right, on: cond, .. } => {
                walk(left, cte, mine, on);
                walk(right, cte, mine, on);
                if let Some(c) = cond {
                    on.push(c.clone());
                }
            }
            _ => {}
        }
    }
    for t in &sel.from {
        walk(t, cte, &mut mine, &mut on);
    }
    if mine.len() < 2 {
        return None;
    }
    if let Some(w) = &sel.where_ {
        on.push(w.clone());
    }
    fn conj(e: &Expr, out: &mut Vec<Expr>) {
        match e {
            Expr::And(a, b) => {
                conj(a, out);
                conj(b, out);
            }
            Expr::Paren(a) => conj(a, out),
            other => out.push(other.clone()),
        }
    }
    let mut all = vec![];
    for e in &on {
        conj(e, &mut all);
    }
    let mut count: HashMap<String, usize> = HashMap::new();
    for c in &all {
        if let Expr::Binary(BinOp::Eq, a, b) = c {
            if let (Expr::Col(x), Expr::Col(y)) = (&**a, &**b) {
                if x.len() == 2 && y.len() == 2 && x[1].eq_ignore_ascii_case(&y[1]) && x[0] != y[0] && mine.contains(&x[0].to_lowercase()) && mine.contains(&y[0].to_lowercase()) {
                    *count.entry(x[1].to_string()).or_default() += 1;
                }
            }
        }
    }
    count.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c)
}

fn json_err(e: serde_json::Error) -> SqlError {
    SqlError::new(50000, format!("the query could not be rewritten for the cluster: {}", e))
}

/// A literal for a value. The rewrite goes through JSON, which has no number for a decimal beyond 64 bits (a sum of money over
/// many rows): such a value is written as a cast of its text, which says the same thing.
fn lit(v: &Value) -> Expr {
    if let Value::Dec(d) = v {
        if serde_json::to_value(v).is_err() {
            let ty = TypeRef { parts: vec!["decimal".into()], args: vec![38, d.s as i64], xml_schema: None };
            return Expr::Cast { e: Box::new(Expr::Lit(Value::Str(v.to_string()))), ty, try_: false, style: None };
        }
    }
    Expr::Lit(v.clone())
}

fn lit_true(yes: bool) -> Expr {
    Expr::Binary(BinOp::Eq, Box::new(Expr::Lit(Value::Int(1))), Box::new(Expr::Lit(Value::Int(if yes { 1 } else { 0 }))))
}

/// Replace every reference to a view by a derived table holding the view's query; true when something was replaced (a view
/// may read other views: call again until nothing is).
fn expand_views(v: &mut J, views: &HashMap<String, (Option<Vec<String>>, J)>) -> bool {
    match v {
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(J::Object(t)) = m.get("Table") {
                    let parts: Vec<String> = t.get("name").and_then(|n| n.as_array()).map(|a| a.iter().filter_map(|p| p.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
                    if let Some((cols, query)) = views.get(&table_id(&parts)) {
                        let alias = match t.get("alias") {
                            Some(a) if !a.is_null() => a.clone(),
                            _ => J::String(parts.last().cloned().unwrap_or_default()),
                        };
                        *v = serde_json::json!({"Derived": {"query": query, "alias": alias, "cols": cols}});
                        return true;
                    }
                }
            }
            let mut any = false;
            for c in m.values_mut() {
                any |= expand_views(c, views);
            }
            any
        }
        J::Array(a) => {
            let mut any = false;
            for c in a.iter_mut() {
                any |= expand_views(c, views);
            }
            any
        }
        _ => false,
    }
}

impl ClusterSession {
    /// The query with the views it reads replaced by their definitions; None when it reads none.
    fn with_views_expanded(&self, q: &Query) -> Result<Option<Query>, SqlError> {
        let defs: HashMap<String, (Option<Vec<String>>, J)> = {
            let views = self.cluster.views.lock().unwrap_or_else(|e| e.into_inner());
            if views.is_empty() {
                return Ok(None);
            }
            views
                .iter()
                .filter_map(|(id, st)| match st {
                    Stmt::CreateView { cols, query, .. } => Some((id.clone(), (cols.clone(), serde_json::to_value(query.as_ref()).ok()?))),
                    _ => None,
                })
                .collect()
        };
        let mut v = serde_json::to_value(q).map_err(json_err)?;
        let mut changed = false;
        // (a view may read views; a cycle cannot be made, but the number of rounds is bounded anyway)
        for _ in 0..16 {
            if !expand_views(&mut v, &defs) {
                break;
            }
            changed = true;
        }
        if !changed {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v).map_err(json_err)?))
    }

    /// Run a query. One with subqueries or CTEs is rewritten around the results of its independent parts when it can be.
    pub(crate) async fn run_query(&mut self, q: &Query) -> Result<ResultSet, SqlError> {
        if let Some(expanded) = self.with_views_expanded(q)? {
            self.plan.push("views expanded into their definitions".into());
            return Box::pin(self.run_query(&expanded)).await;
        }
        // a derived table that is a plain join of several tables is merged into the query that reads it (see `flatten.rs`)
        if let Some(flat) = crate::flatten::flatten_derived(q) {
            self.plan.push("derived table merged into the query".into());
            return Box::pin(self.run_query(&flat)).await;
        }
        // a subquery co-located with the outer table by shard key runs on the shards, before anything is evaluated around them
        if has_nested_query(q) {
            let ids = referenced_tables(q);
            if ids.iter().filter_map(|i| self.table_by_id(i)).any(|t| matches!(t.dist, Dist::Sharded { .. })) {
                if let Some(rs) = self.try_pushdown(q, true).await? {
                    return Ok(rs);
                }
            }
        }
        if !has_nested_query(q) {
            return self.run_flat(q).await;
        }
        // (a correlated subquery is first rewritten as a join; if the rewrite does not bind, the statement is run as it was)
        for decorrelate in [true, false] {
            let mut temps: Vec<String> = vec![];
            let mut rewrote = false;
            let out = match self.resolve_query(q, &mut temps, decorrelate, &mut rewrote).await {
                Ok(Some(flat)) => match self.run_flat(&flat).await {
                    // the rewritten query had to be gathered, and the control node's engine does not have the temporary tables
                    // the pieces were put in (they are on the shards): gather the statement as it was
                    Err(e) if e.number == 208 && e.message.contains("#dpn_") => self.run_flat(q).await,
                    r => r,
                },
                // not rewritable: the whole statement is gathered
                Ok(None) => self.run_flat(q).await,
                Err(e) => Err(e),
            };
            self.drop_temps(temps).await;
            if rewrote && matches!(&out, Err(e) if is_correlation_error(e) || e.number == 209) {
                continue;
            }
            return out;
        }
        unreachable!()
    }

    /// Run a query without nested parts. A long list of literals in its condition (`x IN (1, 2, ...)`) is first made a replicated
    /// temporary table and joined in: the shards then probe a table (a hash join, or an index) where they would otherwise compare each
    /// row with every item of the list.
    async fn run_flat(&mut self, q: &Query) -> Result<ResultSet, SqlError> {
        let mut temps = vec![];
        let rewritten = match self.in_list_to_join(q, &mut temps).await {
            Ok(r) => r,
            Err(e) => {
                self.drop_temps(temps).await;
                return Err(e);
            }
        };
        let out = match &rewritten {
            Some(r) => match self.run_query_flat(r).await {
                // (the rewritten query had to be gathered and the control node's engine does not have the temporary table)
                Err(e) if e.number == 208 && e.message.contains("#dpn_") => self.run_query_flat(q).await,
                r => r,
            },
            None => self.run_query_flat(q).await,
        };
        self.drop_temps(temps).await;
        out
    }

    /// `WHERE x IN (v1, v2, ...)` with at least `in_join_min` literals, at the top level of the condition, becomes a join with a
    /// replicated table of the distinct values; `x NOT IN (...)` becomes a left join that keeps the rows with no match. None when the
    /// query has no such condition or the list cannot be treated so (a NULL or an expression in it, strings that are equal under a
    /// case-insensitive collation, a probe that is not a plain column).
    async fn in_list_to_join(&mut self, q: &Query, temps: &mut Vec<String>) -> Result<Option<Query>, SqlError> {
        let min = self.cluster.in_join_min.load(Ordering::Relaxed) as usize;
        let QueryBody::Select(sel) = &q.body else { return Ok(None) };
        let Some(w) = &sel.where_ else { return Ok(None) };
        // (a bare `*` would show the joined table's column; an INTO or a variable assignment is left alone)
        if sel.into.is_some() || sel.items.iter().any(|i| matches!(i, SelectItem::Star(None) | SelectItem::Assign { .. })) {
            return Ok(None);
        }
        fn conj(e: &Expr, out: &mut Vec<Expr>) {
            match e {
                Expr::And(a, b) => {
                    conj(a, out);
                    conj(b, out);
                }
                Expr::Paren(a) => conj(a, out),
                other => out.push(other.clone()),
            }
        }
        let mut all = vec![];
        conj(w, &mut all);
        let Some(at) = all.iter().position(|c| matches!(c, Expr::InList(a, items, _) if items.len() >= min && matches!(&**a, Expr::Col(_)) && items.iter().all(|i| matches!(i, Expr::Lit(Value::Int(_) | Value::BigInt(_) | Value::Str(_)))))) else { return Ok(None) };
        let Expr::InList(probe, items, neg) = all[at].clone() else { return Ok(None) };
        let values: Vec<Value> = items.iter().filter_map(|i| if let Expr::Lit(v) = i { Some(v.clone()) } else { None }).collect();
        if neg && sel.from.len() != 1 {
            return Ok(None);
        }
        let strings = values.iter().any(|v| matches!(v, Value::Str(_)));
        if strings && !values.iter().all(|v| matches!(v, Value::Str(_))) {
            return Ok(None);
        }
        // the distinct values; for strings, a pair that a collation might call equal is left to the shards
        let mut seen = std::collections::HashSet::new();
        let mut folded = std::collections::HashSet::new();
        let mut rows = vec![];
        for v in &values {
            let key = v.to_string();
            if !seen.insert(key.clone()) {
                continue;
            }
            if strings && !folded.insert(key.trim_end().to_lowercase()) {
                return Ok(None);
            }
            rows.push(vec![v.clone()]);
        }
        let ty = if strings {
            tsql_server::DataType::VarChar(8000)
        } else if values.iter().any(|v| matches!(v, Value::BigInt(_))) {
            tsql_server::DataType::BigInt
        } else {
            tsql_server::DataType::Int
        };
        let rs = ResultSet { columns: vec!["k".into()], types: vec![ty], rows };
        let Some(temp) = self.materialize(rs, Some(vec!["k".into()]), temps).await? else { return Ok(None) };
        let alias = format!("{}_j", temp.trim_start_matches('#'));
        let key = Expr::Col(vec![alias.clone(), "k".into()]);
        let table = TableRef::Table { name: vec![temp], alias: Some(alias), hints: vec![] };
        let mut out = q.clone();
        let QueryBody::Select(s) = &mut out.body else { return Ok(None) };
        let mut rest: Vec<Expr> = all.iter().enumerate().filter(|(i, _)| *i != at).map(|(_, c)| c.clone()).collect();
        let eq = Expr::Binary(BinOp::Eq, probe.clone(), Box::new(key.clone()));
        if neg {
            // an anti join: the rows of the first table that find no partner; a NULL probe is unknown for NOT IN, so it is dropped
            let Some(last) = s.from.pop() else { return Ok(None) };
            s.from.push(TableRef::Join { kind: JoinKind::Left, left: Box::new(last), right: Box::new(table), on: Some(eq) });
            rest.push(Expr::IsNull(Box::new(key), false));
            rest.push(Expr::IsNull(probe, true));
        } else {
            s.from.push(table);
            rest.push(eq);
        }
        let mut it = rest.into_iter();
        let first = it.next().unwrap();
        s.where_ = Some(it.fold(first, |a, b| Expr::And(Box::new(a), Box::new(b))));
        self.plan.push(format!("list of {} values joined as a replicated table", items.len()));
        Ok(Some(out))
    }

    /// The query with its independent nested parts replaced by their results, or None when some part cannot be.
    async fn resolve_query(&mut self, q: &Query, temps: &mut Vec<String>, decorrelate: bool, rewrote: &mut bool) -> Result<Option<Query>, SqlError> {
        let mut v = serde_json::to_value(q).map_err(json_err)?;
        let ctes: Vec<J> = match v.get_mut("ctes") {
            Some(J::Array(a)) => std::mem::take(a),
            _ => vec![],
        };
        if !ctes.is_empty() && q.maxrecursion.is_some() {
            return Ok(None);
        }
        // the CTEs in order: each may use the ones before it
        let mut rest = ctes;
        while !rest.is_empty() {
            let cte = rest.remove(0);
            let name = cte.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            let mut cq_json = cte.get("query").cloned().unwrap_or(J::Null);
            if name.is_empty() || mentions_table(&cq_json, &name) {
                return Ok(None); // recursive
            }
            // (a nested query of the CTE is resolved by running it as a query of its own)
            let cq: Query = serde_json::from_value(std::mem::take(&mut cq_json)).map_err(json_err)?;
            let Some(rs) = self.eval_nested(&cq).await? else { return Ok(None) };
            let cols: Option<Vec<String>> = cte.get("cols").and_then(|c| serde_json::from_value(c.clone()).ok());
            // (a result too big to broadcast is spread over the shards by the column the query joins it on, when it joins it to itself)
            let too_big = rs.rows.len() as u64 > self.cluster.broadcast_limit.load(Ordering::Relaxed);
            let temp = match too_big.then(|| join_column(&v, &name)).flatten() {
                Some(column) => self.materialize_spread(rs, cols, &column, temps).await?,
                None => self.materialize(rs, cols, temps).await?,
            };
            let Some(temp) = temp else { return Ok(None) };
            let mut later = J::Array(rest);
            rename_table(&mut later, &name, &temp);
            rest = match later {
                J::Array(a) => a,
                _ => vec![],
            };
            rename_table(&mut v, &name, &temp);
        }
        if let Some(c) = v.get_mut("ctes") {
            *c = J::Array(vec![]);
        }
        if decorrelate {
            if let Ok(mut typed) = serde_json::from_value::<Query>(v.clone()) {
                let mut counter = self.temp_counter;
                // (the columns of the tables it reads: how an unqualified name is told to be the subquery's or the outer query's)
                let schema: HashMap<String, Vec<String>> = referenced_tables(&typed).iter().filter_map(|i| self.table_by_id(i).map(|t| (i.clone(), t.cols.iter().map(|c| c.name.to_lowercase()).collect()))).collect();
                if crate::decorrelate::decorrelate(&mut typed, &mut counter, schema) {
                    self.temp_counter = counter;
                    self.plan.push("correlated subquery rewritten as a join".into());
                    *rewrote = true;
                    v = serde_json::to_value(&typed).map_err(json_err)?;
                }
            }
        }
        if !self.resolve_value(&mut v, temps).await? {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v).map_err(json_err)?))
    }

    /// A statement with its subqueries replaced by their results (for an UPDATE or DELETE whose condition has one). None when a
    /// subquery cannot be evaluated alone.
    pub(crate) async fn resolve_stmt(&mut self, st: &Stmt) -> Result<Option<Stmt>, SqlError> {
        let mut temps = vec![];
        let mut v = serde_json::to_value(st).map_err(json_err)?;
        let ok = self.resolve_value(&mut v, &mut temps).await;
        self.drop_temps(temps).await;
        if !ok? {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v).map_err(json_err)?))
    }

    /// Run a nested query alone. None when it does not bind alone (it is correlated).
    async fn eval_nested(&mut self, q: &Query) -> Result<Option<ResultSet>, SqlError> {
        match Box::pin(self.run_query(q)).await {
            Ok(rs) => Ok(Some(rs)),
            Err(e) if is_correlation_error(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Replace the nested queries inside `v` (not those inside nested queries: running them resolves theirs). False: one
    /// cannot be replaced.
    fn resolve_value<'a>(&'a mut self, v: &'a mut J, temps: &'a mut Vec<String>) -> Pin<Box<dyn Future<Output = Result<bool, SqlError>> + 'a>> {
        Box::pin(async move {
            match v {
                J::Object(m) => {
                    if m.len() == 1 {
                        let key = m.keys().next().cloned().unwrap_or_default();
                        match key.as_str() {
                            "Subquery" | "Exists" => {
                                let q: Query = serde_json::from_value(m[&key].clone()).map_err(json_err)?;
                                let Some(rs) = self.eval_nested(&q).await? else { return Ok(false) };
                                let e = if key == "Exists" {
                                    lit_true(!rs.rows.is_empty())
                                } else {
                                    if rs.columns.len() != 1 {
                                        return Err(SqlError::new(116, "Only one expression can be specified in the select list when the subquery is not introduced with EXISTS."));
                                    }
                                    if rs.rows.len() > 1 {
                                        return Err(SqlError::new(512, "Subquery returned more than 1 value. This is not permitted when the subquery follows =, !=, <, <= , >, >= or when the subquery is used as an expression."));
                                    }
                                    rs.rows.first().map(|r| lit(&r[0])).unwrap_or(Expr::Lit(Value::Null))
                                };
                                self.plan.push(format!("subquery evaluated first ({} rows)", rs.rows.len()));
                                *v = serde_json::to_value(&e).map_err(json_err)?;
                                return Ok(true);
                            }
                            "InQuery" => {
                                let Some(J::Array(arr)) = m.get_mut("InQuery") else { return Ok(false) };
                                if arr.len() != 3 {
                                    return Ok(false);
                                }
                                if !self.resolve_value(&mut arr[0], temps).await? {
                                    return Ok(false);
                                }
                                let q: Query = serde_json::from_value(arr[1].clone()).map_err(json_err)?;
                                let a: Expr = serde_json::from_value(arr[0].clone()).map_err(json_err)?;
                                let neg = arr[2].as_bool().unwrap_or(false);
                                let Some(rs) = self.eval_nested(&q).await? else { return Ok(false) };
                                if rs.columns.len() != 1 {
                                    return Err(SqlError::new(116, "Only one expression can be specified in the select list when the subquery is not introduced with EXISTS."));
                                }
                                if rs.rows.len() as u64 > self.cluster.in_list_limit.load(Ordering::Relaxed) {
                                    // too long for a list: the result becomes a replicated temporary table (up to the broadcast limit),
                                    // and the condition `x IN (SELECT k FROM that table)` is run by the shards, which have it all
                                    let renamed = ResultSet { columns: vec!["k".into()], ..rs.clone() };
                                    let Some(temp) = self.materialize(renamed, Some(vec!["k".into()]), temps).await? else { return Ok(false) };
                                    let sub = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: col("k"), alias: None }], into: None, from: vec![TableRef::Table { name: vec![temp], alias: None, hints: vec![] }], where_: None, group: None, having: None });
                                    let e = Expr::InQuery(Box::new(a), Box::new(sub), neg);
                                    *v = serde_json::to_value(&e).map_err(json_err)?;
                                    return Ok(true);
                                }
                                self.plan.push(format!("IN subquery evaluated first ({} values)", rs.rows.len()));
                                let e = if rs.rows.is_empty() { lit_true(neg) } else { Expr::InList(Box::new(a), rs.rows.iter().map(|r| lit(&r[0])).collect(), neg) };
                                *v = serde_json::to_value(&e).map_err(json_err)?;
                                return Ok(true);
                            }
                            "CmpQuant" => return Ok(false),
                            "TableVar" => {
                                let obj = m[&key].clone();
                                let var = obj.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let lower = var.to_lowercase();
                                let Some(rs) = self.tvars.get(&lower).cloned() else { return Ok(false) };
                                let Some(temp) = self.materialize(rs, None, temps).await? else { return Ok(false) };
                                let alias = match obj.get("alias") {
                                    Some(a) if !a.is_null() => a.clone(),
                                    _ => J::String(var.trim_start_matches('@').to_string()),
                                };
                                *v = serde_json::json!({"Table": {"name": [temp], "alias": alias, "hints": []}});
                                return Ok(true);
                            }
                            "Derived" => {
                                let obj = m[&key].clone();
                                let q: Query = serde_json::from_value(obj.get("query").cloned().unwrap_or(J::Null)).map_err(json_err)?;
                                let Some(rs) = self.eval_nested(&q).await? else { return Ok(false) };
                                let cols: Option<Vec<String>> = obj.get("cols").and_then(|c| serde_json::from_value(c.clone()).ok());
                                let Some(temp) = self.materialize(rs, cols, temps).await? else { return Ok(false) };
                                let alias = obj.get("alias").cloned().unwrap_or(J::Null);
                                *v = serde_json::json!({"Table": {"name": [temp], "alias": alias, "hints": []}});
                                return Ok(true);
                            }
                            _ => {}
                        }
                    }
                    // a query with CTEs of its own that is part of this one (a parenthesised operand): not rewritten
                    if let Some(J::Array(c)) = m.get("ctes") {
                        if !c.is_empty() {
                            return Ok(false);
                        }
                    }
                    for c in m.values_mut() {
                        if !self.resolve_value(c, temps).await? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                J::Array(a) => {
                    for c in a.iter_mut() {
                        if !self.resolve_value(c, temps).await? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                _ => Ok(true),
            }
        })
    }

    /// Put a result in a replicated temporary table on every shard (`cols` renames its columns) and make it known to this
    /// session as a table. None when it is too big to broadcast or its columns have no names.
    async fn materialize(&mut self, rs: ResultSet, cols: Option<Vec<String>>, temps: &mut Vec<String>) -> Result<Option<String>, SqlError> {
        if rs.rows.len() as u64 > self.cluster.broadcast_limit.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let names: Vec<String> = match cols {
            Some(c) if c.len() == rs.columns.len() => c,
            Some(_) => return Ok(None),
            None => rs.columns.clone(),
        };
        if names.iter().any(|n| n.is_empty()) || names.iter().enumerate().any(|(i, n)| names[..i].iter().any(|m| m.eq_ignore_ascii_case(n))) {
            return Ok(None);
        }
        let types = merge::widen(&rs.types, &rs.rows);
        self.temp_counter += 1;
        let temp = format!("#dpn_{}_{}", self.id, self.temp_counter);
        let defs: Vec<String> = names.iter().zip(&types).map(|(n, t)| format!("[{}] {} NULL", n.replace(']', "]]"), t)).collect();
        let create = format!("CREATE TABLE {} ({})", temp, defs.join(", "));
        let shards = self.all_shards();
        self.fan_run(&shards, |_| Work::Sql(create.clone())).await?;
        temps.push(temp.clone());
        for chunk in rs.rows.chunks(1000) {
            let rows: Vec<Vec<Expr>> = chunk.iter().map(|r| r.iter().cloned().map(Expr::Lit).collect()).collect();
            let ins = Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(vec![temp.clone()]), cols: None, output: None, src: InsertSrc::Values(rows), top: None }));
            self.fan_run(&shards, |_| Work::Stmts(vec![ins.clone()])).await?;
        }
        let table = DistTable {
            id: table_id(std::slice::from_ref(&temp)),
            name: vec![temp.clone()],
            cols: names.iter().zip(&types).map(|(n, t)| ColInfo { name: n.clone(), computed: false, type_name: t.to_string().to_lowercase().split('(').next().unwrap_or("").to_string() }).collect(),
            dist: Dist::Replicated,
            version: 0,
            loaded: None,
            rows: Some((0, rs.rows.len() as u64)),
            identity: None,
        };
        self.local_tables.insert(table.id.clone(), table);
        self.plan.push(format!("materialized {} rows as the replicated table {}", rs.rows.len(), temp));
        Ok(Some(temp))
    }

    /// Put a result that is too big to broadcast in a temporary table that is *spread* over the shards: each row goes to the shard that
    /// owns the hash of its `column`, exactly where a sharded table keyed on that column would have it, and the table is made known to
    /// this session as such. A query that joins it to itself (or to a table sharded the same way) on that column then runs shard by
    /// shard, in place. None when the column cannot be a shard key or the result has no usable names.
    async fn materialize_spread(&mut self, rs: ResultSet, cols: Option<Vec<String>>, column: &str, temps: &mut Vec<String>) -> Result<Option<String>, SqlError> {
        if rs.rows.len() > 5_000_000 {
            return Ok(None);
        }
        let names: Vec<String> = match cols {
            Some(c) if c.len() == rs.columns.len() => c,
            Some(_) => return Ok(None),
            None => rs.columns.clone(),
        };
        if names.iter().any(|n| n.is_empty()) || names.iter().enumerate().any(|(i, n)| names[..i].iter().any(|m| m.eq_ignore_ascii_case(n))) {
            return Ok(None);
        }
        let Some(key) = names.iter().position(|n| n.eq_ignore_ascii_case(column)) else { return Ok(None) };
        let types = merge::widen(&rs.types, &rs.rows);
        let base = |t: &tsql_server::DataType| t.to_string().to_lowercase().split('(').next().unwrap_or("").to_string();
        let Some(class) = KeyClass::of_type_name(&base(&types[key])) else { return Ok(None) };
        self.temp_counter += 1;
        let temp = format!("#dpn_{}_{}", self.id, self.temp_counter);
        let defs: Vec<String> = names.iter().zip(&types).map(|(n, t)| format!("[{}] {} NULL", n.replace(']', "]]"), t)).collect();
        let create = format!("CREATE TABLE {} ({})", temp, defs.join(", "));
        let shards = self.all_shards();
        self.fan_run(&shards, |_| Work::Sql(create.clone())).await?;
        temps.push(temp.clone());
        let n = rs.rows.len();
        let mut per: Vec<Vec<Vec<Value>>> = vec![vec![]; shards.len()];
        for row in rs.rows {
            let h = key_hash(&row[key], class).unwrap_or(0);
            per[self.cluster.shard_of_hash(h)].push(row);
        }
        let mut offset = 0;
        loop {
            let stmts: Vec<Option<Stmt>> = per
                .iter()
                .map(|rows| {
                    if offset >= rows.len() {
                        return None;
                    }
                    let chunk: Vec<Vec<Expr>> = rows[offset..(offset + 1000).min(rows.len())].iter().map(|r| r.iter().cloned().map(Expr::Lit).collect()).collect();
                    Some(Stmt::Insert(Box::new(Insert { ctes: vec![], target: Target::Table(vec![temp.clone()]), cols: None, output: None, src: InsertSrc::Values(chunk), top: None })))
                })
                .collect();
            let active: Vec<usize> = (0..stmts.len()).filter(|i| stmts[*i].is_some()).collect();
            if active.is_empty() {
                break;
            }
            self.fan_run(&active, |i| Work::Stmts(vec![stmts[i].clone().unwrap()])).await?;
            offset += 1000;
        }
        let table = DistTable {
            id: table_id(std::slice::from_ref(&temp)),
            name: vec![temp.clone()],
            cols: names.iter().zip(&types).map(|(n, t)| ColInfo { name: n.clone(), computed: false, type_name: base(t) }).collect(),
            dist: Dist::Sharded { key, class },
            version: 0,
            loaded: None,
            rows: Some((0, n as u64)),
            identity: None,
        };
        self.local_tables.insert(table.id.clone(), table);
        self.plan.push(format!("materialized {} rows spread over the shards by {} as {}", n, column, temp));
        Ok(Some(temp))
    }

    /// Drop the temporary tables a statement made, on every shard, and forget them.
    async fn drop_temps(&mut self, temps: Vec<String>) {
        if temps.is_empty() {
            return;
        }
        let shards = self.all_shards();
        for t in temps {
            self.local_tables.remove(&table_id(std::slice::from_ref(&t)));
            let drop = Stmt::Drop { what: "TABLE".into(), names: vec![vec![t]], if_exists: true };
            let _ = self.fan_run(&shards, |_| Work::Stmts(vec![drop.clone()])).await;
        }
    }
}
