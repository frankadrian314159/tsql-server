//! `BACKUP DATABASE`, `BACKUP LOG`, `RESTORE DATABASE`, `RESTORE LOG` (with `STOPAT`), `RESTORE VERIFYONLY` and
//! `RESTORE HEADERONLY`.
//!
//! A **full backup** is a logical copy of one database taken from a snapshot: the catalog (tables, views, procedures,
//! sequences, permissions, ...) and every committed row as of the moment the backup started, written as checksummed
//! frames. Other sessions keep working while it runs; it holds the catalog shared (so no DDL can start) and registers as
//! a snapshot so old row versions it needs are kept. A backup waits for transactions that have changed the catalog
//! and not yet finished, so the catalog it copies is a committed one.
//!
//! A **log backup** copies the committed log records of one database written since the previous backup (the *log
//! chain*, which a full backup in the FULL recovery model starts). The server keeps log segments at checkpoints for
//! every database whose chain has started, until they have been backed up.
//!
//! **Restore** rebuilds the database from the full backup (`NORECOVERY` leaves it in the *restoring* state so log backups
//! can be applied in order; `RECOVERY`, the default, makes it usable). `RESTORE LOG ... WITH STOPAT = '<time>'` stops
//! at the last commit written at or before that time (server local time): point-in-time recovery. Every restore step is
//! made durable with a checkpoint.
use super::query::Env;
use super::*;
use crate::persist::{apply_record, LogOp, WalRecord};
use crate::storage::tx::Undo;
use crate::storage::{ReadView, ViewMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};

const MAGIC: &[u8; 8] = b"TSQLBAK1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupHeader {
    pub log: bool,
    /// Database name as written when the backup was taken (display form) and its lower-case key.
    pub db: String,
    pub db_key: String,
    pub server: String,
    /// Wall-clock time the backup started, microseconds since the Unix epoch.
    pub wall: i64,
    /// Full: the commit timestamp the snapshot was taken at. Log: the newest commit timestamp the backup covers.
    pub ts: u64,
    /// Log: the commit timestamp the chain had reached before this backup (this backup holds everything after it).
    pub after_ts: u64,
    /// Differential backup: the commit timestamp of the full backup it is based on.
    pub diff_base: Option<u64>,
    pub recovery_full: bool,
    pub copy_only: bool,
    pub name: String,
    pub description: String,
}

#[derive(Serialize, Deserialize)]
enum Frame {
    Header(BackupHeader),
    Catalog(Box<Database>),
    Rows { key: String, rows: Vec<(usize, Row)> },
    /// Differential backups: the ids of every row the table has (rows not listed were deleted since the base).
    Present { key: String, ids: Vec<usize> },
    Log(Box<WalRecord>),
    End { frames: u64, tables: u64, rows: u64 },
}

/// True when the file is a backup written by SQL Server (an `MSSQLBAK` file or a Microsoft Tape Format one).
fn is_sqlserver_backup(path: &str) -> bool {
    let mut head = [0u8; 8];
    File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && (crate::mssqlbak::is_bak(&head) || crate::mtf::is_mtf(&head))
}

fn io_fail(path: &str, what: &str, e: impl std::fmt::Display) -> SqlError {
    SqlError::with(3201, 16, 1, format!("Cannot {} backup device '{}'. Operating system error: {}.", what, path, e))
}

fn bad_media(path: &str, why: &str) -> SqlError {
    SqlError::with(3241, 16, 1, format!("The media family on device '{}' is incorrectly formed: {}. SQL Server cannot process this media family.", path, why))
}

struct FrameWriter {
    w: BufWriter<File>,
    frames: u64,
    path: String,
}

impl FrameWriter {
    fn create(path: &str) -> Result<FrameWriter> {
        let f = File::create(path).map_err(|e| io_fail(path, "open", e))?;
        let mut w = BufWriter::with_capacity(256 << 10, f);
        w.write_all(MAGIC).map_err(|e| io_fail(path, "write to", e))?;
        Ok(FrameWriter { w, frames: 0, path: path.to_string() })
    }

    fn put(&mut self, f: &Frame) -> Result<()> {
        let bytes = postcard::to_allocvec(f).map_err(|e| SqlError::with(3013, 16, 1, format!("BACKUP is terminating abnormally: {}.", e)))?;
        let wr = |w: &mut BufWriter<File>, b: &[u8]| w.write_all(b);
        wr(&mut self.w, &(bytes.len() as u32).to_le_bytes()).and_then(|_| wr(&mut self.w, &crc32fast::hash(&bytes).to_le_bytes())).and_then(|_| wr(&mut self.w, &bytes)).map_err(|e| io_fail(&self.path, "write to", e))?;
        self.frames += 1;
        Ok(())
    }

    fn finish(mut self, tables: u64, rows: u64) -> Result<()> {
        let frames = self.frames;
        self.put(&Frame::End { frames, tables, rows })?;
        self.w.flush().map_err(|e| io_fail(&self.path, "write to", e))?;
        self.w.get_ref().sync_all().map_err(|e| io_fail(&self.path, "write to", e))
    }
}

struct FrameReader {
    r: BufReader<File>,
    path: String,
}

impl FrameReader {
    fn open(path: &str) -> Result<FrameReader> {
        let f = File::open(path).map_err(|e| io_fail(path, "open", e))?;
        let mut r = BufReader::with_capacity(256 << 10, f);
        let mut magic = [0u8; 8];
        let read = r.read_exact(&mut magic).is_ok();
        if read && (crate::mtf::is_mtf(&magic) || crate::mssqlbak::is_bak(&magic)) {
            return Err(bad_media(path, "it is a SQL Server backup, which this server cannot restore yet; tsql-bakinfo describes it"));
        }
        if !read || &magic != MAGIC {
            return Err(bad_media(path, "it is not a backup written by this server"));
        }
        Ok(FrameReader { r, path: path.to_string() })
    }

