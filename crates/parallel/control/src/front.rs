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
//! end: OUTPUT parameters (other than those of `sp_executesql`), and the T-SQL `BULK INSERT` statement. `INSERT BULK` (the
//! protocol's bulk load) works: the listener turns its rows into `INSERT` statements, which come here.
use crate::astutil::*;
use crate::*;
use std::collections::HashMap;
use tsql_server::{Interceptor, Streamed};

/// Whether any batch or stored module this process has seen mentions `XACT_STATE`. Until one does, a statement in a transaction needs no
/// substitution of its value (which costs a round trip through JSON for every statement of a transaction).
static XACT_STATE_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Note that `text` mentions `XACT_STATE` (case apart), if it does.
pub(crate) fn note_xact_state(text: &str) {
    if XACT_STATE_SEEN.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let pat = b"xact_state";
    if text.as_bytes().windows(pat.len()).any(|w| w.eq_ignore_ascii_case(pat)) {
        XACT_STATE_SEEN.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// As [`note_xact_state`] for a schema log (the procedures, functions and triggers it holds).
pub(crate) fn note_xact_state_in_log(log: &[Stmt]) {
    if let Ok(j) = serde_json::to_string(log) {
        note_xact_state(&j);
    }
}

/// The words of a batch that can name a table: not those of comments, and not variables (`@ErrorLog` is not the table `ErrorLog`). The
/// contents of string literals are left out when `literals` is false: in a batch that runs no dynamic SQL a table's name in a string
/// (`WHERE name = 'orders'`) is a value.
fn identifiers(sql: &str, literals: bool) -> Vec<String> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < c.len() {
        if !literals && c[i] == '\'' {
            i += 1;
            while i < c.len() {
                if c[i] == '\'' {
                    if c.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c[i] == '-' && c.get(i + 1) == Some(&'-') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c[i] == '@' {
            // a variable (or @@function): its whole name
            i += 1;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '@') {
                i += 1;
            }
        } else if c[i].is_alphanumeric() || c[i] == '_' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            out.push(c[start..i].iter().collect());
        } else {
            i += 1;
        }
    }
    out
}

impl ClusterSession {
    /// Whether a batch needs the cluster (else the engine runs it itself).
    pub fn wants(&self, sql: &str) -> bool {
        let parsed = tsql_server::parser::parse(sql).ok();
        // a system procedure that reads the catalog (`EXEC sp_fkeys null, null, null, N'Person', N'Person', N'advw'`, which SSMS runs for a
        // table's relationships) is the engine's: the names it is given are values, and the engine holds the schema of every table
        if let Some(stmts) = &parsed {
            if !stmts.is_empty() && stmts.iter().all(|s| matches!(s, Stmt::Exec(ex) if self.is_catalog_proc(ex))) {
                return false;
            }
        }
        let understood = parsed.as_ref().is_some_and(|stmts| stmts.iter().any(|s| self.stmt_wants(s)));
        // a parameterized call (what drivers send, and SSMS): `DECLARE @rc; EXEC @rc = sp_executesql N'text', N'declaration', @p = N'value'; SELECT @rc`.
        // Only its text can name a table: a parameter value that is the name of one (SSMS passes the name of the table it is asking about) is
        // not a mention of it
        if let Some(stmts) = &parsed {
            let wrapper = stmts.iter().all(|s| match s {
                Stmt::Declare(_) => true,
                Stmt::Exec(ex) => self.executesql_text(ex).is_some(),
                Stmt::Query(q) => referenced_tables(q).is_empty(),
                _ => false,
            });
            if wrapper && stmts.iter().any(|s| matches!(s, Stmt::Exec(_))) {
                return understood || stmts.iter().any(|s| matches!(s, Stmt::Exec(ex) if self.executesql_text(ex).is_some_and(|t| self.mentions_distributed_table(&t))));
            }
        }
        // The control node's own engine holds the schema of the distributed tables but none of their rows, so a batch that
        // mentions one must not be run there by mistake: if it is not understood, the cluster session says it is not
        // supported (an error) instead of the engine answering from an empty table.
        understood || self.mentions_distributed_table(sql)
    }

    /// Whether an `EXEC` is of a system procedure (`sp_...`, `xp_...`) other than `sp_executesql`, and not one the cluster runs itself.
    fn is_catalog_proc(&self, ex: &ExecStmt) -> bool {
        let Some(n) = ex.name.last().map(|n| n.to_lowercase()) else { return false };
        // (an sp_rename of an index or a column of a table of the cluster is the cluster's: see renames.rs)
        if self.is_cluster_rename(ex) || renames::is_security_proc(ex) {
            return false;
        }
        (n.starts_with("sp_") || n.starts_with("xp_")) && n != "sp_executesql" && !ex.name_is_var && self.cluster.proc_def(&self.qid(&ex.name)).is_none() && !self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&self.qid(&ex.name))
    }

    fn mentions_distributed_table(&self, sql: &str) -> bool {
        let tables = self.cluster.tables();
        let views: Vec<String> = self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).keys().map(|k| k.rsplit('.').next().unwrap_or("").to_string()).collect();
        if tables.is_empty() && views.is_empty() {
            return false;
        }
        // (a batch that has EXEC may run a string as SQL: then the strings count)
        let runs_strings = words(sql).iter().any(|w| w == "exec" || w == "execute");
        identifiers(sql, runs_strings).iter().any(|w| tables.iter().any(|t| t.name.last().map(|n| n.eq_ignore_ascii_case(w)).unwrap_or(false)) || views.iter().any(|v| v.eq_ignore_ascii_case(w)))
    }

    fn is_view(&self, id: &str) -> bool {
        self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(id)
    }

    fn stmt_wants(&self, st: &Stmt) -> bool {
        // (the engine here holds no rows: a statement that calls a function that reads tables must not be run by it)
        if self.reading_function_tables(st).is_some() || self.calls_function_reading_tables(st) {
            return true;
        }
        match st {
            // (control of flow: what its branches run decides, so that `IF NOT EXISTS (...) CREATE DATABASE x` or `IF ... DROP TABLE t` is the
            // cluster's, as the statement alone would be; the condition's own tables are found by the text of the batch)
            Stmt::If { then, els, .. } => self.stmt_wants(then) || els.as_ref().is_some_and(|e| self.stmt_wants(e)),
            Stmt::Block(b) => b.iter().any(|s| self.stmt_wants(s)),
            Stmt::While { body, .. } => self.stmt_wants(body),
            Stmt::Try { body, catch } => body.iter().chain(catch.iter()).any(|s| self.stmt_wants(s)),
            // (every procedure is the cluster's to make, whether or not it names a table: one that calls another, and takes its OUTPUT parameters, runs on the
            // control node, which can only run a procedure it knows; a procedure the engine alone held would be called without the cluster's variables)
            Stmt::CreateProc { name, .. } if !is_temp_name(name) => true,
            Stmt::Drop { what, names, .. } if (what.eq_ignore_ascii_case("PROCEDURE") || what.eq_ignore_ascii_case("PROC")) && names.iter().any(|n| !is_temp_name(n)) => true,
            Stmt::CreateFunction(_) | Stmt::CreateTrigger { .. } | Stmt::TriggerAdmin(_) => true,
            Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("TRIGGER") => true,
            Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("FUNCTION") => true,
            Stmt::CreateTable { .. } | Stmt::AlterTable { .. } | Stmt::CreateIndex { .. } | Stmt::DropIndex { .. } | Stmt::AlterIndex { .. } | Stmt::Truncate(_) | Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::Rollback(_) | Stmt::SaveTran(_) => {
                // (a temporary table that a batch makes is the engine's own, until a statement needs it with the cluster's tables: see `temps_wanted`)
                self.touches_distributed(st)
                    || match st {
                        Stmt::CreateTable { name, .. } => !is_temp_name(name),
                        Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::Rollback(_) | Stmt::SaveTran(_) => true,
                        _ => false,
                    }
            }
            // a database is made and dropped on every shard too (see dbname.rs)
            Stmt::CreateDatabase(..) => true,
            Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("DATABASE") => true,
            Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("VIEW") => true,
            Stmt::CreateView { .. } => true,
            Stmt::Drop { what, names, .. } => what.eq_ignore_ascii_case("TABLE") && names.iter().any(|n| !is_temp_name(n) || self.table_by_id(&table_id(n)).is_some()),
            Stmt::Query(q) => {
                // (the tables a client names are those of the database it is in)
                let q = match self.qualify(st) {
                    Stmt::Query(q2) => q2,
                    _ => q.clone(),
                };
                referenced_tables(&q).iter().any(|t| self.table_by_id(t).is_some() || self.is_view(t))
            }
            Stmt::Exec(ex) if self.is_cluster_rename(ex) => true,
            // (users, roles, their members and permissions are kept in the schema log, so that a control node that starts again has them: not logins, which
            // carry passwords and are the engine's alone)
            Stmt::Exec(ex) if renames::is_security_proc(ex) => true,
            Stmt::Security(sec) => !matches!(&**sec, SecStmt::CreateLogin { .. } | SecStmt::AlterLogin { .. } | SecStmt::ExecuteAs { .. } | SecStmt::Revert),
            Stmt::Exec(ex) => match self.executesql_text(ex) {
                Some(inner) => self.wants(&inner),
                // (a procedure of the cluster: its body reads the cluster's tables, which the control node's engine holds none of the rows of)
                None => !ex.name_is_var && self.cluster.proc_def(&self.qid(&ex.name)).is_some(),
            },
            // a restore puts the backup's tables on the shards (see restore.rs); a backup of the cluster's database is refused there
            // (the engine here holds none of its rows). The other RESTOREs (HEADERONLY, VERIFYONLY) only read the file.
            Stmt::Restore(r) => r.kind == RestoreKind::Database,
            Stmt::Backup(b) => b.db.eq_ignore_ascii_case(&self.cur_db),
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
        let xact_read = XACT_STATE_SEEN.load(std::sync::atomic::Ordering::Relaxed) && self.txn.borrow().is_some() && matches!(st, Stmt::Query(_) | Stmt::Set { .. } | Stmt::If { .. } | Stmt::While { .. } | Stmt::Print(_));
        if self.vars.is_empty() && self.last_identity.is_none() && self.catch_stack.is_empty() && self.eventdata.is_empty() && self.cursors.is_empty() && !xact_read {
            return Ok(st.clone());
        }
        // ERROR_NUMBER() and the like (in a CATCH block), XACT_STATE(): the control node's engine has no CATCH block, so the values go in
        let caught = self.catch_stack.last().cloned();
        let xact = match self.txn.borrow().as_ref() {
            None => 0,
            Some(t) if t.doomed => -1,
            Some(_) => 1,
        };
        // SCOPE_IDENTITY() and @@IDENTITY: the last value this session was given
        let ident = self.last_identity.map(|v| serde_json::to_value(Value::Dec(tsql_server::decimal::Dec::from_int(v))).unwrap_or(serde_json::Value::Null));
        // EVENTDATA() in a DDL trigger: the event's XML
        let event = self.eventdata.last().and_then(|x| serde_json::to_value(Expr::Cast { e: Box::new(Expr::Lit(Value::Str(x.clone()))), ty: TypeRef { parts: vec!["xml".into()], args: vec![], xml_schema: None }, try_: false, style: None }).ok());
        fn subst(v: &mut serde_json::Value, vars: &HashMap<String, Value>, ident: &Option<serde_json::Value>, caught: &Option<SqlError>, xact: i32, event: &Option<serde_json::Value>, fetch: i32) {
            match v {
                serde_json::Value::Object(m) => {
                    let is_ident = match (m.len(), m.get("SysVar"), m.get("Func")) {
                        (1, Some(serde_json::Value::String(s)), _) => s.eq_ignore_ascii_case("@@IDENTITY"),
                        (1, _, Some(serde_json::Value::Object(f))) => f.get("name").and_then(|n| n.as_str()).map(|n| n.eq_ignore_ascii_case("SCOPE_IDENTITY")).unwrap_or(false),
                        _ => false,
                    };
                    if let (1, Some(serde_json::Value::String(s))) = (m.len(), m.get("SysVar")) {
                        if s.eq_ignore_ascii_case("@@FETCH_STATUS") {
                            *v = serde_json::json!({ "Lit": serde_json::to_value(Value::Int(fetch)).unwrap_or(serde_json::Value::Null) });
                            return;
                        }
                    }
                    if is_ident {
                        let lit = ident.clone().unwrap_or_else(|| serde_json::to_value(Value::Null).unwrap_or(serde_json::Value::Null));
                        *v = serde_json::json!({ "Lit": lit });
                        return;
                    }
                    if let (1, Some(serde_json::Value::Object(f))) = (m.len(), m.get("Func")) {
                        let func = f.get("name").and_then(|n| n.as_str()).map(|n| n.to_uppercase()).unwrap_or_default();
                        let is_call = f.get("args").and_then(|a| a.as_array()).is_some_and(|a| a.is_empty());
                        if is_call && func == "EVENTDATA" {
                            if let Some(e) = event {
                                *v = e.clone();
                                return;
                            }
                        }
                        let value = if !is_call {
                            None
                        } else {
                            match func.as_str() {
                                "XACT_STATE" => Some(Value::Int(xact)),
                                "ERROR_NUMBER" => Some(caught.as_ref().map(|e| Value::Int(e.number)).unwrap_or(Value::Null)),
                                "ERROR_MESSAGE" => Some(caught.as_ref().map(|e| Value::Str(e.message.clone())).unwrap_or(Value::Null)),
                                "ERROR_SEVERITY" => Some(caught.as_ref().map(|e| Value::Int(e.severity as i32)).unwrap_or(Value::Null)),
                                "ERROR_STATE" => Some(caught.as_ref().map(|e| Value::Int(e.state as i32)).unwrap_or(Value::Null)),
                                "ERROR_LINE" => Some(caught.as_ref().map(|_| Value::Int(1)).unwrap_or(Value::Null)),
                                "ERROR_PROCEDURE" => Some(caught.as_ref().and_then(|e| e.procedure.clone()).map(Value::Str).unwrap_or(Value::Null)),
                                _ => None,
                            }
                        };
                        if let Some(j) = value.and_then(|x| serde_json::to_value(x).ok()) {
                            *v = serde_json::json!({ "Lit": j });
                            return;
                        }
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
                        subst(c, vars, ident, caught, xact, event, fetch);
                    }
                }
                serde_json::Value::Array(a) => {
                    for c in a {
                        subst(c, vars, ident, caught, xact, event, fetch);
                    }
                }
                _ => {}
            }
        }
        let mut j = serde_json::to_value(st).map_err(|e| SqlError::new(50000, e.to_string()))?;
        subst(&mut j, &self.vars, &ident, &caught, xact, &event, self.fetch_status);
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

/// Whether a name is that of a temporary table (`#t`).
fn is_temp_name(n: &Name) -> bool {
    n.last().is_some_and(|x| x.starts_with('#'))
}

/// The words of a batch, in lower case, with the names of temporary tables (`#t`) as one word each; comments are left out and
/// variables (`@x`) are not words. For finding the temporary tables a batch makes, drops and mentions.
fn words(sql: &str) -> Vec<String> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < c.len() {
        if c[i] == '-' && c.get(i + 1) == Some(&'-') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c[i] == '@' {
            i += 1;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '@') {
                i += 1;
            }
        } else if c[i].is_alphanumeric() || c[i] == '_' || c[i] == '#' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '#') {
                i += 1;
            }
            out.push(c[start..i].iter().collect::<String>().to_lowercase());
        } else {
            i += 1;
        }
    }
    out
}

