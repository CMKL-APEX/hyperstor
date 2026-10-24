//! `hyperstor-mgmt --listen ADDR[,ADDR...] --api ADDR[,ADDR...] --tls DIR [--metrics ADDR,ADDR...] [--authd SOCKET] [--oidc FILE] [--web-tls DIR] [--tailscale SOCKET] [--hosts NAME,...] [--ui DIR]`
//!
//! The management service (spec/administration.md §23.7): the web
//! dashboard, and with `--authd`, people's logins and the cluster API on
//! their behalf.
//!
//! - It reads the cluster API at `--api` as its own identity, the `mgmt`
//!   certificate in `DIR` (`mgmt.crt`, `mgmt.key`, `root.crt`); with
//!   several members, every one, separated by commas: a call a member
//!   can't serve (not the leader) goes to the next.
//! - It reads each storage node's metrics port listed in `--metrics`.
//! - **Without `--authd`** it is the read-only dashboard of before, on
//!   loopback only, for no one is logged in.
//! - **With `--authd SOCKET`** (`hyperstor-authd`, D99) people log in with
//!   their password, and everything needs a session or an `hst_` token
//!   (§23.9.1); what they may do is decided by their roles
//!   ([`policy`]). People's accounts, sessions and tokens are rows of the
//!   map service's store (D98). While no one is a security administrator,
//!   a claim code is printed at start: whoever logs in with it becomes the
//!   first one.
//! - **With `--oidc FILE`** people log in with an identity provider too
//!   (or only, without `--authd`); see [`oidc`].
//! - `--web-tls DIR` serves HTTPS with `DIR/web.crt` and `web.key`;
//!   listening beyond loopback needs it and `--authd`. `--hosts` adds the
//!   names people reach it by to the `Host` allowlist.
//! - `--ui DIR` serves the web UI's files from `DIR` instead of the
//!   built-in dashboard (D100).
//! - Browser protections as glidex's UI: a `Host` allowlist against DNS
//!   rebinding, a strict Content-Security-Policy, no framing; cookies
//!   `HttpOnly; SameSite=Strict` (and `Secure` over HTTPS); writes with a
//!   cookie carry its CSRF value (`X-Hyperstor-CSRF`), and a cross-site
//!   `Origin` is refused.
//!
//! Exit status: 1 the certificates can't be read or the address can't be
//! bound, 2 usage.

// An `Err` here is the response to send, made once per request: its size
// costs nothing worth boxing it for.
#![allow(clippy::result_large_err)]

use connectrpc::client::{ClientConfig, HttpClient};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyperstor_api::ClusterServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage: hyperstor-mgmt --listen ADDR[,ADDR...] --api ADDR[,ADDR...] --tls DIR [--metrics ADDR,ADDR...] [--authd SOCKET] [--oidc FILE] [--web-tls DIR] [--tailscale SOCKET] [--hosts NAME,...] [--ui DIR]";

mod audit_ship;
mod oidc;
mod people;
mod policy;
mod proxy;
mod web;

const PAGE: &str = include_str!("dashboard.html");
const SCRIPT: &str = include_str!("dashboard.js");

/// The session cookie and the header its CSRF value comes back in.
const COOKIE: &str = "hs_session";
const CSRF_HEADER: &str = "x-hyperstor-csrf";
/// The PAM service people's passwords are checked with (D99).
const PAM_SERVICE: &str = "hyperstor";
/// Requests to mgmt itself are small; cluster API calls may be larger.
const MAX_BODY: usize = 4 << 20;

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn fail(msg: &str) -> ! {
    eprintln!("hyperstor-mgmt: {msg}");
    std::process::exit(1)
}

/// People's side: present with `--authd`.
struct Login {
    people: people::People,
    /// Passwords, through `hyperstor-authd`.
    authd: Option<PathBuf>,
    /// An identity provider.
    oidc: Option<oidc::Oidc>,
    engine: glidex_auth::cedar::Engine<policy::Hyperstor>,
}

struct State {
    api: ClusterServiceClient<HttpClient>,
    api_addrs: Vec<String>,
    tls: Arc<connectrpc::rustls::ClientConfig>,
    metrics: Vec<String>,
    port: u16,
    hosts: Vec<String>,
    https: bool,
    /// The Tailscale names served their certificate now (`--tailscale`).
    held: Option<Arc<glidex_tls::tailscale::Held>>,
    login: Option<Login>,
    ui: Option<PathBuf>,
}

