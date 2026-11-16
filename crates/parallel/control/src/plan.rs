//! Planning a `SELECT` over one sharded table as a scatter-gather.
//!
//! Every shard runs a *shard query* over its own rows; the control node then runs a *final query* over the union of the
//! shard results (held in a temporary table of the control node's own engine, so grouping, ordering, collation and
//! TOP behave exactly as in the single-node server). Two shapes qualify:
//!
//! * **row-wise** (no aggregates): the shard query is the query itself (with TOP and, for a TOP, ORDER BY); sort keys
//!   that are not in the select list travel as hidden columns; the final query sorts, applies DISTINCT and TOP;
//! * **aggregate**: the classic two-phase aggregation. The shard query returns the group keys and *partial* aggregates
//!   (SUM, COUNT, MIN, MAX as themselves, AVG as a SUM and a COUNT); the final query groups the partial rows and combines
//!   them (SUM of the SUMs and COUNTs, MIN of the MINs, SUM / SUM for AVG), with HAVING, ORDER BY and the select list
//!   rewritten over the combined values.
//!
//! Anything else (joins, subqueries, windows, DISTINCT aggregates, ROLLUP, ...) is not planned here and takes the
//! gather path, which is always correct.
use crate::astutil::*;
use tsql_server::ast::*;
use tsql_server::DataType;

/// The prefix of the columns of the temporary table of shard results (chosen so as not to collide with user columns).
pub const COL: &str = "__dp_c";

pub fn cname(i: usize) -> String {
    format!("{}{}", COL, i)
}

pub enum Shape {
    Row(RowPlan),
    Agg(AggPlan),
}

#[derive(Clone, Debug)]
enum RowOrder {
    Visible(usize, bool),
    Hidden(usize, bool),
}

pub struct RowPlan {
    pub shard_query: Query,
    n_hidden: usize,
    distinct: bool,
    top: Option<Top>,
    order: Vec<RowOrder>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum AggKind {
    Count,
    CountBig,
    Sum,
    Min,
    Max,
    Avg,
}

struct AggInfo {
    key: String,
    kind: AggKind,
    /// the partial column (the SUM of an AVG)
    col: usize,
    /// the COUNT column of an AVG
    count_col: Option<usize>,
}

pub struct AggPlan {
    pub shard_query: Query,
    group_keys: Vec<String>,
    aggs: Vec<AggInfo>,
    items: Vec<(Expr, Option<String>)>,
    having: Option<Expr>,
    order: Vec<OrderItem>,
    top: Option<Top>,
    distinct: bool,
}

fn only_select(q: &Query) -> Option<&Select> {
    if q.for_clause.is_some() || q.offset.is_some() || q.fetch.is_some() || q.maxrecursion.is_some() {
        return None;
    }
    match &q.body {
        QueryBody::Select(s) if s.into.is_none() => Some(s),
        _ => None,
    }
}

fn table_item(name: &str) -> Vec<TableRef> {
    vec![TableRef::Table { name: vec![name.to_string()], alias: None, hints: vec![] }]
}

/// Whether two column references can name the same column: the same name, and if both are qualified, the same qualifier
/// (`o.id` is not `c.id`).
fn last_part_eq(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Col(x), Expr::Col(y)) => {
            let same_name = x.last().map(|s| s.to_lowercase()) == y.last().map(|s| s.to_lowercase());
            if x.len() >= 2 && y.len() >= 2 {
                same_name && x[x.len() - 2].eq_ignore_ascii_case(&y[y.len() - 2])
            } else {
                same_name
            }
        }
        _ => false,
    }
}

/// Plan a query over a single sharded table (the caller has checked that it reads nothing else and has no nested
/// query). None: not a shape this planner handles.
pub fn plan_scatter(q: &Query) -> Option<Shape> {
    plan_scatter_with(q, false)
}

/// Like [`plan_scatter`] for a query whose `FROM` is a join that already runs shard by shard (see `joins.rs`).
pub fn plan_scatter_multi(q: &Query) -> Option<Shape> {
    plan_scatter_with(q, true)
}

