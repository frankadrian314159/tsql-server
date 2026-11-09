//! A TDS 7.4 listener, so SQL Server clients (sqlcmd, SSMS-less tools, ODBC, JDBC, ADO.NET, tiberius, ...) can talk
//! to a [`Server`] over TCP.
//!
//! Supported: pre-login, SQL authentication (Login7), SQL batches, RPC requests (`sp_executesql`,
//! `sp_prepare`/`sp_execute`/`sp_prepexec`/`sp_unprepare`, and calls to stored procedures, with OUTPUT parameters
//! and return status), transaction-manager requests (begin/commit/rollback/save), attention (cancel), and
//! the usual data types. Not supported: TLS (the server answers "encryption not supported", so clients must connect
//! with encryption off or optional), integrated/Windows/AAD authentication, bulk load, server cursors, MARS
//! and the `sql_variant`/`text`/`ntext`/`image` wire types (they are sent as `nvarchar(max)`, `varchar(max)`, ...).
mod rpc;
pub mod wire;

use crate::{Connection, Event, Server, SqlError};
use crate::engine::BatchResult;
use rpc::Proc;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use wire::*;

const T_BATCH: u8 = 1;
const T_RPC: u8 = 3;
const T_REPLY: u8 = 4;
const T_ATTN: u8 = 6;
const T_BULK: u8 = 7;
const T_TM: u8 = 14;
const T_LOGIN7: u8 = 16;
const T_SSPI: u8 = 17;
const T_PRELOGIN: u8 = 18;

const DONE_MORE: u16 = 0x01;
const DONE_ERROR: u16 = 0x02;
const DONE_INXACT: u16 = 0x04;
const DONE_COUNT: u16 = 0x10;
const DONE_ATTN: u16 = 0x20;

const TDS_74: u32 = 0x7400_0004;
const DEFAULT_PACKET: usize = 4096;
const MAX_MESSAGE: usize = 256 << 20;
const SERVER_NAME: &str = "tsql-server";

// ----------------------------------------------------------------------------- packets
struct Transport {
    stream: TcpStream,
    packet_size: usize,
    spid: u16,
}

impl Transport {
    /// Read one complete message (packets up to and including the one marked end-of-message).
    fn read_message(&mut self) -> io::Result<(u8, u8, Vec<u8>)> {
        let mut data = vec![];
        let mut kind = 0;
        let mut first_status = 0;
        loop {
            let mut h = [0u8; 8];
            self.stream.read_exact(&mut h)?;
            let len = u16::from_be_bytes([h[2], h[3]]) as usize;
            if len < 8 || !matches!(h[0], 1..=8 | 14 | 16 | 17 | 18) || h[1] > 0x1F {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a TDS packet"));
            }
            if data.is_empty() {
                kind = h[0];
                first_status = h[1];
            }
            let start = data.len();
            data.resize(start + len - 8, 0);
            self.stream.read_exact(&mut data[start..])?;
            if data.len() > MAX_MESSAGE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
            }
            if h[1] & 1 != 0 {
                return Ok((kind, first_status, data));
            }
        }
    }

    fn send(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let room = self.packet_size - 8;
        let mut id = 1u8;
        let mut chunks: Vec<&[u8]> = payload.chunks(room).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let n = chunks.len();
        let mut buf = Vec::with_capacity(payload.len() + 8 * n);
        for (i, c) in chunks.into_iter().enumerate() {
            buf.push(kind);
            buf.push(if i + 1 == n { 1 } else { 0 });
            buf.extend_from_slice(&((c.len() + 8) as u16).to_be_bytes());
            buf.extend_from_slice(&self.spid.to_be_bytes());
            buf.push(id);
            buf.push(0);
            buf.extend_from_slice(c);
            id = id.wrapping_add(1);
        }
        self.stream.write_all(&buf)?;
        self.stream.flush()
    }
}

