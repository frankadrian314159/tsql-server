//! Graph tables: `CREATE TABLE ... AS NODE | AS EDGE`, `MATCH`, `SHORTEST_PATH` (see docs/graph.md).
//!
//! A node table has, besides its own columns, a hidden `$graph_id` (a counter of the table's own) and `$node_id`, which is computed from
//! it: the JSON text `{"type":"node","schema":"dbo","table":"Person","id":3}`. An edge table has `$graph_id`, `$edge_id` (the same,
//! `"type":"edge"`), and `$from_id` and `$to_id`, which hold the `$node_id` of the nodes the edge joins and are given by the `INSERT`.
//! (SQL Server keeps the ids as binary pairs and shows them as this JSON; here the JSON is what is stored, so an id compares as text.)
//!
//! `MATCH(a-(e)->b)` is parsed into the equalities `e.$from_id = a.$node_id AND e.$to_id = b.$node_id`, so a pattern is a join and
//! is planned like one. `SHORTEST_PATH` is worked out here: see `shortest_path`.
use super::*;
use crate::ast::FuncCall;
use crate::catalog::{Column, GraphKind, Table};
use crate::storage::RowId;

/// What the JSON text of a graph id says.
pub struct GraphId {
    pub edge: bool,
    pub schema: String,
    pub table: String,
    pub id: i64,
}

/// The text of the id of row `id` of a node or edge table.
pub fn id_json(edge: bool, schema: &str, table: &str, id: i64) -> String {
    format!("{{\"type\":\"{}\",\"schema\":{},\"table\":{},\"id\":{}}}", if edge { "edge" } else { "node" }, serde_json::to_string(schema).unwrap_or_default(), serde_json::to_string(table).unwrap_or_default(), id)
}

/// Read the text of a graph id; `None` when it is not one.
pub fn parse_id(s: &str) -> Option<GraphId> {
    let j: serde_json::Value = serde_json::from_str(s).ok()?;
    let ty = j.get("type")?.as_str()?;
    if ty != "node" && ty != "edge" {
        return None;
    }
    Some(GraphId { edge: ty == "edge", schema: j.get("schema")?.as_str()?.to_string(), table: j.get("table")?.as_str()?.to_string(), id: j.get("id")?.as_i64()? })
}

/// The columns a graph table has before its own.
pub fn graph_columns(kind: GraphKind, schema: &str, table: &str, external: bool) -> Vec<Column> {
    let col = |name: &str, ty: DataType, nullable: bool| Column {
        name: name.to_string(),
        ty,
        nullable,
        identity: None,
        default: None,
        default_text: None,
        default_name: None,
        computed: None,
        computed_text: None,
        user_type: None,
        xml_schema: None,
        collation: None,
        mask: None,
        hidden: false,
    };
    let edge = kind == GraphKind::Edge;
    let prefix = format!("{{\"type\":\"{}\",\"schema\":{},\"table\":{},\"id\":", if edge { "edge" } else { "node" }, serde_json::to_string(schema).unwrap_or_default(), serde_json::to_string(table).unwrap_or_default());
    let lit = |s: String| Expr::Lit(Value::Str(s));
    let id_expr = Expr::Func(Box::new(FuncCall {
        name: "CONCAT".to_string(),
        schema: None,
        parts: vec![],
        args: vec![
            lit(prefix),
            Expr::Cast { e: Box::new(Expr::Col(vec!["$graph_id".to_string()])), ty: TypeRef { parts: vec!["varchar".to_string()], args: vec![20], xml_schema: None }, try_: false, style: None },
            lit("}".to_string()),
        ],
        distinct: false,
        star: false,
        over: None,
        within: None,
    }));
    let mut gid = col("$graph_id", DataType::BigInt, false);
    // (a cluster's control node numbers the rows, and the shards' tables take the numbers as they are given)
    gid.identity = if external { None } else { Some((0, 1)) };
    gid.hidden = true;
    let mut id = col(if edge { "$edge_id" } else { "$node_id" }, DataType::NVarChar(MAX), true);
    id.computed = Some(Arc::new(id_expr));
    id.computed_text = Some(format!("CONCAT('{{\"type\":\"{}\",\"schema\":\"{}\",\"table\":\"{}\",\"id\":', CAST([$graph_id] AS VARCHAR(20)), '}}')", if edge { "edge" } else { "node" }, schema, table));
    let mut v = vec![gid, id];
    if edge {
        v.push(col("$from_id", DataType::NVarChar(MAX), false));
        v.push(col("$to_id", DataType::NVarChar(MAX), false));
    }
    v
}

