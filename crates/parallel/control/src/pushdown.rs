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
use crate::joins::{resolve, Leaf, PrepSpec};
use crate::{Dist, DistTable};
use tsql_server::ast::*;
use tsql_shared_ces::route::KeyClass;

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

/// The tables of a subquery's `FROM` (plain tables, joined by `JOIN` or commas) with the conditions of its `ON` clauses; None for
/// anything else (an outer join, a derived table, ...).
fn flatten_from(from: &[TableRef], lookup: &dyn Fn(&str) -> Option<DistTable>, refs: &mut Vec<(String, DistTable)>, on: &mut Vec<Expr>) -> Option<()> {
    for t in from {
        match t {
            TableRef::Table { name, alias, .. } => {
                let dt = lookup(&table_id(name))?;
                refs.push((visible_name(name, alias), dt));
            }
            TableRef::Join { kind: JoinKind::Inner | JoinKind::Cross, left, right, on: cond } => {
                flatten_from(std::slice::from_ref(&**left), lookup, refs, on)?;
                flatten_from(std::slice::from_ref(&**right), lookup, refs, on)?;
                if let Some(c) = cond {
                    on.push(c.clone());
                }
            }
            _ => return None,
        }
    }
    Some(())
}

/// Whether the subquery can be run by the shards. `leaves` are the tables of the outer query, `class` the sharded ones among
/// them (co-located with each other: joined on their shard keys); `lookup` finds a table by id. Some(true): it reads sharded
/// tables that are co-located with them; Some(false): it reads replicated tables only; None: the shards cannot run it.
///
/// The sharded tables of the subquery (there can be several, joined) must all be tied to the outer query's by equalities of shard keys
/// of one class: each is equal, in the `WHERE` or an `ON`, to the shard key of another of them or of an outer co-located table, and
/// all of them are connected to the outer one. The rows of the subquery that belong to an outer row then all lie on the shard of that
/// row, whatever else is asked of them.
fn sub_ok(sq: &Query, leaves: &[Leaf], class: &[usize], lookup: &dyn Fn(&str) -> Option<DistTable>) -> Option<bool> {
    if !sq.ctes.is_empty() || !sq.order.is_empty() || sq.offset.is_some() || sq.fetch.is_some() || sq.for_clause.is_some() {
        return None;
    }
    let QueryBody::Select(s) = &sq.body else { return None };
    if s.top.is_some() || s.into.is_some() || has_nested_query(sq) {
        return None;
    }
    let mut refs: Vec<(String, DistTable)> = vec![];
    let mut on: Vec<Expr> = vec![];
    flatten_from(&s.from, lookup, &mut refs, &mut on)?;
    let sharded: Vec<usize> = (0..refs.len()).filter(|i| matches!(refs[*i].1.dist, Dist::Sharded { .. })).collect();
    if sharded.is_empty() {
        return Some(false);
    }
    let mut cs = vec![];
    if let Some(w) = &s.where_ {
        conjuncts(w, &mut cs);
    }
    for c in &on {
        conjuncts(c, &mut cs);
    }
    let inner_all: Vec<&DistTable> = refs.iter().map(|(_, t)| t).collect();
    // does `e` name the shard key of the subquery's table number `i`? (an unqualified name binds to the subquery first, and must
    // belong to that table alone)
    let inner_key = |e: &Expr, i: usize| -> bool {
        let Dist::Sharded { key, .. } = &refs[i].1.dist else { return false };
        let key_name = &refs[i].1.cols[*key].name;
        match e {
            Expr::Col(p) if p.last().map(|x| x.eq_ignore_ascii_case(key_name)).unwrap_or(false) => match p.len() {
                1 => inner_all.iter().filter(|t| t.cols.iter().any(|c| c.name.eq_ignore_ascii_case(key_name))).count() == 1,
                n => p[n - 2].to_lowercase() == refs[i].0,
            },
            _ => false,
        }
    };
    // the class of a sharded table's key, to compare
    let class_of = |t: &DistTable| match &t.dist {
        Dist::Sharded { class, .. } => Some(class.clone()),
        _ => None,
    };
    // the shard key of one of the outer query's co-located tables, named so that it cannot be a column of the subquery's tables
    let outer_class = |e: &Expr| -> Option<KeyClass> {
        let Expr::Col(p) = e else { return None };
        if p.len() == 1 && inner_all.iter().any(|o| o.cols.iter().any(|c| c.name.eq_ignore_ascii_case(&p[0]))) {
            return None;
        }
        // (a qualifier that is a table of the subquery is not the outer query's)
        if p.len() >= 2 && refs.iter().any(|(a, _)| *a == p[p.len() - 2].to_lowercase()) {
            return None;
        }
        let (l, c) = resolve(leaves, p)?;
        match &leaves[l].table.dist {
            Dist::Sharded { key, class: cl } if class.contains(&l) && *key == c => Some(cl.clone()),
            _ => None,
        }
    };
    // union-find over the subquery's sharded tables (by position in `sharded`) and one node for the outer query
    let n = sharded.len();
    let out = n;
    let mut parent: Vec<usize> = (0..=n).collect();
    fn find(p: &mut Vec<usize>, x: usize) -> usize {
        if p[x] != x {
            let r = find(p, p[x]);
            p[x] = r;
        }
        p[x]
    }
    for c in cs {
        let Expr::Binary(BinOp::Eq, a, b) = c else { continue };
        for (x, y) in [(&**a, &**b), (&**b, &**a)] {
            for (xi, &i) in sharded.iter().enumerate() {
                if !inner_key(x, i) {
                    continue;
                }
                let mine = class_of(&refs[i].1);
                // tied to the outer query's key
                if outer_class(y).is_some() && outer_class(y) == mine {
                    let (r1, r2) = (find(&mut parent, xi), find(&mut parent, out));
                    parent[r1] = r2;
                }
                // tied to another of the subquery's tables
                for (yi, &j) in sharded.iter().enumerate() {
                    if yi != xi && inner_key(y, j) && class_of(&refs[j].1) == mine {
                        let (r1, r2) = (find(&mut parent, xi), find(&mut parent, yi));
                        parent[r1] = r2;
                    }
                }
            }
        }
    }
    let root = find(&mut parent, out);
    if (0..n).all(|i| find(&mut parent, i) == root) {
        Some(true)
    } else {
        None
    }
}

