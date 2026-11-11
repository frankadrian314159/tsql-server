//! SQL Server data types and runtime values.
use serde::{Deserialize, Serialize};
use crate::decimal::Dec;
use crate::error::{Result, SqlError};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use std::fmt;
use uuid::Uuid;

pub const MAX: i32 = -1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DataType {
    Bit,
    TinyInt,
    SmallInt,
    Int,
    BigInt,
    Decimal(u8, u8),
    Money,
    SmallMoney,
    Float,
    Real,
    Char(i32),
    VarChar(i32),
    NChar(i32),
    NVarChar(i32),
    Text,
    NText,
    Binary(i32),
    VarBinary(i32),
    Image,
    Timestamp,
    Date,
    Time(u8),
    DateTime,
    SmallDateTime,
    DateTime2(u8),
    DateTimeOffset(u8),
    Guid,
    Xml,
    Variant,
    /// the binary form of a node of a tree (see hierarchyid.rs); a value is `Value::Bin`
    HierarchyId,
    /// planar and round-earth spatial values (see spatial.rs); a value is `Value::Bin`
    Geometry,
    Geography,
    Other(String),
}

impl DataType {
    /// Build from a (lower-case, alias-normalised) type name and numeric args (-1 for MAX).
    pub fn from_name(name: &str, args: &[i64], in_cast: bool) -> Result<DataType> {
        let n = name.to_ascii_lowercase();
        let a0 = args.first().copied();
        let len = |d: i64| -> i32 { a0.map(|x| x as i32).unwrap_or(if in_cast { 30 } else { d as i32 }) };
        Ok(match n.as_str() {
            "bit" => DataType::Bit,
            "tinyint" => DataType::TinyInt,
            "smallint" => DataType::SmallInt,
            "int" | "integer" => DataType::Int,
            "bigint" => DataType::BigInt,
            "decimal" | "numeric" | "dec" => {
                let p = a0.unwrap_or(18);
                let s = args.get(1).copied().unwrap_or(0);
                if !(1..=38).contains(&p) {
                    return Err(SqlError::new(2750, format!("Column, parameter, or variable #1: Specified column precision {} is greater than the maximum precision of 38.", p)));
                }
                if s < 0 || s > p {
                    return Err(SqlError::new(2754, format!("Error severity levels greater than 18 can only be specified by members of the sysadmin role, using the WITH LOG option. (scale {} invalid)", s)));
                }
                DataType::Decimal(p as u8, s as u8)
            }
            "money" => DataType::Money,
            "smallmoney" => DataType::SmallMoney,
            "float" => {
                if a0.map(|b| b <= 24).unwrap_or(false) {
                    DataType::Real
                } else {
                    DataType::Float
                }
            }
            "double" => DataType::Float,
            "real" => DataType::Real,
            "char" | "character" => DataType::Char(len(1)),
            "varchar" => DataType::VarChar(len(1)),
            "nchar" => DataType::NChar(len(1)),
            "nvarchar" => DataType::NVarChar(len(1)),
            "sysname" => DataType::NVarChar(128),
            "text" => DataType::Text,
            "ntext" => DataType::NText,
            "binary" => DataType::Binary(len(1)),
            "varbinary" => DataType::VarBinary(len(1)),
            "image" => DataType::Image,
            "timestamp" | "rowversion" => DataType::Timestamp,
            "date" => DataType::Date,
            "time" => DataType::Time(a0.unwrap_or(7) as u8),
            "datetime" => DataType::DateTime,
            "smalldatetime" => DataType::SmallDateTime,
            "datetime2" => DataType::DateTime2(a0.unwrap_or(7) as u8),
            "datetimeoffset" => DataType::DateTimeOffset(a0.unwrap_or(7) as u8),
            "uniqueidentifier" => DataType::Guid,
            "xml" => DataType::Xml,
            "hierarchyid" => DataType::HierarchyId,
            "sql_variant" => DataType::Variant,
            "geometry" => DataType::Geometry,
            "geography" => DataType::Geography,
            "json" | "cursor" => DataType::Other(n),
            _ => return Err(SqlError::new(2715, format!("Column, parameter, or variable #1: Cannot find data type {}.", name))),
        })
    }

