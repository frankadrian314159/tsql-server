//! Remote procedure call requests. The engine executes T-SQL text, so a call (with its typed parameters) is
//! turned into a short batch: parameter values become literals, OUTPUT parameters and the return status are
//! captured in variables and read back by a trailing `SELECT` that the server strips from the reply.
use super::wire::*;
use crate::convert;
use crate::types::{DtKind, Value};
use chrono::Timelike;

pub struct Param {
    pub name: String,
    pub out: bool,
    pub ti: Ti,
    pub val: Value,
    /// A table-valued parameter: the table type and its rows.
    pub tvp: Option<Tvp>,
}

/// A table-valued parameter as the client sent it (TVP_TYPE_INFO and its rows).
pub struct Tvp {
    pub schema: String,
    pub name: String,
    pub rows: Vec<Vec<Value>>,
}

/// TVPTYPE, the type byte of a table-valued parameter.
const TVPTYPE: u8 = 0xF3;

/// The table-valued parameter at the reader (its type byte included).
fn parse_tvp(rd: &mut Rd) -> R<Tvp> {
    rd.u8()?;
    let _database = rd.bvarchar()?;
    let schema = rd.bvarchar()?;
    let name = rd.bvarchar()?;
    // the columns: a count (0xFFFF for none), and for each a user type, flags, TYPE_INFO and name
    let n = rd.u16()?;
    let mut cols: Vec<Ti> = vec![];
    if n != 0xFFFF {
        for _ in 0..n {
            rd.u32()?;
            rd.u16()?;
            cols.push(read_typeinfo(rd)?);
            rd.bvarchar()?;
        }
    }
    // optional ordering and uniqueness (0x10) and column ordering (0x11) tokens, then the end of the metadata
    loop {
        match rd.b.get(rd.p) {
            Some(0x10) => {
                rd.u8()?;
                let k = rd.u16()? as usize;
                rd.take(3 * k)?;
            }
            Some(0x11) => {
                rd.u8()?;
                let k = rd.u16()? as usize;
                rd.take(2 * k)?;
            }
            _ => break,
        }
    }
    if rd.left() > 0 {
        rd.u8()?; // TVP_END_TOKEN
    }
    let mut rows = vec![];
    while rd.left() > 0 {
        match rd.u8()? {
            0x01 => {
                let mut row = Vec::with_capacity(cols.len());
                for ti in &cols {
                    row.push(read_value(rd, ti)?);
                }
                rows.push(row);
            }
            0x00 => break,
            other => return Err(format!("unexpected token {:#x} in a table-valued parameter", other)),
        }
    }
    Ok(Tvp { schema, name, rows })
}

pub enum Proc {
    Id(u16),
    Name(String),
}

pub struct Rpc {
    pub proc: Proc,
    pub params: Vec<Param>,
}

/// Name of the first column of the trailing result set that carries return values.
pub const TRAILER: &str = "__tds_trailer";

pub const SP_EXECUTESQL: u16 = 10;
pub const SP_PREPARE: u16 = 11;
pub const SP_EXECUTE: u16 = 12;
pub const SP_PREPEXEC: u16 = 13;
pub const SP_UNPREPARE: u16 = 15;
pub const SP_CURSOR: u16 = 1;
pub const SP_CURSOROPEN: u16 = 2;
pub const SP_CURSORPREPARE: u16 = 3;
pub const SP_CURSOREXECUTE: u16 = 4;
pub const SP_CURSORPREPEXEC: u16 = 5;
pub const SP_CURSORUNPREPARE: u16 = 6;
pub const SP_CURSORFETCH: u16 = 7;
pub const SP_CURSOROPTION: u16 = 8;
pub const SP_CURSORCLOSE: u16 = 9;

