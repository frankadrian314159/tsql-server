//! Durability: a checksummed write-ahead log plus atomic checkpoint files.
//!
//! * `wal.log` holds one frame per committed transaction (`[len u32][crc32 u32][postcard payload]`).
//!   A transaction is durable once its frame is flushed to disk; a torn frame at the end of the
//!   log (a crash mid-write) is detected by its checksum and discarded.
//! * `snapshot.bin` is a checkpoint of all committed state. It is written to a temporary file,
//!   flushed and atomically renamed over the previous one, after which the log is truncated.
//! * `lock` is held with an OS file lock so only one process opens a data directory.
use crate::catalog::*;
use crate::security::Security;
use crate::storage::{PageStore, Row, StoreImage, TableData};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

const MAGIC: &[u8; 8] = b"TSQLSNP5";

/// One logged change. Applying a record's operations in order reproduces the commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LogOp {
    Put { db: String, key: String, rowid: usize, row: Option<Row> },
    /// Discard a table's rows (it is about to be reloaded by a series of `Put`s).
    TableReset { db: String, key: String },
    SetIdentity { db: String, key: String, next: Option<i64> },
    PutObject { db: String, key: String, obj: Object },
    DropObject { db: String, key: String },
    PutType { db: String, key: String, ty: UserType },
    DropType { db: String, key: String },
    PutSchema { db: String, key: String, name: String },
    DropSchema { db: String, key: String },
    PutDatabase { key: String, db: Database },
    DropDatabase { key: String },
    SeqSet { db: String, key: String, current: Option<i64> },
    DbOptions { db: String, allow_snapshot: bool, rcsi: bool },
    /// `ALTER DATABASE ... SET RECOVERY FULL | SIMPLE`
    DbRecovery { db: String, full: bool },
    /// The log chain of a database moved (a full or log backup was taken).
    DbChain { db: String, ts: Option<u64> },
    /// The base of differential backups moved (a full backup was taken).
    DbBackupBase { db: String, ts: Option<u64>, sigs: BTreeMap<String, u64> },
    /// The row-level security policies of a database.
    DbPolicies { db: String, policies: Vec<crate::catalog::SecurityPolicy> },
    /// The XML schema collections of a database.
    DbXmlSchemas { db: String, schemas: Vec<crate::catalog::XmlSchemaCollection> },
    /// The full-text catalogs of a database.
    DbFtCatalogs { db: String, catalogs: Vec<crate::catalog::FtCatalog> },
    /// The database-scoped DDL triggers of a database.
    DbDdlTriggers { db: String, triggers: Vec<crate::catalog::DdlTrigger> },
    /// The database audit specifications of a database.
    DbAuditSpecs { db: String, specs: Vec<crate::audit::AuditSpec> },
    Security(Security),
    DbSecurity { db: String, sec: crate::security::DbSecurity, owners: BTreeMap<String, String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalRecord {
    pub lsn: u64,
    pub ts: u64,
    /// When the record was written, in microseconds since the Unix epoch (for `RESTORE LOG ... STOPAT`).
    pub wall: i64,
    pub next_object_id: i32,
    pub next_rowversion: u64,
    pub ops: Vec<LogOp>,
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    lsn: u64,
    clock: u64,
    next_object_id: i32,
    next_rowversion: u64,
    databases: BTreeMap<String, Database>,
    security: Security,
    /// Page table of the data file at this checkpoint.
    store: StoreImage,
}

/// Tunables for a persistent server.
#[derive(Clone, Copy, Debug)]
pub struct StorageOptions {
    /// Pages of 8 KiB the buffer pool keeps in memory (for persistent tables).
    pub pool_pages: usize,
    /// Pages kept in memory for temporary tables and table variables before they spill to disk.
    pub temp_pool_pages: usize,
}

impl Default for StorageOptions {
    fn default() -> Self {
        StorageOptions { pool_pages: crate::storage::page::DEFAULT_POOL_PAGES, temp_pool_pages: 4096 }
    }
}

/// The records of a log file (`[u32 length][u32 crc][postcard record]` frames); a torn or corrupt tail is ignored.
fn parse_frames(buf: &[u8]) -> Vec<WalRecord> {
    let mut out = vec![];
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let Some(payload) = buf.get(pos + 8..pos + 8 + len) else { break };
        if crc32fast::hash(payload) != crc {
            break;
        }
        match postcard::from_bytes::<WalRecord>(payload) {
            Ok(r) => out.push(r),
            Err(_) => break,
        }
        pos += 8 + len;
    }
    out
}

