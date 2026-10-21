//! `hyperstord`: a storage node (spec/engine.md §6, §8).
//!
//! A node runs one reactor thread per engine shard (implementation-notes.md
//! I1). Each reactor listens on its own address, follows the cluster map,
//! and serves:
//!
//! - `Write` as a PG's primary: checks the PG epoch, sends `Repl` to every
//!   replica and queues the local commit, all before reading the next
//!   request, then answers once every copy has committed (§8.1);
//! - `Repl` as a replica: queues the commit in arrival order;
//! - `Read` as the primary.
//!
//! Commits go through a batcher: everything queued since the last commit
//! is committed in one engine call (group commit, I2). Requests are queued
//! in the order they are read from each connection, and a primary queues a
//! write's replication and local commit without waiting in between, so
//! every copy applies writes in the same order.
//!
//! Not yet (HS2): shrinking the in-sync set when a replica fails, peering,
//! recovery, read leases. A write whose replica is unreachable fails with
//! `Unavailable`, and the client retries.

pub mod metrics;

use hyperstor_client::rpc::{Conn, read_frame, writer};
use hyperstor_client::{Client, ClientError};
use hyperstor_engine::{
    BlockDev, Committed, DeviceConfig, EngineError, FileDev, MemDev, Namespace, ObjKey, Op,
    OpError, OpList, Shard, UringDev,
};
use hyperstor_meta::{ClusterMap, NO_DEVICE, PgInfo};
use hyperstor_proto::{
    Code, DAMAGED, Msg, Target, VERSION, WireKey, block_crcs, decode_digests, encode_digests,
    encode_frame, verify_crcs,
};
use hyperstor_reactor::chan::{OneSender, Sender, channel, oneshot};
use hyperstor_reactor::net::{TcpListener, TcpStream};
use hyperstor_reactor::{block_on, spawn, time::sleep};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

/// The device behind one shard.
#[derive(Clone)]
pub enum DeviceSpec {
    Mem {
        bytes: u64,
    },
    File {
        path: PathBuf,
        bytes: u64,
        direct: bool,
        uring: bool,
    },
    /// A device already open, for example one a stopped node returned.
    Existing(Arc<dyn BlockDev>),
}

impl std::fmt::Debug for DeviceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceSpec::Mem { bytes } => write!(f, "Mem({bytes})"),
            DeviceSpec::File { path, .. } => write!(f, "File({})", path.display()),
            DeviceSpec::Existing(d) => write!(f, "Existing({} bytes)", d.len()),
        }
    }
}

impl DeviceSpec {
    fn open(&self) -> std::io::Result<Arc<dyn BlockDev>> {
        Ok(match self {
            DeviceSpec::Mem { bytes } => Arc::new(MemDev::new(*bytes)),
            DeviceSpec::Existing(d) => d.clone(),
            DeviceSpec::File {
                path,
                bytes,
                direct,
                uring: true,
            } => Arc::new(UringDev::open(path, *direct, Some(*bytes), 256)?),
            DeviceSpec::File {
                path,
                bytes,
                direct,
                uring: false,
            } => Arc::new(FileDev::open(path, *direct, Some(*bytes))?),
        })
    }
}

pub struct ShardSpec {
    pub device_id: u32,
    pub device: DeviceSpec,
    /// Bound before the node starts, so its address can be in the map.
    pub listener: std::net::TcpListener,
}

pub struct NodeConfig {
    pub node_id: u32,
    pub meta: SocketAddr,
    pub shards: Vec<ShardSpec>,
    /// Pin reactor `i` to CPU `cpus[i]`, if given.
    pub cpus: Vec<usize>,
    /// Adaptive polling window for this node's reactors (decisions.md D41);
    /// zero turns it off.
    pub spin: Duration,
    /// Report a peer to the map service after this long without an answer
    /// to pings (decisions.md D40); zero turns peer reports off.
    pub peer_timeout: Duration,
    /// How often each primary scrubs its PGs, comparing every member's
    /// digests (replication.md §8.5); zero turns scrub off.
    pub scrub_interval: Duration,
    /// Test hook: answer `Digests` as a node from before digests does
    /// ("unsupported request"), as in a rolling upgrade.
    pub no_digests: bool,
    /// Test hook: while set, this node neither reaches the map service nor
    /// hears from it (no heartbeats, map fetches, pushes or fences), as if
    /// cut off from it while clients and peers can still reach it.
    pub meta_blocked: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Engine deferral threshold: `None` for flash, which redirects every
    /// write (decisions.md D38); `Some(hyperstor_engine::DEFER_MAX)` for
    /// hard disks.
    pub defer_max: Option<usize>,
}

/// A running node. Dropping it without [`NodeHandle::stop`] leaves the
/// threads running.
pub struct NodeHandle {
    /// Each shard's device id and metrics, for the admin port.
    pub metrics: Vec<(u32, Arc<metrics::ShardMetrics>)>,
    stops: Vec<Sender<()>>,
    threads: Vec<std::thread::JoinHandle<Result<Arc<dyn BlockDev>, String>>>,
}

