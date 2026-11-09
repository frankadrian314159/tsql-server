//! Built-in scalar and table-valued functions.
use super::query::{ColMeta, Env, Rowset, Schema};
use super::*;
use crate::convert::{self, arith, cast, compare, to_str};
use crate::decimal::Dec;
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike};
use std::cmp::Ordering;

pub const VERSION_STRING: &str = "Microsoft SQL Server 2022 (RTM) - 16.0.1000.6 (X64) \n\tOct  8 2022 \n\tCopyright (C) 2022 Microsoft Corporation\n\tDeveloper Edition (64-bit) on Windows (tsql-server clone)";

// ---------------------------------------------------------------- helpers
fn s(v: &Value) -> String {
    to_str(v)
}
fn int(v: &Value) -> Result<i64> {
    match v {
        Value::Str(_) | Value::Bit(_) | Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) => Ok(cast(v, &DataType::BigInt, None)?.as_i64().unwrap_or(0)),
        _ => Err(SqlError::new(8116, format!("Argument data type {} is invalid for argument of function.", v.type_label()))),
    }
}
fn num(v: &Value) -> Result<f64> {
    match convert::as_num(v) {
        Some(n) => Ok(convert::num_to_f64(n)),
        None => match v {
            Value::Str(st) => Ok(convert::num_to_f64(convert::as_num(&convert::str_to_number(st, "float")?).unwrap())),
            _ => Err(SqlError::new(8116, format!("Argument data type {} is invalid for argument 1 of function.", v.type_label()))),
        },
    }
}
fn any_null(a: &[Value]) -> bool {
    a.iter().any(|v| v.is_null())
}
fn need(name: &str, a: &[Value], n: usize) -> Result<()> {
    if a.len() != n {
        return Err(SqlError::new(174, format!("The {} function requires {} argument(s).", name.to_lowercase(), n)));
    }
    Ok(())
}
fn need_range(name: &str, a: &[Value], lo: usize, hi: usize) -> Result<()> {
    if a.len() < lo || a.len() > hi {
        return Err(SqlError::new(174, format!("The {} function requires {} to {} argument(s).", name.to_lowercase(), lo, hi)));
    }
    Ok(())
}
pub fn checksum_value(v: &Value) -> i32 {
    let st = convert::key_of(std::slice::from_ref(v));
    let mut h: u32 = 2166136261;
    for b in st.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h as i32
}
fn bigf(f: f64) -> Value {
    Value::Float(f)
}
fn fmt_money_like(f: f64, d: usize) -> String {
    format!("{:.*}", d, f)
}

pub fn format_raiserror(fmt: &str, args: &[Value]) -> String {
    let ch: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut ai = 0;
    while i < ch.len() {
        if ch[i] == '%' && i + 1 < ch.len() {
            let mut j = i + 1;
            if ch[j] == '%' {
                out.push('%');
                i += 2;
                continue;
            }
            let mut flags = String::new();
            while j < ch.len() && "-+ #0".contains(ch[j]) {
                flags.push(ch[j]);
                j += 1;
            }
            let mut width = String::new();
            while j < ch.len() && ch[j].is_ascii_digit() {
                width.push(ch[j]);
                j += 1;
            }
            if j < ch.len() && ch[j] == '.' {
                j += 1;
                while j < ch.len() && ch[j].is_ascii_digit() {
                    j += 1;
                }
            }
            while j < ch.len() && (ch[j] == 'l' || ch[j] == 'h') {
                j += 1;
            }
            if j < ch.len() {
                let conv = ch[j];
                let arg = args.get(ai).cloned().unwrap_or(Value::Null);
                ai += 1;
                let mut piece = match conv {
                    'd' | 'i' => arg.as_i64().map(|x| x.to_string()).unwrap_or_else(|| s(&arg)),
                    'u' => arg.as_i64().map(|x| (x as u32).to_string()).unwrap_or_default(),
                    'x' => arg.as_i64().map(|x| format!("{:x}", x)).unwrap_or_default(),
                    'X' => arg.as_i64().map(|x| format!("{:X}", x)).unwrap_or_default(),
                    'o' => arg.as_i64().map(|x| format!("{:o}", x)).unwrap_or_default(),
                    's' | 'S' => if arg.is_null() { "(null)".into() } else { s(&arg) },
                    _ => String::new(),
                };
                if let Ok(w) = width.parse::<usize>() {
                    if piece.len() < w {
                        if flags.contains('-') {
                            piece = format!("{:<w$}", piece, w = w);
                        } else if flags.contains('0') {
                            piece = format!("{:0>w$}", piece, w = w);
                        } else {
                            piece = format!("{:>w$}", piece, w = w);
                        }
                    }
                }
                out.push_str(&piece);
                i = j + 1;
                continue;
            }
        }
        out.push(ch[i]);
        i += 1;
    }
    out
}

// ------------------------------------------------------------ date helpers
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DUnit {
    Year,
    Quarter,
    Month,
    DayOfYear,
    Day,
    Week,
    Weekday,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
    IsoWeek,
    TzOffset,
}

pub fn unit_of(name: &str) -> Option<DUnit> {
    Some(match name.to_ascii_lowercase().as_str() {
        "year" | "yy" | "yyyy" => DUnit::Year,
        "quarter" | "qq" | "q" => DUnit::Quarter,
        "month" | "mm" | "m" => DUnit::Month,
        "dayofyear" | "dy" | "y" => DUnit::DayOfYear,
        "day" | "dd" | "d" => DUnit::Day,
        "week" | "wk" | "ww" => DUnit::Week,
        "weekday" | "dw" | "w" => DUnit::Weekday,
        "hour" | "hh" => DUnit::Hour,
        "minute" | "mi" | "n" => DUnit::Minute,
        "second" | "ss" | "s" => DUnit::Second,
        "millisecond" | "ms" => DUnit::Millisecond,
        "microsecond" | "mcs" => DUnit::Microsecond,
        "nanosecond" | "ns" => DUnit::Nanosecond,
        "iso_week" | "isowk" | "isoww" => DUnit::IsoWeek,
        "tzoffset" | "tz" => DUnit::TzOffset,
        _ => return None,
    })
}

fn get_unit(v: &Value) -> Result<DUnit> {
    let n = s(v);
    unit_of(&n).ok_or_else(|| SqlError::new(155, format!("'{}' is not a recognized datepart option.", n)))
}

/// Date-ish value to NaiveDateTime. Strings are parsed; times use 1900-01-01.
fn to_dt(v: &Value) -> Result<NaiveDateTime> {
    match v {
        Value::Time(t) => Ok(NaiveDateTime::new(convert::base_date(), *t)),
        Value::Str(st) => {
            let p = convert::parse_datetime_str(st).ok_or_else(|| SqlError::new(241, "Conversion failed when converting date and/or time from character string."))?;
            Ok(NaiveDateTime::new(p.date.unwrap_or_else(convert::base_date), p.time.unwrap_or_default()))
        }
        Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) => convert::ndt_of(v),
        _ => convert::ndt_of(v),
    }
}

fn dt_value_like(orig: &Value, d: NaiveDateTime) -> Result<Value> {
    Ok(match orig {
        Value::Date(_) => Value::Date(d.date()),
        Value::Time(_) => Value::Time(d.time()),
        Value::DateTime(_, DtKind::DateTime) | Value::Str(_) => Value::DateTime(convert::round_datetime(d), DtKind::DateTime),
        Value::DateTime(_, k) => Value::DateTime(d, *k),
        Value::Dto(o) => Value::Dto(d.and_local_timezone(*o.offset()).unwrap()),
        _ => Value::DateTime(convert::round_datetime(d), DtKind::DateTime),
    })
}

fn add_months(d: NaiveDateTime, n: i64) -> Option<NaiveDateTime> {
    if n >= 0 {
        d.checked_add_months(chrono::Months::new(n as u32))
    } else {
        d.checked_sub_months(chrono::Months::new((-n) as u32))
    }
}

fn dateadd(unit: DUnit, n: i64, v: &Value) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let d = to_dt(v)?;
    if matches!(v, Value::Date(_)) && matches!(unit, DUnit::Hour | DUnit::Minute | DUnit::Second | DUnit::Millisecond | DUnit::Microsecond | DUnit::Nanosecond) {
        return Err(SqlError::new(9810, format!("The datepart {:?} is not supported by date function dateadd for data type date.", unit).to_lowercase()));
    }
    let r = match unit {
        DUnit::Year => add_months(d, n * 12),
        DUnit::Quarter => add_months(d, n * 3),
        DUnit::Month => add_months(d, n),
        DUnit::DayOfYear | DUnit::Day | DUnit::Weekday => d.checked_add_signed(TimeDelta::days(n)),
        DUnit::Week | DUnit::IsoWeek => d.checked_add_signed(TimeDelta::days(n * 7)),
        DUnit::Hour => d.checked_add_signed(TimeDelta::hours(n)),
        DUnit::Minute => d.checked_add_signed(TimeDelta::minutes(n)),
        DUnit::Second => d.checked_add_signed(TimeDelta::seconds(n)),
        DUnit::Millisecond => d.checked_add_signed(TimeDelta::milliseconds(n)),
        DUnit::Microsecond => d.checked_add_signed(TimeDelta::microseconds(n)),
        DUnit::Nanosecond => d.checked_add_signed(TimeDelta::nanoseconds(n)),
        DUnit::TzOffset => None,
    };
    match r {
        Some(x) => {
            if x.year() < 1 || x.year() > 9999 {
                return Err(SqlError::new(517, "Adding a value to a 'datetime' column caused an overflow."));
            }
            if matches!(v, Value::DateTime(_, DtKind::DateTime) | Value::Str(_)) && x.year() < 1753 {
                return Err(SqlError::new(517, "Adding a value to a 'datetime' column caused an overflow."));
            }
            dt_value_like(v, x)
        }
        None => Err(SqlError::new(517, "Adding a value to a 'datetime' column caused an overflow.")),
    }
}

fn days_from_ce(d: NaiveDate) -> i64 {
    d.num_days_from_ce() as i64
}

fn datediff(unit: DUnit, a: &Value, b: &Value) -> Result<i64> {
    let x = to_dt(a)?;
    let y = to_dt(b)?;
    Ok(match unit {
        DUnit::Year => (y.year() - x.year()) as i64,
        DUnit::Quarter => ((y.year() - x.year()) * 4) as i64 + ((y.month0() / 3) as i64 - (x.month0() / 3) as i64),
        DUnit::Month => ((y.year() - x.year()) * 12) as i64 + (y.month() as i64 - x.month() as i64),
        DUnit::DayOfYear | DUnit::Day | DUnit::Weekday => days_from_ce(y.date()) - days_from_ce(x.date()),
        DUnit::Week | DUnit::IsoWeek => {
            // weeks start on Sunday (default DATEFIRST)
            let wk = |d: NaiveDate| (days_from_ce(d) + 0).div_euclid(7);
            // 0001-01-01 is a Monday; shift so that Sunday begins a week
            let wk2 = |d: NaiveDate| (days_from_ce(d) + 0 + 0).rem_euclid(7);
            let _ = wk2;
            let w = |d: NaiveDate| (days_from_ce(d) + 1 - 1 + 0).div_euclid(7) + if (days_from_ce(d)).rem_euclid(7) == 0 { 0 } else { 0 };
            let _ = (wk, w);
            let base = |d: NaiveDate| (days_from_ce(d) - 1 + 1).div_euclid(7);
            // day 1 (0001-01-01) is Monday => (days_from_ce % 7)==0 is Sunday
            let sun_week = |d: NaiveDate| (days_from_ce(d)).div_euclid(7);
            let _ = base;
            sun_week(y.date()) - sun_week(x.date())
        }
        DUnit::Hour => {
            let h = |d: NaiveDateTime| days_from_ce(d.date()) * 24 + d.hour() as i64;
            h(y) - h(x)
        }
        DUnit::Minute => {
            let m = |d: NaiveDateTime| (days_from_ce(d.date()) * 24 + d.hour() as i64) * 60 + d.minute() as i64;
            m(y) - m(x)
        }
        DUnit::Second => {
            let m = |d: NaiveDateTime| days_from_ce(d.date()) * 86400 + d.num_seconds_from_midnight() as i64;
            m(y) - m(x)
        }
        DUnit::Millisecond => {
            let m = |d: NaiveDateTime| (days_from_ce(d.date()) * 86400 + d.num_seconds_from_midnight() as i64) * 1000 + (d.nanosecond() / 1_000_000) as i64;
            m(y) - m(x)
        }
        DUnit::Microsecond => {
            let m = |d: NaiveDateTime| (days_from_ce(d.date()) * 86400 + d.num_seconds_from_midnight() as i64) * 1_000_000 + (d.nanosecond() / 1000) as i64;
            m(y) - m(x)
        }
        DUnit::Nanosecond => {
            let m = |d: NaiveDateTime| (days_from_ce(d.date()) * 86400 + d.num_seconds_from_midnight() as i64) * 1_000_000_000 + d.nanosecond() as i64;
            m(y) - m(x)
        }
        DUnit::TzOffset => 0,
    })
}

