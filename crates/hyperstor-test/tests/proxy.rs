//! hyperstor-mgmt behind a proxy or a CDN (04-decisions.md D277): a
//! certificate from another CA (Cloudflare's Origin CA, an
//! organization's) with its chain, whose names are allowed `Host` names;
//! proxies known by a client certificate from `--proxy-ca`, the only ones
//! whose `CF-Connecting-IP` is believed; and `--proxy-only`.

use hyperstor_test::*;
use rustls::pki_types::pem::PemObject;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::sync::Arc;

/// Accepts any server certificate: this test looks at which one is
/// served and what mgmt answers, not whether a browser would trust it.
#[derive(Debug)]
struct AnyCert;

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `METHOD path` to `at` with `Host: host`, `headers` and `body`,
/// presenting `client` (a PEM chain and key) if given: the served
/// certificate's SHA-256 and the answer's head and body, or the error.
fn call(
    at: std::net::SocketAddr,
    host: &str,
    client: Option<&hyperstor_pki::Pem>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<(String, String, String), String> {
    let b = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyCert));
    let config = match client {
        None => b.with_no_client_auth(),
        Some(p) => {
            let chain = rustls::pki_types::CertificateDer::pem_slice_iter(p.cert.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(p.key.as_bytes()).unwrap();
            b.with_client_auth_cert(chain, key).unwrap()
        }
    };
    let name = rustls::pki_types::ServerName::IpAddress(at.ip().into());
    let conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let tcp = std::net::TcpStream::connect(at).map_err(|e| e.to_string())?;
    let mut s = rustls::StreamOwned::new(conn, tcp);
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Err(e) = s.read_to_end(&mut out)
        && out.is_empty()
    {
        return Err(e.to_string());
    }
    let der = s
        .conn
        .peer_certificates()
        .ok_or("no certificate")?
        .first()
        .ok_or("no certificate")?
        .to_vec();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Ok((
        hyperstor_pki::sha256_hex(&der),
        head.to_string(),
        body.to_string(),
    ))
}

/// A leaf from `ca` (an intermediate) for `names` and `ips`, with its
/// chain (leaf, then `ca`) and its key.
fn leaf(ca: &hyperstor_pki::Pem, names: &[&str], ips: &[IpAddr]) -> hyperstor_pki::Pem {
    let who = hyperstor_pki::Identity {
        cluster: "org".into(),
        role: hyperstor_pki::Role::Mgmt,
        id: "proxy-test".into(),
    };
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    let p = hyperstor_pki::issue(ca, &who, ips, &names, 30).unwrap();
    hyperstor_pki::Pem {
        cert: format!("{}{}", p.cert, ca.cert),
        key: p.key,
    }
}

#[test]
fn mgmt_serves_a_proxy_with_an_organizations_certificates() {
    let mut c = Cluster::all_in_one("proxy", 1);
    c.mgmt_all_addresses = true;
    // SAFETY: this test binary runs this one test; nothing else reads the
    // environment meanwhile. mgmt checks its certificate files every 200 ms.
    unsafe { std::env::set_var("HS_WEB_TLS_POLL_MS", "200") };
    // An organization's CA for the web certificate, another for its
    // proxies' client certificates.
    let org_root = hyperstor_pki::new_root("org").unwrap();
    let org = hyperstor_pki::new_intermediate(&org_root, "org").unwrap();
    let proxies_root = hyperstor_pki::new_root("proxies").unwrap();
    let proxies = hyperstor_pki::new_intermediate(&proxies_root, "proxies").unwrap();
    let ca_file = c.pki().join("proxies-ca.pem");
    std::fs::write(&ca_file, &proxies_root.cert).unwrap();
    c.restart_mgmt_with(&[
        "--proxy-ca",
        ca_file.to_str().unwrap(),
        "--trusted-proxies",
        "192.0.2.0/24",
        "--proxy-only",
    ]);
    let port = c.mgmt_port;
    let lo: IpAddr = "127.0.0.1".parse().unwrap();
    let public = format!("mgmt.example.test:{port}");
    let health = |ip: IpAddr, host: &str, client: Option<&hyperstor_pki::Pem>| {
        call((ip, port).into(), host, client, "GET", "/health", &[], "")
    };

    // The public name isn't on the cluster CA's certificate: refused.
    let (_, head, _) = health(lo, &public, None).unwrap();
    assert!(head.starts_with("HTTP/1.1 421"), "{head}");

    // The organization's certificate, with its chain and a PKCS#8 key,
    // put in place: served without a restart, and its name let in.
    let web = leaf(&org, &["mgmt.example.test"], &[lo]);
    std::fs::write(c.pki().join("web.key"), &web.key).unwrap();
    std::fs::write(c.pki().join("web.crt"), &web.cert).unwrap();
    let want = hyperstor_pki::fingerprint(&web.cert).unwrap();
    let head = wait_for("the organization's certificate", 10, || {
        let (fp, head, _) = health(lo, &public, None).ok()?;
        (fp == want).then_some(head)
    });
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let (_, head, _) = health(lo, &format!("evil.example:{port}"), None).unwrap();
    assert!(head.starts_with("HTTP/1.1 421"), "{head}");

    // From another of the host's addresses, as a stranger would come:
    // refused without the proxy's certificate, served with it, and a
    // certificate from another CA fails the handshake.
    let other = glidex_tls::interface_ips()
        .unwrap()
        .into_iter()
        .find(|ip| ip.is_ipv4() && !ip.is_loopback());
    let proxy = leaf(&proxies, &["proxy.example.test"], &[]);
    let login = serde_json::json!({ "user": "ann", "password": PASSWORD }).to_string();
    let json = ("Content-Type", "application/json");
    if let Some(ip) = other {
        let (_, head, body) = health(ip, &public, None).unwrap();
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(body.contains("only through the proxy"), "{body}");
        let (_, head, _) = health(ip, &public, Some(&proxy)).unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let stranger = leaf(&org, &["stranger.example.test"], &[]);
        assert!(
            health(ip, &public, Some(&stranger)).is_err_and(|e| !e.is_empty()),
            "a client certificate from another CA"
        );
        // The proxy says who is calling, and is believed.
        let (_, head, _) = call(
            (ip, port).into(),
            &public,
            Some(&proxy),
            "POST",
            "/auth/login",
            &[json, ("CF-Connecting-IP", "203.0.113.9")],
            &login,
        )
        .unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}\n{}", c.mgmt_log());
        assert!(
            c.mgmt_log().contains("from 203.0.113.9"),
            "{}",
            c.mgmt_log()
        );
    } else {
        eprintln!("no non-loopback IPv4 address here: the proxy's own checks skipped");
    }
    // From loopback (the host itself, not a proxy): served, and what it
    // says about the caller is ignored.
    let (_, head, _) = call(
        (lo, port).into(),
        &public,
        None,
        "POST",
        "/auth/login",
        &[
            json,
            ("CF-Connecting-IP", "198.51.100.66"),
            ("X-Forwarded-For", "198.51.100.67"),
        ],
        &login,
    )
    .unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let log = c.mgmt_log();
    assert!(!log.contains("198.51.100.6"), "{log}");
    assert!(log.contains("user:ann from 127.0.0.1"), "{log}");
}
