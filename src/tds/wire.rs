//! TDS data encoding: byte readers/writers, column and parameter type information, and values.
//!
//! Column types are described by [`Wire`], which carries everything a TYPE_INFO needs; the same enum is used to
//! read parameters sent by the client and to write result columns.
use crate::convert;
use crate::decimal::Dec;
use crate::types::{DataType, DtKind, Value, MAX};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Datelike};
use uuid::Uuid;

pub type R<T> = Result<T, String>;

/// The collation sent with character columns: SQL_Latin1_General_CP1_CI_AS (code page 1252).
pub const COLLATION: [u8; 5] = [0x09, 0x04, 0xD0, 0x00, 0x34];
/// `maxlen` value meaning `(max)`: the value is sent as partially length-prefixed chunks.
pub const PLP: u16 = 0xFFFF;

// ----------------------------------------------------------------------------- reading
pub struct Rd<'a> {
    pub b: &'a [u8],
    pub p: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, p: 0 }
    }
    pub fn left(&self) -> usize {
        self.b.len() - self.p
    }
    pub fn take(&mut self, n: usize) -> R<&'a [u8]> {
        if self.left() < n {
            return Err("unexpected end of message".into());
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> R<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> R<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn utf16(&mut self, chars: usize) -> R<String> {
        let raw = self.take(chars * 2)?;
        Ok(decode_utf16(raw))
    }
    /// A string with a one-byte length in characters.
    pub fn bvarchar(&mut self) -> R<String> {
        let n = self.u8()? as usize;
        self.utf16(n)
    }
    /// Skip the ALL_HEADERS block that starts SQL batch, RPC and transaction-manager requests.
    pub fn all_headers(&mut self) -> R<()> {
        let total = self.u32()? as usize;
        if total < 4 || total - 4 > self.left() {
            return Err("bad ALL_HEADERS length".into());
        }
        self.take(total - 4)?;
        Ok(())
    }
}

pub fn decode_utf16(raw: &[u8]) -> String {
    let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

// ----------------------------------------------------------------------------- writing
pub fn put_utf16(out: &mut Vec<u8>, s: &str) {
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
}
/// A string with a one-byte length in characters.
pub fn put_bvar(out: &mut Vec<u8>, s: &str) {
    let s: String = s.chars().take(255).collect();
    out.push(s.encode_utf16().count() as u8);
    put_utf16(out, &s);
}
/// A string with a two-byte length in characters.
pub fn put_usvar(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.encode_utf16().count() as u16).to_le_bytes());
    put_utf16(out, s);
}

// ----------------------------------------------------------------------------- code page 1252
const CP1252: [(char, u8); 27] = [
    ('\u{20AC}', 0x80), ('\u{201A}', 0x82), ('\u{0192}', 0x83), ('\u{201E}', 0x84), ('\u{2026}', 0x85), ('\u{2020}', 0x86),
    ('\u{2021}', 0x87), ('\u{02C6}', 0x88), ('\u{2030}', 0x89), ('\u{0160}', 0x8A), ('\u{2039}', 0x8B), ('\u{0152}', 0x8C),
    ('\u{017D}', 0x8E), ('\u{2018}', 0x91), ('\u{2019}', 0x92), ('\u{201C}', 0x93), ('\u{201D}', 0x94), ('\u{2022}', 0x95),
    ('\u{2013}', 0x96), ('\u{2014}', 0x97), ('\u{02DC}', 0x98), ('\u{2122}', 0x99), ('\u{0161}', 0x9A), ('\u{203A}', 0x9B),
    ('\u{0153}', 0x9C), ('\u{017E}', 0x9E), ('\u{0178}', 0x9F),
];

/// Text to code page 1252 bytes; characters outside the code page become `?`.
pub fn to_cp1252(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| {
            let u = c as u32;
            if u < 0x80 || (0xA0..=0xFF).contains(&u) {
                u as u8
            } else {
                CP1252.iter().find(|(ch, _)| *ch == c).map(|(_, b)| *b).unwrap_or(b'?')
            }
        })
        .collect()
}

pub fn from_cp1252(b: &[u8]) -> String {
    b.iter()
        .map(|&x| {
            if (0x80..=0x9F).contains(&x) {
                CP1252.iter().find(|(_, b)| *b == x).map(|(c, _)| *c).unwrap_or('\u{FFFD}')
            } else {
                x as char
            }
        })
        .collect()
}

// ----------------------------------------------------------------------------- types
/// A column or parameter type as it appears in TYPE_INFO.
#[derive(Clone, Debug, PartialEq)]
pub enum Wire {
    Bit,
    /// Integer of 1, 2, 4 or 8 bytes (tinyint is unsigned).
    Int(u8),
    Dec(u8, u8),
    /// money (8) or smallmoney (4)
    Money(u8),
    /// float (8) or real (4)
    Float(u8),
    Char(u16),
    VarChar(u16),
    NChar(u16),
    NVarChar(u16),
    Binary(u16),
    VarBinary(u16),
    Date,
    Time(u8),
    /// datetime (8) or smalldatetime (4)
    DateTime(u8),
    DateTime2(u8),
    Dto(u8),
    Guid,
    Xml,
    /// `sql_variant`: a value carries its own base type
    Variant,
    /// A CLR user-defined type, which is how SQL Server sends `hierarchyid` (0), `geometry` (1) and `geography` (2): the
    /// bytes of the value, as partially length-prefixed chunks.
    Udt(u8),
    /// The legacy large object types `text`, `ntext` and `image`.
    Text,
    NText,
    Image,
}

/// A parsed TYPE_INFO: the type, and whether it is one of the fixed-length types whose values carry no length byte.
#[derive(Clone, Debug)]
pub struct Ti {
    pub w: Wire,
    pub fixed: bool,
}

impl Wire {
    fn is_plp(&self) -> bool {
        matches!(self, Wire::VarChar(PLP) | Wire::NVarChar(PLP) | Wire::VarBinary(PLP) | Wire::Xml | Wire::Udt(_))
    }

