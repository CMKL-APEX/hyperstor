//! Who may do what (19-administration.md §23.8, §23.9): roles by
//! certificate, strangers and plain HTTP refused, the break-glass local
//! socket, and the audit of changes and refusals.

use hyperstor_pki::Role;
use hyperstor_test::*;

#[test]
fn roles_strangers_and_plain_http() {
    let c = Cluster::all_in_one("roles", 3);
    let node = c.identity(Role::Node, "node7", false);
    // Health ok first (I284, I307), then status exits 0.
    c.ctl_via(Via::Tls(&node), &["status", "--wait", "60"]).ok();
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
    // Health ok first (I284, I307), then status exits 0.
    c.ctl_via(Via::Socket, &["status", "--wait", "60"]).ok();
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

/// An API that isn't there is exit status 3, not a hang. With the test
/// cluster's certificates: on a host with a cluster installed, the
/// default (`/etc/hyperstor/pki`) is that cluster's, and unreadable.
#[test]
fn unreachable_api_is_status_3() {
    let c = Cluster::all_in_one("unreach", 1);
    let dead = format!("127.0.0.1:{}", free_ports(1));
    let pki = c.pki();
    let r = std::process::Command::new(c.bin.join("hsctl"))
        .args(["--api", &dead, "--tls", pki.to_str().unwrap(), "status"])
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(3));
}

/// Revocation (D56, D93): a certificate revoked by serial is refused by
/// the API from then on, with the reason, while others go on; the list
/// shows it; the local socket still works for root and the service's
/// user, whatever the list says. And on the data path (D384): the
/// command answers once storage nodes hold the map that lists it, and
/// the certificate moves no data from then on.
#[test]
fn revoked_certificates_are_refused() {
    let c = Cluster::all_in_one("revoke", 3);
    let ops = c.identity(Role::Admin, "ops2", false);
    // Health ok first (I284, I307), then status exits 0.
    c.ctl_via(Via::Tls(&ops), &["status", "--wait", "60"]).ok();
    let shown = c
        .ctl(&[
            "cert",
            "show",
            "--file",
            ops.join("admin.crt").to_str().unwrap(),
        ])
        .ok();
    assert!(shown.has("identity hyperstor://"), "{}", shown.out);
    // It moves data, the API reached through the local socket.
    c.ctl_via(Via::SocketWith(&ops), &["io-test", "--mib", "1"])
        .ok();
    let serial = shown
        .out
        .lines()
        .find_map(|l| l.strip_prefix("serial   "))
        .unwrap()
        .to_string();
    let r = c.ctl(&["cert", "revoke", "--serial", &serial]).ok();
    assert!(r.has(&format!("certificate {serial} revoked")), "{}", r.out);
    // A new cluster's mode is `allow` (D380): the nodes hold the list.
    assert!(
        r.has("storage nodes and clients refuse it too"),
        "{}",
        r.out
    );
    let r = c.ctl_via(Via::Tls(&ops), &["status"]).expect(4);
    assert!(r.has("is revoked (revoked)"), "{}", r.out);
    // The data path refuses it as well, where the socket's API doesn't
    // ask for a certificate; the administrator's own goes on.
    let r = c.ctl_via(Via::SocketWith(&ops), &["io-test", "--mib", "1"]);
    assert!(r.code != 0, "{}", r.out);
    c.ctl(&["io-test", "--mib", "1"]).ok();
    // Asked again: the same entry, and still held.
    let again = c
        .ctl(&["cert", "revoke", "--serial", &serial, "--json"])
        .ok();
    assert!(again.has("\"data_path\":\"nodes\""), "{}", again.out);
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

/// Failed password logins are paced per client address by mgmt itself:
/// after ten from one address it answers 429 with `Retry-After`, even to
/// a right password, while people at other addresses log in as before.
#[test]
fn failed_logins_are_paced_per_client_address() {
    let c = Cluster::all_in_one("login-pace", 1);
    let from = std::net::Ipv4Addr::new(127, 0, 0, 2);
    let login = |user: &str, password: &str| {
        c.web_from(
            Some(from),
            "POST",
            "/auth/login",
            &c.dashboard_host(),
            &[("Content-Type", "application/json")],
            &serde_json::json!({ "user": user, "password": password }).to_string(),
        )
    };
    // Each under another name: the PAM helper's own limit is per name.
    for i in 0..10 {
        let (code, head, body) = login(&format!("nobody{i}"), "wrong");
        assert_eq!(code, 401, "{head}{body}");
    }
    let (code, head, body) = login("bob", "wrong");
    assert_eq!(code, 429, "{body}");
    assert!(
        head.lines()
            .any(|l| l.to_ascii_lowercase().starts_with("retry-after: ")),
        "{head}"
    );
    assert!(body.contains("rate_limited"), "{body}");
    assert_eq!(login("bob", PASSWORD).0, 429);
    // Another address: a right password still logs in.
    c.login("bob", None).unwrap();
}

/// The sealing keys' bytes as `dir`'s identity gets them from the API.
fn sealing_keys_as(
    c: &Cluster,
    dir: &std::path::Path,
) -> Result<Vec<(u8, u64, Vec<u8>)>, connectrpc::ConnectError> {
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
    let tls =
        hyperstor_pki::client_config(&read("admin.crt"), &read("admin.key"), &read("root.crt"))
            .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = connectrpc::client::HttpClient::with_tls(std::sync::Arc::new(tls));
        let config =
            connectrpc::client::ClientConfig::new(format!("https://{}", c.api).parse().unwrap());
        let r = hyperstor_api::CaServiceClient::new(client, config)
            .get_sealing_keys(hyperstor_api::GetSealingKeysRequest::default())
            .await?
            .into_owned();
        Ok(r.keys
            .into_iter()
            .map(|k| (k.id as u8, k.seq, k.key))
            .collect())
    })
}

