//! The client library (spec/volumes.md §10.1).
//!
//! A [`Client`] holds the cluster map and connections to storage reactors.
//! Volume I/O is split at chunk boundaries; each piece goes to its PG's
//! primary, all pieces in parallel. A `StaleMap` or `NotPrimary` answer, or
//! a broken connection, refreshes the map and retries.

pub use hyperstor_rpc as rpc;

pub mod cache;
pub mod snapshot;
pub mod trash;
pub mod volume;

use hyperstor_map::ClusterMap;
use hyperstor_placement::pg_of;
use hyperstor_proto::{Code, Msg, Target, VERSION, WireKey, block_crcs, verify_crcs};
use hyperstor_reactor::spawn;
use hyperstor_reactor::time::sleep;
use rpc::{Conn, RpcError};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;
use thiserror::Error;

/// Namespace of volume chunks (matches `hyperstor_engine::Namespace::Vol`).
pub const NS_VOL: u8 = 1;
/// `.meta` objects: headers, lineages, directories, groups (metadata.md §4.2).
pub const NS_META: u8 = 3;
pub(crate) const BLOCK: u64 = 4096;
const RETRIES: usize = 20;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ClientError {
    #[error("rpc: {0}")]
    Rpc(#[from] RpcError),
    #[error("server error {code:?}: {detail}")]
    Server { code: Code, detail: String },
    #[error("unknown pool {0}")]
    NoPool(u32),
    #[error("offset or length not a multiple of 4096")]
    Unaligned,
    #[error("PG {0}.{1} has no primary")]
    NoPrimary(u32, u32),
    #[error("data checksum mismatch")]
    Checksum,
    #[error("gave up after {0} attempts: {1}")]
    Exhausted(usize, String),
    #[error("no volume {0:032x}")]
    NoVolume(u128),
    #[error("volume {0:032x} exists")]
    VolumeExists(u128),
    #[error("volumes don't shrink: {have} bytes, asked for {want}")]
    Shrink { have: u64, want: u64 },
    #[error("bad volume request: {0}")]
    BadVolume(String),
    #[error("the volume is attached by client instance {holder:016x} for {remaining_ms} ms more")]
    LeaseHeld { holder: u64, remaining_ms: u32 },
    #[error("the lease on volume {0:032x} is lost")]
    LeaseLost(u128),
    #[error("volume {0:032x} is attached")]
    VolumeAttached(u128),
    #[error("no snapshot at epoch {0}")]
    NoSnapshot(u64),
    #[error("the snapshot at epoch {0} has clones (or a rollback) that depend on it")]
    SnapshotInUse(u64),
    #[error("{0}")]
    Refused(String),
}

/// The name of a volume chunk object: lineage then chunk index.
pub fn chunk_name(lineage: u128, chunk: u64) -> Vec<u8> {
    let mut n = lineage.to_be_bytes().to_vec();
    n.extend_from_slice(&chunk.to_be_bytes());
    n
}

/// The name of a chunk's versions in `branch` (engine.md §7.2): the chunk
/// name, then the branch (big-endian), except for branch 0, which keeps
/// the chunk name alone, as volumes had before branches.
pub fn branch_name(chunk: &[u8], branch: u32) -> Vec<u8> {
    let mut n = chunk.to_vec();
    if branch != 0 {
        n.extend_from_slice(&branch.to_be_bytes());
    }
    n
}

/// Where a request goes: a volume's chunk, or a named object (`.meta`).
#[derive(Clone, Copy)]
pub(crate) enum Route<'a> {
    /// A volume chunk's version: the chunk's name in `branch`, at `epoch`.
    Chunk {
        lineage: u128,
        chunk: u64,
        branch: u32,
        epoch: u64,
    },
    Object {
        ns: u8,
        name: &'a [u8],
    },
}

/// The PG of a named object: by a hash of its name.
pub fn object_pg(name: &[u8], pg_count: u32) -> u32 {
    // FNV-1a: the same on every client and version.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    pg_of(u128::from(h), 0, pg_count)
}

