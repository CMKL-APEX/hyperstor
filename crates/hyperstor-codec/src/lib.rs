//! Fixed-layout, little-endian encoding (spec/13-protocol.md §13.1).
//!
//! Both the wire protocol and the engine's on-disk records use this codec.
//! Layouts are explicit and versioned by their users; there is no
//! self-describing format and no general-purpose serializer, so an encoded
//! value is exactly the bytes its `encode` writes.

use thiserror::Error;

/// Maximum length accepted for one length-prefixed byte string (64 MiB).
///
/// Decoders refuse longer prefixes before allocating, so a corrupt or
/// hostile length can't make them allocate unbounded memory.
pub const MAX_BYTES: usize = 64 << 20;

/// Errors while decoding.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum DecodeError {
    #[error("unexpected end of input: needed {needed} more bytes")]
    Eof { needed: usize },
    #[error("length {0} exceeds the limit")]
    TooLong(usize),
    #[error("invalid value for {field}: {value}")]
    Invalid { field: &'static str, value: u64 },
    #[error("invalid UTF-8 string")]
    Utf8,
    #[error("{0} trailing bytes")]
    Trailing(usize),
    #[error("checksum mismatch")]
    Checksum,
    #[error("bad magic {0:#x}")]
    Magic(u64),
}

/// Appends encoded values to a byte vector.
#[derive(Default, Debug)]
pub struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    /// Wraps an existing vector; values are appended after its contents.
    pub fn from_vec(buf: Vec<u8>) -> Self {
        Self { buf }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u128(&mut self, v: u128) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// A `u32` length prefix, then the bytes.
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        assert!(v.len() <= MAX_BYTES, "encoded byte string too long");
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
        self
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    /// Raw bytes with no prefix; the reader must know the length.
    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }
    /// A `u32` count, then each item.
    pub fn seq<T>(&mut self, items: &[T], mut f: impl FnMut(&mut Self, &T)) -> &mut Self {
        self.u32(items.len() as u32);
        for it in items {
            f(self, it);
        }
        self
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
    /// Overwrites a `u32` at `at`, for lengths and checksums known only later.
    pub fn patch_u32(&mut self, at: usize, v: u32) {
        self.buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// Reads encoded values from a byte slice.
#[derive(Debug, Clone)]
pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let rem = self.buf.len() - self.pos;
        if n > rem {
            return Err(DecodeError::Eof { needed: n - rem });
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(DecodeError::Invalid {
                field: "bool",
                value: v as u64,
            }),
        }
    }
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn u128(&mut self) -> Result<u128, DecodeError> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.u32()? as usize;
        if n > MAX_BYTES {
            return Err(DecodeError::TooLong(n));
        }
        self.take(n)
    }
    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| DecodeError::Utf8)
    }
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.take(n)
    }
    /// A `u32` count, then each item. The count is checked against the
    /// remaining input (every item takes at least `min_item` bytes) before
    /// anything is allocated.
    pub fn seq<T>(
        &mut self,
        min_item: usize,
        mut f: impl FnMut(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<Vec<T>, DecodeError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item.max(1)) > self.remaining() {
            return Err(DecodeError::TooLong(n));
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(f(self)?);
        }
        Ok(v)
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn position(&self) -> usize {
        self.pos
    }
    /// Fails if any input is left, for decoders that must consume it all.
    pub fn finish(&self) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(DecodeError::Trailing(n)),
        }
    }
}

/// CRC32C (Castagnoli), hardware-accelerated where the CPU has it.
#[inline]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}

/// Continues a CRC32C over more data.
#[inline]
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    crc32c::crc32c_append(crc, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut e = Enc::new();
        e.u8(7)
            .bool(true)
            .u16(0xbeef)
            .u32(0xdead_beef)
            .u64(u64::MAX - 1)
            .u128(1 << 100)
            .bytes(b"hello")
            .str("wörld")
            .raw(&[1, 2, 3])
            .seq(&[10u32, 20, 30], |e, v| {
                e.u32(*v);
            });
        let v = e.into_vec();
        let mut d = Dec::new(&v);
        assert_eq!(d.u8().unwrap(), 7);
        assert!(d.bool().unwrap());
        assert_eq!(d.u16().unwrap(), 0xbeef);
        assert_eq!(d.u32().unwrap(), 0xdead_beef);
        assert_eq!(d.u64().unwrap(), u64::MAX - 1);
        assert_eq!(d.u128().unwrap(), 1 << 100);
        assert_eq!(d.bytes().unwrap(), b"hello");
        assert_eq!(d.str().unwrap(), "wörld");
        assert_eq!(d.raw(3).unwrap(), &[1, 2, 3]);
        assert_eq!(d.seq(4, |d| d.u32()).unwrap(), vec![10, 20, 30]);
        d.finish().unwrap();
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let mut e = Enc::new();
        e.u64(1).bytes(b"abcdef");
        let v = e.into_vec();
        for cut in 0..v.len() {
            let mut d = Dec::new(&v[..cut]);
            let r = d.u64().and_then(|_| d.bytes().map(|_| ()));
            assert!(r.is_err(), "cut at {cut} decoded");
        }
    }

    #[test]
    fn hostile_lengths_are_refused_before_allocating() {
        let mut e = Enc::new();
        e.u32(u32::MAX);
        let v = e.into_vec();
        assert!(matches!(Dec::new(&v).bytes(), Err(DecodeError::TooLong(_))));
        assert!(matches!(
            Dec::new(&v).seq(8, |d| d.u64()),
            Err(DecodeError::TooLong(_))
        ));
    }

    #[test]
    fn bad_bool_and_utf8() {
        assert!(Dec::new(&[2]).bool().is_err());
        let mut e = Enc::new();
        e.bytes(&[0xff, 0xfe]);
        assert_eq!(Dec::new(e.as_slice()).str(), Err(DecodeError::Utf8));
    }

    #[test]
    fn crc32c_known_value() {
        // RFC 3720 test vector: 32 bytes of zeros.
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        let a = crc32c(b"hello ");
        assert_eq!(crc32c_append(a, b"world"), crc32c(b"hello world"));
    }
}
