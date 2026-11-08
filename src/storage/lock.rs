//! Lock manager: multi-granularity locks (table and row), a wait-for graph and deadlock detection.
use super::{RowId, TxId};
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
}

#[derive(Default)]
pub struct LockManager {
    held: HashMap<Res, Vec<(TxId, Mode)>>,
    by_tx: HashMap<TxId, Vec<Res>>,
    waits: HashMap<TxId, Vec<TxId>>,
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

    pub fn release_all(&mut self, tx: TxId) {
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
        self.by_tx.get(&tx).map(|v| v.len()).unwrap_or(0)
    }

    /// (resource, tx, mode) for every held lock, for sys.dm_tran_locks.
    pub fn snapshot(&self) -> Vec<(Res, TxId, Mode)> {
        let mut v = vec![];
        for (r, hs) in &self.held {
            for (t, m) in hs {
                v.push((r.clone(), *t, *m));
            }
        }
        v
    }
}
