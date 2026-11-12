//! The T-SQL execution engine: sessions, statements, procedures, transactions.
/// The most lines of plan text one statement keeps (see `Session::plan`): a correlated subquery runs its scan once
/// per outer row, and a line for each would cost more than the scan and grow without bound.
pub const PLAN_LINES: usize = 256;

/// Add a line to the plan text, unless the statement already has `PLAN_LINES` of them (the line is then not even built).
macro_rules! plan_note {
    ($s:expr, $($arg:tt)*) => {
        if $s.ses.plan.len() < $crate::engine::PLAN_LINES {
            $s.ses.plan.push(format!($($arg)*));
        }
    };
}

pub mod ddl;
pub mod dml;
pub mod expr;
pub mod funcs;
pub mod hooks;
pub mod joins;
pub mod optimizer;
pub mod query;
pub mod secexec;
pub mod setops;
pub mod spill;
pub mod admin;
pub mod backup;
pub mod bakimport;
pub mod auditing;
pub mod bulk;
pub mod ddltrig;
pub mod aggacc;
pub mod ftddl;
pub mod xmlschema;
pub mod spatialfn;
pub mod hashjoin;
pub mod rls;
pub mod stream;
pub mod sysprocs;
pub mod sysviews;
pub mod temporal;
pub mod txn;

use crate::ast::*;
use crate::catalog::*;
use crate::convert;
use crate::error::{Flow, Interrupt, Result, SqlError};
use crate::parser;
use crate::types::*;
use std::collections::{HashMap, HashSet};
use crate::storage::tx::{Isolation, Tx};
use crate::storage::TxId;
use std::ops::Deref;
use std::cell::OnceCell;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

// ------------------------------------------------------------------ results
#[derive(Clone, Debug)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub types: Vec<DataType>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Rows(ResultSet),
    Message(String),
    RowCount(i64),
    Error(SqlError),
}

/// What a streaming run (`Connection::execute_streaming`) reports, in order. Everything the engine reports as an
/// [`Event`] arrives as `Event`; the rows of a big `SELECT` arrive one at a time between `Header` and `End`, as
/// the final sort or merge produces them, so the result set is never held in memory.
#[derive(Clone, Debug)]
pub enum Streamed {
    Event(Event),
    /// A result set starts. `inferred[i]` says the type of column `i` was guessed from the first rows (an expression
    /// has no declared type); a string in such a column may be wider than the guess.
    Header { columns: Vec<String>, types: Vec<DataType>, inferred: Vec<bool> },
    Row(Vec<Value>),
    /// The result set ends; it had this many rows.
    End(u64),
}

/// Receives the output of a streaming run; returns false when the consumer is gone (the statement is abandoned).
pub type Sink<'a> = &'a mut dyn FnMut(Streamed) -> bool;

#[derive(Clone, Debug, Default)]
pub struct BatchResult {
    pub events: Vec<Event>,
}

impl BatchResult {
    pub fn resultsets(&self) -> Vec<&ResultSet> {
        self.events.iter().filter_map(|e| if let Event::Rows(r) = e { Some(r) } else { None }).collect()
    }
    pub fn messages(&self) -> Vec<&str> {
        self.events.iter().filter_map(|e| if let Event::Message(m) = e { Some(m.as_str()) } else { None }).collect()
    }
    pub fn errors(&self) -> Vec<&SqlError> {
        self.events.iter().filter_map(|e| if let Event::Error(m) = e { Some(m) } else { None }).collect()
    }
    pub fn first_error(&self) -> Option<&SqlError> {
        self.errors().into_iter().next()
    }
}

// ------------------------------------------------------------ session state
#[derive(Clone, Debug)]
pub struct Var {
    pub ty: DataType,
    pub val: Value,
}

#[derive(Default)]
pub struct Frame {
    pub vars: HashMap<String, Var>,
    pub tables: HashMap<String, Table>,
    pub cursor_vars: HashMap<String, String>,
    /// The DECLARE statement that declared each variable: running the same one again (in a loop) redeclares it, while a
    /// different one for the same name is error 134.
    pub decl_sites: HashMap<String, usize>,
}

/// See [`Engine::enter_module`].
pub struct ModuleScope {
    temps: std::collections::HashSet<String>,
    nocount: bool,
    xact_abort: bool,
    rowcount_limit: i64,
    datefirst: i32,
    language: String,
    lock_timeout: i64,
    isolation: Isolation,
    fmtonly: bool,
    options: i32,
}

pub struct Cursor {
    pub query: Arc<Query>,
    pub rows: Vec<Row>,
    pub columns: Vec<String>,
    pub pos: i64,
    pub open: bool,
}

pub struct CteEntry {
    pub query: Arc<Query>,
    pub cols: Option<Vec<String>>,
    pub working: Option<query::Rowset>,
}

pub struct TriggerCtx {
    pub table: Table,
    pub inserted: Vec<Row>,
    pub deleted: Vec<Row>,
    pub event: String,
    pub updated_cols: Vec<bool>,
    pub instead_of: bool,
}

pub struct SessionState {
    pub spid: i32,
    /// This session's entry in the session table (none for a session that is not registered).
    pub info: Option<crate::coord::SessionHandle>,
    pub stat_buf: std::sync::Arc<crate::coord::StatBuf>,
    pub login: String,
    pub db: String,
    pub temp: HashMap<String, Table>,
    pub frames: Vec<Frame>,
    pub trancount: i32,
    pub tx: Option<Tx>,
    pub isolation: Isolation,
    pub stmt_ts: u64,
    pub blocked_on: Vec<TxId>,
    /// Where the last request for a wait came from (for the statistics of lock waits).
    pub blocked_at: Option<&'static std::panic::Location<'static>>,
    /// The table whose lock the session last had to wait for.
    pub blocked_oid: i32,
    /// Was NOWAIT in force for the statement that was blocked? (the statement's own flag is put back when it ends,
    /// before the wait is decided)
    pub blocked_nowait: bool,
    /// The kind of statement being run (for the statistics of lock waits).
    pub stmt_kind: &'static str,
    pub lock_timeout: i64,
    pub login_id: i32,
    pub pushdown: Vec<Vec<(String, Vec<Expr>)>>,
    pub imp_stack: Vec<(String, Option<String>)>,
    pub plan: Vec<String>,
    pub order_hint: Option<optimizer::OrderHint>,
    pub ordered_used: bool,
    pub dry_run: bool,
    pub showplan: bool,
    pub optimizer: bool,
    pub module_owner: Vec<String>,
    /// The SET options as `@@OPTIONS` reports them (a bit mask).
    pub options: i32,
    pub nocount: bool,
    pub xact_abort: bool,
    pub rowcount_limit: i64,
    /// The UTC time stamped on the rows a statement writes to system-versioned tables.
    pub period_now: Option<chrono::NaiveDateTime>,
    pub datefirst: i32,
    pub identity_insert: HashSet<String>,
    /// Tables a DML statement is currently working on (see `Engine::checkout_table`).
    pub checked_out: Vec<String>,
    /// The next `SELECT` to start may stream its rows (see `Engine::try_stream_select`).
    pub stream_ok: bool,
    /// The next `SELECT` feeds another operator (set operation, derived table, CTE): it may collect its rows in a
    /// buffer that spills to a file.
    pub stream_to_buf: bool,
    /// A FROM source the streaming executor evaluated and then declined: (address of its TableRef, rows).
    pub preeval: Option<(usize, query::Rowset)>,
    /// Results of subqueries that do not depend on the enclosing row, for the statement being run (see `Engine::sub_rows`).
    /// `None` where caching is not safe: outside a statement, and while an IF or WHILE condition is evaluated.
    pub sub_cache: Option<HashMap<usize, Option<query::Rowset>>>,
    /// Per statement: what planning a correlated subquery finds out that does not depend on the outer row.
    pub push_plans: HashMap<(usize, usize), std::sync::Arc<optimizer::PushPlan>>,
    /// Set while a grouped query runs again without folding its aggregates as the rows arrive (see `aggacc`).
    pub no_fold_agg: bool,
    /// Per statement: for a correlated subquery, the names it mentions (`None`: its result must not be reused), and the
    /// rows it gave for each combination of the values of the outer columns with those names (see `Engine::sub_rows`).
    pub sub_names: HashMap<usize, Option<std::sync::Arc<Vec<(String, Option<String>)>>>>,
    pub sub_memo: HashMap<usize, HashMap<String, query::Rowset>>,
    /// Per statement, for a statement that only reads (a SELECT): the rows a table scan with some conditions gave, kept the
    /// second time the same scan is asked for (a correlated subquery reads the tables of its FROM again for every outer row).
    pub leaf_seen: std::collections::HashSet<String>,
    pub leaf_cache: HashMap<String, spill::RowsRef>,
    pub in_query: bool,
    /// `SET FMTONLY ON`: queries describe their result and return no rows, other statements are not run.
    pub fmtonly: bool,
    /// While a statement writes through a view WITH CHECK OPTION: the base table, the filter of the view and the nesting level.
    pub view_check: Option<(String, Expr, i32)>,
    /// Table hints of the statement being run, by table object id (see `Engine::collect_hints`).
    pub hints: HashMap<i32, Vec<String>>,
    /// A NOWAIT table hint is in effect: a lock that cannot be granted at once is error 1222.
    pub nowait: bool,
    /// A row-level security predicate function is being evaluated (policies do not apply inside it).
    pub in_rls: bool,
    /// `OPTION (...)` hints of the query being run.
    pub query_opts: Vec<String>,
    /// Table latches this session holds (see `Engine::apply_changes`).
    pub latched: Vec<i32>,
    pub last_identity: Option<i64>,
    pub rowcount: i64,
    pub last_error: i32,
    pub cursors: HashMap<String, Cursor>,
    pub fetch_status: i32,
    pub try_depth: i32,
    pub strict: i32,
    pub nest_level: i32,
    pub catch_stack: Vec<SqlError>,
    pub cte_stack: Vec<HashMap<String, CteEntry>>,
    pub trigger_stack: Vec<TriggerCtx>,
    /// `EVENTDATA()` of the DDL or logon trigger being run (innermost last).
    pub eventdata: Vec<String>,
    /// Documents prepared with `sp_xml_preparedocument` for `OPENXML`: handle -> xml text.
    pub xml_docs: HashMap<i32, String>,
    pub xml_next: i32,
    /// Text of the batch being run (for `EVENTDATA()`'s command text).
    pub batch_text: String,
    /// Audit records already written for the current statement (one per action and object).
    pub audit_seen: std::cell::RefCell<HashSet<String>>,
    pub capture: Vec<Vec<ResultSet>>,
    pub ret: Value,
    pub doomed: bool,
    pub language: String,
    pub app_name: String,
    pub host_name: String,
    pub context_info: Vec<u8>,
    pub maxrec: Option<i32>,
}

