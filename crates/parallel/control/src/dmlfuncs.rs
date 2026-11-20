//! Calls of functions that read sharded tables in `INSERT`, `UPDATE` and `DELETE`. Such a function lives on the control node only (a shard
//! holds a slice of each table), so the control node evaluates the calls and the statement goes to the shards with their values:
//!
//! * a call whose arguments are constants (or variables) is evaluated once, and the statement gets its value;
//! * a call over the columns of the table being changed (`SET x = dbo.f(col)`, `WHERE dbo.f(col) > 3`) is evaluated for each distinct value
//!   of its arguments among the rows concerned, and the statement gets a `CASE` that looks the value up. A row that appears between the
//!   two steps, with arguments that were not seen, raises an error (a divide by zero) instead of getting a wrong value: the statement
//!   can be run again.
use super::*;
use serde_json::Value as Json;

/// What a lookup `CASE` may hold: more distinct argument values than this are refused (the statement would be larger than a shard should be sent).
const LOOKUP_LIMIT: usize = 5000;

impl ClusterSession {
    /// A function of the cluster that reads sharded tables (so it is not on the shards): its definition.
    fn reading_function(&self, call: &Json) -> Option<CreateFunction> {
        let Json::Object(f) = call.get("Func")? else { return None };
        let Json::Array(parts) = f.get("parts")? else { return None };
        let p: Vec<String> = parts.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
        if p.is_empty() {
            return None;
        }
        let id = funcs::rel_id(&self.cur_db, &p);
        let st = self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()?;
        if self.cluster.function_on_shards(&id, &st) {
            return None;
        }
        match st {
            Stmt::CreateFunction(cf) => Some(*cf),
            _ => None,
        }
    }