fn main() {
    // glidex-tls's messages (a certificate not reloaded, Tailscale's).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    for a in args.iter().step_by(2) {
        if !matches!(
            a.as_str(),
            "--listen"
                | "--api"
                | "--tls"
                | "--metrics"
                | "--authd"
                | "--oidc"
                | "--web-tls"
                | "--tailscale"
                | "--hosts"
                | "--ui"
        ) {
            usage();
        }
    }
    let (Some(listen), Some(api), Some(tls)) = (get("--listen"), get("--api"), get("--tls")) else {
        usage()
    };
    let listen = glidex_tls::parse_addresses(&listen, "--listen").unwrap_or_else(|e| {
        eprintln!("hyperstor-mgmt: {e}");
        usage()
    });
    let tailscale = get("--tailscale").map(PathBuf::from);
    let authd = get("--authd").map(PathBuf::from);
    let oidc = get("--oidc").map(PathBuf::from);
    let web_tls = get("--web-tls").map(PathBuf::from);
    let loopback = glidex_tls::all_loopback(&listen);
    if !loopback && (authd.is_none() && oidc.is_none() || web_tls.is_none()) {
        // Without logins and HTTPS a non-loopback address would show the
        // cluster to anyone who can reach it.
        eprintln!(
            "hyperstor-mgmt: --listen beyond loopback needs --web-tls, and --authd or --oidc"
        );
        std::process::exit(2);
    }
    let oidc = oidc.map(|f| oidc::load(&f).unwrap_or_else(|e| fail(&format!("--oidc {e}"))));
    let api_addrs: Vec<String> = api
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if api_addrs.is_empty() || api_addrs.iter().any(|a| a.parse::<SocketAddr>().is_err()) {
        usage();
    }
    let list = |flag: &str| -> Vec<String> {
        get(flag)
            .map(|m| {
                m.split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let metrics = list("--metrics");
    if metrics.iter().any(|m| m.parse::<SocketAddr>().is_err()) {
        usage();
    }
    let mut hosts = list("--hosts");
    if !loopback {
        // Reached by the host's own names and addresses (D110).
        hosts.extend(glidex_tls::LocalNames::discover().hosts());
    }
    if tailscale.is_some() && web_tls.is_none() {
        eprintln!("hyperstor-mgmt: --tailscale needs --web-tls (the certificate for other names)");
        std::process::exit(2);
    }
    let dir = PathBuf::from(tls);
    let read = |d: &std::path::Path, f: &str| {
        std::fs::read_to_string(d.join(f))
            .unwrap_or_else(|e| fail(&format!("{}: {e}", d.join(f).display())))
    };
    let tls = Arc::new(
        hyperstor_pki::client_config(
            &read(&dir, "mgmt.crt"),
            &read(&dir, "mgmt.key"),
            &read(&dir, "root.crt"),
        )
        .unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display()))),
    );
    let ui = get("--ui").map(PathBuf::from);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let web = web_tls
            .as_ref()
            .map(|d| web::start(d, tailscale).unwrap_or_else(|e| fail(&format!("--web-tls {e}"))));
        let mut listeners = Vec::new();
        for a in &listen {
            match glidex_tls::bind(*a) {
                Ok(l) => listeners.push(l),
                // A host without IPv6 still serves on IPv4.
                Err(e) if glidex_tls::ipv6_unavailable(a, &e) => {
                    eprintln!("hyperstor-mgmt: {a}: no IPv6 here ({e}); IPv4 only")
                }
                Err(e) => fail(&format!("bind {a}: {e}")),
            }
        }
        if listeners.is_empty() {
            fail("nothing to listen on");
        }
        let port = listeners[0]
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(listen[0].port());
        let uri: http::Uri = format!("https://{}", api_addrs[0]).parse().expect("uri");
        let login = (authd.is_some() || oidc.is_some()).then(|| Login {
            people: people::People::new(people::Rows::api(&api_addrs, tls.clone())),
            authd,
            oidc,
            engine: policy::engine(),
        });
        let state = Arc::new(State {
            api: ClusterServiceClient::new(
                HttpClient::with_tls(tls.clone()),
                ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5)),
            ),
            api_addrs,
            tls,
            metrics,
            port,
            hosts,
            https: web.is_some(),
            held: web.as_ref().and_then(|w| w.held.clone()),
            login,
            ui,
        });
        let scheme = if state.https { "https" } else { "http" };
        let at: Vec<String> = listen.iter().map(|a| format!("{scheme}://{a}/")).collect();
        let at = at.join(" ");
        if state.login.is_some() {
            eprintln!("hyperstor-mgmt: on {at} (people log in)");
            tokio::spawn(offer_claim(state.clone(), at));
        } else {
            eprintln!("hyperstor-mgmt: dashboard on {at} (read-only)");
        }
        if state.login.is_some() {
            tokio::spawn(audit_ship::ship(state.api_addrs.clone(), state.tls.clone()));
        }
        let acceptor = web.map(|w| tokio_rustls::TlsAcceptor::from(w.config));
        let mut tasks = tokio::task::JoinSet::new();
        for listener in listeners {
            let (state, acceptor) = (state.clone(), acceptor.clone());
            tasks.spawn(serve(listener, state, acceptor));
        }
        while tasks.join_next().await.is_some() {}
    });
}