    fn next(&mut self) -> Result<Frame> {
        let mut head = [0u8; 8];
        self.r.read_exact(&mut head).map_err(|_| bad_media(&self.path, "the backup ends before its trailer (it is incomplete)"))?;
        let len = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(head[4..].try_into().unwrap());
        let mut buf = vec![0u8; len];
        self.r.read_exact(&mut buf).map_err(|_| bad_media(&self.path, "the backup is truncated"))?;
        if crc32fast::hash(&buf) != crc {
            return Err(SqlError::with(3043, 16, 1, format!("BACKUP '{}' detected an error on a block: the checksum does not match; the backup is damaged.", self.path)));
        }
        postcard::from_bytes(&buf).map_err(|e| bad_media(&self.path, &format!("a frame cannot be decoded ({})", e)))
    }
}

/// Read the whole backup once, checking every frame, and return its header and the totals of its trailer.
fn verify(path: &str) -> Result<(BackupHeader, u64, u64, u64)> {
    let mut rd = FrameReader::open(path)?;
    let Frame::Header(h) = rd.next()? else { return Err(bad_media(path, "the header is missing")) };
    let mut n = 1u64;
    loop {
        match rd.next()? {
            Frame::End { frames, tables, rows } => {
                if frames != n {
                    return Err(bad_media(path, "a frame is missing"));
                }
                return Ok((h, frames, tables, rows));
            }
            Frame::Header(_) => return Err(bad_media(path, "a second header was found")),
            _ => n += 1,
        }
    }
}

/// Ends the snapshot a backup reads from.
struct Snapshot(std::sync::Arc<crate::coord::Core>, crate::storage::TxId);

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.0.end(self.1);
    }
}

fn op_db(op: &LogOp) -> Option<&str> {
    match op {
        LogOp::Put { db, .. }
        | LogOp::TableReset { db, .. }
        | LogOp::SetIdentity { db, .. }
        | LogOp::PutObject { db, .. }
        | LogOp::DropObject { db, .. }
        | LogOp::PutType { db, .. }
        | LogOp::DropType { db, .. }
        | LogOp::PutSchema { db, .. }
        | LogOp::DropSchema { db, .. }
        | LogOp::SeqSet { db, .. }
        | LogOp::DbOptions { db, .. }
        | LogOp::DbRecovery { db, .. }
        | LogOp::DbPolicies { db, .. }
        | LogOp::DbFtCatalogs { db, .. }
        | LogOp::DbXmlSchemas { db, .. }
        | LogOp::DbDdlTriggers { db, .. }
        | LogOp::DbAuditSpecs { db, .. }
        | LogOp::DbSecurity { db, .. } => Some(db),
        _ => None,
    }
}

fn retarget(op: &mut LogOp, to: &str) {
    match op {
        LogOp::Put { db, .. }
        | LogOp::TableReset { db, .. }
        | LogOp::SetIdentity { db, .. }
        | LogOp::PutObject { db, .. }
        | LogOp::DropObject { db, .. }
        | LogOp::PutType { db, .. }
        | LogOp::DropType { db, .. }
        | LogOp::PutSchema { db, .. }
        | LogOp::DropSchema { db, .. }
        | LogOp::SeqSet { db, .. }
        | LogOp::DbOptions { db, .. }
        | LogOp::DbRecovery { db, .. }
        | LogOp::DbPolicies { db, .. }
        | LogOp::DbFtCatalogs { db, .. }
        | LogOp::DbXmlSchemas { db, .. }
        | LogOp::DbDdlTriggers { db, .. }
        | LogOp::DbAuditSpecs { db, .. }
        | LogOp::DbSecurity { db, .. } => *db = to.to_string(),
        _ => {}
    }
}

fn set_object_id(o: &mut Object, id: i32) {
    match o {
        Object::Table(t) => t.object_id = id,
        Object::View(v) => v.object_id = id,
        Object::Proc(p) => p.object_id = id,
        Object::Func(f) => f.object_id = id,
        Object::Seq(s) => s.object_id = id,
        Object::Synonym(s) => s.object_id = id,
    }
}

/// A signature of a table's column definitions (what a differential backup compares to decide whether a table's rows
/// must all be included).
fn table_sig(t: &Table) -> u64 {
    let bytes = postcard::to_allocvec(&t.columns).unwrap_or_default();
    crc32fast::hash(&bytes) as u64 | ((bytes.len() as u64) << 32)
}

fn micros_to_datetime(us: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_micros(us).map(|d| d.with_timezone(&chrono::Local).naive_local()).unwrap_or_default()
}

impl<'a> Engine<'a> {
    fn backup_path(&mut self, e: &Expr) -> Result<String> {
        let v = self.ev(e, &Env::empty())?;
        let p = convert::to_str(&v);
        if v.is_null() || p.trim().is_empty() {
            return Err(SqlError::with(3201, 16, 1, "Cannot open backup device ''. The device name must not be empty or NULL."));
        }
        Ok(p)
    }

    /// Checks common to every BACKUP/RESTORE statement.
    fn backup_preflight(&mut self, what: &str) -> Result<()> {
        if self.ses.trancount > 0 {
            return Err(SqlError::with(574, 16, 1, format!("{} cannot be used inside a user transaction.", what)));
        }
        Ok(())
    }