// ----------------------------------------------------------------------------- tokens
fn done(out: &mut Vec<u8>, token: u8, status: u16, count: u64) {
    out.push(token);
    out.extend_from_slice(&status.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
}

fn message_token(out: &mut Vec<u8>, token: u8, number: i32, state: u8, class: u8, text: &str, procedure: &str) {
    let text: String = text.chars().take(2000).collect();
    let mut body = vec![];
    body.extend_from_slice(&number.to_le_bytes());
    body.push(state);
    body.push(class);
    put_usvar(&mut body, &text);
    put_bvar(&mut body, SERVER_NAME);
    put_bvar(&mut body, procedure);
    body.extend_from_slice(&1u32.to_le_bytes());
    out.push(token);
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(&body);
}

fn error_token(out: &mut Vec<u8>, e: &SqlError) {
    message_token(out, 0xAA, e.number, e.state, e.severity, &e.message, e.procedure.as_deref().unwrap_or(""));
}

fn info_token(out: &mut Vec<u8>, number: i32, text: &str) {
    message_token(out, 0xAB, number, 1, if number == 0 { 0 } else { 10 }, text, "");
}

fn envchange(out: &mut Vec<u8>, kind: u8, new: &[u8], old: &[u8], binary: bool) {
    let mut body = vec![kind];
    if binary {
        body.push(new.len() as u8);
        body.extend_from_slice(new);
        body.push(old.len() as u8);
        body.extend_from_slice(old);
    } else {
        put_bvar(&mut body, std::str::from_utf8(new).unwrap_or(""));
        put_bvar(&mut body, std::str::from_utf8(old).unwrap_or(""));
    }
    out.push(0xE3);
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(&body);
}

fn colmetadata(out: &mut Vec<u8>, names: &[String], wires: &[Wire]) {
    out.push(0x81);
    out.extend_from_slice(&(names.len() as u16).to_le_bytes());
    for (n, w) in names.iter().zip(wires) {
        out.extend_from_slice(&0u32.to_le_bytes()); // user type
        out.extend_from_slice(&0x0009u16.to_le_bytes()); // nullable, update status unknown
        write_typeinfo(out, w);
        put_bvar(out, n);
    }
}

fn wires_for(rs: &crate::ResultSet) -> Vec<Wire> {
    (0..rs.columns.len())
        .map(|c| {
            let dt = rs.types.get(c).cloned().unwrap_or(crate::DataType::NVarChar(crate::types::MAX));
            let observed = rs
                .rows
                .iter()
                .map(|r| match r.get(c) {
                    Some(Value::Str(s)) => s.encode_utf16().count(),
                    Some(Value::Bin(b)) => b.len(),
                    _ => 0,
                })
                .max()
                .unwrap_or(0);
            let mut w = Wire::for_type(&dt, observed);
            // a runtime string whose type was only estimated (a literal, an expression) may hold characters the
            // code page cannot represent; send such a column as nvarchar
            if let Wire::VarChar(n) | Wire::Char(n) = w {
                let lossy = rs.rows.iter().any(|r| matches!(r.get(c), Some(Value::Str(s)) if to_cp1252(s).iter().zip(s.chars()).any(|(b, ch)| *b == b'?' && ch != '?')));
                if lossy {
                    w = Wire::NVarChar(if n == PLP { PLP } else { (n.saturating_mul(2)).min(PLP - 1).max(2) });
                    if let Wire::NVarChar(m) = w {
                        if m > 8000 {
                            w = Wire::NVarChar(PLP);
                        }
                    }
                }
            }
            w
        })
        .collect()
}

use crate::Value;

// ----------------------------------------------------------------------------- the session
/// How a request's reply ends: batches finish with DONE, RPCs with DONEPROC (statements inside with DONEINPROC).
#[derive(Clone, Copy, PartialEq)]
enum Reply {
    Batch,
    Proc,
}

struct Session {
    t: Transport,
    conn: Connection,
    db: String,
    prepared: HashMap<i32, (Option<String>, String)>,
    next_handle: i32,
    tx_descriptor: u64,
    tx_counter: u64,
    in_tx: bool,
}

impl Session {
    /// Append the tokens for a batch's events. Returns whether an error occurred and the number of the last one.
    fn events(&mut self, events: &[Event], reply: Reply, out: &mut Vec<u8>) -> bool {
        let more = match reply {
            Reply::Batch => 0xFD,
            Reply::Proc => 0xFF,
        };
        let mut errored = false;
        let mut i = 0;
        while i < events.len() {
            match &events[i] {
                Event::Rows(rs) => {
                    if !rs.columns.is_empty() {
                        let wires = wires_for(rs);
                        colmetadata(out, &rs.columns, &wires);
                        for row in &rs.rows {
                            out.push(0xD1);
                            for (c, w) in wires.iter().enumerate() {
                                write_value(out, w, row.get(c).unwrap_or(&Value::Null));
                            }
                        }
                        done(out, more, DONE_MORE | DONE_COUNT, rs.rows.len() as u64);
                    }
                    // the engine reports the row count of a SELECT separately; the DONE above already carries it
                    if matches!(events.get(i + 1), Some(Event::RowCount(n)) if *n == rs.rows.len() as i64) {
                        i += 1;
                    }
                }
                Event::Message(m) => info_token(out, 0, m),
                Event::RowCount(n) => done(out, more, DONE_MORE | DONE_COUNT, (*n).max(0) as u64),
                Event::Error(e) => {
                    errored = true;
                    error_token(out, e);
                }
            }
            i += 1;
        }
        errored
    }

    /// Tokens describing session changes a request caused: the current database and the transaction state.
    fn changes(&mut self, errored: bool, rolled_back_hint: bool, out: &mut Vec<u8>) {
        let db = self.conn.current_database();
        if db != self.db {
            envchange(out, 1, db.as_bytes(), self.db.as_bytes(), false);
            info_token(out, 5701, &format!("Changed database context to '{}'.", db));
            self.db = db;
        }
        let open = self.conn.transaction_count() > 0;
        if open && !self.in_tx {
            self.tx_counter += 1;
            self.tx_descriptor = self.tx_counter;
            envchange(out, 8, &self.tx_descriptor.to_le_bytes(), &[], true);
        } else if !open && self.in_tx {
            let kind = if errored || rolled_back_hint { 10 } else { 9 };
            envchange(out, kind, &[], &self.tx_descriptor.to_le_bytes(), true);
            self.tx_descriptor = 0;
        }
        self.in_tx = open;
    }

    fn finish(&mut self, mut out: Vec<u8>, errored: bool, reply: Reply) -> Vec<u8> {
        let mut status = if errored { DONE_ERROR } else { 0 };
        if self.in_tx {
            status |= DONE_INXACT;
        }
        done(&mut out, if reply == Reply::Batch { 0xFD } else { 0xFE }, status, 0);
        out
    }

    fn batch(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        let mut rd = Rd::new(payload);
        rd.all_headers()?;
        let sql = rd.utf16(rd.left() / 2)?;
        let res = self.conn.execute(&sql);
        let mut out = vec![];
        let errored = self.events(&res.events, Reply::Batch, &mut out);
        self.changes(errored, sql.to_ascii_lowercase().contains("rollback"), &mut out);
        Ok(self.finish(out, errored, Reply::Batch))
    }

    /// Run the batch for one RPC and write its tokens; returns whether it failed.
    fn run_plan(&mut self, plan: &rpc::Plan, params: &[rpc::Param], out: &mut Vec<u8>, status_override: Option<i32>) -> bool {
        let mut res: BatchResult = self.conn.execute(&plan.sql);
        // the trailer is the last result set whose first column is the marker
        let mut trailer: Option<Vec<Value>> = None;
        if let Some(pos) = res.events.iter().rposition(|e| matches!(e, Event::Rows(rs) if rs.columns.first().map(|c| c == rpc::TRAILER).unwrap_or(false))) {
            if let Event::Rows(rs) = res.events.remove(pos) {
                trailer = rs.rows.into_iter().next();
            }
            if matches!(res.events.get(pos), Some(Event::RowCount(1))) {
                res.events.remove(pos);
            }
        }
        let errored = self.events(&res.events, Reply::Proc, out);
        if !errored {
            if let Some(t) = &trailer {
                let status = status_override.unwrap_or_else(|| if plan.status { as_i32(&t[0]) } else { 0 });
                out.push(0x79);
                out.extend_from_slice(&status.to_le_bytes());
                for (k, &pi) in plan.outs.iter().enumerate() {
                    let p = &params[pi];
                    return_value(out, pi as u16, &p.name, &p.ti.w, t.get(k + 1).unwrap_or(&Value::Null));
                }
            }
        }
        errored
    }

    fn rpc(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        let calls = rpc::parse(payload)?;
        let mut out = vec![];
        let mut errored = false;
        let mut hint = false;
        for call in calls {
            let e = match self.one_rpc(&call, &mut out) {
                Ok(e) => e,
                Err(msg) => {
                    error_token(&mut out, &SqlError::new(8144, msg));
                    true
                }
            };
            errored |= e;
            hint |= matches!(&call.proc, Proc::Name(n) if n.to_ascii_lowercase().contains("rollback"));
        }
        self.changes(errored, hint, &mut out);
        Ok(self.finish(out, errored, Reply::Proc))
    }

    fn one_rpc(&mut self, call: &rpc::Rpc, out: &mut Vec<u8>) -> R<bool> {
        let id = match &call.proc {
            Proc::Id(i) => *i,
            Proc::Name(n) => match n.trim().trim_start_matches("master..").trim_start_matches("dbo.").to_ascii_lowercase().as_str() {
                "sp_executesql" => rpc::SP_EXECUTESQL,
                "sp_prepare" => rpc::SP_PREPARE,
                "sp_execute" => rpc::SP_EXECUTE,
                "sp_prepexec" => rpc::SP_PREPEXEC,
                "sp_unprepare" => rpc::SP_UNPREPARE,
                "sp_reset_connection" => {
                    self.conn.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION");
                    return Ok(false);
                }
                _ => 0,
            },
        };
        let ps = &call.params;
        match id {
            0 => {
                let Proc::Name(n) = &call.proc else { unreachable!() };
                let plan = rpc::plan_named(n, ps)?;
                Ok(self.run_plan(&plan, ps, out, None))
            }
            rpc::SP_EXECUTESQL => {
                let stmt = rpc::text_param(ps, 0, "@stmt")?.ok_or("@stmt must not be NULL")?;
                let decl = rpc::text_param(ps, 1, "@params").unwrap_or(None);
                let args: Vec<usize> = (2..ps.len()).collect();
                let plan = rpc::plan_executesql(&stmt, decl.as_deref(), ps, &args)?;
                Ok(self.run_plan(&plan, ps, out, Some(0)))
            }
            rpc::SP_PREPARE | rpc::SP_PREPEXEC => {
                let decl = rpc::text_param(ps, 1, "@params")?;
                let stmt = rpc::text_param(ps, 2, "@stmt")?.ok_or("@stmt must not be NULL")?;
                self.next_handle += 1;
                let handle = self.next_handle;
                self.prepared.insert(handle, (decl.clone(), stmt.clone()));
                let mut errored = false;
                if id == rpc::SP_PREPEXEC {
                    let args: Vec<usize> = (3..ps.len()).collect();
                    let plan = rpc::plan_executesql(&stmt, decl.as_deref(), ps, &args)?;
                    errored = self.run_plan(&plan, ps, out, Some(0));
                } else {
                    out.push(0x79);
                    out.extend_from_slice(&0i32.to_le_bytes());
                }
                if !errored {
                    let name = ps.first().map(|p| p.name.as_str()).unwrap_or("@handle");
                    return_value(out, 0, name, &Wire::Int(4), &Value::Int(handle));
                } else {
                    self.prepared.remove(&handle);
                }
                Ok(errored)
            }
            rpc::SP_EXECUTE => {
                let handle = match ps.first().map(|p| &p.val) {
                    Some(Value::Int(h)) => *h,
                    _ => return Err("@handle must be an int".into()),
                };
                let (decl, stmt) = self.prepared.get(&handle).cloned().ok_or_else(|| format!("The prepared statement handle {} is not valid in this context.", handle))?;
                let args: Vec<usize> = (1..ps.len()).collect();
                let plan = rpc::plan_executesql(&stmt, decl.as_deref(), ps, &args)?;
                Ok(self.run_plan(&plan, ps, out, Some(0)))
            }
            rpc::SP_UNPREPARE => {
                if let Some(Value::Int(h)) = ps.first().map(|p| &p.val) {
                    self.prepared.remove(h);
                }
                out.push(0x79);
                out.extend_from_slice(&0i32.to_le_bytes());
                Ok(false)
            }
            other => Err(format!("The remote procedure call (system procedure id {}) is not supported; server cursors are not implemented.", other)),
        }
    }

    fn tm(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        let mut rd = Rd::new(payload);
        rd.all_headers()?;
        let kind = rd.u16()?;
        let sql = match kind {
            5 => {
                let level = if rd.left() > 0 { rd.u8()? } else { 0 };
                let mut sql = String::new();
                if let Some(l) = [None, Some("READ UNCOMMITTED"), Some("READ COMMITTED"), Some("REPEATABLE READ"), Some("SERIALIZABLE"), Some("SNAPSHOT")].get(level as usize).copied().flatten() {
                    sql.push_str(&format!("SET TRANSACTION ISOLATION LEVEL {}; ", l));
                }
                sql.push_str("BEGIN TRANSACTION");
                sql
            }
            7 | 8 | 9 => {
                let name = if rd.left() > 0 { rd.bvarchar()? } else { String::new() };
                let name = if name.chars().all(|c| c.is_alphanumeric() || c == '_') { name } else { return Err("invalid transaction name".into()) };
                match kind {
                    7 => "COMMIT TRANSACTION".to_string(),
                    8 if name.is_empty() => "ROLLBACK TRANSACTION".to_string(),
                    8 => format!("ROLLBACK TRANSACTION {}", name),
                    _ => format!("SAVE TRANSACTION {}", name),
                }
            }
            6 => "SELECT 1".to_string(), // promote: no distributed transactions here
            _ => return Err(format!("unsupported transaction manager request {}", kind)),
        };
        let res = self.conn.execute(&sql);
        let mut out = vec![];
        let errored = res.first_error().map(|e| error_token(&mut out, e)).is_some();
        self.changes(errored, kind == 8 && sql == "ROLLBACK TRANSACTION", &mut out);
        Ok(self.finish(out, errored, Reply::Batch))
    }
}

fn as_i32(v: &Value) -> i32 {
    match v {
        Value::Int(i) => *i,
        Value::BigInt(i) => *i as i32,
        _ => 0,
    }
}

fn return_value(out: &mut Vec<u8>, ordinal: u16, name: &str, w: &Wire, v: &Value) {
    out.push(0xAC);
    out.extend_from_slice(&ordinal.to_le_bytes());
    put_bvar(out, name);
    out.push(1); // output parameter
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0x0001u16.to_le_bytes());
    write_typeinfo(out, w);
    write_value(out, w, v);
}

