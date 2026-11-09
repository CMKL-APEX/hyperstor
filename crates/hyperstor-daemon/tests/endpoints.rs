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
use std::rc::Rc;
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

/// A running 3-node cluster whose devices also listen on endpoints of
/// network `fast` (127.0.0.2), with a pool of `copies`.
struct Rig {
    meta_addr: SocketAddr,
    /// Each device's endpoints, by node.
    endpoints: Vec<Vec<SocketAddr>>,
    handles: Vec<NodeHandle>,
    stop_tx: std::sync::mpsc::Sender<()>,
    meta_thread: std::thread::JoinHandle<()>,
}

impl Rig {
    /// `per` endpoints for each device, nodes on `fast` if `nodes`,
    /// pinging each other with `peer_timeout` (zero: not at all), cut by
    /// `partitions`.
    fn start(
        nodes: bool,
        copies: u8,
        per: u16,
        peer_timeout: Duration,
        partitions: Option<Arc<faults::Partitions>>,
    ) -> Rig {
        Rig::start_with(nodes, copies, per, peer_timeout, partitions, None)
    }

    /// [`Rig::start`], and with `fabric`, each node's endpoint i on
    /// 127.0.0.(2 + i), port `ep{i}` of its node for the fabric
    /// reconciler, which runs every 200 ms with that policy, and each
    /// node's `ep1` a `compute` port.
    fn start_with(
        nodes: bool,
        copies: u8,
        per: u16,
        peer_timeout: Duration,
        partitions: Option<Arc<faults::Partitions>>,
        fabric: Option<hyperstor_fabric::reconcile::Policy>,
    ) -> Rig {
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
        let mut endpoints = Vec::new();
        for n in 0..3u32 {
            let main = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let eps: Vec<std::net::TcpListener> = (0..per)
                .map(|i| {
                    let ip = if fabric.is_some() { 2 + i as u8 } else { 2 };
                    std::net::TcpListener::bind((std::net::Ipv4Addr::new(127, 0, 0, ip), 0))
                        .unwrap()
                })
                .collect();
            let addrs: Vec<SocketAddr> = eps.iter().map(|l| l.local_addr().unwrap()).collect();
            map.add_node(n, &[main.local_addr().unwrap()], 0);
            map.endpoints.insert(
                n * 1000,
                addrs.iter().map(|a| ("fast".to_string(), *a)).collect(),
            );
            endpoints.push(addrs);
            specs.push((n, main, eps));
        }
        if fabric.is_some() {
            for n in 0..3u32 {
                map.set_port_role(&format!("{n}/ep1"), Some(hyperstor_meta::PortRole::Compute))
                    .unwrap();
            }
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
                if let Some(p) = fabric {
                    meta.set_fabric(p, Duration::from_millis(200));
                }
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
        let handles: Vec<NodeHandle> = specs
            .into_iter()
            .map(|(n, main, eps)| {
                start(NodeConfig {
                    no_discard: Vec::new(),
                    endpoints: eps.into_iter().map(|l| (n * 1000, l)).collect(),
                    networks: networks(nodes),
                    push_part_bytes: 0,
                    stray_grace: Duration::ZERO,
                    io_timeout: Duration::ZERO,
                    read_timeout: Duration::ZERO,
                    perf_every: Duration::ZERO,
                    commit_delays: None,
                    pings: None,
                    older: None,
                    kv: Default::default(),
                    partitions: partitions.clone(),
                    node_id: n,
                    meta: vec![meta_addr],
                    shards: vec![ShardSpec {
                        device_id: n * 1000,
                        device: DeviceSpec::Existing(Arc::new(MemDev::new(64 << 20))),
                        listener: main,
                    }],
                    cpus: Vec::new(),
                    spin: Duration::ZERO,
                    peer_timeout,
                    meta_blocked: None,
                    defer_max: None,
                    scrub_interval: Duration::ZERO,
                    no_digests: false,
                })
            })
            .collect();
        Rig {
            meta_addr,
            endpoints,
            handles,
            stop_tx,
            meta_thread,
        }
    }

    fn stop(self) {
        for h in self.handles {
            h.stop();
        }
        let _ = self.stop_tx.send(());
        self.meta_thread.join().unwrap();
    }
}

fn networks(on: bool) -> Vec<String> {
    if on {
        vec!["fast".to_string()]
    } else {
        Vec::new()
    }
}

/// Waits up to 30 s for `ok` of the client's map, refreshing it.
async fn wait_map(cl: &Client, what: &str, ok: impl Fn(&ClusterMap) -> bool) {
    let t = std::time::Instant::now();
    loop {
        cl.refresh_map().await.unwrap();
        if ok(&cl.map()) {
            return;
        }
        assert!(
            t.elapsed().as_secs() < 30,
            "{what}: {:?}",
            cl.map().endpoints_down
        );
        hyperstor_reactor::time::sleep(Duration::from_millis(100)).await;
    }
}

/// [`run`] with `per` endpoints on `fast` for each device.
fn run_with(client: bool, nodes: bool, copies: u8, per: u16) -> (BTreeSet<u16>, BTreeSet<u16>) {
    let rig = Rig::start(nodes, copies, per, Duration::ZERO, None);
    let ports: BTreeSet<u16> = rig.endpoints.iter().flatten().map(|a| a.port()).collect();
    let meta_addr = rig.meta_addr;
    let seen = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta_addr, 7).await.unwrap();
            cl.set_networks(networks(client));
            wait_map(&cl, "nodes not up", |m| m.nodes.values().all(|n| n.up)).await;
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
    rig.stop();
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

/// Health per network (26-networks.md §29.7, D210): an endpoint no node
/// reaches is marked down while its node stays up, replication to the
/// node goes on over its other endpoint, and the endpoint comes back once
/// reachable. A node none of whose endpoints answers is down, though its
/// address still does: no data network reaches it.
#[test]
fn a_failed_endpoint_is_down_and_its_node_stays_up() {
    let cut = Arc::new(faults::Partitions::default());
    let rig = Rig::start(true, 3, 2, Duration::from_millis(300), Some(cut.clone()));
    let (meta_addr, eps) = (rig.meta_addr, rig.endpoints.clone());
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta_addr, 7).await.unwrap();
            wait_map(&cl, "nodes up", |m| m.nodes.values().all(|n| n.up)).await;
            let data = vec![5u8; 2 * CHUNK as usize];
            cl.write(POOL, 1, 0, data.clone()).await.unwrap();
            cut.cut_addr(eps[0][0], true);
            wait_map(&cl, "endpoint down", |m| {
                m.endpoints_down.contains(&eps[0][0])
            })
            .await;
            let m = cl.map();
            assert_eq!(m.endpoints_down.len(), 1, "{:?}", m.endpoints_down);
            assert!(m.nodes.values().all(|n| n.up), "node 0 stays up");
            assert_eq!(m.device_addrs_on(0, &networks(true)), vec![eps[0][1]]);
            // Three copies, so node 0 gets every write, over its other
            // endpoint.
            for v in 2..=6u128 {
                cl.write(POOL, v, 0, data.clone()).await.unwrap();
                assert_eq!(cl.read(POOL, v, 0, data.len() as u64).await.unwrap(), data);
            }
            cut.cut_addr(eps[0][0], false);
            wait_map(&cl, "endpoint up", |m| m.endpoints_down.is_empty()).await;
            // Both cut: node 0 is unreachable on the data network.
            cut.cut_addr(eps[0][0], true);
            cut.cut_addr(eps[0][1], true);
            wait_map(&cl, "node 0 down", |m| !m.nodes[&0].up).await;
            assert!(cl.map().nodes[&1].up && cl.map().nodes[&2].up);
            cut.heal_all();
            wait_map(&cl, "node 0 back", |m| {
                m.nodes.values().all(|n| n.up) && m.endpoints_down.is_empty()
            })
            .await;
            assert_eq!(cl.read(POOL, 1, 0, data.len() as u64).await.unwrap(), data);
        })
    })
    .join()
    .unwrap();
    rig.stop();
}