impl<'a> Engine<'a> {
    /// The checks an edge row has to pass: `$from_id` and `$to_id` are ids of nodes, and, when the table has `CONNECTION` constraints, they
    /// are of node tables that one of them lets it connect.
    pub(super) fn check_edge_row(&self, t: &Table, row: &Row, verb: &str) -> Result<()> {
        if t.graph != Some(GraphKind::Edge) {
            return Ok(());
        }
        let ends: Vec<GraphId> = ["$from_id", "$to_id"]
            .iter()
            .map(|c| {
                let i = t.col_index(c).expect("an edge table has $from_id and $to_id");
                match &row[i] {
                    Value::Str(s) => parse_id(s).filter(|g| !g.edge).ok_or_else(|| SqlError::new(13919, format!("The value of '{}' in table '{}' is not the id of a node (it is not the $node_id of a row of a node table).", c, t.display_name()))),
                    _ => Err(SqlError::new(13919, format!("The value of '{}' in table '{}' is not the id of a node.", c, t.display_name()))),
                }
            })
            .collect::<Result<_>>()?;
        if t.edge_constraints.is_empty() {
            return Ok(());
        }
        let key = |g: &GraphId| (g.schema.to_lowercase(), g.table.to_lowercase());
        let (from, to) = (key(&ends[0]), key(&ends[1]));
        if t.edge_constraints.iter().any(|c| c.pairs.iter().any(|(f, tt)| *f == from && *tt == to)) {
            return Ok(());
        }
        Err(SqlError::new(547, format!("The {} statement conflicted with the EDGE CONSTRAINT \"{}\". The conflict occurred in database \"{}\", table \"{}\".", verb, t.edge_constraints[0].name, t.db, t.display_name())))
    }

    /// Deleting nodes: the edges at them, in the edge tables whose `CONNECTION` constraints name the node's table, are deleted too when the
    /// constraint says `ON DELETE CASCADE`, and stop the delete (error 547) when it does not. An edge table with no constraint is not asked.
    pub(super) fn graph_parent_delete(&mut self, loc: &Loc, t: &Table, ch: &super::dml::Changes) -> Result<()> {
        if t.graph != Some(GraphKind::Node) {
            return Ok(());
        }
        let Some(nid) = t.col_index("$node_id") else { return Ok(()) };
        let view = self.row_view(loc);
        let ids: HashSet<String> = ch.deletes.iter().filter_map(|rid| t.data.get(*rid, &view)).filter_map(|r| if let Value::Str(s) = &r[nid] { Some(s.clone()) } else { None }).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let me = (t.schema.to_lowercase(), t.name.to_lowercase());
        let db = t.db.to_lowercase();
        let mut edge_tables: Vec<(Loc, bool)> = vec![];
        if let Some(d) = self.srv.databases.get(&db) {
            for (key, o) in d.objects.iter() {
                if let Object::Table(e) = o {
                    let mine: Vec<&crate::catalog::EdgeConstraintDef> = e.edge_constraints.iter().filter(|c| c.pairs.iter().any(|(f, to)| *f == me || *to == me)).collect();
                    if e.graph == Some(GraphKind::Edge) && !mine.is_empty() {
                        edge_tables.push((Loc::Db(db.clone(), key.clone()), mine.iter().any(|c| c.cascade)));
                    }
                }
            }
        }
        for (cloc, cascade) in edge_tables {
            let Some(et) = self.table_ref(&cloc).cloned() else { continue };
            let (Some(fi), Some(ti)) = (et.col_index("$from_id"), et.col_index("$to_id")) else { continue };
            let hit: Vec<RowId> = self
                .scan_rows(&cloc, &et)?
                .into_iter()
                .filter(|(_, r)| matches!(&r[fi], Value::Str(s) if ids.contains(s)) || matches!(&r[ti], Value::Str(s) if ids.contains(s)))
                .map(|(rid, _)| rid)
                .collect();
            if hit.is_empty() {
                continue;
            }
            if !cascade {
                return Err(SqlError::new(547, format!("The DELETE statement conflicted with the EDGE REFERENCE constraint \"{}\". The conflict occurred in database \"{}\", table \"{}\".", et.edge_constraints[0].name, et.db, et.display_name())));
            }
            self.apply_child(&cloc, super::dml::Changes { updates: vec![], deletes: hit, inserts: vec![] }, "DELETE")?;
        }
        Ok(())
    }

