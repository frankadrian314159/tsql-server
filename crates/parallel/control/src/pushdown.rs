//! Subqueries that the shards can evaluate themselves.
//!
//! A subquery in the `WHERE` of a query over one sharded table does not have to be evaluated by the control node (as a list or a
//! value, or as a join with a copy of its result) when each shard can answer it from what the shard holds:
//!
//! * **it reads only replicated tables** (the rest of the query, correlated or not, is then run by every shard with its own
//!   copy: this is also what a large `IN (SELECT ...)` becomes once its result is made a replicated temporary table);
//! * **it is co-located with the outer query**: it reads one sharded table, and an equality in its `WHERE` ties that table's
//!   shard key to the shard key of the outer table (`EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND ...)`
//!   from `orders`, both sharded on the order key). Rows that agree on the key live on the same shard, so whatever else the
//!   subquery asks of them can be asked there.
//!
//! The whole query is then planned as a scatter-gather (see `plan::plan_scatter_sub`) with the subquery left inside it.
use crate::astutil::*;
use crate::joins::{resolve, Leaf};
use crate::{Dist, DistTable};
use tsql_server::ast::*;

fn conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::And(a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        Expr::Paren(a) => conjuncts(a, out),
        other => out.push(other),
    }
}

/// Every subquery of an expression (not those inside them).
fn subqueries(e: &Expr) -> Vec<&Query> {
    let mut out = vec![];
    fn collect<'a>(e: &'a Expr, out: &mut Vec<&'a Query>) {
        match e {
            Expr::Exists(q) | Expr::Subquery(q) => out.push(q),
            Expr::InQuery(a, q, _) => {
                collect(a, out);
                out.push(q);
            }
            Expr::CmpQuant(_, _, a, q) => {
                collect(a, out);
                out.push(q);
            }
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Binary(_, a, b) | Expr::IsDistinct(a, b, _) | Expr::AtTimeZone(a, b) => {
                collect(a, out);
                collect(b, out);
            }
            Expr::Not(a) | Expr::Neg(a) | Expr::BitNot(a) | Expr::Paren(a) | Expr::IsNull(a, _) | Expr::Collate(a, _) => collect(a, out),
            Expr::Between(a, b, c, _) => {
                collect(a, out);
                collect(b, out);
                collect(c, out);
            }
            Expr::Cast { e, .. } => collect(e, out),
            Expr::Case { operand, whens, els } => {
                if let Some(o) = operand {
                    collect(o, out);
                }
                for (w, t) in whens {
                    collect(w, out);
                    collect(t, out);
                }
                if let Some(x) = els {
                    collect(x, out);
                }
            }
            Expr::Func(fc) => {
                for a in &fc.args {
                    collect(a, out);
                }
            }
            Expr::InList(a, l, _) => {
                collect(a, out);
                for x in l {
                    collect(x, out);
                }
            }
            Expr::Like(a, p, esc, _) => {
                collect(a, out);
                collect(p, out);
                if let Some(x) = esc {
                    collect(x, out);
                }
            }
            _ => {}
        }
    }
    collect(e, &mut out);
    out
}

/// The name a table is known by in a `FROM` entry (its alias, or its name).
fn visible_name(name: &[String], alias: &Option<String>) -> String {
    alias.clone().or_else(|| name.last().cloned()).unwrap_or_default().to_lowercase()
}

/// Whether the column reference names column `col` of the table seen as `me`: qualified by that name, or unqualified when no
/// other table in `others` has a column of that name (and `me` does, as the caller checked).
fn names(e: &Expr, me: &str, col: &str, others: &[&DistTable]) -> bool {
    match e {
        Expr::Col(p) if p.last().map(|c| c.eq_ignore_ascii_case(col)).unwrap_or(false) => match p.len() {
            1 => !others.iter().any(|o| o.cols.iter().any(|c| c.name.eq_ignore_ascii_case(col))),
            n => p[n - 2].to_lowercase() == me,
        },
        _ => false,
    }
}

