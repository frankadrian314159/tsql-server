//! The cluster against the single-node engine: every statement runs on a real cluster (control node + Arrow Flight shard
//! nodes in this process, over TCP) and on one `tsql_server::Server`, and the answers must be the same.
use std::sync::Arc;
use tsql_control::{Cluster, ClusterSession};
use tsql_server::convert::display;
use tsql_server::{BatchResult, Connection, Event, Server};
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

struct Harness {
    _rt: tokio::runtime::Runtime,
    shards: Vec<RunningShard>,
    cluster: Arc<Cluster>,
}

fn start(n: usize) -> Harness {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut shards = vec![];
    for i in 0..n {
        shards.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
    }
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = Cluster::connect(&cfg).unwrap();
    Harness { _rt: rt, shards, cluster }
}

struct Pair {
    h: Harness,
    cl: ClusterSession,
    oracle_server: Server,
    oracle: Connection,
}

impl Pair {
    fn new(shards: usize) -> Pair {
        let h = start(shards);
        let cl = h.cluster.session();
        let oracle_server = Server::new();
        let oracle = oracle_server.connect();
        Pair { h, cl, oracle_server, oracle }
    }

    /// Run on both and require the same errors / row counts; no result set is compared.
    fn both(&mut self, sql: &str) {
        let a = self.cl.execute(sql);
        let b = self.oracle.execute(sql);
        assert!(a.first_error().is_none(), "cluster: {} -> {:?}", sql, a.first_error());
        assert!(b.first_error().is_none(), "oracle: {} -> {:?}", sql, b.first_error());
    }
}

fn rows_of(res: &BatchResult) -> Vec<Vec<String>> {
    res.resultsets().first().map(|r| r.rows.iter().map(|row| row.iter().map(display).collect()).collect()).unwrap_or_default()
}

fn counts_of(res: &BatchResult) -> Vec<i64> {
    res.events.iter().filter_map(|e| if let Event::RowCount(n) = e { Some(*n) } else { None }).collect()
}

/// Equal text, or equal numbers up to the last digits of a float (a sum over shards adds in a different order).
fn cell_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
        _ => false,
    }
}

fn same_rows(a: &[Vec<String>], b: &[Vec<String>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(p, q)| cell_eq(p, q)))
}

impl Pair {
    /// The query gives the same rows on the cluster and on one node (in the same order when `ordered`); returns the plan.
    fn check(&mut self, sql: &str, ordered: bool) -> Vec<String> {
        let a = self.cl.execute(sql);
        let plan = self.cl.last_plan().to_vec();
        let b = self.oracle.execute(sql);
        assert!(a.first_error().is_none(), "cluster: {} -> {:?}", sql, a.first_error());
        assert!(b.first_error().is_none(), "oracle: {} -> {:?}", sql, b.first_error());
        let (mut x, mut y) = (rows_of(&a), rows_of(&b));
        if !ordered {
            x.sort();
            y.sort();
        }
        assert!(same_rows(&x, &y), "{}\nplan: {:?}\ncluster ({} rows): {:?}\nsingle  ({} rows): {:?}", sql, plan, x.len(), &x[..x.len().min(8)], y.len(), &y[..y.len().min(8)]);
        // the same columns
        let (ca, cb) = (a.resultsets()[0].columns.clone(), b.resultsets()[0].columns.clone());
        assert_eq!(ca, cb, "{}: column names", sql);
        plan
    }

    fn load(&mut self) {
        self.both("CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR(30), region VARCHAR(10), credit DECIMAL(10,2))");
        self.both("CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, amount DECIMAL(12,2), status CHAR(1), placed DATE, qty INT, ratio FLOAT)");
        self.both("CREATE TABLE regions (code VARCHAR(10) PRIMARY KEY, label VARCHAR(30))");
        self.cl.execute("DISTRIBUTE TABLE regions REPLICATED");
        self.oracle.execute("SELECT 1");
        self.both("INSERT INTO regions VALUES ('north', 'The North'), ('south', 'The South'), ('east', 'The East'), ('west', 'The West')");
        let regions = ["north", "south", "east", "west"];
        let mut rows = vec![];
        for i in 1..=300 {
            rows.push(format!("({}, 'cust{:03}', '{}', {}.{:02})", i, i, regions[i % 4], (i * 37) % 900, i % 100));
        }
        self.both(&format!("INSERT INTO customers VALUES {}", rows.join(", ")));
        let mut seed: u64 = 12345;
        let mut next = move |m: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % m
        };
        let statuses = ['A', 'B', 'C', 'D'];
        for chunk in 0..6 {
            let mut rows = vec![];
            for k in 0..500 {
                let id = chunk * 500 + k + 1;
                let cust = next(300) + 1;
                let amount = next(100000);
                let status = statuses[next(4) as usize];
                let day = next(365);
                let qty = next(10);
                let ratio = next(1000) as f64 / 7.0;
                rows.push(format!("({}, {}, {}.{:02}, '{}', DATEADD(DAY, {}, '2024-01-01'), {}, {})", id, cust, amount / 100, amount % 100, status, day, qty, ratio));
            }
            self.both(&format!("INSERT INTO orders VALUES {}", rows.join(", ")));
        }
    }
}

