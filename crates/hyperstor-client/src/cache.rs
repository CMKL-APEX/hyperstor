//! The host read cache (decisions.md D32, D102): blocks of chunk versions
//! that can't change, shared by every volume a client serves, so the VMs
//! of a host that boot clones of one golden image read it once.
//!
//! A block is keyed by the version it came from: lineage, the lineage's
//! random value (a volume recreated with the same id gets a new one),
//! branch, epoch, chunk and block. Only a volume's current head version
//! changes, so blocks from any other version are cached, and no entry is
//! ever invalidated; the attached volume remembers which version each of
//! its blocks came from, and forgets it when it writes the block or its
//! header changes (`volume::Attached`). In memory, least recently used
//! first out.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

/// Default size of a client's cache.
pub const DEFAULT_BYTES: usize = 256 << 20;
const BLOCK: usize = 4096;

/// Where a cached block comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub lineage: u128,
    pub incarnation: u64,
    pub branch: u32,
    pub epoch: u64,
    pub chunk: u64,
    pub block: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub blocks: u64,
}

pub struct ReadCache {
    capacity: usize,
    blocks: HashMap<Key, (Rc<[u8]>, u64)>,
    /// Use order: tick -> key.
    order: BTreeMap<u64, Key>,
    /// Blocks held per (lineage, incarnation).
    lineages: HashMap<(u128, u64), u64>,
    tick: u64,
    stats: Stats,
}

impl ReadCache {
    pub fn new(bytes: usize) -> Self {
        Self {
            capacity: bytes / BLOCK,
            blocks: HashMap::new(),
            order: BTreeMap::new(),
            lineages: HashMap::new(),
            tick: 0,
            stats: Stats::default(),
        }
    }

    /// Changes the size; shrinking drops the least recently used.
    pub fn set_bytes(&mut self, bytes: usize) {
        self.capacity = bytes / BLOCK;
        self.evict();
    }

    pub fn get(&mut self, k: &Key) -> Option<Rc<[u8]>> {
        let next = self.tick + 1;
        match self.blocks.get_mut(k) {
            Some((b, t)) => {
                self.order.remove(t);
                *t = next;
                self.order.insert(next, *k);
                self.tick = next;
                self.stats.hits += 1;
                Some(b.clone())
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    pub fn insert(&mut self, k: Key, block: Rc<[u8]>) {
        if self.capacity == 0 || block.len() != BLOCK {
            return;
        }
        self.tick += 1;
        match self.blocks.insert(k, (block, self.tick)) {
            Some((_, t)) => {
                self.order.remove(&t);
            }
            None => *self.lineages.entry((k.lineage, k.incarnation)).or_default() += 1,
        }
        self.order.insert(self.tick, k);
        self.evict();
    }

    fn evict(&mut self) {
        while self.blocks.len() > self.capacity {
            let Some((_, k)) = self.order.pop_first() else {
                break;
            };
            self.blocks.remove(&k);
            let l = (k.lineage, k.incarnation);
            if let Some(n) = self.lineages.get_mut(&l) {
                *n -= 1;
                if *n == 0 {
                    self.lineages.remove(&l);
                }
            }
        }
    }

    /// Whether it holds any block of the lineage: then a read that doesn't
    /// know where its blocks come from asks first, without the data.
    pub fn has_lineage(&self, lineage: u128, incarnation: u64) -> bool {
        self.lineages.contains_key(&(lineage, incarnation))
    }

    pub fn stats(&self) -> Stats {
        Stats {
            blocks: self.blocks.len() as u64,
            ..self.stats
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(block: u64) -> Key {
        Key {
            lineage: 1,
            incarnation: 2,
            branch: 0,
            epoch: 1,
            chunk: 0,
            block,
        }
    }

    #[test]
    fn least_recently_used_goes_first() {
        let mut c = ReadCache::new(2 * BLOCK);
        let b = |x: u8| -> Rc<[u8]> { vec![x; BLOCK].into() };
        c.insert(key(1), b(1));
        c.insert(key(2), b(2));
        assert_eq!(c.get(&key(1)).unwrap()[0], 1);
        c.insert(key(3), b(3));
        assert!(c.get(&key(2)).is_none(), "2 was least recently used");
        assert_eq!(c.get(&key(1)).unwrap()[0], 1);
        assert_eq!(c.get(&key(3)).unwrap()[0], 3);
        assert_eq!(
            c.stats(),
            Stats {
                hits: 3,
                misses: 1,
                blocks: 2
            }
        );
        // Another incarnation of the lineage: a recreated volume.
        assert!(
            c.get(&Key {
                incarnation: 9,
                ..key(1)
            })
            .is_none()
        );
        assert!(c.has_lineage(1, 2) && !c.has_lineage(1, 9));
        c.set_bytes(BLOCK);
        assert_eq!(c.stats().blocks, 1);
        c.set_bytes(0);
        assert!(!c.has_lineage(1, 2), "the last block of the lineage went");
        c.set_bytes(0);
        c.insert(key(4), b(4));
        assert_eq!(c.stats().blocks, 0, "a cache of size 0 keeps nothing");
    }
}
