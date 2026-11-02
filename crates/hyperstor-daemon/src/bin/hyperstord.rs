//! `hyperstord --node N --meta ADDR --shard [ID@]ADDR=PATH:SIZE_GIB [--shard ...]
//!            [--cpus 2,3] [--buffered] [--hdd] [--spin-us N] [--peer-timeout-ms N]
//!            [--admin ADDR] [--io-timeout-secs N] [--read-timeout-ms N]`
//!
//! Runs storage node N: one reactor per `--shard`, listening at ADDR (as in
//! the cluster file) as device ID (default `N * 1000 +` its position among
//! the `--shard` options; D169), which the map must have on node N at ADDR,
//! and storing on PATH (a file, created and extended to
//! SIZE_GIB if needed, or a block device), opened `O_DIRECT` through
//! io_uring unless `--buffered`. Formats unformatted devices. Stops on
//! SIGINT or SIGTERM by exiting; the engine recovers on the next start.
//! `--spin-us N` sets the reactors' adaptive polling window (default 200;
//! 0 sleeps in the kernel whenever idle, 04-decisions.md D41 and D49).
//! `--peer-timeout-ms N` reports a peer that doesn't answer pings for N ms
//! to the map service (default 300; 0 turns it off, 04-decisions.md D40 and
//! D48).
//! `--admin ADDR` serves metrics in Prometheus format (`GET /metrics`) and
//! a liveness answer (`GET /health`) there, from a thread of its own
//! (19-administration.md §23.3).
//! `--scrub-hours N` compares the copies of each PG this node is primary
//! of every N hours (default 24; 0 turns scrub off; 09-replication.md §8.5).
//! `--io-timeout-secs N` fails a device whose commit (its writes and
//! flush) takes longer than N seconds (default 30, 04-decisions.md D166).
//! `--read-timeout-ms N`: a read that waits longer for its device is
//! served from a member, and three in a row fail the device (default 2000,
//! 04-decisions.md D190).
//! `--hdd` marks the devices as hard disks, which defer small overwrites
//! through the log (04-decisions.md D38); flash redirects every write.

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
            "usage: hyperstord --node N --meta ADDR --shard [ID@]ADDR=PATH:SIZE_GIB [--shard ...] [--cpus 2,3] [--buffered] [--hdd] [--admin ADDR] [--scrub-hours N] [--io-timeout-secs N] [--read-timeout-ms N] [--endpoint DEV=ADDR ...] [--networks NAME[,NAME...]]"
        );
        std::process::exit(2)
    };
    let node: u32 = get("--node")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| usage());
    // One map service member, or several separated by commas (D72).
    let meta: Vec<std::net::SocketAddr> = get("--meta")
        .map(|s| {
            s.split(',')
                .map(|a| a.parse().unwrap_or_else(|_| usage()))
                .collect()
        })
        .filter(|v: &Vec<_>| !v.is_empty())
        .unwrap_or_else(|| usage());
    let direct = !args.iter().any(|a| a == "--buffered");
    let cpus: Vec<usize> = get("--cpus")
        .map(|s| s.split(',').filter_map(|c| c.parse().ok()).collect())
        .unwrap_or_default();
    let mut shards = Vec::new();
    // Device ids as named (`ID@`), else by position (D169).
    let mut named = Vec::new();
    for (i, w) in args.windows(2).enumerate() {
        if w[0] != "--shard" {
            continue;
        }
        let (addr, rest) = w[1].split_once('=').unwrap_or_else(|| usage());
        let (id, addr) = hyperstor_meta::split_device_id(addr).unwrap_or_else(|e| fail_args(&e));
        named.push(id);
        let (path, gib) = rest.rsplit_once(':').unwrap_or_else(|| usage());
        let gib: u64 = gib.parse().unwrap_or_else(|_| usage());
        // Served by a reactor: wait a moment if a previous instance's
        // still holds it (hyperstor_reactor::net::bind_retrying).
        let addr: std::net::SocketAddr = addr.parse().unwrap_or_else(|_| usage());
        let listener = hyperstor_reactor::net::bind_retrying(addr).unwrap_or_else(|e| {
            eprintln!("bind {addr}: {e}");
            std::process::exit(1)
        });
        shards.push(ShardSpec {
            // Set below, once every --shard is read.
            device_id: 0,
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
    let ids = hyperstor_meta::device_ids(node, &named).unwrap_or_else(|e| fail_args(&e));
    for (s, id) in shards.iter_mut().zip(ids) {
        s.device_id = id;
    }
    // D166: how long a commit's writes and flush may take before the
    // device is failed.
    let io_timeout = std::time::Duration::from_secs(
        get("--io-timeout-secs")
            .map(|s| {
                s.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .unwrap_or_else(|| usage())
            })
            .unwrap_or(hyperstor_daemon::IO_TIMEOUT.as_secs()),
    );
    // D190: how long a read may wait for the device.
    let read_timeout = std::time::Duration::from_millis(
        get("--read-timeout-ms")
            .map(|s| {
                s.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .unwrap_or_else(|| usage())
            })
            .unwrap_or(hyperstor_daemon::READ_TIMEOUT.as_millis() as u64),
    );
    // More addresses a device's reactor listens on, one per endpoint in
    // the map (26-networks.md §29.5, D207).
    let mut endpoints = Vec::new();
    for w in args.windows(2).filter(|w| w[0] == "--endpoint") {
        let (dev, addr) = w[1].split_once('=').unwrap_or_else(|| usage());
        let dev: u32 = dev.parse().unwrap_or_else(|_| usage());
        if !shards.iter().any(|s| s.device_id == dev) {
            fail_args(&format!("--endpoint {dev}: not one of this node's devices"));
        }
        let addr: std::net::SocketAddr = addr.parse().unwrap_or_else(|_| usage());
        let l = hyperstor_reactor::net::bind_retrying(addr).unwrap_or_else(|e| {
            eprintln!("bind {addr}: {e}");
            std::process::exit(1)
        });
        endpoints.push((dev, l));
    }
    // The data networks to reach peers on, in order (D206).
    let networks: Vec<String> = get("--networks")
        .map(|s| {
            s.split(',')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let admin = get("--admin").map(|a| {
        std::net::TcpListener::bind(&a).unwrap_or_else(|e| {
            eprintln!("bind {a}: {e}");
            std::process::exit(1)
        })
    });
    eprintln!(
        "hyperstord: node {node}, {} shard(s), map service {}",
        shards.len(),
        meta.iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let h = start(NodeConfig {
        endpoints,
        networks,
        push_part_bytes: 0,
        stray_grace: std::time::Duration::ZERO,
        io_timeout,
        read_timeout,
        commit_delays: None,
        pings: None,
        partitions: None,
        node_id: node,
        meta,
        shards,
        cpus,
        meta_blocked: None,
        spin: std::time::Duration::from_micros(
            get("--spin-us").and_then(|s| s.parse().ok()).unwrap_or(200),
        ),
        peer_timeout: std::time::Duration::from_millis(
            get("--peer-timeout-ms")
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
        ),
        defer_max: args
            .iter()
            .any(|a| a == "--hdd")
            .then_some(hyperstor_engine::DEFER_MAX),
        scrub_interval: std::time::Duration::from_secs(
            get("--scrub-hours")
                .map(|s| s.parse::<u64>().unwrap_or_else(|_| usage()))
                .unwrap_or(24)
                * 3600,
        ),
        no_digests: false,
    });
    if let Some(l) = admin {
        hyperstor_daemon::metrics::serve(l, node, h.metrics.clone());
    }
    // Run until killed; a reactor that fails to start ends the process.
    for r in h.stop_when_any_fails() {
        if let Err(e) = r {
            eprintln!("hyperstord: reactor failed: {e}");
            std::process::exit(1);
        }
    }
}

/// A `--shard` the node can't use: says why and exits as `usage` does.
fn fail_args(why: &str) -> ! {
    eprintln!("hyperstord: --shard: {why}");
    std::process::exit(2)
}
