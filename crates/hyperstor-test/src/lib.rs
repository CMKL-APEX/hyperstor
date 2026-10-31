//! Hyperstor's end-to-end test harness.
//!
//! The suite (`tests/`) works as an operator would: it installs clusters
//! with `hyperstor-install` into scratch roots, starts what was installed
//! with the installed configuration, and manages them with the installed
//! `hyperstorctl`, the cluster API and the metrics ports. Nothing runs as
//! root and nothing outside the scratch directory is touched, so it runs
//! anywhere `cargo test` does, CI included.
//!
//! - **Binaries** come from the workspace's build directory (`target/release`
//!   beside the test executable), or `HS_BIN_DIR`. [`bin_dir`] refuses a
//!   binary older than any source cargo built it from, so a stale build can't
//!   pass for the current code (01-implementation-notes.md I47): run
//!   `cargo build --release --workspace` (or `cargo test --workspace`).
//! - **Ports** are taken from a range per process and per call
//!   ([`free_ports`]), so tests in one binary run in parallel.
//! - **Scratch** directories live under `target/tmp` and are removed when
//!   the test ends, unless `HS_E2E_KEEP` is set.
//!
//! [`Cluster::all_in_one`] is the usual fixture. Tests of a host that
//! really has Hyperstor installed are in `tests/installed.rs`, ignored by
//! default (`cargo test -p hyperstor-test --release -- --ignored`).

pub mod vhost;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

pub const BINARIES: [&str; 6] = [
    "hyperstor-install",
    "hyperstor-meta",
    "hyperstord",
    "hyperstorctl",
    "hyperstor-mgmt",
    "hyperstor-authd",
];

/// The workspace root.
pub fn workspace() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    p.canonicalize().unwrap_or(p)
}

/// Where the binaries under test are: `HS_BIN_DIR`, else the build
/// directory the test executable is in. Panics if one is missing, or
/// older than a source file of the crates that build it.
pub fn bin_dir() -> PathBuf {
    let dir = std::env::var_os("HS_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // target/<profile>/deps/<test> -> target/<profile>; a binary
            // of this crate (ui-e2e-cluster) is in target/<profile>.
            let exe = std::env::current_exe().expect("test executable");
            let dir = exe.parent().expect("build directory");
            if dir.file_name().is_some_and(|n| n == "deps") {
                dir.parent().expect("build directory").to_path_buf()
            } else {
                dir.to_path_buf()
            }
        });
    for b in BINARIES {
        let p = dir.join(b);
        let built = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| {
                panic!(
                    "{} is missing: run cargo build --release --workspace",
                    p.display()
                )
            });
        // Cargo's dep-info lists every source the binary was built from;
        // one newer than the binary means a rebuild is due.
        if let Ok(deps) = std::fs::read_to_string(dir.join(format!("{b}.d"))) {
            let sources = deps.split_once(": ").map_or("", |(_, s)| s);
            for src in sources.split_whitespace() {
                let newer = std::fs::metadata(src)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t > built);
                assert!(
                    !newer,
                    "{} is older than {src}: run cargo build --release --workspace",
                    p.display()
                );
            }
        }
    }
    dir
}

/// The first port the kernel hands out to outgoing connections; test
/// ports stay below it, so a client socket can't take one between our
/// check and the service binding it (a CI failure: the dashboard,
/// started last, found its port taken).
fn ephemeral_low() -> u16 {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(32768)
}

/// What the tests add to every `hyperstord` they start: no spinning on
/// shared CPUs, and peers reported silent after 600 ms, not 300. A loaded
/// CI runner pauses every process for a few hundred milliseconds at a time;
/// at 300 ms that marked every node down at once, and the map service's
/// damping (D149) then held them down past a client's retries. Still under
/// the map service's 800 ms heartbeat timeout, so peers notice a dead node
/// first, as in a deployment.
pub const NODE_ARGS: [&str; 4] = ["--spin-us", "0", "--peer-timeout-ms", "600"];

