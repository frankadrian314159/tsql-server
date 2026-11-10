//! Query execution: SELECT pipeline, FROM/joins, grouping, aggregates, windows, CTEs.
use super::*;
use crate::convert::{arith, compare, key_of, sort_cmp, to_str};
use crate::ast::BinOp;
use std::cmp::Ordering;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct ColMeta {
    pub qual: Option<String>,
    pub name: String,
    pub ty: Option<DataType>,
    /// Set when the user may not read this column (column-level permissions): the text that completes the
    /// error message, e.g. ` of the object 't', database 'd', schema 's'.`
    pub denied: Option<Arc<String>>,
    /// A `HIDDEN` period column: left out of `SELECT *`.
    pub hidden: bool,
    /// Collation of a string column when it is not the built-in default (see `collation.rs`).
    pub coll: Option<Arc<crate::collation::Collation>>,
    /// Masking function of a masked column (see `mask.rs`); applied when the column is returned to a user without UNMASK.
    pub mask: Option<Arc<crate::mask::Mask>>,
}

#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub cols: Vec<ColMeta>,
}

pub enum Found {
    None,
    One(usize),
    Ambiguous,
}

impl Schema {
    pub fn new(cols: Vec<ColMeta>) -> Schema {
        Schema { cols }
    }
    pub fn find(&self, qual: Option<&str>, name: &str) -> Found {
        let mut found = None;
        for (i, c) in self.cols.iter().enumerate() {
            if c.name.eq_ignore_ascii_case(name) && (qual.is_none() || c.qual.as_deref().map(|q| q.eq_ignore_ascii_case(qual.unwrap())).unwrap_or(false)) {
                if found.is_some() {
                    return Found::Ambiguous;
                }
                found = Some(i);
            }
        }
        match found {
            Some(i) => Found::One(i),
            None => Found::None,
        }
    }
    pub fn find_parts(&self, parts: &[String]) -> Found {
        let n = parts.len();
        if n == 1 {
            self.find(None, &parts[0])
        } else {
            self.find(Some(&parts[n - 2]), &parts[n - 1])
        }
    }
    pub fn requalify(&mut self, alias: &str) {
        for c in &mut self.cols {
            c.qual = Some(alias.to_string());
        }
    }
    pub fn concat(&self, o: &Schema) -> Schema {
        let mut cols = self.cols.clone();
        cols.extend(o.cols.iter().cloned());
        Schema { cols }
    }
}

#[derive(Clone)]
pub struct Rowset {
    pub schema: Schema,
    pub rows: super::spill::RowsRef,
    pub suppress: bool,
    pub report_count: bool,
}

impl Rowset {
    pub fn new(schema: Schema, rows: Vec<Row>) -> Rowset {
        Rowset { schema, rows: rows.into(), suppress: false, report_count: false }
    }
    pub fn from_rows(cols: &[String], rows: Vec<Row>) -> Rowset {
        let schema = Schema::new(cols.iter().map(|c| ColMeta { qual: None, name: c.clone(), ty: None, denied: None, hidden: false, coll: None, mask: None }).collect());
        Rowset::new(schema, rows)
    }
    /// All rows of a private table (table variable, temp table) as its owner sees them.
    pub fn from_table(t: &Table, alias: &str) -> Rowset {
        let rows: Vec<Row> = t.data.iter(crate::storage::ReadView::dirty()).into_iter().map(|(_, r)| r).collect();
        Rowset::table_rows(t, alias, rows)
    }
    pub fn table_rows(t: &Table, alias: &str, rows: Vec<Row>) -> Rowset {
        let schema = Schema::new(t.columns.iter().map(|c| ColMeta { qual: Some(alias.to_string()), name: c.name.clone(), ty: Some(c.ty.clone()), denied: None, hidden: c.hidden, coll: c.collation.as_deref().and_then(crate::collation::get), mask: c.mask.as_deref().and_then(|m| crate::mask::Mask::parse(m).ok()).map(Arc::new) }).collect());
        Rowset { schema, rows: rows.into(), suppress: false, report_count: false }
    }
    pub fn empty() -> Rowset {
        Rowset::new(Schema::default(), vec![])
    }
}

pub struct GroupCtx {
    pub rows: Vec<Row>,
    pub excluded: Vec<bool>,
    pub base: Vec<String>, // Debug-rendered base grouping expressions (for GROUPING())
}

#[derive(Clone, Copy)]
pub struct Env<'a> {
    pub schema: &'a Schema,
    pub row: &'a [Value],
    pub parent: Option<&'a Env<'a>>,
    pub group: Option<&'a GroupCtx>,
    pub win: Option<&'a HashMap<usize, Value>>,
}

static EMPTY_SCHEMA: Schema = Schema { cols: Vec::new() };

impl<'a> Env<'a> {
    pub fn empty() -> Env<'static> {
        Env { schema: &EMPTY_SCHEMA, row: &[], parent: None, group: None, win: None }
    }
    pub fn new(schema: &'a Schema, row: &'a [Value], parent: Option<&'a Env<'a>>) -> Env<'a> {
        Env { schema, row, parent, group: None, win: None }
    }
    pub fn lookup(&self, parts: &[String]) -> Result<Value> {
        let mut cur = Some(self);
        while let Some(e) = cur {
            match e.schema.find_parts(parts) {
                Found::One(i) => {
                    if let Some(w) = &e.schema.cols[i].denied {
                        return Err(SqlError::with(230, 14, 1, format!("The SELECT permission was denied on the column '{}'{}", e.schema.cols[i].name, w)));
                    }
                    return Ok(e.row[i].clone());
                }
                Found::Ambiguous => return Err(SqlError::new(209, format!("Ambiguous column name '{}'.", parts[parts.len() - 1]))),
                Found::None => {}
            }
            cur = e.parent;
        }
        Err(SqlError::new(207, format!("Invalid column name '{}'.", parts[parts.len() - 1])))
    }
}

// ------------------------------------------------------- expression walking
pub fn is_agg_name(n: &str) -> bool {
    matches!(
        n,
        "COUNT" | "COUNT_BIG" | "SUM" | "AVG" | "MIN" | "MAX" | "STDEV" | "STDEVP" | "VAR" | "VARP" | "STRING_AGG" | "CHECKSUM_AGG" | "APPROX_COUNT_DISTINCT" | "JSON_ARRAYAGG" | "JSON_OBJECTAGG"
    )
}

pub fn children<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Neg(a) | Expr::BitNot(a) | Expr::Not(a) | Expr::Paren(a) | Expr::IsNull(a, _) | Expr::Collate(a, _) => out.push(a),
        Expr::Binary(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) | Expr::IsDistinct(a, b, _) | Expr::AtTimeZone(a, b) => {
            out.push(a);
            out.push(b);
        }
        Expr::InList(a, l, _) => {
            out.push(a);
            out.extend(l.iter());
        }
        Expr::InQuery(a, _, _) | Expr::CmpQuant(_, _, a, _) => out.push(a),
        Expr::Like(a, b, c, _) => {
            out.push(a);
            out.push(b);
            if let Some(c) = c {
                out.push(c);
            }
        }
        Expr::Between(a, b, c, _) => {
            out.push(a);
            out.push(b);
            out.push(c);
        }
        Expr::Case { operand, whens, els } => {
            if let Some(o) = operand {
                out.push(o);
            }
            for (c, r) in whens {
                out.push(c);
                out.push(r);
            }
            if let Some(e) = els {
                out.push(e);
            }
        }
        Expr::Cast { e, style, .. } => {
            out.push(e);
            if let Some(s) = style {
                out.push(s);
            }
        }
        Expr::Func(fc) => {
            out.extend(fc.args.iter());
            if let Some(o) = &fc.over {
                out.extend(o.partition.iter());
                out.extend(o.order.iter().map(|i| &i.e));
            }
            if let Some(w) = &fc.within {
                out.extend(w.iter().map(|i| &i.e));
            }
        }
        Expr::IdentityFn { seed, inc, .. } => {
            out.push(seed);
            out.push(inc);
        }
        _ => {}
    }
}

pub fn has_agg(e: &Expr) -> bool {
    if let Expr::Func(fc) = e {
        if fc.over.is_none() && fc.schema.is_none() && (is_agg_name(&fc.name) || fc.name == "GROUPING" || fc.name == "GROUPING_ID") {
            return true;
        }
    }
    let mut ch = vec![];
    children(e, &mut ch);
    ch.into_iter().any(has_agg)
}

pub fn collect_windows<'e>(e: &'e Expr, out: &mut Vec<&'e FuncCall>) {
    if let Expr::Func(fc) = e {
        if fc.over.is_some() {
            out.push(fc);
        }
    }
    let mut ch = vec![];
    children(e, &mut ch);
    for c in ch {
        collect_windows(c, out);
    }
}

fn query_refs_name(q: &Query, name: &str) -> bool {
    fn body(b: &QueryBody, name: &str) -> bool {
        match b {
            QueryBody::Select(s) => s.from.iter().any(|t| tref(t, name)),
            QueryBody::SetOp { left, right, .. } => body(left, name) || body(right, name),
            QueryBody::Paren(q) => query_refs_name(q, name),
        }
    }
    fn tref(t: &TableRef, name: &str) -> bool {
        match t {
            TableRef::Table { name: n, .. } => n.len() == 1 && n[0].eq_ignore_ascii_case(name),
            TableRef::Derived { query, .. } => query_refs_name(query, name),
            TableRef::Join { left, right, .. } => tref(left, name) || tref(right, name),
            TableRef::Pivot { src, .. } | TableRef::Unpivot { src, .. } => tref(src, name),
            _ => false,
        }
    }
    body(&q.body, name)
}

fn flatten_union<'q>(b: &'q QueryBody, out: &mut Vec<&'q QueryBody>) {
    if let QueryBody::SetOp { op: SetOp::Union, all: true, left, right } = b {
        flatten_union(left, out);
        flatten_union(right, out);
    } else {
        out.push(b);
    }
}

struct Unit {
    row: Row,
    group: Option<GroupCtx>,
}

pub fn cmp_keys(a: &[Value], b: &[Value], desc: &[bool]) -> Ordering {
    for i in 0..desc.len() {
        let mut o = sort_cmp(&a[i], &b[i]);
        if desc[i] {
            o = o.reverse();
        }
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// One column of a SELECT list after `*` is expanded.
pub struct OutCol<'q> {
    pub name: String,
    pub expr: Option<&'q Expr>,
    pub src_idx: Option<usize>,
    pub ty: Option<DataType>,
    /// Collation of the column when it is not the built-in default.
    pub coll: Option<Arc<crate::collation::Collation>>,
    /// The column is masked for this user: its values are replaced by the masked form on output.
    pub mask: Option<(Arc<crate::mask::Mask>, DataType)>,
}

impl OutCol<'_> {
    /// The value as returned to the user (masked if the column is masked for them).
    pub fn out_value(&self, v: Value) -> Value {
        match &self.mask {
            Some((m, ty)) => m.apply(ty, &v),
            None => v,
        }
    }
}

