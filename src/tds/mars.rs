//! MARS (Multiple Active Result Sets): the Session Multiplex Protocol (MS-SMP).
//!
//! A client that asks for MARS in its pre-login message gets it: after the login, every TDS packet travels inside an SMP
//! frame (a 16-byte header: `0x53`, flags SYN/ACK/FIN/DATA, the session id, the frame length, a sequence number and a
//! window) and the client opens several logical sessions (SYN) on the one connection, each carrying its own requests and
//! replies. The sessions share the connection's engine state (the transaction, temporary tables, SET options), as in SQL
//! Server.
//!
//! The engine runs one statement at a time, so requests are executed one after another in the order they arrive, and the
//! reply to each is queued for its session and sent as the client's window for that session allows, interleaved with the
//! replies of other sessions. A reply is therefore held in memory while the client is busy with another session, and a
//! statement does not start until the one before it has finished running (it need not have been read). That keeps the
//! common use of MARS (read a result while issuing other commands on the same connection) from deadlocking, without
//! running statements in parallel.
use super::*;
use std::collections::VecDeque;

const SMID: u8 = 0x53;
const SYN: u8 = 0x01;
const ACK: u8 = 0x02;
const FIN: u8 = 0x04;
const DATA: u8 = 0x08;
const HEADER: usize = 16;
/// How many packets the client may send ahead of our acknowledgements.
const WINDOW: u32 = 4;

struct Smp {
    /// Bytes received on the session that do not yet make up whole TDS packets.
    inbytes: Vec<u8>,
    /// The message being assembled from packets: type, status of its first packet, payload.
    msg: Vec<u8>,
    kind: u8,
    status: u8,
    /// Sequence number of the last DATA frame received (the SYN is number 0, the first DATA frame 1).
    recv_count: u32,
    /// Sequence number of the last DATA frame sent (0: none).
    out_seq: u32,
    /// The client accepts DATA frames whose sequence number is below this.
    peer_wnd: u32,
    /// TDS packets waiting for the window.
    outq: VecDeque<Vec<u8>>,
}

#[derive(Default)]
pub struct Mux {
    sessions: HashMap<u16, Smp>,
    /// Complete requests: session, type, status, payload.
    ready: VecDeque<(u16, u8, u8, Vec<u8>)>,
    /// The session whose request is being served: replies go to it.
    cur: u16,
}

fn frame(buf: &mut Vec<u8>, flags: u8, sid: u16, seq: u32, wnd: u32, payload: &[u8]) {
    buf.push(SMID);
    buf.push(flags);
    buf.extend_from_slice(&sid.to_le_bytes());
    buf.extend_from_slice(&((HEADER + payload.len()) as u32).to_le_bytes());
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&wnd.to_le_bytes());
    buf.extend_from_slice(payload);
}

impl Transport {
    /// Switch to SMP framing (right after the login reply has been sent).
    pub(super) fn enable_mars(&mut self) {
        self.mux = Some(Mux::default());
        // the packet size the client chose includes the SMP header
        self.packet_size = self.packet_size.saturating_sub(HEADER).max(512);
    }

    pub(super) fn is_mars(&self) -> bool {
        self.mux.is_some()
    }

    /// Whether a request is waiting that needs no read from the socket.
    pub(super) fn has_ready(&self) -> bool {
        self.mux.as_ref().map(|m| !m.ready.is_empty()).unwrap_or(false)
    }

