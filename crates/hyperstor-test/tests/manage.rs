//! Managing a running cluster with `hyperstorctl` (19-administration.md
//! §23.6): status and tables, I/O on every pool, taking devices out and
//! in, and a node failing and coming back.

use hyperstor_test::*;

#[test]
fn status_and_tables() {
    let c = Cluster::all_in_one("status", 3);
    let s = c.ctl(&["status", "--json"]).ok();
    assert!(s.has("\"health\":\"HEALTH_OK\""), "{}", s.out);
    assert!(
        s.has(&format!("\"cluster_id\":\"{}\"", c.cluster_id)),
        "{}",
        s.out
    );
    assert_eq!(s.out.matches("\"up\":true").count(), 3, "{}", s.out);
    assert_eq!(
        s.out.matches("\"last_heard_ms\"").count(),
        3,
        "every node heard from: {}",
        s.out
    );
    // Every device reports its space within a period (D156): then every
    // device and every pool has it.
    wait_for("every pool's space", 10, || {
        let s = c.ctl(&["status", "--json"]).ok();
        (s.out.matches("\"free_bytes\"").count()
            == s.out.matches("\"pg_count\"").count() + s.out.matches("\"up_in\"").count())
        .then_some(())
    });
    let n = c.ctl(&["nodes"]).ok();
    assert!(
        n.has("FREE") && n.out.lines().skip(1).all(|l| !l.contains(" - ")),
        "{}",
        n.out
    );
    let t = c.ctl(&["status"]).ok();
    assert!(
        t.has("health  ok") && t.has("nodes   3 up of 3; devices 3 in of 3"),
        "{}",
        t.out
    );
    let p = c.ctl(&["pools"]).ok();
    assert!(p.has("replicated") && p.has("cache"), "{}", p.out);
    let n = c.ctl(&["nodes"]).ok();
    assert_eq!(n.out.lines().count(), 4, "{}", n.out);
    let g = c.ctl(&["pgs", "--pool", "1"]).ok();
    assert_eq!(g.out.lines().count(), 65, "64 PGs and a header: {}", g.out);
    c.ctl(&["pgs", "--pool", "9"]).expect(1);
    let g = c.ctl(&["pgs", "--pool", "1", "--pg", "5"]).ok();
    assert!(
        g.out.lines().count() == 2 && g.has("\n1.5 "),
        "one PG: {}",
        g.out
    );
    c.ctl(&["pgs", "--pool", "1", "--pg", "64"]).expect(1);
    // The grid: a character per PG, 64 to a line (D154).
    let g = c.ctl(&["pg-grid", "--pool", "1"]).ok();
    assert!(
        g.has(&format!("\n     0  {}\n", ".".repeat(64))),
        "{}",
        g.out
    );
    c.ctl(&["pg-grid", "--pool", "9"]).expect(1);
}

/// D167: a device appended to a running node waits at weight 0, so
/// nothing moves; once the node is restarted with one more `--shard`, the
/// device reports its capacity, is weighted, and PGs move onto it, while
/// the data written before reads back. An address in use and a node that
/// isn't there are refused, and the change is audited.
#[test]
fn a_device_is_added_to_a_running_node() {
    let mut c = Cluster::all_in_one("adddev", 3);
    let made = c
        .ctl(&["create-volume", "--size", "16M", "--name", "before"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let data: Vec<u8> = (0..(8u32 << 20)).map(|i| (i % 251) as u8).collect();
    let src = c.scratch.path("before.raw");
    std::fs::write(&src, &data).unwrap();
    c.ctl(&["write-volume", "--id", &id, "--file", src.to_str().unwrap()])
        .ok();
    // Each device's (id, weight), and the PGs that list device 1.
    let devices = |c: &Cluster| -> Vec<(u64, f64)> {
        // Exit status 1 while health is warn (backfilling): read it anyway.
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|n| n["devices"].as_array().unwrap().clone())
            .map(|d| {
                (
                    d["id"].as_u64().unwrap_or(0),
                    d["weight"].as_f64().unwrap_or(0.0),
                )
            })
            .collect()
    };
    let on_device_1 = |c: &Cluster| -> usize {
        let s = c.ctl(&["pgs", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        v["pgs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| {
                p["acting"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|d| d.as_u64() == Some(1)))
            })
            .count()
    };
    let addr = format!("127.0.0.1:{}", free_ports(1));
    let add = c
        .ctl(&["device", "add", "--node", "0", "--address", &addr])
        .ok();
    assert!(add.has("device 1 added to node 0"), "{}", add.out);
    // Published once fenced, as other map changes are.
    wait_for("device 1 in the map at weight 0", 10, || {
        devices(&c).contains(&(1, 0.0)).then_some(())
    });
    assert_eq!(on_device_1(&c), 0, "nothing placed on it yet");
    c.ctl(&["device", "add", "--node", "1", "--address", &addr])
        .expect(1);
    c.ctl(&["device", "add", "--node", "9", "--address", "127.0.0.1:1"])
        .expect(1);
    c.ctl(&["device", "add", "--node", "0"]).expect(2);
    // The node runs it once restarted with one more --shard.
    c.kill_node(0);
    let img = c.data_dir().join("node-0-1.img");
    let shard = format!("{addr}={}:1", img.display());
    c.start_node_with(0, &["--shard", &shard]);
    wait_for("device 1 weighted by its capacity", 30, || {
        devices(&c)
            .iter()
            .any(|(d, w)| *d == 1 && *w > 0.0)
            .then_some(())
    });
    wait_for("PGs on device 1, and health ok", 120, || {
        (on_device_1(&c) > 0 && c.ctl(&["status"]).has("health  ok")).then_some(())
    });
    let out = c.scratch.path("after.raw");
    c.ctl(&[
        "read-volume",
        "--id",
        &id,
        "--size",
        &data.len().to_string(),
        "--out",
        out.to_str().unwrap(),
    ])
    .ok();
    assert!(std::fs::read(&out).unwrap() == data, "the data reads back");
    wait_for("the change in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "AddDevice"]).ok();
        a.has(&format!("node 0 address {addr}")).then_some(())
    });
}

