//! Stored procedures that run on one shard.
//!
//! A cluster runs a transaction as statements the client sends one at a time, each a round trip to a shard. A procedure whose every
//! statement stays on one shard can instead be sent whole: `DISTRIBUTE PROCEDURE name BY @param` declares that the value of the
//! parameter `@param` is the shard key of every row the procedure reads or writes, and `EXEC name ...` is then one request to the
//! shard that owns that value (inside the session's transaction when there is one, so that a transaction can call several, on several
//! shards, and commit them together).
//!
//! That is only safe when it is true, so the declaration is *checked* against the body: each statement must be of a kind the check
//! understands, and each read or write of a sharded table must be fixed to the parameter by an equality on the table's shard key (the
//! same test that routes a join: `where_key = @param`, or the key equal to the key of a table that is), an `INSERT` must give the key
//! the parameter, and a table that is replicated may be read but not written. Anything else (subqueries, derived tables, `TRY`,
//! `EXEC`, cursors, dynamic SQL, a write to a replicated table, a statement the check does not know) refuses the declaration, with the
//! statement named; such a procedure is not routed, and a call of it is not supported.
use crate::astutil::*;
use crate::*;
use serde_json::Value as J;

/// How a procedure is routed: the position of the parameter that is the shard key, how it is hashed, whether it returns rows, and the
/// tables it writes (their versions are bumped after a call).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcRoute {
    pub param: usize,
    pub class: KeyClass,
    pub rows: bool,
    pub writes: Vec<String>,
}

fn var_name(s: &str) -> String {
    s.trim_start_matches('@').to_lowercase()
}

/// The value of the key parameter in a statement or query as a literal (any value of the class: all that matters is that every sharded
/// table is fixed to *the same* value).
fn with_param_as_literal(v: &mut J, var: &str, lit: &J) {
    match v {
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(J::String(name)) = m.get("Var") {
                    if var_name(name) == var {
                        *v = lit.clone();
                        return;
                    }
                }
            }
            for c in m.values_mut() {
                with_param_as_literal(c, var, lit);
            }
        }
        J::Array(a) => {
            for c in a {
                with_param_as_literal(c, var, lit);
            }
        }
        _ => {}
    }
}

struct Check<'a> {
    var: String,
    lit: J,
    tables: &'a dyn Fn(&str) -> Option<DistTable>,
    cluster: &'a Cluster,
    rows: bool,
    writes: Vec<String>,
}

