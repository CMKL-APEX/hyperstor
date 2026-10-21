//! `hyperstor-install check|install|uninstall [--all-in-one] [--nodes N]
//!                   [--size-gib G] [--data-dir DIR] [--api-port P]
//!                   [--map-port P] [--node-port P] [--admin-port P] [--root DIR] [--no-start]`
//!
//! The installer of spec/administration.md §23.4: one linear file,
//! non-interactive and convergent. Every run compares what is installed
//! with what should be and changes only the difference, so the same
//! command installs and later repairs or updates.
//!
//! - `check` runs the host checks (§23.4.1) and changes nothing.
//! - `install --cluster FILE --role meta|storage|both [--node N] --map ADDR`
//!   installs this host's part of the cluster the file describes: the map
//!   service listening on ADDR (with the CAs and the API), storage node N
//!   with one reactor per address the file lists for it, or both. Every
//!   host gets the same file (interim, until join tokens; I50).
//! - `install --all-in-one` sets up a trial cluster on this host: the map
//!   service (map protocol on 8849, cluster API on 8850, both loopback)
//!   and N storage nodes (default 3), each on a file of G GiB
//!   (default 4) in the data directory, a replicated pool and a cache
//!   pool (D45), systemd units,
//!   and `hyperstorctl`'s configuration. Flags are saved in
//!   `/etc/hyperstor/install.conf` and reused by later runs.
//!   It also installs itself and a daily timer that runs `install` again,
//!   which renews certificates (D56) and repairs drift.
//! - `uninstall` stops and removes the units and binaries; configuration
//!   and data stay.
//!
//! Binaries are taken from the installer's own directory (`target/release`
//! when run from a checkout). `--root DIR` writes every file under DIR
//! instead of `/` (for packaging and tests) and never touches users or
//! systemd; the files still name their final paths.
//!
//! Exit status: 0 done, 1 a check failed or a step failed, 2 usage.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "usage: hyperstor-install check|install|uninstall [--all-in-one] [--cluster FILE] [--role R] [--node N] [--map ADDR] [--nodes N] [--size-gib G] [--data-dir DIR] [--api-port P] [--map-port P] [--node-port P] [--admin-port P] [--root DIR] [--no-start]";

const BINARIES: [&str; 3] = ["hyperstor-meta", "hyperstord", "hyperstorctl"];

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn fail(msg: &str) -> ! {
    eprintln!("hyperstor-install: {msg}");
    std::process::exit(1)
}

/// The saved and given settings; given flags override saved ones.
#[derive(Clone, Debug, PartialEq)]
struct Settings {
    all_in_one: bool,
    /// With a cluster file: `meta`, `storage` or `both`; empty otherwise.
    role: String,
    /// The storage node this host runs, from the cluster file.
    node: Option<u32>,
    /// The map service's address, `IP:PORT`, for a cluster file install.
    map: String,
    nodes: u32,
    size_gib: u64,
    data_dir: String,
    api_port: u16,
    map_port: u16,
    node_port: u16,
    admin_port: u16,
}

impl Settings {
    fn defaults() -> Self {
        Settings {
            all_in_one: false,
            role: String::new(),
            node: None,
            map: String::new(),
            nodes: 3,
            size_gib: 4,
            data_dir: "/var/lib/hyperstor".into(),
            api_port: 8850,
            map_port: 8849,
            node_port: 9100,
            admin_port: 8853,
        }
    }