/// D169: a reactor serves only a device the map has, on its node, at its
/// address; a `--shard` the map lacks or places elsewhere stops the node,
/// saying why. A device id can be named (`--shard ID@ADDR=...`), so a
/// node can run its second added device without its first.
#[test]
fn a_shard_must_be_the_maps_and_can_name_its_id() {
    let mut c = Cluster::all_in_one("deviceids", 3);
    let ports = free_ports(2);
    let (a, b) = (
        format!("127.0.0.1:{ports}"),
        format!("127.0.0.1:{}", ports + 1),
    );
    let img = |i: u32| c.data_dir().join(format!("node-0-{i}.img"));
    let (img1, img2) = (img(1), img(2));
    let stopped_saying = |c: &mut Cluster, what: &str| {
        wait_for("node 0 to stop", 20, || c.node_exited(0));
        let log = c.node_log(0);
        assert!(log.contains(what), "{log}");
    };
    // Not in the map: refused, after asking again for a few seconds.
    c.kill_node(0);
    c.start_node_with(0, &["--shard", &format!("7@{a}={}:1", img1.display())]);
    stopped_saying(&mut c, "device 7: not in the map");
    // Its file is made (sparse) before the check, but nothing is written:
    // no superblock.
    let mut head = vec![0u8; 64 << 10];
    if let Ok(mut f) = std::fs::File::open(&img1) {
        use std::io::Read;
        let _ = f.read_exact(&mut head);
    }
    assert!(
        head.iter().all(|&x| x == 0),
        "nothing written for a refused device"
    );
    // Two devices added: 1 at a, 2 at b.
    c.ctl(&["device", "add", "--node", "0", "--address", &a])
        .ok();
    let add = c
        .ctl(&["device", "add", "--node", "0", "--address", &b])
        .ok();
    assert!(add.has("device 2 added"), "{}", add.out);
    // Device 2 at device 1's address: refused.
    c.start_node_with(0, &["--shard", &format!("2@{a}={}:1", img2.display())]);
    stopped_saying(&mut c, "device 2: the map has it on node 0 at");
    // Device 2 by its id, without device 1: served, weighted, given PGs.
    c.start_node_with(0, &["--shard", &format!("2@{b}={}:1", img2.display())]);
    wait_for("device 2 weighted by its capacity", 30, || {
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).ok()?;
        v["nodes"]
            .as_array()?
            .iter()
            .flat_map(|n| n["devices"].as_array().cloned().unwrap_or_default())
            .any(|d| d["id"].as_u64() == Some(2) && d["weight"].as_f64().unwrap_or(0.0) > 0.0)
            .then_some(())
    });
    assert!(c.node_exited(0).is_none(), "node 0 runs");
}