pub type Colls = Vec<Option<Arc<crate::collation::Collation>>>;

/// `vals` with every string replaced by its sort key in the matching collation, so the built-in hashing and ordering
/// treat the values as that collation does. Values without a collation (or with the default one) are left alone.
pub fn keyed(vals: &[Value], colls: &[Option<Arc<crate::collation::Collation>>]) -> Vec<Value> {
    vals.iter()
        .enumerate()
        .map(|(i, v)| match (v, colls.get(i)) {
            (Value::Str(s), Some(Some(c))) if !c.is_default() => Value::Str(c.key(s)),
            _ => v.clone(),
        })
        .collect()
}

/// Does any entry change how values compare?
pub fn any_coll(colls: &[Option<Arc<crate::collation::Collation>>]) -> bool {
    colls.iter().any(|c| c.as_ref().is_some_and(|c| !c.is_default()))
}

impl<'a> Engine<'a> {
    // ------------------------------------------------------------- queries
    pub fn exec_query(&mut self, q: &Query, outer: Option<&Env<'_>>) -> Result<Rowset> {
        crate::span!("exec_query");
        let pushed = !q.ctes.is_empty();
        if pushed {
            let mut m = HashMap::new();
            for c in &q.ctes {
                m.insert(c.name.to_lowercase(), CteEntry { query: c.query.clone(), cols: c.cols.clone(), working: None });
            }
            self.ses.cte_stack.push(m);
        }
        let prev_rec = self.ses.maxrec;
        if q.maxrecursion.is_some() {
            self.ses.maxrec = q.maxrecursion;
        }
        let new_opts = if q.options.is_empty() { self.ses.query_opts.clone() } else { q.options.clone() };
        let prev_opts = std::mem::replace(&mut self.ses.query_opts, new_opts);
        let r = self.exec_query_inner(q, outer);
        self.ses.query_opts = prev_opts;
        self.ses.maxrec = prev_rec;
        if pushed {
            self.ses.cte_stack.pop();
        }
        let mut rs = r?;
        if let Some(fc) = &q.for_clause {
            rs = self.apply_for_clause(rs, fc)?;
        }
        Ok(rs)
    }

    fn exec_query_inner(&mut self, q: &Query, outer: Option<&Env<'_>>) -> Result<Rowset> {
        match &q.body {
            QueryBody::Select(sel) => self.exec_select(sel, &q.order, &q.offset, &q.fetch, outer),
            _ => {
                let rs = self.exec_body(&q.body, outer)?;
                self.order_output(rs, &q.order, &q.offset, &q.fetch, outer)
            }
        }
    }

    pub fn exec_body(&mut self, b: &QueryBody, outer: Option<&Env<'_>>) -> Result<Rowset> {
        match b {
            QueryBody::Select(s) => self.exec_select(s, &[], &None, &None, outer),
            QueryBody::Paren(q) => self.exec_query(q, outer),
            QueryBody::SetOp { op, all, left, right } => {
                self.ses.stream_to_buf = matches!(**left, QueryBody::Select(_));
                let l = self.exec_body(left, outer)?;
                self.ses.stream_to_buf = matches!(**right, QueryBody::Select(_));
                let r = self.exec_body(right, outer)?;
                self.ses.stream_to_buf = false;
                if l.schema.cols.len() != r.schema.cols.len() {
                    return Err(SqlError::new(205, "All queries combined using a UNION, INTERSECT or EXCEPT operator must have an equal number of expressions in their target lists."));
                }
                let schema = Schema::new(l.schema.cols.iter().map(|c| ColMeta { qual: None, name: c.name.clone(), ty: c.ty.clone(), denied: None, hidden: false, coll: c.coll.clone(), mask: None }).collect());
                let sc: Colls = l.schema.cols.iter().map(|c| c.coll.clone()).collect();
                let rk_of = |r: &Row| if any_coll(&sc) { key_of(&keyed(r, &sc)) } else { key_of(r) };
                if self.setop_needs_spill(&l.rows, &r.rows) {
                    let rows = self.set_op_external(op, *all, &l.rows, &r.rows, &rk_of)?;
                    self.ses.plan.push(format!("{:?}{} by external sort -> {} rows", op, if *all { " ALL" } else { "" }, rows.count()));
                    return Ok(Rowset { schema, rows, suppress: false, report_count: false });
                }
                let lrows = (*l.rows).clone();
                let rrows = (*r.rows).clone();
                let rows = match (op, all) {
                    (SetOp::Union, true) => {
                        let mut v = lrows;
                        v.extend(rrows);
                        v
                    }
                    (SetOp::Union, false) => {
                        let mut seen = std::collections::HashSet::new();
                        let mut v = vec![];
                        for row in lrows.into_iter().chain(rrows) {
                            if seen.insert(rk_of(&row)) {
                                v.push(row);
                            }
                        }
                        v
                    }
                    (SetOp::Intersect, _) => {
                        let rk: std::collections::HashSet<String> = rrows.iter().map(|r| rk_of(r)).collect();
                        let mut seen = std::collections::HashSet::new();
                        lrows.into_iter().filter(|r| {
                            let k = rk_of(r);
                            rk.contains(&k) && seen.insert(k)
                        }).collect()
                    }
                    (SetOp::Except, _) => {
                        let rk: std::collections::HashSet<String> = rrows.iter().map(|r| rk_of(r)).collect();
                        let mut seen = std::collections::HashSet::new();
                        lrows.into_iter().filter(|r| {
                            let k = rk_of(r);
                            !rk.contains(&k) && seen.insert(k)
                        }).collect()
                    }
                };
                Ok(Rowset::new(schema, rows))
            }
        }
    }

    /// ORDER BY / OFFSET / FETCH applied to a finished rowset (set operations).
    fn order_output(&mut self, rs: Rowset, order: &[OrderItem], offset: &Option<Expr>, fetch: &Option<Expr>, outer: Option<&Env<'_>>) -> Result<Rowset> {
        let mut rows = (*rs.rows).clone();
        if !order.is_empty() {
            let oc: Colls = if crate::collation::in_use() {
                let empty: Row = vec![];
                let env = Env::new(&rs.schema, &empty, outer);
                order
                    .iter()
                    .map(|o| match &o.e {
                        Expr::Lit(Value::Int(n)) if *n >= 1 => rs.schema.cols.get(*n as usize - 1).and_then(|c| c.coll.clone()),
                        e => self.expr_coll(e, &env).map(|c| c.0),
                    })
                    .collect()
            } else {
                vec![]
            };
            let mut keys: Vec<Vec<Value>> = vec![];
            for row in &rows {
                let env = Env::new(&rs.schema, row, outer);
                let mut k = vec![];
                for o in order {
                    if let Expr::Lit(Value::Int(n)) = &o.e {
                        let idx = *n as usize;
                        if idx == 0 || idx > row.len() {
                            return Err(SqlError::new(108, format!("The ORDER BY position number {} is out of range of the number of items in the select list.", n)));
                        }
                        k.push(row[idx - 1].clone());
                    } else {
                        k.push(self.ev(&o.e, &env)?);
                    }
                }
                if any_coll(&oc) {
                    k = keyed(&k, &oc);
                }
                keys.push(k);
            }
            let desc: Vec<bool> = order.iter().map(|o| o.desc).collect();
            let mut idx: Vec<usize> = (0..rows.len()).collect();
            idx.sort_by(|&a, &b| cmp_keys(&keys[a], &keys[b], &desc));
            rows = idx.into_iter().map(|i| rows[i].clone()).collect();
        }
        rows = self.apply_offset_fetch(rows, offset, fetch)?;
        Ok(Rowset::new(rs.schema, rows))
    }

    fn apply_offset_fetch(&mut self, mut rows: Vec<Row>, offset: &Option<Expr>, fetch: &Option<Expr>) -> Result<Vec<Row>> {
        let env = Env::empty();
        if let Some(o) = offset {
            let n = self.ev(o, &env)?.as_i64().unwrap_or(0);
            if n < 0 {
                return Err(SqlError::new(10743, "The number of rows provided for a OFFSET clause must be greater then or equal to zero."));
            }
            rows = rows.into_iter().skip(n as usize).collect();
        }
        if let Some(f) = fetch {
            let n = self.ev(f, &env)?.as_i64().unwrap_or(0);
            if n < 1 {
                return Err(SqlError::new(10744, "The number of rows provided for a FETCH clause must be greater then zero."));
            }
            rows.truncate(n as usize);
        }
        Ok(rows)
    }

    /// Verify that every column reference binds (SQL Server binds names at compile time,
    /// so an unknown column is an error even when the table is empty).
    pub fn validate_columns(&self, e: &Expr, schema: &Schema, outer: Option<&Env<'_>>) -> Result<()> {
        if let Expr::Col(parts) = e {
            match schema.find_parts(parts) {
                Found::One(_) => return Ok(()),
                Found::Ambiguous => return Err(SqlError::new(209, format!("Ambiguous column name '{}'.", parts[parts.len() - 1]))),
                Found::None => {
                    let mut cur = outer;
                    while let Some(o) = cur {
                        match o.schema.find_parts(parts) {
                            Found::None => cur = o.parent,
                            Found::One(_) => return Ok(()),
                            Found::Ambiguous => return Err(SqlError::new(209, format!("Ambiguous column name '{}'.", parts[parts.len() - 1]))),
                        }
                    }
                    if parts.len() == 1 && parts[0].starts_with('$') {
                        return Ok(());
                    }
                    return Err(SqlError::new(207, format!("Invalid column name '{}'.", parts[parts.len() - 1])));
                }
            }
        }
        let mut ch = vec![];
        children(e, &mut ch);
        for c in ch {
            self.validate_columns(c, schema, outer)?;
        }
        Ok(())
    }

    fn eval_from_list(&mut self, from: &[TableRef], where_: &Option<Expr>, outer: Option<&Env<'_>>) -> Result<Rowset> {
        crate::span!("eval_from_list");
        self.eval_from_cluster(from, where_, outer)
    }

