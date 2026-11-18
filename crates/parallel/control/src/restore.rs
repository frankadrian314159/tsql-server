//! `RESTORE DATABASE` in a cluster: the backup's tables are made through the cluster (so they are placed on the shards like
//! any table made with `CREATE TABLE`) and their rows are sent to the shards that own them as the backup is read, a batch at a
//! time, so the control node never holds the database. Its own engine (the mirror) gets the schema only, as for every
//! distributed table.
//!
//! The order: check the backup (every checksum) and count each table's rows; refuse what a cluster cannot hold (unless
//! `WITH CONTINUE_AFTER_ERROR`, which restores the rest and says what was left out); make the schemas, then the tables with
//! their keys, defaults and check constraints, then the foreign keys (while the tables are empty, so the order of the tables
//! does not matter); send the rows; build the other indexes; set the identity counters; make the views and procedures in the
//! order they were made in the source database. A failure drops everything the restore made.
//!
//! How each table is spread: `WITH DISTRIBUTION = N'dbo.orders BY customer_id; dbo.region REPLICATED'` says it; a table it
//! does not name is replicated when it has at most `REPLICATE_UPTO` rows and otherwise sharded on the first column of its
//! primary key, as `CREATE TABLE` does.
//!
//! The objects go into the cluster's one database, whatever the statement names (a cluster has no `CREATE DATABASE`);
//! `WITH REPLACE` first drops the cluster's objects that have the names of restored ones. Only a full backup can be restored,
//! `WITH RECOVERY` (`NORECOVERY`, `STANDBY`, `STOPAT` and log or differential backups are refused).
use crate::*;
use tsql_server::catalog::{Object, Table};
use tsql_server::engine::backup::BackupSource;
use tsql_server::script;

/// Rows of a table sent to a shard in one message.
const LOAD_BATCH: usize = 5_000;
/// The largest table (in rows) restored as a replicated table when the `DISTRIBUTION` option does not name it.
pub const REPLICATE_UPTO: u64 = 10_000;

/// How a table that is being made is spread, when it is not the default of `CREATE TABLE` (see `create_table`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DistChoice {
    By(String),
    Replicated,
}

/// A restored table, as its rows are sent.
struct Target {
    id: String,
    /// `schema.name`, as the shards' `load` finds it
    shard_name: String,
    /// the positions of the backup's row that the shards are given (not computed, not `rowversion`)
    keep: Vec<usize>,
    /// the shard key: its position in the kept values, and its class; None: replicated
    key: Option<(usize, KeyClass)>,
    key_name: String,
    rows: u64,
}

/// Something the restore made, to drop again if it fails.
enum Made {
    Table(Name),
    View(Name),
    Proc(Name),
}

