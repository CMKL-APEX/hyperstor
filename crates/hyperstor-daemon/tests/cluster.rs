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

enum MetaCmd {
    Stop,
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
        map.recompute();
        let (meta_tx, mut meta_rx) = channel::<MetaCmd>();
        let meta_thread = std::thread::spawn(move || {
            block_on(async move {
                let meta = hyperstor_meta::Meta::with_lease(map, lease);
                spawn(hyperstor_meta::serve(
                    hyperstor_reactor::net::TcpListener::from_std(meta_l),
                    meta.clone(),
                ));
                spawn(meta.clone().liveness(down_after));
                spawn(meta.clone().publisher());
                if let Some(MetaCmd::Stop) = meta_rx.recv().await {}
                meta.published.borrow().clone()
            })
        });
        let blocked: Vec<Arc<std::sync::atomic::AtomicBool>> =
            (0..nodes).map(|_| Arc::default()).collect();
        let nodes = listeners
            .into_iter()
            .enumerate()
            .map(|(n, ls)| {
                Some(start(NodeConfig {
                    node_id: n as u32,
                    meta: vec![meta_addr],
                    shards: ls
                        .into_iter()
                        .enumerate()
                        .map(|(i, l)| ShardSpec {
                            device_id: n as u32 * 1000 + i as u32,
                            device: DeviceSpec::Mem { bytes: 256 << 20 },
                            listener: l,
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
    // that changed are sent.
    assert!(
        (CHUNK as u64..=2 * CHUNK as u64).contains(&pushed),
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
