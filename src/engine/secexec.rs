//! Execution of security statements and permission checks.
use super::*;
use crate::ast::{PermKind, PermTargetAst, SecStmt};
use crate::security::{DbPrincipal, Login, PermState, PermTarget};

fn denied_object(perm: &str, db: &str, schema: &str, name: &str) -> SqlError {
    SqlError::with(229, 14, 5, format!("The {} permission was denied on the object '{}', database '{}', schema '{}'.", perm, name, db, schema))
}

impl<'a> Engine<'a> {
    pub fn eff_login(&self) -> String {
        self.ses.imp_stack.last().map(|(l, _)| l.clone()).unwrap_or_else(|| self.ses.login.clone())
    }

    /// May this session see masked columns unmasked (sysadmin, db_owner, or granted UNMASK)?
    pub fn can_unmask(&self) -> bool {
        if self.is_sysadmin() {
            return true;
        }
        let dbk = self.ses.db.to_lowercase();
        match (self.db_user(&dbk), self.srv.databases.get(&dbk)) {
            (Some(u), Some(db)) => db.security.allowed(&u, "UNMASK", "dbo", None),
            _ => false,
        }
    }

    pub fn is_sysadmin(&self) -> bool {
        self.srv.security.is_sysadmin(&self.eff_login())
    }

    /// The database user the current login acts as in `dbkey`; `None` when it has no access.
    pub fn db_user(&self, dbkey: &str) -> Option<String> {
        if let Some((_, Some(u))) = self.ses.imp_stack.last() {
            return Some(u.clone());
        }
        let login = self.eff_login();
        if self.srv.security.is_sysadmin(&login) {
            return Some("dbo".into());
        }
        let db = self.srv.databases.get(dbkey)?;
        if let Some(u) = db.security.user_for_login(&login) {
            return Some(u.name.clone());
        }
        if matches!(dbkey, "master" | "tempdb") {
            return Some("guest".into());
        }
        None
    }

    pub fn current_user_name(&self) -> String {
        self.db_user(&self.ses.db.to_lowercase()).unwrap_or_else(|| "guest".into())
    }

    pub fn schema_owner_of(&self, dbkey: &str, schema: &str) -> String {
        self.srv.databases.get(dbkey).and_then(|d| d.schema_owner.get(&schema.to_lowercase()).cloned()).unwrap_or_else(|| "dbo".into())
    }

    /// Check `perm` on an object (or, with `key = None`, a schema / the database).
    pub fn check_perm(&self, perm: &str, dbkey: &str, schema: &str, key: Option<&str>, display: &str) -> Result<()> {
        if self.is_sysadmin() {
            return Ok(());
        }
        if schema.eq_ignore_ascii_case("sys") || schema.eq_ignore_ascii_case("information_schema") {
            return Ok(());
        }
        let owner = self.schema_owner_of(dbkey, schema);
        // ownership chaining: inside a module owned by the same principal nothing is re-checked
        if self.ses.module_owner.last().map(|o| *o == owner).unwrap_or(false) {
            return Ok(());
        }
        let user = match self.db_user(dbkey) {
            Some(u) => u,
            None => return Err(SqlError::with(916, 14, 1, format!("The server principal \"{}\" is not able to access the database \"{}\" under the current security context.", self.eff_login(), dbkey))),
        };
        if user.eq_ignore_ascii_case(&owner) {
            return Ok(());
        }
        let db = match self.srv.databases.get(dbkey) {
            Some(d) => d,
            None => return Ok(()),
        };
        if db.security.allowed(&user, perm, schema, key) {
            Ok(())
        } else {
            let dbname = db.name.clone();
            Err(denied_object(perm, &dbname, schema, display))
        }
    }

    /// SELECT permission on a table for a query that is about to read it. Without table-wide SELECT, a user who
    /// holds SELECT on some columns may still read the table; the result says, per column, whether reading it is
    /// denied (the text completing the error) so that using such a column fails where it is used.
    /// An empty vector means every column is readable.
    pub fn select_denials(&self, loc: &Loc, name: &[String], t: &Table) -> Result<Vec<Option<Arc<String>>>> {
        let shown = name.last().map(|s| s.as_str()).unwrap_or("");
        let table_wide = self.check_object_perm(loc, "SELECT", shown);
        if self.srv.core.audit.is_active() {
            let res = self.select_denials_inner(loc, name, t, table_wide.clone());
            self.audit_access("SELECT", loc, res.is_ok());
            return res;
        }
        self.select_denials_inner(loc, name, t, table_wide)
    }

