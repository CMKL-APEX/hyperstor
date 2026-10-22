//! A cluster in one process: a map service and storage nodes on threads,
//! each reactor with an in-memory device, and a client on its own reactor.

use hyperstor_client::{Client, chunk_name};
use hyperstor_daemon::*;
use hyperstor_engine::{BlockDev, MemDev, Namespace, ObjKey};
use hyperstor_meta::{ClusterMap, NO_DEVICE, replicated_pool};
use hyperstor_placement::pg_of;
use hyperstor_reactor::chan::{Sender, channel};
use hyperstor_reactor::{block_on, spawn};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::net::SocketAddr;
use std::sync::Arc;

const POOL: u32 = 1;
const CACHE_POOL: u32 = 2;
const CHUNK: u32 = 1 << 20;
/// Short leases and detection, to keep the tests quick.
const LEASE: std::time::Duration = std::time::Duration::from_millis(400);
const DOWN_AFTER: std::time::Duration = std::time::Duration::from_millis(700);
/// The down-out interval (placement.md §5.3): short, so a stopped node's
/// PGs are placed elsewhere soon; tests of the interval set their own.
const DOWN_OUT: std::time::Duration = std::time::Duration::from_secs(1);

enum MetaCmd {
    Stop,
    /// The PGs scrub reported inconsistent.
    Inconsistent(std::sync::mpsc::Sender<hyperstor_meta::Inconsistent>),
}

/// Each stopped device, by device id.
type Devices = Vec<(u32, Arc<dyn BlockDev>)>;

struct Cluster {
    /// Peer timeout given to restarted nodes.
    peer_timeout: std::time::Duration,
    /// Scrub interval of every node; zero: off.
    scrub: std::time::Duration,
    meta_addr: SocketAddr,
    /// Each node's reactor addresses, to restart it at the same place.
    addrs: Vec<Vec<SocketAddr>>,
    /// Per node: set to cut it off from the map service (heartbeats stop)
    /// while clients and peers can still reach it.
    blocked: Vec<Arc<std::sync::atomic::AtomicBool>>,
    /// Devices of stopped nodes, by node.
    stopped: std::collections::HashMap<usize, Vec<Arc<dyn BlockDev>>>,
    meta_tx: Sender<MetaCmd>,
    meta_thread: Option<std::thread::JoinHandle<ClusterMap>>,
    nodes: Vec<Option<NodeHandle>>,
    /// Node pairs cut from each other (D92), shared by every node.
    partitions: Arc<hyperstor_daemon::faults::Partitions>,
    /// Every device, by id, to make it slow or fail (D92).
    faulty: std::collections::HashMap<u32, Arc<hyperstor_daemon::faults::FaultyDev>>,
}

impl Cluster {
    fn start(nodes: u32, shards: u32) -> Cluster {
        Self::start_with(
            nodes,
            shards,
            LEASE,
            DOWN_AFTER,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        )
    }

    /// With a given map lease, liveness timeout and peer timeout (D40).
    fn start_with(
        nodes: u32,
        shards: u32,
        lease: std::time::Duration,
        down_after: std::time::Duration,
        peer_timeout: std::time::Duration,
        scrub: std::time::Duration,
    ) -> Cluster {
        Self::start_full(
            nodes,
            shards,
            lease,
            down_after,
            peer_timeout,
            scrub,
            DOWN_OUT,
        )
    }

    /// With a given down-out interval too.
    fn start_full(
        nodes: u32,
        shards: u32,
        lease: std::time::Duration,
        down_after: std::time::Duration,
        peer_timeout: std::time::Duration,
        scrub: std::time::Duration,
        down_out: std::time::Duration,
    ) -> Cluster {
        let meta_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let meta_addr = meta_l.local_addr().unwrap();
        let mut map = ClusterMap::new(1);
        let mut listeners = Vec::new();
        let mut all_addrs = Vec::new();
        for n in 0..nodes {
            let ls: Vec<std::net::TcpListener> = (0..shards)
                .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
                .collect();
            let addrs: Vec<SocketAddr> = ls.iter().map(|l| l.local_addr().unwrap()).collect();
            map.add_node(n, &addrs, 0);
            all_addrs.push(addrs);
            listeners.push(ls);
        }
        let mut pool = replicated_pool(POOL, "vol", 3, 64);
        pool.chunk_size = CHUNK;
        map.add_pool(pool);
        // A one-copy cache pool (D45).
        map.add_pool(hyperstor_meta::PoolInfo {
            id: CACHE_POOL,
            name: "kv".into(),
            kind: hyperstor_meta::PoolKind::Cache,
            size: 1,
            min_size: 1,
            pg_count: 16,
            domain: hyperstor_placement::Domain::Host,
            chunk_size: CHUNK,
            class: None,
        });
        map.ensure_meta_pool();
        map.recompute();
        let (meta_tx, mut meta_rx) = channel::<MetaCmd>();
        let meta_thread = std::thread::spawn(move || {
            block_on(async move {
                let meta = hyperstor_meta::Meta::with_lease(map, lease);
                meta.set_down_out(down_out);
                spawn(hyperstor_meta::serve(
                    hyperstor_reactor::net::TcpListener::from_std(meta_l),
                    meta.clone(),
                ));
                spawn(meta.clone().liveness(down_after));
                spawn(meta.clone().publisher());
                while let Some(cmd) = meta_rx.recv().await {
                    match cmd {
                        MetaCmd::Stop => break,
                        MetaCmd::Inconsistent(tx) => {
                            let _ = tx.send(meta.inconsistent.borrow().clone());
                        }
                    }
                }
                meta.published.borrow().clone()
            })
        });
        let blocked: Vec<Arc<std::sync::atomic::AtomicBool>> =
            (0..nodes).map(|_| Arc::default()).collect();
        let partitions = Arc::new(hyperstor_daemon::faults::Partitions::default());
        let mut faulty = std::collections::HashMap::new();
        let nodes = listeners
            .into_iter()
            .enumerate()
            .map(|(n, ls)| {
                Some(start(NodeConfig {
                    partitions: Some(partitions.clone()),
                    node_id: n as u32,
                    meta: vec![meta_addr],
                    shards: ls
                        .into_iter()
                        .enumerate()
                        .map(|(i, l)| {
                            let id = n as u32 * 1000 + i as u32;
                            let dev = hyperstor_daemon::faults::FaultyDev::new(Arc::new(
                                MemDev::new(256 << 20),
                            ));
                            faulty.insert(id, dev.clone());
                            ShardSpec {
                                device_id: id,
                                device: DeviceSpec::Existing(dev),
                                listener: l,
                            }
                        })
                        .collect(),
                    cpus: Vec::new(),
                    spin: std::time::Duration::ZERO,
                    peer_timeout,
                    meta_blocked: Some(blocked[n].clone()),
                    defer_max: None,
                    scrub_interval: scrub,
                    no_digests: false,
                }))
            })
            .collect();
        Cluster {
            peer_timeout,
            scrub,
            meta_addr,
            blocked,
            addrs: all_addrs,
            stopped: std::collections::HashMap::new(),
            meta_tx,
            meta_thread: Some(meta_thread),
            nodes,
            partitions,
            faulty,
        }
    }

    /// Stops node `n` (its reactors exit; heartbeats stop). Its devices
    /// are kept, so it can come back with what it had.
    fn kill(&mut self, n: usize) {
        if let Some(h) = self.nodes[n].take() {
            let devs = h.stop().into_iter().map(|d| d.unwrap()).collect();
            self.stopped.insert(n, devs);
        }
    }

    /// Starts node `n` again on its old devices and addresses.
    fn restart(&mut self, n: usize) {
        self.restart_as(n, false)
    }

    /// Restarts node `n`; with `no_digests`, as a node from before
    /// digests (a rolling upgrade).
    fn restart_as(&mut self, n: usize, no_digests: bool) {
        let devs = self.stopped.remove(&n).expect("node was stopped");
        let shards = devs
            .into_iter()
            .zip(&self.addrs[n])
            .enumerate()
            .map(|(i, (d, a))| ShardSpec {
                device_id: n as u32 * 1000 + i as u32,
                device: DeviceSpec::Existing(d),
                listener: std::net::TcpListener::bind(a).unwrap(),
            })
            .collect();
        self.nodes[n] = Some(start(NodeConfig {
            partitions: Some(self.partitions.clone()),
            node_id: n as u32,
            meta: vec![self.meta_addr],
            shards,
            cpus: Vec::new(),
            spin: std::time::Duration::ZERO,
            peer_timeout: self.peer_timeout,
            meta_blocked: Some(self.blocked[n].clone()),
            defer_max: None,
            scrub_interval: self.scrub,
            no_digests,
        }));
    }

    /// The PGs scrub has reported inconsistent to the map service.
    fn inconsistent(&self) -> hyperstor_meta::Inconsistent {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(self.meta_tx.send(MetaCmd::Inconsistent(tx)).is_ok());
        rx.recv().unwrap()
    }

    /// Stops everything; returns the final map and each device by id.
    fn stop(mut self) -> (ClusterMap, Devices) {
        let mut devs = Vec::new();
        for (n, h) in self.nodes.iter_mut().enumerate() {
            if let Some(h) = h.take() {
                for (i, d) in h.stop().into_iter().enumerate() {
                    devs.push((n as u32 * 1000 + i as u32, d.unwrap()));
                }
            }
        }
        let _ = self.meta_tx.send(MetaCmd::Stop);
        let map = self.meta_thread.take().unwrap().join().unwrap();
        (map, devs)
    }
}

fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = SmallRng::seed_from_u64(seed);
    (0..len).map(|_| rng.random()).collect()
}

#[test]
fn writes_reach_every_copy_and_read_back() {
    let c = Cluster::start(4, 2);
    let meta = c.meta_addr;
    let written: Vec<(u128, u64, Vec<u8>)> = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 99).await.unwrap();
            let mut written = Vec::new();
            // Large writes across chunk boundaries, on three volumes.
            for v in 1..=3u128 {
                let data = pattern(v as u64, 3 * CHUNK as usize + 8192);
                let off = (CHUNK as u64) / 2;
                cl.write(POOL, v, off, data.clone()).await.unwrap();
                written.push((v, off, data));
            }
            // Many concurrent small writes.
            let hs: Vec<_> = (0..64u64)
                .map(|i| {
                    let cl = cl.clone();
                    spawn(async move {
                        let data = pattern(1000 + i, 4096);
                        let off = 64 * 4096 * i;
                        cl.write(POOL, 7, off, data.clone()).await.unwrap();
                        (7u128, off, data)
                    })
                })
                .collect();
            for h in hs {
                written.push(h.await);
            }
            for (v, off, data) in &written {
                let back = cl.read(POOL, *v, *off, data.len() as u64).await.unwrap();
                assert!(back == *data, "volume {v} at {off} reads back differently");
            }
            // Never-written space reads as zeros.
            let z = cl.read(POOL, 42, 0, 8192).await.unwrap();
            assert!(z.iter().all(|&b| b == 0));
            // Unaligned I/O is refused.
            assert!(cl.write(POOL, 1, 1, vec![0; 4096]).await.is_err());
            written
        })
    })
    .join()
    .unwrap();

    // Every copy of every chunk holds the same data.
    let (map, devs) = c.stop();
    let shards: std::collections::HashMap<u32, hyperstor_engine::Shard> = devs
        .into_iter()
        .map(|(id, d)| {
            (
                id,
                hyperstor_engine::open(&copy(&d)).unwrap().1.pop().unwrap(),
            )
        })
        .collect();
    let pool = &map.pools[&POOL];
    let mut checked = 0;
    for (v, off, data) in &written {
        let first = off / CHUNK as u64;
        let last = (off + data.len() as u64 - 1) / CHUNK as u64;
        for chunk in first..=last {
            let pg = pg_of(*v, chunk, pool.pg_count);
            let acting = &map.pg(POOL, pg).unwrap().acting;
            assert_eq!(acting.len(), 3);
            let key = ObjKey::new(POOL, pg, Namespace::Vol, chunk_name(*v, chunk), 0);
            let copies: Vec<Vec<u8>> = acting
                .iter()
                .filter(|d| **d != NO_DEVICE)
                .map(|d| {
                    shards[d]
                        .read(&key, 0, CHUNK as u64)
                        .unwrap_or_else(|e| panic!("device {d} lacks {key:?}: {e}"))
                        .to_vec()
                })
                .collect();
            assert!(
                copies.windows(2).all(|w| w[0] == w[1]),
                "copies differ for {key:?}"
            );
            checked += 1;
        }
    }
    assert!(checked > 10);
}

/// A copy of a stopped node's device, to open its engine again.
fn copy(d: &Arc<dyn BlockDev>) -> Arc<dyn BlockDev> {
    let mut img = vec![0u8; d.len() as usize];
    for (i, chunk) in img.chunks_mut(1 << 20).enumerate() {
        d.read((i as u64) << 20, chunk).unwrap();
    }
    Arc::new(MemDev::from_image(img))
}

