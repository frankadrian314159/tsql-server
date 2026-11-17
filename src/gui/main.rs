//! `tsql-gui` — the desktop application for tsql-server: browse and edit tables, design them,
//! run T-SQL, and watch the storage engine.
//!
//! ```text
//! tsql-gui [DATA_DIR] [--pool-pages N] [--embedded]
//! tsql-gui --connect HOST[:PORT] [-U LOGIN] [-P PASSWORD] [-d DATABASE] [--encrypt]
//! ```
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod datasheet;
mod design;
mod query;
mod remote;
mod sample;
mod session;
mod ui;

fn main() -> Result<(), eframe::Error> {
    let mut dir: Option<std::path::PathBuf> = None;
    let mut opts = tsql_server::StorageOptions::default();
    let mut target: Option<remote::Target> = None;
    let mut embedded = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pool-pages" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(n) => opts.pool_pages = n,
                None => {
                    eprintln!("--pool-pages needs a number of 8 KiB pages");
                    std::process::exit(2);
                }
            },
            "--connect" | "-S" => {
                let spec = args.next().unwrap_or_default();
                let mut t = target.take().unwrap_or_default();
                match spec.rsplit_once(':') {
                    Some((h, p)) if p.parse::<u16>().is_ok() => {
                        t.host = h.to_string();
                        t.port = p.parse().unwrap_or(1433);
                    }
                    _ if !spec.is_empty() => t.host = spec,
                    _ => {
                        eprintln!("--connect needs HOST or HOST:PORT");
                        std::process::exit(2);
                    }
                }
                target = Some(t);
            }
            "-U" | "-P" | "-d" => {
                let v = args.next().unwrap_or_default();
                let t = target.get_or_insert_with(Default::default);
                match a.as_str() {
                    "-U" => t.login = v,
                    "-P" => t.password = v,
                    _ => t.database = v,
                }
            }
            "--embedded" => embedded = true,
            "--encrypt" => target.get_or_insert_with(Default::default).encrypt = true,
            "-h" | "--help" => {
                println!("usage: tsql-gui [DATA_DIR] [--pool-pages N] [--embedded]\n       tsql-gui --connect HOST[:PORT] [-U LOGIN] [-P PASSWORD] [-d DATABASE] [--encrypt]\n\nWithout a data directory the database lives in memory only; File > Open data folder... opens one later. --connect works on a server over TDS instead (a cluster's control node, or any SQL Server work-alike); File > Connect to server... does the same from inside.\n\nWithout --connect the app works on the server at 127.0.0.1:1433 when one is listening there (set TSQL_PASSWORD, and TSQL_LOGIN if it is not sa, for its login), and otherwise starts a single-node server on that port, in this process, and connects to it. --embedded keeps the engine inside the window with no network (the way the app worked before).");
                return Ok(());
            }
            other if !other.starts_with('-') && dir.is_none() => dir = Some(other.into()),
            other => {
                eprintln!("unknown argument '{other}'");
                std::process::exit(2);
            }
        }
    }
    // Without a server named: the one at port 1433, or a server of our own there
    let mut held: Option<tsql_server::tds::Listener> = None;
    if target.is_none() && !embedded {
        let mut t = remote::Target::default();
        if let Ok(p) = std::env::var("TSQL_PASSWORD") {
            t.password = p;
        }
        if let Ok(l) = std::env::var("TSQL_LOGIN") {
            t.login = l;
        }
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], t.port).into();
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)).is_ok() {
            target = Some(t);
        } else {
            match own_server(dir.as_deref(), opts, t.port) {
                Ok((l, password)) => {
                    t.password = password;
                    held = Some(l);
                    target = Some(t);
                    dir = None;
                }
                Err(e) => eprintln!("tsql-gui: cannot start a server on port {}: {e}; the engine runs inside the window instead", t.port),
            }
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 760.0]).with_min_inner_size([640.0, 420.0]).with_title("tsql-server"),
        ..Default::default()
    };
    eframe::run_native("tsql-server", options, Box::new(move |cc| {
        let mut app = app::App::new(cc, dir, opts);
        if let Some(l) = held {
            app.hold(l);
        }
        if let Some(t) = target {
            app.connect_or_ask(t);
        }
        Ok(Box::new(app))
    }))
}

/// A single-node server in this process, listening on `port` of this machine only. Its `sa` login gets a password that only this
/// process knows (it is the client too). `dir` keeps the data on disk.
fn own_server(dir: Option<&std::path::Path>, opts: tsql_server::StorageOptions, port: u16) -> Result<(tsql_server::tds::Listener, String), String> {
    use rand::Rng;
    let server = match dir {
        Some(d) => tsql_server::Server::open_with(d, opts).map_err(|e| format!("could not open '{}': {}", d.display(), e.message))?,
        None => tsql_server::Server::new(),
    };
    let password: String = rand::thread_rng().sample_iter(rand::distributions::Alphanumeric).take(24).map(char::from).collect();
    let r = server.connect().execute(&format!("ALTER LOGIN sa WITH PASSWORD = N'{password}'"));
    if let Some(e) = r.first_error() {
        return Err(e.message.clone());
    }
    let l = tsql_server::tds::Listener::bind(server, ("127.0.0.1", port), tsql_server::tds::Config::default()).map_err(|e| e.to_string())?;
    Ok((l, password))
}

// The memory allocator of Windows is slow for the many small allocations of the engine; `--features fast-alloc` links mimalloc
// instead (about a third less time on the benchmark; see docs/performance.md). It is C code, so it is not on by default.
#[cfg(feature = "fast-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;

    #[test]
    fn a_server_of_our_own_takes_the_login_only_this_process_knows() {
        let (l, password) = own_server(None, tsql_server::StorageOptions::default(), 0).expect("a server");
        let mut t = remote::Target { port: l.local_addr().port(), password, ..Default::default() };
        let mut b = remote::RemoteBackend::connect(&t).expect("the generated login works");
        assert_eq!(b.query("SELECT 1 + 1").unwrap()[0][0].to_string(), "2");
        t.password = "guess".into();
        assert!(remote::RemoteBackend::connect(&t).is_err());
    }
}
