//! `tsql-shard`: one shard node of a distributed tsql-server cluster.
//!
//!   tsql-shard --id 0 --listen 127.0.0.1:50051 [--data DIR]
//!   tsql-shard --id 0 --listen HOST:PORT --data DIR --replica 1 --peers 2=HOST:PORT,3=HOST:PORT [--raft-dir DIR]
//!
//! The second form makes the node one replica of a replicated shard: `--replica` is its number in the group, `--peers` the
//! other replicas (number=address). Every replica of a shard is given the same `--id` and its own `--data`. The control
//! node's cluster file lists all replicas of the shard in one address, separated by commas.
//!
//! Without `--data` the shard keeps its data in memory; with it the shard is durable (write-ahead log and checkpoints in
//! DIR, exactly like a single-node `tsql-serve DIR`).
use std::net::SocketAddr;
use tsql_server::Server;
use tsql_storage::{spawn, ReplicaConfig, ShardNode};

fn usage() -> ! {
    eprintln!("usage: tsql-shard --id N --listen HOST:PORT [--data DIR]\n       tsql-shard --id N --listen HOST:PORT --data DIR --replica R --peers R2=HOST:PORT,R3=HOST:PORT [--raft-dir DIR]");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let mut id: Option<u32> = None;
    let mut listen: Option<SocketAddr> = None;
    let mut data: Option<String> = None;
    let mut replica: Option<u64> = None;
    let mut peers: Vec<(u64, String)> = vec![];
    let mut raft_dir: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--id" => id = args.next().and_then(|v| v.parse().ok()),
            "--listen" => listen = args.next().and_then(|v| v.parse().ok()),
            "--data" => data = args.next(),
            "--replica" => replica = args.next().and_then(|v| v.parse().ok()),
            "--raft-dir" => raft_dir = args.next(),
            "--peers" => {
                for p in args.next().unwrap_or_default().split(',').filter(|p| !p.is_empty()) {
                    match p.split_once('=') {
                        Some((n, a)) => match n.parse() {
                            Ok(n) => peers.push((n, a.to_string())),
                            Err(_) => usage(),
                        },
                        None => usage(),
                    }
                }
            }
            _ => usage(),
        }
    }
    let (Some(id), Some(listen)) = (id, listen) else { usage() };
    let server = match &data {
        Some(dir) => match Server::open(dir) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tsql-shard: cannot open {}: {}", dir, e.message);
                std::process::exit(1);
            }
        },
        None => Server::new(),
    };
    let node = match replica {
        Some(r) => {
            let Some(d) = &data else {
                eprintln!("tsql-shard: a replica needs --data (its engine must be durable)");
                std::process::exit(2);
            };
            let dir = raft_dir.clone().unwrap_or_else(|| format!("{}.raft", d));
            match ShardNode::replicated(id, server, ReplicaConfig::new(r, peers.clone(), dir)) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("tsql-shard: cannot start the replica: {}", e);
                    std::process::exit(1);
                }
            }
        }
        None => ShardNode::new(id, server),
    };
    let running = match spawn(node, listen).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tsql-shard: cannot listen on {}: {}", listen, e);
            std::process::exit(1);
        }
    };
    eprintln!("tsql-shard {} listening on {}{}", id, running.addr, data.as_deref().map(|d| format!(" (data in {})", d)).unwrap_or_else(|| " (in memory)".into()));
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("tsql-shard {}: shutting down", id);
    running.shutdown().await;
}
