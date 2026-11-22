//! Planning joins over sharded tables.
//!
//! A join runs shard by shard when, on every shard, all the rows that can join are there. Three ways to get there:
//!
//! * **co-located**: the sharded tables are joined on their shard keys (`orders.id = items.order_id` with both tables
//!   sharded by those columns), so matching rows already live on the same shard;
//! * **broadcast**: a small table is copied to every shard (as a session temporary table) and joined with the rows each
//!   shard holds of the big one; replicated tables need no copy;
//! * **repartition** (shuffle): a table is re-spread over the shards by its join column, with the same hash the other
//!   table's shard key uses, so the rows that join meet on one shard.
//!
//! The planner picks the *anchor*, the sharded table(s) that stay where they are, so as to move the fewest rows, rewrites
//! the `FROM` clause to read the copies, and hands the rewritten query to the scatter-gather planner (`plan.rs`), which adds
//! the two-phase aggregation, ordering and merging. An outer join is planned only if its preserved side is partitioned
//! over the shards (a replicated or broadcast table on the preserved side would produce its unmatched rows once per
//! shard). Anything it cannot plan (subqueries, derived tables, full joins, joins on expressions, ...) is gathered.
use crate::astutil::*;
use crate::{Dist, DistTable};
use std::collections::HashMap;
use tsql_server::ast::*;
use tsql_server::Value;
use tsql_shared_ces::route::KeyClass;

pub(crate) struct Leaf {
    pub(crate) table: DistTable,
    pub(crate) alias: String,
}

pub(crate) enum Node {
    Leaf(usize),
    Join(JoinKind, Box<Node>, Box<Node>),
}

/// A copy of a table to make on every shard before the query runs.
#[derive(Clone, Debug)]
pub struct PrepSpec {
    pub table: DistTable,
    pub alias: String,
    /// the name of the session temporary table on the shards
    pub temp: String,
    /// conditions of the query that mention only this table, applied while copying
    pub filter: Option<Expr>,
    /// None: every shard gets every row (broadcast); Some((column, class)): a row goes to the shard that owns the hash of
    /// that column (repartition)
    pub repartition: Option<(usize, KeyClass)>,
    /// columns to index the copy on once it is filled (a subquery that looks the copy up for each outer row needs them)
    pub index_on: Vec<String>,
    /// column positions of the table to copy (None: copy all columns)
    pub cols: Option<Vec<usize>>,
}

pub struct JoinPlan {
    /// the query to scatter (its `FROM` reads the temporary copies)
    pub query: Query,
    pub preps: Vec<PrepSpec>,
    pub description: String,
}

pub(crate) fn val_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bit(x), Value::Bit(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        (Value::Dec(x), Value::Dec(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bin(x), Value::Bin(y)) => x == y,
        (Value::Date(x), Value::Date(y)) => x == y,
        (Value::Time(x), Value::Time(y)) => x == y,
        (Value::DateTime(x, k1), Value::DateTime(y, k2)) => x == y && k1 == k2,
        (Value::Dto(x), Value::Dto(y)) => x == y,
        (Value::Guid(x), Value::Guid(y)) => x == y,
        _ => false,
    }
}

pub(crate) fn expr_eq(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Lit(va), Expr::Lit(vb)) => val_eq(va, vb),
        (Expr::Col(ca), Expr::Col(cb)) => ca.len() == cb.len() && ca.iter().zip(cb).all(|(x, y)| x.eq_ignore_ascii_case(y)),
        (Expr::Binary(op1, a1, b1), Expr::Binary(op2, a2, b2)) => op1 == op2 && expr_eq(a1, a2) && expr_eq(b1, b2),
        (Expr::InList(e1, l1, n1), Expr::InList(e2, l2, n2)) => n1 == n2 && expr_eq(e1, e2) && l1.len() == l2.len() && l1.iter().zip(l2).all(|(x, y)| expr_eq(x, y)),
        _ => format!("{:?}", a) == format!("{:?}", b),
    }
}

