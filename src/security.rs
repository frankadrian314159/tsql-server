//! Authentication and authorisation: logins, database users and roles, GRANT / DENY / REVOKE.
//!
//! The model follows SQL Server:
//! * a *login* authenticates to the server; members of `sysadmin` bypass every check;
//! * a login is mapped to a *user* in each database; `dbo` (the database owner) is unrestricted;
//! * users belong to *roles*; every user is implicitly a member of `public`;
//! * a `DENY` anywhere in the principal's role closure and the securable's scope chain
//!   (object → schema → database) beats every `GRANT`;
//! * *ownership chaining*: inside a view, procedure, function or trigger, access to objects with
//!   the same owner as the module is not re-checked.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Login {
    pub name: String,
    pub salt: Vec<u8>,
    pub hash: Vec<u8>,
    pub disabled: bool,
    pub default_db: String,
}

pub fn hash_password(salt: &[u8], pw: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(pw.as_bytes());
    h.finalize().to_vec()
}

impl Login {
    pub fn new(name: &str, password: Option<&str>) -> Login {
        let salt: Vec<u8> = uuid::Uuid::new_v4().as_bytes().to_vec();
        let hash = match password {
            Some(p) => hash_password(&salt, p),
            None => vec![],
        };
        Login { name: name.to_string(), salt, hash, disabled: false, default_db: "master".into() }
    }
    pub fn set_password(&mut self, pw: &str) {
        self.hash = hash_password(&self.salt, pw);
    }
    /// A login with no password set accepts any password only for trusted (in-process) connections.
    pub fn check_password(&self, pw: &str) -> bool {
        !self.hash.is_empty() && hash_password(&self.salt, pw) == self.hash
    }
}

/// Server-level security: logins and server-role membership.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Security {
    pub logins: BTreeMap<String, Login>, // lower-case name -> login
    pub sysadmins: BTreeSet<String>,     // lower-case login names
    pub dbcreators: BTreeSet<String>,
}

impl Default for Security {
    fn default() -> Self {
        let mut s = Security { logins: BTreeMap::new(), sysadmins: BTreeSet::new(), dbcreators: BTreeSet::new() };
        s.logins.insert("sa".into(), Login::new("sa", None));
        s.sysadmins.insert("sa".into());
        s
    }
}

