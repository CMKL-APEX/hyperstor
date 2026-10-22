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
//! - `install --join API --token-file F --role storage --address IP
//!   [--reactors N] [--rack R]` joins this host to a running cluster with a
//!   token from `hyperstorctl token create --role node` (§23.4.2, D57):
//!   it makes its own key, pins the cluster's root by the token, is added
//!   to the map as a new storage node, and installs it with N reactors on
//!   IP, ports from `--node-port`. `--role meta --members LIST --member-id
//!   N --join API` joins a map service member with a `--role meta` token
//!   instead: it gets the cluster file and the intermediate CA. The token
//!   is read from `--token-file`, `--token` or `$HYPERSTOR_JOIN_TOKEN` and
//!   never saved; later runs renew the host's certificate over the API.
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
//! - `root-backup --out FILE --passphrase-file F [--remove-key]` writes the
//!   root CA (certificate and key) to FILE, encrypted with the passphrase
//!   (D84), checks that it opens, and records the backup; with
//!   `--remove-key` it then removes the key from this host, which keeps the
//!   root offline (D70). `root-restore --in FILE --passphrase-file F` puts
//!   it back, on this host or a new one. The passphrase may also come from
//!   `$HYPERSTOR_BACKUP_PASSPHRASE`; never from the command line.
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

const USAGE: &str = "usage: hyperstor-install check|install|uninstall|root-backup|root-restore [--all-in-one] [--cluster FILE] [--join API] [--token-file F] [--token T] [--address IP] [--reactors N] [--rack R] [--role R] [--node N] [--map ADDR] [--members LIST] [--member-id N] [--nodes N] [--size-gib G] [--data-dir DIR] [--api-port P] [--map-port P] [--node-port P] [--admin-port P] [--mgmt-port P] [--root DIR] [--no-start] [--out FILE] [--in FILE] [--passphrase-file F] [--remove-key]";

const BINARIES: [&str; 4] = [
    "hyperstor-meta",
    "hyperstord",
    "hyperstorctl",
    "hyperstor-mgmt",
];

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
    /// The map service's members, `N=RAFT_ADDR/MAP_ADDR` separated by
    /// commas, when it runs as a group (decisions.md D72–D74); empty: one.
    members: String,
    /// This host's member number, for a `meta` role in a group.
    member_id: Option<u32>,
    /// The cluster API this host joined through (§23.4.2); empty when it
    /// was installed from a cluster file.
    join: String,
    /// A joined storage host's IP: its reactors listen there.
    address: String,
    /// A joined storage host's reactors, one device each.
    reactors: u32,
    /// A joined storage host's rack.
    rack: u32,
    nodes: u32,
    size_gib: u64,
    data_dir: String,
    api_port: u16,
    map_port: u16,
    node_port: u16,
    admin_port: u16,
    mgmt_port: u16,
}

