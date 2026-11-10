//! Conversions (CAST/CONVERT), comparison, ordering and arithmetic on `Value`.
use crate::ast::BinOp;
use crate::decimal::{Dec, POW10};
use crate::error::{Result, SqlError};
use crate::types::*;
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike};
use regex::Regex;
use std::cmp::Ordering;
use std::sync::OnceLock;
use uuid::Uuid;

pub const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
pub const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

// ---------------------------------------------------------------- errors
pub fn conv_fail(v: &Value, target: &str) -> SqlError {
    let (tn, shown) = match v {
        Value::Str(s) => ("varchar", s.clone()),
        Value::Bin(b) => ("varbinary", format!("0x{}", hex(b))),
        other => (other.type_label(), to_str(other)),
    };
    SqlError::new(245, format!("Conversion failed when converting the {} value '{}' to data type {}.", tn, shown, target))
}
fn dt_fail() -> SqlError {
    SqlError::new(241, "Conversion failed when converting date and/or time from character string.")
}
fn not_allowed(v: &Value, target: &str) -> SqlError {
    SqlError::new(529, format!("Explicit conversion from data type {} to {} is not allowed.", v.type_label(), target))
}
pub fn overflow(target: &str) -> SqlError {
    SqlError::new(8115, format!("Arithmetic overflow error converting expression to data type {}.", target))
}
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02X}", x)).collect()
}

// ------------------------------------------------------- number parsing
fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}
fn int_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\s*[+-]?\d+\s*$")
}
fn dec_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\s*[+-]?(\d+\.?\d*|\.\d+)\s*$")
}
fn flt_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\s*[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?\s*$")
}
fn money_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\s*[$£€¥]?\s*[+-]?[\d,]*\.?\d+\s*$")
}

/// Implicit string -> number conversion (used by arithmetic and comparison).
pub fn str_to_number(s: &str, target: &str) -> Result<Value> {
    if int_re().is_match(s) {
        let t = s.trim();
        if let Ok(v) = t.parse::<i64>() {
            return Ok(if v >= i32::MIN as i64 && v <= i32::MAX as i64 { Value::Int(v as i32) } else { Value::BigInt(v) });
        }
        if let Some(d) = Dec::parse(t) {
            return Ok(Value::Dec(d));
        }
    }
    if dec_re().is_match(s) {
        if let Some(d) = Dec::parse(s) {
            return Ok(Value::Dec(d));
        }
    }
    if flt_re().is_match(s) {
        if let Ok(f) = s.trim().parse::<f64>() {
            return Ok(Value::Float(f));
        }
    }
    Err(conv_fail(&Value::Str(s.to_string()), target))
}

// --------------------------------------------------- date/time parsing
#[derive(Debug, Clone, Default)]
pub struct ParsedDt {
    pub date: Option<NaiveDate>,
    pub time: Option<NaiveTime>,
    pub tz: Option<i32>,
}

fn month_idx(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    if n.len() < 3 {
        return None;
    }
    for (i, m) in MONTHS.iter().enumerate() {
        let ml = m.to_ascii_lowercase();
        if ml == n || (ml.starts_with(&n) && n.len() >= 3) {
            return Some(i as u32 + 1);
        }
    }
    None
}
fn year2(y: &str) -> i32 {
    let v: i32 = y.parse().unwrap_or(0);
    if y.len() <= 2 {
        if v <= 49 {
            2000 + v
        } else {
            1900 + v
        }
    } else {
        v
    }
}

pub fn parse_datetime_str(src: &str) -> Option<ParsedDt> {
    static TIME: OnceLock<Regex> = OnceLock::new();
    static DATES: OnceLock<Vec<Regex>> = OnceLock::new();
    let s = src.trim();
    if s.is_empty() {
        return None;
    }
    let time_re = re(&TIME, r"(?i)(?:^|[\sT])(\d{1,2}):(\d{2})(?::(\d{2})(?:[.:](\d{1,7}))?)?\s*(AM|PM)?\s*(Z|[+-]\d{2}:?\d{2})?\s*$");
    let mut out = ParsedDt::default();
    let mut date_part = s.to_string();
    if let Some(c) = time_re.captures(s) {
        let mut hh: u32 = c[1].parse().ok()?;
        if let Some(ap) = c.get(5) {
            if !(1..=12).contains(&hh) {
                return None;
            }
            hh %= 12;
            if ap.as_str().eq_ignore_ascii_case("pm") {
                hh += 12;
            }
        }
        let mi: u32 = c[2].parse().ok()?;
        let ss: u32 = c.get(3).map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
        let nanos: u32 = c.get(4).map(|m| format!("{:0<7}", m.as_str())[..7].parse::<u32>().unwrap_or(0) * 100).unwrap_or(0);
        out.time = Some(NaiveTime::from_hms_nano_opt(hh, mi, ss, nanos)?);
        if let Some(tz) = c.get(6) {
            let t = tz.as_str();
            out.tz = Some(if t == "Z" || t == "z" {
                0
            } else {
                let sign = if t.starts_with('-') { -1 } else { 1 };
                let d = t[1..].replace(':', "");
                sign * (d[..2].parse::<i32>().ok()? * 60 + d[2..].parse::<i32>().ok()?)
            });
        }
        date_part = s[..c.get(0).unwrap().start()].trim().to_string();
        if date_part.is_empty() {
            return Some(out);
        }
    }
    let pats = DATES.get_or_init(|| {
        [
            r"^(\d{4})-(\d{1,2})-(\d{1,2})$",
            r"^(\d{4})(\d{2})(\d{2})$",
            r"^(\d{4})/(\d{1,2})/(\d{1,2})$",
            r"^(\d{1,2})/(\d{1,2})/(\d{2,4})$",
            r"^(\d{1,2})\.(\d{1,2})\.(\d{2,4})$",
            r"^(\d{1,2})-(\d{1,2})-(\d{2,4})$",
            r"^([A-Za-z]+)\.?\s+(\d{1,2})(?:st|nd|rd|th)?,?\s+(\d{4})$",
            r"^(\d{1,2})\s+([A-Za-z]+)\.?,?\s+(\d{4})$",
            r"^([A-Za-z]+)\s+(\d{4})$",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    });
    let dp = date_part.as_str();
    for (i, r) in pats.iter().enumerate() {
        if let Some(c) = r.captures(dp) {
            let d = match i {
                0 | 1 | 2 => NaiveDate::from_ymd_opt(c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?),
                3 => NaiveDate::from_ymd_opt(year2(&c[3]), c[1].parse().ok()?, c[2].parse().ok()?),
                4 | 5 => NaiveDate::from_ymd_opt(year2(&c[3]), c[2].parse().ok()?, c[1].parse().ok()?),
                6 => NaiveDate::from_ymd_opt(c[3].parse().ok()?, month_idx(&c[1])?, c[2].parse().ok()?),
                7 => NaiveDate::from_ymd_opt(c[3].parse().ok()?, month_idx(&c[2])?, c[1].parse().ok()?),
                _ => NaiveDate::from_ymd_opt(c[2].parse().ok()?, month_idx(&c[1])?, 1),
            }?;
            out.date = Some(d);
            return Some(out);
        }
    }
    None
}

pub fn base_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(1900, 1, 1).unwrap()
}