    /// Reads `key = value` lines over `self`; unknown keys are ignored.
    fn apply(&mut self, text: &str) {
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "all_in_one" => self.all_in_one = v == "true",
                "role" => self.role = v.to_string(),
                "node" => self.node = v.parse().ok(),
                "map" => self.map = v.to_string(),
                "nodes" => self.nodes = v.parse().unwrap_or(self.nodes),
                "size_gib" => self.size_gib = v.parse().unwrap_or(self.size_gib),
                "data_dir" => self.data_dir = v.to_string(),
                "api_port" => self.api_port = v.parse().unwrap_or(self.api_port),
                "map_port" => self.map_port = v.parse().unwrap_or(self.map_port),
                "node_port" => self.node_port = v.parse().unwrap_or(self.node_port),
                "admin_port" => self.admin_port = v.parse().unwrap_or(self.admin_port),
                _ => {}
            }
        }
    }

    fn save(&self) -> String {
        format!(
            "# Written by hyperstor-install; flags given on the command line override these.\n\
             all_in_one = {}\nrole = {}\nnode = {}\nmap = {}\nnodes = {}\nsize_gib = {}\ndata_dir = {}\napi_port = {}\nmap_port = {}\nnode_port = {}\nadmin_port = {}\n",
            self.all_in_one,
            self.role,
            self.node.map(|n| n.to_string()).unwrap_or_default(),
            self.map,
            self.nodes,
            self.size_gib,
            self.data_dir,
            self.api_port,
            self.map_port,
            self.node_port,
            self.admin_port
        )
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        usage()
    };
    if !matches!(cmd.as_str(), "check" | "install" | "uninstall") {
        usage();
    }
    let mut root = PathBuf::from("/");
    let mut no_start = false;
    let mut cluster_file: Option<String> = None;
    let mut given: BTreeMap<&str, String> = BTreeMap::new();
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| usage());
        let key = match a.as_str() {
            "--all-in-one" => {
                given.insert("all_in_one", "true".into());
                continue;
            }
            "--root" => {
                root = PathBuf::from(val());
                continue;
            }
            "--no-start" => {
                no_start = true;
                continue;
            }
            "--cluster" => {
                let f = val();
                cluster_file =
                    Some(fs::read_to_string(&f).unwrap_or_else(|e| fail(&format!("{f}: {e}"))));
                continue;
            }
            "--role" => "role",
            "--node" => "node",
            "--map" => "map",
            "--nodes" => "nodes",
            "--size-gib" => "size_gib",
            "--data-dir" => "data_dir",
            "--api-port" => "api_port",
            "--map-port" => "map_port",
            "--node-port" => "node_port",
            "--admin-port" => "admin_port",
            _ => usage(),
        };
        let v = val();
        let ok = match key {
            "nodes" => v.parse::<u32>().is_ok_and(|n| (1..=16).contains(&n)),
            "size_gib" => v.parse::<u64>().is_ok_and(|n| n >= 1),
            "api_port" | "map_port" | "node_port" | "admin_port" => {
                v.parse::<u16>().is_ok_and(|n| n > 0)
            }
            "role" => matches!(v.as_str(), "meta" | "storage" | "both"),
            "node" => v.parse::<u32>().is_ok(),
            "map" => v.parse::<std::net::SocketAddr>().is_ok(),
            _ => v.starts_with('/'),
        };
        if !ok {
            eprintln!("hyperstor-install: bad value for {a}: {v}");
            usage();
        }
        given.insert(key, v);
    }
    let real = root == Path::new("/");
    let mut s = Settings::defaults();
    s.apply(&fs::read_to_string(root.join("etc/hyperstor/install.conf")).unwrap_or_default());
    s.apply(
        &given
            .iter()
            .map(|(k, v)| format!("{k} = {v}\n"))
            .collect::<String>(),
    );

    match cmd.as_str() {
        "check" => std::process::exit(i32::from(check(&s))),
        "uninstall" => uninstall(&root, real, &s),
        _ => install(&root, real, no_start, &s, cluster_file),
    }
}

// --- Host checks (§23.4.1) ------------------------------------------------

#[derive(PartialEq, PartialOrd, Clone, Copy)]
enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

