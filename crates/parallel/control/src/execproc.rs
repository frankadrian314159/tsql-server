//! Stored procedures on the control node: `EXEC` of a procedure that is not routed to a shard (see `procs.rs` for those) runs its
//! body here, statement by statement, as the session's own statements, so what the procedure reads and writes goes through the
//! cluster like any batch does. This is what makes `EXEC` work anywhere a statement does: in a batch, in a procedure, in a trigger.
use super::*;
use std::collections::HashMap;
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
                        let lower = v.to_lowercase();
                        let trimmed = lower.trim_start_matches('@').to_string();
                        if let Some(rs) = self.tvars.get(&lower).or_else(|| self.tvars.get(&trimmed)).cloned() {
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
        if self.try_pushdown_proc(ex, &def, &values, &table_args, &outs, events).await? {
            return Ok(true);
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
        let stmts = self.cluster.parse_cached(&text)?;
        let saved_vars = std::mem::take(&mut self.vars);
        let mut r = Ok(());
        for s in stmts.iter() {
            r = Box::pin(self.exec_stmt(s, events)).await;
            if r.is_err() || self.returning {
                break;
            }
        }
        self.vars = saved_vars;
        r
    }

    /// Try to push down procedure execution directly to shards:
    /// 1. TVP batch procedures: partition rows across shards by the target table's shard key,
    ///    renumber row ids, and push down execution in parallel.
    /// 2. Scalar procedures: validate whether single-shard routing is safe and auto-register route.
    async fn try_pushdown_proc(
        &mut self,
        ex: &ExecStmt,
        def: &ProcDef,
        values: &[Value],
        table_args: &[(String, ResultSet)],
        outs: &[(String, String)],
        events: &mut Vec<Event>,
    ) -> Result<bool, SqlError> {
        if !outs.is_empty() || ex.retvar.is_some() {
            return Ok(false);
        }
        let id = table_id(&ex.name);

        // Case 1: TVP batch loop pushdown
        if table_args.len() == 1 {
            let (tvp_key, rs) = &table_args[0];
            if rs.rows.is_empty() {
                return Ok(true);
            }
            let mut writes = vec![];
            collect_writes(&def.body, &mut writes);
            if !writes.is_empty() {
                let target_name = &writes[0];
                let all_same = writes.iter().all(|w| table_id(w) == table_id(target_name));
                if all_same {
                    let target_id = table_id(target_name);
                    let target_table = self.table(target_name).ok();
                    if let Some(t) = target_table {
                        if let Dist::Sharded { key: key_idx, class } = t.dist {
                            let key_col_name = &t.cols[key_idx].name;
                            if let Some(tvp_key_idx) = rs.columns.iter().position(|c| c.eq_ignore_ascii_case(key_col_name)) {
                                let mut shard_rows: HashMap<usize, Vec<Vec<Value>>> = HashMap::new();
                                for r in &rs.rows {
                                    if let Some(h) = key_hash(&r[tvp_key_idx], class) {
                                        let s = self.cluster.shard_of_hash(h);
                                        shard_rows.entry(s).or_default().push(r.clone());
                                    }
                                }
                                let row_id_col = rs.columns.iter().position(|c| c.eq_ignore_ascii_case("rowid") || c.eq_ignore_ascii_case("id"));
                                let tvp_param_pos = def.params.iter().position(|p| norm(&p.name) == norm(tvp_key));
                                let Some(tvp_pos) = tvp_param_pos else { return Ok(false) };
                                let tvp_param = &def.params[tvp_pos];

                                let size_param_pos = def.params.iter().position(|p| {
                                    let n = p.name.to_lowercase();
                                    n.contains("size") || n.contains("count") || n.contains("batch")
                                });

                                let mut shard_futs = vec![];
                                for (shard_id, mut sub_rows) in shard_rows {
                                    if sub_rows.is_empty() {
                                        continue;
                                    }
                                    if let Some(rid) = row_id_col {
                                        for (i, row) in sub_rows.iter_mut().enumerate() {
                                            row[rid] = Value::Int((i + 1) as i32);
                                        }
                                    }
                                    let sub_len = sub_rows.len();
                                    let sub_var = format!("@sub_{}", shard_id);
                                    let decl = Stmt::Declare(vec![Decl::Var {
                                        name: sub_var.clone(),
                                        ty: tvp_param.ty.clone(),
                                        default: None,
                                    }]);
                                    let ins = Stmt::Insert(Box::new(Insert {
                                        ctes: vec![],
                                        target: Target::Var(sub_var.clone()),
                                        cols: Some(rs.columns.clone()),
                                        output: None,
                                        src: InsertSrc::Values(sub_rows.into_iter().map(|r| r.into_iter().map(Expr::Lit).collect()).collect()),
                                        top: None,
                                    }));
                                    let mut shard_args = vec![];
                                    for (i, p) in def.params.iter().enumerate() {
                                        if i == tvp_pos {
                                            shard_args.push(ExecArg {
                                                name: Some(p.name.clone()),
                                                e: Expr::Var(sub_var.clone()),
                                                out: false,
                                            });
                                        } else if Some(i) == size_param_pos {
                                            shard_args.push(ExecArg {
                                                name: Some(p.name.clone()),
                                                e: Expr::Lit(Value::Int(sub_len as i32)),
                                                out: false,
                                            });
                                        } else {
                                            shard_args.push(ExecArg {
                                                name: Some(p.name.clone()),
                                                e: Expr::Lit(values[i].clone()),
                                                out: false,
                                            });
                                        }
                                    }
                                    let exec_stmt = Stmt::Exec(Box::new(ExecStmt {
                                        name: ex.name.clone(),
                                        name_is_var: false,
                                        args: shard_args,
                                        retvar: None,
                                    }));
                                    let mut stmts = vec![];
                                    if !self.cur_db.is_empty() && !self.cur_db.eq_ignore_ascii_case("master") {
                                        stmts.push(Stmt::Use(self.cur_db.clone()));
                                    }
                                    stmts.push(decl);
                                    stmts.push(ins);
                                    stmts.push(exec_stmt);
                                    let work = self.prepared(shard_id, Work::Stmts(stmts)).await?;
                                    shard_futs.push((shard_id, work));
                                }

                                let replies = futures::future::join_all(shard_futs.into_iter().map(|(s, w)| {
                                    let client = self.cluster.shard(s);
                                    let sid = self.id;
                                    async move { client.run(sid, w).await }
                                })).await;

                                for reply in replies {
                                    let r = reply?;
                                    if let Some(e) = r.first_error() {
                                        return Err(e.to_sql());
                                    }
                                }
                                self.bump(&target_id);
                                self.plan.push(format!("procedure {} pushed down to shards", ex.name.join(".")));
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }

        // Case 2: Scalar partitioned procedure auto-route
        if table_args.is_empty() && !self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
            let tables = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let lookup = |i: &str| tables.get(i).cloned();
            for p in &def.params {
                if let Ok(route) = procs::validate(&def.params, &p.name, &def.body, &lookup, &self.cluster) {
                    self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), route);
                    return self.exec_routed_proc(ex, events).await;
                }
            }
        }

        Ok(false)
    }
}

fn collect_writes(stmts: &[Stmt], writes: &mut Vec<Name>) {
    for s in stmts {
        match s {
            Stmt::Insert(ins) => {
                if let Target::Table(name) = &ins.target {
                    writes.push(name.clone());
                }
            }
            Stmt::Update(u) => {
                if let Target::Table(name) = &u.target {
                    writes.push(name.clone());
                }
            }
            Stmt::Delete(d) => {
                if let Target::Table(name) = &d.target {
                    writes.push(name.clone());
                }
            }
            Stmt::Block(b) => collect_writes(b, writes),
            Stmt::While { body, .. } => collect_writes(std::slice::from_ref(body), writes),
            Stmt::If { then, els, .. } => {
                collect_writes(std::slice::from_ref(then), writes);
                if let Some(e) = els {
                    collect_writes(std::slice::from_ref(e), writes);
                }
            }
            _ => {}
        }
    }
}
