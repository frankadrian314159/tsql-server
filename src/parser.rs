//! Recursive-descent parser for T-SQL.
use crate::ast::*;
use crate::decimal::Dec;
use crate::error::{Result, SqlError};
use crate::lexer::{tokenize, Tok, Token};
use crate::types::Value;
use std::collections::HashSet;
use std::sync::Arc as Rc;
use std::sync::OnceLock;

fn reserved() -> &'static HashSet<&'static str> {
    static R: OnceLock<HashSet<&'static str>> = OnceLock::new();
    R.get_or_init(|| {
        "ADD ALL ALTER AND ANY AS ASC AUTHORIZATION BACKUP BEGIN BETWEEN BREAK BROWSE BULK BY CASCADE CASE CHECK
CHECKPOINT CLOSE CLUSTERED COLLATE COLUMN COMMIT COMPUTE CONSTRAINT CONTAINS CONTINUE CREATE
CROSS CURRENT CURRENT_DATE CURRENT_TIME CURRENT_TIMESTAMP CURRENT_USER CURSOR DATABASE DBCC DEALLOCATE DECLARE
DEFAULT DELETE DENY DESC DISK DISTINCT DISTRIBUTED DOUBLE DROP DUMP ELSE END ERRLVL ESCAPE EXCEPT EXEC EXECUTE
EXISTS EXIT EXTERNAL FETCH FILE FILLFACTOR FOR FOREIGN FREETEXT FROM FULL FUNCTION GOTO GRANT GROUP HAVING HOLDLOCK
IDENTITY IDENTITY_INSERT IDENTITYCOL IF IN INDEX INNER INSERT INTERSECT INTO IS JOIN KEY KILL LEFT LIKE LINENO LOAD
MERGE NATIONAL NOCHECK NONCLUSTERED NOT NULL OF OFF OFFSETS ON OPEN OPENDATASOURCE OPENQUERY OPENROWSET
OPENXML OPTION OR ORDER OUTER OVER PERCENT PIVOT PLAN PRECISION PRIMARY PRINT PROC PROCEDURE PUBLIC RAISERROR READ
READTEXT RECONFIGURE REFERENCES REPLICATION RESTORE RESTRICT RETURN REVERT REVOKE RIGHT ROLLBACK ROWCOUNT
ROWGUIDCOL RULE SAVE SCHEMA SELECT SESSION_USER SET SETUSER SHUTDOWN SOME STATISTICS SYSTEM_USER TABLE TABLESAMPLE TEXTSIZE
THEN TO TOP TRAN TRANSACTION TRIGGER TRUNCATE TSEQUAL UNION UNIQUE UNPIVOT UPDATE UPDATETEXT USE USER
VALUES VARYING VIEW WAITFOR WHEN WHERE WHILE WITH WITHIN WRITETEXT THROW OUTPUT USING WINDOW"
            .split_whitespace()
            .collect()
    })
}

const STMT_START: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "ALTER", "DROP", "TRUNCATE", "DECLARE", "SET", "IF", "WHILE", "BEGIN", "RETURN", "BREAK", "CONTINUE", "PRINT",
    "EXEC", "EXECUTE", "USE", "COMMIT", "ROLLBACK", "SAVE", "THROW", "RAISERROR", "OPEN", "CLOSE", "FETCH", "DEALLOCATE", "WAITFOR", "GOTO", "GRANT", "REVOKE", "DENY",
    "DBCC", "WITH", "END", "ELSE", "REVERT", "BACKUP", "RESTORE", "KILL", "RECONFIGURE", "BULK", "ENABLE", "DISABLE",
];
const NILADIC: &[&str] = &["CURRENT_TIMESTAMP", "CURRENT_USER", "SESSION_USER", "SYSTEM_USER", "USER", "CURRENT_DATE"];
const DATEPART_FUNCS: &[&str] = &["DATEADD", "DATEDIFF", "DATEDIFF_BIG", "DATEPART", "DATENAME", "DATETRUNC", "DATE_BUCKET"];
/// The methods of the hierarchyid type (see hierarchyid.rs), written `expr.GetLevel()`.
const HID_METHODS: &[&str] = &[
    "GETLEVEL", "TOSTRING", "GETANCESTOR", "GETDESCENDANT", "ISDESCENDANTOF", "GETREPARENTEDVALUE",
    // the methods of geometry and geography (see spatial.rs); they travel the same way
    "STASTEXT", "STNUMPOINTS", "STGEOMETRYTYPE", "STISEMPTY", "STISVALID", "STLENGTH", "STAREA", "STDISTANCE", "STINTERSECTS", "STCONTAINS", "STWITHIN", "STEQUALS", "STNUMGEOMETRIES", "STSTARTPOINT", "STENDPOINT", "STPOINTN",
    "STSRID", "STX", "STY", "LAT", "LONG",
];
/// The members of the spatial types that are written without parentheses: `@g.STX`, `@g.Lat`.
const SPATIAL_PROPS: &[&str] = &["STSRID", "STX", "STY", "LAT", "LONG"];
const RESERVED_OK_AS_FUNC: &[&str] = &["LEFT", "RIGHT", "USER", "CURRENT", "KEY", "PERCENT", "DOUBLE", "PRECISION", "FILE", "OFFSETS", "PLAN", "RULE", "READ", "LOAD", "UPDATE", "TRUNCATE", "CONTAINS", "FREETEXT"];

pub struct Parser {
    toks: std::rc::Rc<Vec<Token>>,
    i: usize,
    src: String,
    /// How deeply expressions and queries are nested at this point (see `MAX_NEST`).
    depth: usize,
}

/// Limits on the shape of an expression. The evaluator recurses once per level of the tree, so a deeply nested or very
/// long left-deep chain (`1+1+1+...`, `a OR b OR ...`) would overflow the stack and take the process down; SQL
/// Server refuses such statements with error 8632 too.
/// (Debug builds use far more stack per level, so their limits are lower.)
const MAX_NEST: usize = if cfg!(debug_assertions) { 25 } else { 100 };
const MAX_CHAIN: usize = if cfg!(debug_assertions) { 60 } else { 700 };

fn complexity_error() -> SqlError {
    SqlError::new(8632, "Internal error: An expression services limit has been reached. Please look for potentially complex expressions in your query, and try to simplify them.")
}

type R<T> = Result<T>;

pub fn parse(sql: &str) -> R<Vec<Stmt>> {
    let mut p = Parser::new(sql)?;
    p.parse_batch()
}

pub fn parse_param_decls(sql: &str) -> R<Vec<Param>> {
    let mut p = Parser::new(sql)?;
    let params = p.param_list(false)?;
    if !p.is_eof() {
        return Err(p.err());
    }
    Ok(params)
}

/// `text` without one pair of parentheses that encloses all of it, so scripted text is stable.
fn unwrap_parens(text: &str) -> String {
    let t = text.trim();
    let mut depth = 0i32;
    let mut quoted = false;
    for (i, ch) in t.char_indices() {
        match ch {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 && i + 1 < t.len() {
                    return t.to_string();
                }
            }
            _ => {}
        }
    }
    if t.starts_with('(') && t.ends_with(')') && depth == 0 {
        t[1..t.len() - 1].trim().to_string()
    } else {
        t.to_string()
    }
}

pub fn parse_expr_str(sql: &str) -> R<Expr> {
    let mut p = Parser::new(sql)?;
    p.expr()
}

impl Parser {
    pub fn new(sql: &str) -> R<Parser> {
        Ok(Parser { toks: std::rc::Rc::new(tokenize(sql)?), i: 0, src: sql.to_string(), depth: 0 })
    }

