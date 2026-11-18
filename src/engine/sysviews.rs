//! Catalog views (sys.* and INFORMATION_SCHEMA.*), computed on demand.
use super::query::{ColMeta, Rowset, Schema};
use super::*;

fn rs(cols: &[(&str, DataType)], rows: Vec<Row>) -> Rowset {
    Rowset::new(Schema::new(cols.iter().map(|(n, t)| ColMeta { qual: None, name: n.to_string(), ty: Some(t.clone()), denied: None, hidden: false, coll: None, mask: None }).collect()), rows)
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
        // the user type id; its system type (the CLR types) is 240
        DataType::HierarchyId => (128, 892, 0, 0),
        DataType::Geometry => (129, -1, 0, 0),
        DataType::Geography => (130, -1, 0, 0),
        _ => (98, 8016, 0, 0),
    }
}

fn type_name(t: &DataType) -> String {
    t.base_name().to_string()
}

/// The columns of `sys.fn_get_audit_file` (a subset of SQL Server's) for a set of records.
pub fn audit_rowset(events: Vec<(String, crate::audit::AuditEvent)>) -> Rowset {
    let rows = events
        .into_iter()
        .map(|(file, e)| {
            vec![
                Value::DateTime(e.time, DtKind::DateTime2),
                Value::BigInt(e.seq),
                sv(&e.action_id),
                Value::Bit(e.succeeded),
                Value::Int(e.session_id),
                sv(&e.server_principal),
                sv(&e.db_principal),
                sv(&e.target_server_principal),
                sv(&e.target_db_principal),
                sv(&e.server_instance),
                sv(&e.database),
                sv(&e.schema),
                sv(&e.object),
                sv(&e.class_type),
                sv(&e.statement),
                sv(&e.info),
                sv(&e.app),
                sv(&e.host),
                sv(&e.audit),
                sv(&file),
            ]
        })
        .collect();
    rs(
        &[
            ("event_time", DataType::DateTime2(7)),
            ("sequence_number", DataType::BigInt),
            ("action_id", DataType::VarChar(4)),
            ("succeeded", DataType::Bit),
            ("session_id", DataType::SmallInt),
            ("server_principal_name", nv()),
            ("database_principal_name", nv()),
            ("target_server_principal_name", nv()),
            ("target_database_principal_name", nv()),
            ("server_instance_name", nv()),
            ("database_name", nv()),
            ("schema_name", nv()),
            ("object_name", nv()),
            ("class_type", DataType::VarChar(2)),
            ("statement", DataType::NVarChar(MAX)),
            ("additional_information", DataType::NVarChar(MAX)),
            ("application_name", nv()),
            ("host_name", nv()),
            ("audit_name", nv()),
            ("file_name", DataType::NVarChar(MAX)),
        ],
        rows,
    )
}

impl<'a> Engine<'a> {
    /// The schemas of the current database with the ids `sys.schemas` gives them.
    pub fn user_schema_names(&self) -> Vec<(i32, String)> {
        let Some(db) = self.srv.databases.get(&self.ses.db.to_lowercase()) else { return vec![] };
        db.schemas.iter().map(|(k, v)| (db.schema_id(k), v.clone())).collect()
    }

