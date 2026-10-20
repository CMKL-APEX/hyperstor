//! The cluster map (spec/design.md §4.1) and a map service.
//!
//! The map names nodes, devices (one per engine shard), pools and each PG's
//! acting set; the first member of an acting set is the PG's primary.
//! [`ClusterMap::recompute`] places every PG with `hyperstor-placement`;
//! EC pools keep shard positions across changes (implementation-notes.md
//! I16).
//!
//! [`serve`] answers `GetMap` from one in-memory map. Replicating the map
//! with Raft (design D2, D20) comes later (implementation-notes.md I18).
//! Each device records the address its reactor listens on.

use hyperstor_codec::{Dec, DecodeError, Enc};
use hyperstor_placement::{Device, Domain, Rule, Topology, assign_positions};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::rc::Rc;

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
    Ec { k: u8, m: u8 },
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
    /// Map epoch of the last change to this PG's acting set.
    pub epoch: u64,
    /// Devices holding the PG, primary first. For EC pools the position is
    /// the shard index, and a missing shard is `u32::MAX`.
    pub acting: Vec<u32>,
}

impl PgInfo {
    pub fn primary(&self) -> Option<u32> {
        self.acting.first().copied().filter(|d| *d != u32::MAX)
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
        for p in self.pools.values() {
            let rule = Rule {
                pool: p.id,
                size: p.size as usize,
                domain: p.domain,
                class: p.class.clone(),
            };
            let topo = Topology::new(&rule, &devs);
            for pg in 0..p.pg_count {
                let members = topo.place(pg);
                let acting = match p.kind {
                    PoolKind::Replicated => members,
                    PoolKind::Ec { .. } => {
                        let prev: Vec<Option<u32>> = match self.pgs.get(&(p.id, pg)) {
                            Some(i) => i
                                .acting
                                .iter()
                                .map(|d| (*d != NO_DEVICE).then_some(*d))
                                .collect(),
                            None => vec![None; p.size as usize],
                        };
                        assign_positions(&prev, &members)
                            .into_iter()
                            .map(|d| d.unwrap_or(NO_DEVICE))
                            .collect()
                    }
                };
                let e = self.pgs.entry((p.id, pg)).or_insert(PgInfo {
                    epoch: self.epoch,
                    acting: Vec::new(),
                });
                if e.acting != acting {
                    e.acting = acting;
                    e.epoch = self.epoch;
                }
            }
        }
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
            m.pgs.insert((pool, pg), PgInfo { epoch, acting });
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

/// Serves the map on `listener` until the reactor stops: `Hello` and
/// `GetMap` (answered with the whole map when the asker's is older).
pub async fn serve(listener: hyperstor_reactor::net::TcpListener, map: Rc<RefCell<ClusterMap>>) {
    use hyperstor_proto::*;
    use hyperstor_reactor::spawn;
    loop {
        let Ok((s, _)) = listener.accept().await else {
            continue;
        };
        let map = map.clone();
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
                let reply = match Msg::decode(h.kind, &body[..h.body_len as usize]) {
                    Ok(Msg::Hello { .. }) => Msg::HelloOk {
                        version: VERSION,
                        node: 0,
                        map_epoch: map.borrow().epoch,
                    },
                    Ok(Msg::GetMap { .. }) => {
                        let m = map.borrow();
                        Msg::Map {
                            epoch: m.epoch,
                            map: m.encode(),
                        }
                    }
                    Ok(Msg::Ping) => Msg::Pong,
                    _ => Msg::Error {
                        code: Code::Invalid,
                        map_epoch: map.borrow().epoch,
                        detail: "unsupported request".into(),
                    },
                };
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
    fn map_service_answers() {
        use hyperstor_proto::*;
        use hyperstor_reactor::{block_on, net::*, spawn};
        let m = Rc::new(RefCell::new(map()));
        let want = m.borrow().clone();
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
