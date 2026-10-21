//! The framed wire protocol (spec/protocol.md §13.1, D5).
//!
//! A frame is a fixed 32-byte header, a typed body and an optional payload
//! (block data, never interpreted by the framing):
//!
//! ```text
//! magic u32 | version u16 | kind u16 | flags u32 | request id u64
//! body len u32 | payload len u32 | header CRC32C u32
//! ```
//!
//! Bodies use the fixed little-endian layouts of `hyperstor-codec`. Block
//! data carries one CRC32C per 4 KiB block in the body, checked by the
//! receiver before it touches the data.

use hyperstor_codec::{Dec, DecodeError, Enc, crc32c};
use thiserror::Error;

pub const MAGIC: u32 = 0x4853_5452; // "HSTR"
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 32;
/// Limits that a receiver enforces before allocating.
pub const MAX_BODY: usize = 1 << 20;
pub const MAX_PAYLOAD: usize = 64 << 20;
/// Flag: this frame is a response.
pub const FLAG_RESPONSE: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u16,
    pub flags: u32,
    pub req: u64,
    pub body_len: u32,
    pub payload_len: u32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("bad frame header")]
    BadHeader,
    #[error("unsupported protocol version {0}")]
    Version(u16),
    #[error("frame too large")]
    TooLarge,
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
    #[error("unknown message kind {0}")]
    Kind(u16),
    #[error("payload checksum mismatch at block {0}")]
    Checksum(usize),
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut e = Enc::with_capacity(HEADER_LEN);
        e.u32(MAGIC)
            .u16(VERSION)
            .u16(self.kind)
            .u32(self.flags)
            .u64(self.req)
            .u32(self.body_len)
            .u32(self.payload_len);
        let c = crc32c(e.as_slice());
        e.u32(c);
        e.into_vec().try_into().unwrap()
    }

    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self, FrameError> {
        let mut d = Dec::new(b);
        if d.u32()? != MAGIC {
            return Err(FrameError::BadHeader);
        }
        let v = d.u16()?;
        let h = Header {
            kind: d.u16()?,
            flags: d.u32()?,
            req: d.u64()?,
            body_len: d.u32()?,
            payload_len: d.u32()?,
        };
        let c = d.u32()?;
        if crc32c(&b[..HEADER_LEN - 4]) != c {
            return Err(FrameError::BadHeader);
        }
        if v != VERSION {
            return Err(FrameError::Version(v));
        }
        if h.body_len as usize > MAX_BODY || h.payload_len as usize > MAX_PAYLOAD {
            return Err(FrameError::TooLarge);
        }
        Ok(h)
    }
}

/// Error codes carried by [`Msg::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Code {
    /// The sender's map is older than the receiver's.
    StaleMap = 1,
    /// The receiver isn't the PG's primary in this epoch.
    NotPrimary = 2,
    NotFound = 3,
    Checksum = 4,
    NoSpace = 5,
    Io = 6,
    Invalid = 7,
    /// Too few copies reachable to accept a write.
    Unavailable = 8,
    /// An assertion in an operation list failed.
    Conflict = 9,
    Blocklisted = 10,
    /// The map service member isn't the leader; `detail` is the leader's
    /// map-service address when known, else empty (decisions.md D72).
    NotLeader = 11,
}

impl Code {
    pub fn from_u16(v: u16) -> Self {
        match v {
            1 => Code::StaleMap,
            2 => Code::NotPrimary,
            3 => Code::NotFound,
            4 => Code::Checksum,
            5 => Code::NoSpace,
            6 => Code::Io,
            8 => Code::Unavailable,
            9 => Code::Conflict,
            10 => Code::Blocklisted,
            11 => Code::NotLeader,
            _ => Code::Invalid,
        }
    }
}

/// Identifies an object on the wire: the engine key without pool and PG,
/// which travel in the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireKey {
    pub ns: u8,
    pub name: Vec<u8>,
    pub version: u64,
}

impl WireKey {
    fn enc(&self, e: &mut Enc) {
        e.u8(self.ns).bytes(&self.name).u64(self.version);
    }
    fn dec(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            ns: d.u8()?,
            name: d.bytes()?.to_vec(),
            version: d.u64()?,
        })
    }
}

/// Where a request is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub pool: u32,
    pub pg: u32,
    pub map_epoch: u64,
    pub pg_epoch: u64,
}

impl Target {
    fn enc(&self, e: &mut Enc) {
        e.u32(self.pool)
            .u32(self.pg)
            .u64(self.map_epoch)
            .u64(self.pg_epoch);
    }
    fn dec(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            pool: d.u32()?,
            pg: d.u32()?,
            map_epoch: d.u64()?,
            pg_epoch: d.u64()?,
        })
    }
}

