//! M4: how long a shard takes to open today, phase by phase, and what the
//! on-device index's free-space bitmap would read instead of walking the
//! index (D412; 08-engine.md §7.6).
//!
//! Each case fills one shard, writes, drops it without closing (as a crash
//! would) and opens the device again, timing each phase
//! ([`hyperstor_engine::OpenTimes`]).

use super::{OBJ_BLOCKS, key};
use hyperstor_engine::*;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn device(gib: u64) -> Arc<dyn BlockDev> {
    // Twice the data, so redirect-on-write always finds space.
    let len = (2 * gib + 2) << 30;
    let dev: Arc<dyn BlockDev> = match std::env::var("HS_OPEN_DIR") {
        Ok(dir) => {
            let path = std::path::Path::new(&dir).join(format!("open-{gib}g.img"));
            let _ = std::fs::remove_file(&path);
            Arc::new(UringDev::open(&path, true, Some(len), 64).expect("open the file"))
        }
        Err(_) => Arc::new(MemDev::new(len)),
    };
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
    dev
}

fn write_whole(sh: &mut Shard, objects: u64) {
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
}

fn write_random(sh: &mut Shard, objects: u64, blocks: u64, rng: &mut SmallRng) {
    let mut done = 0;
    while done < blocks {
        let lists: Vec<OpList> = (0..64.min(blocks - done))
            .map(|_| OpList {
                key: key(rng.random_range(0..objects)),
                ops: vec![Op::Write {
                    off: rng.random_range(0..OBJ_BLOCKS) * BLOCK as u64,
                    data: vec![0xa5u8; BLOCK].into(),
                    crcs: None,
                }],
            })
            .collect();
        done += lists.len() as u64;
        sh.commit(lists).unwrap();
    }
}

fn ms(d: Duration) -> String {
    format!("{:.0}", d.as_secs_f64() * 1e3)
}

fn case(gib: u64, label: &str, random: bool, log_fraction: f64) {
    let dev = device(gib);
    let objects = (gib << 30) / (OBJ_BLOCKS * BLOCK as u64);
    let mut rng = SmallRng::seed_from_u64(gib);
    {
        let mut sh = open(&dev).unwrap().1.pop().unwrap();
        sh.set_defer_max(0);
        write_whole(&mut sh, objects);
        if random {
            write_random(&mut sh, objects, objects * OBJ_BLOCKS, &mut rng);
        }
        sh.checkpoint().unwrap();
        // Records the next open replays: one-block writes filling this
        // fraction of the log. A fuzzy checkpoint starts at a quarter, so
        // just under a quarter is the most a crash leaves between
        // checkpoints.
        let st = sh.stats();
        let want = (st.log_blocks as f64 * log_fraction) as u64;
        while sh.stats().log_used_blocks < want {
            write_random(&mut sh, objects, 64, &mut rng);
        }
    }
    let t = Instant::now();
    let sh = open(&dev).unwrap().1.pop().unwrap();
    let total = t.elapsed();
    let o = sh.open_times();
    let st = sh.stats();
    let ix = sh.index_stats();
    // D412: one bit per data block, read in one pass.
    let bitmap_kib = st.data_blocks.div_ceil(8) / 1024;
    println!(
        "| {gib} | {label} | {:.0} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {bitmap_kib} |",
        ix.runs as f64 / 1e3,
        ix.dense_objects,
        st.checkpoint_bytes as f64 / (1 << 20) as f64,
        o.records,
        ms(o.checkpoint),
        ms(o.scan),
        ms(o.replay),
        ms(o.allocator),
        ms(o.finish),
        ms(total),
    );
    if let Ok(dir) = std::env::var("HS_OPEN_DIR") {
        drop(sh);
        drop(dev);
        let _ = std::fs::remove_file(std::path::Path::new(&dir).join(format!("open-{gib}g.img")));
    }
}

pub fn suite() {
    let gibs: Vec<u64> = std::env::var("HS_GIBS")
        .unwrap_or_else(|_| "4,16,64".into())
        .split(',')
        .map(|s| s.trim().parse().expect("HS_GIBS: a list of GiB"))
        .collect();
    println!(
        "| GiB | case | runs (k) | dense objects | checkpoint MiB | records replayed | \
         checkpoint ms | log scan ms | replay ms | allocator ms | finish ms | open ms | \
         bitmap KiB |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for &gib in &gibs {
        case(gib, "sequential, clean", false, 0.0);
        case(gib, "random 4 KiB, clean", true, 0.0);
        case(gib, "random 4 KiB, log ¼ full", true, 0.24);
    }
}
