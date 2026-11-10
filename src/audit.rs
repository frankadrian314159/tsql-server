//! SQL Server Audit: server audits, server audit specifications and database audit specifications.
//!
//! * A **server audit** says where audit records go: a file (`TO FILE (FILEPATH = 'dir')`, one JSON record per line in
//!   `<name>_<guid>.sqlaudit`, readable with `sys.fn_get_audit_file`), or `APPLICATION_LOG` / `SECURITY_LOG` (kept in an
//!   in-memory log of the last 10,000 records, `sys.dm_audit_log`). It can have a `WHERE` filter over the record's columns.
//! * A **server audit specification** lists server-level action groups (logins, logouts, principal and database changes,
//!   backup and restore, audit changes, DBCC).
//! * A **database audit specification** lists database-level groups (schema object changes, principal changes, permission
//!   changes, schema object access) and actions on objects (`SELECT, INSERT ON dbo.t BY public`).
//!
//! Definitions live in the catalog (server ones in [`crate::security::Security`], database ones in
//! [`crate::catalog::Database`]) and are logged and restored like other security metadata. The records themselves go
//! to the [`AuditSink`] in the shared core.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditTarget {
    File { path: String },
    ApplicationLog,
    SecurityLog,
}

impl AuditTarget {
    pub fn desc(&self) -> &'static str {
        match self {
            AuditTarget::File { .. } => "FILE",
            AuditTarget::ApplicationLog => "APPLICATION LOG",
            AuditTarget::SecurityLog => "SECURITY LOG",
        }
    }
    pub fn code(&self) -> &'static str {
        match self {
            AuditTarget::File { .. } => "FL",
            AuditTarget::ApplicationLog => "AL",
            AuditTarget::SecurityLog => "SL",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerAudit {
    pub name: String,
    pub id: i32,
    pub guid: String,
    pub target: AuditTarget,
    pub enabled: bool,
    pub on_failure: String,
    pub queue_delay: i32,
    /// `WHERE` filter, as source text and parsed predicate.
    pub filter: Option<(String, crate::ast::Expr)>,
    pub created: chrono::NaiveDateTime,
}

/// What an action item is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjSel {
    Database(String),
    Schema(String),
    /// `[schema.]name`
    Object(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditItem {
    Group(String),
    Action { action: String, object: ObjSel, principals: Vec<String> },
}

impl AuditItem {
    pub fn name(&self) -> &str {
        match self {
            AuditItem::Group(g) => g,
            AuditItem::Action { action, .. } => action,
        }
    }
}

/// A server or database audit specification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditSpec {
    pub name: String,
    pub id: i32,
    /// Lower-case name of the server audit it feeds.
    pub audit: String,
    pub enabled: bool,
    pub items: Vec<AuditItem>,
}

pub const SERVER_GROUPS: &[&str] = &[
    "FAILED_LOGIN_GROUP",
    "SUCCESSFUL_LOGIN_GROUP",
    "LOGOUT_GROUP",
    "SERVER_PRINCIPAL_CHANGE_GROUP",
    "DATABASE_CHANGE_GROUP",
    "BACKUP_RESTORE_GROUP",
    "AUDIT_CHANGE_GROUP",
    "DBCC_GROUP",
];

pub const DATABASE_GROUPS: &[&str] = &[
    "SCHEMA_OBJECT_CHANGE_GROUP",
    "DATABASE_OBJECT_CHANGE_GROUP",
    "DATABASE_PRINCIPAL_CHANGE_GROUP",
    "DATABASE_ROLE_MEMBER_CHANGE_GROUP",
    "DATABASE_PERMISSION_CHANGE_GROUP",
    "SCHEMA_OBJECT_PERMISSION_CHANGE_GROUP",
    "SCHEMA_OBJECT_ACCESS_GROUP",
];

pub const ACTIONS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES"];

/// One audit record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub audit: String,
    pub time: chrono::NaiveDateTime,
    pub seq: i64,
    pub action_id: String,
    pub action: String,
    pub succeeded: bool,
    pub session_id: i32,
    pub server_principal: String,
    pub db_principal: String,
    pub target_server_principal: String,
    pub target_db_principal: String,
    pub server_instance: String,
    pub database: String,
    pub schema: String,
    pub object: String,
    pub class_type: String,
    pub statement: String,
    pub info: String,
    pub app: String,
    pub host: String,
}

