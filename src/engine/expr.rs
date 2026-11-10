//! Scalar expression evaluation (three-valued logic).
use super::query::{is_agg_name, Env};
use super::*;
use crate::ast::BinOp;
use crate::convert::{arith, compare, negate, to_str};
use std::cmp::Ordering;

fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bit(b) => Ok(Some(*b)),
        _ => Err(SqlError::new(4145, "An expression of non-boolean type specified in a context where a condition is expected.")),
    }
}
fn tv(b: Option<bool>) -> Value {
    match b {
        Some(x) => Value::Bit(x),
        None => Value::Null,
    }
}

/// Functions whose string result has the collation of their (first string) argument.
const COLLATION_PASSING: &[&str] = &["UPPER", "LOWER", "LTRIM", "RTRIM", "TRIM", "SUBSTRING", "LEFT", "RIGHT", "REPLACE", "REVERSE", "CONCAT", "ISNULL", "COALESCE", "STUFF", "REPLICATE", "NULLIF"];

impl<'a> Engine<'a> {
    /// The collation of an expression as SQL Server derives it: an explicit `COLLATE` wins; otherwise the collation of a
    /// column it is built from; literals and variables take whatever they are compared with. `None` means the
    /// built-in default (or no string involved). The flag says the collation was asked for explicitly.
    pub fn expr_coll(&self, e: &Expr, env: &Env<'_>) -> Option<(Arc<crate::collation::Collation>, bool)> {
        match e {
            Expr::Col(p) => {
                let mut cur = Some(env);
                while let Some(en) = cur {
                    match en.schema.find_parts(p) {
                        super::query::Found::One(i) => return en.schema.cols[i].coll.clone().map(|c| (c, false)),
                        super::query::Found::None => cur = en.parent,
                        _ => return None,
                    }
                }
                None
            }
            Expr::Collate(_, name) => crate::collation::get(name).map(|c| (c, true)),
            Expr::Paren(x) => self.expr_coll(x, env),
            Expr::Cast { e, .. } => self.expr_coll(e, env),
            Expr::Binary(BinOp::Add, a, b) => self.expr_coll(a, env).or_else(|| self.expr_coll(b, env)),
            Expr::Func(f) if COLLATION_PASSING.contains(&f.name.as_str()) => f.args.iter().find_map(|a| self.expr_coll(a, env)),
            _ => None,
        }
    }

    /// The default collation of the current database, if it is not the built-in one.
    fn ambient_coll(&self) -> Option<Arc<crate::collation::Collation>> {
        let db = self.srv.databases.get(&self.ses.db.to_lowercase())?;
        if db.collation == crate::collation::DEFAULT_NAME {
            return None;
        }
        crate::collation::get(&db.collation).filter(|c| !c.is_default())
    }

    /// The collation in which `a` and `b` are compared, or `None` for the built-in comparison. Two different
    /// collations that cannot be reconciled are error 468.
    pub fn pick_coll(&self, a: &Expr, b: &Expr, env: &Env<'_>) -> Result<Option<Arc<crate::collation::Collation>>> {
        if !crate::collation::in_use() {
            return Ok(None);
        }
        self.resolve_coll(self.expr_coll(a, env), self.expr_coll(b, env))
    }

    /// Settle which of two derived collations a comparison uses (see `pick_coll`).
    pub fn resolve_coll(&self, ca: Option<(Arc<crate::collation::Collation>, bool)>, cb: Option<(Arc<crate::collation::Collation>, bool)>) -> Result<Option<Arc<crate::collation::Collation>>> {
        let conflict = |x: &crate::collation::Collation, y: &crate::collation::Collation| {
            SqlError::with(468, 16, 1, format!("Cannot resolve the collation conflict between \"{}\" and \"{}\" in the equal to operation.", y.name, x.name))
        };
        Ok(match (ca, cb) {
            (Some((x, true)), Some((y, true))) | (Some((x, false)), Some((y, false))) => {
                if !x.name.eq_ignore_ascii_case(&y.name) {
                    return Err(conflict(&x, &y));
                }
                Some(x)
            }
            (Some((x, true)), _) | (_, Some((x, true))) => Some(x),
            (Some((x, false)), None) | (None, Some((x, false))) => Some(x),
            (None, None) => self.ambient_coll(),
        })
    }