pub fn round_datetime(dt: NaiveDateTime) -> NaiveDateTime {
    let micros = dt.nanosecond() / 1000;
    let base = dt.with_nanosecond(0).unwrap();
    let ticks = (micros as f64 / 1000.0 * 0.3).round();
    let ms = (ticks * 10.0 / 3.0).round() as i64;
    base + TimeDelta::milliseconds(ms)
}
fn round_small(dt: NaiveDateTime) -> NaiveDateTime {
    let secs = dt.second() as f64 + dt.nanosecond() as f64 / 1e9;
    let base = dt.with_second(0).unwrap().with_nanosecond(0).unwrap();
    if secs >= 30.0 {
        base + TimeDelta::minutes(1)
    } else {
        base
    }
}
fn trunc_scale(dt: NaiveDateTime, scale: u8) -> NaiveDateTime {
    if scale >= 7 {
        return dt;
    }
    let q = 10u32.pow(9 - scale as u32);
    let ns = (dt.nanosecond() / q) * q;
    dt.with_nanosecond(ns).unwrap()
}
fn trunc_time_scale(t: NaiveTime, scale: u8) -> NaiveTime {
    if scale >= 7 {
        return t;
    }
    let q = 10u32.pow(9 - scale as u32);
    t.with_nanosecond((t.nanosecond() / q) * q).unwrap()
}

// -------------------------------------------------------------- formatting
pub fn format_g(v: f64, prec: usize) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if v == 0.0 {
        return "0".into();
    }
    let e = format!("{:.*e}", prec - 1, v);
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if exp < -5 || exp >= prec as i32 {
        let mut m = mant.to_string();
        if m.contains('.') {
            m = m.trim_end_matches('0').trim_end_matches('.').to_string();
        }
        format!("{}e{}{:03}", m, if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let decimals = (prec as i32 - 1 - exp).max(0) as usize;
        let mut s = format!("{:.*}", decimals, v);
        if s.contains('.') {
            s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        }
        s
    }
}

fn frac(ns: u32, scale: usize) -> String {
    let full = format!("{:09}", ns);
    full[..scale.min(9)].to_string()
}
pub fn fmt_time(t: &NaiveTime, scale: u8) -> String {
    if scale == 0 {
        format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
    } else {
        format!("{:02}:{:02}:{:02}.{}", t.hour(), t.minute(), t.second(), frac(t.nanosecond(), scale as usize))
    }
}
pub fn fmt_datetime2(d: &NaiveDateTime, scale: u8) -> String {
    format!("{} {}", d.date().format("%Y-%m-%d"), fmt_time(&d.time(), scale))
}
fn mon3(m: u32) -> &'static str {
    &MONTHS[m as usize - 1][..3]
}
pub fn fmt_datetime_legacy(d: &NaiveDateTime) -> String {
    let h = d.hour() % 12;
    let h = if h == 0 { 12 } else { h };
    format!("{} {:2} {} {:2}:{:02}{}", mon3(d.month()), d.day(), d.year(), h, d.minute(), if d.hour() < 12 { "AM" } else { "PM" })
}

pub fn fmt_dto(d: &DateTime<FixedOffset>, scale: u8) -> String {
    let secs = d.offset().local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs() / 60;
    format!("{} {}{:02}:{:02}", fmt_datetime2(&d.naive_local(), scale), sign, a / 60, a % 60)
}

/// Default CAST(x AS varchar) text.
pub fn to_str(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bit(b) => (if *b { "1" } else { "0" }).into(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::Dec(d) => d.to_string(),
        Value::Float(f) => format_g(*f, 6),
        Value::Str(s) => s.clone(),
        Value::Bin(b) => b.iter().map(|&c| c as char).collect(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => fmt_time(t, 7),
        Value::DateTime(d, DtKind::DateTime2) => fmt_datetime2(d, 7),
        Value::DateTime(d, _) => fmt_datetime_legacy(d),
        Value::Dto(d) => fmt_dto(d, 7),
        Value::Guid(g) => g.to_string().to_uppercase(),
    }
}

/// Text as a client (sqlcmd-like) would display the value.
pub fn display(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                format_g(*f, 6)
            } else if *f == f.trunc() && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                let s = format!("{}", f);
                s
            }
        }
        Value::Bin(b) => format!("0x{}", hex(b)),
        Value::DateTime(d, DtKind::DateTime) => format!("{} {}", d.date().format("%Y-%m-%d"), fmt_time(&d.time(), 3)),
        Value::DateTime(d, DtKind::SmallDateTime) => format!("{} {}", d.date().format("%Y-%m-%d"), fmt_time(&d.time(), 0)),
        other => to_str(other),
    }
}

