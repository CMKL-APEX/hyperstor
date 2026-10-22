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
//!   pass for the current code (implementation-notes.md I47): run
//!   `cargo build --release --workspace` (or `cargo test --workspace`).
//! - **Ports** are taken from a range per process and per call
//!   ([`free_ports`]), so tests in one binary run in parallel.
//! - **Scratch** directories live under `target/tmp` and are removed when
//!   the test ends, unless `HS_E2E_KEEP` is set.
//!
//! [`Cluster::all_in_one`] is the usual fixture. Tests of a host that
//! really has Hyperstor installed are in `tests/installed.rs`, ignored by
//! default (`cargo test -p hyperstor-test --release -- --ignored`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

pub const BINARIES: [&str; 5] = [
    "hyperstor-install",
    "hyperstor-meta",
    "hyperstord",
    "hyperstorctl",
    "hyperstor-mgmt",
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
            // target/<profile>/deps/<test> -> target/<profile>
            let exe = std::env::current_exe().expect("test executable");
            exe.parent()
                .and_then(Path::parent)
                .expect("build directory")
                .to_path_buf()
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

/// A running cluster installed into a scratch root.
pub struct Cluster {
    pub scratch: Scratch,
    pub root: PathBuf,
    /// The installed binaries, `<root>/usr/local/bin`.
    pub bin: PathBuf,
    pub api: String,
    pub cluster_id: String,
    pub nodes: u32,
    /// Node n's metrics port is `admin_port + n`.
    pub admin_port: u16,
    /// The dashboard's port (`hyperstor-mgmt`).
    pub mgmt_port: u16,
    pub meta_log: PathBuf,
    meta: Option<Child>,
    mgmt: Option<Child>,
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
            cluster_id,
            nodes,
            admin_port: admin,
            mgmt_port: mgmt,
            mgmt: None,
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
        let p = Command::new(self.bin.join("hyperstor-mgmt"))
            .args(env_args(&self.root, self.scratch.run(), "mgmt.env"))
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
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.mgmt_port)).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut t = String::new();
        s.read_to_string(&mut t).unwrap();
        let (head, body) = t.split_once("\r\n\r\n").unwrap_or((&t, ""));
        let code = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (code, head.to_string(), body.to_string())
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
            .args(["--spin-us", "0"])
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