/// Runs every check, printing one line each; returns whether one failed.
fn check(s: &Settings) -> bool {
    let mut worst = Level::Ok;
    let mut note = |l: Level, what: &str, detail: String| {
        if l > worst {
            worst = l;
        }
        let tag = match l {
            Level::Ok => "ok  ",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        println!("{tag}  {what:<9} {detail}");
    };

    // Kernel: io_uring with uring_cmd for NVMe passthrough needs 5.19;
    // the reactor is tested on 6.x.
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let release = release.trim();
    let ver: Vec<u32> = release
        .split(|c: char| !c.is_ascii_digit())
        .take(2)
        .filter_map(|p| p.parse().ok())
        .collect();
    match ver.as_slice() {
        [maj, min] if (*maj, *min) >= (6, 1) => note(Level::Ok, "kernel", release.to_string()),
        [maj, min] if (*maj, *min) >= (5, 19) => note(
            Level::Warn,
            "kernel",
            format!("{release}: works, but hyperstor is tested on 6.1 and later"),
        ),
        _ => note(
            Level::Fail,
            "kernel",
            format!("{release}: io_uring passthrough needs 5.19 or later"),
        ),
    }

    // io_uring may be turned off by sysctl (6.6+) or a seccomp profile.
    let disabled = fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);
    match io_uring::IoUring::new(8) {
        Ok(_) => note(Level::Ok, "io_uring", "a ring can be created".into()),
        Err(e) => note(
            Level::Fail,
            "io_uring",
            format!("can't create a ring: {e} (kernel.io_uring_disabled = {disabled})"),
        ),
    }

    // Registered buffers and RDMA need locked memory; the units set
    // LimitMEMLOCK=infinity, so a small limit only matters by hand.
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit into `lim`.
    unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) };
    if lim.rlim_cur == libc::RLIM_INFINITY || lim.rlim_cur >= 64 << 20 {
        note(Level::Ok, "memlock", "enough for registered buffers".into());
    } else {
        note(
            Level::Info,
            "memlock",
            format!(
                "{} KiB in this shell; the systemd units raise it",
                lim.rlim_cur >> 10
            ),
        );
    }

    // CPUs: a reactor per storage node, plus the map service.
    // SAFETY: sysconf has no preconditions.
    let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as u32;
    if cpus > s.nodes {
        note(Level::Ok, "cpus", format!("{cpus} online"));
    } else {
        note(
            Level::Warn,
            "cpus",
            format!(
                "{cpus} online for {} storage nodes: reactors will share cores",
                s.nodes
            ),
        );
    }

    // NVMe generic character devices, for passthrough (D29). The trial
    // setup uses files, so none is fine.
    let mut ng: Vec<String> = fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("ng"))
                .collect()
        })
        .unwrap_or_default();
    ng.sort();
    note(
        Level::Info,
        "nvme",
        if ng.is_empty() {
            "no NVMe generic devices (/dev/ng*); the trial setup uses files".into()
        } else {
            format!("{} generic device(s), e.g. /dev/{}", ng.len(), ng[0])
        },
    );

    // Leases (D48) assume clocks that don't jump.
    match Command::new("timedatectl")
        .args(["show", "-p", "NTPSynchronized", "--value"])
        .output()
    {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "yes" => {
            note(Level::Ok, "clock", "synchronized".into())
        }
        Ok(_) => note(Level::Warn, "clock", "not synchronized (NTP)".into()),
        Err(_) => note(
            Level::Warn,
            "clock",
            "timedatectl missing; can't tell".into(),
        ),
    }

    // Space for the device files, which are sparse but fill up.
    let need = u64::from(s.nodes) * s.size_gib;
    let mut dir = Path::new(&s.data_dir);
    while !dir.exists() {
        dir = dir.parent().unwrap_or(Path::new("/"));
    }
    let free = free_gib(dir);
    note(
        if free >= need { Level::Ok } else { Level::Warn },
        "disk",
        format!(
            "{free} GiB free at {}, {need} GiB needed when full",
            dir.display()
        ),
    );

    // Ports: in use is fine when it's our own running service.
    let busy: Vec<String> = [s.api_port, s.map_port]
        .into_iter()
        .chain((0..s.nodes).map(|n| s.node_port + n as u16))
        .chain((0..s.nodes).map(|n| s.admin_port + n as u16))
        .filter(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_err())
        .map(|p| p.to_string())
        .collect();
    if busy.is_empty() {
        note(Level::Ok, "ports", "free".into());
    } else {
        note(
            Level::Info,
            "ports",
            format!(
                "in use: {} (fine if hyperstor already runs)",
                busy.join(", ")
            ),
        );
    }

    if Path::new("/run/systemd/system").exists() {
        note(Level::Ok, "systemd", "running".into());
    } else {
        note(
            Level::Warn,
            "systemd",
            "not running: install with --no-start and start the services by hand".into(),
        );
    }
    worst == Level::Fail
}

fn free_gib(p: &Path) -> u64 {
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else {
        return 0;
    };
    // SAFETY: an all-zero statvfs is a valid value to overwrite.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `st` a valid out pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    (st.f_bavail as u64 * st.f_frsize as u64) >> 30
}

// --- Install --------------------------------------------------------------

const META_UNIT: &str = "\
# Written by hyperstor-install.
[Unit]
Description=Hyperstor map service
Documentation=https://github.com/CMKL-APEX/hyperstor
After=network-online.target
Wants=network-online.target

[Service]
User=hyperstor
EnvironmentFile=/etc/hyperstor/meta.env
ExecStart=/usr/local/bin/hyperstor-meta $HS_ARGS
Restart=on-failure
RestartSec=1
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
";

// Reruns the installer daily: it changes nothing unless something drifted
// or a certificate is past two thirds of its life (D56), which it renews.
const RENEW_SERVICE: &str = "\
# Written by hyperstor-install.
[Unit]
Description=Hyperstor: renew certificates and repair the installation
Documentation=https://github.com/CMKL-APEX/hyperstor

[Service]
Type=oneshot
ExecStart=/usr/local/bin/hyperstor-install install
";

const RENEW_TIMER: &str = "\
# Written by hyperstor-install.
[Unit]
Description=Daily hyperstor-install run (certificate renewal)