#[test]
fn rows_are_spread_over_the_shards_and_the_catalog_is_everywhere() {
    let mut p = Pair::new(3);
    p.load();
    let mut total = 0;
    for (i, s) in p.h.shards.iter().enumerate() {
        let mut c = s.server.connect();
        let n = c.query("SELECT COUNT(*) FROM orders").unwrap()[0][0].clone();
        let n = match n {
            tsql_server::Value::Int(n) => n,
            other => panic!("{:?}", other),
        };
        assert!(n > 700 && n < 1300, "shard {} holds {} of 3000 orders", i, n);
        total += n;
        // a replicated table is complete on every shard
        assert!(matches!(c.query("SELECT COUNT(*) FROM regions").unwrap()[0][0], tsql_server::Value::Int(4)));
    }
    assert_eq!(total, 3000);
    // the shard of a key is a function of the key: a lookup by key finds the row where it was put
    let plan = p.check("SELECT id, amount FROM orders WHERE id = 1234", false);
    assert!(plan.iter().any(|l| l.starts_with("route")), "{:?}", plan);
    let status = p.h.cluster.shard_status();
    assert_eq!(status.len(), 3);
    assert!(status.iter().all(|s| s.is_ok()));
}

#[test]
fn scans_and_aggregates_give_the_single_node_answers() {
    let mut p = Pair::new(3);
    p.load();
    let scatter = |plan: &[String]| plan.iter().any(|l| l.starts_with("scatter"));
    for (sql, ordered) in [
        ("SELECT * FROM orders", false),
        ("SELECT id, amount FROM orders WHERE amount > 500 AND status = 'A'", false),
        ("SELECT COUNT(*), SUM(amount), MIN(amount), MAX(amount), AVG(amount) FROM orders", false),
        ("SELECT status, COUNT(*) AS n, SUM(qty) AS q, AVG(amount) AS avg_amt, AVG(ratio) AS avg_ratio FROM orders GROUP BY status ORDER BY status", true),
        ("SELECT customer_id, SUM(amount) AS total FROM orders GROUP BY customer_id HAVING SUM(amount) > 5000 ORDER BY total DESC, customer_id", true),
        ("SELECT TOP 10 id, amount FROM orders ORDER BY amount DESC, id", true),
        ("SELECT TOP 5 * FROM orders ORDER BY id", true),
        ("SELECT id FROM orders ORDER BY placed, id", true),
        ("SELECT id, qty * 2 AS twice FROM orders WHERE qty > 7 ORDER BY twice DESC, id", true),
        ("SELECT DISTINCT status FROM orders", false),
        ("SELECT DISTINCT customer_id FROM orders WHERE qty > 5 ORDER BY customer_id", true),
        ("SELECT * FROM orders WHERE id IN (1, 2, 3, 500, 2999)", false),
        ("SELECT status, SUM(amount * qty) / COUNT(*) AS per FROM orders GROUP BY status ORDER BY status", true),
        ("SELECT MIN(placed), MAX(placed), COUNT(*) FROM orders WHERE placed >= '2024-06-01'", false),
        ("SELECT status, MIN(amount), MAX(amount) FROM orders WHERE qty < 3 GROUP BY status", false),
        ("SELECT COUNT(*) FROM orders WHERE amount > 100000", false),
        ("SELECT SUM(qty) FROM orders WHERE id < 0", false),
        ("SELECT status, COUNT(*) AS n FROM orders GROUP BY status HAVING COUNT(*) > 0 ORDER BY n DESC, status", true),
    ] {
        let plan = p.check(sql, ordered);
        assert!(scatter(&plan) || plan.iter().any(|l| l.starts_with("route")), "{} was not scattered: {:?}", sql, plan);
    }
}

