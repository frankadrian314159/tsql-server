//! A TDS 7.4 listener, so SQL Server clients (sqlcmd, SSMS-less tools, ODBC, JDBC, ADO.NET, tiberius, ...) can talk
//! to a [`Server`] over TCP.
//!
//! Supported: pre-login, SQL authentication (Login7), SQL batches, RPC requests (`sp_executesql`,
//! `sp_prepare`/`sp_execute`/`sp_prepexec`/`sp_unprepare`, the server cursor procedures `sp_cursor*` (keyset and updatable cursors
//! included), table-valued parameters, and calls to stored
//! procedures, with OUTPUT parameters and return status), bulk load (`INSERT BULK` and the bulk-load message),
//! transaction-manager requests (begin/commit/rollback/save), attention (cancel), the usual data types (`sql_variant`,
//! `hierarchyid`, spatial and the legacy `text`/`ntext`/`image` among them), and TLS 1.2 (1.3 on request; see [`TlsConfig`];
//! the login is always encrypted when the client supports TLS, the whole session when the client asks for it or the
//! listener requires it), and MARS (the session multiplex protocol, see `mars.rs`; requests that only read run in parallel, see `mars_par.rs`). Not
//! supported: group-based Windows logins (see `auth.rs` for the Azure AD and Kerberos logins that are), DYNAMIC server cursors (served as keyset cursors, which do not see rows others insert).
pub mod auth;
mod bulk;
mod cursor;
#[cfg(all(unix, any(feature = "kerberos-linux", feature = "kerberos-macos")))]
mod gssapi;
mod mars;
mod mars_par;
#[cfg(all(windows, feature = "kerberos-windows"))]
mod sspi;
mod rpc;
mod tls;
pub mod wire;

pub use auth::{AadConfig, AuthConfig, JwksSource, KerberosConfig};
pub use tls::{Encryption, TlsConfig};

use crate::{Connection, Event, Server, SqlError, Streamed};
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
const T_FEDAUTH: u8 = 8;
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
    stream: tls::Wire,
    packet_size: usize,
    spid: u16,
    /// Bytes of a reply that is being streamed and have not been sent yet (see `send_part`).
    pending: Vec<u8>,
    /// Packet id of the next packet of the reply being streamed.
    next_id: u8,
    /// Present when the session uses MARS: every packet then travels inside an SMP frame.
    mux: Option<mars::Mux>,
}

impl Transport {
    /// Write whole packets to the client (to the current MARS session, when there is one).
    fn out(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.mux.is_some() {
            return self.mars_emit(buf);
        }
        self.stream.write_all(buf)?;
        self.stream.flush()
    }

    /// Carry on inside the TLS connection `conn` (whose handshake is complete).
    fn go_tls(&mut self, conn: rustls::ServerConnection) -> io::Result<()> {
        let sock = self.stream.tcp().try_clone()?;
        self.stream = tls::Wire::Tls(Box::new(rustls::StreamOwned::new(conn, sock)));
        Ok(())
    }

    /// Leave TLS (after the Login7 message of a session that only encrypts the login); the socket carries plain TDS.
    fn go_plain(&mut self) -> io::Result<()> {
        let sock = self.stream.tcp().try_clone()?;
        self.stream = tls::Wire::Plain(sock);
        Ok(())
    }

    /// Read one packet: its type, status byte, payload, and whether it ends the message.
    fn read_packet(&mut self) -> io::Result<(u8, u8, Vec<u8>, bool)> {
        let mut h = [0u8; 8];
        self.stream.read_exact(&mut h)?;
        let len = u16::from_be_bytes([h[2], h[3]]) as usize;
        if len < 8 || !matches!(h[0], 1..=8 | 14 | 16 | 17 | 18) || h[1] > 0x1F {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a TDS packet"));
        }
        let mut data = vec![0u8; len - 8];
        self.stream.read_exact(&mut data)?;
        Ok((h[0], h[1], data, h[1] & 1 != 0))
    }

    /// Read one complete message (packets up to and including the one marked end-of-message).
    fn read_message(&mut self) -> io::Result<(u8, u8, Vec<u8>)> {
        match self.read_request()? {
            Request::Message(kind, status, data) => Ok((kind, status, data)),
            Request::Bulk(first, _) => {
                // a bulk load message where something else was expected: read it whole
                let mut data = first;
                loop {
                    let (_, _, d, eom) = self.read_packet()?;
                    data.extend_from_slice(&d);
                    if eom {
                        return Ok((T_BULK, 0, data));
                    }
                }
            }
        }
    }