pub fn fmt_datetime_style(v: &Value, style: i32) -> Result<String> {
    let d: NaiveDateTime = match v {
        Value::Date(d) => d.and_hms_opt(0, 0, 0).unwrap(),
        Value::Time(t) => NaiveDateTime::new(base_date(), *t),
        Value::DateTime(d, _) => *d,
        Value::Dto(d) => d.naive_local(),
        _ => return Err(not_allowed(v, "varchar")),
    };
    let mut st = style;
    let mut century = true;
    if st > 0 && st < 100 && ![20, 21, 23, 24, 25].contains(&st) {
        century = false;
        st += 100;
    }
    let mon = mon3(d.month());
    let yyyy = if century { format!("{:04}", d.year()) } else { format!("{:02}", d.year() % 100) };
    let h12 = if d.hour() % 12 == 0 { 12 } else { d.hour() % 12 };
    let ampm = if d.hour() < 12 { "AM" } else { "PM" };
    let ms3 = format!("{:03}", d.nanosecond() / 1_000_000);
    let hms = format!("{:02}:{:02}:{:02}", d.hour(), d.minute(), d.second());
    let r = match st {
        0 | 100 => fmt_datetime_legacy(&d),
        101 => format!("{:02}/{:02}/{}", d.month(), d.day(), yyyy),
        102 => format!("{}.{:02}.{:02}", yyyy, d.month(), d.day()),
        103 => format!("{:02}/{:02}/{}", d.day(), d.month(), yyyy),
        104 => format!("{:02}.{:02}.{}", d.day(), d.month(), yyyy),
        105 => format!("{:02}-{:02}-{}", d.day(), d.month(), yyyy),
        106 => format!("{:02} {} {}", d.day(), mon, yyyy),
        107 => format!("{} {:02}, {}", mon, d.day(), yyyy),
        108 => hms.clone(),
        109 => format!("{} {:2} {} {:2}:{:02}:{:02}:{}{}", mon, d.day(), d.year(), h12, d.minute(), d.second(), ms3, ampm),
        110 => format!("{:02}-{:02}-{}", d.month(), d.day(), yyyy),
        111 => format!("{}/{:02}/{:02}", yyyy, d.month(), d.day()),
        112 => format!("{}{:02}{:02}", yyyy, d.month(), d.day()),
        113 => format!("{:02} {} {} {}:{}", d.day(), mon, d.year(), hms, ms3),
        114 => format!("{}:{}", hms, ms3),
        20 | 120 => format!("{:04}-{:02}-{:02} {}", d.year(), d.month(), d.day(), hms),
        21 | 25 | 121 => format!("{:04}-{:02}-{:02} {}.{}", d.year(), d.month(), d.day(), hms, ms3),
        23 => format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
        24 => hms.clone(),
        126 | 127 => format!("{:04}-{:02}-{:02}T{}.{}", d.year(), d.month(), d.day(), hms, ms3),
        130 => format!("{} {} {} {:2}:{:02}:{:02}:{}{}", d.day(), mon, d.year(), h12, d.minute(), d.second(), ms3, ampm),
        _ => return Err(SqlError::new(281, format!("{} is not a valid style number when converting from datetime to a character string.", style))),
    };
    Ok(r)
}

// ----------------------------------------------------------------- CAST
fn type_range(t: &DataType) -> (i64, i64) {
    match t {
        DataType::TinyInt => (0, 255),
        DataType::SmallInt => (-32768, 32767),
        DataType::Int => (i32::MIN as i64, i32::MAX as i64),
        _ => (i64::MIN, i64::MAX),
    }
}
fn int_value(v: i64, t: &DataType) -> Value {
    if matches!(t, DataType::BigInt) {
        Value::BigInt(v)
    } else {
        Value::Int(v as i32)
    }
}
fn int_overflow(t: &DataType, shown: &str) -> SqlError {
    match t {
        DataType::TinyInt | DataType::SmallInt => SqlError::new(220, format!("Arithmetic overflow error for data type {}, value = {}.", t.base_name(), shown)),
        _ => overflow(t.base_name()),
    }
}

pub fn cast(v: &Value, t: &DataType, style: Option<i32>) -> Result<Value> {
    cast_inner(v, t, style, false)
}

/// Coerce a value for storage in a column (strict about truncation).
pub fn coerce_store(v: &Value, t: &DataType, table: &str, col: &str) -> Result<Value> {
    match cast_inner(v, t, None, true) {
        Err(e) if e.number == 8152 => {
            let shown = match v {
                Value::Bin(b) => format!("0x{}", hex(b)),
                o => to_str(o),
            };
            let n = t.length().filter(|n| *n > 0).unwrap_or(i32::MAX) as usize;
            let shown: String = shown.chars().take(n).collect();
            Err(SqlError::new(2628, format!("String or binary data would be truncated in table '{}', column '{}'. Truncated value: '{}'.", table, col, shown)))
        }
        other => other,
    }
}

fn cast_inner(v: &Value, t: &DataType, style: Option<i32>, strict: bool) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    match t {
        DataType::TinyInt | DataType::SmallInt | DataType::Int | DataType::BigInt => to_int(v, t),
        DataType::Bit => to_bit(v),
        DataType::Decimal(p, s) => to_dec(v, *p, *s, false),
        DataType::Money => to_dec(v, 19, 4, true),
        DataType::SmallMoney => {
            let d = to_dec(v, 10, 4, true)?;
            Ok(d)
        }
        DataType::Float | DataType::Real => to_float(v, matches!(t, DataType::Real)),
        DataType::Char(_) | DataType::VarChar(_) | DataType::NChar(_) | DataType::NVarChar(_) | DataType::Text | DataType::NText => to_string_type(v, t, style, strict),
        DataType::Binary(_) | DataType::VarBinary(_) | DataType::Image | DataType::Timestamp => to_binary(v, t, style, strict),
        DataType::Date | DataType::Time(_) | DataType::DateTime | DataType::SmallDateTime | DataType::DateTime2(_) | DataType::DateTimeOffset(_) => to_date(v, t),
        DataType::Guid => match v {
            Value::Guid(_) => Ok(v.clone()),
            Value::Str(s) => Uuid::parse_str(s.trim().trim_matches(|c| c == '{' || c == '}'))
                .map(Value::Guid)
                .map_err(|_| SqlError::new(8169, "Conversion failed when converting from a character string to uniqueidentifier.")),
            Value::Bin(b) if b.len() == 16 => {
                let mut a = [0u8; 16];
                a.copy_from_slice(b);
                Ok(Value::Guid(Uuid::from_bytes_le(a)))
            }
            _ => Err(not_allowed(v, "uniqueidentifier")),
        },
        DataType::Xml => Ok(Value::Str(to_str(v))),
        DataType::Variant | DataType::Other(_) => Ok(v.clone()),
    }
}

