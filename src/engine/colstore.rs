//! Reading a table through its columnstore index (see docs/columnstore.md and `storage/colstore.rs`).
//!
//! A scan of a table that has a built columnstore (`CREATE [NONCLUSTERED | CLUSTERED] COLUMNSTORE INDEX`) reads its row groups instead
//! of the heap, when that gives the rows the heap would: for the isolation levels that hold no row locks (`READ UNCOMMITTED`,
//! `READ COMMITTED`, `SNAPSHOT`), without a hint that locks, and when the index covers the columns the query reads. Per row group:
//!
//! * a group whose smallest and largest value of a column rule out a simple condition (`col = 5`, `col < '2020-01-01'`, `col IS NULL`,
//!   `col BETWEEN a AND b`, `col IN (..)`) is skipped whole (*segment elimination*);
//! * the same conditions are evaluated over the column vectors, and only the rows that pass are made into rows (the whole `WHERE` is
//!   still evaluated on those, so the vector pass only has to keep every row the condition could keep);
//! * only the columns the query needs are made;
//! * for a grouped or aggregate query of the right shape the group's aggregates are worked out over the vectors and handed on as a
//!   part, with no rows made at all (batch mode, see colbatch.rs).
//!
//! Then the rows the groups do not hold are read from the heap as always: the rows written since the groups were built (the *dirty*
//! rows) and the ones added after (the *delta store*). The index is built when it is created or rebuilt, extended when the delta
//! store has grown to a row group, and built again when a fifth of its rows have been written.
use super::colbatch::{fold_group, FoldSpec, KPart};
use super::query::{Found, Schema};
use super::*;
use crate::catalog::Table;
use crate::storage::colstore::{self, CsData, CsView, RowGroup, SegData, Segment};
use crate::storage::{ReadView, RowId, ViewMode};
use std::cmp::Ordering;

/// Rows made into a batch before the query's conditions are evaluated on them.
const BATCH: usize = 8192;

/// A condition on one column that can be tested on a segment.
pub(super) enum Pred {
    Cmp(usize, BinOp, Value),
    In(usize, Vec<Value>),
    IsNull(usize, bool),
}

impl Pred {
    fn col(&self) -> usize {
        match self {
            Pred::Cmp(c, ..) | Pred::In(c, _) | Pred::IsNull(c, _) => *c,
        }
    }
}

fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        o => o,
    }
}

fn strip(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(x) => strip(x),
        e => e,
    }
}

fn literal(e: &Expr) -> Option<Value> {
    match strip(e) {
        Expr::Lit(v) if !v.is_null() => Some(v.clone()),
        Expr::Neg(x) => match literal(x)? {
            Value::Int(i) => i.checked_neg().map(Value::Int),
            Value::BigInt(i) => i.checked_neg().map(Value::BigInt),
            Value::Float(f) => Some(Value::Float(-f)),
            Value::Dec(d) => Some(Value::Dec(crate::decimal::Dec { m: -d.m, s: d.s })),
            _ => None,
        },
        _ => None,
    }
}

