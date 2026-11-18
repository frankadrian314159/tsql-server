//! Batch mode: aggregates and groups worked out over the column vectors of a row group, not row by row.
//!
//! For a query of the shape `SELECT group columns, COUNT/SUM/AVG/MIN/MAX of columns FROM t WHERE simple conditions GROUP BY group columns`
//! over a table with a columnstore index, each row group is turned into a *part*: its groups, each with the state of every aggregate
//! (`AggAcc`) over the rows of the group that pass the conditions. The parts of the row groups (worked out on several threads, a wave of
//! groups at a time) are merged in the order of the row groups into the streaming executor's groups with `AggAcc::merge`, which is how
//! it merges the parts of its parallel workers, so the query's `HAVING`, `ORDER BY`, `TOP` and select list are what they always were.
//!
//! What is worked out here is exactly what a row-by-row fold gives, or it is not worked out here: a group of rows whose sums could
//! overflow (judged from the bounds of the segment), a column that is not stored as plain numbers or a dictionary, a condition that the
//! segment cannot decide, is left to the rows, which are read from the group and folded one by one as before.
use super::aggacc::AggAcc;
use super::colstore::Pred;
use crate::storage::colstore::{CsData, IntKind, RowGroup, SegData, Segment};
use crate::types::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AggKind {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

pub(super) struct AggSpec {
    pub kind: AggKind,
    /// the table column the aggregate is of (not used for `COUNT(*)`)
    pub col: usize,
}

/// What a query asks of the row groups.
pub(super) struct FoldSpec {
    /// the table columns of the `GROUP BY` (none: one group)
    pub group_cols: Vec<usize>,
    pub aggs: Vec<AggSpec>,
    /// the whole `WHERE`, as conditions on segments
    pub preds: Vec<Pred>,
    /// strings compare by their bytes (no collation is in use), so `MIN` and `MAX` of a string column can be taken here
    pub plain_strings: bool,
}

pub(super) struct KGroup {
    /// the values of the group columns
    pub key: Vec<Value>,
    /// the position of the group's first row among the rows of the part that passed the conditions
    pub first: usize,
    pub accs: Vec<AggAcc>,
}

pub(super) struct KPart {
    pub groups: Vec<KGroup>,
    /// the rows of the row group that passed the conditions
    pub rows: usize,
}

/// A value of a segment as something that tells groups apart: equal values give equal codes within one segment.
fn code(seg: &Segment, i: usize) -> Option<i128> {
    if seg.is_null(i) {
        return Some(i128::MIN);
    }
    Some(match &seg.data {
        SegData::Nulls => i128::MIN,
        SegData::Int(v, _) => v[i] as i128,
        SegData::Float(v) => v[i].to_bits() as i128,
        SegData::Dec(v, _) => v[i],
        SegData::Dict { codes, .. } => codes[i] as i128,
        SegData::Vals(_) => return None,
    })
}

/// The group of each selected row (numbered from 0 in the order they are met), how many groups, and the first row of each.
fn group_ids(segs: &[&Segment], rows: &[u32]) -> Option<(Vec<u32>, usize, Vec<u32>)> {
    let m = rows.len();
    let mut gid: Vec<u32> = Vec::with_capacity(m);
    let mut first: Vec<u32> = vec![];
    if segs.is_empty() {
        if m == 0 {
            return Some((gid, 0, first));
        }
        gid.resize(m, 0);
        first.push(0);
        return Some((gid, 1, first));
    }
    if segs.len() == 1 {
        let s = segs[0];
        if let SegData::Dict { dict, codes } = &s.data {
            let mut slot = vec![u32::MAX; dict.len() + 1];
            for (k, &i) in rows.iter().enumerate() {
                let i = i as usize;
                let c = if s.is_null(i) { dict.len() } else { codes[i] as usize };
                let mut id = slot[c];
                if id == u32::MAX {
                    id = first.len() as u32;
                    slot[c] = id;
                    first.push(k as u32);
                }
                gid.push(id);
            }
            let n = first.len();
            return Some((gid, n, first));
        }
        let mut map: HashMap<i128, u32> = HashMap::new();
        for (k, &i) in rows.iter().enumerate() {
            let c = code(s, i as usize)?;
            let id = match map.get(&c) {
                Some(id) => *id,
                None => {
                    let id = first.len() as u32;
                    map.insert(c, id);
                    first.push(k as u32);
                    id
                }
            };
            gid.push(id);
        }
        let n = first.len();
        return Some((gid, n, first));
    }
    let mut map: HashMap<Vec<i128>, u32> = HashMap::new();
    let mut key: Vec<i128> = Vec::with_capacity(segs.len());
    for (k, &i) in rows.iter().enumerate() {
        key.clear();
        for s in segs {
            key.push(code(s, i as usize)?);
        }
        let id = match map.get(&key) {
            Some(id) => *id,
            None => {
                let id = first.len() as u32;
                map.insert(key.clone(), id);
                first.push(k as u32);
                id
            }
        };
        gid.push(id);
    }
    let n = first.len();
    Some((gid, n, first))
}

/// The largest absolute value in a segment of whole numbers or decimals, from its bounds.
fn max_abs(seg: &Segment) -> Option<f64> {
    let f = |v: &Value| -> Option<f64> {
        match v {
            Value::Int(i) => Some((*i as f64).abs()),
            Value::BigInt(i) => Some((*i as f64).abs()),
            Value::Bit(_) => Some(1.0),
            Value::Dec(d) => Some((d.m as f64).abs()),
            _ => None,
        }
    };
    match (&seg.min, &seg.max) {
        (Some(a), Some(b)) => Some(f(a)?.max(f(b)?)),
        (None, None) => Some(0.0),
        _ => None,
    }
}

fn run_agg(spec: &AggSpec, seg: Option<&Segment>, rows: &[u32], gid: &[u32], ng: usize, plain_strings: bool) -> Option<Vec<AggAcc>> {
    let m = rows.len();
    if spec.kind == AggKind::CountStar {
        let mut counts = vec![0u64; ng];
        for g in gid {
            counts[*g as usize] += 1;
        }
        return Some(counts.into_iter().map(|c| AggAcc::from_parts(c, None, false)).collect());
    }
    let seg = seg?;
    match spec.kind {
        AggKind::Count => {
            let mut counts = vec![0u64; ng];
            for (k, &i) in rows.iter().enumerate() {
                if !seg.is_null(i as usize) {
                    counts[gid[k] as usize] += 1;
                }
            }
            Some(counts.into_iter().map(|c| AggAcc::from_parts(c, None, true)).collect())
        }
        AggKind::Sum | AggKind::Avg => {
            // (a sum that could leave its type's range is worked out row by row, which raises the overflow where a serial run would)
            let bound = max_abs(seg)? * m as f64;
            let mut counts = vec![0u64; ng];
            let mut sums = vec![0i128; ng];
            match &seg.data {
                SegData::Int(v, kind) => {
                    let limit = if *kind == IntKind::BigInt { 9.0e18 } else { 2.0e9 };
                    if bound > limit {
                        return None;
                    }
                    for (k, &i) in rows.iter().enumerate() {
                        let i = i as usize;
                        if !seg.is_null(i) {
                            let g = gid[k] as usize;
                            sums[g] += v[i] as i128;
                            counts[g] += 1;
                        }
                    }
                    let kind = *kind;
                    Some((0..ng).map(|g| AggAcc::from_parts(counts[g], (counts[g] > 0).then(|| if kind == IntKind::BigInt { Value::BigInt(sums[g] as i64) } else { Value::Int(sums[g] as i32) }), true)).collect())
                }
                SegData::Dec(v, s) => {
                    if bound > 9.0e37 {
                        return None;
                    }
                    for (k, &i) in rows.iter().enumerate() {
                        let i = i as usize;
                        if !seg.is_null(i) {
                            let g = gid[k] as usize;
                            sums[g] += v[i];
                            counts[g] += 1;
                        }
                    }
                    let s = *s;
                    Some((0..ng).map(|g| AggAcc::from_parts(counts[g], (counts[g] > 0).then(|| Value::Dec(crate::decimal::Dec { m: sums[g], s })), true)).collect())
                }
                SegData::Nulls => Some((0..ng).map(|_| AggAcc::from_parts(0, None, true)).collect()),
                _ => None,
            }
        }
        AggKind::Min | AggKind::Max => {
            let min = spec.kind == AggKind::Min;
            let mut counts = vec![0u64; ng];
            // the row (position in the segment) holding the best value of each group so far
            let mut best: Vec<u32> = vec![u32::MAX; ng];
            // `better(a, b)`: the value at row `a` is strictly better than the one at row `b`
            macro_rules! pick {
                ($better:expr) => {{
                    for (k, &i) in rows.iter().enumerate() {
                        let i = i as usize;
                        if seg.is_null(i) {
                            continue;
                        }
                        let g = gid[k] as usize;
                        counts[g] += 1;
                        let b = best[g];
                        if b == u32::MAX || ($better)(i, b as usize) {
                            best[g] = i as u32;
                        }
                    }
                }};
            }
            match &seg.data {
                SegData::Int(v, _) => pick!(|a: usize, b: usize| if min { v[a] < v[b] } else { v[a] > v[b] }),
                SegData::Float(v) => pick!(|a: usize, b: usize| if min { v[a] < v[b] } else { v[a] > v[b] }),
                SegData::Dec(v, _) => pick!(|a: usize, b: usize| if min { v[a] < v[b] } else { v[a] > v[b] }),
                SegData::Dict { dict, codes } => {
                    if dict.iter().any(|d| matches!(d, Value::Str(_))) && !plain_strings {
                        return None;
                    }
                    // the order of the distinct values, by the engine's own comparison
                    let mut order: Vec<usize> = (0..dict.len()).collect();
                    let mut failed = false;
                    order.sort_by(|a, b| match crate::convert::compare(&dict[*a], &dict[*b]) {
                        Ok(Some(o)) => o,
                        _ => {
                            failed = true;
                            std::cmp::Ordering::Equal
                        }
                    });
                    if failed {
                        return None;
                    }
                    let mut rank = vec![0u32; dict.len()];
                    let mut r = 0u32;
                    for (n, &d) in order.iter().enumerate() {
                        if n > 0 && crate::convert::compare(&dict[order[n - 1]], &dict[d]).ok().flatten() != Some(std::cmp::Ordering::Equal) {
                            r += 1;
                        }
                        rank[d] = r;
                    }
                    pick!(|a: usize, b: usize| {
                        let (ra, rb) = (rank[codes[a] as usize], rank[codes[b] as usize]);
                        if min { ra < rb } else { ra > rb }
                    })
                }
                SegData::Nulls => {}
                SegData::Vals(_) => return None,
            }
            Some((0..ng).map(|g| AggAcc::from_parts(counts[g], (best[g] != u32::MAX).then(|| seg.get(best[g] as usize)), true)).collect())
        }
        AggKind::CountStar => None,
    }
}

/// The part for the rows of group `g` at `positions` (all of its rows when `None`); `None` when the group has to be done row by row.
pub(super) fn fold_group(data: &CsData, g: &RowGroup, positions: Option<&[u32]>, spec: &FoldSpec) -> Option<KPart> {
    let rows: Vec<u32> = match positions {
        Some(p) => p.to_vec(),
        None => (0..g.rows() as u32).collect(),
    };
    let seg = |col: usize| -> Option<&Segment> { data.colmap.iter().position(|c| *c == col).map(|p| &g.cols[p]) };
    let gsegs: Vec<&Segment> = spec.group_cols.iter().map(|c| seg(*c)).collect::<Option<_>>()?;
    let (gid, ng, first) = group_ids(&gsegs, &rows)?;
    let mut per_agg: Vec<std::vec::IntoIter<AggAcc>> = vec![];
    for a in &spec.aggs {
        let s = if a.kind == AggKind::CountStar { None } else { Some(seg(a.col)?) };
        per_agg.push(run_agg(a, s, &rows, &gid, ng, spec.plain_strings)?.into_iter());
    }
    let mut groups = Vec::with_capacity(ng);
    for k in first.iter().take(ng) {
        let k = *k as usize;
        let key: Vec<Value> = gsegs.iter().map(|s| s.get(rows[k] as usize)).collect();
        let accs: Vec<AggAcc> = per_agg.iter_mut().map(|it| it.next().expect("one state per group")).collect();
        groups.push(KGroup { key, first: k, accs });
    }
    Some(KPart { groups, rows: rows.len() })
}
