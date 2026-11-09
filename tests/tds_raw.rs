//! Protocol-level tests for requests that high-level clients rarely expose: RPC with OUTPUT parameters and
//! return status, prepared statements, transaction-manager requests, attention and malformed input.
mod common;
use std::io::{Read, Write};
use std::net::TcpStream;
use tsql_server::tds::wire::*;
use tsql_server::tds::{Config, Listener};
use tsql_server::Value;

struct Raw {
    s: TcpStream,
}

#[derive(Debug)]
#[allow(dead_code)]
enum Tok {
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

fn packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![kind, 1];
    p.extend_from_slice(&((payload.len() + 8) as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 1, 0]);
    p.extend_from_slice(payload);
    p
}

fn headers() -> Vec<u8> {
    let mut h = vec![];
    h.extend_from_slice(&22u32.to_le_bytes());
    h.extend_from_slice(&18u32.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&0u64.to_le_bytes());
    h.extend_from_slice(&1u32.to_le_bytes());
    h
}

fn utf16(s: &str) -> Vec<u8> {
    let mut v = vec![];
    put_utf16(&mut v, s);
    v
}

fn login7(user: &str, pw: &str) -> Vec<u8> {
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
    fn connect(l: &Listener, user: &str, pw: &str) -> (Raw, Vec<Tok>) {
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

    fn read_message(&mut self) -> (u8, Vec<u8>) {
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

    fn tokens(&mut self) -> Vec<Tok> {
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

    fn batch(&mut self, sql: &str) -> Vec<Tok> {
        let mut p = headers();
        p.extend(utf16(sql));
        self.s.write_all(&packet(1, &p)).unwrap();
        self.tokens()
    }

    fn tm(&mut self, kind: u16, extra: &[u8]) -> Vec<Tok> {
        let mut p = headers();
        p.extend_from_slice(&kind.to_le_bytes());
        p.extend_from_slice(extra);
        self.s.write_all(&packet(14, &p)).unwrap();
        self.tokens()
    }

    /// RPC by name or id; params are (name, output, type info bytes, value bytes).
    fn rpc(&mut self, proc: Result<&str, u16>, params: &[(&str, bool, Vec<u8>)]) -> Vec<Tok> {
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

fn nvarchar(s: &str) -> Vec<u8> {
    let mut v = vec![];
    write_typeinfo(&mut v, &Wire::NVarChar(4000));
    write_value(&mut v, &Wire::NVarChar(4000), &Value::str(s));
    v
}
fn int(i: i32) -> Vec<u8> {
    let mut v = vec![];
    write_typeinfo(&mut v, &Wire::Int(4));
    write_value(&mut v, &Wire::Int(4), &Value::Int(i));
    v
}
fn null_int() -> Vec<u8> {
    let mut v = vec![];
    write_typeinfo(&mut v, &Wire::Int(4));
    write_value(&mut v, &Wire::Int(4), &Value::Null);
    v
}

fn start() -> (Listener, tsql_server::Server) {
    let server = common::server();
    server.connect().execute("ALTER LOGIN sa WITH PASSWORD = N'pw'");
    (Listener::bind(server.clone(), "127.0.0.1:0", Config::default()).unwrap(), server)
}

fn last_done(t: &[Tok]) -> (u16, u8) {
    match t.last() {
        Some(Tok::Done { status, kind, .. }) => (*status, *kind),
        o => panic!("no DONE at the end: {:?}", o),
    }
}

#[test]
fn login_reply_has_loginack_and_envchanges() {
    let (l, _s) = start();
    let (_r, toks) = Raw::connect(&l, "sa", "pw");
    assert!(toks.iter().any(|t| matches!(t, Tok::LoginAck)));
    assert!(toks.iter().any(|t| matches!(t, Tok::Env { kind: 1 })));
    assert!(toks.iter().any(|t| matches!(t, Tok::Env { kind: 4 })));
    assert!(matches!(last_done(&toks), (0, 0xFD)));
    l.shutdown();
}

#[test]
fn failed_login_reports_18456() {
    let (l, _s) = start();
    let (_r, toks) = Raw::connect(&l, "sa", "nope");
    assert!(toks.iter().any(|t| matches!(t, Tok::Error { number: 18456, .. })));
    assert!(!toks.iter().any(|t| matches!(t, Tok::LoginAck)));
    l.shutdown();
}

#[test]
fn sp_executesql_returns_output_parameters() {
    let (l, _s) = start();
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.rpc(
        Err(10),
        &[
            ("", false, nvarchar("SET @out = @a * 2; SELECT @a AS echo")),
            ("", false, nvarchar("@a int, @out int OUTPUT")),
            ("@a", false, int(21)),
            ("@out", true, null_int()),
        ],
    );
    assert!(toks.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(21)))), "{:?}", toks);
    assert!(toks.iter().any(|t| matches!(t, Tok::Ret { name, val: Value::Int(42) } if name == "@out")), "{:?}", toks);
    assert!(toks.iter().any(|t| matches!(t, Tok::Status(0))));
    assert_eq!(last_done(&toks), (0, 0xFE));
    // the helper result set that carries return values never reaches the client
    assert!(!toks.iter().any(|t| matches!(t, Tok::Meta(n) if n[0] == "__tds_trailer")));
    l.shutdown();
}

#[test]
fn stored_procedure_call_returns_status_and_output() {
    let (l, s) = start();
    s.connect().execute("CREATE PROCEDURE addone @n int, @r int OUTPUT AS BEGIN SET @r = @n + 1; RETURN 7 END");
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.rpc(Ok("addone"), &[("@n", false, int(4)), ("@r", true, null_int())]);
    assert!(toks.iter().any(|t| matches!(t, Tok::Status(7))), "{:?}", toks);
    assert!(toks.iter().any(|t| matches!(t, Tok::Ret { name, val: Value::Int(5) } if name == "@r")), "{:?}", toks);
    // unknown procedure
    let toks = r.rpc(Ok("nosuchproc"), &[]);
    assert!(toks.iter().any(|t| matches!(t, Tok::Error { number: 2812, .. })), "{:?}", toks);
    assert_eq!(last_done(&toks).0 & 2, 2);
    // an unsafe name is refused rather than executed
    let toks = r.rpc(Ok("x; DROP TABLE y"), &[]);
    assert!(toks.iter().any(|t| matches!(t, Tok::Error { .. })));
    l.shutdown();
}

#[test]
fn prepare_execute_unprepare() {
    let (l, s) = start();
    s.connect().execute("CREATE TABLE p (id int, v nvarchar(20)); INSERT p VALUES (1, N'one'), (2, N'two')");
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.rpc(Err(11), &[("@handle", true, null_int()), ("", false, nvarchar("@id int")), ("", false, nvarchar("SELECT v FROM p WHERE id = @id")), ("", false, int(1))]);
    let handle = toks.iter().find_map(|t| if let Tok::Ret { val: Value::Int(h), .. } = t { Some(*h) } else { None }).expect("handle");
    for (id, want) in [(1, "one"), (2, "two")] {
        let toks = r.rpc(Err(12), &[("", false, int(handle)), ("@id", false, int(id))]);
        assert!(toks.iter().any(|t| matches!(t, Tok::Row(v) if matches!(&v[0], Value::Str(s) if s == want))), "{:?}", toks);
    }
    r.rpc(Err(15), &[("", false, int(handle))]);
    let toks = r.rpc(Err(12), &[("", false, int(handle)), ("@id", false, int(1))]);
    assert!(toks.iter().any(|t| matches!(t, Tok::Error { .. })), "executing an unprepared handle must fail");
    // prepexec prepares and runs in one call
    let toks = r.rpc(Err(13), &[("@handle", true, null_int()), ("", false, nvarchar("@id int")), ("", false, nvarchar("SELECT v FROM p WHERE id = @id")), ("@id", false, int(2))]);
    assert!(toks.iter().any(|t| matches!(t, Tok::Row(v) if matches!(&v[0], Value::Str(s) if s == "two"))));
    assert!(toks.iter().any(|t| matches!(t, Tok::Ret { .. })));
    l.shutdown();
}

#[test]
fn transaction_manager_requests() {
    let (l, s) = start();
    s.connect().execute("CREATE TABLE tm (x int)");
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.tm(5, &[0, 0]); // begin, default isolation, no name
    assert!(toks.iter().any(|t| matches!(t, Tok::Env { kind: 8 })), "{:?}", toks);
    assert_ne!(last_done(&toks).0 & 4, 0, "DONE_INXACT while a transaction is open");
    r.batch("INSERT tm VALUES (1)");
    let toks = r.tm(8, &[0]); // rollback
    assert!(toks.iter().any(|t| matches!(t, Tok::Env { kind: 10 })), "{:?}", toks);
    assert_eq!(s.connect().query("SELECT COUNT(*) FROM tm").unwrap()[0][0].to_string(), "0");
    r.tm(5, &[0, 0]);
    r.batch("INSERT tm VALUES (2)");
    let toks = r.tm(7, &[0, 0]); // commit
    assert!(toks.iter().any(|t| matches!(t, Tok::Env { kind: 9 })), "{:?}", toks);
    assert_eq!(s.connect().query("SELECT COUNT(*) FROM tm").unwrap()[0][0].to_string(), "1");
    // a transaction begun by SQL text is announced too
    let toks = r.batch("BEGIN TRAN");
    assert!(toks.iter().any(|t| matches!(t, Tok::Env { kind: 8 })));
    let toks = r.batch("COMMIT");
    assert!(toks.iter().any(|t| matches!(t, Tok::Env { kind: 9 })));
    l.shutdown();
}

#[test]
fn row_counts_messages_and_attention() {
    let (l, s) = start();
    s.connect().execute("CREATE TABLE rc (x int)");
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.batch("INSERT rc VALUES (1),(2),(3); PRINT 'hi'; SELECT x FROM rc");
    assert!(toks.iter().any(|t| matches!(t, Tok::Done { count: 3, status, .. } if status & 0x10 != 0)));
    assert!(toks.iter().any(|t| matches!(t, Tok::Info { number: 0 })));
    assert_eq!(toks.iter().filter(|t| matches!(t, Tok::Row(_))).count(), 3);
    assert!(matches!(last_done(&toks), (0, 0xFD)));
    // attention is acknowledged
    r.s.write_all(&packet(6, &[])).unwrap();
    let toks = r.tokens();
    assert!(matches!(last_done(&toks), (st, _) if st & 0x20 != 0));
    // and the session still works
    let toks = r.batch("SELECT 1");
    assert!(toks.iter().any(|t| matches!(t, Tok::Row(_))));
    l.shutdown();
}

#[test]
fn garbage_closes_the_connection_but_not_the_server() {
    let (l, _s) = start();
    let mut bad = TcpStream::connect(l.local_addr()).unwrap();
    bad.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    let mut buf = [0u8; 16];
    // the server drops it (EOF or reset)
    bad.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    assert!(matches!(bad.read(&mut buf), Ok(0) | Err(_)));
    // a valid client still gets in
    let (_r, toks) = Raw::connect(&l, "sa", "pw");
    assert!(toks.iter().any(|t| matches!(t, Tok::LoginAck)));
    l.shutdown();
}
