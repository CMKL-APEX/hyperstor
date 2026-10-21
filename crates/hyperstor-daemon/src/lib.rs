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
use hyperstor_client::volume::{
    LEASE_ATTR, decode_holder, encode_holder, header_deleting, header_version,
};
use hyperstor_client::{Client, ClientError};
use hyperstor_engine::{
    BlockDev, Committed, DeviceConfig, EngineError, FileDev, MemDev, Namespace, ObjKey, Op,
    OpError, OpList, Shard, UringDev,
};
use hyperstor_meta::{ClusterMap, NO_DEVICE, PgInfo};
use hyperstor_proto::{
    Code, DAMAGED, LEASE_EXPIRES, LEASE_LOST, LEASE_RENEWED, LEASE_RETRY, LEASE_RW, Looked, Msg,
    Target, VERSION, WireKey, WireObject, WireOp, block_crcs, decode_digests, decode_object,
    decode_ops, encode_digests, encode_frame, encode_looked, encode_object, encode_ops,
    verify_crcs,
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
    /// The map service's members (decisions.md D72): any one will do.
    pub meta: Vec<SocketAddr>,
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
        let (node, meta, defer) = (cfg.node_id, cfg.meta.clone(), cfg.defer_max);
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
    /// Objects an operation list is being decided for (`exec`).
    exec_locks: RefCell<HashSet<ObjKey>>,
    /// Volume leases of the headers this device is primary of (metadata.md
    /// §9, D75): the holder, until when it holds the lease, and the PG
    /// epoch that was decided at. The holder is in the header too; the
    /// expiry is kept only here, so a renewal writes nothing.
    leases: RefCell<HashMap<ObjKey, (u64, std::time::Instant, u64)>>,
    /// PGs this device has peered as primary (I61): the PG epoch, and
    /// whether peering is done (`false`: a request is peering it now).
    peered: RefCell<HashMap<(u32, u32), (u64, bool)>>,
    /// Cache-pool PGs (D45), with the epoch this device last served each
    /// at. Serving one at a new epoch, or for the first time since start,
    /// first drops what this device holds of it: the copy may be older
    /// than writes made elsewhere meanwhile, and a cache may lose data but
    /// must never serve stale data.
    cache_epochs: RefCell<HashMap<(u32, u32), u64>>,
    /// Counters for the admin port (`metrics`).
    metrics: Arc<metrics::ShardMetrics>,
}

