//! Join planning: clusters of inner joins are reordered by estimated cost, and a joined table
//! with a suitable index is probed by key instead of being scanned and hashed.
use super::optimizer::{split_conjuncts, Access};
use super::query::{children, ColMeta, Env, Found, Rowset, Schema};
use super::*;
use crate::storage::RowId;
use std::collections::HashSet;

fn col_refs<'e>(e: &'e Expr, out: &mut Vec<&'e Vec<String>>) {
    if let Expr::Col(p) = e {
        out.push(p);
    }
    let mut ch = vec![];
    children(e, &mut ch);
    for c in ch {
        col_refs(c, out);
    }
}

struct TblLeaf {
    loc: Loc,
    t: Table,
    name: Vec<String>,
    alias: String,
    pushed: Vec<Expr>,
}

struct Leaf<'t> {
    _tr: &'t TableRef,
    aliases: Vec<String>,
    cols: HashSet<String>,
    est: f64,
    tbl: Option<TblLeaf>,
    rs: Option<Rowset>,
    ncols: usize,
}

fn collect_cluster<'t>(tr: &'t TableRef, leaves: &mut Vec<&'t TableRef>, conds: &mut Vec<Expr>) {
    match tr {
        TableRef::Join { kind: JoinKind::Inner | JoinKind::Cross, left, right, on } => {
            collect_cluster(left, leaves, conds);
            collect_cluster(right, leaves, conds);
            if let Some(on) = on {
                let mut c = vec![];
                split_conjuncts(on, &mut c);
                conds.extend(c.into_iter().cloned());
            }
        }
        other => leaves.push(other),
    }
}

impl<'a> Engine<'a> {
    /// Estimated rows a table yields once its pushed-down conditions are applied.
    pub fn estimate_table(&mut self, t: &Table, alias: &str, pushed: &[Expr]) -> f64 {
        let base = t.data.slot_count() as f64;
        if pushed.is_empty() {
            return base.max(1.0);
        }
        let conj: Vec<&Expr> = pushed.iter().collect();
        let est = match self.choose_access(t, alias, &conj) {
            Access::Seek { idx, keys, .. } => {
                let sp = &t.data.specs[idx];
                let used = keys.first().map(|k| k.len()).unwrap_or(0);
                if sp.unique && used == sp.cols.len() {
                    keys.len() as f64
                } else {
                    let ndv = distinct_prefixes(t, idx, used).max(1) as f64;
                    base / ndv * keys.len() as f64
                }
            }
            Access::Range { .. } => base / 3.0,
            Access::Full => base * 0.33f64.powi(pushed.len().min(4) as i32),
        };
        est.clamp(1.0, base.max(1.0))
    }

    /// Evaluate a FROM list. Tables joined with commas (or INNER / CROSS JOIN) are treated as one
    /// cluster whose join order is chosen by the optimizer; the WHERE clause supplies join conditions.
    pub fn eval_from_cluster(&mut self, from: &[TableRef], where_: &Option<Expr>, outer: Option<&Env<'_>>) -> Result<Rowset> {
        let mut leaves = vec![];
        let mut conds: Vec<Expr> = vec![];
        for tr in from {
            collect_cluster(tr, &mut leaves, &mut conds);
        }
        let mut wconds: Vec<Expr> = vec![];
        if let (Some(w), true) = (where_, self.ses.optimizer) {
            let mut c = vec![];
            split_conjuncts(w, &mut c);
            for e in c {
                if super::optimizer::is_pushable(e) {
                    wconds.push(e.clone());
                }
            }
        }
        self.run_cluster(leaves, conds, wconds, outer)
    }

    pub fn eval_join_tree(&mut self, tr: &TableRef, outer: Option<&Env<'_>>) -> Result<Rowset> {
        let mut leaves = vec![];
        let mut conds = vec![];
        collect_cluster(tr, &mut leaves, &mut conds);
        self.run_cluster(leaves, conds, vec![], outer)
    }

