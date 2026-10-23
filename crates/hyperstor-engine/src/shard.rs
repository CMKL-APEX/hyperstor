//! A shard: one reactor's part of a device (spec/engine.md §7,
//! spec/implementation-notes.md I1).
//!
//! Layout of a shard's region, in blocks:
//!
//! ```text
//! [super A][super B][log ring: log_blocks][data: the rest]
//! ```
//!
//! - **Supers** alternate by sequence number. Each names the checkpoint
//!   (its extents, length and checksum), the log position the checkpoint
//!   covers (`head_lsn`), and the log's salt.
//! - **Log records** are whole blocks. A record's position in the ring is
//!   its LSN modulo the ring size; its header carries the LSN and checksums
//!   seeded with the salt, so stale blocks from an earlier pass of the ring,
//!   from an earlier format or from tenant data inside a record never pass
//!   as a record. The salt changes every time the shard is opened.
//! - **Commit** (one round): resolve each operation list, write new data
//!   extents and the log records together, flush once, then acknowledge.
//!   Blocks freed by a round are reused only after its flush.
//! - **Recovery**: load the checkpoint, replay records from `head_lsn`, and
//!   check the data checksums of the last commit group, whose data may not
//!   have reached the media; a record whose data didn't is dropped with
//!   everything after it. None of it was acknowledged.

use crate::align::{AlignedBuf, BLOCK};
use crate::alloc::Allocator;
use crate::dev::BlockDev;
use crate::index::{DeferredWrite, Effects, Index, Loc, Object, RList, ROp};
use crate::types::{Committed, EngineError, ObjKey, ObjStat, Op, OpError, OpList};
use hyperstor_codec::{Dec, DecodeError, Enc, crc32c, crc32c_append};
use std::collections::BTreeMap;
use std::sync::Arc;

const SUPER_MAGIC: u64 = 0x4853_5348_5244_3031; // "HSSHRD01"
const LOG_MAGIC: u32 = 0x4853_4c47; // "HSLG"
/// A checkpoint of one blob of objects (I33), read from devices written
/// before paged checkpoints.
const CKPT_MAGIC: u64 = 0x4853_434b_5054_3033; // "HSCKPT03"
const DIR_MAGIC: u64 = 0x4853_434b_4449_5231; // "HSCKDIR1"
/// A paged checkpoint's directory, and one page (D105).
const DIR2_MAGIC: u64 = 0x4853_434b_4449_5232; // "HSCKDIR2"
const PAGE_MAGIC: u64 = 0x4853_434b_5047_3031; // "HSCKPG01"
/// A checkpoint page is cut once it holds this much: small enough that a
/// change rewrites little, large enough that the directory stays small
/// (about 100 bytes a page).
const PAGE_BYTES: usize = 128 * 1024;
/// Checkpoint bytes written per commit round while a fuzzy checkpoint runs
/// (64 KiB had the best tail latency, benchmarks.md); eight times that once
/// half the log is used, so the checkpoint finishes before it is forced.
const SLICE_BYTES: usize = 64 * 1024;
const LOG_VERSION: u16 = 1;
const HEADER_LEN: usize = 40;
/// Writes smaller than this to blocks the object already holds go through
/// the log and are applied in place later (spec/decisions.md D22).
pub const DEFER_MAX: usize = 64 * 1024;
/// Pending in-place writes above this many blocks are applied before the
/// next commit, to bound memory.
const PENDING_LIMIT: usize = 16 * 1024;
/// Checkpoint runs a super block can name.
const MAX_CKPT_RUNS: usize = 200;

/// Where a shard lives on its device, in blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub blocks: u64,
    pub log_blocks: u64,
}

impl Region {
    fn log_start(&self) -> u64 {
        self.start + 2
    }
    fn data_start(&self) -> u64 {
        self.start + 2 + self.log_blocks
    }
    fn data_blocks(&self) -> u64 {
        self.blocks - 2 - self.log_blocks
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Super {
    seq: u64,
    device_id: u128,
    shard: u32,
    salt: u32,
    head_lsn: u64,
    ckpt: Option<Ckpt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Ckpt {
    runs: Vec<(u64, u64)>,
    len: u64,
    crc: u32,
}

impl Super {
    fn encode(&self) -> AlignedBuf {
        let mut e = Enc::with_capacity(BLOCK);
        e.u64(SUPER_MAGIC)
            .u64(self.seq)
            .u128(self.device_id)
            .u32(self.shard)
            .u32(self.salt)
            .u64(self.head_lsn);
        match &self.ckpt {
            None => {
                e.bool(false);
            }
            Some(c) => {
                e.bool(true)
                    .u64(c.len)
                    .u32(c.crc)
                    .seq(&c.runs, |e, (s, n)| {
                        e.u64(*s).u64(*n);
                    });
            }
        }
        let crc = crc32c(e.as_slice());
        e.u32(crc);
        assert!(e.len() <= BLOCK);
        AlignedBuf::from_slice(e.as_slice())
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        let mut d = Dec::new(buf);
        let parse = |d: &mut Dec<'_>| -> Result<Super, DecodeError> {
            if d.u64()? != SUPER_MAGIC {
                return Err(DecodeError::Magic(0));
            }
            let seq = d.u64()?;
            let device_id = d.u128()?;
            let shard = d.u32()?;
            let salt = d.u32()?;
            let head_lsn = d.u64()?;
            let ckpt = if d.bool()? {
                let len = d.u64()?;
                let crc = d.u32()?;
                let runs = d.seq(16, |d| Ok((d.u64()?, d.u64()?)))?;
                Some(Ckpt { runs, len, crc })
            } else {
                None
            };
            Ok(Super {
                seq,
                device_id,
                shard,
                salt,
                head_lsn,
                ckpt,
            })
        };
        let s = parse(&mut d).ok()?;
        let end = d.position();
        let crc = d.u32().ok()?;
        (crc32c(&buf[..end]) == crc).then_some(s)
    }
}

/// Where a checkpoint page is (D105).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PageLoc {
    runs: Vec<(u64, u64)>,
    len: u64,
    crc: u32,
}

/// A fuzzy checkpoint being written (implementation-notes.md I33): only
/// the pages changed since the last one began (D105).
struct Fuzzy {
    /// Log position replay will start from once this checkpoint is named.
    start_lsn: u64,
    next_revision: u64,
    next_group: u64,
    /// Changed pages still to write, by bound, in key order.
    todo: std::collections::VecDeque<Option<ObjKey>>,
    /// The page being written.
    cur: Option<Cur>,
    /// Pages written, by bound.
    written: BTreeMap<Option<ObjKey>, PageLoc>,
    /// Pages of the last checkpoint this one replaces or drops.
    replaced: Vec<Option<ObjKey>>,
}

/// A changed page being written. It is cut into pieces of about
/// `page_bytes`, each a page of its own; an empty page other than the
/// first is dropped, its range joining the page before.
struct Cur {
    /// The page's bound, and the bound of the piece being encoded.
    first: Option<ObjKey>,
    piece: Option<ObjKey>,
    /// The next page's bound: where this page ends.
    end: Option<ObjKey>,
    /// The last object encoded; the next slice continues after it.
    last: Option<ObjKey>,
    /// The piece so far, and whether it holds an object.
    buf: Enc,
    objects: bool,
}

/// Objects in a checkpoint are each preceded by 1; 0 ends them.
fn next_object(d: &mut Dec<'_>) -> Result<bool, EngineError> {
    match d.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(EngineError::Corrupt(format!("checkpoint record {v}"))),
    }
}

fn page_head() -> Enc {
    let mut e = Enc::new();
    e.u64(PAGE_MAGIC);
    e
}

/// A record as read back from the log.
struct LogRecord {
    lsn: u64,
    group: u64,
    nblocks: u64,
    lists: Vec<RList>,
}

/// Counters, for tests and status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShardStats {
    pub objects: u64,
    pub free_blocks: u64,
    pub data_blocks: u64,
    pub log_used_blocks: u64,
    pub log_blocks: u64,
    pub pending_blocks: u64,
    pub checkpoints: u64,
    pub commits: u64,
    pub records: u64,
    /// Size of the last checkpoint (the serialized index), in bytes.
    pub checkpoint_bytes: u64,
    /// Slices written by fuzzy checkpoints, and checkpoints finished
    /// synchronously because the log ran short.
    pub checkpoint_slices: u64,
    pub checkpoint_forced: u64,
    /// Log blocks written since the shard was opened.
    pub log_blocks_written: u64,
    /// Checkpoint bytes written (pages and directories) since opened.
    pub checkpoint_bytes_written: u64,
    /// Pages of the last checkpoint, and pages written since opened (D105).
    pub checkpoint_pages: u64,
    pub checkpoint_pages_written: u64,
    /// The last checkpoint's directory, in bytes: written by every
    /// checkpoint, whatever changed.
    pub checkpoint_directory_bytes: u64,
}