    /// Does a column of this type carry the name of its table in COLMETADATA (the legacy large objects do)?
    pub fn has_table_name(&self) -> bool {
        matches!(self, Wire::Text | Wire::NText | Wire::Image)
    }

    /// The wire type that carries a column of SQL type `dt`; `observed` is the longest value in the column
    /// (characters or bytes) so a computed column whose declared length is too short is widened.
    pub fn for_type(dt: &DataType, observed: usize) -> Wire {
        let var = |n: i32, per: usize| -> Option<u16> {
            if n == MAX || n < 0 {
                return None;
            }
            let bytes = (n as usize).max(observed) * per;
            if bytes > 8000 {
                None
            } else {
                Some(bytes as u16)
            }
        };
        match dt {
            DataType::Bit => Wire::Bit,
            DataType::TinyInt => Wire::Int(1),
            DataType::SmallInt => Wire::Int(2),
            DataType::Int => Wire::Int(4),
            DataType::BigInt => Wire::Int(8),
            DataType::Decimal(p, s) => Wire::Dec((*p).clamp(1, 38), *s),
            DataType::Money => Wire::Money(8),
            DataType::SmallMoney => Wire::Money(4),
            DataType::Float => Wire::Float(8),
            DataType::Real => Wire::Float(4),
            DataType::Char(n) => var(*n, 1).map(|l| if *n >= 0 && l as i32 == *n { Wire::Char(l) } else { Wire::VarChar(l) }).unwrap_or(Wire::VarChar(PLP)),
            DataType::VarChar(n) => var(*n, 1).map(Wire::VarChar).unwrap_or(Wire::VarChar(PLP)),
            DataType::NChar(n) => var(*n, 2).map(|l| if *n >= 0 && l as i32 == *n * 2 { Wire::NChar(l) } else { Wire::NVarChar(l) }).unwrap_or(Wire::NVarChar(PLP)),
            DataType::NVarChar(n) => var(*n, 2).map(Wire::NVarChar).unwrap_or(Wire::NVarChar(PLP)),
            DataType::Text => Wire::Text,
            DataType::NText => Wire::NText,
            DataType::Binary(n) => var(*n, 1).map(|l| if *n >= 0 && l as i32 == *n { Wire::Binary(l) } else { Wire::VarBinary(l) }).unwrap_or(Wire::VarBinary(PLP)),
            DataType::VarBinary(n) => var(*n, 1).map(Wire::VarBinary).unwrap_or(Wire::VarBinary(PLP)),
            DataType::Image => Wire::Image,
            DataType::Timestamp => Wire::Binary(8),
            DataType::Date => Wire::Date,
            DataType::Time(s) => Wire::Time((*s).min(7)),
            DataType::DateTime => Wire::DateTime(8),
            DataType::SmallDateTime => Wire::DateTime(4),
            DataType::DateTime2(s) => Wire::DateTime2((*s).min(7)),
            DataType::DateTimeOffset(s) => Wire::Dto((*s).min(7)),
            DataType::Guid => Wire::Guid,
            DataType::Xml => Wire::Xml,
            DataType::HierarchyId => Wire::Udt(0),
            DataType::Geometry => Wire::Udt(1),
            DataType::Geography => Wire::Udt(2),
            DataType::Variant => Wire::Variant,
            DataType::Other(_) => Wire::NVarChar(PLP),
        }
    }

    /// T-SQL name of the type, used to declare variables that receive OUTPUT parameters.
    pub fn sql_name(&self) -> String {
        let len = |n: u16, per: u16| if n == PLP { "max".to_string() } else { (n / per).max(1).to_string() };
        match self {
            Wire::Bit => "bit".into(),
            Wire::Int(1) => "tinyint".into(),
            Wire::Int(2) => "smallint".into(),
            Wire::Int(4) => "int".into(),
            Wire::Int(_) => "bigint".into(),
            Wire::Dec(p, s) => format!("decimal({},{})", p, s),
            Wire::Money(4) => "smallmoney".into(),
            Wire::Money(_) => "money".into(),
            Wire::Float(4) => "real".into(),
            Wire::Float(_) => "float".into(),
            Wire::Char(n) => format!("char({})", len(*n, 1)),
            Wire::VarChar(n) => format!("varchar({})", len(*n, 1)),
            Wire::NChar(n) => format!("nchar({})", len(*n, 2)),
            Wire::NVarChar(n) => format!("nvarchar({})", len(*n, 2)),
            Wire::Binary(n) => format!("binary({})", len(*n, 1)),
            Wire::VarBinary(n) => format!("varbinary({})", len(*n, 1)),
            Wire::Date => "date".into(),
            Wire::Time(s) => format!("time({})", s),
            Wire::DateTime(4) => "smalldatetime".into(),
            Wire::DateTime(_) => "datetime".into(),
            Wire::DateTime2(s) => format!("datetime2({})", s),
            Wire::Dto(s) => format!("datetimeoffset({})", s),
            Wire::Guid => "uniqueidentifier".into(),
            Wire::Xml => "xml".into(),
            Wire::Variant => "sql_variant".into(),
            Wire::Udt(0) => "hierarchyid".into(),
            Wire::Udt(1) => "geometry".into(),
            Wire::Udt(_) => "geography".into(),
            Wire::Text => "varchar(max)".into(),
            Wire::NText => "nvarchar(max)".into(),
            Wire::Image => "varbinary(max)".into(),
        }
    }
}

/// The name, and the assembly-qualified name, of the CLR type that carries `hierarchyid`, `geometry` or `geography`.
fn udt_names(kind: u8) -> (&'static str, &'static str) {
    let (name, class) = match kind {
        0 => ("hierarchyid", "SqlHierarchyId"),
        1 => ("geometry", "SqlGeometry"),
        _ => ("geography", "SqlGeography"),
    };
    let _ = class;
    match kind {
        0 => (name, "Microsoft.SqlServer.Types.SqlHierarchyId, Microsoft.SqlServer.Types, Version=11.0.0.0, Culture=neutral, PublicKeyToken=89845dcd8080cc91"),
        1 => (name, "Microsoft.SqlServer.Types.SqlGeometry, Microsoft.SqlServer.Types, Version=11.0.0.0, Culture=neutral, PublicKeyToken=89845dcd8080cc91"),
        _ => (name, "Microsoft.SqlServer.Types.SqlGeography, Microsoft.SqlServer.Types, Version=11.0.0.0, Culture=neutral, PublicKeyToken=89845dcd8080cc91"),
    }
}

