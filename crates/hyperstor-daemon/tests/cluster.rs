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
const CHUNK: u32 = 1 << 20;

enum MetaCmd {
    Stop,
}

/// Each stopped device, by device id.
type Devices = Vec<(u32, Arc<dyn BlockDev>)>;

struct Cluster {
    meta_addr: SocketAddr,
    meta_tx: Sender<MetaCmd>,
    meta_thread: Option<std::thread::JoinHandle<ClusterMap>>,
    nodes: Vec<Option<NodeHandle>>,
}

impl Cluster {
    fn start(nodes: u32, shards: u32) -> Cluster {
        let meta_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let meta_addr = meta_l.local_addr().unwrap();
        let mut map = ClusterMap::new(1);
        let mut listeners = Vec::new();
        for n in 0..nodes {
            let ls: Vec<std::net::TcpListener> = (0..shards)
                .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
                .collect();
            let addrs: Vec<SocketAddr> = ls.iter().map(|l| l.local_addr().unwrap()).collect();
            map.add_node(n, &addrs, 0);
            listeners.push(ls);
        }
        let mut pool = replicated_pool(POOL, "vol", 3, 64);
        pool.chunk_size = CHUNK;
        map.add_pool(pool);
        map.recompute();
        let (meta_tx, mut meta_rx) = channel::<MetaCmd>();
        let meta_thread = std::thread::spawn(move || {
            block_on(async move {
                let meta = hyperstor_meta::Meta::new(map);
                let map = &meta.map;
                spawn(hyperstor_meta::serve(
                    hyperstor_reactor::net::TcpListener::from_std(meta_l),
                    meta.clone(),
                ));
                spawn(meta.clone().liveness(std::time::Duration::from_millis(600)));
                if let Some(MetaCmd::Stop) = meta_rx.recv().await {}
                map.borrow().clone()
            })
        });
        let nodes = listeners
            .into_iter()
            .enumerate()
            .map(|(n, ls)| {
                Some(start(NodeConfig {
                    node_id: n as u32,
                    meta: meta_addr,
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
                    defer_max: None,
                }))
            })
            .collect();
        Cluster {
            meta_addr,
            meta_tx,
            meta_thread: Some(meta_thread),
            nodes,
        }
    }

    /// Stops node `n` (its reactors exit; heartbeats stop).
    fn kill(&mut self, n: usize) {
        if let Some(h) = self.nodes[n].take() {
            h.stop();
        }
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
