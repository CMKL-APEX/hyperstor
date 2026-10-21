//! Managing a running cluster with `hyperstorctl` (administration.md
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
    let s = wait_for("node 2 reported down", 20, || {
        let r = c.ctl(&["status"]);
        (r.code == 1 && r.has("node(s) down: 2")).then_some(r)
    });
    assert!(s.has("health  warn"), "{}", s.out);
    c.ctl(&["io-test", "--mib", "4"]).ok();
    c.start_node(2);
    c.ctl(&["status", "--wait", "60"]).ok();
    c.ctl(&["io-test", "--mib", "4"]).ok();
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
