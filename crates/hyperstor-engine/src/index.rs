//! The in-memory index and the logged form of operations.
//!
//! A commit turns each [`crate::Op`] into resolved operations ([`ROp`]):
//! physical block numbers and checksums decided, nothing left to choose.
//! The log records resolved operations, and both a commit and recovery
//! apply them with [`Index::apply`], so replay reproduces exactly the
//! state the commit produced.

pub use crate::blockmap::{BlockMap, DensePolicy, Loc, Run};
use crate::types::ObjKey;
use hyperstor_codec::{Dec, DecodeError, Enc};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Object {
    pub size: u64,
    pub revision: u64,
    /// Logical to physical blocks, as runs or a dense map (D43).
    pub blocks: BlockMap,
    pub attrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub kv: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Object {
    pub fn block_count(&self) -> u64 {
        self.blocks.block_count()
    }

    pub fn lookup(&self, lb: u64) -> Loc {
        self.blocks.lookup(lb)
    }

    /// True if every block of `[lb, lb + n)` is mapped.
    pub fn all_mapped(&self, lb: u64, n: u64) -> bool {
        self.blocks.all_mapped(lb, n)
    }

    /// Removes the mappings of `[lb, lb + n)` and returns the physical
    /// extents they held.
    pub fn unmap(&mut self, lb: u64, n: u64) -> Vec<(u64, u64)> {
        self.blocks.unmap(lb, n)
    }
}

/// An in-place write ([`ROp::Deferred`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredWrite {
    pub lb: u64,
    pub data: Vec<u8>,
    pub crcs: Vec<u32>,
    /// The physical blocks at the time; empty in records written before
    /// they were logged.
    pub pbs: Vec<u64>,
}

/// A resolved operation, as logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ROp {
    /// Removes the object and frees its blocks.
    Delete,
    SetSize(u64),
    /// Maps `[lb, lb + crcs.len())` to physical blocks from `pblock`; any
    /// earlier mapping of those blocks is freed.
    Map {
        lb: u64,
        pblock: u64,
        crcs: Vec<u32>,
    },
    /// Overwrites mapped blocks in place; the data is in the log until
    /// applied (spec/08-engine.md §7.3). Boxed: rare, and the largest.
    Deferred(Box<DeferredWrite>),
    Unmap {
        lb: u64,
        n: u64,
    },
    SetAttr(Vec<u8>, Vec<u8>),
    RmAttr(Vec<u8>),
    KvPut(Vec<u8>, Vec<u8>),
    KvDel(Vec<u8>),
    KvDelRange(Vec<u8>, Vec<u8>),
    /// Sets the revision; ends every list that leaves the object existing.
    SetRevision(u64),
    /// Removes the mappings of blocks another object has taken over
    /// ([`crate::Op::Absorb`]), without freeing them.
    Forget {
        lb: u64,
        n: u64,
    },
}

/// One list's resolved operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RList {
    pub key: ObjKey,
    pub ops: Vec<ROp>,
}

/// Side effects of applying resolved operations.
#[derive(Debug, Default)]
pub struct Effects {
    /// Physical extents no longer referenced.
    pub freed: Vec<(u64, u64)>,
    /// In-place writes still to do: physical block -> 4 KiB of data.
    /// Freeing a block cancels its pending write.
    pub pending: BTreeMap<u64, Box<[u8]>>,
    /// Pending writes replay found no mapping for, at the block the record
    /// names: kept only if some object maps the block with the data's
    /// checksum once replay ends ([`Index::settle_tentative`]).
    pub tentative: BTreeSet<u64>,
}

impl Effects {
    fn free(&mut self, extents: Vec<(u64, u64)>) {
        for (pb, n) in extents {
            let cancel: Vec<u64> = self.pending.range(pb..pb + n).map(|(&k, _)| k).collect();
            for k in cancel {
                self.pending.remove(&k);
                self.tentative.remove(&k);
            }
            self.freed.push((pb, n));
        }
    }
}

