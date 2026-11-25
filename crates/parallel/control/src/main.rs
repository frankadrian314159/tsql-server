//! `tsql-cluster`: a command-line shell for a distributed tsql-server cluster.
//!
//!   tsql-cluster --config cluster.json < script.sql
//!   tsql-cluster --make-certs DIR NAME...
//!
//! Connections to the shards use mutual TLS: give `--node-ca FILE --node-cert FILE --node-key FILE`, or `--insecure` to
//! leave them unprotected. `--make-certs` writes a certificate authority and a certificate for every node into DIR.
//!
//! Reads T-SQL batches from standard input (a batch ends at a line `GO`, or at the end of input), runs them through the
//! control node and prints the result sets as tab-separated text, with the plan that was used on stderr (`--plan`).
//!
//! As a server (`--listen PORT`) it serves SQL Server clients over TDS. More flags for a server:
//!
//! * `--bind HOST` the address to listen on (default 127.0.0.1; use 0.0.0.0 for clients on other machines);
//! * TLS for clients is as in `tsql-serve`: offered with a self-signed certificate, or `--tls-cert FILE --tls-key FILE`;
//!   `--force-encryption`, `--tls13`, `--no-tls`. Listening on an address that other machines can reach needs
//!   `--force-encryption`, or `--insecure-clients`;
//! * `--wait-shards SECS` wait that long for every shard to answer before serving (default 30: shards and control node may be
//!   started together), and exit with an error when some do not;
//! * `--metrics-listen HOST:PORT` serve `/metrics` (Prometheus text: CPU, memory, disk and network use of the control node and
//!   every shard, sessions, lock waits, slots) and `/health`;
//! * `--autoscale --scale-up-cmd CMD --scale-down-cmd CMD` grow and shrink the cluster by the resource thresholds (see
//!   `tsql_control::autoscale`): `CMD` for up starts a shard node and prints its `host:port` last, the one for down gets
//!   `TSQL_SHARD_ADDR` and deletes the node. Thresholds: `--cpu-high PCT` (75) `--cpu-low PCT` (20) `--mem-high PCT` (85)
//!   `--disk-high-mb N` (150) `--net-high-mb N` (500) `--sustain N` readings (6) `--scale-interval SECS` (10) `--cooldown SECS`
//!   (300) `--min-shards N` (1) `--max-shards N` (64);
//! * SIGTERM and Ctrl-C stop it gracefully: it stops serving, which closes the client sessions (their open transactions are
//!   rolled back on the shards), and writes its state.
use std::io::BufRead;
use tsql_control::autoscale::{Action, Autoscaler, CommandProvisioner};
use tsql_control::rebalance::ScalePolicy;
use tsql_control::Cluster;
use tsql_server::tds::ListenerTls;
use tsql_server::{convert::display, Event};
use tsql_shared_ces::tls::{make_certs, TlsFlags};
use tsql_shared_ces::ClusterConfig;

#[cfg(feature = "fast-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn fail(why: impl std::fmt::Display) -> ! {
    eprintln!("tsql-cluster: {}", why);
    std::process::exit(2)
}

