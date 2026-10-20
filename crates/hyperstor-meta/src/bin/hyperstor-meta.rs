//! `hyperstor-meta --listen ADDR --cluster FILE [--lease-ms L] [--down-after-ms N]`
//!
//! Serves the cluster map described by FILE (see
//! `hyperstor_meta::parse_cluster`), grants map leases of L ms (default
//! 2000), marks nodes down after N ms without a heartbeat (default 3000,
//! at least 1.25 × L), and moves their PGs, fencing the nodes that lose a
//! role before publishing (decisions.md D42).

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(listen), Some(file)) = (get("--listen"), get("--cluster")) else {
        eprintln!(
            "usage: hyperstor-meta --listen ADDR --cluster FILE [--lease-ms L] [--down-after-ms N]"
        );
        std::process::exit(2);
    };
    let lease: u64 = get("--lease-ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let down: u64 = get("--down-after-ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    if down * 4 < lease * 5 {
        eprintln!("--down-after-ms must be at least 1.25 x --lease-ms");
        std::process::exit(2);
    }
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| {
        eprintln!("{file}: {e}");
        std::process::exit(1)
    });
    let map = hyperstor_meta::parse_cluster(&text).unwrap_or_else(|e| {
        eprintln!("{file}: {e}");
        std::process::exit(1)
    });
    eprintln!(
        "hyperstor-meta: {} nodes, {} devices, {} pools, {} PGs, epoch {}, lease {lease} ms",
        map.nodes.len(),
        map.devices.len(),
        map.pools.len(),
        map.pgs.len(),
        map.epoch
    );
    let addr = listen.parse().expect("--listen address");
    hyperstor_reactor::block_on(async move {
        let l = hyperstor_reactor::net::TcpListener::bind(addr).expect("bind");
        let meta = hyperstor_meta::Meta::with_lease(map, Duration::from_millis(lease));
        hyperstor_reactor::spawn(meta.clone().liveness(Duration::from_millis(down)));
        hyperstor_reactor::spawn(meta.clone().publisher());
        hyperstor_meta::serve(l, meta).await
    });
}
