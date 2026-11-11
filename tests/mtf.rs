//! The Microsoft Tape Format reader, on files built here to the layout of the MTF specification.
use std::io::Cursor;
use tsql_server::mtf;

fn xor(b: &[u8]) -> u16 {
    b.chunks_exact(2).fold(0u16, |a, c| a ^ u16::from_le_bytes([c[0], c[1]]))
}

/// A descriptor block: common header, `body` (type-specific fields and strings, placed from offset 52), and streams.
fn block(kind: &[u8; 4], body: &[u8], streams: &[(&[u8; 4], Vec<u8>)], unit: usize) -> Vec<u8> {
    let first = 52 + body.len();
    let mut b = vec![0u8; 52];
    b[0..4].copy_from_slice(kind);
    b[8..10].copy_from_slice(&(first as u16).to_le_bytes());
    b[48] = 2; // UTF-16 strings
    let sum = xor(&b[..50]);
    b[50..52].copy_from_slice(&sum.to_le_bytes());
    b.extend_from_slice(body);
    let mut all: Vec<(&[u8; 4], Vec<u8>)> = streams.iter().map(|(i, d)| (*i, d.clone())).collect();
    // pad with a SPAD stream so the block ends on a multiple of `unit`
    let mut used = b.len();
    for (_, d) in &all {
        used += (22 + d.len() + 3) / 4 * 4;
    }
    let pad = (unit - (used + 22) % unit) % unit;
    all.push((b"SPAD", vec![0u8; pad]));
    for (id, d) in all {
        let mut s = vec![0u8; 22];
        s[0..4].copy_from_slice(id);
        s[8..16].copy_from_slice(&(d.len() as u64).to_le_bytes());
        let sum = xor(&s[..20]);
        s[20..22].copy_from_slice(&sum.to_le_bytes());
        b.extend_from_slice(&s);
        b.extend_from_slice(&d);
        while b.len() % 4 != 0 {
            b.push(0);
        }
    }
    b
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// TAPE body: fields from offset 52, with the media name stored right after them.
fn tape_body(name: &str) -> Vec<u8> {
    let mut body = vec![0u8; 44]; // fields 52..96
    let name = utf16(name);
    body[16..18].copy_from_slice(&(name.len() as u16).to_le_bytes()); // media name address at 68: size
    body[18..20].copy_from_slice(&((52 + 44) as u16).to_le_bytes()); // offset
    body[32..34].copy_from_slice(&1024u16.to_le_bytes()); // format logical block size at 84
    body[41] = 1; // MTF major version at 93
    body.extend_from_slice(&name);
    body
}

fn sample() -> Vec<u8> {
    let mut f = vec![];
    f.extend(block(b"TAPE", &tape_body("My media"), &[], 1024));
    f.extend(block(b"SSET", &vec![0u8; 48], &[], 1024));
    f.extend(block(b"VOLB", &vec![0u8; 20], &[], 1024));
    f.extend(block(b"FILE", &vec![0u8; 16], &[(b"MQDA", vec![7u8; 5000]), (b"STAN", vec![1, 2, 3])], 1024));
    f.extend(block(b"ESET", &vec![0u8; 16], &[], 1024));
    f
}

#[test]
fn walks_blocks_and_streams() {
    let f = sample();
    let scan = mtf::scan(&mut Cursor::new(&f)).unwrap();
    assert_eq!(scan.problem, None);
    let names: Vec<String> = scan.blocks.iter().map(|b| b.name()).collect();
    assert_eq!(names, ["TAPE", "SSET", "VOLB", "FILE", "ESET"]);
    let file = &scan.blocks[3];
    let ids: Vec<String> = file.streams.iter().map(|s| s.name()).collect();
    assert_eq!(ids, ["MQDA", "STAN", "SPAD"]);
    assert_eq!(file.streams[0].len, 5000);
    assert_eq!(file.streams[1].len, 3);
    // every block starts on a logical block boundary
    assert!(scan.blocks.iter().all(|b| b.at % 1024 == 0));
    // stream data is where the header says it is
    let d = mtf::read_stream(&mut Cursor::new(&f), &file.streams[0], 10).unwrap();
    assert_eq!(d, vec![7u8; 10]);
}

#[test]
fn reads_the_strings_of_the_tape_header() {
    let f = sample();
    let scan = mtf::scan(&mut Cursor::new(&f)).unwrap();
    let tape = &scan.blocks[0];
    assert_eq!(tape.string_at(68), "My media");
    assert_eq!(tape.u16_at(84), 1024);
    assert_eq!(tape.u8_at(93), 1);
}

#[test]
fn dates_decode_from_five_packed_bytes() {
    // 2024-03-05 14:07:09
    let v: u64 = (2024u64 << 26) | (3 << 22) | (5 << 17) | (14 << 12) | (7 << 6) | 9;
    let mut body = tape_body("x");
    body[36..41].copy_from_slice(&v.to_be_bytes()[3..8]); // media date at 88
    let f = block(b"TAPE", &body, &[], 1024);
    let scan = mtf::scan(&mut Cursor::new(&f)).unwrap();
    assert_eq!(scan.blocks[0].date_at(88).as_deref(), Some("2024-03-05 14:07:09"));
}

#[test]
fn a_damaged_header_is_reported_with_the_blocks_before_it() {
    let mut f = sample();
    let second = mtf::scan(&mut Cursor::new(&f)).unwrap().blocks[1].at as usize;
    f[second + 20] ^= 0xFF; // a byte inside the SSET header
    let scan = mtf::scan(&mut Cursor::new(&f)).unwrap();
    assert_eq!(scan.blocks.len(), 1);
    assert!(scan.problem.unwrap().contains("checksum"));
}

#[test]
fn a_truncated_file_is_reported() {
    let f = sample();
    let cut = &f[..f.len() - 700];
    let scan = mtf::scan(&mut Cursor::new(cut)).unwrap();
    // the last block is described as far as it goes, and the cut is reported
    assert!(scan.problem.unwrap().contains("cut short"));
    assert_eq!(scan.blocks.len(), 5);
}

#[test]
fn zero_fill_after_the_last_block_is_accepted() {
    let mut f = sample();
    f.extend(vec![0u8; 4096]);
    let scan = mtf::scan(&mut Cursor::new(&f)).unwrap();
    assert_eq!(scan.problem, None);
    assert_eq!(scan.blocks.len(), 5);
}

#[test]
fn other_files_are_not_mtf() {
    assert!(!mtf::is_mtf(b"TSQLBAK1"));
    assert!(mtf::is_mtf(b"TAPE...."));
    let scan = mtf::scan(&mut Cursor::new(b"this is just some text, long enough to hold a header block....".to_vec())).unwrap();
    assert!(scan.blocks.is_empty());
    assert!(scan.problem.is_some());
}
