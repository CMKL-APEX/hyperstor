//! `hyperstor-vhost` (11-frontends.md §11.1, 04-decisions.md D116, D117): volumes
//! attached through its control socket and served over vhost-user-blk,
//! with the test playing the hypervisor (`hyperstor_test::vhost`). No VM
//! or root needed.

use hyperstor_test::Cluster;
use hyperstor_test::vhost::*;
use std::path::PathBuf;

fn volume(c: &Cluster, size: &str) -> String {
    let made = c
        .ctl(&["create-volume", "--size", size, "--name", "vhost"])
        .ok();
    made.out.split_whitespace().nth(1).unwrap().to_string()
}

#[test]
fn a_volume_is_served_over_vhost_user_blk() {
    let c = Cluster::all_in_one("vhost", 3);
    let id = volume(&c, "64M");
    // A directory too long for the volumes' sockets is refused at start.
    let long = std::env::temp_dir().join("d".repeat(90));
    let o = std::process::Command::new(hyperstor_test::bin_dir().join("hyperstor-vhost"))
        .args(["serve", "--meta", &c.map, "--dir", long.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("too long"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let svc = Service::start(&c, "vhost-a", &[]);
    assert_eq!(
        svc.ctl(serde_json::json!({"id": 9, "op": "hello"}))["ok"]["protocol"],
        2
    );
    let r = svc.attach(&id, "attach");
    let socket = PathBuf::from(r["ok"]["socket"].as_str().unwrap_or_else(|| panic!("{r}")));
    assert_eq!(r["ok"]["size"], 64 << 20);
    // A retry answers with the same socket.
    assert_eq!(svc.attach(&id, "attach")["ok"]["socket"], r["ok"]["socket"]);
    let mut g = Guest::connect(&socket, false);
    assert_eq!(g.capacity(), 64 << 20);
    // Writes and reads, across a 4 MiB chunk boundary.
    let data = pattern(5, 256 << 10);
    let off = (4 << 20) - (128 << 10);
    g.write(off, &data);
    assert!(g.called(), "the device calls on completion");
    assert!(g.read(off, data.len() as u32) == data, "read back differs");
    assert!(g.read(0, 4096).iter().all(|b| *b == 0), "never written");
    // Flush, the id, write-zeroes, and what's refused.
    assert_eq!(g.request(T_FLUSH, 0, &[], 0).0, S_OK);
    let (s, idb, _) = g.request(T_GET_ID, 0, &[], 20);
    assert_eq!(s, S_OK);
    assert_eq!(&idb[..15], b"disk-under-test");
    let mut z = (off / 512).to_le_bytes().to_vec();
    z.extend(16u32.to_le_bytes());
    z.extend(0u32.to_le_bytes());
    assert_eq!(g.request(T_WRITE_ZEROES, 0, &z, 0).0, S_OK);
    assert!(g.read(off, 8192).iter().all(|b| *b == 0), "zeroed");
    assert!(g.read(off + 8192, 4096) == data[8192..12288]);
    assert_eq!(g.request(99, 0, &[], 0).0, S_UNSUPP);
    let (s, _, _) = g.request(T_IN, (64 << 20) / 512, &[], 4096);
    assert_eq!(s, S_IOERR, "past the end");
    let (s, _, _) = g.request(T_IN, 1, &[], 4000);
    assert_eq!(s, S_IOERR, "not whole sectors");
    // The limits a driver is told (size_max, seg_max), which add up to
    // 8 MiB. Firmware ignores them and sends more in one request, in
    // segments larger than size_max: such a request is served in pieces.
    // 12 MiB from a sector inside a block, across a piece boundary,
    // written and read in one request each, then read in small requests.
    assert_eq!(g.config_u32(8), 64 << 10, "size_max");
    assert_eq!(g.config_u32(12), 128, "seg_max");
    // Bytes that don't repeat, so a piece put in the wrong place shows.
    let big: Vec<u8> = (0..12u64 << 20)
        .map(|i| (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as u8)
        .collect();
    let at = (32 << 20) + 1536;
    assert_eq!(
        g.request(T_OUT, at / 512, &big, 0).0,
        S_OK,
        "a 12 MiB write"
    );
    let (s, back, _) = g.request(T_IN, at / 512, &[], 12 << 20);
    assert_eq!(s, S_OK, "a 12 MiB read");
    assert!(back == big, "the 12 MiB read differs");
    for o in [0, (8 << 20) - 1536 - 4096, (12 << 20) - 4096] {
        assert!(
            g.read(at + o as u64, 4096) == big[o..o + 4096],
            "small read at {o}"
        );
    }
    // 512-byte sectors (D126): the disk's logical blocks are sectors, its
    // physical ones the volume's 4 KiB blocks.
    assert_eq!(g.config_u32(20), 512, "blk_size");
    assert_eq!(g.config_u8(24), 3, "physical blocks of 2^3 sectors");
    // (The first 8 KiB were zeroed above.)
    assert!(
        g.read(off + 8704, 1024) == data[8704..9728],
        "inside a block"
    );
    assert!(
        g.read(off + 11776, 5120) == data[11776..16896],
        "across blocks, partial at both ends"
    );
    // A partial block written: read, merged and written back, the rest of
    // the block kept; and across three blocks, partial at both ends.
    let mut want = data.clone();
    let patch = pattern(9, 512);
    g.write(off + 16384 + 1024, &patch);
    want[16384 + 1024..16384 + 1536].copy_from_slice(&patch);
    let wide = pattern(11, 9216);
    g.write(off + 20480 + 3072, &wide);
    want[20480 + 3072..20480 + 3072 + 9216].copy_from_slice(&wide);
    assert!(g.read(off + 16384, 20480) == want[16384..36864]);
    // Eight writes into the eight sectors of one block, in flight at once:
    // each read-modify-write waits for the others', so none is lost.
    let sectors: Vec<Vec<u8>> = (0..8u8).map(|i| pattern(20 + i, 512)).collect();
    let blk = off + 65536;
    let batch: Vec<(u64, &[u8])> = sectors
        .iter()
        .enumerate()
        .map(|(i, s)| (blk + 512 * i as u64, s.as_slice()))
        .collect();
    assert_eq!(g.write_together(&batch), vec![S_OK; 8]);
    assert!(
        g.read(blk, 4096) == sectors.concat(),
        "a sector's write was lost"
    );
    // Write-zeroes on sectors: only those.
    let mut z = ((blk + 512) / 512).to_le_bytes().to_vec();
    z.extend(2u32.to_le_bytes());
    z.extend(0u32.to_le_bytes());
    assert_eq!(g.request(T_WRITE_ZEROES, 0, &z, 0).0, S_OK);
    let mut zeroed = sectors.concat();
    zeroed[512..1536].fill(0);
    assert!(g.read(blk, 4096) == zeroed, "two sectors zeroed");
    // The hypervisor reconnects (it restarted, say): the volume stayed
    // attached and serves the same data.
    drop(g);
    let mut g = Guest::connect(&socket, true);
    assert!(g.read(off + 8192, 4096) == data[8192..12288]);
    g.write(0, &data[..8192]);
    assert!(g.read(0, 8192) == data[..8192], "with EVENT_IDX");
    let listed = svc.ctl(serde_json::json!({"id": 2, "op": "list"}));
    assert_eq!(listed["ok"]["disks"][0]["connected"], true, "{listed}");
    drop(g);
    // A second disk on the same service, which a preemption of the first
    // leaves alone (D150).
    let id2 = volume(&c, "16M");
    let r = svc.ctl(serde_json::json!({"id": 7, "op": "attach", "args": {
        "volume": id2, "take": "attach", "holder": "node-a"}}));
    let socket_b = PathBuf::from(r["ok"]["socket"].as_str().unwrap_or_else(|| panic!("{r}")));
    let mut gb = Guest::connect(&socket_b, false);
    let other_data = pattern(7, 8192);
    gb.write(0, &other_data);
    // Found held, the answer says who holds it (D153).
    let other0 = Service::start(&c, "vhost-b0", &[]);
    let held = other0.attach(&id2, "attach");
    assert_eq!(held["error"]["code"], "held", "{held}");
    assert_eq!(held["error"]["holder"], "vhost:node-a", "{held}");
    other0.stop();
    // Another node's service can't take a held volume, but preempts it
    // (D79): the first holder's I/O fails from then on.
    let other = Service::start(&c, "vhost-b", &[]);
    let held = other.attach(&id, "attach");
    assert_eq!(held["error"]["code"], "held", "{held}");
    let mut g = Guest::connect(&socket, false);
    let r2 = other.attach(&id, "preempt");
    let socket2 = PathBuf::from(
        r2["ok"]["socket"]
            .as_str()
            .unwrap_or_else(|| panic!("{r2}")),
    );
    let (s, _, _) = g.request(T_IN, 0, &[], 4096);
    assert_eq!(s, S_IOERR, "the preempted holder is cut off");
    drop(g);
    let mut g2 = Guest::connect(&socket2, true);
    assert!(
        g2.read(off + 8192, 4096) == data[8192..12288],
        "data moved with the lease"
    );
    drop(g2);
    // Detach: the socket goes, and the volume can be attached elsewhere.
    let d = other.ctl(serde_json::json!({"id": 3, "op": "detach", "args": {"volume": id}}));
    assert!(d.get("ok").is_some(), "{d}");
    assert!(!socket2.exists());
    // Only that volume's lease was fenced, not the service (D150): its
    // other disk goes on, it lets the preempted one go cleanly, and it can
    // attach it again.
    assert!(gb.read(0, 8192) == other_data, "the other disk is cut off");
    gb.write(8192, &other_data);
    assert!(gb.read(8192, 8192) == other_data);
    drop(gb);
    let listed = svc.ctl(serde_json::json!({"id": 5, "op": "list"}));
    let lost = |v: &str| {
        listed["ok"]["disks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["volume"] == v)
            .map(|d| d["lost"].clone())
    };
    assert_eq!(lost(&id), Some(serde_json::json!(true)), "{listed}");
    assert_eq!(lost(&id2), Some(serde_json::json!(false)), "{listed}");
    let d = svc.ctl(serde_json::json!({"id": 4, "op": "detach", "args": {"volume": id}}));
    assert!(d.get("ok").is_some(), "{d}");
    let r3 = svc.attach(&id, "attach");
    assert!(r3.get("ok").is_some(), "{r3}");
    // Preempted again, and taken back without a detach (glidex starting
    // its VM here again, D153): the lost disk is let go and attached
    // afresh.
    let r4 = other.attach(&id, "preempt");
    assert!(r4.get("ok").is_some(), "{r4}");
    let d = other.ctl(serde_json::json!({"id": 6, "op": "detach", "args": {"volume": id}}));
    assert!(d.get("ok").is_some(), "{d}");
    let r5 = svc.attach(&id, "preempt");
    let socket5 = PathBuf::from(
        r5["ok"]["socket"]
            .as_str()
            .unwrap_or_else(|| panic!("{r5}")),
    );
    let mut g5 = Guest::connect(&socket5, false);
    assert!(g5.read(off + 8192, 4096) == data[8192..12288]);
    drop(g5);
    svc.stop();
    other.stop();
}

/// A restart keeps the volumes attached (04-decisions.md D143): the service
/// names them in its state file, takes them back from the instance it was
/// before, and serves them on the same sockets, where the hypervisor
/// reconnects. One that another client took while it was down is left to
/// that client.
#[test]
fn a_restarted_service_takes_its_volumes_back() {
    let c = Cluster::all_in_one("vhost-restart", 3);
    let (a, b) = (volume(&c, "16M"), volume(&c, "16M"));
    let mut svc = Service::start(&c, "vhost-r", &[]);
    let sa = PathBuf::from(svc.attach(&a, "attach")["ok"]["socket"].as_str().unwrap());
    assert!(svc.attach(&b, "attach").get("ok").is_some());
    let data = pattern(3, 64 << 10);
    let mut g = Guest::connect(&sa, true);
    g.write(4096, &data);
    // A restart while the hypervisor is connected: the guest's next
    // requests go to the new process, on the same socket.
    svc.restart();
    assert_eq!(svc.listed().len(), 2, "both taken back");
    drop(g);
    let mut g = Guest::connect(&sa, true);
    assert!(g.read(4096, data.len() as u32) == data, "the data is there");
    g.write(0, &data[..4096]);
    drop(g);
    // Stopped: the control socket goes, the state file stays.
    svc.pause();
    assert!(!svc.dir.join("control.sock").exists());
    let state = std::fs::read_to_string(svc.dir.join("state.json")).unwrap();
    assert!(state.contains(&a) && state.contains(&b), "{state}");
    // Meanwhile another node takes volume b over (its holder is gone).
    let other = Service::start(&c, "vhost-o", &[]);
    let r = other.attach(&b, "take_over");
    assert!(r.get("ok").is_some(), "{r}");
    svc.resume();
    assert_eq!(svc.listed(), vec![a.clone()], "b is left to the other");
    let mut g = Guest::connect(&sa, true);
    assert!(g.read(0, 4096) == data[..4096], "written before the stop");
    drop(g);
    // The other keeps b, and serves it.
    assert_eq!(other.listed(), vec![b.clone()]);
    other.stop();
    svc.stop();
}

/// A detach is refused while a hypervisor is connected (04-decisions.md
/// D256): the guest keeps its disk. Once the hypervisor hangs up, or with
/// `force`, it goes.
#[test]
fn a_detach_leaves_a_connected_disk_alone() {
    let c = Cluster::all_in_one("vhost-busy", 3);
    let (a, b) = (volume(&c, "16M"), volume(&c, "16M"));
    let svc = Service::start(&c, "vhost-busy", &[]);
    let detach = |v: &str, force: bool| {
        svc.ctl(serde_json::json!({"id": 2, "op": "detach", "args": {"volume": v, "force": force}}))
    };
    let sa = PathBuf::from(svc.attach(&a, "attach")["ok"]["socket"].as_str().unwrap());
    let data = pattern(7, 64 << 10);
    let mut g = Guest::connect(&sa, true);
    g.write(0, &data);
    let t = std::time::Instant::now();
    let d = detach(&a, false);
    assert_eq!(d["error"]["code"], "busy", "{d}");
    assert!(t.elapsed().as_millis() >= 1900, "it waited for a hang-up");
    assert!(sa.exists(), "the socket stays");
    assert!(g.read(0, data.len() as u32) == data, "the guest goes on");
    // A hypervisor that hangs up while the detach waits (#265): a VM just
    // stopped, still exiting. Its disk goes without force.
    let d = std::thread::scope(|s| {
        let pending = s.spawn(|| detach(&a, false));
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(g);
        pending.join().unwrap()
    });
    assert!(d.get("ok").is_some(), "{d}");
    assert!(!sa.exists());
    // Forced: the hypervisor is cut off.
    let sb = PathBuf::from(svc.attach(&b, "attach")["ok"]["socket"].as_str().unwrap());
    let mut g = Guest::connect(&sb, true);
    g.write(0, &data[..4096]);
    let d = detach(&b, true);
    assert!(d.get("ok").is_some(), "{d}");
    assert!(!sb.exists());
    drop(g);
    assert!(svc.listed().is_empty());
    svc.stop();
}

/// D293: a disk attached with `poll` takes requests its guest makes
/// available without notifying, while busy and just after; one without
/// waits for the notification. The choice is listed and kept across a
/// restart, and an idle polled disk lets its reactor sleep.
#[test]
fn a_polled_disk_takes_requests_its_guest_doesnt_notify() {
    let c = Cluster::all_in_one("vhost-poll", 3);
    let (a, b) = (volume(&c, "16M"), volume(&c, "16M"));
    let mut svc = Service::start(&c, "vhost-poll", &[]);
    let r = svc.ctl(serde_json::json!({"id": 1, "op": "attach", "args": {
        "volume": a, "serial": "polled", "poll": true}}));
    let sa = PathBuf::from(r["ok"]["socket"].as_str().unwrap_or_else(|| panic!("{r}")));
    let sb = PathBuf::from(svc.attach(&b, "attach")["ok"]["socket"].as_str().unwrap());
    let polled = |svc: &Service| -> Vec<(String, bool)> {
        let l = svc.ctl(serde_json::json!({"id": 2, "op": "list"}));
        let mut v: Vec<(String, bool)> = l["ok"]["disks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d["volume"].as_str().unwrap().to_string(), d["poll"] == true))
            .collect();
        v.sort();
        v
    };
    let mut want = vec![(a.clone(), true), (b.clone(), false)];
    want.sort();
    assert_eq!(polled(&svc), want);
    let data = pattern(9, 4096);
    const READS: u32 = 200;
    for event_idx in [false, true] {
        let mut g = Guest::connect(&sa, event_idx);
        g.write(8192, &data);
        g.kick_after = Some(std::time::Duration::from_millis(20));
        for _ in 0..READS {
            assert!(g.read(8192, 4096) == data, "read back differs");
        }
        // Most are taken without a notification; a busy host may let the
        // window pass now and then.
        assert!(
            g.late_kicks <= READS - 10,
            "event_idx {event_idx}: {} of {READS} needed a notification",
            g.late_kicks
        );
    }
    let mut g = Guest::connect(&sb, true);
    g.write(8192, &data);
    g.kick_after = Some(std::time::Duration::from_millis(5));
    for _ in 0..20 {
        assert!(g.read(8192, 4096) == data);
    }
    assert_eq!(
        g.late_kicks, 20,
        "not polled: each waits for its notification"
    );
    drop(g);
    // Idle, a polled disk's reactor sleeps: well under a core.
    let g = Guest::connect(&sa, true);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let ticks = || {
        let s = std::fs::read_to_string(format!("/proc/{}/stat", svc.pid())).unwrap();
        let f: Vec<u64> = s[s.rfind(')').unwrap() + 2..]
            .split_whitespace()
            .map(|x| x.parse().unwrap_or(0))
            .collect();
        f[11] + f[12]
    };
    let before = ticks();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let used = ticks() - before;
    assert!(used < 50, "{used} ticks in 2 s idle");
    drop(g);
    svc.restart();
    assert_eq!(polled(&svc), want, "kept across a restart");
    svc.stop();
}

/// The host's NUMA nodes with CPUs, and each one's first CPU.
fn nodes() -> Vec<(u32, usize)> {
    let mut out: Vec<(u32, usize)> = std::fs::read_dir("/sys/devices/system/node")
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let e = e.ok()?;
            let n = e.file_name().to_str()?.strip_prefix("node")?.parse().ok()?;
            let list = std::fs::read_to_string(e.path().join("cpulist")).ok()?;
            let first = list.trim().split([',', '-']).next()?.parse().ok()?;
            Some((n, first))
        })
        .collect();
    out.sort();
    out
}

/// D290–D292: a reactor on each CPU of `--cpus`. A disk goes to the
/// reactor of the node its guest's memory is bound to, else to the least
/// loaded, and its lease goes with it (D291): I/O there works, as do a
/// reconnect, `list` and a detach. On a host of one node both reactors
/// are on it and only the least-loaded rule is seen.
#[test]
fn each_disk_runs_on_its_guests_reactor() {
    let c = Cluster::all_in_one("vhost-numa", 3);
    let (a, b) = (volume(&c, "16M"), volume(&c, "16M"));
    let nodes = nodes();
    let (cpus, bind) = match (nodes.first(), nodes.last()) {
        (Some(f), Some(l)) if f.0 != l.0 => (format!("{},{}", f.1, l.1), Some(l.0)),
        _ => ("0,1".to_string(), None),
    };
    let svc = Service::start(&c, "vhost-numa", &["--cpus", &cpus]);
    let list = || {
        let r = svc.ctl(serde_json::json!({"id": 1, "op": "list"}));
        r["ok"]["disks"].as_array().unwrap().clone()
    };
    let reactor = |v: &str| {
        let l = list();
        let d = l.iter().find(|d| d["volume"] == v).unwrap();
        assert_eq!(d["lost"], false, "{d}");
        d["reactor"].as_u64().unwrap()
    };
    let sa = PathBuf::from(svc.attach(&a, "attach")["ok"]["socket"].as_str().unwrap());
    let sb = PathBuf::from(svc.attach(&b, "attach")["ok"]["socket"].as_str().unwrap());
    // Before a hypervisor connects, on the first reactor.
    assert_eq!(reactor(&a), 0);
    let data = pattern(3, 64 << 10);
    let mut ga = Guest::connect_bound(&sa, true, bind);
    ga.write(0, &data);
    let ra = reactor(&a);
    assert_eq!(
        ra, 1,
        "bound to the second reactor's node, or the least loaded of none"
    );
    if let Some(n) = bind {
        let l = list();
        let d = l.iter().find(|d| d["volume"] == a.as_str()).unwrap();
        assert_eq!(d["node"], n, "{d}");
    }
    // Unbound: the least loaded.
    let mut gb = Guest::connect(&sb, true);
    gb.write(0, &data);
    assert_eq!(reactor(&b), 0);
    assert!(ga.read(0, data.len() as u32) == data);
    assert!(gb.read(0, data.len() as u32) == data);
    // A hypervisor reconnecting: the lease went with the disk, which takes
    // what was written.
    drop(ga);
    let mut ga = Guest::connect_bound(&sa, true, bind);
    assert!(ga.read(0, data.len() as u32) == data, "after a reconnect");
    ga.write(64 << 10, &data);
    assert_eq!(reactor(&a), 1);
    drop(ga);
    drop(gb);
    for v in [&a, &b] {
        let d = svc.ctl(serde_json::json!({"id": 2, "op": "detach", "args": {"volume": v}}));
        assert!(d.get("ok").is_some(), "{d}");
    }
    assert!(svc.listed().is_empty());
    // Detached from the reactor that held it: attached again from scratch.
    let sa = PathBuf::from(svc.attach(&a, "attach")["ok"]["socket"].as_str().unwrap());
    let mut ga = Guest::connect(&sa, true);
    assert!(ga.read(64 << 10, data.len() as u32) == data);
    drop(ga);
    svc.stop();
}

/// Pins this thread to `cpu`.
fn pin(cpu: usize) {
    // SAFETY: cpu_set_t is plain data; CPU_SET writes within it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        assert_eq!(
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set),
            0
        );
    }
}

/// D292's second half: a guest whose memory isn't bound starts on the
/// least-loaded reactor; once the pages its requests use show its memory
/// is on another node, its disk moves to that node's reactor, requests in
/// flight finished first, and goes on there. Needs two NUMA nodes.
#[test]
fn an_unbound_guests_disk_follows_its_pages() {
    let nodes = nodes();
    let (Some(first), Some(last)) = (nodes.first(), nodes.last()) else {
        return;
    };
    if first.0 == last.0 {
        eprintln!("one NUMA node: nothing to move between");
        return;
    }
    let c = Cluster::all_in_one("vhost-pages", 3);
    let a = volume(&c, "16M");
    // Reactor 0 on the last node, 1 on the first: an unbound guest
    // starts on 1, the least loaded that isn't the control socket's.
    let cpus = format!("{},{}", last.1, first.1);
    let svc = Service::start(&c, "vhost-pages", &["--cpus", &cpus]);
    let reactor = || {
        let r = svc.ctl(serde_json::json!({"id": 1, "op": "list"}));
        let d = r["ok"]["disks"][0].clone();
        assert_eq!(d["lost"], false, "{d}");
        (d["reactor"].as_u64().unwrap(), d["node"].as_u64())
    };
    let sa = PathBuf::from(svc.attach(&a, "attach")["ok"]["socket"].as_str().unwrap());
    // The guest's memory first touched on the last node.
    pin(last.1);
    let mut g = Guest::connect_touched(&sa, true);
    let data = pattern(9, 256 << 10);
    g.write(0, &data);
    assert_eq!(reactor().0, 1, "unbound: the least loaded, not the first");
    let t = std::time::Instant::now();
    let mut i = 0u64;
    while reactor().0 != 0 {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(20),
            "never moved"
        );
        i += 1;
        g.write((i % 32) * (256 << 10), &data);
    }
    assert_eq!(reactor().1, Some(last.0 as u64));
    // It goes on there, with what was written before and while it moved.
    for k in 0..=i.min(31) {
        assert!(
            g.read(k * (256 << 10), data.len() as u32) == data,
            "block {k}"
        );
    }
    g.write(0, &data[..4096]);
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert_eq!(reactor().0, 0, "placed by its pages once");
    drop(g);
    let d = svc.ctl(serde_json::json!({"id": 2, "op": "detach", "args": {"volume": a}}));
    assert!(d.get("ok").is_some(), "{d}");
    svc.stop();
}

