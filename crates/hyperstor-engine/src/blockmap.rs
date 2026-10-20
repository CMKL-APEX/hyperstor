//! A chunk version's map from logical to physical blocks (spec/engine.md
//! §7.2, decisions.md D43).
//!
//! Two representations, chosen by memory cost:
//! - **runs**: contiguous logical ranges mapped to contiguous physical
//!   ranges, each with one checksum per block. Cheap while an object has
//!   few runs (written sequentially, or rewritten whole).
//! - **dense**: one 8-byte slot per logical block, the physical block (32
//!   bits) and its checksum. Cheap once random overwrites have fragmented
//!   the object, which redirect-on-write (D38) makes common: a fragmented
//!   run costs about 140 bytes per block, a dense slot 8.
//!
//! The map switches to dense when the runs would cost more than the dense
//! array, and back to runs when a rewrite makes the runs cost less than 60%
//! of it (a contiguous chunk costs about 51%, since runs keep a checksum
//! per block too). The gap is hysteresis, so an object near the boundary
//! doesn't flip on every write. Equality compares content, not representation.

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

/// Where a logical block's data is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loc {
    Hole,
    Mapped { pblock: u64, crc: u32 },
}

/// Physical block number marking a hole in a dense map.
const HOLE: u32 = u32::MAX;
/// Approximate memory per run in the runs representation: the map entry,
/// the run, and the checksum vector's allocation (benchmarks.md, index).
pub const RUN_OVERHEAD: u64 = 96;
/// Largest object a dense map is used for, in blocks (4 GiB).
const MAX_DENSE_SPAN: u64 = 1 << 20;

/// When to switch representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DensePolicy {
    /// Never use dense maps.
    pub disabled: bool,
    /// Estimated memory per run; lowering it delays switching, raising it
    /// (as tests do) switches earlier.
    pub run_overhead: u64,
}

impl Default for DensePolicy {
    fn default() -> Self {
        Self {
            disabled: false,
            run_overhead: RUN_OVERHEAD,
        }
    }
}

#[derive(Debug, Clone)]
pub enum BlockMap {
    /// Runs keyed by their first logical block. Runs never overlap.
    Runs(BTreeMap<u64, Run>),
    /// Slot `i` maps logical block `i`: physical block in the high 32 bits
    /// (`HOLE` for none), CRC32C in the low 32.
    Dense(Vec<u64>),
}

impl Default for BlockMap {
    fn default() -> Self {
        BlockMap::Runs(BTreeMap::new())
    }
}

#[inline]
fn pack(pblock: u64, crc: u32) -> u64 {
    (pblock << 32) | crc as u64
}

#[inline]
fn unpack(slot: u64) -> Option<(u64, u32)> {
    let pb = (slot >> 32) as u32;
    (pb != HOLE).then_some((pb as u64, slot as u32))
}

const EMPTY: u64 = (HOLE as u64) << 32;

impl BlockMap {
    pub fn is_dense(&self) -> bool {
        matches!(self, BlockMap::Dense(_))
    }

