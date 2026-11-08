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

    fn log_security(&mut self) {
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
            SecStmt::CreateLogin { name, password, default_db } => {
                if !self.is_sysadmin() {
                    return Err(SqlError::with(15247, 16, 1, "User does not have permission to perform this action."));
                }
                let k = name.to_lowercase();
                if self.srv.security.logins.contains_key(&k) {
                    return Err(SqlError::new(15025, format!("The server principal '{}' already exists.", name)));
                }
                let mut l = Login::new(name, password.as_deref());
                if let Some(d) = default_db {
                    l.default_db = d.clone();
                }
                self.srv.security.logins.insert(k, l);
                self.log_security();
            }
            SecStmt::AlterLogin { name, password, disabled, default_db } => {
                let own = self.eff_login().eq_ignore_ascii_case(name);
                if !self.is_sysadmin() && !(own && password.is_some()) {
                    return Err(SqlError::with(15247, 16, 1, "User does not have permission to perform this action."));
                }
                let l = self.srv.security.logins.get_mut(&name.to_lowercase()).ok_or_else(|| SqlError::new(15151, format!("Cannot alter the login '{}', because it does not exist or you do not have permission.", name)))?;
                if let Some(p) = password {
                    l.set_password(p);
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
                let d = self.srv.databases.get_mut(&dbk).unwrap();
                if d.security.principals.contains_key(&k) {
                    return Err(SqlError::new(15023, format!("User, group, or role '{}' already exists in the current database.", name)));
                }
                let id = d.security.principals.len() as i32 + 1;
                d.security.principals.insert(k, DbPrincipal { name: name.clone(), login: mapped, is_role: false, default_schema: default_schema.clone().unwrap_or_else(|| "dbo".into()), id });
                self.log_db_security();
            }
            SecStmt::AlterUser { name, default_schema, new_name } => {
                self.require_admin("ALTER USER")?;
                let d = self.srv.databases.get_mut(&dbk).unwrap();
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
                let d = self.srv.databases.get_mut(&dbk).unwrap();
                if d.security.principals.contains_key(&k) {
                    return Err(SqlError::new(15023, format!("User, group, or role '{}' already exists in the current database.", name)));
                }
                let id = d.security.principals.len() as i32 + 1;
                d.security.principals.insert(k, DbPrincipal { name: name.clone(), login: None, is_role: true, default_schema: "dbo".into(), id });
                self.log_db_security();
            }
            SecStmt::AlterRole { role, add, member } => {
                self.require_admin("ALTER ROLE")?;
                let d = self.srv.databases.get_mut(&dbk).unwrap();
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
                    "sysadmin" => &mut self.srv.security.sysadmins,
                    "dbcreator" => &mut self.srv.security.dbcreators,
                    _ => return Err(SqlError::new(15151, format!("Cannot alter the server role '{}', because it does not exist or you do not have permission.", role))),
                };
                if *add {
                    set.insert(member.to_lowercase());
                } else {
                    set.remove(&member.to_lowercase());
                }
                self.log_security();
            }
            SecStmt::Perm { kind, perms, target, principals, with_grant, columns } => {
                if *columns {
                    return Err(SqlError::new(15477, "Column-level permissions are not supported; grant the permission on the whole object."));
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
                    for perm in perms {
                        let state = match kind {
                            PermKind::Grant => Some(PermState::Grant),
                            PermKind::Deny => Some(PermState::Deny),
                            PermKind::Revoke => None,
                        };
                        self.srv.databases.get_mut(&dbk).unwrap().security.set_perm(pr, perm, ptarget.clone(), state, *with_grant);
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
                    let l = p.login.clone().unwrap_or_else(|| self.eff_login());
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
                if self.srv.security.logins.remove(&k).is_none() && !if_exists {
                    return Err(SqlError::new(15151, format!("Cannot drop the login '{}', because it does not exist or you do not have permission.", name)));
                }
                self.srv.security.sysadmins.remove(&k);
                self.srv.security.dbcreators.remove(&k);
                self.log_security();
            }
            _ => {
                self.require_admin("DROP")?;
                let k = name.to_lowercase();
                let d = self.srv.databases.get_mut(&dbk).unwrap();
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
