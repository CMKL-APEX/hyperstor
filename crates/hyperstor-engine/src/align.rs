//! Buffers aligned for `O_DIRECT` and NVMe passthrough I/O.

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Block size of every engine I/O, and the alignment of its buffers.
pub const BLOCK: usize = 4096;

/// A zeroed heap buffer aligned to [`BLOCK`], whose length is a multiple
/// of [`BLOCK`].
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: AlignedBuf owns its allocation exclusively, like a Vec<u8>.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared references only allow reads of the owned bytes.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// A zeroed buffer of `len` bytes, rounded up to whole blocks.
    pub fn zeroed(len: usize) -> Self {
        // SAFETY: the layout has a non-zero size.
        Self::with(len, |l| unsafe { alloc_zeroed(l) })
    }

    /// A buffer holding `data`, padded with zeros to whole blocks. Only
    /// the padding is zeroed: zeroing what the copy overwrites cost nearly
    /// as much as the copy (HS5b, I327).
    pub fn from_slice(data: &[u8]) -> Self {
        // SAFETY: the layout has a non-zero size.
        let b = Self::with(data.len(), |l| unsafe { alloc(l) });
        // SAFETY: `ptr` is valid for `b.len >= data.len()` bytes, and the
        // allocation is new, so it doesn't overlap `data`. Every byte is
        // written here, before the buffer can be read.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), b.ptr.as_ptr(), data.len());
            std::ptr::write_bytes(b.ptr.as_ptr().add(data.len()), 0, b.len - data.len());
        }
        b
    }

    /// A buffer of `len` bytes, rounded up to whole blocks, from `get`.
    fn with(len: usize, get: impl FnOnce(Layout) -> *mut u8) -> Self {
        let len = round_up(len.max(BLOCK));
        let layout = Layout::from_size_align(len, BLOCK).expect("valid layout");
        let p = get(layout);
        let ptr = NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self { ptr, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is valid for len initialized bytes while self lives
        // (zeroed, or copied and padded with zeros).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl AsRef<[u8]> for AlignedBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in deref, and &mut self gives exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, BLOCK).expect("valid layout");
        // SAFETY: ptr was allocated with this exact layout in `with`.
        unsafe { dealloc(self.ptr.as_ptr(), layout) }
    }
}

impl std::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AlignedBuf({} bytes)", self.len)
    }
}

/// Rounds `n` up to a multiple of [`BLOCK`].
pub fn round_up(n: usize) -> usize {
    n.div_ceil(BLOCK) * BLOCK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_and_zeroed() {
        let mut b = AlignedBuf::zeroed(5000);
        assert_eq!(b.len(), 8192);
        assert_eq!(b.as_ptr() as usize % BLOCK, 0);
        assert!(b.iter().all(|&x| x == 0));
        b[8191] = 7;
        let c = AlignedBuf::from_slice(&b[..10]);
        assert_eq!(c.len(), BLOCK);
    }

    /// I327: a copy is aligned and holds the data, then zeros to the end
    /// of its last block, at every length and from any source offset.
    #[test]
    fn copies_are_aligned_and_padded_with_zeros() {
        let src: Vec<u8> = (0..(1 << 20) + 9).map(|i| (i % 251) as u8 + 1).collect();
        for len in [0, 1, 4095, 4096, 4097, 300_000, 1 << 20] {
            for from in [0, 1, 7] {
                // A freed buffer of the same size first, so the copy may
                // land in memory that held other data.
                drop(AlignedBuf::from_slice(&vec![0xee; len]));
                let c = AlignedBuf::from_slice(&src[from..from + len]);
                assert_eq!(c.as_ptr() as usize % BLOCK, 0);
                assert_eq!(c.len(), round_up(len.max(1)));
                assert!(c[..len] == src[from..from + len]);
                assert!(c[len..].iter().all(|&x| x == 0), "padding at {len}");
            }
        }
    }
}
