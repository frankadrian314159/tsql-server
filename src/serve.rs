//! `tsql-serve` — listen for TDS clients (sqlcmd, ODBC, JDBC, ADO.NET, tiberius, ...).
//!
//! ```text
//! tsql-serve [DATA_DIR] [--port N] [--bind ADDR] [--sa-password PW] [--pool-pages N]
//! ```
use tsql_server::tds::{Config, Listener};
use tsql_server::{Server, StorageOptions};

fn usage() -> ! {
    eprintln!(
        "usage: tsql-serve [DATA_DIR] [--port N] [--bind ADDR] [--sa-password PW] [--pool-pages N]\n\n\
         DATA_DIR        directory for the durable database (omitted: in-memory only)\n\
         --port N        TCP port (default 1433)\n\
         --bind ADDR     address to listen on (default 127.0.0.1; the connection is not encrypted)\n\
         --sa-password   password for the sa login (or set TSQL_SA_PASSWORD); without one only logins you\n\
                         create beforehand can connect\n\
         --pool-pages N  buffer pool size in 8 KiB pages"
    );
    std::process::exit(2)
}

fn main() {
    let mut dir: Option<String> = None;
    let mut port: u16 = 1433;
    let mut bind = "127.0.0.1".to_string();
    let mut sa = std::env::var("TSQL_SA_PASSWORD").ok();
    let mut opts = StorageOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--bind" => bind = args.next().unwrap_or_else(|| usage()),
            "--sa-password" => sa = Some(args.next().unwrap_or_else(|| usage())),
            "--pool-pages" => opts.pool_pages = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
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
    let listener = match Listener::bind(server, (bind.as_str(), port), Config::default()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {}:{}: {}", bind, port, e);
            std::process::exit(1);
        }
    };
    eprintln!("tsql-server listening on {} (TDS 7.4, no encryption); data: {}", listener.local_addr(), dir.as_deref().unwrap_or("in memory"));
    listener.wait();
}
