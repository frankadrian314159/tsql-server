//! Built-in system stored procedures.
use super::query::Rowset;
use super::*;
use crate::parser;

pub type SysArg = (Option<String>, Value, Option<String>);

impl<'a> Engine<'a> {
    /// Returns Ok(None) if `name` is not a known system procedure.
    pub fn exec_sysproc(&mut self, name: &str, args: &[SysArg]) -> Flow<Option<Value>> {
        match name {
            "sp_executesql" => {
                let mut stmt: Option<String> = None;
                let mut decl: Option<String> = None;
                let mut positional: Vec<&SysArg> = vec![];
                let mut named: Vec<&SysArg> = vec![];
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
                    Some(d) if !d.trim().is_empty() => parser::parse_param_decls(d).map_err(|mut e| {
                        e.abort_batch = false;
                        e
                    })?,
                    _ => vec![],
                };
                let mut frame = Frame::default();
                let mut out_map: Vec<(String, String)> = vec![];
                let rest: Vec<&SysArg> = positional.iter().skip(pi).copied().collect();
                for (i, p) in params.iter().enumerate() {
                    let dt = self.resolve_type(&p.ty)?;
                    let arg = named.iter().find(|a| a.0.as_deref().map(|n| n.eq_ignore_ascii_case(&p.name)).unwrap_or(false)).copied().or_else(|| rest.get(i).copied());
                    let val = match arg {
                        Some(a) => {
                            if let Some(ov) = &a.2 {
                                out_map.push((p.name.clone(), ov.clone()));
                            }
                            convert::cast(&a.1, &dt, None)?
                        }
                        None => Value::Null,
                    };
                    frame.vars.insert(p.name.clone(), Var { ty: dt, val });
                }
                let stmts = parser::parse(&stmt).map_err(|mut e| {
                    e.abort_batch = false;
                    e
                })?;
                self.ses.frames.push(frame);
                self.ses.nest_level += 1;
                self.ses.module_owner.push(String::new());
                self.ses.ret = Value::Null;
                let r = self.exec_stmts(&stmts);
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
                } else {
                    let loc = self.locate(&parts);
                    match &loc {
                        Loc::Db(d, k) => {
                            let db = self.srv.cat_mut().databases.get_mut(d).unwrap();
                            let mut o = db.objects.remove(k).ok_or_else(|| SqlError::new(15225, format!("No item by the name of '{}' could be found in the current database '{}', given that @itemtype was input as '{}'.", objname, self.ses.db, objtype)))?;
                            let schema = o.schema().to_string();
                            match &mut o {
                                Object::Table(t) => t.name = newname.clone(),
                                Object::View(v) => v.name = newname.clone(),
                                Object::Proc(p) => p.name = newname.clone(),
                                Object::Func(f) => f.name = newname.clone(),
                                Object::Seq(s) => s.name = newname.clone(),
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
                let loc = self.locate(&[n.clone()]);
                let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(&[n.clone()]))?;
                let rows: Vec<Row> = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| vec![Value::Str(t.db.clone()), Value::Str(t.schema.clone()), Value::Str(t.name.clone()), Value::Str(c.name.clone()), Value::Str(c.ty.base_name().to_string()), Value::Int(i as i32 + 1), Value::Str(if c.nullable { "YES" } else { "NO" }.into())])
                    .collect();
                self.emit_rowset(Rowset::from_rows(&["TABLE_QUALIFIER".into(), "TABLE_OWNER".into(), "TABLE_NAME".into(), "COLUMN_NAME".into(), "TYPE_NAME".into(), "ORDINAL_POSITION".into(), "IS_NULLABLE".into()], rows));
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
            "sp_addextendedproperty" | "sp_updateextendedproperty" | "sp_dropextendedproperty" | "sp_recompile" | "sp_refreshview" | "sp_updatestats" | "sp_autostats" | "sp_setapprole" | "sp_procoption" | "sp_unbindrule" | "sp_bindrule" => Ok(Some(Value::Int(0))),
            "sp_spaceused" => {
                self.emit_rowset(Rowset::from_rows(&["database_name".into(), "database_size".into()], vec![vec![Value::Str(self.ses.db.clone()), Value::Str("8.00 MB".into())]]));
                Ok(Some(Value::Int(0)))
            }
            _ => Ok(None),
        }
    }
}