/// Messages. Data travels in the frame's payload; its per-block CRCs travel
/// in the body (`crcs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Hello {
        min_version: u16,
        max_version: u16,
        /// Client id, or node id for storage nodes.
        id: u64,
        /// Random per process (metadata.md §4.1, blocklist).
        instance: u64,
    },
    HelloOk {
        version: u16,
        node: u64,
        map_epoch: u64,
    },
    Ping,
    Pong,
    GetMap {
        have_epoch: u64,
    },
    /// An encoded cluster map (by `hyperstor-meta`).
    Map {
        epoch: u64,
        map: Vec<u8>,
    },
    Write {
        to: Target,
        key: WireKey,
        off: u64,
        crcs: Vec<u32>,
    },
    WriteOk {
        revision: u64,
    },
    Read {
        to: Target,
        key: WireKey,
        off: u64,
        len: u64,
    },
    ReadOk {
        crcs: Vec<u32>,
    },
    /// Client to primary: apply the operation list in the payload
    /// ([`encode_ops`]) to one object atomically, if its assertions hold
    /// (metadata.md §4.2: compare-and-set on `.meta` objects).
    Exec {
        to: Target,
        key: WireKey,
    },
    ExecOk,
    /// Primary to replica: apply an operation list's effects (no
    /// assertions: the primary decided them), in order with writes.
    ReplExec {
        to: Target,
        seq: u64,
        key: WireKey,
    },
    /// Client to primary: an object's size, attributes, and the
    /// key–value entries in `[kv_from, kv_to)` (none: to the end), at most
    /// `kv_limit` of them.
    Lookup {
        to: Target,
        key: WireKey,
        kv_from: Vec<u8>,
        kv_to: Option<Vec<u8>>,
        kv_limit: u32,
    },
    /// Whether the object exists; if it does, the payload is a
    /// [`Looked`] ([`encode_looked`]).
    LookupOk {
        found: bool,
    },
    /// Primary to a member: replace the object with the payload's
    /// ([`encode_object`]): data, size, attributes and key–value table.
    PushObject {
        to: Target,
        key: WireKey,
    },
    /// Primary to replica: apply this write (replication.md §8.1).
    Repl {
        to: Target,
        seq: u64,
        key: WireKey,
        off: u64,
        crcs: Vec<u32>,
    },
    ReplOk {
        revision: u64,
    },
    /// Storage node to map service: still alive (metadata.md §4.1).
    Heartbeat {
        node: u32,
    },
    /// Renews the sender's map lease (replication.md §8.4, decisions.md
    /// D42): it may serve PG I/O for `lease_ms` from when it sent the
    /// heartbeat. 0 revokes the lease.
    HeartbeatOk {
        map_epoch: u64,
        lease_ms: u32,
    },
    /// Map service to a storage reactor: stop serving these PGs until you
    /// have a map of at least `epoch`; answer once every write already
    /// accepted for them has been answered.
    Fence {
        epoch: u64,
        pgs: Vec<(u32, u32)>,
    },
    FenceOk,
    /// Map service to a storage reactor: a newly published map.
    MapPush {
        epoch: u64,
        map: Vec<u8>,
    },
    MapPushOk,
    /// Storage node to map service: `peer` hasn't answered `reporter`'s
    /// pings for the peer timeout (decisions.md D40).
    PeerDown {
        reporter: u32,
        peer: u32,
    },
    PeerDownOk,
    /// Primary to backfill target: replace the object with the payload
    /// (empty: delete it) (replication.md §8.5).
    Push {
        to: Target,
        key: WireKey,
        crcs: Vec<u32>,
    },
    PushOk,
    /// Primary to a member or backfill target: list what you hold of the
    /// PG, as digests of each object's content (replication.md §8.5).
    /// With `deep`, first read every block of the PG and check its CRC: a
    /// damaged object's digest is [`DAMAGED`].
    Digests {
        to: Target,
        deep: bool,
    },
    /// The digests, in the payload ([`encode_digests`]).
    DigestsOk,
    /// Primary to map service: `device` now has every object of the PG and
    /// may join its acting set.
    BackfillDone {
        pool: u32,
        pg: u32,
        pg_epoch: u64,
        device: u32,
    },
    BackfillOk {
        map_epoch: u64,
    },
    Error {
        code: Code,
        map_epoch: u64,
        detail: String,
    },
}