impl ClusterSession {
    pub(crate) async fn restore_database(&mut self, r: &RestoreStmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        tsql_server::span!("ctl_restore");
        if self.txn.borrow().is_some() {
            return Err(SqlError::with(574, 16, 1, "RESTORE DATABASE cannot be used inside a user transaction."));
        }
        let mut replace = false;
        let mut keep_going = false;
        let mut spec_text: Option<String> = None;
        for (name, val) in &r.opts {
            match name.as_str() {
                "REPLACE" => replace = true,
                "CONTINUE_AFTER_ERROR" => keep_going = true,
                "DISTRIBUTION" => {
                    let e = val.as_ref().ok_or_else(|| SqlError::new(102, "Incorrect syntax near 'DISTRIBUTION': it needs a value (DISTRIBUTION = N'table BY column; table REPLICATED')."))?;
                    let v = self.eval_scalar(e).await?;
                    spec_text = Some(tsql_server::convert::display(&v));
                }
                // (what changes nothing here)
                "RECOVERY" | "STATS" | "FILE" | "MOVE" | "CHECKSUM" | "NO_CHECKSUM" | "STOP_ON_ERROR" | "UNLOAD" | "NOUNLOAD" | "REWIND" | "NOREWIND" | "BUFFERCOUNT" | "MAXTRANSFERSIZE" | "BLOCKSIZE" | "MEDIANAME" | "KEEP_REPLICATION" | "KEEP_CDC" | "ENABLE_BROKER" | "NEW_BROKER" | "ERROR_BROKER_CONVERSATIONS" | "RESTRICTED_USER" => {}
                other => return Err(unsupported(&format!("RESTORE ... WITH {} in a cluster (a cluster restores a full backup WITH RECOVERY)", other))),
            }
        }
        let pv = self.eval_scalar(&r.path).await?;
        let path = tsql_server::convert::display(&pv);
        if pv.is_null() || path.trim().is_empty() {
            return Err(SqlError::with(3201, 16, 1, "Cannot open backup device ''. The device name must not be empty or NULL."));
        }
        let started = std::time::Instant::now();
        let src = BackupSource::open(&path)?;
        for n in &src.notes {
            events.push(Event::Message(n.clone()));
        }

        // ---- what the backup holds, and what a cluster cannot take
        let cat = &src.catalog;
        let mut objects: Vec<&Object> = cat.objects.values().collect();
        objects.sort_by_key(|o| object_id(o));
        let mut tables: Vec<&Table> = vec![];
        let mut modules: Vec<(&'static str, Name, String)> = vec![];
        let mut refused: Vec<String> = vec![];
        for o in &objects {
            match o {
                Object::Table(t) => {
                    let shown = format!("{}.{}", t.schema, t.name);
                    if let Some(why) = script::not_scriptable(t) {
                        refused.push(format!("the table {} ({})", shown, why));
                        continue;
                    }
                    if t.schema.contains('.') || t.name.contains('.') {
                        refused.push(format!("the table {} (a name with a dot in it)", shown));
                        continue;
                    }
                    for g in &t.triggers {
                        refused.push(format!("the trigger {} of {} (a cluster has no triggers)", g.name, shown));
                    }
                    tables.push(t);
                }
                Object::View(v) => {
                    for g in &v.triggers {
                        refused.push(format!("the trigger {} of the view {}.{} (a cluster has no triggers)", g.name, v.schema, v.name));
                    }
                    modules.push(("VIEW", vec![v.schema.clone(), v.name.clone()], script::module(&v.text)));
                }
                Object::Proc(p) => modules.push(("PROCEDURE", vec![p.schema.clone(), p.name.clone()], script::module(&p.text))),
                Object::Func(f) => refused.push(format!("the function {}.{} (a cluster has no stored functions)", f.schema, f.name)),
                Object::Seq(s) => refused.push(format!("the sequence {}.{}", s.schema, s.name)),
                Object::Synonym(s) => refused.push(format!("the synonym {}.{}", s.schema, s.name)),
            }
        }
        if !cat.policies.is_empty() {
            refused.push(format!("{} security polic(ies) (row-level security)", cat.policies.len()));
        }
        if !cat.ddl_triggers.is_empty() {
            refused.push(format!("{} DDL trigger(s)", cat.ddl_triggers.len()));
        }
        if !refused.is_empty() {
            if !keep_going {
                return Err(unsupported(&format!("restoring {} into a cluster (RESTORE ... WITH CONTINUE_AFTER_ERROR restores the rest without them)", refused.join("; "))));
            }
            for x in &refused {
                events.push(Event::Message(format!("Not restored: {}.", x)));
            }
        }
        if !cat.types.is_empty() {
            events.push(Event::Message(format!("Not restored: {} user-defined type(s); the columns of the restored tables have their base types.", cat.types.len())));
        }
        let defaults = tsql_server::security::DbSecurity::default();
        let principals = cat.security.principals.keys().filter(|k| !defaults.principals.contains_key(*k)).count();
        if principals > 0 || !cat.security.perms.is_empty() {
            events.push(Event::Message("Not restored: the database's users, roles and permissions (the cluster's database keeps its own).".into()));
        }

        // ---- how each table is spread
        let counts = src.row_counts()?;
        let mut choices: HashMap<String, DistChoice> = match &spec_text {
            Some(s) => parse_distribution(s)?,
            None => HashMap::new(),
        };
        for id in choices.keys() {
            if !tables.iter().any(|t| table_id(&[t.schema.clone(), t.name.clone()]) == *id) {
                return Err(SqlError::new(208, format!("DISTRIBUTION names {}, which is not a table of the backup that can be restored.", id)));
            }
        }
        let mut plan: Vec<(&Table, DistChoice)> = vec![];
        for &t in &tables {
            let id = table_id(&[t.schema.clone(), t.name.clone()]);
            let rows = counts.get(&id).copied().unwrap_or(0);
            let choice = match choices.remove(&id) {
                Some(c) => c,
                None if rows <= REPLICATE_UPTO => DistChoice::Replicated,
                None => DistChoice::By(default_key(t)?),
            };
            if let DistChoice::By(k) = &choice {
                check_key(t, k)?;
            }
            plan.push((t, choice));
        }

        // ---- names already taken
        let existing: Vec<(String, String)> = self.mirror.objects().into_iter().map(|o| (table_id(&[o.schema.clone(), o.name.clone()]), format!("{:?}", o.kind))).collect();
        let mut clashes: Vec<(String, String)> = vec![];
        let mut names: Vec<String> = tables.iter().map(|t| table_id(&[t.schema.clone(), t.name.clone()])).collect();
        names.extend(modules.iter().map(|(_, n, _)| table_id(n)));
        for n in &names {
            if let Some((id, kind)) = existing.iter().find(|(id, _)| id == n) {
                clashes.push((id.clone(), kind.clone()));
            }
        }
        if !clashes.is_empty() {
            if !replace {
                let list: Vec<String> = clashes.iter().map(|(id, _)| id.clone()).collect();
                return Err(SqlError::with(3159, 16, 1, format!("The cluster's database already has {}. Use RESTORE ... WITH REPLACE to drop them and restore the backup's.", list.join(", "))));
            }
            // (views and procedures first: they may depend on the tables; then the tables, the last made first, with the foreign
            // keys between them forgotten so that the order does not matter)
            clashes.sort_by_key(|(_, kind)| kind == "Table");
            let ids: Vec<String> = clashes.iter().filter(|(_, k)| k == "Table").map(|(id, _)| id.clone()).collect();
            self.cluster.fks.lock().unwrap_or_else(|x| x.into_inner()).retain(|f| !ids.contains(&f.child));
            let tables_from = clashes.iter().position(|(_, k)| k == "Table").unwrap_or(clashes.len());
            clashes[tables_from..].reverse();
            for (id, kind) in &clashes {
                let (schema, name) = id.split_once('.').unwrap_or(("dbo", id.as_str()));
                let what = match kind.as_str() {
                    "Table" => "TABLE",
                    "View" => "VIEW",
                    "Procedure" => "PROCEDURE",
                    other => return Err(unsupported(&format!("replacing the {} {} by a restored object of the same name", other.to_lowercase(), id))),
                };
                let drop = Stmt::Drop { what: what.into(), names: vec![vec![schema.to_string(), name.to_string()]], if_exists: true };
                let mut none = vec![];
                Box::pin(self.exec_stmt(&drop, &mut none)).await?;
            }
        }

        // ---- make it, send the rows; undo it all on a failure
        let mut made: Vec<Made> = vec![];
        let out = self.restore_into(&src, &plan, &modules, keep_going, &mut made, events).await;
        let (nrows, sharded, replicated) = match out {
            Ok(x) => x,
            Err(e) => {
                // (the foreign keys between the restored tables go first, so that the tables can be dropped in any order)
                let ids: Vec<String> = made.iter().filter_map(|m| if let Made::Table(n) = m { Some(table_id(n)) } else { None }).collect();
                self.cluster.fks.lock().unwrap_or_else(|x| x.into_inner()).retain(|f| !ids.contains(&f.child));
                for m in made.iter().rev() {
                    let (what, name) = match m {
                        Made::Table(n) => ("TABLE", n),
                        Made::View(n) => ("VIEW", n),
                        Made::Proc(n) => ("PROCEDURE", n),
                    };
                    let drop = Stmt::Drop { what: what.into(), names: vec![name.clone()], if_exists: true };
                    let mut none = vec![];
                    let _ = Box::pin(self.exec_stmt(&drop, &mut none)).await;
                }
                return Err(e);
            }
        };
        events.push(Event::Message(format!(
            "Processed {} row(s) in {} table(s) for database '{}', file '{}': {} sharded and {} replicated over {} shard(s).",
            nrows,
            plan.len(),
            src.database,
            path,
            sharded,
            replicated,
            self.cluster.shard_count()
        )));
        events.push(Event::Message(format!("RESTORE DATABASE successfully processed {} rows in {:.3} seconds.", nrows, started.elapsed().as_secs_f64().max(0.001))));
        self.plan.push(format!("restore {} table(s), {} row(s), sent to the shards as read", plan.len(), nrows));
        Ok(())
    }

    /// The part of the restore that changes the cluster: returns (rows, sharded tables, replicated tables).
    #[allow(clippy::too_many_arguments)]
    async fn restore_into(&mut self, src: &BackupSource, plan: &[(&Table, DistChoice)], modules: &[(&'static str, Name, String)], keep_going: bool, made: &mut Vec<Made>, events: &mut Vec<Event>) -> Result<(u64, usize, usize), SqlError> {
        // the schemas the objects are in (the shards need them too)
        let mut schemas: Vec<String> = plan.iter().map(|(t, _)| t.schema.clone()).chain(modules.iter().filter_map(|(_, n, _)| n.first().cloned())).filter(|s| !s.eq_ignore_ascii_case("dbo")).collect();
        schemas.sort_by_key(|s| s.to_lowercase());
        schemas.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        for s in schemas {
            let st = Stmt::CreateSchema(s.clone(), None);
            // (made on the mirror first: if it is there already, the shards have it too)
            if merge::check(&self.mirror.execute_stmts(std::slice::from_ref(&st))).is_ok() {
                let shards = self.all_shards();
                let clients: Vec<ShardClient> = shards.iter().map(|&i| self.cluster.shard(i)).collect();
                let futs: Vec<_> = clients.iter().map(|c| c.run(self.id, Work::Stmts(vec![st.clone()]))).collect();
                for r in join_all(futs).await {
                    if let Err(e) = r {
                        if e.number != 2714 {
                            return Err(e);
                        }
                    }
                }
            }
        }
        // the tables, with what must be there before the rows
        let mut targets: HashMap<String, Target> = HashMap::new();
        let mut scripts = vec![];
        let (mut sharded, mut replicated) = (0usize, 0usize);
        for (t, choice) in plan {
            let s = script::table(t);
            let name: Name = vec![t.schema.clone(), t.name.clone()];
            let id = table_id(&name);
            self.create_dist = Some(choice.clone());
            let r = self.run_sql(&s.create).await;
            self.create_dist = None;
            r?;
            made.push(Made::Table(name.clone()));
            for x in &s.after_create {
                self.run_sql(x).await?;
            }
            for x in &s.skipped {
                events.push(Event::Message(format!("Not restored: {}.", x)));
            }
            let keep: Vec<usize> = (0..t.columns.len()).filter(|&i| t.columns[i].computed.is_none() && !matches!(t.columns[i].ty, tsql_server::DataType::Timestamp)).collect();
            let (key, key_name) = match choice {
                DistChoice::Replicated => {
                    replicated += 1;
                    (None, String::new())
                }
                DistChoice::By(k) => {
                    sharded += 1;
                    let at = t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(k)).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}'.", k)))?;
                    let class = KeyClass::of_type_name(&script::type_name(&t.columns[at].ty)).ok_or_else(|| unsupported(&format!("the shard key {} of type {}", k, t.columns[at].ty)))?;
                    let pos = keep.iter().position(|&i| i == at).ok_or_else(|| unsupported(&format!("the computed shard key {}", k)))?;
                    (Some((pos, class)), t.columns[at].name.clone())
                }
            };
            targets.insert(id.clone(), Target { id, shard_name: format!("{}.{}", t.schema, t.name), keep, key, key_name, rows: 0 });
            scripts.push((*t, s));
        }
        // the foreign keys, while the tables are empty (so the order of the tables does not matter)
        for (_, s) in &scripts {
            for x in &s.foreign_keys {
                self.run_sql(x).await?;
            }
        }
        // the rows, as they are read
        let mut total = 0u64;
        for b in src.batches()? {
            let (key, rows) = b?;
            let Some(target) = targets.get_mut(&key) else { continue };
            let n = rows.len() as u64;
            self.send_rows(target, rows).await?;
            target.rows += n;
            total += n;
        }
        // the other indexes, the counts and the identity counters
        for (t, s) in &scripts {
            for x in &s.indexes {
                self.run_sql(x).await?;
            }
            let id = table_id(&[t.schema.clone(), t.name.clone()]);
            let rows = targets.get(&id).map(|x| x.rows).unwrap_or(0);
            self.bump(&id);
            if let Some(dt) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                dt.rows = Some((dt.version, rows));
            }
            let inc = t.columns.iter().find_map(|c| c.identity.map(|(_, i)| i));
            if let (Some(next), Some(inc)) = (t.identity_next.get(), inc) {
                self.cluster.raise_identity(&id, next - inc);
            }
        }
        self.cluster.save_state();
        // the views and procedures, in the order they were made
        for (what, name, text) in modules {
            match self.run_sql(text).await {
                Ok(()) => made.push(if *what == "VIEW" { Made::View(name.clone()) } else { Made::Proc(name.clone()) }),
                Err(e) if keep_going => events.push(Event::Message(format!("Not restored: the {} {} ({}).", what.to_lowercase(), name.join("."), e.message))),
                Err(e) => return Err(e),
            }
        }
        Ok((total, sharded, replicated))
    }

