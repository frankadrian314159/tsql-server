//! TLS on the TDS listener, with tiberius (which speaks TLS through its own rustls) as the independent client.
mod common;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tsql_server::tds::{Config as ListenConfig, Encryption, Listener, ListenerTls, TlsConfig};

type C = Client<Compat<TcpStream>>;

fn start(mode: Encryption) -> Listener {
    let server = common::server();
    server.connect().execute("ALTER LOGIN sa WITH PASSWORD = N'Str0ng!pw'");
    let tls = TlsConfig::self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    Listener::bind(server, "127.0.0.1:0", ListenConfig::with_tls(tls, mode)).unwrap()
}

async fn login(l: &Listener, level: EncryptionLevel, pw: &str) -> tiberius::Result<C> {
    let mut cfg = Config::new();
    cfg.host("127.0.0.1");
    cfg.port(l.local_addr().port());
    cfg.authentication(AuthMethod::sql_server("sa", pw));
    cfg.encryption(level);
    cfg.trust_cert(); // the certificate is self-signed
    let tcp = TcpStream::connect(cfg.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(cfg, tcp.compat_write()).await
}

async fn two(c: &mut C) -> i32 {
    let rows = c.simple_query("SELECT 1 + 1 AS two").await.unwrap().into_first_result().await.unwrap();
    rows[0].get::<i32, _>("two").unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_session_can_be_encrypted_from_end_to_end() {
    let l = start(Encryption::Optional);
    let mut c = login(&l, EncryptionLevel::Required, "Str0ng!pw").await.expect("TLS login");
    assert_eq!(two(&mut c).await, 2);
    // a big result goes through many TLS records and TDS packets
    let rows = c.simple_query("SELECT REPLICATE(CAST('x' AS varchar(max)), 300000) AS big").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<&str, _>("big").unwrap().len(), 300000);
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn encrypting_only_the_login_leaves_a_working_plain_session() {
    let l = start(Encryption::Optional);
    let mut c = login(&l, EncryptionLevel::Off, "Str0ng!pw").await.expect("login-only TLS");
    assert_eq!(two(&mut c).await, 2);
    assert_eq!(two(&mut c).await, 2);
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_without_tls_is_served_when_encryption_is_optional() {
    let l = start(Encryption::Optional);
    let mut c = login(&l, EncryptionLevel::NotSupported, "Str0ng!pw").await.expect("plain login");
    assert_eq!(two(&mut c).await, 2);
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_without_tls_is_refused_when_encryption_is_required() {
    let l = start(Encryption::Required);
    assert!(login(&l, EncryptionLevel::NotSupported, "Str0ng!pw").await.is_err());
    let mut c = login(&l, EncryptionLevel::Off, "Str0ng!pw").await.expect("an unspecific client is upgraded to full encryption");
    assert_eq!(two(&mut c).await, 2);
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_wrong_password_is_still_refused_over_tls() {
    let l = start(Encryption::Optional);
    let e = login(&l, EncryptionLevel::Required, "nope").await.err().expect("must fail");
    assert!(e.to_string().contains("18456") || e.to_string().to_lowercase().contains("login failed"), "{}", e);
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn many_encrypted_sessions_at_once() {
    let l = start(Encryption::Required);
    let port = l.local_addr().port();
    let local = tokio::task::LocalSet::new();
    let mut tasks = vec![];
    for i in 0..8 {
        tasks.push(local.spawn_local(async move {
            let mut cfg = Config::new();
            cfg.host("127.0.0.1");
            cfg.port(port);
            cfg.authentication(AuthMethod::sql_server("sa", "Str0ng!pw"));
            cfg.encryption(EncryptionLevel::Required);
            cfg.trust_cert();
            let tcp = TcpStream::connect(cfg.get_addr()).await.unwrap();
            let mut c = Client::connect(cfg, tcp.compat_write()).await.unwrap();
            let rows = c.simple_query(format!("SELECT {} AS n", i)).await.unwrap().into_first_result().await.unwrap();
            assert_eq!(rows[0].get::<i32, _>("n"), Some(i));
        }));
    }
    local
        .run_until(async {
            for t in tasks {
                t.await.unwrap();
            }
        })
        .await;
    l.shutdown();
}

#[test]
fn certificates_can_be_loaded_from_pem() {
    let (cert, key) = TlsConfig::generate_pem(vec!["localhost".into()]).unwrap();
    TlsConfig::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
    assert!(TlsConfig::from_pem(b"not a certificate", key.as_bytes()).is_err());
    assert!(TlsConfig::from_pem(cert.as_bytes(), b"not a key").is_err());
    // and from files
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("c.pem"), &cert).unwrap();
    std::fs::write(dir.path().join("k.pem"), &key).unwrap();
    TlsConfig::from_pem_files(dir.path().join("c.pem"), dir.path().join("k.pem")).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn tls13_can_be_offered_alongside_tls12() {
    let server = common::server();
    server.connect().execute("ALTER LOGIN sa WITH PASSWORD = N'Str0ng!pw'");
    let tls = TlsConfig::self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap().tls13(true).unwrap();
    let l = Listener::bind(server, "127.0.0.1:0", ListenConfig::with_tls(tls, Encryption::Optional)).unwrap();
    // tiberius' rustls offers 1.3 and gets it; the session works in full and for the login only
    let mut c = login(&l, EncryptionLevel::Required, "Str0ng!pw").await.expect("TLS 1.3 login");
    assert_eq!(two(&mut c).await, 2);
    let rows = c.simple_query("SELECT REPLICATE(CAST('x' AS varchar(max)), 200000) AS big").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<&str, _>("big").unwrap().len(), 200000);
    let mut c = login(&l, EncryptionLevel::Off, "Str0ng!pw").await.expect("TLS 1.3 login only");
    assert_eq!(two(&mut c).await, 2);
    l.shutdown();
}

#[test]
fn beyond_this_machine_every_session_must_be_encrypted() {
    let flags = |given: &str| {
        let (mut flags, mut args) = (ListenerTls::default(), given.split_whitespace().map(String::from));
        while let Some(a) = args.next() {
            assert!(flags.take(&a, &mut args), "{a}");
        }
        flags
    };
    for local in ["127.0.0.1", "localhost", "::1"] {
        assert!(flags("").check_exposure(local).is_ok(), "{local}");
    }
    for open in ["0.0.0.0", "::", "203.0.113.1", "no such host"] {
        assert!(flags("").check_exposure(open).is_err(), "{open}");
        assert!(flags("--force-encryption").check_exposure(open).is_ok(), "{open}");
        assert!(flags("--force-encryption --no-tls").check_exposure(open).is_err(), "{open}");
    }
    let config = |given: &str| flags(given).config("127.0.0.1").map(|(c, _)| (c.tls.is_some(), c.encryption));
    assert_eq!(config(""), Ok((true, Encryption::Optional)));
    assert_eq!(config("--force-encryption --tls13"), Ok((true, Encryption::Required)));
    assert_eq!(config("--no-tls").map(|c| c.0), Ok(false));
    assert!(config("--no-tls --force-encryption").unwrap_err().contains("needs TLS"));
    assert!(config("--tls-cert server.pem").unwrap_err().contains("go together"));
    assert!(config("--tls-cert none.pem --tls-key none.key").unwrap_err().contains("cannot set up TLS"));
}