    // ------------------------------------------------------------- SELECT
    fn exec_select(&mut self, sel: &Select, order: &[OrderItem], offset: &Option<Expr>, fetch: &Option<Expr>, outer: Option<&Env<'_>>) -> Result<Rowset> {
        crate::span!("exec_select");
        // only the outermost SELECT of a statement may stream; subqueries started from it must not
        let may_stream = std::mem::take(&mut self.ses.stream_ok);
        let to_buf = std::mem::take(&mut self.ses.stream_to_buf);
        self.collect_hints(&sel.from)?;
        if let Some(rs) = self.try_stream_select(sel, order, offset, fetch, outer, may_stream, to_buf)? {
            return Ok(rs);
        }
        // FROM (conditions on single tables are applied while those tables are read)
        let mut ordered = false;
        let src = if sel.from.is_empty() {
            Rowset::new(Schema::default(), vec![vec![]])
        } else {
            let pushed = self.plan_pushdown_outer(&sel.from, &sel.where_, outer);
            self.ses.pushdown.push(pushed);
            let saved_hint = (self.ses.order_hint.take(), self.ses.ordered_used);
            self.ses.ordered_used = false;
            self.ses.order_hint = self.compute_order_hint(sel, order, offset, fetch);
            let r = self.eval_from_list(&sel.from, &sel.where_, outer);
            self.ses.preeval = None;
            self.ses.pushdown.pop();
            ordered = std::mem::take(&mut self.ses.ordered_used);
            self.ses.order_hint = saved_hint.0;
            self.ses.ordered_used = saved_hint.1;
            r?
        };
        let schema = src.schema.clone();
        // compile-time column binding
        for it in &sel.items {
            match it {
                SelectItem::Expr { e, .. } | SelectItem::Assign { e, .. } => self.validate_columns(e, &schema, outer)?,
                _ => {}
            }
        }
        if let Some(w) = &sel.where_ {
            self.validate_columns(w, &schema, outer)?;
        }
        if let Some(g) = &sel.group {
            for gi in g {
                match gi {
                    GroupItem::Expr(e) => self.validate_columns(e, &schema, outer)?,
                    GroupItem::Rollup(v) | GroupItem::Cube(v) => {
                        for e in v {
                            self.validate_columns(e, &schema, outer)?;
                        }
                    }
                    GroupItem::Sets(ss) => {
                        for v in ss {
                            for e in v {
                                self.validate_columns(e, &schema, outer)?;
                            }
                        }
                    }
                }
            }
        }
        if let Some(h) = &sel.having {
            self.validate_columns(h, &schema, outer)?;
        }
        // WHERE
        // (the rows are taken over, not copied, when nothing else holds the rowset)
        let mut rows: Vec<Row> = Vec::new();
        match &sel.where_ {
            Some(w) => {
                for r in src.rows.into_vec() {
                    let keep = {
                        let env = Env::new(&schema, &r, outer);
                        matches!(self.ev(w, &env)?, Value::Bit(true))
                    };
                    if keep {
                        rows.push(r);
                    }
                }
            }
            None => rows = src.rows.into_vec(),
        }

        let has_assign = sel.items.iter().any(|i| matches!(i, SelectItem::Assign { .. }));
        let item_exprs: Vec<&Expr> = sel.items.iter().filter_map(|i| if let SelectItem::Expr { e, .. } = i { Some(e) } else if let SelectItem::Assign { e, .. } = i { Some(e) } else { None }).collect();
        let aggregated = sel.group.is_some() || sel.having.is_some() || item_exprs.iter().any(|e| has_agg(e)) || order.iter().any(|o| has_agg(&o.e));

        // grouping
        let mut units: Vec<Unit> = Vec::new();
        if aggregated {
            units = self.group_units(sel, &schema, rows, outer)?;
        } else {
            for r in rows {
                units.push(Unit { row: r, group: None });
            }
        }
        // HAVING
        if let Some(h) = &sel.having {
            let mut kept = vec![];
            for u in units {
                let keep = {
                    let env = Env { schema: &schema, row: &u.row, parent: outer, group: u.group.as_ref(), win: None };
                    matches!(self.ev(h, &env)?, Value::Bit(true))
                };
                if keep {
                    kept.push(u);
                }
            }
            units = kept;
        }
        // window functions
        let mut wins_all: Vec<&FuncCall> = vec![];
        for e in &item_exprs {
            collect_windows(e, &mut wins_all);
        }
        for o in order {
            collect_windows(&o.e, &mut wins_all);
        }
        let wins: Vec<HashMap<usize, Value>> = if wins_all.is_empty() { vec![] } else { self.compute_windows(&wins_all, &schema, &units, outer)? };

        // projection setup
        let cols = self.out_columns(sel, &schema)?;

        // assignment select
        if has_assign {
            let mut last: Option<Row> = None;
            let n = units.len();
            for (ui, u) in units.iter().enumerate() {
                let env = Env { schema: &schema, row: &u.row, parent: outer, group: u.group.as_ref(), win: wins.get(ui) };
                for it in &sel.items {
                    if let SelectItem::Assign { var, op, e } = it {
                        let mut v = self.ev(e, &env)?;
                        if let Some(op) = op {
                            let cur = self.get_var(var)?;
                            v = arith(*op, &cur, &v)?;
                        }
                        self.set_var(var, v)?;
                    }
                }
                last = Some(u.row.clone());
            }
            let _ = last;
            self.ses.rowcount = n as i64;
            let mut rs = Rowset::empty();
            rs.suppress = true;
            rs.report_count = true;
            return Ok(rs);
        }

        // project + order keys
        let mut out: Vec<(Row, Vec<Value>)> = vec![];
        let desc: Vec<bool> = order.iter().map(|o| o.desc).collect();
        // collations of the output columns and of the ORDER BY expressions (empty when none is non-default)
        let out_colls: Colls = cols.iter().map(|c| c.coll.clone()).collect();
        let order_colls: Colls = self.order_colls(order, &cols, &schema, outer);
        let mut ident_counter: Vec<Option<(i64, i64, i64)>> = vec![None; cols.len()];
        for (ci, c) in cols.iter().enumerate() {
            if let Some(Expr::IdentityFn { seed, inc, .. }) = c.expr {
                let env = Env::empty();
                let s = self.ev(seed, &env)?.as_i64().unwrap_or(1);
                let i = self.ev(inc, &env)?.as_i64().unwrap_or(1);
                ident_counter[ci] = Some((s, i, 0));
            }
        }
        for (ui, u) in units.iter().enumerate() {
            let env = Env { schema: &schema, row: &u.row, parent: outer, group: u.group.as_ref(), win: wins.get(ui) };
            let mut row = Vec::with_capacity(cols.len());
            for (ci, c) in cols.iter().enumerate() {
                if let Some(i) = c.src_idx {
                    row.push(c.out_value(u.row[i].clone()));
                } else if let Some((s, inc, n)) = ident_counter[ci].as_mut() {
                    row.push(Value::BigInt(*s + *n * *inc));
                    *n += 1;
                } else {
                    let v = self.ev(c.expr.unwrap(), &env)?;
                    row.push(c.out_value(v));
                }
            }
            let mut keys = Vec::with_capacity(order.len());
            for o in order {
                let mut done = false;
                match &o.e {
                    Expr::Lit(Value::Int(n)) => {
                        let idx = *n as usize;
                        if idx == 0 || idx > row.len() {
                            return Err(SqlError::new(108, format!("The ORDER BY position number {} is out of range of the number of items in the select list.", n)));
                        }
                        keys.push(row[idx - 1].clone());
                        done = true;
                    }
                    Expr::Col(p) if p.len() == 1 => {
                        if let Some(ci) = cols.iter().position(|c| !c.name.is_empty() && c.name.eq_ignore_ascii_case(&p[0])) {
                            keys.push(row[ci].clone());
                            done = true;
                        }
                    }
                    _ => {}
                }
                if !done {
                    keys.push(self.ev(&o.e, &env)?);
                }
            }
            if any_coll(&order_colls) {
                keys = keyed(&keys, &order_colls);
            }
            out.push((row, keys));
        }
        // DISTINCT
        if sel.distinct {
            let mut seen = std::collections::HashSet::new();
            let dc = any_coll(&out_colls);
            out.retain(|(r, _)| seen.insert(if dc { key_of(&keyed(r, &out_colls)) } else { key_of(r) }));
        }
        // ORDER BY (skipped when the rows were already read in index order)
        if !order.is_empty() && !ordered {
            out.sort_by(|a, b| cmp_keys(&a.1, &b.1, &desc));
        }
        self.finish_rows(sel, order, offset, fetch, &cols, out, &ident_counter)
    }

