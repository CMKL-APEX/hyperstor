//! Hyperstor's local storage engine (spec/engine.md §7, D4, D22, D29, D34).
//!
//! A device is formatted into shards, one per reactor. Each [`Shard`]
//! stores general objects: versioned sparse data with per-block checksums,
//! attributes, and an ordered key–value table, changed by atomic operation
//! lists that are durable when [`Shard::commit`] returns.

pub mod align;
pub mod alloc;
pub mod dev;
pub mod device;
pub mod index;
pub mod shard;
pub mod types;

pub use align::{AlignedBuf, BLOCK};
pub use dev::{BlockDev, FaultDev, FileDev, MemDev, UringDev};
pub use device::{DeviceConfig, DeviceInfo, format, open};
pub use shard::{DEFER_MAX, Region, Shard, ShardStats};
pub use types::{Committed, EngineError, Namespace, ObjKey, ObjStat, Op, OpError, OpList};
