//! Dynamic data masking: the masking functions of `MASKED WITH (FUNCTION = '...')`.
//!
//! A masked column is returned to users without the `UNMASK` permission in a disguised form; stored data, comparisons and
//! constraints are not affected (as in SQL Server, a user can still probe masked values with `WHERE`).
use crate::types::{DataType, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mask {
    /// `default()`: XXXX for strings, 0 for numbers, 1900-01-01 for dates, 0x00 for binary.
    Default,
    /// `email()`: `aXXX@XXXX.com`.
    Email,
    /// `random(low, high)` for numbers.
    Random(i64, i64),
    /// `partial(prefix, 'padding', suffix)` for strings.
    Partial(usize, String, usize),
}

impl Mask {
    /// Parse the text of a masking function. `Err` carries the reason.
    pub fn parse(text: &str) -> Result<Mask, String> {
        let t = text.trim();
        let lower = t.to_lowercase();
        if lower == "default()" {
            return Ok(Mask::Default);
        }
        if lower == "email()" {
            return Ok(Mask::Email);
        }
        let args = |name: &str| -> Option<&str> { lower.strip_prefix(name).and_then(|r| r.strip_suffix(')')).map(|_| &t[name.len()..t.len() - 1]) };
        if let Some(a) = args("random(") {
            let nums: Vec<&str> = a.split(',').map(|s| s.trim()).collect();
            if let [lo, hi] = nums.as_slice() {
                if let (Ok(lo), Ok(hi)) = (lo.parse::<i64>(), hi.parse::<i64>()) {
                    if lo <= hi {
                        return Ok(Mask::Random(lo, hi));
                    }
                }
            }
            return Err("random() needs two numbers, the first not greater than the second".into());
        }
        if let Some(a) = args("partial(") {
            // partial(prefix, "padding", suffix); the padding may contain commas and is quoted with " or '
            let mut parts: Vec<String> = vec![];
            let (mut cur, mut quote) = (String::new(), None::<char>);
            for c in a.chars() {
                match (c, quote) {
                    ('"' | '\'', None) => quote = Some(c),
                    (q, Some(open)) if q == open => quote = None,
                    (',', None) => parts.push(std::mem::take(&mut cur)),
                    (c, _) => cur.push(c),
                }
            }
            parts.push(cur);
            if let [p, pad, s] = parts.iter().map(|x| x.trim().to_string()).collect::<Vec<_>>().as_slice() {
                if let (Ok(p), Ok(s)) = (p.parse::<usize>(), s.parse::<usize>()) {
                    return Ok(Mask::Partial(p, pad.clone(), s));
                }
            }
            return Err("partial() needs a prefix length, a padding string and a suffix length".into());
        }
        Err(format!("'{}' is not a masking function (default(), email(), random(a, b), partial(prefix, \"padding\", suffix))", text))
    }

    /// Can this function mask a column of type `ty`?
    pub fn fits(&self, ty: &DataType) -> bool {
        match self {
            Mask::Default => true,
            Mask::Email | Mask::Partial(..) => ty.is_string(),
            Mask::Random(..) => ty.is_numeric(),
        }
    }

    /// The disguised form of `v` (a value of a column of type `ty`).
    pub fn apply(&self, ty: &DataType, v: &Value) -> Value {
        if v.is_null() {
            return Value::Null;
        }
        match (self, v) {
            (Mask::Default, Value::Str(_)) => {
                let n = ty.length().filter(|n| *n > 0 && *n < 4).unwrap_or(4) as usize;
                Value::Str("X".repeat(n))
            }
            (Mask::Default, Value::Int(_)) => Value::Int(0),
            (Mask::Default, Value::BigInt(_)) => Value::BigInt(0),
            (Mask::Default, Value::Float(_)) => Value::Float(0.0),
            (Mask::Default, Value::Dec(d)) => Value::Dec(crate::decimal::Dec::new(0, d.s)),
            (Mask::Default, Value::Bit(_)) => Value::Bit(false),
            (Mask::Default, Value::Bin(_)) => Value::Bin(vec![0]),
            (Mask::Default, Value::Date(_)) => Value::Date(chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap()),
            (Mask::Default, Value::DateTime(_, k)) => Value::DateTime(chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap(), *k),
            (Mask::Default, Value::Time(_)) => Value::Time(chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
            (Mask::Email, Value::Str(s)) => match s.chars().next() {
                Some(c) => Value::Str(format!("{}XXX@XXXX.com", c)),
                None => Value::Str(String::new()),
            },
            (Mask::Random(lo, hi), Value::Int(_)) => Value::Int(rand_in(*lo, *hi) as i32),
            (Mask::Random(lo, hi), Value::BigInt(_)) => Value::BigInt(rand_in(*lo, *hi)),
            (Mask::Random(lo, hi), Value::Float(_)) => Value::Float(rand_in(*lo, *hi) as f64),
            (Mask::Random(lo, hi), Value::Dec(d)) => Value::Dec(crate::decimal::Dec::new(rand_in(*lo, *hi) as i128 * crate::decimal::POW10[d.s as usize], d.s)),
            (Mask::Partial(p, pad, s), Value::Str(x)) => {
                let chars: Vec<char> = x.chars().collect();
                if p + s >= chars.len() {
                    // the visible parts would cover everything: show only the padding, as SQL Server does
                    return Value::Str(pad.clone());
                }
                let mut out: String = chars[..*p].iter().collect();
                out.push_str(pad);
                out.extend(chars[chars.len() - s..].iter());
                Value::Str(out)
            }
            _ => v.clone(),
        }
    }
}

fn rand_in(lo: i64, hi: i64) -> i64 {
    use rand::Rng;
    rand::thread_rng().gen_range(lo..=hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_parse_and_mask() {
        assert_eq!(Mask::parse("default()").unwrap(), Mask::Default);
        assert_eq!(Mask::parse("EMAIL()").unwrap(), Mask::Email);
        assert_eq!(Mask::parse("random(1, 9)").unwrap(), Mask::Random(1, 9));
        assert_eq!(Mask::parse("partial(2, \"xx-xx\", 1)").unwrap(), Mask::Partial(2, "xx-xx".into(), 1));
        for bad in ["", "nothing()", "random(9, 1)", "partial(1,2)", "default"] {
            assert!(Mask::parse(bad).is_err(), "{}", bad);
        }
        let s = DataType::VarChar(20);
        let show = |v: Value| format!("{}", v);
        assert_eq!(show(Mask::Default.apply(&s, &Value::Str("secret".into()))), "XXXX");
        assert_eq!(show(Mask::Default.apply(&DataType::VarChar(2), &Value::Str("ab".into()))), "XX");
        assert_eq!(show(Mask::Default.apply(&DataType::Int, &Value::Int(42))), "0");
        assert_eq!(show(Mask::Email.apply(&s, &Value::Str("john@x.org".into()))), "jXXX@XXXX.com");
        assert_eq!(show(Mask::Partial(2, "xxxx".into(), 3).apply(&s, &Value::Str("1234567890".into()))), "12xxxx890");
        assert_eq!(show(Mask::Partial(5, "x".into(), 5).apply(&s, &Value::Str("short".into()))), "x");
        assert_eq!(show(Mask::Default.apply(&s, &Value::Null)), "NULL");
        match Mask::Random(5, 7).apply(&DataType::Int, &Value::Int(1000)) {
            Value::Int(n) => assert!((5..=7).contains(&n)),
            v => panic!("{:?}", v),
        }
    }
}
