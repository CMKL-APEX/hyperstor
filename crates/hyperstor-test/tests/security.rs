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
    // Refusals exit 4, with the API's reason.
    let r = c
        .ctl_via(Via::Tls(&node), &["out", "--device", "0"])
        .expect(4);
    assert!(
        r.has("may not change the cluster (permission_denied)"),
        "{}",
        r.out
    );
    let host = c.identity(Role::Host, "host1", false);
    let r = c.ctl_via(Via::Tls(&host), &["status"]).expect(4);
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

/// Revocation (D56, D93): a certificate revoked by serial is refused by
/// the API from then on, with the reason, while others go on; the list
/// shows it; the local socket still works for root and the service's
/// user, whatever the list says.
#[test]
fn revoked_certificates_are_refused() {
    let c = Cluster::all_in_one("revoke", 3);
    let ops = c.identity(Role::Admin, "ops2", false);
    c.ctl_via(Via::Tls(&ops), &["status"]).ok();
    let shown = c
        .ctl(&[
            "cert",
            "show",
            "--file",
            ops.join("admin.crt").to_str().unwrap(),
        ])
        .ok();
    assert!(shown.has("identity hyperstor://"), "{}", shown.out);
    let serial = shown
        .out
        .lines()
        .find_map(|l| l.strip_prefix("serial   "))
        .unwrap()
        .to_string();
    let r = c.ctl(&["cert", "revoke", "--serial", &serial]).ok();
    assert!(r.has(&format!("certificate {serial} revoked")), "{}", r.out);
    let r = c.ctl_via(Via::Tls(&ops), &["status"]).expect(4);
    assert!(r.has("is revoked (revoked)"), "{}", r.out);
    // Others are untouched; the list shows it.
    let l = c.ctl(&["cert", "list"]).ok();
    assert!(l.has(&serial) && l.has("revoked"), "{}", l.out);
    let json = c.ctl(&["cert", "list", "--json"]).ok();
    assert!(
        json.has(&format!("\"serial\":\"{serial}\"")),
        "{}",
        json.out
    );
    c.ctl_via(Via::Socket, &["status"]).ok();
    // A malformed serial is refused as such; the change is audited.
    let r = c.ctl(&["cert", "revoke", "--serial", "xyz"]).expect(1);
    assert!(r.has("(invalid_argument)"), "{}", r.out);
    assert!(
        c.audit()
            .iter()
            .any(|l| l.contains("\"op\":\"RevokeCertificate\"") && l.contains(&serial))
    );
}

/// mgmt's rows (D98): the `mgmt` identity reads and writes them through
/// the cluster API; any other identity, the administrator's included, is
/// refused.
#[test]
fn only_mgmt_reaches_its_rows() {
    let c = Cluster::all_in_one("mgmtrows", 1);
    let pki = c.pki();
    let call = |who: &str, method: &str, body: &str| {
        let o = std::process::Command::new("curl")
            .args(["-sS", "--cacert"])
            .arg(pki.join("root.crt"))
            .arg("--cert")
            .arg(pki.join(format!("{who}.crt")))
            .arg("--key")
            .arg(pki.join(format!("{who}.key")))
            .args(["-H", "Content-Type: application/json", "-d", body])
            .arg(format!(
                "https://{}/hyperstor.v1.MgmtStoreService/{method}",
                c.api
            ))
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    // "hello", base64 as the JSON form of bytes.
    let put = r#"{"key":"user/1","value":"aGVsbG8=","if_absent":true}"#;
    assert_eq!(call("mgmt", "PutRow", put), "{}");
    assert!(call("mgmt", "PutRow", put).contains("already_exists"));
    let got = call("mgmt", "GetRow", r#"{"key":"user/1"}"#);
    assert!(
        got.contains(r#""found":true"#) && got.contains("aGVsbG8"),
        "{got}"
    );
    let listed = call("mgmt", "ListRows", r#"{"prefix":"user/"}"#);
    assert!(listed.contains(r#""key":"user/1""#), "{listed}");
    let refused = call("admin", "GetRow", r#"{"key":"user/1"}"#);
    assert!(refused.contains("permission_denied"), "{refused}");
    assert!(call("mgmt", "DeleteRow", r#"{"key":"user/1"}"#).contains(r#""found":true"#));
    assert!(!call("mgmt", "GetRow", r#"{"key":"user/1"}"#).contains("found"));
}

/// Only mgmt names the person a call is for (I135); the map service then
/// audits it as mgmt for that person. Anyone else naming one is refused,
/// and so is mgmt naming something that isn't a person.
#[test]
fn only_mgmt_names_a_person() {
    let c = Cluster::all_in_one("onbehalf", 1);
    let pki = c.pki();
    let call = |who: &str, person: &str| {
        let o = std::process::Command::new("curl")
            .args(["-sS", "--cacert"])
            .arg(pki.join("root.crt"))
            .arg("--cert")
            .arg(pki.join(format!("{who}.crt")))
            .arg("--key")
            .arg(pki.join(format!("{who}.key")))
            .args(["-H", "Content-Type: application/json"])
            .args(["-H", &format!("X-Hyperstor-On-Behalf-Of: {person}")])
            .args(["-d", r#"{"pool":1,"size":"1048576","name":"for-ann"}"#])
            .arg(format!(
                "https://{}/hyperstor.v1.ClusterService/CreateVolume",
                c.api
            ))
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let refused = call("admin", "user:ann");
    assert!(refused.contains("permission_denied"), "{refused}");
    let refused = call("mgmt", "ann; rm -rf /");
    assert!(refused.contains("permission_denied"), "{refused}");
    let made = call("mgmt", "user:ann");
    assert!(made.contains("\"volume\""), "{made}");
    assert!(c.audit().iter().any(|l| {
        l.contains("\"op\":\"CreateVolume\"")
            && l.contains("/mgmt/dashboard for user:ann\"")
            && !l.contains("refused")
    }));
}
