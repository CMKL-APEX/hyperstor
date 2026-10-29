//! `hyperstor-vhost` serves the volumes attached on this node to its VMs
//! over vhost-user-blk (frontends.md §11.1, decisions.md D1, D26, D116).
//!
//! One reactor thread runs everything: a client of the cluster, a control
//! socket (D117) through which glidex attaches and detaches volumes, and
//! a vhost-user socket per attached volume, `<dir>/<volume id>.sock`, which
//! the hypervisor connects to (and reconnects to after either side
//! restarts). Each queue of a connected disk is a task on the same
//! reactor; more reactors, pinned per NUMA node, come later.
//!
//! Stopping the service leaves its volumes attached (decisions.md D143):
//! `<dir>/state.json` names them and the instance that held them, and the
//! next start takes them back from that instance, which the hypervisors
//! then reconnect to. A volume another client took meanwhile is dropped.

pub mod blk;
pub mod control;
pub mod device;
pub mod inflight;

use control::{Reply, Request, Take};
use device::Disk;
use hyperstor_client::{Client, ClientError};
use hyperstor_reactor::fd;
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
    /// Users allowed on the control socket besides root and our own.
    pub allow_uids: Vec<u32>,
    /// The group given the sockets (the hypervisors' and glidex's), if any.
    pub group: Option<u32>,
    /// Queues each disk offers.
    pub queues: u16,
}

/// The control socket's path in `dir`.
pub fn control_path(dir: &Path) -> PathBuf {
    dir.join("control.sock")
}

/// The state file's path in `dir`.
pub fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

/// What survives a restart (D143): the instance that held the volumes,
/// and each volume with its serial and lease.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct State {
    pub instance: u64,
    /// Each disk: its volume, serial, the identity of its lease (D150; 0
    /// in a file written before identities, whose leases have the
    /// instance as their identity) and its label (D153).
    pub disks: Vec<(u128, String, u64, String)>,
}

