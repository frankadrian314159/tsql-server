//! Queries that run over the answer of other queries: set operations, and windows, `ROLLUP` and `DISTINCT` aggregates.
//!
//! Both stop being a reason to gather the tables. A *set operation* runs each of its branches as a query of its own (scattered,
//! joined and aggregated over the shards like any query) and combines their answers on the control node. A query with
//! windows, grouping sets or distinct aggregates (see [`plan::plan_split`]) has an inner query of plain groups (or rows) that
//! the shards answer, and an outer query that the control node's engine runs over that answer.
use crate::*;

impl ClusterSession {
    /// Run a query that has windows, grouping sets or distinct aggregates as an inner query over the shards and an outer one on
    /// the control node. None when the query is not of that kind.
    pub(crate) async fn run_split(&mut self, q: &Query) -> Result<Option<ResultSet>, SqlError> {
        let Some(sp) = plan::plan_split(q) else { return Ok(None) };
        self.plan.push("split: grouped / row query over the shards, then windows, grouping sets or distinct aggregates on the control node".into());
        let inner = Box::pin(self.run_query_flat(&sp.inner)).await?;
        merge::run_final(&mut self.mirror, &inner.columns, &inner.types, inner.rows, |_, tys| sp.final_query(merge::tmp_name(), tys)).map(Some)
    }

    /// Run `UNION`, `INTERSECT` and `EXCEPT` by running each branch alone and combining the answers on the control node.
    pub(crate) async fn run_setop(&mut self, q: &Query) -> Result<Option<ResultSet>, SqlError> {
        if !matches!(q.body, QueryBody::SetOp { .. }) || q.for_clause.is_some() {
            return Ok(None);
        }
        let mut branches: Vec<Query> = vec![];
        leaves(&q.body, &mut branches);
        let mut answers: Vec<ResultSet> = vec![];
        for b in &branches {
            answers.push(Box::pin(self.run_query(b)).await?);
        }
        // the columns of the combined answer are named as those of the first branch
        let first = &answers[0].columns;
        if first.iter().any(|n| n.is_empty()) || first.iter().enumerate().any(|(i, n)| first[..i].iter().any(|m| m.eq_ignore_ascii_case(n))) {
            return Ok(None);
        }
        self.plan.push(format!("set operation: {} branches run over the shards, combined on the control node", answers.len()));
        let mut names: Vec<String> = vec![];
        let mut result = Ok(());
        for (k, rs) in answers.into_iter().enumerate() {
            let name = format!("#dps_{}_{}", self.id, k);
            let cols: Vec<String> = if k == 0 { rs.columns.clone() } else { (0..rs.columns.len()).map(plan::cname).collect() };
            names.push(name.clone());
            if let Err(e) = merge::load_table(&mut self.mirror, &name, &cols, &rs.types, rs.rows) {
                result = Err(e);
                break;
            }
        }
        let out = match result {
            Ok(()) => {
                let mut it = names.iter();
                let body = rebuild(&q.body, &mut it);
                let mut combined = q.clone();
                combined.body = body;
                combined.ctes = vec![];
                self.mirror_query(&combined)
            }
            Err(e) => Err(e),
        };
        for n in names {
            let _ = self.mirror.execute(&format!("DROP TABLE IF EXISTS {}", n));
        }
        out.map(Some)
    }
}

/// The branches of a set operation, left to right, each as a query of its own.
fn leaves(b: &QueryBody, out: &mut Vec<Query>) {
    match b {
        QueryBody::SetOp { left, right, .. } => {
            leaves(left, out);
            leaves(right, out);
        }
        QueryBody::Paren(inner) => out.push((**inner).clone()),
        QueryBody::Select(s) => out.push(simple_query((**s).clone())),
    }
}

/// The set operation with each branch replaced by a `SELECT *` of its answer's table.
fn rebuild(b: &QueryBody, names: &mut std::slice::Iter<'_, String>) -> QueryBody {
    match b {
        QueryBody::SetOp { op, all, left, right } => {
            let l = rebuild(left, names);
            let r = rebuild(right, names);
            QueryBody::SetOp { op: *op, all: *all, left: Box::new(l), right: Box::new(r) }
        }
        _ => {
            let name = names.next().cloned().unwrap_or_default();
            QueryBody::Select(Box::new(Select {
                distinct: false,
                top: None,
                items: vec![SelectItem::Star(None)],
                into: None,
                from: vec![TableRef::Table { name: vec![name], alias: None, hints: vec![] }],
                where_: None,
                group: None,
                having: None,
            }))
        }
    }
}