    pub fn is_string(&self) -> bool {
        matches!(self, DataType::Char(_) | DataType::VarChar(_) | DataType::NChar(_) | DataType::NVarChar(_) | DataType::Text | DataType::NText)
    }
    pub fn is_binary(&self) -> bool {
        matches!(self, DataType::Binary(_) | DataType::VarBinary(_) | DataType::Image | DataType::Timestamp)
    }
    pub fn is_integer(&self) -> bool {
        matches!(self, DataType::TinyInt | DataType::SmallInt | DataType::Int | DataType::BigInt)
    }
    pub fn is_numeric(&self) -> bool {
        self.is_integer() || matches!(self, DataType::Bit | DataType::Decimal(..) | DataType::Money | DataType::SmallMoney | DataType::Float | DataType::Real)
    }
    pub fn is_datetime(&self) -> bool {
        matches!(self, DataType::Date | DataType::Time(_) | DataType::DateTime | DataType::SmallDateTime | DataType::DateTime2(_) | DataType::DateTimeOffset(_))
    }
    pub fn base_name(&self) -> &'static str {
        match self {
            DataType::Bit => "bit",
            DataType::TinyInt => "tinyint",
            DataType::SmallInt => "smallint",
            DataType::Int => "int",
            DataType::BigInt => "bigint",
            DataType::Decimal(..) => "decimal",
            DataType::Money => "money",
            DataType::SmallMoney => "smallmoney",
            DataType::Float => "float",
            DataType::Real => "real",
            DataType::Char(_) => "char",
            DataType::VarChar(_) => "varchar",
            DataType::NChar(_) => "nchar",
            DataType::NVarChar(_) => "nvarchar",
            DataType::Text => "text",
            DataType::NText => "ntext",
            DataType::Binary(_) => "binary",
            DataType::VarBinary(_) => "varbinary",
            DataType::Image => "image",
            DataType::Timestamp => "timestamp",
            DataType::Date => "date",
            DataType::Time(_) => "time",
            DataType::DateTime => "datetime",
            DataType::SmallDateTime => "smalldatetime",
            DataType::DateTime2(_) => "datetime2",
            DataType::DateTimeOffset(_) => "datetimeoffset",
            DataType::Guid => "uniqueidentifier",
            DataType::Xml => "xml",
            DataType::Variant => "sql_variant",
            DataType::HierarchyId => "hierarchyid",
            DataType::Geometry => "geometry",
            DataType::Geography => "geography",
            DataType::Other(_) => "other",
        }
    }
    /// Max character/byte length (for sys.columns and INFORMATION_SCHEMA).
    pub fn length(&self) -> Option<i32> {
        match self {
            DataType::Char(n) | DataType::VarChar(n) | DataType::NChar(n) | DataType::NVarChar(n) | DataType::Binary(n) | DataType::VarBinary(n) => Some(*n),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let l = |n: &i32| if *n == MAX { "max".to_string() } else { n.to_string() };
        match self {
            DataType::Char(n) | DataType::VarChar(n) | DataType::NChar(n) | DataType::NVarChar(n) | DataType::Binary(n) | DataType::VarBinary(n) => {
                write!(f, "{}({})", self.base_name(), l(n))
            }
            DataType::Decimal(p, s) => write!(f, "decimal({},{})", p, s),
            DataType::Time(s) | DataType::DateTime2(s) | DataType::DateTimeOffset(s) => write!(f, "{}({})", self.base_name(), s),
            DataType::Other(n) => write!(f, "{}", n),
            _ => write!(f, "{}", self.base_name()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DtKind {
    DateTime,
    SmallDateTime,
    DateTime2,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bit(bool),
    Int(i32),
    BigInt(i64),
    Dec(Dec),
    Float(f64),
    Str(String),
    Bin(Vec<u8>),
    Date(NaiveDate),
    Time(NaiveTime),
    DateTime(NaiveDateTime, DtKind),
    Dto(DateTime<FixedOffset>),
    Guid(Uuid),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }
    pub fn from_bool(b: bool) -> Value {
        Value::Bit(b)
    }
    /// Approximate type of a runtime value (for metadata / SELECT INTO).
    pub fn data_type(&self) -> DataType {
        match self {
            Value::Null => DataType::Int,
            Value::Bit(_) => DataType::Bit,
            Value::Int(_) => DataType::Int,
            Value::BigInt(_) => DataType::BigInt,
            Value::Dec(d) => DataType::Decimal(d.precision().min(38) as u8, d.s),
            Value::Float(_) => DataType::Float,
            Value::Str(s) => DataType::VarChar(if s.chars().count() >= 8000 { MAX } else { s.chars().count().max(1) as i32 }),
            Value::Bin(b) => DataType::VarBinary(b.len().max(1) as i32),
            Value::Date(_) => DataType::Date,
            Value::Time(_) => DataType::Time(7),
            Value::DateTime(_, DtKind::DateTime) => DataType::DateTime,
            Value::DateTime(_, DtKind::SmallDateTime) => DataType::SmallDateTime,
            Value::DateTime(_, DtKind::DateTime2) => DataType::DateTime2(7),
            Value::Dto(_) => DataType::DateTimeOffset(7),
            Value::Guid(_) => DataType::Guid,
        }
    }
    pub fn type_label(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bit(_) => "bit",
            Value::Int(_) => "int",
            Value::BigInt(_) => "bigint",
            Value::Dec(_) => "numeric",
            Value::Float(_) => "float",
            Value::Str(_) => "varchar",
            Value::Bin(_) => "varbinary",
            Value::Date(_) => "date",
            Value::Time(_) => "time",
            Value::DateTime(_, DtKind::DateTime) => "datetime",
            Value::DateTime(_, DtKind::SmallDateTime) => "smalldatetime",
            Value::DateTime(_, DtKind::DateTime2) => "datetime2",
            Value::Dto(_) => "datetimeoffset",
            Value::Guid(_) => "uniqueidentifier",
        }
    }
}
