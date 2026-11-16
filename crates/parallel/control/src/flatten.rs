//! Merging a derived table into the query that reads it.
//!
//! `SELECT nation, SUM(amount) FROM (SELECT n_name AS nation, l_extendedprice * ... AS amount FROM part, lineitem, ... WHERE ...) p
//! GROUP BY nation` computes the join of the derived table and hands its whole result to the outer query. In a cluster that result
//! is big (rows, not groups) and has to travel to the control node before the outer aggregation can start. When the derived table
//! is a plain select-project-join over several tables (no `DISTINCT`, `TOP`, grouping, aggregate, window, set operation or
//! subquery), the two levels are one query: its tables join the outer `FROM`, its conditions join the outer `WHERE`, and every
//! reference to one of its columns becomes the expression that column is. The query then plans as a whole (staged joins and
//! two-phase aggregation over the shards), and only the groups reach the control node.
//!
//! A derived table over one table is left alone (it is cheap to make, and replicated, as it is).
use crate::astutil::*;
use tsql_server::ast::*;

fn simple_spj(q: &Query) -> Option<&Select> {
    if !q.ctes.is_empty() || !q.order.is_empty() || q.offset.is_some() || q.fetch.is_some() || q.for_clause.is_some() || q.maxrecursion.is_some() {
        return None;
    }
    let QueryBody::Select(s) = &q.body else { return None };
    if s.distinct || s.top.is_some() || s.into.is_some() || s.group.is_some() || s.having.is_some() {
        return None;
    }
    // plain tables joined by commas or inner joins
    fn inner_tables(t: &TableRef, n: &mut usize) -> bool {
        match t {
            TableRef::Table { .. } => {
                *n += 1;
                true
            }
            TableRef::Join { kind, left, right, .. } => matches!(kind, JoinKind::Inner | JoinKind::Cross) && inner_tables(left, n) && inner_tables(right, n),
            _ => false,
        }
    }
    let mut n = 0;
    if !s.from.iter().all(|t| inner_tables(t, &mut n)) || n < 2 {
        return None;
    }
    for it in &s.items {
        match it {
            SelectItem::Expr { e, .. } => {
                let mut bad = false;
                walk_expr(e, &mut |x| match x {
                    Expr::Func(fc) if fc.over.is_some() || fc.within.is_some() || AGGREGATES.contains(&fc.name.as_str()) => bad = true,
                    Expr::Var(_) | Expr::SysVar(_) | Expr::NextValue(_) => bad = true,
                    _ => {}
                });
                if bad || has_subquery(e) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if let Some(w) = &s.where_ {
        if has_subquery(w) || has_window_or_session(w) {
            return None;
        }
    }
    Some(s)
}

fn aliases_of(t: &TableRef, out: &mut Vec<String>) {
    match t {
        TableRef::Table { name, alias, .. } => out.push(alias.clone().or_else(|| name.last().cloned()).unwrap_or_default().to_lowercase()),
        TableRef::Join { left, right, .. } => {
            aliases_of(left, out);
            aliases_of(right, out);
        }
        TableRef::Derived { alias, .. } => out.push(alias.clone().unwrap_or_default().to_lowercase()),
        TableRef::Temporal { name, alias, .. } => out.push(alias.clone().or_else(|| name.last().cloned()).unwrap_or_default().to_lowercase()),
        TableRef::TableVar { name, alias } => out.push(alias.clone().unwrap_or_else(|| name.clone()).to_lowercase()),
        TableRef::Values { alias, .. } | TableRef::Func { alias, .. } | TableRef::Pivot { alias, .. } | TableRef::Unpivot { alias, .. } => out.push(alias.clone().unwrap_or_default().to_lowercase()),
    }
}

/// The query with its first mergeable derived table merged in, or None when there is none (call again for the next one).
pub fn flatten_derived(q: &Query) -> Option<Query> {
    if !q.ctes.is_empty() {
        return None;
    }
    let QueryBody::Select(sel) = &q.body else { return None };
    // the outer query: a plain select over the derived table (and perhaps other tables), no subqueries
    let mut exprs: Vec<&Expr> = vec![];
    for it in &sel.items {
        match it {
            SelectItem::Expr { e, .. } => exprs.push(e),
            _ => return None,
        }
    }
    if let Some(w) = &sel.where_ {
        exprs.push(w);
    }
    if let Some(h) = &sel.having {
        exprs.push(h);
    }
    for o in &q.order {
        exprs.push(&o.e);
    }
    if let Some(g) = &sel.group {
        for gi in g {
            match gi {
                GroupItem::Expr(e) => exprs.push(e),
                _ => return None,
            }
        }
    }
    if exprs.iter().any(|e| has_subquery(e) || has_window_or_session(e)) {
        return None;
    }
    // a derived table at the top of the FROM list (not under an outer join, which would change what a missing row means)
    let at = sel.from.iter().position(|t| matches!(t, TableRef::Derived { query, .. } if simple_spj(query).is_some()))?;
    let TableRef::Derived { query: dq, alias, cols } = &sel.from[at] else { return None };
    let inner = simple_spj(dq)?;
    let alias = alias.clone()?.to_lowercase();
    // its columns: name -> expression
    let mut names: Vec<(String, Expr)> = vec![];
    for (i, it) in inner.items.iter().enumerate() {
        let SelectItem::Expr { e, alias: a } = it else { return None };
        let name = match cols {
            Some(c) => c.get(i)?.clone(),
            None => a.clone().or_else(|| if let Expr::Col(p) = e { p.last().cloned() } else { None })?,
        };
        names.push((name.to_lowercase(), e.clone()));
    }
    // the inner tables must not collide with the names the outer query uses
    let (mut outer_names, mut inner_names) = (vec![], vec![]);
    for (i, t) in sel.from.iter().enumerate() {
        if i != at {
            aliases_of(t, &mut outer_names);
        }
    }
    for t in &inner.from {
        aliases_of(t, &mut inner_names);
    }
    if inner_names.iter().any(|n| outer_names.contains(n) || *n == alias) {
        return None;
    }
    // unqualified names are the derived table's only when it is the only table of the outer query
    let sole = sel.from.len() == 1;
    let mut failed = false;
    let mut rewrite = |e: &Expr| -> Expr {
        map_expr(e, &mut |x| match x {
            Expr::Col(p) => {
                let n = p.last()?.to_lowercase();
                let mine = match p.len() {
                    1 => sole,
                    k => p[k - 2].to_lowercase() == alias,
                };
                if !mine {
                    return None;
                }
                match names.iter().find(|(m, _)| *m == n) {
                    Some((_, ex)) => Some(Expr::Paren(Box::new(ex.clone()))),
                    None => {
                        failed = true;
                        None
                    }
                }
            }
            _ => None,
        })
    };
    let mut nq = q.clone();
    let QueryBody::Select(ns) = &mut nq.body else { return None };
    for it in ns.items.iter_mut() {
        if let SelectItem::Expr { e, alias: a } = it {
            // (a bare column of the derived table keeps its name in the result)
            if a.is_none() {
                if let Expr::Col(p) = e {
                    *a = p.last().cloned();
                }
            }
            *e = rewrite(e);
        }
    }
    let mut conds: Vec<Expr> = vec![];
    if let Some(w) = &inner.where_ {
        conds.push(w.clone());
    }
    if let Some(w) = &ns.where_ {
        conds.push(rewrite(w));
    }
    ns.where_ = conds.into_iter().reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
    if let Some(h) = &ns.having {
        ns.having = Some(rewrite(h));
    }
    if let Some(g) = &mut ns.group {
        for gi in g.iter_mut() {
            if let GroupItem::Expr(e) = gi {
                *e = rewrite(e);
            }
        }
    }
    // ORDER BY may name a select alias, which is not a derived column unless it is the column itself
    let select_aliases: Vec<String> = sel.items.iter().filter_map(|i| if let SelectItem::Expr { alias: Some(a), .. } = i { Some(a.to_lowercase()) } else { None }).collect();
    for o in nq.order.iter_mut() {
        let is_alias = matches!(&o.e, Expr::Col(p) if p.len() == 1 && select_aliases.contains(&p[0].to_lowercase()));
        if !is_alias && !matches!(o.e, Expr::Lit(_)) {
            o.e = rewrite(&o.e);
        }
    }
    if failed {
        return None;
    }
    let QueryBody::Select(ns) = &mut nq.body else { return None };
    let mut from = vec![];
    for (i, t) in sel.from.iter().enumerate() {
        if i == at {
            from.extend(inner.from.iter().cloned());
        } else {
            from.push(t.clone());
        }
    }
    ns.from = from;
    Some(nq)
}
