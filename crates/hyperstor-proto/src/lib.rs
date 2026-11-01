//! The framed wire protocol (spec/13-protocol.md §13.1, D5).
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
/// 2: [`Target::lease`], delegates and per-volume fencing (D150, D152).
pub const VERSION: u16 = 2;
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
    /// The connection's client instance is blocklisted: every lease it
    /// held is gone.
    Blocklisted = 10,
    /// The map service member isn't the leader; `detail` is the leader's
    /// map-service address when known, else empty (04-decisions.md D72).
    NotLeader = 11,
    /// Not now: retrying at once won't help (a snapshot whose volume's
    /// holder didn't quiesce in time).
    Busy = 12,
    /// The request's lease identity ([`Target::lease`]) is blocklisted:
    /// that volume's lease is gone, the client's others aren't (D150).
    Fenced = 13,
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
            12 => Code::Busy,
            13 => Code::Fenced,
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

/// Where a request is addressed, and under which lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Target {
    pub pool: u32,
    pub pg: u32,
    pub map_epoch: u64,
    pub pg_epoch: u64,
    /// The lease identity the request acts under (D150): a volume's
    /// holder's, or one of its delegates'; 0 for none (peers, the map
    /// service, tools). Storage nodes refuse a request whose identity is
    /// blocklisted, as they refuse one from a blocklisted instance.
    pub lease: u64,
}

