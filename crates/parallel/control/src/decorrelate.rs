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
use serde_json::Value as J;
use std::cell::RefCell;
use std::collections::HashMap;
use tsql_server::ast::*;

use crate::astutil::*;

thread_local! {
    /// the columns (lower case) of the tables the query reads, by table id: how an unqualified column name is told to belong to
    /// the subquery or to the query around it
    static SCHEMA: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
    /// the columns of the tables of the subquery being taken apart, and of the outer query
    static COLS: RefCell<(Vec<String>, Vec<String>)> = RefCell::new((vec![], vec![]));
    /// the plain tables of the outer query: (alias in lower case, the table reference, its columns in lower case)
    static OUTER: RefCell<Vec<(String, TableRef, Vec<String>)>> = RefCell::new(vec![]);
}

/// The columns of the tables in a `FROM` (from the schema given to [`decorrelate`]).
fn columns_of(from: &[TableRef]) -> Vec<String> {
    fn one(t: &TableRef, out: &mut Vec<String>) {
        match t {
            TableRef::Table { name, .. } => SCHEMA.with(|s| {
                if let Some(c) = s.borrow().get(&table_id(name)) {
                    out.extend(c.iter().cloned());
                }
            }),
            TableRef::Join { left, right, .. } => {
                one(left, out);
                one(right, out);
            }
            _ => {}
        }
    }
    let mut out = vec![];
    for t in from {
        one(t, &mut out);
    }
    out
}

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
                // (an unqualified name binds to the subquery's tables first, then to the outer query's)
                None => {
                    let n = p.last().map(|s| s.to_lowercase()).unwrap_or_default();
                    let (in_sub, in_outer) = COLS.with(|c| {
                        let c = c.borrow();
                        (c.0.contains(&n), c.1.contains(&n))
                    });
                    if !in_sub && in_outer {
                        has_outer = true
                    } else {
                        has_inner = true
                    }
                }
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
    COLS.with(|c| c.borrow_mut().0 = columns_of(&s.from));
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
pub fn decorrelate(q: &mut Query, counter: &mut u64, schema: HashMap<String, Vec<String>>) -> bool {
    SCHEMA.with(|s| *s.borrow_mut() = schema);
    let QueryBody::Select(sel) = &mut q.body else { return false };
    COLS.with(|c| c.borrow_mut().1 = columns_of(&sel.from));
    OUTER.with(|o| {
        *o.borrow_mut() = sel
            .from
            .iter()
            .filter_map(|t| match t {
                TableRef::Table { name, alias, .. } => Some((alias.clone().or_else(|| name.last().cloned())?.to_lowercase(), t.clone(), columns_of(std::slice::from_ref(t)))),
                _ => None,
            })
            .collect()
    });
    let Some(w) = sel.where_.clone() else { return false };
    let mut outer = vec![];
    aliases(&sel.from, &mut outer);
    let mut conj = vec![];
    conjuncts(&w, &mut conj);
    let mut changed = false;
    let mut out: Vec<Expr> = vec![];
    let mut extra_from: Vec<TableRef> = vec![];
    for (i, c) in conj.iter().enumerate() {
        match rewrite_conjunct(c, &conj, i, &outer, counter, &mut extra_from) {
            Some(r) => {
                changed = true;
                out.extend(r);
            }
            None => out.push(c.clone()),
        }
    }
    if changed {
        sel.where_ = and_all(out);
        sel.from.extend(extra_from);
    }
    changed
}

/// Whether a subquery refers to nothing but its own tables (it does not depend on the query around it): every column it names is a
/// column of a table of its own `FROM` (or qualified by one of its own aliases).
fn uncorrelated(q: &Query) -> bool {
    let QueryBody::Select(s) = &q.body else { return false };
    let mut own_aliases = vec![];
    aliases(&s.from, &mut own_aliases);
    let own_cols = columns_of(&s.from);
    fn walk(v: &J, aliases: &[String], cols: &[String]) -> bool {
        match v {
            J::Object(m) => m.iter().all(|(k, c)| {
                if k == "Col" {
                    let parts: Vec<String> = c.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_lowercase())).collect()).unwrap_or_default();
                    match parts.len() {
                        0 => false,
                        1 => cols.contains(&parts[0]),
                        n => aliases.contains(&parts[n - 2]),
                    }
                } else {
                    walk(c, aliases, cols)
                }
            }),
            J::Array(a) => a.iter().all(|c| walk(c, aliases, cols)),
            _ => true,
        }
    }
    serde_json::to_value(q).map(|v| walk(&v, &own_aliases, &own_cols)).unwrap_or(false)
}