/// Sealing keys (D133, D363, D370): the first is made when a storage node
/// asks, which an installed node does at start; a rotation makes the next
/// and keeps the one before, so what the old key sealed still opens until
/// the rotation after; nodes hold what the cluster has; only storage
/// nodes and members read a key's bytes; and the audit names each
/// rotation, never a key.
#[test]
fn sealing_keys_rotate_and_reach_only_storage_nodes() {
    use hyperstor_pki::seal::{Access, Capability, CapabilityError, SealingKey, SealingKeys};
    let c = Cluster::all_in_one("sealkeys", 1);
    c.ctl(&["status", "--wait", "60"]).ok();
    // How many keys node 0 says it holds.
    let held = |c: &Cluster| {
        c.metrics(0)
            .lines()
            .find_map(|l| l.strip_prefix("hyperstor_sealing_keys{node=\"0\"} "))
            .and_then(|v| v.parse::<usize>().ok())
    };
    // The node asked at start, with its own certificate (D378), and that
    // made the first key.
    wait_for("node 0 to hold the first sealing key", 30, || {
        (held(&c) == Some(1)).then_some(())
    });
    let one = c.ctl(&["sealing-key", "list"]).ok();
    assert!(
        one.has("/node/0") && !one.has("no sealing key yet"),
        "{}",
        one.out
    );

    // An administrator lists and rotates, but never reads a key.
    let e = sealing_keys_as(&c, &c.pki()).expect_err("an administrator may not read them");
    assert!(
        e.message
            .as_deref()
            .unwrap_or("")
            .contains("may not read the sealing keys"),
        "{e:?}"
    );

    // A storage node with a certificate of its own reads the same key,
    // and asking makes no other.
    let node = c.identity(Role::Node, "node7", false);
    let first = sealing_keys_as(&c, &node).unwrap();
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!((first[0].0, first[0].1, first[0].2.len()), (1, 1, 32));
    assert_eq!(sealing_keys_as(&c, &node).unwrap(), first);
    // A node may not rotate.
    c.ctl_via(Via::Tls(&node), &["sealing-key", "rotate"])
        .expect(4);

    let holder = |keys: &[(u8, u64, Vec<u8>)]| {
        SealingKeys::new(
            keys.iter()
                .map(|(id, _, k)| SealingKey::new(*id, k.as_slice().try_into().unwrap()))
                .collect(),
        )
    };
    let cap = Capability {
        tenant: 3,
        lineage: 0xabc,
        branch: 0,
        access: Access::ReadWrite,
        instance: 99,
        host: "h1".into(),
        expires_unix: u64::MAX,
    };
    let sealed_by_1 = holder(&first).seal(&cap).unwrap();

    let r = c.ctl(&["sealing-key", "rotate"]).ok();
    assert!(
        r.has("sealing key 2 made") && r.has("key 1 still opens"),
        "{}",
        r.out
    );
    let second = sealing_keys_as(&c, &node).unwrap();
    assert_eq!(
        second.iter().map(|k| (k.0, k.1)).collect::<Vec<_>>(),
        vec![(1, 1), (2, 2)]
    );
    // The node holds both by its next fetch.
    wait_for("node 0 to hold the second sealing key", 60, || {
        (held(&c) == Some(2)).then_some(())
    });
    // Key 1 is the key it was, and still opens what it sealed.
    assert_eq!(second[0], first[0]);
    assert_ne!(second[1].2, first[0].2);
    assert_eq!(
        holder(&second).open(&sealed_by_1, 0, 99, "h1"),
        Ok(cap.clone())
    );
    let sealed_by_2 = holder(&second).seal(&cap).unwrap();

    let l = c.ctl(&["sealing-key", "list"]).ok();
    let row = |id: &str| {
        l.out
            .lines()
            .find(|x| x.starts_with(&format!("{id} ")))
            .unwrap_or_else(|| panic!("no key {id}: {}", l.out))
            .to_string()
    };
    assert!(
        row("1").contains("opens") && row("2").contains("seals"),
        "{}",
        l.out
    );
    assert!(
        row("1").contains("/node/0") && row("2").contains("/admin/"),
        "{}",
        l.out
    );
    // Listed as JSON, without any key's bytes.
    let j = c.ctl(&["sealing-key", "list", "--json"]).ok();
    assert!(j.has("\"current\":true") && !j.has("\"key\""), "{}", j.out);

    // The rotation after drops key 1: what it sealed no longer opens.
    c.ctl(&["sealing-key", "rotate"]).ok();
    let third = sealing_keys_as(&c, &node).unwrap();
    assert_eq!(third.iter().map(|k| k.0).collect::<Vec<_>>(), vec![2, 3]);
    assert_eq!(
        holder(&third).open(&sealed_by_1, 0, 99, "h1"),
        Err(CapabilityError::UnknownKey(1))
    );
    assert_eq!(holder(&third).open(&sealed_by_2, 0, 99, "h1"), Ok(cap));

    // Each key's making is audited, by id; no key's bytes are.
    let audit = c.audit();
    let made = |who: &str, what: &str| {
        audit.iter().any(|l| {
            l.contains("\"op\":\"RotateSealingKey\"")
                && l.contains(who)
                && l.contains(what)
                && l.contains("\"result\":\"ok")
        })
    };
    assert!(made("/node/0", "sealing key 1"), "{audit:#?}");
    assert!(made("/admin/", "sealing key 2"), "{audit:#?}");
    assert!(made("/admin/", "sealing key 3"), "{audit:#?}");
    assert!(
        audit
            .iter()
            .any(|l| l.contains("GetSealingKeys") && l.contains("\"result\":\"refused: ")),
        "{audit:#?}"
    );
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    for (_, _, k) in first.iter().chain(&third) {
        assert!(!audit.iter().any(|l| l.contains(&hex(k))));
    }
}

