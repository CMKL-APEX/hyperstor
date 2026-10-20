//! `hyperstord --node N --meta ADDR --shard ADDR=PATH:SIZE_GIB [--shard ...]
//!            [--cpus 2,3] [--buffered] [--hdd]`
//!
//! Runs storage node N: one reactor per `--shard`, listening at ADDR (as in
//! the cluster file) and storing on PATH (a file, created and extended to
//! SIZE_GIB if needed, or a block device), opened `O_DIRECT` through
//! io_uring unless `--buffered`. Formats unformatted devices. Stops on
//! SIGINT or SIGTERM by exiting; the engine recovers on the next start.
//! `--hdd` marks the devices as hard disks, which defer small overwrites
//! through the log (decisions.md D38); flash redirects every write.

use hyperstor_daemon::*;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let usage = || -> ! {
        eprintln!(
            "usage: hyperstord --node N --meta ADDR --shard ADDR=PATH:SIZE_GIB [--shard ...] [--cpus 2,3] [--buffered] [--hdd]"
        );
        std::process::exit(2)
    };
    let node: u32 = get("--node")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| usage());
    let meta = get("--meta")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| usage());
    let direct = !args.iter().any(|a| a == "--buffered");
    let cpus: Vec<usize> = get("--cpus")
        .map(|s| s.split(',').filter_map(|c| c.parse().ok()).collect())
        .unwrap_or_default();
    let mut shards = Vec::new();
    for (i, w) in args.windows(2).enumerate() {
        if w[0] != "--shard" {
            continue;
        }
        let (addr, rest) = w[1].split_once('=').unwrap_or_else(|| usage());
        let (path, gib) = rest.rsplit_once(':').unwrap_or_else(|| usage());
        let gib: u64 = gib.parse().unwrap_or_else(|_| usage());
        let listener = std::net::TcpListener::bind(addr).unwrap_or_else(|e| {
            eprintln!("bind {addr}: {e}");
            std::process::exit(1)
        });
        shards.push(ShardSpec {
            device_id: node * 1000 + shards.len() as u32,
            device: DeviceSpec::File {
                path: PathBuf::from(path),
                bytes: gib << 30,
                direct,
                uring: true,
            },
            listener,
        });
        let _ = i;
    }
    if shards.is_empty() {
        usage();
    }
    eprintln!(
        "hyperstord: node {node}, {} shard(s), map service {meta}",
        shards.len()
    );
    let h = start(NodeConfig {
        node_id: node,
        meta,
        shards,
        cpus,
        meta_blocked: None,
        defer_max: args
            .iter()
            .any(|a| a == "--hdd")
            .then_some(hyperstor_engine::DEFER_MAX),
    });
    // Run until killed; a reactor that fails to start ends the process.
    for r in h.stop_when_any_fails() {
        if let Err(e) = r {
            eprintln!("hyperstord: reactor failed: {e}");
            std::process::exit(1);
        }
    }
}