    /// Compare two values the way a comparison of expressions `a` and `b` does: strings by their collation.
    pub fn cmp_exprs(&self, a: &Expr, b: &Expr, l: &Value, r: &Value, env: &Env<'_>) -> Result<Option<Ordering>> {
        if let (Value::Str(x), Value::Str(y)) = (l, r) {
            if let Some(c) = self.pick_coll(a, b, env)? {
                return Ok(Some(c.compare(x, y)));
            }
        }
        compare(l, r)
    }

    pub fn ev(&mut self, e: &Expr, env: &Env<'_>) -> Result<Value> {
        match e {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::Paren(x) => self.ev(x, env),
            Expr::Var(n) => self.get_var(n),
            Expr::SysVar(n) => self.sysvar(n),
            Expr::Col(parts) => env.lookup(parts),
            Expr::Star(_) => Err(SqlError::new(102, "Incorrect syntax near '*'.")),
            Expr::Neg(x) => {
                let v = self.ev(x, env)?;
                negate(&v)
            }
            Expr::BitNot(x) => {
                let v = self.ev(x, env)?;
                Ok(match v {
                    Value::Null => Value::Null,
                    Value::Int(i) => Value::Int(!i),
                    Value::BigInt(i) => Value::BigInt(!i),
                    Value::Bit(b) => Value::Bit(!b),
                    Value::Str(s) => return self.ev(&Expr::BitNot(Box::new(Expr::Lit(convert::str_to_number(&s, "int")?))), env),
                    o => return Err(SqlError::new(8117, format!("Operand data type {} is invalid for bitwise not operator.", o.type_label()))),
                })
            }
            Expr::Binary(op, a, b) => {
                let l = self.ev(a, env)?;
                let r = self.ev(b, env)?;
                match op {
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let o = self.cmp_exprs(a, b, &l, &r, env)?;
                        Ok(match o {
                            None => Value::Null,
                            Some(o) => Value::Bit(match op {
                                BinOp::Eq => o == Ordering::Equal,
                                BinOp::Ne => o != Ordering::Equal,
                                BinOp::Lt => o == Ordering::Less,
                                BinOp::Le => o != Ordering::Greater,
                                BinOp::Gt => o == Ordering::Greater,
                                _ => o != Ordering::Less,
                            }),
                        })
                    }
                    BinOp::Div if matches!(l, Value::Dec(_)) || matches!(r, Value::Dec(_)) => {
                        let (p1, p2) = (self.static_pscale(a, env).map(|t| t.0), self.static_pscale(b, env).map(|t| t.0));
                        convert::arith_with(*op, &l, &r, p1, p2)
                    }
                    _ => arith(*op, &l, &r),
                }
            }
            Expr::And(a, b) => {
                let l = truth(&self.ev(a, env)?)?;
                if l == Some(false) {
                    return Ok(Value::Bit(false));
                }
                let r = truth(&self.ev(b, env)?)?;
                Ok(tv(match (l, r) {
                    (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                }))
            }
            Expr::Or(a, b) => {
                let l = truth(&self.ev(a, env)?)?;
                if l == Some(true) {
                    return Ok(Value::Bit(true));
                }
                let r = truth(&self.ev(b, env)?)?;
                Ok(tv(match (l, r) {
                    (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                }))
            }
            Expr::Not(a) => {
                let v = truth(&self.ev(a, env)?)?;
                Ok(tv(v.map(|b| !b)))
            }
            Expr::IsNull(a, neg) => {
                let v = self.ev(a, env)?;
                Ok(Value::Bit(v.is_null() != *neg))
            }
            Expr::IsDistinct(a, b, neg) => {
                let l = self.ev(a, env)?;
                let r = self.ev(b, env)?;
                let same = match (l.is_null(), r.is_null()) {
                    (true, true) => true,
                    (false, false) => self.cmp_exprs(a, b, &l, &r, env)? == Some(Ordering::Equal),
                    _ => false,
                };
                // IS DISTINCT FROM => not same ; IS NOT DISTINCT FROM => same
                Ok(Value::Bit(if *neg { same } else { !same }))
            }
            Expr::InList(a, items, neg) => {
                let l = self.ev(a, env)?;
                if l.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for it in items {
                    let r = self.ev(it, env)?;
                    match self.cmp_exprs(a, it, &l, &r, env)? {
                        Some(Ordering::Equal) => return Ok(Value::Bit(!*neg)),
                        None => saw_null = true,
                        _ => {}
                    }
                }
                if saw_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Bit(*neg))
                }
            }
            Expr::InQuery(a, q, neg) => {
                let l = self.ev(a, env)?;
                let rs = self.sub_rows(q, env)?;
                if rs.schema.cols.len() != 1 {
                    return Err(SqlError::new(116, "Only one expression can be specified in the select list when the subquery is not introduced with EXISTS."));
                }
                if rs.rows.is_empty() {
                    return Ok(Value::Bit(*neg));
                }
                if l.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for r in rs.rows.iter() {
                    match compare(&l, &r[0])? {
                        Some(Ordering::Equal) => return Ok(Value::Bit(!*neg)),
                        None => saw_null = true,
                        _ => {}
                    }
                }
                if saw_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Bit(*neg))
                }
            }
            Expr::Like(a, p, esc, neg) => {
                let v = self.ev(a, env)?;
                let pat = self.ev(p, env)?;
                let esc = match esc {
                    Some(x) => Some(self.ev(x, env)?),
                    None => None,
                };
                if v.is_null() || pat.is_null() {
                    return Ok(Value::Null);
                }
                let escc = esc.as_ref().and_then(|e| if e.is_null() { None } else { to_str(e).chars().next() });
                let m = match (&v, self.pick_coll(a, p, env)?) {
                    (Value::Str(_), Some(c)) if !c.is_default() => like_match_fold(&to_str(&v), &to_str(&pat), escc, &|ch| c.fold_char(ch)),
                    _ => like_match(&to_str(&v), &to_str(&pat), escc),
                };
                Ok(Value::Bit(m != *neg))
            }
            Expr::Between(a, lo, hi, neg) => {
                let v = self.ev(a, env)?;
                let l = self.ev(lo, env)?;
                let h = self.ev(hi, env)?;
                let c1 = self.cmp_exprs(a, lo, &v, &l, env)?.map(|o| o != Ordering::Less);
                let c2 = self.cmp_exprs(a, hi, &v, &h, env)?.map(|o| o != Ordering::Greater);
                let r = match (c1, c2) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                };
                Ok(tv(r.map(|b| b != *neg)))
            }
            Expr::CmpQuant(op, all, a, q) => {
                let l = self.ev(a, env)?;
                let rs = self.sub_rows(q, env)?;
                if rs.rows.is_empty() {
                    return Ok(Value::Bit(*all));
                }
                let mut saw_null = false;
                let lcoll = self.expr_coll(a, env).map(|c| c.0);
                for r in rs.rows.iter() {
                    let o = match (&l, &r[0], &lcoll) {
                        (Value::Str(x), Value::Str(y), Some(c)) => Some(c.compare(x, y)),
                        _ => compare(&l, &r[0])?,
                    };
                    let b = o.map(|o| match op {
                        BinOp::Eq => o == Ordering::Equal,
                        BinOp::Ne => o != Ordering::Equal,
                        BinOp::Lt => o == Ordering::Less,
                        BinOp::Le => o != Ordering::Greater,
                        BinOp::Gt => o == Ordering::Greater,
                        _ => o != Ordering::Less,
                    });
                    match b {
                        None => saw_null = true,
                        Some(true) if !*all => return Ok(Value::Bit(true)),
                        Some(false) if *all => return Ok(Value::Bit(false)),
                        _ => {}
                    }
                }
                if saw_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Bit(*all))
                }
            }
            Expr::Exists(q) => {
                let rs = self.sub_rows(q, env)?;
                Ok(Value::Bit(!rs.rows.is_empty()))
            }
            Expr::Subquery(q) => {
                let rs = self.sub_rows(q, env)?;
                if rs.schema.cols.len() != 1 {
                    return Err(SqlError::new(116, "Only one expression can be specified in the select list when the subquery is not introduced with EXISTS."));
                }
                match rs.rows.len() {
                    0 => Ok(Value::Null),
                    1 => Ok(rs.rows[0][0].clone()),
                    _ => Err(SqlError::new(512, "Subquery returned more than 1 value. This is not permitted when the subquery follows =, !=, <, <= , >, >= or when the subquery is used as an expression.")),
                }
            }
            Expr::Case { operand, whens, els } => {
                let op = match operand {
                    Some(o) => Some(self.ev(o, env)?),
                    None => None,
                };
                for (c, r) in whens {
                    let hit = match &op {
                        Some(o) => {
                            let cv = self.ev(c, env)?;
                            compare(o, &cv)? == Some(Ordering::Equal)
                        }
                        None => matches!(self.ev(c, env)?, Value::Bit(true)),
                    };
                    if hit {
                        return self.ev(r, env);
                    }
                }
                match els {
                    Some(e) => self.ev(e, env),
                    None => Ok(Value::Null),
                }
            }
            Expr::Cast { e, ty, try_, style } => {
                let v = self.ev(e, env)?;
                let st = match style {
                    Some(s) => {
                        let sv = self.ev(s, env)?;
                        sv.as_i64().map(|x| x as i32)
                    }
                    None => None,
                };
                let dt = self.resolve_type_cast(ty)?;
                match convert::cast(&v, &dt, st) {
                    Ok(x) => Ok(x),
                    Err(err) if *try_ && matches!(err.number, 245 | 241 | 242 | 8114 | 8115 | 220 | 8169 | 8152 | 8116) => Ok(Value::Null),
                    Err(err) => Err(err),
                }
            }
            Expr::Collate(x, name) => {
                if crate::collation::get(name).is_none() {
                    return Err(SqlError::with(448, 16, 1, format!("Invalid collation '{}'.", name)));
                }
                self.ev(x, env)
            }
            Expr::AtTimeZone(x, z) => {
                let v = self.ev(x, env)?;
                let zone = self.ev(z, env)?;
                funcs::at_time_zone(&v, &zone)
            }
            Expr::NextValue(name) => self.next_value(name),
            Expr::Default => Err(SqlError::new(8101, "DEFAULT is not allowed in this context.")),
            Expr::IdentityFn { .. } => Err(SqlError::new(177, "The IDENTITY function can only be used when the SELECT statement has an INTO clause.")),
            Expr::Func(fc) => self.eval_func(fc, env),
        }
    }

    /// Run a subquery for the row in `env`. One that does not use the enclosing row gives the same rows every time
    /// within a statement, so its result is kept (a correlated subquery would otherwise be run once per outer row, and
    /// an uncorrelated one is as likely as not to be a full scan). Whether it is correlated is found by running it
    /// once without the enclosing row: if that works, no outer column was evaluated; if it fails because a column is
    /// unknown, it is correlated and runs per row as before.
    fn sub_rows(&mut self, q: &Query, env: &Env<'_>) -> Result<super::query::Rowset> {
        let key = q as *const Query as usize;
        if self.ses.sub_cache.is_none() {
            return self.exec_query(q, Some(env));
        }
        match self.ses.sub_cache.as_ref().and_then(|c| c.get(&key)) {
            Some(Some(rs)) => return Ok(rs.clone()),
            Some(None) => return self.exec_query(q, Some(env)),
            None => {}
        }
        match self.exec_query(q, None) {
            Ok(rs) => {
                if let Some(c) = self.ses.sub_cache.as_mut() {
                    c.insert(key, Some(rs.clone()));
                }
                Ok(rs)
            }
            Err(e) if e.number == 207 || e.number == 4104 => {
                if let Some(c) = self.ses.sub_cache.as_mut() {
                    c.insert(key, None);
                }
                self.exec_query(q, Some(env))
            }
            Err(e) => Err(e),
        }
    }

    /// The declared (precision, scale) of a numeric expression, worked out as SQL Server does from the types of columns,
    /// variables, casts and literals and the rules for the result of `+ - * / %`; `None` when it cannot be told.
    fn static_pscale(&self, e: &Expr, env: &Env<'_>) -> Option<(u32, u32)> {
        fn of_type(t: &DataType) -> Option<(u32, u32)> {
            match t {
                DataType::Decimal(p, s) => Some((*p as u32, *s as u32)),
                DataType::Int => Some((10, 0)),
                DataType::BigInt => Some((19, 0)),
                DataType::SmallInt => Some((5, 0)),
                DataType::TinyInt => Some((3, 0)),
                DataType::Bit => Some((1, 0)),
                DataType::Money => Some((19, 4)),
                DataType::SmallMoney => Some((10, 4)),
                _ => None,
            }
        }
        // a result wider than 38 digits keeps the integer digits and gives up scale (down to 6)
        fn cap(p: u32, s: u32) -> (u32, u32) {
            if p > 38 {
                (38, s.saturating_sub(p - 38).max(s.min(6)))
            } else {
                (p, s)
            }
        }
        match e {
            Expr::Paren(x) | Expr::Neg(x) => self.static_pscale(x, env),
            Expr::Lit(Value::Dec(d)) => Some((d.precision().max(d.s as u32), d.s as u32)),
            Expr::Lit(Value::Int(_)) => Some((10, 0)),
            Expr::Lit(Value::BigInt(_)) => Some((19, 0)),
            Expr::Col(p) => match env.schema.find_parts(p) {
                super::query::Found::One(i) => env.schema.cols[i].ty.as_ref().and_then(of_type),
                _ => env.parent.and_then(|par| self.static_pscale(e, par)),
            },
            Expr::Var(n) => self.ses.frames.last().and_then(|f| f.vars.get(n)).and_then(|v| of_type(&v.ty)),
            Expr::Cast { ty, .. } => ty.parts.last().and_then(|n| DataType::from_name(n, &ty.args, false).ok()).and_then(|t| of_type(&t)),
            Expr::Func(fc) if fc.name == "ROUND" || fc.name == "ABS" || fc.name == "CEILING" || fc.name == "FLOOR" => fc.args.first().and_then(|a| self.static_pscale(a, env)),
            Expr::Binary(op, l, r) => {
                let ((p1, s1), (p2, s2)) = (self.static_pscale(l, env)?, self.static_pscale(r, env)?);
                Some(match op {
                    BinOp::Add | BinOp::Sub => {
                        let s = s1.max(s2);
                        cap(((p1 - s1).max(p2 - s2)) + s + 1, s)
                    }
                    BinOp::Mul => cap(p1 + p2 + 1, s1 + s2),
                    BinOp::Div => {
                        let s = 6.max(s1 + p2 + 1);
                        cap(p1 - s1 + s2 + s, s)
                    }
                    BinOp::Mod => {
                        let s = s1.max(s2);
                        ((p1 - s1).min(p2 - s2) + s, s)
                    }
                    _ => return None,
                })
            }
            _ => None,
        }
    }

    /// Is every leaf of this expression a literal (so a decimal in it is `numeric`, as SQL Server types a number written
    /// out, not `decimal`)?
    fn only_literals(&self, e: &Expr) -> bool {
        match e {
            Expr::Lit(_) => true,
            Expr::Paren(x) | Expr::Neg(x) => self.only_literals(x),
            Expr::Binary(_, l, r) => self.only_literals(l) && self.only_literals(r),
            Expr::Cast { ty, .. } => ty.parts.last().map(|n| n.eq_ignore_ascii_case("numeric")).unwrap_or(false),
            _ => false,
        }
    }

    /// Is this expression known to be NCHAR / NVARCHAR (a column, variable or CAST of that type, or a string function of one)?
    fn is_unicode_expr(&self, e: &Expr, env: &Env<'_>) -> bool {
        let wide_type = |t: &DataType| matches!(t, DataType::NChar(_) | DataType::NVarChar(_) | DataType::NText);
        match e {
            Expr::Paren(x) => self.is_unicode_expr(x, env),
            Expr::Binary(BinOp::Add, l, r) => self.is_unicode_expr(l, env) || self.is_unicode_expr(r, env),
            Expr::Col(p) => match env.schema.find_parts(p) {
                super::query::Found::One(i) => env.schema.cols[i].ty.as_ref().map(wide_type).unwrap_or(false),
                _ => match env.parent {
                    Some(par) => self.is_unicode_expr(e, par),
                    None => false,
                },
            },
            Expr::Var(n) => self.ses.frames.last().and_then(|f| f.vars.get(n)).map(|v| wide_type(&v.ty)).unwrap_or(false),
            Expr::Cast { ty, .. } => ty.parts.last().and_then(|n| DataType::from_name(n, &ty.args, false).ok()).map(|t| wide_type(&t)).unwrap_or(false),
            Expr::Func(fc) => match fc.name.as_str() {
                "NCHAR" | "QUOTENAME" => true,
                "UPPER" | "LOWER" | "LTRIM" | "RTRIM" | "TRIM" | "REPLACE" | "SUBSTRING" | "LEFT" | "RIGHT" | "REVERSE" | "STUFF" | "REPLICATE" | "ISNULL" | "COALESCE" => fc.args.first().map(|a| self.is_unicode_expr(a, env)).unwrap_or(false),
                _ => false,
            },
            _ => false,
        }
    }

    fn eval_func(&mut self, fc: &FuncCall, env: &Env<'_>) -> Result<Value> {
        if fc.over.is_some() {
            let key = fc as *const FuncCall as usize;
            return match env.win.and_then(|w| w.get(&key)) {
                Some(v) => Ok(v.clone()),
                None => Err(SqlError::new(4108, "Windowed functions can only appear in the SELECT or ORDER BY clauses.")),
            };
        }
        let name = fc.name.as_str();
        if name == "SQL_VARIANT_PROPERTY" && fc.args.len() == 2 && fc.schema.is_none() {
            let v = self.ev(&fc.args[0], env)?;
            let prop = self.ev(&fc.args[1], env)?;
            let prop_name = convert::to_str(&prop).to_lowercase();
            match (&v, prop_name.as_str()) {
                (Value::Str(_), "basetype") => return Ok(Value::Str(if self.is_unicode_expr(&fc.args[0], env) { "nvarchar" } else { "varchar" }.to_string())),
                (Value::Dec(_), "basetype") => return Ok(Value::Str(if self.only_literals(&fc.args[0]) { "numeric" } else { "decimal" }.to_string())),
                (Value::Dec(_), "precision") => {
                    if let Some((p, _)) = self.static_pscale(&fc.args[0], env) {
                        return Ok(Value::Int(p as i32));
                    }
                }
                (Value::Dec(_), "scale") => {
                    if let Some((_, s)) = self.static_pscale(&fc.args[0], env) {
                        return Ok(Value::Int(s as i32));
                    }
                }
                _ => {}
            }
            return Ok(self.builtin("SQL_VARIANT_PROPERTY", vec![v, prop], fc)?.unwrap_or(Value::Null));
        }
        if name == "DATALENGTH" && fc.args.len() == 1 && fc.schema.is_none() {
            // the bytes of a string depend on its type (two per character for NCHAR / NVARCHAR), which the value does not carry
            let v = self.ev(&fc.args[0], env)?;
            if let Value::Str(t) = &v {
                let wide = t.chars().any(|c| c as u32 > 0xFF) || self.is_unicode_expr(&fc.args[0], env);
                let units = t.encode_utf16().count() as i32;
                return Ok(Value::Int(if wide { units * 2 } else { t.chars().count() as i32 }));
            }
            return Ok(self.builtin("DATALENGTH", vec![v], fc)?.unwrap_or(Value::Null));
        }
        if fc.schema.is_none() || fc.schema.as_deref().map(|s| s.eq_ignore_ascii_case("sys")).unwrap_or(false) {
            if is_agg_name(name) {
                return self.eval_aggregate(fc, env);
            }
            if name == "GROUPING" {
                let g = env.group.ok_or_else(|| SqlError::new(8, "GROUPING used outside aggregate query"))?;
                let target = format!("{:?}", fc.args[0]);
                let idx = g.base.iter().position(|b| *b == target);
                return Ok(Value::Int(match idx {
                    Some(i) if g.excluded[i] => 1,
                    _ => 0,
                }));
            }
            if name == "GROUPING_ID" {
                let g = env.group.ok_or_else(|| SqlError::new(8, "GROUPING_ID used outside aggregate query"))?;
                let mut r = 0;
                for a in &fc.args {
                    let target = format!("{:?}", a);
                    let bit = match g.base.iter().position(|b| *b == target) {
                        Some(i) if g.excluded[i] => 1,
                        _ => 0,
                    };
                    r = r * 2 + bit;
                }
                return Ok(Value::Int(r));
            }
            // lazy functions
            match name {
                "IIF" if fc.args.len() == 3 => {
                    let c = self.ev(&fc.args[0], env)?;
                    return if matches!(c, Value::Bit(true)) { self.ev(&fc.args[1], env) } else { self.ev(&fc.args[2], env) };
                }
                "COALESCE" => {
                    for a in &fc.args {
                        let v = self.ev(a, env)?;
                        if !v.is_null() {
                            return Ok(v);
                        }
                    }
                    return Ok(Value::Null);
                }
                "ISNULL" if fc.args.len() == 2 => {
                    let v = self.ev(&fc.args[0], env)?;
                    if !v.is_null() {
                        // result takes the type of the first argument
                        return Ok(v);
                    }
                    return self.ev(&fc.args[1], env);
                }
                "CHOOSE" if fc.args.len() >= 2 => {
                    let i = self.ev(&fc.args[0], env)?;
                    if i.is_null() {
                        return Ok(Value::Null);
                    }
                    let n = convert::cast(&i, &DataType::Int, None)?.as_i64().unwrap_or(0);
                    if n >= 1 && (n as usize) < fc.args.len() {
                        return self.ev(&fc.args[n as usize], env);
                    }
                    return Ok(Value::Null);
                }
                "UPDATE" if fc.args.len() == 1 => {
                    // UPDATE(column) inside triggers
                    if let (Expr::Col(p), Some(tc)) = (&fc.args[0], self.ses.trigger_stack.last()) {
                        if let Some(ci) = tc.table.col_index(&p[p.len() - 1]) {
                            return Ok(Value::Bit(tc.updated_cols.get(ci).copied().unwrap_or(false)));
                        }
                    }
                    return Ok(Value::Bit(false));
                }
                _ => {}
            }
        }
        // user-defined function?
        if let Some(f) = self.find_func(fc.schema.as_deref(), &fc.name_orig()) {
            if f.def.ret_type.is_some() {
                let mut args = vec![];
                let mut mask = vec![];
                for a in &fc.args {
                    if matches!(a, Expr::Default) {
                        args.push(Value::Null);
                        mask.push(true);
                    } else {
                        args.push(self.ev(a, env)?);
                        mask.push(false);
                    }
                }
                return self.call_scalar_udf(&f, args, &mask);
            }
        }
        let mut args = Vec::with_capacity(fc.args.len());
        for a in &fc.args {
            args.push(self.ev(a, env)?);
        }
        match self.builtin(name, args, fc)? {
            Some(v) => Ok(v),
            None => {
                if fc.schema.is_some() {
                    Err(SqlError::new(4121, format!("Cannot find either column \"{}\" or the user-defined function or aggregate \"{}\", or the name is ambiguous.", fc.schema.as_ref().unwrap(), fc.parts.join("."))))
                } else {
                    Err(SqlError::new(195, format!("'{}' is not a recognized built-in function name.", fc.parts.last().cloned().unwrap_or_else(|| fc.name.clone()))))
                }
            }
        }
    }

    pub fn next_value(&mut self, name: &[String]) -> Result<Value> {
        let loc = self.locate(name);
        if let Loc::Db(d, k) = &loc {
            if let Some(Object::Seq(s)) = self.srv.databases.get(d).and_then(|db| db.objects.get(k)) {
                let (start, inc, min, max, cycle, sname, ty) = (s.start, s.inc, s.min, s.max, s.cycle, s.name.clone(), s.ty.clone());
                let (dbn, keyn) = (d.clone(), k.clone());
                let srv: &ServerState = &self.srv;
                let (clock, oid, rv) = (srv.core.clock(), srv.core.object_id(), srv.core.rowversion());
                let next = s.current.update(|cur| {
                    let next = match cur {
                        None => start,
                        Some(c) => {
                            let n = c + inc;
                            if (inc > 0 && n > max) || (inc < 0 && n < min) {
                                if cycle {
                                    if inc > 0 {
                                        min
                                    } else {
                                        max
                                    }
                                } else {
                                    return Err(SqlError::new(11728, format!("The sequence object '{}' has reached its minimum or maximum value. Restart the sequence object to allow new values to be generated.", sname)));
                                }
                            } else {
                                n
                            }
                        }
                    };
                    // logged while the counter is held, so the log records values in the order they were handed out
                    if let Some(p) = srv.persist().as_mut() {
                        let _ = p.append(clock, oid, rv, vec![crate::persist::LogOp::SeqSet { db: dbn.clone(), key: keyn.clone(), current: Some(next) }], false);
                    }
                    Ok((Some(next), next))
                })?;
                return convert::cast(&Value::BigInt(next), &ty, None);
            }
        }
        Err(SqlError::new(208, format!("Invalid object name '{}'.", name.join("."))))
    }

    pub fn sysvar(&mut self, n: &str) -> Result<Value> {
        Ok(match n {
            "@@ROWCOUNT" => Value::Int(self.ses.rowcount as i32),
            "@@ERROR" => Value::Int(self.ses.last_error),
            "@@TRANCOUNT" => Value::Int(self.ses.trancount),
            "@@IDENTITY" => match self.ses.last_identity {
                Some(v) => Value::Dec(crate::decimal::Dec::from_int(v)),
                None => Value::Null,
            },
            "@@FETCH_STATUS" => Value::Int(self.ses.fetch_status),
            "@@SPID" => Value::Int(self.ses.spid),
            "@@SERVERNAME" | "@@SERVICENAME" => Value::Str(self.srv.server_name.clone()),
            "@@VERSION" => Value::Str(funcs::VERSION_STRING.to_string()),
            "@@NESTLEVEL" => Value::Int(self.ses.nest_level),
            "@@LANGUAGE" => Value::Str(self.ses.language.clone()),
            "@@DATEFIRST" => Value::Int(self.ses.datefirst),
            "@@MAX_CONNECTIONS" => Value::Int(32767),
            "@@MICROSOFTVERSION" => Value::Int(0x10000000 + 4000),
            "@@CURSOR_ROWS" => Value::Int(-1),
            "@@LOCK_TIMEOUT" => Value::Int(-1),
            "@@TEXTSIZE" => Value::Int(2147483647),
            "@@OPTIONS" => Value::Int(5496),
            "@@PROCID" => Value::Int(0),
            "@@CONNECTIONS" => Value::Int(1),
            "@@TOTAL_ERRORS" | "@@TOTAL_READ" | "@@TOTAL_WRITE" | "@@IO_BUSY" | "@@CPU_BUSY" | "@@IDLE" | "@@PACK_RECEIVED" | "@@PACK_SENT" | "@@PACKET_ERRORS" | "@@TIMETICKS" => Value::Int(0),
            "@@DBTS" => Value::Bin(self.srv.core.rowversion().to_be_bytes().to_vec()),
            _ => return Err(SqlError::new(137, format!("Must declare the scalar variable \"{}\".", n))),
        })
    }
}