/// The conditions of `conds` that are simple enough for a segment: a column of the table (found in the schema of the scan) against
/// literals; and whether they are all of them (the conditions are then exactly what the segments decide). Strings are left to the row
/// evaluation, whose comparison knows the column's collation.
pub(super) fn extract_preds(sc: &Schema, t: &Table, conds: &[Expr], colmap: &[usize]) -> (Vec<Pred>, bool) {
    let mut out = vec![];
    if sc.cols.len() != t.columns.len() {
        return (out, false);
    }
    let col = |e: &Expr| -> Option<usize> {
        let Expr::Col(p) = strip(e) else { return None };
        match sc.find_parts(p) {
            Found::One(i) if colmap.contains(&i) => Some(i),
            _ => None,
        }
    };
    // (a string against a date or time column is read as a date or time by the comparison, whatever the collation; against any other
    // column it would be compared as a string, whose order is the collation's)
    let is_time = |c: usize| matches!(t.columns[c].ty, DataType::Date | DataType::DateTime | DataType::SmallDateTime | DataType::DateTime2(_) | DataType::DateTimeOffset(_) | DataType::Time(_));
    let ok_lit = |c: usize, v: &Value| match v {
        Value::Str(_) => is_time(c),
        Value::Bin(_) => false,
        _ => true,
    };
    fn walk<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
        match strip(e) {
            Expr::And(a, b) => {
                walk(a, out);
                walk(b, out);
            }
            e => out.push(e),
        }
    }
    let mut flat = vec![];
    for c in conds {
        walk(c, &mut flat);
    }
    let total = flat.len();
    let mut understood = 0usize;
    for e in flat {
        let before = out.len();
        match e {
            Expr::Binary(op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), l, r) => {
                if let (Some(c), Some(v)) = (col(l), literal(r)) {
                    if ok_lit(c, &v) {
                        out.push(Pred::Cmp(c, *op, v));
                    }
                } else if let (Some(v), Some(c)) = (literal(l), col(r)) {
                    if ok_lit(c, &v) {
                        out.push(Pred::Cmp(c, flip(*op), v));
                    }
                }
            }
            Expr::Between(x, lo, hi, false) => {
                if let (Some(c), Some(a), Some(b)) = (col(x), literal(lo), literal(hi)) {
                    if ok_lit(c, &a) && ok_lit(c, &b) {
                        out.push(Pred::Cmp(c, BinOp::Ge, a));
                        out.push(Pred::Cmp(c, BinOp::Le, b));
                    }
                }
            }
            Expr::InList(x, list, false) => {
                if let Some(c) = col(x) {
                    let vals: Option<Vec<Value>> = list.iter().map(literal).collect();
                    if let Some(vals) = vals {
                        if !vals.is_empty() && vals.iter().all(|v| ok_lit(c, v)) {
                            out.push(Pred::In(c, vals));
                        }
                    }
                }
            }
            Expr::IsNull(x, neg) => {
                if let Some(c) = col(x) {
                    out.push(Pred::IsNull(c, *neg));
                }
            }
            _ => {}
        }
        if out.len() > before {
            understood += 1;
        }
    }
    (out, understood == total)
}

fn holds(op: BinOp, o: Ordering) -> bool {
    match op {
        BinOp::Eq => o == Ordering::Equal,
        BinOp::Ne => o != Ordering::Equal,
        BinOp::Lt => o == Ordering::Less,
        BinOp::Le => o != Ordering::Greater,
        BinOp::Gt => o == Ordering::Greater,
        _ => o != Ordering::Less,
    }
}

/// Whether no row of the segment can satisfy `p` (true only when that is certain).
fn rules_out(seg: &Segment, p: &Pred) -> bool {
    match p {
        Pred::IsNull(_, false) => seg.null_count == 0,
        Pred::IsNull(_, true) => seg.null_count == seg.rows,
        Pred::Cmp(_, op, v) => {
            if seg.null_count == seg.rows {
                return true;
            }
            let (Some(lo), Some(hi)) = (&seg.min, &seg.max) else { return false };
            let (Ok(Some(a)), Ok(Some(b))) = (crate::convert::compare(lo, v), crate::convert::compare(hi, v)) else { return false };
            match op {
                BinOp::Eq => a == Ordering::Greater || b == Ordering::Less,
                BinOp::Ne => a == Ordering::Equal && b == Ordering::Equal,
                BinOp::Lt => a != Ordering::Less,
                BinOp::Le => a == Ordering::Greater,
                BinOp::Gt => b != Ordering::Greater,
                _ => b == Ordering::Less,
            }
        }
        Pred::In(_, vals) => {
            if seg.null_count == seg.rows {
                return true;
            }
            let (Some(lo), Some(hi)) = (&seg.min, &seg.max) else { return false };
            vals.iter().all(|v| match (crate::convert::compare(lo, v), crate::convert::compare(hi, v)) {
                (Ok(Some(a)), Ok(Some(b))) => a == Ordering::Greater || b == Ordering::Less,
                _ => false,
            })
        }
    }
}