mod kind {
    pub const HELLO: u16 = 1;
    pub const HELLO_OK: u16 = 2;
    pub const PING: u16 = 3;
    pub const PONG: u16 = 4;
    pub const GET_MAP: u16 = 5;
    pub const MAP: u16 = 6;
    pub const WRITE: u16 = 10;
    pub const WRITE_OK: u16 = 11;
    pub const READ: u16 = 12;
    pub const READ_OK: u16 = 13;
    pub const EXEC: u16 = 14;
    pub const EXEC_OK: u16 = 15;
    pub const LOOKUP: u16 = 16;
    pub const LOOKUP_OK: u16 = 17;
    pub const REPL_EXEC: u16 = 26;
    pub const PUSH_OBJECT: u16 = 27;
    pub const REPL: u16 = 20;
    pub const REPL_OK: u16 = 21;
    pub const PUSH: u16 = 22;
    pub const PUSH_OK: u16 = 23;
    pub const DIGESTS: u16 = 24;
    pub const DIGESTS_OK: u16 = 25;
    pub const HEARTBEAT: u16 = 30;
    pub const HEARTBEAT_OK: u16 = 31;
    pub const BACKFILL_DONE: u16 = 32;
    pub const BACKFILL_OK: u16 = 33;
    pub const FENCE: u16 = 34;
    pub const FENCE_OK: u16 = 35;
    pub const MAP_PUSH: u16 = 36;
    pub const MAP_PUSH_OK: u16 = 37;
    pub const PEER_DOWN: u16 = 38;
    pub const PEER_DOWN_OK: u16 = 39;
    pub const ERROR: u16 = 99;
}

impl Msg {
    pub fn kind(&self) -> u16 {
        use kind::*;
        match self {
            Msg::Hello { .. } => HELLO,
            Msg::HelloOk { .. } => HELLO_OK,
            Msg::Ping => PING,
            Msg::Pong => PONG,
            Msg::GetMap { .. } => GET_MAP,
            Msg::Map { .. } => MAP,
            Msg::Write { .. } => WRITE,
            Msg::WriteOk { .. } => WRITE_OK,
            Msg::Read { .. } => READ,
            Msg::ReadOk { .. } => READ_OK,
            Msg::Exec { .. } => EXEC,
            Msg::ExecOk => EXEC_OK,
            Msg::ReplExec { .. } => REPL_EXEC,
            Msg::Lookup { .. } => LOOKUP,
            Msg::LookupOk { .. } => LOOKUP_OK,
            Msg::PushObject { .. } => PUSH_OBJECT,
            Msg::Repl { .. } => REPL,
            Msg::ReplOk { .. } => REPL_OK,
            Msg::Push { .. } => PUSH,
            Msg::PushOk => PUSH_OK,
            Msg::Digests { .. } => DIGESTS,
            Msg::DigestsOk => DIGESTS_OK,
            Msg::Heartbeat { .. } => HEARTBEAT,
            Msg::HeartbeatOk { .. } => HEARTBEAT_OK,
            Msg::BackfillDone { .. } => BACKFILL_DONE,
            Msg::BackfillOk { .. } => BACKFILL_OK,
            Msg::Fence { .. } => FENCE,
            Msg::FenceOk => FENCE_OK,
            Msg::MapPush { .. } => MAP_PUSH,
            Msg::MapPushOk => MAP_PUSH_OK,
            Msg::PeerDown { .. } => PEER_DOWN,
            Msg::PeerDownOk => PEER_DOWN_OK,
            Msg::Error { .. } => ERROR,
        }
    }

    pub fn is_response(&self) -> bool {
        matches!(
            self,
            Msg::HelloOk { .. }
                | Msg::Pong
                | Msg::Map { .. }
                | Msg::WriteOk { .. }
                | Msg::ReadOk { .. }
                | Msg::ExecOk
                | Msg::LookupOk { .. }
                | Msg::ReplOk { .. }
                | Msg::PushOk
                | Msg::DigestsOk
                | Msg::HeartbeatOk { .. }
                | Msg::FenceOk
                | Msg::MapPushOk
                | Msg::PeerDownOk
                | Msg::BackfillOk { .. }
                | Msg::Error { .. }
        )
    }