    /// Send rows of a restored table to the shards that own them (a replicated table's to every shard), `LOAD_BATCH` to a message,
    /// every shard's part at once.
    async fn send_rows(&mut self, t: &Target, rows: Vec<Vec<Value>>) -> Result<(), SqlError> {
        let n = self.cluster.shard_count();
        let mut per_shard: Vec<Vec<Vec<Value>>> = vec![vec![]; n];
        let project = |r: Vec<Value>| -> Vec<Value> { t.keep.iter().map(|&i| r.get(i).cloned().unwrap_or(Value::Null)).collect() };
        match t.key {
            None => {
                let vals: Vec<Vec<Value>> = rows.into_iter().map(project).collect();
                for p in per_shard.iter_mut() {
                    *p = vals.clone();
                }
            }
            Some((k, class)) => {
                for r in rows {
                    let vals = project(r);
                    let h = key_hash(&vals[k], class).ok_or_else(|| SqlError::new(245, format!("The value {} of {} cannot be used as its shard key {}.", tsql_server::convert::display(&vals[k]), t.id, t.key_name)))?;
                    per_shard[self.cluster.shard_of_hash(h)].push(vals);
                }
            }
        }
        let clients: Vec<ShardClient> = (0..n).map(|i| self.cluster.shard(i)).collect();
        let session = self.id;
        let futs: Vec<_> = per_shard
            .into_iter()
            .zip(clients.iter())
            .filter(|(rows, _)| !rows.is_empty())
            .map(|(rows, c)| async move {
                for chunk in rows.chunks(LOAD_BATCH) {
                    c.load(session, &t.shard_name, chunk.to_vec()).await?;
                }
                Ok::<(), SqlError>(())
            })
            .collect();
        for r in join_all(futs).await {
            r?;
        }
        Ok(())
    }

