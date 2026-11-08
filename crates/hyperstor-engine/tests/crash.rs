//! Crash consistency (spec/04-decisions.md D22): random operation lists against a
//! reference model, with power lost at random device operations, unflushed
//! blocks kept or lost at random (reordered and torn writes), and power
//! lost again during recovery.
//!
//! After each crash, the recovered shard must equal the model after every
//! acknowledged list and some prefix of the lists in flight. Revisions are
//! compared only for acknowledged state, since the model doesn't know the
//! revisions of lists whose commit never returned.
//!
//! The `damaged_*` tests (#241) then make the device lie: a block of the
//! log, a superblock or the data region damaged on the media, a write lost
//! or stored at the wrong block. The shard must refuse to open, or open
//! with every acknowledged change, a damaged block reading as a checksum
//! error and never as other data.

use hyperstor_engine::*;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::BTreeMap;
use std::sync::Arc;

const MAX_BLOCK: u64 = 40;

#[derive(Debug, Clone, Default, PartialEq)]
struct MObj {
    size: u64,
    revision: Option<u64>,
    data: BTreeMap<u64, u8>,
    attrs: BTreeMap<Vec<u8>, Vec<u8>>,
    kv: BTreeMap<Vec<u8>, Vec<u8>>,
}

type Model = BTreeMap<ObjKey, MObj>;

/// Applies a list the way the engine does. Returns false if refused.
fn model_apply(m: &mut Model, l: &OpList, revision: Option<u64>) -> bool {
    let exists = m.contains_key(&l.key);
    for op in &l.ops {
        match op {
            Op::AssertExists if !exists => return false,
            Op::AssertAbsent if exists => return false,
            _ => {}
        }
    }
    let mutates = l.ops.iter().any(|o| {
        !matches!(
            o,
            Op::AssertExists | Op::AssertAbsent | Op::AssertRevision(_)
        )
    });
    if !mutates {
        return true;
    }
    if l.ops
        .iter()
        .any(|o| matches!(o, Op::Absorb(s) | Op::Take(s) if *s == l.key))
    {
        return false;
    }
    let mut alive = exists;
    for op in &l.ops {
        match op {
            Op::AssertExists | Op::AssertAbsent | Op::AssertRevision(_) => continue,
            Op::Delete => {
                m.remove(&l.key);
                alive = false;
                continue;
            }
            Op::Take(src) => {
                if let Some(s) = m.remove(src) {
                    m.insert(l.key.clone(), s);
                    alive = true;
                }
                continue;
            }
            Op::Absorb(src) => {
                alive = true;
                let s = m.remove(src);
                let o = m.entry(l.key.clone()).or_default();
                if let Some(s) = s {
                    for (b, v) in s.data {
                        o.data.entry(b).or_insert(v);
                    }
                    o.size = o.size.max(s.size);
                }
                continue;
            }
            _ => alive = true,
        }
        let o = m.entry(l.key.clone()).or_default();
        match op {
            Op::Touch => {}
            Op::Write { off, data, .. } => {
                let lb = off / BLOCK as u64;
                for (i, c) in data.chunks(BLOCK).enumerate() {
                    o.data.insert(lb + i as u64, c[0]);
                }
                o.size = o.size.max(off + data.len() as u64);
            }
            Op::Zero { off, len } => {
                for b in off / BLOCK as u64..(off + len) / BLOCK as u64 {
                    o.data.remove(&b);
                }
            }
            Op::Truncate(s) => {
                let lb = s.div_ceil(BLOCK as u64);
                o.data.retain(|&b, _| b < lb);
                o.size = *s;
            }
            Op::SetAttr(k, v) => {
                o.attrs.insert(k.clone(), v.clone());
            }
            Op::RmAttr(k) => {
                o.attrs.remove(k);
            }
            Op::KvPut(k, v) => {
                o.kv.insert(k.clone(), v.clone());
            }
            Op::KvDel(k) => {
                o.kv.remove(k);
            }
            Op::KvDelRange(a, b) => {
                o.kv.retain(|k, _| !(k >= a && k < b));
            }
            _ => unreachable!(),
        }
    }
    debug_assert_eq!(alive, m.contains_key(&l.key));
    if let Some(o) = m.get_mut(&l.key) {
        o.revision = revision;
    }
    true
}