impl Settings {
    fn defaults() -> Self {
        Settings {
            all_in_one: false,
            role: String::new(),
            node: None,
            map: String::new(),
            members: String::new(),
            member_id: None,
            join: String::new(),
            address: String::new(),
            reactors: 1,
            rack: 0,
            nodes: 3,
            size_gib: 4,
            data_dir: "/var/lib/hyperstor".into(),
            api_port: 8850,
            map_port: 8849,
            node_port: 9100,
            admin_port: 8853,
            mgmt_port: 8852,
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
                "members" => self.members = v.to_string(),
                "member_id" => self.member_id = v.parse().ok(),
                "join" => self.join = v.to_string(),
                "address" => self.address = v.to_string(),
                "reactors" => self.reactors = v.parse().unwrap_or(self.reactors),
                "rack" => self.rack = v.parse().unwrap_or(self.rack),
                "nodes" => self.nodes = v.parse().unwrap_or(self.nodes),
                "size_gib" => self.size_gib = v.parse().unwrap_or(self.size_gib),
                "data_dir" => self.data_dir = v.to_string(),
                "api_port" => self.api_port = v.parse().unwrap_or(self.api_port),
                "map_port" => self.map_port = v.parse().unwrap_or(self.map_port),
                "node_port" => self.node_port = v.parse().unwrap_or(self.node_port),
                "admin_port" => self.admin_port = v.parse().unwrap_or(self.admin_port),
                "mgmt_port" => self.mgmt_port = v.parse().unwrap_or(self.mgmt_port),
                _ => {}
            }
        }
    }

    fn save(&self) -> String {
        format!(
            "# Written by hyperstor-install; flags given on the command line override these.\n\
             all_in_one = {}\nrole = {}\nnode = {}\nmap = {}\nmembers = {}\nmember_id = {}\njoin = {}\naddress = {}\nreactors = {}\nrack = {}\nnodes = {}\nsize_gib = {}\ndata_dir = {}\napi_port = {}\nmap_port = {}\nnode_port = {}\nadmin_port = {}\nmgmt_port = {}\n",
            self.all_in_one,
            self.role,
            self.node.map(|n| n.to_string()).unwrap_or_default(),
            self.map,
            self.members,
            self.member_id.map(|n| n.to_string()).unwrap_or_default(),
            self.join,
            self.address,
            self.reactors,
            self.rack,
            self.nodes,
            self.size_gib,
            self.data_dir,
            self.api_port,
            self.map_port,
            self.node_port,
            self.admin_port,
            self.mgmt_port
        )
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        usage()
    };
    if !matches!(
        cmd.as_str(),
        "check" | "install" | "uninstall" | "root-backup" | "root-restore"
    ) {
        usage();
    }
    let mut root = PathBuf::from("/");
    let mut no_start = false;
    let mut cluster_file: Option<String> = None;
    let (mut out_file, mut in_file, mut remove_key) = (None, None, false);
    let mut passphrase: Option<String> = std::env::var("HYPERSTOR_BACKUP_PASSPHRASE").ok();
    // Never saved: a token is single-use and secret (§23.10).
    let mut token: Option<String> = std::env::var("HYPERSTOR_JOIN_TOKEN").ok();
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
            "--token-file" => {
                let f = val();
                token = Some(fs::read_to_string(&f).unwrap_or_else(|e| fail(&format!("{f}: {e}"))));
                continue;
            }
            "--token" => {
                token = Some(val());
                continue;
            }
            "--out" => {
                out_file = Some(val());
                continue;
            }
            "--in" => {
                in_file = Some(val());
                continue;
            }
            "--remove-key" => {
                remove_key = true;
                continue;
            }
            "--passphrase-file" => {
                let f = val();
                let p = fs::read_to_string(&f).unwrap_or_else(|e| fail(&format!("{f}: {e}")));
                passphrase = Some(p.trim_end_matches(['\n', '\r']).to_string());
                continue;
            }
            "--join" => "join",
            "--address" => "address",
            "--reactors" => "reactors",
            "--rack" => "rack",
            "--role" => "role",
            "--node" => "node",
            "--map" => "map",
            "--members" => "members",
            "--member-id" => "member_id",
            "--nodes" => "nodes",
            "--size-gib" => "size_gib",
            "--data-dir" => "data_dir",
            "--api-port" => "api_port",
            "--map-port" => "map_port",
            "--node-port" => "node_port",
            "--admin-port" => "admin_port",
            "--mgmt-port" => "mgmt_port",
            _ => usage(),
        };
        let v = val();
        let ok = match key {
            "nodes" => v.parse::<u32>().is_ok_and(|n| (1..=16).contains(&n)),
            "size_gib" => v.parse::<u64>().is_ok_and(|n| n >= 1),
            "api_port" | "map_port" | "node_port" | "admin_port" | "mgmt_port" => {
                v.parse::<u16>().is_ok_and(|n| n > 0)
            }
            "role" => matches!(v.as_str(), "meta" | "storage" | "both"),
            "node" => v.parse::<u32>().is_ok(),
            "map" => v.parse::<std::net::SocketAddr>().is_ok(),
            "members" => parse_members(&v).is_ok(),
            "member_id" => v.parse::<u32>().is_ok(),
            "join" => v.parse::<std::net::SocketAddr>().is_ok(),
            "address" => v
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| !ip.is_unspecified()),
            "reactors" => v.parse::<u32>().is_ok_and(|n| (1..=64).contains(&n)),
            "rack" => v.parse::<u32>().is_ok(),
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
    let token = token.filter(|t| !t.trim().is_empty());
    s.apply(&fs::read_to_string(root.join("etc/hyperstor/install.conf")).unwrap_or_default());
    s.apply(
        &given
            .iter()
            .map(|(k, v)| format!("{k} = {v}\n"))
            .collect::<String>(),
    );

    // --out, --in and --remove-key belong to the backup commands only.
    let backup = cmd.starts_with("root-");
    if (!backup && (out_file.is_some() || in_file.is_some() || remove_key))
        || (cmd == "root-backup" && in_file.is_some())
        || (cmd == "root-restore" && (out_file.is_some() || remove_key))
    {
        usage();
    }
    match cmd.as_str() {
        "check" => std::process::exit(i32::from(check(&root, &s))),
        "root-backup" => root_backup(
            &root,
            &out_file.unwrap_or_else(|| usage()),
            &passphrase.unwrap_or_else(|| fail("a passphrase is needed: --passphrase-file F")),
            remove_key,
        ),
        "root-restore" => root_restore(
            &root,
            &in_file.unwrap_or_else(|| usage()),
            &passphrase.unwrap_or_else(|| fail("a passphrase is needed: --passphrase-file F")),
        ),
        "uninstall" => uninstall(&root, real, &s),
        _ => {
            let mut cluster_file = cluster_file;
            if !s.join.is_empty() {
                // SAFETY: geteuid has no preconditions.
                if real && unsafe { libc::geteuid() } != 0 {
                    fail("installing to / needs root: run it with sudo (or use --root DIR)");
                }
                if let Some(f) = join(&root, real, &mut s, token.as_deref()) {
                    cluster_file = Some(f);
                }
            }
            install(&root, real, no_start, &s, cluster_file)
        }
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
fn check(root: &Path, s: &Settings) -> bool {
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

    // The root CA's key (D84): backed up, kept elsewhere, or at risk.
    let pki = under(root, "/etc/hyperstor/pki");
    match (
        pki.join("root.key").exists(),
        fs::read_to_string(pki.join("root.backup")).ok(),
    ) {
        (_, Some(b)) => note(Level::Ok, "rootkey", b.trim().to_string()),
        (true, None) => note(
            Level::Warn,
            "rootkey",
            "the root CA's key has no backup: hyperstor-install root-backup --out FILE --passphrase-file F".into(),
        ),
        (false, None) => note(Level::Info, "rootkey", "no root CA key on this host".into()),
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
# Options for the next start only, such as --restore FILE; absent normally.
EnvironmentFile=-/etc/hyperstor/meta-once.env
ExecStart=/usr/local/bin/hyperstor-meta $HS_ARGS $HS_ONCE
# The intermediate CA's key, readable by root only on disk, handed to the
# service alone (spec/administration.md §23.8).
LoadCredential=intermediate.key:/etc/hyperstor/pki/intermediate.key
# The local socket, /run/hyperstor-meta/api.sock (break-glass).
RuntimeDirectory=hyperstor-meta
RuntimeDirectoryMode=0755
Restart=on-failure
RestartSec=1
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
# The map's store and Raft log (DATA_DIR/meta).
ReadWritePaths=DATA_DIR

[Install]
WantedBy=multi-user.target
";

const MGMT_UNIT: &str = "\
# Written by hyperstor-install. The read-only dashboard, on loopback.
[Unit]
Description=Hyperstor management service (dashboard)
Documentation=https://github.com/CMKL-APEX/hyperstor
After=network-online.target hyperstor-meta.service
Wants=network-online.target

[Service]
User=hyperstor
EnvironmentFile=/etc/hyperstor/mgmt.env
ExecStart=/usr/local/bin/hyperstor-mgmt $HS_ARGS
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
    /// The map service's address, for storage nodes: every member's,
    /// separated by commas, when it runs as a group.
    map: String,
    /// The group, and this host's member number if it runs one.
    members: Vec<Member>,
    member_id: Option<u32>,
    /// Addresses the map service's certificate must name besides
    /// loopback: where members and nodes reach it.
    server_ips: Vec<std::net::IpAddr>,
    nodes: Vec<NodePlan>,
    /// Every storage node's metrics address, for the dashboard.
    metrics: Vec<String>,
    /// Where this host's cluster API listens, if it runs the map service:
    /// loopback for the trial, the member's own address otherwise, so
    /// joining hosts reach it (D59).
    api: String,
}

/// One map service member: `N=RAFT_ADDR/MAP_ADDR`.
#[derive(Debug, Clone, PartialEq)]
struct Member {
    id: u32,
    raft: std::net::SocketAddr,
    map: std::net::SocketAddr,
}

fn parse_members(s: &str) -> Result<Vec<Member>, String> {
    let mut out: Vec<Member> = Vec::new();
    for m in s.split(',').filter(|m| !m.is_empty()) {
        let bad = || format!("{m}: expected N=RAFT_ADDR/MAP_ADDR");
        let (id, addrs) = m.split_once('=').ok_or_else(bad)?;
        let (raft, map) = addrs.split_once('/').ok_or_else(bad)?;
        let m = Member {
            id: id.parse().map_err(|_| bad())?,
            raft: raft.parse().map_err(|_| bad())?,
            map: map.parse().map_err(|_| bad())?,
        };
        if out.iter().any(|o| o.id == m.id) {
            return Err(format!("member {} given twice", m.id));
        }
        out.push(m);
    }
    Ok(out)
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
        let metrics = (0..s.nodes)
            .map(|n| format!("127.0.0.1:{}", s.admin_port + n as u16))
            .collect();
        return Plan {
            api: format!("127.0.0.1:{}", s.api_port),
            metrics,
            cluster,
            cluster_id: id.to_string(),
            meta: Some(map.clone()),
            map,
            members: Vec::new(),
            member_id: None,
            server_ips: Vec::new(),
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
        fail(
            "give --all-in-one, --cluster FILE with --role meta|storage|both, or --join API with --role",
        );
    }
    if !s.join.is_empty() && s.role == "storage" {
        return joined_storage_plan(s);
    }
    // Interim, until joining with tokens (D57, implementation-notes.md
    // I50): every host gets the same cluster file.
    let cluster = cluster_file
        .or_else(|| fs::read_to_string(root.join("etc/hyperstor/cluster.txt")).ok())
        .unwrap_or_else(|| fail("--cluster FILE is needed the first time"));
    let map = hyperstor_meta::parse_cluster(&cluster)
        .unwrap_or_else(|e| fail(&format!("cluster file: {e}")));
    let runs_meta = s.role == "meta" || s.role == "both";
    let members = parse_members(&s.members).unwrap_or_else(|e| fail(&format!("--members: {e}")));
    // Where this host's map service listens, and where nodes find it.
    let (listen, nodes_map, server_ips) = if members.is_empty() {
        if s.map.is_empty() {
            fail("--map ADDR is needed: where the map service listens");
        }
        let ip = s.map.parse::<std::net::SocketAddr>().map(|a| a.ip()).ok();
        (
            s.map.clone(),
            s.map.clone(),
            ip.into_iter().collect::<Vec<_>>(),
        )
    } else {
        let all: Vec<String> = members.iter().map(|m| m.map.to_string()).collect();
        let mine = if runs_meta {
            let id = s
                .member_id
                .unwrap_or_else(|| fail("--member-id N is needed: which member this host runs"));
            Some(
                members
                    .iter()
                    .find(|m| m.id == id)
                    .unwrap_or_else(|| fail(&format!("member {id} is not in --members"))),
            )
        } else {
            None
        };
        (
            mine.map(|m| m.map.to_string()).unwrap_or_default(),
            all.join(","),
            mine.map(|m| vec![m.map.ip(), m.raft.ip()])
                .unwrap_or_default(),
        )
    };
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
    // The dashboard reads every node's metrics: its first address's IP,
    // the metrics port, as each storage host is installed.
    let metrics = map
        .nodes
        .keys()
        .filter_map(|n| {
            let d = map
                .devices
                .values()
                .filter(|d| d.node == *n)
                .min_by_key(|d| d.shard)?;
            Some(std::net::SocketAddr::new(d.addr.ip(), s.admin_port).to_string())
        })
        .collect();
    let api = listen
        .parse::<std::net::SocketAddr>()
        .map(|a| std::net::SocketAddr::new(a.ip(), s.api_port).to_string())
        .unwrap_or_else(|_| format!("127.0.0.1:{}", s.api_port));
    Plan {
        api,
        metrics,
        cluster,
        cluster_id: map.cluster_id.to_string(),
        meta: runs_meta.then_some(listen),
        map: nodes_map,
        member_id: if members.is_empty() {
            None
        } else {
            s.member_id
        },
        members,
        server_ips: server_ips
            .into_iter()
            .filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
            .collect(),
        nodes,
    }
}

/// A storage host that joined with a token: node N of the map (from the
/// join), `reactors` devices on `address`, the map service's addresses as
/// the join gave them.
fn joined_storage_plan(s: &Settings) -> Plan {
    let n = s
        .node
        .unwrap_or_else(|| fail("this host hasn't joined yet: give a token (--token-file F)"));
    let ip: std::net::IpAddr = s
        .address
        .parse()
        .unwrap_or_else(|_| fail("--address IP is needed: where this host's reactors listen"));
    Plan {
        api: String::new(),
        cluster: String::new(),
        cluster_id: String::new(),
        meta: None,
        map: s.map.clone(),
        members: Vec::new(),
        member_id: None,
        server_ips: Vec::new(),
        nodes: vec![NodePlan {
            n,
            shards: (0..s.reactors)
                .map(|i| std::net::SocketAddr::new(ip, s.node_port + i as u16))
                .collect(),
            admin: std::net::SocketAddr::new(ip, s.admin_port).to_string(),
        }],
        metrics: Vec::new(),
    }
}

/// Joins this host to the cluster at `s.join` with `token` (§23.4.2,
/// D57), unless it has already joined: then a storage host's certificate
/// is renewed over the API when due. Records the node number and the map
/// service's addresses in `s`; returns the cluster file for a map service
/// member.
fn join(root: &Path, real: bool, s: &mut Settings, token: Option<&str>) -> Option<String> {
    let dir = under(root, "/etc/hyperstor/pki");
    let (cert_name, key_group) = match s.role.as_str() {
        "storage" => ("node", "hyperstor"),
        "meta" => ("api", "hyperstor"),
        _ => fail("--join installs one role: --role storage or --role meta"),
    };
    if dir.join(format!("{cert_name}.crt")).exists() && (s.role == "meta" || s.node.is_some()) {
        if s.role == "storage" {
            renew_node(root, real, s);
        }
        return None;
    }
    let token = token.unwrap_or_else(|| {
        fail("joining needs a token: --token-file F (from hyperstorctl token create)")
    });
    let (pin, _) = hyperstor_meta::api::parse_token(token)
        .unwrap_or_else(|| fail("the token isn't a hyperstor join token (hsj1-…)"));
    let (key, csr) = hyperstor_pki::new_request().unwrap_or_else(|e| fail(&format!("key: {e}")));
    let mut req = hyperstor_api::JoinRequest {
        token: token.trim().to_string(),
        csr,
        ..Default::default()
    };
    if s.role == "storage" {
        let ip: std::net::IpAddr = s
            .address
            .parse()
            .unwrap_or_else(|_| fail("--address IP is needed: where this host's reactors listen"));
        req.addresses = (0..s.reactors)
            .map(|i| std::net::SocketAddr::new(ip, s.node_port + i as u16).to_string())
            .collect();
        req.rack = s.rack;
    } else {
        let members =
            parse_members(&s.members).unwrap_or_else(|e| fail(&format!("--members: {e}")));
        let id = s
            .member_id
            .unwrap_or_else(|| fail("--member-id N is needed: which member this host runs"));
        let me = members
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| fail(&format!("member {id} is not in --members")));
        req.member_id = id;
        req.member_ips = vec![me.map.ip().to_string(), me.raft.ip().to_string()];
    }
    let tls = hyperstor_pki::pinned_client_config(pin).unwrap_or_else(|e| fail(&e));
    let r = api_call(&s.join, tls, |c| async move {
        hyperstor_api::TokenServiceClient::new(c.0, c.1)
            .join(req)
            .await
            .map(|r| r.into_owned())
    })
    .unwrap_or_else(|e| fail(&format!("join through {}: {e}", s.join)));
    let pki = "/etc/hyperstor/pki";
    let write = |name: &str, content: &str, mode: u32| {
        converge(
            &under(root, &format!("{pki}/{name}")),
            content.as_bytes(),
            mode,
        );
    };
    write("root.crt", &r.root, 0o644);
    let chain = if s.role == "meta" {
        // A member's server certificate carries the root, so the hosts
        // that join through it can pin it.
        format!("{}{}", r.certificate, r.root)
    } else {
        r.certificate.clone()
    };
    write(&format!("{cert_name}.key"), &key, 0o640);
    write(&format!("{cert_name}.crt"), &chain, 0o644);
    if real {
        let k = format!("{pki}/{cert_name}.key");
        if !run("chgrp", &[key_group, &k]) {
            fail(&format!("chgrp {key_group} {k} failed"));
        }
    }
    println!("Joined cluster {} through {}.", r.cluster_id, s.join);
    if s.role == "storage" {
        s.node = Some(r.node);
        s.map = r.map_addresses.clone();
        println!("This host is storage node {}.", r.node);
        None
    } else {
        write("intermediate.crt", &r.intermediate, 0o644);
        write("intermediate.key", &r.intermediate_key, 0o600);
        Some(r.cluster_file.clone())
    }
}

/// Renews a joined storage host's certificate over the API, with the
/// certificate itself (D56), once it is past two thirds of its life.
fn renew_node(root: &Path, real: bool, s: &Settings) {
    let dir = under(root, "/etc/hyperstor/pki");
    let read = |f: &str| fs::read_to_string(dir.join(f)).unwrap_or_default();
    let cert = read("node.crt");
    if !hyperstor_pki::due_for_renewal(&cert, hyperstor_pki::LEAF_DAYS) {
        return;
    }
    let tls = hyperstor_pki::client_config(&cert, &read("node.key"), &read("root.crt"))
        .unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display())));
    let (key, csr) = hyperstor_pki::new_request().unwrap_or_else(|e| fail(&format!("key: {e}")));
    let r = api_call(&s.join, tls, |c| async move {
        hyperstor_api::CaServiceClient::new(c.0, c.1)
            .renew_certificate(hyperstor_api::RenewCertificateRequest {
                csr,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned())
    });
    match r {
        Ok(r) => {
            converge(&dir.join("node.key"), key.as_bytes(), 0o640);
            converge(&dir.join("node.crt"), r.certificate.as_bytes(), 0o644);
            if real && !run("chgrp", &["hyperstor", "/etc/hyperstor/pki/node.key"]) {
                fail("chgrp hyperstor /etc/hyperstor/pki/node.key failed");
            }
            println!("Renewed /etc/hyperstor/pki/node.crt.");
        }
        // Not fatal: the next daily run tries again, and the certificate
        // has a third of its life left.
        Err(e) => eprintln!(
            "hyperstor-install: renewing node.crt through {}: {e}",
            s.join
        ),
    }
}

