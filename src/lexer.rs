//! T-SQL tokenizer and GO batch splitter.
use crate::error::{Result, SqlError};
use crate::decimal::Dec;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Id(String),  // bare identifier / keyword (original case)
    QId(String), // [bracketed] or "quoted"
    Var(String), // @x or @@x
    Int(i64),
    Dec(Dec),
    Float(f64),
    Money(Dec),
    Str(String),
    Hex(Vec<u8>),
    Op(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub pos: usize,
    pub end: usize,
    /// uppercased text for Id tokens
    pub up: String,
    /// a string written N'...' (a Unicode literal)
    pub unicode: bool,
}

#[cfg(test)]
const OPS2: [&str; 14] = ["<>", "<=", ">=", "!=", "!<", "!>", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^="];
#[cfg(test)]
const OPS1: [&str; 17] = ["+", "-", "*", "/", "%", "=", "<", ">", "(", ")", ",", ".", ";", "&", "|", "^", "~"];

fn is_id_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '#'
}
fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '@' || c == '#' || c == '$'
}

/// The character at byte `i` (which is at a character boundary) and its length in bytes.
#[inline]
fn char_at(sql: &str, i: usize) -> (char, usize) {
    let c = sql[i..].chars().next().unwrap_or('\0');
    (c, c.len_utf8().max(1))
}

/// The end of the run of identifier characters that starts at byte `j`.
#[inline]
fn id_end(sql: &str, mut j: usize) -> usize {
    let b = sql.as_bytes();
    while j < b.len() {
        let c = b[j];
        if c < 128 {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'#' || c == b'$' {
                j += 1;
            } else {
                break;
            }
        } else {
            let (ch, l) = char_at(sql, j);
            if is_id_char(ch) {
                j += l;
            } else {
                break;
            }
        }
    }
    j
}

fn op2(a: u8, b: u8) -> Option<&'static str> {
    Some(match (a, b) {
        (b'<', b'>') => "<>",
        (b'<', b'=') => "<=",
        (b'>', b'=') => ">=",
        (b'!', b'=') => "!=",
        (b'!', b'<') => "!<",
        (b'!', b'>') => "!>",
        (b'+', b'=') => "+=",
        (b'-', b'=') => "-=",
        (b'*', b'=') => "*=",
        (b'/', b'=') => "/=",
        (b'%', b'=') => "%=",
        (b'&', b'=') => "&=",
        (b'|', b'=') => "|=",
        (b'^', b'=') => "^=",
        _ => return None,
    })
}

fn op1(a: u8) -> Option<&'static str> {
    Some(match a {
        b'+' => "+",
        b'-' => "-",
        b'*' => "*",
        b'/' => "/",
        b'%' => "%",
        b'=' => "=",
        b'<' => "<",
        b'>' => ">",
        b'(' => "(",
        b')' => ")",
        b',' => ",",
        b'.' => ".",
        b';' => ";",
        b'&' => "&",
        b'|' => "|",
        b'^' => "^",
        b'~' => "~",
        b':' => ":",
        _ => return None,
    })
}

