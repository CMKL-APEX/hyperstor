//! A shard: one reactor's part of a device (spec/08-engine.md §7,
//! spec/01-implementation-notes.md I1).
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

use crate::align::{AlignedBuf, BLOCK, WriteBuf};
use crate::alloc::Allocator;
use crate::dev::BlockDev;
use crate::index::{DeferredWrite, Effects, Index, Loc, Object, RList, ROp};
use crate::types::{Committed, EngineError, ObjKey, ObjStat, Op, OpError, OpList};
use hyperstor_codec::{Dec, DecodeError, Enc, crc32c, crc32c_append};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

const SUPER_MAGIC: u64 = 0x4853_5348_5244_3031; // "HSSHRD01"
/// A superblock of the second shard format (D266, D267, D270): it also
/// names the checksum the record at its head chains to.
const SUPER_MAGIC2: u64 = 0x4853_5348_5244_3032; // "HSSHRD02"
const LOG_MAGIC: u32 = 0x4853_4c47; // "HSLG"
/// A checkpoint of one blob of objects (I33), read from devices written
/// before paged checkpoints.
const CKPT_MAGIC: u64 = 0x4853_434b_5054_3033; // "HSCKPT03"
const DIR_MAGIC: u64 = 0x4853_434b_4449_5231; // "HSCKDIR1"
/// A paged checkpoint's directory, and one page (D105).
const DIR2_MAGIC: u64 = 0x4853_434b_4449_5232; // "HSCKDIR2"
const PAGE_MAGIC: u64 = 0x4853_434b_5047_3031; // "HSCKPG01"
/// Where the second copy of a checkpoint's directory is (D269): after the
/// superblock's checksum, with a checksum of its own, so builds from before
/// it read the superblock as before and leave the copy alone.
const DIR_COPY_MAGIC: u64 = 0x4853_434b_4350_5931; // "HSCKCPY1"
/// A checkpoint page is cut once it holds this much: small enough that a
/// change rewrites little, large enough that the directory stays small
/// (about 100 bytes a page).
const PAGE_BYTES: usize = 128 * 1024;
/// Checkpoint bytes written per commit round while a fuzzy checkpoint runs
/// (64 KiB had the best tail latency, 02-benchmarks.md); eight times that once
/// half the log is used, so the checkpoint finishes before it is forced.
const SLICE_BYTES: usize = 64 * 1024;
const LOG_VERSION: u16 = 1;
const HEADER_LEN: usize = 40;
/// Records of the second shard format carry the payload checksum of the
/// record before them (D266), and a word kept zero.
const LOG_VERSION2: u16 = 2;
const HEADER_LEN2: usize = 48;
/// Blocks the second format takes from the end of the log ring for the
/// superblock mirrors (D267).
const MIRRORS: u64 = 2;
/// Writes smaller than this to blocks the object already holds go through
/// the log and are applied in place later (spec/04-decisions.md D22).
pub const DEFER_MAX: usize = 64 * 1024;
/// Objects with at least this many key–value entries and attributes keep
/// their digest until they change ([`Shard::digest`]).
pub const DIGEST_CACHE_MIN: usize = 1024;
/// At most this many digests are kept; past it the cache starts again.
const DIGEST_CACHE_MAX: usize = 4096;
/// Pending in-place writes above this many blocks are applied before the
/// next commit, to bound memory.
const PENDING_LIMIT: usize = 16 * 1024;
/// A discard batch takes at most this share of the free space (1/8).
const DISCARD_SHARE: u64 = 8;
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
    /// The second shard format: `chain` is the payload checksum of the
    /// record before `head_lsn`, which the record there names (D266).
    v2: bool,
    chain: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Ckpt {
    runs: Vec<(u64, u64)>,
    len: u64,
    crc: u32,
    /// The directory's second copy (D269): the same bytes, so the same
    /// length and checksum; empty when it has none.
    copy: Vec<(u64, u64)>,
}

impl Super {
    fn encode(&self) -> AlignedBuf {
        let mut e = Enc::with_capacity(BLOCK);
        e.u64(if self.v2 { SUPER_MAGIC2 } else { SUPER_MAGIC })
            .u64(self.seq)
            .u128(self.device_id)
            .u32(self.shard)
            .u32(self.salt)
            .u64(self.head_lsn);
        if self.v2 {
            e.u32(self.chain);
        }
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
        if let Some(c) = self.ckpt.as_ref().filter(|c| !c.copy.is_empty()) {
            let at = e.len();
            e.u64(DIR_COPY_MAGIC).seq(&c.copy, |e, (s, n)| {
                e.u64(*s).u64(*n);
            });
            let crc = crc32c(&e.as_slice()[at..]);
            e.u32(crc);
        }
        assert!(e.len() <= BLOCK);
        AlignedBuf::from_slice(e.as_slice())
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        let mut d = Dec::new(buf);
        let parse = |d: &mut Dec<'_>| -> Result<Super, DecodeError> {
            let v2 = match d.u64()? {
                SUPER_MAGIC => false,
                SUPER_MAGIC2 => true,
                _ => return Err(DecodeError::Magic(0)),
            };
            let seq = d.u64()?;
            let device_id = d.u128()?;
            let shard = d.u32()?;
            let salt = d.u32()?;
            let head_lsn = d.u64()?;
            let chain = if v2 { d.u32()? } else { 0 };
            let ckpt = if d.bool()? {
                let len = d.u64()?;
                let crc = d.u32()?;
                let runs = d.seq(16, |d| Ok((d.u64()?, d.u64()?)))?;
                Some(Ckpt {
                    runs,
                    len,
                    crc,
                    copy: Vec::new(),
                })
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
                v2,
                chain,
            })
        };
        let mut s = parse(&mut d).ok()?;
        let end = d.position();
        let crc = d.u32().ok()?;
        if crc32c(&buf[..end]) != crc {
            return None;
        }
        // The directory's copy, if any (D269). A damaged one is no copy:
        // the superblock itself is sound.
        let at = d.position();
        let copy = |d: &mut Dec<'_>| -> Result<Vec<(u64, u64)>, DecodeError> {
            if d.u64()? != DIR_COPY_MAGIC {
                return Err(DecodeError::Magic(0));
            }
            let runs = d.seq(16, |d| Ok((d.u64()?, d.u64()?)))?;
            let end = d.position();
            (crc32c(&buf[at..end]) == d.u32()?)
                .then_some(runs)
                .ok_or(DecodeError::Checksum)
        };
        if let (Some(c), Ok(runs)) = (s.ckpt.as_mut(), copy(&mut d)) {
            c.copy = runs;
        }
        Some(s)
    }
}

/// Where a checkpoint page is (D105).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PageLoc {
    runs: Vec<(u64, u64)>,
    len: u64,
    crc: u32,
}

/// Freed extents on their way to a discard (D275), by generation: `next`
/// (freed since the running checkpoint began) moves to `sealing` when one
/// begins, and `sealing` to `ready` once it is named, so discards go out
/// in batches after each checkpoint, merged into long runs. A block is
/// freed only once the commit that frees it is durable, and recovery
/// drops only a commit never acknowledged, so a freed block is never
/// needed again; what keeps a discard safe is that only the parts still
/// free are discarded ([`Shard::take_discards`]). Each is merged runs,
/// start to length, in blocks.
#[derive(Default)]
struct Discards {
    next: BTreeMap<u64, u64>,
    sealing: BTreeMap<u64, u64>,
    ready: BTreeMap<u64, u64>,
}

/// Adds `[s, s + n)` to merged runs.
fn add_run(runs: &mut BTreeMap<u64, u64>, mut s: u64, mut n: u64) {
    if n == 0 {
        return;
    }
    if let Some((&ps, &pn)) = runs.range(..=s).next_back()
        && ps + pn >= s
    {
        n = (s + n).max(ps + pn) - ps;
        s = ps;
        runs.remove(&ps);
    }
    while let Some((&ns, &nn)) = runs.range(s..).next()
        && ns <= s + n
    {
        n = (s + n).max(ns + nn) - s;
        runs.remove(&ns);
    }
    runs.insert(s, n);
}

fn merge_runs(into: &mut BTreeMap<u64, u64>, from: BTreeMap<u64, u64>) {
    for (s, n) in from {
        add_run(into, s, n);
    }
}

/// Where a checkpoint is on the device ([`Shard::checkpoint_layout`]), as
/// runs of blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckpointLayout {
    pub directory: Vec<(u64, u64)>,
    /// The directory's second copy (D269); empty without one.
    pub directory_copy: Vec<(u64, u64)>,
    pub pages: Vec<Vec<(u64, u64)>>,
}

/// A fuzzy checkpoint being written (01-implementation-notes.md I33): only
/// the pages changed since the last one began (D105).
struct Fuzzy {
    /// Log position replay will start from once this checkpoint is named,
    /// and the checksum the record there chains to (D266).
    start_lsn: u64,
    start_chain: u32,
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

/// A checkpoint whose pages and directory are written and whose
/// superblock is next: its writer writes and flushes the superblock once
/// the round's writes are durable, and [`Shard::seal_done`] then moves the
/// log's head and frees what it replaced.
struct Seal {
    /// The superblock, by byte offset: its slot, then in the second format
    /// its mirror (D267).
    sup: Vec<(u64, AlignedBuf)>,
    seq: u64,
    head_lsn: u64,
    ckpt: Ckpt,
    pages: BTreeMap<Option<ObjKey>, PageLoc>,
    /// Extents of the pages it replaced, freed once it is named.
    free: Vec<(u64, u64)>,
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
    /// The payload checksum, which the next record chains to (D266).
    pcrc: u32,
}

/// A log record's header, past its magic, version, LSN and checksum.
struct Header {
    group: u64,
    nblocks: u64,
    plen: usize,
    pcrc: u32,
    /// The payload checksum of the record before (D266); 0 in the first
    /// format.
    prev: u32,
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
    /// Damaged checkpoint pages and directories found by scrub since
    /// opened, each rewritten by a checkpoint (D269).
    pub checkpoint_damage: u64,
    /// Freed blocks waiting for a discard, and blocks discarded since
    /// opened (D275).
    pub discard_queued_blocks: u64,
    pub discarded_blocks: u64,
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
/// One round of a commit, planned by [`Shard::commit_begin`] and waiting
/// for its writes (I199).
pub struct Round {
    /// Data extents and the log record, by byte offset on the device.
    writes: Vec<(u64, WriteBuf)>,
    /// Log blocks the record takes; 0 when nothing in the round changed.
    nblocks: u64,
    freed: Vec<(u64, u64)>,
    results: Vec<Result<Committed, OpError>>,
    /// A checkpoint the round completes, named after its writes.
    seal: Option<Seal>,
}

impl Round {
    /// The writes to make, by byte offset, which the caller then owns: an
    /// asynchronous writer keeps them until the device is done with them.
    /// They include the shard's upkeep (checkpoint pages, deferred writes
    /// applied in place), so that no commit round writes in the shard's
    /// own device calls. Empty when the round changed nothing, and then
    /// nothing is flushed.
    pub fn take_writes(&mut self) -> Vec<(u64, WriteBuf)> {
        std::mem::take(&mut self.writes)
    }

    /// The superblock of a checkpoint the round completes, by byte
    /// offset (its slot, then its mirror in the second format, D267):
    /// written and flushed only once the writes above are durable, before
    /// [`Shard::commit_end`].
    pub fn take_seal(&mut self) -> Option<Vec<(u64, AlignedBuf)>> {
        let s = self.seal.as_mut()?;
        Some(std::mem::take(&mut s.sup))
    }
}

pub struct Shard {
    dev: Arc<dyn BlockDev>,
    region: Region,
    /// The second shard format (D266, D267, D270): chained log records,
    /// superblock mirrors at the end of the log ring, dense block maps
    /// relative to the data start.
    v2: bool,
    /// Blocks of the log ring that hold records: the region's, less the
    /// mirrors in the second format.
    ring: u64,
    /// The payload checksum of the last record written, which the next one
    /// names (D266).
    chain: u32,
    device_id: u128,
    shard: u32,
    index: Index,
    alloc: Allocator,
    /// Deferred in-place writes not yet applied (physical block -> data).
    pending: BTreeMap<u64, Box<[u8]>>,
    /// Deferred writes being applied in the round in flight, read from
    /// here until they are on the device.
    applying: BTreeMap<u64, Box<[u8]>>,
    /// More deferred blocks than this are applied in the next round.
    pending_limit: usize,
    /// Writes upkeep made, which the next round's writer makes, or
    /// [`Shard::write_staged`] in the shard's own device calls: deferred
    /// blocks in place, and the checkpoint in progress's pages and
    /// directory (dropped with it if it is abandoned).
    staged: Vec<(u64, AlignedBuf)>,
    staged_ckpt: Vec<(u64, AlignedBuf)>,
    /// A checkpoint ready to be named.
    sealing: Option<Seal>,
    salt: u32,
    super_seq: u64,
    head_lsn: u64,
    tail_lsn: u64,
    next_group: u64,
    next_revision: u64,
    ckpt: Option<Ckpt>,
    poisoned: bool,
    /// A round begun by [`Shard::commit_begin`] waits for its writes.
    in_flight: bool,
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
    /// Digests of large objects at the revision they were taken
    /// ([`Shard::digest`]): hashing a table of hundreds of megabytes takes
    /// the reactor for most of a second, and backfill and scrub ask again
    /// and again while it doesn't change.
    digests: std::cell::RefCell<HashMap<ObjKey, (u64, u64)>>,
    page_bytes: usize,
    ckpt_pages_written: u64,
    /// A checkpoint found no room and was abandoned: changes that take
    /// space are refused until one completes (it is retried once the room
    /// it needs is free).
    ckpt_stalled: bool,
    /// Thousandths of the data region past which changes that grow the
    /// used space are refused (04-decisions.md D27); 0: none.
    full_ratio: u16,
    /// A fuzzy checkpoint in progress.
    fuzzy: Option<Fuzzy>,
    slice_bytes: usize,
    opened_lsn: u64,
    ckpt_written: u64,
    /// Opening read the checkpoint directory's second copy (D269).
    directory_copy_read: bool,
    /// Damaged checkpoint pages and directories scrub found (D269).
    checkpoint_damage: u64,
    /// Freed blocks to discard (D275); `None`: off.
    discards: Option<Discards>,
    /// Blocks discarded since opened.
    discarded: u64,
    checkpoint_slices: u64,
    checkpoint_forced: u64,
    checkpoints: u64,
    commits: u64,
    records: u64,
    /// Reads in flight on the device ([`Shard::read_begin`]), by the
    /// generation they began in: extents freed since may still be read,
    /// so they wait in `held` until those reads end (D190).
    reads: BTreeMap<u64, usize>,
    read_gen: u64,
    /// Extents freed while reads were in flight, by the generation they
    /// were freed in.
    held: std::collections::VecDeque<(u64, Vec<(u64, u64)>)>,
}

/// A read planned by [`Shard::read_begin`]: the device reads to make into
/// its buffer, then [`ReadPlan::finish`] checks their checksums. The shard
/// keeps the extents it reads until [`Shard::read_end`].
pub struct ReadPlan {
    generation: u64,
    out: AlignedBuf,
    lb0: u64,
    /// (byte offset on the device, offset in `out`, length) of each run.
    ios: Vec<(u64, usize, usize)>,
    crcs: Vec<(u64, u32)>,
}

impl ReadPlan {
    /// The device reads to make, as (byte offset on the device, offset in
    /// the buffer, length); empty when every block was a hole or in
    /// memory.
    pub fn ios(&self) -> &[(u64, usize, usize)] {
        &self.ios
    }

