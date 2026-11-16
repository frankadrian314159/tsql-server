//! Foreign keys across shards.
//!
//! A shard cannot see the parent rows another shard holds, so the shards do not enforce foreign keys (they are not given
//! them); the control node does. The definitions are kept in its state, and every statement that could break one is checked
//! before it is sent:
//!
//! * an `INSERT` into a child table, or an `UPDATE` of its foreign key columns, needs every new (non-null) key to exist in the
//!   parent table (a query over the cluster; the keys are compared by the control node's engine, so collation applies);
//! * a `DELETE` from a parent table, or an `UPDATE` of the columns a foreign key refers to, looks for child rows that refer
//!   to the rows concerned: `NO ACTION` makes that an error (547), `ON DELETE CASCADE` deletes them (their own children
//!   in turn) and `ON DELETE SET NULL` sets their key columns to null;
//! * `TRUNCATE` and `DROP` of a table that a foreign key refers to are refused.
//!
//! The checks run as queries within the statement's own transaction, so a transaction sees its own changes; but nothing
//! locks the rows the checks looked at across shards, so two sessions at once can still produce an orphan (a child inserted
//! while its parent is deleted). A foreign key whose parent is the same table works (the rows of one statement may refer to
//! each other). `ON UPDATE` actions other than `NO ACTION`, and `SET DEFAULT`, are not supported.
use crate::*;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum FkAction {
    NoAction,
    Cascade,
    SetNull,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FkDef {
    pub name: String,
    /// table ids
    pub child: String,
    pub child_cols: Vec<String>,
    pub parent: String,
    pub parent_cols: Vec<String>,
    pub on_delete: FkAction,
}

fn action(s: &str) -> Result<Option<FkAction>, SqlError> {
    match s.trim().to_uppercase().replace('_', " ").as_str() {
        "" | "NO ACTION" => Ok(Some(FkAction::NoAction)),
        "CASCADE" => Ok(Some(FkAction::Cascade)),
        "SET NULL" => Ok(Some(FkAction::SetNull)),
        other => Err(unsupported(&format!("a foreign key with the action {}", other))),
    }
}

/// `cols` = a tuple of the values, for each tuple (a condition for rows of a table that match any of them).
pub fn tuple_cond(cols: &[String], tuples: &[Vec<Value>]) -> Expr {
    let eq = |c: &String, v: &Value| Expr::Binary(BinOp::Eq, Box::new(col(c)), Box::new(Expr::Lit(v.clone())));
    if cols.len() == 1 {
        return Expr::InList(Box::new(col(&cols[0])), tuples.iter().map(|t| Expr::Lit(t[0].clone())).collect(), false);
    }
    let one = |t: &Vec<Value>| cols.iter().zip(t).map(|(c, v)| eq(c, v)).reduce(|a, b| Expr::And(Box::new(a), Box::new(b))).expect("a key has columns");
    tuples.iter().map(one).reduce(|a, b| Expr::Or(Box::new(a), Box::new(b))).expect("some tuples")
}

fn select(table: &Name, items: Vec<SelectItem>, where_: Option<Expr>, distinct: bool) -> Query {
    simple_query(Select { distinct, top: None, items, into: None, from: vec![TableRef::Table { name: table.clone(), alias: None, hints: vec![] }], where_, group: None, having: None })
}

fn items_of(cols: &[String]) -> Vec<SelectItem> {
    cols.iter().map(|c| SelectItem::Expr { e: col(c), alias: None }).collect()
}

const CHUNK: usize = 400;

impl Cluster {
    pub(crate) fn fks_of_child(&self, id: &str) -> Vec<FkDef> {
        self.fks.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|f| f.child == id).cloned().collect()
    }

    pub(crate) fn fks_of_parent(&self, id: &str) -> Vec<FkDef> {
        self.fks.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|f| f.parent == id).cloned().collect()
    }
}

/// The columns of the primary key of a table, from the control node's engine.
fn primary_key_of(conn: &Connection, name: &Name) -> Vec<String> {
    let shown = format!("[{}].[{}]", if name.len() >= 2 { name[name.len() - 2].clone() } else { "dbo".to_string() }, name.last().cloned().unwrap_or_default());
    conn.table_info(&shown).and_then(|t| t.indexes.into_iter().find(|i| i.primary_key)).map(|i| i.columns.into_iter().map(|(c, _)| c).collect()).unwrap_or_default()
}

