//! Object keys, operations and errors (spec/engine.md §7.2, D34).

use hyperstor_codec::{Dec, DecodeError, Enc};
use thiserror::Error;

/// Object namespaces (spec/engine.md §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Namespace {
    /// Volume chunk versions.
    Vol = 1,
    /// EC fragments.
    Ec = 2,
    /// `.meta` objects: headers, lineages, directories, groups.
    Meta = 3,
    /// `hyperstor-kv` ranges.
    Kv = 4,
    /// S3 object data.
    S3 = 5,
    /// Filesystem file data.
    Fs = 6,
    /// Engine-internal objects and tests.
    Internal = 7,
}

impl Namespace {
    pub fn from_u8(v: u8) -> Result<Self, DecodeError> {
        Ok(match v {
            1 => Self::Vol,
            2 => Self::Ec,
            3 => Self::Meta,
            4 => Self::Kv,
            5 => Self::S3,
            6 => Self::Fs,
            7 => Self::Internal,
            _ => {
                return Err(DecodeError::Invalid {
                    field: "namespace",
                    value: v as u64,
                });
            }
        })
    }
}

/// Key of an object. Keys sort by pool, PG, namespace, name, then version,
/// so a PG's objects are one contiguous range of the index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjKey {
    pub pool: u32,
    pub pg: u32,
    pub ns: Namespace,
    pub name: Vec<u8>,
    pub version: u64,
}

impl ObjKey {
    pub fn new(pool: u32, pg: u32, ns: Namespace, name: impl Into<Vec<u8>>, version: u64) -> Self {
        Self {
            pool,
            pg,
            ns,
            name: name.into(),
            version,
        }
    }

    pub fn encode(&self, e: &mut Enc) {
        e.u32(self.pool)
            .u32(self.pg)
            .u8(self.ns as u8)
            .bytes(&self.name)
            .u64(self.version);
    }

    pub fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            pool: d.u32()?,
            pg: d.u32()?,
            ns: Namespace::from_u8(d.u8()?)?,
            name: d.bytes()?.to_vec(),
            version: d.u64()?,
        })
    }
}

/// One operation of an atomic list (spec/engine.md §7.2).
///
/// Data offsets and lengths are multiples of [`crate::BLOCK`]. Assertions
/// are checked against the object as it was before the list; the other
/// operations apply in order, and any of them creates the object if it is
/// absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// The object's revision must be this one (0: the object must not exist).
    AssertRevision(u64),
    AssertExists,
    AssertAbsent,
    /// Creates the object if it is absent.
    Touch,
    Write {
        off: u64,
        data: Vec<u8>,
    },
    /// Drops the blocks of a range; they read as zeros.
    Zero {
        off: u64,
        len: u64,
    },
    /// Sets the size; blocks wholly past it are dropped.
    Truncate(u64),
    SetAttr(Vec<u8>, Vec<u8>),
    RmAttr(Vec<u8>),
    KvPut(Vec<u8>, Vec<u8>),
    KvDel(Vec<u8>),
    /// Deletes keys in `[start, end)`.
    KvDelRange(Vec<u8>, Vec<u8>),
    /// Deletes the object; later operations in the list create it anew.
    Delete,
}

/// An atomic list of operations on one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpList {
    pub key: ObjKey,
    pub ops: Vec<Op>,
}

/// Outcome of a committed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    /// The object's revision after the list, or 0 if it no longer exists.
    /// Revisions are unique within a shard and only grow, so a deleted and
    /// recreated object never repeats one.
    pub revision: u64,
}

/// Why a list was refused. A refused list changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OpError {
    #[error("assertion failed: object revision is {current}")]
    AssertFailed { current: u64 },
    #[error("offset or length not a multiple of the block size")]
    Unaligned,
    #[error("no space left in the shard")]
    NoSpace,
    #[error("operation list too large for one log record")]
    TooLarge,
}

/// Engine failures. After an I/O error during a commit, the shard refuses
/// further work ([`EngineError::Poisoned`]) until it is reopened, which
/// recovers it from the log.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt metadata: {0}")]
    Corrupt(String),
    #[error("shard failed earlier and must be reopened")]
    Poisoned,
    #[error("not a hyperstor device, or an unsupported format")]
    NotFormatted,
    #[error("device too small for the requested layout")]
    TooSmall,
    #[error("object not found")]
    NotFound,
    #[error("checksum mismatch in logical block {lblock}")]
    Checksum { lblock: u64 },
    #[error("unaligned read")]
    Unaligned,
    #[error("checkpoint needs more space than is free")]
    NoSpace,
}

impl From<DecodeError> for EngineError {
    fn from(e: DecodeError) -> Self {
        EngineError::Corrupt(e.to_string())
    }
}

/// What a stat returns for an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjStat {
    pub size: u64,
    pub revision: u64,
    /// Blocks the object holds.
    pub blocks: u64,
}
