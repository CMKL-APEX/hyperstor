//! HTTPS for people (decisions.md D108–D110): the certificate in
//! `--web-tls DIR` (`web.crt`, `web.key`: the installer's from the cluster
//! CA, or the operator's own), re-read when the files change so an ACME
//! client can keep them current (D109); and with `--tailscale SOCKET` the
//! node's Tailscale certificates for its `ts.net` names, chosen by SNI,
//! through glidex's shared `glidex_tls::tailscale`.
//!
//! - A plain-HTTP request on the HTTPS port is answered `308` to the same
//!   URL over HTTPS (a TLS handshake starts with `0x16`), as glidex-ui
//!   does, so `host:8852` typed in a browser still works.
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
    /// The Tailscale names a certificate is held for now, with `--tailscale`.
    pub held: Option<Arc<Held>>,
}

/// The server config for `dir`'s certificate (and the Tailscale ones from
/// `tailscale`'s `tailscaled`), and the tasks that keep them current. In a
/// tokio runtime.
pub fn start(dir: &Path, tailscale: Option<PathBuf>) -> Result<Web, String> {
    let file = FileCert::load(&dir.join("web.crt"), &dir.join("web.key"))?;
    let resolver = Resolver::new(file.slot.clone());
    // Not the file's name or contents: no certificate data in logs.
    file.watch(poll_every(), |_| {
        eprintln!("hyperstor-mgmt: the web certificate changed: now serving it")
    });
    let held = tailscale.map(|sock| {
        let ts = Tailscale::new(sock, resolver.clone());
        let held = ts.held();
        tokio::spawn(ts.run());
        held
    });
    Ok(Web {
        config: glidex_tls::resolver_config(resolver, &[b"http/1.1"])?,
        held,
    })
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
}