fn keys() -> Vec<ObjKey> {
    (0..6)
        .map(|i| {
            let ns = [Namespace::Vol, Namespace::Kv, Namespace::Meta][i % 3];
            ObjKey::new(
                1,
                (i / 2) as u32,
                ns,
                format!("obj{i}").into_bytes(),
                i as u64 % 2,
            )
        })
        .collect()
}

fn random_list(rng: &mut SmallRng, keys: &[ObjKey]) -> OpList {
    let key = keys[rng.random_range(0..keys.len())].clone();
    let n = rng.random_range(1..4);
    let mut ops = Vec::new();
    for _ in 0..n {
        let op = match rng.random_range(0..100) {
            0..=39 => {
                let blocks = if rng.random_bool(0.6) {
                    rng.random_range(1..4)
                } else {
                    rng.random_range(4..17)
                };
                let off = rng.random_range(0..MAX_BLOCK - blocks) * BLOCK as u64;
                let fill = rng.random_range(1..=255u8);
                // A byte of its own in each block, so a CRC filed under
                // the wrong block doesn't match it; about half the writes
                // come with their CRCs, as writes from the network do (I306).
                let data: Vec<u8> = (0..blocks as usize)
                    .flat_map(|i| [((fill as usize + i) % 255 + 1) as u8; BLOCK])
                    .collect();
                let crcs = rng
                    .random_bool(0.5)
                    .then(|| data.chunks(BLOCK).map(hyperstor_codec::crc32c).collect());
                Op::Write { off, data, crcs }
            }
            40..=47 => {
                let b = rng.random_range(0..MAX_BLOCK - 4);
                Op::Zero {
                    off: b * BLOCK as u64,
                    len: rng.random_range(1..5) * BLOCK as u64,
                }
            }
            48..=52 => Op::Truncate(rng.random_range(0..MAX_BLOCK * BLOCK as u64)),
            53..=60 => Op::KvPut(
                vec![rng.random_range(b'a'..=b'h')],
                vec![rng.random(); rng.random_range(0..40)],
            ),
            61..=65 => Op::KvDel(vec![rng.random_range(b'a'..=b'h')]),
            66..=68 => Op::KvDelRange(vec![b'b'], vec![b'e']),
            69..=74 => Op::SetAttr(vec![rng.random_range(b'x'..=b'z')], vec![rng.random()]),
            75..=77 => Op::RmAttr(vec![rng.random_range(b'x'..=b'z')]),
            78..=83 => Op::Delete,
            84..=86 => Op::Touch,
            87..=89 => {
                let i = keys.iter().position(|k| *k == key).unwrap();
                Op::Absorb(keys[(i + rng.random_range(1..keys.len())) % keys.len()].clone())
            }
            // Rare: a list with a `Take` finishes the checkpoint in
            // progress, and crashes during one must stay common.
            90 => {
                let i = keys.iter().position(|k| *k == key).unwrap();
                Op::Take(keys[(i + rng.random_range(1..keys.len())) % keys.len()].clone())
            }
            91..=96 => Op::AssertExists,
            _ => Op::AssertAbsent,
        };
        ops.push(op);
    }
    OpList { key, ops }
}