/// The outer query of a pushdown: its tables (plain tables, all in the `FROM` list or joined), which of them are sharded (they meet on
/// their shard keys, one class), and its `WHERE`; None when the query is not of that kind or has subqueries anywhere but the `WHERE`.
fn outer_of<'q>(q: &'q Query, lookup: &dyn Fn(&str) -> Option<DistTable>) -> Option<(Vec<Leaf>, Vec<usize>, &'q Expr)> {
    if !q.ctes.is_empty() {
        return None;
    }
    let QueryBody::Select(sel) = &q.body else { return None };
    let mut leaves: Vec<Leaf> = vec![];
    for t in &sel.from {
        let TableRef::Table { name, alias, .. } = t else { return None };
        let Some(dt) = lookup(&table_id(name)) else { return None };
        leaves.push(Leaf { table: dt, alias: alias.clone().or_else(|| name.last().cloned()).unwrap_or_default() });
    }
    let sharded: Vec<usize> = (0..leaves.len()).filter(|i| matches!(leaves[*i].table.dist, Dist::Sharded { .. })).collect();
    if sharded.is_empty() {
        return None;
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
                _ => return None,
            }
        }
    }
    if elsewhere.iter().any(|e| has_subquery(e)) {
        return None;
    }
    let Some(w) = &sel.where_ else { return None };
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
        return None;
    }
    Some((leaves, sharded, w))
}

