//! Where a Raft node keeps its log and its vote.
use crate::Entry;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The persistent state of a node. Both writes must be durable when they return (a vote that is forgotten, or an
/// acknowledged entry that is lost, breaks the guarantees of the protocol).
pub trait Storage {
    fn hard_state(&self) -> (u64, Option<u64>);
    fn set_hard_state(&mut self, term: u64, vote: Option<u64>);
    fn last_index(&self) -> u64;
    /// The term of the entry at `index` (0 is the empty log: term 0); None past the end.
    fn term_at(&self, index: u64) -> Option<u64>;
    /// Entries `from..to` (exclusive end).
    fn entries(&self, from: u64, to: u64) -> Vec<Entry>;
    /// Append entries; the first one's index may be inside the log, in which case the log is cut there first (a conflict).
    fn append(&mut self, entries: &[Entry]);
}

#[derive(Default, Clone)]
pub struct MemStorage {
    term: u64,
    vote: Option<u64>,
    log: Vec<Entry>,
}

impl MemStorage {
    pub fn new() -> MemStorage {
        MemStorage::default()
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
        self.log.last().map(|e| e.index).unwrap_or(0)
    }
    fn term_at(&self, index: u64) -> Option<u64> {
        if index == 0 {
            return Some(0);
        }
        self.log.get(index as usize - 1).map(|e| e.term)
    }
    fn entries(&self, from: u64, to: u64) -> Vec<Entry> {
        let from = from.max(1) as usize - 1;
        let to = ((to.max(1) as usize) - 1).min(self.log.len());
        if from >= to {
            return vec![];
        }
        self.log[from..to].to_vec()
    }
    fn append(&mut self, entries: &[Entry]) {
        if let Some(first) = entries.first() {
            self.log.truncate(first.index as usize - 1);
        }
        self.log.extend_from_slice(entries);
    }
}

/// A log in a file: every append is a frame (length, crc, entry), forced to disk; the vote is in a second small file,
/// replaced atomically. A torn last frame (a crash while writing) is cut off when the file is opened.
pub struct FileStorage {
    mem: MemStorage,
    log: File,
    state_path: PathBuf,
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
    /// Open (or create) the files `dir/raft.log` and `dir/raft.state`. With `sync` every write is forced to disk.
    pub fn open(dir: &Path, sync: bool) -> std::io::Result<FileStorage> {
        std::fs::create_dir_all(dir)?;
        let log_path = dir.join("raft.log");
        let state_path = dir.join("raft.state");
        let mut log = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&log_path)?;
        let mut bytes = vec![];
        log.read_to_end(&mut bytes)?;
        let mut mem = MemStorage::new();
        let mut pos = 0usize;
        let mut good = 0usize;
        while pos + 8 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
            if pos + 8 + len > bytes.len() || crc32fast::hash(&bytes[pos + 8..pos + 8 + len]) != crc {
                break;
            }
            let Ok(e) = postcard::from_bytes::<Entry>(&bytes[pos + 8..pos + 8 + len]) else { break };
            // a frame whose index is inside the log means a conflicting suffix was rewritten
            mem.append(std::slice::from_ref(&e));
            pos += 8 + len;
            good = pos;
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
        Ok(FileStorage { mem, log, state_path, sync })
    }

    fn rewrite_log(&mut self) {
        // a conflicting suffix was dropped: write the whole log again (rare: it happens when a deposed leader rejoins)
        let tmp = self.state_path.with_extension("logtmp");
        let mut data = vec![];
        for e in self.mem.log.iter() {
            data.extend_from_slice(&frame(e));
        }
        let _ = std::fs::write(&tmp, &data);
        let log_path = self.state_path.with_file_name("raft.log");
        let _ = std::fs::rename(&tmp, &log_path);
        if let Ok(mut f) = OpenOptions::new().read(true).write(true).open(&log_path) {
            let _ = f.seek(SeekFrom::End(0));
            self.log = f;
        }
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
}
