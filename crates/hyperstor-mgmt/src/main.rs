//! `hyperstor-mgmt --listen ADDR --api ADDR --tls DIR [--metrics ADDR,ADDR…]`
//!
//! The management service (spec/administration.md §23.7), for now a
//! read-only web dashboard: health with its reasons, nodes, pools and
//! their PGs, and each storage node's I/O rates, latency and space.
//!
//! - It reads the cluster API at `--api` as its own identity, the `mgmt`
//!   certificate in `DIR` (`mgmt.crt`, `mgmt.key`, `root.crt`), and
//!   only reads.
//! - It reads each storage node's metrics port listed in `--metrics`.
//! - People aren't authenticated yet (that is AD2, with `glidex-auth`,
//!   D52), so it listens on loopback only and refuses any other address;
//!   reach it from elsewhere through an SSH tunnel.
//! - Browser protections as glidex's UI: a `Host` allowlist against DNS
//!   rebinding, GET only, a strict Content-Security-Policy, no framing.
//!
//! Exit status: 1 the certificates can't be read or the address can't be
//! bound, 2 usage.

use connectrpc::client::{ClientConfig, HttpClient};
use http_body_util::Full;
use hyper::body::Bytes;
use hyperstor_api::ClusterServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str =
    "usage: hyperstor-mgmt --listen ADDR --api ADDR --tls DIR [--metrics ADDR,ADDR...]";

const PAGE: &str = include_str!("dashboard.html");
const SCRIPT: &str = include_str!("dashboard.js");

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn fail(msg: &str) -> ! {
    eprintln!("hyperstor-mgmt: {msg}");
    std::process::exit(1)
}

struct State {
    api: ClusterServiceClient<HttpClient>,
    metrics: Vec<String>,
    port: u16,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    for a in args.iter().step_by(2) {
        if !matches!(a.as_str(), "--listen" | "--api" | "--tls" | "--metrics") {
            usage();
        }
    }
    let (Some(listen), Some(api), Some(tls)) = (get("--listen"), get("--api"), get("--tls")) else {
        usage()
    };
    let listen: SocketAddr = listen.parse().unwrap_or_else(|_| usage());
    if !listen.ip().is_loopback() {
        // No login yet: a non-loopback address would show the cluster to
        // anyone who can reach it.
        eprintln!(
            "hyperstor-mgmt: --listen must be a loopback address until people can log in (AD2)"
        );
        std::process::exit(2);
    }
    api.parse::<SocketAddr>().unwrap_or_else(|_| usage());
    let metrics: Vec<String> = get("--metrics")
        .map(|m| {
            m.split(',')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if metrics.iter().any(|m| m.parse::<SocketAddr>().is_err()) {
        usage();
    }
    let dir = std::path::PathBuf::from(tls);
    let read = |f: &str| {
        std::fs::read_to_string(dir.join(f))
            .unwrap_or_else(|e| fail(&format!("{}: {e}", dir.join(f).display())))
    };
    let tls = hyperstor_pki::client_config(&read("mgmt.crt"), &read("mgmt.key"), &read("root.crt"))
        .unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display())));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .unwrap_or_else(|e| fail(&format!("bind {listen}: {e}")));
        let port = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(listen.port());
        let uri: http::Uri = format!("https://{api}").parse().expect("uri");
        let state = Arc::new(State {
            api: ClusterServiceClient::new(
                HttpClient::with_tls(Arc::new(tls)),
                ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5)),
            ),
            metrics,
            port,
        });
        eprintln!("hyperstor-mgmt: dashboard on http://{listen}/ (read-only)");
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let state = state.clone();
                    async move { Ok::<_, std::convert::Infallible>(handle(&state, req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
}

type Resp = http::Response<Full<Bytes>>;

fn respond(status: u16, ctype: &str, body: impl Into<Bytes>) -> Resp {
    http::Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        )
        .header("X-Content-Type-Options", "nosniff")
        .header("X-Frame-Options", "DENY")
        .header("Referrer-Policy", "no-referrer")
        .header("Cache-Control", "no-store")
        .body(Full::new(body.into()))
        .expect("response")
}

/// Whether `host` (a `Host` header) names this machine's loopback on our
/// port: anything else may be DNS rebinding.
pub fn host_allowed(host: &str, port: u16) -> bool {
    // "[::1]:8852", "[::1]", "name:8852" or "name".
    let (name, rest) = match host.find(']') {
        Some(i) if host.starts_with('[') => host.split_at(i + 1),
        _ => match host.rsplit_once(':') {
            Some((n, p)) => (n, &host[n.len()..][..p.len() + 1]),
            None => (host, ""),
        },
    };
    let port_ok = match rest.strip_prefix(':') {
        Some(p) => p.parse::<u16>().is_ok_and(|p| p == port),
        None => rest.is_empty(),
    };
    matches!(name, "localhost" | "127.0.0.1" | "[::1]") && port_ok
}

async fn handle(state: &State, req: http::Request<hyper::body::Incoming>) -> Resp {
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host, state.port) {
        return respond(421, "text/plain", "misdirected request\n");
    }
    if req.method() != http::Method::GET && req.method() != http::Method::HEAD {
        return respond(405, "text/plain", "read-only\n");
    }
    let path = req.uri().path();
    let query = req.uri().query().unwrap_or("");
    match path {
        "/" => respond(200, "text/html; charset=utf-8", PAGE),
        "/app.js" => respond(200, "text/javascript; charset=utf-8", SCRIPT),
        "/health" => respond(200, "text/plain", "ok\n"),
        "/api/status" => match state.api.get_status(pb::GetStatusRequest::default()).await {
            Ok(r) => respond(
                200,
                "application/json",
                serde_json::to_string(&r.into_owned()).expect("json"),
            ),
            Err(e) => api_error(&e),
        },
        "/api/pgs" => {
            let pool = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("pool="))
                .and_then(|v| v.parse().ok());
            let Some(pool) = pool else {
                return respond(400, "text/plain", "pool=N is required\n");
            };
            match state
                .api
                .list_pgs(pb::ListPgsRequest {
                    pool,
                    ..Default::default()
                })
                .await
            {
                Ok(r) => respond(
                    200,
                    "application/json",
                    serde_json::to_string(&r.into_owned()).expect("json"),
                ),
                Err(e) => api_error(&e),
            }
        }
        "/api/metrics" => respond(200, "application/json", metrics_json(&state.metrics).await),
        _ => respond(404, "text/plain", "not found\n"),
    }
}

