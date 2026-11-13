//! SQL Server's binary XML (the stored form of the `xml` type), turned back into text ([`to_xml`]) and made from text
//! ([`from_xml`], untyped XML only).
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
//! | `F3 n chars` | a comment |
//! | `F4 name n chars` | a processing instruction |
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
                let val = if b.get(i) == Some(&0x11) {
                    i += 1;
                    let n = mb32(b, &mut i)?;
                    utf16(b, &mut i, n)?
                } else {
                    typed_value(b, &mut i)?
                };
                let (_, pre, nm) = d.qname(q)?;
                let attr = if pre != 0 && nm == 0 {
                    // a namespace declaration whose whole name ("xmlns" or "xmlns:p") is one defined name
                    d.name(pre)?.to_string()
                } else if pre != 0 && d.name(pre)? == "xmlns" {
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
            // a comment: its length in characters, then the text
            0xF3 => {
                let n = mb32(b, &mut i)?;
                let s = utf16(b, &mut i, n)?;
                d.close_start();
                d.out.push_str(&format!("<!--{}-->", s));
            }
            // a processing instruction: the number of its name, the length of its text, the text
            0xF4 => {
                let nm = mb32(b, &mut i)?;
                let n = mb32(b, &mut i)?;
                let s = utf16(b, &mut i, n)?;
                d.close_start();
                let name = d.name(nm)?.to_string();
                d.out.push_str(&format!("<?{} {}?>", name, s));
            }
            // type information, which describes the element or attribute that is declared next; skipped
            0xEA => {
                let n = mb32(b, &mut i)?;
                b.get(i..i + n).ok_or("the XML ends inside type information")?;
                i += n;
            }
            // typed values lead with a byte of their own
            0x87 | 0x7C | 0x03 => {
                i -= 1;
                let v = typed_value(b, &mut i)?;
                d.text(&v);
            }
            other => return Err(format!("the token {:#x} at offset {} is not known", other, i - 1)),
        }
    }
    if !d.stack.is_empty() {
        return Err("the XML ends with an open element".into());
    }
    Ok(d.out)
}

/// A typed value, by its leading byte: `87` a decimal (and the integer types derived from it: `87 13`, precision, scale, a
/// sign byte and the mantissa), `03` a real, `7C` a date (a `datetimeoffset`). Other types are not known.
fn typed_value(b: &[u8], i: &mut usize) -> Res<String> {
    match b.get(*i) {
        Some(0x87) => {
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
            Ok(decimal_text(data[0] == 0, u128::from_le_bytes(m), scale))
        }
        Some(0x03) => {
            let data = b.get(*i..*i + 5).ok_or("the XML ends inside a float")?;
            *i += 5;
            Ok(format!("{}", f32::from_le_bytes([data[1], data[2], data[3], data[4]])))
        }
        Some(0x7C) => {
            let data = b.get(*i..*i + 12).ok_or("the XML ends inside a date")?;
            if data[1] != 7 {
                return Err("a date of a form that is not known".into());
            }
            *i += 12;
            let days = u32::from_le_bytes([data[7], data[8], data[9], 0]) as i32;
            let date = NaiveDate::from_num_days_from_ce_opt(days + 1).ok_or("a date out of range")?;
            let off = i16::from_le_bytes([data[10], data[11]]);
            let tz = if off == 0 { "Z".to_string() } else { format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60) };
            Ok(format!("{}{}", date.format("%Y-%m-%d"), tz))
        }
        _ => Err("a value of a type that is not known".into()),
    }
}

// ----------------------------------------------------------------------------------------------------------- writing

fn put_mb32(out: &mut Vec<u8>, mut v: usize) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// The names, qualified names and tokens of the binary form being built.
struct Enc {
    out: Vec<u8>,
    names: std::collections::HashMap<String, usize>,
    qnames: std::collections::HashMap<(usize, usize), usize>,
}

impl Enc {
    fn name(&mut self, s: &str) -> usize {
        if s.is_empty() {
            return 0;
        }
        if let Some(&n) = self.names.get(s) {
            return n;
        }
        let n = self.names.len() + 1;
        self.names.insert(s.to_string(), n);
        self.out.push(0xF0);
        put_mb32(&mut self.out, s.encode_utf16().count());
        self.out.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
        n
    }

    /// A qualified name from its prefix and local part; the namespace is not recorded.
    fn qname(&mut self, prefix: &str, local: &str) -> usize {
        let (p, l) = (self.name(prefix), self.name(local));
        if let Some(&q) = self.qnames.get(&(p, l)) {
            return q;
        }
        let q = self.qnames.len() + 1;
        self.qnames.insert((p, l), q);
        self.out.push(0xEF);
        put_mb32(&mut self.out, 0);
        put_mb32(&mut self.out, p);
        put_mb32(&mut self.out, l);
        q
    }

    fn text(&mut self, token: u8, s: &str) {
        self.out.push(token);
        put_mb32(&mut self.out, s.encode_utf16().count());
        self.out.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
    }
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.rest().starts_with(p) {
            self.i += p.len();
            true
        } else {
            false
        }
    }

    fn until(&mut self, end: &str) -> Res<&'a str> {
        let at = self.rest().find(end).ok_or_else(|| format!("the XML has no '{}' where one is expected", end))?;
        let t = &self.s[self.i..self.i + at];
        self.i += at + end.len();
        Ok(t)
    }

    fn name(&mut self) -> Res<&'a str> {
        let n = self.rest().find(|c: char| c.is_whitespace() || matches!(c, '=' | '>' | '/' | '<')).unwrap_or(self.rest().len());
        if n == 0 {
            return Err("a name is missing in the XML".into());
        }
        let r = &self.s[self.i..self.i + n];
        self.i += n;
        Ok(r)
    }

    fn ws(&mut self) {
        let n = self.rest().len() - self.rest().trim_start().len();
        self.i += n;
    }
}