    fn select_denials_inner(&self, loc: &Loc, name: &[String], t: &Table, table_wide: Result<()>) -> Result<Vec<Option<Arc<String>>>> {
        let shown = name.last().map(|s| s.as_str()).unwrap_or("");
        let Loc::Db(d, k) = loc else { return table_wide.map(|_| vec![]) };
        let (schema, obj) = Self::split_key(k);
        let Some(db) = self.srv.databases.get(d) else { return table_wide.map(|_| vec![]) };
        let Some(user) = self.db_user(d) else { return table_wide.map(|_| vec![]) };
        // no column-level entries for this user and table: only the table-wide answer matters
        if !db.security.has_column_entries(&user, k) {
            return table_wide.map(|_| vec![]);
        }
        // administrators, the owner and ownership chains are never restricted by column entries
        let owner = self.schema_owner_of(d, &schema);
        let exempt = self.is_sysadmin() || user.eq_ignore_ascii_case(&owner) || self.ses.module_owner.last().map(|o| *o == owner).unwrap_or(false);
        if exempt {
            return table_wide.map(|_| vec![]);
        }
        let readable: Vec<bool> = t.columns.iter().map(|c| db.security.allowed_column(&user, "SELECT", &schema, k, &c.name)).collect();
        if !readable.iter().any(|r| *r) {
            return Err(table_wide.err().unwrap_or_else(|| denied_object("SELECT", &db.name, &schema, shown)));
        }
        if readable.iter().all(|r| *r) {
            return Ok(vec![]);
        }
        let suffix = Arc::new(format!(" of the object '{}', database '{}', schema '{}'.", obj, db.name, schema));
        Ok(readable.into_iter().map(|r| if r { None } else { Some(suffix.clone()) }).collect())
    }

    /// UPDATE permission for the columns a statement assigns (column-level grants are honoured).
    pub fn check_update_columns(&self, loc: &Loc, name: &[String], t: &Table, assigned: &[usize]) -> Result<()> {
        let shown = name.last().map(|s| s.as_str()).unwrap_or("");
        let first = match self.check_object_perm(loc, "UPDATE", shown) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let Loc::Db(d, k) = loc else { return Err(first) };
        let (schema, obj) = Self::split_key(k);
        let Some(user) = self.db_user(d) else { return Err(first) };
        let Some(db) = self.srv.databases.get(d) else { return Err(first) };
        if assigned.is_empty() || !db.security.has_column_entries(&user, k) {
            return Err(first);
        }
        for &c in assigned {
            let col = &t.columns[c].name;
            if !db.security.allowed_column(&user, "UPDATE", &schema, k, col) {
                return Err(SqlError::with(230, 14, 1, format!("The UPDATE permission was denied on the column '{}' of the object '{}', database '{}', schema '{}'.", col, obj, db.name, schema)));
            }
        }
        Ok(())
    }

    fn split_key(key: &str) -> (String, String) {
        match key.split_once('.') {
            Some((s, n)) => (s.to_string(), n.to_string()),
            None => ("dbo".to_string(), key.to_string()),
        }
    }

    pub fn check_object_perm(&self, loc: &Loc, perm: &str, display: &str) -> Result<()> {
        if let Loc::Db(d, k) = loc {
            let (schema, name) = Self::split_key(k);
            let shown = if display.is_empty() { name } else { display.to_string() };
            self.check_perm(perm, d, &schema, Some(k), &shown)?;
        }
        Ok(())
    }

    /// DDL permission: a database-level CREATE permission plus control of the target schema.
    pub fn check_create(&self, perm: &str, schema: &str) -> Result<()> {
        self.check_create_in(perm, Some(schema))
    }

