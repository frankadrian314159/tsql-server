//! Row-level security: `CREATE | ALTER | DROP SECURITY POLICY`.
//!
//! A policy attaches *predicates* (inline table-valued functions of some columns of the target table) to tables:
//!
//! * a **filter** predicate hides rows from every read (`SELECT`) and from `UPDATE` / `DELETE`: only rows for which the
//!   function returns a row are seen;
//! * **block** predicates stop writes: `AFTER INSERT`, `AFTER UPDATE` (the new row), `BEFORE UPDATE` and `BEFORE DELETE`
//!   (the existing row) must satisfy the function, or the statement fails with error 33504.
//!
//! Predicates run in the caller's context (`USER_NAME()`, `SESSION_CONTEXT`, `IS_MEMBER` ...). They apply to every user,
//! including `dbo` and sysadmin, as in SQL Server; a policy can be switched off with `ALTER SECURITY POLICY ... WITH
//! (STATE = OFF)` or dropped. A predicate function that reads the table it protects does not trigger the policy again.
use super::*;
use crate::catalog::{PredKind, Predicate, SecurityPolicy};
use crate::persist::LogOp;
use crate::storage::{Row, RowId};

impl<'a> Engine<'a> {
    fn can_manage_policies(&self) -> bool {
        if self.is_sysadmin() {
            return true;
        }
        let dbk = self.ses.db.to_lowercase();
        match (self.db_user(&dbk), self.srv.databases.get(&dbk)) {
            (Some(u), Some(db)) => db.security.allowed(&u, "CONTROL", "dbo", None),
            _ => false,
        }
    }

    fn policy_names(&self, name: &[String]) -> (String, String) {
        let schema = if name.len() >= 2 { name[name.len() - 2].clone() } else { "dbo".to_string() };
        (schema, name.last().cloned().unwrap_or_default())
    }

    fn check_predicates(&self, add: &[Predicate]) -> Result<()> {
        let dbk = self.ses.db.to_lowercase();
        let db = self.srv.databases.get(&dbk).ok_or_else(|| SqlError::new(911, "Database does not exist."))?;
        for p in add {
            match db.objects.get(&obj_key(&p.func.0, &p.func.1)) {
                Some(Object::Func(f)) if f.def.inline.is_some() => {}
                Some(Object::Func(_)) => return Err(SqlError::with(33263, 16, 1, format!("The function '{}.{}' cannot be used as a predicate: it must be an inline table-valued function.", p.func.0, p.func.1))),
                _ => return Err(SqlError::with(33262, 16, 1, format!("Cannot find the predicate function '{}.{}'.", p.func.0, p.func.1))),
            }
            let Some(Object::Table(t)) = db.objects.get(&p.table) else {
                return Err(SqlError::new(208, format!("Invalid object name '{}'.", p.table)));
            };
            for c in &p.args {
                if t.col_index(c).is_none() {
                    return Err(SqlError::new(207, format!("Invalid column name '{}'.", c)));
                }
            }
        }
        Ok(())
    }

    fn save_policies(&mut self, dbk: &str) {
        let policies = self.srv.databases.get(dbk).map(|d| d.policies.clone()).unwrap_or_default();
        self.log_nowait(vec![LogOp::DbPolicies { db: dbk.to_string(), policies }]);
    }

    pub fn exec_policy(&mut self, st: &Stmt) -> Result<()> {
        if !self.can_manage_policies() {
            return Err(SqlError::with(262, 14, 1, "ALTER ANY SECURITY POLICY permission denied in database."));
        }
        let dbk = self.ses.db.to_lowercase();
        match st {
            Stmt::CreateSecurityPolicy(p) => {
                let (schema, name) = self.policy_names(&p.name);
                if self.srv.databases[&dbk].policies.iter().any(|x| x.name.eq_ignore_ascii_case(&name) && x.schema.eq_ignore_ascii_case(&schema)) {
                    return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", name)));
                }
                self.check_predicates(&p.add)?;
                // one filter predicate per table and one block predicate of each kind
                self.check_no_duplicates(&dbk, &p.add, None)?;
                let id = self.srv.new_object_id();
                let sp = SecurityPolicy { schema, name, object_id: id, enabled: p.state.unwrap_or(true), preds: p.add.clone() };
                self.srv.cat_mut().databases.get_mut(&dbk).unwrap().policies.push(sp);
            }
            Stmt::AlterSecurityPolicy(p) => {
                let (schema, name) = self.policy_names(&p.name);
                let Some(i) = self.srv.databases[&dbk].policies.iter().position(|x| x.name.eq_ignore_ascii_case(&name) && x.schema.eq_ignore_ascii_case(&schema)) else {
                    return Err(SqlError::new(15151, format!("Cannot find the security policy '{}', because it does not exist or you do not have permission.", name)));
                };
                self.check_predicates(&p.add)?;
                self.check_no_duplicates(&dbk, &p.add, Some(i))?;
                let pol = &mut self.srv.cat_mut().databases.get_mut(&dbk).unwrap().policies[i];
                for d in &p.drop {
                    pol.preds.retain(|x| !(x.kind == d.kind && x.table == d.table));
                }
                pol.preds.extend(p.add.iter().cloned());
                if let Some(s) = p.state {
                    pol.enabled = s;
                }
            }
            Stmt::DropSecurityPolicy { name, if_exists } => {
                let (schema, n) = self.policy_names(name);
                let pols = &self.srv.databases[&dbk].policies;
                match pols.iter().position(|x| x.name.eq_ignore_ascii_case(&n) && x.schema.eq_ignore_ascii_case(&schema)) {
                    Some(i) => {
                        self.srv.cat_mut().databases.get_mut(&dbk).unwrap().policies.remove(i);
                    }
                    None if *if_exists => {}
                    None => return Err(SqlError::new(3701, format!("Cannot drop the security policy '{}', because it does not exist or you do not have permission.", n))),
                }
            }
            _ => {}
        }
        self.save_policies(&dbk);
        Ok(())
    }