    pub fn lookup(&self, lb: u64) -> Loc {
        match self {
            BlockMap::Runs(m) => {
                if let Some((&start, run)) = m.range(..=lb).next_back()
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
            BlockMap::Dense(v) => match v.get(lb as usize).copied().and_then(unpack) {
                Some((pblock, crc)) => Loc::Mapped { pblock, crc },
                None => Loc::Hole,
            },
        }
    }

    /// True if every block of `[lb, lb + n)` is mapped.
    pub fn all_mapped(&self, lb: u64, n: u64) -> bool {
        match self {
            BlockMap::Runs(m) => {
                let mut cur = lb;
                let end = lb + n;
                while cur < end {
                    match m.range(..=cur).next_back() {
                        Some((&s, r)) if cur < s + r.len() => cur = s + r.len(),
                        _ => return false,
                    }
                }
                true
            }
            BlockMap::Dense(v) => {
                (lb..lb + n).all(|b| v.get(b as usize).copied().and_then(unpack).is_some())
            }
        }
    }

    /// Blocks mapped.
    pub fn block_count(&self) -> u64 {
        match self {
            BlockMap::Runs(m) => m.values().map(Run::len).sum(),
            BlockMap::Dense(v) => v.iter().filter(|s| unpack(**s).is_some()).count() as u64,
        }
    }

    /// Every mapping as runs (logical start, physical start, CRCs), in
    /// logical order, maximal (adjacent logical and physical blocks merged).
    pub fn runs(&self) -> Vec<(u64, u64, Vec<u32>)> {
        let mut out: Vec<(u64, u64, Vec<u32>)> = Vec::new();
        let mut push = |lb: u64, pb: u64, crc: u32| match out.last_mut() {
            Some((l, p, c)) if *l + c.len() as u64 == lb && *p + c.len() as u64 == pb => {
                c.push(crc)
            }
            _ => out.push((lb, pb, vec![crc])),
        };
        match self {
            BlockMap::Runs(m) => {
                for (lb, r) in m {
                    for (i, c) in r.crcs.iter().enumerate() {
                        push(lb + i as u64, r.pblock + i as u64, *c);
                    }
                }
            }
            BlockMap::Dense(v) => {
                for (i, s) in v.iter().enumerate() {
                    if let Some((pb, c)) = unpack(*s) {
                        push(i as u64, pb, c);
                    }
                }
            }
        }
        out
    }

    /// Number of runs (maximal contiguous mappings).
    pub fn run_count(&self) -> u64 {
        match self {
            BlockMap::Runs(m) => m.len() as u64,
            BlockMap::Dense(_) => self.runs().len() as u64,
        }
    }

    /// Physical extents in use, merged where contiguous.
    pub fn extents(&self) -> Vec<(u64, u64)> {
        match self {
            BlockMap::Runs(m) => m.values().map(|r| (r.pblock, r.len())).collect(),
            BlockMap::Dense(_) => self
                .runs()
                .into_iter()
                .map(|(_, p, c)| (p, c.len() as u64))
                .collect(),
        }
    }

    /// Estimated memory, in bytes.
    pub fn mem_bytes(&self) -> u64 {
        match self {
            BlockMap::Runs(m) => {
                m.len() as u64 * RUN_OVERHEAD + 4 * m.values().map(Run::len).sum::<u64>()
            }
            BlockMap::Dense(v) => 8 * v.capacity() as u64,
        }
    }

    /// Removes the mappings of `[lb, lb + n)` and returns the physical
    /// extents they held.
    pub fn unmap(&mut self, lb: u64, n: u64) -> Vec<(u64, u64)> {
        if n == 0 {
            return Vec::new();
        }
        match self {
            BlockMap::Runs(m) => {
                let end = lb.saturating_add(n);
                split_at(m, lb);
                if end != u64::MAX {
                    split_at(m, end);
                }
                let keys: Vec<u64> = m.range(lb..end).map(|(&k, _)| k).collect();
                keys.into_iter()
                    .map(|k| {
                        let r = m.remove(&k).unwrap();
                        (r.pblock, r.len())
                    })
                    .collect()
            }
            BlockMap::Dense(v) => {
                let mut freed: Vec<(u64, u64)> = Vec::new();
                let end = (lb.saturating_add(n)).min(v.len() as u64);
                for b in lb.min(end)..end {
                    let slot = &mut v[b as usize];
                    if let Some((pb, _)) = unpack(*slot) {
                        match freed.last_mut() {
                            Some((s, c)) if *s + *c == pb => *c += 1,
                            _ => freed.push((pb, 1)),
                        }
                        *slot = EMPTY;
                    }
                }
                // Trailing holes cost memory for nothing.
                while v.last().is_some_and(|s| unpack(*s).is_none()) {
                    v.pop();
                }
                freed
            }
        }
    }

    /// Maps `[lb, lb + crcs.len())` to physical blocks from `pblock`. The
    /// range must be unmapped.
    pub fn map(&mut self, lb: u64, pblock: u64, crcs: &[u32]) {
        if crcs.is_empty() {
            return;
        }
        let end = lb + crcs.len() as u64;
        if let BlockMap::Dense(_) = self
            && (pblock + crcs.len() as u64 >= HOLE as u64 || end > MAX_DENSE_SPAN)
        {
            self.make_runs();
        }
        match self {
            BlockMap::Runs(m) => {
                m.insert(
                    lb,
                    Run {
                        pblock,
                        crcs: crcs.to_vec(),
                    },
                );
            }
            BlockMap::Dense(v) => {
                if v.len() < end as usize {
                    v.resize(end as usize, EMPTY);
                }
                for (i, c) in crcs.iter().enumerate() {
                    v[lb as usize + i] = pack(pblock + i as u64, *c);
                }
            }
        }
    }

    /// Updates the checksums of mapped blocks `[lb, lb + crcs.len())` and
    /// returns their physical block numbers, in order.
    pub fn set_crcs(&mut self, lb: u64, crcs: &[u32]) -> Vec<u64> {
        let mut out = Vec::with_capacity(crcs.len());
        match self {
            BlockMap::Runs(m) => {
                for (i, &c) in crcs.iter().enumerate() {
                    let b = lb + i as u64;
                    let (&s, run) = m.range_mut(..=b).next_back().expect("mapped block");
                    let j = (b - s) as usize;
                    run.crcs[j] = c;
                    out.push(run.pblock + j as u64);
                }
            }
            BlockMap::Dense(v) => {
                for (i, &c) in crcs.iter().enumerate() {
                    let slot = &mut v[lb as usize + i];
                    let (pb, _) = unpack(*slot).expect("mapped block");
                    *slot = pack(pb, c);
                    out.push(pb);
                }
            }
        }
        out
    }

    /// Switches representation if the other one now costs clearly less.
    pub fn rebalance(&mut self, policy: &DensePolicy) {
        match self {
            BlockMap::Runs(m) => {
                if policy.disabled || m.len() < 2 {
                    return;
                }
                let (&last_lb, last) = m.iter().next_back().unwrap();
                let span = last_lb + last.len();
                let blocks: u64 = m.values().map(Run::len).sum();
                let runs_cost = m.len() as u64 * policy.run_overhead + 4 * blocks;
                let fits =
                    span <= MAX_DENSE_SPAN && m.values().all(|r| r.pblock + r.len() < HOLE as u64);
                if fits && runs_cost > 8 * span {
                    self.make_dense();
                }
            }
            BlockMap::Dense(v) => {
                if policy.disabled {
                    self.make_runs();
                    return;
                }
                let span = v.len() as u64;
                let runs = self.runs();
                let blocks: u64 = runs.iter().map(|r| r.2.len() as u64).sum();
                let runs_cost = runs.len() as u64 * policy.run_overhead + 4 * blocks;
                // Back to runs below 60% of the dense cost (a contiguous
                // chunk is about 51%: runs keep 4 bytes of checksum per
                // block too); dense again above 100%.
                if span == 0 || 5 * runs_cost < 3 * 8 * span {
                    self.make_runs();
                }
            }
        }
    }

    fn make_dense(&mut self) {
        let BlockMap::Runs(m) = self else { return };
        let span = m.iter().next_back().map_or(0, |(lb, r)| lb + r.len()) as usize;
        let mut v = vec![EMPTY; span];
        for (lb, r) in m.iter() {
            for (i, c) in r.crcs.iter().enumerate() {
                v[*lb as usize + i] = pack(r.pblock + i as u64, *c);
            }
        }
        *self = BlockMap::Dense(v);
    }

    fn make_runs(&mut self) {
        if let BlockMap::Dense(_) = self {
            let m = self
                .runs()
                .into_iter()
                .map(|(lb, pblock, crcs)| (lb, Run { pblock, crcs }))
                .collect();
            *self = BlockMap::Runs(m);
        }
    }
}

/// Splits the run containing `lb`, if any, so that a run starts at `lb`.
fn split_at(m: &mut BTreeMap<u64, Run>, lb: u64) {
    let Some((&s, r)) = m.range(..lb).next_back() else {
        return;
    };
    if lb >= s + r.len() {
        return;
    }
    let i = (lb - s) as usize;
    let mut r = m.remove(&s).unwrap();
    let tail = Run {
        pblock: r.pblock + i as u64,
        crcs: r.crcs.split_off(i),
    };
    m.insert(s, r);
    m.insert(lb, tail);
}

impl PartialEq for BlockMap {
    fn eq(&self, other: &Self) -> bool {
        self.runs() == other.runs()
    }
}

impl Eq for BlockMap {}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::SmallRng;
    use rand::{Rng, SeedableRng};

