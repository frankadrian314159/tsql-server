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
use std::sync::Arc;

const MAGIC: &[u8; 8] = b"TSQLSNP2";

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
    Security(Security),
    DbSecurity { db: String, sec: crate::security::DbSecurity, owners: BTreeMap<String, String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalRecord {
    pub lsn: u64,
    pub ts: u64,
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

pub struct Persist {
    pub dir: PathBuf,
    wal: File,
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
            srv.clock = cp.clock;
            srv.next_object_id = cp.next_object_id;
            srv.next_rowversion = cp.next_rowversion;
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
        let p = Persist { dir: dir.to_path_buf(), wal, wal_len: good as u64, lsn, checkpoint_threshold: 16 * 1024 * 1024, _lock: lock };
        Ok((p, srv))
    }

    /// Append a frame and force it to disk (when `sync`).
    pub fn append(&mut self, ts: u64, next_object_id: i32, next_rowversion: u64, ops: Vec<LogOp>, sync: bool) -> Result<(), crate::error::SqlError> {
        self.lsn += 1;
        let rec = WalRecord { lsn: self.lsn, ts, next_object_id, next_rowversion, ops };
        let payload = postcard::to_allocvec(&rec).map_err(|e| crate::error::SqlError::with(9002, 17, 1, format!("log serialisation failed: {}", e)))?;
        let mut frame = Vec::with_capacity(payload.len() + 8);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
        frame.extend_from_slice(&payload);
        self.wal.write_all(&frame).map_err(io_err)?;
        if sync {
            self.wal.sync_data().map_err(io_err)?;
        }
        self.wal_len += frame.len() as u64;
        Ok(())
    }

    /// Write a checkpoint of all committed state and truncate the log.
    pub fn checkpoint(&mut self, srv: &ServerState) -> Result<(), crate::error::SqlError> {
        // every changed page goes to the page file first; the checkpoint then names the page table
        let image = srv.store.prepare_checkpoint()?;
        let cp = Checkpoint { lsn: self.lsn, clock: srv.clock, next_object_id: srv.next_object_id, next_rowversion: srv.next_rowversion, databases: srv.databases.clone(), security: srv.security.clone(), store: image };
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
        self.wal.set_len(0).map_err(io_err)?;
        self.wal.seek(SeekFrom::Start(0)).map_err(io_err)?;
        self.wal.sync_data().map_err(io_err)?;
        self.wal_len = 0;
        Ok(())
    }
}

fn apply_record(srv: &mut ServerState, rec: &WalRecord) {
    let store = srv.store.clone();
    srv.clock = srv.clock.max(rec.ts);
    srv.next_object_id = srv.next_object_id.max(rec.next_object_id);
    srv.next_rowversion = srv.next_rowversion.max(rec.next_rowversion);
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
                    t.identity_next = *next;
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
                    s.current = *current;
                }
            }
            LogOp::DbOptions { db, allow_snapshot, rcsi } => {
                if let Some(d) = srv.databases.get_mut(db) {
                    d.allow_snapshot = *allow_snapshot;
                    d.rcsi = *rcsi;
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
