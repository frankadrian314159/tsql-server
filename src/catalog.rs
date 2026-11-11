//! Database catalog: tables, views, procedures, functions, triggers, types, sequences.
use serde::{Deserialize, Serialize};
use crate::ast::*;
use crate::types::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub use crate::storage::Row;
use crate::storage::{IndexSpec, PageStore, ReadView, RowId, TableData};
pub type Rows = Arc<Vec<Row>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
    pub nullable: bool,
    pub identity: Option<(i64, i64)>,
    pub default: Option<Arc<Expr>>,
    /// Source text of the default and computed expressions (for scripting).
    pub default_text: Option<String>,
    pub default_name: Option<String>,
    pub computed: Option<Arc<Expr>>,
    pub computed_text: Option<String>,
    pub user_type: Option<String>,
    /// A typed xml column: (database, key of the schema collection, DOCUMENT rather than CONTENT).
    #[serde(default)]
    pub xml_schema: Option<(String, String, bool)>,
    /// Collation of a string column when it is not the default one (`None`: `SQL_Latin1_General_CP1_CI_AS`).
    pub collation: Option<String>,
    /// Dynamic data masking function (`default()`, `email()`, ...), shown to users without UNMASK.
    pub mask: Option<String>,
    /// A `GENERATED ALWAYS AS ROW START/END` column declared `HIDDEN`.
    #[serde(default)]
    pub hidden: bool,
}

/// System-versioning metadata of a table: the period columns, and the history link on either side.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Temporal {
    pub start: String,
    pub end: String,
    /// For a system-versioned table: its history table (schema, name).
    pub history: Option<(String, String)>,
    /// For a history table: the key of the table it records.
    pub history_for: Option<String>,
}

impl Column {
    pub fn simple(name: &str, ty: DataType) -> Column {
        Column { name: name.to_string(), ty, nullable: true, identity: None, default: None, default_text: None, default_name: None, computed: None, computed_text: None, user_type: None, xml_schema: None, collation: None, mask: None, hidden: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyDef {
    pub name: String,
    pub cols: Vec<usize>,
    pub is_index: bool,
    pub clustered: bool,
    /// Columns carried in the index leaf but not part of its key (`INCLUDE (...)`).
    #[serde(default)]
    pub include: Vec<usize>,
    /// `WHERE` clause of a filtered unique index.
    #[serde(default)]
    pub filter: Option<crate::storage::FilterPred>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckDef {
    pub name: String,
    pub expr: Arc<Expr>,
    pub text: String,
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
    #[serde(default)]
    pub include: Vec<usize>,
    #[serde(default)]
    pub filter: Option<crate::storage::FilterPred>,
}

/// An XML index. It has no structure of its own: this server evaluates XPath over the stored text, so such an index changes no
/// result and no plan; it is kept so that scripts and backups that create one still work, and shows in the catalog views.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XmlIndexDef {
    pub name: String,
    /// the xml column
    pub col: usize,
    pub primary: bool,
    /// a secondary index: the primary XML index it is built on
    pub using: Option<String>,
    /// a secondary index: `V` value, `P` path or `R` property
    pub secondary: Option<char>,
}

/// A spatial index. Like an XML index it has no structure of its own: spatial methods are evaluated on the values, so the index
/// changes no result and no plan; it is kept so that scripts and backups that create one work, and shows in the catalog views.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpatialIndexDef {
    pub name: String,
    /// the geometry or geography column
    pub col: usize,
    pub geography: bool,
    /// `AUTO_GRID` tessellation
    pub auto: bool,
    /// geometry only: xmin, ymin, xmax, ymax
    pub bbox: Option<[f64; 4]>,
    /// the density of the four grid levels: 1 LOW, 2 MEDIUM, 3 HIGH
    pub grids: [u8; 4],
    pub cells: i32,
}

/// An XML schema collection: the text of its schema documents (see xsd.rs).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XmlSchemaCollection {
    pub id: i32,
    pub schema: String,
    pub name: String,
    pub texts: Vec<String>,
    pub created: chrono::NaiveDateTime,
}

/// A full-text catalog of a database. Like the full-text index (`FullTextDef`) it is a declaration: this server searches the text
/// with a scan (see fulltext.rs), so neither changes a result; they are kept so that scripts and backups that create them work.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FtCatalog {
    pub name: String,
    pub is_default: bool,
    pub accent_sensitive: bool,
}