    /// The object id of the node or edge table `schema.table` of the current database.
    fn graph_table_id(&self, schema: &str, table: &str) -> Option<i32> {
        let db = self.srv.databases.get(&self.ses.db.to_lowercase())?;
        db.objects.values().find_map(|o| match o {
            Object::Table(t) if t.graph.is_some() && t.schema.eq_ignore_ascii_case(schema) && t.name.eq_ignore_ascii_case(table) => Some(t.object_id),
            _ => None,
        })
    }

    /// `NODE_ID_FROM_PARTS`, `OBJECT_ID_FROM_NODE_ID`, `GRAPH_ID_FROM_NODE_ID` and the three for edges.
    pub(super) fn graph_function(&mut self, name: &str, a: &[Value]) -> Result<Value> {
        let edge = name.contains("EDGE");
        if a.iter().any(|v| v.is_null()) {
            return Ok(Value::Null);
        }
        if name.ends_with("_FROM_PARTS") {
            let (oid, gid) = (convert::cast(&a[0], &DataType::Int, None)?.as_i64().unwrap_or(0) as i32, convert::cast(&a[1], &DataType::BigInt, None)?.as_i64().unwrap_or(0));
            let Some(db) = self.srv.databases.get(&self.ses.db.to_lowercase()) else { return Ok(Value::Null) };
            let found = db.objects.values().find_map(|o| match o {
                Object::Table(t) if t.object_id == oid && t.graph == Some(if edge { GraphKind::Edge } else { GraphKind::Node }) => Some((t.schema.clone(), t.name.clone())),
                _ => None,
            });
            return Ok(match found {
                Some((s, t)) => Value::Str(id_json(edge, &s, &t, gid)),
                None => Value::Null,
            });
        }
        let Value::Str(text) = &a[0] else { return Ok(Value::Null) };
        let Some(g) = parse_id(text).filter(|g| g.edge == edge) else { return Ok(Value::Null) };
        if name.starts_with("OBJECT_ID_") {
            return Ok(self.graph_table_id(&g.schema, &g.table).map(Value::Int).unwrap_or(Value::Null));
        }
        Ok(Value::BigInt(g.id))
    }
}

// ---------------------------------------------------------------- SHORTEST_PATH
//
// `FROM Person AS p, Person FOR PATH AS q, Knows FOR PATH AS k WHERE MATCH(SHORTEST_PATH(p(-(k)->q)+)) AND p.name = 'Alice'` asks, for each
// start node (here, the people named Alice), the shortest path to each node that can be reached from it by edges of `k`, and the select
// list is of the start node and of aggregates over the steps of the path (`STRING_AGG(q.name, '->') WITHIN GROUP (GRAPH PATH)`,
// `LAST_VALUE(q.name) ...`, `COUNT(q.name) ...`, `SUM`, `MIN`, `MAX`, `AVG`) and of `LAST_NODE(q)`.
//
// It is worked out by walking the graph breadth first from each start node, and writing every shortest path down as rows, one for each
// step: the number of the path, the number of the step, the length of the path, and the columns of the start node, the edge and the node
// of the step. The query is then rewritten to read those rows as a table, with `GROUP BY` the path, in which the path aggregates are
// aggregates over the steps (a `STRING_AGG` ordered by the step; `LAST_VALUE(x)` the `MAX` of `x` over the last step), and the columns
// are the columns of that table. So everything else the select can say (`ORDER BY`, `HAVING`, `TOP`, `DISTINCT`, being a derived table)
// is what the engine does.
//
// Of SQL Server's syntax it takes one step in the repeated pattern (`start(-(edge)->node)+` or `{m,n}`, with `<-(edge)-` for the other
// direction), conditions in the `WHERE` on the start alias only, and no `GROUP BY`. A path of at least `m` steps is a shortest path with
// its length at least `m`, filtered from the shortest paths (not the shortest of those with `m` steps or more).
use std::collections::{HashMap, HashSet, VecDeque};

