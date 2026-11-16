//! Correlated subqueries rewritten into joins.
//!
//! A subquery that refers to the query around it cannot be evaluated alone, so the whole statement used to be gathered. The
//! common shapes are rewritten before that, into queries whose parts do not depend on the outer rows (which `nested.rs` then
//! evaluates around the shards):
//!
//! * a scalar aggregate subquery compared in the `WHERE` (`x > (SELECT AVG(y) FROM t WHERE t.k = o.k)`) becomes a join with a
//!   grouped derived table (`(SELECT k, AVG(y) FROM t GROUP BY k) d ... d.k = o.k`). A row with no match gets a NULL from the
//!   subquery, which makes the comparison false, as the join dropping the row does; `COUNT` is not rewritten (it gives 0, not
//!   NULL, for no rows);
//! * `EXISTS (SELECT .. WHERE t.k = o.k AND ..)` becomes `o.k IN (SELECT t.k .. WHERE ..)`, and `NOT EXISTS` the same with
//!   `NOT IN`, written so that a NULL key behaves as `NOT EXISTS` does.
//!
//! Only equalities between a column of the subquery and a *qualified* column of the outer query are recognised; anything else
//! is left alone (and the statement is gathered, which is always exact). A rewrite that turns out not to bind is undone by the
//! caller, which runs the original.
use tsql_server::ast::*;

use crate::astutil::*;

/// The names (aliases, or table names) a `FROM` makes visible.
fn aliases(from: &[TableRef], out: &mut Vec<String>) {
    for t in from {
        one(t, out);
    }
}

fn one(t: &TableRef, out: &mut Vec<String>) {
    match t {
        TableRef::Table { name, alias, .. } | TableRef::Temporal { name, alias, .. } => {
            if let Some(n) = alias.clone().or_else(|| name.last().cloned()) {
                out.push(n.to_lowercase());
            }
        }
        TableRef::TableVar { name, alias } => out.push(alias.clone().unwrap_or_else(|| name.clone()).to_lowercase()),
        TableRef::Derived { alias, .. } | TableRef::Values { alias, .. } | TableRef::Func { alias, .. } | TableRef::Pivot { alias, .. } | TableRef::Unpivot { alias, .. } => {
            if let Some(a) = alias {
                out.push(a.to_lowercase());
            }
        }
        TableRef::Join { left, right, .. } => {
            one(left, out);
            one(right, out);
        }
    }
}

fn conjuncts(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::And(a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        Expr::Paren(a) if matches!(**a, Expr::And(..)) => conjuncts(a, out),
        other => out.push(other.clone()),
    }
}

fn and_all(mut parts: Vec<Expr>) -> Option<Expr> {
    let first = if parts.is_empty() { return None } else { parts.remove(0) };
    Some(parts.into_iter().fold(first, |a, b| Expr::And(Box::new(a), Box::new(b))))
}

#[derive(PartialEq, Clone, Copy)]
enum Side {
    /// no column at all
    Const,
    /// columns of the subquery (or unqualified: those bind to the subquery first)
    Inner,
    /// only qualified columns of the outer query
    Outer,
    Mixed,
}

fn side(e: &Expr, inner: &[String], outer: &[String]) -> Side {
    let (mut has_inner, mut has_outer, mut bad) = (false, false, false);
    walk_expr(e, &mut |x| match x {
        Expr::Col(p) => {
            let q = if p.len() >= 2 { Some(p[p.len() - 2].to_lowercase()) } else { None };
            match q {
                Some(q) if inner.contains(&q) => has_inner = true,
                Some(q) if outer.contains(&q) => has_outer = true,
                Some(_) => bad = true,
                None => has_inner = true,
            }
        }
        Expr::Exists(_) | Expr::Subquery(_) | Expr::InQuery(..) | Expr::CmpQuant(..) | Expr::Var(_) | Expr::SysVar(_) => bad = true,
        _ => {}
    });
    match (has_inner, has_outer, bad) {
        (_, _, true) => Side::Mixed,
        (false, false, _) => Side::Const,
        (true, false, _) => Side::Inner,
        (false, true, _) => Side::Outer,
        _ => Side::Mixed,
    }
}

/// A subquery taken apart: its keys (inner expression, outer expression), what is left of its `WHERE`, and its `FROM`.
struct Parts {
    keys: Vec<(Expr, Expr)>,
    rest: Vec<Expr>,
    from: Vec<TableRef>,
    item: Option<Expr>,
}