    pub fn check_create_in(&self, perm: &str, schema: Option<&str>) -> Result<()> {
        if self.is_sysadmin() {
            return Ok(());
        }
        let dbkey = self.ses.db.to_lowercase();
        let user = self.db_user(&dbkey).unwrap_or_else(|| "guest".into());
        let db = self.srv.databases.get(&dbkey).unwrap();
        if user.eq_ignore_ascii_case("dbo") || db.security.is_member(&user, "db_owner") {
            return Ok(());
        }
        if !db.security.allowed(&user, perm, "", None) {
            return Err(SqlError::with(262, 14, 1, format!("{} permission denied in database '{}'.", perm, db.name)));
        }
        let Some(schema) = schema else { return Ok(()) };
        let owner = self.schema_owner_of(&dbkey, schema);
        if !user.eq_ignore_ascii_case(&owner) && !db.security.allowed(&user, "ALTER", schema, None) {
            return Err(SqlError::with(2760, 16, 1, format!("The specified schema name \"{}\" either does not exist or you do not have permission to use it.", schema)));
        }
        Ok(())
    }

    /// ALTER / DROP on an existing object.
    pub fn check_alter(&self, loc: &Loc, display: &str) -> Result<()> {
        if self.check_object_perm(loc, "ALTER", display).is_err() {
            return Err(SqlError::with(4902, 16, 1, format!("Cannot find the object \"{}\" because it does not exist or you do not have permission.", display)));
        }
        Ok(())
    }

    fn require_admin(&self, what: &str) -> Result<()> {
        if self.is_sysadmin() {
            return Ok(());
        }
        let dbkey = self.ses.db.to_lowercase();
        if let (Some(u), Some(db)) = (self.db_user(&dbkey), self.srv.databases.get(&dbkey)) {
            if u.eq_ignore_ascii_case("dbo") || db.security.is_member(&u, "db_owner") {
                return Ok(());
            }
        }
        Err(SqlError::with(15247, 16, 1, format!("User does not have permission to perform this action. ({})", what)))
    }

    pub(crate) fn log_security(&mut self) {
        let sec = self.srv.security.clone();
        self.log_nowait(vec![crate::persist::LogOp::Security(sec)]);
    }

    fn log_db_security(&mut self) {
        let dbk = self.ses.db.to_lowercase();
        if let Some(d) = self.srv.databases.get(&dbk) {
            let op = crate::persist::LogOp::DbSecurity { db: dbk.clone(), sec: d.security.clone(), owners: d.schema_owner.clone() };
            self.log_nowait(vec![op]);
        }
    }

