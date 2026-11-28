//! Key–value entries per PG on a copy of a device (point 375, G15): how
//! many keys a PG's metadata tree would hold. Opens the copy offline
//! (opening writes a checkpoint, so never point it at a live device) and
//! prints counts only, never keys or values.

use hyperstor_engine::*;
use std::collections::BTreeMap;
use std::sync::Arc;

pub fn suite(path: &str) {
    let dev: Arc<dyn BlockDev> =
        Arc::new(FileDev::open(std::path::Path::new(path), true, None).expect("open the copy"));
    let (_, shards) = open(&dev).expect("open the device");
    // (pool) -> per PG: (objects, kv entries, kv bytes, attrs, largest object's entries)
    let mut pools: BTreeMap<u32, BTreeMap<u32, [u64; 5]>> = BTreeMap::new();
    for sh in &shards {
        for (k, o) in &sh.index().objects {
            let e = pools.entry(k.pool).or_default().entry(k.pg).or_default();
            e[0] += 1;
            e[1] += o.kv.len() as u64;
            e[2] += o.kv_bytes;
            e[3] += o.attrs.len() as u64;
            e[4] = e[4].max(o.kv.len() as u64);
        }
    }
    println!(
        "| Pool | PGs here | Objects | KV entries | KV MiB | Entries per PG: p50 / max | \
         Largest object's entries | Attributes |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for (pool, pgs) in &pools {
        let mut per: Vec<u64> = pgs.values().map(|e| e[1]).collect();
        per.sort_unstable();
        let sum = |i: usize| pgs.values().map(|e| e[i]).sum::<u64>();
        println!(
            "| {pool} | {} | {} | {} | {:.1} | {} / {} | {} | {} |",
            pgs.len(),
            sum(0),
            sum(1),
            sum(2) as f64 / (1 << 20) as f64,
            per[per.len() / 2],
            per.last().unwrap(),
            pgs.values().map(|e| e[4]).max().unwrap(),
            sum(3),
        );
    }
}