/// The log file, written in the order the batches were taken (see `Persist::take_batch`). A write waits for its turn, so
/// the bytes of a commit that was taken earlier are always in the file before those of a later one, even when the
/// writers are different sessions that run outside the persist latch.
struct WalFile {
    state: Mutex<WalState>,
    turn: Condvar,
}

struct WalState {
    file: File,
    /// The ticket whose write is next.
    next: u64,
}

impl WalFile {
    fn lock(&self) -> std::sync::MutexGuard<'_, WalState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `bytes` (and force them to disk when `sync`) when it is the turn of `ticket`.
    fn write(&self, ticket: u64, bytes: &[u8], sync: bool) -> io::Result<()> {
        let mut g = self.lock();
        while g.next != ticket {
            g = self.turn.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        let r = (|| {
            if !bytes.is_empty() {
                g.file.write_all(bytes)?;
            }
            if sync {
                g.file.sync_data()?;
            }
            Ok(())
        })();
        g.next += 1;
        self.turn.notify_all();
        r
    }

    /// Wait until every ticket before `ticket` has been written.
    fn wait_for(&self, ticket: u64) {
        let mut g = self.lock();
        while g.next < ticket {
            g = self.turn.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Log bytes taken out of a `Persist` to be written and forced by a session that no longer holds its latch.
pub struct LogBatch {
    /// The last log record the batch (and everything before it) covers.
    pub upto: u64,
    ticket: u64,
    bytes: Vec<u8>,
    wal: Arc<WalFile>,
}

impl LogBatch {
    /// Write the batch and force the log to disk; gives the number of the last record covered.
    pub fn force(self) -> Result<u64, String> {
        self.wal.write(self.ticket, &self.bytes, true).map(|_| self.upto).map_err(|e| format!("I/O error: {}", e))
    }
}

pub struct Persist {
    pub dir: PathBuf,
    wal: Arc<WalFile>,
    /// Frames appended by commits and not yet written to the file: a commit only adds its frame here (under the latch,
    /// which is cheap), and the session that forces the log for the group writes them all (see `take_batch`). Writing
    /// each frame as it came would make every commit wait for the force in progress, as a write to a file that is being
    /// forced blocks on Windows, so no two commits would ever share one.
    pending: Vec<u8>,
    /// The next ticket for a write to the file.
    ticket: u64,
    pub wal_len: u64,
    pub lsn: u64,
    pub checkpoint_threshold: u64,
    _lock: File,
}

fn io_err(e: io::Error) -> crate::error::SqlError {
    crate::error::SqlError::with(3314, 21, 1, format!("I/O error: {}", e))
}

impl Persist {
    /// Open (creating if needed) a data directory and recover the server state from it.
    pub fn open(dir: &Path, opts: StorageOptions) -> Result<(Persist, ServerState), crate::error::SqlError> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let lock = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(dir.join("lock")).map_err(io_err)?;
        if lock.try_lock().is_err() {
            return Err(crate::error::SqlError::with(5120, 16, 1, format!("Unable to open the data directory '{}': it is in use by another process.", dir.display())));
        }
        let mut srv = ServerState::new();
        let mut lsn = 0u64;
        let mut image: Option<StoreImage> = None;
        let snap = dir.join("snapshot.bin");
        if snap.exists() {
            let mut buf = vec![];
            File::open(&snap).and_then(|mut f| f.read_to_end(&mut buf)).map_err(io_err)?;
            if buf.len() < 12 || &buf[..8] != MAGIC {
                return Err(crate::error::SqlError::with(824, 24, 2, "The checkpoint file is not a valid tsql-server snapshot."));
            }
            let crc = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            if crc32fast::hash(&buf[12..]) != crc {
                return Err(crate::error::SqlError::with(824, 24, 2, "The checkpoint file failed its checksum; the database is corrupt."));
            }
            let cp: Checkpoint = postcard::from_bytes(&buf[12..]).map_err(|e| crate::error::SqlError::with(824, 24, 2, format!("Corrupt checkpoint: {}", e)))?;
            lsn = cp.lsn;
            srv.core.advance_clock(cp.clock);
            srv.core.raise_object_id(cp.next_object_id);
            srv.core.raise_rowversion(cp.next_rowversion);
            srv.databases = cp.databases;
            srv.security = cp.security;
            image = Some(cp.store);
        }
        // the page file: tables of the checkpoint refer to pages in it
        let store = PageStore::open(&dir.join("data.db"), true, opts.pool_pages, image)?;
        srv.store = store.clone();
        srv.temp_store = PageStore::open(&dir.join("temp.db"), false, opts.temp_pool_pages, None)?;
        for db in srv.databases.values_mut() {
            for o in db.objects.values_mut() {
                if let Object::Table(t) = o {
                    t.data_mut().attach(&store);
                }
            }
        }
        // replay the log
        let walp = dir.join("wal.log");
        let mut wal = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&walp).map_err(io_err)?;
        let mut data = vec![];
        wal.read_to_end(&mut data).map_err(io_err)?;
        let mut off = 0usize;
        let mut good = 0usize;
        let base_lsn = lsn;
        while off + 8 <= data.len() {
            let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
            if off + 8 + len > data.len() || crc32fast::hash(&data[off + 8..off + 8 + len]) != crc {
                break; // torn tail
            }
            let rec: WalRecord = match postcard::from_bytes(&data[off + 8..off + 8 + len]) {
                Ok(r) => r,
                Err(_) => break,
            };
            off += 8 + len;
            good = off;
            if rec.lsn <= base_lsn {
                continue;
            }
            lsn = rec.lsn;
            apply_record(&mut srv, &rec);
        }
        // drop any torn tail so new frames start at a clean boundary
        wal.set_len(good as u64).map_err(io_err)?;
        wal.seek(SeekFrom::End(0)).map_err(io_err)?;
        // derived structures
        for db in srv.databases.values_mut() {
            for o in db.objects.values_mut() {
                if let Object::Table(t) = o {
                    t.sync_indexes();
                }
            }
            db.rebuild_triggers();
        }
        if let Some(e) = store.failed() {
            return Err(crate::error::SqlError::with(824, 24, 2, format!("Recovery failed: {}", e)));
        }
        let wal = Arc::new(WalFile { state: Mutex::new(WalState { file: wal, next: 0 }), turn: Condvar::new() });
        let p = Persist { dir: dir.to_path_buf(), wal, pending: vec![], ticket: 0, wal_len: good as u64, lsn, checkpoint_threshold: 16 * 1024 * 1024, _lock: lock };
        Ok((p, srv))
    }

    /// Take every frame appended so far out of the latch, to be written and forced by `LogBatch::force` with the latch
    /// released: the commits that arrive meanwhile add to the next batch instead of waiting for this one.
    pub fn take_batch(&mut self) -> LogBatch {
        let ticket = self.ticket;
        self.ticket += 1;
        LogBatch { upto: self.lsn, ticket, bytes: std::mem::take(&mut self.pending), wal: self.wal.clone() }
    }

    /// Write the frames that wait in memory to the file (and force it when `sync`).
    fn flush_pending(&mut self, sync: bool) -> Result<(), crate::error::SqlError> {
        let ticket = self.ticket;
        self.ticket += 1;
        let bytes = std::mem::take(&mut self.pending);
        self.wal.write(ticket, &bytes, sync).map_err(io_err)
    }

    /// Append a frame and write it to the file at once (and force it to disk when `sync`); returns its log sequence
    /// number.
    pub fn append(&mut self, ts: u64, next_object_id: i32, next_rowversion: u64, ops: Vec<LogOp>, sync: bool) -> Result<u64, crate::error::SqlError> {
        let lsn = self.append_buffered(ts, next_object_id, next_rowversion, ops)?;
        self.flush_pending(sync)?;
        Ok(lsn)
    }

    /// Append a frame to the batch the next force writes (see `take_batch`); returns its log sequence number.
    pub fn append_buffered(&mut self, ts: u64, next_object_id: i32, next_rowversion: u64, ops: Vec<LogOp>) -> Result<u64, crate::error::SqlError> {
        self.lsn += 1;
        let rec = WalRecord { lsn: self.lsn, ts, wall: chrono::Utc::now().timestamp_micros(), next_object_id, next_rowversion, ops };
        let payload = postcard::to_allocvec(&rec).map_err(|e| crate::error::SqlError::with(9002, 17, 1, format!("log serialisation failed: {}", e)))?;
        let mut frame = Vec::with_capacity(payload.len() + 8);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
        frame.extend_from_slice(&payload);
        self.pending.extend_from_slice(&frame);
        self.wal_len += frame.len() as u64;
        Ok(self.lsn)
    }

    fn archive_dir(&self) -> PathBuf {
        self.dir.join("log-archive")
    }

    /// Keep a copy of the log (as it is about to be truncated) for log backups.
    fn archive_wal(&mut self) -> Result<(), crate::error::SqlError> {
        if self.wal_len == 0 {
            return Ok(());
        }
        self.flush_pending(false)?;
        std::fs::create_dir_all(self.archive_dir()).map_err(io_err)?;
        let target = self.archive_dir().join(format!("wal-{:020}.log", self.lsn));
        std::fs::copy(self.dir.join("wal.log"), &target).map_err(io_err)?;
        Ok(())
    }

    /// Every committed record written after commit timestamp `ts`, from the archived segments and the live log, in log order.
    pub fn log_records_after(&self, ts: u64) -> Result<Vec<WalRecord>, crate::error::SqlError> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.archive_dir()).map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect()).unwrap_or_default();
        files.sort();
        let live = self.dir.join("wal.log");
        files.push(live.clone());
        // what a session is writing right now must be in the file before it is read
        self.wal.wait_for(self.ticket);
        let mut by_lsn: BTreeMap<u64, WalRecord> = BTreeMap::new();
        for f in files {
            let mut buf = vec![];
            if File::open(&f).and_then(|mut h| h.read_to_end(&mut buf)).is_err() {
                continue;
            }
            if f == live {
                buf.extend_from_slice(&self.pending);
            }
            for rec in parse_frames(&buf) {
                if rec.ts > ts {
                    by_lsn.insert(rec.lsn, rec);
                }
            }
        }
        Ok(by_lsn.into_values().collect())
    }

    /// Delete archived log segments whose records all lie at or before commit timestamp `upto_ts` (they have been backed up).
    pub fn prune_archive(&self, upto_ts: u64) {
        let Ok(rd) = std::fs::read_dir(self.archive_dir()) else { return };
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            let mut buf = vec![];
            if File::open(&path).and_then(|mut h| h.read_to_end(&mut buf)).is_err() {
                continue;
            }
            if parse_frames(&buf).iter().all(|r| r.ts <= upto_ts) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Write a checkpoint of all committed state and truncate the log.
    pub fn checkpoint(&mut self, srv: &ServerState) -> Result<(), crate::error::SqlError> {
        // every changed page goes to the page file first; the checkpoint then names the page table
        let image = srv.store.prepare_checkpoint()?;
        let cp = Checkpoint { lsn: self.lsn, clock: srv.core.clock(), next_object_id: srv.core.object_id(), next_rowversion: srv.core.rowversion(), databases: srv.databases.clone(), security: srv.security.clone(), store: image };
        let body = postcard::to_allocvec(&cp).map_err(|e| crate::error::SqlError::with(9002, 17, 1, format!("checkpoint serialisation failed: {}", e)))?;
        let tmp = self.dir.join("snapshot.tmp");
        {
            let mut f = File::create(&tmp).map_err(io_err)?;
            f.write_all(MAGIC).map_err(io_err)?;
            f.write_all(&crc32fast::hash(&body).to_le_bytes()).map_err(io_err)?;
            f.write_all(&body).map_err(io_err)?;
            f.sync_all().map_err(io_err)?;
        }
        std::fs::rename(&tmp, self.dir.join("snapshot.bin")).map_err(io_err)?;
        srv.store.commit_checkpoint();
        // a database in the FULL recovery model with a log chain keeps its log until it has been backed up
        if srv.databases.values().any(|d| d.recovery_full && d.log_chain_ts.is_some()) {
            self.archive_wal()?;
        }
        self.flush_pending(false)?;
        {
            let mut g = self.wal.lock();
            g.file.set_len(0).map_err(io_err)?;
            g.file.seek(SeekFrom::Start(0)).map_err(io_err)?;
            g.file.sync_data().map_err(io_err)?;
        }
        self.wal_len = 0;
        Ok(())
    }
}

