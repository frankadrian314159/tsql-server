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

fn cursor_params(scroll: i32) -> Vec<(&'static str, bool, Vec<u8>)> {
    vec![
        ("", true, null_int()),
        ("", false, nvarchar("SELECT n FROM (VALUES (10), (20), (30), (40), (50)) AS t(n) ORDER BY n")),
        ("", true, int(scroll)),
        ("", true, int(4)),
        ("", true, null_int()),
    ]
}

fn fetch(r: &mut Raw, cursor: i32, kind: i32, row: i32, n: i32) -> Vec<Tok> {
    r.rpc(Err(7), &[("", false, int(cursor)), ("", false, int(kind)), ("", false, int(row)), ("", false, int(n))])
}

/// The values fetched: the first column is the value, the last rowstat.
fn fetched(t: &[Tok]) -> Vec<i32> {
    t.iter().filter_map(|t| if let Tok::Row(v) = t { if let Value::Int(n) = v[0] { Some(n) } else { None } } else { None }).collect()
}

fn ret_ints(t: &[Tok]) -> Vec<i32> {
    t.iter().filter_map(|t| if let Tok::Ret { val: Value::Int(v), .. } = t { Some(*v) } else { None }).collect()
}

#[test]
fn server_cursor_scrolls_over_a_snapshot() {
    let (l, _s) = start();
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.rpc(Err(2), &cursor_params(1)); // asks for a keyset cursor
    let rets = ret_ints(&toks);
    assert_eq!(rets.len(), 4, "{:?}", toks);
    let cursor = rets[0];
    assert!(cursor != 0);
    // the cursor in force is static and read-only, and has 5 rows
    assert_eq!(&rets[1..], &[8, 1, 5], "{:?}", toks);
    assert_eq!(last_done(&toks), (0, 0xFE));
    let t = fetch(&mut r, cursor, 2, 0, 2); // NEXT 2
    assert!(t.iter().any(|t| matches!(t, Tok::Meta(n) if n[0] == "n" && n[1] == "rowstat")), "{:?}", t);
    assert_eq!(fetched(&t), vec![10, 20]);
    assert_eq!(fetched(&fetch(&mut r, cursor, 2, 0, 2)), vec![30, 40]);
    assert_eq!(fetched(&fetch(&mut r, cursor, 2, 0, 2)), vec![50]);
    assert_eq!(fetched(&fetch(&mut r, cursor, 2, 0, 2)), Vec::<i32>::new()); // past the end
    assert_eq!(fetched(&fetch(&mut r, cursor, 8, 0, 2)), vec![40, 50]); // LAST
    assert_eq!(fetched(&fetch(&mut r, cursor, 4, 0, 2)), vec![20, 30]); // PREV
    assert_eq!(fetched(&fetch(&mut r, cursor, 1, 0, 1)), vec![10]); // FIRST
    assert_eq!(fetched(&fetch(&mut r, cursor, 4, 0, 1)), Vec::<i32>::new()); // before the first
    assert_eq!(fetched(&fetch(&mut r, cursor, 0x10, 3, 1)), vec![30]); // ABSOLUTE 3
    assert_eq!(fetched(&fetch(&mut r, cursor, 0x10, -1, 1)), vec![50]); // ABSOLUTE -1
    assert_eq!(fetched(&fetch(&mut r, cursor, 0x20, -2, 1)), vec![30]); // RELATIVE -2
    let t = r.rpc(Err(9), &[("", false, int(cursor))]); // sp_cursorclose
    assert_eq!(last_done(&t), (0, 0xFE));
    let t = fetch(&mut r, cursor, 2, 0, 1);
    assert!(t.iter().any(|t| matches!(t, Tok::Error { number: 16916, .. })), "{:?}", t);
    // a positioned update is refused
    let t = r.rpc(Err(1), &[("", false, int(cursor)), ("", false, int(33))]);
    assert!(t.iter().any(|t| matches!(t, Tok::Error { .. })));
    l.shutdown();
}