fn weekday_num(d: NaiveDate, datefirst: i32) -> i64 {
    let iso = d.weekday().number_from_monday() as i32; // 1=Mon..7=Sun
    (((iso - datefirst + 7) % 7) + 1) as i64
}

fn datepart(unit: DUnit, v: &Value, datefirst: i32) -> Result<i64> {
    let d = to_dt(v)?;
    Ok(match unit {
        DUnit::Year => d.year() as i64,
        DUnit::Quarter => (d.month0() / 3 + 1) as i64,
        DUnit::Month => d.month() as i64,
        DUnit::DayOfYear => d.ordinal() as i64,
        DUnit::Day => d.day() as i64,
        DUnit::Week => {
            let jan1 = NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap();
            let off = weekday_num(jan1, datefirst) - 1;
            ((d.ordinal() as i64 - 1 + off) / 7) + 1
        }
        DUnit::IsoWeek => d.iso_week().week() as i64,
        DUnit::Weekday => weekday_num(d.date(), datefirst),
        DUnit::Hour => d.hour() as i64,
        DUnit::Minute => d.minute() as i64,
        DUnit::Second => d.second() as i64,
        DUnit::Millisecond => (d.nanosecond() / 1_000_000) as i64,
        DUnit::Microsecond => (d.nanosecond() / 1000) as i64,
        DUnit::Nanosecond => d.nanosecond() as i64,
        DUnit::TzOffset => match v {
            Value::Dto(o) => (o.offset().local_minus_utc() / 60) as i64,
            _ => 0,
        },
    })
}

fn datetrunc(unit: DUnit, v: &Value) -> Result<Value> {
    let d = to_dt(v)?;
    let date = d.date();
    let r = match unit {
        DUnit::Year => NaiveDateTime::new(NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap(), NaiveTime::MIN),
        DUnit::Quarter => NaiveDateTime::new(NaiveDate::from_ymd_opt(d.year(), d.month0() / 3 * 3 + 1, 1).unwrap(), NaiveTime::MIN),
        DUnit::Month => NaiveDateTime::new(NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap(), NaiveTime::MIN),
        DUnit::Week => {
            let back = (date.weekday().num_days_from_sunday()) as i64;
            NaiveDateTime::new(date - TimeDelta::days(back), NaiveTime::MIN)
        }
        DUnit::IsoWeek => {
            let back = (date.weekday().num_days_from_monday()) as i64;
            NaiveDateTime::new(date - TimeDelta::days(back), NaiveTime::MIN)
        }
        DUnit::DayOfYear | DUnit::Day | DUnit::Weekday => NaiveDateTime::new(date, NaiveTime::MIN),
        DUnit::Hour => d.with_minute(0).unwrap().with_second(0).unwrap().with_nanosecond(0).unwrap(),
        DUnit::Minute => d.with_second(0).unwrap().with_nanosecond(0).unwrap(),
        DUnit::Second => d.with_nanosecond(0).unwrap(),
        DUnit::Millisecond => d.with_nanosecond(d.nanosecond() / 1_000_000 * 1_000_000).unwrap(),
        _ => d,
    };
    dt_value_like(v, r)
}

fn from_parts_date(y: i64, m: i64, d: i64) -> Result<NaiveDate> {
    NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32).ok_or_else(|| SqlError::new(289, "Cannot construct data type date, some of the arguments have values which are not valid."))
}

// ------------------------------------------------------------ time zones
struct Zone {
    name: &'static str,
    std: i32, // minutes
    dst: u8,  // 0 none, 1 US, 2 EU, 3 AU(south)
}
const ZONES: &[Zone] = &[
    Zone { name: "UTC", std: 0, dst: 0 },
    Zone { name: "Greenwich Standard Time", std: 0, dst: 0 },
    Zone { name: "GMT Standard Time", std: 0, dst: 2 },
    Zone { name: "W. Europe Standard Time", std: 60, dst: 2 },
    Zone { name: "Central Europe Standard Time", std: 60, dst: 2 },
    Zone { name: "Romance Standard Time", std: 60, dst: 2 },
    Zone { name: "Central European Standard Time", std: 60, dst: 2 },
    Zone { name: "E. Europe Standard Time", std: 120, dst: 2 },
    Zone { name: "FLE Standard Time", std: 120, dst: 2 },
    Zone { name: "Russian Standard Time", std: 180, dst: 0 },
    Zone { name: "Arab Standard Time", std: 180, dst: 0 },
    Zone { name: "India Standard Time", std: 330, dst: 0 },
    Zone { name: "China Standard Time", std: 480, dst: 0 },
    Zone { name: "Singapore Standard Time", std: 480, dst: 0 },
    Zone { name: "Tokyo Standard Time", std: 540, dst: 0 },
    Zone { name: "Korea Standard Time", std: 540, dst: 0 },
    Zone { name: "AUS Eastern Standard Time", std: 600, dst: 3 },
    Zone { name: "New Zealand Standard Time", std: 720, dst: 0 },
    Zone { name: "Eastern Standard Time", std: -300, dst: 1 },
    Zone { name: "Central Standard Time", std: -360, dst: 1 },
    Zone { name: "Mountain Standard Time", std: -420, dst: 1 },
    Zone { name: "US Mountain Standard Time", std: -420, dst: 0 },
    Zone { name: "Pacific Standard Time", std: -480, dst: 1 },
    Zone { name: "Alaskan Standard Time", std: -540, dst: 1 },
    Zone { name: "Hawaiian Standard Time", std: -600, dst: 0 },
    Zone { name: "SA Pacific Standard Time", std: -300, dst: 0 },
    Zone { name: "E. South America Standard Time", std: -180, dst: 0 },
];

fn nth_weekday(year: i32, month: u32, weekday: chrono::Weekday, n: u32) -> NaiveDate {
    let mut d = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let mut count = 0;
    loop {
        if d.weekday() == weekday {
            count += 1;
            if count == n {
                return d;
            }
        }
        d += TimeDelta::days(1);
    }
}
fn last_weekday(year: i32, month: u32, weekday: chrono::Weekday) -> NaiveDate {
    let mut d = if month == 12 { NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap() } else { NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap() } - TimeDelta::days(1);
    while d.weekday() != weekday {
        d -= TimeDelta::days(1);
    }
    d
}

/// Offset (minutes) of a zone at the given UTC instant.
fn zone_offset_utc(z: &Zone, utc: NaiveDateTime) -> i32 {
    let y = utc.year();
    let in_dst = match z.dst {
        1 => {
            let start = nth_weekday(y, 3, chrono::Weekday::Sun, 2).and_hms_opt(2, 0, 0).unwrap() - TimeDelta::minutes(z.std as i64);
            let end = nth_weekday(y, 11, chrono::Weekday::Sun, 1).and_hms_opt(2, 0, 0).unwrap() - TimeDelta::minutes(z.std as i64 + 60);
            utc >= start && utc < end
        }
        2 => {
            let start = last_weekday(y, 3, chrono::Weekday::Sun).and_hms_opt(1, 0, 0).unwrap();
            let end = last_weekday(y, 10, chrono::Weekday::Sun).and_hms_opt(1, 0, 0).unwrap();
            utc >= start && utc < end
        }
        3 => {
            let end = nth_weekday(y, 4, chrono::Weekday::Sun, 1).and_hms_opt(3, 0, 0).unwrap() - TimeDelta::minutes(z.std as i64 + 60);
            let start = nth_weekday(y, 10, chrono::Weekday::Sun, 1).and_hms_opt(2, 0, 0).unwrap() - TimeDelta::minutes(z.std as i64);
            utc < end || utc >= start
        }
        _ => false,
    };
    z.std + if in_dst { 60 } else { 0 }
}

pub fn at_time_zone(v: &Value, zone: &Value) -> Result<Value> {
    if v.is_null() || zone.is_null() {
        return Ok(Value::Null);
    }
    let zn = s(zone);
    let z = ZONES.iter().find(|z| z.name.eq_ignore_ascii_case(&zn)).ok_or_else(|| SqlError::new(9820, format!("The time zone parameter '{}' provided to AT TIME ZONE is invalid.", zn)))?;
    let utc: NaiveDateTime = match v {
        Value::Dto(d) => d.naive_utc(),
        other => {
            let local = to_dt(other)?;
            // interpret as local time in zone: try std offset then dst offset
            let guess = local - TimeDelta::minutes(z.std as i64);
            let off = zone_offset_utc(z, guess);
            let utc1 = local - TimeDelta::minutes(off as i64);
            let off2 = zone_offset_utc(z, utc1);
            local - TimeDelta::minutes(off2 as i64)
        }
    };
    let off = zone_offset_utc(z, utc);
    let fo = FixedOffset::east_opt(off * 60).unwrap();
    Ok(Value::Dto(DateTime::<FixedOffset>::from_naive_utc_and_offset(utc, fo)))
}

