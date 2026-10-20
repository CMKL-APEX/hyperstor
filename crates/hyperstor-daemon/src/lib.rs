//! `hyperstord`: a storage node (spec/design.md §6, §8).
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

use hyperstor_client::rpc::{Conn, read_frame, writer};
use hyperstor_client::{Client, ClientError};
use hyperstor_engine::{
    BlockDev, Committed, DeviceConfig, EngineError, FileDev, MemDev, Namespace, ObjKey, Op,
    OpError, OpList, Shard, UringDev,
};
use hyperstor_meta::{ClusterMap, NO_DEVICE, PgInfo};
use hyperstor_proto::{Code, Msg, Target, VERSION, WireKey, block_crcs, encode_frame, verify_crcs};
use hyperstor_reactor::chan::{OneSender, Sender, channel, oneshot};
use hyperstor_reactor::net::{TcpListener, TcpStream};
use hyperstor_reactor::{block_on, spawn, time::sleep};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

/// The device behind one shard.
#[derive(Debug, Clone)]
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
}

impl DeviceSpec {
    fn open(&self) -> std::io::Result<Arc<dyn BlockDev>> {
        Ok(match self {
            DeviceSpec::Mem { bytes } => Arc::new(MemDev::new(*bytes)),
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
    /// Engine deferral threshold (implementation-notes.md I14).
    pub defer_max: Option<usize>,
}

/// A running node. Dropping it without [`NodeHandle::stop`] leaves the
/// threads running.
pub struct NodeHandle {
    stops: Vec<Sender<()>>,
    threads: Vec<std::thread::JoinHandle<Result<Arc<dyn BlockDev>, String>>>,
}

impl NodeHandle {
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
    for (i, sh) in cfg.shards.into_iter().enumerate() {
        let (tx, mut rx) = channel::<()>();
        stops.push(tx);
        let cpu = cfg.cpus.get(i).copied();
        let (node, meta, defer) = (cfg.node_id, cfg.meta, cfg.defer_max);
        threads.push(
            std::thread::Builder::new()
                .name(format!("hs-reactor-{node}-{i}"))
                .spawn(move || {
                    if let Some(c) = cpu {
                        let _ = hyperstor_reactor::pin_to_cpu(c);
                    }
                    let dev = sh.device.open().map_err(|e| e.to_string())?;
                    let d2 = dev.clone();
                    block_on(async move {
                        let st = ShardState::open(node, sh.device_id, d2, meta, defer).await?;
                        let l = TcpListener::from_std(sh.listener);
                        spawn(st.clone().committer());
                        spawn(st.clone().follow_map());
                        spawn(st.clone().accept(l));
                        rx.recv().await;
                        Ok::<(), String>(())
                    })?;
                    Ok(dev)
                })
                .expect("spawn reactor thread"),
        );
    }
    NodeHandle { stops, threads }
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
}

fn err(code: Code, map: &ClusterMap, detail: impl Into<String>) -> Msg {
    Msg::Error {
        code,
        map_epoch: map.epoch,
        detail: detail.into(),
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
        if let Some(d) = defer {
            shard.set_defer_max(d);
        }
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
            let batch = std::mem::take(&mut *self.queue.borrow_mut());
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
            let pending = self.shard.borrow().stats().pending_blocks;
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
            let _ = self.client.refresh_map().await;
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
        if to.map_epoch > self.map().epoch {
            let _ = self.client.refresh_map().await;
        }
        let map = self.map();
        let Some(pg) = map.pg(to.pool, to.pg) else {
            return Err(err(Code::Invalid, &map, "no such PG"));
        };
        if pg.epoch != to.pg_epoch {
            return Err(err(Code::StaleMap, &map, "PG epoch changed"));
        }
        let ok = if primary {
            pg.primary() == Some(self.me)
        } else {
            pg.acting.contains(&self.me)
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
                    self.repl(h.req, to, key, off, crcs, payload, tx.clone())
                        .await
                }
                Msg::Read { to, key, off, len } => {
                    let (m, p) = self.read(to, key, off, len).await;
                    reply(&m, &p);
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
        let send = move |m: Msg| {
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
        let map = self.map();
        let mut conns = Vec::new();
        for d in pg.acting.iter().skip(1).filter(|d| **d != NO_DEVICE) {
            let Some(addr) = map.device_addr(*d) else {
                return send(err(Code::Unavailable, &map, "replica has no address"));
            };
            match self.peer(addr).await {
                Ok(c) => conns.push(c),
                Err(e) => return send(err(Code::Unavailable, &map, format!("replica: {e}"))),
            }
        }
        if (pg.acting.iter().filter(|d| **d != NO_DEVICE).count() as u8)
            < map.pools[&to.pool].min_size
        {
            return send(err(Code::Unavailable, &map, "too few copies"));
        }
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
        let local = self.enqueue(OpList {
            key: okey,
            ops: vec![Op::Write { off, data }],
        });
        spawn(async move {
            let r = local.await;
            let mut failed = None;
            for f in remote {
                match f.await {
                    Ok((Msg::ReplOk { .. }, _)) => {}
                    Ok((Msg::Error { detail, .. }, _)) => failed = Some(detail),
                    Ok((m, _)) => failed = Some(format!("unexpected {m:?}")),
                    Err(e) => failed = Some(e.to_string()),
                }
            }
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
