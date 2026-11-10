//! `hyperstor-install check|install|uninstall [--all-in-one] [--nodes N]
//!                   [--size-gib G] [--data-dir DIR] [--api-port P]
//!                   [--map-port P] [--node-port P] [--admin-port P] [--root DIR] [--no-start]`
//!
//! The installer of spec/19-administration.md §23.4: one linear file,
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
//!   instead: it gets the cluster file and the intermediate CA; `--role
//!   both --node N` also runs storage node N of that file. The token
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
//! - `--vhost` adds `hyperstor-vhost` (11-frontends.md §11.1, D142) to
//!   whatever the host runs: VM disks over vhost-user-blk, for glidex.
//!   `install --role vhost --map ADDR[,ADDR...]` sets up a host that only
//!   runs VMs. `--vhost-user NAME` (repeatable) lets that user use its
//!   control socket and gives its sockets the user's primary group
//!   (D144); `--no-vhost` removes it. A restart keeps its volumes (D143).
//! - `--s3` adds `hyperstor-s3`, the S3 gateway (14-s3.md §14.2, D233),
//!   listening on every address on `--s3-port` (8848, D247), with the site's
//!   certificate in `--s3-tls DIR` (`s3.crt`, `s3.key`) or its own `s3`
//!   certificate from the cluster CA. On a map service host the
//!   certificate is signed here; elsewhere it comes from a join with an
//!   `s3` token (`hyperstorctl token create --role s3 --name N`): `install
//!   --join API --token-file F --role s3 --address IP` sets up a host that
//!   only runs a gateway, and a joined storage host takes `--s3` with a
//!   token of its own in a later run. The all-in-one cluster runs one and
//!   sets the `default` placement to its pool once the map service
//!   answers; `--no-s3` removes it.
//! - `uninstall` stops and removes the units and binaries; configuration
//!   and data stay. It detaches `hyperstor-vhost`'s volumes first.
//! - `root-backup --out FILE --passphrase-file F [--remove-key]` writes the
//!   root CA (certificate and key) to FILE, encrypted with the passphrase
//!   (D84), checks that it opens, and records the backup; with
//!   `--remove-key` it then removes the key from this host, which keeps the
//!   root offline (D70). `root-restore --in FILE --passphrase-file F` puts
//!   it back, on this host or a new one. The passphrase may also come from
//!   `$HYPERSTOR_BACKUP_PASSPHRASE`; never from the command line.
//!
//! Binaries are taken from the installer's own directory (`target/release`
//! when run from a checkout). Where the map service runs, the web UI's
//! built files (D100) are installed to `/usr/local/share/hyperstor/ui`
//! from `--ui DIR` (`crates/hyperstor-mgmt/ui/dist` after `bun run
//! build`), else from `hyperstor-ui` beside the installer, and
//! `hyperstor-mgmt` serves them; without either, an installed UI stays as
//! it is, and with none the built-in dashboard is served. `--root DIR` writes every file under DIR
//! instead of `/` (for packaging and tests) and never touches users or
//! systemd; the files still name their final paths.
//!
//! Exit status: 0 done, 1 a check failed or a step failed, 2 usage.

mod tailscale;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "usage: hyperstor-install check|install|uninstall|root-backup|root-restore|format-device [--all-in-one] [--cluster FILE] [--join API] [--token-file F] [--token T] [--address IP] [--reactors N] [--rack R] [--role R] [--node N] [--map ADDR] [--members LIST] [--member-id N] [--raft-port P] [--force] [--nodes N] [--size-gib G] [--data-dir DIR] [--api-port P] [--map-port P] [--node-port P] [--admin-port P] [--mgmt-port P] [--root DIR] [--no-start] [--out FILE] [--in FILE] [--passphrase-file F] [--remove-key] [--ui DIR] [--mgmt-listen all|loopback] [--endpoints auto|off] [--client-networks auto|none|NAME,...] [--web-tls DIR] [--mgmt-hosts NAME,...] [--mgmt-proxy-ca FILE] [--mgmt-trusted-proxies CIDR,...] [--mgmt-proxy-only on|off] [--tailscale-cert] [--no-tailscale-cert] [--vhost] [--no-vhost] [--vhost-user NAME]... [--s3] [--no-s3] [--s3-port P] [--s3-tls DIR] [--s3-realm NAME] [--s3-domain NAME] [--device PATH]... [--device-address IP:PORT] [--remove-device ID]... [--path P] [--id D] [--force-format]";

const BINARIES: [&str; 8] = [
    "hyperstor-meta",
    "hyperstord",
    "hyperstorctl",
    "hyperstor-mgmt",
    "hyperstor-authd",
    "hyperstor-vhost",
    "hyperstor-smart",
    "hyperstor-s3",
];

