//! Devices served on endpoints of a second network (26-networks.md §29.5,
//! D207): each device's reactor also listens on its endpoint, and nodes
//! and clients on that network reach it there, for I/O and replication.

use hyperstor_client::Client;
use hyperstor_daemon::*;
use hyperstor_engine::MemDev;
use hyperstor_meta::{ClusterMap, NetKind, NetworkDef, replicated_pool};
use hyperstor_reactor::{block_on, spawn};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const POOL: u32 = 1;
const CHUNK: u32 = 1 << 20;

/// The local ports of established TCP connections on `ip` (IPv4), from
/// /proc/net/tcp: the endpoints' side of every session to them.
fn established_on(ip: [u8; 4]) -> BTreeSet<u16> {
    let want = format!("{:08X}", u32::from_le_bytes(ip));
    std::fs::read_to_string("/proc/net/tcp")
        .unwrap()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (addr, port) = f.get(1)?.split_once(':')?;
            // 01: ESTABLISHED.
            (addr == want && f.get(3) == Some(&"01")).then(|| u16::from_str_radix(port, 16).ok())?
        })
        .collect()
}

/// Runs a 3-node cluster whose devices also listen on endpoints of
/// network `fast` (127.0.0.2), writes and reads 8 volumes in a pool of
/// `copies`, and returns the endpoints' ports and those with established
/// sessions afterwards. The client is on `fast` if `client`, the nodes if
/// `nodes`.
fn run(client: bool, nodes: bool, copies: u8) -> (BTreeSet<u16>, BTreeSet<u16>) {
    run_with(client, nodes, copies, 1)
}

/// [`run`] with `per` endpoints on `fast` for each device.
fn run_with(client: bool, nodes: bool, copies: u8, per: u16) -> (BTreeSet<u16>, BTreeSet<u16>) {
    let meta_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let meta_addr = meta_l.local_addr().unwrap();
    let mut map = ClusterMap::new(1);
    map.networks.insert(
        "fast".into(),
        NetworkDef {
            kind: NetKind::Data,
            trusted: true,
            matches: "subnet 127.0.0.2/32".into(),
        },
    );
    let mut specs = Vec::new();
    let mut ports = BTreeSet::new();
    for n in 0..3u32 {
        let main = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let eps: Vec<std::net::TcpListener> = (0..per)
            .map(|_| std::net::TcpListener::bind("127.0.0.2:0").unwrap())
            .collect();
        let addrs: Vec<SocketAddr> = eps.iter().map(|l| l.local_addr().unwrap()).collect();
        ports.extend(addrs.iter().map(|a| a.port()));
        map.add_node(n, &[main.local_addr().unwrap()], 0);
        map.endpoints.insert(
            n * 1000,
            addrs.iter().map(|a| ("fast".to_string(), *a)).collect(),
        );
        specs.push((n, main, eps));
    }
    let mut pool = replicated_pool(POOL, "vol", copies, 16);
    pool.min_size = 1;
    pool.chunk_size = CHUNK;
    pool.autoscale = false;
    map.add_pool(pool);
    map.ensure_meta_pool();
    map.recompute();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let meta_thread = std::thread::spawn(move || {
        block_on(async move {
            let meta = hyperstor_meta::Meta::with_lease(map, Duration::from_millis(400));
            spawn(hyperstor_meta::serve(
                hyperstor_reactor::net::TcpListener::from_std(meta_l),
                meta.clone(),
            ));
            spawn(meta.clone().liveness(Duration::from_millis(700)));
            spawn(meta.clone().publisher());
            while stop_rx.try_recv().is_err() {
                hyperstor_reactor::time::sleep(Duration::from_millis(50)).await;
            }
        })
    });
    let networks = |on: bool| {
        if on {
            vec!["fast".to_string()]
        } else {
            Vec::new()
        }
    };
    let handles: Vec<NodeHandle> = specs
        .into_iter()
        .map(|(n, main, eps)| {
            start(NodeConfig {
                endpoints: eps.into_iter().map(|l| (n * 1000, l)).collect(),
                networks: networks(nodes),
                push_part_bytes: 0,
                stray_grace: Duration::ZERO,
                io_timeout: Duration::ZERO,
                read_timeout: Duration::ZERO,
                commit_delays: None,
                pings: None,
                partitions: None,
                node_id: n,
                meta: vec![meta_addr],
                shards: vec![ShardSpec {
                    device_id: n * 1000,
                    device: DeviceSpec::Existing(Arc::new(MemDev::new(64 << 20))),
                    listener: main,
                }],
                cpus: Vec::new(),
                spin: Duration::ZERO,
                peer_timeout: Duration::ZERO,
                meta_blocked: None,
                defer_max: None,
                scrub_interval: Duration::ZERO,
                no_digests: false,
            })
        })
        .collect();
    let seen = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta_addr, 7).await.unwrap();
            cl.set_networks(networks(client));
            let t = std::time::Instant::now();
            while cl.map().nodes.values().any(|n| !n.up) {
                assert!(t.elapsed().as_secs() < 30, "nodes not up");
                hyperstor_reactor::time::sleep(Duration::from_millis(100)).await;
                cl.refresh_map().await.unwrap();
            }
            // 32 chunks over 16 PGs: every device leads some. Then small
            // writes at once, so a device's endpoints share the load.
            for v in 1..=16u128 {
                let data = vec![v as u8; 2 * CHUNK as usize];
                cl.write(POOL, v, 0, data.clone()).await.unwrap();
                assert_eq!(cl.read(POOL, v, 0, data.len() as u64).await.unwrap(), data);
            }
            let hs: Vec<_> = (0..64u64)
                .map(|i| {
                    let cl = cl.clone();
                    spawn(async move { cl.write(POOL, 9, i * 4096, vec![1; 4096]).await.unwrap() })
                })
                .collect();
            for h in hs {
                h.await;
            }
            established_on([127, 0, 0, 2])
        })
    })
    .join()
    .unwrap();
    for h in handles {
        h.stop();
    }
    let _ = stop_tx.send(());
    meta_thread.join().unwrap();
    (ports, seen)
}

