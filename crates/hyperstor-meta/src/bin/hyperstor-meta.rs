//! `hyperstor-meta --listen ADDR --cluster FILE [--api ADDR] [--api-tls DIR] [--api-socket PATH] [--lease-ms L] [--down-after-ms N]`
//!
//! Serves the cluster map described by FILE (see
//! `hyperstor_meta::parse_cluster`), grants map leases of L ms (default
//! 600), marks nodes down after N ms without a heartbeat (default 800,
//! at least 1.25 × L; decisions.md D48), and moves their PGs, fencing the
//! nodes that lose a role before publishing (decisions.md D42). With
//! `--api ADDR`, also serves the cluster API (`hyperstor.v1`;
//! spec/administration.md §23.5) there: over mutual TLS with
//! `--api-tls DIR` (`api.crt`, `api.key`, `root.crt`; §23.8), else plain
//! HTTP for loopback and tests. `--api-socket PATH` also serves it on a
//! local Unix socket, for root, the service's user and the
//! `hyperstor-admin` group, without certificates (break-glass, §23.6).

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
            "usage: hyperstor-meta --listen ADDR --cluster FILE [--api ADDR] [--api-tls DIR] [--api-socket PATH] [--lease-ms L] [--down-after-ms N]"
        );
        std::process::exit(2);
    };
    let lease: u64 = get("--lease-ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let down: u64 = get("--down-after-ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(800);
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
    let addr: std::net::SocketAddr = listen.parse().unwrap_or_else(|_| {
        eprintln!("--listen: not an address: {listen}");
        std::process::exit(2)
    });
    // Bound before anything starts, so a taken port fails at once.
    let api = get("--api").map(|a| {
        std::net::TcpListener::bind(&a).unwrap_or_else(|e| {
            eprintln!("--api {a}: {e}");
            std::process::exit(1)
        })
    });
    let tls = get("--api-tls").map(|d| {
        hyperstor_meta::api::tls_from_dir(std::path::Path::new(&d), map.cluster_id).unwrap_or_else(
            |e| {
                eprintln!("--api-tls: {e}");
                std::process::exit(1)
            },
        )
    });
    let socket = get("--api-socket").map(|p| {
        let _ = std::fs::remove_file(&p);
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap_or_else(|e| {
            eprintln!("--api-socket {p}: {e}");
            std::process::exit(1)
        });
        // Anyone may connect; each request is checked by the caller's uid.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666));
        l
    });
    if socket.is_some() && api.is_none() {
        eprintln!("--api-socket needs --api");
        std::process::exit(2);
    }
    if api.is_some() && tls.is_none() {
        eprintln!(
            "hyperstor-meta: the cluster API has no TLS and no authentication: loopback only"
        );
    }
    hyperstor_reactor::block_on(async move {
        let l = hyperstor_reactor::net::TcpListener::bind(addr).expect("bind");
        let meta = hyperstor_meta::Meta::with_lease(map, Duration::from_millis(lease));
        hyperstor_reactor::spawn(meta.clone().liveness(Duration::from_millis(down)));
        hyperstor_reactor::spawn(meta.clone().publisher());
        if let Some(api) = api {
            let (tx, rx) = hyperstor_reactor::chan::channel();
            hyperstor_reactor::spawn(meta.clone().admin(rx));
            eprintln!(
                "hyperstor-meta: cluster API on {}",
                api.local_addr().expect("address")
            );
            hyperstor_meta::api::spawn(api, socket, tx, addr, tls);
        }
        hyperstor_meta::serve(l, meta).await
    });
}