/// Where `hyperstor-vhost`'s sockets and state file are (its unit's
/// runtime directory).
const VHOST_DIR: &str = "/run/hyperstor/vhost";

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
    /// commas, when it runs as a group (04-decisions.md D72–D74); empty: one.
    members: String,
    /// This host's member number, for a `meta` role in a group.
    member_id: Option<u32>,
    /// A promoted host's member isn't in the group yet: each run adds it
    /// once its map service answers, until it is a voter (D288).
    member_pending: bool,
    /// The Raft port a promoted host's member listens on (D288).
    raft_port: u16,
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
    /// Where people reach mgmt (D110): `all` addresses over HTTPS, or
    /// `loopback` over plain HTTP (an SSH tunnel).
    mgmt_listen: String,
    /// `auto`: propose this host's endpoints and keep them in the map
    /// (D215); `off`: leave the map's as they are (set by hand).
    endpoints: String,
    /// The data networks this host's clients (`hyperstor-vhost`,
    /// `hyperstor-s3`) reach storage on (D289): `auto`, those its storage
    /// nodes use; `none`; or names, in order.
    client_networks: String,
    /// The operator's own web certificate's directory (`web.crt`,
    /// `web.key`, kept current by their ACME client, D109); empty: the
    /// cluster CA's (D108).
    web_tls: String,
    /// Serve the node's Tailscale certificate too (D109), opt-in.
    tailscale_cert: bool,
    /// More names people reach mgmt by (a proxy's public name, D277),
    /// comma-separated; the names on `web_tls`'s certificate need none.
    mgmt_hosts: String,
    /// A CA whose client certificates mark a proxy's connections
    /// (Cloudflare's Authenticated Origin Pulls, the organization's, D277).
    mgmt_proxy_ca: String,
    /// Proxies by address, comma-separated `ADDR[/BITS]` (D277).
    mgmt_trusted_proxies: String,
    /// `on`: mgmt serves only proxies and loopback (D277).
    mgmt_proxy_only: String,
    /// Run `hyperstor-vhost` here (D142), besides the role.
    vhost: bool,
    /// The users who may use it (D144), comma-separated.
    vhost_users: String,
    /// Run `hyperstor-s3` here (D233), besides the role. The all-in-one
    /// cluster runs one unless told not to.
    s3: bool,
    /// Where the gateway listens, on every address.
    s3_port: u16,
    /// The site's certificate's directory (`s3.crt`, `s3.key`); empty:
    /// the gateway's own certificate from the cluster CA serves HTTPS.
    s3_tls: String,
    /// The realm the gateway serves (D250); empty: "default".
    s3_realm: String,
    /// The gateway's domains for virtual-hosted requests (D245),
    /// comma-separated; empty: path-style only.
    s3_domain: String,
    /// Block devices added to this host's nodes (D167, D168), as
    /// `ID@ADDR=PATH`, comma-separated; each becomes a `--shard`.
    devices: String,
    /// Devices removed from this host's nodes (D186), comma-separated
    /// ids: no `--shard` for them, whether the cluster file or `devices`
    /// names them.
    removed_devices: String,
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
            member_pending: false,
            raft_port: DEFAULT_RAFT_PORT,
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
            mgmt_listen: "all".into(),
            endpoints: "auto".into(),
            client_networks: "auto".into(),
            web_tls: String::new(),
            tailscale_cert: false,
            mgmt_hosts: String::new(),
            mgmt_proxy_ca: String::new(),
            mgmt_trusted_proxies: String::new(),
            mgmt_proxy_only: "off".into(),
            vhost: false,
            vhost_users: String::new(),
            s3: false,
            s3_port: DEFAULT_S3_PORT,
            s3_tls: String::new(),
            s3_realm: String::new(),
            s3_domain: String::new(),
            devices: String::new(),
            removed_devices: String::new(),
        }
    }

    /// The devices removed (`removed_devices`).
    fn removed(&self) -> Vec<u32> {
        self.removed_devices
            .split(',')
            .filter_map(|d| d.trim().parse().ok())
            .collect()
    }

    /// The devices added (`devices`): id, address and path.
    fn devices(&self) -> Vec<(u32, std::net::SocketAddr, String)> {
        self.devices
            .split(',')
            .filter_map(|d| {
                let (id, rest) = d.trim().split_once('@')?;
                let (addr, path) = rest.split_once('=')?;
                Some((id.parse().ok()?, addr.parse().ok()?, path.to_string()))
            })
            .collect()
    }

    /// Whether this host runs `hyperstor-vhost`.
    fn runs_vhost(&self) -> bool {
        self.vhost || self.role == "vhost"
    }

    /// Whether this host runs `hyperstor-s3`.
    fn runs_s3(&self) -> bool {
        self.s3 || self.role == "s3"
    }

    fn vhost_users(&self) -> Vec<&str> {
        self.vhost_users
            .split(',')
            .filter(|u| !u.is_empty())
            .collect()
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
                "member_pending" => self.member_pending = v == "true",
                "raft_port" => self.raft_port = v.parse().unwrap_or(self.raft_port),
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
                "mgmt_listen" => self.mgmt_listen = v.to_string(),
                "endpoints" => self.endpoints = v.to_string(),
                "client_networks" => self.client_networks = v.to_string(),
                "web_tls" => self.web_tls = v.to_string(),
                "tailscale_cert" => self.tailscale_cert = v == "true",
                "mgmt_hosts" => self.mgmt_hosts = v.to_string(),
                "mgmt_proxy_ca" => self.mgmt_proxy_ca = v.to_string(),
                "mgmt_trusted_proxies" => self.mgmt_trusted_proxies = v.to_string(),
                "mgmt_proxy_only" => self.mgmt_proxy_only = v.to_string(),
                "vhost" => self.vhost = v == "true",
                "vhost_users" => self.vhost_users = v.to_string(),
                "s3" => self.s3 = v == "true",
                "s3_port" => self.s3_port = v.parse().unwrap_or(self.s3_port),
                "s3_tls" => self.s3_tls = v.to_string(),
                "s3_realm" => self.s3_realm = v.to_string(),
                "s3_domain" => self.s3_domain = v.to_string(),
                "devices" => self.devices = v.to_string(),
                "removed_devices" => self.removed_devices = v.to_string(),
                _ => {}
            }
        }
    }

    fn save(&self) -> String {
        format!(
            "# Written by hyperstor-install; flags given on the command line override these.\n\
             all_in_one = {}\nrole = {}\nnode = {}\nmap = {}\nmembers = {}\nmember_id = {}\njoin = {}\naddress = {}\nreactors = {}\nrack = {}\nnodes = {}\nsize_gib = {}\ndata_dir = {}\napi_port = {}\nmap_port = {}\nnode_port = {}\nadmin_port = {}\nmgmt_port = {}\nmgmt_listen = {}\nweb_tls = {}\ntailscale_cert = {}\nvhost = {}\nvhost_users = {}\n",
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
            self.mgmt_port,
            self.mgmt_listen,
            self.web_tls,
            self.tailscale_cert,
            self.vhost,
            self.vhost_users
        ) + &if self.member_pending {
            "member_pending = true\n".to_string()
        } else {
            String::new()
        } + &if self.raft_port == DEFAULT_RAFT_PORT {
            String::new()
        } else {
            format!("raft_port = {}\n", self.raft_port)
        } + &if self.devices.is_empty() {
            // Written only once there is one, so other installs' files
            // stay as they were.
            String::new()
        } else {
            format!("devices = {}\n", self.devices)
        } + &if self.removed_devices.is_empty() {
            String::new()
        } else {
            format!("removed_devices = {}\n", self.removed_devices)
        } + &if self.mgmt_hosts.is_empty()
            && self.mgmt_proxy_ca.is_empty()
            && self.mgmt_trusted_proxies.is_empty()
            && self.mgmt_proxy_only == "off"
        {
            // Written only once one is set, as `devices`.
            String::new()
        } else {
            format!(
                "mgmt_hosts = {}\nmgmt_proxy_ca = {}\nmgmt_trusted_proxies = {}\nmgmt_proxy_only = {}\n",
                self.mgmt_hosts,
                self.mgmt_proxy_ca,
                self.mgmt_trusted_proxies,
                self.mgmt_proxy_only
            )
        } + &if self.endpoints == "auto" {
            String::new()
        } else {
            format!("endpoints = {}\n", self.endpoints)
        } + &if self.client_networks == "auto" {
            String::new()
        } else {
            format!("client_networks = {}\n", self.client_networks)
        } + &if self.s3
            || self.all_in_one
            || self.s3_port != DEFAULT_S3_PORT
            || !self.s3_tls.is_empty()
            || !self.s3_realm.is_empty()
            || !self.s3_domain.is_empty()
        {
            // The all-in-one's choice is kept either way: it runs one
            // unless told not to.
            format!(
                "s3 = {}\ns3_port = {}\ns3_tls = {}\ns3_realm = {}\ns3_domain = {}\n",
                self.s3, self.s3_port, self.s3_tls, self.s3_realm, self.s3_domain
            )
        } else {
            String::new()
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        usage()
    };
    if cmd == "format-device" {
        format_device_cmd(&args[1..]);
    }
    if !matches!(
        cmd.as_str(),
        "check" | "install" | "uninstall" | "root-backup" | "root-restore"
    ) {
        usage();
    }
    let mut root = PathBuf::from("/");
    let mut no_start = false;
    let mut cluster_file: Option<String> = None;
    // Never saved: where this install's UI comes from.
    let mut ui_from: Option<String> = None;
    let (mut out_file, mut in_file, mut remove_key) = (None, None, false);
    let mut passphrase: Option<String> = std::env::var("HYPERSTOR_BACKUP_PASSPHRASE").ok();
    // Never saved: a token is single-use and secret (§23.10).
    let mut token: Option<String> = std::env::var("HYPERSTOR_JOIN_TOKEN").ok();
    let mut given: BTreeMap<&str, String> = BTreeMap::new();
    // Repeatable: replaces the saved list when given at all.
    let mut vhost_users: Vec<String> = Vec::new();
    // Devices to add to this host's node (D167), never saved as given:
    // each is recorded with its id once added.
    let mut new_devices: Vec<String> = Vec::new();
    let mut device_address: Option<String> = None;
    let mut force_format = false;
    // Never saved: a role change past its refusals (D288).
    let mut force_role = false;
    // Devices removed from the map to drop from this host's node (D186).
    let mut removed: Vec<u32> = Vec::new();
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
            "--ui" => {
                ui_from = Some(val());
                continue;
            }
            "--tailscale-cert" => {
                given.insert("tailscale_cert", "true".into());
                continue;
            }
            "--no-tailscale-cert" => {
                given.insert("tailscale_cert", "false".into());
                continue;
            }
            "--vhost" => {
                given.insert("vhost", "true".into());
                continue;
            }
            "--no-vhost" => {
                given.insert("vhost", "false".into());
                continue;
            }
            "--s3" => {
                given.insert("s3", "true".into());
                continue;
            }
            "--no-s3" => {
                given.insert("s3", "false".into());
                continue;
            }
            "--vhost-user" => {
                let u = val();
                if !user_name_ok(&u) {
                    eprintln!("hyperstor-install: bad value for --vhost-user: {u}");
                    usage();
                }
                vhost_users.push(u);
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
            "--device" => {
                let d = val();
                if !d.starts_with('/') || d.contains([',', '=']) {
                    eprintln!("hyperstor-install: bad value for --device: {d}");
                    usage();
                }
                new_devices.push(d);
                continue;
            }
            "--device-address" => {
                device_address = Some(val());
                continue;
            }
            "--remove-device" => {
                let d = val();
                let Ok(id) = d.parse() else {
                    eprintln!("hyperstor-install: bad value for --remove-device: {d}");
                    usage();
                };
                removed.push(id);
                continue;
            }
            "--force-format" => {
                force_format = true;
                continue;
            }
            "--force" => {
                force_role = true;
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
            "--raft-port" => "raft_port",
            "--nodes" => "nodes",
            "--size-gib" => "size_gib",
            "--data-dir" => "data_dir",
            "--api-port" => "api_port",
            "--map-port" => "map_port",
            "--node-port" => "node_port",
            "--admin-port" => "admin_port",
            "--mgmt-port" => "mgmt_port",
            "--mgmt-listen" => "mgmt_listen",
            "--endpoints" => "endpoints",
            "--client-networks" => "client_networks",
            "--web-tls" => "web_tls",
            "--mgmt-hosts" => "mgmt_hosts",
            "--mgmt-proxy-ca" => "mgmt_proxy_ca",
            "--mgmt-trusted-proxies" => "mgmt_trusted_proxies",
            "--mgmt-proxy-only" => "mgmt_proxy_only",
            "--s3-port" => "s3_port",
            "--s3-tls" => "s3_tls",
            "--s3-realm" => "s3_realm",
            "--s3-domain" => "s3_domain",
            _ => usage(),
        };
        let v = val();
        let ok = match key {
            "nodes" => v.parse::<u32>().is_ok_and(|n| (1..=16).contains(&n)),
            "size_gib" => v.parse::<u64>().is_ok_and(|n| n >= 1),
            "api_port" | "map_port" | "node_port" | "admin_port" | "mgmt_port" | "s3_port" => {
                v.parse::<u16>().is_ok_and(|n| n > 0)
            }
            "role" => matches!(v.as_str(), "meta" | "storage" | "both" | "vhost" | "s3"),
            "mgmt_listen" => matches!(v.as_str(), "all" | "loopback"),
            "mgmt_proxy_only" => matches!(v.as_str(), "on" | "off"),
            // Names, comma-separated (D277); empty: none beyond the host's.
            "mgmt_hosts" => v.is_empty() || v.split(',').all(valid_domain),
            // Empty: no proxies by address.
            "mgmt_trusted_proxies" => v.is_empty() || v.split(',').all(valid_cidr),
            "endpoints" => matches!(v.as_str(), "auto" | "off"),
            // Network names as the map takes them (D284), comma-separated.
            "client_networks" => {
                matches!(v.as_str(), "auto" | "none")
                    || v.split(',').all(|n| {
                        (1..=32).contains(&n.len())
                            && n.bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    })
            }
            // Empty: back to the cluster CA's certificate.
            "web_tls" | "s3_tls" | "mgmt_proxy_ca" => v.is_empty() || v.starts_with('/'),
            // A realm's name (D248); empty: back to "default".
            "s3_realm" => {
                v.len() <= 63
                    && v.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            }
            // DNS names, comma-separated (D245); empty: path-style only.
            "s3_domain" => v.is_empty() || v.split(',').all(valid_domain),
            "node" => v.parse::<u32>().is_ok(),
            // One address, or a map service's members for `--role vhost`.
            "map" => v
                .split(',')
                .all(|a| a.parse::<std::net::SocketAddr>().is_ok()),
            "members" => parse_members(&v).is_ok(),
            "member_id" => v.parse::<u32>().is_ok(),
            "raft_port" => v.parse::<u16>().is_ok_and(|n| n > 0),
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
    if !vhost_users.is_empty() {
        given.insert("vhost_users", vhost_users.join(","));
    }
    let real = root == Path::new("/");
    let mut s = Settings::defaults();
    let token = token.filter(|t| !t.trim().is_empty());
    let saved = fs::read_to_string(root.join("etc/hyperstor/install.conf")).unwrap_or_default();
    s.apply(&saved);
    // A role given that isn't the saved one changes the host's (D288).
    let was_role = s.role.clone();
    // The all-in-one cluster runs a gateway (D233) unless told not to.
    let s3_said =
        given.contains_key("s3") || saved.lines().any(|l| l.trim_start().starts_with("s3 ="));
    s.apply(
        &given
            .iter()
            .map(|(k, v)| format!("{k} = {v}\n"))
            .collect::<String>(),
    );
    if s.all_in_one && !s3_said {
        s.s3 = true;
    }

    // --device, its options and --remove-device belong to install only.
    if cmd != "install"
        && (!new_devices.is_empty()
            || device_address.is_some()
            || force_format
            || !removed.is_empty())
        || new_devices.is_empty() && (device_address.is_some() || force_format)
    {
        usage();
    }
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
            // D288: a storage host made a member too, or a member host
            // made storage only.
            if was_role == "storage" && s.role == "both" {
                promote_host(&root, &mut s, token.is_some(), force_role);
            } else if was_role == "both" && s.role == "storage" {
                // SAFETY: geteuid has no preconditions.
                if real && unsafe { libc::geteuid() } != 0 {
                    fail("changing a host's role needs root: run it with sudo");
                }
                demote_host(&root, real, &mut s, force_role);
            } else if force_role {
                usage();
            }
            if !s.join.is_empty() {
                // SAFETY: geteuid has no preconditions.
                if real && unsafe { libc::geteuid() } != 0 {
                    fail("installing to / needs root: run it with sudo (or use --root DIR)");
                }
                let storage_joined = root.join("etc/hyperstor/pki/node.crt").exists();
                if let Some(f) = join(&root, real, &mut s, token.as_deref()) {
                    cluster_file = Some(f);
                }
                // A storage host's gateway joins with an s3 token of its
                // own; one spent on the storage join can't serve.
                if s.role == "storage"
                    && s.runs_s3()
                    && !root.join("etc/hyperstor/pki/s3.crt").exists()
                {
                    if storage_joined && token.is_some() {
                        join_gateway(&root, real, &mut s, token.as_deref());
                    } else {
                        eprintln!(
                            "hyperstor-install: the gateway needs an s3 token of its own: run again with --s3 --token-file F (hyperstorctl token create --role s3 --name N)"
                        );
                        s.s3 = false;
                    }
                }
            }
            if !new_devices.is_empty() {
                // SAFETY: geteuid has no preconditions.
                if real && unsafe { libc::geteuid() } != 0 {
                    fail("adding a device needs root: run it with sudo");
                }
                add_devices(
                    &root,
                    real,
                    &mut s,
                    cluster_file.clone(),
                    &new_devices,
                    device_address,
                    force_format,
                );
            }
            if !removed.is_empty() {
                // SAFETY: geteuid has no preconditions.
                if real && unsafe { libc::geteuid() } != 0 {
                    fail("removing a device needs root: run it with sudo");
                }
                remove_devices(&root, &mut s, cluster_file.clone(), &removed);
            }
            install(&root, real, no_start, &s, cluster_file, ui_from)
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
        // Storage nodes answer pings over UDP at their TCP port's number
        // (D185).
        .chain(
            (0..s.nodes)
                .map(|n| s.node_port + n as u16)
                .filter(|p| std::net::UdpSocket::bind(("127.0.0.1", *p)).is_err())
                .map(|p| format!("{p}/udp")),
        )
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

    // hyperstor-vhost's users must exist (D144).
    if s.runs_vhost() {
        let users = s.vhost_users();
        let missing: Vec<&str> = users
            .iter()
            .copied()
            .filter(|u| user_ids(u).is_none())
            .collect();
        if !missing.is_empty() {
            note(
                Level::Fail,
                "vhost",
                format!("--vhost-user {}: no such user", missing.join(", ")),
            );
        } else if users.is_empty() {
            note(
                Level::Info,
                "vhost",
                "no --vhost-user: only root and hyperstor may use hyperstor-vhost".into(),
            );
        } else {
            note(Level::Ok, "vhost", format!("for {}", users.join(", ")));
        }
    }

    // Endpoints in one subnet on several ports need routing by source
    // (26-networks.md §29.5, D207, G47). Only when this host's node has
    // endpoints in the installed cluster file.
    let endpoints: Vec<std::net::IpAddr> = s
        .node
        .and_then(|n| {
            let text = fs::read_to_string(root.join("etc/hyperstor/cluster.txt")).ok()?;
            let map = hyperstor_meta::parse_cluster(&text).ok()?;
            Some(
                map.endpoints
                    .iter()
                    .filter(|(d, _)| **d / 1000 == n)
                    .flat_map(|(_, eps)| eps.iter().map(|(_, a)| a.ip()))
                    .collect(),
            )
        })
        .unwrap_or_default();
    if !endpoints.is_empty() {
        let rules: String = ["-4", "-6"]
            .iter()
            .filter_map(|f| Command::new("ip").args([f, "rule", "show"]).output().ok())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .collect();
        let host = glidex_topology::Host::read();
        let missing = hyperstor_fabric::routing::unrouted(&host.ports, &endpoints, &rules);
        if missing.is_empty() {
            note(
                Level::Ok,
                "routing",
                format!(
                    "{} endpoint address(es) leave by their own ports",
                    endpoints.len()
                ),
            );
        } else {
            // D216: strict reverse-path filtering drops what arrives on the
            // "wrong" port, so connections fail: that fails the check.
            // Loose or none only costs the spread over ports: a warning.
            let strict: Vec<String> = missing
                .iter()
                .filter(|a| {
                    hyperstor_fabric::routing::port_of(&host.ports, **a).is_some_and(|p| {
                        hyperstor_fabric::routing::rp_filter_strict(Path::new("/proc"), &p.name)
                    })
                })
                .map(|a| a.to_string())
                .collect();
            let list: Vec<String> = missing.iter().map(|a| a.to_string()).collect();
            let how = "add `ip rule add from ADDR lookup TABLE` with a route by that port";
            if strict.is_empty() {
                note(
                    Level::Warn,
                    "routing",
                    format!(
                        "{} share a subnet with other ports and have no rule of their own: traffic from them leaves by one port ({how})",
                        list.join(", ")
                    ),
                );
            } else {
                note(
                    Level::Fail,
                    "routing",
                    format!(
                        "{} share a subnet with other ports, have no rule of their own, and their ports filter reverse paths strictly (rp_filter 1): connections to them will fail ({how}, or set rp_filter to 2)",
                        strict.join(", ")
                    ),
                );
            }
        }
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
# service alone (spec/19-administration.md §23.8).
LoadCredential=intermediate.key:/etc/hyperstor/pki/intermediate.key
CLUSTER_KEY# The local socket, /run/hyperstor-meta/api.sock (break-glass).
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

// The PAM helper for people's logins (D99): glidex-authd's server, root,
// socket-activated, answering only the hyperstor user.
const AUTHD_SOCKET: &str = "\
# Written by hyperstor-install.
[Unit]
Description=Hyperstor PAM helper socket
Documentation=https://github.com/CMKL-APEX/hyperstor

[Socket]
ListenStream=/run/hyperstor-authd/auth.sock
SocketUser=root
SocketGroup=hyperstor
SocketMode=0660
# Traversable by the hyperstor user; the socket's mode is the access
# control, and the helper also checks the peer's uid.
DirectoryMode=0755
RemoveOnStop=yes

[Install]
WantedBy=sockets.target
";

const AUTHD_SERVICE: &str = "\
# Written by hyperstor-install.
[Unit]
Description=Hyperstor PAM helper (checks people's passwords for hyperstor-mgmt)
Documentation=https://github.com/CMKL-APEX/hyperstor
Requires=hyperstor-authd.socket
After=hyperstor-authd.socket

[Service]
# Socket-activated: the listener arrives as fd 3. No RuntimeDirectory=: it
# would delete the socket's directory on stop.
Type=simple
ExecStart=/usr/local/bin/hyperstor-authd --config /etc/hyperstor/authd.json
Restart=on-failure
RestartSec=2
NoNewPrivileges=yes
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=-/run/faillock
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
UMask=0077

[Install]
Also=hyperstor-authd.socket
";

// The NVMe health reader for storage nodes (D221): root, one process per
// connection, answering only the hyperstor group, for the devices the
// nodes' options name.
const SMART_SOCKET: &str = "\
# Written by hyperstor-install.
[Unit]
Description=Hyperstor NVMe health reader socket
Documentation=https://github.com/CMKL-APEX/hyperstor

[Socket]
ListenStream=/run/hyperstor-smart/smart.sock
SocketUser=root
SocketGroup=hyperstor
SocketMode=0660
DirectoryMode=0755
Accept=yes
MaxConnections=16
RemoveOnStop=yes

[Install]
WantedBy=sockets.target
";

const SMART_SERVICE: &str = "\
# Written by hyperstor-install. One instance per request.
[Unit]
Description=Hyperstor NVMe health reader (reads drives' SMART logs for hyperstord)
Documentation=https://github.com/CMKL-APEX/hyperstor

[Service]
ExecStart=/usr/local/bin/hyperstor-smart --serve
StandardInput=socket
StandardOutput=socket
StandardError=journal
RuntimeMaxSec=30
# The log needs CAP_SYS_ADMIN and nothing else. Without CAP_DAC_OVERRIDE,
# root opens the shard devices (hyperstor's, mode 0660) through the group.
CapabilityBoundingSet=CAP_SYS_ADMIN
SupplementaryGroups=hyperstor
NoNewPrivileges=yes
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
";

// The PAM service hyperstor-mgmt names (Debian and Ubuntu's includes).
const PAM_SERVICE: &str = "\
# /etc/pam.d/hyperstor: people's logins to hyperstor-mgmt (D99).
# Written by hyperstor-install.
@include common-auth
@include common-account
";

const AUTHD_CONFIG: &str =
    "{\"service_user\": \"hyperstor\", \"allowed_groups\": [\"hyperstor-users\"]}\n";

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

const VHOST_UNIT: &str = "\
# Written by hyperstor-install. VM disks over vhost-user-blk (11-frontends.md §11.1).
[Unit]
Description=Hyperstor vhost-user-blk service (VM disks)
Documentation=https://github.com/CMKL-APEX/hyperstor
After=network-online.target hyperstor-meta.service
Wants=network-online.target

[Service]
User=hyperstor
GROUPS_LINE
EnvironmentFile=/etc/hyperstor/vhost.env
ExecStart=/usr/local/bin/hyperstor-vhost serve $HS_ARGS
# Kept across restarts (D143): its state file names the volumes the next
# start takes back, and the hypervisors reconnect to the same sockets.
RuntimeDirectory=hyperstor/vhost
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=yes
Restart=on-failure
RestartSec=1
LimitMEMLOCK=infinity
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
";

const S3_UNIT: &str = "\
# Written by hyperstor-install. The S3 gateway (14-s3.md §14.2, D233).
[Unit]
Description=Hyperstor S3 gateway
Documentation=https://github.com/CMKL-APEX/hyperstor
After=network-online.target hyperstor-meta.service
Wants=network-online.target

[Service]
User=hyperstor
EnvironmentFile=/etc/hyperstor/s3.env
ExecStart=/usr/local/bin/hyperstor-s3 $HS_ARGS
# A port under 1024 (--s3-port 443) without root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
Restart=on-failure
RestartSec=1
LimitMEMLOCK=infinity
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
";

/// `hyperstor-vhost`'s users (D144), as ids: who may use its control
/// socket, and the groups its sockets go to (the first) and it runs in.
#[derive(Debug, Clone, PartialEq)]
struct VhostPlan {
    uids: Vec<u32>,
    gids: Vec<u32>,
}

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
    /// The web UI is installed (`UI_DIR`): mgmt serves it (D100).
    ui: bool,
    /// mgmt serves the node's Tailscale certificate too (D109): asked
    /// for, and `tailscaled` lets the `hyperstor` user have it.
    tailscale: bool,
    /// `hyperstor-vhost`, if this host runs it (D142).
    vhost: Option<VhostPlan>,
    /// `/etc/hyperstor/pki/cluster.key` is there, for the map service
    /// (D162); found after the certificates are made.
    cluster_key: bool,
    /// `hyperstor-s3`, if this host runs it (D233).
    s3: Option<S3Plan>,
}

/// A gateway's options (D233): the cluster API it reads keys from, the
/// directory of the certificate it serves, where it listens, and the
/// realm it serves (D250; empty: "default").
#[derive(Debug, Clone, PartialEq)]
struct S3Plan {
    api: String,
    tls: String,
    listen: String,
    realm: String,
    /// Its domains for virtual-hosted requests (D245), comma-separated.
    domain: String,
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
    /// Each one's device id (D169), named in `--shard` when it isn't the
    /// position's.
    ids: Vec<u32>,
    /// A block device added (D167) for each shard that has one; the
    /// others are files in the data directory.
    paths: Vec<Option<String>>,
    admin: String,
    /// Its devices' endpoints in the cluster file (26-networks.md §29.5,
    /// D207): `hyperstord --endpoint`.
    endpoints: Vec<(u32, std::net::SocketAddr)>,
    /// The data networks it has endpoints on, by name: `--networks`.
    networks: Vec<String>,
}

/// The plan for `s`: the all-in-one trial cluster, or this host's part of
/// the cluster described by a cluster file (given now, or installed by an
/// earlier run).
fn plan(root: &Path, s: &Settings, cluster_file: Option<String>) -> Plan {
    let mut p = role_plan(root, s, cluster_file);
    // Devices added to this host's nodes, after the nodes' own (D167).
    for (id, addr, path) in s.devices() {
        if let Some(n) = p.nodes.iter_mut().find(|n| n.n == id / 1000)
            && !n.ids.contains(&id)
        {
            n.shards.push(addr);
            n.ids.push(id);
            n.paths.push(Some(path));
        }
    }
    // Devices removed (D186): no --shard, whoever named them.
    let removed = s.removed();
    for n in &mut p.nodes {
        let kept: Vec<_> = n
            .ids
            .iter()
            .zip(&n.shards)
            .zip(&n.paths)
            .filter(|((id, _), _)| !removed.contains(id))
            .map(|((id, a), path)| (*id, *a, path.clone()))
            .collect();
        n.ids = kept.iter().map(|k| k.0).collect();
        n.shards = kept.iter().map(|k| k.1).collect();
        n.paths = kept.into_iter().map(|k| k.2).collect();
    }
    p.vhost = s.runs_vhost().then(|| vhost_plan(s));
    if p.vhost.is_some() && p.map.is_empty() {
        fail("--vhost needs the map service's addresses: --map ADDR[,ADDR...]");
    }
    p.s3 = s.runs_s3().then(|| s3_plan(s, &p));
    p
}

/// The gateway's options (D233): the cluster API is this host's own where
/// it runs the map service, else the one it joined through.
fn s3_plan(s: &Settings, p: &Plan) -> S3Plan {
    let api = if p.meta.is_some() {
        p.api.clone()
    } else {
        s.join.clone()
    };
    if api.is_empty() || p.map.is_empty() {
        fail(
            "--s3 needs the cluster API: run it on a map service host, or on one joined with --join API",
        );
    }
    S3Plan {
        api,
        tls: if s.s3_tls.is_empty() {
            "/etc/hyperstor/pki".into()
        } else {
            s.s3_tls.clone()
        },
        listen: format!("0.0.0.0:{}", s.s3_port),
        realm: s.s3_realm.clone(),
        domain: s.s3_domain.clone(),
    }
}

/// A host that only runs an S3 gateway (D233), joined with an `s3` token:
/// the map service's addresses came with its join.
fn s3_only_plan(s: &Settings) -> Plan {
    if s.join.is_empty() {
        fail(
            "--role s3 needs --join API and an s3 token (hyperstorctl token create --role s3 --name N)",
        );
    }
    if s.map.is_empty() {
        fail("this host's join left no map service addresses: join again with a new s3 token");
    }
    let mut p = vhost_only_plan(s);
    p.vhost = None;
    p
}

/// The users `--vhost-user` names, as ids; an unknown one fails the
/// install.
fn vhost_plan(s: &Settings) -> VhostPlan {
    let mut v = VhostPlan {
        uids: Vec::new(),
        gids: Vec::new(),
    };
    for u in s.vhost_users() {
        let (uid, gid) =
            user_ids(u).unwrap_or_else(|| fail(&format!("--vhost-user {u}: no such user")));
        v.uids.push(uid);
        if !v.gids.contains(&gid) {
            v.gids.push(gid);
        }
    }
    v
}

/// A user's uid and primary gid.
fn user_ids(name: &str) -> Option<(u32, u32)> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam with a NUL-terminated name; the entry is read
    // before any other call can replace it.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    // SAFETY: a non-null result points to a valid passwd entry.
    (!pw.is_null()).then(|| unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

/// A plain user name: what `--vhost-user` takes.
fn user_name_ok(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 32
        && u.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && u.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A host that only runs VMs (D142): `hyperstor-vhost` for the map
/// service at `--map`, and nothing else of the cluster's.
fn vhost_only_plan(s: &Settings) -> Plan {
    if s.map.is_empty() {
        fail("--role vhost needs --map ADDR[,ADDR...]: the map service's addresses");
    }
    Plan {
        ui: false,
        tailscale: false,
        api: String::new(),
        cluster: String::new(),
        cluster_id: String::new(),
        meta: None,
        map: s.map.clone(),
        members: Vec::new(),
        member_id: None,
        server_ips: Vec::new(),
        nodes: Vec::new(),
        metrics: Vec::new(),
        vhost: None,
        cluster_key: false,
        s3: None,
    }
}

fn role_plan(root: &Path, s: &Settings, cluster_file: Option<String>) -> Plan {
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
        // Each pool at the smallest PG count (at least 64) the autoscaler
        // keeps for these devices, so a new trial doesn't split (D184).
        let pgs = |copies: u8| {
            let mut n = 64;
            while hyperstor_meta::autoscale_wants(s.nodes as usize, copies, n) {
                n *= 2;
            }
            n
        };
        // One host: `domain host` puts copies on different storage nodes,
        // which here are hyperstord instances.
        cluster += &format!(
            "pool 1 vol replicated size {size} min {min} pgs {} chunk 4194304 domain host\n",
            pgs(size as u8)
        );
        // A cache pool (D45): one copy, lost rather than recovered.
        cluster += &format!(
            "pool 2 cache cache copies 1 pgs {} chunk 4194304 domain host\n",
            pgs(1)
        );
        let map = format!("127.0.0.1:{}", s.map_port);
        let metrics = (0..s.nodes)
            .map(|n| format!("127.0.0.1:{}", s.admin_port + n as u16))
            .collect();
        return Plan {
            vhost: None,
            cluster_key: false,
            s3: None,
            ui: false,
            tailscale: false,
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
                    ids: vec![n * 1000],
                    paths: vec![None],
                    admin: format!("127.0.0.1:{}", s.admin_port + n as u16),
                    endpoints: Vec::new(),
                    networks: Vec::new(),
                })
                .collect(),
        };
    }
    if s.role.is_empty() {
        fail(
            "give --all-in-one, --cluster FILE with --role meta|storage|both, --join API with --role, or --role vhost --map ADDR",
        );
    }
    if s.role == "vhost" {
        return vhost_only_plan(s);
    }
    if s.role == "s3" {
        return s3_only_plan(s);
    }
    if !s.join.is_empty() && s.role == "storage" {
        return joined_storage_plan(s);
    }
    // Interim, until joining with tokens (D57, 01-implementation-notes.md
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
    // A storage host that joined with a token and was promoted (D288):
    // its node isn't in the cluster file the group started from.
    let promoted = s.role == "both"
        && !s.join.is_empty()
        && under(root, "/etc/hyperstor/pki/node.crt").exists()
        && s.node
            .is_some_and(|n| !map.devices.values().any(|d| d.node == n));
    if promoted {
        nodes.extend(joined_storage_plan(s).nodes);
    }
    if (s.role == "storage" || s.role == "both") && !promoted {
        let n = s
            .node
            .unwrap_or_else(|| fail("--node N is needed for a storage role"));
        let mut devs: Vec<_> = map.devices.values().filter(|d| d.node == n).collect();
        if devs.is_empty() {
            fail(&format!("the cluster file has no node {n}"));
        }
        devs.sort_by_key(|d| d.shard);
        let ip = devs[0].addr.ip();
        let endpoints: Vec<(u32, std::net::SocketAddr, &str)> = devs
            .iter()
            .flat_map(|d| {
                map.endpoints
                    .get(&d.id)
                    .into_iter()
                    .flatten()
                    .map(|(net, a)| (d.id, *a, net.as_str()))
            })
            .collect();
        nodes.push(NodePlan {
            n,
            shards: devs.iter().map(|d| d.addr).collect(),
            ids: devs.iter().map(|d| d.id).collect(),
            paths: vec![None; devs.len()],
            admin: std::net::SocketAddr::new(ip, s.admin_port).to_string(),
            endpoints: endpoints.iter().map(|(d, a, _)| (*d, *a)).collect(),
            // The data networks it has endpoints on: it can reach others'
            // there too.
            networks: map
                .networks
                .iter()
                .filter(|(name, n)| {
                    n.kind != hyperstor_meta::NetKind::Control
                        && endpoints.iter().any(|(_, _, net)| net == name)
                })
                .map(|(name, _)| name.clone())
                .collect(),
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
    if s.map.contains(',') {
        fail("--map takes several addresses only with --role vhost");
    }
    Plan {
        vhost: None,
        cluster_key: false,
        s3: None,
        ui: false,
        tailscale: false,
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
        vhost: None,
        cluster_key: false,
        s3: None,
        ui: false,
        tailscale: false,
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
            ids: (0..s.reactors).map(|i| n * 1000 + i).collect(),
            paths: vec![None; s.reactors as usize],
            admin: std::net::SocketAddr::new(ip, s.admin_port).to_string(),
            endpoints: Vec::new(),
            networks: Vec::new(),
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
    if s.role == "s3" {
        join_gateway(root, real, s, token);
        return None;
    }
    let dir = under(root, "/etc/hyperstor/pki");
    let (cert_name, key_group) = match s.role.as_str() {
        "storage" => ("node", "hyperstor"),
        // A member that also runs a storage node of the cluster file joins
        // as the member; its node, like the first member's, has no
        // certificate of its own (the storage network uses none yet).
        "meta" | "both" => ("api", "hyperstor"),
        _ => fail("--join takes --role storage, meta, both or s3"),
    };
    if s.role == "both" && s.node.is_none() {
        fail("--node N is needed for a storage role: which node of the cluster file");
    }
    if dir.join(format!("{cert_name}.crt")).exists() && (s.role != "storage" || s.node.is_some()) {
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
    let chain = if s.role != "storage" {
        // A member's server certificate carries the root, so the hosts
        // that join through it can pin it.
        format!("{}{}", r.certificate, r.root)
    } else {
        r.certificate.clone()
    };
    // The key's group is the service user's, which a fresh host doesn't
    // have yet.
    if real {
        service_user(s);
    }
    if s.role != "storage" {
        // Before anything can fail: a later run, without the spent token,
        // finds everything a member needs.
        write("intermediate.crt", &r.intermediate, 0o644);
        write("intermediate.key", &r.intermediate_key, 0o600);
        // The cluster key (D162), from a member that has one.
        if !r.cluster_key.is_empty() {
            write("cluster.key", &r.cluster_key, 0o600);
        }
        converge(
            &under(root, "/etc/hyperstor/cluster.txt"),
            r.cluster_file.as_bytes(),
            0o644,
        );
    }
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
        Some(r.cluster_file.clone())
    }
}

// --- A host's roles (19-administration.md §23.4.3, D288) -------------

/// A client of the cluster API with this host's certificate `who`
/// (`node`, `admin`, `s3`), if it has one.
fn host_tls(root: &Path, who: &str) -> Option<hyperstor_pki::ClientConfig> {
    let dir = under(root, "/etc/hyperstor/pki");
    let read = |f: &str| fs::read_to_string(dir.join(f)).ok();
    hyperstor_pki::client_config(
        &read(&format!("{who}.crt"))?,
        &read(&format!("{who}.key"))?,
        &read("root.crt")?,
    )
    .ok()
}

/// The map service's members, as the first of `apis` that answers says,
/// with that API's address.
fn members_at(
    root: &Path,
    who: &str,
    apis: &[String],
) -> Result<(String, hyperstor_api::ListMembersResponse), String> {
    let mut last = format!("no {who} certificate here");
    for api in apis.iter().filter(|a| !a.is_empty()) {
        let Some(tls) = host_tls(root, who) else {
            break;
        };
        match api_call(api, tls, |c| async move {
            hyperstor_api::MemberServiceClient::new(c.0, c.1)
                .list_members(hyperstor_api::ListMembersRequest::default())
                .await
                .map(|r| r.into_owned())
        }) {
            Ok(r) => return Ok((api.clone(), r)),
            Err(e) => last = format!("{api}: {e}"),
        }
    }
    Err(last)
}

/// The APIs to ask: `first`, then each member's at its map address's IP
/// on the API port.
fn api_candidates(first: &str, map: &str, api_port: u16) -> Vec<String> {
    let mut out = vec![first.to_string()];
    for a in map
        .split(',')
        .filter_map(|a| a.trim().parse::<std::net::SocketAddr>().ok())
    {
        let api = std::net::SocketAddr::new(a.ip(), api_port).to_string();
        if !out.contains(&api) {
            out.push(api);
        }
    }
    out
}

/// One call through the leader: `api` first, then the leader a
/// follower's refusal names. `call` gets the address to use.
fn on_leader<T>(
    members: &hyperstor_api::ListMembersResponse,
    api: &str,
    mut call: impl FnMut(&str) -> Result<T, String>,
) -> Result<T, String> {
    let leader = members
        .members
        .iter()
        .find(|m| m.leader)
        .map(|m| m.api_address.clone())
        .filter(|a| !a.is_empty());
    let mut tried = Vec::new();
    for a in leader.into_iter().chain([api.to_string()]) {
        if tried.contains(&a) {
            continue;
        }
        match call(&a) {
            Ok(v) => return Ok(v),
            Err(e) => tried.push(format!("{a}: {e}")),
        }
    }
    Err(tried.join("; "))
}

/// Promotes a storage host that joined with a token to a map service
/// member too (`--role both`, D288): its member list is the group's and
/// itself, at its address on the Raft and map ports; the member joins
/// with the meta token like any member (I86), keeps its node, and adds
/// itself once its map service answers (`ensure_member`). Refused without
/// a token, for a member id the group has, and for an even count of
/// voters unless `force`.
fn promote_host(root: &Path, s: &mut Settings, has_token: bool, force: bool) {
    let pki = under(root, "/etc/hyperstor/pki");
    if pki.join("api.crt").exists() {
        return;
    }
    if s.join.is_empty() || s.node.is_none() || !pki.join("node.crt").exists() {
        fail(
            "--role both makes a storage host a member too: one that joined with --join --role storage (D288)",
        );
    }
    if !has_token {
        fail(
            "promoting needs a meta token: --token-file F (hyperstorctl token create --role meta)",
        );
    }
    let (_, r) = members_at(root, "node", &api_candidates(&s.join, &s.map, s.api_port))
        .unwrap_or_else(|e| fail(&format!("reading the map service's members: {e}")));
    let voters = r.members.iter().filter(|m| m.voter).count();
    if (voters + 1) % 2 == 0 && !force {
        fail(&format!(
            "{} voters survive no more failures than {voters} and commit slower: --force allows it",
            voters + 1
        ));
    }
    let id = s
        .member_id
        .unwrap_or_else(|| r.members.iter().map(|m| m.id as u32).max().unwrap_or(0) + 1);
    if r.members.iter().any(|m| m.id == u64::from(id)) {
        fail(&format!("member {id} is in the group already"));
    }
    let ip: std::net::IpAddr = s
        .address
        .parse()
        .unwrap_or_else(|_| fail("this host's address isn't an IP: --address IP"));
    let mut list: Vec<String> = r
        .members
        .iter()
        .map(|m| format!("{}={}/{}", m.id, m.raft_address, m.map_address))
        .collect();
    list.push(format!(
        "{id}={}/{}",
        std::net::SocketAddr::new(ip, s.raft_port),
        std::net::SocketAddr::new(ip, s.map_port)
    ));
    s.members = list.join(",");
    s.member_id = Some(id);
    s.member_pending = true;
    // A member's nodes find the map service from the member list.
    s.map.clear();
    println!(
        "Promoting: this host joins as map service member {id} and adds itself once its map service runs."
    );
}

/// Adds a promoted host's member to the group (D288) once its map service
/// answers, through the leader, with the administrator's certificate, and
/// waits up to `wait_secs` for it to be a voter. Whether it is.
fn ensure_member(root: &Path, s: &Settings, p: &Plan, wait_secs: u64) -> bool {
    let Some(id) = s.member_id else {
        return false;
    };
    let me = parse_members(&s.members)
        .ok()
        .and_then(|m| m.into_iter().find(|m| m.id == id))
        .unwrap_or_else(|| fail(&format!("member {id} is not in --members")));
    // The API this host joined through first: its own member, not in the
    // group yet, knows no leader.
    let mut apis = vec![s.join.clone()];
    apis.extend(api_candidates(&p.api, &p.map, s.api_port));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    let mut added = false;
    loop {
        // Added only once its own map service answers: the group may make
        // it a voter at once, and a voter that isn't running weakens it.
        let runs = members_at(root, "admin", std::slice::from_ref(&p.api)).is_ok();
        if !runs {
        } else if let Ok((api, r)) = members_at(root, "admin", &apis) {
            let mine = r.members.iter().find(|m| m.id == u64::from(id));
            if mine.is_some_and(|m| m.voter) {
                println!("Member {id} is a voter.");
                return true;
            }
            if mine.is_none() && !added {
                let req = hyperstor_api::AddMemberRequest {
                    id: u64::from(id),
                    raft_address: me.raft.to_string(),
                    map_address: me.map.to_string(),
                    ..Default::default()
                };
                let done = on_leader(&r, &api, |a| {
                    let tls = host_tls(root, "admin").ok_or("no administrator's certificate")?;
                    let req = req.clone();
                    api_call(a, tls, |c| async move {
                        hyperstor_api::MemberServiceClient::new(c.0, c.1)
                            .add_member(req)
                            .await
                            .map(|_| ())
                    })
                });
                match done {
                    Ok(()) => {
                        println!(
                            "Added member {id}: a learner until it has the log, then a voter."
                        );
                        added = true;
                    }
                    Err(e) => eprintln!("hyperstor-install: adding member {id}: {e}"),
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "hyperstor-install: member {id} isn't a voter yet: the next run adds it, or waits for it (hyperstorctl member list)"
            );
            return false;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Demotes a member host that also runs a storage node to a storage host
/// (`--role storage`, D288): refused where the root CA's key is, where an
/// S3 gateway runs on the member's certificate, below three voters unless
/// `force`, and for a node whose devices' ports aren't consecutive. Then:
/// a backup of the store; its node a certificate of its own (a join with
/// its node's addresses gives the node back, D167); out of the group;
/// its member certificates revoked; the member's services, files, keys and
/// store gone. `s` is then a storage host's, joined through the leader.
fn demote_host(root: &Path, real: bool, s: &mut Settings, force: bool) {
    let pki = under(root, "/etc/hyperstor/pki");
    if pki.join("root.key").exists() {
        fail(
            "the root CA's key is on this host: back it up and take it off first (root-backup --remove-key), and restore it on another member (root-restore)",
        );
    }
    if s.runs_s3() {
        fail("this host's S3 gateway runs on its member certificate: remove it first (--no-s3)");
    }
    let id = s
        .member_id
        .unwrap_or_else(|| fail("this host's member id isn't known (install.conf member_id)"));
    let mut was = s.clone();
    was.role = "both".into();
    let plan = plan(root, &was, None);
    let api_here = plan.api.clone();
    let node = plan
        .nodes
        .first()
        .unwrap_or_else(|| fail("this host runs no storage node: uninstall it instead"));
    let added: Vec<u32> = s.devices().iter().map(|d| d.0).collect();
    let own: Vec<std::net::SocketAddr> = node
        .ids
        .iter()
        .zip(&node.shards)
        .filter(|(i, _)| !added.contains(i))
        .map(|(_, a)| *a)
        .collect();
    let first = own[0];
    if own
        .iter()
        .enumerate()
        .any(|(i, a)| a.ip() != first.ip() || a.port() != first.port() + i as u16)
    {
        fail(
            "its node's devices aren't on consecutive ports of one address, as a joined node's are",
        );
    }
    let (api, r) = members_at(
        root,
        "admin",
        &api_candidates(&api_here, &plan.map, s.api_port),
    )
    .unwrap_or_else(|e| fail(&format!("reading the map service's members: {e}")));
    let voters = r.members.iter().filter(|m| m.voter).count();
    if !r.members.iter().any(|m| m.id == u64::from(id)) {
        println!("Member {id} has left the group already.");
    } else if voters <= 3 && !force {
        fail(&format!(
            "{} voters would be left, which survive no failure: --force allows it",
            voters - 1
        ));
    }
    // Another member's API, the leader's if it is another: the one this
    // host reaches the cluster through from now on.
    let others: Vec<&hyperstor_api::Member> = r
        .members
        .iter()
        .filter(|m| m.id != u64::from(id) && !m.api_address.is_empty())
        .collect();
    let join_api = others
        .iter()
        .find(|m| m.leader)
        .or_else(|| others.first())
        .map(|m| m.api_address.clone())
        .unwrap_or_else(|| fail("no other member to join through"));
    // 1. The store, backed up before anything changes.
    let ctl = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("hyperstorctl")))
        .unwrap_or_else(|| fail("can't find hyperstorctl beside me"));
    let backups = under(root, &format!("{}/meta-backups", s.data_dir));
    let _ = fs::create_dir_all(&backups);
    let backup = backups.join(format!(
        "demoted-member-{id}-{}.store",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    ));
    let out = Command::new(&ctl)
        .args(["--api", &api, "--tls"])
        .arg(&pki)
        .args(["map", "backup", "--out"])
        .arg(&backup)
        .output()
        .unwrap_or_else(|e| fail(&format!("{}: {e}", ctl.display())));
    if !out.status.success() {
        fail(&format!(
            "map backup: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    println!("Backed up the map service's store to {}.", backup.display());
    // 2. Its node's own certificate: a node token, and a join with the
    // node's addresses, which gives that node back.
    if !pki.join("node.crt").exists() {
        let token = on_leader(&r, &api, |a| {
            let tls = host_tls(root, "admin").ok_or("no administrator's certificate")?;
            api_call(a, tls, |c| async move {
                hyperstor_api::TokenServiceClient::new(c.0, c.1)
                    .create_token(hyperstor_api::CreateTokenRequest {
                        role: "node".into(),
                        ttl_seconds: 600,
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned().token)
            })
        })
        .unwrap_or_else(|e| fail(&format!("a node token: {e}")));
        let n = node.n;
        let mut joined = s.clone();
        joined.role = "storage".into();
        joined.join = join_api.clone();
        joined.address = first.ip().to_string();
        joined.reactors = own.len() as u32;
        joined.node_port = first.port();
        joined.node = None;
        join(root, real, &mut joined, Some(&token));
        if joined.node != Some(n) {
            fail(&format!(
                "the join gave node {:?}, not this host's node {n}: nothing else changed; revoke the new node's certificate",
                joined.node
            ));
        }
        s.map = joined.map.clone();
    }
    // 3. Out of the group, through the leader.
    if r.members.iter().any(|m| m.id == u64::from(id)) {
        on_leader(&r, &api, |a| {
            let tls = host_tls(root, "admin").ok_or("no administrator's certificate")?;
            api_call(a, tls, |c| async move {
                hyperstor_api::MemberServiceClient::new(c.0, c.1)
                    .remove_member(hyperstor_api::RemoveMemberRequest {
                        id: u64::from(id),
                        force,
                        ..Default::default()
                    })
                    .await
                    .map(|_| ())
            })
        })
        .unwrap_or_else(|e| fail(&format!("removing member {id}: {e}")));
        println!("Removed member {id} from the map service.");
    }
    // 4. Its member certificates refused from now on: the administrator's
    // last, since it makes the calls.
    for name in ["api", "mgmt", "web", "admin"] {
        let Some(serial) = fs::read_to_string(pki.join(format!("{name}.crt")))
            .ok()
            .and_then(|c| hyperstor_pki::serial_in(&c))
        else {
            continue;
        };
        let r = api_call(
            &join_api,
            host_tls(root, "admin").unwrap_or_else(|| fail("no administrator's certificate")),
            |c| async move {
                hyperstor_api::CaServiceClient::new(c.0, c.1)
                    .revoke_certificate(hyperstor_api::RevokeCertificateRequest {
                        serial,
                        ..Default::default()
                    })
                    .await
                    .map(|_| ())
            },
        );
        if let Err(e) = r {
            eprintln!("hyperstor-install: revoking this host's {name} certificate: {e}");
        }
    }
    // 5. The member's services, files, keys and store.
    if real {
        let _ = run(
            "systemctl",
            &[
                "disable",
                "--now",
                "hyperstor-meta.service",
                "hyperstor-mgmt.service",
                "hyperstor-authd.socket",
                "hyperstor-authd.service",
            ],
        );
    }
    for f in [
        "/etc/systemd/system/hyperstor-meta.service",
        "/etc/systemd/system/hyperstor-mgmt.service",
        "/etc/systemd/system/hyperstor-authd.socket",
        "/etc/systemd/system/hyperstor-authd.service",
        "/etc/hyperstor/meta.env",
        "/etc/hyperstor/mgmt.env",
        "/etc/hyperstor/authd.json",
        "/etc/hyperstor/prometheus-targets.json",
        "/etc/hyperstor/hyperstorctl.conf",
        "/etc/hyperstor/cluster.txt",
    ] {
        let _ = fs::remove_file(under(root, f));
    }
    for k in [
        "intermediate.key",
        "intermediate.crt",
        "cluster.key",
        "api.crt",
        "api.key",
        "api.ips",
        "admin.crt",
        "admin.key",
        "mgmt.crt",
        "mgmt.key",
        "web.crt",
        "web.key",
        "web.ips",
    ] {
        let _ = fs::remove_file(pki.join(k));
    }
    let store = under(root, &format!("{}/meta", s.data_dir));
    if store.is_dir() {
        fs::remove_dir_all(&store).unwrap_or_else(|e| fail(&format!("{}: {e}", store.display())));
    }
    if real {
        let _ = run("systemctl", &["daemon-reload"]);
    }
    s.role = "storage".into();
    s.join = join_api;
    s.address = first.ip().to_string();
    s.reactors = own.len() as u32;
    s.node_port = first.port();
    s.node = Some(node.n);
    s.members.clear();
    s.member_id = None;
    s.member_pending = false;
    println!(
        "Demoted: this host is storage node {} only, joined through {}.",
        node.n, s.join
    );
}

/// Member lists follow (D288): the map service's members, through the API,
/// for the services that find it by its members' map addresses (nodes,
/// `hyperstor-vhost`, `hyperstor-s3`). A joined host keeps them in `s`
/// (and moves to another member's API if its own has gone); a member
/// host's only change where its nodes and clients look. A member's own
/// list stays as installed: the group knows its members, and the list
/// only forms a new group. Nothing when no API answers.
fn follow_members(root: &Path, s: &mut Settings, p: &mut Plan) {
    let (who, first) = if s.role == "storage" && !s.join.is_empty() {
        ("node", s.join.clone())
    } else if p.meta.is_some() && s.member_id.is_some() {
        // A promoted member asks the API it joined through: its own may
        // not be in the group yet.
        (
            "admin",
            if s.join.is_empty() {
                p.api.clone()
            } else {
                s.join.clone()
            },
        )
    } else {
        return;
    };
    let Ok((api, r)) = members_at(root, who, &api_candidates(&first, &p.map, s.api_port)) else {
        return;
    };
    let maps: Vec<String> = r
        .members
        .iter()
        .filter(|m| !m.map_address.is_empty())
        .map(|m| m.map_address.clone())
        .collect();
    if maps.is_empty() {
        return;
    }
    let list = maps.join(",");
    if who == "node" {
        if s.map != list {
            println!("The map service's members are now at {list}.");
            s.map = list.clone();
        }
        if api != s.join {
            s.join = api;
        }
    }
    p.map = list;
}

/// Creates the `hyperstor` system user and its group, the services' own,
/// unless it exists; whether it was created.
fn service_user(s: &Settings) -> bool {
    let exists = Command::new("id")
        .arg("hyperstor")
        .output()
        .is_ok_and(|o| o.status.success());
    if exists {
        return false;
    }
    if !run(
        "useradd",
        &[
            "--system",
            // The group too: the API's key is readable by it.
            "--user-group",
            "--no-create-home",
            "--home-dir",
            s.data_dir.as_str(),
            "--shell",
            "/usr/sbin/nologin",
            "hyperstor",
        ],
    ) {
        fail("useradd hyperstor failed");
    }
    true
}

/// Renews a joined storage host's certificate over the API, with the
/// certificate itself (D56), once it is past two thirds of its life.
fn renew_node(root: &Path, real: bool, s: &Settings) {
    renew_leaf(root, real, s, "node");
}

/// Renews this host's certificate `name` (`node.crt`, `s3.crt`) over the
/// API it joined through, with the certificate itself (D56), once it is
/// past two thirds of its life.
fn renew_leaf(root: &Path, real: bool, s: &Settings, name: &str) -> bool {
    let dir = under(root, "/etc/hyperstor/pki");
    let read = |f: &str| fs::read_to_string(dir.join(f)).unwrap_or_default();
    let cert = read(&format!("{name}.crt"));
    if !hyperstor_pki::due_for_renewal(&cert, hyperstor_pki::LEAF_DAYS) {
        return false;
    }
    let tls = hyperstor_pki::client_config(&cert, &read(&format!("{name}.key")), &read("root.crt"))
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
            converge(&dir.join(format!("{name}.key")), key.as_bytes(), 0o640);
            converge(
                &dir.join(format!("{name}.crt")),
                r.certificate.as_bytes(),
                0o644,
            );
            let k = format!("/etc/hyperstor/pki/{name}.key");
            if real && !run("chgrp", &["hyperstor", &k]) {
                fail(&format!("chgrp hyperstor {k} failed"));
            }
            println!("Renewed /etc/hyperstor/pki/{name}.crt.");
            true
        }
        // Not fatal: the next daily run tries again, and the certificate
        // has a third of its life left.
        Err(e) => {
            eprintln!(
                "hyperstor-install: renewing {name}.crt through {}: {e}",
                s.join
            );
            false
        }
    }
}

/// An S3 gateway's identity (D233), on a host without the map service:
/// joined with an `s3` token the first time, its certificate naming
/// `--address`; renewed by later runs.
fn join_gateway(root: &Path, real: bool, s: &mut Settings, token: Option<&str>) {
    let dir = under(root, "/etc/hyperstor/pki");
    if dir.join("s3.crt").exists() {
        return;
    }
    let token = token.unwrap_or_else(|| {
        fail("the gateway needs an s3 token: --token-file F (from hyperstorctl token create --role s3 --name N)")
    });
    let (pin, _) = hyperstor_meta::api::parse_token(token)
        .unwrap_or_else(|| fail("the token isn't a hyperstor join token (hsj1-…)"));
    let ip: std::net::IpAddr = s
        .address
        .parse()
        .unwrap_or_else(|_| fail("--address IP is needed: where clients reach the gateway"));
    let (key, csr) = hyperstor_pki::new_request().unwrap_or_else(|e| fail(&format!("key: {e}")));
    let req = hyperstor_api::JoinRequest {
        token: token.trim().to_string(),
        csr,
        gateway_ips: vec![ip.to_string()],
        ..Default::default()
    };
    let tls = hyperstor_pki::pinned_client_config(pin).unwrap_or_else(|e| fail(&e));
    let r = api_call(&s.join, tls, |c| async move {
        hyperstor_api::TokenServiceClient::new(c.0, c.1)
            .join(req)
            .await
            .map(|r| r.into_owned())
    })
    .unwrap_or_else(|e| fail(&format!("join through {}: {e}", s.join)));
    if hyperstor_pki::identity_in(&r.certificate).map(|i| i.role) != Some(hyperstor_pki::Role::S3) {
        fail(
            "the token isn't an s3 token: make one with hyperstorctl token create --role s3 --name N",
        );
    }
    if real {
        service_user(s);
    }
    let write = |name: &str, content: &str, mode: u32| {
        converge(&dir.join(name), content.as_bytes(), mode);
    };
    write("root.crt", &r.root, 0o644);
    write("s3.key", &key, 0o640);
    write("s3.crt", &r.certificate, 0o644);
    if real && !run("chgrp", &["hyperstor", "/etc/hyperstor/pki/s3.key"]) {
        fail("chgrp hyperstor /etc/hyperstor/pki/s3.key failed");
    }
    if s.map.is_empty() {
        s.map = r.map_addresses.clone();
    }
    println!(
        "Joined cluster {} through {} as gateway {}.",
        r.cluster_id, s.join, r.identity
    );
}

/// D215: proposes this host's nodes' endpoints from its topology and
/// keeps them in the map, through the cluster API this host joined with
/// its node's certificate (or the administrator's). Each run: the map's
/// data networks, the closest two ports per network for each device,
/// `SetEndpoints` where the map differs, and `--endpoint` and
/// `--networks` for `hyperstord`. Nothing without data networks; a
/// warning when the API can't be reached (the node keeps what the
/// cluster file gives it).
///
/// D285: with `--endpoints off`, and on a map service host (through its
/// own API, with the administrator's certificate), nothing is proposed:
/// the node serves the endpoints the map has for its devices, as
/// `hyperstorctl device endpoints` set them, besides the cluster file's.
fn keep_endpoints(root: &Path, s: &Settings, p: &mut Plan) {
    if p.nodes.is_empty() {
        return;
    }
    let joined = !s.join.is_empty();
    let dir = under(root, "/etc/hyperstor/pki");
    let read = |f: &str| fs::read_to_string(dir.join(f)).unwrap_or_default();
    let who = if joined {
        ["node", "admin"]
            .into_iter()
            .find(|w| dir.join(format!("{w}.crt")).exists())
    } else {
        (!p.api.is_empty() && dir.join("admin.crt").exists()).then_some("admin")
    };
    let Some(who) = who else { return };
    let api = if joined {
        s.join.clone()
    } else {
        p.api.clone()
    };
    let tls = || {
        hyperstor_pki::client_config(
            &read(&format!("{who}.crt")),
            &read(&format!("{who}.key")),
            &read("root.crt"),
        )
    };
    let Ok(t) = tls() else { return };
    let status = api_call(&api, t, |c| async move {
        hyperstor_api::ClusterServiceClient::new(c.0, c.1)
            .get_status(hyperstor_api::GetStatusRequest::default())
            .await
            .map(|r| r.into_owned())
    });
    let status = match status {
        Ok(st) => st,
        // A map service host installed before its map service runs.
        Err(_) if !joined => return,
        Err(e) => {
            eprintln!("hyperstor-install: endpoints: reading the map through {api}: {e}");
            return;
        }
    };
    let networks: Vec<hyperstor_fabric::model::Network> = status
        .networks
        .iter()
        .filter_map(|n| {
            hyperstor_fabric::model::Network::from_parts(&n.name, &n.kind, n.trusted, &n.matches)
        })
        .filter(|n| n.kind.carries_data())
        .collect();
    if networks.is_empty() {
        return;
    }
    if !joined || s.endpoints == "off" {
        for node in &mut p.nodes {
            serve_mapped_endpoints(node, &status);
        }
        return;
    }
    let host = glidex_topology::Host::read();
    let data = under(root, &s.data_dir);
    for node in &mut p.nodes {
        let files = node.paths.iter().filter(|p| p.is_none()).count();
        let devices: Vec<hyperstor_fabric::endpoints::Device> = node
            .ids
            .iter()
            .zip(&node.shards)
            .enumerate()
            .map(|(i, (id, addr))| {
                // Its block device, or the data directory its file goes in.
                let path = match node.paths.get(i).cloned().flatten() {
                    Some(dev) => PathBuf::from(dev),
                    None if files == 1 => data.join(format!("node-{}.img", node.n)),
                    None => data.join(format!("node-{}-{i}.img", node.n)),
                };
                let path = if path.exists() { path } else { data.clone() };
                let pci = hyperstor_fabric::endpoints::block_of_path(&path).and_then(|(a, b)| {
                    hyperstor_fabric::endpoints::pci_of_block(Path::new("/sys"), a, b)
                });
                hyperstor_fabric::endpoints::Device {
                    id: *id,
                    pci,
                    addr: *addr,
                }
            })
            .collect();
        let proposed = hyperstor_fabric::endpoints::propose(&host, &devices, &networks, 2);
        for d in &devices {
            let want = proposed.get(&d.id).cloned().unwrap_or_default();
            let have: Vec<(String, std::net::SocketAddr)> = status
                .nodes
                .iter()
                .flat_map(|n| n.devices.iter())
                .filter(|x| x.id == d.id)
                .flat_map(|x| x.endpoints.iter())
                .filter_map(|e| Some((e.network.clone(), e.address.parse().ok()?)))
                .collect();
            if want != have {
                let Ok(t) = tls() else { return };
                let endpoints = want
                    .iter()
                    .map(|(n, a)| hyperstor_api::Endpoint {
                        network: n.clone(),
                        address: a.to_string(),
                        ..Default::default()
                    })
                    .collect();
                let id = d.id;
                let r = api_call(&s.join, t, |c| async move {
                    hyperstor_api::ClusterServiceClient::new(c.0, c.1)
                        .set_endpoints(hyperstor_api::SetEndpointsRequest {
                            device: id,
                            endpoints,
                            ..Default::default()
                        })
                        .await
                        .map(|r| r.into_owned())
                });
                match r {
                    Ok(_) => println!(
                        "Device {id}'s endpoints: {}.",
                        if want.is_empty() {
                            "none".to_string()
                        } else {
                            want.iter()
                                .map(|(n, a)| format!("{a} on {n}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ),
                    Err(e) => eprintln!("hyperstor-install: endpoints of device {id}: {e}"),
                }
            }
            for (net, a) in want {
                if !node.endpoints.contains(&(d.id, a)) {
                    node.endpoints.push((d.id, a));
                }
                if !node.networks.contains(&net) {
                    node.networks.push(net);
                }
            }
        }
    }
}

/// D289: ` --networks A,B` for this host's clients (`hyperstor-vhost`,
/// `hyperstor-s3`), or nothing: `auto` takes the networks its storage
/// nodes reach others on, in order (D207), so the host's clients take the
/// paths its storage does.
fn networks_arg(s: &Settings, p: &Plan) -> String {
    let names: Vec<String> = match s.client_networks.as_str() {
        "none" => Vec::new(),
        "auto" => {
            let mut v: Vec<String> = Vec::new();
            for n in p.nodes.iter().flat_map(|n| &n.networks) {
                if !v.contains(n) {
                    v.push(n.clone());
                }
            }
            v
        }
        names => names.split(',').map(str::to_string).collect(),
    };
    if names.is_empty() {
        String::new()
    } else {
        format!(" --networks {}", names.join(","))
    }
}

/// D285: `node` serves the endpoints `status`'s map has for its devices,
/// after the cluster file's, and reaches others on their networks.
fn serve_mapped_endpoints(node: &mut NodePlan, status: &hyperstor_api::GetStatusResponse) {
    let mapped = status
        .nodes
        .iter()
        .flat_map(|n| n.devices.iter())
        .filter(|d| node.ids.contains(&d.id))
        .flat_map(|d| d.endpoints.iter().map(move |e| (d.id, e)));
    for (id, e) in mapped {
        let Ok(a) = e.address.parse() else { continue };
        if !node.endpoints.contains(&(id, a)) {
            node.endpoints.push((id, a));
        }
        if !node.networks.contains(&e.network) {
            node.networks.push(e.network.clone());
        }
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
                META_UNIT.replace("DATA_DIR", data).replace(
                    "CLUSTER_KEY",
                    if p.cluster_key {
                        // The cluster key (D162), as the intermediate's.
                        "LoadCredential=cluster.key:/etc/hyperstor/pki/cluster.key\n"
                    } else {
                        ""
                    },
                ),
                0o644,
            ),
            (
                "/etc/hyperstor/mgmt.env".to_string(),
                format!(
                    "HS_ARGS={} --api {} --tls /etc/hyperstor/pki --authd /run/hyperstor-authd/auth.sock --metrics {}{}\n",
                    mgmt_listen_args(s, p),
                    p.api,
                    p.metrics.join(","),
                    if p.ui { format!(" --ui {UI_DIR}") } else { String::new() }
                ),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-mgmt.service".to_string(),
                MGMT_UNIT.to_string(),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-authd.socket".to_string(),
                AUTHD_SOCKET.to_string(),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-authd.service".to_string(),
                AUTHD_SERVICE.to_string(),
                0o644,
            ),
            ("/etc/pam.d/hyperstor".to_string(), PAM_SERVICE.to_string(), 0o644),
            (
                "/etc/hyperstor/authd.json".to_string(),
                AUTHD_CONFIG.to_string(),
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
        out.push((
            "/etc/systemd/system/hyperstor-smart.socket".to_string(),
            SMART_SOCKET.to_string(),
            0o644,
        ));
        out.push((
            "/etc/systemd/system/hyperstor-smart@.service".to_string(),
            SMART_SERVICE.to_string(),
            0o644,
        ));
    }
    if let Some(v) = &p.vhost {
        let mut args = format!("--meta {} --dir {VHOST_DIR}", p.map);
        for u in &v.uids {
            args += &format!(" --allow-uid {u}");
        }
        if let Some(g) = v.gids.first() {
            args += &format!(" --group {g}");
        }
        args += &networks_arg(s, p);
        // It hands its sockets to these groups, so it is in them.
        let groups = if v.gids.is_empty() {
            "# No --vhost-user: only root and hyperstor use it.".to_string()
        } else {
            let g: Vec<String> = v.gids.iter().map(u32::to_string).collect();
            format!("SupplementaryGroups={}", g.join(" "))
        };
        out.extend([
            (
                "/etc/hyperstor/vhost.env".to_string(),
                format!("HS_ARGS={args}\n"),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-vhost.service".to_string(),
                VHOST_UNIT.replace("GROUPS_LINE", &groups),
                0o644,
            ),
        ]);
    }
    if let Some(g) = &p.s3 {
        out.extend([
            (
                "/etc/hyperstor/s3.env".to_string(),
                format!(
                    "HS_ARGS=--meta {} --api {} --identity /etc/hyperstor/pki --tls {} --listen {}{}\n",
                    p.map,
                    g.api,
                    g.tls,
                    g.listen,
                    if g.realm.is_empty() {
                        String::new()
                    } else {
                        format!(" --realm {}", g.realm)
                    } + &if g.domain.is_empty() {
                        String::new()
                    } else {
                        format!(" --domain {}", g.domain)
                    } + &networks_arg(s, p)
                ),
                0o644,
            ),
            (
                "/etc/systemd/system/hyperstor-s3.service".to_string(),
                S3_UNIT.to_string(),
                0o644,
            ),
        ]);
    }
    for node in &p.nodes {
        let n = node.n;
        let shards: Vec<String> = node
            .shards
            .iter()
            .enumerate()
            .map(|(i, a)| {
                // One file per reactor; a single-reactor node keeps the
                // all-in-one's name. A device added is used whole (size
                // 0, D167).
                let added = node.paths.get(i).cloned().flatten();
                let files = node.paths.iter().filter(|p| p.is_none()).count();
                let file = match &added {
                    Some(path) => path.clone(),
                    None if files == 1 => format!("{data}/node-{n}.img"),
                    None => format!("{data}/node-{n}-{i}.img"),
                };
                let size = if added.is_some() { 0 } else { s.size_gib };
                // The id only when it isn't the position's (D169), so
                // existing installs' options stay as they were.
                let id = node.ids.get(i).copied().unwrap_or(n * 1000 + i as u32);
                let named = if id == n * 1000 + i as u32 {
                    String::new()
                } else {
                    format!("{id}@")
                };
                format!("--shard {named}{a}={file}:{size}")
            })
            .collect();
        // Endpoints and networks only when the cluster file has them, so
        // other installs' options stay as they were.
        let mut more: Vec<String> = node
            .endpoints
            .iter()
            .map(|(d, a)| format!(" --endpoint {d}={a}"))
            .collect();
        if !node.networks.is_empty() {
            more.push(format!(" --networks {}", node.networks.join(",")));
        }
        out.push((
            format!("/etc/hyperstor/node-{n}.env"),
            format!(
                "HS_ARGS=--node {n} --meta {} {} --admin {}{}\n",
                p.map,
                shards.join(" "),
                node.admin,
                more.concat()
            ),
            0o644,
        ));
    }
    // The devices added (D167): the service user may open each, and no
    // other disk (not the `disk` group, which would open every one).
    let rules: String = s
        .devices()
        .iter()
        .filter_map(|(_, _, path)| path.strip_prefix("/dev/disk/by-id/"))
        .map(|name| {
            // DEVLINKS lists the disk's links, space-separated: the name
            // last, or followed by a space.
            format!(
                "SUBSYSTEM==\"block\", ENV{{DEVTYPE}}==\"disk\", ENV{{DEVLINKS}}==\"*/dev/disk/by-id/{name}|*/dev/disk/by-id/{name} *\", OWNER=\"hyperstor\", GROUP=\"hyperstor\", MODE=\"0660\"\n"
            )
        })
        .collect();
    if !rules.is_empty() {
        out.push((
            UDEV_RULES.to_string(),
            format!("# Written by hyperstor-install: the block devices hyperstord uses.\n{rules}"),
            0o644,
        ));
    }
    out
}

/// The udev rules for devices added (D167). Their name sorts after
/// `60-persistent-storage.rules`, which makes the `/dev/disk/by-id/`
/// links they match (I333).
const UDEV_RULES: &str = "/etc/udev/rules.d/61-hyperstor.rules";

/// Where they were, read before the by-id links existed, so they never
/// matched (I333): removed.
const OLD_UDEV_RULES: &str = "/etc/udev/rules.d/60-hyperstor.rules";

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

/// A DNS name, lowercase: labels of letters, digits and hyphens, at
/// least two (the S3 gateway's `--domain`, D245).
fn valid_domain(d: &str) -> bool {
    let labels: Vec<&str> = d.split('.').collect();
    labels.len() >= 2
        && d.len() <= 253
        && labels.iter().all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// Every binary of [`BINARIES`] in `dir`, read whole; the first that
/// can't be read is the error.
fn read_binaries(dir: &Path) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    BINARIES
        .iter()
        .map(|b| {
            let src = dir.join(b);
            fs::read(&src).map(|bytes| (*b, bytes)).map_err(|e| {
                format!(
                    "{}: {e} (build the workspace first: cargo build --release); nothing was installed",
                    src.display()
                )
            })
        })
        .collect()
}

/// The binary a unit runs.
fn unit_binary(unit: &str) -> Option<&'static str> {
    Some(match unit {
        u if u.starts_with("hyperstord@") => "hyperstord",
        "hyperstor-meta.service" => "hyperstor-meta",
        "hyperstor-mgmt.service" => "hyperstor-mgmt",
        "hyperstor-authd.socket" | "hyperstor-authd.service" => "hyperstor-authd",
        "hyperstor-s3.service" => "hyperstor-s3",
        "hyperstor-vhost.service" => "hyperstor-vhost",
        _ => return None,
    })
}

/// Whether a process's executable, as `/proc/PID/exe` names it, is
/// `/usr/local/bin/<binary>` as installed now: a replaced file reads
/// "… (deleted)".
fn runs_installed(exe: &Path, binary: &str) -> bool {
    exe == Path::new(&format!("/usr/local/bin/{binary}"))
}

/// The running services among `units` whose process runs another binary
/// than the one installed. A socket stands for its service; one not
/// running isn't stale.
fn stale_units(units: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for u in units {
        let Some(binary) = unit_binary(u) else {
            continue;
        };
        let service = u.replace(".socket", ".service");
        let pid = Command::new("systemctl")
            .args(["show", "-p", "MainPID", "--value", &service])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        if pid == 0 {
            continue;
        }
        if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe"))
            && !runs_installed(&exe, binary)
        {
            out.push(service);
        }
    }
    out
}

fn units(p: &Plan) -> Vec<String> {
    p.meta
        .iter()
        .flat_map(|_| {
            [
                "hyperstor-meta.service".to_string(),
                "hyperstor-mgmt.service".to_string(),
                // Socket-activated: the service starts on the first login.
                "hyperstor-authd.socket".to_string(),
            ]
        })
        .chain(
            p.nodes
                .iter()
                .map(|n| format!("hyperstord@{}.service", n.n)),
        )
        // Socket-activated, a process per request (D221).
        .chain(
            p.nodes
                .first()
                .map(|_| "hyperstor-smart.socket".to_string()),
        )
        .chain(p.s3.iter().map(|_| "hyperstor-s3.service".to_string()))
        .chain(
            p.vhost
                .iter()
                .map(|_| "hyperstor-vhost.service".to_string()),
        )
        .collect()
}

/// Detaches every volume `hyperstor-vhost` serves (before it stops for
/// good: a stopped service's volumes stay held, D143), then stops and
/// removes it. Returns what it removed.
fn remove_vhost(root: &Path, real: bool) -> Vec<String> {
    let unit = "/etc/systemd/system/hyperstor-vhost.service";
    if !under(root, unit).exists() {
        return Vec::new();
    }
    if real {
        let ctl = "/usr/local/bin/hyperstor-vhost";
        let listed = Command::new(ctl)
            .args(["ctl", "--dir", VHOST_DIR, "list"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for v in listed_volumes(&listed) {
            // Stopping for good cuts the VMs off anyway: forced (D256).
            if !run(ctl, &["ctl", "--dir", VHOST_DIR, "detach", &v, "--force"]) {
                eprintln!("hyperstor-install: detaching volume {v} failed; it stays held");
            }
        }
        let _ = run(
            "systemctl",
            &["disable", "--now", "hyperstor-vhost.service"],
        );
    }
    let mut removed = Vec::new();
    for f in [unit, "/etc/hyperstor/vhost.env"] {
        if fs::remove_file(under(root, f)).is_ok() {
            removed.push(f.to_string());
        }
    }
    removed
}

/// Stops and removes `hyperstor-s3` (`--no-s3`, uninstall); its
/// certificate stays, as other certificates do.
fn remove_s3(root: &Path, real: bool) -> Vec<String> {
    let unit = "/etc/systemd/system/hyperstor-s3.service";
    if !under(root, unit).exists() {
        return Vec::new();
    }
    if real {
        let _ = run("systemctl", &["disable", "--now", "hyperstor-s3.service"]);
    }
    let mut removed = Vec::new();
    for f in [unit, "/etc/hyperstor/s3.env"] {
        if fs::remove_file(under(root, f)).is_ok() {
            removed.push(f.to_string());
        }
    }
    removed
}

/// The volume ids in `hyperstor-vhost ctl list`'s answer.
fn listed_volumes(answer: &str) -> Vec<String> {
    answer
        .split("\"volume\":\"")
        .skip(1)
        .filter_map(|r| r.get(..32))
        .filter(|v| v.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect()
}

/// mgmt's listeners and certificates (D108–D110): every address over
/// HTTPS with the cluster CA's certificate (or the operator's, and the
/// node's Tailscale one when granted), or loopback over plain HTTP.
fn mgmt_listen_args(s: &Settings, p: &Plan) -> String {
    if s.mgmt_listen == "loopback" {
        return format!("--listen 127.0.0.1:{}", s.mgmt_port);
    }
    let dir = if s.web_tls.is_empty() {
        "/etc/hyperstor/pki"
    } else {
        s.web_tls.as_str()
    };
    let mut a = format!(
        "--listen 0.0.0.0:{port},[::]:{port} --web-tls {dir}",
        port = s.mgmt_port
    );
    if p.tailscale {
        a.push_str(&format!(" --tailscale {}", tailscale::SOCKET));
    }
    // Behind a proxy (D277).
    if !s.mgmt_hosts.is_empty() {
        a.push_str(&format!(" --hosts {}", s.mgmt_hosts));
    }
    if !s.mgmt_proxy_ca.is_empty() {
        a.push_str(&format!(" --proxy-ca {}", s.mgmt_proxy_ca));
    }
    if !s.mgmt_trusted_proxies.is_empty() {
        a.push_str(&format!(" --trusted-proxies {}", s.mgmt_trusted_proxies));
    }
    if s.mgmt_proxy_only == "on" {
        a.push_str(" --proxy-only");
    }
    a
}

/// `ADDR` or `ADDR/BITS`, as `--mgmt-trusted-proxies` takes them.
fn valid_cidr(v: &str) -> bool {
    let (a, bits) = v.split_once('/').unwrap_or((v, ""));
    match a.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            bits.is_empty()
                || bits
                    .parse::<u8>()
                    .is_ok_and(|b| b <= if ip.is_ipv4() { 32 } else { 128 })
        }
        Err(_) => false,
    }
}

/// Where the web UI's files go (D100).
const UI_DIR: &str = "/usr/local/share/hyperstor/ui";

/// The S3 gateway's port unless `--s3-port` says otherwise (D247): just
/// below the map service's 8849, since the all-in-one's ports per node
/// (metrics from 8853, storage from 9100) grow upward.
const DEFAULT_S3_PORT: u16 = 8848;

/// Where a promoted host's member talks Raft (D288), as the pilot's and
/// the installation guide's members do.
const DEFAULT_RAFT_PORT: u16 = 8851;

/// Makes `UI_DIR` under `root` hold exactly `src`'s files (mode 0644):
/// changed ones written, gone ones removed. Whether anything changed.
fn sync_ui(root: &Path, src: &Path) -> bool {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display()))) {
            let e = e.unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display())));
            let path = e.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                out.push(path.strip_prefix(base).expect("under base").to_path_buf());
            }
        }
    }
    if !src.join("index.html").is_file() {
        fail(&format!(
            "{}: no index.html (bun run build in crates/hyperstor-mgmt/ui)",
            src.display()
        ));
    }
    let dst = under(root, UI_DIR);
    let mut want = Vec::new();
    walk(src, src, &mut want);
    let mut changed = false;
    for rel in &want {
        let bytes =
            fs::read(src.join(rel)).unwrap_or_else(|e| fail(&format!("{}: {e}", rel.display())));
        changed |= converge(&dst.join(rel), &bytes, 0o644);
    }
    let mut have = Vec::new();
    if dst.is_dir() {
        walk(&dst, &dst, &mut have);
    }
    for rel in have.iter().filter(|r| !want.contains(r)) {
        changed |= fs::remove_file(dst.join(rel)).is_ok();
    }
    changed
}

fn install(
    root: &Path,
    real: bool,
    no_start: bool,
    s: &Settings,
    cluster_file: Option<String>,
    ui_from: Option<String>,
) {
    let mut own = s.clone();
    let s = &mut own;
    let mut p = plan(root, s, cluster_file);
    follow_members(root, s, &mut p);
    // SAFETY: geteuid has no preconditions.
    if real && unsafe { libc::geteuid() } != 0 {
        fail("installing to / needs root: run it with sudo (or use --root DIR)");
    }
    // Every binary read before any is written: one missing changes
    // nothing, rather than leaving some new and some old.
    let here = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| fail("can't find my own directory"));
    let binaries: Vec<(&str, Vec<u8>)> = read_binaries(&here).unwrap_or_else(|e| fail(&e));
    keep_endpoints(root, s, &mut p);
    if real {
        println!("Checking the host:");
        if check(root, s) {
            fail("a host check failed; see above");
        }
        println!();
    }
    let mut changed: Vec<String> = Vec::new();

    // 1. Binaries, from beside this executable, read above.
    let mut binaries_changed: Vec<&str> = Vec::new();
    for (b, bytes) in &binaries {
        let dst = format!("/usr/local/bin/{b}");
        if converge(&under(root, &dst), bytes, 0o755) {
            binaries_changed.push(*b);
            changed.push(dst);
        }
    }
    drop(binaries);

    // The web UI (D100), where mgmt runs: from --ui, else from beside
    // this executable (a release's `hyperstor-ui`). Kept as it is when
    // there is neither, as for the renewal timer's runs.
    if p.meta.is_some() {
        let src = ui_from
            .map(PathBuf::from)
            .or_else(|| Some(here.join("hyperstor-ui")).filter(|d| d.is_dir()));
        if let Some(src) = src
            && sync_ui(root, &src)
        {
            changed.push(UI_DIR.into());
        }
        p.ui = under(root, &format!("{UI_DIR}/index.html")).is_file();
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
    if real && service_user(s) {
        changed.push("user hyperstor".into());
    }
    // People who may log in to hyperstor-mgmt with their password (D99).
    if real
        && !Command::new("getent")
            .args(["group", "hyperstor-users"])
            .output()
            .is_ok_and(|o| o.status.success())
    {
        if !run("groupadd", &["--system", "hyperstor-users"]) {
            fail("groupadd hyperstor-users failed");
        }
        changed.push("group hyperstor-users".into());
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
        // mgmt's web certificate from the cluster CA (D108), unless the
        // operator brings their own.
        let web =
            (s.mgmt_listen == "all" && s.web_tls.is_empty()).then(glidex_tls::LocalNames::discover);
        // The gateway's names, for its certificate (D233).
        let s3_names = p.s3.as_ref().map(|_| glidex_tls::LocalNames::discover());
        // Other members' cluster APIs, where a missing cluster key comes
        // from: the one this host joined through, and each listed
        // member's address on this host's API port.
        let mut peers: Vec<String> = Vec::new();
        if !s.join.is_empty() {
            peers.push(s.join.clone());
        }
        for m in &p.members {
            if Some(m.id) != p.member_id {
                peers.push(format!("{}:{}", m.map.ip(), s.api_port));
            }
        }
        peers.dedup();
        restart.extend(pki(
            root,
            real,
            &p.cluster_id,
            &member,
            &p.server_ips,
            web.as_ref(),
            s3_names.as_ref(),
            &peers,
            &mut changed,
        ));
        p.cluster_key = under(root, "/etc/hyperstor/pki/cluster.key").exists();
    } else if p.s3.is_some() && renew_leaf(root, real, s, "s3") {
        // A joined gateway's own certificate; it reads it at start.
        restart.push("hyperstor-s3.service".into());
    }
    // The node's Tailscale certificate for mgmt (D109): granted by a
    // tailscaled drop-in, unless someone else holds the grant.
    if p.meta.is_some() && s.mgmt_listen == "all" {
        let dropin = under(root, tailscale::DROPIN);
        let mut tailscaled_changed = false;
        if s.tailscale_cert {
            let uid = if real {
                Command::new("id")
                    .args(["-u", tailscale::USER])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            } else {
                None
            };
            let installed = !real
                || Path::new("/lib/systemd/system/tailscaled.service").exists()
                || Path::new("/usr/lib/systemd/system/tailscaled.service").exists()
                || Path::new("/etc/systemd/system/tailscaled.service").exists();
            // Our own drop-in doesn't count as someone else's grant.
            let configured = if tailscale::ours(&dropin) {
                Some(tailscale::USER.to_string())
            } else {
                tailscale::configured(root, real)
            };
            match (installed, tailscale::grant(configured.as_deref(), uid)) {
                (false, _) => eprintln!(
                    "hyperstor-install: --tailscale-cert: tailscaled isn't installed here; mgmt serves its usual certificate"
                ),
                (true, tailscale::Grant::Refuse(who)) => eprintln!(
                    "hyperstor-install: --tailscale-cert: tailscaled lets only {who} fetch certificates (TS_PERMIT_CERT_UID); left as it is, and mgmt serves its usual certificate"
                ),
                (true, tailscale::Grant::Have) => p.tailscale = true,
                (true, tailscale::Grant::Write) => {
                    if converge(&dropin, tailscale::dropin().as_bytes(), 0o644) {
                        changed.push(tailscale::DROPIN.into());
                        tailscaled_changed = true;
                    }
                    p.tailscale = true;
                }
            }
        } else if tailscale::ours(&dropin) {
            let _ = fs::remove_file(&dropin);
            changed.push(format!("removed {}", tailscale::DROPIN));
            tailscaled_changed = true;
        }
        if tailscaled_changed {
            if real && !no_start {
                // Before mgmt restarts, so its first fetch is allowed.
                let _ = run("systemctl", &["daemon-reload"]);
                if !run("systemctl", &["try-restart", "tailscaled.service"]) {
                    eprintln!("hyperstor-install: restarting tailscaled failed");
                }
            } else {
                println!("would restart tailscaled.service");
            }
        }
    }
    let mut units_changed = false;
    if p.vhost.is_none() {
        let removed = remove_vhost(root, real && !no_start);
        units_changed |= !removed.is_empty();
        changed.extend(removed.into_iter().map(|r| format!("removed {r}")));
    }
    if p.s3.is_none() {
        let removed = remove_s3(root, real && !no_start);
        units_changed |= !removed.is_empty();
        changed.extend(removed.into_iter().map(|r| format!("removed {r}")));
    }
    if fs::remove_file(under(root, OLD_UDEV_RULES)).is_ok() {
        changed.push(format!("removed {OLD_UDEV_RULES}"));
    }
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
        if path.ends_with("hyperstor-authd.service") || path.ends_with("authd.json") {
            service_units_changed.push("hyperstor-authd");
        }
        if path.ends_with("hyperstord@.service") {
            service_units_changed.push("hyperstord");
        }
        // Its service runs per request, so only the socket restarts.
        if path.ends_with("hyperstor-smart.socket") {
            restart.push("hyperstor-smart.socket".into());
        }
        // Restarting it keeps its VMs' disks (D143).
        if path.ends_with("hyperstor-vhost.service") || path.ends_with("vhost.env") {
            restart.push("hyperstor-vhost.service".into());
        }
        if path.ends_with("hyperstor-s3.service") || path.ends_with("s3.env") {
            restart.push("hyperstor-s3.service".into());
        }
        if path.ends_with("cluster.txt") || path.ends_with("meta.env") {
            restart.push("hyperstor-meta.service".into());
        }
        if path.ends_with("mgmt.env") {
            restart.push("hyperstor-mgmt.service".into());
        }
        if path == UDEV_RULES && real {
            let _ = run("udevadm", &["control", "--reload-rules"]);
            let _ = run(
                "udevadm",
                &["trigger", "--subsystem-match=block", "--action=change"],
            );
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
            "hyperstor-vhost" => u == "hyperstor-vhost.service",
            "hyperstor-s3" => u == "hyperstor-s3.service",
            _ => false,
        }));
        // The helper's service, not its socket, which stays: systemd hands
        // it the socket again.
        if *what == "hyperstor-authd" && p.meta.is_some() {
            restart.push("hyperstor-authd.service".into());
        }
    }
    // Services running another binary than the one installed: a run that
    // stopped after writing binaries, or a binary replaced by hand. Their
    // binaries are unchanged now, so nothing above restarts them.
    if real {
        restart.extend(stale_units(&units(&p)));
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

    // A promoted host's member, once its map service answers (D288).
    if s.member_pending && ensure_member(root, s, &p, if real && !no_start { 120 } else { 5 }) {
        s.member_pending = false;
        if converge(
            &under(root, "/etc/hyperstor/install.conf"),
            s.save().as_bytes(),
            0o644,
        ) {
            changed.push("/etc/hyperstor/install.conf".into());
        }
    }
    // The all-in-one's default placement, once its map service answers.
    if let Some(done) = s3_default_placement(root, s, &p, if real && !no_start { 30 } else { 1 }) {
        changed.push(done);
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
        if let Some(g) = &p.s3 {
            println!(
                "S3 on https://<this host>:{}/ : hyperstorctl s3 key create --tenant 1 prints a key and its secret",
                g.listen.rsplit(':').next().unwrap_or("8848")
            );
        }
        if !s.all_in_one {
            println!(
                "Add a storage host: hyperstorctl token create --role node, then on that host\n  sudo hyperstor-install install --join {} --token-file FILE --role storage --address IP",
                p.api
            );
        }
    }
}

/// The root certificate this host trusts.
fn root_cert_of(root: &Path) -> String {
    fs::read_to_string(under(root, "/etc/hyperstor/pki/root.crt")).unwrap_or_default()
}

/// The cluster key from the first of `peers` (cluster API addresses) that
/// has one, asked with this member's own certificate (`GetClusterKey`,
/// for members only).
fn fetch_cluster_key(
    api: &hyperstor_pki::Pem,
    root_cert: &str,
    peers: &[String],
) -> Option<hyperstor_pki::ClusterKey> {
    for peer in peers {
        let Ok(tls) = hyperstor_pki::client_config(&api.cert, &api.key, root_cert) else {
            return None;
        };
        let r = api_call(peer, tls, |c| async move {
            hyperstor_api::CaServiceClient::new(c.0, c.1)
                .get_cluster_key(hyperstor_api::GetClusterKeyRequest::default())
                .await
                .map(|r| r.into_owned())
        });
        match r.map(|r| hyperstor_pki::ClusterKey::parse(&r.cluster_key)) {
            Ok(Some(k)) => {
                println!("Fetched the cluster key from member {peer}.");
                return Some(k);
            }
            Ok(None) => eprintln!("hyperstor-install: {peer} answered something not a cluster key"),
            Err(e) => eprintln!("hyperstor-install: the cluster key from {peer}: {e}"),
        }
    }
    None
}

/// Where a run records that it set the all-in-one's default placement.
const S3_PLACED: &str = "/etc/hyperstor/s3-placement.set";

/// The all-in-one cluster's `default` S3 placement (D232): its replicated
/// pool, `vol`, for data and index, set through the API with the
/// administrator's certificate when no placement is, once: what an
/// administrator sets or deletes later stays. Tries `tries` times, a
/// second apart, while the map service starts; what it did, if anything.
fn s3_default_placement(root: &Path, s: &Settings, p: &Plan, tries: u32) -> Option<String> {
    if !s.all_in_one || p.s3.is_none() || p.meta.is_none() || under(root, S3_PLACED).exists() {
        return None;
    }
    let dir = under(root, "/etc/hyperstor/pki");
    let read = |f: &str| fs::read_to_string(dir.join(f)).unwrap_or_default();
    for i in 0..tries {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        let tls =
            hyperstor_pki::client_config(&read("admin.crt"), &read("admin.key"), &read("root.crt"))
                .ok()?;
        let r = api_call(&p.api, tls, |c| async move {
            let client = hyperstor_api::S3ServiceClient::new(c.0, c.1);
            let have = client
                .list_s3_placements(hyperstor_api::ListS3PlacementsRequest::default())
                .await?
                .into_owned();
            if !have.placements.is_empty() {
                return Ok(false);
            }
            client
                .set_s3_placement(hyperstor_api::SetS3PlacementRequest {
                    placement: Some(hyperstor_api::S3Placement {
                        name: "default".into(),
                        data_pool: "vol".into(),
                        index_pool: "vol".into(),
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                })
                .await?;
            Ok(true)
        });
        match r {
            Ok(set) => {
                converge(&under(root, S3_PLACED), b"", 0o644);
                return set.then(|| "S3's default placement: pool vol".to_string());
            }
            Err(e) if i + 1 == tries && tries > 1 => {
                eprintln!(
                    "hyperstor-install: S3's default placement through {}: {e}",
                    p.api
                );
            }
            Err(_) => {}
        }
    }
    None
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
#[allow(clippy::too_many_arguments)]
fn pki(
    root: &Path,
    real: bool,
    cluster: &str,
    member: &str,
    server_ips: &[std::net::IpAddr],
    web: Option<&glidex_tls::LocalNames>,
    s3: Option<&glidex_tls::LocalNames>,
    peers: &[String],
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
    // The cluster key (D162): from another member that has it (a member
    // that joined before it existed, or lost it), else made where the
    // root is; members that join get it with the intermediate.
    if read("cluster.key").is_none() {
        let fetched =
            pair("api").and_then(|api| fetch_cluster_key(&api, &root_cert_of(root), peers));
        let made = match (&fetched, &rootca) {
            (Some(_), _) => fetched,
            (None, Some(_)) => {
                Some(ClusterKey::generate().unwrap_or_else(|e| fail(&format!("cluster key: {e}"))))
            }
            (None, None) => None,
        };
        match made {
            Some(k) => {
                converge(
                    &under(root, &format!("{dir}/cluster.key")),
                    k.text().as_bytes(),
                    0o600,
                );
                changed.push(format!("{dir}/cluster.key"));
            }
            None => eprintln!(
                "hyperstor-install: no cluster key on this host, and no other member gave one: the map service can't keep S3 keys here until one does (run install again once another member has one), or restore the root key here (root-restore) and install again"
            ),
        }
    }
    // Services read their certificates at start: restart those whose
    // certificate is new.
    let mut restart = Vec::new();
    let mut leaves = vec![
        ("api", Role::Meta, member, true, "hyperstor"),
        ("admin", Role::Admin, "local", false, "hyperstor-admin"),
        // The dashboard's identity (D50); it only reads.
        ("mgmt", Role::Mgmt, "dashboard", false, "hyperstor"),
    ];
    if web.is_some() {
        // mgmt's web certificate for people (D108): for every name and
        // address of this host. mgmt re-reads it when it changes.
        leaves.push(("web", Role::Mgmt, "web", true, "hyperstor"));
    }
    if s3.is_some() {
        // The gateway's identity (D233), which reads S3's keys; it serves
        // HTTPS with it too unless the site brings a certificate, so it
        // names this host's names and addresses.
        leaves.push(("s3", Role::S3, member, true, "hyperstor"));
    }
    for (name, role, id, server, group) in leaves {
        // A server certificate names where it is reached: loopback, and
        // for a map service member its addresses (D72). Kept only while it
        // names the same ones, recorded beside it.
        let (ips, names) = if let (Some(w), "web") = (web, name) {
            (w.ips.clone(), w.dns.clone())
        } else if let (Some(n), "s3") = (s3, name) {
            let mut ips: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
            ips.extend(server_ips.iter().copied());
            ips.extend(n.ips.iter().copied());
            ips.sort();
            ips.dedup();
            let mut dns = n.dns.clone();
            dns.push("localhost".into());
            dns.sort();
            dns.dedup();
            (ips, dns)
        } else if server {
            let mut ips: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
            ips.extend(server_ips.iter().copied());
            ips.sort();
            ips.dedup();
            (ips, vec!["localhost".to_string()])
        } else {
            (vec![], vec![])
        };
        let recorded: String = ips
            .iter()
            .map(|ip| ip.to_string())
            .chain(if name == "web" || name == "s3" {
                names.clone()
            } else {
                vec![]
            })
            .map(|l| format!("{l}\n"))
            .collect();
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
        if name == "api" {
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
            "s3" => restart.push("hyperstor-s3.service".to_string()),
            _ => {}
        }
    }
    restart
}

/// Passphrases shorter than this are refused: the backup is only as good
/// as its passphrase.
const MIN_PASSPHRASE: usize = 12;

/// `root-backup`: see the top of this file.
/// Opens `path` as hyperstord does: `O_DIRECT`, or buffered where the
/// filesystem refuses it (tmpfs, for tests). Never creates it.
fn open_device(path: &str) -> Result<std::sync::Arc<dyn hyperstor_engine::BlockDev>, String> {
    use hyperstor_engine::FileDev;
    let p = Path::new(path);
    Ok(match FileDev::open(p, true, None) {
        Ok(d) => std::sync::Arc::new(d),
        Err(_) => {
            std::sync::Arc::new(FileDev::open(p, false, None).map_err(|e| format!("{path}: {e}"))?)
        }
    })
}

/// Adds the block devices `paths` to this host's storage node `s.node`
/// (24-pools-and-devices.md §27.2.1; D167, D168). Each is checked for
/// another system's signature before anything changes; registered with
/// the map service (`AddDevice`: with the administrator's certificate
/// through this member's API, or with the node's own through the API a
/// storage-only host joined with, D280; the leader's if that member
/// isn't it); formatted with the id it was given; and recorded in
/// `s.devices`, which this run and every later one turn into a `--shard`.
/// A device already recorded is left as it is.
fn add_devices(
    root: &Path,
    real: bool,
    s: &mut Settings,
    cluster_file: Option<String>,
    paths: &[String],
    address: Option<String>,
    force: bool,
) {
    use std::os::unix::fs::FileTypeExt;
    let n = s
        .node
        .unwrap_or_else(|| fail("--device needs --node N: the storage node to add it to"));
    let pki = under(root, "/etc/hyperstor/pki");
    let p = plan(root, s, cluster_file);
    // The administrator's certificate on a map service host, through its
    // own API; a storage-only host's node certificate, through the API it
    // joined with (D280).
    let (who, api) = if pki.join("admin.key").exists() {
        ("admin", p.api.clone())
    } else if pki.join("node.key").exists() && !s.join.is_empty() {
        ("node", s.join.clone())
    } else {
        fail(
            "--device needs the administrator's certificate (a map service host) or the node's own (a host joined with --join)",
        );
    };
    let Some(node) = p.nodes.iter().find(|x| x.n == n) else {
        fail(&format!("node {n} isn't this host's"));
    };
    let new: Vec<&String> = paths
        .iter()
        .filter(|path| !s.devices().iter().any(|d| &d.2 == *path))
        .collect();
    if address.is_some() && new.len() > 1 {
        fail("--device-address names one device's address: add devices one at a time");
    }
    // The next free port after the node's own, skipping the others in use.
    let ip = node
        .shards
        .first()
        .map(|a| a.ip())
        .unwrap_or_else(|| fail(&format!("node {n} has no address")));
    let used: Vec<u16> = p
        .nodes
        .iter()
        .flat_map(|x| x.shards.iter().map(|a| a.port()))
        .chain([s.api_port, s.map_port, s.mgmt_port])
        .chain((0..s.nodes.max(1)).map(|i| s.admin_port + i as u16))
        .collect();
    let mut port = node
        .shards
        .iter()
        .map(|a| a.port())
        .max()
        .unwrap_or(s.node_port);
    for path in new {
        let meta = fs::metadata(path).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
        let block = meta.file_type().is_block_device();
        if block && !path.starts_with("/dev/disk/by-id/") {
            fail(&format!(
                "{path}: name a block device by /dev/disk/by-id/: /dev/nvmeXnY and /dev/sdX can change at boot"
            ));
        }
        // Before anything changes: refused here leaves the map as it was.
        let dev = open_device(path).unwrap_or_else(|e| fail(&e));
        let sig = hyperstor_engine::probe::signature(&dev)
            .unwrap_or_else(|e| fail(&format!("{path}: {e}")));
        drop(dev);
        if let Some(sig) = &sig
            && !force
        {
            fail(&format!(
                "{path} holds {sig}: not added. Check the path; --force-format overwrites it"
            ));
        }
        let addr = match &address {
            Some(a) => a
                .parse::<std::net::SocketAddr>()
                .unwrap_or_else(|_| fail(&format!("--device-address {a}: not IP:PORT"))),
            None => {
                port += 1;
                while used.contains(&port) {
                    port += 1;
                }
                std::net::SocketAddr::new(ip, port)
            }
        };
        let id = register_device(who, &api, &pki, s.api_port, n, addr);
        match format_device(root, path, id, force) {
            Ok(done) => println!("{done}"),
            Err(e) => fail(&e),
        }
        if real && block {
            // Now; the udev rule this run writes keeps it across boots.
            let real_path = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
            if !run(
                "chown",
                &["hyperstor:hyperstor", &real_path.to_string_lossy()],
            ) {
                fail(&format!("chown hyperstor {} failed", real_path.display()));
            }
        }
        let entry = format!("{id}@{addr}={path}");
        s.devices = if s.devices.is_empty() {
            entry
        } else {
            format!("{},{entry}", s.devices)
        };
        println!("added {path} to node {n} as device {id}, served at {addr}");
    }
}

/// Drops devices removed from the map from this host's node
/// (24-pools-and-devices.md §27.2.5, D186): recorded in
/// `s.removed_devices` (and out of `s.devices`), so this run and every
/// later one leave out their `--shard`. With the administrator's
/// certificate, a device the map still has is refused: `hyperstorctl
/// device remove` comes first. A node's last device is refused: the node
/// left the map with it.
fn remove_devices(root: &Path, s: &mut Settings, cluster_file: Option<String>, ids: &[u32]) {
    let p = plan(root, s, cluster_file);
    let pki = under(root, "/etc/hyperstor/pki");
    let checked = pki.join("admin.key").exists();
    let in_map: Vec<u32> = if checked {
        let ctl = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("hyperstorctl")))
            .unwrap_or_else(|| fail("can't find hyperstorctl beside me"));
        let out = Command::new(&ctl)
            .args(["--api", &p.api, "--tls"])
            .arg(&pki)
            .args(["nodes", "--json"])
            .output()
            .unwrap_or_else(|e| fail(&format!("{}: {e}", ctl.display())));
        if !out.status.success() {
            fail(&format!(
                "can't read the map to check the device has left it: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| fail(&format!("hyperstorctl nodes: {e}")));
        v["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|n| n["devices"].as_array().cloned().unwrap_or_default())
            // JSON leaves out a 0: device 0.
            .map(|d| d["id"].as_u64().unwrap_or(0) as u32)
            .collect()
    } else {
        Vec::new()
    };
    for &id in ids {
        if s.removed().contains(&id) {
            continue;
        }
        let Some(node) = p.nodes.iter().find(|n| n.ids.contains(&id)) else {
            fail(&format!(
                "device {id} isn't one of this host's nodes' devices"
            ));
        };
        if node.ids.iter().all(|d| *d == id || ids.contains(d)) {
            fail(&format!(
                "device {id} is node {}'s last: the node left the map with it; uninstall the storage role instead",
                node.n
            ));
        }
        if in_map.contains(&id) {
            fail(&format!(
                "device {id} is still in the map: remove it first (hyperstorctl device remove --device {id}) and wait for it to leave"
            ));
        }
        if !checked {
            println!(
                "no administrator's certificate here to check the map: device {id} is taken to have left it"
            );
        }
        s.devices = s
            .devices()
            .iter()
            .filter(|d| d.0 != id)
            .map(|(d, a, path)| format!("{d}@{a}={path}"))
            .collect::<Vec<_>>()
            .join(",");
        s.removed_devices = if s.removed_devices.is_empty() {
            id.to_string()
        } else {
            format!("{},{id}", s.removed_devices)
        };
        println!(
            "dropped device {id} from node {}; its data is left as it is",
            node.n
        );
    }
}

/// Registers a device at `addr` on node `n` (`AddDevice`, with the
/// certificate `who` in `pki`: `admin`, or a storage-only host's `node`,
/// D280) and returns its id: through `api`, else through the leader it
/// names; an address already registered (a run that stopped before
/// saving) gives that device back.
fn register_device(
    who: &str,
    api: &str,
    pki: &Path,
    api_port: u16,
    n: u32,
    addr: std::net::SocketAddr,
) -> u32 {
    let read = |f: &str| fs::read_to_string(pki.join(f)).unwrap_or_default();
    let mut api = api.to_string();
    for _ in 0..2 {
        let tls = hyperstor_pki::client_config(
            &read(&format!("{who}.crt")),
            &read(&format!("{who}.key")),
            &read("root.crt"),
        )
        .unwrap_or_else(|e| fail(&format!("{who}'s certificate: {e}")));
        let r = api_call(&api, tls, |c| async move {
            hyperstor_api::ClusterServiceClient::new(c.0, c.1)
                .add_device(hyperstor_api::AddDeviceRequest {
                    node: n,
                    address: addr.to_string(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
        });
        let text = match r {
            Ok(r) => return r.device,
            Err(e) => e,
        };
        let number_after = |key: &str| -> Option<u32> {
            let rest = &text[text.find(key)? + key.len()..];
            rest.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        };
        if let Some(id) = number_after("is already device ")
            && id / 1000 == n
        {
            return id;
        }
        // A follower: the leader's map address, and its API on our port.
        if let Some(leader) = text
            .split("the leader is ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|a| {
                a.trim_end_matches([')', '.', ','])
                    .parse::<std::net::SocketAddr>()
                    .ok()
            })
        {
            api = std::net::SocketAddr::new(leader.ip(), api_port).to_string();
            continue;
        }
        fail(&format!("adding the device through {api}: {}", text.trim()));
    }
    fail("adding the device: the leader kept moving; run it again")
}

/// `format-device --path P --id D [--force-format] [--root DIR]`
/// (D168): formats a block device for `hyperstord`, which formats only
/// files. Exits.
fn format_device_cmd(args: &[String]) -> ! {
    let (mut path, mut device, mut force, mut root) = (None, None, false, PathBuf::from("/"));
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| usage());
        match a.as_str() {
            "--path" => path = Some(val()),
            "--id" => device = Some(val().parse::<u32>().unwrap_or_else(|_| usage())),
            "--force-format" => force = true,
            "--root" => root = PathBuf::from(val()),
            _ => usage(),
        }
    }
    let (Some(path), Some(device)) = (path, device) else {
        usage()
    };
    match format_device(&root, &path, device, force) {
        Ok(done) => {
            println!("{done}");
            std::process::exit(0)
        }
        Err(e) => fail(&e),
    }
}

/// Formats `path` as device `id` (D168), only when it holds no other
/// signature (hyperstor_engine::probe), unless `force`: then its first
/// and last 128 KiB are zeroed first, so no old signature is left for
/// other tools to find. The cluster is the installed cluster file's, or
/// 0, which the node replaces with the map's at its first start (I203).
fn format_device(root: &Path, path: &str, id: u32, force: bool) -> Result<String, String> {
    use hyperstor_engine::{AlignedBuf, DeviceConfig};
    let dev = open_device(path)?;
    // Run again (a later install): already this device, so done.
    if let Ok(i) = hyperstor_engine::device_info(&dev)
        && i.device_id == id as u128
    {
        return Ok(format!("{path} is device {id} already"));
    }
    let found = hyperstor_engine::probe::signature(&dev).map_err(|e| format!("{path}: {e}"))?;
    if let Some(sig) = &found {
        if !force {
            return Err(format!(
                "{path} holds {sig}: not formatted. Check the path; --force-format overwrites it"
            ));
        }
        let span = (128u64 << 10).min(dev.len());
        let zero = AlignedBuf::zeroed(span as usize);
        for at in [0, dev.len() - span] {
            dev.write(at, &zero).map_err(|e| format!("{path}: {e}"))?;
        }
        dev.flush().map_err(|e| format!("{path}: {e}"))?;
    }
    let cluster: u128 = fs::read_to_string(root.join("etc/hyperstor/cluster.txt"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.trim().strip_prefix("cluster ")?.trim().parse().ok())
        })
        .unwrap_or(0);
    hyperstor_engine::format(
        &dev,
        &DeviceConfig {
            device_id: id as u128,
            cluster_id: cluster,
            shards: 1,
            ..Default::default()
        },
    )
    .map_err(|e| format!("{path}: {e}"))?;
    Ok(match found {
        Some(sig) => format!("formatted {path} as device {id} of cluster {cluster}, over {sig}"),
        None => format!("formatted {path} as device {id} of cluster {cluster}"),
    })
}

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
    // The cluster key (D162) goes with the root, where this host has it.
    let cluster_key = fs::read_to_string(pki.join("cluster.key")).ok().map(|t| {
        hyperstor_pki::ClusterKey::parse(&t).unwrap_or_else(|| {
            fail(&format!(
                "{}: not a cluster key",
                pki.join("cluster.key").display()
            ))
        })
    });
    let cluster = cluster_id(root).to_string();
    let text = hyperstor_pki::root_backup(&ca, cluster_key.as_ref(), &cluster, passphrase)
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
        Ok(r)
            if r.root.key == ca.key
                && r.root.cert == ca.cert
                && r.cluster_key.as_ref().map(|k| k.text())
                    == cluster_key.as_ref().map(|k| k.text()) => {}
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
        "Wrote {out}: the root CA{}, encrypted. Keep it, and the passphrase, apart and offline.",
        if cluster_key.is_some() {
            " and the cluster key"
        } else {
            ""
        }
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
    let Ok(hyperstor_pki::Restored {
        root: ca,
        cluster_key,
        ..
    }) = hyperstor_pki::root_restore(&text, passphrase)
    else {
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
    // Only the cluster key this host already has, if any: another would
    // leave the secrets sealed with this one unreadable.
    if let (Some(k), Ok(have)) = (&cluster_key, fs::read_to_string(pki.join("cluster.key")))
        && have != k.text()
    {
        fail(
            "this host has another cluster key: the backup's would make the stored secrets unreadable",
        );
    }
    converge(&pki.join("root.crt"), ca.cert.as_bytes(), 0o644);
    converge(&pki.join("root.key"), ca.key.as_bytes(), 0o600);
    if let Some(k) = &cluster_key {
        converge(&pki.join("cluster.key"), k.text().as_bytes(), 0o600);
    }
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
    // Its volumes detached first, so they don't stay held (D143).
    let mut removed = remove_vhost(root, real);
    removed.extend(remove_s3(root, real));
    if real {
        // What was installed: the map service if its options are, and
        // every storage node with an options file.
        let mut all: Vec<String> = Vec::new();
        if under(root, "/etc/hyperstor/meta.env").exists() {
            all.push("hyperstor-meta.service".into());
        }
        if under(root, "/etc/hyperstor/mgmt.env").exists() {
            all.push("hyperstor-mgmt.service".into());
            all.push("hyperstor-authd.socket".into());
            all.push("hyperstor-authd.service".into());
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
        if under(root, "/etc/systemd/system/hyperstor-smart.socket").exists() {
            all.push("hyperstor-smart.socket".into());
        }
        all.push("hyperstor-renew.timer".into());
        let mut args = vec!["disable", "--now"];
        args.extend(all.iter().map(String::as_str));
        // Units that were never installed make this fail; that's fine.
        let _ = run("systemctl", &args);
    }
    let paths = [
        "/etc/systemd/system/hyperstor-meta.service".to_string(),
        "/etc/systemd/system/hyperstord@.service".to_string(),
        "/etc/systemd/system/hyperstor-mgmt.service".to_string(),
        "/etc/systemd/system/hyperstor-authd.socket".to_string(),
        "/etc/systemd/system/hyperstor-authd.service".to_string(),
        "/etc/systemd/system/hyperstor-smart.socket".to_string(),
        "/etc/systemd/system/hyperstor-smart@.service".to_string(),
        "/etc/systemd/system/hyperstor-renew.service".to_string(),
        "/etc/systemd/system/hyperstor-renew.timer".to_string(),
        "/usr/local/bin/hyperstor-install".to_string(),
        UDEV_RULES.to_string(),
        OLD_UDEV_RULES.to_string(),
    ]
    .into_iter()
    .chain(BINARIES.iter().map(|b| format!("/usr/local/bin/{b}")));
    for p in paths {
        if fs::remove_file(under(root, &p)).is_ok() {
            removed.push(p);
        }
    }
    if fs::remove_dir_all(under(root, UI_DIR)).is_ok() {
        removed.push(UI_DIR.to_string());
    }
    let dropin = under(root, tailscale::DROPIN);
    if tailscale::ours(&dropin) && fs::remove_file(&dropin).is_ok() {
        removed.push(tailscale::DROPIN.to_string());
        if real {
            let _ = run("systemctl", &["daemon-reload"]);
            let _ = run("systemctl", &["try-restart", "tailscaled.service"]);
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
    fn services_on_replaced_binaries_are_stale() {
        assert!(runs_installed(
            Path::new("/usr/local/bin/hyperstord"),
            "hyperstord"
        ));
        assert!(!runs_installed(
            Path::new("/usr/local/bin/hyperstord (deleted)"),
            "hyperstord"
        ));
        assert!(!runs_installed(
            Path::new("/home/x/hyperstord"),
            "hyperstord"
        ));
        assert_eq!(unit_binary("hyperstord@3.service"), Some("hyperstord"));
        assert_eq!(
            unit_binary("hyperstor-authd.socket"),
            Some("hyperstor-authd")
        );
        // Socket-activated, a process per request: nothing kept running.
        assert_eq!(unit_binary("hyperstor-smart.socket"), None);
        let dir = tempfile::tempdir().unwrap();
        let e = read_binaries(dir.path()).unwrap_err();
        assert!(
            e.contains("hyperstor-meta") && e.contains("nothing was installed"),
            "{e}"
        );
    }

    /// D247: the gateway's default port is none of the ports the
    /// installer gives anything else by default, for an all-in-one of up
    /// to 64 nodes (each node takes a metrics and a storage port).
    #[test]
    fn the_default_s3_port_is_no_other_default_port() {
        let s = Settings::defaults();
        assert_eq!(s.s3_port, 8848);
        for nodes in 1..=64u16 {
            let others: Vec<u16> = [s.api_port, s.map_port, s.mgmt_port]
                .into_iter()
                .chain((0..nodes).map(|n| s.admin_port + n))
                .chain((0..nodes).map(|n| s.node_port + n))
                .collect();
            assert!(!others.contains(&s.s3_port), "{nodes} nodes: {others:?}");
        }
    }

    /// D168: format-device formats a blank device with the installed
    /// cluster's id, refuses one that holds another system's signature
    /// (or hyperstor's), and overwrites it only when forced.
    #[test]
    fn format_device_refuses_foreign_signatures_unless_forced() {
        let dir = std::env::temp_dir().join(format!("hs-fmtdev-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc/hyperstor")).unwrap();
        fs::write(dir.join("etc/hyperstor/cluster.txt"), "cluster 42\n").unwrap();
        let disk = dir.join("disk");
        let f = fs::File::create(&disk).unwrap();
        f.set_len(64 << 20).unwrap();
        drop(f);
        let d = disk.to_str().unwrap();
        let done = format_device(&dir, d, 1002, false).unwrap();
        assert_eq!(done, format!("formatted {d} as device 1002 of cluster 42"));
        // Formatted already: refused, naming it.
        let e = format_device(&dir, d, 1003, false).unwrap_err();
        assert!(
            e.contains("hyperstor device (device 1002 of cluster 42)"),
            "{e}"
        );
        // An LVM physical volume: refused; forced, overwritten.
        let mut b = vec![0u8; 8192];
        b[512..520].copy_from_slice(b"LABELONE");
        b[536..544].copy_from_slice(b"LVM2 001");
        fs::write(&disk, &b).unwrap();
        fs::File::options()
            .write(true)
            .open(&disk)
            .unwrap()
            .set_len(64 << 20)
            .unwrap();
        let e = format_device(&dir, d, 1003, false).unwrap_err();
        assert!(
            e.contains("an LVM physical volume") && e.contains("--force-format"),
            "{e}"
        );
        let done = format_device(&dir, d, 1003, true).unwrap();
        assert!(done.ends_with("over an LVM physical volume"), "{done}");
        let head = fs::read(&disk).unwrap();
        assert!(
            !head.windows(8).any(|w| w == b"LABELONE"),
            "the label is gone"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

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

    /// D167: devices added are kept in install.conf and become `--shard`
    /// options after the node's own, used whole (size 0), named by id
    /// when it isn't the position's; block devices by-id get a udev rule
    /// for the service user; install.conf has no `devices` line until
    /// there is one.
    #[test]
    fn added_devices_become_shards_and_udev_rules() {
        let cluster = "cluster 9\nnode 0 rack 0 10.0.0.1:9100 10.0.0.1:9101\n\
                       pool 1 vol replicated size 1 min 1 pgs 8 domain host\n";
        let mut s = Settings::defaults();
        s.role = "storage".into();
        s.node = Some(0);
        s.map = "10.0.0.9:8849".into();
        assert!(!s.save().contains("devices"), "no line until there is one");
        s.devices =
            "2@10.0.0.1:9102=/dev/disk/by-id/nvme-X_1,5@10.0.0.1:9103=/srv/extra.img".into();
        let mut t = Settings::defaults();
        t.apply(&s.save());
        assert_eq!(t.devices, s.devices, "kept across runs");
        let p = plan(Path::new("/nonexistent"), &s, Some(cluster.into()));
        let f = files(&s, &p);
        let get = |name: &str| &f.iter().find(|x| x.0 == name).unwrap().1;
        let env = get("/etc/hyperstor/node-0.env");
        for want in [
            "--shard 10.0.0.1:9100=/var/lib/hyperstor/node-0-0.img:4",
            "--shard 10.0.0.1:9101=/var/lib/hyperstor/node-0-1.img:4",
            "--shard 10.0.0.1:9102=/dev/disk/by-id/nvme-X_1:0",
            "--shard 5@10.0.0.1:9103=/srv/extra.img:0",
        ] {
            assert!(env.contains(want), "{want}: {env}");
        }
        let rules = get(UDEV_RULES);
        assert!(
            rules.contains(
                "ENV{DEVLINKS}==\"*/dev/disk/by-id/nvme-X_1|*/dev/disk/by-id/nvme-X_1 *\""
            ) && rules.contains("OWNER=\"hyperstor\""),
            "{rules}"
        );
        assert!(!rules.contains("extra.img"), "files need no rule");
    }

    /// D285: a node serves the endpoints the map has for its own devices,
    /// after the cluster file's and none twice, and reaches others on
    /// their networks; other nodes' endpoints aren't its.
    #[test]
    fn a_node_serves_its_devices_endpoints_from_the_map() {
        let ep = |net: &str, a: &str| hyperstor_api::Endpoint {
            network: net.into(),
            address: a.into(),
            ..Default::default()
        };
        let dev = |id: u32, eps: Vec<hyperstor_api::Endpoint>| hyperstor_api::Device {
            id,
            endpoints: eps,
            ..Default::default()
        };
        let status = hyperstor_api::GetStatusResponse {
            nodes: vec![
                hyperstor_api::Node {
                    devices: vec![
                        dev(0, vec![ep("fast", "198.51.100.1:9100")]),
                        dev(
                            1,
                            vec![ep("fast", "198.51.100.1:9101"), ep("ib", "192.0.2.1:9101")],
                        ),
                    ],
                    ..Default::default()
                },
                hyperstor_api::Node {
                    devices: vec![dev(1000, vec![ep("fast", "198.51.100.2:9100")])],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let a = |s: &str| s.parse::<std::net::SocketAddr>().unwrap();
        let mut node = NodePlan {
            n: 0,
            shards: vec![a("203.0.113.1:9100"), a("203.0.113.1:9101")],
            ids: vec![0, 1],
            paths: vec![None, None],
            admin: String::new(),
            endpoints: vec![(0, a("198.51.100.1:9100"))],
            networks: vec!["fast".into()],
        };
        serve_mapped_endpoints(&mut node, &status);
        assert_eq!(
            node.endpoints,
            vec![
                (0, a("198.51.100.1:9100")),
                (1, a("198.51.100.1:9101")),
                (1, a("192.0.2.1:9101")),
            ]
        );
        assert_eq!(node.networks, vec!["fast".to_string(), "ib".to_string()]);
    }

    /// I333: udev reads rules files in the order of their names; ours
    /// match by-id links, which `60-persistent-storage.rules` makes, so
    /// they come after it, and the old name came before.
    #[test]
    fn udev_rules_come_after_the_by_id_links() {
        let name = |p: &'static str| p.rsplit('/').next().unwrap();
        assert!(name(UDEV_RULES) > "60-persistent-storage.rules");
        assert!(name(OLD_UDEV_RULES) < "60-persistent-storage.rules");
    }

    /// D207: a node serves the endpoints the cluster file gives its
    /// devices and reaches others on the data networks it has endpoints
    /// on; a node without endpoints gets neither option.
    #[test]
    fn endpoints_and_networks_come_from_the_cluster_file() {
        let cluster = "cluster 9\n\
                       network mgmt control subnet 192.0.2.0/24\n\
                       network ib data trusted ib fe80:0000:0000:0000\n\
                       node 0 rack 0 192.0.2.1:9100 192.0.2.1:9101\n\
                       endpoint 0 ib 198.51.100.1:9100\n\
                       endpoint 1 ib 198.51.100.2:9101\n\
                       node 1 rack 0 192.0.2.2:9100\n\
                       pool 1 vol replicated size 1 min 1 pgs 8 domain host\n";
        let env = |node: u32| {
            let mut s = Settings::defaults();
            s.role = "storage".into();
            s.node = Some(node);
            s.map = "192.0.2.9:8849".into();
            let p = plan(Path::new("/nonexistent"), &s, Some(cluster.into()));
            let f = files(&s, &p);
            f.iter()
                .find(|x| x.0 == format!("/etc/hyperstor/node-{node}.env"))
                .unwrap()
                .1
                .clone()
        };
        let e = env(0);
        assert!(
            e.trim_end().ends_with(
                " --endpoint 0=198.51.100.1:9100 --endpoint 1=198.51.100.2:9101 --networks ib"
            ),
            "{e}"
        );
        let e = env(1);
        assert!(
            !e.contains("--endpoint") && !e.contains("--networks"),
            "{e}"
        );
    }

    /// D186: a removed device has no `--shard`, whether the cluster file
    /// or `devices` named it; the devices after it keep their ids, and
    /// install.conf keeps the list.
    #[test]
    fn removed_devices_get_no_shard() {
        let cluster = "cluster 9\nnode 0 rack 0 10.0.0.1:9100 10.0.0.1:9101 10.0.0.1:9102\n\
                       pool 1 vol replicated size 1 min 1 pgs 8 domain host\n";
        let mut s = Settings::defaults();
        s.role = "storage".into();
        s.node = Some(0);
        s.map = "10.0.0.9:8849".into();
        s.devices = "3@10.0.0.1:9103=/srv/a.img,4@10.0.0.1:9104=/srv/b.img".into();
        assert!(
            !s.save().contains("removed_devices"),
            "no line until there is one"
        );
        s.removed_devices = "1,3".into();
        let mut t = Settings::defaults();
        t.apply(&s.save());
        assert_eq!(t.removed(), [1, 3], "kept across runs");
        let p = plan(Path::new("/nonexistent"), &s, Some(cluster.into()));
        assert_eq!(p.nodes[0].ids, [0, 2, 4]);
        let f = files(&s, &p);
        let env = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/node-0.env")
            .unwrap()
            .1;
        for want in [
            "--shard 10.0.0.1:9100=",
            "--shard 2@10.0.0.1:9102=",
            "--shard 4@10.0.0.1:9104=/srv/b.img:0",
        ] {
            assert!(env.contains(want), "{want}: {env}");
        }
        assert!(!env.contains(":9101") && !env.contains(":9103"), "{env}");
    }

    /// D169: a device id the cluster file names is passed to `--shard`
    /// when it isn't the position's; positional ones are written as
    /// before, so an existing install's options don't change.
    #[test]
    fn named_device_ids_reach_the_node_options() {
        let cluster = "cluster 9\nnode 0 rack 0 10.0.0.1:9100 2@10.0.0.1:9102\n\
                       pool 1 vol replicated size 1 min 1 pgs 8 domain host\n";
        let mut s = Settings::defaults();
        s.role = "storage".into();
        s.node = Some(0);
        s.map = "10.0.0.9:8849".into();
        let p = plan(Path::new("/nonexistent"), &s, Some(cluster.into()));
        let f = files(&s, &p);
        let env = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/node-0.env")
            .unwrap()
            .1;
        assert!(
            env.contains("--shard 10.0.0.1:9100=/var/lib/hyperstor/node-0-0.img:4"),
            "{env}"
        );
        assert!(
            env.contains("--shard 2@10.0.0.1:9102=/var/lib/hyperstor/node-0-1.img:4"),
            "{env}"
        );
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
            [
                "hyperstor-meta.service",
                "hyperstor-mgmt.service",
                "hyperstor-authd.socket"
            ]
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
        assert!(
            !mgmt.contains("proxy") && !mgmt.contains("--hosts"),
            "{mgmt}"
        );
        // Behind a proxy (D277): its options in mgmt.env, kept in
        // install.conf, which says nothing of them until one is set.
        assert!(!s.save().contains("mgmt_hosts"));
        s.mgmt_hosts = "mgmt.example.test".into();
        s.mgmt_proxy_ca = "/etc/hyperstor/proxies.pem".into();
        s.mgmt_trusted_proxies = "127.0.0.1,::1".into();
        s.mgmt_proxy_only = "on".into();
        let f = files(&s, &p);
        let mgmt = &f
            .iter()
            .find(|x| x.0 == "/etc/hyperstor/mgmt.env")
            .unwrap()
            .1;
        assert!(
            mgmt.contains(
                " --hosts mgmt.example.test --proxy-ca /etc/hyperstor/proxies.pem \
                 --trusted-proxies 127.0.0.1,::1 --proxy-only"
            ),
            "{mgmt}"
        );
        let mut back = Settings::defaults();
        back.apply(&s.save());
        assert_eq!(back.mgmt_hosts, s.mgmt_hosts);
        assert_eq!(back.mgmt_proxy_ca, s.mgmt_proxy_ca);
        assert_eq!(back.mgmt_trusted_proxies, s.mgmt_trusted_proxies);
        assert_eq!(back.mgmt_proxy_only, "on");
        assert!(valid_cidr("10.0.0.0/8") && valid_cidr("::1") && valid_cidr("2001:db8::/32"));
        assert!(!valid_cidr("10.0.0.0/33") && !valid_cidr("proxy.example") && !valid_cidr(""));
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
            None,
            None,
            &[],
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
            None,
            None,
            &[],
            &mut changed,
        );
        assert_eq!(cert(root.path()), first, "same addresses: kept");
        let restart = pki(
            root.path(),
            false,
            "9",
            "0",
            &ips(&["10.0.0.13"]),
            None,
            None,
            &[],
            &mut changed,
        );
        assert_ne!(cert(root.path()), first, "new addresses: issued again");
        assert!(restart.contains(&"hyperstor-meta.service".to_string()));
    }

    /// mgmt's web certificate (D108): for every name and address it's
    /// given, from the cluster CA, kept while they stay, issued again when
    /// they change; mgmt re-reads it, so nothing restarts.
    #[test]
    fn the_web_certificate_names_the_host() {
        let root = tempfile::tempdir().unwrap();
        let names = |dns: &[&str], ips: &[&str]| glidex_tls::LocalNames {
            dns: dns.iter().map(|d| d.to_string()).collect(),
            ips: ips.iter().map(|i| i.parse().unwrap()).collect(),
        };
        let web = |r: &Path| fs::read_to_string(r.join("etc/hyperstor/pki/web.crt")).unwrap();
        let mut changed = Vec::new();
        let a = names(&["box.example", "localhost"], &["127.0.0.1", "10.0.0.5"]);
        pki(
            root.path(),
            false,
            "9",
            "0",
            &[],
            Some(&a),
            None,
            &[],
            &mut changed,
        );
        let first = web(root.path());
        let (ips, dns) = hyperstor_pki::names_of(&first).unwrap();
        // No certificate data in the failure message.
        assert!(dns.contains(&"box.example".to_string()), "no box.example");
        assert!(ips.contains(&"10.0.0.5".parse().unwrap()), "no 10.0.0.5");
        let root_pem = fs::read_to_string(root.path().join("etc/hyperstor/pki/root.crt")).unwrap();
        assert!(
            !first.contains(root_pem.trim()),
            "the root isn't in the chain"
        );
        pki(
            root.path(),
            false,
            "9",
            "0",
            &[],
            Some(&a),
            None,
            &[],
            &mut changed,
        );
        assert_eq!(web(root.path()), first, "same names: kept");
        let b = names(&["box.example", "localhost"], &["127.0.0.1", "10.0.0.6"]);
        let restart = pki(
            root.path(),
            false,
            "9",
            "0",
            &[],
            Some(&b),
            None,
            &[],
            &mut changed,
        );
        assert_ne!(web(root.path()), first, "a new address: issued again");
        assert!(!restart.iter().any(|r| r.contains("mgmt")), "{restart:?}");
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

    /// `--vhost` adds hyperstor-vhost to a role (D142), for the users
    /// named (D144); `--role vhost` is a host with only it.
    #[test]
    fn vhost_is_installed_for_its_users() {
        let mut s = Settings::defaults();
        s.all_in_one = true;
        assert!(plan(Path::new("/nonexistent"), &s, None).vhost.is_none());
        assert!(
            !files(&s, &plan(Path::new("/nonexistent"), &s, None))
                .iter()
                .any(|f| f.0.contains("vhost"))
        );
        s.vhost = true;
        s.vhost_users = "root".into();
        let p = plan(Path::new("/nonexistent"), &s, None);
        assert_eq!(
            p.vhost,
            Some(VhostPlan {
                uids: vec![0],
                gids: vec![0]
            })
        );
        assert!(units(&p).contains(&"hyperstor-vhost.service".to_string()));
        let f = files(&s, &p);
        let file = |n: &str| f.iter().find(|x| x.0 == n).unwrap().1.clone();
        assert_eq!(
            file("/etc/hyperstor/vhost.env"),
            "HS_ARGS=--meta 127.0.0.1:8849 --dir /run/hyperstor/vhost --allow-uid 0 --group 0\n"
        );
        let unit = file("/etc/systemd/system/hyperstor-vhost.service");
        assert!(unit.contains("\nSupplementaryGroups=0\n"), "{unit}");
        assert!(unit.contains("\nRuntimeDirectoryPreserve=yes\n"), "{unit}");
        // Only VMs here: the map service's members, nothing of the cluster's.
        let mut v = Settings::defaults();
        v.role = "vhost".into();
        v.map = "10.0.0.5:8849,10.0.0.6:8849".into();
        let p = plan(Path::new("/nonexistent"), &v, None);
        assert_eq!(units(&p), vec!["hyperstor-vhost.service".to_string()]);
        let f = files(&v, &p);
        let env = &f.iter().find(|x| x.0.ends_with("vhost.env")).unwrap().1;
        assert!(
            env.starts_with("HS_ARGS=--meta 10.0.0.5:8849,10.0.0.6:8849 "),
            "{env}"
        );
        assert!(!env.contains("--allow-uid"), "{env}");
        let unit = &f.iter().find(|x| x.0.ends_with("vhost.service")).unwrap().1;
        assert!(!unit.contains("SupplementaryGroups="), "{unit}");
        assert!(
            !f.iter()
                .any(|x| x.0.contains("cluster.txt") || x.0.contains("node-"))
        );
        // Saved and read back.
        let mut t = Settings::defaults();
        t.apply(&s.save());
        assert!(t.vhost && t.vhost_users == "root");
        assert!(user_name_ok("glidex") && !user_name_ok("Root") && !user_name_ok("a b"));
    }

    /// D289: this host's clients reach storage on the networks its storage
    /// nodes use (`auto`), on none, or on the ones named; kept across runs.
    #[test]
    fn clients_take_the_networks_of_the_hosts_storage() {
        let mut s = Settings::defaults();
        s.all_in_one = true;
        s.vhost = true;
        let mut p = plan(Path::new("/nonexistent"), &s, None);
        let env = |s: &Settings, p: &Plan| {
            files(s, p)
                .into_iter()
                .find(|x| x.0.ends_with("vhost.env"))
                .unwrap()
                .1
        };
        assert!(!env(&s, &p).contains("--networks"), "no networks yet");
        p.nodes[0].networks = vec!["fast".into(), "ib".into()];
        p.nodes[1].networks = vec!["ib".into()];
        assert!(
            env(&s, &p).ends_with(" --networks fast,ib\n"),
            "{}",
            env(&s, &p)
        );
        s.client_networks = "none".into();
        assert!(!env(&s, &p).contains("--networks"));
        s.client_networks = "ib".into();
        assert!(env(&s, &p).ends_with(" --networks ib\n"));
        let mut t = Settings::defaults();
        t.apply(&s.save());
        assert_eq!(t.client_networks, "ib");
        assert!(!Settings::defaults().save().contains("client_networks"));
    }

    #[test]
    fn the_volumes_hyperstor_vhost_lists_are_read() {
        let a = "0123456789abcdef0123456789abcdef";
        let answer = format!(
            r#"{{"id":1,"ok":{{"disks":[{{"volume":"{a}","socket":"/x","size":1,"connected":true}},{{"volume":"zz"}}]}}}}"#
        );
        assert_eq!(listed_volumes(&answer), vec![a.to_string()]);
        assert!(listed_volumes("").is_empty());
    }
}
