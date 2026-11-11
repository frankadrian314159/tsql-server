//! SQL Server's binary XML (the stored form of the `xml` type), turned back into text.
//!
//! A value starts with `DF FF`, a version byte (1 for untyped XML, 2 for XML bound to a schema collection) and a code page
//! (`B0 04` is 1200, UTF-16). What follows is a stream of tokens, which this decoder reads as they were observed in real
//! backups:
//!
//! | token | meaning |
//! | --- | --- |
//! | `F0 n chars` | defines the next name (`n` UTF-16 units, `n` a base-128 number); names are numbered from 1 |
//! | `EF ns prefix name` | defines the next qualified name from three name numbers (0 is none) |
//! | `F8 q` | start of an element with qualified name `q`; `F6 q value` are its attributes and `F5` ends them |
//! | `F7` | end of an element |
//! | `11 n chars` | text |
//! | `EA n bytes` | type information (skipped), or in typed XML the type of the value that follows |
//!
//! A qualified name with prefix `xmlns` is a namespace declaration. In typed XML a value is stored in the form of its type:
//! strings and years as text, decimals (and the integer types derived from them) as a precision, a scale, a sign byte and a
//! 16-byte little-endian mantissa, floats as `03` and four bytes, dates as `7C` and a `datetimeoffset`. A type that is not
//! known makes the whole value an error, so that it is left out instead of being written wrongly.
use chrono::NaiveDate;

type Res<T> = Result<T, String>;

fn mb32(b: &[u8], i: &mut usize) -> Res<usize> {
    let (mut v, mut s) = (0usize, 0u32);
    loop {
        let c = *b.get(*i).ok_or("the XML ends inside a number")?;
        *i += 1;
        v |= ((c & 0x7F) as usize) << s;
        s += 7;
        if c & 0x80 == 0 {
            return Ok(v);
        }
        if s > 28 {
            return Err("a number in the XML is too long".into());
        }
    }
}

fn utf16(b: &[u8], i: &mut usize, n: usize) -> Res<String> {
    let bytes = b.get(*i..*i + 2 * n).ok_or("the XML ends inside a string")?;
    *i += 2 * n;
    let u: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16(&u).map_err(|_| "a string in the XML is not valid UTF-16".to_string())
}

fn escape(s: &str, attr: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' if attr => o.push_str("&quot;"),
            '\r' => o.push_str("&#xD;"),
            _ => o.push(c),
        }
    }
    o
}

/// A decimal given by its sign, mantissa and scale, as the shortest text.
fn decimal_text(neg: bool, mantissa: u128, scale: u32) -> String {
    let digits = mantissa.to_string();
    let s = scale as usize;
    let padded = if digits.len() <= s { format!("{}{}", "0".repeat(s + 1 - digits.len()), digits) } else { digits };
    let (int, frac) = padded.split_at(padded.len() - s);
    let frac = frac.trim_end_matches('0');
    let body = if frac.is_empty() { int.to_string() } else { format!("{}.{}", int, frac) };
    if neg && mantissa != 0 {
        format!("-{}", body)
    } else {
        body
    }
}

struct Doc {
    names: Vec<String>,
    qnames: Vec<(usize, usize, usize)>,
    out: String,
    stack: Vec<String>,
    open: bool,
}

impl Doc {
    fn close_start(&mut self) {
        if self.open {
            self.out.push('>');
            self.open = false;
        }
    }

    fn name(&self, n: usize) -> Res<&str> {
        self.names.get(n.wrapping_sub(1)).map(|s| s.as_str()).ok_or_else(|| "a name in the XML is not defined".to_string())
    }

    fn qname(&self, q: usize) -> Res<(usize, usize, usize)> {
        self.qnames.get(q.wrapping_sub(1)).copied().ok_or_else(|| "a qualified name in the XML is not defined".to_string())
    }

    fn tag(&self, q: usize) -> Res<String> {
        let (_, pre, nm) = self.qname(q)?;
        Ok(if pre != 0 { format!("{}:{}", self.name(pre)?, self.name(nm)?) } else { self.name(nm)?.to_string() })
    }

    fn text(&mut self, t: &str) {
        self.close_start();
        self.out.push_str(&escape(t, false));
    }
}