#[test]
fn clients_follow_map_changes() {
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let client = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            let before = cl.map().epoch;
            let d1 = pattern(1, 2 * CHUNK as usize);
            cl.write(POOL, 1, 0, d1.clone()).await.unwrap();
            // A volume whose first chunk's primary is on node 0, so the
            // client's map is certainly wrong for it once node 0 is gone.
            let old = cl.map();
            let v2 = (2u128..)
                .find(|v| {
                    let pg = pg_of(*v, 0, old.pools[&POOL].pg_count);
                    let p = old.pg(POOL, pg).unwrap().primary().unwrap();
                    old.devices[&p].node == 0
                })
                .unwrap();
            // Node 0 stops. The client still holds the old map; its next
            // requests fail (unreachable, stale epoch or not primary) and
            // are retried with a fresh map once the failure is detected.
            kill_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            let d2 = pattern(2, 2 * CHUNK as usize);
            cl.write(POOL, v2, 0, d2.clone()).await.unwrap();
            assert!(cl.map().epoch > before, "the client learned the new map");
            assert_eq!(cl.read(POOL, v2, 0, d2.len() as u64).await.unwrap(), d2);
            assert_eq!(cl.read(POOL, 1, 0, d1.len() as u64).await.unwrap(), d1);
        })
    });
    kill_rx.recv().unwrap();
    c.kill(0);
    done_tx.send(()).unwrap();
    client.join().unwrap();
    let (map, _) = c.stop();
    assert!(!map.nodes[&0].up);
    for pg in map.pgs.values() {
        assert!(pg.acting.iter().all(|d| map.devices[d].node != 0));
    }
}

/// Checks that every copy in each PG's acting set holds the same data for
/// the given chunks.
fn copies_agree(map: &ClusterMap, devs: Devices, chunks: &[(u128, u64)]) {
    let shards: std::collections::HashMap<u32, hyperstor_engine::Shard> = devs
        .into_iter()
        .map(|(id, d)| {
            (
                id,
                hyperstor_engine::open(&copy(&d)).unwrap().1.pop().unwrap(),
            )
        })
        .collect();
    let pool = &map.pools[&POOL];
    for (v, chunk) in chunks {
        let pg = pg_of(*v, *chunk, pool.pg_count);
        let info = map.pg(POOL, pg).unwrap();
        assert_eq!(info.acting, info.up, "PG {pg} still backfilling");
        assert_eq!(info.acting.len(), 3);
        let key = ObjKey::new(POOL, pg, Namespace::Vol, chunk_name(*v, *chunk), 0);
        let copies: Vec<Vec<u8>> = info
            .acting
            .iter()
            .map(|d| {
                shards[d]
                    .read(&key, 0, CHUNK as u64)
                    .unwrap_or_else(|e| panic!("device {d} lacks {key:?}: {e}"))
                    .to_vec()
            })
            .collect();
        assert!(
            copies.windows(2).all(|w| w[0] == w[1]),
            "copies differ for {key:?}"
        );
    }
}

#[test]
fn a_node_fails_writes_continue_and_backfill_restores_copies() {
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let d1 = pattern(11, 12 * CHUNK as usize);
    let d1c = d1.clone();
    // Volume 1 is written before the failure.
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, d1c).await.unwrap();
        })
    })
    .join()
    .unwrap();
    c.kill(2);
    let d2 = pattern(12, 12 * CHUNK as usize);
    let (d1r, d2r) = (d1.clone(), d2.clone());
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            // Writes go on (after the failure is detected) with two copies.
            cl.write(POOL, 2, 0, d2r.clone()).await.unwrap();
            // Data written before the failure is still there: each PG's new
            // primary is a surviving member of its old acting set.
            assert!(cl.read(POOL, 1, 0, d1r.len() as u64).await.unwrap() == d1r);
            assert!(cl.read(POOL, 2, 0, d2r.len() as u64).await.unwrap() == d2r);
            // Backfill brings every PG back to three copies.
            let t = std::time::Instant::now();
            loop {
                cl.refresh_map().await.unwrap();
                let m = cl.map();
                let pending = m.pgs.values().filter(|p| p.acting != p.up).count();
                if pending == 0 {
                    break;
                }
                assert!(
                    t.elapsed().as_secs() < 30,
                    "{pending} PGs still backfilling"
                );
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert!(cl.read(POOL, 1, 0, d1r.len() as u64).await.unwrap() == d1r);
        })
    })
    .join()
    .unwrap();
    let (map, devs) = c.stop();
    assert!(!map.nodes[&2].up);
    let chunks: Vec<(u128, u64)> = (1..=2u128)
        .flat_map(|v| (0..12u64).map(move |c| (v, c)))
        .collect();
    copies_agree(&map, devs, &chunks);
}

/// Waits until no PG is backfilling, from a client's view of the map.
async fn wait_backfilled(cl: &std::rc::Rc<Client>) {
    let t = std::time::Instant::now();
    loop {
        cl.refresh_map().await.unwrap();
        let m = cl.map();
        let pending = m.pgs.values().filter(|p| p.acting != p.up).count();
        if pending == 0 {
            return;
        }
        assert!(
            t.elapsed().as_secs() < 30,
            "{pending} PGs still backfilling"
        );
        hyperstor_reactor::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[test]
fn a_node_returns_with_stale_data_and_is_refilled() {
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let v1 = pattern(21, 8 * CHUNK as usize);
    let v1c = v1.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, v1c).await.unwrap();
        })
    })
    .join()
    .unwrap();
    c.kill(3);
    // While node 3 is away, volume 1 is overwritten and volume 2 written;
    // node 3's devices keep the old volume 1 and no volume 2.
    let v1b = pattern(22, 8 * CHUNK as usize);
    let v2 = pattern(23, 8 * CHUNK as usize);
    let (v1bc, v2c) = (v1b.clone(), v2.clone());
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            cl.write(POOL, 1, 0, v1bc).await.unwrap();
            cl.write(POOL, 2, 0, v2c).await.unwrap();
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    c.restart(3);
    let (v1r, v2r) = (v1b.clone(), v2.clone());
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 7).await.unwrap();
            // Node 3 is placed again; it must be refilled before it serves,
            // and the members that stood in for it are then dropped.
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(800)).await;
            wait_backfilled(&cl).await;
            assert!(cl.map().nodes[&3].up);
            assert!(cl.read(POOL, 1, 0, v1r.len() as u64).await.unwrap() == v1r);
            assert!(cl.read(POOL, 2, 0, v2r.len() as u64).await.unwrap() == v2r);
        })
    })
    .join()
    .unwrap();
    let (map, devs) = c.stop();
    assert!(
        map.pgs
            .values()
            .any(|p| p.acting.iter().any(|d| map.devices[d].node == 3)),
        "node 3 serves PGs again"
    );
    let chunks: Vec<(u128, u64)> = (1..=2u128)
        .flat_map(|v| (0..8u64).map(move |c| (v, c)))
        .collect();
    copies_agree(&map, devs, &chunks);
}

/// Delta recovery (replication.md §8.5): a node that returns after a short
/// absence is sent only the objects that changed while it was away, not
/// every object of its PGs, and its copies end up the same as the others.
#[test]
fn a_returning_node_gets_only_what_changed() {
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let v1 = pattern(31, 32 * CHUNK as usize);
    let v1c = v1.clone();
    // The chunks node 3 holds a copy of.
    let on3: Vec<u64> = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, v1c).await.unwrap();
            wait_backfilled(&cl).await;
            let m = cl.map();
            let pg_count = m.pools[&POOL].pg_count;
            (0..32u64)
                .filter(|ch| {
                    m.pg(POOL, pg_of(1, *ch, pg_count))
                        .unwrap()
                        .acting
                        .iter()
                        .any(|d| m.devices[d].node == 3)
                })
                .collect()
        })
    })
    .join()
    .unwrap();
    assert!(on3.len() >= 8, "node 3 holds {} chunks", on3.len());
    let changed = [on3[0], on3[on3.len() / 2]];
    c.kill(3);
    // Two of the chunks node 3 holds change while it is down.
    let mut v1b = v1.clone();
    for ch in changed {
        let at = (ch * CHUNK as u64) as usize;
        v1b[at..at + CHUNK as usize].copy_from_slice(&pattern(40 + ch, CHUNK as usize));
    }
    let v1bc = v1b.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            let t = std::time::Instant::now();
            loop {
                cl.refresh_map().await.unwrap();
                if !cl.map().nodes[&3].up {
                    break;
                }
                assert!(t.elapsed().as_secs() < 10, "node 3 never marked down");
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            for ch in changed {
                let at = (ch * CHUNK as u64) as usize;
                let data = v1bc[at..at + CHUNK as usize].to_vec();
                cl.write(POOL, 1, ch * CHUNK as u64, data).await.unwrap();
            }
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    c.restart(3);
    let v1r = v1b.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 7).await.unwrap();
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(800)).await;
            wait_backfilled(&cl).await;
            assert!(cl.map().nodes[&3].up);
            assert!(cl.read(POOL, 1, 0, v1r.len() as u64).await.unwrap() == v1r);
        })
    })
    .join()
    .unwrap();
    let pushed: u64 = c.nodes[3]
        .as_ref()
        .unwrap()
        .metrics
        .iter()
        .map(|(_, m)| m.push_bytes.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    // A whole refill would send node 3 every chunk it holds; only the two
    // that changed are sent (each with its block CRCs and size).
    assert!(
        (CHUNK as u64..=2 * CHUNK as u64 + 8192).contains(&pushed),
        "node 3 was sent {pushed} bytes for {} chunks, 2 of them changed",
        on3.len()
    );
    let (map, devs) = c.stop();
    assert!(
        map.pgs
            .values()
            .any(|p| p.acting.iter().any(|d| map.devices[d].node == 3)),
        "node 3 serves PGs again"
    );
    let chunks: Vec<(u128, u64)> = (0..32u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// A node from before digests (a rolling upgrade) answers `Digests` with
/// "unsupported request"; the primary then resets its copy and sends every
/// object, as backfill did before delta recovery.
#[test]
fn a_returning_node_without_digests_is_refilled_whole() {
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let v1 = pattern(61, 16 * CHUNK as usize);
    let v1c = v1.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, v1c).await.unwrap();
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    c.kill(3);
    let v1b = pattern(62, 16 * CHUNK as usize);
    let v1bc = v1b.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            let t = std::time::Instant::now();
            while cl.map().nodes[&3].up {
                assert!(t.elapsed().as_secs() < 10, "node 3 never marked down");
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(50)).await;
                cl.refresh_map().await.unwrap();
            }
            cl.write(POOL, 1, 0, v1bc).await.unwrap();
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    c.restart_as(3, true);
    let v1r = v1b.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 7).await.unwrap();
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(800)).await;
            wait_backfilled(&cl).await;
            assert!(cl.map().nodes[&3].up);
            assert!(cl.read(POOL, 1, 0, v1r.len() as u64).await.unwrap() == v1r);
        })
    })
    .join()
    .unwrap();
    let pushes: u64 = c.nodes[3]
        .as_ref()
        .unwrap()
        .metrics
        .iter()
        .map(|(_, m)| m.pushes.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    assert!(pushes > 0, "node 3 was refilled");
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..16u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// Light scrub (replication.md §8.5): a replica whose copy diverged (here,
/// overwritten behind the primary's back) is found by comparing digests
/// and repaired from the primary, which the third copy agrees with.
#[test]
fn scrub_repairs_a_copy_the_others_outvote() {
    use hyperstor_proto::{Msg, Target, WireKey, block_crcs};
    let c = Cluster::start_with(
        4,
        1,
        LEASE,
        DOWN_AFTER,
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(300),
    );
    let meta = c.meta_addr;
    let v1 = pattern(51, 4 * CHUNK as usize);
    let v1c = v1.clone();
    let (primary, replica) = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, v1c).await.unwrap();
            wait_backfilled(&cl).await;
            let m = cl.map();
            let pg = pg_of(1, 0, m.pools[&POOL].pg_count);
            let info = m.pg(POOL, pg).unwrap();
            let (primary, replica) = (info.acting[0], info.acting[1]);
            // Overwrite the replica's copy of chunk 0 directly.
            let conn = hyperstor_rpc::Conn::connect(m.device_addr(replica).unwrap())
                .await
                .unwrap();
            let bad = pattern(52, CHUNK as usize);
            let (reply, _) = conn
                .call(
                    &Msg::Push {
                        to: Target {
                            pool: POOL,
                            pg,
                            map_epoch: m.epoch,
                            pg_epoch: info.epoch,
                        },
                        key: WireKey {
                            ns: hyperstor_client::NS_VOL,
                            name: chunk_name(1, 0),
                            version: 0,
                        },
                        crcs: block_crcs(&bad),
                    },
                    &bad,
                )
                .await
                .unwrap();
            assert_eq!(reply, Msg::PushOk);
            (primary, replica)
        })
    })
    .join()
    .unwrap();
    let node_of = |d: u32| (d / 1000) as usize;
    let repaired = || {
        c.nodes[node_of(primary)].as_ref().unwrap().metrics[0]
            .1
            .scrub_repaired
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let t = std::time::Instant::now();
    while repaired() == 0 {
        assert!(t.elapsed().as_secs() < 10, "scrub never repaired the copy");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(repaired(), 1, "only the overwritten copy differed");
    let m = &c.nodes[node_of(primary)].as_ref().unwrap().metrics[0].1;
    assert_eq!(
        m.scrub_unresolved
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    let _ = replica;
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..4u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// Deep scrub (replication.md §8.5): a block damaged on a replica's
/// device, under the engine (its index, and so its digest, still look
/// right), is found by reading every block, and the primary replaces the
/// damaged copy from its own, which the third copy agrees with.
#[test]
fn deep_scrub_finds_a_damaged_block_and_repairs_it() {
    let mut c = Cluster::start_with(
        4,
        1,
        LEASE,
        DOWN_AFTER,
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(300),
    );
    let meta = c.meta_addr;
    let v1 = pattern(71, 4 * CHUNK as usize);
    let v1c = v1.clone();
    let (primary, replica) = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, v1c).await.unwrap();
            // Later commits on every node, so chunk 0's write isn't the
            // last group on any device: recovery checks that group's data
            // and would drop it as torn (I4), rather than keep it damaged.
            cl.write(POOL, 2, 0, pattern(72, 32 * CHUNK as usize))
                .await
                .unwrap();
            wait_backfilled(&cl).await;
            let m = cl.map();
            let info = m.pg(POOL, pg_of(1, 0, m.pools[&POOL].pg_count)).unwrap();
            (info.acting[0], info.acting[1])
        })
    })
    .join()
    .unwrap();
    let node_of = |d: u32| (d / 1000) as usize;
    // Flip a byte of chunk 0's first block on the replica's device.
    c.kill(node_of(replica));
    let dev = c.stopped.get_mut(&node_of(replica)).unwrap();
    let mut img = vec![0u8; dev[0].len() as usize];
    for (i, part) in img.chunks_mut(1 << 20).enumerate() {
        dev[0].read((i as u64) << 20, part).unwrap();
    }
    let at = img
        .chunks(4096)
        .position(|b| b == &v1[..4096])
        .expect("chunk 0's first block on the device")
        * 4096;
    img[at + 100] ^= 0xff;
    dev[0] = Arc::new(MemDev::from_image(img));
    c.restart(node_of(replica));
    let count = |n: usize, f: fn(&metrics::ShardMetrics) -> &std::sync::atomic::AtomicU64| {
        f(&c.nodes[n].as_ref().unwrap().metrics[0].1).load(std::sync::atomic::Ordering::Relaxed)
    };
    let t = std::time::Instant::now();
    while count(node_of(primary), |m| &m.scrub_repaired) == 0 {
        assert!(
            t.elapsed().as_secs() < 15,
            "the damaged copy was never repaired"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(count(node_of(replica), |m| &m.scrub_damaged), 1);
    let v1r = v1.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            wait_backfilled(&cl).await;
            assert!(cl.read(POOL, 1, 0, v1r.len() as u64).await.unwrap() == v1r);
        })
    })
    .join()
    .unwrap();
    // Every copy reads back through the engine's CRC check, the
    // replica's included.
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..4u64)
        .map(|c| (1, c))
        .chain((0..32u64).map(|c| (2, c)))
        .collect();
    copies_agree(&map, devs, &chunks);
}