/// Totals over the whole index. Computing them walks every block map, so
/// they are separate from [`ShardStats`], which costs nothing to read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Extent runs in the index (one per contiguous mapped range).
    pub runs: u64,
    /// Objects whose block map is dense (D43).
    pub dense_objects: u64,
    /// Estimated memory of every block map, in bytes.
    pub map_bytes: u64,
}

/// One shard of a device.
pub struct Shard {
    dev: Arc<dyn BlockDev>,
    region: Region,
    device_id: u128,
    shard: u32,
    index: Index,
    alloc: Allocator,
    /// Deferred in-place writes not yet applied (physical block -> data).
    pending: BTreeMap<u64, Box<[u8]>>,
    salt: u32,
    super_seq: u64,
    head_lsn: u64,
    tail_lsn: u64,
    next_group: u64,
    next_revision: u64,
    ckpt: Option<Ckpt>,
    poisoned: bool,
    /// Physical extents allocated in the current commit round. A write is
    /// never deferred onto them: recovery checks the last round's new
    /// extents against the checksums their `Map` recorded, which an
    /// in-place write of the same round would contradict.
    round_fresh: BTreeMap<u64, u64>,
    /// Writes smaller than this onto blocks the object holds are deferred.
    defer_max: usize,
    /// The current checkpoint's pages, by bound (the superblock names
    /// their directory, `ckpt`).
    pages: BTreeMap<Option<ObjKey>, PageLoc>,
    /// The blob of a checkpoint from before pages, freed once a paged one
    /// replaces it.
    legacy_blob: Vec<(u64, u64)>,
    page_bytes: usize,
    ckpt_pages_written: u64,
    /// A fuzzy checkpoint in progress.
    fuzzy: Option<Fuzzy>,
    slice_bytes: usize,
    opened_lsn: u64,
    ckpt_written: u64,
    checkpoint_slices: u64,
    checkpoint_forced: u64,
    checkpoints: u64,
    commits: u64,
    records: u64,
}

fn random_u32() -> u32 {
    let mut v = 0u32;
    // SAFETY: getrandom writes at most 4 bytes into `v`, which outlives the call.
    let r = unsafe { libc::getrandom(&mut v as *mut u32 as *mut libc::c_void, 4, 0) };
    if r == 4 {
        v
    } else {
        // Fall back to the clock; the salt guards against stale blocks, not
        // against an adversary who can predict it.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (t.as_nanos() as u32) ^ std::process::id()
    }
}

impl Shard {
    /// Formats an empty shard in `region`.
    pub fn format(
        dev: Arc<dyn BlockDev>,
        region: Region,
        device_id: u128,
        shard: u32,
    ) -> Result<(), EngineError> {
        let s = Super {
            seq: 1,
            device_id,
            shard,
            salt: random_u32(),
            head_lsn: 0,
            ckpt: None,
        };
        let buf = s.encode();
        let zero = AlignedBuf::zeroed(BLOCK);
        dev.write_batch(&[
            (region.start * BLOCK as u64, &buf),
            ((region.start + 1) * BLOCK as u64, &zero),
            (region.log_start() * BLOCK as u64, &zero),
        ])?;
        dev.flush()?;
        Ok(())
    }

    /// Opens a shard: recovers it from its checkpoint and log, then writes
    /// a fresh checkpoint under a new salt.
    pub fn open(
        dev: Arc<dyn BlockDev>,
        region: Region,
        device_id: u128,
        shard: u32,
    ) -> Result<Self, EngineError> {
        let mut a = AlignedBuf::zeroed(BLOCK);
        let mut b = AlignedBuf::zeroed(BLOCK);
        dev.read_batch(&mut [
            (region.start * BLOCK as u64, &mut a),
            ((region.start + 1) * BLOCK as u64, &mut b),
        ])?;
        let sup = [Super::decode(&a), Super::decode(&b)]
            .into_iter()
            .flatten()
            .filter(|s| s.device_id == device_id && s.shard == shard)
            .max_by_key(|s| s.seq)
            .ok_or(EngineError::NotFormatted)?;

        let mut sh = Shard {
            dev,
            region,
            device_id,
            shard,
            index: Index::default(),
            alloc: Allocator::default(),
            pending: BTreeMap::new(),
            salt: sup.salt,
            super_seq: sup.seq,
            head_lsn: sup.head_lsn,
            tail_lsn: sup.head_lsn,
            next_group: 1,
            next_revision: 1,
            ckpt: sup.ckpt.clone(),
            poisoned: false,
            round_fresh: BTreeMap::new(),
            defer_max: DEFER_MAX,
            pages: BTreeMap::new(),
            legacy_blob: Vec::new(),
            page_bytes: PAGE_BYTES,
            ckpt_pages_written: 0,
            fuzzy: None,
            slice_bytes: SLICE_BYTES,
            opened_lsn: 0,
            ckpt_written: 0,
            checkpoint_slices: 0,
            checkpoint_forced: 0,
            checkpoints: 0,
            commits: 0,
            records: 0,
        };
        if let Some(c) = &sup.ckpt {
            sh.load_checkpoint(c)?;
        }
        let mut records = sh.scan_log()?;
        let (base_index, base_rev, base_group) =
            (sh.index.clone(), sh.next_revision, sh.next_group);
        // Replay, then check the last group's new data; if some didn't reach
        // the media, drop that record and everything after it, and replay
        // again from the checkpoint.
        let mut fx = loop {
            sh.index = base_index.clone();
            sh.next_revision = base_rev;
            sh.next_group = base_group;
            sh.tail_lsn = sh.head_lsn;
            let mut fx = Effects::default();
            for r in &records {
                for l in &r.lists {
                    sh.index.apply(l, &mut fx);
                    for op in &l.ops {
                        if let ROp::SetRevision(v) = op {
                            sh.next_revision = sh.next_revision.max(v + 1);
                        }
                    }
                }
                sh.next_group = sh.next_group.max(r.group + 1);
                sh.tail_lsn = r.lsn + r.nblocks;
            }
            match sh.first_unwritten(&records, &fx)? {
                None => break fx,
                Some(i) => records.truncate(i),
            }
        };
        sh.index.settle_tentative(&mut fx);
        sh.pending = fx.pending;
        sh.rebuild_allocator()?;
        sh.apply_deferred()?;
        sh.salt = random_u32();
        sh.checkpoint()?;
        sh.opened_lsn = sh.tail_lsn;
        sh.ckpt_written = 0;
        sh.ckpt_pages_written = 0;
        Ok(sh)
    }

    fn rebuild_allocator(&mut self) -> Result<(), EngineError> {
        let mut a = Allocator::new(self.region.data_start(), self.region.data_blocks());
        let mut used: Vec<(u64, u64)> = self.index.used_extents().collect();
        if let Some(c) = &self.ckpt {
            used.extend(c.runs.iter().copied());
        }
        used.extend(self.legacy_blob.iter().copied());
        used.extend(self.pages.values().flat_map(|p| p.runs.iter().copied()));
        for (s, n) in used {
            if !a.reserve(s, n) {
                return Err(EngineError::Corrupt(format!(
                    "extent {s}+{n} is outside the data region or used twice"
                )));
            }
        }
        self.alloc = a;
        Ok(())
    }

    fn read_extents(&self, runs: &[(u64, u64)], len: u64) -> Result<AlignedBuf, EngineError> {
        let nblocks: u64 = runs.iter().map(|r| r.1).sum();
        if len > nblocks * BLOCK as u64 {
            return Err(EngineError::Corrupt("checkpoint length".into()));
        }
        let mut buf = AlignedBuf::zeroed((nblocks.max(1) as usize) * BLOCK);
        let mut reads = Vec::new();
        let mut rest: &mut [u8] = &mut buf;
        for &(s, n) in runs {
            let (head, tail) = rest.split_at_mut(n as usize * BLOCK);
            reads.push((s * BLOCK as u64, head));
            rest = tail;
        }
        self.dev.read_batch(&mut reads)?;
        Ok(buf)
    }

