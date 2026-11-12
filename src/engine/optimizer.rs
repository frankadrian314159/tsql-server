//! Query optimizer: predicate pushdown and access-path (index) selection.
//!
//! The optimizer works on the bound query before any row is read. It never changes the answer;
//! the executor still re-checks every condition, so a path only has to be a superset of the
//! matching rows.
use super::query::{children, Env, Schema};
use super::*;
use crate::convert::{cast, compare};
use crate::storage::{ReadView, RowId};
use std::cmp::Ordering;

/// How a table's rows are located.
#[derive(Clone, Debug)]
pub enum Access {
    Full,
    /// Equality seek on a prefix of an index; several keys for `IN` lists.
    Seek { idx: usize, keys: Vec<Vec<Value>>, name: String },
    /// Range scan on an index (first column bounds), returned in key order.
    Range { idx: usize, lo: Option<(Vec<Value>, bool)>, hi: Option<(Vec<Value>, bool)>, forward: bool, name: String },
}

impl Access {
    pub fn describe(&self) -> String {
        match self {
            Access::Full => "full scan".into(),
            Access::Seek { name, keys, .. } => format!("index seek via {} ({} key(s))", name, keys.len()),
            Access::Range { name, .. } => format!("index range scan via {}", name),
        }
    }
    pub fn row_ids(&self, t: &Table) -> Vec<RowId> {
        match self {
            Access::Full => t.data.iter(ReadView::dirty()).into_iter().map(|(r, _)| r).collect(),
            Access::Seek { idx, keys, .. } => {
                let mut v: Vec<RowId> = vec![];
                for k in keys {
                    v.extend(t.data.seek_eq(*idx, k));
                }
                v.sort_unstable();
                v.dedup();
                v
            }
            Access::Range { idx, lo, hi, forward, .. } => t.data.seek_range(*idx, lo.as_ref().map(|(v, i)| (v.as_slice(), *i)), hi.as_ref().map(|(v, i)| (v.as_slice(), *i)), *forward),
        }
    }
}

pub fn split_conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::And(a, b) => {
            split_conjuncts(a, out);
            split_conjuncts(b, out);
        }
        Expr::Paren(x) => split_conjuncts(x, out),
        _ => out.push(e),
    }
}

/// Does the expression read only constants (no columns, no subqueries, deterministic)?
fn is_constant(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Var(_) | Expr::SysVar(_) => true,
        Expr::Col(_) | Expr::Star(_) | Expr::Subquery(_) | Expr::Exists(_) | Expr::InQuery(..) | Expr::CmpQuant(..) | Expr::NextValue(_) | Expr::Default | Expr::IdentityFn { .. } => false,
        Expr::Func(fc) => {
            if fc.over.is_some() || fc.schema.is_some() || matches!(fc.name.as_str(), "NEWID" | "NEWSEQUENTIALID" | "RAND" | "GETDATE" | "SYSDATETIME" | "GETUTCDATE" | "SYSUTCDATETIME" | "CURRENT_TIMESTAMP") {
                return false;
            }
            let mut ch = vec![];
            children(e, &mut ch);
            ch.into_iter().all(is_constant)
        }
        _ => {
            let mut ch = vec![];
            children(e, &mut ch);
            ch.into_iter().all(is_constant)
        }
    }
}

/// Can this conjunct be evaluated early (no subqueries, deterministic functions)?
pub fn is_pushable(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::Exists(_) | Expr::InQuery(..) | Expr::CmpQuant(..) | Expr::NextValue(_) | Expr::IdentityFn { .. } => false,
        Expr::Func(fc) => {
            if fc.over.is_some() || fc.schema.is_some() || matches!(fc.name.as_str(), "NEWID" | "NEWSEQUENTIALID" | "RAND") || super::query::is_agg_name(&fc.name) {
                return false;
            }
            let mut ch = vec![];
            children(e, &mut ch);
            ch.into_iter().all(is_pushable)
        }
        _ => {
            let mut ch = vec![];
            children(e, &mut ch);
            ch.into_iter().all(is_pushable)
        }
    }
}

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

enum Pred {
    Eq(Value),
    In(Vec<Value>),
    Lower(Value, bool),
    Upper(Value, bool),
}