#[test]
fn forward_only_cursor_refuses_to_scroll_back_and_prepared_cursors_work() {
    let (l, _s) = start();
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.rpc(Err(2), &cursor_params(4)); // forward only
    let rets = ret_ints(&toks);
    let cursor = rets[0];
    assert_eq!(rets[1], 4);
    assert_eq!(fetched(&fetch(&mut r, cursor, 2, 0, 3)), vec![10, 20, 30]);
    let t = fetch(&mut r, cursor, 4, 0, 1);
    assert!(t.iter().any(|t| matches!(t, Tok::Error { number: 16911, .. })), "{:?}", t);
    // sp_cursorprepexec: the handle and the cursor come back, with a parameter
    let toks = r.rpc(
        Err(5),
        &[
            ("", true, null_int()),
            ("", true, null_int()),
            ("", false, nvarchar("@lo int")),
            ("", false, nvarchar("SELECT n FROM (VALUES (10), (20), (30)) AS t(n) WHERE n > @lo ORDER BY n")),
            ("", true, int(8)),
            ("", true, int(1)),
            ("", true, null_int()),
            ("@lo", false, int(10)),
        ],
    );
    let rets = ret_ints(&toks);
    assert_eq!(rets.len(), 5, "{:?}", toks);
    assert_eq!(rets[4], 2, "two rows are above 10");
    let cur = rets[1];
    assert_eq!(fetched(&fetch(&mut r, cur, 1, 0, 10)), vec![20, 30]);
    l.shutdown();
}

#[test]
fn sp_executesql_returns_the_status_of_the_batch_and_ansi_defaults_show_in_options() {
    let (l, _s) = start();
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    let toks = r.rpc(Err(10), &[("", false, nvarchar("RETURN 7"))]);
    assert!(toks.iter().any(|t| matches!(t, Tok::Status(7))), "{:?}", toks);
    let t = r.batch("SET ANSI_DEFAULTS ON; SELECT @@OPTIONS");
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(n) if n & 2 != 0 && n & 4 != 0 && n & 32 != 0))), "{:?}", t);
    let t = r.batch("SET ANSI_DEFAULTS OFF; SELECT @@OPTIONS");
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(n) if n & 2 == 0 && n & 32 == 0))), "{:?}", t);
    l.shutdown();
}

// ------------------------------------------------------------------------------------------------ MARS
/// A frame of the Session Multiplex Protocol: flags, session, sequence number, window, payload.
struct Smp(u8, u16, u32, u32, Vec<u8>);

impl Raw {
    /// Log in asking for MARS in the pre-login message; what follows is framed as SMP.
    fn connect_mars(l: &Listener) -> Raw {
        let mut s = TcpStream::connect(l.local_addr()).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
        // options: VERSION, ENCRYPTION (off), MARS (on)
        let mut pre = vec![0, 0, 16, 0, 6, 1, 0, 22, 0, 1, 4, 0, 23, 0, 1, 0xFF];
        pre.extend_from_slice(&[9, 0, 0, 0, 0, 0, 0, 1]);
        s.write_all(&packet(18, &pre)).unwrap();
        let mut r = Raw { s };
        let (kind, reply) = r.read_message();
        assert_eq!(kind, 4);
        // the reply carries the MARS option, granted
        let mut i = 0;
        let mut granted = false;
        while reply[i] != 0xFF {
            if reply[i] == 4 {
                let off = u16::from_be_bytes([reply[i + 1], reply[i + 2]]) as usize;
                granted = reply[off] == 1;
            }
            i += 5;
        }
        assert!(granted, "MARS must be granted");
        r.s.write_all(&packet(16, &login7("sa", "pw"))).unwrap(); // the login itself is plain
        let toks = r.tokens();
        assert!(toks.iter().any(|t| matches!(t, Tok::LoginAck)));
        r
    }

