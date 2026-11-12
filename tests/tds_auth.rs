//! Azure AD access-token login over TDS (the FEDAUTH feature of Login7), with tiberius as the client. The tokens are signed
//! here with a throw-away RSA key whose public half is the server's key set.
mod common;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tsql_server::tds::{AadConfig, Config as ListenConfig, JwksSource, Listener};

type C = Client<Compat<TcpStream>>;

/// A throw-away 2048-bit RSA key (PKCS#8, hex) that signs the test tokens, and its modulus.
const KEY_PKCS8_HEX: &str = "308204bd020100300d06092a864886f70d0101010500048204a7308204a30201000282010100a125d5669b8b6f0db07234f908bac14c8a748ccad641e40b22a80a07abfeeda902e149e06fd8b042ea906d9155ac7a8d23d473e3d63278ddbccdae5a4dd30e80746a5844cf1b8e5cefd356ca5ffec7c6f3c9a5e8dbc60bac87d0a67f9058790102022f074b9de03dce54da57cac489faa7ddb9cd087daad7a2f3009114c3a87333450dcf198724f000e7e05220e5257eb6e529ec5bc35284704f6448d08131a73a49480d2177a65b5f1d23b2357a514986dc825bd091457f80c9a64b38284b9cc3e633e14a6edebecf4a8ff8279aed722f88520ed176f3e78fb235118ee04d5ff6263241959426b1a06717caacdbaa9b5772eba0c4d1e242824cd35738b5984302030100010282010018e0ceb83f8d3580dc86552d43d9691c2d9d3405e086b2f3bbcef6201c6a87e5a41f2f18ea20d432bb2469a09aa467c06b1152843b13a1b0972efdc577e6fc81d24c7f57682343458fca30724a82fa9ea4110929bb9185f3307dd826861ca7d9ada60952a156e7414373101e50f1d4b210c107528db15b9db222127ffe14fd67ef6ff0be45f2da93b5dddfb7f8322c7113908fccd3723b1e218f28b86e52ba1d3e506bfd7b536c70ceebec80c0dd00d8037fe591d0dce9279477f3571619308568f7b6710e64c2c70e2cf7d15b09b1b872301fa11d173025bb2d12072a66e4f58b9aa0dfb1ccae251a2991fb01cb454865c4fa83253041bb558453fadd2052c102818100d4f7e163fced030ef22997f9c46022551524c90d0b91a157782fb148766f8badaee72e563327f4812a13e9b858535a0964b3db01a6fe071b5f12e37d6f869f143e76f0a417d826feda73159f1a2cd27ed37c91ec712330cf5db5b7236086f0eb359c25843ce08114ddddd9b6a873c39fd5bba99ed52b531baecb84ec48aacc2102818100c1b574dca24bccd0c2980d8bf0234bb2ab371ab52dc19b0912cf7568bd4f1462e01cb499c651b98e25d60df54f029afd53a4d5e61704c64b4b5d6a3eb4825e282edf5764d9b7041991e01f7efe1ef7432e9f5169f8d0fe705717c78bad45af0814d9b91506f2f46242fee0aa19620369ea844ce49158e8948c24f5120e77b7e302818100b8284d67a80bcf8c3a0d8b9478a264adcc25ce1110c2f2906fa65618020a3cef8881f248a9ddd0096d3860ef7eb93cef487c5a525873ebf358483e1d76b623935edc7d047875fa6a80bde9a17601f257a74b5de7e68176b0aa28fb5f8515caf8a99807bd0ce359db906b49db3059427d515600bc0f3b72974a9444b209b85fe10281801fad29b00ec630770eeb4883d2c022d7d3d1ce0744b53d420d96694fb86d5f1b1025018f87f3c3513074c19194db0b3753df47af39a2693ed4d330d59235236a94c7a705f1e850be51b181d5057f50d5e6a0dcf26462e0c8c4253a5767fd56330f1dcf7b55c893182276858cab5bf0ccf831ac55504366bd2bf1bb35b9b1f01b02818026632f798eb105169ef2a724dece9fdc38a7a8ada209214f57d263e90806d4e3d60281116ea807f2d9a1dd3f098496bccd72cf7ac18468e9c81df63468131a7eae7fba000bb83934a7f84aec8057e4d5374b9eaa29c2bc4d05f226e8165f4b9e22a665632ef7518eb84f3c9daf4c6a128e33f08308d0cdd84d1936bf1e5e566d";
const KEY_MODULUS_HEX: &str = "A125D5669B8B6F0DB07234F908BAC14C8A748CCAD641E40B22A80A07ABFEEDA902E149E06FD8B042EA906D9155AC7A8D23D473E3D63278DDBCCDAE5A4DD30E80746A5844CF1B8E5CEFD356CA5FFEC7C6F3C9A5E8DBC60BAC87D0A67F9058790102022F074B9DE03DCE54DA57CAC489FAA7DDB9CD087DAAD7A2F3009114C3A87333450DCF198724F000E7E05220E5257EB6E529EC5BC35284704F6448D08131A73A49480D2177A65B5F1D23B2357A514986DC825BD091457F80C9A64B38284B9CC3E633E14A6EDEBECF4A8FF8279AED722F88520ED176F3E78FB235118EE04D5FF6263241959426B1A06717CAACDBAA9B5772EBA0C4D1E242824CD35738B59843";

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn b64u(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn jwks() -> String {
    serde_json::json!({"keys": [{"kty": "RSA", "kid": "k1", "n": b64u(&hex(KEY_MODULUS_HEX)), "e": "AQAB"}]}).to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A token for `user`, signed by the test key, with claims that `tweak` may change.
fn token_with(user: &str, tweak: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut claims = serde_json::json!({
        "iss": "https://sts.windows.net/tenant-1/",
        "aud": "https://database.windows.net/",
        "exp": now() + 3600,
        "nbf": now() - 60,
        "upn": user,
    });
    tweak(&mut claims);
    let header = serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "k1"});
    let signed = format!("{}.{}", b64u(header.to_string().as_bytes()), b64u(claims.to_string().as_bytes()));
    let kp = ring::signature::RsaKeyPair::from_pkcs8(&hex(KEY_PKCS8_HEX)).unwrap();
    let mut sig = vec![0u8; kp.public().modulus_len()];
    kp.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signed.as_bytes(), &mut sig).unwrap();
    format!("{}.{}", signed, b64u(&sig))
}

