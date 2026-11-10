//! Streaming execution of single-table queries and of joins between base tables, so a sort, `DISTINCT`, `GROUP BY`,
//! `TOP n` or join over big tables does not hold the tables (and several copies of them) in memory.
//!
//! The table is read a chunk at a time (for a join, the biggest table is read and every other table is hashed; see
//! `hashjoin.rs`, which spills to partition files when a hash table does not fit); each qualifying row is projected
//! and handed to a sink that collects, keeps the best `n` rows (`TOP`), or sorts externally with spilling
//! (`ORDER BY`, `DISTINCT`). Results are the same as the materialising executor's, which still handles everything
//! else (windows, set operations, outer joins other than LEFT, derived tables, small tables); only the working set
//! differs.
use super::hashjoin::{finish_all, push_through, HashJoin, JoinPlan};
use super::query::{any_coll, cmp_keys, collect_windows, has_agg, keyed, Colls, Env, OutCol, Rowset};
use super::spill::{Cmp, Entry, RowsWriter, Sorter, WorkMem};
use super::*;
use crate::convert::key_of;

/// Largest `TOP`/`OFFSET + FETCH` for which the best rows are kept in memory instead of sorting everything.
const TOP_N_IN_MEMORY: usize = 200_000;

/// Where projected rows go: collected, kept as a bounded best-N, or sorted externally.
struct Sinks {
    work: WorkMem,
    order_desc: Vec<bool>,
    cap: Option<usize>,
    dedupe: Option<Sorter>,
    sorter: Option<Sorter>,
    best: Vec<Entry>,
    plain: Vec<(Row, Vec<Value>)>,
    use_top_n: bool,
    has_order: bool,
    /// Collations of the output columns: rows are de-duplicated by their sort keys.
    out_colls: Colls,
    /// Plain rows go straight to a (spilling) buffer: rows still to skip, rows still wanted.
    direct: Option<(RowsWriter, usize, Option<usize>)>,
}

impl Sinks {
    fn by_order(&self) -> Cmp {
        let desc = self.order_desc.clone();
        Box::new(move |a: &Entry, b: &Entry| cmp_keys(&a.keys, &b.keys, &desc).then(a.seq.cmp(&b.seq)))
    }

    /// `restore_arrival`: the rows arrive in a different order than they should leave when there is no ORDER BY
    /// (grouping delivers groups sorted by key but the result keeps first-appearance order).
    fn new(work: WorkMem, order_desc: Vec<bool>, distinct: bool, cap: Option<usize>, restore_arrival: bool, out_colls: Colls) -> Sinks {
        let has_order = !order_desc.is_empty();
        let out_colls = if any_coll(&out_colls) { out_colls } else { vec![] };
        let mut k = Sinks { work: work.clone(), order_desc, cap, dedupe: None, sorter: None, best: vec![], plain: vec![], use_top_n: false, has_order, out_colls, direct: None };
        if distinct {
            // sort by the row's identity, then by arrival, so the first arrival of each distinct row survives
            let cmp: Cmp = Box::new(|a: &Entry, b: &Entry| a.keys[0].to_string().cmp(&b.keys[0].to_string()).then(a.seq.cmp(&b.seq)));
            k.dedupe = Some(Sorter::new(cmp, work));
        } else if has_order {
            if cap.is_some_and(|c| c <= TOP_N_IN_MEMORY) {
                k.use_top_n = true;
            } else {
                k.sorter = Some(Sorter::new(k.by_order(), work));
            }
        } else if restore_arrival {
            k.sorter = Some(Sorter::new(Box::new(|a: &Entry, b: &Entry| a.seq.cmp(&b.seq)), work));
        }
        k
    }

