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
use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

const POOL: u32 = 1;
const CHUNK: u32 = 1 << 20;

enum MetaCmd {
    NodeDown(u32),
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
                let map = Rc::new(RefCell::new(map));
                spawn(hyperstor_meta::serve(
                    hyperstor_reactor::net::TcpListener::from_std(meta_l),
                    map.clone(),
                ));
                while let Some(cmd) = meta_rx.recv().await {
                    match cmd {
                        MetaCmd::NodeDown(n) => {
                            map.borrow_mut().nodes.get_mut(&n).unwrap().up = false;
                            map.borrow_mut().recompute();
                        }
                        MetaCmd::Stop => break,
                    }
                }
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
    let c = Cluster::start(4, 1);
    let meta = c.meta_addr;
    let tx = c.meta_tx.clone();
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect(meta, 5).await.unwrap();
            let before = cl.map().epoch;
            let d1 = pattern(1, 2 * CHUNK as usize);
            cl.write(POOL, 1, 0, d1.clone()).await.unwrap();
            // Node 0 goes down: its PGs move. The client still holds the
            // old map; its next requests are refused (stale epoch, or not
            // primary any more, or the node is unreachable) and retried
            // with a fresh map.
            tx.send(MetaCmd::NodeDown(0)).ok().unwrap();
            // Storage nodes poll the map every 200 ms. Until they have the
            // new one, a stale primary and stale replicas still accept writes
            // under the old PG epoch (implementation-notes.md I20; closed by
            // map leases in HS2). Wait for them.
            hyperstor_reactor::time::sleep(std::time::Duration::from_millis(500)).await;
            // A volume whose first chunk's primary was on node 0, so the
            // client's old map is certainly wrong for it.
            let old = cl.map();
            let v2 = (2u128..)
                .find(|v| {
                    let pg = pg_of(*v, 0, old.pools[&POOL].pg_count);
                    let p = old.pg(POOL, pg).unwrap().primary().unwrap();
                    old.devices[&p].node == 0
                })
                .unwrap();
            let d2 = pattern(2, 2 * CHUNK as usize);
            cl.write(POOL, v2, 0, d2.clone()).await.unwrap();
            assert!(cl.map().epoch > before, "the client learned the new map");
            assert_eq!(cl.read(POOL, v2, 0, d2.len() as u64).await.unwrap(), d2);
            // No backfill yet (HS2): only PGs that kept their primary still
            // have volume 1's data. Writing it again restores it everywhere.
            cl.write(POOL, 1, 0, d1.clone()).await.unwrap();
            assert_eq!(cl.read(POOL, 1, 0, d1.len() as u64).await.unwrap(), d1);
        })
    })
    .join()
    .unwrap();
    let (map, _) = c.stop();
    for pg in map.pgs.values() {
        assert!(pg.acting.iter().all(|d| map.devices[d].node != 0));
    }
}