// ----------------------------------------------------------------------------- pre-login and login
fn prelogin_reply() -> Vec<u8> {
    // options: VERSION, ENCRYPTION (not supported), INSTOPT, MARS (off), terminator
    let table_len = 4 * 5 + 1;
    let mut table = vec![];
    let mut data = vec![];
    let mut add = |token: u8, bytes: &[u8]| {
        table.push(token);
        table.extend_from_slice(&((table_len + data.len()) as u16).to_be_bytes());
        table.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        data.extend_from_slice(bytes);
    };
    add(0, &[16, 0, 0x03, 0xE8, 0, 0]);
    add(1, &[2]);
    add(2, &[0]);
    add(4, &[0]);
    table.push(0xFF);
    table.extend(data);
    table
}

struct Login {
    tds_version: u32,
    packet_size: usize,
    user: String,
    password: String,
    database: String,
    sspi: bool,
}

fn parse_login7(p: &[u8]) -> R<Login> {
    let mut rd = Rd::new(p);
    rd.u32()?;
    let tds_version = rd.u32()?;
    let packet_size = rd.u32()? as usize;
    rd.take(36 - 12)?;
    // offset table: (offset, length-in-characters) pairs
    fn pair(rd: &mut Rd) -> R<(usize, usize)> {
        Ok((rd.u16()? as usize, rd.u16()? as usize))
    }
    let _host = pair(&mut rd)?;
    let user = pair(&mut rd)?;
    let pass = pair(&mut rd)?;
    let _app = pair(&mut rd)?;
    let _server = pair(&mut rd)?;
    let _ext = pair(&mut rd)?;
    let _clt = pair(&mut rd)?;
    let _lang = pair(&mut rd)?;
    let db = pair(&mut rd)?;
    rd.take(6)?; // client id
    let sspi = pair(&mut rd)?;
    let text = |(off, n): (usize, usize)| -> R<Vec<u8>> {
        let end = off + n * 2;
        p.get(off..end).map(|s| s.to_vec()).ok_or_else(|| "login field out of range".to_string())
    };
    // the password is obfuscated: nibbles swapped, then XOR 0xA5
    let pw: Vec<u8> = text(pass)?.into_iter().map(|b| (b ^ 0xA5).rotate_left(4)).collect();
    Ok(Login { tds_version, packet_size, user: decode_utf16(&text(user)?), password: decode_utf16(&pw), database: decode_utf16(&text(db)?), sspi: sspi.1 > 0 })
}