/// Peering (replication.md §8.4, I61): when a primary fails, a write it
/// had sent to only one replica (never acknowledged) leaves that replica's
/// copy different. The new primary makes the members' copies the same as
/// its own before it serves, so the PG's copies agree at once, not only
/// after a scrub.
#[test]
fn a_new_primary_makes_its_members_agree_before_serving() {
    use hyperstor_proto::{Msg, Target, WireKey, block_crcs};
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let v1 = pattern(81, 4 * CHUNK as usize);
    let v1c = v1.clone();
    let (primary, second) = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, v1c).await.unwrap();
            wait_backfilled(&cl).await;
            let m = cl.map();
            let pg = pg_of(1, 0, m.pools[&POOL].pg_count);
            let info = m.pg(POOL, pg).unwrap();
            // The third copy gets a write the others never see.
            let third = info.acting[2];
            let conn = hyperstor_rpc::Conn::connect(m.device_addr(third).unwrap())
                .await
                .unwrap();
            let lost = pattern(82, CHUNK as usize);
            let (reply, _) = conn
                .call(
                    &Msg::Push {
                        to: Target {
                            pool: POOL,
                            pg,
                            map_epoch: m.epoch,
                            pg_epoch: info.epoch,
                        },
                        key: WireKey {
                            ns: hyperstor_client::NS_VOL,
                            name: chunk_name(1, 0),
                            version: 0,
                        },
                        crcs: block_crcs(&lost),
                    },
                    &lost,
                )
                .await
                .unwrap();
            assert_eq!(reply, Msg::PushOk);
            (info.acting[0], info.acting[1])
        })
    })
    .join()
    .unwrap();
    let node_of = |d: u32| (d / 1000) as usize;
    c.kill(node_of(primary));
    let v1r = v1.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            // Served by the new primary, the second copy, once peered.
            assert!(cl.read(POOL, 1, 0, v1r.len() as u64).await.unwrap() == v1r);
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    let pushed = c.nodes[node_of(second)].as_ref().unwrap().metrics[0]
        .1
        .peer_pushes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(pushed >= 1, "the new primary pushed nothing while peering");
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..4u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// Random failures against a model (CLAUDE.md, "Code conventions"):
/// writes to random chunks interleaved with random node stops and
/// restarts (at most one node down, so every PG keeps two copies), which
/// exercises peering at each change of primary and delta recovery at
/// each return. Every acknowledged write must read back, and in the end
/// every copy must agree. `HS_SEED` picks one seed; by default three run.
#[test]
fn random_failures_lose_no_acknowledged_write() {
    let seeds: Vec<u64> = match std::env::var("HS_SEED") {
        Ok(s) => vec![s.parse().unwrap()],
        Err(_) => vec![1, 2, 3],
    };
    for seed in seeds {
        random_failures(seed);
    }
}

fn random_failures(seed: u64) {
    const CHUNKS: u64 = 8;
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    // Every chunk exists from the start, so every copy can be compared.
    let mut model: Vec<Vec<u8>> = (0..CHUNKS)
        .map(|ch| pattern(seed * 1000 + 500 + ch, CHUNK as usize))
        .collect();
    let all = model.concat();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            cl.write(POOL, 1, 0, all).await.unwrap();
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    let mut down: Option<usize> = None;
    let mut log = Vec::new();
    for step in 0..14u64 {
        match rng.random_range(0..4u32) {
            0 if down.is_none() => {
                let n = rng.random_range(0..4usize);
                c.kill(n);
                down = Some(n);
                log.push(format!("kill {n}"));
            }
            1 if down.is_some() => {
                let n = down.take().unwrap();
                c.restart(n);
                log.push(format!("restart {n}"));
            }
            _ => {
                let ch = rng.random_range(0..CHUNKS);
                let data = pattern(seed * 1000 + step, CHUNK as usize);
                let d = data.clone();
                std::thread::spawn(move || {
                    block_on(async move {
                        let cl = Client::connect(meta, 10 + step).await.unwrap();
                        cl.write(POOL, 1, ch * CHUNK as u64, d).await.unwrap();
                    })
                })
                .join()
                .unwrap_or_else(|_| panic!("seed {seed}: write failed after {log:?}"));
                model[ch as usize] = data;
                log.push(format!("write {ch}"));
            }
        }
    }
    if let Some(n) = down.take() {
        c.restart(n);
        log.push(format!("restart {n}"));
    }
    let expect: Vec<u8> = model.concat();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 99).await.unwrap();
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(800)).await;
            wait_backfilled(&cl).await;
            cl.read(POOL, 1, 0, expect.len() as u64).await.unwrap() == expect
        })
    })
    .join()
    .map(|same| {
        assert!(
            same,
            "seed {seed}: reads differ from the model after {log:?}"
        )
    })
    .unwrap_or_else(|_| panic!("seed {seed}: final read failed after {log:?}"));
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..CHUNKS).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// Object operations (metadata.md §4.2): an operation list on a named
/// object applies atomically when its assertions hold, on every copy; a
/// compare-and-set on a version attribute lets exactly one of several
/// racing writers win; a replica that missed a change is refilled with the
/// whole object, attributes and key–value table included.
#[test]
fn objects_change_atomically_and_every_copy_follows() {
    use hyperstor_client::{ClientError, NS_META, object_pg};
    use hyperstor_proto::{Code, WireOp as W};
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let name = b"header/1".to_vec();
    let n = name.clone();
    let v = |x: &str| W::SetAttr(b"v".to_vec(), x.as_bytes().to_vec());
    let at = |x: &str| W::AssertAttr {
        name: b"v".to_vec(),
        value: Some(x.as_bytes().to_vec()),
    };
    let conflict = |r: Result<(), ClientError>| {
        matches!(
            r,
            Err(ClientError::Server {
                code: Code::Conflict,
                ..
            })
        )
    };
    let (pg, acting) = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            let kv = |k: &str, x: &str| W::KvPut(k.as_bytes().to_vec(), x.as_bytes().to_vec());
            cl.exec(POOL, NS_META, &n, &[W::AssertAbsent, v("1"), kv("a", "1")])
                .await
                .unwrap();
            assert!(conflict(
                cl.exec(POOL, NS_META, &n, &[W::AssertAbsent, v("x")]).await
            ));
            cl.exec(POOL, NS_META, &n, &[at("1"), v("2"), kv("b", "2")])
                .await
                .unwrap();
            assert!(conflict(
                cl.exec(POOL, NS_META, &n, &[at("1"), v("x")]).await
            ));
            // Twenty rounds of sixteen writers, each with its own connection,
            // racing from the same version: exactly one wins each round.
            let mut clients = Vec::new();
            for i in 0..16 {
                clients.push(Client::connect(meta, 100 + i).await.unwrap());
            }
            const ROUNDS: u32 = 20;
            for round in 2..2 + ROUNDS {
                let racers: Vec<_> = clients
                    .iter()
                    .enumerate()
                    .map(|(i, cl)| {
                        let (cl, n) = (cl.clone(), n.clone());
                        let ops = vec![
                            at(&round.to_string()),
                            v(&(round + 1).to_string()),
                            kv(&format!("w{round}-{i}"), ""),
                        ];
                        spawn(async move { cl.exec(POOL, NS_META, &n, &ops).await })
                    })
                    .collect();
                let mut won = 0;
                for r in racers {
                    match r.await {
                        Ok(()) => won += 1,
                        e => {
                            let shown = format!("{e:?}");
                            assert!(conflict(e), "{shown}");
                        }
                    }
                }
                assert_eq!(won, 1, "round {round}: one compare-and-set wins");
            }
            let l = cl
                .lookup(POOL, NS_META, &n, b"", None, 100)
                .await
                .unwrap()
                .unwrap();
            let ver = &l.attrs.iter().find(|(k, _)| k == b"v").unwrap().1;
            assert_eq!(ver, (2 + ROUNDS).to_string().as_bytes());
            let keys: Vec<&[u8]> = l.kv.iter().map(|(k, _)| k.as_slice()).collect();
            assert_eq!(
                keys.len(),
                2 + ROUNDS as usize,
                "a, b and each round's winner"
            );
            assert_eq!(&keys[..2], &[b"a".as_slice(), b"b".as_slice()]);
            // A range of the table, and a limit.
            let l = cl
                .lookup(POOL, NS_META, &n, b"b", None, 1)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(l.kv, vec![(b"b".to_vec(), b"2".to_vec())]);
            assert!(
                cl.lookup(POOL, NS_META, b"nothing", b"", None, 1)
                    .await
                    .unwrap()
                    .is_none()
            );
            let m = cl.map();
            let pg = object_pg(&n, m.pools[&POOL].pg_count);
            (pg, m.pg(POOL, pg).unwrap().acting.clone())
        })
    })
    .join()
    .unwrap();
    // A replica is away while the object changes again.
    let away = (acting[1] / 1000) as usize;
    c.kill(away);
    let n = name.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            let l = cl
                .lookup(POOL, NS_META, &n, b"", None, 0)
                .await
                .unwrap()
                .unwrap();
            let ver = l.attrs.iter().find(|(k, _)| k == b"v").unwrap().1.clone();
            let ops = [
                W::AssertAttr {
                    name: b"v".to_vec(),
                    value: Some(ver),
                },
                W::SetAttr(b"v".to_vec(), b"4".to_vec()),
                W::KvPut(b"c".to_vec(), b"3".to_vec()),
                W::KvDel(b"a".to_vec()),
            ];
            cl.exec(POOL, NS_META, &n, &ops).await.unwrap();
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    c.restart(away);
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 7).await.unwrap();
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(800)).await;
            wait_backfilled(&cl).await;
        })
    })
    .join()
    .unwrap();
    let (map, devs) = c.stop();
    let shards: std::collections::HashMap<u32, hyperstor_engine::Shard> = devs
        .into_iter()
        .map(|(id, d)| {
            (
                id,
                hyperstor_engine::open(&copy(&d)).unwrap().1.pop().unwrap(),
            )
        })
        .collect();
    let info = map.pg(POOL, pg).unwrap();
    assert!(
        info.acting.iter().any(|d| *d / 1000 == away as u32),
        "the replica is back"
    );
    let key = ObjKey::new(POOL, pg, Namespace::Meta, name, 0);
    let copies: Vec<_> = info
        .acting
        .iter()
        .map(|d| {
            let sh = &shards[d];
            (
                sh.stat(&key).map(|s| s.size),
                sh.attrs(&key).cloned(),
                sh.kv_scan(&key, b"", None, usize::MAX),
            )
        })
        .collect();
    assert!(
        copies.windows(2).all(|w| w[0] == w[1]),
        "copies differ: {copies:?}"
    );
    let kv: Vec<Vec<u8>> = copies[0].2.iter().map(|(k, _)| k.clone()).collect();
    assert!(kv.contains(&b"c".to_vec()) && !kv.contains(&b"a".to_vec()));
}