/// The shard's index of objects.
#[derive(Debug, Default, Clone)]
pub struct Index {
    pub objects: BTreeMap<ObjKey, Object>,
    /// When block maps switch representation (not part of the content).
    pub policy: DensePolicy,
    /// Where checkpoint pages start (D105): a page holds the objects from
    /// its bound up to the next; the first page starts before every key
    /// and has no bound.
    pub bounds: BTreeSet<ObjKey>,
    /// Pages changed since the checkpoint that wrote them began, by their
    /// bound; `first_dirty` for the first page.
    pub dirty: BTreeSet<ObjKey>,
    pub first_dirty: bool,
}

impl PartialEq for Index {
    fn eq(&self, other: &Self) -> bool {
        self.objects == other.objects
    }
}

impl Eq for Index {}

impl Index {
    /// Applies one resolved list.
    pub fn apply(&mut self, list: &RList, fx: &mut Effects) {
        for op in &list.ops {
            self.apply_op(&list.key, op, fx);
        }
    }

    /// The start of the page that holds `key`.
    pub fn page_of(&self, key: &ObjKey) -> Option<&ObjKey> {
        self.bounds.range(..=key).next_back()
    }

    fn touch(&mut self, key: &ObjKey) {
        match self.page_of(key) {
            None => self.first_dirty = true,
            Some(p) if !self.dirty.contains(p) => {
                let p = p.clone();
                self.dirty.insert(p);
            }
            Some(_) => {}
        }
    }

    /// Marks a page (by its bound) to be written by the next checkpoint.
    pub fn mark(&mut self, page: Option<ObjKey>) {
        match page {
            None => self.first_dirty = true,
            Some(p) => {
                self.dirty.insert(p);
            }
        }
    }

    /// The changed pages, in key order; none are marked after.
    pub fn take_dirty(&mut self) -> Vec<Option<ObjKey>> {
        let first = std::mem::take(&mut self.first_dirty).then_some(None);
        first
            .into_iter()
            .chain(std::mem::take(&mut self.dirty).into_iter().map(Some))
            .collect()
    }