[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=timers.target
";

const NODE_UNIT: &str = "\
# Written by hyperstor-install. One instance per storage node: hyperstord@N.
[Unit]
Description=Hyperstor storage node %i
Documentation=https://github.com/CMKL-APEX/hyperstor
After=network-online.target hyperstor-meta.service
Wants=network-online.target

[Service]
User=hyperstor
EnvironmentFile=/etc/hyperstor/node-%i.env
ExecStart=/usr/local/bin/hyperstord $HS_ARGS
Restart=on-failure
RestartSec=1
LimitMEMLOCK=infinity
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=DATA_DIR

[Install]
WantedBy=multi-user.target
";

/// What this host runs: the map service and which storage nodes.
struct Plan {
    cluster: String,
    cluster_id: String,
    /// The map service's listen address, if this host runs it.
    meta: Option<String>,
    /// The map service's address, for storage nodes.
    map: String,
    nodes: Vec<NodePlan>,
}

struct NodePlan {
    n: u32,
    /// One reactor per address, as the cluster file lists them.
    shards: Vec<std::net::SocketAddr>,
    admin: String,
}

/// The plan for `s`: the all-in-one trial cluster, or this host's part of
/// the cluster described by a cluster file (given now, or installed by an
/// earlier run).
fn plan(root: &Path, s: &Settings, cluster_file: Option<String>) -> Plan {
    if s.all_in_one {
        let id = cluster_id(root);
        let size = s.nodes.min(3);
        let min = if size == 3 { 2 } else { 1 };
        let mut cluster = format!(
            "# Written by hyperstor-install --all-in-one (docs: reference/cluster-file.md).\ncluster {id}\n"
        );
        for n in 0..s.nodes {
            cluster += &format!("node {n} rack 0 127.0.0.1:{}\n", s.node_port + n as u16);
        }
        // One host: `domain host` puts copies on different storage nodes,
        // which here are hyperstord instances.
        cluster += &format!(
            "pool 1 vol replicated size {size} min {min} pgs 64 chunk 4194304 domain host\n"
        );
        // A cache pool (D45): one copy, lost rather than recovered.
        cluster += "pool 2 cache cache copies 1 pgs 64 chunk 4194304 domain host\n";
        let map = format!("127.0.0.1:{}", s.map_port);
        return Plan {
            cluster,
            cluster_id: id.to_string(),
            meta: Some(map.clone()),
            map,
            nodes: (0..s.nodes)
                .map(|n| NodePlan {
                    n,
                    shards: vec![([127, 0, 0, 1], s.node_port + n as u16).into()],
                    admin: format!("127.0.0.1:{}", s.admin_port + n as u16),
                })
                .collect(),
        };
    }
    if s.role.is_empty() {
        fail("give --all-in-one, or --cluster FILE with --role meta|storage|both");
    }
    // Interim, until joining with tokens (D57, implementation-notes.md
    // I50): every host gets the same cluster file.
    let cluster = cluster_file
        .or_else(|| fs::read_to_string(root.join("etc/hyperstor/cluster.txt")).ok())
        .unwrap_or_else(|| fail("--cluster FILE is needed the first time"));
    let map = hyperstor_meta::parse_cluster(&cluster)
        .unwrap_or_else(|e| fail(&format!("cluster file: {e}")));
    if s.map.is_empty() {
        fail("--map ADDR is needed: where the map service listens");
    }
    let runs_meta = s.role == "meta" || s.role == "both";
    let mut nodes = Vec::new();
    if s.role == "storage" || s.role == "both" {
        let n = s
            .node
            .unwrap_or_else(|| fail("--node N is needed for a storage role"));
        let mut devs: Vec<_> = map.devices.values().filter(|d| d.node == n).collect();
        if devs.is_empty() {
            fail(&format!("the cluster file has no node {n}"));
        }
        devs.sort_by_key(|d| d.shard);
        let ip = devs[0].addr.ip();
        nodes.push(NodePlan {
            n,
            shards: devs.iter().map(|d| d.addr).collect(),
            admin: std::net::SocketAddr::new(ip, s.admin_port).to_string(),
        });
    }
    Plan {
        cluster,
        cluster_id: map.cluster_id.to_string(),
        meta: runs_meta.then(|| s.map.clone()),
        map: s.map.clone(),
        nodes,
    }
}

/// The files an install writes: final path, content, mode.
fn files(s: &Settings, p: &Plan) -> Vec<(String, String, u32)> {
    let data = s.data_dir.trim_end_matches('/');
    let mut out = vec![
        (
            "/etc/hyperstor/cluster.txt".to_string(),
            p.cluster.clone(),
            0o644,
        ),
        (
            "/etc/systemd/system/hyperstor-renew.service".to_string(),
            RENEW_SERVICE.to_string(),
            0o644,
        ),
        (
            "/etc/systemd/system/hyperstor-renew.timer".to_string(),
            RENEW_TIMER.to_string(),
            0o644,
        ),
    ];
    if let Some(listen) = &p.meta {
        out.extend([
            (
                "/etc/hyperstor/meta.env".to_string(),
                format!(
                    "HS_ARGS=--listen {listen} --api 127.0.0.1:{} --api-tls /etc/hyperstor/pki --cluster /etc/hyperstor/cluster.txt\n",
                    s.api_port
                ),
                0o644,
            ),
            (
                "/etc/hyperstor/hyperstorctl.conf".to_string(),
                format!("api 127.0.0.1:{}\ntls /etc/hyperstor/pki\n", s.api_port),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-meta.service".to_string(),
                META_UNIT.to_string(),
                0o644,
            ),
        ]);
    }
    if !p.nodes.is_empty() {
        out.push((
            "/etc/systemd/system/hyperstord@.service".to_string(),
            NODE_UNIT.replace("DATA_DIR", data),
            0o644,
        ));
    }
    for node in &p.nodes {
        let n = node.n;
        let shards: Vec<String> = node
            .shards
            .iter()
            .enumerate()
            .map(|(i, a)| {
                // One file per reactor; a single-reactor node keeps the
                // all-in-one's name.
                let file = if node.shards.len() == 1 {
                    format!("{data}/node-{n}.img")
                } else {
                    format!("{data}/node-{n}-{i}.img")
                };
                format!("--shard {a}={file}:{}", s.size_gib)
            })
            .collect();
        out.push((
            format!("/etc/hyperstor/node-{n}.env"),
            format!(
                "HS_ARGS=--node {n} --meta {} {} --admin {}\n",
                p.map,
                shards.join(" "),
                node.admin
            ),
            0o644,
        ));
    }
    out
}

/// The cluster id: kept from an existing cluster file, else fresh.
fn cluster_id(root: &Path) -> u64 {
    let existing = fs::read_to_string(root.join("etc/hyperstor/cluster.txt"))
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.strip_prefix("cluster ")
                    .and_then(|v| v.trim().parse().ok())
            })
        });
    existing.unwrap_or_else(|| {
        let mut v = 0u64;
        // SAFETY: getrandom writes at most 8 bytes into `v`.
        unsafe { libc::getrandom(&mut v as *mut u64 as *mut libc::c_void, 8, 0) };
        v >> 24 // short enough to read and type
    })
}

