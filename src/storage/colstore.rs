//! Columnstore: a table's rows kept a second time as compressed column segments in row groups (see docs/columnstore.md).
//!
//! The heap stays the table's storage and the source of truth for every reader; the columnstore is built from it and read in its
//! place only where that gives the same rows. Each *row group* holds up to `rowgroup_rows()` rows of consecutive row ids, a *segment*
//! per column (integers, floats and decimals as plain vectors, everything else dictionary-encoded when that pays), and the smallest
//! and largest value of each segment so that a scan can skip a whole group.
//!
//! What goes in a group is only a row that has exactly one version, committed: that version is what every reader sees, as long as
//! nothing writes to the row. A write to a row inside the built range puts its row id in the *dirty* set (`TableData::push_version`
//! and the other writers call `cs_mark`), and a scan reads dirty rows from the heap, as it reads rows that came after the build (the
//! *delta store*: the rows with row ids above `upto`) and rows that have more than one version. So a reader of any isolation level
//! gets what a heap scan gives it, except a snapshot reader older than the newest row in the groups, which does not use them.
//!
//! The columnstore lives in memory, in the table's `ColCell` (shared by the clones of its `TableData`); it is not logged or written
//! to the page file. After a restart it is built again when first needed, or by `ALTER INDEX ... REBUILD`.
use super::*;
use crate::types::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering as AO};
use std::sync::Mutex;

/// Set when any table has been given a columnstore index: until then a query is not searched for the columns it names.
pub static USED: AtomicBool = AtomicBool::new(false);