/// Accepts connections on `listener` for good.
async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<State>,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let builder = hyper::server::conn::http1::Builder::new();
            let Some(a) = acceptor else {
                let svc = hyper::service::service_fn(move |req| {
                    let state = state.clone();
                    async move { Ok::<_, std::convert::Infallible>(handle(&state, req, peer).await) }
                });
                let _ = builder
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
                return;
            };
            match web::starts_tls(&stream).await {
                Some(true) => {}
                Some(false) => {
                    // Plain HTTP on the HTTPS port: to HTTPS.
                    let svc = hyper::service::service_fn(|req| async move {
                        Ok::<_, std::convert::Infallible>(web::to_https(&req))
                    });
                    let _ = builder
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                        .await;
                    return;
                }
                None => return,
            }
            let Ok(tls) = a.accept(stream).await else {
                return;
            };
            // Served a Tailscale certificate: a browser trusts it, so HSTS.
            let hsts = tls
                .get_ref()
                .1
                .server_name()
                .is_some_and(|n| state.held.as_ref().is_some_and(|h| h.contains(n)));
            let svc = hyper::service::service_fn(move |req| {
                let state = state.clone();
                async move {
                    let mut r = handle(&state, req, peer).await;
                    if hsts {
                        r.headers_mut().insert(
                            http::header::STRICT_TRANSPORT_SECURITY,
                            http::HeaderValue::from_static("max-age=86400"),
                        );
                    }
                    Ok::<_, std::convert::Infallible>(r)
                }
            });
            let _ = builder
                .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                .await;
        });
    }
}

/// While no one is a security administrator, prints a claim code: whoever
/// logs in with it becomes the first (§23.7). Waits for the map service
/// as long as it takes (a new group of members has no quorum until the
/// others join), asking again every second, then every 30 s.
async fn offer_claim(s: Arc<State>, at: String) {
    let Some(l) = &s.login else { return };
    for i in 0u32.. {
        match l.people.claimed().await {
            Ok(true) => return,
            Ok(false) => {
                if let Ok(code) = l.people.new_claim().await {
                    eprintln!(
                        "hyperstor-mgmt: no security administrator yet. Log in at {at} with the claim code {code} (valid 1 h) to become the first."
                    );
                    return;
                }
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(if i < 120 { 1 } else { 30 })).await;
    }
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

fn json_resp(status: u16, v: serde_json::Value) -> Resp {
    respond(status, "application/json", v.to_string())
}

/// A refusal as the Connect protocol writes one (`code`, `message`, and
/// the reason as an `ErrorInfo` detail, as the cluster API's own), so
/// `hyperstorctl` and the UI read mgmt's refusals and the API's alike;
/// `reason` is also a field of its own, for browsers.
fn error(status: u16, reason: &str, message: &str) -> Resp {
    use connectrpc::ConnectError as E;
    let e = match status {
        400 => E::invalid_argument(message),
        401 => E::unauthenticated(message),
        403 => E::permission_denied(message),
        404 => E::not_found(message),
        409 => E::already_exists(message),
        413 | 429 => E::resource_exhausted(message),
        503 => E::unavailable(message),
        _ => E::internal(message),
    };
    let e = hyperstor_api::with_reason(e, reason);
    let mut v: serde_json::Value = serde_json::from_slice(&e.to_json()).expect("json");
    v["reason"] = reason.into();
    json_resp(status, v)
}

/// One audit record (§23.9.4) for what a person did, on standard error.
fn audit(principal: &str, op: &str, target: &str, result: &str) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let line = serde_json::json!({
        "audit": 1, "time": format!("{}.{:03}", t.as_secs(), t.subsec_millis()),
        "principal": principal, "op": op, "target": target, "result": result,
    })
    .to_string();
    eprintln!("{line}");
    // And into the cluster's audit log in .meta (D112).
    audit_ship::push(line);
}

/// Whether `host` (a `Host` header) names this machine's loopback, or one
/// of `extra`, on our port: anything else may be DNS rebinding.
pub fn host_allowed(host: &str, port: u16, extra: &[String]) -> bool {
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
    (matches!(name, "localhost" | "127.0.0.1" | "[::1]") || extra.iter().any(|e| e == name))
        && port_ok
}

fn cookie_of(req: &http::Request<hyper::body::Incoming>) -> Option<String> {
    req.headers()
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(String::from)
        })
}

