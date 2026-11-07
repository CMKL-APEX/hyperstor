//! Hyperstor's S3 gateway (14-s3.md §14). This is its object layer,
//! stage 1 of §14.7: buckets as tables of `hyperstor-kv`'s store 1, each
//! object an index entry in its bucket's table and its bytes inline or
//! in `s3` chunk objects of the bucket's data pool (§14.6). It runs on the
//! client library's reactor; the HTTP side (stage 2) hands it requests.
//!
//! Every write goes to new chunks, then one batch on the object's row
//! makes it visible and queues what it replaced for deletion (D192).
//! Uploads are recorded before their chunks are written and expire, so
//! a gateway that stops in the middle leaves nothing that isn't deleted
//! in the end ([`S3::clean`]).

pub mod chunked;
pub mod dates;
pub mod gateway;
pub mod keys;
pub mod layout;
pub mod sigv4;
pub mod xml;

use hyperstor_client::{Client, ClientError};
use hyperstor_kv::{Batch, KvError, S3_STORE, Scan, Store, Table};
use hyperstor_proto::{Code, WireOp};
use hyperstor_reactor::JoinHandle;
use layout::*;
use md5::{Digest, Md5};
use std::rc::Rc;

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("no bucket {0:?}")]
    NoSuchBucket(String),
    /// The tenant has a bucket of that name already; another tenant's
    /// never clash (D243, D244).
    #[error("you own bucket {0:?} already")]
    BucketExists(String),
    #[error("bucket {0:?} isn't empty")]
    BucketNotEmpty(String),
    #[error("{0:?} isn't a valid bucket name")]
    InvalidBucketName(String),
    #[error("object names are 1 to 1024 bytes")]
    InvalidKey,
    #[error("{got} bytes sent, {want} promised")]
    BadSize { want: u64, got: u64 },
    #[error("the range is outside the object")]
    InvalidRange,
    #[error("the upload expired or was given up")]
    UploadGone,
    #[error("no such multipart upload")]
    NoSuchUpload,
    /// A part a completion names wasn't uploaded, or its ETag differs.
    #[error("part {0} wasn't uploaded as named")]
    InvalidPart(u32),
    #[error("the parts aren't in ascending order")]
    InvalidPartOrder,
    /// A part but the last is smaller than the least a part may be.
    #[error("part {0} is smaller than the least a part may be")]
    EntityTooSmall(u32),
    /// The upload asked for a checksum that a part didn't come with.
    #[error("part {0} has no checksum of the upload's algorithm")]
    MissingPartChecksum(u32),
    /// The checksum a completion gave for the whole object differs.
    #[error("the checksum given doesn't match the parts'")]
    BadDigest,
    #[error("the object was replaced while it was read")]
    Changed,
    /// A condition on the object a write or delete replaces doesn't hold
    /// (D194).
    #[error("the object isn't as the request's condition says")]
    PreconditionFailed,
    #[error("bad record: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Kv(#[from] KvError),
    #[error(transparent)]
    Client(#[from] ClientError),
}

impl From<hyperstor_codec::DecodeError> for S3Error {
    fn from(e: hyperstor_codec::DecodeError) -> Self {
        S3Error::Corrupt(e.to_string())
    }
}

type Result<T> = std::result::Result<T, S3Error>;

/// Sizes and times (14-s3.md §14.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Objects up to this size are kept in their index entry.
    pub inline: u64,
    /// Bytes per chunk object.
    pub chunk: u64,
    /// Seconds a PUT has before it expires.
    pub put_expiry: u64,
    /// Seconds before its expiry when an upload stops taking writes, so
    /// gateways' clocks may differ by as much.
    pub margin: u64,
    /// Seconds a replaced or deleted object's chunks are kept, so that a
    /// GET that found it before can finish.
    pub linger: u64,
    /// Chunk writes a PUT keeps in flight.
    pub in_flight: usize,
    /// Seconds a multipart upload has before it expires (§14.6).
    pub multipart_expiry: u64,
    /// The least a multipart upload's part may be, but its last (S3's
    /// 5 MiB).
    pub min_part: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            inline: 64 << 10,
            chunk: 4 << 20,
            put_expiry: 24 * 3600,
            margin: 3600,
            linger: 900,
            in_flight: 4,
            multipart_expiry: 7 * 24 * 3600,
            min_part: 5 << 20,
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// S3's most parts in a multipart upload, numbered from 1.
pub const MAX_PARTS: u32 = 10_000;

/// A multipart upload's completed object: the MD5 of its parts' MD5s,
/// and its whole CRC32C, combined from its parts' (D196).
fn combine(parts: &[&PartRecord]) -> ([u8; 16], u32) {
    let mut md5 = Md5::new();
    let mut crc = 0u32;
    for p in parts {
        md5.update(p.md5);
        crc = crc32c::crc32c_combine(crc, p.crc32c, p.size as usize);
    }
    (md5.finalize().into(), crc)
}

/// The completed object's checksum from its parts' (each of `a`): of the
/// whole object for a CRC when `full`, else of the parts' checksums, one
/// after another (S3's COMPOSITE).
fn combined_checksum(a: chunked::Algorithm, full: bool, parts: &[&PartRecord]) -> Result<Vec<u8>> {
    use chunked::Algorithm::*;
    let mut sums = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        match &p.checksum {
            Some((pa, v)) if *pa == a => sums.push((v.clone(), p.size)),
            _ => return Err(S3Error::MissingPartChecksum(i as u32 + 1)),
        }
    }
    let be = |v: &[u8]| -> Result<u32> {
        v.try_into()
            .map(u32::from_be_bytes)
            .map_err(|_| S3Error::Corrupt("a part's CRC".into()))
    };
    Ok(match (a, full) {
        (Crc32c, true) => {
            let mut crc = 0;
            for (v, size) in &sums {
                crc = crc32c::crc32c_combine(crc, be(v)?, *size as usize);
            }
            crc.to_be_bytes().to_vec()
        }
        (Crc32, true) => {
            let mut h = crc32fast::Hasher::new();
            for (v, size) in &sums {
                h.combine(&crc32fast::Hasher::new_with_initial_len(be(v)?, *size));
            }
            h.finalize().to_be_bytes().to_vec()
        }
        _ => {
            let mut h = chunked::Hasher::new(a);
            for (v, _) in &sums {
                h.update(v);
            }
            h.finish()
        }
    })
}

fn random_u128() -> u128 {
    (u128::from(hyperstor_client::random_u64()) << 64) | u128::from(hyperstor_client::random_u64())
}