    /// Applies one resolved operation to the object `key`.
    pub fn apply_op(&mut self, key: &ObjKey, op: &ROp, fx: &mut Effects) {
        let policy = self.policy;
        self.touch(key);
        {
            if let ROp::Delete = op {
                if let Some(o) = self.objects.remove(key) {
                    fx.free(o.blocks.extents());
                }
                return;
            }
            let o = match self.objects.get_mut(key) {
                Some(o) => o,
                None => self.objects.entry(key.clone()).or_default(),
            };
            match op {
                ROp::Delete => unreachable!(),
                ROp::SetSize(s) => o.size = *s,
                ROp::Map { lb, pblock, crcs } => {
                    // Replayed over a fuzzy checkpoint that already has
                    // this mapping, the blocks it maps aren't freed (that
                    // would cancel their pending writes).
                    let (lo, hi) = (*pblock, pblock + crcs.len() as u64);
                    let mut freed = o.unmap(*lb, crcs.len() as u64);
                    if freed.iter().any(|&(s, n)| s < hi && lo < s + n) {
                        freed = freed
                            .into_iter()
                            .flat_map(|(s, n)| {
                                let e = s + n;
                                [(s, e.min(lo)), (s.max(hi), e)]
                            })
                            .filter(|(s, e)| s < e)
                            .map(|(s, e)| (s, e - s))
                            .collect();
                    }
                    fx.free(freed);
                    o.blocks.map(*lb, *pblock, crcs);
                    o.blocks.rebalance(&policy, crcs.len() as u64);
                }
                ROp::Deferred(w) => {
                    let DeferredWrite {
                        lb,
                        data,
                        crcs,
                        pbs,
                    } = &**w;
                    // At run time every target block is mapped. Replaying
                    // over a fuzzy checkpoint may find holes: the block was
                    // trimmed later, or moved to another object by an
                    // absorb. The write is kept at the block the record
                    // names, tentatively: whether it still belongs there is
                    // known once replay ends.
                    // A record names the blocks it wrote; one written
                    // before it named them goes to whatever block the
                    // object maps.
                    for (i, crc) in crcs.iter().enumerate() {
                        let b = lb + i as u64;
                        let blk = &data[i * crate::BLOCK..(i + 1) * crate::BLOCK];
                        let now = match o.lookup(b) {
                            Loc::Mapped { pblock, .. } => Some(pblock),
                            Loc::Hole => None,
                        };
                        match (now, pbs.get(i)) {
                            (Some(pb), None) => {
                                o.blocks.set_crcs(b, &[*crc]);
                                fx.pending.insert(pb, blk.into());
                            }
                            (Some(pb), Some(&was)) if pb == was => {
                                o.blocks.set_crcs(b, &[*crc]);
                                fx.pending.insert(pb, blk.into());
                                fx.tentative.remove(&pb);
                            }
                            (_, Some(&was)) => {
                                fx.pending.insert(was, blk.into());
                                fx.tentative.insert(was);
                            }
                            (None, None) => {}
                        }
                    }
                }
                ROp::Unmap { lb, n } => {
                    let freed = o.unmap(*lb, *n);
                    fx.free(freed);
                    o.blocks.rebalance(&policy, *n);
                }
                ROp::SetAttr(k, v) => {
                    o.attrs.insert(k.clone(), v.clone());
                }
                ROp::RmAttr(k) => {
                    o.attrs.remove(k);
                }
                ROp::KvPut(k, v) => {
                    o.kv.insert(k.clone(), v.clone());
                }
                ROp::KvDel(k) => {
                    o.kv.remove(k);
                }
                ROp::KvDelRange(a, b) => {
                    if a < b {
                        let keys: Vec<Vec<u8>> =
                            o.kv.range(a.clone()..b.clone())
                                .map(|(k, _)| k.clone())
                                .collect();
                        for k in keys {
                            o.kv.remove(&k);
                        }
                    }
                }
                ROp::SetRevision(r) => o.revision = *r,
                ROp::Forget { lb, n } => {
                    // Now another object's: not freed.
                    let _ = o.unmap(*lb, *n);
                    o.blocks.rebalance(&policy, *n);
                }
            }
        }
    }

    /// Drops the tentative pending writes whose block no object maps with
    /// the data's checksum: the block was freed, or reused for other data.
    /// One that matches belongs to the object the block moved to.
    pub fn settle_tentative(&self, fx: &mut Effects) {
        if fx.tentative.is_empty() {
            return;
        }
        let mut found: BTreeMap<u64, u32> = BTreeMap::new();
        for o in self.objects.values() {
            for (_, pb, crcs) in o.blocks.runs() {
                for (pb, crc) in fx
                    .tentative
                    .range(pb..pb + crcs.len() as u64)
                    .map(|&t| (t, crcs[(t - pb) as usize]))
                {
                    found.insert(pb, crc);
                }
            }
        }
        for pb in std::mem::take(&mut fx.tentative) {
            let keep = fx
                .pending
                .get(&pb)
                .is_some_and(|d| found.get(&pb) == Some(&hyperstor_codec::crc32c(d)));
            if !keep {
                fx.pending.remove(&pb);
            }
        }
    }

    /// Every physical extent in use.
    pub fn used_extents(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.objects.values().flat_map(|o| o.blocks.extents())
    }

    pub fn encode(&self, e: &mut Enc) {
        e.u64(self.objects.len() as u64);
        for (k, o) in &self.objects {
            encode_object(k, o, e);
        }
    }

