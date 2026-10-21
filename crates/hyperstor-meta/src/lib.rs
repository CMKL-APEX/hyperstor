//! The cluster map (spec/metadata.md §4.1) and a map service.
//!
//! The map names nodes, devices (one per engine shard), pools and each PG's
//! acting set; the first member of an acting set is the PG's primary.
//! [`ClusterMap::recompute`] places every PG with `hyperstor-placement`;
//! EC pools keep shard positions across changes (implementation-notes.md
//! I16).
//!
//! [`serve`] answers `GetMap` from one in-memory map. Replicating the map
//! with Raft (decisions.md D2, D20) comes later (implementation-notes.md I18).
//! Each device records the address its reactor listens on.

pub mod api;
pub mod health;

use hyperstor_codec::{Dec, DecodeError, Enc};
use hyperstor_placement::{Device, Domain, Rule, Topology, assign_positions};
use hyperstor_reactor::time::sleep;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    pub id: u32,
    pub reactors: u32,
    pub rack: u32,
    pub up: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub id: u32,
    pub node: u32,
    /// The engine shard, and so the reactor, that serves this device.
    pub shard: u32,
    /// Where that reactor listens.
    pub addr: SocketAddr,
    pub weight: f64,
    pub class: String,
    pub up_in: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    Replicated,
    Ec {
        k: u8,
        m: u8,
    },
    /// Recomputable data (decisions.md D45, replication.md §8.7): `size`
    /// copies, lost without recovery.
    Cache,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolInfo {
    pub id: u32,
    pub name: String,
    pub kind: PoolKind,
    /// Copies (replicated) or k + m (EC).
    pub size: u8,
    pub min_size: u8,
    pub pg_count: u32,
    pub domain: Domain,
    pub chunk_size: u32,
    pub class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgInfo {
    /// Map epoch of the last change to this PG's `up` or `acting` set.
    pub epoch: u64,
    /// Devices that hold every acknowledged write and serve the PG,
    /// primary first (placement.md §5.3). For EC pools the position is the shard
    /// index, and a missing shard is `NO_DEVICE`.
    pub acting: Vec<u32>,
    /// Where placement wants the PG. Members of `up` not in `acting` are
    /// being backfilled: they receive new writes and a copy of every
    /// object, then join `acting`.
    pub up: Vec<u32>,
}

impl PgInfo {
    pub fn primary(&self) -> Option<u32> {
        self.acting.first().copied().filter(|d| *d != NO_DEVICE)
    }

    /// Members of `up` still being backfilled.
    pub fn backfilling(&self) -> impl Iterator<Item = u32> + '_ {
        self.up
            .iter()
            .copied()
            .filter(|d| *d != NO_DEVICE && !self.acting.contains(d))
    }

    /// Devices with a live copy in `acting`.
    pub fn acting_count(&self) -> usize {
        self.acting.iter().filter(|d| **d != NO_DEVICE).count()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClusterMap {
    pub epoch: u64,
    pub cluster_id: u128,
    pub nodes: BTreeMap<u32, NodeInfo>,
    pub devices: BTreeMap<u32, DeviceInfo>,
    pub pools: BTreeMap<u32, PoolInfo>,
    pub pgs: BTreeMap<(u32, u32), PgInfo>,
}

pub const NO_DEVICE: u32 = u32::MAX;

impl ClusterMap {
    pub fn new(cluster_id: u128) -> Self {
        Self {
            epoch: 1,
            cluster_id,
            ..Default::default()
        }
    }

    /// Adds a node with one device (engine shard and reactor) per address
    /// in `addrs`; returns the device ids (`node * 1000 + shard`).
    pub fn add_node(&mut self, id: u32, addrs: &[SocketAddr], rack: u32) -> Vec<u32> {
        self.nodes.insert(
            id,
            NodeInfo {
                id,
                reactors: addrs.len() as u32,
                rack,
                up: true,
            },
        );
        (0..addrs.len() as u32)
            .map(|shard| {
                let dev = id * 1000 + shard;
                self.devices.insert(
                    dev,
                    DeviceInfo {
                        id: dev,
                        node: id,
                        shard,
                        addr: addrs[shard as usize],
                        weight: 1.0,
                        class: "nvme".into(),
                        up_in: true,
                    },
                );
                dev
            })
            .collect()
    }

    pub fn add_pool(&mut self, pool: PoolInfo) {
        assert!(pool.pg_count.is_power_of_two());
        self.pools.insert(pool.id, pool);
    }

    /// Places every PG for the current devices and bumps the epoch. PGs
    /// whose acting set changes get the new epoch.
    fn live(&self, device: u32) -> bool {
        self.devices
            .get(&device)
            .is_some_and(|d| d.up_in && self.nodes.get(&d.node).is_some_and(|n| n.up))
    }

    /// Places every PG for the current devices and bumps the epoch
    /// (placement.md §5.3):
    /// - `up` is where placement wants the PG;
    /// - `acting` keeps its live members, in order, so a failed primary is
    ///   replaced by the next member, which has every acknowledged write;
    /// - a new PG, or one that lost every copy, starts with `acting = up`.
    ///
    /// PGs whose sets change get the new epoch.
    pub fn recompute(&mut self) {
        self.epoch += 1;
        let devs: Vec<Device> = self
            .devices
            .values()
            .map(|d| {
                let node = &self.nodes[&d.node];
                Device {
                    id: d.id,
                    host: d.node,
                    rack: node.rack,
                    weight: d.weight,
                    class: d.class.clone(),
                    up_in: d.up_in && node.up,
                }
            })
            .collect();
        let pools: Vec<PoolInfo> = self.pools.values().cloned().collect();
        for p in pools {
            let rule = Rule {
                pool: p.id,
                size: p.size as usize,
                domain: p.domain,
                class: p.class.clone(),
            };
            let topo = Topology::new(&rule, &devs);
            for pg in 0..p.pg_count {
                let members = topo.place(pg);
                let old = self.pgs.get(&(p.id, pg)).cloned();
                let up: Vec<u32> = match p.kind {
                    PoolKind::Replicated | PoolKind::Cache => members,
                    PoolKind::Ec { .. } => {
                        let prev: Vec<Option<u32>> = match &old {
                            Some(i) => {
                                i.up.iter()
                                    .map(|d| (*d != NO_DEVICE).then_some(*d))
                                    .collect()
                            }
                            None => vec![None; p.size as usize],
                        };
                        assign_positions(&prev, &members)
                            .into_iter()
                            .map(|d| d.unwrap_or(NO_DEVICE))
                            .collect()
                    }
                };
                let acting = match &old {
                    None => up.clone(),
                    // Cache pools (D45): no backfill and no recovery; the
                    // acting set is always where placement wants it, and a
                    // new member starts empty.
                    Some(_) if p.kind == PoolKind::Cache => up.clone(),
                    Some(o) => {
                        let kept: Vec<u32> = match p.kind {
                            PoolKind::Replicated | PoolKind::Cache => {
                                o.acting.iter().copied().filter(|d| self.live(*d)).collect()
                            }
                            PoolKind::Ec { .. } => o
                                .acting
                                .iter()
                                .map(|d| if self.live(*d) { *d } else { NO_DEVICE })
                                .collect(),
                        };
                        if kept.iter().all(|d| *d == NO_DEVICE) {
                            // Every copy is down: the PG stays down, with
                            // its acting set as it was, until a member
                            // returns. Starting it again empty would let
                            // backfill wipe the members when they return.
                            o.acting.clone()
                        } else {
                            kept
                        }
                    }
                };
                let e = self.pgs.entry((p.id, pg)).or_insert(PgInfo {
                    epoch: self.epoch,
                    acting: Vec::new(),
                    up: Vec::new(),
                });
                if e.acting != acting || e.up != up {
                    e.acting = acting;
                    e.up = up;
                    e.epoch = self.epoch;
                }
            }
        }
    }

    /// Records that `device` has a full copy of a PG (replication.md §8.5). It
    /// joins `acting`; once every member of `up` has, `acting` becomes
    /// `up`, dropping members placement no longer wants. Refused (false)
    /// if the PG changed since the backfill started.
    pub fn backfill_done(&mut self, pool: u32, pg: u32, pg_epoch: u64, device: u32) -> bool {
        let live = self.live(device);
        let ec = matches!(
            self.pools.get(&pool).map(|p| p.kind),
            Some(PoolKind::Ec { .. })
        );
        let Some(info) = self.pgs.get(&(pool, pg)) else {
            return false;
        };
        if info.epoch != pg_epoch
            || !live
            || !info.up.contains(&device)
            || info.acting.contains(&device)
        {
            return false;
        }
        let mut info = info.clone();
        if ec {
            let pos = info.up.iter().position(|d| *d == device).unwrap();
            info.acting[pos] = device;
        } else {
            info.acting.push(device);
        }
        if info.backfilling().next().is_none() {
            info.acting = info.up.clone();
        }
        self.epoch += 1;
        info.epoch = self.epoch;
        self.pgs.insert((pool, pg), info);
        true
    }

    pub fn pg(&self, pool: u32, pg: u32) -> Option<&PgInfo> {
        self.pgs.get(&(pool, pg))
    }

    /// Address of the reactor serving `device`.
    pub fn device_addr(&self, device: u32) -> Option<SocketAddr> {
        self.devices.get(&device).map(|d| d.addr)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u32(1).u64(self.epoch).u128(self.cluster_id);
        e.u32(self.nodes.len() as u32);
        for n in self.nodes.values() {
            e.u32(n.id).u32(n.reactors).u32(n.rack).bool(n.up);
        }
        e.u32(self.devices.len() as u32);
        for d in self.devices.values() {
            e.u32(d.id)
                .u32(d.node)
                .u32(d.shard)
                .str(&d.addr.to_string())
                .u64(d.weight.to_bits())
                .str(&d.class)
                .bool(d.up_in);
        }
        e.u32(self.pools.len() as u32);
        for p in self.pools.values() {
            e.u32(p.id).str(&p.name);
            match p.kind {
                PoolKind::Replicated => e.u8(0).u8(0).u8(0),
                PoolKind::Ec { k, m } => e.u8(1).u8(k).u8(m),
                PoolKind::Cache => e.u8(2).u8(0).u8(0),
            };
            e.u8(p.size).u8(p.min_size).u32(p.pg_count);
            e.u8(match p.domain {
                Domain::Device => 0,
                Domain::Host => 1,
                Domain::Rack => 2,
            });
            e.u32(p.chunk_size);
            match &p.class {
                Some(c) => e.bool(true).str(c),
                None => e.bool(false),
            };
        }
        e.u32(self.pgs.len() as u32);
        for ((pool, pg), i) in &self.pgs {
            e.u32(*pool).u32(*pg).u64(i.epoch).seq(&i.acting, |e, d| {
                e.u32(*d);
            });
            e.seq(&i.up, |e, d| {
                e.u32(*d);
            });
        }
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut d = Dec::new(b);
        let v = d.u32()?;
        if v != 1 {
            return Err(DecodeError::Invalid {
                field: "map version",
                value: v as u64,
            });
        }
        let mut m = ClusterMap {
            epoch: d.u64()?,
            cluster_id: d.u128()?,
            ..Default::default()
        };
        for _ in 0..d.u32()? {
            let id = d.u32()?;
            m.nodes.insert(
                id,
                NodeInfo {
                    id,
                    reactors: d.u32()?,
                    rack: d.u32()?,
                    up: d.bool()?,
                },
            );
        }
        for _ in 0..d.u32()? {
            let id = d.u32()?;
            m.devices.insert(
                id,
                DeviceInfo {
                    id,
                    node: d.u32()?,
                    shard: d.u32()?,
                    addr: d.str()?.parse().map_err(|_| DecodeError::Utf8)?,
                    weight: f64::from_bits(d.u64()?),
                    class: d.str()?.to_string(),
                    up_in: d.bool()?,
                },
            );
        }
        for _ in 0..d.u32()? {
            let id = d.u32()?;
            let name = d.str()?.to_string();
            let kind = match (d.u8()?, d.u8()?, d.u8()?) {
                (0, _, _) => PoolKind::Replicated,
                (2, _, _) => PoolKind::Cache,
                (_, k, m) => PoolKind::Ec { k, m },
            };
            let size = d.u8()?;
            let min_size = d.u8()?;
            let pg_count = d.u32()?;
            let domain = match d.u8()? {
                0 => Domain::Device,
                1 => Domain::Host,
                _ => Domain::Rack,
            };
            let chunk_size = d.u32()?;
            let class = if d.bool()? {
                Some(d.str()?.to_string())
            } else {
                None
            };
            m.pools.insert(
                id,
                PoolInfo {
                    id,
                    name,
                    kind,
                    size,
                    min_size,
                    pg_count,
                    domain,
                    chunk_size,
                    class,
                },
            );
        }
        for _ in 0..d.u32()? {
            let pool = d.u32()?;
            let pg = d.u32()?;
            let epoch = d.u64()?;
            let acting = d.seq(4, |d| d.u32())?;
            let up = d.seq(4, |d| d.u32())?;
            m.pgs.insert((pool, pg), PgInfo { epoch, acting, up });
        }
        d.finish()?;
        Ok(m)
    }
}

/// A replicated pool with defaults for tests and small clusters.
pub fn replicated_pool(id: u32, name: &str, size: u8, pg_count: u32) -> PoolInfo {
    PoolInfo {
        id,
        name: name.into(),
        kind: PoolKind::Replicated,
        size,
        min_size: size.div_ceil(2).max(1),
        pg_count,
        domain: Domain::Host,
        chunk_size: 4 << 20,
        class: None,
    }
}

/// A request from the cluster API's thread to the map service's reactor
/// (`Meta::admin`). Replies go back on one-shot channels that work across
/// threads and runtimes.
pub enum Admin {
    /// The published map, what clients and storage nodes see, and how
    /// long ago each node was last heard from (absent: not since start).
    Published(hyperstor_reactor::chan::OneSender<(ClusterMap, BTreeMap<u32, Duration>)>),
    SetDeviceIn {
        device: u32,
        up_in: bool,
        reply: hyperstor_reactor::chan::OneSender<Result<u64, String>>,
    },
}

/// The map service's state (metadata.md §4.1, decisions.md D42).
///
/// - `map` is the working map: liveness and backfill change it.
/// - `published` is what nodes and clients see. A change is published only
///   after every device that loses a role in it has been fenced: it has
///   acknowledged a `Fence` (and answered every write it had accepted for
///   those PGs), or its last granted lease has run out. Then the map is
///   pushed to every live device.
/// - Heartbeats renew a node's map lease; a node may serve PG I/O only
///   while its lease is valid. A node that doesn't answer a fence gets no
///   new lease until the fence resolves.
pub struct Meta {
    pub map: RefCell<ClusterMap>,
    pub published: RefCell<ClusterMap>,
    beats: RefCell<HashMap<u32, Instant>>,
    /// When each node was last granted a non-zero lease.
    granted: RefCell<HashMap<u32, Instant>>,
    /// Nodes that didn't answer a fence: no leases until it resolves.
    fencing: RefCell<std::collections::HashSet<u32>>,
    conns: RefCell<HashMap<SocketAddr, hyperstor_rpc::Conn>>,
    /// Peer reports (D40): peer -> reporter -> when last reported.
    reports: RefCell<HashMap<u32, HashMap<u32, Instant>>>,
    /// Nodes marked down on peer reports: no leases while reported.
    suspect: RefCell<std::collections::HashSet<u32>>,
    lease: Duration,
    started: Instant,
}

/// Default map lease (decisions.md D48).
pub const LEASE: Duration = Duration::from_millis(600);

impl Meta {
    pub fn new(map: ClusterMap) -> Rc<Self> {
        Self::with_lease(map, LEASE)
    }

    pub fn with_lease(map: ClusterMap, lease: Duration) -> Rc<Self> {
        Rc::new(Meta {
            published: RefCell::new(map.clone()),
            map: RefCell::new(map),
            beats: RefCell::new(HashMap::new()),
            granted: RefCell::new(HashMap::new()),
            fencing: RefCell::new(std::collections::HashSet::new()),
            conns: RefCell::new(HashMap::new()),
            reports: RefCell::new(HashMap::new()),
            suspect: RefCell::new(std::collections::HashSet::new()),
            lease,
            started: Instant::now(),
        })
    }

    /// Marks a device in or out of placement (administration.md §23.6) and
    /// recomputes the working map if that changed it; the publisher
    /// fences and publishes. Returns the working map's epoch.
    pub fn set_device_in(&self, device: u32, up_in: bool) -> Result<u64, String> {
        let mut map = self.map.borrow_mut();
        let d = map
            .devices
            .get_mut(&device)
            .ok_or_else(|| format!("no device {device}"))?;
        if d.up_in != up_in {
            d.up_in = up_in;
            map.recompute();
        }
        Ok(map.epoch)
    }

    /// Answers the cluster API's requests (`api`), which arrive from its
    /// own thread: the map service's state lives on this reactor.
    pub async fn admin(self: Rc<Self>, mut rx: hyperstor_reactor::chan::Receiver<Admin>) {
        while let Some(req) = rx.recv().await {
            match req {
                Admin::Published(reply) => {
                    let now = Instant::now();
                    let heard = self
                        .beats
                        .borrow()
                        .iter()
                        .map(|(n, t)| (*n, now.duration_since(*t)))
                        .collect();
                    let _ = reply.send((self.published.borrow().clone(), heard));
                }
                Admin::SetDeviceIn {
                    device,
                    up_in,
                    reply,
                } => {
                    let _ = reply.send(self.set_device_in(device, up_in));
                }
            }
        }
    }

    pub fn lease(&self) -> Duration {
        self.lease
    }

    /// Marks nodes down after `timeout` without a heartbeat, and up again
    /// when they send one, recomputing the working map on every change. A
    /// node gets `timeout` of grace from the service's start. The timeout
    /// must exceed the lease with margin, so a node marked down has
    /// stopped serving (its lease has run out) by the time it is.
    pub async fn liveness(self: Rc<Self>, timeout: Duration) {
        assert!(
            timeout >= self.lease + self.lease / 4,
            "liveness timeout must exceed the lease by a quarter"
        );
        loop {
            sleep((timeout / 4).min(Duration::from_millis(100))).await;
            if self.liveness_tick(timeout, Instant::now()) {
                self.map.borrow_mut().recompute();
            }
        }
    }

    /// One liveness check; true if any node changed state.
    fn liveness_tick(&self, timeout: Duration, now: Instant) -> bool {
        let mut changed = false;
        {
            let beats = self.beats.borrow();
            let mut map = self.map.borrow_mut();
            // Peer reports (D40): a node reported silent by at least two
            // distinct nodes, recently, is down even if its heartbeats
            // still arrive (cut off from its peers), and gets no lease.
            let fresh = timeout / 2;
            let mut reports = self.reports.borrow_mut();
            for r in reports.values_mut() {
                r.retain(|_, t| now.duration_since(*t) < fresh);
            }
            let mut suspect = self.suspect.borrow_mut();
            suspect.clear();
            for (peer, r) in reports.iter() {
                if r.len() >= 2 {
                    suspect.insert(*peer);
                }
            }
            for n in map.nodes.values_mut() {
                let last = beats.get(&n.id).copied().unwrap_or(self.started);
                let alive = now.duration_since(last) < timeout && !suspect.contains(&n.id);
                if n.up != alive {
                    n.up = alive;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Publishes working-map changes: fence, publish, push.
    pub async fn publisher(self: Rc<Self>) {
        loop {
            sleep(Duration::from_millis(10)).await;
            if self.map.borrow().epoch > self.published.borrow().epoch {
                self.publish().await;
            }
        }
    }

    /// Devices that lose a role between `old` and `new`, with the PGs.
    pub fn losers(old: &ClusterMap, new: &ClusterMap) -> HashMap<u32, Vec<(u32, u32)>> {
        let mut out: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for (k, o) in &old.pgs {
            let Some(n) = new.pgs.get(k) else { continue };
            for (pos, d) in o.acting.iter().enumerate() {
                if *d == NO_DEVICE {
                    continue;
                }
                let lost = match n.acting.iter().position(|x| x == d) {
                    None => true,
                    // Losing primacy, or (EC) changing shard position.
                    Some(p) => {
                        (pos == 0 && p != 0)
                            || (n.acting.len() == o.acting.len()
                                && p != pos
                                && new
                                    .pools
                                    .get(&k.0)
                                    .is_some_and(|pl| matches!(pl.kind, PoolKind::Ec { .. })))
                    }
                };
                if lost {
                    out.entry(*d).or_default().push(*k);
                }
            }
        }
        out
    }

    async fn publish(self: &Rc<Self>) {
        let new = self.map.borrow().clone();
        let old = self.published.borrow().clone();
        let mut waits = Vec::new();
        for (dev, pgs) in Self::losers(&old, &new) {
            let me = self.clone();
            let node = new
                .devices
                .get(&dev)
                .or(old.devices.get(&dev))
                .map(|d| d.node);
            let addr = new.device_addr(dev).or(old.device_addr(dev));
            let up = node.and_then(|n| new.nodes.get(&n)).is_some_and(|n| n.up);
            waits.push(hyperstor_reactor::spawn(async move {
                me.fence(node, addr, up, pgs, new.epoch).await
            }));
        }
        for w in waits {
            w.await;
        }
        *self.published.borrow_mut() = new.clone();
        // Fast path: push the new map to every live device.
        let bytes = Rc::new(new.encode());
        for d in new.devices.values() {
            if !new.nodes.get(&d.node).is_some_and(|n| n.up) {
                continue;
            }
            let (me, addr, bytes, epoch) = (self.clone(), d.addr, bytes.clone(), new.epoch);
            hyperstor_reactor::spawn(async move {
                if let Ok(c) = me.conn(addr).await {
                    let _ = hyperstor_reactor::time::timeout(
                        me.lease,
                        c.call(
                            &hyperstor_proto::Msg::MapPush {
                                epoch,
                                map: bytes.to_vec(),
                            },
                            &[],
                        ),
                    )
                    .await;
                }
            });
        }
    }

    async fn conn(&self, addr: SocketAddr) -> Result<hyperstor_rpc::Conn, hyperstor_rpc::RpcError> {
        if let Some(c) = self.conns.borrow().get(&addr)
            && !c.is_dead()
        {
            return Ok(c.clone());
        }
        let c = hyperstor_rpc::Conn::connect(addr).await?;
        self.conns.borrow_mut().insert(addr, c.clone());
        Ok(c)
    }

    /// Returns once `dev` can no longer serve `pgs` under an older map.
    async fn fence(
        self: Rc<Self>,
        node: Option<u32>,
        addr: Option<SocketAddr>,
        up: bool,
        pgs: Vec<(u32, u32)>,
        epoch: u64,
    ) {
        use hyperstor_proto::Msg;
        if up && let Some(addr) = addr {
            let asked = async {
                let c = self.conn(addr).await.ok()?;
                c.call(
                    &Msg::Fence {
                        epoch,
                        pgs: pgs.clone(),
                    },
                    &[],
                )
                .await
                .ok()
            };
            if let Ok(Some((Msg::FenceOk, _))) =
                hyperstor_reactor::time::timeout(self.lease, asked).await
            {
                return;
            }
        }
        // No answer: stop renewing the node's lease and wait out the last
        // one granted.
        let Some(node) = node else { return };
        self.fencing.borrow_mut().insert(node);
        loop {
            let last = self.granted.borrow().get(&node).copied();
            let Some(last) = last else { break };
            // Leases run from when the node sent its heartbeat, which is
            // before we granted it; a quarter more covers clock rate drift.
            let until = last + self.lease + self.lease / 4;
            let now = Instant::now();
            if now >= until {
                break;
            }
            sleep(until - now).await;
        }
        self.fencing.borrow_mut().remove(&node);
    }

    fn handle(
        &self,
        msg: Result<hyperstor_proto::Msg, hyperstor_proto::FrameError>,
    ) -> hyperstor_proto::Msg {
        use hyperstor_proto::*;
        match msg {
            Ok(Msg::Hello { .. }) => Msg::HelloOk {
                version: VERSION,
                node: 0,
                map_epoch: self.published.borrow().epoch,
            },
            Ok(Msg::GetMap { have_epoch }) => {
                let m = self.published.borrow();
                Msg::Map {
                    epoch: m.epoch,
                    // Nothing new: an empty map body saves the encoding.
                    map: if have_epoch >= m.epoch {
                        Vec::new()
                    } else {
                        m.encode()
                    },
                }
            }
            Ok(Msg::PeerDown { reporter, peer }) => {
                self.reports
                    .borrow_mut()
                    .entry(peer)
                    .or_default()
                    .insert(reporter, Instant::now());
                Msg::PeerDownOk
            }
            Ok(Msg::Heartbeat { node }) => {
                let now = Instant::now();
                self.beats.borrow_mut().insert(node, now);
                let lease_ms = if self.fencing.borrow().contains(&node)
                    || self.suspect.borrow().contains(&node)
                {
                    0
                } else {
                    self.granted.borrow_mut().insert(node, now);
                    self.lease.as_millis() as u32
                };
                Msg::HeartbeatOk {
                    map_epoch: self.published.borrow().epoch,
                    lease_ms,
                }
            }
            Ok(Msg::BackfillDone {
                pool,
                pg,
                pg_epoch,
                device,
            }) => {
                let ok = self
                    .map
                    .borrow_mut()
                    .backfill_done(pool, pg, pg_epoch, device);
                let epoch = self.map.borrow().epoch;
                if ok {
                    Msg::BackfillOk { map_epoch: epoch }
                } else {
                    Msg::Error {
                        code: Code::StaleMap,
                        map_epoch: epoch,
                        detail: "PG changed since the backfill started".into(),
                    }
                }
            }
            Ok(Msg::Ping) => Msg::Pong,
            _ => Msg::Error {
                code: Code::Invalid,
                map_epoch: self.map.borrow().epoch,
                detail: "unsupported request".into(),
            },
        }
    }
}

/// Serves the map service on `listener` until the reactor stops.
pub async fn serve(listener: hyperstor_reactor::net::TcpListener, meta: Rc<Meta>) {
    use hyperstor_proto::*;
    use hyperstor_reactor::spawn;
    loop {
        let Ok((s, _)) = listener.accept().await else {
            continue;
        };
        let meta = meta.clone();
        spawn(async move {
            loop {
                let (r, hb) = s.read_exact(vec![0; HEADER_LEN]).await;
                if r.is_err() {
                    return;
                }
                let Ok(h) = Header::decode(hb[..].try_into().unwrap()) else {
                    return;
                };
                let (r, body) = s
                    .read_exact(vec![0; (h.body_len + h.payload_len) as usize])
                    .await;
                if r.is_err() {
                    return;
                }
                let reply = meta.handle(Msg::decode(h.kind, &body[..h.body_len as usize]));
                let (w, _) = s.write_all(encode_frame(h.req, &reply, &[])).await;
                if w.is_err() {
                    return;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> ClusterMap {
        let mut m = ClusterMap::new(7);
        for n in 0..4 {
            let addrs: Vec<SocketAddr> = (0..2)
                .map(|i| format!("127.0.0.1:{}", 7000 + n * 10 + i).parse().unwrap())
                .collect();
            m.add_node(n, &addrs, 0);
        }
        m.add_pool(replicated_pool(1, "rbd", 3, 64));
        m.add_pool(PoolInfo {
            id: 2,
            name: "ec".into(),
            kind: PoolKind::Ec { k: 2, m: 1 },
            size: 3,
            min_size: 3,
            pg_count: 32,
            domain: Domain::Host,
            chunk_size: 4 << 20,
            class: None,
        });
        m.recompute();
        m
    }

    #[test]
    fn encode_round_trip_and_addresses() {
        let m = map();
        assert_eq!(ClusterMap::decode(&m.encode()).unwrap(), m);
        assert_eq!(m.device_addr(2001).unwrap().port(), 7021);
        let pg = m.pg(1, 5).unwrap();
        assert_eq!(pg.acting.len(), 3);
        let hosts: std::collections::HashSet<u32> =
            pg.acting.iter().map(|d| m.devices[d].node).collect();
        assert_eq!(hosts.len(), 3, "copies on distinct nodes");
    }

    #[test]
    fn node_down_changes_only_its_pgs_and_bumps_epochs() {
        let mut m = map();
        let before = m.clone();
        m.nodes.get_mut(&2).unwrap().up = false;
        m.recompute();
        assert_eq!(m.epoch, before.epoch + 1);
        for (k, pg) in &m.pgs {
            let old = &before.pgs[k];
            let used = old
                .acting
                .iter()
                .any(|d| *d != NO_DEVICE && m.devices[d].node == 2);
            if used {
                assert_eq!(pg.epoch, m.epoch, "{k:?}");
                assert!(
                    pg.acting
                        .iter()
                        .all(|d| *d == NO_DEVICE || m.devices[d].node != 2)
                );
            } else if k.0 == 1 {
                assert_eq!(pg, old, "{k:?} changed without involving node 2");
            }
        }
        // EC: surviving shards keep their positions.
        for pg in 0..32 {
            let a = &before.pgs[&(2, pg)].acting;
            let b = &m.pgs[&(2, pg)].acting;
            for (x, y) in a.iter().zip(b) {
                if m.devices[x].node != 2 {
                    assert_eq!(x, y);
                }
            }
        }
    }

    #[test]
    fn degraded_acting_then_backfill() {
        let mut m = map();
        let before = m.clone();
        m.nodes.get_mut(&1).unwrap().up = false;
        m.recompute();
        let mut degraded = 0;
        let keys: Vec<(u32, u32)> = m.pgs.keys().filter(|k| k.0 == 1).copied().collect();
        for k in &keys {
            let pg = m.pgs[k].clone();
            let old = &before.pgs[k];
            assert_eq!(old.acting, old.up, "healthy PGs have acting = up");
            let survivors: Vec<u32> = old
                .acting
                .iter()
                .copied()
                .filter(|d| m.devices[d].node != 1)
                .collect();
            assert_eq!(
                pg.acting, survivors,
                "{k:?}: acting keeps its live members in order"
            );
            if survivors.len() < 3 {
                degraded += 1;
                assert_eq!(pg.primary(), Some(survivors[0]));
                let new: Vec<u32> = pg.backfilling().collect();
                assert_eq!(new.len(), 1, "{k:?}: one new member to fill");
                // A stale epoch is refused; the right one admits it.
                assert!(!m.backfill_done(k.0, k.1, pg.epoch - 1, new[0]));
                let e = m.pgs[k].epoch;
                assert!(m.backfill_done(k.0, k.1, e, new[0]));
                let after = &m.pgs[k];
                assert_eq!(after.acting, after.up);
                assert!(after.epoch > e);
            }
        }
        assert!(degraded > 10);
    }

    #[test]
    fn a_pg_whose_copies_are_all_down_stays_down() {
        let mut m = map();
        let before = m.clone();
        for n in 0..4 {
            m.nodes.get_mut(&n).unwrap().up = false;
        }
        m.recompute();
        for (k, pg) in &m.pgs {
            // Keeps its acting set; nothing would be backfilled from an
            // empty primary when the members return.
            assert_eq!(pg.acting, before.pgs[k].acting, "{k:?}");
        }
        for n in 0..4 {
            m.nodes.get_mut(&n).unwrap().up = true;
        }
        m.recompute();
        for (k, pg) in m.pgs.iter().filter(|(k, _)| k.0 == 1) {
            assert_eq!(pg.acting, before.pgs[k].acting, "{k:?}");
        }
    }

    #[test]
    fn losers_are_devices_that_lose_a_role() {
        let mut m = map();
        let before = m.clone();
        m.nodes.get_mut(&1).unwrap().up = false;
        m.recompute();
        let losers = Meta::losers(&before, &m);
        for (dev, pgs) in &losers {
            for k in pgs {
                let (o, n) = (&before.pgs[k], &m.pgs[k]);
                let removed = !n.acting.contains(dev);
                let lost_primary = o.primary() == Some(*dev) && n.primary() != Some(*dev);
                let moved_shard = k.0 == 2
                    && o.acting.iter().position(|d| d == dev)
                        != n.acting.iter().position(|d| d == dev);
                assert!(removed || lost_primary || moved_shard, "{dev} {k:?}");
            }
        }
        // Node 1's devices lost their roles everywhere they had one.
        for (k, o) in &before.pgs {
            for d in o.acting.iter().filter(|d| m.devices[d].node == 1) {
                assert!(losers.get(d).is_some_and(|v| v.contains(k)), "{d} {k:?}");
            }
        }
        // Survivors keep their positions and primacy: nothing else is fenced.
        assert!(losers.keys().all(|d| m.devices[d].node == 1));
        assert!(Meta::losers(&m, &m).is_empty());
    }

    #[test]
    fn two_peer_reports_mark_a_node_down_and_stop_its_lease() {
        use hyperstor_proto::Msg;
        let m = Meta::with_lease(map(), Duration::from_millis(400));
        let beat = |n: u32| match m.handle(Ok(Msg::Heartbeat { node: n })) {
            Msg::HeartbeatOk { lease_ms, .. } => lease_ms,
            other => panic!("{other:?}"),
        };
        for n in 0..4 {
            assert_eq!(beat(n), 400);
        }
        let timeout = Duration::from_secs(3);
        assert!(!m.liveness_tick(timeout, Instant::now()));
        // One reporter is not enough.
        m.handle(Ok(Msg::PeerDown {
            reporter: 0,
            peer: 3,
        }));
        assert!(!m.liveness_tick(timeout, Instant::now()));
        assert!(m.map.borrow().nodes[&3].up);
        // Two are, even though node 3's heartbeats still arrive.
        m.handle(Ok(Msg::PeerDown {
            reporter: 1,
            peer: 3,
        }));
        assert!(m.liveness_tick(timeout, Instant::now()));
        assert!(!m.map.borrow().nodes[&3].up);
        assert_eq!(beat(3), 0, "no lease while reported");
        assert_eq!(beat(0), 400);
        // Reports go stale; heartbeats bring it back.
        let later = Instant::now() + timeout;
        beat(0);
        beat(1);
        beat(2);
        m.beats.borrow_mut().insert(3, later);
        for n in 0..3 {
            m.beats.borrow_mut().insert(n, later);
        }
        assert!(m.liveness_tick(timeout, later));
        assert!(m.map.borrow().nodes[&3].up);
        assert_eq!(beat(3), 400);
    }

    #[test]
    fn cache_pools_follow_placement_without_backfill() {
        let mut m = map();
        m.add_pool(PoolInfo {
            id: 3,
            name: "kv".into(),
            kind: PoolKind::Cache,
            size: 1,
            min_size: 1,
            pg_count: 32,
            domain: Domain::Host,
            chunk_size: 4 << 20,
            class: None,
        });
        m.recompute();
        let before = m.clone();
        m.nodes.get_mut(&1).unwrap().up = false;
        m.recompute();
        for pg in 0..32 {
            let o = &before.pgs[&(3, pg)];
            let n = &m.pgs[&(3, pg)];
            assert_eq!(n.acting, n.up, "cache PGs never backfill");
            assert_eq!(n.acting.len(), 1);
            assert!(n.backfilling().next().is_none());
            if before.devices[&o.acting[0]].node == 1 {
                assert_ne!(n.acting, o.acting, "the lost copy is replaced, empty");
                assert!(n.epoch > o.epoch);
            } else {
                assert_eq!(n, o);
            }
        }
        let parsed =
            super::parse_cluster("node 0 rack 0 127.0.0.1:9100\npool 3 kv cache copies 2 pgs 16\n")
                .unwrap();
        assert_eq!(parsed.pools[&3].kind, PoolKind::Cache);
        assert_eq!(parsed.pools[&3].size, 2);
        assert_eq!(ClusterMap::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn map_service_answers() {
        use hyperstor_proto::*;
        use hyperstor_reactor::{block_on, net::*, spawn};
        let m = Meta::new(map());
        let want = m.map.borrow().clone();
        block_on(async move {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            spawn(serve(l, m));
            let s = TcpStream::connect(addr).await.unwrap();
            let (w, _) = s
                .write_all(encode_frame(9, &Msg::GetMap { have_epoch: 0 }, &[]))
                .await;
            w.unwrap();
            let (r, hb) = s.read_exact(vec![0; HEADER_LEN]).await;
            r.unwrap();
            let h = Header::decode(hb[..].try_into().unwrap()).unwrap();
            assert_eq!(h.req, 9);
            let (r, body) = s.read_exact(vec![0; h.body_len as usize]).await;
            r.unwrap();
            let Msg::Map { epoch, map } = Msg::decode(h.kind, &body).unwrap() else {
                panic!()
            };
            assert_eq!(epoch, want.epoch);
            assert_eq!(ClusterMap::decode(&map).unwrap(), want);
        });
    }
}

/// Parses a cluster file:
///
/// ```text
/// # comments and blank lines are ignored
/// cluster 42
/// node 0 rack 0 10.0.0.1:9100 10.0.0.1:9101   # one address per reactor
/// node 1 rack 0 10.0.0.2:9100 10.0.0.2:9101
/// pool 1 vol replicated size 3 min 2 pgs 128 chunk 4194304 domain host
/// ```
///
/// and returns the map with every PG placed.
pub fn parse_cluster(text: &str) -> Result<ClusterMap, String> {
    let mut map = ClusterMap::new(0);
    for (lineno, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let w: Vec<&str> = line.split_whitespace().collect();
        let bad = |what: &str| format!("line {}: {what}: {line}", lineno + 1);
        let num = |s: &str| s.parse::<u64>().map_err(|_| bad("expected a number"));
        match w[0] {
            "cluster" if w.len() == 2 => map.cluster_id = num(w[1])? as u128,
            "node" if w.len() >= 5 && w[2] == "rack" => {
                let addrs: Vec<SocketAddr> = w[4..]
                    .iter()
                    .map(|a| a.parse().map_err(|_| bad("bad address")))
                    .collect::<Result<_, _>>()?;
                map.add_node(num(w[1])? as u32, &addrs, num(w[3])? as u32);
            }
            "pool" if w.len() >= 4 => {
                let mut p = replicated_pool(num(w[1])? as u32, w[2], 3, 128);
                let mut i = 3;
                if w[3] == "replicated" {
                    i = 4;
                } else if w[3] == "cache" {
                    // cache [copies N]: one copy unless told otherwise
                    p.kind = PoolKind::Cache;
                    p.size = 1;
                    p.min_size = 1;
                    i = 4;
                } else if let Some(km) = w[3].strip_prefix("ec") {
                    // ec4+2
                    let (k, m) = km.split_once('+').ok_or_else(|| bad("ec needs k+m"))?;
                    let (k, m) = (num(k)? as u8, num(m)? as u8);
                    p.kind = PoolKind::Ec { k, m };
                    p.size = k + m;
                    p.min_size = k + 1;
                    i = 4;
                }
                while i + 1 < w.len() {
                    match w[i] {
                        "size" | "copies" => p.size = num(w[i + 1])? as u8,
                        "min" => p.min_size = num(w[i + 1])? as u8,
                        "pgs" => p.pg_count = num(w[i + 1])? as u32,
                        "chunk" => p.chunk_size = num(w[i + 1])? as u32,
                        "class" => p.class = Some(w[i + 1].to_string()),
                        "domain" => {
                            p.domain = match w[i + 1] {
                                "device" => Domain::Device,
                                "host" => Domain::Host,
                                "rack" => Domain::Rack,
                                _ => return Err(bad("domain is device, host or rack")),
                            }
                        }
                        _ => return Err(bad("unknown pool option")),
                    }
                    i += 2;
                }
                if !p.pg_count.is_power_of_two() {
                    return Err(bad("pgs must be a power of two"));
                }
                map.add_pool(p);
            }
            _ => return Err(bad("unknown line")),
        }
    }
    map.recompute();
    Ok(map)
}

#[cfg(test)]
mod parse_tests {
    #[test]
    fn cluster_file() {
        let m = super::parse_cluster(
            "cluster 7\n# x\nnode 0 rack 0 127.0.0.1:9100 127.0.0.1:9101\nnode 1 rack 1 127.0.0.1:9200\nnode 2 rack 1 127.0.0.1:9300\n\
             pool 1 vol replicated size 3 min 2 pgs 64 chunk 1048576 domain host\npool 2 cold ec2+1 pgs 32\n",
        )
        .unwrap();
        assert_eq!(m.cluster_id, 7);
        assert_eq!(m.devices.len(), 4);
        assert_eq!(m.pools[&1].chunk_size, 1 << 20);
        assert_eq!(m.pools[&2].size, 3);
        assert!(super::parse_cluster("pool 1 v replicated pgs 3").is_err());
        assert!(super::parse_cluster("node x rack 0 1.2.3.4:1").is_err());
    }
}
