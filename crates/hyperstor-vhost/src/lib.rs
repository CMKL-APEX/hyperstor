//! `hyperstor-vhost` serves the volumes attached on this node to its VMs
//! over vhost-user-blk (11-frontends.md §11.1, 04-decisions.md D1, D26, D116).
//!
//! The first reactor runs a client of the cluster, a control socket
//! (D117) through which glidex attaches and detaches volumes, and a
//! vhost-user socket per attached volume, `<dir>/<volume id>.sock`, which
//! the hypervisor connects to (and reconnects to after either side
//! restarts). There is a reactor per NUMA node, each pinned to one CPU
//! (D290, [`reactors`]); a disk runs whole on one of them, that of the
//! node its guest's memory is bound to (D291, D292): its connection and
//! its lease move there when the hypervisor's memory table comes. Each
//! queue of a connected disk is a task on that reactor.
//!
//! Stopping the service leaves its volumes attached (04-decisions.md D143):
//! `<dir>/state.json` names them and the instance that held them, and the
//! next start takes them back from that instance, which the hypervisors
//! then reconnect to. A volume another client took meanwhile is dropped.

pub mod blk;
pub mod budget;
pub mod control;
pub mod device;
pub mod inflight;
pub mod numa;
pub mod reactors;

use control::{Reply, Request, Take};
use device::Disk;
use hyperstor_client::{Client, ClientError};
use hyperstor_reactor::fd;
use reactors::Reactors;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub struct Options {
    /// The map service's members.
    pub meta: Vec<SocketAddr>,
    /// Where the control and disk sockets go.
    pub dir: PathBuf,
    /// Users allowed on the control socket and the disks' sockets besides
    /// root and our own.
    pub allow_uids: Vec<u32>,
    /// The group of the volumes a state file from before D355 names no
    /// user for: they go to the first allowed user and this group, as
    /// D144 gave every socket.
    pub group: Option<u32>,
    /// Queues each disk offers.
    pub queues: u16,
    /// How long a write refused for space is held before the guest sees
    /// an I/O error (04-decisions.md D183; 0: at once).
    pub hold_full: std::time::Duration,
    /// How long a request is held while the cluster can't serve it (no
    /// primary, no map service, nodes unreachable) before the guest sees
    /// an I/O error (D354; 0: the client's own tries only).
    pub hold_outage: std::time::Duration,
    /// The data networks it reaches storage nodes on, in order
    /// (26-networks.md §29.5, D207); none: devices' addresses.
    pub networks: Vec<String>,
    /// The host it names to storage nodes, for realm bindings (`--host`,
    /// D259, D261); empty: none.
    pub host: String,
    /// The reactors' CPUs, one reactor each (`--cpus`, D290); None: the
    /// first CPU of each NUMA node with CPUs.
    pub cpus: Option<Vec<usize>>,
    /// How long a polled disk's queues stay polled after they were last
    /// busy (`--poll-us`, D293).
    pub poll: std::time::Duration,
}

/// The reactors' CPUs and nodes: `cpus` as given, or the first CPU of
/// each node; one reactor, not pinned, on a host that lists no nodes.
pub fn reactor_cpus(cpus: Option<&[usize]>, sys: &Path) -> Vec<(Option<usize>, Option<u32>)> {
    let nodes = numa::nodes(sys);
    let cpus = cpus.map_or_else(|| numa::default_cpus(&nodes), <[usize]>::to_vec);
    if cpus.is_empty() {
        return vec![(None, None)];
    }
    cpus.iter()
        .map(|&c| (Some(c), numa::node_of(&nodes, c)))
        .collect()
}

/// The control socket's path in `dir`.
pub fn control_path(dir: &Path) -> PathBuf {
    dir.join("control.sock")
}

/// The state file's path in `dir`.
pub fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

/// Who a volume's socket is for (D355): the user who attached it, or the
/// one root named with `for`. Its socket has that user's group; `gid`
/// None keeps the service's own (root's volumes, and the service user's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: Option<u32>,
}

/// What survives a restart (D143): the instance that held the volumes,
/// and each volume with its serial and lease.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct State {
    pub instance: u64,
    /// Each disk: its volume, serial, the identity of its lease (D150; 0
    /// in a file written before identities, whose leases have the
    /// instance as their identity), its label (D153), whether its
    /// rings are polled (D293) and who it is for (D355; None in a file
    /// written before).
    pub disks: Vec<(u128, String, u64, String, bool, Option<Owner>)>,
}

impl State {
    pub fn to_json(&self) -> String {
        let disks: Vec<serde_json::Value> = self
            .disks
            .iter()
            .map(|(v, s, l, label, poll, owner)| {
                let mut d = serde_json::json!({
                    "volume": format!("{v:032x}"),
                    "serial": s,
                    "lease": format!("{l:016x}"),
                    "label": label,
                    "poll": poll,
                });
                if let Some(o) = owner {
                    d["uid"] = o.uid.into();
                    d["gid"] = o.gid.into();
                }
                d
            })
            .collect();
        serde_json::json!({"instance": format!("{:016x}", self.instance), "disks": disks})
            .to_string()
    }

