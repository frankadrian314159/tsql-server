//! Mutual TLS between the nodes: a cluster with certificates answers as one engine does, and anything without a
//! certificate of the cluster's authority is refused.
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::Empty;
use std::path::Path;
use std::time::{Duration, Instant};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
use tsql_control::Cluster;
use tsql_server::convert::display;
use tsql_server::{BatchResult, Server};
use tsql_shared_ces::tls::{make_certs, NodeTls, NODE_NAME};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, spawn_on, ReplicaConfig, RunningShard, ShardNode};

/// Certificates for `names`, signed by a new authority in `dir`.
fn certificates<const N: usize>(dir: &Path, names: [&str; N]) -> [NodeTls; N] {
    make_certs(dir, &names.map(String::from)).unwrap();
    names.map(|n| NodeTls::from_files(dir.join("ca.pem"), dir.join(format!("{n}.pem")), dir.join(format!("{n}.key"))).unwrap())
}

fn shard(rt: &tokio::runtime::Runtime, id: u32, tls: Option<NodeTls>) -> RunningShard {
    let node = ShardNode::new(id, Server::new()).with_tls(tls);
    rt.block_on(spawn(node, "127.0.0.1:0".parse().unwrap())).unwrap()
}

fn config(shards: &[RunningShard]) -> ClusterConfig {
    ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() }
}

fn rows(result: BatchResult) -> Vec<Vec<String>> {
    assert!(result.first_error().is_none(), "{:?}", result.first_error());
    result.resultsets().first().map(|r| r.rows.iter().map(|row| row.iter().map(display).collect()).collect()).unwrap_or_default()
}

