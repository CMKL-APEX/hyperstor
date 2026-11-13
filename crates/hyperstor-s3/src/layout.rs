//! How buckets and objects are kept (14-s3.md §14.6): rows and columns
//! of a bucket's table, the records in them, and the names of chunk
//! objects. Each record begins with a format byte; later fields are
//! appended, and a reader ignores those it doesn't know.

use crate::chunked::Algorithm;
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
/// An upload row's column for a multipart upload: when it began (Unix
/// milliseconds, big-endian).
pub const MULTIPART: &[u8] = b"m";

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

/// A multipart upload's part, as a column of its object's row: the
/// upload's column, then the part number and the part's own object id,
/// which names its chunks, so a part uploaded again never writes over
/// the chunks of the one before (I291).
pub fn part_column(id: u128, number: u32, object: u128) -> Vec<u8> {
    let mut c = pending_column(id);
    c.extend_from_slice(&number.to_be_bytes());
    c.extend_from_slice(&object.to_be_bytes());
    c
}

/// The part number and object id a column of upload `id` names, if it
/// is one of its parts.
pub fn part_of(column: &[u8], id: u128) -> Option<(u32, u128)> {
    let rest = column.strip_prefix(pending_column(id).as_slice())?;
    if rest.len() != 20 {
        return None;
    }
    let (n, o) = rest.split_at(4);
    Some((
        u32::from_be_bytes(n.try_into().ok()?),
        u128::from_be_bytes(o.try_into().ok()?),
    ))
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

/// A bucket's table: its tenant, big-endian, then its name (D243), so
/// each tenant has its own names and its buckets sort together.
pub fn bucket_table(tenant: u32, name: &str) -> Vec<u8> {
    [&tenant.to_be_bytes()[..], name.as_bytes()].concat()
}

/// The tenant and name of a bucket's table.
pub fn bucket_of(table: &[u8]) -> Option<(u32, &[u8])> {
    let (t, name) = table.split_at_checked(4)?;
    Some((u32::from_be_bytes(t.try_into().ok()?), name))
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
    /// Whether every write since it was made kept its counts (§14.6):
    /// buckets made by an earlier build have none.
    pub counted: bool,
    /// Its project (20-tenants.md §24.9, D300); 0: its tenant's default
    /// project, as for a bucket made before projects.
    pub project: u32,
}

impl BucketInfo {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(V1)
            .u32(self.tenant)
            .u64(self.created)
            .bool(self.counted)
            .u32(self.project);
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<BucketInfo, DecodeError> {
        let mut d = Dec::new(b);
        version(&mut d)?;
        Ok(BucketInfo {
            tenant: d.u32()?,
            created: d.u64()?,
            counted: d.remaining() > 0 && d.bool()?,
            project: if d.remaining() > 0 { d.u32()? } else { 0 },
        })
    }
}

/// One part of an object's data, as the index entry has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub number: u32,
    pub size: u64,
}

/// A multipart object's part: its own object id, which with its number
/// names its chunks, and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartAt {
    pub object: u128,
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
    /// A multipart upload's parts, in order, each with its own object id
    /// (stage 4).
    Parts { chunk: u64, parts: Vec<PartAt> },
}

