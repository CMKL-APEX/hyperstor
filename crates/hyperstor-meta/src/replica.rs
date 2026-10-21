//! The map service's Raft member (decisions.md D20, D71): `glidex-raftstore`
//! on a tokio runtime of its own, off the map service's reactor.
//!
//! The reactor keeps the map service's state ([`crate::Meta`]); this side
//! holds the replicated store. It tells the reactor which member leads and
//! every map applied ([`ToReactor`]), and commits the maps the leader's
//! reactor publishes ([`Commit`]).
//!
//! Members talk to each other over TCP, with mutual TLS when certificates
//! are given (port 8851, overview.md §3): a request is `kind (u8), length
//! (u32), body`, an answer `ok (u8), length (u32), body`.

use crate::ClusterMap;
use crate::store::{self, Db, MapSchema};
use glidex_raftstore::raft::{
    ClusterNode, RaftId, RaftService, RaftSettings, Rpc, Transport, TransportError,
};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A member of the map service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub id: RaftId,
    /// Where it listens for Raft.
    pub raft: SocketAddr,
    /// Where it serves the map service (where nodes are redirected to).
    pub map: SocketAddr,
}

/// Mutual TLS between members: the member's own `meta` certificate as both
/// server and client, and the cluster id peers' identities must carry.
#[derive(Clone)]
pub struct PeerTls {
    pub server: Arc<rustls::ServerConfig>,
    pub client: Arc<rustls::ClientConfig>,
    pub cluster: String,
}

pub struct Config {
    pub id: RaftId,
    /// Every member, this one included.
    pub members: Vec<Member>,
    /// The store (`map.redb`) and the Raft log (`raft/`).
    pub dir: PathBuf,
    pub tls: Option<PeerTls>,
    pub settings: RaftSettings,
}

