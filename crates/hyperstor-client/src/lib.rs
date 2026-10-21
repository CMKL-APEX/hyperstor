//! The client library (spec/volumes.md §10.1).
//!
//! A [`Client`] holds the cluster map and connections to storage reactors.
//! Volume I/O is split at chunk boundaries; each piece goes to its PG's
//! primary, all pieces in parallel. A `StaleMap` or `NotPrimary` answer, or
//! a broken connection, refreshes the map and retries.

pub use hyperstor_rpc as rpc;

use hyperstor_meta::ClusterMap;
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
const BLOCK: u64 = 4096;
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
}

/// The name of a volume chunk object: lineage then chunk index.
pub fn chunk_name(lineage: u128, chunk: u64) -> Vec<u8> {
    let mut n = lineage.to_be_bytes().to_vec();
    n.extend_from_slice(&chunk.to_be_bytes());
    n
}

/// A connection to the cluster.
pub struct Client {
    /// The map service's members (decisions.md D72).
    members: RefCell<Vec<SocketAddr>>,
    /// The member believed to lead, an index into `members`.
    leader: std::cell::Cell<usize>,
    map: RefCell<Rc<ClusterMap>>,
    conns: RefCell<HashMap<SocketAddr, Conn>>,
    pub id: u64,
    pub instance: u64,
}

fn random_u64() -> u64 {
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
            conns: RefCell::new(HashMap::new()),
            id,
            instance: random_u64(),
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
                Ok(conn) => conn.call(msg, &[]).await.map_err(ClientError::Rpc),
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

    /// A connection to `addr`, opened (with `Hello`) if needed.
    pub async fn conn(&self, addr: SocketAddr) -> Result<Conn, ClientError> {
        if let Some(c) = self.conns.borrow().get(&addr)
            && !c.is_dead()
        {
            return Ok(c.clone());
        }
        let c = Conn::connect(addr).await?;
        c.call(
            &Msg::Hello {
                min_version: VERSION,
                max_version: VERSION,
                id: self.id,
                instance: self.instance,
            },
            &[],
        )
        .await?;
        self.conns.borrow_mut().insert(addr, c.clone());
        Ok(c)
    }

    /// Sends `msg` to the primary of the PG that holds chunk `chunk` of
    /// `lineage` in `pool`, retrying across map changes. `make` builds the
    /// message for the current map.
    async fn to_primary(
        &self,
        pool: u32,
        lineage: u128,
        chunk: u64,
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
            let pg = pg_of(lineage, chunk, p.pg_count);
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
            let key = WireKey {
                ns: NS_VOL,
                name: chunk_name(lineage, chunk),
                version: 0,
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
                    let (msg, _) = me
                        .to_primary(pool, lineage, chunk, piece, &|to, key| Msg::Write {
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
                    let (msg, payload) = me
                        .to_primary(pool, lineage, chunk, &[], &|to, key| Msg::Read {
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
}
