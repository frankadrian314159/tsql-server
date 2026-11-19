//! Databases in a cluster.
//!
//! A cluster has the databases `CREATE DATABASE` made (on the control node's engine and on every shard) besides `master`. A table
//! is named by its database, schema and name (`db.schema.table`; the id of a table of `master` has no database in it, as it never
//! had, so a saved state reads as it was).
//!
//! The statements of a client name tables the way a client does, relative to the database it is in (`USE`). Before the cluster runs
//! a statement of a session that is in a database other than `master`, `qualify` gives the tables that statement names the
//! database they are in (`Person.Person` becomes `advw.Person.Person`), so that everything after that (the plan, the SQL the shards
//! are sent, the ids of the catalog) names a table in full. What a statement says about a database as a whole (`CREATE SCHEMA`,
//! `CREATE VIEW`, `CREATE PROCEDURE`) is sent behind a `USE` of the database, and logged behind one.
use crate::*;
use serde_json::Value as Json;

/// The databases a schema log makes (besides `master`) and the one its last statement is in.
pub(crate) fn databases_of_log(log: &[Stmt]) -> (Vec<String>, String) {
    let mut dbs: Vec<String> = vec![];
    let mut cur = "master".to_string();
    for st in log {
        match st {
            Stmt::CreateDatabase(n, _) => dbs.push(n.clone()),
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("DATABASE") => dbs.retain(|d| !names.iter().any(|n| n.last().is_some_and(|x| x.eq_ignore_ascii_case(d)))),
            Stmt::Use(d) => cur = d.clone(),
            _ => {}
        }
    }
    (dbs, cur)
}

fn is_temp(n: &[String]) -> bool {
    n.last().is_some_and(|x| x.starts_with('#'))
}

impl ClusterSession {
    /// The database the session is in.
    pub fn current_database(&self) -> &str {
        &self.cur_db
    }

    /// The database the client's connection is in (the front end tells it before each batch: the engine ran any `USE`).
    pub fn set_current_database(&mut self, db: &str) {
        if !self.cur_db.eq_ignore_ascii_case(db) {
            self.cur_db = db.to_string();
        }
    }

