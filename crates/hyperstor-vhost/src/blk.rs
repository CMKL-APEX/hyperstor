//! virtio-blk as `hyperstor-vhost` serves it (frontends.md §11.1): the
//! feature bits, the configuration space, and requests decoded from a
//! descriptor chain's bytes. Nothing here touches guest memory or a
//! volume; [`crate::device`] does.

use virtio_bindings::virtio_blk::{
    VIRTIO_BLK_F_BLK_SIZE, VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_MQ, VIRTIO_BLK_F_SEG_MAX,
    VIRTIO_BLK_F_TOPOLOGY, VIRTIO_BLK_F_WRITE_ZEROES, VIRTIO_BLK_ID_BYTES, VIRTIO_BLK_S_IOERR,
    VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP, VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN,
    VIRTIO_BLK_T_OUT, VIRTIO_BLK_T_WRITE_ZEROES,
};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::virtio_ring::{VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC};

/// The volume's block size, which the guest sees as the disk's physical
/// block size: requests in whole blocks go to the volume as they are.
pub const BLOCK: u64 = 4096;
/// The disk's logical block size (512e, decisions.md D125): what cloud
/// images' partition tables and firmware assume. A request is in whole
/// sectors; a partial block at either end is read, merged and written.
pub const SECTOR: u64 = 512;
/// Segments per request and bytes per write-zeroes range.
pub const SEG_MAX: u32 = 128;
pub const ZEROES_MAX_SECTORS: u32 = (16 << 20) / 512;
/// Write-zeroes are written in pieces of this many bytes.
pub const ZERO_PIECE: u64 = 1 << 20;

/// Virtio features offered. Not offered yet (implementation-notes.md):
/// discard, which needs a way to drop the head's blocks without
/// uncovering a snapshot's; packed rings, which `virtio-queue` lacks.
pub fn features() -> u64 {
    [
        VIRTIO_BLK_F_SEG_MAX,
        VIRTIO_BLK_F_BLK_SIZE,
        VIRTIO_BLK_F_TOPOLOGY,
        VIRTIO_BLK_F_FLUSH,
        VIRTIO_BLK_F_MQ,
        VIRTIO_BLK_F_WRITE_ZEROES,
        VIRTIO_RING_F_INDIRECT_DESC,
        VIRTIO_RING_F_EVENT_IDX,
        VIRTIO_F_VERSION_1,
    ]
    .iter()
    .fold(0, |f, b| f | 1 << b)
}

/// `struct virtio_blk_config` (virtio 1.2, section 5.2.4), little-endian,
/// 60 bytes.
pub fn config(size: u64, queues: u16) -> Vec<u8> {
    let mut c = Vec::with_capacity(60);
    c.extend((size / SECTOR).to_le_bytes()); // capacity
    c.extend(0u32.to_le_bytes()); // size_max
    c.extend(SEG_MAX.to_le_bytes()); // seg_max
    c.extend([0u8; 4]); // geometry
    c.extend((SECTOR as u32).to_le_bytes()); // blk_size: logical
    // topology: physical blocks of 2^3 sectors, aligned at sector 0, and
    // the minimum I/O one of them.
    c.push((BLOCK / SECTOR).trailing_zeros() as u8);
    c.push(0);
    c.extend(((BLOCK / SECTOR) as u16).to_le_bytes());
    c.extend(0u32.to_le_bytes()); // opt_io_size: none
    c.push(0); // writeback: write-through (volumes.md §10.1)
    c.push(0);
    c.extend(queues.to_le_bytes()); // num_queues
    c.extend([0u8; 12]); // discard: not offered
    c.extend(ZEROES_MAX_SECTORS.to_le_bytes()); // max_write_zeroes_sectors
    c.extend(1u32.to_le_bytes()); // max_write_zeroes_seg
    c.push(0); // write_zeroes_may_unmap
    c.extend([0u8; 3]);
    debug_assert_eq!(c.len(), 60);
    c
}

/// A request, from the bytes the guest made readable to us (the header,
/// then a write's data or the write-zeroes ranges) and how many bytes it
/// made writable (a read's data, or the id, then the status byte).
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Read {
        off: u64,
        len: u64,
    },
    Write {
        off: u64,
        len: u64,
    },
    Flush,
    GetId,
    /// (offset, length) ranges, in bytes.
    WriteZeroes(Vec<(u64, u64)>),
    Unsupported(u32),
}

/// Why a chain isn't a request we can answer: the status to give (or none
/// if there's no status byte to put it in).
#[derive(Debug, PartialEq, Eq)]
pub enum Bad {
    /// Too short for a header or a status byte: the chain is dropped with
    /// no status.
    Malformed,
    /// A range outside the volume or not in whole sectors.
    Range,
}

