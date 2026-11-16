//! A raw TDS client for the tests (copied from the single-node suite's tds_raw.rs): RPCs by id or name, with typed parameters.
#![allow(dead_code)]
use std::io::{Read, Write};
use std::net::TcpStream;
use tsql_server::tds::wire::*;
use tsql_server::tds::{Config, Listener};
use tsql_server::Value;

pub struct Raw {
    pub s: TcpStream,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum Tok {
    Done { status: u16, count: u64, kind: u8 },
    Error { number: i32, text: String },
    Info { number: i32 },
    Env { kind: u8 },
    Status(i32),
    Ret { name: String, val: Value },
    Meta(Vec<String>),
    Row(Vec<Value>),
    LoginAck,
}

pub fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![kind, 1];
    p.extend_from_slice(&((payload.len() + 8) as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 1, 0]);
    p.extend_from_slice(payload);
    p
}

pub fn headers() -> Vec<u8> {
    let mut h = vec![];
    h.extend_from_slice(&22u32.to_le_bytes());
    h.extend_from_slice(&18u32.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&0u64.to_le_bytes());
    h.extend_from_slice(&1u32.to_le_bytes());
    h
}

pub fn utf16(s: &str) -> Vec<u8> {
    let mut v = vec![];
    put_utf16(&mut v, s);
    v
}

pub fn login7(user: &str, pw: &str) -> Vec<u8> {
    let fields = [utf16("host"), utf16(user), utf16(pw).into_iter().map(|b| b.rotate_left(4) ^ 0xA5).collect(), utf16("raw"), utf16("srv"), vec![], utf16("rawtest"), vec![], vec![]];
    let mut body = vec![0u8; 94];
    let mut data = vec![];
    for (i, f) in fields.iter().enumerate() {
        let off = 94 + data.len();
        let slot = 36 + i * 4;
        body[slot..slot + 2].copy_from_slice(&(off as u16).to_le_bytes());
        body[slot + 2..slot + 4].copy_from_slice(&((f.len() / 2) as u16).to_le_bytes());
        data.extend_from_slice(f);
    }
    body.extend(data);
    let total = body.len() as u32;
    body[0..4].copy_from_slice(&total.to_le_bytes());
    body[4..8].copy_from_slice(&0x7400_0004u32.to_le_bytes());
    body[8..12].copy_from_slice(&4096u32.to_le_bytes());
    body
}

impl Raw {
    pub fn connect(l: &Listener, user: &str, pw: &str) -> (Raw, Vec<Tok>) {
        let mut s = TcpStream::connect(l.local_addr()).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
        // prelogin: VERSION and ENCRYPTION options
        let mut pre = vec![0, 0, 11, 0, 6, 1, 0, 17, 0, 1, 0xFF];
        pre.extend_from_slice(&[9, 0, 0, 0, 0, 0, 0]);
        s.write_all(&packet(18, &pre)).unwrap();
        let mut r = Raw { s };
        let (kind, _) = r.read_message();
        assert_eq!(kind, 4);
        r.s.write_all(&packet(16, &login7(user, pw))).unwrap();
        let toks = r.tokens();
        (r, toks)
    }

    pub fn read_message(&mut self) -> (u8, Vec<u8>) {
        let mut data = vec![];
        loop {
            let mut h = [0u8; 8];
            self.s.read_exact(&mut h).unwrap();
            let len = u16::from_be_bytes([h[2], h[3]]) as usize;
            let mut body = vec![0u8; len - 8];
            self.s.read_exact(&mut body).unwrap();
            data.extend(body);
            if h[1] & 1 != 0 {
                return (h[0], data);
            }
        }
    }

