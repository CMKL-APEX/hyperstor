//! Shared byte buffers: a payload received once, sent on to each replica
//! and written by the engine, without a copy for any of them (HS5b, I340).

use std::ops::{Deref, Range};
use std::sync::Arc;

use crate::align::{AlignedBuf, BLOCK};

/// Bytes of a heap buffer, read-only and shared: a clone or a slice
/// shares the memory. Received payloads are held in an [`AlignedBuf`], so
/// pieces that start on a block can go to the device as they are. An
/// empty one holds no memory: most frames have no payload.
#[derive(Clone)]
pub struct IoBuf {
    mem: Option<Arc<Mem>>,
    range: Range<usize>,
}

enum Mem {
    Vec(Vec<u8>),
    Aligned(AlignedBuf),
}

impl IoBuf {
    /// The first `len` bytes of `buf`.
    pub fn aligned(buf: AlignedBuf, len: usize) -> IoBuf {
        assert!(
            len <= buf.len(),
            "{len} bytes of a {}-byte buffer",
            buf.len()
        );
        IoBuf {
            mem: Some(Arc::new(Mem::Aligned(buf))),
            range: 0..len,
        }
    }

    /// `self[range]`, sharing the memory.
    pub fn slice(&self, range: Range<usize>) -> IoBuf {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "{range:?} of {} bytes",
            self.len()
        );
        IoBuf {
            mem: self.mem.clone(),
            range: self.range.start + range.start..self.range.start + range.end,
        }
    }

    /// Whether the bytes start on a block, as `O_DIRECT` needs.
    pub fn is_aligned(&self) -> bool {
        (self.as_ptr() as usize).is_multiple_of(BLOCK)
    }

    /// The bytes as a vector: the one this was made from when nothing
    /// else shares it, else a copy.
    pub fn into_vec(self) -> Vec<u8> {
        let Some(mem) = self.mem else {
            return Vec::new();
        };
        let whole = self.range.start == 0 && self.range.end == mem.bytes().len();
        match Arc::try_unwrap(mem) {
            Ok(Mem::Vec(v)) if whole => v,
            Ok(m) => m.bytes()[self.range].to_vec(),
            Err(m) => m.bytes()[self.range].to_vec(),
        }
    }
}

impl Mem {
    fn bytes(&self) -> &[u8] {
        match self {
            Mem::Vec(v) => v,
            Mem::Aligned(a) => a,
        }
    }
}

impl Deref for IoBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.mem {
            Some(m) => &m.bytes()[self.range.clone()],
            None => &[],
        }
    }
}

impl AsRef<[u8]> for IoBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Default for IoBuf {
    fn default() -> IoBuf {
        IoBuf {
            mem: None,
            range: 0..0,
        }
    }
}

impl From<Vec<u8>> for IoBuf {
    fn from(v: Vec<u8>) -> IoBuf {
        if v.is_empty() {
            return IoBuf::default();
        }
        IoBuf {
            range: 0..v.len(),
            mem: Some(Arc::new(Mem::Vec(v))),
        }
    }
}

impl From<AlignedBuf> for IoBuf {
    fn from(b: AlignedBuf) -> IoBuf {
        let len = b.len();
        IoBuf::aligned(b, len)
    }
}

/// Equal when the bytes are, wherever they are held.
impl PartialEq for IoBuf {
    fn eq(&self, other: &IoBuf) -> bool {
        **self == **other
    }
}

impl Eq for IoBuf {}

impl std::fmt::Debug for IoBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IoBuf({} bytes)", self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I340: slices of slices share the memory and hold the bytes cut;
    /// a received payload's pieces that start on a block are aligned.
    #[test]
    fn slices_share_the_bytes_and_keep_their_alignment() {
        let data: Vec<u8> = (0..5 * BLOCK + 100).map(|i| (i % 251) as u8).collect();
        let b = IoBuf::aligned(AlignedBuf::from_slice(&data), data.len());
        assert!(*b == data[..]);
        let s = b.slice(BLOCK..4 * BLOCK + 7);
        assert!(*s == data[BLOCK..4 * BLOCK + 7]);
        assert!(s.is_aligned());
        let t = s.slice(BLOCK..2 * BLOCK);
        assert!(*t == data[2 * BLOCK..3 * BLOCK]);
        assert!(t.is_aligned());
        assert!(!s.slice(1..BLOCK).is_aligned());
        assert_eq!(t.as_ptr(), b[2 * BLOCK..].as_ptr());
        assert!(b.slice(3..3).is_empty());
        let empty = IoBuf::from(Vec::new());
        assert!(empty.is_empty() && empty.slice(0..0).is_empty());
        assert!(empty.into_vec().is_empty());
    }

    #[test]
    fn into_vec_gives_back_an_unshared_vector_and_copies_otherwise() {
        let v = vec![7u8; 100];
        let p = v.as_ptr();
        let back = IoBuf::from(v).into_vec();
        assert_eq!(back.as_ptr(), p);
        let b = IoBuf::from(vec![1u8, 2, 3, 4]);
        let other = b.clone();
        assert_eq!(b.into_vec(), [1, 2, 3, 4]);
        assert_eq!(other.slice(1..3).into_vec(), [2, 3]);
        let a = IoBuf::aligned(AlignedBuf::from_slice(&[9; 10]), 10);
        assert_eq!(a.into_vec(), [9; 10]);
    }

    #[test]
    #[should_panic]
    fn a_slice_past_the_end_panics() {
        IoBuf::from(vec![0u8; 10]).slice(5..11);
    }
}