impl Data {
    /// The chunk size and each piece in order: (object id, part number,
    /// size). None for inline bytes.
    pub fn pieces(&self) -> Option<(u64, Vec<PartAt>)> {
        match self {
            Data::Inline(_) => None,
            Data::Chunks {
                object,
                chunk,
                parts,
            } => Some((
                *chunk,
                parts
                    .iter()
                    .map(|p| PartAt {
                        object: *object,
                        number: p.number,
                        size: p.size,
                    })
                    .collect(),
            )),
            Data::Parts { chunk, parts } => Some((*chunk, parts.clone())),
        }
    }
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
    /// The `x-amz-checksum-*` value the object was checked against when
    /// written, to give back (appended to the record in stage 4).
    pub checksum: Option<(Algorithm, Vec<u8>)>,
    /// The checksum is of the parts' checksums (S3's `COMPOSITE`), shown
    /// with `-` and the number of parts; else of the whole object.
    pub composite: bool,
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
        let Some((chunk, pieces)) = self.data.pieces() else {
            return Vec::new();
        };
        pieces
            .iter()
            .map(|p| {
                (
                    chunk_prefix(bucket, p.object, p.number),
                    chunks_of(p.size, chunk),
                )
            })
            .collect()
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
            Data::Parts { chunk, parts } => {
                e.u8(2).u64(*chunk).seq(parts, |e, p| {
                    e.u128(p.object).u32(p.number).u64(p.size);
                });
            }
        }
        // The high bit: composite (S3's COMPOSITE checksum type).
        match &self.checksum {
            None => e.u8(0),
            Some((a, v)) => e
                .u8(a.code() | if self.composite { COMPOSITE } else { 0 })
                .bytes(v),
        };
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
            2 => Data::Parts {
                chunk: d.u64()?,
                parts: d.seq(28, |d| {
                    Ok(PartAt {
                        object: d.u128()?,
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
        // Written before stage 4: no checksum.
        let code = if d.remaining() > 0 { d.u8()? } else { 0 };
        let composite = code & COMPOSITE != 0;
        let checksum = match code & !COMPOSITE {
            0 => None,
            c => Some((algorithm(c)?, d.bytes()?.to_vec())),
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
            checksum,
            composite,
        })
    }
}

/// An entry's checksum algorithm byte's high bit: a composite checksum.
const COMPOSITE: u8 = 0x80;

fn algorithm(c: u8) -> Result<Algorithm, DecodeError> {
    Algorithm::of_code(c).ok_or(DecodeError::Invalid {
        field: "checksum algorithm",
        value: c.into(),
    })
}

fn checksum_of(d: &mut Dec) -> Result<Option<(Algorithm, Vec<u8>)>, DecodeError> {
    Ok(match d.u8()? {
        0 => None,
        c => Some((algorithm(c)?, d.bytes()?.to_vec())),
    })
}

fn put_checksum(e: &mut Enc, c: &Option<(Algorithm, Vec<u8>)>) {
    match c {
        None => e.u8(0),
        Some((a, v)) => e.u8(a.code()).bytes(v),
    };
}

/// What a multipart upload was begun with, kept in its pending record
/// until it is completed (stage 4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Multipart {
    /// Unix milliseconds.
    pub initiated: u64,
    pub content_type: String,
    pub meta: Vec<(String, String)>,
    /// The checksum every part comes with, if the upload asked for one.
    pub algorithm: Option<Algorithm>,
    /// The completed object's checksum is of the whole object (CRCs
    /// only), not of the parts' checksums.
    pub full_object: bool,
}

/// A part of a multipart upload, as its column has it: being written,
/// or done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRecord {
    pub done: bool,
    /// Bytes promised, then written.
    pub size: u64,
    pub chunk: u64,
    /// Unix milliseconds, when done.
    pub written: u64,
    pub md5: [u8; 16],
    pub crc32c: u32,
    pub checksum: Option<(Algorithm, Vec<u8>)>,
}

