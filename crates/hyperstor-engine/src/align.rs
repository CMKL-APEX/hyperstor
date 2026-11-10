//! Buffers aligned for `O_DIRECT` and NVMe passthrough I/O, from
//! `hyperstor-codec` so that the wire side can receive into them (I338).

use hyperstor_codec::IoBuf;
pub use hyperstor_codec::align::*;

/// One write of a commit round: a buffer of the engine's own, or a piece
/// of a payload as it arrived, which starts on a block (I338).
pub enum WriteBuf {
    Own(AlignedBuf),
    Shared(IoBuf),
}

impl std::ops::Deref for WriteBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            WriteBuf::Own(b) => b,
            WriteBuf::Shared(b) => b,
        }
    }
}

impl AsRef<[u8]> for WriteBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl From<AlignedBuf> for WriteBuf {
    fn from(b: AlignedBuf) -> WriteBuf {
        WriteBuf::Own(b)
    }
}

impl std::fmt::Debug for WriteBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WriteBuf({} bytes)", self.len())
    }
}
