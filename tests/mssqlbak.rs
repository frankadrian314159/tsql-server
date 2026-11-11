//! The container of recent SQL Server backups (`MSSQLBAK`, compressed chunks) and its Xpress Huffman compression.
use std::io::{Cursor, Read, Seek, SeekFrom};
use tsql_server::{mssqlbak, mtf, xpress};

/// A chunk that codes every byte 0..=254 with 8 bits and symbol 256 (a match of length 3, distance 1) with 8 bits:
/// the code of byte `b` is `b` itself and the match is 0xFF, so the bit stream is the data with its bytes in 16-bit words.
fn table_with_match() -> Vec<u8> {
    let mut t = vec![0u8; 256];
    for i in 0..127 {
        t[i] = 0x88; // symbols 2i and 2i+1 both 8 bits (symbols 0..253)
    }
    t[127] = 0x08; // symbol 254: 8 bits, symbol 255: unused
    t[128] = 0x08; // symbol 256: 8 bits, symbol 257: unused
    t
}

fn words(bytes: &[u8]) -> Vec<u8> {
    // the bit stream is read as 16-bit little-endian words, most significant bit first
    let mut v = bytes.to_vec();
    while v.len() % 2 != 0 || v.len() < 8 {
        v.push(0);
    }
    for c in v.chunks_exact_mut(2) {
        c.swap(0, 1);
    }
    v
}

#[test]
fn xpress_literals_and_a_match() {
    let mut chunk = table_with_match();
    // 'a' 'b' then the match symbol (repeat the last byte 3 times) then 'c'
    chunk.extend(words(&[b'a', b'b', 0xFF, b'c']));
    assert_eq!(xpress::decompress(&chunk, 6).unwrap(), b"abbbbc");
}

#[test]
fn xpress_rejects_a_match_before_the_start() {
    let mut chunk = table_with_match();
    chunk.extend(words(&[0xFF, 0, 0, 0]));
    let e = xpress::decompress(&chunk, 4).unwrap_err();
    assert!(e.to_string().contains("before the start"), "{}", e);
}

#[test]
fn xpress_rejects_a_table_that_is_not_a_prefix_code() {
    // three symbols of length 1 cannot all have a code
    let mut chunk = vec![0u8; 256];
    chunk[0] = 0x11;
    chunk[1] = 0x01;
    chunk.extend(vec![0u8; 8]);
    let e = xpress::decompress(&chunk, 1).unwrap_err();
    assert!(e.to_string().contains("over-subscribed"), "{}", e);
}

#[test]
fn xpress_rejects_a_chunk_without_a_table() {
    assert!(xpress::decompress(&[0u8; 10], 1).is_err());
}

#[test]
fn files_that_are_not_this_container_are_refused() {
    assert!(!mssqlbak::is_bak(b"TSQLBAK1"));
    assert!(mssqlbak::is_bak(b"MSSQLBAK\x02\x00"));
    let mut small = Cursor::new(b"MSSQLBAK".to_vec());
    assert!(mssqlbak::layout(&mut small).unwrap().is_err());
}

/// The sample backup of AdventureWorks is not part of the repository; the tests below run when it is there.
fn sample() -> Option<std::fs::File> {
    std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/AdventureWorks2025.bak")).ok()
}

#[test]
fn sample_backup_is_expanded_into_a_well_formed_tape_stream() {
    let Some(f) = sample() else { return };
    let mut ex = mssqlbak::Expanded::open(std::io::BufReader::new(f)).unwrap().unwrap();
    assert_eq!(ex.layout.chunks.len(), 3221);
    assert_eq!(ex.layout.total, 210_857_984);
    let scan = mtf::scan(&mut ex).unwrap();
    assert_eq!(scan.problem, None);
    let kinds: Vec<String> = scan.blocks.iter().map(|b| b.name()).collect();
    assert_eq!(&kinds[..5], ["SSET", "VOLB", "MSCI", "MSDA", "MSDA"]);
    assert_eq!(scan.blocks[0].string_at(64), "AdventureWorks2025-Full Database Backup");
    // the data stream is a run of 8 KiB pages after a two-byte prefix; the file header page comes first
    let data = scan.blocks[3].streams.iter().find(|s| &s.id == b"MQDA").unwrap().clone();
    assert_eq!(data.len, 200 * 1024 * 1024 + 2);
    ex.seek(SeekFrom::Start(data.data_at + 2)).unwrap();
    let mut page = vec![0u8; 8192];
    ex.read_exact(&mut page).unwrap();
    assert_eq!(page[1], 15, "page 0 is the file header page");
    assert_eq!(&page[32..38], &[0, 0, 0, 0, 1, 0], "its page id is 0 in file 1");
}

#[test]
fn a_damaged_chunk_is_an_error_not_a_panic() {
    let Some(mut f) = sample() else { return };
    let mut bytes = vec![];
    f.read_to_end(&mut bytes).unwrap();
    // damage the table of the first chunk
    for b in &mut bytes[0x2020..0x2120] {
        *b = 0xFF;
    }
    let mut ex = mssqlbak::Expanded::open(Cursor::new(bytes)).unwrap().unwrap();
    assert!(ex.chunk(0).is_err());
    // the others are still readable
    assert_eq!(ex.chunk(1).unwrap().len(), ex.layout.chunks[1].size as usize);
}