struct PathSpec {
    start: String,
    edge: String,
    node: String,
    out: bool,
    min: i32,
    max: i32,
}

fn unparen(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(x) => unparen(x),
        e => e,
    }
}

fn flatten_and<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match unparen(e) {
        Expr::And(a, b) => {
            flatten_and(a, out);
            flatten_and(b, out);
        }
        e => out.push(e),
    }
}

fn path_marker(e: &Expr) -> Option<PathSpec> {
    let Expr::Func(fc) = unparen(e) else { return None };
    if fc.name != "__SHORTEST_PATH" || fc.args.len() != 6 {
        return None;
    }
    let s = |i: usize| match &fc.args[i] {
        Expr::Lit(Value::Str(s)) => s.clone(),
        _ => String::new(),
    };
    let n = |i: usize| match &fc.args[i] {
        Expr::Lit(Value::Int(n)) => *n,
        _ => 0,
    };
    Some(PathSpec { start: s(0), edge: s(1), node: s(2), out: s(3) == "out", min: n(4), max: n(5) })
}

/// The qualifiers of the column references in a JSON form of an expression (an empty one for an unqualified column).
fn qualifiers(v: &serde_json::Value, out: &mut HashSet<String>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::Array(p)) = m.get("Col") {
                out.insert(if p.len() >= 2 { p[p.len() - 2].as_str().unwrap_or("").to_lowercase() } else { String::new() });
            }
            m.values().for_each(|c| qualifiers(c, out));
        }
        serde_json::Value::Array(a) => a.iter().for_each(|c| qualifiers(c, out)),
        _ => {}
    }
}

fn is_graph_path(f: &serde_json::Value) -> bool {
    f.get("within").and_then(|w| w.as_array()).is_some_and(|w| w.iter().any(|o| o.get("e").and_then(|e| e.get("SysVar")).and_then(|s| s.as_str()) == Some("GRAPH PATH")))
}

/// `template` (a JSON expression) with the placeholder column `__arg__` replaced by `arg`.
fn fill(template: &mut serde_json::Value, arg: &serde_json::Value) {
    match template {
        serde_json::Value::Object(m) => {
            if m.get("Col").and_then(|c| c.as_array()).is_some_and(|c| c.len() == 1 && c[0].as_str() == Some("__arg__")) {
                *template = arg.clone();
                return;
            }
            m.values_mut().for_each(|c| fill(c, arg));
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|c| fill(c, arg)),
        _ => {}
    }
}

const AGGS: [&str; 7] = ["STRING_AGG", "MAX", "MIN", "SUM", "COUNT", "COUNT_BIG", "AVG"];

/// The columns, outside any aggregate, that a JSON expression reads from the rows of steps.
fn loose_columns(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(f) = m.get("Func") {
                if f.get("name").and_then(|n| n.as_str()).is_some_and(|n| AGGS.contains(&n)) {
                    return;
                }
            }
            if let Some(serde_json::Value::Array(p)) = m.get("Col") {
                if p.len() == 2 && p[0].as_str() == Some("__sp") && p[1].as_str() != Some("__pid") && !out.contains(v) {
                    out.push(v.clone());
                }
            }
            m.values().for_each(|c| loose_columns(c, out));
        }
        serde_json::Value::Array(a) => a.iter().for_each(|c| loose_columns(c, out)),
        _ => {}
    }
}

/// The three tables a pattern is over, as the rows read from them, and what they are called in the table of steps.
struct PathTables {
    names: [(String, Vec<String>); 3],
}

impl PathTables {
    /// The column of the table of steps that `parts` (as written in the query) is, if it is a column of the start, the edge or the node.
    fn column(&self, parts: &[String]) -> Option<String> {
        let (q, c) = match parts {
            [c] => (None, c),
            [q, c] => (Some(q), c),
            _ => return None,
        };
        let mut found: Vec<String> = vec![];
        for (alias, cols) in &self.names {
            if q.is_some_and(|q| !q.eq_ignore_ascii_case(alias)) {
                continue;
            }
            if let Some(real) = cols.iter().find(|x| x.eq_ignore_ascii_case(c)) {
                found.push(format!("{}${}", alias, real));
            }
        }
        if found.len() == 1 {
            found.pop()
        } else {
            None
        }
    }

