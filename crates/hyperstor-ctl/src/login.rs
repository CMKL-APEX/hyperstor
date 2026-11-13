//! `hyperstorctl auth login --directory ISSUER`: a person of a trusted
//! glidex directory signs in for the command line (19-administration.md
//! §23.12, D307), by the directory's device grant as glidex's own CLI
//! does (`gxctl auth login`, glidex D55): an access token for this
//! cluster, and the refresh token that renews it, kept in `--token-file`
//! (mode 0600), which `--mgmt` then reads.
//!
//! The directory is reached at its issuer over HTTPS: its certificate
//! checked against the host's CAs, or pinned by `--directory-pin SHA256`
//! (glidex D63), as a glidex directory with its own certificate needs.

use hyperstor_ctl::DirToken;
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

/// glidex's public CLI client, whose device grant directories offer.
pub const CLIENT: &str = "gxctl";

/// The `resource` that asks a directory for an access token for cluster
/// `id` (glidex D57).
pub fn resource(id: &str) -> String {
    format!("urn:glidex:cluster:{id}")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A TLS client for a directory: pinned to `pin` (its leaf's SHA-256), or
/// the host's CAs.
pub fn directory_tls(pin: Option<&str>) -> Result<Arc<rustls::ClientConfig>, String> {
    match pin {
        None => hyperstor_pki::web_client_config(None).map(Arc::new),
        Some(p) => {
            let h: String = p
                .chars()
                .filter(|c| *c != ':')
                .collect::<String>()
                .to_ascii_lowercase();
            if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("--directory-pin: a certificate's SHA-256 is 64 hex digits".into());
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            Ok(Arc::new(
                rustls::ClientConfig::builder_with_provider(provider.clone())
                    .with_safe_default_protocol_versions()
                    .map_err(|e| e.to_string())?
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(Pinned { pin: h, provider }))
                    .with_no_client_auth(),
            ))
        }
    }
}

/// Takes the one certificate whose SHA-256 is `pin`; its handshake
/// signatures checked as any.
#[derive(Debug)]
struct Pinned {
    pin: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let got: String = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the directory's certificate isn't the pinned one".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// One HTTP/1.1 request to `url` (`https://`, or `http://` with no
/// `tls`): the status and the body, JSON when it is.
pub fn request(
    url: &str,
    tls: Option<Arc<rustls::ClientConfig>>,
    method: &str,
    form: Option<&[(&str, &str)]>,
) -> Result<(u16, serde_json::Value), String> {
    let uri: http::Uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
    let https = uri.scheme_str() == Some("https");
    if https != tls.is_some() {
        return Err(format!("{url}: not the scheme expected"));
    }
    let host = uri
        .host()
        .ok_or("no host")?
        .trim_matches(['[', ']'])
        .to_string();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let body = form
        .map(|f| {
            f.iter()
                .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
                .collect::<Vec<_>>()
                .join("&")
        })
        .unwrap_or_default();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n",
        uri.authority().map_or(host.as_str(), |a| a.as_str())
    );
    if form.is_some() {
        req.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    let tcp = std::net::TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("{host}:{port}: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    match tls {
        Some(cfg) => {
            let name =
                rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
            let conn = rustls::ClientConnection::new(cfg, name).map_err(|e| e.to_string())?;
            let mut s = rustls::StreamOwned::new(conn, tcp);
            s.write_all(req.as_bytes())
                .map_err(|e| format!("{host}:{port}: {e}"))?;
            // A server may close without close_notify: what came counts.
            if let Err(e) = s.read_to_end(&mut raw)
                && e.kind() != std::io::ErrorKind::UnexpectedEof
            {
                return Err(format!("{host}:{port}: {e}"));
            }
        }
        None => {
            let mut s = tcp;
            s.write_all(req.as_bytes())
                .map_err(|e| format!("{host}:{port}: {e}"))?;
            s.read_to_end(&mut raw)
                .map_err(|e| format!("{host}:{port}: {e}"))?;
        }
    }
    hyperstor_ctl::parse_http(&raw)
}

/// `application/x-www-form-urlencoded` of `s`.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The cluster's id, as mgmt at `mgmt` says it (`/auth/methods`).
pub fn cluster_id(mgmt: &str, tls: Option<Arc<rustls::ClientConfig>>) -> Result<String, String> {
    let (s, v) = request(
        &format!("{}/auth/methods", mgmt.trim_end_matches('/')),
        tls,
        "GET",
        None,
    )?;
    match v["cluster_id"].as_str() {
        Some(id) if s == 200 && !id.is_empty() => Ok(id.to_string()),
        _ => Err(format!(
            "{mgmt} names no cluster id (status {s}): is it hyperstor-mgmt, with directories trusted?"
        )),
    }
}

/// The token request's answer as a [`DirToken`] for `directory`, keeping
/// `old`'s refresh token if a new one isn't given.
fn token_of(
    v: &serde_json::Value,
    directory: &str,
    pin: Option<&str>,
    cluster_id: &str,
    old_refresh: &str,
) -> Result<DirToken, String> {
    let access = v["access_token"]
        .as_str()
        .ok_or("the directory gave no access token for this cluster")?;
    Ok(DirToken {
        directory: directory.to_string(),
        pin: pin.map(String::from),
        cluster_id: cluster_id.to_string(),
        access_token: access.to_string(),
        refresh_token: v["refresh_token"]
            .as_str()
            .unwrap_or(old_refresh)
            .to_string(),
        expires_at: now() + v["expires_in"].as_u64().unwrap_or(300),
    })
}

/// The device grant at `directory` for cluster `cluster_id`: says where
/// to approve it, then waits for that. The token.
pub fn device_login(
    directory: &str,
    pin: Option<&str>,
    cluster_id: &str,
) -> Result<DirToken, String> {
    let directory = directory.trim_end_matches('/');
    let tls = directory_tls(pin)?;
    let (s, v) = request(
        &format!("{directory}/device_authorization"),
        Some(tls.clone()),
        "POST",
        Some(&[("client_id", CLIENT), ("scope", "openid")]),
    )?;
    if s != 200 {
        return Err(format!(
            "the directory refused the device login ({s}): {}",
            v["error"].as_str().unwrap_or("?")
        ));
    }
    let (Some(code), Some(user_code)) = (v["device_code"].as_str(), v["user_code"].as_str()) else {
        return Err("the directory's answer has no device code".into());
    };
    let page = v["verification_uri_complete"]
        .as_str()
        .or(v["verification_uri"].as_str())
        .unwrap_or("the directory's device page");
    eprintln!("hyperstorctl: open {page} and confirm the code {user_code}");
    let mut wait = v["interval"].as_u64().unwrap_or(5).max(1);
    let until = now() + v["expires_in"].as_u64().unwrap_or(600);
    let res = resource(cluster_id);
    loop {
        if now() > until {
            return Err("the device login expired before it was approved".into());
        }
        let (s, v) = request(
            &format!("{directory}/token"),
            Some(tls.clone()),
            "POST",
            Some(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", code),
                ("client_id", CLIENT),
                ("resource", &res),
            ]),
        )?;
        if s == 200 {
            return token_of(&v, directory, pin, cluster_id, "");
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => wait += 5,
            Some(e) => return Err(format!("the directory refused the login: {e}")),
            None => return Err(format!("the directory answered {s}")),
        }
        std::thread::sleep(Duration::from_secs(wait));
    }
}

/// `t` renewed with its refresh token.
pub fn refresh(t: &DirToken) -> Result<DirToken, String> {
    if t.refresh_token.is_empty() {
        return Err("no refresh token".into());
    }
    let tls = directory_tls(t.pin.as_deref())?;
    let res = resource(&t.cluster_id);
    let (s, v) = request(
        &format!("{}/token", t.directory),
        Some(tls),
        "POST",
        Some(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &t.refresh_token),
            ("client_id", CLIENT),
            ("resource", &res),
        ]),
    )?;
    if s != 200 {
        return Err(format!(
            "the directory refused to renew the login ({s}: {})",
            v["error"].as_str().unwrap_or("?")
        ));
    }
    token_of(
        &v,
        &t.directory,
        t.pin.as_deref(),
        &t.cluster_id,
        &t.refresh_token,
    )
}

/// Keeps `t` in `file`, readable by its owner only.
pub fn save(file: &str, t: &DirToken) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = format!("{file}.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("{tmp}: {e}"))?;
    f.write_all(t.to_json().as_bytes())
        .map_err(|e| format!("{tmp}: {e}"))?;
    std::fs::rename(&tmp, file).map_err(|e| format!("{file}: {e}"))
}
