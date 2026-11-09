//! Streaming hash joins with a grace-hash fallback, for the streaming executor (see `stream.rs`).
//!
//! A join is a chain of stages. The first table is *probed*: it is read a chunk at a time and each row is pushed
//! through the stages. Every later table is a *build* side: its rows are read once into an in-memory hash table.
//! If a build side outgrows its share of the working memory the stage becomes a grace hash join: the rows already
//! hashed and the rest of the build side are written to partition files by the hash of the join key, probe rows are
//! written the same way, and when the probe input is exhausted each partition pair is joined on its own. A partition
//! whose build side still does not fit is joined in several passes (it is read in chunks that do fit, and the probe
//! partition is read once per chunk), so memory is bounded however skewed the keys are.
//!
//! Supported: inner joins and left outer joins on one or more equality conditions (plus any residual conditions),
//! between base tables. Anything else is left to the general executor.
use super::optimizer::Access;
use super::query::{children, Env, Found, Schema};
use super::spill::{row_bytes, PartFile, PartWriter, WorkMem};
use super::*;
use crate::convert::key_of;
use std::collections::{HashMap, HashSet};

/// Number of partitions a spilling stage splits into.
const PARTS: usize = 32;

/// How one table is joined to the rows before it.
pub struct JoinSpec {
    /// Left outer join: probe rows without a match are kept, extended with NULLs.
    pub left: bool,
    /// Key expressions over the probe row (the rows joined so far) and over the build row, pairwise equal.
    pub left_keys: Vec<Expr>,
    pub right_keys: Vec<Expr>,
    /// Collation each key pair is compared in (empty when all use the built-in comparison).
    pub colls: super::query::Colls,
    /// Further conditions over the joined row.
    pub residual: Vec<Expr>,
    pub left_schema: Schema,
    pub right_schema: Schema,
    pub joined: Schema,
}

fn part_of(key: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    (h.finish() % PARTS as u64) as usize
}

fn key_for(eng: &mut Engine<'_>, keys: &[Expr], colls: &super::query::Colls, schema: &Schema, row: &[Value], outer: Option<&Env<'_>>) -> Result<Option<String>> {
    let env = Env::new(schema, row, outer);
    let mut kv = Vec::with_capacity(keys.len());
    for k in keys {
        let v = eng.ev(k, &env)?;
        if v.is_null() {
            return Ok(None); // NULL never equals anything
        }
        kv.push(v);
    }
    if colls.is_empty() {
        Ok(Some(key_of(&kv)))
    } else {
        Ok(Some(key_of(&super::query::keyed(&kv, colls))))
    }
}

struct Grace {
    build: Vec<PartWriter>,
    probe: Vec<Option<PartWriter>>,
}

/// One join stage (see the module docs).
pub struct HashJoin {
    spec: JoinSpec,
    budget: usize,
    work: WorkMem,
    map: HashMap<String, Vec<Row>>,
    mem: usize,
    grace: Option<Grace>,
    right_cols: usize,
    /// Rows read into the build side, probe rows seen, and partitions that went through files (statistics).
    pub built: usize,
    pub probed: usize,
    pub spilled_partitions: usize,
    pub extra_passes: usize,
}