/// Clear the bits of `mask` of the rows of `seg` that do not satisfy `p`. False when the condition cannot be decided here (a comparison
/// that raises an error): the mask is left as it is, for the row evaluation to raise.
fn apply(seg: &Segment, p: &Pred, mask: &mut [bool]) -> bool {
    let n = seg.rows;
    match p {
        Pred::IsNull(_, neg) => {
            for i in 0..n {
                if mask[i] && seg.is_null(i) != !*neg {
                    mask[i] = false;
                }
            }
            true
        }
        Pred::Cmp(_, op, v) => {
            // integers against an integer
            if let (SegData::Int(xs, _), Value::Int(_) | Value::BigInt(_)) = (&seg.data, v) {
                let lit = match v {
                    Value::Int(i) => *i as i64,
                    Value::BigInt(i) => *i,
                    _ => unreachable!(),
                };
                for i in 0..n {
                    if mask[i] && (seg.is_null(i) || !holds(*op, xs[i].cmp(&lit))) {
                        mask[i] = false;
                    }
                }
                return true;
            }
            if let (SegData::Float(xs), Value::Float(l)) = (&seg.data, v) {
                if l.is_nan() {
                    return false;
                }
                for i in 0..n {
                    if mask[i] && (seg.is_null(i) || !holds(*op, xs[i].partial_cmp(l).unwrap_or(Ordering::Equal))) {
                        mask[i] = false;
                    }
                }
                return true;
            }
            if let (SegData::Dec(xs, s), Value::Dec(l)) = (&seg.data, v) {
                if *s == l.s {
                    for i in 0..n {
                        if mask[i] && (seg.is_null(i) || !holds(*op, xs[i].cmp(&l.m))) {
                            mask[i] = false;
                        }
                    }
                    return true;
                }
            }
            // anything else: the engine's own comparison, once for each distinct value when the segment has a dictionary
            if let SegData::Dict { dict, codes } = &seg.data {
                let mut keep = Vec::with_capacity(dict.len());
                for d in dict {
                    match crate::convert::compare(d, v) {
                        Ok(Some(o)) => keep.push(holds(*op, o)),
                        _ => return false,
                    }
                }
                for i in 0..n {
                    if mask[i] && (seg.is_null(i) || !keep[codes[i] as usize]) {
                        mask[i] = false;
                    }
                }
                return true;
            }
            let mut res = Vec::with_capacity(n);
            for i in 0..n {
                if !mask[i] || seg.is_null(i) {
                    res.push(false);
                    continue;
                }
                match crate::convert::compare(&seg.get(i), v) {
                    Ok(Some(o)) => res.push(holds(*op, o)),
                    _ => return false,
                }
            }
            for i in 0..n {
                if !res[i] {
                    mask[i] = false;
                }
            }
            true
        }
        Pred::In(_, vals) => {
            let test = |x: &Value| -> Option<bool> {
                let mut any = false;
                for v in vals {
                    match crate::convert::compare(x, v) {
                        Ok(Some(Ordering::Equal)) => any = true,
                        Ok(_) => {}
                        Err(_) => return None,
                    }
                }
                Some(any)
            };
            if let SegData::Dict { dict, codes } = &seg.data {
                let mut keep = Vec::with_capacity(dict.len());
                for d in dict {
                    match test(d) {
                        Some(b) => keep.push(b),
                        None => return false,
                    }
                }
                for i in 0..n {
                    if mask[i] && (seg.is_null(i) || !keep[codes[i] as usize]) {
                        mask[i] = false;
                    }
                }
                return true;
            }
            let mut res = Vec::with_capacity(n);
            for i in 0..n {
                if !mask[i] || seg.is_null(i) {
                    res.push(false);
                    continue;
                }
                match test(&seg.get(i)) {
                    Some(b) => res.push(b),
                    None => return false,
                }
            }
            for i in 0..n {
                if !res[i] {
                    mask[i] = false;
                }
            }
            true
        }
    }
}

/// The positions in the group of the rows that pass the simple conditions (`None`: all of them; `Some(vec![])`: none), and whether the
/// conditions were all decided (when not, the positions are a superset of the rows that pass).
pub(super) fn select(data: &CsData, g: &RowGroup, preds: &[Pred]) -> (Option<Vec<u32>>, bool) {
    if preds.is_empty() {
        return (None, true);
    }
    let pos = |c: usize| data.colmap.iter().position(|x| *x == c);
    for p in preds {
        if let Some(k) = pos(p.col()) {
            if rules_out(&g.cols[k], p) {
                return (Some(vec![]), true);
            }
        }
    }
    let mut mask = vec![true; g.rows()];
    let mut exact = true;
    for p in preds {
        match pos(p.col()) {
            Some(k) => exact &= apply(&g.cols[k], p, &mut mask),
            None => exact = false,
        }
    }
    if mask.iter().all(|b| *b) {
        return (None, exact);
    }
    (Some(mask.iter().enumerate().filter(|(_, b)| **b).map(|(i, _)| i as u32).collect()), exact)
}

/// One row group's share of a scan: all of its rows, some of them, or none.
enum Job {
    Skip,
    /// positions of the rows to take (`None`: all), and whether the conditions were decided exactly
    Take(Option<Vec<u32>>, bool),
}

impl<'a> Engine<'a> {
    /// The columns a columnstore index of `t` is of (all of them for a clustered one), when it is there to be read.
    pub(super) fn columnstore_cols(t: &Table) -> Option<Vec<usize>> {
        let d = t.columnstore.as_ref().filter(|d| !d.disabled)?;
        Some(if d.clustered { (0..t.columns.len()).collect() } else { d.cols.clone() })
    }