fn token(user: &str) -> String {
    token_with(user, |_| {})
}

fn start() -> (Listener, tsql_server::Server) {
    let server = common::server();
    let mut c = server.connect();
    c.execute("ALTER LOGIN sa WITH PASSWORD = N'Str0ng!pw'");
    c.execute("CREATE LOGIN [alice@contoso.com] FROM EXTERNAL PROVIDER");
    c.execute("CREATE LOGIN plain_bob WITH PASSWORD = N'Str0ng!pw', CHECK_POLICY = OFF");
    let mut cfg = ListenConfig::default();
    cfg.auth.aad = Some(AadConfig { tenant: "tenant-1".into(), audiences: vec![], jwks: JwksSource::Inline(jwks()) });
    let l = Listener::bind(server.clone(), "127.0.0.1:0", cfg).unwrap();
    (l, server)
}

async fn connect(l: &Listener, auth: AuthMethod) -> tiberius::Result<C> {
    let mut cfg = Config::new();
    cfg.host("127.0.0.1");
    cfg.port(l.local_addr().port());
    cfg.authentication(auth);
    cfg.encryption(EncryptionLevel::NotSupported);
    let tcp = TcpStream::connect(cfg.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(cfg, tcp.compat_write()).await
}

async fn who(c: &mut C) -> String {
    let rows = c.simple_query("SELECT SUSER_SNAME() AS n").await.unwrap().into_first_result().await.unwrap();
    rows[0].get::<&str, _>("n").unwrap().to_string()
}

async fn refused(l: &Listener, tok: String) -> String {
    match connect(l, AuthMethod::aad_token(tok)).await {
        Ok(_) => panic!("the token must be refused"),
        Err(e) => e.to_string(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_azure_ad_token_logs_in_as_the_external_login() {
    let (l, _s) = start();
    let mut c = connect(&l, AuthMethod::aad_token(token("alice@contoso.com"))).await.expect("token login");
    assert_eq!(who(&mut c).await, "alice@contoso.com");
    // the login name is matched without regard to case
    let mut c = connect(&l, AuthMethod::aad_token(token("Alice@Contoso.com"))).await.expect("token login, other case");
    assert_eq!(who(&mut c).await, "alice@contoso.com");
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn tokens_that_are_not_good_are_refused() {
    let (l, _s) = start();
    // no login for this user
    let msg = refused(&l, token("mallory@contoso.com")).await;
    assert!(msg.to_lowercase().contains("login failed"), "{}", msg);
    // a SQL login cannot be entered with a token, even one that names it
    assert!(refused(&l, token("plain_bob")).await.to_lowercase().contains("login failed"));
    // expired, not yet valid, wrong audience, wrong tenant
    let expired = token_with("alice@contoso.com", |c| c["exp"] = serde_json::json!(now() - 4000));
    assert!(refused(&l, expired).await.contains("expired"));
    let early = token_with("alice@contoso.com", |c| c["nbf"] = serde_json::json!(now() + 4000));
    assert!(refused(&l, early).await.contains("not valid yet"));
    let aud = token_with("alice@contoso.com", |c| c["aud"] = serde_json::json!("https://other.example/"));
    assert!(refused(&l, aud).await.contains("not for this server"));
    let tenant = token_with("alice@contoso.com", |c| c["iss"] = serde_json::json!("https://sts.windows.net/another/"));
    assert!(refused(&l, tenant).await.contains("not by tenant"));
    // a changed body no longer matches the signature
    let good = token("alice@contoso.com");
    let parts: Vec<&str> = good.split('.').collect();
    let forged = format!("{}.{}.{}", parts[0], b64u(serde_json::json!({"iss": "https://sts.windows.net/tenant-1/", "aud": "https://database.windows.net/", "exp": now() + 3600, "upn": "alice@contoso.com", "x": 1}).to_string().as_bytes()), parts[2]);
    assert!(refused(&l, forged).await.contains("signature"));
    // garbage
    assert!(refused(&l, "not-a-token".into()).await.to_lowercase().contains("login failed"));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn sql_logins_still_work_and_external_logins_have_no_password() {
    let (l, _s) = start();
    let mut c = connect(&l, AuthMethod::sql_server("plain_bob", "Str0ng!pw")).await.expect("sql login");
    assert_eq!(who(&mut c).await, "plain_bob");
    assert!(connect(&l, AuthMethod::sql_server("alice@contoso.com", "anything")).await.is_err());
    // and the principal types are visible in the catalog
    let mut c = connect(&l, AuthMethod::sql_server("sa", "Str0ng!pw")).await.unwrap();
    let rows = c.simple_query("SELECT type FROM sys.server_principals WHERE name = N'alice@contoso.com'").await.unwrap().into_first_result().await.unwrap();
    assert_eq!(rows[0].get::<&str, _>(0), Some("E"));
    l.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn a_server_without_azure_ad_does_not_accept_tokens() {
    let server = common::server();
    server.connect().execute("CREATE LOGIN [alice@contoso.com] FROM EXTERNAL PROVIDER");
    let l = Listener::bind(server, "127.0.0.1:0", ListenConfig::default()).unwrap();
    let r = connect(&l, AuthMethod::aad_token(token("alice@contoso.com"))).await;
    assert!(r.is_err());
    l.shutdown();
}

#[test]
fn windows_logins_are_external_and_matched_by_any_of_their_names() {
    let server = common::server();
    let mut c = server.connect();
    c.execute("CREATE LOGIN [CORP\\dana] FROM WINDOWS");
    c.execute("CREATE LOGIN sqlbob WITH PASSWORD = N'Str0ng!pw', CHECK_POLICY = OFF");
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // the first name that is a login is used, without regard to case
    let mut d = server.connect_external(&names(&["dana@CORP.EXAMPLE", "corp\\DANA"])).expect("external login");
    assert_eq!(d.query("SELECT SUSER_SNAME()").unwrap()[0][0].to_string(), "CORP\\dana");
    // an unknown name, a SQL login and a disabled login are refused
    assert!(server.connect_external(&names(&["CORP\\eve"])).is_err());
    assert!(server.connect_external(&names(&["sqlbob"])).is_err());
    c.execute("ALTER LOGIN [CORP\\dana] DISABLE");
    assert!(server.connect_external(&names(&["CORP\\dana"])).is_err());
    // an external login has no password
    assert!(server.connect_as("CORP\\dana", "").is_err());
    assert!(server.connect_as("CORP\\dana", "anything").is_err());
    // the catalog shows the type
    let rows = server.connect().query("SELECT name, type FROM sys.server_principals WHERE name IN (N'CORP\\dana', N'sqlbob') ORDER BY name").unwrap();
    assert_eq!(rows[0][1].to_string(), "U");
    assert_eq!(rows[1][1].to_string(), "S");
}
