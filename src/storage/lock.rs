//! Lock manager: multi-granularity locks (table and row), a wait-for graph and deadlock detection.
use super::{RowId, TxId};
use crate::convert::sort_cmp;
use crate::types::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    IS,
    IX,
    S,
    U,
    SIX,
    X,
}

impl Mode {
    pub fn compatible(self, other: Mode) -> bool {
        use Mode::*;
        match (self, other) {
            (IS, X) | (X, IS) => false,
            (IS, _) | (_, IS) => true,
            (IX, IX) => true,
            (IX, _) | (_, IX) => false,
            (S, S) | (S, U) | (U, S) => true,
            _ => false,
        }
    }

    /// Least mode covering both (for lock conversion).
    pub fn sup(self, o: Mode) -> Mode {
        use Mode::*;
        if self == o {
            return self;
        }
        match (self, o) {
            (X, _) | (_, X) => X,
            (SIX, _) | (_, SIX) => SIX,
            (IS, m) | (m, IS) => m,
            (IX, S) | (S, IX) => SIX,
            (IX, U) | (U, IX) => X,
            (S, U) | (U, S) => U,
            _ => X,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Res {
    Table(i32),
    Row(i32, RowId),
    /// A key range of one index of a table (see [`KeyRange`]); only reported by `snapshot`.
    Range(i32, usize),
}

/// A shared lock on a range of one index's key space, taken by SERIALIZABLE readers so that no other
/// transaction can insert a row whose key falls inside it (a phantom) before the reader commits.
///
/// Bounds are key prefixes compared column by column with SQL Server's ordering, so a seek on the first column of a
/// two-column index locks every key that starts with that value. `None` is unbounded. Readers never conflict with
/// each other; writers check the keys they are about to add with [`LockManager::range_conflicts`].
#[derive(Clone, Debug)]
pub struct KeyRange {
    pub tx: TxId,
    pub oid: i32,
    pub idx: usize,
    pub lo: Option<(Vec<Value>, bool)>,
    pub hi: Option<(Vec<Value>, bool)>,
}

fn cmp_prefix(key: &[Value], bound: &[Value]) -> Ordering {
    for (a, b) in key.iter().zip(bound.iter()) {
        let c = sort_cmp(a, b);
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

impl KeyRange {
    pub fn covers(&self, key: &[Value]) -> bool {
        let above = match &self.lo {
            None => true,
            Some((b, incl)) => match cmp_prefix(key, b) {
                Ordering::Greater => true,
                Ordering::Equal => *incl,
                Ordering::Less => false,
            },
        };
        above
            && match &self.hi {
                None => true,
                Some((b, incl)) => match cmp_prefix(key, b) {
                    Ordering::Less => true,
                    Ordering::Equal => *incl,
                    Ordering::Greater => false,
                },
            }
    }
}

#[derive(Default)]
pub struct LockManager {
    held: HashMap<Res, Vec<(TxId, Mode)>>,
    by_tx: HashMap<TxId, Vec<Res>>,
    waits: HashMap<TxId, Vec<TxId>>,
    ranges: Vec<KeyRange>,
}

impl LockManager {
    /// Transactions (other than `tx`) whose locks conflict with `mode` on `res`.
    pub fn conflicts(&self, tx: TxId, res: &Res, mode: Mode) -> Vec<TxId> {
        let want = match self.mode_of(tx, res) {
            Some(m) => m.sup(mode),
            None => mode,
        };
        match self.held.get(res) {
            None => vec![],
            Some(hs) => hs.iter().filter(|(t, m)| *t != tx && !m.compatible(want)).map(|(t, _)| *t).collect(),
        }
    }

    pub fn mode_of(&self, tx: TxId, res: &Res) -> Option<Mode> {
        self.held.get(res).and_then(|hs| hs.iter().find(|(t, _)| *t == tx).map(|(_, m)| *m))
    }

    /// Grant (or upgrade) a lock. `Err` lists the blocking transactions.
    pub fn acquire(&mut self, tx: TxId, res: &Res, mode: Mode) -> Result<(), Vec<TxId>> {
        let blockers = self.conflicts(tx, res, mode);
        if !blockers.is_empty() {
            return Err(blockers);
        }
        let hs = self.held.entry(res.clone()).or_default();
        match hs.iter_mut().find(|(t, _)| *t == tx) {
            Some(h) => h.1 = h.1.sup(mode),
            None => {
                hs.push((tx, mode));
                self.by_tx.entry(tx).or_default().push(res.clone());
            }
        }
        Ok(())
    }

    /// Record a shared key-range lock for `tx` (never blocks: readers do not conflict with readers).
    pub fn acquire_range(&mut self, r: KeyRange) {
        let same = |a: &Option<(Vec<Value>, bool)>, b: &Option<(Vec<Value>, bool)>| match (a, b) {
            (None, None) => true,
            (Some((x, i)), Some((y, j))) => i == j && x.len() == y.len() && cmp_prefix(x, y) == Ordering::Equal,
            _ => false,
        };
        if self.ranges.iter().any(|e| e.tx == r.tx && e.oid == r.oid && e.idx == r.idx && same(&e.lo, &r.lo) && same(&e.hi, &r.hi)) {
            return;
        }
        self.ranges.push(r);
    }

    /// Are there any key-range locks at all? (A cheap test so writers skip the key checks otherwise.)
    pub fn has_ranges(&self) -> bool {
        !self.ranges.is_empty()
    }

    /// Transactions other than `tx` holding a key-range lock on index `idx` of table `oid` that covers `key`.
    pub fn range_conflicts(&self, tx: TxId, oid: i32, idx: usize, key: &[Value]) -> Vec<TxId> {
        let mut v: Vec<TxId> = self.ranges.iter().filter(|r| r.tx != tx && r.oid == oid && r.idx == idx && r.covers(key)).map(|r| r.tx).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Does nobody hold any lock or key range?
    pub fn is_idle(&self) -> bool {
        self.held.is_empty() && self.ranges.is_empty()
    }

    pub fn release_all(&mut self, tx: TxId) {
        self.ranges.retain(|r| r.tx != tx);
        if let Some(rs) = self.by_tx.remove(&tx) {
            for r in rs {
                if let Some(hs) = self.held.get_mut(&r) {
                    hs.retain(|(t, _)| *t != tx);
                    if hs.is_empty() {
                        self.held.remove(&r);
                    }
                }
            }
        }
        self.waits.remove(&tx);
    }

    pub fn set_wait(&mut self, tx: TxId, blockers: Vec<TxId>) {
        self.waits.insert(tx, blockers);
    }

    pub fn clear_wait(&mut self, tx: TxId) {
        self.waits.remove(&tx);
    }

    /// Would `tx` waiting on its recorded blockers close a cycle?
    pub fn deadlocked(&self, tx: TxId) -> bool {
        let mut seen = HashSet::new();
        let mut stack: Vec<TxId> = self.waits.get(&tx).cloned().unwrap_or_default();
        while let Some(t) = stack.pop() {
            if t == tx {
                return true;
            }
            if !seen.insert(t) {
                continue;
            }
            if let Some(ws) = self.waits.get(&t) {
                stack.extend(ws.iter().copied());
            }
        }
        false
    }

    pub fn lock_count(&self, tx: TxId) -> usize {
        self.by_tx.get(&tx).map(|v| v.len()).unwrap_or(0) + self.ranges.iter().filter(|r| r.tx == tx).count()
    }

    /// (resource, tx, mode) for every held lock, for sys.dm_tran_locks.
    pub fn snapshot(&self) -> Vec<(Res, TxId, Mode)> {
        let mut v = vec![];
        for (r, hs) in &self.held {
            for (t, m) in hs {
                v.push((r.clone(), *t, *m));
            }
        }
        for r in &self.ranges {
            v.push((Res::Range(r.oid, r.idx), r.tx, Mode::S));
        }
        v
    }
}
