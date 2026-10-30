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
        .filter(|l| l.starts_with("hyperstor-meta: node 2 "))
        .collect();
    assert_eq!(
        changes.iter().filter(|l| l.contains(" down (")).count(),
        1,
        "{changes:#?}"
    );
    // Its peers notice a dead node first (600 ms here, NODE_ARGS; before the 800 ms
    // heartbeat timeout); it is held down at least 2 s (D149).
    assert!(
        changes[0].contains("down (reported silent by nodes [0, 1]; held down 2 s)"),
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
        if status.contains("health  ok") {
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
    assert!(status.contains("health  ok"), "{status}");
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
