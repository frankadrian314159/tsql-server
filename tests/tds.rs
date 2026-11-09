//! The TDS listener, exercised through tiberius, an independent TDS client implementation.
mod common;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
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

async fn login(l: &Listener, user: &str, pw: &str, db: Option<&str>) -> tiberius::Result<C> {
    let mut cfg = Config::new();
    cfg.host("127.0.0.1");
    cfg.port(l.local_addr().port());
    cfg.authentication(AuthMethod::sql_server(user, pw));
    cfg.encryption(EncryptionLevel::NotSupported);
    if let Some(d) = db {
        cfg.database(d);
    }
    let tcp = TcpStream::connect(cfg.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(cfg, tcp.compat_write()).await
}

/// Run `sql` and return the server error it produces (tiberius reports it either from the call or from reading results).
async fn fails(c: &mut C, sql: &str) -> tiberius::error::Error {
    match c.simple_query(sql).await {
        Err(e) => e,
        Ok(s) => s.into_results().await.err().expect("expected an error"),
    }
}

async fn sa(l: &Listener) -> C {
    login(l, "sa", "Str0ng!pw", None).await.expect("login")
}

#[tokio::test(flavor = "current_thread")]
async fn login_and_simple_query() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    let rows = c.simple_query("SELECT 1 + 1 AS two, N'héllo ☃' AS s, CAST(NULL AS int) AS n").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<i32, _>("two"), Some(2));
    assert_eq!(rows[0].get::<&str, _>("s"), Some("héllo ☃"));
    assert_eq!(rows[0].get::<i32, _>("n"), None);
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn bad_password_and_unknown_login_are_refused() {
    let (l, _s) = start();
    let e = login(&l, "sa", "wrong", None).await.err().expect("must fail");
    assert!(e.to_string().contains("18456") || e.to_string().to_lowercase().contains("login failed"), "{}", e);
    assert!(login(&l, "nobody", "x", None).await.is_err());
    // a login without a password cannot be used over the network
    let _ = _s.connect().execute("CREATE LOGIN nopw WITH PASSWORD = 'abc'");
    assert!(login(&l, "nopw", "wrong", None).await.is_err());
    assert!(login(&l, "nopw", "abc", None).await.is_ok());
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn default_database_at_login() {
    let (l, s) = start();
    s.connect().execute("CREATE DATABASE shop");
    let mut c = login(&l, "sa", "Str0ng!pw", Some("shop")).await.unwrap();
    let r = c.simple_query("SELECT DB_NAME()").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(r[0].get::<&str, _>(0), Some("shop"));
    assert!(login(&l, "sa", "Str0ng!pw", Some("missing")).await.is_err());
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn data_types_round_trip() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    let sql = "SELECT CAST(1 AS bit) b, CAST(200 AS tinyint) t, CAST(-5 AS smallint) sm, CAST(70000 AS int) i, CAST(5000000000 AS bigint) bi, \
               CAST(1.5 AS float) f, CAST(2.5 AS real) r, CAST('abc' AS varchar(10)) vc, CAST('ab' AS char(4)) ch, N'xyz' nv, \
               CAST(0x0102 AS varbinary(10)) vb, CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) g, \
               CAST('2024-02-29' AS date) d, CAST('2024-02-29 13:14:15.123' AS datetime) dt, CAST('2024-02-29 13:14:15.1234567' AS datetime2) dt2";
    let rows = c.simple_query(sql).await.unwrap().into_first_result().await.unwrap();
    let r = &rows[0];
    assert_eq!(r.get::<bool, _>("b"), Some(true));
    assert_eq!(r.get::<u8, _>("t"), Some(200));
    assert_eq!(r.get::<i16, _>("sm"), Some(-5));
    assert_eq!(r.get::<i32, _>("i"), Some(70000));
    assert_eq!(r.get::<i64, _>("bi"), Some(5_000_000_000));
    assert_eq!(r.get::<f64, _>("f"), Some(1.5));
    assert_eq!(r.get::<f32, _>("r"), Some(2.5));
    assert_eq!(r.get::<&str, _>("vc"), Some("abc"));
    assert_eq!(r.get::<&str, _>("nv"), Some("xyz"));
    assert_eq!(r.get::<&[u8], _>("vb"), Some(&[1u8, 2][..]));
    assert_eq!(r.get::<tiberius::Uuid, _>("g").map(|g| g.to_string().to_uppercase()), Some("6F9619FF-8B86-D011-B42D-00C04FC964FF".to_string()));
    use tiberius::time::chrono::{NaiveDate, NaiveDateTime};
    assert_eq!(r.get::<NaiveDate, _>("d"), NaiveDate::from_ymd_opt(2024, 2, 29));
    // datetime counts 1/300 seconds, so .123 comes back as the nearest tick, .123333
    assert_eq!(r.get::<NaiveDateTime, _>("dt"), NaiveDate::from_ymd_opt(2024, 2, 29).unwrap().and_hms_nano_opt(13, 14, 15, 123_333_333));
    assert_eq!(r.get::<NaiveDateTime, _>("dt2"), NaiveDate::from_ymd_opt(2024, 2, 29).unwrap().and_hms_nano_opt(13, 14, 15, 123_456_700));
    // fixed-length columns are wide enough for computed strings that are longer than the declared type guess
    let rows = c.simple_query("SELECT REPLICATE('x', 500) + 'y' AS s").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<&str, _>(0).map(|s| s.len()), Some(501));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn parameters_use_sp_executesql() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE t (id int PRIMARY KEY, name nvarchar(50), score float, ok bit, born date)").await.unwrap().into_results().await.unwrap();
    use tiberius::time::chrono::NaiveDate;
    let born = NaiveDate::from_ymd_opt(2000, 1, 2).unwrap();
    let res = c.execute("INSERT INTO t VALUES (@P1, @P2, @P3, @P4, @P5)", &[&1i32, &"O'Brien", &2.5f64, &true, &born]).await.unwrap();
    assert_eq!(res.total(), 1);
    c.execute("INSERT INTO t VALUES (@P1, @P2, NULL, NULL, NULL)", &[&2i32, &Option::<&str>::None]).await.unwrap();
    let rows = c.query("SELECT name, score, ok, born FROM t WHERE id = @P1", &[&1i32]).await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<&str, _>(0), Some("O'Brien"));
    assert_eq!(rows[0].get::<f64, _>(1), Some(2.5));
    assert_eq!(rows[0].get::<bool, _>(2), Some(true));
    assert_eq!(rows[0].get::<NaiveDate, _>(3), Some(born));
    let n = c.query("SELECT COUNT(*) FROM t WHERE name IS NULL", &[]).await.unwrap().into_first_result().await.unwrap();
    assert_eq!(n[0].get::<i32, _>(0), Some(1));
    // an update reports its row count
    let res = c.execute("UPDATE t SET score = @P1", &[&9.0f64]).await.unwrap();
    assert_eq!(res.total(), 2);
    // string parameters keep their quotes and unicode
    let rows = c.query("SELECT @P1", &[&"a'b ☃"]).await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<&str, _>(0), Some("a'b ☃"));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn errors_messages_and_multiple_result_sets() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    let e = fails(&mut c, "SELECT * FROM nosuch").await;
    match e {
        tiberius::error::Error::Server(te) => {
            assert_eq!(te.code(), 208);
            assert!(te.message().contains("nosuch"));
        }
        other => panic!("{:?}", other),
    }
    // the connection is still usable after an error
    let r = c.simple_query("SELECT 1 AS a; SELECT 2 AS b UNION ALL SELECT 3").await.unwrap().into_results().await.unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[1].len(), 2);
    // PRINT does not break the stream
    let r = c.simple_query("PRINT 'hello'; SELECT 7").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(r[0].get::<i32, _>(0), Some(7));
    // an error inside a procedure call is reported with its number
    c.simple_query("CREATE PROCEDURE boom AS RAISERROR('bad thing', 16, 1)").await.unwrap().into_results().await.unwrap();
    let e = c.execute("EXEC boom", &[]).await.err().expect("error");
    assert!(e.to_string().contains("bad thing"), "{}", e);
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn transactions_and_isolation_between_connections() {
    let (l, _s) = start();
    let mut a = sa(&l).await;
    let mut b = sa(&l).await;
    a.simple_query("CREATE TABLE acct (id int PRIMARY KEY, bal int); INSERT acct VALUES (1, 100)").await.unwrap().into_results().await.unwrap();
    a.simple_query("BEGIN TRAN; UPDATE acct SET bal = 50").await.unwrap().into_results().await.unwrap();
    // read uncommitted sees the change, read committed would block; snapshot-style readers do not
    b.simple_query("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED").await.unwrap().into_results().await.unwrap();
    let r = b.simple_query("SELECT bal FROM acct").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(r[0].get::<i32, _>(0), Some(50));
    a.simple_query("ROLLBACK").await.unwrap().into_results().await.unwrap();
    let r = b.simple_query("SELECT bal FROM acct").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(r[0].get::<i32, _>(0), Some(100));
    // @@TRANCOUNT follows the connection's own transaction
    let r = a.simple_query("BEGIN TRAN; SELECT @@TRANCOUNT").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(r[0].get::<i32, _>(0), Some(1));
    a.simple_query("COMMIT").await.unwrap().into_results().await.unwrap();
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn large_results_span_many_packets() {
    let (l, _s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE big (id int, s nvarchar(max))").await.unwrap().into_results().await.unwrap();
    c.simple_query("DECLARE @i int = 0; WHILE @i < 2000 BEGIN INSERT big VALUES (@i, REPLICATE(N'é', 3000)); SET @i += 1; END").await.unwrap().into_results().await.unwrap();
    let rows = c.simple_query("SELECT id, s FROM big ORDER BY id").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows.len(), 2000);
    assert_eq!(rows[1999].get::<i32, _>(0), Some(1999));
    assert_eq!(rows[1999].get::<&str, _>(1).map(|s| s.chars().count()), Some(3000));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn permissions_apply_to_network_logins() {
    let (l, s) = start();
    s.connect().execute("CREATE LOGIN ann WITH PASSWORD = 'pw1'; CREATE TABLE secret (x int); INSERT secret VALUES (1); CREATE USER ann FOR LOGIN ann");
    let mut c = login(&l, "ann", "pw1", None).await.unwrap();
    let e = fails(&mut c, "SELECT * FROM secret").await;
    match e {
        tiberius::error::Error::Server(te) => assert_eq!(te.code(), 229),
        other => panic!("{:?}", other),
    }
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn many_clients_at_once() {
    let (l, _s) = start();
    let mut first = sa(&l).await;
    first.simple_query("CREATE TABLE hits (n int)").await.unwrap().into_results().await.unwrap();
    let local = tokio::task::LocalSet::new();
    let mut handles = vec![];
    let port = l.local_addr().port();
    for i in 0..8 {
        handles.push(local.spawn_local(async move {
            let mut cfg = Config::new();
            cfg.host("127.0.0.1");
            cfg.port(port);
            cfg.authentication(AuthMethod::sql_server("sa", "Str0ng!pw"));
            cfg.encryption(EncryptionLevel::NotSupported);
            let tcp = TcpStream::connect(cfg.get_addr()).await.unwrap();
            let mut c = Client::connect(cfg, tcp.compat_write()).await.unwrap();
            for _ in 0..5 {
                c.execute("INSERT hits VALUES (@P1)", &[&(i as i32)]).await.unwrap();
            }
        }));
    }
    local
        .run_until(async {
            for h in handles {
                h.await.unwrap();
            }
        })
        .await;
    let r = first.simple_query("SELECT COUNT(*) FROM hits").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(r[0].get::<i32, _>(0), Some(40));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn date_and_time_parameters_keep_their_precision() {
    use tiberius::time::chrono::{NaiveDate, NaiveDateTime, NaiveTime};
    let (l, _s) = start();
    let mut c = sa(&l).await;
    let dt: NaiveDateTime = NaiveDate::from_ymd_opt(2024, 3, 4).unwrap().and_hms_nano_opt(5, 6, 7, 123_456_700).unwrap();
    let t = NaiveTime::from_hms_nano_opt(23, 59, 58, 987_654_300).unwrap();
    let rows = c.query("SELECT @P1, @P2, @P3", &[&dt, &t, &NaiveDate::from_ymd_opt(1999, 12, 31).unwrap()]).await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<NaiveDateTime, _>(0), Some(dt));
    assert_eq!(rows[0].get::<NaiveTime, _>(1), Some(t));
    assert_eq!(rows[0].get::<NaiveDate, _>(2), NaiveDate::from_ymd_opt(1999, 12, 31));
    l.shutdown();
}

// ------------------------------------------------------------------------------------------------ streamed replies
async fn streaming_fixture() -> (Listener, tsql_server::Server, C) {
    let (l, s) = start();
    let mut c = sa(&l).await;
    c.simple_query("CREATE TABLE sbig (id INT PRIMARY KEY, grp INT, name VARCHAR(30), note NVARCHAR(40) NULL)").await.unwrap().into_results().await.unwrap();
    let mut sql = String::new();
    for i in 0..5000 {
        sql.push_str(&format!("INSERT sbig VALUES ({}, {}, 'name{}', {});\n", i, (i * 7919) % 41, (i * 31) % 301, if i % 4 == 0 { "NULL".to_string() } else { format!("N'note ☃ {}'", i % 13) }));
    }
    c.simple_query(sql).await.unwrap().into_results().await.unwrap();
    c.simple_query("CREATE TABLE sdim (grp INT PRIMARY KEY, label VARCHAR(20))").await.unwrap().into_results().await.unwrap();
    let mut sql = String::new();
    for g in 0..41 {
        sql.push_str(&format!("INSERT sdim VALUES ({}, 'group {}');\n", g, g));
    }
    c.simple_query(sql).await.unwrap().into_results().await.unwrap();
    // every eligible query uses the streaming executor with a tiny budget, so its rows are sent as they are produced
    s.set_work_mem(4096, 0, None);
    (l, s, c)
}

#[tokio::test(flavor = "current_thread")]
async fn a_big_sorted_result_is_streamed_to_the_client() {
    let (l, _s, mut c) = streaming_fixture().await;
    let rows = c.simple_query("SELECT id, name, note FROM sbig ORDER BY grp, id").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows.len(), 5000);
    let mut last = (-1i32, -1i32);
    for r in &rows {
        let id = r.get::<i32, _>("id").unwrap();
        let grp = (id * 7919) % 41;
        assert!((grp, id) > last, "rows out of order at id {}", id);
        last = (grp, id);
        assert_eq!(r.get::<&str, _>("name"), Some(format!("name{}", (id * 31) % 301).as_str()));
        if id % 4 == 0 {
            assert_eq!(r.get::<&str, _>("note"), None);
        } else {
            assert_eq!(r.get::<&str, _>("note"), Some(format!("note ☃ {}", id % 13).as_str()));
        }
    }
    // the session carries on normally after a streamed reply
    let two = c.simple_query("SELECT 1 + 1 AS two").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(two[0].get::<i32, _>("two"), Some(2));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_streamed_join_with_messages_and_counts_around_it() {
    let (l, _s, mut c) = streaming_fixture().await;
    let results = c
        .simple_query("PRINT 'start'; SELECT b.id, d.label FROM sbig b JOIN sdim d ON b.grp = d.grp ORDER BY b.id; SELECT COUNT(*) AS n FROM sbig; PRINT 'done'")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].len(), 5000);
    assert_eq!(results[1][0].get::<i32, _>("n"), Some(5000));
    assert_eq!(results[0][17].get::<&str, _>("label"), Some(format!("group {}", (17 * 7919) % 41).as_str()));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn parameterised_queries_stream_too_and_keep_their_output_values() {
    let (l, _s, mut c) = streaming_fixture().await;
    let rows = c.query("SELECT id, grp FROM sbig WHERE grp >= @P1 ORDER BY id DESC", &[&30i32]).await.unwrap().into_first_result().await.unwrap();
    let expected = (0..5000).filter(|i| (i * 7919) % 41 >= 30).count();
    assert_eq!(rows.len(), expected);
    assert_eq!(rows[0].get::<i32, _>("id"), (0..5000).rev().find(|i| (i * 7919) % 41 >= 30));
    // an OUTPUT parameter and a return value after a streamed result set (sp_executesql with OUTPUT)
    let r = c
        .simple_query("DECLARE @n INT; EXEC sp_executesql N'SELECT id FROM sbig ORDER BY name, id; SELECT @cnt = COUNT(*) FROM sbig', N'@cnt INT OUTPUT', @cnt = @n OUTPUT; SELECT @n AS n")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    assert_eq!(r[0].len(), 5000);
    assert_eq!(r.last().unwrap()[0].get::<i32, _>("n"), Some(5000));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn columns_of_guessed_type_can_hold_any_text_when_streamed() {
    let (l, _s, mut c) = streaming_fixture().await;
    // the first 64 rows are ASCII, later ones are not representable in the code page: the column must not be varchar
    let rows = c.simple_query("SELECT id, CASE WHEN id < 200 THEN 'plain' ELSE N'snow ☃ ' + name END AS txt FROM sbig ORDER BY id").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows.len(), 5000);
    assert_eq!(rows[10].get::<&str, _>("txt"), Some("plain"));
    assert_eq!(rows[4999].get::<&str, _>("txt"), Some(format!("snow ☃ name{}", (4999 * 31) % 301).as_str()));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_that_disconnects_mid_result_does_not_hurt_the_server() {
    let (l, s, mut c) = streaming_fixture().await;
    {
        let mut other = sa(&l).await;
        // start a big result and drop the connection without reading it all
        let stream = other.simple_query("SELECT id, name, note FROM sbig ORDER BY name, id").await.unwrap();
        drop(stream);
        drop(other);
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let rows = c.simple_query("SELECT COUNT(*) AS n FROM sbig").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<i32, _>("n"), Some(5000));
    let _ = s;
    l.shutdown();
}