    pub fn tokens(&mut self) -> Vec<Tok> {
        let (_, data) = self.read_message();
        let mut rd = Rd::new(&data);
        let mut out = vec![];
        let mut cols: Vec<Ti> = vec![];
        while rd.left() > 0 {
            let t = rd.u8().unwrap();
            match t {
                0xFD | 0xFE | 0xFF => {
                    let status = rd.u16().unwrap();
                    rd.u16().unwrap();
                    let count = rd.u64().unwrap();
                    out.push(Tok::Done { status, count, kind: t });
                }
                0xAA | 0xAB => {
                    let len = rd.u16().unwrap() as usize;
                    let body = rd.take(len).unwrap();
                    let mut b = Rd::new(body);
                    let number = b.u32().unwrap() as i32;
                    b.u16().unwrap();
                    let n = b.u16().unwrap() as usize;
                    let text = b.utf16(n).unwrap();
                    out.push(if t == 0xAA { Tok::Error { number, text } } else { Tok::Info { number } });
                }
                0xE3 => {
                    let len = rd.u16().unwrap() as usize;
                    let body = rd.take(len).unwrap();
                    out.push(Tok::Env { kind: body[0] });
                }
                0xAD => {
                    let len = rd.u16().unwrap() as usize;
                    rd.take(len).unwrap();
                    out.push(Tok::LoginAck);
                }
                // TABNAME and COLINFO: the browse information of a cursor
                0xA4 | 0xA5 => {
                    let len = rd.u16().unwrap() as usize;
                    rd.take(len).unwrap();
                }
                0x79 => out.push(Tok::Status(rd.u32().unwrap() as i32)),
                0xAC => {
                    rd.u16().unwrap();
                    let name = rd.bvarchar().unwrap();
                    rd.u8().unwrap();
                    rd.u32().unwrap();
                    rd.u16().unwrap();
                    let ti = read_typeinfo(&mut rd).unwrap();
                    let val = read_value(&mut rd, &ti).unwrap();
                    out.push(Tok::Ret { name, val });
                }
                0x81 => {
                    let n = rd.u16().unwrap();
                    cols.clear();
                    let mut names = vec![];
                    for _ in 0..n {
                        rd.u32().unwrap();
                        rd.u16().unwrap();
                        cols.push(read_typeinfo(&mut rd).unwrap());
                        names.push(rd.bvarchar().unwrap());
                    }
                    out.push(Tok::Meta(names));
                }
                0xD1 => {
                    let row = cols.iter().map(|ti| read_value(&mut rd, ti).unwrap()).collect();
                    out.push(Tok::Row(row));
                }
                other => panic!("unexpected token 0x{:02X}", other),
            }
        }
        out
    }

    pub fn batch(&mut self, sql: &str) -> Vec<Tok> {
        let mut p = headers();
        p.extend(utf16(sql));
        self.s.write_all(&packet(1, &p)).unwrap();
        self.tokens()
    }

    pub fn tm(&mut self, kind: u16, extra: &[u8]) -> Vec<Tok> {
        let mut p = headers();
        p.extend_from_slice(&kind.to_le_bytes());
        p.extend_from_slice(extra);
        self.s.write_all(&packet(14, &p)).unwrap();
        self.tokens()
    }

    /// RPC by name or id; params are (name, output, type info bytes, value bytes).
    pub fn rpc(&mut self, proc: Result<&str, u16>, params: &[(&str, bool, Vec<u8>)]) -> Vec<Tok> {
        let mut p = headers();
        match proc {
            Ok(n) => {
                p.extend_from_slice(&(n.encode_utf16().count() as u16).to_le_bytes());
                p.extend(utf16(n));
            }
            Err(id) => {
                p.extend_from_slice(&0xFFFFu16.to_le_bytes());
                p.extend_from_slice(&id.to_le_bytes());
            }
        }
        p.extend_from_slice(&0u16.to_le_bytes());
        for (name, out, typed) in params {
            p.push(name.encode_utf16().count() as u8);
            p.extend(utf16(name));
            p.push(*out as u8);
            p.extend_from_slice(typed);
        }
        self.s.write_all(&packet(3, &p)).unwrap();
        self.tokens()
    }
}