    /// The two representations, driven by the same random operations, must
    /// agree on every lookup, on what they free, and after switching.
    #[test]
    fn representations_agree_under_random_operations() {
        let forced = DensePolicy {
            disabled: false,
            run_overhead: 1 << 20, // switch to dense at once
        };
        let never = DensePolicy {
            disabled: true,
            run_overhead: RUN_OVERHEAD,
        };
        for seed in 0..200 {
            let mut rng = SmallRng::seed_from_u64(seed);
            let mut a = BlockMap::default(); // runs only
            let mut b = BlockMap::default(); // dense when it can
            let mut next_pb = 1000u64;
            for _ in 0..200 {
                let lb = rng.random_range(0..256u64);
                let n = rng.random_range(1..32u64);
                match rng.random_range(0..10) {
                    0..=5 => {
                        let crcs: Vec<u32> = (0..n).map(|_| rng.random()).collect();
                        let fa = a.unmap(lb, n);
                        let fb = b.unmap(lb, n);
                        assert_eq!(total(&fa), total(&fb), "seed {seed}");
                        a.map(lb, next_pb, &crcs);
                        b.map(lb, next_pb, &crcs);
                        next_pb += n + rng.random_range(0..3);
                    }
                    6..=7 => {
                        let fa = a.unmap(lb, n);
                        let fb = b.unmap(lb, n);
                        assert_eq!(sorted(fa), sorted(fb), "seed {seed}");
                    }
                    8 if a.all_mapped(lb, n) => {
                        let crcs: Vec<u32> = (0..n).map(|_| rng.random()).collect();
                        assert_eq!(a.set_crcs(lb, &crcs), b.set_crcs(lb, &crcs));
                    }
                    _ => {
                        assert_eq!(a.all_mapped(lb, n), b.all_mapped(lb, n));
                    }
                }
                a.rebalance(&never);
                let default = DensePolicy::default();
                b.rebalance(if rng.random_bool(0.9) {
                    &forced
                } else {
                    &default
                });
                assert!(!a.is_dense());
                assert_eq!(a, b, "seed {seed}");
                assert_eq!(a.block_count(), b.block_count());
                for q in 0..300 {
                    assert_eq!(a.lookup(q), b.lookup(q), "seed {seed} block {q}");
                }
                assert_eq!(sorted(a.extents()), sorted(b.extents()));
            }
        }
    }

