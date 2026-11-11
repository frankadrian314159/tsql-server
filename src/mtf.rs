//! Reader for Microsoft Tape Format (MTF), the container of native SQL Server `.bak` files.
//!
//! A backup is a sequence of *descriptor blocks* (`TAPE`, `SSET`, `VOLB`, `FILE`, `SFMB`, `ESET`, ...). Every block starts
//! with a 52-byte common header, then fields specific to its type, then strings, then a list of *streams* (a 22-byte
//! header, the data, padding to 4 bytes) that ends with a `SPAD` stream padding the block to the format's logical block
//! size. SQL Server puts its own data in streams of its own; this module only walks the container and does not look inside
//! them, so it works whatever a stream holds. A stream's data stays in the file: only its position and length are kept.
//!
//! The scan is tolerant: it returns the blocks it could read together with the reason it stopped, if it did not reach the
//! end of the file cleanly, so a damaged or unexpected file can still be described.
use std::io::{Read, Seek, SeekFrom};

/// Size of the header every descriptor block starts with.
pub const DBLK_HEADER: usize = 52;
/// Size of the header in front of every stream.
pub const STREAM_HEADER: usize = 22;

/// The block types of MTF. SQL Server adds its own (`MSCI`, ...): any four upper-case letters are accepted as a block type
/// when the header checksum is right.
pub const BLOCK_TYPES: [&[u8; 4]; 10] = [b"TAPE", b"SSET", b"VOLB", b"DIRB", b"FILE", b"CFIL", b"ESPB", b"ESET", b"EOTM", b"SFMB"];

#[derive(Debug, Clone)]
pub struct Stream {
    pub id: [u8; 4],
    pub fs_attrs: u16,
    pub media_attrs: u16,
    /// Length of the data in bytes (without the padding that follows it).
    pub len: u64,
    pub encryption: u16,
    pub compression: u16,
    /// File offset of the first byte of the data.
    pub data_at: u64,
}

impl Stream {
    pub fn name(&self) -> String {
        printable(&self.id)
    }
}

#[derive(Debug, Clone)]
pub struct Block {
    /// File offset of the block.
    pub at: u64,
    pub kind: [u8; 4],
    pub attrs: u32,
    pub os_id: u8,
    pub os_version: u8,
    pub displayable_size: u64,
    /// Format logical address: where the block sits in the media, counted in format logical blocks.
    pub fla: u64,
    pub control_block_id: u32,
    /// 0 none, 1 ANSI, 2 Unicode (UTF-16LE).
    pub string_type: u8,
    /// Everything from the start of the block up to its first stream: the common header, the fields specific to the
    /// type, and the strings.
    pub raw: Vec<u8>,
    pub streams: Vec<Stream>,
}

