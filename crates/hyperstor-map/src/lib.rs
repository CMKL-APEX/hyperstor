//! The cluster map (spec/06-metadata.md §4.1): nodes, devices (one per engine
//! shard), pools and each PG's acting set; the first member of an acting
//! set is the PG's primary. [`ClusterMap::recompute`] places every PG with
//! `hyperstor-placement`; EC pools keep shard positions across changes
//! (01-implementation-notes.md I16). Each device records the address its
//! reactor listens on.
//!
//! Its own crate so that clients (`hyperstor-client`) and the map service
//! (`hyperstor-meta`, which re-exports it) can both use it, and the map
//! service can use the client.

use hyperstor_codec::{Dec, DecodeError, Enc};
use hyperstor_placement::{Device, Domain, Rule, Topology, assign_positions};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

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
    /// Recomputable data (04-decisions.md D45, 09-replication.md §8.7): `size`
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
    /// primary first (07-placement.md §5.3). For EC pools the position is the shard
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
    /// Client instances storage nodes refuse (06-metadata.md §4.1, §9): the
    /// map epoch each was added at, and when it expires (Unix seconds).
    pub blocklist: BTreeMap<u64, Blocked>,
    /// Devices marked out because their node stayed down past the
    /// down-out interval (07-placement.md §5.3); marked in again when it
    /// returns. Devices an operator marked out are not here.
    pub auto_out: BTreeSet<u32>,
    /// Devices that failed (09-replication.md §15): out until an operator
    /// marks them in.
    pub failed: BTreeSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocked {
    pub epoch: u64,
    pub expires: u64,
}