    pub fn parse(text: &str) -> Result<State, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let hex = |v: &serde_json::Value, what: &str| {
            v.as_str()
                .ok_or_else(|| format!("no {what}"))
                .map(str::to_string)
        };
        let instance = u64::from_str_radix(&hex(&v["instance"], "instance")?, 16)
            .map_err(|e| format!("instance: {e}"))?;
        let mut disks = Vec::new();
        for d in v["disks"].as_array().ok_or("no disks")? {
            let volume = u128::from_str_radix(&hex(&d["volume"], "volume")?, 16)
                .map_err(|e| format!("volume: {e}"))?;
            let lease = match d["lease"].as_str() {
                Some(l) => u64::from_str_radix(l, 16).map_err(|e| format!("lease: {e}"))?,
                None => 0,
            };
            let label = d["label"].as_str().unwrap_or("vhost").to_string();
            let poll = d["poll"].as_bool().unwrap_or(false);
            let id = |what: &str| match &d[what] {
                serde_json::Value::Null => Ok(None),
                v => v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .map(Some)
                    .ok_or_else(|| format!("{what}: {v}")),
            };
            let owner = match (id("uid")?, id("gid")?) {
                (Some(uid), gid) => Some(Owner { uid, gid }),
                (None, _) => None,
            };
            disks.push((
                volume,
                hex(&d["serial"], "serial")?,
                lease,
                label,
                poll,
                owner,
            ));
        }
        Ok(State { instance, disks })
    }
}

/// A disk socket's path in `dir`.
pub fn disk_path(dir: &Path, volume: u128) -> PathBuf {
    dir.join(format!("{volume:032x}.sock"))
}

struct Served {
    /// The reactor that holds the disk and its lease (D291).
    home: Rc<Cell<usize>>,
    serial: String,
    /// Its lease's identity and label, for the state file (D150, D153).
    lease: u64,
    label: String,
    /// Its rings are polled while busy (D293).
    poll: bool,
    /// Who its socket is for (D355).
    owner: Owner,
    /// Attached as a delegate waiting for promotion, a live migration's
    /// target (D350): not in the state file, since it holds no lease.
    delegate: Rc<Cell<bool>>,
    socket: PathBuf,
    listener: UnixListener,
    /// The connected frontend, if any, to cut off at detach.
    conn: Rc<RefCell<Option<UnixStream>>>,
    stopping: Rc<Cell<bool>>,
    task: hyperstor_reactor::JoinHandle<()>,
}

/// Why an `attach` was refused: held (by whom, D153), or another code.
enum Refused {
    Held { message: String, holder: String },
    Other(&'static str, String),
}

impl From<(&'static str, String)> for Refused {
    fn from((code, msg): (&'static str, String)) -> Self {
        Refused::Other(code, msg)
    }
}

struct Service {
    opts: Options,
    client: Rc<Client>,
    disks: RefCell<BTreeMap<u128, Served>>,
    reactors: Reactors,
    /// Connections each reactor serves, for placing the next (D292).
    load: RefCell<Vec<usize>>,
}

/// A disk on the reactor this runs on, around its attached volume.
fn new_disk(
    volume: Rc<hyperstor_client::volume::Attached>,
    serial: String,
    queues: u16,
    (hold, hold_outage): (std::time::Duration, std::time::Duration),
    poll: bool,
) -> Rc<Disk> {
    Rc::new(Disk {
        volume,
        serial,
        queues,
        poll,
        locks: Default::default(),
        hold,
        hold_outage,
        held: Default::default(),
        stalled: Default::default(),
    })
}

/// Runs the service on this thread until `stop` receives (or is dropped),
/// then detaches every volume. Calls `ready` once the control socket
/// listens.
pub fn serve(
    opts: Options,
    ready: impl FnOnce(&Path) + 'static,
    stop: std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    // A volume's socket must fit a Unix socket's path (107 bytes).
    device::set_poll_window(opts.poll);
    let longest = disk_path(&opts.dir, u128::MAX);
    if longest.as_os_str().len() > 107 {
        return Err(format!(
            "{}: too long for the volumes' sockets (a socket's path is at most 107 bytes)",
            opts.dir.display()
        ));
    }
    std::fs::create_dir_all(&opts.dir).map_err(|e| format!("{}: {e}", opts.dir.display()))?;
    // Anyone may pass through to a socket they know the name of; only
    // this service lists them (D355). Never given to another group:
    // systemd would then reset what is in it at the next start (G60).
    std::fs::set_permissions(&opts.dir, std::fs::Permissions::from_mode(0o711))
        .map_err(|e| format!("{}: {e}", opts.dir.display()))?;
    let previous = match std::fs::read_to_string(state_path(&opts.dir)) {
        Ok(t) => State::parse(&t)
            .map_err(|e| {
                eprintln!(
                    "hyperstor-vhost: {}: {e}; ignored",
                    state_path(&opts.dir).display()
                )
            })
            .ok(),
        Err(_) => None,
    };
    let path = control_path(&opts.dir);
    // Every allowed user reaches it; they share no group, so its uid
    // check is the gate (D355).
    let listener = bind_shared(&path, 0o666, None)?;
    let (tx, mut rx) = hyperstor_reactor::chan::channel::<()>();
    std::thread::spawn(move || {
        let _ = stop.recv();
        let _ = tx.send(());
    });
    let cpus = reactor_cpus(opts.cpus.as_deref(), Path::new(numa::SYS_NODES));
    hyperstor_reactor::block_on(async move {
        let id = instance_id();
        let client = Client::connect_members(opts.meta.clone(), id)
            .await
            .map_err(|e| format!("map service: {e}"))?;
        client.set_networks(opts.networks.clone());
        client.set_host(&opts.host);
        let spec = reactors::ClientSpec {
            meta: opts.meta.clone(),
            id,
            instance: client.instance,
            networks: opts.networks.clone(),
            host: opts.host.clone(),
        };
        let (reactors, jobs) = Reactors::start(&cpus, spec, client.clone());
        let _jobs = hyperstor_reactor::spawn(jobs);
        let listed: Vec<String> = reactors
            .all
            .iter()
            .map(|r| match (r.cpu, r.node) {
                (Some(c), Some(n)) => format!("CPU {c} (node {n})"),
                (Some(c), None) => format!("CPU {c}"),
                _ => "not pinned".into(),
            })
            .collect();
        eprintln!("hyperstor-vhost: reactors on {}", listed.join(", "));
        let n = reactors.all.len();
        let svc = Rc::new(Service {
            opts,
            client,
            disks: RefCell::new(BTreeMap::new()),
            reactors,
            load: RefCell::new(vec![0; n]),
        });
        // Before glidex can ask: the volumes the last instance had.
        if let Some(prev) = previous {
            // A file from before D355: D144's owner, the first allowed user
            // and the sockets' group.
            let before = Owner {
                // SAFETY: getuid has no preconditions.
                uid: (svc.opts.allow_uids.first().copied()).unwrap_or(unsafe { libc::getuid() }),
                gid: svc.opts.group,
            };
            for (volume, serial, lease, label, poll, owner) in prev.disks {
                let old = if lease == 0 { prev.instance } else { lease };
                let owner = owner.unwrap_or(before);
                match svc.reclaim(volume, serial, old, &label, poll, owner).await {
                    Ok(()) => eprintln!("hyperstor-vhost: volume {volume:032x}: taken back"),
                    Err(e) => eprintln!("hyperstor-vhost: volume {volume:032x}: dropped: {e}"),
                }
            }
        }
        svc.save_state();
        ready(&path);
        let _accept = hyperstor_reactor::spawn(accept_control(svc.clone(), listener));
        let _ = rx.recv().await;
        // The volumes stay attached for the next start (D143): their
        // leases lapse with this process, and the state file names them.
        let _ = std::fs::remove_file(&path);
        Ok(())
    })
}

/// This instance's id in the cluster (its leases and blocklist entry).
fn instance_id() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u64) ^ ((std::process::id() as u64) << 40)
}

