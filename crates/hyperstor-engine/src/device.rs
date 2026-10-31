//! A formatted device: its superblock and its shards (spec/08-engine.md §7.1).
//!
//! Block 0 holds the device superblock, block 1 a copy. The rest is split
//! evenly into one region per shard (see [`crate::shard`]).
//!
//! The superblock carries feature flags (D157): a device whose flags are
//! all clear is written as format version 1, which builds before the flags
//! read; one with any set, as version 2.

use crate::align::{AlignedBuf, BLOCK};
use crate::dev::BlockDev;
use crate::shard::{Region, Shard};
use crate::types::EngineError;
use hyperstor_codec::{Dec, DecodeError, Enc, crc32c};
use std::sync::Arc;

const DEV_MAGIC: u64 = 0x4853_4445_5643_3031; // "HSDEVC01"
/// Without feature flags.
const FORMAT_V1: u32 = 1;
/// With feature flags (D157).
const FORMAT_V2: u32 = 2;
/// The features this build knows, by kind. None yet: each format change
/// from now on adds its flag here.
const KNOWN_COMPAT: u64 = 0;
const KNOWN_RO_COMPAT: u64 = 0;
const KNOWN_INCOMPAT: u64 = 0;
/// Smallest and largest default log ring, in blocks (1 MiB and 1 GiB,
/// 04-decisions.md D47).
const MIN_LOG: u64 = 256;
const MAX_LOG: u64 = 262_144;

/// A device's feature flags (D157), kept as ext4 and XFS keep theirs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    /// Features a build that doesn't know them may ignore.
    pub compat: u64,
    /// Features a build that doesn't know them could only read past. No
    /// build opens a device read-only yet, so these are refused as well.
    pub ro_compat: u64,
    /// Features a build that doesn't know them must refuse.
    pub incompat: u64,
}

impl Features {
    pub fn is_empty(&self) -> bool {
        *self == Features::default()
    }

    /// The flags this build can't open a device with: the incompatible and
    /// read-only-compatible ones it doesn't know.
    pub fn refused(&self) -> Features {
        Features {
            compat: 0,
            ro_compat: self.ro_compat & !KNOWN_RO_COMPAT,
            incompat: self.incompat & !KNOWN_INCOMPAT,
        }
    }

    /// The compatible flags this build doesn't know, which it ignores.
    pub fn ignored(&self) -> u64 {
        self.compat & !KNOWN_COMPAT
    }
}

/// How to format a device.
#[derive(Debug, Clone, Default)]
pub struct DeviceConfig {
    pub device_id: u128,
    pub cluster_id: u128,
    /// One shard per reactor that will use the device.
    pub shards: u32,
    /// Log ring per shard, in blocks; by default a sixteenth of the shard,
    /// between 1 MiB and 1 GiB.
    pub log_blocks: Option<u64>,
    pub features: Features,
}

/// What a device's superblock says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_id: u128,
    pub cluster_id: u128,
    pub blocks: u64,
    pub regions: Vec<Region>,
    pub features: Features,
}

/// Why a superblock copy wasn't read.
#[derive(Debug, PartialEq, Eq)]
enum Unread {
    /// No hyperstor superblock there.
    Blank,
    /// Ours, but its checksum or layout is wrong.
    Corrupt,
    /// Ours, in a format version this build doesn't read.
    Version(u32),
}

impl DeviceInfo {
    /// Whether the superblock is device `device`'s of cluster `cluster`.
    /// A cluster of 0 on either side (formatted before clusters were
    /// recorded; a map without one) matches any.
    pub fn belongs_to(&self, device: u128, cluster: u128) -> Result<(), EngineError> {
        let other_cluster = self.cluster_id != 0 && cluster != 0 && self.cluster_id != cluster;
        if self.device_id != device || other_cluster {
            return Err(EngineError::NotThisDevice {
                found_device: self.device_id,
                found_cluster: self.cluster_id,
                device,
                cluster,
            });
        }
        Ok(())
    }