static ROWGROUP_ROWS: AtomicUsize = AtomicUsize::new(0);
static MIN_ROWS: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Rows in a full row group: 1,048,576 as in SQL Server, or `TSQL_COLUMNSTORE_ROWGROUP` (the tests make it small).
pub fn rowgroup_rows() -> usize {
    let n = ROWGROUP_ROWS.load(AO::Relaxed);
    if n != 0 {
        return n;
    }
    let n = std::env::var("TSQL_COLUMNSTORE_ROWGROUP").ok().and_then(|s| s.parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(1_048_576);
    ROWGROUP_ROWS.store(n, AO::Relaxed);
    n
}

/// Slots a table needs before its columnstore is built when it is first read: below that the heap is read, as a small table is not
/// worth compressing (`TSQL_COLUMNSTORE_MIN_ROWS`).
pub fn min_rows() -> usize {
    let n = MIN_ROWS.load(AO::Relaxed);
    if n != usize::MAX {
        return n;
    }
    let n = std::env::var("TSQL_COLUMNSTORE_MIN_ROWS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1000);
    MIN_ROWS.store(n, AO::Relaxed);
    n
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntKind {
    Bit,
    Int,
    BigInt,
}

#[derive(Debug)]
pub enum SegData {
    /// every value is NULL
    Nulls,
    /// `bit`, `int` (also `tinyint` and `smallint`, which this server holds as `int`) and `bigint` values
    Int(Vec<i64>, IntKind),
    Float(Vec<f64>),
    /// decimals of one scale: the unscaled values
    Dec(Vec<i128>, u8),
    /// the distinct values, and for each row the position of its value in them
    Dict { dict: Vec<Value>, codes: Vec<u32> },
    Vals(Vec<Value>),
}

/// One column of a row group. A NULL has a bit in `nulls` (empty when there is none) and a placeholder in the data.
#[derive(Debug)]
pub struct Segment {
    pub data: SegData,
    pub nulls: Vec<u64>,
    pub null_count: usize,
    pub rows: usize,
    /// the smallest and largest value that is not NULL, when the type has an order that does not depend on a collation
    pub min: Option<Value>,
    pub max: Option<Value>,
}

impl Segment {
    #[inline]
    pub fn is_null(&self, i: usize) -> bool {
        self.null_count > 0 && (self.nulls[i >> 6] >> (i & 63)) & 1 == 1
    }

    pub fn get(&self, i: usize) -> Value {
        if self.is_null(i) {
            return Value::Null;
        }
        match &self.data {
            SegData::Nulls => Value::Null,
            SegData::Int(v, IntKind::Bit) => Value::Bit(v[i] != 0),
            SegData::Int(v, IntKind::Int) => Value::Int(v[i] as i32),
            SegData::Int(v, IntKind::BigInt) => Value::BigInt(v[i]),
            SegData::Float(v) => Value::Float(v[i]),
            SegData::Dec(v, s) => Value::Dec(crate::decimal::Dec { m: v[i], s: *s }),
            SegData::Dict { dict, codes } => dict[codes[i] as usize].clone(),
            SegData::Vals(v) => v[i].clone(),
        }
    }

    /// Bytes held (an estimate: strings by their length).
    pub fn bytes(&self) -> usize {
        let vb = |v: &Value| -> usize {
            match v {
                Value::Str(s) => 24 + s.len(),
                Value::Bin(b) => 24 + b.len(),
                _ => 24,
            }
        };
        self.nulls.len() * 8
            + match &self.data {
                SegData::Nulls => 0,
                SegData::Int(v, _) => v.len() * 8,
                SegData::Float(v) => v.len() * 8,
                SegData::Dec(v, _) => v.len() * 16,
                SegData::Dict { dict, codes } => codes.len() * 4 + dict.iter().map(vb).sum::<usize>(),
                SegData::Vals(v) => v.iter().map(vb).sum(),
            }
    }

    /// Compress the values of one column of a row group.
    pub fn encode(vals: &[&Value]) -> Segment {
        let n = vals.len();
        let mut nulls: Vec<u64> = vec![];
        let mut null_count = 0usize;
        for (i, v) in vals.iter().enumerate() {
            if v.is_null() {
                if nulls.is_empty() {
                    nulls = vec![0u64; (n + 63) / 64];
                }
                nulls[i >> 6] |= 1u64 << (i & 63);
                null_count += 1;
            }
        }
        let mk = |data: SegData, min: Option<Value>, max: Option<Value>| Segment { data, nulls: nulls.clone(), null_count, rows: n, min, max };
        if null_count == n {
            return mk(SegData::Nulls, None, None);
        }
        let first = vals.iter().find(|v| !v.is_null()).copied().expect("a value that is not NULL");
        // plain vectors for a column whose values all have the same type
        match first {
            Value::Int(_) | Value::BigInt(_) | Value::Bit(_) => {
                let kind = match first {
                    Value::Int(_) => IntKind::Int,
                    Value::BigInt(_) => IntKind::BigInt,
                    _ => IntKind::Bit,
                };
                let mut out = Vec::with_capacity(n);
                let (mut lo, mut hi) = (i64::MAX, i64::MIN);
                let mut ok = true;
                for v in vals {
                    let x = match (v, kind) {
                        (Value::Null, _) => 0,
                        (Value::Int(x), IntKind::Int) => *x as i64,
                        (Value::BigInt(x), IntKind::BigInt) => *x,
                        (Value::Bit(b), IntKind::Bit) => *b as i64,
                        _ => {
                            ok = false;
                            break;
                        }
                    };
                    if !v.is_null() {
                        lo = lo.min(x);
                        hi = hi.max(x);
                    }
                    out.push(x);
                }
                if ok {
                    let wrap = |x: i64| match kind {
                        IntKind::Int => Value::Int(x as i32),
                        IntKind::BigInt => Value::BigInt(x),
                        IntKind::Bit => Value::Bit(x != 0),
                    };
                    return mk(SegData::Int(out, kind), Some(wrap(lo)), Some(wrap(hi)));
                }
            }
            Value::Float(_) => {
                let mut out = Vec::with_capacity(n);
                let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
                let mut ok = true;
                for v in vals {
                    match v {
                        Value::Null => out.push(0.0),
                        Value::Float(x) if !x.is_nan() => {
                            lo = lo.min(*x);
                            hi = hi.max(*x);
                            out.push(*x);
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    return mk(SegData::Float(out), Some(Value::Float(lo)), Some(Value::Float(hi)));
                }
            }
            Value::Dec(d0) => {
                let s = d0.s;
                let mut out = Vec::with_capacity(n);
                let (mut lo, mut hi) = (i128::MAX, i128::MIN);
                let mut ok = true;
                for v in vals {
                    match v {
                        Value::Null => out.push(0),
                        Value::Dec(d) if d.s == s => {
                            lo = lo.min(d.m);
                            hi = hi.max(d.m);
                            out.push(d.m);
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    return mk(SegData::Dec(out, s), Some(Value::Dec(crate::decimal::Dec { m: lo, s })), Some(Value::Dec(crate::decimal::Dec { m: hi, s })));
                }
            }
            _ => {}
        }
        // a dictionary when the values repeat; one entry per row is no compression
        let key = |v: &Value| crate::convert::key_of(std::slice::from_ref(v));
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut dict: Vec<Value> = vec![];
        let mut codes: Vec<u32> = Vec::with_capacity(n);
        let mut poor = false;
        for v in vals {
            if v.is_null() {
                codes.push(0);
                continue;
            }
            let k = key(v);
            let id = match ids.get(&k) {
                Some(id) => *id,
                None => {
                    let id = dict.len() as u32;
                    dict.push((*v).clone());
                    ids.insert(k, id);
                    if n > 64 && dict.len() * 2 > n {
                        poor = true;
                        break;
                    }
                    id
                }
            };
            codes.push(id);
        }
        // the order of a string depends on a collation, so it gives no bounds; the other types order the same everywhere
        let ordered = !matches!(first, Value::Str(_) | Value::Bin(_));
        let bounds = |dict: &[Value]| -> (Option<Value>, Option<Value>) {
            if !ordered || dict.is_empty() {
                return (None, None);
            }
            let (mut lo, mut hi) = (&dict[0], &dict[0]);
            for d in dict {
                if crate::convert::sort_cmp(d, lo) == std::cmp::Ordering::Less {
                    lo = d;
                }
                if crate::convert::sort_cmp(d, hi) == std::cmp::Ordering::Greater {
                    hi = d;
                }
            }
            (Some(lo.clone()), Some(hi.clone()))
        };
        if !poor {
            let (lo, hi) = bounds(&dict);
            return mk(SegData::Dict { dict, codes }, lo, hi);
        }
        let vs: Vec<Value> = vals.iter().map(|v| (*v).clone()).collect();
        let present: Vec<Value> = vs.iter().filter(|v| !v.is_null()).cloned().collect();
        let (lo, hi) = bounds(&present);
        mk(SegData::Vals(vs), lo, hi)
    }
}

/// Up to `rowgroup_rows()` rows: their row ids (ascending) and a segment per column of the index.
#[derive(Debug)]
pub struct RowGroup {
    pub rowids: Vec<RowId>,
    pub cols: Vec<Segment>,
    /// the newest commit timestamp among its rows
    pub max_ts: u64,
}

impl RowGroup {
    pub fn rows(&self) -> usize {
        self.rowids.len()
    }
    pub fn bytes(&self) -> usize {
        self.rowids.len() * 8 + self.cols.iter().map(|c| c.bytes()).sum::<usize>()
    }
}

/// The row groups of a columnstore, and what they cover.
#[derive(Debug)]
pub struct CsData {
    /// the table columns the segments are of, in the order of `RowGroup::cols`
    pub colmap: Vec<usize>,
    /// the number of columns the table had (a table that has changed shape needs a new columnstore)
    pub width: usize,
    pub groups: Vec<Arc<RowGroup>>,
    /// rows with a row id above this are not in the groups (the delta store); `None`: the heap was empty
    pub upto: Option<RowId>,
    pub max_ts: u64,
    /// slots the heap had when this was built
    pub slots: usize,
}

impl CsData {
    pub fn rows(&self) -> usize {
        self.groups.iter().map(|g| g.rows()).sum()
    }
    pub fn bytes(&self) -> usize {
        self.groups.iter().map(|g| g.bytes()).sum()
    }
}

/// What a reader gets: the groups, and the row ids (sorted, none above `upto`) of rows to read from the heap instead.
#[derive(Clone, Debug)]
pub struct CsView {
    pub data: Arc<CsData>,
    pub dirty: Vec<RowId>,
}

/// A summary for `sys.column_store_row_groups` and for deciding when to build again.
#[derive(Clone, Debug, Default)]
pub struct CsInfo {
    pub built: bool,
    pub groups: usize,
    pub rows: usize,
    pub dirty: usize,
    /// slots added to the heap since the build (approximately: some slots may have been collected)
    pub delta: usize,
    pub bytes: usize,
}

#[derive(Default)]
struct CellState {
    data: Option<Arc<CsData>>,
    /// row ids written since they were read for the groups (or that were not eligible), with the sequence number of the mark
    dirty: HashMap<RowId, u64>,
}

/// The columnstore of a table (shared by the clones of its `TableData`).
pub struct ColCell {
    /// some columnstore data exists or is being built: writes are noted
    active: AtomicBool,
    /// writes to row ids up to this are noted (everything while a build runs)
    limit: AtomicI64,
    seq: AtomicU64,
    gen: AtomicU64,
    building: AtomicBool,
    state: Mutex<CellState>,
}

impl Default for ColCell {
    fn default() -> ColCell {
        ColCell { active: AtomicBool::new(false), limit: AtomicI64::new(-1), seq: AtomicU64::new(0), gen: AtomicU64::new(0), building: AtomicBool::new(false), state: Mutex::new(CellState::default()) }
    }
}

impl std::fmt::Debug for ColCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ColCell")
    }
}

struct BuildGuard<'a>(&'a AtomicBool);

impl Drop for BuildGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, AO::Release);
    }
}

enum Class {
    Clean(Row, u64),
    Dirty,
    Gone,
}

fn classify(s: &Slot) -> Class {
    match s.versions.len() {
        0 => Class::Gone,
        1 => match (&s.versions[0].stamp, &s.versions[0].row) {
            (Stamp::Ts(ts), Some(row)) => Class::Clean(row.clone(), *ts),
            (Stamp::Ts(_), None) => Class::Gone,
            _ => Class::Dirty,
        },
        _ => Class::Dirty,
    }
}

impl ColCell {
    fn mark(&self, rids: &[RowId]) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for &r in rids {
            let s = self.seq.fetch_add(1, AO::Relaxed) + 1;
            st.dirty.insert(r, s);
        }
    }
}