// ---------------------------------------------------------------- FORMAT
fn group_digits(int_part: &str) -> String {
    let mut out = String::new();
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn render_with(d: Option<Dec>, v: &Value, x: f64, decimals: usize) -> String {
    match d {
        Some(d) if matches!(v, Value::Dec(_) | Value::Int(_) | Value::BigInt(_)) => d.rescale(decimals.min(38) as u8).map(|x| x.to_string()).unwrap_or_else(|| format!("{:.*}", decimals, x)),
        _ => format!("{:.*}", decimals, x),
    }
}

fn format_number(v: &Value, fmt: &str) -> Result<String> {
    let f = num(v)?;
    let d: Option<Dec> = match v {
        Value::Dec(d) => Some(*d),
        Value::Int(i) => Some(Dec::from_int(*i as i64)),
        Value::BigInt(i) => Some(Dec::from_int(*i)),
        _ => Dec::from_f64(f),
    };
    let render = |x: f64, decimals: usize| -> String {
        render_with(d, v, x, decimals)
    };
    let render_pct = |x: f64, decimals: usize| -> String {
        let dp = d.and_then(|d| d.mul(&Dec::from_int(100)));
        render_with(dp, v, x, decimals)
    };
    let _unused = |x: f64, decimals: usize| -> String {
        let r = match d {
            Some(d) if matches!(v, Value::Dec(_) | Value::Int(_) | Value::BigInt(_)) => d.rescale(decimals.min(38) as u8).map(|x| x.to_string()).unwrap_or_else(|| format!("{:.*}", decimals, x)),
            _ => format!("{:.*}", decimals, x),
        };
        r
    };
    let (kind, prec) = {
        let mut it = fmt.chars();
        let k = it.next().unwrap_or('G');
        let rest: String = it.collect();
        (k, if rest.is_empty() { None } else { rest.parse::<usize>().ok() })
    };
    if fmt.len() <= 3 && kind.is_ascii_alphabetic() && (fmt.len() == 1 || prec.is_some()) {
        let r = match kind.to_ascii_uppercase() {
            'N' => {
                let r = render(f, prec.unwrap_or(2));
                let (neg, body) = if let Some(x) = r.strip_prefix('-') { (true, x.to_string()) } else { (false, r) };
                let (ip, fp) = match body.find('.') {
                    Some(i) => (body[..i].to_string(), body[i..].to_string()),
                    None => (body, String::new()),
                };
                format!("{}{}{}", if neg { "-" } else { "" }, group_digits(&ip), fp)
            }
            'F' => render(f, prec.unwrap_or(2)),
            'C' => {
                let r = render(f.abs(), prec.unwrap_or(2));
                let (ip, fp) = match r.find('.') {
                    Some(i) => (r[..i].to_string(), r[i..].to_string()),
                    None => (r.clone(), String::new()),
                };
                format!("{}${}{}", if f < 0.0 { "-" } else { "" }, group_digits(&ip), fp)
            }
            'P' => {
                let r = render_pct(f * 100.0, prec.unwrap_or(2));
                let (ip, fp) = match r.find('.') {
                    Some(i) => (r[..i].to_string(), r[i..].to_string()),
                    None => (r.clone(), String::new()),
                };
                format!("{} %", format!("{}{}", group_digits(&ip), fp))
            }
            'D' => {
                let i = f as i64;
                let body = format!("{}", i.abs());
                let body = format!("{:0>w$}", body, w = prec.unwrap_or(1));
                format!("{}{}", if i < 0 { "-" } else { "" }, body)
            }
            'X' => {
                let i = f as i64;
                let b = if kind == 'x' { format!("{:x}", i) } else { format!("{:X}", i) };
                format!("{:0>w$}", b, w = prec.unwrap_or(1))
            }
            'E' => {
                let p = prec.unwrap_or(6);
                let e = format!("{:.*e}", p, f);
                let (m, ex) = e.split_once('e').unwrap();
                let exn: i32 = ex.parse().unwrap();
                format!("{}{}{}{:03}", m, if kind == 'e' { "e" } else { "E" }, if exn < 0 { "-" } else { "+" }, exn.abs())
            }
            'G' => {
                if let Some(d) = d {
                    if matches!(v, Value::Dec(_) | Value::Int(_) | Value::BigInt(_)) {
                        return Ok(d.to_string());
                    }
                }
                convert::format_g(f, prec.unwrap_or(15))
            }
            'R' => format!("{}", f),
            _ => return Ok(fmt.to_string()),
        };
        return Ok(r);
    }
    // custom numeric format: sections separated by ';'
    let sections: Vec<&str> = fmt.split(';').collect();
    let sec = if f < 0.0 && sections.len() > 1 { sections[1] } else if f == 0.0 && sections.len() > 2 { sections[2] } else { sections[0] };
    let neg_sep = f < 0.0 && sections.len() <= 1;
    let x = f.abs();
    let percent = sec.contains('%');
    let x = if percent { x * 100.0 } else { x };
    let chars: Vec<char> = sec.chars().collect();
    let dot = chars.iter().position(|&c| c == '.');
    let (ipat, fpat): (Vec<char>, Vec<char>) = match dot {
        Some(i) => (chars[..i].to_vec(), chars[i + 1..].to_vec()),
        None => (chars.clone(), vec![]),
    };
    let is_digit_ph = |c: &char| *c == '0' || *c == '#';
    let frac_digits = fpat.iter().filter(|c| is_digit_ph(c)).count();
    let frac_min = fpat.iter().filter(|c| **c == '0').count();
    let comma = ipat.contains(&',');
    let rendered = format!("{:.*}", frac_digits, x);
    let (rint, rfrac) = match rendered.find('.') {
        Some(i) => (rendered[..i].to_string(), rendered[i + 1..].to_string()),
        None => (rendered.clone(), String::new()),
    };
    let min_int = ipat.iter().filter(|c| **c == '0').count();
    let mut rint = rint.trim_start_matches('0').to_string();
    while rint.len() < min_int {
        rint.insert(0, '0');
    }
    let mut rfrac = rfrac;
    while rfrac.len() > frac_min && rfrac.ends_with('0') {
        rfrac.pop();
    }
    let int_str = if comma { group_digits(&rint) } else { rint.clone() };
    // splice literals: prefix/suffix chars not placeholders
    let prefix: String = ipat.iter().take_while(|c| !is_digit_ph(c) && **c != ',').filter(|c| **c != '%').collect();
    let suffix: String = {
        let tail: String = if fpat.is_empty() { ipat.iter().rev().take_while(|c| !is_digit_ph(c)).collect::<Vec<_>>().into_iter().rev().filter(|c| **c != ',').collect() } else { fpat.iter().rev().take_while(|c| !is_digit_ph(c)).collect::<Vec<_>>().into_iter().rev().collect() };
        tail
    };
    let mut out = String::new();
    if neg_sep && x != 0.0 {
        out.push('-');
    }
    out.push_str(&prefix.replace('\'', ""));
    out.push_str(&int_str);
    if !rfrac.is_empty() {
        out.push('.');
        out.push_str(&rfrac);
    }
    out.push_str(&suffix.replace('\'', ""));
    if percent {
        out.push('%');
    }
    Ok(out)
}

fn format_datetime(d: NaiveDateTime, fmt: &str, tz: Option<i32>) -> String {
    let std = fmt.len() == 1;
    let pattern: String = if std {
        match fmt {
            "d" => "MM/dd/yyyy".into(),
            "D" => "dddd, MMMM dd, yyyy".into(),
            "f" => "dddd, MMMM dd, yyyy h:mm tt".into(),
            "F" => "dddd, MMMM dd, yyyy h:mm:ss tt".into(),
            "g" => "MM/dd/yyyy h:mm tt".into(),
            "G" => "MM/dd/yyyy h:mm:ss tt".into(),
            "M" | "m" => "MMMM dd".into(),
            "o" | "O" => "yyyy-MM-ddTHH:mm:ss.fffffff".into(),
            "R" | "r" => "ddd, dd MMM yyyy HH:mm:ss 'GMT'".into(),
            "s" => "yyyy-MM-ddTHH:mm:ss".into(),
            "t" => "h:mm tt".into(),
            "T" => "h:mm:ss tt".into(),
            "u" => "yyyy-MM-dd HH:mm:ssZ".into(),
            "U" => "dddd, MMMM dd, yyyy h:mm:ss tt".into(),
            "Y" | "y" => "MMMM yyyy".into(),
            _ => fmt.to_string(),
        }
    } else {
        fmt.to_string()
    };
    let ch: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let h12 = if d.hour() % 12 == 0 { 12 } else { d.hour() % 12 };
    while i < ch.len() {
        let c = ch[i];
        let mut n = 1;
        while i + n < ch.len() && ch[i + n] == c {
            n += 1;
        }
        match c {
            'y' => {
                if n == 1 {
                    out.push_str(&(d.year() % 100).to_string());
                } else if n == 2 {
                    out.push_str(&format!("{:02}", d.year() % 100));
                } else {
                    out.push_str(&format!("{:0>w$}", d.year(), w = n.max(4)));
                }
            }
            'M' => match n {
                1 => out.push_str(&d.month().to_string()),
                2 => out.push_str(&format!("{:02}", d.month())),
                3 => out.push_str(&convert::MONTHS[d.month0() as usize][..3]),
                _ => out.push_str(convert::MONTHS[d.month0() as usize]),
            },
            'd' => match n {
                1 => out.push_str(&d.day().to_string()),
                2 => out.push_str(&format!("{:02}", d.day())),
                3 => out.push_str(&convert::DAYS[d.weekday().num_days_from_monday() as usize][..3]),
                _ => out.push_str(convert::DAYS[d.weekday().num_days_from_monday() as usize]),
            },
            'H' => out.push_str(&if n == 1 { d.hour().to_string() } else { format!("{:02}", d.hour()) }),
            'h' => out.push_str(&if n == 1 { h12.to_string() } else { format!("{:02}", h12) }),
            'm' => out.push_str(&if n == 1 { d.minute().to_string() } else { format!("{:02}", d.minute()) }),
            's' => out.push_str(&if n == 1 { d.second().to_string() } else { format!("{:02}", d.second()) }),
            'f' | 'F' => {
                let full = format!("{:09}", d.nanosecond());
                let part = &full[..n.min(9)];
                if c == 'F' {
                    out.push_str(part.trim_end_matches('0'));
                } else {
                    out.push_str(part);
                }
            }
            't' => {
                let ap = if d.hour() < 12 { "AM" } else { "PM" };
                if n == 1 {
                    out.push_str(&ap[..1]);
                } else {
                    out.push_str(ap);
                }
            }
            'z' => {
                let off = tz.unwrap_or(0);
                let sign = if off < 0 { '-' } else { '+' };
                let a = off.abs();
                match n {
                    1 => out.push_str(&format!("{}{}", sign, a / 60)),
                    2 => out.push_str(&format!("{}{:02}", sign, a / 60)),
                    _ => out.push_str(&format!("{}{:02}:{:02}", sign, a / 60, a % 60)),
                }
            }
            '\'' => {
                let mut j = i + 1;
                while j < ch.len() && ch[j] != '\'' {
                    out.push(ch[j]);
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            '\\' => {
                if i + 1 < ch.len() {
                    out.push(ch[i + 1]);
                }
                i += 2;
                continue;
            }
            '/' => out.push('/'),
            ':' => out.push(':'),
            other => {
                for _ in 0..n {
                    out.push(other);
                }
            }
        }
        i += n;
    }
    out
}

fn format_value(v: &Value, fmt: &str) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(Value::Str(match v {
        Value::Date(_) | Value::DateTime(..) => format_datetime(to_dt(v)?, fmt, None),
        Value::Dto(d) => format_datetime(d.naive_local(), fmt, Some(d.offset().local_minus_utc() / 60)),
        Value::Time(t) => {
            let f = if fmt.len() == 1 { match fmt { "t" => "hh:mm tt", "T" => "hh:mm:ss tt", _ => "HH:mm:ss" }.to_string() } else { fmt.to_string() };
            format_datetime(NaiveDateTime::new(convert::base_date(), *t), &f, None)
        }
        Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) | Value::Bit(_) => format_number(v, fmt)?,
        other => s(other),
    }))
}

// ------------------------------------------------------------ misc string
fn soundex(st: &str) -> String {
    let code = |c: char| match c.to_ascii_uppercase() {
        'B' | 'F' | 'P' | 'V' => '1',
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => '2',
        'D' | 'T' => '3',
        'L' => '4',
        'M' | 'N' => '5',
        'R' => '6',
        _ => '0',
    };
    let letters: Vec<char> = st.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    if letters.is_empty() {
        return "0000".into();
    }
    let mut out = String::new();
    out.push(letters[0].to_ascii_uppercase());
    let mut last = code(letters[0]);
    for &c in &letters[1..] {
        let cd = code(c);
        let u = c.to_ascii_uppercase();
        if cd != '0' && cd != last {
            out.push(cd);
        }
        if u != 'H' && u != 'W' {
            last = cd;
        }
        if out.len() == 4 {
            break;
        }
    }
    while out.len() < 4 {
        out.push('0');
    }
    out
}

fn replace_ci(src: &str, find: &str, rep: &str) -> String {
    if find.is_empty() {
        return src.to_string();
    }
    let sl: Vec<char> = src.chars().collect();
    let fl: Vec<char> = find.chars().collect();
    let fl_lower: Vec<char> = fl.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let mut out = String::new();
    let mut i = 0;
    while i < sl.len() {
        if i + fl.len() <= sl.len() && (0..fl.len()).all(|k| sl[i + k].to_lowercase().next().unwrap_or(sl[i + k]) == fl_lower[k]) {
            out.push_str(rep);
            i += fl.len();
        } else {
            out.push(sl[i]);
            i += 1;
        }
    }
    out
}

fn charindex(find: &str, src: &str, start: i64) -> i64 {
    let sl: Vec<char> = src.chars().collect();
    let fl: Vec<char> = find.chars().collect();
    let st = if start < 1 { 0 } else { (start - 1) as usize };
    if fl.is_empty() {
        return 0;
    }
    let low = |c: char| c.to_lowercase().next().unwrap_or(c);
    let mut i = st;
    while i + fl.len() <= sl.len() {
        if (0..fl.len()).all(|k| low(sl[i + k]) == low(fl[k])) {
            return i as i64 + 1;
        }
        i += 1;
    }
    0
}

fn patindex(pat: &str, src: &str) -> i64 {
    let lead = pat.starts_with('%');
    let trail = pat.ends_with('%') && pat.len() > 1;
    let core = pat.trim_start_matches('%').trim_end_matches('%');
    let sl: Vec<char> = src.chars().collect();
    let max_start = if lead { sl.len() } else { 0 };
    for st in 0..=max_start {
        if st > sl.len() {
            break;
        }
        let ends: Vec<usize> = if trail { (st..=sl.len()).collect() } else { vec![sl.len()] };
        for en in ends {
            let sub: String = sl[st..en].iter().collect();
            if super::expr::like_match(&sub, core, None) {
                return st as i64 + 1;
            }
        }
    }
    0
}

fn json_escape(sv: &str) -> String {
    serde_json::Value::String(sv.to_string()).to_string()
}

// ------------------------------------------------------------------ JSON
#[derive(Debug, Clone)]
enum PStep {
    Key(String),
    Idx(usize),
}

fn parse_json_path(p: &str) -> Result<(bool, Vec<PStep>)> {
    let mut path = p.trim();
    let mut strict = false;
    if let Some(r) = path.strip_prefix("lax ") {
        path = r.trim();
    } else if let Some(r) = path.strip_prefix("strict ") {
        strict = true;
        path = r.trim();
    }
    let bad = || SqlError::new(13607, format!("JSON path is not properly formatted. Unexpected character '{}' is found at position 0.", p.chars().next().unwrap_or(' ')));
    let ch: Vec<char> = path.chars().collect();
    if ch.first() != Some(&'$') {
        return Err(bad());
    }
    let mut i = 1;
    let mut steps = vec![];
    while i < ch.len() {
        match ch[i] {
            '.' => {
                i += 1;
                if i < ch.len() && ch[i] == '"' {
                    let mut j = i + 1;
                    let mut k = String::new();
                    while j < ch.len() && ch[j] != '"' {
                        k.push(ch[j]);
                        j += 1;
                    }
                    steps.push(PStep::Key(k));
                    i = j + 1;
                } else {
                    let mut k = String::new();
                    while i < ch.len() && ch[i] != '.' && ch[i] != '[' {
                        k.push(ch[i]);
                        i += 1;
                    }
                    if k.is_empty() {
                        return Err(bad());
                    }
                    steps.push(PStep::Key(k));
                }
            }
            '[' => {
                let mut j = i + 1;
                let mut n = String::new();
                while j < ch.len() && ch[j] != ']' {
                    n.push(ch[j]);
                    j += 1;
                }
                steps.push(PStep::Idx(n.trim().parse().map_err(|_| bad())?));
                i = j + 1;
            }
            _ => return Err(bad()),
        }
    }
    Ok((strict, steps))
}

fn json_get<'j>(v: &'j serde_json::Value, steps: &[PStep]) -> Option<&'j serde_json::Value> {
    let mut cur = v;
    for st in steps {
        cur = match (st, cur) {
            (PStep::Key(k), serde_json::Value::Object(m)) => m.get(k)?,
            (PStep::Idx(i), serde_json::Value::Array(a)) => a.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn json_set(v: &mut serde_json::Value, steps: &[PStep], newv: Option<serde_json::Value>, append: bool) -> bool {
    if steps.is_empty() {
        if let Some(n) = newv {
            *v = n;
        }
        return true;
    }
    let (first, rest) = (&steps[0], &steps[1..]);
    if rest.is_empty() {
        match (first, v) {
            (PStep::Key(k), serde_json::Value::Object(m)) => {
                match newv {
                    Some(n) => {
                        if append {
                            if let Some(serde_json::Value::Array(a)) = m.get_mut(k) {
                                a.push(n);
                                return true;
                            }
                        }
                        m.insert(k.clone(), n);
                    }
                    None => {
                        m.shift_remove(k);
                    }
                }
                true
            }
            (PStep::Idx(i), serde_json::Value::Array(a)) => {
                match newv {
                    Some(n) => {
                        if *i < a.len() {
                            a[*i] = n;
                        } else {
                            a.push(n);
                        }
                    }
                    None => {
                        if *i < a.len() {
                            a.remove(*i);
                        }
                    }
                }
                true
            }
            _ => false,
        }
    } else {
        match (first, v) {
            (PStep::Key(k), serde_json::Value::Object(m)) => match m.get_mut(k) {
                Some(c) => json_set(c, rest, newv, append),
                None => false,
            },
            (PStep::Idx(i), serde_json::Value::Array(a)) => match a.get_mut(*i) {
                Some(c) => json_set(c, rest, newv, append),
                None => false,
            },
            _ => false,
        }
    }
}

fn json_insert_key(root: &mut serde_json::Value, steps: &[PStep], key: &str, n: serde_json::Value) {
    let mut cur = root;
    for st in steps {
        let next = match (st, cur) {
            (PStep::Key(kk), serde_json::Value::Object(m)) => m.get_mut(kk),
            (PStep::Idx(i), serde_json::Value::Array(arr)) => arr.get_mut(*i),
            _ => None,
        };
        match next {
            Some(x) => cur = x,
            None => return,
        }
    }
    if let Some(m) = cur.as_object_mut() {
        m.insert(key.to_string(), n);
    }
}

fn json_scalar_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Str(b.to_string()),
        serde_json::Value::Number(n) => Value::Str(n.to_string()),
        serde_json::Value::String(st) => Value::Str(st.clone()),
        other => Value::Str(other.to_string()),
    }
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bit(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::BigInt(i) => serde_json::Value::from(*i),
        Value::Dec(d) => serde_json::from_str(&d.to_string()).unwrap_or_else(|_| serde_json::Value::String(d.to_string())),
        Value::Float(f) => serde_json::Number::from_f64(*f).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
        Value::Date(d) => serde_json::Value::String(d.format("%Y-%m-%d").to_string()),
        Value::DateTime(d, _) => serde_json::Value::String(d.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        Value::Dto(d) => serde_json::Value::String(d.format("%Y-%m-%dT%H:%M:%S%.f%:z").to_string()),
        Value::Time(t) => serde_json::Value::String(convert::fmt_time(t, 7)),
        Value::Bin(b) => serde_json::Value::String(convert::hex(b)),
        other => serde_json::Value::String(to_str(other)),
    }
}

pub fn for_json(names: &[String], rows: &[Row], fc: &ForClause) -> String {
    let mut arr = vec![];
    for r in rows {
        let mut obj = serde_json::Map::new();
        for (i, v) in r.iter().enumerate() {
            if v.is_null() && !fc.include_null {
                continue;
            }
            let name = &names[i];
            let jv = value_to_json(v);
            if fc.mode == "PATH" && name.contains('.') {
                let parts: Vec<&str> = name.split('.').collect();
                let mut cur = &mut obj;
                for p in &parts[..parts.len() - 1] {
                    let e = cur.entry(p.to_string()).or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                    if !e.is_object() {
                        *e = serde_json::Value::Object(serde_json::Map::new());
                    }
                    cur = e.as_object_mut().unwrap();
                }
                cur.insert(parts[parts.len() - 1].to_string(), jv);
            } else {
                obj.insert(name.clone(), jv);
            }
        }
        arr.push(serde_json::Value::Object(obj));
    }
    let body = if fc.without_array_wrapper {
        arr.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")
    } else {
        serde_json::Value::Array(arr).to_string()
    };
    match &fc.root {
        Some(r) => format!("{{{}:{}}}", json_escape(r), body),
        None => body,
    }
}

fn xml_escape(st: &str) -> String {
    st.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
fn xml_val(v: &Value) -> String {
    match v {
        Value::Dec(_) | Value::Int(_) | Value::BigInt(_) => to_str(v),
        Value::Float(f) => convert::format_g(*f, 15),
        Value::DateTime(d, _) => d.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
        other => to_str(other),
    }
}

pub fn for_xml(names: &[String], rows: &[Row], fc: &ForClause) -> String {
    let rowname = fc.name.clone().unwrap_or_else(|| "row".to_string());
    let mut out = String::new();
    for r in rows {
        if fc.mode == "RAW" || fc.mode == "AUTO" {
            out.push_str(&format!("<{}", rowname));
            for (i, v) in r.iter().enumerate() {
                if !v.is_null() {
                    out.push_str(&format!(" {}=\"{}\"", names[i], xml_escape(&xml_val(v)).replace('"', "&quot;")));
                }
            }
            out.push_str("/>");
            continue;
        }
        let wrap = !(fc.mode == "PATH" && fc.name.as_deref() == Some(""));
        let mut attrs = String::new();
        let mut body = String::new();
        for (i, v) in r.iter().enumerate() {
            if v.is_null() {
                continue;
            }
            let n = &names[i];
            if let Some(a) = n.strip_prefix('@') {
                attrs.push_str(&format!(" {}=\"{}\"", a, xml_escape(&xml_val(v)).replace('"', "&quot;")));
            } else if n.is_empty() {
                body.push_str(&xml_escape(&xml_val(v)));
            } else {
                body.push_str(&format!("<{}>{}</{}>", n, xml_escape(&xml_val(v)), n));
            }
        }
        if wrap {
            out.push_str(&format!("<{}{}>{}</{}>", rowname, attrs, body, rowname));
        } else {
            out.push_str(&body);
        }
    }
    match &fc.root {
        Some(r) => format!("<{}>{}</{}>", r, out, r),
        None => out,
    }
}

// ----------------------------------------------------------- the dispatcher
impl<'a> Engine<'a> {
    pub fn builtin(&mut self, name: &str, a: Vec<Value>, fc: &FuncCall) -> Result<Option<Value>> {
        let v = match name {
            // ---------------------------------------------------- strings
            "LEN" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else {
                    let t = s(&a[0]);
                    let n = t.trim_end_matches(' ').chars().count();
                    if let Value::Str(x) = &a[0] { if x.chars().count() > 2_147_483_647 { Value::BigInt(n as i64) } else { Value::Int(n as i32) } } else { Value::Int(n as i32) }
                }
            }
            "DATALENGTH" => {
                need(name, &a, 1)?;
                match &a[0] {
                    Value::Null => Value::Null,
                    Value::Str(t) => Value::Int(t.chars().count() as i32),
                    Value::Bin(b) => Value::Int(b.len() as i32),
                    Value::Int(_) => Value::Int(4),
                    Value::BigInt(_) => Value::Int(8),
                    Value::Bit(_) => Value::Int(1),
                    Value::Float(_) => Value::Int(8),
                    Value::Dec(d) => Value::Int(match d.precision() { 1..=9 => 5, 10..=19 => 9, 20..=28 => 13, _ => 17 }),
                    Value::Guid(_) => Value::Int(16),
                    Value::Date(_) => Value::Int(3),
                    Value::Time(_) => Value::Int(5),
                    Value::DateTime(_, DtKind::DateTime) => Value::Int(8),
                    Value::DateTime(_, DtKind::SmallDateTime) => Value::Int(4),
                    Value::DateTime(..) => Value::Int(8),
                    Value::Dto(_) => Value::Int(10),
                }
            }
            "LEFT" | "RIGHT" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let n = int(&a[1])?;
                    if n < 0 {
                        return Err(SqlError::new(536, format!("Invalid length parameter passed to the {} function.", name.to_lowercase())));
                    }
                    let ch: Vec<char> = s(&a[0]).chars().collect();
                    let n = (n as usize).min(ch.len());
                    Value::Str(if name == "LEFT" { ch[..n].iter().collect() } else { ch[ch.len() - n..].iter().collect() })
                }
            }
            "SUBSTRING" => {
                need(name, &a, 3)?;
                if a[0].is_null() || a[1].is_null() || a[2].is_null() { Value::Null } else {
                    if let Value::Bin(b) = &a[0] {
                        let st = int(&a[1])?;
                        let ln = int(&a[2])?;
                        let from = (st.max(1) - 1) as usize;
                        let end = ((st + ln - 1).max(0) as usize).min(b.len());
                        return Ok(Some(Value::Bin(if from < end { b[from..end].to_vec() } else { vec![] })));
                    }
                    let st = int(&a[1])?;
                    let ln = int(&a[2])?;
                    if ln < 0 {
                        return Err(SqlError::new(537, "Invalid length parameter passed to the LEFT or SUBSTRING function."));
                    }
                    let ch: Vec<char> = s(&a[0]).chars().collect();
                    let from = st.max(1) - 1;
                    let end = (st + ln - 1).max(0);
                    let (from, end) = (from as usize, (end as usize).min(ch.len()));
                    Value::Str(if from < end { ch[from..end].iter().collect() } else { String::new() })
                }
            }
            "CHARINDEX" => {
                need_range(name, &a, 2, 3)?;
                if a[0].is_null() || a[1].is_null() { Value::Null } else {
                    let st = if a.len() > 2 { int(&a[2])? } else { 1 };
                    Value::Int(charindex(&s(&a[0]), &s(&a[1]), st) as i32)
                }
            }
            "PATINDEX" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else { Value::Int(patindex(&s(&a[0]), &s(&a[1])) as i32) }
            }
            "REPLACE" => {
                need(name, &a, 3)?;
                if any_null(&a) { Value::Null } else { Value::Str(replace_ci(&s(&a[0]), &s(&a[1]), &s(&a[2]))) }
            }
            "STUFF" => {
                need(name, &a, 4)?;
                if any_null(&a) { Value::Null } else {
                    let ch: Vec<char> = s(&a[0]).chars().collect();
                    let st = int(&a[1])?;
                    let ln = int(&a[2])?;
                    if st < 1 || st as usize > ch.len() + 0 || ln < 0 {
                        Value::Null
                    } else {
                        let st = (st - 1) as usize;
                        let en = (st + ln as usize).min(ch.len());
                        let mut o: String = ch[..st].iter().collect();
                        o.push_str(&s(&a[3]));
                        o.extend(ch[en..].iter());
                        Value::Str(o)
                    }
                }
            }
            "REPLICATE" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let n = int(&a[1])?;
                    if n < 0 { Value::Null } else { Value::Str(s(&a[0]).repeat(n as usize)) }
                }
            }
            "REVERSE" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else { Value::Str(s(&a[0]).chars().rev().collect()) }
            }
            "UPPER" | "LOWER" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else if name == "UPPER" { Value::Str(s(&a[0]).chars().map(|ch| { let u: String = ch.to_uppercase().collect(); if u.chars().count() == 1 { u } else { ch.to_string() } }).collect()) } else { Value::Str(s(&a[0]).to_lowercase()) }
            }
            "LTRIM" | "RTRIM" | "TRIM" => {
                need_range(name, &a, 1, 2)?;
                if a[0].is_null() { Value::Null } else {
                    let t = s(&a[0]);
                    let chars: Vec<char> = if a.len() > 1 { s(&a[1]).chars().collect() } else { vec![' '] };
                    let pred = |c: char| chars.contains(&c);
                    Value::Str(match name {
                        "LTRIM" => t.trim_start_matches(pred).to_string(),
                        "RTRIM" => t.trim_end_matches(pred).to_string(),
                        _ => t.trim_matches(pred).to_string(),
                    })
                }
            }
            "SPACE" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else { let n = int(&a[0])?; if n < 0 { Value::Null } else { Value::Str(" ".repeat(n as usize)) } }
            }
            "STR" => {
                need_range(name, &a, 1, 3)?;
                if a[0].is_null() { Value::Null } else {
                    let f = num(&a[0])?;
                    let len = if a.len() > 1 { int(&a[1])? as usize } else { 10 };
                    let dec = if a.len() > 2 { int(&a[2])?.clamp(0, 16) as usize } else { 0 };
                    let rendered = match &a[0] {
                        Value::Dec(d) => d.rescale(dec as u8).map(|x| x.to_string()).unwrap_or_else(|| format!("{:.*}", dec, f)),
                        _ => format!("{:.*}", dec, (f * 10f64.powi(dec as i32)).round() / 10f64.powi(dec as i32)),
                    };
                    if rendered.len() > len { Value::Str("*".repeat(len)) } else { Value::Str(format!("{:>w$}", rendered, w = len)) }
                }
            }
            "CONCAT" => {
                if a.len() < 2 {
                    return Err(SqlError::new(189, "The concat function requires 2 to 254 arguments."));
                }
                Value::Str(a.iter().map(|v| if v.is_null() { String::new() } else { s(v) }).collect())
            }
            "CONCAT_WS" => {
                if a.len() < 3 {
                    return Err(SqlError::new(189, "The concat_ws function requires 3 to 254 arguments."));
                }
                if a[0].is_null() { Value::Null } else {
                    let sep = s(&a[0]);
                    Value::Str(a[1..].iter().filter(|v| !v.is_null()).map(s).collect::<Vec<_>>().join(&sep))
                }
            }
            "FORMAT" => {
                need_range(name, &a, 2, 3)?;
                if a[0].is_null() || a[1].is_null() { Value::Null } else { format_value(&a[0], &s(&a[1]))? }
            }
            "ASCII" | "UNICODE" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else {
                    match s(&a[0]).chars().next() { Some(c) => Value::Int(c as i32), None => Value::Null }
                }
            }
            "CHAR" | "NCHAR" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else {
                    let n = int(&a[0])?;
                    if n < 0 || (name == "CHAR" && n > 255) { Value::Null } else { char::from_u32(n as u32).map(|c| Value::Str(c.to_string())).unwrap_or(Value::Null) }
                }
            }
            "QUOTENAME" => {
                need_range(name, &a, 1, 2)?;
                if a[0].is_null() { Value::Null } else {
                    let q = if a.len() > 1 { s(&a[1]) } else { "[".into() };
                    let t = s(&a[0]);
                    match q.as_str() {
                        "[" | "]" => Value::Str(format!("[{}]", t.replace(']', "]]"))),
                        "'" => Value::Str(format!("'{}'", t.replace('\'', "''"))),
                        "\"" => Value::Str(format!("\"{}\"", t.replace('"', "\"\""))),
                        "(" | ")" => Value::Str(format!("({})", t)),
                        "<" | ">" => Value::Str(format!("<{}>", t)),
                        "{" | "}" => Value::Str(format!("{{{}}}", t)),
                        "`" => Value::Str(format!("`{}`", t.replace('`', "``"))),
                        _ => Value::Null,
                    }
                }
            }
            "SOUNDEX" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else { Value::Str(soundex(&s(&a[0]))) }
            }
            "DIFFERENCE" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let x = soundex(&s(&a[0]));
                    let y = soundex(&s(&a[1]));
                    Value::Int(x.chars().zip(y.chars()).filter(|(p, q)| p == q).count() as i32)
                }
            }
            "TRANSLATE" => {
                need(name, &a, 3)?;
                if any_null(&a) { Value::Null } else {
                    let from: Vec<char> = s(&a[1]).chars().collect();
                    let to: Vec<char> = s(&a[2]).chars().collect();
                    if from.len() != to.len() {
                        return Err(SqlError::new(8185, "The second and third arguments of the TRANSLATE built-in function must contain an equal number of characters."));
                    }
                    Value::Str(s(&a[0]).chars().map(|c| from.iter().position(|f| *f == c).map(|i| to[i]).unwrap_or(c)).collect())
                }
            }
            "STRING_ESCAPE" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let t = json_escape(&s(&a[0]));
                    Value::Str(t[1..t.len() - 1].to_string())
                }
            }
            "REVERSE_" => Value::Null,
            // --------------------------------------------------------- math
            "ABS" => {
                need(name, &a, 1)?;
                match &a[0] {
                    Value::Null => Value::Null,
                    Value::Int(i) => Value::Int(i.checked_abs().ok_or_else(|| convert::overflow("int"))?),
                    Value::BigInt(i) => Value::BigInt(i.checked_abs().ok_or_else(|| convert::overflow("bigint"))?),
                    Value::Dec(d) => Value::Dec(d.abs()),
                    Value::Float(f) => Value::Float(f.abs()),
                    Value::Bit(b) => Value::Int(*b as i32),
                    other => Value::Float(num(other)?.abs()),
                }
            }
            "CEILING" | "FLOOR" => {
                need(name, &a, 1)?;
                match &a[0] {
                    Value::Null => Value::Null,
                    Value::Int(_) | Value::BigInt(_) | Value::Bit(_) => a[0].clone(),
                    Value::Dec(d) => {
                        let t = d.rescale(0).unwrap_or(*d);
                        let up = Dec::from_int(1);
                        let tr = Dec::new(d.to_i128_trunc(), 0);
                        let has_frac = d.rescale(0).map(|x| x != *d).unwrap_or(false);
                        let _ = (t, up);
                        let r = if !has_frac { tr } else if name == "CEILING" { if d.is_neg() { tr } else { tr.add(&Dec::from_int(1)).unwrap() } } else if d.is_neg() { tr.sub(&Dec::from_int(1)).unwrap() } else { tr };
                        Value::Dec(r)
                    }
                    other => { let f = num(other)?; Value::Float(if name == "CEILING" { f.ceil() } else { f.floor() }) }
                }
            }
            "ROUND" => {
                need_range(name, &a, 2, 3)?;
                if a[0].is_null() || a[1].is_null() { Value::Null } else {
                    let n = int(&a[1])?;
                    let trunc = a.len() > 2 && int(&a[2])? != 0;
                    match &a[0] {
                        Value::Int(_) | Value::BigInt(_) => {
                            if n >= 0 { a[0].clone() } else {
                                let f = 10f64.powi((-n) as i32);
                                let x = num(&a[0])?;
                                let r = if trunc { (x / f).trunc() * f } else { (x / f).round() * f };
                                cast(&Value::Float(r), &DataType::BigInt, None).map(|v| if matches!(a[0], Value::Int(_)) { cast(&v, &DataType::Int, None).unwrap_or(v.clone()) } else { v })?
                            }
                        }
                        Value::Dec(d) => {
                            if n >= 0 {
                                let ns = (n as u8).min(d.s);
                                let r = if trunc {
                                    let f = crate::decimal::POW10[(d.s - ns) as usize];
                                    Dec::new(d.m / f, ns)
                                } else {
                                    d.rescale(ns).unwrap_or(*d)
                                };
                                Value::Dec(r.rescale(d.s).unwrap_or(r))
                            } else {
                                let f = 10f64.powi((-n) as i32);
                                let x = d.to_f64();
                                let r = if trunc { (x / f).trunc() * f } else { (x / f).round() * f };
                                Value::Dec(Dec::from_f64(r).unwrap_or(*d).rescale(d.s).unwrap_or(*d))
                            }
                        }
                        other => {
                            let x = num(other)?;
                            let f = 10f64.powi(n as i32);
                            Value::Float(if trunc { (x * f).trunc() / f } else { (x * f).round() / f })
                        }
                    }
                }
            }
            "SIGN" => {
                need(name, &a, 1)?;
                match &a[0] {
                    Value::Null => Value::Null,
                    Value::Dec(d) => Value::Dec(Dec::new(if d.is_zero() { 0 } else if d.is_neg() { -crate::decimal::POW10[d.s as usize] } else { crate::decimal::POW10[d.s as usize] }, d.s)),
                    Value::Float(f) => Value::Float(if *f == 0.0 { 0.0 } else { f.signum() }),
                    Value::BigInt(i) => Value::BigInt(i.signum()),
                    other => Value::Int(num(other)?.signum() as i32 * if num(other)? == 0.0 { 0 } else { 1 }),
                }
            }
            "SQRT" | "SQUARE" | "EXP" | "LOG10" | "SIN" | "COS" | "TAN" | "ASIN" | "ACOS" | "ATAN" | "COT" | "DEGREES" | "RADIANS" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else {
                    let x = num(&a[0])?;
                    let r = match name {
                        "SQRT" => { if x < 0.0 { return Err(SqlError::new(3623, "A domain error occurred.")); } x.sqrt() }
                        "SQUARE" => x * x,
                        "EXP" => x.exp(),
                        "LOG10" => { if x <= 0.0 { return Err(SqlError::new(3623, "A domain error occurred.")); } x.log10() }
                        "SIN" => x.sin(),
                        "COS" => x.cos(),
                        "TAN" => x.tan(),
                        "ASIN" => { if !(-1.0..=1.0).contains(&x) { return Err(SqlError::new(3623, "A domain error occurred.")); } x.asin() }
                        "ACOS" => { if !(-1.0..=1.0).contains(&x) { return Err(SqlError::new(3623, "A domain error occurred.")); } x.acos() }
                        "ATAN" => x.atan(),
                        "COT" => 1.0 / x.tan(),
                        "DEGREES" => return Ok(Some(match &a[0] { Value::Int(_) => Value::Int(x.to_degrees() as i32), Value::BigInt(_) => Value::BigInt(x.to_degrees() as i64), _ => Value::Float(x.to_degrees()) })),
                        _ => x.to_radians(),
                    };
                    bigf(r)
                }
            }
            "LOG" => {
                need_range(name, &a, 1, 2)?;
                if any_null(&a) { Value::Null } else {
                    let x = num(&a[0])?;
                    if x <= 0.0 { return Err(SqlError::new(3623, "A domain error occurred.")); }
                    if a.len() > 1 { bigf(x.ln() / num(&a[1])?.ln()) } else { bigf(x.ln()) }
                }
            }
            "ATN2" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else { bigf(num(&a[0])?.atan2(num(&a[1])?)) }
            }
            "POWER" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    match (&a[0], &a[1]) {
                        (Value::Int(x), Value::Int(y)) if *y >= 0 => Value::Int((*x as i64).checked_pow(*y as u32).and_then(|v| i32::try_from(v).ok()).ok_or_else(|| convert::overflow("int"))?),
                        (Value::BigInt(x), Value::Int(y)) if *y >= 0 => Value::BigInt(x.checked_pow(*y as u32).ok_or_else(|| convert::overflow("bigint"))?),
                        (Value::Dec(d), Value::Int(y)) if *y >= 0 => {
                            let mut r = Dec::from_int(1);
                            for _ in 0..*y { r = r.mul(d).ok_or_else(|| convert::overflow("numeric"))?; }
                            Value::Dec(r.rescale(d.s).unwrap_or(r))
                        }
                        _ => bigf(num(&a[0])?.powf(num(&a[1])?)),
                    }
                }
            }
            "PI" => bigf(std::f64::consts::PI),
            "RAND" => {
                use rand::{Rng, SeedableRng};
                if !a.is_empty() && !a[0].is_null() {
                    let seed = int(&a[0])?;
                    let mut r = rand::rngs::StdRng::seed_from_u64(seed as u64);
                    bigf(r.gen::<f64>())
                } else {
                    bigf(rand::thread_rng().gen::<f64>())
                }
            }
            "GREATEST" | "LEAST" => {
                if a.is_empty() { return Err(SqlError::new(189, "The function requires at least 1 argument.")); }
                if any_null(&a) { Value::Null } else {
                    let mut best = a[0].clone();
                    for v in &a[1..] {
                        let o = compare(v, &best)?.unwrap_or(Ordering::Equal);
                        if (name == "GREATEST" && o == Ordering::Greater) || (name == "LEAST" && o == Ordering::Less) { best = v.clone(); }
                    }
                    best
                }
            }
            "NULLIF" => {
                need(name, &a, 2)?;
                if a[0].is_null() { Value::Null } else if compare(&a[0], &a[1])? == Some(Ordering::Equal) { Value::Null } else { a[0].clone() }
            }
            // --------------------------------------------------------- dates
            "GETDATE" | "CURRENT_TIMESTAMP" => Value::DateTime(convert::round_datetime(chrono::Local::now().naive_local()), DtKind::DateTime),
            "GETUTCDATE" => Value::DateTime(convert::round_datetime(chrono::Utc::now().naive_utc()), DtKind::DateTime),
            "SYSDATETIME" => Value::DateTime(chrono::Local::now().naive_local(), DtKind::DateTime2),
            "SYSUTCDATETIME" => Value::DateTime(chrono::Utc::now().naive_utc(), DtKind::DateTime2),
            "SYSDATETIMEOFFSET" => Value::Dto(chrono::Local::now().fixed_offset()),
            "CURRENT_DATE" => Value::Date(chrono::Local::now().date_naive()),
            "DATEADD" => {
                need(name, &a, 3)?;
                if a[1].is_null() || a[2].is_null() { Value::Null } else { dateadd(get_unit(&a[0])?, int(&a[1])?, &a[2])? }
            }
            "DATEDIFF" | "DATEDIFF_BIG" => {
                need(name, &a, 3)?;
                if a[1].is_null() || a[2].is_null() { Value::Null } else {
                    let r = datediff(get_unit(&a[0])?, &a[1], &a[2])?;
                    if name == "DATEDIFF" {
                        if r > i32::MAX as i64 || r < i32::MIN as i64 {
                            return Err(SqlError::new(535, format!("The datediff function resulted in an overflow. The number of dateparts separating two date/time instances is too large. Try to use datediff with a less precise datepart.")));
                        }
                        Value::Int(r as i32)
                    } else { Value::BigInt(r) }
                }
            }
            "DATEPART" => {
                need(name, &a, 2)?;
                if a[1].is_null() { Value::Null } else { Value::Int(datepart(get_unit(&a[0])?, &a[1], self.ses.datefirst)? as i32) }
            }
            "DATENAME" => {
                need(name, &a, 2)?;
                if a[1].is_null() { Value::Null } else {
                    let u = get_unit(&a[0])?;
                    let d = to_dt(&a[1])?;
                    match u {
                        DUnit::Month => Value::Str(convert::MONTHS[d.month0() as usize].to_string()),
                        DUnit::Weekday => Value::Str(convert::DAYS[d.weekday().num_days_from_monday() as usize].to_string()),
                        DUnit::TzOffset => match &a[1] {
                            Value::Dto(o) => { let m = o.offset().local_minus_utc() / 60; Value::Str(format!("{}{:02}:{:02}", if m < 0 { '-' } else { '+' }, m.abs() / 60, m.abs() % 60)) }
                            _ => Value::Str("+00:00".into()),
                        },
                        _ => Value::Str(datepart(u, &a[1], self.ses.datefirst)?.to_string()),
                    }
                }
            }
            "YEAR" | "MONTH" | "DAY" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Null } else {
                    let d = to_dt(&a[0])?;
                    Value::Int(match name { "YEAR" => d.year(), "MONTH" => d.month() as i32, _ => d.day() as i32 })
                }
            }
            "DATETRUNC" => {
                need(name, &a, 2)?;
                if a[1].is_null() { Value::Null } else { datetrunc(get_unit(&a[0])?, &a[1])? }
            }
            "EOMONTH" => {
                need_range(name, &a, 1, 2)?;
                if a[0].is_null() { Value::Null } else {
                    let mut d = to_dt(&a[0])?;
                    if a.len() > 1 && !a[1].is_null() {
                        d = add_months(d, int(&a[1])?).ok_or_else(|| SqlError::new(517, "Adding a value to a 'datetime' column caused an overflow."))?;
                    }
                    let first = NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap();
                    let next = if d.month() == 12 { NaiveDate::from_ymd_opt(d.year() + 1, 1, 1).unwrap() } else { NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1).unwrap() };
                    let _ = first;
                    Value::Date(next - TimeDelta::days(1))
                }
            }
            "DATEFROMPARTS" => {
                need(name, &a, 3)?;
                if any_null(&a) { Value::Null } else { Value::Date(from_parts_date(int(&a[0])?, int(&a[1])?, int(&a[2])?)?) }
            }
            "TIMEFROMPARTS" => {
                need(name, &a, 5)?;
                if any_null(&a) { Value::Null } else {
                    let frac = int(&a[3])?;
                    let prec = int(&a[4])?;
                    let ns = (frac as f64 / 10f64.powi(prec as i32) * 1e9) as u32;
                    Value::Time(NaiveTime::from_hms_nano_opt(int(&a[0])? as u32, int(&a[1])? as u32, int(&a[2])? as u32, ns).ok_or_else(|| SqlError::new(289, "Cannot construct data type time, some of the arguments have values which are not valid."))?)
                }
            }
            "DATETIMEFROMPARTS" | "DATETIME2FROMPARTS" | "SMALLDATETIMEFROMPARTS" => {
                if any_null(&a) { Value::Null } else {
                    let d = from_parts_date(int(&a[0])?, int(&a[1])?, int(&a[2])?)?;
                    let h = int(&a[3])? as u32;
                    let mi = int(&a[4])? as u32;
                    let (sec, ms) = if name == "SMALLDATETIMEFROMPARTS" { (0, 0) } else { (int(&a[5])? as u32, if a.len() > 6 { int(&a[6])? } else { 0 }) };
                    let ns = if name == "DATETIME2FROMPARTS" { (ms as f64 / 10f64.powi(if a.len() > 7 { int(&a[7])? as i32 } else { 7 }) * 1e9) as u32 } else { (ms * 1_000_000) as u32 };
                    let t = NaiveTime::from_hms_nano_opt(h, mi, sec, ns).ok_or_else(|| SqlError::new(289, "Cannot construct data type datetime, some of the arguments have values which are not valid."))?;
                    let dt = NaiveDateTime::new(d, t);
                    match name {
                        "DATETIMEFROMPARTS" => Value::DateTime(convert::round_datetime(dt), DtKind::DateTime),
                        "SMALLDATETIMEFROMPARTS" => Value::DateTime(dt, DtKind::SmallDateTime),
                        _ => Value::DateTime(dt, DtKind::DateTime2),
                    }
                }
            }
            "DATETIMEOFFSETFROMPARTS" => {
                if any_null(&a) { Value::Null } else {
                    let d = from_parts_date(int(&a[0])?, int(&a[1])?, int(&a[2])?)?;
                    let t = NaiveTime::from_hms_opt(int(&a[3])? as u32, int(&a[4])? as u32, int(&a[5])? as u32).ok_or_else(|| SqlError::new(289, "Cannot construct data type datetimeoffset."))?;
                    let off = FixedOffset::east_opt((int(&a[7])? * 3600 + int(&a[8])? * 60) as i32).ok_or_else(|| SqlError::new(289, "Cannot construct data type datetimeoffset."))?;
                    Value::Dto(NaiveDateTime::new(d, t).and_local_timezone(off).unwrap())
                }
            }
            "ISDATE" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Int(0) } else {
                    match &a[0] {
                        Value::Str(st) => Value::Int(convert::parse_datetime_str(st).map(|p| p.date.is_some() || p.time.is_some()).unwrap_or(false) as i32),
                        Value::Date(_) | Value::DateTime(..) => Value::Int(1),
                        _ => Value::Int(0),
                    }
                }
            }
            "SWITCHOFFSET" | "TODATETIMEOFFSET" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let minutes = match &a[1] {
                        Value::Str(st) => {
                            let p = convert::parse_datetime_str(&format!("2000-01-01 00:00 {}", st));
                            p.and_then(|p| p.tz).map(|x| x as i64).unwrap_or(0)
                        }
                        other => int(other)?,
                    };
                    let off = FixedOffset::east_opt((minutes * 60) as i32).ok_or_else(|| SqlError::new(9809, "The timezone provided to builtin function switchoffset is invalid."))?;
                    if name == "SWITCHOFFSET" {
                        match &a[0] {
                            Value::Dto(d) => Value::Dto(d.with_timezone(&off)),
                            other => { let d = to_dt(other)?; Value::Dto(DateTime::<FixedOffset>::from_naive_utc_and_offset(d, off).with_timezone(&off)) }
                        }
                    } else {
                        Value::Dto(to_dt(&a[0])?.and_local_timezone(off).unwrap())
                    }
                }
            }
            // ---------------------------------------------------- conversion
            "ISNUMERIC" => {
                need(name, &a, 1)?;
                if a[0].is_null() { Value::Int(0) } else {
                    Value::Int(match &a[0] {
                        Value::Str(st) => {
                            let t = st.trim();
                            (!t.is_empty() && (convert::str_to_number(t, "float").is_ok() || t.trim_start_matches(|c| "$£€¥".contains(c)).replace(',', "").parse::<f64>().is_ok())) as i32
                        }
                        Value::Int(_) | Value::BigInt(_) | Value::Dec(_) | Value::Float(_) | Value::Bit(_) => 1,
                        _ => 0,
                    })
                }
            }
            // ----------------------------------------------------- system
            "NEWID" | "NEWSEQUENTIALID" => Value::Guid(uuid::Uuid::new_v4()),
            "DB_NAME" => {
                if a.is_empty() || a[0].is_null() { Value::Str(self.ses.db.clone()) } else {
                    let id = int(&a[0])? as i32;
                    self.srv.databases.values().find(|d| d.id == id).map(|d| Value::Str(d.name.clone())).unwrap_or(Value::Null)
                }
            }
            "DB_ID" => {
                if a.is_empty() || a[0].is_null() { Value::Int(self.srv.databases[&self.ses.db.to_lowercase()].id) } else {
                    self.srv.databases.get(&s(&a[0]).to_lowercase()).map(|d| Value::Int(d.id)).unwrap_or(Value::Null)
                }
            }
            "SCHEMA_NAME" => {
                if a.is_empty() { Value::Str("dbo".into()) } else if a[0].is_null() { Value::Null } else {
                    match int(&a[0])? { 1 => Value::Str("dbo".into()), 2 => Value::Str("guest".into()), 3 => Value::Str("INFORMATION_SCHEMA".into()), 4 => Value::Str("sys".into()), _ => Value::Null }
                }
            }
            "SCHEMA_ID" => {
                if a.is_empty() { Value::Int(1) } else {
                    match s(&a[0]).to_lowercase().as_str() { "dbo" => Value::Int(1), "guest" => Value::Int(2), "information_schema" => Value::Int(3), "sys" => Value::Int(4), _ => Value::Null }
                }
            }
            "OBJECT_ID" => {
                need_range(name, &a, 1, 2)?;
                if a[0].is_null() { Value::Null } else {
                    let raw = s(&a[0]);
                    let parts: Vec<String> = raw.split('.').map(|p| p.trim().trim_matches(|c| c == '[' || c == ']' || c == '"').to_string()).collect();
                    let parts = if parts.len() >= 3 && parts[0].eq_ignore_ascii_case("tempdb") && parts[2].starts_with('#') { vec![parts[2].clone()] } else { parts };
                    let loc = self.locate(&parts);
                    let want = if a.len() > 1 && !a[1].is_null() { Some(s(&a[1]).trim().to_uppercase()) } else { None };
                    let found = match &loc {
                        Loc::Db(..) => self.object_ref(&loc).map(|o| (o.object_id(), o.type_code().to_string())),
                        _ => self.table_ref(&loc).map(|t| (t.object_id, "U".to_string())),
                    };
                    match found {
                        Some((id, tc)) if want.as_ref().map(|w| *w == tc || (w == "FN" && matches!(tc.as_str(), "FN" | "IF" | "TF"))).unwrap_or(true) => Value::Int(id),
                        _ => Value::Null,
                    }
                }
            }
            "OBJECT_NAME" | "OBJECT_SCHEMA_NAME" => {
                if a[0].is_null() { Value::Null } else {
                    let id = int(&a[0])? as i32;
                    let mut r = Value::Null;
                    if let Some(db) = self.srv.databases.get(&self.ses.db.to_lowercase()) {
                        for o in db.objects.values() {
                            if o.object_id() == id { r = Value::Str(if name == "OBJECT_NAME" { o.name().to_string() } else { o.schema().to_string() }); }
                        }
                    }
                    if r.is_null() {
                        for t in self.ses.temp.values() { if t.object_id == id && name == "OBJECT_NAME" { r = Value::Str(t.name.clone()); } }
                    }
                    r
                }
            }
            "USER_NAME" | "CURRENT_USER" | "SESSION_USER" | "USER" => Value::Str(self.current_user_name()),
            "SUSER_SNAME" | "SUSER_NAME" | "SYSTEM_USER" => Value::Str(self.eff_login()),
            "ORIGINAL_LOGIN" => Value::Str(self.ses.login.clone()),
            "IS_MEMBER" => {
                need(name, &a, 1)?;
                let dbk = self.ses.db.to_lowercase();
                let user = self.current_user_name();
                let role = s(&a[0]);
                let is = self.srv.databases.get(&dbk).map(|d| d.security.is_member(&user, &role)).unwrap_or(false) || (role.eq_ignore_ascii_case("db_owner") && self.is_sysadmin());
                Value::Int(is as i32)
            }
            "IS_SRVROLEMEMBER" => {
                need_range(name, &a, 1, 2)?;
                let role = s(&a[0]).to_lowercase();
                let login = if a.len() > 1 { s(&a[1]) } else { self.eff_login() };
                Value::Int(match role.as_str() {
                    "sysadmin" => self.srv.security.is_sysadmin(&login),
                    "dbcreator" => self.srv.security.dbcreators.contains(&login.to_lowercase()) || self.srv.security.is_sysadmin(&login),
                    _ => false,
                } as i32)
            }
            "HAS_PERMS_BY_NAME" => {
                need_range(name, &a, 3, 5)?;
                let perm = s(&a[2]).to_uppercase();
                let sec = s(&a[0]);
                let parts: Vec<String> = sec.split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                let loc = self.locate(&parts);
                Value::Int(self.check_object_perm(&loc, &perm, parts.last().map(|x| x.as_str()).unwrap_or("")).is_ok() as i32)
            }
            "HOST_NAME" => Value::Str(self.ses.host_name.clone()),
            "APP_NAME" => Value::Str(self.ses.app_name.clone()),
            "HOST_ID" => Value::Str("1234".into()),
            "DATABASEPROPERTYEX" => {
                need(name, &a, 2)?;
                let key = s(&a[0]).to_lowercase();
                let Some(d) = self.srv.databases.get(&key) else { return Ok(Some(Value::Null)) };
                match s(&a[1]).to_lowercase().as_str() {
                    "collation" => Value::Str(d.collation.clone()),
                    "recovery" => Value::Str(if d.recovery_full { "FULL" } else { "SIMPLE" }.into()),
                    "status" => Value::Str(if d.restoring { "RESTORING" } else { "ONLINE" }.into()),
                    "isanalyticsprocessingenabled" | "isautoclose" | "isautoshrink" => Value::Int(0),
                    "updateability" => Value::Str("READ_WRITE".into()),
                    "userfaccess" | "useraccess" => Value::Str("MULTI_USER".into()),
                    _ => Value::Null,
                }
            }
            "COLLATIONPROPERTY" => {
                need(name, &a, 2)?;
                let Some(c) = crate::collation::get(&s(&a[0])) else { return Ok(Some(Value::Null)) };
                match s(&a[1]).to_lowercase().as_str() {
                    "codepage" => Value::Int(1252),
                    "lcid" => Value::Int(1033),
                    "comparisonstyle" => Value::Int((if c.case_insensitive { 0 } else { 1 }) + (if c.accent_insensitive { 2 } else { 0 }) + (if c.binary { 0x1_0000 } else { 0 })),
                    "version" => Value::Int(if c.name.contains("_100") { 2 } else { 0 }),
                    _ => Value::Null,
                }
            }
            "SERVERPROPERTY" => {
                need(name, &a, 1)?;
                match s(&a[0]).to_lowercase().as_str() {
                    "productversion" => Value::Str("16.0.1000.6".into()),
                    "productmajorversion" => Value::Str("16".into()),
                    "edition" => Value::Str("Developer Edition (64-bit)".into()),
                    "engineedition" => Value::Int(3),
                    "servername" | "machinename" => Value::Str(self.srv.server_name.clone()),
                    "collation" => Value::Str(crate::collation::DEFAULT_NAME.into()),
                    "isclustered" | "isintegratedsecurityonly" => Value::Int(0),
                    "productlevel" => Value::Str("RTM".into()),
                    "instancename" => Value::Null,
                    _ => Value::Null,
                }
            }
            "CHECKSUM" | "BINARY_CHECKSUM" => {
                let mut h: i32 = 0;
                for v in &a { h = h.wrapping_mul(31).wrapping_add(checksum_value(v)); }
                Value::Int(h)
            }
            "HASHBYTES" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let data: Vec<u8> = match &a[1] { Value::Bin(b) => b.clone(), other => s(other).into_bytes() };
                    use sha2::Digest;
                    let out: Vec<u8> = match s(&a[0]).to_uppercase().as_str() {
                        "MD5" => md5::Md5::digest(&data).to_vec(),
                        "SHA" | "SHA1" => sha1::Sha1::digest(&data).to_vec(),
                        "SHA2_256" => sha2::Sha256::digest(&data).to_vec(),
                        "SHA2_512" => sha2::Sha512::digest(&data).to_vec(),
                        other => return Err(SqlError::new(15404, format!("Could not obtain information about Windows NT group/user '{}'. (invalid HASHBYTES algorithm)", other))),
                    };
                    Value::Bin(out)
                }
            }
            "ERROR_NUMBER" => self.ses.catch_stack.last().map(|e| Value::Int(e.number)).unwrap_or(Value::Null),
            "ERROR_MESSAGE" => self.ses.catch_stack.last().map(|e| Value::Str(e.message.clone())).unwrap_or(Value::Null),
            "ERROR_SEVERITY" => self.ses.catch_stack.last().map(|e| Value::Int(e.severity as i32)).unwrap_or(Value::Null),
            "ERROR_STATE" => self.ses.catch_stack.last().map(|e| Value::Int(e.state as i32)).unwrap_or(Value::Null),
            "ERROR_LINE" => self.ses.catch_stack.last().map(|_| Value::Int(1)).unwrap_or(Value::Null),
            "ERROR_PROCEDURE" => self.ses.catch_stack.last().map(|e| e.procedure.clone().map(Value::Str).unwrap_or(Value::Null)).unwrap_or(Value::Null),
            "XACT_STATE" => Value::Int(if self.ses.trancount == 0 { 0 } else if self.ses.doomed { -1 } else { 1 }),
            "SCOPE_IDENTITY" => match self.ses.last_identity { Some(v) => Value::Dec(Dec::from_int(v)), None => Value::Null },
            "IDENT_CURRENT" => {
                need(name, &a, 1)?;
                let parts: Vec<String> = s(&a[0]).split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                let loc = self.locate(&parts);
                match self.table_ref(&loc) {
                    Some(t) => match (t.identity_col(), t.identity_next.get()) {
                        (Some(c), Some(n)) => { let inc = t.columns[c].identity.unwrap().1; if t.live_rows() == 0 && n == t.columns[c].identity.unwrap().0 { Value::Dec(Dec::from_int(n)) } else { Value::Dec(Dec::from_int(n - inc)) } }
                        _ => Value::Null,
                    },
                    None => Value::Null,
                }
            }
            "IDENT_SEED" | "IDENT_INCR" => {
                let parts: Vec<String> = s(&a[0]).split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                let loc = self.locate(&parts);
                match self.table_ref(&loc).and_then(|t| t.identity_col().map(|c| t.columns[c].identity.unwrap())) {
                    Some((sd, inc)) => Value::Dec(Dec::from_int(if name == "IDENT_SEED" { sd } else { inc })),
                    None => Value::Null,
                }
            }
            "ROWCOUNT_BIG" => Value::BigInt(self.ses.rowcount),
            "COL_LENGTH" => {
                need(name, &a, 2)?;
                let parts: Vec<String> = s(&a[0]).split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
                let loc = self.locate(&parts);
                match self.table_ref(&loc).and_then(|t| t.col_index(&s(&a[1])).map(|i| t.columns[i].ty.clone())) {
                    Some(ty) => match ty.length() { Some(n) => Value::Int(n), None => Value::Int(4) },
                    None => Value::Null,
                }
            }
            "COL_NAME" => {
                need(name, &a, 2)?;
                let id = int(&a[0])? as i32;
                let cid = int(&a[1])? as usize;
                let mut r = Value::Null;
                if let Some(db) = self.srv.databases.get(&self.ses.db.to_lowercase()) {
                    for o in db.objects.values() {
                        if let Object::Table(t) = o { if t.object_id == id && cid >= 1 && cid <= t.columns.len() { r = Value::Str(t.columns[cid - 1].name.clone()); } }
                    }
                }
                r
            }
            "OBJECTPROPERTY" => {
                need(name, &a, 2)?;
                let id = int(&a[0])? as i32;
                let prop = s(&a[1]).to_lowercase();
                let mut r = Value::Null;
                if let Some(db) = self.srv.databases.get(&self.ses.db.to_lowercase()) {
                    for o in db.objects.values() {
                        if o.object_id() == id {
                            r = Value::Int(match prop.as_str() {
                                "istable" | "isusertable" => (o.type_code() == "U") as i32,
                                "isview" => (o.type_code() == "V") as i32,
                                "isprocedure" => (o.type_code() == "P") as i32,
                                "isscalarfunction" => (o.type_code() == "FN") as i32,
                                "istablefunction" => matches!(o.type_code(), "IF" | "TF") as i32,
                                "hasidentity" | "tablehasidentity" => if let Object::Table(t) = o { t.identity_col().is_some() as i32 } else { 0 },
                                "tablehasprimarykey" => if let Object::Table(t) = o { t.pk.is_some() as i32 } else { 0 },
                                _ => 0,
                            });
                        }
                    }
                }
                r
            }
            "COLUMNPROPERTY" => {
                need(name, &a, 3)?;
                let id = int(&a[0])? as i32;
                let cname = s(&a[1]);
                let prop = s(&a[2]).to_lowercase();
                let mut r = Value::Null;
                if let Some(db) = self.srv.databases.get(&self.ses.db.to_lowercase()) {
                    for o in db.objects.values() {
                        if let Object::Table(t) = o {
                            if t.object_id == id { if let Some(i) = t.col_index(&cname) {
                                r = Value::Int(match prop.as_str() { "allowsnull" => t.columns[i].nullable as i32, "isidentity" => t.columns[i].identity.is_some() as i32, "iscomputed" => t.columns[i].computed.is_some() as i32, "ordinal" => i as i32 + 1, _ => 0 });
                            } }
                        }
                    }
                }
                r
            }
            "TYPE_NAME" => Value::Null,
            "CONTEXT_INFO" => Value::Bin(self.ses.context_info.clone()),
            "CURRENT_TRANSACTION_ID" => Value::BigInt(self.ses.trancount as i64),
            "GETANSINULL" => Value::Int(1),
            "FN_" => Value::Null,
            // --------------------------------------------------------- JSON
            "ISJSON" => {
                need_range(name, &a, 1, 2)?;
                if a[0].is_null() { Value::Null } else { Value::Int(serde_json::from_str::<serde_json::Value>(&s(&a[0])).is_ok() as i32) }
            }
            "JSON_VALUE" | "JSON_QUERY" => {
                need_range(name, &a, 1, 2)?;
                if a[0].is_null() { Value::Null } else {
                    let (strict, steps) = parse_json_path(&if a.len() > 1 { s(&a[1]) } else { "$".into() })?;
                    let j: serde_json::Value = serde_json::from_str(&s(&a[0])).map_err(|_| SqlError::new(13609, "JSON text is not properly formatted. Unexpected character is found."))?;
                    match json_get(&j, &steps) {
                        None => {
                            if strict { return Err(SqlError::new(13608, "Property cannot be found on the specified JSON path.")); }
                            Value::Null
                        }
                        Some(x) => {
                            if name == "JSON_VALUE" {
                                if x.is_object() || x.is_array() { if strict { return Err(SqlError::new(13623, "Scalar value cannot be found in the specified JSON path.")); } Value::Null } else { json_scalar_to_value(x) }
                            } else if x.is_object() || x.is_array() { Value::Str(x.to_string()) } else if strict { return Err(SqlError::new(13624, "Object or array cannot be found in the specified JSON path.")); } else { Value::Null }
                        }
                    }
                }
            }
            "JSON_PATH_EXISTS" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let (_, steps) = parse_json_path(&s(&a[1]))?;
                    let j: serde_json::Value = serde_json::from_str(&s(&a[0])).map_err(|_| SqlError::new(13609, "JSON text is not properly formatted."))?;
                    Value::Int(json_get(&j, &steps).is_some() as i32)
                }
            }
            "JSON_MODIFY" => {
                need(name, &a, 3)?;
                if a[0].is_null() { Value::Null } else {
                    let mut p = s(&a[1]);
                    let mut append = false;
                    if let Some(r) = p.strip_prefix("append ") { append = true; p = r.to_string(); }
                    let (_, steps) = parse_json_path(&p)?;
                    let mut j: serde_json::Value = serde_json::from_str(&s(&a[0])).map_err(|_| SqlError::new(13609, "JSON text is not properly formatted."))?;
                    let from_query = matches!(fc.args.get(2), Some(Expr::Func(f)) if f.name == "JSON_QUERY");
                    let newv = if a[2].is_null() { None } else if from_query { serde_json::from_str(&s(&a[2])).ok() } else { Some(value_to_json(&a[2])) };
                    // setting a missing last key adds it
                    if !json_set(&mut j, &steps, newv.clone(), append) {
                        if let (Some(PStep::Key(k)), Some(n)) = (steps.last(), newv) {
                            json_insert_key(&mut j, &steps[..steps.len() - 1], k, n);
                        }
                    }
                    Value::Str(j.to_string())
                }
            }
            "JSON_ARRAY" => Value::Str(serde_json::Value::Array(a.iter().filter(|v| !v.is_null()).map(value_to_json).collect()).to_string()),
            "JSON_OBJECT" => {
                // arguments arrive as key:value pairs flattened by the parser as separate args
                let mut m = serde_json::Map::new();
                for pair in a.chunks(2) { if pair.len() == 2 { m.insert(s(&pair[0]), value_to_json(&pair[1])); } }
                Value::Str(serde_json::Value::Object(m).to_string())
            }
            // ----------------------------------------------------- misc
            "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE" | "LAG" | "LEAD" | "FIRST_VALUE" | "LAST_VALUE" | "PERCENT_RANK" | "CUME_DIST" => {
                return Err(SqlError::new(4112, format!("The function '{}' must have an OVER clause.", name)));
            }
            "PARSENAME" => {
                need(name, &a, 2)?;
                if any_null(&a) { Value::Null } else {
                    let full = s(&a[0]);
                    let parts: Vec<&str> = full.split('.').collect();
                    let n = int(&a[1])? as usize;
                    if n >= 1 && n <= parts.len() && parts.len() <= 4 { Value::Str(parts[parts.len() - n].to_string()) } else { Value::Null }
                }
            }
            "COMPRESS" | "DECOMPRESS" => a.first().cloned().unwrap_or(Value::Null),
            "TRY_CONVERT_" => Value::Null,
            _ => return Ok(None),
        };
        let _ = arith;
        Ok(Some(v))
    }

    // ------------------------------------------------------ table functions
    pub fn table_function(&mut self, name: &[String], args: Vec<Value>, with: Option<&Vec<(String, TypeRef, Option<String>)>>) -> Result<Rowset> {
        let up = name.last().unwrap().to_uppercase();
        let plain = name.len() == 1 || name[name.len() - 2].eq_ignore_ascii_case("sys");
        if plain {
            match up.as_str() {
                "STRING_SPLIT" => {
                    if args.len() < 2 {
                        return Err(SqlError::new(174, "The string_split function requires 2 argument(s)."));
                    }
                    let ordinal = args.get(2).map(|v| v.as_i64().unwrap_or(0) != 0).unwrap_or(false);
                    let mut cols = vec![ColMeta { qual: None, name: "value".into(), ty: Some(DataType::NVarChar(MAX)), denied: None, coll: None, mask: None }];
                    if ordinal {
                        cols.push(ColMeta { qual: None, name: "ordinal".into(), ty: Some(DataType::BigInt), denied: None, coll: None, mask: None });
                    }
                    if args[0].is_null() {
                        return Ok(Rowset::new(Schema::new(cols), vec![]));
                    }
                    let sep = s(&args[1]);
                    if sep.chars().count() != 1 {
                        return Err(SqlError::new(214, "Procedure expects parameter 'separator' of type 'nchar(1)/nvarchar(1)'."));
                    }
                    let sc = sep.chars().next().unwrap();
                    let rows: Vec<Row> = s(&args[0]).split(sc).enumerate().map(|(i, p)| {
                        let mut r = vec![Value::Str(p.to_string())];
                        if ordinal {
                            r.push(Value::BigInt(i as i64 + 1));
                        }
                        r
                    }).collect();
                    return Ok(Rowset::new(Schema::new(cols), rows));
                }
                "GENERATE_SERIES" => {
                    if args.len() < 2 {
                        return Err(SqlError::new(174, "The generate_series function requires 2 argument(s)."));
                    }
                    let col = |ty| Schema::new(vec![ColMeta { qual: None, name: "value".into(), ty: Some(ty), denied: None, coll: None, mask: None }]);
                    if args.iter().any(|v| v.is_null()) {
                        return Ok(Rowset::new(col(DataType::Int), vec![]));
                    }
                    let big = args.iter().any(|v| matches!(v, Value::BigInt(_)));
                    let start = int(&args[0])?;
                    let stop = int(&args[1])?;
                    let step = if args.len() > 2 { int(&args[2])? } else { 1 };
                    if step == 0 {
                        return Err(SqlError::new(8, "Step argument for GENERATE_SERIES cannot be zero."));
                    }
                    let mut rows = vec![];
                    let mut x = start;
                    while (step > 0 && x <= stop) || (step < 0 && x >= stop) {
                        rows.push(vec![if big { Value::BigInt(x) } else { Value::Int(x as i32) }]);
                        x += step;
                        if rows.len() > 50_000_000 {
                            break;
                        }
                    }
                    return Ok(Rowset::new(col(if big { DataType::BigInt } else { DataType::Int }), rows));
                }
                "OPENJSON" => return self.openjson(&args, with),
                "FN_HELPCOLLATIONS" => {
                    // the collations this server implements: every sensitivity combination of the common families
                    let mut rows = vec![];
                    for fam in ["SQL_Latin1_General_CP1", "Latin1_General", "Latin1_General_100", "French", "German_PhoneBook", "Turkish"] {
                        for sens in ["CI_AS", "CS_AS", "CI_AI", "CS_AI"] {
                            let n = format!("{}_{}", fam, sens);
                            let desc = format!("{}, {}, {}", fam.replace('_', "-"), if sens.contains("CI") { "case-insensitive" } else { "case-sensitive" }, if sens.contains("AI") { "accent-insensitive" } else { "accent-sensitive" });
                            rows.push(vec![Value::Str(n), Value::Str(desc)]);
                        }
                        for bin in ["BIN", "BIN2"] {
                            rows.push(vec![Value::Str(format!("{}_{}", fam, bin)), Value::Str(format!("{}, binary code point comparison sort", fam.replace('_', "-")))]);
                        }
                    }
                    return Ok(Rowset::from_rows(&["name".to_string(), "description".to_string()], rows));
                }
                _ => {}
            }
        }
        // user-defined table-valued function
        let schema = if name.len() >= 2 { Some(name[name.len() - 2].as_str()) } else { None };
        if let Some(f) = self.find_func(schema, name.last().unwrap()) {
            return self.call_tvf(&f, args);
        }
        Err(SqlError::new(208, format!("Invalid object name '{}'.", name.join("."))))
    }

    pub fn call_tvf(&mut self, f: &FuncDef, args: Vec<Value>) -> Result<Rowset> {
        let def = f.def.clone();
        let fdb = self.ses.db.to_lowercase();
        self.check_perm("SELECT", &fdb, &f.schema, Some(&obj_key(&f.schema, &f.name)), &f.name)?;
        let owner = self.schema_owner_of(&fdb, &f.schema);
        self.ses.module_owner.push(owner);
        let r = self.call_tvf_inner(f, &def, args);
        self.ses.module_owner.pop();
        r
    }

    fn call_tvf_inner(&mut self, f: &FuncDef, def: &Arc<CreateFunction>, args: Vec<Value>) -> Result<Rowset> {
        let mut frame = Frame::default();
        for (i, prm) in def.params.iter().enumerate() {
            let dt = self.resolve_type(&prm.ty)?;
            let val = match args.get(i) {
                Some(v) => cast(v, &dt, None)?,
                None => match &prm.default {
                    Some(d) => self.ev(d, &Env::empty())?,
                    None => return Err(SqlError::new(313, format!("An insufficient number of arguments were supplied for the procedure or function {}.{}.", f.schema, f.name))),
                },
            };
            frame.vars.insert(prm.name.clone(), Var { ty: dt, val });
        }
        if let Some(q) = &def.inline {
            self.ses.frames.push(frame);
            let saved = std::mem::take(&mut self.ses.cte_stack);
            let r = self.exec_query(q, None);
            self.ses.cte_stack = saved;
            self.ses.frames.pop();
            return r;
        }
        // multi-statement TVF
        let tdefs = def.ret_table.clone().unwrap_or_default();
        let var = def.ret_var.clone().unwrap_or_default();
        let t = self.build_table(&self.ses.db.clone(), "dbo", &var, &tdefs, true)?;
        frame.tables.insert(var.clone(), t);
        self.ses.frames.push(frame);
        self.ses.strict += 1;
        self.ses.nest_level += 1;
        let r = self.exec_stmts(&def.body);
        self.ses.nest_level -= 1;
        self.ses.strict -= 1;
        let fr = self.ses.frames.pop().unwrap();
        match r {
            Ok(()) | Err(Interrupt::Return) => {}
            Err(Interrupt::Error(e)) => return Err(e),
            Err(_) => {}
        }
        let t = fr.tables.get(&var).cloned().ok_or_else(|| SqlError::new(208, "Invalid return table."))?;
        Ok(Rowset::from_table(&t, &f.name))
    }

    fn openjson(&mut self, args: &[Value], with: Option<&Vec<(String, TypeRef, Option<String>)>>) -> Result<Rowset> {
        if args.is_empty() || args[0].is_null() {
            return Ok(Rowset::from_rows(&["key".to_string(), "value".to_string(), "type".to_string()], vec![]));
        }
        let j: serde_json::Value = serde_json::from_str(&s(&args[0])).map_err(|_| SqlError::new(13609, "JSON text is not properly formatted. Unexpected character is found."))?;
        let target = if args.len() > 1 && !args[1].is_null() {
            let (_, steps) = parse_json_path(&s(&args[1]))?;
            json_get(&j, &steps).cloned().unwrap_or(serde_json::Value::Null)
        } else {
            j
        };
        let items: Vec<(String, serde_json::Value)> = match &target {
            serde_json::Value::Array(a) => a.iter().enumerate().map(|(i, v)| (i.to_string(), v.clone())).collect(),
            serde_json::Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => return Err(SqlError::new(13610, "JSON path is not properly formatted or the target is not an object/array.")),
        };
        if let Some(w) = with {
            let mut cols = vec![];
            let mut types = vec![];
            for (n, tr, _) in w {
                let dt = self.resolve_type(tr)?;
                cols.push(ColMeta { qual: None, name: n.clone(), ty: Some(dt.clone()), denied: None, coll: None, mask: None });
                types.push(dt);
            }
            let mut rows = vec![];
            for (_, item) in &items {
                let mut row = vec![];
                for (i, (n, _, path)) in w.iter().enumerate() {
                    let p = path.clone().unwrap_or_else(|| format!("$.{}", n));
                    let (_, steps) = parse_json_path(&p)?;
                    let v = match json_get(item, &steps) {
                        None | Some(serde_json::Value::Null) => Value::Null,
                        Some(x) if x.is_object() || x.is_array() => Value::Str(x.to_string()),
                        Some(x) => cast(&json_scalar_to_value(x), &types[i], None)?,
                    };
                    row.push(v);
                }
                rows.push(row);
            }
            return Ok(Rowset::new(Schema::new(cols), rows));
        }
        let rows: Vec<Row> = items
            .iter()
            .map(|(k, v)| {
                let (val, ty) = match v {
                    serde_json::Value::Null => (Value::Null, 0),
                    serde_json::Value::String(st) => (Value::Str(st.clone()), 1),
                    serde_json::Value::Number(n) => (Value::Str(n.to_string()), 2),
                    serde_json::Value::Bool(b) => (Value::Str(b.to_string()), 3),
                    serde_json::Value::Array(_) => (Value::Str(v.to_string()), 4),
                    serde_json::Value::Object(_) => (Value::Str(v.to_string()), 5),
                };
                vec![Value::Str(k.clone()), val, Value::Int(ty)]
            })
            .collect();
        Ok(Rowset::from_rows(&["key".to_string(), "value".to_string(), "type".to_string()], rows))
    }
}

#[allow(dead_code)]
fn _keep(_: &Dec) {
    let _ = fmt_money_like;
}