impl ClusterSession {
    /// The foreign keys a `CREATE TABLE` (or an `ALTER TABLE ... ADD`) declares, validated; `own_pk` is the primary key of the
    /// table being created (a key may refer to its own table).
    pub(crate) fn declared_fks(&self, child: &Name, defs: &[TableDef], own_pk: &[String]) -> Result<Vec<FkDef>, SqlError> {
        let mut out = vec![];
        let mut add = |cols: Vec<String>, def: &ForeignKeyDef| -> Result<(), SqlError> {
            if action(&def.on_update)? != Some(FkAction::NoAction) {
                return Err(unsupported("ON UPDATE actions of a foreign key"));
            }
            let on_delete = action(&def.on_delete)?.unwrap_or(FkAction::NoAction);
            let parent = table_id(&def.table);
            let child_id = table_id(child);
            let parent_cols = match &def.rcols {
                Some(c) => c.clone(),
                None if parent == child_id => own_pk.to_vec(),
                None => primary_key_of(&self.mirror, &def.table),
            };
            if parent_cols.is_empty() || parent_cols.len() != cols.len() {
                return Err(SqlError::new(1773, "Foreign key references invalid table or column list."));
            }
            if parent != child_id && self.table_by_id(&parent).is_none() {
                return Err(SqlError::new(1767, format!("Foreign key '{}' references invalid table '{}'.", def.name.clone().unwrap_or_default(), def.table.join("."))));
            }
            let name = def.name.clone().unwrap_or_else(|| format!("FK__{}__{}", child.last().cloned().unwrap_or_default(), parent_cols.join("_")));
            out.push(FkDef { name, child: child_id, child_cols: cols, parent, parent_cols, on_delete });
            Ok(())
        };
        for d in defs {
            match d {
                TableDef::Column(cd) => {
                    if let Some(f) = &cd.fk {
                        add(vec![cd.name.clone()], f)?;
                    }
                }
                TableDef::ForeignKey(f) => add(f.cols.clone(), f)?,
                _ => {}
            }
        }
        Ok(out)
    }

    /// Every (non-null) key of `tuples` must be a key of the parent table of `fk`.
    async fn require_parents(&mut self, fk: &FkDef, verb: &str, tuples: Vec<Vec<Value>>) -> Result<(), SqlError> {
        let mut seen = std::collections::HashSet::new();
        let tuples: Vec<Vec<Value>> = tuples.into_iter().filter(|t| t.iter().all(|v| !v.is_null()) && seen.insert(t.iter().map(tsql_server::convert::display).collect::<Vec<_>>())).collect();
        if tuples.is_empty() {
            return Ok(());
        }
        let parent = self.table_by_id(&fk.parent).ok_or_else(|| SqlError::new(208, format!("Invalid object name '{}'.", fk.parent)))?;
        for chunk in tuples.chunks(CHUNK) {
            let found = self.run_query(&select(&parent.name, items_of(&fk.parent_cols), Some(tuple_cond(&fk.parent_cols, chunk)), true)).await?;
            // which of the wanted keys are not there: the control node's engine compares them (so collation applies)
            let wanted = values_query(chunk);
            let have = values_query(&found.rows);
            let missing = if found.rows.is_empty() {
                wanted
            } else {
                simple_query_body(QueryBody::SetOp { op: SetOp::Except, all: false, left: Box::new(wanted.body), right: Box::new(have.body) })
            };
            let res = self.mirror.execute_stmts(&[Stmt::Query(Box::new(missing))]);
            merge::check(&res)?;
            if res.resultsets().first().map(|r| !r.rows.is_empty()).unwrap_or(false) {
                return Err(SqlError::new(
                    547,
                    format!(
                        "The {} statement conflicted with the FOREIGN KEY constraint \"{}\". The conflict occurred in database \"master\", table \"{}\", column '{}'.",
                        verb,
                        fk.name,
                        fk.parent,
                        fk.parent_cols.join(", ")
                    ),
                ));
            }
        }
        Ok(())
    }

    /// An INSERT into a table that has foreign keys: the keys of the new rows must exist.
    pub(crate) async fn fk_check_insert(&mut self, t: &DistTable, names: &[String], rows: &[Vec<Expr>]) -> Result<(), SqlError> {
        let fks = self.cluster.fks_of_child(&t.id);
        for fk in fks {
            let pos: Vec<Option<usize>> = fk.child_cols.iter().map(|c| names.iter().position(|n| n.eq_ignore_ascii_case(c))).collect();
            if pos.iter().any(|p| p.is_none()) {
                continue; // a column that is not given is null (or its default): nothing to look for
            }
            let mut tuples = vec![];
            for r in rows {
                let mut tup = vec![];
                for p in &pos {
                    let b = self.bind_expr(&r[p.expect("checked")])?;
                    tup.push(self.eval_scalar(&b).await?);
                }
                tuples.push(tup);
            }
            if fk.parent == t.id {
                // a row of the same statement may be the parent: the keys of the batch count
                let own: Vec<Vec<Value>> = {
                    let ppos: Vec<Option<usize>> = fk.parent_cols.iter().map(|c| names.iter().position(|n| n.eq_ignore_ascii_case(c))).collect();
                    let mut v = vec![];
                    if ppos.iter().all(|p| p.is_some()) {
                        for r in rows {
                            let mut tup = vec![];
                            for p in &ppos {
                                let b = self.bind_expr(&r[p.expect("checked")])?;
                                tup.push(self.eval_scalar(&b).await?);
                            }
                            v.push(tup);
                        }
                    }
                    v
                };
                let own_keys: std::collections::HashSet<Vec<String>> = own.iter().map(|t| t.iter().map(tsql_server::convert::display).collect()).collect();
                tuples.retain(|t| !own_keys.contains(&t.iter().map(tsql_server::convert::display).collect::<Vec<_>>()));
            }
            self.require_parents(&fk, "INSERT", tuples).await?;
        }
        Ok(())
    }