fn bearer_of(req: &http::Request<hyper::body::Incoming>) -> Option<String> {
    req.headers()
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|t| t.trim().to_string())
}

async fn body_of(
    req: http::Request<hyper::body::Incoming>,
) -> Result<(http::request::Parts, Bytes), Resp> {
    let (parts, body) = req.into_parts();
    match Limited::new(body, MAX_BODY).collect().await {
        Ok(b) => Ok((parts, b.to_bytes())),
        Err(_) => Err(error(413, "too_large", "request too large")),
    }
}

fn set_cookie(s: &State, value: &str, max_age: Option<u64>) -> String {
    let mut c = format!("{COOKIE}={value}; HttpOnly; SameSite=Strict; Path=/");
    if s.https {
        c.push_str("; Secure");
    }
    if let Some(a) = max_age {
        c.push_str(&format!("; Max-Age={a}"));
    }
    c
}

async fn handle(
    state: &State,
    req: http::Request<hyper::body::Incoming>,
    peer: SocketAddr,
) -> Resp {
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let tailscale_name = || {
        state.held.as_ref().is_some_and(|h| {
            let (name, port) = host.rsplit_once(':').unwrap_or((&host, ""));
            (port.is_empty() || port.parse::<u16>().is_ok_and(|p| p == state.port))
                && h.contains(name)
        })
    };
    if !host_allowed(&host, state.port, &state.hosts) && !tailscale_name() {
        return respond(421, "text/plain", "misdirected request\n");
    }
    let path = req.uri().path().to_string();
    let read = req.method() == http::Method::GET || req.method() == http::Method::HEAD;
    match &state.login {
        None => {
            if !read {
                return respond(405, "text/plain", "read-only\n");
            }
            dashboard(state, &path, req.uri().query().unwrap_or("")).await
        }
        Some(l) => {
            // A cross-site page may not drive a logged-in browser.
            if !read
                && let Some(o) = req
                    .headers()
                    .get(http::header::ORIGIN)
                    .and_then(|o| o.to_str().ok())
            {
                let scheme = if state.https { "https" } else { "http" };
                if o != format!("{scheme}://{host}") {
                    return error(403, "bad_origin", "cross-site request refused");
                }
            }
            with_login(state, l, req, &path, read, peer).await
        }
    }
}

/// The built-in dashboard, or the UI's files with `--ui`.
async fn dashboard(state: &State, path: &str, query: &str) -> Resp {
    match path {
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
        _ => match &state.ui {
            Some(dir) => ui_file(dir, path),
            None => match path {
                "/" => respond(200, "text/html; charset=utf-8", PAGE),
                "/app.js" => respond(200, "text/javascript; charset=utf-8", SCRIPT),
                _ => respond(404, "text/plain", "not found\n"),
            },
        },
    }
}

/// A file of the UI under `dir`; `index.html` for anything without an
/// extension (the UI's own routes). Never outside `dir`.
fn ui_file(dir: &std::path::Path, path: &str) -> Resp {
    let rel = path.trim_start_matches('/');
    if rel.split('/').any(|p| p == ".." || p.starts_with('.')) || rel.contains('\\') {
        return respond(404, "text/plain", "not found\n");
    }
    let has_ext = rel.rsplit('/').next().is_some_and(|f| f.contains('.'));
    let file = if rel.is_empty() || !has_ext {
        dir.join("index.html")
    } else {
        dir.join(rel)
    };
    let ctype = match file.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    match std::fs::read(&file) {
        Ok(b) => respond(200, ctype, b),
        Err(_) => respond(404, "text/plain", "not found\n"),
    }
}

fn people_error(e: people::Error) -> Resp {
    match e {
        people::Error::Unauthenticated => error(401, "unauthenticated", "log in first"),
        people::Error::Invalid(m) => error(400, "invalid_argument", &m),
        people::Error::NotFound => error(404, "not_found", "not found"),
        people::Error::Exists => error(409, "already_exists", "already exists"),
        people::Error::Unavailable(m) => error(503, "unavailable", &m),
    }
}

/// Checks `action` for `p`: `Err` with the response to send if refused.
fn authorize(l: &Login, p: &people::Principal, action: &str, target: &str) -> Result<(), Resp> {
    let d = policy::allowed(&l.engine, &p.user, &p.roles, action, &p.method, p.age_secs);
    if d.allowed {
        return Ok(());
    }
    let step_up = policy::action(action).is_some_and(|(_, s)| s) && d.denied_by("base.step-up");
    audit(&format!("user:{}", p.user), action, target, "refused");
    Err(if step_up {
        error(
            401,
            "reauth_required",
            "log in again to do this (a login of the last 10 minutes)",
        )
    } else {
        error(
            403,
            "permission_denied",
            &format!("{} may not do {action}", p.user),
        )
    })
}

