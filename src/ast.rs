//! Abstract syntax tree for T-SQL.
use serde::{Deserialize, Serialize};
use crate::types::Value;
use std::sync::Arc as Rc;

pub type Name = Vec<String>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeRef {
    pub parts: Vec<String>,
    pub args: Vec<i64>, // -1 == MAX
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderItem {
    pub e: Expr,
    pub desc: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Bound {
    UnboundedPreceding,
    UnboundedFollowing,
    CurrentRow,
    Preceding(Box<Expr>),
    Following(Box<Expr>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    pub rows: bool,
    pub start: Bound,
    pub end: Bound,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Over {
    pub partition: Vec<Expr>,
    pub order: Vec<OrderItem>,
    pub frame: Option<Frame>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FuncCall {
    pub name: String, // upper-case
    pub schema: Option<String>,
    pub parts: Vec<String>,
    pub args: Vec<Expr>,
    pub distinct: bool,
    pub star: bool,
    pub over: Option<Over>,
    pub within: Option<Vec<OrderItem>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Expr {
    Lit(Value),
    Var(String),
    SysVar(String),
    Col(Vec<String>),
    Star(Option<Vec<String>>),
    Neg(Box<Expr>),
    BitNot(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>, bool),
    InList(Box<Expr>, Vec<Expr>, bool),
    InQuery(Box<Expr>, Box<Query>, bool),
    Like(Box<Expr>, Box<Expr>, Option<Box<Expr>>, bool),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    CmpQuant(BinOp, bool, Box<Expr>, Box<Query>),
    Exists(Box<Query>),
    Subquery(Box<Query>),
    Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, els: Option<Box<Expr>> },
    Cast { e: Box<Expr>, ty: TypeRef, try_: bool, style: Option<Box<Expr>> },
    Func(Box<FuncCall>),
    Collate(Box<Expr>, String),
    NextValue(Name),
    Default,
    IsDistinct(Box<Expr>, Box<Expr>, bool),
    AtTimeZone(Box<Expr>, Box<Expr>),
    IdentityFn { ty: TypeRef, seed: Box<Expr>, inc: Box<Expr> },
    Paren(Box<Expr>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cte {
    pub name: String,
    pub cols: Option<Vec<String>>,
    pub query: Rc<Query>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetOp {
    Union,
    Except,
    Intersect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum QueryBody {
    Select(Box<Select>),
    SetOp { op: SetOp, all: bool, left: Box<QueryBody>, right: Box<QueryBody> },
    Paren(Box<Query>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForClause {
    pub json: bool,
    pub mode: String, // RAW/AUTO/PATH
    pub name: Option<String>,
    pub root: Option<String>,
    pub without_array_wrapper: bool,
    pub include_null: bool,
    pub xml_type: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Query {
    pub ctes: Vec<Cte>,
    pub body: QueryBody,
    pub order: Vec<OrderItem>,
    pub offset: Option<Expr>,
    pub fetch: Option<Expr>,
    pub for_clause: Option<ForClause>,
    pub maxrecursion: Option<i32>,
    /// `OPTION (...)` query hints other than MAXRECURSION, upper-case (`HASH JOIN`, `FORCE ORDER`, `MAXDOP 4`, ...).
    pub options: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Top {
    pub e: Expr,
    pub percent: bool,
    pub ties: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SelectItem {
    Expr { e: Expr, alias: Option<String> },
    Star(Option<Vec<String>>),
    Assign { var: String, op: Option<BinOp>, e: Expr },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum GroupItem {
    Expr(Expr),
    Rollup(Vec<Expr>),
    Cube(Vec<Expr>),
    Sets(Vec<Vec<Expr>>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Select {
    pub distinct: bool,
    pub top: Option<Top>,
    pub items: Vec<SelectItem>,
    pub into: Option<Name>,
    pub from: Vec<TableRef>,
    pub where_: Option<Expr>,
    pub group: Option<Vec<GroupItem>>,
    pub having: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
    CrossApply,
    OuterApply,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TableRef {
    Table { name: Name, alias: Option<String>, hints: Vec<String> },
    /// A table read with `FOR SYSTEM_TIME`.
    Temporal { name: Name, alias: Option<String>, clause: Box<TemporalClause> },
    TableVar { name: String, alias: Option<String> },
    Derived { query: Box<Query>, alias: Option<String>, cols: Option<Vec<String>> },
    Values { rows: Vec<Vec<Expr>>, alias: Option<String>, cols: Option<Vec<String>> },
    Func { name: Name, args: Vec<Expr>, alias: Option<String>, cols: Option<Vec<String>>, with: Option<Vec<(String, TypeRef, Option<String>)>> },
    Join { kind: JoinKind, left: Box<TableRef>, right: Box<TableRef>, on: Option<Expr> },
    Pivot { src: Box<TableRef>, agg: String, arg: Expr, col: Vec<String>, vals: Vec<String>, alias: Option<String> },
    Unpivot { src: Box<TableRef>, vcol: String, ncol: String, cols: Vec<String>, alias: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum TemporalKind {
    AsOf,
    FromTo,
    Between,
    ContainedIn,
    All,
}

/// `FOR SYSTEM_TIME AS OF a | FROM a TO b | BETWEEN a AND b | CONTAINED IN (a, b) | ALL`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemporalClause {
    pub kind: TemporalKind,
    pub a: Option<Expr>,
    pub b: Option<Expr>,
}

/// `SYSTEM_VERSIONING` options of CREATE TABLE / ALTER TABLE ... SET.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Versioning {
    pub on: bool,
    pub history: Option<Name>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Target {
    Table(Name),
    Var(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Output {
    pub items: Vec<SelectItem>,
    pub into: Option<Target>,
    pub cols: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum InsertSrc {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
    Exec(Box<Stmt>),
    DefaultValues,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Insert {
    pub ctes: Vec<Cte>,
    pub target: Target,
    pub cols: Option<Vec<String>>,
    pub output: Option<Output>,
    pub src: InsertSrc,
    pub top: Option<(Expr, bool)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SetItem {
    Col { parts: Vec<String>, op: Option<BinOp>, e: Expr },
    Var { var: String, op: Option<BinOp>, e: Expr },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    pub ctes: Vec<Cte>,
    pub target: Target,
    pub sets: Vec<SetItem>,
    pub output: Option<Output>,
    pub from: Vec<TableRef>,
    pub where_: Option<Expr>,
    pub current_of: Option<String>,
    pub top: Option<(Expr, bool)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delete {
    pub ctes: Vec<Cte>,
    pub target: Target,
    pub output: Option<Output>,
    pub from: Vec<TableRef>,
    pub where_: Option<Expr>,
    pub current_of: Option<String>,
    pub top: Option<(Expr, bool)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MergeAction {
    Delete,
    Update(Vec<SetItem>),
    Insert { cols: Option<Vec<String>>, values: Option<Vec<Expr>> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeKind {
    Matched,
    NotMatched,
    NotMatchedBySource,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeWhen {
    pub kind: MergeKind,
    pub cond: Option<Expr>,
    pub action: MergeAction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Merge {
    pub ctes: Vec<Cte>,
    pub target: Name,
    pub target_alias: Option<String>,
    pub source: TableRef,
    pub on: Expr,
    pub whens: Vec<MergeWhen>,
    pub output: Option<Output>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: Option<TypeRef>,
    pub nullable: Option<bool>,
    pub identity: Option<(Expr, Expr)>,
    pub default: Option<Expr>,
    pub default_text: Option<String>,
    pub default_name: Option<String>,
    pub computed: Option<Expr>,
    pub computed_text: Option<String>,
    pub pk: Option<Option<String>>,
    pub unique: Option<Option<String>>,
    pub checks: Vec<(Option<String>, Expr, String)>,
    pub fk: Option<ForeignKeyDef>,
    pub collation: Option<String>,
    pub mask: Option<String>,
    /// `GENERATED ALWAYS AS ROW START` (true) or `ROW END` (false), and whether the column is `HIDDEN`.
    pub generated: Option<(bool, bool)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForeignKeyDef {
    pub name: Option<String>,
    pub cols: Vec<String>,
    pub table: Name,
    pub rcols: Option<Vec<String>>,
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TableDef {
    Column(ColumnDef),
    PrimaryKey { name: Option<String>, cols: Vec<(String, bool)> },
    Unique { name: Option<String>, cols: Vec<(String, bool)> },
    Check { name: Option<String>, e: Expr, text: String },
    ForeignKey(ForeignKeyDef),
    DefaultFor { name: String, e: Expr, col: String, text: String },
    Index { name: String, cols: Vec<(String, bool)>, unique: bool },
    /// `PERIOD FOR SYSTEM_TIME (start, end)`
    Period { start: String, end: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
    pub default: Option<Expr>,
    pub out: bool,
    pub readonly: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Decl {
    Var { name: String, ty: TypeRef, default: Option<Expr> },
    Table { name: String, defs: Vec<TableDef> },
    CursorVar(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SetOpt {
    Flag(String, String),
    Expr(String, Expr),
    Isolation(String),
    IdentityInsert(Name, bool),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecArg {
    pub name: Option<String>,
    pub e: Expr,
    pub out: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecStmt {
    pub name: Name,
    pub name_is_var: bool,
    pub args: Vec<ExecArg>,
    pub retvar: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SequenceOpts {
    pub ty: Option<TypeRef>,
    pub start: Option<Expr>,
    pub inc: Option<Expr>,
    pub min: Option<Expr>,
    pub max: Option<Expr>,
    pub cycle: Option<bool>,
    pub restart: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AlterAction {
    AddDefs(Vec<TableDef>),
    DropConstraint(Vec<String>, bool),
    DropColumn(Vec<String>, bool),
    SetVersioning(Versioning),
    AlterColumn { col: String, ty: TypeRef, nullable: Option<bool>, collation: Option<String> },
    /// `ALTER COLUMN c ADD MASKED WITH (FUNCTION = '...')` / `DROP MASKED`
    AlterMask { col: String, mask: Option<String> },
    CheckConstraint { name: Option<String>, enable: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Stmt {
    Query(Box<Query>),
    Insert(Box<Insert>),
    Update(Box<Update>),
    Delete(Box<Delete>),
    Merge(Box<Merge>),
    Truncate(Target),
    Declare(Vec<Decl>),
    Set { var: String, op: Option<BinOp>, e: Expr },
    SetCursorVar { var: String, query: Box<Query> },
    SetOpts(Vec<SetOpt>),
    If { cond: Expr, then: Box<Stmt>, els: Option<Box<Stmt>> },
    While { cond: Expr, body: Box<Stmt> },
    Block(Vec<Stmt>),
    Break,
    Continue,
    Return(Option<Expr>),
    Goto(String),
    Label(String),
    Print(Expr),
    Throw(Option<(Expr, Expr, Expr)>),
    RaiseError { args: Vec<Expr>, opts: Vec<String> },
    Try { body: Vec<Stmt>, catch: Vec<Stmt> },
    BeginTran(Option<String>),
    Commit(Option<String>),
    Rollback(Option<String>),
    SaveTran(String),
    Use(String),
    Exec(Box<ExecStmt>),
    ExecStr(Vec<Expr>),
    /// `WAITFOR DELAY e` (time = false) or `WAITFOR TIME e` (time = true)
    WaitFor { time: bool, e: Expr },
    Kill(Expr),
    Reconfigure,
    CreateTable { name: Name, defs: Vec<TableDef>, versioning: Option<Versioning> },
    CreateIndex { name: String, table: Name, cols: Vec<(String, bool)>, unique: bool, include: Vec<String>, filter: Option<(Expr, String)> },
    CreateView { name: Name, cols: Option<Vec<String>>, query: Rc<Query>, text: String, or_alter: bool, alter: bool },
    CreateProc { name: Name, params: Vec<Param>, body: Rc<Vec<Stmt>>, text: String, or_alter: bool, alter: bool },
    CreateFunction(Box<CreateFunction>),
    CreateTrigger { name: Name, table: Name, events: Vec<String>, instead: bool, body: Rc<Vec<Stmt>>, or_alter: bool, text: String },
    CreateSchema(String, Option<String>),
    CreateDatabase(String, Option<String>),
    CreateType { name: Name, base: Option<TypeRef>, nullable: Option<bool>, table: Option<Vec<TableDef>> },
    CreateSequence { name: Name, opts: SequenceOpts },
    AlterSequence { name: Name, opts: SequenceOpts },
    AlterDatabase { db: String, options: Vec<(String, String)> },
    Security(Box<SecStmt>),
    Checkpoint,
    Backup(Box<BackupStmt>),
    BulkInsert(Box<BulkStmt>),
    TriggerAdmin(Box<TriggerAdmin>),
    Audit(Box<AuditStmt>),
    CreateSecurityPolicy(Box<PolicyStmt>),
    AlterSecurityPolicy(Box<PolicyStmt>),
    DropSecurityPolicy { name: Name, if_exists: bool },
    Restore(Box<RestoreStmt>),
    AlterTable { name: Name, action: AlterAction },
    Drop { what: String, names: Vec<Name>, if_exists: bool },
    DropIndex { names: Vec<(Name, Option<Name>)>, if_exists: bool },
    DeclareCursor { name: String, query: Rc<Query> },
    CursorOp { op: String, name: String },
    Fetch { orient: String, arg: Option<Expr>, name: String, into: Option<Vec<String>> },
    Dbcc { name: String, args: Vec<Expr> },
    Noop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermKind {
    Grant,
    Deny,
    Revoke,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PermTargetAst {
    Database,
    Schema(String),
    Object(Name),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SecStmt {
    CreateLogin { name: String, password: Option<String>, default_db: Option<String>, opts: LoginOpts },
    AlterLogin { name: String, password: Option<String>, disabled: Option<bool>, default_db: Option<String>, opts: LoginOpts },
    CreateUser { name: String, login: Option<String>, default_schema: Option<String> },
    AlterUser { name: String, default_schema: Option<String>, new_name: Option<String> },
    CreateRole { name: String },
    AlterRole { role: String, add: bool, member: String },
    AlterServerRole { role: String, add: bool, member: String },
    Perm { kind: PermKind, perms: Vec<String>, target: PermTargetAst, principals: Vec<String>, with_grant: bool, cols: Vec<Vec<String>> },
    ExecuteAs { login: bool, name: String },
    Revert,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateFunction {
    pub name: Name,
    pub params: Vec<Param>,
    pub ret_type: Option<TypeRef>,
    pub ret_table: Option<Vec<TableDef>>,
    pub ret_var: Option<String>,
    pub inline: Option<Rc<Query>>,
    pub body: Rc<Vec<Stmt>>,
    pub or_alter: bool,
    pub text: String,
}

/// `BACKUP DATABASE db TO DISK = 'file' [WITH ...]` or `BACKUP LOG db TO DISK = 'file' [WITH ...]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkStmt {
    pub table: Vec<String>,
    pub path: Expr,
    pub opts: Vec<(String, Option<Expr>)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupStmt {
    pub log: bool,
    pub db: String,
    pub path: Expr,
    /// `WITH` options: upper-case name and optional value.
    pub opts: Vec<(String, Option<Expr>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreKind {
    Database,
    Log,
    VerifyOnly,
    HeaderOnly,
}

/// `RESTORE DATABASE|LOG db FROM DISK = 'file' [WITH ...]`, `RESTORE VERIFYONLY|HEADERONLY FROM DISK = 'file'`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoreStmt {
    pub kind: RestoreKind,
    pub db: Option<String>,
    pub path: Expr,
    pub opts: Vec<(String, Option<Expr>)>,
}

/// `CREATE | ALTER SECURITY POLICY name [ADD ...] [DROP ...] [WITH (STATE = ON|OFF)]`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyStmt {
    pub name: Name,
    pub add: Vec<crate::catalog::Predicate>,
    pub drop: Vec<crate::catalog::Predicate>,
    pub state: Option<bool>,
}

/// Password-policy options of CREATE / ALTER LOGIN.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoginOpts {
    pub check_policy: Option<bool>,
    pub check_expiration: Option<bool>,
    pub must_change: bool,
    pub unlock: bool,
    pub old_password: Option<String>,
}

/// Where a trigger administration statement looks for its triggers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TriggerScope {
    Database,
    Server,
    Table(Name),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TriggerAct {
    /// `CREATE [OR ALTER] TRIGGER name ON DATABASE | ALL SERVER FOR events AS body`
    Create { events: Vec<String>, body: Rc<Vec<Stmt>>, text: String, or_alter: bool },
    /// `DROP TRIGGER [IF EXISTS] name ON DATABASE | ALL SERVER`
    Drop { if_exists: bool },
    /// `ENABLE TRIGGER name | ALL ON scope` (true) or `DISABLE TRIGGER ...` (false)
    Enable(bool),
}

/// DDL / logon trigger administration, and `ENABLE | DISABLE TRIGGER` for every kind of trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TriggerAdmin {
    pub names: Vec<Name>,
    pub all: bool,
    pub scope: TriggerScope,
    pub act: TriggerAct,
}

/// SQL Server Audit statements.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuditStmt {
    CreateAudit { name: String, target: crate::audit::AuditTarget, opts: Vec<(String, String)>, filter: Option<(String, Expr)>, state_on: bool },
    AlterAudit { name: String, target: Option<crate::audit::AuditTarget>, opts: Vec<(String, String)>, filter: Option<Option<(String, Expr)>>, state: Option<bool> },
    DropAudit { name: String },
    CreateSpec { server: bool, name: String, audit: String, items: Vec<crate::audit::AuditItem>, state_on: bool },
    AlterSpec { server: bool, name: String, add: Vec<crate::audit::AuditItem>, drop: Vec<crate::audit::AuditItem>, state: Option<bool> },
    DropSpec { server: bool, name: String },
}
