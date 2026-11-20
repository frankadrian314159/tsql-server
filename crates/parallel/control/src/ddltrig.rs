//! DDL triggers (`CREATE TRIGGER name ON DATABASE FOR CREATE_TABLE, DROP_TABLE ... AS ...`) in a cluster. The control node keeps them and runs
//! their bodies itself (a body reads and writes the cluster's tables, which only the control node can do), around the DDL statements it runs.
//!
//! * A trigger runs **before** the statement, not after it as on a single server (the statement is a change of the shards, and of the control
//!   node's engine, that cannot be taken back as a whole). An error, or a `ROLLBACK` followed by `THROW`/`RAISERROR`, stops the statement, so
//!   a trigger that refuses statements (`DROP_TABLE`, `ALTER_TABLE`) does what it does on a single server. A `ROLLBACK` alone ends the batch with
//!   error 3609, as there. A statement that fails after the triggers ran leaves what they wrote (an audit row, say).
//! * `EVENTDATA()` is the event's XML (`EventType`, `PostTime`, `SPID`, `ServerName`, `LoginName`, `UserName`, `DatabaseName`, `SchemaName`,
//!   `ObjectName`, `ObjectType`, `TSQLCommand/CommandText`), read with the xml `.value()` method.
//! * The events are those of the statements a cluster runs for tables, indexes, views, procedures, functions, triggers, schemas and types.
//!   `ON ALL SERVER` and `LOGON` triggers are not supported. A trigger is kept per database, logged with the schema (the log is replayed when
//!   the control node starts), and is not in the control node's engine, so the catalog views do not list it.
use super::*;
use tsql_server::engine::ddltrig::expand_event;

/// A DDL trigger of a database.
#[derive(Clone, Debug)]
pub(crate) struct DdlTrig {
    pub db: String,
    pub name: String,
    pub events: Vec<String>,
    pub body: Arc<Vec<Stmt>>,
    pub text: String,
    pub disabled: bool,
}