impl SessionState {
    pub fn new(spid: i32) -> SessionState {
        SessionState {
            spid,
            info: None,
            stat_buf: Default::default(),
            login: "sa".into(),
            db: "master".into(),
            temp: HashMap::new(),
            frames: vec![],
            trancount: 0,
            tx: None,
            isolation: Isolation::ReadCommitted,
            stmt_ts: 0,
            blocked_on: vec![],
            blocked_at: None,
            blocked_oid: 0,
            blocked_nowait: false,
            stmt_kind: "",
            lock_timeout: -1,
            login_id: 0,
            pushdown: vec![],
            imp_stack: vec![],
            plan: vec![],
            order_hint: None,
            ordered_used: false,
            dry_run: false,
            showplan: false,
            optimizer: true,
            module_owner: vec![],
            options: 5496,
            nocount: false,
            xact_abort: false,
            rowcount_limit: 0,
            period_now: None,
            datefirst: 7,
            identity_insert: HashSet::new(),
            checked_out: vec![],
            stream_ok: false,
            stream_to_buf: false,
            preeval: None,
            sub_cache: None,
            push_plans: HashMap::new(),
            no_fold_agg: false,
            sub_names: HashMap::new(),
            sub_memo: HashMap::new(),
            leaf_seen: std::collections::HashSet::new(),
            leaf_cache: HashMap::new(),
            in_query: false,
            fmtonly: false,
            view_check: None,
            hints: HashMap::new(),
            nowait: false,
            in_rls: false,
            query_opts: vec![],
            latched: vec![],
            last_identity: None,
            rowcount: 0,
            last_error: 0,
            cursors: HashMap::new(),
            fetch_status: -1,
            try_depth: 0,
            strict: 0,
            nest_level: 0,
            catch_stack: vec![],
            cte_stack: vec![],
            trigger_stack: vec![],
            eventdata: vec![],
            xml_docs: HashMap::new(),
            xml_next: 1,
            batch_text: String::new(),
            audit_seen: Default::default(),
            capture: vec![],
            ret: Value::Null,
            doomed: false,
            language: "us_english".into(),
            app_name: "tsql-server".into(),
            host_name: "localhost".into(),
            context_info: vec![0; 128],
            maxrec: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Loc {
    Temp(String),
    GTemp(String),
    Var(String),
    Db(String, String),
}

/// Statements that only read the catalog (queries, DML, session `SET`s) run under a shared catalog guard, so sessions
/// execute them in parallel; everything else (DDL, security, database options, ...) takes it exclusively.
pub const SHARED_READS: bool = true;

/// A session's hold on the catalog ([`ServerState`]).
///
/// The guard is taken lazily, on the first access after [`SrvGuard::release`] (which the engine calls between
/// top-level statements and while a statement waits for a lock), in the mode set by [`SrvGuard::set_intent`].
/// Reading goes through `Deref`. Code that changes the catalog must call [`SrvGuard::cat_mut`], which switches
/// to exclusive mode (releasing the shared guard first, so the catalog can change at that point; nothing borrowed
/// from it can be alive, because `cat_mut` needs `&mut self`).
pub struct SrvGuard<'a> {
    lock: &'a RwLock<ServerState>,
    write: bool,
    r: OnceCell<RwLockReadGuard<'a, ServerState>>,
    w: OnceCell<RwLockWriteGuard<'a, ServerState>>,
}

impl<'a> SrvGuard<'a> {
    /// A guard that takes the catalog shared when first used. Each statement then chooses its mode with
    /// [`SrvGuard::set_intent`]; code that changes the catalog calls [`SrvGuard::cat_mut`].
    pub fn new(lock: &'a RwLock<ServerState>) -> SrvGuard<'a> {
        SrvGuard { lock, write: false, r: OnceCell::new(), w: OnceCell::new() }
    }
    /// Choose the mode for the next acquisition; a guard held in another mode is released.
    pub fn set_intent(&mut self, write: bool) {
        if write != self.write {
            self.release();
            self.write = write;
        }
    }
    /// Give the catalog up (it is taken again on the next access).
    pub fn release(&mut self) {
        self.r.take();
        self.w.take();
    }
    /// Run `f` without holding the catalog (to wait for something that other sessions have to do).
    pub fn unlocked<T>(&mut self, f: impl FnOnce() -> T) -> T {
        self.release();
        f()
    }
    /// Let other sessions run between statements.
    pub fn yield_lock(&mut self) {
        // only a session that held the catalog exclusively needs to step aside for the others to get in
        let was_exclusive = self.w.get().is_some();
        self.release();
        if was_exclusive {
            std::thread::yield_now();
        }
    }
    /// Exclusive access to the catalog.
    pub fn cat_mut(&mut self) -> &mut ServerState {
        if self.w.get().is_none() {
            if !self.write && std::env::var_os("TSQL_TRACE_UPGRADES").is_some() {
                eprintln!("catalog upgrade:
{}", std::backtrace::Backtrace::force_capture());
            }
            self.r.take();
            self.write = true;
            let g = self.lock.write().unwrap_or_else(|e| e.into_inner());
            let _ = self.w.set(g);
        }
        let s = &mut **self.w.get_mut().unwrap();
        s.epoch += 1;
        s
    }
}

impl<'a> Deref for SrvGuard<'a> {
    type Target = ServerState;
    fn deref(&self) -> &ServerState {
        if let Some(w) = self.w.get() {
            return w;
        }
        if self.write {
            self.w.get_or_init(|| self.lock.write().unwrap_or_else(|e| e.into_inner()))
        } else {
            self.r.get_or_init(|| self.lock.read().unwrap_or_else(|e| e.into_inner()))
        }
    }
}

/// Does this statement change the catalog (so that it must hold it exclusively from the start)?
/// Queries, DML, variables, cursors and transaction control only read it; anything that does change it
/// (an undo of DDL, `SELECT ... INTO`) switches to exclusive mode where it happens ([`SrvGuard::cat_mut`]).
pub fn stmt_needs_write(st: &Stmt) -> bool {
    !matches!(
        st,
        Stmt::Query(_)
            | Stmt::Insert(_)
            | Stmt::BulkInsert(_)
            | Stmt::Update(_)
            | Stmt::Delete(_)
            | Stmt::Merge(_)
            | Stmt::Declare(_)
            | Stmt::Set { .. }
            | Stmt::SetCursorVar { .. }
            | Stmt::SetOpts(_)
            | Stmt::Print(_)
            | Stmt::BeginTran(_)
            | Stmt::Commit(_)
            | Stmt::Rollback(_)
            | Stmt::SaveTran(_)
            | Stmt::DeclareCursor { .. }
            | Stmt::CursorOp { .. }
            | Stmt::Fetch { .. }
            | Stmt::Backup(_)
            | Stmt::WaitFor { .. }
            | Stmt::Kill(_)
            | Stmt::Reconfigure
            | Stmt::Noop
    )
}

pub struct Engine<'a> {
    pub srv: SrvGuard<'a>,
    pub ses: &'a mut SessionState,
    pub out: Vec<Event>,
    /// Where output goes instead of `out` when running in streaming mode.
    pub sink: Option<Sink<'a>>,
}

// ------------------------------------------------------------------------------ plan cache
//
// The parsed form of a batch is kept per thread (a connection's thread, for the listener) so a statement a client sends
// again, as most applications do, is not parsed again. The cache is keyed by the exact batch text; entries are dropped
// at random-ish once there are too many. `DBCC FREEPROCCACHE` empties it. (Execution plans themselves are not cached:
// the optimizer's decisions depend on the data and on the parameter values.)
thread_local! {
    static PARSE_CACHE: std::cell::RefCell<HashMap<String, (std::rc::Rc<Vec<Stmt>>, u64)>> = std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    static PARAM_CACHE: std::cell::RefCell<HashMap<String, std::rc::Rc<Vec<Param>>>> = std::cell::RefCell::new(HashMap::new());
}

/// The parsed `@params` declaration of an `sp_executesql` call (`N'@id INT, @name VARCHAR(20) OUTPUT'`); clients send
/// the same text with every call.
pub fn cached_param_decls(decl: &str) -> std::result::Result<std::rc::Rc<Vec<Param>>, SqlError> {
    if let Some(h) = PARAM_CACHE.with(|c| c.borrow().get(decl).cloned()) {
        return Ok(h);
    }
    let parsed = std::rc::Rc::new(parser::parse_param_decls(decl)?);
    if decl.len() <= 4096 {
        PARAM_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= 256 {
                c.clear();
            }
            c.insert(decl.to_string(), parsed.clone());
        });
    }
    Ok(parsed)
}

const PARSE_CACHE_ENTRIES: usize = 512;
const PARSE_CACHE_MAX_TEXT: usize = 64 << 10;

pub fn cached_parse(sql: &str) -> std::result::Result<std::rc::Rc<Vec<Stmt>>, SqlError> {
    if sql.len() <= PARSE_CACHE_MAX_TEXT {
        let hit = PARSE_CACHE.with(|c| {
            c.borrow_mut().get_mut(sql).map(|e| {
                e.1 += 1;
                e.0.clone()
            })
        });
        if let Some(h) = hit {
            return Ok(h);
        }
    }
    let stmts = {
        crate::span!("parse");
        std::rc::Rc::new(parser::parse(sql)?)
    };
    if sql.len() <= PARSE_CACHE_MAX_TEXT {
        PARSE_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= PARSE_CACHE_ENTRIES {
                // make room: drop the least used half
                let mut uses: Vec<u64> = c.values().map(|e| e.1).collect();
                uses.sort_unstable();
                let cut = uses[uses.len() / 2];
                c.retain(|_, e| e.1 > cut);
            }
            c.insert(sql.to_string(), (stmts.clone(), 1));
        });
    }
    Ok(stmts)
}