fn to_int(v: &Value, t: &DataType) -> Result<Value> {
    let (lo, hi) = type_range(t);
    let r: i64 = match v {
        Value::Bit(b) => *b as i64,
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        Value::Dec(d) => {
            let q = d.to_i128_trunc();
            if q < i64::MIN as i128 || q > i64::MAX as i128 {
                return Err(SqlError::new(8115, format!("Arithmetic overflow error converting numeric to data type {}.", t.base_name())));
            }
            q as i64
        }
        Value::Float(f) => {
            if !f.is_finite() || f.abs() >= 9.3e18 {
                return Err(SqlError::new(8115, format!("Arithmetic overflow error converting float to data type {}.", t.base_name())));
            }
            f.trunc() as i64
        }
        Value::Str(s) => {
            if int_re().is_match(s) {
                s.trim().parse::<i64>().map_err(|_| conv_fail(v, t.base_name()))?
            } else if money_re().is_match(s) && s.trim_start().starts_with(|c| "$£€¥".contains(c)) {
                Dec::parse(&s.trim().trim_start_matches(|c| "$£€¥".contains(c)).replace(',', ""))
                    .map(|d| d.to_i128_trunc() as i64)
                    .ok_or_else(|| conv_fail(v, t.base_name()))?
            } else {
                return Err(conv_fail(v, t.base_name()));
            }
        }
        Value::Bin(b) => {
            // the bytes, big-endian, padded with zeros on the left (or cut on the left) to the width of the target
            // type, read as a two's complement number of that width: 0xFF is 255 as an INT and -1 as a SMALLINT's 0x00FF
            // is 255, while 0xFFFFFFFF is -1
            let width = match t {
                DataType::TinyInt => 1,
                DataType::SmallInt => 2,
                DataType::Int => 4,
                _ => 8,
            };
            let mut x: i64 = 0;
            let tail: Vec<u8> = b.iter().rev().take(width).rev().copied().collect();
            for c in tail.iter() {
                x = (x << 8) | *c as i64;
            }
            if width < 8 && !matches!(t, DataType::TinyInt) && width > 0 && tail.len() == width && tail[0] & 0x80 != 0 {
                x -= 1i64 << (8 * width);
            }
            x
        }
        Value::DateTime(d, _) => {
            let days = (d.date() - base_date()).num_days();
            if d.hour() >= 12 { days + 1 } else { days }
        }
        _ => return Err(not_allowed(v, t.base_name())),
    };
    if r < lo || r > hi {
        if matches!(v, Value::Dec(_) | Value::Float(_) | Value::Str(_)) {
            let src = if matches!(v, Value::Float(_)) { "float" } else { "numeric" };
            if matches!(v, Value::Str(_)) {
                return Err(conv_fail(v, t.base_name()));
            }
            return Err(SqlError::new(8115, format!("Arithmetic overflow error converting {} to data type {}.", src, t.base_name())));
        }
        return Err(int_overflow(t, &r.to_string()));
    }
    Ok(int_value(r, t))
}

fn to_bit(v: &Value) -> Result<Value> {
    Ok(Value::Bit(match v {
        Value::Bit(b) => *b,
        Value::Int(i) => *i != 0,
        Value::BigInt(i) => *i != 0,
        Value::Dec(d) => !d.is_zero(),
        Value::Float(f) => *f != 0.0,
        Value::Str(s) => {
            let l = s.trim().to_ascii_lowercase();
            if l == "true" {
                true
            } else if l == "false" {
                false
            } else if flt_re().is_match(&l) {
                l.parse::<f64>().unwrap_or(0.0) != 0.0
            } else {
                return Err(conv_fail(v, "bit"));
            }
        }
        Value::Bin(b) => b.iter().any(|c| *c != 0),
        _ => return Err(not_allowed(v, "bit")),
    }))
}

fn to_dec(v: &Value, p: u8, s: u8, money: bool) -> Result<Value> {
    let d: Dec = match v {
        Value::Bit(b) => Dec::from_int(*b as i64),
        Value::Int(i) => Dec::from_int(*i as i64),
        Value::BigInt(i) => Dec::from_int(*i),
        Value::Dec(d) => *d,
        Value::Float(f) => Dec::from_f64(*f).ok_or_else(|| SqlError::new(8114, "Error converting data type float to numeric."))?,
        Value::Str(st) => {
            let mut t = st.trim().to_string();
            if money && money_re().is_match(&t) {
                t = t.trim_start_matches(|c| "$£€¥".contains(c)).replace(',', "");
            }
            if !dec_re().is_match(&t) {
                if flt_re().is_match(&t) && money {
                    Dec::from_f64(t.parse().unwrap_or(0.0)).ok_or_else(|| conv_fail(v, "money"))?
                } else if money {
                    return Err(conv_fail(v, "money"));
                } else {
                    return Err(SqlError::new(8114, "Error converting data type varchar to numeric."));
                }
            } else {
                Dec::parse(&t).ok_or_else(|| SqlError::new(8115, "Arithmetic overflow error converting varchar to data type numeric."))?
            }
        }
        Value::Bin(b) => {
            let mut x: i64 = 0;
            for c in b.iter().take(8) {
                x = (x << 8) | *c as i64;
            }
            Dec::from_int(x)
        }
        Value::DateTime(d, _) => Dec::from_int((d.date() - base_date()).num_days()),
        _ => return Err(not_allowed(v, "numeric")),
    };
    d.fit(p, s).map(Value::Dec).ok_or_else(|| {
        SqlError::new(8115, format!("Arithmetic overflow error converting {} to data type numeric.", if matches!(v, Value::Dec(_)) { "numeric" } else { "expression" }))
    })
}

fn to_float(v: &Value, real: bool) -> Result<Value> {
    let f = match v {
        Value::Bit(b) => *b as i32 as f64,
        Value::Int(i) => *i as f64,
        Value::BigInt(i) => *i as f64,
        Value::Dec(d) => d.to_f64(),
        Value::Float(f) => *f,
        Value::Str(s) => {
            if !flt_re().is_match(s) {
                let l = s.trim().to_ascii_lowercase();
                if l == "nan" || l == "inf" {
                    return Err(conv_fail(v, if real { "real" } else { "float" }));
                }
                return Err(conv_fail(v, if real { "real" } else { "float" }));
            }
            s.trim().parse::<f64>().map_err(|_| conv_fail(v, "float"))?
        }
        Value::Bin(b) => {
            let mut x: i64 = 0;
            for c in b.iter().take(8) {
                x = (x << 8) | *c as i64;
            }
            x as f64
        }
        Value::DateTime(d, _) => {
            (d.date() - base_date()).num_days() as f64 + d.time().num_seconds_from_midnight() as f64 / 86400.0
        }
        _ => return Err(not_allowed(v, "float")),
    };
    Ok(Value::Float(if real { f as f32 as f64 } else { f }))
}