    pub fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let n = d.u64()?;
        let mut objects = BTreeMap::new();
        for _ in 0..n {
            let (k, o) = decode_object(d)?;
            objects.insert(k, o);
        }
        Ok(Self {
            objects,
            ..Default::default()
        })
    }
}

/// Encodes one object with its key (checkpoints write objects one by one).
pub fn encode_object(k: &ObjKey, o: &Object, e: &mut Enc) {
    k.encode(e);
    e.u64(o.size).u64(o.revision);
    match &o.blocks {
        BlockMap::Runs(m) => {
            e.u8(0).u32(m.len() as u32);
            for (lb, r) in m {
                e.u64(*lb).u64(r.pblock).u32(r.crcs.len() as u32);
                for c in &r.crcs {
                    e.u32(*c);
                }
            }
        }
        BlockMap::Dense(v) => {
            e.u8(1).u32(v.len() as u32);
            for slot in v {
                e.u64(*slot);
            }
        }
    }
    e.u32(o.attrs.len() as u32);
    for (k, v) in &o.attrs {
        e.bytes(k).bytes(v);
    }
    e.u64(o.kv.len() as u64);
    for (k, v) in &o.kv {
        e.bytes(k).bytes(v);
    }
}

pub fn decode_object(d: &mut Dec<'_>) -> Result<(ObjKey, Object), DecodeError> {
    let k = ObjKey::decode(d)?;
    let mut o = Object {
        size: d.u64()?,
        revision: d.u64()?,
        ..Default::default()
    };
    o.blocks = match d.u8()? {
        0 => {
            let mut m = BTreeMap::new();
            for _ in 0..d.u32()? {
                let lb = d.u64()?;
                let pblock = d.u64()?;
                let crcs = d.seq(4, |d| d.u32())?;
                m.insert(lb, Run { pblock, crcs });
            }
            BlockMap::Runs(m)
        }
        1 => BlockMap::Dense(d.seq(8, |d| d.u64())?),
        v => {
            return Err(DecodeError::Invalid {
                field: "block map",
                value: v as u64,
            });
        }
    };
    let na = d.u32()?;
    for _ in 0..na {
        o.attrs.insert(d.bytes()?.to_vec(), d.bytes()?.to_vec());
    }
    let nk = d.u64()?;
    for _ in 0..nk {
        o.kv.insert(d.bytes()?.to_vec(), d.bytes()?.to_vec());
    }
    Ok((k, o))
}

impl ROp {
    pub fn encode(&self, e: &mut Enc) {
        match self {
            ROp::Delete => {
                e.u8(1);
            }
            ROp::SetSize(s) => {
                e.u8(2).u64(*s);
            }
            ROp::Map { lb, pblock, crcs } => {
                e.u8(3).u64(*lb).u64(*pblock).seq(crcs, |e, c| {
                    e.u32(*c);
                });
            }
            ROp::Deferred(w) => {
                let DeferredWrite {
                    lb,
                    data,
                    crcs,
                    pbs,
                } = &**w;
                e.u8(13).u64(*lb).bytes(data).seq(crcs, |e, c| {
                    e.u32(*c);
                });
                e.seq(pbs, |e, p| {
                    e.u64(*p);
                });
            }
            ROp::Unmap { lb, n } => {
                e.u8(5).u64(*lb).u64(*n);
            }
            ROp::SetAttr(k, v) => {
                e.u8(6).bytes(k).bytes(v);
            }
            ROp::RmAttr(k) => {
                e.u8(7).bytes(k);
            }
            ROp::KvPut(k, v) => {
                e.u8(8).bytes(k).bytes(v);
            }
            ROp::KvDel(k) => {
                e.u8(9).bytes(k);
            }
            ROp::KvDelRange(a, b) => {
                e.u8(10).bytes(a).bytes(b);
            }
            ROp::SetRevision(r) => {
                e.u8(11).u64(*r);
            }
            ROp::Forget { lb, n } => {
                e.u8(12).u64(*lb).u64(*n);
            }
        }
    }