fn plan_scatter_with(q: &Query, multi: bool) -> Option<Shape> {
    let sel = only_select(q)?;
    if !multi && !matches!(sel.from.as_slice(), [TableRef::Table { .. }]) {
        return None;
    }
    // expressions that need the session or a window cannot be split over shards
    let mut all: Vec<&Expr> = vec![];
    for it in &sel.items {
        match it {
            SelectItem::Expr { e, .. } => all.push(e),
            SelectItem::Star(_) => {}
            SelectItem::Assign { .. } => return None,
        }
    }
    if let Some(w) = &sel.where_ {
        all.push(w);
    }
    if let Some(h) = &sel.having {
        all.push(h);
    }
    for o in &q.order {
        all.push(&o.e);
    }
    if let Some(g) = &sel.group {
        for gi in g {
            match gi {
                GroupItem::Expr(e) => all.push(e),
                _ => return None,
            }
        }
    }
    if all.iter().any(|e| has_window_or_session(e) || has_subquery(e)) {
        return None;
    }
    let mut has_agg = false;
    for e in &all {
        walk_expr(e, &mut |x| {
            if let Expr::Func(fc) = x {
                if AGGREGATES.contains(&fc.name.as_str()) {
                    has_agg = true;
                }
            }
        });
    }
    if has_agg || sel.group.is_some() {
        plan_agg(q, sel).map(Shape::Agg)
    } else if sel.having.is_some() {
        None
    } else {
        plan_row(q, sel).map(Shape::Row)
    }
}

fn plan_row(q: &Query, sel: &Select) -> Option<RowPlan> {
    if let Some(t) = &sel.top {
        if t.percent || t.ties || !matches!(t.e, Expr::Lit(_)) {
            return None;
        }
    }
    let has_star = sel.items.iter().any(|i| matches!(i, SelectItem::Star(_)));
    let mut hidden: Vec<Expr> = vec![];
    let mut order = vec![];
    for o in &q.order {
        // an ordinal names an output column
        if let Expr::Lit(tsql_server::Value::Int(k)) = &o.e {
            order.push(RowOrder::Visible((*k as usize).checked_sub(1)?, o.desc));
            continue;
        }
        let mut found: Option<usize> = None;
        if !has_star {
            for (i, it) in sel.items.iter().enumerate() {
                let SelectItem::Expr { e: ie, alias } = it else { continue };
                let by_alias = matches!((&o.e, alias), (Expr::Col(p), Some(a)) if p.len() == 1 && p[0].eq_ignore_ascii_case(a));
                let by_name = alias.is_none() && last_part_eq(ie, &o.e);
                if by_alias || by_name || expr_key(ie) == expr_key(&o.e) {
                    found = Some(i);
                    break;
                }
            }
        }
        match found {
            Some(i) => order.push(RowOrder::Visible(i, o.desc)),
            None => {
                if sel.distinct {
                    return None;
                }
                order.push(RowOrder::Hidden(hidden.len(), o.desc));
                hidden.push(o.e.clone());
            }
        }
    }
    let mut items = sel.items.clone();
    for (j, h) in hidden.iter().enumerate() {
        items.push(SelectItem::Expr { e: h.clone(), alias: Some(format!("__dp_o{}", j)) });
    }
    let shard = Select { distinct: sel.distinct, top: sel.top.clone(), items, into: None, from: sel.from.clone(), where_: sel.where_.clone(), group: None, having: None };
    let mut shard_query = simple_query(shard);
    if sel.top.is_some() {
        shard_query.order = q.order.clone();
    }
    Some(RowPlan { shard_query, n_hidden: hidden.len(), distinct: sel.distinct, top: sel.top.clone(), order })
}

impl RowPlan {
    /// The final query over the temporary table `tmp` of shard results whose columns are `header`.
    pub fn final_query(&self, tmp: &str, header: &[String]) -> Query {
        let nvisible = header.len().saturating_sub(self.n_hidden);
        let items = (0..nvisible)
            .map(|i| SelectItem::Expr { e: col(&cname(i)), alias: if header[i].is_empty() { None } else { Some(header[i].clone()) } })
            .collect();
        let sel = Select { distinct: self.distinct, top: self.top.clone(), items, into: None, from: table_item(tmp), where_: None, group: None, having: None };
        let mut q = simple_query(sel);
        q.order = self
            .order
            .iter()
            .map(|o| match o {
                RowOrder::Visible(i, desc) => OrderItem { e: col(&cname(*i)), desc: *desc },
                RowOrder::Hidden(j, desc) => OrderItem { e: col(&cname(nvisible + *j)), desc: *desc },
            })
            .collect();
        q
    }
}