fn login_failure(t: &mut Transport, number: i32, state: u8, class: u8, text: &str) -> io::Result<()> {
    let mut out = vec![];
    message_token(&mut out, 0xAA, number, state, class, text, "");
    done(&mut out, 0xFD, DONE_ERROR, 0);
    t.send(T_REPLY, &out)
}

fn handshake(server: &Server, mut t: Transport, cfg: &Config) -> io::Result<Option<Session>> {
    let bad = |e: String| io::Error::new(io::ErrorKind::InvalidData, e);
    let (kind, _, _) = t.read_message()?;
    if kind != T_PRELOGIN {
        return Ok(None);
    }
    t.send(T_REPLY, &prelogin_reply())?;
    let (kind, _, payload) = t.read_message()?;
    if kind == T_SSPI || kind != T_LOGIN7 {
        return Ok(None);
    }
    let login = parse_login7(&payload).map_err(bad)?;
    if login.sspi {
        login_failure(&mut t, 18452, 1, 14, "Login failed. Integrated (Windows) authentication is not supported; use a SQL Server login.")?;
        return Ok(None);
    }
    if login.tds_version < 0x7200_0000 {
        login_failure(&mut t, 18456, 1, 14, "This server requires TDS 7.2 or later.")?;
        return Ok(None);
    }
    let conn = match server.connect_as(&login.user, &login.password) {
        Ok(c) => c,
        Err(_) => {
            login_failure(&mut t, 18456, 1, 14, &format!("Login failed for user '{}'.", login.user))?;
            return Ok(None);
        }
    };
    let mut s = Session { t, conn, db: String::new(), prepared: HashMap::new(), next_handle: 0, tx_descriptor: 0, tx_counter: 0, in_tx: false };
    if !login.database.is_empty() {
        let r = s.conn.execute(&format!("USE {}", crate::tools::bracket(&login.database)));
        if r.first_error().is_some() {
            let mut out = vec![];
            message_token(&mut out, 0xAA, 4060, 1, 11, &format!("Cannot open database \"{}\" requested by the login. The login failed.", login.database), "");
            message_token(&mut out, 0xAA, 18456, 1, 14, &format!("Login failed for user '{}'.", login.user), "");
            done(&mut out, 0xFD, DONE_ERROR, 0);
            s.t.send(T_REPLY, &out)?;
            return Ok(None);
        }
    }
    if let Ok(rows) = s.conn.query("SELECT @@SPID") {
        if let Some(Value::Int(n)) = rows.first().and_then(|r| r.first()) {
            s.t.spid = *n as u16;
        }
    }
    let packet = if (512..=32767).contains(&login.packet_size) { login.packet_size } else { DEFAULT_PACKET }.min(cfg.max_packet);
    s.db = s.conn.current_database();
    let mut out = vec![];
    envchange(&mut out, 1, s.db.as_bytes(), b"master", false);
    info_token(&mut out, 5701, &format!("Changed database context to '{}'.", s.db));
    envchange(&mut out, 2, b"us_english", b"", false);
    info_token(&mut out, 5703, "Changed language setting to us_english.");
    envchange(&mut out, 7, &COLLATION, &[], true);
    envchange(&mut out, 4, packet.to_string().as_bytes(), DEFAULT_PACKET.to_string().as_bytes(), false);
    // LOGINACK
    let mut ack = vec![1u8];
    ack.extend_from_slice(&TDS_74.to_be_bytes());
    put_bvar(&mut ack, "Microsoft SQL Server");
    ack.extend_from_slice(&[16, 0, 0x03, 0xE8]);
    out.push(0xAD);
    out.extend_from_slice(&(ack.len() as u16).to_le_bytes());
    out.extend_from_slice(&ack);
    done(&mut out, 0xFD, 0, 0);
    s.t.send(T_REPLY, &out)?;
    s.t.packet_size = packet;
    Ok(Some(s))
}