/// The interceptor of a connection of the control node's TDS listener.
struct ControlInterceptor {
    session: ClusterSession,
    /// The temporary tables (`#t`, lower case) that the connection's own engine has: made by a batch the cluster did not take. They stay
    /// the engine's (a batch that reads them with the catalog, as SSMS's scripts do, runs there); one that a batch needs with the cluster's
    /// tables is handed over (see `temps_wanted`).
    locals: std::collections::HashSet<String>,
}

impl ControlInterceptor {
    /// Note the temporary tables a batch that the engine runs makes (`CREATE TABLE #t`, `SELECT ... INTO #t`) and drops.
    fn note_locals(&mut self, sql: &str) {
        let w = words(sql);
        for (i, x) in w.iter().enumerate() {
            if !x.starts_with('#') {
                continue;
            }
            let before = |k: usize| if i >= k { w[i - k].as_str() } else { "" };
            if (before(2) == "create" && before(1) == "table") || (before(1) == "into" && before(2) != "insert" && before(2) != "merge") {
                self.locals.insert(x.clone());
            } else if (before(2) == "drop" && before(1) == "table") || (before(4) == "drop" && before(3) == "table" && before(2) == "if" && before(1) == "exists") {
                self.locals.remove(x);
            }
        }
    }
}

impl Interceptor for ControlInterceptor {
    fn set_database(&mut self, db: &str) -> Option<String> {
        self.session.set_current_database(db);
        // (a database that was dropped while the connection was in it: the connection is in master now, the engine's own session too)
        if !self.session.current_database().eq_ignore_ascii_case(db) {
            return Some(self.session.current_database().to_string());
        }
        None
    }

