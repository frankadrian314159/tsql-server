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

impl<'a> Engine<'a> {
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
                        let o = compare(&l, &r)?;
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
                    (false, false) => compare(&l, &r)? == Some(Ordering::Equal),
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
                    match compare(&l, &r)? {
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
                let rs = self.exec_query(q, Some(env))?;
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
                let m = like_match(&to_str(&v), &to_str(&pat), escc);
                Ok(Value::Bit(m != *neg))
            }
            Expr::Between(a, lo, hi, neg) => {
                let v = self.ev(a, env)?;
                let l = self.ev(lo, env)?;
                let h = self.ev(hi, env)?;
                let c1 = compare(&v, &l)?.map(|o| o != Ordering::Less);
                let c2 = compare(&v, &h)?.map(|o| o != Ordering::Greater);
                let r = match (c1, c2) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                };
                Ok(tv(r.map(|b| b != *neg)))
            }
            Expr::CmpQuant(op, all, a, q) => {
                let l = self.ev(a, env)?;
                let rs = self.exec_query(q, Some(env))?;
                if rs.rows.is_empty() {
                    return Ok(Value::Bit(*all));
                }
                let mut saw_null = false;
                for r in rs.rows.iter() {
                    let o = compare(&l, &r[0])?;
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
                let rs = self.exec_query(q, Some(env))?;
                Ok(Value::Bit(!rs.rows.is_empty()))
            }
            Expr::Subquery(q) => {
                let rs = self.exec_query(q, Some(env))?;
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
            Expr::Collate(x, _) => self.ev(x, env),
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

    fn eval_func(&mut self, fc: &FuncCall, env: &Env<'_>) -> Result<Value> {
        if fc.over.is_some() {
            let key = fc as *const FuncCall as usize;
            return match env.win.and_then(|w| w.get(&key)) {
                Some(v) => Ok(v.clone()),
                None => Err(SqlError::new(4108, "Windowed functions can only appear in the SELECT or ORDER BY clauses.")),
            };
        }
        let name = fc.name.as_str();
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
            if let Some(Object::Seq(s)) = self.srv.databases.get_mut(d).and_then(|db| db.objects.get_mut(k)) {
                let next = match s.current {
                    None => s.start,
                    Some(c) => {
                        let n = c + s.inc;
                        if (s.inc > 0 && n > s.max) || (s.inc < 0 && n < s.min) {
                            if s.cycle {
                                if s.inc > 0 {
                                    s.min
                                } else {
                                    s.max
                                }
                            } else {
                                return Err(SqlError::new(11728, format!("The sequence object '{}' has reached its minimum or maximum value. Restart the sequence object to allow new values to be generated.", s.name)));
                            }
                        } else {
                            n
                        }
                    }
                };
                s.current = Some(next);
                let ty = s.ty.clone();
                let (dbn, keyn) = (d.clone(), k.clone());
                self.log_nowait(vec![crate::persist::LogOp::SeqSet { db: dbn, key: keyn, current: Some(next) }]);
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
            "@@DBTS" => Value::Bin(self.srv.next_rowversion.to_be_bytes().to_vec()),
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

fn lc(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn compile_like(p: &str, esc: Option<char>) -> Vec<Pat> {
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

fn like_rec(p: &[Pat], s: &[char]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match &p[0] {
        Pat::Many => {
            for k in 0..=s.len() {
                if like_rec(&p[1..], &s[k..]) {
                    return true;
                }
            }
            false
        }
        Pat::One => !s.is_empty() && like_rec(&p[1..], &s[1..]),
        Pat::Lit(c) => !s.is_empty() && lc(s[0]) == *c && like_rec(&p[1..], &s[1..]),
        Pat::Set(r, neg) => {
            if s.is_empty() {
                return false;
            }
            let c = lc(s[0]);
            let inside = r.iter().any(|(a, b)| c >= *a && c <= *b);
            inside != *neg && like_rec(&p[1..], &s[1..])
        }
    }
}

pub fn like_match(s: &str, pat: &str, esc: Option<char>) -> bool {
    let p = compile_like(pat, esc);
    let chars: Vec<char> = s.trim_end_matches(' ').chars().collect();
    like_rec(&p, &chars)
}
