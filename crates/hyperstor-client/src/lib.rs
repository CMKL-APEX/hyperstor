//! The client library (spec/10-volumes.md §10.1).
//!
//! A [`Client`] holds the cluster map and connections to storage reactors.
//! Volume I/O is split at chunk boundaries; each piece goes to its PG's
//! primary, all pieces in parallel. A `StaleMap` or `NotPrimary` answer, or
//! a broken connection, refreshes the map and retries.

pub use hyperstor_rpc as rpc;

pub mod cache;
pub mod copy;
pub mod snapshot;
pub mod trash;
pub mod volume;

use hyperstor_map::ClusterMap;
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
pub const NS_VOL: u8 = hyperstor_placement::NS_VOL;
/// `.meta` objects: headers, lineages, directories, groups (06-metadata.md §4.2).
pub const NS_META: u8 = 3;
pub(crate) const BLOCK: u64 = 4096;

/// What an empty device answers to a read, for a cache PG waiting for its
/// down member (D189): a miss for a plain read, zeros from no version for
/// a volume's, nothing found. None: not a read.
fn empty_answer(msg: &Msg) -> Option<Result<rpc::Reply, ClientError>> {
    let none = |len: u64| vec![(u32::MAX, 0u64); len.div_ceil(BLOCK) as usize];
    Some(match msg {
        Msg::Read { to, .. } => Err(ClientError::Server {
            code: Code::NotFound,
            detail: format!(
                "not in the cache: PG {}.{} waits for its device",
                to.pool, to.pg
            ),
        }),
        Msg::ReadAt { len, .. } => {
            let data = vec![0; *len as usize];
            Ok((
                Msg::ReadAtOk {
                    crcs: block_crcs(&data),
                    sources: none(*len),
                },
                data,
            ))
        }
        Msg::Sources { len, .. } => Ok((
            Msg::ReadAtOk {
                crcs: Vec::new(),
                sources: none(*len),
            },
            Vec::new(),
        )),
        Msg::Lookup { .. } => Ok((Msg::LookupOk { found: false }, Vec::new())),
        Msg::Versions { .. } => Ok((
            Msg::VersionsOk {
                versions: Vec::new(),
            },
            Vec::new(),
        )),
        _ => return None,
    })
}
/// Tries of a request to a PG's primary that fail other than by the
/// primary answering it can't serve the PG yet (no primary in the map,
/// no connection): at 10 ms more each time, about 1.9 s in all.
const RETRIES: usize = 20;

/// How long a request is tried again while the PG's primary answers it
/// can't serve it yet (`Unavailable`, `StaleMap`, `NotPrimary`): a PG
/// splitting, fenced for a map change, or a map the client hasn't got.
/// Each lasts a few commits, which a slow or busy device can stretch past
/// RETRIES's 1.9 s; a guest then sees a pause rather than an I/O error
/// (D296).
const BUSY_WAIT: Duration = Duration::from_secs(30);

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
    #[error(
        "the volume is attached by {label:?} under lease {holder:016x} for {remaining_ms} ms more"
    )]
    LeaseHeld {
        holder: u64,
        delegates: Vec<u64>,
        remaining_ms: u32,
        /// The holder's label (D153).
        label: String,
    },
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

/// The name of a chunk's versions in `branch` (08-engine.md §7.2): the chunk
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

