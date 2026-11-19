//! The control node as an ordinary SQL Server to its clients: a TDS client (tiberius) connects to the control node, creates
//! tables, inserts, queries with parameters, and runs transactions; the rows end up spread over the shards.
use std::sync::Arc;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, IntoRow};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tsql_control::Cluster;
use tsql_server::tds::{Config as ListenConfig, Listener};
use tsql_server::Server;
use tsql_shared_ces::{ClusterConfig, ShardInfo};
use tsql_storage::{spawn, RunningShard, ShardNode};

type C = Client<Compat<TcpStream>>;

struct Front {
    rt: tokio::runtime::Runtime,
    shards: Vec<RunningShard>,
    cluster: Arc<Cluster>,
    listener: Listener,
}

fn start(n: usize) -> Front {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut shards = vec![];
    for i in 0..n {
        shards.push(rt.block_on(spawn(ShardNode::new(i as u32, Server::new()), "127.0.0.1:0".parse().unwrap())).unwrap());
    }
    let cfg = ClusterConfig { shards: shards.iter().enumerate().map(|(i, s)| ShardInfo { id: i as u32, addr: s.addr.to_string() }).collect() };
    let cluster = Cluster::connect(&cfg).unwrap();
    cluster.enable_tds();
    let server = cluster.mirror_server();
    server.connect_internal().execute("ALTER LOGIN sa WITH PASSWORD = N'Str0ng!pw'");
    let listener = Listener::bind(server, "127.0.0.1:0", ListenConfig::default()).unwrap();
    Front { rt, shards, cluster, listener }
}

async fn login(l: &Listener) -> C {
    let mut cfg = Config::new();
    cfg.host("127.0.0.1");
    cfg.port(l.local_addr().port());
    cfg.authentication(AuthMethod::sql_server("sa", "Str0ng!pw"));
    cfg.encryption(EncryptionLevel::NotSupported);
    let tcp = TcpStream::connect(cfg.get_addr()).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    Client::connect(cfg, tcp.compat_write()).await.expect("login")
}

async fn strings(c: &mut C, sql: &str) -> Vec<Vec<String>> {
    let rows = c.simple_query(sql).await.unwrap().into_first_result().await.unwrap();
    rows.iter().map(|r| r.cells().map(|(_, v)| format!("{:?}", v)).collect()).collect()
}

async fn run(c: &mut C, sql: &str) -> u64 {
    c.simple_query(sql).await.unwrap().into_results().await.unwrap();
    0
}

#[test]
fn an_ordinary_sql_client_uses_the_cluster() {
    let f = start(3);
    f.rt.block_on(async {
        let mut c = login(&f.listener).await;
        run(&mut c, "CREATE TABLE people (id INT PRIMARY KEY, name VARCHAR(20), score DECIMAL(6,2))").await;
        let values: Vec<String> = (1..=60).map(|i| format!("({}, 'p{}', {}.50)", i, i, i)).collect();
        run(&mut c, &format!("INSERT INTO people VALUES {}", values.join(", "))).await;
        // an aggregate over the cluster, a lookup by key, and a query that reads no distributed table
        let r = strings(&mut c, "SELECT COUNT(*) AS n, CAST(SUM(score) AS VARCHAR(20)) AS total FROM people").await;
        assert!(r[0][0].contains("60") && r[0][1].contains("1860.00"), "{:?}", r);
        let r = strings(&mut c, "SELECT name FROM people WHERE id = 42").await;
        assert!(r[0][0].contains("p42"), "{:?}", r);
        let r = strings(&mut c, "SELECT 1 + 1 AS two").await;
        assert!(r[0][0].contains('2'), "{:?}", r);
        // parameters (sp_executesql, as ADO.NET and the other drivers send them)
        let rows = c.query("SELECT name FROM people WHERE id = @P1", &[&7i32]).await.unwrap().into_first_result().await.unwrap();
        assert_eq!(rows[0].get::<&str, _>("name"), Some("p7"));
        let rows = c.query("SELECT COUNT(*) AS n FROM people WHERE id > @P1 AND name <> @P2", &[&50i32, &"p55"]).await.unwrap().into_first_result().await.unwrap();
        assert_eq!(rows[0].get::<i32, _>("n"), Some(9));
        let res = c.execute("UPDATE people SET score = score + @P1 WHERE id = @P2", &[&1i32, &9i32]).await.unwrap();
        assert_eq!(res.total(), 1);
        // transactions: a rollback undoes a change on a shard, a commit keeps it
        run(&mut c, "BEGIN TRANSACTION").await;
        run(&mut c, "DELETE FROM people WHERE id IN (1, 2, 3, 4, 5)").await;
        assert!(strings(&mut c, "SELECT COUNT(*) AS n FROM people").await[0][0].contains("55"));
        run(&mut c, "ROLLBACK").await;
        assert!(strings(&mut c, "SELECT COUNT(*) AS n FROM people").await[0][0].contains("60"));
        run(&mut c, "BEGIN TRANSACTION").await;
        run(&mut c, "DELETE FROM people WHERE id <= 10").await;
        run(&mut c, "COMMIT").await;
        assert!(strings(&mut c, "SELECT COUNT(*) AS n FROM people").await[0][0].contains("50"));
        // a join is run over the shards too
        run(&mut c, "CREATE TABLE tags (person_id INT, tag VARCHAR(10), PRIMARY KEY (person_id, tag))").await;
        run(&mut c, "INSERT INTO tags VALUES (11, 'a'), (11, 'b'), (12, 'a'), (40, 'c')").await;
        let r = strings(&mut c, "SELECT p.name, COUNT(*) AS k FROM people p JOIN tags t ON t.person_id = p.id GROUP BY p.name ORDER BY p.name").await;
        assert_eq!(r.len(), 3, "{:?}", r);
        // an error is the error of the engine, and the connection goes on
        let msg = match c.simple_query("INSERT INTO people VALUES (11, 'dup', 1.0)").await {
            Err(e) => e.to_string(),
            Ok(s) => s.into_results().await.err().expect("a duplicate key").to_string(),
        };
        assert!(msg.contains("2627"), "{}", msg);
        assert!(strings(&mut c, "SELECT COUNT(*) AS n FROM people").await[0][0].contains("50"));
    });
    // the rows really are spread over the shards
    let mut total = 0;
    for s in &f.shards {
        let n = match s.server.connect().query("SELECT COUNT(*) FROM people").unwrap()[0][0].clone() {
            tsql_server::Value::Int(n) => n,
            other => panic!("{:?}", other),
        };
        assert!(n > 5, "a shard holds {} of 50 rows", n);
        total += n;
    }
    assert_eq!(total, 50);
    f.listener.shutdown();
    let _ = &f.cluster;
}