    /// Whether the cluster has a database besides `master`.
    pub(crate) fn multi_db(&self) -> bool {
        !self.cluster.dbs.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    fn in_master(&self) -> bool {
        self.cur_db.eq_ignore_ascii_case("master")
    }

    /// A table name in full, in the session's database (a name that has its database is as it is; a temporary table's has none).
    pub(crate) fn qn(&self, name: &Name) -> Name {
        if self.in_master() || name.len() >= 3 || is_temp(name) {
            return name.clone();
        }
        let schema = if name.len() == 2 { name[0].clone() } else { "dbo".to_string() };
        vec![self.cur_db.clone(), schema, name.last().cloned().unwrap_or_default()]
    }

    /// The id of the table of a name in the session's database.
    pub(crate) fn qid(&self, name: &Name) -> String {
        table_id(&self.qn(name))
    }

    fn known_name(&self, name: &Name) -> bool {
        let id = self.qid(name);
        self.table_by_id(&id).is_some() || self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) || self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id)
    }

    /// The statement with the tables it names in full (see the top of this file). A session in `master` is left as it is.
    pub(crate) fn qualify(&self, st: &Stmt) -> Stmt {
        if self.in_master() {
            return st.clone();
        }
        let Ok(mut v) = serde_json::to_value(st) else { return st.clone() };
        self.qualify_json(&mut v);
        serde_json::from_value(v).unwrap_or_else(|_| st.clone())
    }

    fn qualify_json(&self, v: &mut Json) {
        fn names_of(v: &Json) -> Option<Name> {
            v.as_array()?.iter().map(|p| p.as_str().map(|s| s.to_string())).collect()
        }
        fn put(v: &mut Json, n: &Name) {
            *v = Json::Array(n.iter().map(|s| Json::String(s.clone())).collect());
        }
        match v {
            Json::Object(m) => {
                // what a statement makes or drops is named in full whether or not it is there yet; what it reads or changes, when it is a
                // table (or view) of the cluster
                for (key, always) in [("CreateTable", true), ("AlterTable", false), ("CreateView", true), ("CreateProc", true), ("CreateIndex", false), ("AlterIndex", false), ("Exec", false)] {
                    if let Some(Json::Object(inner)) = m.get_mut(key) {
                        let field = if matches!(key, "CreateIndex" | "AlterIndex") { "table" } else { "name" };
                        if let Some(n) = inner.get(field).and_then(names_of) {
                            if !n.is_empty() && (always || self.known_name(&n)) && !is_temp(&n) {
                                put(inner.get_mut(field).unwrap(), &self.qn(&n));
                            }
                        }
                    }
                }
                // the table a foreign key refers to (`ForeignKeyDef`: a column's, a table's, or an `ALTER TABLE ... ADD`'s)
                if m.contains_key("rcols") && m.contains_key("on_delete") {
                    if let Some(n) = m.get("table").and_then(names_of) {
                        if !n.is_empty() && !is_temp(&n) {
                            put(m.get_mut("table").unwrap(), &self.qn(&n));
                        }
                    }
                }
                // the node tables an edge constraint connects
                if let Some(Json::Object(inner)) = m.get_mut("EdgeConstraint") {
                    if let Some(Json::Array(pairs)) = inner.get_mut("pairs") {
                        for p in pairs.iter_mut() {
                            if let Json::Array(two) = p {
                                for nm in two.iter_mut() {
                                    if let Some(n) = names_of(nm) {
                                        if !n.is_empty() {
                                            put(nm, &self.qn(&n));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(Json::Object(inner)) = m.get_mut("Drop") {
                    let tables = inner.get("what").and_then(|w| w.as_str()).is_some_and(|w| matches!(w.to_uppercase().as_str(), "TABLE" | "VIEW" | "PROC" | "PROCEDURE"));
                    if tables {
                        if let Some(Json::Array(names)) = inner.get_mut("names") {
                            for nm in names.iter_mut() {
                                if let Some(n) = names_of(nm) {
                                    if !n.is_empty() && !is_temp(&n) {
                                        put(nm, &self.qn(&n));
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(Json::Object(inner)) = m.get_mut("DropIndex") {
                    if let Some(Json::Array(pairs)) = inner.get_mut("names") {
                        for p in pairs.iter_mut() {
                            if let Some(Json::Array(two)) = Some(p).filter(|p| p.is_array()) {
                                if let Some(first) = two.first_mut() {
                                    if let Some(n) = names_of(first) {
                                        if !n.is_empty() && !is_temp(&n) {
                                            put(first, &self.qn(&n));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // a table of a query (`{"Table": {"name": [...], ...}}`), and the target of a statement that changes one (`{"Table": [...]}`)
                if m.len() == 1 {
                    if let Some(t) = m.get_mut("Table") {
                        match t {
                            Json::Object(inner) => {
                                if let Some(n) = inner.get("name").and_then(names_of) {
                                    if !n.is_empty() && !is_temp(&n) && self.known_name(&n) {
                                        put(inner.get_mut("name").unwrap(), &self.qn(&n));
                                    }
                                }
                            }
                            Json::Array(_) => {
                                if let Some(n) = names_of(t) {
                                    if !n.is_empty() && !is_temp(&n) && self.known_name(&n) {
                                        put(t, &self.qn(&n));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                for c in m.values_mut() {
                    self.qualify_json(c);
                }
            }
            Json::Array(a) => {
                for c in a.iter_mut() {
                    self.qualify_json(c);
                }
            }
            _ => {}
        }
    }

    /// The mirror (the control node's engine) is in the session's database.
    pub(crate) fn sync_mirror_db(&mut self) -> Result<(), SqlError> {
        if !self.mirror.current_database().eq_ignore_ascii_case(&self.cur_db) {
            merge::check(&self.mirror.execute_stmts(&[Stmt::Use(self.cur_db.clone())]))?;
        }
        Ok(())
    }

    /// The work that runs a statement of the whole database (a schema, a view, a procedure) on a shard: behind a `USE` of the session's
    /// database when the cluster has more than one.
    pub(crate) fn db_work(&self, st: Stmt) -> Work {
        if self.multi_db() {
            Work::Stmts(vec![Stmt::Use(self.cur_db.clone()), st])
        } else {
            Work::Stmts(vec![st])
        }
    }

    /// Log a statement of the schema, made in the session's database.
    pub(crate) fn log_schema(&self, st: &Stmt) {
        self.cluster.log_schema_in(&self.cur_db, st);
    }

    /// `USE db`.
    pub(crate) fn use_database(&mut self, db: &str) -> Result<(), SqlError> {
        let known = db.eq_ignore_ascii_case("master") || self.cluster.dbs.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|d| d.eq_ignore_ascii_case(db));
        if !known {
            return Err(SqlError::with(911, 16, 1, format!("Database '{}' does not exist. Make sure that the name is entered correctly.", db)));
        }
        let shown = if db.eq_ignore_ascii_case("master") { "master".to_string() } else { self.cluster.dbs.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|d| d.eq_ignore_ascii_case(db)).cloned().unwrap_or_else(|| db.to_string()) };
        self.cur_db = shown;
        self.sync_mirror_db()
    }

    /// `CREATE DATABASE`: made on the control node's engine, then on every shard.
    pub(crate) async fn create_database(&mut self, st: &Stmt, name: &str) -> Result<(), SqlError> {
        if name.eq_ignore_ascii_case("master") || self.cluster.dbs.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|d| d.eq_ignore_ascii_case(name)) {
            return Err(SqlError::with(1801, 16, 3, format!("Database '{}' already exists. Choose a different database name.", name)));
        }
        merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
        let shards = self.all_shards();
        if let Err(e) = self.fan_run(&shards, |_| Work::Stmts(vec![st.clone()])).await {
            let drop = Stmt::Drop { what: "DATABASE".into(), names: vec![vec![name.to_string()]], if_exists: true };
            let _ = self.mirror.execute_stmts(std::slice::from_ref(&drop));
            return Err(e);
        }
        self.plan.push(format!("broadcast CREATE DATABASE to {} shards", shards.len()));
        self.cluster.log_schema_in("master", st);
        Ok(())
    }

    /// `DROP DATABASE`: its tables, views and procedures are forgotten, and the database is dropped from the control node's engine and every shard.
    pub(crate) async fn drop_database(&mut self, st: &Stmt, names: &[Name], if_exists: bool) -> Result<(), SqlError> {
        for n in names {
            let name = n.last().cloned().unwrap_or_default();
            let known = self.cluster.dbs.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|d| d.eq_ignore_ascii_case(&name)).cloned();
            let Some(db) = known else {
                if name.eq_ignore_ascii_case("master") {
                    return Err(SqlError::with(3701, 16, 1, "Cannot drop the database 'master' because it is a system database."));
                }
                if if_exists {
                    continue;
                }
                return Err(SqlError::with(3701, 16, 1, format!("Cannot drop the database '{}', because it does not exist or you do not have permission.", name)));
            };
            if self.cur_db.eq_ignore_ascii_case(&db) {
                return Err(SqlError::with(3702, 16, 3, format!("Cannot drop database \"{}\" because it is currently in use.", db)));
            }
            let one = Stmt::Drop { what: "DATABASE".into(), names: vec![vec![db.clone()]], if_exists: false };
            merge::check(&self.mirror.execute_stmts(std::slice::from_ref(&one)))?;
            let shards = self.all_shards();
            self.fan_run(&shards, |_| Work::Stmts(vec![one.clone()])).await?;
            let prefix = format!("{}.", db.to_lowercase());
            self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| !k.starts_with(&prefix));
            self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| !k.starts_with(&prefix));
            self.cluster.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| !k.starts_with(&prefix));
            self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).retain(|f| !f.child.starts_with(&prefix) && !f.parent.starts_with(&prefix));
            self.cluster.log_schema_in("master", &one);
            self.plan.push(format!("broadcast DROP DATABASE to {} shards", shards.len()));
        }
        let _ = st;
        Ok(())
    }
}