/// A request's payload: copied into its frame, or `buf[range]` sent as it
/// is when large ([`rpc::Frame::part`]).
enum Payload<'a> {
    Copy(&'a [u8]),
    Part(&'a hyperstor_codec::IoBuf, std::ops::Range<usize>),
}

/// The PG of a named object: by a hash of its name.
pub use hyperstor_placement::object_pg;

/// A connection to the cluster.
pub struct Client {
    /// The map service's members (04-decisions.md D72).
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
    /// Recorded with every lease this client takes, for whoever finds the
    /// volume held (D153): `hyperstor-vhost` names its node, tools
    /// themselves.
    lease_label: RefCell<String>,
    /// The host it runs on, as it names itself (`--host`, D261), told to
    /// each peer that knows [`Msg::Host`]; empty: none.
    host: RefCell<String>,
    /// The data networks this client is on, in order (26-networks.md
    /// §29.5, D207): a device is reached at its endpoint on the first it
    /// has one on, else at its address.
    networks: RefCell<Vec<String>>,
    /// For choosing among a device's endpoints.
    rng: RefCell<rand::rngs::SmallRng>,
}

/// `label` cut to [`hyperstor_proto::LABEL_MAX`] bytes, at a character
/// boundary.
pub fn cut_label(label: &str) -> String {
    let mut end = label.len().min(hyperstor_proto::LABEL_MAX);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label[..end].to_string()
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
    /// redirects (04-decisions.md D72).
    pub async fn connect_members(
        members: Vec<SocketAddr>,
        id: u64,
    ) -> Result<Rc<Client>, ClientError> {
        Self::connect_members_as(members, id, random_u64().max(1)).await
    }

    /// [`Self::connect_members`] as client instance `instance` (never 0):
    /// for a process with a client on each of its reactors, all one
    /// instance to the cluster (04-decisions.md D291), so a blocklist entry
    /// for it fences them all.
    pub async fn connect_members_as(
        members: Vec<SocketAddr>,
        id: u64,
        instance: u64,
    ) -> Result<Rc<Client>, ClientError> {
        assert!(!members.is_empty(), "no map service members");
        assert!(instance != 0, "instance 0 is a storage node's peers'");
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
            instance,
            lease_label: RefCell::new(String::new()),
            host: RefCell::new(String::new()),
            networks: RefCell::new(Vec::new()),
            rng: RefCell::new(rand::SeedableRng::seed_from_u64(random_u64())),
        });
        c.refresh_map().await?;
        Ok(c)
    }

    pub fn map(&self) -> Rc<ClusterMap> {
        self.map.borrow().clone()
    }

    /// The data networks this client reaches storage nodes on, in order
    /// (D207); none: devices' addresses.
    pub fn set_networks(&self, networks: Vec<String>) {
        *self.networks.borrow_mut() = networks;
    }

    /// Where to reach `device` in `map` for one request: among its
    /// endpoints on this client's first network that has any, by two
    /// random choices, weighted by their ports' weights, taking the one
    /// with fewer requests outstanding (D208); else its address.
    pub fn addr_of(&self, map: &ClusterMap, device: u32) -> Option<SocketAddr> {
        let addrs = map.weighted_addrs_on(device, &self.networks.borrow());
        if addrs.len() < 2 {
            return addrs.first().map(|(a, _)| *a);
        }
        let loads: Vec<hyperstor_fabric::path::PathLoad> = addrs
            .iter()
            .map(|(a, w)| hyperstor_fabric::path::PathLoad {
                weight: *w,
                bytes_in_flight: self.conns.pending(*a) as u64,
                rtt_us: 0,
            })
            .collect();
        let i = hyperstor_fabric::path::choose(&loads, &mut *self.rng.borrow_mut())?;
        Some(addrs[i].0)
    }

    /// The label recorded with the leases this client takes from now on
    /// (D153), cut to [`hyperstor_proto::LABEL_MAX`] bytes.
    pub fn set_lease_label(&self, label: &str) {
        *self.lease_label.borrow_mut() = cut_label(label);
    }

    pub fn lease_label(&self) -> String {
        self.lease_label.borrow().clone()
    }

    /// This machine's hostname, the default host name of `hyperstor-vhost`
    /// and `hyperstor-ublk` (D261); empty if it can't be read.
    pub fn this_host() -> String {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_string())
            .unwrap_or_default()
    }

    /// The host this client names at each connection from now on (D261),
    /// for realm bindings (D259), cut to [`hyperstor_proto::LABEL_MAX`]
    /// bytes. Connections already open keep the name they gave.
    pub fn set_host(&self, host: &str) {
        *self.host.borrow_mut() = cut_label(host);
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
        self.call_meta_with(msg, &[]).await
    }

    /// [`Self::call_meta`] with a payload.
    pub async fn call_meta_with(
        &self,
        msg: &Msg,
        payload: &[u8],
    ) -> Result<rpc::Reply, ClientError> {
        let tries = 2 * self.members.borrow().len() + 2;
        let mut last = None;
        for _ in 0..tries {
            let addr = self.member();
            let reply = match self.conn(addr).await {
                // A member that doesn't answer is let go: the next is tried.
                Ok(conn) => conn
                    .call_within(msg, payload, rpc::CALL_TIMEOUT)
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
            min_version: hyperstor_proto::OLDEST_VERSION,
            max_version: VERSION,
            id: self.id,
            instance: self.instance,
        };
        let host = self.host.borrow().clone();
        let c = self
            .conns
            .get_with(addr, |addr| async move {
                let c = Conn::connect(addr).await?;
                let (ok, _) = c.call_within(&hello, &[], rpc::CALL_TIMEOUT).await?;
                if let Msg::HelloOk { version, .. } = ok {
                    c.set_version(version);
                }
                // Only a peer that knows it: older ones refuse what they
                // can't decode (D261).
                if c.version() >= 3 && !host.is_empty() {
                    let name = Msg::Host { name: host };
                    c.call_within(&name, &[], rpc::CALL_TIMEOUT).await?;
                }
                Ok(c)
            })
            .await?;
        Ok(c)
    }

    /// Sends `msg` to the primary of the PG that holds chunk `chunk` of
    /// `lineage` in `pool`, retrying across map changes, under the lease
    /// identity `lease` (D150; 0 for none). `make` builds the message for
    /// the current map.
    pub(crate) async fn to_primary(
        &self,
        pool: u32,
        route: Route<'_>,
        lease: u64,
        payload: &[u8],
        make: &dyn Fn(Target, WireKey) -> Msg,
    ) -> Result<rpc::Reply, ClientError> {
        self.send_primary(pool, route, lease, Payload::Copy(payload), make)
            .await
    }

    /// [`Client::to_primary`] with a payload that may be part of a shared
    /// buffer, sent without a copy when large.
    async fn send_primary(
        &self,
        pool: u32,
        route: Route<'_>,
        lease: u64,
        payload: Payload<'_>,
        make: &dyn Fn(Target, WireKey) -> Msg,
    ) -> Result<rpc::Reply, ClientError> {
        let start = std::time::Instant::now();
        let mut last = String::new();
        // Tries that failed other than by a primary answering it can't
        // serve the PG yet: they give up after RETRIES. Those answers
        // (a split, a fence, a map change) give up only after BUSY_WAIT
        // (D296).
        let mut others = 0;
        let mut busy = false;
        let mut attempt = 0;
        loop {
            if attempt > 0 {
                if !busy {
                    others += 1;
                }
                if others >= RETRIES || start.elapsed() >= BUSY_WAIT {
                    return Err(ClientError::Exhausted(attempt, last));
                }
                sleep(Duration::from_millis((10 * attempt as u64).min(200))).await;
                let _ = self.refresh_map().await;
            }
            attempt += 1;
            busy = false;
            let map = self.map();
            let p = map.pools.get(&pool).ok_or(ClientError::NoPool(pool))?;
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
            // By the key sent, as a device splitting the PG goes by it
            // (24-pools-and-devices.md §27.5).
            let pg = p.pg_of_key(key.ns, &key.name);
            let info = map.pg(pool, pg).ok_or(ClientError::NoPrimary(pool, pg))?;
            let Some(primary) = info.primary() else {
                last = format!("PG {pool}.{pg} has no primary");
                continue;
            };
            let Some(addr) = self.addr_of(&map, primary) else {
                last = format!("device {primary} has no address");
                continue;
            };
            let to = Target {
                pool,
                pg,
                map_epoch: map.epoch,
                pg_epoch: info.epoch,
                lease,
            };
            // A cache PG waiting for its down member (D189): a read is
            // answered at once as an empty device answers it; a write ends
            // the wait, so the PG moves and takes the write empty, and is
            // sent again.
            if p.kind == hyperstor_map::PoolKind::Cache && !map.live(primary) {
                if let Some(r) = empty_answer(&make(to, key.clone())) {
                    return r;
                }
                last = match self.call_meta(&Msg::CacheRelease { pool, pg }).await {
                    Ok(_) => format!(
                        "cache PG {pool}.{pg} waited for device {primary}; released for a write"
                    ),
                    Err(e) => e.to_string(),
                };
                continue;
            }
            let conn = match self.conn(addr).await {
                Ok(c) => c,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let sent = match &payload {
                Payload::Copy(p) => conn.call(&make(to, key), p).await,
                Payload::Part(buf, range) => {
                    conn.start_part(&make(to, key), buf, range.clone()).await
                }
            };
            match sent {
                Ok((
                    Msg::Error {
                        code: Code::StaleMap | Code::NotPrimary | Code::Unavailable,
                        detail,
                        ..
                    },
                    _,
                )) => {
                    last = detail;
                    busy = true;
                }
                Ok((Msg::Error { code, detail, .. }, _)) => {
                    return Err(ClientError::Server { code, detail });
                }
                Ok(r) => return Ok(r),
                Err(e) => last = e.to_string(),
            }
        }
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
    /// every copy of every chunk has it (spec/09-replication.md §8.1).
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
    /// `epoch` (08-engine.md §7.2): each chunk's version `epoch`.
    pub async fn write_at(
        self: &Rc<Self>,
        pool: u32,
        lineage: u128,
        branch: u32,
        epoch: u64,
        off: u64,
        data: Vec<u8>,
    ) -> Result<(), ClientError> {
        self.write_under(0, pool, lineage, branch, epoch, off, data)
            .await
    }

    /// [`Self::write_at`] under the lease identity `lease` (D150), as an
    /// [`volume::Attached`] writes; refused once `lease` is fenced.
    #[allow(clippy::too_many_arguments)]
    pub async fn write_under(
        self: &Rc<Self>,
        lease: u64,
        pool: u32,
        lineage: u128,
        branch: u32,
        epoch: u64,
        off: u64,
        data: Vec<u8>,
    ) -> Result<(), ClientError> {
        let pieces = self.pieces(pool, off, data.len() as u64)?;
        // Each piece is sent from `data` as it is when large (HS5b, I324).
        let data = hyperstor_codec::IoBuf::from(data);
        let handles: Vec<_> = pieces
            .into_iter()
            .map(|(chunk, coff, roff, n)| {
                let me = self.clone();
                let data = data.clone();
                spawn(async move {
                    let range = roff as usize..(roff + n) as usize;
                    let crcs = block_crcs(&data[range.clone()]);
                    let route = Route::Chunk {
                        lineage,
                        chunk,
                        branch,
                        epoch,
                    };
                    let (msg, _) = me
                        .send_primary(
                            pool,
                            route,
                            lease,
                            Payload::Part(&data, range),
                            &|to, key| Msg::Write {
                                to,
                                key,
                                off: coff,
                                crcs: crcs.clone(),
                            },
                        )
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
                        .to_primary(pool, route, 0, &[], &|to, key| Msg::Read {
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
    /// first, then its clone parents' (08-engine.md §7.2).
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
        self.read_layers(0, pool, lineage, layers, off, len, true)
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
            .read_layers(0, pool, lineage, layers, off, len, false)
            .await?
            .1)
    }

    /// [`Self::read_sourced`] (with `data`) or [`Self::sources`], under
    /// the lease identity `lease` (D150).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn read_layers(
        self: &Rc<Self>,
        lease: u64,
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
                        .to_primary(pool, route, lease, &[], &|to, key| {
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
        self.to_primary(pool, route, 0, &[], make).await
    }

    /// Applies `ops` to the object `name` of namespace `ns` in `pool`,
    /// atomically, if its assertions hold (06-metadata.md §4.2): an assertion
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
        let send = async |ops: &[hyperstor_proto::WireOp]| {
            let payload = hyperstor_proto::encode_ops(ops);
            let (msg, _) = self
                .to_primary(pool, route, 0, &payload, &|to, key| Msg::Exec { to, key })
                .await?;
            match msg {
                Msg::ExecOk => Ok(()),
                m => Err(ClientError::Rpc(RpcError::Protocol(format!(
                    "unexpected {m:?}"
                )))),
            }
        };
        let crcs =
            |o: &hyperstor_proto::WireOp| matches!(o, hyperstor_proto::WireOp::WriteCrcs { .. });
        match send(ops).await {
            // A primary from before D279 can't decode checksummed writes:
            // once more as plain writes, which it checks as before.
            Err(ClientError::Server {
                code: Code::Invalid,
                ..
            }) if ops.iter().any(crcs) => {
                let plain: Vec<_> = ops.iter().cloned().map(|o| o.without_crcs()).collect();
                send(&plain).await
            }
            r => r,
        }
    }

    /// A write op for [`Client::exec`] that carries the CRC32C of each
    /// 4 KiB block of `data` (D279), checked by every copy on arrival and
    /// stored as given: computed here, as early as the data is whole.
    /// `off` and `data`'s length are multiples of 4096.
    pub fn write_op(off: u64, data: Vec<u8>) -> hyperstor_proto::WireOp {
        let crcs = hyperstor_proto::block_crcs(&data);
        hyperstor_proto::WireOp::WriteCrcs { off, data, crcs }
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
        self.lookup_at(pool, ns, name, kv_from, kv_to, kv_limit, false)
            .await
    }

    /// [`Self::lookup`], with the key–value table's size in bytes
    /// (`kv_bytes`, 25-kv-ranges.md §28.2). Only storage nodes on a build
    /// that has it answer: an older one refuses the message.
    pub async fn lookup_sized(
        &self,
        pool: u32,
        ns: u8,
        name: &[u8],
        kv_from: &[u8],
        kv_to: Option<&[u8]>,
        kv_limit: u32,
    ) -> Result<Option<hyperstor_proto::Looked>, ClientError> {
        self.lookup_at(pool, ns, name, kv_from, kv_to, kv_limit, true)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn lookup_at(
        &self,
        pool: u32,
        ns: u8,
        name: &[u8],
        kv_from: &[u8],
        kv_to: Option<&[u8]>,
        kv_limit: u32,
        sized: bool,
    ) -> Result<Option<hyperstor_proto::Looked>, ClientError> {
        let (msg, payload) = self
            .to_primary(pool, Route::Object { ns, name }, 0, &[], &|to, key| {
                Msg::Lookup {
                    to,
                    key,
                    kv_from: kv_from.to_vec(),
                    kv_to: kv_to.map(|t| t.to_vec()),
                    kv_limit,
                    sized,
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

    /// Reads `len` bytes at `off` of object `name` of namespace `ns`, both
    /// multiples of 4096; past its end, zeros. A missing object is
    /// `Server { code: NotFound, .. }`.
    pub async fn read_object(
        &self,
        pool: u32,
        ns: u8,
        name: &[u8],
        off: u64,
        len: u64,
    ) -> Result<Vec<u8>, ClientError> {
        if !off.is_multiple_of(4096) || !len.is_multiple_of(4096) {
            return Err(ClientError::Unaligned);
        }
        let (msg, payload) = self
            .to_primary(pool, Route::Object { ns, name }, 0, &[], &|to, key| {
                Msg::Read { to, key, off, len }
            })
            .await?;
        match msg {
            Msg::ReadOk { crcs } => {
                verify_crcs(&payload, &crcs).map_err(|_| ClientError::Checksum)?;
                Ok(payload)
            }
            m => Err(ClientError::Rpc(RpcError::Protocol(format!(
                "unexpected {m:?}"
            )))),
        }
    }
}
