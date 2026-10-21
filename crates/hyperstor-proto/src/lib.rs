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
    /// An administrator marks a device in or out of placement
    /// (administration.md §23.6): out moves its PGs elsewhere, in brings
    /// them back. Answered with the working map's epoch.
    SetDeviceIn {
        device: u32,
        up_in: bool,
    },
    SetDeviceInOk {
        map_epoch: u64,
    },
    /// Primary to backfill target: replace the object with the payload
    /// (empty: delete it) (replication.md §8.5).
    Push {
        to: Target,
        key: WireKey,
        crcs: Vec<u32>,
    },
    PushOk,
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
    pub const REPL: u16 = 20;
    pub const REPL_OK: u16 = 21;
    pub const PUSH: u16 = 22;
    pub const PUSH_OK: u16 = 23;
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
    pub const SET_DEVICE_IN: u16 = 40;
    pub const SET_DEVICE_IN_OK: u16 = 41;
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
            Msg::Repl { .. } => REPL,
            Msg::ReplOk { .. } => REPL_OK,
            Msg::Push { .. } => PUSH,
            Msg::PushOk => PUSH_OK,
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
            Msg::SetDeviceIn { .. } => SET_DEVICE_IN,
            Msg::SetDeviceInOk { .. } => SET_DEVICE_IN_OK,
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
                | Msg::ReplOk { .. }
                | Msg::PushOk
                | Msg::HeartbeatOk { .. }
                | Msg::FenceOk
                | Msg::MapPushOk
                | Msg::PeerDownOk
                | Msg::SetDeviceInOk { .. }
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
            Msg::Ping | Msg::Pong | Msg::PushOk => {}
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
            Msg::SetDeviceIn { device, up_in } => {
                e.u32(*device).u8(u8::from(*up_in));
            }
            Msg::SetDeviceInOk { map_epoch } => {
                e.u64(*map_epoch);
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
            SET_DEVICE_IN => Msg::SetDeviceIn {
                device: d.u32()?,
                up_in: d.u8()? != 0,
            },
            SET_DEVICE_IN_OK => Msg::SetDeviceInOk {
                map_epoch: d.u64()?,
            },
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
            Msg::SetDeviceIn {
                device: 7,
                up_in: false,
            },
            Msg::SetDeviceInOk { map_epoch: 12 },
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
