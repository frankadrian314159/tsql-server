//! `tsql-serve` — listen for TDS clients (sqlcmd, ODBC, JDBC, ADO.NET, tiberius, ...).
//!
//! ```text
//! tsql-serve [DATA_DIR] [--port N] [--bind ADDR] [--sa-password PW] [--pool-pages N]
//!             [--tls-cert FILE --tls-key FILE | --no-tls] [--force-encryption]
//! ```
use tsql_server::tds::{Config, Encryption, Listener, TlsConfig};
use tsql_server::{Server, StorageOptions};

fn usage() -> ! {
    eprintln!(
        "usage: tsql-serve [DATA_DIR] [--port N] [--bind ADDR] [--sa-password PW] [--pool-pages N]\n\
                          [--tls-cert FILE --tls-key FILE | --no-tls] [--force-encryption]\n\n\
         DATA_DIR        directory for the durable database (omitted: in-memory only)\n\
         --port N        TCP port (default 1433)\n\
         --bind ADDR     address to listen on (default 127.0.0.1)\n\
         --sa-password   password for the sa login (or set TSQL_SA_PASSWORD); without one only logins you\n\
                         create beforehand can connect\n\
         --pool-pages N  buffer pool size in 8 KiB pages\n\
         --tls-cert/--tls-key  PEM certificate (chain) and private key for TLS; without them a self-signed\n\
                         certificate is generated at start-up, as SQL Server does (clients must then trust it)\n\
         --no-tls        do not offer TLS at all\n\
         --force-encryption  refuse clients that cannot use TLS and encrypt every session"
    );
    std::process::exit(2)
}

fn main() {
    let mut dir: Option<String> = None;
    let mut port: u16 = 1433;
    let mut bind = "127.0.0.1".to_string();
    let mut sa = std::env::var("TSQL_SA_PASSWORD").ok();
    let mut opts = StorageOptions::default();
    let (mut cert, mut key, mut no_tls, mut force) = (None::<String>, None::<String>, false, false);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--bind" => bind = args.next().unwrap_or_else(|| usage()),
            "--sa-password" => sa = Some(args.next().unwrap_or_else(|| usage())),
            "--pool-pages" => opts.pool_pages = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--tls-cert" => cert = Some(args.next().unwrap_or_else(|| usage())),
            "--tls-key" => key = Some(args.next().unwrap_or_else(|| usage())),
            "--no-tls" => no_tls = true,
            "--force-encryption" => force = true,
            "-h" | "--help" => usage(),
            other if !other.starts_with('-') && dir.is_none() => dir = Some(other.to_string()),
            _ => usage(),
        }
    }
    let server = match &dir {
        Some(d) => match Server::open_with(d, opts) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot open {}: {}", d, e);
                std::process::exit(1);
            }
        },
        None => Server::new(),
    };
    if let Some(pw) = sa {
        let mut c = server.connect();
        let r = c.execute(&format!("ALTER LOGIN sa WITH PASSWORD = N'{}'", pw.replace('\'', "''")));
        if let Some(e) = r.first_error() {
            eprintln!("cannot set the sa password: {}", e);
            std::process::exit(1);
        }
    }
    let mut cfg = Config::default();
    let mut tls_note = "no encryption";
    if !no_tls {
        let tls = match (&cert, &key) {
            (Some(c), Some(k)) => TlsConfig::from_pem_files(c, k).map(|t| (t, "TLS")),
            (None, None) => TlsConfig::self_signed(vec!["localhost".into(), "127.0.0.1".into(), bind.clone()]).map(|t| (t, "TLS, self-signed certificate")),
            _ => usage(),
        };
        match tls {
            Ok((t, note)) => {
                cfg = Config::with_tls(t, if force { Encryption::Required } else { Encryption::Optional });
                tls_note = note;
            }
            Err(e) => {
                eprintln!("cannot set up TLS: {}", e);
                std::process::exit(1);
            }
        }
    } else if force {
        eprintln!("--force-encryption needs TLS");
        std::process::exit(2);
    }
    let listener = match Listener::bind(server, (bind.as_str(), port), cfg) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {}:{}: {}", bind, port, e);
            std::process::exit(1);
        }
    };
    eprintln!("tsql-server listening on {} (TDS 7.4, {}); data: {}", listener.local_addr(), tls_note, dir.as_deref().unwrap_or("in memory"));
    listener.wait();
}

// The memory allocator of Windows is slow for the many small allocations of the engine; `--features fast-alloc` links mimalloc
// instead (about a third less time on the benchmark; see docs/performance.md). It is C code, so it is not on by default.
#[cfg(feature = "fast-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
