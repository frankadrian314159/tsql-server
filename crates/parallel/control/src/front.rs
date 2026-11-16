//! The front end of a control node for ordinary SQL Server clients.
//!
//! The control node's own engine (the mirror, which holds the schema) is served over TDS by the single-node listener
//! (`tsql_server::tds::Listener`), so logins, TLS, prepared statements, MARS and all the protocol work as in `tsql-serve`.
//! What differs is what a batch does: an [`Interceptor`] installed on that engine looks at each batch, sends the ones that
//! touch distributed tables (or change them, or begin and end transactions) to a [`ClusterSession`], and declines the
//! others (`SET` options, `SELECT @@VERSION`, system views, ...), which the engine then runs itself.
//!
//! Parameterized statements (what drivers send as `sp_executesql`, and what prepared statements become) are run by binding
//! their parameters as variables of the session and substituting them in the statement. Not supported through the front
//! end: OUTPUT parameters, server cursors, table-valued parameters and bulk load.
use crate::astutil::*;
use crate::*;
use std::collections::HashMap;
use tsql_server::{Interceptor, Streamed};

impl ClusterSession {
    /// Whether a batch needs the cluster (else the engine runs it itself).
    pub fn wants(&self, sql: &str) -> bool {
        let understood = match tsql_server::parser::parse(sql) {
            Ok(stmts) => stmts.iter().any(|s| self.stmt_wants(s)),
            Err(_) => false,
        };
        // The control node's own engine holds the schema of the distributed tables but none of their rows, so a batch that
        // mentions one must not be run there by mistake: if it is not understood, the cluster session says it is not
        // supported (an error) instead of the engine answering from an empty table.
        understood || self.mentions_distributed_table(sql)
    }