impl Drop for Persist {
    fn drop(&mut self) {
        // frames of commits that were never forced still belong in the file when the server is shut down cleanly
        let _ = self.flush_pending(true);
    }
}

pub fn apply_record(srv: &mut ServerState, rec: &WalRecord) {
    let store = srv.store.clone();
    srv.core.advance_clock(rec.ts);
    srv.core.raise_object_id(rec.next_object_id);
    srv.core.raise_rowversion(rec.next_rowversion);
    for op in &rec.ops {
        match op {
            LogOp::Put { db, key, rowid, row } => {
                if let Some(Object::Table(t)) = srv.databases.get_mut(db).and_then(|d| d.objects.get_mut(key)) {
                    t.data_mut().replace_slot(*rowid, row.clone(), rec.ts);
                }
            }
            LogOp::TableReset { db, key } => {
                if let Some(Object::Table(t)) = srv.databases.get_mut(db).and_then(|d| d.objects.get_mut(key)) {
                    t.data = Arc::new(TableData::new(&store));
                    t.sync_indexes();
                }
            }
            LogOp::SetIdentity { db, key, next } => {
                if let Some(Object::Table(t)) = srv.databases.get_mut(db).and_then(|d| d.objects.get_mut(key)) {
                    t.identity_next.set(*next);
                }
            }
            LogOp::PutObject { db, key, obj } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    let mut o = obj.clone();
                    if let Object::Table(t) = &mut o {
                        // the logged copy carries no usable row data: keep the rows we have
                        t.data = match d.objects.get(key) {
                            Some(Object::Table(old)) if old.data.is_attached() => old.data.clone(),
                            _ => Arc::new(TableData::new(&store)),
                        };
                        t.sync_indexes();
                    }
                    d.objects.insert(key.clone(), o);
                }
            }
            LogOp::DropObject { db, key } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.objects.remove(key);
                }
            }
            LogOp::PutType { db, key, ty } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.types.insert(key.clone(), ty.clone());
                }
            }
            LogOp::DropType { db, key } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.types.remove(key);
                }
            }
            LogOp::PutSchema { db, key, name } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.schemas.insert(key.clone(), name.clone());
                    d.assign_schema_id(key);
                }
            }
            LogOp::DropSchema { db, key } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.schemas.remove(key);
                }
            }
            LogOp::PutDatabase { key, db } => {
                let mut nd = db.clone();
                for (k, o) in nd.objects.iter_mut() {
                    if let Object::Table(t) = o {
                        t.data = match srv.databases.get(key).and_then(|d| d.objects.get(k)) {
                            Some(Object::Table(old)) if old.data.is_attached() => old.data.clone(),
                            _ => Arc::new(TableData::new(&store)),
                        };
                        t.sync_indexes();
                    }
                }
                srv.databases.insert(key.clone(), nd);
            }
            LogOp::DropDatabase { key } => {
                srv.databases.remove(key);
            }
            LogOp::SeqSet { db, key, current } => {
                if let Some(Object::Seq(s)) = srv.databases.get_mut(db).and_then(|d| d.objects.get_mut(key)) {
                    s.current.set(*current);
                }
            }
            LogOp::DbOptions { db, allow_snapshot, rcsi } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.allow_snapshot = *allow_snapshot;
                    d.rcsi = *rcsi;
                }
            }
            LogOp::DbRecovery { db, full } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.recovery_full = *full;
                }
            }
            LogOp::DbPolicies { db, policies } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.policies = policies.clone();
                }
            }
            LogOp::DbXmlSchemas { db, schemas } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.xml_schemas = schemas.clone();
                }
            }
            LogOp::DbFtCatalogs { db, catalogs } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.fulltext_catalogs = catalogs.clone();
                }
            }
            LogOp::DbAuditSpecs { db, specs } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.audit_specs = specs.clone();
                }
            }
            LogOp::DbDdlTriggers { db, triggers } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.ddl_triggers = triggers.clone();
                }
            }
            LogOp::DbBackupBase { db, ts, sigs } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.last_full_ts = *ts;
                    d.backup_sigs = sigs.clone();
                }
            }
            LogOp::DbChain { db, ts } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.log_chain_ts = *ts;
                }
            }
            LogOp::Security(s) => srv.security = s.clone(),
            LogOp::DbSecurity { db, sec, owners } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.security = sec.clone();
                    d.schema_owner = owners.clone();
                }
            }
        }
    }
}