impl AuditEvent {
    /// The value of one of the record's columns, for `WHERE` filters.
    fn column(&self, name: &str) -> Option<crate::types::Value> {
        use crate::types::Value::{Bit, Int, Str};
        Some(match name.to_lowercase().as_str() {
            "action_id" => Str(self.action_id.clone()),
            "succeeded" => Bit(self.succeeded),
            "session_id" => Int(self.session_id),
            "server_principal_name" | "session_server_principal_name" => Str(self.server_principal.clone()),
            "database_principal_name" => Str(self.db_principal.clone()),
            "target_server_principal_name" => Str(self.target_server_principal.clone()),
            "target_database_principal_name" => Str(self.target_db_principal.clone()),
            "server_instance_name" => Str(self.server_instance.clone()),
            "database_name" => Str(self.database.clone()),
            "schema_name" => Str(self.schema.clone()),
            "object_name" => Str(self.object.clone()),
            "class_type" => Str(self.class_type.clone()),
            "statement" => Str(self.statement.clone()),
            "application_name" => Str(self.app.clone()),
            "host_name" => Str(self.host.clone()),
            "additional_information" => Str(self.info.clone()),
            _ => return None,
        })
    }
}

/// Does `e` pass a `WHERE` filter? Supports `=`, `<>`, `LIKE`, `IN`, `IS [NOT] NULL`, `AND`, `OR`, `NOT` over the record's
/// columns and literals; an expression it cannot evaluate lets the record through.
pub fn passes(filter: &crate::ast::Expr, e: &AuditEvent) -> bool {
    use crate::ast::{BinOp, Expr};
    use crate::types::Value;
    fn val(x: &Expr, e: &AuditEvent) -> Option<Value> {
        match x {
            Expr::Lit(v) => Some(v.clone()),
            Expr::Col(parts) => e.column(parts.last()?),
            Expr::Paren(i) => val(i, e),
            _ => None,
        }
    }
    fn text(v: &Value) -> String {
        match v {
            Value::Bit(b) => (*b as i32).to_string(),
            v => crate::convert::to_str(v),
        }
    }
    fn go(x: &Expr, e: &AuditEvent) -> bool {
        match x {
            Expr::And(a, b) => go(a, e) && go(b, e),
            Expr::Or(a, b) => go(a, e) || go(b, e),
            Expr::Not(a) => !go(a, e),
            Expr::Paren(i) => go(i, e),
            Expr::Binary(op, l, r) => match (val(l, e), val(r, e)) {
                (Some(a), Some(b)) => {
                    let (a, b) = (text(&a).to_lowercase(), text(&b).to_lowercase());
                    match op {
                        BinOp::Eq => a == b,
                        BinOp::Ne => a != b,
                        _ => true,
                    }
                }
                _ => true,
            },
            Expr::Like(l, p, _, neg) => match (val(l, e), val(p, e)) {
                (Some(a), Some(p)) => {
                    let pat = regex::escape(&text(&p).to_lowercase()).replace("%", ".*").replace('_', ".");
                    let m = regex::Regex::new(&format!("^{}$", pat)).map(|r| r.is_match(&text(&a).to_lowercase())).unwrap_or(true);
                    m != *neg
                }
                _ => true,
            },
            Expr::InList(l, items, neg) => match val(l, e) {
                Some(a) => items.iter().filter_map(|i| val(i, e)).any(|b| text(&a).eq_ignore_ascii_case(&text(&b))) != *neg,
                None => true,
            },
            Expr::IsNull(l, neg) => match val(l, e) {
                Some(a) => a.is_null() != *neg,
                None => true,
            },
            _ => true,
        }
    }
    go(filter, e)
}

const RING: usize = 10_000;

#[derive(Default)]
struct SinkState {
    seq: HashMap<String, i64>,
    files: HashMap<String, std::io::BufWriter<std::fs::File>>,
    ring: VecDeque<AuditEvent>,
    failed: HashMap<String, String>,
}

/// Where audit records are written. Shared by every session; it has its own lock.
#[derive(Default)]
pub struct AuditSink {
    st: Mutex<SinkState>,
    active: std::sync::atomic::AtomicBool,
}

impl AuditSink {
    /// Is any audit enabled? (a cheap check made before work is done for an event)
    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_active(&self, on: bool) {
        self.active.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// The file a file audit writes to.
    pub fn file_for(audit: &ServerAudit) -> Option<std::path::PathBuf> {
        match &audit.target {
            AuditTarget::File { path } => Some(std::path::Path::new(path).join(format!("{}_{}.sqlaudit", audit.name, audit.guid))),
            _ => None,
        }
    }

    pub fn write(&self, audit: &ServerAudit, mut ev: AuditEvent) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let seq = st.seq.entry(audit.name.to_lowercase()).or_insert(0);
        *seq += 1;
        ev.seq = *seq;
        ev.audit = audit.name.clone();
        match Self::file_for(audit) {
            Some(path) => {
                let key = path.to_string_lossy().to_string();
                let res = (|| -> std::io::Result<()> {
                    if !st.files.contains_key(&key) {
                        if let Some(dir) = path.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        let f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
                        st.files.insert(key.clone(), std::io::BufWriter::new(f));
                    }
                    let w = st.files.get_mut(&key).unwrap();
                    serde_json::to_writer(&mut *w, &ev).map_err(std::io::Error::other)?;
                    w.write_all(b"\n")?;
                    w.flush()
                })();
                match res {
                    Ok(()) => {
                        st.failed.remove(&audit.name.to_lowercase());
                    }
                    Err(e) => {
                        st.files.remove(&key);
                        st.failed.insert(audit.name.to_lowercase(), e.to_string());
                    }
                }
            }
            None => {
                if st.ring.len() >= RING {
                    st.ring.pop_front();
                }
                st.ring.push_back(ev);
            }
        }
    }

