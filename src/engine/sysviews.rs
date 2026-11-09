//! Catalog views (sys.* and INFORMATION_SCHEMA.*), computed on demand.
use super::query::{ColMeta, Rowset, Schema};
use super::*;

fn rs(cols: &[(&str, DataType)], rows: Vec<Row>) -> Rowset {
    Rowset::new(Schema::new(cols.iter().map(|(n, t)| ColMeta { qual: None, name: n.to_string(), ty: Some(t.clone()), denied: None, coll: None, mask: None }).collect()), rows)
}
fn sv(s: &str) -> Value {
    Value::Str(s.to_string())
}
fn nv() -> DataType {
    DataType::NVarChar(128)
}

fn type_info(t: &DataType) -> (i32, i32, i32, i32) {
    // (system_type_id, max_length, precision, scale)
    match t {
        DataType::Bit => (104, 1, 1, 0),
        DataType::TinyInt => (48, 1, 3, 0),
        DataType::SmallInt => (52, 2, 5, 0),
        DataType::Int => (56, 4, 10, 0),
        DataType::BigInt => (127, 8, 19, 0),
        DataType::Decimal(p, s) => (106, 17, *p as i32, *s as i32),
        DataType::Money => (60, 8, 19, 4),
        DataType::SmallMoney => (122, 4, 10, 4),
        DataType::Float => (62, 8, 53, 0),
        DataType::Real => (59, 4, 24, 0),
        DataType::Char(n) => (175, *n, 0, 0),
        DataType::VarChar(n) => (167, *n, 0, 0),
        DataType::NChar(n) => (239, if *n == MAX { -1 } else { *n * 2 }, 0, 0),
        DataType::NVarChar(n) => (231, if *n == MAX { -1 } else { *n * 2 }, 0, 0),
        DataType::Text => (35, 16, 0, 0),
        DataType::NText => (99, 16, 0, 0),
        DataType::Binary(n) => (173, *n, 0, 0),
        DataType::VarBinary(n) => (165, *n, 0, 0),
        DataType::Image => (34, 16, 0, 0),
        DataType::Timestamp => (189, 8, 0, 0),
        DataType::Date => (40, 3, 10, 0),
        DataType::Time(s) => (41, 5, 16, *s as i32),
        DataType::DateTime => (61, 8, 23, 3),
        DataType::SmallDateTime => (58, 4, 16, 0),
        DataType::DateTime2(s) => (42, 8, 27, *s as i32),
        DataType::DateTimeOffset(s) => (43, 10, 34, *s as i32),
        DataType::Guid => (36, 16, 0, 0),
        DataType::Xml => (241, -1, 0, 0),
        _ => (98, 8016, 0, 0),
    }
}

fn type_name(t: &DataType) -> String {
    t.base_name().to_string()
}