/// `n` consecutive free ports on 127.0.0.1, from 10000 up to the
/// ephemeral range: a block of 500 per process (by pid) and 12 more on
/// each call, so parallel tests and test processes don't collide.
///
/// Each port is also reserved for the rest of this process by a lock
/// file (`flock`) in the system's temporary directory, which other test
/// processes skip: a port free when checked could otherwise be taken by
/// another process's test before ours binds it, or while a service
/// restarts on it (seen when several checkouts run the suite at once).
pub fn free_ports(n: u16) -> u16 {
    static CALLS: AtomicU16 = AtomicU16::new(0);
    static HELD: std::sync::Mutex<Vec<std::fs::File>> = std::sync::Mutex::new(Vec::new());
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    let (lo, hi) = (
        10000u16,
        ephemeral_low().saturating_sub(n + 1).max(10000 + 500),
    );
    let span = hi - lo;
    let mut p = lo + ((std::process::id() % 40) as u16 * 500 + (call % 40) * 12) % span;
    let dir = std::env::temp_dir().join("hyperstor-e2e-ports");
    let _ = std::fs::create_dir_all(&dir);
    let reserve = |port: u16| -> Option<std::fs::File> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(port.to_string()))
            .ok()?;
        f.try_lock().ok()?;
        Some(f)
    };
    loop {
        let locks: Option<Vec<std::fs::File>> = (0..n).map(|i| reserve(p + i)).collect();
        if let Some(locks) = locks
            && (0..n).all(|i| std::net::TcpListener::bind(("127.0.0.1", p + i)).is_ok())
        {
            HELD.lock().unwrap_or_else(|e| e.into_inner()).extend(locks);
            return p;
        }
        p += n + 1;
        if p >= hi {
            p = lo;
        }
    }
}

/// A scratch directory, removed on drop unless `HS_E2E_KEEP` is set,
/// with a short runtime directory for sockets beside it: a Unix socket's
/// path is limited to 107 bytes, which a checkout's path can use up.
pub struct Scratch(pub PathBuf, pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        static N: AtomicU16 = AtomicU16::new(0);
        let d = workspace()
            .join("target/tmp/e2e")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let run = std::env::temp_dir().join(format!(
            "hs{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        Scratch(d, run)
    }

    /// The runtime directory that stands for `/run/hyperstor-meta`.
    pub fn run(&self) -> &Path {
        &self.1
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("HS_E2E_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
            let _ = std::fs::remove_dir_all(&self.1);
        } else {
            eprintln!("kept {}", self.0.display());
        }
    }
}

/// A command's exit status and output, stdout then stderr.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub out: String,
}

impl Run {
    fn of(o: Output) -> Run {
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned()
                + &String::from_utf8_lossy(&o.stderr),
        }
    }

    /// Panics, showing the output, unless the exit status is `code`.
    pub fn expect(self, code: i32) -> Run {
        assert_eq!(self.code, code, "exit status {}:\n{}", self.code, self.out);
        self
    }

    pub fn ok(self) -> Run {
        self.expect(0)
    }

    pub fn has(&self, s: &str) -> bool {
        self.out.contains(s)
    }
}

/// Hard-links the built binaries into `root/usr/local/bin`, so an install
/// there finds them already in place and writes none: each root would
/// otherwise get about half a gigabyte of copies, and tests that install
/// many roots at once slow every fsync on the host (Raft's among them).
/// Falls back to letting the installer copy (another filesystem).
pub fn link_binaries(root: &Path) {
    let dir = root.join("usr/local/bin");
    std::fs::create_dir_all(&dir).unwrap();
    for b in BINARIES {
        let _ = std::fs::hard_link(bin_dir().join(b), dir.join(b));
    }
}

/// `hyperstor-install install --root ROOT ARGS…`, which must succeed.
pub fn install(root: &Path, args: &[&str]) -> Run {
    install_cmd(root, "install", args).ok()
}