impl State {
    pub fn to_json(&self) -> String {
        let disks: Vec<serde_json::Value> = self
            .disks
            .iter()
            .map(|(v, s, l, label)| {
                serde_json::json!({
                    "volume": format!("{v:032x}"),
                    "serial": s,
                    "lease": format!("{l:016x}"),
                    "label": label,
                })
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
            disks.push((volume, hex(&d["serial"], "serial")?, lease, label));
        }
        Ok(State { instance, disks })
    }
}

/// A disk socket's path in `dir`.
pub fn disk_path(dir: &Path, volume: u128) -> PathBuf {
    dir.join(format!("{volume:032x}.sock"))
}

struct Served {
    disk: Rc<Disk>,
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
    let longest = disk_path(&opts.dir, u128::MAX);
    if longest.as_os_str().len() > 107 {
        return Err(format!(
            "{}: too long for the volumes' sockets (a socket's path is at most 107 bytes)",
            opts.dir.display()
        ));
    }
    std::fs::create_dir_all(&opts.dir).map_err(|e| format!("{}: {e}", opts.dir.display()))?;
    // The sockets' group may pass through to them, no one else (D144).
    if let Some(g) = opts.group {
        std::fs::set_permissions(&opts.dir, std::fs::Permissions::from_mode(0o750))
            .map_err(|e| format!("{}: {e}", opts.dir.display()))?;
        chown_group(&opts.dir, g)?;
    }
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
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(g) = opts.group {
        chown_group(&path, g)?;
    }
    let (tx, mut rx) = hyperstor_reactor::chan::channel::<()>();
    std::thread::spawn(move || {
        let _ = stop.recv();
        let _ = tx.send(());
    });
    hyperstor_reactor::block_on(async move {
        let client = Client::connect_members(opts.meta.clone(), instance_id())
            .await
            .map_err(|e| format!("map service: {e}"))?;
        let svc = Rc::new(Service {
            opts,
            client,
            disks: RefCell::new(BTreeMap::new()),
        });
        // Before glidex can ask: the volumes the last instance had.
        if let Some(prev) = previous {
            for (volume, serial, lease, label) in prev.disks {
                let old = if lease == 0 { prev.instance } else { lease };
                match svc.reclaim(volume, serial, old, &label).await {
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

fn chown_group(path: &Path, gid: u32) -> Result<(), String> {
    std::os::unix::fs::chown(path, None, Some(gid)).map_err(|e| format!("{}: {e}", path.display()))
}

/// The uid at the other end of a Unix socket.
fn peer_uid(s: &UnixStream) -> Option<u32> {
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
    (r == 0).then_some(cred.uid)
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
        let allowed =
            peer_uid(&s).is_some_and(|u| u == 0 || u == own || svc.opts.allow_uids.contains(&u));
        if !allowed {
            let r = Reply::error(0, "denied", "this user may not use hyperstor-vhost");
            let _ = writeln!(s, "{}", r.line());
            continue;
        }
        hyperstor_reactor::spawn(control_conn(svc.clone(), s));
    }
}

/// One control connection: a request per line, a reply per line.
async fn control_conn(svc: Rc<Service>, mut s: UnixStream) {
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
                Ok((id, req)) => svc.handle(id, req).await,
                Err(e) => Reply::error(0, "invalid", &e),
            };
            if writeln!(s, "{}", reply.line()).is_err() {
                return;
            }
        }
    }
}

impl Service {
    async fn handle(self: &Rc<Self>, id: u64, req: Request) -> Reply {
        match req {
            Request::Hello => Reply::ok(id, control::hello()),
            Request::List => Reply::ok(id, control::list(self.list())),
            Request::Attach {
                volume,
                serial,
                take,
                holder,
            } => {
                let label = if holder.is_empty() {
                    "vhost".to_string()
                } else {
                    format!("vhost:{holder}")
                };
                match self.attach(volume, serial, take, &label).await {
                    Ok(v) => Reply::ok(id, v),
                    Err(Refused::Held { message, holder }) => Reply::held(id, &message, &holder),
                    Err(Refused::Other(code, msg)) => Reply::error(id, code, &msg),
                }
            }
            Request::Detach { volume } => match self.detach(volume).await {
                Ok(()) => Reply::ok(id, control::detached()),
                Err((code, msg)) => Reply::error(id, code, &msg),
            },
        }
    }

    fn list(&self) -> Vec<control::Listed> {
        self.disks
            .borrow()
            .iter()
            .map(|(id, d)| control::Listed {
                volume: *id,
                socket: d.socket.clone(),
                size: d.disk.volume.volume().size,
                connected: d.conn.borrow().is_some(),
                lost: d.disk.volume.lost(),
            })
            .collect()
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
                .map(|(v, d)| {
                    let a = &d.disk.volume;
                    (*v, d.disk.serial.clone(), a.lease(), a.label().to_string())
                })
                .collect(),
        };
        let path = state_path(&self.opts.dir);
        let tmp = path.with_extension("json.new");
        let r = std::fs::write(&tmp, state.to_json())
            .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = r {
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
        self.open_disk(volume, serial, att)
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
    ) -> Result<control::Value, Refused> {
        let lost = match self.disks.borrow().get(&volume) {
            Some(d) if !d.disk.volume.lost() => {
                return Ok(control::attached(&d.socket, d.disk.volume.volume().size));
            }
            d => d.is_some(),
        };
        if lost {
            self.detach(volume).await.map_err(Refused::from)?;
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
        self.open_disk(volume, serial, att)
            .await
            .map_err(Refused::from)
    }

    /// Opens the socket of `volume`, attached as `att`, and serves it.
    async fn open_disk(
        self: &Rc<Self>,
        volume: u128,
        serial: String,
        att: Rc<hyperstor_client::volume::Attached>,
    ) -> Result<control::Value, (&'static str, String)> {
        // Another request may have attached it meanwhile.
        if let Some(d) = self.disks.borrow().get(&volume) {
            return Ok(control::attached(&d.socket, d.disk.volume.volume().size));
        }
        let socket = disk_path(&self.opts.dir, volume);
        let _ = std::fs::remove_file(&socket);
        let opened = UnixListener::bind(&socket)
            .and_then(|l| Ok((l.try_clone()?, l)))
            .map_err(|e| format!("{}: {e}", socket.display()))
            .and_then(|l| {
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))
                    .map_err(|e| format!("{}: {e}", socket.display()))?;
                self.opts
                    .group
                    .map_or(Ok(()), |g| chown_group(&socket, g))?;
                Ok(l)
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
        let disk = Rc::new(Disk {
            volume: att,
            serial,
            queues: self.opts.queues,
            locks: Default::default(),
        });
        let conn = Rc::new(RefCell::new(None));
        let stopping = Rc::new(Cell::new(false));
        let task = hyperstor_reactor::spawn(serve_disk(
            disk.clone(),
            accepting,
            conn.clone(),
            stopping.clone(),
        ));
        self.disks.borrow_mut().insert(
            volume,
            Served {
                disk,
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
    async fn detach(&self, volume: u128) -> Result<(), (&'static str, String)> {
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
        d.disk
            .volume
            .detach()
            .await
            .map_err(|e| ("internal", e.to_string()))
    }
}

/// Accepts one frontend at a time on a disk's socket, until detached.
async fn serve_disk(
    disk: Rc<Disk>,
    listener: UnixListener,
    conn: Rc<RefCell<Option<UnixStream>>>,
    stopping: Rc<Cell<bool>>,
) {
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
        let Ok(keep) = s.try_clone() else { continue };
        *conn.borrow_mut() = Some(keep);
        let r = device::serve_connection(disk.clone(), s).await;
        *conn.borrow_mut() = None;
        if stopping.get() {
            return;
        }
        if let Err(e) = r {
            eprintln!(
                "hyperstor-vhost: volume {:032x}: connection ended: {e}",
                disk.volume.volume().id
            );
        }
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
                (u128::MAX, "serial-1".into(), 9, "vhost:node-1".into()),
                (7, String::new(), 1, "vhost".into()),
            ],
        };
        assert_eq!(State::parse(&s.to_json()), Ok(s));
        // A file from before lease identities: their leases are the
        // instance's (D150).
        let old = State::parse(r#"{"instance": "5", "disks": [{"volume": "1", "serial": "s"}]}"#)
            .unwrap();
        assert_eq!(
            old.disks,
            vec![(1, "s".to_string(), 0, "vhost".to_string())]
        );
        assert!(State::parse("{}").is_err());
        assert!(State::parse(r#"{"instance": "zz", "disks": []}"#).is_err());
        assert!(
            State::parse(r#"{"instance": "1", "disks": [{"volume": "1"}]}"#)
                .unwrap_err()
                .contains("serial")
        );
    }
}