impl Target {
    fn enc(&self, e: &mut Enc) {
        e.u32(self.pool)
            .u32(self.pg)
            .u64(self.map_epoch)
            .u64(self.pg_epoch)
            .u64(self.lease);
    }
    fn dec(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            pool: d.u32()?,
            pg: d.u32()?,
            map_epoch: d.u64()?,
            pg_epoch: d.u64()?,
            lease: d.u64()?,
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
        /// Random per process (06-metadata.md §4.1, blocklist).
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
    /// (06-metadata.md §4.2: compare-and-set on `.meta` objects).
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
    /// Primary to a member: add the payload's key–value entries
    /// ([`encode_pairs`]) to the object the last [`Msg::PushObject`] on
    /// this connection replaced. An object whose table doesn't fit one
    /// frame ([`MAX_PAYLOAD`]) is pushed as a `PushObject` with the first
    /// part, then `PushKv` for the rest, in order (09-replication.md §8.3).
    PushKv {
        to: Target,
        key: WireKey,
    },
    /// Primary to replica: apply this write (09-replication.md §8.1).
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
    /// Storage node to map service: still alive (06-metadata.md §4.1).
    Heartbeat {
        node: u32,
    },
    /// Renews the sender's map lease (09-replication.md §8.4, 04-decisions.md
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
    /// pings for the peer timeout (04-decisions.md D40).
    PeerDown {
        reporter: u32,
        peer: u32,
    },
    PeerDownOk,
    /// Primary to backfill target: replace the object with the payload
    /// (empty: delete it) (09-replication.md §8.5).
    Push {
        to: Target,
        key: WireKey,
        crcs: Vec<u32>,
    },
    PushOk,
    /// Primary to a member or backfill target: list what you hold of the
    /// PG, as digests of each object's content (09-replication.md §8.5).
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
    /// Client to the primary of a volume's header: take the volume's lease
    /// for the identity `to.lease` (D150) of the instance named at `Hello`
    /// (06-metadata.md §9, D75), in mode [`LEASE_RW`] or [`LEASE_RO`]. With
    /// `takeover`, an expired lease whose holder and delegates are all
    /// blocklisted is broken (a forced takeover). `label` says who holds
    /// it, for whoever finds it held (D153; up to [`LABEL_MAX`] bytes).
    Lease {
        to: Target,
        key: WireKey,
        mode: u8,
        takeover: bool,
        label: String,
    },
    /// The lease is held by the sender (or released): the header's version.
    LeaseOk {
        version: u64,
    },
    /// Another identity holds the lease for `remaining_ms` more; 0: it has
    /// expired, and may be broken once its holder and its delegates (D33)
    /// are blocklisted.
    LeaseHeld {
        holder: u64,
        delegates: Vec<u64>,
        remaining_ms: u32,
        /// The holder's label, as it took the lease.
        label: String,
    },
    /// The holder (`to.lease`) to its header's primary: the delegates
    /// that act under its lease are now `delegates` (D33, D151).
    LeaseDelegates {
        to: Target,
        key: WireKey,
        delegates: Vec<u64>,
    },
    /// Client to the primary of a volume's header: give the lease
    /// `to.lease` up.
    LeaseRelease {
        to: Target,
        key: WireKey,
    },
    /// Client to a `.meta` primary: renew every lease the sender holds on
    /// headers this device is primary of (D75): each is a PG, its epoch,
    /// the header's name, in `pool`, and the lease's identity.
    LeaseRenew {
        pool: u32,
        map_epoch: u64,
        leases: Vec<(u32, u64, Vec<u8>, u64)>,
    },
    /// For each lease of a [`Msg::LeaseRenew`], in order: [`LEASE_RENEWED`],
    /// [`LEASE_LOST`] or [`LEASE_RETRY`], and the header's version.
    LeaseRenewOk {
        results: Vec<(u8, u64)>,
    },
    /// Client to map service: blocklist `ids`, client instances or lease
    /// identities (06-metadata.md §9, a forced takeover; D150). Answered once
    /// every storage node holds the map that has them, or has lost its
    /// map lease.
    Blocklist {
        ids: Vec<u64>,
    },
    BlocklistOk {
        map_epoch: u64,
    },
    /// Storage node to map service: `device` failed (its engine can no
    /// longer write it); take it out of placement (09-replication.md §15).
    DeviceFailed {
        device: u32,
    },
    /// Primary `reporter` to map service: `device` hasn't answered a
    /// replicated write of the PG within the operation timeout (D91).
    MemberSlow {
        reporter: u32,
        pool: u32,
        pg: u32,
        pg_epoch: u64,
        device: u32,
    },
    /// Primary to map service: after a scrub of the PG, `unresolved`
    /// objects whose copies differ and couldn't be repaired.
    ScrubReport {
        pool: u32,
        pg: u32,
        unresolved: u32,
    },
    /// Primary `device` to map service: the bytes its copy of each PG
    /// holds (D155), for the PGs whose bytes changed since its last
    /// report, or for all it is primary of if `full`.
    PgUsage {
        device: u32,
        full: bool,
        pgs: Vec<(u32, u32, u64)>,
    },
    /// Answer to `PgUsage`: how many PGs the map service now holds from
    /// `device`. Another number than the sender's asks for a full report.
    PgUsageOk {
        known: u32,
    },
    /// Storage node to map service: `device`'s data region, and how much
    /// of it is free, every few seconds (04-decisions.md D156). Not kept
    /// in the map.
    DeviceSpace {
        device: u32,
        total_bytes: u64,
        free_bytes: u64,
    },
    /// Storage node to map service: what `device` is, as
    /// `hyperstor_map::DeviceKind::code` (24-pools-and-devices.md §27.2.4,
    /// D178), with its space reports. Answered `Noted`.
    DeviceKind {
        device: u32,
        kind: u8,
    },
    /// Storage node to map service: `device` has split PG `pool`.`pg`,
    /// marked to split at `pg_epoch`: its objects that belong to the PG's
    /// new sibling are under the sibling's keys (24-pools-and-devices.md
    /// §27.5, D187). Answered `Noted`.
    PgSplit {
        pool: u32,
        pg: u32,
        pg_epoch: u64,
        device: u32,
    },
    /// Primary to a member of a replicated PG: the read its own device
    /// didn't answer in time, from the member's copy (04-decisions.md
    /// D190). `layers` as in [`Msg::ReadAt`], answered as it is; without,
    /// as [`Msg::Read`] is.
    ReadCopy {
        to: Target,
        key: WireKey,
        layers: Option<Vec<(u32, u64, u64)>>,
        off: u64,
        len: u64,
    },
    /// Follower to the map service's leader: what only the leader knows,
    /// for a follower's `GetStatus` (I197).
    LeaderState,
    /// Answer to `LeaderState`: how long ago each node was last heard
    /// from (ms), PGs scrub couldn't repair (pool, pg, objects), nodes
    /// flapping (node, transitions) and each device's space (device,
    /// total, free bytes).
    LeaderStateOk {
        heard: Vec<(u32, u64)>,
        inconsistent: Vec<(u32, u32, u32)>,
        flapping: Vec<(u32, u32)>,
        space: Vec<(u32, u64, u64)>,
    },
    /// Answer to `DeviceFailed`, `MemberSlow`, `ScrubReport` and
    /// `DeviceSpace`: the map epoch that has the change (or the current
    /// one).
    Noted {
        map_epoch: u64,
    },
    /// Primary to a member: send me your copy of the object (to repair
    /// the primary's own, 09-replication.md §8.2, §8.5), once every write
    /// queued before this request is applied.
    Fetch {
        to: Target,
        key: WireKey,
    },
    /// The copy, in the payload ([`encode_object`]), if `found`.
    FetchOk {
        found: bool,
    },
    /// [`Msg::Fetch`] in parts, for an object whose key–value table
    /// doesn't fit one frame (01-implementation-notes.md I182): with `first`,
    /// the object with the first part of its table; otherwise the table's
    /// next part from `kv_from` on.
    FetchPart {
        to: Target,
        key: WireKey,
        first: bool,
        kv_from: Vec<u8>,
    },
    /// A part: with `first`, an object ([`encode_object`]) if `found`;
    /// otherwise entries ([`encode_pairs`]). `next`: where the next part
    /// starts, if there is one.
    FetchPartOk {
        found: bool,
        next: Option<Vec<u8>>,
    },
    /// Client to primary: read a volume chunk at an epoch (08-engine.md
    /// §7.2). `key.name` is the chunk without a branch; each layer is a
    /// branch and the range of epochs read from it (both ends included),
    /// the volume's own first, then its clone parents'. Each block comes
    /// from the newest version that holds it in the first layer that has
    /// one.
    ReadAt {
        to: Target,
        key: WireKey,
        layers: Vec<(u32, u64, u64)>,
        off: u64,
        len: u64,
    },
    /// Client to primary: where each block a [`Msg::ReadAt`] would read
    /// comes from, without the data. Answered with a [`Msg::ReadAtOk`]
    /// with no CRCs or payload. A client asks it when its read cache may
    /// hold the blocks (04-decisions.md D102).
    Sources {
        to: Target,
        key: WireKey,
        layers: Vec<(u32, u64, u64)>,
        off: u64,
        len: u64,
    },
    /// The answer to [`Msg::ReadAt`]: the data in the payload, with each
    /// block's CRC and the version it came from, as (branch, epoch);
    /// `(u32::MAX, 0)` for a block no version holds. A client caches the
    /// blocks that came from versions that can't change (04-decisions.md
    /// D102).
    ReadAtOk {
        crcs: Vec<u32>,
        sources: Vec<(u32, u64)>,
    },
    /// Client to primary: the versions of the object `key` names.
    Versions {
        to: Target,
        key: WireKey,
    },
    VersionsOk {
        versions: Vec<u64>,
    },
    /// Client to primary: delete the versions of `key`'s name with epochs
    /// from `from` to `upto`, both included (a volume's branch, what a
    /// rollback left behind).
    DropVersions {
        to: Target,
        key: WireKey,
        from: u64,
        upto: u64,
    },
    /// Client to primary: copy into `key` (a clone's chunk at its head
    /// epoch) every block no version of its own branch holds, read
    /// through `layers` (its parents), so the clone stops depending on
    /// them (10-volumes.md §10.4).
    Flatten {
        to: Target,
        key: WireKey,
        layers: Vec<(u32, u64, u64)>,
    },
    /// Lease holder to a header's primary: answer when there is news for
    /// this volume past header version `version` (04-decisions.md D103).
    Watch {
        to: Target,
        key: WireKey,
        version: u64,
    },
    /// News: [`WATCH_CHANGED`] (read the header again) or [`WATCH_QUIESCE`]
    /// (stop writing, drain, and answer [`Msg::Quiesced`] with `token`).
    WatchOk {
        event: u8,
        token: u64,
    },
    /// Holder to primary: drained for quiesce `token`. Answered with
    /// [`Msg::ExecOk`] once the snapshot is taken or abandoned: then
    /// read the header again and resume.
    Quiesced {
        to: Target,
        key: WireKey,
        token: u64,
    },
    /// Client to a header's primary: take a snapshot, an operation list in
    /// the payload ([`encode_ops`]) applied with the volume's holder, if
    /// any, quiesced (10-volumes.md §10.3); abandoned after 2 s without. With
    /// a `token`, under the hold of [`Msg::Quiesce`] with that token (a
    /// consistency group), which it ends.
    Snapshot {
        to: Target,
        key: WireKey,
        token: u64,
    },
    /// Client to a header's primary: quiesce the volume's holder, if any,
    /// and hold it (a consistency group's first step, 10-volumes.md §10.3);
    /// answered once it has drained. The hold ends with a [`Msg::Snapshot`]
    /// or [`Msg::Release`] with the same token, or after 5 s.
    Quiesce {
        to: Target,
        key: WireKey,
        token: u64,
    },
    Release {
        to: Target,
        key: WireKey,
        token: u64,
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
    pub const LEASE: u16 = 40;
    pub const LEASE_OK: u16 = 41;
    pub const LEASE_HELD: u16 = 42;
    pub const LEASE_RELEASE: u16 = 43;
    pub const LEASE_RENEW: u16 = 44;
    pub const LEASE_RENEW_OK: u16 = 45;
    pub const BLOCKLIST: u16 = 46;
    pub const BLOCKLIST_OK: u16 = 47;
    pub const DEVICE_FAILED: u16 = 48;
    pub const MEMBER_SLOW: u16 = 49;
    pub const SCRUB_REPORT: u16 = 50;
    pub const NOTED: u16 = 51;
    pub const FETCH: u16 = 52;
    pub const FETCH_OK: u16 = 53;
    pub const READ_AT: u16 = 54;
    pub const VERSIONS: u16 = 55;
    pub const VERSIONS_OK: u16 = 56;
    pub const DROP_VERSIONS: u16 = 57;
    pub const FLATTEN: u16 = 58;
    pub const WATCH: u16 = 59;
    pub const WATCH_OK: u16 = 60;
    pub const QUIESCED: u16 = 61;
    pub const SNAPSHOT: u16 = 62;
    pub const QUIESCE: u16 = 63;
    pub const RELEASE: u16 = 64;
    pub const READ_AT_OK: u16 = 65;
    pub const SOURCES: u16 = 66;
    pub const PUSH_KV: u16 = 67;
    pub const FETCH_PART: u16 = 68;
    pub const FETCH_PART_OK: u16 = 69;
    pub const LEASE_DELEGATES: u16 = 70;
    pub const PG_USAGE: u16 = 71;
    pub const PG_USAGE_OK: u16 = 72;
    pub const DEVICE_SPACE: u16 = 73;
    pub const LEADER_STATE: u16 = 74;
    pub const LEADER_STATE_OK: u16 = 75;
    pub const DEVICE_KIND: u16 = 76;
    pub const PG_SPLIT: u16 = 77;
    pub const READ_COPY: u16 = 78;
    pub const ERROR: u16 = 99;
}

impl Msg {
    /// Where the request is addressed, if it is addressed to a PG.
    pub fn target(&self) -> Option<&Target> {
        match self {
            Msg::Digests { to, .. }
            | Msg::DropVersions { to, .. }
            | Msg::Exec { to, .. }
            | Msg::Fetch { to, .. }
            | Msg::FetchPart { to, .. }
            | Msg::Flatten { to, .. }
            | Msg::Lease { to, .. }
            | Msg::LeaseDelegates { to, .. }
            | Msg::LeaseRelease { to, .. }
            | Msg::Lookup { to, .. }
            | Msg::Push { to, .. }
            | Msg::PushKv { to, .. }
            | Msg::PushObject { to, .. }
            | Msg::Quiesce { to, .. }
            | Msg::Quiesced { to, .. }
            | Msg::Read { to, .. }
            | Msg::ReadAt { to, .. }
            | Msg::ReadCopy { to, .. }
            | Msg::Release { to, .. }
            | Msg::Repl { to, .. }
            | Msg::ReplExec { to, .. }
            | Msg::Snapshot { to, .. }
            | Msg::Sources { to, .. }
            | Msg::Versions { to, .. }
            | Msg::Watch { to, .. }
            | Msg::Write { to, .. } => Some(to),
            _ => None,
        }
    }

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
            Msg::PushKv { .. } => PUSH_KV,
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
            Msg::Lease { .. } => LEASE,
            Msg::LeaseOk { .. } => LEASE_OK,
            Msg::LeaseHeld { .. } => LEASE_HELD,
            Msg::LeaseRelease { .. } => LEASE_RELEASE,
            Msg::LeaseRenew { .. } => LEASE_RENEW,
            Msg::LeaseRenewOk { .. } => LEASE_RENEW_OK,
            Msg::Blocklist { .. } => BLOCKLIST,
            Msg::BlocklistOk { .. } => BLOCKLIST_OK,
            Msg::DeviceFailed { .. } => DEVICE_FAILED,
            Msg::DeviceSpace { .. } => DEVICE_SPACE,
            Msg::DeviceKind { .. } => DEVICE_KIND,
            Msg::PgSplit { .. } => PG_SPLIT,
            Msg::ReadCopy { .. } => READ_COPY,
            Msg::LeaderState => LEADER_STATE,
            Msg::LeaderStateOk { .. } => LEADER_STATE_OK,
            Msg::MemberSlow { .. } => MEMBER_SLOW,
            Msg::ScrubReport { .. } => SCRUB_REPORT,
            Msg::Noted { .. } => NOTED,
            Msg::PgUsage { .. } => PG_USAGE,
            Msg::PgUsageOk { .. } => PG_USAGE_OK,
            Msg::Fetch { .. } => FETCH,
            Msg::FetchOk { .. } => FETCH_OK,
            Msg::FetchPart { .. } => FETCH_PART,
            Msg::FetchPartOk { .. } => FETCH_PART_OK,
            Msg::LeaseDelegates { .. } => LEASE_DELEGATES,
            Msg::ReadAt { .. } => READ_AT,
            Msg::Versions { .. } => VERSIONS,
            Msg::VersionsOk { .. } => VERSIONS_OK,
            Msg::DropVersions { .. } => DROP_VERSIONS,
            Msg::Flatten { .. } => FLATTEN,
            Msg::Watch { .. } => WATCH,
            Msg::WatchOk { .. } => WATCH_OK,
            Msg::Quiesced { .. } => QUIESCED,
            Msg::Snapshot { .. } => SNAPSHOT,
            Msg::Quiesce { .. } => QUIESCE,
            Msg::Release { .. } => RELEASE,
            Msg::ReadAtOk { .. } => READ_AT_OK,
            Msg::Sources { .. } => SOURCES,
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
                | Msg::LeaseOk { .. }
                | Msg::LeaseHeld { .. }
                | Msg::LeaseRenewOk { .. }
                | Msg::BlocklistOk { .. }
                | Msg::Noted { .. }
                | Msg::PgUsageOk { .. }
                | Msg::LeaderStateOk { .. }
                | Msg::FetchOk { .. }
                | Msg::FetchPartOk { .. }
                | Msg::VersionsOk { .. }
                | Msg::ReadAtOk { .. }
                | Msg::WatchOk { .. }
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
            Msg::Exec { to, key } | Msg::PushObject { to, key } | Msg::PushKv { to, key } => {
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
            Msg::Lease {
                to,
                key,
                mode,
                takeover,
                label,
            } => {
                to.enc(e);
                key.enc(e);
                e.u8(*mode).bool(*takeover).str(label);
            }
            Msg::LeaseRelease { to, key } => {
                to.enc(e);
                key.enc(e);
            }
            Msg::LeaseDelegates { to, key, delegates } => {
                to.enc(e);
                key.enc(e);
                e.seq(delegates, |e, d| {
                    e.u64(*d);
                });
            }
            Msg::LeaseOk { version } => {
                e.u64(*version);
            }
            Msg::LeaseHeld {
                holder,
                delegates,
                remaining_ms,
                label,
            } => {
                e.u64(*holder)
                    .seq(delegates, |e, d| {
                        e.u64(*d);
                    })
                    .u32(*remaining_ms)
                    .str(label);
            }
            Msg::LeaseRenew {
                pool,
                map_epoch,
                leases,
            } => {
                e.u32(*pool)
                    .u64(*map_epoch)
                    .seq(leases, |e, (pg, pe, name, lease)| {
                        e.u32(*pg).u64(*pe).bytes(name).u64(*lease);
                    });
            }
            Msg::LeaseRenewOk { results } => {
                e.seq(results, |e, (r, v)| {
                    e.u8(*r).u64(*v);
                });
            }
            Msg::Blocklist { ids } => {
                e.seq(ids, |e, i| {
                    e.u64(*i);
                });
            }
            Msg::BlocklistOk { map_epoch } | Msg::Noted { map_epoch } => {
                e.u64(*map_epoch);
            }
            Msg::DeviceFailed { device } => {
                e.u32(*device);
            }
            Msg::DeviceSpace {
                device,
                total_bytes,
                free_bytes,
            } => {
                e.u32(*device).u64(*total_bytes).u64(*free_bytes);
            }
            Msg::DeviceKind { device, kind } => {
                e.u32(*device).u8(*kind);
            }
            Msg::PgSplit {
                pool,
                pg,
                pg_epoch,
                device,
            } => {
                e.u32(*pool).u32(*pg).u64(*pg_epoch).u32(*device);
            }
            Msg::ReadCopy {
                to,
                key,
                layers,
                off,
                len,
            } => {
                to.enc(e);
                key.enc(e);
                e.bool(layers.is_some());
                if let Some(l) = layers {
                    layers_enc(e, l);
                }
                e.u64(*off).u64(*len);
            }
            Msg::MemberSlow {
                reporter,
                pool,
                pg,
                pg_epoch,
                device,
            } => {
                e.u32(*reporter)
                    .u32(*pool)
                    .u32(*pg)
                    .u64(*pg_epoch)
                    .u32(*device);
            }
            Msg::ScrubReport {
                pool,
                pg,
                unresolved,
            } => {
                e.u32(*pool).u32(*pg).u32(*unresolved);
            }
            Msg::PgUsage { device, full, pgs } => {
                e.u32(*device).bool(*full).seq(pgs, |e, (p, g, b)| {
                    e.u32(*p).u32(*g).u64(*b);
                });
            }
            Msg::PgUsageOk { known } => {
                e.u32(*known);
            }
            Msg::LeaderState => {}
            Msg::LeaderStateOk {
                heard,
                inconsistent,
                flapping,
                space,
            } => {
                e.seq(heard, |e, (n, ms)| {
                    e.u32(*n).u64(*ms);
                })
                .seq(inconsistent, |e, (p, g, k)| {
                    e.u32(*p).u32(*g).u32(*k);
                })
                .seq(flapping, |e, (n, k)| {
                    e.u32(*n).u32(*k);
                })
                .seq(space, |e, (d, t, f)| {
                    e.u32(*d).u64(*t).u64(*f);
                });
            }
            Msg::Fetch { to, key } => {
                to.enc(e);
                key.enc(e);
            }
            Msg::FetchOk { found } => {
                e.bool(*found);
            }
            Msg::FetchPart {
                to,
                key,
                first,
                kv_from,
            } => {
                to.enc(e);
                key.enc(e);
                e.bool(*first).bytes(kv_from);
            }
            Msg::FetchPartOk { found, next } => {
                e.bool(*found);
                match next {
                    Some(n) => e.bool(true).bytes(n),
                    None => e.bool(false),
                };
            }
            Msg::ReadAt {
                to,
                key,
                layers,
                off,
                len,
            }
            | Msg::Sources {
                to,
                key,
                layers,
                off,
                len,
            } => {
                to.enc(e);
                key.enc(e);
                layers_enc(e, layers);
                e.u64(*off).u64(*len);
            }
            Msg::Versions { to, key } => {
                to.enc(e);
                key.enc(e);
            }
            Msg::Snapshot { to, key, token }
            | Msg::Quiesce { to, key, token }
            | Msg::Release { to, key, token } => {
                to.enc(e);
                key.enc(e);
                e.u64(*token);
            }
            Msg::ReadAtOk { crcs: c, sources } => {
                crcs(e, c);
                e.seq(sources, |e, (b, ep)| {
                    e.u32(*b).u64(*ep);
                });
            }
            Msg::VersionsOk { versions } => {
                e.seq(versions, |e, v| {
                    e.u64(*v);
                });
            }
            Msg::DropVersions {
                to,
                key,
                from,
                upto,
            } => {
                to.enc(e);
                key.enc(e);
                e.u64(*from).u64(*upto);
            }
            Msg::Flatten { to, key, layers } => {
                to.enc(e);
                key.enc(e);
                layers_enc(e, layers);
            }
            Msg::Watch { to, key, version } => {
                to.enc(e);
                key.enc(e);
                e.u64(*version);
            }
            Msg::WatchOk { event, token } => {
                e.u8(*event).u64(*token);
            }
            Msg::Quiesced { to, key, token } => {
                to.enc(e);
                key.enc(e);
                e.u64(*token);
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
            PUSH_KV => Msg::PushKv {
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
            LEASE => Msg::Lease {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                mode: d.u8()?,
                takeover: d.bool()?,
                label: d.str()?.to_string(),
            },
            LEASE_OK => Msg::LeaseOk { version: d.u64()? },
            LEASE_HELD => Msg::LeaseHeld {
                holder: d.u64()?,
                delegates: d.seq(8, |d| d.u64())?,
                remaining_ms: d.u32()?,
                label: d.str()?.to_string(),
            },
            LEASE_DELEGATES => Msg::LeaseDelegates {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                delegates: d.seq(8, |d| d.u64())?,
            },
            LEASE_RELEASE => Msg::LeaseRelease {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
            },
            LEASE_RENEW => Msg::LeaseRenew {
                pool: d.u32()?,
                map_epoch: d.u64()?,
                leases: d.seq(24, |d| {
                    Ok((d.u32()?, d.u64()?, d.bytes()?.to_vec(), d.u64()?))
                })?,
            },
            LEASE_RENEW_OK => Msg::LeaseRenewOk {
                results: d.seq(9, |d| Ok((d.u8()?, d.u64()?)))?,
            },
            BLOCKLIST => Msg::Blocklist {
                ids: d.seq(8, |d| d.u64())?,
            },
            BLOCKLIST_OK => Msg::BlocklistOk {
                map_epoch: d.u64()?,
            },
            DEVICE_FAILED => Msg::DeviceFailed { device: d.u32()? },
            DEVICE_SPACE => Msg::DeviceSpace {
                device: d.u32()?,
                total_bytes: d.u64()?,
                free_bytes: d.u64()?,
            },
            DEVICE_KIND => Msg::DeviceKind {
                device: d.u32()?,
                kind: d.u8()?,
            },
            PG_SPLIT => Msg::PgSplit {
                pool: d.u32()?,
                pg: d.u32()?,
                pg_epoch: d.u64()?,
                device: d.u32()?,
            },
            READ_COPY => Msg::ReadCopy {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                layers: if d.bool()? {
                    Some(layers_dec(&mut d)?)
                } else {
                    None
                },
                off: d.u64()?,
                len: d.u64()?,
            },
            MEMBER_SLOW => Msg::MemberSlow {
                reporter: d.u32()?,
                pool: d.u32()?,
                pg: d.u32()?,
                pg_epoch: d.u64()?,
                device: d.u32()?,
            },
            SCRUB_REPORT => Msg::ScrubReport {
                pool: d.u32()?,
                pg: d.u32()?,
                unresolved: d.u32()?,
            },
            NOTED => Msg::Noted {
                map_epoch: d.u64()?,
            },
            PG_USAGE => Msg::PgUsage {
                device: d.u32()?,
                full: d.bool()?,
                pgs: d.seq(16, |d| Ok((d.u32()?, d.u32()?, d.u64()?)))?,
            },
            PG_USAGE_OK => Msg::PgUsageOk { known: d.u32()? },
            LEADER_STATE => Msg::LeaderState,
            LEADER_STATE_OK => Msg::LeaderStateOk {
                heard: d.seq(12, |d| Ok((d.u32()?, d.u64()?)))?,
                inconsistent: d.seq(12, |d| Ok((d.u32()?, d.u32()?, d.u32()?)))?,
                flapping: d.seq(8, |d| Ok((d.u32()?, d.u32()?)))?,
                space: d.seq(20, |d| Ok((d.u32()?, d.u64()?, d.u64()?)))?,
            },
            FETCH => Msg::Fetch {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
            },
            FETCH_OK => Msg::FetchOk { found: d.bool()? },
            FETCH_PART => Msg::FetchPart {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                first: d.bool()?,
                kv_from: d.bytes()?.to_vec(),
            },
            FETCH_PART_OK => Msg::FetchPartOk {
                found: d.bool()?,
                next: if d.bool()? {
                    Some(d.bytes()?.to_vec())
                } else {
                    None
                },
            },
            READ_AT => Msg::ReadAt {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                layers: layers_dec(&mut d)?,
                off: d.u64()?,
                len: d.u64()?,
            },
            VERSIONS => Msg::Versions {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
            },
            SOURCES => Msg::Sources {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                layers: layers_dec(&mut d)?,
                off: d.u64()?,
                len: d.u64()?,
            },
            READ_AT_OK => Msg::ReadAtOk {
                crcs: crcs(&mut d)?,
                sources: d.seq(12, |d| Ok((d.u32()?, d.u64()?)))?,
            },
            VERSIONS_OK => Msg::VersionsOk {
                versions: d.seq(8, |d| d.u64())?,
            },
            DROP_VERSIONS => Msg::DropVersions {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                from: d.u64()?,
                upto: d.u64()?,
            },
            FLATTEN => Msg::Flatten {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                layers: layers_dec(&mut d)?,
            },
            WATCH => Msg::Watch {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                version: d.u64()?,
            },
            WATCH_OK => Msg::WatchOk {
                event: d.u8()?,
                token: d.u64()?,
            },
            QUIESCED => Msg::Quiesced {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                token: d.u64()?,
            },
            SNAPSHOT => Msg::Snapshot {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                token: d.u64()?,
            },
            QUIESCE => Msg::Quiesce {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                token: d.u64()?,
            },
            RELEASE => Msg::Release {
                to: Target::dec(&mut d)?,
                key: WireKey::dec(&mut d)?,
                token: d.u64()?,
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

fn layers_enc(e: &mut Enc, layers: &[(u32, u64, u64)]) {
    e.seq(layers, |e, (b, lo, hi)| {
        e.u32(*b).u64(*lo).u64(*hi);
    });
}

fn layers_dec(d: &mut Dec<'_>) -> Result<Vec<(u32, u64, u64)>, DecodeError> {
    d.seq(20, |d| Ok((d.u32()?, d.u64()?, d.u64()?)))
}

/// [`Msg::WatchOk`] events.
pub const WATCH_CHANGED: u8 = 0;
pub const WATCH_QUIESCE: u8 = 1;

/// Lease modes (06-metadata.md §9): `rw` is exclusive; `ro` is shared, and
/// only for snapshots (D18).
pub const LEASE_RW: u8 = 0;
/// The longest lease label ([`Msg::Lease`]).
pub const LABEL_MAX: usize = 64;
pub const LEASE_RO: u8 = 1;
/// Results of a renewal ([`Msg::LeaseRenewOk`]).
pub const LEASE_RENEWED: u8 = 0;
/// Another instance holds it now, or the volume is gone.
pub const LEASE_LOST: u8 = 1;
/// Not decided here (map changed, not primary): renew again.
pub const LEASE_RETRY: u8 = 2;
/// How often a holder renews its leases, and how long after the last
/// renewal its header's primary keeps the lease (04-decisions.md D75).
pub const LEASE_RENEW_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
pub const LEASE_EXPIRES: std::time::Duration = std::time::Duration::from_secs(4);

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
/// engine's operations (08-engine.md §7.2), with assertions on attributes in
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
    /// Takes the blocks that version `.0` of this object's name holds and
    /// this one lacks, then deletes that version (04-decisions.md D104).
    Absorb(u64),
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
            WireOp::Absorb(v) => {
                e.u8(14).u64(*v);
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
            14 => WireOp::Absorb(d.u64()?),
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

/// Key–value entries alone ([`Msg::PushKv`]'s payload).
pub fn encode_pairs(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut e = Enc::new();
    enc_pairs(&mut e, pairs);
    e.into_vec()
}

pub fn decode_pairs(b: &[u8]) -> Result<Pairs, DecodeError> {
    let mut d = Dec::new(b);
    let p = dec_pairs(&mut d)?;
    d.finish()?;
    Ok(p)
}

/// What an entry adds to a [`Msg::PushKv`] payload, counted as the
/// sender splits a table into parts.
pub fn pair_bytes(k: &[u8], v: &[u8]) -> usize {
    k.len() + v.len() + 8
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
            lease: 5,
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
            Msg::PushKv {
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
            Msg::Lease {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                mode: LEASE_RW,
                takeover: true,
                label: "vhost:node-1".into(),
            },
            Msg::LeaseOk { version: 3 },
            Msg::LeaseHeld {
                holder: 9,
                delegates: vec![10, 11],
                remaining_ms: 1500,
                label: "tool".into(),
            },
            Msg::LeaseDelegates {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                delegates: vec![12],
            },
            Msg::LeaseRelease {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
            },
            Msg::LeaseRenew {
                pool: 0,
                map_epoch: 7,
                leases: vec![(1, 2, b"h/1".to_vec(), 8), (5, 6, b"h/2".to_vec(), 9)],
            },
            Msg::LeaseRenewOk {
                results: vec![(LEASE_RENEWED, 2), (LEASE_LOST, 0)],
            },
            Msg::Blocklist { ids: vec![77, 78] },
            Msg::BlocklistOk { map_epoch: 8 },
            Msg::DeviceFailed { device: 2001 },
            Msg::DeviceSpace {
                device: 2001,
                total_bytes: 1 << 40,
                free_bytes: 1 << 39,
            },
            Msg::DeviceKind {
                device: 2001,
                kind: 1,
            },
            Msg::PgSplit {
                pool: 1,
                pg: 9,
                pg_epoch: 77,
                device: 2001,
            },
            Msg::ReadCopy {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 0,
                },
                layers: Some(vec![(2, 0, 5)]),
                off: 4096,
                len: 8192,
            },
            Msg::ReadCopy {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"o".to_vec(),
                    version: 3,
                },
                layers: None,
                off: 0,
                len: 4096,
            },
            Msg::MemberSlow {
                reporter: 1000,
                pool: 1,
                pg: 2,
                pg_epoch: 3,
                device: 2000,
            },
            Msg::ScrubReport {
                pool: 1,
                pg: 2,
                unresolved: 4,
            },
            Msg::PgUsage {
                device: 2000,
                full: true,
                pgs: vec![(1, 2, 4096), (1, 3, 0)],
            },
            Msg::PgUsageOk { known: 2 },
            Msg::LeaderState,
            Msg::LeaderStateOk {
                heard: vec![(0, 40), (1, 1200)],
                inconsistent: vec![(1, 2, 3)],
                flapping: vec![(2, 6)],
                space: vec![(1000, 1 << 30, 1 << 29)],
            },
            Msg::Noted { map_epoch: 9 },
            Msg::Fetch {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 0,
                },
            },
            Msg::FetchOk { found: true },
            Msg::FetchPart {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 0,
                },
                first: false,
                kv_from: b"k7".to_vec(),
            },
            Msg::FetchPartOk {
                found: true,
                next: Some(b"k9".to_vec()),
            },
            Msg::FetchPartOk {
                found: false,
                next: None,
            },
            Msg::ReadAt {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 0,
                },
                layers: vec![(2, 0, 5), (0, 1, 3)],
                off: 4096,
                len: 8192,
            },
            Msg::Versions {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 0,
                },
            },
            Msg::VersionsOk {
                versions: vec![1, 3, 9],
            },
            Msg::Sources {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 0,
                },
                layers: vec![(2, 0, 5)],
                off: 0,
                len: 4096,
            },
            Msg::ReadAtOk {
                crcs: vec![1, 2],
                sources: vec![(0, 3), (u32::MAX, 0)],
            },
            Msg::DropVersions {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 0,
                },
                from: 4,
                upto: 9,
            },
            Msg::Flatten {
                to,
                key: WireKey {
                    ns: 1,
                    name: b"c".to_vec(),
                    version: 7,
                },
                layers: vec![(0, 0, 3)],
            },
            Msg::Watch {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                version: 4,
            },
            Msg::WatchOk {
                event: WATCH_QUIESCE,
                token: 11,
            },
            Msg::Quiesced {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                token: 11,
            },
            Msg::Snapshot {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                token: 5,
            },
            Msg::Quiesce {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                token: 5,
            },
            Msg::Release {
                to,
                key: WireKey {
                    ns: 3,
                    name: b"h".to_vec(),
                    version: 0,
                },
                token: 5,
            },
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
            WireOp::Absorb(7),
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
