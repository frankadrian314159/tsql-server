//! Spilling to disk: sorted runs in temporary files and an external merge sort.
//!
//! Operators that must see all their input (ORDER BY, DISTINCT) collect entries in memory until a budget is
//! reached, write them out as a sorted run, and merge the runs at the end. A run is a sequence of
//! `[u32 length][postcard entry]` frames. Run files are deleted when their handle is dropped, including when a
//! statement fails half-way.
use crate::error::SqlError;
use crate::storage::Row;
use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AtOrd};

type R<T> = Result<T, SqlError>;

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// How much memory operators may use before spilling, and where they spill to.
#[derive(Clone, Debug)]
pub struct WorkMem {
    /// Bytes of rows an operator keeps in memory before it writes a run.
    pub bytes: usize,
    /// Single-table queries over at least this many heap slots use the streaming executor.
    pub stream_min_slots: usize,
    pub dir: PathBuf,
}

impl Default for WorkMem {
    fn default() -> Self {
        let dir = std::env::var_os("TSQL_SPILL_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        WorkMem { bytes: 64 << 20, stream_min_slots: 20_000, dir }
    }
}

fn io_err(e: std::io::Error) -> SqlError {
    SqlError::with(1105, 17, 1, format!("Could not write or read a temporary spill file: {}.", e))
}

/// Approximate in-memory size of a value.
pub fn value_bytes(v: &Value) -> usize {
    32 + match v {
        Value::Str(s) => s.len(),
        Value::Bin(b) => b.len(),
        Value::Dec(_) | Value::Guid(_) | Value::Dto(_) => 16,
        _ => 0,
    }
}

pub fn row_bytes(r: &[Value]) -> usize {
    24 + r.iter().map(value_bytes).sum::<usize>()
}

/// One item being sorted: its sort key, its arrival number (so equal keys keep arrival order, like a stable
/// in-memory sort), and the row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub keys: Vec<Value>,
    pub seq: u64,
    pub row: Row,
}

impl Entry {
    pub fn bytes(&self) -> usize {
        48 + row_bytes(&self.row) + row_bytes(&self.keys)
    }
}

/// A finished sorted run. The file is removed when the run is dropped.
pub struct Run {
    path: PathBuf,
    pub count: usize,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct RunReader {
    r: BufReader<File>,
    _run: Run,
    head: Option<Entry>,
}

impl RunReader {
    fn open(run: Run) -> R<RunReader> {
        let f = File::open(&run.path).map_err(io_err)?;
        let mut rr = RunReader { r: BufReader::with_capacity(256 << 10, f), _run: run, head: None };
        rr.advance()?;
        Ok(rr)
    }

    fn advance(&mut self) -> R<()> {
        let mut len = [0u8; 4];
        match self.r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.head = None;
                return Ok(());
            }
            Err(e) => return Err(io_err(e)),
        }
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        self.r.read_exact(&mut buf).map_err(io_err)?;
        self.head = Some(postcard::from_bytes(&buf).map_err(|e| SqlError::with(1105, 17, 1, format!("Corrupt spill file: {}.", e)))?);
        Ok(())
    }
}

/// Writes entries to a new run file one at a time.
struct RunWriter {
    w: BufWriter<File>,
    run: Run,
}

impl RunWriter {
    fn create(dir: &std::path::Path) -> R<RunWriter> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let path = dir.join(format!("tsql-spill-{}-{}.run", std::process::id(), NEXT_FILE.fetch_add(1, AtOrd::SeqCst)));
        let f = File::create(&path).map_err(io_err)?;
        // the handle removes a half-written file if anything fails before `finish`
        Ok(RunWriter { w: BufWriter::with_capacity(256 << 10, f), run: Run { path, count: 0 } })
    }

    fn push(&mut self, e: &Entry) -> R<()> {
        let bytes = postcard::to_allocvec(e).map_err(|e| SqlError::with(1105, 17, 1, format!("Cannot encode a spilled row: {}.", e)))?;
        self.w.write_all(&(bytes.len() as u32).to_le_bytes()).map_err(io_err)?;
        self.w.write_all(&bytes).map_err(io_err)?;
        self.run.count += 1;
        Ok(())
    }

    fn finish(mut self) -> R<Run> {
        self.w.flush().map_err(io_err)?;
        Ok(self.run)
    }
}

/// A partition of rows written by a grace hash join: `[u32 length][postcard row]` frames in a temporary file that is
/// removed when the handle is dropped.
pub struct PartFile {
    path: PathBuf,
    pub count: usize,
}