/// Binds a socket at `path` that `group` can open from the moment it is
/// there: bound under a temporary name, given `mode` and the group, then
/// renamed over `path`. A hypervisor reconnecting after a restart finds the
/// old socket (refused) or the new one, never one it can't open: Cloud
/// Hypervisor gives up on EACCES and the guest loses its disk (glidex #584).
fn bind_shared(path: &Path, mode: u32, group: Option<u32>) -> Result<UnixListener, String> {
    let tmp = path.with_extension("part");
    let _ = std::fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let shared = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{}: {e}", tmp.display()))
        .and_then(|()| group.map_or(Ok(()), |g| chown_group(&tmp, g)))
        .and_then(|()| std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display())));
    if let Err(e) = shared {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(listener)
}

fn chown_group(path: &Path, gid: u32) -> Result<(), String> {
    std::os::unix::fs::chown(path, None, Some(gid)).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `path` through `<path>.new`, created with mode 0600: the file is
/// never readable by the sockets' group, even while it is written.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("json.new");
    // One left by a crash keeps its mode through open; start afresh.
    let _ = std::fs::remove_file(&tmp);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path))
}

/// Whether `peer` may use the control socket (`allowed`: the users of
/// `--allow-uid`) or a disk's (`allowed`: the one it is for, D355): root,
/// the service's own user, or one of `allowed`. A socket's group alone
/// isn't enough: anyone in it could otherwise open the disk.
fn may_use(peer: Option<u32>, own: u32, allowed: &[u32]) -> bool {
    peer.is_some_and(|u| u == 0 || u == own || allowed.contains(&u))
}

/// Who a volume attached by `peer` (uid, gid) is for (D355): `peer`, or
/// the user root names with `for`, one of `allowed`. Root's volumes and
/// the service user's keep the service's group.
fn owner_for(
    peer: (u32, u32),
    own: u32,
    allowed: &[u32],
    named: Option<(u32, u32)>,
) -> Result<Owner, (&'static str, String)> {
    match named {
        Some(_) if peer.0 != 0 => Err(("denied", "only root attaches for another user".into())),
        Some((uid, _)) if !allowed.contains(&uid) => Err((
            "invalid",
            format!("uid {uid} may not use hyperstor-vhost (--allow-uid)"),
        )),
        Some((uid, gid)) => Ok(Owner {
            uid,
            gid: Some(gid),
        }),
        None if peer.0 == 0 || peer.0 == own => Ok(Owner {
            uid: peer.0,
            gid: None,
        }),
        None => Ok(Owner {
            uid: peer.0,
            gid: Some(peer.1),
        }),
    }
}

/// Why a volume attached here for one user isn't answered to another.
fn for_another(volume: u128) -> String {
    format!("volume {volume:032x} is attached here for another user")
}

/// The uid and gid at the other end of a Unix socket.
fn peer_cred(s: &UnixStream) -> Option<(u32, u32)> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, a local
    // of that size.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some((cred.uid, cred.gid))
}