    /// Read the next request. A message is read whole, except a bulk load, which can be far bigger than memory and is
    /// handed over after its first packet to be read as it is decoded.
    fn read_request(&mut self) -> io::Result<Request> {
        if self.mux.is_some() {
            let (_, kind, status, data) = self.mars_request()?;
            return Ok(Request::Message(kind, status, data));
        }
        let (kind, status, mut data, mut eom) = self.read_packet()?;
        if kind == T_BULK && !eom {
            return Ok(Request::Bulk(data, false));
        }
        while !eom {
            let (_, _, d, e) = self.read_packet()?;
            data.extend_from_slice(&d);
            eom = e;
            if data.len() > MAX_MESSAGE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
            }
        }
        Ok(Request::Message(kind, status, data))
    }

    /// Send the full packets of a reply that is not finished yet (the rest waits for more data or for `send`), so a
    /// big result goes out as it is produced.
    fn send_part(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(payload);
        let room = self.packet_size - 8;
        let mut buf = vec![];
        // always keep something back, so that the last packet of the reply can be marked end-of-message
        while self.pending.len() > room {
            buf.push(kind);
            buf.push(0);
            buf.extend_from_slice(&((room + 8) as u16).to_be_bytes());
            buf.extend_from_slice(&self.spid.to_be_bytes());
            buf.push(self.next_id);
            buf.push(0);
            buf.extend(self.pending.drain(..room));
            self.next_id = self.next_id.wrapping_add(1);
        }
        if buf.is_empty() {
            return Ok(());
        }
        self.out(&buf)
    }

    /// Send the rest of a reply (all of it, when `send_part` was not used).
    fn send(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let room = self.packet_size - 8;
        let mut id = if self.pending.is_empty() && self.next_id == 0 { 1 } else { self.next_id.max(1) };
        let joined;
        let payload: &[u8] = if self.pending.is_empty() {
            payload
        } else {
            self.pending.extend_from_slice(payload);
            joined = std::mem::take(&mut self.pending);
            &joined
        };
        self.next_id = 0;
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
        self.out(&buf)
    }
}

/// A request as `Transport::read_request` hands it over.
enum Request {
    /// A whole message: type, status of its first packet, payload.
    Message(u8, u8, Vec<u8>),
    /// The first packet of a bulk load message (and whether it is the last).
    Bulk(Vec<u8>, bool),
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
    colmetadata_flags(out, names, wires, &[]);
}

