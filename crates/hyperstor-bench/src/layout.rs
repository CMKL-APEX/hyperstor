//! `hyperstor-bench layout`: what the on-device index's candidate leaf
//! formats (D43, 05-decision-log.md point 99) would cost on the block maps
//! the engine really builds, and what a copy-on-write tree would write per
//! checkpoint against what today's paged checkpoints write.
//!
//! Workloads go through a real shard (its allocator, redirect-on-write,
//! dense maps) on a memory device, with deferral off as storage nodes on
//! flash run (D38). The formats are then sized from the shard's mappings,
//! not built: this is a model, and its constants are below.

use super::{OBJ_BLOCKS, key};
use hyperstor_engine::*;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Tree node size, and how full a node is on average (B+tree splits leave
/// nodes about 70% full).
const NODE: f64 = 4096.0;
const FILL: f64 = 0.7;
/// An inner node's entry: a key (object id and block, 16 bytes) and a
/// child pointer (8 bytes).
const INNER_ENTRY: f64 = 24.0;
/// Per object in a leaf: its key (pool, PG, namespace, name, version),
/// written once per leaf the object appears in.
const OBJ_HEADER: f64 = 32.0;
/// Checksums: 4 bytes per block (CRC32C); a page holds 1024.
const CRC: f64 = 4.0;
const CRCS_PER_PAGE: u64 = 1024;

/// One object's mappings, as maximal runs contiguous both logically and
/// physically: (first logical block, physical block, blocks).
type Runs = Vec<(u64, u64, u64)>;

fn mappings(sh: &Shard) -> Vec<(ObjKey, Runs)> {
    sh.index()
        .objects
        .iter()
        .map(|(k, o)| {
            let mut out: Runs = Vec::new();
            for (lb, pb, crcs) in o.blocks.runs() {
                let n = crcs.len() as u64;
                match out.last_mut() {
                    Some((l, p, c)) if *l + *c == lb && *p + *c == pb => *c += n,
                    _ => out.push((lb, pb, n)),
                }
            }
            (k.clone(), out)
        })
        .collect()
}

/// The leaf bytes one object's entries take in each format.
#[derive(Default, Clone, Copy)]
struct Sizes {
    /// D43 as planned: an entry per block, (block u32, physical u32, CRC).
    per_block: f64,
    /// The same with 64-bit physical addresses.
    per_block64: f64,
    /// Range keys: (block u32, count u32, physical u64); CRCs elsewhere.
    ranges: f64,
    /// Range keys, and a packed entry where it is smaller: (block u32,
    /// count u32, base u64, then a u32 offset per block); CRCs elsewhere.
    packed: f64,
}

fn object_sizes(runs: &Runs) -> Sizes {
    let blocks: u64 = runs.iter().map(|r| r.2).sum();
    let mut packed = 0.0;
    // Stretches of logically adjacent runs: each is coded as ranges or as
    // one packed entry, whichever is smaller.
    let mut i = 0;
    while i < runs.len() {
        let mut j = i + 1;
        while j < runs.len() && runs[j - 1].0 + runs[j - 1].2 == runs[j].0 {
            j += 1;
        }
        let k = (j - i) as f64;
        let b: u64 = runs[i..j].iter().map(|r| r.2).sum();
        packed += (16.0 * k).min(16.0 + 4.0 * b as f64);
        i = j;
    }
    Sizes {
        per_block: 12.0 * blocks as f64,
        per_block64: 16.0 * blocks as f64,
        ranges: 16.0 * runs.len() as f64,
        packed,
    }
}

/// A tree over `leaf_bytes` of entries: leaf pages and inner bytes.
fn tree(leaf_bytes: f64) -> (u64, f64) {
    let leaves = (leaf_bytes / (NODE * FILL)).ceil().max(1.0) as u64;
    let fan = (NODE * FILL / INNER_ENTRY).floor() as u64;
    let (mut level, mut inner) = (leaves, 0u64);
    while level > 1 {
        level = level.div_ceil(fan);
        inner += level;
    }
    (leaves, inner as f64 * NODE)
}

struct Layout {
    objects: usize,
    blocks: u64,
    runs: u64,
    /// Per object: its cumulative leaf bytes before it, and its own, for
    /// each format (to place a changed block in a leaf).
    place: BTreeMap<ObjKey, [(f64, f64); 4]>,
    total: [f64; 4],
}

fn layout(maps: &[(ObjKey, Runs)]) -> Layout {
    let mut total = [0.0f64; 4];
    let mut place = BTreeMap::new();
    let (mut blocks, mut runs) = (0, 0);
    for (k, r) in maps {
        let s = object_sizes(r);
        let each = [s.per_block, s.per_block64, s.ranges, s.packed];
        let mut p = [(0.0, 0.0); 4];
        for f in 0..4 {
            let own = each[f] + OBJ_HEADER;
            p[f] = (total[f], own);
            total[f] += own;
        }
        place.insert(k.clone(), p);
        blocks += r.iter().map(|x| x.2).sum::<u64>();
        runs += r.len() as u64;
    }
    Layout {
        objects: maps.len(),
        blocks,
        runs,
        place,
        total,
    }
}

const FORMATS: [&str; 4] = [
    "per-block, CRC in leaf",
    "per-block, 64-bit",
    "ranges",
    "ranges + packed",
];

/// Prints the index sizes of each format for the shard as it is now.
fn report_sizes(sh: &Shard, label: &str, data_gib: f64) {
    let maps = mappings(sh);
    let l = layout(&maps);
    let ix = sh.index_stats();
    let st = sh.stats();
    let per_tib = |b: f64| b / data_gib * 1024.0 / (1u64 << 30) as f64;
    let crc = l.blocks as f64 * CRC;
    println!(
        "| {label} | {} | {} | {} | {:.1} | {:.1} |",
        l.objects,
        l.runs,
        ix.dense_objects,
        per_tib(ix.map_bytes as f64),
        per_tib(st.checkpoint_bytes as f64),
    );
    for (f, name) in FORMATS.iter().enumerate() {
        let (leaves, inner) = tree(l.total[f]);
        // CRCs sit in the leaves for the per-block formats; for ranges,
        // in pages of their own (point 99, option b).
        let crc_pages = if f >= 2 { crc / FILL.max(0.9) } else { 0.0 };
        println!(
            "|  ↳ {name} | leaves {:.2} GiB/TiB | inner {:.1} MiB/TiB | CRC pages {:.2} GiB/TiB | total {:.2} GiB/TiB | {:.1} B/block |",
            per_tib(leaves as f64 * NODE),
            per_tib(inner) * 1024.0,
            per_tib(crc_pages),
            per_tib(leaves as f64 * NODE + inner + crc_pages),
            (leaves as f64 * NODE + crc_pages) / l.blocks.max(1) as f64,
        );
    }
}

/// Pages a copy-on-write tree rewrites for the blocks changed in one
/// checkpoint interval, per format: the leaves holding them, their
/// ancestors, and (for range formats) the CRC pages.
fn cow_pages(l: &Layout, changed: &BTreeSet<(ObjKey, u64)>) -> [u64; 4] {
    let fan = (NODE * FILL / INNER_ENTRY).floor() as u64;
    let cap = NODE * FILL;
    let mut out = [0u64; 4];
    for (f, slot) in out.iter_mut().enumerate() {
        let mut leaves = BTreeSet::new();
        let mut crc_pages = BTreeSet::new();
        for (k, lb) in changed {
            let Some(p) = l.place.get(k) else { continue };
            let (before, own) = p[f];
            let at = before + own * (*lb as f64 / OBJ_BLOCKS as f64);
            leaves.insert((at / cap) as u64);
            crc_pages.insert((k.clone(), lb / CRCS_PER_PAGE));
        }
        let mut pages = leaves.len() as u64;
        let mut level: BTreeSet<u64> = leaves;
        while level.len() > 1 || level.iter().next().is_some_and(|&x| x > 0) {
            level = level.iter().map(|x| x / fan).collect();
            pages += level.len() as u64;
            if level.len() == 1 && level.contains(&0) {
                break;
            }
        }
        if f >= 2 {
            pages += crc_pages.len() as u64;
        }
        *slot = pages;
    }
    out
}

/// Writes `n` blocks' worth of overwrites, chosen by `pick`, through the
/// shard; every `interval` blocks takes a checkpoint and prints what
/// today's engine wrote for it against the copy-on-write model.
fn overwrite(
    sh: &mut Shard,
    n: u64,
    interval: u64,
    rng: &mut SmallRng,
    pick: &mut dyn FnMut(&mut SmallRng) -> (u64, u64, u64),
    cow: &mut CowTally,
) {
    let mut done = 0;
    let mut changed: BTreeSet<(ObjKey, u64)> = BTreeSet::new();
    let mut since = 0;
    while done < n {
        let mut lists = Vec::new();
        let mut batch = 0;
        while batch < 64 && done < n {
            let (o, lb, len) = pick(rng);
            let len = len.min(OBJ_BLOCKS - lb).max(1);
            lists.push(OpList {
                key: key(o),
                ops: vec![Op::Write {
                    off: lb * BLOCK as u64,
                    data: vec![0xa5u8; (len * BLOCK as u64) as usize].into(),
                    crcs: None,
                }],
            });
            for b in lb..lb + len {
                changed.insert((key(o), b));
            }
            done += len;
            since += len;
            batch += 1;
        }
        sh.commit(lists).unwrap();
        if since >= interval {
            cow.interval(sh, &changed);
            changed.clear();
            since = 0;
        }
    }
    if since > 0 {
        cow.interval(sh, &changed);
    }
}

/// Bytes written per checkpoint interval: today's engine (log records and
/// checkpoint pages, measured) against the copy-on-write tree (modelled,
/// on top of the same log).
#[derive(Default)]
struct CowTally {
    intervals: u64,
    blocks_changed: u64,
    ckpt_bytes: u64,
    cow_pages: [u64; 4],
}

impl CowTally {
    fn interval(&mut self, sh: &mut Shard, changed: &BTreeSet<(ObjKey, u64)>) {
        let st0 = sh.stats();
        sh.checkpoint().unwrap();
        let st = sh.stats();
        self.ckpt_bytes += st.checkpoint_bytes_written - st0.checkpoint_bytes_written;
        let l = layout(&mappings(sh));
        let pages = cow_pages(&l, changed);
        for (t, p) in self.cow_pages.iter_mut().zip(pages) {
            *t += p;
        }
        self.intervals += 1;
        self.blocks_changed += changed.len() as u64;
    }

    fn print(&self, label: &str, log_blocks: u64) {
        let per = |b: f64| b / self.blocks_changed.max(1) as f64;
        let today = self.ckpt_bytes as f64;
        print!(
            "| {label} | {} | {:.0} | {:.1} | {:.1} |",
            self.intervals,
            self.blocks_changed as f64 / self.intervals.max(1) as f64,
            per(log_blocks as f64 * BLOCK as f64),
            per(today),
        );
        for f in 0..4 {
            print!(" {:.1} |", per(self.cow_pages[f] as f64 * NODE));
        }
        println!();
    }
}

fn fresh(data_gib: u64) -> Shard {
    // Twice the data, so redirect-on-write always finds space.
    let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new((2 * data_gib + 2) << 30));
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

fn fill(sh: &mut Shard, objects: u64) {
    let chunk = vec![0x5au8; (OBJ_BLOCKS * BLOCK as u64) as usize];
    for o in 0..objects {
        sh.commit(vec![OpList {
            key: key(o),
            ops: vec![Op::Write {
                off: 0,
                data: chunk.clone().into(),
                crcs: None,
            }],
        }])
        .unwrap();
    }
    sh.checkpoint().unwrap();
}

/// A write trace: one write per line, `offset,length` in bytes, or the
/// MSR Cambridge format (`time,host,disk,Write,offset,size,response`);
/// reads are skipped. Offsets wrap at the volume's size.
fn read_trace(path: &str, volume: u64) -> Vec<(u64, u64)> {
    let text = std::fs::read_to_string(path).expect("trace file");
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        let (off, len) = match f.len() {
            2 => (f[0].parse::<u64>(), f[1].parse::<u64>()),
            7 if f[3].eq_ignore_ascii_case("write") => (f[4].parse(), f[5].parse()),
            _ => continue,
        };
        let (Ok(off), Ok(len)) = (off, len) else {
            continue;
        };
        if len == 0 {
            continue;
        }
        let b = BLOCK as u64;
        let first = (off / b) % (volume / b);
        let last = (off + len).div_ceil(b);
        out.push((first, (last - off / b).max(1)));
    }
    out
}