    fn encode_body(&self, e: &mut Enc) {
        let crcs = |e: &mut Enc, c: &[u32]| {
            e.seq(c, |e, v| {
                e.u32(*v);
            });
        };
        match self {
            Msg::Hello {
                min_version,
                max_version,
                id,
                instance,
            } => {
                e.u16(*min_version)
                    .u16(*max_version)
                    .u64(*id)
                    .u64(*instance);
            }
            Msg::HelloOk {
                version,
                node,
                map_epoch,
            } => {
                e.u16(*version).u64(*node).u64(*map_epoch);
            }
            Msg::Ping | Msg::Pong | Msg::PushOk | Msg::DigestsOk | Msg::ExecOk => {}
            Msg::Exec { to, key } | Msg::PushObject { to, key } => {
                to.enc(e);
                key.enc(e);
            }
            Msg::ReplExec { to, seq, key } => {
                to.enc(e);
                e.u64(*seq);
                key.enc(e);
            }
            Msg::Lookup {
                to,
                key,
                kv_from,
                kv_to,
                kv_limit,
            } => {
                to.enc(e);
                key.enc(e);
                e.bytes(kv_from);
                match kv_to {
                    Some(t) => e.bool(true).bytes(t),
                    None => e.bool(false),
                };
                e.u32(*kv_limit);
            }
            Msg::LookupOk { found } => {
                e.bool(*found);
            }
            Msg::Digests { to, deep } => {
                to.enc(e);
                e.u8(u8::from(*deep));
            }
            Msg::Heartbeat { node } => {
                e.u32(*node);
            }
            Msg::HeartbeatOk {
                map_epoch,
                lease_ms,
            } => {
                e.u64(*map_epoch).u32(*lease_ms);
            }
            Msg::BackfillOk { map_epoch } => {
                e.u64(*map_epoch);
            }
            Msg::FenceOk | Msg::MapPushOk | Msg::PeerDownOk => {}
            Msg::PeerDown { reporter, peer } => {
                e.u32(*reporter).u32(*peer);
            }
            Msg::Fence { epoch, pgs } => {
                e.u64(*epoch).seq(pgs, |e, (p, g)| {
                    e.u32(*p).u32(*g);
                });
            }
            Msg::MapPush { epoch, map } => {
                e.u64(*epoch).bytes(map);
            }
            Msg::Push { to, key, crcs: c } => {
                to.enc(e);
                key.enc(e);
                crcs(e, c);
            }
            Msg::BackfillDone {
                pool,
                pg,
                pg_epoch,
                device,
            } => {
                e.u32(*pool).u32(*pg).u64(*pg_epoch).u32(*device);
            }
            Msg::GetMap { have_epoch } => {
                e.u64(*have_epoch);
            }
            Msg::Map { epoch, map } => {
                e.u64(*epoch).bytes(map);
            }
            Msg::Write {
                to,
                key,
                off,
                crcs: c,
            } => {
                to.enc(e);
                key.enc(e);
                e.u64(*off);
                crcs(e, c);
            }
            Msg::WriteOk { revision } | Msg::ReplOk { revision } => {
                e.u64(*revision);
            }
            Msg::Read { to, key, off, len } => {
                to.enc(e);
                key.enc(e);
                e.u64(*off).u64(*len);
            }
            Msg::ReadOk { crcs: c } => crcs(e, c),
            Msg::Repl {
                to,
                seq,
                key,
                off,
                crcs: c,
            } => {
                to.enc(e);
                e.u64(*seq);
                key.enc(e);
                e.u64(*off);
                crcs(e, c);
            }
            Msg::Error {
                code,
                map_epoch,
                detail,
            } => {
                e.u16(*code as u16).u64(*map_epoch).str(detail);
            }
        }
    }

