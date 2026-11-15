//! The space volumes use (20-tenants.md §24.4, D335–D339): what each
//! primary finds in its PGs' chunk versions every 30 s, sent as
//! [`crate::Msg::PgSpace`], and the heads the map service sends back for
//! the walk to lay the versions over.
//!
//! A chunk's versions are laid over each other as a volume's head reads
//! them (08-engine.md §7.2): for each block, the newest version holding it
//! in the first layer that has one. A block a head reads from a branch its
//! own volume owns is a *head* block; every other block a version holds
//! is held only by snapshots (blocks newer versions override, or that
//! only a clone reads through). A clone's shared blocks stay its parent's
//! (D336).

use hyperstor_codec::{Dec, DecodeError, Enc};
use std::collections::HashMap;

/// What one branch of a lineage holds in one PG: its versions' blocks, and
/// those of them its volume's head reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BranchBlocks {
    pub lineage: u128,
    pub branch: u32,
    pub blocks: u64,
    pub head: u64,
}

/// A volume's head as the walk lays versions for it: the layers it reads
/// (`(branch, oldest, newest epoch)`, first first) and the branches its
/// volume owns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Head {
    pub layers: Vec<(u32, u64, u64)>,
    pub owned: Vec<u32>,
}

impl Head {
    /// The head of a lineage nothing was cloned from or rolled back: its
    /// first volume's, on branch 0, reading every epoch. What the walk
    /// takes for a lineage the map service didn't name.
    pub fn simple() -> Head {
        Head {
            layers: vec![(0, 0, u64::MAX)],
            owned: vec![0],
        }
    }
}

/// The heads of every lineage that isn't simple, by lineage.
pub type Heads = HashMap<u128, Vec<Head>>;

/// A PG's branches, as a primary reports them.
pub type PgBranches = (u32, u32, Vec<BranchBlocks>);

pub fn encode_report(pgs: &[PgBranches]) -> Vec<u8> {
    let mut e = Enc::new();
    e.seq(pgs, |e, (pool, pg, b)| {
        e.u32(*pool).u32(*pg).seq(b, |e, b| {
            e.u128(b.lineage).u32(b.branch).u64(b.blocks).u64(b.head);
        });
    });
    e.into_vec()
}

pub fn decode_report(buf: &[u8]) -> Result<Vec<PgBranches>, DecodeError> {
    let mut d = Dec::new(buf);
    let r = d.seq(12, |d| {
        Ok((
            d.u32()?,
            d.u32()?,
            d.seq(36, |d| {
                Ok(BranchBlocks {
                    lineage: d.u128()?,
                    branch: d.u32()?,
                    blocks: d.u64()?,
                    head: d.u64()?,
                })
            })?,
        ))
    })?;
    d.finish()?;
    Ok(r)
}

pub fn encode_heads(heads: &[(u128, Vec<Head>)]) -> Vec<u8> {
    let mut e = Enc::new();
    e.seq(heads, |e, (l, hs)| {
        e.u128(*l).seq(hs, |e, h| {
            e.seq(&h.layers, |e, (b, lo, hi)| {
                e.u32(*b).u64(*lo).u64(*hi);
            });
            e.seq(&h.owned, |e, b| {
                e.u32(*b);
            });
        });
    });
    e.into_vec()
}

pub fn decode_heads(buf: &[u8]) -> Result<Heads, DecodeError> {
    let mut d = Dec::new(buf);
    let r = d.seq(20, |d| {
        Ok((
            d.u128()?,
            d.seq(8, |d| {
                Ok(Head {
                    layers: d.seq(20, |d| Ok((d.u32()?, d.u64()?, d.u64()?)))?,
                    owned: d.seq(4, |d| d.u32())?,
                })
            })?,
        ))
    })?;
    d.finish()?;
    Ok(r.into_iter().collect())
}

/// One version of a chunk: its branch, epoch and logical blocks as a bit
/// set (bit `b % 64` of word `b / 64` for block `b`).
#[derive(Debug, Clone, Default)]
pub struct Version {
    pub branch: u32,
    pub epoch: u64,
    pub bits: Vec<u64>,
}

/// Blocks in a bit set.
pub fn count(bits: &[u64]) -> u64 {
    bits.iter().map(|w| u64::from(w.count_ones())).sum()
}

/// Whether one of `heads` reads a lone version of `branch` at `epoch`
/// from a branch its volume owns: a head that reads it at all reads
/// every block of it, as no other version comes first.
pub fn lone_read(heads: &[Head], branch: u32, epoch: u64) -> bool {
    heads.iter().any(|h| {
        h.owned.contains(&branch)
            && h.layers
                .iter()
                .any(|&(br, lo, hi)| br == branch && (lo..=hi).contains(&epoch))
    })
}

