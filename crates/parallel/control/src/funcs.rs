//! Stored functions in a cluster.
//!
//! A function is made on the control node's engine (the *mirror*, which checks it and keeps it) and kept in a registry, by id, as its
//! `CREATE FUNCTION` statement. Where else it lives depends on what it reads:
//!
//! * a function that **runs on the shards** (it reads no table of the cluster, or only *replicated* ones, which every shard holds in full;
//!   no view of one; and it calls no function that does otherwise) is made on every shard too, as a procedure is, so a query that calls
//!   it still runs on the shards;
//! * a function that **reads sharded tables** (or a view) lives on the control node only: the shards hold a slice of each table, and a
//!   function run on one would see only its slice. A query that calls one (directly, through a view, or through another function) is *gathered*: every
//!   table it reaches is copied into the mirror and the whole query runs there, which is exact and runs at single-node speed (see
//!   `gather.rs`).
//!
//! The registry is not part of the saved state: a control node that starts again learns it from the schema log (the statements it
//! replays on the mirror), so no file format changes. A new shard is given the pure functions, not the others.
//!
//! The calls in a statement are given the database of the function (`db.schema.f(...)`, which the engine runs in that database), since a
//! shard's connection is in whatever database it was last sent to.
use crate::*;
use serde_json::Value as Json;
use std::collections::{BTreeSet, HashSet};

/// The database an id (`db.schema.name`; two parts are `master`'s) is in.
pub(crate) fn id_db(id: &str) -> String {
    let p: Vec<&str> = id.split('.').collect();
    if p.len() == 3 {
        p[0].to_string()
    } else {
        "master".to_string()
    }
}

/// The id of a name written in a statement of database `db`.
pub(crate) fn rel_id(db: &str, parts: &[String]) -> String {
    let id = table_id(parts);
    if db.eq_ignore_ascii_case("master") || parts.len() >= 3 {
        id
    } else {
        format!("{}.{}", db.to_lowercase(), id)
    }
}

fn strs(v: &[Json]) -> Vec<String> {
    v.iter().filter_map(|p| p.as_str().map(|s| s.to_string())).collect()
}

#[derive(Default)]
struct Refs {
    tables: Vec<String>,
    funcs: Vec<String>,
}

