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

fn cluster_suite(where_: &str, reactors: u32, secs: f64) {
    use hyperstor_daemon::*;
    use hyperstor_meta::{ClusterMap, replicated_pool};
    let meta_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let meta = meta_l.local_addr().unwrap();
    let mut map = ClusterMap::new(1);
    let mut listeners = Vec::new();
    for n in 0..3u32 {
        let ls: Vec<_> = (0..reactors)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let addrs: Vec<_> = ls.iter().map(|l| l.local_addr().unwrap()).collect();
        map.add_node(n, &addrs, 0);
        listeners.push(ls);
    }
    map.add_pool(replicated_pool(1, "vol", 3, 128));
    map.recompute();
    std::thread::spawn(move || {
        hyperstor_reactor::block_on(async move {
            let meta = hyperstor_meta::Meta::new(map);
            hyperstor_reactor::spawn(meta.clone().liveness(Duration::from_secs(3)));
            hyperstor_reactor::spawn(meta.clone().publisher());
            hyperstor_meta::serve(hyperstor_reactor::net::TcpListener::from_std(meta_l), meta).await
        })
    });
    let mut cpu = 2;
    let nodes: Vec<NodeHandle> = listeners
        .into_iter()
        .enumerate()
        .map(|(n, ls)| {
            let shards = ls
                .into_iter()
                .enumerate()
                .map(|(i, l)| {
                    let device = if where_ == "mem" {
                        DeviceSpec::Mem { bytes: 1 << 30 }
                    } else {
                        let path = Path::new(where_).join(format!("n{n}-s{i}.img"));
                        preallocate(&path, 1 << 30);
                        DeviceSpec::File {
                            path,
                            bytes: 1 << 30,
                            direct: true,
                            uring: true,
                        }
                    };
                    ShardSpec {
                        device_id: n as u32 * 1000 + i as u32,
                        device,
                        listener: l,
                    }
                })
                .collect();
            let cpus = (0..reactors as usize)
                .map(|_| {
                    cpu += 1;
                    cpu
                })
                .collect();
            start(NodeConfig {
                node_id: n as u32,
                meta,
                shards,
                cpus,
                meta_blocked: None,
                defer_max: None,
            })
        })
        .collect();
    let label = format!("3 nodes x {reactors} reactor(s), size 3, {where_}");
    client_workload(meta, &label, secs);
    for n in nodes {
        n.stop();
    }
}

/// Fills eight 64 MiB volumes in pool 1, then runs random and sequential
/// reads and writes against the cluster whose map service is at `meta`.
fn client_workload(meta: std::net::SocketAddr, label: &str, secs: f64) {
    use std::cell::RefCell;
    use std::rc::Rc;
    let label = label.to_string();
    hyperstor_reactor::block_on(async move {
        let _ = hyperstor_reactor::pin_to_cpu(1);
        let cl = hyperstor_client::Client::connect(meta, 1).await.unwrap();
        let vols = 8u128;
        let span = 64u64 << 20; // per volume
        // Fill so reads hit data.
        for v in 0..vols {
            for off in (0..span).step_by(4 << 20) {
                cl.write(1, v, off, vec![0x42; 4 << 20]).await.unwrap();
            }
        }
        for (name, write, len, qd) in [
            ("4 KiB random writes", true, 4096u64, 1usize),
            ("4 KiB random writes", true, 4096, 32),
            ("4 KiB random reads", false, 4096, 1),
            ("4 KiB random reads", false, 4096, 32),
            ("1 MiB writes", true, 1 << 20, 4),
            ("1 MiB reads", false, 1 << 20, 4),
        ] {
            let lats = Rc::new(RefCell::new(Vec::new()));
            let t = Instant::now();
            let hs: Vec<_> = (0..qd)
                .map(|w| {
                    let cl = cl.clone();
                    let lats = lats.clone();
                    hyperstor_reactor::spawn(async move {
                        let mut rng = SmallRng::seed_from_u64(w as u64);
                        let data = vec![0x17u8; len as usize];
                        while t.elapsed().as_secs_f64() < secs {
                            let v = rng.random_range(0..vols);
                            let off = rng.random_range(0..span / len) * len;
                            let s = Instant::now();
                            if write {
                                cl.write(1, v, off, data.clone()).await.unwrap();
                            } else {
                                std::hint::black_box(cl.read(1, v, off, len).await.unwrap());
                            }
                            lats.borrow_mut().push(s.elapsed());
                        }
                    })
                })
                .collect();
            for h in hs {
                h.await;
            }
            let elapsed = t.elapsed();
            let mut l = Lat(std::mem::take(&mut *lats.borrow_mut()));
            let n = l.0.len() as u64;
            row(&format!("{label}: {name}, QD{qd}"), n, elapsed, len, &mut l);
        }
    });
}