fn blocklisted(map: &ClusterMap) -> Msg {
    err(
        Code::Blocklisted,
        map,
        "this client instance is blocklisted",
    )
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

/// The engine operation of an operation list's effect (assertions have
/// none: the primary decides them).
fn engine_op(op: WireOp) -> Option<Op> {
    Some(match op {
        WireOp::AssertExists | WireOp::AssertAbsent | WireOp::AssertAttr { .. } => return None,
        WireOp::Touch => Op::Touch,
        WireOp::Write { off, data } => Op::Write { off, data },
        WireOp::Zero { off, len } => Op::Zero { off, len },
        WireOp::Truncate(n) => Op::Truncate(n),
        WireOp::SetAttr(k, v) => Op::SetAttr(k, v),
        WireOp::RmAttr(k) => Op::RmAttr(k),
        WireOp::KvPut(k, v) => Op::KvPut(k, v),
        WireOp::KvDel(k) => Op::KvDel(k),
        WireOp::KvDelRange(a, b) => Op::KvDelRange(a, b),
        WireOp::Delete => Op::Delete,
    })
}

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
        meta: Vec<SocketAddr>,
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
            match Client::connect_members(meta.clone(), node as u64).await {
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
            exec_locks: RefCell::new(HashSet::new()),
            leases: RefCell::new(HashMap::new()),
            peered: RefCell::new(HashMap::new()),
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
        if primary
            && pg.acting.len() > 1
            && map.pools.get(&to.pool).map(|p| p.kind) != Some(hyperstor_meta::PoolKind::Cache)
            && !self.peer_pg(to.pool, to.pg, pg).await
        {
            return Err(err(
                Code::Unavailable,
                &map,
                "peering with the PG's members",
            ));
        }
        Ok(pg.clone())
    }

    /// Peering (replication.md §8.4, I61): before a primary serves a PG at
    /// a new epoch, its members' copies are made the same as its own, so
    /// a write that reached only some members when the previous primary
    /// failed (and so was never acknowledged) can't be read from one copy
    /// and later not from another. The first request peers; requests that
    /// arrive meanwhile wait for it. Whether the PG may be served.
    async fn peer_pg(&self, pool: u32, pg: u32, info: &PgInfo) -> bool {
        let k = (pool, pg);
        let t = std::time::Instant::now();
        loop {
            match self.peered.borrow().get(&k).copied() {
                Some((e, true)) if e == info.epoch => return true,
                Some((e, false)) if e == info.epoch => {}
                _ => break,
            }
            if t.elapsed() > Duration::from_secs(2) {
                return false;
            }
            sleep(Duration::from_millis(1)).await;
        }
        self.peered.borrow_mut().insert(k, (info.epoch, false));
        let ok = self.sync_members(pool, pg, info).await;
        if ok {
            self.peered.borrow_mut().insert(k, (info.epoch, true));
        } else {
            self.peered.borrow_mut().remove(&k);
        }
        ok
    }

    /// Pushes this device's copy of every object whose digest differs to
    /// the acting members that differ. Objects whose own copy is damaged
    /// are left to scrub.
    async fn sync_members(&self, pool: u32, pg: u32, info: &PgInfo) -> bool {
        let Some(table) = self.member_digests(pool, pg, info, false).await else {
            return false;
        };
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
        let keys: BTreeSet<ObjKey> = table.iter().flat_map(|m| m.keys().cloned()).collect();
        for key in keys {
            let mine = table[0].get(&key);
            if mine == Some(&DAMAGED) {
                continue;
            }
            for (i, m) in table.iter().enumerate().skip(1) {
                if m.get(&key) == mine {
                    continue;
                }
                let Some(addr) = map.device_addr(others[i - 1]) else {
                    return false;
                };
                let Ok(conn) = self.peer(addr).await else {
                    return false;
                };
                self.drained().await;
                let Some((msg, data)) = self.push_of(to, &key, false) else {
                    return false;
                };
                if !matches!(conn.call(&msg, &data).await, Ok((Msg::PushOk, _))) {
                    return false;
                }
                metrics::ShardMetrics::add(&self.metrics.peer_pushes, 1);
            }
        }
        true
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
        // The client instance named at `Hello`; 0 for peers, which don't
        // send one and are never blocklisted.
        let mut instance = 0;
        loop {
            let Ok((h, msg, payload)) = read_frame(&s).await else {
                let _ = s.shutdown();
                return;
            };
            let reply = |m: &Msg, p: &[u8]| {
                let _ = tx.send(encode_frame(h.req, m, p));
            };
            if let Msg::Hello { instance: i, .. } = msg {
                instance = i;
            }
            // A blocklisted instance gets nothing more (metadata.md §4.1).
            if self.map().blocked(instance) && !matches!(msg, Msg::Ping) {
                reply(&blocklisted(&self.map()), &[]);
                continue;
            }
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
                    self.write(h.req, instance, to, key, off, crcs, payload, tx.clone())
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
                Msg::Exec { to, key } => {
                    self.exec(h.req, instance, to, key, payload, tx.clone())
                        .await
                }
                Msg::Lease {
                    to,
                    key,
                    mode,
                    takeover,
                } => {
                    self.lease(h.req, instance, to, key, mode, takeover, tx.clone())
                        .await
                }
                Msg::LeaseRelease { to, key } => {
                    self.lease_release(h.req, instance, to, key, tx.clone())
                        .await
                }
                Msg::LeaseRenew {
                    pool,
                    map_epoch,
                    leases,
                } => {
                    // From a task of its own: one check per lease.
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let m = me.lease_renew(instance, pool, map_epoch, leases).await;
                        let _ = tx.send(encode_frame(req, &m, &[]));
                    });
                }
                Msg::ReplExec { to, key, .. } => {
                    self.repl_exec(h.req, to, key, payload, tx.clone()).await
                }
                Msg::Lookup {
                    to,
                    key,
                    kv_from,
                    kv_to,
                    kv_limit,
                } => {
                    let (m, p) = self.lookup(to, key, kv_from, kv_to, kv_limit).await;
                    reply(&m, &p);
                }
                Msg::PushObject { to, key } => {
                    metrics::ShardMetrics::add(&self.metrics.pushes, 1);
                    metrics::ShardMetrics::add(&self.metrics.push_bytes, payload.len() as u64);
                    self.push_object(h.req, to, key, payload, tx.clone()).await
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
        instance: u64,
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
        let (conns, fills) = match self.replicas(&to, &pg).await {
            Ok(c) => c,
            Err(m) => return send(m),
        };
        // Checked again after the last wait: a write queued from here on
        // comes after the map that blocklists its sender.
        if self.map().blocked(instance) {
            return send(blocklisted(&self.map()));
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

    /// Connections to a PG's replicas for a write: acting members, which
    /// must all answer, and backfill targets, which get it too (so they
    /// miss nothing after their copy), best effort. Refused when too few
    /// copies are acting.
    async fn replicas(
        &self,
        to: &Target,
        pg: &PgInfo,
    ) -> Result<(Vec<Conn>, Vec<(u32, Conn)>), Msg> {
        let map = self.map();
        if (pg.acting_count() as u8) < map.pools[&to.pool].min_size {
            return Err(err(Code::Unavailable, &map, "too few copies"));
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
                return Err(err(Code::Unavailable, &map, "replica has no address"));
            };
            match self.peer(addr).await {
                Ok(c) => conns.push(c),
                Err(e) => return Err(err(Code::Unavailable, &map, format!("replica: {e}"))),
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
        Ok((conns, fills))
    }

    /// An atomic operation list on one object (metadata.md §4.2). The
    /// primary decides the assertions against the object as it is once
    /// every queued commit is done, holding the key so no other list on it
    /// is decided meanwhile; then it queues the effects here and sends them
    /// to every replica with no wait in between, so every copy applies them
    /// in the same order as writes. Replicas apply effects only: revisions
    /// differ between copies, so assertions are on attributes (I6).
    async fn exec(
        self: &Rc<Self>,
        req: u64,
        instance: u64,
        to: Target,
        key: WireKey,
        payload: Vec<u8>,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        let Ok(ops) = decode_ops(&payload) else {
            return send(err(Code::Invalid, &self.map(), "malformed operation list"));
        };
        let decide = |me: &Self, okey: &ObjKey| match me.assertions_hold(okey, &ops) {
            Err(why) => Err(err(Code::Conflict, &me.map(), why)),
            Ok(()) => Ok(ops
                .iter()
                .filter(|o| !o.is_assertion())
                .cloned()
                .collect::<Vec<_>>()),
        };
        let done = self.decide_and_apply(instance, to, key, decide).await;
        spawn(async move {
            send(match done.await {
                Ok(()) => Msg::ExecOk,
                Err(m) => m,
            })
        });
    }

    /// Changes one object as its primary (metadata.md §4.2): holding the
    /// key, once every queued commit is done, `decide` looks at the object
    /// as it is and returns the effects to apply (or the answer, if
    /// none); then they are queued here and sent to every replica with no
    /// wait in between, so every copy applies them in the same order as
    /// writes. Replicas apply effects only: revisions differ between
    /// copies, so decisions rest on attributes (I6). The future returned
    /// resolves once every copy has them.
    async fn decide_and_apply(
        self: &Rc<Self>,
        instance: u64,
        to: Target,
        key: WireKey,
        decide: impl FnOnce(&Self, &ObjKey) -> Result<Vec<WireOp>, Msg>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), Msg>>>> {
        let fail = |m: Msg| -> std::pin::Pin<Box<dyn Future<Output = Result<(), Msg>>>> {
            Box::pin(async move { Err(m) })
        };
        let pg = match self.check(&to, true).await {
            Ok(p) => p,
            Err(m) => return fail(m),
        };
        let Ok(okey) = engine_key(&to, &key) else {
            return fail(err(Code::Invalid, &self.map(), "bad namespace"));
        };
        let (conns, fills) = match self.replicas(&to, &pg).await {
            Ok(c) => c,
            Err(m) => return fail(m),
        };
        while self.exec_locks.borrow().contains(&okey) {
            sleep(Duration::from_millis(1)).await;
        }
        self.exec_locks.borrow_mut().insert(okey.clone());
        self.drained().await;
        // From here to the queueing, no wait: nothing else reaches the
        // object in between.
        let unlock = |me: &Self| {
            me.exec_locks.borrow_mut().remove(&okey);
        };
        if self.map().blocked(instance) {
            unlock(self);
            return fail(blocklisted(&self.map()));
        }
        let effects = match decide(self, &okey) {
            Ok(e) => e,
            Err(m) => {
                unlock(self);
                return fail(m);
            }
        };
        if effects.is_empty() {
            unlock(self);
            return Box::pin(async { Ok(()) });
        }
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
        let repl = Msg::ReplExec { to, seq, key };
        let body = encode_ops(&effects);
        let remote: Vec<_> = conns.iter().map(|c| c.start(&repl, &body)).collect();
        let filling: Vec<_> = fills
            .iter()
            .map(|(d, c)| {
                *self
                    .fill_inflight
                    .borrow_mut()
                    .entry((to.pool, to.pg, *d))
                    .or_default() += 1;
                (*d, c.start(&repl, &body))
            })
            .collect();
        let local = self.enqueue(OpList {
            key: okey.clone(),
            ops: effects.into_iter().filter_map(engine_op).collect(),
        });
        // Queued: the next list on this key waits for it to commit.
        unlock(self);
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
        Box::pin(async move {
            let r = local.await;
            let _inflight = InflightGuard(me2, (pool, pgn));
            let mut failed = None;
            for f in remote {
                match f.await {
                    Ok((Msg::ReplOk { .. }, _)) => {}
                    Ok((Msg::Error { detail, .. }, _)) => failed = Some(detail),
                    Ok((m, _)) => failed = Some(format!("unexpected {m:?}")),
                    Err(e) => failed = Some(e.to_string()),
                }
            }
            match (r, failed) {
                (Ok(_), None) => Ok(()),
                (Err(e), _) => Err(Msg::Error {
                    code: Code::Io,
                    map_epoch: 0,
                    detail: e,
                }),
                (_, Some(e)) => Err(Msg::Error {
                    code: Code::Unavailable,
                    map_epoch: 0,
                    detail: format!("replica: {e}"),
                }),
            }
        })
    }

    /// Takes a volume's lease for `instance` as its header's primary
    /// (metadata.md §9, D75). Granted if nobody holds it, or `instance`
    /// does. Another holder's lease lasts [`LEASE_EXPIRES`] past its last
    /// renewal here, or past when this device first saw it (a new primary
    /// gives it a full lease); once it has expired, `takeover` breaks it,
    /// but only if the holder is blocklisted in this device's map.
    #[allow(clippy::too_many_arguments)]
    async fn lease(
        self: &Rc<Self>,
        req: u64,
        instance: u64,
        to: Target,
        key: WireKey,
        mode: u8,
        takeover: bool,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        if mode != LEASE_RW {
            // Shared `ro` leases are for snapshots (D18), which come with
            // HS4.
            return send(err(Code::Invalid, &self.map(), "only rw leases for now"));
        }
        if instance == 0 {
            return send(err(Code::Invalid, &self.map(), "no instance: send Hello"));
        }
        let version = Rc::new(Cell::new(0));
        let v2 = version.clone();
        let decide = move |me: &Self, okey: &ObjKey| {
            let sh = me.shard.borrow();
            let attrs = match sh.attrs(okey) {
                Some(a) if sh.stat(okey).is_some() => a,
                _ => return Err(err(Code::NotFound, &me.map(), "no such volume")),
            };
            v2.set(header_version(attrs));
            if header_deleting(attrs) {
                return Err(err(Code::Invalid, &me.map(), "the volume is being deleted"));
            }
            let holder = attrs.get(LEASE_ATTR).and_then(|h| decode_holder(h));
            let (pg_epoch, now) = (to.pg_epoch, std::time::Instant::now());
            let grant = |me: &Self| {
                me.leases
                    .borrow_mut()
                    .insert(okey.clone(), (instance, now + LEASE_EXPIRES, pg_epoch));
            };
            match holder {
                None => {}
                Some((h, _)) if h == instance => {
                    grant(me);
                    return Ok(Vec::new());
                }
                Some((h, _)) => {
                    let until = me.lease_until(okey, h, pg_epoch, now);
                    if now < until {
                        return Err(Msg::LeaseHeld {
                            holder: h,
                            remaining_ms: (until - now).as_millis().max(1) as u32,
                        });
                    }
                    if !(takeover && me.map().blocked(h)) {
                        return Err(Msg::LeaseHeld {
                            holder: h,
                            remaining_ms: 0,
                        });
                    }
                }
            }
            grant(me);
            Ok(vec![WireOp::SetAttr(
                LEASE_ATTR.to_vec(),
                encode_holder(instance, mode),
            )])
        };
        let okey = engine_key(&to, &key).ok();
        let done = self.decide_and_apply(instance, to, key, decide).await;
        let me = self.clone();
        spawn(async move {
            send(match done.await {
                Ok(()) => Msg::LeaseOk {
                    version: version.get(),
                },
                Err(m) => {
                    // Not known to have been written: the header decides
                    // again next time.
                    if !matches!(m, Msg::LeaseHeld { .. })
                        && let Some(k) = okey
                        && me.leases.borrow().get(&k).is_some_and(|l| l.0 == instance)
                    {
                        me.leases.borrow_mut().remove(&k);
                    }
                    m
                }
            })
        });
    }

    /// Until when `holder` holds the lease on `okey`, as this device knows
    /// it: from its last renewal here at this PG epoch; else a full lease
    /// from now, recorded (it may have renewed with another primary).
    fn lease_until(
        &self,
        okey: &ObjKey,
        holder: u64,
        pg_epoch: u64,
        now: std::time::Instant,
    ) -> std::time::Instant {
        let mut l = self.leases.borrow_mut();
        match l.get(okey) {
            Some((h, until, e)) if *h == holder && *e == pg_epoch => *until,
            _ => {
                l.insert(okey.clone(), (holder, now + LEASE_EXPIRES, pg_epoch));
                now + LEASE_EXPIRES
            }
        }
    }

    /// Gives up `instance`'s lease on a volume; nothing if it holds none.
    async fn lease_release(
        self: &Rc<Self>,
        req: u64,
        instance: u64,
        to: Target,
        key: WireKey,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        let version = Rc::new(Cell::new(0));
        let v2 = version.clone();
        let decide = move |me: &Self, okey: &ObjKey| {
            let sh = me.shard.borrow();
            let Some(attrs) = sh.attrs(okey) else {
                return Ok(Vec::new());
            };
            v2.set(header_version(attrs));
            let holder = attrs.get(LEASE_ATTR).and_then(|h| decode_holder(h));
            if holder.is_none_or(|(h, _)| h != instance) {
                return Ok(Vec::new());
            }
            me.leases.borrow_mut().remove(okey);
            Ok(vec![WireOp::RmAttr(LEASE_ATTR.to_vec())])
        };
        let done = self.decide_and_apply(instance, to, key, decide).await;
        spawn(async move {
            send(match done.await {
                Ok(()) => Msg::LeaseOk {
                    version: version.get(),
                },
                Err(m) => m,
            })
        });
    }

    /// Renews `instance`'s leases on headers this device is primary of
    /// (D75). Nothing is written: the expiry is kept in memory.
    async fn lease_renew(
        &self,
        instance: u64,
        pool: u32,
        map_epoch: u64,
        leases: Vec<(u32, u64, Vec<u8>)>,
    ) -> Msg {
        let mut results = Vec::with_capacity(leases.len());
        for (pg, pg_epoch, name) in leases {
            let to = Target {
                pool,
                pg,
                map_epoch,
                pg_epoch,
            };
            let key = WireKey {
                ns: Namespace::Meta as u8,
                name,
                version: 0,
            };
            let (Ok(_), Ok(okey)) = (self.check(&to, true).await, engine_key(&to, &key)) else {
                results.push((LEASE_RETRY, 0));
                continue;
            };
            let sh = self.shard.borrow();
            let attrs = sh.attrs(&okey).filter(|_| sh.stat(&okey).is_some());
            let version = attrs.map_or(0, header_version);
            let holder = attrs
                .and_then(|a| a.get(LEASE_ATTR))
                .and_then(|h| decode_holder(h));
            let mut l = self.leases.borrow_mut();
            // Granted to another here, and not yet written.
            let taken = l
                .get(&okey)
                .is_some_and(|(h, _, e)| *e == pg_epoch && *h != instance);
            if holder.is_some_and(|(h, _)| h == instance) && !taken {
                l.insert(
                    okey,
                    (
                        instance,
                        std::time::Instant::now() + LEASE_EXPIRES,
                        pg_epoch,
                    ),
                );
                results.push((LEASE_RENEWED, version));
            } else {
                results.push((LEASE_LOST, version));
            }
        }
        Msg::LeaseRenewOk { results }
    }

    /// Whether `ops`' assertions hold for the object as it is now.
    fn assertions_hold(&self, key: &ObjKey, ops: &[WireOp]) -> Result<(), String> {
        let sh = self.shard.borrow();
        let exists = sh.stat(key).is_some();
        for op in ops {
            match op {
                WireOp::AssertExists if !exists => return Err("the object doesn't exist".into()),
                WireOp::AssertAbsent if exists => return Err("the object exists".into()),
                WireOp::AssertAttr { name, value } => {
                    let have = sh.attrs(key).and_then(|a| a.get(name));
                    if have != value.as_ref() {
                        return Err(format!(
                            "attribute {} is not as asserted",
                            String::from_utf8_lossy(name)
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A replica's part of [`Self::exec`]: the effects, in order.
    async fn repl_exec(
        self: &Rc<Self>,
        req: u64,
        to: Target,
        key: WireKey,
        payload: Vec<u8>,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        if let Err(m) = self.check(&to, false).await {
            return send(m);
        }
        let Ok(ops) = decode_ops(&payload) else {
            return send(err(Code::Invalid, &self.map(), "malformed operation list"));
        };
        let Ok(okey) = engine_key(&to, &key) else {
            return send(err(Code::Invalid, &self.map(), "bad namespace"));
        };
        let local = self.enqueue(OpList {
            key: okey,
            ops: ops.into_iter().filter_map(engine_op).collect(),
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

    /// An object's size, attributes and a range of its key–value table,
    /// from the primary.
    async fn lookup(
        &self,
        to: Target,
        key: WireKey,
        kv_from: Vec<u8>,
        kv_to: Option<Vec<u8>>,
        kv_limit: u32,
    ) -> (Msg, Vec<u8>) {
        if let Err(m) = self.check(&to, true).await {
            return (m, Vec::new());
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return (err(Code::Invalid, &self.map(), "bad namespace"), Vec::new());
        };
        let sh = self.shard.borrow();
        let Some(st) = sh.stat(&okey) else {
            return (Msg::LookupOk { found: false }, Vec::new());
        };
        let looked = Looked {
            size: st.size,
            attrs: sh
                .attrs(&okey)
                .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
            kv: sh.kv_scan(&okey, &kv_from, kv_to.as_deref(), kv_limit as usize),
        };
        (Msg::LookupOk { found: true }, encode_looked(&looked))
    }

    /// Replaces an object with a primary's whole copy: refill, repair and
    /// peering (replication.md §8.5).
    async fn push_object(
        self: &Rc<Self>,
        req: u64,
        to: Target,
        key: WireKey,
        payload: Vec<u8>,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        if let Err(m) = self.check(&to, false).await {
            return send(m);
        }
        let Ok(obj) = decode_object(&payload) else {
            return send(err(
                Code::Checksum,
                &self.map(),
                "malformed or damaged object",
            ));
        };
        let Ok(okey) = engine_key(&to, &key) else {
            return send(err(Code::Invalid, &self.map(), "bad namespace"));
        };
        // In lists the engine accepts: the first replaces the data and
        // attributes, the rest carry the key–value table in batches. Reads
        // come from the primary, so a member between two lists serves no
        // one the half-copied object.
        let limit = self.shard.borrow().max_list_bytes() / 2;
        let mut first = vec![Op::Delete];
        if !obj.data.is_empty() {
            first.push(Op::Write {
                off: 0,
                data: obj.data,
            });
        }
        first.push(Op::Truncate(obj.size));
        first.extend(obj.attrs.into_iter().map(|(k, v)| Op::SetAttr(k, v)));
        let mut lists = vec![OpList {
            key: okey.clone(),
            ops: first,
        }];
        let mut batch = Vec::new();
        let mut bytes = 0;
        for (k, v) in obj.kv {
            bytes += k.len() + v.len() + 32;
            batch.push(Op::KvPut(k, v));
            if bytes >= limit {
                lists.push(OpList {
                    key: okey.clone(),
                    ops: std::mem::take(&mut batch),
                });
                bytes = 0;
            }
        }
        if !batch.is_empty() {
            lists.push(OpList {
                key: okey.clone(),
                ops: batch,
            });
        }
        let done: Vec<_> = lists.into_iter().map(|l| self.enqueue(l)).collect();
        let me = self.clone();
        spawn(async move {
            let mut ok = true;
            for d in done {
                ok &= d.await.is_ok();
            }
            if ok {
                me.damaged.borrow_mut().remove(&okey);
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
                    if map_epoch > self.map().epoch {
                        let _ = self.client.refresh_map().await;
                    }
                    // A lease is taken only with a map at least as new as
                    // the one published when it was granted, which may
                    // blocklist a client (metadata.md §9): the map service
                    // counts on it. A revocation applies at once.
                    if lease_ms == 0 || self.map().epoch >= map_epoch {
                        self.lease_until
                            .set(Some(sent + Duration::from_millis(lease_ms as u64)));
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
        let mut legacy = false;
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
                legacy = true;
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
            let Some((m, data)) = self.push_of(to, &key, legacy) else {
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
    fn push_of(&self, to: Target, key: &ObjKey, legacy: bool) -> Option<(Msg, Vec<u8>)> {
        let sh = self.shard.borrow();
        let Some(st) = sh.stat(key) else {
            // Absent: a delete, which every version understands.
            let m = Msg::Push {
                to,
                key: wire_key(key),
                crcs: Vec::new(),
            };
            return Some((m, Vec::new()));
        };
        let data = sh
            .read(key, 0, st.size.div_ceil(4096) * 4096)
            .ok()?
            .to_vec();
        if legacy {
            // A node from before whole objects (a rolling upgrade): data
            // only, as before.
            let m = Msg::Push {
                to,
                key: wire_key(key),
                crcs: block_crcs(&data),
            };
            return Some((m, data));
        }
        let obj = WireObject {
            size: st.size,
            data,
            attrs: sh
                .attrs(key)
                .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
            kv: sh.kv_scan(key, b"", None, usize::MAX),
        };
        let m = Msg::PushObject {
            to,
            key: wire_key(key),
        };
        Some((m, encode_object(&obj)))
    }

    /// Scrubs every PG this device is primary of, once per `every`
    /// (replication.md §8.5), spread evenly over the interval rather than
    /// back to back. Only clean PGs are scrubbed: backfill already
    /// compares a filling member's copy.
    async fn scrubber(self: Rc<Self>, every: Duration) {
        let mut pass = 0u64;
        loop {
            let deep = pass.is_multiple_of(DEEP_SCRUB_EVERY);
            pass += 1;
            let pgs: Vec<(u32, u32)> = self
                .map()
                .pgs
                .iter()
                .filter(|(k, _)| self.scrubs(**k).is_some())
                .map(|(k, _)| *k)
                .collect();
            if pgs.is_empty() {
                sleep(every).await;
                continue;
            }
            let gap = every / pgs.len() as u32;
            for (pool, pg) in pgs {
                sleep(gap).await;
                // The map may have changed while waiting.
                if let Some(info) = self.scrubs((pool, pg)) {
                    self.scrub_pg(pool, pg, &info, deep).await;
                }
            }
        }
    }

    /// The PG's info if this device scrubs it now: its primary, clean,
    /// with more than one copy, and not a cache pool's.
    fn scrubs(&self, (pool, pg): (u32, u32)) -> Option<PgInfo> {
        let map = self.map();
        let i = map.pg(pool, pg)?;
        (i.primary() == Some(self.me)
            && i.acting == i.up
            && i.acting.len() > 1
            && map.pools.get(&pool).map(|p| p.kind) != Some(hyperstor_meta::PoolKind::Cache))
        .then(|| i.clone())
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
        // `None`: a member from before digests (a rolling upgrade), which
        // can't be compared and is taken to agree, as before digests.
        let mut theirs: Vec<Option<HashMap<ObjKey, u64>>> = Vec::new();
        for d in &info.acting {
            if *d == self.me {
                continue;
            }
            let conn = self.peer(map.device_addr(*d)?).await.ok()?;
            match conn.call(&Msg::Digests { to, deep }, &[]).await {
                Ok((Msg::DigestsOk, p)) => theirs.push(Some(
                    decode_digests(&p)
                        .ok()?
                        .into_iter()
                        .filter_map(|(k, d)| Some((engine_key(&to, &k).ok()?, d)))
                        .collect(),
                )),
                Ok((
                    Msg::Error {
                        code: Code::Invalid,
                        ..
                    },
                    _,
                )) => theirs.push(None),
                _ => return None,
            }
        }
        if deep {
            self.verify_pg(pool, pg).await;
        }
        self.drained().await;
        let mine: HashMap<ObjKey, u64> = self.local_digests(pool, pg).into_iter().collect();
        let mut out = vec![mine.clone()];
        out.extend(
            theirs
                .into_iter()
                .map(|t| t.unwrap_or_else(|| mine.clone())),
        );
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
                        let Some((m, data)) = self.push_of(to, &key, false) else {
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