/// Lays one chunk's versions (any order, every branch) over each other for
/// `heads`, and adds each branch's blocks and head blocks to `out`, by
/// branch. `scratch` is reused between chunks.
pub fn overlay(
    versions: &[Version],
    heads: &[Head],
    out: &mut HashMap<u32, (u64, u64)>,
    scratch: &mut Overlay,
) {
    if let [v] = versions {
        let n = count(&v.bits);
        let e = out.entry(v.branch).or_default();
        e.0 += n;
        if lone_read(heads, v.branch, v.epoch) {
            e.1 += n;
        }
        return;
    }
    let n = versions.len();
    let words = versions.iter().map(|v| v.bits.len()).max().unwrap_or(0);
    if scratch.seen.len() < n {
        scratch.seen.resize(n, Vec::new());
    }
    for s in &mut scratch.seen[..n] {
        s.clear();
        s.resize(words, 0);
    }
    // Newest first within a branch.
    scratch.order.clear();
    scratch.order.extend(0..n);
    scratch.order.sort_unstable_by(|&a, &b| {
        (versions[a].branch, versions[b].epoch).cmp(&(versions[b].branch, versions[a].epoch))
    });
    for h in heads {
        scratch.covered.clear();
        scratch.covered.resize(words, 0);
        for &(br, lo, hi) in &h.layers {
            let own = h.owned.contains(&br);
            for &i in &scratch.order {
                let v = &versions[i];
                if v.branch != br || v.epoch < lo || v.epoch > hi {
                    continue;
                }
                for (w, &b) in v.bits.iter().enumerate() {
                    if own {
                        scratch.seen[i][w] |= b & !scratch.covered[w];
                    }
                    scratch.covered[w] |= b;
                }
            }
        }
    }
    for (i, v) in versions.iter().enumerate() {
        let e = out.entry(v.branch).or_default();
        e.0 += count(&v.bits);
        e.1 += count(&scratch.seen[i]);
    }
}

/// [`overlay`]'s reusable buffers.
#[derive(Debug, Default)]
pub struct Overlay {
    seen: Vec<Vec<u64>>,
    order: Vec<usize>,
    covered: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngExt, SeedableRng, rngs::SmallRng};

    fn set(v: &[bool]) -> Vec<u64> {
        let mut out = vec![0u64; v.len().div_ceil(64)];
        for (i, &b) in v.iter().enumerate() {
            if b {
                out[i / 64] |= 1 << (i % 64);
            }
        }
        // As a block map gives it: no longer than its last block needs.
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }

    /// The overlay against reading each block as 08-engine.md §7.2 says.
    #[test]
    fn overlay_counts_what_each_owner_s_head_reads() {
        let mut rng = SmallRng::seed_from_u64(2);
        let mut s = Overlay::default();
        for _ in 0..3000 {
            let n = rng.random_range(1..8);
            let versions: Vec<Version> = (0..n)
                .map(|i| Version {
                    branch: rng.random_range(0..3),
                    epoch: i as u64 * 2 + rng.random_range(0..2),
                    bits: set(&(0..rng.random_range(1..130))
                        .map(|_| rng.random_bool(0.3))
                        .collect::<Vec<_>>()),
                })
                .collect();
            let heads: Vec<Head> = (0..rng.random_range(1..3))
                .map(|_| {
                    let mut layers = Vec::new();
                    for _ in 0..rng.random_range(1..4) {
                        let lo = rng.random_range(0..10);
                        layers.push((rng.random_range(0..3), lo, lo + rng.random_range(0..10)));
                    }
                    Head {
                        layers,
                        owned: (0..3).filter(|_| rng.random_bool(0.4)).collect(),
                    }
                })
                .collect();
            let mut got = HashMap::new();
            overlay(&versions, &heads, &mut got, &mut s);
            // Each block: which version each head reads it from.
            let mut seen = vec![vec![false; 130]; n];
            #[allow(clippy::needless_range_loop)]
            for h in &heads {
                for b in 0..130 {
                    'found: for &(br, lo, hi) in &h.layers {
                        let mut best: Option<usize> = None;
                        for (i, v) in versions.iter().enumerate() {
                            if v.branch == br
                                && (lo..=hi).contains(&v.epoch)
                                && v.bits.get(b / 64).is_some_and(|w| w >> (b % 64) & 1 == 1)
                                && best.is_none_or(|j| versions[j].epoch < v.epoch)
                            {
                                best = Some(i);
                            }
                        }
                        if let Some(i) = best {
                            if h.owned.contains(&br) {
                                seen[i][b] = true;
                            }
                            break 'found;
                        }
                    }
                }
            }
            let mut want: HashMap<u32, (u64, u64)> = HashMap::new();
            for (i, v) in versions.iter().enumerate() {
                let e = want.entry(v.branch).or_default();
                e.0 += count(&v.bits);
                e.1 += seen[i].iter().filter(|x| **x).count() as u64;
            }
            assert_eq!(got, want, "{versions:?} {heads:?}");
        }
    }

    #[test]
    fn reports_and_heads_round_trip() {
        let pgs = vec![(
            1,
            7,
            vec![BranchBlocks {
                lineage: 5 << 100,
                branch: 2,
                blocks: 9,
                head: 4,
            }],
        )];
        assert_eq!(decode_report(&encode_report(&pgs)).unwrap(), pgs);
        let heads = vec![(
            3u128,
            vec![Head {
                layers: vec![(1, 0, 9), (0, 0, 4)],
                owned: vec![1],
            }],
        )];
        let back = decode_heads(&encode_heads(&heads)).unwrap();
        assert_eq!(back.get(&3), Some(&heads[0].1));
    }
}