    pub fn decode(kind: u16, body: &[u8]) -> Result<Self, FrameError> {
        use kind::*;
        let mut d = Dec::new(body);
        let crcs = |d: &mut Dec<'_>| d.seq(4, |d| d.u32());
        let m = match kind {
            HELLO => Msg::Hello {
                min_version: d.u16()?,
                max_version: d.u16()?,
                id: d.u64()?,
                instance: d.u64()?,
            },
            HELLO_OK => Msg::HelloOk {
                version: d.u16()?,
                node: d.u64()?,
                map_epoch: d.u64()?,
            },
            PING => Msg::Ping,
            PONG => Msg::Pong,
            GET_MAP => Msg::GetMap {
                have_epoch: d.u64()?,
            },
            MAP => Msg::Map {
                epoch: d.u64()?,
                map: d.bytes()?.to_vec(),
            },
            WRITE => Msg::Write {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                off: d.u64()?,
                crcs: crcs(&mut d)?,
            },
            WRITE_OK => Msg::WriteOk { revision: d.u64()? },
            READ => Msg::Read {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                off: d.u64()?,
                len: d.u64()?,
            },
            READ_OK => Msg::ReadOk {
                crcs: crcs(&mut d)?,
            },
            REPL => Msg::Repl {
                to: Target::dec(&mut d)?,
                seq: d.u64()?,
                key: WireKey::dec(&mut d)?,
                off: d.u64()?,
                crcs: crcs(&mut d)?,
            },
            REPL_OK => Msg::ReplOk { revision: d.u64()? },
            PUSH => Msg::Push {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                crcs: crcs(&mut d)?,
            },
            PUSH_OK => Msg::PushOk,
            EXEC => Msg::Exec {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
            },
            EXEC_OK => Msg::ExecOk,
            REPL_EXEC => Msg::ReplExec {
                to: Target::dec(&mut d)?,
                seq: d.u64()?,
                key: WireKey::dec(&mut d)?,
            },
            LOOKUP => Msg::Lookup {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                kv_from: d.bytes()?.to_vec(),
                kv_to: if d.bool()? {
                    Some(d.bytes()?.to_vec())
                } else {
                    None
                },
                kv_limit: d.u32()?,
            },
            LOOKUP_OK => Msg::LookupOk { found: d.bool()? },
            PUSH_OBJECT => Msg::PushObject {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
            },
            DIGESTS => Msg::Digests {
                to: Target::dec(&mut d)?,
                deep: d.u8()? != 0,
            },
            DIGESTS_OK => Msg::DigestsOk,
            HEARTBEAT => Msg::Heartbeat { node: d.u32()? },
            HEARTBEAT_OK => Msg::HeartbeatOk {
                map_epoch: d.u64()?,
                lease_ms: d.u32()?,
            },
            FENCE => Msg::Fence {
                epoch: d.u64()?,
                pgs: d.seq(8, |d| Ok((d.u32()?, d.u32()?)))?,
            },
            FENCE_OK => Msg::FenceOk,
            MAP_PUSH => Msg::MapPush {
                epoch: d.u64()?,
                map: d.bytes()?.to_vec(),
            },
            MAP_PUSH_OK => Msg::MapPushOk,
            PEER_DOWN => Msg::PeerDown {
                reporter: d.u32()?,
                peer: d.u32()?,
            },
            PEER_DOWN_OK => Msg::PeerDownOk,
            BACKFILL_DONE => Msg::BackfillDone {
                pool: d.u32()?,
                pg: d.u32()?,
                pg_epoch: d.u64()?,
                device: d.u32()?,
            },
            BACKFILL_OK => Msg::BackfillOk {
                map_epoch: d.u64()?,
            },
            ERROR => Msg::Error {
                code: Code::from_u16(d.u16()?),
                map_epoch: d.u64()?,
                detail: d.str()?.to_string(),
            },
            k => return Err(FrameError::Kind(k)),
        };
        d.finish()?;
        Ok(m)
    }
}

/// The digest reported for a copy whose blocks failed their checksums; a
/// healthy object's digest is never this value.
pub const DAMAGED: u64 = 0;

/// The payload of [`Msg::DigestsOk`]: each object's key and content
/// digest, in key order. In the payload rather than the body, so a PG's
/// list isn't bounded by [`MAX_BODY`].
pub fn encode_digests(entries: &[(WireKey, u64)]) -> Vec<u8> {
    let mut e = Enc::new();
    e.seq(entries, |e, (k, d)| {
        k.enc(e);
        e.u64(*d);
    });
    e.into_vec()
}

pub fn decode_digests(payload: &[u8]) -> Result<Vec<(WireKey, u64)>, DecodeError> {
    let mut d = Dec::new(payload);
    let v = d.seq(1 + 4 + 8 + 8, |d| Ok((WireKey::dec(d)?, d.u64()?)))?;
    d.finish()?;
    Ok(v)
}

/// One operation of an atomic list on an object ([`Msg::Exec`]): the
/// engine's operations (engine.md §7.2), with assertions on attributes in
/// place of revisions, which differ between an object's copies (I6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireOp {
    AssertExists,
    AssertAbsent,
    /// The attribute has this value (`None`: it is absent).
    AssertAttr {
        name: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    /// Creates the object if it is absent.
    Touch,
    /// `data` is whole blocks.
    Write {
        off: u64,
        data: Vec<u8>,
    },
    Zero {
        off: u64,
        len: u64,
    },
    Truncate(u64),
    SetAttr(Vec<u8>, Vec<u8>),
    RmAttr(Vec<u8>),
    KvPut(Vec<u8>, Vec<u8>),
    KvDel(Vec<u8>),
    /// Deletes keys in `[start, end)`.
    KvDelRange(Vec<u8>, Vec<u8>),
    Delete,
}