/// As [`colmetadata`], with the flags of each column (the default is "nullable, update status unknown"): bit 0 nullable,
/// bits 2-3 whether the column can be updated (0 no, 1 yes, 2 unknown).
fn colmetadata_flags(out: &mut Vec<u8>, names: &[String], wires: &[Wire], flags: &[u16]) {
    out.push(0x81);
    out.extend_from_slice(&(names.len() as u16).to_le_bytes());
    for (i, (n, w)) in names.iter().zip(wires).enumerate() {
        out.extend_from_slice(&0u32.to_le_bytes()); // user type
        out.extend_from_slice(&flags.get(i).copied().unwrap_or(0x0009).to_le_bytes());
        write_typeinfo(out, w);
        write_table_name(out, w);
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

// ----------------------------------------------------------------------------- streaming replies
/// Turns the output of `Connection::execute_streaming` into TDS tokens, sending packets to the client as they fill.
struct Live {
    reply: Reply,
    wires: Vec<Wire>,
    /// Row count of the result set the engine has just finished, so the row count it reports next is not repeated.
    skip_count: Option<i64>,
    /// The last result set of an RPC that carries OUTPUT parameters and the return status (see `rpc::TRAILER`).
    trailer: Option<Vec<Value>>,
    errored: bool,
}

/// Tokens are collected until this many bytes, then sent.
const FLUSH_AT: usize = 32 << 10;

impl Live {
    fn new(reply: Reply) -> Live {
        Live { reply, wires: vec![], skip_count: None, trailer: None, errored: false }
    }

    /// Handle one item of engine output; returns false when the client can no longer be written to.
    fn on(&mut self, item: Streamed, out: &mut Vec<u8>, t: Option<&mut Transport>) -> bool {
        let more = match self.reply {
            Reply::Batch => 0xFD,
            Reply::Proc => 0xFF,
        };
        let skip = self.skip_count.take();
        match item {
            Streamed::Event(Event::Rows(rs)) => {
                if rs.columns.first().map(|c| c == rpc::TRAILER).unwrap_or(false) {
                    self.skip_count = Some(rs.rows.len() as i64);
                    self.trailer = rs.rows.into_iter().next();
                } else if !rs.columns.is_empty() {
                    let wires = wires_for(&rs);
                    colmetadata(out, &rs.columns, &wires);
                    for row in &rs.rows {
                        out.push(0xD1);
                        for (c, w) in wires.iter().enumerate() {
                            write_value(out, w, row.get(c).unwrap_or(&Value::Null));
                        }
                    }
                    done(out, more, DONE_MORE | DONE_COUNT, rs.rows.len() as u64);
                    self.skip_count = Some(rs.rows.len() as i64);
                }
            }
            Streamed::Event(Event::Message(m)) => info_token(out, 0, &m),
            Streamed::Event(Event::RowCount(n)) => {
                if skip != Some(n) {
                    done(out, more, DONE_MORE | DONE_COUNT, n.max(0) as u64);
                }
            }
            Streamed::Event(Event::Error(e)) => {
                self.errored = true;
                error_token(out, &e);
            }
            Streamed::Header { columns, types, inferred } => {
                let probe = crate::ResultSet { columns: columns.clone(), types, rows: vec![] };
                let mut wires = wires_for(&probe);
                // the type of an expression was guessed from the first rows: a string column may hold wider or
                // non-code-page text later, so it is sent as nvarchar(max)
                for (w, guessed) in wires.iter_mut().zip(inferred.iter()) {
                    if *guessed && matches!(w, Wire::VarChar(_) | Wire::Char(_) | Wire::NVarChar(_) | Wire::NChar(_)) {
                        *w = Wire::NVarChar(PLP);
                    }
                }
                colmetadata(out, &columns, &wires);
                self.wires = wires;
            }
            Streamed::Row(row) => {
                out.push(0xD1);
                for (c, w) in self.wires.iter().enumerate() {
                    write_value(out, w, row.get(c).unwrap_or(&Value::Null));
                }
                if out.len() >= FLUSH_AT {
                    // (a reply that is built without a transport is kept whole, and sent when it is finished)
                    if let Some(t) = t {
                        if t.send_part(T_REPLY, out).is_err() {
                            return false;
                        }
                        out.clear();
                    }
                }
            }
            Streamed::End(n) => {
                done(out, more, DONE_MORE | DONE_COUNT, n);
                self.skip_count = Some(n as i64);
            }
        }
        true
    }
}

// ----------------------------------------------------------------------------- the session
/// Run the batch of an RPC on `conn` and write its tokens (and, with a transport, send them as they fill); returns whether it
/// failed.
fn run_plan_on(conn: &mut Connection, mut t: Option<&mut Transport>, plan: &rpc::Plan, params: &[rpc::Param], out: &mut Vec<u8>, status_override: Option<i32>) -> bool {
    // the trailer is the last result set whose first column is the marker; `Live` keeps it apart
    if tracing() {
        trace(&format!("rpc batch: {:?}", plan.sql));
    }
    let mut live = Live::new(Reply::Proc);
    conn.execute_streaming(&plan.sql, &mut |item| live.on(item, out, t.as_deref_mut()));
    let errored = live.errored;
    if !errored {
        if let Some(tr) = &live.trailer {
            let status = status_override.unwrap_or_else(|| if plan.status { as_i32(&tr[0]) } else { 0 });
            out.push(0x79);
            out.extend_from_slice(&status.to_le_bytes());
            for (k, &pi) in plan.outs.iter().enumerate() {
                let p = &params[pi];
                return_value(out, pi as u16, &p.name, &p.ti.w, tr.get(k + 1).unwrap_or(&Value::Null));
            }
        }
    }
    errored
}

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
    /// The target named by `INSERT BULK`, waiting for the data message that follows it.
    bulk: Option<bulk::BulkTarget>,
    /// The server cursors this connection has open, by handle.
    cursors: HashMap<i32, cursor::Cursor>,
    next_cursor: i32,
}

impl Session {
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
        if tracing() {
            trace(&format!("batch: {:?}", sql));
        }
        if let Some(target) = bulk::parse_insert_bulk(&sql) {
            return Ok(self.insert_bulk(target));
        }
        let mut out = vec![];
        let mut live = Live::new(Reply::Batch);
        {
            // rows of a big result go to the client while the statement is still running
            let Session { t, conn, .. } = self;
            conn.execute_streaming(&sql, &mut |item| live.on(item, &mut out, Some(&mut *t)));
        }
        let errored = live.errored;
        self.changes(errored, sql.to_ascii_lowercase().contains("rollback"), &mut out);
        Ok(self.finish(out, errored, Reply::Batch))
    }

    /// Run the batch for one RPC and write its tokens; returns whether it failed.
    fn run_plan(&mut self, plan: &rpc::Plan, params: &[rpc::Param], out: &mut Vec<u8>, status_override: Option<i32>) -> bool {
        let Session { t, conn, .. } = self;
        run_plan_on(conn, Some(t), plan, params, out, status_override)
    }

    fn rpc(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        let calls = rpc::parse(payload)?;
        let mut out = vec![];
        let mut errored = false;
        let mut hint = false;
        for call in calls {
            if tracing() {
                let name = match &call.proc {
                    Proc::Id(i) => format!("system procedure {}", i),
                    Proc::Name(n) => n.clone(),
                };
                let ps: Vec<String> = call.params.iter().map(|p| format!("{}{}={:?}", p.name, if p.out { "(out)" } else { "" }, p.val)).collect();
                trace(&format!("rpc: {} ({})", name, ps.join(", ")));
            }
            let before = out.len();
            let e = match self.one_rpc(&call, &mut out) {
                Ok(e) => e,
                Err(msg) => {
                    error_token(&mut out, &SqlError::new(8144, msg));
                    true
                }
            };
            if tracing() && matches!(&call.proc, Proc::Id(1..=9)) {
                trace(&format!("rpc reply: {}", out[before..].iter().take(400).map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")));
            }
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
                "sp_cursor" => rpc::SP_CURSOR,
                "sp_cursoropen" => rpc::SP_CURSOROPEN,
                "sp_cursorprepare" => rpc::SP_CURSORPREPARE,
                "sp_cursorexecute" => rpc::SP_CURSOREXECUTE,
                "sp_cursorprepexec" => rpc::SP_CURSORPREPEXEC,
                "sp_cursorunprepare" => rpc::SP_CURSORUNPREPARE,
                "sp_cursorfetch" => rpc::SP_CURSORFETCH,
                "sp_cursoroption" => rpc::SP_CURSOROPTION,
                "sp_cursorclose" => rpc::SP_CURSORCLOSE,
                "sp_reset_connection" => {
                    self.conn.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION");
                    self.cursors.clear();
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
                Ok(self.run_plan(&plan, ps, out, None))
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
                    errored = self.run_plan(&plan, ps, out, None);
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
                Ok(self.run_plan(&plan, ps, out, None))
            }
            rpc::SP_UNPREPARE => {
                if let Some(Value::Int(h)) = ps.first().map(|p| &p.val) {
                    self.prepared.remove(h);
                }
                out.push(0x79);
                out.extend_from_slice(&0i32.to_le_bytes());
                Ok(false)
            }
            1..=9 => self.cursor_rpc(id, ps, out),
            other => Err(format!("The remote procedure call (system procedure id {}) is not supported.", other)),
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
/// The ENCRYPTION option of a pre-login message (0 off, 1 on, 2 not supported, 3 required), if there is one.
fn prelogin_encryption(p: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i < p.len() && p[i] != 0xFF {
        let entry = p.get(i..i + 5)?;
        if entry[0] == 1 {
            let off = u16::from_be_bytes([entry[1], entry[2]]) as usize;
            return p.get(off).copied();
        }
        i += 5;
    }
    None
}

/// What the server answers to the client's encryption request: 0 login only, 1 whole session, 2 none, 3 required
/// (the client must then give up if it cannot encrypt).
fn encryption_answer(client: u8, tls: bool, mode: Encryption) -> u8 {
    if !tls {
        return 2;
    }
    match (client, mode) {
        (2, Encryption::Required) => 3,
        (2, _) => 2,
        (_, Encryption::Required) => 1,
        (1 | 3, _) => 1,
        _ => 0,
    }
}

/// Whether the client's pre-login message asks for MARS (option 4).
fn prelogin_mars(p: &[u8]) -> bool {
    let mut i = 0;
    while i < p.len() && p[i] != 0xFF {
        let Some(entry) = p.get(i..i + 5) else { return false };
        if entry[0] == 4 {
            let off = u16::from_be_bytes([entry[1], entry[2]]) as usize;
            return p.get(off).copied() == Some(1);
        }
        i += 5;
    }
    false
}

/// Whether the client's pre-login message announces federated authentication (option 6, FEDAUTHREQUIRED).
fn prelogin_fedauth(p: &[u8]) -> bool {
    let mut i = 0;
    while i < p.len() && p[i] != 0xFF {
        let Some(entry) = p.get(i..i + 5) else { return false };
        if entry[0] == 6 {
            let off = u16::from_be_bytes([entry[1], entry[2]]) as usize;
            return p.get(off).copied() == Some(1);
        }
        i += 5;
    }
    false
}

fn prelogin_reply(encryption: u8, mars: bool, fedauth: bool) -> Vec<u8> {
    // options: VERSION, ENCRYPTION, INSTOPT, MARS, terminator
    let table_len = 5 * (4 + fedauth as usize) + 1;
    let mut table = vec![];
    let mut data = vec![];
    let mut add = |token: u8, bytes: &[u8]| {
        table.push(token);
        table.extend_from_slice(&((table_len + data.len()) as u16).to_be_bytes());
        table.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        data.extend_from_slice(bytes);
    };
    add(0, &[16, 0, 0x03, 0xE8, 0, 0]);
    add(1, &[encryption]);
    add(2, &[0]);
    add(4, &[mars as u8]);
    if fedauth {
        add(6, &[1]);
    }
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
    /// The client's first SSPI token (Kerberos or NTLM), when it asks for integrated authentication.
    sspi_token: Vec<u8>,
    /// The FEDAUTH feature (Azure AD), when there is one.
    fedauth: Option<FedAuth>,
    /// OptionFlags2 `fODBC` or TypeFlags `fOLEDB`: the driver wants the ANSI defaults.
    ansi_defaults: bool,
}

/// The FEDAUTH feature of Login7.
struct FedAuth {
    /// 1: the client brings an access token; 2: it wants to be told where to get one (ADAL/MSAL).
    library: u8,
    token: Option<Vec<u8>>,
}

fn parse_feature_ext(p: &[u8], mut at: usize) -> Option<FedAuth> {
    let mut found = None;
    loop {
        let id = *p.get(at)?;
        if id == 0xFF {
            return found;
        }
        let len = u32::from_le_bytes(p.get(at + 1..at + 5)?.try_into().ok()?) as usize;
        let data = p.get(at + 5..at + 5 + len)?;
        if id == 2 && !data.is_empty() {
            let library = data[0] >> 1;
            let token = if library == 1 && data.len() >= 5 {
                let n = u32::from_le_bytes(data[1..5].try_into().ok()?) as usize;
                data.get(5..5 + n).map(|t| t.to_vec())
            } else {
                None
            };
            found = Some(FedAuth { library, token });
        }
        at += 5 + len;
    }
}

fn parse_login7(p: &[u8]) -> R<Login> {
    let mut rd = Rd::new(p);
    rd.u32()?;
    let tds_version = rd.u32()?;
    let packet_size = rd.u32()? as usize;
    rd.take(24 - 12)?;
    let _flags1 = rd.u8()?;
    let flags2 = rd.u8()?;
    let type_flags = rd.u8()?;
    let flags3 = rd.u8()?;
    rd.take(36 - 28)?;
    // offset table: (offset, length-in-characters) pairs
    fn pair(rd: &mut Rd) -> R<(usize, usize)> {
        Ok((rd.u16()? as usize, rd.u16()? as usize))
    }
    let _host = pair(&mut rd)?;
    let user = pair(&mut rd)?;
    let pass = pair(&mut rd)?;
    let _app = pair(&mut rd)?;
    let _server = pair(&mut rd)?;
    let ext = pair(&mut rd)?;
    let _clt = pair(&mut rd)?;
    let _lang = pair(&mut rd)?;
    let db = pair(&mut rd)?;
    rd.take(6)?; // client id
    let sspi = pair(&mut rd)?;
    let _atch = pair(&mut rd)?;
    let _chpw = pair(&mut rd)?;
    let sspi_long = rd.u32().unwrap_or(0) as usize;
    let text = |(off, n): (usize, usize)| -> R<Vec<u8>> {
        let end = off + n * 2;
        p.get(off..end).map(|s| s.to_vec()).ok_or_else(|| "login field out of range".to_string())
    };
    // the password is obfuscated: nibbles swapped, then XOR 0xA5
    let pw: Vec<u8> = text(pass)?.into_iter().map(|b| (b ^ 0xA5).rotate_left(4)).collect();
    let sspi_len = if sspi.1 == 0xFFFF && sspi_long > 0 { sspi_long } else { sspi.1 };
    let sspi_token = if sspi_len > 0 { p.get(sspi.0..sspi.0 + sspi_len).map(|t| t.to_vec()).unwrap_or_default() } else { vec![] };
    // OptionFlags3 bit 4: ibExtension points to a 4-byte offset of the feature block
    let fedauth = if flags3 & 0x10 != 0 && ext.1 == 4 {
        p.get(ext.0..ext.0 + 4).and_then(|b| parse_feature_ext(p, u32::from_le_bytes(b.try_into().unwrap()) as usize))
    } else {
        None
    };
    Ok(Login { tds_version, packet_size, user: decode_utf16(&text(user)?), password: decode_utf16(&pw), database: decode_utf16(&text(db)?), sspi: sspi_len > 0, sspi_token, fedauth, ansi_defaults: flags2 & 0x02 != 0 || type_flags & 0x10 != 0 })
}

fn login_failure(t: &mut Transport, number: i32, state: u8, class: u8, text: &str) -> io::Result<()> {
    let mut out = vec![];
    message_token(&mut out, 0xAA, number, state, class, text, "");
    done(&mut out, 0xFD, DONE_ERROR, 0);
    t.send(T_REPLY, &out)
}

/// With `TSQL_TDS_TRACE=1` in the environment the server prints every packet it receives and sends (type and size) and why
/// a session ends, to find out what a client that fails to connect is doing.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSQL_TDS_TRACE").is_some())
}

fn trace(msg: &str) {
    if tracing() {
        eprintln!("tsql-serve trace: {}", msg);
    }
}

/// Say why a connection is being dropped before it is logged in (otherwise the client just sees the socket close).
fn refused(why: &str) {
    eprintln!("tsql-serve: closing a connection during login: {}", why);
}

/// Report a failed external login to the client and the operator.
fn external_failure(t: &mut Transport, why: &str) -> io::Result<Option<Connection>> {
    refused(why);
    login_failure(t, 18456, 1, 14, &format!("Login failed. {}", why))?;
    Ok(None)
}

/// The FEDAUTHINFO token: where the client gets an access token (the security token service) and for which resource.
fn fedauth_info(sts: &str, spn: &str) -> Vec<u8> {
    let (sts16, spn16): (Vec<u8>, Vec<u8>) = (sts.encode_utf16().flat_map(|c| c.to_le_bytes()).collect(), spn.encode_utf16().flat_map(|c| c.to_le_bytes()).collect());
    let first = 4 + 2 * 9;
    let mut body = vec![];
    body.extend_from_slice(&2u32.to_le_bytes());
    for (id, len, off) in [(1u8, sts16.len(), first), (2u8, spn16.len(), first + sts16.len())] {
        body.push(id);
        body.extend_from_slice(&(len as u32).to_le_bytes());
        body.extend_from_slice(&(off as u32).to_le_bytes());
    }
    body.extend_from_slice(&sts16);
    body.extend_from_slice(&spn16);
    let mut out = vec![0xEE];
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

/// Login with an Azure AD access token. `Ok(None)`: refused (the client has been told).
fn aad_login(server: &Server, t: &mut Transport, cfg: &Config, fa: &FedAuth) -> io::Result<Option<Connection>> {
    let Some(aad) = &cfg.auth.aad else {
        return external_failure(t, "Azure Active Directory authentication is not enabled on this server.");
    };
    let raw = match (fa.library, &fa.token) {
        (1, Some(tok)) => tok.clone(),
        (1, None) => return external_failure(t, "The login carried no access token."),
        (2, _) => {
            // the client fetches the token itself: say where, then read it from the FEDAUTH token message
            let mut out = fedauth_info(&format!("https://login.microsoftonline.com/{}", aad.tenant), aad.audiences.first().map(|s| s.as_str()).unwrap_or("https://database.windows.net/"));
            done(&mut out, 0xFD, DONE_MORE, 0);
            t.send(T_REPLY, &out)?;
            let (kind, _, data) = t.read_message()?;
            let token = (kind == T_FEDAUTH && data.len() >= 8)
                .then(|| {
                    let n = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
                    data.get(8..8 + n).map(|x| x.to_vec())
                })
                .flatten();
            match token {
                Some(tok) => tok,
                None => return external_failure(t, "The federated authentication token message was not understood."),
            }
        }
        _ => return external_failure(t, "This federated authentication library is not supported."),
    };
    match auth::validate_aad_token(aad, &auth::token_text(&raw)) {
        Ok(names) => match server.connect_external(&names) {
            Ok(c) => Ok(Some(c)),
            Err(e) => {
                refused(&format!("Azure AD identity {:?} has no login: {}", names, e.message));
                login_failure(t, e.number, 1, 14, &e.message)?;
                Ok(None)
            }
        },
        Err(why) => external_failure(t, &format!("The access token was not accepted: {}.", why)),
    }
}

/// Login with Kerberos (SSPI or GSSAPI): run the exchange, with the client's first token already in hand.
fn kerberos_login(server: &Server, t: &mut Transport, cfg: &Config, first: &[u8], last_token: &mut Vec<u8>) -> io::Result<Option<Connection>> {
    let Some(k) = &cfg.auth.kerberos else {
        return external_failure(t, "Integrated (Windows) authentication is not enabled on this server; use a SQL Server login.");
    };
    let mut acceptor = match auth::new_acceptor(k) {
        Ok(a) => a,
        Err(e) => return external_failure(t, &format!("Integrated authentication is not available: {}.", e)),
    };
    let mut token = first.to_vec();
    for _ in 0..8 {
        match acceptor.step(&token) {
            Err(e) => return external_failure(t, &format!("Integrated authentication failed: {}.", e)),
            Ok((out, Some(names))) => {
                *last_token = out;
                return match server.connect_external(&names) {
                    Ok(c) => Ok(Some(c)),
                    Err(e) => {
                        refused(&format!("Kerberos identity {:?} has no login: {}", names, e.message));
                        login_failure(t, e.number, 1, 14, &e.message)?;
                        Ok(None)
                    }
                };
            }
            Ok((out, None)) => {
                // another round: the token goes to the client in an SSPI message, its answer comes back as type 17
                let mut m = vec![0xED];
                m.extend_from_slice(&(out.len() as u16).to_le_bytes());
                m.extend_from_slice(&out);
                t.send(T_REPLY, &m)?;
                let (kind, _, data) = t.read_message()?;
                if kind != T_SSPI {
                    return external_failure(t, "An SSPI message was expected.");
                }
                token = data;
            }
        }
    }
    external_failure(t, "The integrated authentication exchange did not finish.")
}

fn handshake(server: &Server, mut t: Transport, cfg: &Config) -> io::Result<Option<Session>> {
    let bad = |e: String| io::Error::new(io::ErrorKind::InvalidData, e);
    let (kind, _, pre) = t.read_message()?;
    if kind != T_PRELOGIN {
        refused(&format!("the first packet was type {} ({}), not PRELOGIN (18); the jTDS driver does this (it needs TDS 7.0/7.1, which is not supported): use the Microsoft JDBC driver", kind, if kind == T_LOGIN7 { "LOGIN7" } else { "unknown" }));
        return Ok(None);
    }
    trace(&format!("PRELOGIN, {} bytes, client encryption option {:?}", pre.len(), prelogin_encryption(&pre)));
    let answer = encryption_answer(prelogin_encryption(&pre).unwrap_or(2), cfg.tls.is_some(), cfg.encryption);
    trace(&format!("answering PRELOGIN with encryption {}", answer));
    let mars = cfg.mars && prelogin_mars(&pre);
    trace(&format!("client asks for MARS: {}; answering {}", prelogin_mars(&pre), mars));
    let fedauth = cfg.auth.aad.is_some() && prelogin_fedauth(&pre);
    t.send(T_REPLY, &prelogin_reply(answer, mars, fedauth))?;
    if answer == 3 {
        refused("the server requires encryption and the client does not offer it");
        return Ok(None); // the client cannot encrypt and the server insists: it will close the connection
    }
    if answer < 2 {
        let tls = cfg.tls.as_ref().expect("an answer of 0 or 1 needs a certificate");
        let t_ref = std::cell::RefCell::new(&mut t);
        let conn = tls::handshake(
            tls,
            || {
                let (kind, _, payload) = t_ref.borrow_mut().read_message()?;
                if kind != T_PRELOGIN {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "expected a TLS handshake message"));
                }
                Ok(payload)
            },
            |bytes| t_ref.borrow_mut().send(T_PRELOGIN, bytes),
        )?;
        trace(&format!("TLS established: {:?}", conn.protocol_version()));
        t.go_tls(conn)?;
    }
    let (kind, _, payload) = t.read_message()?;
    if answer == 0 {
        // only the login is encrypted: the rest of the session, starting with the reply to it, is plain
        t.go_plain()?;
    }
    if kind == T_SSPI || kind != T_LOGIN7 {
        refused(&format!("expected LOGIN7 (16) after PRELOGIN, got packet type {}", kind));
        return Ok(None);
    }
    trace(&format!("LOGIN7, {} bytes", payload.len()));
    let login = parse_login7(&payload).map_err(bad)?;
    trace(&format!("login as '{}', TDS version {:#x}, packet size {}, database '{}'", login.user, login.tds_version, login.packet_size, login.database));
    if login.tds_version < 0x7200_0000 {
        refused(&format!("the client asked for TDS version {:#x}; this server needs 7.2 or later (0x72000000)", login.tds_version));
        login_failure(&mut t, 18456, 1, 14, "This server requires TDS 7.2 or later.")?;
        return Ok(None);
    }
    // who the client is: an Azure AD token, a Kerberos exchange, or a SQL login and password
    let mut sspi_final: Vec<u8> = vec![];
    let mut fedauth_ack = false;
    let conn = if let Some(fa) = &login.fedauth {
        match aad_login(server, &mut t, cfg, fa)? {
            Some(c) => {
                fedauth_ack = true;
                c
            }
            None => return Ok(None),
        }
    } else if login.sspi {
        match kerberos_login(server, &mut t, cfg, &login.sspi_token, &mut sspi_final)? {
            Some(c) => c,
            None => return Ok(None),
        }
    } else {
        match server.connect_as(&login.user, &login.password) {
            Ok(c) => c,
            Err(e) => {
                login_failure(&mut t, e.number, 1, 14, &e.message)?;
                return Ok(None);
            }
        }
    };
    let mut s = Session { t, conn, db: String::new(), prepared: HashMap::new(), next_handle: 0, tx_descriptor: 0, tx_counter: 0, in_tx: false, bulk: None, cursors: HashMap::new(), next_cursor: 0 };
    if login.ansi_defaults {
        // ODBC and OLE DB drivers get SET ANSI_DEFAULTS ON (and no implicit transactions) from the login
        s.conn.execute("SET ANSI_DEFAULTS ON; SET IMPLICIT_TRANSACTIONS OFF; SET CURSOR_CLOSE_ON_COMMIT OFF");
    }
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
    if !sspi_final.is_empty() {
        // the last SSPI token (Kerberos mutual authentication) rides in front of the login acknowledgement
        out.push(0xED);
        out.extend_from_slice(&(sspi_final.len() as u16).to_le_bytes());
        out.extend_from_slice(&sspi_final);
    }
    // LOGINACK
    let mut ack = vec![1u8];
    ack.extend_from_slice(&TDS_74.to_be_bytes());
    put_bvar(&mut ack, "Microsoft SQL Server");
    ack.extend_from_slice(&[16, 0, 0x03, 0xE8]);
    out.push(0xAD);
    out.extend_from_slice(&(ack.len() as u16).to_le_bytes());
    out.extend_from_slice(&ack);
    if fedauth_ack {
        // FEATUREEXTACK: the FEDAUTH feature (id 2) is accepted
        out.push(0xAE);
        out.push(0x02);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(0xFF);
    }
    done(&mut out, 0xFD, 0, 0);
    s.t.send(T_REPLY, &out)?;
    s.t.packet_size = packet;
    if mars {
        // from here on every packet is inside an SMP frame
        s.t.enable_mars();
    }
    Ok(Some(s))
}

/// Block until the client has sent something. Returns false when the connection is closed or the session was killed
/// (its open transaction is then rolled back and the caller drops the connection).
fn wait_readable(tcp: &TcpStream, conn: &mut Connection) -> bool {
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_millis(250)));
    let mut b = [0u8; 1];
    loop {
        match tcp.peek(&mut b) {
            Ok(0) => return false,
            Ok(_) => {
                let _ = tcp.set_read_timeout(None);
                return true;
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if conn.is_killed() {
                    conn.finish_kill();
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

fn serve_connection(server: Server, stream: TcpStream, cfg: Arc<Config>) {
    let _ = stream.set_nodelay(true);
    // a client that connects and says nothing must not hold a thread forever
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let t = Transport { stream: tls::Wire::Plain(stream), packet_size: DEFAULT_PACKET, spid: 0, pending: vec![], next_id: 0, mux: None };
    let mut s = match handshake(&server, t, &cfg) {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            // a TLS failure, a malformed packet, a client that gave up: the client sees only a closed connection
            eprintln!("tsql-serve: login handshake failed: {} ({:?})", e, e.kind());
            return;
        }
    };
    let _ = s.t.stream.tcp().set_read_timeout(None);
    let Ok(probe) = s.t.stream.tcp().try_clone() else { return };
    let mut pool = mars_par::Pool::new();
    loop {
        // the next request: with MARS from any of the logical sessions, while the replies of requests that run on workers are
        // delivered as they finish; without MARS, the next message
        let mut request_session = 0u16;
        let (kind, status, payload) = if s.t.mux.is_some() {
            match pool.next_request(&mut s, &probe) {
                Some((sid, k, st, p)) => {
                    request_session = sid;
                    (k, st, p)
                }
                None => return,
            }
        } else {
            // wait for the next request, noticing a KILL of this session while the client is idle
            if !wait_readable(&probe, &mut s.conn) {
                return;
            }
            match s.t.read_request() {
                Ok(Request::Message(k, st, p)) => (k, st, p),
                Ok(Request::Bulk(first, eom)) => {
                    trace(&format!("bulk load, first packet {} bytes", first.len()));
                    let out = s.bulk_load(first, eom).unwrap_or_else(|msg| {
                        eprintln!("tsql-serve: a bulk load could not be processed: {}", msg);
                        let mut out = vec![];
                        error_token(&mut out, &SqlError::new(4832, format!("Bulk load: {}", msg)));
                        done(&mut out, 0xFD, DONE_ERROR, 0);
                        out
                    });
                    if let Err(e) = s.t.send(T_REPLY, &out) {
                        trace(&format!("sending the reply failed: {} ({:?})", e, e.kind()));
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    trace(&format!("the client's connection ended: {} ({:?})", e, e.kind()));
                    return;
                }
            }
        };
        trace(&format!("request: packet type {}, {} bytes, status {:#x}", kind, payload.len(), status));
        if s.t.mux.is_some() {
            // a request that only reads runs on a worker, beside the others; any other waits for them to finish first
            if pool.try_run(&s, kind, status, &payload) {
                continue;
            }
            if pool.drain(&mut s).is_err() {
                return;
            }
            // (the replies of the workers went to their own sessions: the reply to this request goes to its own)
            s.t.set_current_session(request_session);
        }
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
                // a bulk load that fitted in one packet
                s.bulk_load(payload, true)
            }
            other => {
                eprintln!("tsql-serve: closing a session: unsupported packet type {}", other);
                return;
            }
        };
        let out = reply.unwrap_or_else(|msg| {
            eprintln!("tsql-serve: a request could not be processed: {}", msg);
            let mut out = vec![];
            error_token(&mut out, &SqlError::new(8144, format!("The request could not be processed: {}", msg)));
            done(&mut out, 0xFD, DONE_ERROR, 0);
            out
        });
        trace(&format!("reply, {} bytes", out.len()));
        if let Err(e) = s.t.send(T_REPLY, &out) {
            trace(&format!("sending the reply failed: {} ({:?})", e, e.kind()));
            return;
        }
    }
}

// ----------------------------------------------------------------------------- the listener
/// Listener settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Whether a client that asks for MARS (Multiple Active Result Sets) gets it.
    pub mars: bool,
    /// Azure AD and Kerberos (off by default).
    pub auth: AuthConfig,
    /// Largest packet size granted to clients.
    pub max_packet: usize,
    /// The server certificate; without one the listener answers "encryption not supported".
    pub tls: Option<TlsConfig>,
    /// Whether clients that cannot encrypt are accepted (only meaningful with a certificate).
    pub encryption: Encryption,
}

impl Default for Config {
    fn default() -> Self {
        Config { max_packet: 32767, tls: None, encryption: Encryption::Optional, mars: true, auth: AuthConfig::default() }
    }
}

impl Config {
    /// Settings with TLS enabled using `tls`; `encryption` says whether clients without TLS are refused.
    pub fn with_tls(tls: TlsConfig, encryption: Encryption) -> Config {
        Config { tls: Some(tls), encryption, ..Config::default() }
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
