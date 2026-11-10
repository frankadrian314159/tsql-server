//! Executing SQL Server Audit statements and recording audit events (see [`crate::audit`]).
use super::*;
use crate::audit::{self, AuditEvent, AuditItem, AuditSpec, ServerAudit};

fn split_key(key: &str) -> (String, String) {
    match key.split_once('.') {
        Some((s, n)) => (s.to_string(), n.to_string()),
        None => ("dbo".to_string(), key.to_string()),
    }
}

/// An audit event with the session-independent fields blank.
pub fn blank_event(server: &str) -> AuditEvent {
    AuditEvent {
        audit: String::new(),
        time: chrono::Local::now().naive_local(),
        seq: 0,
        action_id: String::new(),
        action: String::new(),
        succeeded: true,
        session_id: 0,
        server_principal: String::new(),
        db_principal: String::new(),
        target_server_principal: String::new(),
        target_db_principal: String::new(),
        server_instance: server.to_string(),
        database: String::new(),
        schema: String::new(),
        object: String::new(),
        class_type: String::new(),
        statement: String::new(),
        info: String::new(),
        app: String::new(),
        host: String::new(),
    }
}

/// Record a server-level event (login, logout) that has no session: `groups` select the specifications.
pub fn server_event(state: &crate::catalog::ServerState, groups: &[&str], mut ev: AuditEvent) {
    let sec = &state.security;
    if !sec.audits.iter().any(|a| a.enabled) {
        return;
    }
    ev.server_instance = state.server_name.clone();
    for audit in sec.audits.iter().filter(|a| a.enabled) {
        let hit = sec.server_specs.iter().any(|s| s.enabled && s.audit.eq_ignore_ascii_case(&audit.name) && s.items.iter().any(|i| matches!(i, AuditItem::Group(g) if groups.iter().any(|x| x.eq_ignore_ascii_case(g)))));
        if hit && audit.filter.as_ref().is_none_or(|(_, f)| audit::passes(f, &ev)) {
            state.core.audit.write(audit, ev.clone());
        }
    }
}

/// A login attempt (`ok` says whether it succeeded) or a logout.
pub fn login_event(state: &crate::catalog::ServerState, kind: &str, login: &str, spid: i32, info: &str) {
    let mut ev = blank_event(&state.server_name);
    let (group, ok) = match kind {
        "LOGIN SUCCEEDED" => ("SUCCESSFUL_LOGIN_GROUP", true),
        "LOGIN FAILED" => ("FAILED_LOGIN_GROUP", false),
        _ => ("LOGOUT_GROUP", true),
    };
    ev.action = kind.to_string();
    ev.action_id = audit::action_id(kind);
    ev.succeeded = ok;
    ev.session_id = spid;
    ev.server_principal = login.to_string();
    ev.class_type = "LX".into();
    ev.info = info.to_string();
    server_event(state, &[group], ev);
}

impl<'a> Engine<'a> {
    pub fn audit_on(&self) -> bool {
        self.srv.core.audit.is_active() && self.srv.security.audits.iter().any(|a| a.enabled)
    }

    fn base_event(&self, action: &str, ok: bool, db: &str, schema: &str, object: &str, class: &str) -> AuditEvent {
        let mut ev = blank_event(&self.srv.server_name);
        ev.action = action.to_string();
        ev.action_id = audit::action_id(action);
        ev.succeeded = ok;
        ev.session_id = self.ses.spid;
        ev.server_principal = self.eff_login();
        ev.db_principal = self.current_user_name();
        ev.database = db.to_string();
        ev.schema = schema.to_string();
        ev.object = object.to_string();
        ev.class_type = class.to_string();
        ev.statement = self.ses.batch_text.clone();
        ev.app = self.ses.app_name.clone();
        ev.host = self.ses.host_name.clone();
        ev
    }