/// Whether the subquery can be run by the shards. `leaves` are the tables of the outer query, `class` the sharded ones among
/// them (co-located with each other: joined on their shard keys); `lookup` finds a table by id. Some(true): it reads a sharded
/// table that is co-located with them; Some(false): it reads replicated tables only; None: the shards cannot run it.
fn sub_ok(sq: &Query, leaves: &[Leaf], class: &[usize], lookup: &dyn Fn(&str) -> Option<DistTable>) -> Option<bool> {
    if !sq.ctes.is_empty() || !sq.order.is_empty() || sq.offset.is_some() || sq.fetch.is_some() || sq.for_clause.is_some() {
        return None;
    }
    let QueryBody::Select(s) = &sq.body else { return None };
    if s.top.is_some() || s.into.is_some() || has_nested_query(sq) {
        return None;
    }
    // plain tables only
    let mut refs: Vec<(String, DistTable)> = vec![];
    for t in &s.from {
        let TableRef::Table { name, alias, .. } = t else { return None };
        let dt = lookup(&table_id(name))?;
        refs.push((visible_name(name, alias), dt));
    }
    let sharded: Vec<&(String, DistTable)> = refs.iter().filter(|(_, t)| matches!(t.dist, Dist::Sharded { .. })).collect();
    match sharded.len() {
        0 => Some(false),
        1 => {
            let (alias, t2) = sharded[0];
            let Dist::Sharded { key: k2, class: c2 } = &t2.dist else { return None };
            let key2 = t2.cols[*k2].name.clone();
            let inner_all: Vec<&DistTable> = refs.iter().map(|(_, t)| t).collect();
            let w = s.where_.as_ref()?;
            let mut cs = vec![];
            conjuncts(w, &mut cs);
            for c in cs {
                let Expr::Binary(BinOp::Eq, a, b) = c else { continue };
                // inner side: the key of the subquery's table (an unqualified name binds to the subquery first)
                let inner_is = |e: &Expr| match e {
                    Expr::Col(p) if p.last().map(|x| x.eq_ignore_ascii_case(&key2)).unwrap_or(false) => match p.len() {
                        1 => true,
                        n => p[n - 2].to_lowercase() == *alias,
                    },
                    _ => false,
                };
                // outer side: the shard key of one of the outer query's co-located tables, named so that it cannot be a column
                // of the subquery's tables
                let outer_is = |e: &Expr| -> bool {
                    let Expr::Col(p) = e else { return false };
                    if p.len() == 1 && inner_all.iter().any(|o| o.cols.iter().any(|c| c.name.eq_ignore_ascii_case(&p[0]))) {
                        return false;
                    }
                    // (a qualifier that is a table of the subquery is not the outer query's)
                    if p.len() >= 2 && refs.iter().any(|(a, _)| *a == p[p.len() - 2].to_lowercase()) {
                        return false;
                    }
                    match resolve(leaves, p) {
                        Some((l, c)) => class.contains(&l) && matches!(&leaves[l].table.dist, Dist::Sharded { key, class: cl } if *key == c && cl == c2),
                        None => false,
                    }
                };
                if (inner_is(a) && outer_is(b)) || (inner_is(b) && outer_is(a)) {
                    return Some(true);
                }
            }
            None
        }
        _ => None,
    }
}

/// Whether the query (plain tables joined shard by shard, subqueries in its `WHERE`) can have its subqueries evaluated by the
/// shards. The sharded tables of the query must be co-located (joined on their shard keys). With `need_colocated` at least one
/// subquery must read a sharded table (the case where nothing else could have been done without a gather); without, subqueries
/// over replicated tables alone count too.
pub fn pushdown_ok(q: &Query, lookup: &dyn Fn(&str) -> Option<DistTable>, need_colocated: bool) -> bool {
    if !q.ctes.is_empty() {
        return false;
    }
    let QueryBody::Select(sel) = &q.body else { return false };
    let mut leaves: Vec<Leaf> = vec![];
    for t in &sel.from {
        let TableRef::Table { name, alias, .. } = t else { return false };
        let Some(dt) = lookup(&table_id(name)) else { return false };
        leaves.push(Leaf { table: dt, alias: alias.clone().or_else(|| name.last().cloned()).unwrap_or_default() });
    }
    let sharded: Vec<usize> = (0..leaves.len()).filter(|i| matches!(leaves[*i].table.dist, Dist::Sharded { .. })).collect();
    if sharded.is_empty() {
        return false;
    }
    // subqueries only in the WHERE
    let mut elsewhere: Vec<&Expr> = vec![];
    for it in &sel.items {
        if let SelectItem::Expr { e, .. } = it {
            elsewhere.push(e);
        }
    }
    if let Some(h) = &sel.having {
        elsewhere.push(h);
    }
    for o in &q.order {
        elsewhere.push(&o.e);
    }
    if let Some(g) = &sel.group {
        for gi in g {
            match gi {
                GroupItem::Expr(e) => elsewhere.push(e),
                _ => return false,
            }
        }
    }
    if elsewhere.iter().any(|e| has_subquery(e)) {
        return false;
    }
    let Some(w) = &sel.where_ else { return false };
    // the sharded tables meet on their shard keys (`a.key = b.key` conditions of the WHERE): one class
    let mut cs = vec![];
    conjuncts(w, &mut cs);
    let mut parent: Vec<usize> = (0..leaves.len()).collect();
    fn find(p: &mut Vec<usize>, x: usize) -> usize {
        if p[x] != x {
            let r = find(p, p[x]);
            p[x] = r;
        }
        p[x]
    }
    for c in &cs {
        let Expr::Binary(BinOp::Eq, a, b) = c else { continue };
        let (Expr::Col(x), Expr::Col(y)) = (&**a, &**b) else { continue };
        let (Some((l1, c1)), Some((l2, c2))) = (resolve(&leaves, x), resolve(&leaves, y)) else { continue };
        if let (Dist::Sharded { key: k1, class: cl1 }, Dist::Sharded { key: k2, class: cl2 }) = (&leaves[l1].table.dist, &leaves[l2].table.dist) {
            if c1 == *k1 && c2 == *k2 && cl1 == cl2 {
                let (r1, r2) = (find(&mut parent, l1), find(&mut parent, l2));
                parent[r1] = r2;
            }
        }
    }
    let root = find(&mut parent, sharded[0]);
    if sharded.iter().any(|l| find(&mut parent, *l) != root) {
        return false;
    }
    let subs = subqueries(w);
    if subs.is_empty() {
        return false;
    }
    let mut any_sharded = false;
    for sq in subs {
        match sub_ok(sq, &leaves, &sharded, lookup) {
            Some(sharded) => any_sharded |= sharded,
            None => return false,
        }
    }
    any_sharded || !need_colocated
}