async fn accept_control(svc: Rc<Service>, listener: UnixListener) {
    // SAFETY: getuid has no preconditions.
    let own = unsafe { libc::getuid() };
    loop {
        if fd::readable(listener.as_raw_fd()).await.is_err() {
            return;
        }
        let Ok((mut s, _)) = listener.accept() else {
            continue;
        };
        let peer = peer_cred(&s);
        let Some(peer) = peer.filter(|p| may_use(Some(p.0), own, &svc.opts.allow_uids)) else {
            let r = Reply::error(0, "denied", "this user may not use hyperstor-vhost");
            let _ = writeln!(s, "{}", r.line());
            continue;
        };
        hyperstor_reactor::spawn(control_conn(svc.clone(), s, peer));
    }
}

/// One control connection from `peer` (uid, gid): a request per line, a
/// reply per line.
async fn control_conn(svc: Rc<Service>, mut s: UnixStream, peer: (u32, u32)) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if fd::readable(s.as_raw_fd()).await.is_err() {
            return;
        }
        let n = match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 << 10 {
            return;
        }
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let reply = match control::parse(&line[..line.len() - 1]) {
                Ok((id, req)) => svc.handle(id, req, peer).await,
                Err(e) => Reply::error(0, "invalid", &e),
            };
            if writeln!(s, "{}", reply.line()).is_err() {
                return;
            }
        }
    }
}

impl Service {
    async fn handle(self: &Rc<Self>, id: u64, req: Request, peer: (u32, u32)) -> Reply {
        match req {
            Request::Hello => Reply::ok(id, control::hello()),
            Request::List => Reply::ok(id, control::list(self.list().await)),
            Request::Attach {
                volume,
                serial,
                take,
                holder,
                poll,
                delegate,
                user,
            } => {
                // SAFETY: getuid has no preconditions.
                let own = unsafe { libc::getuid() };
                let owner = match owner_for(peer, own, &self.opts.allow_uids, user) {
                    Ok(o) => o,
                    Err((code, msg)) => return Reply::error(id, code, &msg),
                };
                let label = if holder.is_empty() {
                    "vhost".to_string()
                } else {
                    format!("vhost:{holder}")
                };
                let attached = match delegate {
                    Some(d) => {
                        self.attach_delegate(volume, serial, d, &label, poll, owner)
                            .await
                    }
                    None => self.attach(volume, serial, take, &label, poll, owner).await,
                };
                match attached {
                    Ok(v) => Reply::ok(id, v),
                    Err(Refused::Held { message, holder }) => Reply::held(id, &message, &holder),
                    Err(Refused::Other(code, msg)) => Reply::error(id, code, &msg),
                }
            }
            Request::Detach { volume, force } => match self.detach(volume, force).await {
                Ok(()) => Reply::ok(id, control::detached()),
                Err((code, msg)) => Reply::error(id, code, &msg),
            },
            Request::Delegate { volume } => match self.delegate(volume, None).await {
                Ok(d) => Reply::ok(id, control::delegated(d)),
                Err((code, msg)) => Reply::error(id, code, &msg),
            },
            Request::Undelegate { volume, delegate } => {
                match self.delegate(volume, Some(delegate)).await {
                    Ok(_) => Reply::ok(id, control::detached()),
                    Err((code, msg)) => Reply::error(id, code, &msg),
                }
            }
            Request::Promote { volume } => match self.promote(volume).await {
                Ok(fenced) => Reply::ok(id, control::promoted(&fenced)),
                Err((code, msg)) => Reply::error(id, code, &msg),
            },
        }
    }

    /// On the source of a migration (D350): adds a delegate to the lease
    /// of `volume`, attached here as its holder, and answers it; with
    /// `remove`, removes and fences that delegate instead.
    async fn delegate(
        &self,
        volume: u128,
        remove: Option<u64>,
    ) -> Result<u64, (&'static str, String)> {
        let Some(home) = self.disks.borrow().get(&volume).map(|d| d.home.get()) else {
            return Err((
                "not_attached",
                format!("volume {volume:032x} isn't attached here"),
            ));
        };
        self.reactors
            .run_on(home, move || async move {
                let Some(disk) = device::local_disk(volume) else {
                    return Err((
                        "not_attached",
                        format!("volume {volume:032x} isn't attached here"),
                    ));
                };
                let v = &disk.volume;
                if v.is_delegate() {
                    return Err((
                        "invalid",
                        format!(
                            "volume {volume:032x} is attached here as a delegate, not its holder"
                        ),
                    ));
                }
                let r = match remove {
                    Some(d) => v.remove_delegate(d).await.map(|()| d),
                    None => v.add_delegate().await,
                };
                r.map_err(|e| (control::error_code(&e), e.to_string()))
            })
            .await
            .unwrap_or_else(|| Err(("internal", "its reactor stopped".into())))
    }