/// The full-text index of a table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FullTextDef {
    pub catalog: Option<String>,
    /// the unique single-column index that identifies the rows
    pub key_index: String,
    /// the indexed columns and the language (LCID) of each; 0 is the neutral language
    pub cols: Vec<(usize, i32)>,
    /// `AUTO`, `MANUAL` or `OFF`
    pub tracking: String,
    pub enabled: bool,
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
    /// `DISABLE TRIGGER`: the trigger exists but does not fire.
    pub disabled: bool,
}

/// A DDL trigger (`CREATE TRIGGER ... ON DATABASE | ALL SERVER FOR CREATE_TABLE, ...`) or a `LOGON` trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DdlTrigger {
    pub name: String,
    /// Event names or event groups, upper case.
    pub events: Vec<String>,
    pub body: Arc<Vec<Stmt>>,
    pub text: String,
    pub disabled: bool,
    pub object_id: i32,
}

/// A value shared between clones until one of them changes it (copy on write). Cloning a table for a statement
/// copies a few reference counts instead of every column, key and constraint definition.
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Shared<T>(Arc<T>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(self.0.clone())
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Shared(Arc::new(T::default()))
    }
}

impl<T> std::ops::Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> std::ops::DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        Arc::make_mut(&mut self.0)
    }
}

impl<T> From<T> for Shared<T> {
    fn from(v: T) -> Self {
        Shared(Arc::new(v))
    }
}

