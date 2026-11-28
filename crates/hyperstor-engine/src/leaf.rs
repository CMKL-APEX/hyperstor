//! What the on-device index's leaves would take for the block maps held
//! now (D408; 08-engine.md §7.6): the formats `hyperstor-bench layout`
//! compares, sized from a shard's in-memory index, a slice of objects at a
//! time so a running node can walk it without holding its reactor.

use crate::blockmap::BlockMap;
use crate::{ObjKey, Shard};
use std::ops::Bound;

/// Per object in a leaf: its key, written once per leaf it appears in.
pub const OBJ_HEADER: u64 = 32;

/// One object's mappings as maximal runs contiguous both logically and
/// physically: (first logical block, physical block, blocks).
pub type Runs = Vec<(u64, u64, u64)>;

/// The leaf bytes of each format.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LeafBytes {
    /// D43 as first planned: an entry per block (block u32, physical u32,
    /// CRC u32).
    pub per_block: u64,
    /// The same with 64-bit physical addresses (D413).
    pub per_block64: u64,
    /// Range keys (block u32, count u32, physical u64); CRCs elsewhere.
    pub ranges: u64,
    /// Range keys, and a packed entry where smaller (block u32, count u32,
    /// base u64, then a u32 offset per block): D408; CRCs elsewhere.
    pub packed: u64,
}

impl LeafBytes {
    /// The bytes of one object's entries in each format, without its key.
    pub fn of(runs: &Runs) -> Self {
        let blocks: u64 = runs.iter().map(|r| r.2).sum();
        let mut packed = 0;
        // Stretches of logically adjacent runs: each is coded as ranges or
        // as one packed entry, whichever is smaller.
        let mut i = 0;
        while i < runs.len() {
            let mut j = i + 1;
            while j < runs.len() && runs[j - 1].0 + runs[j - 1].2 == runs[j].0 {
                j += 1;
            }
            let b: u64 = runs[i..j].iter().map(|r| r.2).sum();
            packed += (16 * (j - i) as u64).min(16 + 4 * b);
            i = j;
        }
        Self {
            per_block: 12 * blocks,
            per_block64: 16 * blocks,
            ranges: 16 * runs.len() as u64,
            packed,
        }
    }
}

/// Totals over the objects sized so far.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LeafTally {
    pub objects: u64,
    /// Blocks mapped, and the runs they make.
    pub blocks: u64,
    pub runs: u64,
    /// Leaf bytes of each format, each object's key included.
    pub bytes: LeafBytes,
}

impl LeafTally {
    pub fn add(&mut self, runs: &Runs) {
        let b = LeafBytes::of(runs);
        self.objects += 1;
        self.blocks += runs.iter().map(|r| r.2).sum::<u64>();
        self.runs += runs.len() as u64;
        self.bytes.per_block += b.per_block + OBJ_HEADER;
        self.bytes.per_block64 += b.per_block64 + OBJ_HEADER;
        self.bytes.ranges += b.ranges + OBJ_HEADER;
        self.bytes.packed += b.packed + OBJ_HEADER;
    }
}

/// The runs of a block map, merged where contiguous both ways. Unlike
/// [`BlockMap::runs`], copies no checksums.
pub fn runs_of(map: &BlockMap) -> Runs {
    let mut out: Runs = Vec::new();
    map.each_mapping(|lb, pb, n| match out.last_mut() {
        Some((l, p, c)) if *l + *c == lb && *p + *c == pb => *c += n,
        _ => out.push((lb, pb, n)),
    });
    out
}

impl Shard {
    /// Sizes up to `max` objects after `after` (from the first with
    /// `None`) into `tally`, and returns the last one sized, or `None` once
    /// none is left: a walk that resumes where it stopped, so a node can
    /// spread it out. Objects written between slices are sized as they are
    /// when reached.
    pub fn size_leaves(
        &self,
        after: Option<&ObjKey>,
        max: usize,
        tally: &mut LeafTally,
    ) -> Option<ObjKey> {
        let from = after.map_or(Bound::Unbounded, Bound::Excluded);
        let mut last = None;
        for (k, o) in self
            .index()
            .objects
            .range::<ObjKey, _>((from, Bound::Unbounded))
            .take(max)
        {
            tally.add(&runs_of(&o.blocks));
            last = Some(k);
        }
        last.cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fragmented_stretch_packs_and_a_run_stays_a_range() {
        // One run of 100 blocks: one range, or a packed entry of 416 bytes.
        let b = LeafBytes::of(&vec![(0, 1000, 100)]);
        assert_eq!((b.per_block, b.ranges, b.packed), (1200, 16, 16));
        // Ten adjacent single blocks scattered on the device: ten ranges
        // (160 bytes) or one packed entry (16 + 40).
        let runs: Runs = (0..10).map(|i| (i, 5000 + 7 * i, 1)).collect();
        let b = LeafBytes::of(&runs);
        assert_eq!((b.per_block64, b.ranges, b.packed), (160, 160, 56));
    }
}
