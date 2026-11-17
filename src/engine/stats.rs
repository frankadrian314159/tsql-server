//! Column statistics for the optimizer: how many rows a condition on a column keeps.
//!
//! The optimizer used to guess that every condition it could not answer from an index keeps a third of the rows, which orders joins
//! badly (a filter that keeps 0.1% of a table looks the same as one that keeps 90%). A table's statistics are taken from a sample of
//! its rows, spread over the whole table, the first time a plan needs them, and kept until the table has changed in size by a
//! fifth. For each column: the share of NULLs, the number of distinct values (the estimate of Charikar et al., which scales the values
//! seen once), the most common values with their shares, and an equi-depth histogram (the bounds of equal-sized slices of the sorted
//! sample). A condition on one column then has a selectivity: equality is the share of a common value, or what is left of the rows
//! divided by the other distinct values; a range is read off the histogram; `IN`, `BETWEEN`, `IS NULL`, `LIKE 'prefix%'`, `AND`, `OR`
//! and `NOT` are built from those. A condition that is none of these keeps the old guess.
//!
//! The statistics are taken again when the table has changed in size by a fifth, or when more than `STALE_BASE` + a fifth of its
//! rows have been written since the sample (`TableData::modifications`), so an `UPDATE` that changes the values of a column
//! without changing the number of rows makes them stale too (as SQL Server's modification counter does).
use super::*;
use crate::catalog::Table;
use crate::convert::compare;
use std::cmp::Ordering;

/// Tables with fewer slots than this are not sampled (the old guess is good enough for a small table, and the plan is cheap anyway).
const MIN_SLOTS: usize = 2000;
/// Blocks of consecutive slots taken, and slots per block: the sample is at most `BLOCKS * BLOCK` rows.
const BLOCKS: usize = 64;
const BLOCK: usize = 64;
/// Slices of the histogram, and most common values kept.
const BUCKETS: usize = 32;
const MCV: usize = 12;
/// Row versions written since the sample, beyond a fifth of the table, that make the statistics stale.
const STALE_BASE: f64 = 500.0;

pub struct ColStats {
    /// Share of rows that are NULL.
    pub nulls: f64,
    /// Distinct non-NULL values in the table (estimated).
    pub ndv: f64,
    /// The most common values (in the sample), with their share of all rows.
    pub mcv: Vec<(Value, f64)>,
    /// The bounds of the slices: `bounds[0]` is the smallest sampled value, the last the largest.
    pub bounds: Vec<Value>,
}

pub struct TableStats {
    /// Slots of the heap when the sample was taken.
    pub slots: usize,
    /// `TableData::modifications` when the sample was taken.
    pub mods: u64,
    pub cols: Vec<ColStats>,
}

fn key(v: &Value) -> String {
    crate::convert::key_of(std::slice::from_ref(v))
}

/// The statistics of a table taken from `rows` (a sample) of a heap with `slots` slots.
fn build(rows: &[Vec<Value>], ncols: usize, slots: usize, mods: u64) -> TableStats {
    let n = rows.len().max(1) as f64;
    let total = slots as f64;
    let mut cols = vec![];
    for c in 0..ncols {
        let mut vals: Vec<&Value> = rows.iter().filter_map(|r| r.get(c)).filter(|v| !v.is_null()).collect();
        let nulls = 1.0 - vals.len() as f64 / n;
        // value counts
        let mut counts: HashMap<String, (usize, &Value)> = HashMap::new();
        for v in &vals {
            counts.entry(key(v)).and_modify(|e| e.0 += 1).or_insert((1, *v));
        }
        let d = counts.len() as f64;
        let f1 = counts.values().filter(|e| e.0 == 1).count() as f64;
        let m = vals.len().max(1) as f64;
        // (a table that was read whole has exactly the distinct values it has)
        let ndv = if rows.len() as f64 >= total { d } else { ((total / n).sqrt() * f1 + (d - f1)).clamp(d, total) };
        let mut common: Vec<(usize, &Value)> = counts.values().filter(|e| e.0 >= 2).map(|e| (e.0, e.1)).collect();
        common.sort_by(|a, b| b.0.cmp(&a.0));
        common.truncate(MCV);
        let mcv = common.into_iter().map(|(k, v)| (v.clone(), k as f64 / m * (1.0 - nulls))).collect();
        vals.sort_by(|a, b| compare(a, b).ok().flatten().unwrap_or(Ordering::Equal));
        let mut bounds = vec![];
        if !vals.is_empty() {
            for i in 0..=BUCKETS {
                let at = (i * (vals.len() - 1)) / BUCKETS;
                bounds.push(vals[at].clone());
            }
        }
        cols.push(ColStats { nulls, ndv, mcv, bounds });
    }
    TableStats { slots, mods, cols }
}