impl Drop for PartFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl PartFile {
    pub fn reader(&self) -> R<PartReader> {
        let f = File::open(&self.path).map_err(io_err)?;
        Ok(PartReader { r: BufReader::with_capacity(64 << 10, f) })
    }
}

pub struct PartReader {
    r: BufReader<File>,
}

impl PartReader {
    pub fn next_row(&mut self) -> R<Option<Row>> {
        let mut len = [0u8; 4];
        match self.r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(io_err(e)),
        }
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        self.r.read_exact(&mut buf).map_err(io_err)?;
        postcard::from_bytes(&buf).map(Some).map_err(|e| SqlError::with(1105, 17, 1, format!("Corrupt spill file: {}.", e)))
    }
}

/// Writes rows to a new partition file one at a time.
pub struct PartWriter {
    w: BufWriter<File>,
    part: PartFile,
}

impl PartWriter {
    pub fn create(dir: &std::path::Path) -> R<PartWriter> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let path = dir.join(format!("tsql-spill-{}-{}.part", std::process::id(), NEXT_FILE.fetch_add(1, AtOrd::SeqCst)));
        let f = File::create(&path).map_err(io_err)?;
        Ok(PartWriter { w: BufWriter::with_capacity(32 << 10, f), part: PartFile { path, count: 0 } })
    }

    pub fn push(&mut self, row: &[Value]) -> R<()> {
        let bytes = postcard::to_allocvec(row).map_err(|e| SqlError::with(1105, 17, 1, format!("Cannot encode a spilled row: {}.", e)))?;
        self.w.write_all(&(bytes.len() as u32).to_le_bytes()).map_err(io_err)?;
        self.w.write_all(&bytes).map_err(io_err)?;
        self.part.count += 1;
        Ok(())
    }

    pub fn finish(mut self) -> R<PartFile> {
        self.w.flush().map_err(io_err)?;
        Ok(self.part)
    }
}

fn write_run(dir: &std::path::Path, entries: &[Entry]) -> R<Run> {
    let mut w = RunWriter::create(dir)?;
    for e in entries {
        w.push(e)?;
    }
    w.finish()
}

/// Comparison of two entries; ties must be broken by `seq` by the caller's function.
pub type Cmp = Box<dyn Fn(&Entry, &Entry) -> Ordering>;

/// External merge sort.
pub struct Sorter {
    cmp: Cmp,
    work: WorkMem,
    buf: Vec<Entry>,
    buf_bytes: usize,
    runs: Vec<Run>,
    /// Number of runs written (for tests and statistics).
    pub spilled_runs: usize,
}

const FAN_IN: usize = 48;

impl Sorter {
    pub fn new(cmp: Cmp, work: WorkMem) -> Sorter {
        Sorter { cmp, work, buf: vec![], buf_bytes: 0, runs: vec![], spilled_runs: 0 }
    }