    /// Run T-SQL text through the cluster, statement by statement.
    async fn run_sql(&mut self, sql: &str) -> Result<(), SqlError> {
        let stmts = tsql_server::parser::parse(sql)?;
        let mut none = vec![];
        for st in &stmts {
            Box::pin(self.exec_stmt(st, &mut none)).await?;
        }
        Ok(())
    }
}

fn object_id(o: &Object) -> i32 {
    match o {
        Object::Table(t) => t.object_id,
        Object::View(v) => v.object_id,
        Object::Proc(p) => p.object_id,
        Object::Func(f) => f.object_id,
        Object::Seq(s) => s.object_id,
        Object::Synonym(s) => s.object_id,
    }
}

/// The shard key `CREATE TABLE` would choose: the first column of the primary key.
fn default_key(t: &Table) -> Result<String, SqlError> {
    let shown = format!("{}.{}", t.schema, t.name);
    let first = t.pk.as_ref().and_then(|p| p.cols.first()).map(|&c| &t.columns[c]).or_else(|| t.columns.iter().find(|c| c.computed.is_none()));
    match first {
        Some(c) if KeyClass::of_type_name(&script::type_name(&c.ty)).is_some() => Ok(c.name.clone()),
        Some(c) => Err(unsupported(&format!("the shard key {} of {} (type {}); name another with RESTORE ... WITH DISTRIBUTION = N'{} BY column' or N'{} REPLICATED'", c.name, shown, c.ty, shown, shown))),
        None => Err(SqlError::new(1113, format!("The table {} has no column to shard it on.", shown))),
    }
}