    /// Takes the buffer the reads land in, for a caller that hands it to
    /// the device and gives it back to [`ReadPlan::finish`].
    pub fn take_buf(&mut self) -> AlignedBuf {
        std::mem::replace(&mut self.out, AlignedBuf::zeroed(0))
    }

    /// The data, once the device reads into `buf` are done, with every
    /// block read from the device checked against its CRC.
    pub fn finish(self, buf: AlignedBuf) -> Result<AlignedBuf, EngineError> {
        for &(lb, crc) in &self.crcs {
            let at = ((lb - self.lb0) as usize) * BLOCK;
            if crc32c(&buf[at..at + BLOCK]) != crc {
                return Err(EngineError::Checksum { lblock: lb });
            }
        }
        Ok(buf)
    }
}

/// Whether `blk` starts like a shard superblock of either format: a device
/// whose own superblock copies are gone still has its shards (D271).
pub(crate) fn holds_super(blk: &[u8]) -> bool {
    blk.get(..8)
        .map(|m| u64::from_le_bytes(m.try_into().unwrap()))
        .is_some_and(|m| m == SUPER_MAGIC || m == SUPER_MAGIC2)
}

/// Parses a record header of the second format (`v2`) or the first: its
/// LSN, header checksum and the rest. Neither checksum is checked.
fn parse_header(blk: &[u8], v2: bool) -> Option<(u64, u32, Header)> {
    let mut d = Dec::new(blk);
    let r = (|| -> Result<_, DecodeError> {
        let magic = d.u32()?;
        let version = d.u16()?;
        let _flags = d.u16()?;
        let rlsn = d.u64()?;
        let group = d.u64()?;
        let nblocks = d.u32()? as u64;
        let plen = d.u32()? as usize;
        let pcrc = d.u32()?;
        let prev = if v2 {
            let prev = d.u32()?;
            let _zero = d.u32()?;
            prev
        } else {
            0
        };
        let hcrc = d.u32()?;
        let h = Header {
            group,
            nblocks,
            plen,
            pcrc,
            prev,
        };
        Ok((magic, version, rlsn, hcrc, h))
    })();
    let (magic, version, rlsn, hcrc, h) = r.ok()?;
    let want = if v2 { LOG_VERSION2 } else { LOG_VERSION };
    (magic == LOG_MAGIC && version == want).then_some((rlsn, hcrc, h))
}

/// Blocks of a region's log ring that hold records (D267).
fn ring_blocks(region: &Region, v2: bool) -> u64 {
    region.log_blocks - if v2 { MIRRORS } else { 0 }
}

/// Byte offsets of superblock `seq`: its slot, and in the second format its
/// mirror at the end of the log ring (D267).
fn super_slots(region: &Region, v2: bool, seq: u64) -> Vec<u64> {
    let mut at = vec![(region.start + seq % 2) * BLOCK as u64];
    if v2 {
        at.push((region.log_start() + ring_blocks(region, true) + seq % 2) * BLOCK as u64);
    }
    at
}

/// The seed `crc` was computed from over `data`. CRC32C is affine in its
/// seed, so the seed solves 32 equations over GF(2); `None` if none does.
fn crc_seed(data: &[u8], crc: u32) -> Option<u32> {
    let base = crc32c_append(0, data);
    // By leading bit: what a combination of seed bits changes, and the
    // combination.
    let mut basis: [Option<(u32, u32)>; 32] = [None; 32];
    let reduce = |basis: &[Option<(u32, u32)>; 32], mut v: u32, mut c: u32| {
        for bit in (0..32).rev() {
            if v >> bit & 1 == 1
                && let Some((bv, bc)) = basis[bit]
            {
                v ^= bv;
                c ^= bc;
            }
        }
        (v, c)
    };
    for i in 0..32 {
        let (v, c) = reduce(&basis, crc32c_append(1 << i, data) ^ base, 1 << i);
        if v != 0 {
            basis[31 - v.leading_zeros() as usize] = Some((v, c));
        }
    }
    let (v, c) = reduce(&basis, crc ^ base, 0);
    (v == 0).then_some(c)
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
    /// Formats an empty shard in `region`, in the second format.
    pub fn format(
        dev: Arc<dyn BlockDev>,
        region: Region,
        device_id: u128,
        shard: u32,
    ) -> Result<(), EngineError> {
        Self::format_as(dev, region, device_id, shard, true)
    }

    /// Formats an empty shard in `region`, in the second format (`v2`) or
    /// the first.
    pub fn format_as(
        dev: Arc<dyn BlockDev>,
        region: Region,
        device_id: u128,
        shard: u32,
        v2: bool,
    ) -> Result<(), EngineError> {
        let s = Super {
            seq: 1,
            device_id,
            shard,
            salt: random_u32(),
            head_lsn: 0,
            ckpt: None,
            v2,
            chain: 0,
        };
        let buf = s.encode();
        let zero = AlignedBuf::zeroed(BLOCK);
        // In the slot its number names, as every later superblock is, so
        // the first checkpoint's goes beside it instead of over it.
        let mut io = vec![];
        for (at, b) in super_slots(&region, v2, s.seq)
            .into_iter()
            .zip([&buf, &buf])
        {
            io.push((at, &b[..]));
        }
        for at in super_slots(&region, v2, s.seq + 1) {
            io.push((at, &zero[..]));
        }
        io.push((region.log_start() * BLOCK as u64, &zero[..]));
        dev.write_batch(&io)?;
        dev.flush()?;
        Ok(())
    }

    /// Opens a shard of the second format: recovers it from its checkpoint
    /// and log, then writes a fresh checkpoint under a new salt.
    pub fn open(
        dev: Arc<dyn BlockDev>,
        region: Region,
        device_id: u128,
        shard: u32,
    ) -> Result<Self, EngineError> {
        Self::open_as(dev, region, device_id, shard, true)
    }

    /// Opens a shard of the second format (`v2`) or the first.
    pub fn open_as(
        dev: Arc<dyn BlockDev>,
        region: Region,
        device_id: u128,
        shard: u32,
        v2: bool,
    ) -> Result<Self, EngineError> {
        // Both slots, and in the second format their mirrors (D267).
        let at: Vec<u64> = [1, 2]
            .into_iter()
            .flat_map(|seq| super_slots(&region, v2, seq))
            .collect();
        let mut bufs: Vec<AlignedBuf> = at.iter().map(|_| AlignedBuf::zeroed(BLOCK)).collect();
        let mut reads: Vec<(u64, &mut [u8])> = at
            .iter()
            .copied()
            .zip(bufs.iter_mut().map(|b| &mut b[..]))
            .collect();
        dev.read_batch(&mut reads)?;
        let supers: Vec<Super> = bufs
            .iter()
            .filter_map(|b| Super::decode(b))
            .filter(|s| s.device_id == device_id && s.shard == shard && s.v2 == v2)
            .collect();
        let unreadable = supers.len() < at.len();
        let sup = supers
            .into_iter()
            .max_by_key(|s| s.seq)
            // Formatting writes every shard before the device superblock,
            // so a shard without one is damaged, not blank.
            .ok_or_else(|| EngineError::Corrupt(format!("shard {shard}: no valid superblock")))?;

        let mut sh = Shard {
            dev,
            region,
            v2,
            ring: ring_blocks(&region, v2),
            chain: sup.chain,
            device_id,
            shard,
            index: Index::default(),
            alloc: Allocator::default(),
            pending: BTreeMap::new(),
            applying: BTreeMap::new(),
            pending_limit: PENDING_LIMIT,
            staged: Vec::new(),
            staged_ckpt: Vec::new(),
            sealing: None,
            salt: sup.salt,
            super_seq: sup.seq,
            head_lsn: sup.head_lsn,
            tail_lsn: sup.head_lsn,
            next_group: 1,
            next_revision: 1,
            digests: Default::default(),
            ckpt: sup.ckpt.clone(),
            poisoned: false,
            in_flight: false,
            round_fresh: BTreeMap::new(),
            defer_max: DEFER_MAX,
            pages: BTreeMap::new(),
            legacy_blob: Vec::new(),
            page_bytes: PAGE_BYTES,
            ckpt_pages_written: 0,
            ckpt_stalled: false,
            full_ratio: 0,
            fuzzy: None,
            slice_bytes: SLICE_BYTES,
            opened_lsn: 0,
            ckpt_written: 0,
            directory_copy_read: false,
            checkpoint_damage: 0,
            discards: None,
            discarded: 0,
            checkpoint_slices: 0,
            checkpoint_forced: 0,
            checkpoints: 0,
            commits: 0,
            records: 0,
            reads: BTreeMap::new(),
            read_gen: 0,
            held: Default::default(),
        };
        if let Some(c) = &sup.ckpt {
            sh.load_checkpoint(c)?;
        }
        sh.index.base = sh.dense_base();
        let mut records = sh.scan_log()?;
        // Records are written only under a salt a durable superblock names,
        // so a whole record under another salt where the log stops means the
        // newer superblock was lost, not torn (D267).
        let stop = records.last().map_or(sh.head_lsn, |r| r.lsn + r.nblocks);
        if unreadable && sh.head_lsn + sh.ring > stop && sh.foreign_record(stop)? {
            return Err(EngineError::Corrupt(format!(
                "shard {shard}: a superblock is unreadable and the log at LSN {stop} \
                 holds a record its salt doesn't name"
            )));
        }
        let (mut base_index, base_rev, base_group) = (
            std::mem::take(&mut sh.index),
            sh.next_revision,
            sh.next_group,
        );
        // Replay, then check the last group's new data; if some didn't reach
        // the media, drop that record and everything after it, and replay
        // again from the checkpoint. The checkpoint's index is copied only
        // for a replay, which may need it again: a copy of a large index
        // takes seconds (I188).
        let mut dropped = Vec::new();
        let mut fx = loop {
            sh.index = if records.is_empty() {
                std::mem::take(&mut base_index)
            } else {
                base_index.clone()
            };
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
                Some(i) => dropped.extend(records.drain(i..).map(|r| r.lsn)),
            }
        };
        // The next record names the last one kept (D266).
        sh.chain = records.last().map_or(sup.chain, |r| r.pcrc);
        sh.index.settle_tentative(&mut fx);
        sh.pending = fx.pending;
        sh.rebuild_allocator()?;
        sh.apply_deferred()?;
        // Records dropped above would replay under the old salt if the
        // shard keeps it, and look like damage (D266, D267) otherwise.
        if !dropped.is_empty() {
            let zero = AlignedBuf::zeroed(BLOCK);
            let io: Vec<(u64, &[u8])> = dropped
                .iter()
                .map(|l| (sh.log_pos(*l), &zero[..]))
                .collect();
            sh.dev.write_batch(&io)?;
            sh.dev.flush()?;
        }
        let salt = sh.salt;
        sh.salt = random_u32();
        if let Err(e) = sh.checkpoint() {
            if !matches!(e, EngineError::NoSpace) {
                return Err(e);
            }
            // A full shard opens without its fresh checkpoint: it keeps
            // the salt its superblock names, so the records it writes
            // replay.
            sh.poisoned = false;
            sh.salt = salt;
        }
        sh.opened_lsn = sh.tail_lsn;
        sh.ckpt_written = 0;
        sh.ckpt_pages_written = 0;
        Ok(sh)
    }

