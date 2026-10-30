//! Map backups and restore (04-decisions.md D86): the leader writes backups
//! by itself, `hyperstorctl map backup` fetches one, and a map service
//! whose store is gone comes back from it, with its map, join tokens and
//! revocations, as the storage nodes keep the data.

use hyperstor_pki::Role;
use hyperstor_test::*;

#[test]
fn a_lost_store_comes_back_from_a_backup() {
    let mut c = Cluster::all_in_one("backup", 3);
    // Something in every table: a volume (its header lives on the storage
    // nodes, in .meta), a token, a revoked certificate.
    let v = c
        .ctl(&["create-volume", "--size", "64M", "--name", "keep"])
        .ok();
    let vol = v.out.split_whitespace().nth(1).unwrap().to_string();
    let token = c.ctl(&["token", "create", "--role", "node"]).ok();
    let ops = c.identity(Role::Admin, "ops3", false);
    let shown = c
        .ctl(&[
            "cert",
            "show",
            "--file",
            ops.join("admin.crt").to_str().unwrap(),
        ])
        .ok();
    let serial = shown
        .out
        .lines()
        .find_map(|l| l.strip_prefix("serial   "))
        .unwrap()
        .to_string();
    c.ctl(&["cert", "revoke", "--serial", &serial]).ok();

    // The leader writes its own backups; one can be fetched too.
    let auto = c.data_dir().join("meta-backups");
    wait_for("an automatic backup", 20, || {
        std::fs::read_dir(&auto)
            .ok()?
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".hsbackup"))
            .then_some(())
    });
    let file = c.data_dir().join("taken.hsbackup");
    let r = c
        .ctl(&["map", "backup", "--out", file.to_str().unwrap()])
        .ok();
    assert!(
        r.has("wrote ") && r.has(&format!("cluster {}", c.cluster_id)),
        "{}",
        r.out
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    c.ctl(&["map", "backup", "--out", file.to_str().unwrap()])
        .expect(1);
    // Only an administrator may take one: it holds the token records.
    let reader = c.identity(Role::Node, "7", false);
    c.ctl_via(
        Via::Tls(&reader),
        &["map", "backup", "--out", "/nonexistent/x"],
    )
    .expect(4);
    let before = c.ctl(&["status", "--json"]).ok();

    // The store is lost. A restore into it while it's there is refused.
    c.stop_meta();
    let store = c.data_dir().join("meta");
    c.start_meta_with(&["--restore", file.to_str().unwrap()]);
    wait_for("the refused restore to exit", 20, || {
        c.meta_log().contains("already holds a store").then_some(())
    });
    c.stop_meta();
    std::fs::remove_dir_all(&store).unwrap();
    c.start_meta_with(&["--restore", file.to_str().unwrap()]);
    c.ctl(&["status", "--wait", "30"]).ok();
    assert!(
        c.meta_log().contains("restored the backup of map epoch"),
        "{}",
        c.meta_log()
    );

    // Everything is back: the map, the token, the revocation; the volume
    // and the data were on the storage nodes all along.
    let after = c.ctl(&["status", "--json"]).ok();
    let pools = |s: &str| s.split("\"pools\":").nth(1).map(String::from);
    assert_eq!(
        pools(&before.out).map(|p| p.len()),
        pools(&after.out).map(|p| p.len())
    );
    let tokens = c.ctl(&["token", "list"]).ok();
    let id = token.out.lines().next().unwrap();
    let id = hyperstor_pki::sha256_hex(id.trim().rsplit('-').next().unwrap().as_bytes());
    assert!(tokens.has(&id[..12]), "{}", tokens.out);
    let r = c.ctl_via(Via::Tls(&ops), &["status"]).expect(4);
    assert!(r.has("(revoked)"), "{}", r.out);
    let vols = c.ctl(&["volumes"]).ok();
    assert!(vols.has(&vol), "{}", vols.out);
    c.ctl(&["io-test", "--mib", "4"]).ok();
}
