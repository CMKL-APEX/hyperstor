//! People reach hyperstor-mgmt on the host's addresses over HTTPS
//! (04-decisions.md D108–D110): the cluster CA's certificate, re-read when
//! its files change (an ACME client's renewals, D109), the node's
//! Tailscale certificate for its ts.net names with HSTS, and plain HTTP
//! sent to HTTPS; a request body no one reads leaves the connection
//! open (#329), a request's head must come within 10 s (I355), and odd
//! paths are answered (I371); the session cookie is `__Host-` prefixed
//! (I372).

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
/// A TLS connection to `ip`:`port`, naming `sni` (else the address).
fn connect(
    ip: std::net::IpAddr,
    port: u16,
    sni: Option<&str>,
) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
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
    rustls::StreamOwned::new(conn, tcp)
}

fn probe(ip: std::net::IpAddr, port: u16, sni: Option<&str>, host: &str) -> (String, String) {
    let mut s = connect(ip, port, sni);
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

/// Sends `head` and `body` on `s` and reads one answer: its status line
/// and headers, lower-cased, and its body.
fn exchange(s: &mut (impl Read + Write), head: &str, body: &[u8]) -> (String, Vec<u8>) {
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    s.flush().unwrap();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        assert_eq!(
            s.read(&mut byte).unwrap(),
            1,
            "closed: {:?}",
            String::from_utf8_lossy(&buf)
        );
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |n| n.trim().parse::<usize>().unwrap());
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).unwrap();
    (head, body)
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

    // A body no one reads (a POST to a page that doesn't exist, as
    // scanners send) leaves the connection open for the next request: a
    // proxy or tunnel that keeps connections reuses it (#329).
    let mut s = connect(lo, port, None);
    let junk = vec![b'x'; 100_000];
    let (head, _) = exchange(
        &mut s,
        &format!(
            "POST /no-such-page.php HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n",
            junk.len()
        ),
        &junk,
    );
    assert!(!head.contains("connection: close"), "{head}");
    let (head, _) = exchange(
        &mut s,
        &format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        b"",
    );
    assert!(head.starts_with("http/1.1 200"), "the next request: {head}");
    // A body said to be over the limit: refused unread, and the answer
    // says the connection closes.
    let (head, _) = exchange(
        &mut s,
        &format!(
            "POST /no-such-page.php HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n",
            64 << 20
        ),
        b"",
    );
    assert!(head.starts_with("http/1.1 413"), "{head}");
    assert!(head.contains("connection: close"), "{head}");
    // Over 256 KiB needs a person logged in: without a valid session or
    // token, refused unread, a made-up cookie included (#329's follow-up).
    for cookie in ["", "Cookie: __Host-hs_session=made-up\r\n"] {
        let mut s = connect(lo, port, None);
        let (head, _) = exchange(
            &mut s,
            &format!(
                "POST /no-such-page.php HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Content-Length: {}\r\n\r\n",
                300_000
            ),
            b"",
        );
        assert!(head.starts_with("http/1.1 413"), "{cookie}: {head}");
        assert!(head.contains("connection: close"), "{head}");
    }
    // People's paths with odd parts are answered, not dropped: a roles
    // path without a user's name is not found, and mgmt goes on serving.
    let ann = c.login("ann", Some(&c.claim_code())).unwrap();
    for odd in ["/auth/users/roles", "/auth/users//roles"] {
        let (code, head, b) = c.as_session(&ann, "PUT", odd, r#"{"roles":["viewer"]}"#);
        assert_eq!(code, 404, "{odd}: {head}{b}");
    }
    let (code, _, w) = c.as_session(&ann, "GET", "/auth/whoami", "");
    assert_eq!(code, 200, "{w}");

    // Over HTTPS the session cookie is `__Host-` prefixed: `Secure`,
    // `Path=/` and no `Domain`, as the prefix needs (I372). The plain
    // name of before is ignored; a logout clears the prefixed one.
    let set = &ann.set_cookie;
    assert!(set.starts_with("__Host-hs_session="), "{set}");
    for want in ["; Secure", "; Path=/", "; HttpOnly", "; SameSite=Strict"] {
        assert!(set.contains(want), "{want}: {set}");
    }
    assert!(!set.to_ascii_lowercase().contains("domain="), "{set}");
    let id = ann.cookie.split_once('=').unwrap().1;
    let old = format!("hs_session={id}");
    let (code, _, w) = c.web(
        "GET",
        "/auth/whoami",
        &c.dashboard_host(),
        &[("Cookie", &old)],
        "",
    );
    assert_eq!(code, 401, "the old name: {w}");
    let (code, head, _) = c.as_session(&ann, "POST", "/auth/logout", "");
    assert_eq!(code, 200, "{head}");
    assert!(
        head.lines().any(|l| l
            .to_ascii_lowercase()
            .starts_with("set-cookie: __host-hs_session=;")
            && l.contains("Max-Age=0")
            && l.contains("Secure")),
        "{head}"
    );
    let (code, _, _) = c.as_session(&ann, "GET", "/auth/whoami", "");
    assert_eq!(code, 401, "logged out");

    // A body that stops arriving: given up after 10 s.
    let mut s = connect(lo, port, None);
    let t = std::time::Instant::now();
    let (head, _) = exchange(
        &mut s,
        &format!(
            "POST /no-such-page.php HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1000\r\n\r\n"
        ),
        b"only some",
    );
    assert!(head.starts_with("http/1.1 408"), "{head}");
    assert!(head.contains("connection: close"), "{head}");
    assert!(
        t.elapsed() >= std::time::Duration::from_secs(9),
        "{:?}",
        t.elapsed()
    );

    // Connections that never finish a request's head are closed after
    // 10 s (I355): one that sends nothing, one that stops in its TLS
    // handshake, and a kept one trickling its next request's headers.
    let t = std::time::Instant::now();
    let silent = std::thread::spawn(move || {
        let mut tcp = std::net::TcpStream::connect((lo, port)).unwrap();
        let mut b = [0u8; 64];
        (tcp.read(&mut b).unwrap_or(0), t.elapsed())
    });
    let stalled = std::thread::spawn(move || {
        let mut tcp = std::net::TcpStream::connect((lo, port)).unwrap();
        // A TLS record's first bytes, then nothing.
        tcp.write_all(&[0x16, 0x03, 0x01]).unwrap();
        let mut b = [0u8; 64];
        (tcp.read(&mut b).unwrap_or(0), t.elapsed())
    });
    let mut s = connect(lo, port, None);
    let (head, _) = exchange(
        &mut s,
        &format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        b"",
    );
    assert!(head.starts_with("http/1.1 200"), "{head}");
    let trickled = std::time::Instant::now();
    let mut closed = false;
    for b in format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Slow: ")
        .bytes()
        .chain(std::iter::repeat(b'x'))
    {
        if s.write_all(&[b]).and_then(|_| s.flush()).is_err() {
            closed = true;
            break;
        }
        assert!(trickled.elapsed().as_secs() < 30, "still open");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(closed);
    let took = trickled.elapsed();
    assert!(took.as_secs() >= 9, "{took:?}");
    for (what, h) in [("silent", silent), ("stalled handshake", stalled)] {
        let (n, took) = h.join().unwrap();
        assert_eq!(n, 0, "{what}");
        assert!((9..20).contains(&took.as_secs()), "{what}: {took:?}");
    }

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
