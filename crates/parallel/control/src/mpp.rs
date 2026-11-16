//! Joins in stages: the shuffle between joins.
//!
//! `joins.rs` runs a join shard by shard when one move is enough: some tables stay where they are, the others are broadcast or
//! repartitioned onto them. A join whose tables are sharded on *different* keys (TPC-H Q9: `lineitem` by order key,
//! `partsupp` by part key, `orders` by order key again) needs more: the result of the first joins has to be re-spread by the
//! next join's column before it can meet the next table. Until now such a query was gathered into the control node's engine.
//!
//! The planner here works through the join graph like a left-deep pipeline over the shards:
//!
//! * the *group* is what every shard has joined so far; it starts as the largest sharded table, partitioned by its shard key;
//! * the next table is added where it is when the shards can do it alone: a replicated table, one sharded on the very column the
//!   group is partitioned by (co-located), or a small one copied to every shard (broadcast);
//! * otherwise rows move, in a *stage*: a shard runs a scan (the joins done so far, or one table with its filters) and sends each
//!   row to the shard that owns the hash of a column (action `shuffle`, shard to shard: the rows do not pass through the control
//!   node), where they are put in a session temporary table. Either the group is re-spread by the column that joins the next
//!   table, or the next table is re-spread by its join column, whichever moves fewer rows;
//! * when every table is in, the query (its select list, `GROUP BY`, ... rewritten over the temporary tables) runs shard by shard
//!   as an ordinary scatter-gather: two-phase aggregation on the shards, and only the small final result reaches the control node.
//!
//! The result is the single-node answer; only where the work happens changes.
use crate::astutil::*;
use crate::joins::{build, conjuncts, key_of, resolve, Leaf, Node};
use crate::plan;
use crate::DistTable;
use std::collections::HashMap;
use tsql_server::ast::*;
use tsql_shared_ces::route::KeyClass;

/// One movement of rows: run `scan` on every shard and send each row to the shard that owns the hash of the column `hash`
/// (index in the scan's select list; None: to every shard) into the session temporary table `temp`.
pub struct Stage {
    pub temp: String,
    pub scan: Select,
    pub hash: Option<(usize, KeyClass)>,
    pub note: String,
}

pub struct MppPlan {
    pub stages: Vec<Stage>,
    /// the query to scatter once the stages are done
    pub query: Query,
    pub description: String,
}

#[derive(Clone)]
enum Src {
    Native(usize),
    Temp { name: String, leaves: Vec<usize> },
}

struct St<'a> {
    leaves: &'a [Leaf],
    /// per leaf, per column: used anywhere in the query
    used: Vec<Vec<bool>>,
    /// the conditions of the WHERE and ON clauses, with the leaves they mention
    preds: Vec<(Expr, Vec<usize>)>,
    applied: Vec<bool>,
    /// column = column conditions between two leaves
    eqs: Vec<(usize, usize, usize, usize)>,
    srcs: Vec<Src>,
    members: Vec<usize>,
    /// columns the group is partitioned by (all equal), and the hash class of their values
    part: Vec<(usize, usize)>,
    gclass: KeyClass,
}

fn class_of(l: &Leaf, c: usize) -> Option<KeyClass> {
    KeyClass::of_type_name(&l.table.cols[c].type_name)
}

fn temp_col(l: &Leaf, c: usize) -> String {
    format!("{}__{}", l.alias, l.table.cols[c].name)
}

impl<'a> St<'a> {
    fn colref(&self, leaf: usize, col: usize) -> Expr {
        for s in &self.srcs {
            if let Src::Temp { name, leaves } = s {
                if leaves.contains(&leaf) {
                    return Expr::Col(vec![name.clone(), temp_col(&self.leaves[leaf], col)]);
                }
            }
        }
        Expr::Col(vec![self.leaves[leaf].alias.clone(), self.leaves[leaf].table.cols[col].name.clone()])
    }

    fn rewrite(&self, e: &Expr) -> Expr {
        map_expr(e, &mut |x| match x {
            Expr::Col(p) => resolve(self.leaves, p).map(|(l, c)| self.colref(l, c)),
            _ => None,
        })
    }