impl<'a> Engine<'a> {
    pub fn sysview(&self, schema: &str, name: &str) -> Option<Rowset> {
        let db = self.srv.databases.get(&self.ses.db.to_lowercase())?;
        let dbname = db.name.clone();
        let sid = |s: &str| -> i32 {
            match s.to_lowercase().as_str() {
                "dbo" => 1,
                "guest" => 2,
                "information_schema" => 3,
                "sys" => 4,
                _ => 5 + db.schemas.keys().position(|k| *k == s.to_lowercase()).unwrap_or(0) as i32,
            }
        };
        let date = |d: chrono::NaiveDateTime| Value::DateTime(d, DtKind::DateTime);
        let lname = name.to_lowercase();
        if schema.eq_ignore_ascii_case("sys") {
            match lname.as_str() {
                "objects" | "all_objects" | "tables" | "views" | "procedures" | "sequences" | "system_objects" => {
                    let filter: Option<&str> = match lname.as_str() {
                        "tables" => Some("U"),
                        "views" => Some("V"),
                        "procedures" => Some("P"),
                        "sequences" => Some("SO"),
                        _ => None,
                    };
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Some(f) = filter {
                            if o.type_code() != f {
                                continue;
                            }
                        }
                        let parent = 0;
                        rows.push(vec![sv(o.name()), Value::Int(o.object_id()), Value::Int(sid(o.schema())), Value::Int(parent), sv(o.type_code()), sv(o.type_desc()), date(db.created), date(db.created)]);
                    }
                    if lname == "objects" || lname == "all_objects" {
                        for o in db.objects.values() {
                            if let Object::Table(t) = o {
                                let mut add = |n: &str, code: &str, desc: &str| {
                                    rows.push(vec![sv(n), Value::Int(t.object_id * 10 + rows.len() as i32 % 9 + 1), Value::Int(sid(&t.schema)), Value::Int(t.object_id), sv(code), sv(desc), date(db.created), date(db.created)]);
                                };
                                if let Some(pk) = &t.pk {
                                    add(&pk.name, "PK", "PRIMARY_KEY_CONSTRAINT");
                                }
                                for u in t.uniques.iter().filter(|u| !u.is_index) {
                                    add(&u.name, "UQ", "UNIQUE_CONSTRAINT");
                                }
                                for f in &t.fks {
                                    add(&f.name, "F", "FOREIGN_KEY_CONSTRAINT");
                                }
                                for ck in &t.checks {
                                    add(&ck.name, "C", "CHECK_CONSTRAINT");
                                }
                            }
                        }
                    }
                    for (tn, (tk, trg)) in &db.triggers {
                        let _ = tn;
                        if lname == "objects" || lname == "all_objects" {
                            let pid = match db.objects.get(tk) {
                                Some(Object::Table(t)) => t.object_id,
                                _ => 0,
                            };
                            rows.push(vec![sv(&trg.name), Value::Int(trg.object_id), Value::Int(sid(&trg.schema)), Value::Int(pid), sv("TR"), sv("SQL_TRIGGER"), date(db.created), date(db.created)]);
                        }
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("schema_id", DataType::Int), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime)], rows))
                }
                "columns" | "all_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                let (st, ml, p, s) = type_info(&c.ty);
                                rows.push(vec![Value::Int(t.object_id), sv(&c.name), Value::Int(i as i32 + 1), Value::Int(st), Value::Int(st), Value::Int(ml), Value::Int(p), Value::Int(s), Value::Bit(c.nullable), Value::Bit(c.identity.is_some()), Value::Bit(c.computed.is_some()), if c.ty.is_string() { sv(c.collation.as_deref().unwrap_or(&db.collation_or_default())) } else { Value::Null }]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("system_type_id", DataType::Int), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_nullable", DataType::Bit), ("is_identity", DataType::Bit), ("is_computed", DataType::Bit), ("collation_name", nv())], rows))
                }
                "security_policies" => {
                    let rows = db.policies.iter().map(|p| vec![sv(&p.name), Value::Int(p.object_id), Value::Bit(p.enabled), sv(&p.schema)]).collect();
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("is_enabled", DataType::Bit), ("schema_name", nv())], rows))
                }
                "security_predicates" => {
                    let mut rows = vec![];
                    for p in &db.policies {
                        for x in &p.preds {
                            let (kind, op) = x.kind.describe();
                            let target = db.objects.get(&x.table).map(|o| o.object_id()).unwrap_or(0);
                            rows.push(vec![Value::Int(p.object_id), Value::Int(target), sv(&format!("[{}].[{}]({})", x.func.0, x.func.1, x.args.join(", "))), sv(kind), if op.is_empty() { Value::Null } else { sv(op) }]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("target_object_id", DataType::Int), ("predicate_definition", nv()), ("predicate_type_desc", nv()), ("operation_desc", nv())], rows))
                }
                "masked_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                if let Some(f) = &c.mask {
                                    rows.push(vec![Value::Int(t.object_id), sv(&c.name), Value::Int(i as i32 + 1), Value::Bit(true), sv(f)]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("is_masked", DataType::Bit), ("masking_function", nv())], rows))
                }
                "identity_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            if let Some(ci) = t.identity_col() {
                                let (sd, inc) = t.columns[ci].identity.unwrap();
                                rows.push(vec![Value::Int(t.object_id), sv(&t.columns[ci].name), Value::Int(ci as i32 + 1), Value::BigInt(sd), Value::BigInt(inc), t.identity_next.get().map(|n| Value::BigInt(n - inc)).unwrap_or(Value::Null)]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("seed_value", DataType::BigInt), ("increment_value", DataType::BigInt), ("last_value", DataType::BigInt)], rows))
                }
                "schemas" => {
                    let rows = db.schemas.values().map(|s| vec![sv(s), Value::Int(sid(s)), Value::Int(1)]).collect();
                    Some(rs(&[("name", nv()), ("schema_id", DataType::Int), ("principal_id", DataType::Int)], rows))
                }
                "databases" => {
                    let rows = self
                        .srv
                        .databases
                        .values()
                        .map(|d| {
                            vec![
                                sv(&d.name),
                                Value::Int(d.id),
                                date(d.created),
                                sv(&d.collation),
                                Value::Int(160),
                                sv(if d.restoring { "RESTORING" } else { "ONLINE" }),
                                Value::Int(if d.recovery_full { 1 } else { 3 }),
                                sv(if d.recovery_full { "FULL" } else { "SIMPLE" }),
                            ]
                        })
                        .collect();
                    Some(rs(
                        &[("name", nv()), ("database_id", DataType::Int), ("create_date", DataType::DateTime), ("collation_name", nv()), ("compatibility_level", DataType::TinyInt), ("state_desc", nv()), ("recovery_model", DataType::TinyInt), ("recovery_model_desc", nv())],
                        rows,
                    ))
                }
                "types" | "systypes" => {
                    let builtin = [("int", 56), ("bigint", 127), ("smallint", 52), ("tinyint", 48), ("bit", 104), ("decimal", 106), ("numeric", 108), ("money", 60), ("smallmoney", 122), ("float", 62), ("real", 59), ("char", 175), ("varchar", 167), ("nchar", 239), ("nvarchar", 231), ("text", 35), ("ntext", 99), ("binary", 173), ("varbinary", 165), ("image", 34), ("date", 40), ("time", 41), ("datetime", 61), ("smalldatetime", 58), ("datetime2", 42), ("datetimeoffset", 43), ("uniqueidentifier", 36), ("xml", 241), ("sql_variant", 98), ("timestamp", 189)];
                    let mut rows: Vec<Row> = builtin.iter().map(|(n, id)| vec![sv(n), Value::Int(*id), Value::Int(*id), Value::Int(4), Value::Bit(false)]).collect();
                    for u in db.types.values() {
                        rows.push(vec![sv(&u.name), Value::Int(u.type_id), Value::Int(0), Value::Int(sid(&u.schema)), Value::Bit(true)]);
                    }
                    Some(rs(&[("name", nv()), ("user_type_id", DataType::Int), ("system_type_id", DataType::Int), ("schema_id", DataType::Int), ("is_user_defined", DataType::Bit)], rows))
                }
                "indexes" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut id = 1;
                            let row = |name: Value, id: i32, ty: &str, uniq: bool, pk: bool, f: &Option<crate::storage::FilterPred>| vec![Value::Int(t.object_id), name, Value::Int(id), sv(ty), Value::Bit(uniq), Value::Bit(pk), Value::Bit(f.is_some()), f.as_ref().map(|f| sv(&f.text)).unwrap_or(Value::Null)];
                            if t.pk.is_none() {
                                rows.push(row(Value::Null, 0, "HEAP", false, false, &None));
                            }
                            if let Some(pk) = &t.pk {
                                rows.push(row(sv(&pk.name), id, "CLUSTERED", true, true, &None));
                                id += 1;
                            }
                            for u in t.uniques.iter().filter(|u| !u.is_index) {
                                rows.push(row(sv(&u.name), id, "NONCLUSTERED", true, false, &None));
                                id += 1;
                            }
                            for i in &t.indexes {
                                rows.push(row(sv(&i.name), id, "NONCLUSTERED", i.unique, false, &i.filter));
                                id += 1;
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("index_id", DataType::Int), ("type_desc", nv()), ("is_unique", DataType::Bit), ("is_primary_key", DataType::Bit), ("has_filter", DataType::Bit), ("filter_definition", DataType::NVarChar(MAX))], rows))
                }
                "index_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut id = 1;
                            if let Some(pk) = &t.pk {
                                for (k, &c) in pk.cols.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(c as i32 + 1), Value::Bit(false), Value::Bit(false)]);
                                }
                                id += 1;
                            }
                            for u in t.uniques.iter().filter(|u| !u.is_index) {
                                for (k, &c) in u.cols.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(c as i32 + 1), Value::Bit(false), Value::Bit(false)]);
                                }
                                id += 1;
                            }
                            for i in &t.indexes {
                                for (k, (c, desc)) in i.cols.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(*c as i32 + 1), Value::Bit(*desc), Value::Bit(false)]);
                                }
                                for (k, &c) in i.include.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(0), Value::Int(c as i32 + 1), Value::Bit(false), Value::Bit(true)]);
                                    let _ = k;
                                }
                                id += 1;
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("index_column_id", DataType::Int), ("column_id", DataType::Int), ("is_descending_key", DataType::Bit), ("is_included_column", DataType::Bit)], rows))
                }
                "foreign_keys" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for f in &t.fks {
                                let rid = match db.objects.get(&f.ref_key) {
                                    Some(Object::Table(p)) => p.object_id,
                                    _ => 0,
                                };
                                rows.push(vec![sv(&f.name), Value::Int(t.object_id * 10 + 1), Value::Int(t.object_id), Value::Int(rid), sv(&f.on_delete.replace(' ', "_")), sv(&f.on_update.replace(' ', "_"))]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_object_id", DataType::Int), ("referenced_object_id", DataType::Int), ("delete_referential_action_desc", nv()), ("update_referential_action_desc", nv())], rows))
                }
                "check_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for c in &t.checks {
                                rows.push(vec![sv(&c.name), Value::Int(t.object_id), Value::Bit(!c.enabled)]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("parent_object_id", DataType::Int), ("is_disabled", DataType::Bit)], rows))
                }
                "key_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            if let Some(pk) = &t.pk {
                                rows.push(vec![sv(&pk.name), Value::Int(t.object_id), sv("PK"), sv("PRIMARY_KEY_CONSTRAINT")]);
                            }
                            for u in t.uniques.iter().filter(|u| !u.is_index) {
                                rows.push(vec![sv(&u.name), Value::Int(t.object_id), sv("UQ"), sv("UNIQUE_CONSTRAINT")]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv())], rows))
                }
                "sql_modules" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        let def = match o {
                            Object::View(v) => Some(v.text.clone()),
                            Object::Proc(p) => Some(p.text.clone()),
                            Object::Func(f) => Some(f.def.text.clone()),
                            _ => None,
                        };
                        if let Some(d) = def {
                            rows.push(vec![Value::Int(o.object_id()), sv(&d)]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("definition", DataType::NVarChar(MAX))], rows))
                }
                "database_principals" => {
                    let rows = db.security.principals.values().map(|p| vec![sv(&p.name), Value::Int(p.id), sv(if p.is_role { "R" } else { "S" }), sv(&p.default_schema)]).collect();
                    Some(rs(&[("name", nv()), ("principal_id", DataType::Int), ("type", DataType::Char(1)), ("default_schema_name", nv())], rows))
                }
                "server_principals" => {
                    let rows = self.srv.security.logins.values().enumerate().map(|(i, l)| vec![sv(&l.name), Value::Int(i as i32 + 1), sv("S"), Value::Bit(l.disabled)]).collect();
                    Some(rs(&[("name", nv()), ("principal_id", DataType::Int), ("type", DataType::Char(1)), ("is_disabled", DataType::Bit)], rows))
                }
                "database_permissions" => {
                    let rows = db
                        .security
                        .perms
                        .iter()
                        .map(|e| {
                            let pid = db.security.principals.get(&e.grantee).map(|p| p.id).unwrap_or(0);
                            let (class, major) = match &e.target {
                                crate::security::PermTarget::Database => ("DATABASE", 0),
                                crate::security::PermTarget::Schema(s) => ("SCHEMA", sid(s)),
                                crate::security::PermTarget::Object(k) | crate::security::PermTarget::Column(k, _) => ("OBJECT_OR_COLUMN", db.objects.get(k).map(|o| o.object_id()).unwrap_or(0)),
                            };
                            vec![sv(class), Value::Int(major), Value::Int(pid), sv(&e.perm), sv(if e.state == crate::security::PermState::Grant { "GRANT" } else { "DENY" })]
                        })
                        .collect();
                    Some(rs(&[("class_desc", nv()), ("major_id", DataType::Int), ("grantee_principal_id", DataType::Int), ("permission_name", nv()), ("state_desc", nv())], rows))
                }
                "database_role_members" => {
                    let rows = db
                        .security
                        .members
                        .iter()
                        .map(|(r, m)| vec![Value::Int(db.security.principals.get(r).map(|p| p.id).unwrap_or(0)), Value::Int(db.security.principals.get(m).map(|p| p.id).unwrap_or(0))])
                        .collect();
                    Some(rs(&[("role_principal_id", DataType::Int), ("member_principal_id", DataType::Int)], rows))
                }
                "dm_tran_locks" => {
                    let rows = self.srv.core.locks.snapshot().into_iter().map(|(r, tx, m)| {
                        let (rt, oid, rid) = match r {
                            crate::storage::lock::Res::Table(o) => ("OBJECT", o, 0),
                            crate::storage::lock::Res::Row(o, rid) => ("KEY", o, rid as i32),
                            crate::storage::lock::Res::Range(o, _) => ("KEY_RANGE", o, 0),
                        };
                        let mode = if rt == "KEY_RANGE" { "RangeS_S".to_string() } else { format!("{:?}", m) };
                        vec![Value::BigInt(tx as i64), sv(rt), Value::Int(oid), Value::Int(rid), sv(&mode)]
                    }).collect();
                    Some(rs(&[("request_owner_id", DataType::BigInt), ("resource_type", nv()), ("resource_associated_entity_id", DataType::Int), ("resource_lock_row", DataType::Int), ("request_mode", nv())], rows))
                }
                "dm_os_buffer_pool" => {
                    let s = self.srv.store.stats();
                    let t = self.srv.temp_store.stats();
                    let row = |name: &str, s: &crate::storage::StoreStats| vec![sv(name), Value::BigInt(s.pool_capacity as i64), Value::BigInt(s.pages_cached as i64), Value::BigInt(s.pages_live as i64), Value::BigInt(s.file_blocks as i64), Value::BigInt(s.free_blocks as i64), Value::BigInt(s.reads as i64), Value::BigInt(s.writes as i64), Value::BigInt(s.hits as i64), Value::BigInt(s.evictions as i64)];
                    Some(rs(&[("store", nv()), ("pool_pages", DataType::BigInt), ("cached_pages", DataType::BigInt), ("live_pages", DataType::BigInt), ("file_pages", DataType::BigInt), ("free_pages", DataType::BigInt), ("page_reads", DataType::BigInt), ("page_writes", DataType::BigInt), ("cache_hits", DataType::BigInt), ("evictions", DataType::BigInt)], vec![row("data", &s), row("temp", &t)]))
                }
                "dm_exec_sessions" => {
                    let rows = self
                        .srv
                        .core
                        .session_list()
                        .into_iter()
                        .map(|s| {
                            let dbid = self.srv.databases.get(&s.db.to_lowercase()).map(|d| d.id).unwrap_or(1);
                            vec![Value::Int(s.spid), sv(&s.login), sv(&s.host), sv(&s.app), Value::Int(dbid), Value::Int(s.isolation), sv(s.status), date(s.login_time), date(s.last_batch), Value::BigInt(s.batches as i64)]
                        })
                        .collect();
                    Some(rs(&[("session_id", DataType::SmallInt), ("login_name", nv()), ("host_name", nv()), ("program_name", nv()), ("database_id", DataType::SmallInt), ("transaction_isolation_level", DataType::SmallInt), ("status", nv()), ("login_time", DataType::DateTime), ("last_request_start_time", DataType::DateTime), ("batch_count", DataType::BigInt)], rows))
                }
                "dm_exec_requests" => {
                    let rows = self.srv.core.session_list().into_iter().filter(|s| s.status == "running").map(|s| vec![Value::Int(s.spid), sv(s.status), sv(&s.command), date(s.last_batch)]).collect();
                    Some(rs(&[("session_id", DataType::SmallInt), ("status", nv()), ("command", nv()), ("start_time", DataType::DateTime)], rows))
                }
                "dm_exec_query_stats" => {
                    let rows = self
                        .srv
                        .core
                        .stats_list()
                        .into_iter()
                        .map(|q| vec![sv(&q.text), Value::BigInt(q.executions as i64), Value::BigInt(q.total_us as i64), Value::BigInt(q.last_us as i64), Value::BigInt(q.max_us as i64), Value::BigInt(q.rows as i64), date(q.created), date(q.last)])
                        .collect();
                    Some(rs(&[("sql_text", DataType::NVarChar(4000)), ("execution_count", DataType::BigInt), ("total_elapsed_time", DataType::BigInt), ("last_elapsed_time", DataType::BigInt), ("max_elapsed_time", DataType::BigInt), ("total_rows", DataType::BigInt), ("creation_time", DataType::DateTime), ("last_execution_time", DataType::DateTime)], rows))
                }
                "dm_exec_cached_plans" => {
                    let mut e = super::parse_cache_entries();
                    e.sort_by(|a, b| b.1.cmp(&a.1));
                    let rows = e.into_iter().map(|(t, n)| vec![Value::BigInt(n as i64), sv("Compiled Plan"), sv("Adhoc"), Value::Int(t.len() as i32), sv(&t)]).collect();
                    Some(rs(&[("usecounts", DataType::BigInt), ("cacheobjtype", nv()), ("objtype", nv()), ("size_in_bytes", DataType::Int), ("text", DataType::NVarChar(MAX))], rows))
                }
                "configurations" => {
                    let rows = self.srv.core.config_list().into_iter().map(|o| vec![sv(o.name), Value::BigInt(o.config), Value::BigInt(o.minimum), Value::BigInt(o.maximum), Value::BigInt(o.run), Value::Bit(o.dynamic), Value::Bit(o.advanced), sv(o.description)]).collect();
                    Some(rs(&[("name", nv()), ("value", DataType::BigInt), ("minimum", DataType::BigInt), ("maximum", DataType::BigInt), ("value_in_use", DataType::BigInt), ("is_dynamic", DataType::Bit), ("is_advanced", DataType::Bit), ("description", nv())], rows))
                }
                "dm_os_sys_info" => Some(rs(&[("cpu_count", DataType::Int), ("sqlserver_start_time", DataType::DateTime)], vec![vec![Value::Int(std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(1)), date(self.srv.start_time)]])),
                _ => None,
            }
        } else {
            let cat = Value::Str(dbname.clone());
            match lname.as_str() {
                "tables" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        let tt = match o {
                            Object::Table(_) => "BASE TABLE",
                            Object::View(_) => "VIEW",
                            _ => continue,
                        };
                        rows.push(vec![cat.clone(), sv(o.schema()), sv(o.name()), sv(tt)]);
                    }
                    Some(rs(&[("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("TABLE_TYPE", DataType::VarChar(10))], rows))
                }
                "columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                let (_, ml, p, s) = type_info(&c.ty);
                                let is_num = c.ty.is_numeric();
                                let chars = if c.ty.is_string() { c.ty.length().map(Value::Int).unwrap_or(Value::Null) } else { Value::Null };
                                let _ = ml;
                                rows.push(vec![
                                    cat.clone(),
                                    sv(&t.schema),
                                    sv(&t.name),
                                    sv(&c.name),
                                    Value::Int(i as i32 + 1),
                                    Value::Null,
                                    sv(if c.nullable { "YES" } else { "NO" }),
                                    sv(&type_name(&c.ty)),
                                    chars,
                                    if is_num { Value::Int(p) } else { Value::Null },
                                    if is_num { Value::Int(s) } else { Value::Null },
                                    if c.ty.is_string() { sv(c.collation.as_deref().unwrap_or(crate::collation::DEFAULT_NAME)) } else { Value::Null },
                                ]);
                            }
                        }
                    }
                    Some(rs(&[("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("COLUMN_NAME", nv()), ("ORDINAL_POSITION", DataType::Int), ("COLUMN_DEFAULT", DataType::NVarChar(MAX)), ("IS_NULLABLE", DataType::VarChar(3)), ("DATA_TYPE", nv()), ("CHARACTER_MAXIMUM_LENGTH", DataType::Int), ("NUMERIC_PRECISION", DataType::TinyInt), ("NUMERIC_SCALE", DataType::Int), ("COLLATION_NAME", nv())], rows))
                }
                "views" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::View(v) = o {
                            rows.push(vec![cat.clone(), sv(&v.schema), sv(&v.name), sv(&v.text)]);
                        }
                    }
                    Some(rs(&[("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("VIEW_DEFINITION", DataType::NVarChar(MAX))], rows))
                }
                "routines" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        let (kind, def) = match o {
                            Object::Proc(p) => ("PROCEDURE", p.text.clone()),
                            Object::Func(f) => ("FUNCTION", f.def.text.clone()),
                            _ => continue,
                        };
                        rows.push(vec![cat.clone(), sv(o.schema()), sv(o.name()), sv(kind), sv(&def)]);
                    }
                    Some(rs(&[("ROUTINE_CATALOG", nv()), ("ROUTINE_SCHEMA", nv()), ("ROUTINE_NAME", nv()), ("ROUTINE_TYPE", nv()), ("ROUTINE_DEFINITION", DataType::NVarChar(MAX))], rows))
                }
                "schemata" => {
                    let rows = db.schemas.values().map(|s| vec![cat.clone(), sv(s), sv("dbo")]).collect();
                    Some(rs(&[("CATALOG_NAME", nv()), ("SCHEMA_NAME", nv()), ("SCHEMA_OWNER", nv())], rows))
                }
                "table_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut add = |n: &str, ty: &str| rows.push(vec![cat.clone(), sv(&t.schema), sv(n), cat.clone(), sv(&t.schema), sv(&t.name), sv(ty)]);
                            if let Some(pk) = &t.pk {
                                add(&pk.name, "PRIMARY KEY");
                            }
                            for u in t.uniques.iter().filter(|u| !u.is_index) {
                                add(&u.name, "UNIQUE");
                            }
                            for f in &t.fks {
                                add(&f.name, "FOREIGN KEY");
                            }
                            for c in &t.checks {
                                add(&c.name, "CHECK");
                            }
                        }
                    }
                    Some(rs(&[("CONSTRAINT_CATALOG", nv()), ("CONSTRAINT_SCHEMA", nv()), ("CONSTRAINT_NAME", nv()), ("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("CONSTRAINT_TYPE", DataType::VarChar(11))], rows))
                }
                "key_column_usage" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut keys: Vec<(&str, &Vec<usize>)> = vec![];
                            if let Some(pk) = &t.pk {
                                keys.push((&pk.name, &pk.cols));
                            }
                            for u in t.uniques.iter().filter(|u| !u.is_index) {
                                keys.push((&u.name, &u.cols));
                            }
                            for f in &t.fks {
                                keys.push((&f.name, &f.cols));
                            }
                            for (n, cols) in keys {
                                for (pos, &ci) in cols.iter().enumerate() {
                                    rows.push(vec![cat.clone(), sv(&t.schema), sv(n), cat.clone(), sv(&t.schema), sv(&t.name), sv(&t.columns[ci].name), Value::Int(pos as i32 + 1)]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("CONSTRAINT_CATALOG", nv()), ("CONSTRAINT_SCHEMA", nv()), ("CONSTRAINT_NAME", nv()), ("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("COLUMN_NAME", nv()), ("ORDINAL_POSITION", DataType::Int)], rows))
                }
                _ => None,
            }
        }
    }
}
