//! Where a Raft node keeps its log, its vote and its snapshot.
use crate::{Conf, Entry};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// What a snapshot stands for: the state of the replicated state machine after applying the log up to `index` (an entry of
/// term `term`), and the membership of the group as of that index. The log of a node that has a snapshot starts after it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SnapMeta {
    pub index: u64,
    pub term: u64,
    pub conf: Conf,
}

/// The persistent state of a node. Writes must be durable when they return (a vote that is forgotten, or an acknowledged
/// entry that is lost, breaks the guarantees of the protocol).
pub trait Storage {
    fn hard_state(&self) -> (u64, Option<u64>);
    fn set_hard_state(&mut self, term: u64, vote: Option<u64>);
    /// The index of the last entry (the snapshot's index when the log is empty after it, 0 when there is neither).
    fn last_index(&self) -> u64;
    /// The term of the entry at `index` (0 is the empty log: term 0; the snapshot's index has the snapshot's term); None for
    /// an index that is compacted away or past the end.
    fn term_at(&self, index: u64) -> Option<u64>;
    /// Entries `from..to` (exclusive end); entries that were compacted are not returned.
    fn entries(&self, from: u64, to: u64) -> Vec<Entry>;
    /// Append entries; the first one's index may be inside the log, in which case the log is cut there first (a conflict).
    fn append(&mut self, entries: &[Entry]);
    /// The snapshot the log starts after, if there is one.
    fn snap_meta(&self) -> Option<SnapMeta>;
    /// The data of that snapshot (empty when there is none).
    fn snap_data(&self) -> Vec<u8>;
    /// Replace the snapshot by one taken at `meta.index` (an index of this log) and drop the entries up to it.
    fn compact(&mut self, meta: SnapMeta, data: Vec<u8>);
    /// Replace the whole log by a snapshot received from the leader: the log is empty after `meta.index`.
    fn install_snapshot(&mut self, meta: SnapMeta, data: Vec<u8>);
}

#[derive(Default, Clone)]
pub struct MemStorage {
    term: u64,
    vote: Option<u64>,
    snap: Option<SnapMeta>,
    snap_data: Vec<u8>,
    log: Vec<Entry>,
}

impl MemStorage {
    pub fn new() -> MemStorage {
        MemStorage::default()
    }

    /// The index the log starts after.
    fn off(&self) -> u64 {
        self.snap.as_ref().map(|s| s.index).unwrap_or(0)
    }

    /// How many entries the log holds (not counting what a snapshot replaced).
    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }
}

impl Storage for MemStorage {
    fn hard_state(&self) -> (u64, Option<u64>) {
        (self.term, self.vote)
    }
    fn set_hard_state(&mut self, term: u64, vote: Option<u64>) {
        self.term = term;
        self.vote = vote;
    }
    fn last_index(&self) -> u64 {
        self.log.last().map(|e| e.index).unwrap_or(self.off())
    }
    fn term_at(&self, index: u64) -> Option<u64> {
        if index == 0 {
            return Some(0);
        }
        let off = self.off();
        if index == off {
            return self.snap.as_ref().map(|s| s.term);
        }
        if index < off {
            return None;
        }
        self.log.get((index - off) as usize - 1).map(|e| e.term)
    }
    fn entries(&self, from: u64, to: u64) -> Vec<Entry> {
        let off = self.off();
        let from = from.max(off + 1);
        if from >= to {
            return vec![];
        }
        let a = (from - off - 1) as usize;
        let b = ((to - off - 1) as usize).min(self.log.len());
        if a >= b {
            return vec![];
        }
        self.log[a..b].to_vec()
    }
    fn append(&mut self, entries: &[Entry]) {
        if let Some(first) = entries.first() {
            let off = self.off();
            debug_assert!(first.index > off, "appending inside the snapshot");
            self.log.truncate((first.index.max(off + 1) - off - 1) as usize);
        }
        self.log.extend_from_slice(entries);
    }
    fn snap_meta(&self) -> Option<SnapMeta> {
        self.snap.clone()
    }
    fn snap_data(&self) -> Vec<u8> {
        self.snap_data.clone()
    }
    fn compact(&mut self, meta: SnapMeta, data: Vec<u8>) {
        let off = self.off();
        if meta.index <= off {
            return;
        }
        let drop = ((meta.index - off) as usize).min(self.log.len());
        self.log.drain(..drop);
        self.snap = Some(meta);
        self.snap_data = data;
    }
    fn install_snapshot(&mut self, meta: SnapMeta, data: Vec<u8>) {
        self.log.clear();
        self.snap = Some(meta);
        self.snap_data = data;
    }
}

/// A log in a file: every append is a frame (length, crc, entry), forced to disk; the vote is in a second small file,
/// replaced atomically, and the snapshot in a third. A torn last frame (a crash while writing) is cut off when the file is
/// opened.
pub struct FileStorage {
    mem: MemStorage,
    log: File,
    state_path: PathBuf,
    snap_path: PathBuf,
    sync: bool,
}

fn frame(e: &Entry) -> Vec<u8> {
    let body = postcard::to_allocvec(e).expect("encode entry");
    let mut f = Vec::with_capacity(body.len() + 8);
    f.extend_from_slice(&(body.len() as u32).to_le_bytes());
    f.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    f.extend_from_slice(&body);
    f
}