impl Check<'_> {
    fn expr_ok(&self, e: &Expr, what: &str) -> Result<(), String> {
        if has_subquery(e) {
            return Err(format!("{} has a subquery", what));
        }
        Ok(())
    }

    /// Whether every sharded table the query reads is fixed to the parameter's shard (the replicated tables are everywhere).
    fn query_pinned(&self, q: &Query) -> Result<(), String> {
        let mut v = serde_json::to_value(q).map_err(|e| e.to_string())?;
        with_param_as_literal(&mut v, &self.var, &self.lit);
        let q2: Query = serde_json::from_value(v).map_err(|e| e.to_string())?;
        if !q2.ctes.is_empty() {
            return Err("a query with a CTE".into());
        }
        // a derived table in the FROM is a query of its own, which has to be fixed to the parameter too; the tables that are left are
        // checked as they are (a derived table is not looked into by the test that pins a join, nor does it need to be)
        if let QueryBody::Select(sel) = &q2.body {
            if sel.from.iter().any(|t| matches!(t, TableRef::Derived { .. })) {
                let mut rest = q2.clone();
                let mut inner: Vec<Query> = vec![];
                if let QueryBody::Select(rs) = &mut rest.body {
                    let (derived, others): (Vec<TableRef>, Vec<TableRef>) = std::mem::take(&mut rs.from).into_iter().partition(|t| matches!(t, TableRef::Derived { .. }));
                    for d in derived {
                        if let TableRef::Derived { query, .. } = d {
                            inner.push(*query);
                        }
                    }
                    rs.from = others;
                }
                for q in &inner {
                    self.query_pinned(q).map_err(|why| format!("a derived table: {}", why))?;
                }
                let QueryBody::Select(rs) = &rest.body else { unreachable!() };
                if rs.from.is_empty() {
                    return Ok(());
                }
                if rs.where_.as_ref().map_or(false, |w| has_subquery(w)) {
                    return Err("a condition with a subquery".into());
                }
                return self.query_pinned_flat(&rest);
            }
        }
        self.query_pinned_flat(&q2)
    }

    /// `query_pinned` for a query with the parameter already a literal and no derived table at its top level.
    fn query_pinned_flat(&self, q2: &Query) -> Result<(), String> {
        if has_nested_query(q2) {
            return Err("a query with a subquery or a derived table".into());
        }
        let mut map = HashMap::new();
        for id in referenced_tables(q2) {
            let Some(t) = (self.tables)(&id) else { return Err(format!("the table {} is not a table of the cluster", id)) };
            map.insert(id, t);
        }
        if !map.values().any(|t| matches!(t.dist, Dist::Sharded { .. })) {
            return Ok(());
        }
        if crate::pin_shard(q2, &map, self.cluster).is_some() {
            Ok(())
        } else {
            Err("it is not fixed to the shard of the parameter by an equality on every sharded table's shard key".into())
        }
    }

    /// What the control node does around a write, which a procedure on a shard would skip: it numbers IDENTITY columns, enforces foreign
    /// keys, and adds the entries of global indexes. A table with any of those cannot be written by a procedure that runs on a shard.
    fn writable(&self, t: &DistTable) -> Result<(), String> {
        let name = t.name.join(".");
        if t.identity.is_some() {
            return Err(format!("it writes {}, which has an IDENTITY column (the control node numbers it)", name));
        }
        if !t.indexes.is_empty() {
            return Err(format!("it writes {}, which has a global index (the control node keeps it)", name));
        }
        if !self.cluster.fks_of_child(&t.id).is_empty() || !self.cluster.fks_of_parent(&t.id).is_empty() {
            return Err(format!("it writes {}, which is in a foreign key (the control node enforces them)", name));
        }
        Ok(())
    }

    fn write_pinned(&mut self, name: &Name, where_: Option<&Expr>) -> Result<(), String> {
        let id = table_id(name);
        let Some(t) = (self.tables)(&id) else { return Err(format!("the table {} is not a table of the cluster", name.join("."))) };
        if matches!(t.dist, Dist::Replicated) {
            return Err(format!("it writes the replicated table {} (that is every shard's to write)", name.join(".")));
        }
        self.writable(&t)?;
        let pseudo = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: Expr::Lit(Value::Int(1)), alias: None }], into: None, from: vec![TableRef::Table { name: name.clone(), alias: None, hints: vec![] }], where_: where_.cloned(), group: None, having: None });
        self.query_pinned(&pseudo)?;
        if !self.writes.contains(&id) {
            self.writes.push(id);
        }
        Ok(())
    }

    fn stmt(&mut self, st: &Stmt) -> Result<(), String> {
        match st {
            Stmt::Query(q) => {
                let QueryBody::Select(sel) = &q.body else { return Err("a set operation".into()) };
                if sel.into.is_some() {
                    return Err("SELECT ... INTO".into());
                }
                let mut assigns = false;
                for it in &sel.items {
                    match it {
                        SelectItem::Expr { e, .. } => self.expr_ok(e, "a select item")?,
                        SelectItem::Assign { var, e, .. } => {
                            if var_name(var) == self.var {
                                return Err("it assigns the key parameter".into());
                            }
                            assigns = true;
                            self.expr_ok(e, "a select item")?
                        }
                        SelectItem::Star(_) => {}
                    }
                }
                if let Some(w) = &sel.where_ {
                    self.expr_ok(w, "a condition")?;
                }
                if !assigns {
                    self.rows = true;
                }
                if sel.from.is_empty() {
                    return Ok(());
                }
                self.query_pinned(q)
            }
            Stmt::Insert(ins) => {
                let Target::Table(name) = &ins.target else { return Err("an INSERT into a table variable".into()) };
                let InsertSrc::Values(rows) = &ins.src else { return Err("an INSERT that is not VALUES".into()) };
                if ins.output.is_some() || ins.top.is_some() || !ins.ctes.is_empty() {
                    return Err("an INSERT with OUTPUT, TOP or CTEs".into());
                }
                let id = table_id(name);
                let Some(t) = (self.tables)(&id) else { return Err(format!("the table {} is not a table of the cluster", name.join("."))) };
                let Dist::Sharded { key, .. } = &t.dist else { return Err(format!("it writes the replicated table {}", name.join("."))) };
                self.writable(&t)?;
                let key_name = t.cols[*key].name.clone();
                let names: Vec<String> = match &ins.cols {
                    Some(c) => c.clone(),
                    None => t.cols.iter().filter(|c| !c.computed).map(|c| c.name.clone()).collect(),
                };
                let Some(pos) = names.iter().position(|c| c.eq_ignore_ascii_case(&key_name)) else { return Err(format!("an INSERT into {} that does not give the shard key {}", name.join("."), key_name)) };
                for r in rows {
                    for e in r {
                        self.expr_ok(e, "a value")?;
                    }
                    match r.get(pos) {
                        Some(Expr::Var(v)) if var_name(v) == self.var => {}
                        _ => return Err(format!("an INSERT into {} whose shard key {} is not the parameter", name.join("."), key_name)),
                    }
                }
                if !self.writes.contains(&id) {
                    self.writes.push(id);
                }
                Ok(())
            }
            Stmt::Update(u) => {
                let Target::Table(name) = &u.target else { return Err("an UPDATE of a table variable".into()) };
                if !u.from.is_empty() || u.output.is_some() || u.top.is_some() || u.current_of.is_some() || !u.ctes.is_empty() {
                    return Err("an UPDATE with FROM, OUTPUT, TOP, CURRENT OF or CTEs".into());
                }
                for s in &u.sets {
                    match s {
                        SetItem::Col { e, .. } => self.expr_ok(e, "a SET")?,
                        SetItem::Var { .. } => return Err("an UPDATE that assigns a variable".into()),
                    }
                }
                if let Some(w) = &u.where_ {
                    self.expr_ok(w, "a condition")?;
                }
                self.write_pinned(name, u.where_.as_ref())
            }
            Stmt::Delete(d) => {
                let Target::Table(name) = &d.target else { return Err("a DELETE from a table variable".into()) };
                if !d.from.is_empty() || d.output.is_some() || d.top.is_some() || d.current_of.is_some() || !d.ctes.is_empty() {
                    return Err("a DELETE with FROM, OUTPUT, TOP, CURRENT OF or CTEs".into());
                }
                if let Some(w) = &d.where_ {
                    self.expr_ok(w, "a condition")?;
                }
                self.write_pinned(name, d.where_.as_ref())
            }
            Stmt::Declare(decls) => {
                for d in decls {
                    match d {
                        Decl::Var { default: Some(e), .. } => self.expr_ok(e, "a DECLARE")?,
                        Decl::Var { .. } => {}
                        _ => return Err("a table or cursor variable".into()),
                    }
                }
                Ok(())
            }
            Stmt::Set { var, e, .. } => {
                if var_name(var) == self.var {
                    return Err("it assigns the key parameter".into());
                }
                self.expr_ok(e, "a SET")
            }
            Stmt::If { cond, then, els } => {
                self.expr_ok(cond, "an IF condition")?;
                self.stmt(then)?;
                if let Some(e) = els {
                    self.stmt(e)?;
                }
                Ok(())
            }
            Stmt::While { cond, body } => {
                self.expr_ok(cond, "a WHILE condition")?;
                self.stmt(body)
            }
            Stmt::Block(b) => b.iter().try_for_each(|s| self.stmt(s)),
            Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::Rollback(_) | Stmt::Return(None) | Stmt::SetOpts(_) | Stmt::Break | Stmt::Continue | Stmt::Noop => Ok(()),
            Stmt::Return(Some(e)) => self.expr_ok(e, "a RETURN"),
            Stmt::Print(e) => self.expr_ok(e, "a PRINT"),
            other => Err(format!("the statement {}", stmt_kind(other))),
        }
    }
}