fn plan_agg(q: &Query, sel: &Select) -> Option<AggPlan> {
    let mut group_exprs: Vec<Expr> = vec![];
    if let Some(g) = &sel.group {
        for gi in g {
            let GroupItem::Expr(e) = gi else { return None };
            if !group_exprs.iter().any(|x| expr_key(x) == expr_key(e)) {
                group_exprs.push(e.clone());
            }
        }
    }
    let mut items: Vec<(Expr, Option<String>)> = vec![];
    for it in &sel.items {
        match it {
            SelectItem::Expr { e, alias } => items.push((e.clone(), alias.clone())),
            _ => return None,
        }
    }
    // the aggregate calls of the select list, HAVING and ORDER BY, each once
    let mut calls: Vec<FuncCall> = vec![];
    let mut unsupported = false;
    let mut collect = |e: &Expr| {
        walk_expr(e, &mut |x| {
            if let Expr::Func(fc) = x {
                if AGGREGATES.contains(&fc.name.as_str()) {
                    let ok = matches!(fc.name.as_str(), "COUNT" | "COUNT_BIG" | "SUM" | "AVG" | "MIN" | "MAX") && !fc.distinct && fc.over.is_none() && fc.within.is_none();
                    if !ok {
                        unsupported = true;
                    } else if !calls.iter().any(|c| expr_key(&Expr::Func(Box::new(c.clone()))) == expr_key(x)) {
                        calls.push((**fc).clone());
                    }
                }
            }
        });
    };
    for (e, _) in &items {
        collect(e);
    }
    if let Some(h) = &sel.having {
        collect(h);
    }
    for o in &q.order {
        collect(&o.e);
    }
    if unsupported {
        return None;
    }
    // the shard query: group keys, then the partial aggregates
    let mut shard_items: Vec<SelectItem> = vec![];
    for (i, g) in group_exprs.iter().enumerate() {
        shard_items.push(SelectItem::Expr { e: g.clone(), alias: Some(format!("g{}", i)) });
    }
    let mut aggs = vec![];
    for c in &calls {
        let col = shard_items.len();
        let kind = match c.name.as_str() {
            "COUNT" => AggKind::Count,
            "COUNT_BIG" => AggKind::CountBig,
            "SUM" => AggKind::Sum,
            "MIN" => AggKind::Min,
            "MAX" => AggKind::Max,
            _ => AggKind::Avg,
        };
        let key = expr_key(&Expr::Func(Box::new(c.clone())));
        if kind == AggKind::Avg {
            let mut s = c.clone();
            s.name = "SUM".into();
            let mut n = c.clone();
            n.name = "COUNT".into();
            shard_items.push(SelectItem::Expr { e: Expr::Func(Box::new(s)), alias: Some(format!("p{}", col)) });
            shard_items.push(SelectItem::Expr { e: Expr::Func(Box::new(n)), alias: Some(format!("p{}", col + 1)) });
            aggs.push(AggInfo { key, kind, col, count_col: Some(col + 1) });
        } else {
            shard_items.push(SelectItem::Expr { e: Expr::Func(Box::new(c.clone())), alias: Some(format!("p{}", col)) });
            aggs.push(AggInfo { key, kind, col, count_col: None });
        }
    }
    let shard = Select { distinct: false, top: None, items: shard_items, into: None, from: sel.from.clone(), where_: sel.where_.clone(), group: sel.group.clone(), having: None };
    let plan = AggPlan {
        shard_query: simple_query(shard),
        group_keys: group_exprs.iter().map(expr_key).collect(),
        aggs,
        items,
        having: sel.having.clone(),
        order: q.order.clone(),
        top: sel.top.clone(),
        distinct: sel.distinct,
    };
    // a dry run: can every expression be rewritten over the partial columns?
    let dummy: Vec<DataType> = vec![DataType::Float; plan.shard_query_columns()];
    plan.final_query("t", &dummy)?;
    Some(plan)
}