fn to_string_type(v: &Value, t: &DataType, style: Option<i32>, strict: bool) -> Result<Value> {
    let s = match v {
        Value::Bin(b) => match style {
            Some(1) => format!("0x{}", hex(b)),
            Some(2) => hex(b),
            _ => b.iter().map(|&c| c as char).collect(),
        },
        Value::Date(_) | Value::Time(_) | Value::DateTime(..) | Value::Dto(_) if style.map(|s| s != 0).unwrap_or(false) => fmt_datetime_style(v, style.unwrap())?,
        Value::Float(f) if matches!(style, Some(1)) => format!("{:.7e}", f).replace("e", "e+0").replace("e+0-", "e-0"),
        Value::Float(f) if matches!(style, Some(2)) => format_g(*f, 17),
        Value::Float(f) if matches!(style, Some(3)) => format_g(*f, 17),
        Value::Dec(d) if matches!(style, Some(1)) => {
            let s = d.rescale(2).map(|x| x.to_string()).unwrap_or_default();
            add_commas(&s)
        }
        Value::Dec(d) if matches!(style, Some(2)) => d.rescale(4).map(|x| x.to_string()).unwrap_or_default(),
        other => to_str(other),
    };
    let len = t.length();
    let mut s = s;
    if let Some(n) = len {
        if n != MAX {
            let n = n as usize;
            let cc = s.chars().count();
            if cc > n {
                if strict {
                    return Err(SqlError::new(8152, "String or binary data would be truncated."));
                }
                s = s.chars().take(n).collect();
            } else if matches!(t, DataType::Char(_) | DataType::NChar(_)) {
                s.push_str(&" ".repeat(n - cc));
            }
        }
    }
    Ok(Value::Str(s))
}

fn add_commas(s: &str) -> String {
    let (neg, body) = if let Some(r) = s.strip_prefix('-') { (true, r) } else { (false, s) };
    let (ip, fp) = match body.find('.') {
        Some(i) => (&body[..i], &body[i..]),
        None => (body, ""),
    };
    let mut out = String::new();
    for (i, c) in ip.chars().enumerate() {
        if i > 0 && (ip.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}{}{}", if neg { "-" } else { "" }, out, fp)
}

fn to_binary(v: &Value, t: &DataType, style: Option<i32>, strict: bool) -> Result<Value> {
    let mut b: Vec<u8> = match v {
        Value::Bin(b) => b.clone(),
        Value::Str(s) => {
            if matches!(style, Some(1) | Some(2)) {
                let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                let h = if h.len() % 2 == 1 { format!("0{}", h) } else { h.to_string() };
                let mut out = Vec::new();
                for i in (0..h.len()).step_by(2) {
                    out.push(u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| SqlError::new(8114, "Error converting data type varchar to varbinary."))?);
                }
                out
            } else {
                s.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect()
            }
        }
        Value::Bit(x) => vec![*x as u8],
        Value::Int(i) => i.to_be_bytes().to_vec(),
        Value::BigInt(i) => i.to_be_bytes().to_vec(),
        Value::Float(f) => f.to_be_bytes().to_vec(),
        Value::Dec(d) => (d.to_i128_trunc() as i64).to_be_bytes().to_vec(),
        Value::Guid(g) => g.to_bytes_le().to_vec(),
        other => to_str(other).bytes().collect(),
    };
    // integers are right-aligned: a shorter target keeps their low bytes (CAST(255 AS BINARY(2)) is 0x00FF) and a
    // longer fixed-length one is padded with zeros on the left; strings and binary values are left-aligned
    let right = matches!(v, Value::Int(_) | Value::BigInt(_) | Value::Dec(_));
    if let Some(n) = t.length() {
        if n != MAX {
            let n = n as usize;
            if b.len() > n {
                if strict {
                    return Err(SqlError::new(8152, "String or binary data would be truncated."));
                }
                if right {
                    b.drain(..b.len() - n);
                } else {
                    b.truncate(n);
                }
            } else if matches!(t, DataType::Binary(_)) {
                if right {
                    let mut padded = vec![0u8; n - b.len()];
                    padded.extend_from_slice(&b);
                    b = padded;
                } else {
                    b.resize(n, 0);
                }
            }
        }
    }
    Ok(Value::Bin(b))
}

fn finish_dt(d: NaiveDateTime, t: &DataType) -> Result<Value> {
    Ok(match t {
        DataType::DateTime => {
            if d.year() < 1753 {
                return Err(SqlError::new(242, "The conversion of a varchar data type to a datetime data type resulted in an out-of-range value."));
            }
            Value::DateTime(round_datetime(d), DtKind::DateTime)
        }
        DataType::SmallDateTime => Value::DateTime(round_small(d), DtKind::SmallDateTime),
        DataType::DateTime2(s) => Value::DateTime(trunc_scale(d, *s), DtKind::DateTime2),
        _ => unreachable!(),
    })
}