#[test]
fn a_secured_cluster_answers_queries_moves_rows_and_takes_new_shards() {
    let dir = tempfile::tempdir().unwrap();
    let [node] = certificates(dir.path(), ["node"]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shards: Vec<_> = (0..3).map(|i| shard(&rt, i, Some(node.clone()))).collect();
    let cluster = Cluster::connect_with(&config(&shards[..2]), Some(node)).unwrap();
    let (mut session, one) = (cluster.session(), Server::new());
    let mut oracle = one.connect();
    let values: Vec<String> = (1..=200).map(|i| format!("({i}, {})", 1 + i * 7 % 200)).collect();
    for sql in ["CREATE TABLE t (id INT PRIMARY KEY, other INT)".to_string(), format!("INSERT INTO t VALUES {}", values.join(", "))] {
        rows(session.execute(&sql));
        rows(oracle.execute(&sql));
    }
    // too big to broadcast, and joined on a column that is not the key: shards send rows to one another
    cluster.set_broadcast_limit(10);
    let join = "SELECT a.other % 5 AS g, COUNT(*) AS n, SUM(b.other) AS s FROM t a JOIN t b ON b.id = a.other GROUP BY a.other % 5 ORDER BY g";
    let expected = rows(oracle.execute(join));
    assert_eq!(rows(session.execute(join)), expected);
    assert!(session.last_plan().iter().any(|l| l.contains("repartition")), "{:?}", session.last_plan());
    // a shard added later is reached with the same certificates, and rows move to it
    cluster.add_shard(&shards[2].addr.to_string()).unwrap();
    assert_eq!(rows(session.execute(join)), expected);
    assert_ne!(display(&shards[2].server.connect().query("SELECT COUNT(*) FROM t").unwrap()[0][0]), "0");
    assert!(cluster.wait_for_shards(Duration::ZERO).is_ok());
}

/// Whether a control node using `tls` gets an answer from the shards.
fn reaches(shards: &[RunningShard], tls: Option<NodeTls>) -> bool {
    Cluster::connect_with(&config(shards), tls).unwrap().wait_for_shards(Duration::ZERO).is_ok()
}

#[test]
fn only_holders_of_the_authoritys_certificates_get_through() {
    let (ours, theirs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let [node] = certificates(ours.path(), ["node"]);
    let [stranger] = certificates(theirs.path(), ["stranger"]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shards = [shard(&rt, 0, Some(node.clone()))];
    assert!(reaches(&shards, Some(node.clone())));
    // unprotected, and another authority's certificate
    assert!(!reaches(&shards, None));
    assert!(!reaches(&shards, Some(stranger.clone())));
    // a caller that believes our shard but shows a stranger's certificate
    let mixed = NodeTls::from_files(ours.path().join("ca.pem"), theirs.path().join("stranger.pem"), theirs.path().join("stranger.key"));
    assert!(!reaches(&shards, Some(mixed.unwrap())));
    // a caller that believes our shard and shows no certificate at all
    let authority = Certificate::from_pem(std::fs::read(ours.path().join("ca.pem")).unwrap());
    let anonymous = ClientTlsConfig::new().ca_certificate(authority).domain_name(NODE_NAME);
    let address = format!("https://{}", shards[0].addr);
    let answered = rt.block_on(async {
        let endpoint = Endpoint::from_shared(address).unwrap().tls_config(anonymous).unwrap();
        match endpoint.connect().await {
            Ok(channel) => FlightServiceClient::new(channel).list_actions(Empty {}).await.is_ok(),
            Err(_) => false,
        }
    });
    assert!(!answered);
    // and a control node does not talk to a shard that is not of its cluster
    let impostor = [shard(&rt, 0, Some(stranger))];
    assert!(!reaches(&impostor, Some(node.clone())));
    assert!(!reaches(&[shard(&rt, 0, None)], Some(node)));
}

#[test]
fn replicas_replicate_and_take_a_new_member_over_mutual_tls() {
    let dir = tempfile::tempdir().unwrap();
    let [node] = certificates(dir.path(), ["node"]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listeners: Vec<_> = (0..4).map(|_| rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap()).collect();
    let addrs: Vec<String> = listeners.iter().map(|l| l.local_addr().unwrap().to_string()).collect();
    // three replicas, and a fourth that joins them later
    let replicas: Vec<_> = (1u64..)
        .zip(listeners)
        .map(|(id, listener)| {
            let peers = (1u64..).zip(addrs[..3].iter().cloned()).filter(|p| p.0 != id).collect();
            let mut cfg = ReplicaConfig::new(id, peers, dir.path().join(format!("raft{id}")));
            (cfg.tick_ms, cfg.election_ticks, cfg.heartbeat_ticks, cfg.sync) = (10, 30, 3, false);
            (cfg.addr, cfg.join, cfg.tls) = (addrs[id as usize - 1].clone(), id == 4, Some(node.clone()));
            let server = Server::open(dir.path().join(format!("data{id}"))).unwrap();
            rt.block_on(async { spawn_on(ShardNode::replicated(0, server, cfg).unwrap(), listener).await }).unwrap()
        })
        .collect();
    let shard = ShardInfo { id: 0, addr: addrs[..3].join(",") };
    let cluster = Cluster::connect_with(&ClusterConfig { shards: vec![shard] }, Some(node)).unwrap();
    // a leader can only be elected, and a change committed, if the replicas reach one another
    assert!(cluster.wait_for_shards(Duration::from_secs(20)).is_ok());
    let mut session = cluster.session();
    let give_up = Instant::now() + Duration::from_secs(20);
    while let Some(e) = session.execute("CREATE TABLE t (a INT PRIMARY KEY)").first_error() {
        assert!(Instant::now() < give_up, "{e:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    rows(session.execute("INSERT INTO t VALUES (1), (2), (3)"));
    cluster.add_replica(0, 4, &addrs[3]).unwrap();
    assert_eq!(rows(session.execute("SELECT COUNT(*) FROM t")), vec![vec!["3"]]);
    for replica in &replicas {
        while !replica.server.connect().query("SELECT COUNT(*) FROM t").is_ok_and(|r| display(&r[0][0]) == "3") {
            assert!(Instant::now() < give_up, "a replica did not get the rows");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    drop(session);
    drop(cluster);
    for replica in replicas {
        rt.block_on(replica.shutdown());
    }
}