/// §27.2.5, D175, D182: `hyperstorctl device remove` takes a device out,
/// follows its PGs off it as an operation and removes it from the map,
/// while the data written before reads back; a node restarted with its
/// `--shard` stops, saying so. A removal that would leave pools short of
/// copies is refused unless forced; forced, the pools keep fewer copies
/// and the node leaves the map with its last device. Neither id is given
/// again, and each removal is audited.
#[test]
fn a_device_is_removed_once_its_pgs_have_moved() {
    let mut c = Cluster::all_in_one("rmdev", 3);
    // The PGs that list device `d` in `up` or `acting`, and the map's
    // devices and nodes.
    let listing = |c: &Cluster, d: u64| -> usize {
        let s = c.ctl(&["pgs", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        v["pgs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| {
                ["up", "acting"].iter().any(|k| {
                    p[k].as_array()
                        .is_some_and(|a| a.iter().any(|x| x.as_u64() == Some(d)))
                })
            })
            .count()
    };
    let in_map = |c: &Cluster| -> (Vec<u64>, Vec<u64>) {
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        let nodes = v["nodes"].as_array().unwrap();
        (
            nodes
                .iter()
                .flat_map(|n| n["devices"].as_array().unwrap().clone())
                .map(|d| d["id"].as_u64().unwrap_or(0))
                .collect(),
            nodes
                .iter()
                .map(|n| n["id"].as_u64().unwrap_or(0))
                .collect(),
        )
    };
    // A second device on node 0, run and given PGs.
    let ports = free_ports(2);
    let addr = format!("127.0.0.1:{ports}");
    c.ctl(&["device", "add", "--node", "0", "--address", &addr])
        .ok();
    c.kill_node(0);
    let img = c.data_dir().join("node-0-1.img");
    let shard = format!("{addr}={}:1", img.display());
    c.start_node_with(0, &["--shard", &shard]);
    wait_for("PGs on device 1, and health ok", 120, || {
        (listing(&c, 1) > 0 && c.ctl(&["status"]).has("health  ok")).then_some(())
    });
    let made = c
        .ctl(&["create-volume", "--size", "16M", "--name", "kept"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let data: Vec<u8> = (0..(8u32 << 20)).map(|i| (i % 253) as u8).collect();
    let src = c.scratch.path("kept.raw");
    std::fs::write(&src, &data).unwrap();
    c.ctl(&["write-volume", "--id", &id, "--file", src.to_str().unwrap()])
        .ok();
    // A dry run changes nothing.
    let dry = c
        .ctl(&["device", "remove", "--device", "1", "--dry-run"])
        .ok();
    assert!(dry.has("would move off device 1"), "{}", dry.out);
    assert!(listing(&c, 1) > 0 && in_map(&c).0.contains(&1));
    c.ctl(&["device", "remove", "--device", "9"]).expect(1);
    c.ctl(&["device", "remove", "--device", "1", "--node", "0"])
        .expect(2);
    let rm = c
        .ctl(&["device", "remove", "--device", "1", "--wait", "120"])
        .ok();
    assert!(rm.has("device 1 removed from the map"), "{}", rm.out);
    wait_for("device 1 out of the map", 10, || {
        (!in_map(&c).0.contains(&1)).then_some(())
    });
    assert_eq!(listing(&c, 1), 0);
    let out = c.scratch.path("kept.out");
    c.ctl(&[
        "read-volume",
        "--id",
        &id,
        "--size",
        &data.len().to_string(),
        "--out",
        out.to_str().unwrap(),
    ])
    .ok();
    assert!(std::fs::read(&out).unwrap() == data, "the data reads back");
    // Its --shard stops the node, which runs without it.
    c.kill_node(0);
    c.start_node_with(0, &["--shard", &shard]);
    wait_for("node 0 to stop", 20, || c.node_exited(0));
    let log = c.node_log(0);
    assert!(log.contains("device 1: removed from the map"), "{log}");
    c.start_node(0);
    wait_for("health ok without device 1", 60, || {
        c.ctl(&["status"]).has("health  ok").then_some(())
    });
    // Node 2's only device: three copies would span two hosts.
    let short = c.ctl(&["device", "remove", "--device", "2000"]).expect(1);
    assert!(
        short.has("would keep 2 of its 3 copies; --force"),
        "{}",
        short.out
    );
    let forced = c
        .ctl(&[
            "device", "remove", "--device", "2000", "--force", "--wait", "120",
        ])
        .ok();
    assert!(
        forced.has("pool 1 keeps 2 of its 3 copies")
            && forced.has("device 2000 removed from the map, and node 2 with it"),
        "{}",
        forced.out
    );
    wait_for("node 2 out of the map", 10, || {
        (!in_map(&c).1.contains(&2)).then_some(())
    });
    c.ctl(&[
        "read-volume",
        "--id",
        &id,
        "--size",
        &data.len().to_string(),
        "--out",
        out.to_str().unwrap(),
    ])
    .ok();
    assert!(std::fs::read(&out).unwrap() == data, "and on two copies");
    // Device 1's id isn't given again.
    let again = format!("127.0.0.1:{}", ports + 1);
    let add = c
        .ctl(&["device", "add", "--node", "0", "--address", &again])
        .ok();
    assert!(add.has("device 2 added to node 0"), "{}", add.out);
    wait_for("both removals in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "RemoveDevice"]).ok();
        (a.has("device 1 ") && a.has("device 2000 force") && a.has("done")).then_some(())
    });
}

/// D167, D168: `hyperstor-install install --node N --device PATH` adds a
/// device to an installed node: checked for another system's data first
/// (refused with the map unchanged), registered, formatted, recorded in
/// install.conf and written into the node's options, so a later run keeps
/// it and the node runs it once restarted.
#[test]
fn the_installer_adds_a_device_to_a_node() {
    let mut c = Cluster::all_in_one("instdev", 3);
    let device_ids = |c: &Cluster| -> Vec<u64> {
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|n| n["devices"].as_array().unwrap().clone())
            .map(|d| d["id"].as_u64().unwrap_or(0))
            .collect()
    };
    let addr = format!("127.0.0.1:{}", free_ports(1));
    // An LVM physical volume: refused before the map changes.
    let lvm = c.scratch.path("lvm.img");
    let mut b = vec![0u8; 8192];
    b[512..520].copy_from_slice(b"LABELONE");
    b[536..544].copy_from_slice(b"LVM2 001");
    std::fs::write(&lvm, &b).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&lvm)
        .unwrap()
        .set_len(64 << 20)
        .unwrap();
    let refused = install_cmd(
        &c.root,
        "install",
        &[
            "--no-start",
            "--node",
            "0",
            "--device",
            lvm.to_str().unwrap(),
            "--device-address",
            &addr,
        ],
    );
    assert!(
        refused.code != 0 && refused.has("an LVM physical volume"),
        "{}",
        refused.out
    );
    assert!(!device_ids(&c).contains(&1), "nothing registered");
    // A blank disk: added as device 1.
    let disk = c.scratch.path("extra.img");
    std::fs::File::create(&disk)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let d = disk.to_str().unwrap();
    let added = install(
        &c.root,
        &[
            "--no-start",
            "--node",
            "0",
            "--device",
            d,
            "--device-address",
            &addr,
        ],
    );
    assert!(
        added.has(&format!(
            "added {d} to node 0 as device 1, served at {addr}"
        )),
        "{}",
        added.out
    );
    let conf = std::fs::read_to_string(c.root.join("etc/hyperstor/install.conf")).unwrap();
    assert!(conf.contains(&format!("devices = 1@{addr}={d}")), "{conf}");
    let env = std::fs::read_to_string(c.root.join("etc/hyperstor/node-0.env")).unwrap();
    assert!(env.contains(&format!("--shard {addr}={d}:0")), "{env}");
    // A later run keeps it, and adds nothing twice.
    let again = install(&c.root, &["--no-start", "--node", "0", "--device", d]);
    assert!(!again.has("added"), "{}", again.out);
    let env2 = std::fs::read_to_string(c.root.join("etc/hyperstor/node-0.env")).unwrap();
    assert_eq!(env, env2);
    // The node runs it once restarted from its options.
    c.kill_node(0);
    c.start_node(0);
    wait_for("device 1 weighted, with PGs, and health ok", 120, || {
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).ok()?;
        let weighted = v["nodes"]
            .as_array()?
            .iter()
            .flat_map(|n| n["devices"].as_array().cloned().unwrap_or_default())
            .any(|d| d["id"].as_u64() == Some(1) && d["weight"].as_f64().unwrap_or(0.0) > 0.0);
        let pgs: serde_json::Value = serde_json::from_str(&c.ctl(&["pgs", "--json"]).out).ok()?;
        let on_1 = pgs["pgs"].as_array()?.iter().any(|p| {
            p["acting"]
                .as_array()
                .is_some_and(|a| a.iter().any(|d| d.as_u64() == Some(1)))
        });
        (weighted && on_1 && c.ctl(&["status"]).has("health  ok")).then_some(())
    });
}