    pub fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => ROp::Delete,
            2 => ROp::SetSize(d.u64()?),
            3 => ROp::Map {
                lb: d.u64()?,
                pblock: d.u64()?,
                crcs: d.seq(4, |d| d.u32())?,
            },
            t @ (4 | 13) => {
                let lb = d.u64()?;
                let data = d.bytes()?.to_vec();
                let crcs = d.seq(4, |d| d.u32())?;
                let pbs = if t == 13 {
                    d.seq(8, |d| d.u64())?
                } else {
                    Vec::new()
                };
                if data.len() != crcs.len() * crate::BLOCK || (t == 13 && pbs.len() != crcs.len()) {
                    return Err(DecodeError::Invalid {
                        field: "deferred length",
                        value: data.len() as u64,
                    });
                }
                ROp::Deferred(Box::new(DeferredWrite {
                    lb,
                    data,
                    crcs,
                    pbs,
                }))
            }
            5 => ROp::Unmap {
                lb: d.u64()?,
                n: d.u64()?,
            },
            6 => ROp::SetAttr(d.bytes()?.to_vec(), d.bytes()?.to_vec()),
            7 => ROp::RmAttr(d.bytes()?.to_vec()),
            8 => ROp::KvPut(d.bytes()?.to_vec(), d.bytes()?.to_vec()),
            9 => ROp::KvDel(d.bytes()?.to_vec()),
            10 => ROp::KvDelRange(d.bytes()?.to_vec(), d.bytes()?.to_vec()),
            11 => ROp::SetRevision(d.u64()?),
            12 => ROp::Forget {
                lb: d.u64()?,
                n: d.u64()?,
            },
            v => {
                return Err(DecodeError::Invalid {
                    field: "resolved op",
                    value: v as u64,
                });
            }
        })
    }
}

impl RList {
    pub fn encode(&self, e: &mut Enc) {
        self.key.encode(e);
        e.u32(self.ops.len() as u32);
        for op in &self.ops {
            op.encode(e);
        }
    }

    pub fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let key = ObjKey::decode(d)?;
        let n = d.u32()?;
        let mut ops = Vec::new();
        for _ in 0..n {
            ops.push(ROp::decode(d)?);
        }
        Ok(Self { key, ops })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Namespace;

    fn key() -> ObjKey {
        ObjKey::new(1, 2, Namespace::Internal, b"k".to_vec(), 0)
    }

    #[test]
    fn map_unmap_split() {
        let mut idx = Index::default();
        let mut fx = Effects::default();
        let l = RList {
            key: key(),
            ops: vec![
                ROp::Map {
                    lb: 0,
                    pblock: 100,
                    crcs: vec![1, 2, 3, 4],
                },
                ROp::SetRevision(5),
            ],
        };
        idx.apply(&l, &mut fx);
        let o = idx.objects.get(&key()).unwrap();
        assert_eq!(
            o.lookup(2),
            Loc::Mapped {
                pblock: 102,
                crc: 3
            }
        );
        assert!(o.all_mapped(0, 4));
        assert!(!o.all_mapped(0, 5));
        // Remap the middle two blocks.
        idx.apply(
            &RList {
                key: key(),
                ops: vec![ROp::Map {
                    lb: 1,
                    pblock: 200,
                    crcs: vec![9, 9],
                }],
            },
            &mut fx,
        );
        assert_eq!(fx.freed, vec![(101, 2)]);
        let o = idx.objects.get(&key()).unwrap();
        assert_eq!(
            o.lookup(0),
            Loc::Mapped {
                pblock: 100,
                crc: 1
            }
        );
        assert_eq!(
            o.lookup(2),
            Loc::Mapped {
                pblock: 201,
                crc: 9
            }
        );
        assert_eq!(
            o.lookup(3),
            Loc::Mapped {
                pblock: 103,
                crc: 4
            }
        );
        assert_eq!(o.block_count(), 4);
    }

