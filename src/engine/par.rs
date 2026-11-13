//! Intra-query parallelism.
//!
//! A statement runs on its session's thread. Where it spends its time evaluating expressions over many rows (the conditions
//! of a scan, the arguments of aggregates, the group keys), that evaluation is split over a few worker threads and the results
//! are put back in row order, so a query returns what it returned before, in the same order:
//!
//! * the table is still read by the session's thread (visibility, locks, waiting for writers and every other rule of a scan are
//!   unchanged); only the evaluation of the rows it has read is shared out, a batch at a time: the workers start when a batch
//!   is complete and the session's thread waits for them, so nothing else is going on while they read the catalog;
//! * a worker is a [`Engine`] of its own: it borrows the catalog the session holds (shared, read-only) and has a copy of the few
//!   things an expression reads of the session (the variables, the date and language settings);
//! * only expressions that read nothing but the row, the variables and the settings are shared out (see [`par_safe`]): no
//!   subqueries, no functions of the user's, no sequences, no `@@` variables, no randomness. A statement that has any such
//!   expression, a row-level security policy or a masked column runs as before.
//!
//! The degree of parallelism is the server's `max degree of parallelism` (0: the number of processors, at most 8) or the
//! query's `OPTION (MAXDOP n)`, and never more than the processors there are. A scan is shared out only when the table has
//! at least [`min_slots`] rows. Differences from a serial run: the sum of `float` values is added in a different order, so
//! the last digits can differ (SQL Server's parallel plans do the same); everything else, including the order of the rows
//! and which error is raised first, is the same.
use super::query::{Env, Schema};
use super::*;
use crate::ast::{Expr, FuncCall};
use crate::storage::RowId;

/// The tables with fewer slots than this are scanned by one thread. (`TSQL_PARALLEL_MIN_SLOTS` overrides it, for tests.)
pub fn min_slots() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("TSQL_PARALLEL_MIN_SLOTS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000))
}