    /// Route an event to every enabled audit whose specifications select it.
    fn audit_emit(&self, actions: &[&str], groups: &[&str], ev: AuditEvent) {
        let sec = &self.srv.security;
        if !sec.audits.iter().any(|a| a.enabled) {
            return;
        }
        let mut hit: Vec<&ServerAudit> = vec![];
        let mut add = |name: &str| {
            if let Some(a) = sec.audits.iter().find(|a| a.enabled && a.name.eq_ignore_ascii_case(name)) {
                if !hit.iter().any(|h| h.name == a.name) {
                    hit.push(a);
                }
            }
        };
        for s in sec.server_specs.iter().filter(|s| s.enabled) {
            if s.items.iter().any(|i| matches!(i, AuditItem::Group(g) if groups.iter().any(|x| x.eq_ignore_ascii_case(g)))) {
                add(&s.audit);
            }
        }
        if let Some(db) = self.srv.databases.get(&ev.database.to_lowercase()) {
            let user = ev.db_principal.clone();
            let member = |p: &str| db.security.is_member(&user, p);
            for s in db.audit_specs.iter().filter(|s| s.enabled) {
                if s.items.iter().any(|i| audit::item_matches(i, actions, groups, &ev.database, &ev.schema, &ev.object, &user, &member)) {
                    add(&s.audit);
                }
            }
        }
        for a in hit {
            if a.filter.as_ref().is_none_or(|(_, f)| audit::passes(f, &ev)) {
                self.srv.core.audit.write(a, ev.clone());
            }
        }
    }

    /// An access to a table or view (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `EXECUTE`); at most one record per statement
    /// and object.
    pub fn audit_access(&self, action: &str, loc: &Loc, ok: bool) {
        if !self.audit_on() {
            return;
        }
        let Loc::Db(d, k) = loc else { return };
        let (schema, object) = split_key(k);
        if schema.eq_ignore_ascii_case("sys") || schema.eq_ignore_ascii_case("information_schema") {
            return;
        }
        if !self.ses.audit_seen.borrow_mut().insert(format!("{}|{}|{}|{}", action, d, k, ok)) {
            return;
        }
        let class = match self.srv.databases.get(d).and_then(|db| db.objects.get(k)) {
            Some(Object::View(_)) => "V ",
            Some(Object::Proc(_)) => "P ",
            Some(Object::Func(_)) => "FN",
            _ => "U ",
        };
        let dbname = self.srv.databases.get(d).map(|db| db.name.clone()).unwrap_or_else(|| d.clone());
        let ev = self.base_event(action, ok, &dbname, &schema, &object, class);
        self.audit_emit(&[action], &["SCHEMA_OBJECT_ACCESS_GROUP"], ev);
    }

    /// Record the events a DDL statement raised, mapped to audit groups.
    pub fn audit_ddl(&self, events: &[super::ddltrig::DdlEvent]) {
        if !self.audit_on() {
            return;
        }
        for e in events {
            let ty = e.object_type;
            let mut groups: Vec<&str> = match (e.name.split('_').next().unwrap_or(""), ty) {
                ("CREATE" | "ALTER" | "DROP", "TABLE" | "VIEW" | "INDEX" | "PROCEDURE" | "FUNCTION" | "TRIGGER" | "SEQUENCE" | "TYPE") => vec!["SCHEMA_OBJECT_CHANGE_GROUP"],
                ("CREATE" | "ALTER" | "DROP", "SCHEMA") => vec!["DATABASE_OBJECT_CHANGE_GROUP"],
                ("CREATE" | "ALTER" | "DROP", "USER" | "ROLE") => vec!["DATABASE_PRINCIPAL_CHANGE_GROUP"],
                ("CREATE" | "ALTER" | "DROP", "LOGIN") => vec!["SERVER_PRINCIPAL_CHANGE_GROUP"],
                ("CREATE" | "ALTER" | "DROP", "DATABASE") => vec!["DATABASE_CHANGE_GROUP"],
                ("GRANT" | "DENY" | "REVOKE", "DATABASE") => vec!["DATABASE_PERMISSION_CHANGE_GROUP"],
                ("GRANT" | "DENY" | "REVOKE", _) => vec!["SCHEMA_OBJECT_PERMISSION_CHANGE_GROUP"],
                _ => vec![],
            };
            if e.name == "ALTER_ROLE" {
                groups.push("DATABASE_ROLE_MEMBER_CHANGE_GROUP");
            }
            if groups.is_empty() {
                continue;
            }
            let class = match ty {
                "TABLE" => "U ",
                "VIEW" => "V ",
                "PROCEDURE" => "P ",
                "FUNCTION" => "FN",
                "TRIGGER" => "TR",
                "SCHEMA" => "SC",
                "DATABASE" => "DB",
                "LOGIN" => "LX",
                "USER" => "US",
                "ROLE" => "RL",
                "OBJECT" => match self.srv.databases.get(&self.ses.db.to_lowercase()).and_then(|d| d.objects.get(&obj_key(&e.schema, &e.object))) {
                    Some(Object::View(_)) => "V ",
                    Some(Object::Proc(_)) => "P ",
                    Some(Object::Func(_)) => "FN",
                    _ => "U ",
                },
                _ => "  ",
            };
            let database = if e.database.is_empty() { self.ses.db.clone() } else { e.database.clone() };
            let mut ev = self.base_event(e.name, true, if e.server_only && ty == "LOGIN" { "" } else { &database }, &e.schema, &e.object, class);
            if ty == "LOGIN" {
                ev.target_server_principal = e.object.clone();
            } else if ty == "USER" || ty == "ROLE" {
                ev.target_db_principal = e.object.clone();
            }
            self.audit_emit(&[], &groups, ev);
        }
    }

