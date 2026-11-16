//! `tsql-cluster`: a command-line shell for a distributed tsql-server cluster.
//!
//!   tsql-cluster --config cluster.json < script.sql
//!
//! Reads T-SQL batches from standard input (a batch ends at a line `GO`, or at the end of input), runs them through the
//! control node and prints the result sets as tab-separated text, with the plan that was used on stderr (`--plan`).
use std::io::BufRead;
use tsql_control::Cluster;
use tsql_server::{convert::display, Event};
use tsql_shared_ces::ClusterConfig;

fn main() {
    let mut config: Option<String> = None;
    let mut show_plan = false;
    let mut state: Option<String> = None;
    let mut listen: Option<u16> = None;
    let mut sa_password: Option<String> = std::env::var("TSQL_SA_PASSWORD").ok();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = args.next(),
            "--plan" => show_plan = true,
            "--state" => state = args.next(),
            "--listen" => listen = args.next().and_then(|v| v.parse().ok()),
            "--sa-password" => sa_password = args.next(),
            _ => {
                eprintln!("usage: tsql-cluster --config cluster.json [--state FILE] [--plan] < script.sql\n       tsql-cluster --config cluster.json [--state FILE] --listen PORT [--sa-password PW]   (serve TDS clients)");
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
    let cluster = match &state {
        Some(p) => Cluster::open(p, &cfg),
        None => Cluster::connect(&cfg),
    }
    .unwrap_or_else(|e| {
        eprintln!("tsql-cluster: {}", e.message);
        std::process::exit(1);
    });
    if let Some(port) = listen {
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
        match tsql_server::tds::Listener::bind(server, ("127.0.0.1", port), tsql_server::tds::Config::default()) {
            Ok(l) => {
                eprintln!("tsql-cluster: control node listening on {} (TDS), {} shards", l.local_addr(), cluster.shard_count());
                l.wait();
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