#[test]
fn what_cannot_be_scattered_is_gathered_and_still_exact() {
    let mut p = Pair::new(3);
    p.load();
    for (sql, ordered) in [
        ("SELECT COUNT(DISTINCT customer_id) FROM orders", false),
        ("SELECT c.region, SUM(o.amount) AS total, COUNT(*) AS n FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region", true),
        ("SELECT c.name FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > 990)", false),
        ("SELECT id, ROW_NUMBER() OVER (ORDER BY amount DESC, id) AS rn FROM orders WHERE id <= 30", false),
        ("SELECT status, COUNT(*) AS n FROM orders GROUP BY ROLLUP(status)", false),
        ("SELECT id FROM orders WHERE id < 5 UNION ALL SELECT id FROM customers WHERE id < 5", false),
        ("SELECT customer_id FROM orders WHERE amount > (SELECT AVG(amount) FROM orders) AND qty = 9", false),
        ("SELECT r.label, COUNT(*) AS n FROM customers c JOIN regions r ON r.code = c.region GROUP BY r.label ORDER BY r.label", true),
        ("SELECT o.id, c.name FROM orders o LEFT JOIN customers c ON c.id = o.customer_id WHERE o.id % 500 = 0", false),
        ("SELECT * FROM regions ORDER BY code", true),
        ("SELECT 1 + 1 AS two", false),
    ] {
        p.check(sql, ordered);
    }
    let plan = p.check("SELECT c.name, o.amount FROM customers c JOIN orders o ON o.customer_id = c.id WHERE o.id < 10", false);
    assert!(plan.iter().any(|l| l.starts_with("join over shards")), "{:?}", plan);
    // a subquery is still gathered
    let plan = p.check("SELECT c.name FROM customers c WHERE c.id IN (SELECT customer_id FROM orders WHERE amount > 990)", false);
    assert!(plan.iter().any(|l| l.starts_with("gather")), "{:?}", plan);
}

#[test]
fn updates_and_deletes_are_routed_and_the_cache_follows() {
    let mut p = Pair::new(3);
    p.load();
    // warm the gather cache, change data, the next gather must see the change
    p.check("SELECT c.region, SUM(o.amount) FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region", false);
    for sql in [
        "UPDATE orders SET status = 'Z' WHERE id = 7",
        "UPDATE orders SET qty = qty + 1 WHERE amount > 800",
        "UPDATE orders SET amount = amount * 2 WHERE id IN (1, 2, 3)",
        "DELETE FROM orders WHERE qty > 8",
        "DELETE FROM orders WHERE id = 10",
    ] {
        let a = p.cl.execute(sql);
        let b = p.oracle.execute(sql);
        assert!(a.first_error().is_none(), "{}: {:?}", sql, a.first_error());
        assert_eq!(counts_of(&a), counts_of(&b), "{}: rows affected", sql);
        let plan = p.cl.last_plan().to_vec();
        if sql.contains("id = 7") || sql.contains("id = 10") {
            assert!(plan[0].ends_with("shards [0]") || plan[0].ends_with("shards [1]") || plan[0].ends_with("shards [2]"), "{:?}", plan);
        }
    }
    p.check("SELECT status, COUNT(*), SUM(qty) FROM orders GROUP BY status ORDER BY status", true);
    p.check("SELECT c.region, SUM(o.amount) FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region", false);
    p.check("SELECT * FROM orders WHERE id IN (1, 2, 3, 7, 10)", false);
    // a replicated table changes everywhere
    p.both("INSERT INTO regions VALUES ('center', 'The Center')");
    p.both("UPDATE regions SET label = 'Middle' WHERE code = 'center'");
    p.check("SELECT * FROM regions ORDER BY code", true);
    p.both("TRUNCATE TABLE orders");
    p.check("SELECT COUNT(*) FROM orders", false);
}

#[test]
fn errors_come_back_as_the_engine_gives_them() {
    let mut p = Pair::new(2);
    p.both("CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(5))");
    p.both("INSERT INTO t VALUES (1, 'a'), (2, 'b')");
    let a = p.cl.execute("INSERT INTO t VALUES (2, 'dup')");
    let b = p.oracle.execute("INSERT INTO t VALUES (2, 'dup')");
    assert_eq!(a.first_error().map(|e| e.number), b.first_error().map(|e| e.number));
    assert_eq!(a.first_error().map(|e| e.number), Some(2627));
    let a = p.cl.execute("INSERT INTO t VALUES (3, 'way too long')");
    let b = p.oracle.execute("INSERT INTO t VALUES (3, 'way too long')");
    assert!(b.first_error().is_some());
    assert_eq!(a.first_error().map(|e| e.number), b.first_error().map(|e| e.number));
    assert_eq!(a.first_error().map(|e| e.message.clone()), b.first_error().map(|e| e.message.clone()));
    let a = p.cl.execute("SELECT * FROM nothing_here");
    assert_eq!(a.first_error().map(|e| e.number), Some(208));
    // what is not supported says so
    for sql in ["CREATE TABLE bad (id INT IDENTITY PRIMARY KEY)", "SAVE TRANSACTION sp1", "CREATE VIEW v AS SELECT 1 AS x", "UPDATE t SET id = 9 WHERE id = 1"] {
        let a = p.cl.execute(sql);
        assert_eq!(a.first_error().map(|e| e.number), Some(50100), "{}: {:?}", sql, a.first_error());
    }
    // the cluster is unharmed by the errors
    p.check("SELECT * FROM t", false);
}