/// A pool made to require capabilities (D371) with `hsctl pool set
/// --capabilities on` serves no client that shows none, as `io-test`
/// doesn't; says so in `pools`; is audited; and serves again once it is
/// turned off.
#[test]
fn a_pool_requires_capabilities_when_told() {
    let c = Cluster::all_in_one("capspool", 1);
    c.ctl(&["status", "--wait", "60"]).ok();
    c.ctl(&["io-test", "--mib", "1"]).ok();

    let on = c
        .ctl(&["pool", "set", "--pool", "1", "--capabilities", "on"])
        .ok();
    assert!(
        on.has("serves clients only what they show a capability for"),
        "{}",
        on.out
    );
    let pools = c.ctl(&["pools"]).ok();
    assert!(pools.has("pool 1: requires capabilities"), "{}", pools.out);
    assert!(
        c.ctl(&["pools", "--json"])
            .ok()
            .has("\"capabilities\":true")
    );
    // The node takes the map within moments; from then on a client
    // without a capability is refused, and told why.
    let refused = wait_for("io-test to be refused", 30, || {
        let r = c.ctl(&["io-test", "--mib", "1"]);
        (r.code != 0).then_some(r)
    });
    assert!(
        refused.has("Denied") && refused.has("no capability shown"),
        "{}",
        refused.out
    );
    // .meta keeps none: volumes' headers are fenced by leases.
    c.ctl(&["pool", "set", "--pool", "0", "--capabilities", "on"])
        .expect(1);
    // Only `pool set` takes it.
    c.ctl(&["pools", "--capabilities", "on"]).expect(2);

    let off = c
        .ctl(&["pool", "set", "--pool", "1", "--capabilities", "off"])
        .ok();
    assert!(
        off.has("serves clients without capabilities"),
        "{}",
        off.out
    );
    wait_for("io-test to be served again", 30, || {
        (c.ctl(&["io-test", "--mib", "1"]).code == 0).then_some(())
    });
    assert!(!c.ctl(&["pools"]).ok().has("requires capabilities"));

    let audit = c.audit();
    for what in ["capabilities=on", "capabilities=off"] {
        assert!(
            audit.iter().any(|l| l.contains("\"op\":\"SetPool\"")
                && l.contains(what)
                && l.contains("\"result\":\"ok")),
            "{what}: {audit:#?}"
        );
    }
}