/// A connection to the cluster.
pub struct Client {
    /// The map service's members (decisions.md D72).
    members: RefCell<Vec<SocketAddr>>,
    /// The member believed to lead, an index into `members`.
    leader: std::cell::Cell<usize>,
    map: RefCell<Rc<ClusterMap>>,
    conns: rpc::Pool,
    /// Volumes attached through this client, whose leases it renews
    /// (`volume`), and whether the renewing task runs.
    attached: RefCell<HashMap<u128, std::rc::Weak<volume::Attached>>>,
    renewing: std::cell::Cell<bool>,
    /// Blocks of versions that can't change, for every volume this client
    /// serves (D102).
    pub(crate) cache: RefCell<cache::ReadCache>,
    pub id: u64,
    pub instance: u64,
}

pub fn random_u64() -> u64 {
    let mut v = 0u64;
    // SAFETY: getrandom writes at most 8 bytes into `v`.
    unsafe { libc::getrandom(&mut v as *mut u64 as *mut libc::c_void, 8, 0) };
    v
}

impl Client {
    /// Connects to the map service at `meta` and fetches the map.
    pub async fn connect(meta: SocketAddr, id: u64) -> Result<Rc<Client>, ClientError> {
        Self::connect_members(vec![meta], id).await
    }

    /// Connects to a map service of several members and fetches the map
    /// from any of them. Requests only the leader answers follow its
    /// redirects (decisions.md D72).
    pub async fn connect_members(
        members: Vec<SocketAddr>,
        id: u64,
    ) -> Result<Rc<Client>, ClientError> {
        assert!(!members.is_empty(), "no map service members");
        let c = Rc::new(Client {
            members: RefCell::new(members),
            leader: std::cell::Cell::new(0),
            map: RefCell::new(Rc::new(ClusterMap::default())),
            conns: Default::default(),
            attached: RefCell::new(HashMap::new()),
            renewing: std::cell::Cell::new(false),
            cache: RefCell::new(cache::ReadCache::new(cache::DEFAULT_BYTES)),
            id,
            // Never 0: storage nodes take 0 for peers, which are never
            // blocklisted.
            instance: random_u64().max(1),
        });
        c.refresh_map().await?;
        Ok(c)
    }

    pub fn map(&self) -> Rc<ClusterMap> {
        self.map.borrow().clone()
    }

    /// Installs `m` if it is newer than the map held (a pushed map).
    pub fn install_map(&self, m: ClusterMap) {
        if m.epoch > self.map.borrow().epoch {
            *self.map.borrow_mut() = Rc::new(m);
        }
    }

    /// The member requests go to first.
    fn member(&self) -> SocketAddr {
        let m = self.members.borrow();
        m[self.leader.get() % m.len()]
    }

    /// Try the next member next time (this one didn't answer).
    fn next_member(&self) {
        let n = self.members.borrow().len();
        self.leader.set((self.leader.get() + 1) % n);
    }

    /// Fetches the map if the service has a newer one. Any member serves
    /// it, from its applied copy; one that can't be reached is skipped.
    pub async fn refresh_map(&self) -> Result<(), ClientError> {
        let have = self.map.borrow().epoch;
        let tries = self.members.borrow().len();
        let mut last = None;
        let mut reply = None;
        for _ in 0..tries {
            let addr = self.member();
            match self.conn(addr).await {
                Ok(conn) => match conn.call(&Msg::GetMap { have_epoch: have }, &[]).await {
                    Ok((m, _)) => {
                        reply = Some(m);
                        break;
                    }
                    Err(e) => last = Some(ClientError::Rpc(e)),
                },
                Err(e) => last = Some(e),
            }
            self.next_member();
        }
        let Some(msg) = reply else {
            return Err(last.expect("at least one member"));
        };
        match msg {
            Msg::Map { epoch, map } if epoch > have => {
                let m = ClusterMap::decode(&map)
                    .map_err(|e| ClientError::Rpc(RpcError::Protocol(e.to_string())))?;
                *self.map.borrow_mut() = Rc::new(m);
            }
            Msg::Map { .. } => {}
            Msg::Error { code, detail, .. } => return Err(ClientError::Server { code, detail }),
            m => {
                return Err(ClientError::Rpc(RpcError::Protocol(format!(
                    "unexpected {m:?}"
                ))));
            }
        }
        Ok(())
    }