    /// `e` with the calls of functions that read sharded tables replaced by what they give (see the top of this file). `target`: the table
    /// that is being changed, which a call may take its arguments from (None: there is no row, as in `VALUES`); `domain`: a condition that the
    /// rows concerned meet. None when `e` has no such call.
    pub(crate) async fn fold_reading_calls(&mut self, e: &Expr, target: Option<&Name>, domain: Option<&Expr>) -> Result<Option<Expr>, SqlError> {
        if !self.cluster.has_functions() || self.reading_function_tables(e).is_none() {
            return Ok(None);
        }
        let mut j = serde_json::to_value(e).map_err(|x| SqlError::new(50000, x.to_string()))?;
        let mut changed = false;
        self.fold_value(&mut j, target, domain, &mut changed).await?;
        if !changed {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(j).map_err(|x| SqlError::new(50000, format!("a function call could not be replaced: {}", x)))?))
    }

    fn fold_value<'a>(&'a mut self, v: &'a mut Json, target: Option<&'a Name>, domain: Option<&'a Expr>, changed: &'a mut bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SqlError>> + 'a>> {
        Box::pin(async move {
            match v {
                Json::Object(m) => {
                    // (a nested query is run by the cluster, which gathers what it reads: see `resolve_stmt`)
                    if m.len() == 1 && m.keys().any(|k| matches!(k.as_str(), "Subquery" | "Exists" | "InQuery" | "CmpQuant")) {
                        return Ok(());
                    }
                    for c in m.values_mut() {
                        self.fold_value(c, target, domain, changed).await?;
                    }
                    if m.len() == 1 && m.contains_key("Func") {
                        if let Some(cf) = self.reading_function(v) {
                            let replaced = self.fold_call(v, &cf, target, domain).await?;
                            *v = replaced;
                            *changed = true;
                        }
                    }
                }
                Json::Array(a) => {
                    for c in a {
                        self.fold_value(c, target, domain, changed).await?;
                    }
                }
                _ => {}
            }
            Ok(())
        })
    }

    /// One call: its value when its arguments are constants, else a `CASE` over its arguments.
    async fn fold_call(&mut self, call: &Json, cf: &CreateFunction, target: Option<&Name>, domain: Option<&Expr>) -> Result<Json, SqlError> {
        let to_json = |e: &Expr| serde_json::to_value(e).map_err(|x| SqlError::new(50000, x.to_string()));
        let args: Vec<Json> = call["Func"]["args"].as_array().cloned().unwrap_or_default();
        let on_rows = args.iter().any(|a| mentions_column(a));
        let shown = cf.name.join(".");
        if !on_rows {
            let e: Expr = serde_json::from_value(call.clone()).map_err(|x| SqlError::new(50000, x.to_string()))?;
            let b = self.bind_expr(&e)?;
            let v = self.eval_scalar(&b).await?;
            self.plan.push(format!("function {} evaluated first on the control node", shown));
            return to_json(&nested::lit(&v));
        }
        let Some(table) = target else {
            return Err(unsupported(&format!("a call of {}, which reads sharded tables, over columns, where there is no row", shown)));
        };
        let Some(ty) = cf.ret_type.clone() else { return Err(unsupported(&format!("a call of {}, which returns a table, in an expression", shown))) };
        // the distinct values of the arguments among the rows concerned (compared as bytes: a collation would call 'a' and 'A' one value,
        // and the function may tell them apart)
        let arg_exprs: Vec<Expr> = args.iter().map(|a| serde_json::from_value(a.clone()).map_err(|x| SqlError::new(50000, x.to_string()))).collect::<Result<_, _>>()?;
        let varbin = TypeRef { parts: vec!["varbinary".into()], args: vec![-1], xml_schema: None };
        let bytes = |e: &Expr| Expr::Cast { e: Box::new(e.clone()), ty: varbin.clone(), try_: false, style: None };
        let mut items: Vec<SelectItem> = vec![];
        for a in &arg_exprs {
            items.push(SelectItem::Expr { e: a.clone(), alias: None });
        }
        for a in &arg_exprs {
            items.push(SelectItem::Expr { e: bytes(a), alias: None });
        }
        let probe = simple_query(Select { distinct: true, top: None, items, into: None, from: vec![TableRef::Table { name: table.clone(), alias: None, hints: vec![] }], where_: domain.cloned(), group: None, having: None });
        let rs = self.run_query(&probe).await?;
        if rs.rows.len() > LOOKUP_LIMIT {
            return Err(unsupported(&format!("a call of {}, which reads sharded tables, over more than {} distinct values of its arguments", shown, LOOKUP_LIMIT)));
        }
        let n = arg_exprs.len();
        let mut whens: Vec<(Expr, Expr)> = vec![];
        for row in &rs.rows {
            // the call for these values
            let mut call_expr: Expr = serde_json::from_value(call.clone()).map_err(|x| SqlError::new(50000, x.to_string()))?;
            if let Expr::Func(f) = &mut call_expr {
                f.args = row[..n].iter().map(nested::lit).collect();
            }
            let b = self.bind_expr(&call_expr)?;
            let result = self.eval_scalar(&b).await?;
            // the rows that have these values
            let mut cond: Option<Expr> = None;
            for (k, a) in arg_exprs.iter().enumerate() {
                let one = if row[n + k].is_null() { Expr::IsNull(Box::new(a.clone()), false) } else { Expr::Binary(BinOp::Eq, Box::new(bytes(a)), Box::new(Expr::Lit(row[n + k].clone()))) };
                cond = Some(match cond {
                    None => one,
                    Some(c) => Expr::And(Box::new(c), Box::new(one)),
                });
            }
            whens.push((cond.unwrap_or_else(|| Expr::Lit(Value::Bit(true))), nested::lit(&result)));
        }
        // (rows that were not there when the values were read)
        let boom = Expr::Cast { e: Box::new(Expr::Binary(BinOp::Div, Box::new(Expr::Lit(Value::Int(1))), Box::new(Expr::Lit(Value::Int(0))))), ty, try_: false, style: None };
        self.plan.push(format!("function {} evaluated first on the control node for {} distinct values", shown, rs.rows.len()));
        to_json(&Expr::Case { operand: None, whens, els: Some(Box::new(boom)) })
    }

    /// `u` with its calls of functions that read sharded tables replaced; None when it has none.
    pub(crate) async fn fold_update(&mut self, u: &Update) -> Result<Option<Update>, SqlError> {
        let Target::Table(name) = &u.target else { return Ok(None) };
        let mut out = u.clone();
        let mut any = false;
        if let Some(w) = &u.where_ {
            if let Some(w2) = self.fold_reading_calls(w, Some(name), None).await? {
                out.where_ = Some(w2);
                any = true;
            }
        }
        for (i, s) in u.sets.iter().enumerate() {
            if let SetItem::Col { e, .. } = s {
                // (only the rows the condition lets through are changed)
                if let Some(e2) = self.fold_reading_calls(e, Some(name), out.where_.as_ref()).await? {
                    if let SetItem::Col { e: slot, .. } = &mut out.sets[i] {
                        *slot = e2;
                    }
                    any = true;
                }
            }
        }
        Ok(any.then_some(out))
    }

    /// `d` with its calls of functions that read sharded tables replaced; None when it has none.
    pub(crate) async fn fold_delete(&mut self, d: &Delete) -> Result<Option<Delete>, SqlError> {
        let Target::Table(name) = &d.target else { return Ok(None) };
        let Some(w) = &d.where_ else { return Ok(None) };
        Ok(self.fold_reading_calls(w, Some(name), None).await?.map(|w2| {
            let mut out = d.clone();
            out.where_ = Some(w2);
            out
        }))
    }
}

/// Whether an expression (as JSON) names a column.
fn mentions_column(v: &Json) -> bool {
    match v {
        Json::Object(m) => m.contains_key("Col") || m.values().any(mentions_column),
        Json::Array(a) => a.iter().any(mentions_column),
        _ => false,
    }
}