/// Writes `content` to `path` if it differs; returns whether it changed.
fn converge(path: &Path, content: &[u8], mode: u32) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if fs::read(path).is_ok_and(|c| c == content) {
        return false;
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap_or_else(|e| fail(&format!("create {}: {e}", dir.display())));
    }
    // Write beside and rename, so a crash never leaves half a file and a
    // running binary is replaced, not overwritten.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".hs-new");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, content).unwrap_or_else(|e| fail(&format!("write {}: {e}", tmp.display())));
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| fail(&format!("chmod {}: {e}", tmp.display())));
    fs::rename(&tmp, path).unwrap_or_else(|e| fail(&format!("rename {}: {e}", path.display())));
    true
}

fn under(root: &Path, p: &str) -> PathBuf {
    root.join(p.trim_start_matches('/'))
}

fn run(cmd: &str, args: &[&str]) -> bool {
    match Command::new(cmd).args(args).status() {
        Ok(st) => st.success(),
        Err(e) => {
            eprintln!("hyperstor-install: {cmd}: {e}");
            false
        }
    }
}

fn units(p: &Plan) -> Vec<String> {
    p.meta
        .iter()
        .map(|_| "hyperstor-meta.service".to_string())
        .chain(
            p.nodes
                .iter()
                .map(|n| format!("hyperstord@{}.service", n.n)),
        )
        .collect()
}