    fn smp_send(&mut self, flags: u8, sid: u16, seq: u32, wnd: u32, payload: &[u8]) {
        let mut f = vec![0x53, flags];
        f.extend_from_slice(&sid.to_le_bytes());
        f.extend_from_slice(&((16 + payload.len()) as u32).to_le_bytes());
        f.extend_from_slice(&seq.to_le_bytes());
        f.extend_from_slice(&wnd.to_le_bytes());
        f.extend_from_slice(payload);
        self.s.write_all(&f).unwrap();
    }

    fn smp_read(&mut self) -> Smp {
        let mut h = [0u8; 16];
        self.s.read_exact(&mut h).unwrap();
        assert_eq!(h[0], 0x53);
        let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        let mut p = vec![0u8; len - 16];
        self.s.read_exact(&mut p).unwrap();
        Smp(h[1], u16::from_le_bytes([h[2], h[3]]), u32::from_le_bytes([h[8], h[9], h[10], h[11]]), u32::from_le_bytes([h[12], h[13], h[14], h[15]]), p)
    }

    /// A SQL batch on session `sid`, as its n-th DATA frame.
    fn smp_batch(&mut self, sid: u16, n: u32, sql: &str) {
        let mut p = headers();
        p.extend(utf16(sql));
        self.smp_send(0x08, sid, n, 4 + n, &packet(1, &p));
    }
}

/// The tokens of a reply made of the payloads of TDS packets joined in `data`.
fn reply_tokens(data: Vec<u8>) -> Vec<Tok> {
    let mut rd = Rd::new(&data);
    let mut out = vec![];
    let mut cols: Vec<Ti> = vec![];
    while rd.left() > 0 {
        match rd.u8().unwrap() {
            0xFD | 0xFE | 0xFF => {
                let status = rd.u16().unwrap();
                rd.u16().unwrap();
                let count = rd.u64().unwrap();
                out.push(Tok::Done { status, count, kind: 0xFD });
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
            0xE3 => {
                let len = rd.u16().unwrap() as usize;
                rd.take(len).unwrap();
            }
            other => panic!("unexpected token 0x{:02X}", other),
        }
    }
    out
}

/// Add the frame `f` of session `sid` to the message being assembled; the tokens when it is complete.
fn take_frame(r: &mut Raw, f: Smp, inbytes: &mut Vec<u8>, data: &mut Vec<u8>) -> Option<Vec<Tok>> {
    r.smp_send(0x02, f.1, 0, f.2 + 4, &[]);
    inbytes.extend(f.4);
    while inbytes.len() >= 8 {
        let len = u16::from_be_bytes([inbytes[2], inbytes[3]]) as usize;
        if inbytes.len() < len {
            break;
        }
        let pkt: Vec<u8> = inbytes.drain(..len).collect();
        data.extend_from_slice(&pkt[8..]);
        if pkt[1] & 1 != 0 {
            return Some(reply_tokens(std::mem::take(data)));
        }
    }
    None
}

/// Frames until the message of `sid` is complete; frames of other sessions are collected in `others`.
fn read_reply(r: &mut Raw, sid: u16, others: &mut Vec<Smp>) -> Vec<Tok> {
    let (mut data, mut inbytes) = (vec![], vec![]);
    loop {
        let f = r.smp_read();
        if f.0 & 0x08 == 0 {
            continue; // an ACK
        }
        if f.1 != sid {
            others.push(f);
            continue;
        }
        if let Some(t) = take_frame(r, f, &mut inbytes, &mut data) {
            return t;
        }
    }
}

#[test]
fn mars_sessions_share_a_connection() {
    let (l, _s) = start();
    let mut r = Raw::connect_mars(&l);
    // open two logical sessions: each SYN is acknowledged
    for sid in [1u16, 2] {
        r.smp_send(0x01, sid, 0, 4, &[]);
        let f = r.smp_read();
        assert_eq!((f.0, f.1), (0x02, sid), "a SYN is acknowledged");
    }
    let mut others = vec![];
    r.smp_batch(1, 1, "CREATE TABLE mars_t (n INT); INSERT mars_t VALUES (1), (2), (3)");
    read_reply(&mut r, 1, &mut others);
    r.smp_batch(1, 2, "SELECT COUNT(*) FROM mars_t");
    let t = read_reply(&mut r, 1, &mut others);
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(3)))), "{:?}", t);
    // the second session sees the same connection state: a temporary table of the first
    r.smp_batch(1, 3, "CREATE TABLE #mine (a INT); INSERT #mine VALUES (7)");
    read_reply(&mut r, 1, &mut others);
    r.smp_batch(2, 1, "SELECT a FROM #mine");
    let t = read_reply(&mut r, 2, &mut others);
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(7)))), "{:?}", t);
    // a big result on session 1 that the client does not acknowledge must not hold up session 2
    r.smp_batch(1, 4, "SELECT REPLICATE(CAST('x' AS varchar(max)), 100000)");
    r.smp_batch(2, 2, "SELECT 42");
    let t = read_reply(&mut r, 2, &mut others);
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(42)))), "{:?}", t);
    let early = others.iter().filter(|f| f.1 == 1).count();
    assert!(early <= 4, "only the window of frames of session 1 can have arrived: {}", early);
    // now read session 1's result (the frames already read are in `others`)
    let mut frames: std::collections::VecDeque<Smp> = others.drain(..).filter(|f| f.1 == 1).collect();
    let (mut data, mut inbytes) = (vec![], vec![]);
    let t = loop {
        let f = match frames.pop_front() {
            Some(f) => f,
            None => {
                let f = r.smp_read();
                if f.0 & 0x08 == 0 || f.1 != 1 {
                    continue;
                }
                f
            }
        };
        if let Some(t) = take_frame(&mut r, f, &mut inbytes, &mut data) {
            break t;
        }
    };
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(&v[0], Value::Str(s) if s.len() == 100000))), "the whole value arrives");
    // closing a session is acknowledged with FIN
    r.smp_send(0x04, 2, 3, 8, &[]);
    let f = r.smp_read();
    assert_eq!((f.0, f.1), (0x04, 2));
    l.shutdown();
}

