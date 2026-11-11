//! Xpress Huffman decompression ([MS-XCA] section 2.2, "LZ77+Huffman"), the compression SQL Server uses for
//! `BACKUP ... WITH COMPRESSION`.
//!
//! A compressed chunk starts with 256 bytes holding 512 code lengths of 4 bits (symbol `2i` in the low nibble of byte
//! `i`, symbol `2i+1` in the high nibble). Symbols 0-255 are literal bytes. A symbol `256 + (b << 4) + l` is a match: its
//! length is `l + 3`, extended when `l` is 15 (one more byte, which when 255 is followed by a 16-bit or, when that is 0, a
//! 32-bit length), and its distance is `(1 << b) + the next b bits`. Codes are canonical (by length, then by symbol). The
//! bits are read from 16-bit little-endian words, most significant bit first, and the extension bytes are read from the
//! same byte stream at the position after the words the bit reader has already taken.
//!
//! The one detail that is easy to get wrong, and that was checked against real backups: the extension byte of a match is
//! read **before** the distance bits.

#[derive(Debug)]
pub struct XpressError(pub String);

impl std::fmt::Display for XpressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn err<T>(m: &str) -> Result<T, XpressError> {
    Err(XpressError(m.to_string()))
}

const TABLE_BITS: u32 = 15;

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8], pos: usize) -> Bits<'a> {
        let mut b = Bits { d, pos, buf: 0, cnt: 32 };
        let hi = b.word();
        let lo = b.word();
        b.buf = (hi << 16) | lo;
        b
    }

    fn word(&mut self) -> u32 {
        let w = match self.d.get(self.pos..self.pos + 2) {
            Some(b) => u16::from_le_bytes([b[0], b[1]]) as u32,
            None => 0,
        };
        self.pos += 2;
        w
    }

    #[inline]
    fn peek(&self, n: u32) -> usize {
        (self.buf >> (32 - n)) as usize
    }

    #[inline]
    fn skip(&mut self, n: u32) {
        self.buf <<= n;
        self.cnt -= n;
        if self.cnt < 16 {
            let w = self.word();
            self.buf |= w << (16 - self.cnt);
            self.cnt += 16;
        }
    }
}

/// Decode a chunk that expands to `out_len` bytes. `input` holds the 256-byte table and the bit stream.
pub fn decompress(input: &[u8], out_len: usize) -> Result<Vec<u8>, XpressError> {
    if input.len() < 256 + 4 {
        return err("an Xpress Huffman chunk is shorter than its table");
    }
    // symbol 0..511 -> code length
    let mut len = [0u8; 512];
    for i in 0..256 {
        len[2 * i] = input[i] & 15;
        len[2 * i + 1] = input[i] >> 4;
    }
    // canonical code assignment, and a table indexed by the next 15 bits: (length << 9) | symbol
    let mut count = [0u32; 16];
    for &l in len.iter() {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut table = vec![0u16; 1 << TABLE_BITS];
    let mut next = [0u32; 17];
    let mut code = 0u32;
    for l in 1..16 {
        code = (code + count[l - 1]) << 1;
        next[l] = code;
    }
    for sym in 0..512usize {
        let l = len[sym] as u32;
        if l == 0 {
            continue;
        }
        let c = next[l as usize];
        next[l as usize] += 1;
        if c >= (1 << l) {
            return err("the Huffman table of a chunk is over-subscribed");
        }
        let lo = (c << (TABLE_BITS - l)) as usize;
        let hi = ((c + 1) << (TABLE_BITS - l)) as usize;
        let e = ((l as u16) << 9) | sym as u16;
        for slot in &mut table[lo..hi] {
            *slot = e;
        }
    }
    let mut br = Bits::new(input, 256);
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    while out.len() < out_len {
        let e = table[br.peek(TABLE_BITS)];
        if e == 0 {
            return err("an invalid Huffman code in a chunk");
        }
        br.skip((e >> 9) as u32);
        let sym = (e & 0x1FF) as usize;
        if sym < 256 {
            out.push(sym as u8);
            continue;
        }
        let s = sym - 256;
        let mut mlen = s & 15;
        let ob = (s >> 4) as u32;
        if mlen == 15 {
            let p = br.pos;
            let Some(&b) = input.get(p) else { return err("a chunk ends inside a match length") };
            br.pos += 1;
            if b == 255 {
                let Some(w) = input.get(br.pos..br.pos + 2) else { return err("a chunk ends inside a match length") };
                mlen = u16::from_le_bytes([w[0], w[1]]) as usize;
                br.pos += 2;
                if mlen == 0 {
                    let Some(w) = input.get(br.pos..br.pos + 4) else { return err("a chunk ends inside a match length") };
                    mlen = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) as usize;
                    br.pos += 4;
                }
            } else {
                mlen = b as usize + 15;
            }
        }
        mlen += 3;
        let offset = if ob == 0 {
            1
        } else {
            let v = br.peek(ob);
            br.skip(ob);
            (1usize << ob) | v
        };
        if offset > out.len() {
            return err("a match in a chunk points before the start of its output");
        }
        let n = mlen.min(out_len - out.len());
        let from = out.len() - offset;
        if offset >= n {
            out.extend_from_within(from..from + n);
        } else {
            for i in 0..n {
                let b = out[from + i];
                out.push(b);
            }
        }
    }
    Ok(out)
}
