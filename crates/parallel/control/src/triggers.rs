//! Triggers in a cluster.
//!
//! A table's triggers are the control node's: they are kept in a registry (the `CREATE TRIGGER` statements, rebuilt from the schema log
//! when a control node starts again) and are *not* made on the control node's engine or on the shards, which would fire them on the
//! wrong rows (a shard holds a slice of each table, and a trigger that reads or writes another table has to reach every shard). The
//! control node fires them itself, around the `INSERT`, `UPDATE` or `DELETE` it runs:
//!
//! * the statement is run inside a transaction (its own when the session has none), with `OUTPUT` added on the shards, so that the control
//!   node has the rows the statement inserted and deleted (`inserted` and `deleted`);
//! * an `AFTER` trigger then runs once for the statement, as T-SQL statements of the session: `inserted` and `deleted` are replicated
//!   temporary tables holding those rows, and everything the body reads or writes goes through the cluster, in the same transaction. An
//!   error, or a `ROLLBACK`, undoes the statement and what the trigger did;
//! * an `INSTEAD OF` trigger runs in place of the statement (`deleted` holds the rows the statement would delete, or change, and `inserted`
//!   the rows it would insert, or the changed rows).
//!
//! The same rules as SQL Server's: a trigger does not fire again for what it does to its own table (direct recursion is off), an
//! `INSTEAD OF` trigger's own statement on its table is done for real, triggers nest to 32 levels, `UPDATE(column)` says whether the
//! statement set the column. Not supported: triggers on views, DDL and logon triggers, `COLUMNS_UPDATED()`, and an `UPDATE` that changes the
//! shard key of a table that has triggers.
use crate::*;
use serde_json::Value as Json;
use std::collections::HashSet;
use std::sync::Arc;

const NEST_LIMIT: usize = 32;