impl<'a, X> IntoIterator for &'a Shared<Vec<X>> {
    type Item = &'a X;
    type IntoIter = std::slice::Iter<'a, X>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a, X: Clone> IntoIterator for &'a mut Shared<Vec<X>> {
    type Item = &'a mut X;
    type IntoIter = std::slice::IterMut<'a, X>;
    fn into_iter(self) -> Self::IntoIter {
        Arc::make_mut(&mut self.0).iter_mut()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub db: String,
    pub schema: String,
    pub name: String,
    pub object_id: i32,
    pub columns: Shared<Vec<Column>>,
    pub data: Arc<TableData>,
    pub pk: Option<KeyDef>,
    pub uniques: Shared<Vec<KeyDef>>,
    pub checks: Shared<Vec<CheckDef>>,
    pub fks: Shared<Vec<FkDef>>,
    pub indexes: Shared<Vec<IndexDef>>,
    pub identity_next: SharedCounter,
    pub triggers: Vec<Arc<Trigger>>,
    pub is_temp: bool,
    #[serde(default)]
    pub temporal: Option<Temporal>,
    #[serde(default)]
    pub xml_indexes: Vec<XmlIndexDef>,
    #[serde(default)]
    pub fulltext: Option<FullTextDef>,
    #[serde(default)]
    pub spatial_indexes: Vec<SpatialIndexDef>,
}

/// A counter that clones share: the next identity value of a table, the current value of a sequence. A statement
/// working on a copy of the table advances the real counter without writing anything back to the catalog, so
/// sessions can use identity columns and sequences while holding the catalog only for reading.
#[derive(Clone, Debug, Default)]
pub struct SharedCounter(Arc<std::sync::Mutex<Option<i64>>>);

impl Serialize for SharedCounter {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.get().serialize(s)
    }
}
impl<'de> Deserialize<'de> for SharedCounter {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<SharedCounter, D::Error> {
        Option::<i64>::deserialize(d).map(SharedCounter::new)
    }
}

impl SharedCounter {
    pub fn new(v: Option<i64>) -> SharedCounter {
        SharedCounter(Arc::new(std::sync::Mutex::new(v)))
    }
    pub fn get(&self) -> Option<i64> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Read, compute and store in one step; `f` returns the new value and a result, or an error to leave it as is.
    pub fn update<R, E>(&self, f: impl FnOnce(Option<i64>) -> Result<(Option<i64>, R), E>) -> Result<R, E> {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (v, r) = f(*g)?;
        *g = v;
        Ok(r)
    }
    pub fn set(&self, v: Option<i64>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }
}

impl Table {
    pub fn new(db: &str, schema: &str, name: &str, columns: Vec<Column>, store: &Arc<PageStore>) -> Table {
        let identity_next = columns.iter().find_map(|c| c.identity.map(|(s, _)| s));
        Table {
            db: db.to_string(),
            schema: schema.to_string(),
            name: name.to_string(),
            object_id: 0,
            columns: columns.into(),
            data: Arc::new(TableData::new(store)),
            pk: None,
            uniques: Shared::default(),
            checks: Shared::default(),
            fks: Shared::default(),
            indexes: Shared::default(),
            identity_next: SharedCounter::new(identity_next),
            triggers: vec![],
            is_temp: false,
            temporal: None,
            xml_indexes: vec![],
            fulltext: None,
            spatial_indexes: vec![],
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
        // the collation of each key column, when it is not the built-in one
        let colls = |cols: &[usize]| -> Vec<Option<String>> {
            let c: Vec<Option<String>> = cols.iter().map(|&i| self.columns.get(i).and_then(|c| c.collation.clone())).collect();
            if c.iter().any(|x| x.is_some()) {
                c
            } else {
                vec![]
            }
        };
        if let Some(pk) = &self.pk {
            v.push(IndexSpec { name: pk.name.clone(), cols: pk.cols.clone(), unique: true, pk: true, colls: colls(&pk.cols), filter: None });
        }
        for u in &self.uniques {
            v.push(IndexSpec { name: u.name.clone(), cols: u.cols.clone(), unique: true, pk: false, colls: colls(&u.cols), filter: u.filter.clone() });
        }
        for i in &self.indexes {
            if !i.unique {
                let cols: Vec<usize> = i.cols.iter().map(|c| c.0).collect();
                v.push(IndexSpec { name: i.name.clone(), colls: colls(&cols), cols, unique: false, pk: false, filter: i.filter.clone() });
            }
        }
        v
    }

    /// A hashing key for the values of columns `cols`, in the columns' collations (so `'a'` and `'A'` are the same key in
    /// a case-insensitive column and different keys in a case-sensitive one).
    pub fn key_string(&self, cols: &[usize], vals: &[Value]) -> String {
        let spec = IndexSpec { name: String::new(), cols: cols.to_vec(), unique: false, pk: false, colls: cols.iter().map(|&i| self.columns.get(i).and_then(|c| c.collation.clone())).collect(), filter: None };
        if spec.colls.iter().any(|c| c.is_some()) {
            crate::convert::key_of(&spec.xform(vals))
        } else {
            crate::convert::key_of(vals)
        }
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
    /// WITH CHECK OPTION: rows written through the view must satisfy its WHERE clause.
    #[serde(default)]
    pub check_option: bool,
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
    pub current: SharedCounter,
    pub start: i64,
    pub inc: i64,
    pub min: i64,
    pub max: i64,
    pub cycle: bool,
    pub object_id: i32,
}

/// An alternative name for another object (`CREATE SYNONYM`); names are resolved through it to the target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SynonymDef {
    pub schema: String,
    pub name: String,
    /// The name it stands for, as written (one to four parts).
    pub target: Vec<String>,
    pub object_id: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Object {
    Table(Table),
    View(ViewDef),
    Proc(ProcDef),
    Func(FuncDef),
    Seq(Sequence),
    Synonym(SynonymDef),
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
            Object::Synonym(_) => "SN",
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
            "SN" => "SYNONYM",
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
            Object::Synonym(s) => s.object_id,
        }
    }
    pub fn schema(&self) -> &str {
        match self {
            Object::Table(t) => &t.schema,
            Object::View(v) => &v.schema,
            Object::Proc(p) => &p.schema,
            Object::Func(f) => &f.schema,
            Object::Seq(s) => &s.schema,
            Object::Synonym(s) => &s.schema,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Object::Table(t) => &t.name,
            Object::View(v) => &v.name,
            Object::Proc(p) => &p.name,
            Object::Func(f) => &f.name,
            Object::Seq(s) => &s.name,
            Object::Synonym(s) => &s.name,
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
    /// The id each user schema was given when it was created (`sys.schemas.schema_id`); never reused or renumbered.
    #[serde(default)]
    pub schema_ids: BTreeMap<String, i32>,
    pub triggers: HashMap<String, (String, Arc<Trigger>)>, // trigger key -> (table key, trigger)
    pub created: chrono::NaiveDateTime,
    pub allow_snapshot: bool,
    pub rcsi: bool,
    pub security: crate::security::DbSecurity,
    pub schema_owner: BTreeMap<String, String>, // lower-case schema -> lower-case owner
    /// Recovery model: `true` = FULL (log backups possible once a full backup has been taken), `false` = SIMPLE.
    pub recovery_full: bool,
    /// The database is being restored (`RESTORE ... WITH NORECOVERY`) and cannot be used yet.
    pub restoring: bool,
    /// Commit timestamp up to which the log has been backed up (or the full backup was taken): the log chain.
    /// `None` until a full backup starts the chain.
    pub log_chain_ts: Option<u64>,
    /// While restoring: commit timestamp up to which backups have been applied.
    pub applied_ts: u64,
    /// Default collation of the database: what new string columns and literals compared in it use.
    pub collation: String,
    /// Row-level security policies.
    pub policies: Vec<SecurityPolicy>,
    /// Database-scoped DDL triggers.
    pub ddl_triggers: Vec<DdlTrigger>,
    /// Database audit specifications.
    pub audit_specs: Vec<crate::audit::AuditSpec>,
    /// Commit timestamp of the last full (non-COPY_ONLY) backup: the base of differential backups.
    pub last_full_ts: Option<u64>,
    /// Signature of each table's column definition at that backup (a differential includes every row of a table whose
    /// definition has changed since).
    pub backup_sigs: BTreeMap<String, u64>,
    /// Full-text catalogs.
    #[serde(default)]
    pub fulltext_catalogs: Vec<FtCatalog>,
    /// XML schema collections.
    #[serde(default)]
    pub xml_schemas: Vec<XmlSchemaCollection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredKind {
    /// Rows for which the function returns no row are invisible (and untouchable) to the session.
    Filter,
    BlockAfterInsert,
    BlockAfterUpdate,
    BlockBeforeUpdate,
    BlockBeforeDelete,
}

impl PredKind {
    pub fn describe(&self) -> (&'static str, &'static str) {
        match self {
            PredKind::Filter => ("FILTER", ""),
            PredKind::BlockAfterInsert => ("BLOCK", "AFTER INSERT"),
            PredKind::BlockAfterUpdate => ("BLOCK", "AFTER UPDATE"),
            PredKind::BlockBeforeUpdate => ("BLOCK", "BEFORE UPDATE"),
            PredKind::BlockBeforeDelete => ("BLOCK", "BEFORE DELETE"),
        }
    }
}

/// `ADD FILTER|BLOCK PREDICATE schema.fn(col, ...) ON schema.table`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Predicate {
    pub kind: PredKind,
    pub func: (String, String),
    /// Columns of the target table passed to the function.
    pub args: Vec<String>,
    /// The table key (`schema.name`, lower case).
    pub table: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecurityPolicy {
    pub schema: String,
    pub name: String,
    pub object_id: i32,
    pub enabled: bool,
    pub preds: Vec<Predicate>,
}

impl Database {
    /// The id of a schema: fixed for the built-in ones, the assigned one for the others (a database from before ids were
    /// kept numbers its schemas by position, as it always did).
    pub fn schema_id(&self, lname: &str) -> i32 {
        match lname {
            "dbo" => 1,
            "guest" => 2,
            "information_schema" => 3,
            "sys" => 4,
            _ => self.schema_ids.get(lname).copied().unwrap_or_else(|| 5 + self.schemas.keys().position(|k| k == lname).unwrap_or(0) as i32),
        }
    }

    /// Give a newly created schema its id (call after adding it to `schemas`).
    pub fn assign_schema_id(&mut self, lname: &str) {
        if matches!(lname, "dbo" | "guest" | "information_schema" | "sys") || self.schema_ids.contains_key(lname) {
            return;
        }
        let legacy_max = 4 + self.schemas.len() as i32;
        let next = self.schema_ids.values().copied().max().unwrap_or(4).max(legacy_max) + 1;
        self.schema_ids.insert(lname.to_string(), next);
    }

    /// The default collation's name for columns that store none (the built-in one: the database default may have changed since).
    pub fn collation_or_default(&self) -> String {
        crate::collation::DEFAULT_NAME.to_string()
    }

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
            schema_ids: BTreeMap::new(),
            triggers: HashMap::new(),
            created: chrono::Local::now().naive_local(),
            allow_snapshot: false,
            rcsi: false,
            security: crate::security::DbSecurity::default(),
            schema_owner: ["dbo", "guest", "INFORMATION_SCHEMA", "sys"].iter().map(|s| (s.to_lowercase(), "dbo".to_string())).collect(),
            recovery_full: true,
            restoring: false,
            log_chain_ts: None,
            applied_ts: 0,
            collation: crate::collation::DEFAULT_NAME.to_string(),
            policies: vec![],
            ddl_triggers: vec![],
            audit_specs: vec![],
            last_full_ts: None,
            backup_sigs: BTreeMap::new(),
            fulltext_catalogs: vec![],
            xml_schemas: vec![],
        }
    }
}

pub struct TxInfo {
    pub snapshot_ts: Option<u64>,
    /// The transaction has changed the catalog (it is not yet committed, so a backup must wait for it).
    pub ddl: bool,
}

pub struct ServerState {
    /// Clock, counters, running transactions and locks; independent of this struct's lock.
    pub core: Arc<crate::coord::Core>,
    pub databases: BTreeMap<String, Database>,
    pub security: crate::security::Security,
    /// The redo log and checkpoint writer (None for a server that lives in memory), behind its own latch.
    pub persist: std::sync::Mutex<Option<crate::persist::Persist>>,
    pub global_temp: HashMap<String, Table>,
    pub start_time: chrono::NaiveDateTime,
    pub server_name: String,
    /// Pages of persistent tables.
    pub store: Arc<PageStore>,
    /// Pages of temporary tables and table variables (never recovered).
    pub temp_store: Arc<PageStore>,
    /// Memory budget and location for operators that spill to disk.
    pub work: crate::engine::spill::WorkMem,
}

impl ServerState {
    pub fn new() -> ServerState {
        let mut s = ServerState {
            core: Arc::new(crate::coord::Core::new()),
            databases: BTreeMap::new(),
            security: crate::security::Security::default(),
            persist: std::sync::Mutex::new(None),
            global_temp: HashMap::new(),
            start_time: chrono::Local::now().naive_local(),
            server_name: "TSQL-SERVER".to_string(),
            store: PageStore::memory(),
            temp_store: PageStore::memory(),
            work: crate::engine::spill::WorkMem::default(),
        };
        for (i, n) in ["master", "tempdb", "model", "msdb"].iter().enumerate() {
            s.databases.insert(n.to_string(), Database::new(n, i as i32 + 1));
        }
        s
    }
    pub fn persist(&self) -> std::sync::MutexGuard<'_, Option<crate::persist::Persist>> {
        self.persist.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn new_object_id(&self) -> i32 {
        self.core.new_object_id()
    }
}

impl Default for ServerState {
    fn default() -> Self {
        ServerState::new()
    }
}
