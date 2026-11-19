//! vhost-user's dirty-page log (the vhost-user specification's
//! "Migration", `LOG_SHMFD` and `VHOST_F_LOG_ALL`; 04-decisions.md D349):
//! a bitmap the frontend gives with `SET_LOG_BASE`, one bit per 4 KiB page
//! of guest-physical memory, byte by byte (bit `page % 8` of byte
//! `page / 8`), as libvhost-user sets it. While the frontend copies a
//! running guest's memory to another host, every page the backend writes
//! (data a read brought in, a status byte, the used ring) is marked, so
//! the frontend copies it again.

use std::fs::File;
use std::sync::atomic::{AtomicU8, Ordering};
use vm_memory::{FileOffset, MmapRegion, VolatileMemory};

/// The log's page: the unit of guest memory one bit stands for.
pub const PAGE: u64 = 4096;

pub struct DirtyLog {
    region: MmapRegion,
    /// Where the bitmap starts in the mapping: the frontend's offset may
    /// not be page-aligned, as a mapping's must.
    at: usize,
    /// The bitmap's bytes.
    len: usize,
}

impl DirtyLog {
    /// Maps the frontend's log: `size` bytes of `file` from `offset`.
    pub fn map(file: File, offset: u64, size: u64) -> Result<DirtyLog, String> {
        if size == 0 || size > 1 << 40 {
            return Err(format!("a dirty log of {size} bytes"));
        }
        let start = offset & !(PAGE - 1);
        let at = (offset - start) as usize;
        let region = MmapRegion::from_file(FileOffset::new(file, start), at + size as usize)
            .map_err(|e| format!("mapping the dirty log: {e}"))?;
        Ok(DirtyLog {
            region,
            at,
            len: size as usize,
        })
    }

    /// Marks the pages of `len` bytes from guest address `gpa`. Pages past
    /// the bitmap aren't guest memory the frontend copies: they are left.
    pub fn mark(&self, gpa: u64, len: u64) {
        if len == 0 {
            return;
        }
        let (first, last) = (gpa / PAGE, gpa.saturating_add(len - 1) / PAGE);
        for page in first..=last {
            let byte = (page / 8) as usize;
            if byte >= self.len {
                return;
            }
            if let Ok(b) = self.region.get_atomic_ref::<AtomicU8>(self.at + byte) {
                b.fetch_or(1 << (page % 8), Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    fn memfd(len: u64) -> File {
        // SAFETY: memfd_create with a static name; the descriptor is owned
        // by the File.
        let fd = unsafe { libc::memfd_create(c"log-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: a fresh descriptor nothing else owns.
        let f = unsafe { File::from_raw_fd(fd) };
        f.set_len(len).unwrap();
        f
    }

    fn bits(f: &File, offset: u64, len: usize) -> Vec<u64> {
        use std::os::unix::fs::FileExt;
        let mut b = vec![0u8; len];
        f.read_exact_at(&mut b, offset).unwrap();
        (0..len as u64 * 8)
            .filter(|p| b[(*p / 8) as usize] & (1 << (p % 8)) != 0)
            .collect()
    }

    /// One bit per page, byte by byte, at the frontend's offset (not
    /// page-aligned here); writes past the bitmap are left out.
    #[test]
    fn written_pages_are_marked_and_only_they() {
        let f = memfd(2 * PAGE);
        let log = DirtyLog::map(f.try_clone().unwrap(), 100, 4).unwrap();
        log.mark(0, 1);
        log.mark(3 * PAGE + 4095, 2);
        log.mark(9 * PAGE, 3 * PAGE);
        log.mark(20 * PAGE, 0);
        log.mark(31 * PAGE, 4 * PAGE);
        assert_eq!(bits(&f, 100, 4), vec![0, 3, 4, 9, 10, 11, 31]);
        assert_eq!(bits(&f, 0, 100), Vec::<u64>::new());
        assert_eq!(bits(&f, 104, 100), Vec::<u64>::new());
    }
}