pub fn parse(payload: &[u8]) -> R<Vec<Rpc>> {
    let mut rd = Rd::new(payload);
    rd.all_headers()?;
    let mut calls = vec![];
    while rd.left() > 0 {
        let n = rd.u16()?;
        let proc = if n == 0xFFFF { Proc::Id(rd.u16()?) } else { Proc::Name(rd.utf16(n as usize)?) };
        rd.u16()?; // option flags
        let mut params = vec![];
        while rd.left() > 0 {
            let first = rd.b[rd.p];
            if first == 0x80 || first == 0xFF {
                rd.u8()?; // batch separator / no-exec flag: another call follows
                break;
            }
            let name = rd.bvarchar()?;
            let status = rd.u8()?;
            // an untyped NULL (type 0x1F) has no value bytes; give it a type that converts to anything
            if rd.b[rd.p] == TVPTYPE {
                let tvp = parse_tvp(&mut rd)?;
                params.push(Param { name, out: false, ti: Ti { w: Wire::Int(4), fixed: false, cp: 1252 }, val: Value::Null, tvp: Some(tvp) });
                continue;
            }
            let (ti, val) = if rd.b[rd.p] == 0x1F {
                rd.u8()?;
                (Ti { w: Wire::Int(4), fixed: false, cp: 1252 }, Value::Null)
            } else {
                let ti = read_typeinfo(&mut rd)?;
                let val = read_value(&mut rd, &ti)?;
                (ti, val)
            };
            params.push(Param { name, out: status & 1 != 0, ti, val, tvp: None });
        }
        calls.push(Rpc { proc, params });
    }
    Ok(calls)
}

/// The fraction of a second in units of 100 ns (chrono has no `%.7f`).
fn frac7(nanos: u32) -> u32 {
    (nanos % 1_000_000_000) / 100
}

/// A value as a T-SQL literal that converts back to the same value.
pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bit(b) => (*b as i32).to_string(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => format!("{:e}", f),
        Value::Float(_) => "NULL".into(),
        Value::Str(s) => format!("N'{}'", s.replace('\'', "''")),
        Value::Bin(b) => format!("0x{}", b.iter().map(|x| format!("{:02X}", x)).collect::<String>()),
        // dates and times use ISO 8601 so no precision is lost (the default string style drops seconds)
        Value::Date(d) => format!("'{}'", d.format("%Y-%m-%d")),
        Value::Time(t) => format!("'{}.{:07}'", t.format("%H:%M:%S"), frac7(t.nanosecond())),
        Value::DateTime(d, DtKind::DateTime2) => format!("'{}.{:07}'", d.format("%Y-%m-%dT%H:%M:%S"), frac7(d.nanosecond())),
        Value::DateTime(d, DtKind::DateTime) => format!("'{}'", d.format("%Y-%m-%dT%H:%M:%S%.3f")),
        Value::DateTime(d, DtKind::SmallDateTime) => format!("'{}'", d.format("%Y-%m-%dT%H:%M:%S")),
        Value::Dto(d) => format!("'{}.{:07}{}'", d.format("%Y-%m-%dT%H:%M:%S"), frac7(d.nanosecond()), d.format("%:z")),
        other => format!("'{}'", convert::to_str(other).replace('\'', "''")),
    }
}

/// What to run for a call, and which parameters come back.
pub struct Plan {
    pub sql: String,
    /// Indexes (into the call's parameters) of the OUTPUT parameters, in trailer column order.
    pub outs: Vec<usize>,
    /// The call reports a return status.
    pub status: bool,
}

fn safe_proc_name(n: &str) -> R<&str> {
    let n = n.trim();
    if !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || "_.[]#$@ ".contains(c)) {
        Ok(n)
    } else {
        Err(format!("invalid procedure name '{}'", n))
    }
}

/// Build the batch for calling `target` (a procedure name, or `sp_executesql` with the text and declarations
/// already in `lead`) with the parameters `args`, given as indexes into `params`.
pub fn plan(target: &str, lead: &[String], params: &[Param], args: &[usize], capture_status: bool) -> R<Plan> {
    plan_with_names(target, lead, params, args, capture_status, &[])
}