/// Volumes (metadata.md §4.2, D77): created with a header and a directory
/// entry in `.meta`, listed per pool, grown but not shrunk, and deleted
/// with their data.
#[test]
fn volumes_are_created_listed_grown_and_deleted() {
    use hyperstor_client::ClientError;
    use hyperstor_client::volume::VolumeState;
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let gone = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            let size = 4 * CHUNK as u64;
            let a = cl.create_volume(POOL, size, None, "a").await.unwrap();
            let b = cl.create_volume(POOL, size, Some(0xb), "b").await.unwrap();
            let d = cl.create_volume(POOL, size, None, "d").await.unwrap();
            assert_eq!(b.id, 0xb);
            assert!(matches!(
                cl.create_volume(POOL, size, Some(0xb), "again").await,
                Err(ClientError::VolumeExists(0xb))
            ));
            assert!(
                cl.create_volume(hyperstor_meta::META_POOL, size, None, "x")
                    .await
                    .is_err()
            );
            let mut names: Vec<String> = cl
                .list_volumes(POOL)
                .await
                .unwrap()
                .into_iter()
                .map(|v| v.name)
                .collect();
            names.sort();
            assert_eq!(names, ["a", "b", "d"]);
            // Grown, not shrunk.
            let a2 = cl.expand_volume(a.id, 2 * size).await.unwrap();
            assert_eq!((a2.size, a2.version), (2 * size, 2));
            assert!(matches!(
                cl.expand_volume(a.id, size).await,
                Err(ClientError::Shrink { .. })
            ));
            assert_eq!(cl.volume(a.id).await.unwrap().unwrap().size, 2 * size);
            // Deleted with its data.
            cl.write(POOL, d.lineage, 0, pattern(9, size as usize))
                .await
                .unwrap();
            cl.delete_volume(d.id).await.unwrap();
            assert!(cl.volume(d.id).await.unwrap().is_none());
            assert!(matches!(
                cl.delete_volume(d.id).await,
                Err(ClientError::NoVolume(_))
            ));
            let left: Vec<u128> = cl
                .list_volumes(POOL)
                .await
                .unwrap()
                .iter()
                .map(|v| v.id)
                .collect();
            assert!(!left.contains(&d.id) && left.len() == 2);
            assert!(
                cl.list_volumes(POOL)
                    .await
                    .unwrap()
                    .iter()
                    .all(|v| v.state == VolumeState::Active)
            );
            d.lineage
        })
    })
    .join()
    .unwrap();
    let (map, devs) = c.stop();
    let pool = &map.pools[&POOL];
    for (id, d) in devs {
        let sh = hyperstor_engine::open(&copy(&d)).unwrap().1.pop().unwrap();
        for chunk in 0..4u64 {
            let pg = pg_of(gone, chunk, pool.pg_count);
            let key = ObjKey::new(POOL, pg, Namespace::Vol, chunk_name(gone, chunk), 0);
            assert!(
                sh.stat(&key).is_none(),
                "device {id} keeps chunk {chunk} of a deleted volume"
            );
        }
    }
}

/// The case map leases exist for (decisions.md D42): a primary cut off from
/// the map service but still reachable by clients and peers. Once its lease
/// runs out it must refuse everything, so no write it acknowledges can be
/// missing from the acting set that replaces it.
#[test]
fn a_primary_cut_off_from_the_map_service_loses_no_acknowledged_write() {
    use hyperstor_proto::{Code, Msg, Target, WireKey, block_crcs};
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let blocked = c.blocked[0].clone();
    let (cut_tx, cut_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let client = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            let old = cl.map();
            // A volume whose chunk 0 has its primary on node 0.
            let v = (1u128..)
                .find(|v| {
                    let pg = pg_of(*v, 0, old.pools[&POOL].pg_count);
                    old.devices[&old.pg(POOL, pg).unwrap().primary().unwrap()].node == 0
                })
                .unwrap();
            let block = |i: u64| pattern(1000 + i, 16 * 4096);
            cl.write(POOL, v, 0, block(0)).await.unwrap();
            cut_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            // Keep overwriting the same block through the change; remember
            // the last write that was acknowledged.
            let mut last = 0u64;
            let t = std::time::Instant::now();
            let mut i = 1u64;
            while t.elapsed() < std::time::Duration::from_millis(2500) {
                if cl.write(POOL, v, 0, block(i)).await.is_ok() {
                    last = i;
                }
                i += 1;
            }
            assert!(last > 0, "writes resumed after the change");
            assert!(cl.map().epoch > old.epoch);
            // Node 0, cut off and without a lease, refuses a read and a write
            // sent with the old map. Without leases it would still think it
            // is the primary and answer the read with data older than writes
            // the new primary has acknowledged.
            let pg = pg_of(v, 0, old.pools[&POOL].pg_count);
            let info = old.pg(POOL, pg).unwrap();
            let conn =
                hyperstor_rpc::Conn::connect(old.device_addr(info.primary().unwrap()).unwrap())
                    .await
                    .unwrap();
            let to = Target {
                pool: POOL,
                pg,
                map_epoch: old.epoch,
                pg_epoch: info.epoch,
            };
            let key = WireKey {
                ns: hyperstor_client::NS_VOL,
                name: chunk_name(v, 0),
                version: 0,
            };
            let (reply, data) = conn
                .call(
                    &Msg::Read {
                        to,
                        key: key.clone(),
                        off: 0,
                        len: 16 * 4096,
                    },
                    &[],
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    reply,
                    Msg::Error {
                        code: Code::Unavailable,
                        ..
                    }
                ),
                "the cut-off primary served a read ({} bytes, current: {}): {reply:?}",
                data.len(),
                data == block(last)
            );
            let data = block(9999);
            let (reply, _) = conn
                .call(
                    &Msg::Write {
                        to,
                        key,
                        off: 0,
                        crcs: block_crcs(&data),
                    },
                    &data,
                )
                .await
                .unwrap();
            assert!(
                matches!(reply, Msg::Error { .. }),
                "the cut-off primary accepted a write: {reply:?}"
            );
            // What the cluster holds is the last acknowledged write.
            let back = cl.read(POOL, v, 0, 16 * 4096).await.unwrap();
            assert!(
                back == block(last),
                "lost an acknowledged write (last acknowledged: {last})"
            );
            (v, last)
        })
    });
    cut_rx.recv().unwrap();
    blocked.store(true, std::sync::atomic::Ordering::Relaxed);
    go_tx.send(()).unwrap();
    let (v, last) = client.join().unwrap();
    // Node 0 reconnects: it is placed again, refilled, and serves.
    blocked.store(false, std::sync::atomic::Ordering::Relaxed);
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(500)).await;
            wait_backfilled(&cl).await;
            let back = cl.read(POOL, v, 0, 16 * 4096).await.unwrap();
            assert!(back == pattern(1000 + last, 16 * 4096));
        })
    })
    .join()
    .unwrap();
    let (map, devs) = c.stop();
    copies_agree(&map, devs, &[(v, 0)]);
}

/// Peer reports (D40): with the map service's own timeout long (3 s), a
/// node that dies is still replaced quickly, because its peers report it.
#[test]
fn peers_report_a_dead_node_before_the_map_service_notices() {
    let lease = std::time::Duration::from_millis(400);
    let mut c = Cluster::start_with(
        4,
        1,
        lease,
        std::time::Duration::from_millis(3000),
        std::time::Duration::from_millis(300),
        std::time::Duration::ZERO,
    );
    let meta = c.meta_addr;
    let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<std::time::Instant>();
    let client = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            let m = cl.map();
            let v = (1u128..)
                .find(|v| {
                    let pg = pg_of(*v, 0, m.pools[&POOL].pg_count);
                    m.devices[&m.pg(POOL, pg).unwrap().primary().unwrap()].node == 2
                })
                .unwrap();
            let data = pattern(7, 4096);
            cl.write(POOL, v, 0, data.clone()).await.unwrap();
            kill_tx.send(()).unwrap();
            let killed = done_rx.recv().unwrap();
            // Keep writing until a write succeeds after the kill.
            loop {
                if cl.write(POOL, v, 0, data.clone()).await.is_ok() {
                    return killed.elapsed();
                }
            }
        })
    });
    kill_rx.recv().unwrap();
    c.kill(2);
    done_tx.send(std::time::Instant::now()).unwrap();
    let stall = client.join().unwrap();
    eprintln!("stall with peer reports: {stall:?}");
    // Peer timeout 0.3 s, then the dead node's lease (0.4 s, plus a
    // quarter) is waited out; the map service alone would take 3 s.
    assert!(
        stall < std::time::Duration::from_millis(2000),
        "stall {stall:?}: peers didn't report the dead node"
    );
    let (map, _) = c.stop();
    assert!(!map.nodes[&2].up);
}

/// Cache pools (D45): one copy; when its node dies the copy is gone, reads
/// answer "not found" (no stale data, no zeros), and writes go on to the
/// replacement at once, with no backfill.
#[test]
fn a_cache_pool_loses_data_cleanly() {
    use hyperstor_proto::Code;
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let (kill_tx, kill_rx) = std::sync::mpsc::channel::<u32>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let client = std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            let data = pattern(3, 64 * 1024);
            cl.write(CACHE_POOL, 9, 0, data.clone()).await.unwrap();
            assert_eq!(
                cl.read(CACHE_POOL, 9, 0, data.len() as u64).await.unwrap(),
                data
            );
            // A never-written object is a miss, not zeros.
            let miss = cl.read(CACHE_POOL, 10, 0, 4096).await;
            assert!(
                matches!(
                    miss,
                    Err(hyperstor_client::ClientError::Server {
                        code: Code::NotFound,
                        ..
                    })
                ),
                "{miss:?}"
            );
            // Kill the node holding the one copy.
            let m = cl.map();
            let pg = pg_of(9, 0, m.pools[&CACHE_POOL].pg_count);
            let holder = m.devices[&m.pg(CACHE_POOL, pg).unwrap().primary().unwrap()].node;
            kill_tx.send(holder).unwrap();
            done_rx.recv().unwrap();
            // After the change: a miss, then writes and reads work again.
            let t = std::time::Instant::now();
            loop {
                match cl.read(CACHE_POOL, 9, 0, data.len() as u64).await {
                    Err(hyperstor_client::ClientError::Server {
                        code: Code::NotFound,
                        ..
                    }) => break,
                    Ok(_) => panic!("read the lost copy back"),
                    Err(_) => {}
                }
                assert!(t.elapsed().as_secs() < 10, "no miss after the node died");
            }
            let newer = pattern(4, 64 * 1024);
            cl.write(CACHE_POOL, 9, 0, newer.clone()).await.unwrap();
            assert_eq!(
                cl.read(CACHE_POOL, 9, 0, newer.len() as u64).await.unwrap(),
                newer
            );
            let m = cl.map();
            assert!(
                m.pgs
                    .iter()
                    .filter(|(k, _)| k.0 == CACHE_POOL)
                    .all(|(_, p)| p.acting == p.up)
            );
        })
    });
    let holder = kill_rx.recv().unwrap();
    c.kill(holder as usize);
    done_tx.send(()).unwrap();
    client.join().unwrap();
    // The holder returns with its old copy. Placement may give it the PG
    // back; it must not serve that copy, which is older than the write
    // made while it was away.
    c.restart(holder as usize);
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 6).await.unwrap();
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(1500)).await;
            cl.refresh_map().await.unwrap();
            let m = cl.map();
            let pg = pg_of(9, 0, m.pools[&CACHE_POOL].pg_count);
            let now = m.devices[&m.pg(CACHE_POOL, pg).unwrap().primary().unwrap()].node;
            eprintln!("cache PG served by node {now} (old holder {holder})");
            let t = std::time::Instant::now();
            loop {
                match cl.read(CACHE_POOL, 9, 0, 64 * 1024).await {
                    Ok(d) => {
                        assert!(d != pattern(3, 64 * 1024), "served the stale copy");
                        assert!(d == pattern(4, 64 * 1024), "served data never written");
                        break;
                    }
                    Err(hyperstor_client::ClientError::Server {
                        code: Code::NotFound,
                        ..
                    }) => break,
                    Err(_) => assert!(t.elapsed().as_secs() < 10, "cache pool unavailable"),
                }
            }
        })
    })
    .join()
    .unwrap();
    c.stop();
}

/// Volume leases (metadata.md §9, D75): one `rw` holder at a time, kept
/// by renewals; a planned hand-off is a detach then an attach, with no
/// blocklist; an attached volume can't be deleted; a holder hears of a
/// change to its header (grown) on a renewal.
#[test]
fn a_volume_has_one_writer_and_is_handed_off() {
    use hyperstor_client::ClientError;
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    std::thread::spawn(move || {
        block_on(async move {
            let a = Client::connect(meta, 1).await.unwrap();
            let b = Client::connect(meta, 2).await.unwrap();
            let size = 2 * CHUNK as u64;
            let id = a.create_volume(POOL, size, None, "v").await.unwrap().id;
            let va = a.attach(id).await.unwrap();
            assert_eq!(
                a.volume(id).await.unwrap().unwrap().holder,
                Some(a.instance)
            );
            assert!(matches!(
                b.attach(id).await,
                Err(ClientError::LeaseHeld { holder, remaining_ms })
                    if holder == a.instance && remaining_ms > 0
            ));
            assert!(matches!(
                b.delete_volume(id).await,
                Err(ClientError::VolumeAttached(_))
            ));
            va.write(0, pattern(1, size as usize)).await.unwrap();
            assert!(va.write(size, vec![0; 4096]).await.is_err(), "past the end");
            // Renewals keep it held well past one lease.
            let t = std::time::Instant::now();
            while t.elapsed() < hyperstor_proto::LEASE_EXPIRES * 3 / 2 {
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(500)).await;
                assert!(matches!(
                    b.attach(id).await,
                    Err(ClientError::LeaseHeld { remaining_ms, .. }) if remaining_ms > 0
                ));
            }
            assert!(!va.lost());
            // Grown: the holder sees the new size after a renewal.
            b.expand_volume(id, 2 * size).await.unwrap();
            let t = std::time::Instant::now();
            while va.volume().size != 2 * size {
                assert!(t.elapsed() < std::time::Duration::from_secs(5));
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            // Handed off: no blocklist needed.
            va.detach().await.unwrap();
            assert!(matches!(
                va.write(0, vec![0; 4096]).await,
                Err(ClientError::LeaseLost(_))
            ));
            let vb = b.attach(id).await.unwrap();
            assert!(b.map().blocklist.is_empty());
            assert_eq!(vb.read(0, size).await.unwrap(), pattern(1, size as usize));
            vb.write(size, pattern(2, size as usize)).await.unwrap();
            vb.detach().await.unwrap();
            assert_eq!(a.volume(id).await.unwrap().unwrap().holder, None);
            // Deleted while attached only by force (D80): the holder is
            // preempted first, and its I/O refused.
            let vb = b.attach(id).await.unwrap();
            assert!(matches!(
                a.delete_volume(id).await,
                Err(ClientError::VolumeAttached(_))
            ));
            a.force_delete_volume(id).await.unwrap();
            assert!(a.volume(id).await.unwrap().is_none());
            assert!(a.map().blocked(b.instance));
            assert!(vb.write(0, vec![0; 4096]).await.is_err());
            assert!(vb.lost());
        })
    })
    .join()
    .unwrap();
    c.stop();
}