pub fn nvarchar(s: &str) -> Vec<u8> {
    let mut v = vec![];
    write_typeinfo(&mut v, &Wire::NVarChar(4000));
    write_value(&mut v, &Wire::NVarChar(4000), &Value::str(s));
    v
}
pub fn int(i: i32) -> Vec<u8> {
    let mut v = vec![];
    write_typeinfo(&mut v, &Wire::Int(4));
    write_value(&mut v, &Wire::Int(4), &Value::Int(i));
    v
}
pub fn null_int() -> Vec<u8> {
    let mut v = vec![];
    write_typeinfo(&mut v, &Wire::Int(4));
    write_value(&mut v, &Wire::Int(4), &Value::Null);
    v
}
pub fn cursor_params(scroll: i32) -> Vec<(&'static str, bool, Vec<u8>)> {
    vec![
        ("", true, null_int()),
        ("", false, nvarchar("SELECT n FROM (VALUES (10), (20), (30), (40), (50)) AS t(n) ORDER BY n")),
        ("", true, int(scroll)),
        ("", true, int(4)),
        ("", true, null_int()),
    ]
}

pub fn fetch(r: &mut Raw, cursor: i32, kind: i32, row: i32, n: i32) -> Vec<Tok> {
    r.rpc(Err(7), &[("", false, int(cursor)), ("", false, int(kind)), ("", false, int(row)), ("", false, int(n))])
}

/// The values fetched: the first column is the value, the last rowstat.
pub fn fetched(t: &[Tok]) -> Vec<i32> {
    t.iter().filter_map(|t| if let Tok::Row(v) = t { if let Value::Int(n) = v[0] { Some(n) } else { None } } else { None }).collect()
}

pub fn ret_ints(t: &[Tok]) -> Vec<i32> {
    t.iter().filter_map(|t| if let Tok::Ret { val: Value::Int(v), .. } = t { Some(*v) } else { None }).collect()
}


pub fn bvar(s: &str) -> Vec<u8> {
    let mut v = vec![s.encode_utf16().count() as u8];
    v.extend(utf16(s));
    v
}

/// A table-valued parameter: its type, columns and rows, as TVP_TYPE_INFO and the rows that follow.
pub fn tvp(schema: &str, name: &str, cols: &[Wire], rows: &[Vec<Value>]) -> Vec<u8> {
    let mut v = vec![0xF3];
    v.extend(bvar(""));
    v.extend(bvar(schema));
    v.extend(bvar(name));
    v.extend_from_slice(&(cols.len() as u16).to_le_bytes());
    for w in cols {
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        write_typeinfo(&mut v, w);
        v.extend(bvar(""));
    }
    v.push(0x00); // the end of the metadata
    for r in rows {
        v.push(0x01);
        for (w, x) in cols.iter().zip(r) {
            write_value(&mut v, w, x);
        }
    }
    v.push(0x00);
    v
}


/// The rows of a fetch of a cursor over `ct (id, name)`: (id, name, rowstat).
pub fn ct_rows(t: &[Tok]) -> Vec<(i32, String, i32)> {
    t.iter()
        .filter_map(|t| match t {
            Tok::Row(v) => match (&v[0], &v[1], &v[2]) {
                (Value::Int(i), Value::Str(n), Value::Int(s)) => Some((*i, n.clone(), *s)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

pub fn open_ct(r: &mut Raw, scroll: i32, cc: i32) -> Vec<i32> {
    let toks = r.rpc(Err(2), &[("", true, null_int()), ("", false, nvarchar("SELECT id, name FROM ct ORDER BY id")), ("", true, int(scroll)), ("", true, int(cc)), ("", true, null_int())]);
    assert!(!toks.iter().any(|t| matches!(t, Tok::Error { .. })), "{:?}", toks);
    ret_ints(&toks)
}

pub fn change(r: &mut Raw, cursor: i32, op: i32, row: i32, table: &str, values: &[(&str, Vec<u8>)]) -> Vec<Tok> {
    let mut ps: Vec<(&str, bool, Vec<u8>)> = vec![("", false, int(cursor)), ("", false, int(op)), ("", false, int(row)), ("", false, nvarchar(table))];
    for (n, v) in values {
        ps.push((n, false, v.clone()));
    }
    r.rpc(Err(1), &ps)
}

