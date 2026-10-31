//! `hyperstord`: a storage node (spec/08-engine.md §6, §8).
//!
//! A node runs one reactor thread per engine shard (01-implementation-notes.md
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

// An `Err` here is the reply to send (a protocol message), made once per
// request: its size costs nothing worth boxing it for.
#![allow(clippy::result_large_err)]

pub mod diag;
pub mod faults;
pub mod metrics;
mod shard_cell;

use hyperstor_client::rpc::{CALL_TIMEOUT, Conn, read_frame, writer};
use hyperstor_client::volume::{
    DELEGATES_ATTR, Holder, LEASE_ATTR, decode_delegates, decode_holder, encode_delegates,
    encode_holder, header_deleting, header_version,
};
use hyperstor_client::{Client, ClientError, branch_name};
use hyperstor_engine::{
    BlockDev, Committed, DeviceConfig, DigestPart, EngineError, FileDev, MemDev, Namespace, ObjKey,
    Op, OpError, OpList, UringDev,
};
use hyperstor_meta::{ClusterMap, NO_DEVICE, PgInfo};
use hyperstor_proto::{
    Code, DAMAGED, LEASE_EXPIRES, LEASE_LOST, LEASE_RENEWED, LEASE_RETRY, LEASE_RW, Looked, Msg,
    Target, VERSION, WATCH_CHANGED, WATCH_QUIESCE, WireKey, WireObject, WireOp, block_crcs,
    decode_digests, decode_object, decode_ops, encode_digests, encode_frame, encode_looked,
    encode_object, encode_ops, verify_crcs,
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
    /// The map service's members (04-decisions.md D72): any one will do.
    pub meta: Vec<SocketAddr>,
    pub shards: Vec<ShardSpec>,
    /// Pin reactor `i` to CPU `cpus[i]`, if given.
    pub cpus: Vec<usize>,
    /// Adaptive polling window for this node's reactors (04-decisions.md D41);
    /// zero turns it off.
    pub spin: Duration,
    /// Report a peer to the map service after this long without an answer
    /// to pings (04-decisions.md D40); zero turns peer reports off.
    pub peer_timeout: Duration,
    /// How often each primary scrubs its PGs, comparing every member's
    /// digests (09-replication.md §8.5); zero turns scrub off.
    pub scrub_interval: Duration,
    /// Test hook: answer `Digests` as a node from before digests does
    /// ("unsupported request"), as in a rolling upgrade.
    pub no_digests: bool,
    /// Test hook: while set, this node neither reaches the map service nor
    /// hears from it (no heartbeats, map fetches, pushes or fences), as if
    /// cut off from it while clients and peers can still reach it.
    pub meta_blocked: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Engine deferral threshold: `None` for flash, which redirects every
    /// write (04-decisions.md D38); `Some(hyperstor_engine::DEFER_MAX)` for
    /// hard disks.
    pub defer_max: Option<usize>,
    /// Test hook: node pairs that can't reach each other (04-decisions.md
    /// D92); this node's reactors send nothing to a node it is cut from.
    pub partitions: Option<Arc<faults::Partitions>>,
    /// The most bytes of an object's key–value table one push carries
    /// before the rest follows in parts ([`Msg::PushKv`]); zero: a quarter
    /// of the frame limit ([`PUSH_PART_BYTES`]). Tests set it small.
    pub push_part_bytes: usize,
    /// How long a commit round's writes and flush may take before the
    /// device is failed (I199); zero: [`IO_TIMEOUT`].
    pub io_timeout: Duration,
    /// Test hook: devices whose commits take longer without holding their
    /// reactor (04-decisions.md D92, I199).
    pub commit_delays: Option<Arc<faults::CommitDelays>>,
}

/// Objects digested or compared between two yields (I183): digests of
/// large objects take long, and a reactor that doesn't yield misses its
/// peers' pings.
const DIGEST_BATCH: usize = 64;

/// Bytes of a large key–value table hashed between yields (I188): at
/// most 1.5 ms of the reactor on the pilot's hosts.
const DIGEST_PART: usize = 1 << 20;

/// Bytes of pushes a reactor's backfills, together, have on the wire to
/// one target before they wait for answers (I187); one object may go
/// past it. Every PG backfill shares the connection to its target with
/// replication: unbounded, a refilled node had hundreds of MiB queued
/// ahead of each write.
const BACKFILL_BUDGET: usize = 16 << 20;

/// Pushes one PG backfill has on the wire before it waits for answers.
const BACKFILL_WINDOW: usize = 16;

/// How long a reactor's thread may be held before it is logged (I188):
/// a third of the production peer timeout (300 ms), so a stall that gets
/// a node reported silent is always in the log.
pub const STALL: Duration = Duration::from_millis(100);

/// How long a commit round's writes and flush may take before the device
/// is failed (I199). Before, a hung device held its reactor, so the node
/// went down; now the reactor goes on, and this ends the wait. Linux gives
/// an NVMe command 30 s before it aborts and retries it.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a device reports its space to the map service (D156).
const SPACE_EVERY: Duration = Duration::from_secs(5);

/// One backfill's bytes in its target's [`BACKFILL_BUDGET`], given back
/// once answered, or when the backfill gives up.
struct Held<'a> {
    st: &'a ShardState,
    target: u32,
    bytes: Cell<usize>,
}

impl Held<'_> {
    fn add(&self, n: usize) {
        self.bytes.set(self.bytes.get() + n);
        *self
            .st
            .backfill_bytes
            .borrow_mut()
            .entry(self.target)
            .or_default() += n;
    }

    fn release(&self) {
        let n = self.bytes.replace(0);
        let mut b = self.st.backfill_bytes.borrow_mut();
        if let Some(v) = b.get_mut(&self.target) {
            *v -= n;
            if *v == 0 {
                b.remove(&self.target);
            }
        }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// An object's key–value table goes in parts of about this many bytes, so
/// that no push exceeds the frame limit ([`hyperstor_proto::MAX_PAYLOAD`]).
pub const PUSH_PART_BYTES: usize = hyperstor_proto::MAX_PAYLOAD / 4;

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
    // Devices still opening: the node heartbeats once none is (I188).
    let opening = Arc::new(std::sync::atomic::AtomicUsize::new(cfg.shards.len()));
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
        let push_part_bytes = match cfg.push_part_bytes {
            0 => PUSH_PART_BYTES,
            n => n,
        };
        let partitions = cfg.partitions.clone();
        let commit_delays = cfg.commit_delays.clone();
        let io_timeout = match cfg.io_timeout {
            Duration::ZERO => IO_TIMEOUT,
            t => t,
        };
        let opened = Opening(Some(opening.clone()));
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
                        let began = std::time::Instant::now();
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
                        st.push_part_bytes.set(push_part_bytes);
                        *st.partitions.borrow_mut() = partitions.map(|p| (node, p));
                        *st.commit_delays.borrow_mut() = commit_delays;
                        st.io_timeout.set(io_timeout);
                        let l = TcpListener::from_std(sh.listener);
                        eprintln!(
                            "hyperstord: device {} open in {} ms",
                            sh.device_id,
                            began.elapsed().as_millis()
                        );
                        let all_open = opened.0.clone().unwrap();
                        drop(opened);
                        spawn(st.clone().stall_watch());
                        spawn(st.clone().committer());
                        spawn(st.deadlines.clone().ticker());
                        spawn(st.clone().follow_map());
                        spawn(st.clone().heartbeat(node, all_open));
                        spawn(st.clone().space());
                        if !peer_timeout.is_zero() {
                            spawn(st.clone().peer_watch(node, peer_timeout));
                        }
                        spawn(st.clone().backfiller());
                        if !scrub_interval.is_zero() {
                            spawn(st.clone().scrubber(scrub_interval));
                        }
                        spawn(st.clone().accept(l));
                        spawn(st.clone().gauges());
                        // Every 30 s (HS_PG_USAGE_MS for tests).
                        let every = std::env::var("HS_PG_USAGE_MS")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .map_or(Duration::from_secs(30), Duration::from_millis);
                        spawn(st.clone().usage(every));
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

/// Counts a device out of those still opening when dropped: once open,
/// or when it couldn't be (its reactor's thread then ends).
struct Opening(Option<Arc<std::sync::atomic::AtomicUsize>>);

impl Drop for Opening {
    fn drop(&mut self) {
        if let Some(n) = self.0.take() {
            n.fetch_sub(1, std::sync::atomic::Ordering::Release);
        }
    }
}

type CommitResult = Result<Committed, String>;

/// One reactor's state.
struct ShardState {
    me: u32,
    shard: shard_cell::ShardCell,
    /// The shard's device, which commits write through (I199).
    dev: Arc<dyn BlockDev>,
    /// `NodeConfig::io_timeout`, resolved.
    io_timeout: Cell<Duration>,
    /// The test hook of `NodeConfig::commit_delays`.
    commit_delays: RefCell<Option<Arc<faults::CommitDelays>>>,
    client: Rc<Client>,
    queue: RefCell<Vec<(OpList, OneSender<CommitResult>)>>,
    committer: RefCell<Option<Waker>>,
    /// Per-PG replication sequence numbers (primary side).
    seqs: RefCell<HashMap<(u32, u32), u64>>,
    /// Connections to replicas, by address.
    /// Connections to peers, one live each (I184).
    peers: hyperstor_client::rpc::Pool,
    /// Connections that carry only pings, one live each per peer (I187):
    /// on the peer's data connection a ping waits behind every push and
    /// replication queued before it, and liveness should not depend on how
    /// much data is on its way.
    pings: hyperstor_client::rpc::Pool,
    batches: Cell<u64>,
    /// Backfills this primary is running: (pool, pg, target) -> whether a
    /// write or push to the target failed meanwhile (the copy may then be
    /// incomplete, so the backfill starts over).
    backfills: RefCell<HashMap<(u32, u32, u32), bool>>,
    /// Bytes of backfill pushes on the wire, by target device
    /// ([`BACKFILL_BUDGET`]).
    backfill_bytes: RefCell<HashMap<u32, usize>>,
    /// Best-effort writes still in flight to each backfill target.
    fill_inflight: RefCell<HashMap<(u32, u32, u32), u32>>,
    /// Backfill targets whose copy is complete (or being reported): every
    /// write must reach them from now on, because the map service may make
    /// them acting members at any moment.
    required: RefCell<std::collections::HashSet<(u32, u32, u32)>>,
    /// Until when this reactor may serve PG I/O (its map lease, 04-decisions.md
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
    /// `NodeConfig::push_part_bytes`, resolved.
    push_part_bytes: Cell<usize>,
    /// Objects a deep scrub found damaged (a block failed its CRC), until a
    /// push replaces them; reported with the digest [`DAMAGED`].
    damaged: RefCell<HashSet<ObjKey>>,
    /// Objects an operation list is being decided for (`exec`).
    exec_locks: RefCell<HashSet<ObjKey>>,
    /// Volume leases of the headers this device is primary of (06-metadata.md
    /// §9, D75): the holder, until when it holds the lease, and the PG
    /// epoch that was decided at. The holder is in the header too; the
    /// expiry is kept only here, so a renewal writes nothing.
    leases: RefCell<HashMap<ObjKey, (u64, std::time::Instant, u64)>>,
    /// Deadlines on the I/O path (`REPL_TIMEOUT`).
    deadlines: Rc<Deadlines>,
    /// Replicas reported too slow: (pool, pg, PG epoch, device).
    slow_reported: RefCell<HashSet<(u32, u32, u64, u32)>>,
    /// The engine can no longer write this device (an I/O error poisoned
    /// it): nothing is served, and the map service is told (`heartbeat`).
    failed: Cell<bool>,
    failure_reported: Cell<bool>,
    /// Lease holders waiting for news of a header (`Watch`, D103).
    watches: RefCell<HashMap<ObjKey, Vec<Watcher>>>,
    /// Snapshots waiting for a header's holder to quiesce: the token, the
    /// snapshot's wait for the drain, and the holder's wait to resume.
    quiesce: RefCell<HashMap<ObjKey, Quiesce>>,
    /// The test hook of `NodeConfig::partitions`, with this node's id.
    partitions: RefCell<Option<(u32, Arc<faults::Partitions>)>>,
    /// Objects the last scrub of each PG left unresolved, as reported.
    inconsistent: RefCell<HashMap<(u32, u32), u32>>,
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

/// Coarse deadlines for the I/O path: one ticker per reactor wakes them
/// every `TICK`. A reactor timer is an io_uring timeout, one submission
/// per request that stays in the kernel until it fires; at thousands of
/// writes a second that cost about 3% of write throughput.
#[derive(Default)]
struct Deadlines {
    next: Cell<u64>,
    waiting: RefCell<std::collections::BTreeMap<(std::time::Instant, u64), Waker>>,
}

impl Deadlines {
    const TICK: Duration = Duration::from_millis(50);

    /// Resolves at `at`, or up to a tick later.
    fn at(&self, at: std::time::Instant) -> Expiry<'_> {
        let id = self.next.get();
        self.next.set(id + 1);
        Expiry {
            d: self,
            key: (at, id),
            registered: false,
        }
    }

    async fn ticker(self: Rc<Self>) {
        loop {
            sleep(Self::TICK).await;
            let now = std::time::Instant::now();
            let due: Vec<Waker> = {
                let mut w = self.waiting.borrow_mut();
                let later = w.split_off(&(now, u64::MAX));
                std::mem::replace(&mut *w, later).into_values().collect()
            };
            for w in due {
                w.wake();
            }
        }
    }
}

struct Expiry<'a> {
    d: &'a Deadlines,
    key: (std::time::Instant, u64),
    registered: bool,
}