/// Number of bytes (including the sign byte) of a decimal of precision `p`.
fn dec_len(p: u8) -> usize {
    match p {
        0..=9 => 5,
        10..=19 => 9,
        20..=28 => 13,
        _ => 17,
    }
}

/// Number of bytes of the time part for a `time(scale)`.
fn time_len(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

pub fn write_typeinfo(out: &mut Vec<u8>, w: &Wire) {
    match w {
        Wire::Bit => out.extend_from_slice(&[0x68, 1]),
        Wire::Int(n) => out.extend_from_slice(&[0x26, *n]),
        Wire::Dec(p, s) => out.extend_from_slice(&[0x6C, 17, *p, *s]),
        Wire::Money(n) => out.extend_from_slice(&[0x6E, *n]),
        Wire::Float(n) => out.extend_from_slice(&[0x6D, *n]),
        Wire::Char(n) | Wire::VarChar(n) | Wire::NChar(n) | Wire::NVarChar(n) => {
            out.push(match w {
                Wire::Char(_) => 0xAF,
                Wire::VarChar(_) => 0xA7,
                Wire::NChar(_) => 0xEF,
                _ => 0xE7,
            });
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(&COLLATION);
        }
        Wire::Binary(n) | Wire::VarBinary(n) => {
            out.push(if matches!(w, Wire::Binary(_)) { 0xAD } else { 0xA5 });
            out.extend_from_slice(&n.to_le_bytes());
        }
        Wire::Date => out.push(0x28),
        Wire::Time(s) => out.extend_from_slice(&[0x29, *s]),
        Wire::DateTime(n) => out.extend_from_slice(&[0x6F, *n]),
        Wire::DateTime2(s) => out.extend_from_slice(&[0x2A, *s]),
        Wire::Dto(s) => out.extend_from_slice(&[0x2B, *s]),
        Wire::Guid => out.extend_from_slice(&[0x24, 16]),
        Wire::Xml => out.extend_from_slice(&[0xF1, 0]),
        Wire::Variant => {
            out.push(0x62);
            out.extend_from_slice(&8009u32.to_le_bytes());
        }
        Wire::Udt(kind) => {
            let (name, assembly) = udt_names(*kind);
            out.push(0xF0);
            out.extend_from_slice(&PLP.to_le_bytes());
            put_bvar(out, "");
            put_bvar(out, "sys");
            put_bvar(out, name);
            put_usvar(out, assembly);
        }
        Wire::Text | Wire::NText => {
            out.push(if matches!(w, Wire::Text) { 0x23 } else { 0x63 });
            out.extend_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
            out.extend_from_slice(&COLLATION);
        }
        Wire::Image => {
            out.push(0x22);
            out.extend_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        }
    }
}

/// What follows the TYPE_INFO of a column in COLMETADATA when the type is one of the legacy large objects: the name of
/// the table it belongs to (not known here, so it is empty).
pub fn write_table_name(out: &mut Vec<u8>, w: &Wire) {
    if w.has_table_name() {
        out.push(1);
        put_usvar(out, "");
    }
}

pub fn read_typeinfo(rd: &mut Rd) -> R<Ti> {
    let tok = rd.u8()?;
    let fixed = |w| Ok(Ti { w, fixed: true });
    let var = |w| Ok(Ti { w, fixed: false });
    match tok {
        0x30 => fixed(Wire::Int(1)),
        0x32 => fixed(Wire::Bit),
        0x34 => fixed(Wire::Int(2)),
        0x38 => fixed(Wire::Int(4)),
        0x7F => fixed(Wire::Int(8)),
        0x3B => fixed(Wire::Float(4)),
        0x3E => fixed(Wire::Float(8)),
        0x3C => fixed(Wire::Money(8)),
        0x7A => fixed(Wire::Money(4)),
        0x3D => fixed(Wire::DateTime(8)),
        0x3A => fixed(Wire::DateTime(4)),
        0x26 => var(Wire::Int(rd.u8()?)),
        0x68 => {
            rd.u8()?;
            var(Wire::Bit)
        }
        0x6D => var(Wire::Float(rd.u8()?)),
        0x6E => var(Wire::Money(rd.u8()?)),
        0x6F => var(Wire::DateTime(rd.u8()?)),
        0x24 => {
            rd.u8()?;
            var(Wire::Guid)
        }
        0x6A | 0x6C => {
            rd.u8()?;
            let p = rd.u8()?;
            let s = rd.u8()?;
            if !(1..=38).contains(&p) || s > p {
                return Err("bad decimal precision or scale".into());
            }
            var(Wire::Dec(p, s))
        }
        0x28 => var(Wire::Date),
        0x29 => var(Wire::Time(rd.u8()?.min(7))),
        0x2A => var(Wire::DateTime2(rd.u8()?.min(7))),
        0x2B => var(Wire::Dto(rd.u8()?.min(7))),
        0xA7 | 0xAF | 0xE7 | 0xEF => {
            let n = rd.u16()?;
            rd.take(5)?;
            var(match tok {
                0xA7 => Wire::VarChar(n),
                0xAF => Wire::Char(n),
                0xE7 => Wire::NVarChar(n),
                _ => Wire::NChar(n),
            })
        }
        0xA5 | 0xAD => {
            let n = rd.u16()?;
            var(if tok == 0xA5 { Wire::VarBinary(n) } else { Wire::Binary(n) })
        }
        0xF1 => {
            if rd.u8()? == 1 {
                rd.bvarchar()?;
                rd.bvarchar()?;
                let n = rd.u16()? as usize;
                rd.utf16(n)?;
            }
            var(Wire::Xml)
        }
        0x62 => {
            rd.u32()?;
            var(Wire::Variant)
        }
        0xF0 => {
            rd.u16()?;
            rd.bvarchar()?;
            rd.bvarchar()?;
            let name = rd.bvarchar()?;
            let n = rd.u16()? as usize;
            rd.utf16(n)?;
            var(match name.to_ascii_lowercase().as_str() {
                "hierarchyid" => Wire::Udt(0),
                "geometry" => Wire::Udt(1),
                "geography" => Wire::Udt(2),
                // another CLR type: its bytes can still be carried
                _ => Wire::VarBinary(PLP),
            })
        }
        0x23 | 0x63 => {
            rd.u32()?;
            rd.take(5)?;
            // a table name follows in COLMETADATA, which the caller of a column list skips (`skip_table_name`)
            var(if tok == 0x23 { Wire::Text } else { Wire::NText })
        }
        0x22 => {
            rd.u32()?;
            var(Wire::Image)
        }
        other => Err(format!("unsupported parameter type 0x{:02X}", other)),
    }
}

/// Skip the table name that follows the TYPE_INFO of a legacy large object column in COLMETADATA.
pub fn skip_table_name(rd: &mut Rd, w: &Wire) -> R<()> {
    if w.has_table_name() {
        let parts = rd.u8()?;
        for _ in 0..parts {
            let n = rd.u16()? as usize;
            rd.utf16(n)?;
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------- values: writing
fn base_1900() -> NaiveDate {
    NaiveDate::from_ymd_opt(1900, 1, 1).unwrap()
}

fn as_type(v: &Value, dt: &DataType) -> Value {
    convert::cast(v, dt, None).unwrap_or(Value::Null)
}

fn time_ticks(t: &NaiveTime, scale: u8) -> u64 {
    let nanos = t.num_seconds_from_midnight() as u64 * 1_000_000_000 + (t.nanosecond() % 1_000_000_000) as u64;
    nanos / 10u64.pow(9 - scale as u32)
}

fn put_time(out: &mut Vec<u8>, t: &NaiveTime, scale: u8) {
    let ticks = time_ticks(t, scale);
    out.extend_from_slice(&ticks.to_le_bytes()[..time_len(scale)]);
}

fn put_date(out: &mut Vec<u8>, d: &NaiveDate) {
    let days = (d.num_days_from_ce() - 1).max(0) as u32;
    out.extend_from_slice(&days.to_le_bytes()[..3]);
}

fn put_plp(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    if !bytes.is_empty() {
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
}

/// Write one value of type `w` (the length prefix included).
pub fn write_value(out: &mut Vec<u8>, w: &Wire, v: &Value) {
    if v.is_null() {
        if matches!(w, Wire::Variant) {
            out.extend_from_slice(&0u32.to_le_bytes());
        } else if w.has_table_name() {
            out.push(0);
        } else if w.is_plp() {
            out.extend_from_slice(&u64::MAX.to_le_bytes());
        } else if matches!(w, Wire::Char(_) | Wire::VarChar(_) | Wire::NChar(_) | Wire::NVarChar(_) | Wire::Binary(_) | Wire::VarBinary(_)) {
            out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        } else {
            out.push(0);
        }
        return;
    }
    match w {
        Wire::Bit => {
            let b = match v {
                Value::Bit(b) => *b,
                o => matches!(as_type(o, &DataType::Bit), Value::Bit(true)),
            };
            out.extend_from_slice(&[1, b as u8]);
        }
        Wire::Int(n) => {
            let i = match v {
                Value::Int(i) => *i as i64,
                Value::BigInt(i) => *i,
                Value::Bit(b) => *b as i64,
                o => match as_type(o, &DataType::BigInt) {
                    Value::BigInt(i) => i,
                    Value::Int(i) => i as i64,
                    _ => 0,
                },
            };
            out.push(*n);
            out.extend_from_slice(&i.to_le_bytes()[..*n as usize]);
        }
        Wire::Dec(p, s) => {
            let d = match v {
                Value::Dec(d) if d.s == *s => *d,
                o => match as_type(o, &DataType::Decimal(*p, *s)) {
                    Value::Dec(d) => d,
                    _ => Dec::new(0, *s),
                },
            };
            let n = dec_len(*p);
            out.push(n as u8);
            out.push(if d.m < 0 { 0 } else { 1 });
            out.extend_from_slice(&d.m.unsigned_abs().to_le_bytes()[..n - 1]);
        }
        Wire::Money(n) => {
            let d = match as_type(v, if *n == 8 { &DataType::Money } else { &DataType::SmallMoney }) {
                Value::Dec(d) => d,
                _ => Dec::new(0, 4),
            };
            let m = convert::cast(&Value::Dec(d), &DataType::Decimal(38, 4), None).ok().and_then(|x| if let Value::Dec(d) = x { Some(d.m as i64) } else { None }).unwrap_or(0);
            out.push(*n);
            if *n == 8 {
                out.extend_from_slice(&((m >> 32) as i32).to_le_bytes());
                out.extend_from_slice(&(m as u32).to_le_bytes());
            } else {
                out.extend_from_slice(&(m as i32).to_le_bytes());
            }
        }
        Wire::Float(n) => {
            let f = match v {
                Value::Float(f) => *f,
                o => match as_type(o, &DataType::Float) {
                    Value::Float(f) => f,
                    _ => 0.0,
                },
            };
            out.push(*n);
            if *n == 8 {
                out.extend_from_slice(&f.to_le_bytes());
            } else {
                out.extend_from_slice(&(f as f32).to_le_bytes());
            }
        }
        Wire::Char(_) | Wire::VarChar(_) => {
            let bytes = to_cp1252(&convert::to_str(v));
            if w.is_plp() {
                put_plp(out, &bytes);
            } else {
                out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                out.extend_from_slice(&bytes);
            }
        }
        Wire::NChar(_) | Wire::NVarChar(_) => {
            let mut bytes = vec![];
            put_utf16(&mut bytes, &convert::to_str(v));
            if w.is_plp() {
                put_plp(out, &bytes);
            } else {
                out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                out.extend_from_slice(&bytes);
            }
        }
        Wire::Binary(_) | Wire::VarBinary(_) => {
            let bytes = match v {
                // a geometry or geography leaves the server as SQL Server's bytes, without the kind flag
                Value::Bin(b) => crate::spatial::external(b),
                o => convert::to_str(o).into_bytes(),
            };
            if w.is_plp() {
                put_plp(out, &bytes);
            } else {
                out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                out.extend_from_slice(&bytes);
            }
        }
        Wire::Xml => {
            let mut bytes = vec![];
            put_utf16(&mut bytes, &convert::to_str(v));
            put_plp(out, &bytes);
        }
        Wire::Udt(_) => {
            let bytes = match v {
                Value::Bin(b) => crate::spatial::external(b),
                o => convert::to_str(o).into_bytes(),
            };
            put_plp(out, &bytes);
        }
        Wire::Text | Wire::NText | Wire::Image => {
            let bytes = match w {
                Wire::Text => to_cp1252(&convert::to_str(v)),
                Wire::NText => {
                    let mut b = vec![];
                    put_utf16(&mut b, &convert::to_str(v));
                    b
                }
                _ => match v {
                    Value::Bin(b) => b.clone(),
                    o => convert::to_str(o).into_bytes(),
                },
            };
            // a text pointer (16 bytes) and a timestamp (8), which clients keep and never use here, then the length
            out.push(16);
            out.extend_from_slice(&[0x01; 16]);
            out.extend_from_slice(&[0u8; 8]);
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&bytes);
        }
        Wire::Variant => write_variant(out, v),
        Wire::Date => {
            let d = match as_type(v, &DataType::Date) {
                Value::Date(d) => d,
                _ => NaiveDate::from_ymd_opt(1, 1, 1).unwrap(),
            };
            out.push(3);
            put_date(out, &d);
        }
        Wire::Time(s) => {
            let t = match as_type(v, &DataType::Time(*s)) {
                Value::Time(t) => t,
                _ => NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            };
            out.push(time_len(*s) as u8);
            put_time(out, &t, *s);
        }
        Wire::DateTime(4) => {
            let d = naive(v, &DataType::SmallDateTime);
            out.push(4);
            let days = (d.date() - base_1900()).num_days().clamp(0, 65535) as u16;
            let mins = (d.time().num_seconds_from_midnight() / 60) as u16;
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&mins.to_le_bytes());
        }
        Wire::DateTime(_) => {
            let d = naive(v, &DataType::DateTime);
            out.push(8);
            let days = (d.date() - base_1900()).num_days() as i32;
            let nanos = d.time().num_seconds_from_midnight() as u64 * 1_000_000_000 + (d.time().nanosecond() % 1_000_000_000) as u64;
            let ticks = ((nanos * 3 + 5_000_000) / 10_000_000).min(25_919_999) as u32;
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&ticks.to_le_bytes());
        }
        Wire::DateTime2(s) => {
            let d = naive(v, &DataType::DateTime2(*s));
            out.push((time_len(*s) + 3) as u8);
            put_time(out, &d.time(), *s);
            put_date(out, &d.date());
        }
        Wire::Dto(s) => {
            let d = match as_type(v, &DataType::DateTimeOffset(*s)) {
                Value::Dto(d) => d,
                _ => DateTime::<FixedOffset>::from_naive_utc_and_offset(NaiveDateTime::default(), FixedOffset::east_opt(0).unwrap()),
            };
            let utc = d.naive_utc();
            out.push((time_len(*s) + 5) as u8);
            put_time(out, &utc.time(), *s);
            put_date(out, &utc.date());
            out.extend_from_slice(&((d.offset().local_minus_utc() / 60) as i16).to_le_bytes());
        }
        Wire::Guid => {
            let g = match v {
                Value::Guid(g) => *g,
                o => match as_type(o, &DataType::Guid) {
                    Value::Guid(g) => g,
                    _ => Uuid::nil(),
                },
            };
            out.push(16);
            out.extend_from_slice(&g.to_bytes_le());
        }
    }
}

fn naive(v: &Value, dt: &DataType) -> NaiveDateTime {
    match v {
        Value::DateTime(d, _) => *d,
        o => match as_type(o, dt) {
            Value::DateTime(d, _) => d,
            Value::Date(d) => d.and_hms_opt(0, 0, 0).unwrap(),
            _ => NaiveDateTime::default(),
        },
    }
}

// ----------------------------------------------------------------------------- values: reading
fn read_plp(rd: &mut Rd) -> R<Option<Vec<u8>>> {
    let total = rd.u64()?;
    if total == u64::MAX {
        return Ok(None);
    }
    let mut data = vec![];
    loop {
        let n = rd.u32()? as usize;
        if n == 0 {
            break;
        }
        data.extend_from_slice(rd.take(n)?);
    }
    Ok(Some(data))
}

fn date_from(b: &[u8]) -> R<NaiveDate> {
    let days = u32::from_le_bytes([b[0], b[1], b[2], 0]);
    NaiveDate::from_ymd_opt(1, 1, 1).unwrap().checked_add_days(chrono::Days::new(days as u64)).ok_or_else(|| "date out of range".to_string())
}

fn time_from(b: &[u8], scale: u8) -> NaiveTime {
    let mut raw = [0u8; 8];
    raw[..b.len()].copy_from_slice(b);
    let nanos = u64::from_le_bytes(raw) * 10u64.pow(9 - scale as u32);
    NaiveTime::from_num_seconds_from_midnight_opt((nanos / 1_000_000_000) as u32 % 86400, (nanos % 1_000_000_000) as u32).unwrap_or_default()
}

/// Read one value of the given type.
pub fn read_value(rd: &mut Rd, ti: &Ti) -> R<Value> {
    let w = &ti.w;
    match w {
        Wire::Variant => return read_variant(rd),
        Wire::Text | Wire::NText | Wire::Image => {
            let ptr = rd.u8()? as usize;
            if ptr == 0 {
                return Ok(Value::Null);
            }
            rd.take(ptr)?;
            rd.take(8)?;
            let n = rd.u32()? as usize;
            let d = rd.take(n)?;
            return Ok(match w {
                Wire::Text => Value::Str(from_cp1252(d)),
                Wire::NText => Value::Str(decode_utf16(d)),
                _ => Value::Bin(d.to_vec()),
            });
        }
        Wire::Udt(_) => {
            return Ok(match read_plp(rd)? {
                None => Value::Null,
                Some(d) => Value::Bin(d),
            })
        }
        _ => {}
    }
    // length of the value in bytes, or None for NULL
    let len = if ti.fixed {
        Some(match w {
            Wire::Int(n) | Wire::Money(n) | Wire::Float(n) | Wire::DateTime(n) => *n as usize,
            _ => 1,
        })
    } else {
        match w {
            Wire::Char(_) | Wire::VarChar(_) | Wire::NChar(_) | Wire::NVarChar(_) | Wire::Binary(_) | Wire::VarBinary(_) if !w.is_plp() => {
                let n = rd.u16()?;
                if n == 0xFFFF {
                    None
                } else {
                    Some(n as usize)
                }
            }
            _ if w.is_plp() => {
                return Ok(match read_plp(rd)? {
                    None => Value::Null,
                    Some(d) => match w {
                        Wire::VarChar(_) => Value::Str(from_cp1252(&d)),
                        Wire::VarBinary(_) => Value::Bin(d),
                        _ => Value::Str(decode_utf16(&d)),
                    },
                })
            }
            Wire::Bit => Some(rd.u8()? as usize).filter(|n| *n > 0),
            _ => Some(rd.u8()? as usize).filter(|n| *n > 0),
        }
    };
    let n = match len {
        None | Some(0) if !matches!(w, Wire::Char(_) | Wire::VarChar(_) | Wire::NChar(_) | Wire::NVarChar(_) | Wire::Binary(_) | Wire::VarBinary(_)) => return Ok(Value::Null),
        None => return Ok(Value::Null),
        Some(n) => n,
    };
    let b = rd.take(n)?;
    let short = || "value is shorter than its type requires".to_string();
    Ok(match w {
        Wire::Bit => Value::Bit(b[0] != 0),
        Wire::Int(1) => Value::Int(b[0] as i32),
        Wire::Int(2) => Value::Int(i16::from_le_bytes(b.get(..2).ok_or_else(short)?.try_into().unwrap()) as i32),
        Wire::Int(4) => Value::Int(i32::from_le_bytes(b.get(..4).ok_or_else(short)?.try_into().unwrap())),
        Wire::Int(_) => Value::BigInt(i64::from_le_bytes(b.get(..8).ok_or_else(short)?.try_into().unwrap())),
        Wire::Dec(_, s) => {
            if n < 2 || n > 17 {
                return Err(short());
            }
            let mut raw = [0u8; 16];
            raw[..n - 1].copy_from_slice(&b[1..]);
            let m = u128::from_le_bytes(raw) as i128;
            Value::Dec(Dec::new(if b[0] == 0 { -m } else { m }, *s))
        }
        Wire::Money(4) => Value::Dec(Dec::new(i32::from_le_bytes(b.get(..4).ok_or_else(short)?.try_into().unwrap()) as i128, 4)),
        Wire::Money(_) => {
            let hi = i32::from_le_bytes(b.get(..4).ok_or_else(short)?.try_into().unwrap()) as i64;
            let lo = u32::from_le_bytes(b.get(4..8).ok_or_else(short)?.try_into().unwrap()) as i64;
            Value::Dec(Dec::new(((hi << 32) | lo) as i128, 4))
        }
        Wire::Float(4) => Value::Float(f32::from_le_bytes(b.get(..4).ok_or_else(short)?.try_into().unwrap()) as f64),
        Wire::Float(_) => Value::Float(f64::from_le_bytes(b.get(..8).ok_or_else(short)?.try_into().unwrap())),
        Wire::Char(_) | Wire::VarChar(_) => Value::Str(from_cp1252(b)),
        Wire::NChar(_) | Wire::NVarChar(_) => Value::Str(decode_utf16(b)),
        Wire::Binary(_) | Wire::VarBinary(_) => Value::Bin(b.to_vec()),
        Wire::Xml => Value::Str(decode_utf16(b)),
        Wire::Date => Value::Date(date_from(b.get(..3).ok_or_else(short)?)?),
        Wire::Time(s) => Value::Time(time_from(b, *s)),
        Wire::DateTime(4) => {
            let days = u16::from_le_bytes([b[0], b[1]]) as i64;
            let mins = u16::from_le_bytes([b[2], b[3]]) as i64;
            Value::DateTime(base_1900().and_hms_opt(0, 0, 0).unwrap() + chrono::Duration::days(days) + chrono::Duration::minutes(mins), DtKind::SmallDateTime)
        }
        Wire::DateTime(_) => {
            let days = i32::from_le_bytes(b.get(..4).ok_or_else(short)?.try_into().unwrap()) as i64;
            let ticks = u32::from_le_bytes(b.get(4..8).ok_or_else(short)?.try_into().unwrap()) as i64;
            let ms = (ticks * 10 + 1) / 3;
            Value::DateTime(base_1900().and_hms_opt(0, 0, 0).unwrap() + chrono::Duration::days(days) + chrono::Duration::milliseconds(ms), DtKind::DateTime)
        }
        Wire::DateTime2(s) => {
            let tl = time_len(*s);
            if n != tl + 3 {
                return Err(short());
            }
            Value::DateTime(NaiveDateTime::new(date_from(&b[tl..])?, time_from(&b[..tl], *s)), DtKind::DateTime2)
        }
        Wire::Dto(s) => {
            let tl = time_len(*s);
            if n != tl + 5 {
                return Err(short());
            }
            let utc = NaiveDateTime::new(date_from(&b[tl..tl + 3])?, time_from(&b[..tl], *s));
            let off = i16::from_le_bytes([b[tl + 3], b[tl + 4]]) as i32 * 60;
            let tz = FixedOffset::east_opt(off).ok_or("bad time zone offset")?;
            Value::Dto(DateTime::<FixedOffset>::from_naive_utc_and_offset(utc, tz))
        }
        Wire::Guid => Value::Guid(Uuid::from_bytes_le(b.try_into().map_err(|_| short())?)),
        Wire::Variant | Wire::Udt(_) | Wire::Text | Wire::NText | Wire::Image => unreachable!("handled above"),
    })
}

// ----------------------------------------------------------------------------- sql_variant
/// A `sql_variant` value: its total length, its base type, the base type's properties, then the value's bytes.
fn write_variant(out: &mut Vec<u8>, v: &Value) {
    let mut body = vec![];
    let mut put_str = |body: &mut Vec<u8>, bytes: &[u8], base: u8| {
        body.push(base);
        body.push(7);
        body.extend_from_slice(&COLLATION);
        body.extend_from_slice(&(bytes.len().min(8000) as u16).to_le_bytes());
        body.extend_from_slice(&bytes[..bytes.len().min(8000)]);
    };
    match v {
        Value::Int(i) => {
            body.extend_from_slice(&[0x38, 0]);
            body.extend_from_slice(&i.to_le_bytes());
        }
        Value::BigInt(i) => {
            body.extend_from_slice(&[0x7F, 0]);
            body.extend_from_slice(&i.to_le_bytes());
        }
        Value::Bit(b) => body.extend_from_slice(&[0x32, 0, *b as u8]),
        Value::Float(f) => {
            body.extend_from_slice(&[0x3E, 0]);
            body.extend_from_slice(&f.to_le_bytes());
        }
        Value::Dec(d) => {
            let p = d.precision().clamp(1, 38) as u8;
            body.extend_from_slice(&[0x6A, 2, p, d.s]);
            body.push(if d.m < 0 { 0 } else { 1 });
            body.extend_from_slice(&d.m.unsigned_abs().to_le_bytes()[..dec_len(p) - 1]);
        }
        Value::Str(s) => {
            let mut b = vec![];
            put_utf16(&mut b, s);
            put_str(&mut body, &b, 0xE7);
        }
        Value::Bin(b) => {
            body.extend_from_slice(&[0xA5, 2]);
            body.extend_from_slice(&(b.len().min(8000) as u16).to_le_bytes());
            body.extend_from_slice(&b[..b.len().min(8000)]);
        }
        Value::Date(d) => {
            body.extend_from_slice(&[0x28, 0]);
            put_date(&mut body, d);
        }
        Value::Time(t) => {
            body.extend_from_slice(&[0x29, 1, 7]);
            put_time(&mut body, t, 7);
        }
        Value::DateTime(d, DtKind::DateTime2) => {
            body.extend_from_slice(&[0x2A, 1, 7]);
            put_time(&mut body, &d.time(), 7);
            put_date(&mut body, &d.date());
        }
        Value::DateTime(d, _) => {
            body.extend_from_slice(&[0x3D, 0]);
            let days = (d.date() - base_1900()).num_days() as i32;
            let nanos = d.time().num_seconds_from_midnight() as u64 * 1_000_000_000 + (d.time().nanosecond() % 1_000_000_000) as u64;
            let ticks = ((nanos * 3 + 5_000_000) / 10_000_000).min(25_919_999) as u32;
            body.extend_from_slice(&days.to_le_bytes());
            body.extend_from_slice(&ticks.to_le_bytes());
        }
        Value::Dto(d) => {
            let utc = d.naive_utc();
            body.extend_from_slice(&[0x2B, 1, 7]);
            put_time(&mut body, &utc.time(), 7);
            put_date(&mut body, &utc.date());
            body.extend_from_slice(&((d.offset().local_minus_utc() / 60) as i16).to_le_bytes());
        }
        Value::Guid(g) => {
            body.extend_from_slice(&[0x24, 0]);
            body.extend_from_slice(&g.to_bytes_le());
        }
        Value::Null => {
            out.extend_from_slice(&0u32.to_le_bytes());
            return;
        }
    }
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
}

fn read_variant(rd: &mut Rd) -> R<Value> {
    let total = rd.u32()? as usize;
    if total == 0 {
        return Ok(Value::Null);
    }
    let body = rd.take(total)?;
    let mut r = Rd::new(body);
    let base = r.u8()?;
    let props = r.u8()? as usize;
    let p = r.take(props)?;
    let rest = &body[r.p..];
    let short = || "sql_variant value is shorter than its type requires".to_string();
    Ok(match base {
        0x38 => Value::Int(i32::from_le_bytes(rest.get(..4).ok_or_else(short)?.try_into().unwrap())),
        0x30 => Value::Int(*rest.first().ok_or_else(short)? as i32),
        0x34 => Value::Int(i16::from_le_bytes(rest.get(..2).ok_or_else(short)?.try_into().unwrap()) as i32),
        0x7F => Value::BigInt(i64::from_le_bytes(rest.get(..8).ok_or_else(short)?.try_into().unwrap())),
        0x32 => Value::Bit(*rest.first().ok_or_else(short)? != 0),
        0x3E => Value::Float(f64::from_le_bytes(rest.get(..8).ok_or_else(short)?.try_into().unwrap())),
        0x3B => Value::Float(f32::from_le_bytes(rest.get(..4).ok_or_else(short)?.try_into().unwrap()) as f64),
        0x6A | 0x6C => {
            let s = *p.get(1).ok_or_else(short)?;
            if rest.len() < 2 || rest.len() > 17 {
                return Err(short());
            }
            let mut raw = [0u8; 16];
            raw[..rest.len() - 1].copy_from_slice(&rest[1..]);
            let m = u128::from_le_bytes(raw) as i128;
            Value::Dec(Dec::new(if rest[0] == 0 { -m } else { m }, s))
        }
        0xE7 | 0xEF => Value::Str(decode_utf16(rest)),
        0xA7 | 0xAF => Value::Str(from_cp1252(rest)),
        0xA5 | 0xAD => Value::Bin(rest.to_vec()),
        0x28 => Value::Date(date_from(rest.get(..3).ok_or_else(short)?)?),
        0x29 => {
            let s = *p.first().ok_or_else(short)?;
            Value::Time(time_from(rest, s.min(7)))
        }
        0x2A => {
            let s = (*p.first().ok_or_else(short)?).min(7);
            let tl = time_len(s);
            Value::DateTime(NaiveDateTime::new(date_from(rest.get(tl..tl + 3).ok_or_else(short)?)?, time_from(&rest[..tl], s)), DtKind::DateTime2)
        }
        0x3D => {
            let days = i32::from_le_bytes(rest.get(..4).ok_or_else(short)?.try_into().unwrap()) as i64;
            let ticks = u32::from_le_bytes(rest.get(4..8).ok_or_else(short)?.try_into().unwrap()) as i64;
            let ms = (ticks * 10 + 1) / 3;
            Value::DateTime(base_1900().and_hms_opt(0, 0, 0).unwrap() + chrono::Duration::days(days) + chrono::Duration::milliseconds(ms), DtKind::DateTime)
        }
        0x3A => {
            let days = u16::from_le_bytes(rest.get(..2).ok_or_else(short)?.try_into().unwrap()) as i64;
            let mins = u16::from_le_bytes(rest.get(2..4).ok_or_else(short)?.try_into().unwrap()) as i64;
            Value::DateTime(base_1900().and_hms_opt(0, 0, 0).unwrap() + chrono::Duration::days(days) + chrono::Duration::minutes(mins), DtKind::SmallDateTime)
        }
        0x2B => {
            let s = (*p.first().ok_or_else(short)?).min(7);
            let tl = time_len(s);
            let utc = NaiveDateTime::new(date_from(rest.get(tl..tl + 3).ok_or_else(short)?)?, time_from(&rest[..tl], s));
            let off = i16::from_le_bytes(rest.get(tl + 3..tl + 5).ok_or_else(short)?.try_into().unwrap()) as i32 * 60;
            Value::Dto(DateTime::<FixedOffset>::from_naive_utc_and_offset(utc, FixedOffset::east_opt(off).ok_or("bad time zone offset")?))
        }
        0x24 => Value::Guid(Uuid::from_bytes_le(rest.try_into().map_err(|_| short())?)),
        0x3C => {
            let hi = i32::from_le_bytes(rest.get(..4).ok_or_else(short)?.try_into().unwrap()) as i64;
            let lo = u32::from_le_bytes(rest.get(4..8).ok_or_else(short)?.try_into().unwrap()) as i64;
            Value::Dec(Dec::new(((hi << 32) | lo) as i128, 4))
        }
        0x7A => Value::Dec(Dec::new(i32::from_le_bytes(rest.get(..4).ok_or_else(short)?.try_into().unwrap()) as i128, 4)),
        other => return Err(format!("unsupported sql_variant base type 0x{:02X}", other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(w: Wire, v: Value) -> Value {
        let mut out = vec![];
        write_value(&mut out, &w, &v);
        let mut rd = Rd::new(&out);
        let got = read_value(&mut rd, &Ti { w, fixed: false }).unwrap();
        assert_eq!(rd.left(), 0);
        got
    }

    #[test]
    fn values_round_trip() {
        let cases: Vec<(Wire, Value)> = vec![
            (Wire::Int(4), Value::Int(-7)),
            (Wire::Int(8), Value::BigInt(i64::MIN)),
            (Wire::Int(1), Value::Int(255)),
            (Wire::Bit, Value::Bit(true)),
            (Wire::Dec(20, 4), Value::Dec(Dec::new(-123456789012345678, 4))),
            (Wire::Money(8), Value::Dec(Dec::new(-9_223_372_036_854_775_000, 4))),
            (Wire::Money(4), Value::Dec(Dec::new(12345, 4))),
            (Wire::Float(8), Value::Float(1.5e300)),
            (Wire::NVarChar(100), Value::Str("héllo ☃".into())),
            (Wire::NVarChar(PLP), Value::Str("long".repeat(5000))),
            (Wire::VarChar(50), Value::Str("café €".into())),
            (Wire::VarBinary(PLP), Value::Bin(vec![1, 2, 3])),
            (Wire::Date, Value::Date(NaiveDate::from_ymd_opt(2024, 2, 29).unwrap())),
            (Wire::Time(3), Value::Time(NaiveTime::from_hms_milli_opt(13, 14, 15, 678).unwrap())),
            (Wire::DateTime2(7), Value::DateTime(NaiveDate::from_ymd_opt(1999, 12, 31).unwrap().and_hms_nano_opt(23, 59, 58, 123_456_700).unwrap(), DtKind::DateTime2)),
            (Wire::Guid, Value::Guid(Uuid::parse_str("6F9619FF-8B86-D011-B42D-00C04FC964FF").unwrap())),
        ];
        for (w, v) in cases {
            assert_eq!(format!("{:?}", roundtrip(w.clone(), v.clone())), format!("{:?}", v), "{:?}", w);
        }
        assert!(roundtrip(Wire::Int(4), Value::Null).is_null());
        assert!(roundtrip(Wire::NVarChar(PLP), Value::Null).is_null());
        assert!(roundtrip(Wire::VarChar(10), Value::Null).is_null());
        // an empty (not NULL) string stays empty
        assert_eq!(format!("{:?}", roundtrip(Wire::NVarChar(10), Value::Str(String::new()))), "Str(\"\")");
    }

    #[test]
    fn datetime_legacy_is_accurate_to_the_tick() {
        let d = NaiveDate::from_ymd_opt(2024, 5, 6).unwrap().and_hms_milli_opt(7, 8, 9, 123).unwrap();
        match roundtrip(Wire::DateTime(8), Value::DateTime(d, DtKind::DateTime)) {
            Value::DateTime(back, _) => assert_eq!(back, d),
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn code_page() {
        assert_eq!(to_cp1252("a€é☃"), vec![b'a', 0x80, 0xE9, b'?']);
        assert_eq!(from_cp1252(&[0x80, 0xE9]), "€é");
    }

    #[test]
    fn computed_columns_are_widened() {
        assert_eq!(Wire::for_type(&DataType::VarChar(3), 10), Wire::VarChar(10));
        assert_eq!(Wire::for_type(&DataType::NVarChar(5000), 1), Wire::NVarChar(PLP));
        assert_eq!(Wire::for_type(&DataType::VarChar(MAX), 1), Wire::VarChar(PLP));
    }
}