/// Whether the query (plain tables joined shard by shard, subqueries in its `WHERE`) can have its subqueries evaluated by the
/// shards. The sharded tables of the query must be co-located (joined on their shard keys). With `need_colocated` at least one
/// subquery must read a sharded table (the case where nothing else could have been done without a gather); without, subqueries
/// over replicated tables alone count too.
pub fn pushdown_ok(q: &Query, lookup: &dyn Fn(&str) -> Option<DistTable>, need_colocated: bool) -> bool {
    let Some((leaves, sharded, w)) = outer_of(q, lookup) else { return false };
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

/// A query whose `WHERE` has subqueries the shards cannot answer because they read a sharded table that is not co-located with the outer
/// query (`history` by its customer, next to `customer` by its warehouse), made answerable by copying the tables of such a subquery to every
/// shard, when every one of them is small (`rows`: a table's row count, `limit`: the largest table copied): the subquery then reads
/// copies that every shard holds whole. Returns the query with those tables renamed to the copies and the copies to make; None when
/// some subquery cannot be made answerable that way, or when none needed it.
pub fn with_small_tables_copied(q: &Query, lookup: &dyn Fn(&str) -> Option<DistTable>, rows: &dyn Fn(&str) -> Option<u64>, limit: u64, next_temp: &mut dyn FnMut() -> String) -> Option<(Query, Vec<PrepSpec>)> {
    let (leaves, sharded, w) = outer_of(q, lookup)?;
    let mut preps: Vec<PrepSpec> = vec![];
    let mut failed = false;
    let mut changed = false;
    let new_w = map_expr(w, &mut |x| {
        let mut rewrite = |sq: &Query, preps: &mut Vec<PrepSpec>, failed: &mut bool, changed: &mut bool| -> Option<Box<Query>> {
            if sub_ok(sq, &leaves, &sharded, lookup).is_some() {
                return None;
            }
            // only a plain subquery of plain tables can have its tables renamed
            let QueryBody::Select(s) = &sq.body else {
                *failed = true;
                return None;
            };
            let mut refs: Vec<(String, DistTable)> = vec![];
            let mut on: Vec<Expr> = vec![];
            if flatten_from(&s.from, lookup, &mut refs, &mut on).is_none() {
                *failed = true;
                return None;
            }
            let mut conds: Vec<&Expr> = vec![];
            if let Some(w) = &s.where_ {
                conjuncts(w, &mut conds);
            }
            for c in &on {
                conjuncts(c, &mut conds);
            }
            let mut names: Vec<(String, String)> = vec![];
            for (alias, t) in &refs {
                if !matches!(t.dist, Dist::Sharded { .. }) {
                    continue;
                }
                if rows(&t.id).is_none_or(|n| n > limit) {
                    *failed = true;
                    return None;
                }
                let temp = next_temp();
                // the columns the subquery compares for equality, to look the copy up by (the correlation columns)
                let mut index_on: Vec<String> = vec![];
                for c in &conds {
                    let Expr::Binary(BinOp::Eq, a, b) = *c else { continue };
                    for side in [&**a, &**b] {
                        let Expr::Col(p) = side else { continue };
                        let col = p.last().cloned().unwrap_or_default();
                        let mine = t.cols.iter().find(|x| x.name.eq_ignore_ascii_case(&col));
                        let named = p.len() == 1 || p[p.len() - 2].to_lowercase() == *alias;
                        if let (Some(m), true) = (mine, named) {
                            if !index_on.iter().any(|x| x.eq_ignore_ascii_case(&m.name)) && index_on.len() < 4 {
                                index_on.push(m.name.clone());
                            }
                        }
                    }
                }
                preps.push(PrepSpec { table: t.clone(), alias: alias.clone(), temp: temp.clone(), filter: None, repartition: None, index_on, cols: None });
                names.push((t.id.clone(), temp));
            }
            fn rename(tr: &TableRef, names: &[(String, String)]) -> TableRef {
                match tr {
                    TableRef::Table { name, alias, hints } => match names.iter().find(|(id, _)| *id == table_id(name)) {
                        // (the copy is seen under the name the table was seen under)
                        Some((_, temp)) => TableRef::Table { name: vec![temp.clone()], alias: Some(alias.clone().unwrap_or_else(|| name.last().cloned().unwrap_or_default())), hints: hints.clone() },
                        None => tr.clone(),
                    },
                    TableRef::Join { kind, left, right, on } => TableRef::Join { kind: *kind, left: Box::new(rename(left, names)), right: Box::new(rename(right, names)), on: on.clone() },
                    other => other.clone(),
                }
            }
            let mut nq = sq.clone();
            if let QueryBody::Select(ns) = &mut nq.body {
                ns.from = s.from.iter().map(|t| rename(t, &names)).collect();
            }
            *changed = true;
            Some(Box::new(nq))
        };
        match x {
            Expr::Subquery(sq) => rewrite(sq, &mut preps, &mut failed, &mut changed).map(Expr::Subquery),
            Expr::Exists(sq) => rewrite(sq, &mut preps, &mut failed, &mut changed).map(Expr::Exists),
            Expr::InQuery(a, sq, neg) => rewrite(sq, &mut preps, &mut failed, &mut changed).map(|nq| Expr::InQuery(a.clone(), nq, *neg)),
            _ => None,
        }
    });
    if failed || !changed {
        return None;
    }
    let mut nq = q.clone();
    if let QueryBody::Select(s) = &mut nq.body {
        s.where_ = Some(new_w);
    }
    Some((nq, preps))
}