/// One call to the cluster API at `addr` over TLS, on a small runtime.
fn api_call<T, F>(
    addr: &str,
    tls: hyperstor_pki::ClientConfig,
    call: impl FnOnce(
        (
            connectrpc::client::HttpClient,
            connectrpc::client::ClientConfig,
        ),
    ) -> F,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, connectrpc::ConnectError>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let uri: http::Uri = format!("https://{addr}")
        .parse()
        .map_err(|_| format!("{addr}: not an address"))?;
    let client = connectrpc::client::HttpClient::with_tls(std::sync::Arc::new(tls));
    let config = connectrpc::client::ClientConfig::new(uri)
        .with_default_timeout(std::time::Duration::from_secs(10));
    rt.block_on(call((client, config))).map_err(|e| {
        let reason = hyperstor_meta::api::reason_of(&e)
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        let mut msg = e.message.clone().unwrap_or_else(|| "request failed".into());
        // The transport's causes, such as a certificate that doesn't
        // chain to the token's root.
        let mut src = std::error::Error::source(&e);
        while let Some(c) = src {
            let t = c.to_string();
            if !msg.contains(&t) {
                msg = format!("{msg}: {t}");
            }
            src = c.source();
        }
        format!("{msg}{reason}")
    })
}

/// The map service's group options (D72–D74): this member and every
/// member, or nothing for a map service of one.
fn group_args(p: &Plan) -> String {
    let Some(id) = p.member_id else {
        return String::new();
    };
    let mut out = format!(" --id {id}");
    for m in &p.members {
        out.push_str(&format!(" --member {}={}/{}", m.id, m.raft, m.map));
    }
    out
}