    /// On a migration's target (D350): attaches `volume` as delegate
    /// `delegate` of its holder, standing by: its socket takes the
    /// receiving hypervisor, and it writes nothing until promoted. Checked
    /// first with the header's storage node, so a cluster that can't
    /// promote refuses before the copy starts (D352).
    async fn attach_delegate(
        self: &Rc<Self>,
        volume: u128,
        serial: String,
        delegate: u64,
        label: &str,
        poll: bool,
        owner: Owner,
    ) -> Result<control::Value, Refused> {
        let found = self
            .disks
            .borrow()
            .get(&volume)
            .map(|d| (d.socket.clone(), d.lease, d.delegate.get(), d.owner));
        if let Some((socket, lease, standing, was)) = found {
            if was != owner {
                return Err(Refused::Other("denied", for_another(volume)));
            }
            match self.status(volume).await {
                // A retry.
                Some((false, size)) if standing && lease == delegate => {
                    return Ok(control::attached(&socket, size));
                }
                Some((false, _)) => {
                    return Err(Refused::Other(
                        "invalid",
                        format!("volume {volume:032x} is attached here already"),
                    ));
                }
                _ => self.detach(volume, true).await.map_err(Refused::from)?,
            }
        }
        let fail = |e: ClientError| Refused::Other(control::error_code(&e), e.to_string());
        let att = self.client.delegate(volume, delegate).await.map_err(fail)?;
        att.set_standby().map_err(fail)?;
        *att.label_mut() = hyperstor_client::cut_label(label);
        if let Err(e) = att.check_promote().await {
            att.detach().await.ok();
            return Err(fail(e));
        }
        self.open_disk(volume, serial, att, poll, owner)
            .await
            .map_err(Refused::from)
    }

    /// On a migration's target, at the switch (D351): promotes the
    /// delegate attached here to the volume's holder. Answers the
    /// identities being fenced in the background.
    async fn promote(&self, volume: u128) -> Result<Vec<u64>, (&'static str, String)> {
        let Some((home, delegate)) = self
            .disks
            .borrow()
            .get(&volume)
            .map(|d| (d.home.get(), d.delegate.clone()))
        else {
            return Err((
                "not_attached",
                format!("volume {volume:032x} isn't attached here"),
            ));
        };
        if !delegate.get() {
            // Promoted already: a retry.
            return Ok(Vec::new());
        }
        let fenced = self
            .reactors
            .run_on(home, move || async move {
                let Some(disk) = device::local_disk(volume) else {
                    return Err((
                        "not_attached",
                        format!("volume {volume:032x} isn't attached here"),
                    ));
                };
                let label = disk.volume.label();
                disk.volume
                    .promote(&label)
                    .await
                    .map_err(|e| (control::error_code(&e), e.to_string()))
            })
            .await
            .unwrap_or_else(|| Err(("internal", "its reactor stopped".into())))?;
        delegate.set(false);
        self.save_state();
        Ok(fenced)
    }

    async fn list(&self) -> Vec<control::Listed> {
        let disks: Vec<(u128, PathBuf, bool, usize, bool, bool)> = self
            .disks
            .borrow()
            .iter()
            .map(|(id, d)| {
                (
                    *id,
                    d.socket.clone(),
                    d.conn.borrow().is_some(),
                    d.home.get(),
                    d.poll,
                    d.delegate.get(),
                )
            })
            .collect();
        let mut out = Vec::new();
        for (volume, socket, connected, home, poll, delegate) in disks {
            let (lost, size) = self.status(volume).await.unwrap_or((true, 0));
            out.push(control::Listed {
                volume,
                socket,
                size,
                connected,
                lost,
                reactor: home,
                node: self.reactors.all[home].node,
                poll,
                delegate,
            });
        }
        out
    }

    /// Whether `volume`'s lease is lost, and its size, as the reactor
    /// holding it knows; None if it isn't attached.
    async fn status(&self, volume: u128) -> Option<(bool, u64)> {
        let home = self.disks.borrow().get(&volume)?.home.get();
        self.reactors
            .run_on(home, move || async move {
                device::local_disk(volume).map(|d| (d.volume.lost(), d.volume.volume().size))
            })
            .await
            .flatten()
    }

    /// Writes the state file (D143): this instance and its volumes. A
    /// failure is reported, not fatal: only a restart needs the file.
    fn save_state(&self) {
        let state = State {
            instance: self.client.instance,
            disks: self
                .disks
                .borrow()
                .iter()
                // A delegate holds no lease to take back (D350).
                .filter(|(_, d)| !d.delegate.get())
                .map(|(v, d)| {
                    let (serial, label) = (d.serial.clone(), d.label.clone());
                    (*v, serial, d.lease, label, d.poll, Some(d.owner))
                })
                .collect(),
        };
        let path = state_path(&self.opts.dir);
        if let Err(e) = write_private(&path, &state.to_json()) {
            eprintln!("hyperstor-vhost: {}: {e}", path.display());
        }
    }

    /// Takes `volume` back after a restart: from no one, or from `old`,
    /// the lease this service held it under before (it lapses within
    /// seconds of its exit; taking it fences only that lease, D150). Held
    /// by anyone else, it is left to them.
    async fn reclaim(
        self: &Rc<Self>,
        volume: u128,
        serial: String,
        old: u64,
        label: &str,
        poll: bool,
        owner: Owner,
    ) -> Result<(), String> {
        use hyperstor_client::volume::Take;
        let att = match self.client.attach_as(volume, Take::Attach, label).await {
            Ok(a) => a,
            Err(ClientError::LeaseHeld { holder, .. }) if holder == old => self
                .client
                .attach_as(volume, Take::TakeOver, label)
                .await
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(e.to_string()),
        };
        self.open_disk(volume, serial, att, poll, owner)
            .await
            .map(|_| ())
            .map_err(|(_, e)| e)
    }