    /// Build the columnstore of `loc` from its heap (at `CREATE`, `REBUILD`); `full`: all of it, not just the rows after what it holds.
    pub(super) fn columnstore_build(&mut self, loc: &Loc, full: bool) -> Result<()> {
        let Some(t) = self.table_ref(loc) else { return Ok(()) };
        let Some(colmap) = Self::columnstore_cols(t) else { return Ok(()) };
        let (data, width) = (t.data.clone(), t.columns.len());
        // (a build that another session is making is as good as this one)
        data.columnstore_build(&colmap, width, full);
        Ok(())
    }

    /// The columnstore of `t` for a scan with `view` that reads the columns `needed` (`None`: all), built or brought up to date first
    /// when it is time to; `None` when the scan has to read the heap.
    pub(super) fn columnstore_for_scan(&mut self, t: &Table, view: &ReadView, needed: Option<&[bool]>) -> Option<CsView> {
        let colmap = Self::columnstore_cols(t)?;
        let width = t.columns.len();
        let covers = match needed {
            Some(n) if n.len() == width => n.iter().enumerate().all(|(c, w)| !*w || colmap.contains(&c)),
            _ => colmap.len() == width,
        };
        if !covers {
            return None;
        }
        let info = t.data.columnstore_info();
        if !info.built {
            // (a table that has no index built is built here, when it is big enough to be worth it: after a restart)
            if t.data.slot_count() < colstore::min_rows() || !t.data.columnstore_build(&colmap, width, true) {
                return None;
            }
        } else if info.dirty > info.rows / 5 + 16 {
            t.data.columnstore_build(&colmap, width, true);
        } else if info.delta >= colstore::rowgroup_rows() {
            t.data.columnstore_build(&colmap, width, false);
        }
        let cs = match t.data.columnstore_view(&colmap, width) {
            Some(cs) => cs,
            // (the table has changed shape since it was built)
            None if info.built => {
                t.data.columnstore_build(&colmap, width, true);
                t.data.columnstore_view(&colmap, width)?
            }
            None => return None,
        };
        // a snapshot older than the newest row of the groups sees other versions of it
        if let ViewMode::Snapshot(s) = view.mode {
            if cs.data.max_ts > s {
                return None;
            }
        }
        Some(cs)
    }

