//! CREATE / ALTER / DROP.
use super::query::Env;
use super::*;
use crate::convert::key_of;
use crate::storage::lock::Mode;
use crate::storage::tx::Undo;
use std::collections::HashSet;

impl<'a> Engine<'a> {
    fn const_i64(&mut self, e: &Expr) -> Result<i64> {
        let v = self.ev(e, &Env::empty())?;
        v.as_i64().ok_or_else(|| SqlError::new(8115, "Arithmetic overflow error converting expression to data type bigint."))
    }

    /// Build a table from column / constraint definitions.
    pub fn build_table(&mut self, db: &str, schema: &str, name: &str, defs: &[TableDef], is_temp: bool) -> Result<Table> {
        let oid = self.srv.new_object_id();
        let mut cols: Vec<Column> = vec![];
        for d in defs {
            if let TableDef::Column(c) = d {
                if cols.iter().any(|x| x.name.eq_ignore_ascii_case(&c.name)) {
                    return Err(SqlError::new(2705, format!("Column names in each table must be unique. Column name '{}' in table '{}' is specified more than once.", c.name, name)));
                }
                let (ty, user_type) = match &c.ty {
                    Some(tr) => {
                        let ut = if tr.parts.len() == 1 && DataType::from_name(&tr.parts[0], &tr.args, false).is_err() { Some(tr.parts[0].clone()) } else { None };
                        (self.resolve_type(tr)?, ut)
                    }
                    None => (DataType::Variant, None),
                };
                let identity = match &c.identity {
                    Some((s, i)) => {
                        let sv = self.const_i64(s)?;
                        let iv = self.const_i64(i)?;
                        if iv == 0 {
                            return Err(SqlError::new(2744, "Identity increment cannot be zero."));
                        }
                        Some((sv, iv))
                    }
                    None => None,
                };
                // a column takes the nullability of its alias type (CREATE TYPE ... NOT NULL) unless it says otherwise
                let alias_nullable = c.ty.as_ref().and_then(|tr| {
                    let (sch, nm) = if tr.parts.len() == 1 { ("dbo".to_string(), tr.parts[0].clone()) } else { (tr.parts[tr.parts.len() - 2].clone(), tr.parts[tr.parts.len() - 1].clone()) };
                    self.srv.databases.get(&db.to_lowercase()).and_then(|d| d.types.get(&obj_key(&sch, &nm))).and_then(|u| if let UserTypeKind::Alias(_, n) = &u.kind { Some(*n) } else { None })
                });
                let mut nullable = c.nullable.or(alias_nullable).unwrap_or(true);
                if c.pk.is_some() || identity.is_some() {
                    nullable = false; // PRIMARY KEY and IDENTITY columns are never nullable
                }
                if matches!(ty, DataType::Timestamp) {
                    nullable = false;
                }
                let collation = self.column_collation(&c.collation, &ty)?;
                let mask = self.validated_mask(&c.mask, &ty, &c.name)?;
                let nullable = if c.generated.is_some() { false } else { nullable };
                cols.push(Column {
                    name: c.name.clone(),
                    ty,
                    nullable,
                    identity,
                    default: c.default.clone().map(Arc::new),
                    default_text: c.default_text.clone(),
                    computed_text: c.computed_text.clone(),
                    default_name: c.default.as_ref().map(|_| c.default_name.clone().unwrap_or_else(|| format!("DF__{}__{}__{:08X}", name, c.name, oid))),
                    computed: c.computed.clone().map(Arc::new),
                    user_type,
                    collation,
                    mask,
                    hidden: false,
                });
            }
        }
        let store = if is_temp { self.srv.temp_store.clone() } else { self.srv.store.clone() };
        let mut t = Table::new(db, schema, name, cols, &store);
        t.object_id = oid;
        t.is_temp = is_temp;
        self.define_period(&mut t, defs)?;
        let idx_of = |t: &Table, n: &str| -> Result<usize> { t.col_index(n).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", n))) };
        // constraints
        for d in defs {
            match d {
                TableDef::Column(c) => {
                    let ci = t.col_index(&c.name).unwrap();
                    if let Some(n) = &c.pk {
                        self.add_pk(&mut t, n.clone(), vec![ci])?;
                    }
                    if let Some(n) = &c.unique {
                        let kn = n.clone().unwrap_or_else(|| format!("UQ__{}__{:08X}", name, self.srv.new_object_id()));
                        t.uniques.push(KeyDef { name: kn, cols: vec![ci], is_index: false, clustered: false, include: vec![], filter: None });
                    }
                    for (n, e, ctext) in &c.checks {
                        let cn = n.clone().unwrap_or_else(|| format!("CK__{}__{}__{:08X}", name, c.name, self.srv.new_object_id()));
                        t.checks.push(CheckDef { name: cn, expr: Arc::new(e.clone()), text: ctext.clone(), enabled: true, col: Some(c.name.clone()) });
                    }
                }
                TableDef::PrimaryKey { name: n, cols: kc } => {
                    let idx: Vec<usize> = kc.iter().map(|(c, _)| idx_of(&t, c)).collect::<Result<_>>()?;
                    self.add_pk(&mut t, n.clone(), idx)?;
                }
                TableDef::Unique { name: n, cols: kc } => {
                    let idx: Vec<usize> = kc.iter().map(|(c, _)| idx_of(&t, c)).collect::<Result<_>>()?;
                    let kn = n.clone().unwrap_or_else(|| format!("UQ__{}__{:08X}", name, self.srv.new_object_id()));
                    t.uniques.push(KeyDef { name: kn, cols: idx, is_index: false, clustered: false, include: vec![], filter: None });
                }
                TableDef::Check { name: n, e, text: ctext } => {
                    let cn = n.clone().unwrap_or_else(|| format!("CK__{}__{:08X}", name, self.srv.new_object_id()));
                    t.checks.push(CheckDef { name: cn, expr: Arc::new(e.clone()), text: ctext.clone(), enabled: true, col: None });
                }
                TableDef::DefaultFor { name: n, e, col, text: dtext } => {
                    let ci = idx_of(&t, col)?;
                    t.columns[ci].default = Some(Arc::new(e.clone()));
                    t.columns[ci].default_text = Some(dtext.clone());
                    t.columns[ci].default_name = Some(n.clone());
                }
                TableDef::Index { name: n, cols: kc, unique } => {
                    let idx: Vec<(usize, bool)> = kc.iter().map(|(c, d)| idx_of(&t, c).map(|i| (i, *d))).collect::<Result<_>>()?;
                    t.indexes.push(IndexDef { name: n.clone(), cols: idx, unique: *unique, include: vec![], filter: None });
                }
                TableDef::ForeignKey(_) | TableDef::Period { .. } => {}
            }
        }
        // foreign keys last (self references need the PK)
        for d in defs {
            let fk = match d {
                TableDef::ForeignKey(f) => Some(f.clone()),
                TableDef::Column(c) => c.fk.clone().map(|mut f| {
                    f.cols = vec![c.name.clone()];
                    f
                }),
                _ => None,
            };
            if let Some(f) = fk {
                self.add_fk(&mut t, &f)?;
            }
        }
        t.sync_indexes();
        Ok(t)
    }

    fn add_pk(&mut self, t: &mut Table, name: Option<String>, idx: Vec<usize>) -> Result<()> {
        if t.pk.is_some() {
            return Err(SqlError::new(8110, format!("Cannot add multiple PRIMARY KEY constraints to table '{}'.", t.name)));
        }
        for &i in &idx {
            t.columns[i].nullable = false;
        }
        let kn = name.unwrap_or_else(|| format!("PK__{}__{:08X}", t.name, self.srv.new_object_id()));
        t.pk = Some(KeyDef { name: kn, cols: idx, is_index: false, clustered: true, include: vec![], filter: None });
        Ok(())
    }

    fn add_fk(&mut self, t: &mut Table, f: &ForeignKeyDef) -> Result<()> {
        let cols: Vec<usize> = f.cols.iter().map(|c| t.col_index(c).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", c)))).collect::<Result<_>>()?;
        let ploc = self.locate(&f.table);
        let (ref_db, ref_key) = match &ploc {
            Loc::Db(d, k) => (d.clone(), k.clone()),
            _ => return Err(SqlError::new(1767, format!("Foreign key '{}' references invalid table '{}'.", f.name.clone().unwrap_or_default(), f.table.join(".")))),
        };
        let self_ref = ref_key == t.key() && ref_db.eq_ignore_ascii_case(&t.db);
        let ref_cols: Vec<String> = match &f.rcols {
            Some(c) => c.clone(),
            None => {
                let pk = if self_ref {
                    t.pk.clone()
                } else {
                    match self.srv.databases.get(&ref_db).and_then(|d| d.objects.get(&ref_key)) {
                        Some(Object::Table(p)) => p.pk.clone().map(|k| KeyDef { name: k.name, cols: k.cols, is_index: false, clustered: true, include: vec![], filter: None }).map(|k| {
                            let _ = &p;
                            k
                        }),
                        _ => None,
                    }
                };
                match pk {
                    Some(k) => {
                        let tab: &Table = if self_ref {
                            t
                        } else {
                            match self.srv.databases.get(&ref_db).and_then(|d| d.objects.get(&ref_key)) {
                                Some(Object::Table(p)) => p,
                                _ => unreachable!(),
                            }
                        };
                        k.cols.iter().map(|&i| tab.columns[i].name.clone()).collect()
                    }
                    None => return Err(SqlError::new(1773, format!("Foreign key '{}' has implicit reference to object '{}' which does not have a primary key defined on it.", f.name.clone().unwrap_or_default(), f.table.join(".")))),
                }
            }
        };
        // verify the referenced columns are a key
        {
            let tab: Option<&Table> = if self_ref {
                Some(&*t)
            } else {
                match self.srv.databases.get(&ref_db).and_then(|d| d.objects.get(&ref_key)) {
                    Some(Object::Table(p)) => Some(p),
                    _ => None,
                }
            };
            match tab {
                None => return Err(SqlError::new(1767, format!("Foreign key '{}' references invalid table '{}'.", f.name.clone().unwrap_or_default(), f.table.join(".")))),
                Some(p) => {
                    let ridx: Vec<usize> = ref_cols.iter().filter_map(|c| p.col_index(c)).collect();
                    if ridx.len() != ref_cols.len() {
                        return Err(SqlError::new(1769, format!("Foreign key '{}' references invalid column '{}' in referenced table '{}'.", f.name.clone().unwrap_or_default(), ref_cols.join(","), p.name)));
                    }
                    let is_key = p.all_keys().iter().any(|k| {
                        let mut a = k.cols.clone();
                        let mut b = ridx.clone();
                        a.sort();
                        b.sort();
                        a == b
                    });
                    if !is_key {
                        return Err(SqlError::new(1776, format!("There are no primary or candidate keys in the referenced table '{}' that match the referencing column list in the foreign key '{}'.", p.name, f.name.clone().unwrap_or_default())));
                    }
                    if ridx.len() != cols.len() {
                        return Err(SqlError::new(1774, format!("Number of referencing columns in foreign key differs from number of referenced columns, table '{}'.", t.name)));
                    }
                }
            }
        }
        // existing rows must satisfy the constraint
        if t.live_rows() > 0 {
            let pset: HashSet<String> = {
                let tab: &Table = if self_ref {
                    t
                } else {
                    match self.srv.databases.get(&ref_db).and_then(|d| d.objects.get(&ref_key)) {
                        Some(Object::Table(p)) => p,
                        _ => t,
                    }
                };
                let ridx: Vec<usize> = ref_cols.iter().filter_map(|c| tab.col_index(c)).collect();
                tab.rows_vec().iter().map(|r| key_of(&ridx.iter().map(|&i| r[i].clone()).collect::<Vec<_>>())).collect()
            };
            for r in t.rows_vec().iter() {
                let vals: Vec<Value> = cols.iter().map(|&i| r[i].clone()).collect();
                if vals.iter().any(|v| v.is_null()) {
                    continue;
                }
                if !pset.contains(&key_of(&vals)) {
                    return Err(SqlError::new(547, format!("The ALTER TABLE statement conflicted with the FOREIGN KEY constraint \"{}\". The conflict occurred in database \"{}\", table \"{}\".", f.name.clone().unwrap_or_default(), t.db, f.table.join("."))));
                }
            }
        }
        let name = f.name.clone().unwrap_or_else(|| format!("FK__{}__{}__{:08X}", t.name, f.cols.first().cloned().unwrap_or_default(), self.srv.new_object_id()));
        t.fks.push(FkDef { name, cols, ref_db, ref_key, ref_cols, on_delete: f.on_delete.clone(), on_update: f.on_update.clone(), enabled: true });
        Ok(())
    }

    pub(crate) fn ddl_names(&self, name: &[String]) -> (String, String, String) {
        let (db, schema, n) = self.split_name(name);
        (db.unwrap_or_else(|| self.ses.db.clone()).to_lowercase(), schema.unwrap_or_else(|| "dbo".into()), n)
    }

    /// `ENABLE | DISABLE TRIGGER names | ALL ON table`
    pub fn set_dml_triggers_enabled(&mut self, table: &Name, names: &[String], all: bool, enable: bool) -> Result<()> {
        let tloc = self.locate(table);
        let Some(tt) = self.table_ref(&tloc).cloned() else {
            return Err(SqlError::new(4902, format!("Cannot find the object \"{}\" because it does not exist or you do not have permissions.", table.join("."))));
        };
        self.check_alter(&tloc, table.last().unwrap())?;
        if !all {
            for n in names {
                if !tt.triggers.iter().any(|g| g.name.eq_ignore_ascii_case(n)) {
                    return Err(SqlError::new(4947, format!("Cannot find the trigger '{}', because it does not exist or you do not have permission.", n)));
                }
            }
        }
        self.ddl_lock(&tloc, &tt)?;
        self.undo_table(&tloc, &tt);
        let mut t = self.take_table(&tloc).unwrap();
        let mut changed: Vec<Arc<Trigger>> = vec![];
        for g in t.triggers.iter_mut() {
            if all || names.iter().any(|n| g.name.eq_ignore_ascii_case(n)) {
                let mut ng = (**g).clone();
                ng.disabled = !enable;
                *g = Arc::new(ng);
                changed.push(g.clone());
            }
        }
        let tk = t.key();
        let db = t.db.to_lowercase();
        self.put_table(&tloc, t);
        if let Some(d) = self.srv.cat_mut().databases.get_mut(&db) {
            for (_, (k, g)) in d.triggers.iter_mut() {
                if *k == tk {
                    if let Some(c) = changed.iter().find(|c| c.name.eq_ignore_ascii_case(&g.name)) {
                        *g = c.clone();
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn check_schema(&self, db: &str, schema: &str) -> Result<()> {
        let d = self.srv.databases.get(db).ok_or_else(|| SqlError::new(911, format!("Database '{}' does not exist.", db)))?;
        if !d.schemas.contains_key(&schema.to_lowercase()) {
            return Err(SqlError::new(2760, format!("The specified schema name \"{}\" either does not exist or you do not have permission to use it.", schema)));
        }
        Ok(())
    }

    fn object_exists_err(&self, name: &str) -> SqlError {
        SqlError::new(2714, format!("There is already an object named '{}' in the database.", name))
    }

    pub fn exec_ddl(&mut self, st: &Stmt) -> Result<()> {
        match st {
            Stmt::CreateTable { name, defs, versioning } => {
                let (db, schema, n) = self.ddl_names(name);
                let loc = self.locate(name);
                match &loc {
                    Loc::Temp(_) | Loc::GTemp(_) => {
                        if self.table_ref(&loc).is_some() {
                            return Err(self.object_exists_err(&n));
                        }
                        let mut t = self.build_table("tempdb", "dbo", &n, defs, true)?;
                        if versioning.is_some() || t.temporal.is_some() {
                            return Err(SqlError::new(13506, "System versioning is not supported on temporary tables."));
                        }
                        t.is_temp = true;
                        self.put_table(&loc, t);
                        self.record_table_creation(&loc)?;
                    }
                    _ => {
                        self.check_schema(&db, &schema)?;
                        self.check_create("CREATE TABLE", &schema)?;
                        let key = obj_key(&schema, &n);
                        if self.srv.databases[&db].objects.contains_key(&key) {
                            return Err(self.object_exists_err(&n));
                        }
                        let dbname = self.srv.databases[&db].name.clone();
                        let mut t = self.build_table(&dbname, &schema, &n, defs, false)?;
                        match versioning {
                            Some(v) if v.on => self.attach_history(&db, &mut t, &v.history)?,
                            _ => {}
                        }
                        self.ensure_tx()?;
                        self.undo_object(&db, &key);
                        self.srv.cat_mut().databases.get_mut(&db).unwrap().objects.insert(key, Object::Table(t));
                    }
                }
            }
            Stmt::CreateIndex { name, table, cols, unique, include, filter } => {
                let loc = self.locate(table);
                let t = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(table))?;
                self.check_alter(&loc, table.last().unwrap())?;
                self.ddl_lock(&loc, &t)?;
                self.undo_table(&loc, &t);
                let mut t = self.take_table(&loc).ok_or_else(|| self.invalid_object(table))?;
                let r = (|| -> Result<()> {
                    let idx: Vec<(usize, bool)> = cols.iter().map(|(c, d)| t.col_index(c).map(|i| (i, *d)).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", c)))).collect::<Result<_>>()?;
                    if t.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(name)) || t.all_keys().iter().any(|k| k.name.eq_ignore_ascii_case(name)) {
                        return Err(SqlError::new(1913, format!("The operation failed because an index or statistics with name '{}' already exists on table '{}'.", name, t.name)));
                    }
                    // INCLUDE columns ride along in the index; they may not also be key columns
                    let mut inc: Vec<usize> = vec![];
                    for c in include {
                        let i = t.col_index(c).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", c)))?;
                        if idx.iter().any(|(k, _)| *k == i) || inc.contains(&i) {
                            return Err(SqlError::new(1909, format!("Cannot use duplicate column names in index. Column name '{}' listed more than once.", c)));
                        }
                        inc.push(i);
                    }
                    let fp = match filter {
                        Some((e, text)) => Some(self.filter_pred(&t, e, text, name)?),
                        None => None,
                    };
                    if *unique {
                        let mut seen = HashSet::new();
                        for r in t.rows_vec().iter() {
                            if fp.as_ref().is_some_and(|f| !f.admits(r)) {
                                continue; // not in the index, so not subject to its uniqueness
                            }
                            let vals: Vec<Value> = idx.iter().map(|(i, _)| r[*i].clone()).collect();
                            let icols: Vec<usize> = idx.iter().map(|(i, _)| *i).collect();
                            if !seen.insert(t.key_string(&icols, &vals)) {
                                let shown = vals.iter().map(|v| if v.is_null() { "<NULL>".into() } else { convert::to_str(v) }).collect::<Vec<_>>().join(", ");
                                return Err(SqlError::new(1505, format!("CREATE UNIQUE INDEX terminated because a duplicate key was found for the object name '{}.{}' and the index name '{}'. The duplicate key value is ({}).", t.schema, t.name, name, shown)));
                            }
                        }
                        t.uniques.push(KeyDef { name: name.clone(), cols: idx.iter().map(|x| x.0).collect(), is_index: true, clustered: false, include: inc.clone(), filter: fp.clone() });
                    }
                    t.indexes.push(IndexDef { name: name.clone(), cols: idx, unique: *unique, include: inc, filter: fp });
                    t.sync_indexes();
                    Ok(())
                })();
                self.put_table(&loc, t);
                r?;
            }
            Stmt::CreateView { name, cols, query, text, or_alter, alter, check_option } => {
                let (db, schema, n) = self.ddl_names(name);
                self.check_schema(&db, &schema)?;
                self.check_create("CREATE VIEW", &schema)?;
                let key = obj_key(&schema, &n);
                let exists = self.srv.databases[&db].objects.contains_key(&key);
                if *alter && !exists {
                    return Err(SqlError::new(208, format!("Invalid object name '{}'.", name.join("."))));
                }
                if exists && !*or_alter && !*alter {
                    return Err(self.object_exists_err(&n));
                }
                // validate by compiling
                let saved = std::mem::take(&mut self.ses.cte_stack);
                let probe = self.exec_query(query, None);
                self.ses.cte_stack = saved;
                let rs = probe?;
                if let Some(c) = cols {
                    if c.len() != rs.schema.cols.len() {
                        return Err(SqlError::new(4511, format!("Create View or Function failed because no column name was specified for column {}.", c.len() + 1)));
                    }
                } else if let Some((i, _)) = rs.schema.cols.iter().enumerate().find(|(_, c)| c.name.is_empty()) {
                    return Err(SqlError::new(4511, format!("Create View or Function failed because no column name was specified for column {}.", i + 1)));
                }
                let dbname = self.srv.databases[&db].name.clone();
                let oid = if exists { self.srv.databases[&db].objects[&key].object_id() } else { self.srv.new_object_id() };
                let v = ViewDef { db: dbname, schema, name: n, query: query.clone(), cols: cols.clone(), text: text.clone(), object_id: oid, triggers: vec![], check_option: *check_option };
                self.ensure_tx()?;
                self.undo_object(&db, &key);
                self.srv.cat_mut().databases.get_mut(&db).unwrap().objects.insert(key, Object::View(v));
            }
            Stmt::CreateProc { name, params, body, text, or_alter, alter } => {
                let (db, schema, n) = self.ddl_names(name);
                self.check_schema(&db, &schema)?;
                self.check_create("CREATE PROCEDURE", &schema)?;
                let key = obj_key(&schema, &n);
                let exists = self.srv.databases[&db].objects.contains_key(&key);
                if *alter && !exists {
                    return Err(SqlError::new(208, format!("Invalid object name '{}'.", name.join("."))));
                }
                if exists && !*or_alter && !*alter {
                    return Err(self.object_exists_err(&n));
                }
                let oid = if exists { self.srv.databases[&db].objects[&key].object_id() } else { self.srv.new_object_id() };
                let p = ProcDef { schema, name: n, params: params.clone(), body: body.clone(), text: text.clone(), object_id: oid };
                self.ensure_tx()?;
                self.undo_object(&db, &key);
                self.srv.cat_mut().databases.get_mut(&db).unwrap().objects.insert(key, Object::Proc(p));
            }
            Stmt::CreateFunction(f) => {
                let (db, schema, n) = self.ddl_names(&f.name);
                self.check_schema(&db, &schema)?;
                self.check_create("CREATE FUNCTION", &schema)?;
                let key = obj_key(&schema, &n);
                let exists = self.srv.databases[&db].objects.contains_key(&key);
                if exists && !f.or_alter {
                    return Err(self.object_exists_err(&n));
                }
                let oid = if exists { self.srv.databases[&db].objects[&key].object_id() } else { self.srv.new_object_id() };
                let fd = FuncDef { schema, name: n, def: Arc::new((**f).clone()), object_id: oid };
                self.ensure_tx()?;
                self.undo_object(&db, &key);
                self.srv.cat_mut().databases.get_mut(&db).unwrap().objects.insert(key, Object::Func(fd));
            }
            Stmt::CreateTrigger { name, table, events, instead, body, or_alter, text } => {
                let (db, schema, n) = self.ddl_names(name);
                let tkey = obj_key(&schema, &n);
                let tloc = self.locate(table);
                let oid = self.srv.new_object_id();
                let trg = Arc::new(Trigger { name: n.clone(), schema: schema.clone(), events: events.clone(), instead: *instead, body: body.clone(), text: text.clone(), object_id: oid, disabled: false });
                match self.table_ref(&tloc).cloned() {
                    Some(tt) => {
                        self.check_alter(&tloc, table.last().unwrap())?;
                        self.ddl_lock(&tloc, &tt)?;
                        self.undo_table(&tloc, &tt);
                    }
                    None => return Err(SqlError::new(8197, format!("The object '{}' does not exist or is invalid for this operation.", table.join(".")))),
                }
                let mut t = self.take_table(&tloc).unwrap();
                let exists = t.triggers.iter().any(|x| x.name.eq_ignore_ascii_case(&n));
                if exists && !*or_alter {
                    self.put_table(&tloc, t);
                    return Err(self.object_exists_err(&n));
                }
                t.triggers.retain(|x| !x.name.eq_ignore_ascii_case(&n));
                t.triggers.push(trg.clone());
                let tk = t.key();
                self.put_table(&tloc, t);
                self.srv.cat_mut().databases.get_mut(&db).unwrap().triggers.insert(tkey, (tk, trg));
            }
            Stmt::CreateSchema(n, owner) => {
                self.check_create_in("CREATE SCHEMA", None)?;
                let dbk = self.ses.db.to_lowercase();
                if self.current_db().schemas.contains_key(&n.to_lowercase()) {
                    return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", n)));
                }
                self.ensure_tx()?;
                self.push_undo(Undo::Schema { db: dbk.clone(), key: n.to_lowercase(), prev: None });
                let owner = owner.clone().unwrap_or_else(|| self.current_user_name()).to_lowercase();
                self.current_db().schemas.insert(n.to_lowercase(), n.clone());
                self.current_db().assign_schema_id(&n.to_lowercase());
                self.current_db().schema_owner.insert(n.to_lowercase(), owner);
            }
            Stmt::CreateDatabase(n, coll) => {
                if let Some(cn) = coll {
                    if crate::collation::get(cn).is_none() {
                        return Err(SqlError::with(448, 16, 1, format!("Invalid collation '{}'.", cn)));
                    }
                }
                if !self.is_sysadmin() && !self.srv.security.dbcreators.contains(&self.eff_login().to_lowercase()) {
                    return Err(SqlError::with(262, 14, 1, "CREATE DATABASE permission denied in database 'master'."));
                }
                let k = n.to_lowercase();
                if self.srv.databases.contains_key(&k) {
                    return Err(SqlError::new(1801, format!("Database '{}' already exists. Choose a different database name.", n)));
                }
                let id = self.srv.databases.len() as i32 + 1;
                self.ensure_tx()?;
                self.push_undo(Undo::Database { key: k.clone(), prev: None });
                let mut nd = Database::new(n, id);
                if let Some(cn) = coll {
                    nd.collation = cn.clone();
                }
                self.srv.cat_mut().databases.insert(k, nd);
            }
            Stmt::CreateType { name, base, nullable, table } => {
                let (db, schema, n) = self.ddl_names(name);
                self.check_create("CREATE TYPE", &schema)?;
                let key = obj_key(&schema, &n);
                if self.srv.databases[&db].types.contains_key(&key) {
                    return Err(SqlError::new(2714, format!("There is already an object named '{}' in the database.", n)));
                }
                let kind = match (base, table) {
                    (Some(b), _) => UserTypeKind::Alias(self.resolve_type(b)?, nullable.unwrap_or(true)),
                    (None, Some(defs)) => UserTypeKind::Table(defs.clone()),
                    _ => unreachable!(),
                };
                let id = self.srv.new_object_id();
                self.ensure_tx()?;
                self.push_undo(Undo::Type { db: db.clone(), key: key.clone(), prev: None });
                self.srv.cat_mut().databases.get_mut(&db).unwrap().types.insert(key, UserType { schema, name: n, kind, type_id: id });
            }
            Stmt::CreateSynonym { name, target } => {
                let (db, schema, n) = self.ddl_names(name);
                self.check_schema(&db, &schema)?;
                self.check_create("CREATE SYNONYM", &schema)?;
                let key = obj_key(&schema, &n);
                if self.srv.databases[&db].objects.contains_key(&key) {
                    return Err(self.object_exists_err(&n));
                }
                let oid = self.srv.new_object_id();
                self.ensure_tx()?;
                self.undo_object(&db, &key);
                let def = SynonymDef { schema, name: n, target: target.clone(), object_id: oid };
                self.srv.cat_mut().databases.get_mut(&db).unwrap().objects.insert(key, Object::Synonym(def));
            }
            Stmt::CreateSequence { name, opts } => {
                let (db, schema, n) = self.ddl_names(name);
                self.check_schema(&db, &schema)?;
                self.check_create("CREATE SEQUENCE", &schema)?;
                let key = obj_key(&schema, &n);
                if self.srv.databases[&db].objects.contains_key(&key) {
                    return Err(self.object_exists_err(&n));
                }
                let ty = match &opts.ty {
                    Some(t) => self.resolve_type(t)?,
                    None => DataType::BigInt,
                };
                let (tmin, tmax) = match ty {
                    DataType::TinyInt => (0i64, 255i64),
                    DataType::SmallInt => (-32768, 32767),
                    DataType::Int => (i32::MIN as i64, i32::MAX as i64),
                    _ => (i64::MIN, i64::MAX),
                };
                let inc = match &opts.inc {
                    Some(e) => self.const_i64(e)?,
                    None => 1,
                };
                if inc == 0 {
                    return Err(SqlError::new(11709, "The increment for sequence object cannot be zero."));
                }
                let min = match &opts.min {
                    Some(e) => self.const_i64(e)?,
                    None => tmin,
                };
                let max = match &opts.max {
                    Some(e) => self.const_i64(e)?,
                    None => tmax,
                };
                let start = match &opts.start {
                    Some(e) => self.const_i64(e)?,
                    None => {
                        if inc > 0 {
                            min
                        } else {
                            max
                        }
                    }
                };
                let oid = self.srv.new_object_id();
                let s = Sequence { schema, name: n, ty, current: crate::catalog::SharedCounter::new(None), start, inc, min, max, cycle: opts.cycle.unwrap_or(false), object_id: oid };
                self.ensure_tx()?;
                self.undo_object(&db, &key);
                self.srv.cat_mut().databases.get_mut(&db).unwrap().objects.insert(key, Object::Seq(s));
            }
            Stmt::AlterSequence { name, opts } => {
                let (db, schema, n) = self.ddl_names(name);
                let key = obj_key(&schema, &n);
                let inc = match &opts.inc {
                    Some(e) => Some(self.const_i64(e)?),
                    None => None,
                };
                let min = match &opts.min {
                    Some(e) => Some(self.const_i64(e)?),
                    None => None,
                };
                let max = match &opts.max {
                    Some(e) => Some(self.const_i64(e)?),
                    None => None,
                };
                let start = match &opts.start {
                    Some(e) => Some(self.const_i64(e)?),
                    None => None,
                };
                match self.srv.cat_mut().databases.get_mut(&db).and_then(|d| d.objects.get_mut(&key)) {
                    Some(Object::Seq(s)) => {
                        if let Some(i) = inc {
                            s.inc = i;
                        }
                        if let Some(m) = min {
                            s.min = m;
                        }
                        if let Some(m) = max {
                            s.max = m;
                        }
                        if let Some(c) = opts.cycle {
                            s.cycle = c;
                        }
                        if opts.restart {
                            s.current.set(None);
                            if let Some(st) = start {
                                s.start = st;
                            }
                        }
                    }
                    _ => return Err(SqlError::new(208, format!("Invalid object name '{}'.", name.join(".")))),
                }
            }
            Stmt::AlterTable { name, action: AlterAction::SetVersioning(v) } => self.set_versioning(name, v)?,
            Stmt::AlterTable { name, action } => self.alter_table(name, action)?,
            Stmt::Drop { what, names, if_exists } => {
                for n in names {
                    self.drop_object(what, n, *if_exists)?;
                }
            }
            Stmt::DropIndex { names, if_exists } => {
                for (iname, tbl) in names {
                    let (tname, idx) = match tbl {
                        Some(t) => (t.clone(), iname.last().unwrap().clone()),
                        None if iname.len() >= 2 => (iname[..iname.len() - 1].to_vec(), iname.last().unwrap().clone()),
                        None => return Err(SqlError::new(7999, "Could not find index.")),
                    };
                    let loc = self.locate(&tname);
                    match self.table_ref(&loc).cloned() {
                        Some(tt) => {
                            self.check_alter(&loc, tname.last().unwrap())?;
                            self.ddl_lock(&loc, &tt)?;
                            self.undo_table(&loc, &tt);
                        }
                        None => {
                            if *if_exists {
                                continue;
                            }
                            return Err(self.invalid_object(&tname));
                        }
                    }
                    let mut t = self.take_table(&loc).unwrap();
                    let before = t.indexes.len();
                    t.indexes.retain(|i| !i.name.eq_ignore_ascii_case(&idx));
                    t.uniques.retain(|k| !(k.is_index && k.name.eq_ignore_ascii_case(&idx)));
                    let found = t.indexes.len() != before;
                    t.sync_indexes();
                    self.put_table(&loc, t);
                    if !found && !*if_exists {
                        return Err(SqlError::new(3703, format!("Cannot drop the index '{}', because it does not exist or you do not have permission.", idx)));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn drop_object(&mut self, what: &str, name: &[String], if_exists: bool) -> Result<()> {
        if matches!(what, "LOGIN" | "USER" | "ROLE") {
            return self.drop_principal(what, name.last().unwrap(), if_exists);
        }
        let (db, schema, n) = self.ddl_names(name);
        let key = obj_key(&schema, &n);
        let kind_desc = what.to_lowercase();
        if matches!(what, "VIEW" | "PROCEDURE" | "FUNCTION" | "SEQUENCE") && self.srv.databases.get(&db).map(|d| d.objects.contains_key(&key)).unwrap_or(false) {
            self.check_alter(&Loc::Db(db.clone(), key.clone()), &n)?;
        }
        match what {
            "TABLE" => {
                let loc = self.locate(name);
                match &loc {
                    Loc::Temp(k) => {
                        self.ensure_tx()?;
                        let prev = self.ses.temp.get(k).cloned();
                        self.push_undo(Undo::Temp { name: k.clone(), prev });
                        if self.ses.temp.remove(k).is_none() && !if_exists {
                            return Err(SqlError::new(3701, format!("Cannot drop the table '{}', because it does not exist or you do not have permission.", n)));
                        }
                        return Ok(());
                    }
                    Loc::GTemp(k) => {
                        self.ensure_tx()?;
                        let prev = self.srv.global_temp.get(k).cloned();
                        self.push_undo(Undo::GTemp { name: k.clone(), prev });
                        if self.srv.cat_mut().global_temp.remove(k).is_none() && !if_exists {
                            return Err(SqlError::new(3701, format!("Cannot drop the table '{}', because it does not exist or you do not have permission.", n)));
                        }
                        return Ok(());
                    }
                    _ => {}
                }
                let t = match self.table_ref(&loc) {
                    Some(t) => t.clone(),
                    None => {
                        if if_exists {
                            return Ok(());
                        }
                        return Err(SqlError::new(3701, format!("Cannot drop the table '{}', because it does not exist or you do not have permission.", n)));
                    }
                };
                // referenced by another table's FK?
                let tk = t.key();
                for (_, d) in &self.srv.databases {
                    for o in d.objects.values() {
                        if let Object::Table(c) = o {
                            if c.key() != tk && c.fks.iter().any(|f| f.ref_key == tk && f.ref_db.eq_ignore_ascii_case(&t.db)) {
                                return Err(SqlError::new(3726, format!("Could not drop object '{}' because it is referenced by a FOREIGN KEY constraint.", n)));
                            }
                        }
                    }
                }
                self.temporal_guard(&t, "DROP TABLE")?;
                self.check_alter(&loc, &n)?;
                self.ddl_lock(&loc, &t)?;
                self.undo_object(&db, &key);
                let d = self.srv.cat_mut().databases.get_mut(&db).unwrap();
                d.objects.remove(&key);
                d.triggers.retain(|_, (tk2, _)| *tk2 != tk);
            }
            "VIEW" | "PROCEDURE" | "FUNCTION" | "SEQUENCE" | "SYNONYM" => {
                let d = self.srv.cat_mut().databases.get_mut(&db).ok_or_else(|| SqlError::new(911, "Database does not exist."))?;
                let ok = match d.objects.get(&key) {
                    Some(Object::View(_)) => what == "VIEW",
                    Some(Object::Proc(_)) => what == "PROCEDURE",
                    Some(Object::Func(_)) => what == "FUNCTION",
                    Some(Object::Seq(_)) => what == "SEQUENCE",
                    Some(Object::Synonym(_)) => what == "SYNONYM",
                    _ => false,
                };
                if ok {
                    self.ensure_tx()?;
                    self.undo_object(&db, &key);
                    let d = self.srv.cat_mut().databases.get_mut(&db).unwrap();
                    d.objects.remove(&key);
                } else if !if_exists {
                    return Err(SqlError::new(3701, format!("Cannot drop the {} '{}', because it does not exist or you do not have permission.", kind_desc, n)));
                }
            }
            "TRIGGER" => {
                let found = self.srv.cat_mut().databases.get_mut(&db).and_then(|d| d.triggers.remove(&key));
                match found {
                    Some((tk, _)) => {
                        let loc = Loc::Db(db.clone(), tk);
                        if let Some(mut t) = self.take_table(&loc) {
                            t.triggers.retain(|x| !x.name.eq_ignore_ascii_case(&n));
                            self.put_table(&loc, t);
                        }
                    }
                    None => {
                        if !if_exists {
                            return Err(SqlError::new(3701, format!("Cannot drop the trigger '{}', because it does not exist or you do not have permission.", n)));
                        }
                    }
                }
            }
            "SCHEMA" => {
                let d = self.srv.cat_mut().databases.get_mut(&db).unwrap();
                let k = n.to_lowercase();
                if !d.schemas.contains_key(&k) {
                    if if_exists {
                        return Ok(());
                    }
                    return Err(SqlError::new(3701, format!("Cannot drop the schema '{}', because it does not exist or you do not have permission.", n)));
                }
                if d.objects.values().any(|o| o.schema().eq_ignore_ascii_case(&n)) {
                    return Err(SqlError::new(3729, format!("Cannot drop schema '{}' because it is being referenced by object.", n)));
                }
                d.schemas.remove(&k);
            }
            "DATABASE" => {
                let k = n.to_lowercase();
                if ["master", "tempdb", "model", "msdb"].contains(&k.as_str()) {
                    return Err(SqlError::new(3701, format!("Cannot drop the database '{}', because it is a system database.", n)));
                }
                if let Some(prev) = self.srv.databases.get(&k).cloned() {
                    self.ensure_tx()?;
                    self.push_undo(Undo::Database { key: k.clone(), prev: Some(prev) });
                }
                if self.srv.cat_mut().databases.remove(&k).is_none() && !if_exists {
                    return Err(SqlError::new(3701, format!("Cannot drop the database '{}', because it does not exist or you do not have permission.", n)));
                }
                if self.ses.db.eq_ignore_ascii_case(&n) {
                    self.ses.db = "master".into();
                }
            }
            "TYPE" => {
                let d = self.srv.cat_mut().databases.get_mut(&db).unwrap();
                if d.types.remove(&key).is_none() && !if_exists {
                    return Err(SqlError::new(3701, format!("Cannot drop type '{}', because it does not exist or you do not have permission.", n)));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn alter_table(&mut self, name: &[String], action: &AlterAction) -> Result<()> {
        let loc = self.locate(name);
        let cur = self.table_ref(&loc).cloned().ok_or_else(|| self.invalid_object(name))?;
        self.check_alter(&loc, name.last().unwrap())?;
        self.ddl_lock(&loc, &cur)?;
        self.undo_table(&loc, &cur);
        drop(cur);
        let mut t = self.take_table(&loc).ok_or_else(|| self.invalid_object(name))?;
        let backup = t.clone();
        let r = self.alter_table_inner(&mut t, action);
        match r {
            Ok(()) => {
                t.sync_indexes();
                self.put_table(&loc, t);
                Ok(())
            }
            Err(e) => {
                self.put_table(&loc, backup);
                Err(e)
            }
        }
    }

    fn alter_table_inner(&mut self, t: &mut Table, action: &AlterAction) -> Result<()> {
        if (Self::is_versioned(t) || Self::is_history(t)) && matches!(action, AlterAction::AddDefs(_) | AlterAction::DropColumn(..) | AlterAction::AlterColumn { .. }) {
            return self.temporal_guard(t, "ALTER TABLE");
        }
        match action {
            AlterAction::AddDefs(defs) => {
                let col_defs: Vec<&ColumnDef> = defs.iter().filter_map(|d| if let TableDef::Column(c) = d { Some(c) } else { None }).collect();
                for c in &col_defs {
                    if t.col_index(&c.name).is_some() {
                        return Err(SqlError::new(2705, format!("Column names in each table must be unique. Column name '{}' in table '{}' is specified more than once.", c.name, t.name)));
                    }
                    let probe_defs = vec![TableDef::Column((*c).clone())];
                    let tmp = self.build_table(&t.db.clone(), &t.schema.clone(), &t.name.clone(), &probe_defs, false)?;
                    let mut col = tmp.columns[0].clone();
                    if c.nullable.is_none() && c.pk.is_none() {
                        col.nullable = true;
                    }
                    if t.live_rows() > 0 && !col.nullable && col.default.is_none() && col.identity.is_none() && col.computed.is_none() {
                        return Err(SqlError::new(4901, format!("ALTER TABLE only allows columns to be added that can contain nulls, or have a DEFAULT definition specified, or the column being added is an identity or timestamp column, or alternatively if none of the previous conditions are satisfied the table must be empty to allow addition of this column. Column '{}' cannot be added to non-empty table '{}' because it does not satisfy these conditions.", c.name, t.name)));
                    }
                    let fill = if col.default.is_some() { self.eval_default_pub(&col)? } else { Value::Null };
                    let mut rows: Vec<Row> = t.rows_vec();
                    let mut ident = col.identity.map(|(s, _)| s);
                    for r in rows.iter_mut() {
                        if let Some((_, inc)) = col.identity {
                            r.push(Value::BigInt(ident.unwrap()));
                            ident = Some(ident.unwrap() + inc);
                        } else {
                            r.push(fill.clone());
                        }
                    }
                    t.replace_rows(rows);
                    if col.identity.is_some() {
                        t.identity_next.set(ident);
                    }
                    t.columns.push(col);
                    let ci = t.columns.len() - 1;
                    if let Some(pk) = &c.pk {
                        self.add_pk(t, pk.clone(), vec![ci])?;
                    }
                    if let Some(u) = &c.unique {
                        let kn = u.clone().unwrap_or_else(|| format!("UQ__{}__{:08X}", t.name, self.srv.new_object_id()));
                        t.uniques.push(KeyDef { name: kn, cols: vec![ci], is_index: false, clustered: false, include: vec![], filter: None });
                    }
                    for (n, e, ctext) in &c.checks {
                        let cn = n.clone().unwrap_or_else(|| format!("CK__{}__{}__{:08X}", t.name, c.name, self.srv.new_object_id()));
                        t.checks.push(CheckDef { name: cn, expr: Arc::new(e.clone()), text: ctext.clone(), enabled: true, col: Some(c.name.clone()) });
                    }
                    if let Some(f) = &c.fk {
                        let mut f = f.clone();
                        f.cols = vec![c.name.clone()];
                        self.add_fk(t, &f)?;
                    }
                }
                for d in defs {
                    match d {
                        TableDef::PrimaryKey { name, cols } => {
                            let idx: Vec<usize> = cols.iter().map(|(c, _)| t.col_index(c).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", c)))).collect::<Result<_>>()?;
                            for &i in &idx {
                                if t.rows_vec().iter().any(|r| r[i].is_null()) {
                                    return Err(SqlError::new(8111, format!("Cannot define PRIMARY KEY constraint on nullable column in table '{}'.", t.name)));
                                }
                            }
                            self.add_pk(t, name.clone(), idx)?;
                            self.validate_keys(t)?;
                        }
                        TableDef::Unique { name, cols } => {
                            let idx: Vec<usize> = cols.iter().map(|(c, _)| t.col_index(c).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", c)))).collect::<Result<_>>()?;
                            let kn = name.clone().unwrap_or_else(|| format!("UQ__{}__{:08X}", t.name, self.srv.new_object_id()));
                            t.uniques.push(KeyDef { name: kn, cols: idx, is_index: false, clustered: false, include: vec![], filter: None });
                            self.validate_keys(t)?;
                        }
                        TableDef::Check { name, e, text: ctext } => {
                            let cn = name.clone().unwrap_or_else(|| format!("CK__{}__{:08X}", t.name, self.srv.new_object_id()));
                            let ck = CheckDef { name: cn, expr: Arc::new(e.clone()), text: ctext.clone(), enabled: true, col: None };
                            // existing rows must satisfy
                            let schema = super::dml::table_schema(t, &t.name);
                            for r in t.rows_vec().iter() {
                                let env = Env::new(&schema, r, None);
                                if matches!(self.ev(e, &env)?, Value::Bit(false)) {
                                    return Err(SqlError::new(547, format!("The ALTER TABLE statement conflicted with the CHECK constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\".", ck.name, t.db, t.schema, t.name)));
                                }
                            }
                            t.checks.push(ck);
                        }
                        TableDef::ForeignKey(f) => self.add_fk(t, f)?,
                        TableDef::DefaultFor { name, e, col, text: dtext } => {
                            let ci = t.col_index(col).ok_or_else(|| SqlError::new(1911, format!("Column name '{}' does not exist in the target table or view.", col)))?;
                            t.columns[ci].default = Some(Arc::new(e.clone()));
                    t.columns[ci].default_text = Some(dtext.clone());
                            t.columns[ci].default_name = Some(name.clone());
                        }
                        TableDef::Index { name, cols, unique } => {
                            let idx: Vec<(usize, bool)> = cols.iter().filter_map(|(c, d)| t.col_index(c).map(|i| (i, *d))).collect();
                            t.indexes.push(IndexDef { name: name.clone(), cols: idx, unique: *unique, include: vec![], filter: None });
                        }
                        TableDef::Column(_) => {}
                        TableDef::Period { .. } => {}
                    }
                }
            }
            AlterAction::SetVersioning(_) => {}
            AlterAction::DropConstraint(names, if_exists) => {
                for n in names {
                    let mut found = false;
                    if t.pk.as_ref().map(|k| k.name.eq_ignore_ascii_case(n)).unwrap_or(false) {
                        // referenced by FKs?
                        let tk = t.key();
                        for (_, d) in &self.srv.databases {
                            for o in d.objects.values() {
                                if let Object::Table(c) = o {
                                    if c.fks.iter().any(|f| f.ref_key == tk) {
                                        return Err(SqlError::new(3725, format!("The constraint '{}' is being referenced by table '{}', foreign key constraint '{}'.", n, c.name, c.fks[0].name)));
                                    }
                                }
                            }
                        }
                        t.pk = None;
                        found = true;
                    }
                    let before = (t.uniques.len(), t.checks.len(), t.fks.len());
                    t.uniques.retain(|k| !k.name.eq_ignore_ascii_case(n) || k.is_index);
                    t.checks.retain(|k| !k.name.eq_ignore_ascii_case(n));
                    t.fks.retain(|k| !k.name.eq_ignore_ascii_case(n));
                    if before != (t.uniques.len(), t.checks.len(), t.fks.len()) {
                        found = true;
                    }
                    for c in t.columns.iter_mut() {
                        if c.default_name.as_deref().map(|d| d.eq_ignore_ascii_case(n)).unwrap_or(false) {
                            c.default = None;
                            c.default_text = None;
                            c.default_name = None;
                            found = true;
                        }
                    }
                    if !found && !*if_exists {
                        return Err(SqlError::new(3728, format!("'{}' is not a constraint.", n)));
                    }
                }
            }
            AlterAction::DropColumn(names, if_exists) => {
                for n in names {
                    let ci = match t.col_index(n) {
                        Some(i) => i,
                        None => {
                            if *if_exists {
                                continue;
                            }
                            return Err(SqlError::new(4924, format!("ALTER TABLE DROP COLUMN failed because column '{}' does not exist in table '{}'.", n, t.name)));
                        }
                    };
                    let in_key = t.all_keys().iter().any(|k| k.cols.contains(&ci)) || t.fks.iter().any(|f| f.cols.contains(&ci)) || t.checks.iter().any(|c| c.col.as_deref() == Some(n.as_str())) || t.indexes.iter().any(|i| i.cols.iter().any(|(c, _)| *c == ci));
                    if in_key {
                        return Err(SqlError::new(5074, format!("The object depends on column '{}'. ALTER TABLE DROP COLUMN failed because one or more objects access this column.", n)));
                    }
                    if t.columns.len() == 1 {
                        return Err(SqlError::new(4923, "ALTER TABLE DROP COLUMN failed because it would leave the table with no columns."));
                    }
                    let mut rows: Vec<Row> = t.rows_vec();
                    for r in rows.iter_mut() {
                        r.remove(ci);
                    }
                    t.replace_rows(rows);
                    t.columns.remove(ci);
                    let shift = |v: &mut Vec<usize>| {
                        for x in v.iter_mut() {
                            if *x > ci {
                                *x -= 1;
                            }
                        }
                    };
                    if let Some(pk) = t.pk.as_mut() {
                        shift(&mut pk.cols);
                    }
                    for k in t.uniques.iter_mut() {
                        shift(&mut k.cols);
                    }
                    for f in t.fks.iter_mut() {
                        shift(&mut f.cols);
                    }
                    for i in t.indexes.iter_mut() {
                        for c in i.cols.iter_mut() {
                            if c.0 > ci {
                                c.0 -= 1;
                            }
                        }
                    }
                }
            }
            AlterAction::AlterMask { col, mask } => {
                let ci = t.col_index(col).ok_or_else(|| SqlError::new(4902, format!("Cannot find the object \"{}\" because it does not exist or you do not have permission.", col)))?;
                let m = self.validated_mask(mask, &t.columns[ci].ty, col)?;
                t.columns[ci].mask = m;
            }
            AlterAction::AlterColumn { col, ty, nullable, collation } => {
                let ci = t.col_index(col).ok_or_else(|| SqlError::new(4902, format!("Cannot find the object \"{}\" because it does not exist or you do not have permission.", col)))?;
                let dt = self.resolve_type(ty)?;
                let mut rows: Vec<Row> = t.rows_vec();
                for r in rows.iter_mut() {
                    r[ci] = convert::coerce_store(&r[ci], &dt, &t.display_name(), col)?;
                }
                let nn = nullable.unwrap_or(t.columns[ci].nullable);
                if !nn && rows.iter().any(|r| r[ci].is_null()) {
                    return Err(SqlError::new(515, format!("Cannot insert the value NULL into column '{}', table '{}'; column does not allow nulls. UPDATE fails.", col, t.display_name())));
                }
                t.replace_rows(rows);
                let coll = self.column_collation(collation, &dt)?;
                // a change of type or collation keeps the old collation unless the statement names one
                t.columns[ci].collation = if collation.is_some() || !dt.is_string() { coll } else { t.columns[ci].collation.clone() };
                t.columns[ci].ty = dt;
                t.columns[ci].nullable = nn;
            }
            AlterAction::CheckConstraint { name, enable } => {
                for c in t.checks.iter_mut() {
                    if name.as_ref().map(|n| c.name.eq_ignore_ascii_case(n)).unwrap_or(true) {
                        c.enabled = *enable;
                    }
                }
                for f in t.fks.iter_mut() {
                    if name.as_ref().map(|n| f.name.eq_ignore_ascii_case(n)).unwrap_or(true) {
                        f.enabled = *enable;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_keys(&mut self, t: &Table) -> Result<()> {
        for k in t.all_keys() {
            let mut seen = HashSet::new();
            for r in t.rows_vec().iter() {
                let vals: Vec<Value> = k.cols.iter().map(|&c| r[c].clone()).collect();
                if !seen.insert(t.key_string(&k.cols, &vals)) {
                    let shown = vals.iter().map(|v| if v.is_null() { "<NULL>".into() } else { convert::to_str(v) }).collect::<Vec<_>>().join(", ");
                    return Err(SqlError::new(1750, format!("Could not create constraint or index. See previous errors. (duplicate key value {} for '{}')", shown, k.name)));
                }
            }
        }
        Ok(())
    }

    /// Schema-modification lock: DDL waits for every other transaction using the table.
    /// Turn the `WHERE` clause of a filtered index into a [`FilterPred`]: a conjunction of `column <op> constant`,
    /// `column IS [NOT] NULL` and `column IN (constants)`. Anything else is error 10735.
    pub fn filter_pred(&mut self, t: &Table, e: &Expr, text: &str, index: &str) -> Result<crate::storage::FilterPred> {
        use crate::storage::{FilterCond, FilterOp};
        fn flatten<'q>(e: &'q Expr, out: &mut Vec<&'q Expr>) {
            match e {
                Expr::And(a, b) => {
                    flatten(a, out);
                    flatten(b, out);
                }
                Expr::Paren(x) => flatten(x, out),
                other => out.push(other),
            }
        }
        let bad = || SqlError::with(10735, 15, 1, format!("Incorrect WHERE clause for filtered index '{}' on table '{}'.", index, t.name));
        let col_of = |x: &Expr| -> Option<usize> {
            match x {
                Expr::Col(p) => t.col_index(p.last()?),
                Expr::Paren(inner) => match &**inner {
                    Expr::Col(p) => t.col_index(p.last()?),
                    _ => None,
                },
                _ => None,
            }
        };
        let mut parts = vec![];
        flatten(e, &mut parts);
        let mut conds = vec![];
        for p in parts {
            match p {
                Expr::IsNull(x, neg) => {
                    let col = col_of(x).ok_or_else(bad)?;
                    conds.push(FilterCond { col, op: if *neg { FilterOp::IsNotNull } else { FilterOp::IsNull }, vals: vec![] });
                }
                Expr::InList(x, items, false) => {
                    let col = col_of(x).ok_or_else(bad)?;
                    let mut vals = vec![];
                    for it in items {
                        let v = self.ev(it, &Env::empty())?;
                        vals.push(super::optimizer::coerce_const_pub(&t.columns[col], v).ok_or_else(bad)?);
                    }
                    conds.push(FilterCond { col, op: FilterOp::In, vals });
                }
                Expr::Binary(op, a, b) => {
                    use crate::ast::BinOp::*;
                    let (col, other, flipped) = match (col_of(a), col_of(b)) {
                        (Some(c), None) => (c, &**b, false),
                        (None, Some(c)) => (c, &**a, true),
                        _ => return Err(bad()),
                    };
                    let v = self.ev(other, &Env::empty())?;
                    let v = super::optimizer::coerce_const_pub(&t.columns[col], v).ok_or_else(bad)?;
                    let fop = match (op, flipped) {
                        (Eq, _) => FilterOp::Eq,
                        (Ne, _) => FilterOp::Ne,
                        (Lt, false) | (Gt, true) => FilterOp::Lt,
                        (Le, false) | (Ge, true) => FilterOp::Le,
                        (Gt, false) | (Lt, true) => FilterOp::Gt,
                        (Ge, false) | (Le, true) => FilterOp::Ge,
                        _ => return Err(bad()),
                    };
                    conds.push(FilterCond { col, op: fop, vals: vec![v] });
                }
                _ => return Err(bad()),
            }
        }
        Ok(crate::storage::FilterPred { conds, text: text.trim().to_string() })
    }

    /// Check a masking function against the column type; `None` when there is no mask.
    pub fn validated_mask(&self, text: &Option<String>, ty: &DataType, col: &str) -> Result<Option<String>> {
        let Some(t) = text else { return Ok(None) };
        let m = crate::mask::Mask::parse(t).map_err(|e| SqlError::with(16000, 16, 1, format!("Invalid masking function for column '{}': {}.", col, e)))?;
        if !m.fits(ty) {
            return Err(SqlError::with(16003, 16, 1, format!("The masking function '{}' cannot be applied to column '{}' of data type {}.", t, col, ty)));
        }
        Ok(Some(t.clone()))
    }

    /// The collation a new string column gets: the one named, else the current database's default. Stored only when it
    /// is not the built-in default (see `collation.rs`); `None` for columns that are not strings.
    pub fn column_collation(&self, named: &Option<String>, ty: &DataType) -> Result<Option<String>> {
        if !ty.is_string() {
            return Ok(None);
        }
        let name = match named {
            Some(n) => n.clone(),
            None => self.srv.databases.get(&self.ses.db.to_lowercase()).map(|d| d.collation.clone()).unwrap_or_else(|| crate::collation::DEFAULT_NAME.to_string()),
        };
        match crate::collation::get(&name) {
            None => Err(SqlError::with(448, 16, 1, format!("Invalid collation '{}'.", name))),
            // the built-in collation needs no entry, unless the database's own default is another one: then the
            // column must say it is not that
            Some(c) if c.is_default() => {
                let db_default = self.srv.databases.get(&self.ses.db.to_lowercase()).map(|d| d.collation.as_str()).unwrap_or(crate::collation::DEFAULT_NAME);
                Ok(if crate::collation::get(db_default).is_some_and(|d| d.is_default()) { None } else { Some(c.name.clone()) })
            }
            Some(c) => Ok(Some(c.name.clone())),
        }
    }

    pub fn ddl_lock(&mut self, loc: &Loc, t: &Table) -> Result<()> {
        if matches!(loc, Loc::Db(..)) {
            self.lock_table(t.object_id, Mode::X)?;
        } else {
            self.ensure_tx()?;
        }
        Ok(())
    }

    /// Remember a table's current definition (and data) so a rollback can restore it.
    pub fn undo_table(&mut self, loc: &Loc, t: &Table) {
        match loc {
            Loc::Db(d, k) => self.push_undo(Undo::Object { db: d.clone(), key: k.clone(), prev: Some(Object::Table(t.clone())) }),
            Loc::Temp(n) => self.push_undo(Undo::Temp { name: n.clone(), prev: Some(t.clone()) }),
            Loc::GTemp(n) => self.push_undo(Undo::GTemp { name: n.clone(), prev: Some(t.clone()) }),
            Loc::Var(_) => {}
        }
    }

    pub fn eval_default_pub(&mut self, c: &Column) -> Result<Value> {
        match &c.default {
            Some(d) => {
                let v = self.ev(d, &Env::empty())?;
                convert::cast(&v, &c.ty, None)
            }
            None => Ok(Value::Null),
        }
    }
}