impl WireOp {
    pub fn is_assertion(&self) -> bool {
        matches!(
            self,
            WireOp::AssertExists | WireOp::AssertAbsent | WireOp::AssertAttr { .. }
        )
    }
}

pub fn encode_ops(ops: &[WireOp]) -> Vec<u8> {
    let mut e = Enc::new();
    e.u32(ops.len() as u32);
    for op in ops {
        match op {
            WireOp::AssertExists => {
                e.u8(1);
            }
            WireOp::AssertAbsent => {
                e.u8(2);
            }
            WireOp::AssertAttr { name, value } => {
                e.u8(3).bytes(name);
                match value {
                    Some(v) => e.bool(true).bytes(v),
                    None => e.bool(false),
                };
            }
            WireOp::Touch => {
                e.u8(4);
            }
            WireOp::Write { off, data } => {
                e.u8(5).u64(*off).bytes(data);
            }
            WireOp::Zero { off, len } => {
                e.u8(6).u64(*off).u64(*len);
            }
            WireOp::Truncate(n) => {
                e.u8(7).u64(*n);
            }
            WireOp::SetAttr(k, v) => {
                e.u8(8).bytes(k).bytes(v);
            }
            WireOp::RmAttr(k) => {
                e.u8(9).bytes(k);
            }
            WireOp::KvPut(k, v) => {
                e.u8(10).bytes(k).bytes(v);
            }
            WireOp::KvDel(k) => {
                e.u8(11).bytes(k);
            }
            WireOp::KvDelRange(a, b) => {
                e.u8(12).bytes(a).bytes(b);
            }
            WireOp::Delete => {
                e.u8(13);
            }
        }
    }
    e.into_vec()
}

pub fn decode_ops(b: &[u8]) -> Result<Vec<WireOp>, DecodeError> {
    let mut d = Dec::new(b);
    let n = d.u32()? as usize;
    if n > b.len() {
        return Err(DecodeError::TooLong(n));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let bytes = |d: &mut Dec<'_>| -> Result<Vec<u8>, DecodeError> { Ok(d.bytes()?.to_vec()) };
        out.push(match d.u8()? {
            1 => WireOp::AssertExists,
            2 => WireOp::AssertAbsent,
            3 => WireOp::AssertAttr {
                name: bytes(&mut d)?,
                value: if d.bool()? {
                    Some(bytes(&mut d)?)
                } else {
                    None
                },
            },
            4 => WireOp::Touch,
            5 => WireOp::Write {
                off: d.u64()?,
                data: bytes(&mut d)?,
            },
            6 => WireOp::Zero {
                off: d.u64()?,
                len: d.u64()?,
            },
            7 => WireOp::Truncate(d.u64()?),
            8 => WireOp::SetAttr(bytes(&mut d)?, bytes(&mut d)?),
            9 => WireOp::RmAttr(bytes(&mut d)?),
            10 => WireOp::KvPut(bytes(&mut d)?, bytes(&mut d)?),
            11 => WireOp::KvDel(bytes(&mut d)?),
            12 => WireOp::KvDelRange(bytes(&mut d)?, bytes(&mut d)?),
            13 => WireOp::Delete,
            k => {
                return Err(DecodeError::Invalid {
                    field: "operation",
                    value: k as u64,
                });
            }
        });
    }
    d.finish()?;
    Ok(out)
}

/// What a [`Msg::Lookup`] returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Looked {
    pub size: u64,
    pub attrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub kv: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Keys and values, in order.
pub type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

fn enc_pairs(e: &mut Enc, pairs: &[(Vec<u8>, Vec<u8>)]) {
    e.seq(pairs, |e, (k, v)| {
        e.bytes(k).bytes(v);
    });
}

fn dec_pairs(d: &mut Dec<'_>) -> Result<Pairs, DecodeError> {
    d.seq(8, |d| Ok((d.bytes()?.to_vec(), d.bytes()?.to_vec())))
}

pub fn encode_looked(l: &Looked) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(l.size);
    enc_pairs(&mut e, &l.attrs);
    enc_pairs(&mut e, &l.kv);
    e.into_vec()
}

pub fn decode_looked(b: &[u8]) -> Result<Looked, DecodeError> {
    let mut d = Dec::new(b);
    let l = Looked {
        size: d.u64()?,
        attrs: dec_pairs(&mut d)?,
        kv: dec_pairs(&mut d)?,
    };
    d.finish()?;
    Ok(l)
}

