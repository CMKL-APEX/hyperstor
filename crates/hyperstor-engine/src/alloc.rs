//! Free-space allocation within a shard's data region (spec/design.md §7.4).
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

    /// Marks `[start, start + len)` as used. It must be free.
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
}
