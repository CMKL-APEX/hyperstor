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

pub mod layout;

use hyperstor_client::{Client, ClientError};
use hyperstor_kv::{Batch, KvError, S3_STORE, Scan, Store, Table};
use hyperstor_proto::{Code, WireOp};
use layout::*;
use md5::{Digest, Md5};
use std::rc::Rc;

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("no bucket {0:?}")]
    NoSuchBucket(String),
    #[error("bucket {0:?} exists")]
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
    #[error("the object was replaced while it was read")]
    Changed,
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            inline: 64 << 10,
            chunk: 4 << 20,
            put_expiry: 24 * 3600,
            margin: 3600,
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn now_secs() -> u64 {
    now_ms() / 1000
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
    pub name: String,
    pub table: Table,
    pub info: BucketInfo,
}

impl Bucket {
    fn of(table: Table) -> Result<Bucket> {
        Ok(Bucket {
            name: String::from_utf8(table.name.clone())
                .map_err(|_| S3Error::Corrupt("a bucket name".into()))?,
            info: BucketInfo::decode(&table.info)?,
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
fn past_prefix(p: &[u8]) -> Option<Vec<u8>> {
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
    /// data in `data_pool` (D197, D198).
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
            .create_table_with(name.as_bytes(), index_pool, data_pool, &info.encode())
            .await;
        match made {
            Ok(t) => Bucket::of(t),
            Err(KvError::TableExists(_)) => Err(S3Error::BucketExists(name.into())),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn bucket(&self, name: &str) -> Result<Bucket> {
        match self.kv.table(name.as_bytes()).await? {
            Some(t) => Bucket::of(t),
            None => Err(S3Error::NoSuchBucket(name.into())),
        }
    }

    /// Every bucket of `tenant`, by name.
    pub async fn buckets(&self, tenant: u32) -> Result<Vec<Bucket>> {
        let mut out = Vec::new();
        let mut from = Vec::new();
        loop {
            let page = self.kv.tables(&from, 1000).await?;
            let Some(last) = page.last() else {
                return Ok(out);
            };
            from = [&last.name[..], &[0]].concat();
            for t in page {
                let b = Bucket::of(t)?;
                if b.info.tenant == tenant {
                    out.push(b);
                }
            }
        }
    }

    /// Deletes bucket `name`, which must hold no object and no upload under
    /// way. Uploads that expired are cleaned up first.
    pub async fn delete_bucket(&self, name: &str) -> Result<()> {
        let b = self.bucket(name).await?;
        self.clean(&b, now_secs()).await?;
        let any = Scan {
            limit: 1,
            ..Default::default()
        };
        if !self.kv.scan(&b.table, &any).await?.rows.is_empty() {
            return Err(S3Error::BucketNotEmpty(name.into()));
        }
        match self.kv.delete_table(name.as_bytes()).await {
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
        let (object, chunk, parts) = match &e.data {
            Data::Inline(d) => return Ok(d[off as usize..(off + len) as usize].to_vec()),
            Data::Chunks {
                object,
                chunk,
                parts,
            } => (*object, *chunk, parts),
        };
        // (chunk name, offset in it, bytes) for each piece of the range.
        let mut pieces = Vec::new();
        let (mut at, end) = (off, off + len);
        let mut part_start = 0;
        for p in parts {
            let part_end = part_start + p.size;
            while at < end && at < part_end {
                let in_part = at - part_start;
                let index = in_part / chunk;
                let in_chunk = in_part % chunk;
                let n = (chunk - in_chunk).min(end - at).min(part_end - at);
                let name = chunk_name(b.id(), object, p.number, index as u32);
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
        let row = object_row(key);
        for _ in 0..ATTEMPTS {
            let Some((v, e)) = self.latest(b, key).await? else {
                return Ok(());
            };
            let mut batch = Batch::new();
            batch.assert(LATEST, Some(&v)).delete(LATEST);
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
            pending,
            buf: Vec::new(),
            sent: 0,
            chunks: 0,
            md5: Md5::new(),
            crc: 0,
        })
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
                gone.delete(NAME);
                self.kv.apply(&b.table, &r.row, &gone).await?;
                n += 1;
            }
            match page.more {
                Some(m) => from = m,
                None => return Ok(n),
            }
        }
    }

    /// Removes upload `id` of `key`, if it is still under way, and queues
    /// its chunks for deletion; one batch on the object's row, so it and
    /// the upload's completion can't both happen.
    async fn give_up(&self, b: &Bucket, key: &[u8], id: u128) -> Result<()> {
        let row = object_row(key);
        let col = pending_column(id);
        let Some(v) = self.kv.get(&b.table, &row, &col).await? else {
            return Ok(());
        };
        let p = Pending::decode(&v)?;
        let mut batch = Batch::new();
        batch.assert(&col, Some(&v)).delete(&col).collect_series(
            b.table.data_pool,
            NS_S3,
            &chunk_prefix(b.id(), id, 0),
            chunks_of(p.size, p.chunk),
        );
        let r = self.kv.apply(&b.table, &row, &batch).await;
        if kv_conflict(&r) {
            // Completed or given up meanwhile.
            return Ok(());
        }
        Ok(r?)
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
    id: u128,
    /// None: the object is kept inline.
    pending: Option<Pending>,
    buf: Vec<u8>,
    sent: u64,
    /// Chunks written.
    chunks: u32,
    md5: Md5,
    crc: u32,
}

impl Put<'_> {
    /// The upload's id, which names its chunks.
    pub fn id(&self) -> u128 {
        self.id
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
        let chunk = self.s3.cfg.chunk as usize;
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

    async fn write_chunk(&mut self, mut data: Vec<u8>) -> Result<()> {
        self.live()?;
        data.resize(data.len().div_ceil(4096) * 4096, 0);
        let name = chunk_name(self.bucket.id(), self.id, 0, self.chunks);
        self.s3
            .client()
            .exec(
                self.bucket.table.data_pool,
                NS_S3,
                &name,
                &[WireOp::Write { off: 0, data }],
            )
            .await?;
        self.chunks += 1;
        Ok(())
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
            Data::Chunks {
                object: self.id,
                chunk: self.s3.cfg.chunk,
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
        };
        let new = entry.encode();
        let (s3, b) = (self.s3, &self.bucket);
        let row = object_row(&self.key);
        let col = pending_column(self.id);
        let mark = self.pending.as_ref().map(Pending::encode);
        for _ in 0..ATTEMPTS {
            self.live()?;
            let old = s3.latest(b, &self.key).await?;
            let mut batch = Batch::new();
            batch
                .assert(LATEST, old.as_ref().map(|(v, _)| v.as_slice()))
                .put(LATEST, &new);
            if let Some(m) = &mark {
                batch.assert(&col, Some(m)).delete(&col);
            }
            if let Some((_, e)) = &old {
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

    /// Gives the upload up: its chunks are queued for deletion and its
    /// records removed.
    pub async fn abort(self) -> Result<()> {
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