    /// Returns false when no more rows are needed.
    fn push(&mut self, prow: Row, keys: Vec<Value>, seq: u64) -> Result<bool> {
        if let Some(d) = self.dedupe.as_mut() {
            let mut k = vec![Value::Str(if self.out_colls.is_empty() { key_of(&prow) } else { key_of(&keyed(&prow, &self.out_colls)) })];
            k.extend(keys);
            d.push(Entry { keys: k, seq, row: prow })?;
        } else if let Some(s) = self.sorter.as_mut() {
            s.push(Entry { keys, seq, row: prow })?;
        } else if self.use_top_n {
            self.best.push(Entry { keys, seq, row: prow });
            let c = self.cap.unwrap();
            if self.best.len() >= 2 * c.max(64) {
                let cmp = self.by_order();
                self.best.sort_by(|a, b| cmp(a, b));
                self.best.truncate(c);
            }
        } else if let Some((w, skip, take)) = self.direct.as_mut() {
            if *skip > 0 {
                *skip -= 1;
                return Ok(true);
            }
            w.push(prow)?;
            if take.is_some_and(|t| w.len() >= t) {
                return Ok(false);
            }
        } else {
            self.plain.push((prow, keys));
            if let (Some(c), false) = (self.cap, self.has_order) {
                if self.plain.len() >= c {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Hand the rows to `f` in their final order (it returns false to stop); returns how many runs were spilled.
    fn finish_with(mut self, f: &mut dyn FnMut(Row, Vec<Value>) -> Result<bool>) -> Result<usize> {
        let mut spilled = 0usize;
        if let Some(d) = self.dedupe.take() {
            // survivors (the first arrival of each distinct row) go straight into a second sort that puts them in
            // (order keys, arrival) order, or arrival order
            let cmp: Cmp = if self.has_order { self.by_order() } else { Box::new(|a: &Entry, b: &Entry| a.seq.cmp(&b.seq)) };
            let mut s = Sorter::new(cmp, self.work.clone());
            let mut last: Option<String> = None;
            spilled += d.spilled_runs;
            d.finish(|e| {
                let k = e.keys[0].to_string();
                if last.as_deref() != Some(k.as_str()) {
                    last = Some(k);
                    let mut e = e;
                    e.keys.remove(0);
                    s.push(e)?;
                }
                Ok(true)
            })?;
            spilled += s.spilled_runs;
            s.finish(|e| f(e.row, e.keys))?;
        } else if let Some(s) = self.sorter.take() {
            spilled += s.spilled_runs;
            s.finish(|e| f(e.row, e.keys))?;
        } else if self.use_top_n {
            let cmp = self.by_order();
            self.best.sort_by(|a, b| cmp(a, b));
            self.best.truncate(self.cap.unwrap());
            for e in std::mem::take(&mut self.best) {
                if !f(e.row, e.keys)? {
                    break;
                }
            }
        } else {
            for (row, keys) in std::mem::take(&mut self.plain) {
                if !f(row, keys)? {
                    break;
                }
            }
        }
        Ok(spilled)
    }

    /// The rows in their final order, and how many runs were spilled.
    fn finish(self) -> Result<(Vec<(Row, Vec<Value>)>, usize)> {
        let cap = self.cap;
        let mut out: Vec<(Row, Vec<Value>)> = vec![];
        let spilled = self.finish_with(&mut |row, keys| {
            out.push((row, keys));
            // with a limit known, stop reading once enough rows are out
            Ok(cap.is_none_or(|c| out.len() < c))
        })?;
        Ok((out, spilled))
    }
}

impl<'a> Engine<'a> {
    /// Run a single-table SELECT with the streaming executor if it is eligible (see the module docs);
    /// `Ok(None)` means the caller should use the general executor.
    pub fn try_stream_select(&mut self, sel: &Select, order: &[OrderItem], offset: &Option<Expr>, fetch: &Option<Expr>, outer: Option<&Env<'_>>, may_stream: bool, to_buf: bool) -> Result<Option<Rowset>> {
        if self.ses.dry_run || sel.from.is_empty() {
            return Ok(None);
        }
        // GROUP BY over plain expressions is done by sorting on the group key; ROLLUP/CUBE/GROUPING SETS and
        // aggregates without GROUP BY (one group holding every row) stay with the general executor
        let grouped = sel.group.is_some();
        if grouped && !sel.group.as_ref().unwrap().iter().all(|g| matches!(g, GroupItem::Expr(_))) {
            return Ok(None);
        }
        if sel.having.is_some() && !grouped {
            return Ok(None);
        }
        // a lone derived table or CTE is read from its (possibly spilled) buffer
        let rowsrc_tr: Option<&TableRef> = match sel.from.as_slice() {
            [tr @ TableRef::Derived { .. }] => Some(tr),
            [tr @ TableRef::Table { name, .. }] if name.len() == 1 && self.ses.cte_stack.iter().any(|m| m.contains_key(&name[0].to_lowercase())) => Some(tr),
            _ => None,
        };
        let single = rowsrc_tr.is_none() && sel.from.len() == 1 && matches!(sel.from[0], TableRef::Table { .. });
        // inside a statement with CTEs a table name may mean a CTE: only a single table that is none stays eligible
        if !self.ses.cte_stack.is_empty() && rowsrc_tr.is_none() {
            let TableRef::Table { name, .. } = &sel.from[0] else { return Ok(None) };
            let is_cte = name.len() == 1 && self.ses.cte_stack.iter().any(|m| m.contains_key(&name[0].to_lowercase()));
            if !single || is_cte {
                return Ok(None);
            }
        }
        // only single-table queries that must sort, group, de-duplicate or limit gain anything from streaming
        // (a join always does: the general executor holds both inputs and the whole join result);
        // a result that goes into a buffer (set operand, derived table, CTE) streams plain scans too
        if single && !to_buf && !has_window_items(sel) && !(grouped || sel.distinct || !order.is_empty() || sel.top.is_some() || offset.is_some() || fetch.is_some()) {
            return Ok(None);
        }
        if rowsrc_tr.is_some() && !to_buf && !(grouped || sel.distinct || !order.is_empty() || sel.top.is_some() || offset.is_some() || fetch.is_some() || has_window_items(sel)) {
            return Ok(None);
        }
        // an index that already delivers the rows in ORDER BY order (and can stop early) beats any sort
        // (unless the whole result is wanted and can be streamed to the client: then the sort is cheap insurance
        // against holding it all in memory, and a limit would have made the ordered read the better plan)
        let limited = sel.top.is_some() || offset.is_some() || fetch.is_some();
        if single && self.compute_order_hint(sel, order, offset, fetch).is_some() && (!may_stream || limited) {
            return Ok(None);
        }
        // window functions in the select list are evaluated one partition at a time; they must all share one OVER
        // clause, and nothing else may need them (no GROUP BY, no window in ORDER BY or HAVING)
        let mut win_calls: Vec<&FuncCall> = vec![];
        for it in &sel.items {
            match it {
                SelectItem::Assign { .. } => return Ok(None),
                SelectItem::Expr { e, .. } => {
                    collect_windows(e, &mut win_calls);
                    if matches!(e, Expr::IdentityFn { .. }) {
                        return Ok(None);
                    }
                }
                SelectItem::Star(_) => {}
            }
        }
        if !win_calls.is_empty() {
            let first = format!("{:?}", win_calls[0].over);
            if grouped || sel.having.is_some() || win_calls.iter().any(|w| format!("{:?}", w.over) != first) || win_calls[0].within.is_some() {
                return Ok(None);
            }
        }
        let win_over: Option<Over> = win_calls.first().and_then(|w| w.over.clone());
        for it in &sel.items {
            if let SelectItem::Expr { e, .. } = it {
                if has_agg(e) && !grouped {
                    return Ok(None);
                }
            }
        }
        for o in order {
            let mut w = vec![];
            collect_windows(&o.e, &mut w);
            if (has_agg(&o.e) && !grouped) || !w.is_empty() {
                return Ok(None);
            }
        }
        if let Some(h) = &sel.having {
            let mut w = vec![];
            collect_windows(h, &mut w);
            if !w.is_empty() {
                return Ok(None);
            }
        }

        // ---- the source: one table, or a join of several
        struct One {
            loc: Loc,
            t: Table,
            a: String,
            ap: super::optimizer::Access,
            pushed: Vec<Expr>,
            schema: super::query::Schema,
        }
        let mut one: Option<One> = None;
        let mut join: Option<JoinPlan> = None;
        let mut rowsrc: Option<super::query::Rowset> = None;
        let schema: super::query::Schema;
        if let Some(tr) = rowsrc_tr {
            // evaluate the source now; when it turns out small, hand it to the general executor instead (which
            // picks it up from `preeval`, so nothing runs twice)
            let rs = self.eval_table_ref(tr, outer)?;
            if !(rs.rows.is_spilled() || rs.rows.count() >= self.srv.work.stream_min_slots.max(1)) {
                self.ses.preeval = Some((tr as *const TableRef as usize, rs));
                return Ok(None);
            }
            schema = rs.schema.clone();
            rowsrc = Some(rs);
        } else if single {
            let TableRef::Table { name, alias, .. } = &sel.from[0] else { return Ok(None) };
            let loc = self.locate(name);
            if !matches!(loc, Loc::Db(..)) {
                return Ok(None);
            }
            let Some(t) = self.table_ref(&loc).cloned() else { return Ok(None) };
            if t.data.slot_count() < self.srv.work.stream_min_slots {
                return Ok(None);
            }
            let a = alias.clone().unwrap_or_else(|| name.last().cloned().unwrap_or_default());
            // permissions and column binding, as the general executor does
            let denied = self.select_denials(&loc, name, &t)?;
            let guarded = denied.iter().any(|d| d.is_some());
            let mut sc = super::dml::table_schema(&t, &a);
            for (c, d) in sc.cols.iter_mut().zip(denied.iter()) {
                c.denied = d.clone();
            }
            // access path
            let pushed: Vec<Expr> = self.plan_pushdown(&sel.from, &sel.where_).into_iter().find(|(al, _)| al.eq_ignore_ascii_case(&a)).map(|(_, e)| e).unwrap_or_default();
            let ap = if guarded {
                super::optimizer::Access::Full
            } else {
                let conj: Vec<&Expr> = pushed.iter().collect();
                self.choose_access(&t, &a, &conj)
            };
            schema = sc.clone();
            one = Some(One { loc, t, a, ap, pushed, schema: sc });
        } else {
            let Some(plan) = self.plan_join_stream(sel, outer)? else { return Ok(None) };
            schema = plan.schema.clone();
            join = Some(plan);
        }
        let from_rowset = rowsrc.is_some();
        for it in &sel.items {
            if let SelectItem::Expr { e, .. } = it {
                self.validate_columns(e, &schema, outer)?;
            }
        }
        if let Some(w) = &sel.where_ {
            self.validate_columns(w, &schema, outer)?;
        }
        if let Some(g) = &sel.group {
            for gi in g {
                if let GroupItem::Expr(e) = gi {
                    self.validate_columns(e, &schema, outer)?;
                }
            }
        }
        if let Some(h) = &sel.having {
            self.validate_columns(h, &schema, outer)?;
        }
        let cols = self.out_columns(sel, &schema)?;

        // how many rows can the result need at most?
        let env0 = Env::empty();
        let off = match offset {
            Some(o) => self.ev(o, &env0)?.as_i64().unwrap_or(0).max(0) as usize,
            None => 0,
        };
        let mut take: Option<usize> = None;
        if let Some(f) = fetch {
            take = Some(self.ev(f, &env0)?.as_i64().unwrap_or(0).max(0) as usize);
        }
        if let Some(top) = &sel.top {
            if !top.percent && !top.ties {
                let n = self.ev(&top.e, &env0)?.as_i64().unwrap_or(0).max(0) as usize;
                take = Some(take.map_or(n, |t| t.min(n)));
            }
        }
        let cap: Option<usize> = take.map(|n| off + n);

        let out_colls: Colls = cols.iter().map(|c| c.coll.clone()).collect();
        let order_colls: Colls = self.order_colls(order, &cols, &schema, outer);
        let mut sinks = Sinks::new(self.srv.work.clone(), order.iter().map(|o| o.desc).collect(), sel.distinct, cap, grouped, out_colls);
        if to_buf && !may_stream && sel.into.is_none() && sel.top.as_ref().is_none_or(|t| !t.percent && !t.ties) && sinks.dedupe.is_none() && sinks.sorter.is_none() && !sinks.use_top_n {
            sinks.direct = Some((RowsWriter::new(self.srv.work.clone()), off, take));
        }
        let group_exprs: Vec<&Expr> = {
            let mut v: Vec<&Expr> = vec![];
            if let Some(g) = &sel.group {
                for gi in g {
                    if let GroupItem::Expr(e) = gi {
                        if !v.iter().any(|x| format!("{:?}", x) == format!("{:?}", e)) {
                            v.push(e);
                        }
                    }
                }
            }
            v
        };
        let group_colls: Colls = if grouped && crate::collation::in_use() {
            let empty: Row = vec![];
            let env = Env::new(&schema, &empty, outer);
            let v: Colls = group_exprs.iter().map(|g| self.expr_coll(g, &env).map(|c| c.0)).collect();
            if any_coll(&v) {
                v
            } else {
                vec![]
            }
        } else {
            vec![]
        };
        let mut grouper: Option<Sorter> = if grouped {
            Some(Sorter::new(Box::new(|a: &Entry, b: &Entry| a.keys[0].to_string().cmp(&b.keys[0].to_string()).then(a.seq.cmp(&b.seq))), self.srv.work.clone()))
        } else {
            None
        };
        let mut wsorter: Option<Sorter> = win_over.as_ref().map(|o| {
            let desc: Vec<bool> = o.order.iter().map(|x| x.desc).collect();
            let cmp: Cmp = Box::new(move |a: &Entry, b: &Entry| {
                let (ka, kb) = (a.keys[0].to_string(), b.keys[0].to_string());
                ka.cmp(&kb).then_with(|| cmp_keys(&a.keys[1..], &b.keys[1..], &desc)).then(a.seq.cmp(&b.seq))
            });
            Sorter::new(cmp, self.srv.work.clone())
        });
        let mut seq: u64 = 0;
        let mut scanned = 0usize;
        let mut join_note = String::new();
        {
            let cols = &cols;
            let schema = &schema;
            let where_ = &sel.where_;
            let (sinks, seq, grouper, group_exprs) = (&mut sinks, &mut seq, &mut grouper, &group_exprs);
            let (wsorter_ref, win_over_ref) = (&mut wsorter, &win_over);
            let (group_colls, order_colls) = (&group_colls, &order_colls);
            // what happens to each row of the source: filter, then group, or project and hand to the sink
            let mut handle = |eng: &mut Engine<'_>, row: Row| -> Result<bool> {
                let env = Env::new(schema, &row, outer);
                if let Some(w) = where_ {
                    if !matches!(eng.ev(w, &env)?, Value::Bit(true)) {
                        return Ok(true);
                    }
                }
                *seq += 1;
                if let (Some(ws), Some(over)) = (wsorter_ref.as_mut(), win_over_ref.as_ref()) {
                    // window functions: sort by partition, then by the window's order, and process partition by partition
                    let mut pv = Vec::with_capacity(over.partition.len());
                    for p in &over.partition {
                        pv.push(eng.ev(p, &env)?);
                    }
                    let mut keys = vec![Value::Str(key_of(&pv))];
                    for o in &over.order {
                        keys.push(eng.ev(&o.e, &env)?);
                    }
                    ws.push(Entry { keys, seq: *seq, row })?;
                    return Ok(true);
                }
                if let Some(g) = grouper.as_mut() {
                    // grouping: sort the filtered rows by group key; groups are processed afterwards
                    let mut kv = Vec::with_capacity(group_exprs.len());
                    for ge in group_exprs.iter() {
                        kv.push(eng.ev(ge, &env)?);
                    }
                    let gk = if group_colls.is_empty() { key_of(&kv) } else { key_of(&keyed(&kv, group_colls)) };
                    g.push(Entry { keys: vec![Value::Str(gk)], seq: *seq, row })?;
                    return Ok(true);
                }
                let (prow, keys) = eng.project_env(&env, cols, order, order_colls)?;
                sinks.push(prow, keys, *seq)
            };
            if let Some(rs) = rowsrc.take() {
                let mut it = rs.rows.stream();
                while let Some(row) = it.next_row()? {
                    scanned += 1;
                    if !handle(self, row)? {
                        break;
                    }
                }
            } else if let Some(o) = &one {
                let scanned = &mut scanned;
                self.scan_each(&o.loc, &o.t, &o.ap, Some((&o.schema, &o.pushed)), &mut |eng, chunk| {
                    for (_, row) in chunk {
                        *scanned += 1;
                        if !handle(eng, row)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })?;
            } else if let Some(plan) = join.take() {
                let JoinPlan { leaves, order: exec_order, stages, perm, .. } = plan;
                let budget = (self.srv.work.bytes / stages.len().max(1)).max(1);
                let work = self.srv.work.clone();
                // build sides first: each later table goes into its stage's hash table (or its partition files)
                let mut joins: Vec<HashJoin> = vec![];
                let mut names: Vec<(String, String)> = vec![];
                for st in stages {
                    let leaf = &leaves[st.leaf];
                    names.push((leaf.alias.clone(), leaf.ap.describe()));
                    let mut hj = HashJoin::new(st.spec, budget, work.clone());
                    self.scan_each(&leaf.loc, &leaf.t, &leaf.ap, Some((&leaf.schema, &leaf.pushed)), &mut |eng, chunk| {
                        for (_, row) in chunk {
                            hj.add_build(eng, row, outer)?;
                        }
                        Ok(true)
                    })?;
                    joins.push(hj);
                }
                // then the first table is read and every row goes through the stages
                let mut sink = |eng: &mut Engine<'_>, joined: Row| -> Result<bool> {
                    let row: Row = perm.iter().map(|&i| joined[i].clone()).collect();
                    handle(eng, row)
                };
                let probe = &leaves[exec_order[0]];
                let mut stopped = false;
                {
                    let (joins, scanned, stopped) = (&mut joins, &mut scanned, &mut stopped);
                    self.scan_each(&probe.loc, &probe.t, &probe.ap, Some((&probe.schema, &probe.pushed)), &mut |eng, chunk| {
                        for (_, row) in chunk {
                            *scanned += 1;
                            if !push_through(eng, &mut joins[..], row, outer, &mut sink)? {
                                *stopped = true;
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    })?;
                }
                if !stopped {
                    finish_all(self, &mut joins[..], outer, &mut sink)?;
                }
                let mut parts = vec![format!("scan {}: {}", probe.alias, probe.ap.describe())];
                for (hj, (n, ap)) in joins.iter().zip(names.iter()) {
                    let how = if hj.is_grace() { format!("grace hash, {} partition(s) spilled{}", hj.spilled_partitions, if hj.extra_passes > 0 { format!(", {} extra pass(es)", hj.extra_passes) } else { String::new() }) } else { "hash in memory".to_string() };
                    parts.push(format!("scan {} [{} rows]: {}; {}", n, hj.built, ap, how));
                }
                join_note = parts.join(" -> ");
            }
        }
        let mut win_runs = 0usize;
        if let Some(ws) = wsorter.take() {
            win_runs = ws.spilled_runs;
            let mut cur: Vec<(u64, Row)> = vec![];
            let mut cur_key: Option<String> = None;
            let r = {
                let eng = &mut *self;
                let (schema_ref, cols_ref, order_colls_ref, sinks_ref) = (&schema, &cols, &order_colls, &mut sinks);
                let mut more = true;
                let r = ws.finish(|e| {
                    let k = e.keys[0].to_string();
                    if cur_key.as_deref() != Some(k.as_str()) {
                        if !cur.is_empty() {
                            more = flush_window_partition(eng, &win_calls, schema_ref, cols_ref, order, order_colls_ref, outer, sinks_ref, std::mem::take(&mut cur))?;
                            if !more {
                                return Ok(false);
                            }
                        }
                        cur_key = Some(k);
                    }
                    cur.push((e.seq, e.row));
                    Ok(true)
                });
                match r {
                    Ok(()) if more && !cur.is_empty() => flush_window_partition(eng, &win_calls, schema_ref, cols_ref, order, order_colls_ref, outer, sinks_ref, std::mem::take(&mut cur)).map(|_| ()),
                    other => other,
                }
            };
            r?;
        }
        let mut group_runs = 0usize;
        let mut groups = 0usize;
        if let Some(g) = grouper.take() {
            group_runs = g.spilled_runs;
            let base_str: Vec<String> = group_exprs.iter().map(|e| format!("{:?}", e)).collect();
            let mut cur: Vec<Row> = vec![];
            let mut cur_key: Option<String> = None;
            let mut first_seq = 0u64;
            // each finished group: HAVING, then the select list and ORDER BY keys, evaluated over the group, and
            // the group's rows are dropped before the next group is read
            let mut pending_error: Option<SqlError> = None;
            {
                let eng = &mut *self;
                let schema_ref = &schema;
                let cols_ref = &cols;
                let order_colls_ref = &order_colls;
                let sinks_ref = &mut sinks;
                let groups_ref = &mut groups;
                let mut process = |rows: Vec<Row>, fs: u64| -> Result<()> {
                    *groups_ref += 1;
                    let rep: Row = rows[0].clone();
                    let ctx = super::query::GroupCtx { rows, excluded: vec![false; base_str.len()], base: base_str.clone() };
                    let env = Env { schema: schema_ref, row: &rep, parent: outer, group: Some(&ctx), win: None };
                    if let Some(h) = &sel.having {
                        if !matches!(eng.ev(h, &env)?, Value::Bit(true)) {
                            return Ok(());
                        }
                    }
                    let (prow, keys) = eng.project_env(&env, cols_ref, order, order_colls_ref)?;
                    sinks_ref.push(prow, keys, fs)?;
                    Ok(())
                };
                let r = g.finish(|e| {
                    let k = e.keys[0].to_string();
                    if cur_key.as_deref() != Some(k.as_str()) {
                        if !cur.is_empty() {
                            process(std::mem::take(&mut cur), first_seq)?;
                        }
                        cur_key = Some(k);
                        first_seq = e.seq;
                    }
                    cur.push(e.row);
                    Ok(true)
                });
                if let Err(e) = r {
                    pending_error = Some(e);
                } else if !cur.is_empty() {
                    if let Err(e) = process(std::mem::take(&mut cur), first_seq) {
                        pending_error = Some(e);
                    }
                }
            }
            if let Some(e) = pending_error {
                return Err(e);
            }
        }
        let use_top_n = sinks.use_top_n;
        // a result that feeds another operator is collected in a buffer that spills to a file when it is big
        let into_ok = sel.into.is_none() && sel.top.as_ref().is_none_or(|t| !t.percent && !t.ties);
        if to_buf && into_ok && !may_stream {
            let schema_out = super::query::Schema::new(cols.iter().map(|c| super::query::ColMeta { qual: None, name: c.name.clone(), ty: c.ty.clone(), denied: None, hidden: false, coll: c.coll.clone(), mask: None }).collect());
            let mut out = RowsWriter::new(self.srv.work.clone());
            let mut skipped = 0usize;
            let mut sent = 0usize;
            let direct = sinks.direct.take();
            let mut spilled = 0usize;
            if let Some((w, _, _)) = direct {
                out = w;
                sent = out.len();
            } else {
                spilled = sinks.finish_with(&mut |row, _keys| {
                    if skipped < off {
                        skipped += 1;
                        return Ok(true);
                    }
                    if take.is_some_and(|t| sent >= t) {
                        return Ok(false);
                    }
                    sent += 1;
                    out.push(row)?;
                    Ok(take.is_none_or(|t| sent < t))
                })?;
            }
            let rows = out.finish()?;
            let what = match &one {
                Some(o) => format!("streaming scan {} [{} rows read]: {}", o.a, scanned, o.ap.describe()),
                None if from_rowset => format!("streaming rowset source [{} rows read]", scanned),
                None => format!("streaming hash join [{} probe rows read]: {}", scanned, join_note),
            };
            self.ses.plan.push(format!("{}, buffered ({}) -> {} rows", what, if rows.is_spilled() { format!("spilled; {} run(s)", spilled) } else { "in memory".to_string() }, rows.count()));
            return Ok(Some(Rowset { schema: schema_out, rows, suppress: false, report_count: false }));
        }
        // the rows of a plain top-level SELECT go to the client as they leave the final sort or merge
        let top_plain = sel.top.as_ref().is_none_or(|t| !t.percent && !t.ties);
        if may_stream && sel.into.is_none() && top_plain && self.sink.is_some() {
            let (sent, spilled) = self.stream_out(sinks, &cols, off, take)?;
            let sort_note = if order.is_empty() { String::new() } else if use_top_n { ", top-N in memory".to_string() } else { format!(", external sort ({} spilled run(s))", spilled) };
            let group_note = if grouped { format!(", group by sort ({} group(s), {} spilled run(s))", groups, group_runs) } else if win_over.is_some() { format!(", window by sort ({} spilled run(s))", win_runs) } else { String::new() };
            let what = match &one {
                Some(o) => format!("streaming scan {} [{} rows read]: {}", o.a, scanned, o.ap.describe()),
                None if from_rowset => format!("streaming rowset source [{} rows read]", scanned),
                None => format!("streaming hash join [{} probe rows read]: {}", scanned, join_note),
            };
            self.ses.plan.push(format!("{}{}{}{}, rows streamed to the client -> {} rows", what, group_note, if sel.distinct { ", distinct by sort" } else { "" }, sort_note, sent));
            self.ses.rowcount = sent as i64;
            let mut rs = Rowset::empty();
            rs.suppress = true;
            rs.report_count = true;
            return Ok(Some(rs));
        }
        let (out, spilled) = sinks.finish()?;
        let sort_note = if order.is_empty() {
            String::new()
        } else if use_top_n {
            ", top-N in memory".to_string()
        } else {
            format!(", external sort ({} spilled run(s))", spilled)
        };
        let group_note = if grouped { format!(", group by sort ({} group(s), {} spilled run(s))", groups, group_runs) } else if win_over.is_some() { format!(", window by sort ({} spilled run(s))", win_runs) } else { String::new() };
        let what = match &one {
            Some(o) => format!("streaming scan {} [{} rows read]: {}", o.a, scanned, o.ap.describe()),
            None if from_rowset => format!("streaming rowset source [{} rows read]", scanned),
            None => format!("streaming hash join [{} probe rows read]: {}", scanned, join_note),
        };
        self.ses.plan.push(format!("{}{}{}{} -> {} rows", what, group_note, if sel.distinct { ", distinct by sort" } else { "" }, sort_note, out.len()));
        self.finish_rows(sel, order, offset, fetch, &cols, out, &[]).map(Some)
    }

    /// Deliver the sink's rows to the client (see `Streamed`): skip `off`, send at most `take`. The column types of
    /// expressions are not declared, so they are guessed from the first 64 rows, which are held back until then.
    fn stream_out(&mut self, sinks: Sinks, cols: &[OutCol<'_>], off: usize, take: Option<usize>) -> Result<(usize, usize)> {
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let mut buf: Vec<Row> = vec![];
        let mut header_sent = false;
        let (mut skipped, mut sent) = (0usize, 0usize);
        let mut result: Result<()> = Ok(());
        let eng = &mut *self;
        let header = |eng: &mut Engine<'_>, buf: &[Row]| -> Result<()> {
            let types: Vec<DataType> = cols.iter().enumerate().map(|(i, c)| c.ty.clone().unwrap_or_else(|| buf.iter().filter_map(|r| r.get(i)).find(|v| !v.is_null()).map(|v| v.data_type()).unwrap_or(DataType::Int))).collect();
            let inferred: Vec<bool> = cols.iter().map(|c| c.ty.is_none()).collect();
            eng.stream_item(Streamed::Header { columns: names.clone(), types, inferred })
        };
        let spilled = sinks.finish_with(&mut |row, _keys| {
            if skipped < off {
                skipped += 1;
                return Ok(true);
            }
            if take.is_some_and(|t| sent >= t) {
                return Ok(false);
            }
            sent += 1;
            if !header_sent {
                buf.push(row);
                if buf.len() >= 64 {
                    header(eng, &buf)?;
                    header_sent = true;
                    for r in buf.drain(..) {
                        eng.stream_item(Streamed::Row(r))?;
                    }
                }
            } else {
                eng.stream_item(Streamed::Row(row))?;
            }
            Ok(take.is_none_or(|t| sent < t))
        });
        if let Err(e) = spilled {
            result = Err(e);
        }
        result?;
        if !header_sent {
            header(self, &buf)?;
            for r in buf.drain(..) {
                self.stream_item(Streamed::Row(r))?;
            }
        }
        self.stream_item(Streamed::End(sent as u64))?;
        Ok((sent, 0))
    }

    /// Project one source row to the SELECT list and compute its ORDER BY keys (as the general executor does).
    fn project_env(&mut self, env: &Env<'_>, cols: &[OutCol<'_>], order: &[OrderItem], order_colls: &Colls) -> Result<(Row, Vec<Value>)> {
        let row = env.row;
        let mut out = Vec::with_capacity(cols.len());
        for c in cols {
            if let Some(i) = c.src_idx {
                out.push(c.out_value(row[i].clone()));
            } else {
                let v = self.ev(c.expr.unwrap(), env)?;
                out.push(c.out_value(v));
            }
        }
        let mut keys = Vec::with_capacity(order.len());
        for o in order {
            let mut done = false;
            match &o.e {
                Expr::Lit(Value::Int(n)) => {
                    let idx = *n as usize;
                    if idx == 0 || idx > out.len() {
                        return Err(SqlError::new(108, format!("The ORDER BY position number {} is out of range of the number of items in the select list.", n)));
                    }
                    keys.push(out[idx - 1].clone());
                    done = true;
                }
                Expr::Col(p) if p.len() == 1 => {
                    if let Some(ci) = cols.iter().position(|c| !c.name.is_empty() && c.name.eq_ignore_ascii_case(&p[0])) {
                        keys.push(out[ci].clone());
                        done = true;
                    }
                }
                _ => {}
            }
            if !done {
                keys.push(self.ev(&o.e, env)?);
            }
        }
        if !order_colls.is_empty() {
            keys = keyed(&keys, order_colls);
        }
        Ok((out, keys))
    }
}

fn has_window_items(sel: &Select) -> bool {
    let mut w = vec![];
    for it in &sel.items {
        if let SelectItem::Expr { e, .. } = it {
            collect_windows(e, &mut w);
        }
    }
    !w.is_empty()
}

/// Evaluate the window functions of one partition, then project and sink its rows.
#[allow(clippy::too_many_arguments)]
fn flush_window_partition(eng: &mut Engine<'_>, wins: &[&FuncCall], schema: &super::query::Schema, cols: &[OutCol<'_>], order: &[OrderItem], order_colls: &Colls, outer: Option<&Env<'_>>, sinks: &mut Sinks, part: Vec<(u64, Row)>) -> Result<bool> {
    let (seqs, rows): (Vec<u64>, Vec<Row>) = part.into_iter().unzip();
    let vals = eng.window_rows(wins, schema, rows.clone(), outer)?;
    for (i, row) in rows.iter().enumerate() {
        let env = Env { schema, row, parent: outer, group: None, win: vals.get(i) };
        let (prow, keys) = eng.project_env(&env, cols, order, order_colls)?;
        if !sinks.push(prow, keys, seqs[i])? {
            return Ok(false);
        }
    }
    Ok(true)
}