    fn encode(&self) -> AlignedBuf {
        let mut e = Enc::with_capacity(BLOCK);
        e.u64(DEV_MAGIC);
        if self.features.is_empty() {
            e.u32(FORMAT_V1);
        } else {
            e.u32(FORMAT_V2)
                .u64(self.features.compat)
                .u64(self.features.ro_compat)
                .u64(self.features.incompat);
        }
        e.u128(self.device_id)
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

    fn decode(buf: &[u8]) -> Result<Self, Unread> {
        let mut d = Dec::new(buf);
        if d.u64() != Ok(DEV_MAGIC) {
            return Err(Unread::Blank);
        }
        let features = match d.u32().map_err(|_| Unread::Corrupt)? {
            FORMAT_V1 => Features::default(),
            FORMAT_V2 => Features {
                compat: d.u64().map_err(|_| Unread::Corrupt)?,
                ro_compat: d.u64().map_err(|_| Unread::Corrupt)?,
                incompat: d.u64().map_err(|_| Unread::Corrupt)?,
            },
            v => return Err(Unread::Version(v)),
        };
        let info = (|| -> Result<DeviceInfo, DecodeError> {
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
                features,
            })
        })()
        .map_err(|_| Unread::Corrupt)?;
        let end = d.position();
        let crc = d.u32().map_err(|_| Unread::Corrupt)?;
        if crc32c(&buf[..end]) == crc {
            Ok(info)
        } else {
            Err(Unread::Corrupt)
        }
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
        features: cfg.features,
    };
    for (i, r) in info.regions.iter().enumerate() {
        Shard::format(dev.clone(), *r, cfg.device_id, i as u32)?;
    }
    let sb = info.encode();
    dev.write_batch(&[(0, &sb), (BLOCK as u64, &sb)])?;
    dev.flush()?;
    Ok(info)
}

/// Reads a device's superblock. Only a device with no hyperstor superblock
/// in either copy is [`EngineError::NotFormatted`], the one error a caller
/// may answer by formatting: a newer format or a damaged superblock is
/// someone's data.
pub fn info(dev: &Arc<dyn BlockDev>) -> Result<DeviceInfo, EngineError> {
    let mut a = AlignedBuf::zeroed(BLOCK);
    let mut b = AlignedBuf::zeroed(BLOCK);
    dev.read_batch(&mut [(0, &mut a), (BLOCK as u64, &mut b)])?;
    let (a, b) = match DeviceInfo::decode(&a) {
        Ok(info) => return Ok(info),
        Err(ea) => match DeviceInfo::decode(&b) {
            Ok(info) => return Ok(info),
            Err(eb) => (ea, eb),
        },
    };
    match (a, b) {
        (Unread::Version(v), _) | (_, Unread::Version(v)) => Err(EngineError::NewerFormat(v)),
        (Unread::Blank, Unread::Blank) => Err(EngineError::NotFormatted),
        _ => Err(EngineError::Corrupt(
            "device superblock: no copy passes its checksum".into(),
        )),
    }
}

/// Records `cluster` in a superblock written without one (cluster 0, by
/// builds before it was recorded): copy 1 first, then copy 0, each flushed,
/// so a crash at any point leaves a valid copy that reads as before or as
/// after. A superblock of another cluster is refused. The shards are
/// untouched: they never use the superblock's blocks.
pub fn record_cluster(dev: &Arc<dyn BlockDev>, cluster: u128) -> Result<DeviceInfo, EngineError> {
    let mut sb = info(dev)?;
    sb.belongs_to(sb.device_id, cluster)?;
    if sb.cluster_id == cluster {
        return Ok(sb);
    }
    sb.cluster_id = cluster;
    let buf = sb.encode();
    for at in [BLOCK as u64, 0] {
        dev.write_batch(&[(at, &buf)])?;
        dev.flush()?;
    }
    Ok(sb)
}