/// A shard key must be a column of a type the cluster hashes, and in every unique key (else uniqueness is not checked across
/// shards).
fn check_key(t: &Table, key: &str) -> Result<(), SqlError> {
    let shown = format!("{}.{}", t.schema, t.name);
    let at = t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(key)).ok_or_else(|| SqlError::new(207, format!("Invalid column name '{}' (DISTRIBUTION of {}).", key, shown)))?;
    if KeyClass::of_type_name(&script::type_name(&t.columns[at].ty)).is_none() {
        return Err(unsupported(&format!("the shard key {} of {} (type {})", key, shown, t.columns[at].ty)));
    }
    let keys = t.pk.iter().map(|k| &k.cols).chain(t.uniques.iter().map(|u| &u.cols));
    for cols in keys {
        if !cols.contains(&at) {
            return Err(unsupported(&format!("the shard key {} of {}, which a primary key or unique constraint of the table does not contain", key, shown)));
        }
    }
    Ok(())
}

/// `DISTRIBUTION = N'dbo.orders BY customer_id; region REPLICATED'`: entries separated by `;` or `,`, names in brackets or not.
pub(crate) fn parse_distribution(s: &str) -> Result<HashMap<String, DistChoice>, SqlError> {
    let bad = |e: &str| SqlError::new(102, format!("Incorrect syntax in DISTRIBUTION near '{}'. Expected: table BY column | table REPLICATED, separated by ';'.", e.trim()));
    let clean = |w: &str| w.trim_matches(|c| c == '[' || c == ']' || c == '"').to_string();
    let mut out = HashMap::new();
    for entry in s.split(|c| c == ';' || c == ',').map(str::trim).filter(|e| !e.is_empty()) {
        let words: Vec<&str> = entry.split_whitespace().collect();
        let name: Name = words.first().ok_or_else(|| bad(entry))?.split('.').map(clean).collect();
        let choice = match words.as_slice() {
            [_, r] if r.eq_ignore_ascii_case("REPLICATED") => DistChoice::Replicated,
            [_, by, col] if by.eq_ignore_ascii_case("BY") => DistChoice::By(clean(col)),
            _ => return Err(bad(entry)),
        };
        out.insert(table_id(&name), choice);
    }
    Ok(out)
}
