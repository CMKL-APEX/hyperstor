//! The in-memory index and the logged form of operations.
//!
//! A commit turns each [`crate::Op`] into resolved operations ([`ROp`]):
//! physical block numbers and checksums decided, nothing left to choose.
//! The log records resolved operations, and both a commit and recovery
//! apply them with [`Index::apply`], so replay reproduces exactly the
//! state the commit produced.

use crate::types::ObjKey;
use hyperstor_codec::{Dec, DecodeError, Enc};
use std::collections::BTreeMap;

/// A contiguous run of logical blocks mapped to contiguous physical blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub pblock: u64,
    /// One CRC32C per 4 KiB block of the run.
    pub crcs: Vec<u32>,
}

impl Run {
    pub fn len(&self) -> u64 {
        self.crcs.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.crcs.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Object {
    pub size: u64,
    pub revision: u64,
    /// Runs keyed by their first logical block. Runs never overlap.
    pub blocks: BTreeMap<u64, Run>,
    pub attrs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub kv: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// Where a logical block's data is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loc {
    Hole,
    Mapped { pblock: u64, crc: u32 },
}

impl Object {
    pub fn block_count(&self) -> u64 {
        self.blocks.values().map(Run::len).sum()
    }

    pub fn lookup(&self, lb: u64) -> Loc {
        if let Some((&start, run)) = self.blocks.range(..=lb).next_back()
            && lb < start + run.len()
        {
            let i = (lb - start) as usize;
            return Loc::Mapped {
                pblock: run.pblock + i as u64,
                crc: run.crcs[i],
            };
        }
        Loc::Hole
    }

    /// True if every block of `[lb, lb + n)` is mapped.
    pub fn all_mapped(&self, lb: u64, n: u64) -> bool {
        let mut cur = lb;
        let end = lb + n;
        while cur < end {
            match self.blocks.range(..=cur).next_back() {
                Some((&s, r)) if cur < s + r.len() => cur = s + r.len(),
                _ => return false,
            }
        }
        true
    }

    /// Splits the run containing `lb`, if any, so that a run starts at `lb`.
    fn split_at(&mut self, lb: u64) {
        let Some((&s, r)) = self.blocks.range(..lb).next_back() else {
            return;
        };
        if lb >= s + r.len() {
            return;
        }
        let i = (lb - s) as usize;
        let mut r = self.blocks.remove(&s).unwrap();
        let tail = Run {
            pblock: r.pblock + i as u64,
            crcs: r.crcs.split_off(i),
        };
        self.blocks.insert(s, r);
        self.blocks.insert(lb, tail);
    }

    /// Removes the mappings of `[lb, lb + n)` and returns the physical
    /// extents they held.
    pub fn unmap(&mut self, lb: u64, n: u64) -> Vec<(u64, u64)> {
        if n == 0 {
            return Vec::new();
        }
        let end = lb.saturating_add(n);
        self.split_at(lb);
        if end != u64::MAX {
            self.split_at(end);
        }
        let keys: Vec<u64> = self.blocks.range(lb..end).map(|(&k, _)| k).collect();
        keys.into_iter()
            .map(|k| {
                let r = self.blocks.remove(&k).unwrap();
                (r.pblock, r.len())
            })
            .collect()
    }

    /// Updates the checksums of mapped blocks `[lb, lb + crcs.len())` and
    /// returns their physical block numbers, in order.
    fn set_crcs(&mut self, lb: u64, crcs: &[u32]) -> Vec<u64> {
        let mut out = Vec::with_capacity(crcs.len());
        for (i, &c) in crcs.iter().enumerate() {
            let b = lb + i as u64;
            let (&s, run) = self
                .blocks
                .range_mut(..=b)
                .next_back()
                .expect("deferred write to a mapped block");
            let j = (b - s) as usize;
            run.crcs[j] = c;
            out.push(run.pblock + j as u64);
        }
        out
    }
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
    /// applied (spec/engine.md §7.3).
    Deferred {
        lb: u64,
        data: Vec<u8>,
        crcs: Vec<u32>,
    },
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
}

impl Effects {
    fn free(&mut self, extents: Vec<(u64, u64)>) {
        for (pb, n) in extents {
            let cancel: Vec<u64> = self.pending.range(pb..pb + n).map(|(&k, _)| k).collect();
            for k in cancel {
                self.pending.remove(&k);
            }
            self.freed.push((pb, n));
        }
    }
}

/// The shard's index of objects.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Index {
    pub objects: BTreeMap<ObjKey, Object>,
}

impl Index {
    /// Applies one resolved list.
    pub fn apply(&mut self, list: &RList, fx: &mut Effects) {
        for op in &list.ops {
            self.apply_op(&list.key, op, fx);
        }
    }

