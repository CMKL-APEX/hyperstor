//! Free-space allocation within a shard's data region (spec/08-engine.md §7.4).
//!
//! Free space is kept as extents indexed both by start (to merge neighbours
//! on free) and by length (for best fit). Nothing here is persisted: on
//! open, the allocator is rebuilt from the index, which records every block
//! in use.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default, Clone)]
pub struct Allocator {
    by_start: BTreeMap<u64, u64>,
    by_len: BTreeSet<(u64, u64)>,
    free: u64,
}

impl Allocator {
    /// An allocator with `[start, start + len)` free.
    pub fn new(start: u64, len: u64) -> Self {
        let mut a = Self::default();
        if len > 0 {
            a.insert(start, len);
        }
        a
    }

    pub fn free_blocks(&self) -> u64 {
        self.free
    }

    pub fn extents(&self) -> usize {
        self.by_start.len()
    }

    fn insert(&mut self, start: u64, len: u64) {
        self.by_start.insert(start, len);
        self.by_len.insert((len, start));
        self.free += len;
    }

    fn remove(&mut self, start: u64, len: u64) {
        self.by_start.remove(&start);
        self.by_len.remove(&(len, start));
        self.free -= len;
    }

    /// Allocates `n` blocks as few extents as possible: the smallest free
    /// extent that fits all of them, otherwise the largest extents first.
    /// Returns `None`, allocating nothing, if fewer than `n` are free.
    pub fn alloc(&mut self, n: u64) -> Option<Vec<(u64, u64)>> {
        if n == 0 {
            return Some(Vec::new());
        }
        if n > self.free {
            return None;
        }
        if let Some(&(len, start)) = self.by_len.range((n, 0)..).next() {
            self.remove(start, len);
            if len > n {
                self.insert(start + n, len - n);
            }
            return Some(vec![(start, n)]);
        }
        let mut out = Vec::new();
        let mut need = n;
        while need > 0 {
            let &(len, start) = self.by_len.iter().next_back().expect("free > 0");
            let take = len.min(need);
            self.remove(start, len);
            if len > take {
                self.insert(start + take, len - take);
            }
            out.push((start, take));
            need -= take;
        }
        Some(out)
    }

    /// Returns `[start, start + len)` to free space, merging neighbours.
    pub fn free(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut s = start;
        let mut l = len;
        if let Some((&ps, &pl)) = self.by_start.range(..start).next_back() {
            debug_assert!(ps + pl <= start, "double free at {start}");
            if ps + pl == start {
                self.remove(ps, pl);
                s = ps;
                l += pl;
            }
        }
        if let Some((&ns, &nl)) = self.by_start.range(start..).next() {
            debug_assert!(start + len <= ns, "double free at {start}");
            if s + l == ns {
                self.remove(ns, nl);
                l += nl;
            }
        }
        self.insert(s, l);
    }

    /// An allocator for `[start, start + len)` with `used` taken, built in
    /// one pass over the extents sorted (D417): what opening a shard
    /// rebuilds from its index. Reserving each extent instead splits a free
    /// extent in two maps per extent, which took 21 s for 14.5 million
    /// runs (02-benchmarks.md, 2026-11-28). Extents of no blocks are
    /// skipped. Fails with the first extent outside the range or
    /// overlapping another.
    pub fn from_used(start: u64, len: u64, mut used: Vec<(u64, u64)>) -> Result<Self, (u64, u64)> {
        used.retain(|e| e.1 > 0);
        used.sort_unstable();
        let end = start + len;
        let mut free = Vec::with_capacity(used.len() + 1);
        let mut at = start;
        for (s, n) in used {
            if s < at || s.checked_add(n).is_none_or(|e| e > end) {
                return Err((s, n));
            }
            if s > at {
                free.push((at, s - at));
            }
            at = s + n;
        }
        if at < end {
            free.push((at, end - at));
        }
        Ok(Self {
            free: free.iter().map(|e| e.1).sum(),
            by_len: free.iter().map(|&(s, n)| (n, s)).collect(),
            by_start: free.into_iter().collect(),
        })
    }

    /// The parts of `[start, start + len)` that are free, in order.
    pub fn free_within(&self, start: u64, len: u64) -> Vec<(u64, u64)> {
        let end = start + len;
        let first = self
            .by_start
            .range(..=start)
            .next_back()
            .map_or(start, |(&s, _)| s);
        self.by_start
            .range(first..end)
            .filter_map(|(&s, &n)| {
                let (a, b) = (s.max(start), (s + n).min(end));
                (a < b).then_some((a, b - a))
            })
            .collect()
    }