/// D178, D179: each node reports its device's kind; once every node has,
/// a device whose class no operator set takes its kind ("nvme" until
/// then, for every device), so these files become "file". An operator's
/// class stays and is marked; one named after another kind is refused;
/// `auto` goes back to the kind. Audited.
#[test]
fn devices_report_their_kind_and_classes_follow_it() {
    let c = Cluster::all_in_one("kinds", 3);
    // (id, kind, class, class_fixed) of each device.
    let devices = |c: &Cluster| -> Vec<(u64, String, String, bool)> {
        let s = c.ctl(&["status", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|n| n["devices"].as_array().unwrap().clone())
            .map(|d| {
                (
                    d["id"].as_u64().unwrap_or(0),
                    d["kind"].as_str().unwrap_or("").to_string(),
                    d["class"].as_str().unwrap_or("").to_string(),
                    d["class_fixed"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    };
    wait_for("every device's kind, and its class from it", 20, || {
        devices(&c)
            .iter()
            .all(|d| d.1 == "file" && d.2 == "file" && !d.3)
            .then_some(())
    });
    let set = c
        .ctl(&["device", "class", "--device", "1000", "--class", "scratch"])
        .ok();
    assert!(
        set.has("device 1000 class scratch, kept as set"),
        "{}",
        set.out
    );
    wait_for("the class set", 10, || {
        devices(&c)
            .contains(&(1000, "file".into(), "scratch".into(), true))
            .then_some(())
    });
    let n = c.ctl(&["nodes"]).ok();
    assert!(
        n.has(" scratch+ ") && n.has("+ class set with hyperstorctl device class"),
        "{}",
        n.out
    );
    let refused = c
        .ctl(&["device", "class", "--device", "0", "--class", "nvme"])
        .expect(1);
    assert!(
        refused.has("class nvme is for nvme devices"),
        "{}",
        refused.out
    );
    c.ctl(&["device", "class", "--device", "0"]).expect(2);
    c.ctl(&["device", "class", "--device", "99", "--class", "x"])
        .expect(1);
    let auto = c
        .ctl(&["device", "class", "--device", "1000", "--class", "auto"])
        .ok();
    assert!(auto.has("class file, following its kind"), "{}", auto.out);
    wait_for("back to its kind", 10, || {
        devices(&c)
            .contains(&(1000, "file".into(), "file".into(), false))
            .then_some(())
    });
    wait_for("both changes in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "SetDeviceClass"]).ok();
        (a.has("class=scratch") && a.has("class=auto")).then_some(())
    });
}

/// D164: a pool's class set: refused when its devices span fewer hosts
/// than the pool's copies; set, its PGs leave devices of other classes;
/// `any` brings every device back. Audited.
#[test]
fn a_pool_places_on_its_classes_only() {
    let c = Cluster::all_in_one("classsets", 4);
    // Classes once the kinds are in (D178).
    wait_for("every device's kind", 20, || {
        let s = c.ctl(&["status", "--json"]);
        (s.out.matches("\"kind\":\"file\"").count() == 4).then_some(())
    });
    for (d, cl) in [("0", "a"), ("1000", "a"), ("2000", "b"), ("3000", "c")] {
        c.ctl(&["device", "class", "--device", d, "--class", cl])
            .ok();
    }
    let on_3000 = |c: &Cluster| -> usize {
        let pgs: serde_json::Value =
            serde_json::from_str(&c.ctl(&["pgs", "--pool", "1", "--json"]).out).unwrap();
        pgs["pgs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| {
                p["up"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|d| d.as_u64() == Some(3000)))
            })
            .count()
    };
    wait_for("the classes in the map, PGs on device 3000", 10, || {
        (c.ctl(&["nodes"]).out.matches("+ ").count() >= 1 && on_3000(&c) > 0).then_some(())
    });
    let narrow = c
        .ctl(&["pool", "set", "--pool", "1", "--classes", "a"])
        .expect(1);
    assert!(
        narrow.has("span 2 host(s); pool 1 keeps 3 copies"),
        "{}",
        narrow.out
    );
    let set = c
        .ctl(&["pool", "set", "--pool", "1", "--classes", "a,b"])
        .ok();
    assert!(set.has("pool 1 places on classes a,b"), "{}", set.out);
    wait_for("no PG of pool 1 on device 3000", 10, || {
        (on_3000(&c) == 0).then_some(())
    });
    let pools = c.ctl(&["pools"]).ok();
    assert!(pools.has(" a,b"), "{}", pools.out);
    c.ctl(&["pool", "set", "--pool", "0", "--classes", "a,b"])
        .expect(1);
    c.ctl(&["pool", "set", "--pool", "1"]).expect(2);
    c.ctl(&["pool", "set", "--pool", "1", "--classes", "any"])
        .ok();
    wait_for("device 3000 back in pool 1", 10, || {
        (on_3000(&c) > 0).then_some(())
    });
    wait_for("both changes in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "SetPool"]).ok();
        (a.has("classes=a,b") && a.has("classes=any")).then_some(())
    });
}

/// D180: a pool stops using one device of its classes, and its PGs
/// leave it; a second removal would leave too few hosts; a device of
/// another class is added to it, and PGs come to it. Audited.
#[test]
fn devices_are_added_to_and_removed_from_a_pool() {
    let c = Cluster::all_in_one("pooldevs", 4);
    wait_for("every device's kind", 20, || {
        let s = c.ctl(&["status", "--json"]);
        (s.out.matches("\"kind\":\"file\"").count() == 4).then_some(())
    });
    let on_3000 = |c: &Cluster| -> usize {
        let pgs: serde_json::Value =
            serde_json::from_str(&c.ctl(&["pgs", "--pool", "1", "--json"]).out).unwrap();
        pgs["pgs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| {
                p["up"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|d| d.as_u64() == Some(3000)))
            })
            .count()
    };
    c.ctl(&["pool", "set", "--pool", "1", "--classes", "file"])
        .ok();
    wait_for("PGs of pool 1 on device 3000", 10, || {
        (on_3000(&c) > 0).then_some(())
    });
    let out = c
        .ctl(&["pool", "remove-device", "--pool", "1", "--device", "3000"])
        .ok();
    assert!(out.has("pool 1 no longer uses device 3000"), "{}", out.out);
    wait_for("no PG of pool 1 on device 3000", 10, || {
        (on_3000(&c) == 0).then_some(())
    });
    let pools = c.ctl(&["pools"]).ok();
    assert!(pools.has(" file-3000"), "{}", pools.out);
    let narrow = c
        .ctl(&["pool", "remove-device", "--pool", "1", "--device", "2000"])
        .expect(1);
    assert!(narrow.has("span 2 host(s)"), "{}", narrow.out);
    c.ctl(&["pool", "remove-device", "--pool", "0", "--device", "0"])
        .expect(1);
    c.ctl(&["pool", "add-device", "--pool", "1", "--device", "99"])
        .expect(1);
    c.ctl(&["pool", "add-device", "--pool", "1"]).expect(2);
    // Device 3000 in a class of its own: still out of pool 1 once taken
    // back, then added beyond the pool's classes.
    c.ctl(&["device", "class", "--device", "3000", "--class", "spare"])
        .ok();
    c.ctl(&["pool", "add-device", "--pool", "1", "--device", "3000"])
        .ok();
    wait_for("device 3000 back in pool 1", 10, || {
        (on_3000(&c) > 0).then_some(())
    });
    let pools = c.ctl(&["pools"]).ok();
    assert!(pools.has(" file+3000"), "{}", pools.out);
    wait_for("both changes in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "SetPoolDevice"]).ok();
        (a.has("pool 1 remove device 3000") && a.has("pool 1 add device 3000")).then_some(())
    });
}