    /// Feed `f` the rows of the row groups of `cs` that pass the conditions, then the dirty rows from the heap. The rows added after
    /// the build are the caller's to read from the heap (`cs.data.upto`). With `fold`, a row group of the right shape is handed to its
    /// callback as a part (its groups and their aggregates) instead of as rows. False when a callback stopped the scan.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn scan_columnar(
        &mut self,
        db: &str,
        t: &Table,
        iso: Isolation,
        cs: &CsView,
        needed: Option<&[bool]>,
        flt: Option<(&Schema, &[Expr])>,
        mut fold: Option<(&FoldSpec, &mut dyn FnMut(&mut Self, KPart) -> Result<bool>)>,
        f: &mut dyn FnMut(&mut Self, Vec<(RowId, Row)>) -> Result<bool>,
    ) -> Result<bool> {
        let data = cs.data.clone();
        let preds = match (flt, &fold) {
            (_, Some(_)) => vec![],
            (Some((sc, es)), None) => extract_preds(sc, t, es, &data.colmap).0,
            _ => vec![],
        };
        let (batch, dop) = self.par_batch(t, flt);
        let threads = self.dop().clamp(1, 16);
        let batch_mode = fold.is_some();
        let (mut skipped, mut read, mut folded) = (0usize, 0usize, 0usize);
        let mut gi = 0usize;
        while gi < data.groups.len() {
            // a wave of groups: the selection of each, then the parts of those that fold, on several threads
            let end = if fold.is_some() { (gi + threads).min(data.groups.len()) } else { gi + 1 };
            let mut jobs: Vec<Job> = Vec::with_capacity(end - gi);
            for g in &data.groups[gi..end] {
                let (sel, exact) = match &fold {
                    Some((spec, _)) => select(&data, g, &spec.preds),
                    None => select(&data, g, &preds),
                };
                jobs.push(match sel {
                    Some(s) if s.is_empty() => Job::Skip,
                    sel => Job::Take(sel, exact),
                });
            }
            let skips: Vec<&[RowId]> = data.groups[gi..end]
                .iter()
                .map(|g| {
                    let (lo, hi) = (g.rowids[0], g.rowids[g.rows() - 1]);
                    &cs.dirty[cs.dirty.partition_point(|r| *r < lo)..cs.dirty.partition_point(|r| *r <= hi)]
                })
                .collect();
            let mut parts: Vec<Option<KPart>> = (gi..end).map(|_| None).collect();
            if let Some((spec, _)) = &fold {
                let work: Vec<(usize, Option<Vec<u32>>)> = jobs
                    .iter()
                    .enumerate()
                    .filter_map(|(k, j)| match j {
                        Job::Take(sel, true) => {
                            let g = &data.groups[gi + k];
                            // (the rows written since the build are read from the heap: not folded here)
                            let positions = match (sel, skips[k].is_empty()) {
                                (None, true) => None,
                                (sel, _) => {
                                    let all: Vec<u32>;
                                    let base: &[u32] = match sel {
                                        Some(s) => s,
                                        None => {
                                            all = (0..g.rows() as u32).collect();
                                            &all
                                        }
                                    };
                                    let mut out = Vec::with_capacity(base.len());
                                    let mut sk = 0usize;
                                    for &p in base {
                                        let rid = g.rowids[p as usize];
                                        while sk < skips[k].len() && skips[k][sk] < rid {
                                            sk += 1;
                                        }
                                        if sk < skips[k].len() && skips[k][sk] == rid {
                                            continue;
                                        }
                                        out.push(p);
                                    }
                                    Some(out)
                                }
                            };
                            Some((k, positions))
                        }
                        _ => None,
                    })
                    .collect();
                let spec: &FoldSpec = spec;
                let d: &CsData = &data;
                let results: Vec<(usize, Option<KPart>)> = if work.len() > 1 && threads > 1 {
                    std::thread::scope(|s| {
                        let hs: Vec<_> = work.iter().map(|(k, pos)| s.spawn(move || (*k, fold_group(d, &d.groups[gi + *k], pos.as_deref(), spec)))).collect();
                        hs.into_iter().map(|h| h.join().expect("a fold of a row group panicked")).collect()
                    })
                } else {
                    work.iter().map(|(k, pos)| (*k, fold_group(d, &d.groups[gi + *k], pos.as_deref(), spec))).collect()
                };
                for (k, r) in results {
                    parts[k] = r;
                }
            }
            for (k, job) in jobs.into_iter().enumerate() {
                let g = &data.groups[gi + k];
                let Job::Take(sel, _) = job else {
                    skipped += 1;
                    continue;
                };
                read += 1;
                if let (Some(part), Some((_, cb))) = (parts[k].take(), fold.as_mut()) {
                    folded += 1;
                    if !cb(self, part)? {
                        return Ok(false);
                    }
                    continue;
                }
                // the rows of the group
                let n = sel.as_ref().map(|s| s.len()).unwrap_or(g.rows());
                let mut at = 0usize;
                while at < n {
                    let end = (at + BATCH).min(n);
                    let positions: Vec<u32> = match &sel {
                        Some(s) => s[at..end].to_vec(),
                        None => (at as u32..end as u32).collect(),
                    };
                    let rows = data.rows_of(g, needed, Some(&positions), skips[k]);
                    if !self.emit_batch(rows, flt, batch, dop, f)? {
                        return Ok(false);
                    }
                    at = end;
                }
            }
            gi = end;
        }
        self.ses.plan.push(format!(
            "columnstore scan of {}: {} of {} row group(s) read ({} skipped by their bounds or conditions){}, {} written row(s) and the rows after the build read from the heap",
            t.name,
            read,
            data.groups.len(),
            skipped,
            if batch_mode { format!(", batch mode: the aggregates of {} computed over column vectors", folded) } else { String::new() },
            cs.dirty.len()
        ));
        for part in cs.dirty.chunks(512) {
            let rows = self.read_rids(db, t, iso, part, flt)?;
            if !rows.is_empty() && !f(self, rows)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The rows that pass the conditions, to `f`.
    fn emit_batch(&mut self, rows: Vec<(RowId, Row)>, flt: Option<(&Schema, &[Expr])>, batch: usize, dop: usize, f: &mut dyn FnMut(&mut Self, Vec<(RowId, Row)>) -> Result<bool>) -> Result<bool> {
        let rows = match flt {
            Some((sc, es)) if batch > 0 && rows.len() >= 1024 => self.par_filter(sc, rows, es, dop)?,
            Some((sc, es)) => self.filter_pushed(sc, rows, es)?,
            None => rows,
        };
        if rows.is_empty() {
            return Ok(true);
        }
        f(self, rows)
    }
}