fn take_apart(q: &Query, outer: &[String]) -> Option<Parts> {
    if !q.ctes.is_empty() || !q.order.is_empty() || q.offset.is_some() || q.fetch.is_some() || q.for_clause.is_some() {
        return None;
    }
    let QueryBody::Select(s) = &q.body else { return None };
    if s.top.is_some() || s.distinct || s.group.is_some() || s.having.is_some() || s.into.is_some() || s.items.len() != 1 {
        return None;
    }
    let item = match &s.items[0] {
        SelectItem::Expr { e, .. } => Some(e.clone()),
        SelectItem::Star(_) => None,
        _ => return None,
    };
    let mut inner = vec![];
    aliases(&s.from, &mut inner);
    // the subquery's own tables must be plain: no nested queries anywhere in it
    if s.from.iter().any(|t| !matches!(t, TableRef::Table { .. } | TableRef::Join { .. })) {
        return None;
    }
    let outer: Vec<String> = outer.iter().filter(|o| !inner.contains(o)).cloned().collect();
    if let Some(i) = &item {
        if !matches!(side(i, &inner, &outer), Side::Inner | Side::Const) {
            return None;
        }
    }
    let mut parts = vec![];
    if let Some(w) = &s.where_ {
        conjuncts(w, &mut parts);
    }
    let mut keys = vec![];
    let mut rest = vec![];
    for c in parts {
        match side(&c, &inner, &outer) {
            Side::Inner | Side::Const => rest.push(c),
            Side::Outer => return None,
            Side::Mixed => {
                // must be `inner expression = outer expression` (either way round)
                let Expr::Binary(BinOp::Eq, a, b) = &c else { return None };
                match (side(a, &inner, &outer), side(b, &inner, &outer)) {
                    (Side::Inner, Side::Outer) => keys.push(((**a).clone(), (**b).clone())),
                    (Side::Outer, Side::Inner) => keys.push(((**b).clone(), (**a).clone())),
                    _ => return None,
                }
            }
        }
    }
    if keys.is_empty() {
        return None;
    }
    Some(Parts { keys, rest, from: s.from.clone(), item })
}

fn has_count(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |x| {
        if let Expr::Func(fc) = x {
            if matches!(fc.name.as_str(), "COUNT" | "COUNT_BIG") {
                found = true;
            }
        }
    });
    found
}

/// An aggregate expression: at least one aggregate call, and no column outside the aggregates.
fn aggregate_only(e: &Expr) -> bool {
    let (mut has_agg, mut bare) = (false, false);
    let _ = map_expr(e, &mut |x| {
        if is_aggregate_call(x).is_some() {
            has_agg = true;
            return Some(x.clone());
        }
        if matches!(x, Expr::Col(_)) {
            bare = true;
        }
        None
    });
    has_agg && !bare
}

/// Rewrite the correlated subqueries of the top-level `WHERE` of `q`. True when something was rewritten.
pub fn decorrelate(q: &mut Query, counter: &mut u64) -> bool {
    let QueryBody::Select(sel) = &mut q.body else { return false };
    let Some(w) = sel.where_.clone() else { return false };
    let mut outer = vec![];
    aliases(&sel.from, &mut outer);
    let mut conj = vec![];
    conjuncts(&w, &mut conj);
    let mut changed = false;
    let mut out: Vec<Expr> = vec![];
    let mut extra_from: Vec<TableRef> = vec![];
    for c in conj {
        match rewrite_conjunct(&c, &outer, counter, &mut extra_from) {
            Some(r) => {
                changed = true;
                out.extend(r);
            }
            None => out.push(c),
        }
    }
    if changed {
        sel.where_ = and_all(out);
        sel.from.extend(extra_from);
    }
    changed
}