fn api_error(e: &connectrpc::ConnectError) -> Resp {
    let body = serde_json::json!({
        "error": format!("{:?}", e.code),
        "message": e.message.clone().unwrap_or_default(),
    });
    respond(502, "application/json", body.to_string())
}

/// Every node's metrics, parsed: `{"nodes": [{"addr", "up", "samples":
/// [[name, {label: value}, number], …]}]}`. A node that doesn't answer in
/// a second is `"up": false`.
async fn metrics_json(addrs: &[String]) -> String {
    let mut nodes = Vec::new();
    for addr in addrs {
        let text = tokio::time::timeout(Duration::from_secs(1), scrape(addr))
            .await
            .ok()
            .flatten();
        nodes.push(match text {
            Some(t) => serde_json::json!({"addr": addr, "up": true, "samples": parse_metrics(&t)}),
            None => serde_json::json!({"addr": addr, "up": false, "samples": []}),
        });
    }
    serde_json::json!({ "nodes": nodes }).to_string()
}

async fn scrape(addr: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .ok()?;
    let mut t = String::new();
    s.read_to_string(&mut t).await.ok()?;
    let (head, body) = t.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200").then(|| body.to_string())
}

/// Prometheus text format, the subset `hyperstord` writes: `name{k="v",…} value`.
pub fn parse_metrics(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let (series, value) = l.rsplit_once(' ')?;
            let value: f64 = value.parse().ok()?;
            let (name, labels) = match series.split_once('{') {
                Some((n, rest)) => (n, rest.strip_suffix('}')?),
                None => (series, ""),
            };
            let mut map = serde_json::Map::new();
            for kv in labels.split(',').filter(|s| !s.is_empty()) {
                let (k, v) = kv.split_once('=')?;
                map.insert(k.to_string(), v.trim_matches('"').into());
            }
            Some(serde_json::json!([name, map, value]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_names_on_our_port() {
        for ok in [
            "localhost",
            "localhost:8852",
            "127.0.0.1:8852",
            "[::1]:8852",
            "[::1]",
        ] {
            assert!(host_allowed(ok, 8852), "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:8852",
            "127.0.0.1:80",
            "localhost:8853",
            "",
            "10.0.0.1:8852",
        ] {
            assert!(!host_allowed(bad, 8852), "{bad}");
        }
    }

    #[test]
    fn metrics_parse_into_name_labels_value() {
        let v = parse_metrics(
            "# HELP x y\nhyperstor_requests_total{node=\"0\",device=\"0\",kind=\"write\"} 32\n\
             hyperstor_request_duration_seconds_bucket{node=\"0\",device=\"0\",kind=\"read\",le=\"+Inf\"} 4\nplain 1.5\n",
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[0][0], "hyperstor_requests_total");
        assert_eq!(v[0][1]["kind"], "write");
        assert_eq!(v[0][2], 32.0);
        assert_eq!(v[1][1]["le"], "+Inf");
        assert_eq!(v[2][0], "plain");
    }
}
