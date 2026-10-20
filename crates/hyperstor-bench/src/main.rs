//! `hyperstor-bench`: measurements for spec/benchmarks.md.
//!
//! ```text
//! hyperstor-bench engine <file> [size_gib]   engine on an O_DIRECT file (io_uring)
//! hyperstor-bench raw <file> [size_gib]      the same I/O patterns without the engine
//! hyperstor-bench mem                        engine on memory: CPU cost only
//! hyperstor-bench crc                        CRC32C throughput
//! ```
//!
//! Output is Markdown table rows. Files are preallocated so that writes
//! don't allocate filesystem blocks.

use hyperstor_engine::*;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const OBJ_BLOCKS: u64 = 1024; // 4 MiB objects, like volume chunks
const OBJECTS: u64 = 64;

fn preallocate(path: &Path, bytes: u64) {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .expect("open bench file");
    // SAFETY: plain syscall on an open descriptor.
    let r = unsafe { libc::posix_fallocate(f.as_raw_fd(), 0, bytes as libc::off_t) };
    assert_eq!(r, 0, "posix_fallocate failed: {r}");
    drop(f);
    // Preallocated extents are "unwritten": ext4 reads them as zeros without
    // touching the device, and the first write to each converts it in the
    // journal. Write the whole file once so every block is real.
    let dev = UringDev::open(path, true, None, 64).expect("open O_DIRECT");
    let mut buf = AlignedBuf::zeroed(4 << 20);
    buf.fill(0x11);
    let chunks: Vec<u64> = (0..bytes / (4 << 20)).collect();
    for group in chunks.chunks(16) {
        let io: Vec<(u64, &[u8])> = group.iter().map(|c| (c * (4 << 20), &buf[..])).collect();
        dev.write_batch(&io).unwrap();
    }
    dev.flush().unwrap();
}

struct Lat(Vec<Duration>);

impl Lat {
    fn pct(&mut self, p: f64) -> f64 {
        self.0.sort();
        let i = ((self.0.len() as f64 - 1.0) * p).round() as usize;
        self.0[i].as_secs_f64() * 1e6
    }
}

fn row(name: &str, ops: u64, elapsed: Duration, bytes_per_op: u64, lat: &mut Lat) {
    let s = elapsed.as_secs_f64();
    println!(
        "| {name} | {:.0} | {:.1} | {:.0} | {:.0} |",
        ops as f64 / s,
        (ops * bytes_per_op) as f64 / s / 1e6,
        lat.pct(0.5),
        lat.pct(0.99),
    );
}

fn header() {
    println!("| Test | ops/s | MB/s | p50 µs (per call) | p99 µs (per call) |");
    println!("|---|---|---|---|---|");
}

fn key(i: u64) -> ObjKey {
    ObjKey::new(
        1,
        (i % 128) as u32,
        Namespace::Vol,
        i.to_le_bytes().to_vec(),
        0,
    )
}

