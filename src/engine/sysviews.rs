//! Catalog views (sys.* and INFORMATION_SCHEMA.*), computed on demand.
use super::query::{ColMeta, Rowset, Schema};
use super::*;

/// One reference of an object to another (see `Engine::dependency_rows`).
pub struct DependencyRow {
    pub referencing_id: i32,
    pub referencing_schema: String,
    pub referencing_name: String,
    pub ref_schema: String,
    pub ref_name: String,
    pub ref_id: Option<i32>,
}

/// A constraint of a table as an object of the catalog (`sys.objects`, `sysobjects`, `OBJECT_ID`): the ids are those the
/// views of each kind (`sys.key_constraints`, `foreign_keys`, `check_constraints`, `default_constraints`) give.
pub struct ConstraintObject {
    pub name: String,
    pub id: i32,
    pub schema: String,
    pub parent: i32,
    pub code: &'static str,
    pub desc: &'static str,
}

pub fn constraint_objects(db: &crate::catalog::Database) -> Vec<ConstraintObject> {
    let mut out = vec![];
    for o in db.objects.values() {
        if let Object::Table(t) = o {
            let mut add = |name: &str, id: i32, code: &'static str, desc: &'static str| out.push(ConstraintObject { name: name.to_string(), id, schema: t.schema.clone(), parent: t.object_id, code, desc });
            if let Some(pk) = &t.pk {
                add(&pk.name, t.object_id * 1000 + 900, "PK", "PRIMARY_KEY_CONSTRAINT");
            }
            for (n, u) in t.uniques.iter().filter(|u| !u.is_index).enumerate() {
                add(&u.name, t.object_id * 1000 + 901 + n as i32, "UQ", "UNIQUE_CONSTRAINT");
            }
            for (k, f) in t.fks.iter().enumerate() {
                add(&f.name, t.object_id * 1000 + 1 + k as i32, "F", "FOREIGN_KEY_CONSTRAINT");
            }
            for (k, ck) in t.checks.iter().enumerate() {
                add(&ck.name, t.object_id * 1000 + 700 + k as i32, "C", "CHECK_CONSTRAINT");
            }
            for (i, c) in t.columns.iter().enumerate() {
                if let Some(dn) = &c.default_name {
                    add(dn, t.object_id * 1000 + 500 + i as i32, "D", "DEFAULT_CONSTRAINT");
                }
            }
        }
    }
    out
}

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

    /// What the views, procedures, functions and triggers of the current database refer to (the tables and views named in their
    /// definitions): for `sys.sql_expression_dependencies` and `sys.dm_sql_referencing_entities`.
    pub fn dependency_rows(&self) -> Vec<DependencyRow> {
        use crate::rewrite::astutil::referenced_tables_of;
        let Some(db) = self.srv.databases.get(&self.ses.db.to_lowercase()) else { return vec![] };
        let mut out: Vec<DependencyRow> = vec![];
        let mut add = |referencing_id: i32, schema: &str, name: &str, refs: Vec<String>| {
            for id in refs {
                let (rs_, rn) = id.split_once('.').unwrap_or(("dbo", id.as_str()));
                let found = db.objects.get(&id).filter(|o| matches!(o, Object::Table(_) | Object::View(_)));
                let (ref_schema, ref_name, ref_id) = match found {
                    Some(o) => (o.schema().to_string(), o.name().to_string(), Some(o.object_id())),
                    None => (rs_.to_string(), rn.to_string(), None),
                };
                if ref_id == Some(referencing_id) {
                    continue;
                }
                out.push(DependencyRow { referencing_id, referencing_schema: schema.to_string(), referencing_name: name.to_string(), ref_schema, ref_name, ref_id });
            }
        };
        for o in db.objects.values() {
            match o {
                Object::View(v) => add(v.object_id, &v.schema, &v.name, referenced_tables_of(&*v.query)),
                Object::Proc(p) => add(p.object_id, &p.schema, &p.name, referenced_tables_of(&*p.body)),
                Object::Func(f) => add(f.object_id, &f.schema, &f.name, referenced_tables_of(&*f.def)),
                _ => {}
            }
        }
        for (_, trg) in db.triggers.values() {
            add(trg.object_id, &trg.schema, &trg.name, referenced_tables_of(&*trg.body));
        }
        out
    }

    pub fn sysview(&self, schema: &str, name: &str) -> Option<Rowset> {
        let db = self.srv.databases.get(&self.ses.db.to_lowercase())?;
        let dbname = db.name.clone();
        let sid = |s: &str| -> i32 { db.schema_id(&s.to_lowercase()) };
        let date = |d: chrono::NaiveDateTime| Value::DateTime(d, DtKind::DateTime);
        let lname = name.to_lowercase();
        if schema.eq_ignore_ascii_case("sys") {
            match lname.as_str() {
                "objects" | "all_objects" | "tables" | "views" | "all_views" | "procedures" | "sequences" | "system_objects" => {
                    let filter: Option<&str> = match lname.as_str() {
                        "tables" => Some("U"),
                        "views" | "all_views" => Some("V"),
                        "procedures" => Some("P"),
                        "sequences" => Some("SO"),
                        _ => None,
                    };
                    let mut rows = vec![];
                    if lname == "tables" {
                        for o in db.objects.values() {
                            if let Object::Table(t) = o {
                                let (node, edge) = (t.graph == Some(crate::catalog::GraphKind::Node), t.graph == Some(crate::catalog::GraphKind::Edge));
                                let (tt, ttd, hist) = match t.temporal.as_ref() {
                                    Some(tp) if tp.history.is_some() => {
                                        let h = tp.history.as_ref().unwrap();
                                        let id = db.objects.values().find(|o| o.schema().eq_ignore_ascii_case(&h.0) && o.name().eq_ignore_ascii_case(&h.1)).map(|o| Value::Int(o.object_id())).unwrap_or(Value::Null);
                                        (2, "SYSTEM_VERSIONED_TEMPORAL_TABLE", id)
                                    }
                                    Some(tp) if tp.history_for.is_some() => (1, "HISTORY_TABLE", Value::Null),
                                    _ => (0, "NON_TEMPORAL_TABLE", Value::Null),
                                };
                                let ncols = t.columns.len() as i32;
                                rows.push(vec![
                                    sv(&t.name), Value::Int(t.object_id), Value::Int(sid(&t.schema)), Value::Int(0), sv("U"), sv("USER_TABLE"), date(db.created), date(db.created), Value::Bit(false), Value::Null,
                                    Value::Bit(node), Value::Bit(edge),
                                    Value::Bit(t.memory_optimized), Value::Int(0), sv("SCHEMA_AND_DATA"), Value::Int(tt), sv(ttd), hist, Value::Bit(false), Value::Bit(false),
                                    Value::Int(0), sv("TABLE"), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(true),
                                    Value::Bit(false), Value::Int(0), Value::Null, Value::Int(ncols), Value::Bit(false), Value::Int(0), Value::Bit(false), Value::Bit(false),
                                    Value::Bit(false), Value::Int(0), sv("NON_LEDGER_TABLE"), Value::Bit(false),
                                    Value::Int(0), Value::Int(-1), Value::Int(3), sv("INFINITE"),
                                ]);
                            }
                        }
                    } else {
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
                            for c in constraint_objects(db) {
                                rows.push(vec![sv(&c.name), Value::Int(c.id), Value::Int(sid(&c.schema)), Value::Int(c.parent), sv(c.code), sv(c.desc), date(db.created), date(db.created), Value::Bit(false), Value::Null]);
                            }
                        }
                        if lname == "all_objects" {
                            // the system view SSMS probes for before it lists the server's drives
                            rows.push(vec![sv("dm_os_enumerate_fixed_drives"), Value::Int(-1001), Value::Int(sid("sys")), Value::Int(0), sv("V"), sv("VIEW"), date(db.created), date(db.created), Value::Bit(true), Value::Null]);
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
                    }
                    { let mut cols: Vec<(&str, DataType)> = vec![("name", nv()), ("object_id", DataType::Int), ("schema_id", DataType::Int), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("principal_id", DataType::Int)];
                    if lname == "views" || lname == "all_views" {
                        for r in rows.iter_mut() {
                            r.extend([Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Int(0), sv("NON_LEDGER_VIEW"), Value::Null, Value::Bit(false)]);
                        }
                        cols.extend([
                            ("is_replicated", DataType::Bit), ("has_replication_filter", DataType::Bit), ("has_opaque_metadata", DataType::Bit), ("has_unchecked_assembly_data", DataType::Bit),
                            ("with_check_option", DataType::Bit), ("is_date_correlation_view", DataType::Bit), ("is_tracked_by_cdc", DataType::Bit), ("has_snapshot", DataType::Bit),
                        ]);
                        cols.extend([("ledger_view_type", DataType::TinyInt), ("ledger_view_type_desc", nv()), ("ledger_view_id", DataType::Int), ("is_dropped_ledger_view", DataType::Bit)]);
                    }
                    if lname == "procedures" {
                        for r in rows.iter_mut() {
                            r.extend([Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false)]);
                        }
                        cols.extend([("is_auto_executed", DataType::Bit), ("is_execution_replicated", DataType::Bit), ("is_repl_serializable_only", DataType::Bit), ("skips_repl_constraints", DataType::Bit)]);
                    }
                    if lname == "tables" {
                        cols.push(("is_node", DataType::Bit));
                        cols.push(("is_edge", DataType::Bit));
                        cols.extend([
                            ("is_memory_optimized", DataType::Bit), ("durability", DataType::TinyInt), ("durability_desc", nv()), ("temporal_type", DataType::TinyInt),
                            ("temporal_type_desc", nv()), ("history_table_id", DataType::Int), ("is_external", DataType::Bit), ("is_filetable", DataType::Bit),
                            ("lock_escalation", DataType::TinyInt), ("lock_escalation_desc", nv()), ("is_replicated", DataType::Bit), ("is_merge_published", DataType::Bit),
                            ("is_sync_tran_subscribed", DataType::Bit), ("has_replication_filter", DataType::Bit), ("is_tracked_by_cdc", DataType::Bit), ("uses_ansi_nulls", DataType::Bit),
                            ("has_unchecked_assembly_data", DataType::Bit), ("lob_data_space_id", DataType::Int), ("filestream_data_space_id", DataType::Int), ("max_column_id_used", DataType::Int),
                            ("lock_on_bulk_load", DataType::Bit), ("text_in_row_limit", DataType::Int), ("large_value_types_out_of_row", DataType::Bit), ("is_remote_data_archive_enabled", DataType::Bit),
                            ("is_schema_published", DataType::Bit), ("ledger_type", DataType::TinyInt), ("ledger_type_desc", nv()), ("is_dropped_ledger_table", DataType::Bit),
                            ("ledger_view_id", DataType::Int), ("history_retention_period", DataType::Int), ("history_retention_period_unit", DataType::Int), ("history_retention_period_unit_desc", nv()),
                        ]);
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
                                let (gen, gdesc) = match &t.temporal {
                                    Some(tp) if tp.start.eq_ignore_ascii_case(&c.name) => (1, "AS_ROW_START"),
                                    Some(tp) if tp.end.eq_ignore_ascii_case(&c.name) => (2, "AS_ROW_END"),
                                    _ => (0, "NOT_APPLICABLE"),
                                };
                                let def_id = if c.default_name.is_some() { (t.object_id as i64 * 1000 + 500 + i as i64) as i32 } else { 0 };
                                let user_type_id = Value::Int(c.user_type.as_ref().and_then(|ut| db.types.values().find(|u| matches!(u.kind, crate::catalog::UserTypeKind::Alias(..)) && (format!("{}.{}", u.schema, u.name).eq_ignore_ascii_case(ut) || (u.schema.eq_ignore_ascii_case("dbo") && u.name.eq_ignore_ascii_case(ut))))).map(|u| u.type_id).unwrap_or(st));
                                let coll = if c.ty.is_string() { sv(c.collation.as_deref().unwrap_or(&db.collation_or_default())) } else { Value::Null };

                                rows.push(vec![
                                    Value::Int(t.object_id),
                                    sv(&c.name),
                                    Value::Int(i as i32 + 1),
                                    Value::Int(if matches!(c.ty, DataType::HierarchyId | DataType::Geometry | DataType::Geography) { 240 } else { st }),
                                    user_type_id,
                                    Value::Int(ml),
                                    Value::Int(p),
                                    Value::Int(s),
                                    Value::Bit(c.nullable),
                                    Value::Bit(c.identity.is_some()),
                                    Value::Bit(c.computed.is_some()),
                                    coll,
                                    Value::Int(def_id),
                                    gt,
                                    gd,
                                    Value::Bit(true), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false),
                                    Value::Int(0), Value::Int(0), Value::Bit(false), Value::Bit(false), Value::Int(gen), sv(gdesc), Value::Bit(c.hidden), Value::Bit(c.mask.is_some()),
                                    Value::Bit(false), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Bit(false),
                                ]);
                            }
                        }
                    }
                    // the columns of the views (found when the catalog is read: see `prime_view_columns`)
                    for o in db.objects.values() {
                        if let Object::View(v) = o {
                            let Some((_, cols)) = self.ses.view_cols.get(&v.object_id) else { continue };
                            for (i, (name, ty, nullable)) in cols.iter().enumerate() {
                                let ty = ty.clone().unwrap_or(DataType::NVarChar(MAX));
                                let (st, ml, p, s) = type_info(&ty);
                                let st = if matches!(ty, DataType::HierarchyId | DataType::Geometry | DataType::Geography) { 240 } else { st };
                                let coll = if ty.is_string() { sv(&db.collation_or_default()) } else { Value::Null };
                                rows.push(vec![
                                    Value::Int(v.object_id),
                                    sv(name),
                                    Value::Int(i as i32 + 1),
                                    Value::Int(st),
                                    Value::Int(st),
                                    Value::Int(ml),
                                    Value::Int(p),
                                    Value::Int(s),
                                    Value::Bit(*nullable),
                                    Value::Bit(false),
                                    Value::Bit(false),
                                    coll,
                                    Value::Int(0),
                                    Value::Null,
                                    Value::Null,
                                    Value::Bit(true), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false),
                                    Value::Int(0), Value::Int(0), Value::Bit(false), Value::Bit(false), Value::Int(0), sv("NOT_APPLICABLE"), Value::Bit(false), Value::Bit(false),
                                    Value::Bit(false), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Bit(false),
                                ]);
                            }
                        }
                    }
                    { let cols: Vec<(&str, DataType)> = vec![("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("system_type_id", DataType::TinyInt), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_nullable", DataType::Bit), ("is_identity", DataType::Bit), ("is_computed", DataType::Bit), ("collation_name", nv()), ("default_object_id", DataType::Int), ("graph_type", DataType::TinyInt), ("graph_type_desc", nv()), ("is_ansi_padded", DataType::Bit), ("is_rowguidcol", DataType::Bit), ("is_filestream", DataType::Bit), ("is_replicated", DataType::Bit), ("is_non_sql_subscribed", DataType::Bit), ("is_merge_published", DataType::Bit), ("is_dts_replicated", DataType::Bit), ("is_xml_document", DataType::Bit), ("xml_collection_id", DataType::Int), ("rule_object_id", DataType::Int), ("is_sparse", DataType::Bit), ("is_column_set", DataType::Bit), ("generated_always_type", DataType::TinyInt), ("generated_always_type_desc", nv()), ("is_hidden", DataType::Bit), ("is_masked", DataType::Bit),
                        ("is_dropped_ledger_column", DataType::Bit), ("ledger_view_column_type", DataType::TinyInt), ("ledger_view_column_type_desc", nv()), ("encryption_type", DataType::Int), ("encryption_type_desc", nv()), ("encryption_algorithm_name", nv()), ("column_encryption_key_id", DataType::Int), ("is_data_deletion_filter_column", DataType::Bit)];
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
                            rows.push(vec![Value::Int(oid), sv(&nm), Value::Int(i as i32 + 1), Value::Int(st), Value::Int(st), Value::Int(ml), Value::Int(pr), Value::Int(sc), Value::Bit(p.out), Value::Bit(p.default.is_some()), Value::Bit(p.readonly), Value::Null, Value::Bit(false), Value::Bit(false), Value::Int(0), Value::Bit(true)]);
                        }
                    }
                    // (default_value is filled in for CLR parameters only, as in SQL Server: NULL for a T-SQL procedure's)
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("parameter_id", DataType::Int), ("system_type_id", DataType::TinyInt), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_output", DataType::Bit), ("has_default_value", DataType::Bit), ("is_readonly", DataType::Bit), ("default_value", DataType::Variant), ("is_cursor_ref", DataType::Bit), ("is_xml_document", DataType::Bit), ("xml_collection_id", DataType::Int), ("is_nullable", DataType::Bit)], rows))
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
                                rows.push(vec![Value::Int(t.object_id), Value::Int(key), cat, Value::Bit(f.enabled), sv(st), sv(sd), Value::Bit(true), Value::Null, sv(""), Value::Null, Value::Null, Value::Null, Value::Int(0), Value::Null, Value::Int(1)]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("unique_index_id", DataType::Int), ("fulltext_catalog_id", DataType::Int), ("is_enabled", DataType::Bit), ("change_tracking_state", DataType::Char(1)), ("change_tracking_state_desc", nv()), ("has_crawl_completed", DataType::Bit), ("crawl_type", DataType::Char(1)), ("crawl_type_desc", nv()), ("crawl_start_date", DataType::DateTime), ("crawl_end_date", DataType::DateTime), ("incremental_timestamp", DataType::Binary(8)), ("stoplist_id", DataType::Int), ("property_list_id", DataType::Int), ("data_space_id", DataType::Int)], rows))
                }
                // the languages full-text search names a column's by (the ones SQL Server ships word breakers for, the common ones)
                "fulltext_languages" => {
                    const LANGS: [(i32, &str); 28] = [
                        (0, "Neutral"), (1033, "English"), (2057, "British English"), (1031, "German"), (1036, "French"), (3082, "Spanish"), (1040, "Italian"), (1043, "Dutch"),
                        (1046, "Brazilian"), (2070, "Portuguese"), (1053, "Swedish"), (1049, "Russian"), (1041, "Japanese"), (2052, "Simplified Chinese"), (1028, "Traditional Chinese"),
                        (1042, "Korean"), (1055, "Turkish"), (1045, "Polish"), (1030, "Danish"), (1035, "Finnish"), (1044, "Norwegian"), (1038, "Hungarian"), (1029, "Czech"),
                        (1032, "Greek"), (1037, "Hebrew"), (1054, "Thai"), (1025, "Arabic"), (1081, "Hindi"),
                    ];
                    Some(rs(&[("lcid", DataType::Int), ("name", nv())], LANGS.iter().map(|(l, n)| vec![Value::Int(*l), sv(n)]).collect()))
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
                    let rows = db
                        .policies
                        .iter()
                        .map(|p| {
                            vec![
                                sv(&p.name), Value::Int(p.object_id), Value::Bit(p.enabled), sv(&p.schema),
                                Value::Int(sid(&p.schema)), Value::Null, Value::Int(0), sv("SP"), sv("SECURITY_POLICY"), date(db.created), date(db.created), Value::Bit(false), Value::Bit(false), Value::Bit(false),
                            ]
                        })
                        .collect();
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("is_enabled", DataType::Bit), ("schema_name", nv()),
                        ("schema_id", DataType::Int), ("principal_id", DataType::Int), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime),
                        ("is_ms_shipped", DataType::Bit), ("is_not_for_replication", DataType::Bit), ("is_schema_bound", DataType::Bit)], rows))
                }
                "security_predicates" => {
                    let mut rows = vec![];
                    for p in &db.policies {
                        for x in &p.preds {
                            let (kind, op) = x.kind.describe();
                            let target = db.objects.get(&x.table).map(|o| o.object_id()).unwrap_or(0);
                            rows.push(vec![Value::Int(p.object_id), Value::Int(target), sv(&format!("[{}].[{}]({})", x.func.0, x.func.1, x.args.join(", "))), sv(kind), if op.is_empty() { Value::Null } else { sv(op) }, Value::Int(rows.len() as i32 + 1), Value::Int(if kind.starts_with("FILTER") { 1 } else { 2 }), Value::Int(0), Value::Null]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("target_object_id", DataType::Int), ("predicate_definition", nv()), ("predicate_type_desc", nv()), ("operation_desc", nv()),
                        ("security_predicate_id", DataType::Int), ("predicate_type", DataType::Int), ("operation", DataType::Int), ("target_object_column_id", DataType::Int)], rows))
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
                                rows.push(vec![Value::Int(t.object_id), sv(&t.columns[ci].name), Value::Int(ci as i32 + 1), Value::BigInt(sd), Value::BigInt(inc), t.identity_next.get().map(|n| Value::BigInt(n - inc)).unwrap_or(Value::Null), Value::Bit(false)]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("seed_value", DataType::BigInt), ("increment_value", DataType::BigInt), ("last_value", DataType::BigInt), ("is_not_for_replication", DataType::Bit)], rows))
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
                    let rows: Vec<Row> = self
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
                    // the rest of the columns of sys.databases (SQL Server 2022 / 2025), which SSMS selects: the defaults of a database here
                    let rows: Vec<Row> = rows
                        .into_iter()
                        .map(|mut r: Row| {
                            let name = match &r[0] {
                                Value::Str(n) => n.clone(),
                                _ => String::new(),
                            };
                            r.extend([
                                Value::Int(0), sv("DATABASE_DEFAULT"), sv(&name), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false),
                                Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false),
                                Value::Int(0), Value::Int(1033), sv("us_english"), Value::Int(1033), sv("English"), Value::Bit(true), Value::Bit(false), Value::Int(1999),
                                Value::Null, Value::Null, Value::Bit(false), Value::Null, Value::Bit(false), Value::Bit(false),
                            ]);
                            r
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
                            ("catalog_collation_type", DataType::Int),
                            ("catalog_collation_type_desc", nv()),
                            ("physical_database_name", nv()),
                            ("is_result_set_caching_on", DataType::Bit),
                            ("is_accelerated_database_recovery_on", DataType::Bit),
                            ("is_tempdb_spill_to_remote_store", DataType::Bit),
                            ("is_stale_page_detection_on", DataType::Bit),
                            ("is_memory_optimized_enabled", DataType::Bit),
                            ("is_data_retention_enabled", DataType::Bit),
                            ("is_optimized_locking_on", DataType::Bit),
                            ("is_query_store_on", DataType::Bit),
                            ("is_memory_optimized_elevate_to_snapshot_on", DataType::Bit),
                            ("is_federation_member", DataType::Bit),
                            ("is_remote_data_archive_enabled", DataType::Bit),
                            ("is_mixed_page_allocation_on", DataType::Bit),
                            ("is_temporal_history_retention_enabled", DataType::Bit),
                            ("resource_pool_id", DataType::Int),
                            ("default_language_lcid", DataType::SmallInt),
                            ("default_language_name", nv()),
                            ("default_fulltext_language_lcid", DataType::Int),
                            ("default_fulltext_language_name", nv()),
                            ("is_nested_triggers_on", DataType::Bit),
                            ("is_transform_noise_words_on", DataType::Bit),
                            ("two_digit_year_cutoff", DataType::SmallInt),
                            ("replica_id", DataType::Guid),
                            ("group_database_id", DataType::Guid),
                            ("is_sync_with_backup", DataType::Bit),
                            ("service_broker_guid", DataType::Guid),
                            ("is_honor_broker_priority_on", DataType::Bit),
                            ("is_auto_create_stats_incremental_on", DataType::Bit),
                        ],
                        rows,
                    ))
                }
                // the logins as SQL Server 2000 had them (Reporting Services' setup asks whether `sa` is there and has access)
                "syslogins" => {
                    let rows = self
                        .srv
                        .security
                        .logins
                        .values()
                        .map(|l| {
                            let nt = l.external.is_some();
                            let sysadmin = l.name.eq_ignore_ascii_case("sa") as i32;
                            vec![
                                Value::Bin(l.name.to_lowercase().into_bytes()),
                                Value::Int(0),
                                date(self.srv.start_time),
                                date(self.srv.start_time),
                                sv(&l.name),
                                sv(&l.default_db),
                                sv("us_english"),
                                Value::Int(l.disabled as i32),
                                Value::Int(!l.disabled as i32),
                                Value::Int(nt as i32),
                                Value::Int(0),
                                Value::Int(nt as i32),
                                Value::Int(sysadmin),
                                Value::Int(sysadmin),
                                Value::Int(sysadmin),
                                Value::Int(sysadmin),
                                Value::Int(sysadmin),
                                Value::Int(sysadmin),
                                Value::Int(sysadmin),
                                Value::Int(sysadmin),
                            ]
                        })
                        .collect();
                    Some(rs(
                        &[("sid", DataType::VarBinary(85)), ("status", DataType::SmallInt), ("createdate", DataType::DateTime), ("updatedate", DataType::DateTime), ("name", nv()), ("dbname", nv()), ("language", nv()), ("denylogin", DataType::Int), ("hasaccess", DataType::Int), ("isntname", DataType::Int), ("isntgroup", DataType::Int), ("isntuser", DataType::Int), ("sysadmin", DataType::Int), ("securityadmin", DataType::Int), ("serveradmin", DataType::Int), ("setupadmin", DataType::Int), ("processadmin", DataType::Int), ("diskadmin", DataType::Int), ("dbcreator", DataType::Int), ("bulkadmin", DataType::Int)],
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
                            rows.push(vec![Value::Int(d.id), Value::Int(k as i32 + 1), sv(&format!("{}{}", d.name, if k == 0 { "" } else { "_log" })), sv(&format!("{}.{}", d.name, ext)), Value::Int(*kind), sv(desc), Value::Int(128), Value::Int(-1), Value::Int(0), sv("ONLINE"), Value::Int(if k == 0 { 1 } else { 0 }), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Int(0), Value::Null]);
                        }
                    }
                    Some(rs(&[("database_id", DataType::Int), ("file_id", DataType::Int), ("name", nv()), ("physical_name", nv()), ("type", DataType::TinyInt), ("type_desc", nv()), ("size", DataType::Int), ("max_size", DataType::Int), ("growth", DataType::Int), ("state_desc", nv()), ("data_space_id", DataType::Int), ("is_media_read_only", DataType::Bit), ("is_read_only", DataType::Bit), ("is_sparse", DataType::Bit), ("is_percent_growth", DataType::Bit), ("is_name_reserved", DataType::Bit), ("state", DataType::TinyInt), ("drop_lsn", DataType::Decimal(25, 0))], rows))
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
                // the one filegroup, PRIMARY (SSMS joins the indexes of a table to it)
                "internal_tables" => Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("principal_id", DataType::Int), ("schema_id", DataType::Int), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("is_published", DataType::Bit), ("is_schema_published", DataType::Bit), ("internal_type", DataType::TinyInt), ("internal_type_desc", nv()), ("parent_id", DataType::Int), ("parent_minor_id", DataType::Int), ("lob_data_space_id", DataType::Int), ("filestream_data_space_id", DataType::Int)], vec![])),
                "filetable_system_defined_objects" => Some(rs(&[("object_id", DataType::Int), ("parent_object_id", DataType::Int)], vec![])),
                "hash_indexes" => Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("index_id", DataType::Int), ("type", DataType::TinyInt), ("type_desc", nv()), ("is_unique", DataType::Bit), ("data_space_id", DataType::Int), ("ignore_dup_key", DataType::Bit), ("is_primary_key", DataType::Bit), ("is_unique_constraint", DataType::Bit), ("fill_factor", DataType::TinyInt), ("is_padded", DataType::Bit), ("is_disabled", DataType::Bit), ("is_hypothetical", DataType::Bit), ("allow_row_locks", DataType::Bit), ("allow_page_locks", DataType::Bit), ("has_filter", DataType::Bit), ("filter_definition", nv()), ("auto_created", DataType::Bit), ("bucket_count", DataType::Int)], vec![])),
                "index_resumable_operations" => Some(rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("name", nv()), ("sql_text", nv()), ("last_max_dop_used", DataType::TinyInt), ("partition_number", DataType::Int), ("state", DataType::TinyInt), ("state_desc", nv()), ("start_time", DataType::DateTime), ("last_pause_time", DataType::DateTime), ("total_execution_time", DataType::Int), ("percent_complete", DataType::Float), ("page_count", DataType::BigInt)], vec![])),
                "selective_xml_index_paths" => Some(rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("path_id", DataType::Int), ("path", nv()), ("name", nv()), ("path_type", DataType::TinyInt), ("xml_component_id", DataType::Int), ("xquery_type_description", nv()), ("is_xquery_type_inferred", DataType::Bit), ("xquery_max_length", DataType::Int), ("is_xquery_max_length_inferred", DataType::Bit), ("is_node", DataType::Bit), ("system_type_id", DataType::TinyInt), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("collation_name", nv()), ("is_singleton", DataType::Bit)], vec![])),
                "asymmetric_keys" => Some(rs(&[("name", nv()), ("principal_id", DataType::Int), ("asymmetric_key_id", DataType::Int), ("pvt_key_encryption_type", DataType::Char(2)), ("pvt_key_encryption_type_desc", nv()), ("thumbprint", DataType::VarBinary(85)), ("algorithm", DataType::Char(2)), ("algorithm_desc", nv()), ("key_length", DataType::Int), ("sid", DataType::VarBinary(85)), ("string_sid", nv()), ("public_key", DataType::VarBinary(85)), ("attested_by", nv()), ("provider_type", DataType::Char(4)), ("cryptographic_provider_guid", DataType::Guid), ("cryptographic_provider_algid", DataType::Int)], vec![])),
                "certificates" => Some(rs(&[("name", nv()), ("certificate_id", DataType::Int), ("principal_id", DataType::Int), ("pvt_key_encryption_type", DataType::Char(2)), ("pvt_key_encryption_type_desc", nv()), ("is_active_for_begin_dialog", DataType::Bit), ("issuer_name", nv()), ("cert_serial_number", nv()), ("sid", DataType::VarBinary(85)), ("string_sid", nv()), ("subject", nv()), ("expiry_date", DataType::DateTime), ("start_date", DataType::DateTime), ("thumbprint", DataType::VarBinary(85)), ("attested_by", nv()), ("pvt_key_last_backup_date", DataType::DateTime), ("key_length", DataType::Int)], vec![])),
                "partition_schemes" => Some(rs(&[("name", nv()), ("data_space_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("is_default", DataType::Bit), ("is_system", DataType::Bit), ("function_id", DataType::Int)], vec![])),
                "destination_data_spaces" => Some(rs(&[("partition_scheme_id", DataType::Int), ("destination_id", DataType::Int), ("data_space_id", DataType::Int)], vec![])),
                "partition_range_values" => Some(rs(&[("function_id", DataType::Int), ("boundary_id", DataType::Int), ("parameter_id", DataType::Int), ("value", DataType::Variant)], vec![])),
                // full-text stoplists and search property lists: none (the designer joins them to the full-text index)
                "fulltext_stoplists" => Some(rs(&[("stoplist_id", DataType::Int), ("name", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("principal_id", DataType::Int)], vec![])),
                "registered_search_property_lists" => Some(rs(&[("property_list_id", DataType::Int), ("name", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("principal_id", DataType::Int)], vec![])),
                // numbered procedures (`proc;2`) are not supported: none
                "numbered_procedures" => Some(rs(&[("object_id", DataType::Int), ("procedure_number", DataType::SmallInt), ("definition", DataType::NVarChar(MAX))], vec![])),
                "numbered_procedure_parameters" => Some(rs(&[("object_id", DataType::Int), ("procedure_number", DataType::SmallInt), ("name", nv()), ("parameter_id", DataType::Int), ("system_type_id", DataType::TinyInt), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_output", DataType::Bit)], vec![])),
                "partition_parameters" => Some(rs(&[("function_id", DataType::Int), ("parameter_id", DataType::Int), ("system_type_id", DataType::TinyInt), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("collation_name", nv()), ("user_type_id", DataType::Int)], vec![])),
                "partition_functions" => Some(rs(&[("name", nv()), ("function_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("fanout", DataType::Int), ("boundary_value_on_right", DataType::Bit), ("is_system", DataType::Bit), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime)], vec![])),
                "allocation_units" => Some(rs(&[("allocation_unit_id", DataType::BigInt), ("type", DataType::TinyInt), ("type_desc", nv()), ("container_id", DataType::BigInt), ("data_space_id", DataType::Int), ("total_pages", DataType::BigInt), ("used_pages", DataType::BigInt), ("data_pages", DataType::BigInt)], vec![])),
                "syslanguages" => Some(rs(
                    &[("langid", DataType::SmallInt), ("dateformat", nv()), ("datefirst", DataType::TinyInt), ("upgrade", DataType::Int), ("name", nv()), ("alias", nv()), ("months", nv()), ("shortmonths", nv()), ("days", nv()), ("lcid", DataType::Int), ("msglangid", DataType::SmallInt)],
                    vec![vec![Value::Int(0), sv("mdy"), Value::Int(7), Value::Int(0), sv("us_english"), sv("English"), sv("January,February,March,April,May,June,July,August,September,October,November,December"), sv("Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec"), sv("Monday,Tuesday,Wednesday,Thursday,Friday,Saturday,Sunday"), Value::Int(1033), Value::Int(1033)]],
                )),
                "change_tracking_tables" => Some(rs(&[("object_id", DataType::Int), ("is_track_columns_updated_on", DataType::Bit), ("begin_version", DataType::BigInt), ("cleanup_version", DataType::BigInt), ("min_valid_version", DataType::BigInt)], vec![])),
                "external_data_sources" => Some(rs(&[("data_source_id", DataType::Int), ("name", nv()), ("location", nv()), ("type_desc", nv()), ("type", DataType::TinyInt), ("resource_manager_location", nv()), ("credential_id", DataType::Int), ("database_name", nv()), ("shard_map_name", nv())], vec![])),
                "external_file_formats" => Some(rs(&[("file_format_id", DataType::Int), ("name", nv()), ("format_type", nv()), ("field_terminator", nv()), ("string_delimiter", nv()), ("date_format", nv()), ("use_type_default", DataType::Bit), ("serde_method", nv()), ("row_terminator", nv()), ("encoding", nv()), ("data_compression", nv()), ("first_row", DataType::Int)], vec![])),
                "external_tables" => Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("schema_id", DataType::Int), ("data_source_id", DataType::Int), ("file_format_id", DataType::Int), ("location", nv()), ("reject_type", nv()), ("reject_value", DataType::Float), ("reject_sample_value", DataType::Float), ("distribution_type", DataType::TinyInt), ("distribution_desc", nv()), ("sharding_col_id", DataType::Int), ("remote_schema_name", nv()), ("remote_object_name", nv()), ("rejected_row_location", nv())], vec![])),
                "filetables" => Some(rs(&[("object_id", DataType::Int), ("is_enabled", DataType::Bit), ("directory_name", nv()), ("filename_collation_id", DataType::Int), ("filename_collation_name", nv())], vec![])),
                "periods" => {
                    let mut rows: Vec<Row> = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            if let Some(tp) = &t.temporal {
                                let at = |n: &str| t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(n)).map(|i| i as i32 + 1).unwrap_or(0);
                                rows.push(vec![Value::Int(t.object_id), Value::Int(1), sv("SYSTEM_TIME_PERIOD"), sv("SYSTEM_TIME"), Value::Int(at(&tp.start)), Value::Int(at(&tp.end)), Value::Int(t.object_id * 1000 + 800)]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("period_type", DataType::TinyInt), ("period_type_desc", nv()), ("name", nv()), ("start_column_id", DataType::Int), ("end_column_id", DataType::Int), ("period_id", DataType::Int)], rows))
                }
                "column_encryption_keys" => Some(rs(&[("name", nv()), ("column_encryption_key_id", DataType::Int), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime)], vec![])),
                "column_master_keys" => Some(rs(&[("name", nv()), ("column_master_key_id", DataType::Int), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("key_store_provider_name", nv()), ("key_path", nv()), ("allow_enclave_computations", DataType::Bit), ("signature", DataType::VarBinary(8000))], vec![])),
                "column_encryption_key_values" => Some(rs(&[("column_encryption_key_id", DataType::Int), ("column_master_key_id", DataType::Int), ("encrypted_value", DataType::VarBinary(8000)), ("encryption_algorithm_name", nv())], vec![])),
                "sensitivity_classifications" => Some(rs(&[("class", DataType::TinyInt), ("class_desc", nv()), ("major_id", DataType::Int), ("minor_id", DataType::Int), ("label", nv()), ("label_id", nv()), ("information_type", nv()), ("information_type_id", nv()), ("rank", DataType::Int), ("rank_desc", nv())], vec![])),
                "assembly_modules" => Some(rs(&[("object_id", DataType::Int), ("assembly_id", DataType::Int), ("assembly_class", nv()), ("assembly_method", nv()), ("null_on_null_input", DataType::Bit), ("execute_as_principal_id", DataType::Int)], vec![])),
                "assembly_references" => Some(rs(&[("assembly_id", DataType::Int), ("referenced_assembly_id", DataType::Int)], vec![])),
                "assembly_types" => Some(rs(&[("name", nv()), ("system_type_id", DataType::TinyInt), ("user_type_id", DataType::Int), ("schema_id", DataType::Int), ("principal_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("collation_name", nv()), ("is_nullable", DataType::Bit), ("is_user_defined", DataType::Bit), ("is_assembly_type", DataType::Bit), ("default_object_id", DataType::Int), ("rule_object_id", DataType::Int), ("assembly_id", DataType::Int), ("assembly_class", nv()), ("is_binary_ordered", DataType::Bit), ("is_fixed_length", DataType::Bit), ("prog_id", nv()), ("assembly_qualified_name", nv()), ("permission_set_desc", nv())], vec![])),
                "plan_guides" => Some(rs(&[("plan_guide_id", DataType::Int), ("name", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_disabled", DataType::Bit), ("query_text", nv()), ("scope_type", DataType::TinyInt), ("scope_type_desc", nv()), ("scope_object_id", DataType::Int), ("scope_batch", nv()), ("parameters", nv()), ("hints", nv())], vec![])),
                // what each view, procedure, function and trigger refers to, found in its definition (a table or a view by name)
                "sql_expression_dependencies" => {
                    let rows: Vec<Row> = self
                        .dependency_rows()
                        .into_iter()
                        .map(|d| {
                            vec![
                                Value::Int(d.referencing_id), Value::Int(0), Value::Int(1), sv("OBJECT_OR_COLUMN"), Value::Bit(false), Value::Int(1), sv("OBJECT_OR_COLUMN"), Value::Null, Value::Null,
                                sv(&d.ref_schema), sv(&d.ref_name), d.ref_id.map(Value::Int).unwrap_or(Value::Null), Value::Int(0), Value::Bit(false), Value::Bit(false),
                            ]
                        })
                        .collect();
                    Some(rs(&[("referencing_id", DataType::Int), ("referencing_minor_id", DataType::Int), ("referencing_class", DataType::TinyInt), ("referencing_class_desc", nv()), ("is_schema_bound_reference", DataType::Bit),
                        ("referenced_class", DataType::TinyInt), ("referenced_class_desc", nv()), ("referenced_server_name", nv()), ("referenced_database_name", nv()), ("referenced_schema_name", nv()), ("referenced_entity_name", nv()),
                        ("referenced_id", DataType::Int), ("referenced_minor_id", DataType::Int), ("is_caller_dependent", DataType::Bit), ("is_ambiguous", DataType::Bit)], rows))
                }
                // table types (CREATE TYPE ... AS TABLE)
                "table_types" => {
                    let rows = db
                        .types
                        .values()
                        .filter(|u| matches!(u.kind, crate::catalog::UserTypeKind::Table(_)))
                        .map(|u| vec![sv(&u.name), Value::Int(243), Value::Int(u.type_id), Value::Int(sid(&u.schema)), Value::Null, Value::Int(-1), Value::Int(0), Value::Int(0), Value::Null, Value::Bit(true), Value::Bit(true), Value::Bit(false), Value::Int(0), Value::Int(0), Value::Bit(true), Value::Int(u.type_id), Value::Bit(false)])
                        .collect();
                    Some(rs(&[("name", nv()), ("system_type_id", DataType::TinyInt), ("user_type_id", DataType::Int), ("schema_id", DataType::Int), ("principal_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt),
                        ("collation_name", nv()), ("is_nullable", DataType::Bit), ("is_user_defined", DataType::Bit), ("is_assembly_type", DataType::Bit), ("default_object_id", DataType::Int), ("rule_object_id", DataType::Int), ("is_table_type", DataType::Bit), ("type_table_object_id", DataType::Int), ("is_memory_optimized", DataType::Bit)], rows))
                }
                // the policy store (msdb.dbo.syspolicy_*): Policy-Based Management is not implemented, so the store is as a new server's
                // (policy management off, no policies), which is what SSMS's Management > Policy Management reads
                // the backup history of msdb before its tables exist (see backuphist.rs): empty
                "backupset" | "backupmediafamily" | "backupmediaset" | "backupfile" | "backupfilegroup" | "restorehistory" | "restorefile" | "restorefilegroup" | "suspect_pages" => {
                    use DataType::*;
                    let n = |x: &str| (x.to_string(), NVarChar(128));
                    let cols: Vec<(String, DataType)> = match lname.as_str() {
                        "backupset" => {
                            let mut c: Vec<(String, DataType)> = vec![("backup_set_id".into(), Int), ("backup_set_uuid".into(), Guid), ("media_set_id".into(), Int), ("first_family_number".into(), TinyInt), ("first_media_number".into(), SmallInt), ("last_family_number".into(), TinyInt), ("last_media_number".into(), SmallInt), ("catalog_family_number".into(), TinyInt), ("catalog_media_number".into(), SmallInt), ("position".into(), Int), ("expiration_date".into(), DateTime), ("software_vendor_id".into(), Int), n("name"), ("description".into(), NVarChar(255)), n("user_name"), ("software_major_version".into(), TinyInt), ("software_minor_version".into(), TinyInt), ("software_build_version".into(), SmallInt), ("time_zone".into(), SmallInt), ("mtf_minor_version".into(), TinyInt)];
                            for l in ["first_lsn", "last_lsn", "checkpoint_lsn", "database_backup_lsn"] {
                                c.push((l.into(), Decimal(25, 0)));
                            }
                            c.extend([("database_creation_date".into(), DateTime), ("backup_start_date".into(), DateTime), ("backup_finish_date".into(), DateTime), ("type".into(), Char(1)), ("sort_order".into(), SmallInt), ("code_page".into(), SmallInt), ("compatibility_level".into(), TinyInt), ("database_version".into(), Int), ("backup_size".into(), Decimal(20, 0)), n("database_name"), n("server_name"), n("machine_name"), ("flags".into(), Int), ("unicode_locale".into(), Int), ("unicode_compare_style".into(), Int), n("collation_name"), ("is_password_protected".into(), Bit), ("recovery_model".into(), NVarChar(60)), ("has_bulk_logged_data".into(), Bit), ("is_snapshot".into(), Bit), ("is_readonly".into(), Bit), ("is_single_user".into(), Bit), ("has_backup_checksums".into(), Bit), ("is_damaged".into(), Bit), ("begins_log_chain".into(), Bit), ("has_incomplete_metadata".into(), Bit), ("is_force_offline".into(), Bit), ("is_copy_only".into(), Bit), ("first_recovery_fork_guid".into(), Guid), ("last_recovery_fork_guid".into(), Guid), ("fork_point_lsn".into(), Decimal(25, 0)), ("database_guid".into(), Guid), ("family_guid".into(), Guid), ("differential_base_lsn".into(), Decimal(25, 0)), ("differential_base_guid".into(), Guid), ("backup_channel_type".into(), TinyInt), ("backup_channel_type_desc".into(), NVarChar(60)), ("compressed_backup_size".into(), Decimal(20, 0)), ("key_algorithm".into(), NVarChar(32)), ("encryptor_thumbprint".into(), VarBinary(20)), ("encryptor_type".into(), NVarChar(32))]);
                            c
                        }
                        "backupmediafamily" => vec![("media_set_id".into(), Int), ("family_sequence_number".into(), TinyInt), ("media_family_id".into(), Guid), ("media_count".into(), Int), n("logical_device_name"), ("physical_device_name".into(), NVarChar(260)), ("device_type".into(), TinyInt), ("physical_block_size".into(), Int), ("mirror".into(), TinyInt)],
                        "backupmediaset" => vec![("media_set_id".into(), Int), ("media_uuid".into(), Guid), ("media_family_count".into(), TinyInt), n("name"), ("description".into(), NVarChar(255)), n("software_name"), ("software_vendor_id".into(), Int), ("MTF_major_version".into(), TinyInt), ("mirror_count".into(), TinyInt), ("is_password_protected".into(), Bit), ("is_compressed".into(), Bit)],
                        "backupfile" => vec![("backup_set_id".into(), Int), ("first_family_number".into(), TinyInt), ("first_media_number".into(), SmallInt), n("filegroup_name"), ("page_size".into(), Int), ("file_number".into(), Decimal(10, 0)), ("backed_up_page_count".into(), Decimal(10, 0)), ("file_type".into(), Char(1)), ("source_file_block_size".into(), Decimal(10, 0)), ("file_size".into(), Decimal(20, 0)), n("logical_name"), ("physical_name".into(), NVarChar(260)), ("state".into(), TinyInt), ("state_desc".into(), NVarChar(60)), ("create_lsn".into(), Decimal(25, 0)), ("drop_lsn".into(), Decimal(25, 0)), ("file_guid".into(), Guid), ("read_only_lsn".into(), Decimal(25, 0)), ("read_write_lsn".into(), Decimal(25, 0)), ("differential_base_lsn".into(), Decimal(25, 0)), ("differential_base_guid".into(), Guid), ("backup_size".into(), Decimal(20, 0)), ("filegroup_guid".into(), Guid), ("is_readonly".into(), Bit), ("is_present".into(), Bit)],
                        "backupfilegroup" => vec![("backup_set_id".into(), Int), n("name"), ("filegroup_id".into(), Int), ("filegroup_guid".into(), Guid), ("type".into(), Char(2)), ("type_desc".into(), NVarChar(60)), ("is_default".into(), Bit), ("is_readonly".into(), Bit), ("log_filegroup_guid".into(), Guid)],
                        "restorehistory" => vec![("restore_history_id".into(), Int), ("restore_date".into(), DateTime), n("destination_database_name"), n("user_name"), ("backup_set_id".into(), Int), ("restore_type".into(), Char(1)), ("replace".into(), Bit), ("recovery".into(), Bit), ("restart".into(), Bit), ("stop_at".into(), DateTime), ("device_count".into(), TinyInt), n("stop_at_mark_name"), ("stop_before".into(), Bit)],
                        "restorefile" => vec![("restore_history_id".into(), Int), ("file_number".into(), Decimal(10, 0)), ("destination_phys_name".into(), NVarChar(260))],
                        "restorefilegroup" => vec![("restore_history_id".into(), Int), n("filegroup_name")],
                        _ => vec![("database_id".into(), Int), ("file_id".into(), Int), ("page_id".into(), BigInt), ("event_type".into(), Int), ("error_count".into(), Int), ("last_update_date".into(), DateTime)],
                    };
                    let refs: Vec<(&str, DataType)> = cols.iter().map(|(a, t)| (a.as_str(), t.clone())).collect();
                    Some(rs(&refs, vec![]))
                }
                // SQL Server Agent's catalog in msdb: there is no Agent here, so no job, schedule or step; the tables are there and empty (Reporting Services'
                // setup joins its schedules to `msdb.dbo.sysjobs`)
                "sysjobs" => Some(rs(&[("job_id", DataType::Guid), ("originating_server_id", DataType::Int), ("name", nv()), ("enabled", DataType::TinyInt), ("description", nv()), ("start_step_id", DataType::Int), ("category_id", DataType::Int), ("owner_sid", DataType::VarBinary(85)), ("date_created", DataType::DateTime), ("date_modified", DataType::DateTime), ("version_number", DataType::Int)], vec![])),
                "syscategories" => Some(rs(&[("category_id", DataType::Int), ("category_class", DataType::Int), ("category_type", DataType::TinyInt), ("name", nv())], vec![])),
                "sysjobschedules" => Some(rs(&[("schedule_id", DataType::Int), ("job_id", DataType::Guid), ("next_run_date", DataType::Int), ("next_run_time", DataType::Int)], vec![])),
                "sysschedules" => Some(rs(&[("schedule_id", DataType::Int), ("schedule_uid", DataType::Guid), ("originating_server_id", DataType::Int), ("name", nv()), ("owner_sid", DataType::VarBinary(85)), ("enabled", DataType::Int), ("freq_type", DataType::Int), ("freq_interval", DataType::Int), ("date_created", DataType::DateTime), ("date_modified", DataType::DateTime)], vec![])),
                "sysjobsteps" => Some(rs(&[("job_id", DataType::Guid), ("step_id", DataType::Int), ("step_name", nv()), ("subsystem", nv()), ("command", nv()), ("database_name", nv())], vec![])),
                "sysjobservers" => Some(rs(&[("job_id", DataType::Guid), ("server_id", DataType::Int), ("last_run_outcome", DataType::TinyInt)], vec![])),
                "sysjobhistory" => Some(rs(&[("instance_id", DataType::Int), ("job_id", DataType::Guid), ("step_id", DataType::Int), ("step_name", nv()), ("run_status", DataType::Int), ("run_date", DataType::Int), ("run_time", DataType::Int), ("message", nv())], vec![])),
                "syspolicy_configuration" => Some(rs(
                    &[("name", nv()), ("current_value", DataType::Int)],
                    vec![vec![sv("Enabled"), Value::Int(0)], vec![sv("HistoryRetentionInDays"), Value::Int(90)], vec![sv("LogOnSuccess"), Value::Int(0)]],
                )),
                // the objects and users of SQL Server 2000, which SSMS reads for the aggregates and a few others
                "sysobjects" => {
                    let rows = db
                        .objects
                        .values()
                        .map(|o| vec![sv(o.name()), Value::Int(o.object_id()), sv(o.type_code()), Value::Int(sid(o.schema())), Value::Int(0), Value::Int(0), date(db.created), sv(o.type_code()), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0)])
                        .chain(constraint_objects(db).into_iter().map(|c| vec![sv(&c.name), Value::Int(c.id), sv(c.code), Value::Int(sid(&c.schema)), Value::Int(0), Value::Int(c.parent), date(db.created), sv(c.code), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0)]))
                        .collect();
                    Some(rs(&[("name", nv()), ("id", DataType::Int), ("xtype", DataType::Char(2)), ("uid", DataType::SmallInt), ("info", DataType::SmallInt), ("parent_obj", DataType::Int), ("crdate", DataType::DateTime), ("type", DataType::Char(2)), ("status", DataType::Int), ("category", DataType::Int), ("base_schema_ver", DataType::Int), ("replinfo", DataType::Int), ("schema_ver", DataType::Int), ("stats_schema_ver", DataType::Int), ("ftcatid", DataType::SmallInt), ("userstat", DataType::SmallInt), ("sysstat", DataType::SmallInt), ("indexdel", DataType::SmallInt), ("version", DataType::Int), ("deltrig", DataType::Int), ("instrig", DataType::Int), ("updtrig", DataType::Int), ("seltrig", DataType::Int), ("cache", DataType::Int)], rows))
                }
                // the columns of the tables, as SQL Server 2000 had them (Reporting Services' upgrade scripts ask `dbo.syscolumns` whether a column is there)
                "syscolumns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                let (tid, ml, p, s) = type_info(&c.ty);
                                rows.push(vec![
                                    sv(&c.name),
                                    Value::Int(t.object_id),
                                    Value::Int(tid),
                                    Value::Int(tid),
                                    Value::Int(ml),
                                    Value::Int(p),
                                    Value::Int(s),
                                    Value::Int(i as i32 + 1),
                                    Value::Int(0),
                                    Value::Int(tid),
                                    Value::Int(tid),
                                    Value::Int(p),
                                    Value::Int(s),
                                    Value::Int(c.computed.is_some() as i32),
                                    Value::Int(0),
                                    Value::Int(c.nullable as i32),
                                    if c.ty.is_string() { sv(c.collation.as_deref().unwrap_or(crate::collation::DEFAULT_NAME)) } else { Value::Null },
                                ]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("id", DataType::Int), ("xtype", DataType::TinyInt), ("xusertype", DataType::SmallInt), ("length", DataType::SmallInt), ("xprec", DataType::TinyInt), ("xscale", DataType::TinyInt), ("colid", DataType::SmallInt), ("number", DataType::SmallInt), ("type", DataType::TinyInt), ("usertype", DataType::SmallInt), ("prec", DataType::SmallInt), ("scale", DataType::Int), ("iscomputed", DataType::Int), ("isoutparam", DataType::Int), ("isnullable", DataType::Int), ("collation", nv())], rows))
                }
                // the indexes of the tables, as SQL Server 2000 had them: one row per index (a table with no clustered index has its heap, `indid` 0)
                "sysindexes" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut named: Vec<(String, usize, bool, bool)> = vec![];
                            if let Some(pk) = &t.pk {
                                named.push((pk.name.clone(), pk.cols.len(), pk.clustered, true));
                            }
                            for u in &t.uniques {
                                named.push((u.name.clone(), u.cols.len(), u.clustered, true));
                            }
                            for i in &t.indexes {
                                if !named.iter().any(|n| n.0.eq_ignore_ascii_case(&i.name)) {
                                    named.push((i.name.clone(), i.cols.len(), false, i.unique));
                                }
                            }
                            if !named.iter().any(|n| n.2) {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(0), sv(&t.name), Value::Int(0), Value::Int(0)]);
                            }
                            let mut next = 2;
                            for (name, keys, clustered, unique) in named {
                                let indid = if clustered { 1 } else { next };
                                if !clustered {
                                    next += 1;
                                }
                                rows.push(vec![Value::Int(t.object_id), Value::Int(indid), sv(&name), Value::Int(if unique { 2 } else { 0 }), Value::Int(keys as i32)]);
                            }
                        }
                    }
                    Some(rs(&[("id", DataType::Int), ("indid", DataType::SmallInt), ("name", nv()), ("status", DataType::Int), ("keycnt", DataType::SmallInt)], rows))
                }
                // the text of views, procedures, functions and triggers, in pieces of 4000 characters (the view designer reads a view's)
                "syscomments" => {
                    let mut rows = vec![];
                    let mut add = |id: i32, text: &str| {
                        let chars: Vec<char> = text.chars().collect();
                        let pieces: Vec<String> = if chars.is_empty() { vec![String::new()] } else { chars.chunks(4000).map(|c| c.iter().collect()).collect() };
                        for (k, p) in pieces.into_iter().enumerate() {
                            rows.push(vec![Value::Int(id), Value::Int(1), Value::Int(k as i32 + 1), Value::Int(0), Value::Bin(vec![]), Value::Int(2), Value::Int(0), Value::Bit(false), Value::Bit(false), sv(&p)]);
                        }
                    };
                    for o in db.objects.values() {
                        match o {
                            Object::View(v) => add(v.object_id, &v.text),
                            Object::Proc(p) => add(p.object_id, &p.text),
                            Object::Func(f) => add(f.object_id, &f.def.text),
                            _ => {}
                        }
                    }
                    for (_, trg) in db.triggers.values() {
                        add(trg.object_id, &trg.text);
                    }
                    Some(rs(&[("id", DataType::Int), ("number", DataType::SmallInt), ("colid", DataType::SmallInt), ("status", DataType::SmallInt), ("ctext", DataType::VarBinary(8000)), ("texttype", DataType::SmallInt), ("language", DataType::SmallInt), ("encrypted", DataType::Bit), ("compressed", DataType::Bit), ("text", DataType::NVarChar(4000))], rows))
                }
                // which user is in which role, as SQL Server 2000 had it (Reporting Services' upgrade scripts ask whether `RSExecRole` is a `db_owner`)
                "sysmembers" => {
                    let rows = db
                        .security
                        .members
                        .iter()
                        .filter_map(|(role, member)| Some(vec![Value::Int(db.security.principals.get(member)?.id), Value::Int(db.security.principals.get(role)?.id)]))
                        .collect();
                    Some(rs(&[("memberuid", DataType::SmallInt), ("groupuid", DataType::SmallInt)], rows))
                }
                "sysusers" => {
                    let rows = db
                        .security
                        .principals
                        .values()
                        .map(|p| vec![Value::Int(p.id), Value::Int(0), sv(&p.name), Value::Bin(p.name.to_lowercase().into_bytes()), date(db.created), date(db.created), Value::Int(p.is_role as i32), Value::Int(!p.is_role as i32), Value::Int(p.is_role as i32), Value::Int(1)])
                        .collect();
                    Some(rs(&[("uid", DataType::SmallInt), ("status", DataType::SmallInt), ("name", nv()), ("sid", DataType::VarBinary(85)), ("createdate", DataType::DateTime), ("updatedate", DataType::DateTime), ("issqlrole", DataType::Int), ("issqluser", DataType::Int), ("isaliased", DataType::Int), ("hasdbaccess", DataType::Int)], rows))
                }
                // Always On: there is no cluster
                "dm_hadr_cluster" => Some(rs(&[("cluster_name", nv()), ("quorum_type", DataType::TinyInt), ("quorum_type_desc", nv()), ("quorum_state", DataType::TinyInt), ("quorum_state_desc", nv())], vec![])),
                "dm_hadr_cluster_members" => Some(rs(&[("member_name", nv()), ("member_type", DataType::TinyInt), ("member_type_desc", nv()), ("member_state", DataType::TinyInt), ("member_state_desc", nv()), ("number_of_quorum_votes", DataType::TinyInt)], vec![])),
                // the statistics of the tables: the optimizer keeps its own, which are not objects of the catalog
                // each index has its statistics (the optimizer's own are not listed), as SQL Server has
                "stats" => {
                    let ix: Vec<Row> = self.sysview("sys", "indexes").map(|r| r.rows.iter().cloned().collect()).unwrap_or_default();
                    let rows = ix
                        .into_iter()
                        .filter(|r| !matches!(r.get(1), Some(Value::Null) | None))
                        .map(|r| vec![r[0].clone(), r[1].clone(), r[2].clone(), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null, Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Int(0), sv(""), Value::Bit(false)])
                        .collect();
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("stats_id", DataType::Int), ("auto_created", DataType::Bit), ("user_created", DataType::Bit), ("no_recompute", DataType::Bit), ("has_filter", DataType::Bit), ("filter_definition", nv()), ("is_temporary", DataType::Bit), ("is_incremental", DataType::Bit), ("has_persisted_sample", DataType::Bit), ("stats_generation_method", DataType::Int), ("stats_generation_method_desc", nv()), ("auto_drop", DataType::Bit)], rows))
                }
                "stats_columns" => {
                    let mut rows = vec![];
                    if let Some(ic) = self.sysview("sys", "index_columns") {
                        for r in ic.rows.iter() {
                            rows.push(vec![r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone()]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("stats_id", DataType::Int), ("stats_column_id", DataType::Int), ("column_id", DataType::Int)], rows))
                }
                "data_spaces" | "filegroups" => {
                    let mut cols = vec![("name", nv()), ("data_space_id", DataType::Int), ("type", nv()), ("type_desc", nv()), ("is_default", DataType::Bit), ("is_system", DataType::Bit)];
                    let mut rows = vec![vec![sv("PRIMARY"), Value::Int(1), sv("FG"), sv("ROWS_FILEGROUP"), Value::Bit(true), Value::Bit(false)]];
                    let mod_name = format!("{}_mod", dbname);
                    rows.push(vec![sv(&mod_name), Value::Int(2), sv("FX"), sv("MEMORY_OPTIMIZED_DATA_FILEGROUP"), Value::Bit(false), Value::Bit(false)]);
                    if lname == "filegroups" {
                        cols.extend([("filegroup_guid", DataType::Guid), ("log_filegroup_id", DataType::Int), ("is_read_only", DataType::Bit), ("is_autogrow_all_files", DataType::Bit)]);
                        for r in &mut rows {
                            r.extend([Value::Null, Value::Null, Value::Bit(false), Value::Bit(true)]);
                        }
                    }
                    Some(rs(&cols, rows))
                }
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
                // one row per database: its recovery bookkeeping (no log backup has been taken, so no LSN) and its FILESTREAM settings (off)
                "database_recovery_status" => {
                    let rows = self.srv.databases.values().map(|d| vec![Value::Int(d.id), Value::Guid(uuid::Uuid::from_u128(d.id as u128 + 1)), Value::Guid(uuid::Uuid::from_u128(d.id as u128 + 1)), Value::Null, Value::Null, Value::Null, Value::Null]).collect();
                    Some(rs(&[("database_id", DataType::Int), ("database_guid", DataType::Guid), ("family_guid", DataType::Guid), ("last_log_backup_lsn", DataType::Decimal(25, 0)), ("recovery_fork_guid", DataType::Guid), ("first_recovery_fork_guid", DataType::Guid), ("fork_point_lsn", DataType::Decimal(25, 0))], rows))
                }
                "database_filestream_options" => {
                    let rows = self.srv.databases.values().map(|d| vec![Value::Int(d.id), Value::Int(0), sv("OFF"), Value::Null]).collect();
                    Some(rs(&[("database_id", DataType::Int), ("non_transacted_access", DataType::TinyInt), ("non_transacted_access_desc", nv()), ("directory_name", nv())], rows))
                }
                "change_tracking_databases" => Some(rs(&[("database_id", DataType::Int), ("is_auto_cleanup_on", DataType::TinyInt), ("retention_period", DataType::Int), ("retention_period_units", DataType::TinyInt), ("retention_period_units_desc", nv())], vec![])),
                "service_broker_endpoints" => Some(rs(&[("name", nv()), ("endpoint_id", DataType::Int), ("principal_id", DataType::Int), ("protocol", DataType::TinyInt), ("protocol_desc", nv()), ("type", DataType::TinyInt), ("type_desc", nv()), ("state", DataType::TinyInt), ("state_desc", nv()), ("is_admin_endpoint", DataType::Bit), ("port", DataType::Int), ("is_dynamic_port", DataType::Bit), ("ip_address", nv())], vec![])),
                // SQL Server 2000's list of connections (SSMS counts them before it drops a database)
                "sysprocesses" => {
                    let rows = self.srv.core.session_list().into_iter().map(|s| {
                        let dbid = self.srv.databases.get(&s.db.to_lowercase()).map(|d| d.id).unwrap_or(1);
                        vec![
                            Value::Int(s.spid),
                            Value::Int(dbid),
                            sv(&s.login),
                            sv(&s.host),
                            sv(&s.app),
                            sv(s.status),
                            sv(&s.command),
                            date(s.login_time),
                            date(s.last_batch),
                            Value::Int(0),
                            Value::Int(0),
                            sv(""),
                            sv(&s.login),
                            sv(""),
                            sv("TCP/IP"),
                            Value::Int(0),
                            Value::BigInt(0),
                            Value::Int(0),
                            Value::Int(0),
                            Value::Bin(vec![1]),
                        ]
                    }).collect();
                    Some(rs(&[
                        ("spid", DataType::SmallInt),
                        ("dbid", DataType::SmallInt),
                        ("loginame", nv()),
                        ("hostname", nv()),
                        ("program_name", nv()),
                        ("status", nv()),
                        ("cmd", nv()),
                        ("login_time", DataType::DateTime),
                        ("last_batch", DataType::DateTime),
                        ("blocked", DataType::SmallInt),
                        ("waittime", DataType::BigInt),
                        ("nt_domain", nv()),
                        ("nt_username", nv()),
                        ("net_address", nv()),
                        ("net_library", nv()),
                        ("cpu", DataType::Int),
                        ("physical_io", DataType::BigInt),
                        ("memusage", DataType::Int),
                        ("open_tran", DataType::SmallInt),
                        ("sid", DataType::VarBinary(85)),
                    ], rows))
                }
                "sysdatabases" => {
                    let rows = self.srv.databases.values().map(|d| vec![sv(&d.name), Value::Int(d.id), Value::Bin(vec![1]), Value::Int(0), Value::Int(16), Value::Int(0), date(d.created), date(d.created), Value::Int(0), Value::Int(160), sv(&format!("{}.mdf", d.name)), Value::Int(957)]).collect();
                    Some(rs(&[("name", nv()), ("dbid", DataType::SmallInt), ("sid", DataType::VarBinary(85)), ("mode", DataType::SmallInt), ("status", DataType::Int), ("status2", DataType::Int), ("crdate", DataType::DateTime), ("reserved", DataType::DateTime), ("category", DataType::Int), ("cmptlevel", DataType::TinyInt), ("filename", nv()), ("version", DataType::SmallInt)], rows))
                }
                "dm_os_performance_counters" => {
                    let tx_cnt = self.srv.core.next_tx() as i64;
                    let is_mem_opt = self.srv.databases.get(&dbname.to_lowercase())
                        .and_then(|db| db.objects.get("dbo.ticketreservationdetail"))
                        .and_then(|o| match o {
                            Object::Table(t) => Some(t.memory_optimized),
                            _ => None,
                        })
                        .unwrap_or(false);
                    let latch_cnt = if is_mem_opt {
                        0
                    } else {
                        (self.srv.core.lock_waits() as i64).max(tx_cnt * 2)
                    };
                    let rows = vec![
                        vec![sv("SQLServer:Databases"), sv("Transactions/sec"), sv("_Total"), Value::BigInt(tx_cnt), Value::Int(272696576)],
                        vec![sv("SQLServer:Databases"), sv("Transactions/sec"), sv(&dbname), Value::BigInt(tx_cnt), Value::Int(272696576)],
                        vec![sv("SQLServer:Latches"), sv("Latch Waits/sec"), sv(""), Value::BigInt(latch_cnt), Value::Int(272696576)],
                        vec![sv("SQLServer:General Statistics"), sv("User Connections"), sv(""), Value::BigInt(1), Value::Int(65792)],
                        vec![sv("SQLServer:Buffer Manager"), sv("Page life expectancy"), sv(""), Value::BigInt(300), Value::Int(65792)],
                    ];
                    Some(rs(&[("object_name", nv()), ("counter_name", nv()), ("instance_name", nv()), ("cntr_value", DataType::BigInt), ("cntr_type", DataType::Int)], rows))
                }
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
                    // the other columns of sys.types: the collation, and whether the type is a table type
                    for r in rows.iter_mut() {
                        let table_type = matches!(&r[0], Value::Str(n) if db.types.values().any(|u| u.name == *n && matches!(u.kind, crate::catalog::UserTypeKind::Table(_))));
                        r.extend([Value::Null, Value::Bit(false), Value::Int(0), Value::Int(0), Value::Bit(table_type)]);
                    }
                    Some(rs(&[("name", nv()), ("user_type_id", DataType::Int), ("system_type_id", DataType::TinyInt), ("schema_id", DataType::Int), ("is_user_defined", DataType::Bit), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_nullable", DataType::Bit),
                        ("collation_name", nv()), ("is_assembly_type", DataType::Bit), ("default_object_id", DataType::Int), ("rule_object_id", DataType::Int), ("is_table_type", DataType::Bit)], rows))
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
                    // (every index is in the one filegroup, PRIMARY, whose data space id is 1; the other properties are the defaults)
                    for r in rows.iter_mut() {
                        r.extend([Value::Int(1), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(true), Value::Bit(true), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Int(0), Value::Bit(false), Value::Bit(false)]);
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("index_id", DataType::Int), ("type_desc", nv()), ("is_unique", DataType::Bit), ("is_primary_key", DataType::Bit), ("has_filter", DataType::Bit), ("filter_definition", DataType::NVarChar(MAX)), ("type", DataType::TinyInt), ("is_unique_constraint", DataType::Bit), ("is_disabled", DataType::Bit), ("fill_factor", DataType::TinyInt), ("data_space_id", DataType::Int), ("is_hypothetical", DataType::Bit), ("is_padded", DataType::Bit), ("ignore_dup_key", DataType::Bit), ("allow_row_locks", DataType::Bit), ("allow_page_locks", DataType::Bit), ("auto_created", DataType::Bit), ("no_recompute", DataType::Bit), ("optimize_for_sequential_key", DataType::Bit), ("compression_delay", DataType::Int), ("suppress_dup_key_messages", DataType::Bit), ("is_ignored_in_optimization", DataType::Bit)], rows))
                }
                "spatial_indexes" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, x) in t.spatial_indexes.iter().enumerate() {
                                let scheme = format!("{}_{}", if x.geography { "GEOGRAPHY" } else { "GEOMETRY" }, if x.auto { "AUTO_GRID" } else { "GRID" });
                                rows.push(vec![Value::Int(t.object_id), sv(&x.name), Value::Int(257_000 + k as i32), Value::Int(4), sv("SPATIAL"), Value::Int(if x.geography { 2 } else { 1 }), sv(if x.geography { "GEOGRAPHY" } else { "GEOMETRY" }), Value::Int(if x.auto { 3 } else { if x.geography { 2 } else { 1 } }), sv(&scheme), Value::Int(0), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(true), Value::Bit(true), Value::Bit(false), Value::Int(1), Value::Bit(false), Value::Null]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("index_id", DataType::Int), ("type", DataType::TinyInt), ("type_desc", nv()), ("spatial_index_type", DataType::TinyInt), ("spatial_index_type_desc", nv()), ("tessellation_scheme_id", DataType::TinyInt), ("tessellation_scheme", nv()), ("fill_factor", DataType::TinyInt), ("is_padded", DataType::Bit), ("is_disabled", DataType::Bit), ("is_hypothetical", DataType::Bit), ("allow_row_locks", DataType::Bit), ("allow_page_locks", DataType::Bit), ("ignore_dup_key", DataType::Bit), ("data_space_id", DataType::Int), ("has_filter", DataType::Bit), ("filter_definition", nv())], rows))
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
                                    Value::Int(0),
                                    Value::Bit(false),
                                    Value::Bit(false),
                                    Value::Bit(true),
                                    Value::Bit(true),
                                    Value::Bit(false),
                                    Value::Int(1),
                                    Value::Bit(false),
                                    Value::Null,
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
                            ("fill_factor", DataType::TinyInt),
                            ("is_padded", DataType::Bit),
                            ("is_hypothetical", DataType::Bit),
                            ("allow_row_locks", DataType::Bit),
                            ("allow_page_locks", DataType::Bit),
                            ("ignore_dup_key", DataType::Bit),
                            ("data_space_id", DataType::Int),
                            ("has_filter", DataType::Bit),
                            ("filter_definition", nv()),
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
                    for r in rows.iter_mut() {
                        r.push(Value::Int(0));
                    }
                    Some(rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("index_column_id", DataType::Int), ("column_id", DataType::Int), ("is_descending_key", DataType::Bit), ("is_included_column", DataType::Bit), ("key_ordinal", DataType::TinyInt), ("partition_ordinal", DataType::TinyInt), ("column_store_order_ordinal", DataType::TinyInt)], rows))
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
                                let action = |a: &str| match a.to_uppercase().replace('_', " ").as_str() {
                                    "CASCADE" => 1,
                                    "SET NULL" => 2,
                                    "SET DEFAULT" => 3,
                                    _ => 0,
                                };
                                rows.push(vec![
                                    sv(&f.name), Value::Int(t.object_id * 1000 + 1 + k as i32), Value::Int(t.object_id), Value::Int(rid), sv(&f.on_delete.replace(' ', "_")), sv(&f.on_update.replace(' ', "_")),
                                    Value::Int(sid(&t.schema)), sv("F"), sv("FOREIGN_KEY_CONSTRAINT"), date(db.created), date(db.created), Value::Bit(false), Value::Bit(!f.enabled), Value::Bit(false), Value::Bit(false),
                                    Value::Int(1), Value::Int(action(&f.on_delete)), Value::Int(action(&f.on_update)), Value::Bit(false), Value::Null,
                                ]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_object_id", DataType::Int), ("referenced_object_id", DataType::Int), ("delete_referential_action_desc", nv()), ("update_referential_action_desc", nv()),
                        ("schema_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("is_disabled", DataType::Bit), ("is_not_for_replication", DataType::Bit), ("is_not_trusted", DataType::Bit),
                        ("key_index_id", DataType::Int), ("delete_referential_action", DataType::TinyInt), ("update_referential_action", DataType::TinyInt), ("is_system_named", DataType::Bit), ("principal_id", DataType::Int)], rows))
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
                                rows.push(vec![Value::Int(t.object_id), Value::Int(idx), Value::Int(1), Value::BigInt(n), Value::Int(if cci { 3 } else { 0 }), Value::BigInt(t.object_id as i64 * 10 + idx as i64), sv(if cci { "COLUMNSTORE" } else { "NONE" }), Value::Bit(false), sv("OFF"), Value::BigInt(t.object_id as i64 * 10 + idx as i64)]);
                            }
                        }
                    }
                    Some(if stats {
                        rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("partition_number", DataType::Int), ("row_count", DataType::BigInt), ("used_page_count", DataType::BigInt), ("reserved_page_count", DataType::BigInt), ("in_row_data_page_count", DataType::BigInt)], rows)
                    } else {
                        rs(&[("object_id", DataType::Int), ("index_id", DataType::Int), ("partition_number", DataType::Int), ("rows", DataType::BigInt), ("data_compression", DataType::TinyInt), ("partition_id", DataType::BigInt), ("data_compression_desc", nv()), ("xml_compression", DataType::Bit), ("xml_compression_desc", nv()), ("hobt_id", DataType::BigInt)], rows)
                    })
                }
                "database_files" => {
                    let rows = vec![
                        vec![Value::Int(1), sv(&dbname), sv(&format!("{}.mdf", dbname)), sv("ROWS"), sv("ONLINE"), Value::Int(128), Value::Int(-1), Value::Int(0), Value::Int(0), Value::Int(1), Value::Int(0), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null],
                        vec![Value::Int(2), sv(&format!("{}_mod", dbname)), sv(&format!("{}_mod", dbname)), sv("FILESTREAM"), sv("ONLINE"), Value::Int(0), Value::Int(-1), Value::Int(0), Value::Int(2), Value::Int(2), Value::Int(0), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null],
                        vec![Value::Int(3), sv(&format!("{}_log", dbname)), sv(&format!("{}.ldf", dbname)), sv("LOG"), sv("ONLINE"), Value::Int(128), Value::Int(-1), Value::Int(0), Value::Int(1), Value::Int(0), Value::Int(0), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null],
                    ];
                    Some(rs(
                        &[("file_id", DataType::Int), ("name", nv()), ("physical_name", nv()), ("type_desc", nv()), ("state_desc", nv()), ("size", DataType::Int), ("max_size", DataType::Int), ("growth", DataType::Int), ("type", DataType::TinyInt), ("data_space_id", DataType::Int), ("state", DataType::TinyInt), ("is_media_read_only", DataType::Bit), ("is_read_only", DataType::Bit), ("is_sparse", DataType::Bit), ("is_percent_growth", DataType::Bit), ("is_name_reserved", DataType::Bit), ("drop_lsn", DataType::Decimal(25, 0))],
                        rows,
                    ))
                }
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
                                    rows.push(vec![sv(name), Value::Int(t.object_id * 1000 + 500 + i as i32), Value::Int(t.object_id), Value::Int(i as i32 + 1), sv(&format!("({})", text)), sv("D"), sv("DEFAULT_CONSTRAINT"), Value::Int(sid(&t.schema)), date(db.created), date(db.created), Value::Bit(false), Value::Bit(false), Value::Null]);
                                }
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_object_id", DataType::Int), ("parent_column_id", DataType::Int), ("definition", nv()), ("type", DataType::Char(2)), ("type_desc", nv()), ("schema_id", DataType::Int), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("is_system_named", DataType::Bit), ("principal_id", DataType::Int)], rows))
                }
                "synonyms" => {
                    let rows = db
                        .objects
                        .values()
                        .filter_map(|o| match o {
                            Object::Synonym(s) => Some(vec![sv(&s.name), Value::Int(s.object_id), Value::Int(sid(&s.schema)), sv(&s.target.iter().map(|p| format!("[{}]", p)).collect::<Vec<_>>().join(".")), sv("SN"), date(db.created), Value::Null, Value::Bit(false)]),
                            _ => None,
                        })
                        .collect();
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("schema_id", DataType::Int), ("base_object_name", nv()), ("type", DataType::Char(2)), ("create_date", DataType::DateTime), ("principal_id", DataType::Int), ("is_ms_shipped", DataType::Bit)], rows))
                }
                // descriptions (MS_Description and the like) are not kept: clients ask for them and get none
                "extended_properties" => Some(rs(&[("class", DataType::TinyInt), ("class_desc", nv()), ("major_id", DataType::Int), ("minor_id", DataType::Int), ("name", nv()), ("value", DataType::Variant)], vec![])),
                "check_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (k, c) in t.checks.iter().enumerate() {
                                let col = c.col.as_ref().and_then(|n| t.columns.iter().position(|x| x.name.eq_ignore_ascii_case(n))).map(|i| i as i32 + 1).unwrap_or(0);
                                rows.push(vec![sv(&c.name), Value::Int(t.object_id), Value::Bit(!c.enabled), Value::Int(t.object_id * 1000 + 700 + k as i32), sv(&format!("({})", c.text)), Value::Int(col), sv("C"), sv("CHECK_CONSTRAINT"), Value::Int(sid(&t.schema)), date(db.created), date(db.created), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null]);
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("parent_object_id", DataType::Int), ("is_disabled", DataType::Bit), ("object_id", DataType::Int), ("definition", nv()), ("parent_column_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()),
                        ("schema_id", DataType::Int), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("is_not_for_replication", DataType::Bit), ("is_not_trusted", DataType::Bit), ("uses_database_collation", DataType::Bit), ("is_system_named", DataType::Bit), ("principal_id", DataType::Int)], rows))
                }
                "key_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut index_id = 1;
                            if let Some(pk) = &t.pk {
                                rows.push(vec![sv(&pk.name), Value::Int(t.object_id), sv("PK"), sv("PRIMARY_KEY_CONSTRAINT"), Value::Int(t.object_id * 1000 + 900), Value::Int(sid(&t.schema)), date(db.created), date(db.created), Value::Bit(false), Value::Int(index_id), Value::Bit(false), Value::Bit(true), Value::Null]);
                                index_id += 1;
                            }
                            for (n, u) in t.uniques.iter().filter(|u| !u.is_index).enumerate() {
                                rows.push(vec![sv(&u.name), Value::Int(t.object_id), sv("UQ"), sv("UNIQUE_CONSTRAINT"), Value::Int(t.object_id * 1000 + 901 + n as i32), Value::Int(sid(&t.schema)), date(db.created), date(db.created), Value::Bit(false), Value::Int(index_id), Value::Bit(false), Value::Bit(true), Value::Null]);
                                index_id += 1;
                            }
                        }
                    }
                    Some(rs(&[("name", nv()), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()),
                        ("object_id", DataType::Int), ("schema_id", DataType::Int), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("unique_index_id", DataType::Int), ("is_system_named", DataType::Bit), ("is_enforced", DataType::Bit), ("principal_id", DataType::Int)], rows))
                }
                "sql_modules" | "all_sql_modules" | "system_sql_modules" | "server_sql_modules" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        let def = match o {
                            Object::View(v) => Some(v.text.clone()),
                            Object::Proc(p) => Some(p.text.clone()),
                            Object::Func(f) => Some(f.def.text.clone()),
                            _ => None,
                        };
                        if let Some(d) = def {
                            rows.push(vec![Value::Int(o.object_id()), sv(&d), Value::Bit(true), Value::Bit(true), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null, Value::Bit(false), Value::Bit(false), Value::Bit(false)]);
                        }
                    }
                    // triggers are modules too
                    for (_, trg) in db.triggers.values() {
                        rows.push(vec![Value::Int(trg.object_id), sv(&trg.text), Value::Bit(true), Value::Bit(true), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null, Value::Bit(false), Value::Bit(false), Value::Bit(false)]);
                    }
                    for t in &db.ddl_triggers {
                        rows.push(vec![Value::Int(t.object_id), sv(&t.text), Value::Bit(true), Value::Bit(true), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Bit(false), Value::Null, Value::Bit(false), Value::Bit(false), Value::Bit(false)]);
                    }
                    // (the system modules, the ones SQL Server ships, are not kept as text here)
                    if lname == "system_sql_modules" || lname == "server_sql_modules" {
                        rows.clear();
                    }
                    Some(rs(&[("object_id", DataType::Int), ("definition", DataType::NVarChar(MAX)), ("uses_ansi_nulls", DataType::Bit), ("uses_quoted_identifier", DataType::Bit), ("is_schema_bound", DataType::Bit), ("uses_database_collation", DataType::Bit), ("is_recompiled", DataType::Bit), ("null_on_null_input", DataType::Bit), ("execute_as_principal_id", DataType::Int), ("uses_native_compilation", DataType::Bit), ("inline_type", DataType::Bit), ("is_inlineable", DataType::Bit)], rows))
                }
                "database_principals" => {
                    let rows = db
                        .security
                        .principals
                        .values()
                        .map(|p| {
                            vec![
                                sv(&p.name), Value::Int(p.id), sv(if p.is_role { "R" } else { "S" }), sv(&p.default_schema),
                                sv(if p.is_role { "DATABASE_ROLE" } else { "SQL_USER" }), date(db.created), date(db.created), if p.is_role { Value::Int(1) } else { Value::Null }, Value::Bin(p.name.to_lowercase().into_bytes()),
                                Value::Bit(p.is_role && p.id < 16384 && p.id > 0 && p.name.starts_with("db_")), Value::Int(if p.is_role { 0 } else { 1 }), sv(if p.is_role { "NONE" } else { "INSTANCE" }),
                                Value::Null, Value::Null, Value::Bit(false),
                            ]
                        })
                        .collect();
                    Some(rs(&[("name", nv()), ("principal_id", DataType::Int), ("type", DataType::Char(1)), ("default_schema_name", nv()),
                        ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("owning_principal_id", DataType::Int), ("sid", DataType::VarBinary(85)),
                        ("is_fixed_role", DataType::Bit), ("authentication_type", DataType::Int), ("authentication_type_desc", nv()), ("default_language_name", nv()), ("default_language_lcid", DataType::Int), ("allow_encrypted_value_modifications", DataType::Bit)], rows))
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
                        rows.push(vec![sv(&trg.name), Value::Int(trg.object_id), Value::Int(1), sv("OBJECT_OR_COLUMN"), Value::Int(pid), sv("TR"), Value::Bit(trg.disabled), Value::Bit(trg.instead), sv("SQL_TRIGGER"), date(db.created), date(db.created), Value::Bit(false), Value::Bit(false)]);
                    }
                    for t in &db.ddl_triggers {
                        rows.push(vec![sv(&t.name), Value::Int(t.object_id), Value::Int(0), sv("DATABASE"), Value::Int(0), sv("TR"), Value::Bit(t.disabled), Value::Bit(false), sv("SQL_TRIGGER"), date(db.created), date(db.created), Value::Bit(false), Value::Bit(false)]);
                    }
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_class", DataType::TinyInt), ("parent_class_desc", nv()), ("parent_id", DataType::Int), ("type", DataType::Char(2)), ("is_disabled", DataType::Bit), ("is_instead_of_trigger", DataType::Bit),
                        ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("is_not_for_replication", DataType::Bit)], rows))
                }
                "trigger_events" => {
                    let mut rows: Vec<Row> = vec![];
                    for t in &db.ddl_triggers {
                        for e in &t.events {
                            rows.push(vec![Value::Int(t.object_id), Value::Int(trigger_event_type(e)), sv(e), Value::Bit(false), Value::Bit(false), Value::Null, Value::Null, Value::Bit(true)]);
                        }
                    }
                    for (_, trg) in db.triggers.values() {
                        for e in &trg.events {
                            rows.push(vec![Value::Int(trg.object_id), Value::Int(trigger_event_type(e)), sv(e), Value::Bit(false), Value::Bit(false), Value::Null, Value::Null, Value::Bit(true)]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("type", DataType::Int), ("type_desc", nv()), ("is_first", DataType::Bit), ("is_last", DataType::Bit), ("event_group_type", DataType::Int), ("event_group_type_desc", nv()), ("is_trigger_event", DataType::Bit)], rows))
                }
                "trigger_event_types" => {
                    let evs = [
                        (1, "INSERT", 0), (2, "UPDATE", 0), (3, "DELETE", 0),
                        (21, "CREATE_TABLE", 1000), (22, "ALTER_TABLE", 1000), (23, "DROP_TABLE", 1000),
                        (24, "CREATE_INDEX", 1000), (25, "ALTER_INDEX", 1000), (26, "DROP_INDEX", 1000),
                        (41, "CREATE_VIEW", 1000), (42, "ALTER_VIEW", 1000), (43, "DROP_VIEW", 1000),
                        (51, "CREATE_PROCEDURE", 1000), (52, "ALTER_PROCEDURE", 1000), (53, "DROP_PROCEDURE", 1000),
                        (61, "CREATE_FUNCTION", 1000), (62, "ALTER_FUNCTION", 1000), (63, "DROP_FUNCTION", 1000),
                        (71, "CREATE_TRIGGER", 1000), (72, "ALTER_TRIGGER", 1000), (73, "DROP_TRIGGER", 1000),
                        (81, "CREATE_SCHEMA", 1000), (82, "ALTER_SCHEMA", 1000), (83, "DROP_SCHEMA", 1000),
                        (91, "CREATE_TYPE", 1000), (93, "DROP_TYPE", 1000),
                        (101, "CREATE_DATABASE", 1000), (102, "ALTER_DATABASE", 1000), (103, "DROP_DATABASE", 1000),
                        (131, "CREATE_USER", 1000), (132, "ALTER_USER", 1000), (133, "DROP_USER", 1000),
                        (134, "CREATE_ROLE", 1000), (135, "ALTER_ROLE", 1000), (136, "DROP_ROLE", 1000),
                        (141, "CREATE_LOGIN", 1000), (142, "ALTER_LOGIN", 1000), (143, "DROP_LOGIN", 1000),
                        (21, "LOGON", 0),
                    ];
                    let rows = evs.iter().map(|(id, name, parent)| vec![Value::Int(*id), sv(name), Value::Int(*parent)]).collect();
                    Some(rs(&[("type", DataType::Int), ("type_desc", nv()), ("parent_type", DataType::Int)], rows))
                }
                "server_triggers" => {
                    let rows = self
                        .srv
                        .security
                        .server_triggers
                        .iter()
                        .map(|t| vec![sv(&t.name), Value::Int(t.object_id), sv("SERVER"), Value::Bit(t.disabled), Value::Int(100), Value::Int(0), sv("TR"), sv("SQL_TRIGGER"), date(self.srv.start_time), date(self.srv.start_time), Value::Bit(false), Value::Bit(false), Value::Bit(false)])
                        .collect();
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("parent_class_desc", nv()), ("is_disabled", DataType::Bit), ("parent_class", DataType::TinyInt), ("parent_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()),
                        ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("is_not_for_replication", DataType::Bit), ("is_instead_of_trigger", DataType::Bit)], rows))
                }
                "server_trigger_events" => {
                    let mut rows: Vec<Row> = vec![];
                    for t in &self.srv.security.server_triggers {
                        for e in &t.events {
                            rows.push(vec![Value::Int(t.object_id), Value::Int(trigger_event_type(e)), sv(e), Value::Bit(false), Value::Bit(false), Value::Null, Value::Null, Value::Bit(true)]);
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("type", DataType::Int), ("type_desc", nv()), ("is_first", DataType::Bit), ("is_last", DataType::Bit), ("event_group_type", DataType::Int), ("event_group_type_desc", nv()), ("is_trigger_event", DataType::Bit)], rows))
                }
                "spt_values" => {
                    let mut rows: Vec<Row> = vec![];
                    // Page size (E) for space used calculations: 8192 bytes
                    rows.push(vec![sv("PAGE"), Value::Int(1), sv("E"), Value::Int(8192), Value::Null, Value::Int(0)]);
                    // Lock types (L)
                    for (n, name) in [(0, "NULL"), (1, "Sch-S"), (2, "Sch-M"), (3, "S"), (4, "U"), (5, "X"), (6, "IS"), (7, "IU"), (8, "IX"), (9, "SIU"), (10, "SIX")] {
                        rows.push(vec![sv(name), Value::Int(n), sv("L"), Value::Null, Value::Null, Value::Int(0)]);
                    }
                    // Database roles (DBR)
                    for (n, name) in [
                        (16384, "db_owner"), (16385, "db_accessadmin"), (16386, "db_securityadmin"),
                        (16387, "db_ddladmin"), (16388, "db_backupoperator"), (16389, "db_datareader"),
                        (16390, "db_datawriter"), (16391, "db_denydatareader"), (16392, "db_denydatawriter"),
                    ] {
                        rows.push(vec![sv(name), Value::Int(n), sv("DBR"), Value::Null, Value::Null, Value::Int(0)]);
                    }
                    // Index properties (I)
                    for (n, name) in [
                        (1, "ignore_dup_key"), (2, "unique"), (4, "ignore_dup_row"),
                        (16, "clustered"), (32, "hypothetical"), (64, "statistics"),
                        (8388608, "auto_created"), (16777216, "no_recompute"),
                    ] {
                        rows.push(vec![sv(name), Value::Int(n), sv("I"), Value::Null, Value::Null, Value::Int(0)]);
                    }
                    // Permission protect types & actions (T)
                    for (n, name) in [
                        (193, "SELECT"), (195, "INSERT"), (196, "DELETE"), (197, "UPDATE"), (224, "EXECUTE"), (26, "REFERENCES"),
                    ] {
                        rows.push(vec![sv(name), Value::Int(n), sv("T"), Value::Null, Value::Null, Value::Int(0)]);
                    }
                    // Database options (D)
                    for (n, name) in [
                        (1, "autoclose"), (2, "bulkcopy"), (4, "trunc. log"), (8, "read only"),
                        (16, "dbo use"), (32, "single user"), (64, "autoshrink"), (128, "ANSI null default"),
                        (256, "recursive triggers"), (512, "ANSI nulls"), (1024, "concat null yields null"),
                        (2048, "cursor close on commit"), (4096, "default to local cursor"),
                        (8192, "quoted identifier"), (16384, "ANSI warnings"), (32768, "auto create statistics"),
                        (65536, "auto update statistics"), (1048576, "torn page detection"), (2097152, "offline"),
                    ] {
                        rows.push(vec![sv(name), Value::Int(n), sv("D"), Value::Null, Value::Null, Value::Int(0)]);
                    }
                    // Number sequences (P): 0..=2047
                    for i in 0..=2047 {
                        rows.push(vec![Value::Null, Value::Int(i), sv("P"), Value::Null, Value::Null, Value::Int(0)]);
                    }
                    Some(rs(&[("name", DataType::NVarChar(35)), ("number", DataType::Int), ("type", DataType::NChar(3)), ("low", DataType::Int), ("high", DataType::Int), ("status", DataType::Int)], rows))
                }
                "database_permissions" => {
                    // the abbreviation SQL Server keeps for each permission (`type`)
                    let code = |p: &str| match p.to_uppercase().as_str() {
                        "SELECT" => "SL", "INSERT" => "IN", "UPDATE" => "UP", "DELETE" => "DL", "EXECUTE" => "EX", "REFERENCES" => "RF", "ALTER" => "AL", "CONTROL" => "CL",
                        "CONNECT" => "CO", "VIEW DEFINITION" => "VW", "TAKE OWNERSHIP" => "TO", "CREATE TABLE" => "CRTB", "CREATE VIEW" => "CRVW", "CREATE PROCEDURE" => "CRPR",
                        "CREATE FUNCTION" => "CRFN", "CREATE SCHEMA" => "CRSC", "CREATE ROLE" => "CRRL", "CREATE DEFAULT" => "CRDF", "CREATE RULE" => "CRRU", "CREATE TYPE" => "CRTY",
                        "VIEW ANY DEFINITION" => "VWAD", "BACKUP DATABASE" => "BADB", "BACKUP LOG" => "BALO", "SHOWPLAN" => "SPLN", "UNMASK" => "UMSK",
                        _ => "",
                    };
                    let mut rows: Vec<Row> = db
                        .security
                        .perms
                        .iter()
                        .map(|e| {
                            let pid = db.security.principals.get(&e.grantee).map(|p| p.id).unwrap_or(0);
                            let (class, class_desc, major) = match &e.target {
                                crate::security::PermTarget::Database => (0, "DATABASE", 0),
                                crate::security::PermTarget::Schema(s) => (3, "SCHEMA", sid(s)),
                                crate::security::PermTarget::Object(k) | crate::security::PermTarget::Column(k, _) => (1, "OBJECT_OR_COLUMN", db.objects.get(k).map(|o| o.object_id()).unwrap_or(0)),
                            };
                            let granted = e.state == crate::security::PermState::Grant;
                            vec![Value::Int(class), sv(class_desc), Value::Int(major), Value::Int(0), Value::Int(pid), Value::Int(1), sv(code(&e.perm)), sv(&e.perm), sv(if granted { "G" } else { "D" }), sv(if granted { "GRANT" } else { "DENY" })]
                        })
                        .collect();
                    // (every user may connect to the database it is in, as SQL Server grants it when the user is made)
                    for p in db.security.principals.values().filter(|p| !p.is_role) {
                        rows.push(vec![Value::Int(0), sv("DATABASE"), Value::Int(0), Value::Int(0), Value::Int(p.id), Value::Int(1), sv("CO"), sv("CONNECT"), sv("G"), sv("GRANT")]);
                    }
                    Some(rs(&[("class", DataType::TinyInt), ("class_desc", nv()), ("major_id", DataType::Int), ("minor_id", DataType::Int), ("grantee_principal_id", DataType::Int), ("grantor_principal_id", DataType::Int), ("type", DataType::Char(4)), ("permission_name", nv()), ("state", DataType::Char(1)), ("state_desc", nv())], rows))
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
                            vec![Value::Int(s.spid), sv(&s.login), sv(&s.host), sv(&s.app), Value::Int(dbid), Value::Int(s.isolation), sv(s.status), date(s.login_time), date(s.last_batch), Value::BigInt(s.batches as i64), Value::Int(1)]
                        })
                        .collect();
                    Some(rs(&[("session_id", DataType::SmallInt), ("login_name", nv()), ("host_name", nv()), ("program_name", nv()), ("database_id", DataType::SmallInt), ("transaction_isolation_level", DataType::SmallInt), ("status", nv()), ("login_time", DataType::DateTime), ("last_request_start_time", DataType::DateTime), ("batch_count", DataType::BigInt), ("authenticating_database_id", DataType::Int)], rows))
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
                    // configuration_id: SQL Server's numbers for the options it has (a client asks for one by number)
                    let id_of = |n: &str| match n.to_lowercase().as_str() {
                        "recovery interval (min)" => 101,
                        "allow updates" => 102,
                        "user connections" => 103,
                        "locks" => 106,
                        "open objects" => 107,
                        "fill factor (%)" => 109,
                        "disallow results from triggers" => 114,
                        "nested triggers" => 115,
                        "server trigger recursion" => 116,
                        "remote access" => 117,
                        "default language" => 124,
                        "cross db ownership chaining" => 400,
                        "max text repl size (b)" => 505,
                        "show advanced options" => 518,
                        "user options" => 1534,
                        "min server memory (mb)" => 1543,
                        "max server memory (mb)" => 1544,
                        "cost threshold for parallelism" => 1538,
                        "max degree of parallelism" => 1539,
                        "clr enabled" => 1562,
                        "remote admin connections" => 1576,
                        "contained database authentication" => 1577,
                        "database mail xps" => 16386,
                        "ole automation procedures" => 16388,
                        "xp_cmdshell" => 16390,
                        "ad hoc distributed queries" => 16391,
                        "agent xps" => 16384,
                        _ => 0,
                    };
                    let mut rows: Vec<Row> = self.srv.core.config_list().into_iter().map(|o| vec![sv(o.name), Value::BigInt(o.config), Value::BigInt(o.minimum), Value::BigInt(o.maximum), Value::BigInt(o.run), Value::Bit(o.dynamic), Value::Bit(o.advanced), sv(o.description), Value::Int(id_of(o.name))]).collect();
                    // the options of the SQL Server Agent and Database Mail, which this server does not have: off
                    for (n, id) in [("Agent XPs", 16384), ("Database Mail XPs", 16386)] {
                        if !rows.iter().any(|r| matches!(&r[0], Value::Str(x) if x.eq_ignore_ascii_case(n))) {
                            rows.push(vec![sv(n), Value::BigInt(0), Value::BigInt(0), Value::BigInt(1), Value::BigInt(0), Value::Bit(true), Value::Bit(true), sv("Enable or disable the feature"), Value::Int(id)]);
                        }
                    }
                    // the options SMO reads when it scripts a table (by number: 115, 124, 1126, 1127, 1555), which have their defaults here
                    for (n, id, v, lo, hi, adv, d) in [
                        ("nested triggers", 115, 1i64, 0i64, 1i64, false, "Allow triggers to be invoked within triggers"),
                        ("default full-text language", 1126, 1033, 0, 2147483647, true, "default full-text language"),
                        ("two digit year cutoff", 1127, 2049, 1753, 9999, true, "two digit year cutoff for century"),
                        ("transform noise words", 1555, 0, 0, 1, true, "Transform noise words for full-text query"),
                    ] {
                        if !rows.iter().any(|r| matches!(&r[0], Value::Str(x) if x.eq_ignore_ascii_case(n)) || matches!(r.last(), Some(Value::Int(i)) if *i == id)) {
                            rows.push(vec![sv(n), Value::BigInt(v), Value::BigInt(lo), Value::BigInt(hi), Value::BigInt(v), Value::Bit(true), Value::Bit(adv), sv(d), Value::Int(id)]);
                        }
                    }
                    Some(rs(&[("name", nv()), ("value", DataType::BigInt), ("minimum", DataType::BigInt), ("maximum", DataType::BigInt), ("value_in_use", DataType::BigInt), ("is_dynamic", DataType::Bit), ("is_advanced", DataType::Bit), ("description", nv()), ("configuration_id", DataType::Int)], rows))
                }
                "dm_os_sys_info" => {
                    let cpus = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(1);
                    Some(rs(
                        &[("cpu_count", DataType::Int), ("hyperthread_ratio", DataType::Int), ("scheduler_count", DataType::Int), ("max_workers_count", DataType::Int), ("sqlserver_start_time", DataType::DateTime), ("affinity_type", DataType::Int), ("affinity_type_desc", nv())],
                        vec![vec![Value::Int(cpus), Value::Int(1), Value::Int(cpus), Value::Int(cpus * 64), date(self.srv.start_time), Value::Int(2), sv("AUTO")]],
                    ))
                }
                // the machine the server runs on, as SQL Server shows it (SSMS reads it when it connects)
                "dm_os_enumerate_fixed_drives" => {
                    let rows: Vec<Row> = super::sysprocs::fixed_drives().into_iter().map(|(d, mb)| {
                        let path = if cfg!(windows) { format!("{}:", d) } else { d };
                        vec![sv(&path), Value::BigInt(mb as i64 * 1024 * 1024), sv("Fixed")]
                    }).collect();
                    Some(rs(&[("fixed_drive_path", nv()), ("free_space_in_bytes", DataType::BigInt), ("drive_type_desc", nv())], rows))
                }
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
                        rows.push(vec![
                            cat.clone(), sv(o.schema()), sv(o.name()), sv(kind), sv(&def),
                            cat.clone(), sv(o.schema()), sv(o.name()), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null,
                            sv("SQL"), sv("NO"), sv("MODIFIES"), sv("YES"), sv("YES"), date(db.created), date(db.created),
                        ]);
                    }
                    Some(rs(
                        &[
                            ("ROUTINE_CATALOG", nv()), ("ROUTINE_SCHEMA", nv()), ("ROUTINE_NAME", nv()), ("ROUTINE_TYPE", nv()), ("ROUTINE_DEFINITION", DataType::NVarChar(MAX)),
                            ("SPECIFIC_CATALOG", nv()), ("SPECIFIC_SCHEMA", nv()), ("SPECIFIC_NAME", nv()), ("DATA_TYPE", nv()), ("CHARACTER_MAXIMUM_LENGTH", DataType::Int), ("CHARACTER_OCTET_LENGTH", DataType::Int),
                            ("NUMERIC_PRECISION", DataType::TinyInt), ("NUMERIC_PRECISION_RADIX", DataType::SmallInt), ("NUMERIC_SCALE", DataType::Int), ("DATETIME_PRECISION", DataType::SmallInt), ("COLLATION_NAME", nv()),
                            ("ROUTINE_BODY", nv()), ("IS_DETERMINISTIC", nv()), ("SQL_DATA_ACCESS", nv()), ("IS_NULL_CALL", nv()), ("SCHEMA_LEVEL_ROUTINE", nv()), ("CREATED", DataType::DateTime), ("LAST_ALTERED", DataType::DateTime),
                        ],
                        rows,
                    ))
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
                // the rest of INFORMATION_SCHEMA (clients such as Beekeeper Studio and SSRS's designers read them)
                "parameters" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        let params: &[crate::ast::Param] = match o {
                            Object::Proc(p) => &p.params[..],
                            Object::Func(f) => &f.def.params[..],
                            _ => continue,
                        };
                        for (i, p) in params.iter().enumerate() {
                            let dt = p.ty.parts.last().and_then(|n| DataType::from_name(n, &p.ty.args, false).ok());
                            let is_num = dt.as_ref().is_some_and(|d| d.is_numeric());
                            let is_str = dt.as_ref().is_some_and(|d| d.is_string());
                            let (_, _, pr, sc) = dt.as_ref().map(type_info).unwrap_or((0, 0, 0, 0));
                            let nm = if p.name.starts_with('@') { p.name.clone() } else { format!("@{}", p.name) };
                            rows.push(vec![
                                cat.clone(), sv(o.schema()), sv(o.name()), Value::Int(i as i32 + 1), sv(if p.out { "INOUT" } else { "IN" }), sv("NO"), sv("NO"), sv(&nm),
                                sv(&dt.as_ref().map(type_name).unwrap_or_else(|| p.ty.parts.last().cloned().unwrap_or_default())),
                                if is_str { dt.as_ref().and_then(|d| d.length()).map(Value::Int).unwrap_or(Value::Null) } else { Value::Null },
                                Value::Null,
                                if is_str { cat.clone() } else { Value::Null }, if is_str { sv("sys") } else { Value::Null }, if is_str { sv(&db.collation_or_default()) } else { Value::Null },
                                Value::Null, Value::Null, Value::Null,
                                if is_num { Value::Int(pr) } else { Value::Null }, if is_num { Value::Int(10) } else { Value::Null }, if is_num { Value::Int(sc) } else { Value::Null },
                                Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null,
                            ]);
                        }
                    }
                    Some(rs(&[
                        ("SPECIFIC_CATALOG", nv()), ("SPECIFIC_SCHEMA", nv()), ("SPECIFIC_NAME", nv()), ("ORDINAL_POSITION", DataType::Int), ("PARAMETER_MODE", nv()), ("IS_RESULT", nv()), ("AS_LOCATOR", nv()), ("PARAMETER_NAME", nv()),
                        ("DATA_TYPE", nv()), ("CHARACTER_MAXIMUM_LENGTH", DataType::Int), ("CHARACTER_OCTET_LENGTH", DataType::Int), ("COLLATION_CATALOG", nv()), ("COLLATION_SCHEMA", nv()), ("COLLATION_NAME", nv()),
                        ("CHARACTER_SET_CATALOG", nv()), ("CHARACTER_SET_SCHEMA", nv()), ("CHARACTER_SET_NAME", nv()), ("NUMERIC_PRECISION", DataType::TinyInt), ("NUMERIC_PRECISION_RADIX", DataType::SmallInt), ("NUMERIC_SCALE", DataType::Int),
                        ("DATETIME_PRECISION", DataType::SmallInt), ("INTERVAL_TYPE", nv()), ("INTERVAL_PRECISION", DataType::SmallInt), ("USER_DEFINED_TYPE_CATALOG", nv()), ("USER_DEFINED_TYPE_SCHEMA", nv()), ("USER_DEFINED_TYPE_NAME", nv()),
                        ("SCOPE_CATALOG", nv()), ("SCOPE_SCHEMA", nv()), ("SCOPE_NAME", nv()),
                    ], rows))
                }
                "referential_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for f in &t.fks {
                                let (usch, uname) = match db.objects.get(&f.ref_key) {
                                    Some(Object::Table(p)) => {
                                        let by_cols = |cols: &Vec<usize>| cols.len() == f.ref_cols.len() && cols.iter().zip(&f.ref_cols).all(|(&c, n)| p.columns.get(c).is_some_and(|x| x.name.eq_ignore_ascii_case(n)));
                                        let name = p.pk.as_ref().filter(|k| by_cols(&k.cols)).map(|k| k.name.clone()).or_else(|| p.uniques.iter().find(|u| by_cols(&u.cols)).map(|u| u.name.clone()));
                                        (Value::Str(p.schema.clone()), name.map(Value::Str).unwrap_or(Value::Null))
                                    }
                                    _ => (Value::Null, Value::Null),
                                };
                                let rule = |r: &str| sv(&if r.trim().is_empty() { "NO ACTION".to_string() } else { r.to_uppercase() });
                                rows.push(vec![cat.clone(), sv(&t.schema), sv(&f.name), if matches!(uname, Value::Null) { Value::Null } else { cat.clone() }, usch, uname, sv("SIMPLE"), rule(&f.on_update), rule(&f.on_delete)]);
                            }
                        }
                    }
                    Some(rs(&[("CONSTRAINT_CATALOG", nv()), ("CONSTRAINT_SCHEMA", nv()), ("CONSTRAINT_NAME", nv()), ("UNIQUE_CONSTRAINT_CATALOG", nv()), ("UNIQUE_CONSTRAINT_SCHEMA", nv()), ("UNIQUE_CONSTRAINT_NAME", nv()), ("MATCH_OPTION", DataType::VarChar(7)), ("UPDATE_RULE", DataType::VarChar(11)), ("DELETE_RULE", DataType::VarChar(11))], rows))
                }
                "constraint_column_usage" | "constraint_table_usage" => {
                    let by_column = lname == "constraint_column_usage";
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let mut add = |n: &str, cols: Vec<&str>| {
                                if by_column {
                                    for c in cols {
                                        rows.push(vec![cat.clone(), sv(&t.schema), sv(&t.name), sv(c), cat.clone(), sv(&t.schema), sv(n)]);
                                    }
                                } else {
                                    rows.push(vec![cat.clone(), sv(&t.schema), sv(&t.name), cat.clone(), sv(&t.schema), sv(n)]);
                                }
                            };
                            let names = |cols: &Vec<usize>| cols.iter().filter_map(|&i| t.columns.get(i).map(|c| c.name.as_str())).collect::<Vec<_>>();
                            if let Some(pk) = &t.pk {
                                add(&pk.name, names(&pk.cols));
                            }
                            for u in t.uniques.iter().filter(|u| !u.is_index) {
                                add(&u.name, names(&u.cols));
                            }
                            for c in &t.checks {
                                add(&c.name, c.col.as_deref().into_iter().collect());
                            }
                            // (a foreign key is listed under the table and columns it refers to)
                            for f in &t.fks {
                                if let Some(Object::Table(p)) = db.objects.get(&f.ref_key) {
                                    if by_column {
                                        for c in &f.ref_cols {
                                            rows.push(vec![cat.clone(), sv(&p.schema), sv(&p.name), sv(c), cat.clone(), sv(&t.schema), sv(&f.name)]);
                                        }
                                    } else {
                                        rows.push(vec![cat.clone(), sv(&p.schema), sv(&p.name), cat.clone(), sv(&t.schema), sv(&f.name)]);
                                    }
                                }
                            }
                        }
                    }
                    let mut cols = vec![("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv())];
                    if by_column {
                        cols.push(("COLUMN_NAME", nv()));
                    }
                    cols.extend([("CONSTRAINT_CATALOG", nv()), ("CONSTRAINT_SCHEMA", nv()), ("CONSTRAINT_NAME", nv())]);
                    Some(rs(&cols, rows))
                }
                "check_constraints" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for c in &t.checks {
                                rows.push(vec![cat.clone(), sv(&t.schema), sv(&c.name), sv(&format!("({})", c.text))]);
                            }
                        }
                    }
                    Some(rs(&[("CONSTRAINT_CATALOG", nv()), ("CONSTRAINT_SCHEMA", nv()), ("CONSTRAINT_NAME", nv()), ("CHECK_CLAUSE", DataType::NVarChar(MAX))], rows))
                }
                // the tables a view reads, and the columns of them it names (a column counts when its name is a word of the view's text)
                "view_table_usage" | "view_column_usage" => {
                    let by_column = lname == "view_column_usage";
                    let mut rows = vec![];
                    let words_of = |text: &str| -> std::collections::HashSet<String> { text.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect() };
                    let mut seen = std::collections::HashSet::new();
                    for d in self.dependency_rows() {
                        let Some(Object::View(v)) = db.objects.get(&format!("{}.{}", d.referencing_schema, d.referencing_name).to_lowercase()) else { continue };
                        let Some(Object::Table(t)) = db.objects.get(&format!("{}.{}", d.ref_schema, d.ref_name).to_lowercase()) else { continue };
                        if !seen.insert((v.object_id, t.object_id)) {
                            continue;
                        }
                        if by_column {
                            let words = words_of(&v.text);
                            for c in t.columns.iter().filter(|c| words.contains(&c.name.to_lowercase())) {
                                rows.push(vec![cat.clone(), sv(&v.schema), sv(&v.name), cat.clone(), sv(&t.schema), sv(&t.name), sv(&c.name)]);
                            }
                        } else {
                            rows.push(vec![cat.clone(), sv(&v.schema), sv(&v.name), cat.clone(), sv(&t.schema), sv(&t.name)]);
                        }
                    }
                    let mut cols = vec![("VIEW_CATALOG", nv()), ("VIEW_SCHEMA", nv()), ("VIEW_NAME", nv()), ("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv())];
                    if by_column {
                        cols.push(("COLUMN_NAME", nv()));
                    }
                    Some(rs(&cols, rows))
                }
                // the columns of the table-valued functions are not listed
                "routine_columns" => Some(rs(
                    &[("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("COLUMN_NAME", nv()), ("ORDINAL_POSITION", DataType::Int), ("COLUMN_DEFAULT", DataType::NVarChar(MAX)), ("IS_NULLABLE", DataType::VarChar(3)), ("DATA_TYPE", nv()),
                      ("CHARACTER_MAXIMUM_LENGTH", DataType::Int), ("CHARACTER_OCTET_LENGTH", DataType::Int), ("NUMERIC_PRECISION", DataType::TinyInt), ("NUMERIC_PRECISION_RADIX", DataType::SmallInt), ("NUMERIC_SCALE", DataType::Int), ("DATETIME_PRECISION", DataType::SmallInt)],
                    vec![],
                )),
                "domains" => {
                    let mut rows = vec![];
                    for u in db.types.values() {
                        if let crate::catalog::UserTypeKind::Alias(dt, _) = &u.kind {
                            let is_num = dt.is_numeric();
                            let is_str = dt.is_string();
                            let (_, _, pr, sc) = type_info(dt);
                            rows.push(vec![
                                cat.clone(), sv(&u.schema), sv(&u.name), sv(&type_name(dt)),
                                if is_str { dt.length().map(Value::Int).unwrap_or(Value::Null) } else { Value::Null }, Value::Null,
                                if is_str { cat.clone() } else { Value::Null }, if is_str { sv("sys") } else { Value::Null }, if is_str { sv(&db.collation_or_default()) } else { Value::Null },
                                Value::Null, Value::Null, Value::Null,
                                if is_num { Value::Int(pr) } else { Value::Null }, if is_num { Value::Int(10) } else { Value::Null }, if is_num { Value::Int(sc) } else { Value::Null },
                                Value::Null, Value::Null,
                            ]);
                        }
                    }
                    Some(rs(&[
                        ("DOMAIN_CATALOG", nv()), ("DOMAIN_SCHEMA", nv()), ("DOMAIN_NAME", nv()), ("DATA_TYPE", nv()), ("CHARACTER_MAXIMUM_LENGTH", DataType::Int), ("CHARACTER_OCTET_LENGTH", DataType::Int),
                        ("COLLATION_CATALOG", nv()), ("COLLATION_SCHEMA", nv()), ("COLLATION_NAME", nv()), ("CHARACTER_SET_CATALOG", nv()), ("CHARACTER_SET_SCHEMA", nv()), ("CHARACTER_SET_NAME", nv()),
                        ("NUMERIC_PRECISION", DataType::TinyInt), ("NUMERIC_PRECISION_RADIX", DataType::SmallInt), ("NUMERIC_SCALE", DataType::Int), ("DATETIME_PRECISION", DataType::SmallInt), ("DOMAIN_DEFAULT", DataType::NVarChar(MAX)),
                    ], rows))
                }
                // grants are not listed here (see sys.database_permissions)
                "table_privileges" => Some(rs(&[("GRANTOR", nv()), ("GRANTEE", nv()), ("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("PRIVILEGE_TYPE", DataType::VarChar(10)), ("IS_GRANTABLE", DataType::VarChar(3))], vec![])),
                "column_privileges" => Some(rs(&[("GRANTOR", nv()), ("GRANTEE", nv()), ("TABLE_CATALOG", nv()), ("TABLE_SCHEMA", nv()), ("TABLE_NAME", nv()), ("COLUMN_NAME", nv()), ("PRIVILEGE_TYPE", DataType::VarChar(10)), ("IS_GRANTABLE", DataType::VarChar(3))], vec![])),
                "sequences" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Seq(s) = o {
                            let (_, _, pr, _) = type_info(&s.ty);
                            rows.push(vec![cat.clone(), sv(&s.schema), sv(&s.name), sv(&type_name(&s.ty)), Value::Int(pr), Value::Int(10), Value::Int(0), Value::BigInt(s.start), Value::BigInt(s.min), Value::BigInt(s.max), Value::BigInt(s.inc), sv(if s.cycle { "YES" } else { "NO" })]);
                        }
                    }
                    Some(rs(&[("SEQUENCE_CATALOG", nv()), ("SEQUENCE_SCHEMA", nv()), ("SEQUENCE_NAME", nv()), ("DATA_TYPE", nv()), ("NUMERIC_PRECISION", DataType::TinyInt), ("NUMERIC_PRECISION_RADIX", DataType::SmallInt), ("NUMERIC_SCALE", DataType::Int),
                      ("START_VALUE", DataType::BigInt), ("MINIMUM_VALUE", DataType::BigInt), ("MAXIMUM_VALUE", DataType::BigInt), ("INCREMENT", DataType::BigInt), ("CYCLE_OPTION", DataType::VarChar(3))], rows))
                }
                _ => None,
            }
        }
    }
}

fn trigger_event_type(e: &str) -> i32 {
    match e.to_ascii_uppercase().as_str() {
        "INSERT" => 1,
        "UPDATE" => 2,
        "DELETE" => 3,
        "CREATE_TABLE" => 21,
        "ALTER_TABLE" => 22,
        "DROP_TABLE" => 23,
        "CREATE_INDEX" => 24,
        "ALTER_INDEX" => 25,
        "DROP_INDEX" => 26,
        "CREATE_VIEW" => 41,
        "ALTER_VIEW" => 42,
        "DROP_VIEW" => 43,
        "CREATE_PROCEDURE" => 51,
        "ALTER_PROCEDURE" => 52,
        "DROP_PROCEDURE" => 53,
        "CREATE_FUNCTION" => 61,
        "ALTER_FUNCTION" => 62,
        "DROP_FUNCTION" => 63,
        "CREATE_TRIGGER" => 71,
        "ALTER_TRIGGER" => 72,
        "DROP_TRIGGER" => 73,
        "CREATE_SCHEMA" => 81,
        "ALTER_SCHEMA" => 82,
        "DROP_SCHEMA" => 83,
        "CREATE_TYPE" => 91,
        "DROP_TYPE" => 93,
        "CREATE_DATABASE" => 101,
        "ALTER_DATABASE" => 102,
        "DROP_DATABASE" => 103,
        "CREATE_USER" => 131,
        "ALTER_USER" => 132,
        "DROP_USER" => 133,
        "CREATE_ROLE" => 134,
        "ALTER_ROLE" => 135,
        "DROP_ROLE" => 136,
        "CREATE_LOGIN" => 141,
        "ALTER_LOGIN" => 142,
        "DROP_LOGIN" => 143,
        "LOGON" => 21,
        _ => 1000,
    }
}
