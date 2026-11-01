//! The trash through `hyperstorctl` (19-administration.md §23.9.3, D54,
//! D115): deletes go to it and leave the lists, restore brings them back,
//! purge removes them at once (as an operation, D121), and the map
//! service's purger empties it once a pool's grace period has passed.

use hyperstor_test::*;

fn id_of(r: &Run) -> String {
    r.out.split_whitespace().nth(1).expect("an id").to_string()
}

#[test]
fn deletes_go_to_the_trash_until_purged() {
    // SAFETY: this test binary runs this one test; nothing else reads the
    // environment meanwhile. The purger looks every 300 ms.
    unsafe { std::env::set_var("HS_TRASH_SCAN_MS", "300") };
    let c = Cluster::all_in_one("trash", 3);
    let made = c
        .ctl(&["create-volume", "--size", "64M", "--name", "doc"])
        .ok();
    let v = id_of(&made);

    // A delete is undone by a restore.
    let r = c.ctl(&["delete-volume", "--id", &v]).ok();
    assert!(r.has("moved to the trash: purged at"), "{}", r.out);
    assert!(
        !c.ctl(&["volumes"]).ok().has(&v),
        "a trashed volume isn't listed"
    );
    let l = c.ctl(&["trash", "list"]).ok();
    assert!(l.has(&v) && l.has("volume") && l.has("doc"), "{}", l.out);
    let json = c.ctl(&["trash", "list", "--json"]).ok();
    assert!(json.has("\"purge_unix\""), "{}", json.out);
    let r = c.ctl(&["trash", "restore", "--id", &v]).ok();
    assert!(r.has("restored as \"doc\""), "{}", r.out);
    assert!(c.ctl(&["volumes"]).ok().has(&v));
    assert!(c.ctl(&["trash", "list"]).ok().has("The trash is empty."));

    // Snapshots too, by epoch.
    let s = c
        .ctl(&["snapshot", "create", "--id", &v, "--name", "s1"])
        .ok();
    let epoch = s.out.split_whitespace().nth(1).unwrap().to_string();
    let r = c
        .ctl(&["snapshot", "delete", "--id", &v, "--epoch", &epoch])
        .ok();
    assert!(r.has("moved to the trash"), "{}", r.out);
    assert!(!c.ctl(&["snapshot", "list", "--id", &v]).ok().has("s1"));
    assert!(c.ctl(&["trash", "list"]).ok().has("snapshot"));
    c.ctl(&["trash", "restore", "--id", &v, "--epoch", &epoch])
        .ok();
    assert!(c.ctl(&["snapshot", "list", "--id", &v]).ok().has("s1"));
    c.ctl(&["snapshot", "delete", "--id", &v, "--epoch", &epoch])
        .ok();
    // A purge is an operation (D121), followed with --wait.
    let r = c
        .ctl(&[
            "trash", "purge", "--id", &v, "--epoch", &epoch, "--wait", "60",
        ])
        .ok();
    assert!(
        r.has(&format!(
            "snapshot {epoch} of volume {v} purging: operation "
        )) && r.has(&format!("purge volume {v} snapshot {epoch}: done")),
        "{}",
        r.out
    );
    assert!(c.ctl(&["trash", "list"]).ok().has("The trash is empty."));
    // Only what's in the trash is purged.
    let r = c.ctl(&["trash", "purge", "--id", &v]).expect(1);
    assert!(r.has("isn't in the trash"), "{}", r.out);

    // A pool's settings: the default day, then a second.
    let st = c.ctl(&["trash", "settings", "--pool", "1"]).ok();
    assert!(
        st.has("kept 86400 s") && st.has("when full, the pool keeps it"),
        "{}",
        st.out
    );
    let st = c
        .ctl(&[
            "trash",
            "settings",
            "--pool",
            "1",
            "--grace",
            "1",
            "--on-full",
            "purge",
        ])
        .ok();
    assert!(
        st.has("kept 1 s") && st.has("the pool purges the oldest first"),
        "{}",
        st.out
    );
    c.ctl(&["trash", "settings", "--pool", "0", "--grace", "1"])
        .expect(1);

    // The purger empties it once the grace has passed.
    c.ctl(&["delete-volume", "--id", &v]).ok();
    wait_for("the purger", 30, || {
        c.ctl(&["trash", "list"])
            .ok()
            .has("The trash is empty.")
            .then_some(())
    });
    assert!(
        c.ctl(&["delete-volume", "--id", &v])
            .expect(1)
            .has("no volume")
    );
    // Its purge is an operation of the purger's.
    let ops = c.ctl(&["op", "list", "--json"]).ok();
    assert!(
        ops.has("\"created_by\":\"map-service/purger\"") && ops.has(&format!("volume {v}")),
        "{}",
        ops.out
    );
    let audit = c.audit();
    for (who, op) in [
        ("/admin/local", "DeleteVolume"),
        ("/admin/local", "RestoreFromTrash"),
        ("/admin/local", "SetTrashSettings"),
        ("map-service/purger", "PurgeFromTrash"),
    ] {
        assert!(
            audit
                .iter()
                .any(|l| l.contains(who) && l.contains(&format!("\"op\":\"{op}\""))),
            "{who} {op}"
        );
    }
}