/// Is `v` safe to compare directly with a value of the column's type (no implicit conversion of the column)?
fn compatible(col: &Column, v: &Value) -> bool {
    if v.is_null() {
        return false;
    }
    let t = &col.ty;
    if t.is_string() {
        matches!(v, Value::Str(_))
    } else if t.is_numeric() {
        matches!(v, Value::Bit(_) | Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) | Value::Str(_))
    } else if t.is_datetime() {
        matches!(v, Value::Date(_) | Value::DateTime(..) | Value::Dto(_) | Value::Time(_) | Value::Str(_))
    } else if matches!(t, DataType::Guid) {
        matches!(v, Value::Guid(_) | Value::Str(_))
    } else {
        col.ty.is_binary() && matches!(v, Value::Bin(_))
    }
}

/// Convert a constant to the column's type, but only if the conversion is lossless.
fn coerce_const(col: &Column, v: Value) -> Option<Value> {
    if !compatible(col, &v) {
        return None;
    }
    let c = cast(&v, &col.ty, None).ok()?;
    if matches!(compare(&v, &c), Ok(Some(Ordering::Equal))) {
        Some(c)
    } else {
        None
    }
}

impl<'a> Engine<'a> {
    fn const_value(&mut self, e: &Expr) -> Option<Value> {
        if !is_constant(e) {
            return None;
        }
        self.ev(e, &Env::empty()).ok()
    }

    /// Column of `t` that the expression names (`c`, `alias.c` or `table.c`).
    pub(super) fn pred_col(t: &Table, alias: &str, e: &Expr) -> Option<usize> {
        match e {
            Expr::Col(p) if p.len() == 1 => t.col_index(&p[0]),
            Expr::Col(p) if p.len() >= 2 => {
                let q = &p[p.len() - 2];
                if q.eq_ignore_ascii_case(alias) || q.eq_ignore_ascii_case(&t.name) {
                    t.col_index(&p[p.len() - 1])
                } else {
                    None
                }
            }
            Expr::Paren(x) => Self::pred_col(t, alias, x),
            _ => None,
        }
    }