#[derive(serde::Deserialize)]
struct LoginReq {
    user: String,
    password: String,
    #[serde(default)]
    claim: Option<String>,
}

async fn check_password(l: &Login, user: &str, password: &str) -> Result<(), Resp> {
    let Some(authd) = &l.authd else {
        return Err(error(404, "not_found", "password logins are off here"));
    };
    let client = glidex_authd::client::AuthdClient::new(authd.clone());
    let (u, pw) = (
        user.to_string(),
        zeroize::Zeroizing::new(password.to_string()),
    );
    let r = tokio::task::spawn_blocking(move || client.authenticate(&u, &pw, PAM_SERVICE))
        .await
        .map_err(|e| error(500, "internal", &e.to_string()))?;
    match r {
        Ok(_) => Ok(()),
        Err(glidex_authd::client::AuthdError::Denied) => {
            Err(error(401, "login_failed", "wrong user name or password"))
        }
        Err(glidex_authd::client::AuthdError::RateLimited) => Err(error(
            429,
            "rate_limited",
            "too many failed attempts; try again later",
        )),
        Err(e) => Err(error(503, "unavailable", &e.to_string())),
    }
}

/// A session for a person just authenticated by `method` (`user` as the
/// login found them): the user's name, roles, CSRF value and cookie, or
/// the refusal. Audited as `who`.
async fn start_session(
    state: &State,
    l: &Login,
    user: Result<people::User, people::Error>,
    method: &str,
    claim: bool,
    who: &str,
) -> Result<(String, Vec<String>, String, String), Resp> {
    let op = if claim { "Claim" } else { "Login" };
    let user = match user {
        Ok(u) if !u.disabled => u,
        Ok(_) => {
            audit(who, op, "mgmt", "refused: disabled");
            return Err(error(403, "disabled", "this account is disabled"));
        }
        Err(e) => {
            audit(who, op, "mgmt", &format!("refused: {e}"));
            return Err(people_error(e));
        }
    };
    let s = l
        .people
        .new_session(&user.name, method)
        .await
        .map_err(people_error)?;
    audit(who, op, "mgmt", &format!("ok, {method}"));
    Ok((
        user.name,
        user.roles,
        s.csrf,
        set_cookie(state, &s.id, None),
    ))
}

/// Where the provider sends this browser back: as configured, else on the
/// name and scheme it reached us by.
fn redirect_uri(state: &State, o: &oidc::Oidc, host: &str) -> String {
    let scheme = if state.https { "https" } else { "http" };
    o.redirect_uri(Some(&format!("{scheme}://{host}")))
}

