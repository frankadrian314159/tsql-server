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
        let views: Vec<String> = self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).keys().map(|k| k.rsplit('.').next().unwrap_or("").to_string()).collect();
        if tables.is_empty() && views.is_empty() {
            return false;
        }
        sql.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).any(|w| tables.iter().any(|t| t.name.last().map(|n| n.eq_ignore_ascii_case(w)).unwrap_or(false)) || views.iter().any(|v| v.eq_ignore_ascii_case(w)))
    }

    fn is_view(&self, id: &str) -> bool {
        self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(id)
    }

    fn stmt_wants(&self, st: &Stmt) -> bool {
        match st {
            Stmt::CreateTable { .. } | Stmt::AlterTable { .. } | Stmt::CreateIndex { .. } | Stmt::DropIndex { .. } | Stmt::AlterIndex { .. } | Stmt::Truncate(_) | Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::Rollback(_) | Stmt::SaveTran(_) => {
                self.touches_distributed(st) || matches!(st, Stmt::CreateTable { .. } | Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::Rollback(_) | Stmt::SaveTran(_))
            }
            Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("VIEW") => true,
            Stmt::CreateView { .. } => true,
            Stmt::Drop { what, .. } => what.eq_ignore_ascii_case("TABLE"),
            Stmt::Query(q) => referenced_tables(q).iter().any(|t| self.table_by_id(t).is_some() || self.is_view(t)),
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
            Stmt::CreateIndex { table, .. } | Stmt::AlterIndex { table, .. } => vec![table],
            Stmt::AlterTable { name, .. } => vec![name],
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
    pub(crate) fn executesql_text(&self, ex: &ExecStmt) -> Option<String> {
        if ex.name_is_var || !ex.name.last().map(|n| n.eq_ignore_ascii_case("sp_executesql")).unwrap_or(false) {
            return None;
        }
        ex.args.first().and_then(|a| lit_str(&a.e))
    }

    /// `st` with the variables of the session replaced by their values.
    pub(crate) fn bind_vars(&self, st: &Stmt) -> Result<Stmt, SqlError> {
        if self.vars.is_empty() && self.last_identity.is_none() {
            return Ok(st.clone());
        }
        // SCOPE_IDENTITY() and @@IDENTITY: the last value this session was given
        let ident = self.last_identity.map(|v| serde_json::to_value(Value::Dec(tsql_server::decimal::Dec::from_int(v))).unwrap_or(serde_json::Value::Null));
        fn subst(v: &mut serde_json::Value, vars: &HashMap<String, Value>, ident: &Option<serde_json::Value>) {
            match v {
                serde_json::Value::Object(m) => {
                    let is_ident = match (m.len(), m.get("SysVar"), m.get("Func")) {
                        (1, Some(serde_json::Value::String(s)), _) => s.eq_ignore_ascii_case("@@IDENTITY"),
                        (1, _, Some(serde_json::Value::Object(f))) => f.get("name").and_then(|n| n.as_str()).map(|n| n.eq_ignore_ascii_case("SCOPE_IDENTITY")).unwrap_or(false),
                        _ => false,
                    };
                    if is_ident {
                        let lit = ident.clone().unwrap_or_else(|| serde_json::to_value(Value::Null).unwrap_or(serde_json::Value::Null));
                        *v = serde_json::json!({ "Lit": lit });
                        return;
                    }
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
                        subst(c, vars, ident);
                    }
                }
                serde_json::Value::Array(a) => {
                    for c in a {
                        subst(c, vars, ident);
                    }
                }
                _ => {}
            }
        }
        let mut j = serde_json::to_value(st).map_err(|e| SqlError::new(50000, e.to_string()))?;
        subst(&mut j, &self.vars, &ident);
        serde_json::from_value(j).map_err(|e| SqlError::new(50000, format!("a variable could not be substituted: {}", e)))
    }

    /// `DECLARE @v ...`, `SET @v = ...` and `EXEC sp_executesql`: the variables of the session. (Handled before the
    /// variables of the statement are substituted, as their values may depend on earlier ones.)
    pub(crate) async fn exec_variable_stmt(&mut self, st: &Stmt) -> Result<bool, SqlError> {
        match st {
            Stmt::Declare(decls) => {
                for d in decls {
                    match d {
                        Decl::Table { .. } => {
                            if !self.declare_table_var(d) {
                                return Err(SqlError::new(50000, "the table variable could not be declared"));
                            }
                        }
                        Decl::Var { default: None, .. } if self.declare_table_var(d) => {}
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

    /// Declare a table variable (`DECLARE @t TABLE (...)`, or `DECLARE @t dbo.SomeTableType`): the control node's own engine says
    /// what its columns are (it holds the schema and the table types) by declaring it and reading it in one batch, and the
    /// session keeps the variable. False when the declaration is not a table (an ordinary type).
    fn declare_table_var(&mut self, d: &Decl) -> bool {
        let name = match d {
            Decl::Table { name, .. } | Decl::Var { name, .. } => name.clone(),
            _ => return false,
        };
        let probe = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Star(None)], into: None, from: vec![TableRef::TableVar { name: name.clone(), alias: None }], where_: None, group: None, having: None });
        let res = self.mirror.execute_stmts(&[Stmt::Declare(vec![d.clone()]), Stmt::Query(Box::new(probe))]);
        if res.first_error().is_some() {
            return false;
        }
        match res.resultsets().first() {
            Some(rs) => {
                self.tvars.insert(name.to_lowercase(), ResultSet { columns: rs.columns.clone(), types: rs.types.clone(), rows: vec![] });
                true
            }
            None => false,
        }
    }

    pub(crate) fn bind_expr(&self, e: &Expr) -> Result<Expr, SqlError> {
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
        // OUTPUT parameters: (the parameter inside the statement, the variable that is passed)
        let mut outs: Vec<(String, String)> = vec![];
        let mut tvps: Vec<(String, Option<ResultSet>)> = vec![];
        for (k, a) in rest.iter().enumerate() {
            let name = match &a.name {
                Some(n) => n.clone(),
                None => declared.get(k).cloned().ok_or_else(|| SqlError::new(8144, "Procedure or function sp_executesql has too many arguments specified."))?,
            };
            let key = if name.starts_with('@') { name.to_lowercase() } else { format!("@{}", name.to_lowercase()) };
            // a table-valued parameter: the table variable that is passed; its rows are what the statement reads
            if let Expr::Var(outer) = &a.e {
                if let Some(rs) = self.tvars.get(&outer.to_lowercase()).cloned() {
                    let old = self.tvars.insert(key.clone(), rs);
                    tvps.push((key, old));
                    continue;
                }
            }
            if a.out {
                match &a.e {
                    Expr::Var(outer) => outs.push((key.clone(), outer.to_lowercase())),
                    _ => return Err(SqlError::new(179, "Cannot use the OUTPUT option when passing a constant to a stored procedure.")),
                }
            }
            let b = self.bind_expr(&a.e)?;
            let v = self.eval_scalar(&b).await?;
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
        // the values of the OUTPUT parameters go back to the variables they were passed as
        if result.is_ok() {
            for (inner, outer) in &outs {
                if let Some(v) = self.vars.get(inner).cloned() {
                    self.vars.insert(outer.clone(), v);
                }
            }
        }
        for (k, old) in tvps {
            match old {
                Some(rs) => {
                    self.tvars.insert(k, rs);
                }
                None => {
                    self.tvars.remove(&k);
                }
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
    fn handle_stmts(&mut self, stmts: &[Stmt], sink: &mut dyn FnMut(Streamed) -> bool) -> bool {
        if !stmts.iter().any(|s| self.session.stmt_wants(s)) {
            return false;
        }
        let res = self.session.execute_stmts(stmts);
        for ev in res.events {
            if !sink(Streamed::Event(ev)) {
                break;
            }
        }
        true
    }

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
