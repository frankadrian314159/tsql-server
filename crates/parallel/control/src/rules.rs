//! `DEFAULT` and `CHECK` that call a function that reads sharded tables. A shard would evaluate them over its own slice, so the shards (and
//! the control node's engine) are not given them: the control node keeps them as *rules* of the table and applies them itself.
//!
//! * a `CHECK` is verified after the statement that writes the table (an `INSERT` or `UPDATE`), over the rows it wrote, in the statement's
//!   transaction: a row for which the condition is false undoes the statement with error 547, as a shard's own `CHECK` does. (Existing rows
//!   are verified when the constraint is added with `ALTER TABLE`.)
//! * a `DEFAULT` is evaluated when an `INSERT` leaves the column out, or says `DEFAULT` for it: once for the statement.
//!
//! A rule is kept as a hidden trigger of the table (`ck$table$name`, `df$table$column`): that way it is logged with the schema, found
//! again when the control node starts, dropped with the table, and fired by the machinery that fires triggers (see `triggers.rs`), which
//! gives a table that has such rules the limits of a table that has triggers (an `UPDATE` of the shard key is refused).
use super::*;
use std::sync::Arc;

/// A `DEFAULT` or `CHECK` the control node applies.
#[derive(Clone, Debug)]
pub(crate) enum Rule {
    Check { name: String, e: Expr },
    Default { col: String, e: Expr },
}

/// Whether a trigger id is a rule (not listed, enabled or disabled as a trigger).
pub(crate) fn is_rule_id(id: &str) -> bool {
    id.rsplit('.').next().is_some_and(|n| n.starts_with("ck$") || n.starts_with("df$"))
}