    /// An UPDATE of a table's columns: the new foreign keys must exist, and the keys other tables refer to must not change
    /// while rows refer to them.
    pub(crate) async fn fk_check_update(&mut self, t: &DistTable, u: &Update) -> Result<(), SqlError> {
        let set_of = |c: &str| -> Option<Expr> {
            u.sets.iter().find_map(|s| match s {
                SetItem::Col { parts, e, .. } if parts.last().map(|p| p.eq_ignore_ascii_case(c)).unwrap_or(false) => Some(e.clone()),
                _ => None,
            })
        };
        // as a child
        for fk in self.cluster.fks_of_child(&t.id) {
            if !fk.child_cols.iter().any(|c| set_of(c).is_some()) {
                continue;
            }
            let items: Vec<SelectItem> = fk.child_cols.iter().map(|c| SelectItem::Expr { e: set_of(c).unwrap_or_else(|| col(c)), alias: None }).collect();
            let rs = self.run_query(&select(&t.name, items, u.where_.clone(), true)).await?;
            self.require_parents(&fk, "UPDATE", rs.rows).await?;
        }
        // as a parent: a key that changes must have no child
        for fk in self.cluster.fks_of_parent(&t.id) {
            if !fk.parent_cols.iter().any(|c| set_of(c).is_some()) {
                continue;
            }
            let mut items = items_of(&fk.parent_cols);
            items.extend(fk.parent_cols.iter().map(|c| SelectItem::Expr { e: set_of(c).unwrap_or_else(|| col(c)), alias: None }));
            let rs = self.run_query(&select(&t.name, items, u.where_.clone(), true)).await?;
            let n = fk.parent_cols.len();
            let changed: Vec<Vec<Value>> = rs.rows.into_iter().filter(|r| (0..n).any(|i| tsql_server::convert::display(&r[i]) != tsql_server::convert::display(&r[n + i]))).map(|r| r[..n].to_vec()).collect();
            self.reject_children(&fk, "UPDATE", changed).await?;
        }
        Ok(())
    }

    /// A foreign key added to a table with rows: every row must already refer to an existing key.
    pub(crate) async fn fk_validate_existing(&mut self, fk: &FkDef) -> Result<(), SqlError> {
        let (Some(child), Some(parent)) = (self.table_by_id(&fk.child), self.table_by_id(&fk.parent)) else { return Ok(()) };
        let on: Option<Expr> = fk
            .child_cols
            .iter()
            .zip(&fk.parent_cols)
            .map(|(c, p)| Expr::Binary(BinOp::Eq, Box::new(Expr::Col(vec!["c".into(), c.clone()])), Box::new(Expr::Col(vec!["p".into(), p.clone()]))))
            .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
        let not_null = fk.child_cols.iter().map(|c| Expr::IsNull(Box::new(Expr::Col(vec!["c".into(), c.clone()])), true)).reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
        let missing = Expr::IsNull(Box::new(Expr::Col(vec!["p".into(), fk.parent_cols[0].clone()])), false);
        let where_ = match not_null {
            Some(n) => Expr::And(Box::new(n), Box::new(missing)),
            None => missing,
        };
        let q = simple_query(Select {
            distinct: false,
            top: Some(Top { e: Expr::Lit(Value::Int(1)), percent: false, ties: false }),
            items: vec![SelectItem::Expr { e: Expr::Col(vec!["c".into(), fk.child_cols[0].clone()]), alias: None }],
            into: None,
            from: vec![TableRef::Join {
                kind: JoinKind::Left,
                left: Box::new(TableRef::Table { name: child.name.clone(), alias: Some("c".into()), hints: vec![] }),
                right: Box::new(TableRef::Table { name: parent.name.clone(), alias: Some("p".into()), hints: vec![] }),
                on,
            }],
            where_: Some(where_),
            group: None,
            having: None,
        });
        if !self.run_query(&q).await?.rows.is_empty() {
            return Err(SqlError::new(547, format!("The ALTER TABLE statement conflicted with the FOREIGN KEY constraint \"{}\". The conflict occurred in database \"master\", table \"{}\", column '{}'.", fk.name, fk.parent, fk.parent_cols.join(", "))));
        }
        Ok(())
    }