    fn check_backup_options(&mut self, opts: &[(String, Option<Expr>)], what: &str) -> Result<()> {
        const IGNORED: &[&str] = &[
            "INIT", "NOINIT", "FORMAT", "NOFORMAT", "SKIP", "NOSKIP", "CHECKSUM", "NO_CHECKSUM", "STATS", "COMPRESSION", "NO_COMPRESSION", "STOP_ON_ERROR", "CONTINUE_AFTER_ERROR", "MEDIANAME",
            "MEDIADESCRIPTION", "EXPIREDATE", "RETAINDAYS", "BLOCKSIZE", "BUFFERCOUNT", "MAXTRANSFERSIZE", "UNLOAD", "NOUNLOAD", "REWIND", "NOREWIND", "FILE", "MOVE", "NAME", "DESCRIPTION", "COPY_ONLY",
            "REPLACE", "RECOVERY", "NORECOVERY", "STOPAT", "DIFFERENTIAL",
        ];
        for (n, _) in opts {
            if n == "STANDBY" || n == "ENCRYPTION" || n == "STOPATMARK" || n == "STOPBEFOREMARK" {
                return Err(SqlError::with(155, 15, 1, format!("The option {} of {} is not supported by this server.", n, what)));
            }
            if !IGNORED.contains(&n.as_str()) {
                return Err(SqlError::with(155, 15, 1, format!("'{}' is not a recognized {} option.", n, what)));
            }
        }
        Ok(())
    }

    fn opt_text(&mut self, opts: &[(String, Option<Expr>)], name: &str) -> Result<String> {
        for (n, v) in opts {
            if n == name {
                if let Some(e) = v {
                    let v = self.ev(e, &Env::empty())?;
                    return Ok(convert::to_str(&v));
                }
            }
        }
        Ok(String::new())
    }

    fn can_back_up(&self, dbkey: &str) -> bool {
        if self.is_sysadmin() {
            return true;
        }
        match (self.db_user(dbkey), self.srv.databases.get(dbkey)) {
            (Some(u), Some(d)) => d.security.is_member(&u, "db_owner") || d.security.is_member(&u, "db_backupoperator"),
            _ => false,
        }
    }

