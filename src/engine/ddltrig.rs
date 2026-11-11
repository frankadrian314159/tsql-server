//! DDL triggers and LOGON triggers, and `ENABLE | DISABLE TRIGGER`.
//!
//! ```sql
//! CREATE TRIGGER no_drops ON DATABASE FOR DROP_TABLE, ALTER_TABLE AS
//! BEGIN
//!     ROLLBACK;
//!     THROW 50001, 'Tables cannot be dropped or altered here.', 1;
//! END
//! CREATE TRIGGER audit_ddl ON ALL SERVER FOR DDL_DATABASE_LEVEL_EVENTS AS
//!     INSERT audit.dbo.log VALUES (EVENTDATA().value('(/EVENT_INSTANCE/EventType)[1]', 'nvarchar(100)'), SYSTEM_USER);
//! CREATE TRIGGER limit_logins ON ALL SERVER FOR LOGON AS
//!     IF ORIGINAL_LOGIN() = 'contractor' ROLLBACK;
//! ```
//!
//! A DDL trigger runs after the statement, in the statement's transaction and with the caller's permissions;
//! an error, or a `ROLLBACK`, undoes the statement. A LOGON trigger runs after authentication; `ROLLBACK` (or an
//! error) refuses the login with error 17892. `EVENTDATA()` returns the event's XML (`EventType`, `PostTime`, `SPID`,
//! `ServerName`, `LoginName`, `UserName`, `DatabaseName`, `SchemaName`, `ObjectName`, `ObjectType` and
//! `TSQLCommand/CommandText`); read it with the xml `.value()` method.
use super::*;
use crate::catalog::DdlTrigger;

/// One event a statement raised.
pub struct DdlEvent {
    pub name: &'static str,
    pub object_type: &'static str,
    pub schema: String,
    pub object: String,
    /// The database the event happened in (for database-level events), or the object itself for CREATE/DROP DATABASE.
    pub database: String,
    /// Only server-scoped triggers see this event (`CREATE_DATABASE`, logins, ...).
    pub server_only: bool,
}

const TABLE_EVENTS: &[&str] = &["CREATE_TABLE", "ALTER_TABLE", "DROP_TABLE"];
const VIEW_EVENTS: &[&str] = &["CREATE_VIEW", "ALTER_VIEW", "DROP_VIEW"];
const INDEX_EVENTS: &[&str] = &["CREATE_INDEX", "ALTER_INDEX", "DROP_INDEX"];
const PROC_EVENTS: &[&str] = &["CREATE_PROCEDURE", "ALTER_PROCEDURE", "DROP_PROCEDURE"];
const FUNC_EVENTS: &[&str] = &["CREATE_FUNCTION", "ALTER_FUNCTION", "DROP_FUNCTION"];
const TRIGGER_EVENTS: &[&str] = &["CREATE_TRIGGER", "ALTER_TRIGGER", "DROP_TRIGGER"];
const SCHEMA_EVENTS: &[&str] = &["CREATE_SCHEMA", "ALTER_SCHEMA", "DROP_SCHEMA"];
const TYPE_EVENTS: &[&str] = &["CREATE_TYPE", "DROP_TYPE"];
const SEQ_EVENTS: &[&str] = &["CREATE_SEQUENCE", "ALTER_SEQUENCE", "DROP_SEQUENCE"];
const USER_EVENTS: &[&str] = &["CREATE_USER", "ALTER_USER", "DROP_USER"];
const ROLE_EVENTS: &[&str] = &["CREATE_ROLE", "ALTER_ROLE", "DROP_ROLE"];
const GDR_EVENTS: &[&str] = &["GRANT_DATABASE", "DENY_DATABASE", "REVOKE_DATABASE"];
const LOGIN_EVENTS: &[&str] = &["CREATE_LOGIN", "ALTER_LOGIN", "DROP_LOGIN"];
const DATABASE_EVENTS: &[&str] = &["CREATE_DATABASE", "ALTER_DATABASE", "DROP_DATABASE"];

