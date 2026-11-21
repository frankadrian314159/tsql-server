//! `EXEC sp_rename 'table.index', 'new', 'INDEX'` and `... 'table.column', 'new', 'COLUMN'` for a table of the cluster. The control node's engine
//! alone would rename it (and the shards would keep the old name, so a later `DROP INDEX` or a query by the column would fail there): the
//! cluster runs it on the control node's engine and on every shard, and logs it. Renaming a table of the cluster is refused (50100): its id, its
//! foreign keys, its triggers and its place in the schema log all carry the name: that is done here too (a table is renamed on the control node's
//! engine and on every shard, and the cluster's records of it are made again under the new id). A view over the table, written with the old name, breaks
//! as it does on a single server.
use super::*;

fn lit_text(e: &Expr) -> Option<String> {
    match e {
        Expr::Lit(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The names in a `sp_rename` object name: `[schema].[table].[column]`.
fn name_parts(s: &str) -> Name {
    s.split('.').map(|p| p.trim().trim_matches(|c| c == '[' || c == ']').to_string()).collect()
}

/// The older system procedures that make users and roles: the cluster logs them with the schema, so that they are there when the control node starts again.
pub(crate) fn is_security_proc(ex: &ExecStmt) -> bool {
    !ex.name_is_var && ex.name.last().is_some_and(|n| matches!(n.to_lowercase().as_str(), "sp_addrole" | "sp_droprole" | "sp_addrolemember" | "sp_droprolemember" | "sp_adduser" | "sp_grantdbaccess" | "sp_dropuser" | "sp_revokedbaccess"))
}

impl ClusterSession {
    /// (objname, newname, objtype) of an `sp_rename` whose arguments are text, in order or by name.
    pub(crate) fn rename_args(ex: &ExecStmt) -> Option<(String, String, String)> {
        if !ex.name.last().is_some_and(|n| n.eq_ignore_ascii_case("sp_rename")) || ex.name_is_var {
            return None;
        }
        let mut get = |pos: usize, name: &str| -> Option<String> {
            ex.args.iter().find(|a| a.name.as_deref().is_some_and(|n| n.trim_start_matches('@').eq_ignore_ascii_case(name))).or_else(|| ex.args.iter().filter(|a| a.name.is_none()).nth(pos)).and_then(|a| lit_text(&a.e))
        };
        Some((get(0, "objname")?, get(1, "newname")?, get(2, "objtype").unwrap_or_default().to_uppercase()))
    }

    /// The table of the cluster an `sp_rename` is about, and the part of the name that is not the table's (the index or the column).
    pub(crate) fn rename_table(&self, objname: &str, objtype: &str) -> Option<(DistTable, Name)> {
        let parts = name_parts(objname);
        let (table, rest): (Name, Name) = if objtype == "COLUMN" || objtype == "INDEX" {
            if parts.len() < 2 {
                return None;
            }
            (parts[..parts.len() - 1].to_vec(), parts[parts.len() - 1..].to_vec())
        } else {
            (parts.clone(), vec![])
        };
        // (the table is one of the database the session is in)
        self.table(&self.qn(&table)).ok().map(|t| (t, rest))
    }

    /// Whether an `EXEC` is an `sp_rename` of something of a table of the cluster.
    pub(crate) fn is_cluster_rename(&self, ex: &ExecStmt) -> bool {
        Self::rename_args(ex).is_some_and(|(o, _, t)| self.rename_table(&o, &t).is_some())
    }

    pub(crate) async fn exec_rename(&mut self, st: &Stmt, ex: &ExecStmt) -> Result<(), SqlError> {
        let Some((objname, _new, objtype)) = Self::rename_args(ex) else { return Err(unsupported("this sp_rename")) };
        let Some((t, _)) = self.rename_table(&objname, &objtype) else { return Err(unsupported("this sp_rename")) };
        if objtype != "INDEX" && objtype != "COLUMN" {
            return self.rename_cluster_table(ex, &t, &_new, &objtype).await;
        }
        self.broadcast(st).await?;
        if objtype == "COLUMN" {
            // the columns as the control node's engine has them now
            let shown = format!("[{}].[{}]", if t.name.len() >= 2 { t.name[t.name.len() - 2].clone() } else { "dbo".to_string() }, t.name.last().cloned().unwrap_or_default());
            if let Some(info) = self.mirror.table_info(&shown) {
                let cols: Vec<ColInfo> = info.columns.iter().map(|c| ColInfo { name: c.name.clone(), computed: c.computed, type_name: c.ty.split('(').next().unwrap_or("").trim().to_lowercase(), hidden: t.graph.is_some() && c.name == "$graph_id" }).collect();
                if let Some(e) = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&t.id) {
                    e.cols = cols;
                    e.version += 1;
                    e.loaded = None;
                    e.rows = None;
                }
            }
        }
        self.refresh_weak_uniques(&t.name);
        self.cluster.save_state();
        self.log_schema(st);
        Ok(())
    }

    /// `EXEC sp_rename 'table', 'new name'`: the table is renamed everywhere, and the cluster's records of it are kept under its new id.
    async fn rename_cluster_table(&mut self, ex: &ExecStmt, t: &DistTable, new: &str, objtype: &str) -> Result<(), SqlError> {
        if !objtype.is_empty() && objtype != "OBJECT" {
            return Err(unsupported(&format!("sp_rename of a {} of a table of the cluster", objtype.to_lowercase())));
        }
        let new = new.trim().trim_matches(|c| c == '[' || c == ']').to_string();
        if new.is_empty() || new.contains('.') {
            return Err(SqlError::new(15225, "The new name of a table must be a name without a schema."));
        }
        let mut renamed = t.name.clone();
        if let Some(l) = renamed.last_mut() {
            *l = new.clone();
        }
        let old_id = t.id.clone();
        let new_id = table_id(&renamed);
        if new_id != old_id && (self.table_by_id(&new_id).is_some() || self.cluster.views.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&new_id)) {
            return Err(SqlError::new(15335, format!("Error: The new name '{}' is already in use as a OBJECT name and will cause a duplicate that is not permitted.", new)));
        }
        // what is logged and run names the table in full, so that it means the same wherever it is read again (a restart, a new shard)
        let shown = t.name.iter().map(|p| format!("[{}]", p)).collect::<Vec<_>>().join(".");
        let full = Stmt::Exec(Box::new(ExecStmt {
            name: ex.name.clone(),
            name_is_var: false,
            args: vec![ExecArg { name: None, e: Expr::Lit(Value::Str(shown)), out: false }, ExecArg { name: None, e: Expr::Lit(Value::Str(new.clone())), out: false }],
            retvar: None,
        }));
        self.broadcast(&full).await?;
        // the cluster's records of the table, under the new id
        {
            let mut tables = self.cluster.tables.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut e) = tables.remove(&old_id) {
                e.id = new_id.clone();
                e.name = renamed.clone();
                e.version += 1;
                e.loaded = None;
                e.rows = None;
                tables.insert(new_id.clone(), e);
            }
        }
        for f in self.cluster.fks.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
            if f.child == old_id {
                f.child = new_id.clone();
            }
            if f.parent == old_id {
                f.parent = new_id.clone();
            }
        }
        for (_, s) in self.cluster.triggers.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
            if let Stmt::CreateTrigger { table, .. } = s {
                if table_id(table) == old_id {
                    *table = renamed.clone();
                }
            }
        }
        self.refresh_weak_uniques(&renamed);
        self.log_schema(&full);
        self.cluster.save_state();
        Ok(())
    }
}
