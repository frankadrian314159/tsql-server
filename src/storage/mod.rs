//! Row storage: a heap and indexes, both B+trees in the paged store ([`page`]).
//!
//! Every row lives in a *slot* (identified by a stable `RowId`). A slot holds a chain of
//! versions, oldest first. A version is stamped either with the id of the transaction that
//! wrote it (still uncommitted) or with its commit timestamp. Readers choose a version
//! according to their [`ReadView`]; writers push a new version and undo it on rollback.
pub mod lock;
pub mod page;
pub mod tx;

pub use page::{PageStore, StoreImage, StoreStats, Tree, MAX_KEY_BYTES};

use crate::convert::sort_cmp;
use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::Arc;

pub type Row = Vec<Value>;
pub type TxId = u64;
pub type RowId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stamp {
    /// Written by an uncommitted transaction.
    Tx(TxId),
    /// Committed at this timestamp.
    Ts(u64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Version {
    /// `None` is a delete marker.
    pub row: Option<Row>,
    pub stamp: Stamp,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Slot {
    pub versions: Vec<Version>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    /// Newest version regardless of who wrote it (READ UNCOMMITTED).
    Dirty,
    /// Newest committed version, plus the reader's own changes.
    Committed,
    /// Versions committed at or before this timestamp, plus the reader's own changes.
    Snapshot(u64),
}

#[derive(Clone, Copy, Debug)]
pub struct ReadView {
    pub tx: TxId,
    pub mode: ViewMode,
}

impl ReadView {
    pub fn dirty() -> ReadView {
        ReadView { tx: 0, mode: ViewMode::Dirty }
    }
    pub fn committed(tx: TxId) -> ReadView {
        ReadView { tx, mode: ViewMode::Committed }
    }
}

impl Slot {
    pub fn visible(&self, v: &ReadView) -> Option<&Row> {
        match v.mode {
            ViewMode::Dirty => self.versions.last().and_then(|x| x.row.as_ref()),
            ViewMode::Committed => {
                for ver in self.versions.iter().rev() {
                    match ver.stamp {
                        Stamp::Tx(t) if t != v.tx => continue,
                        _ => return ver.row.as_ref(),
                    }
                }
                None
            }
            ViewMode::Snapshot(s) => {
                for ver in self.versions.iter().rev() {
                    match ver.stamp {
                        Stamp::Tx(t) => {
                            if t == v.tx {
                                return ver.row.as_ref();
                            }
                        }
                        Stamp::Ts(ts) => {
                            if ts <= s {
                                return ver.row.as_ref();
                            }
                        }
                    }
                }
                None
            }
        }
    }

    /// Transaction (other than `me`) holding an uncommitted change on this slot.
    pub fn foreign_writer(&self, me: TxId) -> Option<TxId> {
        match self.versions.last()?.stamp {
            Stamp::Tx(t) if t != me => Some(t),
            _ => None,
        }
    }

    /// Commit timestamp of the newest committed version (0 when none).
    pub fn latest_committed_ts(&self) -> u64 {
        self.versions
            .iter()
            .rev()
            .find_map(|v| if let Stamp::Ts(t) = v.stamp { Some(t) } else { None })
            .unwrap_or(0)
    }

    /// The newest version (committed or not) as seen by `me`, ignoring other writers' uncommitted data.
    pub fn current(&self, me: TxId) -> Option<&Row> {
        self.visible(&ReadView::committed(me))
    }
}

// ------------------------------------------------------------------ indexes
#[derive(Clone, Debug)]
pub struct KeyVals(pub Vec<Value>);

impl Serialize for KeyVals {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<'de> Deserialize<'de> for KeyVals {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<KeyVals, D::Error> {
        Vec::<Value>::deserialize(d).map(KeyVals)
    }
}

impl PartialEq for KeyVals {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for KeyVals {}
impl PartialOrd for KeyVals {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for KeyVals {
    fn cmp(&self, o: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(o.0.iter()) {
            let c = sort_cmp(a, b);
            if c != Ordering::Equal {
                return c;
            }
        }
        self.0.len().cmp(&o.0.len())
    }
}

/// Compare the first `bound.len()` key columns against a bound.
pub fn cmp_prefix(key: &[Value], bound: &[Value]) -> Ordering {
    for (a, b) in key.iter().zip(bound.iter()) {
        let c = sort_cmp(a, b);
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexSpec {
    pub name: String,
    pub cols: Vec<usize>,
    pub unique: bool,
    pub pk: bool,
}

fn extract(spec: &IndexSpec, row: &Row) -> KeyVals {
    KeyVals(spec.cols.iter().map(|&c| row[c].clone()).collect())
}

fn heap_key() -> KeyVals {
    KeyVals(Vec::new())
}

fn page_cmp(k1: &KeyVals, r1: u64, k2: &KeyVals, r2: u64) -> Ordering {
    k1.cmp(k2).then(r1.cmp(&r2))
}

/// Rows read by [`TableData::scan_chunk`].
pub struct Chunk {
    pub rows: Vec<(RowId, Row)>,
    /// Last slot examined (resume after it).
    pub last: Option<RowId>,
    /// The scan reached the end of the heap.
    pub done: bool,
    /// An uncommitted writer was found (only when asked to check).
    pub blocked_by: Option<TxId>,
}

#[derive(Serialize, Deserialize)]
struct TableDataDesc {
    heap: page::TreeDesc,
    specs: Vec<IndexSpec>,
    indexes: Vec<page::TreeDesc>,
}

// --------------------------------------------------------------- table data
/// The storage of one table: a heap tree (`RowId -> Slot`) and one tree per index. Handles are
/// cheap to clone; clones share the pages (a clone is a second name for the same data, not a copy).
#[derive(Clone, Debug)]
pub struct TableData {
    heap: Arc<Tree>,
    pub specs: Vec<IndexSpec>,
    indexes: Vec<Arc<Tree>>,
}

impl Serialize for TableData {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        TableDataDesc { heap: self.heap.desc(), specs: self.specs.clone(), indexes: self.indexes.iter().map(|t| t.desc()).collect() }.serialize(s)
    }
}
impl<'de> Deserialize<'de> for TableData {
    /// The result is not attached to a page store; call [`TableData::attach`].
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<TableData, D::Error> {
        let desc = TableDataDesc::deserialize(d)?;
        Ok(TableData { heap: Arc::new(Tree::from_desc(desc.heap, None)), specs: desc.specs, indexes: desc.indexes.into_iter().map(|t| Arc::new(Tree::from_desc(t, None))).collect() })
    }
}

impl TableData {
    pub fn new(store: &Arc<PageStore>) -> TableData {
        TableData { heap: Arc::new(Tree::new(store)), specs: vec![], indexes: vec![] }
    }

    /// An empty table with the same index layout.
    pub fn fresh(&self) -> TableData {
        let store = self.store().clone();
        TableData { heap: Arc::new(Tree::new(&store)), specs: self.specs.clone(), indexes: self.specs.iter().map(|_| Arc::new(Tree::new(&store))).collect() }
    }

    pub fn store(&self) -> &Arc<PageStore> {
        self.heap.store().expect("table data is not attached to a page store")
    }

    pub fn is_attached(&self) -> bool {
        self.heap.is_attached()
    }

    /// Identifies the heap; two handles with the same id share their rows.
    pub fn heap_id(&self) -> u32 {
        self.heap.root
    }

    /// Bind the trees of a table that was just decoded (checkpoint or log) to the page store.
    pub fn attach(&mut self, store: &Arc<PageStore>) {
        self.heap = Arc::new(self.heap.attached(store));
        self.indexes = self.indexes.iter().map(|t| Arc::new(t.attached(store))).collect();
    }

    /// Make the physical indexes match `specs`, keeping trees whose definition is unchanged and
    /// building the others from the heap.
    pub fn set_specs(&mut self, specs: Vec<IndexSpec>) {
        if self.specs == specs && self.indexes.len() == specs.len() {
            return;
        }
        let old: Vec<(IndexSpec, Arc<Tree>)> = self.specs.iter().cloned().zip(self.indexes.iter().cloned()).collect();
        let store = self.store().clone();
        let mut trees = Vec::with_capacity(specs.len());
        let mut build = vec![];
        for (i, sp) in specs.iter().enumerate() {
            match old.iter().find(|(o, _)| o == sp) {
                Some((_, t)) => trees.push(t.clone()),
                None => {
                    trees.push(Arc::new(Tree::new(&store)));
                    build.push(i);
                }
            }
        }
        self.specs = specs;
        self.indexes = trees;
        if build.is_empty() {
            return;
        }
        let mut after = None;
        loop {
            let chunk = self.slot_chunk(after, 256);
            for (rid, slot) in &chunk {
                for v in &slot.versions {
                    if let Some(r) = &v.row {
                        for &i in &build {
                            self.indexes[i].put(extract(&self.specs[i], r), *rid as u64, Slot::default());
                        }
                    }
                }
            }
            match chunk.last() {
                Some((r, _)) if chunk.len() == 256 => after = Some(*r),
                _ => break,
            }
        }
    }

    /// Reserve a new row id.
    pub fn alloc(&mut self) -> RowId {
        self.heap.alloc_id() as RowId
    }

    /// Make sure `rowid` is never allocated again (log replay).
    pub fn ensure_slot(&mut self, rowid: RowId) {
        self.heap.reserve_id(rowid as u64);
    }

    /// Slots in the heap (live rows plus deleted-but-not-yet-collected ones).
    pub fn slot_count(&self) -> usize {
        self.heap.entries() as usize
    }

    // ---- slot access
    pub fn with_slot<T>(&self, rid: RowId, f: impl FnOnce(Option<&Slot>) -> T) -> T {
        self.heap.with(&heap_key(), rid as u64, f)
    }

    pub fn slot(&self, rid: RowId) -> Option<Slot> {
        self.with_slot(rid, |s| s.cloned())
    }

    /// The row version `view` sees in slot `rid`.
    pub fn get(&self, rid: RowId, view: &ReadView) -> Option<Row> {
        self.with_slot(rid, |s| s.and_then(|s| s.visible(view)).cloned())
    }

    pub fn foreign_writer(&self, rid: RowId, me: TxId) -> Option<TxId> {
        self.with_slot(rid, |s| s.and_then(|s| s.foreign_writer(me)))
    }

    pub fn latest_committed_ts(&self, rid: RowId) -> u64 {
        self.with_slot(rid, |s| s.map(|s| s.latest_committed_ts()).unwrap_or(0))
    }

    /// The newest version of a slot, whoever wrote it.
    pub fn last_version(&self, rid: RowId) -> Option<Version> {
        self.with_slot(rid, |s| s.and_then(|s| s.versions.last().cloned()))
    }

    fn write_slot(&self, rid: RowId, slot: Slot) {
        if slot.versions.is_empty() {
            self.heap.remove(&heap_key(), rid as u64);
        } else {
            self.heap.put(heap_key(), rid as u64, slot);
        }
    }

    // ---- writes
    pub fn push_version(&mut self, rowid: RowId, row: Option<Row>, stamp: Stamp) {
        let mut slot = self.slot(rowid).unwrap_or_default();
        if let Some(r) = &row {
            for (i, sp) in self.specs.iter().enumerate() {
                self.indexes[i].put(extract(sp, r), rowid as u64, Slot::default());
            }
        }
        slot.versions.push(Version { row, stamp });
        self.write_slot(rowid, slot);
    }

    fn unindex_unused(&self, rowid: RowId, row: &Row, remaining: &Slot) {
        for (i, sp) in self.specs.iter().enumerate() {
            let key = extract(sp, row);
            let still_used = remaining.versions.iter().any(|v| v.row.as_ref().map(|r| extract(sp, r) == key).unwrap_or(false));
            if !still_used {
                self.indexes[i].remove(&key, rowid as u64);
            }
        }
    }

    /// Remove the newest version of a slot (rollback).
    pub fn pop_version(&mut self, rowid: RowId) {
        let mut slot = match self.slot(rowid) {
            Some(s) => s,
            None => return,
        };
        if let Some(v) = slot.versions.pop() {
            if let Some(r) = v.row {
                self.unindex_unused(rowid, &r, &slot);
            }
        }
        self.write_slot(rowid, slot);
    }

    /// Stamp this transaction's versions on the slot as committed at `ts`.
    pub fn commit_slot(&mut self, rowid: RowId, tx: TxId, ts: u64) {
        let mut slot = match self.slot(rowid) {
            Some(s) => s,
            None => return,
        };
        let n = slot.versions.len();
        let mut k = 0;
        while k < n && slot.versions[n - 1 - k].stamp == Stamp::Tx(tx) {
            k += 1;
        }
        if k == 0 {
            return;
        }
        let removed: Vec<Version> = if k > 1 { slot.versions.drain(n - k..n - 1).collect() } else { vec![] };
        if let Some(last) = slot.versions.last_mut() {
            last.stamp = Stamp::Ts(ts);
        }
        for v in removed {
            if let Some(r) = v.row {
                self.unindex_unused(rowid, &r, &slot);
            }
        }
        self.write_slot(rowid, slot);
    }

    /// Discard versions no snapshot at or after `horizon` can see.
    pub fn gc_slot(&mut self, rowid: RowId, horizon: u64) {
        let mut slot = match self.slot(rowid) {
            Some(s) => s,
            None => return,
        };
        let keep_from = slot.versions.iter().rposition(|v| matches!(v.stamp, Stamp::Ts(t) if t <= horizon));
        let k = match keep_from {
            Some(k) => k,
            None => return,
        };
        let mut changed = false;
        if k > 0 {
            let removed: Vec<Version> = slot.versions.drain(..k).collect();
            for v in removed {
                if let Some(r) = v.row {
                    self.unindex_unused(rowid, &r, &slot);
                }
            }
            changed = true;
        }
        // a lone committed delete marker means the row is gone for everyone
        if slot.versions.len() == 1 && slot.versions[0].row.is_none() {
            slot.versions.clear();
            changed = true;
        }
        if changed {
            self.write_slot(rowid, slot);
        }
    }

    /// Replace a slot by a single committed version (or remove it); used by log replay.
    pub fn replace_slot(&mut self, rowid: RowId, row: Option<Row>, ts: u64) {
        if let Some(old) = self.slot(rowid) {
            for v in old.versions {
                if let Some(r) = v.row {
                    for (i, sp) in self.specs.iter().enumerate() {
                        self.indexes[i].remove(&extract(sp, &r), rowid as u64);
                    }
                }
            }
        }
        self.ensure_slot(rowid);
        match row {
            Some(r) => {
                for (i, sp) in self.specs.iter().enumerate() {
                    self.indexes[i].put(extract(sp, &r), rowid as u64, Slot::default());
                }
                self.heap.put(heap_key(), rowid as u64, Slot { versions: vec![Version { row: Some(r), stamp: Stamp::Ts(ts) }] });
            }
            None => self.heap.remove(&heap_key(), rowid as u64),
        }
    }

    // ---- scans
    /// Up to `n` whole slots after `after`, in row-id order.
    pub fn slot_chunk(&self, after: Option<RowId>, n: usize) -> Vec<(RowId, Slot)> {
        let mut out = Vec::new();
        let first = |_: &KeyVals, r: u64| after.map(|a| r > a as u64).unwrap_or(true);
        self.heap.scan(&first, true, &mut |_, r, s| {
            out.push((r as RowId, s.clone()));
            out.len() < n
        });
        out
    }

    /// Visible rows of up to `max_slots` slots after `after`. With `check_writers`, stops at the first
    /// slot another uncommitted transaction is changing (when the view requires committed data).
    pub fn scan_chunk(&self, view: &ReadView, after: Option<RowId>, max_slots: usize, check_writers: bool) -> Chunk {
        let mut rows = Vec::new();
        let mut last = None;
        let mut seen = 0usize;
        let mut blocked_by = None;
        let mut stopped = false;
        let first = |_: &KeyVals, r: u64| after.map(|a| r > a as u64).unwrap_or(true);
        let check = check_writers && view.mode == ViewMode::Committed;
        self.heap.scan(&first, true, &mut |_, r, s| {
            if seen >= max_slots {
                stopped = true;
                return false;
            }
            if check {
                if let Some(w) = s.foreign_writer(view.tx) {
                    blocked_by = Some(w);
                    stopped = true;
                    return false;
                }
            }
            seen += 1;
            last = Some(r as RowId);
            if let Some(row) = s.visible(view) {
                rows.push((r as RowId, row.clone()));
            }
            true
        });
        Chunk { rows, last, done: !stopped, blocked_by }
    }

    /// All rows visible to `view` in row-id order.
    pub fn iter(&self, view: ReadView) -> Vec<(RowId, Row)> {
        let mut out = vec![];
        let mut after = None;
        loop {
            let c = self.scan_chunk(&view, after, 1024, false);
            out.extend(c.rows);
            if c.done || c.last.is_none() {
                break;
            }
            after = c.last;
        }
        out
    }

    /// Visible rows; `Err(tx)` when a row is being changed by another uncommitted transaction
    /// and the view requires committed data.
    pub fn scan_checked(&self, view: ReadView) -> Result<Vec<(RowId, Row)>, TxId> {
        let mut out = vec![];
        let mut after = None;
        loop {
            let c = self.scan_chunk(&view, after, 1024, true);
            if let Some(w) = c.blocked_by {
                return Err(w);
            }
            out.extend(c.rows);
            if c.done || c.last.is_none() {
                break;
            }
            after = c.last;
        }
        Ok(out)
    }

    pub fn count(&self, view: ReadView) -> usize {
        let mut n = 0;
        let mut after = None;
        loop {
            let c = self.scan_chunk(&view, after, 1024, false);
            n += c.rows.len();
            if c.done || c.last.is_none() {
                break;
            }
            after = c.last;
        }
        n
    }

    pub fn is_empty(&self, view: ReadView) -> bool {
        self.count(view) == 0
    }

    // ---- index access
    /// Up to `n` index entries within the bounds, in key order (or reverse), after the entry `after`.
    /// A bound is (values, inclusive) over the first columns of the key.
    pub fn range_chunk(&self, idx: usize, lo: Option<(&[Value], bool)>, hi: Option<(&[Value], bool)>, forward: bool, after: Option<&(KeyVals, u64)>, n: usize) -> Vec<(KeyVals, u64)> {
        let ge_lo = |k: &KeyVals| match lo {
            None => true,
            Some((v, incl)) => {
                let c = cmp_prefix(&k.0, v);
                c == Ordering::Greater || (c == Ordering::Equal && incl)
            }
        };
        let beyond_hi = |k: &KeyVals| match hi {
            None => false,
            Some((v, incl)) => {
                let c = cmp_prefix(&k.0, v);
                c == Ordering::Greater || (c == Ordering::Equal && !incl)
            }
        };
        let mut out: Vec<(KeyVals, u64)> = Vec::new();
        if forward {
            let first = |k: &KeyVals, r: u64| ge_lo(k) && after.map(|a| page_cmp(k, r, &a.0, a.1) == Ordering::Greater).unwrap_or(true);
            self.indexes[idx].scan(&first, true, &mut |k, r, _| {
                if beyond_hi(k) {
                    return false;
                }
                out.push((k.clone(), r));
                out.len() < n
            });
        } else {
            let first = |k: &KeyVals, r: u64| beyond_hi(k) || after.map(|a| page_cmp(k, r, &a.0, a.1) != Ordering::Less).unwrap_or(false);
            self.indexes[idx].scan(&first, false, &mut |k, r, _| {
                if !ge_lo(k) {
                    return false;
                }
                out.push((k.clone(), r));
                out.len() < n
            });
        }
        out
    }

    /// Row ids whose key (first columns) lies between the bounds, all at once.
    pub fn seek_range(&self, idx: usize, lo: Option<(&[Value], bool)>, hi: Option<(&[Value], bool)>, forward: bool) -> Vec<RowId> {
        let mut out = vec![];
        let mut after: Option<(KeyVals, u64)> = None;
        loop {
            let c = self.range_chunk(idx, lo, hi, forward, after.as_ref(), 4096);
            let full = c.len() == 4096;
            out.extend(c.iter().map(|(_, r)| *r as RowId));
            match c.into_iter().last() {
                Some(l) if full => after = Some(l),
                _ => break,
            }
        }
        out
    }

    pub fn seek_eq(&self, idx: usize, prefix: &[Value]) -> Vec<RowId> {
        self.seek_range(idx, Some((prefix, true)), Some((prefix, true)), true)
    }

    pub fn index_key(&self, idx: usize, row: &Row) -> Vec<Value> {
        extract(&self.specs[idx], row).0
    }

    /// The first index whose serialised key for `row` is longer than SQL Server allows, with its length.
    pub fn oversized_key(&self, row: &Row) -> Option<(String, usize)> {
        for sp in &self.specs {
            let n = postcard::to_allocvec(&extract(sp, row)).map(|v| v.len()).unwrap_or(0);
            if n > MAX_KEY_BYTES {
                return Some((sp.name.clone(), n));
            }
        }
        None
    }

    /// Estimated number of distinct values of the first `prefix` key columns of an index: exact for
    /// small indexes, otherwise extrapolated from a sample of leaf pages.
    pub fn distinct_prefixes(&self, idx: usize, prefix: usize) -> usize {
        let tree = &self.indexes[idx];
        let total = tree.entries() as usize;
        let same = |a: &KeyVals, b: &KeyVals| cmp_prefix(&a.0[..prefix.min(a.0.len())], &b.0[..prefix.min(b.0.len())]) == Ordering::Equal;
        if total <= 20_000 {
            let mut n = 0usize;
            let mut last: Option<KeyVals> = None;
            tree.scan(&|_, _| true, true, &mut |k, _, _| {
                if last.as_ref().map(|l| !same(l, k)).unwrap_or(true) {
                    n += 1;
                    last = Some(k.clone());
                }
                true
            });
            return n;
        }
        const SAMPLES: usize = 48;
        let mut distinct = 0usize;
        let mut entries = 0usize;
        let mut leaves = 0usize;
        for s in 0..SAMPLES {
            let (prev_last, keys) = tree.sample_leaf((s as f64 + 0.5) / SAMPLES as f64);
            if keys.is_empty() {
                continue;
            }
            let mut runs = 0usize;
            for (i, k) in keys.iter().enumerate() {
                if i == 0 || !same(&keys[i - 1], k) {
                    runs += 1;
                }
            }
            if prev_last.map(|p| same(&p, &keys[0])).unwrap_or(false) {
                runs -= 1;
            }
            distinct += runs;
            entries += keys.len();
            leaves += 1;
        }
        if leaves == 0 || entries == 0 {
            return 1;
        }
        let est_leaves = total as f64 / (entries as f64 / leaves as f64);
        ((distinct as f64 / leaves as f64) * est_leaves).round().max(1.0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(n: i64, key: impl Fn(i64) -> i64) -> TableData {
        let store = PageStore::memory();
        let mut d = TableData::new(&store);
        d.set_specs(vec![IndexSpec { name: "ix".into(), cols: vec![0], unique: false, pk: false }]);
        for i in 0..n {
            let rid = d.alloc();
            d.push_version(rid, Some(vec![Value::BigInt(key(i)), Value::Int(i as i32)]), Stamp::Ts(1));
        }
        d
    }

    #[test]
    fn distinct_counts_are_exact_for_small_and_close_for_large_indexes() {
        let small = table(5_000, |i| i % 50);
        assert_eq!(small.distinct_prefixes(0, 1), 50);
        let many = table(60_000, |i| i % 600);
        let est = many.distinct_prefixes(0, 1) as f64;
        assert!((est - 600.0).abs() < 600.0 * 0.2, "estimated {} distinct values, expected about 600", est);
        let unique = table(60_000, |i| i);
        let est = unique.distinct_prefixes(0, 1) as f64;
        assert!((est - 60_000.0).abs() < 60_000.0 * 0.2, "estimated {} distinct values, expected about 60000", est);
        let one = table(60_000, |_| 7);
        assert!(one.distinct_prefixes(0, 1) <= 3, "{}", one.distinct_prefixes(0, 1));
    }

    #[test]
    fn range_chunks_resume_where_they_stopped_in_both_directions() {
        let d = table(1_000, |i| i / 3); // keys 0..=333, three rows each
        let lo = [Value::BigInt(100)];
        let hi = [Value::BigInt(200)];
        for forward in [true, false] {
            let mut got = vec![];
            let mut after = None;
            loop {
                let c = d.range_chunk(0, Some((&lo, true)), Some((&hi, false)), forward, after.as_ref(), 7);
                if c.is_empty() {
                    break;
                }
                got.extend(c.iter().cloned());
                after = c.into_iter().last();
            }
            assert_eq!(got.len(), 300, "forward={}", forward);
            let all = d.seek_range(0, Some((&lo, true)), Some((&hi, false)), forward);
            assert_eq!(all.len(), 300);
            assert_eq!(got.iter().map(|(_, r)| *r as usize).collect::<Vec<_>>(), all);
            let ordered = all.windows(2).all(|w| if forward { w[0] <= w[1] } else { w[0] >= w[1] });
            assert!(ordered);
        }
    }
}