    /// Loads the checkpoint named by the superblock: its directory, then
    /// the pages it lists (or, from before pages, the one blob). The
    /// checkpoint may be fuzzy (objects written at different times after
    /// `head_lsn`); replay from `head_lsn` brings it to the state the log
    /// describes.
    fn load_checkpoint(&mut self, c: &Ckpt) -> Result<(), EngineError> {
        let dir = self.read_extents(&c.runs, c.len)?;
        let dir = &dir[..c.len as usize];
        if crc32c(dir) != c.crc {
            return Err(EngineError::Corrupt("checkpoint directory checksum".into()));
        }
        let mut d = Dec::new(dir);
        match d.u64()? {
            DIR_MAGIC => self.load_blob(&mut d),
            DIR2_MAGIC => self.load_pages(&mut d),
            _ => Err(EngineError::Corrupt("checkpoint directory magic".into())),
        }
    }

    /// A checkpoint from before pages: one blob, which the next checkpoint
    /// replaces with pages.
    fn load_blob(&mut self, d: &mut Dec<'_>) -> Result<(), EngineError> {
        let blob_len = d.u64()?;
        let blob_crc = d.u32()?;
        let runs = d.seq(16, |d| Ok((d.u64()?, d.u64()?)))?;
        d.finish()?;
        let blob = self.read_extents(&runs, blob_len)?;
        let data = &blob[..blob_len as usize];
        if crc32c(data) != blob_crc {
            return Err(EngineError::Corrupt("checkpoint checksum".into()));
        }
        let mut d = Dec::new(data);
        if d.u64()? != CKPT_MAGIC {
            return Err(EngineError::Corrupt("checkpoint magic".into()));
        }
        self.next_revision = d.u64()?;
        self.next_group = d.u64()?;
        let lsn = d.u64()?;
        if lsn != self.head_lsn {
            return Err(EngineError::Corrupt("checkpoint LSN".into()));
        }
        let mut index = Index::default();
        while next_object(&mut d)? {
            let (k, o) = crate::index::decode_object(&mut d)?;
            index.objects.insert(k, o);
        }
        d.finish()?;
        index.first_dirty = true;
        self.index = index;
        self.legacy_blob = runs;
        Ok(())
    }

    /// A paged checkpoint (D105): every page is read in one batch.
    fn load_pages(&mut self, d: &mut Dec<'_>) -> Result<(), EngineError> {
        self.next_revision = d.u64()?;
        self.next_group = d.u64()?;
        if d.u64()? != self.head_lsn {
            return Err(EngineError::Corrupt("checkpoint LSN".into()));
        }
        let n = d.u64()?;
        let mut pages = BTreeMap::new();
        for i in 0..n {
            let bound = if d.bool()? {
                Some(ObjKey::decode(d)?)
            } else {
                None
            };
            if (i == 0) != bound.is_none() {
                return Err(EngineError::Corrupt("checkpoint page bounds".into()));
            }
            let len = d.u64()?;
            let crc = d.u32()?;
            let runs = d.seq(16, |d| Ok((d.u64()?, d.u64()?)))?;
            pages.insert(bound, PageLoc { runs, len, crc });
        }
        d.finish()?;
        if pages.len() as u64 != n {
            return Err(EngineError::Corrupt("checkpoint page bounds".into()));
        }
        let runs: Vec<(u64, u64)> = pages.values().flat_map(|p| p.runs.clone()).collect();
        let nblocks: u64 = runs.iter().map(|r| r.1).sum();
        let all = self.read_extents(&runs, nblocks * BLOCK as u64)?;
        let mut index = Index::default();
        let mut at = 0usize;
        let bounds: Vec<Option<ObjKey>> = pages.keys().cloned().collect();
        for (i, (bound, p)) in pages.iter().enumerate() {
            let blocks: u64 = p.runs.iter().map(|r| r.1).sum();
            if p.len > blocks * BLOCK as u64 {
                return Err(EngineError::Corrupt("checkpoint page length".into()));
            }
            let data = &all[at..at + p.len as usize];
            at += (blocks as usize) * BLOCK;
            if crc32c(data) != p.crc {
                return Err(EngineError::Corrupt("checkpoint page checksum".into()));
            }
            let mut d = Dec::new(data);
            if d.u64()? != PAGE_MAGIC {
                return Err(EngineError::Corrupt("checkpoint page magic".into()));
            }
            let end = bounds.get(i + 1).and_then(|b| b.as_ref());
            while next_object(&mut d)? {
                let (k, o) = crate::index::decode_object(&mut d)?;
                if bound.as_ref().is_some_and(|b| k < *b) || end.is_some_and(|e| k >= *e) {
                    return Err(EngineError::Corrupt(
                        "checkpoint object outside its page".into(),
                    ));
                }
                index.objects.insert(k, o);
            }
            d.finish()?;
        }
        index.bounds = pages.keys().flatten().cloned().collect();
        self.index = index;
        self.pages = pages;
        Ok(())
    }

    fn log_pos(&self, lsn: u64) -> u64 {
        (self.region.log_start() + lsn % self.region.log_blocks) * BLOCK as u64
    }

    /// Reads `n` log blocks starting at `lsn`, following the ring's wrap.
    fn read_log(&self, lsn: u64, n: u64) -> Result<AlignedBuf, EngineError> {
        let mut buf = AlignedBuf::zeroed(n as usize * BLOCK);
        let first = (self.region.log_blocks - lsn % self.region.log_blocks).min(n);
        let (a, b) = buf.split_at_mut(first as usize * BLOCK);
        let mut reads = vec![(self.log_pos(lsn), a)];
        if n > first {
            reads.push((self.log_pos(lsn + first), b));
        }
        self.dev.read_batch(&mut reads)?;
        Ok(buf)
    }

    /// Reads records from `head_lsn` until the first block that isn't the
    /// next valid record.
    fn scan_log(&self) -> Result<Vec<LogRecord>, EngineError> {
        let mut out = Vec::new();
        let mut lsn = self.head_lsn;
        while lsn - self.head_lsn < self.region.log_blocks {
            let hdr = self.read_log(lsn, 1)?;
            let mut d = Dec::new(&hdr);
            let parsed = (|| -> Result<_, DecodeError> {
                let magic = d.u32()?;
                let version = d.u16()?;
                let _flags = d.u16()?;
                let rlsn = d.u64()?;
                let group = d.u64()?;
                let nblocks = d.u32()? as u64;
                let plen = d.u32()? as usize;
                let pcrc = d.u32()?;
                let hcrc = d.u32()?;
                Ok((magic, version, rlsn, group, nblocks, plen, pcrc, hcrc))
            })();
            let Ok((magic, version, rlsn, group, nblocks, plen, pcrc, hcrc)) = parsed else {
                break;
            };
            if magic != LOG_MAGIC
                || version != LOG_VERSION
                || rlsn != lsn
                || crc32c_append(self.salt, &hdr[..HEADER_LEN - 4]) != hcrc
                || nblocks == 0
                || nblocks > self.region.log_blocks / 2
                || HEADER_LEN + plen > nblocks as usize * BLOCK
                || lsn + nblocks - self.head_lsn > self.region.log_blocks
            {
                break;
            }
            let full = if nblocks == 1 {
                hdr
            } else {
                self.read_log(lsn, nblocks)?
            };
            let payload = &full[HEADER_LEN..HEADER_LEN + plen];
            if crc32c_append(self.salt, payload) != pcrc {
                break;
            }
            let mut d = Dec::new(payload);
            let lists = match d.seq(8, RList::decode) {
                Ok(l) if d.finish().is_ok() => l,
                _ => {
                    return Err(EngineError::Corrupt(format!(
                        "log record at LSN {lsn} passed its checksums but doesn't decode"
                    )));
                }
            };
            out.push(LogRecord {
                lsn,
                group,
                nblocks,
                lists,
            });
            lsn += nblocks;
        }
        Ok(out)
    }

