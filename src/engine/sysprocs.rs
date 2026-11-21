//! Built-in system stored procedures.
use super::query::Rowset;
use super::*;

pub type SysArg = (Option<String>, Value, Option<String>);

impl<'a> Engine<'a> {
    /// Returns Ok(None) if `name` is not a known system procedure.
    pub fn exec_sysproc(&mut self, name: &str, args: &[SysArg]) -> Flow<Option<Value>> {
        match name {
            // documents for OPENXML (see funcs.rs `openxml`)
            "sp_xml_preparedocument" => {
                let text = args.get(1).filter(|a| !a.1.is_null()).map(|a| convert::to_str(&a.1)).unwrap_or_default();
                if crate::xmlpath::Doc::parse(&text).is_none() {
                    return Err(SqlError::new(6603, "XML parsing error: the document is not well-formed.").into());
                }
                let h = self.ses.xml_next;
                self.ses.xml_next += 1;
                self.ses.xml_docs.insert(h, text);
                if let Some(ov) = args.first().and_then(|a| a.2.clone()) {
                    self.set_var(&ov, Value::Int(h))?;
                }
                Ok(Some(Value::Int(0)))
            }
            "sp_xml_removedocument" => {
                let h = args.first().and_then(|a| a.1.as_i64()).unwrap_or(0) as i32;
                if self.ses.xml_docs.remove(&h).is_none() {
                    return Err(SqlError::new(6602, "The XML document handle is not valid.").into());
                }
                Ok(Some(Value::Int(0)))
            }
            "sp_executesql" => {
                let mut stmt: Option<String> = None;
                let mut decl: Option<String> = None;
                let mut positional: Vec<&SysArg> = vec![];
                let mut named: Vec<&SysArg> = vec![];
                // the table arguments (table-valued parameters), by the position of the argument
                let sys_tables = self.ses.sys_tables.clone();
                let arg_table = |a: &SysArg| -> Option<Table> { args.iter().position(|x| std::ptr::eq(x, a)).and_then(|i| sys_tables.get(i).cloned().flatten()) };
                for a in args {
                    match a.0.as_deref() {
                        Some("@stmt") => stmt = Some(convert::to_str(&a.1)),
                        Some("@params") => decl = Some(convert::to_str(&a.1)),
                        Some(_) => named.push(a),
                        None => positional.push(a),
                    }
                }
                let mut pi = 0;
                if stmt.is_none() {
                    if let Some(a) = positional.get(pi) {
                        stmt = Some(convert::to_str(&a.1));
                        pi += 1;
                    }
                }
                if decl.is_none() {
                    if let Some(a) = positional.get(pi) {
                        if !a.1.is_null() {
                            decl = Some(convert::to_str(&a.1));
                        }
                        pi += 1;
                    }
                }
                let stmt = stmt.ok_or_else(|| SqlError::new(214, "Procedure expects parameter '@statement' of type 'ntext/nchar/nvarchar'."))?;
                let params = match &decl {
                    Some(d) if !d.trim().is_empty() => super::cached_param_decls(d).map_err(|mut e| {
                        e.abort_batch = false;
                        e
                    })?,
                    _ => std::rc::Rc::new(vec![]),
                };
                let mut frame = Frame::default();
                let mut out_map: Vec<(String, String)> = vec![];
                let rest: Vec<&SysArg> = positional.iter().skip(pi).copied().collect();
                for (i, p) in params.iter().enumerate() {
                    if let Some(defs) = self.table_type_defs(&p.ty) {
                        // a table-valued parameter: the rows of the table variable that was passed
                        let arg = named.iter().find(|a| a.0.as_deref().map(|n| n.eq_ignore_ascii_case(&p.name)).unwrap_or(false)).copied().or_else(|| rest.get(i).copied());
                        let mut t = self.build_table(&self.ses.db.clone(), "dbo", &p.name, &defs, true)?;
                        if let Some(src) = arg.and_then(|a| arg_table(a)) {
                            let rows: Vec<Row> = src.data.iter(crate::storage::ReadView::dirty()).into_iter().map(|(_, r)| r).collect();
                            t.load_rows(rows);
                        }
                        frame.tables.insert(p.name.clone(), t);
                        continue;
                    }
                    let dt = self.resolve_type(&p.ty)?;
                    let arg = named.iter().find(|a| a.0.as_deref().map(|n| n.eq_ignore_ascii_case(&p.name)).unwrap_or(false)).copied().or_else(|| rest.get(i).copied());
                    let val = match arg {
                        Some(a) => {
                            if let Some(ov) = &a.2 {
                                out_map.push((p.name.clone(), ov.clone()));
                            }
                            convert::cast(&a.1, &dt, None)?
                        }
                        // a declared parameter that is not supplied takes its default, or it is an error
                        None => match &p.default {
                            Some(e) => {
                                let v = self.ev(e, &super::query::Env::empty())?;
                                convert::cast(&v, &dt, None)?
                            }
                            None => {
                                let shown: Vec<String> = params.iter().map(|q| format!("{} {}", q.name, q.ty.parts.join(".").to_lowercase())).collect();
                                return Err(SqlError::new(8178, format!("The parameterized query '({}){}' expects the parameter '{}', which was not supplied.", shown.join(","), stmt, p.name)).into());
                            }
                        },
                    };
                    frame.vars.insert(p.name.clone(), Var { ty: dt, val });
                }
                let stmts = super::cached_parse(&stmt).map_err(|mut e| {
                    e.abort_batch = false;
                    e
                })?;
                self.ses.frames.push(frame);
                self.ses.nest_level += 1;
                self.ses.module_owner.push(String::new());
                self.ses.ret = Value::Null;
                let scope = self.enter_module();
                let r = self.exec_stmts(&stmts);
                self.leave_module(scope);
                self.ses.module_owner.pop();
                self.ses.nest_level -= 1;
                let callee = self.ses.frames.pop().unwrap();
                // a RETURN <int> in the statement is the procedure's return status
                let mut status = Value::Int(0);
                match r {
                    Ok(()) => {}
                    Err(Interrupt::Return) => {
                        let v = std::mem::replace(&mut self.ses.ret, Value::Null);
                        if !v.is_null() {
                            status = v;
                        }
                    }
                    Err(e) => return Err(e),
                }
                for (pn, cv) in out_map {
                    if let Some(v) = callee.vars.get(&pn) {
                        self.set_var(&cv, v.val.clone())?;
                    }
                }
                Ok(Some(status))
            }
            "sp_rename" => {
                let objname = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let newname = args.get(1).map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let objtype = args.get(2).map(|a| convert::to_str(&a.1).to_uppercase()).unwrap_or_default();
                let parts: Vec<String> = objname.split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                if objtype == "COLUMN" {
                    if parts.len() < 2 {
                        return Err(SqlError::new(15248, "Either the parameter @objname is ambiguous or the claimed @objtype (COLUMN) is wrong.").into());
                    }
                    let tparts = &parts[..parts.len() - 1];
                    let loc = self.locate(tparts);
                    if let Loc::Db(d, k) = &loc {
                        // renaming is DDL: a rollback puts the old column name back
                        self.ensure_tx()?;
                        self.undo_object(d, k);
                    }
                    let mut t = self.take_table(&loc).ok_or_else(|| self.invalid_object(tparts))?;
                    let ci = t.col_index(&parts[parts.len() - 1]);
                    let r = match ci {
                        Some(i) => {
                            t.columns[i].name = newname.clone();
                            Ok(())
                        }
                        None => Err(SqlError::new(15248, format!("Either the parameter @objname is ambiguous or the claimed @objtype (COLUMN) is wrong."))),
                    };
                    self.put_table(&loc, t);
                    r?;
                } else if objtype == "INDEX" {
                    // `sp_rename 'table.index', 'new name', 'INDEX'`
                    if parts.len() < 2 {
                        return Err(SqlError::new(15248, "Either the parameter @objname is ambiguous or the claimed @objtype (INDEX) is wrong.").into());
                    }
                    let tparts = &parts[..parts.len() - 1];
                    let old = parts[parts.len() - 1].clone();
                    let loc = self.locate(tparts);
                    let tt = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(tparts))?;
                    self.check_alter(&loc, tparts.last().unwrap())?;
                    self.ddl_lock(&loc, &tt)?;
                    self.undo_table(&loc, &tt);
                    let mut t = self.take_table(&loc).ok_or_else(|| self.invalid_object(tparts))?;
                    let taken = |t: &crate::catalog::Table| {
                        t.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(&newname)) || t.uniques.iter().any(|k| k.name.eq_ignore_ascii_case(&newname)) || t.pk.as_ref().is_some_and(|k| k.name.eq_ignore_ascii_case(&newname))
                    };
                    let r: Result<()> = if taken(&t) && !old.eq_ignore_ascii_case(&newname) {
                        Err(SqlError::new(15338, format!("Error: The new name '{}' is already in use as a INDEX name and will cause a duplicate that is not permitted.", newname)))
                    } else {
                        let mut found = false;
                        for i in t.indexes.iter_mut().filter(|i| i.name.eq_ignore_ascii_case(&old)) {
                            i.name = newname.clone();
                            found = true;
                        }
                        for k in t.uniques.iter_mut().filter(|k| k.name.eq_ignore_ascii_case(&old)) {
                            k.name = newname.clone();
                            found = true;
                        }
                        if let Some(k) = t.pk.as_mut().filter(|k| k.name.eq_ignore_ascii_case(&old)) {
                            k.name = newname.clone();
                            found = true;
                        }
                        if found {
                            t.sync_indexes();
                            Ok(())
                        } else {
                            Err(SqlError::new(15225, format!("No item by the name of '{}' could be found in the current database '{}', given that @itemtype was input as '{}'.", objname, self.ses.db, objtype)))
                        }
                    };
                    self.put_table(&loc, t);
                    r?;
                } else {
                    let loc = self.locate(&parts);
                    match &loc {
                        Loc::Db(d, k) => {
                            let schema = match self.srv.databases.get(d).and_then(|db| db.objects.get(k)) {
                                Some(o) => o.schema().to_string(),
                                None => return Err(SqlError::new(15225, format!("No item by the name of '{}' could be found in the current database '{}', given that @itemtype was input as '{}'.", objname, self.ses.db, objtype)).into()),
                            };
                            let new_key = obj_key(&schema, &newname);
                            if new_key != *k && self.srv.databases.get(d).is_some_and(|db| db.objects.contains_key(&new_key)) {
                                return Err(SqlError::new(15335, format!("Error: The new name '{}' is already in use as a OBJECT name and will cause a duplicate that is not permitted.", newname)).into());
                            }
                            // renaming is DDL: a rollback restores the old name and removes the new one
                            self.ensure_tx()?;
                            self.undo_object(d, k);
                            self.undo_object(d, &new_key);
                            let db = self.srv.cat_mut().databases.get_mut(d).unwrap();
                            let mut o = db.objects.remove(k).ok_or_else(|| SqlError::new(15225, format!("No item by the name of '{}' could be found in the current database '{}', given that @itemtype was input as '{}'.", objname, self.ses.db, objtype)))?;
                            match &mut o {
                                Object::Table(t) => t.name = newname.clone(),
                                Object::View(v) => v.name = newname.clone(),
                                Object::Proc(p) => p.name = newname.clone(),
                                Object::Func(f) => f.name = newname.clone(),
                                Object::Seq(s) => s.name = newname.clone(),
                                Object::Synonym(s) => s.name = newname.clone(),
                            }
                            db.objects.insert(obj_key(&schema, &newname), o);
                        }
                        _ => {}
                    }
                }
                self.emit(Event::Message("Caution: Changing any part of an object name could break scripts and stored procedures.".into()));
                Ok(Some(Value::Int(0)))
            }
            "sp_helptext" => {
                let n = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let parts: Vec<String> = n.split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                let loc = self.locate(&parts);
                let text = match self.object_ref(&loc) {
                    Some(Object::View(v)) => v.text.clone(),
                    Some(Object::Proc(p)) => p.text.clone(),
                    Some(Object::Func(f)) => f.def.text.clone(),
                    _ => return Err(SqlError::new(15009, format!("The object '{}' does not exist in database '{}' or is invalid for this operation.", n, self.ses.db)).into()),
                };
                let rows: Vec<Row> = text.lines().map(|l| vec![Value::Str(format!("{}\n", l))]).collect();
                self.emit_rowset(Rowset::from_rows(&["Text".to_string()], rows));
                Ok(Some(Value::Int(0)))
            }
            "sp_tables" => {
                let db = self.srv.databases.get(&self.ses.db.to_lowercase()).unwrap();
                let rows: Vec<Row> = db
                    .objects
                    .values()
                    .filter_map(|o| match o {
                        Object::Table(t) => Some(vec![Value::Str(db.name.clone()), Value::Str(t.schema.clone()), Value::Str(t.name.clone()), Value::Str("TABLE".into()), Value::Null]),
                        Object::View(v) => Some(vec![Value::Str(db.name.clone()), Value::Str(v.schema.clone()), Value::Str(v.name.clone()), Value::Str("VIEW".into()), Value::Null]),
                        _ => None,
                    })
                    .collect();
                self.emit_rowset(Rowset::from_rows(&["TABLE_QUALIFIER".into(), "TABLE_OWNER".into(), "TABLE_NAME".into(), "TABLE_TYPE".into(), "REMARKS".into()], rows));
                Ok(Some(Value::Int(0)))
            }
            "sp_columns" => {
                let n = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let parts: Vec<String> = n.split('.').map(|p| p.trim().trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                let loc = self.locate(&parts);
                let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(&parts))?;
                let rows: Vec<Row> = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| vec![Value::Str(t.db.clone()), Value::Str(t.schema.clone()), Value::Str(t.name.clone()), Value::Str(c.name.clone()), Value::Str(c.ty.base_name().to_string()), Value::Int(i as i32 + 1), Value::Str(if c.nullable { "YES" } else { "NO" }.into())])
                    .collect();
                self.emit_rowset(Rowset::from_rows(&["TABLE_QUALIFIER".into(), "TABLE_OWNER".into(), "TABLE_NAME".into(), "COLUMN_NAME".into(), "TYPE_NAME".into(), "ORDINAL_POSITION".into(), "IS_NULLABLE".into()], rows));
                Ok(Some(Value::Int(0)))
            }
            // the foreign keys that refer to a table or that a table has (SSMS asks for them when it opens a table's design)
            "sp_fkeys" => {
                let arg = |pos: usize, nm: &str| -> Option<String> {
                    args.iter().enumerate().find(|(i, a)| a.0.as_deref().map(|n| n.trim_start_matches('@').eq_ignore_ascii_case(nm)).unwrap_or(a.0.is_none() && *i == pos)).and_then(|(_, a)| if a.1.is_null() { None } else { Some(convert::to_str(&a.1)) })
                };
                let (pk_name, pk_owner, fk_name, fk_owner) = (arg(0, "pktable_name"), arg(1, "pktable_owner"), arg(3, "fktable_name"), arg(4, "fktable_owner"));
                if pk_name.is_none() && fk_name.is_none() {
                    return Err(SqlError::new(15250, "Primary or foreign key table name must be given.").into());
                }
                let same = |want: &Option<String>, have: &str| want.as_ref().map(|w| w.eq_ignore_ascii_case(have)).unwrap_or(true);
                let db = self.srv.databases.get(&self.ses.db.to_lowercase()).unwrap();
                let rule = |r: &str| match r.trim().to_uppercase().as_str() {
                    "CASCADE" => 0i32,
                    "SET NULL" => 2,
                    "SET DEFAULT" => 3,
                    _ => 1,
                };
                let mut rows: Vec<Row> = vec![];
                for o in db.objects.values() {
                    let Object::Table(t) = o else { continue };
                    if !same(&fk_name, &t.name) || !same(&fk_owner, &t.schema) {
                        continue;
                    }
                    for f in &t.fks {
                        let Some(Object::Table(p)) = db.objects.get(&f.ref_key) else { continue };
                        if !same(&pk_name, &p.name) || !same(&pk_owner, &p.schema) {
                            continue;
                        }
                        let by_cols = |cols: &Vec<usize>| cols.len() == f.ref_cols.len() && cols.iter().zip(&f.ref_cols).all(|(&c, n)| p.columns.get(c).is_some_and(|x| x.name.eq_ignore_ascii_case(n)));
                        let pk_key = p.pk.as_ref().filter(|k| by_cols(&k.cols)).map(|k| k.name.clone()).or_else(|| p.uniques.iter().find(|u| by_cols(&u.cols)).map(|u| u.name.clone()));
                        for (k, &ci) in f.cols.iter().enumerate() {
                            rows.push(vec![
                                Value::Str(db.name.clone()), Value::Str(p.schema.clone()), Value::Str(p.name.clone()), Value::Str(f.ref_cols.get(k).cloned().unwrap_or_default()),
                                Value::Str(db.name.clone()), Value::Str(t.schema.clone()), Value::Str(t.name.clone()), Value::Str(t.columns[ci].name.clone()),
                                Value::Int(k as i32 + 1), Value::Int(rule(&f.on_update)), Value::Int(rule(&f.on_delete)), Value::Str(f.name.clone()), pk_key.clone().map(Value::Str).unwrap_or(Value::Null), Value::Int(7),
                            ]);
                        }
                    }
                }
                rows.sort_by(|x, y| format!("{:?}{:?}{:?}", x[6], x[5], x[8]).cmp(&format!("{:?}{:?}{:?}", y[6], y[5], y[8])));
                let names: Vec<String> = ["PKTABLE_QUALIFIER", "PKTABLE_OWNER", "PKTABLE_NAME", "PKCOLUMN_NAME", "FKTABLE_QUALIFIER", "FKTABLE_OWNER", "FKTABLE_NAME", "FKCOLUMN_NAME", "KEY_SEQ", "UPDATE_RULE", "DELETE_RULE", "FK_NAME", "PK_NAME", "DEFERRABILITY"].iter().map(|n| n.to_string()).collect();
                let mut rs = Rowset::from_rows(&names, rows);
                for i in [8, 9, 10, 13] {
                    rs.schema.cols[i].ty = Some(DataType::SmallInt);
                }
                self.emit_rowset(rs);
                Ok(Some(Value::Int(0)))
            }
            // the collations of the columns of a table, which bulk copy asks for before it sends rows
            "sp_tablecollations_100" | "sp_tablecollations" => {
                let n = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                // `.[t]` (no schema) and `[s].[t]`
                let parts: Vec<String> = n.split('.').map(|p| p.trim().trim_matches(|c| c == '[' || c == ']').to_string()).filter(|p| !p.is_empty()).collect();
                let loc = self.locate(&parts);
                let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(&parts))?;
                let rows: Vec<Row> = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let text = matches!(c.ty, DataType::Char(_) | DataType::VarChar(_) | DataType::NChar(_) | DataType::NVarChar(_) | DataType::Text | DataType::NText);
                        let coll = c.collation.clone().unwrap_or_else(|| "SQL_Latin1_General_CP1_CI_AS".to_string());
                        vec![Value::Int(i as i32 + 1), Value::Str(c.name.clone()), if text { Value::Bin(vec![0x09, 0x04, 0xD0, 0x00, 0x34]) } else { Value::Null }, if text { Value::Str(coll) } else { Value::Null }]
                    })
                    .collect();
                let mut rs = Rowset::from_rows(&["colid".into(), "name".into(), "tds_collation".into(), "collation_name".into()], rows);
                rs.schema.cols[0].ty = Some(DataType::SmallInt);
                rs.schema.cols[2].ty = Some(DataType::Binary(5));
                self.emit_rowset(rs);
                Ok(Some(Value::Int(0)))
            }
            // what a driver asks of the server before it uses it (ODBC's SQLGetInfo reads sp_server_info)
            "sp_server_info" => {
                let info: [(i32, &str, String); 9] = [
                    (1, "DBMS_NAME", "Microsoft SQL Server".to_string()),
                    (2, "DBMS_VER", funcs::VERSION_STRING.lines().next().unwrap_or("").to_string()),
                    (10, "OWNER_TERM", "owner".to_string()),
                    (11, "TABLE_TERM", "table".to_string()),
                    (12, "MAX_OWNER_NAME_LENGTH", "128".to_string()),
                    (13, "TABLE_LENGTH", "128".to_string()),
                    (14, "MAX_QUAL_LENGTH", "128".to_string()),
                    (17, "QUALIFIER_TERM", "database".to_string()),
                    (500, "SERVER_NAME", self.srv.server_name.clone()),
                ];
                let rows: Vec<Row> = info.iter().map(|(id, n, v)| vec![Value::Int(*id), Value::Str(n.to_string()), Value::Str(v.clone())]).collect();
                self.emit_rowset(Rowset::from_rows(&["attribute_id".into(), "attribute_name".into(), "attribute_value".into()], rows));
                Ok(Some(Value::Int(0)))
            }
            // the folder picker (SMO's Locate Folder / file dialogs) asks the server for its drives and folders
            "xp_fixeddrives" => {
                let rows: Vec<Row> = fixed_drives().into_iter().map(|(d, mb)| vec![Value::Str(d), Value::Int(mb)]).collect();
                self.emit_rowset(Rowset::from_rows(&["drive".into(), "MB free".into()], rows));
                Ok(Some(Value::Int(0)))
            }
            "xp_subdirs" | "xp_dirtree" => {
                let path = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let with_files = name == "xp_dirtree" && args.get(2).map(|a| convert::to_str(&a.1).trim() == "1").unwrap_or(false);
                let mut entries: Vec<(String, bool)> = std::fs::read_dir(&path).map(|rd| rd.flatten().filter_map(|e| { let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false); if is_dir || with_files { Some((e.file_name().to_string_lossy().to_string(), is_dir)) } else { None } }).collect()).unwrap_or_default();
                entries.sort();
                let (cols, rows): (Vec<String>, Vec<Row>) = if name == "xp_subdirs" {
                    (vec!["subdirectory".into()], entries.into_iter().map(|(n, _)| vec![Value::Str(n)]).collect())
                } else {
                    (vec!["subdirectory".into(), "depth".into(), "file".into()], entries.into_iter().map(|(n, d)| vec![Value::Str(n), Value::Int(1), Value::Int(if d { 0 } else { 1 })]).collect())
                };
                self.emit_rowset(Rowset::from_rows(&cols, rows));
                Ok(Some(Value::Int(0)))
            }
            "xp_fileexist" => {
                let path = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let p = std::path::Path::new(&path);
                let (is_dir, exists) = (p.is_dir(), p.exists());
                let parent = p.parent().map(|x| x.as_os_str().is_empty() || x.is_dir()).unwrap_or(true);
                self.emit_rowset(Rowset::from_rows(&["File Exists".into(), "File is a Directory".into(), "Parent Directory Exists".into()], vec![vec![Value::Int((exists && !is_dir) as i32), Value::Int(is_dir as i32), Value::Int(parent as i32)]]));
                Ok(Some(Value::Int(0)))
            }
            // SSMS's Delete dialog clears the backup history of a database it drops; this server keeps none
            "sp_delete_database_backuphistory" => {
                let db = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                self.delete_database_backup_history(&db)?;
                Ok(Some(Value::Int(0)))
            }
            "sp_delete_backuphistory" => {
                let when = args.first().map(|a| a.1.clone()).unwrap_or(Value::Null);
                if let Ok(Value::DateTime(d, _)) = convert::cast(&when, &DataType::DateTime, None) {
                    self.delete_backup_history_before(d)?;
                }
                Ok(Some(Value::Int(0)))
            }
            "xp_msver" => {
                let ver = funcs::VERSION_STRING.lines().next().unwrap_or("").to_string();
                let rows: Vec<Row> = vec![
                    vec![Value::Int(1), Value::Str("ProductName".into()), Value::Null, Value::Str("Microsoft SQL Server".into())],
                    vec![Value::Int(2), Value::Str("ProductVersion".into()), Value::Int(0), Value::Str("16.0.1000.6".into())],
                    vec![Value::Int(3), Value::Str("Language".into()), Value::Int(1033), Value::Str("English (United States)".into())],
                    vec![Value::Int(4), Value::Str("Platform".into()), Value::Null, Value::Str(if cfg!(windows) { "NT x64" } else { "Linux" }.into())],
                    vec![Value::Int(5), Value::Str("Comments".into()), Value::Null, Value::Str(ver)],
                    vec![Value::Int(8), Value::Str("ProcessorCount".into()), Value::Int(std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(1)), Value::Str(std::thread::available_parallelism().map(|n| n.get().to_string()).unwrap_or_else(|_| "1".into()))],
                ];
                self.emit_rowset(Rowset::from_rows(&["Index".into(), "Name".into(), "Internal_Value".into(), "Character_Value".into()], rows));
                Ok(Some(Value::Int(0)))
            }
            "sp_databases" => {
                let rows: Vec<Row> = self.srv.databases.values().map(|d| vec![Value::Str(d.name.clone()), Value::Int(0), Value::Null]).collect();
                self.emit_rowset(Rowset::from_rows(&["DATABASE_NAME".into(), "DATABASE_SIZE".into(), "REMARKS".into()], rows));
                Ok(Some(Value::Int(0)))
            }
            "sp_who" | "sp_who2" => {
                self.sp_who(name == "sp_who2");
                Ok(Some(Value::Int(0)))
            }
            "sp_configure" => {
                self.sp_configure(args)?;
                Ok(Some(Value::Int(0)))
            }
            "sp_help" => {
                let n = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                if n.is_empty() {
                    return self.exec_sysproc("sp_tables", &[]);
                }
                self.exec_sysproc("sp_columns", args)
            }
            "sp_addrolemember" | "sp_droprolemember" => {
                let role = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let member = args.get(1).map(|a| convert::to_str(&a.1)).unwrap_or_default();
                self.exec_security(&crate::ast::SecStmt::AlterRole { role, add: name == "sp_addrolemember", member })?;
                Ok(Some(Value::Int(0)))
            }
            // (the legacy forms of CREATE ROLE and DROP ROLE: Reporting Services makes its role with sp_addrole)
            // The parameters of a procedure or function, as `SqlCommandBuilder.DeriveParameters` asks for them (the Reporting Services portal does,
            // for a function of the database a data source names): one row each, the return value of a procedure first
            "sp_procedure_params_100_managed" | "sp_procedure_params_managed" => {
                let arg = |pos: usize, nm: &str| -> Option<String> {
                    args.iter().enumerate().find(|(i, a)| a.0.as_deref().map(|n| n.trim_start_matches('@').eq_ignore_ascii_case(nm)).unwrap_or(a.0.is_none() && *i == pos)).and_then(|(_, a)| if a.1.is_null() { None } else { Some(convert::to_str(&a.1)) })
                };
                let wanted = arg(0, "procedure_name").unwrap_or_default();
                let mut parts: Vec<String> = wanted.split('.').map(|p| p.trim().trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                if let Some(schema) = arg(2, "procedure_schema") {
                    if parts.len() == 1 {
                        parts.insert(0, schema);
                    }
                }
                let only = arg(3, "parameter_name");
                // (in the database the procedure was named in, if it was)
                let saved_db = self.ses.db.clone();
                if let Some(db) = self.ses.sys_db.clone() {
                    if self.srv.databases.contains_key(&db.to_lowercase()) {
                        self.ses.db = db;
                    }
                }
                let found: Option<(bool, Vec<crate::ast::Param>)> = match self.locate(&parts) {
                    Loc::Db(d, k) => match self.srv.databases.get(&d).and_then(|db| db.objects.get(&k)) {
                        Some(Object::Proc(p)) => Some((true, p.params.clone())),
                        Some(Object::Func(f)) => Some((false, f.def.params.clone())),
                        _ => None,
                    },
                    _ => None,
                };
                let mut rows: Vec<Row> = vec![];
                let mut failure: Option<SqlError> = None;
                if let Some((is_proc, params)) = found {
                    // OLE DB type codes, as the provider's rowset gives them
                    let oledb = |t: &DataType| -> i32 {
                        match t.base_name() {
                            "tinyint" => 17,
                            "smallint" => 2,
                            "int" => 3,
                            "bigint" => 20,
                            "bit" => 11,
                            "decimal" => 131,
                            "money" | "smallmoney" => 6,
                            "float" => 5,
                            "real" => 4,
                            "char" | "varchar" | "text" => 129,
                            "nchar" | "nvarchar" | "ntext" | "xml" => 130,
                            "binary" | "varbinary" | "image" | "timestamp" => 128,
                            "date" => 133,
                            "time" => 134,
                            "datetime" | "smalldatetime" | "datetime2" => 135,
                            "datetimeoffset" => 146,
                            "uniqueidentifier" => 72,
                            _ => 12,
                        }
                    };
                    let mut push = |name: &str, pos: i32, kind: i32, has_default: bool, ty: &DataType| {
                        let (len, prec, scale) = match ty {
                            DataType::Decimal(p, s) => (Value::Null, Value::Int(*p as i32), Value::Int(*s as i32)),
                            DataType::TinyInt => (Value::Null, Value::Int(3), Value::Int(0)),
                            DataType::SmallInt => (Value::Null, Value::Int(5), Value::Int(0)),
                            DataType::Int => (Value::Null, Value::Int(10), Value::Int(0)),
                            DataType::BigInt => (Value::Null, Value::Int(19), Value::Int(0)),
                            other => (other.length().map(|n| Value::Int(if n == crate::types::MAX { i32::MAX } else { n })).unwrap_or(Value::Null), Value::Null, Value::Null),
                        };
                        rows.push(vec![
                            Value::Str(name.to_string()),
                            Value::Int(pos),
                            Value::Int(kind),
                            Value::Bit(has_default),
                            Value::Bit(true),
                            Value::Int(oledb(ty)),
                            len,
                            prec,
                            scale,
                            Value::Null,
                            Value::Null,
                            Value::Str(ty.base_name().to_string()),
                            Value::Null,
                            Value::Null,
                            Value::Null,
                        ]);
                    };
                    if is_proc && only.is_none() {
                        push("@RETURN_VALUE", 0, 4, false, &DataType::Int);
                    }
                    for (i, prm) in params.iter().enumerate() {
                        if only.as_ref().is_some_and(|o| !o.eq_ignore_ascii_case(&prm.name)) {
                            continue;
                        }
                        let name = if prm.name.starts_with('@') { prm.name.clone() } else { format!("@{}", prm.name) };
                        match self.resolve_type(&prm.ty) {
                            Ok(ty) => push(&name, i as i32 + 1, if prm.out { 2 } else { 1 }, prm.default.is_some(), &ty),
                            Err(e) => {
                                failure = Some(e);
                                break;
                            }
                        }
                    }
                }
                self.ses.db = saved_db;
                if let Some(e) = failure {
                    return Err(e.into());
                }
                let cols: Vec<String> = ["PARAMETER_NAME", "ORDINAL_POSITION", "PARAMETER_TYPE", "PARAMETER_HASDEFAULT", "IS_NULLABLE", "DATA_TYPE", "CHARACTER_MAXIMUM_LENGTH", "NUMERIC_PRECISION", "NUMERIC_SCALE", "TYPE_CATALOG_NAME", "TYPE_SCHEMA_NAME", "TYPE_NAME", "XML_CATALOGNAME", "XML_SCHEMANAME", "XML_SCHEMACOLLECTIONNAME"].iter().map(|c| c.to_string()).collect();
                // (the provider casts the type codes, precision and scale to smallint, as SQL Server returns them)
                let mut rs = Rowset::from_rows(&cols, rows);
                for i in [2usize, 3, 4, 5, 7, 8] {
                    rs.schema.cols[i].ty = Some(DataType::SmallInt);
                }
                self.emit_rowset(rs);
                Ok(Some(Value::Int(0)))
            }
            "sp_addrole" => {
                let role = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                self.exec_security(&crate::ast::SecStmt::CreateRole { name: role })?;
                Ok(Some(Value::Int(0)))
            }
            "sp_droprole" => {
                let role = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                self.drop_object("ROLE", &[role], false)?;
                Ok(Some(Value::Int(0)))
            }
            "sp_addsrvrolemember" | "sp_dropsrvrolemember" => {
                let member = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let role = args.get(1).map(|a| convert::to_str(&a.1)).unwrap_or_default();
                self.exec_security(&crate::ast::SecStmt::AlterServerRole { role, add: name == "sp_addsrvrolemember", member })?;
                Ok(Some(Value::Int(0)))
            }
            "sp_addlogin" => {
                let n = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let pw = args.get(1).map(|a| convert::to_str(&a.1));
                self.exec_security(&crate::ast::SecStmt::CreateLogin { name: n, password: pw, default_db: None, opts: Default::default() })?;
                Ok(Some(Value::Int(0)))
            }
            "sp_adduser" | "sp_grantdbaccess" => {
                let login = args.first().map(|a| convert::to_str(&a.1)).unwrap_or_default();
                let user = args.get(1).map(|a| convert::to_str(&a.1)).unwrap_or_else(|| login.clone());
                self.exec_security(&crate::ast::SecStmt::CreateUser { name: user, login: Some(login), default_schema: None })?;
                Ok(Some(Value::Int(0)))
            }
            // the registry and the installation of Windows: this server has neither, so a value that is asked for stays NULL (SSMS reads
            // the paths and settings of the instance with these when it connects)
            "xp_instance_regread" | "xp_regread" | "xp_instance_regenumvalues" | "xp_regenumvalues" | "xp_instance_regenumkeys" | "xp_regenumkeys" | "xp_qv" => Ok(Some(Value::Int(0))),
            "sp_tableoption" | "sp_indexoption" | "sp_dboption" | "sp_addextendedproperty" | "sp_updateextendedproperty" | "sp_dropextendedproperty" | "sp_recompile" | "sp_refreshview" | "sp_updatestats" | "sp_autostats" | "sp_setapprole" | "sp_procoption" | "sp_unbindrule" | "sp_bindrule" => Ok(Some(Value::Int(0))),
            "sp_spaceused" => {
                self.emit_rowset(Rowset::from_rows(&["database_name".into(), "database_size".into()], vec![vec![Value::Str(self.ses.db.clone()), Value::Str("8.00 MB".into())]]));
                Ok(Some(Value::Int(0)))
            }
            _ => Ok(None),
        }
    }
}

/// The drives the server can see with their free space in MB (Windows letters; on other systems the root).
pub(crate) fn fixed_drives() -> Vec<(String, i32)> {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        }
        let mut v = vec![];
        for c in b'A'..=b'Z' {
            let root: Vec<u16> = format!("{}:\\", c as char).encode_utf16().chain(std::iter::once(0)).collect();
            let (mut a, mut t, mut f) = (0u64, 0u64, 0u64);
            if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut a, &mut t, &mut f) } != 0 {
                v.push(((c as char).to_string(), (a / (1024 * 1024)).min(i32::MAX as u64) as i32));
            }
        }
        v
    }
    #[cfg(not(windows))]
    {
        vec![("/".to_string(), 0)]
    }
}