fn to_date(v: &Value, t: &DataType) -> Result<Value> {
    match v {
        Value::Str(s) => {
            let p = parse_datetime_str(s).ok_or_else(dt_fail)?;
            match t {
                DataType::Time(sc) => Ok(Value::Time(trunc_time_scale(p.time.ok_or_else(dt_fail)?, *sc))),
                DataType::Date => Ok(Value::Date(p.date.unwrap_or_else(base_date))),
                DataType::DateTimeOffset(_) => {
                    let nd = NaiveDateTime::new(p.date.unwrap_or_else(base_date), p.time.unwrap_or_default());
                    let off = FixedOffset::east_opt(p.tz.unwrap_or(0) * 60).ok_or_else(dt_fail)?;
                    Ok(Value::Dto(nd.and_local_timezone(off).unwrap()))
                }
                _ => finish_dt(NaiveDateTime::new(p.date.unwrap_or_else(base_date), p.time.unwrap_or_default()), t),
            }
        }
        Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) | Value::Bit(_) => {
            let days = match v {
                Value::Int(i) => *i as f64,
                Value::BigInt(i) => *i as f64,
                Value::Dec(d) => d.to_f64(),
                Value::Float(f) => *f,
                Value::Bit(b) => *b as i32 as f64,
                _ => 0.0,
            };
            match t {
                DataType::DateTime | DataType::SmallDateTime | DataType::DateTime2(_) | DataType::Date => {
                    let base = NaiveDateTime::new(base_date(), NaiveTime::MIN) + TimeDelta::microseconds((days * 86_400_000_000.0).round() as i64);
                    if matches!(t, DataType::Date) {
                        return Err(not_allowed(v, "date"));
                    }
                    finish_dt(base, t)
                }
                _ => Err(not_allowed(v, t.base_name())),
            }
        }
        Value::DateTime(d, _) => match t {
            DataType::Date => Ok(Value::Date(d.date())),
            DataType::Time(sc) => Ok(Value::Time(trunc_time_scale(d.time(), *sc))),
            DataType::DateTimeOffset(_) => Ok(Value::Dto(d.and_local_timezone(FixedOffset::east_opt(0).unwrap()).unwrap())),
            _ => finish_dt(*d, t),
        },
        Value::Dto(d) => match t {
            DataType::Date => Ok(Value::Date(d.date_naive())),
            DataType::Time(sc) => Ok(Value::Time(trunc_time_scale(d.time(), *sc))),
            DataType::DateTimeOffset(_) => Ok(v.clone()),
            _ => finish_dt(d.naive_local(), t),
        },
        Value::Date(d) => match t {
            DataType::Date => Ok(v.clone()),
            DataType::Time(_) => Err(not_allowed(v, "time")),
            DataType::DateTimeOffset(_) => Ok(Value::Dto(d.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(FixedOffset::east_opt(0).unwrap()).unwrap())),
            _ => finish_dt(d.and_hms_opt(0, 0, 0).unwrap(), t),
        },
        Value::Time(tm) => match t {
            DataType::Time(sc) => Ok(Value::Time(trunc_time_scale(*tm, *sc))),
            DataType::Date => Err(not_allowed(v, "date")),
            DataType::DateTimeOffset(_) => Ok(Value::Dto(NaiveDateTime::new(base_date(), *tm).and_local_timezone(FixedOffset::east_opt(0).unwrap()).unwrap())),
            _ => finish_dt(NaiveDateTime::new(base_date(), *tm), t),
        },
        Value::Bin(b) => {
            let mut x: i64 = 0;
            for c in b.iter().take(8) {
                x = (x << 8) | *c as i64;
            }
            to_date(&Value::BigInt(x), t)
        }
        _ => Err(not_allowed(v, t.base_name())),
    }
}

// ------------------------------------------------------ numeric helpers
#[derive(Clone, Copy, Debug)]
pub enum Num {
    I(i64),
    D(Dec),
    F(f64),
}

/// Numeric view of a value (strings are NOT converted here).
pub fn as_num(v: &Value) -> Option<Num> {
    Some(match v {
        Value::Bit(b) => Num::I(*b as i64),
        Value::Int(i) => Num::I(*i as i64),
        Value::BigInt(i) => Num::I(*i),
        Value::Dec(d) => Num::D(*d),
        Value::Float(f) => Num::F(*f),
        _ => return None,
    })
}
pub fn num_to_f64(n: Num) -> f64 {
    match n {
        Num::I(i) => i as f64,
        Num::D(d) => d.to_f64(),
        Num::F(f) => f,
    }
}
pub fn num_to_dec(n: Num) -> Option<Dec> {
    match n {
        Num::I(i) => Some(Dec::from_int(i)),
        Num::D(d) => Some(d),
        Num::F(f) => Dec::from_f64(f),
    }
}

// -------------------------------------------------------------- comparing
fn norm_str(s: &str) -> String {
    s.trim_end_matches(' ').to_lowercase()
}

fn to_ndt(v: &Value) -> Result<NaiveDateTime> {
    Ok(match v {
        Value::Date(d) => d.and_hms_opt(0, 0, 0).unwrap(),
        Value::DateTime(d, _) => *d,
        Value::Dto(d) => d.naive_utc(),
        Value::Time(t) => NaiveDateTime::new(base_date(), *t),
        Value::Str(s) => {
            let p = parse_datetime_str(s).ok_or_else(dt_fail)?;
            NaiveDateTime::new(p.date.unwrap_or_else(base_date), p.time.unwrap_or_default())
        }
        Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) => {
            let f = num_to_f64(as_num(v).unwrap());
            NaiveDateTime::new(base_date(), NaiveTime::MIN) + TimeDelta::microseconds((f * 86_400_000_000.0) as i64)
        }
        _ => return Err(SqlError::new(402, "The data types are incompatible in the equal to operator.")),
    })
}

pub fn cmp_num(a: Num, b: Num) -> Option<Ordering> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => Some(x.cmp(&y)),
        (Num::F(_), _) | (_, Num::F(_)) => num_to_f64(a).partial_cmp(&num_to_f64(b)),
        _ => {
            let x = num_to_dec(a)?;
            let y = num_to_dec(b)?;
            Some(x.cmp(&y))
        }
    }
}

/// Three-valued comparison: Ok(None) when either side is NULL.
pub fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>> {
    if a.is_null() || b.is_null() {
        return Ok(None);
    }
    use Value::*;
    match (a, b) {
        (Str(x), Str(y)) => return Ok(Some(norm_str(x).cmp(&norm_str(y)))),
        (Bin(x), Bin(y)) => return Ok(Some(x.cmp(y))),
        (Guid(x), Guid(y)) => return Ok(Some(guid_sql_cmp(x, y))),
        _ => {}
    }
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return Ok(cmp_num(x, y));
    }
    let is_dt = |v: &Value| matches!(v, Date(_) | DateTime(..) | Dto(_));
    if is_dt(a) || is_dt(b) {
        if let (Time(_), Time(_)) = (a, b) {
        } else {
            return Ok(Some(to_ndt(a)?.cmp(&to_ndt(b)?)));
        }
    }
    match (a, b) {
        (Time(x), Time(y)) => Ok(Some(x.cmp(y))),
        (Time(x), Str(s)) | (Str(s), Time(x)) => {
            let p = parse_datetime_str(s).and_then(|p| p.time).ok_or_else(dt_fail)?;
            let o = x.cmp(&p);
            Ok(Some(if matches!(a, Time(_)) { o } else { o.reverse() }))
        }
        (Str(s), n) | (n, Str(s)) if as_num(n).is_some() => {
            let sn = as_num(&str_to_number(s, n.type_label())?).unwrap();
            let nn = as_num(n).unwrap();
            let o = cmp_num(sn, nn);
            Ok(if matches!(a, Str(_)) { o } else { o.map(|x| x.reverse()) })
        }
        (Guid(g), Str(s)) | (Str(s), Guid(g)) => {
            let other = Uuid::parse_str(s.trim()).map_err(|_| SqlError::new(8169, "Conversion failed when converting from a character string to uniqueidentifier."))?;
            let o = guid_sql_cmp(g, &other);
            Ok(Some(if matches!(a, Guid(_)) { o } else { o.reverse() }))
        }
        (Bin(x), Str(s)) => Ok(Some(x.as_slice().cmp(s.as_bytes()))),
        (Str(s), Bin(y)) => Ok(Some(s.as_bytes().cmp(y.as_slice()))),
        (Bin(x), n) if as_num(n).is_some() => Ok(Some(to_int_from_bin(x).cmp(&(num_to_f64(as_num(n).unwrap()) as i64)))),
        _ => Err(SqlError::new(402, format!("The data types {} and {} are incompatible in the equal to operator.", a.type_label(), b.type_label()))),
    }
}
fn to_int_from_bin(b: &[u8]) -> i64 {
    let mut x = 0i64;
    for c in b.iter().take(8) {
        x = (x << 8) | *c as i64;
    }
    x
}

