//! Fixed-point decimal with up to 38 digits (i128 mantissa + scale), SQL Server semantics.
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Dec {
    pub m: i128,
    pub s: u8,
}

pub const POW10: [i128; 39] = {
    let mut t = [1i128; 39];
    let mut i = 1;
    while i < 39 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

fn div_round(n: i128, d: i128) -> i128 {
    let q = n / d;
    let r = n % d;
    if r != 0 && (r.unsigned_abs() * 2) >= d.unsigned_abs() {
        if (n < 0) != (d < 0) {
            q - 1
        } else {
            q + 1
        }
    } else {
        q
    }
}

pub fn digits(m: i128) -> u32 {
    let mut a = m.unsigned_abs();
    let mut n = 0;
    while a > 0 {
        a /= 10;
        n += 1;
    }
    n
}

impl Dec {
    pub fn new(m: i128, s: u8) -> Dec {
        Dec { m, s }
    }
    pub fn from_int(v: i64) -> Dec {
        Dec { m: v as i128, s: 0 }
    }
    /// Number of significant digits (precision), at least scale and at least 1.
    pub fn precision(&self) -> u32 {
        digits(self.m).max(self.s as u32).max(1)
    }
    pub fn is_zero(&self) -> bool {
        self.m == 0
    }
    pub fn is_neg(&self) -> bool {
        self.m < 0
    }
    pub fn neg(&self) -> Dec {
        Dec { m: -self.m, s: self.s }
    }
    pub fn abs(&self) -> Dec {
        Dec { m: self.m.abs(), s: self.s }
    }
    pub fn rescale(&self, ns: u8) -> Option<Dec> {
        if ns == self.s {
            return Some(*self);
        }
        if ns > self.s {
            let f = POW10[(ns - self.s) as usize];
            self.m.checked_mul(f).map(|m| Dec { m, s: ns })
        } else {
            let f = POW10[(self.s - ns) as usize];
            Some(Dec { m: div_round(self.m, f), s: ns })
        }
    }
    /// Does the value fit decimal(p, s) after rescale?
    pub fn fit(&self, p: u8, s: u8) -> Option<Dec> {
        let r = self.rescale(s)?;
        if digits(r.m) > p as u32 {
            None
        } else {
            Some(r)
        }
    }
    pub fn parse(src: &str) -> Option<Dec> {
        let t = src.trim();
        let first = *t.as_bytes().first()?;
        let (neg, body) = match first {
            b'-' => (true, &t[1..]),
            b'+' => (false, &t[1..]),
            _ => (false, t),
        };
        if body.is_empty() {
            return None;
        }
        let (ip, fp) = match body.find('.') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, ""),
        };
        if ip.is_empty() && fp.is_empty() {
            return None;
        }
        if !ip.bytes().all(|c| c.is_ascii_digit()) || !fp.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let ip = ip.trim_start_matches('0');
        let mut all = String::from(ip);
        let mut fp = fp.to_string();
        while all.len() + fp.len() > 38 && !fp.is_empty() {
            fp.pop();
        }
        all.push_str(&fp);
        if all.len() > 38 {
            return None;
        }
        let m: i128 = if all.is_empty() { 0 } else { all.parse().ok()? };
        Some(Dec { m: if neg { -m } else { m }, s: fp.len() as u8 })
    }
    pub fn to_f64(&self) -> f64 {
        self.m as f64 / 10f64.powi(self.s as i32)
    }
    pub fn from_f64(f: f64) -> Option<Dec> {
        if !f.is_finite() {
            return None;
        }
        let s = format!("{:.15}", f);
        let mut d = Dec::parse(&s)?;
        while d.s > 0 && d.m % 10 == 0 {
            d.m /= 10;
            d.s -= 1;
        }
        Some(d)
    }
    pub fn to_i128_trunc(&self) -> i128 {
        self.m / POW10[self.s as usize]
    }
    /// Remove trailing zeros (for hashing / normalised compare).
    pub fn normalized(&self) -> Dec {
        let mut d = *self;
        if d.m == 0 {
            d.s = 0;
            return d;
        }
        while d.s > 0 && d.m % 10 == 0 {
            d.m /= 10;
            d.s -= 1;
        }
        d
    }
    fn align(a: &Dec, b: &Dec) -> Option<(i128, i128, u8)> {
        let s = a.s.max(b.s);
        let x = a.rescale(s)?.m;
        let y = b.rescale(s)?.m;
        Some((x, y, s))
    }
    fn fit38(mut m: i128, mut s: u8) -> Option<Dec> {
        while digits(m) > 38 && s > 0 {
            m = div_round(m, 10);
            s -= 1;
        }
        if digits(m) > 38 {
            None
        } else {
            Some(Dec { m, s })
        }
    }
    pub fn add(&self, o: &Dec) -> Option<Dec> {
        let (x, y, s) = Dec::align(self, o)?;
        Dec::fit38(x.checked_add(y)?, s)
    }
    pub fn sub(&self, o: &Dec) -> Option<Dec> {
        let (x, y, s) = Dec::align(self, o)?;
        Dec::fit38(x.checked_sub(y)?, s)
    }
    pub fn mul(&self, o: &Dec) -> Option<Dec> {
        let mut a = *self;
        let mut b = *o;
        let mut m = a.m.checked_mul(b.m);
        while m.is_none() && (a.s > 0 || b.s > 0) {
            if a.s >= b.s && a.s > 0 {
                a = a.rescale(a.s - 1)?;
            } else {
                b = b.rescale(b.s - 1)?;
            }
            m = a.m.checked_mul(b.m);
        }
        let m = m?;
        let s = a.s as u32 + b.s as u32;
        if s > 38 {
            let drop = (s - 38) as usize;
            return Dec::fit38(div_round(m, POW10[drop]), 38);
        }
        Dec::fit38(m, s as u8)
    }
    /// Division following SQL Server result-scale rules. p1/p2 are operand precisions.
    pub fn div(&self, o: &Dec, p1: u32, p2: u32) -> Option<Dec> {
        if o.m == 0 {
            return None;
        }
        let s1 = self.s as i32;
        let s2 = o.s as i32;
        let mut rs = std::cmp::max(6, s1 + p2 as i32 + 1);
        let rp = p1 as i32 - s1 + s2 + rs;
        if rp > 38 {
            rs = std::cmp::max(std::cmp::min(rs, 6), rs - (rp - 38));
        }
        let e = rs - s1 + s2;
        let (num, den) = if e >= 0 {
            (self.m.checked_mul(POW10[e as usize])?, o.m)
        } else {
            (self.m, o.m.checked_mul(POW10[(-e) as usize])?)
        };
        Dec::fit38(div_round(num, den), rs as u8)
    }
    pub fn rem(&self, o: &Dec) -> Option<Dec> {
        let (x, y, s) = Dec::align(self, o)?;
        if y == 0 {
            return None;
        }
        Some(Dec { m: x % y, s })
    }
}

impl PartialEq for Dec {
    fn eq(&self, o: &Dec) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Dec {}
impl PartialOrd for Dec {
    fn partial_cmp(&self, o: &Dec) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Dec {
    fn cmp(&self, o: &Dec) -> Ordering {
        match Dec::align(self, o) {
            Some((x, y, _)) => x.cmp(&y),
            None => self.to_f64().partial_cmp(&o.to_f64()).unwrap_or(Ordering::Equal),
        }
    }
}

impl fmt::Display for Dec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.m < 0;
        let mut digs = self.m.unsigned_abs().to_string();
        let s = self.s as usize;
        if s > 0 {
            while digs.len() <= s {
                digs.insert(0, '0');
            }
            digs.insert(digs.len() - s, '.');
        }
        if neg {
            write!(f, "-{}", digs)
        } else {
            write!(f, "{}", digs)
        }
    }
}