/// How many batches of rows have been evaluated by workers since the process started (for tests).
static BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn parallel_batches() -> u64 {
    BATCHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Rows each worker is given per batch.
const ROWS_PER_WORKER: usize = 2048;

/// Built-in scalar functions that read nothing but their arguments.
const PURE: &[&str] = &[
    "ABS", "CEILING", "FLOOR", "ROUND", "SIGN", "POWER", "SQRT", "SQUARE", "EXP", "LOG", "LOG10", "UPPER", "LOWER", "LTRIM", "RTRIM", "TRIM", "LEN", "DATALENGTH", "SUBSTRING", "LEFT", "RIGHT", "REPLACE",
    "REVERSE", "CHARINDEX", "PATINDEX", "CONCAT", "STR", "SPACE", "REPLICATE", "ISNULL", "COALESCE", "NULLIF", "IIF", "CHOOSE", "YEAR", "MONTH", "DAY", "DATEPART", "DATENAME", "DATEADD", "DATEDIFF",
    "DATEFROMPARTS", "EOMONTH", "ISNUMERIC", "ISDATE", "ASCII", "CHAR", "UNICODE", "NCHAR", "TRY_CAST", "TRY_CONVERT", "CONVERT",
];

/// Does the expression read nothing but the row, the variables and the session's settings, and have no effect? Only such an
/// expression is evaluated by workers.
pub fn par_safe(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Col(_) | Expr::Var(_) => true,
        Expr::Neg(a) | Expr::BitNot(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Paren(a) | Expr::Collate(a, _) => par_safe(a),
        Expr::Binary(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) | Expr::IsDistinct(a, b, _) | Expr::AtTimeZone(a, b) => par_safe(a) && par_safe(b),
        Expr::InList(a, list, _) => par_safe(a) && list.iter().all(par_safe),
        Expr::Like(a, b, c, _) => par_safe(a) && par_safe(b) && c.as_ref().is_none_or(|x| par_safe(x)),
        Expr::Between(a, b, c, _) => par_safe(a) && par_safe(b) && par_safe(c),
        Expr::Case { operand, whens, els } => operand.as_ref().is_none_or(|x| par_safe(x)) && whens.iter().all(|(w, t)| par_safe(w) && par_safe(t)) && els.as_ref().is_none_or(|x| par_safe(x)),
        Expr::Cast { e, style, .. } => par_safe(e) && style.as_ref().is_none_or(|x| par_safe(x)),
        Expr::Func(fc) => func_safe(fc),
        _ => false,
    }
}

fn func_safe(fc: &FuncCall) -> bool {
    fc.over.is_none() && fc.schema.is_none() && fc.within.is_none() && !fc.distinct && !fc.star && PURE.contains(&fc.name.as_str()) && fc.args.iter().all(par_safe)
}

impl SessionState {
    /// What a worker evaluating expressions for this session needs of it.
    pub fn worker_copy(&self) -> SessionState {
        let mut w = SessionState::new(self.spid);
        w.login = self.login.clone();
        w.db = self.db.clone();
        w.language = self.language.clone();
        w.datefirst = self.datefirst;
        w.options = self.options;
        w.isolation = self.isolation;
        w.stmt_ts = self.stmt_ts;
        w.period_now = self.period_now;
        w.rowcount = self.rowcount;
        w.context_info = self.context_info.clone();
        w.app_name = self.app_name.clone();
        w.host_name = self.host_name.clone();
        // the variables of the batch being run
        if let Some(top) = self.frames.last() {
            w.frames = vec![Frame { vars: top.vars.clone(), ..Frame::default() }];
        }
        w
    }
}

impl SessionState {
    /// A second session for the same login and settings, for statements that only read (parallel MARS requests, see
    /// `tds/mars_par.rs`): same session number, database, temporary tables, isolation level and settings; its own variables and
    /// statement state; it is not entered in the session table.
    pub fn fork(&self) -> SessionState {
        let mut f = self.worker_copy();
        f.frames = vec![];
        f.temp = self.temp.clone();
        f.login_id = self.login_id;
        f.imp_stack = self.imp_stack.clone();
        f.lock_timeout = self.lock_timeout;
        f.nocount = self.nocount;
        f.rowcount_limit = self.rowcount_limit;
        f.xact_abort = self.xact_abort;
        f.stat_buf = self.stat_buf.clone();
        f
    }
}

impl<'a> Engine<'a> {
    /// The degree of parallelism for the statement being run (1: serial).
    pub fn dop(&self) -> usize {
        if self.ses.dry_run || self.ses.in_rls || self.ses.fmtonly {
            return 1;
        }
        let hint = self.ses.query_opts.iter().find_map(|o| o.strip_prefix("MAXDOP ").and_then(|n| n.trim().parse::<usize>().ok()));
        if hint == Some(1) {
            return 1;
        }
        // (for tests: a degree of parallelism that does not depend on the machine)
        if let Some(n) = std::env::var("TSQL_PARALLEL_FORCE_DOP").ok().and_then(|v| v.parse::<usize>().ok()) {
            return n.max(1);
        }
        let configured = self.srv.core.run_value("max degree of parallelism").max(0) as usize;
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let wanted = hint.unwrap_or(configured);
        let n = if wanted == 0 { cores.min(8) } else { wanted };
        n.min(cores).max(1)
    }

    /// Run `work` over `rows` on `dop` workers, each taking a contiguous part of the rows, and give the results back in the
    /// order of the parts. `work` gets an engine of its own (sharing the catalog) and the part.
    pub(super) fn par_map<T: Send, R: Send>(&mut self, rows: Vec<T>, dop: usize, work: &(dyn Fn(&mut Engine<'_>, Vec<T>) -> R + Sync)) -> Vec<R> {
        BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let per = rows.len().div_ceil(dop.max(1)).max(1);
        let mut parts: Vec<Vec<T>> = vec![];
        let mut it = rows.into_iter();
        loop {
            let part: Vec<T> = it.by_ref().take(per).collect();
            if part.is_empty() {
                break;
            }
            parts.push(part);
        }
        // each worker's session is made here (a session is not shared between threads, but can move to one)
        let sessions: Vec<SessionState> = parts.iter().map(|_| self.ses.worker_copy()).collect();
        let srv: &ServerState = &self.srv;
        std::thread::scope(|s| {
            let handles: Vec<_> = parts
                .into_iter()
                .zip(sessions)
                .map(|(part, mut ws)| {
                    s.spawn(move || {
                        let mut eng = Engine { srv: SrvGuard::shared(srv), ses: &mut ws, out: vec![], sink: None };
                        work(&mut eng, part)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
        })
    }

    /// The rows of `rows` that satisfy all of `conds` (see [`Engine::filter_pushed`]), evaluated by `dop` workers.
    pub(super) fn par_filter(&mut self, schema: &Schema, rows: Vec<(RowId, Row)>, conds: &[Expr], dop: usize) -> Result<Vec<(RowId, Row)>> {
        let parts = self.par_map(rows, dop, &|eng, part| eng.filter_pushed(schema, part, conds));
        let mut out = Vec::new();
        for p in parts {
            out.extend(p?);
        }
        Ok(out)
    }

    /// A scan that has batches of rows to evaluate: the number of rows to collect before sharing them out (0: do not).
    pub(super) fn par_batch(&self, t: &Table, flt: Option<(&Schema, &[Expr])>) -> (usize, usize) {
        let Some((sc, conds)) = flt else { return (0, 1) };
        if conds.is_empty() || t.data.slot_count() < min_slots() || sc.cols.iter().any(|c| c.denied.is_some()) || !conds.iter().all(par_safe) {
            return (0, 1);
        }
        let dop = self.dop();
        if dop < 2 {
            return (0, 1);
        }
        (dop * ROWS_PER_WORKER, dop)
    }

    /// Evaluate an expression for one row in a worker (a helper of the callers of [`Engine::par_map`]).
    pub(super) fn ev_row(&mut self, schema: &Schema, row: &[Value], e: &Expr) -> Result<Value> {
        let env = Env::new(schema, row, None);
        self.ev(e, &env)
    }
}