fn main() {
    let mut config: Option<String> = None;
    let mut show_plan = false;
    let mut state: Option<String> = None;
    let mut listen: Option<u16> = None;
    let mut sa_password: Option<String> = std::env::var("TSQL_SA_PASSWORD").ok();
    let mut bind = "127.0.0.1".to_string();
    let mut wait_shards: u64 = 30;
    let mut metrics_listen: Option<String> = None;
    let (mut autoscale, mut up_cmd, mut down_cmd) = (false, None::<String>, None::<String>);
    let mut scale_interval: u64 = 10;
    let mut policy = ScalePolicy::default();
    let (mut node_tls, mut client_tls, mut insecure_clients) = (TlsFlags::default(), ListenerTls::default(), false);
    let mut args = std::env::args().skip(1);
    let num = |v: Option<String>| v.and_then(|v| v.parse::<f64>().ok()).unwrap_or_else(|| {
        eprintln!("tsql-cluster: a number is needed after the flag");
        std::process::exit(2)
    });
    while let Some(a) = args.next() {
        if node_tls.take(&a, &mut args) || client_tls.take(&a, &mut args) {
            continue;
        }
        match a.as_str() {
            "--make-certs" => {
                let dir = args.next().unwrap_or_else(|| fail("--make-certs needs a directory and the names of the nodes"));
                let names: Vec<String> = args.collect();
                make_certs(&dir, &names).unwrap_or_else(|e| fail(e));
                eprintln!("tsql-cluster: certificates are in {}", dir);
                return;
            }
            "--insecure-clients" => insecure_clients = true,
            "--config" => config = args.next(),
            "--plan" => show_plan = true,
            "--state" => state = args.next(),
            "--listen" => listen = args.next().and_then(|v| v.parse().ok()),
            "--sa-password" => sa_password = args.next(),
            "--bind" => bind = args.next().unwrap_or(bind),
            "--wait-shards" => wait_shards = num(args.next()) as u64,
            "--metrics-listen" => metrics_listen = args.next(),
            "--autoscale" => autoscale = true,
            "--scale-up-cmd" => up_cmd = args.next(),
            "--scale-down-cmd" => down_cmd = args.next(),
            "--scale-interval" => scale_interval = num(args.next()) as u64,
            "--cpu-high" => policy.cpu_high_pct = num(args.next()),
            "--cpu-low" => policy.cpu_low_pct = num(args.next()),
            "--mem-high" => policy.mem_high_pct = num(args.next()),
            "--disk-high-mb" => policy.disk_high_bps = num(args.next()) * 1e6,
            "--net-high-mb" => policy.net_high_bps = num(args.next()) * 1e6,
            "--sustain" => policy.sustain = num(args.next()) as usize,
            "--cooldown" => policy.cooldown_secs = num(args.next()) as u64,
            "--min-shards" => policy.min_shards = num(args.next()) as usize,
            "--max-shards" => policy.max_shards = num(args.next()) as usize,
            _ => {
                eprintln!("usage: tsql-cluster --config cluster.json [--state FILE] [--plan] < script.sql\n       tsql-cluster --config cluster.json [--state FILE] --listen PORT [--sa-password PW]   (serve TDS clients)\n         [--bind HOST] [--tls-cert FILE --tls-key FILE | --no-tls] [--force-encryption] [--tls13] [--insecure-clients]\n       tsql-cluster --make-certs DIR NAME...\n       connections to the shards need --node-ca FILE --node-cert FILE --node-key FILE, or --insecure");
                std::process::exit(2);
            }
        }
    }
    let Some(path) = config else {
        eprintln!("usage: tsql-cluster --config cluster.json [--plan] < script.sql");
        std::process::exit(2);
    };
    let cfg = ClusterConfig::load(&path).unwrap_or_else(|e| {
        eprintln!("tsql-cluster: {}", e);
        std::process::exit(1);
    });
    let node_tls = node_tls.load().unwrap_or_else(|e| fail(e));
    if node_tls.is_none() {
        eprintln!("tsql-cluster: WARNING: connections to the shards are neither encrypted nor authenticated (--insecure)");
    }
    // how clients will be served: refuse an unsafe address before anything is started
    let serving = listen.map(|_| {
        if let Err(why) = client_tls.check_exposure(&bind) {
            if !insecure_clients {
                fail(format!("{}, or --insecure-clients to listen unprotected", why));
            }
            eprintln!("tsql-cluster: WARNING: {}; continuing (--insecure-clients)", why);
        }
        client_tls.config(&bind).unwrap_or_else(|e| fail(e))
    });
    let cluster = match &state {
        Some(p) => Cluster::open_with(p, &cfg, node_tls),
        None => Cluster::connect_with(&cfg, node_tls),
    }
    .unwrap_or_else(|e| {
        eprintln!("tsql-cluster: {}", e.message);
        std::process::exit(1);
    });
    if listen.is_some() && wait_shards > 0 {
        if let Err(down) = cluster.wait_for_shards(std::time::Duration::from_secs(wait_shards)) {
            eprintln!("tsql-cluster: shards {:?} do not answer after {}s", down, wait_shards);
            std::process::exit(1);
        }
    }
    if let Some(addr) = &metrics_listen {
        let c = cluster.clone();
        match tsql_shared_ces::metrics::serve_http(addr, move || c.prometheus()) {
            Ok(a) => eprintln!("tsql-cluster: metrics on http://{}/metrics", a),
            Err(e) => {
                eprintln!("tsql-cluster: cannot serve metrics on {}: {}", addr, e);
                std::process::exit(1);
            }
        }
    }
    if let Some((port, (tds, encryption))) = listen.zip(serving) {
        // serve SQL Server clients (TDS): the control node's own engine is the front, and an interceptor sends what touches
        // distributed tables to the cluster
        cluster.enable_tds();
        let server = cluster.mirror_server();
        if let Some(pw) = sa_password {
            let mut c = server.connect_internal();
            let r = c.execute(&format!("ALTER LOGIN sa WITH PASSWORD = N'{}'", pw.replace('\'', "''")));
            if let Some(e) = r.first_error() {
                eprintln!("tsql-cluster: cannot set the sa password: {}", e);
                std::process::exit(1);
            }
        }
        match tsql_server::tds::Listener::bind(server, (bind.as_str(), port), tds) {
            Ok(l) => {
                eprintln!("tsql-cluster: control node listening on {} (TDS, {}), {} shards", l.local_addr(), encryption, cluster.shard_count());
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                if autoscale {
                    let (Some(up), Some(down)) = (up_cmd.clone(), down_cmd.clone()) else {
                        eprintln!("tsql-cluster: --autoscale needs --scale-up-cmd and --scale-down-cmd");
                        std::process::exit(2);
                    };
                    let mut scaler = Autoscaler::new(cluster.clone(), policy.clone(), Box::new(CommandProvisioner { up, down }));
                    let stop = stop.clone();
                    eprintln!("tsql-cluster: autoscaling every {}s: cpu {}..{}%, memory {}%, disk {} MB/s, network {} MB/s, {} readings, {} to {} shards", scale_interval, policy.cpu_low_pct, policy.cpu_high_pct, policy.mem_high_pct, policy.disk_high_bps / 1e6, policy.net_high_bps / 1e6, policy.sustain, policy.min_shards, policy.max_shards);
                    std::thread::spawn(move || {
                        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                            match scaler.step() {
                                Action::Nothing(_) => {}
                                other => eprintln!("tsql-cluster: autoscaler: {:?}", other),
                            }
                            // (sleeps in short steps so that a stop is noticed)
                            for _ in 0..scale_interval.max(1) * 4 {
                                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(250));
                            }
                        }
                    });
                }
                wait_for_stop();
                eprintln!("tsql-cluster: stopping: no more sessions, writing the state");
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                l.shutdown();
                cluster.shutdown();
                eprintln!("tsql-cluster: stopped");
                return;
            }
            Err(e) => {
                eprintln!("tsql-cluster: cannot listen on port {}: {}", port, e);
                std::process::exit(1);
            }
        }
    }
    let mut session = cluster.session();
    let mut batch = String::new();
    let mut failed = false;
    let mut run = |session: &mut tsql_control::ClusterSession, batch: &mut String| {
        if batch.trim().is_empty() {
            batch.clear();
            return;
        }
        let res = session.execute(batch);
        batch.clear();
        for ev in &res.events {
            match ev {
                Event::Rows(rs) => {
                    println!("{}", rs.columns.join("\t"));
                    for r in &rs.rows {
                        println!("{}", r.iter().map(display).collect::<Vec<_>>().join("\t"));
                    }
                    println!("({} rows)", rs.rows.len());
                }
                Event::RowCount(n) => println!("({} rows affected)", n),
                Event::Message(m) => println!("{}", m),
                Event::Error(e) => {
                    eprintln!("Msg {}, Level {}, State {}: {}", e.number, e.severity, e.state, e.message);
                    failed = true;
                }
            }
        }
        if show_plan {
            for p in session.last_plan() {
                eprintln!("plan: {}", p);
            }
        }
    };
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.trim().eq_ignore_ascii_case("GO") {
            run(&mut session, &mut batch);
        } else {
            batch.push_str(&line);
            batch.push('\n');
        }
    }
    run(&mut session, &mut batch);
    std::process::exit(if failed { 1 } else { 0 });
}

/// Block until SIGTERM or SIGINT (Ctrl-C), where the platform has SIGTERM.
fn wait_for_stop() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut term) = signal(SignalKind::terminate()) {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                return;
            }
        }
        let _ = tokio::signal::ctrl_c().await;
    });
}