    #[test]
    fn deferred_then_free_cancels_pending() {
        let mut idx = Index::default();
        let mut fx = Effects::default();
        idx.apply(
            &RList {
                key: key(),
                ops: vec![ROp::Map {
                    lb: 0,
                    pblock: 10,
                    crcs: vec![0, 0],
                }],
            },
            &mut fx,
        );
        idx.apply(
            &RList {
                key: key(),
                ops: vec![ROp::Deferred(Box::new(DeferredWrite {
                    lb: 1,
                    data: vec![7; crate::BLOCK],
                    crcs: vec![42],
                    pbs: vec![11],
                }))],
            },
            &mut fx,
        );
        assert!(fx.pending.contains_key(&11));
        idx.apply(
            &RList {
                key: key(),
                ops: vec![ROp::Unmap { lb: 0, n: u64::MAX }],
            },
            &mut fx,
        );
        assert!(fx.pending.is_empty(), "freeing cancels the in-place write");
    }

    fn map(lb: u64, pblock: u64, crcs: Vec<u32>) -> ROp {
        ROp::Map { lb, pblock, crcs }
    }

    fn deferred(lb: u64, byte: u8, pb: u64) -> ROp {
        let data = vec![byte; crate::BLOCK];
        ROp::Deferred(Box::new(DeferredWrite {
            lb,
            crcs: vec![hyperstor_codec::crc32c(&data)],
            data,
            pbs: vec![pb],
        }))
    }

    fn other() -> ObjKey {
        ObjKey::new(1, 1, Namespace::Vol, b"other".to_vec(), 0)
    }

    fn crc_of(idx: &Index, k: &ObjKey, lb: u64) -> Loc {
        idx.objects.get(k).map_or(Loc::Hole, |o| o.lookup(lb))
    }

    /// Replaying over a fuzzy checkpoint written after an absorb moved a
    /// block (01-implementation-notes.md I149): the source no longer maps it,
    /// so the deferred write waits; the block's new owner maps it with the
    /// write's checksum, so it is kept.
    #[test]
    fn a_deferred_write_follows_its_block_to_another_object() {
        let mut idx = Index::default();
        let mut fx = Effects::default();
        let d = deferred(9, 7, 72);
        let ROp::Deferred(w) = &d else { unreachable!() };
        let crc = w.crcs[0];
        // The checkpoint: `other` already holds block 72 with the new crc.
        idx.apply_op(&other(), &map(9, 72, vec![crc]), &mut fx);
        // Replay: the deferred write on key(), then the absorb.
        idx.apply_op(&key(), &d, &mut fx);
        assert!(fx.tentative.contains(&72));
        idx.apply_op(&key(), &ROp::Forget { lb: 9, n: 1 }, &mut fx);
        idx.apply_op(&other(), &map(9, 72, vec![crc]), &mut fx);
        assert!(
            fx.pending.contains_key(&72),
            "mapping a block again doesn't free it"
        );
        idx.settle_tentative(&mut fx);
        assert_eq!(fx.pending[&72][0], 7, "the write is kept");
        assert!(fx.tentative.is_empty());
    }

    /// A deferred write whose block was remapped later: the checkpoint
    /// maps the logical block elsewhere, so neither its crc nor the new
    /// block's data change; the old block was freed, so the write goes.
    #[test]
    fn a_deferred_write_to_a_block_since_remapped_is_dropped() {
        let mut idx = Index::default();
        let mut fx = Effects::default();
        idx.apply_op(&key(), &map(10, 184, vec![5]), &mut fx);
        idx.apply_op(&key(), &deferred(10, 7, 126), &mut fx);
        assert_eq!(
            crc_of(&idx, &key(), 10),
            Loc::Mapped {
                pblock: 184,
                crc: 5
            }
        );
        assert!(!fx.pending.contains_key(&184));
        idx.apply_op(&key(), &map(10, 184, vec![5]), &mut fx);
        idx.settle_tentative(&mut fx);
        assert!(fx.pending.is_empty(), "block 126 belongs to no one");
        // Reused for other data by another object: dropped too.
        idx.apply_op(&key(), &deferred(10, 7, 126), &mut fx);
        idx.apply_op(&other(), &map(0, 126, vec![99]), &mut fx);
        idx.settle_tentative(&mut fx);
        assert!(fx.pending.is_empty(), "block 126 holds other data now");
    }