/// The triggers a schema log makes, in the order they were made, and which of them are disabled.
pub(crate) fn triggers_of_log(log: &[Stmt]) -> (Vec<(String, Stmt)>, HashSet<String>) {
    let mut out: Vec<(String, Stmt)> = vec![];
    let mut off: HashSet<String> = HashSet::new();
    for st in log {
        match st {
            Stmt::CreateTrigger { name, .. } => {
                let id = table_id(name);
                match out.iter_mut().find(|(i, _)| *i == id) {
                    Some(slot) => slot.1 = st.clone(),
                    None => out.push((id, st.clone())),
                }
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("TRIGGER") => {
                for n in names {
                    let id = table_id(n);
                    out.retain(|(i, _)| *i != id);
                    off.remove(&id);
                }
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("TABLE") => {
                for n in names {
                    let tid = table_id(n);
                    out.retain(|(_, s)| !matches!(s, Stmt::CreateTrigger { table, .. } if table_id(table) == tid));
                }
            }
            // a table renamed (`sp_rename 'db.schema.table', 'new'`, logged with the table named in full) takes its triggers with it
            Stmt::Exec(ex) => {
                if let Some((obj, new, ty)) = ClusterSession::rename_args(ex) {
                    if ty.is_empty() || ty == "OBJECT" {
                        let old: Name = obj.split('.').map(|p| p.trim().trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                        let mut renamed = old.clone();
                        if let Some(l) = renamed.last_mut() {
                            *l = new.trim().trim_matches(|c| c == '[' || c == ']').to_string();
                        }
                        let old_id = table_id(&old);
                        for (_, s) in out.iter_mut() {
                            if let Stmt::CreateTrigger { table, .. } = s {
                                if table_id(table) == old_id {
                                    *table = renamed.clone();
                                }
                            }
                        }
                    }
                }
            }
            Stmt::Drop { what, names, .. } if what.eq_ignore_ascii_case("DATABASE") => {
                for n in names {
                    if let Some(db) = n.last() {
                        let prefix = format!("{}.", db.to_lowercase());
                        out.retain(|(i, _)| !i.starts_with(&prefix));
                    }
                }
            }
            Stmt::TriggerAdmin(a) => {
                if let (TriggerScope::Table(table), TriggerAct::Enable(on)) = (&a.scope, &a.act) {
                    let tid = table_id(table);
                    let ids: Vec<String> = if a.all {
                        out.iter().filter(|(i, s)| !crate::rules::is_rule_id(i) && matches!(s, Stmt::CreateTrigger { table, .. } if table_id(table) == tid)).map(|(i, _)| i.clone()).collect()
                    } else {
                        a.names.iter().map(|n| table_id(n)).collect()
                    };
                    for id in ids {
                        if *on {
                            off.remove(&id);
                        } else {
                            off.insert(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (out, off)
}

fn is_trigger_stmt(st: &Stmt) -> bool {
    matches!(st, Stmt::CreateTrigger { .. } | Stmt::TriggerAdmin(_)) || matches!(st, Stmt::Drop { what, .. } if what.eq_ignore_ascii_case("TRIGGER"))
}

/// Whether a statement of the schema log is about triggers (the control node's engine and a new shard are not given them).
pub(crate) fn is_trigger_log_stmt(st: &Stmt) -> bool {
    is_trigger_stmt(st)
}

/// What a trigger body uses that the control node cannot run (None: all of it can). The statements it runs are the ones it runs for a
/// client: data changes, queries, variables, `IF`, `WHILE`, `BEGIN ... END`, `PRINT`, `THROW`, `RAISERROR`, `RETURN` and `ROLLBACK`.
pub(crate) fn body_problem(body: &[Stmt]) -> Option<String> {
    fn check(st: &Stmt) -> Option<String> {
        match st {
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Query(_) | Stmt::Declare(_) | Stmt::Set { .. } | Stmt::SetOpts(_) | Stmt::Print(_) | Stmt::RaiseError { .. } | Stmt::Return(None) | Stmt::Rollback(None) | Stmt::Noop => None,
            Stmt::Throw(_) => None,
            Stmt::Try { body, catch } => body.iter().chain(catch.iter()).find_map(check),
            Stmt::Block(b) => b.iter().find_map(check),
            Stmt::If { then, els, .. } => check(then).or_else(|| els.as_ref().and_then(|e| check(e))),
            Stmt::While { body, .. } => check(body),
            Stmt::Try { .. } => Some("TRY ... CATCH".into()),
            Stmt::Exec(_) | Stmt::ExecStr(_) | Stmt::Break | Stmt::Continue => None,
            Stmt::DeclareCursor { .. } | Stmt::CursorOp { .. } | Stmt::Fetch { .. } => None,
            Stmt::Return(Some(_)) => Some("RETURN with a value".into()),
            Stmt::CreateTable { .. } | Stmt::Drop { .. } | Stmt::BeginTran(_) | Stmt::Commit(_) | Stmt::SaveTran(_) | Stmt::Goto(_) | Stmt::Label(_) => None,
            other => Some(stmt_kind(other)),
        }
    }
    body.iter().find_map(check).map(|w| format!("a trigger that uses {} (the control node runs data changes, queries, variables, cursors, IF, WHILE, PRINT, THROW, RAISERROR, RETURN and ROLLBACK)", w))
}

/// The rows of the statement being run, kept for its triggers.
pub(crate) struct Capture {
    pub table: String,
    pub event: &'static str,
    pub columns: Vec<String>,
    pub types: Vec<tsql_server::DataType>,
    pub ins: Vec<Vec<Value>>,
    pub del: Vec<Vec<Value>>,
    /// a shard answered (the columns are known)
    pub seen: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ev {
    Insert,
    Update,
    Delete,
}

impl Ev {
    fn name(self) -> &'static str {
        match self {
            Ev::Insert => "INSERT",
            Ev::Update => "UPDATE",
            Ev::Delete => "DELETE",
        }
    }
}

/// `st` with the `OUTPUT` that gives the rows it changed.
pub(crate) fn with_output(st: Stmt) -> Stmt {
    let star = |q: &str| SelectItem::Star(Some(vec![q.to_string()]));
    match st {
        Stmt::Insert(mut i) => {
            i.output = Some(Output { items: vec![star("inserted")], into: None, cols: None });
            Stmt::Insert(i)
        }
        Stmt::Update(mut u) => {
            u.output = Some(Output { items: vec![star("inserted"), star("deleted")], into: None, cols: None });
            Stmt::Update(u)
        }
        Stmt::Delete(mut d) => {
            d.output = Some(Output { items: vec![star("deleted")], into: None, cols: None });
            Stmt::Delete(d)
        }
        other => other,
    }
}

impl Cluster {
    pub(crate) fn has_triggers(&self, table: &str) -> bool {
        self.triggers.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(_, s)| matches!(s, Stmt::CreateTrigger { table: t, .. } if table_id(t) == table))
    }

    /// The triggers of a table for an event (`instead`: the INSTEAD OF ones), in the order they were made, that are not disabled: as
    /// (id, body).
    pub(crate) fn triggers_on(&self, table: &str, event: &str, instead: bool) -> Vec<(String, Arc<Vec<Stmt>>)> {
        let off = self.triggers_off.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.triggers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(id, s)| match s {
                Stmt::CreateTrigger { table: t, events, instead: i, body, .. } if *i == instead && table_id(t) == table && events.iter().any(|e| e.eq_ignore_ascii_case(event)) && !off.contains(id) => Some((id.clone(), body.clone())),
                _ => None,
            })
            .collect()
    }
}

impl ClusterSession {
    /// `CREATE [OR ALTER] TRIGGER name ON table FOR | AFTER | INSTEAD OF events AS body`.
    pub(crate) fn create_trigger(&mut self, st: &Stmt) -> Result<(), SqlError> {
        let Stmt::CreateTrigger { name, table, events, instead, or_alter, .. } = st else { return Ok(()) };
        let tid = table_id(table);
        if self.table_by_id(&tid).is_none() {
            if self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&tid) {
                return Err(unsupported("a trigger on a view"));
            }
            return Err(SqlError::new(208, format!("Invalid object name '{}'.", table.join("."))));
        }
        if let Stmt::CreateTrigger { body, .. } = st {
            if let Some(why) = body_problem(body) {
                return Err(unsupported(&why));
            }
        }
        let id = table_id(name);
        let mut reg = self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner());
        let known = reg.iter().any(|(i, _)| *i == id);
        if known && !*or_alter {
            return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", name.last().cloned().unwrap_or_default())));
        }
        if *instead {
            for ev in events {
                let taken = reg.iter().any(|(i, s)| *i != id && matches!(s, Stmt::CreateTrigger { table: t, events: e2, instead: true, .. } if table_id(t) == tid && e2.iter().any(|x| x.eq_ignore_ascii_case(ev))));
                if taken {
                    return Err(SqlError::new(2111, format!("Cannot create trigger '{}' on '{}' because an INSTEAD OF {} trigger already exists on the table.", name.last().cloned().unwrap_or_default(), table.join("."), ev)));
                }
            }
        }
        match reg.iter_mut().find(|(i, _)| *i == id) {
            Some(slot) => slot.1 = st.clone(),
            None => reg.push((id, st.clone())),
        }
        drop(reg);
        self.plan.push("trigger: kept by the control node, which fires it around the statements that change the table".into());
        self.log_schema(st);

        let Stmt::CreateTrigger { body, text, .. } = st else { return Ok(()) };
        let db = if table.len() >= 3 { table[0].as_str() } else { self.cur_db.as_str() };
        let (schema, trg_name) = match name.as_slice() {
            [_, s, n] => (s.as_str(), n.as_str()),
            [s, n] => (s.as_str(), n.as_str()),
            _ => ("dbo", name.last().map(|s| s.as_str()).unwrap_or_default()),
        };
        let (tbl_schema, tbl_name) = match table.as_slice() {
            [_, s, n] => (s.as_str(), n.as_str()),
            [s, n] => (s.as_str(), n.as_str()),
            _ => ("dbo", table.last().map(|s| s.as_str()).unwrap_or_default()),
        };
        let oid = self.cluster.mirror.new_object_id();
        let trg = Arc::new(tsql_server::catalog::Trigger {
            name: trg_name.to_string(),
            schema: schema.to_string(),
            events: events.clone(),
            instead: *instead,
            body: body.clone(),
            text: text.clone(),
            object_id: oid,
            disabled: false,
        });
        self.cluster.mirror.register_trigger_meta(db, tbl_schema, tbl_name, trg);

        Ok(())
    }

    /// `DROP TRIGGER`.
    pub(crate) fn drop_triggers(&mut self, st: &Stmt, names: &[Name], if_exists: bool) -> Result<(), SqlError> {
        for n in names {
            let id = table_id(n);
            let mut reg = self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner());
            if reg.iter().any(|(i, _)| *i == id) {
                reg.retain(|(i, _)| *i != id);
                self.cluster.triggers_off.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                let db = if n.len() >= 3 { n[0].as_str() } else { self.cur_db.as_str() };
                let (schema, trg_name) = match n.as_slice() {
                    [_, s, name] => (s.as_str(), name.as_str()),
                    [s, name] => (s.as_str(), name.as_str()),
                    _ => ("dbo", n.last().map(|s| s.as_str()).unwrap_or_default()),
                };
                self.cluster.mirror.drop_trigger_meta(db, schema, trg_name);
            } else if !if_exists {
                return Err(SqlError::new(3701, format!("Cannot drop the trigger '{}', because it does not exist or you do not have permission.", n.join("."))));
            }
        }
        self.log_schema(st);
        Ok(())
    }

    /// `ENABLE | DISABLE TRIGGER name | ALL ON table`.
    pub(crate) fn trigger_admin(&mut self, a: &TriggerAdmin) -> Result<(), SqlError> {
        if matches!(a.scope, TriggerScope::Database) {
            return self.ddl_trigger_admin(a);
        }
        let (TriggerScope::Table(table), TriggerAct::Enable(on)) = (&a.scope, &a.act) else {
            return Err(unsupported("ALL SERVER and LOGON triggers"));
        };
        let table = self.qn(table);
        let names: Vec<Name> = a.names.iter().map(|n| self.qn(n)).collect();
        let tid = table_id(&table);
        let ids: Vec<String> = {
            let reg = self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner());
            if a.all {
                reg.iter().filter(|(i, s)| !crate::rules::is_rule_id(i) && matches!(s, Stmt::CreateTrigger { table: t, .. } if table_id(t) == tid)).map(|(i, _)| i.clone()).collect()
            } else {
                let mut v = vec![];
                for n in &names {
                    let id = table_id(n);
                    if !reg.iter().any(|(i, _)| *i == id) {
                        return Err(SqlError::new(8197, format!("Trigger '{}' does not exist or you do not have permission.", n.join("."))));
                    }
                    v.push(id);
                }
                v
            }
        };
        {
            let mut off = self.cluster.triggers_off.lock().unwrap_or_else(|e| e.into_inner());
            for id in &ids {
                if *on {
                    off.remove(id);
                } else {
                    off.insert(id.clone());
                }
            }
        }
        for id in &ids {
            let reg = self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, Stmt::CreateTrigger { name, table, .. })) = reg.iter().find(|(i, _)| i == id) {
                let db = if table.len() >= 3 { table[0].as_str() } else { self.cur_db.as_str() };
                let (schema, trg_name) = match name.as_slice() {
                    [_, s, n] => (s.as_str(), n.as_str()),
                    [s, n] => (s.as_str(), n.as_str()),
                    _ => ("dbo", name.last().map(|s| s.as_str()).unwrap_or_default()),
                };
                self.cluster.mirror.enable_trigger_meta(db, schema, trg_name, *on);
            }
        }
        // (logged with the names in full, as `table_id` reads them back)
        let logged = Stmt::TriggerAdmin(Box::new(TriggerAdmin { names, all: a.all, scope: TriggerScope::Table(table), act: TriggerAct::Enable(*on) }));
        self.log_schema(&logged);
        Ok(())
    }

    /// Whether the statement of an event on a table must go through the triggers (it is not already being run for them, and a trigger
    /// of the table would fire).
    pub(crate) fn triggers_apply(&self, table: &str, ev: Ev) -> bool {
        if self.capture.as_ref().is_some_and(|c| c.table == table) {
            return false;
        }
        if !self.cluster.has_triggers(table) {
            return false;
        }
        let after = !self.trigger_stack.last().is_some_and(|(t, instead)| t == table && !*instead) && !self.cluster.triggers_on(table, ev.name(), false).is_empty();
        let instead = !self.trigger_stack.iter().any(|(t, i)| t == table && *i) && !self.cluster.triggers_on(table, ev.name(), true).is_empty();
        after || instead
    }

    /// Whether the statement being run for `table` is to have its rows captured.
    pub(crate) fn capturing(&self, table: &str) -> bool {
        self.capture.as_ref().is_some_and(|c| c.table == table)
    }

    /// Keep the rows a shard's `OUTPUT` gave (a replicated table's come once: every shard has the same).
    pub(crate) fn capture_rows(&mut self, results: Vec<ResultSet>, replicated: bool) {
        let Some(c) = self.capture.as_mut() else { return };
        let take = if replicated { 1 } else { results.len() };
        for rs in results.into_iter().take(take) {
            // (an UPDATE's rows are the inserted columns and then the deleted ones)
            let n = if c.event == "UPDATE" { rs.columns.len() / 2 } else { rs.columns.len() };
            if !c.seen {
                c.columns = rs.columns[..n].to_vec();
                c.types = rs.types[..n].to_vec();
                c.seen = true;
            }
            for row in rs.rows {
                match c.event {
                    "INSERT" => c.ins.push(row),
                    "DELETE" => c.del.push(row),
                    _ => {
                        c.ins.push(row[..n].to_vec());
                        c.del.push(row[n..].to_vec());
                    }
                }
            }
        }
    }

    /// Run an `INSERT`, `UPDATE` or `DELETE` of a table that has triggers: in a transaction, with its triggers (see the top of this file).
    pub(crate) async fn dml_with_triggers(&mut self, t: &DistTable, ev: Ev, st: Stmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        if matches!(ev, Ev::Update) && matches!(&st, Stmt::Update(u) if self.update_moves_key(t, u)) {
            return Err(unsupported("an UPDATE of the shard key of a table that has triggers"));
        }
        if self.trigger_stack.len() >= NEST_LIMIT {
            return Err(SqlError::new(217, "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32)."));
        }
        let own_txn = self.txn.borrow().is_none();
        if own_txn {
            *self.txn.borrow_mut() = Some(TxnState { shards: Default::default(), depth: 1, doomed: false, wrote: Default::default(), savepoints: vec![] });
        }
        let out = self.dml_and_triggers(t, ev, st, events).await;
        if own_txn {
            // (a trigger that rolled the transaction back has ended it already)
            if self.txn.borrow().is_none() {
                return out;
            }
            match out {
                Ok(()) => self.commit_txn(events).await,
                Err(e) => {
                    let _ = self.rollback_txn().await;
                    Err(e)
                }
            }
        } else {
            out
        }
    }

    async fn dml_and_triggers(&mut self, t: &DistTable, ev: Ev, st: Stmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let instead = !self.trigger_stack.iter().any(|(x, i)| *x == t.id && *i) && !self.cluster.triggers_on(&t.id, ev.name(), true).is_empty();
        if instead {
            return self.run_instead(t, ev, &st, events).await;
        }
        // the statement itself, with its rows kept
        self.capture = Some(Capture { table: t.id.clone(), event: ev.name(), columns: vec![], types: vec![], ins: vec![], del: vec![], seen: false });
        let done = match &st {
            Stmt::Insert(i) => Box::pin(self.insert_values(i, events)).await,
            Stmt::Update(u) => Box::pin(self.update(u, events)).await,
            Stmt::Delete(d) => Box::pin(self.delete(d, events)).await,
            _ => Ok(()),
        };
        let cap = self.capture.take();
        done?;
        let Some(cap) = cap else { return Ok(()) };
        if !cap.seen || (cap.ins.is_empty() && cap.del.is_empty()) {
            return Ok(());
        }
        // (the columns the statement set, for UPDATE(column))
        let set: Vec<bool> = match &st {
            Stmt::Update(u) => cap
                .columns
                .iter()
                .map(|c| u.sets.iter().any(|s| matches!(s, SetItem::Col { parts, .. } if parts.last().is_some_and(|p| p.eq_ignore_ascii_case(c)))))
                .collect(),
            _ => vec![true; cap.columns.len()],
        };
        let skip_after = self.trigger_stack.last().is_some_and(|(x, i)| *x == t.id && !*i);
        if skip_after {
            return Ok(());
        }
        for (id, body) in self.cluster.triggers_on(&t.id, ev.name(), false) {
            self.run_trigger(t, &id, &body, false, &cap.columns, &cap.types, &cap.ins, &cap.del, &set).await?;
        }
        Ok(())
    }

    /// An `INSTEAD OF` trigger: in place of the statement. `deleted` has the rows the statement would delete or change, `inserted` the rows
    /// it would insert or the changed rows.
    async fn run_instead(&mut self, t: &DistTable, ev: Ev, st: &Stmt, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let all = simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Star(None)], into: None, from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }], where_: None, group: None, having: None });
        let (columns, types, ins, del, set): (Vec<String>, Vec<tsql_server::DataType>, Vec<Vec<Value>>, Vec<Vec<Value>>, Vec<bool>);
        match st {
            Stmt::Delete(d) => {
                let mut q = all;
                if let QueryBody::Select(s) = &mut q.body {
                    s.where_ = d.where_.clone();
                }
                let rs = self.run_query(&q).await?;
                (columns, types, del, ins) = (rs.columns.clone(), rs.types.clone(), rs.rows, vec![]);
                set = vec![true; columns.len()];
            }
            Stmt::Update(u) => {
                let mut q = all.clone();
                if let QueryBody::Select(s) = &mut q.body {
                    s.where_ = u.where_.clone();
                }
                let old = self.run_query(&q).await?;
                // (the new rows: every column as it is, or as the statement sets it)
                let items: Vec<SelectItem> = old
                    .columns
                    .iter()
                    .map(|c| {
                        let e = u.sets.iter().find_map(|s| match s {
                            SetItem::Col { parts, e, op: None } if parts.last().is_some_and(|p| p.eq_ignore_ascii_case(c)) => Some(e.clone()),
                            _ => None,
                        });
                        SelectItem::Expr { e: e.unwrap_or_else(|| col(c)), alias: Some(c.clone()) }
                    })
                    .collect();
                let mut nq = all;
                if let QueryBody::Select(s) = &mut nq.body {
                    s.items = items;
                    s.where_ = u.where_.clone();
                }
                let new = self.run_query(&nq).await?;
                set = old.columns.iter().map(|c| u.sets.iter().any(|s| matches!(s, SetItem::Col { parts, .. } if parts.last().is_some_and(|p| p.eq_ignore_ascii_case(c))))).collect();
                (columns, types, del, ins) = (old.columns.clone(), old.types.clone(), old.rows, new.rows);
            }
            Stmt::Insert(i) => {
                // the shape of the table, and the rows given as the table's columns (the others NULL)
                let mut q = all;
                if let QueryBody::Select(s) = &mut q.body {
                    s.top = Some(Top { e: Expr::Lit(Value::Int(0)), percent: false, ties: false });
                }
                let shape = self.run_query(&q).await?;
                let given: Vec<String> = i.cols.clone().unwrap_or_else(|| shape.columns.clone());
                let source: Vec<Vec<Value>> = match &i.src {
                    InsertSrc::Values(rows) => {
                        let mut out = vec![];
                        for r in rows {
                            let mut vals = vec![];
                            for e in r {
                                let b = self.bind_expr(e)?;
                                vals.push(self.eval_scalar(&b).await?);
                            }
                            out.push(vals);
                        }
                        out
                    }
                    InsertSrc::Query(sq) => self.run_query(sq).await?.rows,
                    _ => return Err(unsupported("an INSERT ... EXEC or DEFAULT VALUES into a table that has an INSTEAD OF trigger")),
                };
                let mut rows = vec![];
                for r in source {
                    if r.len() != given.len() {
                        return Err(SqlError::new(213, "Column name or number of supplied values does not match table definition."));
                    }
                    let row: Vec<Value> = shape.columns.iter().map(|c| given.iter().position(|g| g.eq_ignore_ascii_case(c)).map(|p| r[p].clone()).unwrap_or(Value::Null)).collect();
                    rows.push(row);
                }
                (columns, types, ins, del) = (shape.columns.clone(), shape.types.clone(), rows, vec![]);
                set = vec![true; columns.len()];
            }
            _ => return Ok(()),
        }
        let n = ins.len().max(del.len()) as i64;
        if n > 0 {
            for (id, body) in self.cluster.triggers_on(&t.id, ev.name(), true) {
                self.run_trigger(t, &id, &body, true, &columns, &types, &ins, &del, &set).await?;
            }
        }
        // (what the statement would have changed: the client is told as it would have been)
        events.push(Event::RowCount(n));
        Ok(())
    }

    /// Run the body of one trigger over the rows of a statement.
    #[allow(clippy::too_many_arguments)]
    async fn run_trigger(&mut self, t: &DistTable, _id: &str, body: &Arc<Vec<Stmt>>, instead: bool, columns: &[String], types: &[tsql_server::DataType], ins: &[Vec<Value>], del: &[Vec<Value>], set: &[bool]) -> Result<(), SqlError> {
        if self.trigger_stack.len() >= NEST_LIMIT {
            return Err(SqlError::new(217, "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32)."));
        }
        let mut temps: Vec<String> = vec![];
        let made = async {
            let mk = |rows: &[Vec<Value>]| ResultSet { columns: columns.to_vec(), types: types.to_vec(), rows: rows.to_vec() };
            let i = self.materialize(mk(ins), None, &mut temps).await?;
            let d = self.materialize(mk(del), None, &mut temps).await?;
            match (i, d) {
                (Some(i), Some(d)) => Ok((i, d)),
                _ => Err(unsupported("a trigger over more rows than a statement can broadcast, or over columns that have no distinct names")),
            }
        }
        .await;
        let (ins_t, del_t) = match made {
            Ok(x) => x,
            Err(e) => {
                self.drop_temps(temps).await;
                return Err(e);
            }
        };
        let stmts = rewrite_body(body, &ins_t, &del_t, columns, set);
        self.trigger_stack.push((t.id.clone(), instead));
        let saved_vars = self.vars.clone();
        let saved_cursors = std::mem::take(&mut self.cursors);
        let had_txn = self.txn.borrow().is_some();
        let mut quiet: Vec<Event> = vec![];
        let mut r: Result<(), SqlError> = Ok(());
        self.returning = false;
        let mut i = 0;
        while i < stmts.len() {
            r = Box::pin(self.exec_stmt(&stmts[i], &mut quiet)).await;
            if r.is_err() || self.returning || (had_txn && self.txn.borrow().is_none()) {
                break;
            }
            if let Some(target) = self.goto_target.take() {
                if let Some(idx) = stmts.iter().position(|s| matches!(s, Stmt::Label(l) if l.eq_ignore_ascii_case(&target))) {
                    i = idx + 1;
                    continue;
                } else {
                    self.goto_target = Some(target);
                    break;
                }
            }
            i += 1;
        }
        self.returning = false;
        // (an error that ends a trigger is one that the transaction cannot go on after, as in SQL Server, where XACT_ABORT is on in a trigger: the
        // statement and what the trigger did before the error stay, and the transaction can only be rolled back)
        if r.is_err() && had_txn {
            if let Some(t) = self.txn.borrow_mut().as_mut() {
                t.doomed = true;
            }
        }
        self.vars = saved_vars;
        self.cursors = saved_cursors;
        self.trigger_stack.pop();
        self.drop_temps(temps).await;
        r?;
        if had_txn && self.txn.borrow().is_none() {
            let mut e = SqlError::new(3609, "The transaction ended in the trigger. The batch has been aborted.");
            e.abort_batch = true;
            return Err(e);
        }
        Ok(())
    }

    /// Whether an `UPDATE` sets the shard key.
    pub(crate) fn update_moves_key(&self, t: &DistTable, u: &Update) -> bool {
        let Dist::Sharded { key, .. } = &t.dist else { return false };
        u.sets.iter().any(|s| matches!(s, SetItem::Col { parts, .. } if parts.last().is_some_and(|p| p.eq_ignore_ascii_case(&t.cols[*key].name))))
    }
}