    /// Marks `[start, start + len)` as used. False, changing nothing, if
    /// any of it isn't free.
    pub fn reserve(&mut self, start: u64, len: u64) -> bool {
        let Some((&fs, &fl)) = self.by_start.range(..=start).next_back() else {
            return false;
        };
        if fs + fl < start + len {
            return false;
        }
        self.remove(fs, fl);
        if fs < start {
            self.insert(fs, start - fs);
        }
        if start + len < fs + fl {
            self.insert(start + len, fs + fl - start - len);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_within_clips_to_the_range() {
        let mut a = Allocator::new(100, 100);
        assert!(a.reserve(110, 10));
        assert!(a.reserve(150, 20));
        assert_eq!(a.free_within(105, 60), vec![(105, 5), (120, 30)]);
        assert_eq!(a.free_within(112, 5), vec![]);
        assert_eq!(a.free_within(165, 100), vec![(170, 30)]);
    }

    #[test]
    fn best_fit_split_and_merge() {
        let mut a = Allocator::new(100, 1000);
        let x = a.alloc(10).unwrap();
        assert_eq!(x, vec![(100, 10)]);
        let y = a.alloc(20).unwrap();
        assert_eq!(y, vec![(110, 20)]);
        assert_eq!(a.free_blocks(), 970);
        a.free(100, 10);
        // A 5-block request takes the 10-block hole, the best fit.
        assert_eq!(a.alloc(5).unwrap(), vec![(100, 5)]);
        a.free(100, 5);
        a.free(110, 20);
        assert_eq!(a.free_blocks(), 1000);
        assert_eq!(a.extents(), 1, "everything merged back");
    }

    #[test]
    fn fragmented_allocation_and_exhaustion() {
        let mut a = Allocator::new(0, 10);
        for i in (0..10).step_by(2) {
            assert!(a.reserve(i, 1));
        }
        assert_eq!(a.free_blocks(), 5);
        let v = a.alloc(3).unwrap();
        assert_eq!(v.iter().map(|x| x.1).sum::<u64>(), 3);
        assert_eq!(v.len(), 3);
        assert!(a.alloc(3).is_none(), "only 2 left");
        assert_eq!(a.free_blocks(), 2);
    }

    #[test]
    fn reserve_refuses_used_space() {
        let mut a = Allocator::new(0, 10);
        assert!(a.reserve(2, 3));
        assert!(!a.reserve(4, 2));
        assert!(a.reserve(5, 5));
        assert!(a.reserve(0, 2));
        assert_eq!(a.free_blocks(), 0);
    }

    #[test]
    fn from_used_matches_reserving_each_extent() {
        // A small LCG: random used extents, some adjacent, in any order.
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = |m: u64| {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (x >> 33) % m
        };
        for _ in 0..200 {
            let (start, len) = (next(1000), 1 + next(5000));
            let mut by_reserve = Allocator::new(start, len);
            let mut used = Vec::new();
            for _ in 0..next(300) {
                let (s, n) = (start + next(len), 1 + next(8));
                if by_reserve.reserve(s, n) {
                    used.push((s, n));
                }
            }
            let mut shuffled = used.clone();
            shuffled.reverse();
            let built = Allocator::from_used(start, len, shuffled).unwrap();
            assert_eq!(built.by_start, by_reserve.by_start);
            assert_eq!(built.by_len, by_reserve.by_len);
            assert_eq!(built.free, by_reserve.free);
        }
    }

    #[test]
    fn from_used_refuses_overlaps_and_extents_outside() {
        assert_eq!(
            Allocator::from_used(10, 10, vec![(12, 4), (15, 2)]).unwrap_err(),
            (15, 2)
        );
        assert_eq!(
            Allocator::from_used(10, 10, vec![(5, 2)]).unwrap_err(),
            (5, 2)
        );
        assert_eq!(
            Allocator::from_used(10, 10, vec![(18, 3)]).unwrap_err(),
            (18, 3)
        );
        assert_eq!(
            Allocator::from_used(10, 10, vec![(u64::MAX, 2)]).unwrap_err(),
            (u64::MAX, 2)
        );
        let a = Allocator::from_used(10, 10, vec![(10, 10), (12, 0)]).unwrap();
        assert_eq!(a.free_blocks(), 0);
        assert_eq!(a.extents(), 0);
    }
}