    /// A statement that is audited as a whole (`BACKUP`, `RESTORE`, `DBCC`, audit changes).
    pub fn audit_server_group(&self, group: &str, action: &str, object: &str) {
        if !self.audit_on() {
            return;
        }
        let mut ev = self.base_event(action, true, &self.ses.db.clone(), "", object, "DB");
        if action == "BACKUP" || action == "RESTORE" {
            ev.database = object.to_string();
        }
        server_event(&self.srv, &[group], ev);
    }

    // ------------------------------------------------------------------ statements
    fn can_manage_audits(&self) -> bool {
        self.is_sysadmin()
    }

    fn save_server_audits(&mut self) {
        self.log_security();
    }

    fn save_db_specs(&mut self, dbk: &str) {
        let specs = self.srv.databases.get(dbk).map(|d| d.audit_specs.clone()).unwrap_or_default();
        self.log_nowait(vec![crate::persist::LogOp::DbAuditSpecs { db: dbk.to_string(), specs }]);
    }

    fn check_items(&self, items: &[AuditItem], server: bool) -> Result<()> {
        for i in items {
            match i {
                AuditItem::Group(g) => {
                    let ok = if server { audit::SERVER_GROUPS.contains(&g.as_str()) } else { audit::DATABASE_GROUPS.contains(&g.as_str()) };
                    if !ok {
                        return Err(SqlError::with(25707, 16, 1, format!("'{}' is not a valid audit action group for a {} audit specification.", g, if server { "server" } else { "database" })));
                    }
                }
                AuditItem::Action { action, .. } => {
                    if server || !audit::ACTIONS.contains(&action.as_str()) {
                        return Err(SqlError::with(25707, 16, 1, format!("'{}' is not a valid audit action.", action)));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn exec_audit(&mut self, a: &AuditStmt) -> Result<()> {
        let res = self.exec_audit_inner(a);
        self.srv.core.audit.set_active(self.srv.security.audits.iter().any(|a| a.enabled));
        if res.is_ok() {
            let (action, name) = match a {
                AuditStmt::CreateAudit { name, .. } | AuditStmt::CreateSpec { name, .. } => ("AUDIT_CREATE", name.clone()),
                AuditStmt::AlterAudit { name, .. } | AuditStmt::AlterSpec { name, .. } => ("AUDIT_ALTER", name.clone()),
                AuditStmt::DropAudit { name } | AuditStmt::DropSpec { name, .. } => ("AUDIT_DROP", name.clone()),
            };
            self.audit_server_group("AUDIT_CHANGE_GROUP", action, &name);
        }
        res
    }

    fn exec_audit_inner(&mut self, a: &AuditStmt) -> Result<()> {
        let denied = |what: &str| SqlError::with(262, 14, 1, format!("{} permission denied.", what));
        match a {
            AuditStmt::CreateAudit { name, target, opts, filter, state_on } => {
                if !self.can_manage_audits() {
                    return Err(denied("ALTER ANY SERVER AUDIT"));
                }
                if self.srv.security.audits.iter().any(|x| x.name.eq_ignore_ascii_case(name)) {
                    return Err(SqlError::with(15012, 16, 1, format!("The server audit '{}' already exists.", name)));
                }
                let get = |k: &str| opts.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                let on_failure = get("ON_FAILURE").unwrap_or_else(|| "CONTINUE".into()).to_uppercase();
                if !["CONTINUE", "SHUTDOWN", "FAIL_OPERATION"].contains(&on_failure.as_str()) {
                    return Err(SqlError::with(25709, 16, 1, "ON_FAILURE must be CONTINUE, SHUTDOWN or FAIL_OPERATION."));
                }
                let guid = get("AUDIT_GUID").unwrap_or_else(|| uuid::Uuid::new_v4().to_string()).to_lowercase();
                let audit = ServerAudit {
                    name: name.clone(),
                    id: self.srv.security.audits.iter().map(|x| x.id).max().unwrap_or(65535) + 1,
                    guid,
                    target: target.clone(),
                    enabled: *state_on,
                    on_failure,
                    queue_delay: get("QUEUE_DELAY").and_then(|v| v.parse().ok()).unwrap_or(1000),
                    filter: filter.clone(),
                    created: chrono::Local::now().naive_local(),
                };
                self.srv.cat_mut().security.audits.push(audit);
            }
            AuditStmt::AlterAudit { name, target, opts, filter, state } => {
                if !self.can_manage_audits() {
                    return Err(denied("ALTER ANY SERVER AUDIT"));
                }
                let sink = self.srv.core.clone();
                let sec = &mut self.srv.cat_mut().security;
                let Some(x) = sec.audits.iter_mut().find(|x| x.name.eq_ignore_ascii_case(name)) else {
                    return Err(SqlError::with(15151, 16, 1, format!("Cannot alter the server audit '{}', because it does not exist or you do not have permission.", name)));
                };
                if let Some(t) = target {
                    sink.audit.close(x);
                    x.target = t.clone();
                }
                if let Some((_, v)) = opts.iter().find(|(n, _)| n == "ON_FAILURE") {
                    x.on_failure = v.to_uppercase();
                }
                if let Some((_, v)) = opts.iter().find(|(n, _)| n == "QUEUE_DELAY") {
                    x.queue_delay = v.parse().unwrap_or(x.queue_delay);
                }
                if let Some(f) = filter {
                    x.filter = f.clone();
                }
                if let Some(s) = state {
                    x.enabled = *s;
                }
            }
            AuditStmt::DropAudit { name } => {
                if !self.can_manage_audits() {
                    return Err(denied("ALTER ANY SERVER AUDIT"));
                }
                if self.srv.security.audits.iter().find(|x| x.name.eq_ignore_ascii_case(name)).is_some_and(|x| x.enabled) {
                    return Err(SqlError::with(33071, 16, 1, "Cannot drop a server audit while it is enabled. Disable it first (ALTER SERVER AUDIT ... WITH (STATE = OFF))."));
                }
                let core = self.srv.core.clone();
                let sec = &mut self.srv.cat_mut().security;
                let Some(i) = sec.audits.iter().position(|x| x.name.eq_ignore_ascii_case(name)) else {
                    return Err(SqlError::with(15151, 16, 1, format!("Cannot drop the server audit '{}', because it does not exist or you do not have permission.", name)));
                };
                let gone = sec.audits.remove(i);
                core.audit.close(&gone);
                // specifications that fed it are disabled and detached
                for s in sec.server_specs.iter_mut().filter(|s| s.audit.eq_ignore_ascii_case(name)) {
                    s.enabled = false;
                }
                let dbs: Vec<String> = self.srv.databases.keys().cloned().collect();
                for d in dbs {
                    let mut changed = false;
                    if let Some(db) = self.srv.cat_mut().databases.get_mut(&d) {
                        for s in db.audit_specs.iter_mut().filter(|s| s.audit.eq_ignore_ascii_case(name) && s.enabled) {
                            s.enabled = false;
                            changed = true;
                        }
                    }
                    if changed {
                        self.save_db_specs(&d);
                    }
                }
            }
            AuditStmt::CreateSpec { server, name, audit, items, state_on } => {
                self.spec_access(*server)?;
                self.check_items(items, *server)?;
                if !self.srv.security.audits.iter().any(|x| x.name.eq_ignore_ascii_case(audit)) {
                    return Err(SqlError::with(15151, 16, 1, format!("The server audit '{}' does not exist.", audit)));
                }
                let id = self.srv.new_object_id();
                let spec = AuditSpec { name: name.clone(), id, audit: audit.to_lowercase(), enabled: *state_on, items: items.clone() };
                if *server {
                    if self.srv.security.server_specs.iter().any(|s| s.name.eq_ignore_ascii_case(name)) {
                        return Err(SqlError::with(15012, 16, 1, format!("The server audit specification '{}' already exists.", name)));
                    }
                    self.srv.cat_mut().security.server_specs.push(spec);
                } else {
                    let dbk = self.ses.db.to_lowercase();
                    if self.srv.databases[&dbk].audit_specs.iter().any(|s| s.name.eq_ignore_ascii_case(name)) {
                        return Err(SqlError::with(15012, 16, 1, format!("The database audit specification '{}' already exists.", name)));
                    }
                    self.srv.cat_mut().databases.get_mut(&dbk).unwrap().audit_specs.push(spec);
                    self.save_db_specs(&dbk);
                }
            }
            AuditStmt::AlterSpec { server, name, add, drop, state } => {
                self.spec_access(*server)?;
                self.check_items(add, *server)?;
                let dbk = self.ses.db.to_lowercase();
                let missing = || SqlError::with(15151, 16, 1, format!("Cannot alter the audit specification '{}', because it does not exist or you do not have permission.", name));
                let apply = |s: &mut AuditSpec| {
                    for i in add {
                        if !s.items.contains(i) {
                            s.items.push(i.clone());
                        }
                    }
                    s.items.retain(|i| !drop.contains(i));
                    if let Some(on) = state {
                        s.enabled = *on;
                    }
                };
                if *server {
                    let s = self.srv.cat_mut().security.server_specs.iter_mut().find(|s| s.name.eq_ignore_ascii_case(name)).ok_or_else(missing)?;
                    apply(s);
                } else {
                    let db = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                    let s = db.audit_specs.iter_mut().find(|s| s.name.eq_ignore_ascii_case(name)).ok_or_else(missing)?;
                    apply(s);
                    self.save_db_specs(&dbk);
                }
            }
            AuditStmt::DropSpec { server, name } => {
                self.spec_access(*server)?;
                let missing = || SqlError::with(15151, 16, 1, format!("Cannot drop the audit specification '{}', because it does not exist or you do not have permission.", name));
                if *server {
                    let l = &mut self.srv.cat_mut().security.server_specs;
                    let i = l.iter().position(|s| s.name.eq_ignore_ascii_case(name)).ok_or_else(missing)?;
                    l.remove(i);
                } else {
                    let dbk = self.ses.db.to_lowercase();
                    let l = &mut self.srv.cat_mut().databases.get_mut(&dbk).unwrap().audit_specs;
                    let i = l.iter().position(|s| s.name.eq_ignore_ascii_case(name)).ok_or_else(missing)?;
                    l.remove(i);
                    self.save_db_specs(&dbk);
                }
            }
        }
        if matches!(a, AuditStmt::CreateAudit { .. } | AuditStmt::AlterAudit { .. } | AuditStmt::DropAudit { .. } | AuditStmt::CreateSpec { server: true, .. } | AuditStmt::AlterSpec { server: true, .. } | AuditStmt::DropSpec { server: true, .. }) {
            self.save_server_audits();
        }
        Ok(())
    }

    fn spec_access(&self, server: bool) -> Result<()> {
        let ok = if server { self.is_sysadmin() } else { self.can_manage_policies() };
        if ok {
            Ok(())
        } else {
            Err(SqlError::with(262, 14, 1, if server { "ALTER ANY SERVER AUDIT permission denied." } else { "ALTER ANY DATABASE AUDIT permission denied." }))
        }
    }
}
