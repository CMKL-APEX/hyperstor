//! The last open questions of the on-device index (08-engine.md §7.6;
//! #299): what snapshots cost as version objects against ranges shared
//! with reference counts (D10, D44), and whether metadata needs the
//! Bε-tree flavour once the log is the tree's write-ahead log (D44, D410,
//! G15).

use super::{OBJ_BLOCKS, layout as model};
use hyperstor_engine::*;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

fn vkey(o: u64, epoch: u64) -> ObjKey {
    ObjKey::new(
        1,
        (o % 128) as u32,
        Namespace::Vol,
        o.to_le_bytes().to_vec(),
        epoch,
    )
}

fn shard(bytes: u64) -> Shard {
    let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(bytes));
    format(
        &dev,
        &DeviceConfig {
            device_id: 1,
            cluster_id: 1,
            shards: 1,
            log_blocks: None,
            ..Default::default()
        },
    )
    .unwrap();
    let mut sh = open(&dev).unwrap().1.pop().unwrap();
    sh.set_defer_max(0);
    sh
}

/// Whether `runs` (sorted, as [`model::mappings`] gives) hold block `lb`.
fn holds(runs: &model::Runs, lb: u64) -> Option<u64> {
    let i = runs.partition_point(|r| r.0 <= lb);
    let r = runs[..i].last()?;
    (lb < r.0 + r.2).then(|| r.1 + lb - r.0)
}

fn leaf_of(place: (f64, f64), lb: u64) -> u64 {
    let at = place.0 + place.1 * lb as f64 / OBJ_BLOCKS as f64;
    (at / (model::NODE * model::FILL)) as u64
}

/// Volume chunks with `k` snapshots, each epoch overwriting `f` of the
/// blocks at random, 4 KiB at a time (D10: a write after a snapshot goes
/// to a new version object holding only the blocks written).
fn snapshot_case(gib: u64, f: f64, k: u64) {
    let chunks = (gib << 30) / (OBJ_BLOCKS * BLOCK as u64);
    let blocks = chunks * OBJ_BLOCKS;
    let data = (gib << 30) as f64 * (1.0 + f * k as f64);
    let mut sh = shard((((data * 1.3) as u64 >> 20) + 2048) << 20);
    let mut rng = SmallRng::seed_from_u64(k * 1000 + (f * 1000.0) as u64);
    let chunk = vec![0x5au8; (OBJ_BLOCKS * BLOCK as u64) as usize];
    for o in 0..chunks {
        sh.commit(vec![OpList {
            key: vkey(o, 0),
            ops: vec![Op::Write {
                off: 0,
                data: chunk.clone().into(),
                crcs: None,
            }],
        }])
        .unwrap();
    }
    // Per epoch, the blocks written, to count the head's leaves a shared
    // tree would copy for the snapshot that epoch's writes follow.
    let mut written: Vec<Vec<(u64, u64)>> = Vec::new();
    for e in 1..=k {
        let n = (blocks as f64 * f) as u64;
        let mut w = Vec::with_capacity(n as usize);
        let mut done = 0;
        while done < n {
            let lists: Vec<OpList> = (0..64.min(n - done))
                .map(|_| {
                    let (o, lb) = (rng.random_range(0..chunks), rng.random_range(0..OBJ_BLOCKS));
                    w.push((o, lb));
                    OpList {
                        key: vkey(o, e),
                        ops: vec![Op::Write {
                            off: lb * BLOCK as u64,
                            data: vec![0xa5u8; BLOCK].into(),
                            crcs: None,
                        }],
                    }
                })
                .collect();
            done += lists.len() as u64;
            sh.commit(lists).unwrap();
        }
        written.push(w);
    }
    sh.checkpoint().unwrap();

    // Version objects as built: every version's map in the tree.
    let maps = model::mappings(&sh);
    let versions = model::layout(&maps);
    let by_key: HashMap<ObjKey, &model::Runs> = maps.iter().map(|(k, r)| (k.clone(), r)).collect();

    // Shared ranges: the head's map (each block from its newest version),
    // and per snapshot the head leaves its later writes copied.
    let mut head_maps = Vec::new();
    for o in 0..chunks {
        let mut runs: model::Runs = Vec::new();
        for lb in 0..OBJ_BLOCKS {
            let pb = (0..=k)
                .rev()
                .find_map(|e| by_key.get(&vkey(o, e)).and_then(|r| holds(r, lb)))
                .expect("version 0 holds every block");
            match runs.last_mut() {
                Some((l, p, c)) if *l + *c == lb && *p + *c == pb => *c += 1,
                _ => runs.push((lb, pb, 1)),
            }
        }
        head_maps.push((vkey(o, 0), runs));
    }
    let head = model::layout(&head_maps);
    let mut copied = 0u64;
    for w in &written {
        let leaves: HashSet<u64> = w
            .iter()
            .map(|&(o, lb)| leaf_of(head.place[&vkey(o, 0)][3], lb))
            .collect();
        copied += leaves.len() as u64;
    }

    // A read at the head: versions probed, newest first, until one holds
    // the block, and the leaves their entries are in.
    let samples = 100_000;
    let (mut probes, mut leaves): (Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new());
    let mut reads = Vec::with_capacity(samples);
    for _ in 0..samples {
        let (o, lb) = (rng.random_range(0..chunks), rng.random_range(0..OBJ_BLOCKS));
        reads.push((o, lb));
        let mut seen = HashSet::new();
        let mut n = 0;
        for e in (0..=k).rev() {
            let key = vkey(o, e);
            let Some(r) = by_key.get(&key) else { continue };
            n += 1;
            seen.insert(leaf_of(versions.place[&key][3], lb));
            if holds(r, lb).is_some() {
                break;
            }
        }
        probes.push(n);
        leaves.push(seen.len() as u64);
    }
    let pct = |v: &mut Vec<u64>, p: f64| {
        v.sort_unstable();
        v[((v.len() - 1) as f64 * p) as usize]
    };
    let mean = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
    let (pm, lm) = (mean(&probes), mean(&leaves));
    let (p99, l99) = (pct(&mut probes, 0.99), pct(&mut leaves, 0.99));

    // Today's engine, in memory: a 4 KiB read through every layer against
    // the same read from one object.
    let layers: Vec<Vec<ObjKey>> = (0..chunks)
        .map(|o| (0..=k).rev().map(|e| vkey(o, e)).collect())
        .collect();
    let t = Instant::now();
    for &(o, lb) in &reads {
        sh.read_layered(&layers[o as usize], lb * BLOCK as u64, BLOCK as u64)
            .unwrap();
    }
    let layered = t.elapsed().as_secs_f64() * 1e9 / reads.len() as f64;
    let t = Instant::now();
    for &(o, lb) in &reads {
        sh.read(&vkey(o, 0), lb * BLOCK as u64, BLOCK as u64)
            .unwrap();
    }
    let single = t.elapsed().as_secs_f64() * 1e9 / reads.len() as f64;

    let mib = |b: f64| b / (1 << 20) as f64;
    println!(
        "| {:.0}% | {k} | {:.1} | {:.1} | {:.1} | {:.1} | {pm:.1} / {p99} | {lm:.2} / {l99} | {layered:.0} | {single:.0} |",
        f * 100.0,
        mib(versions.total[3]),
        mib(head.total[3]),
        mib(copied as f64 * model::NODE),
        mib(head.total[3] + copied as f64 * model::NODE),
    );
}