impl PartRecord {
    /// Its ETag, quoted.
    pub fn etag(&self) -> String {
        let hex: String = self.md5.iter().map(|b| format!("{b:02x}")).collect();
        format!("\"{hex}\"")
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(V1)
            .bool(self.done)
            .u64(self.size)
            .u64(self.chunk)
            .u64(self.written)
            .raw(&self.md5)
            .u32(self.crc32c);
        put_checksum(&mut e, &self.checksum);
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<PartRecord, DecodeError> {
        let mut d = Dec::new(b);
        version(&mut d)?;
        Ok(PartRecord {
            done: d.bool()?,
            size: d.u64()?,
            chunk: d.u64()?,
            written: d.u64()?,
            md5: d.raw(16)?.try_into().expect("16 bytes"),
            crc32c: d.u32()?,
            checksum: checksum_of(&mut d)?,
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
    /// A multipart upload's settings (appended in stage 4); none for a
    /// single PUT, whose chunks are part 0 of the upload's id.
    pub multipart: Option<Multipart>,
}

impl Pending {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u8(V1).u64(self.expires).u64(self.size).u64(self.chunk);
        if let Some(m) = &self.multipart {
            e.u8(1)
                .u64(m.initiated)
                .str(&m.content_type)
                .seq(&m.meta, |e, (k, v)| {
                    e.str(k).str(v);
                })
                .u8(m.algorithm.map_or(0, Algorithm::code))
                .bool(m.full_object);
        }
        e.into_vec()
    }

    pub fn decode(b: &[u8]) -> Result<Pending, DecodeError> {
        let mut d = Dec::new(b);
        version(&mut d)?;
        let (expires, size, chunk) = (d.u64()?, d.u64()?, d.u64()?);
        let multipart = match if d.remaining() > 0 { d.u8()? } else { 0 } {
            0 => None,
            _ => Some(Multipart {
                initiated: d.u64()?,
                content_type: d.str()?.to_string(),
                meta: d.seq(8, |d| Ok((d.str()?.to_string(), d.str()?.to_string())))?,
                algorithm: match d.u8()? {
                    0 => None,
                    c => Some(algorithm(c)?),
                },
                full_object: d.bool()?,
            }),
        };
        Ok(Pending {
            expires,
            size,
            chunk,
            multipart,
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
            checksum: None,
            composite: false,
        };
        assert_eq!(Entry::decode(&e.encode()), Ok(e.clone()));
        // An entry written before checksums were kept reads as none.
        let old = e.encode();
        assert_eq!(Entry::decode(&old[..old.len() - 1]), Ok(e.clone()));
        e.checksum = Some((Algorithm::Sha256, vec![9; 32]));
        assert_eq!(Entry::decode(&e.encode()), Ok(e.clone()));
        assert_eq!(e.series(5), [(chunk_prefix(5, u128::MAX - 3, 0), 3)]);
        e.data = Data::Inline(b"x".to_vec());
        e.parts = 2;
        assert_eq!(Entry::decode(&e.encode()), Ok(e.clone()));
        assert_eq!(e.etag(), format!("\"{}-2\"", "07".repeat(16)));
        let mut p = Pending {
            expires: 9,
            size: 10,
            chunk: 11,
            multipart: None,
        };
        assert_eq!(Pending::decode(&p.encode()), Ok(p.clone()));
        p.multipart = Some(Multipart {
            initiated: 12,
            content_type: "a/b".into(),
            meta: vec![("k".into(), "v".into())],
            algorithm: Some(Algorithm::Crc32),
            full_object: true,
        });
        assert_eq!(Pending::decode(&p.encode()), Ok(p));
        let mut part = PartRecord {
            done: false,
            size: 5,
            chunk: 6,
            written: 7,
            md5: [8; 16],
            crc32c: 9,
            checksum: None,
        };
        assert_eq!(PartRecord::decode(&part.encode()), Ok(part.clone()));
        part.checksum = Some((Algorithm::Crc32c, vec![1, 2, 3, 4]));
        assert_eq!(PartRecord::decode(&part.encode()), Ok(part));
        e.data = Data::Parts {
            chunk: 4,
            parts: vec![
                PartAt {
                    object: 1,
                    number: 1,
                    size: 9,
                },
                PartAt {
                    object: 2,
                    number: 3,
                    size: 0,
                },
            ],
        };
        e.composite = true;
        assert_eq!(Entry::decode(&e.encode()), Ok(e.clone()));
        assert_eq!(
            e.series(5),
            [(chunk_prefix(5, 1, 1), 3), (chunk_prefix(5, 2, 3), 0)]
        );
        assert_eq!(part_of(&part_column(7, 3, 9), 7), Some((3, 9)));
        assert_eq!(part_of(&part_column(7, 3, 9), 8), None);
        assert_eq!(part_of(&pending_column(7), 7), None);
        let b = BucketInfo {
            tenant: 3,
            created: 4,
            counted: true,
            project: 9,
        };
        assert_eq!(BucketInfo::decode(&b.encode()), Ok(b.clone()));
        // A record from before projects: project 0 (D300)...
        assert_eq!(
            BucketInfo::decode(&b.encode()[..14]),
            Ok(BucketInfo {
                project: 0,
                ..b.clone()
            })
        );
        // ...and from before counts: not counted.
        let old = &b.encode()[..13];
        let uncounted = BucketInfo {
            counted: false,
            project: 0,
            ..b
        };
        assert_eq!(BucketInfo::decode(old), Ok(uncounted));
        assert_eq!(upload_of(&upload_row(5, 6)), Some((5, 6)));
        assert_eq!(upload_of(&object_row(b"x")), None);
        assert_eq!(bucket_of(&bucket_table(258, "ab")), Some((258, &b"ab"[..])));
        assert_eq!(bucket_of(b"abc"), None);
    }

    #[test]
    fn uploads_sort_by_expiry_and_chunks_by_index() {
        assert!(upload_row(1, u128::MAX) < upload_row(2, 0));
        assert!(chunk_name(1, 2, 3, 255) < chunk_name(1, 2, 3, 256));
        // A tenant's buckets sort together, before the next tenant's.
        assert!(bucket_table(1, "zz") < bucket_table(2, "aa"));
        assert_eq!(chunks_of(0, 4), 0);
        assert_eq!(chunks_of(4, 4), 1);
        assert_eq!(chunks_of(5, 4), 2);
    }
}
