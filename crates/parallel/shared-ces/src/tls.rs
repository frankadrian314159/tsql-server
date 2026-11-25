//! Mutual TLS between the nodes of a cluster. Every node holds a certificate signed by the cluster's own certificate
//! authority: a caller talks only to a node that has one, and a node answers only such a caller. The authority must sign
//! nothing but this cluster's nodes, because whoever holds one of its certificates can run any T-SQL on every shard, as `sa`.
use chrono::Datelike;
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use std::io::Write;
use std::path::Path;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig};

/// The name every node certificate carries. Callers check for it rather than for the host they connect to, so that a
/// certificate stays good when a node moves.
pub const NODE_NAME: &str = "tsql-node";
/// How long made certificates are good for.
const YEARS: i32 = 10;

/// What a node proves who it is with, and the authority it believes about others.
#[derive(Clone)]
pub struct NodeTls {
    authority: Certificate,
    identity: Identity,
}

/// (Never the key.)
impl std::fmt::Debug for NodeTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NodeTls")
    }
}

impl NodeTls {
    /// From PEM files: the cluster's certificate authority, and this node's certificate and key.
    pub fn from_files(authority: impl AsRef<Path>, cert: impl AsRef<Path>, key: impl AsRef<Path>) -> Result<NodeTls, String> {
        let read = |p: &Path| std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display()));
        let tls = NodeTls {
            authority: Certificate::from_pem(read(authority.as_ref())?),
            identity: Identity::from_pem(read(cert.as_ref())?, read(key.as_ref())?),
        };
        // find a bad certificate or key now, not at the first connection
        Endpoint::from_static("https://localhost").tls_config(tls.client()).map_err(|e| format!("bad certificate or key: {e:?}"))?;
        Ok(tls)
    }

    /// What a listening node requires: it shows its certificate and refuses callers without one of the authority's.
    pub fn server(&self) -> ServerTlsConfig {
        ServerTlsConfig::new().identity(self.identity.clone()).client_ca_root(self.authority.clone())
    }

    fn client(&self) -> ClientTlsConfig {
        ClientTlsConfig::new().ca_certificate(self.authority.clone()).identity(self.identity.clone()).domain_name(NODE_NAME)
    }
}

/// How to reach the node at `addr`: with mutual TLS when there is `tls`, otherwise unprotected. The caller sets the rest.
pub fn endpoint(addr: &str, tls: Option<&NodeTls>) -> Result<Endpoint, String> {
    let scheme = if tls.is_some() { "https" } else { "http" };
    let bad = |e: tonic::transport::Error| format!("bad node address {addr}: {e}");
    let mut endpoint = Endpoint::from_shared(format!("{scheme}://{addr}")).map_err(bad)?;
    if let Some(tls) = tls {
        endpoint = endpoint.tls_config(tls.client()).map_err(bad)?;
    }
    Ok(endpoint)
}

/// The `--node-ca`, `--node-cert`, `--node-key` and `--insecure` flags of `tsql-shard` and `tsql-cluster`.
#[derive(Default)]
pub struct TlsFlags {
    authority: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    insecure: bool,
}

impl TlsFlags {
    /// Take `flag`, with its value from `args`, if it is one of these.
    pub fn take(&mut self, flag: &str, args: &mut impl Iterator<Item = String>) -> bool {
        match flag {
            "--node-ca" => self.authority = args.next(),
            "--node-cert" => self.cert = args.next(),
            "--node-key" => self.key = args.next(),
            "--insecure" => self.insecure = true,
            _ => return false,
        }
        true
    }

    /// The certificates to use: `None` only when `--insecure` asks for unprotected connections.
    pub fn load(&self) -> Result<Option<NodeTls>, String> {
        match (&self.authority, &self.cert, &self.key, self.insecure) {
            (Some(authority), Some(cert), Some(key), false) => NodeTls::from_files(authority, cert, key).map(Some),
            (None, None, None, true) => Ok(None),
            (None, None, None, false) => Err("connections between nodes need --node-ca, --node-cert and --node-key (make the \
                                              files with 'tsql-cluster --make-certs DIR NAME...'), or --insecure to leave them \
                                              unprotected"
                .into()),
            (_, _, _, true) => Err("--insecure cannot be combined with --node-ca, --node-cert or --node-key".into()),
            _ => Err("--node-ca, --node-cert and --node-key go together".into()),
        }
    }
}

/// Write a private key to a new file that only its owner can read (where the system has such permissions).
fn write_new(path: &Path, text: &str, private: bool) -> Result<(), String> {
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    }
    let _ = private;
    open.open(path).and_then(|mut f| f.write_all(text.as_bytes())).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Write certificates for a cluster into `dir`: `NAME.pem` and `NAME.key` for every name, signed by the authority in
