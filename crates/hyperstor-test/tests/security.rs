//! Who may do what (administration.md §23.8, §23.9): roles by
//! certificate, strangers and plain HTTP refused, the break-glass local
//! socket, and the audit of changes and refusals.

use hyperstor_pki::Role;
use hyperstor_test::*;

#[test]
fn roles_strangers_and_plain_http() {
    let c = Cluster::all_in_one("roles", 3);
    let node = c.identity(Role::Node, "node7", false);
    c.ctl_via(Via::Tls(&node), &["status"]).ok();
    let r = c
        .ctl_via(Via::Tls(&node), &["out", "--device", "0"])
        .expect(1);
    assert!(r.has("may not change the cluster"), "{}", r.out);
    let host = c.identity(Role::Host, "host1", false);
    let r = c.ctl_via(Via::Tls(&host), &["status"]).expect(1);
    assert!(r.has("may not read"), "{}", r.out);
    let mgmt = c.identity(Role::Mgmt, "mgmt1", false);
    c.ctl_via(Via::Tls(&mgmt), &["in", "--device", "0"]).ok();
    // Another cluster's CA fails the handshake; plain HTTP gets nowhere.
    let stranger = c.identity(Role::Admin, "stranger", true);
    assert_ne!(c.ctl_via(Via::Tls(&stranger), &["status"]).code, 0);
    assert_ne!(c.ctl_via(Via::Plain, &["status"]).code, 0);

    // Every change and every refusal is audited, reads are not.
    let audit = c.audit();
    let id = &c.cluster_id;
    assert!(
        audit.iter().any(
            |l| l.contains(&format!("\"principal\":\"hyperstor://{id}/mgmt/mgmt1\""))
                && l.contains("\"op\":\"SetDeviceIn\"")
                && l.contains("\"result\":\"ok, map epoch")
        ),
        "{audit:#?}"
    );
    assert!(
        audit
            .iter()
            .any(|l| l.contains("/node/node7\"") && l.contains("\"result\":\"refused: ")),
        "{audit:#?}"
    );
    assert!(
        audit
            .iter()
            .any(|l| l.contains("/host/host1\"") && l.contains("GetStatus")),
        "{audit:#?}"
    );
    assert!(
        !audit
            .iter()
            .any(|l| l.contains("\"result\":\"ok") && l.contains("GetStatus")),
        "{audit:#?}"
    );
}

/// The local socket needs no certificate; the kernel names the caller,
/// here the service's own user. Its changes are audited as local.
#[test]
fn local_socket_break_glass() {
    let c = Cluster::all_in_one("socket", 3);
    c.ctl_via(Via::Socket, &["status"]).ok();
    c.ctl_via(Via::Socket, &["out", "--device", "0"]).ok();
    c.ctl_via(Via::Socket, &["in", "--device", "0", "--wait", "30"])
        .ok();
    assert!(
        c.audit()
            .iter()
            .any(|l| l.contains("\"principal\":\"local:uid=") && l.contains("SetDeviceIn")),
        "{:#?}",
        c.audit()
    );
}

/// An API that isn't there is exit status 3, not a hang.
#[test]
fn unreachable_api_is_status_3() {
    let c = Cluster::all_in_one("unreach", 1);
    let dead = format!("127.0.0.1:{}", free_ports(1));
    let r = std::process::Command::new(c.bin.join("hyperstorctl"))
        .args(["--api", &dead, "status"])
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(3));
}