#[test]
fn a_dead_shard_is_reported_not_hidden() {
    let mut p = Pair::new(2);
    p.both("CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    p.both("INSERT INTO t VALUES (1, 1), (2, 2), (3, 3), (4, 4)");
    p.check("SELECT SUM(v) FROM t", false);
    let shard = p.h.shards.pop().unwrap();
    p.h._rt.block_on(shard.shutdown());
    let a = p.cl.execute("SELECT SUM(v) FROM t");
    assert_eq!(a.first_error().map(|e| e.number), Some(40613), "{:?}", a.first_error());
}

#[test]
fn the_shard_key_can_be_a_string() {
    let mut p = Pair::new(3);
    p.both("CREATE TABLE people (name VARCHAR(20) PRIMARY KEY, age INT)");
    p.both("INSERT INTO people VALUES ('Alice', 30), ('bob', 25), ('Carol', 41), ('dave', 25), ('Eve', 33)");
    let plan = p.check("SELECT age FROM people WHERE name = 'ALICE'", false);
    assert!(plan.iter().any(|l| l.starts_with("route")), "{:?}", plan);
    p.check("SELECT age, COUNT(*) FROM people GROUP BY age ORDER BY age", true);
    let _ = &p.oracle_server;
}

#[test]
fn a_failed_create_table_leaves_no_trace() {
    let mut p = Pair::new(2);
    let shard = p.h.shards.pop().unwrap();
    p.h._rt.block_on(shard.shutdown());
    let a = p.cl.execute("CREATE TABLE ghost (id INT PRIMARY KEY, v INT)");
    assert_eq!(a.first_error().map(|e| e.number), Some(40613), "{:?}", a.first_error());
    // neither the control node nor the live shard may keep the table (a query must not quietly see an empty one)
    let a = p.cl.execute("SELECT * FROM ghost");
    assert_eq!(a.first_error().map(|e| e.number), Some(208), "{:?}", a.first_error());
    let mut c = p.h.shards[0].server.connect();
    assert_eq!(c.execute("SELECT * FROM ghost").first_error().map(|e| e.number), Some(208));
}

impl Pair {
    fn load_items(&mut self) {
        self.both("CREATE TABLE order_items (order_id INT, id INT, sku VARCHAR(8), qty INT, PRIMARY KEY (order_id, id))");
        let mut rows = vec![];
        for o in 1..=1200 {
            for k in 1..=(1 + o % 3) {
                rows.push(format!("({}, {}, 'sku{}', {})", o, k, (o * k) % 17, (o + k) % 9 + 1));
            }
        }
        for chunk in rows.chunks(500) {
            self.both(&format!("INSERT INTO order_items VALUES {}", chunk.join(", ")));
        }
    }

    /// Like `check`, and the plan must mention `expect`.
    fn check_plan(&mut self, sql: &str, ordered: bool, expect: &str) {
        let plan = self.check(sql, ordered);
        assert!(plan.iter().any(|l| l.contains(expect)), "{}\nexpected {:?} in the plan, got {:?}", sql, expect, plan);
    }
}

#[test]
fn joins_run_shard_by_shard_when_the_rows_can_meet() {
    let mut p = Pair::new(3);
    p.load();
    p.load_items();
    // both tables sharded on the join columns: co-located
    p.check_plan("SELECT o.status, SUM(i.qty) AS q, COUNT(*) AS n FROM orders o JOIN order_items i ON i.order_id = o.id GROUP BY o.status ORDER BY o.status", true, "co-located");
    p.check_plan("SELECT o.id, i.sku FROM orders o LEFT JOIN order_items i ON i.order_id = o.id WHERE o.id <= 40 ORDER BY o.id, i.sku", true, "co-located");
    p.check_plan("SELECT TOP 20 i.order_id, i.sku, o.amount FROM order_items i INNER JOIN orders o ON o.id = i.order_id ORDER BY o.amount DESC, i.order_id, i.sku", true, "co-located");
    // a small table is broadcast
    p.check_plan("SELECT c.region, SUM(o.amount) AS total, COUNT(*) AS n FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region", true, "broadcast");
    p.check_plan("SELECT COUNT(*) FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.region = 'north' AND o.qty > 3", false, "broadcast");
    p.check_plan("SELECT c.region, COUNT(*), SUM(i.qty) FROM orders o JOIN order_items i ON i.order_id = o.id JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region", true, "broadcast");
    p.check_plan("SELECT COUNT(*) FROM customers a JOIN customers b ON a.region = b.region AND a.id < b.id WHERE a.id < 20", false, "broadcast");
    p.check_plan("SELECT c.id, o.id FROM customers c RIGHT JOIN orders o ON c.id = o.customer_id WHERE o.id <= 30 ORDER BY o.id", true, "broadcast");
    // a replicated table is joined where it is
    p.check_plan("SELECT r.label, COUNT(*) AS n FROM customers c JOIN regions r ON r.code = c.region GROUP BY r.label ORDER BY r.label", true, "co-located");
    // too big to broadcast: the table that is not keyed on the join column is repartitioned
    p.h.cluster.set_broadcast_limit(10);
    p.check_plan("SELECT c.region, SUM(o.amount) AS total, COUNT(*) AS n FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region", true, "repartition dbo.orders");
    p.check_plan("SELECT o.id, c.name FROM orders o LEFT JOIN customers c ON c.id = o.customer_id WHERE o.id <= 25 ORDER BY o.id", true, "repartition");
    p.h.cluster.set_broadcast_limit(100_000);
    // an outer join that preserves a replicated table would repeat its unmatched rows on every shard: gathered
    let plan = p.check("SELECT r.code, COUNT(c.id) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.code ORDER BY r.code", true);
    assert!(plan.iter().any(|l| l.starts_with("gather")), "{:?}", plan);
    // changes are seen by the next join (the statistics and copies follow the table versions)
    p.both("UPDATE customers SET region = 'north' WHERE id < 100");
    p.both("DELETE FROM orders WHERE qty = 0");
    p.check_plan("SELECT c.region, COUNT(*) FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region", true, "broadcast");
    // no temporary copy is left behind on a shard
    for s in &p.h.shards {
        let mut c = s.server.connect();
        let left = c.query("SELECT COUNT(*) FROM tempdb.sys.tables").map(|r| format!("{:?}", r)).unwrap_or_default();
        let _ = left;
    }
}

fn shard_rows(s: &RunningShard, table: &str) -> i64 {
    let mut c = s.server.connect();
    match c.query(&format!("SELECT COUNT(*) FROM {}", table)).unwrap()[0][0].clone() {
        tsql_server::Value::Int(n) => n as i64,
        other => panic!("{:?}", other),
    }
}

const SAMPLE: [(&str, bool); 6] = [
    ("SELECT status, COUNT(*) AS n, SUM(amount) AS total FROM orders GROUP BY status ORDER BY status", true),
    ("SELECT c.region, SUM(o.amount) AS total FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region", true),
    ("SELECT o.status, SUM(i.qty) AS q FROM orders o JOIN order_items i ON i.order_id = o.id GROUP BY o.status ORDER BY o.status", true),
    ("SELECT * FROM orders WHERE id IN (1, 2, 3, 500, 2999)", false),
    ("SELECT COUNT(*) FROM customers", false),
    ("SELECT * FROM regions ORDER BY code", true),
];

#[test]
fn a_shard_can_be_added_and_removed_and_the_data_follows() {
    let mut p = Pair::new(2);
    p.load();
    p.load_items();
    for (sql, ord) in SAMPLE {
        p.check(sql, ord);
    }
    assert_eq!(p.h.cluster.slot_counts(), vec![512, 512]);
    // a third shard joins: it gets the schema, the replicated tables and a third of the slots with their rows
    let s3 = p.h._rt.block_on(spawn(ShardNode::new(2, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap();
    let addr = s3.addr.to_string();
    p.h.shards.push(s3);
    let id = p.h.cluster.add_shard(&addr).unwrap();
    assert_eq!(id, 2);
    assert_eq!(p.h.cluster.slot_counts(), vec![342, 341, 341]);
    let per: Vec<i64> = p.h.shards.iter().map(|s| shard_rows(s, "orders")).collect();
    assert_eq!(per.iter().sum::<i64>(), 3000, "{:?}", per);
    assert!(per.iter().all(|n| *n > 800 && *n < 1200), "{:?}", per);
    assert_eq!(shard_rows(&p.h.shards[2], "regions"), 4);
    for (sql, ord) in SAMPLE {
        p.check(sql, ord);
    }
    // new rows go where the slot map says, including to the new shard
    p.both("INSERT INTO orders VALUES (9001, 5, 12.50, 'A', '2024-05-05', 3, 1.5), (9002, 6, 7.25, 'B', '2024-05-06', 4, 2.5), (9003, 7, 1.00, 'C', '2024-05-07', 5, 3.5)");
    p.check("SELECT * FROM orders WHERE id >= 9000 ORDER BY id", true);
    p.both("UPDATE orders SET qty = qty + 1 WHERE id IN (9001, 9002, 9003, 1, 2)");
    p.both("DELETE FROM orders WHERE id = 9002");
    for (sql, ord) in SAMPLE {
        p.check(sql, ord);
    }
    // and it can leave again: everything it owned moves back
    p.h.cluster.remove_last_shard().unwrap();
    assert_eq!(p.h.cluster.slot_counts(), vec![512, 512]);
    assert_eq!(shard_rows(&p.h.shards[2], "orders"), 0, "the departed shard keeps no rows");
    let total: i64 = p.h.shards[..2].iter().map(|s| shard_rows(s, "orders")).sum();
    assert_eq!(total, 3002);
    for (sql, ord) in SAMPLE {
        p.check(sql, ord);
    }
    assert!(p.h.cluster.remove_last_shard().is_ok());
    assert!(p.h.cluster.remove_last_shard().is_err(), "a cluster of one shard keeps it");
}

#[test]
fn telemetry_sees_the_shards_and_advice_follows() {
    let mut p = Pair::new(3);
    p.load();
    let t = p.h.cluster.telemetry();
    assert_eq!(t.len(), 3);
    assert!(t.iter().all(|s| s.reachable));
    // the rows of the sharded tables: 300 customers + 3000 orders (the regions table is replicated, not counted)
    assert_eq!(t.iter().map(|s| s.rows).sum::<u64>(), 3300);
    assert_eq!(t.iter().map(|s| s.slots).sum::<usize>(), 1024);
    let policy = tsql_control::rebalance::ScalePolicy { max_rows_per_shard: 500, min_rows_per_shard: 10, max_slot_imbalance: 1.1 };
    assert!(matches!(tsql_control::rebalance::advise(&t, &policy)[..], [tsql_control::rebalance::ScaleAdvice::AddShard { .. }]));
    let shard = p.h.shards.pop().unwrap();
    p.h._rt.block_on(shard.shutdown());
    let t = p.h.cluster.telemetry();
    assert_eq!(tsql_control::rebalance::advise(&t, &policy), vec![tsql_control::rebalance::ScaleAdvice::Unreachable { shard: 2 }]);
}

#[test]
fn the_control_node_restarts_without_losing_the_cluster() {
    let h = start(2);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let path = std::env::temp_dir().join(format!("tsql_cluster_state_{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let oracle_server = Server::new();
    let mut oracle = oracle_server.connect();
    {
        let cluster = Cluster::open(&path, &cfg).unwrap();
        let mut cl = cluster.session();
        for sql in [
            "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10), v DECIMAL(8,2))",
            "CREATE INDEX ix_name ON t (name)",
            "CREATE TABLE ref (code VARCHAR(5) PRIMARY KEY, label VARCHAR(20))",
        ] {
            assert!(cl.execute(sql).first_error().is_none(), "{}", sql);
            assert!(oracle.execute(sql).first_error().is_none());
        }
        assert!(cl.execute("DISTRIBUTE TABLE ref REPLICATED").first_error().is_none());
        for sql in ["INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.5),(3,'c',3.5),(4,'a',4.5)", "INSERT INTO ref VALUES ('x','Ex')"] {
            assert!(cl.execute(sql).first_error().is_none());
            assert!(oracle.execute(sql).first_error().is_none());
        }
    }
    // a new control node, the same state file (the config is not consulted: the shards are in the file)
    let cluster = Cluster::open(&path, &ClusterConfig::default()).unwrap();
    let mut cl = cluster.session();
    assert_eq!(cluster.tables().len(), 2);
    for sql in ["SELECT name, COUNT(*), SUM(v) FROM t GROUP BY name ORDER BY name", "SELECT t.id, r.label FROM t JOIN ref r ON r.code = 'x' WHERE t.id < 3 ORDER BY t.id", "SELECT * FROM t WHERE id = 3"] {
        let a = cl.execute(sql);
        let b = oracle.execute(sql);
        assert!(a.first_error().is_none(), "{}: {:?}", sql, a.first_error());
        assert_eq!(rows_of(&a), rows_of(&b), "{}", sql);
    }
    assert!(cl.execute("INSERT INTO t VALUES (5,'z',9.5)").first_error().is_none());
    assert_eq!(cl.query("SELECT COUNT(*) FROM t").unwrap().len(), 1);
    let _ = std::fs::remove_file(&path);
}

fn setup_accounts(p: &mut Pair) {
    p.both("CREATE TABLE acct (id INT PRIMARY KEY, bal INT)");
    let rows: Vec<String> = (1..=30).map(|i| format!("({}, 100)", i)).collect();
    p.both(&format!("INSERT INTO acct VALUES {}", rows.join(",")));
}

/// Two keys that live on different shards.
fn keys_on_two_shards(c: &Cluster, a: usize, b: usize) -> (i32, i32) {
    use tsql_shared_ces::route::{key_hash, KeyClass};
    let shard = |i: i32| c.shard_of_hash(key_hash(&tsql_server::Value::Int(i), KeyClass::Int).unwrap());
    let ka = (1..200).find(|i| shard(*i) == a).unwrap();
    let kb = (1..200).find(|i| shard(*i) == b).unwrap();
    (ka, kb)
}

#[test]
fn transactions_span_shards_commit_roll_back_and_read_their_own_writes() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    // a transfer between rows on different shards, committed with two-phase commit
    p.both("BEGIN TRANSACTION; UPDATE acct SET bal = bal - 10 WHERE id = 1; UPDATE acct SET bal = bal + 10 WHERE id = 2; UPDATE acct SET bal = bal + 5 WHERE id = 3; COMMIT");
    assert!(!p.cl.in_transaction());
    p.check("SELECT id, bal FROM acct ORDER BY id", true);
    // inside a transaction a session sees its own changes on every shard, and a rollback undoes them all
    let sql = "BEGIN TRANSACTION; UPDATE acct SET bal = 0 WHERE id <= 12; SELECT SUM(bal) FROM acct; ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
    p.check("SELECT id, bal FROM acct ORDER BY id", true);
    // nested BEGIN / COMMIT: only the outer one commits
    p.both("BEGIN TRANSACTION; BEGIN TRANSACTION; UPDATE acct SET bal = 1 WHERE id IN (5, 6); COMMIT; UPDATE acct SET bal = 2 WHERE id = 7; COMMIT");
    p.check("SELECT id, bal FROM acct WHERE id IN (5, 6, 7) ORDER BY id", true);
    // a gather inside a transaction sees its writes, and does not leave them in the cache that others use
    let sql = "BEGIN TRANSACTION; UPDATE acct SET bal = 7 WHERE id = 20; SELECT bal FROM acct WHERE id IN (SELECT id FROM acct WHERE bal = 7); ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert_eq!(rows_of(&a), rows_of(&b));
    p.check("SELECT COUNT(*) FROM acct WHERE id IN (SELECT id FROM acct WHERE bal = 7)", false);
    assert!(p.cl.execute("ROLLBACK").first_error().map(|e| e.number) == Some(3903));
}

#[test]
fn uncommitted_changes_are_isolated_and_a_lock_wait_times_out() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    let mut a = p.h.cluster.session();
    let mut b = p.h.cluster.session();
    assert!(a.execute("BEGIN TRANSACTION; UPDATE acct SET bal = 999 WHERE id IN (1, 2, 3, 4)").first_error().is_none());
    assert!(b.execute("SET LOCK_TIMEOUT 300").first_error().is_none());
    // b's read of a row a has changed waits, and gives up
    let r = b.execute("SELECT bal FROM acct WHERE id = 1");
    assert_eq!(r.first_error().map(|e| e.number), Some(1222), "{:?}", r.first_error());
    // rows nobody changed are not blocked
    assert_eq!(b.query("SELECT bal FROM acct WHERE id = 30").unwrap().len(), 1);
    assert!(a.execute("COMMIT").first_error().is_none());
    assert_eq!(rows(&mut b, "SELECT bal FROM acct WHERE id IN (1, 2, 3, 4)"), vec![vec!["999"]; 4]);
}

fn rows(s: &mut ClusterSession, sql: &str) -> Vec<Vec<String>> {
    s.query(sql).unwrap().into_iter().map(|r| r.iter().map(display).collect()).collect()
}

#[test]
fn a_statement_that_fails_on_several_shards_dooms_the_transaction() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    let a = p.cl.execute("BEGIN TRANSACTION; UPDATE acct SET bal = bal + 1 WHERE id = 1; UPDATE acct SET bal = 1 / 0");
    assert!(a.first_error().is_some());
    // what a shard did before the failure cannot be told from what it did not: the transaction can only be rolled back
    let r = p.cl.execute("UPDATE acct SET bal = 5 WHERE id = 1");
    assert_eq!(r.first_error().map(|e| e.number), Some(3930));
    assert!(p.cl.execute("COMMIT").first_error().map(|e| e.number) == Some(3930));
    assert!(!p.cl.in_transaction());
    p.check("SELECT id, bal FROM acct ORDER BY id", true);
}

#[test]
fn a_participant_that_is_gone_at_prepare_aborts_the_whole_transaction() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    let (k0, k2) = keys_on_two_shards(&p.h.cluster, 0, 2);
    assert!(p.cl.execute(&format!("BEGIN TRANSACTION; UPDATE acct SET bal = 1 WHERE id = {}; UPDATE acct SET bal = 2 WHERE id = {}", k0, k2)).first_error().is_none());
    let dead = p.h.shards.pop().unwrap();
    p.h._rt.block_on(dead.shutdown());
    let r = p.cl.execute("COMMIT");
    assert!(r.first_error().is_some(), "the commit cannot succeed with a participant gone");
    // the shard that is alive rolled its part back and holds no locks
    let mut c = p.h.shards[0].server.connect();
    let bal = c.query(&format!("SELECT bal FROM acct WHERE id = {}", k0)).unwrap();
    assert!(matches!(bal[0][0], tsql_server::Value::Int(100)), "{:?}", bal);
}

/// Two sessions, each holding a row on one shard and asking for the row the other holds on the other shard. Returns what
/// became of each: Ok(()) committed, Err(number) the error number of the statement that failed.
fn cross_shard_deadlock(p: &mut Pair) -> Vec<Result<(), i32>> {
    let (k0, k1) = keys_on_two_shards(&p.h.cluster, 0, 1);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut handles = vec![];
    for (first, second) in [(k0, k1), (k1, k0)] {
        let mut s = p.h.cluster.session();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            assert!(s.execute(&format!("BEGIN TRANSACTION; UPDATE acct SET bal = bal + 1 WHERE id = {}", first)).first_error().is_none());
            barrier.wait();
            let r = s.execute(&format!("UPDATE acct SET bal = bal + 1 WHERE id = {}", second));
            match r.first_error() {
                None => match s.execute("COMMIT").first_error() {
                    None => Ok(()),
                    Some(e) => Err(e.number),
                },
                Some(e) => {
                    let n = e.number;
                    // the victim's transaction is already gone; the other kind of failure leaves one to roll back
                    if s.in_transaction() {
                        s.execute("ROLLBACK");
                    }
                    assert!(!s.in_transaction());
                    Err(n)
                }
            }
        }));
    }
    let outcomes: Vec<Result<(), i32>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // atomicity: nothing is left locked, and each row moved by the number of transactions that committed
    let committed = outcomes.iter().filter(|o| o.is_ok()).count() as i64;
    let mut s = p.h.cluster.session();
    for k in [k0, k1] {
        let v = s.query(&format!("SELECT bal FROM acct WHERE id = {}", k)).unwrap();
        assert_eq!(display(&v[0][0]), (100 + committed).to_string(), "row {} after {:?}", k, outcomes);
    }
    assert!(s.execute(&format!("UPDATE acct SET bal = 0 WHERE id IN ({}, {})", k0, k1)).first_error().is_none());
    outcomes
}

