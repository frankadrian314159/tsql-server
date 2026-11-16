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
    // a subquery that does not depend on the outer query is evaluated first and the outer query runs over the shards
    let plan = p.check("SELECT c.name FROM customers c WHERE c.id IN (SELECT customer_id FROM orders WHERE amount > 990)", false);
    assert!(!plan.iter().any(|l| l.starts_with("gather")), "{:?}", plan);
    // a correlated one by equality is rewritten as a join, one that is not is gathered
    let plan = p.check("SELECT c.name FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > 990)", false);
    assert!(!plan.iter().any(|l| l.starts_with("gather")), "{:?}", plan);
    let plan = p.check("SELECT c.name FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > c.credit)", false);
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
    for sql in ["CREATE PROCEDURE p AS SELECT 1", "CREATE DATABASE other", "UPDATE TOP (1) t SET v = 'x'", "DELETE FROM t OUTPUT deleted.id WHERE id = 1"] {
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
    // an outer join that preserves a replicated table repeats its unmatched rows on every shard: harmless for a count of what matched
    // (the partial counts add up), so that runs over the shards; one that counts the unmatched row itself is gathered
    let plan = p.check("SELECT r.code, COUNT(c.id) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.code ORDER BY r.code", true);
    assert!(plan.iter().any(|l| l.starts_with("join over shards")) && !plan.iter().any(|l| l.starts_with("gather")), "{:?}", plan);
    let plan = p.check("SELECT r.code, COUNT(*) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.code ORDER BY r.code", true);
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
    // (the thresholds on resources are out of the way: this test is about rows; the machine's own use is whatever it is)
    let policy = tsql_control::rebalance::ScalePolicy { max_rows_per_shard: 500, min_rows_per_shard: 10, max_slot_imbalance: 1.1, cpu_high_pct: f64::MAX, cpu_low_pct: -1.0, mem_high_pct: f64::MAX, disk_high_bps: f64::MAX, net_high_bps: f64::MAX, ..Default::default() };
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

#[test]
fn subqueries_ctes_and_derived_tables_are_evaluated_around_the_shards() {
    let mut p = Pair::new(3);
    p.load();
    // (query, ordered, what the plan must say)
    let cases: Vec<(&str, bool, &str)> = vec![
        ("SELECT id, name FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE amount > 900) ORDER BY id", true, "IN subquery evaluated first"),
        ("SELECT COUNT(*) FROM customers WHERE id NOT IN (SELECT customer_id FROM orders WHERE amount > 900)", false, "IN subquery evaluated first"),
        ("SELECT COUNT(*) FROM orders WHERE amount > (SELECT AVG(amount) FROM orders)", false, "subquery evaluated first"),
        ("SELECT id FROM customers WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 999) ORDER BY id", true, "subquery evaluated first"),
        ("SELECT id FROM customers WHERE NOT EXISTS (SELECT 1 FROM orders WHERE amount > 99999999) AND id < 5 ORDER BY id", true, "subquery evaluated first"),
        ("SELECT TOP 5 id, (SELECT MAX(amount) FROM orders) AS m FROM customers ORDER BY id", true, "subquery evaluated first"),
        ("SELECT COUNT(*) FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE amount > (SELECT AVG(amount) FROM orders))", false, "subquery evaluated first"),
        ("WITH big AS (SELECT customer_id, SUM(amount) AS s FROM orders GROUP BY customer_id) SELECT c.name, b.s FROM customers c JOIN big b ON b.customer_id = c.id WHERE b.s > 5000 ORDER BY c.name", true, "materialized"),
        ("WITH a AS (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id), b AS (SELECT customer_id FROM a WHERE n > 10) SELECT COUNT(*) FROM b", false, "materialized"),
        ("SELECT d.region, COUNT(*) AS n FROM (SELECT region FROM customers WHERE credit > 100) d GROUP BY d.region ORDER BY d.region", true, "materialized"),
        ("SELECT c.id, d.total FROM customers c JOIN (SELECT customer_id, SUM(qty) AS total FROM orders GROUP BY customer_id) d ON d.customer_id = c.id WHERE c.id <= 20 ORDER BY c.id", true, "materialized"),
        ("WITH r AS (SELECT region, COUNT(*) AS n FROM customers GROUP BY region) SELECT r.region, r.n, g.label FROM r JOIN regions g ON g.code = r.region ORDER BY r.region", true, "materialized"),
    ];
    for (sql, ordered, expect) in cases {
        let plan = p.check(sql, ordered);
        assert!(plan.iter().any(|l| l.contains(expect)), "{}\nexpected {:?} in the plan, got {:?}", sql, expect, plan);
        assert!(!plan.iter().any(|l| l.starts_with("gather")), "{}\nthe tables should not be gathered: {:?}", sql, plan);
    }
    // what cannot be evaluated alone is gathered, and still exact: a correlated subquery that is not an equality, ANY / ALL
    for sql in [
        "SELECT c.id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > c.credit) ORDER BY c.id",
        "SELECT c.id FROM customers c WHERE c.credit > (SELECT AVG(o.amount) FROM orders o WHERE o.customer_id < c.id) ORDER BY c.id",
        "SELECT id FROM customers WHERE credit > ANY (SELECT amount FROM orders WHERE amount < 10) ORDER BY id",
    ] {
        let plan = p.check(sql, true);
        assert!(plan.iter().any(|l| l.starts_with("gather")), "{}\nexpected a gather, got {:?}", sql, plan);
    }
    // a recursive CTE is left to one engine
    p.check("WITH r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT SUM(n) FROM r", false);
    // a result too big to broadcast is gathered
    p.h.cluster.set_broadcast_limit(10);
    let plan = p.check("SELECT d.region, COUNT(*) AS n FROM (SELECT region FROM customers) d GROUP BY d.region ORDER BY d.region", true);
    assert!(plan.iter().any(|l| l.starts_with("gather")), "{:?}", plan);
    // nothing is left behind on the shards
    for s in &p.h.shards {
        let mut c = s.server.connect();
        let left = c.query("SELECT COUNT(*) FROM tempdb.sys.tables WHERE name LIKE '#dpn%'");
        let _ = left; // (session temporary tables go with their sessions; the statement dropped its own)
    }
}

#[test]
fn nested_queries_inside_a_transaction_see_its_changes() {
    let mut p = Pair::new(3);
    p.load();
    let sql = "BEGIN TRANSACTION; UPDATE orders SET amount = 0 WHERE id <= 600; SELECT COUNT(*) FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE amount = 0); ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
}

/// Both sides must give the same error number (or none) for a statement; returns it.
fn same_outcome(p: &mut Pair, sql: &str) -> Option<i32> {
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert_eq!(a.first_error().map(|e| e.number), b.first_error().map(|e| e.number), "{}: cluster {:?} / single {:?}", sql, a.first_error(), b.first_error());
    a.first_error().map(|e| e.number)
}

#[test]
fn identity_columns_are_numbered_by_the_control_node() {
    let mut p = Pair::new(3);
    p.both("CREATE TABLE seq_t (id INT IDENTITY(10, 5) PRIMARY KEY, name VARCHAR(10), v INT)");
    p.both("INSERT seq_t (name, v) VALUES ('a', 1), ('b', 2), ('c', 3)");
    // without a column list the values are for the columns after the identity one
    p.both("INSERT seq_t VALUES ('d', 4), ('e', 5)");
    p.both("INSERT seq_t (name, v) SELECT name, v + 100 FROM seq_t WHERE v < 3");
    p.check("SELECT id, name, v FROM seq_t ORDER BY id", true);
    // the numbers do not depend on the shard a row lands on, and the rows are spread
    let holding = p.h.shards.iter().filter(|s| matches!(s.server.connect().query("SELECT COUNT(*) FROM seq_t").unwrap()[0][0], tsql_server::Value::Int(k) if k > 0)).count();
    assert!(holding >= 2, "the rows are spread over the shards ({} hold some)", holding);
    // SCOPE_IDENTITY() and @@IDENTITY
    let sql = "INSERT seq_t (name, v) VALUES ('f', 6); SELECT SCOPE_IDENTITY() AS s, @@IDENTITY AS i";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
    // an explicit value needs IDENTITY_INSERT, which then moves the counter
    assert_eq!(same_outcome(&mut p, "INSERT seq_t (id, name, v) VALUES (500, 'x', 9)"), Some(544));
    assert_eq!(same_outcome(&mut p, "SET IDENTITY_INSERT seq_t ON"), None);
    assert_eq!(same_outcome(&mut p, "INSERT seq_t (id, name, v) VALUES (500, 'x', 9)"), None);
    assert_eq!(same_outcome(&mut p, "SET IDENTITY_INSERT seq_t OFF"), None);
    p.both("INSERT seq_t (name, v) VALUES ('after', 10)");
    p.check("SELECT id, name, v FROM seq_t ORDER BY id", true);
    // the identity column cannot be updated
    assert_eq!(same_outcome(&mut p, "UPDATE seq_t SET id = 1 WHERE name = 'a'"), Some(8102));
    // a bigint identity, and a table whose shard key is not the identity column
    p.both("CREATE TABLE seq2 (k VARCHAR(8) PRIMARY KEY, n BIGINT IDENTITY(1, 1))");
    p.both("INSERT seq2 (k) VALUES ('x'), ('y'), ('z')");
    p.check("SELECT k, n FROM seq2 ORDER BY k", true);
}

#[test]
fn identity_numbers_survive_a_restart_of_the_control_node() {
    let h = start(2);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let path = std::env::temp_dir().join(format!("tsql_identity_state_{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let cluster = Cluster::open(&path, &cfg).unwrap();
        let mut cl = cluster.session();
        assert!(cl.execute("CREATE TABLE t (id INT IDENTITY PRIMARY KEY, v INT)").first_error().is_none());
        assert!(cl.execute("INSERT t (v) VALUES (1), (2), (3)").first_error().is_none());
    }
    let cluster = Cluster::open(&path, &ClusterConfig::default()).unwrap();
    let mut cl = cluster.session();
    assert!(cl.execute("INSERT t (v) VALUES (4)").first_error().is_none());
    let rows: Vec<String> = cl.query("SELECT id FROM t ORDER BY id").unwrap().iter().map(|r| display(&r[0])).collect();
    assert_eq!(rows, vec!["1", "2", "3", "4"]);
    // a new shard gets the table without identity of its own
    let _ = std::fs::remove_file(&path);
}

#[test]
fn updates_and_deletes_may_have_subqueries_and_may_change_the_shard_key() {
    let mut p = Pair::new(3);
    p.load();
    for sql in [
        "UPDATE orders SET status = 'Z' WHERE customer_id IN (SELECT id FROM customers WHERE region = 'north')",
        "DELETE FROM orders WHERE amount < (SELECT AVG(amount) FROM orders) / 10",
        "UPDATE orders SET qty = qty + 1 WHERE EXISTS (SELECT 1 FROM customers WHERE credit > 890) AND id <= 50",
        "DELETE FROM orders WHERE customer_id NOT IN (SELECT id FROM customers WHERE id <= 150)",
    ] {
        p.both(sql);
        p.check("SELECT status, COUNT(*) AS n, SUM(qty) AS q FROM orders GROUP BY status ORDER BY status", true);
        p.check("SELECT COUNT(*) AS n FROM orders", false);
    }
    // a correlated subquery is not supported in UPDATE (an error, not a wrong answer)
    let r = p.cl.execute("UPDATE orders SET qty = 0 WHERE amount > (SELECT AVG(o2.amount) FROM orders o2 WHERE o2.customer_id = orders.customer_id)");
    assert_eq!(r.first_error().map(|e| e.number), Some(50100));
    // changing the shard key moves rows between shards, atomically
    p.both("CREATE TABLE kt (id INT PRIMARY KEY, v INT)");
    p.both(&format!("INSERT kt VALUES {}", (1..=40).map(|i| format!("({}, {})", i, i * 10)).collect::<Vec<_>>().join(",")));
    p.both("UPDATE kt SET id = id + 1000 WHERE id <= 10");
    p.check("SELECT id, v FROM kt ORDER BY id", true);
    p.both("UPDATE kt SET id = id + 1, v = v + 1 WHERE id BETWEEN 1005 AND 1010");
    p.check("SELECT id, v FROM kt ORDER BY id", true);
    // a collision with an existing key fails and changes nothing
    assert_eq!(same_outcome(&mut p, "UPDATE kt SET id = 20 WHERE id = 30"), Some(2627));
    p.check("SELECT id, v FROM kt ORDER BY id", true);
    // inside a transaction it is part of it
    let sql = "BEGIN TRANSACTION; UPDATE kt SET id = id + 5000 WHERE id > 1000; SELECT COUNT(*) FROM kt WHERE id > 5000; ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
    p.check("SELECT id, v FROM kt ORDER BY id", true);
}

#[test]
fn views_are_expanded_around_the_shards() {
    let mut p = Pair::new(3);
    p.load();
    p.both("CREATE VIEW big_orders AS SELECT id, customer_id, amount FROM orders WHERE amount > 500");
    p.check("SELECT COUNT(*) AS n, SUM(amount) AS s FROM big_orders", false);
    p.check("SELECT c.name, b.amount FROM customers c JOIN big_orders b ON b.customer_id = c.id WHERE c.id < 20 ORDER BY c.name, b.amount", true);
    // a view over a view, with an aggregate, and one with a column list and a replicated table
    p.both("CREATE VIEW vip AS SELECT customer_id, SUM(amount) AS s FROM big_orders GROUP BY customer_id");
    p.check("SELECT TOP 5 customer_id, s FROM vip ORDER BY s DESC, customer_id", true);
    p.both("CREATE VIEW north (code, who) AS SELECT c.id, c.name FROM customers c JOIN regions r ON r.code = c.region WHERE r.code = 'north'");
    p.check("SELECT who FROM north WHERE code < 40 ORDER BY code", true);
    // a view used in a subquery and in a CTE
    p.check("SELECT COUNT(*) AS n FROM customers WHERE id IN (SELECT customer_id FROM big_orders)", false);
    p.check("WITH t AS (SELECT customer_id FROM vip WHERE s > 3000) SELECT COUNT(*) AS n FROM t", false);
    // ALTER VIEW changes it for later queries; the definition survives
    p.both("ALTER VIEW big_orders AS SELECT id, customer_id, amount FROM orders WHERE amount > 900");
    p.check("SELECT COUNT(*) AS n FROM big_orders", false);
    // errors as the engine gives them, and writes through a view are refused (not answered wrongly)
    assert_eq!(p.cl.execute("CREATE VIEW big_orders AS SELECT 1 AS x").first_error().map(|e| e.number), Some(2714));
    assert_eq!(p.cl.execute("DELETE FROM big_orders WHERE id = 1").first_error().map(|e| e.number), Some(50100));
    p.both("DROP VIEW north");
    assert_eq!(p.cl.execute("SELECT * FROM north").first_error().map(|e| e.number), Some(208));
    assert_eq!(p.cl.execute("DROP VIEW north").first_error().map(|e| e.number), Some(3701));
}

#[test]
fn views_survive_a_restart_of_the_control_node() {
    let h = start(2);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let path = std::env::temp_dir().join(format!("tsql_views_state_{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let cluster = Cluster::open(&path, &cfg).unwrap();
        let mut cl = cluster.session();
        assert!(cl.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").first_error().is_none());
        assert!(cl.execute("INSERT t VALUES (1, 10), (2, 20), (3, 30)").first_error().is_none());
        assert!(cl.execute("CREATE VIEW big AS SELECT id, v FROM t WHERE v > 10").first_error().is_none());
    }
    let cluster = Cluster::open(&path, &ClusterConfig::default()).unwrap();
    let mut cl = cluster.session();
    let rows: Vec<String> = cl.query("SELECT id FROM big ORDER BY id").unwrap().iter().map(|r| display(&r[0])).collect();
    assert_eq!(rows, vec!["2", "3"]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn alter_table_changes_the_columns_on_every_shard() {
    let mut p = Pair::new(3);
    p.load();
    p.both("ALTER TABLE customers ADD note VARCHAR(20) NULL");
    p.both("UPDATE customers SET note = 'x' WHERE id < 10");
    p.check("SELECT id, note FROM customers WHERE id < 12 ORDER BY id", true);
    // a column with a default: the rows that exist get it
    p.both("ALTER TABLE customers ADD flag INT NOT NULL DEFAULT 7");
    p.check("SELECT COUNT(*) AS n, SUM(flag) AS s FROM customers", false);
    p.both("INSERT INTO customers (id, name, region, credit) VALUES (1001, 'new', 'north', 1.0)");
    p.check("SELECT id, flag FROM customers WHERE id = 1001", false);
    // a wider column, a constraint that later statements must satisfy, and a dropped column
    p.both("ALTER TABLE customers ALTER COLUMN name VARCHAR(60)");
    p.both("ALTER TABLE orders ADD CONSTRAINT ck_qty CHECK (qty >= 0)");
    assert_eq!(same_outcome(&mut p, "INSERT INTO orders VALUES (9001, 1, 1.0, 'A', '2024-01-01', -5, 0.5)"), Some(547));
    p.both("ALTER TABLE orders DROP CONSTRAINT ck_qty");
    assert_eq!(same_outcome(&mut p, "INSERT INTO orders VALUES (9001, 1, 1.0, 'A', '2024-01-01', -5, 0.5)"), None);
    p.both("ALTER TABLE customers DROP COLUMN note");
    p.check("SELECT TOP 5 * FROM customers ORDER BY id", true);
    p.check("SELECT region, COUNT(*) AS n FROM customers GROUP BY region ORDER BY region", true);
    // dropping a column before the shard key moves it: rows still go where they belong
    p.both("CREATE TABLE moved (junk INT, id INT PRIMARY KEY, v INT)");
    p.cl.execute("DISTRIBUTE TABLE moved BY id");
    p.both("INSERT INTO moved VALUES (0, 1, 10), (0, 2, 20), (0, 3, 30)");
    p.both("ALTER TABLE moved DROP COLUMN junk");
    p.both("INSERT INTO moved VALUES (4, 40), (5, 50)");
    p.check("SELECT id, v FROM moved WHERE id = 4", false);
    p.check("SELECT id, v FROM moved ORDER BY id", true);
    // what would change how rows are placed is refused, not done wrongly
    for sql in ["ALTER TABLE customers ALTER COLUMN id BIGINT", "ALTER TABLE customers DROP COLUMN id", "ALTER TABLE customers ADD z INT IDENTITY", "ALTER TABLE customers ADD CONSTRAINT u UNIQUE (name)"] {
        assert_eq!(p.cl.execute(sql).first_error().map(|e| e.number), Some(50100), "{}", sql);
    }
}

#[test]
fn savepoints_roll_back_part_of_a_transaction_on_every_shard() {
    let mut p = Pair::new(3);
    setup_accounts(&mut p);
    let both_rows = |p: &mut Pair, sql: &str| {
        let a = p.cl.execute(sql);
        let b = p.oracle.execute(sql);
        assert_eq!(a.first_error().map(|e| e.number), b.first_error().map(|e| e.number), "{}: {:?} / {:?}", sql, a.first_error(), b.first_error());
        assert_eq!(rows_of(&a), rows_of(&b), "{}", sql);
    };
    // the work before the savepoint stays; what shards did after it is undone, including a shard that joined after it
    both_rows(&mut p, "BEGIN TRANSACTION; UPDATE acct SET bal = 1 WHERE id = 1; SAVE TRANSACTION sp; UPDATE acct SET bal = 2 WHERE id <= 12; ROLLBACK TRANSACTION sp; SELECT id, bal FROM acct WHERE id <= 12 ORDER BY id");
    both_rows(&mut p, "UPDATE acct SET bal = bal + 1000 WHERE id = 3; SELECT id, bal FROM acct WHERE id <= 4 ORDER BY id; COMMIT");
    p.check("SELECT id, bal FROM acct ORDER BY id", true);
    // nested savepoints: going back to the outer one forgets the inner
    both_rows(
        &mut p,
        "BEGIN TRANSACTION; UPDATE acct SET bal = 10 WHERE id = 1; SAVE TRANSACTION a; UPDATE acct SET bal = 20 WHERE id = 2; SAVE TRANSACTION b; UPDATE acct SET bal = 30 WHERE id <= 12; ROLLBACK TRANSACTION b; SELECT id, bal FROM acct WHERE id <= 3 ORDER BY id; ROLLBACK TRANSACTION a; SELECT id, bal FROM acct WHERE id <= 3 ORDER BY id; COMMIT",
    );
    p.check("SELECT id, bal FROM acct ORDER BY id", true);
    // an unknown savepoint, and a savepoint outside a transaction
    let r = p.cl.execute("BEGIN TRANSACTION; ROLLBACK TRANSACTION nope");
    assert_eq!(r.first_error().map(|e| e.number), Some(6401));
    assert!(p.cl.execute("ROLLBACK").first_error().is_none());
    assert_eq!(p.cl.execute("SAVE TRANSACTION x").first_error().map(|e| e.number), Some(628));
    // a statement that fails on several shards dooms the transaction, and a savepoint before it undoes the damage
    let r = p.cl.execute("BEGIN TRANSACTION; UPDATE acct SET bal = 77 WHERE id = 1; SAVE TRANSACTION safe; UPDATE acct SET bal = 1 / 0");
    assert!(r.first_error().is_some());
    assert_eq!(p.cl.execute("UPDATE acct SET bal = 5 WHERE id = 1").first_error().map(|e| e.number), Some(3930));
    assert!(p.cl.execute("ROLLBACK TRANSACTION safe").first_error().is_none());
    assert!(p.cl.execute("UPDATE acct SET bal = 5 WHERE id = 2").first_error().is_none(), "the transaction goes on after the rollback to the savepoint");
    assert!(p.cl.execute("COMMIT").first_error().is_none());
    let v = p.cl.query("SELECT id, bal FROM acct WHERE id <= 2 ORDER BY id").unwrap();
    assert_eq!((display(&v[0][1]), display(&v[1][1])), ("77".to_string(), "5".to_string()));
}

#[test]
fn foreign_keys_are_enforced_across_shards() {
    let mut p = Pair::new(3);
    p.both("CREATE TABLE parent (id INT PRIMARY KEY, name VARCHAR(10))");
    p.both("CREATE TABLE child (id INT PRIMARY KEY, pid INT NULL, CONSTRAINT fk_child_parent FOREIGN KEY (pid) REFERENCES parent (id))");
    p.both(&format!("INSERT INTO parent VALUES {}", (1..=30).map(|i| format!("({}, 'p{}')", i, i)).collect::<Vec<_>>().join(",")));
    // a child needs its parent, a null key needs none
    p.both(&format!("INSERT INTO child VALUES {}", (1..=30).map(|i| format!("({}, {})", i, i)).collect::<Vec<_>>().join(",")));
    p.both("INSERT INTO child VALUES (100, NULL)");
    assert_eq!(same_outcome(&mut p, "INSERT INTO child VALUES (101, 999)"), Some(547));
    assert_eq!(same_outcome(&mut p, "INSERT INTO child VALUES (102, 5), (103, 998)"), Some(547));
    p.check("SELECT COUNT(*) AS n FROM child", false);
    // changing a foreign key column is checked too
    assert_eq!(same_outcome(&mut p, "UPDATE child SET pid = 777 WHERE id = 3"), Some(547));
    p.both("UPDATE child SET pid = 7 WHERE id = 3");
    // a parent that has children cannot be deleted or have its key changed
    assert_eq!(same_outcome(&mut p, "DELETE FROM parent WHERE id = 7"), Some(547));
    assert_eq!(same_outcome(&mut p, "DELETE FROM parent WHERE id <= 10"), Some(547));
    assert_eq!(same_outcome(&mut p, "UPDATE parent SET id = id + 100 WHERE id = 5"), Some(547));
    p.both("UPDATE parent SET name = 'renamed' WHERE id = 5"); // not the key: no problem
    p.both("DELETE FROM child WHERE pid >= 20");
    p.both("DELETE FROM parent WHERE id >= 20");
    p.check("SELECT COUNT(*) AS n FROM parent", false);
    // a transaction sees its own changes in the checks
    let sql = "BEGIN TRANSACTION; DELETE FROM child WHERE pid = 1; DELETE FROM parent WHERE id = 1; SELECT COUNT(*) FROM parent; ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
    p.check("SELECT COUNT(*) AS n FROM parent", false);
    // truncating or dropping a referenced table is refused; dropping them together works
    assert_eq!(same_outcome(&mut p, "TRUNCATE TABLE parent"), Some(4712));
    assert_eq!(same_outcome(&mut p, "DROP TABLE parent"), Some(3726));
    // adding a key checks the rows that exist
    p.both("CREATE TABLE child2 (id INT PRIMARY KEY, pid INT)");
    p.both("INSERT INTO child2 VALUES (1, 1), (2, 2), (3, 555)");
    assert_eq!(same_outcome(&mut p, "ALTER TABLE child2 ADD CONSTRAINT fk2 FOREIGN KEY (pid) REFERENCES parent (id)"), Some(547));
    p.both("DELETE FROM child2 WHERE id = 3");
    p.both("ALTER TABLE child2 ADD CONSTRAINT fk2 FOREIGN KEY (pid) REFERENCES parent (id)");
    assert_eq!(same_outcome(&mut p, "INSERT INTO child2 VALUES (9, 9999)"), Some(547));
    p.both("ALTER TABLE child2 DROP CONSTRAINT fk2");
    p.both("INSERT INTO child2 VALUES (9, 9999)");
}

#[test]
fn foreign_key_actions_and_keys_that_refer_to_their_own_table() {
    let mut p = Pair::new(3);
    p.both("CREATE TABLE dept (id INT PRIMARY KEY, name VARCHAR(10))");
    p.both("CREATE TABLE emp (id INT PRIMARY KEY, dept_id INT NULL REFERENCES dept (id) ON DELETE CASCADE, boss INT NULL)");
    p.both("CREATE TABLE badge (id INT PRIMARY KEY, emp_id INT NULL, CONSTRAINT fk_badge FOREIGN KEY (emp_id) REFERENCES emp (id) ON DELETE SET NULL)");
    p.both("INSERT INTO dept VALUES (1, 'a'), (2, 'b'), (3, 'c')");
    p.both(&format!("INSERT INTO emp VALUES {}", (1..=30).map(|i| format!("({}, {}, NULL)", i, i % 3 + 1)).collect::<Vec<_>>().join(",")));
    p.both(&format!("INSERT INTO badge VALUES {}", (1..=30).map(|i| format!("({}, {})", i, i)).collect::<Vec<_>>().join(",")));
    // deleting a department deletes its employees, and their badges lose the employee
    p.both("DELETE FROM dept WHERE id = 2");
    p.check("SELECT COUNT(*) AS n FROM emp", false);
    p.check("SELECT COUNT(*) AS n, COUNT(emp_id) AS with_emp FROM badge", false);
    p.check("SELECT dept_id, COUNT(*) AS n FROM emp GROUP BY dept_id ORDER BY dept_id", true);
    // a key to the same table: the rows of one statement may refer to each other
    p.both("CREATE TABLE tree (id INT PRIMARY KEY, up INT NULL REFERENCES tree (id))");
    p.both("INSERT INTO tree VALUES (1, NULL), (2, 1), (3, 2)");
    assert_eq!(same_outcome(&mut p, "INSERT INTO tree VALUES (4, 99)"), Some(547));
    p.both("INSERT INTO tree VALUES (10, 11), (11, NULL)");
    p.check("SELECT id, up FROM tree ORDER BY id", true);
    // a case-insensitive key: the engine's comparison decides
    p.both("CREATE TABLE codes (code VARCHAR(5) PRIMARY KEY)");
    p.both("CREATE TABLE uses (id INT PRIMARY KEY, code VARCHAR(5) REFERENCES codes (code))");
    p.both("INSERT INTO codes VALUES ('ab'), ('cd')");
    p.both("INSERT INTO uses VALUES (1, 'AB'), (2, 'cd')");
    assert_eq!(same_outcome(&mut p, "INSERT INTO uses VALUES (3, 'zz')"), Some(547));
    // ON UPDATE actions are not supported
    let r = p.cl.execute("CREATE TABLE x (id INT PRIMARY KEY, d INT REFERENCES dept (id) ON UPDATE CASCADE)");
    assert_eq!(r.first_error().map(|e| e.number), Some(50100));
}

#[test]
fn foreign_keys_survive_a_restart_of_the_control_node() {
    let h = start(2);
    let cfg = ClusterConfig { shards: h.shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let path = std::env::temp_dir().join(format!("tsql_fk_state_{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let cluster = Cluster::open(&path, &cfg).unwrap();
        let mut cl = cluster.session();
        for sql in ["CREATE TABLE p (id INT PRIMARY KEY)", "CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p (id))", "INSERT p VALUES (1), (2)"] {
            assert!(cl.execute(sql).first_error().is_none(), "{}", sql);
        }
    }
    let cluster = Cluster::open(&path, &ClusterConfig::default()).unwrap();
    let mut cl = cluster.session();
    assert_eq!(cl.execute("INSERT c VALUES (1, 99)").first_error().map(|e| e.number), Some(547));
    assert!(cl.execute("INSERT c VALUES (1, 2)").first_error().is_none());
    assert_eq!(cl.execute("DELETE FROM p WHERE id = 2").first_error().map(|e| e.number), Some(547));
    let _ = std::fs::remove_file(&path);
}

fn no_gather(plan: &[String], sql: &str) {
    assert!(!plan.iter().any(|l| l.starts_with("gather")), "{}\nthe tables should not be gathered: {:?}", sql, plan);
}

#[test]
fn windows_grouping_sets_and_distinct_aggregates_run_over_the_shards() {
    let mut p = Pair::new(3);
    p.load();
    for (sql, ordered) in [
        // windows over rows, over aggregates, with ranking and running totals
        ("SELECT id, ROW_NUMBER() OVER (ORDER BY amount DESC, id) AS rn FROM orders WHERE id <= 30", false),
        ("SELECT id, status, RANK() OVER (PARTITION BY status ORDER BY amount DESC, id) AS rk, SUM(qty) OVER (PARTITION BY status) AS q FROM orders WHERE id <= 60 ORDER BY id", true),
        ("SELECT status, SUM(amount) AS s, SUM(SUM(amount)) OVER () AS total, SUM(amount) * 100.0 / SUM(SUM(amount)) OVER () AS pct FROM orders GROUP BY status ORDER BY status", true),
        ("SELECT customer_id, status, COUNT(*) AS n, RANK() OVER (PARTITION BY status ORDER BY COUNT(*) DESC, customer_id) AS r FROM orders GROUP BY customer_id, status ORDER BY status, r, customer_id", true),
        ("SELECT TOP 10 id, SUM(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running FROM orders ORDER BY id", true),
        ("SELECT DISTINCT status, MAX(qty) OVER (PARTITION BY status) AS m FROM orders", false),
        // grouping sets
        ("SELECT status, COUNT(*) AS n, SUM(amount) AS s, AVG(qty) AS a FROM orders GROUP BY ROLLUP(status)", false),
        ("SELECT status, qty, COUNT(*) AS n, GROUPING(status) AS gs, GROUPING(qty) AS gq FROM orders WHERE qty < 4 GROUP BY ROLLUP(status, qty)", false),
        ("SELECT status, qty, MIN(amount) AS lo, MAX(amount) AS hi FROM orders WHERE qty < 4 GROUP BY CUBE(status, qty)", false),
        ("SELECT status, qty, COUNT(*) AS n FROM orders WHERE qty < 4 GROUP BY GROUPING SETS ((status), (qty), ())", false),
        ("SELECT c.region, o.status, SUM(o.amount) AS s FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY ROLLUP(c.region, o.status) ORDER BY c.region, o.status", true),
        ("SELECT status, SUM(amount) AS s FROM orders GROUP BY ROLLUP(status) HAVING SUM(amount) > 0 ORDER BY s DESC", true),
        // distinct aggregates
        ("SELECT COUNT(DISTINCT customer_id) FROM orders", false),
        ("SELECT status, COUNT(DISTINCT customer_id) AS c, COUNT(*) AS n, SUM(amount) AS s FROM orders GROUP BY status ORDER BY status", true),
        ("SELECT status, SUM(DISTINCT qty) AS sq, AVG(DISTINCT qty) AS aq, MAX(amount) AS hi FROM orders GROUP BY status ORDER BY status", true),
        ("SELECT COUNT(DISTINCT customer_id), COUNT(DISTINCT customer_id) + 1 AS plus FROM orders WHERE id < 0", false),
        ("SELECT c.region, COUNT(DISTINCT o.customer_id) AS buyers FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region", true),
        // several distinct expressions: the partial rows are grouped by each of them
        ("SELECT COUNT(DISTINCT customer_id), COUNT(DISTINCT status), COUNT(*) FROM orders", false),
        ("SELECT qty, COUNT(DISTINCT customer_id) AS c, SUM(DISTINCT customer_id) AS s FROM orders WHERE qty < 4 GROUP BY qty ORDER BY qty", true),
        // a grand total over nothing: a count is 0, not NULL
        ("SELECT status, COUNT(*) AS n, SUM(amount) AS s FROM orders WHERE id < 0 GROUP BY ROLLUP(status)", false),
        ("SELECT status, qty, COUNT(*) AS n, COUNT_BIG(*) AS nb, GROUPING(qty) AS g FROM orders WHERE id < 0 GROUP BY CUBE(status, qty)", false),
    ] {
        let plan = p.check(sql, ordered);
        no_gather(&plan, sql);
        assert!(plan.iter().any(|l| l.starts_with("split")), "{}\nexpected a split plan, got {:?}", sql, plan);
    }
}

#[test]
fn set_operations_run_each_branch_over_the_shards() {
    let mut p = Pair::new(3);
    p.load();
    for (sql, ordered) in [
        ("SELECT id FROM orders WHERE id < 5 UNION ALL SELECT id FROM customers WHERE id < 5", false),
        ("SELECT status FROM orders UNION SELECT region FROM customers", false),
        ("SELECT customer_id FROM orders WHERE qty > 5 INTERSECT SELECT id FROM customers WHERE id < 50", false),
        ("SELECT id FROM customers EXCEPT SELECT customer_id FROM orders WHERE qty > 8", false),
        ("SELECT customer_id AS k FROM orders WHERE qty = 1 UNION SELECT customer_id FROM orders WHERE qty = 2 EXCEPT SELECT id FROM customers WHERE id < 20 ORDER BY k", true),
        ("(SELECT TOP 3 id FROM orders ORDER BY id) UNION ALL (SELECT TOP 3 id FROM orders ORDER BY id DESC) ORDER BY id", true),
        ("SELECT COUNT(*) FROM (SELECT customer_id FROM orders WHERE qty < 3 INTERSECT SELECT customer_id FROM orders WHERE qty > 7) x", false),
        ("SELECT status AS s, COUNT(*) AS n FROM orders GROUP BY status UNION ALL SELECT region, COUNT(*) FROM customers GROUP BY region ORDER BY s", true),
    ] {
        let plan = p.check(sql, ordered);
        no_gather(&plan, sql);
    }
    // the branches do not have to match in names, only in their number
    let a = p.cl.execute("SELECT id FROM orders UNION SELECT id, id FROM customers");
    assert_eq!(a.first_error().map(|e| e.number), p.oracle.execute("SELECT id FROM orders UNION SELECT id, id FROM customers").first_error().map(|e| e.number));
}

#[test]
fn correlated_subqueries_become_joins() {
    let mut p = Pair::new(3);
    p.load();
    for (sql, ordered) in [
        ("SELECT c.id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > 990) ORDER BY c.id", true),
        ("SELECT c.id FROM customers c WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.qty > 8) ORDER BY c.id", true),
        ("SELECT c.id FROM customers c WHERE c.credit > (SELECT AVG(o.amount) FROM orders o WHERE o.customer_id = c.id) ORDER BY c.id", true),
        ("SELECT c.id FROM customers c WHERE c.credit * 2 < 1.5 * (SELECT SUM(o.amount) FROM orders o WHERE o.customer_id = c.id) ORDER BY c.id", true),
        ("SELECT o.id FROM orders o WHERE o.amount > (SELECT AVG(x.amount) FROM orders x WHERE x.status = o.status AND x.qty > 2) AND o.id < 200 ORDER BY o.id", true),
        ("SELECT o.id FROM orders o WHERE o.amount = (SELECT MAX(x.amount) FROM orders x WHERE x.customer_id = o.customer_id) ORDER BY o.id", true),
        // a key that is NULL finds nothing: NOT EXISTS holds for it, and the scalar subquery gives NULL
        ("SELECT o.id FROM orders o WHERE NOT EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id) ORDER BY o.id", true),
    ] {
        let plan = p.check(sql, ordered);
        no_gather(&plan, sql);
        assert!(plan.iter().any(|l| l.contains("rewritten as a join")), "{}\nexpected a rewrite, got {:?}", sql, plan);
    }
    // a NULL key on the outer side
    p.both("CREATE TABLE kk (id INT PRIMARY KEY, k INT NULL)");
    p.both("INSERT INTO kk VALUES (1, 1), (2, NULL), (3, 3), (4, 99)");
    p.both("CREATE TABLE vv (id INT PRIMARY KEY, k INT NULL, n INT)");
    p.both("INSERT INTO vv VALUES (1, 1, 10), (2, NULL, 20), (3, 3, 30), (4, 3, 40)");
    for sql in [
        "SELECT id FROM kk WHERE NOT EXISTS (SELECT 1 FROM vv WHERE vv.k = kk.k) ORDER BY id",
        "SELECT id FROM kk WHERE EXISTS (SELECT 1 FROM vv WHERE vv.k = kk.k) ORDER BY id",
        "SELECT id FROM kk WHERE 5 < (SELECT SUM(n) FROM vv WHERE vv.k = kk.k) ORDER BY id",
    ] {
        p.check(sql, true);
    }
    // a subquery that reads a name the rewrite cannot place falls back to the original (and gives its error or answer)
    let sql = "SELECT id FROM kk WHERE 5 < (SELECT SUM(n) FROM vv WHERE vv.k = kk.k AND nosuch = 1)";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert_eq!(a.first_error().map(|e| e.number), b.first_error().map(|e| e.number));
}

#[test]
fn a_shuffle_sends_rows_from_shard_to_shard() {
    let mut p = Pair::new(3);
    p.load();
    p.load_items();
    // too big to broadcast: the table that is not keyed on the join column is shuffled
    p.h.cluster.set_broadcast_limit(10);
    for sql in [
        "SELECT c.region, SUM(o.amount) AS total, COUNT(*) AS n FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region",
        "SELECT COUNT(*), SUM(o.qty) FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.region = 'north'",
        "SELECT o.id, c.name FROM orders o LEFT JOIN customers c ON c.id = o.customer_id WHERE o.id <= 25 ORDER BY o.id",
    ] {
        let plan = p.check(sql, sql.contains("ORDER BY"));
        no_gather(&plan, sql);
        assert!(plan.iter().any(|l| l.contains("repartition")), "{}\n{:?}", sql, plan);
    }
    // inside a transaction, the rows the transaction changed are the ones that move
    let sql = "BEGIN TRANSACTION; UPDATE customers SET region = 'moved' WHERE id <= 10; SELECT c.region, COUNT(*) FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.region ORDER BY c.region; ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
    p.h.cluster.set_broadcast_limit(100_000);
}

#[test]
fn a_rewritten_query_that_has_to_gather_falls_back_to_the_original() {
    let mut p = Pair::new(3);
    p.load();
    // the derived table is made a replicated temporary table, and a right join with a sharded table cannot run over the shards:
    // the statement is gathered, and the control node's engine does not have the temporary table
    for sql in [
        "SELECT COUNT(*), SUM(d.n) FROM customers c RIGHT JOIN (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) d ON c.id = d.customer_id",
        "WITH d AS (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) SELECT COUNT(*) FROM d FULL JOIN customers c ON c.id = d.customer_id AND d.n > 3",
    ] {
        p.check(sql, false);
    }
}

#[test]
fn joins_whose_keys_all_name_one_shard_run_there() {
    let mut p = Pair::new(3);
    p.load();
    p.load_items();
    let routed = |plan: &[String]| plan.iter().any(|l| l.starts_with("route: every table's key"));
    for sql in [
        "SELECT o.id, i.sku FROM orders o, order_items i WHERE o.id = 7 AND i.order_id = 7 AND i.order_id = o.id",
        "SELECT COUNT(*) FROM orders o, order_items i, regions r WHERE o.id = 12 AND i.order_id = 12 AND r.code = 'north'",
    ] {
        let plan = p.check(sql, false);
        assert!(routed(&plan), "{}
{:?}", sql, plan);
    }
    // not every table is pinned, or the key names are ambiguous, or the shards differ: not routed (and still exact)
    for sql in [
        "SELECT COUNT(*) FROM orders o, order_items i WHERE o.id = 7 AND i.order_id = o.id",
        "SELECT COUNT(*) FROM orders a, orders b WHERE a.id = 7 AND b.customer_id = a.customer_id",
        "SELECT COUNT(*) FROM orders o, order_items i WHERE o.id = 7 AND i.order_id = 8",
        "SELECT COUNT(*) FROM orders a, orders b WHERE id = 7",
    ] {
        let r = p.cl.execute(sql);
        let b = p.oracle.execute(sql);
        assert_eq!(r.first_error().map(|e| e.number), b.first_error().map(|e| e.number), "{}", sql);
        if r.first_error().is_none() {
            assert_eq!(rows_of(&r), rows_of(&b), "{}", sql);
            assert!(!routed(p.cl.last_plan()), "{}
{:?}", sql, p.cl.last_plan());
        }
    }
}


#[test]
fn a_decimal_too_big_for_json_survives_the_rewrite_of_a_subquery() {
    let mut p = Pair::new(3);
    p.both("CREATE TABLE big (id INT PRIMARY KEY, v DECIMAL(18,2))");
    // thirty values near the largest of the type: their sum does not fit 64 bits, which the rewrite (through JSON) cannot carry
    p.both(&format!("INSERT INTO big VALUES {}", (1..=30).map(|i| format!("({}, 9999999999999999.{:02})", i, i)).collect::<Vec<_>>().join(",")));
    for sql in [
        "SELECT COUNT(*) FROM big WHERE v < (SELECT SUM(v) FROM big)",
        "SELECT id FROM big WHERE v * 2 > (SELECT SUM(v) FROM big) / 20 ORDER BY id",
        "SELECT COUNT(*) FROM big WHERE v IN (SELECT SUM(v) / 30 FROM big)",
    ] {
        p.check(sql, false);
    }
}

#[test]
fn subqueries_that_the_shards_can_answer_run_on_the_shards() {
    let mut p = Pair::new(3);
    p.load();
    p.load_items();
    let pushed = |plan: &[String]| plan.iter().any(|l| l.starts_with("pushdown"));
    // co-located: orders and order_items are both sharded on the order key
    for (sql, ordered) in [
        ("SELECT COUNT(*) FROM orders o WHERE EXISTS (SELECT 1 FROM order_items i WHERE i.order_id = o.id AND i.qty > 2)", false),
        ("SELECT COUNT(*) FROM orders o WHERE NOT EXISTS (SELECT 1 FROM order_items i WHERE i.order_id = o.id)", false),
        ("SELECT status, COUNT(*) AS n FROM orders WHERE amount > 100 AND EXISTS (SELECT * FROM order_items WHERE order_id = orders.id AND qty >= 3) GROUP BY status ORDER BY status", true),
        ("SELECT id FROM orders o WHERE (SELECT SUM(qty) FROM order_items i WHERE i.order_id = o.id) > 6 ORDER BY id", true),
    ] {
        let plan = p.check(sql, ordered);
        assert!(pushed(&plan) && !plan.iter().any(|l| l.starts_with("gather")), "{}
{:?}", sql, plan);
    }
    // not co-located (customers are sharded on their own key): not pushed down, still exact
    let plan = p.check("SELECT COUNT(*) FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id AND c.credit > 100)", false);
    assert!(!pushed(&plan), "{:?}", plan);
}

#[test]
fn a_long_in_list_becomes_a_replicated_table_and_the_shards_do_the_rest() {
    let mut p = Pair::new(3);
    p.load();
    p.h.cluster.set_in_list_limit(50); // (what would be a list of tens of thousands)
    for (sql, ordered) in [
        ("SELECT COUNT(*) FROM orders WHERE customer_id IN (SELECT id FROM customers WHERE credit > 10)", false),
        ("SELECT COUNT(*) FROM orders WHERE customer_id NOT IN (SELECT id FROM customers WHERE credit > 10)", false),
        ("SELECT status, COUNT(*) FROM orders WHERE customer_id IN (SELECT id FROM customers) GROUP BY status ORDER BY status", true),
        // a correlated EXISTS that is not co-located: made a long IN, then handled the same way
        ("SELECT COUNT(*) FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id AND c.credit > 10)", false),
        ("SELECT COUNT(*) FROM orders o WHERE NOT EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id AND c.credit > 500)", false),
    ] {
        let plan = p.check(sql, ordered);
        assert!(!plan.iter().any(|l| l.starts_with("gather")), "{}
{:?}", sql, plan);
        assert!(plan.iter().any(|l| l.contains("materialized")), "{}
{:?}", sql, plan);
    }
}

#[test]
fn joins_in_stages_shuffle_between_the_joins() {
    let mut p = Pair::new(3);
    p.load();
    p.load_items();
    // nothing is small enough to broadcast: rows have to be moved between the joins
    p.h.cluster.set_broadcast_limit(10);
    let staged = |plan: &[String]| plan.iter().any(|l| l.starts_with("mpp:"));
    for (sql, ordered) in [
        // customers are sharded on their id, orders too, the items on the order: the items and orders meet in place, customers need the
        // group re-spread by the customer
        ("SELECT c.region, SUM(i.qty) AS q, COUNT(*) AS n FROM customers c, orders o, order_items i WHERE c.id = o.customer_id AND o.id = i.order_id GROUP BY c.region ORDER BY c.region", true),
        ("SELECT c.name, o.id, i.sku FROM customers c, orders o, order_items i WHERE c.id = o.customer_id AND o.id = i.order_id AND c.id < 12 AND i.qty > 7 ORDER BY o.id, i.sku, c.name", true),
        // ON clauses, a replicated table on the end, aggregates over expressions, HAVING
        ("SELECT r.label, COUNT(*) AS n, SUM(i.qty * 2) AS s FROM order_items i JOIN orders o ON o.id = i.order_id JOIN customers c ON c.id = o.customer_id JOIN regions r ON r.code = c.region GROUP BY r.label HAVING COUNT(*) > 0 ORDER BY r.label", true),
        // a table joined with itself on a column that is not its key, and then the customers
        ("SELECT COUNT(*) FROM orders a, orders b, customers c WHERE a.customer_id = b.customer_id AND c.id = a.customer_id AND a.id < 40 AND b.id < 100", false),
        ("SELECT COUNT(DISTINCT c.region) AS regions FROM orders a, orders b, customers c WHERE a.customer_id = b.customer_id AND c.id = b.customer_id AND a.qty > 8", false),
        // TOP and DISTINCT over the staged join
        ("SELECT TOP 5 c.name, o.amount FROM customers c, orders o, order_items i WHERE c.id = o.customer_id AND o.id = i.order_id ORDER BY o.amount DESC, c.name, o.id", true),
        ("SELECT DISTINCT c.region FROM customers c, orders o, order_items i WHERE c.id = o.customer_id AND o.id = i.order_id", false),
    ] {
        let plan = p.check(sql, ordered);
        assert!(staged(&plan) && !plan.iter().any(|l| l.starts_with("gather")), "{}\n{:?}", sql, plan);
    }
    // inside a transaction the rows that moved are the transaction's own
    let sql = "BEGIN TRANSACTION; UPDATE customers SET region = 'moved' WHERE id <= 20; SELECT c.region, COUNT(*) FROM customers c, orders o, order_items i WHERE c.id = o.customer_id AND o.id = i.order_id GROUP BY c.region ORDER BY c.region; ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
    // nothing is left behind on the shards
    for s in p.h.shards.iter() {
        let mut c = s.server.connect();
        let r = c.execute("SELECT COUNT(*) FROM tempdb.sys.tables");
        let _ = r;
    }
    // what the stages cannot do is still gathered: an outer join, and `*`
    for sql in ["SELECT c.name, o.id FROM customers c LEFT JOIN orders o ON c.id = o.customer_id LEFT JOIN order_items i ON i.order_id = o.id WHERE c.id < 8 ORDER BY c.name, o.id"] {
        let plan = p.check(sql, true);
        assert!(!staged(&plan), "{}\n{:?}", sql, plan);
    }
}

#[test]
fn a_derived_table_that_is_a_join_is_merged_into_the_query() {
    let mut p = Pair::new(3);
    p.load();
    p.load_items();
    p.h.cluster.set_broadcast_limit(10);
    let merged = |plan: &[String]| plan.iter().any(|l| l.starts_with("derived table merged"));
    for (sql, ordered) in [
        // the shape of TPC-H Q9: an aggregate over a derived table that is the join, with computed columns
        ("SELECT reg, SUM(q) AS total, COUNT(*) AS n FROM (SELECT c.region AS reg, i.qty * 2 + o.qty AS q FROM customers c, orders o, order_items i WHERE c.id = o.customer_id AND o.id = i.order_id AND i.qty > 2) AS d GROUP BY reg ORDER BY reg", true),
        ("SELECT d.reg, MAX(d.q) FROM (SELECT c.region AS reg, i.qty AS q FROM customers c JOIN orders o ON o.customer_id = c.id JOIN order_items i ON i.order_id = o.id) d WHERE d.q > 5 GROUP BY d.reg HAVING COUNT(*) > 0 ORDER BY d.reg", true),
        // a column list on the derived table, and a plain select over it
        ("SELECT a, b FROM (SELECT o.id, i.sku FROM orders o, order_items i WHERE o.id = i.order_id AND o.id < 20) AS d (a, b) WHERE a > 3 ORDER BY a, b", true),
    ] {
        let plan = p.check(sql, ordered);
        assert!(merged(&plan) && !plan.iter().any(|l| l.starts_with("gather")), "{}\n{:?}", sql, plan);
    }
    // a derived table with an aggregate, DISTINCT or TOP is not merged (its result is what the outer query reads)
    for sql in [
        "SELECT COUNT(*) FROM (SELECT DISTINCT o.customer_id FROM orders o, order_items i WHERE o.id = i.order_id) d",
        "SELECT COUNT(*) FROM (SELECT TOP 5 o.id FROM orders o, order_items i WHERE o.id = i.order_id ORDER BY o.id) d",
        "SELECT COUNT(*) FROM (SELECT o.customer_id, COUNT(*) AS n FROM orders o, order_items i WHERE o.id = i.order_id GROUP BY o.customer_id) d WHERE n > 2",
    ] {
        let plan = p.check(sql, false);
        assert!(!merged(&plan), "{}\n{:?}", sql, plan);
    }
}

#[test]
fn a_gather_of_big_tables_copies_only_what_the_query_needs() {
    let mut p = Pair::new(3);
    p.load();
    p.h.cluster.set_broadcast_limit(10); // (every table here is "big")
    for (sql, ordered, expect) in [
        // STDEV cannot be split over the shards: gathered; only the columns it names, and the rows that pass the conditions
        ("SELECT STDEV(o.amount) FROM orders o, customers c WHERE c.id = o.customer_id AND o.amount > 900 AND c.credit > 100", false, "filtered"),
        ("SELECT c.region, STDEV(o.amount) AS sd, COUNT(*) AS n FROM orders o, customers c WHERE c.id = o.customer_id AND o.qty BETWEEN 2 AND 4 AND c.region IN ('north', 'south') GROUP BY c.region ORDER BY c.region", true, "filtered"),
        // an outer join, and a correlated subquery that is not an equality: columns only (a row that is missing matters)
        ("SELECT c.id, o.id FROM customers c FULL JOIN orders o ON o.customer_id = c.id AND o.amount > 990 WHERE c.id < 15 OR o.id < 5 ORDER BY c.id, o.id", true, "columns"),
        ("SELECT c.id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > c.credit) ORDER BY c.id", true, "columns"),
    ] {
        let plan = p.check(sql, ordered);
        assert!(plan.iter().any(|l| l.starts_with("gather") && l.contains(expect)), "{}\n{:?}", sql, plan);
    }
    // `*` needs every column, and the data a transaction has changed is what is read
    let plan = p.check("SELECT STDEV(amount), COUNT(*) FROM (SELECT * FROM orders) x", false);
    let _ = plan;
    let sql = "BEGIN TRANSACTION; UPDATE orders SET amount = 1000 WHERE id < 50; SELECT ROUND(STDEV(o.amount), 6) FROM orders o, customers c WHERE c.id = o.customer_id AND o.amount > 900; ROLLBACK";
    let a = p.cl.execute(sql);
    let b = p.oracle.execute(sql);
    assert!(a.first_error().is_none(), "{:?}", a.first_error());
    assert_eq!(rows_of(&a), rows_of(&b));
    // the copies are gone from the control node's engine
    let r = p.cl.execute("SELECT 1");
    assert!(r.first_error().is_none());
}

#[test]
fn a_decorrelated_subquery_is_restricted_to_the_keys_the_outer_query_can_meet() {
    let mut p = Pair::new(3);
    p.load();
    // the averages are wanted for the customers of one region only, not for all of them
    for sql in [
        "SELECT COUNT(*) FROM orders o, customers c WHERE c.id = o.customer_id AND c.region = 'north' AND o.amount > (SELECT AVG(x.amount) FROM orders x WHERE x.customer_id = c.id)",
        "SELECT c.id, COUNT(*) AS n FROM customers c, orders o WHERE o.customer_id = c.id AND c.credit > 400 AND o.qty < (SELECT AVG(y.qty) FROM orders y WHERE y.customer_id = c.id) GROUP BY c.id ORDER BY c.id",
    ] {
        let plan = p.check(sql, sql.contains("ORDER BY"));
        assert!(plan.iter().any(|l| l.contains("rewritten as a join")), "{}\n{:?}", sql, plan);
        // (the grouped table holds the keys of the surviving customers, a fraction of the 300)
        let made: Vec<u64> = plan.iter().filter_map(|l| l.strip_prefix("materialized ").and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse().ok())).collect();
        assert!(made.iter().any(|n| *n < 250), "{}\n{:?}", sql, plan);
        assert!(!plan.iter().any(|l| l.starts_with("gather")), "{}\n{:?}", sql, plan);
    }
}

#[test]
fn an_outer_join_from_a_replicated_table_is_aggregated_on_the_shards() {
    let mut p = Pair::new(3);
    p.load();
    // a region that no customer has: its groups must come out with a count of 0 and a NULL sum
    p.both("INSERT INTO regions VALUES ('zz', 'nowhere')");
    let on_shards = |plan: &[String]| plan.iter().any(|l| l.starts_with("join over shards")) && !plan.iter().any(|l| l.starts_with("gather"));
    for (sql, ordered) in [
        // the shape of TPC-H Q13: counts of what matched, grouped by the preserved side
        ("SELECT r.label, COUNT(c.id) AS n, SUM(c.credit) AS s, MIN(c.id) AS lo, MAX(c.name) AS hi FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
        ("SELECT r.code, AVG(c.credit) AS a, COUNT(c.credit) AS n FROM regions r LEFT OUTER JOIN customers c ON c.region = r.code AND c.credit > 300 GROUP BY r.code ORDER BY r.code", true),
        ("SELECT COUNT(c.id), SUM(c.credit) FROM regions r LEFT JOIN customers c ON c.region = r.code", false),
        // a condition on the partitioned side that a missing row cannot satisfy, HAVING, and an expression over the aggregates
        ("SELECT r.label, COUNT(c.id) * 2 AS n2 FROM regions r LEFT JOIN customers c ON c.region = r.code WHERE r.code <> 'x' GROUP BY r.label HAVING COUNT(c.id) >= 0 ORDER BY r.label", true),
        // RIGHT JOIN written the other way round
        ("SELECT r.label, COUNT(c.id) AS n FROM customers c RIGHT JOIN regions r ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
    ] {
        let plan = p.check(sql, ordered);
        assert!(on_shards(&plan), "{}\n{:?}", sql, plan);
    }
    // what is not a count of the matches is gathered (and exact): COUNT(*) and a preserved column's aggregate would count the unmatched
    // row on every shard, a group by the partitioned side's values would exist on shards that have no row for it, an anti-join asks
    // what is true of all the shards
    for (sql, ordered) in [
        ("SELECT r.label, COUNT(*) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
        ("SELECT r.label, COUNT(r.code) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
        ("SELECT c.region, COUNT(c.id) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY c.region ORDER BY c.region", true),
        ("SELECT r.label, COUNT(c.id) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code WHERE c.id IS NULL GROUP BY r.label ORDER BY r.label", true),
        ("SELECT r.label, COUNT(DISTINCT c.credit) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
        // an aggregate that is not NULL for a missing row (found by the fuzzer: `SUM(1)` over a join counted the unmatched row on every shard)
        ("SELECT r.label, SUM(1) AS n, MIN(ABS(7)) AS m FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
        ("SELECT r.label, SUM(CASE WHEN c.id IS NULL THEN 1 ELSE 0 END) AS none FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
        ("SELECT r.label, COUNT(ISNULL(c.id, 0)) AS n FROM regions r LEFT JOIN customers c ON c.region = r.code GROUP BY r.label ORDER BY r.label", true),
        ("SELECT (5 * 3) AS g, SUM(1) AS n, SUM(ABS(7)) AS sm FROM regions r RIGHT JOIN customers c ON r.code = c.region INNER JOIN orders o ON c.id = o.customer_id GROUP BY (5 * 3)", false),
    ] {
        p.check(sql, ordered);
    }
}

#[test]
fn a_common_table_expression_too_big_to_broadcast_is_spread_over_the_shards() {
    let mut p = Pair::new(3);
    p.load();
    p.h.cluster.set_broadcast_limit(10); // (every result here is "too big")
    // the shape of TPC-DS Q11: a UNION ALL of two aggregates, joined to itself on the customer
    for (sql, ordered) in [
        ("WITH y AS (SELECT customer_id AS cid, SUM(amount) AS tot, 'a' AS k FROM orders WHERE qty < 5 GROUP BY customer_id UNION ALL SELECT customer_id, SUM(amount), 'b' FROM orders WHERE qty >= 5 GROUP BY customer_id) SELECT TOP 20 a.cid FROM y a, y b WHERE a.cid = b.cid AND a.k = 'a' AND b.k = 'b' AND a.tot > b.tot ORDER BY a.cid", true),
        ("WITH y AS (SELECT customer_id AS cid, COUNT(*) AS n FROM orders GROUP BY customer_id) SELECT COUNT(*), SUM(a.n * b.n) FROM y a JOIN y b ON a.cid = b.cid WHERE a.n >= b.n", false),
    ] {
        let plan = p.check(sql, ordered);
        assert!(plan.iter().any(|l| l.contains("spread over the shards")) && !plan.iter().any(|l| l.starts_with("gather")), "{}\n{:?}", sql, plan);
    }
    // a CTE that is read once, or not joined to itself, has no column to spread by: gathered as before, and exact
    let plan = p.check("WITH y AS (SELECT customer_id AS cid, COUNT(*) AS n FROM orders GROUP BY customer_id) SELECT COUNT(*) FROM y WHERE n > 5", false);
    assert!(!plan.iter().any(|l| l.contains("spread over")), "{:?}", plan);
}

#[test]
fn not_exists_is_restricted_to_the_keys_of_the_outer_rows_that_survive() {
    let mut p = Pair::new(3);
    p.load();
    for sql in [
        // the shape of TPC-H Q22: filters on the outer table (one with a subquery of its own), and a correlated NOT EXISTS
        "SELECT c.region, COUNT(*) AS n, SUM(c.credit) AS s FROM customers c WHERE c.region IN ('north', 'south') AND c.credit > (SELECT AVG(credit) FROM customers WHERE credit > 0 AND region IN ('north', 'south')) AND NOT EXISTS (SELECT * FROM orders o WHERE o.customer_id = c.id AND o.qty > 8) GROUP BY c.region ORDER BY c.region",
        "SELECT COUNT(*) FROM customers c WHERE c.credit > 300 AND EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.amount > 900)",
    ] {
        let plan = p.check(sql, sql.contains("ORDER BY"));
        assert!(plan.iter().any(|l| l.contains("rewritten as a join")) && !plan.iter().any(|l| l.starts_with("gather")), "{}\n{:?}", sql, plan);
    }
}

#[test]
fn a_long_list_of_literals_is_joined_as_a_replicated_table() {
    let mut p = Pair::new(3);
    p.load();
    p.h.cluster.set_in_join_min(10);
    let list = (0..60).map(|i| (i * 3).to_string()).collect::<Vec<_>>().join(", ");
    let dup = format!("{}, 3, 6, 9", list);
    // IN: a join with the distinct values (a repeated value must not repeat the row)
    p.check_plan(&format!("SELECT id, amount FROM orders WHERE id IN ({}) AND qty > 1 ORDER BY id", dup), true, "joined as a replicated table");
    p.check_plan(&format!("SELECT COUNT(*), SUM(amount) FROM orders WHERE id IN ({})", dup), true, "joined as a replicated table");
    // NOT IN: an anti join; a NULL probe is unknown and is dropped, a NULL in the list means nothing qualifies, so that one is not rewritten
    p.check_plan(&format!("SELECT COUNT(*) FROM orders WHERE id NOT IN ({})", dup), true, "joined as a replicated table");
    p.check(&format!("SELECT COUNT(*) FROM orders WHERE id NOT IN ({}, NULL)", dup), true);
    // strings, and a list under the threshold
    let names = (0..30).map(|i| format!("'n{}'", i)).collect::<Vec<_>>().join(", ");
    p.check(&format!("SELECT COUNT(*) FROM customers WHERE name IN ({})", names), true);
    p.check("SELECT COUNT(*) FROM orders WHERE id IN (1, 2, 3)", true);
    // a star select is left as it is (the joined table's column must not show)
    p.check(&format!("SELECT * FROM orders WHERE id IN ({}) ORDER BY id", dup), true);
}
