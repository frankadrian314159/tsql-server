//! Transactions, isolation levels, lock acquisition and blocking for the engine.
use super::*;
use crate::storage::lock::{Mode, Res};
use crate::storage::tx::{Isolation, TabRef, Tx, Undo};
use crate::storage::{ReadView, RowId, Stamp, TxId, ViewMode};
use super::query::Schema;
use std::time::{Duration, Instant};

/// Internal error number meaning "a lock is held by another transaction; wait and retry".
pub const BLOCKED: i32 = -4242;

/// What table hints do to the reads of one table.
#[derive(Default)]
pub struct HintEffect {
    pub iso: Option<Isolation>,
    /// UPDLOCK / XLOCK: rows read are locked exclusively and kept locked.
    pub write_lock: bool,
    pub table_lock: Option<Mode>,
    pub readpast: bool,
}

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

    /// The error a killed session's statement ends with (the transaction is rolled back).
    pub fn kill_error(&mut self) -> SqlError {
        self.srv.core.clear_kill(self.ses.spid);
        self.rollback_all();
        let mut e = SqlError::with(596, 21, 1, "Cannot continue the execution because the session is in the kill state.");
        e.abort_batch = true;
        e
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
        let snap = iso == Isolation::Snapshot || self.db_rcsi();
        let (id, start) = self.srv.core.begin(snap);
        let tx = Tx::new(id, iso, implicit, start);
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
        crate::span!("commit_tx");
        let tx = match self.ses.tx.take() {
            Some(t) => t,
            None => return Ok(()),
        };
        self.finish_commit(tx)
    }

    /// Commit protocol:
    ///
    /// 1. under the commit lock, take the next timestamp and append the transaction's redo record to the log (so the
    ///    log is in timestamp order);
    /// 2. wait until the record is on disk (group commit: sessions that commit together share one force);
    /// 3. stamp every version the transaction wrote with the timestamp;
    /// 4. publish the timestamp as the clock once it and all earlier ones are stamped (`Core::finish_commit_ts`),
    ///    so a snapshot never sees half a commit;
    /// 5. leave the active table, collect old versions no snapshot needs any more, release the locks.
    fn finish_commit(&mut self, mut tx: Tx) -> Result<()> {
        let id = tx.id;
        let mut undo = std::mem::take(&mut tx.undo);
        let wrote = undo.iter().any(|u| !matches!(u, Undo::Temp { .. } | Undo::GTemp { .. } | Undo::Row { tab: TabRef::Temp(_), .. } | Undo::Row { tab: TabRef::GTemp(_), .. }));
        let core = self.srv.core.clone();
        if undo.is_empty() {
            // changed nothing (a query, or a transaction that rolled everything back): no timestamp, log record or
            // versions to stamp, so none of the commit machinery
            core.end(id);
            core.locks.release_all(id);
            return Ok(());
        }
        let commit = core.commit_lock();
        let ts = core.alloc_commit_ts();
        let mut lsn = None;
        if wrote {
            match self.write_redo(id, &undo, ts) {
                Ok(l) => lsn = l,
                Err(e) => {
                    // could not log the commit: undo everything
                    drop(commit);
                    core.finish_commit_ts(ts);
                    self.ses.tx = Some(Tx { undo, ..tx });
                    self.rollback_tx();
                    return Err(e);
                }
            }
        }
        drop(commit);
        if let Some(l) = lsn {
            if let Err(e) = self.sync_log(l) {
                core.finish_commit_ts(ts);
                self.ses.tx = Some(Tx { undo, ..tx });
                self.rollback_tx();
                return Err(e);
            }
        }
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
        for (tab, rids) in rows.iter_mut() {
            let mut seen = std::collections::HashSet::new();
            rids.retain(|r| seen.insert(*r));
            if let Some(t) = self.table_by_ref(tab) {
                let d = &*t.data;
                for r in rids.iter() {
                    d.commit_slot(*r, id, ts);
                }
            }
        }
        core.finish_commit_ts(ts);
        core.end(id);
        let horizon = core.horizon();
        for (tab, rids) in &rows {
            if let Some(t) = self.table_by_ref(tab) {
                let d = &*t.data;
                for r in rids {
                    d.gc_slot(*r, horizon);
                }
            }
        }
        core.locks.release_all(id);
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
        self.srv.core.end(id);
        self.srv.core.locks.release_all(id);
    }

    /// Roll back everything and reset the nesting count.
    pub fn rollback_all(&mut self) {
        self.rollback_tx();
        self.ses.trancount = 0;
        self.ses.doomed = false;
    }

    /// The table an undo or commit record refers to (its rows are changed through the shared storage handle).
    pub fn table_by_ref(&self, tab: &TabRef) -> Option<&Table> {
        match tab {
            TabRef::Db(d, k) => match self.srv.databases.get(d).and_then(|db| db.objects.get(k)) {
                Some(Object::Table(t)) => Some(t),
                _ => None,
            },
            TabRef::Temp(n) => self.ses.temp.get(n),
            TabRef::GTemp(n) => self.srv.global_temp.get(n),
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
                    if let Some(t) = self.table_by_ref(&tab) {
                        t.data.pop_version(rowid);
                    }
                }
                Undo::Object { db, key, prev } => {
                    if let Some(d) = self.srv.cat_mut().databases.get_mut(&db) {
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
                        self.srv.cat_mut().global_temp.insert(name, t);
                    }
                    None => {
                        self.srv.cat_mut().global_temp.remove(&name);
                    }
                },
                Undo::Type { db, key, prev } => {
                    if let Some(d) = self.srv.cat_mut().databases.get_mut(&db) {
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
                    if let Some(d) = self.srv.cat_mut().databases.get_mut(&db) {
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
                        self.srv.cat_mut().databases.insert(key, d);
                    }
                    None => {
                        self.srv.cat_mut().databases.remove(&key);
                    }
                },
                Undo::Trigger { .. } => {}
            }
        }
    }

    pub fn push_undo(&mut self, u: Undo) {
        if let Some(tx) = self.ses.tx.as_mut() {
            tx.wrote = true;
            if !matches!(u, Undo::Row { .. } | Undo::Temp { .. } | Undo::GTemp { .. }) {
                self.srv.core.mark_ddl(tx.id);
            }
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
        match self.srv.core.locks.acquire(tx, &Res::Table(oid), mode) {
            Ok(()) => Ok(()),
            Err(b) => Err(self.blocked(b)),
        }
    }

    pub fn lock_row(&mut self, oid: i32, rowid: RowId, mode: Mode) -> Result<()> {
        let tx = self.ensure_tx()?;
        match self.srv.core.locks.acquire(tx, &Res::Row(oid, rowid), mode) {
            Ok(()) => Ok(()),
            Err(b) => Err(self.blocked(b)),
        }
    }

    /// SERIALIZABLE read of `oid` through `ap`. A full scan has no key range to lock, so it takes the table in
    /// shared mode (as SQL Server does for a scan without a usable index). An index seek or range scan takes an
    /// intention lock on the table and shared locks on exactly the key ranges it reads, so other transactions may
    /// still insert, update and delete outside them; rows read also get shared row locks (see `read_rids`).
    pub fn lock_for_serializable(&mut self, t: &Table, ap: &optimizer::Access) -> Result<()> {
        let oid = t.object_id;
        use crate::storage::lock::KeyRange;
        let tx = self.ensure_tx()?;
        match ap {
            optimizer::Access::Full => self.lock_table(oid, Mode::S),
            optimizer::Access::Seek { idx, keys, .. } => {
                self.lock_table(oid, Mode::IS)?;
                for k in keys {
                    // bounds are kept as the index stores keys (see `IndexSpec::colls`)
                    let k = t.data.index_bound(*idx, k);
                    self.srv.core.locks.acquire_range(KeyRange { tx, oid, idx: *idx, lo: Some((k.clone(), true)), hi: Some((k, true)) });
                }
                Ok(())
            }
            optimizer::Access::Range { idx, lo, hi, .. } => {
                self.lock_table(oid, Mode::IS)?;
                let conv = |b: &Option<(Vec<Value>, bool)>| b.as_ref().map(|(v, i)| (t.data.index_bound(*idx, v), *i));
                self.srv.core.locks.acquire_range(KeyRange { tx, oid, idx: *idx, lo: conv(lo), hi: conv(hi) });
                Ok(())
            }
        }
    }

    /// Before a write: no other SERIALIZABLE transaction may hold a key range that the new row images would
    /// fall into (that would be a phantom for it). Returns a "blocked" error naming those transactions.
    pub fn check_key_ranges(&mut self, t: &Table, rows: &[&Row]) -> Result<()> {
        if !self.srv.core.locks.has_ranges() {
            return Ok(());
        }
        let tx = self.cur_tx();
        let mut blockers: Vec<TxId> = vec![];
        for row in rows {
            for idx in 0..t.data.specs.len() {
                if !t.data.specs[idx].admits(row) {
                    continue;
                }
                let key = t.data.index_key(idx, row);
                blockers.extend(self.srv.core.locks.range_conflicts(tx, t.object_id, idx, &key));
            }
        }
        if blockers.is_empty() {
            return Ok(());
        }
        blockers.sort_unstable();
        blockers.dedup();
        Err(self.blocked(blockers))
    }

    /// The version-selection rule for reads of persistent tables in the current database.
    pub fn read_view_for(&self, db: &str) -> ReadView {
        match &self.ses.tx {
            Some(t) => self.read_view_iso(db, t.isolation),
            None => ReadView::committed(0),
        }
    }

    /// The read view of the current transaction as if it ran at isolation level `iso` (table hints change it per table).
    pub fn read_view_iso(&self, db: &str, iso: Isolation) -> ReadView {
        let tx = match &self.ses.tx {
            Some(t) => t,
            None => return ReadView::committed(0),
        };
        match iso {
            Isolation::ReadUncommitted => ReadView { tx: tx.id, mode: ViewMode::Dirty },
            Isolation::ReadCommitted => {
                if self.srv.databases.get(db).map(|d| d.rcsi).unwrap_or(false) {
                    ReadView { tx: tx.id, mode: ViewMode::Snapshot(self.ses.stmt_ts) }
                } else {
                    ReadView::committed(tx.id)
                }
            }
            Isolation::RepeatableRead | Isolation::Serializable => ReadView::committed(tx.id),
            Isolation::Snapshot => ReadView { tx: tx.id, mode: ViewMode::Snapshot(tx.snapshot_ts.unwrap_or_else(|| self.srv.core.clock())) },
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
        let mut out: Vec<(RowId, Row)> = Vec::new();
        self.scan_each(loc, t, ap, flt, &mut |_, chunk| {
            out.extend(chunk);
            Ok(true)
        })?;
        Ok(out)
    }

    /// Like [`Engine::scan_rows_flt`] but hands the qualifying rows to `f` a chunk at a time (so a statement
    /// need not hold them all); `f` returns `false` to stop the scan early. Locking and visibility rules are
    /// those of a scan.
    pub fn scan_each(&mut self, loc: &Loc, t: &Table, ap: &optimizer::Access, flt: Option<(&Schema, &[Expr])>, f: &mut dyn FnMut(&mut Self, Vec<(RowId, Row)>) -> Result<bool>) -> Result<()> {
        // row-level security: a filter predicate hides rows from every reader of the table
        if self.has_filter_policy(loc) {
            return self.scan_each_inner(loc, t, ap, flt, &mut |eng, rows| {
                let rows = eng.rls_filter(loc, t, rows)?;
                if rows.is_empty() {
                    return Ok(true);
                }
                f(eng, rows)
            });
        }
        self.scan_each_inner(loc, t, ap, flt, f)
    }

    fn scan_each_inner(&mut self, loc: &Loc, t: &Table, ap: &optimizer::Access, flt: Option<(&Schema, &[Expr])>, f: &mut dyn FnMut(&mut Self, Vec<(RowId, Row)>) -> Result<bool>) -> Result<()> {
        let db = match loc {
            Loc::Db(d, _) => d.clone(),
            // global temporary tables are shared between sessions, so they are locked like any table
            Loc::GTemp(_) => "tempdb".to_string(),
            _ => {
                let rows = t.rows_in(ReadView::dirty());
                let rows = match flt {
                    Some((sc, es)) => self.filter_pushed(sc, rows, es)?,
                    None => rows,
                };
                for chunk in rows.chunks(512) {
                    if !f(self, chunk.to_vec())? {
                        break;
                    }
                }
                return Ok(());
            }
        };
        let tx = self.ensure_tx()?;
        let oid = t.object_id;
        let h = self.table_hints(oid);
        let iso = h.iso.unwrap_or_else(|| self.ses.tx.as_ref().map(|t| t.isolation).unwrap_or(Isolation::ReadCommitted));
        let rcsi = self.db_rcsi();
        if let Some(mode) = h.table_lock {
            self.lock_table(oid, mode)?;
        }
        if h.write_lock {
            self.lock_table(oid, Mode::IX)?;
        }
        match iso {
            Isolation::ReadUncommitted | Isolation::Snapshot => {}
            Isolation::ReadCommitted => {
                if !rcsi {
                    let b = self.srv.core.locks.conflicts(tx, &Res::Table(oid), Mode::IS);
                    if !b.is_empty() {
                        return Err(self.blocked(b));
                    }
                }
            }
            Isolation::RepeatableRead => self.lock_table(oid, Mode::IS)?,
            Isolation::Serializable => self.lock_for_serializable(t, ap)?,
        }
        if let optimizer::Access::Full = ap {
            // stream the heap in chunks, keeping only rows that pass the filter
            let view = self.read_view_iso(&db, iso);
            let mut after = None;
            loop {
                let mut chunk = t.data.scan_chunk(&view, after, 512, !h.readpast);
                if let Some(w) = chunk.blocked_by {
                    return Err(self.blocked(vec![w]));
                }
                if h.readpast {
                    // skip the rows other transactions have uncommitted changes in
                    chunk.rows.retain(|(rid, _)| t.data.foreign_writer(*rid, tx).is_none());
                }
                if h.write_lock {
                    for (rid, _) in &chunk.rows {
                        self.lock_row(oid, *rid, Mode::X)?;
                    }
                    self.changed_since_read(t, chunk.rows.iter().map(|(r, _)| *r))?;
                } else if iso == Isolation::RepeatableRead {
                    for (rid, _) in &chunk.rows {
                        self.lock_row(oid, *rid, Mode::S)?;
                    }
                    self.changed_since_read(t, chunk.rows.iter().map(|(r, _)| *r))?;
                }
                let rows = match flt {
                    Some((sc, es)) => self.filter_pushed(sc, chunk.rows, es)?,
                    None => chunk.rows,
                };
                if !rows.is_empty() && !f(self, rows)? {
                    return Ok(());
                }
                if chunk.done || chunk.last.is_none() {
                    break;
                }
                after = chunk.last;
            }
            return Ok(());
        }
        let rids = ap.row_ids(t);
        for part in rids.chunks(512) {
            let rows = self.read_rids(&db, t, iso, part, flt)?;
            if !rows.is_empty() && !f(self, rows)? {
                break;
            }
        }
        Ok(())
    }

    /// Visible rows for an explicit list of row ids (same locking rules as a scan).
    pub fn scan_rows_ids(&mut self, loc: &Loc, t: &Table, rids: &[RowId]) -> Result<Vec<(RowId, Row)>> {
        let db = match loc {
            Loc::Db(d, _) => d.clone(),
            Loc::GTemp(_) => "tempdb".to_string(),
            _ => {
                let v = ReadView::dirty();
                return Ok(rids.iter().filter_map(|&r| t.data.get(r, &v).map(|row| (r, row))).collect());
            }
        };
        self.ensure_tx()?;
        let iso = self.ses.tx.as_ref().map(|t| t.isolation).unwrap_or(Isolation::ReadCommitted);
        let rows = self.read_rids(&db, t, iso, rids, None)?;
        if self.has_filter_policy(loc) {
            return self.rls_filter(loc, t, rows);
        }
        Ok(rows)
    }

    fn read_rids(&mut self, db: &str, t: &Table, iso: Isolation, rids: &[RowId], flt: Option<(&Schema, &[Expr])>) -> Result<Vec<(RowId, Row)>> {
        let oid = t.object_id;
        let h = self.table_hints(oid);
        let iso = h.iso.unwrap_or(iso);
        let view = self.read_view_iso(db, iso);
        let mut all: Vec<(RowId, Row)> = Vec::new();
        for chunk in rids.chunks(512) {
            let mut rows: Vec<(RowId, Row)> = Vec::new();
            for &rid in chunk {
                if view.mode == ViewMode::Committed {
                    if let Some(w) = t.data.foreign_writer(rid, view.tx) {
                        if h.readpast {
                            continue;
                        }
                        return Err(self.blocked(vec![w]));
                    }
                }
                if let Some(r) = t.data.get(rid, &view) {
                    rows.push((rid, r));
                }
            }
            if h.write_lock {
                self.lock_table(oid, Mode::IX)?;
                for (rid, _) in &rows {
                    self.lock_row(oid, *rid, Mode::X)?;
                }
                self.changed_since_read(t, rows.iter().map(|(r, _)| *r))?;
            } else if iso == Isolation::RepeatableRead || iso == Isolation::Serializable {
                for (rid, _) in &rows {
                    self.lock_row(oid, *rid, Mode::S)?;
                }
                self.changed_since_read(t, rows.iter().map(|(r, _)| *r))?;
            }
            match flt {
                Some((sc, es)) => all.extend(self.filter_pushed(sc, rows, es)?),
                None => all.extend(rows),
            }
        }
        Ok(all)
    }

    /// Rows are read before they are locked, so a transaction may have committed a change to one of them in between;
    /// the lock is then granted on a row whose value this statement has not seen (a read-modify-write would lose that
    /// update). A row committed after the statement began makes the statement run again with a fresh view.
    fn changed_since_read(&mut self, t: &Table, rids: impl Iterator<Item = RowId>) -> Result<()> {
        let began = self.ses.stmt_ts;
        for rid in rids {
            if t.data.latest_committed_ts(rid) > began {
                return Err(self.blocked(vec![]));
            }
        }
        Ok(())
    }

    /// A single row by id as the current transaction sees it (no locking).
    pub fn row_view(&self, loc: &Loc) -> ReadView {
        match loc {
            Loc::Db(d, _) => self.read_view_for(d),
            _ => ReadView::dirty(),
        }
    }

    // ------------------------------------------------------------ waiting
    /// The effect of the table hints on `oid` in the statement being run.
    pub fn table_hints(&self, oid: i32) -> HintEffect {
        let mut h = HintEffect::default();
        if let Some(v) = self.ses.hints.get(&oid) {
            for x in v {
                match x.as_str() {
                    "NOLOCK" | "READUNCOMMITTED" => h.iso = Some(Isolation::ReadUncommitted),
                    "READCOMMITTED" | "READCOMMITTEDLOCK" => h.iso = Some(Isolation::ReadCommitted),
                    "REPEATABLEREAD" => h.iso = Some(Isolation::RepeatableRead),
                    "SERIALIZABLE" | "HOLDLOCK" => h.iso = Some(Isolation::Serializable),
                    "UPDLOCK" | "XLOCK" => h.write_lock = true,
                    "TABLOCK" => h.table_lock = h.table_lock.or(Some(Mode::S)),
                    "TABLOCKX" => h.table_lock = Some(Mode::X),
                    "READPAST" => h.readpast = true,
                    _ => {}
                }
            }
        }
        h
    }

    /// Record the table hints of the tables in `from` for the statement being run (and validate them).
    pub fn collect_hints(&mut self, from: &[TableRef]) -> Result<()> {
        const KNOWN: &[&str] = &[
            "NOLOCK", "READUNCOMMITTED", "READCOMMITTED", "READCOMMITTEDLOCK", "REPEATABLEREAD", "SERIALIZABLE", "HOLDLOCK", "UPDLOCK", "XLOCK", "TABLOCK", "TABLOCKX", "READPAST", "NOWAIT", "ROWLOCK",
            "PAGLOCK", "NOEXPAND", "INDEX", "FORCESEEK", "FORCESCAN", "KEEPIDENTITY", "KEEPDEFAULTS", "IGNORE_CONSTRAINTS", "IGNORE_TRIGGERS", "SNAPSHOT", "SPATIAL_WINDOW_MAX_CELLS",
        ];
        fn walk<'q>(tr: &'q TableRef, out: &mut Vec<(&'q Vec<String>, &'q Vec<String>)>) {
            match tr {
                TableRef::Table { name, hints, .. } if !hints.is_empty() => out.push((name, hints)),
                TableRef::Join { left, right, .. } => {
                    walk(left, out);
                    walk(right, out);
                }
                _ => {}
            }
        }
        let mut found = vec![];
        for tr in from {
            walk(tr, &mut found);
        }
        for (name, hints) in found {
            for h in hints {
                if !KNOWN.contains(&h.as_str()) {
                    return Err(SqlError::with(321, 15, 1, format!("\"{}\" is not a recognized table hints option. If it is intended as a parameter to a table-valued function or to the CHANGETABLE function, ensure that your database compatibility mode is set to 90.", h.to_lowercase())));
                }
            }
            let loc = self.locate(name);
            if let Some(t) = self.table_ref(&loc) {
                let oid = t.object_id;
                self.ses.hints.entry(oid).or_default().extend(hints.iter().cloned());
            }
            if hints.iter().any(|h| h == "NOWAIT") {
                self.ses.nowait = true;
            }
        }
        Ok(())
    }

    /// Wait for the transactions in `ses.blocked_on` to finish. Errors on deadlock or timeout.
    pub fn wait_blocked(&mut self) -> Result<()> {
        let blockers = std::mem::take(&mut self.ses.blocked_on);
        let me = self.cur_tx();
        let timeout = self.ses.lock_timeout;
        if timeout == 0 || self.ses.nowait {
            if me != 0 {
                self.srv.core.locks.cancel_pending(me);
            }
            return Err(SqlError::new(1222, "Lock request time out period exceeded."));
        }
        if me != 0 {
            if self.srv.core.locks.wait_and_check(me, blockers.clone()) {
                let spid = self.ses.spid;
                self.rollback_all();
                let mut e = SqlError::with(1205, 13, 51, format!("Transaction (Process ID {}) was deadlocked on lock resources with another process and has been chosen as the deadlock victim. Rerun the transaction.", spid));
                e.abort_batch = true;
                return Err(e);
            }
        }
        let started = Instant::now();
        let dur = if timeout > 0 { Some(Duration::from_millis(timeout as u64)) } else { None };
        let core = self.srv.core.clone();
        let spid = self.ses.spid;
        let timed_out = self.srv.unlocked(|| core.wait_for_end(&blockers, dur, spid));
        if self.srv.core.is_killed(spid) {
            return Err(self.kill_error());
        }
        if me != 0 {
            self.srv.core.locks.clear_wait(me);
        }
        if timed_out {
            if me != 0 {
                self.srv.core.locks.cancel_pending(me);
            }
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
