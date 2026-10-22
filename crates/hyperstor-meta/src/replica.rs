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
    /// Write backups of the store (D86).
    pub backup: Option<crate::backup::Schedule>,
    /// Restore this backup with the group's first commit (a group of one
    /// with an empty store, D86).
    pub restore: Option<crate::backup::Backup>,
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

/// A member as Raft records it: `RAFT_ADDR/MAP_ADDR`, so redirects and
/// member changes know where each member serves the map (D87). Groups
/// formed before recorded the Raft address alone.
pub fn node_addr(m: &Member) -> String {
    format!("{}/{}", m.raft, m.map)
}

/// The Raft address and, when recorded, the map address of a member's
/// Raft record.
pub fn split_node(addr: &str) -> (&str, Option<SocketAddr>) {
    match addr.split_once('/') {
        Some((raft, map)) => (raft, map.parse().ok()),
        None => (addr, None),
    }
}

/// The group as this member sees it.
#[derive(Debug, Clone, Default)]
pub struct Members {
    pub voters: Vec<Member>,
    pub learners: Vec<Member>,
    pub leader: Option<RaftId>,
    pub me: RaftId,
}

/// A change to, or a look at, the group's members (D87), done by the
/// Raft side; changes only on the leader.
pub enum MemberOp {
    List(hyperstor_reactor::chan::OneSender<Members>),
    /// Adds `member` as a learner, waits until it has caught up, then
    /// makes it a voter.
    Add {
        member: Member,
        reply: hyperstor_reactor::chan::OneSender<Result<Members, String>>,
    },
    /// Removes a member; refused below three voters unless `force`.
    Remove {
        id: RaftId,
        force: bool,
        reply: hyperstor_reactor::chan::OneSender<Result<Members, String>>,
    },
}