impl ColStats {
    /// Share of the non-NULL values that are `<= v` (`strict`: `< v`), read off the histogram.
    fn below(&self, v: &Value, strict: bool) -> Option<f64> {
        let b = &self.bounds;
        if b.len() < 2 {
            return None;
        }
        let steps = (b.len() - 1) as f64;
        // the first bound that is not below v
        let mut lo = 0usize;
        let mut hi = b.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            match compare(&b[mid], v).ok()?? {
                Ordering::Less => lo = mid + 1,
                Ordering::Equal if !strict => lo = mid + 1,
                _ => hi = mid,
            }
        }
        // `lo` bounds are below (or equal, when not strict): v lies in the slice before bound lo
        if lo == 0 {
            return Some(0.0);
        }
        if lo >= b.len() {
            return Some(1.0);
        }
        // interpolate within the slice for numbers
        let within = match (num(&b[lo - 1]), num(&b[lo]), num(v)) {
            (Some(a), Some(c), Some(x)) if c > a => ((x - a) / (c - a)).clamp(0.0, 1.0),
            _ => 0.5,
        };
        Some((((lo - 1) as f64 + within) / steps).clamp(0.0, 1.0))
    }

    /// Share of all rows equal to `v`.
    fn equal(&self, v: &Value) -> f64 {
        for (m, f) in &self.mcv {
            if matches!(compare(m, v), Ok(Some(Ordering::Equal))) {
                return *f;
            }
        }
        let common: f64 = self.mcv.iter().map(|m| m.1).sum();
        let others = (self.ndv - self.mcv.len() as f64).max(1.0);
        ((1.0 - self.nulls - common).max(0.0) / others).max(0.0)
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::BigInt(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Dec(d) => Some(d.m as f64 / crate::decimal::POW10[d.s as usize] as f64),
        Value::Date(d) => Some(d.num_days_from_ce() as f64),
        Value::DateTime(t, _) => Some(t.and_utc().timestamp() as f64 / 86400.0),
        _ => None,
    }
}

use chrono::Datelike;