    /// Attaches `volume` and opens its socket; for a volume attached here
    /// already, answers with its socket (a retry, or glidex restarting).
    /// One whose lease was taken elsewhere (D150) is let go first and
    /// attached afresh.
    async fn attach(
        self: &Rc<Self>,
        volume: u128,
        serial: String,
        take: Take,
        label: &str,
        poll: bool,
        owner: Owner,
    ) -> Result<control::Value, Refused> {
        let found = self
            .disks
            .borrow()
            .get(&volume)
            .map(|d| (d.socket.clone(), d.owner));
        if let Some((socket, was)) = found {
            if was != owner {
                return Err(Refused::Other("denied", for_another(volume)));
            }
            match self.status(volume).await {
                Some((false, size)) => return Ok(control::attached(&socket, size)),
                // Lost, or its reactor gone: let go and attach afresh.
                _ => self.detach(volume, true).await.map_err(Refused::from)?,
            }
        }
        let how = match take {
            Take::Attach => hyperstor_client::volume::Take::Attach,
            Take::TakeOver => hyperstor_client::volume::Take::TakeOver,
            Take::Preempt => hyperstor_client::volume::Take::Preempt,
        };
        let att = self
            .client
            .attach_as(volume, how, label)
            .await
            .map_err(|e| match &e {
                ClientError::LeaseHeld { label, .. } => Refused::Held {
                    holder: label.clone(),
                    message: e.to_string(),
                },
                _ => Refused::Other(control::error_code(&e), e.to_string()),
            })?;
        self.open_disk(volume, serial, att, poll, owner)
            .await
            .map_err(Refused::from)
    }

    /// Opens the socket of `volume`, attached as `att`, and serves it.
    async fn open_disk(
        self: &Rc<Self>,
        volume: u128,
        serial: String,
        att: Rc<hyperstor_client::volume::Attached>,
        poll: bool,
        owner: Owner,
    ) -> Result<control::Value, (&'static str, String)> {
        // Another request may have attached it meanwhile.
        if let Some(d) = self.disks.borrow().get(&volume) {
            return Ok(control::attached(&d.socket, att.volume().size));
        }
        let socket = disk_path(&self.opts.dir, volume);
        let opened = bind_shared(&socket, 0o660, owner.gid).and_then(|l| {
            let accepting = l
                .try_clone()
                .map_err(|e| format!("{}: {e}", socket.display()))?;
            Ok((accepting, l))
        });
        let (listener, accepting) = match opened {
            Ok(l) => l,
            Err(e) => {
                let _ = att.detach().await;
                let _ = std::fs::remove_file(&socket);
                return Err(("internal", e));
            }
        };
        let size = att.volume().size;
        let (lease, label) = (att.lease(), att.label());
        let delegate = Rc::new(Cell::new(att.is_delegate()));
        // Until a hypervisor connects, on this, the first, reactor (D292).
        device::hold_disk(new_disk(
            att,
            serial.clone(),
            self.opts.queues,
            (self.opts.hold_full, self.opts.hold_outage),
            poll,
        ));
        let home = Rc::new(Cell::new(0));
        let conn = Rc::new(RefCell::new(None));
        let stopping = Rc::new(Cell::new(false));
        let task = hyperstor_reactor::spawn(serve_disk(
            self.clone(),
            volume,
            home.clone(),
            accepting,
            owner.uid,
            conn.clone(),
            stopping.clone(),
        ));
        self.disks.borrow_mut().insert(
            volume,
            Served {
                home,
                serial,
                lease,
                label,
                poll,
                owner,
                delegate,
                socket: socket.clone(),
                listener,
                conn,
                stopping,
                task,
            },
        );
        self.save_state();
        Ok(control::attached(&socket, size))
    }

    /// Closes the volume's socket, cuts off its frontend, and detaches it.
    /// Refused (`busy`) while a hypervisor is connected, unless `force` or
    /// the volume was taken elsewhere (D256): a client's mistake must not
    /// pull a running VM's disk. One that hangs up within [`HANGUP_GRACE`]
    /// is let go.
    async fn detach(&self, volume: u128, force: bool) -> Result<(), (&'static str, String)> {
        if !force && self.still_connected(volume).await {
            return Err((
                "busy",
                format!(
                    "volume {volume:032x}: a hypervisor is connected; stop it first, or detach with force"
                ),
            ));
        }
        let Some(d) = self.disks.borrow_mut().remove(&volume) else {
            return Err((
                "not_attached",
                format!("volume {volume:032x} isn't attached here"),
            ));
        };
        d.stopping.set(true);
        // SAFETY: shutdown on sockets this service owns; it wakes the
        // accept and the connection's read, which then end.
        unsafe {
            libc::shutdown(d.listener.as_raw_fd(), libc::SHUT_RDWR);
            if let Some(c) = d.conn.borrow().as_ref() {
                libc::shutdown(c.as_raw_fd(), libc::SHUT_RDWR);
            }
        }
        d.task.await;
        let _ = std::fs::remove_file(&d.socket);
        self.save_state();
        self.reactors
            .run_on(d.home.get(), move || async move {
                match device::drop_disk(volume) {
                    Some(disk) => disk.volume.detach().await.map_err(|e| e.to_string()),
                    None => Ok(()),
                }
            })
            .await
            .unwrap_or(Ok(()))
            .map_err(|e| ("internal", e))
    }