/// `hyperstor-install CMD --root ROOT ARGS…`.
pub fn install_cmd(root: &Path, cmd: &str, args: &[&str]) -> Run {
    Run::of(
        Command::new(bin_dir().join("hyperstor-install"))
            .arg(cmd)
            .args(["--root", root.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap(),
    )
}

/// The arguments in an installed `.env` file, with configuration paths
/// moved under `root` and the runtime directory to `run`.
pub fn env_args(root: &Path, run: &Path, env: &str) -> Vec<String> {
    let text = std::fs::read_to_string(root.join("etc/hyperstor").join(env)).unwrap();
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("HS_ARGS="))
        .unwrap();
    line.split_whitespace()
        .map(|a| {
            a.replace(
                "/etc/hyperstor",
                root.join("etc/hyperstor").to_str().unwrap(),
            )
            .replace("/run/hyperstor-meta", run.to_str().unwrap())
            .replace(
                "/usr/local/share/hyperstor",
                root.join("usr/local/share/hyperstor").to_str().unwrap(),
            )
        })
        .collect()
}

/// Polls `f` every 100 ms until it gives a value, for up to `secs`.
pub fn wait_for<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let t = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            t.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How `hyperstorctl` reaches the API.
pub enum Via<'a> {
    /// Over mutual TLS with the certificates in this directory.
    Tls(&'a Path),
    /// The local socket.
    Socket,
    /// Plain HTTP (refused by a TLS API).
    Plain,
}

/// A person logged in to hyperstor-mgmt.
#[derive(Debug, Clone)]
pub struct Session {
    /// `hs_session=…`, for a `Cookie` header.
    pub cookie: String,
    /// The whole `Set-Cookie` value, attributes included.
    pub set_cookie: String,
    pub csrf: String,
    pub roles: Vec<String>,
}

/// Everyone's password in the made-up account database ([`start_authd`]).
/// A test value: these people exist only in the test.
pub const PASSWORD: &str = "test-only password";

/// A PAM helper (glidex-authd's server, in this process) on `socket`, with
/// made-up people: `ann`, `bob` and `cy` in `hyperstor-users`, and `eve`,
/// who exists but isn't. Their password is [`PASSWORD`]. It answers the
/// user running the test, as the installed one answers `hyperstor`.
pub fn start_authd(socket: &Path) {
    use glidex_authd::authenticator::{Account, Accounts, AuthFailure, Authenticator};
    use std::os::unix::fs::MetadataExt;
    struct Pam;
    impl Authenticator for Pam {
        fn authenticate(&self, service: &str, user: &str, pw: &str) -> Result<(), AuthFailure> {
            match (service, user, pw) {
                ("hyperstor", "ann" | "bob" | "cy" | "eve", PASSWORD) => Ok(()),
                _ => Err(AuthFailure::Denied("bad password".into())),
            }
        }
    }
    struct People;
    impl Accounts for People {
        fn lookup(&self, user: &str) -> Option<Account> {
            let groups = match user {
                "ann" | "bob" | "cy" => vec![user.to_string(), "hyperstor-users".to_string()],
                "eve" => vec![user.to_string()],
                _ => return None,
            };
            Some(Account { uid: 5000, groups })
        }
    }
    let config = glidex_authd::config::Config {
        allowed_groups: vec!["hyperstor-users".to_string()],
        failure_delay: Duration::from_millis(2),
        ..Default::default()
    };
    let me = std::fs::metadata(socket.parent().unwrap()).unwrap().uid();
    let listener = glidex_authd::server::bind(socket, 0o600, None).unwrap();
    let authd = std::sync::Arc::new(glidex_authd::server::Authd::new(
        config,
        Some(me),
        std::sync::Arc::new(Pam),
        std::sync::Arc::new(People),
    ));
    std::thread::spawn(move || authd.serve(listener));
}

/// A made-up identity provider on loopback, for OIDC logins: discovery,
/// its key (ES256), and a token endpoint that answers with the ID token
/// the test sets ([`FakeIdp::next_id_token`]), whatever the code. It keeps
/// the token requests' bodies.
pub struct FakeIdp {
    pub issuer: String,
    key: jsonwebtoken::EncodingKey,
    next: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    pub token_requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Who `/authorize` logs in at once, for a browser ([`FakeIdp::auto_login`]).
    auto: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// An ID token from `issuer` for `client_id`, valid an hour, with `claims`.
fn sign_id_token(
    key: &jsonwebtoken::EncodingKey,
    issuer: &str,
    client_id: &str,
    claims: &serde_json::Value,
) -> String {
    let now = now_unix();
    let mut c = serde_json::json!({
        "iss": issuer, "aud": client_id, "iat": now, "exp": now + 3600,
    });
    for (k, v) in claims.as_object().unwrap() {
        c[k] = v.clone();
    }
    let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    h.kid = Some("k1".into());
    jsonwebtoken::encode(&h, &c, key).unwrap()
}

impl FakeIdp {
    pub fn start() -> FakeIdp {
        use base64::Engine as _;
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let raw = kp.public_key_raw();
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let jwks = serde_json::json!({ "keys": [{
            "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig",
            "x": b64(&raw[1..33]), "y": b64(&raw[33..65]),
        }]})
        .to_string();
        let key = jsonwebtoken::EncodingKey::from_ec_pem(kp.serialize_pem().as_bytes()).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", l.local_addr().unwrap());
        let discovery = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
        })
        .to_string();
        let next = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let token_requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let auto = std::sync::Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
        let (n, t, au) = (next.clone(), token_requests.clone(), auto.clone());
        let (k, iss) = (key.clone(), issuer.clone());
        std::thread::spawn(move || {
            use std::io::{BufRead, Read, Write};
            for s in l.incoming().flatten() {
                let mut r = std::io::BufReader::new(&s);
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0;
                let mut auth = String::new();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        auth = h.trim().to_string();
                    }
                }
                let mut body = vec![0; len];
                let _ = r.read_exact(&mut body);
                // A browser's login: whoever auto_login names, at once.
                if path.starts_with("/authorize?") {
                    let who = au.lock().unwrap().clone();
                    let q = |name: &str| query_param(&path, name).unwrap_or_default();
                    let answer = match who {
                        Some(mut claims) => {
                            claims["nonce"] = q("nonce").into();
                            *n.lock().unwrap() =
                                Some(sign_id_token(&k, &iss, &q("client_id"), &claims));
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: {}?code=auto&state={}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                q("redirect_uri"),
                                q("state")
                            )
                        }
                        None => "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
                    };
                    let _ = (&s).write_all(answer.as_bytes());
                    continue;
                }
                let (code, answer) = match path.as_str() {
                    "/.well-known/openid-configuration" => (200, discovery.clone()),
                    "/jwks" => (200, jwks.clone()),
                    "/token" => {
                        t.lock()
                            .unwrap()
                            .push(format!("{auth} {}", String::from_utf8_lossy(&body)));
                        match n.lock().unwrap().take() {
                            Some(id) => (
                                200,
                                serde_json::json!({
                                    "id_token": id, "access_token": "x", "token_type": "Bearer",
                                })
                                .to_string(),
                            ),
                            None => (400, r#"{"error":"invalid_grant"}"#.to_string()),
                        }
                    }
                    _ => (404, "{}".to_string()),
                };
                let _ = write!(
                    &s,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
        FakeIdp {
            issuer,
            key,
            next,
            token_requests,
            auto,
        }
    }

    /// From now on `/authorize` logs a browser in at once as `claims`
    /// (`sub`, `email`, `email_verified`...) and sends it back.
    pub fn auto_login(&self, claims: serde_json::Value) {
        *self.auto.lock().unwrap() = Some(claims);
    }

    /// The ID token the next token request gets: signed, for `client_id`,
    /// valid an hour, with these claims added (`sub`, `email`, `nonce`…).
    pub fn next_id_token(&self, client_id: &str, claims: serde_json::Value) {
        *self.next.lock().unwrap() =
            Some(sign_id_token(&self.key, &self.issuer, client_id, &claims));
    }
}

/// A query parameter of `url`, decoded.
pub fn query_param(url: &str, name: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| {
            let mut out = Vec::new();
            let b = v.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap_or(b'?'));
                    i += 3;
                } else {
                    out.push(if b[i] == b'+' { b' ' } else { b[i] });
                    i += 1;
                }
            }
            String::from_utf8_lossy(&out).into_owned()
        })
    })
}

