//! Stored procedures on the control node: `EXEC` of a procedure that is not routed to a shard (see `procs.rs` for those) runs its
//! body here, statement by statement, as the session's own statements, so what the procedure reads and writes goes through the
//! cluster like any batch does. This is what makes `EXEC` work anywhere a statement does: in a batch, in a procedure, in a trigger.
use super::*;
use std::sync::Arc;

/// A procedure of the cluster (one made with `CREATE PROCEDURE` through the control node).
pub(crate) struct ProcDef {
    pub params: Vec<Param>,
    pub body: Arc<Vec<Stmt>>,
}

/// The deepest a chain of procedures, functions and triggers may nest (as in SQL Server).
const NEST_LIMIT: usize = 32;

fn norm(name: &str) -> String {
    format!("@{}", name.trim_start_matches('@').to_lowercase())
}

impl Cluster {
    /// The procedure with this id, as the schema log last left it.
    pub(crate) fn proc_def(&self, id: &str) -> Option<ProcDef> {
        let log = self.schema_log.lock().unwrap_or_else(|e| e.into_inner());
        for st in log.iter().rev() {
            match st {
                Stmt::CreateProc { name, params, body, .. } if table_id(name) == id => return Some(ProcDef { params: params.clone(), body: body.clone() }),
                Stmt::Drop { what, names, .. } if (what.eq_ignore_ascii_case("PROCEDURE") || what.eq_ignore_ascii_case("PROC")) && names.iter().any(|n| table_id(n) == id) => return None,
                Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("DATABASE") => {
                    if names.iter().filter_map(|n| n.last()).any(|db| id.starts_with(&format!("{}.", db.to_lowercase()))) {
                        return None;
                    }
                }
                _ => {}
            }
        }
        None
    }
}

