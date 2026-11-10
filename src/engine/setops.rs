//! `UNION`, `INTERSECT` and `EXCEPT` with bounded memory.
//!
//! The in-memory implementation (hash sets of row keys) is used while the inputs are small. When an input has
//! been spilled, or the inputs together would exceed the work-memory budget, the operators run on external
//! sorts instead: rows are keyed by their identity (the same key the hash sets use), sorted with spilling, and
//! de-duplicated or compared by merging, and the survivors are put back into first-arrival order. The result goes
//! to a [`RowsWriter`], so a large result stays in a file until something reads it.
use super::spill::{row_bytes, Cmp, Entry, PartWriter, RowsRef, RowsWriter, Sorter};
use super::*;
use std::cmp::Ordering;

fn key_str(e: &Entry) -> &str {
    match e.keys.first() {
        Some(Value::Str(s)) => s.as_str(),
        _ => "",
    }
}

fn by_key_then_seq() -> Cmp {
    Box::new(|a: &Entry, b: &Entry| key_str(a).cmp(key_str(b)).then(a.seq.cmp(&b.seq)))
}

fn by_seq() -> Cmp {
    Box::new(|a: &Entry, b: &Entry| a.seq.cmp(&b.seq))
}

impl<'a> Engine<'a> {
    /// Should a set operation over these inputs use the external algorithms?
    pub fn setop_needs_spill(&self, l: &RowsRef, r: &RowsRef) -> bool {
        if l.is_spilled() || r.is_spilled() {
            return true;
        }
        let budget = self.srv.work.bytes;
        let est = |x: &RowsRef| -> usize {
            let n = x.count();
            if n == 0 {
                return 0;
            }
            let sample = n.min(64);
            let avg = x.iter().take(sample).map(|r| row_bytes(r)).sum::<usize>() / sample;
            // a hash set of row keys costs about as much again as the rows
            n * avg * 2
        };
        est(l) + est(r) > budget
    }

    /// The result of `op` over two inputs, computed with external sorts (see the module docs).
    pub fn set_op_external(&mut self, op: &SetOp, all: bool, l: &RowsRef, r: &RowsRef, rk_of: &dyn Fn(&Row) -> String) -> Result<RowsRef> {
        let work = self.srv.work.clone();
        let mut out = RowsWriter::new(work.clone());
        let mut seq = 0u64;
        match (op, all) {
            (SetOp::Union, true) => {
                for side in [l, r] {
                    let mut it = side.stream();
                    while let Some(row) = it.next_row()? {
                        out.push(row)?;
                    }
                }
            }
            (SetOp::Union, false) => {
                let mut dedupe = Sorter::new(by_key_then_seq(), work.clone());
                for side in [l, r] {
                    let mut it = side.stream();
                    while let Some(row) = it.next_row()? {
                        dedupe.push(Entry { keys: vec![Value::Str(rk_of(&row))], seq, row })?;
                        seq += 1;
                    }
                }
                let mut ordered = Sorter::new(by_seq(), work.clone());
                let mut last: Option<String> = None;
                dedupe.finish(|e| {
                    let k = key_str(&e).to_string();
                    if last.as_deref() != Some(k.as_str()) {
                        last = Some(k);
                        ordered.push(e)?;
                    }
                    Ok(true)
                })?;
                ordered.finish(|e| {
                    out.push(e.row)?;
                    Ok(true)
                })?;
            }
            (SetOp::Intersect, _) | (SetOp::Except, _) => {
                let intersect = matches!(op, SetOp::Intersect);
                // the distinct keys of the right input, sorted, in a file that is read back in step with the left
                let mut rs = Sorter::new(by_key_then_seq(), work.clone());
                let mut it = r.stream();
                while let Some(row) = it.next_row()? {
                    rs.push(Entry { keys: vec![Value::Str(rk_of(&row))], seq: 0, row: vec![] })?;
                }
                let mut w = PartWriter::create(&work.dir)?;
                let mut last: Option<String> = None;
                rs.finish(|e| {
                    let k = key_str(&e).to_string();
                    if last.as_deref() != Some(k.as_str()) {
                        w.push(&[Value::Str(k.clone())])?;
                        last = Some(k);
                    }
                    Ok(true)
                })?;
                let rfile = w.finish()?;
                let mut rread = rfile.reader()?;
                let mut rhead: Option<String> = match rread.next_row()? {
                    Some(row) => Some(match &row[0] { Value::Str(x) => x.clone(), _ => String::new() }),
                    None => None,
                };
                let mut ls = Sorter::new(by_key_then_seq(), work.clone());
                let mut it = l.stream();
                while let Some(row) = it.next_row()? {
                    ls.push(Entry { keys: vec![Value::Str(rk_of(&row))], seq, row })?;
                    seq += 1;
                }
                let mut ordered = Sorter::new(by_seq(), work.clone());
                let mut last: Option<String> = None;
                ls.finish(|e| {
                    let k = key_str(&e).to_string();
                    if last.as_deref() == Some(k.as_str()) {
                        return Ok(true); // a repeat of a key already decided
                    }
                    last = Some(k.clone());
                    // advance the right side to the first key >= this one
                    while let Some(h) = &rhead {
                        if h.as_str().cmp(k.as_str()) == Ordering::Less {
                            rhead = match rread.next_row()? {
                                Some(row) => Some(match &row[0] { Value::Str(x) => x.clone(), _ => String::new() }),
                                None => None,
                            };
                        } else {
                            break;
                        }
                    }
                    let present = rhead.as_deref() == Some(k.as_str());
                    if present == intersect {
                        ordered.push(e)?;
                    }
                    Ok(true)
                })?;
                ordered.finish(|e| {
                    out.push(e.row)?;
                    Ok(true)
                })?;
            }
        }
        out.finish()
    }
}
