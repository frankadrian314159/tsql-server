//! Transactions, isolation levels, lock acquisition and blocking for the engine.
use super::*;
use crate::storage::lock::{Mode, Res};
use crate::storage::tx::{Isolation, TabRef, Tx, Undo};
use crate::storage::{ReadView, RowId, Stamp, TxId, ViewMode};
use super::query::Schema;
use std::time::{Duration, Instant};

/// Internal error number meaning "a lock is held by another transaction; wait and retry".
pub const BLOCKED: i32 = -4242;

pub fn tab_ref(loc: &Loc) -> Option<TabRef> {
    match loc {
        Loc::Db(d, k) => Some(TabRef::Db(d.clone(), k.clone())),
        Loc::Temp(n) => Some(TabRef::Temp(n.clone())),
        Loc::GTemp(n) => Some(TabRef::GTemp(n.clone())),
        Loc::Var(_) => None,
    }
}

impl<'a> Engine<'a> {
    pub fn blocked(&mut self, blockers: Vec<TxId>) -> SqlError {
        self.ses.blocked_on = blockers;
        SqlError::new(BLOCKED, "lock wait required")
    }

    pub fn cur_tx(&self) -> TxId {
        self.ses.tx.as_ref().map(|t| t.id).unwrap_or(0)
    }

    pub fn begin_tx(&mut self, implicit: bool) -> Result<()> {
        let iso = self.ses.isolation;
        if iso == Isolation::Snapshot {
            let db = self.srv.databases.get(&self.ses.db.to_lowercase());
            if !db.map(|d| d.allow_snapshot).unwrap_or(false) {
                return Err(SqlError::new(3952, format!("Snapshot isolation transaction failed accessing database '{}' because snapshot isolation is not allowed in this database. Use ALTER DATABASE to allow snapshot isolation.", self.ses.db)));
            }
        }
        let id = self.srv.next_tx;
        self.srv.next_tx += 1;
        let start = self.srv.clock;
        let tx = Tx::new(id, iso, implicit, start);
        let snap = if iso == Isolation::Snapshot || self.db_rcsi() { Some(start) } else { None };
        self.srv.active.insert(id, crate::catalog::TxInfo { snapshot_ts: snap });
        self.ses.tx = Some(tx);
        Ok(())
    }

    pub fn db_rcsi(&self) -> bool {
        self.srv.databases.get(&self.ses.db.to_lowercase()).map(|d| d.rcsi).unwrap_or(false)
    }

    pub fn ensure_tx(&mut self) -> Result<TxId> {
        if self.ses.tx.is_none() {
            self.begin_tx(true)?;
        }
        Ok(self.cur_tx())
    }

    /// Commit the current transaction: write the redo log, stamp versions, release locks.
    pub fn commit_tx(&mut self) -> Result<()> {
        let tx = match self.ses.tx.take() {
            Some(t) => t,
            None => return Ok(()),
        };
        self.finish_commit(tx)
    }

    fn finish_commit(&mut self, mut tx: Tx) -> Result<()> {
        let id = tx.id;
        let mut undo = std::mem::take(&mut tx.undo);
        let wrote = undo.iter().any(|u| !matches!(u, Undo::Temp { .. } | Undo::GTemp { .. } | Undo::Row { tab: TabRef::Temp(_), .. } | Undo::Row { tab: TabRef::GTemp(_), .. }));
        let ts = self.srv.clock + 1;
        if wrote {
            if let Err(e) = self.write_redo(id, &undo, ts) {
                // could not make the commit durable: undo everything
                self.ses.tx = Some(Tx { undo, ..tx });
                self.rollback_tx();
                return Err(e);
            }
        }
        self.srv.clock = ts;
        // stamp versions, grouped per table
        let mut rows: Vec<(TabRef, Vec<RowId>)> = vec![];
        for u in undo.drain(..) {
            if let Undo::Row { tab, rowid } = u {
                match rows.iter_mut().find(|(t, _)| *t == tab) {
                    Some((_, v)) => v.push(rowid),
                    None => rows.push((tab, vec![rowid])),
                }
            }
        }
        self.srv.active.remove(&id);
        let horizon = self.srv.horizon();
        for (tab, rids) in rows {
            if let Some(t) = self.table_by_ref_mut(&tab) {
                let d = t.data_mut();
                let mut seen = std::collections::HashSet::new();
                for r in &rids {
                    if seen.insert(*r) {
                        d.commit_slot(*r, id, ts);
                        d.gc_slot(*r, horizon);
                    }
                }
            }
        }
        self.srv.locks.release_all(id);
        self.srv.cv_notify();
        self.maybe_checkpoint();
        Ok(())
    }