pub fn suite() {
    let gib: u64 = std::env::var("HS_GIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let interval: u64 = std::env::var("HS_INTERVAL_BLOCKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16384);
    let objects = (gib << 30) / (OBJ_BLOCKS * BLOCK as u64);
    let total = objects * OBJ_BLOCKS;
    let data_gib = gib as f64;

    println!(
        "Model constants: {NODE} B nodes {FILL} full; inner entry {INNER_ENTRY} B; object header {OBJ_HEADER} B per leaf; CRC {CRC} B per block, {CRCS_PER_PAGE} per page. {gib} GiB of 4 MiB chunks; checkpoint every {interval} blocks changed."
    );
    println!();
    println!(
        "| Workload | objects | runs | dense objects | today: index RAM GiB/TiB | today: checkpoint GiB/TiB |"
    );
    println!("|---|---|---|---|---|---|");

    let mut cows: Vec<(String, CowTally, u64)> = Vec::new();

    // 1. Written once, sequentially: images, weights, checkpoints.
    let mut sh = fresh(gib);
    fill(&mut sh, objects);
    report_sizes(&sh, "sequential fill", data_gib);
    drop(sh);

    // 2. Uniform random 4 KiB overwrites, in stages.
    type Picker = Box<dyn FnMut(&mut SmallRng) -> (u64, u64, u64)>;
    let workloads: Vec<(&str, Picker)> = vec![
        (
            "random 4 KiB",
            Box::new(move |r: &mut SmallRng| {
                let b = r.random_range(0..total);
                (b / OBJ_BLOCKS, b % OBJ_BLOCKS, 1)
            }),
        ),
        (
            "hot 10% gets 90%, 4 KiB",
            Box::new(move |r: &mut SmallRng| {
                let hot = total / 10;
                let b = if r.random_range(0..10) < 9 {
                    r.random_range(0..hot)
                } else {
                    r.random_range(0..total)
                };
                (b / OBJ_BLOCKS, b % OBJ_BLOCKS, 1)
            }),
        ),
        (
            "mixed sizes 4 KiB–1 MiB",
            Box::new(move |r: &mut SmallRng| {
                let len = match r.random_range(0..10) {
                    0..5 => 1,
                    5..9 => 1 << r.random_range(1..5), // 8–64 KiB
                    _ => 1 << r.random_range(5..9),    // 128 KiB–1 MiB
                };
                let b = r.random_range(0..total) / len * len;
                (b / OBJ_BLOCKS, b % OBJ_BLOCKS, len)
            }),
        ),
    ];
    for (name, mut pick) in workloads {
        let mut sh = fresh(gib);
        fill(&mut sh, objects);
        let mut rng = SmallRng::seed_from_u64(99);
        let mut cow = CowTally::default();
        let log0 = sh.stats().log_blocks_written;
        let mut written = 0;
        for frac in [0.05, 0.25, 1.0, 3.0] {
            let upto = (total as f64 * frac) as u64;
            overwrite(
                &mut sh,
                upto - written,
                interval,
                &mut rng,
                &mut *pick,
                &mut cow,
            );
            written = upto;
            report_sizes(&sh, &format!("{name}, {frac}× overwritten"), data_gib);
        }
        let log = sh.stats().log_blocks_written - log0;
        cows.push((name.to_string(), cow, log));
    }

    // 3. A trace, if given, on an empty volume (thin, as a new VM disk).
    if let Ok(path) = std::env::var("HS_TRACE") {
        let writes = read_trace(&path, gib << 30);
        let mut sh = fresh(gib);
        let mut it = writes.into_iter();
        let mut rng = SmallRng::seed_from_u64(1);
        let mut cow = CowTally::default();
        let log0 = sh.stats().log_blocks_written;
        let n: u64 = std::env::var("HS_TRACE_BLOCKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(u64::MAX);
        let mut pick = |_: &mut SmallRng| match it.next() {
            Some((b, len)) => {
                let lb = b % OBJ_BLOCKS;
                (b / OBJ_BLOCKS, lb, len.min(OBJ_BLOCKS - lb).max(1))
            }
            None => (u64::MAX, 0, 0),
        };
        // Replays until the trace ends or n blocks are written.
        let mut done = 0;
        let mut ended = false;
        while !ended && done < n {
            let mut batch: Vec<(u64, u64, u64)> = Vec::new();
            for _ in 0..1024 {
                let w = pick(&mut rng);
                if w.0 == u64::MAX {
                    ended = true;
                    break;
                }
                batch.push(w);
            }
            let blocks: u64 = batch.iter().map(|w| w.2).sum();
            let mut q = batch.into_iter();
            overwrite(
                &mut sh,
                blocks,
                interval,
                &mut rng,
                &mut |_| q.next().unwrap_or((0, 0, 1)),
                &mut cow,
            );
            done += blocks;
        }
        report_sizes(&sh, &format!("trace {path}"), data_gib);
        let log = sh.stats().log_blocks_written - log0;
        cows.push((format!("trace {path}"), cow, log));
    }

    println!();
    println!(
        "Bytes written per block changed, per checkpoint interval (log: today's records, which the tree keeps as its write-ahead log; today: paged checkpoints, measured; then each format's copy-on-write pages, modelled):"
    );
    println!();
    print!(
        "| Workload | intervals | blocks changed per interval | log B/block | today's checkpoint B/block |"
    );
    for f in FORMATS {
        print!(" CoW {f} B/block |");
    }
    println!();
    println!("|---|---|---|---|---|---|---|---|---|");
    for (name, cow, log) in &cows {
        cow.print(name, *log);
    }
}
