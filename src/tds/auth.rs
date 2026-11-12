//! Authentication beyond SQL logins: Azure AD (Entra ID) access tokens and Kerberos (SSPI on Windows, GSSAPI on Linux and
//! macOS).
//!
//! Neither kind of identity has a password in this server. A login that may authenticate this way is created with
//! `CREATE LOGIN [name] FROM EXTERNAL PROVIDER` (Azure AD: `user@contoso.com`) or `CREATE LOGIN [DOMAIN\user] FROM WINDOWS`
//! (Kerberos), and the TDS login finds it with [`crate::Server::connect_external`] once the identity is established.
//!
//! * **Azure AD**: the client sends an access token (a JWT signed with RS256) in the FEDAUTH feature of Login7, or fetches
//!   one itself after the server has told it where (the `ActiveDirectory*` authentication modes). The token's signature is
//!   checked against the tenant's signing keys, which the operator supplies as a JWKS file (see [`AadConfig`]), and its
//!   issuer, audience and lifetime are checked.
//! * **Kerberos**: the client's SSPI token travels in Login7 and in follow-up SSPI messages; [`Acceptor`] is the server's
//!   side of the exchange, implemented by `sspi.rs` (feature `kerberos-windows`) and `gssapi.rs` (features `kerberos-linux`
//!   and `kerberos-macos`).
use std::path::PathBuf;

/// What the listener accepts besides SQL logins; both are off by default.
#[derive(Clone, Debug, Default)]
pub struct AuthConfig {
    pub aad: Option<AadConfig>,
    pub kerberos: Option<KerberosConfig>,
}

/// Azure AD token validation.
#[derive(Clone, Debug)]
pub struct AadConfig {
    /// The tenant id (a GUID or a domain): tokens must be issued by it.
    pub tenant: String,
    /// Accepted `aud` claims. Empty: `https://database.windows.net/`, what a client asks for when it connects to SQL.
    pub audiences: Vec<String>,
    /// The tenant's signing keys. They change a few times a year; with a file the server re-reads it at every login, so
    /// replacing the file is enough.
    pub jwks: JwksSource,
}

#[derive(Clone, Debug)]
pub enum JwksSource {
    /// A JSON Web Key Set on disk, e.g. downloaded from `https://login.microsoftonline.com/<tenant>/discovery/v2.0/keys`.
    File(PathBuf),
    /// The key set itself (for tests).
    Inline(String),
}

/// Kerberos acceptor settings.
#[derive(Clone, Debug, Default)]
pub struct KerberosConfig {
    /// The service principal the server answers to (`MSSQLSvc/host.example.com:1433`, or `MSSQLSvc@host` in the host-based
    /// form); without one every principal in the keytab is accepted (GSSAPI) or the machine account is used (SSPI).
    pub service: Option<String>,
    /// The keytab holding the service's key (GSSAPI; it is exported as `KRB5_KTNAME`). SSPI needs none.
    pub keytab: Option<PathBuf>,
    /// Windows only: accept NTLM, which `Negotiate` falls back to when Kerberos is not possible (no domain, or an SPN that
    /// does not resolve). Off by default: only Kerberos is accepted.
    pub allow_ntlm: bool,
}

/// The server's side of an SSPI/GSSAPI exchange.
pub trait Acceptor {
    /// Process the client's token. Returns the token to send back (possibly empty) and, once the exchange is complete,
    /// the authenticated identity in the forms it can be known by (`DOMAIN\user`, `user@REALM`).
    fn step(&mut self, token: &[u8]) -> Result<(Vec<u8>, Option<Vec<String>>), String>;
}

/// A new acceptor for one login, or why this build has none.
pub fn new_acceptor(cfg: &KerberosConfig) -> Result<Box<dyn Acceptor>, String> {
    #[cfg(all(windows, feature = "kerberos-windows"))]
    {
        return super::sspi::SspiAcceptor::new(cfg).map(|a| Box::new(a) as Box<dyn Acceptor>);
    }
    #[cfg(all(unix, any(feature = "kerberos-linux", feature = "kerberos-macos")))]
    {
        return super::gssapi::GssAcceptor::new(cfg).map(|a| Box::new(a) as Box<dyn Acceptor>);
    }
    #[allow(unreachable_code)]
    {
        let _ = cfg;
        Err("this build has no Kerberos support (build with --features kerberos-windows, kerberos-linux or kerberos-macos)".into())
    }
}

/// Whether this build can accept Kerberos logins.
pub fn kerberos_built_in() -> bool {
    cfg!(any(all(windows, feature = "kerberos-windows"), all(unix, any(feature = "kerberos-linux", feature = "kerberos-macos"))))
}

