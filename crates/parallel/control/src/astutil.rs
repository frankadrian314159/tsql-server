//! Helpers over the engine's syntax tree: rebuilding an expression with some parts replaced, comparing expressions
//! structurally, and finding the tables a statement reads.
use tsql_server::ast::*;

/// Rebuild `e`, replacing every sub-expression for which `f` returns a replacement (the replacement is not searched
/// again). Subqueries are kept as they are; callers that cannot handle them check [`has_subquery`] first.
pub fn map_expr(e: &Expr, f: &mut dyn FnMut(&Expr) -> Option<Expr>) -> Expr {
    if let Some(r) = f(e) {
        return r;
    }
    let m = |x: &Expr, f: &mut dyn FnMut(&Expr) -> Option<Expr>| map_expr(x, f);
    let bx = |x: Expr| Box::new(x);
    match e {
        Expr::Lit(_) | Expr::Var(_) | Expr::SysVar(_) | Expr::Col(_) | Expr::Star(_) | Expr::NextValue(_) | Expr::Default | Expr::IdentityFn { .. } => e.clone(),
        Expr::Neg(a) => Expr::Neg(bx(m(a, f))),
        Expr::BitNot(a) => Expr::BitNot(bx(m(a, f))),
        Expr::Binary(op, a, b) => Expr::Binary(*op, bx(m(a, f)), bx(m(b, f))),
        Expr::And(a, b) => Expr::And(bx(m(a, f)), bx(m(b, f))),
        Expr::Or(a, b) => Expr::Or(bx(m(a, f)), bx(m(b, f))),
        Expr::Not(a) => Expr::Not(bx(m(a, f))),
        Expr::IsNull(a, n) => Expr::IsNull(bx(m(a, f)), *n),
        Expr::InList(a, l, n) => Expr::InList(bx(m(a, f)), l.iter().map(|x| m(x, f)).collect(), *n),
        Expr::InQuery(a, q, n) => Expr::InQuery(bx(m(a, f)), q.clone(), *n),
        Expr::Like(a, p, esc, n) => Expr::Like(bx(m(a, f)), bx(m(p, f)), esc.as_ref().map(|x| bx(m(x, f))), *n),
        Expr::Between(a, lo, hi, n) => Expr::Between(bx(m(a, f)), bx(m(lo, f)), bx(m(hi, f)), *n),
        Expr::CmpQuant(op, all, a, q) => Expr::CmpQuant(*op, *all, bx(m(a, f)), q.clone()),
        Expr::Exists(_) | Expr::Subquery(_) => e.clone(),
        Expr::Case { operand, whens, els } => Expr::Case {
            operand: operand.as_ref().map(|x| bx(m(x, f))),
            whens: whens.iter().map(|(w, t)| (m(w, f), m(t, f))).collect(),
            els: els.as_ref().map(|x| bx(m(x, f))),
        },
        Expr::Cast { e: inner, ty, try_, style } => Expr::Cast { e: bx(m(inner, f)), ty: ty.clone(), try_: *try_, style: style.as_ref().map(|x| bx(m(x, f))) },
        Expr::Func(fc) => {
            let mut c = (**fc).clone();
            c.args = fc.args.iter().map(|x| m(x, f)).collect();
            // (the partition and the order of a window, and the order of WITHIN GROUP, are expressions too)
            if let Some(o) = &mut c.over {
                o.partition = o.partition.iter().map(|x| m(x, f)).collect();
                o.order = o.order.iter().map(|oi| OrderItem { e: m(&oi.e, f), desc: oi.desc }).collect();
            }
            if let Some(w) = &mut c.within {
                *w = w.iter().map(|oi| OrderItem { e: m(&oi.e, f), desc: oi.desc }).collect();
            }
            Expr::Func(Box::new(c))
        }
        Expr::Collate(a, s) => Expr::Collate(bx(m(a, f)), s.clone()),
        Expr::IsDistinct(a, b, n) => Expr::IsDistinct(bx(m(a, f)), bx(m(b, f)), *n),
        Expr::AtTimeZone(a, b) => Expr::AtTimeZone(bx(m(a, f)), bx(m(b, f))),
        Expr::Paren(a) => Expr::Paren(bx(m(a, f))),
    }
}

/// Visit `e` and every sub-expression (not descending into subqueries).
pub fn walk_expr(e: &Expr, visit: &mut dyn FnMut(&Expr)) {
    map_expr(e, &mut |x| {
        visit(x);
        None
    });
}

/// A canonical text of an expression for comparing two expressions structurally (case-insensitive, which only ever
/// makes two expressions that differ in the case of a literal compare equal: harmless for the uses here).
pub fn expr_key(e: &Expr) -> String {
    serde_json::to_string(e).unwrap_or_default().to_lowercase()
}