    pub fn rollback_tx(&mut self) {
        let tx = match self.ses.tx.take() {
            Some(t) => t,
            None => return,
        };
        let id = tx.id;
        self.ses.tx = Some(tx);
        self.undo_to(0);
        self.ses.tx = None;
        self.srv.active.remove(&id);
        self.srv.locks.release_all(id);
        self.srv.cv_notify();
    }

    /// Roll back everything and reset the nesting count.
    pub fn rollback_all(&mut self) {
        self.rollback_tx();
        self.ses.trancount = 0;
        self.ses.doomed = false;
    }

    pub fn table_by_ref_mut(&mut self, tab: &TabRef) -> Option<&mut Table> {
        match tab {
            TabRef::Db(d, k) => match self.srv.databases.get_mut(d).and_then(|db| db.objects.get_mut(k)) {
                Some(Object::Table(t)) => Some(t),
                _ => None,
            },
            TabRef::Temp(n) => self.ses.temp.get_mut(n),
            TabRef::GTemp(n) => self.srv.global_temp.get_mut(n),
        }
    }

    /// Undo this transaction's changes back to undo-log length `mark`.
    pub fn undo_to(&mut self, mark: usize) {
        loop {
            let u = match self.ses.tx.as_mut() {
                Some(tx) if tx.undo.len() > mark => tx.undo.pop().unwrap(),
                _ => break,
            };
            match u {
                Undo::Row { tab, rowid } => {
                    if let Some(t) = self.table_by_ref_mut(&tab) {
                        t.data_mut().pop_version(rowid);
                    }
                }
                Undo::Object { db, key, prev } => {
                    if let Some(d) = self.srv.databases.get_mut(&db) {
                        match prev {
                            Some(o) => {
                                d.objects.insert(key, o);
                            }
                            None => {
                                d.objects.remove(&key);
                            }
                        }
                        d.rebuild_triggers();
                    }
                }
                Undo::Temp { name, prev } => match prev {
                    Some(t) => {
                        self.ses.temp.insert(name, t);
                    }
                    None => {
                        self.ses.temp.remove(&name);
                    }
                },
                Undo::GTemp { name, prev } => match prev {
                    Some(t) => {
                        self.srv.global_temp.insert(name, t);
                    }
                    None => {
                        self.srv.global_temp.remove(&name);
                    }
                },
                Undo::Type { db, key, prev } => {
                    if let Some(d) = self.srv.databases.get_mut(&db) {
                        match prev {
                            Some(t) => {
                                d.types.insert(key, t);
                            }
                            None => {
                                d.types.remove(&key);
                            }
                        }
                    }
                }
                Undo::Schema { db, key, prev } => {
                    if let Some(d) = self.srv.databases.get_mut(&db) {
                        match prev {
                            Some(t) => {
                                d.schemas.insert(key, t);
                            }
                            None => {
                                d.schemas.remove(&key);
                            }
                        }
                    }
                }
                Undo::Database { key, prev } => match prev {
                    Some(d) => {
                        self.srv.databases.insert(key, d);
                    }
                    None => {
                        self.srv.databases.remove(&key);
                    }
                },
                Undo::Trigger { .. } => {}
            }
        }
    }

    pub fn push_undo(&mut self, u: Undo) {
        if let Some(tx) = self.ses.tx.as_mut() {
            tx.wrote = true;
            tx.undo.push(u);
        }
    }

    /// Record the previous state of a catalog object before DDL changes it.
    pub fn undo_object(&mut self, db: &str, key: &str) {
        if self.ses.tx.is_none() {
            return;
        }
        let prev = self.srv.databases.get(db).and_then(|d| d.objects.get(key)).cloned();
        self.push_undo(Undo::Object { db: db.to_string(), key: key.to_string(), prev });
    }

