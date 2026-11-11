//! `tsql-bakinfo`: describe a native SQL Server backup (`.bak`) without restoring it.
//!
//! It walks the Microsoft Tape Format container (see `tsql_server::mtf`) and prints every block and stream, so one can
//! see how a file is built: the software that wrote it, whether it is compressed or encrypted, and how its data is laid out.
//! It only reads the file.
use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use tsql_server::mssqlbak;
use tsql_server::mtf::{self, Block};

fn usage() -> ! {
    eprintln!("usage: tsql-bakinfo FILE.bak [--hex N] [--all] [--extract OUT]");
    eprintln!("  --hex N  also show the first N bytes of every stream other than padding");
    eprintln!("  --all    list every block (by default a long run of blocks of one kind is summarised)");
    eprintln!("  --extract OUT  write the expanded stream (Microsoft Tape Format) of a compressed backup to OUT");
    std::process::exit(2);
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for (i, chunk) in b.chunks(16).enumerate() {
        let h: Vec<String> = chunk.iter().map(|x| format!("{:02x}", x)).collect();
        s.push_str(&format!("      {:06x}  {:<48} {}\n", i * 16, h.join(" "), mtf::printable(chunk)));
    }
    s
}

fn describe(b: &Block) {
    match &b.kind {
        b"TAPE" => {
            println!("    media family id    {:#x}", b.u32_at(52));
            println!("    attributes         {:#x}", b.u32_at(56));
            println!("    media sequence     {}", b.u16_at(60));
            println!("    password encrypt   {}", b.u16_at(62));
            println!("    media name         '{}'", b.string_at(68));
            println!("    description        '{}'", b.string_at(72));
            println!("    software           '{}'", b.string_at(80));
            println!("    block size         {}", b.u16_at(84));
            println!("    vendor id          {:#x}", b.u16_at(86));
            println!("    media date         {}", b.date_at(88).unwrap_or_default());
            println!("    MTF version        {}", b.u8_at(93));
        }
        b"SSET" => {
            println!("    attributes         {:#x}", b.u32_at(52));
            println!("    password encrypt   {}", b.u16_at(56));
            println!("    compression        {}", b.u16_at(58));
            println!("    vendor id          {:#x}", b.u16_at(60));
            println!("    data set number    {}", b.u16_at(62));
            println!("    data set name      '{}'", b.string_at(64));
            println!("    description        '{}'", b.string_at(68));
            println!("    user               '{}'", b.string_at(76));
            println!("    write date         {}", b.date_at(88).unwrap_or_default());
            println!("    software version   {}.{}", b.u8_at(93), b.u8_at(94));
            println!("    MTF version        {}.{}", b.u8_at(93), b.u8_at(96));
        }
        _ => {}
    }
}

fn main() {
    let mut path = None;
    let mut hex_bytes = 0usize;
    let mut all = false;
    let mut extract = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--hex" => hex_bytes = args.next().and_then(|n| n.parse().ok()).unwrap_or_else(|| usage()),
            "--all" => all = true,
            "--extract" => extract = Some(args.next().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            _ if path.is_none() => path = Some(a),
            _ => usage(),
        }
    }
    let path = path.unwrap_or_else(|| usage());
    let f = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tsql-bakinfo: cannot open {}: {}", path, e);
            std::process::exit(1);
        }
    };
    let mut r = BufReader::with_capacity(1 << 16, f);
    let mut head = [0u8; 8];
    let _ = r.read_exact(&mut head);
    r.rewind().unwrap();
    if mssqlbak::is_bak(&head) {
        let mut ex = match mssqlbak::Expanded::open(r) {
            Ok(Ok(e)) => e,
            Ok(Err(why)) => {
                eprintln!("tsql-bakinfo: {}: {}", path, why);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("tsql-bakinfo: cannot read {}: {}", path, e);
                std::process::exit(1);
            }
        };
        let l = &ex.layout;
        println!("{}: MSSQLBAK container, header words {:x?}", path, l.header_words);
        println!(
            "  {} compressed chunk(s): {} bytes in the file expand to {} bytes ({:.1}x); chunks end at offset {:#x}",
            l.chunks.len(),
            l.comp_total,
            l.total,
            l.total as f64 / l.comp_total.max(1) as f64,
            l.end
        );
        if let Some(out) = extract {
            let mut w = std::io::BufWriter::new(File::create(&out).unwrap_or_else(|e| {
                eprintln!("tsql-bakinfo: cannot create {}: {}", out, e);
                std::process::exit(1);
            }));
            for i in 0..ex.layout.chunks.len() {
                match ex.chunk(i) {
                    Ok(d) => w.write_all(d).unwrap(),
                    Err(e) => {
                        eprintln!("tsql-bakinfo: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            w.flush().unwrap();
            println!("wrote {} bytes to {}", ex.layout.total, out);
        }
        report(&mut ex, &path, hex_bytes, all);
    } else {
        report(&mut r, &path, hex_bytes, all);
    }
}

fn report<R: Read + Seek>(r: &mut R, path: &str, hex_bytes: usize, all: bool) {
    let scan = match mtf::scan(r) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tsql-bakinfo: cannot read {}: {}", path, e);
            std::process::exit(1);
        }
    };
    println!("{}: {} bytes, {} block(s)", path, scan.file_len, scan.blocks.len());
    if scan.blocks.is_empty() {
        println!("not a Microsoft Tape Format stream{}", scan.problem.map(|p| format!(": {}", p)).unwrap_or_default());
        std::process::exit(1);
    }
    let mut shown_of_kind = std::collections::HashMap::<String, usize>::new();
    let mut skipped = std::collections::BTreeMap::<String, (u64, u64)>::new();
    for (i, b) in scan.blocks.iter().enumerate() {
        let n = shown_of_kind.entry(b.name()).or_default();
        *n += 1;
        let streams_bytes: u64 = b.streams.iter().map(|s| s.len).sum();
        if !all && *n > 8 {
            let e = skipped.entry(b.name()).or_default();
            e.0 += 1;
            e.1 += streams_bytes;
            continue;
        }
        println!("block {} at {:#x}: {}  attrs {:#x}  fla {}  strings {}  streams {}", i, b.at, b.name(), b.attrs, b.fla, match b.string_type { 1 => "ANSI", 2 => "UTF-16", _ => "none" }, b.streams.len());
        describe(b);
        for s in &b.streams {
            println!("    stream {} at {:#x}: {} bytes  compression {}  encryption {}  fs-attrs {:#x}  media-attrs {:#x}", s.name(), s.data_at, s.len, s.compression, s.encryption, s.fs_attrs, s.media_attrs);
            if hex_bytes > 0 && &s.id != b"SPAD" {
                match mtf::read_stream(r, s, hex_bytes) {
                    Ok(d) => print!("{}", hex(&d)),
                    Err(e) => println!("      (cannot read: {})", e),
                }
            }
        }
    }
    for (k, (n, bytes)) in &skipped {
        println!("... and {} more {} block(s) holding {} bytes in streams (use --all to list them)", n, k, bytes);
    }
    let mut totals = std::collections::BTreeMap::<String, (u64, u64)>::new();
    for b in &scan.blocks {
        for s in &b.streams {
            let e = totals.entry(s.name()).or_default();
            e.0 += 1;
            e.1 += s.len;
        }
    }
    println!("streams by kind:");
    for (k, (n, bytes)) in &totals {
        println!("  {}  {} stream(s)  {} bytes", k, n, bytes);
    }
    match &scan.problem {
        Some(p) => {
            println!("the scan stopped early: {}", p);
            std::process::exit(1);
        }
        None => println!("the container is well formed"),
    }
}
