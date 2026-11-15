//! HTTPS for people (04-decisions.md D108–D110): the certificate in
//! `--web-tls DIR` (`web.crt`, `web.key`: the installer's from the cluster
//! CA, or the operator's own), re-read when the files change so an ACME
//! client can keep them current (D109); and with `--tailscale SOCKET` the
//! node's Tailscale certificates for its `ts.net` names, chosen by SNI,
//! through glidex's shared `glidex_tls::tailscale`.
//!
//! - A plain-HTTP request on the HTTPS port is answered `308` to the same
//!   URL over HTTPS (a TLS handshake starts with `0x16`), as glidex-ui
//!   does, so `host:8852` typed in a browser still works.
//! - The DNS names on `web.crt` are `Host` names people may use (D277):
//!   a certificate from the operator's CA (Cloudflare's Origin CA, the
//!   organization's) for a proxy's public name lets that name in, and a
//!   new certificate's names replace the old ones'.
//! - With `--proxy-ca FILE`, a client may present a certificate from that
//!   CA (Cloudflare's Authenticated Origin Pulls, or the organization's
//!   proxies): its connection is a proxy's ([`crate::front`]). Others
//!   present none, as before.
//! - HSTS only on connections served a Tailscale certificate
//!   (`max-age=86400`, as glidex-ui): with the cluster CA's certificate a
//!   browser that hasn't imported the root would be locked out.

