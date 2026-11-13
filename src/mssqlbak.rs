//! The container of backups written by recent SQL Server versions (verified on a SQL Server 2025 backup).
//!
//! ```text
//! 0x0000  "MSSQLBAK" header page (4 KiB)            -- replaces the MTF TAPE block
//! 0x1000  SFMB block (an MTF soft filemark, 4 KiB)
//! 0x2000  chunk, chunk, chunk, ...                  -- the compressed stream
//!         SFMB blocks again, then a 16-byte footer
//! ```
//!
//! Each chunk is a 24-byte header, a 4-byte field, and one Xpress Huffman chunk ([`crate::xpress`]):
//!
//! ```text
//! byte 0     0x21
//! byte 1     the size of the chunk once expanded, in units of 512 bytes (0x80 = 64 KiB; the first and last are shorter)
//! bytes 2-3  length of the compressed data, not counting the 4 bytes that follow the header
//! bytes 4-23 unused (they contain leftovers of memory)
//! 4 bytes    (a checksum, not yet checked)
//! n bytes    256-byte table and bit stream
//! ```
//!
//! Expanding the chunks and joining them gives an ordinary Microsoft Tape Format stream (see [`crate::mtf`]) that starts
//! with the `SSET` block (the `TAPE` block is the header page). [`Expanded`] presents that stream as a seekable reader that
//! expands a chunk when it is first read, so the container can be scanned without expanding the data streams.
use crate::xpress;
use std::io::{self, Read, Seek, SeekFrom};

pub const MAGIC: &[u8; 8] = b"MSSQLBAK";
const HEADER_PAGE: u64 = 0x1000;
const CHUNK_HEADER: u64 = 0x18;

/// True when the file starts like a backup of this form.
pub fn is_bak(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

#[derive(Debug, Clone)]
pub struct Chunk {
    /// Offset of the chunk header in the file.
    pub file_at: u64,
    /// Compressed length (table and bit stream).
    pub comp_len: u32,
    /// Length once expanded.
    pub size: u32,
    /// Offset of the chunk's first byte in the expanded stream.
    pub virt: u64,
    /// The 4 bytes in front of the compressed data.
    pub check: u32,
}

#[derive(Debug, Clone)]
pub struct Layout {
    /// The first words of the header page.
    pub header_words: Vec<u32>,
    pub chunks: Vec<Chunk>,
    /// Size of the expanded stream.
    pub total: u64,
    /// Bytes of the file the chunks occupy.
    pub comp_total: u64,
    /// File offset just after the last chunk.
    pub end: u64,
}

/// Find the chunks of a file without expanding any of them.
pub fn layout<R: Read + Seek>(r: &mut R) -> io::Result<Result<Layout, String>> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut head = vec![0u8; HEADER_PAGE as usize];
    if len < HEADER_PAGE + 24 || r.read_exact(&mut head).is_err() || !is_bak(&head) {
        return Ok(Err("it does not start with the MSSQLBAK header".to_string()));
    }
    let header_words = (2..12).map(|i| u32::from_le_bytes(head[i * 4..i * 4 + 4].try_into().unwrap())).collect();
    // the SFMB block after the header page says where the chunks start
    let mut sfmb = [0u8; 12];
    r.seek(SeekFrom::Start(HEADER_PAGE))?;
    r.read_exact(&mut sfmb)?;
    if &sfmb[0..4] != b"SFMB" {
        return Ok(Err(format!("expected an SFMB block at offset {:#x}", HEADER_PAGE)));
    }
    let first = u16::from_le_bytes([sfmb[8], sfmb[9]]) as u64;
    let mut at = HEADER_PAGE + first;
    let mut virt = 0u64;
    let mut chunks = vec![];
    let mut comp_total = 0;
    loop {
        if at + CHUNK_HEADER + 4 > len {
            break;
        }
        let mut h = [0u8; 28];
        r.seek(SeekFrom::Start(at))?;
        r.read_exact(&mut h)?;
        if h[0] != 0x21 || h[1] == 0 {
            break;
        }
        let comp_len = u16::from_le_bytes([h[2], h[3]]) as u32;
        let size = h[1] as u32 * 512;
        let check = u32::from_le_bytes(h[24..28].try_into().unwrap());
        let next = at + CHUNK_HEADER + 4 + comp_len as u64;
        if next > len {
            return Ok(Err(format!("the chunk at offset {:#x} runs past the end of the file", at)));
        }
        chunks.push(Chunk { file_at: at, comp_len, size, virt, check });
        virt += size as u64;
        comp_total += CHUNK_HEADER + 4 + comp_len as u64;
        at = next;
    }
    if chunks.is_empty() {
        return Ok(Err("no compressed chunks follow the header".to_string()));
    }
    Ok(Ok(Layout { header_words, chunks, total: virt, comp_total, end: at }))
}

/// The expanded stream as a `Read + Seek`.
pub struct Expanded<R: Read + Seek> {
    r: R,
    pub layout: Layout,
    pos: u64,
    cache: Option<(usize, Vec<u8>)>,
    /// An uncompressed backup (a plain Microsoft Tape Format file): the stream is the file itself.
    plain: bool,
}

impl<R: Read + Seek> Expanded<R> {
    pub fn open(mut r: R) -> io::Result<Result<Expanded<R>, String>> {
        // an uncompressed backup starts with the TAPE block of Microsoft Tape Format
        let len = r.seek(SeekFrom::End(0))?;
        r.seek(SeekFrom::Start(0))?;
        let mut head = [0u8; 4];
        if len >= 4 && r.read_exact(&mut head).is_ok() && crate::mtf::is_mtf(&head) {
            let layout = Layout { header_words: vec![], chunks: vec![], total: len, comp_total: len, end: len };
            return Ok(Ok(Expanded { r, layout, pos: 0, cache: None, plain: true }));
        }
        Ok(layout(&mut r)?.map(|layout| Expanded { r, layout, pos: 0, cache: None, plain: false }))
    }

    fn chunk_of(&self, pos: u64) -> Option<usize> {
        let i = self.layout.chunks.partition_point(|c| c.virt + c.size as u64 <= pos);
        (i < self.layout.chunks.len()).then_some(i)
    }

    /// The expanded bytes of chunk `i`.
    pub fn chunk(&mut self, i: usize) -> io::Result<&[u8]> {
        if !matches!(&self.cache, Some((c, _)) if *c == i) {
            let c = self.layout.chunks[i].clone();
            let mut buf = vec![0u8; c.comp_len as usize];
            self.r.seek(SeekFrom::Start(c.file_at + CHUNK_HEADER + 4))?;
            self.r.read_exact(&mut buf)?;
            let data = xpress::decompress(&buf, c.size as usize).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("chunk {} at offset {:#x}: {}", i, c.file_at, e)))?;
            self.cache = Some((i, data));
        }
        Ok(&self.cache.as_ref().unwrap().1)
    }
}

impl<R: Read + Seek> Read for Expanded<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.plain {
            self.r.seek(SeekFrom::Start(self.pos))?;
            let n = self.r.read(out)?;
            self.pos += n as u64;
            return Ok(n);
        }
        let Some(i) = self.chunk_of(self.pos) else { return Ok(0) };
        let start = (self.pos - self.layout.chunks[i].virt) as usize;
        let data = self.chunk(i)?;
        let n = out.len().min(data.len() - start);
        out[..n].copy_from_slice(&data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for Expanded<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let p = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => self.layout.total as i64 + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if p < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}