    /// A record from before deferred writes named their blocks replays as
    /// it did: onto whatever block the object maps, skipping holes.
    #[test]
    fn an_old_deferred_record_goes_where_the_object_maps() {
        let mut idx = Index::default();
        let mut fx = Effects::default();
        idx.apply_op(&key(), &map(0, 40, vec![1]), &mut fx);
        let old = ROp::Deferred(Box::new(DeferredWrite {
            lb: 0,
            data: vec![3; 2 * crate::BLOCK],
            crcs: vec![8, 9],
            pbs: Vec::new(),
        }));
        idx.apply_op(&key(), &old, &mut fx);
        assert_eq!(crc_of(&idx, &key(), 0), Loc::Mapped { pblock: 40, crc: 8 });
        assert_eq!(fx.pending.keys().copied().collect::<Vec<_>>(), vec![40]);
        assert!(fx.tentative.is_empty());
        let mut e = Enc::new();
        old.encode(&mut e);
        assert_eq!(e.as_slice()[0], 13, "written with its blocks");
        let mut b = e.into_vec();
        // Tag 4: a record written before the blocks, without them.
        b[0] = 4;
        b.truncate(b.len() - 4);
        assert_eq!(ROp::decode(&mut Dec::new(&b)).unwrap(), old);
    }

    /// Changing an object marks its page for the next checkpoint (D105).
    #[test]
    fn changes_mark_their_page() {
        let mut idx = Index::default();
        let mut fx = Effects::default();
        let k = |n: &[u8]| ObjKey::new(1, 1, Namespace::Vol, n.to_vec(), 0);
        idx.bounds.insert(k(b"m"));
        idx.apply_op(&k(b"a"), &ROp::SetSize(1), &mut fx);
        idx.apply_op(&k(b"m"), &ROp::SetSize(1), &mut fx);
        idx.apply_op(&k(b"z"), &ROp::SetSize(1), &mut fx);
        assert_eq!(idx.take_dirty(), vec![None, Some(k(b"m"))]);
        assert_eq!(idx.take_dirty(), Vec::<Option<ObjKey>>::new());
    }

    #[test]
    fn encode_round_trip() {
        let mut idx = Index::default();
        let mut fx = Effects::default();
        let l = RList {
            key: key(),
            ops: vec![
                ROp::Map {
                    lb: 3,
                    pblock: 7,
                    crcs: vec![1, 2],
                },
                ROp::SetSize(20000),
                ROp::SetAttr(b"a".to_vec(), b"b".to_vec()),
                ROp::KvPut(b"x".to_vec(), b"y".to_vec()),
                ROp::KvPut(b"z".to_vec(), b"w".to_vec()),
                ROp::KvDelRange(b"y".to_vec(), b"zz".to_vec()),
                ROp::SetRevision(3),
            ],
        };
        let mut e = Enc::new();
        l.encode(&mut e);
        let back = RList::decode(&mut Dec::new(e.as_slice())).unwrap();
        assert_eq!(back, l);
        idx.apply(&l, &mut fx);
        assert_eq!(idx.objects[&key()].kv.len(), 1);
        let mut e = Enc::new();
        idx.encode(&mut e);
        let back = Index::decode(&mut Dec::new(e.as_slice())).unwrap();
        assert_eq!(back, idx);
    }
}