#[test]
fn bulk_load_spreads_rows_over_the_shards_and_a_failed_load_leaves_nothing() {
    let f = start(3);
    f.rt.block_on(async {
        let mut c = login(&f.listener).await;
        run(&mut c, "CREATE TABLE bulky (id INT PRIMARY KEY, name NVARCHAR(40) NULL, amount FLOAT NULL)").await;
        let mut req = c.bulk_insert("bulky").await.unwrap();
        for i in 0..3000i32 {
            let name: Option<String> = if i % 10 == 0 { None } else { Some(format!("n\u{e9}{}", i)) };
            req.send((i, name, i as f64 * 0.5).into_row()).await.unwrap();
        }
        assert_eq!(req.finalize().await.unwrap().total(), 3000);
        let r = strings(&mut c, "SELECT COUNT(*) AS n, COUNT(name) AS named FROM bulky").await;
        assert!(r[0][0].contains("3000") && r[0][1].contains("2700"), "{:?}", r);
        let r = strings(&mut c, "SELECT name FROM bulky WHERE id = 1234").await;
        assert!(r[0][0].contains("n\u{e9}1234"), "{:?}", r);
        // a load with a duplicate key fails, and none of its rows is kept
        let mut req = c.bulk_insert("bulky").await.unwrap();
        for i in 5000..5100i32 {
            req.send((i, Some(format!("x{}", i)), 1.0f64).into_row()).await.unwrap();
        }
        req.send((17i32, Some("dup".to_string()), 1.0f64).into_row()).await.unwrap();
        assert!(req.finalize().await.is_err(), "a duplicate key must fail the load");
        let r = strings(&mut c, "SELECT COUNT(*) AS n FROM bulky").await;
        assert!(r[0][0].contains("3000"), "{:?}", r);
    });
    let mut total = 0;
    for s in &f.shards {
        let n = match s.server.connect().query("SELECT COUNT(*) FROM bulky").unwrap()[0][0].clone() {
            tsql_server::Value::Int(n) => n,
            other => panic!("{:?}", other),
        };
        assert!(n > 500, "a shard holds {} of 3000 rows", n);
        total += n;
    }
    assert_eq!(total, 3000);
    f.listener.shutdown();
}