    /// Converts a shard of the first format to the second (D266, D267,
    /// D270; D274): its log emptied by a checkpoint, the ring gives its last
    /// two blocks to the superblock mirrors, dense block maps count from the
    /// data start, and a checkpoint of every page under a new salt names it
    /// all. Until the device's superblock carries the flag (see
    /// [`crate::upgrade`]), a crash leaves the shard as it was: the first
    /// format's open reads neither the new superblock nor the new records,
    /// and the pages the old checkpoint names are untouched. On an error the
    /// shard is poisoned; opened again, it is in the first format.
    pub fn upgrade(&mut self) -> Result<(), EngineError> {
        self.check()?;
        if self.v2 {
            return Ok(());
        }
        if self.head_lsn != self.tail_lsn || self.fuzzy.is_some() {
            self.checkpoint()?;
        }
        if self.head_lsn != self.tail_lsn {
            return Err(EngineError::NoSpace);
        }
        self.v2 = true;
        self.ring = ring_blocks(&self.region, true);
        self.index.rebase(self.region.data_start());
        self.index.mark(None);
        for b in self.index.bounds.clone() {
            self.index.mark(Some(b));
        }
        self.salt = random_u32();
        self.chain = 0;
        let r = self.checkpoint();
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    /// Gives the region's last block up to the device, for its backup
    /// superblock (D271), if nothing uses it: the shard's region is then one
    /// block shorter, which the device superblock records. Whether it was
    /// free.
    pub fn give_up_last_block(&mut self) -> bool {
        let last = self.region.start + self.region.blocks - 1;
        if self.region.data_blocks() < 2 || !self.alloc.reserve(last, 1) {
            return false;
        }
        self.region.blocks -= 1;
        true
    }

    /// Whether the shard is in the second format (D266, D267, D270).
    pub fn is_v2(&self) -> bool {
        self.v2
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
        // The directory's copy last, and only if its blocks are free: a
        // build from before copies reads past it, and may have written
        // over it before this one opened the device again (D269).
        if let Some(c) = self.ckpt.as_mut() {
            let mut taken = Vec::new();
            for &(s, n) in &c.copy {
                if !a.reserve(s, n) {
                    for &(s, n) in &taken {
                        a.free(s, n);
                    }
                    taken.clear();
                    c.copy.clear();
                    break;
                }
                taken.push((s, n));
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
        let dir = match self.read_directory(&c.runs, c) {
            Some(d) => d,
            // The second copy (D269), when the first can't be read or
            // fails its checksum; the fresh checkpoint every open writes
            // replaces both.
            None if !c.copy.is_empty() => {
                self.directory_copy_read = true;
                self.read_directory(&c.copy, c)
                    .ok_or_else(|| EngineError::Corrupt("checkpoint directory checksum".into()))?
            }
            None => return Err(EngineError::Corrupt("checkpoint directory checksum".into())),
        };
        let dir = &dir[..c.len as usize];
        let mut d = Dec::new(dir);
        match d.u64()? {
            DIR_MAGIC => self.load_blob(&mut d),
            DIR2_MAGIC => self.load_pages(&mut d),
            _ => Err(EngineError::Corrupt("checkpoint directory magic".into())),
        }
    }

    /// One copy of a checkpoint's directory at `runs`, if it reads and
    /// matches the superblock's checksum.
    fn read_directory(&self, runs: &[(u64, u64)], c: &Ckpt) -> Option<AlignedBuf> {
        let dir = self.read_extents(runs, c.len).ok()?;
        (crc32c(&dir[..c.len as usize]) == c.crc).then_some(dir)
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
            let (k, o) = crate::index::decode_object(&mut d, 0)?;
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
                let (k, o) = crate::index::decode_object(&mut d, self.dense_base())?;
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
        (self.region.log_start() + lsn % self.ring) * BLOCK as u64
    }

    /// Reads `n` log blocks starting at `lsn`, following the ring's wrap.
    fn read_log(&self, lsn: u64, n: u64) -> Result<AlignedBuf, EngineError> {
        let mut buf = AlignedBuf::zeroed(n as usize * BLOCK);
        self.read_log_into(lsn, &mut buf)?;
        Ok(buf)
    }

    /// Fills `buf` with log blocks starting at `lsn`.
    fn read_log_into(&self, lsn: u64, buf: &mut [u8]) -> Result<(), EngineError> {
        let n = (buf.len() / BLOCK) as u64;
        let first = (self.ring - lsn % self.ring).min(n);
        let (a, b) = buf.split_at_mut(first as usize * BLOCK);
        let mut reads = vec![(self.log_pos(lsn), a)];
        if n > first {
            reads.push((self.log_pos(lsn + first), b));
        }
        self.dev.read_batch(&mut reads)?;
        Ok(())
    }

    /// Parses the header in `blk` if it is a record header of the shard's
    /// format written at `lsn` under the current salt.
    fn header(&self, blk: &[u8], lsn: u64) -> Option<Header> {
        let (rlsn, hcrc, h) = parse_header(blk, self.v2)?;
        (rlsn == lsn && crc32c_append(self.salt, &blk[..self.hlen() - 4]) == hcrc).then_some(h)
    }

    /// The length of a record header in the shard's format.
    fn hlen(&self) -> usize {
        if self.v2 { HEADER_LEN2 } else { HEADER_LEN }
    }

    /// What dense block maps count from (D270).
    fn dense_base(&self) -> u64 {
        if self.v2 { self.region.data_start() } else { 0 }
    }

    /// Reads records from `head_lsn` until the first block that isn't the
    /// next valid record. A valid record header after that block means the
    /// block was damaged, not torn (D266): only the last round can be torn,
    /// and its record is the last one written.
    fn scan_log(&self) -> Result<Vec<LogRecord>, EngineError> {
        let mut out = Vec::new();
        let mut lsn = self.head_lsn;
        let mut chain = self.chain;
        while lsn - self.head_lsn < self.ring {
            let hdr = self.read_log(lsn, 1)?;
            let Some(h) = self.header(&hdr, lsn) else {
                break;
            };
            let nblocks = h.nblocks;
            if nblocks == 0
                || nblocks > self.ring / 2
                || self.hlen() + h.plen > nblocks as usize * BLOCK
                || lsn + nblocks - self.head_lsn > self.ring
            {
                break;
            }
            let full = if nblocks == 1 {
                hdr
            } else {
                self.read_log(lsn, nblocks)?
            };
            let payload = &full[self.hlen()..self.hlen() + h.plen];
            if crc32c_append(self.salt, payload) != h.pcrc {
                break;
            }
            // A whole record that doesn't name the one before it follows
            // another history: a record between was lost or replaced
            // (D266).
            if self.v2 && h.prev != chain {
                return Err(EngineError::Corrupt(format!(
                    "shard {}: the log record at LSN {lsn} doesn't follow the one before it",
                    self.shard
                )));
            }
            chain = h.pcrc;
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
                group: h.group,
                nblocks,
                lists,
                pcrc: h.pcrc,
            });
            lsn += nblocks;
        }
        if let Some(later) = self.header_after(lsn)? {
            return Err(EngineError::Corrupt(format!(
                "shard {}: the log record at LSN {lsn} is damaged and the one at LSN {later} \
                 after it is valid",
                self.shard
            )));
        }
        Ok(out)
    }

    /// Whether the log at `lsn` holds a whole record written there under
    /// another salt: its header names `lsn`, and its payload checksum holds
    /// under the salt its header checksum was seeded with.
    fn foreign_record(&self, lsn: u64) -> Result<bool, EngineError> {
        let hdr = self.read_log(lsn, 1)?;
        let Some((rlsn, hcrc, h)) = parse_header(&hdr, self.v2) else {
            return Ok(false);
        };
        if rlsn != lsn
            || h.nblocks == 0
            || h.nblocks > self.ring / 2
            || self.hlen() + h.plen > h.nblocks as usize * BLOCK
        {
            return Ok(false);
        }
        let Some(salt) = crc_seed(&hdr[..self.hlen() - 4], hcrc) else {
            return Ok(false);
        };
        if salt == self.salt {
            return Ok(false);
        }
        let full = if h.nblocks == 1 {
            hdr
        } else {
            self.read_log(lsn, h.nblocks)?
        };
        Ok(crc32c_append(salt, &full[self.hlen()..self.hlen() + h.plen]) == h.pcrc)
    }

    /// Looks through the rest of the ring after the block at `lsn` for a
    /// record header written there under the current salt. Blocks from an
    /// earlier pass of the ring carry an older LSN, and blocks from earlier
    /// opens another salt, so neither passes.
    fn header_after(&self, lsn: u64) -> Result<Option<u64>, EngineError> {
        // 16 MiB a read, into one buffer: 1 MiB reads, or a fresh buffer
        // for each, make a 1 GiB ring several times slower (02-benchmarks.md).
        const CHUNK: u64 = 4096;
        let end = self.head_lsn + self.ring;
        let mut at = lsn + 1;
        let mut buf = AlignedBuf::zeroed(((end - at.min(end)).min(CHUNK) as usize) * BLOCK);
        while at < end {
            let n = (end - at).min(CHUNK);
            let buf = &mut buf[..n as usize * BLOCK];
            self.read_log_into(at, buf)?;
            for (i, blk) in buf.chunks(BLOCK).enumerate() {
                if self.header(blk, at + i as u64).is_some() {
                    return Ok(Some(at + i as u64));
                }
            }
            at += n;
        }
        Ok(None)
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
        // Each block a record mapped: (record, object, logical block,
        // physical block). A later `Take` moves an object's blocks to
        // another key, so its entries follow; the object it replaces had
        // its blocks freed.
        let mut maps: Vec<(usize, ObjKey, u64, u64)> = Vec::new();
        for (i, rec) in records.iter().enumerate().skip(first) {
            for l in &rec.lists {
                for op in &l.ops {
                    match op {
                        ROp::Map { lb, pblock, crcs } => {
                            for k in 0..crcs.len() as u64 {
                                maps.push((i, l.key.clone(), lb + k, pblock + k));
                            }
                        }
                        ROp::Take(src) => {
                            maps.retain(|m| m.1 != l.key);
                            for m in maps.iter_mut().filter(|m| m.1 == *src) {
                                m.1 = l.key.clone();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        for (i, key, lb, pblock) in maps {
            let Some(o) = self.index.objects.get(&key) else {
                continue;
            };
            let Loc::Mapped { pblock: pb, crc } = o.lookup(lb) else {
                continue;
            };
            if pb != pblock || fx.pending.contains_key(&pb) || self.applying.contains_key(&pb) {
                continue;
            }
            let mut buf = AlignedBuf::zeroed(BLOCK);
            self.dev.read(pb * BLOCK as u64, &mut buf)?;
            if crc32c(&buf) != crc {
                // A log record lands in the data region only by a
                // misdirected write, which also lost a record after this
                // one: the group was acknowledged, not torn.
                let rlsn = u64::from_le_bytes(buf[8..16].try_into().unwrap());
                if self.header(&buf, rlsn).is_some() {
                    return Err(EngineError::Corrupt(format!(
                        "shard {}: block {pb} holds the log record for LSN {rlsn}",
                        self.shard
                    )));
                }
                return Ok(Some(i));
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
        let mut results = Vec::with_capacity(batch.len());
        let mut rest: std::collections::VecDeque<OpList> = batch.into();
        while !rest.is_empty() {
            let mut round = self.commit_begin(&mut rest)?;
            let writes = round.take_writes();
            let io: Vec<(u64, &[u8])> = writes.iter().map(|(o, b)| (*o, &b[..])).collect();
            let mut r = if io.is_empty() {
                Ok(())
            } else {
                self.dev.write_batch(&io).and_then(|()| self.dev.flush())
            };
            if let (Ok(()), Some(sup)) = (&r, round.take_seal()) {
                let io: Vec<(u64, &[u8])> = sup.iter().map(|(o, b)| (*o, &b[..])).collect();
                r = self.dev.write_batch(&io).and_then(|()| self.dev.flush());
            }
            results.append(&mut self.commit_end(round, r)?);
        }
        self.commit_done();
        Ok(results)
    }

    /// Plans one round of a commit (I199): takes lists from the front of
    /// `lists` up to what one log record holds, resolves them and applies
    /// them to the index, and returns the writes that make them durable.
    /// The caller writes them, flushes, and hands the outcome to
    /// [`Shard::commit_end`]; nothing else may use the shard in between,
    /// since the index already shows the round. [`Shard::commit`] does all
    /// of it at once.
    pub fn commit_begin(
        &mut self,
        lists: &mut std::collections::VecDeque<OpList>,
    ) -> Result<Round, EngineError> {
        self.check()?;
        assert!(!self.in_flight, "a commit round is already in flight");
        match self.commit_round(lists) {
            Ok(r) => {
                self.in_flight = !r.writes.is_empty() || r.seal.is_some();
                Ok(r)
            }
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }

    /// Ends a round begun by [`Shard::commit_begin`], given whether its
    /// writes and the flush after them succeeded: the log advances and the
    /// extents the round freed can be reused. A failed write poisons the
    /// shard, as in [`Shard::commit`].
    pub fn commit_end(
        &mut self,
        round: Round,
        io: std::io::Result<()>,
    ) -> Result<Vec<Result<Committed, OpError>>, EngineError> {
        self.in_flight = false;
        self.applying.clear();
        if let Err(e) = io {
            self.poisoned = true;
            return Err(e.into());
        }
        if round.nblocks > 0 {
            self.tail_lsn += round.nblocks;
            self.records += 1;
        }
        if let Some(seal) = round.seal {
            self.seal_done(seal);
        }
        self.free_after_reads(round.freed);
        Ok(round.results)
    }

    /// Counts a commit whose rounds have all ended.
    pub fn commit_done(&mut self) {
        self.commits += 1;
    }

    /// Sets the size below which overwrites of held blocks are deferred
    /// (spec/04-decisions.md D22; spec/01-implementation-notes.md I14). 0 redirects
    /// every write to new extents.
    pub fn set_defer_max(&mut self, bytes: usize) {
        self.defer_max = bytes;
    }

    /// Sets how many deferred blocks the shard holds before the next
    /// commit round applies them in place.
    pub fn set_pending_limit(&mut self, blocks: usize) {
        self.pending_limit = blocks;
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
        (self.ring / 4) as usize * BLOCK - self.hlen()
    }

    fn commit_round(
        &mut self,
        lists: &mut std::collections::VecDeque<OpList>,
    ) -> Result<Round, EngineError> {
        if self.pending.len() > self.pending_limit {
            self.stage_deferred();
        }
        // Fuzzy checkpoints (D43): start one when a quarter of the log is
        // used, write a slice of it each round, and finish it at once if
        // the log reaches three quarters first.
        let used = self.tail_lsn - self.head_lsn;
        let can = !self.ckpt_stalled || self.alloc.free_blocks() >= self.checkpoint_reserve();
        let r = if !can {
            Ok(())
        } else if self.ckpt_stalled {
            // Room again: checkpoint now, which lifts the stall (nothing
            // that would grow the log is let through until then).
            self.checkpoint_inner()
        } else if used > self.ring * 3 / 4 {
            if self.fuzzy.is_some() {
                self.checkpoint_forced += 1;
            }
            self.checkpoint_inner()
        } else if self.fuzzy.is_some() {
            let slice = if used > self.ring / 2 {
                self.slice_bytes.saturating_mul(8)
            } else {
                self.slice_bytes
            };
            self.checkpoint_slice(slice)
        } else if used > self.ring / 4 {
            self.checkpoint_start()
                .and_then(|()| self.checkpoint_slice(self.slice_bytes))
        } else {
            Ok(())
        };
        match r {
            Err(EngineError::NoSpace) => self.abandon_checkpoint(),
            r => r?,
        }
        // A `Take` names its source, not what it holds: replayed over pages
        // of a checkpoint written on either side of it, it couldn't tell
        // which state they hold. So no checkpoint is in progress when one
        // is logged: replay then starts before it, over pages that are all
        // older, or after it.
        if self.fuzzy.is_some()
            && lists
                .iter()
                .any(|l| l.ops.iter().any(|o| matches!(o, Op::Take(_))))
        {
            match self.checkpoint_inner() {
                Err(EngineError::NoSpace) => self.abandon_checkpoint(),
                r => r?,
            }
        }
        // A stalled checkpoint can't move the log's head: a round takes at
        // most what the ring has left.
        let used = self.tail_lsn - self.head_lsn;
        let room = ((self.ring - used) as usize * BLOCK).saturating_sub(self.hlen());
        let budget = self.max_payload().min(room);
        self.round_fresh.clear();
        let mut results = Vec::new();
        let mut payload = Enc::new();
        payload.u32(0);
        let mut count = 0u32;
        let mut writes: Vec<(u64, WriteBuf)> = Vec::new();
        let mut fx = Effects {
            freed: Vec::new(),
            pending: std::mem::take(&mut self.pending),
            tentative: Default::default(),
        };
        while let Some(list) = lists.front() {
            let est = self.estimate(list);
            if est > budget {
                lists.pop_front();
                results.push(Err(if est > self.max_payload() {
                    OpError::TooLarge
                } else {
                    OpError::NoSpace
                }));
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
        // Upkeep's writes go with the round's: no ordering between them
        // matters until the flush, after which the superblock of a
        // checkpoint the round completes names it.
        let mut io: Vec<(u64, WriteBuf)> = std::mem::take(&mut self.staged)
            .into_iter()
            .chain(std::mem::take(&mut self.staged_ckpt))
            .map(|(at, b)| (at, b.into()))
            .collect();
        let seal = self.sealing.take();
        if count == 0 {
            return Ok(Round {
                writes: io,
                nblocks: 0,
                freed: fx.freed,
                results,
                seal,
            });
        }
        payload.patch_u32(0, count);
        let group = self.next_group;
        self.next_group += 1;
        let rec = self.encode_record(group, payload.as_slice());
        let nblocks = (rec.len() / BLOCK) as u64;
        let lsn = self.tail_lsn;
        io.extend(writes.into_iter().map(|(pb, b)| (pb * BLOCK as u64, b)));
        let first = (self.ring - lsn % self.ring).min(nblocks);
        if nblocks > first {
            // The record wraps around the ring: two writes.
            let (a, b) = rec.split_at(first as usize * BLOCK);
            for (at, part) in [(lsn, a), (lsn + first, b)] {
                let mut buf = AlignedBuf::zeroed(part.len());
                buf.copy_from_slice(part);
                io.push((self.log_pos(at), buf.into()));
            }
        } else {
            io.push((self.log_pos(lsn), rec.into()));
        }
        Ok(Round {
            writes: io,
            nblocks,
            freed: fx.freed,
            results,
            seal,
        })
    }

    /// Encodes the record at the log's tail; in the second format it names
    /// the record before, and becomes the one the next names (D266).
    fn encode_record(&mut self, group: u64, payload: &[u8]) -> AlignedBuf {
        let nblocks = (self.hlen() + payload.len()).div_ceil(BLOCK);
        let mut buf = AlignedBuf::zeroed(nblocks * BLOCK);
        let mut e = Enc::with_capacity(self.hlen());
        let pcrc = crc32c_append(self.salt, payload);
        e.u32(LOG_MAGIC)
            .u16(if self.v2 { LOG_VERSION2 } else { LOG_VERSION })
            .u16(0)
            .u64(self.tail_lsn)
            .u64(group)
            .u32(nblocks as u32)
            .u32(payload.len() as u32)
            .u32(pcrc);
        if self.v2 {
            e.u32(self.chain).u32(0);
        }
        self.chain = pcrc;
        let hcrc = crc32c_append(self.salt, e.as_slice());
        e.u32(hcrc);
        buf[..self.hlen()].copy_from_slice(e.as_slice());
        buf[self.hlen()..self.hlen() + payload.len()].copy_from_slice(payload);
        buf
    }

    /// Validates one list, resolves its operations and applies them to the
    /// index. Validation happens before anything changes, so a refused list
    /// leaves no trace.
    fn plan(
        &mut self,
        list: OpList,
        fx: &mut Effects,
        writes: &mut Vec<(u64, WriteBuf)>,
        others: &mut Vec<RList>,
    ) -> Result<(RList, Committed), OpError> {
        let key = list.key;
        let cur = self.index.objects.get(&key).map_or(0, |o| o.revision);
        let mut need = 0u64;
        let mut mutates = false;
        // Whether the list can take space (data or index); one that only
        // frees is let through on a full shard, so it can be emptied.
        let mut grows = false;
        // Whether it grows the used space past what it frees (D27's
        // `full`): a new object, or a write to blocks the object doesn't
        // hold yet. Keys and attributes of an object that exists don't
        // count: leases and volume headers are kept so, and their index
        // space comes from the room above `full`.
        let mut takes_room = false;
        let held = self.index.objects.get(&key);
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
                Op::Write { off, data, crcs } => {
                    if off % BLOCK as u64 != 0 || data.len() % BLOCK != 0 {
                        return Err(OpError::Unaligned);
                    }
                    if crcs.as_ref().is_some_and(|c| c.len() != data.len() / BLOCK) {
                        return Err(OpError::Crcs);
                    }
                    need += (data.len() / BLOCK) as u64;
                    mutates = true;
                    grows = true;
                    let n = (data.len() / BLOCK) as u64;
                    takes_room |= !held.is_some_and(|o| o.all_mapped(off / BLOCK as u64, n));
                }
                Op::Zero { off, len } => {
                    if off % BLOCK as u64 != 0 || len % BLOCK as u64 != 0 {
                        return Err(OpError::Unaligned);
                    }
                    mutates = true;
                }
                Op::Absorb(src) | Op::Take(src) if *src == key => {
                    return Err(OpError::SelfAbsorb);
                }
                Op::Delete
                | Op::Truncate(_)
                | Op::KvDel(_)
                | Op::KvDelRange(..)
                | Op::RmAttr(_)
                | Op::Absorb(_)
                | Op::Take(_) => mutates = true,
                _ => {
                    mutates = true;
                    grows = true;
                    takes_room |= held.is_none();
                }
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
        let free = self.alloc.free_blocks();
        if need > free || (grows && (self.ckpt_stalled || need + self.checkpoint_reserve() > free))
        {
            return Err(OpError::NoSpace);
        }
        let total = self.region.data_blocks();
        if takes_room
            && self.full_ratio > 0
            && self.used_blocks() + need > Self::limit(total, self.full_ratio)
        {
            return Err(OpError::NoSpace);
        }
        let mut rops = Vec::new();
        let step = |idx: &mut Index, fx: &mut Effects, rop: ROp, rops: &mut Vec<ROp>| {
            idx.apply_op(&key, &rop, fx);
            rops.push(rop);
        };
        // Whether the object exists at this point of the list: `Delete`
        // ends it, `Take` of a missing source changes nothing, and any
        // other changing operation (re)creates it.
        let mut alive = cur != 0;
        for op in list.ops {
            match &op {
                Op::AssertRevision(_) | Op::AssertExists | Op::AssertAbsent => {}
                Op::Delete => alive = false,
                Op::Take(src) => alive |= self.index.objects.contains_key(src),
                _ => alive = true,
            }
            match op {
                Op::AssertRevision(_) | Op::AssertExists | Op::AssertAbsent | Op::Touch => {}
                Op::Write { off, data, crcs } => {
                    if data.is_empty() {
                        continue;
                    }
                    let lb = off / BLOCK as u64;
                    let n = (data.len() / BLOCK) as u64;
                    // CRCs that came with the data were checked against it
                    // when it arrived; computing them again here would
                    // also bless data damaged since (I306).
                    let crcs = crcs.unwrap_or_else(|| data.chunks(BLOCK).map(crc32c).collect());
                    // Never under a read in flight, which may have
                    // planned the old data from the device (D190).
                    let deferred = data.len() < self.defer_max
                        && self.reads.is_empty()
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
                                data: data.into_vec(),
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
                            // A received payload starts on a block, and so
                            // does every run of it: written as it is (I338).
                            let run = data.slice(done * BLOCK..(done + len) * BLOCK);
                            writes.push((
                                pb,
                                if run.is_aligned() {
                                    WriteBuf::Shared(run)
                                } else {
                                    AlignedBuf::from_slice(&run).into()
                                },
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
                Op::Take(src) => {
                    if self.index.objects.contains_key(&src) {
                        step(&mut self.index, fx, ROp::Take(src), &mut rops);
                    }
                }
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
    /// it until the next checkpoint), in this shard's own device calls.
    pub fn apply_deferred(&mut self) -> Result<(), EngineError> {
        self.check()?;
        assert!(!self.in_flight, "a commit round is in flight");
        self.stage_deferred();
        self.write_staged()
    }

    /// Moves the deferred blocks to `staged`, as writes in place, one per
    /// run of consecutive blocks; reads find them in `applying` until
    /// they are written.
    fn stage_deferred(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        let mut run: Option<(u64, Vec<u8>)> = None;
        for (pb, data) in &pending {
            match &mut run {
                Some((start, v)) if *start + (v.len() / BLOCK) as u64 == *pb => {
                    v.extend_from_slice(data)
                }
                _ => {
                    if let Some((s, v)) = run.take() {
                        self.staged
                            .push((s * BLOCK as u64, AlignedBuf::from_slice(&v)));
                    }
                    run = Some((*pb, data.to_vec()));
                }
            }
        }
        if let Some((s, v)) = run {
            self.staged
                .push((s * BLOCK as u64, AlignedBuf::from_slice(&v)));
        }
        self.applying.extend(pending);
    }

    /// Makes what upkeep staged outside a commit round, in this shard's
    /// own device calls: the writes, then a checkpoint's superblock after
    /// a flush. A failure poisons the shard.
    fn write_staged(&mut self) -> Result<(), EngineError> {
        let mut writes = std::mem::take(&mut self.staged);
        writes.append(&mut self.staged_ckpt);
        let seal = self.sealing.take();
        let io: Vec<(u64, &[u8])> = writes.iter().map(|(o, b)| (*o, &b[..])).collect();
        let mut r = if io.is_empty() {
            Ok(())
        } else {
            self.dev.write_batch(&io)
        };
        if let (Ok(()), Some(s)) = (&r, &seal) {
            let sup: Vec<(u64, &[u8])> = s.sup.iter().map(|(o, b)| (*o, &b[..])).collect();
            r = self
                .dev
                .flush()
                .and_then(|()| self.dev.write_batch(&sup))
                .and_then(|()| self.dev.flush());
        }
        self.applying.clear();
        if let Err(e) = r {
            self.poisoned = true;
            return Err(e.into());
        }
        if let Some(s) = seal {
            self.seal_done(s);
        }
        Ok(())
    }

    /// Writes a checkpoint of the index and moves the log's head to its
    /// tail, so the whole ring is free again.
    pub fn checkpoint(&mut self) -> Result<(), EngineError> {
        self.check()?;
        assert!(!self.in_flight, "a commit round is in flight");
        let r = self.checkpoint_inner();
        match &r {
            Err(EngineError::NoSpace) => self.abandon_checkpoint(),
            Err(_) => self.poisoned = true,
            Ok(()) => {}
        }
        r?;
        self.write_staged()
    }

    /// Gives up a checkpoint that found no room: frees the pages it wrote,
    /// puts back the page bounds it cut or dropped, and marks every page it
    /// was to write, so the next one writes them. The shard stays usable;
    /// changes that take space are refused until a checkpoint completes.
    fn abandon_checkpoint(&mut self) {
        self.ckpt_stalled = true;
        // Pages not yet written go with it: their extents are free again.
        self.staged_ckpt.clear();
        let Some(f) = self.fuzzy.take() else { return };
        for loc in f.written.values() {
            for &(s, n) in &loc.runs {
                self.alloc.free(s, n);
            }
        }
        // The bounds are the last checkpoint's pages again; a mark on a
        // bound that goes moves to the page that now holds its range.
        self.index.bounds = self.pages.keys().flatten().cloned().collect();
        let marks = std::mem::take(&mut self.index.dirty);
        for b in marks {
            let page = self.index.page_of(&b).cloned();
            self.index.mark(page);
        }
        for p in f.replaced.iter().chain(f.todo.iter()) {
            let page = match p {
                Some(b) => self.index.page_of(b).cloned(),
                None => None,
            };
            self.index.mark(page);
        }
    }

    /// Starts or stops discarding freed blocks (D275). Started right after
    /// opening, while nothing is in the log since the checkpoint every open
    /// writes, every free block is queued too, so space freed before a
    /// crash, or by a build without discards, is discarded once.
    pub fn set_discard(&mut self, on: bool) {
        if !on {
            self.discards = None;
            return;
        }
        if self.discards.is_some() {
            return;
        }
        let mut d = Discards::default();
        if self.head_lsn == self.tail_lsn && self.fuzzy.is_none() && self.sealing.is_none() {
            for (s, n) in self.alloc.free_within(0, u64::MAX) {
                add_run(&mut d.ready, s, n);
            }
        }
        self.discards = Some(d);
    }

    /// Up to `max_blocks` of freed blocks ready to discard (D275), as runs
    /// of blocks: only the parts still free, which stay out of the
    /// allocator until [`Shard::discards_done`] returns them, so nothing
    /// is written there while the discard is in flight. Parts reused since
    /// they were freed are dropped from the queue. At most an eighth of the
    /// free space is taken at once, so writes go on meanwhile.
    pub fn take_discards(&mut self, max_blocks: u64) -> Vec<(u64, u64)> {
        let max_blocks = max_blocks.min(self.alloc.free_blocks() / DISCARD_SHARE);
        let Some(d) = self.discards.as_mut().filter(|_| !self.poisoned) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // Queued blocks looked at: those reused since count too, so that a
        // queue of them can't make one call long.
        let mut seen = 0;
        while seen < max_blocks {
            let Some((s, n)) = d.ready.pop_first() else {
                break;
            };
            let n = if n > max_blocks - seen {
                let keep = max_blocks - seen;
                d.ready.insert(s + keep, n - keep);
                keep
            } else {
                n
            };
            seen += n;
            for (fs, fn_) in self.alloc.free_within(s, n) {
                let reserved = self.alloc.reserve(fs, fn_);
                debug_assert!(reserved, "a free run reserves");
                out.push((fs, fn_));
            }
        }
        out
    }

    /// Discards made for [`Shard::take_discards`]'s runs have ended, well
    /// (`ok`) or not: the blocks are free again.
    pub fn discards_done(&mut self, runs: &[(u64, u64)], ok: bool) {
        for &(s, n) in runs {
            self.alloc.free(s, n);
            if ok {
                self.discarded += n;
            }
        }
    }

    /// Where the last checkpoint is on the device: its directory, the
    /// directory's copy (empty without one), and each page, in key order.
    /// For tests and diagnostics.
    pub fn checkpoint_layout(&self) -> CheckpointLayout {
        CheckpointLayout {
            directory: self.ckpt.as_ref().map_or(Vec::new(), |c| c.runs.clone()),
            directory_copy: self.ckpt.as_ref().map_or(Vec::new(), |c| c.copy.clone()),
            pages: self.pages.values().map(|p| p.runs.clone()).collect(),
        }
    }

    /// Whether opening read the checkpoint directory's second copy, the
    /// first being damaged (D269).
    pub fn directory_copy_read(&self) -> bool {
        self.directory_copy_read
    }

    /// Reads page `i` of the last checkpoint (in key order) and checks it
    /// against the directory (D269). A damaged one, or one that can't be
    /// read, is marked changed and a checkpoint written at once, so a
    /// restart never needs it; returns whether it was damaged. Past the
    /// last page, nothing is read. The caller holds the shard between
    /// commit rounds.
    pub fn scrub_checkpoint_page(&mut self, i: usize) -> Result<bool, EngineError> {
        self.check()?;
        let Some((bound, p)) = self.pages.iter().nth(i) else {
            return Ok(false);
        };
        let sound = self
            .read_extents(&p.runs, p.len)
            .is_ok_and(|b| crc32c(&b[..p.len as usize]) == p.crc);
        if sound {
            return Ok(false);
        }
        // Its range's page now: a fuzzy checkpoint may have cut or dropped
        // bounds since this one was named.
        let page = match bound {
            Some(b) => self.index.page_of(b).cloned(),
            None => None,
        };
        self.index.mark(page);
        self.checkpoint_damage += 1;
        self.rewrite_checkpoint()?;
        Ok(true)
    }

    /// Reads both copies of the last checkpoint's directory (D269). If one
    /// is damaged, or the checkpoint is from before copies, a checkpoint
    /// is written at once, with both; returns whether one was damaged.
    pub fn scrub_checkpoint_directory(&mut self) -> Result<bool, EngineError> {
        self.check()?;
        let Some(c) = self.ckpt.clone() else {
            return Ok(false);
        };
        let damaged = self.read_directory(&c.runs, &c).is_none()
            || (!c.copy.is_empty() && self.read_directory(&c.copy, &c).is_none());
        self.checkpoint_damage += damaged as u64;
        if damaged || c.copy.is_empty() {
            self.rewrite_checkpoint()?;
        }
        Ok(damaged)
    }

    /// Writes a checkpoint now for scrub's repairs: first finishes one
    /// that is running (its pages were chosen before the mark), then
    /// writes the pages marked since.
    fn rewrite_checkpoint(&mut self) -> Result<(), EngineError> {
        if self.fuzzy.is_some() {
            self.checkpoint()?;
        }
        self.checkpoint()
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

    /// Blocks the next checkpoint may need, kept free for it: every page
    /// written anew (the pages it replaces are freed only once it is
    /// named) and the directory, with room for the index to grow until
    /// then. Without it, a full shard's checkpoint fails and poisons it.
    fn checkpoint_reserve(&self) -> u64 {
        let blocks = |len: u64| len.div_ceil(BLOCK as u64);
        let pages: u64 = self.pages.values().map(|p| blocks(p.len) + 1).sum();
        let legacy: u64 = self.legacy_blob.iter().map(|r| r.1).sum();
        // The directory and its copy (D269).
        let dir = 2 * self.ckpt.as_ref().map_or(1, |c| blocks(c.len));
        let base = pages + legacy + dir + 1;
        base + base / 4 + 8
    }

    /// Refuses, past `thousandths` of the data region, changes that grow
    /// the used space (04-decisions.md D27): new objects, and writes to
    /// blocks an object doesn't hold. Overwrites, an existing object's keys
    /// and attributes, deletes and the rest go on, in the room above it. 0
    /// lifts the limit.
    pub fn set_full_ratio(&mut self, thousandths: u16) {
        self.full_ratio = thousandths;
    }

    /// Whether the used space is past `thousandths` of the data region
    /// (D27's thresholds, D182's `backfillfull`).
    pub fn past(&self, thousandths: u16) -> bool {
        self.used_blocks() > Self::limit(self.region.data_blocks(), thousandths)
    }

    fn used_blocks(&self) -> u64 {
        self.region.data_blocks() - self.alloc.free_blocks()
    }

    fn limit(total: u64, thousandths: u16) -> u64 {
        (total as u128 * thousandths as u128 / 1000) as u64
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
        self.stage_deferred();
        // Freed so far: discarded once this checkpoint is named (D275).
        if let Some(d) = self.discards.as_mut() {
            let next = std::mem::take(&mut d.next);
            merge_runs(&mut d.sealing, next);
        }
        if !self.pages.contains_key(&None) {
            // A new shard: the first page is always written.
            self.index.mark(None);
        }
        let todo = self.index.take_dirty().into();
        self.fuzzy = Some(Fuzzy {
            start_lsn: self.tail_lsn,
            start_chain: self.chain,
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
        let mut at = 0usize;
        for &(s, n) in &runs {
            let end = ((at + n as usize) * BLOCK).min(bytes.len());
            self.staged_ckpt.push((
                s * BLOCK as u64,
                AlignedBuf::from_slice(&bytes[at * BLOCK..end]),
            ));
            at += n as usize;
        }
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

    /// Stages the directory and readies the superblock that points at the
    /// new checkpoint with the log's head at its start ([`Seal`]); once
    /// it is written, [`Shard::seal_done`] frees the pages it replaced.
    fn checkpoint_finish(&mut self, f: Fuzzy) -> Result<(), EngineError> {
        assert!(self.sealing.is_none(), "a checkpoint is already sealing");
        let mut pages = self.pages.clone();
        let mut free = Vec::new();
        for p in &f.replaced {
            if let Some(old) = pages.remove(p) {
                free.extend(old.runs);
            }
        }
        pages.extend(f.written.clone());
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
                self.fuzzy = Some(f);
                return Err(EngineError::NoSpace);
            }
            None => {
                self.fuzzy = Some(f);
                return Err(EngineError::NoSpace);
            }
        };
        // The directory's copy (D269), in blocks of its own; without room
        // for it, or past the runs a superblock holds, the checkpoint goes
        // on without one rather than stall.
        let copy_runs = match self.alloc.alloc(nblocks) {
            Some(r) if dir_runs.len() + r.len() <= MAX_CKPT_RUNS => r,
            Some(r) => {
                for (s, n) in r {
                    self.alloc.free(s, n);
                }
                Vec::new()
            }
            None => Vec::new(),
        };
        for runs in [&dir_runs, &copy_runs] {
            let mut done = 0usize;
            for &(s, n) in runs {
                let end = ((done + n as usize) * BLOCK).min(dir.len());
                self.staged_ckpt.push((
                    s * BLOCK as u64,
                    AlignedBuf::from_slice(&dir[done * BLOCK..end]),
                ));
                done += n as usize;
            }
            if !runs.is_empty() {
                self.ckpt_written += nblocks * BLOCK as u64;
            }
        }
        // Pages, directory, and every deferred write applied since the
        // start become durable (the flush after the round's writes)
        // before the superblock refers to them.
        let ckpt = Ckpt {
            runs: dir_runs,
            len: dir.len() as u64,
            crc: dir_crc,
            copy: copy_runs,
        };
        let sup = Super {
            seq: self.super_seq + 1,
            device_id: self.device_id,
            shard: self.shard,
            salt: self.salt,
            head_lsn: f.start_lsn,
            ckpt: Some(ckpt.clone()),
            v2: self.v2,
            chain: f.start_chain,
        };
        let buf = sup.encode();
        self.sealing = Some(Seal {
            sup: super_slots(&self.region, self.v2, sup.seq)
                .into_iter()
                .map(|at| (at, AlignedBuf::from_slice(&buf)))
                .collect(),
            seq: sup.seq,
            head_lsn: f.start_lsn,
            ckpt,
            pages,
            free,
        });
        Ok(())
    }

    /// Ends a checkpoint whose superblock is durable: the log's head moves
    /// to its start, and the pages and directory it replaced are free.
    fn seal_done(&mut self, seal: Seal) {
        let Seal {
            seq,
            head_lsn,
            ckpt,
            pages,
            mut free,
            ..
        } = seal;
        self.super_seq = seq;
        self.head_lsn = head_lsn;
        if let Some(old) = self.ckpt.replace(ckpt) {
            free.extend(old.runs);
            free.extend(old.copy);
        }
        free.append(&mut self.legacy_blob);
        // Blocks freed before this checkpoint began are discarded now;
        // what it frees waits for the next one.
        if let Some(d) = self.discards.as_mut() {
            let sealing = std::mem::take(&mut d.sealing);
            merge_runs(&mut d.ready, sealing);
            for &(s, n) in &free {
                add_run(&mut d.next, s, n);
            }
        }
        for (s, n) in free {
            self.alloc.free(s, n);
        }
        self.pages = pages;
        self.checkpoints += 1;
        self.ckpt_stalled = false;
    }

    /// Reads `[off, off + len)` of an object. Holes read as zeros; every
    /// mapped block's checksum is verified.
    /// The device reads hold the caller's thread, and end before anything
    /// else can change the shard.
    pub fn read(&self, key: &ObjKey, off: u64, len: u64) -> Result<AlignedBuf, EngineError> {
        let plan = self.plan_object(key, off, len)?;
        self.read_through(plan)
    }

    /// Plans a read of `[off, off + len)` of an object, for a caller that
    /// makes the device reads itself (on a reactor's ring) and then calls
    /// [`Shard::read_end`] and [`ReadPlan::finish`].
    pub fn read_begin(
        &mut self,
        key: &ObjKey,
        off: u64,
        len: u64,
    ) -> Result<ReadPlan, EngineError> {
        let plan = self.plan_object(key, off, len)?;
        Ok(self.track(plan))
    }

    fn plan_object(&self, key: &ObjKey, off: u64, len: u64) -> Result<ReadPlan, EngineError> {
        self.check()?;
        if !off.is_multiple_of(BLOCK as u64) || !len.is_multiple_of(BLOCK as u64) {
            return Err(EngineError::Unaligned);
        }
        let o = self.index.objects.get(key).ok_or(EngineError::NotFound)?;
        Ok(self.plan_read(off, len, |lb| match o.lookup(lb) {
            Loc::Mapped { pblock, crc } => Some((pblock, crc)),
            Loc::Hole => None,
        }))
    }

    /// A read planned by [`Shard::read_begin`] or
    /// [`Shard::read_layered_begin`] has ended, or was abandoned: what it
    /// read no longer matters, so extents freed since it began can be
    /// reused once older reads end too.
    pub fn read_end(&mut self, plan: &ReadPlan) {
        if plan.ios.is_empty() {
            return;
        }
        let Some(n) = self.reads.get_mut(&plan.generation) else {
            return;
        };
        *n -= 1;
        if *n == 0 {
            self.reads.remove(&plan.generation);
        }
        // Extents freed in generation g may be read by reads begun in g or
        // before.
        let oldest = self.reads.keys().next().copied().unwrap_or(u64::MAX);
        while self.held.front().is_some_and(|(g, _)| *g < oldest) {
            let (_, freed) = self.held.pop_front().expect("checked above");
            for (s, n) in freed {
                self.alloc.free(s, n);
            }
        }
    }

    /// Reads in flight on the device.
    pub fn reads_in_flight(&self) -> usize {
        self.reads.values().sum()
    }

    /// Blocks freed but held for reads in flight.
    pub fn held_blocks(&self) -> u64 {
        self.held
            .iter()
            .flat_map(|(_, f)| f.iter().map(|(_, n)| *n))
            .sum()
    }

    /// Frees extents now, or once the reads in flight that may use them
    /// end.
    fn free_after_reads(&mut self, freed: Vec<(u64, u64)>) {
        if let Some(d) = self.discards.as_mut() {
            for &(s, n) in &freed {
                add_run(&mut d.next, s, n);
            }
        }
        if self.reads.is_empty() {
            for (s, n) in freed {
                self.alloc.free(s, n);
            }
        } else if !freed.is_empty() {
            self.held.push_back((self.read_gen, freed));
            self.read_gen += 1;
        }
    }

    fn track(&mut self, mut plan: ReadPlan) -> ReadPlan {
        plan.generation = self.read_gen;
        if !plan.ios.is_empty() {
            *self.reads.entry(self.read_gen).or_default() += 1;
        }
        plan
    }

    /// Makes a planned read's device reads in this call, holding the
    /// caller's thread.
    fn read_through(&self, mut plan: ReadPlan) -> Result<AlignedBuf, EngineError> {
        let mut buf = plan.take_buf();
        let r = {
            let mut reads: Vec<(u64, &mut [u8])> = Vec::with_capacity(plan.ios.len());
            let mut rest: &mut [u8] = &mut buf;
            let mut at = 0;
            for &(dev_off, buf_off, len) in &plan.ios {
                let (_, tail) = rest.split_at_mut(buf_off - at);
                let (head, tail) = tail.split_at_mut(len);
                reads.push((dev_off, head));
                rest = tail;
                at = buf_off + len;
            }
            self.dev.read_batch(&mut reads)
        };
        r?;
        plan.finish(buf)
    }

    /// Plans reading `[off, off + len)` with `locate` saying where each
    /// logical block is (physical block and CRC; none: a hole). Blocks
    /// whose deferred write is still in memory are copied now.
    fn plan_read(
        &self,
        off: u64,
        len: u64,
        locate: impl Fn(u64) -> Option<(u64, u32)>,
    ) -> ReadPlan {
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
                if let Some(d) = self.pending.get(&pblock).or(self.applying.get(&pblock)) {
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
        let ios = ranges
            .iter()
            .map(|&(l, p, c)| {
                (
                    p * BLOCK as u64,
                    ((l - lb0) as usize) * BLOCK,
                    c as usize * BLOCK,
                )
            })
            .collect();
        ReadPlan {
            generation: 0,
            out,
            lb0,
            ios,
            crcs,
        }
    }

    pub fn stat(&self, key: &ObjKey) -> Option<ObjStat> {
        self.index.objects.get(key).map(|o| ObjStat {
            size: o.size,
            revision: o.revision,
            blocks: o.block_count(),
            kv_bytes: o.kv_bytes,
        })
    }

    /// A digest of an object's content, from the index alone (no data is
    /// read): its size in blocks, each mapped block's number and CRC, its
    /// attributes and its key–value table. Two copies with equal digests
    /// read the same, so delta recovery and scrub compare digests
    /// (09-replication.md §8.5). A block of zeros counts as a hole, since a
    /// hole reads as zeros; FNV-1a keeps the digest the same on every node
    /// and version.
    pub fn digest(&self, key: &ObjKey) -> Option<u64> {
        let o = self.index.objects.get(key)?;
        // Every list that leaves an object gives it a revision of its own,
        // so a large object's digest is kept until its revision changes.
        // Revision 0 (an object from before revisions) is never trusted.
        let cache = o.revision != 0 && o.kv.len() + o.attrs.len() >= DIGEST_CACHE_MIN;
        if cache
            && let Some((rev, d)) = self.digests.borrow().get(key)
            && *rev == o.revision
        {
            return Some(*d);
        }
        let d = Self::digest_of(o);
        if cache {
            let mut c = self.digests.borrow_mut();
            if c.len() >= DIGEST_CACHE_MAX {
                c.clear();
            }
            c.insert(key.clone(), (o.revision, d));
        }
        Some(d)
    }

    fn digest_of(o: &Object) -> u64 {
        let mut h = Self::digest_head(o);
        for table in [&o.attrs, &o.kv] {
            h.u64(table.len() as u64);
            for (k, v) in table {
                h.bytes(k).bytes(v);
            }
        }
        h.0
    }

    /// The digest of an object's size and blocks, which come first.
    fn digest_head(o: &Object) -> Fnv {
        let mut h = Fnv::new();
        h.u64(o.size.div_ceil(BLOCK as u64));
        for (lb, _, crcs) in o.blocks.runs() {
            for (i, c) in crcs.iter().enumerate() {
                if *c != zero_block_crc() {
                    h.u64(lb + i as u64).u64(u64::from(*c));
                }
            }
        }
        h
    }

    /// Begins [`Shard::digest`] of an object that may take a while: the
    /// same digest, a part at a time ([`Shard::digest_part`]), so that the
    /// caller can serve other requests in between (I188). A table of
    /// 200 MB takes the better part of a second to hash, longer than a
    /// peer waits for a ping. A cached digest, or one of an object with
    /// few entries, or of one from before revisions, is done at once.
    pub fn digest_begin(&self, key: &ObjKey) -> Option<DigestPart> {
        let o = self.index.objects.get(key)?;
        if o.revision == 0 || o.kv.len() + o.attrs.len() < DIGEST_CACHE_MIN {
            return self.digest(key).map(DigestPart::Done);
        }
        if let Some((rev, d)) = self.digests.borrow().get(key)
            && *rev == o.revision
        {
            return Some(DigestPart::Done(*d));
        }
        let mut h = Self::digest_head(o);
        h.u64(o.attrs.len() as u64);
        Some(DigestPart::More(Digesting {
            key: key.clone(),
            revision: o.revision,
            h,
            kv: false,
            after: None,
        }))
    }

    /// Hashes about `budget` more bytes of a digest begun with
    /// [`Shard::digest_begin`]; `None` if the object changed or went
    /// meanwhile (begin again). The finished digest is cached as
    /// [`Shard::digest`] caches it.
    pub fn digest_part(&self, mut d: Digesting, budget: usize) -> Option<DigestPart> {
        use std::ops::Bound;
        let o = self.index.objects.get(&d.key)?;
        if o.revision != d.revision {
            return None;
        }
        let mut spent = 0;
        loop {
            let table = if d.kv { &o.kv } else { &o.attrs };
            let from = match &d.after {
                Some(k) => Bound::Excluded(k.clone()),
                None => Bound::Unbounded,
            };
            for (k, v) in table.range((from, Bound::Unbounded)) {
                d.h.bytes(k).bytes(v);
                spent += k.len() + v.len() + 16;
                if spent >= budget {
                    d.after = Some(k.clone());
                    return Some(DigestPart::More(d));
                }
            }
            if d.kv {
                break;
            }
            d.kv = true;
            d.after = None;
            d.h.u64(o.kv.len() as u64);
        }
        let mut c = self.digests.borrow_mut();
        if c.len() >= DIGEST_CACHE_MAX {
            c.clear();
        }
        c.insert(d.key, (d.revision, d.h.0));
        Some(DigestPart::Done(d.h.0))
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

    /// The PGs this shard holds objects of, in order: one step through
    /// the index per PG, as keys sort by pool and PG first (D177's
    /// sweep of copies no longer served).
    pub fn held_pgs(&self) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut next = self.index.objects.keys().next().map(|k| (k.pool, k.pg));
        while let Some((pool, pg)) = next {
            out.push((pool, pg));
            let (pool, pg) = match pg.checked_add(1) {
                Some(pg) => (pool, pg),
                None => match pool.checked_add(1) {
                    Some(pool) => (pool, 0),
                    None => break,
                },
            };
            let from = ObjKey::new(pool, pg, crate::types::Namespace::Vol, Vec::new(), 0);
            next = self
                .index
                .objects
                .range(from..)
                .next()
                .map(|(k, _)| (k.pool, k.pg));
        }
        out
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

    /// Blocks mapped by at most `max` objects of one PG, from just after
    /// `after` (from the PG's first if `None`), and the last key counted
    /// while the PG may hold more: a PG is summed a slice at a time (D155).
    pub fn pg_blocks(
        &self,
        pool: u32,
        pg: u32,
        after: Option<&ObjKey>,
        max: usize,
    ) -> (u64, Option<ObjKey>) {
        use std::ops::Bound;
        let lo = match after {
            Some(k) => Bound::Excluded(k.clone()),
            None => Bound::Included(ObjKey::new(
                pool,
                pg,
                crate::types::Namespace::Vol,
                Vec::new(),
                0,
            )),
        };
        let mut blocks = 0;
        let mut last: Option<&ObjKey> = None;
        for (n, (k, o)) in self.index.objects.range((lo, Bound::Unbounded)).enumerate() {
            if (k.pool, k.pg) != (pool, pg) {
                return (blocks, None);
            }
            if n == max {
                return (blocks, last.cloned());
            }
            blocks += o.block_count();
            last = Some(k);
        }
        (blocks, None)
    }

    pub fn stats(&self) -> ShardStats {
        ShardStats {
            objects: self.index.objects.len() as u64,
            free_blocks: self.alloc.free_blocks(),
            data_blocks: self.region.data_blocks(),
            log_used_blocks: self.tail_lsn - self.head_lsn,
            log_blocks: self.ring,
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
            checkpoint_damage: self.checkpoint_damage,
            discard_queued_blocks: self.discards.as_ref().map_or(0, |d| {
                [&d.next, &d.sealing, &d.ready]
                    .iter()
                    .flat_map(|m| m.values())
                    .sum()
            }),
            discarded_blocks: self.discarded,
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

    /// The blocks object `key` holds, as (first block, count) in order,
    /// contiguous ones merged; none if it doesn't exist. What isn't in
    /// them is a hole, which a layered read passes through (issue #158).
    pub fn held_runs(&self, key: &ObjKey) -> Option<Vec<(u64, u64)>> {
        let o = self.index.objects.get(key)?;
        let mut out: Vec<(u64, u64)> = Vec::new();
        for (lb, _, crcs) in o.blocks.runs() {
            let n = crcs.len() as u64;
            match out.last_mut() {
                Some((l, c)) if *l + *c == lb => *c += n,
                _ => out.push((lb, n)),
            }
        }
        Some(out)
    }

    /// A digest of which blocks object `key` holds, and how many (D223):
    /// copies with equal [`Shard::digest`]s that differ here differ only in
    /// blocks of zeros one holds where another has a hole. From the index.
    pub fn held(&self, key: &ObjKey) -> Option<(u64, u64)> {
        let mut h = Fnv::new();
        let mut blocks = 0;
        for (lb, n) in self.held_runs(key)? {
            h.u64(lb).u64(n);
            blocks += n;
        }
        Some((h.0, blocks))
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
    /// and its clone's parents, 08-engine.md §7.2). `NotFound` if no layer
    /// exists.
    pub fn read_layered(
        &self,
        layers: &[ObjKey],
        off: u64,
        len: u64,
    ) -> Result<AlignedBuf, EngineError> {
        let plan = self.plan_layered(layers, off, len)?;
        self.read_through(plan)
    }

    /// Plans [`Shard::read_layered`], as [`Shard::read_begin`] does.
    pub fn read_layered_begin(
        &mut self,
        layers: &[ObjKey],
        off: u64,
        len: u64,
    ) -> Result<ReadPlan, EngineError> {
        let plan = self.plan_layered(layers, off, len)?;
        Ok(self.track(plan))
    }

    fn plan_layered(&self, layers: &[ObjKey], off: u64, len: u64) -> Result<ReadPlan, EngineError> {
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
        Ok(self.plan_read(off, len, |lb| {
            objs.iter().find_map(|o| match o.lookup(lb) {
                Loc::Mapped { pblock, crc } => Some((pblock, crc)),
                Loc::Hole => None,
            })
        }))
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
#[derive(Debug, Clone)]
struct Fnv(u64);

/// A digest under way ([`Shard::digest_begin`]).
#[derive(Debug, Clone)]
pub struct Digesting {
    key: ObjKey,
    /// The object's revision when it began: any change to the object
    /// gives it a new one.
    revision: u64,
    h: Fnv,
    /// Into the key–value table yet (else still in the attributes).
    kv: bool,
    /// The last key hashed in the current table.
    after: Option<Vec<u8>>,
}

/// A digest done, or a part of one done ([`Shard::digest_part`]).
#[derive(Debug, Clone)]
pub enum DigestPart {
    Done(u64),
    More(Digesting),
}

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

    /// D266: a record that passes its checksums but names another record
    /// before it (one from another history) refuses the shard, where the
    /// first format would replay it.
    #[test]
    fn a_record_that_doesnt_follow_the_one_before_refuses() {
        let mut sh = small_shard(512);
        let mut lsns = Vec::new();
        for i in 0..3 {
            lsns.push(sh.tail_lsn);
            put(&mut sh, &key(i), 0, vec![i as u8; BLOCK]);
        }
        let (dev, region, salt) = (sh.dev.clone(), sh.region, sh.salt);
        let at = sh.log_pos(lsns[1]);
        drop(sh);
        let mut blk = AlignedBuf::zeroed(BLOCK);
        dev.read(at, &mut blk).unwrap();
        let prev = u32::from_le_bytes(blk[36..40].try_into().unwrap());
        blk[36..40].copy_from_slice(&(prev ^ 1).to_le_bytes());
        let hcrc = crc32c_append(salt, &blk[..HEADER_LEN2 - 4]);
        blk[HEADER_LEN2 - 4..HEADER_LEN2].copy_from_slice(&hcrc.to_le_bytes());
        dev.write(at, &blk).unwrap();
        dev.flush().unwrap();
        match Shard::open(dev, region, 7, 0) {
            Err(EngineError::Corrupt(m)) => assert!(m.contains("doesn't follow"), "{m}"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("opened"),
        }
    }

    /// D267: the salt a record header was written under is found from its
    /// checksum alone.
    #[test]
    fn a_checksum_seed_is_solved() {
        for (i, salt) in [0u32, 1, 0xdead_beef, u32::MAX].into_iter().enumerate() {
            let data: Vec<u8> = (0..36).map(|b| (b * 7 + i) as u8).collect();
            let crc = crc32c_append(salt, &data);
            assert_eq!(crc_seed(&data, crc), Some(salt));
        }
    }

    /// D155: a PG's blocks, summed a slice at a time, without its
    /// neighbours'.
    #[test]
    fn a_pg_is_summed_in_slices() {
        let region = Region {
            start: 0,
            blocks: 2048,
            log_blocks: 64,
        };
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(2048 * BLOCK as u64));
        Shard::format(dev.clone(), region, 7, 0).unwrap();
        let mut sh = Shard::open(dev, region, 7, 0).unwrap();
        let write = |sh: &mut Shard, pg: u32, i: u64, blocks: usize| {
            let key = ObjKey::new(1, pg, Namespace::Vol, format!("o{i}").into_bytes(), 0);
            sh.commit(vec![OpList {
                key,
                ops: vec![Op::Write {
                    off: 0,
                    data: vec![1; blocks * BLOCK].into(),
                    crcs: None,
                }],
            }])
            .unwrap()
            .pop()
            .unwrap()
            .unwrap();
        };
        for i in 0..5 {
            write(&mut sh, 2, i, i as usize + 1);
        }
        write(&mut sh, 1, 9, 7);
        write(&mut sh, 3, 9, 7);
        let sum = |max| {
            let (mut total, mut after, mut slices) = (0, None, 0);
            loop {
                let (b, next) = sh.pg_blocks(1, 2, after.as_ref(), max);
                total += b;
                slices += 1;
                match next {
                    Some(k) => after = Some(k),
                    None => return (total, slices),
                }
            }
        };
        assert_eq!(sum(100), (15, 1));
        assert_eq!(sum(2), (15, 3));
        assert_eq!(sh.pg_blocks(1, 4, None, 2), (0, None));
    }

    /// D177: the PGs a shard holds, each once, across pools and
    /// namespaces, and none once its objects are deleted.
    #[test]
    fn held_pgs_are_listed_once_each() {
        let region = Region {
            start: 0,
            blocks: 2048,
            log_blocks: 64,
        };
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(2048 * BLOCK as u64));
        Shard::format(dev.clone(), region, 7, 0).unwrap();
        let mut sh = Shard::open(dev, region, 7, 0).unwrap();
        assert!(sh.held_pgs().is_empty());
        let keys = [
            ObjKey::new(0, 5, Namespace::Meta, b"h".to_vec(), 0),
            ObjKey::new(1, 2, Namespace::Vol, b"a".to_vec(), 0),
            ObjKey::new(1, 2, Namespace::Vol, b"b".to_vec(), 3),
            ObjKey::new(1, 2, Namespace::Kv, b"c".to_vec(), 0),
            ObjKey::new(1, u32::MAX, Namespace::Vol, b"d".to_vec(), 0),
            ObjKey::new(2, 0, Namespace::Vol, b"e".to_vec(), 0),
        ];
        let lists = keys
            .iter()
            .map(|k| OpList {
                key: k.clone(),
                ops: vec![Op::SetAttr(b"x".to_vec(), b"1".to_vec())],
            })
            .collect();
        assert!(sh.commit(lists).unwrap().iter().all(|r| r.is_ok()));
        assert_eq!(sh.held_pgs(), [(0, 5), (1, 2), (1, u32::MAX), (2, 0)]);
        let deletes = sh
            .list_pg(1, 2)
            .into_iter()
            .map(|key| OpList {
                key,
                ops: vec![Op::Delete],
            })
            .collect();
        sh.commit(deletes).unwrap();
        assert_eq!(sh.held_pgs(), [(0, 5), (1, u32::MAX), (2, 0)]);
    }

    /// A checkpoint with no room is abandoned, not fatal (implementation-
    /// notes.md I162): the shard refuses what takes space, still deletes,
    /// and checkpoints again once room is back; it reopens consistent.
    /// D27: past `full`, what would grow the used space is refused;
    /// overwrites of held blocks, deletes and discards go on.
    #[test]
    fn past_full_only_growth_is_refused() {
        let region = Region {
            start: 0,
            blocks: 2048,
            log_blocks: 64,
        };
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(2048 * BLOCK as u64));
        Shard::format(dev.clone(), region, 7, 0).unwrap();
        let mut sh = Shard::open(dev.clone(), region, 7, 0).unwrap();
        let write = |sh: &mut Shard, i: u64, off: u64, blocks: usize| {
            sh.commit(vec![OpList {
                key: key(i),
                ops: vec![Op::Write {
                    off: off * BLOCK as u64,
                    data: vec![i as u8; blocks * BLOCK].into(),
                    crcs: None,
                }],
            }])
            .unwrap()
            .pop()
            .unwrap()
        };
        // Fill to a little past a fifth, then hold the shard to a fifth.
        let total = sh.region.data_blocks();
        let mut i = 0;
        while sh.used_blocks() * 5 <= total {
            write(&mut sh, i, 0, 16).unwrap();
            i += 1;
        }
        sh.set_full_ratio(200);
        assert!(sh.past(200) && !sh.past(500));
        assert_eq!(
            write(&mut sh, 1000, 0, 1),
            Err(OpError::NoSpace),
            "a new object"
        );
        assert_eq!(
            write(&mut sh, 0, 16, 1),
            Err(OpError::NoSpace),
            "a block not held"
        );
        let kv = |sh: &mut Shard, i: u64| {
            sh.commit(vec![OpList {
                key: key(i),
                ops: vec![Op::KvPut(b"k".to_vec(), b"v".to_vec())],
            }])
            .unwrap()
            .pop()
            .unwrap()
            .map(|_| ())
        };
        assert_eq!(
            kv(&mut sh, 1001),
            Err(OpError::NoSpace),
            "a new object's key"
        );
        kv(&mut sh, 0).expect("a key of an object that exists");
        write(&mut sh, 0, 0, 16).expect("an overwrite of held blocks");
        write(&mut sh, 1, 4, 1).expect("an overwrite of one held block");
        let used = sh.used_blocks();
        sh.commit(vec![OpList {
            key: key(1),
            ops: vec![Op::Delete],
        }])
        .unwrap()
        .pop()
        .unwrap()
        .expect("a delete");
        assert!(sh.used_blocks() < used);
        // Below the limit again, growth goes through; lifted, too.
        while sh.past(200) {
            i -= 1;
            sh.commit(vec![OpList {
                key: key(i),
                ops: vec![Op::Delete],
            }])
            .unwrap()
            .pop()
            .unwrap()
            .unwrap();
        }
        write(&mut sh, 1000, 0, 1).expect("room again");
        sh.set_full_ratio(0);
        write(&mut sh, 1001, 0, 64).expect("no limit");
    }

    #[test]
    fn a_checkpoint_without_room_is_abandoned() {
        let region = Region {
            start: 0,
            blocks: 2048,
            log_blocks: 64,
        };
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(2048 * BLOCK as u64));
        Shard::format(dev.clone(), region, 7, 0).unwrap();
        let mut sh = Shard::open(dev.clone(), region, 7, 0).unwrap();
        sh.set_checkpoint_page(256);
        let put = |sh: &mut Shard, i: u64| {
            sh.commit(vec![OpList {
                key: key(i),
                ops: vec![Op::KvPut(b"k".to_vec(), vec![i as u8; 100])],
            }])
            .unwrap()
            .pop()
            .unwrap()
        };
        for i in 0..60 {
            put(&mut sh, i).unwrap();
        }
        sh.checkpoint().unwrap();
        let bounds = sh.index.bounds.clone();
        for i in 0..60 {
            put(&mut sh, i).unwrap();
        }
        // Every free block taken: the checkpoint the log asks for finds
        // none, and the commit that started it still goes through.
        let stolen = sh.alloc.alloc(sh.alloc.free_blocks()).unwrap();
        let mut i = 0u64;
        while !sh.ckpt_stalled {
            let r = sh
                .commit(vec![OpList {
                    key: key(i % 60),
                    ops: vec![Op::KvDel(b"absent".to_vec())],
                }])
                .expect("a checkpoint without room doesn't fail the commit");
            assert!(r[0].is_ok());
            i += 1;
            assert!(i < 10_000, "no checkpoint was started");
        }
        // And one asked for directly fails alone.
        assert!(matches!(sh.checkpoint(), Err(EngineError::NoSpace)));
        assert!(!sh.poisoned && sh.ckpt_stalled && sh.fuzzy.is_none());
        assert_eq!(sh.index.bounds, bounds, "no bound of the failed one stays");
        assert_eq!(put(&mut sh, 99), Err(OpError::NoSpace), "growth refused");
        let gone = sh
            .commit(vec![OpList {
                key: key(5),
                ops: vec![Op::Delete],
            }])
            .unwrap();
        assert!(gone[0].is_ok(), "deletes still go");
        for (s, n) in stolen {
            sh.alloc.free(s, n);
        }
        // Room again: the next rounds checkpoint, and growth is let through.
        for i in 0..200 {
            let _ = put(&mut sh, i % 50 + 100);
        }
        assert!(!sh.ckpt_stalled, "a checkpoint completed");
        put(&mut sh, 99).unwrap();
        let idx = sh.index.clone();
        drop(sh);
        let sh = Shard::open(dev, region, 7, 0).unwrap();
        assert_eq!(sh.index, idx);
        assert!(!sh.index.objects.contains_key(&key(5)));
    }

    /// A device whose checkpoint is one blob (before D105) opens, and its
    /// first checkpoint replaces the blob with pages and frees it.
    #[test]
    fn a_checkpoint_from_before_pages_is_read_and_replaced() {
        // Only devices of the first format have them.
        let region = Region {
            start: 0,
            blocks: 4096,
            log_blocks: 256,
        };
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(4096 * BLOCK as u64));
        Shard::format_as(dev.clone(), region, 7, 0, false).unwrap();
        let mut sh = Shard::open_as(dev.clone(), region, 7, 0, false).unwrap();
        sh.set_checkpoint_page(512);
        for i in 0..40 {
            sh.commit(vec![OpList {
                key: key(i),
                ops: vec![
                    Op::Write {
                        off: 0,
                        data: vec![i as u8; BLOCK].into(),
                        crcs: None,
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
                copy: Vec::new(),
            }),
            v2: false,
            chain: 0,
        };
        dev.write((region.start + sup.seq % 2) * BLOCK as u64, &sup.encode())
            .unwrap();
        dev.flush().unwrap();
        drop(sh);
        let sh = Shard::open_as(dev.clone(), region, 7, 0, false).unwrap();
        assert_eq!(sh.index, idx, "the blob reads as it was written");
        assert!(sh.legacy_blob.is_empty(), "the blob is freed");
        assert!(sh.stats().checkpoint_pages >= 1);
        drop(sh);
        let sh = Shard::open_as(dev, region, 7, 0, false).unwrap();
        assert_eq!(sh.index, idx, "and the pages that replace it too");
        let used: u64 = sh.index.used_extents().map(|e| e.1).sum::<u64>()
            + sh.pages
                .values()
                .flat_map(|p| &p.runs)
                .map(|r| r.1)
                .sum::<u64>()
            + sh.ckpt
                .as_ref()
                .map_or(0, |c| c.runs.iter().chain(&c.copy).map(|r| r.1).sum());
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

    fn small_shard(blocks: u64) -> Shard {
        let region = Region {
            start: 0,
            blocks,
            log_blocks: 64,
        };
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(blocks * BLOCK as u64));
        Shard::format(dev.clone(), region, 7, 0).unwrap();
        Shard::open(dev, region, 7, 0).unwrap()
    }

    /// The directory's copy follows the superblock's checksum (D269): up
    /// to it, the bytes are a superblock without one, which builds from
    /// before copies read; a damaged copy is read as none.
    #[test]
    fn a_superblock_names_its_directory_copy_after_its_checksum() {
        let mut sup = Super {
            seq: 5,
            device_id: 7,
            shard: 0,
            salt: 9,
            head_lsn: 40,
            ckpt: Some(Ckpt {
                runs: vec![(300, 1)],
                len: 120,
                crc: 77,
                copy: Vec::new(),
            }),
            v2: true,
            chain: 3,
        };
        let without = sup.encode();
        sup.ckpt.as_mut().unwrap().copy = vec![(310, 1)];
        let with = sup.encode();
        assert_eq!(Super::decode(&with), Some(sup.clone()));
        let end = without.iter().rposition(|&b| b != 0).unwrap() + 1;
        assert_eq!(with[..end], without[..end], "older builds read the same");
        let mut damaged = with.to_vec();
        damaged[end + 9] ^= 1;
        let read = Super::decode(&damaged).unwrap();
        assert!(read.ckpt.unwrap().copy.is_empty());
    }

    /// A copy whose blocks something else holds, as when a build from
    /// before copies wrote there, is dropped rather than refuse the shard.
    #[test]
    fn a_directory_copy_written_over_is_dropped() {
        let mut sh = small_shard(2048);
        for i in 0..8 {
            put(&mut sh, &key(i), 0, vec![i as u8; BLOCK]);
        }
        sh.checkpoint().unwrap();
        let (region, dev) = (sh.region, sh.dev.clone());
        // The superblock the checkpoint wrote, named again with the copy
        // moved onto an object's blocks.
        let mut buf = AlignedBuf::zeroed(BLOCK);
        let at = super_slots(&region, sh.v2, sh.super_seq)[0];
        dev.read(at, &mut buf).unwrap();
        let mut sup = Super::decode(&buf).unwrap();
        assert!(!sup.ckpt.as_ref().unwrap().copy.is_empty());
        sup.seq += 1;
        let taken = sh.index.used_extents().next().unwrap();
        sup.ckpt.as_mut().unwrap().copy = vec![taken];
        drop(sh);
        let buf = sup.encode();
        for at in super_slots(&region, sup.v2, sup.seq) {
            dev.write(at, &buf).unwrap();
        }
        dev.flush().unwrap();
        // It opens, and its fresh checkpoint's copy is elsewhere.
        let sh = Shard::open(dev, region, 7, 0).unwrap();
        let copy = sh.checkpoint_layout().directory_copy;
        assert!(!copy.is_empty() && !copy.contains(&taken));
        for i in 0..8 {
            let r = sh.read(&key(i), 0, BLOCK as u64).unwrap();
            assert!(r.iter().all(|b| *b == i as u8));
        }
    }

    /// Scrub gives a checkpoint from before copies one, and counts no
    /// damage for it; a damaged first copy is read past and rewritten.
    #[test]
    fn scrub_writes_a_missing_or_damaged_directory_again() {
        let mut sh = small_shard(2048);
        for i in 0..8 {
            put(&mut sh, &key(i), 0, vec![i as u8; BLOCK]);
        }
        sh.checkpoint().unwrap();
        // As a build from before copies left it.
        let copy = std::mem::take(&mut sh.ckpt.as_mut().unwrap().copy);
        for (s, n) in copy {
            sh.alloc.free(s, n);
        }
        let checkpoints = sh.stats().checkpoints;
        assert!(!sh.scrub_checkpoint_directory().unwrap());
        assert_eq!(sh.stats().checkpoints, checkpoints + 1);
        assert_eq!(sh.stats().checkpoint_damage, 0);
        let l = sh.checkpoint_layout();
        assert!(!l.directory_copy.is_empty());
        assert!(!sh.scrub_checkpoint_directory().unwrap(), "sound now");
        assert_eq!(sh.stats().checkpoints, checkpoints + 1, "and left alone");
        // The first copy damaged while the shard runs: scrub rewrites both.
        let (region, dev) = (sh.region, sh.dev.clone());
        let zero = AlignedBuf::zeroed(BLOCK);
        dev.write(l.directory[0].0 * BLOCK as u64, &zero).unwrap();
        assert!(sh.scrub_checkpoint_directory().unwrap());
        assert_eq!(sh.stats().checkpoint_damage, 1);
        assert_eq!(sh.stats().checkpoints, checkpoints + 2);
        // Damaged again, and the shard stops: it opens from the copy, and
        // its fresh checkpoint writes both again.
        let l = sh.checkpoint_layout();
        drop(sh);
        dev.write(l.directory[0].0 * BLOCK as u64, &zero).unwrap();
        let sh = Shard::open(dev.clone(), region, 7, 0).unwrap();
        assert!(sh.directory_copy_read());
        for i in 0..8 {
            let r = sh.read(&key(i), 0, BLOCK as u64).unwrap();
            assert!(r.iter().all(|b| *b == i as u8));
        }
        drop(sh);
        let sh = Shard::open(dev, region, 7, 0).unwrap();
        assert!(!sh.directory_copy_read(), "both copies are sound again");
    }

    #[test]
    fn freed_runs_merge() {
        let mut m = BTreeMap::new();
        add_run(&mut m, 10, 5);
        add_run(&mut m, 20, 5);
        add_run(&mut m, 15, 5);
        assert_eq!(m.into_iter().collect::<Vec<_>>(), vec![(10, 15)]);
        let mut m = BTreeMap::new();
        add_run(&mut m, 30, 2);
        add_run(&mut m, 10, 2);
        add_run(&mut m, 5, 40);
        add_run(&mut m, 50, 0);
        assert_eq!(m.into_iter().collect::<Vec<_>>(), vec![(5, 40)]);
    }

    /// Freed blocks are discarded only once a checkpoint that began after
    /// they were freed is named (D275), only where still free, and held
    /// out of the allocator while the discard runs.
    #[test]
    fn freed_blocks_are_discarded_after_the_next_checkpoint() {
        let mut sh = small_shard(2048);
        sh.set_discard(true);
        // Right after opening, the free space is queued once, and taken
        // an eighth of what is free at a time.
        let free = sh.stats().free_blocks;
        let first = sh.take_discards(u64::MAX);
        let n: u64 = first.iter().map(|r| r.1).sum();
        assert_eq!(n, free / 8);
        assert_eq!(sh.stats().free_blocks, free - n, "held while in flight");
        sh.discards_done(&first, true);
        assert_eq!(sh.stats().discarded_blocks, n);
        loop {
            let more = sh.take_discards(u64::MAX);
            if more.is_empty() {
                break;
            }
            sh.discards_done(&more, true);
        }
        assert_eq!(sh.stats().discard_queued_blocks, 0);
        put(&mut sh, &key(2), 0, vec![2; 4 * BLOCK]);
        let kept: Vec<(u64, u64)> = sh.index.used_extents().collect();
        put(&mut sh, &key(1), 0, vec![1; 64 * BLOCK]);
        let old: Vec<(u64, u64)> = sh
            .index
            .used_extents()
            .filter(|e| !kept.contains(e))
            .collect();
        // Deleted: its blocks are freed, but not ready.
        sh.commit(vec![OpList {
            key: key(1),
            ops: vec![Op::Delete],
        }])
        .unwrap();
        assert!(sh.take_discards(u64::MAX).is_empty());
        assert!(sh.stats().discard_queued_blocks >= 64);
        sh.checkpoint().unwrap();
        // Freed before the checkpoint began: ready now, unless reused.
        let mut ready = Vec::new();
        while let Some(more) = Some(sh.take_discards(u64::MAX)).filter(|r| !r.is_empty()) {
            ready.extend(more);
        }
        let ready_blocks: Vec<u64> = ready.iter().flat_map(|&(s, n)| s..s + n).collect();
        // In use: objects' blocks, and the checkpoint's own.
        let l = sh.checkpoint_layout();
        let live: Vec<u64> = sh
            .index
            .used_extents()
            .chain(l.directory.iter().copied())
            .chain(l.directory_copy.iter().copied())
            .chain(l.pages.iter().flatten().copied())
            .flat_map(|(s, n)| s..s + n)
            .collect();
        assert!(ready_blocks.iter().all(|b| !live.contains(b)), "never live");
        for (s, n) in old {
            for b in s..s + n {
                assert!(
                    ready_blocks.contains(&b) || live.contains(&b),
                    "block {b}: freed and free, so discarded"
                );
            }
        }
        assert!(!ready.is_empty());
        sh.discards_done(&ready, false);
        let r = sh.read(&key(2), 0, 4 * BLOCK as u64).unwrap();
        assert!(r.iter().all(|b| *b == 2));
    }

    /// Started with records in the log, only what is freed from then on
    /// is queued: free space may be freed by records not yet checkpointed.
    #[test]
    fn free_space_is_queued_only_right_after_opening() {
        let mut sh = small_shard(2048);
        put(&mut sh, &key(1), 0, vec![1; BLOCK]);
        sh.set_discard(true);
        assert_eq!(sh.stats().discard_queued_blocks, 0);
        sh.set_discard(false);
        assert!(sh.take_discards(u64::MAX).is_empty());
    }

    /// Discards are taken at most `max_blocks` at a time, and the rest
    /// stays queued.
    #[test]
    fn discards_are_taken_in_bounded_batches() {
        let mut sh = small_shard(2048);
        sh.set_discard(true);
        let first = sh.take_discards(100);
        assert_eq!(first.iter().map(|r| r.1).sum::<u64>(), 100);
        let rest = sh.take_discards(u64::MAX);
        assert!(!rest.is_empty());
        assert!(
            rest.iter()
                .all(|&(s, _)| first.iter().all(|&(f, n)| s >= f + n || s < f))
        );
        sh.discards_done(&first, true);
        sh.discards_done(&rest, true);
        while let Some(more) = Some(sh.take_discards(u64::MAX)).filter(|r| !r.is_empty()) {
            sh.discards_done(&more, true);
        }
        assert_eq!(sh.stats().discard_queued_blocks, 0);
    }

    fn put(sh: &mut Shard, k: &ObjKey, off: u64, data: Vec<u8>) {
        sh.commit(vec![OpList {
            key: k.clone(),
            ops: vec![Op::Write {
                off,
                data: data.into(),
                crcs: None,
            }],
        }])
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    }

    /// Makes a planned read's device reads now, as a ring would.
    fn complete(sh: &Shard, plan: &mut ReadPlan) -> AlignedBuf {
        let mut buf = plan.take_buf();
        for &(dev_off, at, len) in plan.ios() {
            sh.dev.read(dev_off, &mut buf[at..at + len]).unwrap();
        }
        buf
    }

    /// D190: an extent freed while a read that may use it is in flight
    /// isn't reused until the read ends, so the read gets what it planned.
    #[test]
    fn freed_extents_wait_for_reads_in_flight() {
        let mut sh = small_shard(512);
        sh.set_defer_max(0);
        let a = key(1);
        put(&mut sh, &a, 0, vec![1; 8 * BLOCK]);
        let mut plan = sh.read_begin(&a, 0, 8 * BLOCK as u64).unwrap();
        assert_eq!(sh.reads_in_flight(), 1);
        // Overwritten elsewhere: its old blocks are freed, and held.
        put(&mut sh, &a, 0, vec![2; 8 * BLOCK]);
        assert_eq!(sh.held_blocks(), 8);
        // Everything else free is taken and written over.
        let rest = sh.stats().free_blocks - 32;
        for i in 0..rest / 8 {
            put(&mut sh, &key(100 + i), 0, vec![3; 8 * BLOCK]);
        }
        let buf = complete(&sh, &mut plan);
        let before = sh.stats().free_blocks;
        sh.read_end(&plan);
        let got = plan.finish(buf).unwrap();
        assert!(got.iter().all(|b| *b == 1), "the read got what it planned");
        assert_eq!(sh.held_blocks(), 0);
        assert_eq!(sh.reads_in_flight(), 0);
        assert_eq!(
            sh.stats().free_blocks,
            before + 8,
            "the held blocks are free"
        );
        assert!(
            sh.read(&a, 0, BLOCK as u64)
                .unwrap()
                .iter()
                .all(|b| *b == 2)
        );
    }

    /// D190: a read ended out of order frees only what no older read may
    /// still use.
    #[test]
    fn held_extents_follow_the_oldest_read() {
        let mut sh = small_shard(512);
        sh.set_defer_max(0);
        let (a, b) = (key(1), key(2));
        put(&mut sh, &a, 0, vec![1; 4 * BLOCK]);
        put(&mut sh, &b, 0, vec![1; 4 * BLOCK]);
        let old = sh.read_begin(&a, 0, 4 * BLOCK as u64).unwrap();
        put(&mut sh, &a, 0, vec![2; 4 * BLOCK]);
        let new = sh.read_begin(&b, 0, 4 * BLOCK as u64).unwrap();
        put(&mut sh, &b, 0, vec![2; 4 * BLOCK]);
        assert_eq!(sh.held_blocks(), 8);
        sh.read_end(&new);
        assert_eq!(sh.held_blocks(), 8, "the older read may use both");
        sh.read_end(&old);
        assert_eq!(sh.held_blocks(), 0);
    }

    /// D190: a write isn't deferred onto blocks while a read is in flight:
    /// the read may have planned their old data from the device.
    #[test]
    fn nothing_is_deferred_under_a_read() {
        let mut sh = small_shard(512);
        sh.set_defer_max(DEFER_MAX);
        let a = key(1);
        put(&mut sh, &a, 0, vec![1; 4 * BLOCK]);
        let plan = sh.read_begin(&a, 0, 4 * BLOCK as u64).unwrap();
        put(&mut sh, &a, 0, vec![2; BLOCK]);
        assert_eq!(sh.pending_blocks(), 0, "redirected");
        sh.read_end(&plan);
        put(&mut sh, &a, BLOCK as u64, vec![3; BLOCK]);
        assert_eq!(sh.pending_blocks(), 1, "deferred again");
    }

    /// D190, against a model: reads begun at random, completed late and
    /// ended in any order while objects are overwritten, deleted and
    /// written, each get the content they began with; once all end, no
    /// space is lost.
    #[test]
    fn reads_in_flight_against_a_model() {
        use rand::rngs::SmallRng;
        use rand::{RngExt, SeedableRng};
        for seed in 0..20u64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            let mut sh = small_shard(1024);
            sh.set_defer_max(if seed % 2 == 0 { 0 } else { DEFER_MAX });
            let used = |sh: &Shard| {
                sh.ckpt.as_ref().map_or(0, |c| {
                    c.runs.iter().chain(&c.copy).map(|r| r.1).sum::<u64>()
                }) + sh
                    .pages
                    .values()
                    .flat_map(|p| &p.runs)
                    .map(|r| r.1)
                    .sum::<u64>()
            };
            let free0 = sh.stats().free_blocks + used(&sh);
            let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
            let mut reads: Vec<(ReadPlan, Vec<u8>)> = Vec::new();
            let mut fill = 0u8;
            for _ in 0..300 {
                let k = rng.random_range(0..6u64);
                match rng.random_range(0..10) {
                    0..=3 => {
                        fill = fill.wrapping_add(1);
                        let blocks = rng.random_range(1..=4usize);
                        let off = rng.random_range(0..4u64) * BLOCK as u64;
                        put(&mut sh, &key(k), off, vec![fill; blocks * BLOCK]);
                        let m = model.entry(k).or_default();
                        let end = off as usize + blocks * BLOCK;
                        if m.len() < end {
                            m.resize(end, 0);
                        }
                        m[off as usize..end].fill(fill);
                    }
                    4 => {
                        sh.commit(vec![OpList {
                            key: key(k),
                            ops: vec![Op::Delete],
                        }])
                        .unwrap();
                        model.remove(&k);
                    }
                    5..=7 => {
                        if let Some(m) = model.get(&k) {
                            let len = m.len().div_ceil(BLOCK) * BLOCK;
                            let plan = sh.read_begin(&key(k), 0, len as u64).unwrap();
                            let mut want = m.clone();
                            want.resize(len, 0);
                            reads.push((plan, want));
                        }
                    }
                    _ => {
                        if !reads.is_empty() {
                            let (mut plan, want) =
                                reads.swap_remove(rng.random_range(0..reads.len()));
                            let buf = complete(&sh, &mut plan);
                            sh.read_end(&plan);
                            assert_eq!(plan.finish(buf).unwrap()[..], want[..], "seed {seed}");
                        }
                    }
                }
            }
            for (mut plan, want) in reads.drain(..) {
                let buf = complete(&sh, &mut plan);
                sh.read_end(&plan);
                assert_eq!(plan.finish(buf).unwrap()[..], want[..], "seed {seed}");
            }
            assert_eq!(sh.held_blocks(), 0);
            for k in model.keys().copied().collect::<Vec<_>>() {
                sh.commit(vec![OpList {
                    key: key(k),
                    ops: vec![Op::Delete],
                }])
                .unwrap();
            }
            sh.checkpoint().unwrap();
            sh.apply_deferred().unwrap();
            assert_eq!(
                sh.stats().free_blocks + used(&sh),
                free0,
                "seed {seed}: no space lost"
            );
        }
    }
}
