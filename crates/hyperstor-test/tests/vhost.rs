//! `hyperstor-vhost` (frontends.md §11.1, decisions.md D116, D117): volumes
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
        1
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
    // The preempted service is blocklisted as a client (metadata.md §9):
    // it can't detach cleanly or attach again; a new service can.
    let d = svc.ctl(serde_json::json!({"id": 4, "op": "detach", "args": {"volume": id}}));
    assert!(d.get("error").is_some(), "{d}");
    assert!(svc.attach(&id, "attach").get("error").is_some());
    let fresh = Service::start(&c, "vhost-c", &[]);
    let r3 = fresh.attach(&id, "attach");
    assert!(r3.get("ok").is_some(), "{r3}");
    fresh.stop();
    svc.stop();
    other.stop();
}