impl Cluster {
    /// The `DEFAULT`s of a table that the control node applies: (column, expression).
    pub(crate) fn default_rules(&self, table: &str) -> Vec<(String, Expr)> {
        self.triggers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(id, s)| match s {
                Stmt::CreateTrigger { table: t, body, text, .. } if is_rule_id(id) && id.rsplit('.').next().is_some_and(|n| n.starts_with("df$")) && table_id(t) == table => match body.first() {
                    Some(Stmt::Set { e, .. }) => Some((text.clone(), e.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

impl ClusterSession {
    /// `st` without the `DEFAULT`s and `CHECK`s that call a function that reads sharded tables, and those, as rules.
    pub(crate) fn split_control_rules(&self, st: &Stmt) -> (Stmt, Vec<Rule>) {
        if !self.cluster.has_functions() {
            return (st.clone(), vec![]);
        }
        let reads = |e: &Expr| self.reading_function_tables(e).is_some();
        let mut rules: Vec<Rule> = vec![];
        let mut strip = |defs: &[TableDef], rules: &mut Vec<Rule>| -> Vec<TableDef> {
            let mut out = vec![];
            for d in defs {
                match d {
                    TableDef::Column(cd) => {
                        let mut cd = cd.clone();
                        if let Some(e) = cd.default.clone().filter(|e| reads(e)) {
                            rules.push(Rule::Default { col: cd.name.clone(), e });
                            cd.default = None;
                            cd.default_text = None;
                            cd.default_name = None;
                        }
                        let mut kept = vec![];
                        for (n, e, text) in cd.checks.drain(..) {
                            if reads(&e) {
                                rules.push(Rule::Check { name: n.unwrap_or_default(), e });
                            } else {
                                kept.push((n, e, text));
                            }
                        }
                        cd.checks = kept;
                        out.push(TableDef::Column(cd));
                    }
                    TableDef::Check { name, e, .. } if reads(e) => rules.push(Rule::Check { name: name.clone().unwrap_or_default(), e: e.clone() }),
                    TableDef::DefaultFor { e, col, .. } if reads(e) => rules.push(Rule::Default { col: col.clone(), e: e.clone() }),
                    other => out.push(other.clone()),
                }
            }
            out
        };
        match st {
            Stmt::CreateTable { name, defs, versioning } => {
                let defs = strip(defs, &mut rules);
                (Stmt::CreateTable { name: name.clone(), defs, versioning: versioning.clone() }, rules)
            }
            Stmt::AlterTable { name, action: AlterAction::AddDefs(defs) } => {
                let defs = strip(defs, &mut rules);
                // (nothing left to add: a statement that adds no definition is not sent on)
                (Stmt::AlterTable { name: name.clone(), action: AlterAction::AddDefs(defs) }, rules)
            }
            _ => (st.clone(), rules),
        }
    }

    /// The name a rule is kept under: the table's name with its last part changed.
    fn rule_name(table: &Name, prefix: &str, what: &str) -> Name {
        let mut n = table.clone();
        let last = table.last().cloned().unwrap_or_default();
        if let Some(l) = n.last_mut() {
            *l = format!("{}${}${}", prefix, last, what);
        }
        n
    }

    /// Verify the rows a table has against checks that are being added (an `ALTER TABLE ... ADD CHECK` does).
    pub(crate) async fn verify_control_checks(&mut self, table: &Name, rules: &[Rule]) -> Result<(), SqlError> {
        for r in rules {
            if let Rule::Check { name, e, .. } = r {
                let q = simple_query(Select {
                    distinct: false,
                    top: Some(Top { e: Expr::Lit(Value::Int(1)), percent: false, ties: false }),
                    items: vec![SelectItem::Expr { e: Expr::Lit(Value::Int(1)), alias: None }],
                    into: None,
                    from: vec![TableRef::Table { name: table.clone(), alias: None, hints: vec![] }],
                    where_: Some(Expr::Not(Box::new(e.clone()))),
                    group: None,
                    having: None,
                });
                if !self.run_query(&q).await?.rows.is_empty() {
                    return Err(SqlError::new(547, format!("The ALTER TABLE statement conflicted with the CHECK constraint \"{}\". The conflict occurred in table \"{}\".", name, table.join("."))));
                }
            }
        }
        Ok(())
    }

    /// Keep rules of a table (see the top of this file).
    pub(crate) fn add_control_rules(&mut self, table: &Name, rules: Vec<Rule>) -> Result<(), SqlError> {
        let schema = table.iter().rev().nth(1).cloned().unwrap_or_else(|| "dbo".into());
        let tname = table.last().cloned().unwrap_or_default();
        let mut unnamed = 0;
        for r in rules {
            let (name, events, body, text) = match r {
                Rule::Check { name, e, .. } => {
                    let name = if name.is_empty() {
                        unnamed += 1;
                        format!("CK_{}_{}", tname, unnamed)
                    } else {
                        name
                    };
                    let shown = format!("{}.{}", schema, tname);
                    let msg = |what: &str| Expr::Lit(Value::Str(format!("The {} statement conflicted with the CHECK constraint \"{}\". The conflict occurred in table \"{}\".", what, name, shown)));
                    let throw = |what: &str| Stmt::Throw(Some((Expr::Lit(Value::Int(547)), msg(what), Expr::Lit(Value::Int(1)))));
                    let from = |t: &str| vec![TableRef::Table { name: vec![t.to_string()], alias: None, hints: vec![] }];
                    let select = |t: &str, w: Option<Expr>| simple_query(Select { distinct: false, top: None, items: vec![SelectItem::Expr { e: Expr::Lit(Value::Int(1)), alias: None }], into: None, from: from(t), where_: w, group: None, having: None });
                    // IF EXISTS (SELECT 1 FROM inserted WHERE NOT (condition)) IF EXISTS (SELECT 1 FROM deleted) THROW (update) ELSE THROW (insert)
                    let body = vec![Stmt::If {
                        cond: Expr::Exists(Box::new(select("inserted", Some(Expr::Not(Box::new(e)))))),
                        then: Box::new(Stmt::If { cond: Expr::Exists(Box::new(select("deleted", None))), then: Box::new(throw("UPDATE")), els: Some(Box::new(throw("INSERT"))) }),
                        els: None,
                    }];
                    (Self::rule_name(table, "ck", &name), vec!["INSERT".to_string(), "UPDATE".to_string()], body, String::new())
                }
                Rule::Default { col, e } => (Self::rule_name(table, "df", &col), vec!["DEFAULT".to_string()], vec![Stmt::Set { var: "@default".into(), op: None, e }], col),
            };
            let st = Stmt::CreateTrigger { name: name.clone(), table: table.clone(), events, instead: false, body: Arc::new(body), or_alter: true, text };
            let id = table_id(&name);
            {
                let mut reg = self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner());
                match reg.iter_mut().find(|(i, _)| *i == id) {
                    Some(slot) => slot.1 = st.clone(),
                    None => reg.push((id, st.clone())),
                }
            }
            self.log_schema(&st);
            self.plan.push("DEFAULT / CHECK that reads sharded tables: kept by the control node, which applies it".into());
        }
        Ok(())
    }

    /// The checks of `table` that the control node keeps and that `names` drop: they are dropped; the other names are returned.
    pub(crate) fn take_control_checks(&mut self, table: &Name, names: &[String]) -> Vec<String> {
        let mut rest = vec![];
        for n in names {
            let rule = Self::rule_name(table, "ck", n);
            let id = table_id(&rule);
            let known = self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(i, _)| *i == id);
            if known {
                self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner()).retain(|(i, _)| *i != id);
                self.log_schema(&Stmt::Drop { what: "TRIGGER".into(), names: vec![rule], if_exists: true });
            } else {
                rest.push(n.clone());
            }
        }
        rest
    }
}