    fn column_preds(&mut self, t: &Table, alias: &str, conj: &[&Expr]) -> Vec<(usize, Pred)> {
        let mut out = vec![];
        for e in conj {
            match e {
                Expr::Binary(op, a, b) => {
                    use crate::ast::BinOp::*;
                    if !matches!(op, Eq | Lt | Le | Gt | Ge) {
                        continue;
                    }
                    let (col, val, flipped) = if let Some(c) = Self::pred_col(t, alias, a) {
                        (c, self.const_value(b), false)
                    } else if let Some(c) = Self::pred_col(t, alias, b) {
                        (c, self.const_value(a), true)
                    } else {
                        continue;
                    };
                    let Some(v) = val.and_then(|v| coerce_const(&t.columns[col], v)) else { continue };
                    let op = if flipped {
                        match op {
                            Lt => Gt,
                            Le => Ge,
                            Gt => Lt,
                            Ge => Le,
                            o => *o,
                        }
                    } else {
                        *op
                    };
                    out.push((
                        col,
                        match op {
                            Eq => Pred::Eq(v),
                            Gt => Pred::Lower(v, false),
                            Ge => Pred::Lower(v, true),
                            Lt => Pred::Upper(v, false),
                            _ => Pred::Upper(v, true),
                        },
                    ));
                }
                Expr::Between(x, lo, hi, false) => {
                    if let Some(col) = Self::pred_col(t, alias, x) {
                        let lo = self.const_value(lo).and_then(|v| coerce_const(&t.columns[col], v));
                        let hi = self.const_value(hi).and_then(|v| coerce_const(&t.columns[col], v));
                        if let Some(l) = lo {
                            out.push((col, Pred::Lower(l, true)));
                        }
                        if let Some(h) = hi {
                            out.push((col, Pred::Upper(h, true)));
                        }
                    }
                }
                Expr::InList(x, items, false) => {
                    if let Some(col) = Self::pred_col(t, alias, x) {
                        let mut vals = vec![];
                        let mut ok = items.len() <= 64;
                        for it in items {
                            match self.const_value(it).and_then(|v| coerce_const(&t.columns[col], v)) {
                                Some(v) => vals.push(v),
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok && !vals.is_empty() {
                            out.push((col, Pred::In(vals)));
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Do the query's conditions (`preds` are the column conditions found in `conj`) imply every condition of the filtered
    /// index's `WHERE` clause?
    fn filter_implied(f: &crate::storage::FilterPred, preds: &[(usize, Pred)], conj: &[&Expr], t: &Table, alias: &str) -> bool {
        use crate::storage::FilterOp::*;
        let cmp = |a: &Value, b: &Value| crate::convert::compare(a, b).ok().flatten();
        f.conds.iter().all(|c| {
            let on_col = || preds.iter().filter(|(pc, _)| *pc == c.col);
            match c.op {
                IsNull => conj.iter().any(|e| matches!(e, Expr::IsNull(x, false) if Self::pred_col(t, alias, x) == Some(c.col))),
                IsNotNull => on_col().next().is_some() || conj.iter().any(|e| matches!(e, Expr::IsNull(x, true) if Self::pred_col(t, alias, x) == Some(c.col))),
                Eq | Ne | In => on_col().any(|(_, p)| match p {
                    Pred::Eq(w) => c.holds(w),
                    Pred::In(ws) => ws.iter().all(|w| c.holds(w)),
                    _ => false,
                }),
                Gt | Ge | Lt | Le => on_col().any(|(_, p)| match p {
                    Pred::Eq(w) => c.holds(w),
                    Pred::In(ws) => ws.iter().all(|w| c.holds(w)),
                    // x > w (or x >= w) implies x > v / x >= v when w is far enough up
                    Pred::Lower(w, incl) => match (c.op, cmp(w, &c.vals[0])) {
                        (Gt, Some(Ordering::Greater)) => true,
                        (Gt, Some(Ordering::Equal)) => !*incl,
                        (Ge, Some(Ordering::Greater | Ordering::Equal)) => true,
                        _ => false,
                    },
                    Pred::Upper(w, incl) => match (c.op, cmp(w, &c.vals[0])) {
                        (Lt, Some(Ordering::Less)) => true,
                        (Lt, Some(Ordering::Equal)) => !*incl,
                        (Le, Some(Ordering::Less | Ordering::Equal)) => true,
                        _ => false,
                    },
                }),
            }
        })
    }

    /// Pick the cheapest way to find the rows that can satisfy `conj` in table `t` (alias `alias`).
    pub fn choose_access(&mut self, t: &Table, alias: &str, conj: &[&Expr]) -> Access {
        if !self.ses.optimizer || t.data.specs.is_empty() || conj.is_empty() || self.ses.hints.get(&t.object_id).is_some_and(|h| h.iter().any(|x| x == "FORCESCAN" || x == "NOEXPAND_SCAN")) {
            return Access::Full;
        }
        let preds = self.column_preds(t, alias, conj);
        if preds.is_empty() {
            return Access::Full;
        }
        let mut best: Option<(i64, Access)> = None;
        for (ix, sp) in t.data.specs.iter().enumerate() {
            // a filtered index is usable only when the query's conditions guarantee every row it needs is in it
            if let Some(f) = &sp.filter {
                if !Self::filter_implied(f, &preds, conj, t, alias) {
                    continue;
                }
            }
            let mut prefix: Vec<Vec<Value>> = vec![vec![]];
            let mut eq_cols = 0;
            let mut next_col: Option<usize> = None;
            for &c in &sp.cols {
                let eq = preds.iter().find_map(|(pc, p)| if *pc == c { match p { Pred::Eq(v) => Some(vec![v.clone()]), Pred::In(vs) => Some(vs.clone()), _ => None } } else { None });
                match eq {
                    Some(vals) if prefix.len() * vals.len() <= 64 => {
                        let mut np = vec![];
                        for p in &prefix {
                            for v in &vals {
                                let mut k = p.clone();
                                k.push(v.clone());
                                np.push(k);
                            }
                        }
                        prefix = np;
                        eq_cols += 1;
                    }
                    _ => {
                        next_col = Some(c);
                        break;
                    }
                }
            }
            let mut cand: Option<(i64, Access)> = None;
            if eq_cols > 0 {
                let full_unique = sp.unique && eq_cols == sp.cols.len();
                let score = eq_cols as i64 * 10 + if full_unique { 1000 } else { 0 } + if sp.unique { 1 } else { 0 };
                cand = Some((score, Access::Seek { idx: ix, keys: prefix.clone(), name: sp.name.clone() }));
            } else if let Some(c) = next_col {
                // range on the first index column
                let mut lo: Option<(Vec<Value>, bool)> = None;
                let mut hi: Option<(Vec<Value>, bool)> = None;
                for (pc, p) in &preds {
                    if *pc != c {
                        continue;
                    }
                    match p {
                        Pred::Lower(v, incl) => lo = Some((vec![v.clone()], *incl)),
                        Pred::Upper(v, incl) => hi = Some((vec![v.clone()], *incl)),
                        _ => {}
                    }
                }
                if lo.is_some() || hi.is_some() {
                    let score = if lo.is_some() && hi.is_some() { 8 } else { 4 };
                    cand = Some((score, Access::Range { idx: ix, lo, hi, forward: true, name: sp.name.clone() }));
                }
            }
            if let Some((sc, ac)) = cand {
                if best.as_ref().map(|(b, _)| sc > *b).unwrap_or(true) {
                    best = Some((sc, ac));
                }
            }
        }
        best.map(|b| b.1).unwrap_or(Access::Full)
    }

    /// For each table in a FROM clause, the conditions of `where_` that can be applied while
    /// the table is read (before any join). Outer joins' null-supplying sides get none.
    pub fn plan_pushdown(&mut self, from: &[TableRef], where_: &Option<Expr>) -> Vec<(String, Vec<Expr>)> {
        self.plan_pushdown_outer(from, where_, None)
    }

    /// Like `plan_pushdown`; with `outer` (a correlated subquery's enclosing row), a comparison between a column of
    /// the subquery's tables and a column of the outer row is pushed with the outer value as a constant, so
    /// `WHERE d.id = t.grp` can seek on `d.id` instead of scanning `d` for every outer row.
    pub fn plan_pushdown_outer(&mut self, from: &[TableRef], where_: &Option<Expr>, outer: Option<&Env<'_>>) -> Vec<(String, Vec<Expr>)> {
        let Some(w) = where_ else { return vec![] };
        if !self.ses.optimizer {
            return vec![];
        }
        // A correlated subquery is planned once per outer row, always the same way but for the values taken from the
        // outer row: what does not depend on them is worked out once per statement (see `PushPlan`).
        if let Some(o) = outer {
            let key = (from.as_ptr() as usize, w as *const Expr as usize);
            let plan = match self.ses.push_plans.get(&key) {
                Some(p) => p.clone(),
                None => {
                    let p = std::sync::Arc::new(self.build_push_plan(from, w));
                    self.ses.push_plans.insert(key, p.clone());
                    p
                }
            };
            return plan.instantiate(o);
        }
        let mut ls: Vec<PdLeaf> = vec![];
        let mut opaque = false;
        for tr in from {
            pd_leaves(self, tr, true, &mut ls, &mut opaque);
        }
        let mut conj = vec![];
        split_conjuncts(w, &mut conj);
        let mut out: Vec<(String, Vec<Expr>)> = vec![];
        for c in conj {
            if let Some(o) = pd_owner(c, &ls, opaque) {
                push_cond(&mut out, &ls[o].alias, c.clone());
            }
        }
        out
    }

    fn build_push_plan(&self, from: &[TableRef], w: &Expr) -> PushPlan {
        let mut ls: Vec<PdLeaf> = vec![];
        let mut opaque = false;
        for tr in from {
            pd_leaves(self, tr, true, &mut ls, &mut opaque);
        }
        let mut conj = vec![];
        split_conjuncts(w, &mut conj);
        let is_local = |p: &[String]| -> bool {
            if p.len() >= 2 {
                let q = p[p.len() - 2].to_lowercase();
                ls.iter().any(|l| l.alias == q)
            } else {
                let n = p[0].to_lowercase();
                ls.iter().any(|l| l.cols.contains(&n))
            }
        };
        fn strip(e: &Expr) -> &Expr {
            match e {
                Expr::Paren(a) => strip(a),
                _ => e,
            }
        }
        let mut plans = vec![];
        for c in conj {
            let plain = pd_owner(c, &ls, opaque).map(|o| ls[o].alias.clone());
            let mut cands = vec![];
            if let (false, Expr::Binary(op, a, b)) = (opaque, c) {
                if matches!(op, BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
                    // the outer column may be on either side; the left one is tried first, as `bind_outer` does
                    for (outer_left, side, other) in [(true, a, b), (false, b, a)] {
                        if let Expr::Col(p) = strip(side) {
                            if !is_local(p) {
                                let probe = if outer_left { Expr::Binary(*op, Box::new(Expr::Lit(Value::Null)), other.clone()) } else { Expr::Binary(*op, other.clone(), Box::new(Expr::Lit(Value::Null))) };
                                let owner = pd_owner(&probe, &ls, opaque).map(|o| ls[o].alias.clone());
                                cands.push(BindCand { op: *op, outer_left, path: p.clone(), local: (**other).clone(), owner });
                            }
                        }
                    }
                }
            }
            plans.push(ConjPlan { expr: c.clone(), plain, cands });
        }
        PushPlan { conj: plans }
    }

    /// Apply already-planned table conditions to freshly scanned rows.
    pub fn filter_pushed(&mut self, schema: &Schema, rows: Vec<(RowId, Row)>, conds: &[Expr]) -> Result<Vec<(RowId, Row)>> {
        if conds.is_empty() {
            return Ok(rows);
        }
        let mut out = Vec::with_capacity(rows.len());
        'rows: for (rid, r) in rows {
            let env = Env::new(schema, &r, None);
            for c in conds {
                if !matches!(self.ev(c, &env)?, Value::Bit(true)) {
                    continue 'rows;
                }
            }
            out.push((rid, r));
        }
        Ok(out)
    }
}

/// Read a table through an index so `ORDER BY` needs no sort (and `TOP` can stop early).
#[derive(Clone, Debug)]
pub struct OrderHint {
    pub alias: String,
    pub idx: usize,
    pub forward: bool,
    pub limit: Option<usize>,
    pub all_pushed: bool,
}

pub fn coerce_const_pub(col: &Column, v: Value) -> Option<Value> {
    coerce_const(col, v)
}

impl<'a> Engine<'a> {
    /// Decide whether a single-table query can be answered by reading an index in order.
    pub fn compute_order_hint(&mut self, sel: &Select, order: &[OrderItem], offset: &Option<Expr>, fetch: &Option<Expr>) -> Option<OrderHint> {
        if !self.ses.optimizer || order.is_empty() || sel.distinct || sel.group.is_some() || sel.having.is_some() || sel.into.is_some() || sel.from.len() != 1 {
            return None;
        }
        if sel.items.iter().any(|i| match i {
            SelectItem::Assign { .. } => true,
            SelectItem::Expr { e, .. } => super::query::has_agg(e) || {
                let mut w = vec![];
                super::query::collect_windows(e, &mut w);
                !w.is_empty()
            },
            _ => false,
        }) {
            return None;
        }
        if let Some(t) = &sel.top {
            if t.percent || t.ties {
                return None;
            }
        }
        let TableRef::Table { name, alias, .. } = &sel.from[0] else { return None };
        let loc = self.locate(name);
        let t = self.table_ref(&loc)?.clone();
        let a = alias.clone().unwrap_or_else(|| name.last().unwrap().clone());
        let mut cols = vec![];
        for o in order {
            cols.push(Self::pred_col(&t, &a, &o.e)?);
        }
        let desc = order[0].desc;
        if order.iter().any(|o| o.desc != desc) {
            return None;
        }
        // the index must be keyed by exactly the ORDER BY columns so ties come out in row order
        let idx = t.data.specs.iter().position(|sp| sp.cols == cols && sp.filter.is_none())?;
        let sp = &t.data.specs[idx];
        if desc && !(sp.unique && sp.cols.len() == cols.len()) {
            return None;
        }
        // row limit: TOP n / OFFSET .. FETCH, both constant
        let env = Env::empty();
        let mut limit: Option<usize> = None;
        let top_n = match &sel.top {
            Some(tp) => Some(self.ev(&tp.e, &env).ok()?.as_i64()?.max(0) as usize),
            None => None,
        };
        let fetch_n = match fetch {
            Some(f) => Some(self.ev(f, &env).ok()?.as_i64()?.max(0) as usize),
            None => None,
        };
        let off = match offset {
            Some(o) => self.ev(o, &env).ok()?.as_i64()?.max(0) as usize,
            None => 0,
        };
        for n in [top_n, fetch_n].into_iter().flatten() {
            limit = Some(limit.map(|l: usize| l.min(n)).unwrap_or(n));
        }
        if let Some(l) = limit {
            limit = Some(l + off);
        }
        // every WHERE conjunct must be applied during the scan for an early stop to be safe
        let mut conj = vec![];
        if let Some(w) = &sel.where_ {
            split_conjuncts(w, &mut conj);
        }
        let pushed = self.plan_pushdown(&sel.from, &sel.where_);
        let n_pushed: usize = pushed.iter().filter(|(x, _)| x.eq_ignore_ascii_case(&a)).map(|(_, e)| e.len()).sum();
        Some(OrderHint { alias: a, idx, forward: !desc, limit, all_pushed: n_pushed == conj.len() })
    }
}

/// A table of a FROM clause, as `plan_pushdown_outer` sees it.
struct PdLeaf {
    alias: String,
    cols: Vec<String>,
    preserved: bool,
}

fn pd_leaves(eng: &Engine, tr: &TableRef, preserved: bool, out: &mut Vec<PdLeaf>, opaque: &mut bool) {
    match tr {
        TableRef::Table { name, alias, .. } => {
            let loc = eng.locate(name);
            let a = alias.clone().unwrap_or_else(|| name.last().unwrap().clone());
            match eng.table_ref(&loc) {
                Some(t) if name.len() > 1 || !eng.ses.cte_stack.iter().any(|m| m.contains_key(&name[0].to_lowercase())) => {
                    out.push(PdLeaf { alias: a.to_lowercase(), cols: t.columns.iter().map(|c| c.name.to_lowercase()).collect(), preserved });
                }
                _ => *opaque = true,
            }
        }
        TableRef::Join { kind, left, right, .. } => {
            let lp = preserved && !matches!(kind, JoinKind::Right | JoinKind::Full);
            let rp = preserved && !matches!(kind, JoinKind::Left | JoinKind::Full | JoinKind::CrossApply | JoinKind::OuterApply);
            pd_leaves(eng, left, lp, out, opaque);
            pd_leaves(eng, right, rp, out, opaque);
        }
        _ => *opaque = true,
    }
}

/// The table (index into `ls`) whose rows alone decide the condition `c`, when `c` can be applied while that table
/// is read: it must be evaluable per row, mention columns of one table only, and that table must not be on the
/// null-supplying side of an outer join.
fn pd_owner(c: &Expr, ls: &[PdLeaf], opaque: bool) -> Option<usize> {
    if !is_pushable(c) {
        return None;
    }
    let mut refs = vec![];
    col_refs(c, &mut refs);
    if refs.is_empty() {
        return None;
    }
    let mut owner: Option<usize> = None;
    for r in refs {
        let idx = if r.len() >= 2 {
            let q = r[r.len() - 2].to_lowercase();
            ls.iter().position(|l| l.alias == q)?
        } else {
            if opaque {
                return None;
            }
            let n = r[0].to_lowercase();
            let hits: Vec<usize> = ls.iter().enumerate().filter(|(_, l)| l.cols.contains(&n)).map(|(i, _)| i).collect();
            if hits.len() != 1 {
                return None;
            }
            hits[0]
        };
        match owner {
            None => owner = Some(idx),
            Some(o) if o == idx => {}
            _ => return None,
        }
    }
    let o = owner?;
    ls[o].preserved.then_some(o)
}

fn push_cond(out: &mut Vec<(String, Vec<Expr>)>, alias: &str, c: Expr) {
    match out.iter_mut().find(|(a, _)| a == alias) {
        Some((_, v)) => v.push(c),
        None => out.push((alias.to_string(), vec![c])),
    }
}

/// What `plan_pushdown_outer` finds out about the WHERE clause of a correlated subquery that does not depend on the
/// outer row, kept for the rest of the statement: for each conjunct, the table it belongs to as it is, and, for a
/// comparison with a column that is not the subquery's own, where the outer value goes and the table it then belongs to.
pub struct PushPlan {
    conj: Vec<ConjPlan>,
}

struct ConjPlan {
    expr: Expr,
    plain: Option<String>,
    cands: Vec<BindCand>,
}

struct BindCand {
    op: BinOp,
    /// the outer column is the left operand
    outer_left: bool,
    path: Vec<String>,
    /// the other operand
    local: Expr,
    owner: Option<String>,
}

impl PushPlan {
    /// The conditions to push for the outer row `o`: the same as planning the clause against that row from scratch.
    fn instantiate(&self, o: &Env<'_>) -> Vec<(String, Vec<Expr>)> {
        let mut out: Vec<(String, Vec<Expr>)> = vec![];
        for cp in &self.conj {
            let mut bound: Option<(&Option<String>, Expr)> = None;
            for cand in &cp.cands {
                if let Ok(v) = o.lookup(&cand.path) {
                    let (l, r) = if cand.outer_left { (Expr::Lit(v), cand.local.clone()) } else { (cand.local.clone(), Expr::Lit(v)) };
                    bound = Some((&cand.owner, Expr::Binary(cand.op, Box::new(l), Box::new(r))));
                    break;
                }
            }
            match bound {
                Some((Some(alias), e)) => push_cond(&mut out, alias, e),
                Some((None, _)) => {}
                None => {
                    if let Some(alias) = &cp.plain {
                        push_cond(&mut out, alias, cp.expr.clone());
                    }
                }
            }
        }
        out
    }
}