/// The data path's mutual TLS on an installed cluster (D373, D380;
/// 30-data-path-tls.md §33.4). A new cluster starts in `allow`: its
/// nodes, its map service and `hsctl` each hold a certificate, so
/// every connection authenticates and no node serves one that didn't.
/// `require` is taken at once, as every node holds one (D377): the
/// cluster's own connections go on, none refused. Turned `off`,
/// connections made from then on don't authenticate, and are counted;
/// `require` isn't offered from `off`. Each change is audited.
#[test]
fn a_new_clusters_connections_authenticate() {
    let c = Cluster::all_in_one("datatls", 3);
    c.ctl(&["status", "--wait", "60"]).ok();
    let unauthenticated =
        |c: &Cluster| c.metric_sum("hyperstor_unauthenticated_connections_total", "");
    let s = c.ctl(&["data-tls", "status"]).ok();
    assert!(s.has("data path TLS: allow"), "{}", s.out);
    // And it encrypts off trusted networks (D405); its own addresses are
    // loopback, on no network, so its sessions stay plaintext.
    assert!(s.has("sessions stay encrypted"), "{}", s.out);
    // Every connection of a new cluster shows a certificate: the list of
    // those that don't is empty, and says so.
    assert!(
        s.has("no address connected without a certificate"),
        "{}",
        s.out
    );
    assert!(
        c.ctl(&["data-tls", "status", "--json"])
            .ok()
            .has("\"data_tls\":\"allow\"")
    );
    // Writes through three nodes, replicated among them, read back: all
    // of it over authenticated connections.
    c.ctl(&["io-test", "--mib", "4"]).ok();
    assert_eq!(unauthenticated(&c), 0, "{}", c.metrics(0));

    // Every node holds a certificate and refuses by it: `require`.
    let refused = |c: &Cluster| c.metric_sum("hyperstor_refused_connections_total", "");
    let r = c.ctl(&["data-tls", "set", "--mode", "require"]).ok();
    assert!(r.has("data path TLS: require, was allow"), "{}", r.out);
    assert!(
        c.ctl(&["data-tls", "status"])
            .ok()
            .has("only authenticated connections are served")
    );
    // Writes, replication, heartbeats and the tool go on; nothing of the
    // cluster's own was refused, here or in `allow`.
    c.ctl(&["io-test", "--mib", "4"]).ok();
    c.ctl(&["status", "--wait", "60"]).ok();
    assert_eq!(refused(&c), 0, "{}", c.metrics(0));
    assert_eq!(unauthenticated(&c), 0, "{}", c.metrics(0));
    c.ctl(&["data-tls", "set", "--mode", "on"]).expect(2);
    c.ctl(&["data-tls", "set"]).expect(2);

    let off = c.ctl(&["data-tls", "set", "--mode", "off"]).ok();
    assert!(off.has("data path TLS: off, was require"), "{}", off.out);
    assert!(
        c.ctl(&["data-tls", "status"])
            .ok()
            .has("data path TLS: off")
    );
    // Not from `off`: nodes show what they hold in `allow` first.
    let r = c.ctl(&["data-tls", "set", "--mode", "require"]);
    assert!(r.code != 0 && r.has("allow first"), "{}", r.out);
    // Connections made from now on don't authenticate: the tool's next
    // ones are served, and counted.
    wait_for("an unauthenticated connection to be counted", 30, || {
        c.ctl(&["io-test", "--mib", "1"]).ok();
        (unauthenticated(&c) > 0).then_some(())
    });
    let on = c.ctl(&["data-tls", "set", "--mode", "allow"]).ok();
    assert!(on.has("data path TLS: allow, was off"), "{}", on.out);
    c.ctl(&["io-test", "--mib", "1"]).ok();

    let audit = c.audit();
    for (what, result) in [
        ("mode=require", "\"result\":\"ok, was allow"),
        ("mode=require", "\"result\":\"failed: "),
        ("mode=off", "\"result\":\"ok, was require"),
        ("mode=allow", "\"result\":\"ok, was off"),
    ] {
        assert!(
            audit.iter().any(|l| l.contains("\"op\":\"SetDataTls\"")
                && l.contains(what)
                && l.contains(result)),
            "{what}: {audit:#?}"
        );
    }
}
