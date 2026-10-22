//! A replicated map service (decisions.md D20, D71–D74): three members over
//! Raft on loopback, four storage nodes that know all three, and clients
//! that follow the leader's redirects.

use hyperstor_client::Client;
use hyperstor_daemon::*;
use hyperstor_meta::replica::{self, Member};
use hyperstor_meta::{ClusterMap, Meta, replicated_pool};
use hyperstor_reactor::chan::{Sender, channel};
use hyperstor_reactor::{block_on, spawn};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const POOL: u32 = 1;
const CHUNK: u32 = 1 << 20;
const LEASE: Duration = Duration::from_millis(400);
const DOWN_AFTER: Duration = Duration::from_millis(700);

/// A free port below the kernel's ephemeral range (32768 and up on
/// Linux): members and nodes bind their port again when they restart, and
/// an outgoing connection elsewhere could take an ephemeral one meanwhile.
fn free_addr() -> SocketAddr {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = 10_000 + (std::process::id() % 200 * 100 + n * 7) % 22_000;
        let a: SocketAddr = ([127, 0, 0, 1], port as u16).into();
        if std::net::TcpListener::bind(a).is_ok() {
            return a;
        }
    }
}

/// One map service member, on threads of its own as on its own host.
struct MetaMember {
    member: Member,
    dir: PathBuf,
    stop: Option<Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MetaMember {
    fn start(&mut self, all: &[Member], seed: &ClusterMap) {
        let (me, all, seed, dir) = (self.member, all.to_vec(), seed.clone(), self.dir.clone());
        let (stop, mut stop_rx) = channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        self.thread = Some(std::thread::spawn(move || {
            let raft_l = std::net::TcpListener::bind(me.raft).unwrap();
            let map_l = std::net::TcpListener::bind(me.map).unwrap();
            let (to_reactor, from_raft) = channel();
            let r = replica::spawn(
                replica::Config {
                    id: me.id,
                    members: all,
                    dir,
                    tls: None,
                    settings: replica::settings(),
                    backup: None,
                    restore: None,
                },
                raft_l,
                to_reactor,
            )
            .unwrap();
            let commits = r.commits.clone();
            block_on(async move {
                let meta = Meta::replicated(seed, LEASE, commits);
                spawn(meta.clone().follow(from_raft));
                spawn(meta.clone().liveness(DOWN_AFTER));
                spawn(meta.clone().publisher());
                spawn(hyperstor_meta::serve(
                    hyperstor_reactor::net::TcpListener::from_std(map_l),
                    meta,
                ));
                ready_tx.send(()).unwrap();
                stop_rx.recv().await;
            });
            r.stop();
        }));
        ready_rx.recv().unwrap();
        self.stop = Some(stop);
    }

    fn kill(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

struct Group {
    _root: tempfile::TempDir,
    members: Vec<MetaMember>,
    seed: ClusterMap,
    node_addrs: Vec<SocketAddr>,
    nodes: Vec<Option<NodeHandle>>,
    stopped: std::collections::HashMap<usize, Arc<dyn hyperstor_engine::BlockDev>>,
}

impl Group {
    fn start(storage_nodes: u32) -> Group {
        let root = tempfile::tempdir().unwrap();
        let mut seed = ClusterMap::new(1);
        let node_addrs: Vec<SocketAddr> = (0..storage_nodes).map(|_| free_addr()).collect();
        for (n, a) in node_addrs.iter().enumerate() {
            seed.add_node(n as u32, &[*a], 0);
        }
        let mut pool = replicated_pool(POOL, "vol", 3, 64);
        pool.chunk_size = CHUNK;
        seed.add_pool(pool);
        seed.recompute();
        let all: Vec<Member> = (1..=3)
            .map(|id| Member {
                id,
                raft: free_addr(),
                map: free_addr(),
            })
            .collect();
        let mut members: Vec<MetaMember> = all
            .iter()
            .map(|m| MetaMember {
                member: *m,
                dir: root.path().join(format!("m{}", m.id)),
                stop: None,
                thread: None,
            })
            .collect();
        for m in &mut members {
            m.start(&all, &seed);
        }
        let mut g = Group {
            _root: root,
            members,
            seed,
            node_addrs,
            nodes: Vec::new(),
            stopped: Default::default(),
        };
        for n in 0..storage_nodes as usize {
            let h = g.node(n, DeviceSpec::Mem { bytes: 256 << 20 });
            g.nodes.push(Some(h));
        }
        g
    }

    fn maps(&self) -> Vec<SocketAddr> {
        self.members.iter().map(|m| m.member.map).collect()
    }

    fn all(&self) -> Vec<Member> {
        self.members.iter().map(|m| m.member).collect()
    }

    fn node(&self, n: usize, device: DeviceSpec) -> NodeHandle {
        start(NodeConfig {
            partitions: None,
            node_id: n as u32,
            meta: self.maps(),
            shards: vec![ShardSpec {
                device_id: n as u32 * 1000,
                device,
                listener: std::net::TcpListener::bind(self.node_addrs[n]).unwrap(),
            }],
            cpus: Vec::new(),
            spin: Duration::ZERO,
            peer_timeout: Duration::ZERO,
            meta_blocked: None,
            defer_max: None,
            scrub_interval: Duration::ZERO,
            no_digests: false,
        })
    }

    fn kill_node(&mut self, n: usize) {
        let h = self.nodes[n].take().unwrap();
        let dev = h.stop().into_iter().next().unwrap().unwrap();
        self.stopped.insert(n, dev);
    }

    fn restart_node(&mut self, n: usize) {
        let dev = self.stopped.remove(&n).unwrap();
        self.nodes[n] = Some(self.node(n, DeviceSpec::Existing(dev)));
    }

    fn kill_member(&mut self, i: usize) {
        self.members[i].kill();
    }

    fn restart_member(&mut self, i: usize) {
        let (all, seed) = (self.all(), self.seed.clone());
        self.members[i].start(&all, &seed);
    }

    fn stop(mut self) {
        for n in self.nodes.iter_mut() {
            if let Some(h) = n.take() {
                h.stop();
            }
        }
        for m in &mut self.members {
            m.kill();
        }
    }
}

/// Runs `f` with a client of the group, on a reactor thread of its own.
fn with_client<T: Send + 'static>(
    maps: Vec<SocketAddr>,
    id: u64,
    f: impl AsyncFnOnce(std::rc::Rc<Client>) -> T + Send + 'static,
) -> T {
    std::thread::spawn(move || {
        block_on(async move {
            let cl = Client::connect_members(maps, id).await.unwrap();
            f(cl).await
        })
    })
    .join()
    .unwrap()
}

async fn wait_clean(cl: &Client, within: Duration) {
    let t = Instant::now();
    loop {
        cl.refresh_map().await.unwrap();
        if cl.map().pgs.values().all(|p| p.acting == p.up) {
            return;
        }
        assert!(t.elapsed() < within, "PGs still backfilling");
        hyperstor_reactor::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Which member leads: the one that answers a heartbeat.
fn leader(maps: &[SocketAddr]) -> Option<usize> {
    let maps = maps.to_vec();
    std::thread::spawn(move || {
        block_on(async move {
            for (i, a) in maps.iter().enumerate() {
                let Ok(c) = hyperstor_rpc::Conn::connect(*a).await else {
                    continue;
                };
                if let Ok((hyperstor_proto::Msg::HeartbeatOk { .. }, _)) = c
                    .call(&hyperstor_proto::Msg::Heartbeat { node: 999 }, &[])
                    .await
                {
                    return Some(i);
                }
            }
            None
        })
    })
    .join()
    .unwrap()
}

fn wait_leader(maps: &[SocketAddr], within: Duration) -> usize {
    let t = Instant::now();
    loop {
        if let Some(l) = leader(maps) {
            return l;
        }
        assert!(t.elapsed() < within, "no leader elected");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn epoch_at(addr: SocketAddr) -> u64 {
    std::thread::spawn(move || {
        block_on(async move {
            let c = hyperstor_rpc::Conn::connect(addr).await.unwrap();
            match c
                .call(&hyperstor_proto::Msg::GetMap { have_epoch: 0 }, &[])
                .await
                .unwrap()
            {
                (hyperstor_proto::Msg::Map { epoch, .. }, _) => epoch,
                (m, _) => panic!("{m:?}"),
            }
        })
    })
    .join()
    .unwrap()
}

fn pattern(seed: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (seed as usize * 31 + i * 7) as u8)
        .collect()
}

/// The leader dies: another member takes over within an election and a
/// lease, I/O resumes, the new leader handles a storage node's failure and
/// return (after waiting out the old leader's leases, D73), and the old
/// leader comes back as a follower with the current map.
#[test]
fn the_map_service_survives_its_leader() {
    let mut g = Group::start(4);
    let maps = g.maps();
    let data = pattern(1, 8 * CHUNK as usize);
    let d = data.clone();
    with_client(maps.clone(), 5, async move |cl| {
        cl.write(POOL, 1, 0, d).await.unwrap();
        wait_clean(&cl, Duration::from_secs(20)).await;
    });
    let first = wait_leader(&maps, Duration::from_secs(10));
    let before = epoch_at(maps[first]);
    g.kill_member(first);
    let killed = Instant::now();
    // Writing on through the old leases running out, the election and
    // the new leader's first leases: the longest gap between two writes
    // that succeed is the I/O stall a map-service failover costs.
    let d = data[..CHUNK as usize].to_vec();
    let gap = with_client(maps.clone(), 6, async move |cl| {
        let mut last_ok = killed;
        let mut gap = Duration::ZERO;
        while killed.elapsed() < 6 * LEASE + Duration::from_secs(1) {
            if cl.write(POOL, 1, 0, d.clone()).await.is_ok() {
                gap = gap.max(last_ok.elapsed());
                last_ok = Instant::now();
            }
            assert!(
                last_ok.elapsed() < Duration::from_secs(10),
                "writes never resumed"
            );
        }
        gap
    });
    let second = wait_leader(&maps, Duration::from_secs(5));
    assert_ne!(second, first);
    eprintln!("map service failover: longest gap between writes {gap:?}");
    assert!(gap < Duration::from_secs(3), "writes stalled {gap:?}");
    // The new leader marks a failed storage node down and moves its PGs.
    g.kill_node(3);
    let d = data.clone();
    with_client(maps.clone(), 7, async move |cl| {
        let t = Instant::now();
        loop {
            cl.refresh_map().await.unwrap();
            if !cl.map().nodes[&3].up {
                break;
            }
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "node 3 never marked down"
            );
            hyperstor_reactor::time::sleep(Duration::from_millis(50)).await;
        }
        cl.write(POOL, 1, 0, d).await.unwrap();
    });
    assert!(epoch_at(maps[second]) > before, "the new leader published");
    // The old leader returns as a follower and catches up.
    g.restart_member(first);
    let t = Instant::now();
    while epoch_at(maps[first]) < epoch_at(maps[second]) {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "the old leader never caught up"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(leader(&maps), Some(second), "it came back as a follower");
    g.restart_node(3);
    let want = data.clone();
    with_client(maps.clone(), 8, async move |cl| {
        hyperstor_reactor::time::sleep(Duration::from_millis(800)).await;
        wait_clean(&cl, Duration::from_secs(20)).await;
        assert!(cl.map().nodes[&3].up);
        assert!(cl.read(POOL, 1, 0, want.len() as u64).await.unwrap() == want);
    });
    g.stop();
}

/// The map lives in the members' stores: a group stopped and started again
/// carries on from its last map, not from the cluster file's.
#[test]
fn a_group_restarted_whole_keeps_its_map() {
    let mut g = Group::start(4);
    let maps = g.maps();
    with_client(maps.clone(), 5, async move |cl| {
        cl.write(POOL, 1, 0, pattern(2, CHUNK as usize))
            .await
            .unwrap();
        wait_clean(&cl, Duration::from_secs(20)).await;
    });
    // A failure changes the map well past the file's epoch.
    g.kill_node(3);
    with_client(maps.clone(), 6, async move |cl| {
        let t = Instant::now();
        loop {
            cl.refresh_map().await.unwrap();
            if !cl.map().nodes[&3].up {
                return;
            }
            assert!(t.elapsed() < Duration::from_secs(10));
            hyperstor_reactor::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let l = wait_leader(&maps, Duration::from_secs(5));
    let before = epoch_at(maps[l]);
    assert!(before > g.seed.epoch);
    for i in 0..3 {
        g.kill_member(i);
    }
    for i in 0..3 {
        g.restart_member(i);
    }
    let l = wait_leader(&maps, Duration::from_secs(10));
    assert!(
        epoch_at(maps[l]) >= before,
        "the stored map, not the file's"
    );
    with_client(maps.clone(), 7, async move |cl| {
        cl.write(POOL, 1, 0, pattern(3, CHUNK as usize))
            .await
            .unwrap();
        assert!(!cl.map().nodes[&3].up, "node 3 is still down");
    });
    g.stop();
}