fn assert_matches(sh: &Shard, m: &Model, check_revisions: bool) -> Result<(), String> {
    let engine_keys: Vec<ObjKey> = sh.index().objects.keys().cloned().collect();
    let model_keys: Vec<ObjKey> = m.keys().cloned().collect();
    if engine_keys != model_keys {
        return Err(format!(
            "keys differ: engine {engine_keys:?} model {model_keys:?}"
        ));
    }
    for (k, o) in m {
        let st = sh.stat(k).unwrap();
        if st.size != o.size {
            return Err(format!("{k:?}: size {} vs model {}", st.size, o.size));
        }
        if check_revisions
            && let Some(r) = o.revision
            && st.revision != r
        {
            return Err(format!("{k:?}: revision {} vs model {r}", st.revision));
        }
        let data = sh
            .read(k, 0, MAX_BLOCK * BLOCK as u64)
            .map_err(|e| format!("{k:?}: read failed: {e}"))?;
        for b in 0..MAX_BLOCK {
            let want = o.data.get(&b).copied().unwrap_or(0);
            let got = &data[b as usize * BLOCK..(b as usize + 1) * BLOCK];
            if got.iter().any(|&x| x != want) {
                return Err(format!("{k:?}: block {b} is {} not {want}", got[0]));
            }
        }
        if sh.attrs(k).unwrap() != &o.attrs {
            return Err(format!("{k:?}: attrs differ"));
        }
        let kv = sh.kv_scan(k, b"", None, usize::MAX);
        let mkv: Vec<(Vec<u8>, Vec<u8>)> =
            o.kv.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
        if kv != mkv {
            return Err(format!("{k:?}: kv differs"));
        }
        // The byte count kept as the table changes, after overwrites,
        // deletes, ranges, takes and replay (25-kv-ranges.md §28.2).
        let bytes: u64 = o.kv.iter().map(|(a, b)| (a.len() + b.len()) as u64).sum();
        if st.kv_bytes != bytes {
            return Err(format!("{k:?}: kv_bytes {} vs model {bytes}", st.kv_bytes));
        }
    }
    Ok(())
}

/// Crashes that happened while a fuzzy checkpoint was being written.
static MID_CHECKPOINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Commits after which some object's block map was dense.
static DENSE_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// The block-map policy each open applies (the dense-map test forces
    /// dense maps for every object with more than one run).
    static POLICY: std::cell::Cell<Option<DensePolicy>> = const { std::cell::Cell::new(None) };
    /// The checkpoint page size of this run (D105): small in most runs, so
    /// checkpoints write some pages and keep others, cut and drop them.
    static PAGE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn open_shard(d: &Arc<dyn BlockDev>) -> Result<Shard, EngineError> {
    open(d).map(|(_, mut v)| {
        let mut sh = v.pop().unwrap();
        // Tiny slices: fuzzy checkpoints span many commit rounds, so power
        // is lost in the middle of them.
        sh.set_checkpoint_slice(1024);
        if let Some(b) = PAGE.with(|p| p.get()) {
            sh.set_checkpoint_page(b);
        }
        if let Some(p) = POLICY.with(|p| p.get()) {
            sh.set_dense_policy(p);
        }
        sh
    })
}