fn engine_suite(dev: Arc<dyn BlockDev>, label: &str, secs: f64) {
    format(
        &dev,
        &DeviceConfig {
            device_id: 1,
            cluster_id: 1,
            shards: 1,
            log_blocks: None,
        },
    )
    .unwrap();
    let (_, mut v) = open(&dev).unwrap();
    let mut sh = v.pop().unwrap();
    let defer: Option<usize> = std::env::var("HS_DEFER_MAX")
        .ok()
        .and_then(|s| s.parse().ok());
    if let Some(d) = defer {
        sh.set_defer_max(d);
    }
    let label = &format!("{label}, defer<{}", defer.unwrap_or(DEFER_MAX));
    let mut rng = SmallRng::seed_from_u64(1);
    let budget = Duration::from_secs_f64(secs);

    // Fill: 1 MiB sequential writes, batches of 8 (new extents).
    let mb = vec![0x5au8; 256 * BLOCK];
    let mut lat = Lat(Vec::new());
    let t = Instant::now();
    let mut ops = 0;
    'fill: for o in 0..OBJECTS {
        for c in 0..(OBJ_BLOCKS / 256) {
            let s = Instant::now();
            sh.commit(vec![OpList {
                key: key(o),
                ops: vec![Op::Write {
                    off: c * 256 * BLOCK as u64,
                    data: mb.clone(),
                }],
            }])
            .unwrap();
            lat.0.push(s.elapsed());
            ops += 1;
            if t.elapsed() > budget * 4 {
                break 'fill;
            }
        }
    }
    row(
        &format!("{label}: 1 MiB writes, new extents, QD1"),
        ops,
        t.elapsed(),
        256 * BLOCK as u64,
        &mut lat,
    );

    let filled = ops * 256; // blocks written
    let objs = (filled / OBJ_BLOCKS).max(1);
    for batch in [1usize, 8, 32, 128] {
        let data = vec![0xa5u8; BLOCK];
        let mut lat = Lat(Vec::new());
        let t = Instant::now();
        let mut ops = 0u64;
        while t.elapsed() < budget {
            let lists: Vec<OpList> = (0..batch)
                .map(|_| OpList {
                    key: key(rng.random_range(0..objs)),
                    ops: vec![Op::Write {
                        off: rng.random_range(0..OBJ_BLOCKS) * BLOCK as u64,
                        data: data.clone(),
                    }],
                })
                .collect();
            let s = Instant::now();
            sh.commit(lists).unwrap();
            lat.0.push(s.elapsed());
            ops += batch as u64;
            if sh.stats().pending_blocks > 4096 {
                sh.apply_deferred().unwrap();
            }
        }
        row(
            &format!("{label}: 4 KiB overwrites, batch {batch}"),
            ops,
            t.elapsed(),
            BLOCK as u64,
            &mut lat,
        );
    }
    sh.apply_deferred().unwrap();
    for batch in [1usize, 32] {
        let data = vec![0x3cu8; 16 * BLOCK];
        let mut lat = Lat(Vec::new());
        let t = Instant::now();
        let mut ops = 0u64;
        while t.elapsed() < budget {
            let lists: Vec<OpList> = (0..batch)
                .map(|_| OpList {
                    key: key(rng.random_range(0..objs)),
                    ops: vec![Op::Write {
                        off: rng.random_range(0..OBJ_BLOCKS / 16) * 16 * BLOCK as u64,
                        data: data.clone(),
                    }],
                })
                .collect();
            let s = Instant::now();
            sh.commit(lists).unwrap();
            lat.0.push(s.elapsed());
            ops += batch as u64;
        }
        row(
            &format!("{label}: 64 KiB overwrites, batch {batch}"),
            ops,
            t.elapsed(),
            16 * BLOCK as u64,
            &mut lat,
        );
    }
    for (name, len) in [("4 KiB", 1u64), ("128 KiB", 32), ("1 MiB", 256)] {
        let mut lat = Lat(Vec::new());
        let t = Instant::now();
        let mut ops = 0u64;
        while t.elapsed() < budget {
            let k = key(rng.random_range(0..objs));
            let off = rng.random_range(0..OBJ_BLOCKS / len) * len * BLOCK as u64;
            let s = Instant::now();
            let r = sh.read(&k, off, len * BLOCK as u64).unwrap();
            lat.0.push(s.elapsed());
            std::hint::black_box(&r);
            ops += 1;
        }
        row(
            &format!("{label}: {name} random reads, QD1"),
            ops,
            t.elapsed(),
            len * BLOCK as u64,
            &mut lat,
        );
    }
    let st = sh.stats();
    eprintln!("{label}: {st:?}");
}

