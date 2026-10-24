//! vhost-user's in-flight I/O tracking for split rings (the vhost-user
//! specification's "Inflight I/O tracking", `INFLIGHT_SHMFD`; decisions.md
//! D130): a buffer the frontend keeps across this service's restarts.
//! Each request taken from a ring is marked in it until its completion is
//! on the used ring, so a restarted service finds the requests the last
//! one took and never answered, and runs them again.
//!
//! The layout is the backend's own (the frontend only keeps the buffer);
//! it follows libvhost-user's, per queue, 64-byte aligned:
//!
//! ```text
//! u64 features; u16 version; u16 desc_num; u16 last_batch_head; u16 used_idx;
//! desc_num × { u8 inflight; u8 padding[5]; u16 next; u64 counter; }
//! ```

use std::fs::File;
use std::os::fd::FromRawFd;
use std::sync::atomic::{Ordering, fence};
use vm_memory::{Bytes, FileOffset, MmapRegion, VolatileMemory};

const HEADER: u64 = 16;
const DESC: u64 = 16;
/// The layout's version, written once a queue's part is in use.
const VERSION: u16 = 1;

/// Bytes one queue of `queue_size` descriptors takes.
pub fn queue_bytes(queue_size: u16) -> u64 {
    (HEADER + DESC * queue_size as u64).next_multiple_of(64)
}

pub struct Inflight {
    region: MmapRegion,
    file: File,
    queues: u16,
    queue_size: u16,
}

/// What a queue's part says after a restart: where the ring's next
/// available request is, and the requests to run again (heads, in the
/// order they were taken).
#[derive(Debug, PartialEq, Eq)]
pub struct Recovered {
    pub next_avail: u16,
    pub heads: Vec<u16>,
    pub next_counter: u64,
}