    fn mentions_distributed_table(&self, sql: &str) -> bool {
        let tables = self.cluster.tables();
        if tables.is_empty() {
            return false;
        }
        sql.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).any(|w| tables.iter().any(|t| t.name.last().map(|n| n.eq_ignore_ascii_case(w)).unwrap_or(false)))
    }

    fn stmt_wants(&self, st: &Stmt) -> bool {
        match st {
            Stmt::CreateTable { .. } | Stmt::CreateIndex { .. } | Stmt::DropIndex { .. } | Stmt::Truncate(_) | Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::Rollback(_) | Stmt::SaveTran(_) => {
                self.touches_distributed(st) || matches!(st, Stmt::CreateTable { .. } | Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::Rollback(_) | Stmt::SaveTran(_))
            }
            Stmt::Drop { what, .. } => what.eq_ignore_ascii_case("TABLE"),
            Stmt::Query(q) => referenced_tables(q).iter().any(|t| self.table_by_id(t).is_some()),
            Stmt::Exec(ex) => match self.executesql_text(ex) {
                Some(inner) => self.wants(&inner),
                None => false,
            },
            Stmt::Declare(_) | Stmt::Set { .. } => false,
            _ => false,
        }
    }

    fn touches_distributed(&self, st: &Stmt) -> bool {
        let names: Vec<&Name> = match st {
            Stmt::CreateIndex { table, .. } => vec![table],
            Stmt::Truncate(Target::Table(n)) => vec![n],
            Stmt::Insert(i) => match &i.target {
                Target::Table(n) => vec![n],
                _ => vec![],
            },
            Stmt::Update(u) => match &u.target {
                Target::Table(n) => vec![n],
                _ => vec![],
            },
            Stmt::Delete(d) => match &d.target {
                Target::Table(n) => vec![n],
                _ => vec![],
            },
            Stmt::DropIndex { .. } => return true,
            _ => vec![],
        };
        names.iter().any(|n| self.table_by_id(&table_id(n)).is_some())
    }

    /// The statement text of `EXEC sp_executesql N'...'` when it is a literal.
    fn executesql_text(&self, ex: &ExecStmt) -> Option<String> {
        if ex.name_is_var || !ex.name.last().map(|n| n.eq_ignore_ascii_case("sp_executesql")).unwrap_or(false) {
            return None;
        }
        ex.args.first().and_then(|a| lit_str(&a.e))
    }

    /// `st` with the variables of the session replaced by their values.
    pub(crate) fn bind_vars(&self, st: &Stmt) -> Result<Stmt, SqlError> {
        if self.vars.is_empty() {
            return Ok(st.clone());
        }
        fn subst(v: &mut serde_json::Value, vars: &HashMap<String, Value>) {
            match v {
                serde_json::Value::Object(m) => {
                    if m.len() == 1 {
                        if let Some(serde_json::Value::String(name)) = m.get("Var") {
                            if let Some(val) = vars.get(&name.to_lowercase()) {
                                if let Ok(j) = serde_json::to_value(val) {
                                    *v = serde_json::json!({ "Lit": j });
                                    return;
                                }
                            }
                        }
                    }
                    for c in m.values_mut() {
                        subst(c, vars);
                    }
                }
                serde_json::Value::Array(a) => {
                    for c in a {
                        subst(c, vars);
                    }
                }
                _ => {}
            }
        }
        let mut j = serde_json::to_value(st).map_err(|e| SqlError::new(50000, e.to_string()))?;
        subst(&mut j, &self.vars);
        serde_json::from_value(j).map_err(|e| SqlError::new(50000, format!("a variable could not be substituted: {}", e)))
    }

    /// `DECLARE @v ...`, `SET @v = ...` and `EXEC sp_executesql`: the variables of the session. (Handled before the
    /// variables of the statement are substituted, as their values may depend on earlier ones.)
    pub(crate) async fn exec_variable_stmt(&mut self, st: &Stmt) -> Result<bool, SqlError> {
        match st {
            Stmt::Declare(decls) => {
                for d in decls {
                    match d {
                        Decl::Var { name, default, .. } => {
                            let v = match default {
                                Some(e) => {
                                    let b = self.bind_expr(e)?;
                                    self.eval_scalar(&b).await?
                                }
                                None => Value::Null,
                            };
                            self.vars.insert(name.to_lowercase(), v);
                        }
                        _ => return Err(unsupported("table variables and cursors")),
                    }
                }
                Ok(true)
            }
            Stmt::Set { var, op: None, e } => {
                let b = self.bind_expr(e)?;
                let v = self.eval_scalar(&b).await?;
                self.vars.insert(var.to_lowercase(), v);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn bind_expr(&self, e: &Expr) -> Result<Expr, SqlError> {
        let st = Stmt::Set { var: "@x".into(), op: None, e: e.clone() };
        match self.bind_vars(&st)? {
            Stmt::Set { e, .. } => Ok(e),
            _ => Ok(e.clone()),
        }
    }

    /// `EXEC [@rc =] sp_executesql @stmt, @params, @p1 = v1, ...` with input parameters: the parameters become variables
    /// for the duration of the inner batch, which is then run like any batch.
    pub(crate) async fn exec_executesql(&mut self, ex: &ExecStmt, events: &mut Vec<Event>) -> Result<bool, SqlError> {
        let Some(text) = self.executesql_text(ex) else { return Ok(false) };
        let mut declared: Vec<String> = vec![];
        let mut rest = &ex.args[1..];
        if let Some(a) = rest.first() {
            if a.name.as_deref().map(|n| n.eq_ignore_ascii_case("@params")).unwrap_or(true) {
                if let Some(d) = lit_str(&a.e) {
                    declared = tsql_server::parser::parse_param_decls(&d)?.into_iter().map(|p| p.name).collect();
                    rest = &rest[1..];
                } else if matches!(a.e, Expr::Lit(Value::Null)) && a.name.is_none() {
                    rest = &rest[1..];
                }
            }
        }
        let mut bound: Vec<(String, Option<Value>)> = vec![];
        for (k, a) in rest.iter().enumerate() {
            if a.out {
                return Err(unsupported("OUTPUT parameters through the front end"));
            }
            let name = match &a.name {
                Some(n) => n.clone(),
                None => declared.get(k).cloned().ok_or_else(|| SqlError::new(8144, "Procedure or function sp_executesql has too many arguments specified."))?,
            };
            let b = self.bind_expr(&a.e)?;
            let v = self.eval_scalar(&b).await?;
            let key = if name.starts_with('@') { name.to_lowercase() } else { format!("@{}", name.to_lowercase()) };
            bound.push((key.clone(), self.vars.insert(key, v)));
        }
        let stmts = tsql_server::parser::parse(&text)?;
        let mut result = Ok(());
        for st in &stmts {
            if let Err(e) = Box::pin(self.exec_stmt(st, events)).await {
                result = Err(e);
                break;
            }
        }
        for (k, old) in bound {
            match old {
                Some(v) => {
                    self.vars.insert(k, v);
                }
                None => {
                    self.vars.remove(&k);
                }
            }
        }
        result?;
        if let Some(rc) = &ex.retvar {
            self.vars.insert(rc.to_lowercase(), Value::Int(0));
        }
        Ok(true)
    }
}

/// The text of a string literal (`N'...'` is a cast of one).
fn lit_str(e: &Expr) -> Option<String> {
    match e {
        Expr::Lit(Value::Str(s)) => Some(s.clone()),
        Expr::Cast { e, .. } => lit_str(e),
        Expr::Paren(e) => lit_str(e),
        _ => None,
    }
}

/// The interceptor of a connection of the control node's TDS listener.
struct ControlInterceptor {
    session: ClusterSession,
}

impl Interceptor for ControlInterceptor {
    fn handle(&mut self, sql: &str, sink: &mut dyn FnMut(Streamed) -> bool) -> bool {
        if !self.session.wants(sql) {
            return false;
        }
        let res = self.session.execute(sql);
        for ev in res.events {
            if !sink(Streamed::Event(ev)) {
                break;
            }
        }
        true
    }
}

impl Cluster {
    /// The control node's own engine (the mirror): serve it over TDS with `tsql_server::tds::Listener` after
    /// [`Cluster::enable_tds`].
    pub fn mirror_server(&self) -> Server {
        self.mirror.clone()
    }

    /// Make the batches of the connections made to the mirror from now on run on the cluster (see this module).
    pub fn enable_tds(self: &Arc<Self>) {
        let cluster = self.clone();
        self.mirror.set_interceptor(Some(Arc::new(move || Box::new(ControlInterceptor { session: cluster.session() }) as Box<dyn Interceptor>)));
    }
}