/// A forced takeover (metadata.md §9, D24, D76): the holder stops renewing
/// but is still running and still writing. The new holder waits out its
/// lease, blocklists it, and takes the lease; from then on every storage
/// node refuses the old instance, so none of its writes lands after the
/// new holder's. A preemption does the same to a holder that is renewing.
#[test]
fn a_forced_takeover_blocklists_the_old_holder() {
    use hyperstor_client::ClientError;
    use hyperstor_proto::Code;
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    std::thread::spawn(move || {
        block_on(async move {
            let a = Client::connect(meta, 1).await.unwrap();
            let b = Client::connect(meta, 2).await.unwrap();
            let size = 2 * CHUNK as u64;
            let v = a.create_volume(POOL, size, None, "v").await.unwrap();
            let va = a.attach(v.id).await.unwrap();
            va.write(0, pattern(1, size as usize)).await.unwrap();
            // A stops renewing (its handle is gone) but keeps writing.
            drop(va);
            let (a2, lineage) = (a.clone(), v.lineage);
            let writer = hyperstor_reactor::spawn(async move {
                let (mut n, t) = (0u64, std::time::Instant::now());
                while t.elapsed() < std::time::Duration::from_secs(15) {
                    match a2.write(POOL, lineage, 0, pattern(100 + n, 4096)).await {
                        Ok(()) => n += 1,
                        Err(e) => return (n, Some(e)),
                    }
                    hyperstor_reactor::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                (n, None)
            });
            let t = std::time::Instant::now();
            let vb = b.take_over(v.id).await.unwrap();
            let took = t.elapsed();
            assert!(
                took >= std::time::Duration::from_secs(2)
                    && took < hyperstor_proto::LEASE_EXPIRES * 2,
                "took {took:?}"
            );
            assert!(b.map().blocked(a.instance));
            let (n, e) = writer.await;
            assert!(n > 0, "the old holder was writing until the takeover");
            assert!(
                matches!(
                    e,
                    Some(ClientError::Server {
                        code: Code::Blocklisted,
                        ..
                    })
                ),
                "{e:?}"
            );
            // The new holder's data stays: nothing from A lands later.
            vb.write(0, pattern(2, size as usize)).await.unwrap();
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(200)).await;
            assert_eq!(vb.read(0, size).await.unwrap(), pattern(2, size as usize));
            assert!(a.attach(v.id).await.is_err(), "blocklisted");
            // Take-over leaves a renewing holder alone; preemption doesn't.
            let c3 = Client::connect(meta, 3).await.unwrap();
            assert!(matches!(
                c3.take_over(v.id).await,
                Err(ClientError::LeaseHeld { remaining_ms, .. }) if remaining_ms > 0
            ));
            assert!(!vb.lost());
            let vc = c3.preempt(v.id).await.unwrap();
            assert!(c3.map().blocked(b.instance));
            vc.write(0, pattern(3, 4096)).await.unwrap();
            assert!(vb.write(0, pattern(4, 4096)).await.is_err());
            assert!(vb.lost(), "a blocklisted holder knows it lost the lease");
            assert_eq!(vc.read(0, 4096).await.unwrap(), pattern(3, 4096));
        })
    })
    .join()
    .unwrap();
    c.stop();
}

/// A lease outlives its header's primary: the new primary gives the holder
/// a full lease from when it first sees it, and the holder's renewals
/// follow the map to it.
#[test]
fn a_lease_outlives_its_headers_primary() {
    use hyperstor_client::ClientError;
    use hyperstor_client::volume::header_key;
    let mut c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let (tx, mut rx) = channel::<usize>();
    let (go_tx, mut go_rx) = channel::<()>();
    let t = std::thread::spawn(move || {
        block_on(async move {
            let a = Client::connect(meta, 1).await.unwrap();
            let b = Client::connect(meta, 2).await.unwrap();
            let id = a
                .create_volume(POOL, CHUNK as u64, None, "v")
                .await
                .unwrap()
                .id;
            let va = a.attach(id).await.unwrap();
            let map = a.map();
            let pgs = map.pools[&hyperstor_meta::META_POOL].pg_count;
            let pg = hyperstor_client::object_pg(&header_key(id), pgs);
            let primary = map
                .pg(hyperstor_meta::META_POOL, pg)
                .unwrap()
                .primary()
                .unwrap();
            tx.send((primary / 1000) as usize).unwrap();
            go_rx.recv().await;
            // The PG has a new primary now.
            let t = std::time::Instant::now();
            loop {
                let _ = b.refresh_map().await;
                if b.map().pg(hyperstor_meta::META_POOL, pg).unwrap().primary() != Some(primary) {
                    break;
                }
                assert!(t.elapsed() < std::time::Duration::from_secs(5));
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let t = std::time::Instant::now();
            while t.elapsed() < hyperstor_proto::LEASE_EXPIRES * 3 / 2 {
                assert!(matches!(
                    b.attach(id).await,
                    Err(ClientError::LeaseHeld { remaining_ms, .. }) if remaining_ms > 0
                ));
                hyperstor_reactor::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            assert!(!va.lost());
            va.write(0, pattern(5, 4096)).await.unwrap();
        })
    });
    let node = block_on(async move { rx.recv().await.unwrap() });
    c.kill(node);
    go_tx.send(()).unwrap();
    t.join().unwrap();
    c.stop();
}

/// Runs `f` with a client of its own, on a reactor of its own.
fn with_client<T: Send + 'static, F, Fut>(meta: SocketAddr, id: u64, f: F) -> T
where
    F: FnOnce(std::rc::Rc<Client>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
{
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, id).await.unwrap();
            f(cl).await
        })
    })
    .join()
    .unwrap()
}

