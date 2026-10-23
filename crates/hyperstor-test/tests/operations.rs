//! Drains as operations (administration.md §23.6): a dry run that
//! changes nothing, a drain followed to the end, a cancelled one that
//! brings the devices back in; output as YAML, and the API's error
//! reasons and exit codes.

use hyperstor_pki::Role;
use hyperstor_test::*;

/// The id in `drain`'s "operation ID" line.
fn op_id(out: &str) -> String {
    out.split("operation ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or_else(|| panic!("no operation id in {out}"))
        .to_string()
}

#[test]
fn drains_run_as_operations() {
    let c = Cluster::all_in_one("drain", 4);
    // A dry run lists what would move and changes nothing.
    let dry = c.ctl(&["drain", "--node", "3", "--dry-run"]).ok();
    assert!(
        dry.has("would move off node 3; nothing changed"),
        "{}",
        dry.out
    );
    assert!(dry.has("BEFORE") && dry.has("AFTER"), "{}", dry.out);
    assert!(!c.ctl(&["nodes"]).ok().has(" out\n"), "still in");
    assert!(
        c.ctl(&["op", "list"]).ok().out.lines().count() == 1,
        "no operation for a dry run"
    );

    // A drain, followed until no PG lives on the node.
    let r = c.ctl(&["drain", "--node", "3", "--wait", "60"]).ok();
    let id = op_id(&r.out);
    assert!(r.has(&format!("{id} drain node 3: done")), "{}", r.out);
    let nodes = c.ctl(&["nodes"]).ok();
    let node3: Vec<&str> = nodes.out.lines().filter(|l| l.starts_with("3 ")).collect();
    assert!(
        !node3.is_empty() && node3.iter().all(|l| l.ends_with(" out")),
        "{}",
        nodes.out
    );
    let pgs = c.ctl(&["pgs", "--json"]).ok();
    assert!(
        !pgs.out.contains("3000"),
        "no PG on node 3's device: {}",
        pgs.out
    );
    let op = c.ctl(&["op", "get", "--id", &id, "-o", "yaml"]).ok();
    assert!(
        op.has("state: \"OPERATION_STATE_DONE\"") && op.has(&format!("id: \"{id}\"")),
        "{}",
        op.out
    );
    c.ctl(&["in", "--node", "3", "--wait", "60"]).ok();
    c.ctl(&["status", "--wait", "60"]).ok();

    // A cancelled drain brings the devices back in.
    let r = c.ctl(&["drain", "--node", "2"]).ok();
    let id = op_id(&r.out);
    let cancelled = c.ctl(&["op", "cancel", "--id", &id]);
    // It may have finished first on a fast host; then cancel says so.
    if cancelled.code == 0 {
        assert!(
            cancelled.has("cancelled; node 2's devices are back in"),
            "{}",
            cancelled.out
        );
        assert!(!c.ctl(&["nodes"]).ok().has(" out\n"));
    } else {
        assert!(cancelled.has("(finished)"), "{}", cancelled.out);
        c.ctl(&["in", "--node", "2"]).ok();
    }
    let list = c.ctl(&["op", "list"]).ok();
    assert_eq!(
        list.out.lines().count(),
        3,
        "two operations, newest first: {}",
        list.out
    );
    assert!(
        list.out.lines().nth(1).unwrap().starts_with(&id),
        "{}",
        list.out
    );

    // Reasons and exit codes.
    let r = c.ctl(&["op", "get", "--id", "nope"]);
    assert!(r.code == 1 && r.has("(not_found)"), "{}", r.out);
    let reader = c.identity(Role::Node, "9", false);
    let r = c.ctl_via(Via::Tls(&reader), &["drain", "--node", "1"]);
    assert!(r.code == 4 && r.has("(permission_denied)"), "{}", r.out);
    // Reading is allowed: a dry run is a read.
    c.ctl_via(Via::Tls(&reader), &["drain", "--node", "1", "--dry-run"])
        .ok();
    let r = c.ctl_via(Via::Tls(&reader), &["op", "list"]).ok();
    assert!(r.has(&id), "{}", r.out);
    let foreign = c.identity(Role::Admin, "x", true);
    assert_eq!(
        c.ctl_via(Via::Tls(&foreign), &["op", "list"]).code,
        3,
        "TLS refused: unreachable"
    );
    let y = c.ctl(&["status", "-o", "yaml"]);
    assert!(
        y.out.starts_with("cluster_id: \"") && y.has("\npools:\n  - "),
        "{}",
        y.out
    );
    // Audited: the drains, the cancel and the refusal.
    let audit = c.audit();
    assert!(
        audit
            .iter()
            .filter(|l| l.contains("\"op\":\"DrainNode\""))
            .count()
            >= 3,
        "{audit:?}"
    );
    assert!(
        audit
            .iter()
            .any(|l| l.contains("\"op\":\"DrainNode\"") && l.contains("refused"))
    );
}

/// Operations are rows of the replicated store (D120): they outlive the
/// map service, which carries on with a drain it was following once it has
/// fenced the last leader (D125); a flatten can't be cancelled (D121);
/// `op watch` follows a change at a time.
#[test]
fn operations_outlive_the_map_service() {
    let mut c = Cluster::all_in_one("ops-restart", 4);
    let id_of = |r: &Run| r.out.split_whitespace().nth(1).unwrap().to_string();
    let v = id_of(
        &c.ctl(&["create-volume", "--size", "64M", "--name", "v"])
            .ok(),
    );
    c.ctl(&["snapshot", "create", "--id", &v, "--name", "s"])
        .ok();
    let k = id_of(
        &c.ctl(&["clone-volume", "--id", &v, "--epoch", "1", "--name", "k"])
            .ok(),
    );
    let f = c.ctl(&["flatten-volume", "--id", &k]).ok();
    let flatten = op_id(&f.out);
    let r = c
        .ctl(&["op", "watch", "--id", &flatten, "--wait", "120"])
        .ok();
    assert!(
        r.has(&format!("{flatten} flatten volume {k}: done")),
        "{}",
        r.out
    );
    let r = c.ctl(&["op", "cancel", "--id", &flatten]).expect(1);
    assert!(r.has("(not_cancellable)"), "{}", r.out);

    // A drain under way when the map service stops is carried on by the
    // next one to lead (here, the same member again).
    let d = c.ctl(&["drain", "--node", "3"]).ok();
    let drain = op_id(&d.out);
    c.stop_meta();
    c.start_meta();
    wait_for("the map service", 60, || {
        (c.ctl(&["op", "list"]).code == 0).then_some(())
    });
    let r = c
        .ctl(&["op", "watch", "--id", &drain, "--wait", "120"])
        .ok();
    assert!(r.has(&format!("{drain} drain node 3: done")), "{}", r.out);
    // Not before the last leader (the killed process) was fenced (D125).
    let log = c.meta_log();
    assert!(
        log.contains("operations: the last leader's client"),
        "{log}"
    );
    // This leader's own client isn't: its writes go on.
    let w = id_of(
        &c.ctl(&["create-volume", "--size", "64M", "--name", "after"])
            .ok(),
    );
    c.ctl(&["snapshot", "create", "--id", &w, "--name", "s"])
        .ok();
    let list = c.ctl(&["op", "list"]).ok();
    assert!(list.has(&flatten) && list.has(&drain), "{}", list.out);
    c.ctl(&["in", "--node", "3", "--wait", "60"]).ok();
}