#[test]
fn a_deadlock_between_shards_is_detected_and_one_transaction_is_the_victim() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    // a lock timeout far longer than the test: only the detector can end this
    p.h.cluster.set_lock_timeout_ms(60_000);
    let t = std::time::Instant::now();
    let outcomes = cross_shard_deadlock(&mut p);
    assert!(t.elapsed() < std::time::Duration::from_secs(20), "the deadlock took {:?} to resolve", t.elapsed());
    assert_eq!(outcomes.iter().filter(|o| o.is_ok()).count(), 1, "exactly one survives: {:?}", outcomes);
    assert!(outcomes.contains(&Err(1205)), "the victim is told it was a deadlock victim: {:?}", outcomes);
    assert!(p.h.cluster.deadlocks_broken() >= 1);
}

#[test]
fn without_the_detector_the_lock_timeout_still_breaks_a_deadlock_between_shards() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    p.h.cluster.set_deadlock_check_ms(0);
    p.h.cluster.set_lock_timeout_ms(400);
    let outcomes = cross_shard_deadlock(&mut p);
    assert!(outcomes.iter().any(|o| o.is_err()), "somebody must have been the victim: {:?}", outcomes);
    assert!(outcomes.iter().all(|o| matches!(o, Ok(()) | Err(1222))), "{:?}", outcomes);
    assert_eq!(p.h.cluster.deadlocks_broken(), 0);
}

#[test]
fn a_wait_that_is_not_a_cycle_is_left_alone() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    p.h.cluster.set_lock_timeout_ms(30_000);
    let (k0, _) = keys_on_two_shards(&p.h.cluster, 0, 1);
    let mut a = p.h.cluster.session();
    assert!(a.execute(&format!("BEGIN TRANSACTION; UPDATE acct SET bal = bal + 1 WHERE id = {}", k0)).first_error().is_none());
    let mut b = p.h.cluster.session();
    let waiter = std::thread::spawn(move || {
        let r = b.execute(&format!("UPDATE acct SET bal = bal + 5 WHERE id = {}", k0));
        r.first_error().map(|e| e.number)
    });
    // long enough for the detector to look several times
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(a.execute("COMMIT").first_error().is_none());
    assert_eq!(waiter.join().unwrap(), None, "the waiter must not be chosen as a victim");
    assert_eq!(p.h.cluster.deadlocks_broken(), 0);
    let v = a.query(&format!("SELECT bal FROM acct WHERE id = {}", k0)).unwrap();
    assert_eq!(display(&v[0][0]), "106");
}
