//! `hyperstor-mgmt --listen ADDR --api ADDR[,ADDR...] --tls DIR [--metrics ADDR,ADDR...] [--authd SOCKET] [--web-tls DIR] [--hosts NAME,...] [--ui DIR]`
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

const USAGE: &str = "usage: hyperstor-mgmt --listen ADDR --api ADDR[,ADDR...] --tls DIR [--metrics ADDR,ADDR...] [--authd SOCKET] [--web-tls DIR] [--hosts NAME,...] [--ui DIR]";

mod people;
mod policy;
mod proxy;

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
    authd: PathBuf,
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
    login: Option<Login>,
    ui: Option<PathBuf>,
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
        if !matches!(
            a.as_str(),
            "--listen"
                | "--api"
                | "--tls"
                | "--metrics"
                | "--authd"
                | "--web-tls"
                | "--hosts"
                | "--ui"
        ) {
            usage();
        }
    }
    let (Some(listen), Some(api), Some(tls)) = (get("--listen"), get("--api"), get("--tls")) else {
        usage()
    };
    let listen: SocketAddr = listen.parse().unwrap_or_else(|_| usage());
    let authd = get("--authd").map(PathBuf::from);
    let web_tls = get("--web-tls").map(PathBuf::from);
    if !listen.ip().is_loopback() && (authd.is_none() || web_tls.is_none()) {
        // Without logins and HTTPS a non-loopback address would show the
        // cluster to anyone who can reach it.
        eprintln!("hyperstor-mgmt: --listen beyond loopback needs --authd and --web-tls");
        std::process::exit(2);
    }
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
    let hosts = list("--hosts");
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
    let acceptor = web_tls.as_ref().map(|d| {
        let config = hyperstor_pki::web_server_config(&read(d, "web.crt"), &read(d, "web.key"))
            .unwrap_or_else(|e| fail(&format!("{}: {e}", d.display())));
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    });
    let ui = get("--ui").map(PathBuf::from);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .unwrap_or_else(|e| fail(&format!("bind {listen}: {e}")));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(listen.port());
        let uri: http::Uri = format!("https://{}", api_addrs[0]).parse().expect("uri");
        let login = authd.map(|authd| Login {
            people: people::People::new(people::Rows::api(&api_addrs, tls.clone())),
            authd,
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
            https: acceptor.is_some(),
            login,
            ui,
        });
        let scheme = if state.https { "https" } else { "http" };
        if state.login.is_some() {
            eprintln!("hyperstor-mgmt: on {scheme}://{listen}/ (people log in)");
            tokio::spawn(offer_claim(state.clone(), format!("{scheme}://{listen}/")));
        } else {
            eprintln!("hyperstor-mgmt: dashboard on {scheme}://{listen}/ (read-only)");
        }
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let state = state.clone();
                    async move { Ok::<_, std::convert::Infallible>(handle(&state, req, peer).await) }
                });
                let builder = hyper::server::conn::http1::Builder::new();
                match acceptor {
                    Some(a) => {
                        if let Ok(s) = a.accept(stream).await {
                            let _ = builder
                                .serve_connection(hyper_util::rt::TokioIo::new(s), svc)
                                .await;
                        }
                    }
                    None => {
                        let _ = builder
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                            .await;
                    }
                }
            });
        }
    });
}

/// While no one is a security administrator, prints a claim code: whoever
/// logs in with it becomes the first (§23.7). Waits for the map service
/// while it starts.
async fn offer_claim(s: Arc<State>, at: String) {
    let Some(l) = &s.login else { return };
    for _ in 0..120 {
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
        tokio::time::sleep(Duration::from_secs(1)).await;
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

fn error(status: u16, code: &str, message: &str) -> Resp {
    json_resp(
        status,
        serde_json::json!({ "code": code, "message": message }),
    )
}

/// One audit record (§23.9.4) for what a person did, on standard error.
fn audit(principal: &str, op: &str, target: &str, result: &str) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!(
        "{}",
        serde_json::json!({
            "audit": 1, "time": format!("{}.{:03}", t.as_secs(), t.subsec_millis()),
            "principal": principal, "op": op, "target": target, "result": result,
        })
    );
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
    if !host_allowed(&host, state.port, &state.hosts) {
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
    let client = glidex_authd::client::AuthdClient::new(l.authd.clone());
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
            return json_resp(200, serde_json::json!({ "pam": true, "claim_open": open }));
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
            let user = match user {
                Ok(u) if !u.disabled => u,
                Ok(_) => return error(403, "disabled", "this account is disabled"),
                Err(e) => {
                    audit(&who, "Login", "mgmt", &format!("refused: {e}"));
                    return people_error(e);
                }
            };
            let s = match l.people.new_session(&user.name, "pam").await {
                Ok(s) => s,
                Err(e) => return people_error(e),
            };
            audit(
                &who,
                if r.claim.is_some() { "Claim" } else { "Login" },
                "mgmt",
                "ok",
            );
            let mut resp = json_resp(
                200,
                serde_json::json!({ "user": user.name, "roles": user.roles, "csrf": s.csrf }),
            );
            resp.headers_mut().insert(
                http::header::SET_COOKIE,
                set_cookie(state, &s.id, None).parse().expect("cookie"),
            );
            return resp;
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