/// Fencing doesn't depend on the path (26-networks.md §29.11, D150): a
/// holder whose lease was taken over is refused on the device's endpoint
/// and on its address alike.
#[test]
fn a_fenced_holder_is_refused_on_every_network() {
    use hyperstor_client::ClientError;
    use hyperstor_proto::Code;
    let rig = Rig::start(true, 3, 1, Duration::ZERO, None);
    let meta_addr = rig.meta_addr;
    std::thread::spawn(move || {
        block_on(async move {
            let a = Client::connect(meta_addr, 1).await.unwrap();
            let b = Client::connect(meta_addr, 2).await.unwrap();
            a.set_networks(networks(true));
            wait_map(&a, "nodes up", |m| m.nodes.values().all(|n| n.up)).await;
            let v = a
                .create_volume(POOL, CHUNK as u64, None, "v")
                .await
                .unwrap();
            let va = a.attach(v.id).await.unwrap();
            va.write(0, vec![1; 4096]).await.unwrap();
            let lease = va.lease();
            drop(va);
            b.take_over(v.id).await.unwrap();
            for on in [true, false] {
                a.set_networks(networks(on));
                let e = a
                    .write_under(lease, POOL, v.lineage, 0, 0, 0, vec![2; 4096])
                    .await
                    .unwrap_err();
                assert!(
                    matches!(
                        e,
                        ClientError::Server {
                            code: Code::Fenced,
                            ..
                        }
                    ),
                    "on fast {on}: {e:?}"
                );
            }
        })
    })
    .join()
    .unwrap();
    rig.stop();
}

