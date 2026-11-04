//! How buckets and objects are kept (14-s3.md §14.6): rows and columns
//! of a bucket's table, the records in them, and the names of chunk
//! objects. Each record begins with a format byte; later fields are
//! appended, and a reader ignores those it doesn't know.

use hyperstor_codec::{Dec, DecodeError, Enc};

/// The object namespace of chunks (08-engine.md §7.2).
pub const NS_S3: u8 = 5;

/// An object's row: `OBJECT` + its name.
pub const OBJECT: u8 = b'o';
/// An upload's row: `UPLOAD` + when it expires + its id.
pub const UPLOAD: u8 = b'u';
/// An object row's latest version: its [`Entry`].
pub const LATEST: &[u8] = b"l";
/// An object row's uploads under way: `PENDING` + upload id.
pub const PENDING: u8 = b'p';
/// An upload row's column: the object's name.
pub const NAME: &[u8] = b"k";

const V1: u8 = 1;

pub fn object_row(key: &[u8]) -> Vec<u8> {
    [&[OBJECT][..], key].concat()
}

pub fn upload_row(expires: u64, id: u128) -> Vec<u8> {
    let mut r = vec![UPLOAD];
    r.extend_from_slice(&expires.to_be_bytes());
    r.extend_from_slice(&id.to_be_bytes());
    r
}

/// The expiry and id an upload row names.
pub fn upload_of(row: &[u8]) -> Option<(u64, u128)> {
    let (&UPLOAD, rest) = row.split_first()? else {
        return None;
    };
    let (expires, id) = rest.split_at_checked(8)?;
    Some((
        u64::from_be_bytes(expires.try_into().ok()?),
        u128::from_be_bytes(id.try_into().ok()?),
    ))
}

pub fn pending_column(id: u128) -> Vec<u8> {
    let mut c = vec![PENDING];
    c.extend_from_slice(&id.to_be_bytes());
    c
}

/// The first 28 bytes of a chunk's name: its part's chunks are this
/// followed by their index, a series (25-kv-ranges.md §28.6).
pub fn chunk_prefix(bucket: u64, object: u128, part: u32) -> Vec<u8> {
    let mut n = Vec::with_capacity(32);
    n.extend_from_slice(&bucket.to_be_bytes());
    n.extend_from_slice(&object.to_be_bytes());
    n.extend_from_slice(&part.to_be_bytes());
    n
}

pub fn chunk_name(bucket: u64, object: u128, part: u32, chunk: u32) -> Vec<u8> {
    let mut n = chunk_prefix(bucket, object, part);
    n.extend_from_slice(&chunk.to_be_bytes());
    n
}

/// Chunks of `chunk` bytes that `size` bytes take.
pub fn chunks_of(size: u64, chunk: u64) -> u32 {
    size.div_ceil(chunk) as u32
}

/// A bucket's own record, kept in its table's record (D197).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketInfo {
    pub tenant: u32,
    /// Unix milliseconds.
    pub created: u64,
}

impl BucketInfo {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(V1).u32(self.tenant).u64(self.created);
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<BucketInfo, DecodeError> {
        let mut d = Dec::new(b);
        version(&mut d)?;
        Ok(BucketInfo {
            tenant: d.u32()?,
            created: d.u64()?,
        })
    }
}

/// One part of an object's data, as the index entry has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub number: u32,
    pub size: u64,
}

/// Where an object's bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    /// In the entry: 64 KiB at most.
    Inline(Vec<u8>),
    /// In chunk objects of the bucket's data pool, `chunk` bytes each, the
    /// parts in order.
    Chunks {
        object: u128,
        chunk: u64,
        parts: Vec<Part>,
    },
}

/// An object's index entry (14-s3.md §14.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Unix milliseconds.
    pub written: u64,
    pub size: u64,
    pub md5: [u8; 16],
    /// Parts of a multipart upload, for its ETag; 0 for a single PUT.
    pub parts: u32,
    /// CRC32C of the whole body.
    pub crc32c: u32,
    pub content_type: String,
    pub meta: Vec<(String, String)>,
    pub data: Data,
}

impl Entry {
    /// The ETag, quoted, as S3 shows it (D196).
    pub fn etag(&self) -> String {
        let hex: String = self.md5.iter().map(|b| format!("{b:02x}")).collect();
        if self.parts == 0 {
            format!("\"{hex}\"")
        } else {
            format!("\"{hex}-{}\"", self.parts)
        }
    }