/// Entities and character references in text.
fn unescape(s: &str) -> Res<String> {
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut o = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        o.push_str(&rest[..at]);
        let end = rest[at..].find(';').ok_or("an entity in the XML is not closed")?;
        let ent = &rest[at + 1..at + end];
        match ent {
            "amp" => o.push('&'),
            "lt" => o.push('<'),
            "gt" => o.push('>'),
            "quot" => o.push('"'),
            "apos" => o.push('\''),
            _ => {
                let code = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) { u32::from_str_radix(h, 16).ok() } else { ent.strip_prefix('#').and_then(|d| d.parse().ok()) };
                o.push(code.and_then(char::from_u32).ok_or_else(|| format!("the entity &{}; is not known", ent))?);
            }
        }
        rest = &rest[at + end + 1..];
    }
    o.push_str(rest);
    Ok(o)
}

fn split_qname(n: &str) -> (&str, &str) {
    n.split_once(':').unwrap_or(("", n))
}

fn element(p: &mut Parser, e: &mut Enc) -> Res<()> {
    // after '<'
    let tag = p.name()?;
    let (pre, local) = split_qname(tag);
    let q = e.qname(pre, local);
    e.out.push(0xF8);
    put_mb32(&mut e.out, q);
    loop {
        p.ws();
        if p.eat("/>") {
            e.out.push(0xF7);
            return Ok(());
        }
        if p.eat(">") {
            break;
        }
        let an = p.name()?;
        p.ws();
        if !p.eat("=") {
            return Err(format!("the attribute '{}' has no value", an));
        }
        p.ws();
        let quote = if p.eat("\"") { "\"" } else if p.eat("'") { "'" } else { return Err("an attribute value is not quoted".into()) };
        let val = unescape(p.until(quote)?)?;
        let (apre, alocal) = split_qname(an);
        // a namespace declaration is `xmlns` (the prefix) with the declared prefix as the local name
        let q = if an == "xmlns" { e.qname("xmlns", "") } else if apre == "xmlns" { e.qname("xmlns", alocal) } else { e.qname(apre, alocal) };
        e.out.push(0xF6);
        put_mb32(&mut e.out, q);
        e.text(0x11, &val);
    }
    e.out.push(0xF5);
    content(p, e, Some(tag))?;
    e.out.push(0xF7);
    Ok(())
}

/// Children up to the end tag of `parent` (or the end of the text, at the top level).
fn content(p: &mut Parser, e: &mut Enc, parent: Option<&str>) -> Res<()> {
    loop {
        if p.rest().is_empty() {
            return match parent {
                Some(t) => Err(format!("the element '{}' is not closed", t)),
                None => Ok(()),
            };
        }
        if p.eat("</") {
            let t = p.until(">")?.trim();
            return match parent {
                Some(open) if open == t => Ok(()),
                _ => Err(format!("the end tag '{}' does not match", t)),
            };
        }
        if p.eat("<!--") {
            let c = p.until("-->")?;
            e.text(0xF3, c);
        } else if p.eat("<![CDATA[") {
            let t = p.until("]]>")?;
            e.text(0x11, t);
        } else if p.eat("<?") {
            let body = p.until("?>")?;
            let (name, text) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
            if !name.eq_ignore_ascii_case("xml") || parent.is_some() {
                let n = e.name(name);
                e.out.push(0xF4);
                put_mb32(&mut e.out, n);
                put_mb32(&mut e.out, text.trim_start().encode_utf16().count());
                e.out.extend(text.trim_start().encode_utf16().flat_map(|u| u.to_le_bytes()));
            }
        } else if p.eat("<!") {
            p.until(">")?; // a DOCTYPE
        } else if p.eat("<") {
            element(p, e)?;
        } else {
            let end = p.rest().find('<').unwrap_or(p.rest().len());
            let t = unescape(&p.rest()[..end])?;
            p.i += end;
            e.text(0x11, &t);
        }
    }
}

/// The binary form of an untyped XML document or fragment: the inverse of [`to_xml`].
pub fn from_xml(text: &str) -> Res<Vec<u8>> {
    let mut e = Enc { out: vec![0xDF, 0xFF, 1, 0xB0, 0x04], names: Default::default(), qnames: Default::default() };
    let mut p = Parser { s: text, i: 0 };
    content(&mut p, &mut e, None)?;
    Ok(e.out)
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[test]
    fn xml_survives_the_binary_form() {
        for x in [
            "<a/>",
            "<a></a>",
            "<root xmlns=\"urn:x\" xmlns:p=\"urn:p\"><p:item id=\"1\" name=\"a &amp; b\">text &lt;here&gt;</p:item><!--note--><?pi data?><empty/></root>",
            "<a>one<b>two</b>three</a>",
            "plain text",
            "<a x=\"&quot;q&quot;\">caf\u{e9} \u{2603}</a>",
            "<a>line\r\nbreak</a>",
        ] {
            let bin = from_xml(x).unwrap();
            let back = to_xml(&bin).unwrap();
            // the decoder writes a carriage return as a character reference
            assert_eq!(back, x.replace('\r', "&#xD;"), "{}", x);
        }
        assert!(from_xml("<a><b></a>").is_err());
    }
}