    pub fn exec_backup(&mut self, b: &BackupStmt) -> Result<()> {
        let what = if b.log { "BACKUP LOG" } else { "BACKUP DATABASE" };
        self.backup_preflight(what)?;
        self.check_backup_options(&b.opts, "BACKUP")?;
        let path = self.backup_path(&b.path)?;
        let dbkey = b.db.to_lowercase();
        if !self.srv.databases.contains_key(&dbkey) {
            return Err(SqlError::with(911, 16, 1, format!("Database '{}' does not exist. Make sure that the name is entered correctly.", b.db)));
        }
        if dbkey == "tempdb" {
            return Err(SqlError::with(3147, 16, 1, "Backup and restore operations are not allowed on database tempdb."));
        }
        if !self.can_back_up(&dbkey) {
            return Err(SqlError::with(262, 14, 1, format!("{} permission denied in database '{}'.", what, b.db)));
        }
        if self.srv.databases[&dbkey].restoring {
            return Err(SqlError::with(927, 14, 1, format!("Database '{}' cannot be opened. It is in the middle of a restore.", b.db)));
        }
        let copy_only = b.opts.iter().any(|(n, _)| n == "COPY_ONLY");
        let diff = b.opts.iter().any(|(n, _)| n == "DIFFERENTIAL");
        if diff && b.log {
            return Err(SqlError::with(155, 15, 1, "The option DIFFERENTIAL is not valid for BACKUP LOG."));
        }
        if diff && copy_only {
            return Err(SqlError::with(155, 15, 1, "COPY_ONLY cannot be combined with DIFFERENTIAL."));
        }
        let name = self.opt_text(&b.opts, "NAME")?;
        let description = self.opt_text(&b.opts, "DESCRIPTION")?;
        let started = std::time::Instant::now();
        let tmp = format!("{}.tmp", path);
        let result = if b.log { self.backup_log(&dbkey, &tmp, name, description) } else { self.backup_full(&dbkey, &tmp, copy_only, diff, name, description) };
        let (records, tables) = match result {
            Ok(x) => x,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        std::fs::rename(&tmp, &path).map_err(|e| io_fail(&path, "write to", e))?;
        let secs = started.elapsed().as_secs_f64().max(0.001);
        if b.log {
            self.emit(Event::Message(format!("Processed {} log record(s) for database '{}', file '{}'.", records, b.db, path)));
        } else {
            self.emit(Event::Message(format!("Processed {} row(s) in {} table(s) for database '{}', file '{}'.", records, tables, b.db, path)));
        }
        self.emit(Event::Message(format!("{} successfully processed {} {} in {:.3} seconds.", what, records, if b.log { "log records" } else { "rows" }, secs)));
        Ok(())
    }

    /// Returns (rows written, tables written).
    fn backup_full(&mut self, dbkey: &str, tmp: &str, copy_only: bool, diff: bool, name: String, description: String) -> Result<(u64, u64)> {
        let diff_base = if diff {
            match self.srv.databases[dbkey].last_full_ts {
                Some(t) => Some(t),
                None => return Err(SqlError::with(3035, 16, 1, format!("Cannot perform a differential backup for database \"{}\", because a current database backup does not exist. Perform a full database backup by reissuing BACKUP DATABASE, omitting the WITH DIFFERENTIAL option.", dbkey))),
            }
        } else {
            None
        };
        // wait for transactions that have changed the catalog and not finished: the catalog copied must be a committed one
        let pending = self.srv.core.ddl_txs(self.cur_tx());
        if !pending.is_empty() {
            return Err(self.blocked(pending));
        }
        let core = self.srv.core.clone();
        let (snap_id, start) = core.begin(true);
        let _snap = Snapshot(core.clone(), snap_id);
        let view = ReadView { tx: snap_id, mode: ViewMode::Snapshot(start) };
        let db: Database = self.srv.databases[dbkey].clone();
        let header = BackupHeader { log: false, db: db.name.clone(), db_key: dbkey.to_string(), server: self.srv.server_name.clone(), wall: chrono::Utc::now().timestamp_micros(), ts: start, after_ts: 0, diff_base, recovery_full: db.recovery_full, copy_only, name, description };
        let mut w = FrameWriter::create(tmp)?;
        w.put(&Frame::Header(header))?;
        w.put(&Frame::Catalog(Box::new(db.clone())))?;
        let (mut tables, mut rows) = (0u64, 0u64);
        for (key, obj) in &db.objects {
            let Object::Table(t) = obj else { continue };
            tables += 1;
            if let Some(base) = diff_base {
                // a differential: which rows exist, and every row changed since the base (all of them if the table's
                // definition changed)
                let changed_def = db.backup_sigs.get(key) != Some(&table_sig(t));
                let mut after: Option<usize> = None;
                loop {
                    let slots = t.data.slot_chunk(after, 512);
                    let (mut ids, mut changed) = (vec![], vec![]);
                    for (rid, slot) in &slots {
                        if let Some(row) = slot.visible(&view) {
                            ids.push(*rid);
                            if changed_def || slot.latest_committed_ts() > base {
                                changed.push((*rid, row.clone()));
                            }
                        }
                    }
                    if !ids.is_empty() {
                        w.put(&Frame::Present { key: key.clone(), ids })?;
                    }
                    if !changed.is_empty() {
                        rows += changed.len() as u64;
                        w.put(&Frame::Rows { key: key.clone(), rows: changed })?;
                    }
                    match slots.last() {
                        Some((last, _)) if slots.len() == 512 => after = Some(*last),
                        _ => break,
                    }
                }
                continue;
            }
            let mut after = None;
            loop {
                let chunk = t.data.scan_chunk(&view, after, 512, false);
                if !chunk.rows.is_empty() {
                    rows += chunk.rows.len() as u64;
                    w.put(&Frame::Rows { key: key.clone(), rows: chunk.rows })?;
                }
                if chunk.done || chunk.last.is_none() {
                    break;
                }
                after = chunk.last;
            }
        }
        w.finish(tables, rows)?;
        drop(_snap);
        // a full backup is the base of later differentials
        if !copy_only && !diff {
            let sigs: BTreeMap<String, u64> = db.objects.iter().filter_map(|(k, o)| if let Object::Table(t) = o { Some((k.clone(), table_sig(t))) } else { None }).collect();
            let d = self.srv.cat_mut().databases.get_mut(dbkey).unwrap();
            d.last_full_ts = Some(start);
            d.backup_sigs = sigs.clone();
            self.log_nowait(vec![LogOp::DbBackupBase { db: dbkey.to_string(), ts: Some(start), sigs }]);
        }
        // a full backup in the FULL recovery model starts (or, unless COPY_ONLY, restarts) the log chain
        if !copy_only && !diff {
            let d = self.srv.cat_mut().databases.get_mut(dbkey).unwrap();
            d.log_chain_ts = if d.recovery_full { Some(start) } else { None };
            let ts = d.log_chain_ts;
            self.log_nowait(vec![LogOp::DbChain { db: dbkey.to_string(), ts }]);
        }
        Ok((rows, tables))
    }

    /// Returns (records written, 0).
    fn backup_log(&mut self, dbkey: &str, tmp: &str, name: String, description: String) -> Result<(u64, u64)> {
        let d = &self.srv.databases[dbkey];
        if !d.recovery_full {
            return Err(SqlError::with(4208, 16, 1, "The statement BACKUP LOG is not allowed while the recovery model is SIMPLE. Use BACKUP DATABASE or change the recovery model using ALTER DATABASE."));
        }
        let Some(chain) = d.log_chain_ts else {
            return Err(SqlError::with(4214, 16, 1, "BACKUP LOG cannot be performed because there is no current database backup."));
        };
        let display = d.name.clone();
        let all = {
            let p = self.srv.persist();
            match p.as_ref() {
                Some(p) => p.log_records_after(chain)?,
                None => return Err(SqlError::with(3041, 16, 1, "BACKUP failed to complete the command BACKUP LOG. This server keeps no transaction log (it runs in memory).")),
            }
        };
        let last_ts = all.iter().map(|r| r.ts).max().unwrap_or(chain);
        let mut w = FrameWriter::create(tmp)?;
        let header = BackupHeader { log: true, db: display, db_key: dbkey.to_string(), server: self.srv.server_name.clone(), wall: chrono::Utc::now().timestamp_micros(), ts: last_ts, after_ts: chain, diff_base: None, recovery_full: true, copy_only: false, name, description };
        w.put(&Frame::Header(header))?;
        let mut n = 0u64;
        for mut rec in all {
            rec.ops.retain(|op| op_db(op) == Some(dbkey));
            if rec.ops.is_empty() {
                continue;
            }
            n += 1;
            w.put(&Frame::Log(Box::new(rec)))?;
        }
        w.finish(0, n)?;
        let srv = self.srv.cat_mut();
        srv.databases.get_mut(dbkey).unwrap().log_chain_ts = Some(last_ts);
        // segments every chain has moved past are no longer needed
        let floor = srv.databases.values().filter(|d| d.recovery_full).filter_map(|d| d.log_chain_ts).min().unwrap_or(last_ts);
        if let Some(p) = srv.persist().as_ref() {
            p.prune_archive(floor);
        }
        self.log_nowait(vec![LogOp::DbChain { db: dbkey.to_string(), ts: Some(last_ts) }]);
        Ok((n, 0))
    }

    pub fn exec_restore(&mut self, r: &RestoreStmt) -> Result<()> {
        self.check_backup_options(&r.opts, "RESTORE")?;
        let path = self.backup_path(&r.path)?;
        if is_sqlserver_backup(&path) {
            return self.restore_sqlserver(r, &path);
        }
        match r.kind {
            RestoreKind::VerifyOnly => {
                let (_, frames, tables, rows) = verify(&path)?;
                self.emit(Event::Message(format!("The backup set on file 1 is valid. ({} frames, {} table(s), {} row(s) or log record(s))", frames, tables, rows)));
                Ok(())
            }
            RestoreKind::HeaderOnly => {
                let (h, ..) = verify(&path)?;
                let when = micros_to_datetime(h.wall);
                let dt = |t: chrono::NaiveDateTime| Value::DateTime(t, crate::types::DtKind::DateTime);
                let cols = ["BackupName", "BackupDescription", "BackupType", "ServerName", "DatabaseName", "BackupStartDate", "BackupFinishDate", "FirstLSN", "LastLSN", "RecoveryModel", "IsCopyOnly"];
                let types = vec![DataType::NVarChar(128), DataType::NVarChar(255), DataType::TinyInt, DataType::NVarChar(128), DataType::NVarChar(128), DataType::DateTime, DataType::DateTime, DataType::BigInt, DataType::BigInt, DataType::NVarChar(60), DataType::Bit];
                let row = vec![
                    Value::Str(h.name.clone()),
                    Value::Str(h.description.clone()),
                    Value::Int(if h.log { 2 } else if h.diff_base.is_some() { 5 } else { 1 }),
                    Value::Str(h.server.clone()),
                    Value::Str(h.db.clone()),
                    dt(when),
                    dt(when),
                    Value::BigInt(h.after_ts as i64),
                    Value::BigInt(h.ts as i64),
                    Value::Str(if h.recovery_full { "FULL".into() } else { "SIMPLE".into() }),
                    Value::Bit(h.copy_only),
                ];
                self.emit(Event::Rows(ResultSet { columns: cols.iter().map(|c| c.to_string()).collect(), types, rows: vec![row] }));
                if !self.ses.nocount {
                    self.emit(Event::RowCount(1));
                }
                self.ses.rowcount = 1;
                Ok(())
            }
            RestoreKind::Database => {
                let (h, ..) = verify(&path)?;
                if h.diff_base.is_some() {
                    self.restore_diff(r, &path, &h)
                } else {
                    self.restore_full(r, &path)
                }
            }
            RestoreKind::Log => self.restore_log(r, &path),
        }
    }

    /// A backup written by SQL Server: read its tables, convert them to a backup of this server's format in a temporary
    /// file, and restore that (see `bakimport`).
    fn restore_sqlserver(&mut self, r: &RestoreStmt, path: &str) -> Result<()> {
        let unreadable = |why: String| SqlError::with(3241, 16, 1, format!("The media family on device '{}' is incorrectly formed: {}. SQL Server cannot process this media family.", path, why));
        match r.kind {
            RestoreKind::Log => Err(SqlError::with(3154, 16, 1, "Log backups of SQL Server cannot be restored by this server; only full database backups can.")),
            RestoreKind::VerifyOnly | RestoreKind::HeaderOnly => {
                let f = File::open(path).map_err(|e| io_fail(path, "open", e))?;
                let mut ex = match crate::mssqlbak::Expanded::open(BufReader::new(f)) {
                    Ok(Ok(ex)) => ex,
                    Ok(Err(why)) => return Err(unreadable(why)),
                    Err(e) => return Err(io_fail(path, "read", e)),
                };
                // every chunk must expand: this checks the whole file
                for i in 0..ex.layout.chunks.len() {
                    ex.chunk(i).map_err(|e| unreadable(e.to_string()))?;
                }
                let scan = crate::mtf::scan(&mut ex).map_err(|e| io_fail(path, "read", e))?;
                if let Some(p) = scan.problem {
                    return Err(unreadable(p));
                }
                let set = scan.blocks.iter().find(|b| &b.kind == b"SSET").ok_or_else(|| unreadable("it has no backup set".to_string()))?;
                if r.kind == RestoreKind::VerifyOnly {
                    self.emit(Event::Message(format!("The backup set on file 1 is valid. ({} chunk(s), {} byte(s) expanded)", ex.layout.chunks.len(), ex.layout.total)));
                    return Ok(());
                }
                let db = crate::sqlbak::Backup::open(ex).map(|b| b.db_name.clone()).unwrap_or_default();
                let when = set.date_at(88).and_then(|d| chrono::NaiveDateTime::parse_from_str(&d, "%Y-%m-%d %H:%M:%S").ok()).unwrap_or_default();
                let dt = |t: chrono::NaiveDateTime| Value::DateTime(t, crate::types::DtKind::DateTime);
                let cols = ["BackupName", "BackupDescription", "BackupType", "UserName", "DatabaseName", "BackupStartDate", "SoftwareVersionMajor"];
                let types = vec![DataType::NVarChar(128), DataType::NVarChar(255), DataType::TinyInt, DataType::NVarChar(128), DataType::NVarChar(128), DataType::DateTime, DataType::Int];
                let row = vec![Value::Str(set.string_at(64)), Value::Str(set.string_at(68)), Value::Int(1), Value::Str(set.string_at(76)), Value::Str(db), dt(when), Value::Int(set.u8_at(93) as i32)];
                self.emit(Event::Rows(ResultSet { columns: cols.iter().map(|c| c.to_string()).collect(), types, rows: vec![row] }));
                if !self.ses.nocount {
                    self.emit(Event::RowCount(1));
                }
                self.ses.rowcount = 1;
                Ok(())
            }
            RestoreKind::Database => {
                self.backup_preflight("RESTORE DATABASE")?;
                if r.db.as_deref().unwrap_or("").is_empty() {
                    return Err(SqlError::with(102, 15, 1, "Incorrect syntax near 'FROM': RESTORE DATABASE needs a database name."));
                }
                let tmp = std::env::temp_dir().join(format!("tsql-import-{}-{}.tbak", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
                let tmp_path = tmp.to_string_lossy().to_string();
                let started = std::time::Instant::now();
                let report = super::bakimport::convert(path, &tmp_path).map_err(|why| {
                    let _ = std::fs::remove_file(&tmp);
                    unreadable(why)
                })?;
                self.emit(Event::Message(format!(
                    "Read the SQL Server backup of database '{}': {} table(s), {} row(s) in {:.1} seconds.",
                    report.database,
                    report.tables,
                    report.rows,
                    started.elapsed().as_secs_f64()
                )));
                for (table, col, n) in &report.skipped {
                    self.emit(Event::Message(format!("Warning: {} value(s) of {}.{} are stored in a form that is not read yet (large objects, XML) and were set to NULL.", n, table, col)));
                }
                self.emit(Event::Message(format!(
                    "Created {} primary/unique key(s), {} index(es), {} XML index(es), {} spatial index(es), {} full-text catalog(s), {} full-text index(es), {} foreign key(s), {} default(s) and {} check constraint(s).",
                    report.keys, report.indexes, report.xml_indexes, report.spatial_indexes, report.fulltext_catalogs, report.fulltext_indexes, report.foreign_keys, report.defaults, report.checks
                )));
                self.emit(Event::Message(format!(
                    "Created {} view(s), {} procedure(s), {} function(s) and {} trigger(s).",
                    report.views, report.procedures, report.functions, report.triggers
                )));
                for p in report.problems.iter().take(30) {
                    self.emit(Event::Message(format!("Warning: {}.", p)));
                }
                if report.problems.len() > 30 {
                    self.emit(Event::Message(format!("Warning: {} more problem(s) like these.", report.problems.len() - 30)));
                }
                if !report.computed.is_empty() {
                    let list: Vec<String> = report.computed.iter().map(|(t, c)| format!("{}.{}", t, c)).collect();
                    self.emit(Event::Message(format!("Warning: computed column(s) were not created: {}.", list.join(", "))));
                }
                let out = self.restore_full(r, &tmp_path);
                let _ = std::fs::remove_file(&tmp);
                out
            }
        }
    }

    fn restore_recovery(&self, r: &RestoreStmt) -> bool {
        // RECOVERY is the default; NORECOVERY leaves the database restoring
        !r.opts.iter().any(|(n, _)| n == "NORECOVERY")
    }

    fn restore_full(&mut self, r: &RestoreStmt, path: &str) -> Result<()> {
        self.backup_preflight("RESTORE DATABASE")?;
        let target = r.db.clone().unwrap_or_default();
        let key = target.to_lowercase();
        if matches!(key.as_str(), "master" | "tempdb" | "model" | "msdb") {
            return Err(SqlError::with(3147, 16, 1, format!("Backup and restore operations are not allowed on database {}.", key)));
        }
        let exists = self.srv.databases.contains_key(&key);
        if !self.is_sysadmin() && !self.srv.security.dbcreators.contains(&self.eff_login().to_lowercase()) {
            return Err(SqlError::with(262, 14, 1, format!("RESTORE DATABASE permission denied in database '{}'.", target)));
        }
        if exists && !r.opts.iter().any(|(n, _)| n == "REPLACE") {
            return Err(SqlError::with(3159, 16, 1, format!("The tail of the log for the database \"{}\" has not been backed up. Use BACKUP LOG WITH NORECOVERY to back up the log if it contains work you do not want to lose. Use the WITH REPLACE or WITH STOPAT clause of the RESTORE statement to just overwrite the contents of the log.", target)));
        }
        // replacing a database that other sessions may be using needs them to be gone
        if exists && !self.srv.core.others_active(self.cur_tx()).is_empty() {
            return Err(SqlError::with(3101, 16, 1, format!("Exclusive access could not be obtained because the database '{}' is in use.", target)));
        }
        // check the whole file before anything is replaced
        let (header, ..) = verify(path)?;
        if header.log {
            return Err(SqlError::with(3154, 16, 1, "The backup set holds a log backup; use RESTORE LOG. A full backup is needed to restore a database."));
        }
        let started = std::time::Instant::now();
        let recovery = self.restore_recovery(r);
        let now_ts = self.srv.core.clock();
        let renamed = !header.db.eq_ignore_ascii_case(&target);
        // read the catalog frame, then the rows into fresh storage
        let mut rd = FrameReader::open(path)?;
        let Frame::Header(_) = rd.next()? else { return Err(bad_media(path, "the header is missing")) };
        let Frame::Catalog(db) = rd.next()? else { return Err(bad_media(path, "the catalog is missing")) };
        let mut db: Database = *db;
        let store = self.srv.store.clone();
        db.name = target.clone();
        db.id = self.srv.databases.get(&key).map(|d| d.id).unwrap_or_else(|| self.srv.databases.values().map(|d| d.id).max().unwrap_or(4) + 1);
        db.restoring = !recovery;
        db.applied_ts = header.ts;
        db.log_chain_ts = None;
        let mut trigger_fix = false;
        for obj in db.objects.values_mut() {
            let id = self.srv.core.new_object_id();
            set_object_id(obj, id);
            if let Object::Table(t) = obj {
                if renamed {
                    t.db = target.clone();
                    for fk in t.fks.iter_mut() {
                        if fk.ref_db.eq_ignore_ascii_case(&header.db) {
                            fk.ref_db = target.clone();
                        }
                    }
                }
                for trg in t.triggers.iter_mut() {
                    let mut x = (**trg).clone();
                    x.object_id = self.srv.core.new_object_id();
                    *trg = std::sync::Arc::new(x);
                    trigger_fix = true;
                }
                t.data = std::sync::Arc::new(crate::storage::TableData::new(&store));
                t.sync_indexes();
            }
        }
        if trigger_fix {
            db.rebuild_triggers();
        }
        let mut rows = 0u64;
        let tables;
        loop {
            match rd.next()? {
                Frame::Rows { key: tk, rows: batch } => {
                    if let Some(Object::Table(t)) = db.objects.get(&tk) {
                        for (rid, row) in batch {
                            t.data.replace_slot(rid, Some(row), now_ts);
                            rows += 1;
                        }
                    }
                }
                Frame::End { tables: t, .. } => {
                    tables = t;
                    break;
                }
                _ => {}
            }
        }
        // The restored database becomes part of this statement's transaction: committing it writes the whole database
        // (catalog and rows) to the redo log, so it is durable like any other change, and a failure undoes it.
        self.ensure_tx()?;
        let prev = self.srv.databases.get(&key).cloned();
        self.push_undo(Undo::Database { key: key.clone(), prev });
        self.srv.cat_mut().databases.insert(key.clone(), db);
        self.emit(Event::Message(format!("Processed {} row(s) in {} table(s) for database '{}', file '{}'.", rows, tables, target, path)));
        self.emit(Event::Message(format!(
            "RESTORE DATABASE successfully processed {} rows in {:.3} seconds.{}",
            rows,
            started.elapsed().as_secs_f64().max(0.001),
            if recovery { "" } else { " The database is restoring: apply log backups with RESTORE LOG, then finish with RESTORE LOG ... WITH RECOVERY." }
        )));
        Ok(())
    }

    /// Apply a differential backup to a database that was restored from its base full backup with NORECOVERY.
    fn restore_diff(&mut self, r: &RestoreStmt, path: &str, header: &BackupHeader) -> Result<()> {
        self.backup_preflight("RESTORE DATABASE")?;
        let target = r.db.clone().unwrap_or_default();
        let key = target.to_lowercase();
        if !self.is_sysadmin() && !self.srv.security.dbcreators.contains(&self.eff_login().to_lowercase()) {
            return Err(SqlError::with(262, 14, 1, format!("RESTORE DATABASE permission denied in database '{}'.", target)));
        }
        let Some(cur) = self.srv.databases.get(&key) else {
            return Err(SqlError::with(3136, 16, 1, format!("This differential backup cannot be restored because the database '{}' has not been restored to the correct earlier state (restore the full backup it is based on WITH NORECOVERY first).", target)));
        };
        if !cur.restoring || Some(cur.applied_ts) != header.diff_base {
            return Err(SqlError::with(3136, 16, 1, format!("This differential backup cannot be restored because the database '{}' has not been restored to the correct earlier state (it is based on the full backup taken at {}, the database is at {}).", target, header.diff_base.unwrap_or(0), cur.applied_ts)));
        }
        let recovery = self.restore_recovery(r);
        let started = std::time::Instant::now();
        let now_ts = self.srv.core.clock();
        let renamed = !header.db.eq_ignore_ascii_case(&target);
        let mut rd = FrameReader::open(path)?;
        let Frame::Header(_) = rd.next()? else { return Err(bad_media(path, "the header is missing")) };
        let Frame::Catalog(newcat) = rd.next()? else { return Err(bad_media(path, "the catalog is missing")) };
        let mut db: Database = *newcat;
        let old = self.srv.databases[&key].clone();
        let store = self.srv.store.clone();
        db.name = target.clone();
        db.id = old.id;
        db.restoring = !recovery;
        db.applied_ts = header.ts;
        db.log_chain_ts = None;
        db.last_full_ts = None;
        // the tables of the differential's catalog: unchanged ones start from the rows the base restored
        let mut trigger_fix = false;
        for (k, obj) in db.objects.iter_mut() {
            let id = match old.objects.get(k) {
                Some(o) => o.object_id(),
                None => self.srv.core.new_object_id(),
            };
            set_object_id(obj, id);
            if let Object::Table(t) = obj {
                if renamed {
                    t.db = target.clone();
                    for fk in t.fks.iter_mut() {
                        if fk.ref_db.eq_ignore_ascii_case(&header.db) {
                            fk.ref_db = target.clone();
                        }
                    }
                }
                for trg in t.triggers.iter_mut() {
                    let mut x = (**trg).clone();
                    x.object_id = self.srv.core.new_object_id();
                    *trg = std::sync::Arc::new(x);
                    trigger_fix = true;
                }
                t.data = std::sync::Arc::new(crate::storage::TableData::new(&store));
                t.sync_indexes();
            }
        }
        if trigger_fix {
            db.rebuild_triggers();
        }
        let mut present: std::collections::HashMap<String, std::collections::HashSet<usize>> = Default::default();
        let mut started_tables: std::collections::HashSet<String> = Default::default();
        let view = ReadView::committed(0);
        // copy the base's rows that still exist into a table the first time anything is written to it
        let seed = |db: &Database, key: &str, present: &std::collections::HashMap<String, std::collections::HashSet<usize>>, started: &mut std::collections::HashSet<String>| {
            if !started.insert(key.to_string()) {
                return;
            }
            if let (Some(Object::Table(new_t)), Some(Object::Table(old_t))) = (db.objects.get(key), old.objects.get(key)) {
                if let Some(ids) = present.get(key) {
                    for (rid, row) in old_t.data.iter(view) {
                        if ids.contains(&rid) && old_t.columns.len() == new_t.columns.len() {
                            new_t.data.replace_slot(rid, Some(row), now_ts);
                        }
                    }
                }
            }
        };
        let mut rows_applied = 0u64;
        loop {
            match rd.next()? {
                Frame::Present { key: tk, ids } => present.entry(tk).or_default().extend(ids),
                Frame::Rows { key: tk, rows: batch } => {
                    seed(&db, &tk, &present, &mut started_tables);
                    if let Some(Object::Table(t)) = db.objects.get(&tk) {
                        for (rid, row) in batch {
                            t.data.replace_slot(rid, Some(row), now_ts);
                            rows_applied += 1;
                        }
                    }
                }
                Frame::End { .. } => break,
                _ => {}
            }
        }
        // tables nothing was written to: unchanged since the base
        let keys: Vec<String> = db.objects.iter().filter(|(_, o)| matches!(o, Object::Table(_))).map(|(k, _)| k.clone()).collect();
        for k in keys {
            seed(&db, &k, &present, &mut started_tables);
        }
        self.ensure_tx()?;
        self.push_undo(Undo::Database { key: key.clone(), prev: Some(old) });
        self.srv.cat_mut().databases.insert(key.clone(), db);
        self.emit(Event::Message(format!("Processed {} changed row(s) for database '{}', file '{}'.", rows_applied, target, path)));
        self.emit(Event::Message(format!("RESTORE DATABASE (differential) successfully processed {} rows in {:.3} seconds.{}", rows_applied, started.elapsed().as_secs_f64().max(0.001), if recovery { "" } else { " The database is restoring: apply log backups with RESTORE LOG, then finish with RESTORE LOG ... WITH RECOVERY." })));
        Ok(())
    }

    fn restore_log(&mut self, r: &RestoreStmt, path: &str) -> Result<()> {
        self.backup_preflight("RESTORE LOG")?;
        let target = r.db.clone().unwrap_or_default();
        let key = target.to_lowercase();
        if !self.is_sysadmin() && !self.srv.security.dbcreators.contains(&self.eff_login().to_lowercase()) {
            return Err(SqlError::with(262, 14, 1, format!("RESTORE LOG permission denied in database '{}'.", target)));
        }
        let Some(d) = self.srv.databases.get(&key) else {
            return Err(SqlError::with(911, 16, 1, format!("Database '{}' does not exist. Make sure that the name is entered correctly.", target)));
        };
        if !d.restoring {
            return Err(SqlError::with(3117, 16, 1, format!("The log or differential backup cannot be restored because no files are ready to rollforward (database '{}' is not in the restoring state; restore a full backup WITH NORECOVERY first).", target)));
        }
        let applied = d.applied_ts;
        let (header, ..) = verify(path)?;
        if !header.log {
            return Err(SqlError::with(3154, 16, 1, "The backup set holds a full backup; use RESTORE DATABASE."));
        }
        if header.after_ts > applied {
            return Err(SqlError::with(4305, 16, 1, format!("The log in this backup set begins at LSN {}, which is too recent to apply to the database. An earlier log backup that includes LSN {} can be restored.", header.after_ts, applied)));
        }
        // STOPAT: the last commit written at or before this time (server local time)
        let mut stop_wall: Option<i64> = None;
        for (n, v) in &r.opts {
            if n == "STOPAT" {
                let Some(e) = v else { return Err(SqlError::with(155, 15, 1, "STOPAT needs a date and time.")) };
                let val = self.ev(e, &Env::empty())?;
                let text = convert::to_str(&val);
                let t = match convert::cast(&val, &DataType::DateTime, None) {
                    Ok(Value::DateTime(t, _)) => t,
                    _ => return Err(SqlError::with(8115, 16, 1, format!("Cannot convert '{}' to a date and time for STOPAT.", text))),
                };
                use chrono::TimeZone;
                let local = chrono::Local.from_local_datetime(&t).earliest().ok_or_else(|| SqlError::with(8115, 16, 1, "The STOPAT time does not exist in the local time zone."))?;
                stop_wall = Some(local.timestamp_micros());
            }
        }
        let recovery = self.restore_recovery(r);
        let renamed = !header.db_key.eq_ignore_ascii_case(&key);
        let mut rd = FrameReader::open(path)?;
        let Frame::Header(_) = rd.next()? else { return Err(bad_media(path, "the header is missing")) };
        let (mut applied_ts, mut count, mut stopped) = (applied, 0u64, false);
        self.ensure_tx()?;
        let prev = self.srv.databases.get(&key).cloned();
        self.push_undo(Undo::Database { key: key.clone(), prev });
        let srv = self.srv.cat_mut();
        loop {
            match rd.next()? {
                Frame::Log(mut rec) => {
                    if rec.ts <= applied_ts {
                        continue; // already in the database (an overlapping backup)
                    }
                    if stop_wall.is_some_and(|s| rec.wall > s) {
                        stopped = true;
                        break;
                    }
                    if renamed {
                        for op in rec.ops.iter_mut() {
                            retarget(op, &key);
                        }
                    }
                    apply_record(srv, &rec);
                    applied_ts = rec.ts;
                    count += 1;
                }
                Frame::End { .. } => break,
                _ => {}
            }
        }
        {
            let d = srv.databases.get_mut(&key).unwrap();
            d.applied_ts = applied_ts;
            d.restoring = !recovery;
        }
        self.emit(Event::Message(format!("Processed {} log record(s) for database '{}', file '{}'.{}", count, target, path, if stopped { " Stopped at the requested time (STOPAT)." } else { "" })));
        self.emit(Event::Message(format!("RESTORE LOG successfully processed {} log records.{}", count, if recovery { " The database is online." } else { " The database is still restoring." })));
        Ok(())
    }
}