/// Check a procedure's body against the declaration that parameter `param` is the shard key of everything it touches.
pub fn validate(params: &[Param], param: &str, body: &[Stmt], tables: &dyn Fn(&str) -> Option<DistTable>, cluster: &Cluster) -> Result<ProcRoute, String> {
    let var = var_name(param);
    let Some(pos) = params.iter().position(|p| var_name(&p.name) == var) else { return Err(format!("the procedure has no parameter {}", param)) };
    let ty = params[pos].ty.parts.last().map(|s| s.to_lowercase()).unwrap_or_default();
    let class = KeyClass::of_type_name(&ty).ok_or_else(|| format!("a parameter of type {} cannot be a shard key", ty))?;
    if params.iter().any(|p| p.out) {
        return Err("a procedure with OUTPUT parameters".into());
    }
    let lit = match class {
        KeyClass::Int => serde_json::json!({"Lit": {"Int": 0}}),
        KeyClass::Str => serde_json::json!({"Lit": {"Str": ""}}),
        KeyClass::Guid => return Err("a uniqueidentifier parameter".into()),
    };
    let mut c = Check { var, lit, tables, cluster, rows: false, writes: vec![] };
    for st in body {
        c.stmt(st).map_err(|why| format!("{}: {}", stmt_kind(st), why))?;
    }
    // the shard key classes of the tables it touches must be the parameter's (a string key is hashed differently from an integer)
    Ok(ProcRoute { param: pos, class, rows: c.rows, writes: c.writes })
}