// ------------------------------------------------------------------------------------------------ table-valued parameters
fn bvar(s: &str) -> Vec<u8> {
    let mut v = vec![s.encode_utf16().count() as u8];
    v.extend(utf16(s));
    v
}

/// A table-valued parameter: its type, columns and rows, as TVP_TYPE_INFO and the rows that follow.
fn tvp(schema: &str, name: &str, cols: &[Wire], rows: &[Vec<Value>]) -> Vec<u8> {
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

#[test]
fn table_valued_parameters_reach_procedures_and_sp_executesql() {
    let (l, _s) = start();
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    r.batch("CREATE TYPE dbo.IdList AS TABLE (id INT NOT NULL PRIMARY KEY, name NVARCHAR(20) NULL)");
    r.batch("CREATE PROCEDURE dbo.p_sum @t dbo.IdList READONLY, @k INT AS SELECT SUM(id) + @k AS s, COUNT(*) AS n, MAX(name) AS m FROM @t");
    let cols = [Wire::Int(4), Wire::NVarChar(20)];
    let rows = vec![vec![Value::Int(1), Value::str("a")], vec![Value::Int(2), Value::Null], vec![Value::Int(3), Value::str("caf\u{e9}")]];
    // sp_executesql with the table in its parameter list
    let t = r.rpc(
        Err(10),
        &[("", false, nvarchar("SELECT SUM(id) AS s, COUNT(*) AS n FROM @t")), ("", false, nvarchar("@t dbo.IdList READONLY")), ("@t", false, tvp("dbo", "IdList", &cols, &rows))],
    );
    let row = t.iter().find_map(|t| if let Tok::Row(v) = t { Some(v.clone()) } else { None }).unwrap_or_else(|| panic!("{:?}", t));
    assert!(matches!(row[0], Value::Int(6)) || matches!(row[0], Value::BigInt(6)), "{:?}", row);
    assert!(matches!(row[1], Value::Int(3)), "{:?}", row);
    // a stored procedure by name, with the table and another parameter
    let t = r.rpc(Ok("dbo.p_sum"), &[("@t", false, tvp("dbo", "IdList", &cols, &rows)), ("@k", false, int(100))]);
    let row = t.iter().find_map(|t| if let Tok::Row(v) = t { Some(v.clone()) } else { None }).unwrap_or_else(|| panic!("{:?}", t));
    assert!(matches!(row[0], Value::Int(106)) || matches!(row[0], Value::BigInt(106)), "{:?}", row);
    assert!(matches!(&row[2], Value::Str(s) if s == "caf\u{e9}"), "{:?}", row);
    // no rows at all, and a type that is not there
    let t = r.rpc(Ok("dbo.p_sum"), &[("@t", false, tvp("dbo", "IdList", &cols, &[])), ("@k", false, int(5))]);
    assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[1], Value::Int(0)))), "{:?}", t);
    let t = r.rpc(Err(10), &[("", false, nvarchar("SELECT 1")), ("", false, nvarchar("@t dbo.Nothing READONLY")), ("@t", false, tvp("dbo", "Nothing", &cols, &rows))]);
    assert!(t.iter().any(|t| matches!(t, Tok::Error { .. })), "{:?}", t);
    l.shutdown();
}