/// A running cluster installed into a scratch root.
pub struct Cluster {
    pub scratch: Scratch,
    pub root: PathBuf,
    /// The installed binaries, `<root>/usr/local/bin`.
    pub bin: PathBuf,
    pub api: String,
    /// The map service's address, for clients (`hyperstor-ublk`).
    pub map: String,
    pub cluster_id: String,
    pub nodes: u32,
    /// Node n's metrics port is `admin_port + n`.
    pub admin_port: u16,
    /// The dashboard's port (`hyperstor-mgmt`).
    pub mgmt_port: u16,
    pub meta_log: PathBuf,
    meta: Option<Child>,
    mgmt: Option<Child>,
    /// Options hyperstor-mgmt gets after its installed ones.
    mgmt_extra: Vec<String>,
    /// Run mgmt on every address, as installed (D110); else on loopback
    /// only, so a test's cluster isn't reachable from the network.
    pub mgmt_all_addresses: bool,
    /// mgmt serves HTTPS (`--web-tls`), with the cluster CA's certificate.
    mgmt_https: bool,
    node_procs: BTreeMap<u32, Child>,
}

impl Cluster {
    /// Installs an all-in-one cluster of `nodes` storage nodes (1 GiB each)
    /// into a new scratch root, starts it from what was installed, and
    /// waits for health ok.
    pub fn all_in_one(name: &str, nodes: u32) -> Cluster {
        let scratch = Scratch::new(name);
        let root = scratch.path("root");
        let data = scratch.path("data");
        // --root puts the data directory under the root like any path;
        // the processes use the final path, so make it.
        std::fs::create_dir_all(&data).unwrap();
        let base = free_ports(4 + 2 * nodes as u16);
        let (api, map, node, admin) = (base, base + 1, base + 2, base + 2 + nodes as u16);
        let mgmt = base + 2 + 2 * nodes as u16;
        install(
            &root,
            &[
                "--all-in-one",
                "--nodes",
                &nodes.to_string(),
                "--size-gib",
                "1",
                "--data-dir",
                data.to_str().unwrap(),
                "--api-port",
                &api.to_string(),
                "--map-port",
                &map.to_string(),
                "--node-port",
                &node.to_string(),
                "--admin-port",
                &admin.to_string(),
                "--mgmt-port",
                &mgmt.to_string(),
            ],
        );
        let cluster_id = std::fs::read_to_string(root.join("etc/hyperstor/cluster.txt"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("cluster ").map(|v| v.trim().to_string()))
            .unwrap();
        let mut c = Cluster {
            bin: root.join("usr/local/bin"),
            meta_log: scratch.path("meta.log"),
            root,
            scratch,
            api: format!("127.0.0.1:{api}"),
            map: format!("127.0.0.1:{map}"),
            cluster_id,
            nodes,
            admin_port: admin,
            mgmt_port: mgmt,
            mgmt: None,
            mgmt_extra: Vec::new(),
            mgmt_all_addresses: false,
            mgmt_https: false,
            meta: None,
            node_procs: BTreeMap::new(),
        };
        c.start_meta();
        for n in 0..nodes {
            c.start_node(n);
        }
        c.ctl(&["status", "--wait", "30"]).ok();
        c.start_mgmt();
        c
    }

    /// Starts the dashboard from its installed options and waits for it
    /// to listen; if it exits first, fails with its status and output.
    pub fn start_mgmt(&mut self) {
        let log_path = self.scratch.path("mgmt.log");
        let sock = self.scratch.run().join("auth.sock");
        if !sock.exists() {
            start_authd(&sock);
        }
        let all = format!("0.0.0.0:{port},[::]:{port}", port = self.mgmt_port);
        let args: Vec<String> = env_args(&self.root, self.scratch.run(), "mgmt.env")
            .into_iter()
            .map(|a| a.replace("/run/hyperstor-authd", self.scratch.run().to_str().unwrap()))
            .map(|a| {
                if a == all && !self.mgmt_all_addresses {
                    format!("127.0.0.1:{}", self.mgmt_port)
                } else {
                    a
                }
            })
            .collect();
        self.mgmt_https = args.iter().any(|a| a == "--web-tls");
        let p = Command::new(self.bin.join("hyperstor-mgmt"))
            .args(args)
            .args(&self.mgmt_extra)
            .stderr(std::fs::File::create(&log_path).unwrap())
            .spawn()
            .unwrap();
        self.mgmt = Some(p);
        let port = self.mgmt_port;
        let mgmt = self.mgmt.as_mut().unwrap();
        wait_for("the dashboard", 20, || {
            if let Ok(Some(st)) = mgmt.try_wait() {
                panic!(
                    "hyperstor-mgmt exited ({st}) before listening on {port}:\n{}",
                    std::fs::read_to_string(&log_path).unwrap_or_default()
                );
            }
            std::net::TcpStream::connect(("127.0.0.1", port)).ok()
        });
    }

    /// `METHOD path` to the dashboard with this `Host` header: the status
    /// code, the head and the body.
    pub fn dashboard(&self, method: &str, path: &str, host: &str) -> (u16, String, String) {
        self.web(method, path, host, &[], "")
    }

    /// `METHOD path` to hyperstor-mgmt with this `Host`, extra headers and
    /// body: the status code, the head and the body.
    pub fn web(
        &self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String, String) {
        use std::io::{Read, Write};
        let tcp = std::net::TcpStream::connect(("127.0.0.1", self.mgmt_port)).unwrap();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        let mut raw = Vec::new();
        if self.mgmt_https {
            // Verified as a client of the host would: the cluster's root.
            let root = std::fs::read_to_string(self.pki().join("root.crt")).unwrap();
            let config = hyperstor_pki::web_client_config(Some(&root)).unwrap();
            let name = rustls::pki_types::ServerName::IpAddress(
                std::net::IpAddr::from([127, 0, 0, 1]).into(),
            );
            let conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
            let mut s = rustls::StreamOwned::new(conn, tcp);
            s.write_all(req.as_bytes()).unwrap();
            // A server may close without close_notify: what came counts.
            if let Err(e) = s.read_to_end(&mut raw) {
                assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "{e}");
            }
        } else {
            let mut s = tcp;
            s.write_all(req.as_bytes()).unwrap();
            s.read_to_end(&mut raw).unwrap();
        }
        let t = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = t.split_once("\r\n\r\n").unwrap_or((&t, ""));
        let code = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (code, head.to_string(), body.to_string())
    }