/// As [`plan`]; `pnames` are the names the unnamed arguments take, by position (the parameter names of the declaration of
/// `sp_executesql`, since a call may not put an unnamed argument after a named one).
pub fn plan_with_names(target: &str, lead: &[String], params: &[Param], args: &[usize], capture_status: bool, pnames: &[String]) -> R<Plan> {
    let mut sql = String::from("DECLARE @__rc int = 0;\n");
    let mut outs = vec![];
    let mut call: Vec<String> = lead.to_vec();
    for (k, &i) in args.iter().enumerate() {
        let p = &params[i];
        let named = if p.name.is_empty() { pnames.get(k).map(|n| n.trim_start_matches('@')).unwrap_or("") } else { p.name.trim_start_matches('@') };
        let expr = if let Some(t) = &p.tvp {
            // a table-valued parameter: a table variable of the type, filled with the rows, passed by name
            let var = format!("@__tv{}", i);
            let q = |n: &str| format!("[{}]", n.replace(']', "]]"));
            sql.push_str(&format!("DECLARE {} {}.{};\n", var, q(if t.schema.is_empty() { "dbo" } else { &t.schema }), q(&t.name)));
            for chunk in t.rows.chunks(500) {
                let rows: Vec<String> = chunk.iter().map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", "))).collect();
                sql.push_str(&format!("INSERT {} VALUES {};\n", var, rows.join(", ")));
            }
            var
        } else if p.out {
            let var = format!("@__o{}", i);
            sql.push_str(&format!("DECLARE {} {} = {};\n", var, p.ti.w.sql_name(), literal(&p.val)));
            outs.push(i);
            format!("{} OUTPUT", var)
        } else {
            literal(&p.val)
        };
        if named.is_empty() {
            call.push(expr);
        } else {
            if !named.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Err(format!("invalid parameter name '{}'", p.name));
            }
            call.push(format!("@{} = {}", named, expr));
        }
    }
    let rc = if capture_status { "@__rc = " } else { "" };
    sql.push_str(&format!("EXEC {}{} {};\n", rc, target, call.join(", ")));
    sql.push_str(&format!("SELECT @__rc AS {}", TRAILER));
    for &i in &outs {
        sql.push_str(&format!(", @__o{} AS __tds_o{}", i, i));
    }
    sql.push_str(";\n");
    Ok(Plan { sql, outs, status: capture_status })
}

/// Parameters of a call to a user procedure, by name.
pub fn plan_named(name: &str, params: &[Param]) -> R<Plan> {
    let name = safe_proc_name(name)?;
    let args: Vec<usize> = (0..params.len()).collect();
    plan(name, &[], params, &args, true)
}

/// `sp_executesql @stmt, @params, ...` with `args` as the user parameters.
pub fn plan_executesql(stmt: &str, decl: Option<&str>, params: &[Param], args: &[usize]) -> R<Plan> {
    let lead = vec![literal(&Value::str(stmt)), decl.map(|d| literal(&Value::str(d))).unwrap_or_else(|| "NULL".into())];
    // the names of the declared parameters: an unnamed argument after a named one (the Microsoft JDBC driver names a
    // table-valued parameter and not the others) must be named to be a valid call
    let pnames: Vec<String> = decl.and_then(|d| crate::parser::parse_param_decls(d).ok()).map(|v| v.into_iter().map(|p| p.name).collect()).unwrap_or_default();
    let mixed = args.iter().any(|&i| !params[i].name.is_empty()) && args.iter().any(|&i| params[i].name.is_empty());
    plan_with_names("sp_executesql", &lead, params, args, true, if mixed { &pnames } else { &[] })
}

pub fn text_param(params: &[Param], i: usize, what: &str) -> R<Option<String>> {
    match params.get(i) {
        Some(p) if p.val.is_null() => Ok(None),
        Some(p) => Ok(Some(convert::to_str(&p.val))),
        None => Err(format!("missing parameter {}", what)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(literal(&Value::str("it's")), "N'it''s'");
        assert_eq!(literal(&Value::Bin(vec![0xAB, 1])), "0xAB01");
        assert_eq!(literal(&Value::Float(0.5)), "5e-1");
        assert_eq!(literal(&Value::Null), "NULL");
        let d = chrono::NaiveDate::from_ymd_opt(2024, 3, 4).unwrap().and_hms_milli_opt(5, 6, 7, 123).unwrap();
        assert_eq!(literal(&Value::DateTime(d, DtKind::DateTime)), "'2024-03-04T05:06:07.123'");
        assert_eq!(literal(&Value::Date(d.date())), "'2024-03-04'");
        let d = chrono::NaiveDate::from_ymd_opt(2024, 3, 4).unwrap().and_hms_milli_opt(5, 6, 7, 123).unwrap();
        assert_eq!(literal(&Value::DateTime(d, crate::types::DtKind::DateTime)), "'2024-03-04T05:06:07.123'");
        assert_eq!(literal(&Value::Date(d.date())), "'2024-03-04'");
    }
}