/// D183: a write refused for space (a device past full, D27) is held and
/// tried again every second; space freed in time lets it finish, and past
/// `--hold-full` the guest sees `IOERR`. An overwrite of written blocks
/// isn't refused, so isn't held.
#[test]
fn a_write_refused_for_space_is_held() {
    let c = Cluster::all_in_one("vhostfull", 3);
    // Exits 1 while health is an error, as it is once full.
    let used = || -> f64 {
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        let n = |d: &serde_json::Value, f: &str| {
            d[f].as_str()
                .and_then(|t| t.parse::<f64>().ok())
                .or(d[f].as_f64())
        };
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|n| n["devices"].as_array().unwrap().clone())
            .filter_map(|d| Some(100.0 * (1.0 - n(&d, "free_bytes")? / n(&d, "total_bytes")?)))
            .fold(0.0, f64::max)
    };
    let held = volume(&c, "1G");
    let refused = volume(&c, "1G");
    let chunk = 32u64 << 20;
    let src = c.scratch.path("chunk.raw");
    std::fs::write(&src, pattern(3, chunk as usize)).unwrap();
    let write = |id: &str, at: u64| {
        c.ctl(&[
            "write-volume",
            "--id",
            id,
            "--file",
            src.to_str().unwrap(),
            "--offset",
            &(at * chunk).to_string(),
            "--map",
            &c.map,
        ])
    };
    // Past 52 % on every device (each holds a copy of everything).
    let mut next = 0;
    while used() < 52.0 {
        let had = used();
        for _ in 0..4 {
            write(&held, next).ok();
            next += 1;
        }
        assert!(next * chunk < 1 << 30, "the volume filled first");
        hyperstor_test::wait_for("a space report", 15, || (used() > had).then_some(()));
    }
    let full = |pct: &str| {
        c.ctl(&[
            "full-ratios",
            "--nearfull",
            "50",
            "--backfillfull",
            "50.5",
            "--full",
            pct,
        ])
        .ok();
    };
    // Each probe a chunk never written: one written before the devices
    // had the ratio would let the next through as an overwrite.
    let probe = std::cell::Cell::new(0);
    let refuses = || {
        probe.set(probe.get() + 1);
        write(&refused, probe.get()).code != 0
    };
    full("51");
    hyperstor_test::wait_for("new blocks refused", 20, || refuses().then_some(()));

    // Held for 30 s: space freed after 3 s lets the write finish.
    let a = Service::start(&c, "full-a", &["--hold-full", "30"]);
    let r = a.attach(&held, "attach");
    let mut g = Guest::connect(&PathBuf::from(r["ok"]["socket"].as_str().unwrap()), false);
    g.wait = std::time::Duration::from_secs(40);
    let t = std::time::Instant::now();
    g.write(0, &pattern(4, 8192));
    assert!(
        t.elapsed() < std::time::Duration::from_secs(1),
        "an overwrite isn't held"
    );
    let fresh = 1000 << 20;
    let data = pattern(5, 64 << 10);
    let (status, t) = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            full("95");
        });
        let t = std::time::Instant::now();
        let (status, _, _) = g.request(T_OUT, fresh / 512, &data, 0);
        (status, t.elapsed())
    });
    assert_eq!(status, S_OK, "held until space was freed");
    assert!(t >= std::time::Duration::from_secs(3), "held {t:?}");
    assert!(g.read(fresh, data.len() as u32) == data);

    // Held for 2 s, then an I/O error.
    full("51");
    hyperstor_test::wait_for("new blocks refused again", 20, || refuses().then_some(()));
    let b = Service::start(&c, "full-b", &["--hold-full", "2"]);
    let r = b.attach(&refused, "attach");
    let mut g = Guest::connect(&PathBuf::from(r["ok"]["socket"].as_str().unwrap()), false);
    let t = std::time::Instant::now();
    let (status, _, _) = g.request(T_OUT, fresh / 512, &data, 0);
    assert_eq!(status, S_IOERR, "past the hold");
    assert!(
        t.elapsed() >= std::time::Duration::from_secs(2),
        "{:?}",
        t.elapsed()
    );
    // A bad value is refused at start.
    let o = std::process::Command::new(hyperstor_test::bin_dir().join("hyperstor-vhost"))
        .args(["serve", "--meta", &c.map, "--hold-full", "3601"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    drop(g);
    a.stop();
    b.stop();
}

/// A host bound to a realm (D251, D259) attaches only volumes of its
/// realm's tenants, on the name it gives (`--host`, D261); unbound, or
/// bound to "default" too, it attaches the others. A realm with a host
/// bound to it isn't deleted.
#[test]
fn a_host_bound_to_a_realm_attaches_only_its_volumes() {
    let c = Cluster::all_in_one("vhostrealm", 3);
    c.ctl(&["realm", "create", "--name", "acme"]).ok();
    c.ctl(&["realm", "tenant", "--tenant", "2", "--realm", "acme"])
        .ok();
    let vol = |name: &str, tenant: &str| {
        let mut args = vec!["create-volume", "--size", "16M", "--name", name];
        if !tenant.is_empty() {
            args.extend(["--tenant", tenant]);
        }
        let made = c.ctl(&args).ok();
        made.out.split_whitespace().nth(1).unwrap().to_string()
    };
    let (acmes, plain, before, other) = (
        vol("acmes", "2"),
        vol("plain", ""),
        vol("before", ""),
        vol("other", "3"),
    );
    let svc = Service::start(&c, "vhost-realm", &["--host", "hv-acme"]);
    // Unbound: any realm's.
    let r = svc.attach(&before, "attach");
    assert!(r.get("ok").is_some(), "{r}");
    let bound = c
        .ctl(&["realm", "host", "--host", "hv-acme", "--realm", "acme"])
        .ok();
    assert!(bound.has("only volumes of realm(s) acme"), "{}", bound.out);
    let list = c.ctl(&["realm", "list"]).ok();
    let row = list.out.lines().find(|l| l.starts_with("acme"));
    assert!(row.unwrap_or_default().contains("hv-acme"), "{}", list.out);
    // Bound to acme: acme's, and not default's.
    let r = svc.attach(&plain, "attach");
    assert!(r.to_string().contains("isn't bound to realm 0"), "{r}");
    let r = svc.attach(&acmes, "attach");
    assert!(r.get("ok").is_some(), "{r}");
    // ...until bound to default too.
    c.ctl(&[
        "realm",
        "host",
        "--host",
        "hv-acme",
        "--realm",
        "acme,default",
    ])
    .ok();
    let r = svc.attach(&other, "attach");
    assert!(r.get("ok").is_some(), "{r}");
    let r = c.ctl(&["realm", "delete", "--name", "acme"]).expect(1);
    assert!(r.has("1 host(s)"), "{}", r.out);
    c.ctl(&["realm", "host", "--host", "hv-acme", "--realm", "any"])
        .ok();
    let r = svc.attach(&plain, "attach");
    assert!(r.get("ok").is_some(), "{r}");
    c.ctl(&["realm", "host", "--host", "bad host", "--realm", "acme"])
        .expect(1);
    svc.stop();
}
