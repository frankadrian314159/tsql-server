//! Database catalog: tables, views, procedures, functions, triggers, types, sequences.
use serde::{Deserialize, Serialize};
use crate::ast::*;
use crate::types::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub use crate::storage::Row;
use crate::storage::{lock::LockManager, IndexSpec, PageStore, ReadView, RowId, TableData, TxId};
pub type Rows = Arc<Vec<Row>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
    pub nullable: bool,
    pub identity: Option<(i64, i64)>,
    pub default: Option<Arc<Expr>>,
    pub default_name: Option<String>,
    pub computed: Option<Arc<Expr>>,
    pub user_type: Option<String>,
}

impl Column {
    pub fn simple(name: &str, ty: DataType) -> Column {
        Column { name: name.to_string(), ty, nullable: true, identity: None, default: None, default_name: None, computed: None, user_type: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyDef {
    pub name: String,
    pub cols: Vec<usize>,
    pub is_index: bool,
    pub clustered: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckDef {
    pub name: String,
    pub expr: Arc<Expr>,
    pub enabled: bool,
    pub col: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FkDef {
    pub name: String,
    pub cols: Vec<usize>,
    pub ref_db: String,
    pub ref_key: String, // "schema.name" lowercase
    pub ref_cols: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub cols: Vec<(usize, bool)>,
    pub unique: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trigger {
    pub name: String,
    pub schema: String,
    pub events: Vec<String>,
    pub instead: bool,
    pub body: Arc<Vec<Stmt>>,
    pub text: String,
    pub object_id: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub db: String,
    pub schema: String,
    pub name: String,
    pub object_id: i32,
    pub columns: Vec<Column>,
    pub data: Arc<TableData>,
    pub pk: Option<KeyDef>,
    pub uniques: Vec<KeyDef>,
    pub checks: Vec<CheckDef>,
    pub fks: Vec<FkDef>,
    pub indexes: Vec<IndexDef>,
    pub identity_next: Option<i64>,
    pub triggers: Vec<Arc<Trigger>>,
    pub is_temp: bool,
}

impl Table {
    pub fn new(db: &str, schema: &str, name: &str, columns: Vec<Column>, store: &Arc<PageStore>) -> Table {
        let identity_next = columns.iter().find_map(|c| c.identity.map(|(s, _)| s));
        Table {
            db: db.to_string(),
            schema: schema.to_string(),
            name: name.to_string(),
            object_id: 0,
            columns,
            data: Arc::new(TableData::new(store)),
            pk: None,
            uniques: vec![],
            checks: vec![],
            fks: vec![],
            indexes: vec![],
            identity_next,
            triggers: vec![],
            is_temp: false,
        }
    }
    pub fn col_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }
    pub fn identity_col(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.identity.is_some())
    }
    pub fn display_name(&self) -> String {
        if self.is_temp {
            self.name.clone()
        } else {
            format!("{}.{}.{}", self.db, self.schema, self.name)
        }
    }
    pub fn key(&self) -> String {
        format!("{}.{}", self.schema, self.name).to_lowercase()
    }
    /// Physical index layout: primary key, every unique key / unique index, then plain indexes.
    pub fn index_specs(&self) -> Vec<IndexSpec> {
        let mut v = vec![];
        if let Some(pk) = &self.pk {
            v.push(IndexSpec { name: pk.name.clone(), cols: pk.cols.clone(), unique: true, pk: true });
        }
        for u in &self.uniques {
            v.push(IndexSpec { name: u.name.clone(), cols: u.cols.clone(), unique: true, pk: false });
        }
        for i in &self.indexes {
            if !i.unique {
                v.push(IndexSpec { name: i.name.clone(), cols: i.cols.iter().map(|c| c.0).collect(), unique: false, pk: false });
            }
        }
        v
    }

    /// Re-derive the physical indexes after a metadata change.
    pub fn sync_indexes(&mut self) {
        let specs = self.index_specs();
        Arc::make_mut(&mut self.data).set_specs(specs);
    }

    pub fn data_mut(&mut self) -> &mut TableData {
        Arc::make_mut(&mut self.data)
    }

    /// Rows visible to `view`, with their row ids.
    pub fn rows_in(&self, view: ReadView) -> Vec<(RowId, Row)> {
        self.data.iter(view)
    }

    /// Row count as seen by the table's own writer (used for DDL checks and metadata).
    /// Append rows to a table nobody else can see yet (new table, table variable).
    pub fn load_rows(&mut self, rows: Vec<Row>) {
        let d = self.data_mut();
        for r in rows {
            let rid = d.alloc();
            d.push_version(rid, Some(r), crate::storage::Stamp::Ts(0));
        }
    }

    /// Every row the owner sees, in row-id order (DDL runs under an exclusive table lock).
    pub fn rows_vec(&self) -> Vec<Row> {
        self.data.iter(ReadView::dirty()).into_iter().map(|(_, r)| r).collect()
    }

    /// Replace all data (ALTER TABLE rewrites).
    pub fn replace_rows(&mut self, rows: Vec<Row>) {
        self.data = Arc::new(self.data.fresh());
        self.load_rows(rows);
    }

    pub fn live_rows(&self) -> usize {
        self.data.count(ReadView::dirty())
    }

    pub fn all_keys(&self) -> Vec<&KeyDef> {
        self.pk.iter().chain(self.uniques.iter()).collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewDef {
    pub db: String,
    pub schema: String,
    pub name: String,
    pub query: Arc<Query>,
    pub cols: Option<Vec<String>>,
    pub text: String,
    pub object_id: i32,
    pub triggers: Vec<Arc<Trigger>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcDef {
    pub schema: String,
    pub name: String,
    pub params: Vec<Param>,
    pub body: Arc<Vec<Stmt>>,
    pub text: String,
    pub object_id: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FuncDef {
    pub schema: String,
    pub name: String,
    pub def: Arc<CreateFunction>,
    pub object_id: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sequence {
    pub schema: String,
    pub name: String,
    pub ty: DataType,
    pub current: Option<i64>,
    pub start: i64,
    pub inc: i64,
    pub min: i64,
    pub max: i64,
    pub cycle: bool,
    pub object_id: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Object {
    Table(Table),
    View(ViewDef),
    Proc(ProcDef),
    Func(FuncDef),
    Seq(Sequence),
}

impl Object {
    pub fn type_code(&self) -> &'static str {
        match self {
            Object::Table(_) => "U",
            Object::View(_) => "V",
            Object::Proc(_) => "P",
            Object::Func(f) => {
                if f.def.inline.is_some() {
                    "IF"
                } else if f.def.ret_table.is_some() {
                    "TF"
                } else {
                    "FN"
                }
            }
            Object::Seq(_) => "SO",
        }
    }
    pub fn type_desc(&self) -> &'static str {
        match self.type_code() {
            "U" => "USER_TABLE",
            "V" => "VIEW",
            "P" => "SQL_STORED_PROCEDURE",
            "IF" => "SQL_INLINE_TABLE_VALUED_FUNCTION",
            "TF" => "SQL_TABLE_VALUED_FUNCTION",
            "FN" => "SQL_SCALAR_FUNCTION",
            _ => "SEQUENCE_OBJECT",
        }
    }
    pub fn object_id(&self) -> i32 {
        match self {
            Object::Table(t) => t.object_id,
            Object::View(v) => v.object_id,
            Object::Proc(p) => p.object_id,
            Object::Func(f) => f.object_id,
            Object::Seq(s) => s.object_id,
        }
    }
    pub fn schema(&self) -> &str {
        match self {
            Object::Table(t) => &t.schema,
            Object::View(v) => &v.schema,
            Object::Proc(p) => &p.schema,
            Object::Func(f) => &f.schema,
            Object::Seq(s) => &s.schema,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Object::Table(t) => &t.name,
            Object::View(v) => &v.name,
            Object::Proc(p) => &p.name,
            Object::Func(f) => &f.name,
            Object::Seq(s) => &s.name,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UserTypeKind {
    Alias(DataType, bool),
    Table(Vec<TableDef>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserType {
    pub schema: String,
    pub name: String,
    pub kind: UserTypeKind,
    pub type_id: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Database {
    pub name: String,
    pub id: i32,
    pub objects: BTreeMap<String, Object>,
    pub types: HashMap<String, UserType>,
    pub schemas: BTreeMap<String, String>, // lower -> display
    pub triggers: HashMap<String, (String, Arc<Trigger>)>, // trigger key -> (table key, trigger)
    pub created: chrono::NaiveDateTime,
    pub allow_snapshot: bool,
    pub rcsi: bool,
    pub security: crate::security::DbSecurity,
    pub schema_owner: BTreeMap<String, String>, // lower-case schema -> lower-case owner
}

impl Database {
    /// Re-derive the trigger lookup map from the tables that own the triggers.
    pub fn rebuild_triggers(&mut self) {
        let mut m = HashMap::new();
        for o in self.objects.values() {
            if let Object::Table(t) = o {
                for trg in &t.triggers {
                    m.insert(format!("{}.{}", trg.schema, trg.name).to_lowercase(), (t.key(), trg.clone()));
                }
            }
        }
        self.triggers = m;
    }

    pub fn new(name: &str, id: i32) -> Database {
        let mut schemas = BTreeMap::new();
        for s in ["dbo", "guest", "INFORMATION_SCHEMA", "sys"] {
            schemas.insert(s.to_lowercase(), s.to_string());
        }
        Database {
            name: name.to_string(),
            id,
            objects: BTreeMap::new(),
            types: HashMap::new(),
            schemas,
            triggers: HashMap::new(),
            created: chrono::Local::now().naive_local(),
            allow_snapshot: false,
            rcsi: false,
            security: crate::security::DbSecurity::default(),
            schema_owner: ["dbo", "guest", "INFORMATION_SCHEMA", "sys"].iter().map(|s| (s.to_lowercase(), "dbo".to_string())).collect(),
        }
    }
}

pub struct TxInfo {
    pub snapshot_ts: Option<u64>,
}

pub struct ServerState {
    pub clock: u64,
    pub next_tx: TxId,
    pub active: BTreeMap<TxId, TxInfo>,
    pub locks: LockManager,
    pub databases: BTreeMap<String, Database>,
    pub security: crate::security::Security,
    pub persist: Option<crate::persist::Persist>,
    pub global_temp: HashMap<String, Table>,
    pub next_object_id: i32,
    pub start_time: chrono::NaiveDateTime,
    pub next_spid: i32,
    pub server_name: String,
    pub next_rowversion: u64,
    /// Pages of persistent tables.
    pub store: Arc<PageStore>,
    /// Pages of temporary tables and table variables (never recovered).
    pub temp_store: Arc<PageStore>,
}

impl ServerState {
    pub fn new() -> ServerState {
        let mut s = ServerState {
            clock: 1,
            next_tx: 1,
            active: BTreeMap::new(),
            locks: LockManager::default(),
            databases: BTreeMap::new(),
            security: crate::security::Security::default(),
            persist: None,
            global_temp: HashMap::new(),
            next_object_id: 100_000,
            start_time: chrono::Local::now().naive_local(),
            next_spid: 51,
            server_name: "TSQL-SERVER".to_string(),
            next_rowversion: 1,
            store: PageStore::memory(),
            temp_store: PageStore::memory(),
        };
        for (i, n) in ["master", "tempdb", "model", "msdb"].iter().enumerate() {
            s.databases.insert(n.to_string(), Database::new(n, i as i32 + 1));
        }
        s
    }
    /// Oldest timestamp any running snapshot still needs.
    pub fn horizon(&self) -> u64 {
        self.active.values().filter_map(|t| t.snapshot_ts).min().unwrap_or(self.clock)
    }

    pub fn new_object_id(&mut self) -> i32 {
        self.next_object_id += 1;
        self.next_object_id
    }
}

impl Default for ServerState {
    fn default() -> Self {
        ServerState::new()
    }
}