    /// The columns equal to the ones the group is partitioned by, through the equalities that hold in the group.
    fn closure(&self) -> Vec<(usize, usize)> {
        let mut set = self.part.clone();
        loop {
            let mut grew = false;
            for &(l1, c1, l2, c2) in &self.eqs {
                if !self.members.contains(&l1) || !self.members.contains(&l2) {
                    continue;
                }
                if set.contains(&(l1, c1)) && !set.contains(&(l2, c2)) {
                    set.push((l2, c2));
                    grew = true;
                }
                if set.contains(&(l2, c2)) && !set.contains(&(l1, c1)) {
                    set.push((l1, c1));
                    grew = true;
                }
            }
            if !grew {
                return set;
            }
        }
    }

    fn from_of(&self, srcs: &[Src]) -> Vec<TableRef> {
        srcs.iter()
            .map(|s| match s {
                Src::Native(l) => TableRef::Table { name: self.leaves[*l].table.name.clone(), alias: Some(self.leaves[*l].alias.clone()), hints: vec![] },
                Src::Temp { name, .. } => TableRef::Table { name: vec![name.clone()], alias: Some(name.clone()), hints: vec![] },
            })
            .collect()
    }

    /// A scan of the given sources that outputs the used columns of `out` leaves (named `alias__column`), with the conditions that
    /// mention only leaves in `scope` (those not applied yet). Returns the select and the position of each (leaf, column).
    fn scan(&self, srcs: &[Src], scope: &[usize], out: &[usize]) -> (Select, Vec<(usize, usize)>) {
        let mut items = vec![];
        let mut at = vec![];
        for &l in out {
            for c in 0..self.leaves[l].table.cols.len() {
                if self.used[l][c] {
                    items.push(SelectItem::Expr { e: self.colref(l, c), alias: Some(temp_col(&self.leaves[l], c)) });
                    at.push((l, c));
                }
            }
        }
        let conds: Vec<Expr> = self.preds.iter().enumerate().filter(|(i, (_, ls))| !self.applied[*i] && ls.iter().all(|l| scope.contains(l))).map(|(_, (e, _))| self.rewrite(e)).collect();
        let where_ = conds.into_iter().reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
        (Select { distinct: false, top: None, items, into: None, from: self.from_of(srcs), where_, group: None, having: None }, at)
    }

    fn mark_applied(&mut self, scope: &[usize]) {
        for (i, (_, ls)) in self.preds.iter().enumerate() {
            if ls.iter().all(|l| scope.contains(l)) {
                self.applied[i] = true;
            }
        }
    }
}