    /// NO ACTION: no row of the child table may refer to any of these keys.
    async fn reject_children(&mut self, fk: &FkDef, verb: &str, keys: Vec<Vec<Value>>) -> Result<(), SqlError> {
        let keys: Vec<Vec<Value>> = keys.into_iter().filter(|t| t.iter().all(|v| !v.is_null())).collect();
        let Some(child) = self.table_by_id(&fk.child) else { return Ok(()) };
        for chunk in keys.chunks(CHUNK) {
            let mut q = select(&child.name, items_of(&fk.child_cols), Some(tuple_cond(&fk.child_cols, chunk)), false);
            if let QueryBody::Select(s) = &mut q.body {
                s.top = Some(Top { e: Expr::Lit(Value::Int(1)), percent: false, ties: false });
            }
            if !self.run_query(&q).await?.rows.is_empty() {
                return Err(SqlError::new(
                    547,
                    format!("The {} statement conflicted with the REFERENCE constraint \"{}\". The conflict occurred in database \"master\", table \"{}\", column '{}'.", verb, fk.name, fk.child, fk.child_cols.join(", ")),
                ));
            }
        }
        Ok(())
    }

    /// Before rows of a table are deleted: what refers to them is deleted (CASCADE), set to null (SET NULL), or makes the
    /// delete an error (NO ACTION).
    pub(crate) async fn fk_before_delete(&mut self, t: &DistTable, where_: Option<&Expr>, events: &mut Vec<Event>) -> Result<(), SqlError> {
        let fks: Vec<FkDef> = self.cluster.fks_of_parent(&t.id);
        for fk in fks {
            let rs = self.run_query(&select(&t.name, items_of(&fk.parent_cols), where_.cloned(), true)).await?;
            let keys: Vec<Vec<Value>> = rs.rows.into_iter().filter(|t| t.iter().all(|v| !v.is_null())).collect();
            if keys.is_empty() {
                continue;
            }
            let Some(child) = self.table_by_id(&fk.child) else { continue };
            match fk.on_delete {
                FkAction::NoAction => {
                    // (rows of the same table that go with this delete do not count: a tree is deleted whole)
                    if fk.child == t.id {
                        continue;
                    }
                    self.reject_children(&fk, "DELETE", keys).await?;
                }
                FkAction::Cascade => {
                    for chunk in keys.chunks(CHUNK) {
                        let d = Delete { ctes: vec![], target: Target::Table(child.name.clone()), output: None, from: vec![], where_: Some(tuple_cond(&fk.child_cols, chunk)), current_of: None, top: None };
                        let mut none = vec![];
                        Box::pin(self.delete(&d, &mut none)).await?;
                    }
                }
                FkAction::SetNull => {
                    for chunk in keys.chunks(CHUNK) {
                        let sets = fk.child_cols.iter().map(|c| SetItem::Col { parts: vec![c.clone()], op: None, e: Expr::Lit(Value::Null) }).collect();
                        let u = Update { ctes: vec![], target: Target::Table(child.name.clone()), sets, output: None, from: vec![], where_: Some(tuple_cond(&fk.child_cols, chunk)), current_of: None, top: None };
                        let mut none = vec![];
                        Box::pin(self.update(&u, &mut none)).await?;
                    }
                }
            }
        }
        let _ = events;
        Ok(())
    }
}

fn values_query(rows: &[Vec<Value>]) -> Query {
    let n = rows.first().map(|r| r.len()).unwrap_or(0);
    let cols: Vec<String> = (0..n).map(|i| format!("k{}", i)).collect();
    simple_query(Select {
        distinct: false,
        top: None,
        items: vec![SelectItem::Star(None)],
        into: None,
        from: vec![TableRef::Values { rows: rows.iter().map(|r| r.iter().cloned().map(Expr::Lit).collect()).collect(), alias: Some("v".into()), cols: Some(cols) }],
        where_: None,
        group: None,
        having: None,
    })
}

fn simple_query_body(body: QueryBody) -> Query {
    Query { ctes: vec![], body, order: vec![], offset: None, fetch: None, for_clause: None, maxrecursion: None, options: vec![] }
}

/// A `CREATE TABLE` or `ALTER TABLE` statement without its foreign keys (what the control node's engine and the shards get).
pub fn without_fks(st: &Stmt) -> Stmt {
    match st {
        Stmt::CreateTable { name, defs, versioning } => {
            let defs = defs
                .iter()
                .filter(|d| !matches!(d, TableDef::ForeignKey(_)))
                .map(|d| match d {
                    TableDef::Column(cd) if cd.fk.is_some() => {
                        let mut c = cd.clone();
                        c.fk = None;
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
