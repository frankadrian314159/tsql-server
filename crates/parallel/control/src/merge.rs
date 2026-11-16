//! Merging shard results with the control node's own engine.
//!
//! The rows that the shards returned are put in a temporary table of the control node's engine and the final query of the
//! plan runs over it, so grouping, ordering, collation, TOP and every expression behave exactly as in the single-node
//! server (nothing about SQL semantics is reimplemented in the merge).
use tsql_server::ast::*;
use tsql_server::decimal::Dec;
use tsql_server::types::MAX;
use tsql_server::{Connection, DataType, Event, ResultSet, SqlError, Value};

const TMP: &str = "#__dp_merge";
const CHUNK: usize = 20_000;

pub fn tmp_name() -> &'static str {
    TMP
}

/// The first error of a batch, if any.
pub fn check(res: &tsql_server::BatchResult) -> Result<(), SqlError> {
    match res.first_error() {
        Some(e) => Err(e.clone()),
        None => Ok(()),
    }
}

/// The type of each column of the temporary table: the type the shard reported, widened to hold every value that
/// actually arrived (the shards infer the type of an expression from what they saw; another shard may have longer
/// strings or more decimals).
pub fn widen(types: &[DataType], rows: &[Vec<Value>]) -> Vec<DataType> {
    types
        .iter()
        .enumerate()
        .map(|(i, t)| match t {
            DataType::Char(n) | DataType::VarChar(n) | DataType::NChar(n) | DataType::NVarChar(n) if *n != MAX => {
                let longest = rows.iter().filter_map(|r| if let Value::Str(s) = &r[i] { Some(s.chars().count() as i32) } else { None }).max().unwrap_or(0);
                let n2 = (*n).max(longest);
                let n2 = if n2 > 8000 { MAX } else { n2.max(1) };
                match t {
                    DataType::Char(_) => DataType::Char(n2),
                    DataType::VarChar(_) => DataType::VarChar(n2),
                    DataType::NChar(_) => DataType::NChar(n2),
                    _ => DataType::NVarChar(n2),
                }
            }
            DataType::Binary(n) | DataType::VarBinary(n) if *n != MAX => {
                let longest = rows.iter().filter_map(|r| if let Value::Bin(b) = &r[i] { Some(b.len() as i32) } else { None }).max().unwrap_or(0);
                let n2 = (*n).max(longest);
                let n2 = if n2 > 8000 { MAX } else { n2.max(1) };
                if matches!(t, DataType::Binary(_)) {
                    DataType::Binary(n2)
                } else {
                    DataType::VarBinary(n2)
                }
            }
            DataType::Decimal(p, s) => {
                let mut int_digits = (*p).saturating_sub(*s) as u32;
                let mut scale = *s as u32;
                for r in rows {
                    if let Value::Dec(d) = &r[i] {
                        let d: &Dec = d;
                        scale = scale.max(d.s as u32);
                        int_digits = int_digits.max(d.precision().saturating_sub(d.s as u32));
                    }
                }
                DataType::Decimal((int_digits + scale).min(38) as u8, scale.min(38) as u8)
            }
            DataType::Int | DataType::SmallInt | DataType::TinyInt if rows.iter().any(|r| matches!(r[i], Value::BigInt(_))) => DataType::BigInt,
            other => other.clone(),
        })
        .collect()
}

/// Put `rows` in a temporary table and run the final query that `build` makes (given the column names and the widened
/// types) over it.
pub fn run_final(
    conn: &mut Connection,
    columns: &[String],
    types: &[DataType],
    rows: Vec<Vec<Value>>,
    build: impl FnOnce(&[String], &[DataType]) -> Option<Query>,
) -> Result<ResultSet, SqlError> {
    let types = widen(types, &rows);
    let query = build(columns, &types).ok_or_else(|| SqlError::new(50000, "the query could not be rewritten for the cluster"))?;
    let cols: Vec<String> = (0..types.len()).map(|i| format!("{} {} NULL", crate::plan::cname(i), types[i])).collect();
    let _ = conn.execute(&format!("DROP TABLE IF EXISTS {}", TMP));
    check(&conn.execute(&format!("CREATE TABLE {} ({})", TMP, cols.join(", "))))?;
    let result = (|| {
        let mut it = rows.into_iter();
        loop {
            let chunk: Vec<Vec<Value>> = it.by_ref().take(CHUNK).collect();
            if chunk.is_empty() {
                break;
            }
            conn.load_rows(&[TMP.to_string()], None, chunk)?;
        }
        let res = conn.execute_stmts(&[Stmt::Query(Box::new(query))]);
        check(&res)?;
        for ev in res.events {
            if let Event::Rows(rs) = ev {
                return Ok(rs);
            }
        }
        Err(SqlError::new(50000, "the final query returned no result set"))
    })();
    let _ = conn.execute(&format!("DROP TABLE IF EXISTS {}", TMP));
    result
}

/// Make the temporary table `name` in the control node's engine with these columns (types widened to hold the rows) and fill it.
pub fn load_table(conn: &mut Connection, name: &str, columns: &[String], types: &[DataType], rows: Vec<Vec<Value>>) -> Result<(), SqlError> {
    let types = widen(types, &rows);
    let defs: Vec<String> = columns.iter().zip(&types).map(|(n, t)| format!("[{}] {} NULL", n.replace(']', "]]"), t)).collect();
    let _ = conn.execute(&format!("DROP TABLE IF EXISTS {}", name));
    check(&conn.execute(&format!("CREATE TABLE {} ({})", name, defs.join(", "))))?;
    let mut it = rows.into_iter();
    loop {
        let chunk: Vec<Vec<Value>> = it.by_ref().take(CHUNK).collect();
        if chunk.is_empty() {
            return Ok(());
        }
        conn.load_rows(&[name.to_string()], None, chunk)?;
    }
}