/// The tables (and views) and the functions a serialized statement names. A table reference is `{"Table": {"name": [...]}}`, a call of a
/// function in an expression `{"Func": {"name": "F", "parts": [...]}}`, and one in a `FROM` `{"Func": {"name": [...], "args": ...}}`.
fn collect(v: &Json, db: &str, out: &mut Refs) {
    match v {
        Json::Object(m) => {
            if m.len() == 1 {
                if let Some(Json::Object(t)) = m.get("Table") {
                    if let Some(Json::Array(parts)) = t.get("name") {
                        out.tables.push(rel_id(db, &strs(parts)));
                    }
                }
                if let Some(Json::Object(f)) = m.get("Func") {
                    match f.get("name") {
                        Some(Json::Array(parts)) => out.funcs.push(rel_id(db, &strs(parts))),
                        Some(Json::String(_)) => {
                            if let Some(Json::Array(parts)) = f.get("parts") {
                                let p = strs(parts);
                                if !p.is_empty() {
                                    out.funcs.push(rel_id(db, &p));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            for c in m.values() {
                collect(c, db, out);
            }
        }
        Json::Array(a) => {
            for c in a {
                collect(c, db, out);
            }
        }
        _ => {}
    }
}

/// The ids of the tables (or views) and of the functions a serializable thing (the definition of a function in a backup) names, in
/// the form `schema.name`.
pub(crate) fn references<T: Serialize>(x: &T) -> (Vec<String>, Vec<String>) {
    let mut refs = Refs::default();
    if let Ok(j) = serde_json::to_value(x) {
        collect(&j, "master", &mut refs);
    }
    (refs.tables, refs.funcs)
}

#[derive(Default)]
struct Reach {
    /// every distributed table reached, through views and functions
    all: BTreeSet<String>,
    /// those reached through a function
    via: BTreeSet<String>,
    /// a view was read through a function (the shards hold no views)
    via_view: bool,
    /// a function of the cluster is called
    called: bool,
    seen_views: HashSet<String>,
    seen_funcs: HashSet<String>,
}

/// The registry as it is after a schema log: the last `CREATE FUNCTION` of each id that was not dropped since.
pub(crate) fn functions_of_log(log: &[Stmt]) -> HashMap<String, Stmt> {
    let mut out = HashMap::new();
    for st in log {
        match st {
            Stmt::CreateFunction(cf) => {
                out.insert(table_id(&cf.name), st.clone());
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("FUNCTION") => {
                for n in names {
                    out.remove(&table_id(n));
                }
            }
            // (a dropped database takes its functions with it)
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("DATABASE") => {
                for n in names {
                    if let Some(db) = n.last() {
                        let prefix = format!("{}.", db.to_lowercase());
                        out.retain(|k, _| !k.starts_with(&prefix));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The procedures a schema log makes (the last `CREATE PROCEDURE` of each id not dropped since), by id.
fn procedures_of_log(log: &[Stmt]) -> HashMap<String, Stmt> {
    let mut out = HashMap::new();
    for st in log {
        match st {
            Stmt::CreateProc { name, .. } => {
                out.insert(table_id(name), st.clone());
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("PROCEDURE") || what.eq_ignore_ascii_case("PROC") => {
                for n in names {
                    out.remove(&table_id(n));
                }
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("DATABASE") => {
                for n in names {
                    if let Some(db) = n.last() {
                        let prefix = format!("{}.", db.to_lowercase());
                        out.retain(|k, _| !k.starts_with(&prefix));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

impl Cluster {
    /// A procedure (which every shard has, and runs) that calls, directly or through other functions, a function that is on the shards
    /// now but would read a sharded table as the tables are: the procedure and the function, as ids. The shards would run the function
    /// over their slices only.
    pub(crate) fn stale_procedure(&self) -> Option<(String, String)> {
        self.procedure_calling_reader(None, true)
    }

    /// A routed procedure (or just `only`, which is about to be one) that calls a function that is not on the shards: the procedure and the
    /// function. With `placed_only`, only a function that is on the shards now counts (what a change would strand).
    pub(crate) fn procedure_calling_reader(&self, only: Option<&str>, placed_only: bool) -> Option<(String, String)> {
        let procs = {
            let log = self.schema_log.lock().unwrap_or_else(|e| e.into_inner());
            procedures_of_log(&log)
        };
        let placed = self.funcs_on_shards.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let routed: HashSet<String> = self.proc_routes.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
        let mut ids: Vec<&String> = procs.keys().filter(|p| only.map_or(routed.contains(*p), |o| *p == o)).collect();
        ids.sort();
        for pid in ids {
            let mut seen: HashSet<String> = HashSet::new();
            let mut todo: Vec<(String, Json)> = vec![(pid.clone(), serde_json::to_value(&procs[pid]).unwrap_or(Json::Null))];
            while let Some((owner, j)) = todo.pop() {
                let mut refs = Refs::default();
                collect(&j, &id_db(&owner), &mut refs);
                for f in refs.funcs {
                    if !seen.insert(f.clone()) {
                        continue;
                    }
                    let st = self.funcs.lock().unwrap_or_else(|e| e.into_inner()).get(&f).cloned();
                    let Some(st) = st else { continue };
                    if (!placed_only || placed.contains(&f)) && !self.function_on_shards(&f, &st) {
                        return Some((pid.clone(), f));
                    }
                    todo.push((f, serde_json::to_value(&st).unwrap_or(Json::Null)));
                }
            }
        }
        None
    }

    pub(crate) fn has_functions(&self) -> bool {
        !self.funcs.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// The distributed tables a statement reaches (through views and functions) and those it reaches through functions, as ids.
    /// `db`: the database the statement is in; `in_fn`: it is the body of a function.
    /// A function that runs on the shards (see the top of this file) does not count as reading: what it reads is in `all` (a gathered
    /// query needs it on the control node too) but not in the second set.
    pub(crate) fn reach<T: Serialize>(&self, x: &T, db: &str, in_fn: bool) -> (BTreeSet<String>, BTreeSet<String>) {
        let st = self.reach_full(x, db, in_fn, true);
        (st.all, st.via)
    }

    /// `shortcut`: a call of a function that runs on the shards is not followed as a reading one (false: every function is followed as
    /// one, which is how it is found out whether a function runs on the shards).
    fn reach_full<T: Serialize>(&self, x: &T, db: &str, in_fn: bool, shortcut: bool) -> Reach {
        let mut st = Reach::default();
        if let Ok(j) = serde_json::to_value(x) {
            self.reach_json(&j, db, in_fn, shortcut, &mut st);
        }
        st
    }

    fn reach_json(&self, v: &Json, db: &str, in_fn: bool, shortcut: bool, st: &mut Reach) {
        let mut refs = Refs::default();
        collect(v, db, &mut refs);
        for id in refs.tables {
            if self.tables.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
                st.all.insert(id.clone());
                if in_fn {
                    st.via.insert(id);
                }
                continue;
            }
            let view = self.views.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
            if let Some(view) = view {
                if st.seen_views.insert(id.clone()) {
                    if in_fn {
                        st.via_view = true;
                    }
                    if let Ok(j) = serde_json::to_value(&view) {
                        self.reach_json(&j, &id_db(&id), in_fn, shortcut, st);
                    }
                }
            }
        }
        for id in refs.funcs {
            let f = self.funcs.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
            if let Some(f) = f {
                st.called = true;
                if st.seen_funcs.insert(id.clone()) {
                    // (a function that runs on the shards reads what every shard holds: what it reads is not read through a function that
                    // reads tables, though a gathered query needs the tables on the control node as well)
                    let reads = !(shortcut && self.function_on_shards(&id, &f));
                    if let Ok(j) = serde_json::to_value(&f) {
                        self.reach_json(&j, &id_db(&id), in_fn || reads, shortcut, st);
                    }
                }
            }
        }
    }

    /// Whether a statement calls a function of the cluster that reads a table (replicated or not). The control node's own copy of a
    /// table holds no rows, so such a statement must not be answered by it, whatever the statement names: it runs where the rows are.
    pub(crate) fn calls_function_reading_tables<T: Serialize>(&self, x: &T, db: &str) -> bool {
        if !self.has_functions() {
            return false;
        }
        let r = self.reach_full(x, db, false, true);
        r.called && !r.all.is_empty()
    }

    /// Whether the function can live on the shards: everything it reads (through the functions it calls too) is a replicated table or
    /// no table at all, and it reads no view.
    pub(crate) fn function_on_shards(&self, id: &str, st: &Stmt) -> bool {
        let r = self.reach_full(st, &id_db(id), true, false);
        if r.via_view {
            return false;
        }
        let tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        r.via.iter().all(|t| tables.get(t).is_some_and(|d| matches!(d.dist, Dist::Replicated)))
    }

    /// The functions that are on the shards.
    pub(crate) fn pure_functions(&self) -> Vec<(String, Stmt)> {
        let all: Vec<(String, Stmt)> = self.funcs.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut out: Vec<(String, Stmt)> = all.into_iter().filter(|(id, st)| self.function_on_shards(id, st)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The display form of a database id (the case it was made in).
    fn db_shown(&self, db: &str) -> String {
        if db.eq_ignore_ascii_case("master") {
            return "master".into();
        }
        self.dbs.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|d| d.eq_ignore_ascii_case(db)).cloned().unwrap_or_else(|| db.to_string())
    }

    /// The work that makes (or drops) a function in its own database on a shard, leaving the connection where the session is.
    pub(crate) fn function_work(&self, id: &str, st: Stmt, back_to: &str) -> Work {
        if self.dbs.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return Work::Stmts(vec![st]);
        }
        Work::Stmts(vec![Stmt::Use(self.db_shown(&id_db(id))), st, Stmt::Use(back_to.to_string())])
    }
}

/// `CREATE OR ALTER`, so that a function that is on a shard already (a changed one) is replaced.
pub(crate) fn or_alter_pub(st: &Stmt) -> Stmt {
    or_alter(st)
}

fn or_alter(st: &Stmt) -> Stmt {
    match st {
        Stmt::CreateFunction(cf) => {
            let mut c = (**cf).clone();
            c.or_alter = true;
            Stmt::CreateFunction(Box::new(c))
        }
        other => other.clone(),
    }
}

impl ClusterSession {
    /// The distributed tables a statement reaches when it calls (directly, through a view, or through a function) a function that reads
    /// tables; None when it calls none. Such a statement is gathered into the control node.
    pub(crate) fn reading_function_tables<T: Serialize>(&self, x: &T) -> Option<Vec<DistTable>> {
        if !self.cluster.has_functions() {
            return None;
        }
        let (all, via) = self.cluster.reach(x, &self.cur_db, false);
        if via.is_empty() {
            return None;
        }
        let mut tables: Vec<DistTable> = all.iter().filter_map(|i| self.table_by_id(i)).collect();
        // (the temporary tables of the session that the statement names are read with it: `inserted` and `deleted` of a trigger are such)
        if !self.local_tables.is_empty() {
            let text = serde_json::to_string(x).unwrap_or_default();
            for t in self.local_tables.values() {
                if text.contains(&format!("\"{}\"", t.name.last().cloned().unwrap_or_default())) && !tables.iter().any(|o| o.id == t.id) {
                    tables.push(t.clone());
                }
            }
        }
        Some(tables)
    }

    /// See [`Cluster::calls_function_reading_tables`].
    pub(crate) fn calls_function_reading_tables<T: Serialize>(&self, x: &T) -> bool {
        self.cluster.calls_function_reading_tables(x, &self.cur_db)
    }

    /// `st` with the calls of the functions of the cluster given their database (`db.schema.f(...)`), so that a shard finds them
    /// whichever database its connection is in.
    pub(crate) fn qualify_functions(&self, st: &Stmt) -> Stmt {
        if !self.cluster.has_functions() {
            return st.clone();
        }
        let Ok(mut v) = serde_json::to_value(st) else { return st.clone() };
        self.qualify_calls(&mut v);
        serde_json::from_value(v).unwrap_or_else(|_| st.clone())
    }

    fn qualify_calls(&self, v: &mut Json) {
        match v {
            Json::Object(m) => {
                if m.len() == 1 {
                    if let Some(Json::Object(f)) = m.get_mut("Func") {
                        // a call in an expression
                        if let (Some(Json::String(_)), Some(Json::Array(parts))) = (f.get("name"), f.get("parts")) {
                            let p = strs(parts);
                            if !p.is_empty() && p.len() < 3 {
                                let id = rel_id(&self.cur_db, &p);
                                if self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
                                    let schema = if p.len() == 2 { p[0].clone() } else { "dbo".to_string() };
                                    let full = vec![self.cluster.db_shown(&id_db(&id)), schema.clone(), p.last().cloned().unwrap_or_default()];
                                    f.insert("parts".into(), Json::Array(full.into_iter().map(Json::String).collect()));
                                    f.insert("schema".into(), Json::String(schema));
                                }
                            }
                        } else if let Some(Json::Array(parts)) = f.get("name") {
                            // one in a FROM
                            let p = strs(parts);
                            if !p.is_empty() && p.len() < 3 {
                                let id = rel_id(&self.cur_db, &p);
                                if self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
                                    let schema = if p.len() == 2 { p[0].clone() } else { "dbo".to_string() };
                                    let full = vec![self.cluster.db_shown(&id_db(&id)), schema, p.last().cloned().unwrap_or_default()];
                                    f.insert("name".into(), Json::Array(full.into_iter().map(Json::String).collect()));
                                }
                            }
                        }
                    }
                }
                for c in m.values_mut() {
                    self.qualify_calls(c);
                }
            }
            Json::Array(a) => {
                for c in a.iter_mut() {
                    self.qualify_calls(c);
                }
            }
            _ => {}
        }
    }

    /// `CREATE [OR ALTER] FUNCTION`: checked and kept by the mirror, and made on every shard when it reads no table.
    pub(crate) async fn create_function(&mut self, st: &Stmt, cf: &CreateFunction) -> Result<(), SqlError> {
        let id = table_id(&cf.name);
        self.sync_mirror_db()?;
        merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
        let before = self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), st.clone());
        // (a procedure runs on the shards, so it can call only the functions that are there: a change that would take this one from them
        // under a procedure is refused, and the function stays as it was)
        if let Some((p, f)) = self.cluster.stale_procedure() {
            match &before {
                Some(old) => {
                    self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), old.clone());
                    let _ = self.mirror.execute_stmts(&[or_alter(old)]);
                }
                None => {
                    self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                    let drop = Stmt::Drop { what: "FUNCTION".into(), names: vec![cf.name.clone()], if_exists: true };
                    let _ = self.mirror.execute_stmts(&[drop]);
                }
            }
            return Err(SqlError::new(50105, format!("The function {} would read sharded tables, and the procedure {}, which runs on the shards, calls it ({}): each shard would answer from its slice. Drop or change the procedure first.", id, p, f)));
        }
        if let Err(e) = self.place_functions().await {
            // (a function that cannot be made everywhere is not made at all)
            match before {
                Some(old) => {
                    self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).insert(id, old);
                }
                None => {
                    self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                    let drop = Stmt::Drop { what: "FUNCTION".into(), names: vec![cf.name.clone()], if_exists: true };
                    let _ = self.mirror.execute_stmts(&[drop]);
                }
            }
            return Err(e);
        }
        let pure = self.cluster.function_on_shards(&id, st);
        self.plan.push(if pure { "function: made on the control node and on every shard (it reads no table, or only replicated ones)".to_string() } else { "function: made on the control node only (it reads sharded tables, so a query that calls it runs there)".to_string() });
        self.log_schema(st);
        Ok(())
    }

    /// `DROP FUNCTION`.
    pub(crate) async fn drop_functions(&mut self, st: &Stmt, names: &[Name], if_exists: bool) -> Result<(), SqlError> {
        let mut known: Vec<String> = vec![];
        for n in names {
            let id = table_id(n);
            if self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
                known.push(id);
            } else if !if_exists {
                return Err(SqlError::new(3701, format!("Cannot drop the function '{}', because it does not exist or you do not have permission.", n.join("."))));
            }
        }
        self.sync_mirror_db()?;
        merge::check(&self.mirror.execute_stmts(std::slice::from_ref(st)))?;
        for id in &known {
            self.cluster.funcs.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        }
        self.place_functions().await?;
        self.log_schema(st);
        Ok(())
    }

    /// Make the shards hold the pure functions and no other (after a function was made, changed or dropped: one that reads a table now,
    /// or calls one that does, is taken from the shards; one that does not any more is put on them).
    pub(crate) async fn place_functions(&mut self) -> Result<(), SqlError> {
        let want: Vec<(String, Stmt)> = self.cluster.pure_functions();
        let want_ids: BTreeSet<String> = want.iter().map(|(i, _)| i.clone()).collect();
        let have: BTreeSet<String> = self.cluster.funcs_on_shards.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let shards = self.all_shards();
        let back = self.cur_db.clone();
        for id in have.difference(&want_ids) {
            let parts: Name = id.split('.').rev().take(2).map(|s| s.to_string()).collect::<Vec<_>>().into_iter().rev().collect();
            let drop = Stmt::Drop { what: "FUNCTION".into(), names: vec![parts], if_exists: true };
            let work = self.cluster.function_work(id, drop, &back);
            self.fan_run(&shards, |_| work.clone()).await?;
            self.cluster.funcs_on_shards.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        }
        for (id, st) in &want {
            if have.contains(id) && self.cluster.funcs_on_shards.lock().unwrap_or_else(|e| e.into_inner()).contains(id) && !self.changed_since_placed(id, st) {
                continue;
            }
            let work = self.cluster.function_work(id, or_alter(st), &back);
            self.fan_run(&shards, |_| work.clone()).await?;
            self.cluster.funcs_on_shards.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone());
            self.cluster.funcs_placed.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), serde_json::to_string(st).unwrap_or_default());
        }
        Ok(())
    }

    /// Whether the statement of a function on the shards is not the one the registry has now (an `ALTER`).
    fn changed_since_placed(&self, id: &str, st: &Stmt) -> bool {
        let now = serde_json::to_string(st).unwrap_or_default();
        self.cluster.funcs_placed.lock().unwrap_or_else(|e| e.into_inner()).get(id).map(|p| *p != now).unwrap_or(true)
    }

    /// After a table is made, dropped or distributed: a function that read a replicated table may read a sharded one now (and the other
    /// way), so the shards are given the functions that run there and not the others.
    pub(crate) async fn replace_functions_after_table_change(&mut self) -> Result<(), SqlError> {
        if self.cluster.has_functions() {
            self.place_functions().await?;
        }
        Ok(())
    }

    /// The function of a restore or a statement refused when it would run where its tables are not.
    pub(crate) fn refuse_reading_functions<T: Serialize>(&self, x: &T, what: &str) -> Result<(), SqlError> {
        if self.reading_function_tables(x).is_some() {
            return Err(unsupported(&format!(
                "a function that reads tables in {} (the shards run it, and each holds only part of the tables; compute the value in a SELECT instead, which the control node runs)",
                what
            )));
        }
        Ok(())
    }
}