    fn transform(&self, v: &mut serde_json::Value) -> Result<()> {
        match v {
            serde_json::Value::Object(m) => {
                if let Some(serde_json::Value::Array(p)) = m.get("Col") {
                    let parts: Vec<String> = p.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
                    if let Some(c) = self.column(&parts) {
                        *v = serde_json::json!({ "Col": ["__sp", c] });
                    }
                    return Ok(());
                }
                if m.contains_key("Func") {
                    // (the arguments first, then what the call itself becomes; the argument of LAST_NODE is an alias, not a column)
                    let is_last_node = m.get("Func").and_then(|f| f.get("name")).and_then(|n| n.as_str()) == Some("LAST_NODE");
                    if let (Some(f), false) = (m.get_mut("Func"), is_last_node) {
                        if let Some(args) = f.get_mut("args") {
                            self.transform(args)?;
                        }
                        if let Some(o) = f.get_mut("over") {
                            self.transform(o)?;
                        }
                    }
                    let f = m.get("Func").cloned().unwrap_or(serde_json::Value::Null);
                    let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let first_arg = f.get("args").and_then(|a| a.get(0)).cloned().unwrap_or(serde_json::Value::Null);
                    if is_graph_path(&f) {
                        match name.as_str() {
                            "STRING_AGG" => {
                                if let Some(w) = m.get_mut("Func").and_then(|f| f.get_mut("within")) {
                                    *w = serde_json::json!([{ "e": { "Col": ["__sp", "__step"] }, "desc": false }]);
                                }
                            }
                            "LAST_VALUE" => {
                                let mut t = serde_json::to_value(crate::parser::parse_expr_str("MAX(CASE WHEN __sp.__step = __sp.__plen THEN __arg__ END)")?).unwrap_or_default();
                                fill(&mut t, &first_arg);
                                *v = t;
                            }
                            "COUNT" | "COUNT_BIG" | "SUM" | "MIN" | "MAX" | "AVG" => {
                                if let Some(w) = m.get_mut("Func").and_then(|f| f.get_mut("within")) {
                                    *w = serde_json::Value::Null;
                                }
                            }
                            _ => return Err(SqlError::new(13929, format!("{} cannot be used WITHIN GROUP (GRAPH PATH).", name))),
                        }
                    } else if name == "LAST_NODE" {
                        let alias = match first_arg.get("Col").and_then(|c| c.as_array()).and_then(|c| c.last()).and_then(|c| c.as_str()) {
                            Some(a) => a.to_string(),
                            None => return Err(SqlError::new(13929, "LAST_NODE takes the alias of a node of the path.")),
                        };
                        // (the alias was mapped as if it were a column: map the node's id instead)
                        let id = self.column(&[alias, "$node_id".to_string()]).ok_or_else(|| SqlError::new(13929, "LAST_NODE takes the alias of a node of the path."))?;
                        let mut t = serde_json::to_value(crate::parser::parse_expr_str("MAX(CASE WHEN __sp.__step = __sp.__plen THEN __arg__ END)")?).unwrap_or_default();
                        fill(&mut t, &serde_json::json!({ "Col": ["__sp", id] }));
                        *v = t;
                    }
                    return Ok(());
                }
                for c in m.values_mut() {
                    self.transform(c)?;
                }
                Ok(())
            }
            serde_json::Value::Array(a) => {
                for c in a.iter_mut() {
                    self.transform(c)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl<'a> Engine<'a> {
    /// A statement with `MATCH(SHORTEST_PATH(...))` rewritten to read the steps of the shortest paths (see above); `None` for any other.
    pub(super) fn graph_rewrite(&mut self, sel: &Select, order: &[OrderItem]) -> Result<Option<(Select, Vec<OrderItem>)>> {
        let Some(w) = &sel.where_ else { return Ok(None) };
        let mut conj: Vec<&Expr> = vec![];
        flatten_and(w, &mut conj);
        let Some(mi) = conj.iter().position(|e| path_marker(e).is_some()) else { return Ok(None) };
        let spec = path_marker(conj[mi]).expect("a marker was found");
        let unsupported = |what: &str| SqlError::new(13926, format!("{} is not supported with SHORTEST_PATH on this server.", what));
        if sel.group.is_some() {
            return Err(unsupported("GROUP BY"));
        }
        let table_of = |alias: &str| -> Option<(Vec<String>, String)> {
            sel.from.iter().find_map(|t| match t {
                TableRef::Table { name, alias: a, .. } => {
                    let shown = a.clone().unwrap_or_else(|| name.last().cloned().unwrap_or_default());
                    shown.eq_ignore_ascii_case(alias).then(|| (name.clone(), shown))
                }
                _ => None,
            })
        };
        let (Some(start_t), Some(edge_t), Some(node_t)) = (table_of(&spec.start), table_of(&spec.edge), table_of(&spec.node)) else {
            return Err(SqlError::new(13926, "The aliases of a SHORTEST_PATH pattern must be tables of the FROM clause."));
        };
        if sel.from.len() != 3 || spec.start.eq_ignore_ascii_case(&spec.node) {
            return Err(unsupported("A FROM clause of other than the start node, the edge and the node of the path"));
        }
        // the conditions of the WHERE are about the start nodes
        let mut filters: Vec<Expr> = vec![];
        for (i, c) in conj.iter().enumerate() {
            if i == mi {
                continue;
            }
            let mut q = HashSet::new();
            qualifiers(&serde_json::to_value(c).unwrap_or_default(), &mut q);
            if !q.iter().all(|x| x.is_empty() || x.eq_ignore_ascii_case(&spec.start)) {
                return Err(unsupported("A condition on the nodes or edges of the path"));
            }
            filters.push((*c).clone());
        }
        let where_ = filters.into_iter().reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
        let mut read = |me: &mut Self, t: &(Vec<String>, String), w: Option<Expr>| -> Result<super::query::Rowset> {
            let q = crate::rewrite::astutil::simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Star(None)], into: None, from: vec![TableRef::Table { name: t.0.clone(), alias: Some(t.1.clone()), hints: vec![] }], where_: w, group: None, having: None });
            me.exec_query(&q, None)
        };
        let starts = read(self, &start_t, where_)?;
        let edges = read(self, &edge_t, None)?;
        let nodes = read(self, &node_t, None)?;
        let col_of = |rs: &super::query::Rowset, n: &str| rs.schema.cols.iter().position(|c| c.name.eq_ignore_ascii_case(n));
        let missing = |what: &str, alias: &str| SqlError::new(13926, format!("'{}' is not {}.", alias, what));
        let (e_from, e_to) = (col_of(&edges, "$from_id").ok_or_else(|| missing("an edge table", &spec.edge))?, col_of(&edges, "$to_id").ok_or_else(|| missing("an edge table", &spec.edge))?);
        let n_id = col_of(&nodes, "$node_id").ok_or_else(|| missing("a node table", &spec.node))?;
        let s_id = col_of(&starts, "$node_id").ok_or_else(|| missing("a node table", &spec.start))?;
        let node_rows: Vec<Row> = nodes.rows.iter().cloned().collect();
        let edge_rows: Vec<Row> = edges.rows.iter().cloned().collect();
        let mut node_at: HashMap<String, usize> = HashMap::new();
        for (i, r) in node_rows.iter().enumerate() {
            if let Value::Str(k) = &r[n_id] {
                node_at.insert(k.clone(), i);
            }
        }
        let (from_c, to_c) = if spec.out { (e_from, e_to) } else { (e_to, e_from) };
        let mut adj: HashMap<String, Vec<(usize, String)>> = HashMap::new();
        for (i, r) in edge_rows.iter().enumerate() {
            if let (Value::Str(f), Value::Str(t)) = (&r[from_c], &r[to_c]) {
                adj.entry(f.clone()).or_default().push((i, t.clone()));
            }
        }
        // breadth first from each start node: the first way found to a node is a shortest one
        let mut steps: Vec<Row> = vec![];
        let mut pid: i64 = 0;
        for srow in starts.rows.iter() {
            let Value::Str(skey) = &srow[s_id] else { continue };
            let mut parent: HashMap<String, (String, usize, i32)> = HashMap::new();
            let mut seen: HashSet<String> = HashSet::new();
            seen.insert(skey.clone());
            let mut found: Vec<String> = vec![];
            let mut queue: VecDeque<(String, i32)> = VecDeque::new();
            queue.push_back((skey.clone(), 0));
            while let Some((cur, d)) = queue.pop_front() {
                if d >= spec.max {
                    continue;
                }
                for (ei, nk) in adj.get(&cur).map(|v| v.as_slice()).unwrap_or(&[]) {
                    if seen.insert(nk.clone()) {
                        parent.insert(nk.clone(), (cur.clone(), *ei, d + 1));
                        found.push(nk.clone());
                        queue.push_back((nk.clone(), d + 1));
                    }
                }
            }
            for end in found {
                if parent[&end].2 < spec.min {
                    continue;
                }
                let mut path: Vec<(usize, String)> = vec![];
                let mut cur = end.clone();
                while cur != *skey {
                    let (prev, ei, _) = parent[&cur].clone();
                    path.push((ei, cur.clone()));
                    cur = prev;
                }
                path.reverse();
                pid += 1;
                for (k, (ei, nk)) in path.iter().enumerate() {
                    let Some(&ni) = node_at.get(nk) else { continue };
                    let mut row: Row = vec![Value::BigInt(pid), Value::Int(k as i32 + 1), Value::Int(path.len() as i32)];
                    row.extend(srow.iter().cloned());
                    row.extend(edge_rows[*ei].iter().cloned());
                    row.extend(node_rows[ni].iter().cloned());
                    steps.push(row);
                }
            }
        }
        let names = |rs: &super::query::Rowset| -> Vec<String> { rs.schema.cols.iter().map(|c| c.name.clone()).collect() };
        let tables = PathTables { names: [(spec.start.clone(), names(&starts)), (spec.edge.clone(), names(&edges)), (spec.node.clone(), names(&nodes))] };
        let mut cols: Vec<String> = vec!["__pid".to_string(), "__step".to_string(), "__plen".to_string()];
        for (alias, cs) in &tables.names {
            cols.extend(cs.iter().map(|c| format!("{}${}", alias, c)));
        }
        // (no path at all: one row of nothing, and a condition that nothing passes)
        let empty = steps.is_empty();
        if empty {
            steps.push(vec![Value::Null; cols.len()]);
        }
        let values = TableRef::Values { rows: steps.into_iter().map(|r| r.into_iter().map(Expr::Lit).collect()).collect(), alias: Some("__sp".to_string()), cols: Some(cols) };
        // the rewrite, over the JSON form of the statement
        let mut sj = serde_json::to_value(sel).map_err(|e| SqlError::new(13926, e.to_string()))?;
        let mut oj = serde_json::to_value(order).map_err(|e| SqlError::new(13926, e.to_string()))?;
        for key in ["items", "having"] {
            if let Some(x) = sj.get_mut(key) {
                tables.transform(x)?;
            }
        }
        tables.transform(&mut oj)?;
        let mut loose: Vec<serde_json::Value> = vec![];
        for key in ["items", "having"] {
            if let Some(x) = sj.get(key) {
                loose_columns(x, &mut loose);
            }
        }
        loose_columns(&oj, &mut loose);
        let mut group = vec![serde_json::json!({ "Expr": { "Col": ["__sp", "__pid"] } })];
        group.extend(loose.into_iter().map(|c| serde_json::json!({ "Expr": c })));
        sj["group"] = serde_json::Value::Array(group);
        sj["from"] = serde_json::to_value(vec![values]).map_err(|e| SqlError::new(13926, e.to_string()))?;
        sj["where_"] = if empty { serde_json::to_value(crate::parser::parse_expr_str("__sp.__pid IS NOT NULL")?).unwrap_or_default() } else { serde_json::Value::Null };
        let nsel: Select = serde_json::from_value(sj).map_err(|e| SqlError::new(13926, format!("The SHORTEST_PATH rewrite failed: {}.", e)))?;
        let norder: Vec<OrderItem> = serde_json::from_value(oj).map_err(|e| SqlError::new(13926, format!("The SHORTEST_PATH rewrite failed: {}.", e)))?;
        Ok(Some((nsel, norder)))
    }
}