    // ---------------------------------------------------------------- helpers
    fn tok(&self) -> &Token {
        &self.toks[self.i]
    }
    fn peek(&self, k: usize) -> &Token {
        &self.toks[(self.i + k).min(self.toks.len() - 1)]
    }
    /// The text of the current token when it is a variable (a copy is made only then).
    fn var_tok(&self) -> Option<String> {
        match &self.tok().tok {
            Tok::Var(v) => Some(v.clone()),
            _ => None,
        }
    }
    /// The text of the current token when it is a string literal.
    fn str_tok(&self) -> Option<String> {
        match &self.tok().tok {
            Tok::Str(v) => Some(v.clone()),
            _ => None,
        }
    }
    /// Take the current token and give its upper-cased text (`advance` copies the whole token).
    fn advance_up(&mut self) -> String {
        let up = self.toks[self.i].up.clone();
        if self.toks[self.i].tok != Tok::Eof {
            self.i += 1;
        }
        up
    }
    fn advance(&mut self) -> Token {
        let t = self.toks[self.i].clone();
        if t.tok != Tok::Eof {
            self.i += 1;
        }
        t
    }
    fn err(&self) -> SqlError {
        self.err_at(self.tok())
    }
    fn err_at(&self, t: &Token) -> SqlError {
        if t.tok == Tok::Eof {
            SqlError::syntax_eof()
        } else {
            SqlError::syntax(&self.src[t.pos..t.end])
        }
    }
    fn is_eof(&self) -> bool {
        self.tok().tok == Tok::Eof
    }
    fn is_kw(&self, w: &str) -> bool {
        matches!(self.tok().tok, Tok::Id(_)) && self.tok().up == w
    }
    fn is_kw_any(&self, ws: &[&str]) -> bool {
        matches!(self.tok().tok, Tok::Id(_)) && ws.contains(&self.tok().up.as_str())
    }
    fn peek_kw(&self, k: usize, ws: &[&str]) -> bool {
        let t = self.peek(k);
        matches!(t.tok, Tok::Id(_)) && ws.contains(&t.up.as_str())
    }
    fn peek_is_op(&self, k: usize, o: &str) -> bool {
        matches!(&self.peek(k).tok, Tok::Op(x) if *x == o)
    }
    fn accept_kw(&mut self, w: &str) -> bool {
        if self.is_kw(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect_kw(&mut self, w: &str) -> R<()> {
        if self.accept_kw(w) {
            Ok(())
        } else {
            Err(self.err())
        }
    }
    fn is_op(&self, o: &str) -> bool {
        matches!(&self.tok().tok, Tok::Op(x) if *x == o)
    }
    fn accept_op(&mut self, o: &str) -> bool {
        if self.is_op(o) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect_op(&mut self, o: &str) -> R<()> {
        if self.accept_op(o) {
            Ok(())
        } else {
            Err(self.err())
        }
    }
    fn ident(&mut self) -> R<String> {
        match &self.tok().tok {
            Tok::Id(s) | Tok::QId(s) => {
                let s = s.clone();
                self.i += 1;
                Ok(s)
            }
            _ => Err(self.err()),
        }
    }
    fn at_eos(&self) -> bool {
        self.is_eof() || self.is_op(";")
    }
    /// The part of CREATE/ALTER SECURITY POLICY after the keywords.
    fn policy_body(&mut self) -> R<PolicyStmt> {
        use crate::catalog::{PredKind, Predicate};
        let name = self.name_parts()?;
        let (mut add, mut drop, mut state) = (vec![], vec![], None);
        loop {
            let adding = if self.accept_kw("ADD") {
                true
            } else if self.accept_kw("DROP") {
                false
            } else {
                break;
            };
            let filter = if self.accept_kw("FILTER") {
                true
            } else {
                self.expect_kw("BLOCK")?;
                false
            };
            self.expect_kw("PREDICATE")?;
            // DROP names only the table (and kind); ADD names the function too
            let f = if adding { self.name_parts()? } else { vec![] };
            let func = (if f.len() >= 2 { f[f.len() - 2].clone() } else { "dbo".to_string() }, f.last().cloned().unwrap_or_default());
            let mut args = vec![];
            if adding {
                self.expect_op("(")?;
                if !self.is_op(")") {
                    loop {
                        args.push(self.ident()?);
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                }
                self.expect_op(")")?;
            }
            self.expect_kw("ON")?;
            let t = self.name_parts()?;
            let table = format!("{}.{}", if t.len() >= 2 { t[t.len() - 2].clone() } else { "dbo".to_string() }, t.last().cloned().unwrap_or_default()).to_lowercase();
            let mut kind = if filter { PredKind::Filter } else { PredKind::BlockAfterInsert };
            let mut explicit = false;
            if !filter && self.is_kw_any(&["AFTER", "BEFORE"]) {
                let when = self.advance_up();
                let op = self.advance_up();
                explicit = true;
                kind = match (when.as_str(), op.as_str()) {
                    ("AFTER", "INSERT") => PredKind::BlockAfterInsert,
                    ("AFTER", "UPDATE") => PredKind::BlockAfterUpdate,
                    ("BEFORE", "UPDATE") => PredKind::BlockBeforeUpdate,
                    ("BEFORE", "DELETE") => PredKind::BlockBeforeDelete,
                    _ => return Err(self.err()),
                };
            }
            let pred = Predicate { kind, func, args, table };
            if adding {
                // a block predicate without AFTER/BEFORE covers every operation
                if !filter && !explicit {
                    for k in [PredKind::BlockAfterInsert, PredKind::BlockAfterUpdate, PredKind::BlockBeforeUpdate, PredKind::BlockBeforeDelete] {
                        add.push(Predicate { kind: k, ..pred.clone() });
                    }
                } else {
                    add.push(pred);
                }
            } else {
                drop.push(pred);
            }
            if !self.accept_op(",") {
                if !self.is_kw_any(&["ADD", "DROP"]) {
                    break;
                }
            }
        }
        if self.accept_kw("WITH") {
            self.expect_op("(")?;
            loop {
                let k = self.ident()?.to_uppercase();
                self.expect_op("=")?;
                let v = self.advance_up();
                if k == "STATE" {
                    state = Some(v == "ON");
                }
                if !self.accept_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        Ok(PolicyStmt { name, add, drop, state })
    }

    /// One option of CREATE / ALTER LOGIN ... WITH.
    fn login_option(&mut self, password: &mut Option<String>, default_db: &mut Option<String>, opts: &mut LoginOpts) -> R<()> {
        let opt = self.ident()?.to_uppercase();
        if opt == "MUST_CHANGE" {
            opts.must_change = true;
            return Ok(());
        }
        if opt == "UNLOCK" {
            opts.unlock = true;
            return Ok(());
        }
        if self.accept_op("=") {
            let v = self.advance();
            let val = match v.tok {
                Tok::Str(s) | Tok::Id(s) | Tok::QId(s) => s,
                _ => String::new(),
            };
            match opt.as_str() {
                "PASSWORD" => *password = Some(val),
                "OLD_PASSWORD" => opts.old_password = Some(val),
                "DEFAULT_DATABASE" => *default_db = Some(val),
                "CHECK_POLICY" => opts.check_policy = Some(v.up == "ON"),
                "CHECK_EXPIRATION" => opts.check_expiration = Some(v.up == "ON"),
                _ => {}
            }
        }
        Ok(())
    }

    /// `MASKED WITH (FUNCTION = 'default()')`: the function text.
    fn masked_with(&mut self) -> R<String> {
        self.expect_kw("MASKED")?;
        self.expect_kw("WITH")?;
        self.expect_op("(")?;
        self.expect_kw("FUNCTION")?;
        self.expect_op("=")?;
        let f = match self.advance().tok {
            Tok::Str(s) => s,
            _ => return Err(self.err()),
        };
        self.expect_op(")")?;
        Ok(f)
    }

    /// `DISK = 'path'` (one device only).
    fn backup_device(&mut self) -> R<Expr> {
        self.expect_kw("DISK")?;
        self.expect_op("=")?;
        let e = self.expr()?;
        if self.is_op(",") {
            return Err(SqlError::new(3205, "Too many backup devices specified for BACKUP or RESTORE; this server supports one DISK device."));
        }
        Ok(e)
    }

    /// `WITH name [= value] [, ...]`
    fn backup_options(&mut self) -> R<Vec<(String, Option<Expr>)>> {
        let mut opts = vec![];
        if self.accept_kw("WITH") {
            loop {
                let name = self.ident()?.to_uppercase();
                let val = if self.accept_op("=") { Some(self.expr()?) } else { None };
                opts.push((name, val));
                if !self.accept_op(",") {
                    break;
                }
            }
        }
        Ok(opts)
    }

    fn is_stmt_start(&self) -> bool {
        self.is_kw_any(STMT_START)
    }
    fn name_parts(&mut self) -> R<Vec<String>> {
        // `..name` (no database, no schema), as client libraries write a procedure of the current database
        let mut parts = if self.is_op(".") && self.peek_is_op(1, ".") {
            self.i += 2;
            vec![String::new(), String::new(), self.ident()?]
        } else {
            vec![self.ident()?]
        };
        while self.is_op(".") {
            self.i += 1;
            if self.is_op(".") {
                parts.push(String::new());
                continue;
            }
            parts.push(self.ident()?);
        }
        Ok(parts)
    }

    // ------------------------------------------------------------------ types
    fn parse_type(&mut self) -> R<TypeRef> {
        let mut parts = vec![self.ident()?];
        while self.is_op(".") {
            self.i += 1;
            parts.push(self.ident()?);
        }
        if parts.len() == 1 && parts[0].eq_ignore_ascii_case("double") && self.is_kw("PRECISION") {
            self.i += 1;
            parts = vec!["float".into()];
        }
        // xml(DOCUMENT dbo.sc), xml(CONTENT sc), xml(sc)
        if parts.len() == 1 && parts[0].eq_ignore_ascii_case("xml") && self.is_op("(") && matches!(self.toks.get(self.i + 1).map(|t| &t.tok), Some(Tok::Id(_) | Tok::QId(_))) {
            self.i += 1;
            let document = self.accept_kw("DOCUMENT");
            if !document {
                self.accept_kw("CONTENT");
            }
            let name = self.name_parts()?;
            self.expect_op(")")?;
            return Ok(TypeRef { parts, args: vec![], xml_schema: Some((name, document)) });
        }
        let mut args = vec![];
        if self.is_op("(") {
            self.i += 1;
            loop {
                match self.tok().tok.clone() {
                    Tok::Int(v) => {
                        args.push(v);
                        self.i += 1;
                    }
                    Tok::Id(_) if self.tok().up == "MAX" => {
                        args.push(-1);
                        self.i += 1;
                    }
                    _ => return Err(self.err()),
                }
                if !self.accept_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        Ok(TypeRef { parts, args, xml_schema: None })
    }

    // ------------------------------------------------------------ expressions
    pub fn expr(&mut self) -> R<Expr> {
        self.depth += 1;
        if self.depth > MAX_NEST {
            return Err(complexity_error());
        }
        let r = self.or_expr();
        self.depth -= 1;
        r
    }
    /// An expression together with its source text.
    fn expr_text(&mut self) -> R<(Expr, String)> {
        let start = self.toks.get(self.i).map(|t| t.pos).unwrap_or(self.src.len());
        let e = self.expr()?;
        let end = if self.i > 0 { self.toks[self.i - 1].end } else { start };
        Ok((e, unwrap_parens(self.src.get(start..end).unwrap_or(""))))
    }
    fn or_expr(&mut self) -> R<Expr> {
        let mut l = self.and_expr()?;
        let mut terms = 1;
        while self.accept_kw("OR") {
            terms += 1;
            if terms > MAX_CHAIN {
                return Err(complexity_error());
            }
            let r = self.and_expr()?;
            l = Expr::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn and_expr(&mut self) -> R<Expr> {
        let mut l = self.not_expr()?;
        let mut terms = 1;
        while self.accept_kw("AND") {
            terms += 1;
            if terms > MAX_CHAIN {
                return Err(complexity_error());
            }
            let r = self.not_expr()?;
            l = Expr::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn not_expr(&mut self) -> R<Expr> {
        if self.accept_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.not_expr()?)));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> R<Expr> {
        let left = self.add_expr()?;
        if let Tok::Op(o) = self.tok().tok {
            let op = match o {
                "=" => Some(BinOp::Eq),
                "<>" | "!=" => Some(BinOp::Ne),
                "<" | "!>" => Some(if o == "<" { BinOp::Lt } else { BinOp::Le }),
                ">" | "!<" => Some(if o == ">" { BinOp::Gt } else { BinOp::Ge }),
                "<=" => Some(BinOp::Le),
                ">=" => Some(BinOp::Ge),
                _ => None,
            };
            if let Some(op) = op {
                self.i += 1;
                if self.is_kw_any(&["ALL", "ANY", "SOME"]) && self.peek_is_op(1, "(") {
                    let all = self.tok().up == "ALL";
                    self.i += 2;
                    let q = self.query()?;
                    self.expect_op(")")?;
                    return Ok(Expr::CmpQuant(op, all, Box::new(left), Box::new(q)));
                }
                let r = self.add_expr()?;
                return Ok(Expr::Binary(op, Box::new(left), Box::new(r)));
            }
        }
        if matches!(self.tok().tok, Tok::Id(_)) {
            let mut u = self.tok().up.clone();
            let mut neg = false;
            if u == "IS" {
                self.i += 1;
                neg = self.accept_kw("NOT");
                if self.accept_kw("NULL") {
                    return Ok(Expr::IsNull(Box::new(left), neg));
                }
                if self.accept_kw("DISTINCT") {
                    self.expect_kw("FROM")?;
                    let r = self.add_expr()?;
                    return Ok(Expr::IsDistinct(Box::new(left), Box::new(r), neg));
                }
                return Err(self.err());
            }
            if u == "NOT" && self.peek_kw(1, &["IN", "LIKE", "BETWEEN"]) {
                self.i += 1;
                neg = true;
                u = self.tok().up.clone();
            }
            match u.as_str() {
                "IN" => {
                    self.i += 1;
                    self.expect_op("(")?;
                    if self.is_kw_any(&["SELECT", "WITH"]) {
                        let q = self.query()?;
                        self.expect_op(")")?;
                        return Ok(Expr::InQuery(Box::new(left), Box::new(q), neg));
                    }
                    let mut items = vec![self.expr()?];
                    while self.accept_op(",") {
                        items.push(self.expr()?);
                    }
                    self.expect_op(")")?;
                    return Ok(Expr::InList(Box::new(left), items, neg));
                }
                "LIKE" => {
                    self.i += 1;
                    let pat = self.add_expr()?;
                    let esc = if self.accept_kw("ESCAPE") { Some(Box::new(self.add_expr()?)) } else { None };
                    return Ok(Expr::Like(Box::new(left), Box::new(pat), esc, neg));
                }
                "BETWEEN" => {
                    self.i += 1;
                    let lo = self.add_expr()?;
                    self.expect_kw("AND")?;
                    let hi = self.add_expr()?;
                    return Ok(Expr::Between(Box::new(left), Box::new(lo), Box::new(hi), neg));
                }
                _ => {}
            }
        }
        Ok(left)
    }

    fn add_expr(&mut self) -> R<Expr> {
        let mut l = self.mul_expr()?;
        let mut terms = 1;
        loop {
            let op = match &self.tok().tok {
                Tok::Op("+") => BinOp::Add,
                Tok::Op("-") => BinOp::Sub,
                Tok::Op("&") => BinOp::BitAnd,
                Tok::Op("|") => BinOp::BitOr,
                Tok::Op("^") => BinOp::BitXor,
                _ => return Ok(l),
            };
            self.i += 1;
            terms += 1;
            if terms > MAX_CHAIN {
                return Err(complexity_error());
            }
            let r = self.mul_expr()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
    }
    fn mul_expr(&mut self) -> R<Expr> {
        let mut l = self.unary()?;
        let mut terms = 1;
        loop {
            let op = match &self.tok().tok {
                Tok::Op("*") => BinOp::Mul,
                Tok::Op("/") => BinOp::Div,
                Tok::Op("%") => BinOp::Mod,
                _ => return Ok(l),
            };
            self.i += 1;
            terms += 1;
            if terms > MAX_CHAIN {
                return Err(complexity_error());
            }
            let r = self.unary()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
    }
    fn unary(&mut self) -> R<Expr> {
        match self.tok().tok {
            Tok::Op("-") => {
                self.i += 1;
                let e = self.unary()?;
                Ok(match e {
                    Expr::Lit(Value::Int(i)) => Expr::Lit(Value::Int(-i)),
                    Expr::Lit(Value::BigInt(i)) => {
                        if i == 2147483648 {
                            Expr::Lit(Value::Int(i32::MIN))
                        } else {
                            Expr::Lit(Value::BigInt(-i))
                        }
                    }
                    Expr::Lit(Value::Dec(d)) => Expr::Lit(Value::Dec(d.neg())),
                    Expr::Lit(Value::Float(f)) => Expr::Lit(Value::Float(-f)),
                    other => Expr::Neg(Box::new(other)),
                })
            }
            Tok::Op("+") => {
                self.i += 1;
                self.unary()
            }
            Tok::Op("~") => {
                self.i += 1;
                Ok(Expr::BitNot(Box::new(self.unary()?)))
            }
            _ => self.postfix(),
        }
    }
    fn postfix(&mut self) -> R<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.is_kw("COLLATE") {
                crate::collation::mark_in_use();
                self.i += 1;
                let n = self.ident()?;
                e = Expr::Collate(Box::new(e), n);
            } else if self.is_op(".") && self.peek_kw(1, &["VALUE", "EXIST", "QUERY"]) && self.peek_is_op(2, "(") {
                self.i += 1;
                e = self.xml_method(e)?;
            } else if self.is_op(".") && self.peek_kw(1, SPATIAL_PROPS) && !self.peek_is_op(2, "(") {
                self.i += 1;
                let m = self.ident()?;
                e = Expr::Func(Box::new(FuncCall { name: format!("HID_{}", m.to_uppercase()), schema: None, parts: vec![], args: vec![e], distinct: false, star: false, over: None, within: None }));
            } else if self.is_op(".") && self.peek_kw(1, HID_METHODS) && self.peek_is_op(2, "(") {
                self.i += 1;
                let m = self.ident()?;
                e = self.hid_call(Some(e), &m)?;
            } else if self.is_kw("AT") && self.peek_kw(1, &["TIME"]) && self.peek_kw(2, &["ZONE"]) {
                self.i += 3;
                let z = self.add_expr()?;
                e = Expr::AtTimeZone(Box::new(e), Box::new(z));
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> R<Expr> {
        let toks = self.toks.clone();
        let t = &toks[self.i];
        match &t.tok {
            Tok::Int(v) => {
                self.i += 1;
                Ok(Expr::Lit(if *v >= i32::MIN as i64 && *v <= i32::MAX as i64 { Value::Int(*v as i32) } else { Value::BigInt(*v) }))
            }
            Tok::Dec(d) => {
                self.i += 1;
                Ok(Expr::Lit(Value::Dec(*d)))
            }
            Tok::Money(d) => {
                self.i += 1;
                Ok(Expr::Lit(Value::Dec(d.rescale(4).unwrap_or(*d))))
            }
            Tok::Float(f) => {
                self.i += 1;
                Ok(Expr::Lit(Value::Float(*f)))
            }
            Tok::Str(s) => {
                let unicode = self.tok().unicode;
                self.i += 1;
                if unicode {
                    // N'...' is an nvarchar value: casting it says so, and later code can tell it from a varchar literal
                    return Ok(Expr::Cast { e: Box::new(Expr::Lit(Value::Str(s.clone()))), ty: TypeRef { parts: vec!["nvarchar".to_string()], args: vec![-1], xml_schema: None }, try_: false, style: None });
                }
                Ok(Expr::Lit(Value::Str(s.clone())))
            }
            Tok::Hex(b) => {
                self.i += 1;
                Ok(Expr::Lit(Value::Bin(b.clone())))
            }
            Tok::Var(v) => {
                self.i += 1;
                if v.starts_with("@@") {
                    Ok(Expr::SysVar(v.to_uppercase()))
                } else {
                    Ok(Expr::Var(v.to_lowercase()))
                }
            }
            Tok::Op("(") => {
                self.i += 1;
                if self.is_kw_any(&["SELECT", "WITH"]) {
                    let q = self.query()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Subquery(Box::new(q)));
                }
                let e = self.expr()?;
                self.expect_op(")")?;
                Ok(Expr::Paren(Box::new(e)))
            }
            Tok::QId(_) => self.name_or_call(),
            Tok::Id(_) => {
                let u = t.up.as_str();
                match u {
                    "NULL" => {
                        self.i += 1;
                        return Ok(Expr::Lit(Value::Null));
                    }
                    "CASE" => return self.case_expr(),
                    "CAST" | "TRY_CAST" if self.peek_is_op(1, "(") => {
                        self.i += 2;
                        let e = self.expr()?;
                        self.expect_kw("AS")?;
                        let ty = self.parse_type()?;
                        self.expect_op(")")?;
                        return Ok(Expr::Cast { e: Box::new(e), ty, try_: u == "TRY_CAST", style: None });
                    }
                    "CONVERT" | "TRY_CONVERT" if self.peek_is_op(1, "(") => {
                        self.i += 2;
                        let ty = self.parse_type()?;
                        self.expect_op(",")?;
                        let e = self.expr()?;
                        let style = if self.accept_op(",") { Some(Box::new(self.expr()?)) } else { None };
                        self.expect_op(")")?;
                        return Ok(Expr::Cast { e: Box::new(e), ty, try_: u == "TRY_CONVERT", style });
                    }
                    "PARSE" | "TRY_PARSE" if self.peek_is_op(1, "(") => {
                        self.i += 2;
                        let e = self.expr()?;
                        self.expect_kw("AS")?;
                        let ty = self.parse_type()?;
                        if self.accept_kw("USING") {
                            self.expr()?;
                        }
                        self.expect_op(")")?;
                        return Ok(Expr::Cast { e: Box::new(e), ty, try_: u == "TRY_PARSE", style: None });
                    }
                    "EXISTS" if self.peek_is_op(1, "(") => {
                        self.i += 2;
                        let q = self.query()?;
                        self.expect_op(")")?;
                        return Ok(Expr::Exists(Box::new(q)));
                    }
                    "NEXT" if self.peek_kw(1, &["VALUE"]) => {
                        self.i += 2;
                        self.expect_kw("FOR")?;
                        return Ok(Expr::NextValue(self.name_parts()?));
                    }
                    "DEFAULT" => {
                        self.i += 1;
                        return Ok(Expr::Default);
                    }
                    "IDENTITY" if self.peek_is_op(1, "(") => {
                        self.i += 2;
                        let ty = self.parse_type()?;
                        let (mut seed, mut inc) = (Expr::Lit(Value::Int(1)), Expr::Lit(Value::Int(1)));
                        if self.accept_op(",") {
                            seed = self.expr()?;
                            self.expect_op(",")?;
                            inc = self.expr()?;
                        }
                        self.expect_op(")")?;
                        return Ok(Expr::IdentityFn { ty, seed: Box::new(seed), inc: Box::new(inc) });
                    }
                    "TRIM" if self.peek_is_op(1, "(") => return self.trim_call(),
                    _ => {}
                }
                if NILADIC.contains(&u) && !self.peek_is_op(1, "(") {
                    self.i += 1;
                    return Ok(Expr::Func(Box::new(FuncCall { name: u.to_string(), schema: None, parts: vec![], args: vec![], distinct: false, star: false, over: None, within: None })));
                }
                if reserved().contains(u) && !RESERVED_OK_AS_FUNC.contains(&u) {
                    return Err(self.err());
                }
                self.name_or_call()
            }
            _ => Err(self.err()),
        }
    }

    fn trim_call(&mut self) -> R<Expr> {
        self.i += 2;
        let mut name = "TRIM";
        let mut chars = None;
        let e;
        if self.is_kw_any(&["LEADING", "TRAILING", "BOTH"]) {
            name = match self.tok().up.as_str() {
                "LEADING" => "LTRIM",
                "TRAILING" => "RTRIM",
                _ => "TRIM",
            };
            self.i += 1;
            if !self.is_kw("FROM") {
                chars = Some(self.expr()?);
            }
            self.expect_kw("FROM")?;
            e = self.expr()?;
        } else {
            let first = self.expr()?;
            if self.accept_kw("FROM") {
                chars = Some(first);
                e = self.expr()?;
            } else {
                e = first;
            }
        }
        self.expect_op(")")?;
        let mut args = vec![e];
        if let Some(c) = chars {
            args.push(c);
        }
        Ok(Expr::Func(Box::new(FuncCall { name: name.to_string(), schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None })))
    }

    fn case_expr(&mut self) -> R<Expr> {
        self.i += 1;
        let operand = if !self.is_kw("WHEN") { Some(Box::new(self.expr()?)) } else { None };
        let mut whens = vec![];
        while self.accept_kw("WHEN") {
            let c = self.expr()?;
            self.expect_kw("THEN")?;
            let r = self.expr()?;
            whens.push((c, r));
        }
        if whens.is_empty() {
            return Err(self.err());
        }
        let els = if self.accept_kw("ELSE") { Some(Box::new(self.expr()?)) } else { None };
        self.expect_kw("END")?;
        Ok(Expr::Case { operand, whens, els })
    }

    fn name_or_call(&mut self) -> R<Expr> {
        let mut parts = vec![self.ident()?];
        while self.is_op(".") {
            self.i += 1;
            if self.is_op("*") {
                self.i += 1;
                return Ok(Expr::Star(Some(parts)));
            }
            parts.push(self.ident()?);
        }
        // hierarchyid::Parse('/1/'), hierarchyid::GetRoot()
        if parts.len() == 1 && parts[0].eq_ignore_ascii_case("hierarchyid") && self.is_op(":") && self.peek_is_op(1, ":") {
            self.i += 2;
            let m = self.ident()?;
            return self.hid_call(None, &m);
        }
        // geometry::Point(1, 2, 0), geography::STGeomFromText('...', 4326)
        if parts.len() == 1 && (parts[0].eq_ignore_ascii_case("geometry") || parts[0].eq_ignore_ascii_case("geography")) && self.is_op(":") && self.peek_is_op(1, ":") {
            self.i += 2;
            let m = self.ident()?;
            self.expect_op("(")?;
            let mut args = vec![];
            if !self.is_op(")") {
                args.push(self.expr()?);
                while self.accept_op(",") {
                    args.push(self.expr()?);
                }
            }
            self.expect_op(")")?;
            let name = format!("SP_S_{}_{}", parts[0].to_uppercase(), m.to_uppercase());
            return Ok(Expr::Func(Box::new(FuncCall { name, schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None })));
        }
        if self.is_op("(") {
            // column.GetLevel(), column.GetAncestor(1), ...
            if parts.len() >= 2 && HID_METHODS.contains(&parts.last().unwrap().to_uppercase().as_str()) {
                let method = parts.pop().unwrap();
                return self.hid_call(Some(Expr::Col(parts)), &method);
            }
            // column.value('/path', 'type'), column.exist(...), column.query(...)
            if parts.len() >= 2 && matches!(parts.last().unwrap().to_uppercase().as_str(), "VALUE" | "EXIST" | "QUERY") {
                let method = parts.pop().unwrap();
                let base = Expr::Col(parts);
                return self.xml_method_named(base, &method);
            }
            return self.func_call(parts);
        }
        Ok(Expr::Col(parts))
    }

    /// A method of a hierarchyid: `base.Method(args)`, or, with no base, the static `hierarchyid::Method(args)`; the cursor is on
    /// the `(`. It is the call `HID_METHOD(base, args...)` (`HID_S_METHOD(args...)` for a static one).
    fn hid_call(&mut self, base: Option<Expr>, method: &str) -> R<Expr> {
        self.expect_op("(")?;
        let mut args: Vec<Expr> = base.into_iter().collect();
        let is_static = args.is_empty();
        if !self.is_op(")") {
            args.push(self.expr()?);
            while self.accept_op(",") {
                args.push(self.expr()?);
            }
        }
        self.expect_op(")")?;
        let name = format!("{}{}", if is_static { "HID_S_" } else { "HID_" }, method.to_uppercase());
        Ok(Expr::Func(Box::new(FuncCall { name, schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None })))
    }

    /// `.value(path, type)` / `.exist(path)` / `.query(path)` on an xml expression; the cursor is on the method name.
    fn xml_method(&mut self, base: Expr) -> R<Expr> {
        let method = self.ident()?;
        self.xml_method_named(base, &method)
    }

    /// The query of an xml method. When it is a string literal, each `sql:column("t.c")` and `sql:variable("@v")` in it is
    /// replaced by `sql:arg(n)` and the column or variable becomes the n-th extra argument of the call, so that it is evaluated
    /// like any other expression (see xmlpath.rs).
    fn xml_query_args(&self, path: Expr) -> (Expr, Vec<Expr>) {
        match path {
            Expr::Lit(Value::Str(t)) => {
                let (t, extra) = Self::bind_sql_args(&t);
                (Expr::Lit(Value::Str(t)), extra)
            }
            // N'...' is a cast of a literal
            Expr::Cast { e, ty, try_, style } if matches!(*e, Expr::Lit(Value::Str(_))) => {
                let Expr::Lit(Value::Str(t)) = *e else { unreachable!() };
                let (t, extra) = Self::bind_sql_args(&t);
                (Expr::Cast { e: Box::new(Expr::Lit(Value::Str(t))), ty, try_, style }, extra)
            }
            other => (other, vec![]),
        }
    }

    fn bind_sql_args(text: &str) -> (String, Vec<Expr>) {
        let lower = text.to_lowercase();
        if !lower.contains("sql:column") && !lower.contains("sql:variable") {
            return (text.to_string(), vec![]);
        }
        let mut out = String::new();
        let mut extra: Vec<Expr> = vec![];
        let mut i = 0;
        while i < text.len() {
            let rest = &lower[i..];
            let (kind, len) = if rest.starts_with("sql:column") {
                (1, 10)
            } else if rest.starts_with("sql:variable") {
                (2, 12)
            } else {
                let c = text[i..].chars().next().unwrap();
                out.push(c);
                i += c.len_utf8();
                continue;
            };
            // sql:column ( "name" )
            let after = text[i + len..].trim_start();
            let skipped = text.len() - i - len - after.len();
            let parsed = after.strip_prefix('(').map(|r| r.trim_start()).and_then(|r| {
                let q = r.chars().next().filter(|c| *c == '"' || *c == '\'')?;
                let end = r[1..].find(q)? + 1;
                let name = r[1..end].to_string();
                let tail = r[end + 1..].trim_start().strip_prefix(')')?;
                Some((name, text.len() - tail.len()))
            });
            match parsed {
                Some((name, end)) => {
                    let e = if kind == 2 {
                        Expr::Var(format!("@{}", name.trim_start_matches('@')).to_lowercase())
                    } else {
                        Expr::Col(name.split('.').map(|p| p.trim().trim_matches(|c| c == '[' || c == ']' || c == '"').to_string()).collect())
                    };
                    let _ = skipped;
                    extra.push(e);
                    out.push_str(&format!("sql:arg({})", extra.len() - 1));
                    i = end;
                }
                None => {
                    out.push_str(&text[i..i + len]);
                    i += len;
                }
            }
        }
        (out, extra)
    }

    /// `.modify('...')` after `col` or `@var`; the cursor is after the `(`'s method name. Returns the call
    /// `XML_MODIFY(base, text, args...)`, which is the new value of the xml.
    fn xml_modify_call(&mut self, base: Expr) -> R<Expr> {
        self.expect_op("(")?;
        let stmt = self.expr()?;
        self.expect_op(")")?;
        let (stmt, extra) = self.xml_query_args(stmt);
        let mut args = vec![base, stmt];
        args.extend(extra);
        Ok(Expr::Func(Box::new(FuncCall { name: "XML_MODIFY".into(), schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None })))
    }

    fn xml_method_named(&mut self, base: Expr, method: &str) -> R<Expr> {
        self.expect_op("(")?;
        let path = self.expr()?;
        let (path, extra) = self.xml_query_args(path);
        let m = method.to_uppercase();
        if m == "VALUE" {
            self.expect_op(",")?;
            let ty = match self.expr()? {
                Expr::Lit(Value::Str(s)) => s,
                _ => return Err(SqlError::new(9319, "The second argument of the xml data type method \"value\" must be a string literal.")),
            };
            self.expect_op(")")?;
            let decl = parse_param_decls(&format!("@x {}", ty.trim()))?;
            let p = decl.into_iter().next().ok_or_else(|| self.err())?;
            let mut args = vec![base, path];
            args.extend(extra);
            let call = Expr::Func(Box::new(FuncCall { name: "XML_VALUE".into(), schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None }));
            return Ok(Expr::Cast { e: Box::new(call), ty: p.ty, try_: false, style: None });
        }
        self.expect_op(")")?;
        let mut args = vec![base, path];
        args.extend(extra);
        Ok(Expr::Func(Box::new(FuncCall { name: format!("XML_{}", m), schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None })))
    }

    /// The column list of a full-text predicate or table function: `*`, a column, or `(c1, c2, ...)`. Empty for `*`.
    fn ft_columns(&mut self) -> R<Vec<Expr>> {
        if self.accept_op("*") {
            return Ok(vec![]);
        }
        let mut cols = vec![];
        if self.accept_op("(") {
            loop {
                cols.push(Expr::Col(self.name_parts()?));
                if !self.accept_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        } else {
            cols.push(Expr::Col(self.name_parts()?));
        }
        Ok(cols)
    }

    /// `CONTAINS(cols, 'condition' [, LANGUAGE n])` and `FREETEXT(cols, 'text' [, LANGUAGE n])`, as the call
    /// `FT_CONTAINS(n, col1, ..., coln, condition)` where n is the number of columns (0 for `*`).
    fn fulltext_pred(&mut self, name: &str) -> R<Expr> {
        self.expect_op("(")?;
        let cols = self.ft_columns()?;
        self.expect_op(",")?;
        let cond = self.expr()?;
        if self.accept_op(",") {
            self.expect_kw("LANGUAGE")?;
            self.expr()?;
        }
        self.expect_op(")")?;
        let mut args = vec![Expr::Lit(Value::Int(cols.len() as i32))];
        args.extend(cols);
        args.push(cond);
        Ok(Expr::Func(Box::new(FuncCall { name: format!("FT_{}", name), schema: None, parts: vec![], args, distinct: false, star: false, over: None, within: None })))
    }

    /// `CONTAINSTABLE(table, cols, 'condition' [, LANGUAGE n] [, top])` and `FREETEXTTABLE(...)`: the table and the columns
    /// are passed as text (names separated by a control character), then the condition and the number of rows (or NULL).
    fn fulltext_table(&mut self, name: &str) -> R<TableRef> {
        self.expect_op("(")?;
        let table = self.name_parts()?;
        self.expect_op(",")?;
        let cols: Vec<String> = self.ft_columns()?.into_iter().filter_map(|c| if let Expr::Col(p) = c { p.last().cloned() } else { None }).collect();
        self.expect_op(",")?;
        let cond = self.expr()?;
        let mut top = Expr::Lit(Value::Null);
        while self.accept_op(",") {
            if self.accept_kw("LANGUAGE") {
                self.expr()?;
            } else {
                top = self.expr()?;
            }
        }
        self.expect_op(")")?;
        let (alias, cols_alias) = self.opt_alias()?;
        let args = vec![Expr::Lit(Value::Str(table.join("\u{1}"))), Expr::Lit(Value::Str(cols.join("\u{1}"))), cond, top];
        Ok(TableRef::Func { name: vec![format!("FT_{}", name)], args, alias, cols: cols_alias, with: None })
    }

    fn func_call(&mut self, parts: Vec<String>) -> R<Expr> {
        let name = parts.last().unwrap().to_uppercase();
        if parts.len() == 1 && (name == "CONTAINS" || name == "FREETEXT") && self.is_op("(") {
            return self.fulltext_pred(&name);
        }
        let schema = if parts.len() > 1 { Some(parts[parts.len() - 2].clone()) } else { None };
        self.expect_op("(")?;
        let mut args = vec![];
        let mut distinct = false;
        let mut star = false;
        if !self.is_op(")") {
            if self.accept_kw("DISTINCT") {
                distinct = true;
            } else if self.is_kw("ALL") && !self.peek_is_op(1, ",") && !self.peek_is_op(1, ")") {
                self.i += 1;
            }
            if self.is_op("*") {
                self.i += 1;
                star = true;
            } else {
                let mut first = true;
                loop {
                    if first && DATEPART_FUNCS.contains(&name.as_str()) && matches!(self.tok().tok, Tok::Id(_) | Tok::QId(_)) && self.peek_is_op(1, ",") {
                        let t = self.advance();
                        let s = match t.tok {
                            Tok::Id(s) | Tok::QId(s) => s,
                            _ => unreachable!(),
                        };
                        args.push(Expr::Lit(Value::Str(s.to_lowercase())));
                    } else {
                        args.push(self.expr()?);
                        if name == "JSON_OBJECT" && self.accept_op(":") {
                            args.push(self.expr()?);
                        }
                    }
                    first = false;
                    if !self.accept_op(",") {
                        break;
                    }
                }
            }
        }
        self.skip_nulls_opt();
        self.expect_op(")")?;
        let mut within = None;
        if self.is_kw("WITHIN") && self.peek_kw(1, &["GROUP"]) {
            self.i += 2;
            self.expect_op("(")?;
            self.expect_kw("ORDER")?;
            self.expect_kw("BY")?;
            within = Some(self.order_items()?);
            self.expect_op(")")?;
        }
        self.skip_nulls_opt();
        let mut over = None;
        if self.is_kw("OVER") && self.peek_is_op(1, "(") {
            self.i += 1;
            over = Some(self.over_clause()?);
        }
        Ok(Expr::Func(Box::new(FuncCall { name, schema, parts, args, distinct, star, over, within })))
    }

    fn skip_nulls_opt(&mut self) {
        if self.is_kw_any(&["IGNORE", "RESPECT"]) && self.peek_kw(1, &["NULLS"]) {
            self.i += 2;
        }
    }

    fn over_clause(&mut self) -> R<Over> {
        self.expect_op("(")?;
        let mut partition = vec![];
        let mut order = vec![];
        let mut frame = None;
        if self.accept_kw("PARTITION") {
            self.expect_kw("BY")?;
            partition.push(self.expr()?);
            while self.accept_op(",") {
                partition.push(self.expr()?);
            }
        }
        if self.accept_kw("ORDER") {
            self.expect_kw("BY")?;
            order = self.order_items()?;
        }
        if self.is_kw_any(&["ROWS", "RANGE"]) {
            let rows = self.tok().up == "ROWS";
            self.i += 1;
            let (start, end);
            if self.accept_kw("BETWEEN") {
                start = self.frame_bound()?;
                self.expect_kw("AND")?;
                end = self.frame_bound()?;
            } else {
                start = self.frame_bound()?;
                end = Bound::CurrentRow;
            }
            // SQL Server (through 2022) only has RANGE with UNBOUNDED and CURRENT ROW bounds
            if !rows && matches!(start, Bound::Preceding(_) | Bound::Following(_)) || !rows && matches!(end, Bound::Preceding(_) | Bound::Following(_)) {
                return Err(SqlError::new(4194, "RANGE is only supported with UNBOUNDED and CURRENT ROW window frame delimiters."));
            }
            frame = Some(Frame { rows, start, end });
        }
        self.expect_op(")")?;
        Ok(Over { partition, order, frame })
    }

    fn frame_bound(&mut self) -> R<Bound> {
        if self.accept_kw("UNBOUNDED") {
            let d = self.advance_up();
            return Ok(if d == "PRECEDING" { Bound::UnboundedPreceding } else { Bound::UnboundedFollowing });
        }
        if self.accept_kw("CURRENT") {
            self.expect_kw("ROW")?;
            return Ok(Bound::CurrentRow);
        }
        let e = self.expr()?;
        let d = self.advance_up();
        Ok(if d == "PRECEDING" { Bound::Preceding(Box::new(e)) } else { Bound::Following(Box::new(e)) })
    }

    fn order_items(&mut self) -> R<Vec<OrderItem>> {
        let mut items = vec![];
        loop {
            let e = self.expr()?;
            let desc = if self.accept_kw("DESC") {
                true
            } else {
                self.accept_kw("ASC");
                false
            };
            items.push(OrderItem { e, desc });
            if !self.accept_op(",") {
                break;
            }
        }
        Ok(items)
    }

    // ---------------------------------------------------------------- queries
    pub fn query(&mut self) -> R<Query> {
        self.depth += 1;
        if self.depth > MAX_NEST {
            return Err(complexity_error());
        }
        let q = self.query_inner();
        self.depth -= 1;
        q
    }

    fn query_inner(&mut self) -> R<Query> {
        let ctes = if self.is_kw("WITH") { self.cte_list()? } else { vec![] };
        let body = self.query_expr()?;
        let mut order = vec![];
        let (mut offset, mut fetch) = (None, None);
        if self.is_kw("ORDER") && self.peek_kw(1, &["BY"]) {
            self.i += 2;
            order = self.order_items()?;
            if self.accept_kw("OFFSET") {
                offset = Some(self.expr()?);
                if !(self.accept_kw("ROWS") || self.accept_kw("ROW")) {
                    return Err(self.err());
                }
                if self.accept_kw("FETCH") {
                    if !(self.accept_kw("NEXT") || self.accept_kw("FIRST")) {
                        return Err(self.err());
                    }
                    fetch = Some(self.expr()?);
                    if !(self.accept_kw("ROWS") || self.accept_kw("ROW")) {
                        return Err(self.err());
                    }
                    self.expect_kw("ONLY")?;
                }
            }
        }
        let mut for_clause = None;
        if self.is_kw("FOR") && self.peek_kw(1, &["XML", "JSON"]) {
            for_clause = Some(self.for_clause()?);
        }
        let mut maxrecursion = None;
        let mut options: Vec<String> = vec![];
        if self.is_kw("OPTION") && self.peek_is_op(1, "(") {
            self.i += 2;
            let mut depth = 1;
            let mut cur = String::new();
            while depth > 0 && !self.is_eof() {
                let t = self.advance();
                match t.tok {
                    Tok::Op("(") => depth += 1,
                    Tok::Op(")") => depth -= 1,
                    Tok::Op(",") if depth == 1 => {
                        if !cur.is_empty() {
                            options.push(std::mem::take(&mut cur));
                        }
                    }
                    Tok::Id(_) if t.up == "MAXRECURSION" => {
                        if let Tok::Int(n) = self.tok().tok {
                            maxrecursion = Some(n as i32);
                        }
                        cur.push_str("MAXRECURSION");
                    }
                    Tok::Id(_) | Tok::Int(_) if depth == 1 => {
                        if !cur.is_empty() {
                            cur.push(' ');
                        }
                        match t.tok {
                            // (`MAXDOP 4`: the number is part of the hint)
                            Tok::Int(n) => cur.push_str(&n.to_string()),
                            _ => cur.push_str(&t.up),
                        }
                    }
                    _ => {}
                }
            }
            if !cur.is_empty() {
                options.push(cur);
            }
        }
        Ok(Query { ctes, body, order, offset, fetch, for_clause, maxrecursion, options })
    }

    fn for_clause(&mut self) -> R<ForClause> {
        self.i += 1;
        let json = self.advance_up() == "JSON";
        let mut fc = ForClause { json, mode: String::new(), name: None, root: None, without_array_wrapper: false, include_null: false, xml_type: false };
        if self.is_kw_any(&["RAW", "AUTO", "PATH", "EXPLICIT"]) {
            fc.mode = self.advance_up();
            if self.accept_op("(") {
                if let Some(s) = self.str_tok() {
                    fc.name = Some(s);
                    self.i += 1;
                }
                self.expect_op(")")?;
            }
        }
        while self.accept_op(",") {
            let w = self.advance_up();
            match w.as_str() {
                "ROOT" => {
                    let mut r = "root".to_string();
                    if self.accept_op("(") {
                        if let Some(s) = self.str_tok() {
                            r = s;
                            self.i += 1;
                        }
                        self.expect_op(")")?;
                    }
                    fc.root = Some(r);
                }
                "WITHOUT_ARRAY_WRAPPER" => fc.without_array_wrapper = true,
                "INCLUDE_NULL_VALUES" => fc.include_null = true,
                "TYPE" => fc.xml_type = true,
                _ => {
                    if self.accept_op("(") {
                        while !self.is_op(")") && !self.is_eof() {
                            self.i += 1;
                        }
                        self.expect_op(")")?;
                    }
                }
            }
        }
        Ok(fc)
    }

    fn cte_list(&mut self) -> R<Vec<Cte>> {
        self.expect_kw("WITH")?;
        let mut ctes = vec![];
        loop {
            let name = self.ident()?;
            let mut cols = None;
            if self.accept_op("(") {
                let mut c = vec![self.ident()?];
                while self.accept_op(",") {
                    c.push(self.ident()?);
                }
                self.expect_op(")")?;
                cols = Some(c);
            }
            self.expect_kw("AS")?;
            self.expect_op("(")?;
            let q = self.query()?;
            self.expect_op(")")?;
            ctes.push(Cte { name, cols, query: Rc::new(q) });
            if !self.accept_op(",") {
                break;
            }
        }
        Ok(ctes)
    }

    fn query_expr(&mut self) -> R<QueryBody> {
        let mut left = self.query_term()?;
        while self.is_kw_any(&["UNION", "EXCEPT"]) {
            let op = if self.advance_up() == "UNION" { SetOp::Union } else { SetOp::Except };
            let all = self.accept_kw("ALL");
            let right = self.query_term()?;
            left = QueryBody::SetOp { op, all, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }
    fn query_term(&mut self) -> R<QueryBody> {
        let mut left = self.query_primary()?;
        while self.is_kw("INTERSECT") {
            self.i += 1;
            let all = self.accept_kw("ALL");
            let right = self.query_primary()?;
            left = QueryBody::SetOp { op: SetOp::Intersect, all, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }
    fn query_primary(&mut self) -> R<QueryBody> {
        if self.is_op("(") {
            self.i += 1;
            let q = self.query()?;
            self.expect_op(")")?;
            return Ok(QueryBody::Paren(Box::new(q)));
        }
        Ok(QueryBody::Select(Box::new(self.select_core()?)))
    }

    fn select_core(&mut self) -> R<Select> {
        if !self.is_kw("SELECT") {
            return Err(self.err());
        }
        self.i += 1;
        let distinct = if self.accept_kw("DISTINCT") {
            true
        } else {
            self.accept_kw("ALL");
            false
        };
        let mut top = None;
        if self.accept_kw("TOP") {
            let e = if self.accept_op("(") {
                let e = self.expr()?;
                self.expect_op(")")?;
                e
            } else {
                self.primary()?
            };
            let percent = self.accept_kw("PERCENT");
            let mut ties = false;
            if self.is_kw("WITH") && self.peek_kw(1, &["TIES"]) {
                self.i += 2;
                ties = true;
            }
            top = Some(Top { e, percent, ties });
        }
        let mut items = vec![self.select_item()?];
        while self.accept_op(",") {
            items.push(self.select_item()?);
        }
        let into = if self.accept_kw("INTO") { Some(self.name_parts()?) } else { None };
        let mut from = vec![];
        if self.accept_kw("FROM") {
            from.push(self.table_source()?);
            while self.accept_op(",") {
                from.push(self.table_source()?);
            }
        }
        let where_ = if self.accept_kw("WHERE") { Some(self.expr()?.factor_common()) } else { None };
        let mut group = None;
        if self.is_kw("GROUP") && self.peek_kw(1, &["BY"]) {
            self.i += 2;
            let mut g = self.group_items()?;
            if self.is_kw("WITH") && self.peek_kw(1, &["ROLLUP", "CUBE"]) {
                self.i += 1;
                let kind = self.advance_up();
                let exprs: Vec<Expr> = g
                    .into_iter()
                    .filter_map(|x| if let GroupItem::Expr(e) = x { Some(e) } else { None })
                    .collect();
                g = vec![if kind == "ROLLUP" { GroupItem::Rollup(exprs) } else { GroupItem::Cube(exprs) }];
            }
            group = Some(g);
        }
        let having = if self.accept_kw("HAVING") { Some(self.expr()?) } else { None };
        Ok(Select { distinct, top, items, into, from, where_, group, having })
    }

    fn group_items(&mut self) -> R<Vec<GroupItem>> {
        let mut items = vec![];
        loop {
            if self.is_kw_any(&["ROLLUP", "CUBE"]) && self.peek_is_op(1, "(") {
                let rollup = self.advance_up() == "ROLLUP";
                self.expect_op("(")?;
                let mut a = vec![self.expr()?];
                while self.accept_op(",") {
                    a.push(self.expr()?);
                }
                self.expect_op(")")?;
                items.push(if rollup { GroupItem::Rollup(a) } else { GroupItem::Cube(a) });
            } else if self.is_kw("GROUPING") && self.peek_kw(1, &["SETS"]) {
                self.i += 2;
                self.expect_op("(")?;
                let mut sets = vec![];
                loop {
                    if self.accept_op("(") {
                        let mut s = vec![];
                        if !self.is_op(")") {
                            s.push(self.expr()?);
                            while self.accept_op(",") {
                                s.push(self.expr()?);
                            }
                        }
                        self.expect_op(")")?;
                        sets.push(s);
                    } else {
                        sets.push(vec![self.expr()?]);
                    }
                    if !self.accept_op(",") {
                        break;
                    }
                }
                self.expect_op(")")?;
                items.push(GroupItem::Sets(sets));
            } else {
                items.push(GroupItem::Expr(self.expr()?));
            }
            if !self.accept_op(",") {
                break;
            }
        }
        Ok(items)
    }

    fn alias_ok(&self) -> bool {
        match &self.tok().tok {
            Tok::QId(_) => true,
            Tok::Id(_) => {
                let u = self.tok().up.as_str();
                !reserved().contains(u) && !["PIVOT", "UNPIVOT", "OFFSET", "GO"].contains(&u)
            }
            _ => false,
        }
    }

    fn select_item(&mut self) -> R<SelectItem> {
        let toks = self.toks.clone();
        let t = &toks[self.i];
        let n1 = self.peek(1).clone();
        let n1_eq = matches!(n1.tok, Tok::Op("="));
        let is_nonreserved_id = |t: &Token| match &t.tok {
            Tok::Id(_) => !reserved().contains(t.up.as_str()),
            Tok::QId(_) | Tok::Str(_) => true,
            _ => false,
        };
        if is_nonreserved_id(&t) && n1_eq {
            let alias = match &t.tok {
                Tok::Id(s) | Tok::QId(s) | Tok::Str(s) => s.clone(),
                _ => unreachable!(),
            };
            self.i += 2;
            let e = self.expr()?;
            return Ok(SelectItem::Expr { e, alias: Some(alias) });
        }
        if let Tok::Var(v) = &t.tok {
            if let Tok::Op(o) = n1.tok {
                let op = match o {
                    "=" => Some(None),
                    "+=" => Some(Some(BinOp::Add)),
                    "-=" => Some(Some(BinOp::Sub)),
                    "*=" => Some(Some(BinOp::Mul)),
                    "/=" => Some(Some(BinOp::Div)),
                    "%=" => Some(Some(BinOp::Mod)),
                    "&=" => Some(Some(BinOp::BitAnd)),
                    "|=" => Some(Some(BinOp::BitOr)),
                    "^=" => Some(Some(BinOp::BitXor)),
                    _ => None,
                };
                if let Some(op) = op {
                    self.i += 2;
                    let e = self.expr()?;
                    return Ok(SelectItem::Assign { var: v.to_lowercase(), op, e });
                }
            }
        }
        if self.is_op("*") {
            self.i += 1;
            return Ok(SelectItem::Star(None));
        }
        let e = self.expr()?;
        if let Expr::Star(q) = e {
            return Ok(SelectItem::Star(q));
        }
        let mut alias = None;
        if self.accept_kw("AS") {
            let tk = self.advance();
            match tk.tok {
                Tok::Id(s) | Tok::QId(s) | Tok::Str(s) => alias = Some(s),
                _ => return Err(self.err_at(&tk)),
            }
        } else if self.alias_ok() {
            alias = Some(self.ident()?);
        } else if let Some(s) = self.str_tok() {
            self.i += 1;
            alias = Some(s);
        }
        Ok(SelectItem::Expr { e, alias })
    }

    // ------------------------------------------------------------ FROM clause
    fn table_source(&mut self) -> R<TableRef> {
        let mut left = self.table_primary()?;
        loop {
            let kind;
            if self.is_kw("INNER") {
                self.i += 1;
                self.skip_join_hint();
                self.expect_kw("JOIN")?;
                kind = JoinKind::Inner;
            } else if self.is_kw("JOIN") {
                self.i += 1;
                kind = JoinKind::Inner;
            } else if self.is_kw_any(&["LEFT", "RIGHT", "FULL"]) {
                kind = match self.advance_up().as_str() {
                    "LEFT" => JoinKind::Left,
                    "RIGHT" => JoinKind::Right,
                    _ => JoinKind::Full,
                };
                self.accept_kw("OUTER");
                self.skip_join_hint();
                self.expect_kw("JOIN")?;
            } else if self.is_kw("CROSS") {
                self.i += 1;
                if self.accept_kw("APPLY") {
                    kind = JoinKind::CrossApply;
                } else {
                    self.expect_kw("JOIN")?;
                    kind = JoinKind::Cross;
                }
            } else if self.is_kw("OUTER") && self.peek_kw(1, &["APPLY"]) {
                self.i += 2;
                kind = JoinKind::OuterApply;
            } else {
                break;
            }
            let right = self.table_primary()?;
            let on = if matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full) {
                self.expect_kw("ON")?;
                Some(self.expr()?.factor_common())
            } else {
                None
            };
            left = TableRef::Join { kind, left: Box::new(left), right: Box::new(right), on };
        }
        Ok(left)
    }

    fn skip_join_hint(&mut self) {
        if self.is_kw_any(&["LOOP", "HASH", "MERGE", "REMOTE"]) && self.peek_kw(1, &["JOIN"]) {
            self.i += 1;
        }
    }

    fn opt_alias(&mut self) -> R<(Option<String>, Option<Vec<String>>)> {
        let mut alias = None;
        if self.accept_kw("AS") {
            alias = Some(self.ident()?);
        } else if self.alias_ok() {
            alias = Some(self.ident()?);
        }
        let mut cols = None;
        if alias.is_some() && self.is_op("(") {
            self.i += 1;
            let mut c = vec![self.ident()?];
            while self.accept_op(",") {
                c.push(self.ident()?);
            }
            self.expect_op(")")?;
            cols = Some(c);
        }
        Ok((alias, cols))
    }

    fn skip_parens(&mut self) -> R<()> {
        self.expect_op("(")?;
        let mut depth = 1;
        while depth > 0 {
            if self.is_eof() {
                return Err(self.err());
            }
            match self.advance().tok {
                Tok::Op("(") => depth += 1,
                Tok::Op(")") => depth -= 1,
                _ => {}
            }
        }
        Ok(())
    }

    /// `.nodes('path') [AS] alias(column)` after the xml expression; the cursor is on the `(`.
    fn xml_nodes(&mut self, base: Expr) -> R<TableRef> {
        self.expect_op("(")?;
        let path = self.expr()?;
        self.expect_op(")")?;
        let (path, extra) = self.xml_query_args(path);
        let (alias, cols) = self.opt_alias()?;
        let mut args = vec![base, path];
        args.extend(extra);
        Ok(TableRef::Func { name: vec!["XML_NODES".into()], args, alias, cols, with: None })
    }

    fn table_primary(&mut self) -> R<TableRef> {
        let mut node;
        if self.is_op("(") {
            self.i += 1;
            // a derived table may start with parenthesised queries: ((SELECT ...) EXCEPT (SELECT ...)) AS x
            let mut depth = 0;
            while self.peek_is_op(depth, "(") {
                depth += 1;
            }
            if self.is_kw_any(&["SELECT", "WITH"]) || (depth > 0 && self.peek_kw(depth, &["SELECT", "WITH"])) {
                let q = self.query()?;
                self.expect_op(")")?;
                let (alias, cols) = self.opt_alias()?;
                node = TableRef::Derived { query: Box::new(q), alias, cols };
            } else if self.is_kw("VALUES") {
                let rows = self.values_rows()?;
                self.expect_op(")")?;
                let (alias, cols) = self.opt_alias()?;
                node = TableRef::Values { rows, alias, cols };
            } else {
                node = self.table_source()?;
                self.expect_op(")")?;
            }
        } else if let Some(v) = self.var_tok() {
            self.i += 1;
            if self.is_op(".") && self.peek_kw(1, &["NODES"]) {
                self.i += 2;
                node = self.xml_nodes(Expr::Var(v.to_lowercase()))?;
            } else {
                let (alias, _) = self.opt_alias()?;
                node = TableRef::TableVar { name: v.to_lowercase(), alias };
            }
        } else {
            let name = self.name_parts()?;
            if name.len() >= 2 && name.last().unwrap().eq_ignore_ascii_case("nodes") && self.is_op("(") {
                // col.nodes('path'), alias.col.nodes('path')
                let col = Expr::Col(name[..name.len() - 1].to_vec());
                node = self.xml_nodes(col)?;
            } else if name.len() == 1 && (name[0].eq_ignore_ascii_case("CONTAINSTABLE") || name[0].eq_ignore_ascii_case("FREETEXTTABLE")) && self.is_op("(") {
                node = self.fulltext_table(&name[0].to_uppercase())?;
            } else if self.is_op("(") {
                self.i += 1;
                let mut args = vec![];
                if !self.is_op(")") {
                    args.push(self.expr()?);
                    while self.accept_op(",") {
                        args.push(self.expr()?);
                    }
                }
                self.expect_op(")")?;
                let mut with = None;
                // OPENXML(h, pattern [, flags]) WITH tablename: the table is passed as a fourth argument
                if self.is_kw("WITH") && !self.peek_is_op(1, "(") && name.last().is_some_and(|n| n.eq_ignore_ascii_case("OPENXML")) {
                    self.i += 1;
                    let table = self.name_parts()?;
                    while args.len() < 3 {
                        args.push(Expr::Lit(Value::Int(0)));
                    }
                    args.push(Expr::Lit(Value::Str(table.join("\u{1}"))));
                }
                if self.is_kw("WITH") && self.peek_is_op(1, "(") && name.last().map(|n| n.eq_ignore_ascii_case("OPENJSON") || n.eq_ignore_ascii_case("OPENXML")).unwrap_or(false) {
                    self.i += 2;
                    let mut w = vec![];
                    loop {
                        let cn = self.ident()?;
                        let ty = self.parse_type()?;
                        let path = if let Some(s) = self.str_tok() {
                            self.i += 1;
                            Some(s)
                        } else {
                            None
                        };
                        if self.accept_kw("AS") {
                            self.expect_kw("JSON")?;
                        }
                        w.push((cn, ty, path));
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    with = Some(w);
                }
                let (alias, cols) = self.opt_alias()?;
                node = TableRef::Func { name, args, alias, cols, with };
            } else {
                let mut clause = None;
                if self.is_kw("FOR") && self.peek_kw(1, &["SYSTEM_TIME"]) {
                    clause = Some(self.temporal_clause()?);
                }
                let (alias, _) = self.opt_alias()?;
                if let Some(c) = clause {
                    node = TableRef::Temporal { name, alias, clause: Box::new(c) };
                } else {
                let mut hints = vec![];
                if self.is_kw("WITH") && self.peek_is_op(1, "(") {
                    self.i += 2;
                    // table hints: names, some with arguments (INDEX(ix), FORCESEEK(ix(col)))
                    let mut depth = 1;
                    while depth > 0 {
                        if self.is_eof() {
                            return Err(self.err());
                        }
                        let t = self.advance();
                        match t.tok {
                            Tok::Op("(") => depth += 1,
                            Tok::Op(")") => depth -= 1,
                            Tok::Id(_) if depth == 1 => hints.push(t.up.clone()),
                            _ => {}
                        }
                    }
                } else if self.accept_kw("TABLESAMPLE") && self.is_op("(") {
                    self.skip_parens()?;
                }
                node = TableRef::Table { name, alias, hints };
                }
            }
        }
        if self.is_kw("PIVOT") && self.peek_is_op(1, "(") {
            self.i += 2;
            let agg = self.ident()?.to_uppercase();
            self.expect_op("(")?;
            let arg = self.expr()?;
            self.expect_op(")")?;
            self.expect_kw("FOR")?;
            let col = self.name_parts()?;
            self.expect_kw("IN")?;
            self.expect_op("(")?;
            let mut vals = vec![self.ident()?];
            while self.accept_op(",") {
                vals.push(self.ident()?);
            }
            self.expect_op(")")?;
            self.expect_op(")")?;
            let (alias, _) = self.opt_alias()?;
            node = TableRef::Pivot { src: Box::new(node), agg, arg, col, vals, alias };
        } else if self.is_kw("UNPIVOT") && self.peek_is_op(1, "(") {
            self.i += 2;
            let vcol = self.ident()?;
            self.expect_kw("FOR")?;
            let ncol = self.ident()?;
            self.expect_kw("IN")?;
            self.expect_op("(")?;
            let mut cols = vec![self.ident()?];
            while self.accept_op(",") {
                cols.push(self.ident()?);
            }
            self.expect_op(")")?;
            self.expect_op(")")?;
            let (alias, _) = self.opt_alias()?;
            node = TableRef::Unpivot { src: Box::new(node), vcol, ncol, cols, alias };
        }
        Ok(node)
    }

    fn values_rows(&mut self) -> R<Vec<Vec<Expr>>> {
        self.expect_kw("VALUES")?;
        let mut rows = vec![];
        loop {
            self.expect_op("(")?;
            let mut row = vec![self.expr()?];
            while self.accept_op(",") {
                row.push(self.expr()?);
            }
            self.expect_op(")")?;
            rows.push(row);
            if !self.accept_op(",") {
                break;
            }
        }
        Ok(rows)
    }

    // ------------------------------------------------------------- statements
    pub fn parse_batch(&mut self) -> R<Vec<Stmt>> {
        let mut stmts = vec![];
        while !self.is_eof() {
            if self.accept_op(";") {
                continue;
            }
            let start = self.i;
            match self.statement() {
                Ok(s) => stmts.push(s),
                // the first statement of a batch may call a procedure without EXEC: `sp_help 'tbl'`
                Err(e) if e.number == 102 && self.i == 0 && stmts.is_empty() && start == 0 && matches!(self.toks[0].tok, Tok::Id(_)) && !self.peek_is_op(1, "=") && !self.peek_is_op(1, "(") => {
                    self.i = 0;
                    match self.st_exec_body() {
                        Ok(s) => {
                            self.accept_op(";");
                            stmts.push(s)
                        }
                        Err(_) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> R<Stmt> {
        let s = self.stmt_inner()?;
        self.accept_op(";");
        Ok(s)
    }

    fn stmt_inner(&mut self) -> R<Stmt> {
        let toks = self.toks.clone();
        let t = &toks[self.i];
        match &t.tok {
            Tok::Id(_) => {
                match t.up.as_str() {
                    "SELECT" => Ok(Stmt::Query(Box::new(self.query()?))),
                    "WITH" => self.st_with(),
                    "INSERT" => self.st_insert(Vec::new()),
                    "UPDATE" => self.st_update(Vec::new()),
                    "DELETE" => self.st_delete(Vec::new()),
                    "MERGE" => self.st_merge(Vec::new()),
                    "TRUNCATE" => {
                        self.i += 1;
                        self.expect_kw("TABLE")?;
                        Ok(Stmt::Truncate(self.target()?))
                    }
                    "DECLARE" => self.st_declare(),
                    "SET" => self.st_set(),
                    "IF" => {
                        self.i += 1;
                        let cond = self.expr()?;
                        let then = Box::new(self.statement()?);
                        self.accept_op(";");
                        let els = if self.accept_kw("ELSE") { Some(Box::new(self.statement()?)) } else { None };
                        Ok(Stmt::If { cond, then, els })
                    }
                    "WHILE" => {
                        self.i += 1;
                        let cond = self.expr()?;
                        let body = Box::new(self.statement()?);
                        Ok(Stmt::While { cond, body })
                    }
                    "BEGIN" => self.st_begin(),
                    "BREAK" => {
                        self.i += 1;
                        Ok(Stmt::Break)
                    }
                    "CONTINUE" => {
                        self.i += 1;
                        Ok(Stmt::Continue)
                    }
                    "GOTO" => {
                        self.i += 1;
                        Ok(Stmt::Goto(self.ident()?.to_lowercase()))
                    }
                    "RETURN" => {
                        self.i += 1;
                        if self.at_eos() || (self.is_stmt_start() && !self.is_kw("USER")) {
                            Ok(Stmt::Return(None))
                        } else {
                            Ok(Stmt::Return(Some(self.expr()?)))
                        }
                    }
                    "PRINT" => {
                        self.i += 1;
                        Ok(Stmt::Print(self.expr()?))
                    }
                    "USE" => {
                        self.i += 1;
                        Ok(Stmt::Use(self.ident()?))
                    }
                    "COMMIT" | "ROLLBACK" | "SAVE" => {
                        let w = self.advance_up();
                        if !(self.accept_kw("TRAN") || self.accept_kw("TRANSACTION")) {
                            self.accept_kw("WORK");
                        }
                        let name = self.tran_name();
                        Ok(match w.as_str() {
                            "COMMIT" => Stmt::Commit(name),
                            "ROLLBACK" => Stmt::Rollback(name),
                            _ => Stmt::SaveTran(name.ok_or_else(|| self.err())?),
                        })
                    }
                    "THROW" => {
                        self.i += 1;
                        if self.at_eos() || self.is_stmt_start() {
                            return Ok(Stmt::Throw(None));
                        }
                        let a = self.expr()?;
                        self.expect_op(",")?;
                        let b = self.expr()?;
                        self.expect_op(",")?;
                        let c = self.expr()?;
                        Ok(Stmt::Throw(Some((a, b, c))))
                    }
                    "RAISERROR" => {
                        self.i += 1;
                        self.expect_op("(")?;
                        let mut args = vec![self.expr()?];
                        while self.accept_op(",") {
                            args.push(self.expr()?);
                        }
                        self.expect_op(")")?;
                        let mut opts = vec![];
                        if self.accept_kw("WITH") {
                            opts.push(self.advance_up());
                            while self.accept_op(",") {
                                opts.push(self.advance_up());
                            }
                        }
                        Ok(Stmt::RaiseError { args, opts })
                    }
                    "WAITFOR" => {
                        self.i += 1;
                        let time = if self.accept_kw("DELAY") {
                            false
                        } else if self.accept_kw("TIME") {
                            true
                        } else {
                            return Err(SqlError::new(102, "Incorrect syntax near WAITFOR: only WAITFOR DELAY and WAITFOR TIME are supported."));
                        };
                        let e = self.expr()?;
                        Ok(Stmt::WaitFor { time, e })
                    }
                    "KILL" => {
                        self.i += 1;
                        let e = self.expr()?;
                        if self.accept_kw("WITH") {
                            self.advance();
                        }
                        Ok(Stmt::Kill(e))
                    }
                    "RECONFIGURE" => {
                        self.i += 1;
                        if self.accept_kw("WITH") {
                            self.advance();
                        }
                        Ok(Stmt::Reconfigure)
                    }
                    "OPEN" | "CLOSE" | "DEALLOCATE" => {
                        let op = self.advance_up();
                        self.accept_kw("GLOBAL");
                        Ok(Stmt::CursorOp { op, name: self.ident()?.to_lowercase() })
                    }
                    "FETCH" => self.st_fetch(),
                    "EXEC" | "EXECUTE" if self.peek_kw(1, &["AS"]) && self.peek_kw(2, &["USER", "LOGIN"]) => {
                        self.i += 2;
                        let login = self.advance_up() == "LOGIN";
                        self.expect_op("=")?;
                        let name = match self.advance().tok {
                            Tok::Str(s) | Tok::Id(s) | Tok::QId(s) => s,
                            _ => return Err(self.err()),
                        };
                        if self.accept_kw("WITH") {
                            self.i += 1;
                            self.accept_kw("REVERT");
                            if self.accept_op("=") {
                                self.i += 1;
                            }
                        }
                        Ok(Stmt::Security(Box::new(SecStmt::ExecuteAs { login, name })))
                    }
                    "EXEC" | "EXECUTE" => self.st_exec(),
                    "DROP" => self.st_drop(),
                    "CREATE" => {
                        let start = t.pos;
                        self.i += 1;
                        let mut or_alter = false;
                        if self.is_kw("OR") && self.peek_kw(1, &["ALTER"]) {
                            self.i += 2;
                            or_alter = true;
                        }
                        self.create_object(or_alter, false, start)
                    }
                    "ALTER" => {
                        let start = t.pos;
                        self.i += 1;
                        if self.audit_ahead() {
                            return self.audit_stmt("ALTER");
                        }
                        if self.is_kw("TABLE") {
                            return self.alter_table();
                        }
                        if self.is_kw("SEQUENCE") {
                            self.i += 1;
                            let name = self.name_parts()?;
                            let opts = self.sequence_opts()?;
                            return Ok(Stmt::AlterSequence { name, opts });
                        }
                        if self.is_kw("SECURITY") && self.peek_kw(1, &["POLICY"]) {
                            self.i += 2;
                            return Ok(Stmt::AlterSecurityPolicy(Box::new(self.policy_body()?)));
                        }
                        if self.is_kw("DATABASE") && self.peek_kw(2, &["COLLATE"]) {
                            self.i += 1;
                            let db = self.ident()?;
                            self.expect_kw("COLLATE")?;
                            let name = self.ident()?;
                            return Ok(Stmt::AlterDatabase { db, options: vec![("COLLATE".to_string(), name)] });
                        }
                        if self.is_kw("DATABASE") && !self.peek_kw(2, &["MODIFY", "ADD", "REMOVE", "COLLATE"]) && self.peek_kw(2, &["SET"]) {
                            self.i += 1;
                            let db = self.ident()?;
                            self.expect_kw("SET")?;
                            let mut options = vec![];
                            loop {
                                let name = self.ident()?.to_uppercase();
                                let mut val = String::new();
                                if self.is_kw_any(&["ON", "OFF"]) {
                                    val = self.advance_up();
                                } else if self.accept_op("=") {
                                    val = self.advance_up();
                                } else if name == "RECOVERY" {
                                    val = self.advance_up();
                                }
                                options.push((name, val));
                                if !self.accept_op(",") {
                                    break;
                                }
                            }
                            if self.accept_kw("WITH") {
                                while !self.at_eos() && !self.is_stmt_start() {
                                    self.i += 1;
                                }
                            }
                            return Ok(Stmt::AlterDatabase { db, options });
                        }
                        if self.is_kw("LOGIN") {
                            self.i += 1;
                            let name = self.ident()?;
                            let (mut password, mut disabled, mut default_db) = (None, None, None);
                            let mut opts = LoginOpts::default();
                            if self.accept_kw("ENABLE") {
                                disabled = Some(false);
                            } else if self.accept_kw("DISABLE") {
                                disabled = Some(true);
                            } else if self.accept_kw("WITH") {
                                loop {
                                    self.login_option(&mut password, &mut default_db, &mut opts)?;
                                    // OLD_PASSWORD / UNLOCK / MUST_CHANGE may follow without a comma
                                    while self.is_kw_any(&["OLD_PASSWORD", "UNLOCK", "MUST_CHANGE"]) {
                                        self.login_option(&mut password, &mut default_db, &mut opts)?;
                                    }
                                    if !self.accept_op(",") {
                                        break;
                                    }
                                }
                            }
                            return Ok(Stmt::Security(Box::new(SecStmt::AlterLogin { name, password, disabled, default_db, opts })));
                        }
                        if self.is_kw("USER") {
                            self.i += 1;
                            let name = self.ident()?;
                            self.expect_kw("WITH")?;
                            let (mut default_schema, mut new_name) = (None, None);
                            loop {
                                let opt = self.ident()?.to_uppercase();
                                self.expect_op("=")?;
                                let v = self.advance();
                                let val = match v.tok {
                                    Tok::Str(s) | Tok::Id(s) | Tok::QId(s) => s,
                                    _ => String::new(),
                                };
                                match opt.as_str() {
                                    "DEFAULT_SCHEMA" => default_schema = Some(val),
                                    "NAME" => new_name = Some(val),
                                    _ => {}
                                }
                                if !self.accept_op(",") {
                                    break;
                                }
                            }
                            return Ok(Stmt::Security(Box::new(SecStmt::AlterUser { name, default_schema, new_name })));
                        }
                        if self.is_kw("ROLE") || (self.is_kw("SERVER") && self.peek_kw(1, &["ROLE"])) {
                            let server = self.is_kw("SERVER");
                            self.i += if server { 2 } else { 1 };
                            let role = self.ident()?;
                            let add = if self.accept_kw("ADD") {
                                true
                            } else {
                                self.expect_kw("DROP")?;
                                false
                            };
                            self.expect_kw("MEMBER")?;
                            let member = self.ident()?;
                            return Ok(Stmt::Security(Box::new(if server { SecStmt::AlterServerRole { role, add, member } } else { SecStmt::AlterRole { role, add, member } })));
                        }
                        if self.is_kw_any(&["DATABASE", "INDEX", "SERVER", "AUTHORIZATION", "SCHEMA"]) {
                            return self.skip_ignored();
                        }
                        self.create_object(true, true, start)
                    }
                    "DBCC" => {
                        self.i += 1;
                        let name = self.ident()?.to_uppercase();
                        let mut args = vec![];
                        if self.accept_op("(") {
                            if !self.is_op(")") {
                                loop {
                                    if matches!(self.tok().tok, Tok::Id(_)) && (self.peek_is_op(1, ",") || self.peek_is_op(1, ")")) && !reserved().contains(self.tok().up.as_str()) {
                                        let s = self.advance();
                                        if let Tok::Id(s) = s.tok {
                                            args.push(Expr::Lit(Value::Str(s)));
                                        }
                                    } else {
                                        args.push(self.expr()?);
                                    }
                                    if !self.accept_op(",") {
                                        break;
                                    }
                                }
                            }
                            self.expect_op(")")?;
                        }
                        if self.accept_kw("WITH") {
                            self.advance();
                            while self.accept_op(",") {
                                self.advance();
                            }
                        }
                        Ok(Stmt::Dbcc { name, args })
                    }
                    "GRANT" | "REVOKE" | "DENY" => self.perm_stmt(),
                    "REVERT" => {
                        self.i += 1;
                        Ok(Stmt::Security(Box::new(SecStmt::Revert)))
                    }
                    "CHECKPOINT" => {
                        self.i += 1;
                        Ok(Stmt::Checkpoint)
                    }
                    "ENABLE" => self.st_enable_trigger(true),
                    "DISABLE" => self.st_enable_trigger(false),
                    "BULK" => {
                        self.i += 1;
                        self.expect_kw("INSERT")?;
                        let table = self.name_parts()?;
                        self.expect_kw("FROM")?;
                        let path = self.expr()?;
                        let mut opts = vec![];
                        if self.accept_kw("WITH") {
                            let paren = self.accept_op("(");
                            loop {
                                let name = self.ident()?.to_uppercase();
                                let val = if self.accept_op("=") { Some(self.expr()?) } else { None };
                                opts.push((name, val));
                                if !self.accept_op(",") {
                                    break;
                                }
                            }
                            if paren {
                                self.expect_op(")")?;
                            }
                        }
                        Ok(Stmt::BulkInsert(Box::new(BulkStmt { table, path, opts })))
                    }
                    "BACKUP" => {
                        self.i += 1;
                        let log = if self.accept_kw("LOG") {
                            true
                        } else {
                            self.expect_kw("DATABASE")?;
                            false
                        };
                        let db = self.ident()?;
                        self.expect_kw("TO")?;
                        let path = self.backup_device()?;
                        let opts = self.backup_options()?;
                        Ok(Stmt::Backup(Box::new(BackupStmt { log, db, path, opts })))
                    }
                    "RESTORE" => {
                        self.i += 1;
                        let (kind, db) = if self.accept_kw("DATABASE") {
                            (RestoreKind::Database, Some(self.ident()?))
                        } else if self.accept_kw("LOG") {
                            (RestoreKind::Log, Some(self.ident()?))
                        } else if self.accept_kw("VERIFYONLY") {
                            (RestoreKind::VerifyOnly, None)
                        } else if self.accept_kw("HEADERONLY") {
                            (RestoreKind::HeaderOnly, None)
                        } else {
                            return Err(self.err());
                        };
                        self.expect_kw("FROM")?;
                        let path = self.backup_device()?;
                        let opts = self.backup_options()?;
                        Ok(Stmt::Restore(Box::new(RestoreStmt { kind, db, path, opts })))
                    }
                    _ => {
                        // label?
                        if self.peek_is_op(1, ":") {
                            let n = self.ident()?;
                            self.i += 1;
                            return Ok(Stmt::Label(n.to_lowercase()));
                        }
                        Err(self.err())
                    }
                }
            }
            Tok::Op("(") => Ok(Stmt::Query(Box::new(self.query()?))),
            Tok::QId(_) if self.peek_is_op(1, ":") => {
                let n = self.ident()?;
                self.i += 1;
                Ok(Stmt::Label(n.to_lowercase()))
            }
            _ => Err(self.err()),
        }
    }

    fn perm_stmt(&mut self) -> R<Stmt> {
        let kind = match self.advance_up().as_str() {
            "GRANT" => PermKind::Grant,
            "DENY" => PermKind::Deny,
            _ => PermKind::Revoke,
        };
        if kind == PermKind::Revoke && self.is_kw("GRANT") && self.peek_kw(1, &["OPTION"]) {
            self.i += 2;
            self.accept_kw("FOR");
        }
        let mut perms: Vec<String> = vec![];
        let mut cols: Vec<Vec<String>> = vec![];
        loop {
            let mut words: Vec<String> = vec![];
            while !self.is_eof() && !self.is_op(",") && !self.is_op("(") && !self.is_kw_any(&["ON", "TO", "FROM"]) {
                words.push(self.advance_up());
            }
            if words.is_empty() {
                return Err(self.err());
            }
            let mut p = words.join(" ");
            if p == "ALL PRIVILEGES" {
                p = "ALL".to_string();
            }
            let mut these = vec![];
            if self.accept_op("(") {
                loop {
                    these.push(self.ident()?);
                    if !self.accept_op(",") {
                        break;
                    }
                }
                self.expect_op(")")?;
            }
            perms.push(p);
            cols.push(these);
            if !self.accept_op(",") {
                break;
            }
        }
        let mut target = PermTargetAst::Database;
        if self.accept_kw("ON") {
            // optional class prefix: OBJECT::, SCHEMA::, DATABASE::
            if self.peek_is_op(1, ":") && self.peek_is_op(2, ":") {
                let class = self.advance_up();
                self.i += 2;
                match class.as_str() {
                    "SCHEMA" => target = PermTargetAst::Schema(self.ident()?),
                    "DATABASE" => {
                        self.ident()?;
                        target = PermTargetAst::Database;
                    }
                    _ => target = PermTargetAst::Object(self.name_parts()?),
                }
            } else {
                target = PermTargetAst::Object(self.name_parts()?);
            }
            if self.accept_op("(") {
                // GRANT SELECT ON t (a, b): the column list applies to every permission named
                let mut these = vec![];
                loop {
                    these.push(self.ident()?);
                    if !self.accept_op(",") {
                        break;
                    }
                }
                self.expect_op(")")?;
                for c in cols.iter_mut() {
                    if c.is_empty() {
                        *c = these.clone();
                    }
                }
            }
        }
        if !(self.accept_kw("TO") || self.accept_kw("FROM")) {
            return Err(self.err());
        }
        let mut principals = vec![self.ident()?];
        while self.accept_op(",") {
            principals.push(self.ident()?);
        }
        let mut with_grant = false;
        loop {
            if self.is_kw("WITH") && self.peek_kw(1, &["GRANT"]) {
                self.i += 3;
                with_grant = true;
            } else if self.accept_kw("CASCADE") {
            } else if self.accept_kw("AS") {
                self.ident()?;
            } else {
                break;
            }
        }
        Ok(Stmt::Security(Box::new(SecStmt::Perm { kind, perms, target, principals, with_grant, cols })))
    }

    fn skip_ignored(&mut self) -> R<Stmt> {
        self.i += 1;
        while !self.at_eos() {
            self.i += 1;
            let nt = self.tok();
            if matches!(nt.tok, Tok::Id(_))
                && STMT_START.contains(&nt.up.as_str())
                && !["SELECT", "UPDATE", "INSERT", "DELETE", "ALTER", "EXEC", "EXECUTE", "CREATE", "DROP", "WITH"].contains(&nt.up.as_str())
                && self.src[self.toks[self.i - 1].end..nt.pos].contains('\n')
            {
                break;
            }
        }
        Ok(Stmt::Noop)
    }

    fn tran_name(&mut self) -> Option<String> {
        match self.tok().tok.clone() {
            Tok::Id(s) if !self.is_stmt_start() => {
                self.i += 1;
                Some(s)
            }
            Tok::QId(s) | Tok::Var(s) => {
                self.i += 1;
                Some(s)
            }
            _ => None,
        }
    }

    fn st_with(&mut self) -> R<Stmt> {
        let ctes = self.cte_list()?;
        if self.is_kw("INSERT") {
            return self.st_insert(ctes);
        }
        if self.is_kw("UPDATE") {
            return self.st_update(ctes);
        }
        if self.is_kw("DELETE") {
            return self.st_delete(ctes);
        }
        if self.is_kw("MERGE") {
            return self.st_merge(ctes);
        }
        let mut q = self.query()?;
        let mut all = ctes;
        all.extend(std::mem::take(&mut q.ctes));
        q.ctes = all;
        Ok(Stmt::Query(Box::new(q)))
    }

    // ---- DML
    fn target(&mut self) -> R<Target> {
        if let Some(v) = self.var_tok() {
            self.i += 1;
            return Ok(Target::Var(v.to_lowercase()));
        }
        Ok(Target::Table(self.name_parts()?))
    }

    fn top_clause(&mut self) -> R<Option<(Expr, bool)>> {
        if self.accept_kw("TOP") {
            let e = if self.accept_op("(") {
                let e = self.expr()?;
                self.expect_op(")")?;
                e
            } else {
                self.primary()?
            };
            let pct = self.accept_kw("PERCENT");
            return Ok(Some((e, pct)));
        }
        Ok(None)
    }

    fn output_clause(&mut self) -> R<Option<Output>> {
        if !self.is_kw("OUTPUT") {
            return Ok(None);
        }
        self.i += 1;
        let mut items = vec![self.select_item()?];
        while self.accept_op(",") {
            items.push(self.select_item()?);
        }
        let mut into = None;
        let mut cols = None;
        if self.accept_kw("INTO") {
            into = Some(self.target()?);
            if self.accept_op("(") {
                let mut c = vec![self.ident()?];
                while self.accept_op(",") {
                    c.push(self.ident()?);
                }
                self.expect_op(")")?;
                cols = Some(c);
            }
        }
        Ok(Some(Output { items, into, cols }))
    }

    fn col_list(&mut self) -> R<Vec<String>> {
        self.expect_op("(")?;
        let mut c = vec![self.ident()?];
        while self.accept_op(",") {
            c.push(self.ident()?);
        }
        self.expect_op(")")?;
        Ok(c)
    }

    fn st_insert(&mut self, ctes: Vec<Cte>) -> R<Stmt> {
        self.i += 1;
        let top = self.top_clause()?;
        self.accept_kw("INTO");
        let target = self.target()?;
        let mut cols = None;
        if self.is_op("(") && !self.peek_kw(1, &["SELECT"]) {
            cols = Some(self.col_list()?);
        }
        let output = self.output_clause()?;
        let src = if self.is_kw("VALUES") {
            InsertSrc::Values(self.values_rows()?)
        } else if self.is_kw("DEFAULT") && self.peek_kw(1, &["VALUES"]) {
            self.i += 2;
            InsertSrc::DefaultValues
        } else if self.is_kw_any(&["EXEC", "EXECUTE"]) {
            InsertSrc::Exec(Box::new(self.st_exec()?))
        } else {
            InsertSrc::Query(Box::new(self.query()?))
        };
        Ok(Stmt::Insert(Box::new(Insert { ctes, target, cols, output, src, top })))
    }

    fn assign_op(&mut self) -> Option<Option<BinOp>> {
        let op = match &self.tok().tok {
            Tok::Op("=") => None,
            Tok::Op("+=") => Some(BinOp::Add),
            Tok::Op("-=") => Some(BinOp::Sub),
            Tok::Op("*=") => Some(BinOp::Mul),
            Tok::Op("/=") => Some(BinOp::Div),
            Tok::Op("%=") => Some(BinOp::Mod),
            Tok::Op("&=") => Some(BinOp::BitAnd),
            Tok::Op("|=") => Some(BinOp::BitOr),
            Tok::Op("^=") => Some(BinOp::BitXor),
            _ => return None,
        };
        self.i += 1;
        Some(op)
    }

    fn set_items(&mut self) -> R<Vec<SetItem>> {
        let mut sets = vec![];
        loop {
            if let Some(v) = self.var_tok() {
                self.i += 1;
                let op = self.assign_op().ok_or_else(|| self.err())?;
                let e = self.expr()?;
                sets.push(SetItem::Var { var: v.to_lowercase(), op, e });
            } else {
                let mut parts = self.name_parts()?;
                // SET col.modify('...'): the column becomes what the xml data manipulation makes of it
                if parts.len() >= 2 && parts.last().unwrap().eq_ignore_ascii_case("modify") && self.is_op("(") {
                    parts.pop();
                    let e = self.xml_modify_call(Expr::Col(parts.clone()))?;
                    sets.push(SetItem::Col { parts, op: None, e });
                } else {
                    let op = self.assign_op().ok_or_else(|| self.err())?;
                    let e = self.expr()?;
                    sets.push(SetItem::Col { parts, op, e });
                }
            }
            if !self.accept_op(",") {
                break;
            }
        }
        Ok(sets)
    }

    fn from_clause_opt(&mut self) -> R<Vec<TableRef>> {
        let mut from = vec![];
        if self.accept_kw("FROM") {
            from.push(self.table_source()?);
            while self.accept_op(",") {
                from.push(self.table_source()?);
            }
        }
        Ok(from)
    }

    fn where_current(&mut self) -> R<(Option<Expr>, Option<String>)> {
        if self.accept_kw("WHERE") {
            if self.is_kw("CURRENT") && self.peek_kw(1, &["OF"]) {
                self.i += 2;
                return Ok((None, Some(self.ident()?.to_lowercase())));
            }
            return Ok((Some(self.expr()?.factor_common()), None));
        }
        Ok((None, None))
    }

    fn skip_option(&mut self) -> R<()> {
        if self.is_kw("OPTION") && self.peek_is_op(1, "(") {
            self.i += 1;
            self.skip_parens()?;
        }
        Ok(())
    }

    fn st_update(&mut self, ctes: Vec<Cte>) -> R<Stmt> {
        self.i += 1;
        let top = self.top_clause()?;
        let target = self.target()?;
        self.expect_kw("SET")?;
        let sets = self.set_items()?;
        let output = self.output_clause()?;
        let from = self.from_clause_opt()?;
        let (where_, current_of) = self.where_current()?;
        self.skip_option()?;
        Ok(Stmt::Update(Box::new(Update { ctes, target, sets, output, from, where_, current_of, top })))
    }

    fn st_delete(&mut self, ctes: Vec<Cte>) -> R<Stmt> {
        self.i += 1;
        let top = self.top_clause()?;
        self.accept_kw("FROM");
        let target = self.target()?;
        let output = self.output_clause()?;
        let from = self.from_clause_opt()?;
        let (where_, current_of) = self.where_current()?;
        self.skip_option()?;
        Ok(Stmt::Delete(Box::new(Delete { ctes, target, output, from, where_, current_of, top })))
    }

    fn st_merge(&mut self, ctes: Vec<Cte>) -> R<Stmt> {
        self.i += 1;
        self.accept_kw("INTO");
        let target = self.name_parts()?;
        let target_alias = if self.accept_kw("AS") {
            Some(self.ident()?)
        } else if self.alias_ok() {
            Some(self.ident()?)
        } else {
            None
        };
        self.expect_kw("USING")?;
        let source = self.table_primary()?;
        self.expect_kw("ON")?;
        let on = self.expr()?;
        let mut whens = vec![];
        while self.accept_kw("WHEN") {
            let mut kind = MergeKind::Matched;
            if self.accept_kw("NOT") {
                self.expect_kw("MATCHED")?;
                kind = MergeKind::NotMatched;
                if self.accept_kw("BY") {
                    if self.accept_kw("SOURCE") {
                        kind = MergeKind::NotMatchedBySource;
                    } else {
                        self.expect_kw("TARGET")?;
                    }
                }
            } else {
                self.expect_kw("MATCHED")?;
            }
            let cond = if self.accept_kw("AND") { Some(self.expr()?) } else { None };
            self.expect_kw("THEN")?;
            let action = if self.accept_kw("DELETE") {
                MergeAction::Delete
            } else if self.accept_kw("UPDATE") {
                self.expect_kw("SET")?;
                MergeAction::Update(self.set_items()?)
            } else if self.accept_kw("INSERT") {
                let cols = if self.is_op("(") { Some(self.col_list()?) } else { None };
                if self.accept_kw("DEFAULT") {
                    self.expect_kw("VALUES")?;
                    MergeAction::Insert { cols, values: None }
                } else {
                    self.expect_kw("VALUES")?;
                    self.expect_op("(")?;
                    let mut v = vec![self.expr()?];
                    while self.accept_op(",") {
                        v.push(self.expr()?);
                    }
                    self.expect_op(")")?;
                    MergeAction::Insert { cols, values: Some(v) }
                }
            } else {
                return Err(self.err());
            };
            whens.push(MergeWhen { kind, cond, action });
        }
        let output = self.output_clause()?;
        self.skip_option()?;
        Ok(Stmt::Merge(Box::new(Merge { ctes, target, target_alias, source, on, whens, output })))
    }

    // ---- variables / flow
    fn st_declare(&mut self) -> R<Stmt> {
        self.i += 1;
        let mut decls = vec![];
        loop {
            if let Some(v) = self.var_tok() {
                self.i += 1;
                let name = v.to_lowercase();
                self.accept_kw("AS");
                if self.accept_kw("TABLE") {
                    let defs = self.table_body()?;
                    decls.push(Decl::Table { name, defs });
                } else if self.accept_kw("CURSOR") {
                    decls.push(Decl::CursorVar(name));
                } else {
                    let ty = self.parse_type()?;
                    let default = if self.accept_op("=") { Some(self.expr()?) } else { None };
                    decls.push(Decl::Var { name, ty, default });
                }
            } else {
                let name = self.ident()?.to_lowercase();
                self.expect_kw("CURSOR")?;
                while !self.is_kw("FOR") {
                    if self.is_eof() {
                        return Err(self.err());
                    }
                    self.i += 1;
                }
                self.expect_kw("FOR")?;
                let q = self.query()?;
                if self.is_kw("FOR") && self.peek_kw(1, &["UPDATE", "READ"]) {
                    self.i += 2;
                    if self.accept_kw("OF") {
                        self.ident()?;
                        while self.accept_op(",") {
                            self.ident()?;
                        }
                    }
                }
                return Ok(Stmt::DeclareCursor { name, query: Rc::new(q) });
            }
            if !self.accept_op(",") {
                break;
            }
        }
        Ok(Stmt::Declare(decls))
    }

    fn st_set(&mut self) -> R<Stmt> {
        self.i += 1;
        if let Some(v) = self.var_tok() {
            self.i += 1;
            // SET @x.modify('...')
            if self.is_op(".") && self.peek_kw(1, &["MODIFY"]) {
                self.i += 2;
                let e = self.xml_modify_call(Expr::Var(v.to_lowercase()))?;
                return Ok(Stmt::Set { var: v.to_lowercase(), op: None, e });
            }
            let op = self.assign_op().ok_or_else(|| self.err())?;
            if self.is_kw("CURSOR") {
                self.i += 1;
                self.expect_kw("FOR")?;
                return Ok(Stmt::SetCursorVar { var: v.to_lowercase(), query: Box::new(self.query()?) });
            }
            return Ok(Stmt::Set { var: v.to_lowercase(), op, e: self.expr()? });
        }
        let mut opts = vec![];
        loop {
            let name = self.ident()?.to_uppercase();
            match name.as_str() {
                "TRANSACTION" => {
                    self.expect_kw("ISOLATION")?;
                    self.expect_kw("LEVEL")?;
                    let mut words = vec![self.advance_up()];
                    while self.is_kw_any(&["UNCOMMITTED", "COMMITTED", "READ", "REPEATABLE", "SNAPSHOT", "SERIALIZABLE"]) {
                        words.push(self.advance_up());
                    }
                    opts.push(SetOpt::Isolation(words.join(" ")));
                }
                "IDENTITY_INSERT" => {
                    let tbl = self.name_parts()?;
                    let on = self.advance_up() == "ON";
                    opts.push(SetOpt::IdentityInsert(tbl, on));
                }
                "STATISTICS" => {
                    self.advance();
                    let v = self.advance_up();
                    opts.push(SetOpt::Flag("STATISTICS".into(), v));
                }
                "ROWCOUNT" | "DATEFIRST" | "TEXTSIZE" | "LOCK_TIMEOUT" | "DEADLOCK_PRIORITY" | "LANGUAGE" | "DATEFORMAT" | "CONTEXT_INFO" => {
                    // SET LANGUAGE british and SET DATEFORMAT dmy take a bare name as well as a string or variable
                    let bare = matches!(name.as_str(), "LANGUAGE" | "DATEFORMAT") && matches!(self.tok().tok, Tok::Id(_)) && !self.is_kw_any(&["NULL", "N"]);
                    let e = if bare {
                        let word = match self.advance().tok {
                            Tok::Id(w) => w.to_string(),
                            _ => String::new(),
                        };
                        Expr::Lit(Value::Str(word))
                    } else {
                        self.expr()?
                    };
                    opts.push(SetOpt::Expr(name, e));
                }
                _ => {
                    let v = self.advance();
                    let s = match v.tok {
                        Tok::Id(s) => s.to_uppercase(),
                        Tok::Int(i) => i.to_string(),
                        Tok::Str(s) => s,
                        _ => String::new(),
                    };
                    opts.push(SetOpt::Flag(name, s));
                }
            }
            if !self.accept_op(",") {
                break;
            }
        }
        Ok(Stmt::SetOpts(opts))
    }

    fn st_begin(&mut self) -> R<Stmt> {
        if self.peek_kw(1, &["TRY"]) {
            self.i += 2;
            let body = self.block_until_end()?;
            self.expect_kw("END")?;
            self.expect_kw("TRY")?;
            self.accept_op(";");
            self.expect_kw("BEGIN")?;
            self.expect_kw("CATCH")?;
            let catch = self.block_until_end()?;
            self.expect_kw("END")?;
            self.expect_kw("CATCH")?;
            return Ok(Stmt::Try { body, catch });
        }
        if self.peek_kw(1, &["TRAN", "TRANSACTION", "DISTRIBUTED"]) {
            self.i += 1;
            self.accept_kw("DISTRIBUTED");
            self.i += 1;
            let name = if !self.is_kw("WITH") { self.tran_name() } else { None };
            if self.is_kw("WITH") && self.peek_kw(1, &["MARK"]) {
                self.i += 2;
                if matches!(self.tok().tok, Tok::Str(_)) {
                    self.i += 1;
                }
            }
            return Ok(Stmt::BeginTran(name));
        }
        self.i += 1;
        let body = self.block_until_end()?;
        self.expect_kw("END")?;
        Ok(Stmt::Block(body))
    }

    fn block_until_end(&mut self) -> R<Vec<Stmt>> {
        let mut stmts = vec![];
        while !self.is_kw("END") {
            if self.is_eof() {
                return Err(self.err());
            }
            if self.accept_op(";") {
                continue;
            }
            stmts.push(self.statement()?);
        }
        Ok(stmts)
    }

    fn st_fetch(&mut self) -> R<Stmt> {
        self.i += 1;
        let mut orient = "NEXT".to_string();
        let mut arg = None;
        if self.is_kw_any(&["NEXT", "PRIOR", "FIRST", "LAST"]) {
            orient = self.advance_up();
        } else if self.is_kw_any(&["ABSOLUTE", "RELATIVE"]) {
            orient = self.advance_up();
            arg = Some(self.expr()?);
        }
        self.accept_kw("FROM");
        self.accept_kw("GLOBAL");
        let name = self.ident()?.to_lowercase();
        let mut into = None;
        if self.accept_kw("INTO") {
            let mut v = vec![];
            loop {
                match self.advance().tok {
                    Tok::Var(x) => v.push(x.to_lowercase()),
                    _ => return Err(self.err()),
                }
                if !self.accept_op(",") {
                    break;
                }
            }
            into = Some(v);
        }
        Ok(Stmt::Fetch { orient, arg, name, into })
    }

    fn st_exec(&mut self) -> R<Stmt> {
        self.i += 1;
        self.st_exec_body()
    }

    /// What follows `EXEC`: a procedure call, or `(string expression)`.
    fn st_exec_body(&mut self) -> R<Stmt> {
        if self.is_op("(") {
            self.i += 1;
            let mut parts = vec![self.expr()?];
            while self.accept_op("+") {
                parts.push(self.expr()?);
            }
            self.expect_op(")")?;
            return Ok(Stmt::ExecStr(parts));
        }
        let mut retvar = None;
        if let Some(v) = self.var_tok() {
            if self.peek_is_op(1, "=") {
                retvar = Some(v.to_lowercase());
                self.i += 2;
            }
        }
        let (name, is_var) = if let Some(v) = self.var_tok() {
            self.i += 1;
            (vec![v.to_lowercase()], true)
        } else {
            (self.name_parts()?, false)
        };
        let mut args = vec![];
        while !self.at_eos() && !(self.is_stmt_start()) {
            let mut aname = None;
            if let Some(v) = self.var_tok() {
                if self.peek_is_op(1, "=") {
                    aname = Some(v.to_lowercase());
                    self.i += 2;
                }
            }
            let start_tok = self.tok().clone();
            let start_idx = self.i;
            let e = if self.accept_kw("DEFAULT") { Expr::Default } else { self.expr()? };
            // N'...' is parsed as a cast of the literal, but is still just a constant
            let n_literal = matches!(start_tok.tok, Tok::Str(_)) && start_tok.unicode && self.i == start_idx + 1;
            // as in SQL Server, an argument is a constant, a variable, DEFAULT or a bare word: not an expression or a function call
            let allowed = n_literal || match &e {
                Expr::Lit(_) | Expr::Var(_) | Expr::SysVar(_) | Expr::Default => true,
                Expr::Col(p) => p.len() == 1,
                Expr::Neg(x) => matches!(**x, Expr::Lit(_)),
                _ => false,
            };
            if !allowed {
                return Err(self.err_at(&start_tok));
            }
            let out = self.accept_kw("OUTPUT") || self.accept_kw("OUT");
            args.push(ExecArg { name: aname, e, out });
            if !self.accept_op(",") {
                break;
            }
        }
        if self.is_kw("WITH") && self.peek_kw(1, &["RESULT"]) {
            self.i += 3;
            self.skip_parens()?;
        }
        Ok(Stmt::Exec(Box::new(ExecStmt { name, name_is_var: is_var, args, retvar })))
    }

    // ---- DDL
    /// `DATABASE` or `ALL SERVER` after `ON`: a DDL trigger scope.
    fn trigger_scope_kw(&mut self) -> Option<TriggerScope> {
        if self.is_kw("DATABASE") {
            self.i += 1;
            Some(TriggerScope::Database)
        } else if self.is_kw("ALL") && self.peek_kw(1, &["SERVER"]) {
            self.i += 2;
            Some(TriggerScope::Server)
        } else {
            None
        }
    }

    /// `ENABLE | DISABLE TRIGGER name [, ...] | ALL ON DATABASE | ALL SERVER | table`
    fn st_enable_trigger(&mut self, enable: bool) -> R<Stmt> {
        self.i += 1;
        self.expect_kw("TRIGGER")?;
        let (mut names, mut all) = (vec![], false);
        if self.accept_kw("ALL") {
            all = true;
        } else {
            names.push(self.name_parts()?);
            while self.accept_op(",") {
                names.push(self.name_parts()?);
            }
        }
        self.expect_kw("ON")?;
        let scope = match self.trigger_scope_kw() {
            Some(s) => s,
            None => TriggerScope::Table(self.name_parts()?),
        };
        Ok(Stmt::TriggerAdmin(Box::new(TriggerAdmin { names, all, scope, act: TriggerAct::Enable(enable) })))
    }

    fn st_drop(&mut self) -> R<Stmt> {
        self.i += 1;
        if self.audit_ahead() {
            return self.audit_stmt("DROP");
        }
        let what = self.advance_up();
        if what == "XML" {
            self.expect_kw("SCHEMA")?;
            self.expect_kw("COLLECTION")?;
            let mut if_exists = false;
            if self.accept_kw("IF") {
                self.expect_kw("EXISTS")?;
                if_exists = true;
            }
            return Ok(Stmt::XmlSchema(Box::new(XmlSchemaStmt::Drop { name: self.name_parts()?, if_exists })));
        }
        if what == "FULLTEXT" {
            let st = match self.advance_up().as_str() {
                "CATALOG" => FullTextStmt::DropCatalog { name: self.ident()? },
                "INDEX" => {
                    self.expect_kw("ON")?;
                    FullTextStmt::DropIndex { table: self.name_parts()? }
                }
                other => return Err(SqlError::syntax(other)),
            };
            return Ok(Stmt::FullText(Box::new(st)));
        }
        if what == "INDEX" {
            let mut if_exists = false;
            if self.accept_kw("IF") {
                self.expect_kw("EXISTS")?;
                if_exists = true;
            }
            let mut names = vec![];
            loop {
                let nm = self.name_parts()?;
                let tbl = if self.accept_kw("ON") { Some(self.name_parts()?) } else { None };
                names.push((nm, tbl));
                if !self.accept_op(",") {
                    break;
                }
            }
            return Ok(Stmt::DropIndex { names, if_exists });
        }
        if what == "SECURITY" {
            self.expect_kw("POLICY")?;
            let mut if_exists = false;
            if self.accept_kw("IF") {
                self.expect_kw("EXISTS")?;
                if_exists = true;
            }
            let name = self.name_parts()?;
            return Ok(Stmt::DropSecurityPolicy { name, if_exists });
        }
        if !["TABLE", "VIEW", "PROC", "PROCEDURE", "FUNCTION", "SCHEMA", "DATABASE", "TYPE", "SEQUENCE", "TRIGGER", "LOGIN", "USER", "ROLE", "SYNONYM", "STATISTICS"].contains(&what.as_str()) {
            return Err(SqlError::syntax(&what));
        }
        let mut if_exists = false;
        if self.accept_kw("IF") {
            self.expect_kw("EXISTS")?;
            if_exists = true;
        }
        let mut names = vec![self.name_parts()?];
        while self.accept_op(",") {
            names.push(self.name_parts()?);
        }
        if what == "TRIGGER" && self.is_kw("ON") {
            self.i += 1;
            let scope = self.trigger_scope_kw().ok_or_else(|| self.err())?;
            return Ok(Stmt::TriggerAdmin(Box::new(TriggerAdmin { names, all: false, scope, act: TriggerAct::Drop { if_exists } })));
        }
        let what = if what == "PROC" { "PROCEDURE".to_string() } else { what };
        Ok(Stmt::Drop { what, names, if_exists })
    }

    fn param_list(&mut self, parens: bool) -> R<Vec<Param>> {
        let mut params = vec![];
        if parens {
            self.expect_op("(")?;
        }
        while let Some(v) = self.var_tok() {
            self.i += 1;
            self.accept_kw("AS");
            let ty = self.parse_type()?;
            let mut default = None;
            let (mut out, mut readonly) = (false, false);
            if self.accept_op("=") {
                default = Some(self.expr()?);
            }
            while self.is_kw_any(&["OUT", "OUTPUT", "READONLY", "VARYING"]) {
                match self.advance_up().as_str() {
                    "OUT" | "OUTPUT" => out = true,
                    "READONLY" => readonly = true,
                    _ => {}
                }
            }
            params.push(Param { name: v.to_lowercase(), ty, default, out, readonly });
            if !self.accept_op(",") {
                break;
            }
        }
        if parens {
            self.expect_op(")")?;
        }
        Ok(params)
    }

    fn skip_with_options(&mut self) {
        if self.accept_kw("WITH") {
            loop {
                let option = self.advance();
                // EXECUTE AS CALLER | SELF | OWNER | 'user' is three tokens; it is accepted and not applied
                if option.up == "EXECUTE" && self.accept_kw("AS") {
                    self.advance();
                }
                if !self.accept_op(",") {
                    break;
                }
            }
        }
    }

    fn rest_body(&mut self) -> R<Vec<Stmt>> {
        let mut body = vec![];
        while !self.is_eof() {
            if self.accept_op(";") {
                continue;
            }
            body.push(self.statement()?);
        }
        Ok(body)
    }

    /// `SERVER AUDIT` or `DATABASE AUDIT` ahead.
    fn audit_ahead(&self) -> bool {
        self.is_kw_any(&["SERVER", "DATABASE"]) && self.peek_kw(1, &["AUDIT"])
    }

    /// `( name = value [unit], ... )`, values as text.
    fn audit_options(&mut self) -> R<Vec<(String, String)>> {
        let mut opts = vec![];
        self.expect_op("(")?;
        loop {
            let name = self.ident()?.to_uppercase();
            let mut val = String::new();
            if self.accept_op("=") {
                let t = self.advance();
                val = match t.tok {
                    Tok::Str(s) | Tok::Id(s) | Tok::QId(s) => s,
                    Tok::Int(i) => i.to_string(),
                    _ => t.up.clone(),
                };
                if self.is_kw_any(&["KB", "MB", "GB", "TB"]) {
                    self.i += 1;
                }
            }
            opts.push((name, val));
            if !self.accept_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(opts)
    }

    fn audit_target(&mut self) -> R<crate::audit::AuditTarget> {
        use crate::audit::AuditTarget;
        if self.accept_kw("FILE") {
            let opts = self.audit_options()?;
            let path = opts.iter().find(|(n, _)| n == "FILEPATH").map(|(_, v)| v.clone()).ok_or_else(|| SqlError::with(25706, 16, 1, "The FILEPATH option is required for an audit that writes to a file."))?;
            Ok(AuditTarget::File { path })
        } else if self.accept_kw("APPLICATION_LOG") {
            Ok(AuditTarget::ApplicationLog)
        } else if self.accept_kw("SECURITY_LOG") {
            Ok(AuditTarget::SecurityLog)
        } else {
            Err(self.err())
        }
    }

    fn audit_where(&mut self) -> R<(String, Expr)> {
        let start = self.tok().pos;
        let e = self.expr()?;
        let end = if self.i > 0 { self.toks[self.i - 1].end } else { start };
        Ok((self.src[start..end].trim().to_string(), e))
    }

    /// One `( ... )` of a specification: a group name, or `action [, ...] ON [class::]securable BY principal [, ...]`.
    fn audit_items(&mut self) -> R<Vec<crate::audit::AuditItem>> {
        use crate::audit::{AuditItem, ObjSel};
        self.expect_op("(")?;
        let first = self.ident()?.to_uppercase();
        if first.ends_with("_GROUP") {
            self.expect_op(")")?;
            return Ok(vec![AuditItem::Group(first)]);
        }
        let mut actions = vec![first];
        while self.accept_op(",") {
            actions.push(self.ident()?.to_uppercase());
        }
        self.expect_kw("ON")?;
        let object = if self.peek_is_op(1, ":") && self.peek_is_op(2, ":") {
            let class = self.advance_up();
            self.i += 2;
            match class.as_str() {
                "SCHEMA" => ObjSel::Schema(self.ident()?),
                "DATABASE" => ObjSel::Database(self.ident()?),
                _ => ObjSel::Object(self.name_parts()?),
            }
        } else {
            ObjSel::Object(self.name_parts()?)
        };
        self.expect_kw("BY")?;
        let mut principals = vec![self.ident()?];
        while self.accept_op(",") {
            principals.push(self.ident()?);
        }
        self.expect_op(")")?;
        Ok(actions.into_iter().map(|action| AuditItem::Action { action, object: object.clone(), principals: principals.clone() }).collect())
    }

    /// `CREATE | ALTER | DROP  SERVER AUDIT [SPECIFICATION] ...` and `... DATABASE AUDIT SPECIFICATION ...`; the cursor is on
    /// SERVER or DATABASE.
    fn audit_stmt(&mut self, verb: &str) -> R<Stmt> {
        use crate::audit::AuditTarget;
        let server = self.is_kw("SERVER");
        self.i += 1;
        self.expect_kw("AUDIT")?;
        let spec = self.accept_kw("SPECIFICATION");
        if !spec && !server {
            return Err(self.err());
        }
        let name = self.ident()?;
        let stmt = if !spec {
            match verb {
                "DROP" => AuditStmt::DropAudit { name },
                "CREATE" => {
                    self.expect_kw("TO")?;
                    let target: AuditTarget = self.audit_target()?;
                    let mut opts = vec![];
                    if self.accept_kw("WITH") {
                        opts = self.audit_options()?;
                    }
                    let filter = if self.accept_kw("WHERE") { Some(self.audit_where()?) } else { None };
                    let state_on = opts.iter().any(|(n, v)| n == "STATE" && v.eq_ignore_ascii_case("ON"));
                    AuditStmt::CreateAudit { name, target, opts, filter, state_on }
                }
                _ => {
                    let mut target = None;
                    let mut opts = vec![];
                    let mut filter = None;
                    let mut state = None;
                    if self.accept_kw("TO") {
                        target = Some(self.audit_target()?);
                    }
                    if self.accept_kw("WITH") {
                        opts = self.audit_options()?;
                        if let Some((_, v)) = opts.iter().find(|(n, _)| n == "STATE") {
                            state = Some(v.eq_ignore_ascii_case("ON"));
                        }
                    }
                    if self.accept_kw("WHERE") {
                        filter = Some(Some(self.audit_where()?));
                    } else if self.is_kw("REMOVE") && self.peek_kw(1, &["WHERE"]) {
                        self.i += 2;
                        filter = Some(None);
                    }
                    AuditStmt::AlterAudit { name, target, opts, filter, state }
                }
            }
        } else {
            match verb {
                "DROP" => AuditStmt::DropSpec { server, name },
                "CREATE" => {
                    self.expect_kw("FOR")?;
                    self.expect_kw("SERVER")?;
                    self.expect_kw("AUDIT")?;
                    let audit = self.ident()?;
                    let mut items = vec![];
                    while self.accept_kw("ADD") {
                        items.extend(self.audit_items()?);
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                    let mut state_on = false;
                    if self.accept_kw("WITH") {
                        state_on = self.audit_options()?.iter().any(|(n, v)| n == "STATE" && v.eq_ignore_ascii_case("ON"));
                    }
                    AuditStmt::CreateSpec { server, name, audit, items, state_on }
                }
                _ => {
                    let (mut add, mut drop, mut state) = (vec![], vec![], None);
                    if self.accept_kw("FOR") {
                        // the audit cannot be changed; accept and ignore a repeated FOR SERVER AUDIT name
                        self.expect_kw("SERVER")?;
                        self.expect_kw("AUDIT")?;
                        self.ident()?;
                    }
                    while self.is_kw_any(&["ADD", "DROP"]) {
                        let is_add = self.advance_up() == "ADD";
                        let items = self.audit_items()?;
                        if is_add {
                            add.extend(items);
                        } else {
                            drop.extend(items);
                        }
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                    if self.accept_kw("WITH") {
                        state = self.audit_options()?.iter().find(|(n, _)| n == "STATE").map(|(_, v)| v.eq_ignore_ascii_case("ON"));
                    }
                    AuditStmt::AlterSpec { server, name, add, drop, state }
                }
            }
        };
        Ok(Stmt::Audit(Box::new(stmt)))
    }

    /// The language of a full-text column: an LCID, or the name of a language (those the server knows; others are neutral).
    fn ft_language(&mut self) -> R<i32> {
        match self.advance().tok {
            Tok::Int(n) => Ok(n as i32),
            Tok::Hex(b) => Ok(b.iter().fold(0i32, |a, x| a << 8 | *x as i32)),
            Tok::Str(s) | Tok::Id(s) | Tok::QId(s) => Ok(match s.to_lowercase().as_str() {
                "english" => 1033,
                "german" => 1031,
                "french" => 1036,
                "spanish" => 3082,
                "italian" => 1040,
                _ => 0,
            }),
            _ => Err(self.err()),
        }
    }

    /// `(col [TYPE COLUMN c] [LANGUAGE l] [STATISTICAL_SEMANTICS], ...)`
    fn ft_columns_def(&mut self) -> R<Vec<(String, Option<i32>)>> {
        self.expect_op("(")?;
        let mut cols = vec![];
        loop {
            let c = self.ident()?;
            let mut lang = None;
            loop {
                if self.accept_kw("LANGUAGE") {
                    lang = Some(self.ft_language()?);
                } else if self.accept_kw("TYPE") {
                    self.expect_kw("COLUMN")?;
                    self.ident()?;
                } else if self.accept_kw("STATISTICAL_SEMANTICS") {
                } else {
                    break;
                }
            }
            cols.push((c, lang));
            if !self.accept_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(cols)
    }

    fn signed_number(&mut self) -> R<f64> {
        let neg = self.accept_op("-");
        self.accept_op("+");
        let v = match self.advance().tok {
            Tok::Int(i) => i as f64,
            Tok::Dec(d) => d.to_string().parse::<f64>().map_err(|_| self.err())?,
            Tok::Float(f) => f,
            _ => return Err(self.err()),
        };
        Ok(if neg { -v } else { v })
    }

    /// The `WITH (...)` of CREATE SPATIAL INDEX: BOUNDING_BOX, GRIDS and CELLS_PER_OBJECT are kept, the rest is skipped.
    fn spatial_options(&mut self, sp: &mut SpatialIndexSpec) -> R<()> {
        self.expect_op("(")?;
        while !self.is_op(")") && !self.is_eof() {
            let key = self.advance_up();
            self.expect_op("=")?;
            match key.as_str() {
                "BOUNDING_BOX" => {
                    self.expect_op("(")?;
                    let mut v = vec![];
                    loop {
                        // either the four numbers, or XMIN = n, YMIN = n, ...
                        if matches!(self.tok().tok, Tok::Id(_)) && self.peek_is_op(1, "=") {
                            self.i += 2;
                        }
                        v.push(self.signed_number()?);
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    if v.len() != 4 {
                        return Err(self.err());
                    }
                    sp.bbox = Some([v[0], v[1], v[2], v[3]]);
                }
                "GRIDS" => {
                    self.expect_op("(")?;
                    let mut g = [2u8; 4];
                    let mut k = 0;
                    loop {
                        if matches!(self.tok().tok, Tok::Id(_)) && self.peek_is_op(1, "=") {
                            // LEVEL_n = density
                            let lvl = self.advance_up();
                            self.i += 1;
                            k = lvl.trim_start_matches("LEVEL_").parse::<usize>().ok().filter(|n| (1..=4).contains(n)).ok_or_else(|| self.err())? - 1;
                        }
                        g[k.min(3)] = match self.advance_up().as_str() {
                            "LOW" => 1,
                            "MEDIUM" => 2,
                            "HIGH" => 3,
                            _ => return Err(self.err()),
                        };
                        k += 1;
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    sp.grids = Some(g);
                }
                "CELLS_PER_OBJECT" => match self.advance().tok {
                    Tok::Int(n) => sp.cells = Some(n as i32),
                    _ => return Err(self.err()),
                },
                _ => {
                    if self.is_op("(") {
                        self.skip_parens()?;
                    } else {
                        self.i += 1;
                    }
                }
            }
            if !self.accept_op(",") {
                break;
            }
        }
        self.expect_op(")")
    }

    /// `CREATE | ALTER FULLTEXT CATALOG | INDEX`; the cursor is on FULLTEXT.
    fn fulltext_stmt(&mut self, alter: bool) -> R<Stmt> {
        self.expect_kw("FULLTEXT")?;
        let what = self.advance_up();
        let st = match (what.as_str(), alter) {
            ("CATALOG", false) => {
                let name = self.ident()?;
                let (mut default, mut accent) = (false, true);
                while !self.at_eos() && !self.is_kw_any(&["CREATE", "ALTER", "DROP", "SELECT", "INSERT", "UPDATE", "DELETE", "EXEC", "EXECUTE", "IF", "BEGIN", "PRINT", "DECLARE", "SET", "USE"]) {
                    if self.accept_kw("AS") {
                        self.expect_kw("DEFAULT")?;
                        default = true;
                    } else if self.accept_kw("ACCENT_SENSITIVITY") {
                        self.accept_op("=");
                        accent = self.advance_up() == "ON";
                    } else if self.is_op("(") {
                        self.skip_parens()?;
                    } else {
                        self.i += 1;
                    }
                }
                FullTextStmt::CreateCatalog { name, default, accent_sensitive: accent }
            }
            ("CATALOG", true) => {
                let name = self.ident()?;
                let action = if self.accept_kw("AS") {
                    self.expect_kw("DEFAULT")?;
                    "DEFAULT".to_string()
                } else {
                    self.advance_up()
                };
                while !self.at_eos() && !self.is_kw_any(&["CREATE", "ALTER", "DROP", "SELECT", "INSERT", "UPDATE", "DELETE", "EXEC", "EXECUTE", "IF", "BEGIN", "PRINT", "DECLARE", "SET", "USE"]) {
                    self.i += 1;
                }
                FullTextStmt::AlterCatalog { name, action }
            }
            ("INDEX", false) => {
                self.expect_kw("ON")?;
                let table = self.name_parts()?;
                let cols = if self.is_op("(") { self.ft_columns_def()? } else { vec![] };
                let (mut key_index, mut catalog, mut tracking) = (None, None, None);
                if self.accept_kw("KEY") {
                    self.expect_kw("INDEX")?;
                    key_index = Some(self.ident()?);
                }
                if self.accept_kw("ON") {
                    if self.accept_op("(") {
                        catalog = Some(self.ident()?);
                        while !self.is_op(")") && !self.is_eof() {
                            self.i += 1;
                        }
                        self.expect_op(")")?;
                    } else {
                        catalog = Some(self.ident()?);
                    }
                }
                if self.accept_kw("WITH") {
                    let paren = self.accept_op("(");
                    while !self.at_eos() && !self.is_kw_any(&["CREATE", "ALTER", "DROP", "SELECT", "INSERT", "UPDATE", "DELETE", "EXEC", "EXECUTE", "IF", "BEGIN", "PRINT", "DECLARE", "SET", "USE"]) && !(paren && self.is_op(")")) {
                        if self.accept_kw("CHANGE_TRACKING") {
                            self.accept_op("=");
                            tracking = Some(self.advance_up());
                        } else {
                            self.i += 1;
                        }
                    }
                    if paren {
                        self.expect_op(")")?;
                    }
                }
                FullTextStmt::CreateIndex { table, cols, key_index, catalog, tracking }
            }
            ("INDEX", true) => {
                self.expect_kw("ON")?;
                let table = self.name_parts()?;
                let action = match self.advance_up().as_str() {
                    "ENABLE" => FtAlter::Enable,
                    "DISABLE" => FtAlter::Disable,
                    "SET" => {
                        self.expect_kw("CHANGE_TRACKING")?;
                        self.accept_op("=");
                        FtAlter::SetTracking(self.advance_up())
                    }
                    "ADD" => FtAlter::AddCols(self.ft_columns_def()?),
                    "DROP" => {
                        self.expect_op("(")?;
                        let mut cols = vec![];
                        loop {
                            cols.push(self.ident()?);
                            if !self.accept_op(",") {
                                break;
                            }
                        }
                        self.expect_op(")")?;
                        FtAlter::DropCols(cols)
                    }
                    "START" | "STOP" | "PAUSE" | "RESUME" => FtAlter::Population,
                    _ => return Err(self.err()),
                };
                // [WITH NO POPULATION] and the words of START ... POPULATION
                while !self.at_eos() && !self.is_kw_any(&["CREATE", "ALTER", "DROP", "SELECT", "INSERT", "UPDATE", "DELETE", "EXEC", "EXECUTE", "IF", "BEGIN", "PRINT", "DECLARE", "SET", "USE"]) {
                    self.i += 1;
                }
                FullTextStmt::AlterIndex { table, action }
            }
            _ => return Err(SqlError::syntax(&what)),
        };
        Ok(Stmt::FullText(Box::new(st)))
    }

    fn create_object(&mut self, or_alter: bool, alter: bool, start: usize) -> R<Stmt> {
        if self.audit_ahead() {
            return self.audit_stmt("CREATE");
        }
        if self.is_kw("FULLTEXT") {
            return self.fulltext_stmt(alter);
        }
        if self.is_kw("XML") && self.peek_kw(1, &["SCHEMA"]) {
            self.i += 2;
            self.expect_kw("COLLECTION")?;
            let name = self.name_parts()?;
            let st = if alter {
                self.expect_kw("ADD")?;
                XmlSchemaStmt::Alter { name, text: self.expr()? }
            } else {
                self.expect_kw("AS")?;
                XmlSchemaStmt::Create { name, text: self.expr()? }
            };
            return Ok(Stmt::XmlSchema(Box::new(st)));
        }
        let mut unique = false;
        if self.accept_kw("UNIQUE") {
            unique = true;
        }
        if self.is_kw_any(&["CLUSTERED", "NONCLUSTERED"]) {
            self.i += 1;
        }
        if self.is_kw("COLUMNSTORE") {
            self.i += 1;
        }
        // CREATE [PRIMARY] XML INDEX
        let mut xml_primary = None;
        if self.is_kw("PRIMARY") && self.peek_kw(1, &["XML"]) && self.peek_kw(2, &["INDEX"]) {
            self.i += 2;
            xml_primary = Some(true);
        } else if self.is_kw("XML") && self.peek_kw(1, &["INDEX"]) {
            self.i += 1;
            xml_primary = Some(false);
        }
        // CREATE SPATIAL INDEX
        let spatial_kw = self.is_kw("SPATIAL") && self.peek_kw(1, &["INDEX"]);
        if spatial_kw {
            self.i += 1;
        }
        let t = self.advance();
        let what = t.up.clone();
        match what.as_str() {
            "TABLE" => {
                let name = self.name_parts()?;
                let defs = self.table_body()?;
                let mut versioning = None;
                while self.is_kw_any(&["ON", "WITH", "TEXTIMAGE_ON"]) {
                    let with = self.advance_up() == "WITH";
                    if with && self.is_op("(") {
                        if let Some(v) = self.table_with_options()? {
                            versioning = Some(v);
                        }
                    } else if self.is_op("(") {
                        self.skip_parens()?;
                    } else {
                        self.i += 1;
                    }
                }
                Ok(Stmt::CreateTable { name, defs, versioning })
            }
            "INDEX" => {
                let name = self.ident()?;
                self.expect_kw("ON")?;
                let table = self.name_parts()?;
                let mut cols = vec![];
                if self.accept_op("(") {
                    loop {
                        let c = self.ident()?;
                        let desc = self.accept_kw("DESC");
                        self.accept_kw("ASC");
                        cols.push((c, desc));
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                }
                let mut xml = xml_primary.map(|primary| XmlIndexSpec { primary, using: None, secondary: None });
                if xml.is_some() && self.accept_kw("USING") {
                    self.expect_kw("XML")?;
                    self.expect_kw("INDEX")?;
                    let using = self.ident()?;
                    let mut secondary = None;
                    if self.accept_kw("FOR") {
                        secondary = match self.advance_up().as_str() {
                            "VALUE" => Some('V'),
                            "PATH" => Some('P'),
                            "PROPERTY" => Some('R'),
                            _ => return Err(self.err()),
                        };
                    }
                    if let Some(x) = xml.as_mut() {
                        x.using = Some(using);
                        x.secondary = secondary;
                    }
                }
                let mut spatial = if spatial_kw { Some(SpatialIndexSpec::default()) } else { None };
                if let Some(sp) = spatial.as_mut() {
                    if self.accept_kw("USING") {
                        sp.tessellation = Some(self.ident()?.to_uppercase());
                    }
                }
                let (mut include, mut filter) = (vec![], None);
                while self.is_kw_any(&["INCLUDE", "WHERE", "WITH", "ON"]) {
                    let w = self.advance_up();
                    if w == "WITH" && self.is_op("(") && spatial.is_some() {
                        self.spatial_options(spatial.as_mut().unwrap())?;
                    } else if w == "WHERE" {
                        filter = Some(self.expr_text()?);
                    } else if w == "INCLUDE" && self.is_op("(") {
                        self.i += 1;
                        loop {
                            include.push(self.ident()?);
                            if !self.accept_op(",") {
                                break;
                            }
                        }
                        self.expect_op(")")?;
                    } else if self.is_op("(") {
                        self.skip_parens()?;
                    } else {
                        self.i += 1;
                    }
                }
                Ok(Stmt::CreateIndex { name, table, cols, unique, include, filter, xml, spatial })
            }
            "VIEW" => {
                let name = self.name_parts()?;
                let cols = if self.is_op("(") { Some(self.col_list()?) } else { None };
                self.skip_with_options();
                self.expect_kw("AS")?;
                let q = self.query()?;
                let check_option = self.is_kw("WITH") && self.peek_kw(1, &["CHECK"]) && self.peek_kw(2, &["OPTION"]);
                if check_option {
                    self.i += 3;
                }
                Ok(Stmt::CreateView { name, cols, query: Rc::new(q), text: self.src[start..].trim_end_matches(|c: char| c == ';' || c.is_whitespace()).to_string(), or_alter, alter, check_option })
            }
            "PROC" | "PROCEDURE" => {
                let name = self.name_parts()?;
                let params = self.param_list(self.is_op("("))?;
                self.skip_with_options();
                if self.is_kw("FOR") && self.peek_kw(1, &["REPLICATION"]) {
                    self.i += 2;
                }
                self.expect_kw("AS")?;
                let body = self.rest_body()?;
                Ok(Stmt::CreateProc { name, params, body: Rc::new(body), text: self.src[start..].to_string(), or_alter, alter })
            }
            "FUNCTION" => self.create_function(or_alter, start),
            "TRIGGER" => {
                let name = self.name_parts()?;
                self.expect_kw("ON")?;
                if let Some(scope) = self.trigger_scope_kw() {
                    self.skip_with_options();
                    if !self.accept_kw("AFTER") {
                        self.expect_kw("FOR")?;
                    }
                    let mut events = vec![self.ident()?.to_uppercase()];
                    while self.accept_op(",") {
                        events.push(self.ident()?.to_uppercase());
                    }
                    self.expect_kw("AS")?;
                    let body = self.rest_body()?;
                    let act = TriggerAct::Create { events, body: Rc::new(body), text: self.src[start..].to_string(), or_alter: or_alter || alter };
                    return Ok(Stmt::TriggerAdmin(Box::new(TriggerAdmin { names: vec![name], all: false, scope, act })));
                }
                let table = self.name_parts()?;
                self.skip_with_options();
                let mut instead = false;
                if self.accept_kw("INSTEAD") {
                    self.expect_kw("OF")?;
                    instead = true;
                } else if !self.accept_kw("AFTER") {
                    self.expect_kw("FOR")?;
                }
                let mut events = vec![self.advance_up()];
                while self.accept_op(",") {
                    events.push(self.advance_up());
                }
                if self.accept_kw("WITH") {
                    self.i += 1;
                }
                // NOT FOR REPLICATION: accepted, there is no replication
                if self.is_kw("NOT") && self.peek_kw(1, &["FOR"]) && self.peek_kw(2, &["REPLICATION"]) {
                    self.i += 3;
                }
                self.expect_kw("AS")?;
                let body = self.rest_body()?;
                Ok(Stmt::CreateTrigger { name, table, events, instead, body: Rc::new(body), or_alter: or_alter || alter, text: self.src[start..].to_string() })
            }
            "SECURITY" => {
                self.expect_kw("POLICY")?;
                Ok(Stmt::CreateSecurityPolicy(Box::new(self.policy_body()?)))
            }
            "SCHEMA" => {
                let name = self.ident()?;
                let owner = if self.accept_kw("AUTHORIZATION") { Some(self.ident()?) } else { None };
                Ok(Stmt::CreateSchema(name, owner))
            }
            "DATABASE" => {
                let name = self.ident()?;
                let mut collation = None;
                while !self.at_eos() && !self.is_stmt_start() {
                    if self.accept_kw("COLLATE") {
                        collation = Some(self.ident()?);
                    } else {
                        self.i += 1;
                    }
                }
                Ok(Stmt::CreateDatabase(name, collation))
            }
            "TYPE" => {
                let name = self.name_parts()?;
                if self.accept_kw("FROM") {
                    let base = self.parse_type()?;
                    let mut nullable = None;
                    if self.accept_kw("NOT") {
                        self.expect_kw("NULL")?;
                        nullable = Some(false);
                    } else if self.accept_kw("NULL") {
                        nullable = Some(true);
                    }
                    return Ok(Stmt::CreateType { name, base: Some(base), nullable, table: None });
                }
                self.expect_kw("AS")?;
                self.expect_kw("TABLE")?;
                Ok(Stmt::CreateType { name, base: None, nullable: None, table: Some(self.table_body()?) })
            }
            "SEQUENCE" => {
                let name = self.name_parts()?;
                let opts = self.sequence_opts()?;
                Ok(Stmt::CreateSequence { name, opts })
            }
            "LOGIN" => {
                let name = self.ident()?;
                let (mut password, mut default_db) = (None, None);
                let mut opts = LoginOpts::default();
                if self.accept_kw("FROM") {
                    if self.accept_kw("WINDOWS") {
                        opts.external = Some("WINDOWS".into());
                    } else {
                        self.expect_kw("EXTERNAL")?;
                        self.expect_kw("PROVIDER")?;
                        opts.external = Some("EXTERNAL".into());
                    }
                }
                if self.accept_kw("WITH") {
                    loop {
                        self.login_option(&mut password, &mut default_db, &mut opts)?;
                        while self.is_kw_any(&["MUST_CHANGE", "UNLOCK"]) {
                            self.login_option(&mut password, &mut default_db, &mut opts)?;
                        }
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                }
                Ok(Stmt::Security(Box::new(SecStmt::CreateLogin { name, password, default_db, opts })))
            }
            "USER" => {
                let name = self.ident()?;
                let mut login = None;
                let mut default_schema = None;
                if self.is_kw_any(&["FOR", "FROM"]) {
                    self.i += 1;
                    self.expect_kw("LOGIN")?;
                    login = Some(self.ident()?);
                } else if self.is_kw("WITHOUT") {
                    self.i += 1;
                    self.expect_kw("LOGIN")?;
                }
                if self.accept_kw("WITH") {
                    loop {
                        let opt = self.ident()?.to_uppercase();
                        self.expect_op("=")?;
                        let v = self.advance();
                        let val = match v.tok {
                            Tok::Str(s) | Tok::Id(s) | Tok::QId(s) => s,
                            _ => String::new(),
                        };
                        if opt == "DEFAULT_SCHEMA" {
                            default_schema = Some(val);
                        }
                        if !self.accept_op(",") {
                            break;
                        }
                    }
                }
                Ok(Stmt::Security(Box::new(SecStmt::CreateUser { name, login, default_schema })))
            }
            "ROLE" => {
                let name = self.ident()?;
                if self.accept_kw("AUTHORIZATION") {
                    self.ident()?;
                }
                Ok(Stmt::Security(Box::new(SecStmt::CreateRole { name })))
            }
            "SYNONYM" => {
                let name = self.name_parts()?;
                self.expect_kw("FOR")?;
                let target = self.name_parts()?;
                Ok(Stmt::CreateSynonym { name, target })
            }
            "STATISTICS" | "STATS" => {
                while !self.at_eos() && !self.is_stmt_start() {
                    self.i += 1;
                }
                Ok(Stmt::Noop)
            }
            _ => Err(self.err_at(&t)),
        }
    }

    fn sequence_opts(&mut self) -> R<SequenceOpts> {
        let mut o = SequenceOpts { ty: None, start: None, inc: None, min: None, max: None, cycle: None, restart: false };
        loop {
            if self.accept_kw("AS") {
                o.ty = Some(self.parse_type()?);
            } else if self.is_kw("START") && self.peek_kw(1, &["WITH"]) {
                self.i += 2;
                o.start = Some(self.expr()?);
            } else if self.is_kw("INCREMENT") && self.peek_kw(1, &["BY"]) {
                self.i += 2;
                o.inc = Some(self.expr()?);
            } else if self.accept_kw("MINVALUE") {
                o.min = Some(self.expr()?);
            } else if self.accept_kw("MAXVALUE") {
                o.max = Some(self.expr()?);
            } else if self.accept_kw("RESTART") {
                o.restart = true;
                if self.accept_kw("WITH") {
                    o.start = Some(self.expr()?);
                }
            } else if self.is_kw("NO") && self.peek_kw(1, &["MINVALUE", "MAXVALUE", "CYCLE", "CACHE"]) {
                self.i += 1;
                if self.advance_up() == "CYCLE" {
                    o.cycle = Some(false);
                }
            } else if self.accept_kw("CYCLE") {
                o.cycle = Some(true);
            } else if self.accept_kw("CACHE") {
                if matches!(self.tok().tok, Tok::Int(_)) {
                    self.i += 1;
                }
            } else {
                break;
            }
        }
        Ok(o)
    }

    fn create_function(&mut self, or_alter: bool, start: usize) -> R<Stmt> {
        let name = self.name_parts()?;
        let params = self.param_list(true)?;
        self.expect_kw("RETURNS")?;
        let (mut ret_type, mut ret_table, mut ret_var, mut inline_flag) = (None, None, None, false);
        if self.accept_kw("TABLE") {
            inline_flag = true;
        } else if let Some(v) = self.var_tok() {
            self.i += 1;
            ret_var = Some(v.to_lowercase());
            self.expect_kw("TABLE")?;
            ret_table = Some(self.table_body()?);
        } else {
            ret_type = Some(self.parse_type()?);
        }
        self.skip_with_options();
        self.accept_kw("AS");
        let text = self.src[start..].to_string();
        if inline_flag {
            self.expect_kw("RETURN")?;
            let q = if self.accept_op("(") {
                let q = self.query()?;
                self.expect_op(")")?;
                q
            } else {
                self.query()?
            };
            return Ok(Stmt::CreateFunction(Box::new(CreateFunction { name, params, ret_type: None, ret_table: None, ret_var: None, inline: Some(Rc::new(q)), body: Rc::new(vec![]), or_alter, text })));
        }
        self.expect_kw("BEGIN")?;
        let body = self.block_until_end()?;
        self.expect_kw("END")?;
        Ok(Stmt::CreateFunction(Box::new(CreateFunction { name, params, ret_type, ret_table, ret_var, inline: None, body: Rc::new(body), or_alter, text })))
    }

    // ---- table definitions
    fn table_body(&mut self) -> R<Vec<TableDef>> {
        self.expect_op("(")?;
        let mut defs = vec![];
        loop {
            defs.push(self.table_def()?);
            if !self.accept_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(defs)
    }

    fn col_list_ord(&mut self) -> R<Vec<(String, bool)>> {
        self.expect_op("(")?;
        let mut cols = vec![];
        loop {
            let c = self.ident()?;
            let desc = self.accept_kw("DESC");
            self.accept_kw("ASC");
            cols.push((c, desc));
            if !self.accept_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(cols)
    }

    fn skip_index_opts(&mut self) -> R<()> {
        while self.is_kw_any(&["WITH", "ON"]) {
            self.i += 1;
            if self.is_op("(") {
                self.skip_parens()?;
            } else {
                self.i += 1;
            }
        }
        Ok(())
    }

    fn references(&mut self) -> R<ForeignKeyDef> {
        self.expect_kw("REFERENCES")?;
        let table = self.name_parts()?;
        let rcols = if self.is_op("(") { Some(self.col_list()?) } else { None };
        let (mut on_delete, mut on_update) = ("NO ACTION".to_string(), "NO ACTION".to_string());
        while self.is_kw("ON") && self.peek_kw(1, &["DELETE", "UPDATE"]) {
            self.i += 1;
            let which = self.advance_up();
            let act = if self.accept_kw("NO") {
                self.expect_kw("ACTION")?;
                "NO ACTION".to_string()
            } else if self.accept_kw("SET") {
                format!("SET {}", self.advance_up())
            } else {
                self.advance_up()
            };
            if which == "DELETE" {
                on_delete = act;
            } else {
                on_update = act;
            }
        }
        if self.is_kw("NOT") && self.peek_kw(1, &["FOR"]) {
            self.i += 3;
        }
        Ok(ForeignKeyDef { name: None, cols: vec![], table, rcols, on_delete, on_update })
    }

    fn table_def(&mut self) -> R<TableDef> {
        let mut cname = None;
        if self.accept_kw("CONSTRAINT") {
            cname = Some(self.ident()?);
        }
        if self.is_kw("PERIOD") && self.peek_kw(1, &["FOR"]) {
            self.i += 2;
            self.expect_kw("SYSTEM_TIME")?;
            let cols = self.col_list()?;
            if cols.len() != 2 {
                return Err(self.err());
            }
            return Ok(TableDef::Period { start: cols[0].clone(), end: cols[1].clone() });
        }
        if self.is_kw("PRIMARY") && self.peek_kw(1, &["KEY"]) {
            self.i += 2;
            if self.is_kw_any(&["CLUSTERED", "NONCLUSTERED"]) {
                self.i += 1;
            }
            let cols = self.col_list_ord()?;
            self.skip_index_opts()?;
            return Ok(TableDef::PrimaryKey { name: cname, cols });
        }
        if self.is_kw("UNIQUE") {
            self.i += 1;
            if self.is_kw_any(&["CLUSTERED", "NONCLUSTERED"]) {
                self.i += 1;
            }
            let cols = self.col_list_ord()?;
            self.skip_index_opts()?;
            return Ok(TableDef::Unique { name: cname, cols });
        }
        if self.is_kw("CHECK") {
            self.i += 1;
            self.expect_op("(")?;
            let (e, text) = self.expr_text()?;
            self.expect_op(")")?;
            return Ok(TableDef::Check { name: cname, e, text });
        }
        if self.is_kw("FOREIGN") {
            self.i += 1;
            self.expect_kw("KEY")?;
            let cols: Vec<String> = self.col_list_ord()?.into_iter().map(|x| x.0).collect();
            let mut fk = self.references()?;
            fk.name = cname;
            fk.cols = cols;
            return Ok(TableDef::ForeignKey(fk));
        }
        if self.is_kw("DEFAULT") && cname.is_some() {
            self.i += 1;
            let (e, text) = self.expr_text()?;
            self.expect_kw("FOR")?;
            let col = self.ident()?;
            return Ok(TableDef::DefaultFor { name: cname.unwrap(), e, col, text });
        }
        if self.is_kw("INDEX") {
            self.i += 1;
            let name = self.ident()?;
            if self.is_kw_any(&["CLUSTERED", "NONCLUSTERED"]) {
                self.i += 1;
            }
            let cols = self.col_list_ord()?;
            return Ok(TableDef::Index { name, cols, unique: false });
        }
        self.column_def()
    }

    fn column_def(&mut self) -> R<TableDef> {
        let name = self.ident()?;
        let mut col = ColumnDef { name, ty: None, nullable: None, identity: None, default: None, default_text: None, default_name: None, computed: None, computed_text: None, pk: None, unique: None, checks: vec![], fk: None, collation: None, mask: None, generated: None };
        if self.accept_kw("AS") {
            let (e, t) = self.expr_text()?;
            col.computed = Some(e);
            col.computed_text = Some(t);
            self.accept_kw("PERSISTED");
            if self.is_kw("NOT") && self.peek_kw(1, &["NULL"]) {
                self.i += 2;
            }
        } else {
            col.ty = Some(self.parse_type()?);
        }
        loop {
            let mut cname = None;
            if self.accept_kw("CONSTRAINT") {
                cname = Some(self.ident()?);
            }
            if self.is_kw("NOT") && self.peek_kw(1, &["NULL"]) {
                self.i += 2;
                col.nullable = Some(false);
            } else if self.accept_kw("NULL") {
                col.nullable = Some(true);
            } else if self.accept_kw("IDENTITY") {
                let (mut seed, mut inc) = (Expr::Lit(Value::Int(1)), Expr::Lit(Value::Int(1)));
                if self.accept_op("(") {
                    seed = self.expr()?;
                    self.expect_op(",")?;
                    inc = self.expr()?;
                    self.expect_op(")")?;
                }
                col.identity = Some((seed, inc));
                if self.is_kw("NOT") && self.peek_kw(1, &["FOR"]) {
                    self.i += 3;
                }
            } else if self.accept_kw("DEFAULT") {
                let (e, t) = self.expr_text()?;
                col.default = Some(e);
                col.default_text = Some(t);
                col.default_name = cname;
            } else if self.is_kw("PRIMARY") && self.peek_kw(1, &["KEY"]) {
                self.i += 2;
                if self.is_kw_any(&["CLUSTERED", "NONCLUSTERED"]) {
                    self.i += 1;
                }
                col.pk = Some(cname);
                self.skip_index_opts()?;
            } else if self.accept_kw("UNIQUE") {
                if self.is_kw_any(&["CLUSTERED", "NONCLUSTERED"]) {
                    self.i += 1;
                }
                col.unique = Some(cname);
            } else if self.accept_kw("CHECK") {
                self.expect_op("(")?;
                let (e, t) = self.expr_text()?;
                self.expect_op(")")?;
                col.checks.push((cname, e, t));
            } else if self.is_kw("REFERENCES") || (self.is_kw("FOREIGN") && self.peek_kw(1, &["KEY"]) && self.peek_kw(2, &["REFERENCES"])) {
                // a column-level foreign key may be written with or without FOREIGN KEY
                if self.is_kw("FOREIGN") {
                    self.i += 2;
                }
                let mut fk = self.references()?;
                fk.name = cname;
                col.fk = Some(fk);
            } else if self.accept_kw("COLLATE") {
                col.collation = Some(self.ident()?);
            } else if self.is_kw("GENERATED") || (self.is_kw("AS") && self.peek_kw(1, &["ROW"])) {
                if self.accept_kw("GENERATED") {
                    self.expect_kw("ALWAYS")?;
                }
                self.expect_kw("AS")?;
                self.expect_kw("ROW")?;
                let start = if self.accept_kw("START") {
                    true
                } else {
                    self.expect_kw("END")?;
                    false
                };
                let hidden = self.accept_kw("HIDDEN");
                col.generated = Some((start, hidden));
            } else if self.is_kw("HIDDEN") {
                self.i += 1;
                if let Some((s, _)) = col.generated {
                    col.generated = Some((s, true));
                }
            } else if self.is_kw("MASKED") {
                col.mask = Some(self.masked_with()?);
            } else if self.is_kw_any(&["SPARSE", "ROWGUIDCOL", "FILESTREAM", "PERSISTED", "CLUSTERED", "NONCLUSTERED"]) {
                self.i += 1;
            } else {
                break;
            }
        }
        Ok(TableDef::Column(col))
    }

    /// `( option [= value] , ... )` of a CREATE TABLE ... WITH: `SYSTEM_VERSIONING` is returned, the rest skipped.
    fn table_with_options(&mut self) -> R<Option<Versioning>> {
        self.expect_op("(")?;
        let mut out = None;
        loop {
            if self.is_kw("SYSTEM_VERSIONING") {
                self.i += 1;
                out = Some(self.versioning_value()?);
            } else {
                let mut depth = 0;
                while !self.is_eof() {
                    if self.is_op("(") {
                        depth += 1;
                    } else if self.is_op(")") {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    } else if self.is_op(",") && depth == 0 {
                        break;
                    }
                    self.i += 1;
                }
            }
            if !self.accept_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(out)
    }

    /// `= ON [( HISTORY_TABLE = name [, ...] )]` or `= OFF`.
    fn versioning_value(&mut self) -> R<Versioning> {
        self.expect_op("=")?;
        let on = if self.accept_kw("ON") {
            true
        } else {
            self.expect_kw("OFF")?;
            false
        };
        let mut history = None;
        if self.is_op("(") {
            self.i += 1;
            while !self.is_op(")") {
                if self.is_eof() {
                    return Err(self.err());
                }
                if self.accept_kw("HISTORY_TABLE") {
                    self.expect_op("=")?;
                    history = Some(self.name_parts()?);
                } else {
                    self.i += 1;
                }
            }
            self.expect_op(")")?;
        }
        Ok(Versioning { on, history })
    }

    /// `FOR SYSTEM_TIME ...` after a table name.
    fn temporal_clause(&mut self) -> R<TemporalClause> {
        self.expect_kw("FOR")?;
        self.expect_kw("SYSTEM_TIME")?;
        if self.accept_kw("ALL") {
            return Ok(TemporalClause { kind: TemporalKind::All, a: None, b: None });
        }
        if self.accept_kw("AS") {
            self.expect_kw("OF")?;
            let a = self.expr()?;
            return Ok(TemporalClause { kind: TemporalKind::AsOf, a: Some(a), b: None });
        }
        if self.accept_kw("FROM") {
            let a = self.expr()?;
            self.expect_kw("TO")?;
            let b = self.expr()?;
            return Ok(TemporalClause { kind: TemporalKind::FromTo, a: Some(a), b: Some(b) });
        }
        if self.accept_kw("BETWEEN") {
            let a = self.add_expr()?;
            self.expect_kw("AND")?;
            let b = self.expr()?;
            return Ok(TemporalClause { kind: TemporalKind::Between, a: Some(a), b: Some(b) });
        }
        self.expect_kw("CONTAINED")?;
        self.expect_kw("IN")?;
        self.expect_op("(")?;
        let a = self.expr()?;
        self.expect_op(",")?;
        let b = self.expr()?;
        self.expect_op(")")?;
        Ok(TemporalClause { kind: TemporalKind::ContainedIn, a: Some(a), b: Some(b) })
    }

    fn alter_table(&mut self) -> R<Stmt> {
        self.i += 1;
        let name = self.name_parts()?;
        if self.accept_kw("ADD") {
            let mut defs = vec![];
            loop {
                defs.push(self.table_def()?);
                if !self.accept_op(",") {
                    break;
                }
            }
            return Ok(Stmt::AlterTable { name, action: AlterAction::AddDefs(defs) });
        }
        if self.accept_kw("DROP") {
            let what = self.advance_up();
            let mut if_ex = false;
            if self.accept_kw("IF") {
                self.expect_kw("EXISTS")?;
                if_ex = true;
            }
            let mut items = vec![self.ident()?];
            while self.accept_op(",") {
                items.push(self.ident()?);
            }
            return match what.as_str() {
                "CONSTRAINT" => Ok(Stmt::AlterTable { name, action: AlterAction::DropConstraint(items, if_ex) }),
                "COLUMN" => Ok(Stmt::AlterTable { name, action: AlterAction::DropColumn(items, if_ex) }),
                _ => Err(self.err()),
            };
        }
        if self.accept_kw("ALTER") {
            self.expect_kw("COLUMN")?;
            let col = self.ident()?;
            if self.accept_kw("ADD") {
                let mask = self.masked_with()?;
                return Ok(Stmt::AlterTable { name, action: AlterAction::AlterMask { col, mask: Some(mask) } });
            }
            if self.accept_kw("DROP") {
                self.expect_kw("MASKED")?;
                return Ok(Stmt::AlterTable { name, action: AlterAction::AlterMask { col, mask: None } });
            }
            let ty = self.parse_type()?;
            let (mut nullable, mut collation) = (None, None);
            loop {
                if self.accept_kw("COLLATE") {
                    collation = Some(self.ident()?);
                } else if self.accept_kw("NOT") {
                    self.expect_kw("NULL")?;
                    nullable = Some(false);
                } else if self.accept_kw("NULL") {
                    nullable = Some(true);
                } else {
                    break;
                }
            }
            return Ok(Stmt::AlterTable { name, action: AlterAction::AlterColumn { col, ty, nullable, collation } });
        }
        if self.is_kw_any(&["NOCHECK", "CHECK", "WITH"]) {
            if self.accept_kw("WITH") {
                self.advance();
            }
            let enable = self.advance_up() == "CHECK";
            self.expect_kw("CONSTRAINT")?;
            let cn = if self.accept_kw("ALL") { None } else { Some(self.ident()?) };
            return Ok(Stmt::AlterTable { name, action: AlterAction::CheckConstraint { name: cn, enable } });
        }
        if self.is_kw("SET") && self.peek_is_op(1, "(") && self.peek_kw(2, &["SYSTEM_VERSIONING"]) {
            self.i += 3;
            let v = self.versioning_value()?;
            // further options inside SET ( ... ) are skipped
            while !self.is_op(")") && !self.is_eof() {
                self.i += 1;
            }
            self.expect_op(")")?;
            return Ok(Stmt::AlterTable { name, action: AlterAction::SetVersioning(v) });
        }
        if self.is_kw_any(&["ENABLE", "DISABLE", "REBUILD", "SWITCH", "SET"]) {
            while !self.at_eos() && !self.is_stmt_start() {
                self.i += 1;
            }
            return Ok(Stmt::Noop);
        }
        Err(self.err())
    }
}

#[allow(dead_code)]
fn _unused(_: Dec) {}