    /// Applies one resolved operation to the object `key`.
    pub fn apply_op(&mut self, key: &ObjKey, op: &ROp, fx: &mut Effects) {
        {
            if let ROp::Delete = op {
                if let Some(o) = self.objects.remove(key) {
                    let ext: Vec<(u64, u64)> =
                        o.blocks.values().map(|r| (r.pblock, r.len())).collect();
                    fx.free(ext);
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
                    let freed = o.unmap(*lb, crcs.len() as u64);
                    fx.free(freed);
                    o.blocks.insert(
                        *lb,
                        Run {
                            pblock: *pblock,
                            crcs: crcs.clone(),
                        },
                    );
                }
                ROp::Deferred { lb, data, crcs } => {
                    let pbs = o.set_crcs(*lb, crcs);
                    for (i, pb) in pbs.into_iter().enumerate() {
                        let blk = &data[i * crate::BLOCK..(i + 1) * crate::BLOCK];
                        fx.pending.insert(pb, blk.into());
                    }
                }
                ROp::Unmap { lb, n } => {
                    let freed = o.unmap(*lb, *n);
                    fx.free(freed);
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
            }
        }
    }

    /// Every physical extent in use.
    pub fn used_extents(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.objects
            .values()
            .flat_map(|o| o.blocks.values().map(|r| (r.pblock, r.len())))
    }

    pub fn encode(&self, e: &mut Enc) {
        e.u64(self.objects.len() as u64);
        for (k, o) in &self.objects {
            k.encode(e);
            e.u64(o.size).u64(o.revision);
            e.u32(o.blocks.len() as u32);
            for (lb, r) in &o.blocks {
                e.u64(*lb).u64(r.pblock).u32(r.crcs.len() as u32);
                for c in &r.crcs {
                    e.u32(*c);
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
    }

    pub fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let n = d.u64()?;
        let mut objects = BTreeMap::new();
        for _ in 0..n {
            let k = ObjKey::decode(d)?;
            let mut o = Object {
                size: d.u64()?,
                revision: d.u64()?,
                ..Default::default()
            };
            let runs = d.u32()?;
            for _ in 0..runs {
                let lb = d.u64()?;
                let pblock = d.u64()?;
                let crcs = d.seq(4, |d| d.u32())?;
                o.blocks.insert(lb, Run { pblock, crcs });
            }
            let na = d.u32()?;
            for _ in 0..na {
                o.attrs.insert(d.bytes()?.to_vec(), d.bytes()?.to_vec());
            }
            let nk = d.u64()?;
            for _ in 0..nk {
                o.kv.insert(d.bytes()?.to_vec(), d.bytes()?.to_vec());
            }
            objects.insert(k, o);
        }
        Ok(Self { objects })
    }
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
            ROp::Deferred { lb, data, crcs } => {
                e.u8(4).u64(*lb).bytes(data).seq(crcs, |e, c| {
                    e.u32(*c);
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
            4 => {
                let lb = d.u64()?;
                let data = d.bytes()?.to_vec();
                let crcs = d.seq(4, |d| d.u32())?;
                if data.len() != crcs.len() * crate::BLOCK {
                    return Err(DecodeError::Invalid {
                        field: "deferred length",
                        value: data.len() as u64,
                    });
                }
                ROp::Deferred { lb, data, crcs }
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
                ops: vec![ROp::Deferred {
                    lb: 1,
                    data: vec![7; crate::BLOCK],
                    crcs: vec![42],
                }],
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