    fn database_after(&mut self) -> Option<String> {
        // (a batch the cluster ran that said `USE db`: the connection's engine is in it too, from the next batch)
        Some(self.session.current_database().to_string())
    }

    fn temps_wanted(&mut self, sql: &str) -> Vec<String> {
        if self.locals.is_empty() || !self.session.wants(sql) {
            return vec![];
        }
        let mentioned: std::collections::HashSet<String> = words(sql).into_iter().filter(|x| x.starts_with('#')).collect();
        let mut names: Vec<String> = self.locals.iter().filter(|n| mentioned.contains(*n)).cloned().collect();
        names.sort();
        for n in &names {
            self.locals.remove(n);
        }
        names
    }

    fn adopt_temp(&mut self, name: &str, create: &str, rows: Vec<Vec<Value>>) -> Result<(), SqlError> {
        // a replicated table: small, and one that a client made for its own use, with the keys and indexes it likes
        self.session.create_dist = Some(restore::DistChoice::Replicated);
        let r = self.session.execute(create);
        self.session.create_dist = None;
        if let Some(e) = r.first_error() {
            return Err(e.clone());
        }
        let target = format!("[{}]", name);
        for chunk in rows.chunks(200) {
            let values: Vec<String> = chunk.iter().map(|r| format!("({})", r.iter().map(tsql_server::tds::literal).collect::<Vec<_>>().join(", "))).collect();
            let r = self.session.execute(&format!("INSERT {} VALUES {}", target, values.join(", ")));
            if let Some(e) = r.first_error() {
                return Err(e.clone());
            }
        }
        Ok(())
    }

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
            self.note_locals(sql);
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
        self.mirror.set_interceptor(Some(Arc::new(move || Box::new(ControlInterceptor { session: cluster.session(), locals: Default::default() }) as Box<dyn Interceptor>)));
    }
}
