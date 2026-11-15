//! The walk that counts a PG's bytes (D155) and, in the same pass, what
//! each branch of each lineage holds in it, head and snapshot-only blocks
//! apart (20-tenants.md §24.4, D335, D338).
//!
//! The walk runs on the reactor, a slice of about [`SLICE`] objects at a
//! time, as D155's did. A slice ends only between chunks: a chunk's
//! versions, of every branch, sort next to each other in the index
//! (lineage and chunk index first in the name), so each chunk is laid over
//! whole in one slice.

use hyperstor_engine::index::{Index, Object};
use hyperstor_engine::{Namespace, ObjKey};
use hyperstor_proto::space::{BranchBlocks, Head, Heads, Overlay, Version, overlay};
use std::collections::HashMap;

/// Objects per slice, as D155's walk.
pub const SLICE: usize = 4096;

/// A PG's walk under way.
pub struct Walk {
    /// Every object's blocks, every namespace (D155's number).
    pub blocks: u64,
    branches: HashMap<(u128, u32), (u64, u64)>,
    versions: Vec<Version>,
    scratch: Overlay,
    out: HashMap<u32, (u64, u64)>,
    simple: Vec<Head>,
}

impl Default for Walk {
    fn default() -> Self {
        Walk {
            blocks: 0,
            branches: HashMap::new(),
            versions: Vec::new(),
            scratch: Overlay::default(),
            out: HashMap::new(),
            simple: vec![Head::simple()],
        }
    }
}

/// A volume chunk's name: its chunk (lineage and index) and branch.
fn chunk_of(name: &[u8]) -> Option<([u8; 24], u32)> {
    let chunk: [u8; 24] = name.get(..24)?.try_into().ok()?;
    let branch = match name.len() {
        24 => 0,
        28 => u32::from_be_bytes(name[24..28].try_into().ok()?),
        _ => return None,
    };
    Some((chunk, branch))
}

/// One chunk's versions as gathered: branch, epoch, object.
type Group<'a> = Vec<(u32, u64, &'a Object)>;

impl Walk {
    /// Walks the next slice of PG `pool.pg` after `after` (from its start
    /// if none), at least `max` objects unless the PG ends first. Returns
    /// where the next slice starts, or None when the PG is done.
    pub fn step(
        &mut self,
        index: &Index,
        pool: u32,
        pg: u32,
        after: Option<&ObjKey>,
        max: usize,
        heads: &Heads,
    ) -> Option<ObjKey> {
        use std::ops::Bound;
        let lo = match after {
            Some(k) => Bound::Excluded(k.clone()),
            None => Bound::Included(ObjKey::new(pool, pg, Namespace::Vol, Vec::new(), 0)),
        };
        let mut last: Option<&ObjKey> = None;
        let mut chunk: Option<[u8; 24]> = None;
        let mut group: Group<'_> = Vec::new();
        for (n, (k, o)) in index.objects.range((lo, Bound::Unbounded)).enumerate() {
            if (k.pool, k.pg) != (pool, pg) {
                break;
            }
            let vol = (k.ns == Namespace::Vol)
                .then(|| chunk_of(&k.name))
                .flatten();
            if n >= max && (vol.is_none() || vol.map(|v| v.0) != chunk) {
                self.flush(chunk, &mut group, heads);
                return last.cloned();
            }
            last = Some(k);
            match vol {
                Some((c, branch)) => {
                    if chunk != Some(c) {
                        self.flush(chunk, &mut group, heads);
                        chunk = Some(c);
                    }
                    group.push((branch, k.version, o));
                }
                None => self.blocks += o.block_count(),
            }
        }
        self.flush(chunk, &mut group, heads);
        None
    }

