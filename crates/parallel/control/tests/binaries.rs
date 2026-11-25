//! The `tsql-cluster` binary: it makes certificates, insists on them, and serves other machines only over TLS.
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tsql_server::Server;
use tsql_shared_ces::tls::NodeTls;
use tsql_storage::{spawn, ShardNode};

/// Whether a TDS client asking for `level` can log in as `sa` on `port` and run `sql`.
fn logs_in(rt: &tokio::runtime::Runtime, port: u16, level: EncryptionLevel, sql: &str) -> bool {
    rt.block_on(async {
        let mut cfg = Config::new();
        cfg.host("127.0.0.1");
        cfg.port(port);
        cfg.authentication(AuthMethod::sql_server("sa", "Str0ng!pw"));
        cfg.encryption(level);
        cfg.trust_cert(); // the certificate is self-signed
        let tcp = TcpStream::connect(cfg.get_addr()).await.unwrap();
        let Ok(mut client) = Client::connect(cfg, tcp.compat_write()).await else { return false };
        let Ok(stream) = client.simple_query(sql).await else { return false };
        stream.into_results().await.is_ok()
    })
}

#[test]
fn tsql_cluster_makes_certificates_insists_on_them_and_protects_its_clients() {
    let dir = tempfile::tempdir().unwrap();
    let command = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tsql-cluster"));
        cmd.current_dir(dir.path()).args(args).env_remove("TSQL_SA_PASSWORD").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd
    };
    let run = |args: &[&str], script: &str| {
        let mut child = command(args).spawn().unwrap();
        // a program that refuses to start may have closed its input already
        let _ = child.stdin.take().unwrap().write_all(script.as_bytes());
        let out = child.wait_with_output().unwrap();
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.success(), text(&out.stdout), text(&out.stderr))
    };
    let (made, _, why) = run(&["--make-certs", "certs", "control", "shard"], "");
    assert!(made, "{why}");
    let certs = dir.path().join("certs");
    let node = NodeTls::from_files(certs.join("ca.pem"), certs.join("shard.pem"), certs.join("shard.key")).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let shard = rt.block_on(spawn(ShardNode::new(0, Server::new()).with_tls(Some(node)), "127.0.0.1:0".parse().unwrap())).unwrap();
    std::fs::write(dir.path().join("cluster.json"), format!("{{\"shards\":[{{\"id\":0,\"addr\":\"{}\"}}]}}", shard.addr)).unwrap();

    let secured = ["--config", "cluster.json", "--node-ca", "certs/ca.pem", "--node-cert", "certs/control.pem", "--node-key", "certs/control.key"];
    let with = |more: &[&'static str]| secured.iter().copied().chain(more.iter().copied()).collect::<Vec<_>>();
    // other machines are served only when every session is encrypted
    let open = ["--listen", "0", "--bind", "203.0.113.1"];
    let (ok, _, why) = run(&with(&open), "");
    assert!(!ok && why.contains("--insecure-clients") && !why.contains("cannot listen"), "{why}");
    let (ok, _, why) = run(&with(&[&open[..], &["--insecure-clients"]].concat()), "");
    assert!(!ok && why.contains("WARNING") && why.contains("cannot listen"), "{why}");
    let (ok, _, why) = run(&with(&[&open[..], &["--force-encryption"]].concat()), "");
    assert!(!ok && !why.contains("--insecure-clients") && why.contains("cannot listen"), "{why}");

    // a listening control node reaches its shard, offers TLS to clients, and with --force-encryption demands it
    for forced in [false, true] {
        let mut flags = with(&["--listen", "0", "--sa-password", "Str0ng!pw"]);
        flags.extend(forced.then_some("--force-encryption"));
        let mut server = command(&flags).spawn().unwrap();
        let mut said = vec![];
        for line in BufReader::new(server.stderr.take().unwrap()).lines().map(Result::unwrap) {
            said.push(line);
            if said.last().unwrap().contains("listening on") {
                break;
            }
        }
        let line = said.last().cloned().unwrap_or_default();
        assert!(line.contains("TLS, self-signed certificate") && !said.concat().contains("WARNING"), "{said:?}");
        let port: u16 = line.split("127.0.0.1:").nth(1).and_then(|s| s.split(' ').next()).unwrap().parse().unwrap();
        let table = format!("CREATE TABLE t{0} (a INT PRIMARY KEY); INSERT INTO t{0} VALUES (1), (2); SELECT SUM(a) FROM t{0}", u8::from(forced));
        let encrypted = logs_in(&rt, port, EncryptionLevel::Required, &table);
        let plain = logs_in(&rt, port, EncryptionLevel::NotSupported, "SELECT 1");
        server.kill().unwrap();
        server.wait().unwrap();
        assert!(encrypted && plain != forced, "forced {forced}: encrypted {encrypted}, plain {plain}");
    }

    let (ok, _, why) = run(&["--config", "cluster.json"], "");
    assert!(!ok && why.contains("--insecure"), "{why}");
    let (ok, _, why) = run(&["--config", "cluster.json", "--insecure", "--listen", "0", "--wait-shards", "1"], "");
    assert!(!ok && why.contains("WARNING") && why.contains("do not answer"), "{why}");
}
