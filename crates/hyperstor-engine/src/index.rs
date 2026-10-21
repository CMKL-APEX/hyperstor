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
use std::collections::BTreeMap;

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
#[derive(Debug, Default, Clone)]
pub struct Index {
    pub objects: BTreeMap<ObjKey, Object>,
    /// When block maps switch representation (not part of the content).
    pub policy: DensePolicy,
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

    /// Applies one resolved operation to the object `key`.
    pub fn apply_op(&mut self, key: &ObjKey, op: &ROp, fx: &mut Effects) {
        let policy = self.policy;
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
                    let freed = o.unmap(*lb, crcs.len() as u64);
                    fx.free(freed);
                    o.blocks.map(*lb, *pblock, crcs);
                    o.blocks.rebalance(&policy, crcs.len() as u64);
                }
                ROp::Deferred { lb, data, crcs } => {
                    // At run time every target block is mapped. Replaying
                    // over a fuzzy checkpoint (taken after a later trim) may
                    // find holes; those blocks are skipped, and a later
                    // operation in the log settles them.
                    for (i, crc) in crcs.iter().enumerate() {
                        let b = lb + i as u64;
                        if !o.blocks.all_mapped(b, 1) {
                            continue;
                        }
                        let pb = o.blocks.set_crcs(b, &[*crc])[0];
                        let blk = &data[i * crate::BLOCK..(i + 1) * crate::BLOCK];
                        fx.pending.insert(pb, blk.into());
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
            policy: DensePolicy::default(),
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
