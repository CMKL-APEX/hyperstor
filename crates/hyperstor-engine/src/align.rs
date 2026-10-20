//! Buffers aligned for `O_DIRECT` and NVMe passthrough I/O.

use std::alloc::{Layout, alloc_zeroed, dealloc};
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
        let len = round_up(len.max(BLOCK));
        let layout = Layout::from_size_align(len, BLOCK).expect("valid layout");
        // SAFETY: the layout has a non-zero size.
        let p = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self { ptr, len }
    }

    /// A buffer holding `data`, padded with zeros to whole blocks.
    pub fn from_slice(data: &[u8]) -> Self {
        let mut b = Self::zeroed(data.len());
        b[..data.len()].copy_from_slice(data);
        b
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
        // SAFETY: ptr is valid for len initialized (zeroed) bytes while self lives.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
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
        // SAFETY: ptr was allocated with this exact layout in `zeroed`.
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
}