    /// Lays the gathered chunk's versions over each other. A lone version
    /// needs no laying: its blocks are counted, and a head reads them all
    /// or none.
    fn flush(&mut self, chunk: Option<[u8; 24]>, group: &mut Group<'_>, heads: &Heads) {
        let Some(chunk) = chunk else {
            return;
        };
        let lineage = u128::from_be_bytes(chunk[..16].try_into().expect("16 bytes"));
        let hs = heads.get(&lineage).unwrap_or(&self.simple);
        if let [(branch, epoch, o)] = group[..] {
            let n = o.block_count();
            self.blocks += n;
            let head = if hyperstor_proto::space::lone_read(hs, branch, epoch) {
                n
            } else {
                0
            };
            let e = self.branches.entry((lineage, branch)).or_default();
            e.0 += n;
            e.1 += head;
            group.clear();
            return;
        }
        if self.versions.len() < group.len() {
            self.versions.resize(group.len(), Version::default());
        }
        for (v, &(branch, epoch, o)) in self.versions.iter_mut().zip(group.iter()) {
            v.branch = branch;
            v.epoch = epoch;
            o.blocks.bits(&mut v.bits);
            self.blocks += hyperstor_proto::space::count(&v.bits);
        }
        overlay(
            &self.versions[..group.len()],
            hs,
            &mut self.out,
            &mut self.scratch,
        );
        for (branch, (blocks, head)) in self.out.drain() {
            let e = self.branches.entry((lineage, branch)).or_default();
            e.0 += blocks;
            e.1 += head;
        }
        group.clear();
    }

    /// The PG's branches, in order.
    pub fn branches(&self) -> Vec<BranchBlocks> {
        let mut v: Vec<BranchBlocks> = self
            .branches
            .iter()
            .map(|(&(lineage, branch), &(blocks, head))| BranchBlocks {
                lineage,
                branch,
                blocks,
                head,
            })
            .collect();
        v.sort_unstable_by_key(|b| (b.lineage, b.branch));
        v
    }
}