impl Future for Expiry<'_> {
    type Output = ();
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        if std::time::Instant::now() >= self.key.0 {
            return Poll::Ready(());
        }
        self.d
            .waiting
            .borrow_mut()
            .insert(self.key, cx.waker().clone());
        self.registered = true;
        Poll::Pending
    }
}

impl Drop for Expiry<'_> {
    fn drop(&mut self) {
        if self.registered {
            self.d.waiting.borrow_mut().remove(&self.key);
        }
    }
}

/// How long a holder stays held for a consistency group's snapshot
/// before it resumes by itself.
pub const HOLD_AT_MOST: Duration = Duration::from_secs(5);

/// A chunk's name without a branch: lineage (16 bytes) and index (8).
const CHUNK_NAME_LEN: usize = 24;

/// How long a snapshot waits for an attached volume's holder to quiesce
/// before it is abandoned (10-volumes.md §10.3).
pub const QUIESCE_WITHIN: Duration = Duration::from_secs(2);

/// A holder's pending watch: answered with an event and a token.
type Watcher = OneSender<(u8, u64)>;

/// A snapshot waiting on a holder (`ShardState::quiesce`).
struct Quiesce {
    token: u64,
    /// The identities yet to drain: the holder's and its delegates'
    /// (D151).
    pending: Vec<u64>,
    drained: Option<OneSender<()>>,
    resume: Vec<OneSender<()>>,
}

/// How long a primary waits for a replica to answer a replicated change
/// before reporting it too slow for the PG (04-decisions.md D91).
pub const REPL_TIMEOUT: Duration = Duration::from_secs(2);

/// The code a failed commit is answered with: a shard out of space is
/// `NoSpace` (clients report it as such, `hyperstor-ublk` as `ENOSPC`);
/// anything else is `Io`. Commit errors travel as text (`CommitResult`).
fn commit_code(e: &str) -> Code {
    if e == OpError::NoSpace.to_string() {
        Code::NoSpace
    } else {
        Code::Io
    }
}

fn blocklisted(map: &ClusterMap) -> Msg {
    err(
        Code::Blocklisted,
        map,
        "this client instance is blocklisted",
    )
}

/// The refusal for a request from `instance` under lease identity `lease`
/// (D150), if either is blocklisted: the whole client, or that lease.
/// Peers (instance 0) are never refused: a primary decides for its
/// replicas, which get its forwarded requests with the client's identity.
fn fenced(map: &ClusterMap, instance: u64, lease: u64) -> Option<Msg> {
    if instance == 0 || map.blocklist.is_empty() {
        None
    } else if map.blocked(instance) {
        Some(blocklisted(map))
    } else if map.blocked(lease) {
        Some(err(Code::Fenced, map, "this lease identity is blocklisted"))
    } else {
        None
    }
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
    /// This copy is outvoted (or damaged): fetch member `from`'s, which a
    /// majority holds, then push it to the members in `fix`.
    Pull {
        from: usize,
        fix: Vec<usize>,
    },
    /// No majority agrees on a copy: report, don't guess.
    Unresolved,
}