/// Bytes hyperstor's connections in this process sent and received from
/// `ip`.
fn own_bytes(ip: [u8; 4]) -> u64 {
    let ip = std::net::IpAddr::from(ip);
    hyperstor_rpc::traffic::totals()
        .into_iter()
        .filter(|(a, _, _)| *a == ip)
        .map(|(_, tx, rx)| tx + rx)
        .sum()
}

/// The fabric reconciler in the leader (26-networks.md §29.8; D208, D209,
/// D237, D238): nodes report their ports, a `compute` port idle for the
/// dwell is lent and senders spread over it; when other traffic appears
/// it is taken back, and senders leave it at once.
#[test]
fn a_compute_port_is_lent_while_idle_and_left_when_busy() {
    use hyperstor_fabric::reconcile::Policy;
    use hyperstor_proto::{Msg, PortSample};
    let policy = Policy {
        dwell_ms: 600,
        revoke_ms: 600,
        ..Policy::default()
    };
    let rig = Rig::start_with(true, 3, 2, Duration::ZERO, None, Some(policy));
    let meta_addr = rig.meta_addr;
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta_addr, 7).await.unwrap();
            cl.set_networks(networks(true));
            wait_map(&cl, "nodes up", |m| m.nodes.values().all(|n| n.up)).await;
            // Each node's two ports, as its report would give them: 100
            // Gb/s, with `other` bytes on ep1 since the last report.
            let busy = Rc::new(std::cell::Cell::new(false));
            let reporter = {
                let busy = busy.clone();
                spawn(async move {
                    let c = hyperstor_rpc::Conn::connect(meta_addr).await.unwrap();
                    let mut ep1 = 0u64;
                    loop {
                        if busy.get() {
                            ep1 += 2_000_000_000;
                        }
                        for node in 0..3 {
                            let port = |name: &str, ip: [u8; 4], tx: u64| PortSample {
                                name: name.into(),
                                up: true,
                                speed_mbps: 100_000,
                                addrs: vec![ip.into()],
                                tx_bytes: tx,
                                rx_bytes: 0,
                                own_tx: 0,
                                own_rx: 0,
                            };
                            let ports = vec![
                                port("ep0", [127, 0, 0, 2], 0),
                                port("ep1", [127, 0, 0, 3], ep1),
                            ];
                            let _ = c.call(&Msg::PortReport { node, ports }, &[]).await;
                        }
                        hyperstor_reactor::time::sleep(Duration::from_millis(100)).await;
                    }
                })
            };
            let ep = |i: usize| -> SocketAddr {
                let m = cl.map();
                m.endpoints[&0][i].1
            };
            wait_map(&cl, "ep1 lent", |m| {
                m.ports
                    .get("0/ep1")
                    .is_some_and(|p| p.lent && p.weight_mbs > 0)
            })
            .await;
            let m = cl.map();
            assert_eq!(m.port_role("0/ep0"), hyperstor_meta::PortRole::Storage);
            assert_eq!(m.endpoint_weight(ep(0)), Some(12.5));
            assert_eq!(m.endpoint_weight(ep(1)), Some(12.5));
            // Writes reach every node over both its endpoints.
            let before = own_bytes([127, 0, 0, 3]);
            let data = vec![7u8; 2 * CHUNK as usize];
            for v in 1..=8u128 {
                cl.write(POOL, v, 0, data.clone()).await.unwrap();
            }
            assert!(
                own_bytes([127, 0, 0, 3]) - before > 4 * CHUNK as u64,
                "lent: used"
            );

            // A job starts on ep1: taken back, weight 0 at once.
            busy.set(true);
            wait_map(&cl, "ep1 taken back", |m| {
                m.ports.get("0/ep1").is_some_and(|p| p.weight_mbs == 0)
            })
            .await;
            assert_eq!(
                cl.map().weighted_addrs_on(0, &networks(true)),
                vec![(ep(0), 12.5)]
            );
            // The nodes have the map too (pushed as it's published).
            hyperstor_reactor::time::sleep(Duration::from_millis(500)).await;
            let before = own_bytes([127, 0, 0, 3]);
            for v in 1..=8u128 {
                cl.write(POOL, v, 0, data.clone()).await.unwrap();
                assert_eq!(cl.read(POOL, v, 0, data.len() as u64).await.unwrap(), data);
            }
            assert!(
                own_bytes([127, 0, 0, 3]) - before < CHUNK as u64,
                "taken back: left, but for pings"
            );
            wait_map(&cl, "ep1 returned", |m| {
                m.ports
                    .get("0/ep1")
                    .is_some_and(|p| !p.lent && p.revoking.is_none())
            })
            .await;
            drop(reporter);
        })
    })
    .join()
    .unwrap();
    rig.stop();
}