    /// Expand the SELECT list into output columns (`*` becomes its columns).
    /// The collation of each ORDER BY expression (an output position, an output column name, or an expression over the
    /// source columns); empty when none of them has a non-default collation.
    pub fn order_colls(&self, order: &[OrderItem], cols: &[OutCol<'_>], schema: &Schema, outer: Option<&Env<'_>>) -> Colls {
        if order.is_empty() || !crate::collation::in_use() {
            return vec![];
        }
        let empty: Row = vec![];
        let env = Env::new(schema, &empty, outer);
        let v: Colls = order
            .iter()
            .map(|o| match &o.e {
                Expr::Lit(Value::Int(n)) if *n >= 1 => cols.get(*n as usize - 1).and_then(|c| c.coll.clone()),
                Expr::Col(p) if p.len() == 1 && cols.iter().any(|c| !c.name.is_empty() && c.name.eq_ignore_ascii_case(&p[0])) => cols.iter().find(|c| !c.name.is_empty() && c.name.eq_ignore_ascii_case(&p[0])).and_then(|c| c.coll.clone()),
                e => self.expr_coll(e, &env).map(|c| c.0),
            })
            .collect();
        if any_coll(&v) {
            v
        } else {
            vec![]
        }
    }

    pub fn out_columns<'q>(&mut self, sel: &'q Select, schema: &Schema) -> Result<Vec<OutCol<'q>>> {
        crate::span!("out_columns");
        let mut cols: Vec<OutCol> = vec![];
        let unmask = self.can_unmask();
        for it in &sel.items {
            match it {
                SelectItem::Star(q) => {
                    if sel.from.is_empty() {
                        return Err(SqlError::new(4104, "No tables specified for SELECT *."));
                    }
                    let mut any = false;
                    for (i, c) in schema.cols.iter().enumerate() {
                        let ok = !c.hidden && match q {
                            None => true,
                            Some(qp) => c.qual.as_deref().map(|x| x.eq_ignore_ascii_case(&qp[qp.len() - 1])).unwrap_or(false),
                        };
                        if ok {
                            if let Some(w) = &c.denied {
                                return Err(SqlError::with(230, 14, 1, format!("The SELECT permission was denied on the column '{}'{}", c.name, w)));
                            }
                            any = true;
                            cols.push(OutCol { name: c.name.clone(), expr: None, src_idx: Some(i), ty: c.ty.clone(), coll: c.coll.clone(), mask: if unmask { None } else { c.mask.clone().zip(c.ty.clone()) } });
                        }
                    }
                    if !any && q.is_some() {
                        return Err(SqlError::new(208, format!("Invalid object name '{}'.", q.as_ref().unwrap().join("."))));
                    }
                }
                SelectItem::Expr { e, alias } => {
                    let mut name = alias.clone().unwrap_or_default();
                    let mut ty = None;
                    let mut mask = None;
                    let coll = if crate::collation::in_use() {
                        let empty: Row = vec![];
                        self.expr_coll(e, &Env::new(schema, &empty, None)).map(|c| c.0).filter(|c| !c.is_default())
                    } else {
                        None
                    };
                    match e {
                        Expr::Col(parts) => {
                            if let Found::One(i) = schema.find_parts(parts) {
                                if alias.is_none() {
                                    name = schema.cols[i].name.clone();
                                }
                                ty = schema.cols[i].ty.clone();
                                if !unmask {
                                    mask = schema.cols[i].mask.clone().zip(ty.clone());
                                }
                            } else if alias.is_none() {
                                name = parts[parts.len() - 1].clone();
                            }
                        }
                        Expr::Cast { ty: tr, try_: _, .. } => {
                            ty = self.resolve_type_cast(tr).ok();
                        }
                        _ => {}
                    }
                    cols.push(OutCol { name, expr: Some(e), src_idx: None, ty, coll, mask });
                }
                SelectItem::Assign { .. } => {}
            }
        }
        Ok(cols)
    }

    /// OFFSET / FETCH, TOP and SELECT INTO over rows that are already filtered, projected, distinct and ordered.
    pub fn finish_rows(&mut self, sel: &Select, order: &[OrderItem], offset: &Option<Expr>, fetch: &Option<Expr>, cols: &[OutCol<'_>], mut out: Vec<(Row, Vec<Value>)>, ident_counter: &[Option<(i64, i64, i64)>]) -> Result<Rowset> {
        crate::span!("finish_rows");
        let desc: Vec<bool> = order.iter().map(|o| o.desc).collect();
        // OFFSET / FETCH
        if offset.is_some() || fetch.is_some() {
            let rows: Vec<Row> = out.iter().map(|x| x.0.clone()).collect();
            let keep = self.apply_offset_fetch(rows, offset, fetch)?.len();
            let skip = match offset {
                Some(o) => self.ev(o, &Env::empty())?.as_i64().unwrap_or(0) as usize,
                None => 0,
            };
            out = out.into_iter().skip(skip).take(keep).collect();
        }
        // TOP
        if let Some(top) = &sel.top {
            let env = Env::empty();
            let v = self.ev(&top.e, &env)?;
            let n = if top.percent {
                let p = convert::num_to_f64(convert::as_num(&v).ok_or_else(|| SqlError::new(1031, "Percent values must be between 0 and 100."))?);
                if !(0.0..=100.0).contains(&p) {
                    return Err(SqlError::new(1031, "Percent values must be between 0 and 100."));
                }
                (out.len() as f64 * p / 100.0).ceil() as usize
            } else {
                let n = v.as_i64().unwrap_or(0);
                if n < 0 {
                    return Err(SqlError::new(1019, "Invalid usage of the option NEXT in the FETCH statement."));
                }
                n as usize
            };
            if top.ties && !order.is_empty() && n > 0 && n < out.len() {
                let last = out[n - 1].1.clone();
                let mut end = n;
                while end < out.len() && cmp_keys(&out[end].1, &last, &desc) == Ordering::Equal {
                    end += 1;
                }
                out.truncate(end);
            } else {
                out.truncate(n);
            }
        }
        let schema_out = Schema::new(cols.iter().map(|c| ColMeta { qual: None, name: c.name.clone(), ty: c.ty.clone(), denied: None, hidden: false, coll: c.coll.clone(), mask: None }).collect());
        let rows: Vec<Row> = out.into_iter().map(|x| x.0).collect();
        // SELECT INTO
        if let Some(into) = &sel.into {
            let n = rows.len();
            let idents: Vec<(usize, i64, i64)> = ident_counter.iter().enumerate().filter_map(|(i, x)| x.map(|(s, inc, _)| (i, s, inc))).collect();
            self.select_into(into, &schema_out, rows, &idents)?;
            self.ses.rowcount = n as i64;
            let mut rs = Rowset::empty();
            rs.suppress = true;
            rs.report_count = true;
            return Ok(rs);
        }
        Ok(Rowset::new(schema_out, rows))
    }

    // --------------------------------------------------------- grouping
    fn group_units(&mut self, sel: &Select, schema: &Schema, rows: Vec<Row>, outer: Option<&Env<'_>>) -> Result<Vec<Unit>> {
        crate::span!("group_units");
        // base expressions and grouping sets
        let mut base: Vec<&Expr> = vec![];
        let mut sets: Vec<Vec<usize>> = vec![vec![]];
        let has_group = sel.group.is_some();
        let idx_of = |base: &mut Vec<&'_ Expr>, e: &'_ Expr| -> usize {
            let _ = base;
            let _ = e;
            0
        };
        let _ = idx_of;
        if let Some(items) = &sel.group {
            let mut add = |base: &mut Vec<&Expr>, e: &Expr| -> usize {
                let s = format!("{:?}", e);
                if let Some(p) = base.iter().position(|b| format!("{:?}", b) == s) {
                    p
                } else {
                    // lifetime: items outlive this function
                    // SAFETY-free approach: store via index into a per-call vec of refs
                    base.len()
                }
            };
            let _ = &mut add;
            sets = vec![vec![]];
            // build cross product of per-item set lists
            let mut per_item: Vec<Vec<Vec<usize>>> = vec![];
            for gi in items {
                match gi {
                    GroupItem::Expr(e) => {
                        let s = format!("{:?}", e);
                        let p = match base.iter().position(|b| format!("{:?}", b) == s) {
                            Some(p) => p,
                            None => {
                                base.push(e);
                                base.len() - 1
                            }
                        };
                        per_item.push(vec![vec![p]]);
                    }
                    GroupItem::Rollup(es) | GroupItem::Cube(es) => {
                        let mut idxs = vec![];
                        for e in es {
                            let s = format!("{:?}", e);
                            let p = match base.iter().position(|b| format!("{:?}", b) == s) {
                                Some(p) => p,
                                None => {
                                    base.push(e);
                                    base.len() - 1
                                }
                            };
                            idxs.push(p);
                        }
                        let mut list = vec![];
                        if matches!(gi, GroupItem::Rollup(_)) {
                            for k in (0..=idxs.len()).rev() {
                                list.push(idxs[..k].to_vec());
                            }
                        } else {
                            for mask in (0..(1u32 << idxs.len())).rev() {
                                list.push(idxs.iter().enumerate().filter(|(i, _)| mask & (1 << (idxs.len() - 1 - i)) != 0).map(|(_, x)| *x).collect());
                            }
                        }
                        per_item.push(list);
                    }
                    GroupItem::Sets(ss) => {
                        let mut list = vec![];
                        for s in ss {
                            let mut idxs = vec![];
                            for e in s {
                                let sd = format!("{:?}", e);
                                let p = match base.iter().position(|b| format!("{:?}", b) == sd) {
                                    Some(p) => p,
                                    None => {
                                        base.push(e);
                                        base.len() - 1
                                    }
                                };
                                idxs.push(p);
                            }
                            list.push(idxs);
                        }
                        per_item.push(list);
                    }
                }
            }
            for list in per_item {
                let mut next = vec![];
                for prefix in &sets {
                    for s in &list {
                        let mut v = prefix.clone();
                        for x in s {
                            if !v.contains(x) {
                                v.push(*x);
                            }
                        }
                        next.push(v);
                    }
                }
                sets = next;
            }
        }
        let base_str: Vec<String> = base.iter().map(|b| format!("{:?}", b)).collect();
        // collations of the grouping expressions (strings are grouped by their sort keys)
        let base_colls: Colls = if crate::collation::in_use() {
            let empty: Row = vec![];
            let env = Env::new(schema, &empty, outer);
            let v: Colls = base.iter().map(|b| self.expr_coll(b, &env).map(|c| c.0)).collect();
            if any_coll(&v) {
                v
            } else {
                vec![]
            }
        } else {
            vec![]
        };
        // evaluate base keys per row once
        let mut keyvals: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        let mut group_keys: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        for r in &rows {
            let env = Env::new(schema, r, outer);
            let mut kv = vec![];
            for b in &base {
                kv.push(self.ev(b, &env)?);
            }
            if !base_colls.is_empty() {
                group_keys.push(keyed(&kv, &base_colls));
            }
            keyvals.push(kv);
        }
        let key_src: &Vec<Vec<Value>> = if base_colls.is_empty() { &keyvals } else { &group_keys };
        let mut units = vec![];
        for set in &sets {
            let mut order: Vec<Vec<usize>> = vec![];
            let mut map: HashMap<String, usize> = HashMap::new();
            for (ri, kv) in key_src.iter().enumerate() {
                let key: Vec<Value> = set.iter().map(|&i| kv[i].clone()).collect();
                let k = key_of(&key);
                match map.get(&k) {
                    Some(&g) => order[g].push(ri),
                    None => {
                        map.insert(k, order.len());
                        order.push(vec![ri]);
                    }
                }
            }
            if order.is_empty() && (!has_group || set.is_empty()) {
                order.push(vec![]);
            }
            let excluded: Vec<bool> = (0..base.len()).map(|i| !set.contains(&i)).collect();
            for g in order {
                let grows: Vec<Row> = g.iter().map(|&i| rows[i].clone()).collect();
                let mut rep: Row = if let Some(&f) = g.first() { rows[f].clone() } else { vec![Value::Null; schema.cols.len()] };
                for (bi, b) in base.iter().enumerate() {
                    if excluded[bi] {
                        if let Expr::Col(p) = b {
                            if let Found::One(ci) = schema.find_parts(p) {
                                rep[ci] = Value::Null;
                            }
                        }
                    }
                }
                units.push(Unit { row: rep, group: Some(GroupCtx { rows: grows, excluded: excluded.clone(), base: base_str.clone() }) });
            }
        }
        Ok(units)
    }

    // ---------------------------------------------------------- aggregates
    pub fn eval_aggregate(&mut self, fc: &FuncCall, env: &Env<'_>) -> Result<Value> {
        let g = match env.group {
            Some(g) => g,
            None => {
                return Err(SqlError::new(147, format!("An aggregate may not appear in the WHERE clause unless it is in a subquery contained in a HAVING clause or a select list, and the column being aggregated is an outer reference. ({})", fc.name)));
            }
        };
        let envs: Vec<Env> = g.rows.iter().map(|r| Env { schema: env.schema, row: r, parent: env.parent, group: None, win: None }).collect();
        self.agg_over(fc, &envs)
    }

    pub fn agg_over(&mut self, fc: &FuncCall, envs: &[Env<'_>]) -> Result<Value> {
        let name = fc.name.as_str();
        if fc.star {
            if name == "COUNT" {
                return Ok(Value::Int(envs.len() as i32));
            }
            if name == "COUNT_BIG" {
                return Ok(Value::BigInt(envs.len() as i64));
            }
            return Err(SqlError::new(102, format!("Incorrect syntax near '*'.")));
        }
        if fc.args.is_empty() {
            return Err(SqlError::new(174, format!("The {} function requires 1 argument(s).", name.to_lowercase())));
        }
        // collect argument values (with ordering for STRING_AGG)
        let mut vals: Vec<(Value, Vec<Value>)> = Vec::with_capacity(envs.len());
        for e in envs {
            let v = self.ev(&fc.args[0], e)?;
            let mut keys = vec![];
            if let Some(w) = &fc.within {
                for o in w {
                    keys.push(self.ev(&o.e, e)?);
                }
            }
            vals.push((v, keys));
        }
        if let Some(w) = &fc.within {
            let desc: Vec<bool> = w.iter().map(|o| o.desc).collect();
            vals.sort_by(|a, b| cmp_keys(&a.1, &b.1, &desc));
        }
        let mut vs: Vec<Value> = vals.into_iter().map(|x| x.0).filter(|v| !v.is_null()).collect();
        let acoll: Option<Arc<crate::collation::Collation>> = if crate::collation::in_use() { envs.first().and_then(|e0| self.expr_coll(&fc.args[0], e0)).map(|c| c.0).filter(|c| !c.is_default()) } else { None };
        if fc.distinct {
            let mut seen = std::collections::HashSet::new();
            vs.retain(|v| match (&acoll, v) {
                (Some(c), Value::Str(s)) => seen.insert(key_of(&[Value::Str(c.key(s))])),
                _ => seen.insert(key_of(std::slice::from_ref(v))),
            });
        }
        match name {
            "COUNT" => Ok(Value::Int(vs.len() as i32)),
            "COUNT_BIG" | "APPROX_COUNT_DISTINCT" => {
                if name == "APPROX_COUNT_DISTINCT" {
                    let mut seen = std::collections::HashSet::new();
                    vs.retain(|v| seen.insert(key_of(std::slice::from_ref(v))));
                }
                Ok(Value::BigInt(vs.len() as i64))
            }
            "SUM" | "AVG" => {
                if vs.is_empty() {
                    return Ok(Value::Null);
                }
                let mut acc = match &vs[0] {
                    Value::Str(_) => {
                        return Err(SqlError::new(8117, "Operand data type varchar is invalid for sum operator."));
                    }
                    Value::Bit(b) => Value::Int(*b as i32),
                    v => v.clone(),
                };
                for v in &vs[1..] {
                    acc = arith(BinOp::Add, &acc, v)?;
                }
                if name == "AVG" {
                    let cnt = match acc {
                        Value::BigInt(_) => Value::BigInt(vs.len() as i64),
                        _ => Value::Int(vs.len() as i32),
                    };
                    let scale = if let Value::Dec(d) = &acc { Some(d.s) } else { None };
                    acc = arith(BinOp::Div, &acc, &cnt)?;
                    if let (Some(sc), Value::Dec(d)) = (scale, &acc) {
                        acc = Value::Dec(d.rescale(sc).unwrap_or(*d));
                    }
                }
                Ok(acc)
            }
            "MIN" | "MAX" => {
                let mut best: Option<Value> = None;
                for v in vs {
                    best = Some(match best {
                        None => v,
                        Some(b) => {
                            let o = match (&acoll, &v, &b) {
                                (Some(c), Value::Str(x), Value::Str(y)) => c.compare(x, y),
                                _ => compare(&v, &b)?.unwrap_or(Ordering::Equal),
                            };
                            if (name == "MIN" && o == Ordering::Less) || (name == "MAX" && o == Ordering::Greater) {
                                v
                            } else {
                                b
                            }
                        }
                    });
                }
                Ok(best.unwrap_or(Value::Null))
            }
            "STDEV" | "STDEVP" | "VAR" | "VARP" => {
                let fs: Vec<f64> = vs.iter().filter_map(|v| convert::cast(v, &DataType::Float, None).ok()).filter_map(|v| if let Value::Float(f) = v { Some(f) } else { None }).collect();
                let n = fs.len() as f64;
                let pop = name.ends_with('P');
                if fs.is_empty() || (!pop && fs.len() < 2) {
                    return Ok(Value::Null);
                }
                let mean = fs.iter().sum::<f64>() / n;
                let ss: f64 = fs.iter().map(|x| (x - mean) * (x - mean)).sum();
                let var = ss / if pop { n } else { n - 1.0 };
                Ok(Value::Float(if name.starts_with("STDEV") { var.sqrt() } else { var }))
            }
            "STRING_AGG" => {
                if vs.is_empty() {
                    return Ok(Value::Null);
                }
                let sep = match fc.args.get(1) {
                    Some(e) => to_str(&self.ev(e, &envs.first().copied().unwrap_or(Env::empty().into_static_like()))?),
                    None => String::new(),
                };
                Ok(Value::Str(vs.iter().map(to_str).collect::<Vec<_>>().join(&sep)))
            }
            "CHECKSUM_AGG" => {
                let mut x: i32 = 0;
                for v in &vs {
                    x ^= funcs::checksum_value(v);
                }
                Ok(Value::Int(x))
            }
            _ => Err(SqlError::new(195, format!("'{}' is not a recognized aggregate function.", name))),
        }
    }

    // ------------------------------------------------------------- windows
    /// Window values for the rows of one partition set (used by the streaming executor, one partition at a time).
    pub(crate) fn window_rows(&mut self, wins: &[&FuncCall], schema: &Schema, rows: Vec<Row>, outer: Option<&Env<'_>>) -> Result<Vec<HashMap<usize, Value>>> {
        let units: Vec<Unit> = rows.into_iter().map(|row| Unit { row, group: None }).collect();
        self.compute_windows(wins, schema, &units, outer)
    }

    fn compute_windows(&mut self, wins: &[&FuncCall], schema: &Schema, units: &[Unit], outer: Option<&Env<'_>>) -> Result<Vec<HashMap<usize, Value>>> {
        let mut results: Vec<HashMap<usize, Value>> = (0..units.len()).map(|_| HashMap::new()).collect();
        for fc in wins {
            let key = *fc as *const FuncCall as usize;
            if results.first().map(|m| m.contains_key(&key)).unwrap_or(false) {
                continue;
            }
            let over = fc.over.as_ref().unwrap();
            // partition
            let mut parts: Vec<Vec<usize>> = vec![];
            let mut pmap: HashMap<String, usize> = HashMap::new();
            for (i, u) in units.iter().enumerate() {
                let env = todo_env(schema, u, outer);
                let mut pv = vec![];
                for p in &over.partition {
                    pv.push(self.ev(p, &env)?);
                }
                let k = key_of(&pv);
                match pmap.get(&k) {
                    Some(&g) => parts[g].push(i),
                    None => {
                        pmap.insert(k, parts.len());
                        parts.push(vec![i]);
                    }
                }
            }
            let desc: Vec<bool> = over.order.iter().map(|o| o.desc).collect();
            for part in parts {
                // order keys
                let mut okeys: Vec<Vec<Value>> = vec![];
                for &i in &part {
                    let env = todo_env(schema, &units[i], outer);
                    let mut k = vec![];
                    for o in &over.order {
                        k.push(self.ev(&o.e, &env)?);
                    }
                    okeys.push(k);
                }
                let mut idx: Vec<usize> = (0..part.len()).collect();
                if !over.order.is_empty() {
                    idx.sort_by(|&a, &b| cmp_keys(&okeys[a], &okeys[b], &desc));
                }
                let sorted: Vec<usize> = idx.iter().map(|&i| part[i]).collect();
                let skeys: Vec<&Vec<Value>> = idx.iter().map(|&i| &okeys[i]).collect();
                let n = sorted.len();
                let peers_eq = |a: usize, b: usize| -> bool { over.order.is_empty() || cmp_keys(skeys[a], skeys[b], &desc) == Ordering::Equal };
                let vals = self.window_values(fc, over, &sorted, &skeys, units, schema, outer, &peers_eq, n)?;
                for (p, v) in vals.into_iter().enumerate() {
                    results[sorted[p]].insert(key, v);
                }
            }
        }
        Ok(results)
    }

    #[allow(clippy::too_many_arguments)]
    fn window_values(
        &mut self,
        fc: &FuncCall,
        over: &Over,
        sorted: &[usize],
        skeys: &[&Vec<Value>],
        units: &[Unit],
        schema: &Schema,
        outer: Option<&Env<'_>>,
        peers_eq: &dyn Fn(usize, usize) -> bool,
        n: usize,
    ) -> Result<Vec<Value>> {
        let name = fc.name.as_str();
        let mut out = Vec::with_capacity(n);
        match name {
            "ROW_NUMBER" => {
                for p in 0..n {
                    out.push(Value::BigInt(p as i64 + 1));
                }
            }
            "RANK" | "DENSE_RANK" => {
                let mut rank = 1i64;
                let mut dense = 1i64;
                for p in 0..n {
                    if p > 0 && !peers_eq(p, p - 1) {
                        rank = p as i64 + 1;
                        dense += 1;
                    }
                    out.push(Value::BigInt(if name == "RANK" { rank } else { dense }));
                }
            }
            "PERCENT_RANK" => {
                let mut rank = 1i64;
                for p in 0..n {
                    if p > 0 && !peers_eq(p, p - 1) {
                        rank = p as i64 + 1;
                    }
                    out.push(Value::Float(if n <= 1 { 0.0 } else { (rank - 1) as f64 / (n - 1) as f64 }));
                }
            }
            "CUME_DIST" => {
                for p in 0..n {
                    let mut e = p;
                    while e + 1 < n && peers_eq(e + 1, p) {
                        e += 1;
                    }
                    out.push(Value::Float((e + 1) as f64 / n as f64));
                }
            }
            "NTILE" => {
                let env = Env::empty();
                let tiles = self.ev(&fc.args[0], &env)?.as_i64().unwrap_or(1).max(1) as usize;
                let base = n / tiles;
                let extra = n % tiles;
                let mut p = 0;
                for t in 0..tiles {
                    let size = base + if t < extra { 1 } else { 0 };
                    for _ in 0..size {
                        if p < n {
                            out.push(Value::BigInt(t as i64 + 1));
                            p += 1;
                        }
                    }
                }
            }
            "LAG" | "LEAD" => {
                let off = if fc.args.len() > 1 { self.ev(&fc.args[1], &Env::empty())?.as_i64().unwrap_or(1) } else { 1 };
                for p in 0..n {
                    let t = if name == "LAG" { p as i64 - off } else { p as i64 + off };
                    if t >= 0 && (t as usize) < n {
                        let env = todo_env(schema, &units[sorted[t as usize]], outer);
                        out.push(self.ev(&fc.args[0], &env)?);
                    } else if fc.args.len() > 2 {
                        let env = todo_env(schema, &units[sorted[p]], outer);
                        out.push(self.ev(&fc.args[2], &env)?);
                    } else {
                        out.push(Value::Null);
                    }
                }
            }
            "PERCENTILE_CONT" | "PERCENTILE_DISC" => {
                let pct = convert::num_to_f64(convert::as_num(&self.ev(&fc.args[0], &Env::empty())?).unwrap_or(convert::Num::F(0.5)));
                let w = fc.within.as_ref().ok_or_else(|| SqlError::new(102, "Incorrect syntax near 'OVER'."))?;
                let mut vals: Vec<f64> = vec![];
                for &i in sorted {
                    let env = todo_env(schema, &units[i], outer);
                    let v = self.ev(&w[0].e, &env)?;
                    if let Some(nm) = convert::as_num(&v) {
                        vals.push(convert::num_to_f64(nm));
                    }
                }
                vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
                if w[0].desc {
                    vals.reverse();
                }
                let r = if vals.is_empty() {
                    Value::Null
                } else if name == "PERCENTILE_CONT" {
                    let pos = pct * (vals.len() - 1) as f64;
                    let lo = pos.floor() as usize;
                    let hi = pos.ceil() as usize;
                    Value::Float(vals[lo] + (vals[hi] - vals[lo]) * (pos - lo as f64))
                } else {
                    let k = ((pct * vals.len() as f64).ceil() as usize).max(1) - 1;
                    Value::Float(vals[k.min(vals.len() - 1)])
                };
                for _ in 0..n {
                    out.push(r.clone());
                }
            }
            _ => {
                // frame-based: aggregates, FIRST_VALUE, LAST_VALUE, NTH_VALUE
                let env0 = Env::empty();
                for p in 0..n {
                    let (s, e) = self.frame_range(over, p, n, peers_eq, skeys, &env0)?;
                    if matches!(name, "FIRST_VALUE" | "LAST_VALUE" | "NTH_VALUE") {
                        if s > e || e >= n {
                            out.push(Value::Null);
                            continue;
                        }
                        let pick = match name {
                            "FIRST_VALUE" => s,
                            "LAST_VALUE" => e,
                            _ => {
                                let k = self.ev(&fc.args[1], &env0)?.as_i64().unwrap_or(1).max(1) as usize;
                                if s + k - 1 > e {
                                    out.push(Value::Null);
                                    continue;
                                }
                                s + k - 1
                            }
                        };
                        let env = todo_env(schema, &units[sorted[pick]], outer);
                        out.push(self.ev(&fc.args[0], &env)?);
                    } else if is_agg_name(name) {
                        if s > e || e >= n {
                            // empty frame
                            let empty: Vec<Env> = vec![];
                            out.push(self.agg_over(fc, &empty)?);
                        } else {
                            let envs: Vec<Env> = (s..=e).map(|k| todo_env(schema, &units[sorted[k]], outer)).collect();
                            out.push(self.agg_over(fc, &envs)?);
                        }
                    } else {
                        return Err(SqlError::new(10753, format!("The function '{}' may not have a window frame.", name)));
                    }
                }
            }
        }
        Ok(out)
    }

    fn frame_range(&mut self, over: &Over, p: usize, n: usize, peers_eq: &dyn Fn(usize, usize) -> bool, _skeys: &[&Vec<Value>], env0: &Env<'_>) -> Result<(usize, usize)> {
        let frame = match &over.frame {
            Some(f) => f.clone(),
            None => {
                if over.order.is_empty() {
                    return Ok((0, n.saturating_sub(1)));
                }
                crate::ast::Frame { rows: false, start: Bound::UnboundedPreceding, end: Bound::CurrentRow }
            }
        };
        let off = |b: &Bound, this: &mut Self| -> Result<Option<i64>> {
            Ok(match b {
                Bound::Preceding(e) | Bound::Following(e) => Some(this.ev(e, env0)?.as_i64().unwrap_or(0)),
                _ => None,
            })
        };
        let so = off(&frame.start, self)?;
        let eo = off(&frame.end, self)?;
        let pi = p as i64;
        let nn = n as i64;
        let (mut s, mut e): (i64, i64);
        if frame.rows {
            s = match frame.start {
                Bound::UnboundedPreceding => 0,
                Bound::CurrentRow => pi,
                Bound::Preceding(_) => pi - so.unwrap(),
                Bound::Following(_) => pi + so.unwrap(),
                Bound::UnboundedFollowing => nn,
            };
            e = match frame.end {
                Bound::UnboundedFollowing => nn - 1,
                Bound::CurrentRow => pi,
                Bound::Preceding(_) => pi - eo.unwrap(),
                Bound::Following(_) => pi + eo.unwrap(),
                Bound::UnboundedPreceding => -1,
            };
        } else {
            // RANGE: peers of current row for CURRENT ROW bounds
            let mut ps = p;
            while ps > 0 && peers_eq(ps - 1, p) {
                ps -= 1;
            }
            let mut pe = p;
            while pe + 1 < n && peers_eq(pe + 1, p) {
                pe += 1;
            }
            s = match frame.start {
                Bound::UnboundedPreceding => 0,
                Bound::CurrentRow => ps as i64,
                _ => ps as i64,
            };
            e = match frame.end {
                Bound::UnboundedFollowing => nn - 1,
                Bound::CurrentRow => pe as i64,
                _ => pe as i64,
            };
        }
        if s < 0 {
            s = 0;
        }
        if e >= nn {
            e = nn - 1;
        }
        if s > e {
            return Ok((1, 0));
        }
        Ok((s as usize, e as usize))
    }

    // ------------------------------------------------------------ FROM
    pub fn eval_table_ref(&mut self, tr: &TableRef, outer: Option<&Env<'_>>) -> Result<Rowset> {
        if self.ses.preeval.as_ref().is_some_and(|(p, _)| *p == tr as *const TableRef as usize) {
            if let Some((_, rs)) = self.ses.preeval.take() {
                return Ok(rs);
            }
        }
        match tr {
            TableRef::Table { name, alias, .. } => self.eval_named_source(name, alias.as_deref()),
            TableRef::Temporal { name, alias, clause } => self.eval_temporal(name, alias.as_deref(), clause),
            TableRef::TableVar { name, alias } => match self.ses.frames.last().and_then(|f| f.tables.get(name)) {
                Some(t) => Ok(Rowset::from_table(t, alias.as_deref().unwrap_or(name))),
                None => Err(SqlError::new(1087, format!("Must declare the table variable \"{}\".", name))),
            },
            TableRef::Derived { query, alias, cols } => {
                self.ses.stream_to_buf = query.ctes.is_empty() && matches!(query.body, QueryBody::Select(_));
                let rs = self.exec_query(query, outer);
                self.ses.stream_to_buf = false;
                let mut rs = rs?;
                rename_cols(&mut rs, cols);
                rs.schema.cols.iter_mut().for_each(|c| c.qual = alias.clone());
                if alias.is_none() {
                    // unnamed derived tables are not valid in T-SQL but tolerate
                }
                Ok(rs)
            }
            TableRef::Values { rows, alias, cols } => {
                let mut out = vec![];
                let env = match outer {
                    Some(o) => *o,
                    None => Env::empty().into_static_like(),
                };
                for r in rows {
                    let mut row = vec![];
                    for e in r {
                        row.push(self.ev(e, &env)?);
                    }
                    out.push(row);
                }
                let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
                let names: Vec<String> = match cols {
                    Some(c) => c.clone(),
                    None => (1..=ncols).map(|i| format!("Column{}", i)).collect(),
                };
                let mut rs = Rowset::from_rows(&names, out);
                rs.schema.cols.iter_mut().for_each(|c| c.qual = alias.clone());
                Ok(rs)
            }
            TableRef::Func { name, args, alias, cols, with } => {
                let env = match outer {
                    Some(o) => *o,
                    None => Env::empty().into_static_like(),
                };
                let mut vals = vec![];
                for a in args {
                    // DEFAULT as an argument of a table function means "not given", as in sys.fn_get_audit_file(path, DEFAULT, DEFAULT)
                    vals.push(if matches!(a, Expr::Default) { Value::Null } else { self.ev(a, &env)? });
                }
                let mut rs = self.table_function(name, vals, with.as_ref())?;
                rename_cols(&mut rs, cols);
                let q = alias.clone().unwrap_or_else(|| name.last().unwrap().clone());
                rs.schema.cols.iter_mut().for_each(|c| c.qual = Some(q.clone()));
                Ok(rs)
            }
            TableRef::Join { kind: JoinKind::Inner | JoinKind::Cross, .. } => self.eval_join_tree(tr, outer),
            TableRef::Join { kind, left, right, on } => {
                let l = self.eval_table_ref(left, outer)?;
                match kind {
                    JoinKind::CrossApply | JoinKind::OuterApply => {
                        let mut out_rows: Vec<Row> = vec![];
                        let mut rschema: Option<Schema> = None;
                        for lr in l.rows.iter() {
                            let env = Env::new(&l.schema, lr, outer);
                            let r = self.eval_table_ref(right, Some(&env))?;
                            if rschema.is_none() {
                                rschema = Some(r.schema.clone());
                            }
                            if r.rows.is_empty() {
                                if *kind == JoinKind::OuterApply {
                                    let mut row = lr.clone();
                                    row.extend(std::iter::repeat(Value::Null).take(r.schema.cols.len()));
                                    out_rows.push(row);
                                }
                            } else {
                                for rr in r.rows.iter() {
                                    let mut row = lr.clone();
                                    row.extend(rr.iter().cloned());
                                    out_rows.push(row);
                                }
                            }
                        }
                        let rschema = match rschema {
                            Some(s) => s,
                            None => {
                                let nulls = vec![Value::Null; l.schema.cols.len()];
                                let env = Env::new(&l.schema, &nulls, outer);
                                self.eval_table_ref(right, Some(&env)).map(|r| r.schema).unwrap_or_default()
                            }
                        };
                        Ok(Rowset::new(l.schema.concat(&rschema), out_rows))
                    }
                    _ => {
                        let r = self.eval_table_ref(right, outer)?;
                        self.join_rowsets(*kind, l, r, on, outer)
                    }
                }
            }
            TableRef::Pivot { src, agg, arg, col, vals, alias } => self.eval_pivot(src, agg, arg, col, vals, alias.as_deref(), outer),
            TableRef::Unpivot { src, vcol, ncol, cols, alias } => {
                let s = self.eval_table_ref(src, outer)?;
                let unp: Vec<usize> = cols.iter().map(|c| match s.schema.find(None, c) {
                    Found::One(i) => Ok(i),
                    _ => Err(SqlError::new(207, format!("Invalid column name '{}'.", c))),
                }).collect::<Result<_>>()?;
                let keep: Vec<usize> = (0..s.schema.cols.len()).filter(|i| !unp.contains(i)).collect();
                let mut sc: Vec<ColMeta> = keep.iter().map(|&i| s.schema.cols[i].clone()).collect();
                sc.push(ColMeta { qual: alias.clone(), name: ncol.clone(), ty: None, denied: None, hidden: false, coll: None, mask: None });
                sc.push(ColMeta { qual: alias.clone(), name: vcol.clone(), ty: None, denied: None, hidden: false, coll: None, mask: None });
                let mut rows = vec![];
                for r in s.rows.iter() {
                    for (k, &ui) in unp.iter().enumerate() {
                        if r[ui].is_null() {
                            continue;
                        }
                        let mut row: Row = keep.iter().map(|&i| r[i].clone()).collect();
                        row.push(Value::Str(cols[k].clone()));
                        row.push(r[ui].clone());
                        rows.push(row);
                    }
                }
                Ok(Rowset::new(Schema::new(sc), rows))
            }
        }
    }

    pub(crate) fn eval_named_source(&mut self, name: &[String], alias: Option<&str>) -> Result<Rowset> {
        let display = name.last().unwrap().clone();
        let q = alias.unwrap_or(&display).to_string();
        if name.len() == 1 {
            let lname = display.to_lowercase();
            // CTE?
            let mut found = None;
            for (si, m) in self.ses.cte_stack.iter().enumerate().rev() {
                if let Some(e) = m.get(&lname) {
                    found = Some((si, e.query.clone(), e.cols.clone(), e.working.clone()));
                    break;
                }
            }
            if let Some((si, query, cols, working)) = found {
                let mut rs = match working {
                    Some(w) => w,
                    None => {
                        if query_refs_name(&query, &lname) {
                            self.eval_recursive_cte(si, &lname, &query, &cols)?
                        } else {
                            self.ses.stream_to_buf = query.ctes.is_empty() && matches!(query.body, QueryBody::Select(_));
                            let r = self.exec_query(&query, None);
                            self.ses.stream_to_buf = false;
                            r?
                        }
                    }
                };
                rename_cols(&mut rs, &cols);
                rs.schema.cols.iter_mut().for_each(|c| c.qual = Some(q.clone()));
                return Ok(rs);
            }
            // trigger pseudo-tables
            if lname == "inserted" || lname == "deleted" {
                if let Some(tc) = self.ses.trigger_stack.last() {
                    let rows = if lname == "inserted" { tc.inserted.clone() } else { tc.deleted.clone() };
                    return Ok(Rowset::table_rows(&tc.table, &q, rows));
                }
            }
        }
        // system views
        if name.len() >= 2 {
            let sch = &name[name.len() - 2];
            if sch.eq_ignore_ascii_case("sys") || sch.eq_ignore_ascii_case("information_schema") {
                if let Some(mut rs) = self.sysview(sch, &display) {
                    rs.schema.cols.iter_mut().for_each(|c| c.qual = Some(q.clone()));
                    return Ok(rs);
                }
            }
        }
        let loc = self.locate(name);
        if let Some(t) = self.table_ref(&loc).cloned() {
            let pushed: Vec<Expr> = self.ses.pushdown.last().and_then(|v| v.iter().find(|(a, _)| a.eq_ignore_ascii_case(&q)).map(|(_, e)| e.clone())).unwrap_or_default();
            return self.scan_table_leaf(&loc, &t, &q, name, &pushed, None);
        }
        if let Loc::Db(_, _) = &loc {
            if let Some(Object::View(v)) = self.object_ref(&loc) {
                let v = v.clone();
                self.check_select_permission(&loc, name)?;
                let owner = match &loc {
                    Loc::Db(d, _) => self.schema_owner_of(d, &v.schema),
                    _ => String::new(),
                };
                let saved = std::mem::take(&mut self.ses.cte_stack);
                self.ses.module_owner.push(owner);
                let r = self.exec_query(&v.query, None);
                self.ses.module_owner.pop();
                self.ses.cte_stack = saved;
                let mut rs = r?;
                rename_cols(&mut rs, &v.cols);
                rs.schema.cols.iter_mut().for_each(|c| c.qual = Some(q.clone()));
                return Ok(rs);
            }
        }
        // tempdb.dbo.#t style
        Err(self.invalid_object(name))
    }

    fn eval_recursive_cte(&mut self, si: usize, lname: &str, query: &Arc<Query>, cols: &Option<Vec<String>>) -> Result<Rowset> {
        let mut members = vec![];
        flatten_union(&query.body, &mut members);
        let mut anchors = vec![];
        let mut recs = vec![];
        for m in members {
            let refs = match m {
                QueryBody::Select(s) => s.from.iter().any(|t| {
                    let q = Query { ctes: vec![], body: QueryBody::Select(Box::new(Select { distinct: false, top: None, items: vec![], into: None, from: vec![t.clone()], where_: None, group: None, having: None })), order: vec![], offset: None, fetch: None, for_clause: None, maxrecursion: None, options: vec![] };
                    query_refs_name(&q, lname)
                }),
                _ => false,
            };
            if refs {
                recs.push(m);
            } else {
                anchors.push(m);
            }
        }
        if anchors.is_empty() || recs.is_empty() {
            return Err(SqlError::new(252, format!("Recursive common table expression '{}' does not contain a top-level UNION ALL operator.", lname)));
        }
        let mut result: Vec<Row> = vec![];
        let mut schema = Schema::default();
        for a in anchors {
            let rs = self.exec_body(a, None)?;
            if schema.cols.is_empty() {
                schema = rs.schema.clone();
            }
            result.extend((*rs.rows).clone());
        }
        let mut sch = schema.clone();
        if let Some(c) = cols {
            for (i, n) in c.iter().enumerate() {
                if i < sch.cols.len() {
                    sch.cols[i].name = n.clone();
                }
            }
        }
        let max = self.ses.maxrec.or(query.maxrecursion).unwrap_or(100);
        let mut working = result.clone();
        let mut level = 0;
        while !working.is_empty() {
            level += 1;
            if max > 0 && level > max {
                if let Some(e) = self.ses.cte_stack[si].get_mut(lname) {
                    e.working = None;
                }
                return Err(SqlError::new(530, format!("The statement terminated. The maximum recursion {} has been exhausted before statement completion.", max)));
            }
            if let Some(e) = self.ses.cte_stack[si].get_mut(lname) {
                e.working = Some(Rowset::new(sch.clone(), working.clone()));
            }
            let mut next = vec![];
            for r in &recs {
                let rs = self.exec_body(r, None);
                match rs {
                    Ok(rs) => next.extend((*rs.rows).clone()),
                    Err(e) => {
                        if let Some(en) = self.ses.cte_stack[si].get_mut(lname) {
                            en.working = None;
                        }
                        return Err(e);
                    }
                }
            }
            result.extend(next.iter().cloned());
            working = next;
        }
        if let Some(e) = self.ses.cte_stack[si].get_mut(lname) {
            e.working = None;
        }
        Ok(Rowset::new(schema, result))
    }

    // ------------------------------------------------------------ joins
    pub fn join_rowsets(&mut self, kind: JoinKind, l: Rowset, r: Rowset, on: &Option<Expr>, outer: Option<&Env<'_>>) -> Result<Rowset> {
        let schema = l.schema.concat(&r.schema);
        let ln = l.schema.cols.len();
        let rn = r.schema.cols.len();
        let mut out: Vec<Row> = vec![];
        let mut rmatched = vec![false; r.rows.len()];
        let keep_left = matches!(kind, JoinKind::Left | JoinKind::Full);
        let keep_right = matches!(kind, JoinKind::Right | JoinKind::Full);

        // equi-join extraction
        let mut keys: Vec<(&Expr, &Expr)> = vec![];
        let mut residual: Vec<&Expr> = vec![];
        if let Some(c) = on {
            let mut conj = vec![];
            split_and(c, &mut conj);
            for e in conj {
                let mut used = false;
                if let Expr::Binary(BinOp::Eq, a, b) = e {
                    let sa = side_of(a, &l.schema, &r.schema);
                    let sb = side_of(b, &l.schema, &r.schema);
                    match (sa, sb) {
                        (Side::Left, Side::Right) => {
                            keys.push((a, b));
                            used = true;
                        }
                        (Side::Right, Side::Left) => {
                            keys.push((b, a));
                            used = true;
                        }
                        _ => {}
                    }
                }
                if !used {
                    residual.push(e);
                }
            }
        }
        // OPTION (LOOP JOIN): no hash table, every pair of rows is tested against the whole condition
        if self.ses.query_opts.iter().any(|o| o == "LOOP JOIN") {
            keys.clear();
            residual.clear();
        }
        let l_rows = l.rows.clone();
        let r_rows = r.rows.clone();
        // collation each join key is compared in (strings are hashed by their sort keys)
        let jcolls: Colls = if crate::collation::in_use() && !keys.is_empty() {
            let empty: Row = vec![];
            let (el, er) = (Env::new(&l.schema, &empty, outer), Env::new(&r.schema, &empty, outer));
            let mut v = vec![];
            for (lk, rk) in &keys {
                v.push(self.resolve_coll(self.expr_coll(lk, &el), self.expr_coll(rk, &er))?);
            }
            if any_coll(&v) {
                v
            } else {
                vec![]
            }
        } else {
            vec![]
        };
        if !keys.is_empty() {
            let mut map: HashMap<String, Vec<usize>> = HashMap::new();
            for (j, rr) in r_rows.iter().enumerate() {
                let env = Env::new(&r.schema, rr, outer);
                let mut kv = vec![];
                let mut has_null = false;
                for (_, rk) in &keys {
                    let v = self.ev(rk, &env)?;
                    if v.is_null() {
                        has_null = true;
                    }
                    kv.push(v);
                }
                if !has_null {
                    map.entry(if jcolls.is_empty() { key_of(&kv) } else { key_of(&keyed(&kv, &jcolls)) }).or_default().push(j);
                }
            }
            for lr in l_rows.iter() {
                let env = Env::new(&l.schema, lr, outer);
                let mut kv = vec![];
                let mut has_null = false;
                for (lk, _) in &keys {
                    let v = self.ev(lk, &env)?;
                    if v.is_null() {
                        has_null = true;
                    }
                    kv.push(v);
                }
                let mut matched = false;
                if !has_null {
                    if let Some(cands) = map.get(&if jcolls.is_empty() { key_of(&kv) } else { key_of(&keyed(&kv, &jcolls)) }) {
                        for &j in cands {
                            let mut row = lr.clone();
                            row.extend(r_rows[j].iter().cloned());
                            let mut ok = true;
                            if !residual.is_empty() {
                                let env = Env::new(&schema, &row, outer);
                                for e in &residual {
                                    if !matches!(self.ev(e, &env)?, Value::Bit(true)) {
                                        ok = false;
                                        break;
                                    }
                                }
                            }
                            if ok {
                                matched = true;
                                rmatched[j] = true;
                                out.push(row);
                            }
                        }
                    }
                }
                if !matched && keep_left {
                    let mut row = lr.clone();
                    row.extend(std::iter::repeat(Value::Null).take(rn));
                    out.push(row);
                }
            }
        } else {
            for lr in l_rows.iter() {
                let mut matched = false;
                for (j, rr) in r_rows.iter().enumerate() {
                    let mut row = lr.clone();
                    row.extend(rr.iter().cloned());
                    let ok = match on {
                        None => true,
                        Some(c) => {
                            let env = Env::new(&schema, &row, outer);
                            matches!(self.ev(c, &env)?, Value::Bit(true))
                        }
                    };
                    if ok {
                        matched = true;
                        rmatched[j] = true;
                        out.push(row);
                    }
                }
                if !matched && keep_left {
                    let mut row = lr.clone();
                    row.extend(std::iter::repeat(Value::Null).take(rn));
                    out.push(row);
                }
            }
        }
        if keep_right {
            for (j, rr) in r_rows.iter().enumerate() {
                if !rmatched[j] {
                    let mut row: Row = std::iter::repeat(Value::Null).take(ln).collect();
                    row.extend(rr.iter().cloned());
                    out.push(row);
                }
            }
        }
        Ok(Rowset::new(schema, out))
    }

    fn eval_pivot(&mut self, src: &TableRef, agg: &str, arg: &Expr, col: &[String], vals: &[String], alias: Option<&str>, outer: Option<&Env<'_>>) -> Result<Rowset> {
        let s = self.eval_table_ref(src, outer)?;
        let pidx = match s.schema.find_parts(col) {
            Found::One(i) => i,
            _ => return Err(SqlError::new(207, format!("Invalid column name '{}'.", col.last().unwrap()))),
        };
        // columns used by the aggregate argument are removed from the grouping set
        let mut used = vec![pidx];
        fn cols_of(e: &Expr, out: &mut Vec<Vec<String>>) {
            if let Expr::Col(p) = e {
                out.push(p.clone());
            }
            let mut ch = vec![];
            children(e, &mut ch);
            for c in ch {
                cols_of(c, out);
            }
        }
        let mut ac = vec![];
        cols_of(arg, &mut ac);
        for p in ac {
            if let Found::One(i) = s.schema.find_parts(&p) {
                used.push(i);
            }
        }
        let keep: Vec<usize> = (0..s.schema.cols.len()).filter(|i| !used.contains(i)).collect();
        let mut groups: Vec<(Vec<Value>, Vec<usize>)> = vec![];
        let mut gmap: HashMap<String, usize> = HashMap::new();
        for (ri, r) in s.rows.iter().enumerate() {
            let kv: Vec<Value> = keep.iter().map(|&i| r[i].clone()).collect();
            let k = key_of(&kv);
            match gmap.get(&k) {
                Some(&g) => groups[g].1.push(ri),
                None => {
                    gmap.insert(k, groups.len());
                    groups.push((kv, vec![ri]));
                }
            }
        }
        let mut sc: Vec<ColMeta> = keep.iter().map(|&i| ColMeta { qual: alias.map(|a| a.to_string()), name: s.schema.cols[i].name.clone(), ty: s.schema.cols[i].ty.clone(), denied: None, hidden: false, coll: s.schema.cols[i].coll.clone(), mask: None }).collect();
        for v in vals {
            sc.push(ColMeta { qual: alias.map(|a| a.to_string()), name: v.clone(), ty: None, denied: None, hidden: false, coll: None, mask: None });
        }
        let fc = FuncCall { name: agg.to_string(), schema: None, parts: vec![], args: vec![arg.clone()], distinct: false, star: false, over: None, within: None };
        let mut rows = vec![];
        for (kv, idxs) in groups {
            let mut row = kv;
            for v in vals {
                let sel: Vec<&Row> = idxs.iter().map(|&i| &s.rows[i]).filter(|r| {
                    let pv = &r[pidx];
                    matches!(compare(pv, &Value::Str(v.clone())), Ok(Some(Ordering::Equal)))
                }).collect();
                let envs: Vec<Env> = sel.iter().map(|r| Env::new(&s.schema, r, outer)).collect();
                let a = if envs.is_empty() && agg != "COUNT" { Value::Null } else { self.agg_over(&fc, &envs)? };
                row.push(a);
            }
            rows.push(row);
        }
        Ok(Rowset::new(Schema::new(sc), rows))
    }

    // ----------------------------------------------------------- SELECT INTO
    fn select_into(&mut self, name: &[String], schema: &Schema, rows: Vec<Row>, idents: &[(usize, i64, i64)]) -> Result<()> {
        let loc = self.locate(name);
        if self.table_ref(&loc).is_some() {
            return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", name.last().unwrap())));
        }
        let (db, schema_name, tname) = {
            let (d, s, n) = self.split_name(name);
            (d.unwrap_or_else(|| self.ses.db.clone()), s.unwrap_or_else(|| "dbo".into()), n)
        };
        let mut cols = vec![];
        for (i, c) in schema.cols.iter().enumerate() {
            if c.name.is_empty() {
                return Err(SqlError::new(8155, format!("No column name was specified for column {} of '{}'.", i + 1, tname)));
            }
            let ty = c.ty.clone().unwrap_or_else(|| {
                let mut best: Option<DataType> = None;
                let mut maxlen = 1usize;
                for r in &rows {
                    if !r[i].is_null() {
                        if let Value::Str(s) = &r[i] {
                            maxlen = maxlen.max(s.chars().count());
                        }
                        if best.is_none() {
                            best = Some(r[i].data_type());
                        }
                    }
                }
                match best {
                    Some(DataType::VarChar(_)) => DataType::VarChar(if maxlen >= 8000 { MAX } else { maxlen as i32 }),
                    Some(t) => t,
                    None => DataType::Int,
                }
            });
            let mut col = Column::simple(&c.name, ty);
            col.collation = c.coll.as_ref().map(|x| x.name.clone());
            if let Some((_, s, inc)) = idents.iter().find(|(ci, _, _)| *ci == i) {
                col.identity = Some((*s, *inc));
                col.nullable = false;
                col.ty = DataType::BigInt;
            }
            cols.push(col);
        }
        let store = if matches!(loc, Loc::Temp(_) | Loc::GTemp(_) | Loc::Var(_)) { self.srv.temp_store.clone() } else { self.srv.store.clone() };
        let mut t = Table::new(&db, &schema_name, &tname, cols, &store);
        let mut stored = vec![];
        for r in rows {
            let mut nr = Vec::with_capacity(r.len());
            for (i, v) in r.iter().enumerate() {
                nr.push(convert::cast(v, &t.columns[i].ty, None)?);
            }
            stored.push(nr);
        }
        if let Some((_, s, inc)) = idents.first().map(|x| (x.0, x.1, x.2)) {
            t.identity_next.set(Some(s + stored.len() as i64 * inc));
        }
        t.load_rows(stored);
        match &loc {
            Loc::Temp(_) | Loc::GTemp(_) => {
                t.is_temp = true;
                t.name = tname.clone();
            }
            _ => {}
        }
        t.object_id = self.srv.new_object_id();
        t.sync_indexes();
        self.put_table(&loc, t);
        self.record_table_creation(&loc)?;
        Ok(())
    }

    // ----------------------------------------------------------- FOR JSON/XML
    fn apply_for_clause(&mut self, rs: Rowset, fc: &ForClause) -> Result<Rowset> {
        let names: Vec<String> = rs.schema.cols.iter().map(|c| c.name.clone()).collect();
        let text = if fc.json {
            funcs::for_json(&names, &rs.rows, fc)
        } else {
            funcs::for_xml(&names, &rs.rows, fc)
        };
        let colname = if fc.json { "JSON_F52E2B61-18A1-11d1-B105-00805F49916B" } else { "XML_F52E2B61-18A1-11d1-B105-00805F49916B" };
        let rows = if text.is_empty() && !fc.json { vec![] } else { vec![vec![Value::Str(text)]] };
        Ok(Rowset::from_rows(&[colname.to_string()], rows))
    }
}

fn rename_cols(rs: &mut Rowset, cols: &Option<Vec<String>>) {
    if let Some(c) = cols {
        for (i, n) in c.iter().enumerate() {
            if i < rs.schema.cols.len() {
                rs.schema.cols[i].name = n.clone();
            }
        }
    }
}

fn todo_env<'e>(schema: &'e Schema, u: &'e Unit, outer: Option<&'e Env<'e>>) -> Env<'e> {
    Env { schema, row: &u.row, parent: outer, group: u.group.as_ref(), win: None }
}

fn split_and<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::And(a, b) => {
            split_and(a, out);
            split_and(b, out);
        }
        Expr::Paren(x) => split_and(x, out),
        _ => out.push(e),
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Side {
    Left,
    Right,
    Both,
    None,
}

fn side_of(e: &Expr, l: &Schema, r: &Schema) -> Side {
    let mut cols: Vec<&Vec<String>> = vec![];
    fn walk<'e>(e: &'e Expr, cols: &mut Vec<&'e Vec<String>>, bad: &mut bool) {
        match e {
            Expr::Col(p) => cols.push(p),
            Expr::Subquery(_) | Expr::Exists(_) | Expr::InQuery(..) | Expr::CmpQuant(..) => *bad = true,
            _ => {}
        }
        let mut ch = vec![];
        children(e, &mut ch);
        for c in ch {
            walk(c, cols, bad);
        }
    }
    let mut bad = false;
    walk(e, &mut cols, &mut bad);
    if bad {
        return Side::Both;
    }
    let (mut has_l, mut has_r) = (false, false);
    for p in cols {
        let inl = !matches!(l.find_parts(p), Found::None);
        let inr = !matches!(r.find_parts(p), Found::None);
        match (inl, inr) {
            (true, true) => return Side::Both,
            (true, false) => has_l = true,
            (false, true) => has_r = true,
            _ => {}
        }
    }
    match (has_l, has_r) {
        (true, true) => Side::Both,
        (true, false) => Side::Left,
        (false, true) => Side::Right,
        _ => Side::None,
    }
}

impl<'a> Env<'a> {
    /// Helper: shrink/extend lifetime of the empty env (it holds only 'static data).
    pub fn into_static_like<'b>(self) -> Env<'b> {
        Env { schema: &EMPTY_SCHEMA, row: &[], parent: None, group: None, win: None }
    }
}