    /// Queue TDS packets (`buf` holds whole packets) for the current session and send what the window allows.
    pub(super) fn mars_emit(&mut self, buf: &[u8]) -> io::Result<()> {
        let mux = self.mux.as_mut().expect("MARS is on");
        let cur = mux.cur;
        let Some(sm) = mux.sessions.get_mut(&cur) else {
            return Ok(()); // the session was closed while its reply was being produced
        };
        let mut off = 0;
        while off + 8 <= buf.len() {
            let len = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
            if len < 8 || off + len > buf.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a reply packet is malformed"));
            }
            sm.outq.push_back(buf[off..off + len].to_vec());
            off += len;
        }
        self.mars_pump()
    }

    /// Send queued packets of every session as far as its window allows.
    fn mars_pump(&mut self) -> io::Result<()> {
        let mux = self.mux.as_mut().expect("MARS is on");
        let mut out = vec![];
        for (sid, sm) in mux.sessions.iter_mut() {
            while sm.out_seq.wrapping_add(1) < sm.peer_wnd {
                let Some(p) = sm.outq.pop_front() else { break };
                sm.out_seq = sm.out_seq.wrapping_add(1);
                frame(&mut out, DATA, *sid, sm.out_seq, sm.recv_count.wrapping_add(WINDOW), &p);
                if tracing() {
                    trace(&format!("SMP frame out: DATA session {}, seq {}, {} bytes, peer window {}, {} queued", sid, sm.out_seq, p.len(), sm.peer_wnd, sm.outq.len()));
                }
            }
        }
        if out.is_empty() {
            return Ok(());
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// The next request of any session; blocks, reading frames, until one is complete.
    pub(super) fn mars_request(&mut self) -> io::Result<(u16, u8, u8, Vec<u8>)> {
        loop {
            if let Some(r) = self.mux.as_mut().expect("MARS is on").ready.pop_front() {
                self.mux.as_mut().unwrap().cur = r.0;
                return Ok(r);
            }
            self.mars_read_frame()?;
        }
    }

    /// Read one SMP frame and act on it.
    fn mars_read_frame(&mut self) -> io::Result<()> {
        let mut h = [0u8; HEADER];
        self.stream.read_exact(&mut h)?;
        if h[0] != SMID {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an SMP frame"));
        }
        let flags = h[1];
        let sid = u16::from_le_bytes([h[2], h[3]]);
        let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        let wnd = u32::from_le_bytes([h[12], h[13], h[14], h[15]]);
        if len < HEADER || len - HEADER > MAX_MESSAGE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad SMP frame length"));
        }
        if tracing() {
            trace(&format!("SMP frame in: flags {:#x}, session {}, length {}, seq {}, window {}", flags, sid, len, u32::from_le_bytes([h[8], h[9], h[10], h[11]]), wnd));
        }
        let mut payload = vec![0u8; len - HEADER];
        self.stream.read_exact(&mut payload)?;
        let mux = self.mux.as_mut().expect("MARS is on");
        let mut out = vec![];
        if flags & SYN != 0 {
            mux.sessions.insert(sid, Smp { inbytes: vec![], msg: vec![], kind: 0, status: 0, recv_count: 0, out_seq: 0, peer_wnd: wnd, outq: VecDeque::new() });
            frame(&mut out, ACK, sid, 0, WINDOW, &[]);
        }
        // the window travels in every frame, not only in ACKs
        if flags & (ACK | DATA) != 0 {
            if let Some(sm) = mux.sessions.get_mut(&sid) {
                if wnd.wrapping_sub(sm.peer_wnd) < (1 << 31) {
                    sm.peer_wnd = wnd;
                }
            }
        }
        if flags & DATA != 0 {
            if let Some(sm) = mux.sessions.get_mut(&sid) {
                sm.recv_count = u32::from_le_bytes([h[8], h[9], h[10], h[11]]);
                sm.inbytes.extend_from_slice(&payload);
                // acknowledge at once: the data goes straight into memory
                frame(&mut out, ACK, sid, sm.out_seq, sm.recv_count.wrapping_add(WINDOW), &[]);
                // whole TDS packets
                while sm.inbytes.len() >= 8 {
                    let plen = u16::from_be_bytes([sm.inbytes[2], sm.inbytes[3]]) as usize;
                    if plen < 8 || !matches!(sm.inbytes[0], 1..=8 | 14 | 16 | 17 | 18) {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a TDS packet"));
                    }
                    if sm.inbytes.len() < plen {
                        break;
                    }
                    let pkt: Vec<u8> = sm.inbytes.drain(..plen).collect();
                    if sm.msg.is_empty() {
                        sm.kind = pkt[0];
                        sm.status = pkt[1];
                    }
                    sm.msg.extend_from_slice(&pkt[8..]);
                    if sm.msg.len() > MAX_MESSAGE {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
                    }
                    if pkt[1] & 1 != 0 {
                        let msg = std::mem::take(&mut sm.msg);
                        mux.ready.push_back((sid, sm.kind, sm.status, msg));
                    }
                }
            }
        }
        if flags & FIN != 0 {
            if let Some(sm) = mux.sessions.remove(&sid) {
                frame(&mut out, FIN, sid, sm.out_seq, sm.recv_count.wrapping_add(WINDOW), &[]);
            }
            mux.ready.retain(|r| r.0 != sid);
        }
        if !out.is_empty() {
            self.stream.write_all(&out)?;
            self.stream.flush()?;
        }
        // a window may have opened
        self.mars_pump()
    }
}
