//! The `tsql-shard` binary does not serve unprotected connections unless told to.
use std::process::Command;

#[test]
fn tsql_shard_insists_on_certificates() {
    let refused = |flags: &[&str], why: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tsql-shard"));
        let out = cmd.args(["--id", "0", "--listen", "127.0.0.1:0"]).args(flags).output().unwrap();
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && said.contains(why), "{flags:?}: {said}");
    };
    refused(&[], "--insecure");
    refused(&["--node-ca", "ca.pem"], "go together");
    refused(&["--insecure", "--node-key", "node.key"], "cannot be combined");
    refused(&["--node-ca", "none.pem", "--node-cert", "none.pem", "--node-key", "none.key"], "cannot read none.pem");
}