/// §27.3.2: a pool is renamed and keeps a copy per device rather than
/// per host, its classes staying; it still takes I/O; nothing to change
/// is a usage error; audited.
#[test]
fn a_pool_is_renamed_and_its_domain_changed() {
    let c = Cluster::all_in_one("setpool", 3);
    wait_for("every device's kind", 20, || {
        let s = c.ctl(&["status", "--json"]);
        (s.out.matches("\"kind\":\"file\"").count() == 3).then_some(())
    });
    c.ctl(&["pool", "create", "--name", "fast", "--classes", "file"])
        .ok();
    // §27.3.2: renamed, and a copy per device rather than per host; the
    // classes stay. Nothing to change is a usage error.
    let set = c
        .ctl(&[
            "pool", "set", "--pool", "3", "--name", "quick", "--domain", "device",
        ])
        .ok();
    assert!(
        set.has("pool 3 keeps a copy per device, is named quick"),
        "{}",
        set.out
    );
    wait_for("the new name in the map", 10, || {
        let p = c.ctl(&["pools"]).ok();
        (p.has("quick") && p.has(" file")).then_some(())
    });
    c.ctl(&["pool", "set", "--pool", "3"]).expect(2);
    c.ctl(&["io-test", "--pool", "3", "--mib", "4"]).ok();
    wait_for("the change in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "SetPool"]).ok();
        a.has("pool 3 name=quick domain=device").then_some(())
    });
}

/// D176, D181: a pool that holds a volume, or a volume in the trash,
/// isn't deleted, and `--dry-run` lists it; once purged, the pool is
/// deleted and leaves the map, and a new pool doesn't get its id;
/// `.meta` is refused; audited.
#[test]
fn a_pool_is_deleted_once_it_holds_nothing() {
    let c = Cluster::all_in_one("delpool", 3);
    wait_for("every device's kind", 20, || {
        let s = c.ctl(&["status", "--json"]);
        (s.out.matches("\"kind\":\"file\"").count() == 3).then_some(())
    });
    c.ctl(&["pool", "create", "--name", "gone", "--classes", "file"])
        .ok();
    wait_for("pool 3 in the map", 10, || {
        c.ctl(&["pools"]).has("gone").then_some(())
    });
    let made = c
        .ctl(&[
            "create-volume",
            "--pool",
            "3",
            "--size",
            "16M",
            "--name",
            "v",
        ])
        .ok();
    let v = made.out.split_whitespace().nth(1).unwrap().to_string();
    let dry = c
        .ctl(&["pool", "delete", "--pool", "3", "--dry-run"])
        .expect(1);
    assert!(dry.has("holds these volumes") && dry.has(&v), "{}", dry.out);
    let refused = c.ctl(&["pool", "delete", "--pool", "3"]).expect(1);
    assert!(refused.has("holds 1 volume(s)"), "{}", refused.out);
    // In the trash, it still holds the pool.
    c.ctl(&["delete-volume", "--id", &v]).ok();
    c.ctl(&["pool", "delete", "--pool", "3"]).expect(1);
    c.ctl(&["trash", "purge", "--id", &v, "--wait", "60"]).ok();
    let dry = c.ctl(&["pool", "delete", "--pool", "3", "--dry-run"]).ok();
    assert!(dry.has("holds nothing"), "{}", dry.out);
    let del = c.ctl(&["pool", "delete", "--pool", "3"]).ok();
    assert!(del.has("pool 3 deleted"), "{}", del.out);
    wait_for("pool 3 out of the map", 10, || {
        (!c.ctl(&["pools"]).has("gone")).then_some(())
    });
    c.ctl(&["pool", "delete", "--pool", "3"]).expect(1);
    c.ctl(&["pool", "delete", "--pool", "0"]).expect(1);
    c.ctl(&["pool", "delete"]).expect(2);
    let again = c
        .ctl(&["pool", "create", "--name", "gone", "--classes", "file"])
        .ok();
    assert!(again.has("pool 4 gone made"), "{}", again.out);
    wait_for("the delete in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "DeletePool"]).ok();
        (a.has("pool 3") && a.has("ok, map epoch")).then_some(())
    });
}

/// D176: `pool delete --force` deletes what the pool holds first, as
/// an operation: a volume with a snapshot, its clone, and a volume in the
/// trash, each audited; then the pool.
#[test]
fn a_forced_pool_delete_takes_its_volumes_with_it() {
    let c = Cluster::all_in_one("forcepool", 3);
    wait_for("every device's kind", 20, || {
        let s = c.ctl(&["status", "--json"]);
        (s.out.matches("\"kind\":\"file\"").count() == 3).then_some(())
    });
    c.ctl(&["pool", "create", "--name", "doomed", "--classes", "file"])
        .ok();
    wait_for("pool 3 in the map", 10, || {
        c.ctl(&["pools"]).has("doomed").then_some(())
    });
    let id_of = |r: &Run| r.out.split_whitespace().nth(1).unwrap().to_string();
    let v = id_of(
        &c.ctl(&[
            "create-volume",
            "--pool",
            "3",
            "--size",
            "16M",
            "--name",
            "v",
        ])
        .ok(),
    );
    c.ctl(&["snapshot", "create", "--id", &v, "--name", "s"])
        .ok();
    let k = id_of(
        &c.ctl(&["clone-volume", "--id", &v, "--epoch", "1", "--name", "k"])
            .ok(),
    );
    let t = id_of(
        &c.ctl(&[
            "create-volume",
            "--pool",
            "3",
            "--size",
            "16M",
            "--name",
            "t",
        ])
        .ok(),
    );
    c.ctl(&["delete-volume", "--id", &t]).ok();
    let dry = c
        .ctl(&["pool", "delete", "--pool", "3", "--dry-run"])
        .expect(1);
    assert!(dry.has(&v) && dry.has(&k) && dry.has(&t), "{}", dry.out);
    let del = c
        .ctl(&["pool", "delete", "--pool", "3", "--force", "--wait", "120"])
        .ok();
    assert!(
        del.has("deleting these volumes") && del.has("pool 3 deleted"),
        "{}",
        del.out
    );
    wait_for("pool 3 out of the map", 10, || {
        (!c.ctl(&["pools"]).has("doomed")).then_some(())
    });
    assert!(!c.ctl(&["trash", "list"]).ok().has(&t));
    wait_for("each volume's delete in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "PurgeFromTrash"]).ok();
        [&v, &k, &t]
            .iter()
            .all(|id| a.has(&format!("volume {id} (pool 3 deleted)")))
            .then_some(())
    });
    let a = c.ctl(&["audit", "list", "--op", "DeletePool"]).ok();
    assert!(a.has("pool 3 force") && a.has("done"), "{}", a.out);
}