    pub fn push(&mut self, e: Entry) -> R<()> {
        self.buf_bytes += e.bytes();
        self.buf.push(e);
        if self.buf_bytes >= self.work.bytes && self.buf.len() > 1 {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> R<()> {
        let cmp = &self.cmp;
        self.buf.sort_by(|a, b| cmp(a, b));
        let run = write_run(&self.work.dir, &self.buf)?;
        self.runs.push(run);
        self.spilled_runs += 1;
        self.buf.clear();
        self.buf_bytes = 0;
        Ok(())
    }

    /// Merge groups of runs until at most `FAN_IN` remain; each merge streams into one new run.
    fn reduce(&mut self) -> R<()> {
        while self.runs.len() > FAN_IN {
            let group: Vec<Run> = self.runs.drain(..FAN_IN).collect();
            let mut it = Merge::new(group, &self.cmp)?;
            let mut w = RunWriter::create(&self.work.dir)?;
            while let Some(e) = it.next(&self.cmp)? {
                w.push(&e)?;
            }
            drop(it); // deletes the merged inputs
            self.runs.push(w.finish()?);
        }
        Ok(())
    }

    /// All entries in order. Calls `f` for each; `f` returns false to stop early.
    pub fn finish(mut self, mut f: impl FnMut(Entry) -> R<bool>) -> R<()> {
        if self.runs.is_empty() {
            let cmp = &self.cmp;
            self.buf.sort_by(|a, b| cmp(a, b));
            for e in std::mem::take(&mut self.buf) {
                if !f(e)? {
                    break;
                }
            }
            return Ok(());
        }
        if !self.buf.is_empty() {
            self.flush()?;
        }
        self.reduce()?;
        let runs = std::mem::take(&mut self.runs);
        let mut m = Merge::new(runs, &self.cmp)?;
        while let Some(e) = m.next(&self.cmp)? {
            if !f(e)? {
                break;
            }
        }
        Ok(())
    }
}

struct Merge {
    readers: Vec<RunReader>,
}

impl Merge {
    fn new(runs: Vec<Run>, _cmp: &Cmp) -> R<Merge> {
        let mut readers = vec![];
        for r in runs {
            readers.push(RunReader::open(r)?);
        }
        Ok(Merge { readers })
    }

    fn next(&mut self, cmp: &Cmp) -> R<Option<Entry>> {
        let mut best: Option<usize> = None;
        for (i, r) in self.readers.iter().enumerate() {
            if let Some(h) = &r.head {
                best = match best {
                    None => Some(i),
                    Some(b) => {
                        if cmp(h, self.readers[b].head.as_ref().unwrap()) == Ordering::Less {
                            Some(i)
                        } else {
                            Some(b)
                        }
                    }
                };
            }
        }
        match best {
            None => Ok(None),
            Some(i) => {
                let e = self.readers[i].head.take();
                self.readers[i].advance()?;
                Ok(e)
            }
        }
    }
}

/// Number of spill files currently present in `dir` (for tests: nothing may be left behind).
pub fn leftover_files(dir: &std::path::Path) -> usize {
    let prefix = format!("tsql-spill-{}-", std::process::id());
    std::fs::read_dir(dir).map(|d| d.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)).count()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(k: i32, seq: u64) -> Entry {
        Entry { keys: vec![Value::Int(k)], seq, row: vec![Value::Int(k), Value::Str(format!("row {seq}"))] }
    }

    fn by_key() -> Cmp {
        Box::new(|a, b| {
            let (Value::Int(x), Value::Int(y)) = (&a.keys[0], &b.keys[0]) else { unreachable!() };
            x.cmp(y).then(a.seq.cmp(&b.seq))
        })
    }

    #[test]
    fn sorts_across_many_runs_and_keeps_ties_in_arrival_order() {
        let dir = tempfile::tempdir().unwrap();
        // a budget so small that nearly every few entries spill, and more runs than the merge fan-in
        let work = WorkMem { bytes: 600, stream_min_slots: 0, dir: dir.path().to_path_buf() };
        let mut s = Sorter::new(by_key(), work);
        let mut expect = vec![];
        for i in 0..3000u64 {
            let k = ((i * 7919) % 97) as i32;
            expect.push((k, i));
            s.push(entry(k, i)).unwrap();
        }
        assert!(s.spilled_runs > FAN_IN, "expected more than {} runs, got {}", FAN_IN, s.spilled_runs);
        expect.sort();
        let mut got = vec![];
        s.finish(|e| {
            if let Value::Int(k) = e.keys[0] {
                got.push((k, e.seq));
            }
            Ok(true)
        })
        .unwrap();
        assert_eq!(got, expect);
        assert_eq!(leftover_files(dir.path()), 0, "run files are deleted");
    }

    #[test]
    fn small_inputs_never_touch_the_disk_and_early_stop_works() {
        let dir = tempfile::tempdir().unwrap();
        let work = WorkMem { bytes: 1 << 20, stream_min_slots: 0, dir: dir.path().to_path_buf() };
        let mut s = Sorter::new(by_key(), work);
        for i in (0..100u64).rev() {
            s.push(entry(i as i32, i)).unwrap();
        }
        assert_eq!(s.spilled_runs, 0);
        let mut n = 0;
        s.finish(|_| {
            n += 1;
            Ok(n < 10)
        })
        .unwrap();
        assert_eq!(n, 10);
        assert_eq!(leftover_files(dir.path()), 0);
    }

    #[test]
    fn dropping_an_unfinished_sorter_removes_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let work = WorkMem { bytes: 400, stream_min_slots: 0, dir: dir.path().to_path_buf() };
        {
            let mut s = Sorter::new(by_key(), work);
            for i in 0..200u64 {
                s.push(entry(i as i32, i)).unwrap();
            }
            assert!(s.spilled_runs > 0);
            assert!(leftover_files(dir.path()) > 0);
        }
        assert_eq!(leftover_files(dir.path()), 0);
    }
}