    /// Records held in memory (application and security log targets).
    pub fn ring(&self) -> Vec<AuditEvent> {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).ring.iter().cloned().collect()
    }

    /// The last write error of an audit, if its target is failing.
    pub fn failure(&self, audit: &str) -> Option<String> {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).failed.get(&audit.to_lowercase()).cloned()
    }

    /// Stop writing to an audit's file (it was dropped or moved).
    pub fn close(&self, audit: &ServerAudit) {
        if let Some(p) = Self::file_for(audit) {
            self.st.lock().unwrap_or_else(|e| e.into_inner()).files.remove(&p.to_string_lossy().to_string());
        }
    }
}

/// The records in the audit files matching `pattern`: a directory, a file, or a path with `*` wildcards.
pub fn read_files(pattern: &str) -> Result<Vec<(String, AuditEvent)>, String> {
    let p = std::path::Path::new(pattern);
    let (dir, pat) = if p.is_dir() {
        (p.to_path_buf(), "*.sqlaudit".to_string())
    } else {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        (p.parent().map(|d| d.to_path_buf()).unwrap_or_default(), if name.contains('*') && !name.contains('.') { format!("{}.sqlaudit", name) } else { name })
    };
    let re = regex::Regex::new(&format!("^{}$", regex::escape(&pat).replace("\\*", ".*"))).map_err(|e| e.to_string())?;
    let dir = if dir.as_os_str().is_empty() { std::path::PathBuf::from(".") } else { dir };
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir).map_err(|e| e.to_string())?.filter_map(|d| d.ok()).map(|d| d.path()).filter(|f| f.file_name().is_some_and(|n| re.is_match(&n.to_string_lossy()))).collect();
    files.sort();
    let mut out = vec![];
    for f in files {
        let text = std::fs::read_to_string(&f).map_err(|e| e.to_string())?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(ev) = serde_json::from_str::<AuditEvent>(line) {
                out.push((f.to_string_lossy().to_string(), ev));
            }
        }
    }
    Ok(out)
}

/// Does a database specification item match this event? `member` tells whether the acting user belongs to a principal.
pub fn item_matches(item: &AuditItem, actions: &[&str], groups: &[&str], database: &str, schema: &str, object: &str, user: &str, member: &dyn Fn(&str) -> bool) -> bool {
    match item {
        AuditItem::Group(g) => groups.iter().any(|x| x.eq_ignore_ascii_case(g)),
        AuditItem::Action { action, object: sel, principals } => {
            if !actions.iter().any(|a| a.eq_ignore_ascii_case(action)) {
                return false;
            }
            let on = match sel {
                ObjSel::Database(d) => d.eq_ignore_ascii_case(database),
                ObjSel::Schema(s) => s.eq_ignore_ascii_case(schema),
                ObjSel::Object(parts) => parts.last().is_some_and(|o| o.eq_ignore_ascii_case(object)) && (parts.len() < 2 || parts[parts.len() - 2].eq_ignore_ascii_case(schema)),
            };
            on && principals.iter().any(|p| p.eq_ignore_ascii_case("public") || p.eq_ignore_ascii_case(user) || member(p))
        }
    }
}

/// Short code of an action (`action_id` in audit files).
pub fn action_id(action: &str) -> String {
    match action {
        "SELECT" => "SL",
        "INSERT" => "IN",
        "UPDATE" => "UP",
        "DELETE" => "DL",
        "EXECUTE" => "EX",
        "REFERENCES" => "RF",
        "LOGIN SUCCEEDED" => "LGIS",
        "LOGIN FAILED" => "LGIF",
        "LOGOUT" => "LGO",
        "BACKUP" => "BA",
        "RESTORE" => "RS",
        "DBCC" => "DBCC",
        a if a.starts_with("CREATE_") => "CR",
        a if a.starts_with("ALTER_") => "AL",
        a if a.starts_with("DROP_") => "DR",
        a if a.starts_with("GRANT_") => "G",
        a if a.starts_with("DENY_") => "D",
        a if a.starts_with("REVOKE_") => "R",
        a if a.starts_with("AUDIT_") => "AUSC",
        _ => "",
    }
    .to_string()
}
