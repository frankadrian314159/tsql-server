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
}

const OPS2: [&str; 14] = ["<>", "<=", ">=", "!=", "!<", "!>", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^="];
const OPS1: [&str; 17] = ["+", "-", "*", "/", "%", "=", "<", ">", "(", ")", ",", ".", ";", "&", "|", "^", "~"];

fn is_id_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '#'
}
fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '@' || c == '#' || c == '$'
}

pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
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
            toks.push(Token { tok, pos: s, end: e, up });
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
    toks.push(Token { tok: Tok::Eof, pos: sql.len(), end: sql.len(), up: String::new() });
    Ok(toks)
}

/// Split a script on GO separators. Returns (batch text, repeat count).
pub fn split_batches(script: &str) -> Vec<(String, usize)> {
    let mut batches = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut in_block = 0i32;
    let mut in_str = false;
    for line in script.lines() {
        if in_block == 0 && !in_str {
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