pub fn clear_parse_cache() {
    PARSE_CACHE.with(|c| c.borrow_mut().clear());
    PARAM_CACHE.with(|c| c.borrow_mut().clear());
}

/// (batch text, times used) of this thread's cache.
/// The name of a statement's kind (`Query`, `Insert`, ...): the head of its `Debug` text. The text is not built in
/// full (for a big INSERT that would visit every value): the writer refuses everything after the first word.
pub fn stmt_head(st: &Stmt) -> String {
    let mut s = String::new();
    stmt_head_into(st, &mut s);
    s
}

/// Append the leading word of `st`'s debug form (its kind) to `out`.
pub fn stmt_head_into(st: &Stmt, out: &mut String) {
    use std::fmt::Write;
    struct Head<'a>(&'a mut String);
    impl Write for Head<'_> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            for ch in s.chars() {
                if !ch.is_alphanumeric() {
                    return Err(std::fmt::Error);
                }
                self.0.push(ch);
            }
            Ok(())
        }
    }
    let mut h = Head(out);
    let _ = write!(h, "{:?}", st);
}

pub fn parse_cache_entries() -> Vec<(String, u64)> {
    PARSE_CACHE.with(|c| c.borrow().iter().map(|(k, v)| (k.clone(), v.1)).collect())
}

pub fn obj_key(schema: &str, name: &str) -> String {
    format!("{}.{}", schema, name).to_lowercase()
}