/// The restriction of the grouped inner query to the keys that can matter: the subquery is evaluated for every key of its table, but
/// the outer query only meets the keys of the outer rows that survive the conditions of its `WHERE` that mention the outer table
/// of the key alone. So the derived table is restricted to `inner_key IN (SELECT DISTINCT outer_key FROM that_table WHERE those
/// conditions)`: a superset of what is needed (the other conditions only remove outer rows), and often a tiny one (TPC-H Q17 wants
/// the averages of about twenty parts, not of all of them). None when no condition says anything about that table.
fn reduction(parts: &Parts, conj: &[Expr], me: usize) -> Option<Expr> {
    OUTER.with(|o| {
        let o = o.borrow();
        // the outer table a column reference names
        let owner = |p: &[String]| -> Option<usize> {
            let n = p.last()?.to_lowercase();
            if p.len() >= 2 {
                let q = p[p.len() - 2].to_lowercase();
                return o.iter().position(|(a, _, _)| *a == q);
            }
            let mut found = None;
            for (i, (_, _, cols)) in o.iter().enumerate() {
                if cols.contains(&n) {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(i);
                }
            }
            found
        };
        for (inner, outer_e) in &parts.keys {
            let Expr::Col(p) = outer_e else { continue };
            let Some(a) = owner(p) else { continue };
            let mut filters: Vec<Expr> = vec![];
            for (i, c) in conj.iter().enumerate() {
                if i == me {
                    continue;
                }
                let (mut only, mut any, mut risky) = (true, false, false);
                walk_expr(c, &mut |x| match x {
                    Expr::Col(q) => match owner(q) {
                        Some(b) if b == a => any = true,
                        _ => only = false,
                    },
                    // (a subquery that depends on the outer query may refer to other tables of it: only one that stands alone is taken)
                    Expr::Exists(q) | Expr::Subquery(q) | Expr::InQuery(_, q, _) | Expr::CmpQuant(_, _, _, q) => {
                        if !uncorrelated(q) {
                            risky = true
                        }
                    }
                    Expr::Var(_) | Expr::SysVar(_) => risky = true,
                    _ => {}
                });
                if only && any && !risky {
                    filters.push(c.clone());
                }
            }
            if filters.is_empty() {
                continue;
            }
            let list = simple_query(Select {
                distinct: true,
                top: None,
                items: vec![SelectItem::Expr { e: outer_e.clone(), alias: None }],
                into: None,
                from: vec![o[a].1.clone()],
                where_: and_all(filters),
                group: None,
                having: None,
            });
            return Some(Expr::InQuery(Box::new(inner.clone()), Box::new(list), false));
        }
        None
    })
}

fn rewrite_conjunct(c: &Expr, conj: &[Expr], me: usize, outer: &[String], counter: &mut u64, extra_from: &mut Vec<TableRef>) -> Option<Vec<Expr>> {
    match c {
        Expr::Exists(sq) => exists(sq, false, outer, conj, me),
        Expr::Not(inner) => match &**inner {
            Expr::Exists(sq) => exists(sq, true, outer, conj, me),
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
            let mut inner_where = parts.rest.clone();
            if let Some(r) = reduction(&parts, conj, me) {
                inner_where.push(r);
            }
            let derived = simple_query(Select { distinct: false, top: None, items, into: None, from: parts.from.clone(), where_: and_all(inner_where), group: Some(group), having: None });
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

fn exists(sq: &Query, negated: bool, outer: &[String], conj: &[Expr], me: usize) -> Option<Vec<Expr>> {
    let parts = take_apart(sq, outer)?;
    if parts.keys.len() != 1 {
        return None;
    }
    let (inner_key, outer_key) = parts.keys[0].clone();
    let mut rest = parts.rest.clone();
    if negated {
        rest.push(Expr::IsNull(Box::new(inner_key.clone()), true));
    }
    // (the grouped scalar case restricts its keys to those of the surviving outer rows, see `reduction`; here that was tried and made
    // TPC-H Q22 slower: the restricted list is a long literal IN list, which the engine tests against every row of the scan. The
    // `conj` and `me` arguments stay for when the engine tests long lists with a hash.)
    let _ = (conj, me);
    // (distinct: the list is a set, and a long one is made a replicated table, so it must not repeat itself)
    let list = simple_query(Select {
        distinct: true,
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