/// SQL Server orders uniqueidentifiers by the last 6 bytes first.
fn guid_sql_cmp(a: &Uuid, b: &Uuid) -> Ordering {
    let x = a.to_bytes_le();
    let y = b.to_bytes_le();
    let order = [10, 11, 12, 13, 14, 15, 8, 9, 6, 7, 4, 5, 0, 1, 2, 3];
    for i in order {
        let o = x[i].cmp(&y[i]);
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bit(_) | Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) => 1,
        Value::Str(_) => 2,
        Value::Bin(_) => 3,
        Value::Date(_) | Value::DateTime(..) | Value::Dto(_) => 4,
        Value::Time(_) => 5,
        Value::Guid(_) => 6,
    }
}

/// Total ordering for ORDER BY / DISTINCT sorting. NULL sorts first.
pub fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    if rank(a) != rank(b) {
        return rank(a).cmp(&rank(b));
    }
    match compare(a, b) {
        Ok(Some(o)) => o,
        _ => Ordering::Equal,
    }
}

/// Append a canonical, collation-aware key for grouping/hashing to `out`.
pub fn key_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("\u{1}N"),
        Value::Bit(b) => out.push_str(&format!("n{}", *b as i32)),
        Value::Int(i) => out.push_str(&format!("n{}", i)),
        Value::BigInt(i) => out.push_str(&format!("n{}", i)),
        Value::Dec(d) => out.push_str(&format!("n{}", d.normalized())),
        Value::Float(f) => {
            if f.is_finite() && *f == f.trunc() && f.abs() < 9e18 {
                out.push_str(&format!("n{}", *f as i64));
            } else if let Some(d) = Dec::from_f64(*f) {
                out.push_str(&format!("n{}", d.normalized()));
            } else {
                out.push_str(&format!("f{}", f));
            }
        }
        Value::Str(s) => {
            out.push('s');
            out.push_str(&norm_str(s));
        }
        Value::Bin(b) => out.push_str(&format!("b{}", hex(b))),
        Value::Date(d) => out.push_str(&format!("d{}", d.and_hms_opt(0, 0, 0).unwrap())),
        Value::DateTime(d, _) => out.push_str(&format!("d{}", d)),
        Value::Dto(d) => out.push_str(&format!("d{}", d.naive_utc())),
        Value::Time(t) => out.push_str(&format!("t{}", t)),
        Value::Guid(g) => out.push_str(&format!("g{}", g)),
    }
    out.push('\u{0}');
}
pub fn key_of(vals: &[Value]) -> String {
    let mut s = String::new();
    for v in vals {
        key_into(v, &mut s);
    }
    s
}

// ------------------------------------------------------------- arithmetic
fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "subtract",
        BinOp::Mul => "multiply",
        BinOp::Div => "divide",
        BinOp::Mod => "modulo",
        BinOp::BitAnd => "bitwise AND",
        BinOp::BitOr => "bitwise OR",
        BinOp::BitXor => "bitwise exclusive OR",
        _ => "operator",
    }
}
fn div0() -> SqlError {
    SqlError::new(8134, "Divide by zero error encountered.")
}
fn dec_overflow() -> SqlError {
    SqlError::new(8115, "Arithmetic overflow error converting numeric to data type numeric.")
}

fn prec_of(v: &Value) -> u32 {
    match v {
        Value::Bit(_) => 1,
        Value::Int(_) => 10,
        Value::BigInt(_) => 19,
        Value::Dec(d) => d.precision(),
        _ => 38,
    }
}

