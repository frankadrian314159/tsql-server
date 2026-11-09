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
            let (ti, val) = if rd.b[rd.p] == 0x1F {
                rd.u8()?;
                (Ti { w: Wire::Int(4), fixed: false }, Value::Null)
            } else {
                let ti = read_typeinfo(&mut rd)?;
                let val = read_value(&mut rd, &ti)?;
                (ti, val)
            };
            params.push(Param { name, out: status & 1 != 0, ti, val });
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
    let mut sql = String::from("DECLARE @__rc int = 0;\n");
    let mut outs = vec![];
    let mut call: Vec<String> = lead.to_vec();
    for &i in args {
        let p = &params[i];
        let named = p.name.trim_start_matches('@');
        let expr = if p.out {
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
    plan("sp_executesql", &lead, params, args, false)
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