/// D163, D171: `hyperstorctl pool create` makes a pool on its classes,
/// with about 100 PGs per device per copy; volumes and I/O go to it;
/// a pool without classes, a name in use and classes with too few hosts
/// are refused; audited.
#[test]
fn a_pool_is_made_on_its_classes() {
    let c = Cluster::all_in_one("mkpool", 3);
    wait_for("every device's kind", 20, || {
        let s = c.ctl(&["status", "--json"]);
        (s.out.matches("\"kind\":\"file\"").count() == 3).then_some(())
    });
    let made = c
        .ctl(&["pool", "create", "--name", "fast", "--classes", "file"])
        .ok();
    // Pools 0, 1 and 2 are the trial's; 3 devices, 3 copies: 100, so 128.
    assert!(made.has("pool 3 fast made, 128 PGs"), "{}", made.out);
    wait_for("pool 3 in the map", 10, || {
        c.ctl(&["pools"]).has("fast").then_some(())
    });
    let pools = c.ctl(&["pools"]).ok();
    assert!(pools.has("fast") && pools.has(" file"), "{}", pools.out);
    c.ctl(&["io-test", "--pool", "3", "--mib", "4"]).ok();
    c.ctl(&["pool", "create", "--name", "x"]).expect(2);
    c.ctl(&["pool", "create", "--name", "fast", "--classes", "file"])
        .expect(1);
    let narrow = c
        .ctl(&["pool", "create", "--name", "y", "--classes", "nvme"])
        .expect(1);
    assert!(narrow.has("span 0 host(s)"), "{}", narrow.out);
    wait_for("the pool in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "CreatePool"]).ok();
        a.has("pool fast replicated copies=3 classes=file")
            .then_some(())
    });
}

/// D158: each device's weight follows its capacity in TiB, from its own
/// reports; an operator's weight stays, marked, until set back to auto.
#[test]
fn weights_follow_capacity_or_are_set() {
    let c = Cluster::all_in_one("weights", 3);
    // (id, weight, fixed, reported bytes) of each device.
    let devices = || -> Vec<(u64, f64, bool, Option<u64>)> {
        let s = c.ctl(&["status", "--json"]).ok();
        let v: serde_json::Value = serde_json::from_str(&s.out).unwrap();
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|n| n["devices"].as_array().unwrap().clone())
            .map(|d| {
                (
                    d["id"].as_u64().unwrap_or(0),
                    d["weight"].as_f64().unwrap_or(0.0),
                    d["weight_fixed"].as_bool().unwrap_or(false),
                    // uint64 is a string in protobuf's JSON.
                    d["total_bytes"]
                        .as_str()
                        .and_then(|t| t.parse().ok())
                        .or(d["total_bytes"].as_u64()),
                )
            })
            .collect()
    };
    let capacity = |bytes: u64| bytes as f64 / (1u64 << 40) as f64;
    let near = |a: f64, b: f64| (a - b).abs() <= b / 100.0;
    wait_for("weights from capacity", 20, || {
        devices()
            .iter()
            .all(|(_, w, fixed, t)| !fixed && t.is_some_and(|t| near(*w, capacity(t))))
            .then_some(())
    });
    let (id, _, _, total) = devices()[0];
    let id = id.to_string();
    let set = c.ctl(&["weight", "--device", &id, "--weight", "0.5"]).ok();
    assert!(set.has("weight 0.5000, kept as set"), "{}", set.out);
    // Published once fenced, as out and in are.
    let device = || {
        devices()
            .into_iter()
            .find(|d| d.0.to_string() == id)
            .unwrap()
    };
    wait_for("the weight set", 10, || {
        let d = device();
        (d.1 == 0.5 && d.2).then_some(())
    });
    let n = c.ctl(&["nodes"]).ok();
    assert!(
        n.has(" 0.5000* ") && n.has("weight set with hyperstorctl weight"),
        "{}",
        n.out
    );
    let auto = c.ctl(&["weight", "--device", &id, "--weight", "auto"]).ok();
    assert!(auto.has("following its capacity"), "{}", auto.out);
    wait_for("the weight back to auto", 10, || {
        let d = device();
        (!d.2 && near(d.1, capacity(total.unwrap()))).then_some(())
    });
    assert!(!c.ctl(&["nodes"]).ok().has("*"));
    c.ctl(&["weight", "--device", &id, "--weight", "-1"])
        .expect(2);
    c.ctl(&["weight", "--device", &id]).expect(2);
    c.ctl(&["weight", "--device", "999", "--weight", "1"])
        .expect(1);
    // Audited, as every change; the log's writer reaches .meta every
    // second.
    wait_for("both changes in the audit log", 20, || {
        let a = c.ctl(&["audit", "list", "--op", "SetDeviceWeight"]).ok();
        (a.has("weight=0.5") && a.has("weight=auto")).then_some(())
    });
}

/// I203: each device's superblock names its device and cluster. A node
/// given another node's device (swapped paths) refuses it and says whose
/// it is, writing nothing; with the paths put right it serves again.
#[test]
fn a_swapped_device_is_refused() {
    let mut c = Cluster::all_in_one("swap", 3);
    c.kill_node(1);
    c.kill_node(2);
    let (one, two, tmp) = (
        c.data_dir().join("node-1.img"),
        c.data_dir().join("node-2.img"),
        c.data_dir().join("swap.img"),
    );
    let swap = || {
        std::fs::rename(&one, &tmp).unwrap();
        std::fs::rename(&two, &one).unwrap();
        std::fs::rename(&tmp, &two).unwrap();
    };
    swap();
    // A rename keeps the time it was last written.
    let written = || std::fs::metadata(&one).unwrap().modified().unwrap();
    let before = written();
    c.start_node(1);
    let want = format!(
        "device 1000: the device holds device 2000 of cluster {}, not device 1000 of cluster {}",
        c.cluster_id, c.cluster_id
    );
    wait_for("node 1 to refuse node 2's device", 20, || {
        c.node_log(1).contains(&want).then_some(())
    });
    assert!(
        c.node_log(1).contains("reactor failed"),
        "{}",
        c.node_log(1)
    );
    assert_eq!(written(), before, "the refused device was written");
    c.kill_node(1);
    swap();
    c.start_node(1);
    c.start_node(2);
    let s = c.ctl(&["status", "--wait", "60"]).ok();
    assert!(s.has("health  ok"), "{}", s.out);
}

