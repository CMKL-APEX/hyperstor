//! `hyperstor-vhost` serves the volumes attached on this node to its VMs
//! over vhost-user-blk (frontends.md §11.1, decisions.md D1, D26, D111).
//!
//! One reactor thread runs everything: a client of the cluster, a control
//! socket (D112) through which glidex attaches and detaches volumes, and
//! a vhost-user socket per attached volume, `<dir>/<volume id>.sock`, which
//! the hypervisor connects to (and reconnects to after either side
//! restarts). Each queue of a connected disk is a task on the same
//! reactor; more reactors, pinned per NUMA node, come later.

pub mod blk;
pub mod control;
pub mod device;

use control::{Reply, Request, Take};
use device::Disk;
use hyperstor_client::Client;
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
    std::fs::create_dir_all(&opts.dir).map_err(|e| format!("{}: {e}", opts.dir.display()))?;
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
        ready(&path);
        let _accept = hyperstor_reactor::spawn(accept_control(svc.clone(), listener));
        let _ = rx.recv().await;
        let ids: Vec<u128> = svc.disks.borrow().keys().copied().collect();
        for id in ids {
            if let Err((_, e)) = svc.detach(id).await {
                eprintln!("hyperstor-vhost: volume {id:032x}: {e}");
            }
        }
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
            } => match self.attach(volume, serial, take).await {
                Ok(v) => Reply::ok(id, v),
                Err((code, msg)) => Reply::error(id, code, &msg),
            },
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
            })
            .collect()
    }

    /// Attaches `volume` and opens its socket; for a volume attached here
    /// already, answers with its socket (a retry, or glidex restarting).
    async fn attach(
        self: &Rc<Self>,
        volume: u128,
        serial: String,
        take: Take,
    ) -> Result<control::Value, (&'static str, String)> {
        if let Some(d) = self.disks.borrow().get(&volume) {
            return Ok(control::attached(&d.socket, d.disk.volume.volume().size));
        }
        let att = match take {
            Take::Attach => self.client.attach(volume).await,
            Take::TakeOver => self.client.take_over(volume).await,
            Take::Preempt => self.client.preempt(volume).await,
        }
        .map_err(|e| (control::error_code(&e), e.to_string()))?;
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