/// S3's rules for bucket names (general purpose buckets): 3 to 63
/// lowercase letters, digits, dots and hyphens, beginning and ending with
/// a letter or digit, no two dots in a row, not an IPv4 address.
pub fn valid_bucket_name(name: &str) -> bool {
    let b = name.as_bytes();
    let alnum = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (3..=63).contains(&b.len())
        && b.iter().all(|c| alnum(c) || *c == b'.' || *c == b'-')
        && b.first().is_some_and(alnum)
        && b.last().is_some_and(alnum)
        && !name.contains("..")
        && name.parse::<std::net::Ipv4Addr>().is_err()
}

/// A bucket: its table and its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// Its name within its tenant's (D243).
    pub name: String,
    pub table: Table,
    pub info: BucketInfo,
}

impl Bucket {
    fn of(table: Table) -> Result<Bucket> {
        let corrupt = || S3Error::Corrupt("a bucket's table name".into());
        let (tenant, name) = bucket_of(&table.name).ok_or_else(corrupt)?;
        let info = BucketInfo::decode(&table.info)?;
        if info.tenant != tenant {
            return Err(corrupt());
        }
        Ok(Bucket {
            name: String::from_utf8(name.to_vec()).map_err(|_| corrupt())?,
            info,
            table,
        })
    }

    pub fn id(&self) -> u64 {
        self.table.id
    }
}

/// What a PUT sets beside the bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub content_type: String,
    pub meta: Vec<(String, String)>,
    /// The `x-amz-checksum-*` value the body was checked against.
    pub checksum: Option<(chunked::Algorithm, Vec<u8>)>,
}

/// What a write or delete requires of the object it replaces (D194):
/// checked against the entry read, which the batch then asserts, so the
/// object can't change between the check and the write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Condition {
    #[default]
    Any,
    /// No object of that name (`If-None-Match: *`).
    Absent,
    /// An object of that name (`If-Match: *`).
    Exists,
    /// An object with one of these ETags, unquoted (`If-Match`).
    Matches(Vec<String>),
}

impl Condition {
    pub fn holds(&self, e: Option<&Entry>) -> bool {
        match (self, e) {
            (Condition::Any, _) => true,
            (Condition::Absent, e) => e.is_none(),
            (Condition::Exists, e) => e.is_some(),
            (Condition::Matches(tags), Some(e)) => {
                let tag = e.etag();
                tags.iter().any(|t| t == tag.trim_matches('"'))
            }
            (Condition::Matches(_), None) => false,
        }
    }
}

/// Which objects a [`S3::list`] returns (ListObjectsV2).
#[derive(Debug, Clone, Copy, Default)]
pub struct List<'a> {
    /// Only names that begin with this.
    pub prefix: &'a [u8],
    /// Names with this after the prefix are rolled up into one common
    /// prefix, up to and including it; empty: none are.
    pub delimiter: &'a [u8],
    /// The first name to look at: a page's `next`, or StartAfter + 0x00.
    pub start: &'a [u8],
    /// Objects and common prefixes together, at least 1.
    pub max: usize,
}

/// A page of a listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub objects: Vec<(Vec<u8>, Entry)>,
    pub prefixes: Vec<Vec<u8>>,
    /// Where the next page starts, if there is one.
    pub next: Option<Vec<u8>>,
}

/// The first name past every name beginning with `p`; none: there is none.
pub fn past_prefix(p: &[u8]) -> Option<Vec<u8>> {
    let mut p = p.to_vec();
    while let Some(last) = p.pop() {
        if last < 0xFF {
            p.push(last + 1);
            return Some(p);
        }
    }
    None
}

fn kv_conflict(r: &std::result::Result<(), KvError>) -> bool {
    matches!(r, Err(KvError::Conflict))
}

fn not_found(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Server {
            code: Code::NotFound,
            ..
        }
    )
}

/// Assertions keep failing: someone else keeps changing the row.
const ATTEMPTS: usize = 100;

/// The object layer, on one client's reactor.
pub struct S3 {
    kv: Store,
    cfg: Config,
}

impl S3 {
    pub fn open(client: Rc<Client>, cfg: Config) -> S3 {
        S3 {
            kv: Store::open(client, S3_STORE),
            cfg,
        }
    }