/// Decodes a request. `readable` is the first readable bytes (the header
/// and, for write-zeroes, the ranges; a write's data follows in the
/// chain); `readable_len` and `writable_len` are the chain's totals.
pub fn decode(
    readable: &[u8],
    readable_len: u64,
    writable_len: u64,
    size: u64,
) -> Result<Request, Bad> {
    if readable.len() < 16 || writable_len < 1 {
        return Err(Bad::Malformed);
    }
    let kind = u32::from_le_bytes(readable[0..4].try_into().unwrap());
    let sector = u64::from_le_bytes(readable[8..16].try_into().unwrap());
    let data_in = writable_len - 1;
    let data_out = readable_len - 16;
    let range = |off: u64, len: u64| {
        let end = off.checked_add(len).ok_or(Bad::Range)?;
        if !len.is_multiple_of(SECTOR) || end > size {
            return Err(Bad::Range);
        }
        Ok((off, len))
    };
    let off = sector.checked_mul(SECTOR).ok_or(Bad::Range)?;
    Ok(match kind {
        VIRTIO_BLK_T_IN => {
            let (off, len) = range(off, data_in)?;
            Request::Read { off, len }
        }
        VIRTIO_BLK_T_OUT => {
            let (off, len) = range(off, data_out)?;
            Request::Write { off, len }
        }
        VIRTIO_BLK_T_FLUSH => Request::Flush,
        VIRTIO_BLK_T_GET_ID => {
            if data_in < VIRTIO_BLK_ID_BYTES as u64 {
                return Err(Bad::Malformed);
            }
            Request::GetId
        }
        VIRTIO_BLK_T_WRITE_ZEROES => {
            let segs = &readable[16..];
            if segs.len() < 16 || !segs.len().is_multiple_of(16) {
                return Err(Bad::Malformed);
            }
            let mut ranges = Vec::new();
            for s in segs.as_chunks::<16>().0 {
                let sector = u64::from_le_bytes(s[0..8].try_into().unwrap());
                let n = u32::from_le_bytes(s[8..12].try_into().unwrap()) as u64;
                let off = sector.checked_mul(SECTOR).ok_or(Bad::Range)?;
                ranges.push(range(off, n * SECTOR)?);
            }
            Request::WriteZeroes(ranges)
        }
        k => Request::Unsupported(k),
    })
}

pub const S_OK: u8 = VIRTIO_BLK_S_OK as u8;
pub const S_IOERR: u8 = VIRTIO_BLK_S_IOERR as u8;
pub const S_UNSUPP: u8 = VIRTIO_BLK_S_UNSUPP as u8;

/// The whole volume blocks that bytes `off..off + len` are in.
pub fn blocks(off: u64, len: u64) -> (u64, u64) {
    (off / BLOCK * BLOCK, (off + len).div_ceil(BLOCK) * BLOCK)
}

/// The disk's id, as `GET_ID` returns it: the serial padded with zeros.
pub fn id(serial: &str) -> [u8; VIRTIO_BLK_ID_BYTES as usize] {
    let mut b = [0u8; VIRTIO_BLK_ID_BYTES as usize];
    let s = serial.as_bytes();
    let n = s.len().min(b.len());
    b[..n].copy_from_slice(&s[..n]);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u32, sector: u64) -> Vec<u8> {
        let mut h = kind.to_le_bytes().to_vec();
        h.extend(0u32.to_le_bytes());
        h.extend(sector.to_le_bytes());
        h
    }

    #[test]
    fn requests_decode_to_whole_sectors_inside_the_volume() {
        let size = 1 << 20;
        let h = header(VIRTIO_BLK_T_IN, 8);
        assert_eq!(
            decode(&h, 16, 8193, size),
            Ok(Request::Read {
                off: 4096,
                len: 8192
            })
        );
        let h = header(VIRTIO_BLK_T_OUT, 0);
        assert_eq!(
            decode(&h, 16 + 4096, 1, size),
            Ok(Request::Write { off: 0, len: 4096 })
        );
        // Sectors, not only blocks (512e); not part of one, or past the end.
        assert_eq!(
            decode(&header(VIRTIO_BLK_T_OUT, 3), 16 + 1024, 1, size),
            Ok(Request::Write {
                off: 1536,
                len: 1024
            })
        );
        assert_eq!(decode(&h, 16 + 100, 1, size), Err(Bad::Range));
        let h = header(VIRTIO_BLK_T_IN, (size / 512) - 8);
        assert_eq!(decode(&h, 16, 8193, size), Err(Bad::Range));
        assert_eq!(decode(&h[..8], 8, 1, size), Err(Bad::Malformed));
        assert_eq!(decode(&h, 16, 0, size), Err(Bad::Malformed), "no status");
        let h = header(VIRTIO_BLK_T_FLUSH, 0);
        assert_eq!(decode(&h, 16, 1, size), Ok(Request::Flush));
        let h = header(VIRTIO_BLK_T_GET_ID, 0);
        assert_eq!(decode(&h, 16, 21, size), Ok(Request::GetId));
        let h = header(99, 0);
        assert_eq!(decode(&h, 16, 1, size), Ok(Request::Unsupported(99)));
        let mut h = header(VIRTIO_BLK_T_WRITE_ZEROES, 0);
        h.extend(16u64.to_le_bytes());
        h.extend(8u32.to_le_bytes());
        h.extend(0u32.to_le_bytes());
        assert_eq!(
            decode(&h, 32, 1, size),
            Ok(Request::WriteZeroes(vec![(8192, 4096)]))
        );
    }

    #[test]
    fn the_config_space_names_the_size_and_queues() {
        let c = config(10 << 30, 4);
        assert_eq!(c.len(), 60);
        assert_eq!(
            u64::from_le_bytes(c[0..8].try_into().unwrap()),
            (10 << 30) / 512
        );
        assert_eq!(u32::from_le_bytes(c[20..24].try_into().unwrap()), 512);
        // Physical blocks of 8 sectors, the minimum I/O one of them.
        assert_eq!((c[24], c[25]), (3, 0));
        assert_eq!(u16::from_le_bytes(c[26..28].try_into().unwrap()), 8);
        assert!(features() & (1 << VIRTIO_BLK_F_TOPOLOGY) != 0);
        assert_eq!(blocks(1536, 1024), (0, 4096));
        assert_eq!(blocks(3584, 1024), (0, 8192));
        assert_eq!(blocks(4096, 4096), (4096, 8192));
        assert_eq!(u16::from_le_bytes(c[34..36].try_into().unwrap()), 4);
        assert_eq!(id("abc")[..4], *b"abc\0");
        assert_eq!(id(&"x".repeat(30)).len(), 20);
        assert!(features() & (1 << VIRTIO_F_VERSION_1) != 0);
    }
}