fn rewrite_conjunct(c: &Expr, outer: &[String], counter: &mut u64, extra_from: &mut Vec<TableRef>) -> Option<Vec<Expr>> {
    match c {
        Expr::Exists(sq) => exists(sq, false, outer),
        Expr::Not(inner) => match &**inner {
            Expr::Exists(sq) => exists(sq, true, outer),
            _ => None,
        },
        Expr::Binary(op, a, b) if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) => {
            // the subquery sits, through arithmetic only, on one side
            let (side_a, side_b) = (find_subquery(a), find_subquery(b));
            let (sq, on_left) = match (side_a, side_b) {
                (Some(q), None) => (q, true),
                (None, Some(q)) => (q, false),
                _ => return None,
            };
            let parts = take_apart(&sq, outer)?;
            let item = parts.item.clone()?;
            if !aggregate_only(&item) || has_count(&item) {
                return None;
            }
            *counter += 1;
            let alias = format!("__dc{}", counter);
            let mut items: Vec<SelectItem> = parts.keys.iter().enumerate().map(|(i, (k, _))| SelectItem::Expr { e: k.clone(), alias: Some(format!("dk{}", i)) }).collect();
            items.push(SelectItem::Expr { e: item, alias: Some("dv".into()) });
            let group: Vec<GroupItem> = parts.keys.iter().map(|(k, _)| GroupItem::Expr(k.clone())).collect();
            let derived = simple_query(Select { distinct: false, top: None, items, into: None, from: parts.from.clone(), where_: and_all(parts.rest.clone()), group: Some(group), having: None });
            extra_from.push(TableRef::Derived { query: Box::new(derived), alias: Some(alias.clone()), cols: None });
            let value = Expr::Col(vec![alias.clone(), "dv".into()]);
            let replaced = if on_left { Expr::Binary(*op, Box::new(replace_subquery(a, &value)), b.clone()) } else { Expr::Binary(*op, a.clone(), Box::new(replace_subquery(b, &value))) };
            let mut out = vec![replaced];
            for (i, (_, o)) in parts.keys.iter().enumerate() {
                out.push(Expr::Binary(BinOp::Eq, Box::new(Expr::Col(vec![alias.clone(), format!("dk{}", i)])), Box::new(o.clone())));
            }
            Some(out)
        }
        _ => None,
    }
}

/// The subquery in `e` when `e` is arithmetic (which gives NULL for a NULL) around exactly one.
fn find_subquery(e: &Expr) -> Option<Query> {
    match e {
        Expr::Subquery(q) => Some((**q).clone()),
        Expr::Paren(a) | Expr::Neg(a) => find_subquery(a),
        Expr::Cast { e: a, .. } => find_subquery(a),
        Expr::Binary(op, a, b) if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) => match (find_subquery(a), find_subquery(b)) {
            (Some(q), None) if !has_subquery(b) => Some(q),
            (None, Some(q)) if !has_subquery(a) => Some(q),
            _ => None,
        },
        _ => None,
    }
}

fn replace_subquery(e: &Expr, with: &Expr) -> Expr {
    match e {
        Expr::Subquery(_) => with.clone(),
        Expr::Paren(a) => Expr::Paren(Box::new(replace_subquery(a, with))),
        Expr::Neg(a) => Expr::Neg(Box::new(replace_subquery(a, with))),
        Expr::Cast { e: a, ty, try_, style } => Expr::Cast { e: Box::new(replace_subquery(a, with)), ty: ty.clone(), try_: *try_, style: style.clone() },
        Expr::Binary(op, a, b) => {
            if has_subquery(a) {
                Expr::Binary(*op, Box::new(replace_subquery(a, with)), b.clone())
            } else {
                Expr::Binary(*op, a.clone(), Box::new(replace_subquery(b, with)))
            }
        }
        other => other.clone(),
    }
}

fn exists(sq: &Query, negated: bool, outer: &[String]) -> Option<Vec<Expr>> {
    let parts = take_apart(sq, outer)?;
    if parts.keys.len() != 1 {
        return None;
    }
    let (inner_key, outer_key) = parts.keys[0].clone();
    let mut rest = parts.rest.clone();
    if negated {
        rest.push(Expr::IsNull(Box::new(inner_key.clone()), true));
    }
    let list = simple_query(Select {
        distinct: false,
        top: None,
        items: vec![SelectItem::Expr { e: inner_key, alias: None }],
        into: None,
        from: parts.from.clone(),
        where_: and_all(rest),
        group: None,
        having: None,
    });
    if negated {
        // a NULL key finds no match, so NOT EXISTS holds for it
        let not_in = Expr::InQuery(Box::new(outer_key.clone()), Box::new(list), true);
        Some(vec![Expr::Paren(Box::new(Expr::Or(Box::new(Expr::IsNull(Box::new(outer_key), false)), Box::new(not_in))))])
    } else {
        Some(vec![Expr::InQuery(Box::new(outer_key), Box::new(list), false)])
    }
}