impl NodeHandle {
    /// Waits for the reactor threads, which run until one fails; then
    /// stops the others and returns every result.
    pub fn stop_when_any_fails(self) -> Vec<Result<Arc<dyn BlockDev>, String>> {
        loop {
            if self.threads.iter().any(|t| t.is_finished()) {
                return self.stop();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Stops every reactor and returns the shards' devices, for inspection.
    pub fn stop(self) -> Vec<Result<Arc<dyn BlockDev>, String>> {
        for s in &self.stops {
            let _ = s.send(());
        }
        self.threads
            .into_iter()
            .map(|t| t.join().unwrap_or_else(|_| Err("reactor panicked".into())))
            .collect()
    }
}

/// Starts a node: one reactor thread per shard.
pub fn start(cfg: NodeConfig) -> NodeHandle {
    let mut stops = Vec::new();
    let mut threads = Vec::new();
    let mut all_metrics = Vec::new();
    for (i, sh) in cfg.shards.into_iter().enumerate() {
        let m = Arc::new(metrics::ShardMetrics::default());
        all_metrics.push((sh.device_id, m.clone()));
        let (tx, mut rx) = channel::<()>();
        stops.push(tx);
        let cpu = cfg.cpus.get(i).copied();
        let (node, meta, defer) = (cfg.node_id, cfg.meta, cfg.defer_max);
        let blocked = cfg.meta_blocked.clone();
        let spin = cfg.spin;
        let peer_timeout = cfg.peer_timeout;
        let scrub_interval = cfg.scrub_interval;
        let no_digests = cfg.no_digests;
        threads.push(
            std::thread::Builder::new()
                .name(format!("hs-reactor-{node}-{i}"))
                .spawn(move || {
                    if let Some(c) = cpu {
                        let _ = hyperstor_reactor::pin_to_cpu(c);
                    }
                    hyperstor_reactor::set_spin(spin);
                    let dev = sh.device.open().map_err(|e| e.to_string())?;
                    let d2 = dev.clone();
                    block_on(async move {
                        let st = ShardState::open(
                            node,
                            sh.device_id,
                            d2,
                            meta,
                            defer,
                            blocked.clone(),
                            m,
                        )
                        .await?;
                        st.no_digests.set(no_digests);
                        let l = TcpListener::from_std(sh.listener);
                        spawn(st.clone().committer());
                        spawn(st.clone().follow_map());
                        spawn(st.clone().heartbeat(node));
                        if !peer_timeout.is_zero() {
                            spawn(st.clone().peer_watch(node, peer_timeout));
                        }
                        spawn(st.clone().backfiller());
                        if !scrub_interval.is_zero() {
                            spawn(st.clone().scrubber(scrub_interval));
                        }
                        spawn(st.clone().accept(l));
                        spawn(st.clone().gauges());
                        rx.recv().await;
                        Ok::<(), String>(())
                    })?;
                    Ok(dev)
                })
                .expect("spawn reactor thread"),
        );
    }
    NodeHandle {
        metrics: all_metrics,
        stops,
        threads,
    }
}

type CommitResult = Result<Committed, String>;

/// One reactor's state.
struct ShardState {
    me: u32,
    shard: RefCell<Shard>,
    client: Rc<Client>,
    queue: RefCell<Vec<(OpList, OneSender<CommitResult>)>>,
    committer: RefCell<Option<Waker>>,
    /// Per-PG replication sequence numbers (primary side).
    seqs: RefCell<HashMap<(u32, u32), u64>>,
    /// Connections to replicas, by address.
    peers: RefCell<HashMap<SocketAddr, Conn>>,
    batches: Cell<u64>,
    /// Backfills this primary is running: (pool, pg, target) -> whether a
    /// write or push to the target failed meanwhile (the copy may then be
    /// incomplete, so the backfill starts over).
    backfills: RefCell<HashMap<(u32, u32, u32), bool>>,
    /// Best-effort writes still in flight to each backfill target.
    fill_inflight: RefCell<HashMap<(u32, u32, u32), u32>>,
    /// Backfill targets whose copy is complete (or being reported): every
    /// write must reach them from now on, because the map service may make
    /// them acting members at any moment.
    required: RefCell<std::collections::HashSet<(u32, u32, u32)>>,
    /// Until when this reactor may serve PG I/O (its map lease, decisions.md
    /// D42); none before the first heartbeat answer.
    lease_until: Cell<Option<std::time::Instant>>,
    /// PGs fenced by the map service, with the map epoch that lifts each.
    fenced: RefCell<HashMap<(u32, u32), u64>>,
    /// Writes accepted as primary and not yet answered, per PG.
    pg_inflight: RefCell<HashMap<(u32, u32), u32>>,
    /// The test hook of `NodeConfig::meta_blocked`.
    blocked: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The test hook of `NodeConfig::no_digests`.
    no_digests: Cell<bool>,
    /// Objects a deep scrub found damaged (a block failed its CRC), until a
    /// push replaces them; reported with the digest [`DAMAGED`].
    damaged: RefCell<HashSet<ObjKey>>,
    /// Cache-pool PGs (D45), with the epoch this device last served each
    /// at. Serving one at a new epoch, or for the first time since start,
    /// first drops what this device holds of it: the copy may be older
    /// than writes made elsewhere meanwhile, and a cache may lose data but
    /// must never serve stale data.
    cache_epochs: RefCell<HashMap<(u32, u32), u64>>,
    /// Counters for the admin port (`metrics`).
    metrics: Arc<metrics::ShardMetrics>,
}

fn err(code: Code, map: &ClusterMap, detail: impl Into<String>) -> Msg {
    Msg::Error {
        code,
        map_epoch: map.epoch,
        detail: detail.into(),
    }
}

/// What light scrub does about one object, given each acting member's
/// digest of it (this device's first; `None`: the member lacks it).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Agree,
    /// Push this copy to these members (indexes into the digests).
    Repair(Vec<usize>),
    /// No majority agrees with this copy: report, don't guess.
    Unresolved,
}

fn verdict(digests: &[Option<u64>]) -> Verdict {
    let mine = digests[0];
    if digests.iter().all(|d| *d == mine) {
        return Verdict::Agree;
    }
    let with_me = digests.iter().filter(|d| **d == mine).count();
    if with_me * 2 > digests.len() {
        Verdict::Repair((1..digests.len()).filter(|i| digests[*i] != mine).collect())
    } else {
        Verdict::Unresolved
    }
}

/// Each member's digest of `key`. A damaged copy gets a value of its own,
/// so it never agrees with another copy, damaged or not, and can't be part
/// of a majority.
fn row(table: &[HashMap<ObjKey, u64>], key: &ObjKey) -> Vec<Option<u64>> {
    table
        .iter()
        .enumerate()
        .map(|(i, m)| {
            m.get(key).map(|d| {
                if *d == DAMAGED {
                    u64::MAX - i as u64
                } else {
                    *d
                }
            })
        })
        .collect()
}

/// Every this-many scrub passes, the first included, is deep (replication.md
/// §8.5: light daily, deep weekly).
const DEEP_SCRUB_EVERY: u64 = 7;

/// How fast a deep scrub reads, per device, in bytes a second.
const DEEP_SCRUB_RATE: u64 = 64 << 20;

fn wire_key(k: &ObjKey) -> WireKey {
    WireKey {
        ns: k.ns as u8,
        name: k.name.clone(),
        version: k.version,
    }
}

fn engine_key(to: &Target, k: &WireKey) -> Result<ObjKey, ()> {
    let ns = Namespace::from_u8(k.ns).map_err(|_| ())?;
    Ok(ObjKey::new(to.pool, to.pg, ns, k.name.clone(), k.version))
}

impl ShardState {
    async fn open(
        node: u32,
        me: u32,
        dev: Arc<dyn BlockDev>,
        meta: SocketAddr,
        defer: Option<usize>,
        blocked: Option<Arc<std::sync::atomic::AtomicBool>>,
        metrics: Arc<metrics::ShardMetrics>,
    ) -> Result<Rc<Self>, String> {
        let mut shard = match hyperstor_engine::open(&dev) {
            Ok((_, mut v)) => v.pop().ok_or("device has no shard")?,
            Err(EngineError::NotFormatted) => {
                hyperstor_engine::format(
                    &dev,
                    &DeviceConfig {
                        device_id: me as u128,
                        cluster_id: 0,
                        shards: 1,
                        log_blocks: None,
                    },
                )
                .map_err(|e| e.to_string())?;
                hyperstor_engine::open(&dev)
                    .map_err(|e| e.to_string())?
                    .1
                    .pop()
                    .ok_or("device has no shard")?
            }
            Err(e) => return Err(e.to_string()),
        };
        shard.set_defer_max(defer.unwrap_or(0));
        let mut client = None;
        for _ in 0..100 {
            match Client::connect(meta, node as u64).await {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => sleep(Duration::from_millis(20)).await,
            }
        }
        let client = client.ok_or("map service unreachable")?;
        Ok(Rc::new(ShardState {
            me,
            shard: RefCell::new(shard),
            client,
            queue: RefCell::new(Vec::new()),
            committer: RefCell::new(None),
            seqs: RefCell::new(HashMap::new()),
            peers: RefCell::new(HashMap::new()),
            batches: Cell::new(0),
            backfills: RefCell::new(HashMap::new()),
            fill_inflight: RefCell::new(HashMap::new()),
            required: RefCell::new(std::collections::HashSet::new()),
            lease_until: Cell::new(None),
            fenced: RefCell::new(HashMap::new()),
            pg_inflight: RefCell::new(HashMap::new()),
            no_digests: Cell::new(false),
            damaged: RefCell::new(HashSet::new()),
            blocked,
            cache_epochs: RefCell::new(HashMap::new()),
            metrics,
        }))
    }

    fn map(&self) -> Rc<ClusterMap> {
        self.client.map()
    }

    /// Queues a list for the next group commit.
    fn enqueue(&self, list: OpList) -> impl Future<Output = CommitResult> + 'static {
        let (tx, rx) = oneshot();
        self.queue.borrow_mut().push((list, tx));
        if let Some(w) = self.committer.borrow_mut().take() {
            w.wake();
        }
        async move { rx.recv().await.unwrap_or(Err("commit dropped".into())) }
    }

    /// Commits everything queued now, as one batch, in queue order.
    fn commit_queued(&self) {
        let batch = std::mem::take(&mut *self.queue.borrow_mut());
        if batch.is_empty() {
            return;
        }
        let (lists, txs): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
        let res = self.shard.borrow_mut().commit(lists);
        self.batches.set(self.batches.get() + 1);
        match res {
            Ok(rs) => {
                for (tx, r) in txs.into_iter().zip(rs) {
                    let _ = tx.send(r.map_err(|e: OpError| e.to_string()));
                }
            }
            Err(e) => {
                for tx in txs {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        }
    }

    /// Commits whatever is queued, as one batch, whenever anything is.
    async fn committer(self: Rc<Self>) {
        loop {
            std::future::poll_fn(|cx| {
                if self.queue.borrow().is_empty() {
                    *self.committer.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            self.commit_queued();
            let pending = self.shard.borrow().pending_blocks();
            if pending > 4096 {
                let _ = self.shard.borrow_mut().apply_deferred();
            }
            // Let request handlers queue the next batch.
            hyperstor_reactor::yield_now().await;
        }
    }

    async fn follow_map(self: Rc<Self>) {
        loop {
            sleep(Duration::from_millis(200)).await;
            if !self.cut_off() {
                let _ = self.client.refresh_map().await;
            }
        }
    }

    async fn accept(self: Rc<Self>, l: TcpListener) {
        loop {
            match l.accept().await {
                Ok((s, _)) => {
                    spawn(self.clone().serve(s));
                }
                Err(_) => sleep(Duration::from_millis(10)).await,
            }
        }
    }

    /// Checks a request against the map: the PG exists, its epoch is the
    /// one the sender used, and this device is its primary (or a member).
    async fn check(&self, to: &Target, primary: bool) -> Result<PgInfo, Msg> {
        if to.map_epoch > self.map().epoch && !self.cut_off() {
            let _ = self.client.refresh_map().await;
        }
        let map = self.map();
        // No valid map lease: this node may have been replaced without
        // knowing it, so it serves nothing (replication.md §8.4).
        if self
            .lease_until
            .get()
            .is_none_or(|t| std::time::Instant::now() >= t)
        {
            return Err(err(Code::Unavailable, &map, "no map lease"));
        }
        if let Some(e) = self.fenced.borrow().get(&(to.pool, to.pg))
            && map.epoch < *e
        {
            return Err(err(Code::Unavailable, &map, "PG fenced for a map change"));
        }
        if map.pools.get(&to.pool).map(|p| p.kind) == Some(hyperstor_meta::PoolKind::Cache)
            && let Some(pg) = map.pg(to.pool, to.pg)
            && self.cache_epochs.borrow().get(&(to.pool, to.pg)) != Some(&pg.epoch)
        {
            self.cache_epochs
                .borrow_mut()
                .insert((to.pool, to.pg), pg.epoch);
            // Committed here, after anything already queued and before this
            // request goes on: reads don't pass through the queue.
            let keys = self.shard.borrow().list_pg(to.pool, to.pg);
            self.queue.borrow_mut().extend(keys.into_iter().map(|k| {
                let list = OpList {
                    key: k,
                    ops: vec![Op::Delete],
                };
                (list, oneshot().0)
            }));
            self.commit_queued();
        }
        let Some(pg) = map.pg(to.pool, to.pg) else {
            return Err(err(Code::Invalid, &map, "no such PG"));
        };
        if pg.epoch != to.pg_epoch {
            return Err(err(Code::StaleMap, &map, "PG epoch changed"));
        }
        let ok = if primary {
            pg.primary() == Some(self.me)
        } else {
            pg.acting.contains(&self.me) || pg.up.contains(&self.me)
        };
        if !ok {
            return Err(err(Code::NotPrimary, &map, "not primary for this PG"));
        }
        Ok(pg.clone())
    }

    async fn peer(&self, addr: SocketAddr) -> Result<Conn, ClientError> {
        if let Some(c) = self.peers.borrow().get(&addr)
            && !c.is_dead()
        {
            return Ok(c.clone());
        }
        let c = Conn::connect(addr).await?;
        self.peers.borrow_mut().insert(addr, c.clone());
        Ok(c)
    }

    /// Copies the engine's counters into the admin port's gauges, once a
    /// second: `stats()` is constant-time (I37).
    async fn gauges(self: Rc<Self>) {
        use std::sync::atomic::Ordering::Relaxed;
        let block = hyperstor_engine::BLOCK as u64;
        loop {
            {
                let st = self.shard.borrow().stats();
                let m = &self.metrics;
                m.objects.store(st.objects, Relaxed);
                m.free_bytes.store(st.free_blocks * block, Relaxed);
                m.data_bytes.store(st.data_blocks * block, Relaxed);
                m.log_used_bytes.store(st.log_used_blocks * block, Relaxed);
                m.log_bytes.store(st.log_blocks * block, Relaxed);
                m.commits.store(st.commits, Relaxed);
                m.checkpoints.store(st.checkpoints, Relaxed);
                m.map_epoch.store(self.map().epoch, Relaxed);
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn serve(self: Rc<Self>, s: TcpStream) {
        let s = Rc::new(s);
        let (tx, rx) = channel::<Vec<u8>>();
        spawn(writer(s.clone(), rx));
        loop {
            let Ok((h, msg, payload)) = read_frame(&s).await else {
                let _ = s.shutdown();
                return;
            };
            let reply = |m: &Msg, p: &[u8]| {
                let _ = tx.send(encode_frame(h.req, m, p));
            };
            match msg {
                Msg::Hello { .. } => reply(
                    &Msg::HelloOk {
                        version: VERSION,
                        node: self.me as u64,
                        map_epoch: self.map().epoch,
                    },
                    &[],
                ),
                Msg::Ping => reply(&Msg::Pong, &[]),
                Msg::Write { to, key, off, crcs } => {
                    metrics::ShardMetrics::add(&self.metrics.writes, 1);
                    metrics::ShardMetrics::add(&self.metrics.write_bytes, payload.len() as u64);
                    self.write(h.req, to, key, off, crcs, payload, tx.clone())
                        .await;
                }
                Msg::Repl {
                    to,
                    seq: _,
                    key,
                    off,
                    crcs,
                } => {
                    metrics::ShardMetrics::add(&self.metrics.repls, 1);
                    metrics::ShardMetrics::add(&self.metrics.repl_bytes, payload.len() as u64);
                    self.repl(h.req, to, key, off, crcs, payload, tx.clone())
                        .await
                }
                Msg::Read { to, key, off, len } => {
                    let arrived = std::time::Instant::now();
                    let (m, p) = self.read(to, key, off, len).await;
                    self.metrics.read_latency.observe(arrived.elapsed());
                    metrics::ShardMetrics::add(&self.metrics.reads, 1);
                    metrics::ShardMetrics::add(&self.metrics.read_bytes, p.len() as u64);
                    reply(&m, &p);
                }
                // Cut off from the map service (test hook): its messages
                // don't arrive.
                Msg::Fence { .. } | Msg::MapPush { .. } if self.cut_off() => {}
                Msg::Fence { epoch, pgs } => {
                    let me = self.clone();
                    let tx = tx.clone();
                    let req = h.req;
                    for k in &pgs {
                        let mut f = self.fenced.borrow_mut();
                        let e = f.entry(*k).or_default();
                        *e = (*e).max(epoch);
                    }
                    // Answer once every write already accepted for these
                    // PGs has been answered.
                    spawn(async move {
                        while pgs
                            .iter()
                            .any(|k| me.pg_inflight.borrow().get(k).copied().unwrap_or(0) > 0)
                        {
                            sleep(Duration::from_millis(1)).await;
                        }
                        let _ = tx.send(encode_frame(req, &Msg::FenceOk, &[]));
                    });
                }
                Msg::MapPush { map, .. } => {
                    if let Ok(m) = ClusterMap::decode(&map) {
                        self.client.install_map(m);
                    }
                    reply(&Msg::MapPushOk, &[]);
                }
                Msg::Digests { .. } if self.no_digests.get() => {
                    reply(&err(Code::Invalid, &self.map(), "unsupported request"), &[])
                }
                Msg::Digests { to, deep } => {
                    // Answered from a task of its own: a deep read takes a
                    // while, and replication on this connection goes on.
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let (m, p) = me.digests(to, deep).await;
                        let _ = tx.send(encode_frame(req, &m, &p));
                    });
                }
                Msg::Push { to, key, crcs } => {
                    metrics::ShardMetrics::add(&self.metrics.pushes, 1);
                    metrics::ShardMetrics::add(&self.metrics.push_bytes, payload.len() as u64);
                    self.push(h.req, to, key, crcs, payload, tx.clone()).await
                }
                _ => reply(&err(Code::Invalid, &self.map(), "unsupported request"), &[]),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn write(
        self: &Rc<Self>,
        req: u64,
        to: Target,
        key: WireKey,
        off: u64,
        crcs: Vec<u32>,
        data: Vec<u8>,
        tx: Sender<Vec<u8>>,
    ) {
        let arrived = std::time::Instant::now();
        // Every write is answered once: an error is counted here, success
        // is timed where it's answered.
        let counts = self.metrics.clone();
        let send = move |m: Msg| {
            if matches!(m, Msg::Error { .. }) {
                metrics::ShardMetrics::add(&counts.write_errors, 1);
            }
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        let pg = match self.check(&to, true).await {
            Ok(p) => p,
            Err(m) => return send(m),
        };
        if verify_crcs(&data, &crcs).is_err() {
            return send(err(Code::Checksum, &self.map(), "payload checksum"));
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return send(err(Code::Invalid, &self.map(), "bad namespace"));
        };
        // Connect to every replica first; then queue everything with no
        // wait in between, so every copy sees writes in the same order.
        // Acting members must all answer; backfill targets get the write
        // too (so they miss nothing after their copy), best effort.
        let map = self.map();
        if (pg.acting_count() as u8) < map.pools[&to.pool].min_size {
            return send(err(Code::Unavailable, &map, "too few copies"));
        }
        let mut conns = Vec::new();
        let required: Vec<u32> = pg
            .backfilling()
            .filter(|d| self.required.borrow().contains(&(to.pool, to.pg, *d)))
            .collect();
        for d in pg
            .acting
            .iter()
            .skip(1)
            .filter(|d| **d != NO_DEVICE)
            .chain(required.iter())
        {
            let Some(addr) = map.device_addr(*d) else {
                return send(err(Code::Unavailable, &map, "replica has no address"));
            };
            match self.peer(addr).await {
                Ok(c) => conns.push(c),
                Err(e) => return send(err(Code::Unavailable, &map, format!("replica: {e}"))),
            }
        }
        let mut fills = Vec::new();
        for d in pg.backfilling().filter(|d| !required.contains(d)) {
            match map.device_addr(d) {
                Some(addr) => match self.peer(addr).await {
                    Ok(c) => fills.push((d, c)),
                    Err(_) => self.fill_failed(to.pool, to.pg, d),
                },
                None => self.fill_failed(to.pool, to.pg, d),
            }
        }
        // Counted until answered, so a fence waits for it.
        *self
            .pg_inflight
            .borrow_mut()
            .entry((to.pool, to.pg))
            .or_default() += 1;
        let seq = {
            let mut seqs = self.seqs.borrow_mut();
            let s = seqs.entry((to.pool, to.pg)).or_default();
            *s += 1;
            *s
        };
        let repl = Msg::Repl {
            to,
            seq,
            key: key.clone(),
            off,
            crcs,
        };
        let remote: Vec<_> = conns.iter().map(|c| c.start(&repl, &data)).collect();
        let filling: Vec<_> = fills
            .iter()
            .map(|(d, c)| {
                *self
                    .fill_inflight
                    .borrow_mut()
                    .entry((to.pool, to.pg, *d))
                    .or_default() += 1;
                (*d, c.start(&repl, &data))
            })
            .collect();
        let local = self.enqueue(OpList {
            key: okey,
            ops: vec![Op::Write { off, data }],
        });
        let me = self.clone();
        let (pool, pgn) = (to.pool, to.pg);
        spawn(async move {
            for (d, f) in filling {
                if !matches!(f.await, Ok((Msg::ReplOk { .. }, _))) {
                    me.fill_failed(pool, pgn, d);
                }
                if let Some(n) = me.fill_inflight.borrow_mut().get_mut(&(pool, pgn, d)) {
                    *n -= 1;
                }
            }
        });
        let me2 = self.clone();
        spawn(async move {
            let r = local.await;
            let m = me2.metrics.clone();
            let _inflight = InflightGuard(me2, (to.pool, to.pg));
            let mut failed = None;
            for f in remote {
                match f.await {
                    Ok((Msg::ReplOk { .. }, _)) => {}
                    Ok((Msg::Error { detail, .. }, _)) => failed = Some(detail),
                    Ok((m, _)) => failed = Some(format!("unexpected {m:?}")),
                    Err(e) => failed = Some(e.to_string()),
                }
            }
            let ok = r.is_ok() && failed.is_none();
            send(match (r, failed) {
                (Ok(c), None) => Msg::WriteOk {
                    revision: c.revision,
                },
                (Err(e), _) => Msg::Error {
                    code: Code::Io,
                    map_epoch: 0,
                    detail: e,
                },
                (_, Some(e)) => Msg::Error {
                    code: Code::Unavailable,
                    map_epoch: 0,
                    detail: format!("replica: {e}"),
                },
            });
            if ok {
                m.write_latency.observe(arrived.elapsed());
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn repl(
        self: &Rc<Self>,
        req: u64,
        to: Target,
        key: WireKey,
        off: u64,
        crcs: Vec<u32>,
        data: Vec<u8>,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        if let Err(m) = self.check(&to, false).await {
            return send(m);
        }
        if verify_crcs(&data, &crcs).is_err() {
            return send(err(Code::Checksum, &self.map(), "payload checksum"));
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return send(err(Code::Invalid, &self.map(), "bad namespace"));
        };
        let local = self.enqueue(OpList {
            key: okey,
            ops: vec![Op::Write { off, data }],
        });
        spawn(async move {
            send(match local.await {
                Ok(c) => Msg::ReplOk {
                    revision: c.revision,
                },
                Err(e) => Msg::Error {
                    code: Code::Io,
                    map_epoch: 0,
                    detail: e,
                },
            });
        });
    }

    /// Notes that a backfill target missed a write or push.
    fn fill_failed(&self, pool: u32, pg: u32, device: u32) {
        if let Some(f) = self.backfills.borrow_mut().get_mut(&(pool, pg, device)) {
            *f = true;
        }
    }

    /// What this node holds of a PG, as `(key, digest)` in key order, once
    /// every commit queued so far is done: everything that reached this
    /// node before the request is in it (replication.md §8.5).
    async fn digests(&self, to: Target, deep: bool) -> (Msg, Vec<u8>) {
        if let Err(m) = self.check(&to, false).await {
            return (m, Vec::new());
        }
        if deep {
            self.verify_pg(to.pool, to.pg).await;
        }
        self.drained().await;
        let entries: Vec<_> = self
            .local_digests(to.pool, to.pg)
            .into_iter()
            .map(|(k, d)| (wire_key(&k), d))
            .collect();
        (Msg::DigestsOk, encode_digests(&entries))
    }

    /// This device's digests of a PG; [`DAMAGED`] for objects a deep scrub
    /// found damaged.
    fn local_digests(&self, pool: u32, pg: u32) -> Vec<(ObjKey, u64)> {
        let sh = self.shard.borrow();
        let damaged = self.damaged.borrow();
        sh.list_pg(pool, pg)
            .into_iter()
            .filter_map(|k| {
                let d = match sh.digest(&k)? {
                    _ if damaged.contains(&k) => DAMAGED,
                    DAMAGED => DAMAGED + 1,
                    d => d,
                };
                Some((k, d))
            })
            .collect()
    }

    /// Deep scrub of this device's copy of a PG: reads every object, which
    /// checks every block's CRC (engine.md §7.2), and marks the objects
    /// that fail. At most [`DEEP_SCRUB_RATE`] bytes a second, yielding
    /// between objects so I/O goes on.
    async fn verify_pg(&self, pool: u32, pg: u32) {
        let keys = self.shard.borrow().list_pg(pool, pg);
        for key in keys {
            let read = {
                let sh = self.shard.borrow();
                sh.stat(&key).map(|st| {
                    sh.read(&key, 0, st.size.div_ceil(4096) * 4096)
                        .map(|b| b.len())
                })
            };
            match read {
                Some(Ok(n)) => {
                    sleep(Duration::from_secs_f64(n as f64 / DEEP_SCRUB_RATE as f64)).await;
                }
                Some(Err(EngineError::Checksum { .. })) => {
                    if self.damaged.borrow_mut().insert(key) {
                        metrics::ShardMetrics::add(&self.metrics.scrub_damaged, 1);
                    }
                }
                _ => hyperstor_reactor::yield_now().await,
            }
        }
    }

    /// Replaces an object with a primary's copy (backfill, replication.md §8.5).
    /// Namespace 0 means: delete every object of the PG first.
    async fn push(
        self: &Rc<Self>,
        req: u64,
        to: Target,
        key: WireKey,
        crcs: Vec<u32>,
        data: Vec<u8>,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        if let Err(m) = self.check(&to, false).await {
            return send(m);
        }
        let lists: Vec<OpList> = if key.ns == 0 {
            self.shard
                .borrow()
                .list_pg(to.pool, to.pg)
                .into_iter()
                .map(|k| OpList {
                    key: k,
                    ops: vec![Op::Delete],
                })
                .collect()
        } else {
            if verify_crcs(&data, &crcs).is_err() {
                return send(err(Code::Checksum, &self.map(), "payload checksum"));
            }
            let Ok(okey) = engine_key(&to, &key) else {
                return send(err(Code::Invalid, &self.map(), "bad namespace"));
            };
            let mut ops = vec![Op::Delete];
            if !data.is_empty() {
                ops.push(Op::Write { off: 0, data });
            }
            vec![OpList { key: okey, ops }]
        };
        let replaced: Vec<ObjKey> = lists.iter().map(|l| l.key.clone()).collect();
        let done: Vec<_> = lists.into_iter().map(|l| self.enqueue(l)).collect();
        let me = self.clone();
        spawn(async move {
            let mut ok = true;
            for d in done {
                ok &= d.await.is_ok();
            }
            if ok {
                let mut damaged = me.damaged.borrow_mut();
                for k in &replaced {
                    damaged.remove(k);
                }
            }
            send(if ok {
                Msg::PushOk
            } else {
                Msg::Error {
                    code: Code::Io,
                    map_epoch: 0,
                    detail: "push failed".into(),
                }
            });
        });
    }

    /// True while the test hook cuts this node off from the map service.
    fn cut_off(&self) -> bool {
        self.blocked
            .as_ref()
            .is_some_and(|b| b.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Pings every node this reactor shares a PG with, every 100 ms, and
    /// reports to the map service any that stays silent for
    /// `peer_timeout` (D40). The map service marks a node down on reports
    /// from two distinct nodes.
    async fn peer_watch(self: Rc<Self>, node: u32, peer_timeout: Duration) {
        let last_ok: Rc<RefCell<HashMap<u32, std::time::Instant>>> = Rc::default();
        loop {
            sleep(Duration::from_millis(100)).await;
            let map = self.map();
            let mut peers: HashMap<u32, SocketAddr> = HashMap::new();
            for pg in map.pgs.values() {
                if !pg.acting.contains(&self.me) && !pg.up.contains(&self.me) {
                    continue;
                }
                for d in pg.acting.iter().chain(pg.up.iter()) {
                    if let Some(dev) = map.devices.get(d)
                        && dev.node != node
                        && map.nodes.get(&dev.node).is_some_and(|n| n.up)
                    {
                        peers.entry(dev.node).or_insert(dev.addr);
                    }
                }
            }
            let now = std::time::Instant::now();
            for (peer, addr) in peers {
                let first = *last_ok.borrow_mut().entry(peer).or_insert(now);
                if now.duration_since(first) > peer_timeout && !self.cut_off() {
                    let _ = self
                        .client
                        .call_meta(&Msg::PeerDown {
                            reporter: node,
                            peer,
                        })
                        .await;
                }
                let me = self.clone();
                let last_ok = last_ok.clone();
                spawn(async move {
                    let ping = async {
                        let c = me.peer(addr).await.ok()?;
                        c.call(&Msg::Ping, &[]).await.ok()
                    };
                    if let Ok(Some((Msg::Pong, _))) =
                        hyperstor_reactor::time::timeout(Duration::from_millis(200), ping).await
                    {
                        last_ok.borrow_mut().insert(peer, std::time::Instant::now());
                    }
                });
            }
        }
    }

    async fn heartbeat(self: Rc<Self>, node: u32) {
        loop {
            if !self.cut_off() {
                // The lease runs from when the heartbeat was sent, which is
                // before the map service granted it.
                let sent = std::time::Instant::now();
                if let Ok((
                    Msg::HeartbeatOk {
                        map_epoch,
                        lease_ms,
                    },
                    _,
                )) = self.client.call_meta(&Msg::Heartbeat { node }).await
                {
                    self.lease_until
                        .set(Some(sent + Duration::from_millis(lease_ms as u64)));
                    if map_epoch > self.map().epoch {
                        let _ = self.client.refresh_map().await;
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Starts a backfill for every PG this device is primary of that has
    /// members still to fill.
    async fn backfiller(self: Rc<Self>) {
        loop {
            sleep(Duration::from_millis(100)).await;
            let map = self.map();
            // Targets that are now acting members (or no longer wanted)
            // need no special treatment any more.
            self.required.borrow_mut().retain(|(pool, pg, d)| {
                map.pg(*pool, *pg)
                    .is_some_and(|i| i.backfilling().any(|x| x == *d))
            });
            for ((pool, pg), info) in &map.pgs {
                if info.primary() != Some(self.me) {
                    continue;
                }
                for target in info.backfilling() {
                    let k = (*pool, *pg, target);
                    if self.backfills.borrow().contains_key(&k) {
                        continue;
                    }
                    self.backfills.borrow_mut().insert(k, false);
                    spawn(self.clone().backfill(*pool, *pg, target, info.epoch));
                }
            }
        }
    }

    /// Waits until every queued commit is done, so the engine holds every
    /// write queued so far.
    async fn drained(&self) {
        while !self.queue.borrow().is_empty() {
            hyperstor_reactor::yield_now().await;
        }
    }

    async fn backfill(self: Rc<Self>, pool: u32, pg: u32, target: u32, pg_epoch: u64) {
        let k = (pool, pg, target);
        let mut reported = false;
        if self.backfill_once(pool, pg, target, pg_epoch).await {
            // From here on every write must reach the target. Then wait for
            // the best-effort writes already sent to it: if any failed, the
            // copy is incomplete and the backfill starts over.
            self.required.borrow_mut().insert(k);
            while self.fill_inflight.borrow().get(&k).copied().unwrap_or(0) > 0 {
                hyperstor_reactor::yield_now().await;
            }
            let clean = !self.backfills.borrow().get(&k).copied().unwrap_or(true);
            if clean {
                let r = self
                    .client
                    .call_meta(&Msg::BackfillDone {
                        pool,
                        pg,
                        pg_epoch,
                        device: target,
                    })
                    .await;
                reported = matches!(r, Ok((Msg::BackfillOk { .. }, _)));
                let _ = self.client.refresh_map().await;
            }
        }
        if !reported {
            self.required.borrow_mut().remove(&k);
            sleep(Duration::from_millis(200)).await;
        }
        // Allow a new attempt (or none, once the map says it's done).
        self.backfills.borrow_mut().remove(&k);
    }

    async fn backfill_once(&self, pool: u32, pg: u32, target: u32, pg_epoch: u64) -> bool {
        let map = self.map();
        let Some(addr) = map.device_addr(target) else {
            return false;
        };
        let Ok(conn) = self.peer(addr).await else {
            return false;
        };
        let to = Target {
            pool,
            pg,
            map_epoch: map.epoch,
            pg_epoch,
        };
        // Ask the target what it holds, then send what differs: objects
        // whose digests don't match, and objects it has that this copy
        // doesn't (sent as deletes). Each read and its push happen with no
        // wait in between, after every queued commit is done: writes queued
        // earlier are in the copy, and writes queued later follow it on the
        // same connection. A key that changes after the comparison is
        // carried by the live writes backfill targets get anyway.
        let theirs = match conn.start(&Msg::Digests { to, deep: false }, &[]).await {
            Ok((Msg::DigestsOk, p)) => match decode_digests(&p) {
                Ok(v) => v,
                Err(_) => return false,
            },
            // A node from before digests (a rolling upgrade, operations.md
            // §18.4): reset its copy and send everything, as backfill did.
            Ok((
                Msg::Error {
                    code: Code::Invalid,
                    ..
                },
                _,
            )) => {
                self.drained().await;
                let reset = conn.start(
                    &Msg::Push {
                        to,
                        key: WireKey {
                            ns: 0,
                            name: Vec::new(),
                            version: 0,
                        },
                        crcs: Vec::new(),
                    },
                    &[],
                );
                if !matches!(reset.await, Ok((Msg::PushOk, _))) {
                    return false;
                }
                Vec::new()
            }
            _ => return false,
        };
        let theirs: HashMap<ObjKey, u64> = theirs
            .into_iter()
            .filter_map(|(k, d)| Some((engine_key(&to, &k).ok()?, d)))
            .collect();
        self.drained().await;
        let keys: BTreeSet<ObjKey> = {
            let sh = self.shard.borrow();
            let ours = sh.list_pg(pool, pg);
            let mut differ: BTreeSet<ObjKey> = theirs
                .keys()
                .filter(|k| sh.stat(k).is_none())
                .cloned()
                .collect();
            differ.extend(
                ours.into_iter()
                    .filter(|k| theirs.get(k).copied() != sh.digest(k)),
            );
            differ
        };
        let mut inflight = Vec::new();
        for key in keys {
            if self.map().pg(pool, pg).map(|p| p.epoch) != Some(pg_epoch) {
                return false;
            }
            self.drained().await;
            let Some((m, data)) = self.push_of(to, &key) else {
                return false;
            };
            inflight.push(conn.start(&m, &data));
            if inflight.len() >= 16 {
                for f in inflight.drain(..) {
                    if !matches!(f.await, Ok((Msg::PushOk, _))) {
                        return false;
                    }
                }
            }
        }
        for f in inflight {
            if !matches!(f.await, Ok((Msg::PushOk, _))) {
                return false;
            }
        }
        true
    }

    /// The push of this node's copy of `key` (a delete if it has none).
    /// Read it and send it with no wait in between, once every queued
    /// commit is done ([`Self::drained`]): writes queued later then follow
    /// the push on the same connection.
    fn push_of(&self, to: Target, key: &ObjKey) -> Option<(Msg, Vec<u8>)> {
        let sh = self.shard.borrow();
        let data = match sh.stat(key) {
            Some(st) => sh
                .read(key, 0, st.size.div_ceil(4096) * 4096)
                .ok()?
                .to_vec(),
            None => Vec::new(),
        };
        let m = Msg::Push {
            to,
            key: wire_key(key),
            crcs: block_crcs(&data),
        };
        Some((m, data))
    }

    /// Scrubs every PG this device is primary of, once per `every`
    /// (replication.md §8.5, light scrub). Only clean PGs are scrubbed:
    /// backfill already compares a filling member's copy.
    async fn scrubber(self: Rc<Self>, every: Duration) {
        let mut pass = 0u64;
        loop {
            sleep(every).await;
            let deep = pass.is_multiple_of(DEEP_SCRUB_EVERY);
            pass += 1;
            let map = self.map();
            let pgs: Vec<((u32, u32), PgInfo)> = map
                .pgs
                .iter()
                .filter(|((pool, _), i)| {
                    i.primary() == Some(self.me)
                        && i.acting == i.up
                        && i.acting.len() > 1
                        && map.pools.get(pool).map(|p| p.kind)
                            != Some(hyperstor_meta::PoolKind::Cache)
                })
                .map(|(k, i)| (*k, i.clone()))
                .collect();
            for ((pool, pg), info) in pgs {
                self.scrub_pg(pool, pg, &info, deep).await;
            }
        }
    }

    /// Every acting member's digests of a PG, this device's first; `None`
    /// if a member can't answer for this PG epoch.
    async fn member_digests(
        &self,
        pool: u32,
        pg: u32,
        info: &PgInfo,
        deep: bool,
    ) -> Option<Vec<HashMap<ObjKey, u64>>> {
        let map = self.map();
        let to = Target {
            pool,
            pg,
            map_epoch: map.epoch,
            pg_epoch: info.epoch,
        };
        let mut out = Vec::new();
        for d in &info.acting {
            if *d == self.me {
                continue;
            }
            let conn = self.peer(map.device_addr(*d)?).await.ok()?;
            let Ok((Msg::DigestsOk, p)) = conn.call(&Msg::Digests { to, deep }, &[]).await else {
                return None;
            };
            out.push(
                decode_digests(&p)
                    .ok()?
                    .into_iter()
                    .filter_map(|(k, d)| Some((engine_key(&to, &k).ok()?, d)))
                    .collect(),
            );
        }
        if deep {
            self.verify_pg(pool, pg).await;
        }
        self.drained().await;
        out.insert(0, self.local_digests(pool, pg).into_iter().collect());
        Some(out)
    }

    /// Light scrub of one PG: compares every member's digests. Keys that
    /// differ are checked again after a pause, since a write in flight can
    /// make copies differ for a moment. A difference that persists is
    /// repaired from this copy when a majority of the copies agree with
    /// it, and otherwise reported, not guessed at.
    async fn scrub_pg(&self, pool: u32, pg: u32, info: &PgInfo, deep: bool) {
        let Some(first) = self.member_digests(pool, pg, info, deep).await else {
            return;
        };
        let differing = |t: &[HashMap<ObjKey, u64>], keys: &mut dyn Iterator<Item = ObjKey>| {
            keys.filter(|k| verdict(&row(t, k)) != Verdict::Agree)
                .collect::<BTreeSet<ObjKey>>()
        };
        let all: BTreeSet<ObjKey> = first.iter().flat_map(|m| m.keys().cloned()).collect();
        metrics::ShardMetrics::add(&self.metrics.scrub_objects, all.len() as u64);
        let suspects = differing(&first, &mut all.into_iter());
        if suspects.is_empty() {
            return;
        }
        sleep(Duration::from_millis(200)).await;
        if self.map().pg(pool, pg).map(|p| p.epoch) != Some(info.epoch) {
            return;
        }
        // Damage found by a deep read stays marked, so a light look will do.
        let Some(second) = self.member_digests(pool, pg, info, false).await else {
            return;
        };
        let bad = differing(&second, &mut suspects.into_iter());
        let map = self.map();
        let to = Target {
            pool,
            pg,
            map_epoch: map.epoch,
            pg_epoch: info.epoch,
        };
        let others: Vec<u32> = info
            .acting
            .iter()
            .copied()
            .filter(|d| *d != self.me)
            .collect();
        for key in bad {
            metrics::ShardMetrics::add(&self.metrics.scrub_inconsistent, 1);
            match verdict(&row(&second, &key)) {
                Verdict::Agree => {}
                Verdict::Repair(members) => {
                    let mut ok = true;
                    for m in members {
                        let Some(addr) = map.device_addr(others[m - 1]) else {
                            ok = false;
                            continue;
                        };
                        let Ok(conn) = self.peer(addr).await else {
                            ok = false;
                            continue;
                        };
                        self.drained().await;
                        let Some((m, data)) = self.push_of(to, &key) else {
                            ok = false;
                            continue;
                        };
                        ok &= matches!(conn.call(&m, &data).await, Ok((Msg::PushOk, _)));
                    }
                    if ok {
                        metrics::ShardMetrics::add(&self.metrics.scrub_repaired, 1);
                    }
                }
                Verdict::Unresolved => {
                    metrics::ShardMetrics::add(&self.metrics.scrub_unresolved, 1);
                    eprintln!(
                        "hyperstord: scrub: pool {pool} PG {pg}: copies of an object disagree \
                         and no majority decides; left as is (namespace {:?}, version {})",
                        key.ns, key.version
                    );
                }
            }
        }
    }

    async fn read(&self, to: Target, key: WireKey, off: u64, len: u64) -> (Msg, Vec<u8>) {
        if let Err(m) = self.check(&to, true).await {
            return (m, Vec::new());
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return (err(Code::Invalid, &self.map(), "bad namespace"), Vec::new());
        };
        let r = self.shard.borrow().read(&okey, off, len);
        let data = match r {
            Ok(b) => b.to_vec(),
            // A cache pool's miss is a miss: the caller recomputes (D45).
            Err(EngineError::NotFound)
                if self.map().pools.get(&to.pool).map(|p| p.kind)
                    == Some(hyperstor_meta::PoolKind::Cache) =>
            {
                return (
                    err(Code::NotFound, &self.map(), "not in the cache"),
                    Vec::new(),
                );
            }
            // Never written: reads as zeros.
            Err(EngineError::NotFound) => vec![0; len as usize],
            Err(EngineError::Checksum { lblock }) => {
                return (
                    err(Code::Checksum, &self.map(), format!("block {lblock}")),
                    Vec::new(),
                );
            }
            Err(e) => return (err(Code::Io, &self.map(), e.to_string()), Vec::new()),
        };
        (
            Msg::ReadOk {
                crcs: block_crcs(&data),
            },
            data,
        )
    }
}

/// Decrements a PG's count of unanswered writes when dropped.
struct InflightGuard(Rc<ShardState>, (u32, u32));

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(n) = self.0.pg_inflight.borrow_mut().get_mut(&self.1) {
            *n = n.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Verdict, verdict};

    #[test]
    fn scrub_repairs_only_from_a_majority() {
        assert_eq!(verdict(&[Some(1), Some(1), Some(1)]), Verdict::Agree);
        assert_eq!(verdict(&[None, None]), Verdict::Agree);
        // The primary and one other outvote the third, missing or not.
        assert_eq!(
            verdict(&[Some(1), Some(2), Some(1)]),
            Verdict::Repair(vec![1])
        );
        assert_eq!(verdict(&[Some(1), Some(1), None]), Verdict::Repair(vec![2]));
        // An object only the primary lacks is deleted only if a majority
        // lacks it too.
        assert_eq!(verdict(&[None, None, Some(3)]), Verdict::Repair(vec![2]));
        // The primary outvoted, or no majority: report, don't guess.
        assert_eq!(verdict(&[Some(1), Some(2), Some(2)]), Verdict::Unresolved);
        assert_eq!(verdict(&[Some(1), Some(2)]), Verdict::Unresolved);
        assert_eq!(verdict(&[Some(1), Some(2), Some(3)]), Verdict::Unresolved);
    }
}