/// `ca.pem` and `ca.key`, which are made if they are not there (so that nodes can be added later). Nothing is
/// overwritten.
pub fn make_certs(dir: impl AsRef<Path>, names: &[String]) -> Result<(), String> {
    let dir = dir.as_ref();
    let text = |e: rcgen::Error| e.to_string();
    if let Some(bad) = names.iter().find(|n| n.is_empty() || n.starts_with('.') || n.contains(['/', '\\', ':']) || *n == "ca") {
        return Err(format!("'{bad}' cannot name a node's files"));
    }
    let (authority_pem, authority_key) = (dir.join("ca.pem"), dir.join("ca.key"));
    let new_authority = !authority_key.exists();
    if !new_authority && !authority_pem.exists() {
        return Err(format!("{} is there without {}", authority_key.display(), authority_pem.display()));
    }
    let files = |name: &String| [dir.join(format!("{name}.pem")), dir.join(format!("{name}.key"))];
    // refuse before anything is written
    if let Some(taken) = names.iter().flat_map(files).chain(new_authority.then(|| authority_pem.clone())).find(|p| p.exists()) {
        return Err(format!("{} already exists", taken.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let expiry = rcgen::date_time_ymd(chrono::Utc::now().year() + YEARS, 1, 1);

    let signer = match new_authority {
        true => KeyPair::generate().map_err(text)?,
        false => {
            let pem = std::fs::read_to_string(&authority_key).map_err(|e| format!("cannot read {}: {e}", authority_key.display()))?;
            KeyPair::from_pem(&pem).map_err(text)?
        }
    };
    // the same name and key make an issuer that the certificate in ca.pem vouches for
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, "tsql-server cluster authority");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_after = expiry;
    let authority = params.self_signed(&signer).map_err(text)?;
    if new_authority {
        write_new(&authority_key, &signer.serialize_pem(), true)?;
        write_new(&authority_pem, &authority.pem(), false)?;
    }
    for name in names {
        let mut params = CertificateParams::new(vec![NODE_NAME.to_string()]).map_err(text)?;
        params.distinguished_name.push(DnType::CommonName, name.as_str());
        // a node both listens and calls
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
        params.not_after = expiry;
        let key = KeyPair::generate().map_err(text)?;
        let cert = params.signed_by(&key, &authority, &signer).map_err(text)?;
        let [cert_file, key_file] = files(name);
        write_new(&key_file, &key.serialize_pem(), true)?;
        write_new(&cert_file, &cert.pem(), false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(given: &str) -> Result<Option<NodeTls>, String> {
        let (mut flags, mut args) = (TlsFlags::default(), given.split_whitespace().map(String::from));
        while let Some(a) = args.next() {
            assert!(flags.take(&a, &mut args), "{a}");
        }
        flags.load()
    }

    #[test]
    fn certificates_are_made_once_and_extended_and_the_flags_demand_them() {
        let dir = std::env::temp_dir().join(format!("tsql-certs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let names = |n: &[&str]| n.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        make_certs(&dir, &names(&["control", "shard0"])).unwrap();
        let authority = std::fs::read(dir.join("ca.pem")).unwrap();
        // a node added later is signed by the same authority; existing files stay as they are
        make_certs(&dir, &names(&["shard1"])).unwrap();
        assert_eq!(std::fs::read(dir.join("ca.pem")).unwrap(), authority);
        assert!(make_certs(&dir, &names(&["shard2", "shard0"])).unwrap_err().contains("already exists"));
        assert!(!dir.join("shard2.pem").exists(), "nothing is written when a name is taken");
        for bad in ["ca", "", "../x", "a/b", ".hidden"] {
            assert!(make_certs(&dir, &names(&[bad])).is_err(), "{bad}");
        }
        let file = |f: &str| dir.join(f).display().to_string().replace(' ', "?");
        if !dir.display().to_string().contains(' ') {
            let all = format!("--node-ca {} --node-cert {} --node-key {}", file("ca.pem"), file("shard1.pem"), file("shard1.key"));
            assert!(flags(&all).unwrap().is_some());
            assert!(flags(&format!("{all} --insecure")).err().unwrap().contains("cannot be combined"));
            // a key that is not the certificate's
            assert!(NodeTls::from_files(dir.join("ca.pem"), dir.join("shard1.pem"), dir.join("shard0.key")).is_err());
        }
        assert!(flags("").err().unwrap().contains("--insecure"));
        assert!(flags("--node-ca x").err().unwrap().contains("go together"));
        assert!(flags("--insecure").unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