impl Inflight {
    /// A fresh, zeroed buffer (`GET_INFLIGHT_FD`).
    pub fn create(queues: u16, queue_size: u16) -> Result<Inflight, String> {
        // SAFETY: memfd_create with a static name; the descriptor is owned
        // by the File below.
        let fd = unsafe { libc::memfd_create(c"hyperstor-inflight".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(format!("memfd: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let file = unsafe { File::from_raw_fd(fd) };
        let size = queue_bytes(queue_size) * queues as u64;
        file.set_len(size).map_err(|e| format!("memfd: {e}"))?;
        Self::open(file, 0, size, queues, queue_size)
    }

    /// The frontend's buffer handed back (`SET_INFLIGHT_FD`).
    pub fn open(
        file: File,
        offset: u64,
        size: u64,
        queues: u16,
        queue_size: u16,
    ) -> Result<Inflight, String> {
        if queues == 0 || queue_size == 0 || size < queue_bytes(queue_size) * queues as u64 {
            return Err(format!(
                "an in-flight buffer of {size} bytes for {queues} queues of {queue_size}"
            ));
        }
        let mapped = file.try_clone().map_err(|e| e.to_string())?;
        let region = MmapRegion::from_file(FileOffset::new(mapped, offset), size as usize)
            .map_err(|e| format!("mapping the in-flight buffer: {e}"))?;
        Ok(Inflight {
            region,
            file,
            queues,
            queue_size,
        })
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn size(&self) -> u64 {
        queue_bytes(self.queue_size) * self.queues as u64
    }

    pub fn queues(&self) -> u16 {
        self.queues
    }

    pub fn queue_size(&self) -> u16 {
        self.queue_size
    }

    fn at(&self, q: u16, field: u64) -> Option<usize> {
        (q < self.queues).then(|| (queue_bytes(self.queue_size) * q as u64 + field) as usize)
    }

    fn desc(&self, q: u16, head: u16, field: u64) -> Option<usize> {
        (head < self.queue_size)
            .then(|| self.at(q, HEADER + DESC * head as u64 + field))
            .flatten()
    }

    fn put<T: vm_memory::ByteValued>(&self, at: Option<usize>, v: T) {
        if let Some(at) = at {
            let _ = self.region.as_volatile_slice().write_obj(v, at);
        }
    }

    fn get<T: vm_memory::ByteValued + Default>(&self, at: Option<usize>) -> T {
        at.and_then(|at| self.region.as_volatile_slice().read_obj(at).ok())
            .unwrap_or_default()
    }

    /// Request `head` of queue `q` is taken from the ring, as the
    /// `counter`th.
    pub fn take(&self, q: u16, head: u16, counter: u64) {
        self.put(self.desc(q, head, 8), counter);
        fence(Ordering::Release);
        self.put(self.desc(q, head, 0), 1u8);
    }

    /// Request `head`'s completion is about to go on the used ring.
    pub fn pre_put(&self, q: u16, head: u16) {
        self.put(self.at(q, 12), head);
        fence(Ordering::Release);
    }

    /// Request `head`'s completion is on the used ring, whose index is now
    /// `used_idx`.
    pub fn post_put(&self, q: u16, head: u16, used_idx: u16) {
        fence(Ordering::Release);
        self.put(self.desc(q, head, 0), 0u8);
        fence(Ordering::Release);
        self.put(self.at(q, 14), used_idx);
    }

    /// Queue `q` starts with the used ring at `used_idx`: `None` on a
    /// fresh buffer (now marked in use), else where to go on from. A
    /// completion that reached the used ring before its mark was cleared
    /// (the service died between) is counted as done.
    pub fn recover(&self, q: u16, used_idx: u16) -> Option<Recovered> {
        if self.get::<u16>(self.at(q, 8)) == 0 {
            self.put(self.at(q, 8), VERSION);
            self.put(self.at(q, 10), self.queue_size);
            self.put(self.at(q, 14), used_idx);
            return None;
        }
        if self.get::<u16>(self.at(q, 14)) != used_idx {
            let last: u16 = self.get(self.at(q, 12));
            self.put(self.desc(q, last, 0), 0u8);
            fence(Ordering::Release);
            self.put(self.at(q, 14), used_idx);
        }
        let mut taken: Vec<(u64, u16)> = (0..self.queue_size)
            .filter(|h| self.get::<u8>(self.desc(q, *h, 0)) == 1)
            .map(|h| (self.get::<u64>(self.desc(q, h, 8)), h))
            .collect();
        taken.sort();
        Some(Recovered {
            next_avail: used_idx.wrapping_add(taken.len() as u16),
            next_counter: taken.last().map_or(0, |t| t.0 + 1),
            heads: taken.into_iter().map(|t| t.1).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_taken_and_not_answered_are_found_after_a_restart() {
        let a = Inflight::create(2, 8).unwrap();
        assert_eq!(a.size(), 2 * 192);
        // Fresh: nothing to recover, and marked in use.
        assert_eq!(a.recover(1, 5), None);
        // Three taken (heads 4, 2, 7), one answered.
        a.take(1, 4, 0);
        a.take(1, 2, 1);
        a.take(1, 7, 2);
        a.pre_put(1, 2);
        a.post_put(1, 2, 6);
        // The same buffer, opened again by the next process.
        let b = Inflight::open(a.file().try_clone().unwrap(), 0, a.size(), 2, 8).unwrap();
        assert_eq!(
            b.recover(1, 6),
            Some(Recovered {
                next_avail: 8,
                heads: vec![4, 7],
                next_counter: 3
            })
        );
        // Died between putting 4's completion on the used ring and clearing
        // its mark: the used index moved on, so 4 is done.
        b.pre_put(1, 4);
        assert_eq!(
            b.recover(1, 7),
            Some(Recovered {
                next_avail: 8,
                heads: vec![7],
                next_counter: 3
            })
        );
        // Queue 0 is untouched; out-of-range heads and queues are ignored.
        assert_eq!(b.recover(0, 0), None);
        b.take(1, 99, 9);
        b.take(9, 1, 9);
        assert!(Inflight::open(b.file().try_clone().unwrap(), 0, 10, 2, 8).is_err());
    }
}