/// Waits until `ok` holds for the map the service publishes.
async fn wait_map(cl: &std::rc::Rc<Client>, what: &str, ok: impl Fn(&ClusterMap) -> bool) {
    let t = std::time::Instant::now();
    loop {
        cl.refresh_map().await.unwrap();
        if ok(&cl.map()) {
            return;
        }
        assert!(t.elapsed().as_secs() < 30, "waited 30 s for {what}");
        hyperstor_reactor::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Down and out (placement.md §5.3): a node down for less than the
/// down-out interval keeps its place, so a short reboot moves no data;
/// one down for longer has its devices marked out and its PGs placed
/// elsewhere, and is marked in again when it returns.
#[test]
fn a_node_down_past_the_interval_is_marked_out_and_back_in() {
    use std::time::Duration;
    let mut c = Cluster::start_full(
        4,
        1,
        LEASE,
        DOWN_AFTER,
        Duration::ZERO,
        Duration::ZERO,
        Duration::from_secs(3),
    );
    let meta = c.meta_addr;
    let data = pattern(91, 8 * CHUNK as usize);
    let d = data.clone();
    let before = with_client(meta, 5, move |cl| async move {
        cl.write(POOL, 1, 0, d).await.unwrap();
        wait_backfilled(&cl).await;
        cl.map().as_ref().clone()
    });
    // The data pool's: a cache pool follows live nodes (D45).
    let places = |m: &ClusterMap| -> Vec<Vec<u32>> {
        m.pgs
            .range((POOL, 0)..=(POOL, u32::MAX))
            .map(|(_, p)| p.up.clone())
            .collect()
    };
    let at_first = places(&before);
    // A short stop: down, never out; nothing placed elsewhere.
    c.kill(3);
    let at_first2 = at_first.clone();
    with_client(meta, 6, move |cl| async move {
        wait_map(&cl, "node 3 down", |m| !m.nodes[&3].up).await;
        hyperstor_reactor::time::sleep(Duration::from_millis(500)).await;
        cl.refresh_map().await.unwrap();
        let m = cl.map();
        assert!(m.auto_out.is_empty());
        assert_eq!(places(&m), at_first2, "a down node keeps its place");
        assert!(
            m.pgs
                .range((POOL, 0)..=(POOL, u32::MAX))
                .all(|(_, p)| m.filling(p).is_empty()),
            "nothing to fill"
        );
        // Writes go on with the other copies.
        cl.write(POOL, 1, 0, pattern(92, CHUNK as usize))
            .await
            .unwrap();
    });
    c.restart(3);
    let at_first3 = at_first.clone();
    with_client(meta, 7, move |cl| async move {
        wait_map(&cl, "node 3 up", |m| m.nodes[&3].up).await;
        wait_backfilled(&cl).await;
        assert_eq!(places(&cl.map()), at_first3);
    });
    // A long stop: out after the interval, its PGs placed and filled
    // elsewhere; back in when it returns.
    c.kill(3);
    with_client(meta, 8, move |cl| async move {
        wait_map(&cl, "node 3 out", |m| m.auto_out.contains(&3000)).await;
        let m = cl.map();
        assert!(!m.devices[&3000].up_in);
        assert!(
            m.pgs
                .range((POOL, 0)..=(POOL, u32::MAX))
                .all(|(_, p)| !p.up.contains(&3000))
        );
        wait_backfilled(&cl).await;
    });
    c.restart(3);
    with_client(meta, 9, move |cl| async move {
        wait_map(&cl, "node 3 back in", |m| {
            m.auto_out.is_empty() && m.devices[&3000].up_in
        })
        .await;
        wait_backfilled(&cl).await;
        assert!(cl.map().pgs.values().any(|p| p.acting.contains(&3000)));
    });
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..8u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// A device that fails (replication.md §15) is marked failed and its PGs
/// are placed elsewhere at once, without waiting for the down-out
/// interval; its node stays up, and writes go on.
#[test]
fn a_failed_device_is_placed_elsewhere_at_once() {
    use std::time::Duration;
    let c = Cluster::start_full(
        4,
        1,
        LEASE,
        DOWN_AFTER,
        Duration::ZERO,
        Duration::ZERO,
        Duration::from_secs(600),
    );
    let meta = c.meta_addr;
    let data = pattern(93, 8 * CHUNK as usize);
    let d = data.clone();
    with_client(meta, 5, move |cl| async move {
        cl.write(POOL, 1, 0, d).await.unwrap();
        wait_backfilled(&cl).await;
    });
    c.faulty[&2000].set_failed(true);
    let d = pattern(94, 8 * CHUNK as usize);
    let d2 = d.clone();
    with_client(meta, 6, move |cl| async move {
        // Writes to PGs on the failed device fail it; then go on elsewhere.
        cl.write(POOL, 1, 0, d2.clone()).await.unwrap();
        wait_map(&cl, "device 2000 failed", |m| m.failed.contains(&2000)).await;
        let m = cl.map();
        assert!(m.nodes[&2].up, "the node is up; only its device failed");
        assert!(
            m.pgs
                .values()
                .all(|p| !p.up.contains(&2000) && !p.acting.contains(&2000))
        );
        wait_backfilled(&cl).await;
        assert!(cl.read(POOL, 1, 0, d2.len() as u64).await.unwrap() == d2);
    });
    // Readable again for the final check, which opens every device.
    c.faulty[&2000].set_failed(false);
    let (map, devs) = c.stop();
    assert!(map.failed.contains(&2000));
    let chunks: Vec<(u128, u64)> = (0..8u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
    let _ = data;
}

/// A replica whose disk hangs while its node stays up (D91): the primary
/// waits the operation timeout, reports it, and the map service takes it
/// out of the PG, so writes go on; once the disk answers again it is
/// backfilled back.
#[test]
fn a_hung_replica_is_dropped_from_its_pg_and_comes_back() {
    let c = Cluster::start(4, 2);
    let meta = c.meta_addr;
    let (pg, slow) = with_client(meta, 5, move |cl| async move {
        cl.write(POOL, 1, 0, pattern(95, 4 * CHUNK as usize))
            .await
            .unwrap();
        wait_backfilled(&cl).await;
        let m = cl.map();
        let pg = pg_of(1, 0, m.pools[&POOL].pg_count);
        (pg, m.pg(POOL, pg).unwrap().acting[2])
    });
    c.faulty[&slow].set_delay(std::time::Duration::from_secs(3));
    let t = std::time::Instant::now();
    let d = pattern(96, CHUNK as usize);
    let d2 = d.clone();
    with_client(meta, 6, move |cl| async move {
        cl.write(POOL, 1, 0, d2).await.unwrap();
        let m = cl.map();
        let info = m.pg(POOL, pg).unwrap();
        assert!(!info.acting.contains(&slow), "the slow member left the PG");
        assert!(info.up.contains(&slow), "and will be filled back");
        assert!(m.nodes[&(slow / 1000)].up, "its node stayed up");
    });
    assert!(
        t.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        t.elapsed()
    );
    c.faulty[&slow].set_delay(std::time::Duration::ZERO);
    with_client(meta, 7, move |cl| async move {
        wait_map(&cl, "the slow member back", |m| {
            m.pg(POOL, pg).unwrap().acting.contains(&slow)
        })
        .await;
        assert!(cl.read(POOL, 1, 0, d.len() as u64).await.unwrap() == d);
    });
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..4u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// A partition between a primary and one replica only (D91): neither
/// node is down for anyone else, so liveness and peer reports never act;
/// the primary reports the member it can't reach, and the PG goes on
/// without it until the partition heals.
#[test]
fn a_partition_between_two_nodes_is_worked_around() {
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let (pg, primary, cut) = with_client(meta, 5, move |cl| async move {
        cl.write(POOL, 1, 0, pattern(97, 4 * CHUNK as usize))
            .await
            .unwrap();
        wait_backfilled(&cl).await;
        let m = cl.map();
        let pg = pg_of(1, 0, m.pools[&POOL].pg_count);
        let info = m.pg(POOL, pg).unwrap();
        (pg, info.acting[0], info.acting[1])
    });
    c.partitions.cut(primary / 1000, cut / 1000);
    let d = pattern(98, CHUNK as usize);
    let d2 = d.clone();
    with_client(meta, 6, move |cl| async move {
        cl.write(POOL, 1, 0, d2).await.unwrap();
        let m = cl.map();
        assert!(!m.pg(POOL, pg).unwrap().acting.contains(&cut));
        assert!(m.nodes.values().all(|n| n.up));
    });
    c.partitions.heal_all();
    with_client(meta, 7, move |cl| async move {
        wait_map(&cl, "the cut member back", |m| {
            m.pg(POOL, pg).unwrap().acting.contains(&cut)
        })
        .await;
        assert!(cl.read(POOL, 1, 0, d.len() as u64).await.unwrap() == d);
    });
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..4u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// The primary's own copy fails its checksums (replication.md §8.2): the
/// read is served from a member's copy, which replaces the primary's.
#[test]
fn a_read_of_a_damaged_primary_copy_is_served_and_repaired() {
    use std::time::Duration;
    let mut c = Cluster::start_full(
        4,
        1,
        LEASE,
        DOWN_AFTER,
        Duration::ZERO,
        Duration::ZERO,
        Duration::from_secs(600),
    );
    let meta = c.meta_addr;
    let v1 = pattern(101, 4 * CHUNK as usize);
    let v1c = v1.clone();
    let primary = with_client(meta, 5, move |cl| async move {
        cl.write(POOL, 1, 0, v1c).await.unwrap();
        // Later commits everywhere, so chunk 0's isn't any device's last
        // group (which recovery would drop as torn, I4).
        cl.write(POOL, 2, 0, pattern(102, 32 * CHUNK as usize))
            .await
            .unwrap();
        wait_backfilled(&cl).await;
        let m = cl.map();
        m.pg(POOL, pg_of(1, 0, m.pools[&POOL].pg_count))
            .unwrap()
            .acting[0]
    });
    let node = (primary / 1000) as usize;
    c.kill(node);
    let dev = c.stopped.get_mut(&node).unwrap();
    let mut img = vec![0u8; dev[0].len() as usize];
    for (i, part) in img.chunks_mut(1 << 20).enumerate() {
        dev[0].read((i as u64) << 20, part).unwrap();
    }
    let at = img
        .chunks(4096)
        .position(|b| b == &v1[..4096])
        .expect("chunk 0's first block on the device")
        * 4096;
    img[at + 100] ^= 0xff;
    dev[0] = Arc::new(MemDev::from_image(img));
    c.restart(node);
    let v1r = v1.clone();
    with_client(meta, 6, move |cl| async move {
        // Back as the PG's primary (its index looks right to digests).
        wait_map(&cl, "the damaged node primary again", |m| {
            let pg = m.pg(POOL, pg_of(1, 0, m.pools[&POOL].pg_count)).unwrap();
            pg.primary() == Some(primary) && pg.acting == pg.up
        })
        .await;
        assert!(cl.read(POOL, 1, 0, v1r.len() as u64).await.unwrap() == v1r);
    });
    // Every copy reads back through the CRC check, the primary's included.
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..4u64).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// Scrub when the primary's copy is the one outvoted (replication.md
/// §8.5): it pulls the copy the other two agree on. When all three
/// differ, nothing decides: the PG is reported inconsistent to the map
/// service, for cluster health, until a scrub finds it consistent.
#[test]
fn scrub_repairs_an_outvoted_primary_and_reports_what_it_cant() {
    use hyperstor_proto::{Msg, Target, WireKey, block_crcs};
    let c = Cluster::start_with(
        4,
        1,
        LEASE,
        DOWN_AFTER,
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(300),
    );
    let meta = c.meta_addr;
    let v1 = pattern(103, 4 * CHUNK as usize);
    let v1c = v1.clone();
    // Overwrites chunk `ch`'s copy on `devices` with distinct data.
    async fn overwrite(cl: &std::rc::Rc<Client>, ch: u64, devices: &[u32]) {
        let m = cl.map();
        let pg = pg_of(1, ch, m.pools[&POOL].pg_count);
        let info = m.pg(POOL, pg).unwrap();
        for (i, d) in devices.iter().enumerate() {
            let conn = hyperstor_rpc::Conn::connect(m.device_addr(*d).unwrap())
                .await
                .unwrap();
            let bad = pattern(200 + ch * 10 + i as u64, CHUNK as usize);
            let (reply, _) = conn
                .call(
                    &Msg::Push {
                        to: Target {
                            pool: POOL,
                            pg,
                            map_epoch: m.epoch,
                            pg_epoch: info.epoch,
                        },
                        key: WireKey {
                            ns: hyperstor_client::NS_VOL,
                            name: chunk_name(1, ch),
                            version: 0,
                        },
                        crcs: block_crcs(&bad),
                    },
                    &bad,
                )
                .await
                .unwrap();
            assert_eq!(reply, Msg::PushOk);
        }
    }
    let (primary, pg2) = with_client(meta, 5, move |cl| async move {
        cl.write(POOL, 1, 0, v1c).await.unwrap();
        wait_backfilled(&cl).await;
        let m = cl.map();
        let pgc = m.pools[&POOL].pg_count;
        let pg0 = m.pg(POOL, pg_of(1, 0, pgc)).unwrap().clone();
        // The primary's copy of chunk 0 outvoted by the two others.
        overwrite(&cl, 0, &[pg0.acting[0]]).await;
        // Every copy of chunk 2 different (if it is in another PG).
        let pg2 = pg_of(1, 2, pgc);
        let info2 = m.pg(POOL, pg2).unwrap().clone();
        overwrite(&cl, 2, &info2.acting).await;
        (pg0.acting[0], pg2)
    });
    let repaired = || {
        c.nodes[(primary / 1000) as usize].as_ref().unwrap().metrics[0]
            .1
            .scrub_repaired
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let t = std::time::Instant::now();
    while repaired() == 0 || !c.inconsistent().contains_key(&(POOL, pg2)) {
        assert!(
            t.elapsed().as_secs() < 15,
            "repaired {}, inconsistent {:?}",
            repaired(),
            c.inconsistent()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let v1r = v1.clone();
    with_client(meta, 6, move |cl| async move {
        let chunk0 = cl.read(POOL, 1, 0, CHUNK as u64).await.unwrap();
        assert!(chunk0 == v1r[..CHUNK as usize], "the majority's copy");
        // Rewritten by a client: consistent again, and reported so.
        cl.write(POOL, 1, 2 * CHUNK as u64, pattern(104, CHUNK as usize))
            .await
            .unwrap();
    });
    let t = std::time::Instant::now();
    while c.inconsistent().contains_key(&(POOL, pg2)) {
        assert!(t.elapsed().as_secs() < 15, "still reported inconsistent");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let (map, devs) = c.stop();
    copies_agree(&map, devs, &[(1, 0), (1, 1), (1, 2), (1, 3)]);
}

/// The fault-injection rig (D92): a randomized run of writes while nodes
/// stop and restart, pairs of nodes are cut from each other, and devices
/// hang, against a model. A write that fails may or may not have landed,
/// so each chunk keeps every value it may hold; after everything heals,
/// every chunk must read back one of them, and every copy must agree.
/// `HS_SEED` picks one seed; by default three run.
#[test]
fn random_faults_lose_no_acknowledged_write() {
    let seeds: Vec<u64> = match std::env::var("HS_SEED") {
        Ok(s) => vec![s.parse().unwrap()],
        Err(_) => vec![1, 2, 3],
    };
    for seed in seeds {
        random_faults(seed);
    }
}

fn random_faults(seed: u64) {
    use std::time::Duration;
    const CHUNKS: u64 = 8;
    const NODES: u32 = 5;
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut c = Cluster::start(NODES, 2);
    let meta = c.meta_addr;
    let first: Vec<Vec<u8>> = (0..CHUNKS)
        .map(|ch| pattern(seed * 1000 + 500 + ch, CHUNK as usize))
        .collect();
    let all = first.concat();
    with_client(meta, 5, move |cl| async move {
        cl.write(POOL, 1, 0, all).await.unwrap();
        wait_backfilled(&cl).await;
    });
    // Every value each chunk may hold.
    let mut model: Vec<Vec<Vec<u8>>> = first.into_iter().map(|v| vec![v]).collect();
    let (mut down, mut cut, mut slow) = (None::<usize>, None::<(u32, u32)>, None::<u32>);
    let mut log = Vec::new();
    for step in 0..12u64 {
        match rng.random_range(0..7u32) {
            0 if down.is_none() => {
                let n = rng.random_range(0..NODES as usize);
                c.kill(n);
                down = Some(n);
                log.push(format!("kill {n}"));
            }
            1 if down.is_some() => {
                let n = down.take().unwrap();
                c.restart(n);
                log.push(format!("restart {n}"));
            }
            2 => match cut.take() {
                None => {
                    let a = rng.random_range(0..NODES);
                    let b = (a + rng.random_range(1..NODES)) % NODES;
                    c.partitions.cut(a, b);
                    cut = Some((a, b));
                    log.push(format!("cut {a}-{b}"));
                }
                Some((a, b)) => {
                    c.partitions.heal(a, b);
                    log.push(format!("heal {a}-{b}"));
                }
            },
            3 => match slow.take() {
                None => {
                    let d = rng.random_range(0..NODES) * 1000 + rng.random_range(0..2u32);
                    c.faulty[&d].set_delay(Duration::from_millis(2500));
                    slow = Some(d);
                    log.push(format!("slow {d}"));
                }
                Some(d) => {
                    c.faulty[&d].set_delay(Duration::ZERO);
                    log.push(format!("fast {d}"));
                }
            },
            _ => {
                let ch = rng.random_range(0..CHUNKS);
                let data = pattern(seed * 1000 + step, CHUNK as usize);
                let d = data.clone();
                let ok = with_client(meta, 10 + step, move |cl| async move {
                    cl.write(POOL, 1, ch * CHUNK as u64, d).await.is_ok()
                });
                if ok {
                    model[ch as usize] = vec![data];
                } else {
                    model[ch as usize].push(data);
                }
                log.push(format!("write {ch}: {}", if ok { "ok" } else { "failed" }));
            }
        }
    }
    c.partitions.heal_all();
    if let Some(d) = slow {
        c.faulty[&d].set_delay(Duration::ZERO);
    }
    if let Some(n) = down {
        c.restart(n);
    }
    let read = with_client(meta, 99, move |cl| async move {
        hyperstor_reactor::time::sleep(Duration::from_millis(800)).await;
        wait_backfilled(&cl).await;
        cl.read(POOL, 1, 0, CHUNKS * CHUNK as u64).await
    });
    let read = read.unwrap_or_else(|e| panic!("seed {seed}: final read failed after {log:?}: {e}"));
    for (ch, may) in model.iter().enumerate() {
        let got = &read[ch * CHUNK as usize..(ch + 1) * CHUNK as usize];
        assert!(
            may.iter().any(|v| v[..] == got[..]),
            "seed {seed}: chunk {ch} holds none of its {} possible values after {log:?}",
            may.len()
        );
    }
    eprintln!("random faults, seed {seed}: {log:?}");
    let (map, devs) = c.stop();
    let chunks: Vec<(u128, u64)> = (0..CHUNKS).map(|c| (1, c)).collect();
    copies_agree(&map, devs, &chunks);
}

/// Snapshots, clones, flatten, rollback and their deletion (volumes.md
/// §10.3, §10.4; D101–D104): every snapshot, clone and head reads what it
/// should at every step, a snapshot a clone depends on can't be deleted
/// until the clone is flattened, and deleting snapshots merges their
/// versions, so each chunk ends with one version on every device.
#[test]
fn snapshots_clones_and_rollback_read_what_they_should() {
    use hyperstor_client::ClientError;
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let size = 2 * CHUNK as u64;
    let (lineage, ids) = with_client(meta, 5, move |cl| async move {
        let len = size as usize;
        let v = cl.create_volume(POOL, size, None, "v").await.unwrap();
        let va = cl.attach(v.id).await.unwrap();
        let mut model = pattern(1, len);
        va.write(0, model.clone()).await.unwrap();
        // Snapshot while attached: the holder quiesces (D103).
        let s1 = cl.snapshot(v.id, "s1").await.unwrap();
        let at_s1 = model.clone();
        let b = pattern(2, 3 * 4096);
        va.write(CHUNK as u64 - 4096, b.clone()).await.unwrap();
        model[CHUNK as usize - 4096..CHUNK as usize + 8192].copy_from_slice(&b);
        let s2 = cl.snapshot(v.id, "s2").await.unwrap();
        let at_s2 = model.clone();
        let d = pattern(3, 4096);
        va.write(8192, d.clone()).await.unwrap();
        model[8192..12288].copy_from_slice(&d);
        let read_snap = |vol: hyperstor_client::volume::Volume, e: u64| {
            let cl = cl.clone();
            async move {
                let s = vol.snapshot(e).unwrap().clone();
                cl.read_at(POOL, vol.lineage, &vol.snapshot_layers(&s), 0, size)
                    .await
                    .unwrap()
            }
        };
        let vol = cl.volume(v.id).await.unwrap().unwrap();
        assert_eq!(vol.head_epoch, s2.epoch + 1);
        assert!(read_snap(vol.clone(), s1.epoch).await == at_s1, "s1");
        assert!(read_snap(vol.clone(), s2.epoch).await == at_s2, "s2");
        assert!(va.read(0, size).await.unwrap() == model, "head");
        // A clone of s1 reads s1, and diverges on its own.
        let k = cl.clone_snapshot(v.id, s1.epoch, "k", None).await.unwrap();
        assert_eq!(k.lineage, v.lineage);
        let ka = cl.attach(k.id).await.unwrap();
        assert!(ka.read(0, size).await.unwrap() == at_s1, "clone");
        let mut kmodel = at_s1.clone();
        let e = pattern(4, 8192);
        ka.write(CHUNK as u64, e.clone()).await.unwrap();
        kmodel[CHUNK as usize..CHUNK as usize + 8192].copy_from_slice(&e);
        assert!(ka.read(0, size).await.unwrap() == kmodel);
        assert!(
            va.read(0, size).await.unwrap() == model,
            "the parent's head"
        );
        assert!(matches!(
            cl.delete_snapshot(v.id, s1.epoch).await,
            Err(ClientError::SnapshotInUse(_))
        ));
        assert!(matches!(
            cl.delete_volume(v.id).await,
            Err(ClientError::VolumeAttached(_) | ClientError::SnapshotInUse(_))
        ));
        // Flattened: the clone no longer depends on s1.
        let kf = cl.flatten(k.id).await.unwrap();
        assert!(kf.chain.is_empty());
        assert!(ka.read(0, size).await.unwrap() == kmodel, "flattened clone");
        cl.delete_snapshot(v.id, s1.epoch).await.unwrap();
        let vol = cl.volume(v.id).await.unwrap().unwrap();
        assert!(vol.snapshot(s1.epoch).is_none());
        assert!(
            read_snap(vol.clone(), s2.epoch).await == at_s2,
            "s2 after s1 went"
        );
        assert!(
            va.read(0, size).await.unwrap() == model,
            "head after s1 went"
        );
        assert!(
            ka.read(0, size).await.unwrap() == kmodel,
            "clone after s1 went"
        );
        // Rollback to s2: detached only; then the head reads s2.
        assert!(matches!(
            cl.rollback(v.id, s2.epoch).await,
            Err(ClientError::VolumeAttached(_))
        ));
        va.detach().await.unwrap();
        // To the newest snapshot: in place (D106), no new branch.
        let r = cl.rollback(v.id, s2.epoch).await.unwrap();
        assert_eq!((r.branch, r.chain.len()), (vol.branch, 0));
        assert!(r.skip.is_empty(), "the skipped epochs were trimmed");
        let va = cl.attach(v.id).await.unwrap();
        assert!(va.read(0, size).await.unwrap() == at_s2, "rolled back");
        let f = pattern(5, 4096);
        va.write(0, f.clone()).await.unwrap();
        let mut after = at_s2.clone();
        after[..4096].copy_from_slice(&f);
        assert!(va.read(0, size).await.unwrap() == after);
        let vol = cl.volume(v.id).await.unwrap().unwrap();
        assert!(
            read_snap(vol.clone(), s2.epoch).await == at_s2,
            "s2 after the rollback"
        );
        cl.delete_snapshot(v.id, s2.epoch).await.unwrap();
        assert!(
            va.read(0, size).await.unwrap() == after,
            "head after every snapshot went"
        );
        va.detach().await.unwrap();
        ka.detach().await.unwrap();
        (v.lineage, (v.id, k.id))
    });
    let (map, devs) = c.stop();
    // One version per chunk and branch on every device: deleting the
    // snapshots merged or dropped the rest.
    let pool = &map.pools[&POOL];
    for (id, d) in devs {
        let sh = hyperstor_engine::open(&copy(&d)).unwrap().1.pop().unwrap();
        for chunk in 0..2u64 {
            let pg = pg_of(lineage, chunk, pool.pg_count);
            let lo = ObjKey::new(POOL, pg, Namespace::Vol, chunk_name(lineage, chunk), 0);
            let mut hi = chunk_name(lineage, chunk);
            hi.extend_from_slice(&[0xff; 5]);
            let hi = ObjKey::new(POOL, pg, Namespace::Vol, hi, 0);
            let mut per_name: std::collections::BTreeMap<Vec<u8>, usize> = Default::default();
            for k in sh.list(&lo, &hi) {
                *per_name.entry(k.name).or_default() += 1;
            }
            assert!(
                per_name.values().all(|n| *n == 1),
                "device {id}, chunk {chunk}: {per_name:?}"
            );
        }
    }
    let _ = ids;
}

/// A snapshot of a volume whose holder doesn't answer (it stopped
/// renewing and watching, but its lease stands) is abandoned after 2 s,
/// and changes nothing (volumes.md §10.3).
#[test]
fn a_snapshot_whose_holder_doesnt_quiesce_is_abandoned() {
    use hyperstor_client::ClientError;
    use hyperstor_proto::Code;
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    with_client(meta, 5, move |cl| async move {
        let v = cl
            .create_volume(POOL, CHUNK as u64, None, "v")
            .await
            .unwrap();
        drop(cl.attach(v.id).await.unwrap());
        let other = Client::connect(meta, 6).await.unwrap();
        let t = std::time::Instant::now();
        let r = other.snapshot(v.id, "s").await;
        assert!(
            matches!(
                r,
                Err(ClientError::Server {
                    code: Code::Busy,
                    ..
                })
            ),
            "{r:?}"
        );
        assert!(t.elapsed() >= std::time::Duration::from_secs(2));
        let after = other.volume(v.id).await.unwrap().unwrap();
        assert!(after.snapshots.is_empty() && after.head_epoch == v.head_epoch);
    });
    c.stop();
}

/// Random snapshot, clone, rollback, flatten and delete operations on a
/// family of volumes, with writes in between, against a model: every
/// head and every snapshot must read back what the model holds after
/// every step. `HS_SEED` picks one seed; by default three run.
#[test]
fn random_snapshot_operations_match_a_model() {
    let seeds: Vec<u64> = match std::env::var("HS_SEED") {
        Ok(s) => vec![s.parse().unwrap()],
        Err(_) => vec![1, 2, 3],
    };
    for seed in seeds {
        random_snapshot_ops(seed);
    }
}

fn random_snapshot_ops(seed: u64) {
    use std::collections::BTreeMap;
    const BLOCKS: u64 = 16;
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    with_client(meta, 5, move |cl| async move {
        let mut rng = SmallRng::seed_from_u64(seed);
        // Small chunks would be nicer; whole blocks across two chunks do.
        let size = 2 * CHUNK as u64;
        let base = cl.create_volume(POOL, size, None, "base").await.unwrap();
        // Per volume: its model (block -> fill) and its snapshots' models.
        let mut heads: BTreeMap<u128, BTreeMap<u64, u8>> = BTreeMap::new();
        let mut snaps: BTreeMap<u128, BTreeMap<u64, BTreeMap<u64, u8>>> = BTreeMap::new();
        heads.insert(base.id, BTreeMap::new());
        snaps.insert(base.id, BTreeMap::new());
        let mut log: Vec<String> = Vec::new();
        // Blocks spread over both chunks.
        let block_off = |b: u64| (b % 2) * CHUNK as u64 + (b / 2) * 4096;
        let check = |cl: std::rc::Rc<Client>,
                     heads: BTreeMap<u128, BTreeMap<u64, u8>>,
                     snaps: BTreeMap<u128, BTreeMap<u64, BTreeMap<u64, u8>>>,
                     log: Vec<String>| async move {
            for (id, m) in &heads {
                let v = cl.volume(*id).await.unwrap().unwrap();
                let got = cl
                    .read_at(POOL, v.lineage, &v.layers(), 0, size)
                    .await
                    .unwrap();
                for b in 0..BLOCKS {
                    let at = block_off(b) as usize;
                    let want = m.get(&b).copied().unwrap_or(0);
                    assert!(
                        got[at..at + 4096].iter().all(|x| *x == want),
                        "seed {seed}: volume {id:x} block {b} isn't {want} after {log:?}"
                    );
                }
                for (e, sm) in &snaps[id] {
                    let s = v.snapshot(*e).unwrap().clone();
                    let got = cl
                        .read_at(POOL, v.lineage, &v.snapshot_layers(&s), 0, size)
                        .await
                        .unwrap();
                    for b in 0..BLOCKS {
                        let at = block_off(b) as usize;
                        let want = sm.get(&b).copied().unwrap_or(0);
                        assert!(
                            got[at..at + 4096].iter().all(|x| *x == want),
                            "seed {seed}: volume {id:x} snapshot {e} block {b} isn't {want} after {log:?}"
                        );
                    }
                }
            }
        };
        for step in 0..60u32 {
            let ids: Vec<u128> = heads.keys().copied().collect();
            let id = ids[rng.random_range(0..ids.len())];
            match rng.random_range(0..10u32) {
                0..=3 => {
                    let b = rng.random_range(0..BLOCKS);
                    let fill = rng.random_range(1..=255u8);
                    let a = cl.attach(id).await.unwrap();
                    a.write(block_off(b), vec![fill; 4096]).await.unwrap();
                    a.detach().await.unwrap();
                    heads.get_mut(&id).unwrap().insert(b, fill);
                    log.push(format!("write {id:x} {b}={fill}"));
                }
                4 | 5 => {
                    let s = cl.snapshot(id, "s").await.unwrap();
                    let m = heads[&id].clone();
                    snaps.get_mut(&id).unwrap().insert(s.epoch, m);
                    log.push(format!("snapshot {id:x} @{}", s.epoch));
                }
                6 => {
                    let es: Vec<u64> = snaps[&id].keys().copied().collect();
                    if let Some(e) = es.get(rng.random_range(0..es.len().max(1))) {
                        let k = cl.clone_snapshot(id, *e, "k", None).await.unwrap();
                        heads.insert(k.id, snaps[&id][e].clone());
                        snaps.insert(k.id, BTreeMap::new());
                        log.push(format!("clone {id:x} @{e} -> {:x}", k.id));
                    }
                }
                7 => {
                    let es: Vec<u64> = snaps[&id].keys().copied().collect();
                    if let Some(e) = es.get(rng.random_range(0..es.len().max(1))) {
                        cl.rollback(id, *e).await.unwrap();
                        let m = snaps[&id][e].clone();
                        heads.insert(id, m);
                        log.push(format!("rollback {id:x} @{e}"));
                    }
                }
                8 => {
                    let es: Vec<u64> = snaps[&id].keys().copied().collect();
                    if let Some(e) = es.get(rng.random_range(0..es.len().max(1))) {
                        match cl.delete_snapshot(id, *e).await {
                            Ok(()) => {
                                snaps.get_mut(&id).unwrap().remove(e);
                                log.push(format!("delete {id:x} @{e}"));
                            }
                            Err(hyperstor_client::ClientError::SnapshotInUse(_)) => {
                                log.push(format!("delete {id:x} @{e}: in use"));
                            }
                            Err(err) => panic!("seed {seed}: {err} after {log:?}"),
                        }
                    }
                }
                _ => match cl.flatten(id).await {
                    Ok(_) => log.push(format!("flatten {id:x}")),
                    Err(hyperstor_client::ClientError::Refused(_)) => {
                        log.push(format!("flatten {id:x}: refused"))
                    }
                    Err(err) => panic!("seed {seed}: {err} after {log:?}"),
                },
            }
            if step % 5 == 4 {
                check(cl.clone(), heads.clone(), snaps.clone(), log.clone()).await;
            }
        }
        check(cl.clone(), heads.clone(), snaps.clone(), log.clone()).await;
        eprintln!("snapshot operations, seed {seed}: {log:?}");
    });
    c.stop();
}

/// Quiesce (D103, volumes.md §10.3): snapshots taken while the volume is
/// written without pause never change afterwards, and each holds a value
/// the writer had written.
#[test]
fn a_snapshot_never_changes_while_its_volume_is_written() {
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    with_client(meta, 5, move |cl| async move {
        let v = cl
            .create_volume(POOL, CHUNK as u64, None, "v")
            .await
            .unwrap();
        let va = cl.attach(v.id).await.unwrap();
        let stop = std::rc::Rc::new(std::cell::Cell::new(false));
        let count = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let (st, w, cnt) = (stop.clone(), va.clone(), count.clone());
        let writer = hyperstor_reactor::spawn(async move {
            let mut n = 0u8;
            while !st.get() {
                n = n.wrapping_add(1).max(1);
                w.write(0, vec![n; 4096]).await.unwrap();
                cnt.set(cnt.get() + 1);
            }
            n
        });
        let other = Client::connect(meta, 6).await.unwrap();
        let mut taken = Vec::new();
        for i in 0..5 {
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(50)).await;
            let s = other.snapshot(v.id, &format!("s{i}")).await.unwrap();
            let vol = other.volume(v.id).await.unwrap().unwrap();
            let first = other
                .read_at(POOL, vol.lineage, &vol.snapshot_layers(&s), 0, 4096)
                .await
                .unwrap();
            assert!(first[0] != 0 && first.iter().all(|b| *b == first[0]));
            taken.push((s, first));
        }
        // Under load writes are slow: wait for a few after the snapshots.
        let (after, t) = (count.get(), std::time::Instant::now());
        while count.get() < after + 3 && t.elapsed() < std::time::Duration::from_secs(10) {
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        stop.set(true);
        writer.await;
        assert!(
            count.get() >= after + 3,
            "the writer kept writing ({} writes)",
            count.get()
        );
        let vol = other.volume(v.id).await.unwrap().unwrap();
        for (s, first) in taken {
            let again = other
                .read_at(POOL, vol.lineage, &vol.snapshot_layers(&s), 0, 4096)
                .await
                .unwrap();
            assert!(
                again == first,
                "snapshot {} changed after it was taken",
                s.epoch
            );
        }
    });
    c.stop();
}

/// Deleting a snapshot merges its versions into the next newer version
/// only when nothing between reads them (implementation-notes.md I115):
/// here a later snapshot, taken with no write between, reads exactly
/// what the deleted one did.
#[test]
fn deleting_a_snapshot_keeps_what_a_later_one_reads() {
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    with_client(meta, 5, move |cl| async move {
        let v = cl
            .create_volume(POOL, CHUNK as u64, None, "v")
            .await
            .unwrap();
        let va = cl.attach(v.id).await.unwrap();
        va.write(0, pattern(1, 8192)).await.unwrap();
        let s1 = cl.snapshot(v.id, "s1").await.unwrap();
        let s2 = cl.snapshot(v.id, "s2").await.unwrap();
        va.write(4096, pattern(2, 4096)).await.unwrap();
        cl.delete_snapshot(v.id, s1.epoch).await.unwrap();
        let vol = cl.volume(v.id).await.unwrap().unwrap();
        let got = cl
            .read_at(POOL, vol.lineage, &vol.snapshot_layers(&s2), 0, 8192)
            .await
            .unwrap();
        assert!(
            got == pattern(1, 8192),
            "the later snapshot lost what it read"
        );
        let mut head = pattern(1, 8192);
        head[4096..].copy_from_slice(&pattern(2, 4096));
        assert!(va.read(0, 8192).await.unwrap() == head);
    });
    c.stop();
}

/// Resetting a volume to its newest snapshot again and again (an agent VM
/// after every task, D106) stays on one branch: the chain never grows,
/// every reset reads the snapshot, and the epochs it drops leave no
/// versions behind.
#[test]
fn resets_to_the_newest_snapshot_stay_in_place() {
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let lineage = with_client(meta, 5, move |cl| async move {
        let v = cl
            .create_volume(POOL, CHUNK as u64, None, "agent")
            .await
            .unwrap();
        let a = cl.attach(v.id).await.unwrap();
        a.write(0, pattern(1, 8192)).await.unwrap();
        a.detach().await.unwrap();
        let golden = cl.snapshot(v.id, "golden").await.unwrap();
        for task in 0..20u64 {
            let a = cl.attach(v.id).await.unwrap();
            assert!(
                a.read(0, 8192).await.unwrap() == pattern(1, 8192),
                "task {task}"
            );
            a.write(4096 * (task % 2), pattern(10 + task, 4096))
                .await
                .unwrap();
            a.detach().await.unwrap();
            let r = cl.rollback(v.id, golden.epoch).await.unwrap();
            assert_eq!((r.branch, r.chain.len()), (0, 0), "task {task}");
        }
        assert!(cl.branches(v.lineage).await.unwrap().is_empty());
        v.lineage
    });
    let (map, devs) = c.stop();
    let pool = &map.pools[&POOL];
    let pg = pg_of(lineage, 0, pool.pg_count);
    for (id, d) in devs {
        let sh = hyperstor_engine::open(&copy(&d)).unwrap().1.pop().unwrap();
        let k = ObjKey::new(POOL, pg, Namespace::Vol, chunk_name(lineage, 0), 0);
        let vs = sh.versions(&k);
        assert!(
            vs.len() <= 1,
            "device {id}: versions {vs:?} left by the resets"
        );
    }
}

/// A consistency group (volumes.md §10.3): two disks of one VM written in
/// order (A then B, with the same counter) are snapshot at one moment, so
/// every group's B is A's or the one before; a group whose member doesn't
/// quiesce is abandoned and leaves no snapshot or group behind.
#[test]
fn a_group_snapshot_is_one_moment_across_its_volumes() {
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    with_client(meta, 5, move |cl| async move {
        let a = cl
            .create_volume(POOL, CHUNK as u64, None, "a")
            .await
            .unwrap();
        let b = cl
            .create_volume(POOL, CHUNK as u64, None, "b")
            .await
            .unwrap();
        let (va, vb) = (
            cl.attach(a.id).await.unwrap(),
            cl.attach(b.id).await.unwrap(),
        );
        let stop = std::rc::Rc::new(std::cell::Cell::new(false));
        let (st, wa, wb) = (stop.clone(), va.clone(), vb.clone());
        let writer = hyperstor_reactor::spawn(async move {
            let mut n = 0u8;
            while !st.get() {
                n = n.wrapping_add(1).max(1);
                wa.write(0, vec![n; 4096]).await.unwrap();
                wb.write(0, vec![n; 4096]).await.unwrap();
            }
        });
        let other = Client::connect(meta, 6).await.unwrap();
        let mut groups = Vec::new();
        // B first: the member written second, so separate snapshots would
        // let the writer run on between them.
        for _ in 0..20 {
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(10)).await;
            let (gid, snaps) = other.snapshot_group(&[b.id, a.id], "g").await.unwrap();
            assert_eq!(other.group_complete(gid).await.unwrap(), Some(true));
            groups.push((gid, snaps));
        }
        stop.set(true);
        writer.await;
        for (gid, snaps) in &groups {
            let mut vals = Vec::new();
            for (id, s) in snaps {
                assert_eq!(s.group, *gid);
                let v = other.volume(*id).await.unwrap().unwrap();
                let d = other
                    .read_at(POOL, v.lineage, &v.snapshot_layers(s), 0, 4096)
                    .await
                    .unwrap();
                vals.push(d[0]);
            }
            let (y, x) = (vals[0], vals[1]);
            assert!(
                x == y || x == y.wrapping_add(1).max(1),
                "group {gid:x}: A holds {x}, B holds {y}: not one moment"
            );
        }
        // Deleting a group deletes its members' snapshots.
        other.delete_group(groups[0].0).await.unwrap();
        assert_eq!(other.group_complete(groups[0].0).await.unwrap(), None);
        let left = other.volume(a.id).await.unwrap().unwrap().snapshots.len();
        assert_eq!(left, 19);
        // A member whose holder doesn't answer: abandoned, nothing left.
        vb.detach().await.unwrap();
        drop(cl.attach(b.id).await.unwrap());
        let r = other.snapshot_group(&[a.id, b.id], "bad").await;
        assert!(r.is_err(), "{r:?}");
        for id in [a.id, b.id] {
            let v = other.volume(id).await.unwrap().unwrap();
            assert!(v.snapshots.iter().all(|s| s.name != "bad"));
        }
        let _ = va;
    });
    c.stop();
}

/// The hold a consistency group takes (volumes.md §10.3): after `Quiesce`
/// the volume's holder writes nothing until `Release` (or the hold's
/// 5 s), so holding every member before cutting any gives one moment.
#[test]
fn a_quiesce_holds_the_holders_writes_until_released() {
    use hyperstor_client::volume::header_key;
    use hyperstor_proto::{Msg, Target, WireKey};
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    with_client(meta, 5, move |cl| async move {
        let v = cl
            .create_volume(POOL, CHUNK as u64, None, "v")
            .await
            .unwrap();
        let va = cl.attach(v.id).await.unwrap();
        va.write(0, vec![1; 4096]).await.unwrap();
        let send = |token: u64, quiesce: bool| {
            let cl = cl.clone();
            async move {
                let m = cl.map();
                let meta_pool = hyperstor_meta::META_POOL;
                let name = header_key(v.id);
                let pg = hyperstor_client::object_pg(&name, m.pools[&meta_pool].pg_count);
                let info = m.pg(meta_pool, pg).unwrap();
                let conn =
                    hyperstor_rpc::Conn::connect(m.device_addr(info.primary().unwrap()).unwrap())
                        .await
                        .unwrap();
                let to = Target {
                    pool: meta_pool,
                    pg,
                    map_epoch: m.epoch,
                    pg_epoch: info.epoch,
                };
                let key = WireKey {
                    ns: hyperstor_client::NS_META,
                    name,
                    version: 0,
                };
                let msg = if quiesce {
                    Msg::Quiesce { to, key, token }
                } else {
                    Msg::Release { to, key, token }
                };
                conn.call(&msg, &[]).await.unwrap().0
            }
        };
        assert_eq!(send(7, true).await, Msg::ExecOk);
        let w = va.clone();
        let t = std::time::Instant::now();
        let write = hyperstor_reactor::spawn(async move {
            w.write(0, vec![2; 4096]).await.unwrap();
            std::time::Instant::now()
        });
        hyperstor_reactor::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(send(7, false).await, Msg::ExecOk);
        let released = std::time::Instant::now();
        let done = write.await;
        assert!(done >= released, "the write went through during the hold");
        // A hold nobody releases ends by itself.
        assert_eq!(send(8, true).await, Msg::ExecOk);
        let held = std::time::Instant::now();
        va.write(0, vec![3; 4096]).await.unwrap();
        assert!(
            held.elapsed() >= std::time::Duration::from_secs(4),
            "{:?}",
            held.elapsed()
        );
        let _ = t;
    });
    c.stop();
}

/// The host read cache (D102): clones of one golden snapshot, attached
/// through one client, read its blocks from the network once; a block a
/// clone writes is read from its own version again.
#[test]
fn clones_of_a_golden_image_share_the_read_cache() {
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    with_client(meta, 5, move |cl| async move {
        let size = 2 * CHUNK as u64;
        let golden = cl.create_volume(POOL, size, None, "golden").await.unwrap();
        let g = cl.attach(golden.id).await.unwrap();
        let image = pattern(7, size as usize);
        g.write(0, image.clone()).await.unwrap();
        g.detach().await.unwrap();
        let s = cl.snapshot(golden.id, "base").await.unwrap();
        let k1 = cl
            .clone_snapshot(golden.id, s.epoch, "k1", None)
            .await
            .unwrap();
        let k2 = cl
            .clone_snapshot(golden.id, s.epoch, "k2", None)
            .await
            .unwrap();
        let (a1, a2) = (
            cl.attach(k1.id).await.unwrap(),
            cl.attach(k2.id).await.unwrap(),
        );
        let blocks = size / 4096;
        let before = cl.read_cache_stats();
        assert!(a1.read(0, size).await.unwrap() == image);
        assert_eq!(cl.read_cache_stats().blocks - before.blocks, blocks);
        let warm = cl.read_cache_stats();
        assert!(a1.read(0, size).await.unwrap() == image);
        assert!(a2.read(0, size).await.unwrap() == image);
        assert_eq!(
            cl.read_cache_stats().hits - warm.hits,
            2 * blocks,
            "both clones read the golden image from the cache"
        );
        // A write puts the block in the clone's head: read from there.
        let mine = pattern(8, 8192);
        a1.write(4096, mine.clone()).await.unwrap();
        let mut want = image.clone();
        want[4096..12288].copy_from_slice(&mine);
        for _ in 0..2 {
            assert!(a1.read(0, size).await.unwrap() == want, "a stale block");
            assert!(a2.read(0, size).await.unwrap() == image);
        }
        // A snapshot of the clone: its head becomes a version that can't
        // change, which is cached; a write after it is read back.
        cl.snapshot(k1.id, "k1s").await.unwrap();
        hyperstor_reactor::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(a1.read(0, size).await.unwrap() == want);
        assert!(a1.read(0, size).await.unwrap() == want);
        let more = pattern(9, 4096);
        a1.write(4096, more.clone()).await.unwrap();
        want[4096..8192].copy_from_slice(&more);
        assert!(a1.read(0, size).await.unwrap() == want, "a stale block");
        // Another client writes the clone while this one has it detached:
        // attached again, it reads the new head, not what it cached.
        a2.write(0, pattern(11, size as usize)).await.unwrap();
        a2.read(0, size).await.unwrap();
        a2.detach().await.unwrap();
        let other = Client::connect(meta, 6).await.unwrap();
        let b2 = other.attach(k2.id).await.unwrap();
        let theirs = pattern(10, size as usize);
        b2.write(0, theirs.clone()).await.unwrap();
        b2.detach().await.unwrap();
        let a2 = cl.attach(k2.id).await.unwrap();
        assert!(a2.read(0, size).await.unwrap() == theirs, "a stale head");
        // Turned off, it keeps nothing and reads the same.
        cl.set_read_cache_bytes(0);
        assert_eq!(cl.read_cache_stats().blocks, 0);
        assert!(a1.read(0, size).await.unwrap() == want);
        assert_eq!(cl.read_cache_stats().blocks, 0);
    });
    c.stop();
}
