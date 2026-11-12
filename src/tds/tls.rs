//! TLS for the TDS listener.
//!
//! TDS does not run TLS directly on the socket. During the handshake the TLS records travel inside TDS pre-login
//! packets; once the handshake is done, TLS records are sent as they are. If the client asked for full encryption the
//! whole session stays inside TLS; if it only asked for the login to be protected (`ENCRYPT_OFF`), TLS is dropped
//! again right after the Login7 message and the rest of the session is plain TCP.
//!
//! [`TlsConfig`] holds the server certificate. The handshake itself is driven by [`handshake`], which feeds the
//! TLS state machine (rustls, which does no I/O of its own) with the contents of pre-login packets.
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

/// How the listener treats encryption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encryption {
    /// Encrypt when the client asks for it (the login is always encrypted if the client supports TLS).
    Optional,
    /// Refuse clients that do not support TLS, and encrypt every session ("Force Encryption").
    Required,
}

/// The server certificate and key.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    pub(crate) cfg: Arc<ServerConfig>,
    /// PEM of the server certificate, for clients that want to trust it explicitly.
    pub certificate_pem: String,
    key_pem: Vec<u8>,
}

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

impl TlsConfig {
    /// A certificate chain and private key in PEM format.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<TlsConfig> {
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<_, _>>().map_err(bad)?;
        if chain.is_empty() {
            return Err(bad("no certificate found in the PEM data"));
        }
        TlsConfig::build(cert_pem, key_pem, false)
    }

    fn build(cert_pem: &[u8], key_pem: &[u8], tls13: bool) -> io::Result<TlsConfig> {
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<_, _>>().map_err(bad)?;
        if chain.is_empty() {
            return Err(bad("no certificate found in the PEM data"));
        }
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(bad)?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // TLS 1.2 by default, as SQL Server before 2022 and the TDS 7.x clients (the Microsoft JDBC driver among them) expect;
        // TLS 1.3 (what SQL Server 2022 and TDS 8 clients use) is opt-in. The handshake is wrapped in TDS packets either way
        let versions: &[&'static rustls::SupportedProtocolVersion] = if tls13 { &[&rustls::version::TLS13, &rustls::version::TLS12] } else { &[&rustls::version::TLS12] };
        let mut cfg = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .map_err(bad)?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(bad)?;
        // anything the server sends after the handshake must not need TDS framing, so no session tickets
        cfg.send_tls13_tickets = 0;
        Ok(TlsConfig { cfg: Arc::new(cfg), certificate_pem: String::from_utf8_lossy(cert_pem).into_owned(), key_pem: key_pem.to_vec() })
    }

    /// The same certificate, also offering TLS 1.3 (`enabled`) or only TLS 1.2.
    pub fn tls13(&self, enabled: bool) -> io::Result<TlsConfig> {
        TlsConfig::build(self.certificate_pem.as_bytes(), &self.key_pem, enabled)
    }

    /// A certificate and key read from PEM files.
    pub fn from_pem_files(cert: impl AsRef<Path>, key: impl AsRef<Path>) -> io::Result<TlsConfig> {
        TlsConfig::from_pem(&std::fs::read(cert)?, &std::fs::read(key)?)
    }

    /// A new self-signed certificate valid for `names` (host names or IP addresses). SQL Server does the same at
    /// start-up when no certificate is installed; clients must then be told to trust the server certificate.
    pub fn self_signed(names: Vec<String>) -> io::Result<TlsConfig> {
        let (cert, key) = TlsConfig::generate_pem(names)?;
        TlsConfig::from_pem(cert.as_bytes(), key.as_bytes())
    }

    /// A new self-signed certificate and its private key, both as PEM text (to keep them in files).
    pub fn generate_pem(names: Vec<String>) -> io::Result<(String, String)> {
        let cert = rcgen::generate_simple_self_signed(names).map_err(bad)?;
        Ok((cert.cert.pem(), cert.key_pair.serialize_pem()))
    }
}

/// The client socket: plain TCP, or TCP inside TLS.
pub(crate) enum Wire {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Wire {
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Wire::Plain(s) => s,
            Wire::Tls(t) => &t.sock,
        }
    }
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(buf),
            Wire::Tls(t) => t.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(t) => t.flush(),
        }
    }
}

/// Run the TLS handshake. `read` returns the contents of the next pre-login message from the client and `send`
/// transmits bytes to it as a pre-login message. Returns the established connection.
pub(crate) fn handshake(
    cfg: &TlsConfig,
    mut read: impl FnMut() -> io::Result<Vec<u8>>,
    mut send: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<ServerConnection> {
    let mut conn = ServerConnection::new(cfg.cfg.clone()).map_err(bad)?;
    loop {
        if conn.wants_write() {
            let mut out = vec![];
            while conn.wants_write() {
                conn.write_tls(&mut out)?;
            }
            send(&out)?;
        }
        if !conn.is_handshaking() {
            return Ok(conn);
        }
        let payload = read()?;
        let mut rd = &payload[..];
        while !rd.is_empty() {
            conn.read_tls(&mut rd)?;
            conn.process_new_packets().map_err(bad)?;
        }
    }
}
