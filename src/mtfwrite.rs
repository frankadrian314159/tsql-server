//! Writer for Microsoft Tape Format, the container of uncompressed SQL Server `.bak` files.
//!
//! A backup is a sequence of descriptor blocks (see [`crate::mtf`] for the layout): `TAPE` (the media), `SSET` (the backup
//! set), `VOLB` (the volume), `MSCI` (SQL Server's configuration), `MSDA` (the pages of a data file) and `MSLS`. The pages
//! of the data file are one big stream (`MQDA`) that starts two bytes before a page boundary of the file, so that the pages
//! are aligned, as in the backups SQL Server writes.
use std::io::{self, Read, Seek, SeekFrom, Write};

/// The format logical block size: blocks are padded to a multiple of it.
const UNIT: u64 = 1024;
/// SQL Server's version that the backup says it was written by (SQL Server 2025).
const SOFTWARE_MAJOR: u8 = 17;

fn xor_words(b: &[u8]) -> u16 {
    b.chunks_exact(2).fold(0u16, |a, c| a ^ u16::from_le_bytes([c[0], c[1]]))
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// An MTF date: year (14 bits), month (4), day (5), hour (5), minute (6), second (6) in five bytes.
fn mtf_date(t: chrono::NaiveDateTime) -> [u8; 5] {
    use chrono::{Datelike, Timelike};
    let v: u64 = ((t.year() as u64) << 26) | ((t.month() as u64) << 22) | ((t.day() as u64) << 17) | ((t.hour() as u64) << 12) | ((t.minute() as u64) << 6) | t.second() as u64;
    let b = v.to_be_bytes();
    [b[3], b[4], b[5], b[6], b[7]]
}

/// The header of a descriptor block: `body` (the fields of the type and its strings) starts at offset 52.
fn block_header(kind: &[u8; 4], fla: u64, first_stream: usize) -> Vec<u8> {
    let mut b = vec![0u8; 52];
    b[0..4].copy_from_slice(kind);
    b[8..10].copy_from_slice(&(first_stream as u16).to_le_bytes());
    b[10] = 14; // the operating system of SQL Server backups
    b[11] = 1;
    b[20..28].copy_from_slice(&fla.to_le_bytes());
    b[48] = 2; // strings are UTF-16
    b
}

fn stream_header(id: &[u8; 4], media_attrs: u16, len: u64) -> [u8; 22] {
    let mut s = [0u8; 22];
    s[0..4].copy_from_slice(id);
    s[6..8].copy_from_slice(&media_attrs.to_le_bytes());
    s[8..16].copy_from_slice(&len.to_le_bytes());
    let sum = xor_words(&s[..20]);
    s[20..22].copy_from_slice(&sum.to_le_bytes());
    s
}

/// A stream of a block that is held in memory: (id, media attributes, data).
type Mem<'a> = (&'a [u8; 4], u16, Vec<u8>);

/// Bytes that bring a position up to the next multiple of four (streams start on a four-byte boundary).
fn align4(pos: u64) -> u64 {
    (pos + 3) & !3
}