fn install(root: &Path, real: bool, no_start: bool, s: &Settings, cluster_file: Option<String>) {
    let p = plan(root, s, cluster_file);
    // SAFETY: geteuid has no preconditions.
    if real && unsafe { libc::geteuid() } != 0 {
        fail("installing to / needs root: run it with sudo (or use --root DIR)");
    }
    if real {
        println!("Checking the host:");
        if check(s) {
            fail("a host check failed; see above");
        }
        println!();
    }
    let mut changed: Vec<String> = Vec::new();

    // 1. Binaries, from beside this executable.
    let here = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| fail("can't find my own directory"));
    let mut binaries_changed = false;
    for b in BINARIES {
        let src = here.join(b);
        let bytes = fs::read(&src).unwrap_or_else(|e| {
            fail(&format!(
                "{}: {e} (build the workspace first: cargo build --release)",
                src.display()
            ))
        });
        let dst = format!("/usr/local/bin/{b}");
        if converge(&under(root, &dst), &bytes, 0o755) {
            binaries_changed = true;
            changed.push(dst);
        }
    }

    // The installer itself, for the renewal timer. Its own changes
    // restart nothing.
    let me = std::env::current_exe().unwrap_or_else(|_| fail("can't find my own path"));
    let bytes = fs::read(&me).unwrap_or_else(|e| fail(&format!("{}: {e}", me.display())));
    if converge(
        &under(root, "/usr/local/bin/hyperstor-install"),
        &bytes,
        0o755,
    ) {
        changed.push("/usr/local/bin/hyperstor-install".into());
    }

    // 2. The service user and the data directory.
    if real {
        let exists = Command::new("id")
            .arg("hyperstor")
            .output()
            .is_ok_and(|o| o.status.success());
        if !exists {
            let home = s.data_dir.as_str();
            if !run(
                "useradd",
                &[
                    "--system",
                    // The group too: the API's key is readable by it.
                    "--user-group",
                    "--no-create-home",
                    "--home-dir",
                    home,
                    "--shell",
                    "/usr/sbin/nologin",
                    "hyperstor",
                ],
            ) {
                fail("useradd hyperstor failed");
            }
            changed.push("user hyperstor".into());
        }
    }
    // Members of hyperstor-admin can read hyperstorctl's certificate.
    if real
        && !Command::new("getent")
            .args(["group", "hyperstor-admin"])
            .output()
            .is_ok_and(|o| o.status.success())
    {
        if !run("groupadd", &["--system", "hyperstor-admin"]) {
            fail("groupadd hyperstor-admin failed");
        }
        changed.push("group hyperstor-admin".into());
    }
    let data = under(root, &s.data_dir);
    if !data.exists() {
        fs::create_dir_all(&data)
            .unwrap_or_else(|e| fail(&format!("create {}: {e}", data.display())));
        changed.push(s.data_dir.clone());
    }
    if real && !run("chown", &["hyperstor:hyperstor", &s.data_dir]) {
        fail("chown of the data directory failed");
    }

    // 3. Certificates (§23.8), then configuration and units.
    let mut restart: Vec<String> = Vec::new();
    // The CAs and the API's certificate live where the map service runs.
    if p.meta.is_some() && pki(root, real, &p.cluster_id, &mut changed) {
        restart.push("hyperstor-meta.service".into());
    }
    let mut units_changed = false;
    let mut service_units_changed = false;
    for (path, content, mode) in files(s, &p) {
        if !converge(&under(root, &path), content.as_bytes(), mode) {
            continue;
        }
        if path.starts_with("/etc/systemd/") {
            units_changed = true;
        }
        if path.ends_with("hyperstor-meta.service") || path.ends_with("hyperstord@.service") {
            service_units_changed = true;
        }
        if path.ends_with("cluster.txt") || path.ends_with("meta.env") {
            restart.push("hyperstor-meta.service".into());
        }
        if let Some(n) = path
            .strip_prefix("/etc/hyperstor/node-")
            .and_then(|r| r.strip_suffix(".env"))
        {
            restart.push(format!("hyperstord@{n}.service"));
        }
        changed.push(path);
    }
    if binaries_changed || service_units_changed {
        restart = units(&p);
    }
    restart.dedup();
    // Saved last: a run that failed before here is retried with the same flags.
    if converge(
        &under(root, "/etc/hyperstor/install.conf"),
        s.save().as_bytes(),
        0o644,
    ) {
        changed.push("/etc/hyperstor/install.conf".into());
    }

    // 4. Services.
    if real && !no_start {
        if units_changed && !run("systemctl", &["daemon-reload"]) {
            fail("systemctl daemon-reload failed");
        }
        let mut all = units(&p);
        all.push("hyperstor-renew.timer".into());
        let mut args = vec!["enable", "--now"];
        args.extend(all.iter().map(String::as_str));
        if !run("systemctl", &args) {
            fail("systemctl enable --now failed; see journalctl -u hyperstor-meta");
        }
        if !restart.is_empty() {
            let mut args = vec!["restart"];
            args.extend(restart.iter().map(String::as_str));
            if !run("systemctl", &args) {
                fail("systemctl restart failed");
            }
            changed.push(format!("restarted {}", restart.join(" ")));
        }
    }

    if changed.is_empty() {
        println!("Nothing to change: hyperstor is installed as configured.");
    } else {
        println!("Changed:");
        for c in &changed {
            println!("  {c}");
        }
    }
    if real && !no_start {
        println!(
            "\nNext: hyperstorctl status, then hyperstorctl io-test (as root, or as a member of\nhyperstor-admin: sudo usermod -aG hyperstor-admin $USER, then log in again)"
        );
    }
}

