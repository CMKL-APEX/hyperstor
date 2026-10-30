//! People reach hyperstor-mgmt on the host's addresses over HTTPS
//! (04-decisions.md D108–D110): the cluster CA's certificate, re-read when
//! its files change (an ACME client's renewals, D109), the node's
//! Tailscale certificate for its ts.net names with HSTS, and plain HTTP
//! sent to HTTPS.

use hyperstor_test::*;
use std::io::{Read, Write};
use std::sync::Arc;

/// Accepts any server certificate: these tests look at which one is
/// served, not whether a browser would trust it.
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

/// `GET /health` to `ip:port` naming `sni` (none: by address) and `host`:
/// the served certificate's SHA-256 and the answer's head.
fn probe(ip: std::net::IpAddr, port: u16, sni: Option<&str>, host: &str) -> (String, String) {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyCert))
    .with_no_client_auth();
    let name = match sni {
        Some(n) => rustls::pki_types::ServerName::try_from(n.to_string()).unwrap(),
        None => rustls::pki_types::ServerName::IpAddress(ip.into()),
    };
    let conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let tcp = std::net::TcpStream::connect((ip, port)).unwrap();
    let mut s = rustls::StreamOwned::new(conn, tcp);
    write!(
        s,
        "GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let der = s.conn.peer_certificates().unwrap()[0].to_vec();
    let text = String::from_utf8_lossy(&out).into_owned();
    let head = text.split("\r\n\r\n").next().unwrap_or("").to_string();
    (hyperstor_pki::sha256_hex(&der), head)
}

fn fingerprint_of(pem: &str) -> String {
    hyperstor_pki::fingerprint(pem).unwrap()
}

#[test]
fn people_reach_mgmt_on_the_hosts_addresses() {
    // A fake tailscaled with a certificate for this node's ts.net name.
    let ts_dir = tempfile_dir("ts");
    let ts_pair = glidex_tls::tailscale::testing::pair(&["node.tail.ts.net"], 80);
    let sock = {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = ts_dir.clone();
        let pair = ts_pair.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let fake = glidex_tls::tailscale::testing::Fake {
                    domains: Some(vec!["node.tail.ts.net".into()]),
                    pairs: [("node.tail.ts.net".to_string(), pair)].into(),
                    ..Default::default()
                };
                let sock = glidex_tls::tailscale::testing::serve_fake(
                    &dir,
                    Arc::new(std::sync::Mutex::new(fake)),
                );
                tx.send(sock).unwrap();
                std::future::pending::<()>().await
            })
        });
        rx.recv().unwrap()
    };

    let mut c = Cluster::all_in_one("web", 1);
    c.mgmt_all_addresses = true;
    // SAFETY: this test binary runs this one test; nothing else reads the
    // environment meanwhile. mgmt checks its certificate files every 200 ms.
    unsafe { std::env::set_var("HS_WEB_TLS_POLL_MS", "200") };
    c.restart_mgmt_with(&["--tailscale", sock.to_str().unwrap()]);
    let port = c.mgmt_port;
    let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let web_pem = || std::fs::read_to_string(c.pki().join("web.crt")).unwrap();

    // By address: the cluster CA's certificate, no HSTS.
    let (fp, head) = probe(lo, port, None, &format!("127.0.0.1:{port}"));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(fp, fingerprint_of(&web_pem()));
    assert!(
        !head
            .to_ascii_lowercase()
            .contains("strict-transport-security"),
        "{head}"
    );
    // Another address of this host, as the certificate names them.
    let (ips, _) = hyperstor_pki::names_of(&web_pem()).unwrap();
    if let Some(ip) = ips.iter().find(|ip| !ip.is_loopback() && ip.is_ipv4()) {
        let (_, head) = probe(*ip, port, None, &format!("{ip}:{port}"));
        assert!(head.starts_with("HTTP/1.1 200"), "another address: {head}");
    }
    // A name that isn't this host's: refused (DNS rebinding).
    let (_, head) = probe(lo, port, None, &format!("evil.example:{port}"));
    assert!(head.starts_with("HTTP/1.1 421"), "{head}");

    // Plain HTTP on the port: sent to HTTPS.
    let mut tcp = std::net::TcpStream::connect((lo, port)).unwrap();
    write!(
        tcp,
        "GET /volumes HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    let _ = tcp.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 308"), "{out}");
    assert!(
        out.contains(&format!("location: https://127.0.0.1:{port}/volumes")),
        "{out}"
    );

    // The node's Tailscale name: its certificate, its Host, and HSTS.
    let ts = wait_for("the Tailscale certificate", 20, || {
        let (fp, head) = probe(
            lo,
            port,
            Some("node.tail.ts.net"),
            &format!("node.tail.ts.net:{port}"),
        );
        (fp == fingerprint_of(&ts_pair)).then_some(head)
    });
    assert!(ts.starts_with("HTTP/1.1 200"), "{ts}");
    assert!(
        ts.to_ascii_lowercase()
            .contains("strict-transport-security: max-age=86400"),
        "{ts}"
    );

    // The certificate files replaced (an ACME client's renewal): served
    // without a restart.
    let old = fingerprint_of(&web_pem());
    let ca = hyperstor_pki::Pem {
        cert: std::fs::read_to_string(c.pki().join("intermediate.crt")).unwrap(),
        key: std::fs::read_to_string(c.pki().join("intermediate.key")).unwrap(),
    };
    let who = hyperstor_pki::Identity {
        cluster: c.cluster_id.clone(),
        role: hyperstor_pki::Role::Mgmt,
        id: "web".into(),
    };
    let new = hyperstor_pki::issue(&ca, &who, &[lo], &["localhost".into()], 30).unwrap();
    std::fs::write(c.pki().join("web.key"), &new.key).unwrap();
    std::fs::write(c.pki().join("web.crt"), &new.cert).unwrap();
    let want = fingerprint_of(&new.cert);
    assert_ne!(want, old);
    wait_for("the new certificate", 10, || {
        (probe(lo, port, None, &format!("127.0.0.1:{port}")).0 == want).then_some(())
    });
    assert!(
        c.mgmt_log().contains("changed: now serving it"),
        "{}",
        c.mgmt_log()
    );
}

fn tempfile_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hs-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