impl FileStorage {
    /// Open (or create) the files `dir/raft.log`, `dir/raft.state` and `dir/raft.snap`. With `sync` every write is forced to
    /// disk.
    pub fn open(dir: &Path, sync: bool) -> std::io::Result<FileStorage> {
        std::fs::create_dir_all(dir)?;
        let log_path = dir.join("raft.log");
        let state_path = dir.join("raft.state");
        let snap_path = dir.join("raft.snap");
        let mut mem = MemStorage::new();
        // the snapshot: crc, then (meta, data)
        if let Ok(b) = std::fs::read(&snap_path) {
            if b.len() >= 4 && u32::from_le_bytes(b[..4].try_into().unwrap()) == crc32fast::hash(&b[4..]) {
                if let Ok((meta, data)) = postcard::from_bytes::<(SnapMeta, Vec<u8>)>(&b[4..]) {
                    mem.snap = Some(meta);
                    mem.snap_data = data;
                }
            }
        }
        let mut log = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&log_path)?;
        let mut bytes = vec![];
        log.read_to_end(&mut bytes)?;
        let mut pos = 0usize;
        let mut good = 0usize;
        let off = mem.off();
        let mut stale = false;
        while pos + 8 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
            if pos + 8 + len > bytes.len() || crc32fast::hash(&bytes[pos + 8..pos + 8 + len]) != crc {
                break;
            }
            let Ok(e) = postcard::from_bytes::<Entry>(&bytes[pos + 8..pos + 8 + len]) else { break };
            pos += 8 + len;
            good = pos;
            if e.index <= off {
                // the snapshot was written but the log was not yet cut down (a crash between the two): the entry is in the snapshot
                stale = true;
                continue;
            }
            // a frame whose index is inside the log means a conflicting suffix was rewritten
            mem.append(std::slice::from_ref(&e));
        }
        if good < bytes.len() {
            log.set_len(good as u64)?; // a torn tail
        }
        log.seek(SeekFrom::End(0))?;
        if let Ok(b) = std::fs::read(&state_path) {
            if let Ok((t, v)) = postcard::from_bytes::<(u64, Option<u64>)>(&b) {
                mem.term = t;
                mem.vote = v;
            }
        }
        let mut s = FileStorage { mem, log, state_path, snap_path, sync };
        if stale {
            s.rewrite_log();
        }
        Ok(s)
    }

    fn rewrite_log(&mut self) {
        // the log changed other than by appending (a conflicting suffix was dropped, or it was compacted): write it again
        let tmp = self.state_path.with_extension("logtmp");
        let mut data = vec![];
        for e in self.mem.log.iter() {
            data.extend_from_slice(&frame(e));
        }
        if let Ok(mut f) = File::create(&tmp) {
            let _ = f.write_all(&data);
            if self.sync {
                let _ = f.sync_all();
            }
        }
        let log_path = self.state_path.with_file_name("raft.log");
        let _ = std::fs::rename(&tmp, &log_path);
        if let Ok(mut f) = OpenOptions::new().read(true).write(true).open(&log_path) {
            let _ = f.seek(SeekFrom::End(0));
            self.log = f;
        }
    }

    fn write_snapshot(&self) {
        let Some(meta) = &self.mem.snap else { return };
        let body = postcard::to_allocvec(&(meta, &self.mem.snap_data)).expect("encode snapshot");
        let mut out = Vec::with_capacity(body.len() + 4);
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&body);
        let tmp = self.snap_path.with_extension("tmp");
        if let Ok(mut f) = File::create(&tmp) {
            let _ = f.write_all(&out);
            if self.sync {
                let _ = f.sync_all();
            }
        }
        let _ = std::fs::rename(&tmp, &self.snap_path);
    }
}

impl Storage for FileStorage {
    fn hard_state(&self) -> (u64, Option<u64>) {
        self.mem.hard_state()
    }
    fn set_hard_state(&mut self, term: u64, vote: Option<u64>) {
        if self.mem.hard_state() == (term, vote) {
            return;
        }
        self.mem.set_hard_state(term, vote);
        let bytes = postcard::to_allocvec(&(term, vote)).expect("encode state");
        let tmp = self.state_path.with_extension("tmp");
        if let Ok(mut f) = File::create(&tmp) {
            let _ = f.write_all(&bytes);
            if self.sync {
                let _ = f.sync_all();
            }
        }
        let _ = std::fs::rename(&tmp, &self.state_path);
    }
    fn last_index(&self) -> u64 {
        self.mem.last_index()
    }
    fn term_at(&self, index: u64) -> Option<u64> {
        self.mem.term_at(index)
    }
    fn entries(&self, from: u64, to: u64) -> Vec<Entry> {
        self.mem.entries(from, to)
    }
    fn append(&mut self, entries: &[Entry]) {
        let Some(first) = entries.first() else { return };
        let conflict = first.index <= self.mem.last_index();
        self.mem.append(entries);
        if conflict {
            self.rewrite_log();
        } else {
            let mut data = vec![];
            for e in entries {
                data.extend_from_slice(&frame(e));
            }
            let _ = self.log.write_all(&data);
        }
        if self.sync {
            let _ = self.log.sync_data();
        }
    }
    fn snap_meta(&self) -> Option<SnapMeta> {
        self.mem.snap_meta()
    }
    fn snap_data(&self) -> Vec<u8> {
        self.mem.snap_data()
    }
    fn compact(&mut self, meta: SnapMeta, data: Vec<u8>) {
        if meta.index <= self.mem.off() {
            return;
        }
        self.mem.compact(meta, data);
        // the snapshot first: a crash before the log is cut leaves entries the snapshot already holds, which `open` drops
        self.write_snapshot();
        self.rewrite_log();
    }
    fn install_snapshot(&mut self, meta: SnapMeta, data: Vec<u8>) {
        self.mem.install_snapshot(meta, data);
        self.write_snapshot();
        self.rewrite_log();
    }
}