/// One run: random batches, a crash at a random point, recovery (possibly
/// crashing again), and comparison with the model. Returns the number of
/// crashes checked.
fn run(seed: u64) -> usize {
    let mut rng = SmallRng::seed_from_u64(seed);
    PAGE.with(|p| p.set([None, Some(256), Some(2048)][seed as usize % 3]));
    let keys = keys();
    let len = 1024 * BLOCK as u64;
    let fd = Arc::new(FaultDev::new(len));
    let d: Arc<dyn BlockDev> = fd.clone();
    format(
        &d,
        &DeviceConfig {
            device_id: 1,
            cluster_id: 2,
            shards: 1,
            log_blocks: Some(64),
            ..Default::default()
        },
    )
    .unwrap();
    let mut fd = fd;
    let mut d = d;
    let mut sh = open_shard(&d).unwrap();
    let mut model: Model = BTreeMap::new();
    let mut crashes = 0;
    for _round in 0..6 {
        let batches = rng.random_range(1..12);
        let crash_mid = rng.random_bool(0.7);
        let mut in_flight: Vec<OpList> = Vec::new();
        for b in 0..batches {
            let batch: Vec<OpList> = (0..rng.random_range(1..6))
                .map(|_| random_list(&mut rng, &keys))
                .collect();
            if crash_mid && b == batches - 1 {
                fd.fail_after(rng.random_range(0..12));
            }
            match sh.commit(batch.clone()) {
                Ok(results) => {
                    for (l, r) in batch.iter().zip(results) {
                        match r {
                            Ok(c) => {
                                let ok = model_apply(&mut model, l, Some(c.revision));
                                assert!(
                                    ok,
                                    "seed {seed}: engine accepted a list the model refuses: {l:?}"
                                );
                            }
                            Err(OpError::AssertFailed { .. }) => {
                                let mut probe = model.clone();
                                assert!(
                                    !model_apply(&mut probe, l, None),
                                    "seed {seed}: engine refused a list the model accepts: {l:?}"
                                );
                            }
                            // Refused whole: the tiny test log bounds a list's size.
                            Err(OpError::TooLarge) => {}
                            Err(e) => panic!("seed {seed}: unexpected {e:?}"),
                        }
                    }
                    if sh.index_stats().dense_objects > 0 {
                        DENSE_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let armed = crash_mid && b == batches - 1;
                    if !armed && let Err(e) = assert_matches(&sh, &model, true) {
                        panic!(
                            "seed {seed}: diverged before any crash after batch {batch:#?}: {e}"
                        );
                    }
                    if sh.stats().pending_blocks > 0 && rng.random_bool(0.3) {
                        // Power may go out here too; the state is then
                        // exactly what was acknowledged.
                        if sh.apply_deferred().is_err() {
                            break;
                        }
                    }
                }
                Err(_) => {
                    in_flight = batch;
                    break;
                }
            }
        }
        if sh.checkpoint_in_progress() {
            MID_CHECKPOINT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // Power loss: keep a random subset of unflushed blocks.
        let p = rng.random_range(0.0..1.0);
        let img = fd.crash_image(|_| rng.random_bool(p));
        drop(sh);
        fd = Arc::new(FaultDev::from_image(img));
        d = fd.clone();
        // Sometimes lose power again during recovery.
        if rng.random_bool(0.3) {
            fd.fail_after(rng.random_range(0..20));
            if open_shard(&d).is_err() {
                let p = rng.random_range(0.0..1.0);
                let img = fd.crash_image(|_| rng.random_bool(p));
                fd = Arc::new(FaultDev::from_image(img));
                d = fd.clone();
            } else {
                fd.fail_after(u64::MAX);
            }
        }
        fd.fail_after(u64::MAX);
        sh = open_shard(&d).unwrap_or_else(|e| panic!("seed {seed}: recovery failed: {e}"));
        crashes += 1;
        // Candidates: the acknowledged state, then each prefix of the lists
        // in flight.
        let mut candidates = vec![(model.clone(), true)];
        let mut m = model.clone();
        let max = sh.max_list_bytes();
        for l in &in_flight {
            if hyperstor_engine::shard::estimate(l) > max {
                continue; // refused whole by the engine
            }
            model_apply(&mut m, l, None);
            candidates.push((m.clone(), false));
        }
        let mut errors = Vec::new();
        let mut found = None;
        for (i, (c, acked)) in candidates.iter().enumerate() {
            match assert_matches(&sh, c, *acked) {
                Ok(()) => {
                    found = Some(i);
                    break;
                }
                Err(e) => errors.push(e),
            }
        }
        if std::env::var("HS_CRASH_DEBUG").is_ok() {
            eprintln!(
                "seed {seed}: crash_mid {crash_mid}, in flight {} lists: {:?}",
                in_flight.len(),
                in_flight
                    .iter()
                    .map(|l| (
                        String::from_utf8_lossy(&l.key.name).to_string(),
                        l.ops
                            .iter()
                            .map(|o| match o {
                                Op::Write { off, data, .. } =>
                                    format!("W{}+{}", off / 4096, data.len() / 4096),
                                o => format!("{o:?}").chars().take(20).collect(),
                            })
                            .collect::<Vec<_>>()
                    ))
                    .collect::<Vec<_>>()
            );
        }
        let Some(i) = found else {
            panic!("seed {seed}: recovered state matches no candidate: {errors:#?}");
        };
        // Continue from what was recovered; revisions of lists that were in
        // flight are now known from the engine.
        model = candidates[i].0.clone();
        for (k, o) in model.iter_mut() {
            o.revision = Some(sh.stat(k).unwrap().revision);
        }
        fd.fail_after(u64::MAX);
    }
    crashes
}

fn run_seeds(label: &str) {
    let seeds: u64 = std::env::var("HS_CRASH_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let only: Option<u64> = std::env::var("HS_CRASH_SEED")
        .ok()
        .and_then(|s| s.parse().ok());
    let range = only.map_or(0..seeds, |s| s..s + 1);
    let runs = range.end - range.start;
    let mut crashes = 0;
    for seed in range {
        crashes += run(seed);
    }
    assert!(crashes >= runs as usize);
    let mid = MID_CHECKPOINT.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "{label}: {runs} runs, {crashes} crashes recovered and checked ({mid} so far during a fuzzy checkpoint)"
    );
    if runs >= 100 {
        assert!(mid > 50, "crashes during fuzzy checkpoints were exercised");
    }
}

#[test]
fn random_crashes_recover_to_a_consistent_prefix() {
    run_seeds("default block maps");
}

/// The same, with every fragmented object's block map dense (D43), so
/// recovery replays into dense maps and checkpoints carry them.
#[test]
fn random_crashes_with_dense_block_maps() {
    POLICY.with(|p| {
        p.set(Some(DensePolicy {
            disabled: false,
            run_overhead: 1 << 20,
        }))
    });
    run_seeds("dense block maps");
    let seen = DENSE_SEEN.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("commits with dense block maps present: {seen}");
    assert!(seen > 100, "dense maps were exercised");
}

/// What a device fault did to the reopened shard.
#[derive(Debug, Default)]
struct Outcomes {
    /// Opened with every acknowledged change, every block read back.
    intact: usize,
    /// Opened with every acknowledged change; some block read as a
    /// checksum error.
    detected: usize,
    /// Refused to open.
    refused: usize,
    /// Opened without some acknowledged change: the seeds, and the first
    /// one's differences.
    silent: Vec<u64>,
    first: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Damage {
    /// A block of the log ring, damaged on the media.
    LogBlock,
    /// One of the shard's two superblocks.
    ShardSuper,
    /// One of the device's two superblocks.
    DeviceSuper,
    /// A block of the data region: data, or a checkpoint page or directory.
    DataBlock,
    /// A write the device acknowledged and never stored.
    LostWrite,
    /// A write the device stored at another block.
    MisdirectedWrite,
}

/// The model's lists applied as the engine answered them.
fn apply_results(
    seed: u64,
    model: &mut Model,
    batch: &[OpList],
    results: Vec<Result<Committed, OpError>>,
) {
    for (l, r) in batch.iter().zip(results) {
        match r {
            Ok(c) => assert!(
                model_apply(model, l, Some(c.revision)),
                "seed {seed}: engine accepted a list the model refuses: {l:?}"
            ),
            Err(OpError::AssertFailed { .. }) => {
                let mut probe = model.clone();
                assert!(
                    !model_apply(&mut probe, l, None),
                    "seed {seed}: engine refused a list the model accepts: {l:?}"
                );
            }
            Err(OpError::TooLarge) => {}
            Err(e) => panic!("seed {seed}: unexpected {e:?}"),
        }
    }
}

/// Whether `sh` holds `m`, reading block by block: a block may read as a
/// checksum error (counted), never as other data. Revisions aren't
/// compared: a damaged device's are not the question.
fn holds(sh: &Shard, m: &Model) -> Result<usize, String> {
    let engine_keys: Vec<ObjKey> = sh.index().objects.keys().cloned().collect();
    let model_keys: Vec<ObjKey> = m.keys().cloned().collect();
    if engine_keys != model_keys {
        return Err(format!(
            "keys differ: engine {engine_keys:?} model {model_keys:?}"
        ));
    }
    let mut bad = 0;
    for (k, o) in m {
        let st = sh.stat(k).unwrap();
        if st.size != o.size {
            return Err(format!("{k:?}: size {} vs model {}", st.size, o.size));
        }
        for b in 0..MAX_BLOCK {
            let want = o.data.get(&b).copied().unwrap_or(0);
            match sh.read(k, b * BLOCK as u64, BLOCK as u64) {
                Ok(d) if d.iter().all(|&x| x == want) => {}
                Ok(d) => return Err(format!("{k:?}: block {b} is {} not {want}", d[0])),
                Err(EngineError::Checksum { .. }) => bad += 1,
                Err(e) => return Err(format!("{k:?}: block {b}: {e}")),
            }
        }
        if sh.attrs(k).unwrap() != &o.attrs {
            return Err(format!("{k:?}: attrs differ"));
        }
        let kv = sh.kv_scan(k, b"", None, usize::MAX);
        let mkv: Vec<(Vec<u8>, Vec<u8>)> =
            o.kv.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
        if kv != mkv {
            return Err(format!("{k:?}: kv differs"));
        }
    }
    Ok(bad)
}

/// One run: random batches on a device that may lose or misdirect one
/// write, a last list (the fence), then a block damaged on the media, and
/// a reopen. The fence is the one list whose loss is allowed: damage to
/// the last record looks like a write torn by a crash, and nothing after
/// it says otherwise. Any other acknowledged list lost is a failure.
fn damaged_run(seed: u64, damage: Damage, out: &mut Outcomes) {
    let mut rng = SmallRng::seed_from_u64(seed);
    PAGE.with(|p| p.set([None, Some(256), Some(2048)][seed as usize % 3]));
    let keys = keys();
    let blocks = 1024u64;
    let fd = Arc::new(FaultDev::new(blocks * BLOCK as u64));
    let d: Arc<dyn BlockDev> = fd.clone();
    format(
        &d,
        &DeviceConfig {
            device_id: 1,
            cluster_id: 2,
            shards: 1,
            log_blocks: Some(64),
            ..Default::default()
        },
    )
    .unwrap();
    let mut sh = open_shard(&d).unwrap();
    let region = sh.region();
    match damage {
        Damage::LostWrite => fd.misstep(rng.random_range(0..200), WriteFault::Lost),
        Damage::MisdirectedWrite => fd.misstep(
            rng.random_range(0..200),
            WriteFault::Misdirected(rng.random_range(0..blocks)),
        ),
        _ => {}
    }
    let mut model: Model = BTreeMap::new();
    for _ in 0..rng.random_range(4..30) {
        let batch: Vec<OpList> = (0..rng.random_range(1..6))
            .map(|_| random_list(&mut rng, &keys))
            .collect();
        let results = sh
            .commit(batch.clone())
            .unwrap_or_else(|e| panic!("seed {seed}: commit failed: {e}"));
        apply_results(seed, &mut model, &batch, results);
    }
    let before_fence = model.clone();
    let fence = OpList {
        key: ObjKey::new(1, 7, Namespace::Meta, b"fence".to_vec(), 0),
        ops: vec![Op::KvPut(b"f".to_vec(), seed.to_le_bytes().to_vec())],
    };
    let results = sh.commit(vec![fence.clone()]).unwrap();
    apply_results(seed, &mut model, &[fence], results);
    let mut img = fd.image();
    let struck = fd.struck();
    drop(sh);
    let hit = match damage {
        Damage::LogBlock => Some(region.start + 2 + rng.random_range(0..region.log_blocks)),
        Damage::ShardSuper => Some(region.start + rng.random_range(0..2)),
        Damage::DeviceSuper => Some(rng.random_range(0..2)),
        Damage::DataBlock => Some(
            region.start
                + 2
                + region.log_blocks
                + rng.random_range(0..region.blocks - 2 - region.log_blocks),
        ),
        Damage::LostWrite | Damage::MisdirectedWrite => None,
    };
    if let Some(b) = hit {
        let at = b as usize * BLOCK + rng.random_range(0..BLOCK);
        img[at] ^= rng.random_range(1..=255u8);
    }
    let fd = Arc::new(FaultDev::from_image(img));
    let d: Arc<dyn BlockDev> = fd.clone();
    let sh = match open_shard(&d) {
        Ok(sh) => sh,
        Err(_) => {
            out.refused += 1;
            return;
        }
    };
    let mut errors = Vec::new();
    for c in [&model, &before_fence] {
        match holds(&sh, c) {
            Ok(0) => {
                out.intact += 1;
                return;
            }
            Ok(_) => {
                out.detected += 1;
                return;
            }
            Err(e) => errors.push(e),
        }
    }
    if std::env::var("HS_CRASH_DEBUG").is_ok() {
        eprintln!(
            "silent: seed {seed} {damage:?} hit {hit:?} struck {struck:?} log {}..{}",
            region.start + 2,
            region.start + 2 + region.log_blocks
        );
    }
    out.silent.push(seed);
    out.first.get_or_insert_with(|| {
        format!(
            "seed {seed}: {damage:?} at block {hit:?}, writes {struck:?}: opened without acknowledged changes: {errors:#?}"
        )
    });
}

fn damaged_seeds(damage: Damage) -> Outcomes {
    let seeds: u64 = std::env::var("HS_CRASH_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let only: Option<u64> = std::env::var("HS_CRASH_SEED")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut out = Outcomes::default();
    for seed in only.map_or(0..seeds, |s| s..s + 1) {
        damaged_run(seed, damage, &mut out);
    }
    eprintln!(
        "{damage:?}: intact {}, detected {}, refused {}, silent {}",
        out.intact,
        out.detected,
        out.refused,
        out.silent.len()
    );
    if let Some(f) = &out.first {
        panic!(
            "{} of {damage:?} lost acknowledged changes silently ({:?}); {f}",
            out.silent.len(),
            out.silent
        );
    }
    out
}

#[test]
#[ignore = "#242: a damaged record drops those after it"]
fn damaged_log_blocks_never_lose_acknowledged_changes() {
    damaged_seeds(Damage::LogBlock);
}

#[test]
#[ignore = "#253: the older superblock opens the shard empty"]
fn a_damaged_shard_superblock_never_loses_acknowledged_changes() {
    damaged_seeds(Damage::ShardSuper);
}

#[test]
fn a_damaged_device_superblock_copy_is_read_past() {
    let out = damaged_seeds(Damage::DeviceSuper);
    assert_eq!(out.refused, 0, "the other copy serves");
}

#[test]
fn damaged_data_and_checkpoint_blocks_never_read_as_other_data() {
    damaged_seeds(Damage::DataBlock);
}

#[test]
#[ignore = "#242: a lost log write drops the records after it"]
fn a_lost_write_never_loses_acknowledged_changes_silently() {
    damaged_seeds(Damage::LostWrite);
}

#[test]
#[ignore = "#242: a misdirected log write drops the records after it"]
fn a_misdirected_write_never_loses_acknowledged_changes_silently() {
    damaged_seeds(Damage::MisdirectedWrite);
}

/// A damaged checkpoint page or directory refuses the whole shard today;
/// #243 recovers only the PGs in the page's key range instead.
#[test]
#[ignore = "#243: a damaged checkpoint page refuses the whole shard"]
fn a_damaged_checkpoint_page_never_refuses_the_shard() {
    let out = damaged_seeds(Damage::DataBlock);
    assert_eq!(out.refused, 0);
}