    /// Finds the first record of the last commit group whose new data
    /// didn't all reach the media. Only blocks the replayed index still
    /// maps to the same place are checked: a block mapped and freed within
    /// the group may since have been reused (by the next round, or by the
    /// checkpoint an earlier recovery wrote), and its contents don't matter.
    fn first_unwritten(
        &self,
        records: &[LogRecord],
        fx: &Effects,
    ) -> Result<Option<usize>, EngineError> {
        let Some(last) = records.last().map(|r| r.group) else {
            return Ok(None);
        };
        let first = records.iter().position(|r| r.group == last).unwrap();
        for (i, rec) in records.iter().enumerate().skip(first) {
            for l in &rec.lists {
                let Some(o) = self.index.objects.get(&l.key) else {
                    continue;
                };
                for op in &l.ops {
                    let ROp::Map { lb, pblock, crcs } = op else {
                        continue;
                    };
                    for k in 0..crcs.len() as u64 {
                        let Loc::Mapped { pblock: pb, crc } = o.lookup(lb + k) else {
                            continue;
                        };
                        if pb != pblock + k || fx.pending.contains_key(&pb) {
                            continue;
                        }
                        let mut buf = AlignedBuf::zeroed(BLOCK);
                        self.dev.read(pb * BLOCK as u64, &mut buf)?;
                        if crc32c(&buf) != crc {
                            return Ok(Some(i));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn check(&self) -> Result<(), EngineError> {
        if self.poisoned {
            Err(EngineError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Commits a batch of operation lists. Each list is atomic and sees the
    /// effects of the lists before it. The outer error means the shard
    /// failed (I/O) and is poisoned; inner errors refuse single lists.
    pub fn commit(
        &mut self,
        batch: Vec<OpList>,
    ) -> Result<Vec<Result<Committed, OpError>>, EngineError> {
        self.check()?;
        let mut results = Vec::with_capacity(batch.len());
        let mut rest: std::collections::VecDeque<OpList> = batch.into();
        while !rest.is_empty() {
            let r = self.commit_round(&mut rest);
            match r {
                Ok(mut v) => results.append(&mut v),
                Err(e) => {
                    self.poisoned = true;
                    return Err(e);
                }
            }
        }
        self.commits += 1;
        Ok(results)
    }

    /// Sets the size below which overwrites of held blocks are deferred
    /// (spec/decisions.md D22; spec/implementation-notes.md I14). 0 redirects
    /// every write to new extents.
    pub fn set_defer_max(&mut self, bytes: usize) {
        self.defer_max = bytes;
    }

    /// Sets when block maps switch to and from the dense representation
    /// (D43). Takes effect for later changes; existing maps keep theirs.
    pub fn set_dense_policy(&mut self, policy: crate::DensePolicy) {
        self.index.policy = policy;
    }

    /// The largest list bound ([`estimate`]) the shard accepts.
    pub fn max_list_bytes(&self) -> usize {
        self.max_payload()
    }

    fn max_payload(&self) -> usize {
        (self.region.log_blocks / 4) as usize * BLOCK - HEADER_LEN
    }

    fn commit_round(
        &mut self,
        lists: &mut std::collections::VecDeque<OpList>,
    ) -> Result<Vec<Result<Committed, OpError>>, EngineError> {
        if self.pending.len() > PENDING_LIMIT {
            self.apply_deferred()?;
        }
        // Fuzzy checkpoints (D43): start one when a quarter of the log is
        // used, write a slice of it each round, and finish it at once if
        // the log reaches three quarters first.
        let used = self.tail_lsn - self.head_lsn;
        if used > self.region.log_blocks * 3 / 4 {
            if self.fuzzy.is_some() {
                self.checkpoint_forced += 1;
            }
            self.checkpoint_inner()?;
        } else if self.fuzzy.is_some() {
            let slice = if used > self.region.log_blocks / 2 {
                self.slice_bytes.saturating_mul(8)
            } else {
                self.slice_bytes
            };
            self.checkpoint_slice(slice)?;
        } else if used > self.region.log_blocks / 4 {
            self.checkpoint_start()?;
            self.checkpoint_slice(self.slice_bytes)?;
        }
        let budget = self.max_payload();
        self.round_fresh.clear();
        let mut results = Vec::new();
        let mut payload = Enc::new();
        payload.u32(0);
        let mut count = 0u32;
        let mut writes: Vec<(u64, AlignedBuf)> = Vec::new();
        let mut fx = Effects {
            freed: Vec::new(),
            pending: std::mem::take(&mut self.pending),
            tentative: Default::default(),
        };
        while let Some(list) = lists.front() {
            let est = self.estimate(list);
            if est > budget {
                lists.pop_front();
                results.push(Err(OpError::TooLarge));
                continue;
            }
            if count > 0 && payload.len() + est > budget {
                break;
            }
            let list = lists.pop_front().unwrap();
            let mut others = Vec::new();
            match self.plan(list, &mut fx, &mut writes, &mut others) {
                Ok((rl, c)) => {
                    // Other objects a list changed (`Absorb`), in the same
                    // record, so the change is atomic.
                    for o in others.iter().chain(std::iter::once(&rl)) {
                        if !o.ops.is_empty() {
                            o.encode(&mut payload);
                            count += 1;
                        }
                    }
                    results.push(Ok(c));
                }
                Err(e) => results.push(Err(e)),
            }
        }
        self.pending = fx.pending;
        if count == 0 {
            return Ok(results);
        }
        payload.patch_u32(0, count);
        let group = self.next_group;
        self.next_group += 1;
        let rec = self.encode_record(group, payload.as_slice());
        let nblocks = (rec.len() / BLOCK) as u64;
        let lsn = self.tail_lsn;
        let mut io: Vec<(u64, &[u8])> = writes
            .iter()
            .map(|(pb, b)| (pb * BLOCK as u64, &b[..]))
            .collect();
        let first = (self.region.log_blocks - lsn % self.region.log_blocks).min(nblocks);
        let (a, b) = rec.split_at(first as usize * BLOCK);
        io.push((self.log_pos(lsn), a));
        if nblocks > first {
            io.push((self.log_pos(lsn + first), b));
        }
        self.dev.write_batch(&io)?;
        self.dev.flush()?;
        self.tail_lsn += nblocks;
        self.records += 1;
        for (s, n) in fx.freed {
            self.alloc.free(s, n);
        }
        Ok(results)
    }

    fn encode_record(&self, group: u64, payload: &[u8]) -> AlignedBuf {
        let nblocks = (HEADER_LEN + payload.len()).div_ceil(BLOCK);
        let mut buf = AlignedBuf::zeroed(nblocks * BLOCK);
        let mut e = Enc::with_capacity(HEADER_LEN);
        e.u32(LOG_MAGIC)
            .u16(LOG_VERSION)
            .u16(0)
            .u64(self.tail_lsn)
            .u64(group)
            .u32(nblocks as u32)
            .u32(payload.len() as u32)
            .u32(crc32c_append(self.salt, payload));
        let hcrc = crc32c_append(self.salt, e.as_slice());
        e.u32(hcrc);
        buf[..HEADER_LEN].copy_from_slice(e.as_slice());
        buf[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        buf
    }

    /// Validates one list, resolves its operations and applies them to the
    /// index. Validation happens before anything changes, so a refused list
    /// leaves no trace.
    fn plan(
        &mut self,
        list: OpList,
        fx: &mut Effects,
        writes: &mut Vec<(u64, AlignedBuf)>,
        others: &mut Vec<RList>,
    ) -> Result<(RList, Committed), OpError> {
        let key = list.key;
        let cur = self.index.objects.get(&key).map_or(0, |o| o.revision);
        let mut need = 0u64;
        let mut mutates = false;
        for op in &list.ops {
            match op {
                Op::AssertRevision(r) if *r != cur => {
                    return Err(OpError::AssertFailed { current: cur });
                }
                Op::AssertExists if cur == 0 => return Err(OpError::AssertFailed { current: 0 }),
                Op::AssertAbsent if cur != 0 => {
                    return Err(OpError::AssertFailed { current: cur });
                }
                Op::AssertRevision(_) | Op::AssertExists | Op::AssertAbsent => {}
                Op::Write { off, data } => {
                    if off % BLOCK as u64 != 0 || data.len() % BLOCK != 0 {
                        return Err(OpError::Unaligned);
                    }
                    need += (data.len() / BLOCK) as u64;
                    mutates = true;
                }
                Op::Zero { off, len } => {
                    if off % BLOCK as u64 != 0 || len % BLOCK as u64 != 0 {
                        return Err(OpError::Unaligned);
                    }
                    mutates = true;
                }
                Op::Absorb(src) if *src == key => return Err(OpError::SelfAbsorb),
                _ => mutates = true,
            }
        }
        if !mutates {
            return Ok((
                RList {
                    key,
                    ops: Vec::new(),
                },
                Committed { revision: cur },
            ));
        }
        if need > self.alloc.free_blocks() {
            return Err(OpError::NoSpace);
        }
        let mut rops = Vec::new();
        let step = |idx: &mut Index, fx: &mut Effects, rop: ROp, rops: &mut Vec<ROp>| {
            idx.apply_op(&key, &rop, fx);
            rops.push(rop);
        };
        // Whether the object exists at this point of the list: `Delete`
        // ends it and any other changing operation (re)creates it.
        let mut alive = cur != 0;
        for op in list.ops {
            match &op {
                Op::AssertRevision(_) | Op::AssertExists | Op::AssertAbsent => {}
                Op::Delete => alive = false,
                _ => alive = true,
            }
            match op {
                Op::AssertRevision(_) | Op::AssertExists | Op::AssertAbsent | Op::Touch => {}
                Op::Write { off, data } => {
                    if data.is_empty() {
                        continue;
                    }
                    let lb = off / BLOCK as u64;
                    let n = (data.len() / BLOCK) as u64;
                    let crcs: Vec<u32> = data.chunks(BLOCK).map(crc32c).collect();
                    let deferred = data.len() < self.defer_max
                        && self.index.objects.get(&key).is_some_and(|o| {
                            o.all_mapped(lb, n)
                                && (0..n).all(|i| match o.lookup(lb + i) {
                                    Loc::Mapped { pblock, .. } => {
                                        !in_ranges(&self.round_fresh, pblock)
                                    }
                                    Loc::Hole => false,
                                })
                        });
                    if deferred {
                        let o = self.index.objects.get(&key).expect("checked above");
                        let pbs = (0..n)
                            .map(|i| match o.lookup(lb + i) {
                                Loc::Mapped { pblock, .. } => pblock,
                                Loc::Hole => unreachable!("checked above"),
                            })
                            .collect();
                        step(
                            &mut self.index,
                            fx,
                            ROp::Deferred(Box::new(DeferredWrite {
                                lb,
                                data,
                                crcs,
                                pbs,
                            })),
                            &mut rops,
                        );
                    } else {
                        let runs = self.alloc.alloc(n).expect("space checked");
                        let mut done = 0usize;
                        for (pb, len) in runs {
                            self.round_fresh.insert(pb, len);
                            let len = len as usize;
                            writes.push((
                                pb,
                                AlignedBuf::from_slice(&data[done * BLOCK..(done + len) * BLOCK]),
                            ));
                            step(
                                &mut self.index,
                                fx,
                                ROp::Map {
                                    lb: lb + done as u64,
                                    pblock: pb,
                                    crcs: crcs[done..done + len].to_vec(),
                                },
                                &mut rops,
                            );
                            done += len;
                        }
                    }
                    let end = off + n * BLOCK as u64;
                    let size = self.index.objects.get(&key).map_or(0, |o| o.size);
                    if end > size {
                        step(&mut self.index, fx, ROp::SetSize(end), &mut rops);
                    }
                }
                Op::Zero { off, len } => step(
                    &mut self.index,
                    fx,
                    ROp::Unmap {
                        lb: off / BLOCK as u64,
                        n: len / BLOCK as u64,
                    },
                    &mut rops,
                ),
                Op::Truncate(size) => {
                    let lb = size.div_ceil(BLOCK as u64);
                    step(
                        &mut self.index,
                        fx,
                        ROp::Unmap {
                            lb,
                            n: u64::MAX - lb,
                        },
                        &mut rops,
                    );
                    step(&mut self.index, fx, ROp::SetSize(size), &mut rops);
                }
                Op::SetAttr(k, v) => step(&mut self.index, fx, ROp::SetAttr(k, v), &mut rops),
                Op::RmAttr(k) => step(&mut self.index, fx, ROp::RmAttr(k), &mut rops),
                Op::KvPut(k, v) => step(&mut self.index, fx, ROp::KvPut(k, v), &mut rops),
                Op::KvDel(k) => step(&mut self.index, fx, ROp::KvDel(k), &mut rops),
                Op::KvDelRange(a, b) => step(&mut self.index, fx, ROp::KvDelRange(a, b), &mut rops),
                Op::Delete => step(&mut self.index, fx, ROp::Delete, &mut rops),
                Op::Absorb(src) => {
                    let Some(s) = self.index.objects.get(&src) else {
                        continue;
                    };
                    let ssize = s.size;
                    let mine = self.index.objects.get(&key);
                    // The source's blocks this object lacks, as runs.
                    let mut moves: Vec<(u64, u64, Vec<u32>)> = Vec::new();
                    for (lb, pb, crcs) in s.blocks.runs() {
                        for (i, crc) in crcs.into_iter().enumerate() {
                            let (l, p) = (lb + i as u64, pb + i as u64);
                            if mine.is_some_and(|o| o.all_mapped(l, 1)) {
                                continue;
                            }
                            match moves.last_mut() {
                                Some((ml, mp, mc))
                                    if *ml + mc.len() as u64 == l && *mp + mc.len() as u64 == p =>
                                {
                                    mc.push(crc)
                                }
                                _ => moves.push((l, p, vec![crc])),
                            }
                        }
                    }
                    // The source gives the moved blocks up, then is deleted:
                    // only the blocks this object overrides are freed.
                    let mut sops: Vec<ROp> = moves
                        .iter()
                        .map(|(l, _, c)| ROp::Forget {
                            lb: *l,
                            n: c.len() as u64,
                        })
                        .collect();
                    sops.push(ROp::Delete);
                    for op in &sops {
                        self.index.apply_op(&src, op, fx);
                    }
                    others.push(RList {
                        key: src,
                        ops: sops,
                    });
                    for (lb, pblock, crcs) in moves {
                        step(
                            &mut self.index,
                            fx,
                            ROp::Map { lb, pblock, crcs },
                            &mut rops,
                        );
                    }
                    let size = self.index.objects.get(&key).map_or(0, |o| o.size);
                    if ssize > size {
                        step(&mut self.index, fx, ROp::SetSize(ssize), &mut rops);
                    }
                }
            }
        }
        // A list that ends with the object deleted leaves no revision; one
        // that ends with it existing (even a `Touch` of an absent object,
        // which resolves to nothing else) gives it the next revision, and
        // `SetRevision` creates it if needed.
        let revision = if alive {
            let r = self.next_revision;
            self.next_revision += 1;
            step(&mut self.index, fx, ROp::SetRevision(r), &mut rops);
            r
        } else {
            0
        };
        Ok((RList { key, ops: rops }, Committed { revision }))
    }

    /// Writes deferred data in place (without a flush: the log still holds
    /// it until the next checkpoint).
    pub fn apply_deferred(&mut self) -> Result<(), EngineError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        // Coalesce consecutive blocks into one write each.
        let mut bufs: Vec<(u64, AlignedBuf)> = Vec::new();
        let mut run: Option<(u64, Vec<u8>)> = None;
        for (pb, data) in pending {
            match &mut run {
                Some((start, v)) if *start + (v.len() / BLOCK) as u64 == pb => {
                    v.extend_from_slice(&data)
                }
                _ => {
                    if let Some((s, v)) = run.take() {
                        bufs.push((s, AlignedBuf::from_slice(&v)));
                    }
                    run = Some((pb, data.to_vec()));
                }
            }
        }
        if let Some((s, v)) = run {
            bufs.push((s, AlignedBuf::from_slice(&v)));
        }
        let io: Vec<(u64, &[u8])> = bufs
            .iter()
            .map(|(pb, b)| (pb * BLOCK as u64, &b[..]))
            .collect();
        if let Err(e) = self.dev.write_batch(&io) {
            self.poisoned = true;
            return Err(e.into());
        }
        Ok(())
    }

    /// Writes a checkpoint of the index and moves the log's head to its
    /// tail, so the whole ring is free again.
    pub fn checkpoint(&mut self) -> Result<(), EngineError> {
        self.check()?;
        let r = self.checkpoint_inner();
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    /// True while a fuzzy checkpoint is being written.
    pub fn checkpoint_in_progress(&self) -> bool {
        self.fuzzy.is_some()
    }

    /// Sets how many checkpoint bytes each commit round writes while a
    /// fuzzy checkpoint runs (tests make it small to get many slices). 0
    /// holds a checkpoint until the log forces it (benchmarks of replay).
    pub fn set_checkpoint_slice(&mut self, bytes: usize) {
        self.slice_bytes = bytes;
    }

    /// Finishes a checkpoint now: starts one if none is running, and
    /// writes every remaining slice.
    fn checkpoint_inner(&mut self) -> Result<(), EngineError> {
        if self.fuzzy.is_none() {
            self.checkpoint_start()?;
        }
        while self.fuzzy.is_some() {
            self.checkpoint_slice(usize::MAX)?;
        }
        Ok(())
    }

    /// Sets the size at which checkpoint pages are cut (tests make it
    /// small to get many pages).
    pub fn set_checkpoint_page(&mut self, bytes: usize) {
        self.page_bytes = bytes.max(1);
    }

    /// Starts a fuzzy checkpoint at the current log position: replay will
    /// start there, so everything before it must be in the checkpoint or
    /// durable in place (deferred writes are applied now, and flushed
    /// before the superblock names the checkpoint). It writes the pages
    /// changed since the last one began (D105): a page nothing changed
    /// since then holds what replay from here needs.
    fn checkpoint_start(&mut self) -> Result<(), EngineError> {
        self.apply_deferred()?;
        if !self.pages.contains_key(&None) {
            // A new shard: the first page is always written.
            self.index.mark(None);
        }
        let todo = self.index.take_dirty().into();
        self.fuzzy = Some(Fuzzy {
            start_lsn: self.tail_lsn,
            next_revision: self.next_revision,
            next_group: self.next_group,
            todo,
            cur: None,
            written: BTreeMap::new(),
            replaced: Vec::new(),
        });
        Ok(())
    }

    /// Encodes changed pages until about `budget` bytes are done, writing
    /// each piece as it is cut, and finishes the checkpoint once every
    /// changed page is written.
    fn checkpoint_slice(&mut self, budget: usize) -> Result<(), EngineError> {
        let mut f = self.fuzzy.take().expect("a checkpoint in progress");
        let r = self.write_pages(&mut f, budget);
        match r {
            Ok(true) => self.checkpoint_finish(f),
            Ok(false) => {
                self.fuzzy = Some(f);
                self.checkpoint_slices += 1;
                Ok(())
            }
            Err(e) => {
                self.fuzzy = Some(f);
                Err(e)
            }
        }
    }

    /// Returns whether every changed page is written.
    fn write_pages(&mut self, f: &mut Fuzzy, budget: usize) -> Result<bool, EngineError> {
        use std::ops::Bound;
        if budget == 0 {
            return Ok(false);
        }
        let mut spent = 0usize;
        loop {
            let mut c = match f.cur.take() {
                Some(c) => c,
                None => {
                    let Some(first) = f.todo.pop_front() else {
                        return Ok(true);
                    };
                    // A page dropped since it was marked: its range is the
                    // page before's, which the change marked too.
                    if first
                        .as_ref()
                        .is_some_and(|b| !self.index.bounds.contains(b))
                    {
                        continue;
                    }
                    let end = match &first {
                        None => self.index.bounds.iter().next(),
                        Some(b) => self
                            .index
                            .bounds
                            .range((Bound::Excluded(b), Bound::Unbounded))
                            .next(),
                    }
                    .cloned();
                    f.replaced.push(first.clone());
                    Cur {
                        piece: first.clone(),
                        first,
                        end,
                        last: None,
                        buf: page_head(),
                        objects: false,
                    }
                }
            };
            let lo = match (&c.last, &c.piece) {
                (Some(k), _) => Bound::Excluded(k.clone()),
                (None, Some(b)) => Bound::Included(b.clone()),
                (None, None) => Bound::Unbounded,
            };
            let hi = match &c.end {
                Some(e) => Bound::Excluded(e.clone()),
                None => Bound::Unbounded,
            };
            let mut cut = None;
            let mut done = true;
            for (k, o) in self.index.objects.range((lo, hi)) {
                if c.objects && c.buf.len() >= self.page_bytes {
                    cut = Some(k.clone());
                    done = false;
                    break;
                }
                if spent >= budget {
                    done = false;
                    break;
                }
                let before = c.buf.len();
                c.buf.u8(1);
                crate::index::encode_object(k, o, &mut c.buf);
                spent += c.buf.len() - before;
                c.objects = true;
                c.last = Some(k.clone());
            }
            if let Some(k) = cut {
                // Cut here: the rest of the page is a page of its own.
                self.write_piece(f, &mut c)?;
                self.index.bounds.insert(k.clone());
                c.piece = Some(k);
                // Continue from the cut, not the last object: a key added
                // before the cut belongs to the piece just written's page.
                c.last = None;
                c.buf = page_head();
                c.objects = false;
                f.cur = Some(c);
                continue;
            }
            if !done {
                f.cur = Some(c);
                return Ok(false);
            }
            if !c.objects && c.piece.is_some() && c.piece == c.first {
                // Empty: drop the page, and its range joins the page
                // before. Nothing is lost: the range is empty now, so
                // every change marked on it is reflected, and later ones
                // mark the page before.
                let b = c.first.clone().expect("not the first page");
                self.index.bounds.remove(&b);
                self.index.dirty.remove(&b);
            } else {
                self.write_piece(f, &mut c)?;
            }
            if spent >= budget {
                return Ok(f.todo.is_empty());
            }
        }
    }

    /// Writes the piece `c` holds as a page.
    fn write_piece(&mut self, f: &mut Fuzzy, c: &mut Cur) -> Result<(), EngineError> {
        let mut buf = std::mem::replace(&mut c.buf, Enc::new());
        buf.u8(0);
        let bytes = buf.into_vec();
        let crc = crc32c(&bytes);
        let nblocks = bytes.len().div_ceil(BLOCK) as u64;
        let runs = self.alloc.alloc(nblocks).ok_or(EngineError::NoSpace)?;
        let data = AlignedBuf::from_slice(&bytes);
        let mut io = Vec::new();
        let mut at = 0usize;
        for &(s, n) in &runs {
            io.push((
                s * BLOCK as u64,
                &data[at * BLOCK..(at + n as usize) * BLOCK],
            ));
            at += n as usize;
        }
        self.dev.write_batch(&io)?;
        self.ckpt_written += nblocks * BLOCK as u64;
        self.ckpt_pages_written += 1;
        f.written.insert(
            c.piece.clone(),
            PageLoc {
                runs,
                len: bytes.len() as u64,
                crc,
            },
        );
        Ok(())
    }

    /// Writes the directory, makes everything durable, and points the
    /// superblock at the new checkpoint with the log's head at its start;
    /// then frees the pages it replaced.
    fn checkpoint_finish(&mut self, f: Fuzzy) -> Result<(), EngineError> {
        let mut pages = self.pages.clone();
        let mut free = Vec::new();
        for p in &f.replaced {
            if let Some(old) = pages.remove(p) {
                free.extend(old.runs);
            }
        }
        pages.extend(f.written);
        debug_assert!(
            pages
                .keys()
                .cloned()
                .eq(std::iter::once(None).chain(self.index.bounds.iter().cloned().map(Some))),
            "checkpoint pages and index bounds differ"
        );
        let mut e = Enc::new();
        e.u64(DIR2_MAGIC)
            .u64(f.next_revision)
            .u64(f.next_group)
            .u64(f.start_lsn)
            .u64(pages.len() as u64);
        for (bound, p) in &pages {
            match bound {
                None => {
                    e.bool(false);
                }
                Some(k) => {
                    e.bool(true);
                    k.encode(&mut e);
                }
            }
            e.u64(p.len).u32(p.crc).seq(&p.runs, |e, (s, n)| {
                e.u64(*s).u64(*n);
            });
        }
        let dir = e.into_vec();
        let dir_crc = crc32c(&dir);
        let nblocks = dir.len().div_ceil(BLOCK) as u64;
        let dir_runs = match self.alloc.alloc(nblocks) {
            Some(r) if r.len() <= MAX_CKPT_RUNS => r,
            Some(r) => {
                for (s, n) in r {
                    self.alloc.free(s, n);
                }
                return Err(EngineError::NoSpace);
            }
            None => return Err(EngineError::NoSpace),
        };
        let buf = AlignedBuf::from_slice(&dir);
        let mut io = Vec::new();
        let mut done = 0usize;
        for &(s, n) in &dir_runs {
            io.push((
                s * BLOCK as u64,
                &buf[done * BLOCK..(done + n as usize) * BLOCK],
            ));
            done += n as usize;
        }
        self.dev.write_batch(&io)?;
        self.ckpt_written += nblocks * BLOCK as u64;
        // Pages, directory, and every deferred write applied since the
        // start become durable before the superblock refers to them.
        self.dev.flush()?;
        let ckpt = Ckpt {
            runs: dir_runs,
            len: dir.len() as u64,
            crc: dir_crc,
        };
        let sup = Super {
            seq: self.super_seq + 1,
            device_id: self.device_id,
            shard: self.shard,
            salt: self.salt,
            head_lsn: f.start_lsn,
            ckpt: Some(ckpt.clone()),
        };
        let slot = self.region.start + (sup.seq % 2);
        self.dev.write(slot * BLOCK as u64, &sup.encode())?;
        self.dev.flush()?;
        self.super_seq = sup.seq;
        self.head_lsn = f.start_lsn;
        if let Some(old) = self.ckpt.replace(ckpt) {
            free.extend(old.runs);
        }
        free.append(&mut self.legacy_blob);
        for (s, n) in free {
            self.alloc.free(s, n);
        }
        self.pages = pages;
        self.checkpoints += 1;
        Ok(())
    }

    /// Reads `[off, off + len)` of an object. Holes read as zeros; every
    /// mapped block's checksum is verified.
    pub fn read(&self, key: &ObjKey, off: u64, len: u64) -> Result<AlignedBuf, EngineError> {
        self.check()?;
        if !off.is_multiple_of(BLOCK as u64) || !len.is_multiple_of(BLOCK as u64) {
            return Err(EngineError::Unaligned);
        }
        let o = self.index.objects.get(key).ok_or(EngineError::NotFound)?;
        self.read_object(o, off, len)
    }

    fn read_object(&self, o: &Object, off: u64, len: u64) -> Result<AlignedBuf, EngineError> {
        self.read_with(off, len, |lb| match o.lookup(lb) {
            Loc::Mapped { pblock, crc } => Some((pblock, crc)),
            Loc::Hole => None,
        })
    }

    /// Reads `[off, off + len)` with `locate` saying where each logical
    /// block is (physical block and CRC; none: a hole), checking CRCs.
    fn read_with(
        &self,
        off: u64,
        len: u64,
        locate: impl Fn(u64) -> Option<(u64, u32)>,
    ) -> Result<AlignedBuf, EngineError> {
        let mut out = AlignedBuf::zeroed(len as usize);
        let lb0 = off / BLOCK as u64;
        let n = len / BLOCK as u64;
        // (first logical block, first physical block, count) of each run
        // to read from the device.
        let mut ranges: Vec<(u64, u64, u64)> = Vec::new();
        let mut crcs: Vec<(u64, u32)> = Vec::new();
        for i in 0..n {
            let lb = lb0 + i;
            if let Some((pblock, crc)) = locate(lb) {
                if let Some(d) = self.pending.get(&pblock) {
                    let at = (i as usize) * BLOCK;
                    out[at..at + BLOCK].copy_from_slice(d);
                    continue;
                }
                crcs.push((lb, crc));
                match ranges.last_mut() {
                    Some((l, p, c)) if *l + *c == lb && *p + *c == pblock => *c += 1,
                    _ => ranges.push((lb, pblock, 1)),
                }
            }
        }
        if !ranges.is_empty() {
            let mut reads: Vec<(u64, &mut [u8])> = Vec::with_capacity(ranges.len());
            let mut rest: &mut [u8] = &mut out;
            let mut at = lb0;
            for &(l, p, c) in &ranges {
                let skip = ((l - at) as usize) * BLOCK;
                let (_, tail) = rest.split_at_mut(skip);
                let (head, tail) = tail.split_at_mut(c as usize * BLOCK);
                reads.push((p * BLOCK as u64, head));
                rest = tail;
                at = l + c;
            }
            self.dev.read_batch(&mut reads)?;
            for (lb, crc) in crcs {
                let at = ((lb - lb0) as usize) * BLOCK;
                if crc32c(&out[at..at + BLOCK]) != crc {
                    return Err(EngineError::Checksum { lblock: lb });
                }
            }
        }
        Ok(out)
    }

    pub fn stat(&self, key: &ObjKey) -> Option<ObjStat> {
        self.index.objects.get(key).map(|o| ObjStat {
            size: o.size,
            revision: o.revision,
            blocks: o.block_count(),
        })
    }

    /// A digest of an object's content, from the index alone (no data is
    /// read): its size in blocks, each mapped block's number and CRC, its
    /// attributes and its key–value table. Two copies with equal digests
    /// read the same, so delta recovery and scrub compare digests
    /// (replication.md §8.5). A block of zeros counts as a hole, since a
    /// hole reads as zeros; FNV-1a keeps the digest the same on every node
    /// and version.
    pub fn digest(&self, key: &ObjKey) -> Option<u64> {
        let o = self.index.objects.get(key)?;
        let mut h = Fnv::new();
        h.u64(o.size.div_ceil(BLOCK as u64));
        for (lb, _, crcs) in o.blocks.runs() {
            for (i, c) in crcs.iter().enumerate() {
                if *c != zero_block_crc() {
                    h.u64(lb + i as u64).u64(u64::from(*c));
                }
            }
        }
        for table in [&o.attrs, &o.kv] {
            h.u64(table.len() as u64);
            for (k, v) in table {
                h.bytes(k).bytes(v);
            }
        }
        Some(h.0)
    }

    pub fn attrs(&self, key: &ObjKey) -> Option<&BTreeMap<Vec<u8>, Vec<u8>>> {
        self.index.objects.get(key).map(|o| &o.attrs)
    }

    pub fn kv_get(&self, key: &ObjKey, k: &[u8]) -> Option<&[u8]> {
        self.index
            .objects
            .get(key)
            .and_then(|o| o.kv.get(k))
            .map(|v| v.as_slice())
    }

    /// Up to `limit` keys and values of an object's table in
    /// `[start, end)` (no end: to the last key), in order.
    pub fn kv_scan(
        &self,
        key: &ObjKey,
        start: &[u8],
        end: Option<&[u8]>,
        limit: usize,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Some(o) = self.index.objects.get(key) else {
            return Vec::new();
        };
        use std::ops::Bound;
        let hi = match end {
            Some(e) => Bound::Excluded(e.to_vec()),
            None => Bound::Unbounded,
        };
        o.kv.range((Bound::Included(start.to_vec()), hi))
            .take(limit)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Keys of the objects in `[from, to)`.
    pub fn list(&self, from: &ObjKey, to: &ObjKey) -> Vec<ObjKey> {
        self.index
            .objects
            .range(from.clone()..to.clone())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Every object key in one PG, in order.
    pub fn list_pg(&self, pool: u32, pg: u32) -> Vec<ObjKey> {
        self.index
            .objects
            .keys()
            .filter(|k| k.pool == pool && k.pg == pg)
            .cloned()
            .collect()
    }

    pub fn stats(&self) -> ShardStats {
        ShardStats {
            objects: self.index.objects.len() as u64,
            free_blocks: self.alloc.free_blocks(),
            data_blocks: self.region.data_blocks(),
            log_used_blocks: self.tail_lsn - self.head_lsn,
            log_blocks: self.region.log_blocks,
            pending_blocks: self.pending.len() as u64,
            checkpoints: self.checkpoints,
            commits: self.commits,
            records: self.records,
            checkpoint_bytes: self.pages.values().map(|p| p.len).sum(),
            checkpoint_pages: self.pages.len() as u64,
            checkpoint_pages_written: self.ckpt_pages_written,
            checkpoint_directory_bytes: self.ckpt.as_ref().map_or(0, |c| c.len),
            checkpoint_slices: self.checkpoint_slices,
            checkpoint_forced: self.checkpoint_forced,
            log_blocks_written: self.tail_lsn - self.opened_lsn,
            checkpoint_bytes_written: self.ckpt_written,
        }
    }

    /// Totals over the whole index (walks every block map: not for the
    /// I/O path).
    pub fn index_stats(&self) -> IndexStats {
        let mut st = IndexStats::default();
        for o in self.index.objects.values() {
            st.runs += o.blocks.run_count();
            st.dense_objects += o.blocks.is_dense() as u64;
            st.map_bytes += o.blocks.mem_bytes();
        }
        st
    }

    /// Deferred writes not yet applied in place (cheap).
    pub fn pending_blocks(&self) -> u64 {
        self.pending.len() as u64
    }

    pub fn region(&self) -> Region {
        self.region
    }

    /// The index, for consistency checks in tests.
    pub fn index(&self) -> &Index {
        &self.index
    }
}

fn in_ranges(ranges: &BTreeMap<u64, u64>, x: u64) -> bool {
    ranges
        .range(..=x)
        .next_back()
        .is_some_and(|(&s, &n)| x < s + n)
}

/// An upper bound on a list's encoded size in a log record. A list whose
/// bound exceeds [`Shard::max_list_bytes`] is refused with
/// [`OpError::TooLarge`].
impl Shard {
    /// [`estimate`], with the source objects of `Absorb` as they are now.
    fn estimate(&self, list: &OpList) -> usize {
        let mut n = estimate(list);
        for op in &list.ops {
            if let Op::Absorb(src) = op
                && let Some(s) = self.index.objects.get(src)
            {
                // A Forget and a Map per run, a CRC per block.
                n += 64
                    + src.name.len()
                    + s.blocks.run_count() as usize * 48
                    + 4 * s.block_count() as usize;
            }
        }
        n
    }

    /// Whether object `key` holds logical block `lb` (a hole doesn't).
    pub fn holds(&self, key: &ObjKey, lb: u64) -> bool {
        self.index
            .objects
            .get(key)
            .is_some_and(|o| o.all_mapped(lb, 1))
    }

    /// The versions of the object `key` names (every version of its pool,
    /// PG, namespace and name), oldest first.
    pub fn versions(&self, key: &ObjKey) -> Vec<u64> {
        let lo = ObjKey {
            version: 0,
            ..key.clone()
        };
        let hi = ObjKey {
            version: u64::MAX,
            ..key.clone()
        };
        self.index
            .objects
            .range(lo..=hi)
            .map(|(k, _)| k.version)
            .collect()
    }

    /// Reads `[off, off + len)` through `layers`, the first first: each
    /// block comes from the first object that holds it, and reads as zeros
    /// if none does (a volume chunk read at an epoch through its versions
    /// and its clone's parents, engine.md §7.2). `NotFound` if no layer
    /// exists.
    pub fn read_layered(
        &self,
        layers: &[ObjKey],
        off: u64,
        len: u64,
    ) -> Result<AlignedBuf, EngineError> {
        self.check()?;
        if !off.is_multiple_of(BLOCK as u64) || !len.is_multiple_of(BLOCK as u64) {
            return Err(EngineError::Unaligned);
        }
        let objs: Vec<&Object> = layers
            .iter()
            .filter_map(|k| self.index.objects.get(k))
            .collect();
        if objs.is_empty() {
            return Err(EngineError::NotFound);
        }
        self.read_with(off, len, |lb| {
            objs.iter().find_map(|o| match o.lookup(lb) {
                Loc::Mapped { pblock, crc } => Some((pblock, crc)),
                Loc::Hole => None,
            })
        })
    }

    /// For each block of `[off, off + len)`, the index in `layers` of the
    /// object [`Self::read_layered`] reads it from; none for a hole. From
    /// the index alone.
    pub fn layer_sources(&self, layers: &[ObjKey], off: u64, len: u64) -> Vec<Option<usize>> {
        let objs: Vec<Option<&Object>> = layers.iter().map(|k| self.index.objects.get(k)).collect();
        let lb0 = off / BLOCK as u64;
        (lb0..lb0 + len / BLOCK as u64)
            .map(|lb| {
                objs.iter()
                    .position(|o| o.is_some_and(|o| o.all_mapped(lb, 1)))
            })
            .collect()
    }
}

pub fn estimate(list: &OpList) -> usize {
    let mut n = 64 + list.key.name.len();
    for op in &list.ops {
        n += 16
            + match op {
                Op::Write { data, .. } => {
                    let blocks = data.len() / BLOCK;
                    if data.len() < DEFER_MAX {
                        data.len() + 4 * blocks + 32
                    } else {
                        // Worst case: every block its own run.
                        blocks * (4 + 32)
                    }
                }
                Op::SetAttr(k, v) | Op::KvPut(k, v) | Op::KvDelRange(k, v) => k.len() + v.len(),
                Op::RmAttr(k) | Op::KvDel(k) => k.len(),
                _ => 16,
            };
    }
    n
}

/// The CRC32C of a block of zeros, which [`Shard::digest`] counts as a hole.
fn zero_block_crc() -> u32 {
    static CRC: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CRC.get_or_init(|| crc32c(&[0u8; BLOCK]))
}

/// FNV-1a, 64 bits: a fixed hash for [`Shard::digest`].
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, b: &[u8]) -> &mut Self {
        for x in b {
            self.0 = (self.0 ^ u64::from(*x)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.write(&v.to_le_bytes())
    }
    /// Length-prefixed, so `("ab", "c")` and `("a", "bc")` differ.
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u64(b.len() as u64).write(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::MemDev;
    use crate::types::Namespace;

    fn key(i: u64) -> ObjKey {
        ObjKey::new(1, 1, Namespace::Vol, format!("o{i:03}").into_bytes(), 0)
    }

    /// A device whose checkpoint is one blob (before D105) opens, and its
    /// first checkpoint replaces the blob with pages and frees it.
    #[test]
    fn a_checkpoint_from_before_pages_is_read_and_replaced() {
        let region = Region {
            start: 0,
            blocks: 4096,
            log_blocks: 256,
        };
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(4096 * BLOCK as u64));
        Shard::format(dev.clone(), region, 7, 0).unwrap();
        let mut sh = Shard::open(dev.clone(), region, 7, 0).unwrap();
        sh.set_checkpoint_page(512);
        for i in 0..40 {
            sh.commit(vec![OpList {
                key: key(i),
                ops: vec![
                    Op::Write {
                        off: 0,
                        data: vec![i as u8; BLOCK],
                    },
                    Op::KvPut(b"k".to_vec(), vec![i as u8; 64]),
                ],
            }])
            .unwrap();
        }
        sh.checkpoint().unwrap();
        let idx = sh.index.clone();
        // The same state as the old format: one blob and its directory.
        let mut e = Enc::new();
        e.u64(CKPT_MAGIC)
            .u64(sh.next_revision)
            .u64(sh.next_group)
            .u64(sh.head_lsn);
        for (k, o) in &sh.index.objects {
            e.u8(1);
            crate::index::encode_object(k, o, &mut e);
        }
        e.u8(0);
        let blob = e.into_vec();
        let runs = sh.alloc.alloc(blob.len().div_ceil(BLOCK) as u64).unwrap();
        assert_eq!(runs.len(), 1);
        dev.write(runs[0].0 * BLOCK as u64, &AlignedBuf::from_slice(&blob))
            .unwrap();
        let mut e = Enc::new();
        e.u64(DIR_MAGIC)
            .u64(blob.len() as u64)
            .u32(crc32c(&blob))
            .seq(&runs, |e, (s, n)| {
                e.u64(*s).u64(*n);
            });
        let dir = e.into_vec();
        let dir_runs = sh.alloc.alloc(1).unwrap();
        dev.write(dir_runs[0].0 * BLOCK as u64, &AlignedBuf::from_slice(&dir))
            .unwrap();
        let sup = Super {
            seq: sh.super_seq + 1,
            device_id: 7,
            shard: 0,
            salt: sh.salt,
            head_lsn: sh.head_lsn,
            ckpt: Some(Ckpt {
                runs: dir_runs,
                len: dir.len() as u64,
                crc: crc32c(&dir),
            }),
        };
        dev.write((region.start + sup.seq % 2) * BLOCK as u64, &sup.encode())
            .unwrap();
        dev.flush().unwrap();
        drop(sh);
        let sh = Shard::open(dev.clone(), region, 7, 0).unwrap();
        assert_eq!(sh.index, idx, "the blob reads as it was written");
        assert!(sh.legacy_blob.is_empty(), "the blob is freed");
        assert!(sh.stats().checkpoint_pages >= 1);
        drop(sh);
        let sh = Shard::open(dev, region, 7, 0).unwrap();
        assert_eq!(sh.index, idx, "and the pages that replace it too");
        let used: u64 = sh.index.used_extents().map(|e| e.1).sum::<u64>()
            + sh.pages
                .values()
                .flat_map(|p| &p.runs)
                .map(|r| r.1)
                .sum::<u64>()
            + sh.ckpt
                .as_ref()
                .map_or(0, |c| c.runs.iter().map(|r| r.1).sum());
        assert_eq!(
            sh.stats().free_blocks,
            region.data_blocks() - used,
            "no space is lost"
        );
        for i in 0..40 {
            let r = sh.read(&key(i), 0, BLOCK as u64).unwrap();
            assert!(r.iter().all(|b| *b == i as u8));
        }
    }
}