#[test]
fn io_on_every_pool() {
    let c = Cluster::all_in_one("io", 3);
    let r = c.ctl(&["io-test", "--mib", "8"]).ok();
    assert!(r.has("read and verified"), "{}", r.out);
    c.ctl(&["io-test", "--pool", "2", "--mib", "4"]).ok();
}

/// Out: PGs run degraded (three copies need three hosts) and I/O goes on;
/// in: back to clean. An unknown device is refused.
#[test]
fn out_and_in_settle() {
    let c = Cluster::all_in_one("outin", 3);
    assert!(
        c.ctl(&["out", "--node", "2", "--wait", "30"])
            .ok()
            .has("PGs settled")
    );
    let s = c.ctl(&["status"]).expect(1);
    assert!(
        s.has("device(s) down or out") && s.has("PG(s) degraded"),
        "{}",
        s.out
    );
    // Every PG of three copies on three hosts lost one.
    let g = c.ctl(&["pg-grid", "--pool", "1"]).ok();
    assert!(g.has(&"d".repeat(64)), "{}", g.out);
    c.ctl(&["io-test", "--mib", "4"]).ok();
    assert!(
        c.ctl(&["in", "--node", "2", "--wait", "30"])
            .ok()
            .has("PGs settled")
    );
    c.ctl(&["status", "--wait", "30"]).ok();
    let r = c.ctl(&["out", "--device", "999999"]).expect(1);
    assert!(r.has("no device 999999"), "{}", r.out);
}

/// A killed node is marked down within about a second (D48), status
/// says so and exits 1, I/O goes on with two copies; started again, it
/// is refilled and health returns to ok.
#[test]
fn a_killed_node_is_reported_and_comes_back() {
    let mut c = Cluster::all_in_one("kill", 3);
    c.ctl(&["io-test", "--mib", "4"]).ok();
    c.kill_node(2);
    // Down, and settled at warn: while the map moves the dead node's PGs
    // a slow host can show error for a moment (CI).
    wait_for("node 2 down, writes going on", 20, || {
        let r = c.ctl(&["status"]);
        (r.code == 1 && r.has("node(s) down: 2") && r.has("health  warn")).then_some(r)
    });
    c.ctl(&["io-test", "--mib", "4"]).ok();
    c.start_node(2);
    c.ctl(&["status", "--wait", "60"]).ok();
    c.ctl(&["io-test", "--mib", "4"]).ok();
}

/// A node that comes back isn't reported silent again (D40): its peers
/// forget its old silence, so it goes down once, for the kill, and stays
/// up after. The map service says why each change happened.
#[test]
fn a_returning_node_goes_down_once() {
    let mut c = Cluster::all_in_one("return", 3);
    // On a slow host every node may be marked down once at the start, by
    // the heartbeat timeout, while its device opens (I198): what is
    // checked is the kill and the return.
    let before = c.meta_log().lines().count();
    c.kill_node(2);
    wait_for("node 2 down", 20, || {
        c.ctl(&["status"]).has("node(s) down: 2").then_some(())
    });
    c.start_node(2);
    c.ctl(&["status", "--wait", "60"]).ok();
    // Long enough for stale reports to have done their damage.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let log = std::fs::read_to_string(&c.meta_log).unwrap();
    let changes: Vec<&str> = log
        .lines()
        .skip(before)
        .filter(|l| l.starts_with("hyperstor-meta: node 2 "))
        .collect();
    assert_eq!(
        changes.iter().filter(|l| l.contains(" down (")).count(),
        1,
        "{changes:#?}"
    );
    // Its peers notice a dead node first (600 ms here, NODE_ARGS; before the 800 ms
    // heartbeat timeout); it is held down at least 2 s (D149). A peer whose
    // reactor was held counts silence afresh (I188), so on a slow host the
    // heartbeat timeout may come first (CI).
    let held = [0, 1].iter().any(|n| {
        std::fs::read_to_string(c.scratch.path(&format!("node-{n}.log")))
            .unwrap_or_default()
            .contains("reactor held")
    });
    assert!(
        changes[0].contains("down (reported silent by nodes [0, 1]; held down 2 s)")
            || held
                && changes[0].contains("down (no heartbeat for ")
                && changes[0].ends_with("held down 2 s)"),
        "{changes:#?}"
    );
    assert!(
        changes
            .last()
            .unwrap()
            .contains("node 2 up (heartbeats again)"),
        "{changes:#?}"
    );
}