impl Block {
    pub fn name(&self) -> String {
        printable(&self.kind)
    }

    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }

    pub fn u8_at(&self, off: usize) -> u8 {
        self.raw.get(off).copied().unwrap_or(0)
    }

    pub fn u16_at(&self, off: usize) -> u16 {
        self.raw.get(off..off + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
    }

    pub fn u32_at(&self, off: usize) -> u32 {
        self.raw.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
    }

    /// A string given as an address (size, offset from the start of the block) at `field`.
    pub fn string_at(&self, field: usize) -> String {
        let size = self.u16_at(field) as usize;
        let off = self.u16_at(field + 2) as usize;
        if size == 0 || off == 0 {
            return String::new();
        }
        let Some(b) = self.raw.get(off..off + size) else { return String::new() };
        if self.string_type == 2 {
            let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
        } else {
            String::from_utf8_lossy(b).trim_end_matches('\0').to_string()
        }
    }

    /// A date as stored in MTF: five bytes holding year (14 bits), month (4), day (5), hour (5), minute (6), second (6).
    pub fn date_at(&self, off: usize) -> Option<String> {
        let b = self.raw.get(off..off + 5)?;
        let v = b.iter().fold(0u64, |a, x| (a << 8) | *x as u64);
        if v == 0 {
            return None;
        }
        Some(format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", (v >> 26) & 0x3FFF, (v >> 22) & 15, (v >> 17) & 31, (v >> 12) & 31, (v >> 6) & 63, v & 63))
    }
}

/// The result of scanning a file.
#[derive(Debug, Default)]
pub struct Scan {
    pub blocks: Vec<Block>,
    pub file_len: u64,
    /// Why the scan stopped before the end of the file, when it did.
    pub problem: Option<String>,
}

pub fn printable(b: &[u8]) -> String {
    b.iter().map(|c| if c.is_ascii_graphic() || *c == b' ' { *c as char } else { '.' }).collect()
}

/// MTF's checksum: the XOR of the 16-bit words before it.
fn xor_words(b: &[u8]) -> u16 {
    b.chunks_exact(2).fold(0u16, |a, c| a ^ u16::from_le_bytes([c[0], c[1]]))
}

/// True when the file starts like an MTF backup.
pub fn is_mtf(head: &[u8]) -> bool {
    head.len() >= 4 && head.starts_with(b"TAPE")
}

/// Walk the whole file and describe its blocks and streams.
pub fn scan<R: Read + Seek>(r: &mut R) -> std::io::Result<Scan> {
    let file_len = r.seek(SeekFrom::End(0))?;
    let mut out = Scan { file_len, ..Scan::default() };
    let mut at = 0u64;
    while at + DBLK_HEADER as u64 <= file_len {
        match block_at(r, at, file_len)? {
            Ok(b) => {
                let end = b.streams.last().map(|s| stream_end(s.data_at, s.len)).unwrap_or(at + b.raw.len() as u64);
                let complete = b.streams.last().map(|s| &s.id == b"SPAD").unwrap_or(false) && end <= file_len;
                out.blocks.push(b);
                if !complete {
                    out.problem = Some(format!("the stream list of the block at offset {} does not end with a pad stream (the file is cut short)", at));
                    return Ok(out);
                }
                at = end;
            }
            Err(why) => {
                // a file may end in zero fill
                r.seek(SeekFrom::Start(at))?;
                let mut rest = vec![0u8; (file_len - at).min(4096) as usize];
                r.read_exact(&mut rest)?;
                if !rest.iter().all(|b| *b == 0) {
                    out.problem = Some(why);
                }
                return Ok(out);
            }
        }
    }
    if at < file_len {
        out.problem = Some(format!("{} byte(s) at the end of the file do not form a block", file_len - at));
    }
    Ok(out)
}

/// The position after a stream whose data starts at `data_at`: the stream (header and data) is padded so that the next
/// header starts on a four-byte boundary.
fn stream_end(data_at: u64, len: u64) -> u64 {
    data_at.saturating_add(len).saturating_add(3) & !3
}

fn block_at<R: Read + Seek>(r: &mut R, at: u64, file_len: u64) -> std::io::Result<Result<Block, String>> {
    r.seek(SeekFrom::Start(at))?;
    let mut h = [0u8; DBLK_HEADER];
    r.read_exact(&mut h)?;
    let kind: [u8; 4] = h[0..4].try_into().unwrap();
    if !BLOCK_TYPES.iter().any(|t| **t == kind) && !kind.iter().all(|c| c.is_ascii_uppercase()) {
        return Ok(Err(format!("offset {}: '{}' is not a block type of MTF", at, printable(&kind))));
    }
    let stored = u16::from_le_bytes([h[50], h[51]]);
    if xor_words(&h[..50]) != stored {
        return Ok(Err(format!("offset {}: the checksum of the {} block header does not match", at, printable(&kind))));
    }
    let first = u16::from_le_bytes([h[8], h[9]]) as u64;
    if first < DBLK_HEADER as u64 || at + first > file_len {
        return Ok(Err(format!("offset {}: the {} block gives an impossible offset to its streams ({})", at, printable(&kind), first)));
    }
    let mut raw = vec![0u8; first as usize];
    raw[..DBLK_HEADER].copy_from_slice(&h);
    r.read_exact(&mut raw[DBLK_HEADER..])?;
    let mut b = Block {
        at,
        kind,
        attrs: u32::from_le_bytes(h[4..8].try_into().unwrap()),
        os_id: h[10],
        os_version: h[11],
        displayable_size: u64::from_le_bytes(h[12..20].try_into().unwrap()),
        fla: u64::from_le_bytes(h[20..28].try_into().unwrap()),
        control_block_id: u32::from_le_bytes(h[36..40].try_into().unwrap()),
        string_type: h[48],
        raw,
        streams: vec![],
    };
    let mut pos = at + first;
    loop {
        if pos + STREAM_HEADER as u64 > file_len {
            return Ok(Ok(b)); // cut short; the caller reports the missing pad stream
        }
        r.seek(SeekFrom::Start(pos))?;
        let mut s = [0u8; STREAM_HEADER];
        r.read_exact(&mut s)?;
        if xor_words(&s[..20]) != u16::from_le_bytes([s[20], s[21]]) {
            return Ok(Err(format!("offset {}: the checksum of a stream header in the {} block at {} does not match", pos, printable(&kind), at)));
        }
        let st = Stream {
            id: s[0..4].try_into().unwrap(),
            fs_attrs: u16::from_le_bytes([s[4], s[5]]),
            media_attrs: u16::from_le_bytes([s[6], s[7]]),
            len: u64::from_le_bytes(s[8..16].try_into().unwrap()),
            encryption: u16::from_le_bytes([s[16], s[17]]),
            compression: u16::from_le_bytes([s[18], s[19]]),
            data_at: pos + STREAM_HEADER as u64,
        };
        let end = stream_end(st.data_at, st.len);
        let spad = &st.id == b"SPAD";
        b.streams.push(st);
        if end > file_len {
            return Ok(Ok(b));
        }
        pos = end;
        if spad {
            return Ok(Ok(b));
        }
    }
}

/// Read up to `n` bytes of a stream's data.
pub fn read_stream<R: Read + Seek>(r: &mut R, s: &Stream, n: usize) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(s.data_at))?;
    let mut buf = vec![0u8; (s.len as usize).min(n)];
    r.read_exact(&mut buf)?;
    Ok(buf)
}