pub fn snapshot_suite() {
    let gib: u64 = std::env::var("HS_GIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    println!(
        "| written per epoch | snapshots | version objects MiB | shared: head MiB | \
         shared: leaves kept MiB | shared: total MiB | versions probed (mean / p99) | \
         leaves touched (mean / p99) | layered read ns | one-object read ns |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for f in [0.01, 0.1] {
        for k in [1, 4, 16, 64] {
            snapshot_case(gib, f, k);
        }
    }
}

/// A tree over `leaves` leaves with inner nodes of `fan` children: the
/// ancestors of a leaf, by level.
fn ancestors(leaf: u64, fan: u64, leaves: u64) -> Vec<(u32, u64)> {
    let (mut out, mut i, mut n, mut lvl) = (Vec::new(), leaf, leaves, 1);
    while n > 1 {
        i /= fan;
        n = n.div_ceil(fan);
        out.push((lvl, i));
        lvl += 1;
    }
    out
}

/// Pages written per update, at checkpoints every `per` updates to keys
/// spread evenly over `leaves` leaves: a copy-on-write B+tree (the dirty
/// leaves and their ancestors) and a Bε-tree (messages buffered in inner
/// nodes of `be_fan` children holding `cap` messages, the fullest child
/// flushed when a buffer overflows; every node changed is written).
fn meta_case(leaves: u64, per: u64, cap: usize, be_fan: u64) -> (f64, f64, u32, u32) {
    let bp_fan = (model::NODE * model::FILL / 24.0) as u64;
    let mut rng = SmallRng::seed_from_u64(leaves ^ per);
    let mut buffers: HashMap<(u32, u64), BTreeMap<u64, u32>> = HashMap::new();
    let height = |fan: u64| ancestors(0, fan, leaves).len() as u32 + 1;
    let root = (height(be_fan) - 1, 0u64);
    let (warm, measure) = (3, 5);
    let (mut bp_pages, mut be_pages) = (0u64, 0u64);
    for interval in 0..warm + measure {
        let mut bp: HashSet<(u32, u64)> = HashSet::new();
        let mut be: HashSet<(u32, u64)> = HashSet::new();
        for _ in 0..per {
            let leaf = rng.random_range(0..leaves);
            bp.insert((0, leaf));
            bp.extend(ancestors(leaf, bp_fan, leaves));
            if root.0 == 0 {
                be.insert((0, leaf));
                continue;
            }
            // Into the root's buffer, keyed by the child the message goes
            // to; overflow pushes the fullest child's messages down.
            let mut node = root;
            let mut child = leaf / be_fan.pow(node.0 - 1);
            *buffers.entry(node).or_default().entry(child).or_default() += 1;
            be.insert(node);
            loop {
                let b = buffers.get_mut(&node).unwrap();
                if b.values().map(|&c| c as usize).sum::<usize>() <= cap {
                    break;
                }
                let (&c, _) = b.iter().max_by_key(|(_, n)| **n).unwrap();
                let moved = b.remove(&c).unwrap();
                child = c;
                let next = (node.0 - 1, child);
                be.insert(next);
                if next.0 == 0 {
                    break;
                }
                // The child's buffer gets the messages, by its own children:
                // spread evenly, as uniform keys would be.
                let nb = buffers.entry(next).or_default();
                let base = child * be_fan;
                let span = be_fan.pow(next.0 - 1);
                for m in 0..moved {
                    let grand = base + (m as u64 % be_fan);
                    if grand * span < leaves {
                        *nb.entry(grand).or_default() += 1;
                    }
                }
                node = next;
            }
        }
        if interval >= warm {
            bp_pages += bp.len() as u64;
            be_pages += be.len() as u64;
        }
    }
    let n = (measure * per) as f64;
    (
        bp_pages as f64 / n,
        be_pages as f64 / n,
        height(bp_fan),
        height(be_fan),
    )
}