/// A whole PG at once (tests and the benchmark).
pub fn walk(index: &Index, pool: u32, pg: u32, slice: usize, heads: &Heads) -> Walk {
    let mut w = Walk::default();
    let mut after = None;
    while let Some(k) = w.step(index, pool, pg, after.as_ref(), slice, heads) {
        after = Some(k);
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperstor_engine::shard::Region;
    use hyperstor_engine::{BLOCK, BlockDev, DensePolicy, MemDev, Op, OpList, Shard};
    use rand::rngs::SmallRng;
    use rand::{RngExt, SeedableRng};
    use std::sync::Arc;

    const SPAN: u64 = 130;

    fn name(lineage: u128, chunk: u64, branch: u32) -> Vec<u8> {
        let mut n = lineage.to_be_bytes().to_vec();
        n.extend_from_slice(&chunk.to_be_bytes());
        if branch != 0 {
            n.extend_from_slice(&branch.to_be_bytes());
        }
        n
    }

    /// What each branch holds and its owners' heads read, block by block,
    /// as 08-engine.md §7.2 reads them.
    fn recount(sh: &Shard, pool: u32, pg: u32, heads: &Heads) -> Vec<BranchBlocks> {
        let mut out: HashMap<(u128, u32), (u64, u64)> = HashMap::new();
        let objs: Vec<_> = sh
            .index()
            .objects
            .iter()
            .filter(|(k, _)| (k.pool, k.pg, k.ns) == (pool, pg, Namespace::Vol))
            .collect();
        let mut chunks: HashMap<[u8; 24], Vec<(u32, u64, &hyperstor_engine::index::Object)>> =
            HashMap::new();
        for (k, o) in &objs {
            let (c, b) = chunk_of(&k.name).unwrap();
            chunks.entry(c).or_default().push((b, k.version, o));
        }
        for (c, vs) in chunks {
            let lineage = u128::from_be_bytes(c[..16].try_into().unwrap());
            let simple = vec![Head::simple()];
            let hs = heads.get(&lineage).unwrap_or(&simple);
            let mut seen = vec![std::collections::HashSet::new(); vs.len()];
            for h in hs {
                for blk in 0..SPAN {
                    'layers: for &(br, lo, hi) in &h.layers {
                        let best = vs
                            .iter()
                            .enumerate()
                            .filter(|(_, (b, e, o))| {
                                *b == br
                                    && (lo..=hi).contains(e)
                                    && o.lookup(blk) != hyperstor_engine::index::Loc::Hole
                            })
                            .max_by_key(|(_, (_, e, _))| *e);
                        if let Some((i, _)) = best {
                            if h.owned.contains(&br) {
                                seen[i].insert(blk);
                            }
                            break 'layers;
                        }
                    }
                }
            }
            for (i, (b, _, o)) in vs.iter().enumerate() {
                let e = out.entry((lineage, *b)).or_default();
                e.0 += o.block_count();
                e.1 += seen[i].len() as u64;
            }
        }
        let mut v: Vec<BranchBlocks> = out
            .into_iter()
            .map(|((lineage, branch), (blocks, head))| BranchBlocks {
                lineage,
                branch,
                blocks,
                head,
            })
            .collect();
        v.sort_unstable_by_key(|b| (b.lineage, b.branch));
        v
    }

    /// Chunk versions written, overwritten, zeroed (a discard), merged
    /// into newer ones (a snapshot's delete), deleted (a branch dropped),
    /// across branches (clones, rollbacks) and lineages, beside other
    /// namespaces and PGs; walked in slices of every size, against a
    /// recount block by block, and D155's sum.
    #[test]
    fn the_walk_matches_a_recount_block_by_block() {
        for seed in 0..40u64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            let region = Region {
                start: 0,
                blocks: 65536,
                log_blocks: 256,
            };
            let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(65536 * BLOCK as u64));
            Shard::format(dev.clone(), region, 7, 0).unwrap();
            let mut sh = Shard::open(dev, region, 7, 0).unwrap();
            if seed % 2 == 1 {
                // Dense maps early (D43).
                sh.set_dense_policy(DensePolicy {
                    disabled: false,
                    run_overhead: 4096,
                });
            }
            let key = |rng: &mut SmallRng| {
                let pg = if rng.random_bool(0.9) { 2 } else { 3 };
                let ns = if rng.random_bool(0.9) {
                    Namespace::Vol
                } else {
                    Namespace::Meta
                };
                let lineage = rng.random_range(1..4u128) << 64;
                let n = name(lineage, rng.random_range(0..3), rng.random_range(0..3));
                ObjKey::new(1, pg, ns, n, rng.random_range(0..6))
            };
            for _ in 0..rng.random_range(10..120) {
                let k = key(&mut rng);
                let start = rng.random_range(0..SPAN);
                let len = rng.random_range(1..=SPAN - start);
                let op = match rng.random_range(0..10) {
                    0..=5 => Op::Write {
                        off: start * BLOCK as u64,
                        data: vec![seed as u8; (len * BLOCK as u64) as usize].into(),
                        crcs: None,
                    },
                    6 | 7 => Op::Zero {
                        off: start * BLOCK as u64,
                        len: len * BLOCK as u64,
                    },
                    8 => {
                        // Merged into a newer version of the same chunk.
                        let mut newer = k.clone();
                        newer.version += 1 + rng.random_range(0..3);
                        let src = k.clone();
                        if sh.stat(&src).is_none() {
                            continue;
                        }
                        sh.commit(vec![OpList {
                            key: newer,
                            ops: vec![Op::Absorb(src)],
                        }])
                        .unwrap()
                        .pop()
                        .unwrap()
                        .unwrap();
                        continue;
                    }
                    _ => Op::Delete,
                };
                sh.commit(vec![OpList {
                    key: k,
                    ops: vec![op],
                }])
                .unwrap()
                .pop()
                .unwrap()
                .unwrap();
            }
            // Heads: some lineages simple, others with clones and rollbacks.
            let mut heads = Heads::new();
            for l in 1..4u128 {
                if rng.random_bool(0.5) {
                    continue;
                }
                let hs = (0..rng.random_range(1..4))
                    .map(|_| {
                        let mut layers = Vec::new();
                        for _ in 0..rng.random_range(1..4) {
                            let lo = rng.random_range(0..4);
                            layers.push((rng.random_range(0..3), lo, lo + rng.random_range(0..4)));
                        }
                        Head {
                            layers,
                            owned: (0..3).filter(|_| rng.random_bool(0.4)).collect(),
                        }
                    })
                    .collect();
                heads.insert(l << 64, hs);
            }
            let want = recount(&sh, 1, 2, &heads);
            let total = {
                let (mut t, mut after) = (0, None);
                loop {
                    let (b, next) = sh.pg_blocks(1, 2, after.as_ref(), 7);
                    t += b;
                    match next {
                        Some(k) => after = Some(k),
                        None => break t,
                    }
                }
            };
            for slice in [1, 2, 3, 5, 1000] {
                let w = walk(sh.index(), 1, 2, slice, &heads);
                assert_eq!(w.branches(), want, "seed {seed} slice {slice}");
                assert_eq!(w.blocks, total, "seed {seed} slice {slice}");
            }
        }
    }
}