/// Append a stream (header, data, padding to four bytes) to a block being built; `b` starts at an aligned position.
fn push_stream(b: &mut Vec<u8>, id: &[u8; 4], media_attrs: u16, data: &[u8]) {
    b.extend_from_slice(&stream_header(id, media_attrs, data.len() as u64));
    b.extend_from_slice(data);
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

/// A whole block whose streams are in memory, padded with a `SPAD` stream to the next multiple of the block unit.
fn small_block(kind: &[u8; 4], fla: u64, body: &[u8], streams: Vec<Mem>) -> Vec<u8> {
    let mut body = body.to_vec();
    while (52 + body.len()) % 4 != 0 {
        body.push(0);
    }
    let mut b = block_header(kind, fla, 52 + body.len());
    b.extend_from_slice(&body);
    let sum = xor_words(&b[..50]);
    b[50..52].copy_from_slice(&sum.to_le_bytes());
    for (id, attrs, d) in &streams {
        push_stream(&mut b, id, *attrs, d);
    }
    // the SPAD stream pads the block to the unit: its header (22 bytes) and data end on a multiple of it
    let pad = (UNIT - (b.len() as u64 + 22) % UNIT) % UNIT;
    push_stream(&mut b, b"SPAD", 0, &vec![0u8; pad as usize]);
    debug_assert_eq!(b.len() as u64 % UNIT, 0);
    b
}

/// What the backup says about itself.
pub struct Meta {
    pub database: String,
    pub name: String,
    pub description: String,
    pub user: String,
    pub machine: String,
    pub when: chrono::NaiveDateTime,
    /// The configuration stream (`MQCI`).
    pub config: Vec<u8>,
}

/// Write the backup: `pages` is the data file as a sequence of 8 KiB pages (a file of `page_count` of them).
pub fn write_backup<W: Write>(out: &mut W, meta: &Meta, pages: &mut (impl Read + Seek), page_count: u64) -> io::Result<()> {
    let mut at = 0u64; // bytes written
    let mut put = |out: &mut W, b: &[u8], at: &mut u64| -> io::Result<()> {
        out.write_all(b)?;
        *at += b.len() as u64;
        Ok(())
    };
    // TAPE: the media (name at offset 96, block size, MTF version)
    {
        let name = utf16(&meta.database);
        let mut body = vec![0u8; 44];
        body[16..18].copy_from_slice(&(name.len() as u16).to_le_bytes());
        body[18..20].copy_from_slice(&((52 + 44) as u16).to_le_bytes());
        body[32..34].copy_from_slice(&(UNIT as u16).to_le_bytes());
        body[41] = 1;
        body.extend_from_slice(&name);
        let b = small_block(b"TAPE", 0, &body, vec![]);
        put(out, &b, &mut at)?;
    }
    // SSET: the backup set (names, user, date, software version)
    {
        let fixed = 100usize;
        let name = utf16(&meta.name);
        let desc = utf16(&meta.description);
        let user = utf16(&meta.user);
        let mut body = vec![0u8; fixed - 52];
        let put16 = |body: &mut Vec<u8>, at: usize, v: u16| body[at - 52..at - 50].copy_from_slice(&v.to_le_bytes());
        body[0..4].copy_from_slice(&4u32.to_le_bytes()); // attributes (52)
        put16(&mut body, 60, 0x1200); // software vendor
        put16(&mut body, 62, 1); // data set number
        let mut off = fixed;
        for (field, s) in [(64usize, &name), (68, &desc), (76, &user)] {
            if !s.is_empty() {
                put16(&mut body, field, s.len() as u16);
                put16(&mut body, field + 2, off as u16);
            }
            off += s.len();
        }
        body[88 - 52..93 - 52].copy_from_slice(&mtf_date(meta.when));
        body[93 - 52] = SOFTWARE_MAJOR;
        body[97 - 52] = 2;
        body.extend_from_slice(&name);
        body.extend_from_slice(&desc);
        body.extend_from_slice(&user);
        let b = small_block(b"SSET", at / UNIT, &body, vec![]);
        put(out, &b, &mut at)?;
    }
    // VOLB: the volume (machine name)
    {
        let machine = utf16(&meta.machine);
        let mut body = vec![0u8; 21];
        body[12..14].copy_from_slice(&(machine.len() as u16).to_le_bytes());
        body[14..16].copy_from_slice(&((52 + 21) as u16).to_le_bytes());
        body[16..21].copy_from_slice(&mtf_date(meta.when));
        body.extend_from_slice(&machine);
        let b = small_block(b"VOLB", at / UNIT, &body, vec![]);
        put(out, &b, &mut at)?;
    }
    // MSCI: SQL Server's configuration
    {
        let b = small_block(b"MSCI", at / UNIT, &[0u8; 4], vec![(b"MQCI", 0, meta.config.clone())]);
        put(out, &b, &mut at)?;
    }
    // MSDA: the pages. The data stream starts two bytes before a multiple of the page size, so the pages are aligned.
    {
        let total = 2 + page_count * 8192;
        let header_len = 60u64; // the block header and 8 bytes of fields
        let apad_data_at = at + header_len + 22;
        let mut apad = 0u64;
        // the MQDA data starts after the APAD stream (padded to four bytes) and the MQDA header
        while (align4(apad_data_at + apad) + 22 + 2) % 8192 != 0 {
            apad += 1;
        }
        let mut b = block_header(b"MSDA", at / UNIT, header_len as usize);
        b.extend_from_slice(&[0u8; 8]);
        let sum = xor_words(&b[..50]);
        b[50..52].copy_from_slice(&sum.to_le_bytes());
        push_stream(&mut b, b"APAD", 0x80, &vec![0u8; apad as usize]);
        b.extend_from_slice(&stream_header(b"MQDA", 0x102, total));
        put(out, &b, &mut at)?;
        put(out, &[0u8, 0u8], &mut at)?;
        debug_assert_eq!(at % 8192, 0);
        pages.seek(SeekFrom::Start(0))?;
        let mut crc = crc32fast::Hasher::new();
        let mut buf = vec![0u8; 8192 * 64];
        let mut left = page_count * 8192;
        while left > 0 {
            let n = (buf.len() as u64).min(left) as usize;
            pages.read_exact(&mut buf[..n])?;
            crc.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            at += n as u64;
            left -= n as u64;
        }
        // CSUM (a checksum of the data), the empty end-of-data MQDA stream, and the padding of the block
        let mut tail = vec![];
        push_stream(&mut tail, b"CSUM", 0, &crc.finalize().to_le_bytes());
        push_stream(&mut tail, b"MQDA", 0x6, &[]);
        let pad = (UNIT - (at + tail.len() as u64 + 22) % UNIT) % UNIT;
        push_stream(&mut tail, b"SPAD", 0, &vec![0u8; pad as usize]);
        put(out, &tail, &mut at)?;
    }
    // MSLS: the end of the backup, with the configuration again
    {
        let b = small_block(b"MSLS", at / UNIT, &[], vec![(b"MQCI", 0, meta.config.clone())]);
        put(out, &b, &mut at)?;
    }
    out.flush()
}

/// The configuration stream (`MQCI`): the database and its file. The layout of the stream as SQL Server writes it is only
/// partly known, so this is a simplified one: a `SCIN` header and the database and file names.
pub fn config_stream(database: &str, file: &str, pages: u64) -> Vec<u8> {
    let mut v = vec![];
    v.extend_from_slice(b"SCIN");
    v.extend_from_slice(&0x1C6u32.to_le_bytes());
    v.extend_from_slice(&0x210u32.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes()); // one file group
    v.extend_from_slice(&1u32.to_le_bytes()); // one file
    v.extend_from_slice(&pages.to_le_bytes());
    for s in [database, file, "PRIMARY"] {
        let u = utf16(s);
        v.extend_from_slice(&(u.len() as u16).to_le_bytes());
        v.extend_from_slice(&u);
    }
    v
}
