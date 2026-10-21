//! A formatted device: its superblock and its shards (spec/engine.md §7.1).
//!
//! Block 0 holds the device superblock, block 1 a copy. The rest is split
//! evenly into one region per shard (see [`crate::shard`]).

use crate::align::{AlignedBuf, BLOCK};
use crate::dev::BlockDev;
use crate::shard::{Region, Shard};
use crate::types::EngineError;
use hyperstor_codec::{Dec, DecodeError, Enc, crc32c};
use std::sync::Arc;

const DEV_MAGIC: u64 = 0x4853_4445_5643_3031; // "HSDEVC01"
const FORMAT_VERSION: u32 = 1;
/// Smallest and largest default log ring, in blocks (1 MiB and 1 GiB,
/// decisions.md D47).
const MIN_LOG: u64 = 256;
const MAX_LOG: u64 = 262_144;

/// How to format a device.
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    pub device_id: u128,
    pub cluster_id: u128,
    /// One shard per reactor that will use the device.
    pub shards: u32,
    /// Log ring per shard, in blocks; by default a sixteenth of the shard,
    /// between 1 MiB and 1 GiB.
    pub log_blocks: Option<u64>,
}

/// What a device's superblock says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_id: u128,
    pub cluster_id: u128,
    pub blocks: u64,
    pub regions: Vec<Region>,
}

impl DeviceInfo {
    fn encode(&self) -> AlignedBuf {
        let mut e = Enc::with_capacity(BLOCK);
        e.u64(DEV_MAGIC)
            .u32(FORMAT_VERSION)
            .u128(self.device_id)
            .u128(self.cluster_id)
            .u64(self.blocks)
            .seq(&self.regions, |e, r| {
                e.u64(r.start).u64(r.blocks).u64(r.log_blocks);
            });
        let crc = crc32c(e.as_slice());
        e.u32(crc);
        assert!(e.len() <= BLOCK, "too many shards for one superblock");
        AlignedBuf::from_slice(e.as_slice())
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        let mut d = Dec::new(buf);
        let info = (|| -> Result<DeviceInfo, DecodeError> {
            if d.u64()? != DEV_MAGIC || d.u32()? != FORMAT_VERSION {
                return Err(DecodeError::Magic(0));
            }
            Ok(DeviceInfo {
                device_id: d.u128()?,
                cluster_id: d.u128()?,
                blocks: d.u64()?,
                regions: d.seq(24, |d| {
                    Ok(Region {
                        start: d.u64()?,
                        blocks: d.u64()?,
                        log_blocks: d.u64()?,
                    })
                })?,
            })
        })()
        .ok()?;
        let end = d.position();
        let crc = d.u32().ok()?;
        (crc32c(&buf[..end]) == crc).then_some(info)
    }
}

/// Formats `dev`, erasing what it held, and returns its layout.
pub fn format(dev: &Arc<dyn BlockDev>, cfg: &DeviceConfig) -> Result<DeviceInfo, EngineError> {
    let blocks = dev.len() / BLOCK as u64;
    let shards = cfg.shards.max(1) as u64;
    let per = blocks.saturating_sub(2) / shards;
    let log = cfg
        .log_blocks
        .unwrap_or_else(|| (per / 16).clamp(MIN_LOG, MAX_LOG));
    if per < 2 + log + 64 || log < 8 {
        return Err(EngineError::TooSmall);
    }
    let regions: Vec<Region> = (0..shards)
        .map(|i| Region {
            start: 2 + i * per,
            blocks: per,
            log_blocks: log,
        })
        .collect();
    let info = DeviceInfo {
        device_id: cfg.device_id,
        cluster_id: cfg.cluster_id,
        blocks,
        regions,
    };
    for (i, r) in info.regions.iter().enumerate() {
        Shard::format(dev.clone(), *r, cfg.device_id, i as u32)?;
    }
    let sb = info.encode();
    dev.write_batch(&[(0, &sb), (BLOCK as u64, &sb)])?;
    dev.flush()?;
    Ok(info)
}

/// Reads a device's superblock.
pub fn info(dev: &Arc<dyn BlockDev>) -> Result<DeviceInfo, EngineError> {
    let mut a = AlignedBuf::zeroed(BLOCK);
    let mut b = AlignedBuf::zeroed(BLOCK);
    dev.read_batch(&mut [(0, &mut a), (BLOCK as u64, &mut b)])?;
    DeviceInfo::decode(&a)
        .or_else(|| DeviceInfo::decode(&b))
        .ok_or(EngineError::NotFormatted)
}

/// Opens every shard of a formatted device, recovering each.
pub fn open(dev: &Arc<dyn BlockDev>) -> Result<(DeviceInfo, Vec<Shard>), EngineError> {
    let info = info(dev)?;
    let shards = info
        .regions
        .iter()
        .enumerate()
        .map(|(i, r)| Shard::open(dev.clone(), *r, info.device_id, i as u32))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((info, shards))
}