pub fn has_subquery(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |x| {
        if matches!(x, Expr::InQuery(..) | Expr::CmpQuant(..) | Expr::Exists(_) | Expr::Subquery(_)) {
            found = true;
        }
    });
    found
}

/// Whether the expression uses a window function or anything that needs the session (variables, sequences).
pub fn has_window_or_session(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |x| match x {
        Expr::Func(fc) if fc.over.is_some() || fc.within.is_some() => found = true,
        Expr::Var(_) | Expr::SysVar(_) | Expr::NextValue(_) => found = true,
        _ => {}
    });
    found
}

pub const AGGREGATES: &[&str] = &[
    "COUNT", "COUNT_BIG", "SUM", "AVG", "MIN", "MAX", "STDEV", "STDEVP", "VAR", "VARP", "STRING_AGG", "CHECKSUM_AGG", "APPROX_COUNT_DISTINCT", "JSON_ARRAYAGG", "JSON_OBJECTAGG",
];

pub fn is_aggregate_call(e: &Expr) -> Option<&FuncCall> {
    match e {
        Expr::Func(fc) if AGGREGATES.contains(&fc.name.as_str()) && fc.over.is_none() => Some(fc),
        _ => None,
    }
}

pub fn col(name: &str) -> Expr {
    Expr::Col(vec![name.to_string()])
}

pub fn func(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Func(Box::new(FuncCall { name: name.to_string(), schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None }))
}

/// A query that is just `SELECT <items> [FROM ...]` with everything else empty.
pub fn simple_query(select: Select) -> Query {
    Query { ctes: vec![], body: QueryBody::Select(Box::new(select)), order: vec![], offset: None, fetch: None, for_clause: None, maxrecursion: None, options: vec![] }
}

/// The form of a schema statement the shards run: a table the control node numbers (IDENTITY) is made on the shards without
/// the property, since each shard would hand out the same numbers.
pub fn shard_form(st: &Stmt) -> Stmt {
    match st {
        Stmt::CreateTable { name, defs, versioning } => {
            let defs = defs
                .iter()
                .map(|d| match d {
                    TableDef::Column(cd) if cd.identity.is_some() => {
                        let mut c = cd.clone();
                        c.identity = None;
                        TableDef::Column(c)
                    }
                    other => other.clone(),
                })
                .collect();
            Stmt::CreateTable { name: name.clone(), defs, versioning: versioning.clone() }
        }
        other => other.clone(),
    }
}

/// The normalized id of a table name: `schema.table` in lower case (the schema defaults to dbo; a database prefix is
/// ignored, as a cluster serves one database).
pub fn table_id(name: &[String]) -> String {
    let table = name.last().map(|s| s.to_lowercase()).unwrap_or_default();
    let schema = if name.len() >= 2 { name[name.len() - 2].to_lowercase() } else { "dbo".to_string() };
    format!("{}.{}", if schema.is_empty() { "dbo".to_string() } else { schema }, table)
}

/// The ids of all tables that appear in table references of a query (found by walking its serialized form: a table
/// reference is `{"Table": {"name": [...], ...}}`).
pub fn referenced_tables(q: &Query) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                if m.len() == 1 {
                    if let Some(serde_json::Value::Object(t)) = m.get("Table") {
                        if let Some(serde_json::Value::Array(parts)) = t.get("name") {
                            let name: Vec<String> = parts.iter().filter_map(|p| p.as_str().map(|s| s.to_string())).collect();
                            out.push(table_id(&name));
                        }
                    }
                }
                for c in m.values() {
                    walk(c, out);
                }
            }
            serde_json::Value::Array(a) => {
                for c in a {
                    walk(c, out);
                }
            }
            _ => {}
        }
    }
    let mut out = vec![];
    if let Ok(v) = serde_json::to_value(q) {
        walk(&v, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// Whether anything in the query is a subquery or a common table expression (which would read tables of its own).
pub fn has_nested_query(q: &Query) -> bool {
    if !q.ctes.is_empty() {
        return true;
    }
    fn walk(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => {
                // (`Paren` is both a parenthesised expression and a parenthesised query; the query has a `ctes` field)
                let paren_query = matches!(m.get("Paren"), Some(serde_json::Value::Object(inner)) if inner.contains_key("ctes"));
                paren_query || m.keys().any(|k| matches!(k.as_str(), "InQuery" | "CmpQuant" | "Exists" | "Subquery" | "Derived" | "TableVar")) || m.values().any(walk)
            }
            serde_json::Value::Array(a) => a.iter().any(walk),
            _ => false,
        }
    }
    serde_json::to_value(q).map(|v| walk(&v)).unwrap_or(true)
}