impl TableData {
    /// A write to this row: a columnstore built from the heap must not be read for it any more.
    #[inline]
    pub(super) fn cs_mark(&self, rid: RowId) {
        if self.cs.active.load(AO::Relaxed) && (rid as i64) <= self.cs.limit.load(AO::Relaxed) {
            self.cs.mark(&[rid]);
        }
    }

    pub(super) fn cs_mark_many(&self, rids: &[RowId]) {
        if self.cs.active.load(AO::Relaxed) {
            let lim = self.cs.limit.load(AO::Relaxed);
            let v: Vec<RowId> = rids.iter().copied().filter(|r| (*r as i64) <= lim).collect();
            if !v.is_empty() {
                self.cs.mark(&v);
            }
        }
    }

    /// Forget the columnstore (the index was dropped, disabled or is to be rebuilt).
    pub fn columnstore_clear(&self) {
        let mut st = self.cs.state.lock().unwrap_or_else(|e| e.into_inner());
        st.data = None;
        st.dirty.clear();
        self.cs.gen.fetch_add(1, AO::Relaxed);
        self.cs.active.store(false, AO::Relaxed);
        self.cs.limit.store(-1, AO::Relaxed);
    }

    pub fn columnstore_info(&self) -> CsInfo {
        let st = self.cs.state.lock().unwrap_or_else(|e| e.into_inner());
        match &st.data {
            None => CsInfo::default(),
            Some(d) => {
                let upto = d.upto.map(|u| u as i64).unwrap_or(-1);
                CsInfo { built: true, groups: d.groups.len(), rows: d.rows(), dirty: st.dirty.keys().filter(|r| (**r as i64) <= upto).count(), delta: self.slot_count().saturating_sub(d.slots), bytes: d.bytes() }
            }
        }
    }