/// Split T-SQL text into tokens. Works on the bytes of the text: every character that has a meaning to the lexer is
/// ASCII, so only identifiers and whitespace need to look at multi-byte characters.
pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut toks: Vec<Token> = Vec::with_capacity(n / 4 + 4);
    let mut i = 0;
    let push = |toks: &mut Vec<Token>, tok: Tok, s: usize, e: usize| {
        let up = match &tok {
            Tok::Id(x) if x.is_ascii() => x.to_ascii_uppercase(),
            Tok::Id(x) => x.to_uppercase(),
            _ => String::new(),
        };
        toks.push(Token { tok, pos: s, end: e, up, unicode: false });
    };
    while i < n {
        let c = b[i];
        if c < 128 {
            if c == b' ' || (9..=13).contains(&c) {
                i += 1;
                continue;
            }
        } else {
            let (ch, l) = char_at(sql, i);
            if ch.is_whitespace() {
                i += l;
                continue;
            }
        }
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            let mut depth = 1;
            i += 2;
            while i < n && depth > 0 {
                if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && i + 1 < n && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                return Err(SqlError::new(113, "Missing end comment mark '*/'.").abort());
            }
            continue;
        }
        let pos = i;
        if c == b'\'' || ((c == b'N' || c == b'n') && i + 1 < n && b[i + 1] == b'\'') {
            let mut j = if c == b'\'' { i + 1 } else { i + 2 };
            let mut start = j;
            let mut buf = String::new();
            loop {
                if j >= n {
                    buf.push_str(&sql[start..n]);
                    return Err(SqlError::new(105, format!("Unclosed quotation mark after the character string '{}'.", buf.chars().take(20).collect::<String>())).abort());
                }
                if b[j] == b'\'' {
                    buf.push_str(&sql[start..j]);
                    if j + 1 < n && b[j + 1] == b'\'' {
                        buf.push('\'');
                        j += 2;
                        start = j;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            push(&mut toks, Tok::Str(buf), pos, j + 1);
            if c != b'\'' {
                toks.last_mut().unwrap().unicode = true;
            }
            i = j + 1;
            continue;
        }
        if c == b'[' || c == b'"' {
            let close = if c == b'[' { b']' } else { b'"' };
            let mut j = i + 1;
            let mut start = j;
            let mut buf = String::new();
            loop {
                if j >= n {
                    return Err(SqlError::new(105, "Unclosed identifier quote.").abort());
                }
                if b[j] == close {
                    buf.push_str(&sql[start..j]);
                    if j + 1 < n && b[j + 1] == close {
                        buf.push(close as char);
                        j += 2;
                        start = j;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            push(&mut toks, Tok::QId(buf), pos, j + 1);
            i = j + 1;
            continue;
        }
        if c == b'@' {
            let mut j = i + 1;
            if j < n && b[j] == b'@' {
                j += 1;
            }
            j = id_end(sql, j);
            push(&mut toks, Tok::Var(sql[pos..j].to_string()), pos, j);
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && i + 1 < n && b[i + 1].is_ascii_digit()) {
            if c == b'0' && i + 1 < n && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                let mut j = i + 2;
                while j < n && b[j].is_ascii_hexdigit() {
                    j += 1;
                }
                let h = &sql[i + 2..j];
                let h = if h.len() % 2 == 1 { format!("0{}", h) } else { h.to_string() };
                let bytes: Vec<u8> = (0..h.len()).step_by(2).map(|k| u8::from_str_radix(&h[k..k + 2], 16).unwrap()).collect();
                push(&mut toks, Tok::Hex(bytes), pos, j);
                i = j;
                continue;
            }
            let mut j = i;
            while j < n && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut is_dec = false;
            if j < n && b[j] == b'.' {
                is_dec = true;
                j += 1;
                while j < n && b[j].is_ascii_digit() {
                    j += 1;
                }
            }
            let mut is_float = false;
            if j < n && (b[j] == b'e' || b[j] == b'E') {
                let mut k = j + 1;
                if k < n && (b[k] == b'+' || b[k] == b'-') {
                    k += 1;
                }
                if k < n && b[k].is_ascii_digit() {
                    while k < n && b[k].is_ascii_digit() {
                        k += 1;
                    }
                    j = k;
                    is_float = true;
                }
            }
            let text = &sql[pos..j];
            let tok = if is_float {
                Tok::Float(text.parse().unwrap())
            } else if is_dec {
                Tok::Dec(Dec::parse(text).ok_or_else(|| SqlError::new(105, "Bad numeric literal").abort())?)
            } else {
                match text.parse::<i64>() {
                    Ok(v) => Tok::Int(v),
                    Err(_) => Tok::Dec(Dec::parse(text).ok_or_else(|| SqlError::new(1007, format!("The number '{}' is out of the range for numeric representation (maximum precision 38).", text)).abort())?),
                }
            };
            push(&mut toks, tok, pos, j);
            i = j;
            continue;
        }
        if c == b'$' && i + 1 < n && (b[i + 1].is_ascii_digit() || b[i + 1] == b'.') {
            let mut j = i + 1;
            while j < n && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            let d = Dec::parse(&sql[i + 1..j]).ok_or_else(|| SqlError::new(105, "Bad money literal").abort())?;
            push(&mut toks, Tok::Money(d), pos, j);
            i = j;
            continue;
        }
        let starts_id = if c < 128 { c.is_ascii_alphabetic() || c == b'_' || c == b'#' || c == b'$' } else { is_id_start(char_at(sql, i).0) };
        if starts_id {
            let j = id_end(sql, i);
            push(&mut toks, Tok::Id(sql[pos..j].to_string()), pos, j);
            i = j;
            continue;
        }
        if c < 128 {
            if i + 1 < n && b[i + 1] < 128 {
                if let Some(op) = op2(c, b[i + 1]) {
                    push(&mut toks, Tok::Op(op), pos, i + 2);
                    i += 2;
                    continue;
                }
            }
            if let Some(op) = op1(c) {
                push(&mut toks, Tok::Op(op), pos, i + 1);
                i += 1;
                continue;
            }
        }
        return Err(SqlError::syntax(&char_at(sql, i).0.to_string()));
    }
    toks.push(Token { tok: Tok::Eof, pos: sql.len(), end: sql.len(), up: String::new(), unicode: false });
    Ok(toks)
}

/// Split a script on GO separators. Returns (batch text, repeat count).
pub fn split_batches(script: &str) -> Vec<(String, usize)> {
    let mut batches = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut in_block = 0i32;
    let mut in_str = false;
    // inside a [bracketed] or "quoted" identifier, where an apostrophe or a comment marker is just a character
    let mut in_ident: Option<char> = None;
    for line in script.lines() {
        if in_block == 0 && !in_str && in_ident.is_none() {
            let t = line.trim();
            let tl = t.to_ascii_uppercase();
            if tl == "GO" || (tl.starts_with("GO ") && tl[3..].trim().chars().all(|c| c.is_ascii_digit()) && !tl[3..].trim().is_empty()) || (tl.starts_with("GO") && tl[2..].trim_start().starts_with("--")) {
                let count = tl[2..].trim().parse::<usize>().unwrap_or(1);
                batches.push((cur.join("\n"), count));
                cur.clear();
                continue;
            }
        }
        cur.push(line);
        let b: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < b.len() {
            if in_block > 0 {
                if b[i] == '*' && i + 1 < b.len() && b[i + 1] == '/' {
                    in_block -= 1;
                    i += 2;
                } else if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
                    in_block += 1;
                    i += 2;
                } else {
                    i += 1;
                }
            } else if in_str {
                if b[i] == '\'' {
                    in_str = false;
                }
                i += 1;
            } else if let Some(close) = in_ident {
                if b[i] == close {
                    if i + 1 < b.len() && b[i + 1] == close {
                        i += 2; // ]] or "" is a literal closing character
                        continue;
                    }
                    in_ident = None;
                }
                i += 1;
            } else if b[i] == '[' || b[i] == '"' {
                in_ident = Some(if b[i] == '[' { ']' } else { '"' });
                i += 1;
            } else if b[i] == '-' && i + 1 < b.len() && b[i + 1] == '-' {
                break;
            } else if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
                in_block += 1;
                i += 2;
            } else if b[i] == '\'' {
                in_str = true;
                i += 1;
            } else {
                i += 1;
            }
        }
    }
    batches.push((cur.join("\n"), 1));
    batches.into_iter().filter(|(s, _)| !s.trim().is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tokenizer as it was before it worked on bytes; the new one must give the same tokens (or the same error).
    fn tokenize_reference(sql: &str) -> Result<Vec<Token>> {
        let ch: Vec<(usize, char)> = sql.char_indices().collect();
        let n = ch.len();
        let byte = |i: usize| if i < n { ch[i].0 } else { sql.len() };
        let mut toks = Vec::new();
        let mut i = 0;
        while i < n {
            let (pos, c) = ch[i];
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            if c == '-' && i + 1 < n && ch[i + 1].1 == '-' {
                while i < n && ch[i].1 != '\n' {
                    i += 1;
                }
                continue;
            }
            if c == '/' && i + 1 < n && ch[i + 1].1 == '*' {
                let mut depth = 1;
                i += 2;
                while i < n && depth > 0 {
                    if ch[i].1 == '/' && i + 1 < n && ch[i + 1].1 == '*' {
                        depth += 1;
                        i += 2;
                    } else if ch[i].1 == '*' && i + 1 < n && ch[i + 1].1 == '/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth > 0 {
                    return Err(SqlError::new(113, "Missing end comment mark '*/'.").abort());
                }
                continue;
            }
            let push = |toks: &mut Vec<Token>, tok: Tok, s: usize, e: usize| {
                let up = if let Tok::Id(ref x) = tok { x.to_uppercase() } else { String::new() };
                toks.push(Token { tok, pos: s, end: e, up, unicode: false });
            };
            if c == '\'' || ((c == 'N' || c == 'n') && i + 1 < n && ch[i + 1].1 == '\'') {
                let mut j = if c == '\'' { i + 1 } else { i + 2 };
                let mut buf = String::new();
                loop {
                    if j >= n {
                        return Err(SqlError::new(105, format!("Unclosed quotation mark after the character string '{}'.", buf.chars().take(20).collect::<String>())).abort());
                    }
                    if ch[j].1 == '\'' {
                        if j + 1 < n && ch[j + 1].1 == '\'' {
                            buf.push('\'');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    buf.push(ch[j].1);
                    j += 1;
                }
                push(&mut toks, Tok::Str(buf), pos, byte(j + 1));
                if c != '\'' {
                    toks.last_mut().unwrap().unicode = true;
                }
                i = j + 1;
                continue;
            }
            if c == '[' || c == '"' {
                let close = if c == '[' { ']' } else { '"' };
                let mut j = i + 1;
                let mut buf = String::new();
                loop {
                    if j >= n {
                        return Err(SqlError::new(105, "Unclosed identifier quote.").abort());
                    }
                    if ch[j].1 == close {
                        if j + 1 < n && ch[j + 1].1 == close {
                            buf.push(close);
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    buf.push(ch[j].1);
                    j += 1;
                }
                push(&mut toks, Tok::QId(buf), pos, byte(j + 1));
                i = j + 1;
                continue;
            }
            if c == '@' {
                let mut j = i + 1;
                if j < n && ch[j].1 == '@' {
                    j += 1;
                }
                while j < n && is_id_char(ch[j].1) {
                    j += 1;
                }
                push(&mut toks, Tok::Var(sql[pos..byte(j)].to_string()), pos, byte(j));
                i = j;
                continue;
            }
            if c.is_ascii_digit() || (c == '.' && i + 1 < n && ch[i + 1].1.is_ascii_digit()) {
                if c == '0' && i + 1 < n && (ch[i + 1].1 == 'x' || ch[i + 1].1 == 'X') {
                    let mut j = i + 2;
                    while j < n && ch[j].1.is_ascii_hexdigit() {
                        j += 1;
                    }
                    let h = &sql[byte(i + 2)..byte(j)];
                    let h = if h.len() % 2 == 1 { format!("0{}", h) } else { h.to_string() };
                    let bytes: Vec<u8> = (0..h.len()).step_by(2).map(|k| u8::from_str_radix(&h[k..k + 2], 16).unwrap()).collect();
                    push(&mut toks, Tok::Hex(bytes), pos, byte(j));
                    i = j;
                    continue;
                }
                let mut j = i;
                while j < n && ch[j].1.is_ascii_digit() {
                    j += 1;
                }
                let mut is_dec = false;
                if j < n && ch[j].1 == '.' {
                    is_dec = true;
                    j += 1;
                    while j < n && ch[j].1.is_ascii_digit() {
                        j += 1;
                    }
                }
                let mut is_float = false;
                if j < n && (ch[j].1 == 'e' || ch[j].1 == 'E') {
                    let mut k = j + 1;
                    if k < n && (ch[k].1 == '+' || ch[k].1 == '-') {
                        k += 1;
                    }
                    if k < n && ch[k].1.is_ascii_digit() {
                        while k < n && ch[k].1.is_ascii_digit() {
                            k += 1;
                        }
                        j = k;
                        is_float = true;
                    }
                }
                let text = &sql[pos..byte(j)];
                let tok = if is_float {
                    Tok::Float(text.parse().unwrap())
                } else if is_dec {
                    Tok::Dec(Dec::parse(text).ok_or_else(|| SqlError::new(105, "Bad numeric literal").abort())?)
                } else {
                    match text.parse::<i64>() {
                        Ok(v) => Tok::Int(v),
                        Err(_) => Tok::Dec(Dec::parse(text).ok_or_else(|| SqlError::new(1007, format!("The number '{}' is out of the range for numeric representation (maximum precision 38).", text)).abort())?),
                    }
                };
                push(&mut toks, tok, pos, byte(j));
                i = j;
                continue;
            }
            if c == '$' && i + 1 < n && (ch[i + 1].1.is_ascii_digit() || ch[i + 1].1 == '.') {
                let mut j = i + 1;
                while j < n && (ch[j].1.is_ascii_digit() || ch[j].1 == '.') {
                    j += 1;
                }
                let d = Dec::parse(&sql[byte(i + 1)..byte(j)]).ok_or_else(|| SqlError::new(105, "Bad money literal").abort())?;
                push(&mut toks, Tok::Money(d), pos, byte(j));
                i = j;
                continue;
            }
            if is_id_start(c) || c == '$' {
                let mut j = i + 1;
                while j < n && is_id_char(ch[j].1) {
                    j += 1;
                }
                push(&mut toks, Tok::Id(sql[pos..byte(j)].to_string()), pos, byte(j));
                i = j;
                continue;
            }
            if i + 1 < n {
                let two = &sql[pos..byte(i + 2)];
                if let Some(op) = OPS2.iter().find(|o| **o == two) {
                    push(&mut toks, Tok::Op(op), pos, byte(i + 2));
                    i += 2;
                    continue;
                }
            }
            let one = &sql[pos..byte(i + 1)];
            if let Some(op) = OPS1.iter().find(|o| **o == one) {
                push(&mut toks, Tok::Op(op), pos, byte(i + 1));
                i += 1;
                continue;
            }
            if c == ':' {
                push(&mut toks, Tok::Op(":"), pos, byte(i + 1));
                i += 1;
                continue;
            }
            return Err(SqlError::syntax(&c.to_string()));
        }
        toks.push(Token { tok: Tok::Eof, pos: sql.len(), end: sql.len(), up: String::new(), unicode: false });
        Ok(toks)
    }

    fn show(r: Result<Vec<Token>>) -> String {
        match r {
            Ok(t) => format!("{:?}", t),
            Err(e) => format!("error {} {}", e.number, e.message),
        }
    }

    #[test]
    fn the_byte_tokenizer_agrees_with_the_reference() {
        let fixed = [
            "SELECT id, grp, name FROM t WHERE id = 12345 AND x <> 'it''s' -- tail",
            "INSERT t VALUES (1, 2.5, 1e3, 0x1F, 0xABC, N'caf\u{e9}', $12.50, @v, @@rowcount, [a b], \"q\\\"x\")",
            "a<=b>=c!=d!<e!>f+=g-=h*=i/=j%=k&=l|=m^=n :: ~ ; . .5 5. 5.e",
            "/* nested /* comment */ still */ SELECT 1 /* open",
            "SELECT 'unclosed", "SELECT [unclosed", "SELECT `", "SELECT \u{a0}1\u{85}2", "caf\u{e9} \u{4f60}\u{597d} _x #t ##g $a", "99999999999999999999 1.5e+10 1E-2",
            "x\u{1}y", "N'abc'", "n'abc' N 'abc'", "[a]]b] \"a\"\"b\"", "0x 0xG 0", "", "   ", "-- only a comment", "a--b\nc", "1..2", "$", "$.", "@", "@@",
        ];
        for s in fixed {
            assert_eq!(show(tokenize(s)), show(tokenize_reference(s)), "{:?}", s);
        }
        // random text from the characters that matter
        let alphabet: Vec<char> = "abcXYZnN_09 .,;()[]\"'@#$%^&*-+/=<>!|~:\n\te E x\u{e9}\u{df}\u{4f60}\u{a0}\u{85}\u{1f600}\u{1}".chars().collect();
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..60000 {
            let len = (rnd() % 40) as usize;
            let s: String = (0..len).map(|_| alphabet[(rnd() % alphabet.len() as u64) as usize]).collect();
            assert_eq!(show(tokenize(&s)), show(tokenize_reference(&s)), "{:?}", s);
        }
    }
}