impl ClusterSession {
    /// `EXEC proc args`: when `proc` is a procedure of the cluster, run its body here. False when it is not one (a system procedure, which
    /// the control node's engine runs).
    pub(crate) async fn exec_proc(&mut self, ex: &ExecStmt, events: &mut Vec<Event>) -> Result<bool, SqlError> {
        tsql_server::span!("ctl_exec_proc");
        if ex.name_is_var {
            return Ok(false);
        }
        let id = table_id(&ex.name);
        let Some(def) = self.cluster.proc_def(&id) else { return Ok(false) };
        let shown = ex.name.join(".");
        if self.proc_depth >= NEST_LIMIT {
            return Err(SqlError::new(217, "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32)."));
        }
        // the arguments, in the caller's scope: by name or by position, each for the parameter it is for
        let params = &def.params;
        let mut given: Vec<Option<(Expr, bool)>> = vec![None; params.len()];
        let mut by_name = false;
        for (i, a) in ex.args.iter().enumerate() {
            let idx = match &a.name {
                Some(n) => {
                    by_name = true;
                    params.iter().position(|p| norm(&p.name) == norm(n)).ok_or_else(|| SqlError::new(8145, format!("{} is not a parameter for procedure {}.", n, shown)))?
                }
                None => {
                    if by_name {
                        return Err(SqlError::new(8146, format!("Procedure {} has no parameters and arguments were supplied.", shown)));
                    }
                    if i >= params.len() {
                        return Err(SqlError::new(8144, format!("Procedure or function {} has too many arguments specified.", shown)));
                    }
                    i
                }
            };
            if given[idx].is_some() {
                return Err(SqlError::new(1049, format!("The formal parameter \"{}\" was mentioned twice in the call.", params[idx].name)));
            }
            given[idx] = Some((a.e.clone(), a.out));
        }
        let mut values: Vec<Value> = vec![];
        let mut table_args: Vec<(String, ResultSet)> = vec![];
        // OUTPUT parameters: (the parameter, the variable of the caller that is passed)
        let mut outs: Vec<(String, String)> = vec![];
        for (p, g) in params.iter().zip(given) {
            let key = norm(&p.name);
            let expr = match g {
                Some((e, out)) => {
                    // a table-valued parameter: the table variable that is passed
                    if let Expr::Var(v) = &e {
                        if let Some(rs) = self.tvars.get(&v.to_lowercase()).cloned() {
                            table_args.push((key.clone(), rs));
                            values.push(Value::Null);
                            continue;
                        }
                    }
                    if out && p.out {
                        match &e {
                            Expr::Var(v) => outs.push((key.clone(), v.to_lowercase())),
                            _ => return Err(SqlError::new(179, "Cannot use the OUTPUT option when passing a constant to a stored procedure.")),
                        }
                    }
                    e
                }
                None => match &p.default {
                    Some(d) => d.clone(),
                    None => return Err(SqlError::new(201, format!("Procedure or function '{}' expects parameter '{}', which was not supplied.", shown, p.name))),
                },
            };
            let b = self.bind_expr(&expr)?;
            let v = self.eval_scalar(&b).await?;
            // (the value is the parameter's type: a string that is passed for an INT is converted, one that cannot be is an error)
            let v = if matches!(v, Value::Null) || p.ty.parts.len() != 1 {
                v
            } else {
                self.eval_scalar(&Expr::Cast { e: Box::new(Expr::Lit(v)), ty: p.ty.clone(), try_: false, style: None }).await?
            };
            values.push(v);
        }
        // the body, in a scope of its own: the parameters, nothing else of the caller's, in the database the procedure is in
        let saved_vars = std::mem::take(&mut self.vars);
        let saved_tvars = std::mem::take(&mut self.tvars);
        let saved_db = self.cur_db.clone();
        let saved_return = std::mem::replace(&mut self.return_value, None);
        for (p, v) in params.iter().zip(values) {
            self.vars.insert(norm(&p.name), v);
        }
        for (k, rs) in table_args {
            self.tvars.insert(k, rs);
        }
        self.cur_db = funcs::id_db(&id);
        self.proc_depth += 1;
        self.returning = false;
        let mut r: Result<(), SqlError> = Ok(());
        for s in def.body.iter() {
            r = Box::pin(self.exec_stmt(s, events)).await;
            if r.is_err() || self.returning {
                break;
            }
        }
        self.proc_depth -= 1;
        self.returning = false;
        self.loop_ctl = 0;
        let returned = std::mem::replace(&mut self.return_value, saved_return);
        let out_values: Vec<(String, Value)> = outs.iter().filter_map(|(inner, outer)| self.vars.get(inner).map(|v| (outer.clone(), v.clone()))).collect();
        self.vars = saved_vars;
        self.tvars = saved_tvars;
        self.cur_db = saved_db;
        r?;
        for (outer, v) in out_values {
            self.vars.insert(outer, v);
        }
        if let Some(rv) = &ex.retvar {
            self.vars.insert(rv.to_lowercase(), returned.unwrap_or(Value::Int(0)));
        }
        self.plan.push(format!("procedure {} runs on the control node", shown));
        Ok(true)
    }

    /// `EXEC('text')` / `EXEC(@sql)`: the text is a batch of its own (it does not see the variables of the batch that runs it).
    pub(crate) async fn exec_string(&mut self, parts: &[Expr], events: &mut Vec<Event>) -> Result<(), SqlError> {
        let mut text = String::new();
        for p in parts {
            let b = self.bind_expr(p)?;
            let v = self.eval_scalar(&b).await?;
            text.push_str(&tsql_server::convert::display(&v));
        }
        let stmts = tsql_server::parser::parse(&text)?;
        let saved_vars = std::mem::take(&mut self.vars);
        let mut r = Ok(());
        for s in &stmts {
            r = Box::pin(self.exec_stmt(s, events)).await;
            if r.is_err() || self.returning {
                break;
            }
        }
        self.vars = saved_vars;
        r
    }
}