    // ------------------------------------------------------------------ locks
    pub fn lock_table(&mut self, oid: i32, mode: Mode) -> Result<()> {
        let tx = self.ensure_tx()?;
        match self.srv.locks.acquire(tx, &Res::Table(oid), mode) {
            Ok(()) => Ok(()),
            Err(b) => Err(self.blocked(b)),
        }
    }

    pub fn lock_row(&mut self, oid: i32, rowid: RowId, mode: Mode) -> Result<()> {
        let tx = self.ensure_tx()?;
        match self.srv.locks.acquire(tx, &Res::Row(oid, rowid), mode) {
            Ok(()) => Ok(()),
            Err(b) => Err(self.blocked(b)),
        }
    }

    /// The version-selection rule for reads of persistent tables in the current database.
    pub fn read_view_for(&self, db: &str) -> ReadView {
        let tx = match &self.ses.tx {
            Some(t) => t,
            None => return ReadView::committed(0),
        };
        match tx.isolation {
            Isolation::ReadUncommitted => ReadView { tx: tx.id, mode: ViewMode::Dirty },
            Isolation::ReadCommitted => {
                if self.srv.databases.get(db).map(|d| d.rcsi).unwrap_or(false) {
                    ReadView { tx: tx.id, mode: ViewMode::Snapshot(self.ses.stmt_ts) }
                } else {
                    ReadView::committed(tx.id)
                }
            }
            Isolation::RepeatableRead | Isolation::Serializable => ReadView::committed(tx.id),
            Isolation::Snapshot => ReadView { tx: tx.id, mode: ViewMode::Snapshot(tx.snapshot_ts.unwrap_or(self.srv.clock)) },
        }
    }

    /// Visible rows of a table with their row ids, honouring the isolation level.
    pub fn scan_rows(&mut self, loc: &Loc, t: &Table) -> Result<Vec<(RowId, Row)>> {
        self.scan_rows_ap(loc, t, &optimizer::Access::Full)
    }

    /// Like [`scan_rows`](Self::scan_rows) but locating rows through the given access path.
    /// Returns a "blocked" error when a needed row is being changed by another transaction.
    pub fn scan_rows_ap(&mut self, loc: &Loc, t: &Table, ap: &optimizer::Access) -> Result<Vec<(RowId, Row)>> {
        self.scan_rows_flt(loc, t, ap, None)
    }

    /// Scan through an access path, applying `flt` (the table's pushed-down conditions) while reading
    /// so that only matching rows are kept in memory.
    pub fn scan_rows_flt(&mut self, loc: &Loc, t: &Table, ap: &optimizer::Access, flt: Option<(&Schema, &[Expr])>) -> Result<Vec<(RowId, Row)>> {
        let db = match loc {
            Loc::Db(d, _) => d.clone(),
            _ => {
                let rows = t.rows_in(ReadView::dirty());
                return match flt {
                    Some((sc, es)) => self.filter_pushed(sc, rows, es),
                    None => Ok(rows),
                };
            }
        };
        let tx = self.ensure_tx()?;
        let iso = self.ses.tx.as_ref().map(|t| t.isolation).unwrap_or(Isolation::ReadCommitted);
        let oid = t.object_id;
        let rcsi = self.db_rcsi();
        match iso {
            Isolation::ReadUncommitted | Isolation::Snapshot => {}
            Isolation::ReadCommitted => {
                if !rcsi {
                    let b = self.srv.locks.conflicts(tx, &Res::Table(oid), Mode::IS);
                    if !b.is_empty() {
                        return Err(self.blocked(b));
                    }
                }
            }
            Isolation::RepeatableRead => self.lock_table(oid, Mode::IS)?,
            Isolation::Serializable => self.lock_table(oid, Mode::S)?,
        }
        if let optimizer::Access::Full = ap {
            // stream the heap in chunks, keeping only rows that pass the filter
            let view = self.read_view_for(&db);
            let mut out: Vec<(RowId, Row)> = Vec::new();
            let mut after = None;
            loop {
                let chunk = t.data.scan_chunk(&view, after, 512, true);
                if let Some(w) = chunk.blocked_by {
                    return Err(self.blocked(vec![w]));
                }
                if iso == Isolation::RepeatableRead {
                    for (rid, _) in &chunk.rows {
                        self.lock_row(oid, *rid, Mode::S)?;
                    }
                }
                let rows = match flt {
                    Some((sc, es)) => self.filter_pushed(sc, chunk.rows, es)?,
                    None => chunk.rows,
                };
                out.extend(rows);
                if chunk.done || chunk.last.is_none() {
                    break;
                }
                after = chunk.last;
            }
            return Ok(out);
        }
        let rids = ap.row_ids(t);
        self.read_rids(&db, t, iso, &rids, flt)
    }

