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
        let mut ops = self.redo_ops(tx, undo);
        if let Some(gid) = self.ses.prepared_gid {
            // the commit of a prepared transaction settles it in the same record
            ops.push(LogOp::Resolve { gid });
        }
        if ops.is_empty() {
            return Ok(None);
        }
        let (oid, rv) = (self.srv.core.object_id(), self.srv.core.rowversion());
        let repl = self.srv.replicator.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(r) = repl {
            // a replica: the record goes to the group's log first and is written to this node's own log once the group
            // has committed it (see `replicate_wait`), so a record that the group never commits never reaches the log
            let rec = crate::persist::WalRecord { lsn: 0, ts, wall: chrono::Utc::now().timestamp_micros(), next_object_id: oid, next_rowversion: rv, ops };
            let ticket = r.propose(&rec).map_err(|e| SqlError::with(3314, 21, 1, format!("The commit could not be replicated: {}", e)))?;
            self.ses.pending_repl = Some(crate::persist::PendingRepl { ticket, rec });
            return Ok(None);
        }
        let mut g = self.srv.persist();
        let lsn = g.as_mut().unwrap().append_buffered(ts, oid, rv, ops)?;
        Ok(Some(lsn))
    }

    /// The redo operations of a transaction's changes, as its commit logs them.
    fn redo_ops(&self, tx: TxId, undo: &[Undo]) -> Vec<LogOp> {
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
        ops
    }

    /// Two-phase commit, phase one: make the open explicit transaction's changes durable under `gid` (a `Prepare` record in
    /// the log, replicated when this server is a replica) while it stays open with its locks. After this the server will
    /// finish the commit when told to, even if it restarts or fails over first (`Server::resolve_prepared`); commit the
    /// transaction with `COMMIT` as usual, which settles it, or `ROLLBACK`, which settles it as aborted. Returns false when
    /// the transaction changed nothing (nothing to prepare, so nothing to wait for).
    pub fn prepare_tx(&mut self, gid: u64) -> Result<bool> {
        let Some(tx) = self.ses.tx.as_ref() else {
            return Err(SqlError::new(3902, "There is no open transaction to prepare."));
        };
        if self.ses.doomed {
            return Err(SqlError::new(3930, "The current transaction cannot be committed and cannot support operations that write to the log file. Roll back the transaction."));
        }
        if self.ses.prepared_gid.is_some() || self.srv.prepared.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&gid) {
            return Err(SqlError::new(50212, format!("Transaction id {} is already prepared.", gid)));
        }
        if let Some(e) = self.srv.store.failed() {
            return Err(SqlError::with(823, 24, 2, format!("The transaction cannot be prepared because the storage engine has failed: {}", e)));
        }
        let ops = self.redo_ops(tx.id, &tx.undo);
        if ops.is_empty() {
            return Ok(false);
        }
        let core = self.srv.core.clone();
        let commit = core.commit_lock();
        let ts = core.alloc_commit_ts();
        let (oid, rv) = (core.object_id(), core.rowversion());
        let rec_ops = vec![LogOp::Prepare { gid, ops: ops.clone() }];
        let repl = self.srv.replicator.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let outcome: Result<()> = if let Some(r) = repl {
            let rec = crate::persist::WalRecord { lsn: 0, ts, wall: chrono::Utc::now().timestamp_micros(), next_object_id: oid, next_rowversion: rv, ops: rec_ops };
            match r.propose(&rec) {
                Err(e) => Err(SqlError::with(3314, 21, 1, format!("The prepare could not be replicated: {}", e))),
                Ok(ticket) => {
                    self.ses.pending_repl = Some(crate::persist::PendingRepl { ticket, rec });
                    drop(commit);
                    self.replicate_wait()
                }
            }
        } else if self.srv.persist().is_some() {
            let appended = self.srv.persist().as_mut().unwrap().append_buffered(ts, oid, rv, rec_ops);
            drop(commit);
            appended.and_then(|lsn| self.sync_log(lsn))
        } else {
            drop(commit);
            Ok(())
        };
        core.finish_commit_ts(ts);
        outcome?;
        self.srv.prepared.lock().unwrap_or_else(|e| e.into_inner()).insert(gid, crate::persist::PreparedTxn { ops, live: true });
        self.ses.prepared_gid = Some(gid);
        Ok(true)
    }

    /// The session's prepared transaction is being rolled back on purpose: settle it as aborted (durably) so that a restart
    /// does not bring it back. Nothing to do for a session that has not prepared.
    pub fn abort_prepared(&mut self) -> Result<()> {
        let Some(gid) = self.ses.prepared_gid else { return Ok(()) };
        if let Some((ts, _rec)) = self.srv.log_settlement(gid, false, true)? {
            self.srv.prepared.lock().unwrap_or_else(|e| e.into_inner()).remove(&gid);
            self.srv.core.finish_commit_ts(ts);
        }
        self.ses.prepared_gid = None;
        Ok(())
    }

    /// The session's prepared transaction is going away without a decision (the session ended, or its commit failed): the
    /// transaction becomes *in doubt* — only its `Prepare` record remains, until `Server::resolve_prepared` settles it.
    pub fn abandon_prepared(&mut self) {
        if let Some(gid) = self.ses.prepared_gid.take() {
            if let Some(p) = self.srv.prepared.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&gid) {
                p.live = false;
            }
        }
    }

    /// Wait until the group has committed the record that `write_redo` proposed (nothing to do for a server that is not a
    /// replica), then write it to this node's own log (not forced: the group's log is the durable copy).
    pub fn replicate_wait(&mut self) -> Result<()> {
        let Some(p) = self.ses.pending_repl.take() else { return Ok(()) };
        let repl = self.srv.replicator.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(r) = repl {
            r.wait(p.ticket).map_err(|e| SqlError::with(3314, 21, 1, format!("The commit could not be replicated: {}", e)))?;
        }
        if let Some(pp) = self.srv.persist().as_mut() {
            pp.append_buffered(p.rec.ts, p.rec.next_object_id, p.rec.next_rowversion, p.rec.ops)?;
        }
        Ok(())
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