impl<'a> Engine<'a> {
    /// The statistics of `t`, sampled now when there are none or they are stale (see the module comment); None for a table that is
    /// small, temporary or cannot be sampled.
    fn table_stats(&mut self, t: &Table) -> Option<Arc<TableStats>> {
        let slots = t.data.slot_count();
        if slots < MIN_SLOTS || t.is_temp || !t.data.is_attached() {
            return None;
        }
        let id = t.data.heap_id();
        // (read before the sample, so writes made while it is taken count towards the next one)
        let mods = t.data.modifications();
        if let Some(s) = self.srv.stats.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            if fresh(s, slots, mods) {
                return Some(s.clone());
            }
        }
        let view = crate::storage::ReadView::dirty();
        let max = t.data.max_rowid()? as usize;
        let stride = (max / BLOCKS).max(1);
        let mut rows: Vec<Vec<Value>> = vec![];
        for i in 0..BLOCKS {
            let after = if i == 0 { None } else { Some((i * stride) as crate::storage::RowId) };
            let c = t.data.scan_chunk(&view, after, BLOCK, false);
            rows.extend(c.rows.into_iter().map(|(_, r)| r.to_vec()));
        }
        if rows.is_empty() {
            return None;
        }
        let st = Arc::new(build(&rows, t.columns.len(), slots, mods));
        self.srv.stats.lock().unwrap_or_else(|e| e.into_inner()).insert(id, st.clone());
        Some(st)
    }

    /// The share of the rows of `t` that the conditions keep, from the statistics; None when there are none (the caller guesses).
    pub fn pushed_selectivity(&mut self, t: &Table, alias: &str, conj: &[&Expr]) -> Option<f64> {
        let st = self.table_stats(t)?;
        let mut sel = 1.0f64;
        for c in conj {
            sel *= self.cond_selectivity(&st, t, alias, c).unwrap_or(0.33);
        }
        Some(sel.clamp(0.0, 1.0))
    }

    /// The value of a constant expression (not NULL: a comparison with NULL has no selectivity to read off a histogram).
    fn stat_const(&mut self, e: &Expr) -> Option<Value> {
        self.const_value(e).filter(|v| !v.is_null())
    }

    fn col_of(&self, t: &Table, alias: &str, e: &Expr) -> Option<usize> {
        Self::pred_col(t, alias, e)
    }

    /// The one column of `t` a condition is on (`col op x`, `col BETWEEN ..`, `col IN (..)`, `col IS NULL`, `col LIKE ..`, and
    /// `AND` / `OR` / `NOT` of conditions on that same column); None when it is on no column or several. Used to tell which of a
    /// table's conditions an index access already answers (`Engine::estimate_table`).
    pub fn cond_col(t: &Table, alias: &str, e: &Expr) -> Option<usize> {
        let one = |a: &Expr, b: &Expr| match (Self::pred_col(t, alias, a), Self::pred_col(t, alias, b)) {
            (Some(c), None) | (None, Some(c)) => Some(c),
            _ => None,
        };
        match e {
            Expr::Paren(x) | Expr::Not(x) => Self::cond_col(t, alias, x),
            Expr::And(a, b) | Expr::Or(a, b) => match (Self::cond_col(t, alias, a), Self::cond_col(t, alias, b)) {
                (Some(x), Some(y)) if x == y => Some(x),
                _ => None,
            },
            Expr::Binary(_, a, b) => one(a, b),
            Expr::Between(a, ..) | Expr::InList(a, ..) | Expr::IsNull(a, _) | Expr::Like(a, ..) => Self::pred_col(t, alias, a),
            _ => None,
        }
    }

    fn cond_selectivity(&mut self, st: &TableStats, t: &Table, alias: &str, e: &Expr) -> Option<f64> {
        match e {
            Expr::Paren(x) => self.cond_selectivity(st, t, alias, x),
            Expr::And(a, b) => Some(self.cond_selectivity(st, t, alias, a).unwrap_or(0.33) * self.cond_selectivity(st, t, alias, b).unwrap_or(0.33)),
            Expr::Or(a, b) => {
                let (x, y) = (self.cond_selectivity(st, t, alias, a).unwrap_or(0.33), self.cond_selectivity(st, t, alias, b).unwrap_or(0.33));
                Some((x + y - x * y).clamp(0.0, 1.0))
            }
            Expr::Not(a) => self.cond_selectivity(st, t, alias, a).map(|s| 1.0 - s),
            Expr::IsNull(a, neg) => {
                let cs = st.cols.get(self.col_of(t, alias, a)?)?;
                Some(if *neg { 1.0 - cs.nulls } else { cs.nulls })
            }
            Expr::Binary(op, a, b) if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) => {
                // column op constant, or constant op column
                let (ci, v, op) = match (self.col_of(t, alias, a), self.col_of(t, alias, b)) {
                    (Some(c), None) => (c, self.stat_const(b)?, *op),
                    (None, Some(c)) => (c, self.stat_const(a)?, flip(*op)),
                    _ => return None,
                };
                let cs = st.cols.get(ci)?;
                let nn = 1.0 - cs.nulls;
                match op {
                    BinOp::Eq => Some(cs.equal(&v)),
                    BinOp::Ne => Some((nn - cs.equal(&v)).max(0.0)),
                    BinOp::Lt => Some(cs.below(&v, true)? * nn),
                    BinOp::Le => Some(cs.below(&v, false)? * nn),
                    BinOp::Gt => Some((1.0 - cs.below(&v, false)?) * nn),
                    _ => Some((1.0 - cs.below(&v, true)?) * nn),
                }
            }
            Expr::Between(a, lo, hi, neg) => {
                let ci = self.col_of(t, alias, a)?;
                let (lo, hi) = (self.stat_const(lo)?, self.stat_const(hi)?);
                let cs = st.cols.get(ci)?;
                let inside = ((cs.below(&hi, false)? - cs.below(&lo, true)?).max(0.0)) * (1.0 - cs.nulls);
                Some(if *neg { (1.0 - cs.nulls - inside).max(0.0) } else { inside })
            }
            Expr::InList(a, items, neg) => {
                let ci = self.col_of(t, alias, a)?;
                let cs = st.cols.get(ci)?;
                let mut s = 0.0;
                for it in items {
                    s += cs.equal(&self.stat_const(it)?);
                }
                let s = s.min(1.0 - cs.nulls);
                Some(if *neg { (1.0 - cs.nulls - s).max(0.0) } else { s })
            }
            Expr::Like(a, p, None, neg) => {
                let ci = self.col_of(t, alias, a)?;
                let cs = st.cols.get(ci)?;
                let Value::Str(pat) = self.stat_const(p)? else { return None };
                let s = match pat.find(['%', '_']) {
                    // 'abc%': the strings from 'abc' up to (not including) the next prefix
                    Some(i) if i > 0 && pat[i..] == *"%" => {
                        let lo = Value::Str(pat[..i].to_string());
                        let mut hi = pat[..i].to_string();
                        let last = hi.pop()?;
                        hi.push(char::from_u32(last as u32 + 1)?);
                        ((cs.below(&Value::Str(hi), true)? - cs.below(&lo, true)?).max(0.0)) * (1.0 - cs.nulls)
                    }
                    // no wildcard at all: equality
                    None => cs.equal(&Value::Str(pat)),
                    // a leading wildcard: no way to tell; the share of a typical pattern
                    _ => 0.1 * (1.0 - cs.nulls),
                };
                Some(if *neg { (1.0 - cs.nulls - s).max(0.0) } else { s })
            }
            _ => None,
        }
    }
}

/// Whether statistics sampled as `s` still describe a table that now has `slots` slots and `mods` versions written.
fn fresh(s: &TableStats, slots: usize, mods: u64) -> bool {
    let base = s.slots as f64;
    // (a counter below the sampled one means the table was decoded again since: the statistics may be of other rows)
    let Some(written) = mods.checked_sub(s.mods) else { return false };
    (base - slots as f64).abs() <= 0.2 * base && (written as f64) <= STALE_BASE + 0.2 * base
}

fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}
