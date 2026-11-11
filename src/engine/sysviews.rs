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
                    Some(rs(&[("name", nv()), ("object_id", DataType::Int), ("schema_id", DataType::Int), ("parent_object_id", DataType::Int), ("type", DataType::Char(2)), ("type_desc", nv()), ("create_date", DataType::DateTime), ("modify_date", DataType::DateTime), ("is_ms_shipped", DataType::Bit), ("principal_id", DataType::Int)], rows))
                }
                "columns" | "all_columns" => {
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            for (i, c) in t.columns.iter().enumerate() {
                                let (st, ml, p, s) = type_info(&c.ty);
                                rows.push(vec![Value::Int(t.object_id), sv(&c.name), Value::Int(i as i32 + 1), Value::Int(if matches!(c.ty, DataType::HierarchyId) { 240 } else { st }), Value::Int(st), Value::Int(ml), Value::Int(p), Value::Int(s), Value::Bit(c.nullable), Value::Bit(c.identity.is_some()), Value::Bit(c.computed.is_some()), if c.ty.is_string() { sv(c.collation.as_deref().unwrap_or(&db.collation_or_default())) } else { Value::Null }, Value::Int(if c.default_name.is_some() { t.object_id * 1000 + 500 + i as i32 } else { 0 })]);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("column_id", DataType::Int), ("system_type_id", DataType::Int), ("user_type_id", DataType::Int), ("max_length", DataType::SmallInt), ("precision", DataType::TinyInt), ("scale", DataType::TinyInt), ("is_nullable", DataType::Bit), ("is_identity", DataType::Bit), ("is_computed", DataType::Bit), ("collation_name", nv()), ("default_object_id", DataType::Int)], rows))
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
                    let builtin = [("int", 56), ("bigint", 127), ("smallint", 52), ("tinyint", 48), ("bit", 104), ("decimal", 106), ("numeric", 108), ("money", 60), ("smallmoney", 122), ("float", 62), ("real", 59), ("char", 175), ("varchar", 167), ("nchar", 239), ("nvarchar", 231), ("text", 35), ("ntext", 99), ("binary", 173), ("varbinary", 165), ("image", 34), ("date", 40), ("time", 41), ("datetime", 61), ("smalldatetime", 58), ("datetime2", 42), ("datetimeoffset", 43), ("uniqueidentifier", 36), ("xml", 241), ("sql_variant", 98), ("timestamp", 189), ("hierarchyid", 128)];
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
                        rows.push(vec![sv(&u.name), Value::Int(u.type_id), Value::Int(0), Value::Int(sid(&u.schema)), Value::Bit(true), Value::Int(0), Value::Int(0), Value::Int(0), Value::Bit(true)]);
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
                                    _ => 2,
                                };
                                let uq_constraint = uniq && !pk && !f.is_some() && matches!(&name, Value::Str(n) if t.uniques.iter().any(|u| !u.is_index && u.name == *n));
                                vec![Value::Int(t.object_id), name, Value::Int(id), sv(ty), Value::Bit(uniq), Value::Bit(pk), Value::Bit(f.is_some()), f.as_ref().map(|f| sv(&f.text)).unwrap_or(Value::Null), Value::Int(code), Value::Bit(uq_constraint), Value::Bit(false), Value::Int(0)]
                            };
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
                            // XML indexes are numbered from 256000
                            for (k, x) in t.xml_indexes.iter().enumerate() {
                                let mut r = row(sv(&x.name), 256_000 + k as i32, "XML", false, false, &None);
                                r[8] = Value::Int(3);
                                rows.push(r);
                            }
                        }
                    }
                    Some(rs(&[("object_id", DataType::Int), ("name", nv()), ("index_id", DataType::Int), ("type_desc", nv()), ("is_unique", DataType::Bit), ("is_primary_key", DataType::Bit), ("has_filter", DataType::Bit), ("filter_definition", DataType::NVarChar(MAX)), ("type", DataType::TinyInt), ("is_unique_constraint", DataType::Bit), ("is_disabled", DataType::Bit), ("fill_factor", DataType::TinyInt)], rows))
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
                "partitions" | "dm_db_partition_stats" => {
                    // one partition per table (heap or clustered index); the sizes are estimates from the row count
                    let stats = lname == "dm_db_partition_stats";
                    let mut rows = vec![];
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            let n = t.data.slot_count() as i64;
                            let pages = (n / 40 + 1).max(1);
                            let idx = if t.pk.is_some() { 1 } else { 0 };
                            if stats {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(idx), Value::Int(1), Value::BigInt(n), Value::BigInt(pages), Value::BigInt(pages), Value::BigInt(pages)]);
                            } else {
                                rows.push(vec![Value::Int(t.object_id), Value::Int(idx), Value::Int(1), Value::BigInt(n), Value::Int(0), Value::BigInt(t.object_id as i64 * 10 + idx as i64)]);
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