impl FuncCall {
    pub fn name_orig(&self) -> String {
        self.parts.last().cloned().unwrap_or_else(|| self.name.clone())
    }
}

// ---------------------------------------------------------------- LIKE
#[derive(Debug)]
enum Pat {
    Lit(char),
    One,
    Many,
    Set(Vec<(char, char)>, bool),
}

fn compile_like(p: &str, esc: Option<char>, lc: &dyn Fn(char) -> char) -> Vec<Pat> {
    let ch: Vec<char> = p.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        if Some(c) == esc && i + 1 < ch.len() {
            out.push(Pat::Lit(lc(ch[i + 1])));
            i += 2;
            continue;
        }
        match c {
            '%' => out.push(Pat::Many),
            '_' => out.push(Pat::One),
            '[' => {
                if let Some(end) = ch[i + 1..].iter().position(|&x| x == ']') {
                    let body: Vec<char> = ch[i + 1..i + 1 + end].to_vec();
                    if body.is_empty() {
                        out.push(Pat::Lit('['));
                    } else {
                        let (neg, body) = if body[0] == '^' { (true, &body[1..]) } else { (false, &body[..]) };
                        let mut ranges = vec![];
                        let mut j = 0;
                        while j < body.len() {
                            if j + 2 < body.len() && body[j + 1] == '-' {
                                ranges.push((lc(body[j]), lc(body[j + 2])));
                                j += 3;
                            } else {
                                ranges.push((lc(body[j]), lc(body[j])));
                                j += 1;
                            }
                        }
                        out.push(Pat::Set(ranges, neg));
                        i += end + 2;
                        continue;
                    }
                } else {
                    out.push(Pat::Lit('['));
                }
            }
            _ => out.push(Pat::Lit(lc(c))),
        }
        i += 1;
    }
    out
}