fn serve_connection(server: Server, stream: TcpStream, cfg: Arc<Config>) {
    let _ = stream.set_nodelay(true);
    // a client that connects and says nothing must not hold a thread forever
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let t = Transport { stream, packet_size: DEFAULT_PACKET, spid: 0 };
    let mut s = match handshake(&server, t, &cfg) {
        Ok(Some(s)) => s,
        _ => return,
    };
    let _ = s.t.stream.set_read_timeout(None);
    loop {
        let (kind, status, payload) = match s.t.read_message() {
            Ok(m) => m,
            Err(_) => return,
        };
        if status & 0x08 != 0 {
            // RESETCONNECTION: end any open transaction
            s.conn.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION");
            let mut sink = vec![];
            s.changes(true, true, &mut sink);
        }
        let reply = match kind {
            T_BATCH => s.batch(&payload),
            T_RPC => s.rpc(&payload),
            T_TM => s.tm(&payload),
            T_ATTN => {
                let mut out = vec![];
                done(&mut out, 0xFD, DONE_ATTN, 0);
                Ok(out)
            }
            T_BULK => {
                let mut out = vec![];
                error_token(&mut out, &SqlError::new(4004, "Bulk load is not supported by this server."));
                done(&mut out, 0xFD, DONE_ERROR, 0);
                Ok(out)
            }
            _ => return,
        };
        let out = reply.unwrap_or_else(|msg| {
            let mut out = vec![];
            error_token(&mut out, &SqlError::new(8144, format!("The request could not be processed: {}", msg)));
            done(&mut out, 0xFD, DONE_ERROR, 0);
            out
        });
        if s.t.send(T_REPLY, &out).is_err() {
            return;
        }
    }
}