    fn total(v: &[(u64, u64)]) -> u64 {
        v.iter().map(|x| x.1).sum()
    }

    /// Extents as a set of blocks, so differently split lists compare equal.
    fn sorted(v: Vec<(u64, u64)>) -> Vec<u64> {
        let mut out: Vec<u64> = v.into_iter().flat_map(|(s, n)| s..s + n).collect();
        out.sort();
        out
    }

    #[test]
    fn switches_by_cost_with_hysteresis() {
        let p = DensePolicy::default();
        let mut m = BlockMap::default();
        // A 1024-block chunk written whole: one run, stays runs.
        m.map(0, 10_000, &vec![7; 1024]);
        m.rebalance(&p);
        assert!(!m.is_dense());
        // Every other block remapped: hundreds of runs, switches to dense.
        for b in (0..1024).step_by(2) {
            m.unmap(b, 1);
            m.map(b, 50_000 + b, &[1]);
            m.rebalance(&p);
        }
        assert!(m.is_dense(), "{} runs", m.run_count());
        assert!(m.mem_bytes() <= 8 * 1024);
        // Rewritten whole: contiguous again, back to runs.
        let freed = m.unmap(0, 1024);
        assert_eq!(total(&freed), 1024);
        m.map(0, 90_000, &vec![3; 1024]);
        m.rebalance(&p);
        assert!(!m.is_dense());
        assert_eq!(m.run_count(), 1);
    }

    #[test]
    fn huge_block_numbers_stay_in_runs() {
        let p = DensePolicy {
            disabled: false,
            run_overhead: 1 << 20,
        };
        let mut m = BlockMap::default();
        m.map(0, u32::MAX as u64 + 5, &[1, 2]);
        m.map(4, 1, &[3]);
        m.rebalance(&p);
        assert!(!m.is_dense(), "32-bit slots can't hold the block number");
        let mut d = BlockMap::default();
        d.map(0, 1, &[1]);
        d.map(4, 9, &[1]);
        d.rebalance(&p);
        assert!(d.is_dense());
        d.map(8, u32::MAX as u64 + 1, &[2]);
        assert!(!d.is_dense(), "converted back to hold it");
        assert_eq!(
            d.lookup(8),
            Loc::Mapped {
                pblock: u32::MAX as u64 + 1,
                crc: 2
            }
        );
    }
}