fn verdict(digests: &[Option<u64>]) -> Verdict {
    let mine = digests[0];
    if digests.iter().all(|d| *d == mine) {
        return Verdict::Agree;
    }
    let with_me = digests.iter().filter(|d| **d == mine).count();
    if with_me * 2 > digests.len() {
        return Verdict::Repair((1..digests.len()).filter(|i| digests[*i] != mine).collect());
    }
    for (from, d) in digests.iter().enumerate().skip(1) {
        if digests.iter().filter(|x| *x == d).count() * 2 > digests.len() {
            let fix = (1..digests.len()).filter(|i| digests[*i] != *d).collect();
            return Verdict::Pull { from, fix };
        }
    }
    Verdict::Unresolved
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

/// Every this-many scrub passes, the first included, is deep (09-replication.md
/// §8.5: light daily, deep weekly).
const DEEP_SCRUB_EVERY: u64 = 7;

/// How fast a deep scrub reads, per device, in bytes a second.
const DEEP_SCRUB_RATE: u64 = 64 << 20;

/// The engine operation of an operation list's effect (assertions have
/// none: the primary decides them).
fn engine_op(key: &ObjKey, op: WireOp) -> Option<Op> {
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
        WireOp::Absorb(v) => Op::Absorb(ObjKey {
            version: v,
            ..key.clone()
        }),
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
                        ..Default::default()
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
            shard: shard_cell::ShardCell::new(shard),
            dev,
            io_timeout: Cell::new(IO_TIMEOUT),
            commit_delays: RefCell::new(None),
            client,
            queue: RefCell::new(Vec::new()),
            committer: RefCell::new(None),
            seqs: RefCell::new(HashMap::new()),
            peers: Default::default(),
            backfill_bytes: RefCell::new(HashMap::new()),
            pings: Default::default(),
            batches: Cell::new(0),
            backfills: RefCell::new(HashMap::new()),
            fill_inflight: RefCell::new(HashMap::new()),
            required: RefCell::new(std::collections::HashSet::new()),
            lease_until: Cell::new(None),
            fenced: RefCell::new(HashMap::new()),
            pg_inflight: RefCell::new(HashMap::new()),
            no_digests: Cell::new(false),
            push_part_bytes: Cell::new(PUSH_PART_BYTES),
            damaged: RefCell::new(HashSet::new()),
            exec_locks: RefCell::new(HashSet::new()),
            leases: RefCell::new(HashMap::new()),
            slow_reported: RefCell::new(HashSet::new()),
            deadlines: Rc::default(),
            failed: Cell::new(false),
            failure_reported: Cell::new(false),
            inconsistent: RefCell::new(HashMap::new()),
            partitions: RefCell::new(None),
            watches: RefCell::new(HashMap::new()),
            quiesce: RefCell::new(HashMap::new()),
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

    /// Commits everything queued now, as one batch, in queue order. Each
    /// round's writes and flush go through the reactor's ring where the
    /// device allows (I199), so the reactor answers pings and serves other
    /// work while the device works; nothing uses the shard until the
    /// commit ends.
    async fn commit_queued(&self) {
        let c = self.shard.commit().await;
        let batch = std::mem::take(&mut *self.queue.borrow_mut());
        if batch.is_empty() {
            return;
        }
        let (lists, txs): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
        let n = lists.len();
        let mut rest: std::collections::VecDeque<OpList> = lists.into();
        let mut results = Vec::with_capacity(n);
        let t = std::time::Instant::now();
        let mut waited = Duration::ZERO;
        let res = loop {
            if rest.is_empty() {
                c.shard().commit_done();
                break Ok(());
            }
            let begun = c.shard().commit_begin(&mut rest);
            let mut round = match begun {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            let w = std::time::Instant::now();
            let io = self.write_round(round.take_writes()).await;
            waited += w.elapsed();
            let ended = c.shard().commit_end(round, io);
            match ended {
                Ok(mut v) => results.append(&mut v),
                Err(e) => break Err(e),
            }
        };
        drop(c);
        let held = t.elapsed().saturating_sub(waited);
        if held >= STALL {
            eprintln!(
                "hyperstord: device {}: a commit of {n} list(s) took {} ms",
                self.me,
                held.as_millis()
            );
        }
        if waited >= STALL {
            eprintln!(
                "hyperstord: device {}: a commit of {n} list(s) waited {} ms for the device",
                self.me,
                waited.as_millis()
            );
        }
        self.batches.set(self.batches.get() + 1);
        match res {
            Ok(()) => {
                for (tx, r) in txs.into_iter().zip(results) {
                    let _ = tx.send(r.map_err(|e: OpError| e.to_string()));
                }
            }
            Err(e) => {
                if matches!(e, EngineError::Io(_) | EngineError::Poisoned) && !self.failed.get() {
                    eprintln!("hyperstord: device {} failed: {e}", self.me);
                    self.failed.set(true);
                }
                for tx in txs {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        }
    }

    /// Writes one commit round and flushes (I199): on the reactor's ring
    /// when the device has a descriptor, else in the device's own calls
    /// (memory and test devices), which hold the reactor as before. A
    /// device that takes longer than the I/O timeout fails the round.
    async fn write_round(
        &self,
        writes: Vec<(u64, hyperstor_engine::AlignedBuf)>,
    ) -> std::io::Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let delay = self
            .commit_delays
            .borrow()
            .as_ref()
            .map_or(Duration::ZERO, |d| d.get(self.me));
        let io = async {
            if !delay.is_zero() {
                sleep(delay).await;
            }
            match self.dev.raw_fd() {
                // SAFETY: the descriptor is the device's, which `self.dev`
                // keeps open for as long as this reactor runs; the kernel
                // holds its own reference to the file once an operation
                // starts, so even a write abandoned on the timeout below
                // never reaches another file.
                Some(fd) => unsafe { hyperstor_reactor::file::write_then_sync(fd, writes) }.await,
                None => {
                    let io: Vec<(u64, &[u8])> = writes.iter().map(|(o, b)| (*o, &b[..])).collect();
                    self.dev.write_batch(&io).and_then(|()| self.dev.flush())
                }
            }
        };
        let limit = self.io_timeout.get();
        match hyperstor_reactor::time::timeout(limit, io).await {
            Ok(r) => r,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "the device took over {} ms to write a commit",
                    limit.as_millis()
                ),
            )),
        }
    }

    /// Logs an engine call begun at `t` that held the reactor for
    /// [`STALL`] or more (I188).
    fn note_slow(&self, t: std::time::Instant, what: impl FnOnce() -> String) {
        let took = t.elapsed();
        if took >= STALL {
            eprintln!(
                "hyperstord: device {}: {} took {} ms",
                self.me,
                what(),
                took.as_millis()
            );
        }
    }

    /// Logs and counts each time this reactor's thread is held for
    /// [`STALL`] or more (I188): a 10 ms timer that fires late. Whatever
    /// held it also delayed every ping it should have answered.
    async fn stall_watch(self: Rc<Self>) {
        const TICK: Duration = Duration::from_millis(10);
        loop {
            let t = std::time::Instant::now();
            sleep(TICK).await;
            let late = t.elapsed().saturating_sub(TICK);
            if late >= STALL {
                metrics::ShardMetrics::add(&self.metrics.stalls, 1);
                eprintln!(
                    "hyperstord: device {}: reactor held {} ms",
                    self.me,
                    late.as_millis()
                );
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
            self.commit_queued().await;
            let pending = self.shard.get().await.pending_blocks();
            if pending > 4096 {
                let t = std::time::Instant::now();
                let _ = self.shard.get_mut().await.apply_deferred();
                self.note_slow(t, || format!("applying {pending} deferred block(s)"));
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
        // Retried by clients: the map service is told, and places the PG
        // elsewhere.
        if self.failed.get() {
            return Err(err(Code::Unavailable, &map, "device failed"));
        }
        // No valid map lease: this node may have been replaced without
        // knowing it, so it serves nothing (09-replication.md §8.4).
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
            let keys = self.shard.get().await.list_pg(to.pool, to.pg);
            self.queue.borrow_mut().extend(keys.into_iter().map(|k| {
                let list = OpList {
                    key: k,
                    ops: vec![Op::Delete],
                };
                (list, oneshot().0)
            }));
            self.commit_queued().await;
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

    /// Peering (09-replication.md §8.4, I61): before a primary serves a PG at
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
            lease: 0,
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
                let Some(parts) = self.push_parts(to, &key, false) else {
                    return false;
                };
                if !push_all(&conn, parts).await {
                    return false;
                }
                metrics::ShardMetrics::add(&self.metrics.peer_pushes, 1);
            }
        }
        true
    }

    async fn peer(&self, addr: SocketAddr) -> Result<Conn, ClientError> {
        self.partitioned(addr)?;
        Ok(self.peers.get(addr).await?)
    }

    /// Bytes of backfill pushes on the wire to `target`.
    fn backfill_bytes(&self, target: u32) -> usize {
        self.backfill_bytes
            .borrow()
            .get(&target)
            .copied()
            .unwrap_or(0)
    }

    /// The connection to `addr` that carries only pings (I187).
    async fn ping_peer(&self, addr: SocketAddr) -> Result<Conn, ClientError> {
        self.partitioned(addr)?;
        Ok(self.pings.get(addr).await?)
    }

    /// Refuses a peer the partitions test hook cuts this node from.
    fn partitioned(&self, addr: SocketAddr) -> Result<(), ClientError> {
        if let Some((node, p)) = &*self.partitions.borrow()
            && let Some(d) = self.map().devices.values().find(|d| d.addr == addr)
            && p.blocked(*node, d.node)
        {
            return Err(ClientError::Rpc(hyperstor_client::rpc::RpcError::Io(
                "partitioned (test hook)".into(),
            )));
        }
        Ok(())
    }

    /// Copies the engine's counters into the admin port's gauges, once a
    /// second: `stats()` is constant-time (I37).
    async fn gauges(self: Rc<Self>) {
        use std::sync::atomic::Ordering::Relaxed;
        let block = hyperstor_engine::BLOCK as u64;
        loop {
            {
                let st = self.shard.get().await.stats();
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
            // A blocklisted instance gets nothing more (06-metadata.md §4.1),
            // nor does a request under a blocklisted lease identity (D150).
            if !matches!(msg, Msg::Ping)
                && let Some(m) = fenced(&self.map(), instance, msg.target().map_or(0, |t| t.lease))
            {
                reply(&m, &[]);
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
                    label,
                } => {
                    self.lease(h.req, instance, to, key, mode, takeover, label, tx.clone())
                        .await
                }
                Msg::LeaseRelease { to, key } => {
                    self.lease_release(h.req, instance, to, key, tx.clone())
                        .await
                }
                Msg::LeaseDelegates { to, key, delegates } => {
                    self.lease_delegates(h.req, instance, to, key, delegates, tx.clone())
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
                        let m = me.lease_renew(pool, map_epoch, leases).await;
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
                Msg::PushKv { to, key } => {
                    metrics::ShardMetrics::add(&self.metrics.pushes, 1);
                    metrics::ShardMetrics::add(&self.metrics.push_bytes, payload.len() as u64);
                    self.push_kv(h.req, to, key, payload, tx.clone()).await
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
                Msg::ReadAt {
                    to,
                    key,
                    layers,
                    off,
                    len,
                } => {
                    let arrived = std::time::Instant::now();
                    let (m, p) = self.read_at(to, key, layers, off, len, true).await;
                    self.metrics.read_latency.observe(arrived.elapsed());
                    metrics::ShardMetrics::add(&self.metrics.reads, 1);
                    metrics::ShardMetrics::add(&self.metrics.read_bytes, p.len() as u64);
                    reply(&m, &p);
                }
                Msg::Sources {
                    to,
                    key,
                    layers,
                    off,
                    len,
                } => {
                    let (m, p) = self.read_at(to, key, layers, off, len, false).await;
                    reply(&m, &p);
                }
                Msg::Versions { to, key } => {
                    let m = match self.check(&to, true).await {
                        Err(m) => m,
                        Ok(_) => match engine_key(&to, &key) {
                            Ok(k) => Msg::VersionsOk {
                                versions: self.shard.get().await.versions(&k),
                            },
                            Err(()) => err(Code::Invalid, &self.map(), "bad namespace"),
                        },
                    };
                    reply(&m, &[]);
                }
                Msg::DropVersions {
                    to,
                    key,
                    from,
                    upto,
                } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let m = me.drop_versions(instance, to, key, from, upto).await;
                        let _ = tx.send(encode_frame(req, &m, &[]));
                    });
                }
                Msg::Flatten { to, key, layers } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let m = me.flatten(instance, to, key, layers).await;
                        let _ = tx.send(encode_frame(req, &m, &[]));
                    });
                }
                Msg::Watch { to, key, version } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let m = me.watch(to, key, version).await;
                        let _ = tx.send(encode_frame(req, &m, &[]));
                    });
                }
                Msg::Quiesced { to, key, token } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let m = me.quiesced(to, key, token).await;
                        let _ = tx.send(encode_frame(req, &m, &[]));
                    });
                }
                Msg::Snapshot { to, key, token } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let m = me.snapshot(instance, to, key, token, payload).await;
                        let _ = tx.send(encode_frame(req, &m, &[]));
                    });
                }
                Msg::Quiesce { to, key, token } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let m = match me.hold(to, key, token).await {
                            Ok(_) => Msg::ExecOk,
                            Err(m) => m,
                        };
                        let _ = tx.send(encode_frame(req, &m, &[]));
                    });
                }
                Msg::Release { to, key, token } => {
                    if let Ok(okey) = engine_key(&to, &key)
                        && self
                            .quiesce
                            .borrow()
                            .get(&okey)
                            .is_some_and(|q| q.token == token)
                    {
                        self.resume(&okey);
                    }
                    reply(&Msg::ExecOk, &[]);
                }
                Msg::Fetch { to, key } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let (m, p) = me.fetch(to, key).await;
                        let _ = tx.send(encode_frame(req, &m, &p));
                    });
                }
                Msg::FetchPart {
                    to,
                    key,
                    first,
                    kv_from,
                } => {
                    let (me, tx, req) = (self.clone(), tx.clone(), h.req);
                    spawn(async move {
                        let (m, p) = me.fetch_part(to, key, first, kv_from).await;
                        let _ = tx.send(encode_frame(req, &m, &p));
                    });
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
        if let Some(m) = fenced(&self.map(), instance, to.lease) {
            return send(m);
        }
        // Being repaired from a member (`repair_own`): queued now, the
        // write could be overwritten by the copy fetched. Rare; retried.
        if !self.exec_locks.borrow().is_empty() && self.exec_locks.borrow().contains(&okey) {
            return send(err(Code::Unavailable, &self.map(), "object being repaired"));
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
        let remote: Vec<_> = conns
            .iter()
            .map(|(d, c)| (*d, c.start(&repl, &data)))
            .collect();
        let deadline = std::time::Instant::now() + REPL_TIMEOUT;
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
            let failed = me2.await_replicas(to, remote, deadline).await;
            let m2 = me2.clone();
            let _inflight = InflightGuard(me2, (to.pool, to.pg));
            let ok = r.is_ok() && failed.is_none();
            let failed_dev = m2.failed.get();
            send(match (r, failed) {
                (Ok(c), None) => Msg::WriteOk {
                    revision: c.revision,
                },
                (Err(e), _) if failed_dev => Msg::Error {
                    code: Code::Unavailable,
                    map_epoch: 0,
                    detail: format!("device failed: {e}"),
                },
                (Err(e), _) => Msg::Error {
                    code: commit_code(&e),
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

    /// Waits for each replica's answer to a replicated change until
    /// `deadline` (the operation timeout from when it was sent), with one
    /// timer for them all. A member that doesn't answer in time, or whose
    /// connection breaks, is reported to the map service, which takes it
    /// out of the PG (D91). The first failure, if any.
    async fn await_replicas<F>(
        self: &Rc<Self>,
        to: Target,
        remote: Vec<(u32, F)>,
        deadline: std::time::Instant,
    ) -> Option<String>
    where
        F: Future<Output = Result<hyperstor_client::rpc::Reply, hyperstor_client::rpc::RpcError>>,
    {
        let failed: RefCell<Option<String>> = RefCell::new(None);
        // The member being waited for, when the time runs out.
        let waiting = Cell::new(None);
        let all = async {
            for (d, f) in remote {
                waiting.set(Some(d));
                let why = match f.await {
                    Ok((Msg::ReplOk { .. }, _)) => continue,
                    Ok((Msg::Error { detail, .. }, _)) => detail,
                    Ok((m, _)) => format!("unexpected {m:?}"),
                    Err(e) => {
                        self.report_slow(to, d);
                        e.to_string()
                    }
                };
                failed.borrow_mut().get_or_insert(why);
            }
        };
        let in_time = {
            let mut all = std::pin::pin!(all);
            let mut expired = std::pin::pin!(self.deadlines.at(deadline));
            std::future::poll_fn(|cx| {
                if all.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(true);
                }
                if expired.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(false);
                }
                Poll::Pending
            })
            .await
        };
        if !in_time && let Some(d) = waiting.get() {
            self.report_slow(to, d);
            failed
                .borrow_mut()
                .get_or_insert(format!("device {d} didn't answer within {REPL_TIMEOUT:?}"));
        }
        failed.into_inner()
    }

    /// Tells the map service, once per PG epoch, that `device` is too slow
    /// for the PG (D91).
    fn report_slow(self: &Rc<Self>, to: Target, device: u32) {
        let k = (to.pool, to.pg, to.pg_epoch, device);
        if !self.slow_reported.borrow_mut().insert(k) {
            return;
        }
        let me = self.clone();
        spawn(async move {
            let _ = me
                .client
                .call_meta(&Msg::MemberSlow {
                    reporter: me.me,
                    pool: to.pool,
                    pg: to.pg,
                    pg_epoch: to.pg_epoch,
                    device,
                })
                .await;
        });
    }

    /// Connections to a PG's replicas for a write: acting members, which
    /// must all answer, and backfill targets, which get it too (so they
    /// miss nothing after their copy), best effort. Refused when too few
    /// copies are acting.
    async fn replicas(
        self: &Rc<Self>,
        to: &Target,
        pg: &PgInfo,
    ) -> Result<(Vec<(u32, Conn)>, Vec<(u32, Conn)>), Msg> {
        let map = self.map();
        if (pg.acting_count() as u8) < map.pools[&to.pool].min_size {
            return Err(err(Code::Unavailable, &map, "too few copies"));
        }
        let mut conns = Vec::new();
        // Members being filled now: live ones (a down member waits).
        let filling = map.filling(pg);
        let required: Vec<u32> = filling
            .iter()
            .copied()
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
                Ok(c) => conns.push((*d, c)),
                Err(e) => {
                    // An acting member this primary can't reach is taken
                    // out of the PG as a slow one is (D91): its node may be
                    // up for everyone else (a partition between two nodes).
                    if pg.acting.contains(d) {
                        self.report_slow(*to, *d);
                    }
                    return Err(err(Code::Unavailable, &map, format!("replica: {e}")));
                }
            }
        }
        let mut fills = Vec::new();
        for d in filling.into_iter().filter(|d| !required.contains(d)) {
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

    /// An atomic operation list on one object (06-metadata.md §4.2). The
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

    /// Changes one object as its primary (06-metadata.md §4.2): holding the
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
        if let Some(m) = fenced(&self.map(), instance, to.lease) {
            unlock(self);
            return fail(m);
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
        let remote: Vec<_> = conns
            .iter()
            .map(|(d, c)| (*d, c.start(&repl, &body)))
            .collect();
        let deadline = std::time::Instant::now() + REPL_TIMEOUT;
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
            ops: effects
                .into_iter()
                .filter_map(|o| engine_op(&okey, o))
                .collect(),
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
        let changed = okey.clone();
        Box::pin(async move {
            let r = local.await;
            let failed = me2.await_replicas(to, remote, deadline).await;
            let failed_dev = me2.failed.get();
            if r.is_ok() && failed.is_none() {
                me2.notify(&changed, WATCH_CHANGED, 0);
            }
            let _inflight = InflightGuard(me2, (pool, pgn));
            match (r, failed) {
                (Ok(_), None) => Ok(()),
                (Err(e), _) if failed_dev => Err(Msg::Error {
                    code: Code::Unavailable,
                    map_epoch: 0,
                    detail: format!("device failed: {e}"),
                }),
                (Err(e), _) => Err(Msg::Error {
                    code: commit_code(&e),
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

    /// The keys of a chunk's versions to read through, first first: for
    /// each layer (branch, oldest and newest epoch), that branch's versions
    /// in the range, newest first (08-engine.md §7.2).
    fn layer_keys(
        &self,
        to: &Target,
        base: &WireKey,
        layers: &[(u32, u64, u64)],
    ) -> Result<Vec<(ObjKey, u32)>, ()> {
        let ns = Namespace::from_u8(base.ns).map_err(|_| ())?;
        let sh = self.shard.borrow();
        let mut keys = Vec::new();
        for (branch, oldest, newest) in layers {
            let k = ObjKey::new(to.pool, to.pg, ns, branch_name(&base.name, *branch), 0);
            for v in sh
                .versions(&k)
                .into_iter()
                .rev()
                .filter(|v| v <= newest && v >= oldest)
            {
                keys.push((
                    ObjKey {
                        version: v,
                        ..k.clone()
                    },
                    *branch,
                ));
            }
        }
        Ok(keys)
    }

    /// A read of a volume chunk at an epoch, through its versions and its
    /// clone parents (D101).
    async fn read_at(
        &self,
        to: Target,
        key: WireKey,
        layers: Vec<(u32, u64, u64)>,
        off: u64,
        len: u64,
        data: bool,
    ) -> (Msg, Vec<u8>) {
        if let Err(m) = self.check(&to, true).await {
            return (m, Vec::new());
        }
        self.shard.idle().await;
        let Ok(keyed) = self.layer_keys(&to, &key, &layers) else {
            return (err(Code::Invalid, &self.map(), "bad namespace"), Vec::new());
        };
        let keys: Vec<ObjKey> = keyed.iter().map(|(k, _)| k.clone()).collect();
        // Where each block comes from, for the client's cache (D102).
        let sources = self
            .shard
            .get()
            .await
            .layer_sources(&keys, off, len)
            .into_iter()
            .map(|i| i.map_or((u32::MAX, 0), |i| (keyed[i].1, keyed[i].0.version)))
            .collect();
        if !data {
            return (
                Msg::ReadAtOk {
                    crcs: Vec::new(),
                    sources,
                },
                Vec::new(),
            );
        }
        let r = self.shard.get().await.read_layered(&keys, off, len);
        let data = match r {
            Ok(b) => b.to_vec(),
            Err(EngineError::NotFound) => vec![0; len as usize],
            Err(EngineError::Checksum { lblock }) => {
                return (
                    err(Code::Checksum, &self.map(), format!("block {lblock}")),
                    Vec::new(),
                );
            }
            Err(e) => {
                if matches!(e, EngineError::Io(_) | EngineError::Poisoned) {
                    self.failed.set(true);
                }
                return (err(Code::Io, &self.map(), e.to_string()), Vec::new());
            }
        };
        (
            Msg::ReadAtOk {
                crcs: block_crcs(&data),
                sources,
            },
            data,
        )
    }

    /// Deletes the versions of `key`'s name with epochs from `from` to
    /// `upto`, one by one, each replicated as a delete.
    async fn drop_versions(
        self: &Rc<Self>,
        instance: u64,
        to: Target,
        key: WireKey,
        from: u64,
        upto: u64,
    ) -> Msg {
        if let Err(m) = self.check(&to, true).await {
            return m;
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return err(Code::Invalid, &self.map(), "bad namespace");
        };
        let versions: Vec<u64> = self
            .shard
            .get()
            .await
            .versions(&okey)
            .into_iter()
            .filter(|v| *v >= from && *v <= upto)
            .collect();
        for v in versions {
            let k = WireKey {
                version: v,
                ..key.clone()
            };
            let done = self
                .decide_and_apply(instance, to, k, |_, _| Ok(vec![WireOp::Delete]))
                .await;
            if let Err(m) = done.await {
                return m;
            }
        }
        Msg::ExecOk
    }

    /// Copies into a clone's chunk (`key`, at its head epoch) every block
    /// no version of its own branch holds, read through `layers`, its
    /// parents (10-volumes.md §10.4). Decided and replicated as writes.
    async fn flatten(
        self: &Rc<Self>,
        instance: u64,
        to: Target,
        key: WireKey,
        layers: Vec<(u32, u64, u64)>,
    ) -> Msg {
        let decide = move |me: &Self, okey: &ObjKey| {
            let mut parents = Vec::new();
            {
                let sh = me.shard.borrow();
                let ns = okey.ns;
                for (branch, oldest, newest) in &layers {
                    // The parents' names: the chunk's name (lineage and
                    // index, 24 bytes) without the clone's branch.
                    let chunk = &okey.name[..okey.name.len().min(CHUNK_NAME_LEN)];
                    let k = ObjKey::new(okey.pool, okey.pg, ns, branch_name(chunk, *branch), 0);
                    for v in sh
                        .versions(&k)
                        .into_iter()
                        .rev()
                        .filter(|v| v <= newest && v >= oldest)
                    {
                        parents.push(ObjKey {
                            version: v,
                            ..k.clone()
                        });
                    }
                }
            }
            let sh = me.shard.borrow();
            let own: Vec<ObjKey> = sh
                .versions(okey)
                .into_iter()
                .map(|v| ObjKey {
                    version: v,
                    ..okey.clone()
                })
                .collect();
            let blocks = parents
                .iter()
                .filter_map(|k| sh.stat(k))
                .map(|s| s.size.div_ceil(hyperstor_engine::BLOCK as u64))
                .max()
                .unwrap_or(0);
            let mut effects: Vec<WireOp> = Vec::new();
            let mut run: Option<(u64, Vec<u8>)> = None;
            for lb in 0..blocks {
                let missing =
                    !own.iter().any(|k| sh.holds(k, lb)) && parents.iter().any(|k| sh.holds(k, lb));
                if missing {
                    let b = sh
                        .read_layered(&parents, lb * 4096, 4096)
                        .map_err(|e| err(Code::Checksum, &me.map(), e.to_string()))?;
                    match &mut run {
                        Some((start, data)) if *start + (data.len() / 4096) as u64 == lb => {
                            data.extend_from_slice(&b)
                        }
                        _ => {
                            if let Some((start, data)) = run.take() {
                                effects.push(WireOp::Write {
                                    off: start * 4096,
                                    data,
                                });
                            }
                            run = Some((lb, b.to_vec()));
                        }
                    }
                }
            }
            if let Some((start, data)) = run.take() {
                effects.push(WireOp::Write {
                    off: start * 4096,
                    data,
                });
            }
            Ok(effects)
        };
        let done = self.decide_and_apply(instance, to, key, decide).await;
        match done.await {
            Ok(()) => Msg::ExecOk,
            Err(m) => m,
        }
    }

    /// Wakes every watch of `key` with `event`.
    fn notify(&self, key: &ObjKey, event: u8, token: u64) {
        if self.watches.borrow().is_empty() {
            return;
        }
        if let Some(ws) = self.watches.borrow_mut().remove(key) {
            for w in ws {
                let _ = w.send((event, token));
            }
        }
    }

    /// A lease holder's watch of its header (D103): answered at once if the
    /// header is past `version` or a quiesce is pending, else when either
    /// happens, or after 20 s (the holder watches again).
    async fn watch(&self, to: Target, key: WireKey, version: u64) -> Msg {
        if let Err(m) = self.check(&to, true).await {
            return m;
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return err(Code::Invalid, &self.map(), "bad namespace");
        };
        if let Some(q) = self.quiesce.borrow().get(&okey) {
            return Msg::WatchOk {
                event: WATCH_QUIESCE,
                token: q.token,
            };
        }
        let now = self
            .shard
            .get()
            .await
            .attrs(&okey)
            .map_or(0, header_version);
        if now > version {
            return Msg::WatchOk {
                event: WATCH_CHANGED,
                token: 0,
            };
        }
        let (tx, rx) = oneshot();
        self.watches.borrow_mut().entry(okey).or_default().push(tx);
        let (event, token) =
            match hyperstor_reactor::time::timeout(Duration::from_secs(20), rx.recv()).await {
                Ok(Some(e)) => e,
                _ => (WATCH_CHANGED, 0),
            };
        Msg::WatchOk { event, token }
    }

    /// The holder, or one of its delegates (`to.lease`), has stopped
    /// writing and drained, for quiesce `token`: once all have, tell the
    /// waiting snapshot; answer once it is taken or abandoned.
    async fn quiesced(&self, to: Target, key: WireKey, token: u64) -> Msg {
        let Ok(okey) = engine_key(&to, &key) else {
            return err(Code::Invalid, &self.map(), "bad namespace");
        };
        let (tx, rx) = oneshot();
        {
            let mut q = self.quiesce.borrow_mut();
            match q.get_mut(&okey) {
                Some(q) if q.token == token => {
                    q.pending.retain(|l| *l != to.lease);
                    if q.pending.is_empty()
                        && let Some(d) = q.drained.take()
                    {
                        let _ = d.send(());
                    }
                    q.resume.push(tx);
                }
                // Abandoned already, or another snapshot's.
                _ => return Msg::ExecOk,
            }
        }
        let _ = hyperstor_reactor::time::timeout(Duration::from_secs(5), rx.recv()).await;
        Msg::ExecOk
    }

    /// A snapshot (10-volumes.md §10.3): with the volume's holder, if any,
    /// quiesced first (abandoned if it doesn't within 2 s), the header's
    /// change is applied as an operation list; then the holder resumes.
    async fn snapshot(
        self: &Rc<Self>,
        instance: u64,
        to: Target,
        key: WireKey,
        token: u64,
        payload: Vec<u8>,
    ) -> Msg {
        let Ok(ops) = decode_ops(&payload) else {
            return err(Code::Invalid, &self.map(), "malformed operation list");
        };
        let held = if token == 0 {
            match self
                .hold(to, key.clone(), hyperstor_client::random_u64().max(1))
                .await
            {
                Ok(h) => h,
                Err(m) => return m,
            }
        } else {
            // A group's: under the hold its Quiesce took, if the volume was
            // held then; a hold that expired meanwhile abandons it.
            if let Err(m) = self.check(&to, true).await {
                return m;
            }
            let Ok(okey) = engine_key(&to, &key) else {
                return err(Code::Invalid, &self.map(), "bad namespace");
            };
            let mine = self
                .quiesce
                .borrow()
                .get(&okey)
                .is_some_and(|q| q.token == token);
            let leased = self
                .shard
                .get()
                .await
                .attrs(&okey)
                .is_some_and(|a| a.contains_key(LEASE_ATTR));
            if leased && !mine {
                return err(
                    Code::Busy,
                    &self.map(),
                    "the group's hold on this volume ended",
                );
            }
            mine
        };
        let Ok(okey) = engine_key(&to, &key) else {
            return err(Code::Invalid, &self.map(), "bad namespace");
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
        let r = done.await;
        if held {
            self.resume(&okey);
        }
        match r {
            Ok(()) => Msg::ExecOk,
            Err(m) => m,
        }
    }

    /// Quiesces the volume's holder, if it has one, and holds it with
    /// `token` (D103): its writes wait until [`Self::resume`], or until the
    /// hold expires. Whether a holder is held; abandoned (`Busy`) if it
    /// doesn't drain within 2 s.
    async fn hold(self: &Rc<Self>, to: Target, key: WireKey, token: u64) -> Result<bool, Msg> {
        self.check(&to, true).await?;
        let Ok(okey) = engine_key(&to, &key) else {
            return Err(err(Code::Invalid, &self.map(), "bad namespace"));
        };
        // Who must drain: the holder, and every delegate under its lease.
        let pending: Vec<u64> = self
            .shard
            .get()
            .await
            .attrs(&okey)
            .map_or_else(Vec::new, |a| {
                a.get(LEASE_ATTR)
                    .and_then(|h| decode_holder(h))
                    .map(|h| h.lease)
                    .into_iter()
                    .chain(
                        a.get(DELEGATES_ATTR)
                            .map_or_else(Vec::new, |b| decode_delegates(b)),
                    )
                    .collect()
            });
        let held = !pending.is_empty();
        if held {
            if self.quiesce.borrow().contains_key(&okey) {
                return Err(err(
                    Code::Busy,
                    &self.map(),
                    "another snapshot is quiescing",
                ));
            }
            let (tx, rx) = oneshot();
            self.quiesce.borrow_mut().insert(
                okey.clone(),
                Quiesce {
                    token,
                    pending,
                    drained: Some(tx),
                    resume: Vec::new(),
                },
            );
            self.notify(&okey, WATCH_QUIESCE, token);
            let drained = matches!(
                hyperstor_reactor::time::timeout(QUIESCE_WITHIN, rx.recv()).await,
                Ok(Some(()))
            );
            if !drained {
                self.resume(&okey);
                return Err(err(
                    Code::Busy,
                    &self.map(),
                    format!(
                        "the volume's holder didn't quiesce within {QUIESCE_WITHIN:?}; snapshot abandoned"
                    ),
                ));
            }
            // A hold nobody ends (its client died) ends by itself.
            let me = self.clone();
            spawn(async move {
                sleep(HOLD_AT_MOST).await;
                if me
                    .quiesce
                    .borrow()
                    .get(&okey)
                    .is_some_and(|q| q.token == token)
                {
                    me.resume(&okey);
                }
            });
        }
        Ok(held)
    }

    /// Ends a quiesce: the holder resumes.
    fn resume(&self, okey: &ObjKey) {
        if let Some(q) = self.quiesce.borrow_mut().remove(okey) {
            for r in q.resume {
                let _ = r.send(());
            }
        }
    }

    /// Takes a volume's lease for the identity `to.lease` of `instance` as
    /// its header's primary (06-metadata.md §9, D75, D150). Granted if nobody
    /// holds it, or that identity does. Another holder's lease lasts
    /// [`LEASE_EXPIRES`] past its last renewal here, or past when this
    /// device first saw it (a new primary gives it a full lease); once it
    /// has expired, `takeover` breaks it, but only if its identity and
    /// every delegate's are blocklisted in this device's map.
    #[allow(clippy::too_many_arguments)]
    async fn lease(
        self: &Rc<Self>,
        req: u64,
        instance: u64,
        to: Target,
        key: WireKey,
        mode: u8,
        takeover: bool,
        label: String,
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
        let lease = to.lease;
        if lease == 0 {
            return send(err(Code::Invalid, &self.map(), "no lease identity"));
        }
        if label.len() > hyperstor_proto::LABEL_MAX {
            return send(err(Code::Invalid, &self.map(), "label too long"));
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
            let delegates = attrs
                .get(DELEGATES_ATTR)
                .map_or_else(Vec::new, |b| decode_delegates(b));
            let (pg_epoch, now) = (to.pg_epoch, std::time::Instant::now());
            let grant = |me: &Self| {
                me.leases
                    .borrow_mut()
                    .insert(okey.clone(), (lease, now + LEASE_EXPIRES, pg_epoch));
            };
            let mine = Holder {
                lease,
                instance,
                mode,
                label: label.clone(),
            };
            match holder {
                None => {}
                Some(h) if h.lease == lease => {
                    grant(me);
                    // Taken again by another instance with the identity
                    // (none does yet): the record names it.
                    return Ok(if h == mine {
                        Vec::new()
                    } else {
                        vec![WireOp::SetAttr(LEASE_ATTR.to_vec(), encode_holder(&mine))]
                    });
                }
                Some(h) => {
                    let until = me.lease_until(okey, h.lease, pg_epoch, now);
                    if now < until {
                        return Err(Msg::LeaseHeld {
                            holder: h.lease,
                            delegates,
                            remaining_ms: (until - now).as_millis().max(1) as u32,
                            label: h.label,
                        });
                    }
                    let map = me.map();
                    let cut_off = map.blocked(h.lease) && delegates.iter().all(|d| map.blocked(*d));
                    if !(takeover && cut_off) {
                        return Err(Msg::LeaseHeld {
                            holder: h.lease,
                            delegates,
                            remaining_ms: 0,
                            label: h.label,
                        });
                    }
                }
            }
            grant(me);
            let mut ops = vec![WireOp::SetAttr(LEASE_ATTR.to_vec(), encode_holder(&mine))];
            // The last holder's delegates don't act under this lease.
            if attrs.contains_key(DELEGATES_ATTR) {
                ops.push(WireOp::RmAttr(DELEGATES_ATTR.to_vec()));
            }
            Ok(ops)
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
                        && me.leases.borrow().get(&k).is_some_and(|l| l.0 == lease)
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

    /// Gives up the lease `to.lease` on a volume, with its delegates;
    /// nothing if that isn't the lease held.
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
            if holder.is_none_or(|h| h.lease != to.lease) {
                return Ok(Vec::new());
            }
            me.leases.borrow_mut().remove(okey);
            let mut ops = vec![WireOp::RmAttr(LEASE_ATTR.to_vec())];
            if attrs.contains_key(DELEGATES_ATTR) {
                ops.push(WireOp::RmAttr(DELEGATES_ATTR.to_vec()));
            }
            Ok(ops)
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

    /// Records the delegates of the lease `to.lease` (D33, D151): refused
    /// with [`Msg::LeaseHeld`] unless that is the lease held.
    async fn lease_delegates(
        self: &Rc<Self>,
        req: u64,
        instance: u64,
        to: Target,
        key: WireKey,
        delegates: Vec<u64>,
        tx: Sender<Vec<u8>>,
    ) {
        let send = move |m: Msg| {
            let _ = tx.send(encode_frame(req, &m, &[]));
        };
        if delegates.contains(&0) {
            return send(err(Code::Invalid, &self.map(), "delegate 0"));
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
            match attrs.get(LEASE_ATTR).and_then(|h| decode_holder(h)) {
                Some(h) if h.lease == to.lease => {}
                h => {
                    return Err(Msg::LeaseHeld {
                        holder: h.as_ref().map_or(0, |h| h.lease),
                        delegates: Vec::new(),
                        remaining_ms: 0,
                        label: h.map(|h| h.label).unwrap_or_default(),
                    });
                }
            }
            Ok(vec![if delegates.is_empty() {
                WireOp::RmAttr(DELEGATES_ATTR.to_vec())
            } else {
                WireOp::SetAttr(DELEGATES_ATTR.to_vec(), encode_delegates(&delegates))
            }])
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

    /// Renews leases on headers this device is primary of (D75), each by
    /// its identity (D150); a fenced identity's is lost. Nothing is
    /// written: the expiry is kept in memory.
    async fn lease_renew(
        &self,
        pool: u32,
        map_epoch: u64,
        leases: Vec<(u32, u64, Vec<u8>, u64)>,
    ) -> Msg {
        let mut results = Vec::with_capacity(leases.len());
        for (pg, pg_epoch, name, lease) in leases {
            let to = Target {
                pool,
                pg,
                map_epoch,
                pg_epoch,
                lease,
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
            let sh = self.shard.get().await;
            let attrs = sh.attrs(&okey).filter(|_| sh.stat(&okey).is_some());
            let version = attrs.map_or(0, header_version);
            let holder = attrs
                .and_then(|a| a.get(LEASE_ATTR))
                .and_then(|h| decode_holder(h));
            let mut l = self.leases.borrow_mut();
            // Granted to another here, and not yet written.
            let taken = l
                .get(&okey)
                .is_some_and(|(h, _, e)| *e == pg_epoch && *h != lease);
            if holder.is_some_and(|h| h.lease == lease) && !taken && !self.map().blocked(lease) {
                l.insert(
                    okey,
                    (lease, std::time::Instant::now() + LEASE_EXPIRES, pg_epoch),
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
        let ops = ops
            .into_iter()
            .filter_map(|o| engine_op(&okey, o))
            .collect();
        let local = self.enqueue(OpList { key: okey, ops });
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
        let sh = self.shard.get().await;
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
    /// peering (09-replication.md §8.5).
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
        self.shard.idle().await;
        let lists = self.object_lists(&okey, obj);
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

    /// A part of an object too large for one push ([`Msg::PushKv`]): its
    /// key–value entries added to what the last [`Msg::PushObject`] on
    /// this connection made. This connection's messages are handled in
    /// order, so the parts land after the object they continue.
    async fn push_kv(
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
        let Ok(kv) = hyperstor_proto::decode_pairs(&payload) else {
            return send(err(Code::Checksum, &self.map(), "malformed key-value part"));
        };
        let Ok(okey) = engine_key(&to, &key) else {
            return send(err(Code::Invalid, &self.map(), "bad namespace"));
        };
        self.shard.idle().await;
        let done: Vec<_> = self
            .kv_lists(&okey, kv)
            .into_iter()
            .map(|l| self.enqueue(l))
            .collect();
        spawn(async move {
            let mut ok = true;
            for d in done {
                ok &= d.await.is_ok();
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

    /// Key–value entries as `KvPut` lists the engine accepts.
    fn kv_lists(&self, okey: &ObjKey, kv: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<OpList> {
        let limit = self.shard.borrow().max_list_bytes() / 2;
        let mut lists = Vec::new();
        let mut batch = Vec::new();
        let mut bytes = 0;
        for (k, v) in kv {
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
        lists
    }

    /// The lists that replace an object with `obj`, in lists the engine
    /// accepts: the first replaces the data and attributes, the rest carry
    /// the key–value table in batches. Reads come from the primary, so a
    /// member between two lists serves no one the half-copied object.
    fn object_lists(&self, okey: &ObjKey, obj: WireObject) -> Vec<OpList> {
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
        lists.extend(self.kv_lists(okey, obj.kv));
        lists
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
    /// node before the request is in it (09-replication.md §8.5).
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
            .await
            .into_iter()
            .map(|(k, d)| (wire_key(&k), d))
            .collect();
        (Msg::DigestsOk, encode_digests(&entries))
    }

    /// This device's digests of a PG; [`DAMAGED`] for objects a deep scrub
    /// found damaged. In batches of [`DIGEST_BATCH`] objects with a yield
    /// between (I183), and a large object's digest in parts (I188), so
    /// that pings and heartbeats get through. An
    /// object written meanwhile is digested as its batch finds it: scrub
    /// looks twice before it repairs, and backfill sends what differs,
    /// with live writes covering the rest.
    async fn local_digests(&self, pool: u32, pg: u32) -> Vec<(ObjKey, u64)> {
        let keys = self.shard.get().await.list_pg(pool, pg);
        let mut out = Vec::with_capacity(keys.len());
        for batch in keys.chunks(DIGEST_BATCH) {
            for k in batch {
                let Some(d) = self.digest(k).await else {
                    continue;
                };
                let d = match d {
                    _ if self.damaged.borrow().contains(k) => DAMAGED,
                    DAMAGED => DAMAGED + 1,
                    d => d,
                };
                out.push((k.clone(), d));
            }
            hyperstor_reactor::yield_now().await;
        }
        out
    }

    /// An object's digest ([`Shard::digest`](hyperstor_engine::Shard::digest)); a large one a part of
    /// [`DIGEST_PART`] bytes at a time with a yield between (I188), so that
    /// pings get through: at once, a table of 215 MB held the reactor for
    /// 290 ms, near the peer timeout. If the object changes between parts,
    /// the digest begins again; after two such restarts it is taken at
    /// once.
    async fn digest(&self, key: &ObjKey) -> Option<u64> {
        let mut restarts = 0;
        let mut step = self.shard.get().await.digest_begin(key)?;
        loop {
            match step {
                DigestPart::Done(d) => return Some(d),
                DigestPart::More(g) => {
                    hyperstor_reactor::yield_now().await;
                    let sh = self.shard.get().await;
                    step = match sh.digest_part(g, DIGEST_PART) {
                        Some(s) => s,
                        None if restarts < 2 => {
                            restarts += 1;
                            sh.digest_begin(key)?
                        }
                        None => return sh.digest(key),
                    };
                }
            }
        }
    }

    /// Deep scrub of this device's copy of a PG: reads every object, which
    /// checks every block's CRC (08-engine.md §7.2), and marks the objects
    /// that fail. At most [`DEEP_SCRUB_RATE`] bytes a second, yielding
    /// between objects so I/O goes on.
    async fn verify_pg(&self, pool: u32, pg: u32) {
        let keys = self.shard.get().await.list_pg(pool, pg);
        for key in keys {
            let read = {
                let sh = self.shard.get().await;
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

    /// Replaces an object with a primary's copy (backfill, 09-replication.md §8.5).
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
        // Peers reported since they last answered: logged once each.
        let reported: Rc<RefCell<HashSet<u32>>> = Rc::default();
        const TICK: Duration = Duration::from_millis(100);
        let mut ticked = std::time::Instant::now();
        loop {
            sleep(TICK).await;
            // A tick this late means this reactor was held (I188): the
            // pings it sent meanwhile timed out or their answers wait
            // unread. That says nothing about the peers, so their silence
            // is counted afresh. Otherwise two nodes held at once report
            // every other node, as a primary digesting a large table did.
            let late = ticked.elapsed() >= TICK + STALL;
            ticked = std::time::Instant::now();
            if late {
                for t in last_ok.borrow_mut().values_mut() {
                    *t = ticked;
                }
            }
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
            // A peer that left (down, or no longer sharing a PG) starts
            // afresh when it comes back: its old silence isn't news.
            last_ok.borrow_mut().retain(|p, _| peers.contains_key(p));
            reported.borrow_mut().retain(|p| peers.contains_key(p));
            for (peer, addr) in peers {
                let first = *last_ok.borrow_mut().entry(peer).or_insert(now);
                if now.duration_since(first) > peer_timeout && !self.cut_off() {
                    if reported.borrow_mut().insert(peer) {
                        eprintln!(
                            "hyperstord: device {}: node {peer} silent for {} ms; reporting it",
                            self.me,
                            now.duration_since(first).as_millis()
                        );
                    }
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
                let reported = reported.clone();
                spawn(async move {
                    // The ping gives up on its own (its pending entry
                    // with it); its connection, which carries nothing
                    // else (I187), stays.
                    let ping = async {
                        let c = me.ping_peer(addr).await.ok()?;
                        c.try_within(&Msg::Ping, &[], Duration::from_millis(200))
                            .await
                            .ok()
                    };
                    if let Ok(Some((Msg::Pong, _))) =
                        hyperstor_reactor::time::timeout(Duration::from_millis(200), ping).await
                    {
                        last_ok.borrow_mut().insert(peer, std::time::Instant::now());
                        reported.borrow_mut().remove(&peer);
                    }
                });
            }
        }
    }

    /// Heartbeats to the map service every 100 ms, once every device of
    /// the node is open (`opening` at 0, I188): the map service marks a
    /// node up on its first heartbeat, and a device still opening answers
    /// no pings, so its peers would report the node silent at once. A
    /// device with a large index takes seconds to open.
    async fn heartbeat(self: Rc<Self>, node: u32, opening: Arc<std::sync::atomic::AtomicUsize>) {
        while opening.load(std::sync::atomic::Ordering::Acquire) > 0 {
            sleep(Duration::from_millis(10)).await;
        }
        loop {
            if self.failed.get() && !self.failure_reported.get() && !self.cut_off() {
                let r = self
                    .client
                    .call_meta(&Msg::DeviceFailed { device: self.me })
                    .await;
                self.failure_reported
                    .set(matches!(r, Ok((Msg::Noted { .. }, _))));
            }
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
                    // blocklist a client (06-metadata.md §9): the map service
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

    /// Reports this device's data region and free bytes to the map
    /// service every 5 s (D156), in a task of its own so that a slow
    /// answer never delays a heartbeat. A report not taken (no leader
    /// yet, a member that isn't it) is sent again after 500 ms; an older
    /// map service refuses it as invalid, and is asked again only at the
    /// next period.
    async fn space(self: Rc<Self>) {
        let block = hyperstor_engine::BLOCK as u64;
        loop {
            let mut wait = SPACE_EVERY;
            if !self.cut_off() {
                let st = self.shard.get().await.stats();
                let r = self
                    .client
                    .call_meta(&Msg::DeviceSpace {
                        device: self.me,
                        total_bytes: st.data_blocks * block,
                        free_bytes: st.free_blocks * block,
                    })
                    .await;
                if !matches!(
                    r,
                    Ok((Msg::Noted { .. }, _))
                        | Ok((
                            Msg::Error {
                                code: hyperstor_proto::Code::Invalid,
                                ..
                            },
                            _
                        ))
                ) {
                    wait = Duration::from_millis(500);
                }
            }
            sleep(wait).await;
        }
    }

    /// Reports to the map service, every `every`, the bytes this device's
    /// copy of each PG it is primary of holds (D155): the PGs whose bytes
    /// changed, or all of them when the map service holds another number
    /// of them from this device (a new leader, a PG gone to another
    /// primary) or the last report failed.
    async fn usage(self: Rc<Self>, every: Duration) {
        let mut sent: HashMap<(u32, u32), u64> = HashMap::new();
        let mut full = true;
        loop {
            sleep(every).await;
            if self.cut_off() {
                continue;
            }
            let mine: Vec<(u32, u32)> = self
                .map()
                .pgs
                .iter()
                .filter(|(_, i)| i.primary() == Some(self.me))
                .map(|(k, _)| *k)
                .collect();
            let mut now = HashMap::with_capacity(mine.len());
            for (pool, pg) in mine {
                now.insert((pool, pg), self.pg_bytes(pool, pg).await);
            }
            let pgs = now
                .iter()
                .filter(|(k, b)| full || sent.get(k) != Some(b))
                .map(|(&(pool, pg), &b)| (pool, pg, b))
                .collect();
            let r = self
                .client
                .call_meta(&Msg::PgUsage {
                    device: self.me,
                    full,
                    pgs,
                })
                .await;
            full = match r {
                Ok((Msg::PgUsageOk { known }, _)) => known as usize != now.len(),
                _ => true,
            };
            sent = now;
        }
    }

    /// The bytes this device's copy of a PG holds, summed a slice of the
    /// index at a time so that I/O waits behind none of it for long.
    async fn pg_bytes(&self, pool: u32, pg: u32) -> u64 {
        let (mut blocks, mut after) = (0, None);
        loop {
            let (b, next) = self
                .shard
                .get()
                .await
                .pg_blocks(pool, pg, after.as_ref(), 4096);
            blocks += b;
            match next {
                Some(k) => after = Some(k),
                None => return blocks * hyperstor_engine::BLOCK as u64,
            }
            hyperstor_reactor::yield_now().await;
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
                for target in map.filling(info) {
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
    /// write queued so far. A commit leaves the queue when it begins and
    /// ends later (I199), so this also waits for the one in flight; the
    /// shard may then be used until the caller's next `.await`.
    async fn drained(&self) {
        #[cfg(hs_diag)]
        let t = std::time::Instant::now();
        loop {
            while !self.queue.borrow().is_empty() {
                crate::diag!("drain {}", self.me; "device {}: waiting {:?} for {} queued commits to drain", self.me, t.elapsed(), self.queue.borrow().len());
                hyperstor_reactor::yield_now().await;
            }
            self.shard.idle().await;
            if self.queue.borrow().is_empty() {
                return;
            }
        }
    }

    async fn backfill(self: Rc<Self>, pool: u32, pg: u32, target: u32, pg_epoch: u64) {
        let k = (pool, pg, target);
        let mut reported = false;
        // A diagnostic build says when an attempt runs long (a hung call).
        #[cfg(hs_diag)]
        let watched = {
            let done = Rc::new(std::cell::Cell::new(false));
            let (d, t) = (done.clone(), std::time::Instant::now());
            spawn(async move {
                loop {
                    sleep(Duration::from_secs(10)).await;
                    if d.get() {
                        return;
                    }
                    crate::diag!("long {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} to device {target} still running after {:?}", t.elapsed());
                }
            });
            done
        };
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
                if !reported {
                    crate::diag!("done {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} to device {target}: BackfillDone answered {r:?}");
                } else {
                    crate::diag!("ok {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} to device {target}: done");
                }
                let _ = self.client.refresh_map().await;
            }
        }
        if !reported {
            self.required.borrow_mut().remove(&k);
            sleep(Duration::from_millis(200)).await;
        }
        #[cfg(hs_diag)]
        watched.set(true);
        // Allow a new attempt (or none, once the map says it's done).
        self.backfills.borrow_mut().remove(&k);
    }

    async fn backfill_once(&self, pool: u32, pg: u32, target: u32, pg_epoch: u64) -> bool {
        let map = self.map();
        let Some(addr) = map.device_addr(target) else {
            crate::diag!("addr {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg}: device {target} has no address in map {}", map.epoch);
            return false;
        };
        let conn = match self.peer(addr).await {
            Ok(c) => c,
            Err(e) => {
                crate::diag!("conn {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg}: connecting to device {target} at {addr}: {e:?}");
                let _ = e;
                return false;
            }
        };
        let to = Target {
            pool,
            pg,
            map_epoch: map.epoch,
            pg_epoch,
            lease: 0,
        };
        // Ask the target what it holds, then send what differs: objects
        // whose digests don't match, and objects it has that this copy
        // doesn't (sent as deletes). Each read and its push happen with no
        // wait in between, after every queued commit is done: writes queued
        // earlier are in the copy, and writes queued later follow it on the
        // same connection. A key that changes after the comparison is
        // carried by the live writes backfill targets get anyway.
        let mut legacy = false;
        #[cfg(hs_diag)]
        let asked = std::time::Instant::now();
        let theirs = match conn
            .start_within(&Msg::Digests { to, deep: false }, &[], CALL_TIMEOUT)
            .await
        {
            Ok((Msg::DigestsOk, p)) => match decode_digests(&p) {
                Ok(v) => {
                    crate::diag!("digests {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg}: device {target} holds {} objects ({} bytes of digests, {:?})", v.len(), p.len(), asked.elapsed());
                    v
                }
                Err(_) => {
                    crate::diag!("decode {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg}: device {target}'s digests don't decode");
                    return false;
                }
            },
            // A node from before digests (a rolling upgrade, 15-operations.md
            // §18.4): reset its copy and send everything, as backfill did.
            Ok((
                Msg::Error {
                    code: Code::Invalid,
                    ..
                },
                _,
            )) => {
                self.drained().await;
                let reset = conn.start_within(
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
                    CALL_TIMEOUT,
                );
                if !matches!(reset.await, Ok((Msg::PushOk, _))) {
                    return false;
                }
                legacy = true;
                Vec::new()
            }
            other => {
                crate::diag!("refused {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} (pg epoch {pg_epoch}, map {}): device {target} answered Digests with {:?}", to.map_epoch, other.as_ref().map(|(m, _)| m));
                let _ = other;
                return false;
            }
        };
        let theirs: HashMap<ObjKey, u64> = theirs
            .into_iter()
            .filter_map(|(k, d)| Some((engine_key(&to, &k).ok()?, d)))
            .collect();
        self.drained().await;
        #[cfg(hs_diag)]
        let listed = std::time::Instant::now();
        // Compared in batches with a yield between, as `local_digests`
        // digests (I183). A key that changes meanwhile is compared as its
        // batch finds it; live writes reach this target anyway.
        let ours = self.shard.get().await.list_pg(pool, pg);
        let mut keys: BTreeSet<ObjKey> = {
            let sh = self.shard.get().await;
            theirs
                .keys()
                .filter(|k| sh.stat(k).is_none())
                .cloned()
                .collect()
        };
        for batch in ours.chunks(DIGEST_BATCH) {
            for k in batch {
                if theirs.get(k).copied() != self.digest(k).await {
                    keys.insert(k.clone());
                }
            }
            hyperstor_reactor::yield_now().await;
        }
        crate::diag!("keys {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} to device {target}: {} objects differ (compared in {:?})", keys.len(), listed.elapsed());
        let mut inflight = Vec::new();
        let held = Held {
            st: self,
            target,
            bytes: Cell::new(0),
        };
        for key in keys {
            if self.map().pg(pool, pg).map(|p| p.epoch) != Some(pg_epoch) {
                crate::diag!("epoch {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} to device {target}: the PG changed (epoch {pg_epoch} gone) mid-copy");
                return false;
            }
            // Room on the connection first (I187): this backfill's own
            // pushes are answered when its window is full or the target's
            // budget is spent, then it waits for the other backfills'.
            if inflight.len() >= BACKFILL_WINDOW
                || (!inflight.is_empty() && self.backfill_bytes(target) >= BACKFILL_BUDGET)
            {
                for f in inflight.drain(..) {
                    let r = f.await;
                    if !matches!(r, Ok((Msg::PushOk, _))) {
                        crate::diag!("push {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} to device {target}: a push answered {:?}", r.as_ref().map(|(m, _)| m));
                        return false;
                    }
                }
                held.release();
            }
            while self.backfill_bytes(target) >= BACKFILL_BUDGET {
                sleep(Duration::from_millis(1)).await;
            }
            self.drained().await;
            let Some(parts) = self.push_parts(to, &key, legacy) else {
                return false;
            };
            // An object's parts in order on this connection, which the
            // target handles in order.
            for (m, data) in parts {
                held.add(data.len());
                inflight.push(conn.start_within(&m, &data, CALL_TIMEOUT));
            }
        }
        for f in inflight {
            let r = f.await;
            if !matches!(r, Ok((Msg::PushOk, _))) {
                crate::diag!("push {}.{} {}", pool, pg, target; "backfill of PG {pool}.{pg} to device {target}: a push answered {:?}", r.as_ref().map(|(m, _)| m));
                return false;
            }
        }
        true
    }

    /// The pushes of this node's copy of `key`, to send in order on one
    /// connection: a [`Msg::PushObject`], and for an object whose
    /// key–value table is larger than a part (`push_part_bytes`), the rest
    /// of the table as [`Msg::PushKv`] parts, so that no frame exceeds the
    /// limit. Read all at once, with no wait in between, as
    /// [`Self::push_of`] reads.
    fn push_parts(&self, to: Target, key: &ObjKey, legacy: bool) -> Option<Vec<(Msg, Vec<u8>)>> {
        let whole = legacy || self.shard.borrow().stat(key).is_none();
        if whole {
            return self.push_of(to, key, legacy).map(|p| vec![p]);
        }
        let limit = self.push_part_bytes.get();
        let sh = self.shard.borrow();
        let st = sh.stat(key)?;
        let data = sh
            .read(key, 0, st.size.div_ceil(4096) * 4096)
            .ok()?
            .to_vec();
        let attrs = sh
            .attrs(key)
            .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        // The table in parts of about `limit` bytes, read in batches.
        const BATCH: usize = 4096;
        let mut parts: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![Vec::new()];
        let (mut bytes, mut start) = (0, Vec::new());
        loop {
            let batch = sh.kv_scan(key, &start, None, BATCH);
            let n = batch.len();
            if let Some((last, _)) = batch.last() {
                start = last.clone();
                start.push(0);
            }
            for (k, v) in batch {
                let b = hyperstor_proto::pair_bytes(&k, &v);
                if bytes + b > limit && parts.last().is_some_and(|p| !p.is_empty()) {
                    parts.push(Vec::new());
                    bytes = 0;
                }
                bytes += b;
                parts.last_mut().expect("a part").push((k, v));
            }
            if n < BATCH {
                break;
            }
        }
        let mut rest = parts.into_iter();
        let obj = WireObject {
            size: st.size,
            data,
            attrs,
            kv: rest.next().unwrap_or_default(),
        };
        let mut out = vec![(
            Msg::PushObject {
                to,
                key: wire_key(key),
            },
            encode_object(&obj),
        )];
        out.extend(rest.map(|kv| {
            (
                Msg::PushKv {
                    to,
                    key: wire_key(key),
                },
                hyperstor_proto::encode_pairs(&kv),
            )
        }));
        Some(out)
    }

    /// The push of this node's copy of `key` (a delete if it has none), in
    /// one frame: for [`Msg::Fetch`]'s answer; members get
    /// [`Self::push_parts`].
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
    /// (09-replication.md §8.5), spread evenly over the interval rather than
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
            lease: 0,
        };
        // `None`: a member from before digests (a rolling upgrade), which
        // can't be compared and is taken to agree, as before digests.
        let mut theirs: Vec<Option<HashMap<ObjKey, u64>>> = Vec::new();
        for d in &info.acting {
            if *d == self.me {
                continue;
            }
            let conn = self.peer(map.device_addr(*d)?).await.ok()?;
            // A deep scrub reads every block: it may take minutes, and
            // keepalive catches a peer that's gone. A light one answers
            // from the index.
            let asked = if deep {
                conn.call(&Msg::Digests { to, deep }, &[]).await
            } else {
                conn.call_within(&Msg::Digests { to, deep }, &[], CALL_TIMEOUT)
                    .await
            };
            match asked {
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
        let mine: HashMap<ObjKey, u64> = self.local_digests(pool, pg).await.into_iter().collect();
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
            self.report_scrub(pool, pg, 0).await;
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
            lease: 0,
        };
        let others: Vec<u32> = info
            .acting
            .iter()
            .copied()
            .filter(|d| *d != self.me)
            .collect();
        let mut unresolved = 0;
        for key in bad {
            metrics::ShardMetrics::add(&self.metrics.scrub_inconsistent, 1);
            let v = match verdict(&row(&second, &key)) {
                // Pull the majority's copy first; then it is this copy.
                Verdict::Pull { from, fix } => {
                    if self.repair_own(to, &key, others[from - 1]).await {
                        Verdict::Repair(fix)
                    } else {
                        Verdict::Unresolved
                    }
                }
                v => v,
            };
            match v {
                Verdict::Agree | Verdict::Pull { .. } => {}
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
                        let Some(parts) = self.push_parts(to, &key, false) else {
                            ok = false;
                            continue;
                        };
                        ok &= push_all(&conn, parts).await;
                    }
                    if ok {
                        metrics::ShardMetrics::add(&self.metrics.scrub_repaired, 1);
                    }
                }
                Verdict::Unresolved => {
                    unresolved += 1;
                    metrics::ShardMetrics::add(&self.metrics.scrub_unresolved, 1);
                    eprintln!(
                        "hyperstord: scrub: pool {pool} PG {pg}: copies of an object disagree \
                         and no majority decides; left as is (namespace {:?}, version {})",
                        key.ns, key.version
                    );
                }
            }
        }
        self.report_scrub(pool, pg, unresolved).await;
    }

    /// Tells the map service how many objects of a PG its last scrub left
    /// unresolved, when that changes (for cluster health).
    async fn report_scrub(&self, pool: u32, pg: u32, unresolved: u32) {
        let before = self
            .inconsistent
            .borrow()
            .get(&(pool, pg))
            .copied()
            .unwrap_or(0);
        if before == unresolved {
            return;
        }
        let r = self
            .client
            .call_meta(&Msg::ScrubReport {
                pool,
                pg,
                unresolved,
            })
            .await;
        if matches!(r, Ok((Msg::Noted { .. }, _))) {
            let mut inc = self.inconsistent.borrow_mut();
            if unresolved == 0 {
                inc.remove(&(pool, pg));
            } else {
                inc.insert((pool, pg), unresolved);
            }
        }
    }

    /// Replaces this primary's copy of `key` with member `from`'s (a
    /// copy that failed its checksums, or that scrub's majority outvoted;
    /// 09-replication.md §8.2, §8.5). Holds the key, so no write or operation
    /// list reaches it meanwhile, and first waits until every write
    /// accepted for the PG has been answered, so `from` has them all.
    /// Whether it was replaced.
    async fn repair_own(&self, to: Target, key: &ObjKey, from: u32) -> bool {
        // An EC member holds another shard, not a copy.
        if self.map().pools.get(&to.pool).map(|p| p.kind)
            != Some(hyperstor_meta::PoolKind::Replicated)
        {
            return false;
        }
        let t = std::time::Instant::now();
        while self.exec_locks.borrow().contains(key) {
            if t.elapsed() > REPL_TIMEOUT {
                return false;
            }
            sleep(Duration::from_millis(1)).await;
        }
        self.exec_locks.borrow_mut().insert(key.clone());
        let r = self.fetch_into(to, key, from).await;
        self.exec_locks.borrow_mut().remove(key);
        if r {
            eprintln!(
                "hyperstord: repaired this primary's copy of an object of PG {}.{} from device {from}",
                to.pool, to.pg
            );
        }
        r
    }

    async fn fetch_into(&self, to: Target, key: &ObjKey, from: u32) -> bool {
        let t = std::time::Instant::now();
        while self
            .pg_inflight
            .borrow()
            .get(&(to.pool, to.pg))
            .is_some_and(|n| *n > 0)
        {
            if t.elapsed() > REPL_TIMEOUT {
                return false;
            }
            sleep(Duration::from_millis(1)).await;
        }
        self.drained().await;
        let Some(addr) = self.map().device_addr(from) else {
            return false;
        };
        let Ok(conn) = self.peer(addr).await else {
            return false;
        };
        // In parts (I182); a member from before parts answers that it
        // doesn't know the request, and gets a whole-object Fetch.
        let part = |first: bool, kv_from: Vec<u8>| {
            let (conn, m) = (
                &conn,
                Msg::FetchPart {
                    to,
                    key: wire_key(key),
                    first,
                    kv_from,
                },
            );
            async move { conn.call_within(&m, &[], CALL_TIMEOUT).await }
        };
        match part(true, Vec::new()).await {
            Ok((
                Msg::Error {
                    code: Code::Invalid,
                    ..
                },
                _,
            )) => {}
            Ok((Msg::FetchPartOk { found: false, .. }, _)) => {
                return self
                    .apply_fetched(
                        key,
                        vec![OpList {
                            key: key.clone(),
                            ops: vec![Op::Delete],
                        }],
                    )
                    .await;
            }
            Ok((Msg::FetchPartOk { found: true, next }, payload)) => {
                let Ok(obj) = decode_object(&payload) else {
                    return false;
                };
                self.shard.idle().await;
                let mut done: Vec<_> = self
                    .object_lists(key, obj)
                    .into_iter()
                    .map(|l| self.enqueue(l))
                    .collect();
                let mut next = next;
                let mut ok = true;
                while let Some(from) = next.take() {
                    match part(false, from).await {
                        Ok((
                            Msg::FetchPartOk {
                                found: true,
                                next: n,
                            },
                            payload,
                        )) => {
                            let Ok(kv) = hyperstor_proto::decode_pairs(&payload) else {
                                ok = false;
                                break;
                            };
                            self.shard.idle().await;
                            done.extend(
                                self.kv_lists(key, kv).into_iter().map(|l| self.enqueue(l)),
                            );
                            next = n;
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                for d in done {
                    ok &= d.await.is_ok();
                }
                if ok {
                    self.damaged.borrow_mut().remove(key);
                } else {
                    // Half replaced: reads go to repair, not to this copy.
                    self.damaged.borrow_mut().insert(key.clone());
                }
                return ok;
            }
            _ => return false,
        }
        let lists = match conn
            .call_within(
                &Msg::Fetch {
                    to,
                    key: wire_key(key),
                },
                &[],
                CALL_TIMEOUT,
            )
            .await
        {
            Ok((Msg::FetchOk { found: true }, payload)) => match decode_object(&payload) {
                Ok(obj) => {
                    self.shard.idle().await;
                    self.object_lists(key, obj)
                }
                Err(_) => return false,
            },
            Ok((Msg::FetchOk { found: false }, _)) => vec![OpList {
                key: key.clone(),
                ops: vec![Op::Delete],
            }],
            _ => return false,
        };
        self.apply_fetched(key, lists).await
    }

    /// Applies a fetched copy's lists; the key is no longer damaged once
    /// they all are.
    async fn apply_fetched(&self, key: &ObjKey, lists: Vec<OpList>) -> bool {
        let done: Vec<_> = lists.into_iter().map(|l| self.enqueue(l)).collect();
        let mut ok = true;
        for d in done {
            ok &= d.await.is_ok();
        }
        if ok {
            self.damaged.borrow_mut().remove(key);
        }
        ok
    }

    /// A member's answer to [`Msg::Fetch`]: its copy, once every write
    /// queued before the request is applied.
    async fn fetch(&self, to: Target, key: WireKey) -> (Msg, Vec<u8>) {
        if let Err(m) = self.check(&to, false).await {
            return (m, Vec::new());
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return (err(Code::Invalid, &self.map(), "bad namespace"), Vec::new());
        };
        self.drained().await;
        if self.damaged.borrow().contains(&okey) {
            return (
                err(Code::Checksum, &self.map(), "this copy is damaged too"),
                Vec::new(),
            );
        }
        if self.shard.get().await.stat(&okey).is_none() {
            return (Msg::FetchOk { found: false }, Vec::new());
        }
        match self.push_of(to, &okey, false) {
            Some((_, payload)) => (Msg::FetchOk { found: true }, payload),
            None => (
                err(
                    Code::Checksum,
                    &self.map(),
                    "this copy fails its checksums too",
                ),
                Vec::new(),
            ),
        }
    }

    /// A member's answer to [`Msg::FetchPart`]: a part of its copy, about
    /// `push_part_bytes` of key–value table each (I182). The primary
    /// fetching holds the key's exec lock, so no write changes the copy
    /// between parts.
    async fn fetch_part(
        &self,
        to: Target,
        key: WireKey,
        first: bool,
        kv_from: Vec<u8>,
    ) -> (Msg, Vec<u8>) {
        if let Err(m) = self.check(&to, false).await {
            return (m, Vec::new());
        }
        let Ok(okey) = engine_key(&to, &key) else {
            return (err(Code::Invalid, &self.map(), "bad namespace"), Vec::new());
        };
        self.drained().await;
        if self.damaged.borrow().contains(&okey) {
            return (
                err(Code::Checksum, &self.map(), "this copy is damaged too"),
                Vec::new(),
            );
        }
        let sh = self.shard.get().await;
        let Some(st) = sh.stat(&okey) else {
            return (
                Msg::FetchPartOk {
                    found: false,
                    next: None,
                },
                Vec::new(),
            );
        };
        let (kv, next) = kv_part(&sh, &okey, &kv_from, self.push_part_bytes.get());
        if !first {
            return (
                Msg::FetchPartOk { found: true, next },
                hyperstor_proto::encode_pairs(&kv),
            );
        }
        let Ok(data) = sh.read(&okey, 0, st.size.div_ceil(4096) * 4096) else {
            return (
                err(
                    Code::Checksum,
                    &self.map(),
                    "this copy fails its checksums too",
                ),
                Vec::new(),
            );
        };
        let obj = WireObject {
            size: st.size,
            data: data.to_vec(),
            attrs: sh
                .attrs(&okey)
                .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
            kv,
        };
        (Msg::FetchPartOk { found: true, next }, encode_object(&obj))
    }

    async fn read(&self, to: Target, key: WireKey, off: u64, len: u64) -> (Msg, Vec<u8>) {
        let pg = match self.check(&to, true).await {
            Ok(p) => p,
            Err(m) => return (m, Vec::new()),
        };
        let Ok(okey) = engine_key(&to, &key) else {
            return (err(Code::Invalid, &self.map(), "bad namespace"), Vec::new());
        };
        let mut r = self.shard.get().await.read(&okey, off, len);
        // A block that fails its checksum (09-replication.md §8.2): this copy
        // is replaced by a member's, and the read served from it.
        if matches!(r, Err(EngineError::Checksum { .. })) {
            self.damaged.borrow_mut().insert(okey.clone());
            let members: Vec<u32> = pg
                .acting
                .iter()
                .copied()
                .filter(|d| *d != self.me && *d != NO_DEVICE)
                .collect();
            for m in members {
                if self.repair_own(to, &okey, m).await {
                    r = self.shard.get().await.read(&okey, off, len);
                    break;
                }
            }
        }
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
            Err(e) => {
                if matches!(e, EngineError::Io(_) | EngineError::Poisoned) {
                    self.failed.set(true);
                }
                return (err(Code::Io, &self.map(), e.to_string()), Vec::new());
            }
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

/// Entries of `key`'s table from `from` on, up to about `limit` bytes (at
/// least one), and where the rest starts if any is left.
fn kv_part(
    sh: &hyperstor_engine::Shard,
    key: &ObjKey,
    from: &[u8],
    limit: usize,
) -> (hyperstor_proto::Pairs, Option<Vec<u8>>) {
    const BATCH: usize = 4096;
    let (mut out, mut bytes, mut start) = (Vec::new(), 0, from.to_vec());
    loop {
        let batch = sh.kv_scan(key, &start, None, BATCH);
        let n = batch.len();
        for (k, v) in batch {
            let b = hyperstor_proto::pair_bytes(&k, &v);
            if bytes + b > limit && !out.is_empty() {
                return (out, Some(k));
            }
            bytes += b;
            start = k.clone();
            out.push((k, v));
        }
        if n < BATCH {
            return (out, None);
        }
        start.push(0);
    }
}

/// Sends an object's pushes ([`ShardState::push_parts`]) in order on
/// `conn`, each answered before the next.
async fn push_all(conn: &Conn, parts: Vec<(Msg, Vec<u8>)>) -> bool {
    for (m, data) in parts {
        if !matches!(
            conn.call_within(&m, &data, CALL_TIMEOUT).await,
            Ok((Msg::PushOk, _))
        ) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{Verdict, verdict};

    #[test]
    fn a_full_shard_is_answered_no_space() {
        use super::{Code, OpError, commit_code};
        assert_eq!(commit_code(&OpError::NoSpace.to_string()), Code::NoSpace);
        assert_eq!(commit_code("I/O error: boom"), Code::Io);
    }

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
        // The primary outvoted: it pulls the majority's copy.
        assert_eq!(
            verdict(&[Some(1), Some(2), Some(2)]),
            Verdict::Pull {
                from: 1,
                fix: vec![]
            }
        );
        assert_eq!(
            verdict(&[Some(1), None, Some(2), Some(2), Some(2)]),
            Verdict::Pull {
                from: 2,
                fix: vec![1]
            }
        );
        assert_eq!(
            verdict(&[Some(5), None, None]),
            Verdict::Pull {
                from: 1,
                fix: vec![]
            }
        );
        // No majority: report, don't guess.
        assert_eq!(verdict(&[Some(1), Some(2)]), Verdict::Unresolved);
        assert_eq!(verdict(&[Some(1), Some(2), Some(3)]), Verdict::Unresolved);
    }
}