pub fn arith(op: BinOp, a: &Value, b: &Value) -> Result<Value> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    if op == BinOp::Add {
        match (a, b) {
            (Value::Str(x), Value::Str(y)) => return Ok(Value::Str(format!("{}{}", x, y))),
            (Value::Bin(x), Value::Bin(y)) => {
                let mut z = x.clone();
                z.extend_from_slice(y);
                return Ok(Value::Bin(z));
            }
            _ => {}
        }
    }
    // date arithmetic
    let is_d = |v: &Value| matches!(v, Value::Date(_) | Value::DateTime(..) | Value::Dto(_) | Value::Time(_));
    if is_d(a) || is_d(b) {
        return date_arith(op, a, b);
    }
    if matches!(a, Value::Guid(_)) || matches!(b, Value::Guid(_)) {
        return Err(SqlError::new(8117, format!("Operand data type uniqueidentifier is invalid for {} operator.", op_name(op))));
    }
    // string -> number
    let a2;
    let b2;
    let a = if let Value::Str(s) = a {
        a2 = str_to_number(s, "int")?;
        &a2
    } else {
        a
    };
    let b = if let Value::Str(s) = b {
        b2 = str_to_number(s, "int")?;
        &b2
    } else {
        b
    };
    let a3;
    let b3;
    let a = if let Value::Bin(x) = a {
        a3 = Value::BigInt(to_int_from_bin(x));
        &a3
    } else {
        a
    };
    let b = if let Value::Bin(x) = b {
        b3 = Value::BigInt(to_int_from_bin(x));
        &b3
    } else {
        b
    };
    let (x, y) = match (as_num(a), as_num(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(SqlError::new(402, format!("The data types {} and {} are incompatible in the {} operator.", a.type_label(), b.type_label(), op_name(op)))),
    };
    if matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor) {
        return match (x, y) {
            (Num::I(p), Num::I(q)) => {
                let r = match op {
                    BinOp::BitAnd => p & q,
                    BinOp::BitOr => p | q,
                    _ => p ^ q,
                };
                if matches!(a, Value::BigInt(_)) || matches!(b, Value::BigInt(_)) {
                    Ok(Value::BigInt(r))
                } else if matches!(a, Value::Bit(_)) && matches!(b, Value::Bit(_)) {
                    Ok(Value::Bit(r != 0))
                } else {
                    Ok(Value::Int(r as i32))
                }
            }
            _ => Err(SqlError::new(402, format!("The data types {} and {} are incompatible in the {} operator.", a.type_label(), b.type_label(), op_name(op)))),
        };
    }
    match (x, y) {
        (Num::F(_), _) | (_, Num::F(_)) => {
            let p = num_to_f64(x);
            let q = num_to_f64(y);
            Ok(Value::Float(match op {
                BinOp::Add => p + q,
                BinOp::Sub => p - q,
                BinOp::Mul => p * q,
                BinOp::Div => {
                    if q == 0.0 {
                        return Err(div0());
                    }
                    p / q
                }
                _ => return Err(SqlError::new(402, "The data types float and float are incompatible in the modulo operator.")),
            }))
        }
        (Num::D(_), _) | (_, Num::D(_)) => {
            let p = num_to_dec(x).unwrap();
            let q = num_to_dec(y).unwrap();
            let r = match op {
                BinOp::Add => p.add(&q),
                BinOp::Sub => p.sub(&q),
                BinOp::Mul => p.mul(&q),
                BinOp::Div => {
                    if q.is_zero() {
                        return Err(div0());
                    }
                    p.div(&q, prec_of(a), prec_of(b))
                }
                BinOp::Mod => {
                    if q.is_zero() {
                        return Err(div0());
                    }
                    p.rem(&q)
                }
                _ => None,
            };
            r.map(Value::Dec).ok_or_else(dec_overflow)
        }
        (Num::I(p), Num::I(q)) => {
            let big = matches!(a, Value::BigInt(_)) || matches!(b, Value::BigInt(_));
            let r = match op {
                BinOp::Add => p.checked_add(q),
                BinOp::Sub => p.checked_sub(q),
                BinOp::Mul => p.checked_mul(q),
                BinOp::Div => {
                    if q == 0 {
                        return Err(div0());
                    }
                    p.checked_div(q)
                }
                BinOp::Mod => {
                    if q == 0 {
                        return Err(div0());
                    }
                    p.checked_rem(q)
                }
                _ => None,
            };
            let r = r.ok_or_else(|| overflow("bigint"))?;
            if big {
                Ok(Value::BigInt(r))
            } else if r < i32::MIN as i64 || r > i32::MAX as i64 {
                Err(overflow("int"))
            } else {
                Ok(Value::Int(r as i32))
            }
        }
    }
}

fn date_arith(op: BinOp, a: &Value, b: &Value) -> Result<Value> {
    let days_of = |v: &Value| -> Result<f64> {
        match v {
            Value::Str(s) => Ok(num_to_f64(as_num(&str_to_number(s, "int")?).unwrap())),
            o => as_num(o).map(num_to_f64).ok_or_else(|| SqlError::new(206, format!("Operand type clash: {} is incompatible with {}", a.type_label(), b.type_label()))),
        }
    };
    let clash = || SqlError::new(206, format!("Operand type clash: {} is incompatible with {}", a.type_label(), b.type_label()));
    match (a, b, op) {
        (Value::DateTime(d, k), other, BinOp::Add | BinOp::Sub) if !matches!(k, DtKind::DateTime2) && !matches!(other, Value::Date(_) | Value::DateTime(..) | Value::Dto(_) | Value::Time(_)) => {
            let mut n = days_of(other)?;
            if op == BinOp::Sub {
                n = -n;
            }
            let nd = *d + TimeDelta::microseconds((n * 86_400_000_000.0).round() as i64);
            Ok(Value::DateTime(if *k == DtKind::DateTime { round_datetime(nd) } else { nd }, *k))
        }
        (other, Value::DateTime(d, k), BinOp::Add) if !matches!(k, DtKind::DateTime2) && !matches!(other, Value::Date(_) | Value::DateTime(..) | Value::Dto(_) | Value::Time(_)) => {
            let n = days_of(other)?;
            let nd = *d + TimeDelta::microseconds((n * 86_400_000_000.0).round() as i64);
            Ok(Value::DateTime(if *k == DtKind::DateTime { round_datetime(nd) } else { nd }, *k))
        }
        (Value::DateTime(d1, k1), Value::DateTime(d2, _), BinOp::Sub) if !matches!(k1, DtKind::DateTime2) => {
            let diff = *d1 - *d2;
            let _ = diff;
            Err(SqlError::new(8117, "Operand data type datetime is invalid for subtract operator."))
        }
        _ => Err(clash()),
    }
}

pub fn negate(v: &Value) -> Result<Value> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Bit(b) => Value::Int(-(*b as i32)),
        Value::Int(i) => Value::Int(i.checked_neg().ok_or_else(|| overflow("int"))?),
        Value::BigInt(i) => Value::BigInt(i.checked_neg().ok_or_else(|| overflow("bigint"))?),
        Value::Dec(d) => Value::Dec(d.neg()),
        Value::Float(f) => Value::Float(-f),
        Value::Str(s) => return negate(&str_to_number(s, "int")?),
        _ => return Err(SqlError::new(8117, format!("Operand data type {} is invalid for minus operator.", v.type_label()))),
    })
}

pub fn pow10_dec(scale: u8) -> i128 {
    POW10[scale as usize]
}

pub fn naive_to_value(d: NaiveDateTime, kind: DtKind) -> Value {
    Value::DateTime(d, kind)
}

pub fn date_of(v: &Value) -> Option<NaiveDate> {
    match v {
        Value::Date(d) => Some(*d),
        Value::DateTime(d, _) => Some(d.date()),
        Value::Dto(d) => Some(d.date_naive()),
        _ => None,
    }
}
pub fn ndt_of(v: &Value) -> Result<NaiveDateTime> {
    to_ndt(v)
}
