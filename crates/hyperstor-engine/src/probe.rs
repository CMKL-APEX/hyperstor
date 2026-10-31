//! What a device already holds, before hyperstor formats it
//! (24-pools-and-devices.md §27.2.2, D168): another system's signature in
//! its first or last 128 KiB, or hyperstor's own superblock. A block
//! device is formatted only when this finds nothing, unless the operator
//! forces it. Why: a renumbered `/dev/nvmeXnY`, or a typo, would otherwise
//! format a disk in use, and a Ceph or LVM disk carries no hyperstor magic.
//!
//! The checks follow the formats' published layouts (as util-linux's
//! libblkid reads them); they are signatures, not proof that a device is
//! empty.

use crate::align::{AlignedBuf, BLOCK};
use crate::dev::BlockDev;
use crate::types::EngineError;
use std::io;
use std::sync::Arc;

/// Bytes read at each end of the device.
const SPAN: usize = 128 << 10;

/// The first signature found on `dev`, described for people ("an LVM
/// physical volume"); `None` when it has none hyperstor knows.
pub fn signature(dev: &Arc<dyn BlockDev>) -> io::Result<Option<String>> {
    match crate::device::info(dev) {
        Ok(i) => {
            return Ok(Some(format!(
                "a hyperstor device (device {} of cluster {})",
                i.device_id, i.cluster_id
            )));
        }
        Err(EngineError::NotFormatted) => {}
        Err(EngineError::Io(e)) => return Err(e),
        // A newer format, or a damaged superblock: still hyperstor's.
        Err(e) => {
            return Ok(Some(format!(
                "a hyperstor device that can't be read here ({e})"
            )));
        }
    }
    let len = dev.len();
    let span = (SPAN as u64).min(len - len % BLOCK as u64) as usize;
    if span == 0 {
        return Ok(None);
    }
    let mut head = AlignedBuf::zeroed(span);
    dev.read(0, &mut head)?;
    let tail_at = (len - span as u64) / BLOCK as u64 * BLOCK as u64;
    let mut tail = AlignedBuf::zeroed(span);
    dev.read(tail_at, &mut tail)?;
    Ok(in_head(&head).or_else(|| in_tail(&tail, len - tail_at)))
}

fn at(b: &[u8], off: usize, magic: &[u8]) -> bool {
    b.get(off..off + magic.len()) == Some(magic)
}

fn le32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Signatures at fixed places in the first 128 KiB.
fn in_head(b: &[u8]) -> Option<String> {
    let found = |s: &str| Some(s.to_string());
    if at(b, 0, b"bluestore block device") {
        return found("a Ceph BlueStore OSD");
    }
    // LVM2: "LABELONE" in one of the first four 512-byte sectors.
    if (0..4).any(|s| at(b, s * 512, b"LABELONE") && at(b, s * 512 + 24, b"LVM2 001")) {
        return found("an LVM physical volume");
    }
    if at(b, 0, b"LUKS\xba\xbe") || at(b, 0, b"SKUL\xba\xbe") {
        return found("a LUKS encrypted volume");
    }
    // md RAID, superblock 1.1 (at 0) and 1.2 (at 4 KiB).
    if [0, 4096].iter().any(|&o| le32(b, o) == Some(0xa92b_4efc)) {
        return found("an md RAID member");
    }
    if at(b, 512, b"EFI PART") || at(b, 4096, b"EFI PART") {
        return found("a GPT partition table");
    }
    if at(b, 0, b"XFSB") {
        return found("an XFS filesystem");
    }
    // ext2/3/4: the superblock at 1 KiB, its magic 56 bytes in.
    if b.get(1080..1082) == Some(&[0x53, 0xef]) {
        return found("an ext2/3/4 filesystem");
    }
    if at(b, 65600, b"_BHRfS_M") {
        return found("a btrfs filesystem");
    }
    if at(b, 3, b"NTFS    ") {
        return found("an NTFS filesystem");
    }
    if at(b, 4096 - 10, b"SWAPSPACE2") || at(b, 4096 - 10, b"SWAP-SPACE") {
        return found("a swap area");
    }
    // Last: an MBR (or a FAT filesystem's boot sector) ends in 55 AA.
    if b.get(510..512) == Some(&[0x55, 0xaa]) {
        return found("an MBR partition table or a FAT filesystem");
    }
    None
}

/// Signatures near the end: md RAID superblocks 0.90 and 1.0, and GPT's
/// backup header in the last sector. `tail` ends `end` bytes into it.
fn in_tail(tail: &[u8], end: u64) -> Option<String> {
    let end = end as usize;
    // 0.90 sits 64 KiB before the end (64 KiB-aligned), 1.0 8 KiB before
    // it (4 KiB-aligned): both on a 4 KiB boundary in the tail.
    for o in (0..tail.len()).step_by(4096) {
        if le32(tail, o) == Some(0xa92b_4efc) {
            return Some("an md RAID member".into());
        }
    }
    if end >= 512 && at(tail, end - 512, b"EFI PART") {
        return Some("a GPT partition table (its backup header)".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::MemDev;
    use crate::device::{DeviceConfig, format};

    fn with(at: u64, bytes: &[u8]) -> Arc<dyn BlockDev> {
        let d: Arc<dyn BlockDev> = Arc::new(MemDev::new(4 << 20));
        let blk = at / BLOCK as u64 * BLOCK as u64;
        let o = (at - blk) as usize;
        let mut b = AlignedBuf::zeroed(if o + bytes.len() > BLOCK {
            BLOCK * 2
        } else {
            BLOCK
        });
        b[o..o + bytes.len()].copy_from_slice(bytes);
        d.write(blk, &b).unwrap();
        d
    }

    #[test]
    fn foreign_signatures_are_named_and_blank_devices_pass() {
        let blank: Arc<dyn BlockDev> = Arc::new(MemDev::new(4 << 20));
        assert_eq!(signature(&blank).unwrap(), None);
        let cases: &[(u64, &[u8], &str)] = &[
            (0, b"bluestore block device\n", "Ceph BlueStore"),
            (
                512,
                b"LABELONE\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0LVM2 001",
                "LVM",
            ),
            (0, b"LUKS\xba\xbe", "LUKS"),
            (4096, &0xa92b_4efcu32.to_le_bytes(), "md RAID"),
            (512, b"EFI PART", "GPT"),
            (0, b"XFSB", "XFS"),
            (1080, &[0x53, 0xef], "ext2/3/4"),
            (65600, b"_BHRfS_M", "btrfs"),
            (3, b"NTFS    ", "NTFS"),
            (4086, b"SWAPSPACE2", "swap"),
            (510, &[0x55, 0xaa], "MBR"),
            // md 1.0, near the end of the device.
            ((4 << 20) - 8192, &0xa92b_4efcu32.to_le_bytes(), "md RAID"),
            // GPT's backup header in the last sector.
            ((4 << 20) - 512, b"EFI PART", "backup header"),
        ];
        for (off, bytes, want) in cases {
            let s = signature(&with(*off, bytes)).unwrap();
            assert!(
                s.as_deref().is_some_and(|s| s.contains(want)),
                "{want} at {off}: {s:?}"
            );
        }
    }

    #[test]
    fn a_hyperstor_device_is_named_with_its_ids() {
        let d: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
        format(
            &d,
            &DeviceConfig {
                device_id: 1002,
                cluster_id: 7,
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let s = signature(&d).unwrap().unwrap();
        assert!(
            s.contains("hyperstor device (device 1002 of cluster 7)"),
            "{s}"
        );
    }
}