    pub fn client(&self) -> &Rc<Client> {
        self.kv.client()
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Makes bucket `name` for `tenant`, its index in `index_pool` and its
    /// data in `data_pool` (D197, D198). Names are per tenant (D243).
    pub async fn create_bucket(
        &self,
        name: &str,
        tenant: u32,
        index_pool: u32,
        data_pool: u32,
    ) -> Result<Bucket> {
        if !valid_bucket_name(name) {
            return Err(S3Error::InvalidBucketName(name.into()));
        }
        let info = BucketInfo {
            tenant,
            created: now_ms(),
        };
        let made = self
            .kv
            .create_table_with(
                &bucket_table(tenant, name),
                index_pool,
                data_pool,
                &info.encode(),
            )
            .await;
        match made {
            Ok(t) => Bucket::of(t),
            Err(KvError::TableExists(_)) => Err(S3Error::BucketExists(name.into())),
            Err(e) => Err(e.into()),
        }
    }

    /// `tenant`'s bucket `name`: another tenant's of the same name is
    /// none of its business (D244).
    pub async fn bucket(&self, tenant: u32, name: &str) -> Result<Bucket> {
        match self.kv.table(&bucket_table(tenant, name)).await? {
            Some(t) => Bucket::of(t),
            None => Err(S3Error::NoSuchBucket(name.into())),
        }
    }

    /// Every bucket of `tenant`, by name: its tables, which sort together.
    pub async fn buckets(&self, tenant: u32) -> Result<Vec<Bucket>> {
        let prefix = tenant.to_be_bytes();
        let mut out = Vec::new();
        let mut from = prefix.to_vec();
        loop {
            let page = self.kv.tables(&from, 1000).await?;
            let Some(last) = page.last() else {
                return Ok(out);
            };
            from = [&last.name[..], &[0]].concat();
            for t in page {
                if !t.name.starts_with(&prefix) {
                    return Ok(out);
                }
                out.push(Bucket::of(t)?);
            }
        }
    }

    /// Every tenant's buckets, by tenant and name.
    pub async fn all_buckets(&self) -> Result<Vec<Bucket>> {
        let mut out = Vec::new();
        let mut from = Vec::new();
        loop {
            let page = self.kv.tables(&from, 1000).await?;
            let Some(last) = page.last() else {
                return Ok(out);
            };
            from = [&last.name[..], &[0]].concat();
            for t in page {
                out.push(Bucket::of(t)?);
            }
        }
    }

    /// Deletes bucket `name`, which must hold no object and no upload under
    /// way. Uploads that expired are cleaned up first.
    pub async fn delete_bucket(&self, tenant: u32, name: &str) -> Result<()> {
        let b = self.bucket(tenant, name).await?;
        self.clean(&b, now_secs()).await?;
        let any = Scan {
            limit: 1,
            ..Default::default()
        };
        if !self.kv.scan(&b.table, &any).await?.rows.is_empty() {
            return Err(S3Error::BucketNotEmpty(name.into()));
        }
        match self.kv.delete_table(&b.table.name).await {
            Err(KvError::NoTable(_)) => Err(S3Error::NoSuchBucket(name.into())),
            r => Ok(r?),
        }
    }

    /// Object `key`'s index entry, if it exists (HEAD).
    pub async fn head(&self, b: &Bucket, key: &[u8]) -> Result<Option<Entry>> {
        Ok(self.latest(b, key).await?.map(|(_, e)| e))
    }

    async fn latest(&self, b: &Bucket, key: &[u8]) -> Result<Option<(Vec<u8>, Entry)>> {
        let v = self.kv.get(&b.table, &object_row(key), LATEST).await?;
        Ok(match v {
            Some(v) => {
                let e = Entry::decode(&v)?;
                Some((v, e))
            }
            None => None,
        })
    }

    /// `len` bytes of object `e` from `off` (GET with a range). The chunks
    /// are read at once. [`S3Error::Changed`] if they were deleted: the
    /// object was replaced or deleted after `e` was read.
    pub async fn read(&self, b: &Bucket, e: &Entry, off: u64, len: u64) -> Result<Vec<u8>> {
        if off.checked_add(len).is_none_or(|end| end > e.size) {
            return Err(S3Error::InvalidRange);
        }
        let (chunk, parts) = match &e.data {
            Data::Inline(d) => return Ok(d[off as usize..(off + len) as usize].to_vec()),
            data => data.pieces().expect("chunks"),
        };
        // (chunk name, offset in it, bytes) for each piece of the range.
        let mut pieces = Vec::new();
        let (mut at, end) = (off, off + len);
        let mut part_start = 0;
        for p in &parts {
            let part_end = part_start + p.size;
            while at < end && at < part_end {
                let in_part = at - part_start;
                let index = in_part / chunk;
                let in_chunk = in_part % chunk;
                let n = (chunk - in_chunk).min(end - at).min(part_end - at);
                let name = chunk_name(b.id(), p.object, p.number, index as u32);
                pieces.push((name, in_chunk, n));
                at += n;
            }
            part_start = part_end;
        }
        let pool = b.table.data_pool;
        let reads: Vec<_> = pieces
            .into_iter()
            .map(|(name, in_chunk, n)| {
                let cl = self.client().clone();
                hyperstor_reactor::spawn(async move {
                    let from = in_chunk / 4096 * 4096;
                    let to = (in_chunk + n).div_ceil(4096) * 4096;
                    let got = cl.read_object(pool, NS_S3, &name, from, to - from).await;
                    let skip = (in_chunk - from) as usize;
                    got.map(|d| d[skip..skip + n as usize].to_vec())
                })
            })
            .collect();
        let mut out = Vec::with_capacity(len as usize);
        for r in reads {
            match r.await {
                Ok(d) => out.extend_from_slice(&d),
                Err(e) if not_found(&e) => return Err(S3Error::Changed),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }

    /// Deletes object `key`, if it exists; its chunks are queued for
    /// deletion in the same batch (D192).
    pub async fn delete(&self, b: &Bucket, key: &[u8]) -> Result<()> {
        self.delete_if(b, key, &Condition::Any).await
    }

    /// Deletes object `key` if `cond` holds of it (D194).
    pub async fn delete_if(&self, b: &Bucket, key: &[u8], cond: &Condition) -> Result<()> {
        let row = object_row(key);
        for _ in 0..ATTEMPTS {
            let old = self.latest(b, key).await?;
            if !cond.holds(old.as_ref().map(|(_, e)| e)) {
                return Err(S3Error::PreconditionFailed);
            }
            let Some((v, e)) = old else {
                return Ok(());
            };
            let mut batch = Batch::new();
            batch.assert(LATEST, Some(&v)).delete(LATEST);
            batch.linger(self.cfg.linger);
            collect(&mut batch, b, &e);
            let r = self.kv.apply(&b.table, &row, &batch).await;
            if !kv_conflict(&r) {
                return Ok(r?);
            }
        }
        Err(KvError::Exhausted(ATTEMPTS, "the object kept changing".into()).into())
    }

    /// Begins a PUT of `size` bytes as object `key`. Larger than the inline
    /// limit, the upload is recorded first (14-s3.md §14.6).
    pub async fn put(&self, b: &Bucket, key: &[u8], size: u64) -> Result<Put<'_>> {
        if key.is_empty() || key.len() > 1024 {
            return Err(S3Error::InvalidKey);
        }
        let id = random_u128();
        let pending = if size > self.cfg.inline {
            let p = Pending {
                expires: now_secs() + self.cfg.put_expiry,
                size,
                chunk: self.cfg.chunk,
                multipart: None,
            };
            let mut up = Batch::new();
            up.put(NAME, key);
            self.kv
                .apply(&b.table, &upload_row(p.expires, id), &up)
                .await?;
            let mut mark = Batch::new();
            mark.put(&pending_column(id), &p.encode());
            self.kv.apply(&b.table, &object_row(key), &mark).await?;
            Some(p)
        } else {
            None
        };
        Ok(Put {
            s3: self,
            bucket: b.clone(),
            key: key.to_vec(),
            size,
            id,
            part: 0,
            chunk: self.cfg.chunk,
            upload: None,
            pending,
            buf: Vec::new(),
            sent: 0,
            chunks: 0,
            writing: Default::default(),
            md5: Md5::new(),
            crc: 0,
            cond: Condition::Any,
        })
    }

    /// Copies object `e` of bucket `from` to `key` of bucket `to`, if
    /// `cond` holds of the object there (D193, D194): its bytes are read
    /// and written again, as a PUT of them, so the copy shares nothing
    /// with the source. The checksum is `hash`'s, computed as the bytes
    /// go by, else the source's (the bytes are the same). A source
    /// replaced during the copy is still read whole while its chunks
    /// linger (§14.6); after that, [`S3Error::Changed`].
    #[allow(clippy::too_many_arguments)]
    pub async fn copy(
        &self,
        from: &Bucket,
        e: &Entry,
        to: &Bucket,
        key: &[u8],
        mut meta: Meta,
        cond: Condition,
        hash: Option<chunked::Algorithm>,
    ) -> Result<Entry> {
        let mut put = self.put(to, key, e.size).await?;
        put.require(cond);
        let sum = match self.pump(&mut put, from, e, 0, e.size, hash).await {
            Ok(s) => s,
            Err(err) => {
                let _ = put.abort().await;
                return Err(err);
            }
        };
        meta.checksum = match (hash, sum) {
            (Some(a), Some(v)) => Some((a, v)),
            _ if e.composite => None,
            _ => e.checksum.clone(),
        };
        put.finish(meta).await
    }

    /// Copies `len` bytes of object `e` from `off` into part `number` of
    /// upload `id` (UploadPartCopy, D193): read and written again, with
    /// the upload's checksum computed as they go by.
    #[allow(clippy::too_many_arguments)]
    pub async fn copy_part(
        &self,
        from: &Bucket,
        e: &Entry,
        off: u64,
        len: u64,
        to: &Bucket,
        key: &[u8],
        id: u128,
        number: u32,
    ) -> Result<PartRecord> {
        let mut put = self.upload_part(to, key, id, number, len).await?;
        let hash = put
            .pending
            .as_ref()
            .and_then(|p| p.multipart.as_ref())
            .and_then(|m| m.algorithm);
        match self.pump(&mut put, from, e, off, len, hash).await {
            Ok(sum) => put.finish_part(hash.zip(sum)).await,
            Err(err) => {
                let _ = put.abort().await;
                Err(err)
            }
        }
    }

    /// Reads `len` bytes of `e` from `off` into `put`, a few chunks at a
    /// time (which `read` fetches at once); returns `hash`'s value of
    /// them.
    async fn pump(
        &self,
        put: &mut Put<'_>,
        from: &Bucket,
        e: &Entry,
        off: u64,
        len: u64,
        hash: Option<chunked::Algorithm>,
    ) -> Result<Option<Vec<u8>>> {
        let mut hasher = hash.map(chunked::Hasher::new);
        let step = self.cfg.chunk.max(4096) * self.cfg.in_flight.max(1) as u64;
        let (mut at, end) = (off, off + len);
        while at < end {
            let n = step.min(end - at);
            let d = self.read(from, e, at, n).await?;
            if let Some(h) = &mut hasher {
                h.update(&d);
            }
            put.write(&d).await?;
            at += n;
        }
        Ok(hasher.map(chunked::Hasher::finish))
    }

    /// Gives up the uploads of bucket `b` that expired by `now` (Unix
    /// seconds) and weren't completed: their chunks are queued for deletion
    /// and their records removed. Returns how many it looked at.
    pub async fn clean(&self, b: &Bucket, now: u64) -> Result<usize> {
        let to = upload_row(now, 0);
        let mut from = vec![UPLOAD];
        let mut n = 0;
        loop {
            let scan = Scan {
                from: &from,
                to: Some(&to),
                prefix: &[UPLOAD],
                limit: 100,
            };
            let page = self.kv.scan(&b.table, &scan).await?;
            for r in &page.rows {
                let Some((_, id)) = upload_of(&r.row) else {
                    continue;
                };
                if let Some((_, key)) = r.columns.iter().find(|(c, _)| c == NAME) {
                    self.give_up(b, key, id).await?;
                }
                let mut gone = Batch::new();
                gone.delete(NAME).delete(MULTIPART);
                self.kv.apply(&b.table, &r.row, &gone).await?;
                n += 1;
            }
            match page.more {
                Some(m) => from = m,
                None => return Ok(n),
            }
        }
    }

    /// Removes upload `id` of `key` and every part it still has, written
    /// or being written, and queues their chunks for deletion; one batch
    /// on the object's row, so it and the upload's completion can't both
    /// happen. Only once the upload has expired (or its writers have
    /// stopped): a part still being written would write chunks after.
    async fn give_up(&self, b: &Bucket, key: &[u8], id: u128) -> Result<()> {
        let row = object_row(key);
        for _ in 0..ATTEMPTS {
            let cols = self.upload_columns(b, &row, id).await?;
            if cols.is_empty() {
                return Ok(());
            }
            let mut batch = Batch::new();
            for (c, v) in &cols {
                batch.assert(c, Some(v)).delete(c);
                let (prefix, count) = match part_of(c, id) {
                    Some((number, object)) => {
                        let p = PartRecord::decode(v)?;
                        (
                            chunk_prefix(b.id(), object, number),
                            chunks_of(p.size, p.chunk),
                        )
                    }
                    None => {
                        let p = Pending::decode(v)?;
                        let count = match p.multipart {
                            Some(_) => 0,
                            None => chunks_of(p.size, p.chunk),
                        };
                        (chunk_prefix(b.id(), id, 0), count)
                    }
                };
                batch.collect_series(b.table.data_pool, NS_S3, &prefix, count);
            }
            let r = self.kv.apply(&b.table, &row, &batch).await;
            if !kv_conflict(&r) {
                return Ok(r?);
            }
            // A part finished or the upload completed meanwhile: again.
        }
        Err(KvError::Exhausted(ATTEMPTS, "the upload kept changing".into()).into())
    }

    /// Upload `id`'s columns of `row`: its pending record and its parts.
    async fn upload_columns(
        &self,
        b: &Bucket,
        row: &[u8],
        id: u128,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let prefix = pending_column(id);
        Ok(self
            .kv
            .row(&b.table, row)
            .await?
            .into_iter()
            .filter(|(c, _)| c.starts_with(&prefix))
            .collect())
    }

    /// Begins a multipart upload of `key` (CreateMultipartUpload): its
    /// `u` row, then its pending record, as a PUT's (§14.6). Returns its
    /// id.
    pub async fn create_upload(&self, b: &Bucket, key: &[u8], mut m: Multipart) -> Result<u128> {
        if key.is_empty() || key.len() > 1024 {
            return Err(S3Error::InvalidKey);
        }
        let id = random_u128();
        m.initiated = now_ms();
        let p = Pending {
            expires: now_secs() + self.cfg.multipart_expiry,
            size: 0,
            chunk: self.cfg.chunk,
            multipart: Some(m.clone()),
        };
        let mut up = Batch::new();
        up.put(NAME, key).put(MULTIPART, &m.initiated.to_be_bytes());
        self.kv
            .apply(&b.table, &upload_row(p.expires, id), &up)
            .await?;
        let mut mark = Batch::new();
        mark.put(&pending_column(id), &p.encode());
        self.kv.apply(&b.table, &object_row(key), &mark).await?;
        Ok(id)
    }

    /// Multipart upload `id` of `key`: its pending record as stored, and
    /// read.
    async fn upload(&self, b: &Bucket, key: &[u8], id: u128) -> Result<(Vec<u8>, Pending)> {
        let v = self
            .kv
            .get(&b.table, &object_row(key), &pending_column(id))
            .await?
            .ok_or(S3Error::NoSuchUpload)?;
        let p = Pending::decode(&v)?;
        if p.multipart.is_none() {
            return Err(S3Error::NoSuchUpload);
        }
        Ok((v, p))
    }

    /// Begins part `number` of upload `id`, `size` bytes (UploadPart): its
    /// column is put, as being written, before its chunks are, in a batch
    /// that asserts the upload is still under way.
    pub async fn upload_part(
        &self,
        b: &Bucket,
        key: &[u8],
        id: u128,
        number: u32,
        size: u64,
    ) -> Result<Put<'_>> {
        if !(1..=MAX_PARTS).contains(&number) {
            return Err(S3Error::InvalidPart(number));
        }
        let (v, p) = self.upload(b, key, id).await?;
        if now_secs() + self.cfg.margin >= p.expires {
            return Err(S3Error::NoSuchUpload);
        }
        let object = random_u128();
        let col = part_column(id, number, object);
        let record = PartRecord {
            done: false,
            size,
            chunk: p.chunk,
            written: 0,
            md5: [0; 16],
            crc32c: 0,
            checksum: None,
        }
        .encode();
        let mut batch = Batch::new();
        batch
            .assert(&pending_column(id), Some(&v))
            .put(&col, &record);
        match self.kv.apply(&b.table, &object_row(key), &batch).await {
            Err(KvError::Conflict) => return Err(S3Error::NoSuchUpload),
            r => r?,
        }
        // The upload's chunk size, whatever this gateway's.
        let chunk = p.chunk;
        Ok(Put {
            s3: self,
            bucket: b.clone(),
            key: key.to_vec(),
            size,
            id: object,
            part: number,
            chunk,
            upload: Some(PartOf {
                id,
                pending: v,
                column: col,
                record,
            }),
            pending: Some(p),
            buf: Vec::new(),
            sent: 0,
            chunks: 0,
            writing: Default::default(),
            md5: Md5::new(),
            crc: 0,
            cond: Condition::Any,
        })
    }

    /// Upload `id`'s parts that are done, the latest of each number, and
    /// its record (ListParts).
    pub async fn parts(
        &self,
        b: &Bucket,
        key: &[u8],
        id: u128,
    ) -> Result<(
        Pending,
        std::collections::BTreeMap<u32, (Vec<u8>, Vec<u8>, PartRecord)>,
    )> {
        let row = object_row(key);
        let cols = self.upload_columns(b, &row, id).await?;
        let mut pending = None;
        let mut parts = std::collections::BTreeMap::new();
        for (c, v) in cols {
            match part_of(&c, id) {
                None => pending = Some(Pending::decode(&v)?),
                Some((number, _)) => {
                    let r = PartRecord::decode(&v)?;
                    if !r.done {
                        continue;
                    }
                    let newer = parts.get(&number).is_none_or(
                        |(_, _, old): &(Vec<u8>, Vec<u8>, PartRecord)| r.written >= old.written,
                    );
                    if newer {
                        parts.insert(number, (c, v, r));
                    }
                }
            }
        }
        match pending {
            Some(p) if p.multipart.is_some() => Ok((p, parts)),
            _ => Err(S3Error::NoSuchUpload),
        }
    }

    /// Completes upload `id` (CompleteMultipartUpload) with the parts
    /// named, in order, each by number and ETag, if `cond` holds of the
    /// object it replaces: one batch on the object's row asserts the
    /// upload and each part, puts the entry naming the parts (nothing is
    /// copied, §14.1) and removes the upload's columns, queuing the
    /// chunks of the object replaced and of parts not named. Parts still
    /// being written are left to the upload's expiry.
    pub async fn complete(
        &self,
        b: &Bucket,
        key: &[u8],
        id: u128,
        named: &[(u32, String)],
        cond: &Condition,
        want: Option<&(chunked::Algorithm, Vec<u8>)>,
    ) -> Result<Entry> {
        let row = object_row(key);
        for _ in 0..ATTEMPTS {
            let cols = self.upload_columns(b, &row, id).await?;
            let pcol = pending_column(id);
            let Some((_, pv)) = cols.iter().find(|(c, _)| *c == pcol) else {
                return Err(S3Error::NoSuchUpload);
            };
            let p = Pending::decode(pv)?;
            let Some(m) = p.multipart.clone() else {
                return Err(S3Error::NoSuchUpload);
            };
            // The order first, as S3 checks it.
            if named.windows(2).any(|w| w[1].0 <= w[0].0) {
                return Err(S3Error::InvalidPartOrder);
            }
            let (_, done) = self.parts(b, key, id).await?;
            let mut used = Vec::with_capacity(named.len());
            for (i, (number, etag)) in named.iter().enumerate() {
                let Some((c, v, r)) = done.get(number) else {
                    return Err(S3Error::InvalidPart(*number));
                };
                if r.etag().trim_matches('"') != etag.trim().trim_matches('"') {
                    return Err(S3Error::InvalidPart(*number));
                }
                if i + 1 < named.len() && r.size < self.cfg.min_part {
                    return Err(S3Error::EntityTooSmall(*number));
                }
                used.push((c, v, r));
            }
            if used.is_empty() {
                return Err(S3Error::InvalidPart(0));
            }
            let records: Vec<&PartRecord> = used.iter().map(|(_, _, r)| *r).collect();
            let (md5, crc32c) = combine(&records);
            let checksum = match m.algorithm {
                Some(a) => Some((a, combined_checksum(a, m.full_object, &records)?)),
                None => None,
            };
            if want.is_some_and(|w| Some(w) != checksum.as_ref()) {
                return Err(S3Error::BadDigest);
            }
            let entry = Entry {
                written: now_ms(),
                size: records.iter().map(|r| r.size).sum(),
                md5,
                parts: records.len() as u32,
                crc32c,
                content_type: m.content_type,
                meta: m.meta,
                data: Data::Parts {
                    chunk: p.chunk,
                    parts: used
                        .iter()
                        .map(|(c, _, r)| {
                            let (number, object) = part_of(c, id).expect("a part's column");
                            PartAt {
                                object,
                                number,
                                size: r.size,
                            }
                        })
                        .collect(),
                },
                checksum,
                composite: m.algorithm.is_some() && !m.full_object,
            };
            let old = self.latest(b, key).await?;
            if !cond.holds(old.as_ref().map(|(_, e)| e)) {
                return Err(S3Error::PreconditionFailed);
            }
            let new = entry.encode();
            let mut batch = Batch::new();
            batch
                .assert(LATEST, old.as_ref().map(|(v, _)| v.as_slice()))
                .put(LATEST, &new)
                .linger(self.cfg.linger);
            if let Some((_, e)) = &old {
                collect(&mut batch, b, e);
            }
            let mut writing = false;
            for (c, v) in &cols {
                let named = used.iter().any(|(uc, _, _)| *uc == c);
                batch.assert(c, Some(v));
                match part_of(c, id) {
                    None => {
                        batch.delete(c);
                    }
                    Some((number, object)) => {
                        let r = PartRecord::decode(v)?;
                        if !r.done {
                            writing = true;
                            continue;
                        }
                        batch.delete(c);
                        if !named {
                            batch.collect_series(
                                b.table.data_pool,
                                NS_S3,
                                &chunk_prefix(b.id(), object, number),
                                chunks_of(r.size, r.chunk),
                            );
                        }
                    }
                }
            }
            match self.kv.apply(&b.table, &row, &batch).await {
                Ok(()) => {}
                Err(KvError::Conflict) => continue,
                Err(e) => {
                    // Applied or not, unknown: the row says.
                    if self.kv.get(&b.table, &row, LATEST).await? != Some(new) {
                        return Err(e.into());
                    }
                }
            }
            if !writing {
                // Left to the clean-up if this fails.
                let mut gone = Batch::new();
                gone.delete(NAME).delete(MULTIPART);
                let _ = self
                    .kv
                    .apply(&b.table, &upload_row(p.expires, id), &gone)
                    .await;
            }
            return Ok(entry);
        }
        Err(KvError::Exhausted(ATTEMPTS, "the object kept changing".into()).into())
    }

    /// Gives upload `id` up (AbortMultipartUpload): its record and the
    /// parts done go in one batch, their chunks queued; parts still being
    /// written are left to the upload's expiry, or to their writers, who
    /// find the upload gone.
    pub async fn abort_upload(&self, b: &Bucket, key: &[u8], id: u128) -> Result<()> {
        let row = object_row(key);
        for _ in 0..ATTEMPTS {
            let cols = self.upload_columns(b, &row, id).await?;
            let pcol = pending_column(id);
            let Some((_, pv)) = cols.iter().find(|(c, _)| *c == pcol) else {
                return Err(S3Error::NoSuchUpload);
            };
            let p = Pending::decode(pv)?;
            if p.multipart.is_none() {
                return Err(S3Error::NoSuchUpload);
            }
            let mut batch = Batch::new();
            let mut writing = false;
            for (c, v) in &cols {
                match part_of(c, id) {
                    None => {
                        batch.assert(c, Some(v)).delete(c);
                    }
                    Some((number, object)) => {
                        let r = PartRecord::decode(v)?;
                        if !r.done {
                            writing = true;
                            continue;
                        }
                        batch.assert(c, Some(v)).delete(c).collect_series(
                            b.table.data_pool,
                            NS_S3,
                            &chunk_prefix(b.id(), object, number),
                            chunks_of(r.size, r.chunk),
                        );
                    }
                }
            }
            match self.kv.apply(&b.table, &row, &batch).await {
                Err(KvError::Conflict) => continue,
                r => r?,
            }
            if !writing {
                let mut gone = Batch::new();
                gone.delete(NAME).delete(MULTIPART);
                let _ = self
                    .kv
                    .apply(&b.table, &upload_row(p.expires, id), &gone)
                    .await;
            }
            return Ok(());
        }
        Err(KvError::Exhausted(ATTEMPTS, "the upload kept changing".into()).into())
    }

    /// The multipart uploads of bucket `b` under way, by name and then
    /// id: (name, id, when begun in Unix ms) (ListMultipartUploads).
    /// Every upload row is read, so this costs as many as are under way.
    pub async fn uploads(&self, b: &Bucket) -> Result<Vec<(Vec<u8>, u128, u64)>> {
        let mut from = vec![UPLOAD];
        let mut out = Vec::new();
        loop {
            let scan = Scan {
                from: &from,
                to: None,
                prefix: &[UPLOAD],
                limit: 1000,
            };
            let page = self.kv.scan(&b.table, &scan).await?;
            for r in &page.rows {
                let Some((_, id)) = upload_of(&r.row) else {
                    continue;
                };
                let get = |c: &[u8]| r.columns.iter().find(|(k, _)| k == c).map(|(_, v)| v);
                let (Some(key), Some(at)) = (get(NAME), get(MULTIPART)) else {
                    continue;
                };
                let at = at
                    .as_slice()
                    .try_into()
                    .map(u64::from_be_bytes)
                    .unwrap_or(0);
                // Completed or given up, with parts left to the expiry.
                let live = self
                    .kv
                    .get(&b.table, &object_row(key), &pending_column(id))
                    .await?
                    .is_some();
                if live {
                    out.push((key.clone(), id, at));
                }
            }
            match page.more {
                Some(m) => from = m,
                None => break,
            }
        }
        out.sort();
        Ok(out)
    }

    /// A page of bucket `b`'s objects (ListObjectsV2), in name order.
    pub async fn list(&self, b: &Bucket, q: &List<'_>) -> Result<Listing> {
        let max = q.max.max(1);
        let prefix = object_row(q.prefix);
        let mut start = object_row(q.start.max(q.prefix));
        let mut out = Listing::default();
        'scan: loop {
            let want = max + 1 - out.objects.len() - out.prefixes.len();
            let scan = Scan {
                from: &start,
                to: None,
                prefix: &prefix,
                limit: want,
            };
            let page = self.kv.scan(&b.table, &scan).await?;
            for r in &page.rows {
                let Some((_, v)) = r.columns.iter().find(|(c, _)| c == LATEST) else {
                    // Only uploads under way.
                    continue;
                };
                let key = &r.row[1..];
                if out.objects.len() + out.prefixes.len() == max {
                    out.next = Some(key.to_vec());
                    return Ok(out);
                }
                let rest = &key[q.prefix.len()..];
                if !q.delimiter.is_empty()
                    && let Some(at) = find(rest, q.delimiter)
                {
                    let common = key[..q.prefix.len() + at + q.delimiter.len()].to_vec();
                    let past = past_prefix(&object_row(&common));
                    out.prefixes.push(common);
                    match past {
                        Some(p) => {
                            start = p;
                            continue 'scan;
                        }
                        None => return Ok(out),
                    }
                }
                out.objects.push((key.to_vec(), Entry::decode(v)?));
            }
            match page.more {
                Some(m) => start = m,
                None => return Ok(out),
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Queues `e`'s chunks for deletion in `batch`.
fn collect(batch: &mut Batch, b: &Bucket, e: &Entry) {
    for (prefix, count) in e.series(b.id()) {
        batch.collect_series(b.table.data_pool, NS_S3, &prefix, count);
    }
}

/// A PUT under way: bytes go in with [`Put::write`], then
/// [`Put::finish`] makes the object visible, or [`Put::abort`] gives it
/// up.
pub struct Put<'a> {
    s3: &'a S3,
    bucket: Bucket,
    key: Vec<u8>,
    size: u64,
    /// The object id its chunks are named by: the upload's, or a part's.
    id: u128,
    /// The part number its chunks are named by: 0 for a single PUT.
    part: u32,
    /// Bytes per chunk.
    chunk: u64,
    /// Set for a multipart upload's part.
    upload: Option<PartOf>,
    /// None: the object is kept inline.
    pending: Option<Pending>,
    buf: Vec<u8>,
    sent: u64,
    /// Chunks written or being written.
    chunks: u32,
    writing: std::collections::VecDeque<JoinHandle<std::result::Result<(), ClientError>>>,
    md5: Md5,
    crc: u32,
    cond: Condition,
}

/// The upload a part belongs to, and the part's column as put when it
/// began.
struct PartOf {
    id: u128,
    /// The upload's pending record, as read.
    pending: Vec<u8>,
    column: Vec<u8>,
    record: Vec<u8>,
}

impl Put<'_> {
    /// Makes the object visible only if `cond` holds of the one it
    /// replaces; else [`Put::finish`] gives the upload up (D194).
    pub fn require(&mut self, cond: Condition) {
        self.cond = cond;
    }

    /// The upload's id, which names its chunks.
    pub fn id(&self) -> u128 {
        self.id
    }

    /// The MD5 of the bytes written so far, to check a `Content-MD5`
    /// before [`Put::finish`].
    pub fn md5(&self) -> [u8; 16] {
        self.md5.clone().finalize().into()
    }

    /// Bytes written so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        let got = self.sent + data.len() as u64;
        if got > self.size {
            return Err(S3Error::BadSize {
                want: self.size,
                got,
            });
        }
        self.sent = got;
        self.md5.update(data);
        self.crc = hyperstor_codec::crc32c_append(self.crc, data);
        self.buf.extend_from_slice(data);
        let chunk = self.chunk as usize;
        while self.pending.is_some() && self.buf.len() >= chunk {
            let rest = self.buf.split_off(chunk);
            let full = std::mem::replace(&mut self.buf, rest);
            self.write_chunk(full).await?;
        }
        Ok(())
    }

    /// Whether the upload may still change: not within the margin of its
    /// expiry, when it may be given up (14-s3.md §14.6).
    fn live(&self) -> Result<()> {
        match &self.pending {
            Some(p) if now_secs() + self.s3.cfg.margin >= p.expires => Err(S3Error::UploadGone),
            _ => Ok(()),
        }
    }

    /// Starts writing a chunk, once fewer than `in_flight` are.
    async fn write_chunk(&mut self, mut data: Vec<u8>) -> Result<()> {
        self.live()?;
        while self.writing.len() >= self.s3.cfg.in_flight.max(1) {
            self.wait_one().await?;
        }
        data.resize(data.len().div_ceil(4096) * 4096, 0);
        let name = chunk_name(self.bucket.id(), self.id, self.part, self.chunks);
        let (cl, pool) = (self.s3.client().clone(), self.bucket.table.data_pool);
        self.writing.push_back(hyperstor_reactor::spawn(async move {
            cl.exec(pool, NS_S3, &name, &[WireOp::Write { off: 0, data }])
                .await
        }));
        self.chunks += 1;
        Ok(())
    }

    async fn wait_one(&mut self) -> Result<()> {
        match self.writing.pop_front() {
            Some(w) => Ok(w.await?),
            None => Ok(()),
        }
    }

    /// Writes what is left and makes the object visible, replacing the one
    /// before, whose chunks are queued for deletion in the same batch.
    pub async fn finish(mut self, meta: Meta) -> Result<Entry> {
        if self.sent != self.size {
            let got = self.sent;
            let want = self.size;
            self.abort().await?;
            return Err(S3Error::BadSize { want, got });
        }
        let data = if self.pending.is_some() {
            if !self.buf.is_empty() {
                let last = std::mem::take(&mut self.buf);
                self.write_chunk(last).await?;
            }
            while !self.writing.is_empty() {
                self.wait_one().await?;
            }
            Data::Chunks {
                object: self.id,
                chunk: self.chunk,
                parts: vec![Part {
                    number: 0,
                    size: self.size,
                }],
            }
        } else {
            Data::Inline(std::mem::take(&mut self.buf))
        };
        let entry = Entry {
            written: now_ms(),
            size: self.size,
            md5: self.md5.clone().finalize().into(),
            parts: 0,
            crc32c: self.crc,
            content_type: meta.content_type,
            meta: meta.meta,
            data,
            checksum: meta.checksum,
            composite: false,
        };
        let new = entry.encode();
        let (s3, b) = (self.s3, &self.bucket);
        let row = object_row(&self.key);
        let col = pending_column(self.id);
        let mark = self.pending.as_ref().map(Pending::encode);
        for _ in 0..ATTEMPTS {
            self.live()?;
            let old = s3.latest(b, &self.key).await?;
            if !self.cond.holds(old.as_ref().map(|(_, e)| e)) {
                self.abort().await?;
                return Err(S3Error::PreconditionFailed);
            }
            let mut batch = Batch::new();
            batch
                .assert(LATEST, old.as_ref().map(|(v, _)| v.as_slice()))
                .put(LATEST, &new);
            if let Some(m) = &mark {
                batch.assert(&col, Some(m)).delete(&col);
            }
            if let Some((_, e)) = &old {
                batch.linger(s3.cfg.linger);
                collect(&mut batch, b, e);
            }
            match s3.kv.apply(&b.table, &row, &batch).await {
                Ok(()) => break,
                Err(KvError::Conflict) => {
                    if mark.is_some() && s3.kv.get(&b.table, &row, &col).await?.is_none() {
                        return Err(S3Error::UploadGone);
                    }
                }
                Err(e) => {
                    // Applied or not, unknown: the row says.
                    if s3.kv.get(&b.table, &row, LATEST).await? == Some(new.clone()) {
                        break;
                    }
                    return Err(e.into());
                }
            }
        }
        if let Some(p) = &self.pending {
            // Left to the clean-up if this fails: the object row no longer
            // names the upload.
            let mut gone = Batch::new();
            gone.delete(NAME);
            let _ = s3
                .kv
                .apply(&b.table, &upload_row(p.expires, self.id), &gone)
                .await;
        }
        Ok(entry)
    }

    /// Writes what is left of a part and records it done, with the
    /// checksum its body was checked against, in a batch that asserts
    /// the upload is still under way and drops the parts of its number
    /// done before, queuing their chunks. [`S3Error::NoSuchUpload`] if the
    /// upload was completed or given up meanwhile: the part is given up.
    pub async fn finish_part(
        mut self,
        checksum: Option<(chunked::Algorithm, Vec<u8>)>,
    ) -> Result<PartRecord> {
        if self.sent != self.size {
            let (got, want) = (self.sent, self.size);
            self.abort().await?;
            return Err(S3Error::BadSize { want, got });
        }
        if !self.buf.is_empty() {
            let last = std::mem::take(&mut self.buf);
            self.write_chunk(last).await?;
        }
        while !self.writing.is_empty() {
            self.wait_one().await?;
        }
        let Some(up) = &self.upload else {
            return Err(S3Error::NoSuchUpload);
        };
        let record = PartRecord {
            done: true,
            size: self.size,
            chunk: self.chunk,
            written: now_ms(),
            md5: self.md5.clone().finalize().into(),
            crc32c: self.crc,
            checksum,
        };
        let done = record.encode();
        let (s3, b) = (self.s3, &self.bucket);
        let row = object_row(&self.key);
        let same = part_column(up.id, self.part, 0);
        let same = &same[..same.len() - 16];
        for _ in 0..ATTEMPTS {
            self.live()?;
            let cols = s3.upload_columns(b, &row, up.id).await?;
            let mut batch = Batch::new();
            batch
                .assert(&pending_column(up.id), Some(&up.pending))
                .assert(&up.column, Some(&up.record))
                .put(&up.column, &done);
            for (c, v) in &cols {
                if !c.starts_with(same) || *c == up.column {
                    continue;
                }
                let r = PartRecord::decode(v)?;
                if r.done {
                    let (number, object) = part_of(c, up.id).expect("a part's column");
                    batch.assert(c, Some(v)).delete(c).collect_series(
                        b.table.data_pool,
                        NS_S3,
                        &chunk_prefix(b.id(), object, number),
                        chunks_of(r.size, r.chunk),
                    );
                }
            }
            match s3.kv.apply(&b.table, &row, &batch).await {
                Ok(()) => return Ok(record),
                Err(KvError::Conflict) => {
                    let pending = s3.kv.get(&b.table, &row, &pending_column(up.id)).await?;
                    if pending.as_deref() != Some(up.pending.as_slice()) {
                        self.abort().await?;
                        return Err(S3Error::NoSuchUpload);
                    }
                }
                Err(e) => {
                    if s3.kv.get(&b.table, &row, &up.column).await? == Some(done.clone()) {
                        return Ok(record);
                    }
                    return Err(e.into());
                }
            }
        }
        Err(KvError::Exhausted(ATTEMPTS, "the upload kept changing".into()).into())
    }

    /// Gives the upload up: its chunks are queued for deletion and its
    /// records removed.
    pub async fn abort(mut self) -> Result<()> {
        // Before the chunks are queued, so none is written after.
        while !self.writing.is_empty() {
            let _ = self.wait_one().await;
        }
        if let Some(up) = &self.upload {
            // A part: only its own column and chunks.
            let b = &self.bucket;
            let mut batch = Batch::new();
            batch
                .assert(&up.column, Some(&up.record))
                .delete(&up.column)
                .collect_series(
                    b.table.data_pool,
                    NS_S3,
                    &chunk_prefix(b.id(), self.id, self.part),
                    chunks_of(self.size, self.chunk),
                );
            let r = self
                .s3
                .kv
                .apply(&b.table, &object_row(&self.key), &batch)
                .await;
            if kv_conflict(&r) {
                // Given up with the upload already.
                return Ok(());
            }
            return Ok(r?);
        }
        let Some(p) = &self.pending else {
            return Ok(());
        };
        let b = &self.bucket;
        self.s3.give_up(b, &self.key, self.id).await?;
        let mut gone = Batch::new();
        gone.delete(NAME);
        self.s3
            .kv
            .apply(&b.table, &upload_row(p.expires, self.id), &gone)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunked::{Algorithm, Hasher};

    fn part(data: &[u8], a: Algorithm) -> PartRecord {
        let mut h = Hasher::new(a);
        h.update(data);
        PartRecord {
            done: true,
            size: data.len() as u64,
            chunk: 4096,
            written: 1,
            md5: Md5::digest(data).into(),
            crc32c: crc32c::crc32c(data),
            checksum: Some((a, h.finish())),
        }
    }

    #[test]
    fn parts_combine_into_the_whole_objects_sums() {
        let pieces: [&[u8]; 3] = [b"hello, ", b"", b"multipart world"];
        let whole: Vec<u8> = pieces.concat();
        for a in [Algorithm::Crc32, Algorithm::Crc32c] {
            let parts: Vec<_> = pieces.iter().map(|p| part(p, a)).collect();
            let refs: Vec<&PartRecord> = parts.iter().collect();
            let mut h = Hasher::new(a);
            h.update(&whole);
            assert_eq!(
                combined_checksum(a, true, &refs).unwrap(),
                h.finish(),
                "{a:?}"
            );
            let (md5, crc) = combine(&refs);
            assert_eq!(crc, crc32c::crc32c(&whole));
            let mut m = Md5::new();
            for p in pieces {
                m.update(Md5::digest(p));
            }
            assert_eq!(md5, <[u8; 16]>::from(m.finalize()));
        }
        // Composite: the algorithm over the parts' checksums in order.
        let parts: Vec<_> = pieces.iter().map(|p| part(p, Algorithm::Sha256)).collect();
        let refs: Vec<&PartRecord> = parts.iter().collect();
        let mut h = Hasher::new(Algorithm::Sha256);
        for p in &parts {
            h.update(&p.checksum.as_ref().unwrap().1);
        }
        assert_eq!(
            combined_checksum(Algorithm::Sha256, false, &refs).unwrap(),
            h.finish()
        );
        // A part without the upload's algorithm.
        let mixed = [&parts[0], &part(b"x", Algorithm::Crc32)];
        assert!(matches!(
            combined_checksum(Algorithm::Sha256, false, &mixed),
            Err(S3Error::MissingPartChecksum(2))
        ));
    }
}
