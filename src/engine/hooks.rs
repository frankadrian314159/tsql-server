//! Glue between the engine and the durability / security layers.
use super::*;
use crate::persist::LogOp;
use crate::storage::tx::{TabRef, Undo};
use crate::storage::{ReadView, Row, RowId, Stamp, TxId};
use std::collections::HashSet;

impl<'a> Engine<'a> {
    /// Append operations to the log without waiting for them to reach the disk
    /// (non-transactional changes such as sequence values and security settings).
    pub fn log_nowait(&mut self, ops: Vec<LogOp>) {
        let (clock, oid, rv) = (self.srv.core.clock(), self.srv.core.object_id(), self.srv.core.rowversion());
        if let Some(p) = self.srv.persist().as_mut() {
            let _ = p.append(clock, oid, rv, ops, false);
        }
    }

    /// Persist a database option change (non-transactional).
    pub fn log_database_options(&mut self, db: &str) {
        if let Some(d) = self.srv.databases.get(db) {
            let op = LogOp::DbOptions { db: db.to_string(), allow_snapshot: d.allow_snapshot, rcsi: d.rcsi };
            self.log_nowait(vec![op]);
        }
    }

    /// Operations that load every row a transaction sees in `t` into a table that was just (re)defined.
    fn dump_table(&self, db: &str, key: &str, t: &Table, tx: TxId, ops: &mut Vec<LogOp>) {
        ops.push(LogOp::TableReset { db: db.to_string(), key: key.to_string() });
        for (rowid, row) in t.data.iter(ReadView::committed(tx)) {
            ops.push(LogOp::Put { db: db.to_string(), key: key.to_string(), rowid, row: Some(row) });
        }
        if t.identity_col().is_some() {
            ops.push(LogOp::SetIdentity { db: db.to_string(), key: key.to_string(), next: t.identity_next.get() });
        }
    }

    /// Write a committing transaction's redo records to the log (without forcing them to disk). Returns the log
    /// sequence number to wait for with [`Engine::sync_log`], or `None` when there is nothing to make durable.
    pub fn write_redo(&mut self, tx: TxId, undo: &[Undo], ts: u64) -> Result<Option<u64>> {
        if self.srv.persist().is_none() {
            return Ok(None);
        }
        if let Some(e) = self.srv.store.failed() {
            return Err(SqlError::with(823, 24, 2, format!("The transaction cannot be committed because the storage engine has failed: {}", e)));
        }
        let mut ops: Vec<LogOp> = vec![];
        let mut dbs: HashSet<String> = HashSet::new();
        // objects (re)defined by this transaction, with the heap they had before it (None = new object)
        let mut objs: Vec<((String, String), Option<u32>)> = vec![];
        let mut seen_catalog: HashSet<String> = HashSet::new();
        for u in undo {
            match u {
                Undo::Database { key, .. } => {
                    dbs.insert(key.clone());
                }
                Undo::Object { db, key, prev } => {
                    if !objs.iter().any(|(k, _)| k.0 == *db && k.1 == *key) {
                        let before = match prev {
                            Some(Object::Table(p)) => Some(p.data.heap_id()),
                            _ => None,
                        };
                        objs.push(((db.clone(), key.clone()), before));
                    }
                }
                _ => {}
            }
        }
        // tables whose rows are written out in full (the row-level records would be redundant)
        let mut dumped: HashSet<(String, String)> = HashSet::new();
        for u in undo {
            match u {
                Undo::Database { key, .. } => {
                    if !seen_catalog.insert(format!("D:{}", key)) {
                        continue;
                    }
                    match self.srv.databases.get(key) {
                        Some(d) => {
                            ops.push(LogOp::PutDatabase { key: key.clone(), db: d.clone() });
                            for (k, o) in &d.objects {
                                if let Object::Table(t) = o {
                                    self.dump_table(key, k, t, tx, &mut ops);
                                }
                            }
                        }
                        None => ops.push(LogOp::DropDatabase { key: key.clone() }),
                    }
                }
                Undo::Object { db, key, .. } => {
                    if dbs.contains(db) || !seen_catalog.insert(format!("O:{}:{}", db, key)) {
                        continue;
                    }
                    match self.srv.databases.get(db).and_then(|d| d.objects.get(key)) {
                        Some(Object::Table(t)) => {
                            ops.push(LogOp::PutObject { db: db.clone(), key: key.clone(), obj: Object::Table(t.clone()) });
                            let before = objs.iter().find(|(k, _)| k.0 == *db && k.1 == *key).and_then(|(_, b)| *b);
                            if before != Some(t.data.heap_id()) {
                                self.dump_table(db, key, t, tx, &mut ops);
                                dumped.insert((db.clone(), key.clone()));
                            }
                        }
                        Some(o) => ops.push(LogOp::PutObject { db: db.clone(), key: key.clone(), obj: o.clone() }),
                        None => ops.push(LogOp::DropObject { db: db.clone(), key: key.clone() }),
                    }
                }
                Undo::Type { db, key, .. } => {
                    if dbs.contains(db) || !seen_catalog.insert(format!("T:{}:{}", db, key)) {
                        continue;
                    }
                    match self.srv.databases.get(db).and_then(|d| d.types.get(key)) {
                        Some(t) => ops.push(LogOp::PutType { db: db.clone(), key: key.clone(), ty: t.clone() }),
                        None => ops.push(LogOp::DropType { db: db.clone(), key: key.clone() }),
                    }
                }
                Undo::Schema { db, key, .. } => {
                    if dbs.contains(db) || !seen_catalog.insert(format!("S:{}:{}", db, key)) {
                        continue;
                    }
                    if let Some(d) = self.srv.databases.get(db) {
                        match d.schemas.get(key) {
                            Some(n) => ops.push(LogOp::PutSchema { db: db.clone(), key: key.clone(), name: n.clone() }),
                            None => ops.push(LogOp::DropSchema { db: db.clone(), key: key.clone() }),
                        }
                        ops.push(LogOp::DbSecurity { db: db.clone(), sec: d.security.clone(), owners: d.schema_owner.clone() });
                    }
                }
                _ => {}
            }
        }
        // row changes of tables that were not rewritten in full
        let mut rows_seen: HashSet<(String, String, RowId)> = HashSet::new();
        let mut idents_seen: HashSet<(String, String)> = HashSet::new();
        for u in undo {
            if let Undo::Row { tab: TabRef::Db(db, key), rowid } = u {
                if dbs.contains(db) || dumped.contains(&(db.clone(), key.clone())) {
                    continue;
                }
                if !rows_seen.insert((db.clone(), key.clone(), *rowid)) {
                    continue;
                }
                if let Some(Object::Table(t)) = self.srv.databases.get(db).and_then(|d| d.objects.get(key)) {
                    let row: Option<Row> = match t.data.last_version(*rowid) {
                        Some(v) if v.stamp == Stamp::Tx(tx) => v.row,
                        _ => continue,
                    };
                    ops.push(LogOp::Put { db: db.clone(), key: key.clone(), rowid: *rowid, row });
                    if idents_seen.insert((db.clone(), key.clone())) && t.identity_col().is_some() {
                        ops.push(LogOp::SetIdentity { db: db.clone(), key: key.clone(), next: t.identity_next.get() });
                    }
                }
            }
        }
        if ops.is_empty() {
            return Ok(None);
        }
        let (oid, rv) = (self.srv.core.object_id(), self.srv.core.rowversion());
        let mut g = self.srv.persist();
        let lsn = g.as_mut().unwrap().append_buffered(ts, oid, rv, ops)?;
        Ok(Some(lsn))
    }

