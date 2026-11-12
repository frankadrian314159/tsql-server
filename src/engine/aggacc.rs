//! Aggregates that are worked out as the rows of a group arrive, instead of after all of them have been kept.
//!
//! `COUNT`, `COUNT_BIG`, `SUM`, `AVG`, `MIN` and `MAX` (without `DISTINCT`) can be folded one value at a time. The fold is the one
//! `Engine::agg_over` does over a group's values (same additions in the same order, same result types), so a group gives
//! the same answer either way. An error (an overflow, a value that cannot be added, an argument that fails to evaluate) is kept
//! and raised only when the aggregate is read, as it would have been when the aggregate was evaluated after the rows were
//! collected: a group that `HAVING` throws away never raises it.
use super::query::{is_agg_name, Env};
use super::*;
use crate::convert::{arith, compare};
use crate::ast::FuncCall;
use std::sync::Arc;

/// Can this aggregate call be folded as rows arrive?
pub fn foldable(fc: &FuncCall) -> bool {
    fc.over.is_none()
        && fc.schema.is_none()
        && !fc.distinct
        && fc.within.is_none()
        && matches!(fc.name.as_str(), "COUNT" | "COUNT_BIG" | "SUM" | "AVG" | "MIN" | "MAX")
        && (!fc.star || matches!(fc.name.as_str(), "COUNT" | "COUNT_BIG"))
        && (fc.star || fc.args.len() == 1)
        // an aggregate inside an aggregate is an error that the general path reports
        && !fc.args.iter().any(super::query::has_agg)
}

/// Every aggregate call of the expression, or `false` if one of them cannot be folded.
pub fn collect<'e>(e: &'e Expr, out: &mut Vec<&'e FuncCall>) -> bool {
    if let Expr::Func(fc) = e {
        if fc.over.is_none() && fc.schema.is_none() && is_agg_name(&fc.name) {
            if !foldable(fc) {
                return false;
            }
            out.push(fc);
            return true;
        }
        if fc.over.is_none() && fc.schema.is_none() && (fc.name == "GROUPING" || fc.name == "GROUPING_ID") {
            return true;
        }
    }
    let mut ch = vec![];
    super::query::children(e, &mut ch);
    ch.into_iter().all(|c| collect(c, out))
}

/// The state of one aggregate call over one group.
pub struct AggAcc {
    /// Rows (for `COUNT(*)`) or non-NULL values seen.
    count: u64,
    /// The running sum (`SUM`, `AVG`) or the best value so far (`MIN`, `MAX`).
    val: Option<Value>,
    err: Option<SqlError>,
    /// The collation `MIN` and `MAX` of a string compare with, taken from the first row.
    coll: Option<Arc<crate::collation::Collation>>,
    started: bool,
}

impl AggAcc {
    pub fn new() -> AggAcc {
        AggAcc { count: 0, val: None, err: None, coll: None, started: false }
    }

    /// Take one row of the group into account.
    pub fn update(&mut self, eng: &mut Engine<'_>, fc: &FuncCall, env: &Env<'_>) {
        if self.err.is_some() {
            return;
        }
        if fc.star {
            self.count += 1;
            return;
        }
        let v = match eng.ev(&fc.args[0], env) {
            Ok(v) => v,
            Err(e) => {
                self.err = Some(e);
                return;
            }
        };
        if !self.started {
            self.started = true;
            if matches!(fc.name.as_str(), "MIN" | "MAX") && crate::collation::in_use() {
                self.coll = eng.expr_coll(&fc.args[0], env).map(|c| c.0).filter(|c| !c.is_default());
            }
        }
        if v.is_null() {
            return;
        }
        self.count += 1;
        match fc.name.as_str() {
            "SUM" | "AVG" => {
                self.val = match self.val.take() {
                    None => match &v {
                        Value::Str(_) => {
                            self.err = Some(SqlError::new(8117, "Operand data type varchar is invalid for sum operator."));
                            return;
                        }
                        Value::Bit(b) => Some(Value::Int(*b as i32)),
                        v => Some(v.clone()),
                    },
                    Some(acc) => match arith(BinOp::Add, &acc, &v) {
                        Ok(a) => Some(a),
                        Err(e) => {
                            self.err = Some(e);
                            return;
                        }
                    },
                };
            }
            "MIN" | "MAX" => {
                let min = fc.name == "MIN";
                self.val = Some(match self.val.take() {
                    None => v,
                    Some(b) => {
                        let o = match (&self.coll, &v, &b) {
                            (Some(c), Value::Str(x), Value::Str(y)) => c.compare(x, y),
                            _ => match compare(&v, &b) {
                                Ok(o) => o.unwrap_or(std::cmp::Ordering::Equal),
                                Err(e) => {
                                    self.err = Some(e);
                                    return;
                                }
                            },
                        };
                        if (min && o == std::cmp::Ordering::Less) || (!min && o == std::cmp::Ordering::Greater) {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            _ => {}
        }
    }

    /// The value of the aggregate over the rows taken so far.
    pub fn finish(&self, fc: &FuncCall) -> std::result::Result<Value, SqlError> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        match fc.name.as_str() {
            "COUNT" => Ok(Value::Int(self.count as i32)),
            "COUNT_BIG" => Ok(Value::BigInt(self.count as i64)),
            "SUM" | "AVG" => {
                let Some(acc) = self.val.clone() else { return Ok(Value::Null) };
                if fc.name == "AVG" {
                    let cnt = match acc {
                        Value::BigInt(_) => Value::BigInt(self.count as i64),
                        _ => Value::Int(self.count as i32),
                    };
                    let scale = if let Value::Dec(d) = &acc { Some(d.s) } else { None };
                    let mut r = arith(BinOp::Div, &acc, &cnt)?;
                    if let (Some(sc), Value::Dec(d)) = (scale, r.clone()) {
                        r = Value::Dec(d.rescale(sc).unwrap_or(d));
                    }
                    return Ok(r);
                }
                Ok(acc)
            }
            _ => Ok(self.val.clone().unwrap_or(Value::Null)),
        }
    }
}