type Emit<'e> = &'e mut dyn FnMut(&mut Engine<'_>, Row) -> Result<bool>;

impl HashJoin {
    pub fn new(spec: JoinSpec, budget: usize, work: WorkMem) -> HashJoin {
        let right_cols = spec.right_schema.cols.len();
        HashJoin { spec, budget: budget.max(1), work, map: HashMap::new(), mem: 0, grace: None, right_cols, built: 0, probed: 0, spilled_partitions: 0, extra_passes: 0 }
    }

    /// Did the build side overflow into partition files?
    pub fn is_grace(&self) -> bool {
        self.grace.is_some() || self.spilled_partitions > 0
    }

    fn go_grace(&mut self) -> Result<()> {
        let mut build = Vec::with_capacity(PARTS);
        for _ in 0..PARTS {
            build.push(PartWriter::create(&self.work.dir)?);
        }
        for (k, rows) in self.map.drain() {
            let p = part_of(&k);
            for r in rows {
                let mut stored = Vec::with_capacity(r.len() + 1);
                stored.push(Value::Str(k.clone()));
                stored.extend(r);
                build[p].push(&stored)?;
            }
        }
        self.mem = 0;
        self.grace = Some(Grace { build, probe: (0..PARTS).map(|_| None).collect() });
        Ok(())
    }

    /// Add one row of the build side.
    pub fn add_build(&mut self, eng: &mut Engine<'_>, row: Row, outer: Option<&Env<'_>>) -> Result<()> {
        self.built += 1;
        let Some(k) = key_for(eng, &self.spec.right_keys, &self.spec.colls, &self.spec.right_schema, &row, outer)? else { return Ok(()) };
        if let Some(g) = self.grace.as_mut() {
            let mut stored = Vec::with_capacity(row.len() + 1);
            stored.push(Value::Str(k.clone()));
            stored.extend(row);
            return g.build[part_of(&k)].push(&stored);
        }
        self.mem += row_bytes(&row) + k.len() + 64;
        self.map.entry(k).or_default().push(row);
        if self.mem > self.budget {
            self.go_grace()?;
        }
        Ok(())
    }

    fn null_extended(&self, lrow: &[Value]) -> Row {
        let mut row = lrow.to_vec();
        row.extend(std::iter::repeat(Value::Null).take(self.right_cols));
        row
    }

    fn residual_ok(&self, eng: &mut Engine<'_>, row: &[Value], outer: Option<&Env<'_>>) -> Result<bool> {
        if self.spec.residual.is_empty() {
            return Ok(true);
        }
        let env = Env::new(&self.spec.joined, row, outer);
        for e in &self.spec.residual {
            if !matches!(eng.ev(e, &env)?, Value::Bit(true)) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Push one probe row. Returns false when the consumer wants no more rows.
    pub fn probe(&mut self, eng: &mut Engine<'_>, lrow: Row, outer: Option<&Env<'_>>, emit: Emit<'_>) -> Result<bool> {
        self.probed += 1;
        let key = key_for(eng, &self.spec.left_keys, &self.spec.colls, &self.spec.left_schema, &lrow, outer)?;
        if self.grace.is_some() {
            match key {
                None => {
                    if self.spec.left {
                        let row = self.null_extended(&lrow);
                        return emit(eng, row);
                    }
                }
                Some(k) => {
                    let p = part_of(&k);
                    let dir = self.work.dir.clone();
                    let g = self.grace.as_mut().unwrap();
                    if g.probe[p].is_none() {
                        g.probe[p] = Some(PartWriter::create(&dir)?);
                    }
                    let mut stored = Vec::with_capacity(lrow.len() + 1);
                    stored.push(Value::Str(k));
                    stored.extend(lrow);
                    g.probe[p].as_mut().unwrap().push(&stored)?;
                }
            }
            return Ok(true);
        }
        let mut matched = false;
        if let Some(k) = key {
            if let Some(cands) = self.map.get(&k) {
                for r in cands {
                    let mut row = lrow.clone();
                    row.extend(r.iter().cloned());
                    if self.residual_ok(eng, &row, outer)? {
                        matched = true;
                        if !emit(eng, row)? {
                            return Ok(false);
                        }
                    }
                }
            }
        }
        if !matched && self.spec.left {
            let row = self.null_extended(&lrow);
            return emit(eng, row);
        }
        Ok(true)
    }

    /// The probe input is exhausted: join the spilled partitions. Returns false when the consumer wants no more rows.
    pub fn finish(&mut self, eng: &mut Engine<'_>, outer: Option<&Env<'_>>, emit: Emit<'_>) -> Result<bool> {
        let Some(g) = self.grace.take() else { return Ok(true) };
        for (b, p) in g.build.into_iter().zip(g.probe) {
            let build: PartFile = b.finish()?;
            self.spilled_partitions += 1;
            let Some(pw) = p else { continue };
            let probe: PartFile = pw.finish()?;
            if build.count == 0 {
                if self.spec.left {
                    let mut rd = probe.reader()?;
                    while let Some(prow) = rd.next_row()? {
                        let row = self.null_extended(&prow[1..]);
                        if !emit(eng, row)? {
                            return Ok(false);
                        }
                    }
                }
                continue;
            }
            let mut flags: Vec<bool> = if self.spec.left { vec![false; probe.count] } else { vec![] };
            let mut brd = build.reader()?;
            let mut chunks = 0;
            loop {
                // the next chunk of the build partition that fits in memory
                let mut map: HashMap<String, Vec<Row>> = HashMap::new();
                let mut bytes = 0usize;
                let mut exhausted = false;
                while bytes < self.budget {
                    match brd.next_row()? {
                        Some(mut r) => {
                            let k = match r.remove(0) {
                                Value::Str(k) => k,
                                _ => String::new(),
                            };
                            bytes += row_bytes(&r) + k.len() + 64;
                            map.entry(k).or_default().push(r);
                        }
                        None => {
                            exhausted = true;
                            break;
                        }
                    }
                }
                if map.is_empty() {
                    break;
                }
                chunks += 1;
                if chunks > 1 {
                    self.extra_passes += 1;
                }
                let mut prd = probe.reader()?;
                let mut i = 0usize;
                while let Some(mut prow) = prd.next_row()? {
                    let k = match prow.remove(0) {
                        Value::Str(k) => k,
                        _ => String::new(),
                    };
                    if let Some(cands) = map.get(&k) {
                        for r in cands {
                            let mut row = prow.clone();
                            row.extend(r.iter().cloned());
                            if self.residual_ok(eng, &row, outer)? {
                                if self.spec.left {
                                    flags[i] = true;
                                }
                                if !emit(eng, row)? {
                                    return Ok(false);
                                }
                            }
                        }
                    }
                    i += 1;
                }
                if exhausted {
                    break;
                }
            }
            if self.spec.left {
                let mut prd = probe.reader()?;
                let mut i = 0usize;
                while let Some(prow) = prd.next_row()? {
                    if !flags[i] {
                        let row = self.null_extended(&prow[1..]);
                        if !emit(eng, row)? {
                            return Ok(false);
                        }
                    }
                    i += 1;
                }
            }
        }
        Ok(true)
    }
}

/// Push `row` through `stages`, then into `sink`. Returns false when the sink wants no more rows.
pub fn push_through(eng: &mut Engine<'_>, stages: &mut [HashJoin], row: Row, outer: Option<&Env<'_>>, sink: Emit<'_>) -> Result<bool> {
    match stages.split_first_mut() {
        None => sink(eng, row),
        Some((first, rest)) => first.probe(eng, row, outer, &mut |eng, joined| push_through(eng, rest, joined, outer, sink)),
    }
}

/// The probe input is exhausted: finish every stage in order (a stage's spilled partitions feed the later stages).
pub fn finish_all(eng: &mut Engine<'_>, stages: &mut [HashJoin], outer: Option<&Env<'_>>, sink: Emit<'_>) -> Result<()> {
    for i in 0..stages.len() {
        let (done, rest) = stages.split_at_mut(i + 1);
        let more = done[i].finish(eng, outer, &mut |eng, joined| push_through(eng, rest, joined, outer, sink))?;
        if !more {
            break;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------ planning
pub struct PlanLeaf {
    pub loc: Loc,
    pub t: Table,
    pub alias: String,
    pub schema: Schema,
    pub ap: Access,
    pub pushed: Vec<Expr>,
}

pub struct StagePlan {
    pub leaf: usize,
    pub spec: JoinSpec,
}

pub struct JoinPlan {
    pub leaves: Vec<PlanLeaf>,
    /// Leaves in execution order: the first is probed, the others are build sides.
    pub order: Vec<usize>,
    /// One stage per leaf after the first, in execution order.
    pub stages: Vec<StagePlan>,
    /// All leaves' columns in the order the query wrote them (what `SELECT *` and column lookup see).
    pub schema: Schema,
    /// For each column of `schema`, its position in a joined row in execution order.
    pub perm: Vec<usize>,
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

/// The leaves a condition refers to; `None` when it refers to something unresolved (an outer query, an ambiguous
/// name) or contains a subquery.
fn leaves_of(e: &Expr, schemas: &[&Schema]) -> Option<HashSet<usize>> {
    let mut set = HashSet::new();
    fn walk(e: &Expr, schemas: &[&Schema], set: &mut HashSet<usize>) -> bool {
        match e {
            Expr::Col(p) => {
                let hits: Vec<usize> = schemas.iter().enumerate().filter(|(_, s)| !matches!(s.find_parts(p), Found::None)).map(|(i, _)| i).collect();
                if hits.len() != 1 {
                    return false;
                }
                set.insert(hits[0]);
            }
            Expr::Subquery(_) | Expr::Exists(_) | Expr::InQuery(..) | Expr::CmpQuant(..) => return false,
            _ => {}
        }
        let mut ch = vec![];
        children(e, &mut ch);
        ch.into_iter().all(|c| walk(c, schemas, set))
    }
    if walk(e, schemas, &mut set) {
        Some(set)
    } else {
        None
    }
}

impl<'a> Engine<'a> {
    /// Plan a streaming hash join for `sel` (a left-deep chain of base tables joined with INNER, CROSS or LEFT joins,
    /// or a comma list of tables), or `None` when the query is not of that shape (the general executor handles it).
    pub fn plan_join_stream(&mut self, sel: &Select, outer: Option<&Env<'_>>) -> Result<Option<JoinPlan>> {
        // ---- flatten the FROM clause: leaves in written order, and how each one after the first is joined
        struct Entry<'q> {
            name: &'q Vec<String>,
            alias: Option<&'q String>,
            left: bool,
            on: Option<&'q Expr>,
        }
        fn flatten<'q>(tr: &'q TableRef, out: &mut Vec<Entry<'q>>) -> bool {
            match tr {
                TableRef::Table { name, alias, .. } => {
                    out.push(Entry { name, alias: alias.as_ref(), left: false, on: None });
                    true
                }
                TableRef::Join { kind, left, right, on } => {
                    let l = matches!(kind, JoinKind::Left);
                    if !matches!(kind, JoinKind::Inner | JoinKind::Cross | JoinKind::Left) {
                        return false;
                    }
                    if !flatten(left, out) {
                        return false;
                    }
                    match &**right {
                        TableRef::Table { name, alias, .. } => {
                            out.push(Entry { name, alias: alias.as_ref(), left: l, on: on.as_ref() });
                            true
                        }
                        _ => false,
                    }
                }
                _ => false,
            }
        }
        let mut entries: Vec<Entry<'_>> = vec![];
        for tr in &sel.from {
            if !flatten(tr, &mut entries) {
                return Ok(None);
            }
        }
        if entries.len() < 2 || entries.len() > 8 {
            return Ok(None);
        }
        if self.ses.query_opts.iter().any(|o| o == "LOOP JOIN") {
            return Ok(None);
        }
        let has_left = entries.iter().any(|e| e.left) || self.ses.query_opts.iter().any(|o| o == "FORCE ORDER");

        // ---- leaves
        let pushed_all = self.plan_pushdown(&sel.from, &sel.where_);
        let mut leaves: Vec<PlanLeaf> = vec![];
        for e in &entries {
            let loc = self.locate(e.name);
            if !matches!(loc, Loc::Db(..)) {
                return Ok(None);
            }
            let Some(t) = self.table_ref(&loc).cloned() else { return Ok(None) };
            let alias = e.alias.cloned().unwrap_or_else(|| e.name.last().cloned().unwrap_or_default());
            if leaves.iter().any(|l| l.alias.eq_ignore_ascii_case(&alias)) {
                return Ok(None);
            }
            let denied = self.select_denials(&loc, e.name, &t)?;
            let guarded = denied.iter().any(|d| d.is_some());
            let mut schema = super::dml::table_schema(&t, &alias);
            for (c, d) in schema.cols.iter_mut().zip(denied.iter()) {
                c.denied = d.clone();
            }
            let pushed: Vec<Expr> = pushed_all.iter().find(|(al, _)| al.eq_ignore_ascii_case(&alias)).map(|(_, x)| x.clone()).unwrap_or_default();
            let ap = if guarded {
                Access::Full
            } else {
                let conj: Vec<&Expr> = pushed.iter().collect();
                self.choose_access(&t, &alias, &conj)
            };
            leaves.push(PlanLeaf { loc, t, alias, schema, ap, pushed });
        }
        if !leaves.iter().any(|l| l.t.data.slot_count() >= self.srv.work.stream_min_slots) {
            return Ok(None);
        }
        let n = leaves.len();
        let schemas: Vec<&Schema> = leaves.iter().map(|l| &l.schema).collect();

        // ---- conditions: (conjunct, leaves it needs, the leaf of the LEFT JOIN it belongs to)
        let mut conds: Vec<(Expr, HashSet<usize>, Option<usize>)> = vec![];
        for (i, e) in entries.iter().enumerate() {
            if let Some(on) = e.on {
                let mut parts = vec![];
                split_and(on, &mut parts);
                for c in parts {
                    let Some(refs) = leaves_of(c, &schemas) else { return Ok(None) };
                    conds.push((c.clone(), refs, if e.left { Some(i) } else { None }));
                }
            }
        }
        if !has_left {
            if let Some(w) = &sel.where_ {
                let mut parts = vec![];
                split_and(w, &mut parts);
                for c in parts {
                    if let Some(refs) = leaves_of(c, &schemas) {
                        if refs.len() >= 2 {
                            conds.push((c.clone(), refs, None));
                        }
                    }
                }
            }
        }
        // a condition that is a plain equality between columns of two different leaves can be a join key
        let connects = |c: &(Expr, HashSet<usize>, Option<usize>), a: usize, b: usize| -> bool { matches!(&c.0, Expr::Binary(BinOp::Eq, ..)) && c.1.contains(&a) && c.1.contains(&b) };

        // ---- execution order
        let est: Vec<f64> = {
            let mut v = vec![];
            for l in leaves.iter() {
                let t = l.t.clone();
                let (alias, pushed) = (l.alias.clone(), l.pushed.clone());
                v.push(self.estimate_table(&t, &alias, &pushed));
            }
            v
        };
        let mut order: Vec<usize> = vec![];
        if has_left {
            order = (0..n).collect();
        } else {
            // probe the biggest table, build the others (smallest connected one first)
            let first = (0..n).max_by(|&a, &b| est[a].partial_cmp(&est[b]).unwrap_or(std::cmp::Ordering::Equal)).unwrap();
            order.push(first);
            while order.len() < n {
                let next = (0..n).filter(|i| !order.contains(i)).filter(|&i| order.iter().any(|&j| conds.iter().any(|c| connects(c, i, j)))).min_by(|&a, &b| est[a].partial_cmp(&est[b]).unwrap_or(std::cmp::Ordering::Equal));
                match next {
                    Some(i) => order.push(i),
                    None => return Ok(None), // a leaf with no equality to the rest would be a cross product
                }
            }
            // a big table joined to a few rows through an index is better probed by index seeks (the general executor)
            let mut sorted = est.clone();
            sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            if sorted[0] > 20.0 * sorted[1] && sorted[1] < 1000.0 {
                // is the big table's join column the first column of one of its indexes?
                let big = &leaves[order[0]];
                let indexed = conds.iter().any(|c| match &c.0 {
                    Expr::Binary(BinOp::Eq, a, b) => [a, b].iter().any(|x| match &***x {
                        Expr::Col(p) => matches!(big.schema.find_parts(p), Found::One(i) if big.t.data.specs.iter().any(|sp| sp.cols.first() == Some(&i))),
                        _ => false,
                    }),
                    _ => false,
                });
                if indexed {
                    return Ok(None);
                }
            }
        }

        // ---- stages
        let mut used = vec![false; conds.len()];
        let mut stages: Vec<StagePlan> = vec![];
        let mut acc_schema = leaves[order[0]].schema.clone();
        let mut joined_set: HashSet<usize> = HashSet::new();
        joined_set.insert(order[0]);
        for &li in order.iter().skip(1) {
            let right_schema = leaves[li].schema.clone();
            let joined = acc_schema.concat(&right_schema);
            let mut left_keys = vec![];
            let mut right_keys = vec![];
            let mut residual = vec![];
            for (ci, c) in conds.iter().enumerate() {
                if used[ci] {
                    continue;
                }
                // a LEFT JOIN's own conditions belong to its stage; others go to the first stage where they can be evaluated
                let mine = match c.2 {
                    Some(owner) => owner == li,
                    None => c.1.contains(&li) || c.1.is_empty(),
                };
                let ready = c.1.iter().all(|x| *x == li || joined_set.contains(x));
                if !mine || !ready {
                    if c.2 == Some(li) && !ready {
                        return Ok(None); // a LEFT JOIN condition that refers to a later table
                    }
                    continue;
                }
                used[ci] = true;
                let mut keyed = false;
                if let Expr::Binary(BinOp::Eq, a, b) = &c.0 {
                    let side = |x: &Expr| -> Option<bool> {
                        let r = leaves_of(x, &schemas)?;
                        if r.is_empty() {
                            return None;
                        }
                        if r.iter().all(|i| joined_set.contains(i)) {
                            Some(true) // over the rows joined so far
                        } else if r.iter().all(|i| *i == li) {
                            Some(false) // over the new table
                        } else {
                            None
                        }
                    };
                    match (side(a), side(b)) {
                        (Some(true), Some(false)) => {
                            left_keys.push((**a).clone());
                            right_keys.push((**b).clone());
                            keyed = true;
                        }
                        (Some(false), Some(true)) => {
                            left_keys.push((**b).clone());
                            right_keys.push((**a).clone());
                            keyed = true;
                        }
                        _ => {}
                    }
                }
                if !keyed {
                    residual.push(c.0.clone());
                }
            }
            if left_keys.is_empty() {
                return Ok(None);
            }
            // the collation each key pair is compared in
            let mut colls: super::query::Colls = vec![];
            if crate::collation::in_use() {
                let empty: Row = vec![];
                let (el, er) = (Env::new(&acc_schema, &empty, outer), Env::new(&right_schema, &empty, outer));
                for (lk, rk) in left_keys.iter().zip(right_keys.iter()) {
                    colls.push(self.resolve_coll(self.expr_coll(lk, &el), self.expr_coll(rk, &er))?);
                }
                if !super::query::any_coll(&colls) {
                    colls.clear();
                }
            }
            stages.push(StagePlan { leaf: li, spec: JoinSpec { left: entries[li].left, left_keys, right_keys, colls, residual, left_schema: acc_schema.clone(), right_schema, joined: joined.clone() } });
            acc_schema = joined;
            joined_set.insert(li);
        }
        if conds.iter().enumerate().any(|(i, c)| !used[i] && c.2.is_some()) {
            return Ok(None);
        }

        // ---- column order: the schema the query sees is in written order
        let mut offsets = vec![0usize; n];
        let mut off = 0;
        for &li in &order {
            offsets[li] = off;
            off += leaves[li].schema.cols.len();
        }
        let mut perm = vec![];
        let mut cols = vec![];
        for (li, l) in leaves.iter().enumerate() {
            for (k, c) in l.schema.cols.iter().enumerate() {
                perm.push(offsets[li] + k);
                cols.push(c.clone());
            }
        }
        Ok(Some(JoinPlan { leaves, order, stages, schema: Schema::new(cols), perm }))
    }
}