/// A node that comes back empty, as a rebuilt host does, is refilled
/// without anyone flapping: backfill keeps its peers busy, but pings
/// still get through, so no node is reported silent. On the pilot, a
/// rejoining host set off a cascade of every node reporting the others.
#[test]
fn an_empty_node_is_refilled_without_flapping() {
    let mut c = Cluster::all_in_one("refill", 3);
    let made = c
        .ctl(&["create-volume", "--size", "512M", "--name", "filler"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let data: Vec<u8> = (0..(384usize << 20))
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let src = c.scratch.path("filler.raw");
    std::fs::write(&src, &data).unwrap();
    drop(data);
    c.ctl(&["write-volume", "--id", &id, "--file", src.to_str().unwrap()])
        .ok();
    // Writing the filler on a loaded runner can hold a reactor past the
    // peer timeout, so a node may go down and come back before the kill;
    // D149 then warns "flapping" for 10 minutes (I200). What is checked is
    // the refill, so that is allowed, once every node is up again.
    let settled = |s: &str| {
        s.contains("health  ok")
            || s.contains("health  warn")
                && s.lines()
                    .filter(|l| l.starts_with("  "))
                    .all(|l| l.contains(" flapping ("))
    };
    wait_for("every node up before the kill", 90, || {
        settled(&c.ctl(&["status"]).out).then_some(())
    });
    c.kill_node(2);
    wait_for("node 2 down", 20, || {
        c.ctl(&["status"]).has("node(s) down: 2").then_some(())
    });
    let before = c.meta_log().lines().count();
    for f in std::fs::read_dir(c.data_dir()).unwrap() {
        let p = f.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("node-2") && name.ends_with(".img") {
            std::fs::remove_file(&p).unwrap();
        }
    }
    c.start_node(2);
    let t = std::time::Instant::now();
    let mut status = String::new();
    while t.elapsed().as_secs() < 180 {
        status = c.ctl(&["status"]).out;
        if settled(&status) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let log = c.meta_log();
    let changes: Vec<&str> = log
        .lines()
        .skip(before)
        .filter(|l| l.contains(" down (") || l.contains(" up ("))
        .collect();
    assert_eq!(
        changes,
        ["hyperstor-meta: node 2 up (heartbeats again)"],
        "only node 2 comes back; no one flaps"
    );
    // So any flapping warning left is from before the kill.
    assert!(settled(&status), "{status}");
}

/// Volumes through the cluster API (D77): `hyperstorctl` creates, lists
/// and deletes them; an id in use and a volume that isn't there are
/// refused.
#[test]
fn volumes_through_hyperstorctl() {
    let c = Cluster::all_in_one("volumes", 3);
    let made = c
        .ctl(&["create-volume", "--size", "64M", "--name", "scratch"])
        .ok();
    let id = made
        .out
        .split_whitespace()
        .nth(1)
        .expect("volume ID created")
        .to_string();
    assert_eq!(id.len(), 32, "{}", made.out);
    c.ctl(&[
        "create-volume",
        "--size",
        "1G",
        "--name",
        "fixed",
        "--id",
        "00000000000000000000000000000abc",
    ])
    .ok();
    let l = c.ctl(&["volumes"]).ok();
    assert!(
        l.has(&id) && l.has("scratch") && l.has("67108864"),
        "{}",
        l.out
    );
    assert!(
        l.has("00000000000000000000000000000abc") && l.has("fixed"),
        "{}",
        l.out
    );
    let r = c
        .ctl(&["create-volume", "--size", "1G", "--id", "abc"])
        .expect(1);
    assert!(r.has("exists"), "{}", r.out);
    c.ctl(&["delete-volume", "--id", &id, "--force"]).ok();
    let l = c.ctl(&["volumes", "--json"]).ok();
    assert!(!l.has(&id) && l.has("fixed"), "{}", l.out);
    let r = c.ctl(&["delete-volume", "--id", &id]).expect(1);
    assert!(r.has("no volume"), "{}", r.out);
    c.ctl(&["volumes", "--force"]).expect(2);
    c.ctl(&["create-volume", "--size", "1000", "--name", "odd"])
        .expect(1);
}

/// Snapshots through `hyperstorctl` (10-volumes.md §10.3, §10.4): taken,
/// listed, cloned, refused deletion while the clone depends on them,
/// flattened, rolled back and deleted; a consistency group taken and
/// deleted.
#[test]
fn snapshots_through_hyperstorctl() {
    let c = Cluster::all_in_one("snapshots", 3);
    let id_of = |r: &hyperstor_test::Run| r.out.split_whitespace().nth(1).unwrap().to_string();
    let v = id_of(
        &c.ctl(&["create-volume", "--size", "64M", "--name", "v"])
            .ok(),
    );
    let w = id_of(
        &c.ctl(&["create-volume", "--size", "64M", "--name", "w"])
            .ok(),
    );
    let s = c
        .ctl(&["snapshot", "create", "--id", &v, "--name", "first"])
        .ok();
    assert!(s.has("snapshot 1 of volume"), "{}", s.out);
    let l = c.ctl(&["snapshot", "list", "--id", &v]).ok();
    assert!(l.has("first") && l.has("active"), "{}", l.out);
    let k = c
        .ctl(&["clone-volume", "--id", &v, "--epoch", "1", "--name", "k"])
        .ok();
    assert!(k.has("created"), "{}", k.out);
    let kid = id_of(&k);
    assert!(c.ctl(&["volumes"]).ok().has(&kid));
    let r = c
        .ctl(&["snapshot", "delete", "--id", &v, "--epoch", "1"])
        .expect(1);
    assert!(r.has("in_use"), "{}", r.out);
    // A flatten is an operation (D121): its id at once, then followed.
    let f = c
        .ctl(&["flatten-volume", "--id", &kid, "--wait", "120"])
        .ok();
    assert!(
        f.has(&format!("volume {kid} flattening: operation "))
            && f.has(&format!("flatten volume {kid}: done")),
        "{}",
        f.out
    );
    c.ctl(&["snapshot", "create", "--id", &v, "--name", "second"])
        .ok();
    let r = c.ctl(&["rollback-volume", "--id", &v, "--epoch", "1"]).ok();
    assert!(r.has("rolled back to snapshot 1"), "{}", r.out);
    // Not to the newest: a new branch, which depends on snapshot 1 until
    // flattened.
    c.ctl(&["snapshot", "delete", "--id", &v, "--epoch", "1"])
        .expect(1);
    c.ctl(&["flatten-volume", "--id", &v, "--wait", "120"]).ok();
    c.ctl(&["snapshot", "delete", "--id", &v, "--epoch", "1"])
        .ok();
    let l = c.ctl(&["snapshot", "list", "--id", &v, "--json"]).ok();
    assert!(!l.has("\"first\"") && l.has("\"second\""), "{}", l.out);
    // A group of both volumes, then gone.
    let ids = format!("{v},{w}");
    let g = c
        .ctl(&["snapshot", "group", "--ids", &ids, "--name", "vm"])
        .ok();
    assert!(g.has("group ") && g.has(&w), "{}", g.out);
    let gid = g.out.split_whitespace().nth(1).unwrap().to_string();
    assert!(c.ctl(&["snapshot", "list", "--id", &w]).ok().has(&gid));
    c.ctl(&["snapshot", "group-delete", "--group", &gid]).ok();
    assert!(!c.ctl(&["snapshot", "list", "--id", &w]).ok().has(&gid));
    // Flags only where they belong.
    c.ctl(&["snapshot", "delete", "--id", &v]).expect(2);
    c.ctl(&["volumes", "--epoch", "1"]).expect(2);
}