    /// Whether a frontend stays connected to `volume`'s disk through
    /// [`HANGUP_GRACE`], checked every 50 ms without holding the reactor.
    /// A volume not attached, or taken elsewhere (D150), has none.
    async fn still_connected(&self, volume: u128) -> bool {
        let t = std::time::Instant::now();
        loop {
            let connected = self
                .disks
                .borrow()
                .get(&volume)
                .is_some_and(|d| d.conn.borrow().as_ref().is_some_and(peer_open));
            let open = connected && self.status(volume).await.is_some_and(|(lost, _)| !lost);
            if !open || t.elapsed() >= HANGUP_GRACE {
                return open;
            }
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// How long a detach waits for a connected frontend to hang up before it
/// answers `busy` (D256): a hypervisor that was just stopped closes its
/// socket only once its process has exited, which takes a large VM a
/// while, and a process forked meanwhile holds the socket until it execs.
const HANGUP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the frontend on `s` is still there: a hypervisor that exited
/// has hung up, though the disk's task may not have read the end yet.
fn peer_open(s: &UnixStream) -> bool {
    let mut p = libc::pollfd {
        fd: s.as_raw_fd(),
        events: libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: one pollfd on a socket this service owns, no wait.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n == 0 || p.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) == 0
}

/// Accepts one frontend at a time on a disk's socket, until detached.
/// Each connection starts here, on the first reactor, and goes on on the
/// reactor its guest's memory says once its memory table comes (D292);
/// the disk and its lease move there first (D291).
async fn serve_disk(
    svc: Rc<Service>,
    volume: u128,
    home: Rc<Cell<usize>>,
    listener: UnixListener,
    user: u32,
    conn: Rc<RefCell<Option<UnixStream>>>,
    stopping: Rc<Cell<bool>>,
) {
    // SAFETY: getuid has no preconditions.
    let own = unsafe { libc::getuid() };
    loop {
        if fd::readable(listener.as_raw_fd()).await.is_err() || stopping.get() {
            return;
        }
        let Ok((s, _)) = listener.accept() else {
            if stopping.get() {
                return;
            }
            continue;
        };
        if !may_use(peer_cred(&s).map(|p| p.0), own, &[user]) {
            eprintln!(
                "hyperstor-vhost: volume {volume:032x}: refused a connection from a user it isn't for"
            );
            continue;
        }
        let Ok(keep) = s.try_clone() else { continue };
        *conn.borrow_mut() = Some(keep);
        let size = svc.status(volume).await.map_or(0, |(_, size)| size);
        let mut ended = device::serve_connection(volume, size, svc.opts.queues, s).await;
        // Placed at its first memory table, and again, once, if its
        // guest's memory isn't bound and its pages are elsewhere (D292).
        let r = loop {
            let (moving, policy) = match ended {
                device::Ended::Closed(r) => break r,
                device::Ended::Place(m, p) => (m, p),
            };
            let to = {
                let load = svc.load.borrow();
                numa::place(&policy, &svc.reactors.nodes(), &load)
            };
            let at = svc.move_disk(volume, home.get(), to).await;
            home.set(at);
            let node = svc.reactors.all[at].node;
            svc.load.borrow_mut()[at] += 1;
            ended = svc
                .reactors
                .run_on(at, move || device::resume(moving, node))
                .await
                .unwrap_or_else(|| device::Ended::Closed(Err("its reactor stopped".into())));
            svc.load.borrow_mut()[at] -= 1;
        };
        *conn.borrow_mut() = None;
        if stopping.get() {
            return;
        }
        if let Err(e) = r {
            eprintln!("hyperstor-vhost: volume {volume:032x}: connection ended: {e}");
        }
    }
}

impl Service {
    /// Moves `volume`'s disk from reactor `from` to reactor `to` (D291):
    /// `to`'s client takes over renewing its lease, then `from` lets go of
    /// its handle without detaching. Returns where the disk is: `from`,
    /// if `to` couldn't take it.
    async fn move_disk(&self, volume: u128, from: usize, to: usize) -> usize {
        if from == to {
            return to;
        }
        let Some((serial, lease, label, poll, delegate)) =
            self.disks.borrow().get(&volume).map(|d| {
                (
                    d.serial.clone(),
                    d.lease,
                    d.label.clone(),
                    d.poll,
                    d.delegate.get(),
                )
            })
        else {
            return from;
        };
        let (spec, queues) = (self.reactors.spec(), self.opts.queues);
        let hold = (self.opts.hold_full, self.opts.hold_outage);
        let taken = self
            .reactors
            .run_on(to, move || async move {
                let client = reactors::client(&spec).await?;
                // A migration's target stands by on the new reactor too.
                let att = if delegate {
                    let att = client
                        .delegate(volume, lease)
                        .await
                        .map_err(|e| e.to_string())?;
                    att.set_standby().map_err(|e| e.to_string())?;
                    *att.label_mut() = label;
                    att
                } else {
                    client
                        .adopt(volume, lease, &label)
                        .await
                        .map_err(|e| e.to_string())?
                };
                device::hold_disk(new_disk(att, serial, queues, hold, poll));
                Ok::<(), String>(())
            })
            .await
            .unwrap_or_else(|| Err("its reactor stopped".into()));
        if let Err(e) = taken {
            eprintln!(
                "hyperstor-vhost: volume {volume:032x}: stays on reactor {from}, reactor {to} couldn't take it: {e}"
            );
            return from;
        }
        // Requests still running there finish on the handle they hold.
        self.reactors
            .run_on(from, move || async move {
                device::drop_disk(volume);
            })
            .await;
        to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_file_round_trips_and_bad_ones_say_why() {
        let s = State {
            instance: 0x0123_4567_89ab_cdef,
            disks: vec![
                (
                    u128::MAX,
                    "serial-1".into(),
                    9,
                    "vhost:node-1".into(),
                    true,
                    Some(Owner {
                        uid: 994,
                        gid: Some(984),
                    }),
                ),
                (
                    7,
                    String::new(),
                    1,
                    "vhost".into(),
                    false,
                    Some(Owner { uid: 0, gid: None }),
                ),
                (8, String::new(), 2, "vhost".into(), false, None),
            ],
        };
        assert_eq!(State::parse(&s.to_json()), Ok(s));
        // A file from before lease identities: their leases are the
        // instance's (D150).
        let old = State::parse(r#"{"instance": "5", "disks": [{"volume": "1", "serial": "s"}]}"#)
            .unwrap();
        assert_eq!(
            old.disks,
            vec![(1, "s".to_string(), 0, "vhost".to_string(), false, None)]
        );
        assert!(
            State::parse(
                r#"{"instance": "1", "disks": [{"volume": "1", "serial": "", "uid": "x"}]}"#
            )
            .unwrap_err()
            .contains("uid")
        );
        assert!(State::parse("{}").is_err());
        assert!(State::parse(r#"{"instance": "zz", "disks": []}"#).is_err());
        assert!(
            State::parse(r#"{"instance": "1", "disks": [{"volume": "1"}]}"#)
                .unwrap_err()
                .contains("serial")
        );
    }

    #[test]
    fn only_root_the_service_and_allowed_users_use_its_sockets() {
        assert!(may_use(Some(0), 990, &[994]));
        assert!(may_use(Some(990), 990, &[]));
        assert!(may_use(Some(994), 990, &[994, 1000]));
        assert!(may_use(Some(1000), 990, &[994, 1000]));
        // In the sockets' group, say, but not named.
        assert!(!may_use(Some(995), 990, &[994]));
        assert!(!may_use(None, 990, &[994]));
    }

    /// A volume is for the user who attached it, or the one root names
    /// (D355).
    #[test]
    fn a_volume_is_for_who_attached_it_or_whom_root_names() {
        let (own, allowed) = (990, &[994, 1000][..]);
        let o = |uid, gid| Ok(Owner { uid, gid });
        assert_eq!(owner_for((994, 984), own, allowed, None), o(994, Some(984)));
        assert_eq!(
            owner_for((1000, 1000), own, allowed, None),
            o(1000, Some(1000))
        );
        // Root's and the service's own keep the service's group.
        assert_eq!(owner_for((0, 0), own, allowed, None), o(0, None));
        assert_eq!(owner_for((990, 980), own, allowed, None), o(990, None));
        assert_eq!(
            owner_for((0, 0), own, allowed, Some((994, 984))),
            o(994, Some(984))
        );
        // Only root names another, and only an allowed one.
        assert_eq!(
            owner_for((994, 984), own, allowed, Some((1000, 1000)))
                .unwrap_err()
                .0,
            "denied"
        );
        assert_eq!(
            owner_for((0, 0), own, allowed, Some((995, 995)))
                .unwrap_err()
                .0,
            "invalid"
        );
    }

    /// The state file is never readable but by the service, even while it
    /// is written, nor after a crash left a readable `.new` behind.
    #[test]
    fn the_state_file_is_never_readable_by_others() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("hs-vhost-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = state_path(&dir);
        let tmp = path.with_extension("json.new");
        std::fs::write(&tmp, "left by a crash").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "{}").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(!tmp.exists());
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let (dir, done) = (dir.clone(), done.clone());
            std::thread::spawn(move || {
                let (mut seen, mut wrong) = (0u64, Vec::new());
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    for e in std::fs::read_dir(&dir).unwrap().flatten() {
                        if let Ok(m) = e.metadata() {
                            seen += 1;
                            if m.mode() & 0o077 != 0 {
                                wrong.push(format!("{:?} {:o}", e.file_name(), m.mode() & 0o777));
                            }
                        }
                    }
                }
                (seen, wrong)
            })
        };
        for i in 0..2000 {
            write_private(&path, &format!("{{\"n\": {i}}}")).unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let (seen, wrong) = watcher.join().unwrap();
        assert!(seen > 0);
        assert!(wrong.is_empty(), "seen {wrong:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"n": 1999}"#);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A socket is never seen at its path before it has its mode and group
    /// (glidex #584), even while it is replaced again and again.
    #[test]
    fn a_socket_appears_only_once_its_group_can_open_it() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("hs-vhost-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gid = std::fs::metadata(&dir).unwrap().gid();
        let path = dir.join("v.sock");
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let (path, done) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                let (mut seen, mut wrong) = (0u64, Vec::new());
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(m) = std::fs::symlink_metadata(&path) {
                        seen += 1;
                        if m.mode() & 0o777 != 0o660 || m.gid() != gid {
                            wrong.push(format!("{:o}", m.mode() & 0o777));
                        }
                    }
                }
                (seen, wrong)
            })
        };
        for _ in 0..2000 {
            drop(bind_shared(&path, 0o660, Some(gid)).unwrap());
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let (seen, wrong) = watcher.join().unwrap();
        assert!(seen > 0);
        assert!(wrong.is_empty(), "seen with modes {wrong:?}");
        let m = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(m.mode() & 0o777, 0o660);
        assert!(!dir.join("v.part").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