impl AggPlan {
    fn shard_query_columns(&self) -> usize {
        match &self.shard_query.body {
            QueryBody::Select(s) => s.items.len(),
            _ => 0,
        }
    }

    fn combine(&self, info: &AggInfo, types: &[DataType]) -> Expr {
        let c = |i: usize| col(&cname(i));
        match info.kind {
            AggKind::Count | AggKind::CountBig | AggKind::Sum => func("SUM", vec![c(info.col)]),
            AggKind::Min => func("MIN", vec![c(info.col)]),
            AggKind::Max => func("MAX", vec![c(info.col)]),
            AggKind::Avg => {
                let div = Expr::Binary(BinOp::Div, Box::new(func("SUM", vec![c(info.col)])), Box::new(func("SUM", vec![c(info.count_col.unwrap_or(info.col))])));
                match types.get(info.col) {
                    // the engine's AVG of a decimal has the scale of the SUM
                    Some(DataType::Decimal(_, s)) => Expr::Cast { e: Box::new(div), ty: TypeRef { parts: vec!["decimal".into()], args: vec![38, *s as i64], xml_schema: None }, try_: false, style: None },
                    _ => div,
                }
            }
        }
    }

    /// The final query over the temporary table `tmp` of partial rows with these column types. None when some
    /// expression cannot be written over the partial columns.
    pub fn final_query(&self, tmp: &str, types: &[DataType]) -> Option<Query> {
        let mut failed = false;
        let aliases: Vec<String> = self.items.iter().filter_map(|(e, a)| a.clone().or_else(|| if let Expr::Col(p) = e { p.last().cloned() } else { None })).map(|s| s.to_lowercase()).collect();
        let mut rewrite = |e: &Expr| -> Expr {
            let out = map_expr(e, &mut |x| {
                if is_aggregate_call(x).is_some() {
                    return match self.aggs.iter().find(|a| a.key == expr_key(x)) {
                        Some(info) => Some(self.combine(info, types)),
                        None => {
                            failed = true;
                            None
                        }
                    };
                }
                let k = expr_key(x);
                self.group_keys.iter().position(|g| *g == k).map(|i| col(&cname(i)))
            });
            out
        };
        let items: Vec<SelectItem> = self
            .items
            .iter()
            .map(|(e, alias)| {
                let alias = alias.clone().or_else(|| if let Expr::Col(p) = e { p.last().cloned() } else { None });
                SelectItem::Expr { e: rewrite(e), alias }
            })
            .collect();
        let having = self.having.as_ref().map(&mut rewrite);
        let order: Vec<OrderItem> = self.order.iter().map(|o| OrderItem { e: if matches!(o.e, Expr::Lit(_)) { o.e.clone() } else { rewrite(&o.e) }, desc: o.desc }).collect();
        // what is left must refer to the partial columns (or to an alias of the select list)
        let mut check = |e: &Expr| {
            walk_expr(e, &mut |x| match x {
                Expr::Col(p) => {
                    let n = p.last().map(|s| s.to_lowercase()).unwrap_or_default();
                    if !(p.len() == 1 && (n.starts_with(COL) || aliases.contains(&n))) {
                        failed = true;
                    }
                }
                Expr::Var(_) | Expr::SysVar(_) | Expr::Star(_) => failed = true,
                _ => {}
            });
        };
        for it in &items {
            if let SelectItem::Expr { e, .. } = it {
                check(e);
            }
        }
        if let Some(h) = &having {
            check(h);
        }
        for o in &order {
            check(&o.e);
        }
        if failed {
            return None;
        }
        let group = if self.group_keys.is_empty() { None } else { Some((0..self.group_keys.len()).map(|i| GroupItem::Expr(col(&cname(i)))).collect()) };
        let sel = Select { distinct: self.distinct, top: self.top.clone(), items, into: None, from: table_item(tmp), where_: None, group, having };
        let mut q = simple_query(sel);
        q.order = order;
        Some(q)
    }
}