/// Turn a stored `xml` value into text.
pub fn to_xml(b: &[u8]) -> Res<String> {
    if b.len() < 4 || b[0] != 0xDF || b[1] != 0xFF {
        return Err("not binary XML".into());
    }
    let version = b[2];
    if version != 1 && version != 2 {
        return Err(format!("binary XML version {} is not known", version));
    }
    let mut i = 3;
    mb32(b, &mut i)?; // code page
    let mut d = Doc { names: vec![], qnames: vec![], out: String::new(), stack: vec![], open: false };
    // typed XML starts with one more block of type information
    if version == 2 && b.get(i) == Some(&0xEA) {
        i += 1;
        let n = mb32(b, &mut i)?;
        i += n;
    }
    while i < b.len() {
        let t = b[i];
        i += 1;
        match t {
            0xF0 => {
                let n = mb32(b, &mut i)?;
                let s = utf16(b, &mut i, n)?;
                d.names.push(s);
            }
            0xEF => {
                let ns = mb32(b, &mut i)?;
                let pre = mb32(b, &mut i)?;
                let nm = mb32(b, &mut i)?;
                d.qnames.push((ns, pre, nm));
            }
            0xF8 => {
                let q = mb32(b, &mut i)?;
                d.close_start();
                let tag = d.tag(q)?;
                d.out.push('<');
                d.out.push_str(&tag);
                d.stack.push(tag);
                d.open = true;
            }
            0xF6 => {
                let q = mb32(b, &mut i)?;
                if b.get(i) != Some(&0x11) {
                    return Err("an attribute value that is not text".into());
                }
                i += 1;
                let n = mb32(b, &mut i)?;
                let val = utf16(b, &mut i, n)?;
                let (_, pre, nm) = d.qname(q)?;
                let attr = if pre != 0 && d.name(pre)? == "xmlns" {
                    if nm == 0 {
                        "xmlns".to_string()
                    } else {
                        format!("xmlns:{}", d.name(nm)?)
                    }
                } else if pre == 0 && nm != 0 && d.name(nm)? == "xmlns" {
                    "xmlns".to_string()
                } else {
                    d.tag(q)?
                };
                d.out.push_str(&format!(" {}=\"{}\"", attr, escape(&val, true)));
            }
            0xF5 => d.close_start(),
            0xF7 => {
                let tag = d.stack.pop().ok_or("an end tag without a start tag")?;
                if d.open {
                    d.out.push_str("/>");
                    d.open = false;
                } else {
                    d.out.push_str(&format!("</{}>", tag));
                }
            }
            0x11 => {
                let n = mb32(b, &mut i)?;
                let s = utf16(b, &mut i, n)?;
                d.text(&s);
            }
            0xEA => {
                let n = mb32(b, &mut i)?;
                let blob = b.get(i..i + n).ok_or("the XML ends inside type information")?;
                i += n;
                // five bytes before a value: 0, the type, 0, 0 or 1, the storage type
                if n == 5 {
                    typed_value(b, &mut i, blob[1], blob[4], &mut d)?;
                }
            }
            other => return Err(format!("the token {:#x} at offset {} is not known", other, i - 1)),
        }
    }
    if !d.stack.is_empty() {
        return Err("the XML ends with an open element".into());
    }
    Ok(d.out)
}

/// The value after a type block, by the storage type.
fn typed_value(b: &[u8], i: &mut usize, ty: u8, storage: u8, d: &mut Doc) -> Res<()> {
    match storage {
        // strings and years: ordinary text follows
        0x0F | 0x19 => Ok(()),
        // decimal and the integer types derived from it
        0x13 => {
            let hdr = b.get(*i..*i + 4).ok_or("the XML ends inside a decimal")?;
            let (prec, scale) = (hdr[2], hdr[3] as u32);
            *i += 4;
            let size = match prec {
                0..=9 => 4,
                10..=19 => 8,
                20..=28 => 12,
                _ => 16,
            };
            let data = b.get(*i..*i + 1 + size).ok_or("the XML ends inside a decimal")?;
            *i += 1 + size;
            let mut m = [0u8; 16];
            m[..size].copy_from_slice(&data[1..]);
            let _ = ty;
            let s = decimal_text(data[0] == 0, u128::from_le_bytes(m), scale);
            d.text(&s);
            Ok(())
        }
        // a real
        0x11 => {
            let data = b.get(*i..*i + 5).ok_or("the XML ends inside a float")?;
            if data[0] != 0x03 {
                return Err("a float of a form that is not known".into());
            }
            *i += 5;
            let f = f32::from_le_bytes([data[1], data[2], data[3], data[4]]);
            d.text(&format!("{}", f));
            Ok(())
        }
        // a date: a datetimeoffset (time, date, offset in minutes) of scale 7
        0x17 => {
            let data = b.get(*i..*i + 12).ok_or("the XML ends inside a date")?;
            if data[0] != 0x7C || data[1] != 7 {
                return Err("a date of a form that is not known".into());
            }
            *i += 12;
            let days = u32::from_le_bytes([data[7], data[8], data[9], 0]) as i32;
            let date = NaiveDate::from_num_days_from_ce_opt(days + 1).ok_or("a date out of range")?;
            let off = i16::from_le_bytes([data[10], data[11]]);
            let tz = if off == 0 { "Z".to_string() } else { format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60) };
            d.text(&format!("{}{}", date.format("%Y-%m-%d"), tz));
            Ok(())
        }
        other => Err(format!("the XML type {:#x} is not known", other)),
    }
}