    /// The columnstore as it is, whatever columns it is of (for the catalog views).
    pub fn columnstore_snapshot(&self) -> Option<CsView> {
        let st = self.cs.state.lock().unwrap_or_else(|e| e.into_inner());
        let d = st.data.as_ref()?;
        let upto = d.upto.map(|u| u as i64).unwrap_or(-1);
        let mut dirty: Vec<RowId> = st.dirty.keys().copied().filter(|r| (*r as i64) <= upto).collect();
        dirty.sort_unstable();
        Some(CsView { data: d.clone(), dirty })
    }

    /// The columnstore for a read of `colmap` columns of a table `width` wide, if there is one of that shape.
    pub fn columnstore_view(&self, colmap: &[usize], width: usize) -> Option<CsView> {
        let st = self.cs.state.lock().unwrap_or_else(|e| e.into_inner());
        let d = st.data.as_ref()?;
        if d.colmap != colmap || d.width != width {
            return None;
        }
        let upto = d.upto.map(|u| u as i64).unwrap_or(-1);
        let mut dirty: Vec<RowId> = st.dirty.keys().copied().filter(|r| (*r as i64) <= upto).collect();
        dirty.sort_unstable();
        Some(CsView { data: d.clone(), dirty })
    }

    /// Build the columnstore of the columns `colmap` of this table (`width` wide) from the heap: all of it (`full`), or only the rows
    /// that came after what is already in it. False when another thread is building it, or something changed it meanwhile.
    pub fn columnstore_build(&self, colmap: &[usize], width: usize, full: bool) -> bool {
        let cell = &self.cs;
        if cell.building.swap(true, AO::AcqRel) {
            return false;
        }
        let _guard = BuildGuard(&cell.building);
        let (existing, epoch, gen) = {
            let st = cell.state.lock().unwrap_or_else(|e| e.into_inner());
            let ex = if full { None } else { st.data.clone().filter(|d| d.colmap == colmap && d.width == width) };
            // (from here every write is noted, so that none made while the rows are read is lost)
            cell.limit.store(i64::MAX, AO::Relaxed);
            cell.active.store(true, AO::Relaxed);
            (ex, cell.seq.load(AO::Relaxed), cell.gen.load(AO::Relaxed))
        };
        let full = existing.is_none();
        let rg = rowgroup_rows();
        let mut after: Option<RowId> = existing.as_ref().and_then(|d| d.upto);
        let mut upto = after;
        let mut groups: Vec<Arc<RowGroup>> = existing.as_ref().map(|d| d.groups.clone()).unwrap_or_default();
        let mut max_ts = existing.as_ref().map(|d| d.max_ts).unwrap_or(0);
        let mut pend: Vec<(RowId, Row, u64)> = vec![];
        let mut new_dirty: Vec<RowId> = vec![];
        let seal = |pend: &mut Vec<(RowId, Row, u64)>, take: usize, groups: &mut Vec<Arc<RowGroup>>, max_ts: &mut u64| {
            let rows: Vec<(RowId, Row, u64)> = pend.drain(..take).collect();
            let cols: Vec<Segment> = colmap
                .iter()
                .map(|&c| {
                    let null = Value::Null;
                    let vals: Vec<&Value> = rows.iter().map(|(_, r, _)| r.get(c).unwrap_or(&null)).collect();
                    Segment::encode(&vals)
                })
                .collect();
            let ts = rows.iter().map(|(_, _, t)| *t).max().unwrap_or(0);
            *max_ts = (*max_ts).max(ts);
            groups.push(Arc::new(RowGroup { rowids: rows.iter().map(|(r, _, _)| *r).collect(), cols, max_ts: ts }));
        };
        loop {
            let mut chunk: Vec<(RowId, Row, u64)> = vec![];
            let mut last: Option<RowId> = None;
            let mut seen = 0usize;
            let mut stopped = false;
            let first = |_: &KeyVals, r: u64| after.map(|a| r > a as u64).unwrap_or(true);
            self.heap.scan(&first, true, &mut |_, r, s| {
                if seen >= 4096 {
                    stopped = true;
                    return false;
                }
                seen += 1;
                last = Some(r as RowId);
                match classify(s) {
                    Class::Clean(row, ts) => {
                        if row.len() == width {
                            chunk.push((r as RowId, row, ts));
                        } else {
                            new_dirty.push(r as RowId);
                        }
                    }
                    Class::Dirty => new_dirty.push(r as RowId),
                    Class::Gone => {}
                }
                true
            });
            pend.extend(chunk);
            while pend.len() >= rg {
                seal(&mut pend, rg, &mut groups, &mut max_ts);
            }
            if last.is_some() {
                upto = last;
            }
            if !stopped {
                break;
            }
            after = last;
        }
        if !pend.is_empty() {
            let n = pend.len();
            seal(&mut pend, n, &mut groups, &mut max_ts);
        }
        let slots = self.slot_count();
        let mut st = cell.state.lock().unwrap_or_else(|e| e.into_inner());
        if cell.gen.load(AO::Relaxed) != gen {
            return false;
        }
        if full {
            // (rows written before this build began were read as they are; the ones since stay marked)
            st.dirty.retain(|_, s| *s > epoch);
        }
        for r in new_dirty {
            let s = cell.seq.fetch_add(1, AO::Relaxed) + 1;
            st.dirty.insert(r, s);
        }
        st.data = Some(Arc::new(CsData { colmap: colmap.to_vec(), width, groups, upto, max_ts, slots }));
        cell.limit.store(upto.map(|u| u as i64).unwrap_or(-1), AO::Relaxed);
        true
    }
}