/// The concrete events a trigger's event name stands for (itself, or the members of an event group); `None` if unknown.
pub fn expand_event(name: &str) -> Option<Vec<&'static str>> {
    let groups: &[(&str, &[&[&str]])] = &[
        ("DDL_TABLE_EVENTS", &[TABLE_EVENTS]),
        ("DDL_VIEW_EVENTS", &[VIEW_EVENTS]),
        ("DDL_INDEX_EVENTS", &[INDEX_EVENTS]),
        ("DDL_TABLE_VIEW_EVENTS", &[TABLE_EVENTS, VIEW_EVENTS, INDEX_EVENTS]),
        ("DDL_PROCEDURE_EVENTS", &[PROC_EVENTS]),
        ("DDL_FUNCTION_EVENTS", &[FUNC_EVENTS]),
        ("DDL_TRIGGER_EVENTS", &[TRIGGER_EVENTS]),
        ("DDL_SCHEMA_EVENTS", &[SCHEMA_EVENTS]),
        ("DDL_TYPE_EVENTS", &[TYPE_EVENTS]),
        ("DDL_SEQUENCE_EVENTS", &[SEQ_EVENTS]),
        ("DDL_USER_EVENTS", &[USER_EVENTS]),
        ("DDL_ROLE_EVENTS", &[ROLE_EVENTS]),
        ("DDL_GDR_DATABASE_EVENTS", &[GDR_EVENTS]),
        ("DDL_LOGIN_EVENTS", &[LOGIN_EVENTS]),
        ("DDL_DATABASE_EVENTS", &[DATABASE_EVENTS]),
        (
            "DDL_DATABASE_LEVEL_EVENTS",
            &[TABLE_EVENTS, VIEW_EVENTS, INDEX_EVENTS, PROC_EVENTS, FUNC_EVENTS, TRIGGER_EVENTS, SCHEMA_EVENTS, TYPE_EVENTS, SEQ_EVENTS, USER_EVENTS, ROLE_EVENTS, GDR_EVENTS, &["ALTER_DATABASE"]],
        ),
        ("DDL_SERVER_LEVEL_EVENTS", &[LOGIN_EVENTS, DATABASE_EVENTS]),
    ];
    if let Some((_, g)) = groups.iter().find(|(n, _)| *n == name) {
        return Some(g.iter().flat_map(|l| l.iter().copied()).collect());
    }
    let all = [TABLE_EVENTS, VIEW_EVENTS, INDEX_EVENTS, PROC_EVENTS, FUNC_EVENTS, TRIGGER_EVENTS, SCHEMA_EVENTS, TYPE_EVENTS, SEQ_EVENTS, USER_EVENTS, ROLE_EVENTS, GDR_EVENTS, LOGIN_EVENTS, DATABASE_EVENTS, &["LOGON"]];
    all.iter().flat_map(|l| l.iter()).find(|e| **e == name).map(|e| vec![*e])
}