    pub fn sysview(&self, schema: &str, name: &str) -> Option<Rowset> {
        let db = self.srv.databases.get(&self.ses.db.to_lowercase())?;
        let dbname = db.name.clone();
        let sid = |s: &str| -> i32 { db.schema_id(&s.to_lowercase()) };
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
                        rows.push(vec![sv(o.name()), Value::Int(o.object_id()), Value::Int(sid(o.schema())), Value::Int(parent), sv(o.type_code()), sv(o.type_desc()), date(db.created), date(db.created), Value::Bit(false), Value::Null]);
                    }
                    if lname == "objects" || lname == "all_objects" {
                        for o in db.objects.values() {
                            if let Object::Table(t) = o {
                                let mut add = |n: &str, code: &str, desc: &str| {
                                    rows.push(vec![sv(n), Value::Int(t.object_id * 10 + rows.len() as i32 % 9 + 1), Value::Int(sid(&t.schema)), Value::Int(t.object_id), sv(code), sv(desc), date(db.created), date(db.created), Value::Bit(false), Value::Null]);
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
                            rows.push(vec![sv(&trg.name), Value::Int(trg.object_id), Value::Int(sid(&trg.schema)), Value::Int(pid), sv("TR"), sv("SQL_TRIGGER"), date(db.created), date(db.created), Value::Bit(false), Value::Null]);
                        }
                    }
                    { let mut cols: Vec<(&str, DataType)> = vec![("name", nv()), ("object_id", DataType::Int), ("schema_id", DataType::Int), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("principal_id", DataType::Int)];
                    if lname == "tables" {
                        for r in rows.iter_mut() {
                            let (node, edge) = match r.get(1) {
                                Some(Value::Int(oid)) => db.objects.values().find_map(|o| match o { Object::Table(t) if t.object_id == *oid => Some((t.graph == Some(crate::catalog::GraphKind::Node), t.graph == Some(crate::catalog::GraphKind::Edge))), _ => None }).unwrap_or((false, false)),
                                _ => (false, false),
                            };
                            r.push(Value::Bit(node));
                            r.push(Value::Bit(edge));
                        }
                        cols.push(("is_node", DataType::Bit));
                        cols.push(("is_edge", DataType::Bit));
                    }
                    Some(rs(&cols, rows)) }
                }
                "columns" | "all_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                let (st, ml, p, s) = type_info(&c.ty);
                                let (gt, gd) = match (t.graph, c.name.as_str()) {
                                    (Some(_), "$graph_id") => (Value::Int(1), sv("GRAPH_ID")),
                                    (Some(_), "$node_id") | (Some(_), "$edge_id") => (Value::Int(2), sv("GRAPH_ID_COMPUTED")),
                                    (Some(_), "$from_id") => (Value::Int(5), sv("GRAPH_FROM_ID_COMPUTED")),
                                    (Some(_), "$to_id") => (Value::Int(8), sv("GRAPH_TO_ID_COMPUTED")),
                                    _ => (Value::Null, Value::Null),
                                };
                                rows.push(vec![Value::Int(t.object_id), sv(&c.name), Value::Int(i as i32 + 1), Value::Int(if matches!(c.ty, DataType::HierarchyId | DataType::Geometry | DataType::Geography) { 240 } else { st }), Value::Int(c.user_type.as_ref().and_then(|ut| db.types.values().find(|u| matches!(u.kind, crate::catalog::UserTypeKind::Alias(..)) && (format!("{}.{}", u.schema, u.name).eq_ignore_ascii_case(ut) || (u.schema.eq_ignore_ascii_case("dbo") && u.name.eq_ignore_ascii_case(ut))))).map(|u| u.type_id).unwrap_or(st)), Value::Int(ml), Value::Int(p), Value::Int(s), Value::Bit(c.nullable), Value::Bit(c.identity.is_some()), Value::Bit(c.computed.is_some()), if c.ty.is_string() { sv(c.collation.as_deref().unwrap_or(&db.collation_or_default())) } else { Value::Null }, Value::Int(if c.default_name.is_some() { t.object_id * 1000 + 500 + i as i32 } else { 0 }), gt, gd]);
                            }
                        }
                    }
                    { let cols: Vec<(&str, DataType)> = vec![("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("system_type_id", DataType::Int), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_nullable", DataType::Bit), ("is_identity", DataType::Bit), ("is_computed", DataType::Bit), ("collation_name", nv()), ("default_object_id", DataType::Int), ("graph_type", DataType::TinyInt), ("graph_type_desc", nv())];
                    Some(rs(&cols, rows)) }
                }
                "parameters" | "all_parameters" => {
                    // the parameters of procedures and functions (the return value of a function is not listed)
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        let (oid, params): (i32, &[crate::ast::Param]) = match o {
                            Object::Proc(p) => (p.object_id, &p.params[..]),
                            Object::Func(f) => (f.object_id, &f.def.params[..]),
                            _ => continue,
                        };
                        for (i, p) in params.iter().enumerate() {
                            let dt = p.ty.parts.last().and_then(|n| DataType::from_name(n, &p.ty.args, false).ok());
                            let (st, ml, pr, sc) = dt.as_ref().map(type_info).unwrap_or((0, 0, 0, 0));
                            let nm = if p.name.starts_with('@') { p.name.clone() } else { format!("@{}", p.name) };
                            rows.push(vec![Value::Int(oid), sv(&nm), Value::Int(i as i32 + 1), Value::Int(st), Value::Int(st), Value::Int(ml), Value::Int(pr), Value::Int(sc), Value::Bit(p.out), Value::Bit(p.default.is_some()), Value::Bit(p.readonly)]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("parameter_id", DataType::Int), ("system_type_id", DataType::Int), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_output", DataType::Bit), ("has_default_value", DataType::Bit), ("is_readonly", DataType::Bit)], rows))
                }
                "xml_schema_collections" => {
                    let rows = db.xml_schemas.iter().map(|x| vec![Value::Int(x.id), Value::Int(db.schema_id(&x.schema)), Value::Null, sv(&x.name), Value::DateTime(x.created, DtKind::DateTime), Value::DateTime(x.created, DtKind::DateTime)]).collect();
                    Some(rs(&[("xml_collection_id", DataType::Int), ("schema_id", DataType::Int), ("principal_id", DataType::Int), ("name", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime)], rows))
                }
                "xml_schema_namespaces" => {
                    let mut rows = vec![];
                    for x in &db.xml_schemas {
                        let ns = crate::engine::xmlschema::compiled(&x.texts).map(|c| c.namespaces().to_vec()).unwrap_or_default();
                        for (i, n) in ns.iter().enumerate() {
                            rows.push(vec![Value::Int(x.id), sv(n), Value::Int(i as i32 + 1)]);
                        }
                    }
                    Some(rs(&[("xml_collection_id", DataType::Int), ("name", nv()), ("xml_namespace_id", DataType::Int)], rows))
                }
                "fulltext_catalogs" => {
                    let rows = db.fulltext_catalogs.iter().enumerate().map(|(i, c)| vec![Value::Int(i as i32 + 5), sv(&c.name), Value::Null, Value::Bit(c.is_default), Value::Bit(c.accent_sensitive), Value::Int(1), Value::Int(0), Value::Int(1), Value::Bit(false)]).collect();
                    Some(rs(&[("fulltext_catalog_id", DataType::Int), ("name", nv()), ("path", nv()), ("is_default", DataType::Bit), ("is_accent_sensitivity_on", DataType::Bit), ("data_space_id", DataType::Int), ("file_id", DataType::Int), ("principal_id", DataType::Int), ("is_importing", DataType::Bit)], rows))
                }
                "fulltext_indexes" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            if let Some(f) = &t.fulltext {
                                let cat = f.catalog.as_ref().and_then(|c| db.fulltext_catalogs.iter().position(|x| x.name.eq_ignore_ascii_case(c))).map(|p| Value::Int(p as i32 + 5)).unwrap_or(Value::Null);
                                let key = t.index_specs().iter().position(|s| s.name.eq_ignore_ascii_case(&f.key_index)).map(|p| p as i32 + 1).unwrap_or(1);
                                let (st, sd) = match f.tracking.as_str() {
                                    "MANUAL" => ("M", "MANUAL"),
                                    "OFF" => ("O", "OFF"),
                                    _ => ("A", "AUTO"),
                                };
                                rows.push(vec![Value::Int(t.object_id), Value::Int(key), cat, Value::Bit(f.enabled), sv(st), sv(sd), Value::Bit(true), Value::Null]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("unique_index_id", DataType::Int), ("fulltext_catalog_id", DataType::Int), ("is_enabled", DataType::Bit), ("change_tracking_state", DataType::Char(1)), ("change_tracking_state_desc", nv()), ("has_crawl_completed", DataType::Bit), ("crawl_type", DataType::Char(1))], rows))
                }
                "fulltext_index_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            if let Some(f) = &t.fulltext {
                                for (c, lang) in &f.cols {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(*c as i32 + 1), Value::Null, Value::Int(*lang), Value::Bit(false)]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("column_id", DataType::Int), ("type_column_id", DataType::Int), ("language_id", DataType::Int), ("statistical_semantics", DataType::Bit)], rows))
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
                "computed_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                if c.computed.is_some() {
                                    rows.push(vec![Value::Int(t.object_id), sv(&c.name), Value::Int(i as i32 + 1), sv(c.computed_text.as_deref().unwrap_or("")), Value::Bit(false)]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("definition", nv()), ("is_persisted", DataType::Bit)], rows))
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
                            let state = if d.restoring { 1 } else { 0 };
                            let b = |x: bool| Value::Bit(x);
                            vec![
                                sv(&d.name),
                                Value::Int(d.id),
                                Value::Null,
                                Value::Bin(vec![1]),
                                date(d.created),
                                Value::Int(160),
                                sv(&d.collation),
                                Value::Int(0),
                                sv("MULTI_USER"),
                                b(false),
                                b(false),
                                b(false),
                                Value::Int(state),
                                sv(if d.restoring { "RESTORING" } else { "ONLINE" }),
                                b(false),
                                b(true),
                                Value::Int(if d.allow_snapshot { 1 } else { 0 }),
                                sv(if d.allow_snapshot { "ON" } else { "OFF" }),
                                b(d.rcsi),
                                Value::Int(if d.recovery_full { 1 } else { 3 }),
                                sv(if d.recovery_full { "FULL" } else { "SIMPLE" }),
                                Value::Int(2),
                                sv("CHECKSUM"),
                                b(true),
                                b(true),
                                b(false),
                                b(false),
                                b(true),
                                b(true),
                                b(true),
                                b(true),
                                b(false),
                                b(true),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                Value::Int(0),
                                sv("NOTHING"),
                                b(false),
                                b(false),
                                b(false),
                                b(false),
                                Value::Int(0),
                                sv("NONE"),
                                Value::Int(60),
                                Value::Int(0),
                                sv("DISABLED"),
                                b(false),
                            ]
                        })
                        .collect();
                    Some(rs(
                        &[
                            ("name", nv()),
                            ("database_id", DataType::Int),
                            ("source_database_id", DataType::Int),
                            ("owner_sid", DataType::VarBinary(85)),
                            ("create_date", DataType::DateTime),
                            ("compatibility_level", DataType::TinyInt),
                            ("collation_name", nv()),
                            ("user_access", DataType::TinyInt),
                            ("user_access_desc", nv()),
                            ("is_read_only", DataType::Bit),
                            ("is_auto_close_on", DataType::Bit),
                            ("is_auto_shrink_on", DataType::Bit),
                            ("state", DataType::TinyInt),
                            ("state_desc", nv()),
                            ("is_in_standby", DataType::Bit),
                            ("is_cleanly_shutdown", DataType::Bit),
                            ("snapshot_isolation_state", DataType::TinyInt),
                            ("snapshot_isolation_state_desc", nv()),
                            ("is_read_committed_snapshot_on", DataType::Bit),
                            ("recovery_model", DataType::TinyInt),
                            ("recovery_model_desc", nv()),
                            ("page_verify_option", DataType::TinyInt),
                            ("page_verify_option_desc", nv()),
                            ("is_auto_create_stats_on", DataType::Bit),
                            ("is_auto_update_stats_on", DataType::Bit),
                            ("is_auto_update_stats_async_on", DataType::Bit),
                            ("is_ansi_null_default_on", DataType::Bit),
                            ("is_ansi_nulls_on", DataType::Bit),
                            ("is_ansi_padding_on", DataType::Bit),
                            ("is_ansi_warnings_on", DataType::Bit),
                            ("is_arithabort_on", DataType::Bit),
                            ("is_concat_null_yields_null_on", DataType::Bit),
                            ("is_numeric_roundabort_on", DataType::Bit),
                            ("is_quoted_identifier_on", DataType::Bit),
                            ("is_recursive_triggers_on", DataType::Bit),
                            ("is_cursor_close_on_commit_on", DataType::Bit),
                            ("is_local_cursor_default", DataType::Bit),
                            ("is_fulltext_enabled", DataType::Bit),
                            ("is_trustworthy_on", DataType::Bit),
                            ("is_db_chaining_on", DataType::Bit),
                            ("is_parameterization_forced", DataType::Bit),
                            ("is_master_key_encrypted_by_server", DataType::Bit),
                            ("is_published", DataType::Bit),
                            ("is_subscribed", DataType::Bit),
                            ("is_merge_published", DataType::Bit),
                            ("is_distributor", DataType::Bit),
                            ("log_reuse_wait", DataType::TinyInt),
                            ("log_reuse_wait_desc", nv()),
                            ("is_broker_enabled", DataType::Bit),
                            ("is_date_correlation_on", DataType::Bit),
                            ("is_cdc_enabled", DataType::Bit),
                            ("is_encrypted", DataType::Bit),
                            ("containment", DataType::TinyInt),
                            ("containment_desc", nv()),
                            ("target_recovery_time_in_seconds", DataType::Int),
                            ("delayed_durability", DataType::Int),
                            ("delayed_durability_desc", nv()),
                            ("is_ledger_on", DataType::Bit),
                        ],
                        rows,
                    ))
                }
                "sql_logins" | "server_principals" => {
                    // the logins, then the fixed server roles; a login's id is its place in the list
                    let sql_only = lname == "sql_logins";
                    let mut rows: Vec<Vec<Value>> = vec![];
                    for (i, l) in self.srv.security.logins.values().enumerate() {
                        let ty = match l.external.as_deref() {
                            Some("WINDOWS") => "U",
                            Some(_) => "E",
                            None => "S",
                        };
                        if sql_only && ty != "S" {
                            continue;
                        }
                        let desc = match ty {
                            "U" => "WINDOWS_LOGIN",
                            "E" => "EXTERNAL_LOGIN",
                            _ => "SQL_LOGIN",
                        };
                        let mut r = vec![sv(&l.name), Value::Int(i as i32 + 1), Value::Bin(l.name.to_lowercase().into_bytes()), sv(ty), sv(desc), Value::Bit(l.disabled), date(self.srv.start_time), date(self.srv.start_time), sv(&l.default_db), sv("us_english"), Value::Null, Value::Null, Value::Bit(false)];
                        if sql_only {
                            // sql_logins: no sid column or type_desc split, and its own columns for the policy
                            r.truncate(13);
                            r.push(Value::Bit(l.check_expiration));
                            r.push(Value::Bit(l.check_policy));
                            r.push(Value::Bin(l.hash.clone()));
                        }
                        rows.push(r);
                    }
                    if !sql_only {
                        let base = rows.len() as i32;
                        for (k, role) in ["sysadmin", "securityadmin", "serveradmin", "setupadmin", "processadmin", "diskadmin", "dbcreator", "bulkadmin"].iter().enumerate() {
                            rows.push(vec![sv(role), Value::Int(base + k as i32 + 1), Value::Bin(role.as_bytes().to_vec()), sv("R"), sv("SERVER_ROLE"), Value::Bit(false), date(self.srv.start_time), date(self.srv.start_time), Value::Null, Value::Null, Value::Null, Value::Null, Value::Bit(true)]);
                        }
                    }
                    let mut cols = vec![("name", nv()), ("principal_id", DataType::Int), ("sid", DataType::VarBinary(85)), ("type", DataType::Char(1)), ("type_desc", nv()), ("is_disabled", DataType::Bit), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("default_database_name", nv()), ("default_language_name", nv()), ("credential_id", DataType::Int), ("owning_principal_id", DataType::Int), ("is_fixed_role", DataType::Bit)];
                    if sql_only {
                        cols.push(("is_expiration_checked", DataType::Bit));
                        cols.push(("is_policy_checked", DataType::Bit));
                        cols.push(("password_hash", DataType::VarBinary(256)));
                    }
                    Some(rs(&cols, rows))
                }
                "server_role_members" => {
                    // every sysadmin login is a member of the sysadmin role (the first fixed role)
                    let n = self.srv.security.logins.len() as i32;
                    let rows = self.srv.security.logins.values().enumerate().filter(|(_, l)| l.name.eq_ignore_ascii_case("sa")).map(|(i, _)| vec![Value::Int(n + 1), Value::Int(i as i32 + 1)]).collect();
                    Some(rs(&[("role_principal_id", DataType::Int), ("member_principal_id", DataType::Int)], rows))
                }
                "server_permissions" => Some(rs(&[("class", DataType::TinyInt), ("class_desc", nv()), ("major_id", DataType::Int), ("minor_id", DataType::Int), ("grantee_principal_id", DataType::Int), ("grantor_principal_id", DataType::Int), ("type", DataType::Char(4)), ("permission_name", nv()), ("state", DataType::Char(1)), ("state_desc", nv())], vec![])),
                "servers" => Some(rs(
                    &[("server_id", DataType::Int), ("name", nv()), ("product", nv()), ("provider", nv()), ("data_source", nv()), ("is_linked", DataType::Bit), ("is_remote_login_enabled", DataType::Bit), ("is_rpc_out_enabled", DataType::Bit), ("is_data_access_enabled", DataType::Bit), ("modify_date", DataType::DateTime)],
                    vec![vec![Value::Int(0), sv(&self.srv.server_name), sv("SQL Server"), sv("SQLNCLI"), sv(&self.srv.server_name), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), date(self.srv.start_time)]],
                )),
                "endpoints" => Some(rs(
                    &[("name", nv()), ("endpoint_id", DataType::Int), ("principal_id", DataType::Int), ("protocol", DataType::TinyInt), ("protocol_desc", nv()), ("type", DataType::TinyInt), ("type_desc", nv()), ("state", DataType::TinyInt), ("state_desc", nv()), ("is_admin_endpoint", DataType::Bit)],
                    vec![vec![sv("TSQL Default TCP"), Value::Int(4), Value::Int(1), Value::Int(2), sv("TCP"), Value::Int(2), sv("TSQL"), Value::Int(0), sv("STARTED"), Value::Bit(false)]],
                )),
                "master_files" => {
                    // a data file and a log file for each database, as the engine reports them (see `database_files`)
                    let mut rows = vec![];
                    for d in self.srv.databases.values() {
                        for (k, (kind, desc, ext)) in [(0, "ROWS", "mdf"), (1, "LOG", "ldf")].iter().enumerate() {
                            rows.push(vec![Value::Int(d.id), Value::Int(k as i32 + 1), sv(&format!("{}{}", d.name, if k == 0 { "" } else { "_log" })), sv(&format!("{}.{}", d.name, ext)), Value::Int(*kind), sv(desc), Value::Int(128), Value::Int(-1), Value::Int(0), sv("ONLINE")]);
                        }
                    }
                    Some(rs(&[("database_id", DataType::Int), ("file_id", DataType::Int), ("name", nv()), ("physical_name", nv()), ("type", DataType::TinyInt), ("type_desc", nv()), ("size", DataType::Int), ("max_size", DataType::Int), ("growth", DataType::Int), ("state_desc", nv())], rows))
                }
                "dm_exec_connections" => {
                    let rows = self
                        .srv
                        .core
                        .session_list()
                        .into_iter()
                        .map(|s| vec![Value::Int(s.spid), Value::Int(s.spid), date(s.login_time), sv("TCP"), sv("TSQL"), sv("SQL"), Value::Int(4096), Value::Int(s.batches as i32), Value::Int(s.batches as i32), date(s.last_batch), date(s.last_batch), Value::Null, Value::Null, Value::Null])
                        .collect();
                    Some(rs(
                        &[("session_id", DataType::Int), ("most_recent_session_id", DataType::Int), ("connect_time", DataType::DateTime), ("net_transport", nv()), ("protocol_type", nv()), ("auth_scheme", nv()), ("net_packet_size", DataType::Int), ("num_reads", DataType::Int), ("num_writes", DataType::Int), ("last_read", DataType::DateTime), ("last_write", DataType::DateTime), ("client_net_address", nv()), ("local_net_address", nv()), ("local_tcp_port", DataType::Int)],
                        rows,
                    ))
                }
                // views that SSMS lists in Object Explorer or the server properties: this server has none of what they describe
                "assemblies" => Some(rs(&[("name", nv()), ("principal_id", DataType::Int), ("assembly_id", DataType::Int), ("clr_name", nv()), ("permission_set", DataType::TinyInt), ("permission_set_desc", nv()), ("is_visible", DataType::Bit), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime)], vec![])),
                "availability_replicas" => Some(rs(&[("replica_id", DataType::Guid), ("group_id", DataType::Guid), ("replica_metadata_id", DataType::Int), ("replica_server_name", nv()), ("owner_sid", DataType::VarBinary(85)), ("endpoint_url", nv()), ("availability_mode", DataType::TinyInt), ("availability_mode_desc", nv()), ("failover_mode", DataType::TinyInt), ("failover_mode_desc", nv())], vec![])),
                "dm_hadr_database_replica_states" => Some(rs(&[("database_id", DataType::Int), ("group_id", DataType::Guid), ("replica_id", DataType::Guid), ("group_database_id", DataType::Guid), ("is_local", DataType::Bit), ("synchronization_state", DataType::TinyInt), ("synchronization_state_desc", nv()), ("is_suspended", DataType::Bit), ("database_state", DataType::TinyInt), ("database_state_desc", nv())], vec![])),
                // one row per database, none of them mirrored
                "database_mirroring" => {
                    let rows = self.srv.databases.values().map(|d| vec![Value::Int(d.id), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null]).collect();
                    Some(rs(&[("database_id", DataType::Int), ("mirroring_guid", DataType::Guid), ("mirroring_state", DataType::TinyInt), ("mirroring_state_desc", nv()), ("mirroring_role", DataType::TinyInt), ("mirroring_role_desc", nv()), ("mirroring_role_sequence", DataType::Int), ("mirroring_safety_level", DataType::TinyInt), ("mirroring_safety_level_desc", nv()), ("mirroring_safety_sequence", DataType::Int), ("mirroring_partner_name", nv()), ("mirroring_partner_instance", nv())], rows))
                }
                "availability_groups" => Some(rs(&[("group_id", DataType::Guid), ("name", nv()), ("resource_id", nv()), ("failure_condition_level", DataType::Int), ("cluster_type", DataType::TinyInt), ("cluster_type_desc", nv())], vec![])),
                "dm_hadr_availability_replica_states" => Some(rs(&[("replica_id", DataType::Guid), ("group_id", DataType::Guid), ("is_local", DataType::Bit), ("role", DataType::TinyInt), ("role_desc", nv()), ("operational_state", DataType::TinyInt), ("operational_state_desc", nv()), ("connected_state", DataType::TinyInt), ("connected_state_desc", nv()), ("synchronization_health", DataType::TinyInt), ("synchronization_health_desc", nv())], vec![])),
                "dm_db_mirroring_connections" => Some(rs(&[("connection_id", DataType::Guid), ("transport_stream_id", DataType::Guid), ("state", DataType::TinyInt), ("state_desc", nv()), ("connect_time", DataType::DateTime), ("login_state", DataType::TinyInt), ("login_state_desc", nv())], vec![])),
                "resource_governor_configuration" => Some(rs(&[("classifier_function_id", DataType::Int), ("is_reconfiguration_pending", DataType::Bit), ("max_outstanding_io_per_volume", DataType::Int)], vec![vec![Value::Int(0), Value::Bit(false), Value::Int(0)]])),
                "traces" => Some(rs(&[("id", DataType::Int), ("status", DataType::Int), ("path", nv()), ("max_size", DataType::BigInt), ("stop_time", DataType::DateTime), ("max_files", DataType::Int), ("is_rowset", DataType::Bit), ("is_default", DataType::Bit)], vec![])),
                "trace_events" => Some(rs(&[("trace_event_id", DataType::SmallInt), ("category_id", DataType::SmallInt), ("name", nv())], vec![])),
                "credentials" => Some(rs(&[("credential_id", DataType::Int), ("name", nv()), ("credential_identity", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("target_type", nv()), ("target_id", DataType::Int)], vec![])),
                "linked_logins" => Some(rs(&[("server_id", DataType::Int), ("local_principal_id", DataType::Int), ("uses_self_credential", DataType::Bit), ("remote_name", nv()), ("modify_date", DataType::DateTime)], vec![])),
                "backup_devices" => Some(rs(&[("name", nv()), ("type", DataType::TinyInt), ("type_desc", nv()), ("physical_name", nv())], vec![])),
                "change_tracking_databases" => Some(rs(&[("database_id", DataType::Int), ("is_auto_cleanup_on", DataType::TinyInt), ("retention_period", DataType::Int), ("retention_period_units", DataType::TinyInt), ("retention_period_units_desc", nv())], vec![])),
                "service_broker_endpoints" => Some(rs(&[("name", nv()), ("endpoint_id", DataType::Int), ("principal_id", DataType::Int), ("protocol", DataType::TinyInt), ("protocol_desc", nv()), ("type", DataType::TinyInt), ("type_desc", nv()), ("state", DataType::TinyInt), ("state_desc", nv()), ("is_admin_endpoint", DataType::Bit), ("port", DataType::Int), ("is_dynamic_port", DataType::Bit), ("ip_address", nv())], vec![])),
                "sysdatabases" => {
                    let rows = self.srv.databases.values().map(|d| vec![sv(&d.name), Value::Int(d.id), Value::Bin(vec![1]), Value::Int(0), Value::Int(16), Value::Int(0), date(d.created), date(d.created), Value::Int(0), Value::Int(160), sv(&format!("{}.mdf", d.name)), Value::Int(957)]).collect();
                    Some(rs(&[("name", nv()), ("dbid", DataType::SmallInt), ("sid", DataType::VarBinary(85)), ("mode", DataType::SmallInt), ("status", DataType::Int), ("status2", DataType::Int), ("crdate", DataType::DateTime), ("reserved", DataType::DateTime), ("category", DataType::Int), ("cmptlevel", DataType::TinyInt), ("filename", nv()), ("version", DataType::SmallInt)], rows))
                }
                "dm_os_performance_counters" => Some(rs(&[("object_name", nv()), ("counter_name", nv()), ("instance_name", nv()), ("cntr_value", DataType::BigInt), ("cntr_type", DataType::Int)], vec![])),
                "dm_os_wait_stats" => Some(rs(&[("wait_type", nv()), ("waiting_tasks_count", DataType::BigInt), ("wait_time_ms", DataType::BigInt), ("max_wait_time_ms", DataType::BigInt), ("signal_wait_time_ms", DataType::BigInt)], vec![])),
                "dm_os_sys_memory" => Some(rs(
                    &[("total_physical_memory_kb", DataType::BigInt), ("available_physical_memory_kb", DataType::BigInt), ("total_page_file_kb", DataType::BigInt), ("available_page_file_kb", DataType::BigInt), ("system_memory_state_desc", nv())],
                    vec![vec![Value::BigInt(16 * 1024 * 1024), Value::BigInt(8 * 1024 * 1024), Value::BigInt(32 * 1024 * 1024), Value::BigInt(16 * 1024 * 1024), sv("Available physical memory is high")]],
                )),
                "dm_os_process_memory" => {
                    let s = self.srv.store.stats();
                    Some(rs(
                        &[("physical_memory_in_use_kb", DataType::BigInt), ("virtual_address_space_committed_kb", DataType::BigInt), ("process_physical_memory_low", DataType::Bit), ("process_virtual_memory_low", DataType::Bit)],
                        vec![vec![Value::BigInt(s.pages_cached as i64 * 8), Value::BigInt(s.pages_cached as i64 * 8), Value::Bit(false), Value::Bit(false)]],
                    ))
                }
                "dm_server_services" => Some(rs(
                    &[("servicename", nv()), ("startup_type", DataType::Int), ("startup_type_desc", nv()), ("status", DataType::Int), ("status_desc", nv()), ("process_id", DataType::Int), ("last_startup_time", DataType::DateTime)],
                    vec![vec![sv("SQL Server"), Value::Int(2), sv("Automatic"), Value::Int(4), sv("Running"), Value::Int(std::process::id() as i32), date(self.srv.start_time)]],
                )),
                "types" | "systypes" => {
                    let builtin = [("int", 56), ("bigint", 127), ("smallint", 52), ("tinyint", 48), ("bit", 104), ("decimal", 106), ("numeric", 108), ("money", 60), ("smallmoney", 122), ("float", 62), ("real", 59), ("char", 175), ("varchar", 167), ("nchar", 239), ("nvarchar", 231), ("text", 35), ("ntext", 99), ("binary", 173), ("varbinary", 165), ("image", 34), ("date", 40), ("time", 41), ("datetime", 61), ("smalldatetime", 58), ("datetime2", 42), ("datetimeoffset", 43), ("uniqueidentifier", 36), ("xml", 241), ("sql_variant", 98), ("timestamp", 189), ("hierarchyid", 128), ("geometry", 129), ("geography", 130)];
                    // (max_length, precision, scale) of each system type
                    let size = |n: &str| -> (i32, i32, i32) {
                        match n {
                            "int" => (4, 10, 0),
                            "bigint" => (8, 19, 0),
                            "smallint" => (2, 5, 0),
                            "tinyint" => (1, 3, 0),
                            "bit" => (1, 1, 0),
                            "decimal" | "numeric" => (17, 18, 0),
                            "money" => (8, 19, 4),
                            "smallmoney" => (4, 10, 4),
                            "float" => (8, 53, 0),
                            "real" => (4, 24, 0),
                            "char" | "varchar" | "nchar" | "nvarchar" | "binary" | "varbinary" => (8000, 0, 0),
                            "text" | "ntext" | "image" => (16, 0, 0),
                            "date" => (3, 10, 0),
                            "time" => (5, 16, 7),
                            "datetime" => (8, 23, 3),
                            "smalldatetime" => (4, 16, 0),
                            "datetime2" => (8, 27, 7),
                            "datetimeoffset" => (10, 34, 7),
                            "uniqueidentifier" => (16, 0, 0),
                            "xml" => (-1, 0, 0),
                            "sql_variant" => (8016, 0, 0),
                            "hierarchyid" => (892, 0, 0),
                            "geometry" | "geography" => (-1, 0, 0),
                            _ => (8, 0, 0),
                        }
                    };
                    let mut rows: Vec<Row> = builtin
                        .iter()
                        .map(|(n, id)| {
                            let (ml, p, sc) = size(n);
                            vec![sv(n), Value::Int(*id), Value::Int(*id), Value::Int(4), Value::Bit(false), Value::Int(ml), Value::Int(p), Value::Int(sc), Value::Bit(true)]
                        })
                        .collect();
                    for u in db.types.values() {
                        // (an alias type has the system type and the size of its base type)
                        let (base, ml, p, sc, nullable) = match &u.kind {
                            crate::catalog::UserTypeKind::Alias(t, n) => {
                                let (st, ml, p, sc) = type_info(t);
                                (st, ml, p, sc, *n)
                            }
                            _ => (0, 0, 0, 0, true),
                        };
                        rows.push(vec![sv(&u.name), Value::Int(u.type_id), Value::Int(base), Value::Int(sid(&u.schema)), Value::Bit(true), Value::Int(ml), Value::Int(p), Value::Int(sc), Value::Bit(nullable)]);
                    }
                    Some(rs(&[("name", nv()), ("user_type_id", DataType::Int), ("system_type_id", DataType::Int), ("schema_id", DataType::Int), ("is_user_defined", DataType::Bit), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_nullable", DataType::Bit)], rows))
                }
                "indexes" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut id = 1;
                            let row = |name: Value, id: i32, ty: &str, uniq: bool, pk: bool, f: &Option<crate::storage::FilterPred>| {
                                let code = match ty {
                                    "HEAP" => 0,
                                    "CLUSTERED" => 1,
                                    "CLUSTERED COLUMNSTORE" => 5,
                                    "NONCLUSTERED COLUMNSTORE" => 6,
                                    _ => 2,
                                };
                                let uq_constraint = uniq && !pk && !f.is_some() && matches!(&name, Value::Str(n) if t.uniques.iter().any(|u| !u.is_index && u.name == *n));
                                vec![Value::Int(t.object_id), name, Value::Int(id), sv(ty), Value::Bit(uniq), Value::Bit(pk), Value::Bit(f.is_some()), f.as_ref().map(|f| sv(&f.text)).unwrap_or(Value::Null), Value::Int(code), Value::Bit(uq_constraint), Value::Bit(false), Value::Int(0)]
                            };
                            // a clustered columnstore index stands where the heap or clustered index would: the key of the table is nonclustered
                            let cci = t.columnstore.as_ref().filter(|c| c.clustered);
                            if t.pk.is_none() && cci.is_none() {
                                rows.push(row(Value::Null, 0, "HEAP", false, false, &None));
                            }
                            if let Some(c) = cci {
                                let mut r = row(sv(&c.name), id, "CLUSTERED COLUMNSTORE", false, false, &None);
                                r[10] = Value::Bit(c.disabled);
                                rows.push(r);
                                id += 1;
                            }
                            if let Some(pk) = &t.pk {
                                rows.push(row(sv(&pk.name), id, if cci.is_some() { "NONCLUSTERED" } else { "CLUSTERED" }, true, true, &None));
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
                            if let Some(c) = t.columnstore.as_ref().filter(|c| !c.clustered) {
                                let mut r = row(sv(&c.name), id, "NONCLUSTERED COLUMNSTORE", false, false, &None);
                                r[10] = Value::Bit(c.disabled);
                                rows.push(r);
                                id += 1;
                            }
                            // XML indexes are numbered from 256000
                            for (k, x) in t.xml_indexes.iter().enumerate() {
                                let mut r = row(sv(&x.name), 256_000 + k as i32, "XML", false, false, &None);
                                r[8] = Value::Int(3);
                                rows.push(r);
                            }
                            // spatial indexes are numbered from 257000
                            for (k, x) in t.spatial_indexes.iter().enumerate() {
                                let mut r = row(sv(&x.name), 257_000 + k as i32, "SPATIAL", false, false, &None);
                                r[8] = Value::Int(4);
                                rows.push(r);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("index_id", DataType::Int), ("type_desc", nv()), ("is_unique", DataType::Bit), ("is_primary_key", DataType::Bit), ("has_filter", DataType::Bit), ("filter_definition", DataType::NVarChar(MAX)), ("type", DataType::TinyInt), ("is_unique_constraint", DataType::Bit), ("is_disabled", DataType::Bit), ("fill_factor", DataType::TinyInt)], rows))
                }
                "spatial_indexes" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, x) in t.spatial_indexes.iter().enumerate() {
                                let scheme = format!("{}_{}", if x.geography { "GEOGRAPHY" } else { "GEOMETRY" }, if x.auto { "AUTO_GRID" } else { "GRID" });
                                rows.push(vec![Value::Int(t.object_id), sv(&x.name), Value::Int(257_000 + k as i32), Value::Int(4), sv("SPATIAL"), Value::Int(if x.geography { 2 } else { 1 }), sv(if x.geography { "GEOGRAPHY" } else { "GEOMETRY" }), Value::Int(if x.auto { 3 } else { if x.geography { 2 } else { 1 } }), sv(&scheme)]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("index_id", DataType::Int), ("type", DataType::TinyInt), ("type_desc", nv()), ("spatial_index_type", DataType::TinyInt), ("spatial_index_type_desc", nv()), ("tessellation_scheme_id", DataType::TinyInt), ("tessellation_scheme", nv())], rows))
                }
                "spatial_index_tessellations" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, x) in t.spatial_indexes.iter().enumerate() {
                                let scheme = format!("{}_{}", if x.geography { "GEOGRAPHY" } else { "GEOMETRY" }, if x.auto { "AUTO_GRID" } else { "GRID" });
                                let b = |i: usize| x.bbox.map(|b| Value::Float(b[i])).unwrap_or(Value::Null);
                                let level = |n: u8| sv(match n { 1 => "LOW", 3 => "HIGH", _ => "MEDIUM" });
                                let mut r = vec![Value::Int(t.object_id), Value::Int(257_000 + k as i32), sv(&scheme), b(0), b(1), b(2), b(3)];
                                for g in x.grids {
                                    r.push(Value::Int(g as i32));
                                    r.push(level(g));
                                }
                                r.push(Value::Int(x.cells));
                                rows.push(r);
                            }
                        }
                    }
                    let mut cols = vec![("object_id", DataType::Int), ("index_id", DataType::Int), ("tessellation_scheme", nv()), ("bounding_box_xmin", DataType::Float), ("bounding_box_ymin", DataType::Float), ("bounding_box_xmax", DataType::Float), ("bounding_box_ymax", DataType::Float)];
                    for n in ["level_1_grid", "level_1_grid_desc", "level_2_grid", "level_2_grid_desc", "level_3_grid", "level_3_grid_desc", "level_4_grid", "level_4_grid_desc"] {
                        cols.push((n, if n.ends_with("desc") { nv() } else { DataType::TinyInt }));
                    }
                    cols.push(("cells_per_object", DataType::Int));
                    Some(rs(&cols, rows))
                }
                "xml_indexes" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, x) in t.xml_indexes.iter().enumerate() {
                                let using = x.using.as_ref().and_then(|u| t.xml_indexes.iter().position(|p| p.name.eq_ignore_ascii_case(u))).map(|p| Value::Int(256_000 + p as i32)).unwrap_or(Value::Null);
                                let (st, sd) = match x.secondary {
                                    Some('V') => (sv("V"), sv("VALUE")),
                                    Some('P') => (sv("P"), sv("PATH")),
                                    Some('R') => (sv("R"), sv("PROPERTY")),
                                    _ => (Value::Null, Value::Null),
                                };
                                rows.push(vec![
                                    Value::Int(t.object_id),
                                    sv(&x.name),
                                    Value::Int(256_000 + k as i32),
                                    Value::Int(3),
                                    sv("XML"),
                                    Value::Bit(false),
                                    Value::Bit(false),
                                    Value::Bit(false),
                                    Value::Bit(false),
                                    Value::Int(if x.primary { 0 } else { 1 }),
                                    sv(if x.primary { "PRIMARY_XML" } else { "SECONDARY_XML" }),
                                    using,
                                    st,
                                    sd,
                                ]);
                            }
                        }
                    }
                    Some(rs(
                        &[
                            ("object_id", DataType::Int),
                            ("name", nv()),
                            ("index_id", DataType::Int),
                            ("type", DataType::TinyInt),
                            ("type_desc", nv()),
                            ("is_unique", DataType::Bit),
                            ("is_primary_key", DataType::Bit),
                            ("is_unique_constraint", DataType::Bit),
                            ("is_disabled", DataType::Bit),
                            ("xml_index_type", DataType::TinyInt),
                            ("xml_index_type_description", nv()),
                            ("using_xml_index_id", DataType::Int),
                            ("secondary_type", DataType::Char(1)),
                            ("secondary_type_desc", nv()),
                        ],
                        rows,
                    ))
                }
                "index_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut id = 1;
                            if let Some(c) = t.columnstore.as_ref().filter(|c| c.clustered) {
                                for k in 0..t.columns.len() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(k as i32 + 1), Value::Bit(false), Value::Bit(false), Value::Int(0), Value::Int(0)]);
                                }
                                let _ = c;
                                id += 1;
                            }
                            if let Some(pk) = &t.pk {
                                for (k, &c) in pk.cols.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(c as i32 + 1), Value::Bit(false), Value::Bit(false), Value::Int(k as i32 + 1), Value::Int(0)]);
                                }
                                id += 1;
                            }
                            for u in t.uniques.iter().filter(|u| !u.is_index) {
                                for (k, &c) in u.cols.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(c as i32 + 1), Value::Bit(false), Value::Bit(false), Value::Int(k as i32 + 1), Value::Int(0)]);
                                }
                                id += 1;
                            }
                            for (k, x) in t.xml_indexes.iter().enumerate() {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(256_000 + k as i32), Value::Int(1), Value::Int(x.col as i32 + 1), Value::Bit(false), Value::Bit(false), Value::Int(0), Value::Int(0)]);
                            }
                            for (k, x) in t.spatial_indexes.iter().enumerate() {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(257_000 + k as i32), Value::Int(1), Value::Int(x.col as i32 + 1), Value::Bit(false), Value::Bit(false), Value::Int(0), Value::Int(0)]);
                            }
                            for i in &t.indexes {
                                for (k, (c, desc)) in i.cols.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(*c as i32 + 1), Value::Bit(*desc), Value::Bit(false), Value::Int(k as i32 + 1), Value::Int(0)]);
                                }
                                for (k, &c) in i.include.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(0), Value::Int(c as i32 + 1), Value::Bit(false), Value::Bit(true), Value::Int(0), Value::Int(0)]);
                                    let _ = k;
                                }
                                id += 1;
                            }
                            if let Some(c) = t.columnstore.as_ref().filter(|c| !c.clustered) {
                                for (k, &col) in c.cols.iter().enumerate() {
                                    rows.push(vec![Value::Int(t.object_id), Value::Int(id), Value::Int(k as i32 + 1), Value::Int(col as i32 + 1), Value::Bit(false), Value::Bit(false), Value::Int(0), Value::Int(0)]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("index_column_id", DataType::Int), ("column_id", DataType::Int), ("is_descending_key", DataType::Bit), ("is_included_column", DataType::Bit), ("key_ordinal", DataType::TinyInt), ("partition_ordinal", DataType::TinyInt)], rows))
                }
                "foreign_keys" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, f) in t.fks.iter().enumerate() {
                                let rid = match db.objects.get(&f.ref_key) {
                                    Some(Object::Table(p)) => p.object_id,
                                    _ => 0,
                                };
                                rows.push(vec![sv(&f.name), Value::Int(t.object_id * 1000 + 1 + k as i32), Value::Int(t.object_id), Value::Int(rid), sv(&f.on_delete.replace(' ', "_")), sv(&f.on_update.replace(' ', "_"))]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_object_id", DataType::Int), ("referenced_object_id", DataType::Int), ("delete_referential_action_desc", nv()), ("update_referential_action_desc", nv())], rows))
                }
                "edge_constraints" | "edge_constraint_clauses" => {
                    let clauses = lname == "edge_constraint_clauses";
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, c) in t.edge_constraints.iter().enumerate() {
                                let oid = t.object_id * 100 + 50 + k as i32;
                                let object_of = |key: &(String, String)| db.objects.values().find_map(|o| match o { Object::Table(x) if x.schema.to_lowercase() == key.0 && x.name.to_lowercase() == key.1 => Some(x.object_id), _ => None }).unwrap_or(0);
                                if clauses {
                                    for (n, (f, to)) in c.pairs.iter().enumerate() {
                                        rows.push(vec![Value::Int(oid), Value::Int(n as i32 + 1), Value::Int(object_of(f)), Value::Int(object_of(to))]);
                                    }
                                } else {
                                    rows.push(vec![sv(&c.name), Value::Int(oid), Value::Int(sid(&t.schema)), Value::Int(t.object_id), sv("EC"), sv("EDGE_CONSTRAINT"), date(db.created), date(db.created), Value::Bit(false), Value::Bit(false), Value::Int(if c.cascade { 1 } else { 0 }), sv(if c.cascade { "CASCADE" } else { "NO_ACTION" })]);
                                }
                            }
                        }
                    }
                    Some(if clauses {
                        rs(&[("object_id", DataType::Int), ("clause_number", DataType::Int), ("from_object_id", DataType::Int), ("to_object_id", DataType::Int)], rows)
                    } else {
                        rs(&[("name", nv()), ("object_id", DataType::Int), ("schema_id", DataType::Int), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("is_disabled", DataType::Bit), ("delete_referential_action", DataType::TinyInt), ("delete_referential_action_desc", nv())], rows)
                    })
                }
                "column_store_row_groups" | "dm_db_column_store_row_group_physical_stats" => {
                    // the row groups of each columnstore index: the compressed ones, and the delta store (the rows not in them yet)
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let Some(def) = &t.columnstore else { continue };
                            let Some(cs) = t.data.columnstore_snapshot() else { continue };
                            let index_id = if def.clustered { 1 } else { 2 + t.indexes.len() as i32 + i32::from(t.pk.is_some()) + t.uniques.iter().filter(|u| !u.is_index).count() as i32 };
                            for (k, g) in cs.data.groups.iter().enumerate() {
                                let (lo, hi) = (g.rowids[0], g.rowids[g.rows() - 1]);
                                let deleted = cs.dirty[cs.dirty.partition_point(|r| *r < lo)..cs.dirty.partition_point(|r| *r <= hi)].iter().filter(|r| g.rowids.binary_search(r).is_ok()).count();
                                rows.push(vec![Value::Int(t.object_id), Value::Int(index_id), Value::Int(1), Value::Int(k as i32), Value::Null, Value::Int(3), sv("COMPRESSED"), Value::BigInt(g.rows() as i64), Value::BigInt(deleted as i64), Value::BigInt(g.bytes() as i64)]);
                            }
                            let delta = t.data.slot_count().saturating_sub(cs.data.slots);
                            if delta > 0 {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(index_id), Value::Int(1), Value::Int(cs.data.groups.len() as i32), Value::BigInt(t.object_id as i64 * 10 + 1), Value::Int(1), sv("OPEN"), Value::BigInt(delta as i64), Value::BigInt(0), Value::BigInt(delta as i64 * 64)]);
                            }
                        }
                    }
                    Some(rs(
                        &[("object_id", DataType::Int), ("index_id", DataType::Int), ("partition_number", DataType::Int), ("row_group_id", DataType::Int), ("delta_store_hobt_id", DataType::BigInt), ("state", DataType::TinyInt), ("state_description", nv()), ("total_rows", DataType::BigInt), ("deleted_rows", DataType::BigInt), ("size_in_bytes", DataType::BigInt)],
                        rows,
                    ))
                }
                "column_store_segments" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let Some(def) = &t.columnstore else { continue };
                            let Some(cs) = t.data.columnstore_snapshot() else { continue };
                            let index_id = if def.clustered { 1 } else { 2 + t.indexes.len() as i32 + i32::from(t.pk.is_some()) + t.uniques.iter().filter(|u| !u.is_index).count() as i32 };
                            let pid = t.object_id as i64 * 10 + index_id as i64;
                            for (k, g) in cs.data.groups.iter().enumerate() {
                                for (p, seg) in g.cols.iter().enumerate() {
                                    use crate::storage::colstore::SegData;
                                    let (enc, dict) = match &seg.data {
                                        SegData::Nulls | SegData::Int(..) | SegData::Float(_) | SegData::Dec(..) => (1, -1),
                                        SegData::Dict { dict, .. } => (if dict.iter().any(|v| matches!(v, Value::Str(_))) { 3 } else { 2 }, 0),
                                        SegData::Vals(_) => (4, -1),
                                    };
                                    let bound = |v: &Option<Value>| match v {
                                        Some(Value::Int(i)) => Value::BigInt(*i as i64),
                                        Some(Value::BigInt(i)) => Value::BigInt(*i),
                                        _ => Value::Null,
                                    };
                                    rows.push(vec![Value::BigInt(pid), Value::BigInt(pid), Value::Int(cs.data.colmap[p] as i32 + 1), Value::Int(k as i32), Value::Int(1), Value::Int(enc), Value::Int(seg.rows as i32), Value::Int(i32::from(seg.null_count > 0)), Value::Int(dict), bound(&seg.min), bound(&seg.max), Value::BigInt(seg.bytes() as i64)]);
                                }
                            }
                        }
                    }
                    Some(rs(
                        &[("partition_id", DataType::BigInt), ("hobt_id", DataType::BigInt), ("column_id", DataType::Int), ("segment_id", DataType::Int), ("version", DataType::Int), ("encoding_type", DataType::Int), ("row_count", DataType::Int), ("has_nulls", DataType::Int), ("primary_dictionary_id", DataType::Int), ("min_data_id", DataType::BigInt), ("max_data_id", DataType::BigInt), ("on_disk_size", DataType::BigInt)],
                        rows,
                    ))
                }
                "partitions" | "dm_db_partition_stats" => {
                    // one partition per table (heap or clustered index); the sizes are estimates from the row count
                    let stats = lname == "dm_db_partition_stats";
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let n = t.data.slot_count() as i64;
                            let pages = (n / 40 + 1).max(1);
                            let cci = t.columnstore.as_ref().is_some_and(|c| c.clustered);
                            let idx = if t.pk.is_some() || cci { 1 } else { 0 };
                            if stats {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(idx), Value::Int(1), Value::BigInt(n), Value::BigInt(pages), Value::BigInt(pages), Value::BigInt(pages)]);
                            } else {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(idx), Value::Int(1), Value::BigInt(n), Value::Int(if cci { 3 } else { 0 }), Value::BigInt(t.object_id as i64 * 10 + idx as i64)]);
                            }
                        }
                    }
                    Some(if stats {
                        rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("partition_number", DataType::Int), ("row_count", DataType::BigInt), ("used_page_count", DataType::BigInt), ("reserved_page_count", DataType::BigInt), ("in_row_data_page_count", DataType::BigInt)], rows)
                    } else {
                        rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("partition_number", DataType::Int), ("rows", DataType::BigInt), ("data_compression", DataType::TinyInt), ("partition_id", DataType::BigInt)], rows)
                    })
                }
                "database_files" => Some(rs(
                    &[("file_id", DataType::Int), ("name", nv()), ("physical_name", nv()), ("type_desc", nv()), ("state_desc", nv()), ("size", DataType::Int), ("max_size", DataType::Int), ("growth", DataType::Int)],
                    vec![vec![Value::Int(1), sv(&dbname), sv(&format!("{}.mdf", dbname)), sv("ROWS"), sv("ONLINE"), Value::Int(128), Value::Int(-1), Value::Int(0)]],
                )),
                "foreign_key_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, f) in t.fks.iter().enumerate() {
                                let refd = match db.objects.get(&f.ref_key) {
                                    Some(Object::Table(p)) => Some(p),
                                    _ => None,
                                };
                                for (i, c) in f.cols.iter().enumerate() {
                                    let ref_col = refd.and_then(|p| f.ref_cols.get(i).and_then(|n| p.col_index(n))).map(|x| x as i32 + 1).unwrap_or(0);
                                    rows.push(vec![Value::Int(t.object_id * 1000 + 1 + k as i32), Value::Int(i as i32 + 1), Value::Int(t.object_id), Value::Int(*c as i32 + 1), Value::Int(refd.map(|p| p.object_id).unwrap_or(0)), Value::Int(ref_col)]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("constraint_object_id", DataType::Int), ("constraint_column_id", DataType::Int), ("parent_object_id", DataType::Int), ("parent_column_id", DataType::Int), ("referenced_object_id", DataType::Int), ("referenced_column_id", DataType::Int)], rows))
                }
                "default_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                if let (Some(name), Some(text)) = (&c.default_name, &c.default_text) {
                                    rows.push(vec![sv(name), Value::Int(t.object_id * 1000 + 500 + i as i32), Value::Int(t.object_id), Value::Int(i as i32 + 1), sv(&format!("({})", text)), sv("D"), sv("DEFAULT_CONSTRAINT"), Value::Int(sid(&t.schema))]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_object_id", DataType::Int), ("parent_column_id", DataType::Int), ("definition", nv()), ("type", DataType::Char(2)), ("type_desc", nv()), ("schema_id", DataType::Int)], rows))
                }
                "synonyms" => {
                    let rows = db
                        .objects
                        .values()
                        .filter_map(|o| match o {
                            Object::Synonym(s) => Some(vec![sv(&s.name), Value::Int(s.object_id), Value::Int(sid(&s.schema)), sv(&s.target.iter().map(|p| format!("[{}]", p)).collect::<Vec<_>>().join(".")), sv("SN")]),
                            _ => None,
                        })
                        .collect();
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("schema_id", DataType::Int), ("base_object_name", nv()), ("type", DataType::Char(2))], rows))
                }
                // descriptions (MS_Description and the like) are not kept: clients ask for them and get none
                "extended_properties" => Some(rs(&[("class", DataType::TinyInt), ("class_desc", nv()), ("major_id", DataType::Int), ("minor_id", DataType::Int), ("name", nv()), ("value", DataType::Variant)], vec![])),
                "check_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, c) in t.checks.iter().enumerate() {
                                let col = c.col.as_ref().and_then(|n| t.columns.iter().position(|x| x.name.eq_ignore_ascii_case(n))).map(|i| i as i32 + 1).unwrap_or(0);
                                rows.push(vec![sv(&c.name), Value::Int(t.object_id), Value::Bit(!c.enabled), Value::Int(t.object_id * 1000 + 700 + k as i32), sv(&c.text), Value::Int(col), sv("C"), sv("CHECK_CONSTRAINT")]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("parent_object_id", DataType::Int), ("is_disabled", DataType::Bit), ("object_id", DataType::Int), ("definition", nv()), ("parent_column_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv())], rows))
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
                "server_audits" => {
                    let rows = self
                        .srv
                        .security
                        .audits
                        .iter()
                        .map(|a| {
                            let onf = match a.on_failure.as_str() {
                                "SHUTDOWN" => (2, "SHUTDOWN SERVER INSTANCE"),
                                "FAIL_OPERATION" => (1, "FAIL OPERATION"),
                                _ => (0, "CONTINUE"),
                            };
                            vec![Value::Int(a.id), sv(&a.name), date(a.created), sv(a.target.code()), sv(a.target.desc()), Value::Int(onf.0), sv(onf.1), Value::Bit(a.enabled), Value::Int(a.queue_delay), a.filter.as_ref().map(|(t, _)| sv(t)).unwrap_or(Value::Null)]
                        })
                        .collect();
                    Some(rs(&[("audit_id", DataType::Int), ("name", nv()), ("create_date", DataType::DateTime), ("type", DataType::Char(2)), ("type_desc", nv()), ("on_failure", DataType::TinyInt), ("on_failure_desc", nv()), ("is_state_enabled", DataType::Bit), ("queue_delay", DataType::Int), ("predicate", DataType::NVarChar(MAX))], rows))
                }
                "server_file_audits" => {
                    let rows = self
                        .srv
                        .security
                        .audits
                        .iter()
                        .filter_map(|a| crate::audit::AuditSink::file_for(a).map(|f| (a, f)))
                        .map(|(a, f)| vec![Value::Int(a.id), sv(&a.name), sv(&f.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()), sv(&f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())])
                        .collect();
                    Some(rs(&[("audit_id", DataType::Int), ("name", nv()), ("log_file_path", DataType::NVarChar(MAX)), ("log_file_name", nv())], rows))
                }
                "dm_server_audit_status" => {
                    let rows = self
                        .srv
                        .security
                        .audits
                        .iter()
                        .map(|a| {
                            let failure = self.srv.core.audit.failure(&a.name);
                            let (status, desc) = if !a.enabled { (0, "STOPPED") } else if failure.is_some() { (2, "FAILED") } else { (1, "STARTED") };
                            let size = crate::audit::AuditSink::file_for(a).and_then(|f| std::fs::metadata(f).ok()).map(|m| m.len() as i64).unwrap_or(0);
                            vec![Value::Int(a.id), sv(&a.name), Value::Int(status), sv(desc), crate::audit::AuditSink::file_for(a).map(|f| sv(&f.to_string_lossy())).unwrap_or(Value::Null), Value::BigInt(size)]
                        })
                        .collect();
                    Some(rs(&[("audit_id", DataType::Int), ("name", nv()), ("status", DataType::SmallInt), ("status_desc", nv()), ("audit_file_path", DataType::NVarChar(MAX)), ("audit_file_size", DataType::BigInt)], rows))
                }
                "server_audit_specifications" | "database_audit_specifications" => {
                    let specs: &[crate::audit::AuditSpec] = if lname == "server_audit_specifications" { &self.srv.security.server_specs } else { &db.audit_specs };
                    let rows = specs
                        .iter()
                        .map(|s| {
                            let guid = self.srv.security.audits.iter().find(|a| a.name.eq_ignore_ascii_case(&s.audit)).map(|a| sv(&a.guid)).unwrap_or(Value::Null);
                            vec![Value::Int(s.id), sv(&s.name), guid, Value::Bit(s.enabled)]
                        })
                        .collect();
                    let idname = if lname == "server_audit_specifications" { "server_specification_id" } else { "database_specification_id" };
                    Some(rs(&[(idname, DataType::Int), ("name", nv()), ("audit_guid", nv()), ("is_state_enabled", DataType::Bit)], rows))
                }
                "server_audit_specification_details" | "database_audit_specification_details" => {
                    let server = lname == "server_audit_specification_details";
                    let specs: &[crate::audit::AuditSpec] = if server { &self.srv.security.server_specs } else { &db.audit_specs };
                    let mut rows: Vec<Row> = vec![];
                    for s in specs {
                        for i in &s.items {
                            match i {
                                crate::audit::AuditItem::Group(g) => rows.push(vec![Value::Int(s.id), sv(g), sv(if server { "SERVER" } else { "DATABASE" }), Value::Null, Value::Null, Value::Bit(true)]),
                                crate::audit::AuditItem::Action { action, object, principals } => {
                                    let (class, obj) = match object {
                                        crate::audit::ObjSel::Database(d) => ("DATABASE", d.clone()),
                                        crate::audit::ObjSel::Schema(s) => ("SCHEMA", s.clone()),
                                        crate::audit::ObjSel::Object(p) => ("OBJECT_OR_COLUMN", p.join(".")),
                                    };
                                    rows.push(vec![Value::Int(s.id), sv(action), sv(class), sv(&obj), sv(&principals.join(",")), Value::Bit(false)]);
                                }
                            }
                        }
                    }
                    let idname = if server { "server_specification_id" } else { "database_specification_id" };
                    Some(rs(&[(idname, DataType::Int), ("audit_action_name", nv()), ("class_desc", nv()), ("object_name", nv()), ("audited_principal_name", nv()), ("is_group", DataType::Bit)], rows))
                }
                "dm_audit_log" => Some(audit_rowset(self.srv.core.audit.ring().into_iter().map(|e| (String::new(), e)).collect())),
                "triggers" => {
                    // DML triggers (parent = table, class 1) and database DDL triggers (class 0)
                    let mut rows: Vec<Row> = vec![];
                    for (tk, trg) in db.triggers.values() {
                        let pid = match db.objects.get(tk) {
                            Some(Object::Table(t)) => t.object_id,
                            Some(Object::View(v)) => v.object_id,
                            _ => 0,
                        };
                        rows.push(vec![sv(&trg.name), Value::Int(trg.object_id), Value::Int(1), sv("OBJECT_OR_COLUMN"), Value::Int(pid), sv("TR"), Value::Bit(trg.disabled), Value::Bit(trg.instead)]);
                    }
                    for t in &db.ddl_triggers {
                        rows.push(vec![sv(&t.name), Value::Int(t.object_id), Value::Int(0), sv("DATABASE"), Value::Int(0), sv("TR"), Value::Bit(t.disabled), Value::Bit(false)]);
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_class", DataType::TinyInt), ("parent_class_desc", nv()), ("parent_id", DataType::Int), ("type", DataType::Char(2)), ("is_disabled", DataType::Bit), ("is_instead_of_trigger", DataType::Bit)], rows))
                }
                "trigger_events" => {
                    let mut rows: Vec<Row> = vec![];
                    for t in &db.ddl_triggers {
                        for e in &t.events {
                            rows.push(vec![Value::Int(t.object_id), sv(e)]);
                        }
                    }
                    for (_, trg) in db.triggers.values() {
                        for e in &trg.events {
                            rows.push(vec![Value::Int(trg.object_id), sv(e)]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("type_desc", nv())], rows))
                }
                "server_triggers" => {
                    let rows = self.srv.security.server_triggers.iter().map(|t| vec![sv(&t.name), Value::Int(t.object_id), sv("SERVER"), Value::Bit(t.disabled)]).collect();
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_class_desc", nv()), ("is_disabled", DataType::Bit)], rows))
                }
                "server_trigger_events" => {
                    let mut rows: Vec<Row> = vec![];
                    for t in &self.srv.security.server_triggers {
                        for e in &t.events {
                            rows.push(vec![Value::Int(t.object_id), sv(e)]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("type_desc", nv())], rows))
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
                "dm_os_sys_info" => {
                    let cpus = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(1);
                    Some(rs(
                        &[("cpu_count", DataType::Int), ("hyperthread_ratio", DataType::Int), ("scheduler_count", DataType::Int), ("max_workers_count", DataType::Int), ("sqlserver_start_time", DataType::DateTime), ("affinity_type", DataType::Int), ("affinity_type_desc", nv())],
                        vec![vec![Value::Int(cpus), Value::Int(1), Value::Int(cpus), Value::Int(cpus * 64), date(self.srv.start_time), Value::Int(2), sv("AUTO")]],
                    ))
                }
                // the machine the server runs on, as SQL Server shows it (SSMS reads it when it connects)
                "dm_os_host_info" => {
                    let (platform, distribution, release, sku) = match std::env::consts::OS {
                        "windows" => ("Windows", "Windows", "10.0", 48),
                        "macos" => ("Linux", "macOS", "", 0),
                        _ => ("Linux", "Linux", "", 0),
                    };
                    Some(rs(
                        &[("host_platform", nv()), ("host_distribution", nv()), ("host_release", nv()), ("host_service_pack_level", nv()), ("host_sku", DataType::Int), ("os_language_version", DataType::Int)],
                        vec![vec![sv(platform), sv(distribution), sv(release), sv(""), Value::Int(sku), Value::Int(1033)]],
                    ))
                }
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