// ------------------------------------------------------------------------------------------------ keyset and updatable cursors
/// The rows of a fetch of a cursor over `ct (id, name)`: (id, name, rowstat).
fn ct_rows(t: &[Tok]) -> Vec<(i32, String, i32)> {
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

fn open_ct(r: &mut Raw, scroll: i32, cc: i32) -> Vec<i32> {
    let toks = r.rpc(Err(2), &[("", true, null_int()), ("", false, nvarchar("SELECT id, name FROM ct ORDER BY id")), ("", true, int(scroll)), ("", true, int(cc)), ("", true, null_int())]);
    assert!(!toks.iter().any(|t| matches!(t, Tok::Error { .. })), "{:?}", toks);
    ret_ints(&toks)
}

fn change(r: &mut Raw, cursor: i32, op: i32, row: i32, table: &str, values: &[(&str, Vec<u8>)]) -> Vec<Tok> {
    let mut ps: Vec<(&str, bool, Vec<u8>)> = vec![("", false, int(cursor)), ("", false, int(op)), ("", false, int(row)), ("", false, nvarchar(table))];
    for (n, v) in values {
        ps.push((n, false, v.clone()));
    }
    r.rpc(Err(1), &ps)
}

#[test]
fn keyset_cursors_see_changes_and_updatable_ones_make_them() {
    let (l, server) = start();
    let mut other = server.connect();
    other.execute("CREATE TABLE ct (id INT IDENTITY(1, 1) PRIMARY KEY, name NVARCHAR(20))");
    other.execute("INSERT ct (name) VALUES (N'a'), (N'b'), (N'c'), (N'd')");
    let (mut r, _) = Raw::connect(&l, "sa", "pw");
    // a keyset cursor, updatable: the type and concurrency come back as asked
    let rets = open_ct(&mut r, 1, 4);
    assert_eq!(rets, vec![rets[0], 1, 4, 4], "cursor, keyset, optimistic, 4 rows");
    let cursor = rets[0];
    let t = fetch(&mut r, cursor, 1, 0, 4);
    assert_eq!(ct_rows(&t), vec![(1, "a".into(), 1), (2, "b".into(), 1), (3, "c".into(), 1), (4, "d".into(), 1)]);
    // browse information: the table and the columns (key, hidden rowstat)
    // another session changes a row: a fetch shows the new value
    other.execute("UPDATE ct SET name = N'B2' WHERE id = 2");
    assert_eq!(ct_rows(&fetch(&mut r, cursor, 1, 0, 4))[1], (2, "B2".into(), 1));
    // an update through the cursor (the fourth parameter names the table; the values are named by column)
    let t = change(&mut r, cursor, 33, 1, "ct", &[("@name", nvarchar("changed"))]);
    assert!(t.iter().any(|t| matches!(t, Tok::Status(0))), "{:?}", t);
    assert_eq!(other.query("SELECT name FROM ct WHERE id = 1").unwrap()[0][0].to_string(), "changed");
    assert_eq!(ct_rows(&fetch(&mut r, cursor, 0x80, 0, 0))[0].1, "changed");
    // the optimistic check: a row that someone else changed since it was fetched is not overwritten
    other.execute("UPDATE ct SET name = N'sneaky' WHERE id = 3");
    let t = change(&mut r, cursor, 33, 3, "ct", &[("@name", nvarchar("mine"))]);
    assert!(t.iter().any(|t| matches!(t, Tok::Error { number: 16934, .. })), "{:?}", t);
    assert_eq!(other.query("SELECT name FROM ct WHERE id = 3").unwrap()[0][0].to_string(), "sneaky");
    // after a fetch the cursor knows the new value and the update goes through
    fetch(&mut r, cursor, 1, 0, 4);
    assert!(change(&mut r, cursor, 33, 3, "ct", &[("@name", nvarchar("mine"))]).iter().any(|t| matches!(t, Tok::Status(0))));
    // a delete through the cursor, and a row deleted elsewhere, are flagged (rowstat 2)
    assert!(change(&mut r, cursor, 34, 2, "ct", &[]).iter().any(|t| matches!(t, Tok::Status(0))));
    other.execute("DELETE FROM ct WHERE id = 4");
    let rows = ct_rows(&fetch(&mut r, cursor, 1, 0, 4));
    assert_eq!((rows[1].0, rows[1].2), (2, 2), "{:?}", rows);
    assert_eq!((rows[3].0, rows[3].2), (4, 2), "{:?}", rows);
    assert_eq!(rows[0].2, 1);
    assert_eq!(other.query("SELECT COUNT(*) FROM ct").unwrap()[0][0].to_string(), "2");
    // an insert through the cursor: the identity value is found and the row joins the cursor
    assert!(change(&mut r, cursor, 4, 1, "ct", &[("@name", nvarchar("new"))]).iter().any(|t| matches!(t, Tok::Status(0))));
    let rows = ct_rows(&fetch(&mut r, cursor, 8, 0, 1)); // LAST
    assert_eq!((rows[0].1.as_str(), rows[0].2), ("new", 1), "{:?}", rows);
    // a forward-only cursor that asks to be updatable is a keyset one too
    let rets = open_ct(&mut r, 4, 4);
    assert_eq!(&rets[1..3], &[4, 4]);
    // a static cursor, a read-only one and one that is not a plain read of a table stay read-only and refuse changes
    let rets = open_ct(&mut r, 8, 4);
    assert_eq!(&rets[1..3], &[8, 1]);
    let t = change(&mut r, rets[0], 33, 1, "ct", &[("@name", nvarchar("x"))]);
    assert!(t.iter().any(|t| matches!(t, Tok::Error { number: 16929, .. })), "{:?}", t);
    let rets = open_ct(&mut r, 1, 1);
    assert_eq!(&rets[1..3], &[1, 1], "a read-only keyset cursor");
    let toks = r.rpc(Err(2), &[("", true, null_int()), ("", false, nvarchar("SELECT name, COUNT(*) FROM ct GROUP BY name")), ("", true, int(1)), ("", true, int(4)), ("", true, null_int())]);
    assert_eq!(&ret_ints(&toks)[1..3], &[8, 1], "{:?}", toks);
    l.shutdown();
}

#[test]
fn mars_requests_that_only_read_run_at_the_same_time() {
    let (l, _s) = start();
    let mut r = Raw::connect_mars(&l);
    for sid in [1u16, 2, 3] {
        r.smp_send(0x01, sid, 0, 4, &[]);
        assert_eq!(r.smp_read().0, 0x02);
    }
    let mut others = vec![];
    r.smp_batch(1, 1, "CREATE TABLE ptab (n INT); WITH c AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM c WHERE i < 40000) INSERT ptab SELECT i FROM c OPTION (MAXRECURSION 0)");
    read_reply(&mut r, 1, &mut others);
    r.smp_batch(1, 2, "CREATE TABLE #tmp (a INT); INSERT #tmp VALUES (7)");
    read_reply(&mut r, 1, &mut others);
    // a long read on session 1, and short ones on 2 and 3 sent after it: the short ones are answered first
    r.smp_batch(1, 3, "SELECT COUNT(*) FROM ptab WHERE LEN(REPLICATE('abcdefghij', n % 400)) > 3000 AND CHARINDEX('j', REPLICATE('abcdefghij', n % 50)) > 0");
    r.smp_batch(2, 1, "SELECT 42");
    r.smp_batch(3, 1, "SELECT a FROM #tmp"); // a temporary table of the connection is visible to a worker
    let mut state: std::collections::HashMap<u16, (Vec<u8>, Vec<u8>)> = Default::default();
    let mut finished: Vec<(u16, Vec<Tok>)> = vec![];
    while finished.len() < 3 {
        let f = r.smp_read();
        if f.0 & 0x08 == 0 {
            continue;
        }
        let sid = f.1;
        let e = state.entry(sid).or_default();
        if let Some(t) = take_frame(&mut r, f, &mut e.0, &mut e.1) {
            finished.push((sid, t));
        }
    }
    let order: Vec<u16> = finished.iter().map(|f| f.0).collect();
    assert_eq!(order.last(), Some(&1), "the long read finishes last: {:?}", order);
    for (sid, t) in &finished {
        match sid {
            2 => assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(42)))), "{:?}", t),
            3 => assert!(t.iter().any(|t| matches!(t, Tok::Row(v) if matches!(v[0], Value::Int(7)))), "{:?}", t),
            _ => assert!(t.iter().any(|t| matches!(t, Tok::Row(_))), "{:?}", t),
        }
    }
    // a write waits for the reads that are running, and the reads that follow it see it
    r.smp_batch(1, 4, "SELECT COUNT(*) FROM ptab WHERE LEN(REPLICATE('abcdefghij', n % 400)) > 3000");
    r.smp_batch(2, 2, "INSERT ptab VALUES (-1)");
    r.smp_batch(3, 2, "SELECT COUNT(*) FROM ptab WHERE n = -1");
    let mut state: std::collections::HashMap<u16, (Vec<u8>, Vec<u8>)> = Default::default();
    let mut finished: Vec<(u16, Vec<Tok>)> = vec![];
    while finished.len() < 3 {
        let f = r.smp_read();
        if f.0 & 0x08 == 0 {
            continue;
        }
        let sid = f.1;
        let e = state.entry(sid).or_default();
        if let Some(t) = take_frame(&mut r, f, &mut e.0, &mut e.1) {
            finished.push((sid, t));
        }
    }
    let order: Vec<u16> = finished.iter().map(|f| f.0).collect();
    assert_eq!(order, vec![1, 2, 3], "the write waited for the read before it, and the read after it saw the write: {:?}", order);
    let seen = finished[2].1.iter().find_map(|t| if let Tok::Row(v) = t { Some(v[0].clone()) } else { None });
    assert!(matches!(seen, Some(Value::Int(1))), "{:?}", finished[2].1);
    l.shutdown();
}