    pub fn exec_security(&mut self, st: &SecStmt) -> Result<()> {
        let dbk = self.ses.db.to_lowercase();
        match st {
            SecStmt::CreateLogin { name, password, default_db, opts } => {
                if !self.is_sysadmin() {
                    return Err(SqlError::with(15247, 16, 1, "User does not have permission to perform this action."));
                }
                let k = name.to_lowercase();
                if self.srv.security.logins.contains_key(&k) {
                    return Err(SqlError::new(15025, format!("The server principal '{}' already exists.", name)));
                }
                let default_policy = self.srv.core.run_value("default login check_policy") != 0;
                let policy = opts.check_policy.unwrap_or(default_policy);
                if policy {
                    if let Some(why) = password.as_deref().and_then(|p| crate::security::password_problem(name, p)) {
                        return Err(SqlError::with(15118, 16, 1, format!("Password validation failed. The password does not meet Windows policy requirements because {}.", why)));
                    }
                }
                let mut l = Login::new(name, password.as_deref());
                l.external = opts.external.clone();
                l.check_policy = policy;
                l.check_expiration = opts.check_expiration.unwrap_or(policy);
                l.must_change = opts.must_change;
                if let Some(d) = default_db {
                    l.default_db = d.clone();
                }
                self.srv.cat_mut().security.logins.insert(k, l);
                self.log_security();
            }
            SecStmt::AlterLogin { name, password, disabled, default_db, opts } => {
                let own = self.eff_login().eq_ignore_ascii_case(name);
                let admin = self.is_sysadmin();
                if !admin && !(own && password.is_some()) {
                    return Err(SqlError::with(15247, 16, 1, "User does not have permission to perform this action."));
                }
                let (old_hash_ok, policy_now) = match self.srv.security.logins.get(&name.to_lowercase()) {
                    Some(l) => (opts.old_password.as_deref().is_some_and(|o| l.check_password(o)), opts.check_policy.unwrap_or(l.check_policy)),
                    None => return Err(SqlError::new(15151, format!("Cannot alter the login '{}', because it does not exist or you do not have permission.", name))),
                };
                // a user changing their own password must give the old one (an administrator need not)
                if !admin && !old_hash_ok {
                    return Err(SqlError::with(15151, 16, 1, "Cannot alter the login: the old password must be given (OLD_PASSWORD) when changing your own password."));
                }
                if policy_now {
                    if let Some(why) = password.as_deref().and_then(|p| crate::security::password_problem(name, p)) {
                        return Err(SqlError::with(15118, 16, 1, format!("Password validation failed. The password does not meet Windows policy requirements because {}.", why)));
                    }
                }
                let l = self.srv.cat_mut().security.logins.get_mut(&name.to_lowercase()).unwrap();
                if let Some(p) = password {
                    l.set_password(p);
                    l.bad_count = 0;
                    l.locked_until = None;
                }
                if let Some(c) = opts.check_policy {
                    l.check_policy = c;
                    if !c {
                        l.check_expiration = false;
                    }
                }
                if let Some(c) = opts.check_expiration {
                    l.check_expiration = c;
                }
                if opts.must_change && admin {
                    l.must_change = true;
                }
                if opts.unlock && admin {
                    l.bad_count = 0;
                    l.locked_until = None;
                }
                if let Some(d) = disabled {
                    l.disabled = *d;
                }
                if let Some(d) = default_db {
                    l.default_db = d.clone();
                }
                self.log_security();
            }
            SecStmt::CreateUser { name, login, default_schema } => {
                self.require_admin("CREATE USER")?;
                let k = name.to_lowercase();
                if let Some(l) = login {
                    if !self.srv.security.logins.contains_key(&l.to_lowercase()) {
                        return Err(SqlError::new(15007, format!("'{}' is not a valid login or you do not have permission.", l)));
                    }
                }
                let mapped = login.clone().or_else(|| Some(name.clone()).filter(|n| self.srv.security.logins.contains_key(&n.to_lowercase())));
                let d = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                if d.security.principals.contains_key(&k) {
                    return Err(SqlError::new(15023, format!("User, group, or role '{}' already exists in the current database.", name)));
                }
                let id = d.security.principals.len() as i32 + 1;
                d.security.principals.insert(k, DbPrincipal { name: name.clone(), login: mapped, is_role: false, default_schema: default_schema.clone().unwrap_or_else(|| "dbo".into()), id });
                self.log_db_security();
            }
            SecStmt::AlterUser { name, default_schema, new_name } => {
                self.require_admin("ALTER USER")?;
                let d = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                let p = d.security.principals.get_mut(&name.to_lowercase()).ok_or_else(|| SqlError::new(15151, format!("Cannot alter the user '{}', because it does not exist or you do not have permission.", name)))?;
                if let Some(s) = default_schema {
                    p.default_schema = s.clone();
                }
                if let Some(n) = new_name {
                    p.name = n.clone();
                    let mut p2 = p.clone();
                    p2.name = n.clone();
                    d.security.principals.remove(&name.to_lowercase());
                    d.security.principals.insert(n.to_lowercase(), p2);
                }
                self.log_db_security();
            }
            SecStmt::CreateRole { name } => {
                self.require_admin("CREATE ROLE")?;
                let k = name.to_lowercase();
                let d = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                if d.security.principals.contains_key(&k) {
                    return Err(SqlError::new(15023, format!("User, group, or role '{}' already exists in the current database.", name)));
                }
                let id = d.security.principals.len() as i32 + 1;
                d.security.principals.insert(k, DbPrincipal { name: name.clone(), login: None, is_role: true, default_schema: "dbo".into(), id });
                self.log_db_security();
            }
            SecStmt::AlterRole { role, add, member } => {
                self.require_admin("ALTER ROLE")?;
                let d = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                let r = d.security.principal(role).filter(|p| p.is_role).ok_or_else(|| SqlError::new(15151, format!("Cannot alter the role '{}', because it does not exist or you do not have permission.", role)))?.name.to_lowercase();
                let m = d.security.principal(member).ok_or_else(|| SqlError::new(15410, format!("User or role '{}' does not exist in this database.", member)))?.name.to_lowercase();
                if *add {
                    d.security.members.insert((r, m));
                } else {
                    d.security.members.remove(&(r, m));
                }
                self.log_db_security();
            }
            SecStmt::AlterServerRole { role, add, member } => {
                if !self.is_sysadmin() {
                    return Err(SqlError::with(15247, 16, 1, "User does not have permission to perform this action."));
                }
                if !self.srv.security.logins.contains_key(&member.to_lowercase()) {
                    return Err(SqlError::new(15151, format!("Cannot find the login '{}', because it does not exist or you do not have permission.", member)));
                }
                let set = match role.to_lowercase().as_str() {
                    "sysadmin" => &mut self.srv.cat_mut().security.sysadmins,
                    "dbcreator" => &mut self.srv.cat_mut().security.dbcreators,
                    _ => return Err(SqlError::new(15151, format!("Cannot alter the server role '{}', because it does not exist or you do not have permission.", role))),
                };
                if *add {
                    set.insert(member.to_lowercase());
                } else {
                    set.remove(&member.to_lowercase());
                }
                self.log_security();
            }
            SecStmt::Perm { kind, perms, target, principals, with_grant, cols } => {
                if cols.iter().any(|c| !c.is_empty()) && !matches!(target, PermTargetAst::Object(_)) {
                    return Err(SqlError::new(102, "Incorrect syntax: a column list needs an object."));
                }
                let (ptarget, owner_ok, schema_for_owner) = match target {
                    PermTargetAst::Database => (PermTarget::Database, false, String::new()),
                    PermTargetAst::Schema(s) => {
                        if !self.srv.databases[&dbk].schemas.contains_key(&s.to_lowercase()) {
                            return Err(SqlError::new(15151, format!("Cannot find the schema '{}', because it does not exist or you do not have permission.", s)));
                        }
                        (PermTarget::Schema(s.to_lowercase()), true, s.clone())
                    }
                    PermTargetAst::Object(name) => {
                        let loc = self.locate(name);
                        let key = match &loc {
                            Loc::Db(d, k) if d == &dbk && self.srv.databases[d].objects.contains_key(k) => k.clone(),
                            _ => return Err(SqlError::new(15151, format!("Cannot find the object '{}', because it does not exist or you do not have permission.", name.join(".")))),
                        };
                        let sch = key.split('.').next().unwrap_or("dbo").to_string();
                        (PermTarget::Object(key), true, sch)
                    }
                };
                // who may grant?
                let user = self.current_user_name();
                let allowed = self.is_sysadmin() || {
                    let db = &self.srv.databases[&dbk];
                    user.eq_ignore_ascii_case("dbo") || db.security.is_member(&user, "db_owner") || (owner_ok && self.schema_owner_of(&dbk, &schema_for_owner).eq_ignore_ascii_case(&user))
                };
                if !allowed {
                    return Err(SqlError::with(15247, 16, 1, "User does not have permission to perform this action."));
                }
                for pr in principals {
                    let pk = pr.to_lowercase();
                    if !self.srv.databases[&dbk].security.principals.contains_key(&pk) {
                        return Err(SqlError::new(15151, format!("Cannot find the user '{}', because it does not exist or you do not have permission.", pr)));
                    }
                    for (pi, perm) in perms.iter().enumerate() {
                        let state = match kind {
                            PermKind::Grant => Some(PermState::Grant),
                            PermKind::Deny => Some(PermState::Deny),
                            PermKind::Revoke => None,
                        };
                        let columns = cols.get(pi).cloned().unwrap_or_default();
                        if columns.is_empty() {
                            self.srv.cat_mut().databases.get_mut(&dbk).unwrap().security.set_perm(pr, perm, ptarget.clone(), state, *with_grant);
                            continue;
                        }
                        // column-level: SELECT, UPDATE, INSERT and REFERENCES apply to columns
                        let PermTarget::Object(key) = &ptarget else { unreachable!() };
                        if !matches!(perm.as_str(), "SELECT" | "UPDATE" | "INSERT" | "REFERENCES") {
                            return Err(SqlError::new(1049, format!("The {} permission cannot be granted on columns.", perm)));
                        }
                        let tbl = match &self.srv.databases[&dbk].objects.get(key) {
                            Some(Object::Table(t)) => t.clone(),
                            _ => return Err(SqlError::new(4604, "Column permissions apply to tables and views only.")),
                        };
                        for c in &columns {
                            if tbl.col_index(c).is_none() {
                                return Err(SqlError::new(207, format!("Invalid column name '{}'.", c)));
                            }
                            let canonical = tbl.columns[tbl.col_index(c).unwrap()].name.to_lowercase();
                            self.srv.cat_mut().databases.get_mut(&dbk).unwrap().security.set_perm(pr, perm, PermTarget::Column(key.clone(), canonical), state, *with_grant);
                        }
                    }
                }
                self.log_db_security();
            }
            SecStmt::ExecuteAs { login, name } => {
                if !self.is_sysadmin() {
                    let u = self.current_user_name();
                    let db = &self.srv.databases[&dbk];
                    if !(u.eq_ignore_ascii_case("dbo") || db.security.is_member(&u, "db_owner")) {
                        return Err(SqlError::with(15517, 14, 1, "Cannot execute as the database principal because the principal does not exist, this type of principal cannot be impersonated, or you do not have permission."));
                    }
                }
                if *login {
                    if !self.srv.security.logins.contains_key(&name.to_lowercase()) {
                        return Err(SqlError::with(15517, 14, 1, "Cannot execute as the server principal because the principal does not exist, this type of principal cannot be impersonated, or you do not have permission."));
                    }
                    self.ses.imp_stack.push((name.clone(), None));
                } else {
                    let p = self.srv.databases[&dbk].security.principal(name).filter(|p| !p.is_role).cloned().ok_or_else(|| SqlError::with(15517, 14, 1, "Cannot execute as the database principal because the principal does not exist, this type of principal cannot be impersonated, or you do not have permission."))?;
                    // a user without a login has only its own permissions: a sysadmin caller does not carry its rights into it
                    let l = p.login.clone().unwrap_or_default();
                    self.ses.imp_stack.push((l, Some(p.name.clone())));
                }
            }
            SecStmt::Revert => {
                self.ses.imp_stack.pop();
            }
        }
        Ok(())
    }