fn host_of<B>(req: &http::Request<B>) -> String {
    req.headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[derive(serde::Deserialize, Default)]
struct OidcStartReq {
    #[serde(default)]
    return_to: Option<String>,
    #[serde(default)]
    claim: Option<String>,
    /// Ask the provider for a fresh login (step-up).
    #[serde(default)]
    reauth: bool,
}

/// `POST /auth/oidc/start {return_to?, claim?, reauth?}`: where to send
/// the browser (`{"redirect": URL}`), and the cookie binding the login to
/// it.
async fn oidc_start(state: &State, l: &Login, req: http::Request<hyper::body::Incoming>) -> Resp {
    let Some(o) = &l.oidc else {
        return error(404, "not_found", "identity provider logins are off here");
    };
    let host = host_of(&req);
    let (_, body) = match body_of(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let r: OidcStartReq = if body.is_empty() {
        OidcStartReq::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(_) => {
                return error(
                    400,
                    "invalid_argument",
                    "expected {return_to, claim, reauth}",
                );
            }
        }
    };
    let after = oidc::After { claim: r.claim };
    match o
        .start(
            r.return_to.as_deref(),
            &redirect_uri(state, o, &host),
            r.reauth,
            after,
        )
        .await
    {
        Ok(s) => {
            let mut resp = json_resp(200, serde_json::json!({ "redirect": s.redirect }));
            let mut c = format!(
                "{}={}; HttpOnly; SameSite=Lax; Path={}; Max-Age={}",
                oidc::BROWSER_COOKIE,
                s.browser,
                oidc::CALLBACK,
                glidex_auth::oidc::STATE_SECS
            );
            if state.https {
                c.push_str("; Secure");
            }
            resp.headers_mut()
                .insert(http::header::SET_COOKIE, c.parse().expect("cookie"));
            resp
        }
        Err(e) => error(503, "unavailable", &e.to_string()),
    }
}

/// `GET /auth/oidc/callback?code&state`: the provider sends the browser
/// back; a session, then on to where the login began.
async fn oidc_callback(
    state: &State,
    l: &Login,
    req: http::Request<hyper::body::Incoming>,
    peer: SocketAddr,
) -> Resp {
    let Some(o) = &l.oidc else {
        return error(404, "not_found", "identity provider logins are off here");
    };
    let q: std::collections::HashMap<String, String> = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect();
    let failed = |m: &str| {
        audit(
            &format!("oidc from {}", peer.ip()),
            "Login",
            "mgmt",
            &format!("refused: {m}"),
        );
        respond(
            403,
            "text/plain; charset=utf-8",
            format!("The login failed: {m}\nBack to the start: /\n"),
        )
    };
    if let Some(e) = q.get("error") {
        return failed(&format!("the identity provider said {e}"));
    }
    let (Some(code), Some(st)) = (q.get("code"), q.get("state")) else {
        return failed("no code or state");
    };
    let browser = req
        .headers()
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{}=", oidc::BROWSER_COOKIE))
                .map(String::from)
        });
    let host = host_of(&req);
    let (id, return_to, after) = match o
        .callback(code, st, browser.as_deref(), &redirect_uri(state, o, &host))
        .await
    {
        Ok(v) => v,
        Err(e) => return failed(&e.error.to_string()),
    };
    let user = l
        .people
        .oidc_user(
            &id.issuer,
            &id.subject,
            id.email.as_deref(),
            id.email_verified,
        )
        .await;
    let user = match (user, &after.claim) {
        (Ok(u), Some(code)) => l.people.claim(code, &u.name).await,
        (u, _) => u,
    };
    let user = match user {
        Ok(u) => Ok(u),
        Err(e) => return failed(&e.to_string()),
    };
    let who = format!(
        "user:{} from {}",
        user.as_ref().map_or("?", |u| u.name.as_str()),
        peer.ip()
    );
    match start_session(state, l, user, "oidc", after.claim.is_some(), &who).await {
        Ok((_, _, _, cookie)) => {
            let mut resp = http::Response::builder()
                .status(303)
                .header(http::header::LOCATION, return_to)
                .header("Cache-Control", "no-store")
                .header(http::header::SET_COOKIE, cookie)
                .header(
                    http::header::SET_COOKIE,
                    format!(
                        "{}=; Path={}; Max-Age=0",
                        oidc::BROWSER_COOKIE,
                        oidc::CALLBACK
                    ),
                )
                .body(Full::new(Bytes::new()))
                .expect("response");
            resp.headers_mut()
                .insert("Referrer-Policy", "no-referrer".parse().expect("header"));
            resp
        }
        Err(r) => r,
    }
}