/// An object whole ([`Msg::PushObject`]): its exact size, its data (whole
/// blocks, with each block's CRC), attributes and key–value table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireObject {
    pub size: u64,
    pub data: Vec<u8>,
    pub attrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub kv: Vec<(Vec<u8>, Vec<u8>)>,
}

pub fn encode_object(o: &WireObject) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(o.size).seq(&block_crcs(&o.data), |e, c| {
        e.u32(*c);
    });
    e.bytes(&o.data);
    enc_pairs(&mut e, &o.attrs);
    enc_pairs(&mut e, &o.kv);
    e.into_vec()
}

/// Decodes and checks the data against its CRCs.
pub fn decode_object(b: &[u8]) -> Result<WireObject, DecodeError> {
    let mut d = Dec::new(b);
    let size = d.u64()?;
    let crcs = d.seq(4, |d| d.u32())?;
    let data = d.bytes()?.to_vec();
    if verify_crcs(&data, &crcs).is_err() {
        return Err(DecodeError::Invalid {
            field: "object data checksum",
            value: 0,
        });
    }
    let o = WireObject {
        size,
        data,
        attrs: dec_pairs(&mut d)?,
        kv: dec_pairs(&mut d)?,
    };
    d.finish()?;
    Ok(o)
}

/// Encodes a frame: header, body, then `payload`.
pub fn encode_frame(req: u64, msg: &Msg, payload: &[u8]) -> Vec<u8> {
    let mut body = Enc::with_capacity(64);
    msg.encode_body(&mut body);
    let h = Header {
        kind: msg.kind(),
        flags: if msg.is_response() { FLAG_RESPONSE } else { 0 },
        req,
        body_len: body.len() as u32,
        payload_len: payload.len() as u32,
    };
    let mut out = Vec::with_capacity(HEADER_LEN + body.len() + payload.len());
    out.extend_from_slice(&h.encode());
    out.extend_from_slice(body.as_slice());
    out.extend_from_slice(payload);
    out
}

/// CRC32C of each 4 KiB block of `data` (the last block may be short).
pub fn block_crcs(data: &[u8]) -> Vec<u32> {
    data.chunks(4096).map(crc32c).collect()
}