/// How long a blocklist entry lasts (06-metadata.md §4.1). A blocklisted
/// client has long stopped writing by then: its lease renewals are refused.
pub const BLOCKLIST_FOR: std::time::Duration = std::time::Duration::from_secs(3600);

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

    /// Adds the `.meta` pool (06-metadata.md §4.2) if the map lacks it:
    /// replicated, three copies (fewer on smaller clusters), 256 PGs.
    /// Whether it was added; the caller recomputes.
    pub fn ensure_meta_pool(&mut self) -> bool {
        if self.pools.contains_key(&META_POOL) {
            return false;
        }
        let size = self.nodes.len().clamp(1, 3) as u8;
        let mut p = replicated_pool(META_POOL, ".meta", size, 256);
        p.min_size = if size >= 3 { 2 } else { 1 };
        self.add_pool(p);
        true
    }

    /// Raises `.meta`'s copies as hosts join, to min(3, nodes), with `min`
    /// 2 from three copies (as [`Self::ensure_meta_pool`] sizes it). Never
    /// lowers it. Whether it changed; the caller recomputes.
    pub fn grow_meta_pool(&mut self) -> bool {
        let want = self.nodes.len().clamp(1, 3) as u8;
        match self.pools.get_mut(&META_POOL) {
            Some(p) if p.size < want => {
                p.size = want;
                p.min_size = if want >= 3 { 2 } else { 1 };
                true
            }
            _ => false,
        }
    }

    pub fn add_pool(&mut self, pool: PoolInfo) {
        assert!(pool.pg_count.is_power_of_two());
        self.pools.insert(pool.id, pool);
    }

    /// Adds the nodes, devices and pools of `seed` (a cluster file) that
    /// this map lacks, and recomputes if any were. What this map has is
    /// kept as it is: removals go through `out` and `in`. Whether
    /// anything was added.
    pub fn merge_new(&mut self, seed: &ClusterMap) -> bool {
        let mut added = false;
        for (id, n) in &seed.nodes {
            if !self.nodes.contains_key(id) {
                self.nodes.insert(*id, n.clone());
                added = true;
            }
        }
        for (id, d) in &seed.devices {
            if !self.devices.contains_key(id) {
                self.devices.insert(*id, d.clone());
                added = true;
            }
        }
        for (id, p) in &seed.pools {
            if !self.pools.contains_key(id) {
                self.pools.insert(*id, p.clone());
                added = true;
            }
        }
        if added {
            self.recompute();
        }
        added
    }

    /// Whether `device` is in placement and its node up: it can serve.
    pub fn live(&self, device: u32) -> bool {
        self.devices
            .get(&device)
            .is_some_and(|d| d.up_in && self.nodes.get(&d.node).is_some_and(|n| n.up))
    }

    /// Members of a PG's `up` being backfilled now: not acting, and live.
    /// A member whose node is down waits, without moving data, until it
    /// returns or the down-out interval marks it out.
    pub fn filling(&self, pg: &PgInfo) -> Vec<u32> {
        pg.backfilling().filter(|d| self.live(*d)).collect()
    }

    /// Places every PG for the current devices and bumps the epoch
    /// (07-placement.md §5.3):
    /// - `up` is where placement wants the PG: every device in placement,
    ///   down or not, so a short reboot moves no data (down and out);
    ///   for cache pools, only live devices, since nothing is copied;
    /// - `acting` keeps its live members, in order, so a failed primary is
    ///   replaced by the next member, which has every acknowledged write;
    /// - a new PG, or one that lost every copy, starts with `acting = up`.
    ///
    /// PGs whose sets change get the new epoch.
    pub fn recompute(&mut self) {
        self.epoch += 1;
        let place = |live_only: bool| -> Vec<Device> {
            self.devices
                .values()
                .map(|d| {
                    let node = &self.nodes[&d.node];
                    Device {
                        id: d.id,
                        host: d.node,
                        rack: node.rack,
                        weight: d.weight,
                        class: d.class.clone(),
                        up_in: d.up_in && (node.up || !live_only),
                    }
                })
                .collect()
        };
        let (devs_in, devs_live) = (place(false), place(true));
        let pools: Vec<PoolInfo> = self.pools.values().cloned().collect();
        for p in pools {
            let rule = Rule {
                pool: p.id,
                size: p.size as usize,
                domain: p.domain,
                class: p.class.clone(),
            };
            let devs = if p.kind == PoolKind::Cache {
                &devs_live
            } else {
                &devs_in
            };
            let topo = Topology::new(&rule, devs);
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
                    // Never had a copy (the pool was made before any host
                    // joined): nothing to keep, so start where placement
                    // wants it, as a new PG does.
                    Some(o) if o.acting.iter().all(|d| *d == NO_DEVICE) => up.clone(),
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

    /// Records that `device` has a full copy of a PG (09-replication.md §8.5). It
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

    /// Blocklists `instance` until `expires` (Unix seconds) and bumps the
    /// epoch; returns the epoch that has it. An instance already listed
    /// keeps its epoch, with the later expiry.
    pub fn blocklist_add(&mut self, instance: u64, expires: u64) -> u64 {
        if let Some(b) = self.blocklist.get_mut(&instance) {
            if b.expires < expires {
                self.epoch += 1;
                b.expires = expires;
            }
            return b.epoch;
        }
        self.epoch += 1;
        self.blocklist.insert(
            instance,
            Blocked {
                epoch: self.epoch,
                expires,
            },
        );
        self.epoch
    }

    /// Drops entries expired at `now` (Unix seconds), bumping the epoch if
    /// any were; whether any were.
    pub fn blocklist_prune(&mut self, now: u64) -> bool {
        let before = self.blocklist.len();
        self.blocklist.retain(|_, b| b.expires > now);
        let pruned = self.blocklist.len() != before;
        if pruned {
            self.epoch += 1;
        }
        pruned
    }

    /// Marks the in-placement devices of `node` out, as the down-out
    /// interval does; whether any were.
    pub fn auto_out_node(&mut self, node: u32) -> bool {
        let mut changed = false;
        for d in self.devices.values_mut().filter(|d| d.node == node) {
            if d.up_in && !self.failed.contains(&d.id) {
                d.up_in = false;
                self.auto_out.insert(d.id);
                changed = true;
            }
        }
        changed
    }

    /// Marks in again the devices of `node` that were marked out because
    /// it was down; whether any were.
    pub fn auto_in_node(&mut self, node: u32) -> bool {
        let mut changed = false;
        for d in self.devices.values_mut().filter(|d| d.node == node) {
            if self.auto_out.remove(&d.id) {
                d.up_in = true;
                changed = true;
            }
        }
        changed
    }

    /// Marks a device failed and out; whether that changed anything.
    pub fn fail_device(&mut self, device: u32) -> bool {
        let Some(d) = self.devices.get_mut(&device) else {
            return false;
        };
        let changed = d.up_in || !self.failed.contains(&device);
        d.up_in = false;
        self.auto_out.remove(&device);
        self.failed.insert(device);
        changed
    }

    /// An operator marks a device in or out: that overrides the down-out
    /// interval and a failure. Whether it changed anything.
    pub fn set_in(&mut self, device: u32, up_in: bool) -> Option<bool> {
        let d = self.devices.get_mut(&device)?;
        let changed =
            d.up_in != up_in || self.auto_out.contains(&device) || self.failed.contains(&device);
        d.up_in = up_in;
        self.auto_out.remove(&device);
        self.failed.remove(&device);
        Some(changed)
    }

    /// Takes `device` out of a PG's acting set (09-replication.md §15, D91: a
    /// replica too slow to answer), if the PG is still at `pg_epoch` and
    /// keeps `min_size` copies; it stays in `up`, so it is backfilled back.
    /// Bumps the epoch; whether it was taken out.
    pub fn drop_member(&mut self, pool: u32, pg: u32, pg_epoch: u64, device: u32) -> bool {
        let Some(p) = self.pools.get(&pool) else {
            return false;
        };
        let (min, ec) = (p.min_size as usize, matches!(p.kind, PoolKind::Ec { .. }));
        let Some(info) = self.pgs.get(&(pool, pg)) else {
            return false;
        };
        if info.epoch != pg_epoch
            || info.primary() == Some(device)
            || !info.acting.contains(&device)
            || info.acting_count() <= min
        {
            return false;
        }
        let mut info = info.clone();
        if ec {
            for d in info.acting.iter_mut().filter(|d| **d == device) {
                *d = NO_DEVICE;
            }
        } else {
            info.acting.retain(|d| *d != device);
        }
        self.epoch += 1;
        info.epoch = self.epoch;
        self.pgs.insert((pool, pg), info);
        true
    }

    pub fn blocked(&self, instance: u64) -> bool {
        !self.blocklist.is_empty() && self.blocklist.contains_key(&instance)
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
        // Format 2 adds the blocklist, 3 the devices out by the down-out
        // interval and the failed ones; a map without them keeps the
        // older format, which nodes from before them read.
        let format = if !self.auto_out.is_empty() || !self.failed.is_empty() {
            3
        } else if !self.blocklist.is_empty() {
            2
        } else {
            1
        };
        e.u32(format).u64(self.epoch).u128(self.cluster_id);
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
        if format >= 2 {
            e.u32(self.blocklist.len() as u32);
            for (i, b) in &self.blocklist {
                e.u64(*i).u64(b.epoch).u64(b.expires);
            }
        }
        if format >= 3 {
            for set in [&self.auto_out, &self.failed] {
                e.u32(set.len() as u32);
                for d in set {
                    e.u32(*d);
                }
            }
        }
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut d = Dec::new(b);
        let v = d.u32()?;
        if !(1..=3).contains(&v) {
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
        if v >= 2 {
            for _ in 0..d.u32()? {
                let i = d.u64()?;
                m.blocklist.insert(
                    i,
                    Blocked {
                        epoch: d.u64()?,
                        expires: d.u64()?,
                    },
                );
            }
        }
        if v >= 3 {
            m.auto_out = d.seq(4, |d| d.u32())?.into_iter().collect();
            m.failed = d.seq(4, |d| d.u32())?.into_iter().collect();
        }
        d.finish()?;
        Ok(m)
    }
}

/// A replicated pool with defaults for tests and small clusters.
/// The `.meta` pool's id (06-metadata.md §4.2): volume headers and
/// directories. Reserved; added by [`ClusterMap::ensure_meta_pool`].
pub const META_POOL: u32 = 0;

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
                if num(w[1])? as u32 == META_POOL {
                    return Err(bad("pool 0 is reserved for .meta"));
                }
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
    map.ensure_meta_pool();
    map.recompute();
    Ok(map)
}

#[cfg(test)]
mod join_tests {
    /// A pool made before any host joined starts serving once hosts do:
    /// its PGs had no copy anywhere, so nothing is waited for.
    #[test]
    fn pools_made_before_any_host_place_their_pgs_when_hosts_join() {
        let mut m =
            super::parse_cluster("cluster 1\npool 1 v replicated size 2 min 1 pgs 8\n").unwrap();
        assert!(m.pgs.values().all(|pg| pg.acting.is_empty()));
        for n in 0..2u32 {
            let a: std::net::SocketAddr = format!("127.0.0.1:{}", 9000 + n).parse().unwrap();
            m.add_node(n, &[a], 0);
        }
        m.recompute();
        for (k, pg) in &m.pgs {
            assert!(!pg.acting.is_empty(), "{k:?}");
            assert_eq!(pg.acting, pg.up, "{k:?}: nothing to backfill");
        }
        // A PG whose copies are all down still waits for them.
        let before = m.pgs[&(1, 0)].acting.clone();
        for n in m.nodes.values_mut() {
            n.up = false;
        }
        m.recompute();
        assert_eq!(m.pgs[&(1, 0)].acting, before);
    }
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
        // `.meta` is added, sized to the cluster; pool 0 is reserved.
        assert_eq!(m.pools[&super::META_POOL].size, 3);
        assert_eq!(m.pools[&super::META_POOL].name, ".meta");
        assert!(super::parse_cluster("pool 0 x replicated pgs 8").is_err());
        let one = super::parse_cluster("node 0 rack 0 1.2.3.4:1\n").unwrap();
        assert_eq!(
            (
                one.pools[&super::META_POOL].size,
                one.pools[&super::META_POOL].min_size
            ),
            (1, 1)
        );
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
        let mut m = map();
        assert_eq!(ClusterMap::decode(&m.encode()).unwrap(), m);
        // Format 1 without a blocklist, 2 with one.
        assert_eq!(m.encode()[..4], 1u32.to_le_bytes());
        let e = m.blocklist_add(77, 1000);
        assert_eq!((e, m.epoch), (m.epoch, e));
        assert_eq!(m.encode()[..4], 2u32.to_le_bytes());
        assert_eq!(ClusterMap::decode(&m.encode()).unwrap(), m);
        assert!(m.blocked(77) && !m.blocked(78));
        // Listed again: same epoch; a later expiry is a change.
        assert_eq!(m.blocklist_add(77, 1000), e);
        assert_eq!(m.epoch, e);
        assert_eq!(m.blocklist_add(77, 2000), e);
        assert_eq!(m.epoch, e + 1);
        assert!(!m.blocklist_prune(2000 - 1));
        assert!(m.blocklist_prune(2000));
        assert!(!m.blocked(77));
        assert_eq!(m.epoch, e + 2);
        // Format 3 with devices out by the interval, or failed.
        assert!(m.auto_out_node(1));
        assert!(!m.auto_out_node(1));
        assert!(m.fail_device(2000));
        assert_eq!(m.encode()[..4], 3u32.to_le_bytes());
        assert_eq!(ClusterMap::decode(&m.encode()).unwrap(), m);
        assert!(m.devices.values().filter(|d| d.node == 1).all(|d| !d.up_in));
        // A returning node comes back in; a failed device doesn't.
        assert!(m.auto_in_node(1));
        assert!(m.devices.values().filter(|d| d.node == 1).all(|d| d.up_in));
        assert!(!m.auto_in_node(2) && !m.devices[&2000].up_in);
        assert_eq!(m.set_in(2000, true), Some(true));
        assert!(m.failed.is_empty() && m.auto_out.is_empty());
        assert_eq!(m.encode()[..4], 1u32.to_le_bytes());
        assert_eq!(m.device_addr(2001).unwrap().port(), 7021);
        let pg = m.pg(1, 5).unwrap();
        assert_eq!(pg.acting.len(), 3);
        let hosts: std::collections::HashSet<u32> =
            pg.acting.iter().map(|d| m.devices[d].node).collect();
        assert_eq!(hosts.len(), 3, "copies on distinct nodes");
    }

    #[test]
    fn a_slow_member_leaves_one_pg_and_is_backfilled_back() {
        let mut m = map();
        let before = m.pg(1, 5).unwrap().clone();
        let (primary, slow) = (before.acting[0], before.acting[2]);
        assert!(
            !m.drop_member(1, 5, before.epoch, primary),
            "not the primary"
        );
        assert!(!m.drop_member(1, 5, before.epoch + 1, slow), "stale epoch");
        assert!(m.drop_member(1, 5, before.epoch, slow));
        let pg = m.pg(1, 5).unwrap().clone();
        assert_eq!(pg.epoch, m.epoch);
        assert!(!pg.acting.contains(&slow) && pg.up.contains(&slow));
        assert_eq!(pg.backfilling().collect::<Vec<_>>(), vec![slow]);
        // Never below min_size (2 of 3).
        assert!(!m.drop_member(1, 5, pg.epoch, pg.acting[1]));
        // Recomputing keeps it out of acting until its backfill is done.
        m.recompute();
        assert!(!m.pg(1, 5).unwrap().acting.contains(&slow));
        let e = m.pg(1, 5).unwrap().epoch;
        assert!(m.backfill_done(1, 5, e, slow));
        assert_eq!(m.pg(1, 5).unwrap().acting, before.acting);
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
                // Down, not out: placement still wants it; nothing moves.
                assert_eq!(pg.up, old.up, "{k:?}: a down node keeps its place");
                assert!(m.filling(&pg).is_empty());
            }
        }
        assert!(degraded > 10);
        // Out after the interval: its PGs get new members to fill.
        assert!(m.auto_out_node(1));
        m.recompute();
        for k in &keys {
            let pg = m.pgs[k].clone();
            if pg.acting.len() < 3 {
                let new = m.filling(&pg);
                assert_eq!(new.len(), 1, "{k:?}: one new member to fill");
                assert!(m.devices[&new[0]].node != 1);
                // A stale epoch is refused; the right one admits it.
                assert!(!m.backfill_done(k.0, k.1, pg.epoch - 1, new[0]));
                let e = m.pgs[k].epoch;
                assert!(m.backfill_done(k.0, k.1, e, new[0]));
                let after = &m.pgs[k];
                assert_eq!(after.acting, after.up);
                assert!(after.epoch > e);
            }
        }
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
}