fn like_rec(p: &[Pat], s: &[char], lc: &dyn Fn(char) -> char) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match &p[0] {
        Pat::Many => {
            for k in 0..=s.len() {
                if like_rec(&p[1..], &s[k..], lc) {
                    return true;
                }
            }
            false
        }
        Pat::One => !s.is_empty() && like_rec(&p[1..], &s[1..], lc),
        Pat::Lit(c) => !s.is_empty() && lc(s[0]) == *c && like_rec(&p[1..], &s[1..], lc),
        Pat::Set(r, neg) => {
            if s.is_empty() {
                return false;
            }
            let c = lc(s[0]);
            let inside = r.iter().any(|(a, b)| c >= *a && c <= *b);
            inside != *neg && like_rec(&p[1..], &s[1..], lc)
        }
    }
}

/// LIKE with the default (case-insensitive) rules.
pub fn like_match(s: &str, pat: &str, esc: Option<char>) -> bool {
    like_match_fold(s, pat, esc, &|c: char| c.to_lowercase().next().unwrap_or(c))
}

/// LIKE where `fold` maps every character of the string and of the pattern to the form that is compared (so the case
/// and accent rules of a collation can be applied).
pub fn like_match_fold(s: &str, pat: &str, esc: Option<char>, fold: &dyn Fn(char) -> char) -> bool {
    let p = compile_like(pat, esc, fold);
    let chars: Vec<char> = s.trim_end_matches(' ').chars().collect();
    like_rec(&p, &chars, fold)
}