/// The files an install writes: final path, content, mode.
fn files(s: &Settings, p: &Plan) -> Vec<(String, String, u32)> {
    let data = s.data_dir.trim_end_matches('/');
    let mut out = vec![
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
    if !p.cluster.is_empty() {
        out.push((
            "/etc/hyperstor/cluster.txt".to_string(),
            p.cluster.clone(),
            0o644,
        ));
    }
    if let Some(listen) = &p.meta {
        out.extend([
            (
                "/etc/hyperstor/meta.env".to_string(),
                format!(
                    "HS_ARGS=--listen {listen} --api {} --api-tls /etc/hyperstor/pki --ca /etc/hyperstor/pki --api-socket /run/hyperstor-meta/api.sock --cluster /etc/hyperstor/cluster.txt --data {data}/meta --backup-dir {data}/meta-backups{}\n",
                    p.api,
                    group_args(p)
                ),
                0o644,
            ),
            (
                "/etc/hyperstor/hyperstorctl.conf".to_string(),
                format!(
                    "api {}\ntls /etc/hyperstor/pki\nsocket /run/hyperstor-meta/api.sock\n",
                    p.api
                ),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-meta.service".to_string(),
                META_UNIT.replace("DATA_DIR", data),
                0o644,
            ),
            (
                "/etc/hyperstor/mgmt.env".to_string(),
                format!(
                    "HS_ARGS=--listen 127.0.0.1:{} --api {} --tls /etc/hyperstor/pki --metrics {}\n",
                    s.mgmt_port,
                    p.api,
                    p.metrics.join(",")
                ),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-mgmt.service".to_string(),
                MGMT_UNIT.to_string(),
                0o644,
            ),
            // For Prometheus's file_sd_configs (deploy/prometheus/hyperstor.yml).
            (
                "/etc/hyperstor/prometheus-targets.json".to_string(),
                format!(
                    "[{{\"targets\": [{}], \"labels\": {{\"cluster\": \"{}\"}}}}]\n",
                    p.metrics
                        .iter()
                        .map(|m| format!("\"{m}\""))
                        .collect::<Vec<_>>()
                        .join(", "),
                    p.cluster_id
                ),
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
        .flat_map(|_| {
            [
                "hyperstor-meta.service".to_string(),
                "hyperstor-mgmt.service".to_string(),
            ]
        })
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
        if check(root, s) {
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
    let mut binaries_changed: Vec<&str> = Vec::new();
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
            binaries_changed.push(b);
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
    if p.meta.is_some() {
        let member = p.member_id.unwrap_or(0).to_string();
        restart.extend(pki(
            root,
            real,
            &p.cluster_id,
            &member,
            &p.server_ips,
            &mut changed,
        ));
    }
    let mut units_changed = false;
    let mut service_units_changed: Vec<&str> = Vec::new();
    for (path, content, mode) in files(s, &p) {
        if !converge(&under(root, &path), content.as_bytes(), mode) {
            continue;
        }
        if path.starts_with("/etc/systemd/") {
            units_changed = true;
        }
        // A changed unit or binary restarts only its own services.
        if path.ends_with("hyperstor-meta.service") {
            service_units_changed.push("hyperstor-meta");
        }
        if path.ends_with("hyperstor-mgmt.service") {
            service_units_changed.push("hyperstor-mgmt");
        }
        if path.ends_with("hyperstord@.service") {
            service_units_changed.push("hyperstord");
        }
        if path.ends_with("cluster.txt") || path.ends_with("meta.env") {
            restart.push("hyperstor-meta.service".into());
        }
        if path.ends_with("mgmt.env") {
            restart.push("hyperstor-mgmt.service".into());
        }
        if let Some(n) = path
            .strip_prefix("/etc/hyperstor/node-")
            .and_then(|r| r.strip_suffix(".env"))
        {
            restart.push(format!("hyperstord@{n}.service"));
        }
        changed.push(path);
    }
    for what in binaries_changed.iter().chain(&service_units_changed) {
        restart.extend(units(&p).into_iter().filter(|u| match *what {
            "hyperstord" => u.starts_with("hyperstord@"),
            "hyperstor-meta" => u == "hyperstor-meta.service",
            "hyperstor-mgmt" => u == "hyperstor-mgmt.service",
            _ => false,
        }));
    }
    restart.sort();
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

    if !(real && !no_start) && !restart.is_empty() {
        // Not started here: say what a real run would restart.
        changed.push(format!("would restart {}", restart.join(" ")));
    }
    if changed.is_empty() {
        println!("Nothing to change: hyperstor is installed as configured.");
    } else {
        println!("Changed:");
        for c in &changed {
            println!("  {c}");
        }
    }
    let pki_dir = under(root, "/etc/hyperstor/pki");
    if pki_dir.join("root.key").exists() && !pki_dir.join("root.backup").exists() {
        println!(
            "\nBack up the root CA's key, which only this host has (it signs a new intermediate CA\n\
             every 9 months; without it the cluster needs a new root within a year):\n  \
             sudo hyperstor-install root-backup --out FILE --passphrase-file F [--remove-key]"
        );
    }
    if real && !no_start && p.meta.is_some() {
        println!(
            "\nNext: hyperstorctl status, then hyperstorctl io-test (as root, or as a member of\nhyperstor-admin: sudo usermod -aG hyperstor-admin $USER, then log in again)"
        );
        if !s.all_in_one {
            println!(
                "Add a storage host: hyperstorctl token create --role node, then on that host\n  sudo hyperstor-install install --join {} --token-file FILE --role storage --address IP",
                p.api
            );
        }
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
/// intermediate changes. Returns the services to restart for new
/// certificates.
fn pki(
    root: &Path,
    real: bool,
    cluster: &str,
    member: &str,
    server_ips: &[std::net::IpAddr],
    changed: &mut Vec<String>,
) -> Vec<String> {
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
    // A member that joined has the root's certificate but not its key,
    // which stays on the first host (D70).
    let rootca = match pair("root") {
        Some(p) => Some(p),
        None if read("root.crt").is_some() => None,
        None => {
            let p = new_root(&cid).unwrap_or_else(|e| fail(&format!("root CA: {e}")));
            write("root", &p, 0o600, changed);
            Some(p)
        }
    };
    let root_cert = read("root.crt").unwrap_or_default();
    let mut renewed_ca = false;
    let ca = match (pair("intermediate"), &rootca) {
        (Some(p), _) if !due_for_renewal(&p.cert, INTERMEDIATE_DAYS) => p,
        (Some(p), None) => {
            eprintln!(
                "hyperstor-install: the intermediate CA is due for renewal; renew it on the host with the root key"
            );
            p
        }
        (_, Some(r)) => {
            let p = new_intermediate(r, &cid)
                .unwrap_or_else(|e| fail(&format!("intermediate CA: {e}")));
            write("intermediate", &p, 0o600, changed);
            renewed_ca = true;
            p
        }
        (None, None) => fail("no intermediate CA and no root key to make one: join again"),
    };
    // Services read their certificates at start: restart those whose
    // certificate is new.
    let mut restart = Vec::new();
    for (name, role, id, server, group) in [
        ("api", Role::Meta, member, true, "hyperstor"),
        ("admin", Role::Admin, "local", false, "hyperstor-admin"),
        // The dashboard's identity (D50); it only reads.
        ("mgmt", Role::Mgmt, "dashboard", false, "hyperstor"),
    ] {
        // A server certificate names where it is reached: loopback, and
        // for a map service member its addresses (D72). Kept only while it
        // names the same ones, recorded beside it.
        let (ips, names) = if server {
            let mut ips: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
            ips.extend(server_ips.iter().copied());
            ips.sort();
            ips.dedup();
            (ips, vec!["localhost".to_string()])
        } else {
            (vec![], vec![])
        };
        let recorded: String = ips.iter().map(|ip| format!("{ip}\n")).collect();
        let same_ips =
            !server || read(&format!("{name}.ips")).as_deref() == Some(recorded.as_str());
        if !renewed_ca
            && same_ips
            && pair(name).is_some_and(|p| !due_for_renewal(&p.cert, LEAF_DAYS))
        {
            continue;
        }
        let who = Identity {
            cluster: cid.clone(),
            role,
            id: id.into(),
        };
        if server {
            converge(
                &under(root, &format!("{dir}/{name}.ips")),
                recorded.as_bytes(),
                0o644,
            );
        }
        let mut p = issue(&ca, &who, &ips, &names, LEAF_DAYS)
            .unwrap_or_else(|e| fail(&format!("{name} certificate: {e}")));
        if server {
            // The root too, so hosts joining through this member can pin it.
            p.cert.push_str(&root_cert);
        }
        write(name, &p, 0o640, changed);
        if real {
            let key = format!("{dir}/{name}.key");
            if !run("chgrp", &[group, &key]) {
                fail(&format!("chgrp {group} {key} failed"));
            }
        }
        match name {
            "api" => restart.push("hyperstor-meta.service".to_string()),
            "mgmt" => restart.push("hyperstor-mgmt.service".to_string()),
            _ => {}
        }
    }
    restart
}

/// Passphrases shorter than this are refused: the backup is only as good
/// as its passphrase.
const MIN_PASSPHRASE: usize = 12;

/// `root-backup`: see the top of this file.
fn root_backup(root: &Path, out: &str, passphrase: &str, remove_key: bool) {
    if passphrase.chars().count() < MIN_PASSPHRASE {
        fail(&format!(
            "the passphrase needs at least {MIN_PASSPHRASE} characters"
        ));
    }
    let pki = under(root, "/etc/hyperstor/pki");
    let read = |f: &str| {
        fs::read_to_string(pki.join(f)).unwrap_or_else(|e| {
            fail(&format!(
                "{}: {e} (no root CA key on this host: it's kept offline, or on the first map service host)",
                pki.join(f).display()
            ))
        })
    };
    let ca = hyperstor_pki::Pem {
        cert: read("root.crt"),
        key: read("root.key"),
    };
    let cluster = cluster_id(root).to_string();
    let text = hyperstor_pki::root_backup(&ca, &cluster, passphrase)
        // A fixed message: nothing derived from the passphrase is printed.
        .unwrap_or_else(|_| fail("encrypting the backup failed"));
    let out_path = Path::new(out);
    if out_path.exists() {
        fail(&format!("{out} exists: give another file"));
    }
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(out_path)
            .unwrap_or_else(|e| fail(&format!("{out}: {e}")));
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .unwrap_or_else(|e| fail(&format!("{out}: {e}")));
    }
    // Read back and opened before anything is recorded or removed.
    let back = fs::read_to_string(out_path).unwrap_or_else(|e| fail(&format!("{out}: {e}")));
    match hyperstor_pki::root_restore(&back, passphrase) {
        Ok((_, r)) if r.key == ca.key && r.cert == ca.cert => {}
        _ => fail(&format!(
            "{out} doesn't open as written: not recorded, key kept"
        )),
    }
    let fp = hyperstor_pki::fingerprint(&ca.cert).unwrap_or_default();
    let when = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    converge(
        &pki.join("root.backup"),
        format!(
            "backed up at {when} (unix time) to {out}, root {}\n",
            &fp[..16]
        )
        .as_bytes(),
        0o644,
    );
    println!(
        "Wrote {out}: the root CA, encrypted. Keep it, and the passphrase, apart and offline."
    );
    if remove_key {
        fs::remove_file(pki.join("root.key"))
            .unwrap_or_else(|e| fail(&format!("remove root.key: {e}")));
        println!(
            "Removed {}: restore it with hyperstor-install root-restore when the intermediate CA is due.",
            pki.join("root.key").display()
        );
    }
}

/// `root-restore`: see the top of this file.
fn root_restore(root: &Path, input: &str, passphrase: &str) {
    let text = fs::read_to_string(input).unwrap_or_else(|e| fail(&format!("{input}: {e}")));
    // Named from the clear header: nothing printed comes from what the
    // passphrase opened.
    let cluster = hyperstor_pki::backup_cluster(&text)
        .unwrap_or_else(|| fail(&format!("{input}: not a hyperstor root backup")));
    let Ok((_, ca)) = hyperstor_pki::root_restore(&text, passphrase) else {
        fail(&format!(
            "{input}: wrong passphrase, or the backup was changed"
        ))
    };
    let pki = under(root, "/etc/hyperstor/pki");
    // Only onto its own cluster.
    if let Ok(have) = fs::read_to_string(pki.join("root.crt"))
        && have != ca.cert
    {
        fail("this host trusts another root: the backup is another cluster's");
    }
    if fs::read_to_string(under(root, "/etc/hyperstor/cluster.txt")).is_ok()
        && cluster_id(root).to_string() != cluster
    {
        fail(&format!(
            "the backup is cluster {cluster}'s, not this host's"
        ));
    }
    converge(&pki.join("root.crt"), ca.cert.as_bytes(), 0o644);
    converge(&pki.join("root.key"), ca.key.as_bytes(), 0o600);
    println!(
        "Restored the root CA of cluster {cluster} to {}.",
        pki.display()
    );
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
        if under(root, "/etc/hyperstor/mgmt.env").exists() {
            all.push("hyperstor-mgmt.service".into());
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
        "/etc/systemd/system/hyperstor-mgmt.service".to_string(),
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
        assert_eq!(
            units(&p),
            ["hyperstor-meta.service", "hyperstor-mgmt.service"]
        );
        let mgmt = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/mgmt.env")
            .unwrap()
            .1;
        assert!(
            mgmt.contains("--metrics 10.0.0.1:8853,10.0.0.2:8853"),
            "{mgmt}"
        );
    }

    /// A map service of several members (D72–D74): each meta host runs
    /// its member with the whole group, storage nodes know every member,
    /// and the member's certificate names its addresses.
    #[test]
    fn a_map_service_group_installs_each_member() {
        let cluster =
            "cluster 9\nnode 0 rack 0 10.0.0.1:9100\npool 1 vol replicated size 1 min 1 pgs 8\n";
        let members = "1=10.0.0.11:8851/10.0.0.11:8849,2=10.0.0.12:8851/10.0.0.12:8849,3=10.0.0.13:8851/10.0.0.13:8849";
        let mut s = Settings::defaults();
        s.role = "meta".into();
        s.members = members.into();
        s.member_id = Some(2);
        let p = plan(Path::new("/nonexistent"), &s, Some(cluster.into()));
        assert_eq!(
            p.meta.as_deref(),
            Some("10.0.0.12:8849"),
            "listens as member 2"
        );
        assert_eq!(
            p.server_ips,
            vec!["10.0.0.12".parse::<std::net::IpAddr>().unwrap(); 2]
        );
        let f = files(&s, &p);
        let env = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/meta.env")
            .unwrap()
            .1;
        assert!(env.contains("--listen 10.0.0.12:8849"), "{env}");
        assert!(env.contains("--data /var/lib/hyperstor/meta --backup-dir /var/lib/hyperstor/meta-backups --id 2 --member 1=10.0.0.11:8851/10.0.0.11:8849 --member 2=10.0.0.12:8851/10.0.0.12:8849 --member 3=10.0.0.13:8851/10.0.0.13:8849"), "{env}");
        // A storage host knows every member.
        let mut st = Settings::defaults();
        st.role = "storage".into();
        st.node = Some(0);
        st.members = members.into();
        let p = plan(Path::new("/nonexistent"), &st, Some(cluster.into()));
        let f = files(&st, &p);
        let env = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/node-0.env")
            .unwrap()
            .1;
        assert!(
            env.contains("--meta 10.0.0.11:8849,10.0.0.12:8849,10.0.0.13:8849"),
            "{env}"
        );
        // Settings keep the group.
        let mut t = Settings::defaults();
        t.apply(&s.save());
        assert_eq!((t.members.as_str(), t.member_id), (members, Some(2)));
        assert!(parse_members("1=10.0.0.1:1/10.0.0.1:2,1=10.0.0.2:1/10.0.0.2:2").is_err());
        assert!(parse_members("1=nope").is_err());
    }

    /// The map service's certificate names its member addresses, and is
    /// issued again when they change, not only when it is due.
    #[test]
    fn the_map_service_certificate_follows_its_addresses() {
        let root = tempfile::tempdir().unwrap();
        let ips = |s: &[&str]| {
            s.iter()
                .map(|i| i.parse().unwrap())
                .collect::<Vec<std::net::IpAddr>>()
        };
        let mut changed = Vec::new();
        pki(
            root.path(),
            false,
            "9",
            "0",
            &ips(&["10.0.0.12"]),
            &mut changed,
        );
        let cert = |r: &Path| fs::read_to_string(r.join("etc/hyperstor/pki/api.crt")).unwrap();
        let first = cert(root.path());
        assert_eq!(
            fs::read_to_string(root.path().join("etc/hyperstor/pki/api.ips")).unwrap(),
            "10.0.0.12\n127.0.0.1\n"
        );
        pki(
            root.path(),
            false,
            "9",
            "0",
            &ips(&["10.0.0.12"]),
            &mut changed,
        );
        assert_eq!(cert(root.path()), first, "same addresses: kept");
        let restart = pki(
            root.path(),
            false,
            "9",
            "0",
            &ips(&["10.0.0.13"]),
            &mut changed,
        );
        assert_ne!(cert(root.path()), first, "new addresses: issued again");
        assert!(restart.contains(&"hyperstor-meta.service".to_string()));
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
        // The map service keeps its store under the data directory too.
        let meta = &f
            .iter()
            .find(|x| x.0.ends_with("hyperstor-meta.service"))
            .unwrap()
            .1;
        assert!(
            meta.contains("ReadWritePaths=/var/lib/hyperstor\n"),
            "{meta}"
        );
        let env = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/meta.env")
            .unwrap()
            .1;
        assert!(
            env.contains(
                "--data /var/lib/hyperstor/meta --backup-dir /var/lib/hyperstor/meta-backups\n"
            ),
            "{env}"
        );
    }
}