/// Can executing this statement raise a DDL event?
pub fn is_ddl_stmt(st: &Stmt) -> bool {
    match st {
        Stmt::CreateTable { .. }
        | Stmt::CreateIndex { .. }
        | Stmt::CreateView { .. }
        | Stmt::CreateProc { .. }
        | Stmt::CreateFunction(_)
        | Stmt::CreateTrigger { .. }
        | Stmt::CreateSchema(..)
        | Stmt::CreateDatabase(..)
        | Stmt::CreateType { .. }
        | Stmt::CreateSequence { .. }
        | Stmt::CreateSynonym { .. }
        | Stmt::AlterSequence { .. }
        | Stmt::AlterTable { .. }
        | Stmt::AlterDatabase { .. }
        | Stmt::Drop { .. }
        | Stmt::Backup(_)
        | Stmt::Restore(_)
        | Stmt::Dbcc { .. }
        | Stmt::FullText(_)
        | Stmt::XmlSchema(_)
        | Stmt::DropIndex { .. } => true,
        Stmt::Security(s) => matches!(
            **s,
            SecStmt::CreateLogin { .. } | SecStmt::AlterLogin { .. } | SecStmt::CreateUser { .. } | SecStmt::AlterUser { .. } | SecStmt::CreateRole { .. } | SecStmt::AlterRole { .. } | SecStmt::Perm { .. }
        ),
        _ => false,
    }
}

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl<'a> Engine<'a> {
    /// The events `st` raised (already executed).
    fn ddl_events(&self, st: &Stmt) -> Vec<DdlEvent> {
        let db = self.ses.db.clone();
        let split = |name: &[String]| -> (String, String) {
            let (_, schema, n) = self.ddl_names(name);
            (schema, n)
        };
        let ev = |name: &'static str, object_type: &'static str, schema: String, object: String, server_only: bool| DdlEvent { name, object_type, schema, object, database: if server_only { String::new() } else { db.clone() }, server_only };
        let simple = |name: &'static str, object_type: &'static str, n: &Name| {
            let (s, o) = split(n);
            ev(name, object_type, s, o, false)
        };
        match st {
            Stmt::CreateTable { name, .. } => vec![simple("CREATE_TABLE", "TABLE", name)],
            Stmt::AlterTable { name, .. } => vec![simple("ALTER_TABLE", "TABLE", name)],
            Stmt::CreateIndex { name, table, .. } => {
                let (s, _) = split(table);
                vec![ev("CREATE_INDEX", "INDEX", s, name.clone(), false)]
            }
            Stmt::DropIndex { names, .. } => names
                .iter()
                .map(|(i, t)| {
                    let (s, _) = split(t.as_ref().unwrap_or(i));
                    ev("DROP_INDEX", "INDEX", s, i.last().cloned().unwrap_or_default(), false)
                })
                .collect(),
            Stmt::CreateView { name, alter, .. } => vec![simple(if *alter { "ALTER_VIEW" } else { "CREATE_VIEW" }, "VIEW", name)],
            Stmt::CreateProc { name, alter, .. } => vec![simple(if *alter { "ALTER_PROCEDURE" } else { "CREATE_PROCEDURE" }, "PROCEDURE", name)],
            Stmt::CreateFunction(f) => vec![simple("CREATE_FUNCTION", "FUNCTION", &f.name)],
            Stmt::CreateTrigger { name, or_alter, .. } => vec![simple(if *or_alter { "ALTER_TRIGGER" } else { "CREATE_TRIGGER" }, "TRIGGER", name)],
            Stmt::CreateSchema(n, _) => vec![ev("CREATE_SCHEMA", "SCHEMA", n.clone(), n.clone(), false)],
            Stmt::CreateType { name, .. } => vec![simple("CREATE_TYPE", "TYPE", name)],
            Stmt::CreateSequence { name, .. } => vec![simple("CREATE_SEQUENCE", "SEQUENCE", name)],
            Stmt::CreateSynonym { name, .. } => vec![simple("CREATE_SYNONYM", "SYNONYM", name)],
            Stmt::AlterSequence { name, .. } => vec![simple("ALTER_SEQUENCE", "SEQUENCE", name)],
            Stmt::CreateDatabase(n, _) => vec![DdlEvent { name: "CREATE_DATABASE", object_type: "DATABASE", schema: String::new(), object: n.clone(), database: n.clone(), server_only: true }],
            Stmt::AlterDatabase { db: d, .. } => vec![DdlEvent { name: "ALTER_DATABASE", object_type: "DATABASE", schema: String::new(), object: d.clone(), database: d.clone(), server_only: false }],
            Stmt::Drop { what, names, .. } => {
                let (name, ty, server_only): (&'static str, &'static str, bool) = match what.as_str() {
                    "TABLE" => ("DROP_TABLE", "TABLE", false),
                    "VIEW" => ("DROP_VIEW", "VIEW", false),
                    "PROCEDURE" => ("DROP_PROCEDURE", "PROCEDURE", false),
                    "FUNCTION" => ("DROP_FUNCTION", "FUNCTION", false),
                    "TRIGGER" => ("DROP_TRIGGER", "TRIGGER", false),
                    "SCHEMA" => ("DROP_SCHEMA", "SCHEMA", false),
                    "TYPE" => ("DROP_TYPE", "TYPE", false),
                    "SEQUENCE" => ("DROP_SEQUENCE", "SEQUENCE", false),
                    "USER" => ("DROP_USER", "USER", false),
                    "ROLE" => ("DROP_ROLE", "ROLE", false),
                    "LOGIN" => ("DROP_LOGIN", "LOGIN", true),
                    "DATABASE" => ("DROP_DATABASE", "DATABASE", true),
                    _ => return vec![],
                };
                names
                    .iter()
                    .map(|n| {
                        let (s, o) = split(n);
                        let database = if what == "DATABASE" { n.last().cloned().unwrap_or_default() } else if server_only { String::new() } else { db.clone() };
                        DdlEvent { name, object_type: ty, schema: if matches!(what.as_str(), "LOGIN" | "DATABASE" | "USER" | "ROLE" | "SCHEMA") { String::new() } else { s }, object: o, database, server_only }
                    })
                    .collect()
            }
            Stmt::Security(s) => match &**s {
                SecStmt::CreateLogin { name, .. } => vec![ev("CREATE_LOGIN", "LOGIN", String::new(), name.clone(), true)],
                SecStmt::AlterLogin { name, .. } => vec![ev("ALTER_LOGIN", "LOGIN", String::new(), name.clone(), true)],
                SecStmt::CreateUser { name, .. } => vec![ev("CREATE_USER", "USER", String::new(), name.clone(), false)],
                SecStmt::AlterUser { name, .. } => vec![ev("ALTER_USER", "USER", String::new(), name.clone(), false)],
                SecStmt::CreateRole { name } => vec![ev("CREATE_ROLE", "ROLE", String::new(), name.clone(), false)],
                SecStmt::AlterRole { role, .. } => vec![ev("ALTER_ROLE", "ROLE", String::new(), role.clone(), false)],
                SecStmt::Perm { kind, target, .. } => {
                    let name = match kind {
                        PermKind::Grant => "GRANT_DATABASE",
                        PermKind::Deny => "DENY_DATABASE",
                        PermKind::Revoke => "REVOKE_DATABASE",
                    };
                    vec![match target {
                        PermTargetAst::Database => ev(name, "DATABASE", String::new(), db.clone(), false),
                        PermTargetAst::Schema(s) => ev(name, "SCHEMA", s.clone(), String::new(), false),
                        PermTargetAst::Object(n) => {
                            let (s, o) = split(n);
                            ev(name, "OBJECT", s, o, false)
                        }
                    }]
                }
                _ => vec![],
            },
            _ => vec![],
        }
    }

    fn event_xml(&self, e: &DdlEvent, command: &str) -> String {
        let now = chrono::Local::now().naive_local();
        let mut x = String::from("<EVENT_INSTANCE>");
        let mut tag = |n: &str, v: &str| x.push_str(&format!("<{n}>{}</{n}>", xml_esc(v)));
        tag("EventType", e.name);
        tag("PostTime", &now.format("%Y-%m-%dT%H:%M:%S%.3f").to_string());
        tag("SPID", &self.ses.spid.to_string());
        tag("ServerName", &self.srv.server_name);
        tag("LoginName", &self.eff_login());
        tag("UserName", &self.current_user_name());
        tag("DatabaseName", &e.database);
        tag("SchemaName", &e.schema);
        tag("ObjectName", &e.object);
        tag("ObjectType", e.object_type);
        x.push_str(&format!("<TSQLCommand><SetOptions ANSI_NULLS=\"ON\" ANSI_NULL_DEFAULT=\"ON\" ANSI_PADDING=\"ON\" QUOTED_IDENTIFIER=\"ON\" ENCRYPTED=\"FALSE\"/><CommandText>{}</CommandText></TSQLCommand>", xml_esc(command)));
        x.push_str("</EVENT_INSTANCE>");
        x
    }

    /// Run the DDL triggers that match the events `st` raised (it has just run successfully).
    pub fn fire_ddl_triggers(&mut self, st: &Stmt) -> Flow<()> {
        match st {
            Stmt::Backup(b) => self.audit_server_group("BACKUP_RESTORE_GROUP", "BACKUP", &b.db),
            Stmt::Restore(r) => self.audit_server_group("BACKUP_RESTORE_GROUP", "RESTORE", r.db.as_deref().unwrap_or("")),
            Stmt::Dbcc { name, .. } => self.audit_server_group("DBCC_GROUP", "DBCC", name),
            _ => {}
        }
        let dbk = self.ses.db.to_lowercase();
        let has_db = self.srv.databases.get(&dbk).is_some_and(|d| !d.ddl_triggers.is_empty());
        let audited = self.audit_on();
        if !has_db && self.srv.security.server_triggers.is_empty() && !audited {
            return Ok(());
        }
        let command = if self.ses.batch_text.is_empty() { super::stmt_head(st) } else { self.ses.batch_text.clone() };
        let events = self.ddl_events(st);
        if audited {
            self.audit_ddl(&events);
        }
        for e in events {
            let mut run: Vec<Arc<DdlTrigger>> = vec![];
            let matches = |t: &DdlTrigger| !t.disabled && t.events.iter().any(|n| expand_event(n).is_some_and(|l| l.contains(&e.name)));
            if !e.server_only {
                // a database-level event runs the triggers of the database it happened in
                if let Some(d) = self.srv.databases.get(&e.database.to_lowercase()).or_else(|| self.srv.databases.get(&dbk)) {
                    run.extend(d.ddl_triggers.iter().filter(|t| matches(t)).cloned().map(Arc::new));
                }
            }
            run.extend(self.srv.security.server_triggers.iter().filter(|t| matches(t)).cloned().map(Arc::new));
            if run.is_empty() {
                continue;
            }
            let xml = self.event_xml(&e, &command);
            for t in run {
                self.run_ddl_trigger(&t, xml.clone())?;
            }
        }
        Ok(())
    }

    /// Run one trigger body with `EVENTDATA()` set. The trigger sees an open transaction (`@@TRANCOUNT` 1), so a
    /// `ROLLBACK` is legal; ending the transaction aborts the batch with error 3609, as for a DML trigger.
    pub fn run_ddl_trigger(&mut self, trg: &DdlTrigger, xml: String) -> Result<()> {
        if self.ses.nest_level >= 32 {
            return Err(SqlError::new(217, "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32)."));
        }
        self.ensure_tx()?;
        let outer_trancount = self.ses.trancount;
        if outer_trancount == 0 {
            self.ses.trancount = 1;
        }
        self.ses.eventdata.push(xml);
        self.ses.frames.push(Frame::default());
        self.ses.strict += 1;
        self.ses.nest_level += 1;
        self.ses.module_owner.push(String::new());
        let r = self.exec_stmts(&trg.body);
        self.ses.module_owner.pop();
        self.ses.nest_level -= 1;
        self.ses.strict -= 1;
        self.ses.frames.pop();
        self.ses.eventdata.pop();
        let ended = self.ses.tx.is_none();
        if !ended && outer_trancount == 0 {
            self.ses.trancount = 0;
        }
        match r {
            Ok(()) | Err(Interrupt::Return) => {}
            Err(Interrupt::Error(e)) => return Err(e),
            Err(_) => {}
        }
        if ended {
            let mut e = SqlError::new(3609, "The transaction ended in the trigger. The batch has been aborted.");
            e.abort_batch = true;
            return Err(e);
        }
        Ok(())
    }

    /// Run the LOGON triggers for the session that has just authenticated; an error or `ROLLBACK` refuses the login.
    pub fn run_logon_triggers(&mut self) -> Result<()> {
        let trgs: Vec<DdlTrigger> = self.srv.security.server_triggers.iter().filter(|t| !t.disabled && t.events.iter().any(|e| e == "LOGON")).cloned().collect();
        if trgs.is_empty() {
            return Ok(());
        }
        let now = chrono::Local::now().naive_local();
        let xml = format!(
            "<EVENT_INSTANCE><EventType>LOGON</EventType><PostTime>{}</PostTime><SPID>{}</SPID><ServerName>{}</ServerName><LoginName>{}</LoginName><LoginType>SQL Login</LoginType><IsPooled>0</IsPooled></EVENT_INSTANCE>",
            now.format("%Y-%m-%dT%H:%M:%S%.3f"),
            self.ses.spid,
            xml_esc(&self.srv.server_name),
            xml_esc(&self.ses.login)
        );
        let refused = |login: &str| SqlError::with(17892, 14, 1, format!("Logon failed for login '{}' due to trigger execution.", login));
        let login = self.ses.login.clone();
        self.ses.frames.push(Frame::default());
        let mut outcome = Ok(());
        for t in trgs {
            if let Err(e) = self.run_ddl_trigger(&t, xml.clone()) {
                outcome = Err(e);
                break;
            }
            if let Err(e) = self.commit_tx() {
                outcome = Err(e);
                break;
            }
        }
        self.rollback_all();
        self.ses.frames.clear();
        self.ses.trigger_stack.clear();
        self.ses.capture.clear();
        self.ses.catch_stack.clear();
        self.ses.nest_level = 0;
        self.out.clear();
        outcome.map_err(|_| refused(&login))
    }

    pub fn exec_trigger_admin(&mut self, a: &TriggerAdmin) -> Result<()> {
        match (&a.scope, &a.act) {
            (TriggerScope::Table(t), TriggerAct::Enable(on)) => {
                let names: Vec<String> = a.names.iter().map(|n| n.last().cloned().unwrap_or_default()).collect();
                return self.set_dml_triggers_enabled(t, &names, a.all, *on);
            }
            (TriggerScope::Table(_), _) => return Err(SqlError::new(102, "Incorrect syntax near 'ON'.")),
            _ => {}
        }
        let server = matches!(a.scope, TriggerScope::Server);
        if server {
            if !self.is_sysadmin() {
                return Err(SqlError::with(262, 14, 1, "CREATE TRIGGER permission denied in database 'master'."));
            }
        } else if !self.can_manage_policies() {
            return Err(SqlError::with(262, 14, 1, "CREATE TRIGGER permission denied in database."));
        }
        let dbk = self.ses.db.to_lowercase();
        let mut list: Vec<DdlTrigger> = if server { self.srv.security.server_triggers.clone() } else { self.srv.databases.get(&dbk).map(|d| d.ddl_triggers.clone()).unwrap_or_default() };
        let pos = |l: &[DdlTrigger], n: &str| l.iter().position(|t| t.name.eq_ignore_ascii_case(n));
        match &a.act {
            TriggerAct::Create { events, body, text, or_alter } => {
                let name = a.names[0].last().cloned().unwrap_or_default();
                for e in events {
                    if expand_event(e).is_none() {
                        return Err(SqlError::with(1082, 16, 1, format!("'{}' is an invalid event type.", e)));
                    }
                    if !server && e == "LOGON" {
                        return Err(SqlError::with(1082, 16, 1, "'LOGON' is an invalid event type for a database trigger."));
                    }
                }
                let existing = pos(&list, &name);
                if existing.is_some() && !*or_alter {
                    return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", name)));
                }
                let oid = match existing {
                    Some(i) => list[i].object_id,
                    None => self.srv.new_object_id(),
                };
                let t = DdlTrigger { name, events: events.clone(), body: Arc::new((**body).clone()), text: text.clone(), disabled: false, object_id: oid };
                match existing {
                    Some(i) => list[i] = t,
                    None => list.push(t),
                }
            }
            TriggerAct::Drop { if_exists } => {
                for n in &a.names {
                    let name = n.last().cloned().unwrap_or_default();
                    match pos(&list, &name) {
                        Some(i) => {
                            list.remove(i);
                        }
                        None if *if_exists => {}
                        None => return Err(SqlError::new(3701, format!("Cannot drop the trigger '{}', because it does not exist or you do not have permission.", name))),
                    }
                }
            }
            TriggerAct::Enable(on) => {
                if a.all {
                    list.iter_mut().for_each(|t| t.disabled = !*on);
                } else {
                    for n in &a.names {
                        let name = n.last().cloned().unwrap_or_default();
                        match pos(&list, &name) {
                            Some(i) => list[i].disabled = !*on,
                            None => return Err(SqlError::new(4947, format!("Cannot find the trigger '{}', because it does not exist or you do not have permission.", name))),
                        }
                    }
                }
            }
        }
        if server {
            self.srv.cat_mut().security.server_triggers = list;
            self.log_security();
        } else {
            self.srv.cat_mut().databases.get_mut(&dbk).expect("current database").ddl_triggers = list.clone();
            self.log_nowait(vec![crate::persist::LogOp::DbDdlTriggers { db: dbk, triggers: list }]);
        }
        Ok(())
    }
}