// ----------------------------------------------------------------------------- Azure AD tokens
fn b64url(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err("the token is not base64url".into()),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// The token as text: clients send it as UTF-16 (ADO.NET) or UTF-8.
pub fn token_text(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw.len() % 2 == 0 && raw[1] == 0 {
        let u: Vec<u16> = raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u).trim().to_string()
    } else {
        String::from_utf8_lossy(raw).trim().to_string()
    }
}

fn claim_list(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

/// Check an Azure AD access token; the names of the identity it carries (`upn`, `preferred_username`, ...), best first.
pub fn validate_aad_token(cfg: &AadConfig, token: &str) -> Result<Vec<String>, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("the token is not a JWT".into());
    }
    let header: serde_json::Value = serde_json::from_slice(&b64url(parts[0])?).map_err(|_| "the token header is not JSON")?;
    if header["alg"] != "RS256" {
        return Err(format!("the token is signed with {}, not RS256", header["alg"]));
    }
    let kid = header["kid"].as_str().unwrap_or("");
    let jwks_text = match &cfg.jwks {
        JwksSource::File(p) => std::fs::read_to_string(p).map_err(|e| format!("cannot read the signing keys {}: {}", p.display(), e))?,
        JwksSource::Inline(s) => s.clone(),
    };
    let jwks: serde_json::Value = serde_json::from_str(&jwks_text).map_err(|_| "the signing keys are not JSON")?;
    let key = jwks["keys"].as_array().and_then(|ks| ks.iter().find(|k| k["kid"].as_str() == Some(kid) && k["kty"] == "RSA")).ok_or_else(|| format!("no signing key '{}' for the token", kid))?;
    let n = b64url(key["n"].as_str().unwrap_or(""))?;
    let e = b64url(key["e"].as_str().unwrap_or(""))?;
    let signed = format!("{}.{}", parts[0], parts[1]);
    let sig = b64url(parts[2])?;
    ring::signature::RsaPublicKeyComponents { n: &n[..], e: &e[..] }.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &sig).map_err(|_| "the token's signature is not valid")?;
    let claims: serde_json::Value = serde_json::from_slice(&b64url(parts[1])?).map_err(|_| "the token body is not JSON")?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    const SKEW: i64 = 300;
    match claims["exp"].as_i64() {
        Some(exp) if exp + SKEW >= now => {}
        _ => return Err("the token has expired".into()),
    }
    if let Some(nbf) = claims["nbf"].as_i64() {
        if nbf - SKEW > now {
            return Err("the token is not valid yet".into());
        }
    }
    let iss = claims["iss"].as_str().unwrap_or("");
    let t = &cfg.tenant;
    if ![format!("https://login.microsoftonline.com/{}/v2.0", t), format!("https://sts.windows.net/{}/", t), format!("https://login.windows.net/{}/", t)].iter().any(|i| i.eq_ignore_ascii_case(iss)) {
        return Err(format!("the token was issued by '{}', not by tenant {}", iss, t));
    }
    let wanted: Vec<String> = if cfg.audiences.is_empty() { vec!["https://database.windows.net/".into(), "https://database.windows.net".into()] } else { cfg.audiences.clone() };
    let aud = claim_list(&claims["aud"]);
    if !aud.iter().any(|a| wanted.iter().any(|w| w.eq_ignore_ascii_case(a))) {
        return Err(format!("the token is for {:?}, not for this server", aud));
    }
    let mut names: Vec<String> = ["upn", "preferred_username", "unique_name", "email", "appid", "azp", "oid"].iter().filter_map(|c| claims[*c].as_str()).filter(|s| !s.is_empty()).map(String::from).collect();
    names.dedup();
    if names.is_empty() {
        return Err("the token names no user".into());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url() {
        assert_eq!(b64url("aGVsbG8").unwrap(), b"hello");
        assert_eq!(b64url("_-8").unwrap(), vec![0xFF, 0xEF]);
        assert!(b64url("a$b").is_err());
    }

    #[test]
    fn token_text_reads_utf16_and_utf8() {
        assert_eq!(token_text(b"abc"), "abc");
        assert_eq!(token_text(&[b'a', 0, b'b', 0]), "ab");
    }

    #[test]
    fn a_malformed_token_is_refused() {
        let cfg = AadConfig { tenant: "t".into(), audiences: vec![], jwks: JwksSource::Inline("{\"keys\":[]}".into()) };
        assert!(validate_aad_token(&cfg, "not.a.jwt").is_err());
        assert!(validate_aad_token(&cfg, "a.b").is_err());
    }
}
