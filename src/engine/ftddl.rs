//! `CREATE | ALTER | DROP FULLTEXT CATALOG` and `FULLTEXT INDEX`.
//!
//! These are declarations. `CONTAINS`, `FREETEXT` and their table functions search the text of the columns by scanning (see
//! fulltext.rs), so a full-text index has no structure of its own and changes no result; it is kept in the catalog (shown by
//! `sys.fulltext_catalogs`, `sys.fulltext_indexes` and `sys.fulltext_index_columns`) so that scripts and backups that create
//! one run, and the checks SQL Server makes on the declaration are made here: the key index, the column types, one index per
//! table, a catalog that exists. A predicate on a table with no full-text index is not an error here (SQL Server gives 7601);
//! it is allowed so that the scan-based search stays usable without the declaration.
use super::*;
use crate::ast::{FtAlter, FullTextStmt};
use crate::catalog::{FtCatalog, FullTextDef};
use crate::persist::LogOp;

impl<'a> Engine<'a> {
    fn save_ft_catalogs(&mut self, dbk: &str) {
        let catalogs = self.srv.databases.get(dbk).map(|d| d.fulltext_catalogs.clone()).unwrap_or_default();
        self.log_nowait(vec![LogOp::DbFtCatalogs { db: dbk.to_string(), catalogs }]);
    }

    fn ft_catalog_missing(name: &str) -> SqlError {
        SqlError::new(7608, format!("Full-text catalog '{}' does not exist.", name))
    }

    fn ft_columns(t: &Table, cols: &[(String, Option<i32>)]) -> Result<Vec<(usize, i32)>> {
        let mut out: Vec<(usize, i32)> = vec![];
        for (c, lang) in cols {
            let i = t.col_index(c).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", c)))?;
            let ty = &t.columns[i].ty;
            if !(ty.is_string() || ty.is_binary() || matches!(ty, DataType::Xml)) {
                return Err(SqlError::new(7653 + 1, format!("Column '{}' of table '{}' cannot be used for full-text search because it is not a character, binary or xml column.", c, t.name)));
            }
            if out.iter().any(|(k, _)| *k == i) {
                return Err(SqlError::new(1909, format!("Cannot use duplicate column names in index. Column name '{}' listed more than once.", c)));
            }
            out.push((i, lang.unwrap_or(0)));
        }
        Ok(out)
    }