impl CsData {
    /// The rows of group `g` at the positions `sel` (all when `None`), with the columns `needed` (table column numbers; the others
    /// are NULL; `None`: all of them), leaving out the row ids in `skip` (sorted). The columnstore must cover the columns needed.
    pub fn rows_of(&self, g: &RowGroup, needed: Option<&[bool]>, sel: Option<&[u32]>, skip: &[RowId]) -> Vec<(RowId, Row)> {
        let want: Vec<(usize, usize)> = self.colmap.iter().enumerate().filter(|(_, c)| needed.map(|n| n.get(**c).copied().unwrap_or(true)).unwrap_or(true)).map(|(p, c)| (p, *c)).collect();
        let width = self.width;
        let n = sel.map(|s| s.len()).unwrap_or(g.rows());
        let mut out: Vec<(RowId, Row)> = Vec::with_capacity(n);
        let mut sk = 0usize;
        for k in 0..n {
            let i = sel.map(|s| s[k] as usize).unwrap_or(k);
            let rid = g.rowids[i];
            if !skip.is_empty() {
                while sk < skip.len() && skip[sk] < rid {
                    sk += 1;
                }
                if sk < skip.len() && skip[sk] == rid {
                    continue;
                }
            }
            let mut row: Row = vec![Value::Null; width];
            for &(p, c) in &want {
                row[c] = g.cols[p].get(i);
            }
            out.push((rid, row));
        }
        out
    }
}