impl<'a> Engine<'a> {
    pub fn new(srv: SrvGuard<'a>, ses: &'a mut SessionState) -> Engine<'a> {
        Engine { srv, ses, out: vec![], sink: None }
    }

    /// An engine that reports its output to `sink` as it is produced instead of collecting it.
    pub fn with_sink(srv: SrvGuard<'a>, ses: &'a mut SessionState, sink: Sink<'a>) -> Engine<'a> {
        Engine { srv, ses, out: vec![], sink: Some(sink) }
    }

    /// Report one event (collected, or passed to the sink).
    pub fn emit(&mut self, ev: Event) {
        match self.sink.as_mut() {
            Some(s) => {
                let _ = s(Streamed::Event(ev)); // a vanished consumer is noticed when rows are streamed
            }
            None => self.out.push(ev),
        }
    }

    /// Pass a streamed row set item to the sink; an error when the consumer has gone away.
    pub fn stream_item(&mut self, item: Streamed) -> Result<()> {
        if let Some(s) = self.sink.as_mut() {
            if !s(item) {
                return Err(SqlError::new(64, "A transport-level error has occurred when sending the request to the client."));
            }
        }
        Ok(())
    }

    // --------------------------------------------------------- batch driver
    pub fn run_batch(&mut self, sql: &str) {
        crate::span!("run_batch");
        let stmts = match cached_parse(sql) {
            Ok(s) => s,
            Err(e) => {
                self.emit(Event::Error(e));
                return;
            }
        };
        self.run_parsed(sql, &stmts);
    }

    /// Run statements that are already parsed as a batch (`sql` is the text they came from, for the statistics and the
    /// audit; it may be a description).
    pub fn run_parsed(&mut self, sql: &str, stmts: &[Stmt]) {
        let started = std::time::Instant::now();
        if self.srv.core.audit.is_active() || stmts.iter().any(ddltrig::is_ddl_stmt) {
            self.ses.batch_text = sql.to_string();
        }
        self.ses.frames.push(Frame::default());
        self.ses.try_depth = 0;
        self.ses.strict = 0;
        self.ses.nest_level = 0;
        let r = self.exec_stmts(stmts);
        if let Err(Interrupt::Error(e)) = r {
            self.report_error(e);
        }
        self.ses.frames.truncate(0);
        self.ses.cte_stack.clear();
        self.ses.trigger_stack.clear();
        self.ses.capture.clear();
        self.ses.catch_stack.clear();
        self.ses.checked_out.clear();
        self.srv.core.record_stat(&self.ses.stat_buf, sql, started.elapsed().as_micros() as u64, self.ses.rowcount.max(0) as u64);
        // a batch-aborting error with an open XACT_ABORT transaction rolls back
    }

    pub fn report_error(&mut self, e: SqlError) {
        self.ses.last_error = e.number;
        self.emit(Event::Error(e));
    }

    // ----------------------------------------------------------- statements
    pub fn exec_stmts(&mut self, stmts: &[Stmt]) -> Flow<()> {
        let mut i = 0;
        while i < stmts.len() {
            match self.exec_stmt(&stmts[i]) {
                Ok(()) => i += 1,
                Err(Interrupt::Goto(l)) => {
                    if let Some(idx) = stmts.iter().position(|s| matches!(s, Stmt::Label(x) if *x == l)) {
                        i = idx + 1;
                    } else {
                        return Err(Interrupt::Goto(l));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn exec_stmt(&mut self, st: &Stmt) -> Flow<()> {
        crate::span!("exec_stmt");
        self.ses.stmt_kind = match st {
            Stmt::Query(_) => "select",
            Stmt::Insert(_) => "insert",
            Stmt::Update(_) => "update",
            Stmt::Delete(_) => "delete",
            Stmt::Merge(_) => "merge",
            Stmt::Exec(_) => "exec",
            _ => "other",
        };
        let leaf = !matches!(st, Stmt::If { .. } | Stmt::While { .. } | Stmt::Block(_) | Stmt::Try { .. } | Stmt::Exec(_) | Stmt::ExecStr(_));
        if !leaf {
            if self.ses.nest_level == 0 {
                self.srv.set_intent(!SHARED_READS);
            }
            // a condition may change from one iteration to the next: no cached subquery results
            let saved = self.ses.sub_cache.take();
            let r = self.exec_inner(st);
            self.ses.sub_cache = saved;
            return match r {
                Err(Interrupt::Error(e)) => self.handle_error(e),
                other => other,
            };
        }
        if self.ses.showplan && !matches!(st, Stmt::SetOpts(_)) {
            return self.exec_showplan(st);
        }
        if self.ses.fmtonly && !matches!(st, Stmt::SetOpts(_)) {
            return self.exec_fmtonly(st);
        }
        if self.ses.nest_level == 0 {
            self.ses.plan.clear();
        }
        loop {
            let owns = self.ses.tx.is_none();
            let mark = self.ses.tx.as_ref().map(|t| t.undo.len()).unwrap_or(0);
            if self.ses.nest_level == 0 {
                let spid = self.ses.spid;
                if self.srv.core.is_killed(spid) {
                    let e = self.kill_error();
                    return self.handle_error(e);
                }
                if let Some(h) = &self.ses.info {
                    let iso = self.ses.isolation.level();
                    let mut s = h.lock().unwrap_or_else(|e| e.into_inner());
                    if s.db != self.ses.db {
                        s.db.clone_from(&self.ses.db);
                    }
                    s.status = "running";
                    s.isolation = iso;
                    s.command.clear();
                    stmt_head_into(st, &mut s.command);
                    s.last_batch_at = std::time::SystemTime::now();
                    s.batches += 1;
                }
            }
            let write = !SHARED_READS || stmt_needs_write(st);
            if self.ses.nest_level == 0 || write {
                // a statement that changes the catalog takes it exclusively from the start, wherever it is nested
                self.srv.set_intent(write);
            }
            self.ses.stmt_ts = self.srv.core.clock();
            // The table hints of a statement are those written in it: a statement inside a procedure (or a trigger) starts
            // without those of the statement that called it, and the caller's come back when it ends. (They used to be cleared
            // only at the top level, so after one procedure ran `WITH (UPDLOCK)` on a table every later statement of the
            // session that read the table kept taking exclusive locks on every row it read.)
            let saved_hints = (std::mem::take(&mut self.ses.hints), std::mem::replace(&mut self.ses.nowait, false));
            self.ses.audit_seen.borrow_mut().clear();
            let saved = self.ses.sub_cache.replace(HashMap::new());
            let saved_plans = std::mem::take(&mut self.ses.push_plans);
            let saved_memo = (std::mem::take(&mut self.ses.sub_names), std::mem::take(&mut self.ses.sub_memo));
            let saved_leaf = (std::mem::take(&mut self.ses.leaf_seen), std::mem::take(&mut self.ses.leaf_cache), std::mem::replace(&mut self.ses.in_query, false));
            let mut r = self.exec_inner(st);
            self.ses.sub_cache = saved;
            self.ses.push_plans = saved_plans;
            (self.ses.sub_names, self.ses.sub_memo) = saved_memo;
            (self.ses.leaf_seen, self.ses.leaf_cache, self.ses.in_query) = saved_leaf;
            (self.ses.hints, self.ses.nowait) = saved_hints;
            self.ses.view_check = None;
            if r.is_ok() && ddltrig::is_ddl_stmt(st) {
                r = self.fire_ddl_triggers(st);
            }
            match r {
                Ok(()) => {
                    if owns {
                        if let Err(e) = self.finish_implicit() {
                            return self.handle_error(e);
                        }
                    }
                    if self.ses.nest_level == 0 {
                        if let Some(h) = &self.ses.info {
                            h.lock().unwrap_or_else(|e| e.into_inner()).status = "sleeping";
                        }
                        self.srv.yield_lock();
                    }
                    return Ok(());
                }
                Err(Interrupt::Error(e)) if e.number == txn::BLOCKED => {
                    if owns {
                        self.rollback_tx();
                    } else {
                        self.undo_to(mark);
                    }
                    if let Err(e2) = self.wait_blocked() {
                        return self.handle_error(e2);
                    }
                    continue;
                }
                Err(Interrupt::Error(e)) => {
                    if self.ses.tx.is_some() {
                        if owns {
                            self.rollback_tx();
                        } else {
                            self.undo_to(mark);
                        }
                    }
                    return self.handle_error(e);
                }
                Err(other) => {
                    // control flow (RETURN, BREAK, ...) is a successful statement
                    if owns {
                        if let Err(e) = self.finish_implicit() {
                            return self.handle_error(e);
                        }
                    }
                    return Err(other);
                }
            }
        }
    }

    /// SET FMTONLY ON: a query gives the description of its result (its columns, no rows) and nothing else is run; this is how
    /// client libraries (bulk copy among them) learn the shape of a table.
    fn exec_fmtonly(&mut self, st: &Stmt) -> Flow<()> {
        match st {
            Stmt::Query(q) => {
                self.ses.dry_run = true;
                let r = self.exec_query(q, None);
                self.ses.dry_run = false;
                let mut rs = r?;
                rs.rows = Vec::<Row>::new().into();
                rs.suppress = false;
                self.emit_rowset(rs);
                Ok(())
            }
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) | Stmt::Exec(_) | Stmt::ExecStr(_) | Stmt::BulkInsert(_) => Ok(()),
            _ => {
                self.ses.fmtonly = false;
                let r = self.exec_stmt(st);
                self.ses.fmtonly = true;
                r
            }
        }
    }

    /// SET SHOWPLAN_TEXT ON: describe how a query would run instead of running it.
    fn exec_showplan(&mut self, st: &Stmt) -> Flow<()> {
        self.ses.plan.clear();
        let lines: Vec<String> = match st {
            Stmt::Query(q) => {
                self.ses.dry_run = true;
                let r = self.exec_query(q, None);
                self.ses.dry_run = false;
                r?;
                let mut v = vec!["SELECT".to_string()];
                v.extend(self.ses.plan.iter().map(|l| format!("  |--{}", l)));
                v
            }
            other => {
                let kind = format!("{:?}", other);
                vec![kind.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("statement").to_string() + " (not executed)"]
            }
        };
        let rows = lines.into_iter().map(|l| vec![Value::Str(l)]).collect();
        self.emit_rowset(query::Rowset::from_rows(&["StmtText".to_string()], rows));
        Ok(())
    }

    fn finish_implicit(&mut self) -> Result<()> {
        if self.ses.tx.as_ref().map(|t| t.implicit).unwrap_or(false) {
            self.commit_tx()?;
        }
        Ok(())
    }

    fn handle_error(&mut self, e: SqlError) -> Flow<()> {
        self.ses.last_error = e.number;
        if e.number == 3960 || e.number == 1205 {
            self.rollback_all();
            let mut e = e;
            e.abort_batch = true;
            return Err(Interrupt::Error(e));
        }
        if self.ses.try_depth > 0 || self.ses.strict > 0 || e.abort_batch {
            return Err(Interrupt::Error(e));
        }
        if self.ses.xact_abort {
            if self.ses.trancount > 0 {
                self.rollback_all();
            }
            return Err(Interrupt::Error(e));
        }
        self.report_error(e);
        Ok(())
    }

    fn cond(&mut self, e: &Expr) -> Result<bool> {
        let env = query::Env::empty();
        let v = self.ev(e, &env)?;
        Ok(matches!(v, Value::Bit(true)))
    }

    fn exec_inner(&mut self, st: &Stmt) -> Flow<()> {
        crate::span!("exec_inner");
        // a database that is being restored cannot be used (only a few statements work in it)
        if self.srv.databases.get(&self.ses.db.to_lowercase()).is_some_and(|d| d.restoring)
            && !matches!(st, Stmt::Use(_) | Stmt::Backup(_) | Stmt::Restore(_) | Stmt::SetOpts(_) | Stmt::Set { .. } | Stmt::Declare(_) | Stmt::Print(_) | Stmt::Block(_) | Stmt::If { .. } | Stmt::Try { .. } | Stmt::Noop | Stmt::Rollback(_) | Stmt::Commit(_))
        {
            return Err(SqlError::with(927, 14, 1, format!("Database '{}' cannot be opened. It is in the middle of a restore.", self.ses.db)).into());
        }
        match st {
            Stmt::Query(q) => {
                // the rows of a plain top-level SELECT may go straight to the client
                self.ses.stream_ok = self.sink.is_some() && self.ses.capture.is_empty() && q.ctes.is_empty() && q.for_clause.is_none() && matches!(q.body, QueryBody::Select(_));
                self.ses.in_query = true;
                let rs = self.exec_query(q, None);
                self.ses.in_query = false;
                self.ses.stream_ok = false;
                let rs = rs?;
                if !rs.suppress {
                    let n = rs.rows.len() as i64;
                    self.ses.rowcount = n;
                    self.emit_rowset(rs);
                    if !self.ses.nocount {
                        self.emit(Event::RowCount(n));
                    }
                } else if !self.ses.nocount && rs.report_count {
                    self.emit(Event::RowCount(self.ses.rowcount));
                }
                self.ses.last_error = 0;
            }
            Stmt::Insert(s) => self.exec_insert(s)?,
            Stmt::BulkInsert(b) => self.exec_bulk_insert(b)?,
            Stmt::TriggerAdmin(a) => self.exec_trigger_admin(a)?,
            Stmt::Audit(a) => self.exec_audit(a)?,
            Stmt::Update(s) => self.exec_update(s)?,
            Stmt::Delete(s) => self.exec_delete(s)?,
            Stmt::Merge(s) => self.exec_merge(s)?,
            Stmt::Truncate(t) => self.exec_truncate(t)?,
            Stmt::Declare(d) => self.exec_declare(d)?,
            Stmt::Set { var, op, e } => {
                let env = query::Env::empty();
                let mut v = self.ev(e, &env)?;
                if let Some(op) = op {
                    let cur = self.get_var(var)?;
                    v = convert::arith(*op, &cur, &v)?;
                }
                self.set_var(var, v)?;
                self.ses.last_error = 0;
            }
            Stmt::SetCursorVar { var, query } => {
                let name = format!("$cv_{}", var);
                self.ses.cursors.insert(name.clone(), Cursor { query: Arc::new((**query).clone()), rows: vec![], columns: vec![], pos: -1, open: false });
                if let Some(f) = self.ses.frames.last_mut() {
                    f.cursor_vars.insert(var.clone(), name);
                }
            }
            Stmt::SetOpts(opts) => self.exec_set_opts(opts)?,
            Stmt::If { cond, then, els } => {
                if self.cond(cond)? {
                    self.exec_stmt(then)?;
                } else if let Some(e) = els {
                    self.exec_stmt(e)?;
                }
            }
            Stmt::While { cond, body } => {
                while self.cond(cond)? {
                    match self.exec_stmt(body) {
                        Ok(()) => {}
                        Err(Interrupt::Break) => break,
                        Err(Interrupt::Continue) => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
            Stmt::Block(b) => self.exec_stmts(b)?,
            Stmt::Break => return Err(Interrupt::Break),
            Stmt::Continue => return Err(Interrupt::Continue),
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => {
                        let env = query::Env::empty();
                        self.ev(e, &env)?
                    }
                    None => Value::Null,
                };
                self.ses.ret = v;
                return Err(Interrupt::Return);
            }
            Stmt::Goto(l) => return Err(Interrupt::Goto(l.clone())),
            Stmt::Label(_) | Stmt::Noop => {}
            Stmt::Print(e) => {
                let env = query::Env::empty();
                let v = self.ev(e, &env)?;
                let s = if v.is_null() { String::new() } else { convert::to_str(&v) };
                self.emit(Event::Message(s));
            }
            Stmt::Throw(args) => self.exec_throw(args)?,
            Stmt::RaiseError { args, opts } => self.exec_raiserror(args, opts)?,
            Stmt::Try { body, catch } => self.exec_try(body, catch)?,
            Stmt::BeginTran(_) => {
                match self.ses.tx.as_mut() {
                    Some(t) => t.implicit = false,
                    None => self.begin_tx(false)?,
                }
                self.ses.trancount += 1;
            }
            Stmt::Commit(_) => {
                if self.ses.trancount == 0 {
                    return Err(SqlError::new(3902, "The COMMIT TRANSACTION request has no corresponding BEGIN TRANSACTION.").into());
                }
                if self.ses.doomed {
                    return Err(SqlError::new(3930, "The current transaction cannot be committed and cannot support operations that write to the log file. Roll back the transaction.").into());
                }
                self.ses.trancount -= 1;
                if self.ses.trancount == 0 {
                    self.commit_tx()?;
                }
            }
            Stmt::Rollback(name) => {
                if self.ses.trancount == 0 {
                    return Err(SqlError::new(3903, "The ROLLBACK TRANSACTION request has no corresponding BEGIN TRANSACTION.").into());
                }
                let sp = name.as_ref().and_then(|n| self.ses.tx.as_ref().and_then(|t| t.savepoints.iter().rposition(|(sn, _)| sn.eq_ignore_ascii_case(n))));
                match sp {
                    Some(idx) => {
                        let mark = self.ses.tx.as_ref().unwrap().savepoints[idx].1;
                        self.undo_to(mark);
                        self.ses.tx.as_mut().unwrap().savepoints.truncate(idx + 1);
                    }
                    None => self.rollback_all(),
                }
            }
            Stmt::SaveTran(n) => {
                if let Some(tx) = self.ses.tx.as_mut() {
                    let m = tx.undo.len();
                    tx.savepoints.push((n.clone(), m));
                } else {
                    return Err(SqlError::new(628, "Cannot issue SAVE TRANSACTION when there is no active transaction.").into());
                }
            }
            Stmt::Use(db) => {
                let key = db.to_lowercase();
                if !self.srv.databases.contains_key(&key) {
                    return Err(SqlError::new(911, format!("Database '{}' does not exist. Make sure that the name is entered correctly.", db)).into());
                }
                if self.db_user(&key).is_none() {
                    return Err(SqlError::with(916, 14, 1, format!("The server principal \"{}\" is not able to access the database \"{}\" under the current security context.", self.eff_login(), db)).into());
                }
                if self.srv.databases[&key].restoring {
                    return Err(SqlError::with(927, 14, 1, format!("Database '{}' cannot be opened. It is in the middle of a restore.", db)).into());
                }
                self.ses.db = self.srv.databases[&key].name.clone();
                self.emit(Event::Message(format!("Changed database context to '{}'.", self.ses.db)));
            }
            Stmt::Exec(ex) => self.exec_exec(ex)?,
            Stmt::ExecStr(parts) => {
                let env = query::Env::empty();
                let mut sql = String::new();
                for p in parts {
                    let v = self.ev(p, &env)?;
                    sql.push_str(&convert::to_str(&v));
                }
                self.exec_dynamic(&sql)?;
            }
            Stmt::WaitFor { time, e } => self.exec_waitfor(*time, e)?,
            Stmt::Kill(e) => self.exec_kill(e)?,
            Stmt::Reconfigure => {
                if !self.is_sysadmin() {
                    return Err(SqlError::with(262, 14, 1, "RECONFIGURE permission denied in database 'master'.").into());
                }
                self.srv.core.config_apply();
            }
            Stmt::DeclareCursor { name, query } => {
                self.ses.cursors.insert(name.clone(), Cursor { query: query.clone(), rows: vec![], columns: vec![], pos: -1, open: false });
            }
            Stmt::CursorOp { op, name } => self.exec_cursor_op(op, name)?,
            Stmt::Fetch { orient, arg, name, into } => self.exec_fetch(orient, arg, name, into)?,
            Stmt::Dbcc { name, args } => self.exec_dbcc(name, args)?,
            Stmt::Security(s) => self.exec_security(s)?,
            Stmt::Checkpoint => self.do_checkpoint(),
            Stmt::Backup(b) => self.exec_backup(b)?,
            Stmt::CreateSecurityPolicy(_) | Stmt::AlterSecurityPolicy(_) | Stmt::DropSecurityPolicy { .. } => self.exec_policy(st)?,
            Stmt::Restore(r) => self.exec_restore(r)?,
            Stmt::AlterDatabase { db, options } => {
                let key = if db.eq_ignore_ascii_case("current") { self.ses.db.to_lowercase() } else { db.to_lowercase() };
                if !self.srv.databases.contains_key(&key) {
                    return Err(SqlError::new(5011, format!("User does not have permission to alter database '{}', the database does not exist, or the database is not in a state that allows access checks.", db)).into());
                }
                for (name, val) in options {
                    let on = val == "ON";
                    let d = self.srv.cat_mut().databases.get_mut(&key).unwrap();
                    match name.as_str() {
                        "ALLOW_SNAPSHOT_ISOLATION" => d.allow_snapshot = on,
                        "READ_COMMITTED_SNAPSHOT" => d.rcsi = on,
                        "COLLATE" => {
                            if crate::collation::get(val).is_none() {
                                return Err(SqlError::with(448, 16, 1, format!("Invalid collation '{}'.", val)).into());
                            }
                            d.collation = val.clone(); // new objects use it; existing columns keep theirs
                        }
                        "RECOVERY" => {
                            match val.as_str() {
                                "FULL" | "BULK_LOGGED" => d.recovery_full = true,
                                "SIMPLE" => {
                                    d.recovery_full = false;
                                    d.log_chain_ts = None; // SIMPLE breaks the log chain
                                }
                                other => return Err(SqlError::new(5058, format!("Option '{}' cannot be set in database '{}'; use FULL, BULK_LOGGED or SIMPLE.", other, db)).into()),
                            }
                            let full = d.recovery_full;
                            self.log_nowait(vec![crate::persist::LogOp::DbRecovery { db: key.clone(), full }]);
                        }
                        _ => {}
                    }
                }
                self.log_database_options(&key);
            }
            // DDL
            Stmt::CreateTable { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::CreateView { .. }
            | Stmt::CreateProc { .. }
            | Stmt::CreateFunction(_)
            | Stmt::CreateTrigger { .. }
            | Stmt::CreateSchema(..)
            | Stmt::CreateDatabase(..)
            | Stmt::CreateType { .. }
            | Stmt::CreateSequence { .. }
            | Stmt::CreateSynonym { .. }
            | Stmt::AlterSequence { .. }
            | Stmt::AlterTable { .. }
            | Stmt::Drop { .. }
            | Stmt::DropIndex { .. } => {
                self.exec_ddl(st)?;
                self.ses.last_error = 0;
            }
            Stmt::FullText(f) => {
                self.exec_fulltext(f)?;
                self.ses.last_error = 0;
            }
            Stmt::XmlSchema(x) => {
                self.exec_xml_schema(x)?;
                self.ses.last_error = 0;
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------- helpers
    pub fn emit_rowset(&mut self, rs: query::Rowset) {
        crate::span!("emit_rowset");
        let columns: Vec<String> = rs.schema.cols.iter().map(|c| c.name.clone()).collect();
        let types: Vec<DataType> = rs.schema.cols.iter().enumerate().map(|(i, c)| {
            c.ty.clone().unwrap_or_else(|| rs.rows.iter().map(|r| &r[i]).find(|v| !v.is_null()).map(|v| v.data_type()).unwrap_or(DataType::Int))
        }).collect();
        let rows = rs.rows.into_vec();
        let r = ResultSet { columns, types, rows };
        if let Some(cap) = self.ses.capture.last_mut() {
            cap.push(r);
        } else {
            self.emit(Event::Rows(r));
        }
    }

    pub fn current_db(&mut self) -> &mut Database {
        let k = self.ses.db.to_lowercase();
        self.srv.cat_mut().databases.get_mut(&k).expect("current database")
    }

    pub fn db_of(&self, name: &str) -> Option<&Database> {
        self.srv.databases.get(&name.to_lowercase())
    }

    // ---- variables
    pub fn get_var(&self, name: &str) -> Result<Value> {
        self.ses
            .frames
            .last()
            .and_then(|f| f.vars.get(name))
            .map(|v| v.val.clone())
            .ok_or_else(|| SqlError::new(137, format!("Must declare the scalar variable \"{}\".", name)))
    }
    pub fn set_var(&mut self, name: &str, v: Value) -> Result<()> {
        let ty = self.ses.frames.last().and_then(|f| f.vars.get(name)).map(|v| v.ty.clone());
        match ty {
            None => Err(SqlError::new(137, format!("Must declare the scalar variable \"{}\".", name))),
            Some(t) => {
                let nv = convert::cast(&v, &t, None)?;
                self.ses.frames.last_mut().unwrap().vars.get_mut(name).unwrap().val = nv;
                Ok(())
            }
        }
    }

    pub fn resolve_type(&mut self, tr: &TypeRef) -> Result<DataType> {
        // user-defined alias types first
        let (schema, name) = match tr.parts.len() {
            1 => (None, tr.parts[0].clone()),
            _ => (Some(tr.parts[tr.parts.len() - 2].clone()), tr.parts[tr.parts.len() - 1].clone()),
        };
        {
            let db = self.srv.databases.get(&self.ses.db.to_lowercase()).unwrap();
            let key = obj_key(schema.as_deref().unwrap_or("dbo"), &name);
            if let Some(ut) = db.types.get(&key) {
                if let UserTypeKind::Alias(dt, _) = &ut.kind {
                    return Ok(dt.clone());
                }
                return Err(SqlError::new(2715, format!("Column, parameter, or variable #1: Cannot find data type {}.", name)));
            }
        }
        if tr.parts.len() == 2 && tr.parts[0].eq_ignore_ascii_case("sys") {
            return DataType::from_name(&tr.parts[1], &tr.args, false);
        }
        DataType::from_name(&name, &tr.args, false)
    }

    pub fn resolve_type_cast(&mut self, tr: &TypeRef) -> Result<DataType> {
        let has_user = tr.parts.len() == 1 && {
            let db = self.srv.databases.get(&self.ses.db.to_lowercase()).unwrap();
            db.types.contains_key(&obj_key("dbo", &tr.parts[0]))
        };
        if has_user || tr.parts.len() > 1 {
            return self.resolve_type(tr);
        }
        DataType::from_name(&tr.parts[0], &tr.args, true)
    }

    fn table_type_defs(&self, tr: &TypeRef) -> Option<Vec<TableDef>> {
        let db = self.srv.databases.get(&self.ses.db.to_lowercase())?;
        let (schema, name) = match tr.parts.len() {
            1 => ("dbo".to_string(), tr.parts[0].clone()),
            _ => (tr.parts[tr.parts.len() - 2].clone(), tr.parts[tr.parts.len() - 1].clone()),
        };
        match db.types.get(&obj_key(&schema, &name)) {
            Some(UserType { kind: UserTypeKind::Table(d), .. }) => Some(d.clone()),
            _ => None,
        }
    }

    fn exec_declare(&mut self, decls: &[Decl]) -> Flow<()> {
        for d in decls {
            match d {
                Decl::Var { name, ty, default } => {
                    if let Some(defs) = self.table_type_defs(ty) {
                        let t = self.build_table(&self.ses.db.clone(), "dbo", name, &defs, true)?;
                        self.ses.frames.last_mut().unwrap().tables.insert(name.clone(), t);
                        continue;
                    }
                    let dt = self.resolve_type(ty)?;
                    let site = d as *const Decl as usize;
                    {
                        let f = self.ses.frames.last().unwrap();
                        if (f.vars.contains_key(name) || f.tables.contains_key(name)) && f.decl_sites.get(name) != Some(&site) {
                            return Err(SqlError::new(134, format!("The variable name '{}' has already been declared. Variable names must be unique within a query batch or stored procedure.", name)).into());
                        }
                    }
                    let val = match default {
                        Some(e) => {
                            let env = query::Env::empty();
                            let v = self.ev(e, &env)?;
                            convert::cast(&v, &dt, None)?
                        }
                        None => Value::Null,
                    };
                    let f = self.ses.frames.last_mut().unwrap();
                    f.vars.insert(name.clone(), Var { ty: dt, val });
                    f.decl_sites.insert(name.clone(), site);
                }
                Decl::Table { name, defs } => {
                    let t = self.build_table(&self.ses.db.clone(), "dbo", name, defs, true)?;
                    self.ses.frames.last_mut().unwrap().tables.insert(name.clone(), t);
                }
                Decl::CursorVar(name) => {
                    self.ses.frames.last_mut().unwrap().vars.insert(name.clone(), Var { ty: DataType::Other("cursor".into()), val: Value::Null });
                }
            }
        }
        Ok(())
    }

    fn exec_set_opts(&mut self, opts: &[SetOpt]) -> Flow<()> {
        for o in opts {
            match o {
                SetOpt::Flag(name, val) => {
                    let on = val == "ON";
                    // the options `@@OPTIONS` reports (SQL Server's bit values)
                    let bit = |n: &str| -> i32 {
                        match n {
                            "DISABLE_DEF_CNST_CHK" => 1,
                            "IMPLICIT_TRANSACTIONS" => 2,
                            "CURSOR_CLOSE_ON_COMMIT" => 4,
                            "ANSI_WARNINGS" => 8,
                            "ANSI_PADDING" => 16,
                            "ANSI_NULLS" => 32,
                            "ARITHABORT" => 64,
                            "ARITHIGNORE" => 128,
                            "QUOTED_IDENTIFIER" => 256,
                            "NOCOUNT" => 512,
                            "ANSI_NULL_DFLT_ON" => 1024,
                            "ANSI_NULL_DFLT_OFF" => 2048,
                            "CONCAT_NULL_YIELDS_NULL" => 4096,
                            "NUMERIC_ROUNDABORT" => 8192,
                            "XACT_ABORT" => 16384,
                            // the options that SET ANSI_DEFAULTS turns on together
                            "ANSI_DEFAULTS" => 4 | 2 | 8 | 16 | 32 | 256 | 1024,
                            _ => 0,
                        }
                    };
                    let b = bit(name.as_str());
                    if b != 0 {
                        if on {
                            self.ses.options |= b;
                            if name == "ANSI_NULL_DFLT_ON" {
                                self.ses.options &= !2048;
                            } else if name == "ANSI_NULL_DFLT_OFF" {
                                self.ses.options &= !1024;
                            }
                        } else {
                            self.ses.options &= !b;
                        }
                    }
                    match name.as_str() {
                        "NOCOUNT" => self.ses.nocount = on,
                        "XACT_ABORT" => self.ses.xact_abort = on,
                        "SHOWPLAN_TEXT" | "SHOWPLAN_ALL" => self.ses.showplan = on,
                        "OPTIMIZER" => self.ses.optimizer = on,
                        "FMTONLY" => self.ses.fmtonly = on,
                        _ => {}
                    }
                }
                SetOpt::Expr(name, e) => {
                    let env = query::Env::empty();
                    let v = self.ev(e, &env)?;
                    match name.as_str() {
                        "ROWCOUNT" => self.ses.rowcount_limit = convert::cast(&v, &DataType::BigInt, None)?.as_i64().unwrap_or(0),
                        "DATEFIRST" => self.ses.datefirst = convert::cast(&v, &DataType::Int, None)?.as_i64().unwrap_or(7) as i32,
                        "LANGUAGE" => self.ses.language = convert::to_str(&v),
                        "LOCK_TIMEOUT" => self.ses.lock_timeout = v.as_i64().unwrap_or(-1),
                        _ => {}
                    }
                }
                SetOpt::Isolation(s) => {
                    let iso = Isolation::parse(s).ok_or_else(|| SqlError::new(102, format!("Incorrect syntax near '{}'.", s)))?;
                    if self.ses.trancount > 0 {
                        if let Some(t) = self.ses.tx.as_ref() {
                            if (t.isolation == Isolation::Snapshot) != (iso == Isolation::Snapshot) && iso == Isolation::Snapshot {
                                return Err(SqlError::new(3951, "Transaction failed in database because the statement was run under SNAPSHOT isolation but the transaction did not start in SNAPSHOT isolation. You cannot change the isolation level of the transaction to snapshot after the transaction has started unless the transaction was originally started under snapshot isolation level.").into());
                            }
                        }
                    }
                    self.ses.isolation = iso;
                    if let Some(t) = self.ses.tx.as_mut() {
                        if !t.implicit {
                            t.isolation = iso;
                        }
                    }
                }
                SetOpt::IdentityInsert(name, on) => {
                    let loc = self.locate(name);
                    if self.table_ref(&loc).is_none() {
                        return Err(SqlError::new(208, format!("Invalid object name '{}'.", name.join("."))).into());
                    }
                    let key = format!("{:?}", loc);
                    if *on {
                        self.ses.identity_insert.insert(key);
                    } else {
                        self.ses.identity_insert.remove(&key);
                    }
                }
            }
        }
        Ok(())
    }

    // ---- THROW / RAISERROR / TRY
    fn exec_throw(&mut self, args: &Option<(Expr, Expr, Expr)>) -> Flow<()> {
        match args {
            None => match self.ses.catch_stack.last() {
                Some(e) => {
                    let mut e = e.clone();
                    e.abort_batch = true;
                    Err(Interrupt::Error(e))
                }
                None => Err(SqlError::new(10704, "Incorrect syntax near 'THROW'. The statement before the THROW statement must be terminated with a semicolon (;)  or THROW must be used inside a CATCH block.").into()),
            },
            Some((n, m, s)) => {
                let env = query::Env::empty();
                let num = convert::cast(&self.ev(n, &env)?, &DataType::Int, None)?.as_i64().unwrap_or(0) as i32;
                let msg = convert::to_str(&self.ev(m, &env)?);
                let state = convert::cast(&self.ev(s, &env)?, &DataType::Int, None)?.as_i64().unwrap_or(1) as u8;
                if num < 50000 {
                    return Err(SqlError::new(35100, format!("Error number {} in the THROW statement is outside the valid range. Specify an error number in the valid range of 50000 to 2147483647.", num)).into());
                }
                let mut e = SqlError::with(num, 16, state, msg);
                e.abort_batch = true;
                Err(Interrupt::Error(e))
            }
        }
    }

    fn exec_raiserror(&mut self, args: &[Expr], opts: &[String]) -> Flow<()> {
        let env = query::Env::empty();
        let first = self.ev(&args[0], &env)?;
        let sev = if args.len() > 1 { convert::cast(&self.ev(&args[1], &env)?, &DataType::Int, None)?.as_i64().unwrap_or(16) } else { 16 };
        let state = if args.len() > 2 { convert::cast(&self.ev(&args[2], &env)?, &DataType::Int, None)?.as_i64().unwrap_or(1) } else { 1 };
        let mut fmt_args = vec![];
        for a in args.iter().skip(3) {
            fmt_args.push(self.ev(a, &env)?);
        }
        let msg = match &first {
            Value::Str(s) => funcs::format_raiserror(s, &fmt_args),
            Value::Int(n) => return Err(SqlError::new(18054, format!("Error {}, severity {}, state {} was raised, but no message with that error number was found in sys.messages.", n, sev, state)).into()),
            other => funcs::format_raiserror(&convert::to_str(other), &fmt_args),
        };
        let _ = opts;
        if sev <= 10 {
            self.emit(Event::Message(msg));
            return Ok(());
        }
        let e = SqlError::with(50000, sev as u8, state as u8, msg);
        Err(Interrupt::Error(e))
    }

    fn exec_try(&mut self, body: &[Stmt], catch: &[Stmt]) -> Flow<()> {
        let depth_before = self.ses.frames.len();
        let strict_before = self.ses.strict;
        let nest_before = self.ses.nest_level;
        let cte_before = self.ses.cte_stack.len();
        let trig_before = self.ses.trigger_stack.len();
        let cap_before = self.ses.capture.len();
        self.ses.try_depth += 1;
        let r = self.exec_stmts(body);
        self.ses.try_depth -= 1;
        match r {
            Err(Interrupt::Error(e)) => {
                // unwind any frames left by the failed call chain
                self.ses.frames.truncate(depth_before);
                self.ses.strict = strict_before;
                self.ses.nest_level = nest_before;
                self.ses.cte_stack.truncate(cte_before);
                self.ses.trigger_stack.truncate(trig_before);
                self.ses.capture.truncate(cap_before);
                if e.severity >= 20 {
                    return Err(Interrupt::Error(e));
                }
                self.ses.last_error = e.number;
                if self.ses.xact_abort && self.ses.trancount > 0 {
                    self.ses.doomed = true;
                }
                let mut ce = e.clone();
                ce.abort_batch = false;
                self.ses.catch_stack.push(ce);
                let r2 = self.exec_stmts(catch);
                self.ses.catch_stack.pop();
                r2
            }
            other => other,
        }
    }

    // ---- names
    pub fn split_name(&self, parts: &[String]) -> (Option<String>, Option<String>, String) {
        let n = parts.len();
        let name = parts[n - 1].clone();
        let schema = if n >= 2 && !parts[n - 2].is_empty() { Some(parts[n - 2].clone()) } else { None };
        let db = if n >= 3 { Some(parts[n - 3].clone()) } else { None };
        (db, schema, name)
    }

    pub fn locate(&self, parts: &[String]) -> Loc {
        self.locate_via(parts, 0)
    }

    /// `locate`, following synonyms to the object they stand for (a chain of them is followed a few steps).
    fn locate_via(&self, parts: &[String], depth: usize) -> Loc {
        let (db, schema, name) = self.split_name(parts);
        if name.starts_with("##") {
            return Loc::GTemp(name.to_lowercase());
        }
        if name.starts_with('#') {
            return Loc::Temp(name.to_lowercase());
        }
        if name.starts_with('@') {
            return Loc::Var(name.to_lowercase());
        }
        let dbn = db.unwrap_or_else(|| self.ses.db.clone()).to_lowercase();
        let key = obj_key(&schema.unwrap_or_else(|| "dbo".to_string()), &name);
        if depth < 8 {
            if let Some(Object::Synonym(s)) = self.srv.databases.get(&dbn).and_then(|d| d.objects.get(&key)) {
                return self.locate_via(&s.target.clone(), depth + 1);
            }
        }
        Loc::Db(dbn, key)
    }

    pub fn table_ref(&self, loc: &Loc) -> Option<&Table> {
        match loc {
            Loc::Temp(n) => self.ses.temp.get(n),
            Loc::GTemp(n) => self.srv.global_temp.get(n),
            Loc::Var(n) => self.ses.frames.last().and_then(|f| f.tables.get(n)),
            Loc::Db(d, k) => match self.srv.databases.get(d).filter(|db| !db.restoring).and_then(|db| db.objects.get(k)) {
                Some(Object::Table(t)) => Some(t),
                _ => None,
            },
        }
    }

    pub fn object_ref(&self, loc: &Loc) -> Option<&Object> {
        match loc {
            Loc::Db(d, k) => self.srv.databases.get(d).and_then(|db| db.objects.get(k)),
            _ => None,
        }
    }

    /// A working copy of the table at `loc` for a DML statement. The catalog keeps its entry (copies share the
    /// row storage and the identity counter), so this needs no exclusive access to the catalog. `None` when the
    /// table is already checked out further up the call stack (a cascade that came back around).
    pub fn checkout_table(&mut self, loc: &Loc) -> Option<Table> {
        let key = format!("{:?}", loc);
        if self.ses.checked_out.contains(&key) {
            return None;
        }
        let t = self.table_ref(loc).cloned()?;
        self.ses.checked_out.push(key);
        Some(t)
    }

    pub fn checkin_table(&mut self, loc: &Loc, _t: Table) {
        let key = format!("{:?}", loc);
        if let Some(i) = self.ses.checked_out.iter().rposition(|k| *k == key) {
            self.ses.checked_out.remove(i);
        }
    }

    pub fn take_table(&mut self, loc: &Loc) -> Option<Table> {
        match loc {
            Loc::Temp(n) => self.ses.temp.remove(n),
            Loc::GTemp(n) => self.srv.cat_mut().global_temp.remove(n),
            Loc::Var(n) => self.ses.frames.last_mut().and_then(|f| f.tables.remove(n)),
            Loc::Db(d, k) => {
                let db = self.srv.cat_mut().databases.get_mut(d)?;
                match db.objects.remove(k) {
                    Some(Object::Table(t)) => Some(t),
                    Some(other) => {
                        db.objects.insert(k.clone(), other);
                        None
                    }
                    None => None,
                }
            }
        }
    }

    pub fn put_table(&mut self, loc: &Loc, t: Table) {
        match loc {
            Loc::Temp(n) => {
                self.ses.temp.insert(n.clone(), t);
            }
            Loc::GTemp(n) => {
                self.srv.cat_mut().global_temp.insert(n.clone(), t);
            }
            Loc::Var(n) => {
                if let Some(f) = self.ses.frames.last_mut() {
                    f.tables.insert(n.clone(), t);
                }
            }
            Loc::Db(d, k) => {
                if let Some(db) = self.srv.cat_mut().databases.get_mut(d) {
                    db.objects.insert(k.clone(), Object::Table(t));
                }
            }
        }
    }

    pub fn invalid_object(&self, parts: &[String]) -> SqlError {
        if parts.len() >= 3 {
            if let Some(d) = self.srv.databases.get(&parts[0].to_lowercase()).filter(|d| d.restoring) {
                return SqlError::with(927, 14, 1, format!("Database '{}' cannot be opened. It is in the middle of a restore.", d.name));
            }
        }
        SqlError::new(208, format!("Invalid object name '{}'.", parts.join(".")))
    }

    // ---- cursors
    fn exec_cursor_op(&mut self, op: &str, name: &str) -> Flow<()> {
        let key = self.cursor_key(name);
        if !self.ses.cursors.contains_key(&key) {
            return Err(SqlError::with(16916, 16, 1, format!("A cursor with the name '{}' does not exist.", name)).into());
        }
        match op {
            "OPEN" => {
                let q = self.ses.cursors[&key].query.clone();
                let rs = self.exec_query(&q, None)?;
                let c = self.ses.cursors.get_mut(&key).unwrap();
                c.columns = rs.schema.cols.iter().map(|c| c.name.clone()).collect();
                c.rows = (*rs.rows).clone();
                c.pos = -1;
                c.open = true;
            }
            "CLOSE" => {
                let c = self.ses.cursors.get_mut(&key).unwrap();
                if !c.open {
                    return Err(SqlError::with(16917, 16, 1, format!("Cursor is not open.")).into());
                }
                c.open = false;
                c.rows.clear();
            }
            _ => {
                self.ses.cursors.remove(&key);
            }
        }
        Ok(())
    }

    fn cursor_key(&self, name: &str) -> String {
        if let Some(f) = self.ses.frames.last() {
            if let Some(k) = f.cursor_vars.get(name) {
                return k.clone();
            }
        }
        name.to_string()
    }

    fn exec_fetch(&mut self, orient: &str, arg: &Option<Expr>, name: &str, into: &Option<Vec<String>>) -> Flow<()> {
        let key = self.cursor_key(name);
        let n = match arg {
            Some(e) => {
                let env = query::Env::empty();
                let v = self.ev(e, &env)?;
                convert::cast(&v, &DataType::BigInt, None)?.as_i64().unwrap_or(0)
            }
            None => 0,
        };
        let c = match self.ses.cursors.get_mut(&key) {
            Some(c) => c,
            None => return Err(SqlError::with(16916, 16, 1, format!("A cursor with the name '{}' does not exist.", name)).into()),
        };
        if !c.open {
            return Err(SqlError::with(16917, 16, 1, format!("Cursor is not open.")).into());
        }
        let len = c.rows.len() as i64;
        let target = match orient {
            "NEXT" => c.pos + 1,
            "PRIOR" => c.pos - 1,
            "FIRST" => 0,
            "LAST" => len - 1,
            "ABSOLUTE" => {
                if n > 0 {
                    n - 1
                } else {
                    len + n
                }
            }
            _ => c.pos + n,
        };
        let row = if target >= 0 && target < len {
            c.pos = target;
            Some(c.rows[target as usize].clone())
        } else {
            c.pos = if target < 0 { -1 } else { len };
            None
        };
        let cols = c.columns.clone();
        match row {
            Some(r) => {
                self.ses.fetch_status = 0;
                match into {
                    Some(vars) => {
                        for (i, v) in vars.iter().enumerate() {
                            if i < r.len() {
                                self.set_var(v, r[i].clone())?;
                            }
                        }
                    }
                    None => {
                        let rs = query::Rowset::from_rows(&cols, vec![r]);
                        self.emit_rowset(rs);
                    }
                }
            }
            None => {
                self.ses.fetch_status = -1;
            }
        }
        Ok(())
    }

    fn exec_dbcc(&mut self, name: &str, args: &[Expr]) -> Flow<()> {
        let env = query::Env::empty();
        match name {
            "FREEPROCCACHE" | "FREESYSTEMCACHE" | "DROPCLEANBUFFERS" => {
                if !self.is_sysadmin() {
                    return Err(SqlError::with(262, 14, 1, format!("DBCC {} permission denied in database 'master'.", name)).into());
                }
                clear_parse_cache();
                self.srv.core.clear_stats();
                self.emit(Event::Message("DBCC execution completed. If DBCC printed error messages, contact your system administrator.".into()));
            }
            "CHECKIDENT" => {
                let tn = self.ev(&args[0], &env)?;
                let parts: Vec<String> = convert::to_str(&tn).split('.').map(|s| s.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                let loc = self.locate(&parts);
                let reseed = args.get(1).map(|a| self.ev(a, &env)).transpose()?;
                let reseed_c = reseed.clone();
                let newv = args.get(2).map(|a| self.ev(a, &env)).transpose()?;
                let t = self.take_table(&loc).ok_or_else(|| self.invalid_object(&parts))?;
                if let Some(c) = t.identity_col() {
                    let inc = t.columns[c].identity.unwrap().1;
                    let last = t.identity_next.get().map(|n| n - inc);
                    if let (Some(r), Some(nv)) = (reseed.clone(), newv) {
                        if convert::to_str(&r).eq_ignore_ascii_case("RESEED") {
                            t.identity_next.set(nv.as_i64().map(|v| v + inc));
                        }
                    } else if reseed_c.is_some() {
                        // RESEED with no value: set to max
                    }
                    let msg = format!("Checking identity information: current identity value '{}'.", last.map(|v| v.to_string()).unwrap_or("NULL".into()));
                    self.put_table(&loc, t);
                    self.emit(Event::Message(msg));
                } else {
                    self.put_table(&loc, t);
                }
                self.emit(Event::Message("DBCC execution completed. If DBCC printed error messages, contact your system administrator.".into()));
            }
            _ => self.emit(Event::Message("DBCC execution completed. If DBCC printed error messages, contact your system administrator.".into())),
        }
        Ok(())
    }

    // ------------------------------------------------- procedures/functions
    pub fn find_proc(&self, parts: &[String]) -> Option<ProcDef> {
        let (db, schema, name) = self.split_name(parts);
        let dbn = db.unwrap_or_else(|| self.ses.db.clone()).to_lowercase();
        let key = obj_key(&schema.unwrap_or_else(|| "dbo".into()), &name);
        match self.srv.databases.get(&dbn).and_then(|d| d.objects.get(&key)) {
            Some(Object::Proc(p)) => Some(p.clone()),
            _ => None,
        }
    }

    pub fn find_func(&self, schema: Option<&str>, name: &str) -> Option<FuncDef> {
        let db = self.srv.databases.get(&self.ses.db.to_lowercase())?;
        let key = obj_key(schema.unwrap_or("dbo"), name);
        match db.objects.get(&key) {
            Some(Object::Func(f)) => Some(f.clone()),
            _ => None,
        }
    }

    fn exec_exec(&mut self, ex: &ExecStmt) -> Flow<()> {
        let env = query::Env::empty();
        let name_parts: Vec<String> = if ex.name_is_var {
            let v = self.get_var(&ex.name[0])?;
            convert::to_str(&v).split('.').map(|s| s.trim_matches(|c| c == '[' || c == ']').to_string()).collect()
        } else {
            ex.name.clone()
        };
        // evaluate arguments in caller context
        struct Bound {
            name: Option<String>,
            val: Value,
            table: Option<Table>,
            out_var: Option<String>,
            is_default: bool,
        }
        let mut bound: Vec<Bound> = vec![];
        for a in &ex.args {
            if matches!(a.e, Expr::Default) {
                bound.push(Bound { name: a.name.clone(), val: Value::Null, table: None, out_var: None, is_default: true });
                continue;
            }
            // table-valued parameter?
            if let Expr::Var(v) = &a.e {
                if let Some(t) = self.ses.frames.last().and_then(|f| f.tables.get(v)) {
                    bound.push(Bound { name: a.name.clone(), val: Value::Null, table: Some(t.clone()), out_var: None, is_default: false });
                    continue;
                }
            }
            let val = self.ev(&a.e, &env)?;
            let out_var = if a.out {
                match &a.e {
                    Expr::Var(v) => Some(v.clone()),
                    _ => None,
                }
            } else {
                None
            };
            bound.push(Bound { name: a.name.clone(), val, table: None, out_var, is_default: false });
        }
        let plain: Vec<String> = name_parts.clone();
        let lname = plain.last().unwrap().to_lowercase();
        let proc_def = self.find_proc(&plain).or_else(|| {
            if plain.len() == 1 && lname.starts_with("sp_") {
                // system procs live in master
                let k = obj_key("dbo", &lname);
                match self.srv.databases.get("master").and_then(|d| d.objects.get(&k)) {
                    Some(Object::Proc(p)) => Some(p.clone()),
                    _ => None,
                }
            } else {
                None
            }
        });
        let rc: Value;
        if let Some(p) = proc_def {
            // bind parameters
            let mut pvals: Vec<Option<(Value, Option<Table>, Option<String>)>> = vec![None; p.params.len()];
            let mut pos = 0usize;
            for b in &bound {
                let idx = match &b.name {
                    Some(n) => match p.params.iter().position(|pp| pp.name.eq_ignore_ascii_case(n)) {
                        Some(i) => i,
                        None => return Err(SqlError::new(8145, format!("{} is not a parameter for procedure {}.", n, p.name)).into()),
                    },
                    None => {
                        let i = pos;
                        pos += 1;
                        if i >= p.params.len() {
                            return Err(SqlError::new(8144, format!("Procedure or function {} has too many arguments specified.", p.name)).into());
                        }
                        i
                    }
                };
                if b.is_default {
                    continue;
                }
                pvals[idx] = Some((b.val.clone(), b.table.clone(), b.out_var.clone()));
            }
            // new frame
            let mut frame = Frame::default();
            let mut out_map: Vec<(String, String)> = vec![];
            for (i, prm) in p.params.iter().enumerate() {
                if let Some(defs) = self.table_type_defs(&prm.ty) {
                    let mut t = self.build_table(&self.ses.db.clone(), "dbo", &prm.name, &defs, true)?;
                    if let Some(Some((_, Some(src), _))) = pvals.get(i) {
                        let rows: Vec<Row> = src.data.iter(crate::storage::ReadView::dirty()).into_iter().map(|(_, r)| r).collect();
                        t.load_rows(rows);
                    }
                    frame.tables.insert(prm.name.clone(), t);
                    continue;
                }
                let dt = self.resolve_type(&prm.ty)?;
                let val = match &pvals[i] {
                    Some((v, _, ov)) => {
                        if let Some(ov) = ov {
                            out_map.push((prm.name.clone(), ov.clone()));
                        }
                        convert::cast(v, &dt, None)?
                    }
                    None => match &prm.default {
                        Some(d) => {
                            let v = self.ev(d, &env)?;
                            convert::cast(&v, &dt, None)?
                        }
                        None => {
                            return Err(SqlError::new(201, format!("Procedure or function '{}' expects parameter '{}', which was not supplied.", p.name, prm.name)).into());
                        }
                    },
                };
                frame.vars.insert(prm.name.clone(), Var { ty: dt, val });
            }
            let pdb = self.split_name(&plain).0.unwrap_or_else(|| self.ses.db.clone()).to_lowercase();
            let allowed = self.check_perm("EXECUTE", &pdb, &p.schema, Some(&obj_key(&p.schema, &p.name)), &p.name);
            self.audit_access("EXECUTE", &Loc::Db(pdb.clone(), obj_key(&p.schema, &p.name)), allowed.is_ok());
            allowed?;
            let owner = self.schema_owner_of(&pdb, &p.schema);
            self.ses.module_owner.push(owner);
            let scope = self.enter_module();
            let r = self.run_proc_body(&p.body, frame, &mut out_map);
            // an error raised inside the procedure (and not already attributed to a deeper one) is the procedure's
            let r = match r {
                Err(Interrupt::Error(mut e)) if e.procedure.is_none() => {
                    e.procedure = Some(p.name.clone());
                    Err(Interrupt::Error(e))
                }
                other => other,
            };
            self.leave_module(scope);
            self.ses.module_owner.pop();
            rc = r?;
        } else if let Some(v) = self.exec_sysproc(&lname, &bound.iter().map(|b| (b.name.clone(), b.val.clone(), b.out_var.clone())).collect::<Vec<_>>())? {
            rc = v;
        } else {
            return Err(SqlError::new(2812, format!("Could not find stored procedure '{}'.", plain.join("."))).into());
        }
        if let Some(rv) = &ex.retvar {
            self.set_var(rv, rc)?;
        }
        Ok(())
    }

    /// Run a procedure body in a fresh frame; copy OUTPUT params back to the caller.
    pub fn run_proc_body(&mut self, body: &Arc<Vec<Stmt>>, frame: Frame, out_map: &mut Vec<(String, String)>) -> Flow<Value> {
        if self.ses.nest_level >= 32 {
            return Err(SqlError::new(217, "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32).").into());
        }
        self.ses.frames.push(frame);
        self.ses.nest_level += 1;
        self.ses.ret = Value::Null;
        let r = self.exec_stmts(body);
        self.ses.nest_level -= 1;
        let callee = self.ses.frames.pop().unwrap();
        let rv = match r {
            Ok(()) => Value::Int(0),
            Err(Interrupt::Return) => {
                let v = std::mem::replace(&mut self.ses.ret, Value::Null);
                if v.is_null() {
                    Value::Int(0)
                } else {
                    v
                }
            }
            Err(e) => return Err(e),
        };
        for (pname, caller_var) in out_map.iter() {
            if let Some(v) = callee.vars.get(pname) {
                self.set_var(caller_var, v.val.clone())?;
            }
        }
        Ok(rv)
    }

    /// What a module (a stored procedure, a dynamic batch) gives back when it ends: the local temporary tables it
    /// created are dropped and the SET options it changed return to what the caller had.
    pub fn enter_module(&self) -> ModuleScope {
        ModuleScope {
            temps: self.ses.temp.keys().cloned().collect(),
            nocount: self.ses.nocount,
            xact_abort: self.ses.xact_abort,
            rowcount_limit: self.ses.rowcount_limit,
            datefirst: self.ses.datefirst,
            language: self.ses.language.clone(),
            lock_timeout: self.ses.lock_timeout,
            isolation: self.ses.isolation,
            fmtonly: self.ses.fmtonly,
            options: self.ses.options,
        }
    }

    pub fn leave_module(&mut self, s: ModuleScope) {
        let made: Vec<String> = self.ses.temp.keys().filter(|k| !s.temps.contains(*k)).cloned().collect();
        for k in made {
            self.ses.temp.remove(&k);
        }
        self.ses.nocount = s.nocount;
        self.ses.xact_abort = s.xact_abort;
        self.ses.rowcount_limit = s.rowcount_limit;
        self.ses.datefirst = s.datefirst;
        self.ses.language = s.language;
        self.ses.lock_timeout = s.lock_timeout;
        self.ses.isolation = s.isolation;
        self.ses.fmtonly = s.fmtonly;
        self.ses.options = s.options;
    }

    pub fn exec_dynamic(&mut self, sql: &str) -> Flow<()> {
        let stmts = cached_parse(sql).map_err(|mut e| {
            e.abort_batch = false;
            e
        })?;
        self.ses.frames.push(Frame::default());
        self.ses.nest_level += 1;
        self.ses.module_owner.push(String::new()); // dynamic SQL breaks the ownership chain
        let scope = self.enter_module();
        let r = self.exec_stmts(&stmts);
        self.leave_module(scope);
        self.ses.module_owner.pop();
        self.ses.nest_level -= 1;
        self.ses.frames.pop();
        match r {
            Err(Interrupt::Return) => Ok(()),
            other => other,
        }
    }

    /// Execute a scalar user-defined function.
    pub fn call_scalar_udf(&mut self, f: &FuncDef, args: Vec<Value>, default_mask: &[bool]) -> Result<Value> {
        let def = f.def.clone();
        if args.len() > def.params.len() {
            return Err(SqlError::new(8144, format!("Procedure or function {}.{} has too many arguments specified.", f.schema, f.name)));
        }
        let mut frame = Frame::default();
        for (i, prm) in def.params.iter().enumerate() {
            let dt = self.resolve_type(&prm.ty)?;
            let val = match args.get(i).filter(|_| !default_mask.get(i).copied().unwrap_or(false)) {
                Some(v) => convert::cast(v, &dt, None)?,
                None => match &prm.default {
                    Some(d) => {
                        let env = query::Env::empty();
                        let v = self.ev(d, &env)?;
                        convert::cast(&v, &dt, None)?
                    }
                    None => return Err(SqlError::new(313, format!("An insufficient number of arguments were supplied for the procedure or function {}.{}.", f.schema, f.name))),
                },
            };
            frame.vars.insert(prm.name.clone(), Var { ty: dt, val });
        }
        let rt = self.resolve_type(def.ret_type.as_ref().unwrap())?;
        let fdb = self.ses.db.to_lowercase();
        self.check_perm("EXECUTE", &fdb, &f.schema, Some(&obj_key(&f.schema, &f.name)), &f.name)?;
        let owner = self.schema_owner_of(&fdb, &f.schema);
        self.ses.frames.push(frame);
        self.ses.strict += 1;
        self.ses.nest_level += 1;
        self.ses.ret = Value::Null;
        self.ses.module_owner.push(owner);
        let r = self.exec_stmts(&def.body);
        self.ses.module_owner.pop();
        self.ses.nest_level -= 1;
        self.ses.strict -= 1;
        self.ses.frames.pop();
        match r {
            Ok(()) => Ok(Value::Null),
            Err(Interrupt::Return) => {
                let v = std::mem::replace(&mut self.ses.ret, Value::Null);
                convert::cast(&v, &rt, None)
            }
            Err(Interrupt::Error(e)) => Err(e),
            Err(_) => Ok(Value::Null),
        }
    }
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Bit(b) => Some(*b as i64),
            Value::Int(i) => Some(*i as i64),
            Value::BigInt(i) => Some(*i),
            Value::Dec(d) => Some(d.to_i128_trunc() as i64),
            Value::Float(f) => Some(*f as i64),
            _ => None,
        }
    }
}