/// A client on the network reaches every primary at its endpoint: one
/// copy, so no node sends to another.
#[test]
fn a_client_on_a_network_uses_its_endpoints() {
    let (ports, seen) = run(true, false, 1);
    assert!(ports.is_subset(&seen), "{ports:?} not all in {seen:?}");
}

/// Nodes on the network replicate to each other's endpoints, the client
/// on none.
#[test]
fn nodes_on_a_network_replicate_over_its_endpoints() {
    let (ports, seen) = run(false, true, 3);
    assert!(ports.is_subset(&seen), "{ports:?} not all in {seen:?}");
}

/// A device with two endpoints on the network: requests to it go over
/// both, chosen per request (D208).
#[test]
fn a_devices_endpoints_share_its_requests() {
    let (ports, seen) = run_with(true, true, 3, 2);
    assert_eq!(ports.len(), 6);
    assert!(ports.is_subset(&seen), "{ports:?} not all in {seen:?}");
}

/// The control case: with nobody on the network, nothing reaches the
/// endpoints, though the devices listen on them.
#[test]
fn nobody_on_the_network_leaves_its_endpoints_idle() {
    let (ports, seen) = run(false, false, 3);
    assert!(ports.is_disjoint(&seen), "{ports:?} in {seen:?}");
}

/// A client on no network, or on one a device has no endpoint on, uses
/// the device's address.
#[test]
fn without_a_shared_network_the_address_is_used() {
    let mut map = ClusterMap::new(1);
    let a: SocketAddr = "127.0.0.1:9100".parse().unwrap();
    let e: SocketAddr = "127.0.0.2:9100".parse().unwrap();
    map.add_node(0, &[a], 0);
    map.endpoints.insert(0, vec![("fast".into(), e)]);
    assert_eq!(map.device_addr_on(0, &["fast".into()]), Some(e));
    assert_eq!(
        map.device_addr_on(0, &["slow".into(), "fast".into()]),
        Some(e)
    );
    assert_eq!(map.device_addr_on(0, &["slow".into()]), Some(a));
    assert_eq!(map.device_addr_on(0, &[]), Some(a));
}