/// Opens every shard of a formatted device, recovering each. A device with
/// a feature this build can't use is refused before anything is read
/// further (D157).
pub fn open(dev: &Arc<dyn BlockDev>) -> Result<(DeviceInfo, Vec<Shard>), EngineError> {
    let info = info(dev)?;
    let refused = info.features.refused();
    if !refused.is_empty() {
        return Err(EngineError::UnsupportedFeatures {
            ro_compat: refused.ro_compat,
            incompat: refused.incompat,
        });
    }
    let shards = info
        .regions
        .iter()
        .enumerate()
        .map(|(i, r)| Shard::open(dev.clone(), *r, info.device_id, i as u32))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((info, shards))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(features: Features) -> DeviceInfo {
        DeviceInfo {
            device_id: 7,
            cluster_id: 9,
            blocks: 4096,
            regions: vec![Region {
                start: 2,
                blocks: 4094,
                log_blocks: 256,
            }],
            features,
        }
    }

    #[test]
    fn without_flags_the_superblock_is_version_1_as_before() {
        // Version 1 as builds before D157 wrote it, field by field.
        let mut e = Enc::with_capacity(BLOCK);
        e.u64(DEV_MAGIC).u32(1).u128(7).u128(9).u64(4096).seq(
            &[(2u64, 4094u64, 256u64)],
            |e, r| {
                e.u64(r.0).u64(r.1).u64(r.2);
            },
        );
        let crc = crc32c(e.as_slice());
        e.u32(crc);
        let old = e.as_slice().to_vec();
        assert_eq!(&info(Features::default()).encode()[..old.len()], &old[..]);
        assert_eq!(DeviceInfo::decode(&old), Ok(info(Features::default())));
    }

    #[test]
    fn flags_make_it_version_2_and_survive_a_round_trip() {
        let f = Features {
            compat: 1,
            ro_compat: 1 << 20,
            incompat: 1 << 63,
        };
        let buf = info(f).encode();
        assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()), 2);
        assert_eq!(DeviceInfo::decode(&buf), Ok(info(f)));
        assert_eq!(f.refused(), Features { compat: 0, ..f });
    }

    /// I203: a superblock names its device and cluster; another's is
    /// refused, and one written without a cluster gets it recorded once.
    #[test]
    fn a_device_belongs_to_its_device_and_cluster() {
        let i = info(Features::default());
        assert!(i.belongs_to(7, 9).is_ok());
        assert!(i.belongs_to(7, 0).is_ok(), "a map without a cluster");
        assert!(matches!(
            i.belongs_to(8, 9),
            Err(EngineError::NotThisDevice {
                found_device: 7,
                device: 8,
                ..
            })
        ));
        assert!(i.belongs_to(7, 10).is_err());
        let dev: Arc<dyn BlockDev> = Arc::new(crate::MemDev::new(64 << 20));
        let made = format(
            &dev,
            &DeviceConfig {
                device_id: 7,
                cluster_id: 0,
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(made.belongs_to(7, 42).is_ok(), "no cluster yet: any");
        assert_eq!(record_cluster(&dev, 42).unwrap().cluster_id, 42);
        let (read, shards) = open(&dev).unwrap();
        assert_eq!((read.cluster_id, shards.len()), (42, 1));
        assert!(read.belongs_to(7, 43).is_err());
        assert!(record_cluster(&dev, 43).is_err());
        assert_eq!(record_cluster(&dev, 42).unwrap(), read);
        // Each copy alone reads the same: a crash between the two writes
        // leaves either.
        let mut b = AlignedBuf::zeroed(BLOCK);
        dev.read_batch(&mut [(BLOCK as u64, &mut b)]).unwrap();
        assert_eq!(DeviceInfo::decode(&b), Ok(read));
    }

    #[test]
    fn unread_superblocks_say_why() {
        assert_eq!(DeviceInfo::decode(&[0; BLOCK]), Err(Unread::Blank));
        let mut buf = info(Features::default()).encode().to_vec();
        buf[8..12].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(DeviceInfo::decode(&buf), Err(Unread::Version(9)));
        let mut buf = info(Features::default()).encode().to_vec();
        buf[20] ^= 1;
        assert_eq!(DeviceInfo::decode(&buf), Err(Unread::Corrupt));
    }
}
