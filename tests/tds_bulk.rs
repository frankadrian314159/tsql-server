//! Bulk load over TDS (what `bcp`, `SqlBulkCopy` and tiberius' `bulk_insert` send), through tiberius.
mod common;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, IntoRow, IntoSql, TokenRow};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tsql_server::tds::{Config as ListenConfig, Listener};

type C = Client<Compat<TcpStream>>;

fn start() -> (Listener, tsql_server::Server) {
    let server = common::server();
    server.connect().execute("ALTER LOGIN sa WITH PASSWORD = N'Str0ng!pw'");
    let l = Listener::bind(server.clone(), "127.0.0.1:0", ListenConfig::default()).unwrap();
    (l, server)
}

async fn sa(l: &Listener) -> C {
    let mut cfg = Config::new();
    cfg.host("127.0.0.1");
    cfg.port(l.local_addr().port());
    cfg.authentication(AuthMethod::sql_server("sa", "Str0ng!pw"));
    cfg.encryption(EncryptionLevel::NotSupported);
    let tcp = TcpStream::connect(cfg.get_addr()).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    Client::connect(cfg, tcp.compat_write()).await.expect("login")
}

async fn scalar(c: &mut C, sql: &str) -> String {
    let rows = c.simple_query(sql).await.unwrap().into_first_result().await.unwrap();
    let r = &rows[0];
    if let Some(v) = r.try_get::<i32, _>(0).ok().flatten() {
        return v.to_string();
    }
    if let Some(v) = r.try_get::<i64, _>(0).ok().flatten() {
        return v.to_string();
    }
    if let Some(v) = r.try_get::<&str, _>(0).ok().flatten() {
        return v.to_string();
    }
    "NULL".to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn rows_are_bulk_loaded_with_their_types() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE bulk_t (id INT PRIMARY KEY, name NVARCHAR(40) NULL, amount FLOAT NULL, ok BIT NULL, big BIGINT NULL)").await.unwrap().into_results().await.unwrap();
    let mut req = c.bulk_insert("bulk_t").await.unwrap();
    for i in 0..20i32 {
        let name: Option<String> = if i % 5 == 0 { None } else { Some(format!("name \u{e9} {}", i)) };
        req.send((i, name, i as f64 * 1.5, i % 2 == 0, i as i64 * 1_000_000_000_000).into_row()).await.unwrap();
    }
    let res = req.finalize().await.unwrap();
    assert_eq!(res.total(), 20);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_t").await, "20");
    assert_eq!(scalar(&mut c, "SELECT name FROM bulk_t WHERE id = 7").await, "name \u{e9} 7");
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_t WHERE name IS NULL").await, "4");
    assert_eq!(scalar(&mut c, "SELECT big FROM bulk_t WHERE id = 19").await, "19000000000000");
    assert_eq!(scalar(&mut c, "SELECT CAST(SUM(amount) AS INT) FROM bulk_t").await, "285");
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_load_far_bigger_than_a_packet_arrives_whole() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE bulk_big (id INT PRIMARY KEY, pad VARCHAR(200), n INT)").await.unwrap().into_results().await.unwrap();
    let n = 30_000i32;
    let mut req = c.bulk_insert("bulk_big").await.unwrap();
    for i in 0..n {
        req.send((i, format!("{:0>120}", i), i % 97).into_row()).await.unwrap();
    }
    assert_eq!(req.finalize().await.unwrap().total(), n as u64);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_big").await, n.to_string());
    assert_eq!(scalar(&mut c, "SELECT pad FROM bulk_big WHERE id = 12345").await, format!("{:0>120}", 12345));
    assert_eq!(scalar(&mut c, "SELECT SUM(CAST(n AS BIGINT)) FROM bulk_big").await, (0..n).map(|i| (i % 97) as i64).sum::<i64>().to_string());
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_load_stores_nothing() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE bulk_pk (id INT PRIMARY KEY, v NVARCHAR(10) NOT NULL); INSERT bulk_pk VALUES (5, N'x')").await.unwrap().into_results().await.unwrap();
    // the key 5 exists: the load fails, and the rows before it are not kept
    let mut req = c.bulk_insert("bulk_pk").await.unwrap();
    for i in 1..=8i32 {
        req.send((i, "y").into_row()).await.unwrap();
    }
    let err = req.finalize().await.err().expect("a duplicate key must fail the load");
    assert!(err.to_string().contains("2627") || err.to_string().to_lowercase().contains("duplicate"), "{}", err);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_pk").await, "1");
    // a NULL in a NOT NULL column fails it too, and the connection stays usable
    let mut req = c.bulk_insert("bulk_pk").await.unwrap();
    req.send((100i32, Option::<&str>::None).into_row()).await.unwrap();
    assert!(req.finalize().await.is_err());
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_pk").await, "1");
    let mut req = c.bulk_insert("bulk_pk").await.unwrap();
    req.send((101i32, "ok").into_row()).await.unwrap();
    assert_eq!(req.finalize().await.unwrap().total(), 1);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_pk").await, "2");
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn identity_columns_get_their_own_values_and_defaults_apply() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE bulk_id (id INT IDENTITY(10, 10) PRIMARY KEY, v NVARCHAR(10), d INT DEFAULT 7)").await.unwrap().into_results().await.unwrap();
    // tiberius lists every updatable column, the identity column among them: its values are not stored
    let mut req = c.bulk_insert("bulk_id").await.unwrap();
    req.send((1i32, "a", Option::<i32>::None).into_row()).await.unwrap();
    req.send((1i32, "b", Some(3i32)).into_row()).await.unwrap();
    assert_eq!(req.finalize().await.unwrap().total(), 2);
    assert_eq!(scalar(&mut c, "SELECT id FROM bulk_id WHERE v = N'b'").await, "20");
    assert_eq!(scalar(&mut c, "SELECT d FROM bulk_id WHERE v = N'b'").await, "3");
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_load_inside_a_transaction_follows_the_transaction() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE bulk_tx (id INT PRIMARY KEY)").await.unwrap().into_results().await.unwrap();
    c.simple_query("BEGIN TRANSACTION").await.unwrap().into_results().await.unwrap();
    let mut req = c.bulk_insert("bulk_tx").await.unwrap();
    for i in 0..10i32 {
        let mut row = TokenRow::new();
        row.push(i.into_sql());
        req.send(row).await.unwrap();
    }
    assert_eq!(req.finalize().await.unwrap().total(), 10);
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_tx").await, "10");
    c.simple_query("ROLLBACK TRANSACTION").await.unwrap().into_results().await.unwrap();
    assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM bulk_tx").await, "0");
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn the_metadata_queries_of_bulk_copy_work() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE bulk_meta (id INT PRIMARY KEY, name NVARCHAR(40), n DECIMAL(10,2))").await.unwrap().into_results().await.unwrap();
    c.simple_query("INSERT bulk_meta VALUES (1, N'a', 1.5)").await.unwrap().into_results().await.unwrap();
    // SET FMTONLY ON describes a result and returns no rows, and does not run what is not a query
    let sets = c.simple_query("SET FMTONLY ON; SELECT * FROM bulk_meta; INSERT bulk_meta VALUES (2, N'b', 2.5); SET FMTONLY OFF; SELECT COUNT(*) FROM bulk_meta").await.unwrap().into_results().await.unwrap();
    assert_eq!(sets.len(), 2, "{} result sets", sets.len());
    assert_eq!(sets[0].len(), 0);
    assert_eq!(sets[1][0].get::<i32, _>(0), Some(1));
    // the collations of the columns
    let sets = c.simple_query("exec sp_tablecollations_100 N'[dbo].[bulk_meta]'").await.unwrap().into_results().await.unwrap();
    assert_eq!(sets[0].len(), 3);
    assert_eq!(sets[0][1].get::<&str, _>(1), Some("name"));
    assert!(sets[0][1].get::<&[u8], _>(2).is_some());
    assert!(sets[0][0].get::<&[u8], _>(2).is_none());
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn insert_bulk_for_a_table_that_is_not_there_is_an_error() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    let r = c.simple_query("INSERT BULK [dbo].[nothere] ([id] int)").await;
    let err = match r {
        Err(e) => e,
        Ok(s) => s.into_results().await.err().expect("must fail"),
    };
    assert!(err.to_string().contains("208") || err.to_string().contains("Invalid object name"), "{}", err);
    l.shutdown();
}