pub(crate) fn conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::And(a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        Expr::Paren(a) => conjuncts(a, out),
        other => out.push(other),
    }
}

pub(crate) fn disjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::Or(a, b) => {
            disjuncts(a, out);
            disjuncts(b, out);
        }
        Expr::Paren(a) => disjuncts(a, out),
        other => out.push(other),
    }
}

pub(crate) fn factor_disjuncts(e: &Expr, leaves: &[Leaf]) -> Vec<Expr> {
    let mut branches = vec![];
    disjuncts(e, &mut branches);
    if branches.len() <= 1 {
        return vec![];
    }
    let mut branch_conjuncts: Vec<Vec<&Expr>> = Vec::with_capacity(branches.len());
    for b in &branches {
        let mut cs = vec![];
        conjuncts(b, &mut cs);
        branch_conjuncts.push(cs);
    }
    let mut derived = vec![];
    for (l, leaf) in leaves.iter().enumerate() {
        let mut per_branch: Vec<Vec<&Expr>> = Vec::with_capacity(branches.len());
        let mut all_have = true;
        for cs in &branch_conjuncts {
            let mut leaf_cs = vec![];
            for &c in cs {
                if has_subquery(c) {
                    continue;
                }
                let mut only = true;
                let mut any = false;
                walk_expr(c, &mut |x| match x {
                    Expr::Col(p) => match resolve(leaves, p) {
                        Some((li, _)) if li == l => any = true,
                        _ => only = false,
                    },
                    Expr::Var(_) | Expr::SysVar(_) | Expr::NextValue(_) => only = false,
                    _ => {}
                });
                if only && any {
                    leaf_cs.push(c);
                }
            }
            if leaf_cs.is_empty() {
                all_have = false;
                break;
            }
            per_branch.push(leaf_cs);
        }
        if !all_have {
            continue;
        }

        let get_eq_col = |cond: &Expr| -> Option<(usize, Vec<Expr>)> {
            match cond {
                Expr::Binary(BinOp::Eq, a, b) => {
                    let col_lit = match (&**a, &**b) {
                        (Expr::Col(p), Expr::Lit(v)) => resolve(leaves, p).map(|(li, c)| (li, c, Expr::Lit(v.clone()))),
                        (Expr::Lit(v), Expr::Col(p)) => resolve(leaves, p).map(|(li, c)| (li, c, Expr::Lit(v.clone()))),
                        _ => None,
                    };
                    if let Some((li, c, lit)) = col_lit {
                        if li == l {
                            return Some((c, vec![lit]));
                        }
                    }
                    None
                }
                Expr::InList(a, list, false) => {
                    if let Expr::Col(p) = &**a {
                        if let Some((li, c)) = resolve(leaves, p) {
                            if li == l && list.iter().all(|x| matches!(x, Expr::Lit(_))) {
                                return Some((c, list.clone()));
                            }
                        }
                    }
                    None
                }
                _ => None,
            }
        };

        let mut common_col: Option<usize> = None;
        let mut combined_lits: Vec<Expr> = vec![];
        let mut can_combine_inlist = true;
        for branch in &per_branch {
            let mut branch_col_found = None;
            for &cond in branch {
                if let Some((c, lits)) = get_eq_col(cond) {
                    if common_col.is_none() || common_col == Some(c) {
                        common_col = Some(c);
                        branch_col_found = Some(lits);
                        break;
                    }
                }
            }
            if let Some(lits) = branch_col_found {
                for lit in lits {
                    if !combined_lits.iter().any(|x| expr_eq(x, &lit)) {
                        combined_lits.push(lit);
                    }
                }
            } else {
                can_combine_inlist = false;
                break;
            }
        }

        if can_combine_inlist && common_col.is_some() && !combined_lits.is_empty() {
            let c = common_col.unwrap();
            let col_name = leaf.table.cols[c].name.clone();
            let col_expr = Box::new(Expr::Col(vec![leaf.alias.clone(), col_name]));
            let factored = if combined_lits.len() == 1 {
                Expr::Binary(BinOp::Eq, col_expr, Box::new(combined_lits.pop().unwrap()))
            } else {
                Expr::InList(col_expr, combined_lits, false)
            };
            derived.push(factored);
        } else {
            let mut branch_or_terms = vec![];
            for branch in per_branch {
                if let Some(conj) = branch.into_iter().cloned().reduce(|a, b| Expr::And(Box::new(a), Box::new(b))) {
                    branch_or_terms.push(conj);
                }
            }
            if let Some(factored) = branch_or_terms.into_iter().reduce(|a, b| Expr::Or(Box::new(a), Box::new(b))) {
                derived.push(factored);
            }
        }
    }
    derived
}