/// The body of a trigger with `inserted` and `deleted` made the temporary tables that hold the rows (kept under their own names as
/// aliases, so `inserted.id` still reads), and `UPDATE(column)` made the fact.
fn rewrite_body(body: &[Stmt], ins: &str, del: &str, columns: &[String], set: &[bool]) -> Vec<Stmt> {
    fn walk(v: &mut Json, ins: &str, del: &str, columns: &[String], set: &[bool]) {
        match v {
            Json::Object(m) => {
                if m.len() == 1 {
                    if let Some(Json::Object(t)) = m.get_mut("Table") {
                        let which = match t.get("name") {
                            Some(Json::Array(p)) if p.len() == 1 => p[0].as_str().map(|s| s.to_lowercase()),
                            _ => None,
                        };
                        if let Some(w) = which.filter(|w| w == "inserted" || w == "deleted") {
                            let shown = t.get("name").and_then(|n| n.get(0)).and_then(|s| s.as_str()).unwrap_or("").to_string();
                            t.insert("name".into(), Json::Array(vec![Json::String(if w == "inserted" { ins } else { del }.to_string())]));
                            if t.get("alias").map(|a| a.is_null()).unwrap_or(true) {
                                t.insert("alias".into(), Json::String(shown));
                            }
                        }
                    }
                    if let Some(Json::Object(f)) = m.get("Func") {
                        let is_update = f.get("name").and_then(|n| n.as_str()).is_some_and(|n| n.eq_ignore_ascii_case("UPDATE"));
                        if is_update {
                            let arg = f.get("args").and_then(|a| a.get(0)).and_then(|a| a.get("Col")).and_then(|c| c.as_array()).and_then(|c| c.last()).and_then(|c| c.as_str());
                            if let Some(c) = arg {
                                let flag = columns.iter().position(|x| x.eq_ignore_ascii_case(c)).map(|i| set.get(i).copied().unwrap_or(true)).unwrap_or(false);
                                *v = serde_json::json!({ "Lit": { "Bit": flag } });
                                return;
                            }
                        }
                    }
                }
                for c in m.values_mut() {
                    walk(c, ins, del, columns, set);
                }
            }
            Json::Array(a) => {
                for c in a.iter_mut() {
                    walk(c, ins, del, columns, set);
                }
            }
            _ => {}
        }
    }
    let Ok(mut j) = serde_json::to_value(body) else { return body.to_vec() };
    walk(&mut j, ins, del, columns, set);
    serde_json::from_value(j).unwrap_or_else(|_| body.to_vec())
}