    /// Its chunks, as series: (prefix, count) for each part.
    pub fn series(&self, bucket: u64) -> Vec<(Vec<u8>, u32)> {
        match &self.data {
            Data::Inline(_) => Vec::new(),
            Data::Chunks {
                object,
                chunk,
                parts,
            } => parts
                .iter()
                .map(|p| {
                    (
                        chunk_prefix(bucket, *object, p.number),
                        chunks_of(p.size, *chunk),
                    )
                })
                .collect(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(V1)
            .u64(self.written)
            .u64(self.size)
            .raw(&self.md5)
            .u32(self.parts)
            .u32(self.crc32c)
            .str(&self.content_type)
            .seq(&self.meta, |e, (k, v)| {
                e.str(k).str(v);
            });
        match &self.data {
            Data::Inline(b) => {
                e.u8(0).bytes(b);
            }
            Data::Chunks {
                object,
                chunk,
                parts,
            } => {
                e.u8(1).u128(*object).u64(*chunk).seq(parts, |e, p| {
                    e.u32(p.number).u64(p.size);
                });
            }
        }
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<Entry, DecodeError> {
        let mut d = Dec::new(b);
        version(&mut d)?;
        let written = d.u64()?;
        let size = d.u64()?;
        let md5 = d.raw(16)?.try_into().expect("16 bytes");
        let parts = d.u32()?;
        let crc32c = d.u32()?;
        let content_type = d.str()?.to_string();
        let meta = d.seq(8, |d| Ok((d.str()?.to_string(), d.str()?.to_string())))?;
        let data = match d.u8()? {
            0 => Data::Inline(d.bytes()?.to_vec()),
            1 => Data::Chunks {
                object: d.u128()?,
                chunk: d.u64()?,
                parts: d.seq(12, |d| {
                    Ok(Part {
                        number: d.u32()?,
                        size: d.u64()?,
                    })
                })?,
            },
            t => {
                return Err(DecodeError::Invalid {
                    field: "data kind",
                    value: t.into(),
                });
            }
        };
        Ok(Entry {
            written,
            size,
            md5,
            parts,
            crc32c,
            content_type,
            meta,
            data,
        })
    }
}

/// An upload under way, as its object row's `PENDING` column has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Unix seconds.
    pub expires: u64,
    /// Bytes promised, for a single PUT.
    pub size: u64,
    pub chunk: u64,
}

impl Pending {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(V1).u64(self.expires).u64(self.size).u64(self.chunk);
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<Pending, DecodeError> {
        let mut d = Dec::new(b);
        version(&mut d)?;
        Ok(Pending {
            expires: d.u64()?,
            size: d.u64()?,
            chunk: d.u64()?,
        })
    }
}

fn version(d: &mut Dec) -> Result<(), DecodeError> {
    match d.u8()? {
        V1 => Ok(()),
        v => Err(DecodeError::Invalid {
            field: "format",
            value: v.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_decode_as_encoded() {
        let mut e = Entry {
            written: 1_700_000_000_123,
            size: 9 << 20,
            md5: [7; 16],
            parts: 0,
            crc32c: 0xdead_beef,
            content_type: "application/octet-stream".into(),
            meta: vec![("a".into(), "b".into()), ("c".into(), String::new())],
            data: Data::Chunks {
                object: u128::MAX - 3,
                chunk: 4 << 20,
                parts: vec![Part {
                    number: 0,
                    size: 9 << 20,
                }],
            },
        };
        assert_eq!(Entry::decode(&e.encode()), Ok(e.clone()));
        assert_eq!(e.series(5), [(chunk_prefix(5, u128::MAX - 3, 0), 3)]);
        e.data = Data::Inline(b"x".to_vec());
        e.parts = 2;
        assert_eq!(Entry::decode(&e.encode()), Ok(e.clone()));
        assert_eq!(e.etag(), format!("\"{}-2\"", "07".repeat(16)));
        let p = Pending {
            expires: 9,
            size: 10,
            chunk: 11,
        };
        assert_eq!(Pending::decode(&p.encode()), Ok(p));
        let b = BucketInfo {
            tenant: 3,
            created: 4,
        };
        assert_eq!(BucketInfo::decode(&b.encode()), Ok(b));
        assert_eq!(upload_of(&upload_row(5, 6)), Some((5, 6)));
        assert_eq!(upload_of(&object_row(b"x")), None);
    }

    #[test]
    fn uploads_sort_by_expiry_and_chunks_by_index() {
        assert!(upload_row(1, u128::MAX) < upload_row(2, 0));
        assert!(chunk_name(1, 2, 3, 255) < chunk_name(1, 2, 3, 256));
        assert_eq!(chunks_of(0, 4), 0);
        assert_eq!(chunks_of(4, 4), 1);
        assert_eq!(chunks_of(5, 4), 2);
    }
}