    /// Logs `user` in to hyperstor-mgmt with [`PASSWORD`] (and a claim
    /// code): their session, or the refusal's status and body.
    pub fn login(&self, user: &str, claim: Option<&str>) -> Result<Session, (u16, String)> {
        let body = match claim {
            Some(c) => serde_json::json!({ "user": user, "password": PASSWORD, "claim": c }),
            None => serde_json::json!({ "user": user, "password": PASSWORD }),
        };
        let (code, head, body) = self.web(
            "POST",
            "/auth/login",
            &self.dashboard_host(),
            &[("Content-Type", "application/json")],
            &body.to_string(),
        );
        if code != 200 {
            return Err((code, body));
        }
        let cookie = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(": ")?;
                k.eq_ignore_ascii_case("set-cookie").then(|| v.to_string())
            })
            .expect("a session cookie");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        Ok(Session {
            cookie: cookie.split(';').next().unwrap().to_string(),
            set_cookie: cookie,
            csrf: v["csrf"].as_str().unwrap().to_string(),
            roles: v["roles"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect(),
        })
    }

    /// `METHOD path` as `s` (its cookie and CSRF value), with a JSON body.
    pub fn as_session(
        &self,
        s: &Session,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, String, String) {
        self.web(
            method,
            path,
            &self.dashboard_host(),
            &[
                ("Cookie", &s.cookie),
                ("X-Hyperstor-CSRF", &s.csrf),
                ("Content-Type", "application/json"),
            ],
            body,
        )
    }

    /// The claim code hyperstor-mgmt printed at start, once it has.
    pub fn claim_code(&self) -> String {
        let log = self.scratch.path("mgmt.log");
        wait_for("the claim code", 30, || {
            let text = std::fs::read_to_string(&log).ok()?;
            let rest = text.split("claim code ").nth(1)?;
            Some(rest.split_whitespace().next()?.to_string())
        })
    }

    /// hyperstor-mgmt's standard error so far (its audit records).
    pub fn mgmt_log(&self) -> String {
        std::fs::read_to_string(self.scratch.path("mgmt.log")).unwrap_or_default()
    }

    /// Stops hyperstor-mgmt and starts it again from its installed options.
    pub fn restart_mgmt(&mut self) {
        self.restart_mgmt_with(&[]);
    }

    /// As [`restart_mgmt`](Self::restart_mgmt), with `extra` options after
    /// the installed ones.
    pub fn restart_mgmt_with(&mut self, extra: &[&str]) {
        if let Some(mut p) = self.mgmt.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        self.mgmt_extra = extra.iter().map(|s| s.to_string()).collect();
        self.start_mgmt();
    }

    /// mgmt's URL for this cluster, `https://` once it serves HTTPS.
    pub fn mgmt_url(&self) -> String {
        let scheme = if self.mgmt_https { "https" } else { "http" };
        format!("{scheme}://{}", self.dashboard_host())
    }

    /// The dashboard's `Host` for this cluster.
    pub fn dashboard_host(&self) -> String {
        format!("127.0.0.1:{}", self.mgmt_port)
    }

    pub fn start_meta(&mut self) {
        self.start_meta_with(&[]);
    }

    /// Starts the map service with `extra` options after its installed ones.
    pub fn start_meta_with(&mut self, extra: &[&str]) {
        let p = Command::new(self.bin.join("hyperstor-meta"))
            .args(env_args(&self.root, self.scratch.run(), "meta.env"))
            .args(extra)
            .stderr(std::fs::File::create(&self.meta_log).unwrap())
            .spawn()
            .unwrap();
        self.meta = Some(p);
    }

    pub fn start_node(&mut self, n: u32) {
        let log = std::fs::File::create(self.scratch.path(&format!("node-{n}.log"))).unwrap();
        let p = Command::new(self.bin.join("hyperstord"))
            .args(env_args(
                &self.root,
                self.scratch.run(),
                &format!("node-{n}.env"),
            ))
            .args(NODE_ARGS)
            .stderr(log)
            .spawn()
            .unwrap();
        self.node_procs.insert(n, p);
    }

    /// Kills the map service (as a crash would) and waits for it to go.
    pub fn stop_meta(&mut self) {
        if let Some(mut p) = self.meta.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
    }

    /// The data directory: device files, the map service's store
    /// (`meta/`) and its backups (`meta-backups/`).
    pub fn data_dir(&self) -> PathBuf {
        self.scratch.path("data")
    }

    /// Storage node `n`'s log so far.
    pub fn node_log(&self, n: u32) -> String {
        std::fs::read_to_string(self.scratch.path(&format!("node-{n}.log"))).unwrap_or_default()
    }

    /// The map service's log so far.
    pub fn meta_log(&self) -> String {
        std::fs::read_to_string(&self.meta_log).unwrap_or_default()
    }

    /// Kills storage node `n` (as a crash would).
    pub fn kill_node(&mut self, n: u32) {
        if let Some(mut p) = self.node_procs.remove(&n) {
            p.kill().unwrap();
            p.wait().unwrap();
        }
    }

    pub fn pki(&self) -> PathBuf {
        self.root.join("etc/hyperstor/pki")
    }

    pub fn socket(&self) -> PathBuf {
        self.scratch.run().join("api.sock")
    }

    /// `hyperstorctl` as the administrator, over mutual TLS.
    pub fn ctl(&self, args: &[&str]) -> Run {
        self.ctl_via(Via::Tls(&self.pki()), args)
    }

    pub fn ctl_via(&self, via: Via, args: &[&str]) -> Run {
        let mut c = Command::new(self.bin.join("hyperstorctl"));
        match via {
            Via::Tls(dir) => c.args(["--api", &self.api, "--tls", dir.to_str().unwrap()]),
            Via::Socket => c.args(["--socket", self.socket().to_str().unwrap()]),
            Via::Plain => c.args(["--api", &self.api]),
        };
        Run::of(c.args(args).output().unwrap())
    }

    /// A certificate directory for `role`/`id`, issued by this cluster's
    /// intermediate (or, with `foreign`, by another cluster's), laid out
    /// as `hyperstorctl --tls` expects.
    pub fn identity(&self, role: hyperstor_pki::Role, id: &str, foreign: bool) -> PathBuf {
        let read = |f: &str| std::fs::read_to_string(self.pki().join(f)).unwrap();
        let ca = if foreign {
            let r = hyperstor_pki::new_root("1").unwrap();
            hyperstor_pki::new_intermediate(&r, "1").unwrap()
        } else {
            hyperstor_pki::Pem {
                cert: read("intermediate.crt"),
                key: read("intermediate.key"),
            }
        };
        let who = hyperstor_pki::Identity {
            cluster: self.cluster_id.clone(),
            role,
            id: id.into(),
        };
        let p = hyperstor_pki::issue(&ca, &who, &[], &[], 1).unwrap();
        let dir = self.scratch.path(&format!("id-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("admin.crt"), p.cert).unwrap();
        std::fs::write(dir.join("admin.key"), p.key).unwrap();
        std::fs::write(dir.join("root.crt"), read("root.crt")).unwrap();
        dir
    }

    /// Node `n`'s `GET /metrics`.
    pub fn metrics(&self, n: u32) -> String {
        use std::io::{Read, Write};
        let mut s =
            std::net::TcpStream::connect(("127.0.0.1", self.admin_port + n as u16)).unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut t = String::new();
        s.read_to_string(&mut t).unwrap();
        assert!(t.starts_with("HTTP/1.1 200 OK"), "{t}");
        t
    }

    /// The sum over every node of the samples of `name` whose labels
    /// contain `filter` (`""` for all).
    pub fn metric_sum(&self, name: &str, filter: &str) -> u64 {
        (0..self.nodes)
            .filter(|n| self.node_procs.contains_key(n))
            .map(|n| {
                self.metrics(n)
                    .lines()
                    .filter(|l| l.starts_with(&format!("{name}{{")) && l.contains(filter))
                    .map(|l| l.rsplit(' ').next().unwrap().parse::<f64>().unwrap() as u64)
                    .sum::<u64>()
            })
            .sum()
    }

    /// The map service's audit records so far.
    pub fn audit(&self) -> Vec<String> {
        std::fs::read_to_string(&self.meta_log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("{\"audit\":1"))
            .map(String::from)
            .collect()
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        // A failed test shows what the cluster said: on CI the scratch
        // directory is gone afterwards.
        if std::thread::panicking() {
            let mut logs = vec![self.meta_log.clone()];
            logs.extend((0..self.nodes).map(|n| self.scratch.path(&format!("node-{n}.log"))));
            for log in logs {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|l| !l.starts_with("{\"audit\""))
                    .collect();
                eprintln!("--- {} (last {} lines)", log.display(), lines.len().min(40));
                for l in &lines[lines.len().saturating_sub(40)..] {
                    eprintln!("{l}");
                }
            }
        }
        for p in self
            .node_procs
            .values_mut()
            .chain(self.meta.iter_mut())
            .chain(self.mgmt.iter_mut())
        {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_ports_stay_below_the_ephemeral_range() {
        let low = super::ephemeral_low();
        for _ in 0..100 {
            let p = super::free_ports(10);
            assert!(
                (10000..low).contains(&p) && p + 10 <= low,
                "{p} (ephemeral from {low})"
            );
        }
    }

    /// A port handed out is reserved: no later call, here or in another
    /// process, gets it again while this process lives.
    #[test]
    fn ports_handed_out_stay_reserved() {
        let a = super::free_ports(4);
        let b = super::free_ports(4);
        assert!(b + 4 <= a || a + 4 <= b, "{a} and {b} overlap");
        let lock = std::env::temp_dir()
            .join("hyperstor-e2e-ports")
            .join(a.to_string());
        let f = std::fs::OpenOptions::new().write(true).open(lock).unwrap();
        assert!(f.try_lock().is_err(), "held by this process");
    }
}
