//! Where the time of a routed single-row statement goes: the shard's own engine, a bare round trip to the shard (the status
//! action), a statement run on the shard through the client, and a statement through a cluster session.
use std::time::Instant;
use tsql_control::{client::ShardClient, Cluster};
use tsql_server::Server;
use tsql_shared_ces::{ClusterConfig, ShardInfo, Work};
use tsql_storage::{spawn, ShardNode};

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shard = rt.block_on(spawn(ShardNode::new(0, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap();
    let addr = shard.addr.to_string();
    let cfg = ClusterConfig { shards: vec![ShardInfo { id: 0, addr: addr.clone() }] };
    let cluster = Cluster::connect(&cfg).unwrap();
    let mut cs = cluster.session();
    assert!(cs.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").first_error().is_none());
    assert!(cs.execute("INSERT t VALUES (1, 1), (2, 2)").first_error().is_none());
    let n = 3000;
    let _g = rt.enter();
    let client = ShardClient::new(0, &addr, None).unwrap();
    let t = Instant::now();
    for _ in 0..n {
        rt.block_on(client.status()).unwrap();
    }
    println!("bare round trip (status action):      {:.3} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
    let st = tsql_server::parser::parse("SELECT v FROM t WHERE id = 1").unwrap();
    let t = Instant::now();
    for _ in 0..n {
        rt.block_on(client.run(7, Work::Stmts(st.clone()))).unwrap();
    }
    println!("statement through the shard client:   {:.3} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        rt.block_on(client.query(7, Work::Stmts(st.clone()))).unwrap();
    }
    println!("point select, do_get stream (query):  {:.3} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        rt.block_on(client.run_sets(7, Work::Stmts(st.clone()))).unwrap();
    }
    println!("point select, one unary reply (sets): {:.3} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        assert!(cs.execute("SELECT v FROM t WHERE id = 1").first_error().is_none());
    }
    println!("statement through a cluster session:  {:.3} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        assert!(cs.execute("UPDATE t SET v = v + 1 WHERE id = 1").first_error().is_none());
    }
    println!("update through a cluster session:     {:.3} ms", t.elapsed().as_secs_f64() * 1e3 / n as f64);
}