    /// Sends a request only the map service's leader answers (heartbeats,
    /// reports, backfill completion), following its redirects: a member
    /// that isn't the leader names it, or says it doesn't know yet (an
    /// election), and one that doesn't answer is skipped.
    pub async fn call_meta(&self, msg: &Msg) -> Result<rpc::Reply, ClientError> {
        let tries = 2 * self.members.borrow().len() + 2;
        let mut last = None;
        for _ in 0..tries {
            let addr = self.member();
            let reply = match self.conn(addr).await {
                // A member that doesn't answer is let go: the next is tried.
                Ok(conn) => conn
                    .call_within(msg, &[], rpc::CALL_TIMEOUT)
                    .await
                    .map_err(ClientError::Rpc),
                Err(e) => Err(e),
            };
            match reply {
                Ok((
                    Msg::Error {
                        code: hyperstor_proto::Code::NotLeader,
                        detail,
                        ..
                    },
                    _,
                )) => {
                    match detail.parse::<SocketAddr>() {
                        Ok(leader) if leader != addr => self.follow(leader),
                        // No leader yet: an election is on.
                        _ => {
                            self.next_member();
                            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(20))
                                .await;
                        }
                    }
                    last = Some(ClientError::Server {
                        code: hyperstor_proto::Code::NotLeader,
                        detail: "no leader".into(),
                    });
                }
                Ok(r) => return Ok(r),
                Err(e) => {
                    self.next_member();
                    last = Some(e);
                }
            }
        }
        Err(last.expect("at least one try"))
    }

    /// Send leader-only requests to `leader` from now on.
    fn follow(&self, leader: SocketAddr) {
        let mut m = self.members.borrow_mut();
        let i = match m.iter().position(|a| *a == leader) {
            Some(i) => i,
            None => {
                m.push(leader);
                m.len() - 1
            }
        };
        self.leader.set(i);
    }

    /// A connection to `addr`, opened (with `Hello`) if needed; callers at
    /// once share one connect (I184).
    pub async fn conn(&self, addr: SocketAddr) -> Result<Conn, ClientError> {
        let hello = Msg::Hello {
            min_version: VERSION,
            max_version: VERSION,
            id: self.id,
            instance: self.instance,
        };
        let c = self
            .conns
            .get_with(addr, |addr| async move {
                let c = Conn::connect(addr).await?;
                c.call_within(&hello, &[], rpc::CALL_TIMEOUT).await?;
                Ok(c)
            })
            .await?;
        Ok(c)
    }

    /// Sends `msg` to the primary of the PG that holds chunk `chunk` of
    /// `lineage` in `pool`, retrying across map changes. `make` builds the
    /// message for the current map.
    pub(crate) async fn to_primary(
        &self,
        pool: u32,
        route: Route<'_>,
        payload: &[u8],
        make: &dyn Fn(Target, WireKey) -> Msg,
    ) -> Result<rpc::Reply, ClientError> {
        let mut last = String::new();
        for attempt in 0..RETRIES {
            if attempt > 0 {
                sleep(Duration::from_millis((10 * attempt as u64).min(200))).await;
                let _ = self.refresh_map().await;
            }
            let map = self.map();
            let p = map.pools.get(&pool).ok_or(ClientError::NoPool(pool))?;
            let pg = match route {
                Route::Chunk { lineage, chunk, .. } => pg_of(lineage, chunk, p.pg_count),
                Route::Object { name, .. } => object_pg(name, p.pg_count),
            };
            let info = map.pg(pool, pg).ok_or(ClientError::NoPrimary(pool, pg))?;
            let Some(primary) = info.primary() else {
                last = format!("PG {pool}.{pg} has no primary");
                continue;
            };
            let Some(addr) = map.device_addr(primary) else {
                last = format!("device {primary} has no address");
                continue;
            };
            let to = Target {
                pool,
                pg,
                map_epoch: map.epoch,
                pg_epoch: info.epoch,
            };
            let key = match route {
                Route::Chunk {
                    lineage,
                    chunk,
                    branch,
                    epoch,
                } => WireKey {
                    ns: NS_VOL,
                    name: branch_name(&chunk_name(lineage, chunk), branch),
                    version: epoch,
                },
                Route::Object { ns, name } => WireKey {
                    ns,
                    name: name.to_vec(),
                    version: 0,
                },
            };
            let conn = match self.conn(addr).await {
                Ok(c) => c,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            match conn.call(&make(to, key), payload).await {
                Ok((
                    Msg::Error {
                        code: Code::StaleMap | Code::NotPrimary | Code::Unavailable,
                        detail,
                        ..
                    },
                    _,
                )) => last = detail,
                Ok((Msg::Error { code, detail, .. }, _)) => {
                    return Err(ClientError::Server { code, detail });
                }
                Ok(r) => return Ok(r),
                Err(e) => last = e.to_string(),
            }
        }
        Err(ClientError::Exhausted(RETRIES, last))
    }

    /// Splits `[off, off + len)` of a volume at chunk boundaries:
    /// (chunk, offset in chunk, offset in the request, length).
    fn pieces(
        &self,
        pool: u32,
        off: u64,
        len: u64,
    ) -> Result<Vec<(u64, u64, u64, u64)>, ClientError> {
        if !off.is_multiple_of(BLOCK) || !len.is_multiple_of(BLOCK) {
            return Err(ClientError::Unaligned);
        }
        let cs = self
            .map()
            .pools
            .get(&pool)
            .ok_or(ClientError::NoPool(pool))?
            .chunk_size as u64;
        let mut out = Vec::new();
        let mut at = off;
        while at < off + len {
            let chunk = at / cs;
            let in_chunk = at % cs;
            let n = (cs - in_chunk).min(off + len - at);
            out.push((chunk, in_chunk, at - off, n));
            at += n;
        }
        Ok(out)
    }

    /// Writes `data` at `off` of volume `lineage` in `pool`. Returns once
    /// every copy of every chunk has it (spec/replication.md §8.1).
    pub async fn write(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        off: u64,
        data: Vec<u8>,
    ) -> Result<(), ClientError> {
        self.write_at(pool, lineage, 0, 0, off, data).await
    }

    /// [`Self::write`] into `branch` of lineage `lineage`, at its head epoch
    /// `epoch` (engine.md §7.2): each chunk's version `epoch`.
    pub async fn write_at(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        branch: u32,
        epoch: u64,
        off: u64,
        data: Vec<u8>,
    ) -> Result<(), ClientError> {
        let pieces = self.pieces(pool, off, data.len() as u64)?;
        let data = Rc::new(data);
        let handles: Vec<_> = pieces
            .into_iter()
            .map(|(chunk, coff, roff, n)| {
                let me = self.clone();
                let data = data.clone();
                spawn(async move {
                    let piece = &data[roff as usize..(roff + n) as usize];
                    let crcs = block_crcs(piece);
                    let route = Route::Chunk {
                        lineage,
                        chunk,
                        branch,
                        epoch,
                    };
                    let (msg, _) = me
                        .to_primary(pool, route, piece, &|to, key| Msg::Write {
                            to,
                            key,
                            off: coff,
                            crcs: crcs.clone(),
                        })
                        .await?;
                    match msg {
                        Msg::WriteOk { .. } => Ok(()),
                        m => Err(ClientError::Rpc(RpcError::Protocol(format!(
                            "unexpected {m:?}"
                        )))),
                    }
                })
            })
            .collect();
        for h in handles {
            h.await?;
        }
        Ok(())
    }

    /// Reads `len` bytes at `off` of volume `lineage` in `pool`.
    pub async fn read(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        off: u64,
        len: u64,
    ) -> Result<Vec<u8>, ClientError> {
        let pieces = self.pieces(pool, off, len)?;
        let handles: Vec<_> = pieces
            .into_iter()
            .map(|(chunk, coff, roff, n)| {
                let me = self.clone();
                spawn(async move {
                    let route = Route::Chunk {
                        lineage,
                        chunk,
                        branch: 0,
                        epoch: 0,
                    };
                    let (msg, payload) = me
                        .to_primary(pool, route, &[], &|to, key| Msg::Read {
                            to,
                            key,
                            off: coff,
                            len: n,
                        })
                        .await?;
                    match msg {
                        Msg::ReadOk { crcs } => {
                            verify_crcs(&payload, &crcs).map_err(|_| ClientError::Checksum)?;
                            Ok((roff, payload))
                        }
                        m => Err(ClientError::Rpc(RpcError::Protocol(format!(
                            "unexpected {m:?}"
                        )))),
                    }
                })
            })
            .collect();
        let mut out = vec![0u8; len as usize];
        for h in handles {
            let (roff, p) = h.await?;
            out[roff as usize..roff as usize + p.len()].copy_from_slice(&p);
        }
        Ok(out)
    }

    /// Reads `len` bytes at `off` of a volume in lineage `lineage` through
    /// `layers`: (branch, oldest epoch, newest epoch), the volume's own
    /// first, then its clone parents' (engine.md §7.2).
    pub async fn read_at(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        layers: &[(u32, u64, u64)],
        off: u64,
        len: u64,
    ) -> Result<Vec<u8>, ClientError> {
        Ok(self.read_sourced(pool, lineage, layers, off, len).await?.0)
    }

    /// [`Self::read_at`], with the (branch, epoch) each block came from;
    /// `u32::MAX` for a hole.
    pub async fn read_sourced(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        layers: &[(u32, u64, u64)],
        off: u64,
        len: u64,
    ) -> Result<(Vec<u8>, Vec<(u32, u64)>), ClientError> {
        self.read_layers(pool, lineage, layers, off, len, true)
            .await
    }

    /// Where each block [`Self::read_at`] would read comes from, without
    /// the data.
    pub async fn sources(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        layers: &[(u32, u64, u64)],
        off: u64,
        len: u64,
    ) -> Result<Vec<(u32, u64)>, ClientError> {
        Ok(self
            .read_layers(pool, lineage, layers, off, len, false)
            .await?
            .1)
    }

    async fn read_layers(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        layers: &[(u32, u64, u64)],
        off: u64,
        len: u64,
        data: bool,
    ) -> Result<(Vec<u8>, Vec<(u32, u64)>), ClientError> {
        let pieces = self.pieces(pool, off, len)?;
        let layers: Rc<Vec<(u32, u64, u64)>> = Rc::new(layers.to_vec());
        let handles: Vec<_> = pieces
            .into_iter()
            .map(|(chunk, coff, roff, n)| {
                let (me, layers) = (self.clone(), layers.clone());
                spawn(async move {
                    let route = Route::Chunk {
                        lineage,
                        chunk,
                        branch: 0,
                        epoch: 0,
                    };
                    let (msg, payload) = me
                        .to_primary(pool, route, &[], &|to, key| {
                            let layers = layers.to_vec();
                            if data {
                                Msg::ReadAt {
                                    to,
                                    key,
                                    layers,
                                    off: coff,
                                    len: n,
                                }
                            } else {
                                Msg::Sources {
                                    to,
                                    key,
                                    layers,
                                    off: coff,
                                    len: n,
                                }
                            }
                        })
                        .await?;
                    match msg {
                        Msg::ReadAtOk { crcs, sources } if sources.len() as u64 == n / BLOCK => {
                            verify_crcs(&payload, &crcs).map_err(|_| ClientError::Checksum)?;
                            Ok((roff, payload, sources))
                        }
                        m => Err(ClientError::Rpc(RpcError::Protocol(format!(
                            "unexpected {m:?}"
                        )))),
                    }
                })
            })
            .collect();
        let mut out = vec![0u8; if data { len as usize } else { 0 }];
        let mut all = Vec::with_capacity((len / BLOCK) as usize);
        for h in handles {
            let (roff, p, sources) = h.await?;
            if data {
                out[roff as usize..roff as usize + p.len()].copy_from_slice(&p);
            }
            all.extend(sources);
        }
        Ok((out, all))
    }

    /// Sets the size of the read cache (D102); 0 turns it off.
    pub fn set_read_cache_bytes(&self, bytes: usize) {
        self.cache.borrow_mut().set_bytes(bytes);
    }

    pub fn read_cache_stats(&self) -> cache::Stats {
        self.cache.borrow().stats()
    }

    /// Sends `make`'s message about chunk `chunk` of `lineage` (its name in
    /// `branch`, at `epoch`) to the chunk's primary.
    pub(crate) async fn to_chunk(
        &self,
        pool: u32,
        lineage: u128,
        chunk: u64,
        branch: u32,
        epoch: u64,
        make: &dyn Fn(Target, WireKey) -> Msg,
    ) -> Result<rpc::Reply, ClientError> {
        let route = Route::Chunk {
            lineage,
            chunk,
            branch,
            epoch,
        };
        self.to_primary(pool, route, &[], make).await
    }

    /// Applies `ops` to the object `name` of namespace `ns` in `pool`,
    /// atomically, if its assertions hold (metadata.md §4.2): an assertion
    /// that fails is `ClientError::Server { code: Conflict, .. }`, and
    /// nothing is changed. Assertions apply to the object as it was before
    /// the list. An error after the primary accepted the list (a replica
    /// failed) leaves it unknown whether it was applied: read and compare.
    pub async fn exec(
        &self,
        pool: u32,
        ns: u8,
        name: &[u8],
        ops: &[hyperstor_proto::WireOp],
    ) -> Result<(), ClientError> {
        self.exec_at(pool, Route::Object { ns, name }, ops).await
    }

    async fn exec_at(
        &self,
        pool: u32,
        route: Route<'_>,
        ops: &[hyperstor_proto::WireOp],
    ) -> Result<(), ClientError> {
        let payload = hyperstor_proto::encode_ops(ops);
        let (msg, _) = self
            .to_primary(pool, route, &payload, &|to, key| Msg::Exec { to, key })
            .await?;
        match msg {
            Msg::ExecOk => Ok(()),
            m => Err(ClientError::Rpc(RpcError::Protocol(format!(
                "unexpected {m:?}"
            )))),
        }
    }

    /// The object `name`: its size, attributes, and up to `kv_limit` of its
    /// key–value entries in `[kv_from, kv_to)` (none: to the end); `None` if
    /// it doesn't exist. Read from the primary under its map lease, so it
    /// sees every change acknowledged before.
    pub async fn lookup(
        &self,
        pool: u32,
        ns: u8,
        name: &[u8],
        kv_from: &[u8],
        kv_to: Option<&[u8]>,
        kv_limit: u32,
    ) -> Result<Option<hyperstor_proto::Looked>, ClientError> {
        let (msg, payload) = self
            .to_primary(pool, Route::Object { ns, name }, &[], &|to, key| {
                Msg::Lookup {
                    to,
                    key,
                    kv_from: kv_from.to_vec(),
                    kv_to: kv_to.map(|t| t.to_vec()),
                    kv_limit,
                }
            })
            .await?;
        match msg {
            Msg::LookupOk { found: false } => Ok(None),
            Msg::LookupOk { found: true } => hyperstor_proto::decode_looked(&payload)
                .map(Some)
                .map_err(|e| ClientError::Rpc(RpcError::Protocol(e.to_string()))),
            m => Err(ClientError::Rpc(RpcError::Protocol(format!(
                "unexpected {m:?}"
            )))),
        }
    }
}