    /// Visible rows for an explicit list of row ids (same locking rules as a scan).
    pub fn scan_rows_ids(&mut self, loc: &Loc, t: &Table, rids: &[RowId]) -> Result<Vec<(RowId, Row)>> {
        let db = match loc {
            Loc::Db(d, _) => d.clone(),
            _ => {
                let v = ReadView::dirty();
                return Ok(rids.iter().filter_map(|&r| t.data.get(r, &v).map(|row| (r, row))).collect());
            }
        };
        self.ensure_tx()?;
        let iso = self.ses.tx.as_ref().map(|t| t.isolation).unwrap_or(Isolation::ReadCommitted);
        self.read_rids(&db, t, iso, rids, None)
    }

    fn read_rids(&mut self, db: &str, t: &Table, iso: Isolation, rids: &[RowId], flt: Option<(&Schema, &[Expr])>) -> Result<Vec<(RowId, Row)>> {
        let oid = t.object_id;
        let view = self.read_view_for(db);
        let mut all: Vec<(RowId, Row)> = Vec::new();
        for chunk in rids.chunks(512) {
            let mut rows: Vec<(RowId, Row)> = Vec::new();
            for &rid in chunk {
                if view.mode == ViewMode::Committed {
                    if let Some(w) = t.data.foreign_writer(rid, view.tx) {
                        return Err(self.blocked(vec![w]));
                    }
                }
                if let Some(r) = t.data.get(rid, &view) {
                    rows.push((rid, r));
                }
            }
            if iso == Isolation::RepeatableRead {
                for (rid, _) in &rows {
                    self.lock_row(oid, *rid, Mode::S)?;
                }
            }
            match flt {
                Some((sc, es)) => all.extend(self.filter_pushed(sc, rows, es)?),
                None => all.extend(rows),
            }
        }
        Ok(all)
    }

    /// A single row by id as the current transaction sees it (no locking).
    pub fn row_view(&self, loc: &Loc) -> ReadView {
        match loc {
            Loc::Db(d, _) => self.read_view_for(d),
            _ => ReadView::dirty(),
        }
    }

    // ------------------------------------------------------------ waiting
    /// Wait for the transactions in `ses.blocked_on` to finish. Errors on deadlock or timeout.
    pub fn wait_blocked(&mut self) -> Result<()> {
        let blockers = std::mem::take(&mut self.ses.blocked_on);
        let me = self.cur_tx();
        let timeout = self.ses.lock_timeout;
        if timeout == 0 {
            return Err(SqlError::new(1222, "Lock request time out period exceeded."));
        }
        if me != 0 {
            self.srv.locks.set_wait(me, blockers);
            if self.srv.locks.deadlocked(me) {
                self.srv.locks.clear_wait(me);
                let spid = self.ses.spid;
                self.rollback_all();
                let mut e = SqlError::with(1205, 13, 51, format!("Transaction (Process ID {}) was deadlocked on lock resources with another process and has been chosen as the deadlock victim. Rerun the transaction.", spid));
                e.abort_batch = true;
                return Err(e);
            }
        }
        let started = Instant::now();
        let dur = if timeout > 0 { Some(Duration::from_millis(timeout as u64)) } else { None };
        let timed_out = self.srv.wait(dur);
        if me != 0 {
            self.srv.locks.clear_wait(me);
        }
        if timed_out {
            let _ = started;
            return Err(SqlError::new(1222, "Lock request time out period exceeded."));
        }
        Ok(())
    }

    /// Stamp for writes to `loc`.
    pub fn write_stamp(&self, loc: &Loc) -> Stamp {
        match loc {
            Loc::Var(_) => Stamp::Ts(0),
            _ => Stamp::Tx(self.cur_tx()),
        }
    }
}