/// The cluster's certificates in `/etc/hyperstor/pki` (§23.8, D53, D56):
///
/// - `root.*`, the root CA, made once. Here it stays on the host, readable
///   by root only; a multi-host cluster keeps it offline.
/// - `intermediate.*`, made once and renewed after two thirds of its year.
/// - `api.*`, the map service's server certificate (`meta/0`, for
///   127.0.0.1 and localhost), readable by the `hyperstor` group.
/// - `admin.*`, `hyperstorctl`'s client certificate (`admin/local`),
///   readable by root and the `hyperstor-admin` group.
///
/// Leaves are renewed after two thirds of their 30 days, or when the
/// intermediate changes. Returns whether the API's certificate changed.
fn pki(root: &Path, real: bool, cluster: &str, changed: &mut Vec<String>) -> bool {
    use hyperstor_pki::*;
    let dir = "/etc/hyperstor/pki";
    let read = |f: &str| fs::read_to_string(under(root, &format!("{dir}/{f}"))).ok();
    let pair = |name: &str| -> Option<Pem> {
        Some(Pem {
            cert: read(&format!("{name}.crt"))?,
            key: read(&format!("{name}.key"))?,
        })
    };
    let cid = cluster.to_string();
    let write = |name: &str, p: &Pem, key_mode: u32, changed: &mut Vec<String>| {
        converge(
            &under(root, &format!("{dir}/{name}.key")),
            p.key.as_bytes(),
            key_mode,
        );
        converge(
            &under(root, &format!("{dir}/{name}.crt")),
            p.cert.as_bytes(),
            0o644,
        );
        changed.push(format!("{dir}/{name}.crt"));
    };
    let rootca = match pair("root") {
        Some(p) => p,
        None => {
            let p = new_root(&cid).unwrap_or_else(|e| fail(&format!("root CA: {e}")));
            write("root", &p, 0o600, changed);
            p
        }
    };
    let mut renewed_ca = false;
    let ca = match pair("intermediate") {
        Some(p) if !due_for_renewal(&p.cert, INTERMEDIATE_DAYS) => p,
        _ => {
            let p = new_intermediate(&rootca, &cid)
                .unwrap_or_else(|e| fail(&format!("intermediate CA: {e}")));
            write("intermediate", &p, 0o600, changed);
            renewed_ca = true;
            p
        }
    };
    let mut api_changed = false;
    for (name, role, id, server, group) in [
        ("api", Role::Meta, "0", true, "hyperstor"),
        ("admin", Role::Admin, "local", false, "hyperstor-admin"),
    ] {
        if !renewed_ca && pair(name).is_some_and(|p| !due_for_renewal(&p.cert, LEAF_DAYS)) {
            continue;
        }
        let who = Identity {
            cluster: cid.clone(),
            role,
            id: id.into(),
        };
        let (ips, names) = if server {
            (
                vec!["127.0.0.1".parse().unwrap()],
                vec!["localhost".to_string()],
            )
        } else {
            (vec![], vec![])
        };
        let p = issue(&ca, &who, &ips, &names, LEAF_DAYS)
            .unwrap_or_else(|e| fail(&format!("{name} certificate: {e}")));
        write(name, &p, 0o640, changed);
        if real {
            let key = format!("{dir}/{name}.key");
            if !run("chgrp", &[group, &key]) {
                fail(&format!("chgrp {group} {key} failed"));
            }
        }
        api_changed |= server;
    }
    api_changed
}