pub fn meta_suite() {
    // Today's log bytes per key-value put (32-byte key, 64-byte value),
    // 64 puts a round, as the map service's and kv store's small updates.
    let mut sh = shard(2 << 30);
    sh.checkpoint().unwrap();
    let mut rng = SmallRng::seed_from_u64(7);
    let puts = 20_000u64;
    let before = sh.stats().log_used_blocks;
    for _ in 0..puts / 64 {
        let lists = (0..64)
            .map(|_| {
                let mut k = vec![0u8; 32];
                rng.fill(&mut k[..]);
                OpList {
                    key: vkey(rng.random_range(0..1024), 0),
                    ops: vec![Op::KvPut(k, vec![0x11; 64])],
                }
            })
            .collect();
        sh.commit(lists).unwrap();
    }
    let log_per_put =
        (sh.stats().log_used_blocks - before) as f64 * BLOCK as f64 / (puts / 64 * 64) as f64;
    // The largest log (1 GiB) starts a checkpoint at a quarter.
    let per_interval = (262_144.0 * BLOCK as f64 / 4.0 / log_per_put) as u64;
    println!(
        "Log bytes per put: {log_per_put:.0}; updates per checkpoint with the largest log: {per_interval}\n"
    );
    // Entries of ~100 bytes: 28 to a leaf at 70%. Bε inner nodes: 16
    // children (pivots ~400 bytes), the rest of 4 KiB a buffer of ~33
    // messages.
    let (entry, cap, fan) = (100.0, 33usize, 16u64);
    let per_leaf = (model::NODE * model::FILL / entry) as u64;
    println!(
        "| keys | leaves | updates per checkpoint | per leaf | B+tree pages per update | \
         Bε pages per update | B+tree bytes per update | Bε bytes per update | \
         heights (B+ / Bε) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for keys in [1_000_000u64, 10_000_000, 100_000_000] {
        let leaves = keys / per_leaf;
        let mut pers: Vec<u64> = [0.01, 0.1, 1.0, 4.0]
            .iter()
            .map(|r| (leaves as f64 * r) as u64)
            .collect();
        pers.push(per_interval);
        pers.sort_unstable();
        pers.dedup();
        for per in pers {
            if per * 8 > 50_000_000 {
                continue;
            }
            let (bp, be, hb, he) = meta_case(leaves, per.max(1), cap, fan);
            let mark = if per == per_interval {
                " (largest log)"
            } else {
                ""
            };
            println!(
                "| {keys} | {leaves} | {per}{mark} | {:.2} | {bp:.3} | {be:.3} | {:.0} | {:.0} | {hb} / {he} |",
                per as f64 / leaves as f64,
                bp * model::NODE,
                be * model::NODE,
            );
        }
    }
}