    pub fn exec_fulltext(&mut self, st: &FullTextStmt) -> Result<()> {
        let dbk = self.ses.db.to_lowercase();
        match st {
            FullTextStmt::CreateCatalog { name, default, accent_sensitive } => {
                if self.srv.databases[&dbk].fulltext_catalogs.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
                    return Err(SqlError::new(7609, format!("A full-text catalog named '{}' already exists.", name)));
                }
                let db = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                if *default {
                    for c in db.fulltext_catalogs.iter_mut() {
                        c.is_default = false;
                    }
                }
                db.fulltext_catalogs.push(FtCatalog { name: name.clone(), is_default: *default, accent_sensitive: *accent_sensitive });
                self.save_ft_catalogs(&dbk);
            }
            FullTextStmt::AlterCatalog { name, action } => {
                let Some(i) = self.srv.databases[&dbk].fulltext_catalogs.iter().position(|c| c.name.eq_ignore_ascii_case(name)) else {
                    return Err(Self::ft_catalog_missing(name));
                };
                if action == "DEFAULT" {
                    let db = self.srv.cat_mut().databases.get_mut(&dbk).unwrap();
                    for (k, c) in db.fulltext_catalogs.iter_mut().enumerate() {
                        c.is_default = k == i;
                    }
                    self.save_ft_catalogs(&dbk);
                }
                // REBUILD and REORGANIZE: there is no population to redo
            }
            FullTextStmt::DropCatalog { name } => {
                let Some(i) = self.srv.databases[&dbk].fulltext_catalogs.iter().position(|c| c.name.eq_ignore_ascii_case(name)) else {
                    return Err(Self::ft_catalog_missing(name));
                };
                let used = self.srv.databases[&dbk].objects.values().any(|o| matches!(o, Object::Table(t) if t.fulltext.as_ref().is_some_and(|f| f.catalog.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(name)))));
                if used {
                    return Err(SqlError::new(7610, format!("Cannot drop the full-text catalog '{}': full-text indexes are still defined in it. Drop them first.", name)));
                }
                self.srv.cat_mut().databases.get_mut(&dbk).unwrap().fulltext_catalogs.remove(i);
                self.save_ft_catalogs(&dbk);
            }
            FullTextStmt::CreateIndex { table, cols, key_index, catalog, tracking } => {
                let loc = self.locate(table);
                let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(table))?;
                self.check_alter(&loc, table.last().unwrap())?;
                if t.fulltext.is_some() {
                    return Err(SqlError::new(7611, format!("A full-text index for table or indexed view '{}' has already been created.", t.name)));
                }
                let Some(key) = key_index else {
                    return Err(SqlError::new(102, "Incorrect syntax near ';'. A full-text index needs KEY INDEX."));
                };
                // the key: a unique, single-column, non-nullable index
                let ok = t.all_keys().iter().any(|k| k.name.eq_ignore_ascii_case(key) && k.cols.len() == 1 && !t.columns[k.cols[0]].nullable);
                if !ok {
                    return Err(SqlError::new(7653, format!("'{}' is not a valid index to enforce a full-text search key. A full-text search key must be a unique, non-nullable, single-column index.", key)));
                }
                let key_name = t.all_keys().iter().find(|k| k.name.eq_ignore_ascii_case(key)).map(|k| k.name.clone()).unwrap();
                let cols = Self::ft_columns(&t, cols)?;
                if cols.is_empty() {
                    return Err(SqlError::new(102, "Incorrect syntax: a full-text index needs at least one column."));
                }
                let catalog = match catalog {
                    Some(c) => match self.srv.databases[&dbk].fulltext_catalogs.iter().find(|x| x.name.eq_ignore_ascii_case(c)) {
                        Some(x) => Some(x.name.clone()),
                        None => return Err(Self::ft_catalog_missing(c)),
                    },
                    None => self.srv.databases[&dbk].fulltext_catalogs.iter().find(|c| c.is_default).map(|c| c.name.clone()),
                };
                let tracking = tracking.clone().unwrap_or_else(|| "AUTO".into());
                if !["AUTO", "MANUAL", "OFF"].contains(&tracking.as_str()) {
                    return Err(SqlError::syntax(&tracking));
                }
                self.ddl_lock(&loc, &t)?;
                self.undo_table(&loc, &t);
                let mut t = self.take_table(&loc).ok_or_else(|| self.invalid_object(table))?;
                t.fulltext = Some(FullTextDef { catalog, key_index: key_name, cols, tracking, enabled: true });
                self.put_table(&loc, t);
            }
            FullTextStmt::AlterIndex { table, action } => {
                let loc = self.locate(table);
                let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(table))?;
                self.check_alter(&loc, table.last().unwrap())?;
                let Some(def) = t.fulltext.clone() else {
                    return Err(SqlError::new(7612, format!("Table or indexed view '{}' does not have a full-text index.", t.name)));
                };
                let mut def = def;
                match action {
                    FtAlter::Enable => def.enabled = true,
                    FtAlter::Disable => def.enabled = false,
                    FtAlter::SetTracking(m) => {
                        if !["AUTO", "MANUAL", "OFF"].contains(&m.as_str()) {
                            return Err(SqlError::syntax(m));
                        }
                        def.tracking = m.clone();
                    }
                    FtAlter::AddCols(cols) => {
                        for (i, lang) in Self::ft_columns(&t, cols)? {
                            if def.cols.iter().any(|(k, _)| *k == i) {
                                return Err(SqlError::new(7613, format!("Column '{}' is already part of the full-text index of table '{}'.", t.columns[i].name, t.name)));
                            }
                            def.cols.push((i, lang));
                        }
                    }
                    FtAlter::DropCols(cols) => {
                        for c in cols {
                            let i = t.col_index(c).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", c)))?;
                            let Some(p) = def.cols.iter().position(|(k, _)| *k == i) else {
                                return Err(SqlError::new(7614, format!("Column '{}' is not part of the full-text index of table '{}'.", c, t.name)));
                            };
                            def.cols.remove(p);
                        }
                        if def.cols.is_empty() {
                            return Err(SqlError::new(7615, "A full-text index needs at least one column; use DROP FULLTEXT INDEX to remove it."));
                        }
                    }
                    FtAlter::Population => {}
                }
                self.ddl_lock(&loc, &t)?;
                self.undo_table(&loc, &t);
                let mut t = self.take_table(&loc).ok_or_else(|| self.invalid_object(table))?;
                t.fulltext = Some(def);
                self.put_table(&loc, t);
            }
            FullTextStmt::DropIndex { table } => {
                let loc = self.locate(table);
                let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(table))?;
                self.check_alter(&loc, table.last().unwrap())?;
                if t.fulltext.is_none() {
                    return Err(SqlError::new(7612, format!("Table or indexed view '{}' does not have a full-text index.", t.name)));
                }
                self.ddl_lock(&loc, &t)?;
                self.undo_table(&loc, &t);
                let mut t = self.take_table(&loc).ok_or_else(|| self.invalid_object(table))?;
                t.fulltext = None;
                self.put_table(&loc, t);
            }
        }
        Ok(())
    }
}