/// The DDL triggers a schema log makes.
pub(crate) fn ddl_triggers_of_log(log: &[Stmt]) -> Vec<DdlTrig> {
    let mut out: Vec<DdlTrig> = vec![];
    let mut db = "master".to_string();
    for st in log {
        match st {
            Stmt::Use(d) => db = d.to_lowercase(),
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("DATABASE") => {
                for n in names {
                    if let Some(d) = n.last() {
                        out.retain(|t| !t.db.eq_ignore_ascii_case(d));
                    }
                }
            }
            Stmt::TriggerAdmin(a) if matches!(a.scope, TriggerScope::Database) => {
                let name_of = |n: &Name| n.last().cloned().unwrap_or_default();
                match &a.act {
                    TriggerAct::Create { events, body, text, .. } => {
                        let t = DdlTrig { db: db.clone(), name: name_of(&a.names[0]), events: events.clone(), body: Arc::new((**body).clone()), text: text.clone(), disabled: false };
                        match out.iter_mut().find(|x| x.db == db && x.name.eq_ignore_ascii_case(&t.name)) {
                            Some(slot) => *slot = t,
                            None => out.push(t),
                        }
                    }
                    TriggerAct::Drop { .. } => {
                        for n in &a.names {
                            out.retain(|x| !(x.db == db && x.name.eq_ignore_ascii_case(&name_of(n))));
                        }
                    }
                    TriggerAct::Enable(on) => {
                        for x in out.iter_mut().filter(|x| x.db == db && (a.all || a.names.iter().any(|n| x.name.eq_ignore_ascii_case(&name_of(n))))) {
                            x.disabled = !*on;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// One event a statement raises.
struct DdlEvent {
    name: &'static str,
    object_type: &'static str,
    schema: String,
    object: String,
}

fn schema_and_name(n: &Name) -> (String, String) {
    let schema = if n.len() >= 2 { n[n.len() - 2].clone() } else { "dbo".to_string() };
    (schema, n.last().cloned().unwrap_or_default())
}

fn events_of(st: &Stmt) -> Vec<DdlEvent> {
    let ev = |name: &'static str, object_type: &'static str, n: &Name| {
        let (schema, object) = schema_and_name(n);
        DdlEvent { name, object_type, schema, object }
    };
    match st {
        Stmt::CreateTable { name, .. } => vec![ev("CREATE_TABLE", "TABLE", name)],
        Stmt::AlterTable { name, .. } => vec![ev("ALTER_TABLE", "TABLE", name)],
        Stmt::CreateIndex { name, table, .. } => {
            let (schema, _) = schema_and_name(table);
            vec![DdlEvent { name: "CREATE_INDEX", object_type: "INDEX", schema, object: name.clone() }]
        }
        Stmt::DropIndex { names, .. } => names
            .iter()
            .map(|(i, t)| {
                let (schema, _) = schema_and_name(t.as_ref().unwrap_or(i));
                DdlEvent { name: "DROP_INDEX", object_type: "INDEX", schema, object: i.last().cloned().unwrap_or_default() }
            })
            .collect(),
        Stmt::CreateView { name, alter, .. } => vec![ev(if *alter { "ALTER_VIEW" } else { "CREATE_VIEW" }, "VIEW", name)],
        Stmt::CreateProc { name, alter, .. } => vec![ev(if *alter { "ALTER_PROCEDURE" } else { "CREATE_PROCEDURE" }, "PROCEDURE", name)],
        Stmt::CreateFunction(f) => vec![ev("CREATE_FUNCTION", "FUNCTION", &f.name)],
        Stmt::CreateTrigger { name, or_alter, .. } => vec![ev(if *or_alter { "ALTER_TRIGGER" } else { "CREATE_TRIGGER" }, "TRIGGER", name)],
        Stmt::CreateSchema(n, _) => vec![DdlEvent { name: "CREATE_SCHEMA", object_type: "SCHEMA", schema: n.clone(), object: n.clone() }],
        Stmt::CreateType { name, .. } => vec![ev("CREATE_TYPE", "TYPE", name)],
        Stmt::Drop { what, names, .. } => {
            let (name, ty): (&'static str, &'static str) = match what.to_uppercase().as_str() {
                "TABLE" => ("DROP_TABLE", "TABLE"),
                "VIEW" => ("DROP_VIEW", "VIEW"),
                "PROCEDURE" | "PROC" => ("DROP_PROCEDURE", "PROCEDURE"),
                "FUNCTION" => ("DROP_FUNCTION", "FUNCTION"),
                "TRIGGER" => ("DROP_TRIGGER", "TRIGGER"),
                "SCHEMA" => ("DROP_SCHEMA", "SCHEMA"),
                "TYPE" => ("DROP_TYPE", "TYPE"),
                _ => return vec![],
            };
            names.iter().map(|n| ev(name, ty, n)).collect()
        }
        _ => vec![],
    }
}

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl ClusterSession {
    /// Whether this session has DDL triggers to run for the statement it is about to run.
    fn ddl_triggers_for(&self, st: &Stmt) -> Vec<(DdlEvent, Vec<DdlTrig>)> {
        if !self.eventdata.is_empty() || self.no_ddl_triggers {
            return vec![];
        }
        let events = events_of(st);
        if events.is_empty() {
            return vec![];
        }
        let db = self.cur_db.to_lowercase();
        let reg = self.cluster.ddl_triggers.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = vec![];
        for e in events {
            let run: Vec<DdlTrig> = reg.iter().filter(|t| t.db == db && !t.disabled && t.events.iter().any(|n| expand_event(n).is_some_and(|l| l.contains(&e.name)))).cloned().collect();
            if !run.is_empty() {
                out.push((e, run));
            }
        }
        out
    }

    /// Run the DDL triggers that match the events `st` raises, before it runs.
    pub(crate) async fn fire_ddl_triggers(&mut self, st: &Stmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let todo = self.ddl_triggers_for(st);
        if todo.is_empty() {
            return Ok(());
        }
        let (login, user, server, now) = self.event_identity();
        for (e, trigs) in todo {
            let xml = format!(
                "<EVENT_INSTANCE><EventType>{}</EventType><PostTime>{}</PostTime><SPID>{}</SPID><ServerName>{}</ServerName><LoginName>{}</LoginName><UserName>{}</UserName><DatabaseName>{}</DatabaseName><SchemaName>{}</SchemaName><ObjectName>{}</ObjectName><ObjectType>{}</ObjectType><TSQLCommand><SetOptions ANSI_NULLS=\"ON\" ANSI_NULL_DEFAULT=\"ON\" ANSI_PADDING=\"ON\" QUOTED_IDENTIFIER=\"ON\" ENCRYPTED=\"FALSE\"/><CommandText>{}</CommandText></TSQLCommand></EVENT_INSTANCE>",
                e.name,
                xml_esc(&now),
                self.id,
                xml_esc(&server),
                xml_esc(&login),
                xml_esc(&user),
                xml_esc(&self.cur_db),
                xml_esc(&e.schema),
                xml_esc(&e.object),
                e.object_type,
                xml_esc(&self.batch_text)
            );
            for t in trigs {
                self.eventdata.push(xml.clone());
                let saved = std::mem::replace(&mut self.ddl_rolled_back, false);
                let body = Stmt::Block((*t.body).clone());
                let r = Box::pin(self.exec_stmt(&body, events)).await;
                self.eventdata.pop();
                self.returning = false;
                let rolled = std::mem::replace(&mut self.ddl_rolled_back, saved);
                r?;
                if rolled {
                    let mut e = SqlError::new(3609, "The transaction ended in the trigger. The batch has been aborted.");
                    e.abort_batch = true;
                    return Err(e);
                }
            }
        }
        self.plan.push("DDL trigger: run by the control node before the statement".into());
        Ok(())
    }

    /// (login, user, server, time) as the session's engine knows them.
    fn event_identity(&mut self) -> (String, String, String, String) {
        let mut out = (String::new(), String::new(), String::new(), String::new());
        if let Ok(stmts) = tsql_server::parser::parse("SELECT SYSTEM_USER, USER_NAME(), @@SERVERNAME, CONVERT(VARCHAR(23), GETDATE(), 126)") {
            if let Ok(rs) = first_rows(self.mirror.execute_stmts(&stmts)) {
                if let Some(r) = rs.rows.first() {
                    let s = |i: usize| r.get(i).map(tsql_server::convert::display).unwrap_or_default();
                    out = (s(0), s(1), s(2), s(3));
                }
            }
        }
        out
    }

    /// `CREATE | ALTER | DROP TRIGGER name ON DATABASE` and `ENABLE | DISABLE TRIGGER name | ALL ON DATABASE`.
    pub(crate) fn ddl_trigger_admin(&mut self, a: &TriggerAdmin) -> Result<(), SqlError> {
        let db = self.cur_db.to_lowercase();
        let name_of = |n: &Name| n.last().cloned().unwrap_or_default();
        match &a.act {
            TriggerAct::Create { events, body, text, or_alter } => {
                let name = name_of(&a.names[0]);
                for e in events {
                    if expand_event(e).is_none() {
                        return Err(SqlError::with(1082, 16, 1, format!("'{}' is an invalid event type.", e)));
                    }
                    if e == "LOGON" {
                        return Err(SqlError::with(1082, 16, 1, "'LOGON' is an invalid event type for a database trigger."));
                    }
                }
                if let Some(why) = triggers::body_problem(body) {
                    return Err(unsupported(&why));
                }
                let mut reg = self.cluster.ddl_triggers.lock().unwrap_or_else(|e| e.into_inner());
                let known = reg.iter().position(|x| x.db == db && x.name.eq_ignore_ascii_case(&name));
                if known.is_some() && !*or_alter {
                    return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", name)));
                }
                let t = DdlTrig { db: db.clone(), name, events: events.clone(), body: Arc::new((**body).clone()), text: text.clone(), disabled: false };
                match known {
                    Some(i) => reg[i] = t,
                    None => reg.push(t),
                }
            }
            TriggerAct::Drop { if_exists } => {
                let mut reg = self.cluster.ddl_triggers.lock().unwrap_or_else(|e| e.into_inner());
                for n in &a.names {
                    let name = name_of(n);
                    match reg.iter().position(|x| x.db == db && x.name.eq_ignore_ascii_case(&name)) {
                        Some(i) => {
                            reg.remove(i);
                        }
                        None if *if_exists => {}
                        None => return Err(SqlError::new(3701, format!("Cannot drop the trigger '{}', because it does not exist or you do not have permission.", name))),
                    }
                }
            }
            TriggerAct::Enable(on) => {
                let mut reg = self.cluster.ddl_triggers.lock().unwrap_or_else(|e| e.into_inner());
                if a.all {
                    reg.iter_mut().filter(|x| x.db == db).for_each(|x| x.disabled = !*on);
                } else {
                    for n in &a.names {
                        let name = name_of(n);
                        match reg.iter_mut().find(|x| x.db == db && x.name.eq_ignore_ascii_case(&name)) {
                            Some(x) => x.disabled = !*on,
                            None => return Err(SqlError::new(4947, format!("Cannot find the trigger '{}', because it does not exist or you do not have permission.", name))),
                        }
                    }
                }
            }
        }
        self.log_schema(&Stmt::TriggerAdmin(Box::new(a.clone())));
        Ok(())
    }
}