fn rss_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = s
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64
}

/// Index size and checkpoint cost for 1 GiB of 4 MiB objects, written
/// sequentially (one extent run per object), then overwritten block by
/// block in random order with every write redirected (one run per block,
/// the worst case for D38).
fn index_suite(path: &Path) {
    let gib = 1u64;
    let dev: Arc<dyn BlockDev> =
        Arc::new(FileDev::open(path, false, Some((gib + 1) << 30)).expect("open scratch file"));
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
    let mut sh = open(&dev).unwrap().1.pop().unwrap();
    sh.set_defer_max(0);
    let objects = (gib << 30) / (OBJ_BLOCKS * BLOCK as u64);
    let base = rss_bytes();
    let report = |sh: &mut Shard, label: &str, base: u64| {
        let t = Instant::now();
        sh.checkpoint().unwrap();
        let took = t.elapsed();
        let st = sh.stats();
        let rss = rss_bytes().saturating_sub(base);
        let per_tib = |bytes: u64| bytes as f64 * 1024.0 / (1u64 << 30) as f64;
        println!(
            "| {label} | {} | {:.1} | {:.1} | {:.0} | {:.1} | {:.1} |",
            st.runs,
            rss as f64 / (1 << 20) as f64,
            st.checkpoint_bytes as f64 / (1 << 20) as f64,
            took.as_secs_f64() * 1e3,
            per_tib(rss),
            per_tib(st.checkpoint_bytes),
        );
    };
    println!(
        "| 1 GiB of data | extent runs | index RAM MiB | checkpoint MiB | checkpoint ms | index RAM GiB per TiB | checkpoint GiB per TiB |"
    );
    println!("|---|---|---|---|---|---|---|");
    let chunk = vec![0x5au8; (OBJ_BLOCKS * BLOCK as u64) as usize];
    for o in 0..objects {
        sh.commit(vec![OpList {
            key: key(o),
            ops: vec![Op::Write {
                off: 0,
                data: chunk.clone(),
            }],
        }])
        .unwrap();
    }
    report(&mut sh, "written sequentially", base);
    let mut blocks: Vec<(u64, u64)> = (0..objects)
        .flat_map(|o| (0..OBJ_BLOCKS).map(move |b| (o, b)))
        .collect();
    let mut rng = SmallRng::seed_from_u64(3);
    for i in (1..blocks.len()).rev() {
        blocks.swap(i, rng.random_range(0..=i));
    }
    let data = vec![0xa5u8; BLOCK];
    for batch in blocks.chunks(64) {
        let lists = batch
            .iter()
            .map(|(o, b)| OpList {
                key: key(*o),
                ops: vec![Op::Write {
                    off: b * BLOCK as u64,
                    data: data.clone(),
                }],
            })
            .collect();
        sh.commit(lists).unwrap();
    }
    report(&mut sh, "every block overwritten (fully fragmented)", base);
    drop(sh);
    let _ = std::fs::remove_file(path);
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
        Some("cluster") => {
            // hyperstor-bench cluster mem|<dir> [reactors per node]
            let where_ = args.get(2).cloned().unwrap_or_else(|| "mem".into());
            let reactors: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);
            cluster_suite(&where_, reactors, secs);
        }
        Some("remote") => {
            // hyperstor-bench remote <map service address>
            let meta = args
                .get(2)
                .and_then(|a| a.parse().ok())
                .expect("map service address");
            header();
            client_workload(meta, &format!("remote {meta}"), secs);
        }
        Some("index") => {
            // hyperstor-bench index <file on a filesystem with 2 GiB free>
            index_suite(Path::new(args.get(2).expect("scratch file path")));
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