impl Security {
    pub fn is_sysadmin(&self, login: &str) -> bool {
        self.sysadmins.contains(&login.to_lowercase())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermState {
    Grant,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermTarget {
    Database,
    Schema(String), // lower-case schema name
    Object(String), // "schema.name" lower-case key
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermEntry {
    pub grantee: String, // lower-case principal
    pub perm: String,    // upper-case permission name
    pub target: PermTarget,
    pub state: PermState,
    pub with_grant: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DbPrincipal {
    pub name: String,
    pub login: Option<String>, // mapped login (users)
    pub is_role: bool,
    pub default_schema: String,
    pub id: i32,
}

/// Per-database security: users, roles, role membership and permissions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DbSecurity {
    pub principals: BTreeMap<String, DbPrincipal>, // lower-case name -> principal
    pub members: BTreeSet<(String, String)>,       // (role, member), lower-case
    pub perms: Vec<PermEntry>,
}

pub const FIXED_ROLES: [&str; 8] = ["db_owner", "db_datareader", "db_datawriter", "db_ddladmin", "db_securityadmin", "db_denydatareader", "db_denydatawriter", "db_accessadmin"];

impl Default for DbSecurity {
    fn default() -> Self {
        let mut p = BTreeMap::new();
        let mut id = 0;
        let mut add = |name: &str, role: bool| {
            id += 1;
            p.insert(name.to_lowercase(), DbPrincipal { name: name.to_string(), login: None, is_role: role, default_schema: "dbo".into(), id });
        };
        add("dbo", false);
        add("guest", false);
        add("public", true);
        for r in FIXED_ROLES {
            add(r, true);
        }
        DbSecurity { principals: p, members: BTreeSet::new(), perms: vec![] }
    }
}

fn implies(entry: &str, wanted: &str) -> bool {
    entry == wanted || entry == "CONTROL" || entry == "ALL" || entry == "ALL PRIVILEGES"
}

impl DbSecurity {
    pub fn principal(&self, name: &str) -> Option<&DbPrincipal> {
        self.principals.get(&name.to_lowercase())
    }

    /// The user mapped to `login`, if any.
    pub fn user_for_login(&self, login: &str) -> Option<&DbPrincipal> {
        self.principals.values().find(|p| !p.is_role && p.login.as_deref().map(|l| l.eq_ignore_ascii_case(login)).unwrap_or(false))
    }

    /// `name` plus every role it belongs to (transitively), plus `public`; lower-case.
    pub fn closure(&self, name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut stack = vec![name.to_lowercase()];
        while let Some(n) = stack.pop() {
            if !out.insert(n.clone()) {
                continue;
            }
            for (role, member) in &self.members {
                if *member == n {
                    stack.push(role.clone());
                }
            }
        }
        out.insert("public".into());
        out
    }

    pub fn is_member(&self, user: &str, role: &str) -> bool {
        self.closure(user).contains(&role.to_lowercase())
    }

    /// Does `user` hold `perm` on the securable? `schema`/`key` identify it.
    pub fn allowed(&self, user: &str, perm: &str, schema: &str, key: Option<&str>) -> bool {
        let u = user.to_lowercase();
        if u == "dbo" {
            return true;
        }
        let who = self.closure(&u);
        if who.contains("db_owner") {
            return true;
        }
        let perm = perm.to_uppercase();
        let in_scope = |t: &PermTarget| match t {
            PermTarget::Database => true,
            PermTarget::Schema(s) => s.eq_ignore_ascii_case(schema),
            PermTarget::Object(k) => key.map(|x| x.eq_ignore_ascii_case(k)).unwrap_or(false),
        };
        // fixed-role denies
        if who.contains("db_denydatareader") && perm == "SELECT" {
            return false;
        }
        if who.contains("db_denydatawriter") && matches!(perm.as_str(), "INSERT" | "UPDATE" | "DELETE") {
            return false;
        }
        if self.perms.iter().any(|e| e.state == PermState::Deny && who.contains(&e.grantee) && implies(&e.perm, &perm) && in_scope(&e.target)) {
            return false;
        }
        if self.perms.iter().any(|e| e.state == PermState::Grant && who.contains(&e.grantee) && implies(&e.perm, &perm) && in_scope(&e.target)) {
            return true;
        }
        if who.contains("db_datareader") && perm == "SELECT" {
            return true;
        }
        if who.contains("db_datawriter") && matches!(perm.as_str(), "INSERT" | "UPDATE" | "DELETE") {
            return true;
        }
        if who.contains("db_ddladmin") && (perm.starts_with("CREATE") || perm.starts_with("ALTER") || perm == "REFERENCES") {
            return true;
        }
        false
    }

    pub fn set_perm(&mut self, grantee: &str, perm: &str, target: PermTarget, state: Option<PermState>, with_grant: bool) {
        let g = grantee.to_lowercase();
        let p = perm.to_uppercase();
        self.perms.retain(|e| !(e.grantee == g && e.perm == p && e.target == target));
        if let Some(st) = state {
            self.perms.push(PermEntry { grantee: g, perm: p, target, state: st, with_grant });
        }
    }
}

/// Database-level permissions that can be granted without a securable.
pub fn is_db_permission(p: &str) -> bool {
    p.starts_with("CREATE ") || p == "CONNECT" || p == "CONTROL" || p == "VIEW ANY DEFINITION" || p == "ALTER ANY SCHEMA" || p == "ALTER ANY USER" || p == "ALTER ANY ROLE" || p == "SHOWPLAN" || p == "BACKUP DATABASE" || p == "VIEW DATABASE STATE" || p == "EXECUTE" || p == "SELECT" || p == "INSERT" || p == "UPDATE" || p == "DELETE" || p == "ALTER" || p == "REFERENCES" || p == "ALL"
}