    fn table_leaf(&mut self, tr: &TableRef) -> Option<TblLeaf> {
        if let TableRef::Table { name, alias, .. } = tr {
            if name.len() == 1 {
                let l = name[0].to_lowercase();
                if self.ses.cte_stack.iter().any(|m| m.contains_key(&l)) || l == "inserted" || l == "deleted" {
                    return None;
                }
            }
            if name.len() >= 2 {
                let s = &name[name.len() - 2];
                if s.eq_ignore_ascii_case("sys") || s.eq_ignore_ascii_case("information_schema") {
                    return None;
                }
            }
            let loc = self.locate(name);
            let t = self.table_ref(&loc)?.clone();
            let a = alias.clone().unwrap_or_else(|| name.last().unwrap().clone());
            let pushed = self.ses.pushdown.last().and_then(|v| v.iter().find(|(x, _)| x.eq_ignore_ascii_case(&a)).map(|(_, e)| e.clone())).unwrap_or_default();
            return Some(TblLeaf { loc, t, name: name.clone(), alias: a, pushed });
        }
        None
    }

    fn run_cluster(&mut self, trs: Vec<&TableRef>, on_conds: Vec<Expr>, where_conds: Vec<Expr>, outer: Option<&Env<'_>>) -> Result<Rowset> {
        if trs.len() == 1 {
            return self.eval_table_ref(trs[0], outer);
        }
        // describe each leaf
        let mut leaves: Vec<Leaf> = vec![];
        for tr in &trs {
            if let Some(tl) = self.table_leaf(tr) {
                let est = self.estimate_table(&tl.t, &tl.alias, &tl.pushed);
                leaves.push(Leaf {
                    _tr: tr,
                    aliases: vec![tl.alias.to_lowercase()],
                    cols: tl.t.columns.iter().map(|c| c.name.to_lowercase()).collect(),
                    est,
                    ncols: tl.t.columns.len(),
                    tbl: Some(tl),
                    rs: None,
                });
            } else {
                let rs = self.eval_table_ref(tr, outer)?;
                let mut aliases: Vec<String> = vec![];
                for c in &rs.schema.cols {
                    if let Some(q) = &c.qual {
                        let q = q.to_lowercase();
                        if !aliases.contains(&q) {
                            aliases.push(q);
                        }
                    }
                }
                leaves.push(Leaf { _tr: tr, aliases, cols: rs.schema.cols.iter().map(|c| c.name.to_lowercase()).collect(), est: rs.rows.len() as f64, ncols: rs.schema.cols.len(), tbl: None, rs: Some(rs) });
            }
        }
        // which leaves does a condition touch? (None = refers to something outside the cluster)
        let refs_of = |e: &Expr, leaves: &[Leaf]| -> Option<HashSet<usize>> {
            let mut refs = vec![];
            col_refs(e, &mut refs);
            let mut set = HashSet::new();
            for r in refs {
                if r.len() >= 2 {
                    let q = r[r.len() - 2].to_lowercase();
                    match leaves.iter().position(|l| l.aliases.contains(&q)) {
                        Some(i) => {
                            set.insert(i);
                        }
                        None => return None,
                    }
                } else {
                    let n = r[0].to_lowercase();
                    let hits: Vec<usize> = leaves.iter().enumerate().filter(|(_, l)| l.cols.contains(&n)).map(|(i, _)| i).collect();
                    match hits.len() {
                        1 => {
                            set.insert(hits[0]);
                        }
                        0 => return None,
                        _ => return None,
                    }
                }
            }
            Some(set)
        };
        // WHERE conjuncts that touch a single table are applied while that table is read
        let mut conds: Vec<Expr> = on_conds;
        for w in where_conds {
            if let Some(s) = refs_of(&w, &leaves) {
                if s.len() >= 2 {
                    conds.push(w);
                }
            }
        }
        let cond_refs: Vec<Option<HashSet<usize>>> = conds.iter().map(|c| refs_of(c, &leaves)).collect();
        // greedy order: smallest first, then the cheapest leaf connected to what is already joined
        let n = leaves.len();
        let mut order: Vec<usize> = vec![];
        let mut remaining: Vec<usize> = (0..n).collect();
        if !self.ses.optimizer || self.ses.query_opts.iter().any(|o| o == "FORCE ORDER") {
            order = (0..n).collect();
            remaining.clear();
        } else {
            let first = *remaining.iter().min_by(|&&a, &&b| leaves[a].est.partial_cmp(&leaves[b].est).unwrap_or(std::cmp::Ordering::Equal)).unwrap();
            order.push(first);
            remaining.retain(|&x| x != first);
        }
        while !remaining.is_empty() {
            let joined: HashSet<usize> = order.iter().copied().collect();
            let connected: Vec<usize> = remaining
                .iter()
                .copied()
                .filter(|&c| {
                    cond_refs.iter().any(|r| match r {
                        Some(s) => s.contains(&c) && s.iter().any(|x| joined.contains(x)) && s.len() >= 2,
                        None => false,
                    })
                })
                .collect();
            let pool = if connected.is_empty() { &remaining } else { &connected };
            let next = *pool.iter().min_by(|&&a, &&b| leaves[a].est.partial_cmp(&leaves[b].est).unwrap_or(std::cmp::Ordering::Equal)).unwrap();
            order.push(next);
            remaining.retain(|&x| x != next);
        }
        let reordered = order.iter().enumerate().any(|(i, &o)| i != o);
        {
            let names: Vec<String> = order.iter().map(|&i| leaves[i].aliases.first().cloned().unwrap_or_default()).collect();
            self.ses.plan.push(format!("join order: {}{}", names.join(" -> "), if reordered { " (reordered from the query's order)" } else { "" }));
        }

        // join in the chosen order
        let mut used = vec![false; conds.len()];
        let mut acc: Option<Rowset> = None;
        let mut acc_est = 0.0f64;
        let mut acc_set: HashSet<usize> = HashSet::new();
        let mut layout: Vec<(usize, usize)> = vec![]; // (leaf index, column count) in accumulated order
        for &li in &order {
            // conditions that become evaluable with this leaf
            let mut here: Vec<Expr> = vec![];
            for (ci, c) in conds.iter().enumerate() {
                if used[ci] {
                    continue;
                }
                if let Some(s) = &cond_refs[ci] {
                    let ready = s.iter().all(|x| *x == li || acc_set.contains(x));
                    if (s.contains(&li) && ready) || (s.is_empty() && acc.is_some()) {
                        used[ci] = true;
                        here.push(c.clone());
                    }
                }
            }
            let tl_opt = leaves[li].tbl.take();
            let right: Rowset;
            let mut inl_note = String::new();
            match tl_opt {
                Some(tl) => {
                    // index nested-loop: probe the table's index with the keys of the rows joined so far
                    let mut forced: Option<Access> = None;
                    if let Some(a) = &acc {
                        if let Some(ap) = self.probe_access(&tl, &here, a, acc_est, outer)? {
                            inl_note = match &ap {
                                Access::Seek { name, .. } => format!("index nested-loop via {}", name),
                                other => format!("index nested-loop via {}", other.describe()),
                            };
                            forced = Some(ap);
                        }
                    }
                    right = self.scan_table_leaf(&tl.loc, &tl.t, &tl.alias, &tl.name, &tl.pushed, forced)?;
                    leaves[li].tbl = Some(tl);
                }
                None => {
                    right = leaves[li].rs.take().unwrap();
                }
            }
            let right_est = leaves[li].est;
            let on: Option<Expr> = here.into_iter().reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
            match acc.take() {
                None => {
                    let mut right = right;
                    if let Some(on) = &on {
                        if !self.ses.dry_run {
                            let mut keep = vec![];
                            for r in right.rows.iter() {
                                let env = Env::new(&right.schema, r, outer);
                                if matches!(self.ev(on, &env)?, Value::Bit(true)) {
                                    keep.push(r.clone());
                                }
                            }
                            right = Rowset::new(right.schema.clone(), keep);
                        }
                    }
                    acc = Some(right);
                    acc_est = right_est;
                }
                Some(a) => {
                    let kind = if on.is_some() { JoinKind::Inner } else { JoinKind::Cross };
                    let method = if on.is_none() { "cross join" } else if inl_note.is_empty() { "hash join" } else { "index nested-loop join" };
                    let alias = leaves[li].aliases.first().cloned().unwrap_or_default();
                    let ar = a.rows.len();
                    let joined = self.join_rowsets(kind, a, right, &on, outer)?;
                    self.ses.plan.push(format!("join {}: {}; {} rows in, {} out", alias, if inl_note.is_empty() { method.to_string() } else { inl_note.clone() }, ar, joined.rows.len()));
                    acc_est = if on.is_some() { acc_est.max(right_est) } else { acc_est * right_est };
                    acc = Some(joined);
                }
            }
            acc_set.insert(li);
            layout.push((li, leaves[li].ncols));
        }
        let mut result = acc.unwrap();
        // conditions the join could not place (outer references etc.) are applied as a filter
        let rest: Vec<&Expr> = conds.iter().enumerate().filter(|(i, _)| !used[*i]).map(|(_, c)| c).collect();
        if !rest.is_empty() && !self.ses.dry_run {
            let mut keep = vec![];
            'rows: for r in result.rows.iter() {
                let env = Env::new(&result.schema, r, outer);
                for c in &rest {
                    // a condition referencing columns we cannot resolve here is left to WHERE
                    match self.ev(c, &env) {
                        Ok(Value::Bit(true)) => {}
                        Ok(_) => continue 'rows,
                        Err(_) => {}
                    }
                }
                keep.push(r.clone());
            }
            result = Rowset::new(result.schema.clone(), keep);
        }
        // restore the column order the query was written in (SELECT * depends on it)
        if reordered {
            let mut offsets: Vec<usize> = vec![0; n];
            let mut off = 0;
            for (li, nc) in &layout {
                offsets[*li] = off;
                off += nc;
            }
            let mut perm: Vec<usize> = vec![];
            for li in 0..n {
                for k in 0..leaves[li].ncols {
                    perm.push(offsets[li] + k);
                }
            }
            let cols: Vec<ColMeta> = perm.iter().map(|&i| result.schema.cols[i].clone()).collect();
            let rows: Vec<Row> = result.rows.iter().map(|r| perm.iter().map(|&i| r[i].clone()).collect()).collect();
            let mut rs = Rowset::new(Schema::new(cols), rows);
            rs.suppress = result.suppress;
            result = rs;
        }
        Ok(result)
    }

    /// If joining `tl` to the rows gathered so far can use one of its indexes, return a seek access
    /// path holding the distinct join keys of those rows.
    fn probe_access(&mut self, tl: &TblLeaf, here: &[Expr], acc: &Rowset, acc_est: f64, outer: Option<&Env<'_>>) -> Result<Option<Access>> {
        // OPTION (HASH JOIN) / (MERGE JOIN) rule out the index nested loop
        if !self.ses.optimizer || tl.t.data.specs.is_empty() || here.is_empty() || self.ses.query_opts.iter().any(|o| o == "HASH JOIN" || o == "MERGE JOIN") {
            return Ok(None);
        }
        let total = tl.t.data.slot_count() as f64;
        if acc_est > 64.0 && acc_est * 4.0 > total {
            return Ok(None);
        }
        // equality conjuncts `acc_expr = this.column`
        let mut pairs: Vec<(usize, &Expr)> = vec![]; // (column of this table, expression over accumulated rows)
        for c in here {
            if let Expr::Binary(crate::ast::BinOp::Eq, a, b) = c {
                for (x, y) in [(a, b), (b, a)] {
                    if let Some(col) = Self::table_col(&tl.t, &tl.alias, x) {
                        let mut refs = vec![];
                        col_refs(y, &mut refs);
                        let touches_tbl = refs.iter().any(|r| r.len() >= 2 && r[r.len() - 2].eq_ignore_ascii_case(&tl.alias));
                        if !touches_tbl && !refs.is_empty() {
                            pairs.push((col, y));
                        }
                    }
                }
            }
        }
        if pairs.is_empty() {
            return Ok(None);
        }
        // best index: longest equality prefix
        let mut best: Option<(usize, Vec<(usize, &Expr)>)> = None;
        for (ix, sp) in tl.t.data.specs.iter().enumerate() {
            if sp.filter.is_some() {
                continue; // a filtered index lacks some rows: not usable to look every match up
            }
            let mut matched = vec![];
            for &c in &sp.cols {
                match pairs.iter().find(|(pc, _)| *pc == c) {
                    Some(p) => matched.push(*p),
                    None => break,
                }
            }
            if !matched.is_empty() && best.as_ref().map(|(_, m)| matched.len() > m.len()).unwrap_or(true) {
                best = Some((ix, matched));
            }
        }
        let Some((ix, matched)) = best else { return Ok(None) };
        if self.ses.dry_run {
            return Ok(Some(Access::Seek { idx: ix, keys: vec![], name: tl.t.data.specs[ix].name.clone() }));
        }
        // distinct key tuples from the accumulated rows, coerced losslessly to the column types
        let mut keys: Vec<Vec<Value>> = vec![];
        let mut seen = HashSet::new();
        for r in acc.rows.iter() {
            let env = Env::new(&acc.schema, r, outer);
            let mut k = vec![];
            for (col, e) in &matched {
                let v = match self.ev(e, &env) {
                    Ok(v) => v,
                    Err(_) => return Ok(None),
                };
                if v.is_null() {
                    k.clear();
                    break;
                }
                match super::optimizer::coerce_const_pub(&tl.t.columns[*col], v) {
                    Some(c) => k.push(c),
                    None => return Ok(None),
                }
            }
            let kcolls: super::query::Colls = matched.iter().map(|(col, _)| tl.t.columns[*col].collation.as_deref().and_then(crate::collation::get)).collect();
            let dedupe_key = if super::query::any_coll(&kcolls) { crate::convert::key_of(&super::query::keyed(&k, &kcolls)) } else { crate::convert::key_of(&k) };
            if k.len() == matched.len() && seen.insert(dedupe_key) {
                keys.push(k);
            }
            if keys.len() > 20_000 {
                return Ok(None);
            }
        }
        // the key order must follow the index column order
        let sp_cols = &tl.t.data.specs[ix].cols;
        let order: Vec<usize> = sp_cols.iter().take(matched.len()).map(|c| matched.iter().position(|(pc, _)| pc == c).unwrap()).collect();
        let keys: Vec<Vec<Value>> = keys.into_iter().map(|k| order.iter().map(|&i| k[i].clone()).collect()).collect();
        Ok(Some(Access::Seek { idx: ix, keys, name: tl.t.data.specs[ix].name.clone() }))
    }

    fn table_col(t: &Table, alias: &str, e: &Expr) -> Option<usize> {
        match e {
            Expr::Col(p) if p.len() >= 2 && p[p.len() - 2].eq_ignore_ascii_case(alias) => t.col_index(&p[p.len() - 1]),
            Expr::Col(p) if p.len() == 1 => t.col_index(&p[0]),
            Expr::Paren(x) => Self::table_col(t, alias, x),
            _ => None,
        }
    }

    /// Read one table: the best index for its pushed-down conditions (or `forced`), honouring
    /// ordered-read hints, recording the choice in the plan.
    pub fn scan_table_leaf(&mut self, loc: &Loc, t: &Table, alias: &str, name: &[String], pushed: &[Expr], forced: Option<Access>) -> Result<Rowset> {
        crate::span!("scan_table_leaf");
        let denied = {
            crate::span!("select_denials");
            self.select_denials(loc, name, t)?
        };
        // a table with unreadable columns is read plainly: an index seek or an ordered read must not be able to
        // test or order by a column the user may not see
        let guarded = denied.iter().any(|d| d.is_some());
        let conj: Vec<&Expr> = pushed.iter().collect();
        let mut ap = match forced {
            _ if guarded => Access::Full,
            Some(a) => a,
            None => {
                crate::span!("choose_access");
                self.choose_access(t, alias, &conj)
            }
        };
        let total = t.data.slot_count();
        let mut limit: Option<usize> = None;
        let mut ordered = false;
        if let Some(h) = self.ses.order_hint.clone().filter(|_| !guarded) {
            if h.alias.eq_ignore_ascii_case(alias) && t.data.specs.len() > h.idx {
                let compatible = match &ap {
                    Access::Full => true,
                    Access::Range { idx, .. } => *idx == h.idx,
                    Access::Seek { .. } => false,
                };
                if compatible {
                    let (lo, hi) = match &ap {
                        Access::Range { lo, hi, .. } => (lo.clone(), hi.clone()),
                        _ => (None, None),
                    };
                    ap = Access::Range { idx: h.idx, lo, hi, forward: h.forward, name: t.data.specs[h.idx].name.clone() };
                    ordered = true;
                    if h.all_pushed {
                        limit = h.limit;
                    }
                }
            }
        }
        if ordered {
            self.ses.ordered_used = true;
        }
        let ps = pushed.len();
        let desc = ap.describe();
        let mut schema = {
            crate::span!("table_schema");
            super::dml::table_schema(t, alias)
        };
        let mark = |s: &mut Schema| {
            for (c, d) in s.cols.iter_mut().zip(denied.iter()) {
                c.denied = d.clone();
            }
        };
        mark(&mut schema);
        if self.ses.dry_run {
            let est = self.estimate_table(t, alias, pushed);
            self.ses.plan.push(format!("scan {} [{} rows]: {}{} ({} condition(s) pushed down) -> est {} rows", alias, total, desc, if ordered { ", read in index order" } else { "" }, ps, est as i64));
            return Ok(Rowset::table_rows(t, alias, vec![]));
        }
        let rows: Vec<Row> = match limit {
            Some(lim) => {
                let mut out: Vec<(RowId, Row)> = vec![];
                if let Access::Range { idx, lo, hi, forward, .. } = &ap {
                    if matches!(loc, Loc::Db(..)) && self.ses.tx.as_ref().map(|t| t.isolation == Isolation::Serializable).unwrap_or(false) {
                        // the rows below are read by id, so lock the whole range up front
                        self.lock_for_serializable(t, &ap)?;
                    }
                    // read the index a chunk at a time and stop as soon as enough rows qualify
                    let (lo, hi) = (lo.as_ref().map(|(v, i)| (v.as_slice(), *i)), hi.as_ref().map(|(v, i)| (v.as_slice(), *i)));
                    let mut after = None;
                    loop {
                        let c = t.data.range_chunk(*idx, lo, hi, *forward, after.as_ref(), 64);
                        let rids: Vec<RowId> = c.iter().map(|(_, r)| *r as RowId).collect();
                        let got = self.scan_rows_ids(loc, t, &rids)?;
                        out.extend(self.filter_pushed(&schema, got, pushed)?);
                        if out.len() >= lim || c.len() < 64 {
                            break;
                        }
                        after = c.into_iter().last();
                    }
                }
                out.truncate(lim);
                out.into_iter().map(|r| r.1).collect()
            }
            None => {
                crate::span!("scan_rows_flt");
                let scanned = self.scan_rows_flt(loc, t, &ap, Some((&schema, pushed)))?;
                scanned.into_iter().map(|r| r.1).collect()
            }
        };
        crate::span!("plan_text_and_rowset");
        self.ses.plan.push(format!("scan {} [{} rows]: {}{} ({} condition(s) pushed down) -> {} rows", alias, total, desc, if ordered { ", read in index order" } else { "" }, ps, rows.len()));
        let mut rs = Rowset::table_rows(t, alias, rows);
        mark(&mut rs.schema);
        Ok(rs)
    }
}

/// Number of distinct values of the first `prefix` columns of an index.
pub fn distinct_prefixes(t: &Table, idx: usize, prefix: usize) -> usize {
    t.data.distinct_prefixes(idx, prefix)
}

#[allow(dead_code)]
fn _unused(_: Found) {}