// ----------------------------------------------------------------------------- the listener
/// Listener settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Largest packet size granted to clients.
    pub max_packet: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config { max_packet: 32767 }
    }
}

struct Registered {
    id: u64,
    clients: Arc<Mutex<HashMap<u64, TcpStream>>>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        if let Some(c) = self.clients.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id) {
            let _ = c.shutdown(Shutdown::Both);
        }
    }
}

/// A running TDS listener (one thread per client connection).
pub struct Listener {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    clients: Arc<Mutex<HashMap<u64, TcpStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Listener {
    /// Start listening on `addr` (use port 0 to let the OS choose; see [`Listener::local_addr`]).
    pub fn bind(server: Server, addr: impl ToSocketAddrs, cfg: Config) -> io::Result<Listener> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let clients: Arc<Mutex<HashMap<u64, TcpStream>>> = Default::default();
        let cfg = Arc::new(cfg);
        let (stop2, clients2) = (stop.clone(), clients.clone());
        let next_id = Arc::new(AtomicU64::new(0));
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let id = next_id.fetch_add(1, Ordering::SeqCst);
                if let Ok(c) = stream.try_clone() {
                    clients2.lock().unwrap().insert(id, c);
                }
                let (server, cfg, clients) = (server.clone(), cfg.clone(), clients2.clone());
                std::thread::spawn(move || {
                    // closes the socket however the thread ends, even by a panic (the registry holds a clone of it)
                    let _registered = Registered { id, clients };
                    // the session (and its Connection) lives entirely on this thread
                    serve_connection(server, stream, cfg);
                });
            }
        });
        Ok(Listener { addr: local, stop, clients, thread: Some(thread) })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Block until the listener stops (normally never).
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Stop accepting and disconnect every client.
    pub fn shutdown(mut self) {
        self.stop_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn stop_all(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr); // wake the accept loop
        for (_, c) in self.clients.lock().unwrap().drain() {
            let _ = c.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop_all();
        }
    }
}
