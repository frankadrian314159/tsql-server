//! A result set of the engine as Arrow record batches (the data plane of the cluster) and back.
//!
//! The common value kinds travel as native Arrow columns (booleans, 32 and 64 bit integers, doubles, strings, binary).
//! Every other kind (decimals, dates, times, GUIDs) and any column that mixes kinds (a `sql_variant`) travels as a Binary
//! column holding the engine's own encoding of the value, marked `tsql.pack` in the field metadata, so a value always
//! arrives exactly as it left. The declared SQL types of the columns travel in the schema metadata.
use arrow_array::builder::{BinaryBuilder, BooleanBuilder, Float64Builder, Int32Builder, Int64Builder, StringBuilder};
use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int32Type, Int64Type};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType as ArrowType, Field, Schema, SchemaRef};
use std::collections::HashMap;
use std::sync::Arc;
use tsql_server::{DataType, ResultSet, Value};

const META_TYPES: &str = "tsql.types";
const META_PACK: &str = "tsql.pack";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bool,
    I32,
    I64,
    F64,
    Utf8,
    Bin,
    Pack,
}

fn kind_of_value(v: &Value) -> Option<Kind> {
    match v {
        Value::Null => None,
        Value::Bit(_) => Some(Kind::Bool),
        Value::Int(_) => Some(Kind::I32),
        Value::BigInt(_) => Some(Kind::I64),
        Value::Float(_) => Some(Kind::F64),
        Value::Str(_) => Some(Kind::Utf8),
        Value::Bin(_) => Some(Kind::Bin),
        _ => Some(Kind::Pack),
    }
}