/// The running member.
pub struct Replica {
    pub commits: tokio::sync::mpsc::UnboundedSender<Commit>,
    /// Member changes (D87).
    pub members: tokio::sync::mpsc::UnboundedSender<MemberOp>,
    /// The replicated store, for the API's join tokens (`tokens.rs`).
    pub db: Arc<Db>,
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
    let api_db = db.clone();
    let (commits, commit_rx) = tokio::sync::mpsc::unbounded_channel();
    let (members, member_rx) = tokio::sync::mpsc::unbounded_channel();
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
                cfg, db, listener, to_reactor, commit_rx, member_rx, stop_rx, ready_tx,
            ));
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Replica {
            commits,
            members,
            db: api_db,
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
    mut member_ops: tokio::sync::mpsc::UnboundedReceiver<MemberOp>,
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
        db: db.clone(),
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
    // Where a member serves the map: its Raft record (members added at run
    // time, D87), else the member list it was started with.
    let map_of = {
        let (node, maps) = (Arc::downgrade(&node), maps.clone());
        move |id: RaftId| -> Option<SocketAddr> {
            let recorded = node.upgrade().and_then(|n| {
                let m = n.raft.metrics();
                let b = m.borrow();
                b.membership_config
                    .membership()
                    .get_node(&id)
                    .and_then(|bn| split_node(&bn.addr).1)
            });
            recorded.or_else(|| maps.get(&id).copied())
        }
    };
    {
        let map_of = map_of.clone();
        node.set_leader_lookup(Box::new(move |id| map_of(id).map(|a| a.to_string())));
    }
    if let Err(e) = listener.set_nonblocking(true) {
        return fail(e.to_string());
    }
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => return fail(e.to_string()),
    };
    tokio::spawn(serve(
        listener,
        node.service.clone(),
        cfg.tls.clone(),
        db.clone(),
    ));
    let first = cfg.members.iter().map(|m| m.id).min() == Some(cfg.id);
    if first && node.log.is_fresh().unwrap_or(false) {
        let all: BTreeMap<RaftId, openraft::BasicNode> = cfg
            .members
            .iter()
            .map(|m| (m.id, openraft::BasicNode::new(node_addr(m))))
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
                    Some(l) => map_of(l).map_or(Leading::Unknown, Leading::Other),
                    None => Leading::Unknown,
                };
                if last != Some(leading) {
                    last = Some(leading);
                    let (term, leader) = {
                        let m = metrics.borrow();
                        (m.current_term, m.current_leader)
                    };
                    // For the journal: who leads, and since which term.
                    match leader {
                        Some(l) => eprintln!("hyperstor-meta: member {l} leads (term {term})"),
                        None => eprintln!("hyperstor-meta: no leader (term {term})"),
                    }
                    let _ = to.send(ToReactor::Leader(leading));
                }
                if metrics.changed().await.is_err() {
                    return;
                }
            }
        })
    };
    // Commits from the leader's reactor.
    // Backups, by the leader (D86).
    let backups = cfg.backup.clone().map(|s| {
        let node = node.clone();
        tokio::spawn(crate::backup::writer(db.clone(), s, move || {
            node.raft.metrics().borrow().current_leader == Some(node.id)
        }))
    });
    let committer = {
        let db = db.clone();
        let mut restore = cfg.restore.clone();
        tokio::spawn(async move {
            while let Some(c) = commits.recv().await {
                let db = db.clone();
                let restoring = restore.take();
                let r = tokio::task::spawn_blocking(move || {
                    // The first commit of a restored group writes the
                    // whole backup first, in one entry.
                    if let Some(b) = restoring
                        && !store::has_map(&db)?
                    {
                        b.restore(&db)?;
                        eprintln!(
                            "hyperstor-meta: restored the backup of map epoch {}",
                            b.epoch
                        );
                    }
                    store::save(&db, &c.map).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                let _ = c.reply.send(r);
            }
        })
    };
    // Member changes (D87).
    let membership = {
        let node = node.clone();
        let me = cfg.id;
        tokio::spawn(async move {
            while let Some(op) = member_ops.recv().await {
                match op {
                    MemberOp::List(reply) => {
                        let _ = reply.send(members_now(&node, me));
                    }
                    MemberOp::Add { member, reply } => {
                        let _ = reply.send(add_member(&node, me, member).await);
                    }
                    MemberOp::Remove { id, force, reply } => {
                        let _ = reply.send(remove_member(&node, me, id, force).await);
                    }
                }
            }
        })
    };
    let _ = stop.await;
    membership.abort();
    if let Some(b) = backups {
        b.abort();
    }
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
/// The group from this member's metrics.
fn members_now(node: &ClusterNode<MapSchema>, me: RaftId) -> Members {
    let m = node.raft.metrics();
    let b = m.borrow();
    let membership = b.membership_config.membership();
    let member = |id: RaftId| -> Option<Member> {
        let n = membership.get_node(&id)?;
        let (raft, map) = split_node(&n.addr);
        Some(Member {
            id,
            raft: raft.parse().ok()?,
            map: map.unwrap_or_else(|| ([0, 0, 0, 0], 0).into()),
        })
    };
    Members {
        voters: membership.voter_ids().filter_map(member).collect(),
        learners: membership.learner_ids().filter_map(member).collect(),
        leader: b.current_leader,
        me,
    }
}

fn not_leading(node: &ClusterNode<MapSchema>) -> String {
    match node.leader() {
        Some(l) => format!("this map service member isn't the leader; member {l} leads"),
        None => "this map service member isn't the leader, and no leader is known".into(),
    }
}

/// How long a new member may take to catch up before it votes.
const CATCH_UP: std::time::Duration = std::time::Duration::from_secs(120);

async fn add_member(
    node: &ClusterNode<MapSchema>,
    me: RaftId,
    member: Member,
) -> Result<Members, String> {
    if !node.is_leader() {
        return Err(not_leading(node));
    }
    let now = members_now(node, me);
    if now
        .voters
        .iter()
        .chain(&now.learners)
        .any(|m| m.id == member.id)
    {
        return Err(format!("member {} is already in the group", member.id));
    }
    // A learner first: it receives the log, and votes once it has it.
    tokio::time::timeout(CATCH_UP, node.add_learner(member.id, &node_addr(&member)))
        .await
        .map_err(|_| {
            format!(
                "member {} didn't catch up in {} s: is it running, and reachable at {}?",
                member.id,
                CATCH_UP.as_secs(),
                member.raft
            )
        })?
        .map_err(|e| e.to_string())?;
    let mut voters = node.voters();
    voters.insert(member.id);
    node.set_voters(voters).await.map_err(|e| e.to_string())?;
    Ok(members_now(node, me))
}

async fn remove_member(
    node: &ClusterNode<MapSchema>,
    me: RaftId,
    id: RaftId,
    force: bool,
) -> Result<Members, String> {
    if !node.is_leader() {
        return Err(not_leading(node));
    }
    let voters = node.voters();
    let learners = node.learners();
    if !voters.contains(&id) && !learners.contains(&id) {
        return Err(format!("no member {id}"));
    }
    if voters.contains(&id) && voters.len() <= 3 && !force {
        return Err(format!(
            "removing member {id} leaves {} voters: fewer than three survive no failure (use force)",
            voters.len() - 1
        ));
    }
    if voters.len() == 1 && voters.contains(&id) {
        return Err("the last voter can't be removed".into());
    }
    node.remove_members([id].into_iter().collect())
        .await
        .map_err(|e| e.to_string())?;
    Ok(members_now(node, me))
}

/// A snapshot is the whole map, so a frame is bounded well above it.
const MAX_FRAME: u32 = 256 << 20;

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A connection to one member, used for one request at a time.
type Slot = Arc<tokio::sync::Mutex<Option<Box<dyn Stream>>>>;

struct Tcp {
    tls: Option<PeerTls>,
    conns: tokio::sync::Mutex<HashMap<String, Slot>>,
    /// For the revocation list (`certs.rs`).
    db: Arc<Db>,
}

/// The peer certificate's identity must be a `meta` of this cluster, and
/// its serial not revoked (D56).
fn check_peer(
    certs: Option<&[rustls::pki_types::CertificateDer<'_>]>,
    cluster: &str,
    db: &Arc<Db>,
) -> Result<(), String> {
    let der = certs
        .and_then(|c| c.first())
        .ok_or("the peer sent no certificate")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if crate::certs::Certs::replicated(db.clone()).is_revoked_cert(der, now) {
        let serial = hyperstor_pki::serial_of(der).unwrap_or_default();
        return Err(format!("the peer's certificate {serial} is revoked"));
    }
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
        check_peer(s.get_ref().1.peer_certificates(), &tls.cluster, &self.db)?;
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
        self.request(split_node(addr).0, kind, &body).await
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
        self.request(split_node(addr).0, SNAPSHOT, &body).await
    }
}

async fn serve(
    listener: tokio::net::TcpListener,
    service: Arc<RaftService<MapSchema>>,
    tls: Option<PeerTls>,
    db: Arc<Db>,
) {
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let (service, tls, db) = (service.clone(), tls.clone(), db.clone());
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
                    if let Err(e) = check_peer(s.get_ref().1.peer_certificates(), &t.cluster, &db) {
                        eprintln!("hyperstor-meta: refused a member's connection: {e}");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Members refuse each other's certificates once revoked (D56), and
    /// any that isn't a `meta` of this cluster.
    #[test]
    fn members_refuse_revoked_peers() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(dir.path().join("map.redb")).unwrap());
        let root = hyperstor_pki::new_root("5").unwrap();
        let ca = hyperstor_pki::new_intermediate(&root, "5").unwrap();
        let who = |role, id: &str| hyperstor_pki::Identity {
            cluster: "5".into(),
            role,
            id: id.into(),
        };
        let der = |p: &hyperstor_pki::Pem| {
            use rustls::pki_types::pem::PemObject;
            rustls::pki_types::CertificateDer::pem_slice_iter(p.cert.as_bytes())
                .next()
                .unwrap()
                .unwrap()
        };
        let m2 =
            hyperstor_pki::issue(&ca, &who(hyperstor_pki::Role::Meta, "2"), &[], &[], 30).unwrap();
        let d = der(&m2);
        check_peer(Some(std::slice::from_ref(&d)), "5", &db).unwrap();
        assert!(check_peer(Some(std::slice::from_ref(&d)), "6", &db).is_err());
        let node =
            hyperstor_pki::issue(&ca, &who(hyperstor_pki::Role::Node, "1"), &[], &[], 30).unwrap();
        assert!(check_peer(Some(&[der(&node)]), "5", &db).is_err());
        let serial = hyperstor_pki::serial_in(&m2.cert).unwrap();
        crate::certs::Certs::replicated(db.clone())
            .revoke(
                &serial,
                "test",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
            .unwrap();
        let e = check_peer(Some(std::slice::from_ref(&d)), "5", &db).unwrap_err();
        assert!(e.contains("revoked"));
    }
}
