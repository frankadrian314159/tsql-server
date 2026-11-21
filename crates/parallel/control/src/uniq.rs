//! Unique keys (a `PRIMARY KEY`, a `UNIQUE` constraint, a `UNIQUE` index) that do not contain the shard key. A shard can only keep such a key
//! unique among its own rows, so the control node checks it across the shards:
//!
//! * an `INSERT` looks for the new values among the rows the shards hold (and among the new rows themselves);
//! * an `UPDATE` of a column of the key reads the new values the rows would have (the engine evaluates the `SET`), looks for them among the
//!   rows the statement does not change, and among themselves;
//! * `CREATE UNIQUE INDEX` and `ALTER TABLE ... ADD UNIQUE` verify the rows that are there.
//!
//! Two sessions could each find a value free and write it on different shards, so a session takes a lock of the table's unique keys (the
//! cluster keeps one per table) before it looks, and holds it until its transaction ends (until the statement ends when it has none): the
//! check of the next session then sees what the first wrote. A session that waits more than `LOCK_WAIT` for it gets error 1222.
//!
//! The shards keep their own unique index too (it is what the shard's part of the check rests on, and what a lookup of a key can use). The keys
//! a table has are read from the control node's engine after every change of the table (`refresh_weak_uniques`), so the names of constraints
//! the engine made up are known. Not checked: a filtered unique index (refused when it is made), a key column an `INSERT` leaves out or gives
//! `DEFAULT` (the value is not known here), and rows put in by a restore (they come from a table that was unique as a whole).
use super::*;
use std::time::Duration;

/// How long a session waits for the lock of a table's unique keys.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// How many values are looked for in one query.
const CHUNK: usize = 200;
/// The most rows an `UPDATE` of a unique key reads to check it.
const UPDATE_LIMIT: usize = 200_000;

/// A unique key of a sharded table that does not contain the shard key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WeakUnique {
    pub name: String,
    pub cols: Vec<String>,
    pub primary: bool,
    /// declared as a constraint (not made with `CREATE UNIQUE INDEX`), which decides the error
    pub constraint: bool,
}

/// Values compared as a unique index compares them: NULLs equal, character values without case (the default collation) or trailing spaces.
fn norm(v: &Value) -> String {
    if v.is_null() {
        "\u{1}null".to_string()
    } else {
        tsql_server::convert::display(v).trim_end().to_lowercase()
    }
}

fn tuple_key(vals: &[Value]) -> String {
    vals.iter().map(norm).collect::<Vec<_>>().join("\u{2}")
}

/// `c1 = v1 AND c2 = v2` (a NULL is `IS NULL`).
pub(crate) fn tuple_cond(cols: &[String], vals: &[Value]) -> Expr {
    let mut out: Option<Expr> = None;
    for (c, v) in cols.iter().zip(vals) {
        let one = if v.is_null() { Expr::IsNull(Box::new(col(c)), false) } else { Expr::Binary(BinOp::Eq, Box::new(col(c)), Box::new(nested::lit(v))) };
        out = Some(match out {
            None => one,
            Some(a) => Expr::And(Box::new(a), Box::new(one)),
        });
    }
    out.unwrap_or(Expr::Lit(Value::Int(1)))
}

pub(crate) fn any_tuple(cols: &[String], tuples: &[Vec<Value>]) -> Expr {
    let mut out: Option<Expr> = None;
    for t in tuples {
        let c = Expr::Paren(Box::new(tuple_cond(cols, t)));
        out = Some(match out {
            None => c,
            Some(a) => Expr::Or(Box::new(a), Box::new(c)),
        });
    }
    out.unwrap_or(Expr::Lit(Value::Int(0)))
}

fn shown(vals: &[Value]) -> String {
    vals.iter().map(|v| if v.is_null() { "<NULL>".to_string() } else { tsql_server::convert::display(v) }).collect::<Vec<_>>().join(", ")
}

impl Cluster {
    /// The lock of the unique keys of a table.
    fn uniq_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.uniq_locks.lock().unwrap_or_else(|e| e.into_inner()).entry(id.to_string()).or_default().clone()
    }
}

impl ClusterSession {
    /// The error of a duplicate key, as the engine words it.
    fn duplicate(t: &DistTable, wu: &WeakUnique, vals: &[Value]) -> SqlError {
        let shown_name = {
            let n = &t.name;
            let schema = if n.len() >= 2 { n[n.len() - 2].clone() } else { "dbo".to_string() };
            format!("{}.{}", schema, n.last().cloned().unwrap_or_default())
        };
        if wu.constraint {
            SqlError::new(2627, format!("Violation of {} constraint '{}'. Cannot insert duplicate key in object '{}'. The duplicate key value is ({}).", if wu.primary { "PRIMARY KEY" } else { "UNIQUE KEY" }, wu.name, shown_name, shown(vals)))
        } else {
            SqlError::new(2601, format!("Cannot insert duplicate key row in object '{}' with unique index '{}'. The duplicate key value is ({}).", shown_name, wu.name, shown(vals)))
        }
    }