use glidex_tls::tailscale::{Held, Tailscale};
use glidex_tls::{FileCert, Resolver};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How often `web.crt` and `web.key` are checked for changes:
/// glidex_tls's minute, or `HS_WEB_TLS_POLL_MS` (for tests).
fn poll_every() -> Duration {
    std::env::var("HS_WEB_TLS_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(glidex_tls::RELOAD_EVERY)
}

pub struct Web {
    pub config: Arc<tokio_rustls::rustls::ServerConfig>,
    /// The DNS names on `web.crt` now.
    pub names: Arc<std::sync::RwLock<Vec<String>>>,
    /// The Tailscale names a certificate is held for now, with `--tailscale`.
    pub held: Option<Arc<Held>>,
    /// `web.crt` as served now: what directories pin (D343).
    pub slot: Arc<glidex_tls::CertSlot>,
}

/// The SHA-256 a directory pins for mgmt at `url` (`https://host[:port]`,
/// the offer's address, D312, D343): of `cert`, the leaf mgmt serves from
/// `--web-tls` now, read from mgmt itself and never from what a browser
/// was served, which may be a proxy's. A host given its Tailscale
/// certificate (`held`), or an mgmt with no certificate of its own,
/// needs the pin given.
pub fn offer_pin(url: &str, cert: Option<&[u8]>, held: Option<&Held>) -> Result<String, String> {
    let uri: http::Uri = url
        .parse()
        .map_err(|_| format!("{url:?} isn't an https:// address"))?;
    let host = match (uri.scheme_str(), uri.host()) {
        (Some("https"), Some(h)) => h.trim_matches(['[', ']']),
        _ => return Err(format!("{url:?} isn't an https:// address")),
    };
    if held.is_some_and(|h| h.contains(host)) {
        return Err(format!(
            "{host} is served its Tailscale certificate, renewed by Tailscale: give the pin"
        ));
    }
    let cert = cert
        .ok_or("this hyperstor-mgmt serves no certificate of its own (--web-tls): give the pin")?;
    Ok(ring::digest::digest(&ring::digest::SHA256, cert)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The server config for `dir`'s certificate (and the Tailscale ones from
/// `tailscale`'s `tailscaled`), and the tasks that keep them current. In a
/// tokio runtime.
pub fn start(
    dir: &Path,
    tailscale: Option<PathBuf>,
    proxy_ca: Option<&Path>,
) -> Result<Web, String> {
    let file = FileCert::load(&dir.join("web.crt"), &dir.join("web.key"))?;
    let slot = file.slot.clone();
    let resolver = Resolver::new(file.slot.clone());
    let names = Arc::new(std::sync::RwLock::new(dns_names(&file)));
    let seen = names.clone();
    // Not the file's name or contents: no certificate data in logs.
    file.watch(poll_every(), move |f| {
        *seen.write().unwrap_or_else(|e| e.into_inner()) = dns_names(f);
        eprintln!("hyperstor-mgmt: the web certificate changed: now serving it")
    });
    let held = tailscale.map(|sock| {
        let ts = Tailscale::new(sock, resolver.clone());
        let held = ts.held();
        tokio::spawn(ts.run());
        held
    });
    let config = match proxy_ca {
        None => glidex_tls::resolver_config(resolver, &[b"http/1.1"])?,
        Some(ca) => proxy_config(resolver, ca)?,
    };
    Ok(Web {
        config,
        names,
        held,
        slot,
    })
}

/// The DNS names on the certificate `f` serves now.
fn dns_names(f: &FileCert) -> Vec<String> {
    glidex_tls::leaf_names(&f.slot.get())
        .map(|(n, _)| n)
        .unwrap_or_default()
}

/// As [`glidex_tls::resolver_config`], and a client certificate, if one is
/// presented, must be from a CA in `ca` (PEM, one or more).
fn proxy_config(
    resolver: Arc<Resolver>,
    ca: &Path,
) -> Result<Arc<tokio_rustls::rustls::ServerConfig>, String> {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    for c in glidex_tls::load_certs(ca)? {
        roots.add(c).map_err(|e| format!("{}: {e}", ca.display()))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .allow_unauthenticated()
    .build()
    .map_err(|e| format!("{}: {e}", ca.display()))?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(resolver);
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// Whether the connection's first byte starts a TLS handshake. `None`:
/// it closed first.
pub async fn starts_tls(stream: &tokio::net::TcpStream) -> Option<bool> {
    let mut b = [0u8; 1];
    match tokio::time::timeout(Duration::from_secs(10), stream.peek(&mut b)).await {
        Ok(Ok(1)) => Some(b[0] == 0x16),
        _ => None,
    }
}

/// The `308` to HTTPS for a plain-HTTP request on the HTTPS port.
pub fn to_https<B>(
    req: &http::Request<B>,
) -> http::Response<http_body_util::Full<hyper::body::Bytes>> {
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    let mut r = http::Response::builder().status(308);
    // A Host we can't name gets no Location: nothing to send it to.
    if !host.is_empty() && !host.contains(['/', '\\', ' ']) {
        r = r.header(http::header::LOCATION, format!("https://{host}{path}"));
    }
    r.header("Cache-Control", "no-store")
        .body(http_body_util::Full::new(hyper::body::Bytes::from_static(
            b"use https://\n",
        )))
        .expect("response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_goes_to_https() {
        let req = http::Request::get("/volumes?pool=1")
            .header("Host", "mgmt.example:8852")
            .body(())
            .unwrap();
        let r = to_https(&req);
        assert_eq!(r.status(), 308);
        assert_eq!(
            r.headers()["location"],
            "https://mgmt.example:8852/volumes?pool=1"
        );
        let odd = http::Request::get("/")
            .header("Host", "a/b")
            .body(())
            .unwrap();
        assert!(to_https(&odd).headers().get("location").is_none());
    }

    /// An offer's pin is the SHA-256 of mgmt's own certificate, for an
    /// https address only, and asked for where mgmt has none (D343).
    #[test]
    fn an_offer_pins_mgmt_s_own_certificate() {
        let der = b"a certificate's DER";
        let want: String = ring::digest::digest(&ring::digest::SHA256, der)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        for url in [
            "https://mgmt.example:8852",
            "https://192.0.2.10",
            "https://[2001:db8::1]:8852",
        ] {
            assert_eq!(
                offer_pin(url, Some(der), None).as_deref(),
                Ok(want.as_str()),
                "{url}"
            );
        }
        assert_eq!(want.len(), 64);
        for bad in ["http://mgmt.example", "mgmt.example", "", "https://"] {
            assert!(offer_pin(bad, Some(der), None).is_err(), "{bad}");
        }
        let none = offer_pin("https://mgmt.example", None, None).unwrap_err();
        assert!(none.contains("give the pin"), "{none}");
    }
}