fn raw_suite(dev: &UringDev, secs: f64) {
    let budget = Duration::from_secs_f64(secs);
    let blocks = dev.len() / BLOCK as u64;
    let mut rng = SmallRng::seed_from_u64(2);
    for batch in [1usize, 8, 32, 128] {
        let bufs: Vec<AlignedBuf> = (0..batch).map(|_| AlignedBuf::zeroed(BLOCK)).collect();
        let mut lat = Lat(Vec::new());
        let t = Instant::now();
        let mut ops = 0u64;
        while t.elapsed() < budget {
            let io: Vec<(u64, &[u8])> = bufs
                .iter()
                .map(|b| (rng.random_range(0..blocks) * BLOCK as u64, &b[..]))
                .collect();
            let s = Instant::now();
            dev.write_batch(&io).unwrap();
            dev.flush().unwrap();
            lat.0.push(s.elapsed());
            ops += batch as u64;
        }
        row(
            &format!("raw: 4 KiB random writes + fdatasync, batch {batch}"),
            ops,
            t.elapsed(),
            BLOCK as u64,
            &mut lat,
        );
    }
    for (name, len) in [("4 KiB", 1u64), ("128 KiB", 32), ("1 MiB", 256)] {
        let mut buf = AlignedBuf::zeroed(len as usize * BLOCK);
        let mut lat = Lat(Vec::new());
        let t = Instant::now();
        let mut ops = 0u64;
        while t.elapsed() < budget {
            let off = rng.random_range(0..blocks / len) * len * BLOCK as u64;
            let s = Instant::now();
            dev.read(off, &mut buf).unwrap();
            lat.0.push(s.elapsed());
            ops += 1;
        }
        row(
            &format!("raw: {name} random reads, QD1"),
            ops,
            t.elapsed(),
            len * BLOCK as u64,
            &mut lat,
        );
    }
    // Reads at depth: one batch of 32 random 4 KiB reads per call.
    let mut bufs: Vec<AlignedBuf> = (0..32).map(|_| AlignedBuf::zeroed(BLOCK)).collect();
    let mut lat = Lat(Vec::new());
    let t = Instant::now();
    let mut ops = 0u64;
    while t.elapsed() < budget {
        let mut io: Vec<(u64, &mut [u8])> = bufs
            .iter_mut()
            .map(|b| (rng.random_range(0..blocks) * BLOCK as u64, &mut b[..]))
            .collect();
        let s = Instant::now();
        dev.read_batch(&mut io).unwrap();
        lat.0.push(s.elapsed());
        ops += 32;
    }
    row(
        "raw: 4 KiB random reads, QD32 (one submit)",
        ops,
        t.elapsed(),
        BLOCK as u64,
        &mut lat,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let secs: f64 = std::env::var("HS_BENCH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);
    match args.get(1).map(String::as_str) {
        Some("engine") | Some("raw") => {
            let path = Path::new(args.get(2).expect("file path"));
            let gib: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);
            preallocate(path, gib << 30);
            let dev = UringDev::open(path, true, None, 256).expect("open O_DIRECT");
            header();
            if args[1] == "raw" {
                raw_suite(&dev, secs);
            } else {
                engine_suite(Arc::new(dev), "engine (uring, O_DIRECT)", secs);
            }
        }
        Some("mem") => {
            header();
            engine_suite(Arc::new(MemDev::new(1 << 30)), "engine (memory)", secs);
        }
        Some("placement") => {
            use hyperstor_placement::*;
            let mut devs = Vec::new();
            for i in 0..1024u32 {
                devs.push(Device {
                    id: i,
                    host: i / 8,
                    rack: i / 128,
                    weight: 1.0,
                    class: "nvme".into(),
                    up_in: true,
                });
            }
            for (domain, name) in [(Domain::Host, "host"), (Domain::Rack, "rack")] {
                let rule = Rule {
                    pool: 1,
                    size: 3,
                    domain,
                    class: None,
                };
                let topo = Topology::new(&rule, &devs);
                let t = Instant::now();
                let mut n = 0u32;
                while t.elapsed() < Duration::from_secs_f64(secs) {
                    std::hint::black_box(topo.place(n));
                    n += 1;
                }
                println!(
                    "placement, 1024 devices / 128 hosts / 8 racks, 3 copies by {name}: {:.0} PGs/s ({:.1} µs each)",
                    n as f64 / t.elapsed().as_secs_f64(),
                    t.elapsed().as_secs_f64() * 1e6 / n as f64
                );
            }
        }
        Some("crc") => {
            let data = vec![7u8; 1 << 20];
            let t = Instant::now();
            let mut n = 0u64;
            let mut acc = 0u32;
            while t.elapsed() < Duration::from_secs_f64(secs) {
                acc ^= hyperstor_codec::crc32c(&data);
                n += 1;
            }
            std::hint::black_box(acc);
            println!(
                "CRC32C: {:.2} GB/s",
                (n << 20) as f64 / t.elapsed().as_secs_f64() / 1e9
            );
        }
        _ => {
            eprintln!("usage: hyperstor-bench engine|raw <file> [size_gib] | mem | crc");
            std::process::exit(2);
        }
    }
}