    /// Take the lock of the unique keys of a table, for the rest of the transaction (the statement, without one).
    pub(crate) async fn lock_unique(&mut self, id: &str) -> Result<(), SqlError> {
        if self.uniq_guards.iter().any(|(i, _)| i == id) {
            return Ok(());
        }
        let m = self.cluster.uniq_lock(id);
        match tokio::time::timeout(LOCK_WAIT, m.lock_owned()).await {
            Ok(g) => {
                self.uniq_guards.push((id.to_string(), g));
                Ok(())
            }
            Err(_) => Err(SqlError::new(1222, "Lock request time out period exceeded: another session holds the unique keys of this table.")),
        }
    }

    /// The first row of `t` that has one of `tuples` as the value of `cols` (and satisfies `also`), if any.
    async fn find_tuple(&mut self, t: &DistTable, cols: &[String], tuples: &[Vec<Value>], also: Option<Expr>) -> Result<Option<Vec<Value>>, SqlError> {
        for chunk in tuples.chunks(CHUNK) {
            let mut w = any_tuple(cols, chunk);
            if let Some(a) = &also {
                w = Expr::And(Box::new(Expr::Paren(Box::new(w))), Box::new(a.clone()));
            }
            let q = simple_query(Select {
                distinct: false,
                top: Some(Top { e: Expr::Lit(Value::Int(1)), percent: false, ties: false }),
                items: cols.iter().map(|c| SelectItem::Expr { e: col(c), alias: None }).collect(),
                into: None,
                from: vec![TableRef::Table { name: t.name.clone(), alias: None, hints: vec![] }],
                where_: Some(w),
                group: None,
                having: None,
            });
            let rs = self.run_query(&q).await?;
            if let Some(r) = rs.rows.into_iter().next() {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// Before an `INSERT` of `rows` (the values of `names`).
    pub(crate) async fn check_unique_insert(&mut self, t: &DistTable, names: &[String], rows: &[Vec<Expr>]) -> Result<(), SqlError> {
        if !matches!(t.dist, Dist::Sharded { .. }) {
            return Ok(());
        }
        for wu in &t.weak_uniques {
            let pos: Vec<Option<usize>> = wu.cols.iter().map(|c| names.iter().position(|n| n.eq_ignore_ascii_case(c))).collect();
            if pos.iter().any(|p| p.is_none()) {
                continue;
            }
            let mut tuples: Vec<Vec<Value>> = vec![];
            let mut seen: std::collections::HashSet<String> = Default::default();
            for r in rows {
                if pos.iter().any(|p| matches!(r[p.unwrap()], Expr::Default)) {
                    continue;
                }
                let mut vals = vec![];
                for p in &pos {
                    let b = self.bind_expr(&r[p.unwrap()])?;
                    vals.push(self.eval_scalar(&b).await?);
                }
                if !seen.insert(tuple_key(&vals)) {
                    return Err(Self::duplicate(t, wu, &vals));
                }
                tuples.push(vals);
            }
            if tuples.is_empty() {
                continue;
            }
            self.lock_unique(&t.id).await?;
            if let Some(found) = self.find_tuple(t, &wu.cols, &tuples, None).await? {
                self.plan.push(format!("unique key {} checked across the shards", wu.name));
                return Err(Self::duplicate(t, wu, &found));
            }
            self.plan.push(format!("unique key {} checked across the shards", wu.name));
        }
        Ok(())
    }

    /// Before an `UPDATE` that sets a column of a unique key (one that moves the shard key is checked by the `INSERT` it becomes).
    pub(crate) async fn check_unique_update(&mut self, t: &DistTable, u: &Update) -> Result<(), SqlError> {
        if !matches!(t.dist, Dist::Sharded { .. }) {
            return Ok(());
        }
        let set_cols: Vec<String> = u.sets.iter().filter_map(|s| if let SetItem::Col { parts, .. } = s { parts.last().cloned() } else { None }).collect();
        for wu in &t.weak_uniques {
            if !wu.cols.iter().any(|c| set_cols.iter().any(|s| s.eq_ignore_ascii_case(c))) {
                continue;
            }
            self.lock_unique(&t.id).await?;
            // the values the rows would have
            let items: Vec<SelectItem> = wu
                .cols
                .iter()
                .map(|c| {
                    let set = u.sets.iter().find_map(|s| match s {
                        SetItem::Col { parts, e, .. } if parts.last().is_some_and(|p| p.eq_ignore_ascii_case(c)) => Some(e.clone()),
                        _ => None,
                    });
                    SelectItem::Expr { e: set.unwrap_or_else(|| col(c)), alias: Some(c.clone()) }
                })
                .collect();
            let alias = match &u.target {
                Target::Table(n) => n.last().cloned().unwrap_or_default(),
                _ => String::new(),
            };
            let read = simple_query(Select { distinct: false, top: None, items, into: None, from: vec![TableRef::Table { name: t.name.clone(), alias: Some(alias.clone()), hints: vec![] }], where_: u.where_.clone(), group: None, having: None });
            let rs = self.run_query(&read).await?;
            if rs.rows.len() > UPDATE_LIMIT {
                return Err(SqlError::new(50100, format!("This UPDATE changes a unique key of {} in more than {} rows: the control node cannot check it across the shards.", t.name.join("."), UPDATE_LIMIT)));
            }
            let mut seen: std::collections::HashSet<String> = Default::default();
            for r in &rs.rows {
                if !seen.insert(tuple_key(r)) {
                    return Err(Self::duplicate(t, wu, r));
                }
            }
            // ... against the rows the statement leaves as they are
            if let Some(w) = &u.where_ {
                let outside = Expr::Binary(
                    BinOp::Eq,
                    Box::new(Expr::Case { operand: None, whens: vec![(w.clone(), Expr::Lit(Value::Int(0)))], els: Some(Box::new(Expr::Lit(Value::Int(1)))) }),
                    Box::new(Expr::Lit(Value::Int(1))),
                );
                let tuples: Vec<Vec<Value>> = rs.rows.clone();
                for chunk in tuples.chunks(CHUNK) {
                    let q = simple_query(Select {
                        distinct: false,
                        top: Some(Top { e: Expr::Lit(Value::Int(1)), percent: false, ties: false }),
                        items: wu.cols.iter().map(|c| SelectItem::Expr { e: col(c), alias: None }).collect(),
                        into: None,
                        from: vec![TableRef::Table { name: t.name.clone(), alias: Some(alias.clone()), hints: vec![] }],
                        where_: Some(Expr::And(Box::new(Expr::Paren(Box::new(any_tuple(&wu.cols, chunk)))), Box::new(outside.clone()))),
                        group: None,
                        having: None,
                    });
                    if let Some(found) = self.run_query(&q).await?.rows.into_iter().next() {
                        return Err(Self::duplicate(t, wu, &found));
                    }
                }
            }
            self.plan.push(format!("unique key {} checked across the shards", wu.name));
        }
        Ok(())
    }

    /// The rows `table` has, for a unique key being added over `cols`: a duplicate is error 1505 (2714 for a constraint's table).
    pub(crate) async fn verify_unique_existing(&mut self, table: &Name, cols: &[String], name: &str) -> Result<(), SqlError> {
        let items: Vec<SelectItem> = cols.iter().map(|c| SelectItem::Expr { e: col(c), alias: None }).collect();
        let q = simple_query(Select {
            distinct: false,
            top: Some(Top { e: Expr::Lit(Value::Int(1)), percent: false, ties: false }),
            items,
            into: None,
            from: vec![TableRef::Table { name: table.clone(), alias: None, hints: vec![] }],
            where_: None,
            group: Some(cols.iter().map(|c| GroupItem::Expr(col(c))).collect()),
            having: Some(Expr::Binary(BinOp::Gt, Box::new(Expr::Func(Box::new(FuncCall { name: "COUNT".into(), schema: None, parts: vec![], args: vec![], distinct: false, star: true, over: None, within: None }))), Box::new(Expr::Lit(Value::Int(1))))),
        });
        if let Some(r) = self.run_query(&q).await?.rows.into_iter().next() {
            return Err(SqlError::new(1505, format!("The CREATE UNIQUE INDEX statement terminated because a duplicate key was found for the object name '{}' and the index name '{}'. The duplicate key value is ({}).", table.join("."), name, shown(&r))));
        }
        Ok(())
    }

    /// Read the unique keys of `name` (those without the shard key) from the control node's engine, after the table or its indexes changed.
    pub(crate) fn refresh_weak_uniques(&mut self, name: &Name) {
        let Ok(t) = self.table(name) else { return };
        let Dist::Sharded { key, .. } = &t.dist else { return };
        let key_name = t.cols[*key].name.clone();
        let schema = if name.len() >= 2 { name[name.len() - 2].clone() } else { "dbo".to_string() };
        let shown = format!("[{}].[{}]", schema, name.last().cloned().unwrap_or_default());
        let weak: Vec<WeakUnique> = self
            .mirror
            .table_info(&shown)
            .map(|ti| {
                ti.indexes
                    .into_iter()
                    .filter(|i| (i.unique || i.primary_key) && i.filter.is_none() && !i.columns.iter().any(|(c, _)| c.eq_ignore_ascii_case(&key_name)))
                    .map(|i| WeakUnique { name: i.name, cols: i.columns.into_iter().map(|(c, _)| c).collect(), primary: i.primary_key, constraint: i.constraint || i.primary_key })
                    .collect()
            })
            .unwrap_or_default();
        if weak != t.weak_uniques {
            if let Some(x) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&t.id) {
                x.weak_uniques = weak;
            }
            self.cluster.save_state();
        }
    }
}