#[test]
fn a_temporary_table_is_the_engines_own_until_a_statement_needs_it_with_the_clusters_tables() {
    // (SSMS's scripts make temporary tables, fill them from the catalog, index them uniquely and read them with the catalog: none of that is the
    // cluster's. One that a statement needs together with a table of the cluster is handed to the cluster, rows and all.)
    let f = start(3);
    f.rt.block_on(async {
        let mut c = login(&f.listener).await;
        run(&mut c, "CREATE TABLE orders (id INT PRIMARY KEY, amount INT)").await;
        run(&mut c, "INSERT INTO orders VALUES (1, 10), (2, 20), (3, 30)").await;
        // a temporary table with a column the cluster could not shard on, and a unique index it could not check: the engine's own
        run(&mut c, "CREATE TABLE #dep (nm sysname COLLATE database_default NOT NULL, kind smallint NOT NULL, grp int NULL)").await;
        run(&mut c, "CREATE UNIQUE CLUSTERED INDEX i1 ON #dep (nm, kind) WITH IGNORE_DUP_KEY").await;
        run(&mut c, "INSERT #dep SELECT name, 3, object_id FROM sys.tables WHERE name = 'orders' UNION ALL SELECT 'x', 4, 7").await;
        let r = strings(&mut c, "SELECT d.nm, d.kind FROM #dep d JOIN sys.tables t ON t.name = d.nm ORDER BY 1").await;
        assert!(r.len() == 1 && r[0][0].contains("orders"), "{:?}", r);
        // the control node does not know it as a table of the cluster
        assert!(!f.cluster.tables().iter().any(|t| t.name.last().is_some_and(|n| n == "#dep")));
        // a statement that needs it with the cluster's tables: it is handed over, with its rows
        let r = strings(&mut c, "SELECT o.id, d.kind FROM orders o JOIN #dep d ON d.grp = o.id + 4 ORDER BY o.id").await;
        assert!(r.len() == 1 && r[0][0].contains("3") && r[0][1].contains("4"), "{:?}", r);
        assert!(f.cluster.tables().iter().any(|t| t.name.last().is_some_and(|n| n == "#dep")));
        // and is the cluster's from then on
        run(&mut c, "INSERT #dep VALUES ('y', 5, 1)").await;
        let r = strings(&mut c, "SELECT COUNT(*) FROM #dep").await;
        assert!(r[0][0].contains("3"), "{:?}", r);
        run(&mut c, "DROP TABLE #dep").await;
    });
    // the session's temporary tables are not left behind when it ends
    drop(f.rt);
}

#[test]
fn a_client_makes_a_database_uses_it_and_the_catalog_shows_it() {
    let f = start(3);
    f.rt.block_on(async {
        let mut c = login(&f.listener).await;
        run(&mut c, "CREATE DATABASE shop").await;
        run(&mut c, "USE shop").await;
        run(&mut c, "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(20), qty INT)").await;
        let values: Vec<String> = (1..=40).map(|i| format!("({}, 'i{}', {})", i, i, i % 5)).collect();
        run(&mut c, &format!("INSERT INTO items VALUES {}", values.join(", "))).await;
        // the connection is in the database, the rows are the database's, and the engine's catalog knows both
        assert!(strings(&mut c, "SELECT DB_NAME()").await[0][0].contains("shop"));
        assert!(strings(&mut c, "SELECT COUNT(*) FROM items").await[0][0].contains("40"));
        assert!(strings(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE name = 'items'").await[0][0].contains('1'));
        assert!(strings(&mut c, "SELECT COUNT(*) FROM sys.databases WHERE name = 'shop'").await[0][0].contains('1'));
        // parameters name the same table
        let rows = c.query("SELECT name FROM items WHERE id = @P1", &[&7i32]).await.unwrap().into_first_result().await.unwrap();
        assert_eq!(rows[0].get::<&str, _>("name"), Some("i7"));
        // master has no such table, but the database's can be named from it
        run(&mut c, "USE master").await;
        assert!(c.simple_query("SELECT COUNT(*) FROM items").await.is_err());
        assert!(strings(&mut c, "SELECT COUNT(*) FROM shop.dbo.items").await[0][0].contains("40"));
        assert!(strings(&mut c, "SELECT COUNT(*) FROM sys.tables WHERE name = 'items'").await[0][0].contains('0'));
        // and the database goes with its table
        run(&mut c, "DROP DATABASE shop").await;
        assert!(strings(&mut c, "SELECT COUNT(*) FROM sys.databases WHERE name = 'shop'").await[0][0].contains('0'));
        let after = c.simple_query("SELECT COUNT(*) FROM shop.dbo.items").await;
        let after = match after {
            Ok(st) => st.into_first_result().await.map(|r| format!("{:?}", r.iter().map(|x| x.cells().map(|(_, v)| format!("{:?}", v)).collect::<Vec<_>>()).collect::<Vec<_>>())),
            Err(e) => Err(e),
        };
        assert!(after.is_err(), "the dropped database's table is still readable: {:?}", after);
    });
}

#[test]
fn a_system_procedure_given_the_name_of_a_table_is_the_engines_not_the_clusters() {
    // SSMS runs `EXEC sp_fkeys null, null, null, N'table', N'schema', N'db'` for a table's relationships: the name is a value
    let f = start(3);
    f.rt.block_on(async {
        let mut c = login(&f.listener).await;
        run(&mut c, "CREATE TABLE people (id INT PRIMARY KEY, name VARCHAR(20))").await;
        run(&mut c, "CREATE TABLE pets (id INT PRIMARY KEY, owner INT REFERENCES people (id))").await;
        let r = c.simple_query("EXEC sp_fkeys null, null, null, N'people', N'dbo', N'master'").await;
        assert!(r.is_ok(), "sp_fkeys: {:?}", r.err());
        let rows = r.unwrap().into_first_result().await;
        assert!(rows.is_ok(), "sp_fkeys result: {:?}", rows.err());
        let r = c.simple_query("EXEC sp_columns N'people'").await.unwrap().into_first_result().await.unwrap();
        assert_eq!(r.len(), 2);
    });
}