fn column_kind(rows: &[Vec<Value>], c: usize) -> Kind {
    let mut kind: Option<Kind> = None;
    for r in rows {
        if let Some(k) = kind_of_value(&r[c]) {
            match kind {
                None => kind = Some(k),
                Some(prev) if prev == k => {}
                Some(_) => return Kind::Pack,
            }
        }
    }
    kind.unwrap_or(Kind::Utf8)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn build(kind: Kind, rows: &[Vec<Value>], c: usize) -> Result<ArrayRef, String> {
    Ok(match kind {
        Kind::Bool => {
            let mut b = BooleanBuilder::with_capacity(rows.len());
            for r in rows {
                match &r[c] {
                    Value::Bit(x) => b.append_value(*x),
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        Kind::I32 => {
            let mut b = Int32Builder::with_capacity(rows.len());
            for r in rows {
                match &r[c] {
                    Value::Int(x) => b.append_value(*x),
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        Kind::I64 => {
            let mut b = Int64Builder::with_capacity(rows.len());
            for r in rows {
                match &r[c] {
                    Value::BigInt(x) => b.append_value(*x),
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        Kind::F64 => {
            let mut b = Float64Builder::with_capacity(rows.len());
            for r in rows {
                match &r[c] {
                    Value::Float(x) => b.append_value(*x),
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        Kind::Utf8 => {
            let mut b = StringBuilder::with_capacity(rows.len(), rows.len() * 8);
            for r in rows {
                match &r[c] {
                    Value::Str(x) => b.append_value(x),
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        Kind::Bin => {
            let mut b = BinaryBuilder::with_capacity(rows.len(), rows.len() * 8);
            for r in rows {
                match &r[c] {
                    Value::Bin(x) => b.append_value(x),
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        Kind::Pack => {
            let mut b = BinaryBuilder::with_capacity(rows.len(), rows.len() * 16);
            for r in rows {
                match &r[c] {
                    Value::Null => b.append_null(),
                    v => b.append_value(postcard::to_allocvec(v).map_err(|e| e.to_string())?),
                }
            }
            Arc::new(b.finish())
        }
    })
}

/// The Arrow schema of a result set, and its rows as batches of at most `max_rows` rows (none for an empty result).
pub fn to_batches(rs: &ResultSet, max_rows: usize) -> Result<(SchemaRef, Vec<RecordBatch>), String> {
    let kinds: Vec<Kind> = (0..rs.columns.len()).map(|c| column_kind(&rs.rows, c)).collect();
    let fields: Vec<Field> = rs
        .columns
        .iter()
        .zip(&kinds)
        .map(|(name, k)| {
            let at = match k {
                Kind::Bool => ArrowType::Boolean,
                Kind::I32 => ArrowType::Int32,
                Kind::I64 => ArrowType::Int64,
                Kind::F64 => ArrowType::Float64,
                Kind::Utf8 => ArrowType::Utf8,
                Kind::Bin | Kind::Pack => ArrowType::Binary,
            };
            let f = Field::new(name, at, true);
            if *k == Kind::Pack {
                f.with_metadata(HashMap::from([(META_PACK.to_string(), "1".to_string())]))
            } else {
                f
            }
        })
        .collect();
    let types = postcard::to_allocvec(&rs.types).map_err(|e| e.to_string())?;
    let schema: SchemaRef = Arc::new(Schema::new_with_metadata(fields, HashMap::from([(META_TYPES.to_string(), hex(&types))])));
    let mut batches = vec![];
    for chunk in rs.rows.chunks(max_rows.max(1)) {
        let cols: Result<Vec<ArrayRef>, String> = kinds.iter().enumerate().map(|(c, k)| build(*k, chunk, c)).collect();
        batches.push(RecordBatch::try_new(schema.clone(), cols?).map_err(|e| e.to_string())?);
    }
    Ok((schema, batches))
}

fn decode_column(f: &Field, a: &ArrayRef) -> Result<Vec<Value>, String> {
    let pack = f.metadata().contains_key(META_PACK);
    Ok(match f.data_type() {
        ArrowType::Boolean => a.as_boolean().iter().map(|x| x.map(Value::Bit).unwrap_or(Value::Null)).collect(),
        ArrowType::Int32 => a.as_primitive::<Int32Type>().iter().map(|x| x.map(Value::Int).unwrap_or(Value::Null)).collect(),
        ArrowType::Int64 => a.as_primitive::<Int64Type>().iter().map(|x| x.map(Value::BigInt).unwrap_or(Value::Null)).collect(),
        ArrowType::Float64 => a.as_primitive::<Float64Type>().iter().map(|x| x.map(Value::Float).unwrap_or(Value::Null)).collect(),
        ArrowType::Utf8 => a.as_string::<i32>().iter().map(|x| x.map(|s| Value::Str(s.to_string())).unwrap_or(Value::Null)).collect(),
        ArrowType::Binary => {
            let mut out = Vec::with_capacity(a.len());
            for x in a.as_binary::<i32>().iter() {
                out.push(match x {
                    None => Value::Null,
                    Some(b) if pack => postcard::from_bytes::<Value>(b).map_err(|e| format!("a packed value: {}", e))?,
                    Some(b) => Value::Bin(b.to_vec()),
                });
            }
            out
        }
        other => return Err(format!("the column {} has the unsupported Arrow type {:?}", f.name(), other)),
    })
}

/// The result set that a schema and its batches stand for.
pub fn from_batches(schema: &Schema, batches: &[RecordBatch]) -> Result<ResultSet, String> {
    let types: Vec<DataType> = match schema.metadata().get(META_TYPES) {
        Some(h) => {
            let bytes = unhex(h).ok_or("bad type metadata")?;
            postcard::from_bytes(&bytes).map_err(|e| format!("bad type metadata: {}", e))?
        }
        None => return Err("the stream carries no SQL types".into()),
    };
    let columns: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(total);
    for b in batches {
        let cols: Result<Vec<Vec<Value>>, String> = schema.fields().iter().zip(b.columns()).map(|(f, a)| decode_column(f, a)).collect();
        let mut cols: Vec<std::vec::IntoIter<Value>> = cols?.into_iter().map(|c| c.into_iter()).collect();
        for _ in 0..b.num_rows() {
            rows.push(cols.iter_mut().map(|c| c.next().unwrap_or(Value::Null)).collect());
        }
    }
    Ok(ResultSet { columns, types, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &Value, b: &Value) -> bool {
        format!("{:?}", a) == format!("{:?}", b)
    }

    #[test]
    fn a_result_set_survives_the_trip() {
        let rs = ResultSet {
            columns: vec!["i".into(), "b".into(), "s".into(), "f".into(), "x".into(), "mixed".into(), "empty".into(), "bit".into()],
            types: vec![DataType::Int, DataType::BigInt, DataType::VarChar(20), DataType::Float, DataType::VarBinary(8), DataType::Variant, DataType::Int, DataType::Bit],
            rows: vec![
                vec![Value::Int(1), Value::BigInt(1 << 40), Value::Str("a".into()), Value::Float(1.5), Value::Bin(vec![1, 2]), Value::Int(7), Value::Null, Value::Bit(true)],
                vec![Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Str("seven".into()), Value::Null, Value::Bit(false)],
                vec![Value::Int(-3), Value::BigInt(-1), Value::Str("héllo".into()), Value::Float(f64::MAX), Value::Bin(vec![]), Value::Null, Value::Null, Value::Null],
            ],
        };
        let (schema, batches) = to_batches(&rs, 2).unwrap();
        assert_eq!(batches.len(), 2);
        let back = from_batches(&schema, &batches).unwrap();
        assert_eq!(back.columns, rs.columns);
        assert_eq!(back.types, rs.types);
        assert_eq!(back.rows.len(), 3);
        for (r1, r2) in rs.rows.iter().zip(&back.rows) {
            for (a, b) in r1.iter().zip(r2) {
                assert!(same(a, b), "{:?} != {:?}", a, b);
            }
        }
    }

    #[test]
    fn an_empty_result_keeps_its_columns() {
        let rs = ResultSet { columns: vec!["a".into()], types: vec![DataType::Int], rows: vec![] };
        let (schema, batches) = to_batches(&rs, 100).unwrap();
        assert!(batches.is_empty());
        let back = from_batches(&schema, &batches).unwrap();
        assert_eq!(back.columns, vec!["a".to_string()]);
        assert!(back.rows.is_empty());
    }
}