fn uninstall(root: &Path, real: bool, s: &Settings) {
    // SAFETY: geteuid has no preconditions.
    if real && unsafe { libc::geteuid() } != 0 {
        fail("uninstalling from / needs root: run it with sudo (or use --root DIR)");
    }
    if real {
        // What was installed: the map service if its options are, and
        // every storage node with an options file.
        let mut all: Vec<String> = Vec::new();
        if under(root, "/etc/hyperstor/meta.env").exists() {
            all.push("hyperstor-meta.service".into());
        }
        for e in fs::read_dir(under(root, "/etc/hyperstor"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name
                .strip_prefix("node-")
                .and_then(|r| r.strip_suffix(".env"))
            {
                all.push(format!("hyperstord@{n}.service"));
            }
        }
        all.push("hyperstor-renew.timer".into());
        let mut args = vec!["disable", "--now"];
        args.extend(all.iter().map(String::as_str));
        // Units that were never installed make this fail; that's fine.
        let _ = run("systemctl", &args);
    }
    let mut removed = Vec::new();
    let paths = [
        "/etc/systemd/system/hyperstor-meta.service".to_string(),
        "/etc/systemd/system/hyperstord@.service".to_string(),
        "/etc/systemd/system/hyperstor-renew.service".to_string(),
        "/etc/systemd/system/hyperstor-renew.timer".to_string(),
        "/usr/local/bin/hyperstor-install".to_string(),
    ]
    .into_iter()
    .chain(BINARIES.iter().map(|b| format!("/usr/local/bin/{b}")));
    for p in paths {
        if fs::remove_file(under(root, &p)).is_ok() {
            removed.push(p);
        }
    }
    if real && !removed.is_empty() {
        let _ = run("systemctl", &["daemon-reload"]);
    }
    for r in &removed {
        println!("removed {r}");
    }
    println!(
        "Kept /etc/hyperstor and {} (configuration and data); remove them by hand to start over.",
        s.data_dir
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_flags_override() {
        let mut s = Settings::defaults();
        s.all_in_one = true;
        s.nodes = 5;
        s.data_dir = "/srv/hs".into();
        let mut t = Settings::defaults();
        t.apply(&s.save());
        assert_eq!(s, t);
        t.apply("nodes = 2\n");
        assert_eq!(t.nodes, 2);
        assert_eq!(t.data_dir, "/srv/hs");
    }

    #[test]
    fn cluster_file_roles_install_their_part() {
        let cluster = "cluster 9\nnode 0 rack 0 10.0.0.1:9100 10.0.0.1:9101\nnode 1 rack 1 10.0.0.2:9100\n\
                       pool 1 vol replicated size 2 min 1 pgs 8 domain host\n";
        let mut s = Settings::defaults();
        s.role = "storage".into();
        s.node = Some(0);
        s.map = "10.0.0.9:8849".into();
        let p = plan(Path::new("/nonexistent"), &s, Some(cluster.into()));
        assert!(p.meta.is_none());
        let f = files(&s, &p);
        let env = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/node-0.env")
            .unwrap()
            .1;
        // One reactor per address the file lists for node 0.
        assert!(env.contains("--meta 10.0.0.9:8849"), "{env}");
        assert!(
            env.contains("--shard 10.0.0.1:9100=/var/lib/hyperstor/node-0-0.img:4"),
            "{env}"
        );
        assert!(
            env.contains("--shard 10.0.0.1:9101=/var/lib/hyperstor/node-0-1.img:4"),
            "{env}"
        );
        assert!(env.contains("--admin 10.0.0.1:8853"), "{env}");
        assert!(
            !f.iter()
                .any(|x| x.0.ends_with("meta.env") || x.0.ends_with("node-1.env"))
        );
        // The map service's host: its options and hyperstorctl's, no node.
        s.role = "meta".into();
        let p = plan(Path::new("/nonexistent"), &s, Some(cluster.into()));
        assert_eq!(p.cluster_id, "9");
        let f = files(&s, &p);
        let env = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/meta.env")
            .unwrap()
            .1;
        assert!(env.contains("--listen 10.0.0.9:8849"), "{env}");
        assert!(f.iter().any(|x| x.0.ends_with("hyperstorctl.conf")));
        assert!(!f.iter().any(|x| x.0.contains("/node-")));
        assert_eq!(units(&p), ["hyperstor-meta.service"]);
    }

    #[test]
    fn all_in_one_files_match_the_settings() {
        let mut s = Settings::defaults();
        s.nodes = 2;
        s.all_in_one = true;
        let p = plan(Path::new("/nonexistent"), &s, None);
        let f = files(&s, &p);
        let cluster = &f.iter().find(|x| x.0.ends_with("cluster.txt")).unwrap().1;
        // The map service accepts it, with every PG placed.
        let map = hyperstor_meta::parse_cluster(cluster).unwrap();
        assert_eq!(map.cluster_id.to_string(), p.cluster_id);
        assert_eq!(map.nodes.len(), 2);
        assert!(
            map.pgs
                .range((1, 0)..=(1, u32::MAX))
                .all(|(_, pg)| pg.acting_count() == 2)
        );
        // And the cache pool: one copy each.
        assert_eq!(map.pools[&2].kind, hyperstor_meta::PoolKind::Cache);
        assert!(
            map.pgs
                .range((2, 0)..=(2, u32::MAX))
                .all(|(_, pg)| pg.acting_count() == 1)
        );
        // Two nodes: two copies, writes go on with one.
        assert!(cluster.contains("size 2 min 1"));
        assert_eq!(f.iter().filter(|x| x.0.contains("/node-")).count(), 2);
        let unit = &f
            .iter()
            .find(|x| x.0.ends_with("hyperstord@.service"))
            .unwrap()
            .1;
        assert!(unit.contains("ReadWritePaths=/var/lib/hyperstor\n"));
    }
}