/// `%XX` and `+` of a query value.
fn percent_decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(x) => {
                        out.push(x);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn with_login(
    state: &State,
    l: &Login,
    req: http::Request<hyper::body::Incoming>,
    path: &str,
    read: bool,
    peer: SocketAddr,
) -> Resp {
    let method = req.method().clone();
    let cookie = cookie_of(&req);
    let bearer = bearer_of(&req);
    let csrf = req
        .headers()
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    // Before a login.
    match (&method, path) {
        (_, "/health") => return respond(200, "text/plain", "ok\n"),
        (&http::Method::GET, "/auth/methods") => {
            let open = l.people.claimed().await.map(|c| !c).unwrap_or(false);
            return json_resp(
                200,
                serde_json::json!({
                    "pam": l.authd.is_some(), "oidc": l.oidc.is_some(), "claim_open": open,
                }),
            );
        }
        (&http::Method::POST, "/auth/login") => {
            let (_, body) = match body_of(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let Ok(r) = serde_json::from_slice::<LoginReq>(&body) else {
                return error(400, "invalid_argument", "expected {user, password}");
            };
            let who = format!("user:{} from {}", r.user, peer.ip());
            if let Err(resp) = check_password(l, &r.user, &r.password).await {
                audit(&who, "Login", "mgmt", "refused");
                return resp;
            }
            let user = match &r.claim {
                Some(code) => l.people.claim(code, &r.user).await,
                None => l.people.logged_in_user(&r.user).await,
            };
            return match start_session(state, l, user, "pam", r.claim.is_some(), &who).await {
                Ok((user, roles, csrf, cookie)) => {
                    let mut resp = json_resp(
                        200,
                        serde_json::json!({ "user": user, "roles": roles, "csrf": csrf }),
                    );
                    resp.headers_mut()
                        .insert(http::header::SET_COOKIE, cookie.parse().expect("cookie"));
                    resp
                }
                Err(r) => r,
            };
        }
        (&http::Method::POST, "/auth/oidc/start") => {
            return oidc_start(state, l, req).await;
        }
        (&http::Method::GET, oidc::CALLBACK) => {
            return oidc_callback(state, l, req, peer).await;
        }
        _ => {}
    }
    // Everything else needs a person: a session cookie or a token.
    let p = match (&bearer, &cookie) {
        (Some(t), _) => l.people.token(t).await,
        (None, Some(c)) => l.people.session(c).await,
        (None, None) => Err(people::Error::Unauthenticated),
    };
    let p = match p {
        Ok(p) => p,
        Err(e) => {
            if path.starts_with("/api/") || path.starts_with("/auth/") {
                return people_error(e);
            }
            // Pages load; their calls then ask for a login.
            return dashboard(state, path, "").await;
        }
    };
    // A write with a cookie carries the session's CSRF value.
    if !read
        && bearer.is_none()
        && !p
            .csrf
            .as_deref()
            .zip(csrf.as_deref())
            .is_some_and(|(a, b)| glidex_auth::secrets::constant_eq(a, b))
    {
        return error(403, "csrf", "missing or wrong CSRF value");
    }
    let who = format!("user:{}", p.user);
    let query = req.uri().query().unwrap_or("").to_string();
    match (&method, path) {
        (&http::Method::GET, "/auth/whoami") => json_resp(
            200,
            serde_json::json!({ "user": p.user, "roles": p.roles, "method": p.method, "csrf": p.csrf }),
        ),
        (&http::Method::POST, "/auth/logout") => {
            if let Some(c) = &cookie {
                let _ = l.people.end_session(c).await;
            }
            audit(&who, "Logout", "mgmt", "ok");
            let mut resp = json_resp(200, serde_json::json!({}));
            resp.headers_mut().insert(
                http::header::SET_COOKIE,
                set_cookie(state, "", Some(0)).parse().expect("cookie"),
            );
            resp
        }
        (&http::Method::GET, "/auth/tokens") => {
            if let Err(r) = authorize(l, &p, "People.ListTokens", "own") {
                return r;
            }
            match l.people.tokens_of(&p.user).await {
                Ok(t) => json_resp(200, serde_json::json!({ "tokens": t })),
                Err(e) => people_error(e),
            }
        }
        (&http::Method::POST, "/auth/tokens") => {
            if let Err(r) = authorize(l, &p, "People.CreateToken", "own") {
                return r;
            }
            let (_, body) = match body_of(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let name = v["name"].as_str().unwrap_or("").to_string();
            let days = v["days"].as_u64();
            match l.people.create_token(&p.user, &name, days).await {
                Ok((secret, t)) => {
                    audit(&who, "People.CreateToken", &format!("token {}", t.id), "ok");
                    json_resp(200, serde_json::json!({ "token": secret, "info": t }))
                }
                Err(e) => people_error(e),
            }
        }
        (&http::Method::DELETE, p2) if p2.starts_with("/auth/tokens/") => {
            if let Err(r) = authorize(l, &p, "People.RevokeToken", "own") {
                return r;
            }
            let id = &p2["/auth/tokens/".len()..];
            match l.people.revoke_token(&p.user, id).await {
                Ok(()) => {
                    audit(&who, "People.RevokeToken", &format!("token {id}"), "ok");
                    json_resp(200, serde_json::json!({}))
                }
                Err(e) => people_error(e),
            }
        }
        (&http::Method::GET, "/auth/users") => {
            if let Err(r) = authorize(l, &p, "People.ListUsers", "people") {
                return r;
            }
            match l.people.users().await {
                Ok(u) => json_resp(200, serde_json::json!({ "users": u })),
                Err(e) => people_error(e),
            }
        }
        (&http::Method::PUT, p2) if p2.starts_with("/auth/users/") && p2.ends_with("/roles") => {
            let name = p2["/auth/users/".len()..p2.len() - "/roles".len()].to_string();
            if let Err(r) = authorize(l, &p, "People.SetRoles", &format!("user {name}")) {
                return r;
            }
            let (_, body) = match body_of(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let roles: Vec<String> = v["roles"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            match l.people.set_roles(&name, &roles).await {
                Ok(u) => {
                    audit(
                        &who,
                        "People.SetRoles",
                        &format!("user {name}"),
                        &format!("ok, {:?}", u.roles),
                    );
                    json_resp(200, serde_json::json!({ "user": u }))
                }
                Err(e) => people_error(e),
            }
        }
        (&http::Method::DELETE, p2) if p2.starts_with("/auth/users/") => {
            let name = p2["/auth/users/".len()..].to_string();
            if let Err(r) = authorize(l, &p, "People.DeleteUser", &format!("user {name}")) {
                return r;
            }
            match l.people.delete_user(&name).await {
                Ok(()) => {
                    audit(&who, "People.DeleteUser", &format!("user {name}"), "ok");
                    json_resp(200, serde_json::json!({}))
                }
                Err(e) => people_error(e),
            }
        }
        // The cluster API, on the person's behalf.
        (&http::Method::POST, p2) if p2.starts_with("/api/hyperstor.v1.") => {
            let rpc = &p2["/api/".len()..];
            let Some((service, m)) = rpc
                .strip_prefix("hyperstor.v1.")
                .and_then(|r| r.split_once('/'))
            else {
                return error(404, "not_found", "no such call");
            };
            let action = format!("{service}.{m}");
            if let Err(r) = authorize(l, &p, &action, "cluster") {
                return r;
            }
            let (parts, body) = match body_of(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let r = proxy::forward(
                &state.tls,
                &state.api_addrs,
                &rpc_path(rpc),
                &parts.headers,
                &who,
                body,
            )
            .await;
            let read_only = matches!(policy::action(&action), Some((policy::Group::Read, _)));
            match r {
                Ok(a) => {
                    if !read_only {
                        audit(&who, &action, "cluster", &format!("status {}", a.status));
                    }
                    let mut resp = http::Response::builder().status(a.status);
                    for (k, v) in a.headers.iter() {
                        if k == http::header::CONTENT_TYPE
                            || k.as_str().starts_with("grpc-")
                            || k.as_str().starts_with("connect-")
                        {
                            resp = resp.header(k, v);
                        }
                    }
                    resp.header("Cache-Control", "no-store")
                        .header("X-Content-Type-Options", "nosniff")
                        .body(Full::new(a.body))
                        .expect("response")
                }
                Err(e) => error(503, "unavailable", &e),
            }
        }
        (&http::Method::GET, _) | (&http::Method::HEAD, _) => {
            if (path.starts_with("/api/") || path == "/health")
                && let Err(r) = authorize(l, &p, "Dashboard.Read", "dashboard")
            {
                return r;
            }
            dashboard(state, path, &query).await
        }
        _ => respond(405, "text/plain", "method not allowed\n"),
    }
}

/// The cluster API's path for a call under `/api/`.
fn rpc_path(rpc: &str) -> String {
    format!("/{rpc}")
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
            assert!(host_allowed(ok, 8852, &[]), "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:8852",
            "127.0.0.1:80",
            "localhost:8853",
            "",
            "10.0.0.1:8852",
        ] {
            assert!(!host_allowed(bad, 8852, &[]), "{bad}");
        }
    }

    /// mgmt's refusals read as the API's: a Connect error whose reason
    /// `hyperstorctl` finds.
    #[test]
    fn refusals_are_connect_errors_with_reasons() {
        use http_body_util::BodyExt;
        let r = error(401, "reauth_required", "log in again");
        assert_eq!(r.status(), 401);
        let body = block_on(r.into_body().collect()).unwrap().to_bytes();
        let e: connectrpc::ConnectError = serde_json::from_slice(&body).unwrap();
        assert_eq!(e.code, connectrpc::ErrorCode::Unauthenticated);
        assert_eq!(
            hyperstor_api::reason_of(&e).as_deref(),
            Some("reauth_required"),
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%41"), "A");
    }

    #[test]
    fn named_hosts_join_the_allowlist() {
        let names = vec!["mgmt.example".to_string()];
        assert!(host_allowed("mgmt.example:8852", 8852, &names));
        assert!(!host_allowed("other.example:8852", 8852, &names));
        assert!(!host_allowed("mgmt.example:80", 8852, &names));
    }

    #[test]
    fn ui_files_stay_in_their_directory() {
        let d = std::env::temp_dir().join(format!("hs-ui-{}", std::process::id()));
        std::fs::create_dir_all(d.join("assets")).unwrap();
        std::fs::write(d.join("index.html"), "<html>").unwrap();
        std::fs::write(d.join("assets/a.js"), "x").unwrap();
        assert_eq!(ui_file(&d, "/").status(), 200);
        assert_eq!(ui_file(&d, "/volumes").status(), 200, "the UI's own route");
        assert_eq!(ui_file(&d, "/assets/a.js").status(), 200);
        assert_eq!(ui_file(&d, "/../etc/passwd").status(), 404);
        assert_eq!(ui_file(&d, "/.hidden").status(), 404);
        assert_eq!(ui_file(&d, "/assets/none.js").status(), 404);
        let _ = std::fs::remove_dir_all(&d);
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