/// What the Raft side tells the map service's reactor.
#[derive(Debug)]
pub enum ToReactor {
    /// A map was applied (committed by the group), in commit order.
    Applied(ClusterMap),
    /// Who leads: this member, another (by its map-service address), or
    /// none known (an election).
    Leader(Leading),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leading {
    Me,
    Other(SocketAddr),
    Unknown,
}

/// A map for the group to commit; the answer comes once it is committed
/// and applied here, or with why not (no longer the leader, say).
pub struct Commit {
    pub map: ClusterMap,
    pub reply: hyperstor_reactor::chan::OneSender<Result<(), String>>,
}

/// The running member.
pub struct Replica {
    pub commits: tokio::sync::mpsc::UnboundedSender<Commit>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Replica {
    /// Stops the member: leaves the group's messages unanswered, as a
    /// crash would, but closes its store cleanly.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Opens the store, starts the member on a thread with its own runtime
/// and returns once it serves Raft on `listener`. The lowest-numbered
/// member forms the group the first time (its log is fresh).
pub fn spawn(
    cfg: Config,
    listener: std::net::TcpListener,
    to_reactor: hyperstor_reactor::chan::Sender<ToReactor>,
) -> Result<Replica, String> {
    std::fs::create_dir_all(&cfg.dir).map_err(|e| format!("{}: {e}", cfg.dir.display()))?;
    let db = Arc::new(Db::create(cfg.dir.join("map.redb")).map_err(|e| e.to_string())?);
    let (commits, commit_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stop, stop_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("hyperstor-raft".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            rt.block_on(run(
                cfg, db, listener, to_reactor, commit_rx, stop_rx, ready_tx,
            ));
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Replica {
            commits,
            stop: Some(stop),
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err("the Raft thread ended".into()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    cfg: Config,
    db: Arc<Db>,
    listener: std::net::TcpListener,
    to_reactor: hyperstor_reactor::chan::Sender<ToReactor>,
    mut commits: tokio::sync::mpsc::UnboundedReceiver<Commit>,
    stop: tokio::sync::oneshot::Receiver<()>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let fail = |e: String| {
        let _ = ready.send(Err(e));
    };
    if !cfg.members.iter().any(|m| m.id == cfg.id) {
        return fail(format!("member {} is not in the member list", cfg.id));
    }
    let transport = Arc::new(Tcp {
        tls: cfg.tls.clone(),
        conns: tokio::sync::Mutex::new(HashMap::new()),
    });
    let node = match ClusterNode::<MapSchema>::start(
        db.clone(),
        &cfg.dir.join("raft"),
        cfg.id,
        &cfg.settings,
        transport,
    )
    .await
    {
        Ok(n) => n,
        Err(e) => return fail(format!("starting Raft: {e}")),
    };
    let maps: HashMap<RaftId, SocketAddr> = cfg.members.iter().map(|m| (m.id, m.map)).collect();
    // Before membership is applied, a member knows who leads but not where.
    {
        let maps = maps.clone();
        node.set_leader_lookup(Box::new(move |id| maps.get(&id).map(|a| a.to_string())));
    }
    if let Err(e) = listener.set_nonblocking(true) {
        return fail(e.to_string());
    }
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => return fail(e.to_string()),
    };
    tokio::spawn(serve(listener, node.service.clone(), cfg.tls.clone()));
    let first = cfg.members.iter().map(|m| m.id).min() == Some(cfg.id);
    if first && node.log.is_fresh().unwrap_or(false) {
        let all: BTreeMap<RaftId, openraft::BasicNode> = cfg
            .members
            .iter()
            .map(|m| (m.id, openraft::BasicNode::new(m.raft.to_string())))
            .collect();
        if let Err(e) = node.raft.initialize(all).await {
            return fail(format!("forming the group: {e}"));
        }
    }
    node.replicate();
    let _ = ready.send(Ok(()));

    // Applied maps, in commit order, then whatever the store holds now.
    let mut applied = db.subscribe();
    if let Ok(Some(m)) = store::load(&db) {
        let _ = to_reactor.send(ToReactor::Applied(m));
    }
    let applier = {
        let (db, to) = (db.clone(), to_reactor.clone());
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match applied.recv().await {
                    Ok(_) | Err(RecvError::Lagged(_)) => {
                        if let Ok(Some(m)) = store::load(&db) {
                            let _ = to.send(ToReactor::Applied(m));
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        })
    };
    // Who leads.
    let watcher = {
        let (node, to) = (node.clone(), to_reactor.clone());
        tokio::spawn(async move {
            let mut metrics = node.raft.metrics();
            let mut last = None;
            loop {
                let leading = match metrics.borrow().current_leader {
                    Some(l) if l == node.id => Leading::Me,
                    Some(l) => maps
                        .get(&l)
                        .map_or(Leading::Unknown, |a| Leading::Other(*a)),
                    None => Leading::Unknown,
                };
                if last != Some(leading) {
                    last = Some(leading);
                    let _ = to.send(ToReactor::Leader(leading));
                }
                if metrics.changed().await.is_err() {
                    return;
                }
            }
        })
    };
    // Commits from the leader's reactor.
    let committer = {
        let db = db.clone();
        tokio::spawn(async move {
            while let Some(c) = commits.recv().await {
                let db = db.clone();
                let r = tokio::task::spawn_blocking(move || {
                    store::save(&db, &c.map).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                let _ = c.reply.send(r);
            }
        })
    };
    let _ = stop.await;
    committer.abort();
    watcher.abort();
    applier.abort();
    node.shutdown().await;
}

// ---------------------------------------------------------------------
// The transport.

const APPEND: u8 = 1;
const VOTE: u8 = 2;
const SNAPSHOT: u8 = 3;
/// A snapshot is the whole map, so a frame is bounded well above it.
const MAX_FRAME: u32 = 256 << 20;

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A connection to one member, used for one request at a time.
type Slot = Arc<tokio::sync::Mutex<Option<Box<dyn Stream>>>>;

struct Tcp {
    tls: Option<PeerTls>,
    conns: tokio::sync::Mutex<HashMap<String, Slot>>,
}

/// The peer certificate's identity must be a `meta` of this cluster.
fn check_peer(
    certs: Option<&[rustls::pki_types::CertificateDer<'_>]>,
    cluster: &str,
) -> Result<(), String> {
    let der = certs
        .and_then(|c| c.first())
        .ok_or("the peer sent no certificate")?;
    match hyperstor_pki::identity_of(der) {
        Some(id) if id.role == hyperstor_pki::Role::Meta && id.cluster == cluster => Ok(()),
        Some(id) => Err(format!(
            "{} is not a map service member of this cluster",
            id.uri()
        )),
        None => Err("the peer's certificate has no identity".into()),
    }
}

impl Tcp {
    async fn connect(&self, addr: &str) -> Result<Box<dyn Stream>, String> {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| format!("{addr}: {e}"))?;
        let _ = tcp.set_nodelay(true);
        let Some(tls) = &self.tls else {
            return Ok(Box::new(tcp));
        };
        let sock: SocketAddr = addr
            .parse()
            .map_err(|_| format!("{addr}: not an address"))?;
        let name = rustls::pki_types::ServerName::IpAddress(sock.ip().into());
        let s = tokio_rustls::TlsConnector::from(tls.client.clone())
            .connect(name, tcp)
            .await
            .map_err(|e| format!("{addr}: TLS: {e}"))?;
        check_peer(s.get_ref().1.peer_certificates(), &tls.cluster)?;
        Ok(Box::new(s))
    }

    async fn request(&self, addr: &str, kind: u8, body: &[u8]) -> Result<Vec<u8>, TransportError> {
        let slot = self
            .conns
            .lock()
            .await
            .entry(addr.to_string())
            .or_default()
            .clone();
        let mut conn = slot.lock().await;
        // A connection kept from before may have been closed by the peer
        // (a restart): one fresh try.
        for attempt in 0..2 {
            if conn.is_none() {
                *conn = Some(self.connect(addr).await.map_err(TransportError)?);
            }
            let s = conn.as_mut().expect("connected");
            match exchange(s, kind, body).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    *conn = None;
                    if attempt == 1 {
                        return Err(TransportError(format!("{addr}: {e}")));
                    }
                }
            }
        }
        unreachable!()
    }
}

async fn exchange(s: &mut Box<dyn Stream>, kind: u8, body: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(5 + body.len());
    head.push(kind);
    head.extend_from_slice(&(body.len() as u32).to_le_bytes());
    head.extend_from_slice(body);
    s.write_all(&head).await?;
    s.flush().await?;
    let ok = s.read_u8().await?;
    let len = s.read_u32_le().await?;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("answer too large"));
    }
    let mut out = vec![0; len as usize];
    s.read_exact(&mut out).await?;
    if ok == 1 {
        Ok(out)
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out).to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl Transport for Tcp {
    async fn call(
        &self,
        _target: RaftId,
        addr: &str,
        rpc: Rpc,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, TransportError> {
        let kind = match rpc {
            Rpc::Append => APPEND,
            Rpc::Vote => VOTE,
        };
        self.request(addr, kind, &body).await
    }

    async fn snapshot(
        &self,
        _target: RaftId,
        addr: &str,
        header: Vec<u8>,
        data: PathBuf,
    ) -> Result<Vec<u8>, TransportError> {
        let data = tokio::fs::read(&data)
            .await
            .map_err(|e| TransportError(format!("{}: {e}", data.display())))?;
        let mut body = Vec::with_capacity(4 + header.len() + data.len());
        body.extend_from_slice(&(header.len() as u32).to_le_bytes());
        body.extend_from_slice(&header);
        body.extend_from_slice(&data);
        self.request(addr, SNAPSHOT, &body).await
    }
}

async fn serve(
    listener: tokio::net::TcpListener,
    service: Arc<RaftService<MapSchema>>,
    tls: Option<PeerTls>,
) {
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let (service, tls) = (service.clone(), tls.clone());
        tokio::spawn(async move {
            let s: Box<dyn Stream> = match &tls {
                None => Box::new(tcp),
                Some(t) => {
                    let Ok(s) = tokio_rustls::TlsAcceptor::from(t.server.clone())
                        .accept(tcp)
                        .await
                    else {
                        return;
                    };
                    if check_peer(s.get_ref().1.peer_certificates(), &t.cluster).is_err() {
                        return;
                    }
                    Box::new(s)
                }
            };
            let _ = answer(s, &service).await;
        });
    }
}

async fn answer(mut s: Box<dyn Stream>, service: &RaftService<MapSchema>) -> std::io::Result<()> {
    loop {
        let kind = s.read_u8().await?;
        let len = s.read_u32_le().await?;
        if len > MAX_FRAME {
            return Err(std::io::Error::other("request too large"));
        }
        let mut body = vec![0; len as usize];
        s.read_exact(&mut body).await?;
        let reply = match kind {
            APPEND => service.handle(Rpc::Append, &body).await,
            VOTE => service.handle(Rpc::Vote, &body).await,
            SNAPSHOT => snapshot(service, &body).await,
            k => Err(TransportError(format!("unknown request {k}"))),
        };
        let (ok, out) = match reply {
            Ok(b) => (1u8, b),
            Err(e) => (0u8, e.0.into_bytes()),
        };
        let mut frame = Vec::with_capacity(5 + out.len());
        frame.push(ok);
        frame.extend_from_slice(&(out.len() as u32).to_le_bytes());
        frame.extend_from_slice(&out);
        s.write_all(&frame).await?;
        s.flush().await?;
    }
}

async fn snapshot(
    service: &RaftService<MapSchema>,
    body: &[u8],
) -> Result<Vec<u8>, TransportError> {
    let bad = || TransportError("malformed snapshot".into());
    let hl = u32::from_le_bytes(body.get(..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
    let header = body.get(4..4 + hl).ok_or_else(bad)?;
    let data = &body[4 + hl..];
    tokio::fs::create_dir_all(&service.dir)
        .await
        .map_err(|e| TransportError(e.to_string()))?;
    let mut nonce = [0u8; 8];
    // SAFETY: getrandom writes at most 8 bytes into `nonce`.
    unsafe { libc::getrandom(nonce.as_mut_ptr() as *mut libc::c_void, 8, 0) };
    let path = service
        .dir
        .join(format!("recv-{}.snap", u64::from_le_bytes(nonce)));
    tokio::fs::write(&path, data)
        .await
        .map_err(|e| TransportError(e.to_string()))?;
    service.handle_snapshot(header, path).await
}

/// Raft timing for the map service. No lease is renewed while there is no
/// leader, and leases are short (D48), so elections must be too: a dead
/// leader is replaced within about half a lease.
pub fn settings() -> RaftSettings {
    RaftSettings {
        heartbeat_ms: 50,
        election_ms: (200, 400),
        // An entry is usually what one change touched, but a whole map
        // (the first, a new pool) can be megabytes: keep the log short.
        log_keep_entries: 1_000,
    }
}

/// Mutual TLS between members from the member's `--api-tls` directory: its
/// `meta` certificate (`api.crt`, `api.key`) and the cluster's `root.crt`.
pub fn peer_tls_from_dir(dir: &std::path::Path, cluster: u128) -> Result<PeerTls, String> {
    let read = |f: &str| {
        std::fs::read_to_string(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()))
    };
    let (cert, key, root) = (read("api.crt")?, read("api.key")?, read("root.crt")?);
    Ok(PeerTls {
        server: Arc::new(hyperstor_pki::server_config(&cert, &key, &root)?),
        client: Arc::new(hyperstor_pki::client_config(&cert, &key, &root)?),
        cluster: cluster.to_string(),
    })
}