/// Checks `data` against per-block CRCs.
pub fn verify_crcs(data: &[u8], crcs: &[u32]) -> Result<(), FrameError> {
    if data.len().div_ceil(4096) != crcs.len() {
        return Err(FrameError::Checksum(crcs.len()));
    }
    for (i, (b, c)) in data.chunks(4096).zip(crcs).enumerate() {
        if crc32c(b) != *c {
            return Err(FrameError::Checksum(i));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Msg> {
        let to = Target {
            pool: 1,
            pg: 2,
            map_epoch: 3,
            pg_epoch: 4,
        };
        let key = WireKey {
            ns: 1,
            name: b"vol".to_vec(),
            version: 9,
        };
        vec![
            Msg::Hello {
                min_version: 1,
                max_version: 1,
                id: 5,
                instance: 6,
            },
            Msg::HelloOk {
                version: 1,
                node: 2,
                map_epoch: 3,
            },
            Msg::Ping,
            Msg::Pong,
            Msg::GetMap { have_epoch: 7 },
            Msg::Map {
                epoch: 8,
                map: vec![1, 2, 3],
            },
            Msg::Write {
                to,
                key: key.clone(),
                off: 4096,
                crcs: vec![1, 2],
            },
            Msg::WriteOk { revision: 3 },
            Msg::Read {
                to,
                key: key.clone(),
                off: 0,
                len: 8192,
            },
            Msg::ReadOk { crcs: vec![4] },
            Msg::Repl {
                to,
                seq: 11,
                key,
                off: 0,
                crcs: vec![],
            },
            Msg::ReplOk { revision: 12 },
            Msg::Push {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"p".to_vec(),
                    version: 0,
                },
                crcs: vec![5],
            },
            Msg::PushOk,
            Msg::Exec {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
            },
            Msg::ExecOk,
            Msg::ReplExec {
                to,
                seq: 4,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
            },
            Msg::Lookup {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                kv_from: b"a".to_vec(),
                kv_to: Some(b"z".to_vec()),
                kv_limit: 10,
            },
            Msg::LookupOk { found: true },
            Msg::PushObject {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
            },
            Msg::Digests { to, deep: true },
            Msg::DigestsOk,
            Msg::Heartbeat { node: 3 },
            Msg::HeartbeatOk {
                map_epoch: 4,
                lease_ms: 2000,
            },
            Msg::Fence {
                epoch: 9,
                pgs: vec![(1, 2), (1, 7)],
            },
            Msg::FenceOk,
            Msg::MapPush {
                epoch: 9,
                map: vec![1, 2],
            },
            Msg::MapPushOk,
            Msg::PeerDown {
                reporter: 1,
                peer: 2,
            },
            Msg::PeerDownOk,
            Msg::BackfillDone {
                pool: 1,
                pg: 2,
                pg_epoch: 3,
                device: 4,
            },
            Msg::BackfillOk { map_epoch: 5 },
            Msg::Error {
                code: Code::NotPrimary,
                map_epoch: 5,
                detail: "x".into(),
            },
        ]
    }

    #[test]
    fn frames_round_trip() {
        for m in all() {
            let payload = vec![7u8; 100];
            let f = encode_frame(42, &m, &payload);
            let h = Header::decode(f[..HEADER_LEN].try_into().unwrap()).unwrap();
            assert_eq!(h.req, 42);
            assert_eq!(h.flags & FLAG_RESPONSE != 0, m.is_response());
            let body = &f[HEADER_LEN..HEADER_LEN + h.body_len as usize];
            assert_eq!(Msg::decode(h.kind, body).unwrap(), m);
            assert_eq!(&f[HEADER_LEN + h.body_len as usize..], &payload[..]);
        }
    }

    #[test]
    fn operation_lists_objects_and_lookups_round_trip() {
        let ops = vec![
            WireOp::AssertExists,
            WireOp::AssertAbsent,
            WireOp::AssertAttr {
                name: b"v".to_vec(),
                value: Some(b"7".to_vec()),
            },
            WireOp::AssertAttr {
                name: b"w".to_vec(),
                value: None,
            },
            WireOp::Touch,
            WireOp::Write {
                off: 4096,
                data: vec![1; 4096],
            },
            WireOp::Zero { off: 0, len: 4096 },
            WireOp::Truncate(9),
            WireOp::SetAttr(b"a".to_vec(), b"b".to_vec()),
            WireOp::RmAttr(b"a".to_vec()),
            WireOp::KvPut(b"k".to_vec(), vec![0, 255]),
            WireOp::KvDel(b"k".to_vec()),
            WireOp::KvDelRange(b"a".to_vec(), b"z".to_vec()),
            WireOp::Delete,
        ];
        assert_eq!(decode_ops(&encode_ops(&ops)).unwrap(), ops);
        assert!(decode_ops(&[1, 0, 0, 0, 99]).is_err());
        let o = WireObject {
            size: 5000,
            data: vec![3; 8192],
            attrs: vec![(b"v".to_vec(), b"1".to_vec())],
            kv: vec![(b"x".to_vec(), b"y".to_vec())],
        };
        let mut enc = encode_object(&o);
        assert_eq!(decode_object(&enc).unwrap(), o);
        // A flipped data byte is refused.
        let at = enc.len() - 40;
        enc[at] ^= 1;
        assert!(decode_object(&enc).is_err());
        let l = Looked {
            size: 3,
            attrs: vec![(b"a".to_vec(), b"b".to_vec())],
            kv: vec![],
        };
        assert_eq!(decode_looked(&encode_looked(&l)).unwrap(), l);
    }

    #[test]
    fn corrupt_headers_and_bodies_are_refused() {
        let f = encode_frame(1, &Msg::Ping, &[]);
        for i in 0..HEADER_LEN {
            let mut g = f.clone();
            g[i] ^= 0x40;
            assert!(
                Header::decode(g[..HEADER_LEN].try_into().unwrap()).is_err(),
                "byte {i}"
            );
        }
        let w = encode_frame(1, &all()[6], &[]);
        let h = Header::decode(w[..HEADER_LEN].try_into().unwrap()).unwrap();
        let body = &w[HEADER_LEN..HEADER_LEN + h.body_len as usize];
        for cut in 0..body.len() {
            assert!(Msg::decode(h.kind, &body[..cut]).is_err(), "cut {cut}");
        }
        let mut long = body.to_vec();
        long.push(0);
        assert!(Msg::decode(h.kind, &long).is_err(), "trailing bytes");
        assert_eq!(Msg::decode(500, body), Err(FrameError::Kind(500)));
    }

    #[test]
    fn crcs() {
        let d = vec![3u8; 10000];
        let c = block_crcs(&d);
        assert_eq!(c.len(), 3);
        verify_crcs(&d, &c).unwrap();
        let mut bad = d.clone();
        bad[5000] ^= 1;
        assert_eq!(verify_crcs(&bad, &c), Err(FrameError::Checksum(1)));
    }
}