    /// DROP LOGIN / USER / ROLE.
    pub fn drop_principal(&mut self, what: &str, name: &str, if_exists: bool) -> Result<()> {
        let dbk = self.ses.db.to_lowercase();
        match what {
            "LOGIN" => {
                if !self.is_sysadmin() {
                    return Err(SqlError::with(15247, 16, 1, "User does not have permission to perform this action."));
                }
                let k = name.to_lowercase();
                if k == "sa" {
                    return Err(SqlError::new(15405, "Cannot use the special principal 'sa'."));
                }
                if self.srv.cat_mut().security.logins.remove(&k).is_none() && !if_exists {
                    return Err(SqlError::new(15151, format!("Cannot drop the login '{}', because it does not exist or you do not have permission.", name)));
                }
                self.srv.cat_mut().security.sysadmins.remove(&k);
                self.srv.cat_mut().security.dbcreators.remove(&k);
                self.log_security();
            }
            _ => {
                self.require_admin("DROP")?;
                let k = name.to_lowercase();
                let d = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                let is_role = what == "ROLE";
                let ok = d.security.principals.get(&k).map(|p| p.is_role == is_role).unwrap_or(false);
                if !ok || crate::security::FIXED_ROLES.contains(&k.as_str()) || k == "dbo" || k == "public" || k == "guest" {
                    if if_exists && !ok {
                        return Ok(());
                    }
                    return Err(SqlError::new(15151, format!("Cannot drop the {} '{}', because it does not exist or you do not have permission.", what.to_lowercase(), name)));
                }
                d.security.principals.remove(&k);
                d.security.members.retain(|(r, m)| *r != k && *m != k);
                d.security.perms.retain(|e| e.grantee != k);
                self.log_db_security();
            }
        }
        Ok(())
    }
}