pub(crate) fn propagate_equijoins(
    where_conjuncts: &mut Vec<Expr>,
    eqs: &[(usize, usize, usize, usize)],
    leaves: &[Leaf],
) {
    let mut changed = true;
    while changed {
        changed = false;
        let mut to_add = vec![];
        for &(l1, c1, l2, c2) in eqs {
            let t1 = &leaves[l1].table.cols[c1].type_name;
            let t2 = &leaves[l2].table.cols[c2].type_name;
            if !t1.eq_ignore_ascii_case(t2) {
                if KeyClass::of_type_name(t1) != KeyClass::of_type_name(t2) {
                    continue;
                }
            }
            for w in where_conjuncts.iter() {
                match w {
                    Expr::Binary(BinOp::Eq, a, b) => {
                        let lit = match (&**a, &**b) {
                            (Expr::Col(p), Expr::Lit(v)) if resolve(leaves, p) == Some((l1, c1)) => Some(v.clone()),
                            (Expr::Lit(v), Expr::Col(p)) if resolve(leaves, p) == Some((l1, c1)) => Some(v.clone()),
                            _ => None,
                        };
                        if let Some(val) = lit {
                            let target_col = Box::new(Expr::Col(vec![leaves[l2].alias.clone(), leaves[l2].table.cols[c2].name.clone()]));
                            let pred = Expr::Binary(BinOp::Eq, target_col, Box::new(Expr::Lit(val)));
                            if !where_conjuncts.iter().any(|c| expr_eq(c, &pred)) && !to_add.iter().any(|c| expr_eq(c, &pred)) {
                                to_add.push(pred);
                            }
                        }
                    }
                    Expr::InList(a, list, false) => {
                        if let Expr::Col(p) = &**a {
                            if resolve(leaves, p) == Some((l1, c1)) && list.iter().all(|x| matches!(x, Expr::Lit(_))) {
                                let target_col = Box::new(Expr::Col(vec![leaves[l2].alias.clone(), leaves[l2].table.cols[c2].name.clone()]));
                                let pred = Expr::InList(target_col, list.clone(), false);
                                if !where_conjuncts.iter().any(|c| expr_eq(c, &pred)) && !to_add.iter().any(|c| expr_eq(c, &pred)) {
                                    to_add.push(pred);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !to_add.is_empty() {
            changed = true;
            where_conjuncts.extend(to_add);
        }
    }
}

pub(crate) fn build(tr: &TableRef, tables: &HashMap<String, DistTable>, leaves: &mut Vec<Leaf>, on_preds: &mut Vec<Expr>) -> Option<Node> {
    match tr {
        TableRef::Table { name, alias, .. } => {
            let table = tables.get(&table_id(name))?.clone();
            let alias = alias.clone().or_else(|| name.last().cloned())?;
            leaves.push(Leaf { table, alias });
            Some(Node::Leaf(leaves.len() - 1))
        }
        TableRef::Join { kind, left, right, on } => {
            if !matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Cross) {
                return None;
            }
            let l = build(left, tables, leaves, on_preds)?;
            let r = build(right, tables, leaves, on_preds)?;
            if let Some(on) = on {
                if has_subquery(on) || has_window_or_session(on) {
                    return None;
                }
                let mut cs = vec![];
                conjuncts(on, &mut cs);
                on_preds.extend(cs.into_iter().cloned());
            }
            Some(Node::Join(*kind, Box::new(l), Box::new(r)))
        }
        _ => None,
    }
}

/// The leaf and column a column reference stands for.
pub(crate) fn resolve(leaves: &[Leaf], parts: &[String]) -> Option<(usize, usize)> {
    let col = parts.last()?;
    let by_col = |t: &DistTable| t.cols.iter().position(|c| c.name.eq_ignore_ascii_case(col));
    if parts.len() >= 2 {
        let q = &parts[parts.len() - 2];
        let li = leaves.iter().position(|l| l.alias.eq_ignore_ascii_case(q))?;
        return by_col(&leaves[li].table).map(|c| (li, c));
    }
    let mut found = None;
    for (i, l) in leaves.iter().enumerate() {
        if let Some(c) = by_col(&l.table) {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some((i, c));
        }
    }
    found
}

pub(crate) fn key_of(t: &DistTable) -> Option<(usize, KeyClass)> {
    match t.dist {
        Dist::Sharded { key, class } => Some((key, class)),
        Dist::Replicated => None,
    }
}

fn has_partitioned(n: &Node, part: &[bool]) -> bool {
    match n {
        Node::Leaf(i) => part[*i],
        Node::Join(_, l, r) => has_partitioned(l, part) || has_partitioned(r, part),
    }
}

fn outer_ok(n: &Node, part: &[bool]) -> bool {
    match n {
        Node::Leaf(_) => true,
        Node::Join(kind, l, r) => {
            let side_ok = match kind {
                JoinKind::Left => has_partitioned(l, part),
                JoinKind::Right => has_partitioned(r, part),
                _ => true,
            };
            side_ok && outer_ok(l, part) && outer_ok(r, part)
        }
    }
}

/// An outer join whose preserved side is *not* partitioned over the shards (a replicated table: every shard has all its rows) puts
/// the rows that find no match on this shard in the result of every shard. That is harmless for an aggregate that only counts what
/// matched: each shard gives the preserved row's group a partial count over the matches it holds (0 where it holds none), and the
/// partials add up to the answer (`customer LEFT JOIN orders ... COUNT(o_orderkey) GROUP BY c_custkey`, TPC-H Q13). It holds when:
///
/// * every aggregate is `COUNT`, `SUM`, `MIN`, `MAX` or `AVG` of columns of the partitioned side only, without `DISTINCT` (`COUNT(*)`
///   or an aggregate of a preserved column would count the unmatched row once per shard);
/// * the grouping is by columns of the preserved side only (a group made of the partitioned side's values would show up on a shard
///   that has no row for it), and so are the select items that are not aggregates;
/// * a condition of the `WHERE` that mentions the partitioned side is one a missing row cannot satisfy (a comparison, `BETWEEN`,
///   `IN` or `LIKE`): `WHERE o.x IS NULL` (an anti-join) asks what is true of *all* the shards.
fn outer_agg_ok(q: &Query, leaves: &[Leaf], part: &[bool]) -> bool {
    let QueryBody::Select(sel) = &q.body else { return false };
    if sel.distinct || q.offset.is_some() || q.fetch.is_some() {
        return false;
    }
    // the leaf of a column reference: Some(true) partitioned, Some(false) preserved side, None not a column of the query's tables
    let side = |p: &[String]| resolve(leaves, p).map(|(l, _)| part[l]);
    let mut aggregates = 0usize;
    let mut ok = true;
    let mut check_expr = |e: &Expr, in_group: bool| {
        // columns outside the aggregate calls must be the preserved side's
        map_expr(e, &mut |x| {
            match x {
                Expr::Func(fc) if fc.over.is_some() || fc.within.is_some() => ok = false,
                Expr::Func(fc) if AGGREGATES.contains(&fc.name.as_str()) => {
                    aggregates += 1;
                    let allowed = matches!(fc.name.as_str(), "COUNT" | "COUNT_BIG" | "SUM" | "MIN" | "MAX" | "AVG") && !fc.distinct && !fc.star && !fc.args.is_empty() && !in_group;
                    if !allowed {
                        ok = false;
                    }
                    for a in &fc.args {
                        // the argument must be NULL for a row that found no match (arithmetic and casts of the partitioned side's columns
                        // are; a constant, `CASE`, `ISNULL`, a function are not: `SUM(1)` counts the unmatched row once per shard)
                        let mut has_partitioned_col = false;
                        walk_expr(a, &mut |y| match y {
                            Expr::Col(p) => {
                                if side(p) != Some(true) {
                                    ok = false;
                                } else {
                                    has_partitioned_col = true;
                                }
                            }
                            Expr::Lit(_) | Expr::Paren(_) | Expr::Neg(_) | Expr::Cast { .. } => {}
                            Expr::Binary(op, _, _) if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod) => {}
                            _ => ok = false,
                        });
                        if !has_partitioned_col {
                            ok = false;
                        }
                    }
                    return Some(x.clone());
                }
                Expr::Col(p) => {
                    // (a column that is not inside an aggregate)
                    if side(p) != Some(false) {
                        ok = false;
                    }
                }
                _ => {}
            }
            None
        });
    };
    for it in &sel.items {
        match it {
            SelectItem::Expr { e, .. } => check_expr(e, false),
            _ => return false,
        }
    }
    if let Some(g) = &sel.group {
        for gi in g {
            match gi {
                GroupItem::Expr(e) => check_expr(e, true),
                _ => return false,
            }
        }
    }
    if let Some(h) = &sel.having {
        // (the HAVING is evaluated over the merged groups: its aggregates must be of the same kind)
        check_expr(h, false);
    }
    for o in &q.order {
        if !matches!(&o.e, Expr::Lit(_)) && !matches!(&o.e, Expr::Col(p) if p.len() == 1 && sel.items.iter().any(|i| matches!(i, SelectItem::Expr { alias: Some(a), .. } if a.eq_ignore_ascii_case(&p[0])))) {
            check_expr(&o.e, false);
        }
    }
    if aggregates == 0 || !ok {
        return false;
    }
    // the WHERE: conditions on the partitioned side must be ones a missing row cannot satisfy
    if let Some(w) = &sel.where_ {
        let mut cs = vec![];
        conjuncts(w, &mut cs);
        for c in cs {
            let mut mentions = false;
            let mut unknown = false;
            walk_expr(c, &mut |x| match x {
                Expr::Col(p) => match side(p) {
                    Some(true) => mentions = true,
                    Some(false) => {}
                    None => unknown = true,
                },
                Expr::Exists(_) | Expr::Subquery(_) | Expr::InQuery(..) | Expr::CmpQuant(..) => unknown = true,
                _ => {}
            });
            if unknown || (mentions && !crate::gather::pushable(c)) {
                return false;
            }
        }
    }
    true
}

fn rewrite(tr: &TableRef, idx: &mut usize, leaves: &[Leaf], temps: &[Option<String>]) -> TableRef {
    match tr {
        TableRef::Table { .. } => {
            let i = *idx;
            *idx += 1;
            match &temps[i] {
                Some(t) => TableRef::Table { name: vec![t.clone()], alias: Some(leaves[i].alias.clone()), hints: vec![] },
                None => tr.clone(),
            }
        }
        TableRef::Join { kind, left, right, on } => {
            let l = rewrite(left, idx, leaves, temps);
            let r = rewrite(right, idx, leaves, temps);
            TableRef::Join { kind: *kind, left: Box::new(l), right: Box::new(r), on: on.clone() }
        }
        other => other.clone(),
    }
}

struct Union(Vec<usize>);
impl Union {
    fn find(&mut self, x: usize) -> usize {
        if self.0[x] != x {
            let r = self.find(self.0[x]);
            self.0[x] = r;
        }
        self.0[x]
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        self.0[ra] = rb;
    }
}

/// Plan the joins of `q` (a plain SELECT over base tables, no nested queries). `rows` are the row counts of the tables,
/// `limit` the largest table (in rows) that is broadcast, `shards` the number of shards, `next_temp` hands out names for
/// the temporary copies. None: not a join this planner can run shard by shard. With `allow_sub` the `WHERE` may have subqueries: they are
/// left as they are, only broadcast copies are made (the caller checks that the shards can evaluate them, see `pushdown.rs`).
pub fn plan_join(q: &Query, tables: &HashMap<String, DistTable>, rows: &HashMap<String, u64>, limit: u64, shards: usize, allow_sub: bool, next_temp: &mut dyn FnMut() -> String) -> Option<JoinPlan> {
    let QueryBody::Select(sel) = &q.body else { return None };
    if sel.from.is_empty() {
        return None;
    }
    let mut leaves: Vec<Leaf> = vec![];
    let mut preds: Vec<Expr> = vec![];
    // the FROM list is a cross join of its items
    let mut roots = vec![];
    for tr in &sel.from {
        roots.push(build(tr, tables, &mut leaves, &mut preds)?);
    }
    let mut root = roots.remove(0);
    for r in roots {
        root = Node::Join(JoinKind::Cross, Box::new(root), Box::new(r));
    }
    let mut where_conjuncts: Vec<&Expr> = vec![];
    if let Some(w) = &sel.where_ {
        if (has_subquery(w) && !allow_sub) || has_window_or_session(w) {
            return None;
        }
        conjuncts(w, &mut where_conjuncts);
    }
    let mut all_where: Vec<Expr> = preds.clone();
    for &w in &where_conjuncts {
        all_where.push(w.clone());
    }
    let mut factored = vec![];
    for w in &all_where {
        factored.extend(factor_disjuncts(w, &leaves));
    }
    for f in factored {
        if !all_where.iter().any(|x| expr_eq(x, &f)) {
            all_where.push(f);
        }
    }
    // column = column conditions between two leaves
    let mut eqs: Vec<(usize, usize, usize, usize)> = vec![];
    for p in &all_where {
        if let Expr::Binary(BinOp::Eq, a, b) = p {
            if let (Expr::Col(x), Expr::Col(y)) = (&**a, &**b) {
                if let (Some((l1, c1)), Some((l2, c2))) = (resolve(&leaves, x), resolve(&leaves, y)) {
                    if l1 != l2 {
                        eqs.push((l1, c1, l2, c2));
                    }
                }
            }
        }
    }
    let mut bi_eqs = eqs.clone();
    for &(l1, c1, l2, c2) in &eqs {
        bi_eqs.push((l2, c2, l1, c1));
    }
    propagate_equijoins(&mut all_where, &bi_eqs, &leaves);
    // the sharded leaves, grouped into co-located classes (joined on their shard keys)
    let sharded: Vec<usize> = (0..leaves.len()).filter(|i| key_of(&leaves[*i].table).is_some()).collect();
    if sharded.is_empty() {
        return None;
    }
    let mut uf = Union((0..leaves.len()).collect());
    for &(l1, c1, l2, c2) in &eqs {
        if let (Some((k1, cl1)), Some((k2, cl2))) = (key_of(&leaves[l1].table), key_of(&leaves[l2].table)) {
            if c1 == k1 && c2 == k2 && cl1 == cl2 {
                uf.union(l1, l2);
            }
        }
    }
    let mut classes: Vec<Vec<usize>> = vec![];
    let mut roots_seen: Vec<usize> = vec![];
    for &l in &sharded {
        let r = uf.find(l);
        match roots_seen.iter().position(|x| *x == r) {
            Some(i) => classes[i].push(l),
            None => {
                roots_seen.push(r);
                classes.push(vec![l]);
            }
        }
    }
    let rows_of = |l: usize| rows.get(&leaves[l].table.id).copied().unwrap_or(u64::MAX / 4);
    let class_rows = |c: &Vec<usize>| c.iter().map(|l| rows_of(*l)).sum::<u64>();
    // the cost (rows moved) and moves of keeping class `a` in place
    let try_anchor = |a: usize| -> Option<(u64, Vec<(usize, Option<(usize, KeyClass)>)>)> {
        let mut cost = 0u64;
        let mut moves = vec![];
        for (ci, c) in classes.iter().enumerate() {
            if ci == a {
                continue;
            }
            let r = class_rows(c);
            let broadcast = if r <= limit { Some(r.saturating_mul(shards as u64)) } else { None };
            // repartition a single table onto the anchor: it must be joined to a shard key of the anchor class
            let repart = if c.len() == 1 {
                let l = c[0];
                eqs.iter().find_map(|&(l1, c1, l2, c2)| {
                    let (mine, other, oc) = if l1 == l { (c1, l2, c2) } else if l2 == l { (c2, l1, c1) } else { return None };
                    if !classes[a].contains(&other) {
                        return None;
                    }
                    let (ok, ocl) = key_of(&leaves[other].table)?;
                    if oc != ok {
                        return None;
                    }
                    let mine_class = KeyClass::of_type_name(&leaves[l].table.cols[mine].type_name)?;
                    if mine_class == ocl {
                        Some((mine, ocl))
                    } else {
                        None
                    }
                })
            } else {
                None
            };
            match (broadcast, repart) {
                (Some(b), Some(_)) if b < r => {
                    cost += b;
                    for &l in c {
                        moves.push((l, None));
                    }
                }
                (_, Some(rp)) => {
                    cost += r;
                    moves.push((c[0], Some(rp)));
                }
                (Some(b), None) => {
                    cost += b;
                    for &l in c {
                        moves.push((l, None));
                    }
                }
                (None, None) => return None,
            }
        }
        Some((cost, moves))
    };
    let mut best: Option<(u64, usize, Vec<(usize, Option<(usize, KeyClass)>)>)> = None;
    for a in 0..classes.len() {
        if let Some((cost, moves)) = try_anchor(a) {
            if best.as_ref().map(|b| cost < b.0).unwrap_or(true) {
                best = Some((cost, a, moves));
            }
        }
    }
    let (_, anchor, moves) = best?;
    if allow_sub && moves.iter().any(|(_, rp)| rp.is_some()) {
        return None;
    }
    let mut part = vec![false; leaves.len()];
    for &l in &classes[anchor] {
        part[l] = true;
    }
    for (l, rp) in &moves {
        if rp.is_some() {
            part[*l] = true;
        }
    }
    if !outer_ok(&root, &part) && !outer_agg_ok(q, &leaves, &part) {
        return None;
    }
    // the conditions of the WHERE clause that mention one moved table only are applied while copying it
    let filter_for = |l: usize| -> Option<Expr> {
        let mut fs: Vec<Expr> = vec![];
        for w in &all_where {
            if has_subquery(w) {
                continue;
            }
            let mut only = true;
            let mut any = false;
            walk_expr(w, &mut |x| match x {
                Expr::Col(p) => match resolve(&leaves, p) {
                    Some((li, _)) if li == l => any = true,
                    _ => only = false,
                },
                Expr::Var(_) | Expr::SysVar(_) | Expr::NextValue(_) => only = false,
                _ => {}
            });
            if only && any && !fs.iter().any(|c| expr_eq(c, w)) {
                fs.push(w.clone());
            }
        }
        fs.into_iter().reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
    };

    // determine which columns of each leaf table are referenced in the query
    let mut used_cols: Vec<Vec<bool>> = leaves.iter().map(|l| vec![false; l.table.cols.len()]).collect();
    let mut star_leaves: Vec<bool> = vec![false; leaves.len()];

    fn note_expr_cols(e: &Expr, leaves: &[Leaf], used_cols: &mut [Vec<bool>]) {
        walk_expr(e, &mut |x| match x {
            Expr::Col(p) => {
                if let Some((l, c)) = resolve(leaves, p) {
                    used_cols[l][c] = true;
                }
            }
            Expr::InQuery(_, sq, _) | Expr::CmpQuant(_, _, _, sq) | Expr::Exists(sq) | Expr::Subquery(sq) => {
                note_query_cols(sq, leaves, used_cols);
            }
            _ => {}
        });
    }

    fn note_query_cols(q: &Query, leaves: &[Leaf], used_cols: &mut [Vec<bool>]) {
        if let QueryBody::Select(s) = &q.body {
            for it in &s.items {
                if let SelectItem::Expr { e, .. } | SelectItem::Assign { e, .. } = it {
                    note_expr_cols(e, leaves, used_cols);
                }
            }
            if let Some(w) = &s.where_ {
                note_expr_cols(w, leaves, used_cols);
            }
            if let Some(g) = &s.group {
                for gi in g {
                    if let GroupItem::Expr(e) = gi {
                        note_expr_cols(e, leaves, used_cols);
                    }
                }
            }
            if let Some(h) = &s.having {
                note_expr_cols(h, leaves, used_cols);
            }
        }
        for o in &q.order {
            note_expr_cols(&o.e, leaves, used_cols);
        }
    }

    for it in &sel.items {
        match it {
            SelectItem::Star(None) => {
                for s in star_leaves.iter_mut() {
                    *s = true;
                }
            }
            SelectItem::Star(Some(qual)) => {
                for (l, leaf) in leaves.iter().enumerate() {
                    let matches_alias = qual.last().is_some_and(|q| q.eq_ignore_ascii_case(&leaf.alias));
                    let matches_name = leaf.table.name.ends_with(qual);
                    if matches_alias || matches_name {
                        star_leaves[l] = true;
                    }
                }
            }
            SelectItem::Expr { e, .. } | SelectItem::Assign { e, .. } => {
                note_expr_cols(e, &leaves, &mut used_cols);
            }
        }
    }
    for p in &preds {
        note_expr_cols(p, &leaves, &mut used_cols);
    }
    if let Some(w) = &sel.where_ {
        note_expr_cols(w, &leaves, &mut used_cols);
    }
    for w in &all_where {
        note_expr_cols(w, &leaves, &mut used_cols);
    }
    if let Some(g) = &sel.group {
        for gi in g {
            if let GroupItem::Expr(e) = gi {
                note_expr_cols(e, &leaves, &mut used_cols);
            }
        }
    }
    if let Some(h) = &sel.having {
        note_expr_cols(h, &leaves, &mut used_cols);
    }
    for o in &q.order {
        note_expr_cols(&o.e, &leaves, &mut used_cols);
    }

    let mut temps: Vec<Option<String>> = vec![None; leaves.len()];
    let mut preps = vec![];
    let mut desc = vec![];
    for (l, rp) in &moves {
        let temp = next_temp();
        temps[*l] = Some(temp.clone());
        let cols = if star_leaves[*l] {
            None
        } else {
            let mut used = used_cols[*l].clone();
            if let Some((rp_col, _)) = rp {
                used[*rp_col] = true;
            }
            let mut picked: Vec<usize> = (0..leaves[*l].table.cols.len()).filter(|&c| used[c]).collect();
            if picked.is_empty() {
                picked.push(0);
            }
            if picked.len() == leaves[*l].table.cols.len() {
                None
            } else {
                Some(picked)
            }
        };
        preps.push(PrepSpec { table: leaves[*l].table.clone(), alias: leaves[*l].alias.clone(), temp, filter: filter_for(*l), repartition: *rp, index_on: vec![], cols });
        desc.push(format!("{} {}", if rp.is_some() { "repartition" } else { "broadcast" }, leaves[*l].table.id));
    }
    let new_from = {
        let mut idx = 0usize;
        sel.from.iter().map(|tr| rewrite(tr, &mut idx, &leaves, &temps)).collect::<Vec<_>>()
    };
    let mut nq = q.clone();
    if let QueryBody::Select(s) = &mut nq.body {
        s.from = new_from;
    }
    let description = if preps.is_empty() { "co-located join".to_string() } else { desc.join(", ") };
    Some(JoinPlan { query: nq, preps, description })
}