/// Plan the joins of `q` (a plain SELECT over base tables, no nested queries) in stages. None: not a query this planner can run
/// (it has outer joins, `*`, windows, a join without a column = column condition, ...), or it needs no stage (the join planner
/// of `joins.rs` has run it, or nothing is sharded). `rows` are the row counts of the tables, `limit` the largest table that is
/// broadcast, `shards` the number of shards.
pub fn plan_mpp(q: &Query, tables: &HashMap<String, DistTable>, rows: &HashMap<String, u64>, limit: u64, shards: usize, next_temp: &mut dyn FnMut() -> String) -> Option<MppPlan> {
    let QueryBody::Select(sel) = &q.body else { return None };
    if !q.ctes.is_empty() || sel.from.is_empty() || sel.into.is_some() || q.for_clause.is_some() {
        return None;
    }
    let mut leaves: Vec<Leaf> = vec![];
    let mut on_preds: Vec<Expr> = vec![];
    for tr in &sel.from {
        let node = build(tr, tables, &mut leaves, &mut on_preds)?;
        // only inner and cross joins: an outer join has rows that no single shard can decide
        fn inner_only(n: &Node) -> bool {
            match n {
                Node::Leaf(_) => true,
                Node::Join(k, l, r) => matches!(k, JoinKind::Inner | JoinKind::Cross) && inner_only(l) && inner_only(r),
            }
        }
        if !inner_only(&node) {
            return None;
        }
    }
    if leaves.len() < 2 || !leaves.iter().any(|l| key_of(&l.table).is_some()) {
        return None;
    }
    // every condition, with the leaves it mentions; every column the query uses
    let mut used: Vec<Vec<bool>> = leaves.iter().map(|l| vec![false; l.table.cols.len()]).collect();
    let mut note_cols = |e: &Expr, used: &mut Vec<Vec<bool>>, strict: bool| -> bool {
        let mut ok = true;
        walk_expr(e, &mut |x| match x {
            Expr::Col(p) => match resolve(&leaves, p) {
                Some((l, c)) => used[l][c] = true,
                None => {
                    if strict {
                        ok = false
                    }
                }
            },
            Expr::Exists(_) | Expr::Subquery(_) | Expr::InQuery(..) | Expr::CmpQuant(..) | Expr::Var(_) | Expr::SysVar(_) | Expr::NextValue(_) => ok = false,
            Expr::Func(fc) if fc.over.is_some() || fc.within.is_some() => ok = false,
            _ => {}
        });
        ok
    };
    let mut raw: Vec<Expr> = on_preds.clone();
    if let Some(w) = &sel.where_ {
        let mut cs = vec![];
        conjuncts(w, &mut cs);
        raw.extend(cs.into_iter().cloned());
    }
    let mut preds: Vec<(Expr, Vec<usize>)> = vec![];
    for p in raw {
        if !note_cols(&p, &mut used, true) {
            return None;
        }
        let mut ls: Vec<usize> = vec![];
        walk_expr(&p, &mut |x| {
            if let Expr::Col(c) = x {
                if let Some((l, _)) = resolve(&leaves, c) {
                    if !ls.contains(&l) {
                        ls.push(l);
                    }
                }
            }
        });
        preds.push((p, ls));
    }
    for it in &sel.items {
        match it {
            SelectItem::Expr { e, .. } => {
                if !note_cols(e, &mut used, false) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if let Some(g) = &sel.group {
        for gi in g {
            match gi {
                GroupItem::Expr(e) => {
                    note_cols(e, &mut used, false);
                }
                _ => return None,
            }
        }
    }
    if let Some(h) = &sel.having {
        note_cols(h, &mut used, false);
    }
    for o in &q.order {
        note_cols(&o.e, &mut used, false);
    }
    let mut eqs = vec![];
    for (p, _) in &preds {
        if let Expr::Binary(BinOp::Eq, a, b) = p {
            if let (Expr::Col(x), Expr::Col(y)) = (&**a, &**b) {
                if let (Some((l1, c1)), Some((l2, c2))) = (resolve(&leaves, x), resolve(&leaves, y)) {
                    if l1 != l2 {
                        eqs.push((l1, c1, l2, c2));
                        eqs.push((l2, c2, l1, c1));
                    }
                }
            }
        }
    }
    let n = leaves.len();
    let rows_of = |l: usize| rows.get(&leaves[l].table.id).copied().unwrap_or(u64::MAX / 4);
    // the start: the largest sharded table, partitioned by its shard key
    let start = (0..n).filter(|l| key_of(&leaves[*l].table).is_some()).max_by_key(|l| rows_of(*l))?;
    let (skey, sclass) = key_of(&leaves[start].table)?;
    let applied = vec![false; preds.len()];
    let mut st = St { leaves: &leaves, used, preds, applied, eqs, srcs: vec![Src::Native(start)], members: vec![start], part: vec![(start, skey)], gclass: sclass };
    let mut remaining: Vec<usize> = (0..n).filter(|l| *l != start).collect();
    let mut stages: Vec<Stage> = vec![];
    let mut desc: Vec<String> = vec![];
    let group_rows = |st: &St| st.members.iter().filter(|l| key_of(&leaves[**l].table).is_some()).map(|l| rows_of(*l)).max().unwrap_or(0);
    while !remaining.is_empty() {
        let group_class = Some(st.gclass);
        let closure = st.closure();
        // what each remaining table needs: (priority, cost, leaf, action)
        #[derive(Clone, Debug)]
        enum Act {
            Local { colocated: Option<usize> },
            Broadcast,
            ShuffleT { ycol: usize },
            ShuffleGroup { x: (usize, usize), y: usize, t_aligned: bool },
        }
        let mut best: Option<(u8, u64, usize, Act)> = None;
        for &t in &remaining {
            let edges: Vec<(usize, usize, usize)> = st.eqs.iter().filter(|(l1, _, l2, _)| *l2 == t && st.members.contains(l1)).map(|(l1, c1, _, c2)| (*l1, *c1, *c2)).collect();
            if edges.is_empty() {
                continue;
            }
            let tab = &leaves[t].table;
            let cand: Option<(u8, u64, Act)> = match key_of(tab) {
                None => Some((0, 0, Act::Local { colocated: None })),
                Some((tkey, tclass)) => {
                    let colo = edges.iter().find(|(xl, xc, yc)| closure.contains(&(*xl, *xc)) && *yc == tkey && class_of(&leaves[*xl], *xc) == Some(tclass) && group_class == Some(tclass));
                    if let Some(&(_, _, yc)) = colo {
                        Some((1, 0, Act::Local { colocated: Some(yc) }))
                    } else if rows_of(t) <= limit {
                        Some((2, rows_of(t).saturating_mul(shards as u64), Act::Broadcast))
                    } else {
                        let mut opts: Vec<(u64, Act)> = vec![];
                        // the group is already partitioned by a column equal to a join column: move the table only
                        for &(xl, xc, yc) in &edges {
                            if closure.contains(&(xl, xc)) && class_of(&leaves[t], yc).is_some() && class_of(&leaves[t], yc) == class_of(&leaves[xl], xc) {
                                opts.push((rows_of(t), Act::ShuffleT { ycol: yc }));
                            }
                        }
                        // else move the group by the join column (and the table too unless it is already partitioned by its end)
                        for &(xl, xc, yc) in &edges {
                            let (cx, cy) = (class_of(&leaves[xl], xc), class_of(&leaves[t], yc));
                            if cx.is_none() || cx != cy {
                                continue;
                            }
                            let aligned = yc == tkey && cx == Some(tclass);
                            opts.push((group_rows(&st).saturating_add(if aligned { 0 } else { rows_of(t) }), Act::ShuffleGroup { x: (xl, xc), y: yc, t_aligned: aligned }));
                        }
                        opts.into_iter().min_by_key(|o| o.0).map(|(c, a)| (3, c, a))
                    }
                }
            };
            if let Some((pr, cost, act)) = cand {
                if best.as_ref().map(|b| (pr, cost, rows_of(t)) < (b.0, b.1, rows_of(b.2))).unwrap_or(true) {
                    best = Some((pr, cost, t, act));
                }
            }
        }
        let (_, _, t, act) = best?;
        let empty = |sel: &Select| sel.items.is_empty();
        remaining.retain(|l| *l != t);
        let t_scan = |st: &St, hash_col: Option<usize>| -> (Select, Option<(usize, KeyClass)>) {
            let (sel, at) = st.scan(&[Src::Native(t)], &[t], &[t]);
            let h = hash_col.and_then(|c| at.iter().position(|a| *a == (t, c)).and_then(|i| class_of(&leaves[t], c).map(|k| (i, k))));
            (sel, h)
        };
        match act {
            Act::Local { colocated } => {
                st.srcs.push(Src::Native(t));
                st.members.push(t);
                if let Some(yc) = colocated {
                    st.part.push((t, yc));
                }
                desc.push(format!("join {} in place", leaves[t].table.id));
            }
            Act::Broadcast => {
                let (sel, _) = t_scan(&st, None);
                if empty(&sel) {
                    return None;
                }
                let temp = next_temp();
                stages.push(Stage { temp: temp.clone(), scan: sel, hash: None, note: format!("broadcast {}", leaves[t].table.id) });
                st.mark_applied(&[t]);
                st.srcs.push(Src::Temp { name: temp, leaves: vec![t] });
                st.members.push(t);
                desc.push(format!("broadcast {}", leaves[t].table.id));
            }
            Act::ShuffleT { ycol } => {
                let (sel, h) = t_scan(&st, Some(ycol));
                if empty(&sel) {
                    return None;
                }
                let temp = next_temp();
                stages.push(Stage { temp: temp.clone(), scan: sel, hash: Some(h?), note: format!("repartition {} by {}", leaves[t].table.id, leaves[t].table.cols[ycol].name) });
                st.mark_applied(&[t]);
                st.srcs.push(Src::Temp { name: temp, leaves: vec![t] });
                st.members.push(t);
                st.part.push((t, ycol));
                desc.push(format!("repartition {}", leaves[t].table.id));
            }
            Act::ShuffleGroup { x, y, t_aligned } => {
                // the group, as joined so far, is sent by the column x
                let members = st.members.clone();
                let (sel, at) = st.scan(&st.srcs.clone(), &members, &members);
                if empty(&sel) {
                    return None;
                }
                let pos = at.iter().position(|a| *a == x)?;
                let class = class_of(&leaves[x.0], x.1)?;
                let temp = next_temp();
                stages.push(Stage { temp: temp.clone(), scan: sel, hash: Some((pos, class)), note: format!("repartition the join of {} tables by {}", members.len(), leaves[x.0].table.cols[x.1].name) });
                st.mark_applied(&members);
                st.srcs = vec![Src::Temp { name: temp, leaves: members }];
                st.part = vec![x];
                st.gclass = class;
                desc.push(format!("repartition the group by {}", leaves[x.0].table.cols[x.1].name));
                if t_aligned {
                    st.srcs.push(Src::Native(t));
                    st.members.push(t);
                    st.part.push((t, y));
                    desc.push(format!("join {} in place", leaves[t].table.id));
                } else {
                    let (sel, h) = t_scan(&st, Some(y));
                    if empty(&sel) {
                        return None;
                    }
                    let temp = next_temp();
                    stages.push(Stage { temp: temp.clone(), scan: sel, hash: Some(h?), note: format!("repartition {} by {}", leaves[t].table.id, leaves[t].table.cols[y].name) });
                    st.mark_applied(&[t]);
                    st.srcs.push(Src::Temp { name: temp, leaves: vec![t] });
                    st.members.push(t);
                    st.part.push((t, y));
                    desc.push(format!("repartition {}", leaves[t].table.id));
                }
            }
        }
    }
    if stages.is_empty() {
        return None;
    }
    // the query over what the stages left: its FROM, the conditions not yet applied, and every column reference rewritten
    let mut nq = q.clone();
    let aliases: Vec<String> = sel.items.iter().filter_map(|i| if let SelectItem::Expr { alias: Some(a), .. } = i { Some(a.to_lowercase()) } else { None }).collect();
    if let QueryBody::Select(s) = &mut nq.body {
        s.from = st.from_of(&st.srcs);
        let rest: Vec<Expr> = st.preds.iter().enumerate().filter(|(i, _)| !st.applied[*i]).map(|(_, (e, _))| st.rewrite(e)).collect();
        s.where_ = rest.into_iter().reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
        for it in s.items.iter_mut() {
            if let SelectItem::Expr { e, alias } = it {
                // (a bare column keeps its name in the result)
                if alias.is_none() {
                    if let Expr::Col(p) = e {
                        *alias = p.last().cloned();
                    }
                }
                *e = st.rewrite(e);
            }
        }
        if let Some(g) = &mut s.group {
            for gi in g.iter_mut() {
                if let GroupItem::Expr(e) = gi {
                    *e = st.rewrite(e);
                }
            }
        }
        if let Some(h) = &mut s.having {
            *h = st.rewrite(h);
        }
    }
    for o in nq.order.iter_mut() {
        let is_alias = matches!(&o.e, Expr::Col(p) if p.len() == 1 && aliases.contains(&p[0].to_lowercase()));
        if !is_alias && !matches!(o.e, Expr::Lit(_)) {
            o.e = st.rewrite(&o.e);
        }
    }
    // it must be a query that the shards can run and the control node can merge
    plan::plan_scatter_multi(&nq)?;
    Some(MppPlan { stages, query: nq, description: desc.join("; ") })
}