    fn check_no_duplicates(&self, dbk: &str, add: &[Predicate], skip: Option<usize>) -> Result<()> {
        let db = &self.srv.databases[dbk];
        for (n, p) in add.iter().enumerate() {
            let existing = db.policies.iter().enumerate().filter(|(i, _)| Some(*i) != skip).flat_map(|(_, pol)| pol.preds.iter());
            let dup = existing.chain(add[..n].iter()).any(|x| x.kind == p.kind && x.table == p.table)
                || skip.is_some_and(|i| db.policies[i].preds.iter().any(|x| x.kind == p.kind && x.table == p.table));
            if dup {
                return Err(SqlError::with(33265, 16, 1, format!("The {} predicate on table '{}' is already defined by a security policy.", p.kind.describe().0.to_lowercase(), p.table)));
            }
        }
        Ok(())
    }

    /// The enabled predicates of kind `kind` for the table with key `table` in database `dbk`.
    fn policy_preds(&self, dbk: &str, table: &str, filter: bool) -> Vec<Predicate> {
        match self.srv.databases.get(dbk) {
            Some(d) if !d.policies.is_empty() => d.policies.iter().filter(|p| p.enabled).flat_map(|p| p.preds.iter()).filter(|p| p.table == table && (p.kind == PredKind::Filter) == filter).cloned().collect(),
            _ => vec![],
        }
    }

    /// Does the table have any enabled block predicate? (Writes skip the checks, and their cost, when it has none.)
    pub fn has_block_policy(&self, loc: &Loc) -> bool {
        match loc {
            Loc::Db(d, k) if !self.ses.in_rls => !self.policy_preds(d, k, false).is_empty(),
            _ => false,
        }
    }

    /// Does the table have any enabled filter predicate? (A cheap test for the read path.)
    pub fn has_filter_policy(&self, loc: &Loc) -> bool {
        match loc {
            Loc::Db(d, k) if !self.ses.in_rls => !self.policy_preds(d, k, true).is_empty(),
            _ => false,
        }
    }

    /// Evaluate predicate `p` for one row: true when the function returns a row.
    fn eval_predicate(&mut self, t: &Table, p: &Predicate, row: &Row, cache: &mut HashMap<String, bool>) -> Result<bool> {
        let args: Vec<Value> = p.args.iter().map(|c| t.col_index(c).map(|i| row[i].clone()).unwrap_or(Value::Null)).collect();
        let key = convert::key_of(&args);
        if let Some(b) = cache.get(&key) {
            return Ok(*b);
        }
        let f = self.find_func(Some(&p.func.0), &p.func.1).ok_or_else(|| SqlError::with(33262, 16, 1, format!("Cannot find the predicate function '{}.{}'.", p.func.0, p.func.1)))?;
        // the querying user needs no permission on the predicate function: it runs as its schema's owner
        let owner = self.schema_owner_of(&self.ses.db.to_lowercase(), &p.func.0);
        self.ses.module_owner.push(owner);
        self.ses.in_rls = true;
        let r = self.call_tvf(&f, args);
        self.ses.in_rls = false;
        self.ses.module_owner.pop();
        let ok = !r?.rows.is_empty();
        cache.insert(key, ok);
        Ok(ok)
    }

    /// Keep only the rows the session's filter predicates allow.
    pub fn rls_filter(&mut self, loc: &Loc, t: &Table, rows: Vec<(RowId, Row)>) -> Result<Vec<(RowId, Row)>> {
        let Loc::Db(d, k) = loc else { return Ok(rows) };
        let preds = self.policy_preds(d, k, true);
        if preds.is_empty() || self.ses.in_rls {
            return Ok(rows);
        }
        let mut caches: Vec<HashMap<String, bool>> = vec![HashMap::new(); preds.len()];
        let mut out = Vec::with_capacity(rows.len());
        'rows: for (rid, row) in rows {
            for (i, p) in preds.iter().enumerate() {
                if !self.eval_predicate(t, p, &row, &mut caches[i])? {
                    continue 'rows;
                }
            }
            out.push((rid, row));
        }
        Ok(out)
    }

    /// Block predicates of `kind` must hold for `rows` (new images for AFTER, existing rows for BEFORE).
    pub fn rls_check(&mut self, loc: &Loc, t: &Table, kind: PredKind, rows: &[&Row]) -> Result<()> {
        let Loc::Db(d, k) = loc else { return Ok(()) };
        if self.ses.in_rls || rows.is_empty() {
            return Ok(());
        }
        let preds: Vec<Predicate> = self.policy_preds(d, k, false).into_iter().filter(|p| p.kind == kind).collect();
        for p in preds {
            let mut cache = HashMap::new();
            for r in rows {
                if !self.eval_predicate(t, &p, r, &mut cache)? {
                    return Err(SqlError::with(
                        33504,
                        16,
                        1,
                        format!("The attempted operation failed because the target object '{}.{}.{}' has a block predicate that conflicts with this operation. If the operation is performed on a view, the block predicate might be enforced on the underlying table. Modify the operation to target only the rows that are allowed by the block predicate.", t.db, t.schema, t.name),
                    ));
                }
            }
        }
        Ok(())
    }
}