    /// Wait until the log is on disk up to `lsn` (group commit: one session forces the log for all that wait).
    pub fn sync_log(&mut self, lsn: u64) -> Result<()> {
        let srv: &ServerState = &self.srv;
        srv.core.sync_log(lsn, || {
            // the frames of every commit so far are taken out under the persist latch; they are written and forced
            // outside it, so other sessions can keep appending (to the next batch) during the force
            let batch = match srv.persist().as_mut() {
                Some(p) => p.take_batch(),
                None => return Err("no log".to_string()),
            };
            batch.force()
        })
        .map_err(|e| SqlError::with(3314, 21, 1, format!("I/O error: {}", e)))
    }

    /// Checkpoint the data directory when the log has grown and nothing is in flight.
    pub fn maybe_checkpoint(&mut self) {
        let due = self.srv.persist().as_ref().map(|p| p.wal_len > p.checkpoint_threshold).unwrap_or(false);
        if due && self.srv.core.no_active() {
            self.do_checkpoint();
        }
    }

    pub fn do_checkpoint(&mut self) {
        if !self.srv.core.no_active() {
            return;
        }
        // a checkpoint needs the catalog to itself: no statement may change a table while it is written
        let srv: &ServerState = self.srv.cat_mut();
        if !srv.core.no_active() {
            return;
        }
        if let Some(p) = srv.persist().as_mut() {
            let _ = p.checkpoint(srv);
        }
    }

    pub fn check_dml_permission(&mut self, loc: &Loc, name: &[String], perm: &str) -> Result<()> {
        let r = self.check_object_perm(loc, perm, name.last().map(|s| s.as_str()).unwrap_or(""));
        if matches!(perm, "DELETE" | "UPDATE" | "INSERT") {
            self.audit_access(perm, loc, r.is_ok());
        }
        r
    }

    pub fn check_select_permission(&mut self, loc: &Loc, name: &[String]) -> Result<()> {
        self.check_object_perm(loc, "SELECT", name.last().map(|s| s.as_str()).unwrap_or(""))
    }

    /// A table was created (SELECT INTO): record undo information.
    pub fn record_table_creation(&mut self, loc: &Loc) -> Result<()> {
        self.ensure_tx()?;
        match loc {
            Loc::Temp(n) => self.push_undo(Undo::Temp { name: n.clone(), prev: None }),
            Loc::GTemp(n) => self.push_undo(Undo::GTemp { name: n.clone(), prev: None }),
            Loc::Db(d, k) => self.push_undo(Undo::Object { db: d.clone(), key: k.clone(), prev: None }),
            Loc::Var(_) => {}
        }
        Ok(())
    }
}
