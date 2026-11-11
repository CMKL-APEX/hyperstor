//! The management service (`hyperstor-mgmt`, 19-administration.md §23.7,
//! §23.9): people's logins, the first administrator's claim, roles, API
//! tokens and step-up, the cluster API on people's behalf, the dashboard's
//! data, and the browser protections.

use hyperstor_test::*;

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

#[test]
fn people_log_in_and_do_what_their_roles_allow() {
    let mut c = Cluster::all_in_one("dash", 3);
    let host = c.dashboard_host();

    // The page loads for anyone; its data needs a login.
    let (code, head, body) = c.dashboard("GET", "/", &host);
    assert_eq!(code, 200, "{head}");
    assert!(body.contains("<title>Hyperstor</title>"), "{body}");
    assert!(
        head.contains("script-src 'self'") && head.contains("frame-ancestors 'none'"),
        "{head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("x-content-type-options: nosniff"),
        "{head}"
    );
    let (code, _, js) = c.dashboard("GET", "/app.js", &host);
    assert_eq!(code, 200);
    assert!(js.contains("/api/status") && js.contains("/auth/login"));
    let (code, _, b) = c.dashboard("GET", "/api/status", &host);
    assert_eq!(code, 401, "{b}");
    assert_eq!(json(&b)["reason"], "unauthenticated");
    let (_, _, m) = c.dashboard("GET", "/auth/methods", &host);
    assert_eq!(json(&m)["claim_open"], true, "{m}");

    // Wrong passwords, and people outside hyperstor-users, don't get in.
    assert_eq!(c.login("nobody", None).unwrap_err().0, 401);
    assert_eq!(c.login("eve", None).unwrap_err().0, 401);
    let (code, _, _) = c.web(
        "POST",
        "/auth/login",
        &host,
        &[("Content-Type", "application/json")],
        r#"{"user":"ann","password":"wrong"}"#,
    );
    assert_eq!(code, 401);

    // The first security administrator claims the cluster with the code
    // printed at start; a wrong code is refused, and a claim is once.
    assert_eq!(c.login("ann", Some("not-the-code")).unwrap_err().0, 400);
    let code = c.claim_code();
    let ann = c.login("ann", Some(&code)).unwrap();
    assert_eq!(ann.roles, ["security-admin"]);
    assert!(
        ann.set_cookie.contains("HttpOnly") && ann.set_cookie.contains("SameSite=Strict"),
        "{}",
        ann.set_cookie
    );
    assert_eq!(c.login("bob", Some(&code)).unwrap_err().0, 400);
    let (_, _, m) = c.dashboard("GET", "/auth/methods", &host);
    assert_eq!(json(&m)["claim_open"], false, "{m}");
    let (code, _, w) = c.as_session(&ann, "GET", "/auth/whoami", "");
    assert_eq!(code, 200, "{w}");
    assert_eq!(json(&w)["user"], "ann");
    // A person logged in may send bodies over the 256 KiB anyone may
    // (#329's follow-up; web.rs checks the refusals).
    let (code, _, w) = c.as_session(&ann, "GET", "/auth/whoami", &"x".repeat(1 << 20));
    assert_eq!(code, 200, "{w}");

    // Security administrators manage people, not storage.
    assert_eq!(c.as_session(&ann, "GET", "/api/status", "").0, 403);
    // A first login makes an account with no roles.
    let bob = c.login("bob", None).unwrap();
    assert!(bob.roles.is_empty(), "{:?}", bob.roles);
    assert_eq!(c.as_session(&bob, "GET", "/api/status", "").0, 403);
    assert_eq!(c.as_session(&bob, "GET", "/auth/users", "").0, 403);
    let (code, _, u) = c.as_session(&ann, "GET", "/auth/users", "");
    assert_eq!(code, 200, "{u}");
    assert_eq!(json(&u)["users"].as_array().unwrap().len(), 2, "{u}");

    // A write with a cookie needs its CSRF value, and a cross-site origin
    // is refused.
    let roles = r#"{"roles":["viewer"]}"#;
    let (code, _, b) = c.web(
        "PUT",
        "/auth/users/bob/roles",
        &host,
        &[
            ("Cookie", &ann.cookie),
            ("Content-Type", "application/json"),
        ],
        roles,
    );
    assert_eq!(
        (code, json(&b)["reason"].clone()),
        (403, "csrf".into()),
        "{b}"
    );
    let (code, _, b) = c.web(
        "PUT",
        "/auth/users/bob/roles",
        &host,
        &[
            ("Cookie", &ann.cookie),
            ("X-Hyperstor-CSRF", &ann.csrf),
            ("Origin", "https://evil.example"),
        ],
        roles,
    );
    assert_eq!(
        (code, json(&b)["reason"].clone()),
        (403, "bad_origin".into()),
        "{b}"
    );
    assert_eq!(
        c.as_session(&bob, "PUT", "/auth/users/bob/roles", roles).0,
        403
    );
    let (code, _, b) = c.as_session(&ann, "PUT", "/auth/users/bob/roles", roles);
    assert_eq!(code, 200, "{b}");
    let king = r#"{"roles":["king"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/bob/roles", king).0,
        400
    );
    let both = r#"{"roles":["security-admin","storage-admin"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/ann/roles", both).0,
        200
    );

    // Viewers read: the dashboard's data, and the cluster API through mgmt
    // (Connect, JSON) with mgmt's identity on their behalf.
    let (code, _, status) = c.as_session(&bob, "GET", "/api/status", "");
    assert_eq!(code, 200, "{status}");
    assert!(status.contains("\"health\":\"HEALTH_OK\""), "{status}");
    assert!(
        status.contains(&format!("\"cluster_id\":\"{}\"", c.cluster_id)),
        "{status}"
    );
    let (code, _, pgs) = c.as_session(&bob, "GET", "/api/pgs?pool=1", "");
    assert_eq!(code, 200, "{pgs}");
    assert_eq!(pgs.matches("\"acting\"").count(), 64, "{pgs}");
    assert_eq!(c.as_session(&bob, "GET", "/api/pgs", "").0, 400);
    let get_status = "/api/hyperstor.v1.ClusterService/GetStatus";
    let (code, _, s) = c.as_session(&bob, "POST", get_status, "{}");
    assert_eq!(code, 200, "{s}");
    assert!(s.contains("HEALTH_OK"), "{s}");
    // But don't change anything.
    let create = "/api/hyperstor.v1.ClusterService/CreateVolume";
    let vol = r#"{"pool":1,"size":"1048576","name":"by-mgmt"}"#;
    let (code, _, b) = c.as_session(&bob, "POST", create, vol);
    assert_eq!(
        (code, json(&b)["reason"].clone()),
        (403, "permission_denied".into()),
        "{b}"
    );
    // Machines' calls aren't anyone's.
    let rows = "/api/hyperstor.v1.MgmtStoreService/ListRows";
    assert_eq!(c.as_session(&ann, "POST", rows, "{}").0, 403);

    // Storage administrators manage volumes, through the same API; the map
    // service hears whom for, whatever a browser claims (I135).
    let (code, _, b) = c.web(
        "POST",
        create,
        &host,
        &[
            ("Cookie", &ann.cookie),
            ("X-Hyperstor-CSRF", &ann.csrf),
            ("Content-Type", "application/json"),
            ("X-Hyperstor-On-Behalf-Of", "user:bob"),
        ],
        vol,
    );
    assert_eq!(code, 200, "{b}");
    let for_ann = c
        .audit()
        .into_iter()
        .find(|l| l.contains("\"op\":\"CreateVolume\""));
    assert!(
        for_ann
            .as_deref()
            .is_some_and(|l| l.contains("/mgmt/dashboard for user:ann\"")),
        "{for_ann:?}"
    );
    let id = json(&b)["volume"]["id"].as_str().unwrap().to_string();
    assert!(c.ctl(&["volumes"]).ok().has("by-mgmt"));

    // API tokens: shown once, listed without the secret, good for reads
    // and writes, but never fresh enough for what is hard to undo.
    let (code, _, b) = c.as_session(&ann, "POST", "/auth/tokens", r#"{"name":"ci","days":7}"#);
    assert_eq!(code, 200, "{b}");
    let t = json(&b);
    let token = t["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("hst_"));
    let (_, _, list) = c.as_session(&ann, "GET", "/auth/tokens", "");
    assert!(list.contains("\"ci\"") && !list.contains(&token), "{list}");
    let bearer = format!("Bearer {token}");
    let by_token = |method: &str, path: &str, body: &str| {
        c.web(
            method,
            path,
            &host,
            &[
                ("Authorization", &bearer),
                ("Content-Type", "application/json"),
            ],
            body,
        )
    };
    assert_eq!(by_token("GET", "/api/status", "").0, 200);
    let (code, _, b) = by_token(
        "POST",
        create,
        r#"{"pool":1,"size":"1048576","name":"by-token"}"#,
    );
    assert_eq!(code, 200, "{b}");
    let delete = "/api/hyperstor.v1.ClusterService/DeleteVolume";
    let del = format!(r#"{{"id":"{id}"}}"#);
    let (code, _, b) = by_token("POST", delete, &del);
    assert_eq!(
        (code, json(&b)["reason"].clone()),
        (401, "reauth_required".into()),
        "{b}"
    );
    // A fresh login can.
    let (code, _, b) = c.as_session(&ann, "POST", delete, &del);
    assert_eq!(code, 200, "{b}");
    // hyperstorctl through mgmt, as ann with her token: the same roles,
    // the same step-up, the API's refusals as hyperstorctl shows them.
    let ctl = |args: &[&str], token: &str| {
        let o = std::process::Command::new(c.bin.join("hyperstorctl"))
            .args(["--mgmt", &c.mgmt_url()])
            .arg("--mgmt-ca")
            .arg(c.pki().join("root.crt"))
            .args(args)
            .env("HYPERSTOR_TOKEN", token)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        (o.status.code().unwrap_or(-1), text)
    };
    let (code, out) = ctl(&["status"], &token);
    assert_eq!(code, 0, "{out}");
    let (code, out) = ctl(
        &["create-volume", "--size", "1M", "--name", "by-ctl"],
        &token,
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = ctl(&["volumes", "--json"], &token);
    assert!(code == 0 && out.contains("by-ctl"), "{out}");
    let (code, out) = ctl(&["delete-volume", "--id", &id], &token);
    assert!(code == 4 && out.contains("(reauth_required)"), "{out}");
    let (code, out) = ctl(&["status"], "hst_not-a-token");
    assert!(code == 4 && out.contains("(unauthenticated)"), "{out}");
    let out = std::process::Command::new(c.bin.join("hyperstorctl"))
        .args(["--mgmt", "http://192.0.2.1:8852", "status"])
        .env("HYPERSTOR_TOKEN", &token)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "no token in clear beyond loopback"
    );
    // A revoked token is gone.
    let tid = t["info"]["id"].as_str().unwrap();
    let revoke = format!("/auth/tokens/{tid}");
    assert_eq!(c.as_session(&ann, "DELETE", &revoke, "").0, 200);
    assert_eq!(by_token("GET", "/api/status", "").0, 401);

    // What people did is audited, as themselves.
    let log = c.mgmt_log();
    let audited = |principal: &str, op: &str, result: &str| {
        log.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|r| {
                r["principal"]
                    .as_str()
                    .is_some_and(|p| p.starts_with(principal))
                    && r["op"] == op
                    && r["result"].as_str().is_some_and(|x| x.starts_with(result))
            })
    };
    for (principal, op, result) in [
        ("user:ann", "Claim", "ok"),
        ("user:ann", "ClusterService.CreateVolume", "status 200"),
        ("user:ann", "ClusterService.DeleteVolume", "refused"),
        ("user:ann", "ClusterService.DeleteVolume", "status 200"),
        ("user:ann", "People.SetRoles", "ok"),
        ("user:bob", "ClusterService.CreateVolume", "refused"),
        ("user:eve", "Login", "refused"),
    ] {
        assert!(
            audited(principal, op, result),
            "{principal} {op} {result} not in:\n{log}"
        );
    }
    assert!(!log.contains(&token), "a token in the log");

    // Logging out ends the session.
    let (code, head, _) = c.as_session(&bob, "POST", "/auth/logout", "");
    assert_eq!(code, 200);
    assert!(head.contains("Max-Age=0"), "{head}");
    assert_eq!(c.as_session(&bob, "GET", "/api/status", "").0, 401);

    // Only this machine's names (DNS rebinding).
    assert_eq!(c.dashboard("GET", "/", "evil.example").0, 421);
    assert_eq!(c.dashboard("GET", "/", "evil.example:80").0, 421);
    assert_eq!(c.as_session(&ann, "GET", "/nope", "").0, 404);

    // Every node's metrics; a node gone says so, and status reports it.
    c.ctl(&["io-test", "--mib", "4"]).ok();
    let (code, _, m) = c.as_session(&ann, "GET", "/api/metrics", "");
    assert_eq!(code, 200, "{m}");
    assert_eq!(m.matches("\"up\":true").count(), 3, "{m}");
    assert!(m.contains("[\"hyperstor_requests_total\",{"), "{m}");
    c.kill_node(2);
    let m = wait_for("node 2 without metrics", 10, || {
        let (_, _, m) = c.as_session(&ann, "GET", "/api/metrics", "");
        (m.matches("\"up\":false").count() == 1).then_some(m)
    });
    assert_eq!(m.matches("\"up\":true").count(), 2, "{m}");
    wait_for("node 2 down in the status", 20, || {
        let (_, _, s) = c.as_session(&ann, "GET", "/api/status", "");
        s.contains("node(s) down: 2").then_some(())
    });

    // Accounts and sessions are the map service's rows: they outlive a
    // restart of mgmt, and no new claim is offered.
    c.restart_mgmt();
    assert_eq!(c.as_session(&ann, "GET", "/api/status", "").0, 200);
    assert!(!c.mgmt_log().contains("claim code"));
}

/// Roles granted on a realm (D252, D260, D262–D265, D283, D286): a storage
/// administrator of acme sees and changes only acme's tenants' volumes and
/// their operations, and the realm's own things, and nothing of the
/// cluster as a whole; creations in realms name their tenant or realm;
/// lists name each row's realm; a security administrator of acme manages
/// acme's keys and acme's grants only; `--in-realm` narrows anyone to one
/// realm; and the realms mgmt names can't be forged by a browser.
#[test]
fn roles_on_a_realm_reach_only_it() {
    let c = Cluster::all_in_one("realmroles", 3);
    let ann = c.login("ann", Some(&c.claim_code())).unwrap();
    let both = r#"{"roles":["security-admin","storage-admin"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/ann/roles", both).0,
        200
    );
    // Tenant 7 is acme's; tenant 8 and volumes with no tenant are
    // default's.
    c.ctl(&["realm", "create", "--name", "acme"]).ok();
    c.ctl(&["realm", "tenant", "--tenant", "7", "--realm", "acme"])
        .ok();
    let vol = |tenant: &str, name: &str| {
        let made = c
            .ctl(&[
                "create-volume",
                "--size",
                "1M",
                "--name",
                name,
                "--tenant",
                tenant,
            ])
            .ok();
        made.out.split_whitespace().nth(1).unwrap().to_string()
    };
    let acme_vol = vol("7", "acme-vol");
    let other_vol = vol("8", "other-vol");
    c.ctl(&["s3", "key", "create", "--tenant", "7"]).ok();
    c.ctl(&["s3", "key", "create", "--tenant", "8"]).ok();

    // Grants name a realm that exists.
    let bob = c.login("bob", None).unwrap();
    let cy = c.login("cy", None).unwrap();
    let nope = r#"{"roles":["viewer@nope"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/bob/roles", nope).0,
        404
    );
    let r = r#"{"roles":["storage-admin@acme"]}"#;
    assert_eq!(c.as_session(&ann, "PUT", "/auth/users/bob/roles", r).0, 200);
    let r = r#"{"roles":["security-admin@acme"]}"#;
    assert_eq!(c.as_session(&ann, "PUT", "/auth/users/cy/roles", r).0, 200);

    // bob: acme's volumes only, and nothing of the cluster.
    let call = |s: &Session, rpc: &str, body: &str| {
        c.as_session(s, "POST", &format!("/api/hyperstor.v1.{rpc}"), body)
    };
    let (code, _, b) = call(&bob, "ClusterService/ListVolumes", r#"{"pool":1}"#);
    assert_eq!(code, 200, "{b}");
    assert!(b.contains("acme-vol") && !b.contains("other-vol"), "{b}");
    for (rpc, body) in [
        ("ClusterService/GetStatus", "{}"),
        ("ClusterService/CreatePool", r#"{"name":"x"}"#),
        ("RealmService/SetHostRealms", r#"{"host":"h"}"#),
        ("OperationService/CancelOperation", r#"{"id":"x"}"#),
    ] {
        let (code, _, b) = call(&bob, rpc, body);
        assert_eq!(code, 403, "{rpc}: {b}");
    }
    assert_eq!(c.as_session(&bob, "GET", "/api/status", "").0, 403);
    let create = |tenant: u32, name: &str| {
        call(
            &bob,
            "ClusterService/CreateVolume",
            &format!(r#"{{"pool":1,"size":"1048576","name":"{name}","tenant":{tenant}}}"#),
        )
    };
    let (code, _, b) = create(7, "by-bob");
    assert_eq!(code, 200, "{b}");
    let by_bob = json(&b)["volume"]["id"].as_str().unwrap().to_string();
    let (code, _, b) = create(8, "not-bob");
    assert!(code == 403 && b.contains("outside the realms"), "{b}");
    // No tenant would be default's: in realms, a creation names one (D286).
    let (code, _, b) = create(0, "not-bob");
    assert!(code == 400 && b.contains("name the volume's tenant"), "{b}");
    let (code, _, b) = call(
        &bob,
        "S3Service/SetS3Placement",
        r#"{"placement":{"name":"p"}}"#,
    );
    assert!(
        code == 400 && b.contains("name the placement's realm"),
        "{b}"
    );

    // Operations on acme's volumes are bob's to see, whoever started them,
    // with their realm; others' answer as ones that don't exist (D283).
    let flatten = |id: &str| {
        let (code, _, b) = call(
            &ann,
            "ClusterService/FlattenVolume",
            &format!(r#"{{"id":"{id}"}}"#),
        );
        assert_eq!(code, 200, "{b}");
        json(&b)["operation"]["id"].as_str().unwrap().to_string()
    };
    let (acme_op, other_op) = (flatten(&by_bob), flatten(&other_vol));
    for op in [&acme_op, &other_op] {
        wait_for("the flatten done", 30, || {
            let (_, _, b) = call(
                &ann,
                "OperationService/GetOperation",
                &format!(r#"{{"id":"{op}"}}"#),
            );
            b.contains("OPERATION_STATE_DONE").then_some(())
        });
    }
    let (code, _, b) = call(&bob, "OperationService/ListOperations", "{}");
    assert_eq!(code, 200, "{b}");
    assert!(b.contains(&acme_op) && !b.contains(&other_op), "{b}");
    assert!(
        b.contains("\"realm\":\"acme\"") && b.contains("\"tenant\":7"),
        "{b}"
    );
    let get = |id: &str| {
        call(
            &bob,
            "OperationService/GetOperation",
            &format!(r#"{{"id":"{id}"}}"#),
        )
    };
    assert_eq!(get(&acme_op).0, 200);
    assert_eq!(get(&other_op).0, 404);
    let (_, _, b) = call(&ann, "OperationService/ListOperations", "{}");
    assert!(b.contains(&acme_op) && b.contains(&other_op), "{b}");
    // Another realm's volume answers as one that doesn't exist.
    let expand = |id: &str| {
        call(
            &bob,
            "ClusterService/ExpandVolume",
            &format!(r#"{{"id":"{id}","size":"2097152"}}"#),
        )
    };
    let (code, _, b) = expand(&other_vol);
    assert_eq!(code, 404, "{b}");
    let (code, _, b) = expand(&acme_vol);
    assert_eq!(code, 200, "{b}");
    let (code, _, b) = call(
        &bob,
        "ClusterService/CreateSnapshot",
        &format!(r#"{{"id":"{other_vol}","name":"s"}}"#),
    );
    assert_eq!(code, 404, "{b}");
    let (code, _, b) = call(
        &bob,
        "ClusterService/DeleteVolume",
        &format!(r#"{{"id":"{acme_vol}"}}"#),
    );
    assert_eq!(code, 200, "{b}");
    c.ctl(&["delete-volume", "--id", &other_vol]).ok();
    let (code, _, b) = call(&bob, "TrashService/ListTrash", "{}");
    assert_eq!(code, 200, "{b}");
    assert!(b.contains(&acme_vol) && !b.contains(&other_vol), "{b}");
    let (code, _, b) = call(&bob, "RealmService/ListRealms", "{}");
    assert_eq!(code, 200, "{b}");
    assert!(b.contains("\"acme\"") && !b.contains("\"default\""), "{b}");
    assert!(b.contains("\"open_pool_ids\":[1,"), "{b}");
    // A browser's own realms header doesn't pass mgmt.
    let (code, _, b) = c.web(
        "POST",
        "/api/hyperstor.v1.ClusterService/ListVolumes",
        &c.dashboard_host(),
        &[
            ("Cookie", &bob.cookie),
            ("X-Hyperstor-CSRF", &bob.csrf),
            ("Content-Type", "application/json"),
            ("X-Hyperstor-Realms", "default"),
        ],
        r#"{"pool":1}"#,
    );
    assert!(code == 200 && !b.contains("other-vol"), "{b}");
    // The map service audits whom for, and mgmt in which realms.
    let line = c
        .audit()
        .into_iter()
        .find(|l| l.contains("\"op\":\"CreateVolume\"") && l.contains("by-bob"));
    assert!(
        line.as_deref().is_some_and(|l| l.contains("for user:bob")),
        "{line:?}"
    );
    assert!(
        c.mgmt_log().lines().any(|l| l.contains("user:bob")
            && l.contains("ClusterService.CreateVolume")
            && l.contains("realms acme")),
        "{}",
        c.mgmt_log()
    );

    // cy: acme's keys and acme's grants.
    let (code, _, b) = call(&cy, "S3Service/ListS3Keys", "{}");
    assert_eq!(code, 200, "{b}");
    assert_eq!(b.matches("\"tenant\":7").count(), 1, "{b}");
    assert!(!b.contains("\"tenant\":8"), "{b}");
    let (code, _, b) = call(&cy, "S3Service/CreateS3Key", r#"{"tenant":8}"#);
    assert_eq!(code, 403, "{b}");
    let (code, _, b) = call(&cy, "S3Service/CreateS3Key", r#"{"tenant":7}"#);
    assert_eq!(code, 200, "{b}");
    assert_eq!(
        call(&cy, "ClusterService/ListVolumes", r#"{"pool":1}"#).0,
        403
    );
    let (code, _, u) = c.as_session(&cy, "GET", "/auth/users", "");
    assert_eq!(code, 200, "{u}");
    let users = json(&u);
    let roles_of = |name: &str| {
        users["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["name"] == name)
            .map(|u| u["roles"].clone())
            .unwrap()
    };
    assert_eq!(roles_of("ann"), serde_json::json!([]), "{u}");
    assert_eq!(
        roles_of("bob"),
        serde_json::json!(["storage-admin@acme"]),
        "{u}"
    );
    let (code, _, b) = c.as_session(
        &cy,
        "PUT",
        "/auth/users/ann/roles",
        r#"{"roles":["viewer"]}"#,
    );
    assert!(code == 403 && b.contains("outside_realm"), "{b}");
    assert_eq!(
        c.as_session(
            &cy,
            "PUT",
            "/auth/users/ann/roles",
            r#"{"roles":["viewer@default"]}"#
        )
        .0,
        403
    );
    // cy's grants in acme replace bob's there; ann's stay.
    let (code, _, b) = c.as_session(
        &cy,
        "PUT",
        "/auth/users/ann/roles",
        r#"{"roles":["viewer@acme"]}"#,
    );
    assert_eq!(code, 200, "{b}");
    let (_, _, u) = c.as_session(&ann, "GET", "/auth/users", "");
    let all = json(&u);
    let ann_roles = all["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["name"] == "ann")
        .map(|u| u["roles"].clone())
        .unwrap();
    assert_eq!(
        ann_roles,
        serde_json::json!(["security-admin", "storage-admin", "viewer@acme"])
    );
    assert_eq!(c.as_session(&cy, "DELETE", "/auth/users/bob", "").0, 403);

    // hyperstorctl through mgmt, with bob's token, and ann's narrowed to
    // a realm with --in-realm.
    let token_of = |s: &Session| {
        let (code, _, b) = c.as_session(s, "POST", "/auth/tokens", r#"{"name":"t","days":1}"#);
        assert_eq!(code, 200, "{b}");
        json(&b)["token"].as_str().unwrap().to_string()
    };
    let ctl = |args: &[&str], token: &str| {
        let o = std::process::Command::new(c.bin.join("hyperstorctl"))
            .args(["--mgmt", &c.mgmt_url()])
            .arg("--mgmt-ca")
            .arg(c.pki().join("root.crt"))
            .args(args)
            .env("HYPERSTOR_TOKEN", token)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        (o.status.code().unwrap_or(-1), text)
    };
    let (bob_t, ann_t) = (token_of(&bob), token_of(&ann));
    // With no --pool, the first pool bob's realm may use.
    let (code, out) = ctl(
        &[
            "create-volume",
            "--size",
            "1M",
            "--name",
            "ctl-bob",
            "--tenant",
            "7",
        ],
        &bob_t,
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = ctl(&["volumes"], &bob_t);
    assert!(
        code == 0
            && out.contains("by-bob")
            && out.contains("ctl-bob")
            && !out.contains("other-vol"),
        "{out}"
    );
    let (code, out) = ctl(&["status"], &bob_t);
    assert_eq!(code, 4, "{out}");
    let (code, out) = ctl(&["--in-realm", "default", "volumes", "--pool", "1"], &bob_t);
    assert_eq!(code, 4, "{out}");
    let (code, out) = ctl(&["volumes", "--pool", "1"], &ann_t);
    assert!(code == 0 && out.contains("by-bob"), "{out}");
    // Each row names its realm (D286).
    let row = |out: &str, what: &str| {
        out.lines()
            .find(|l| l.contains(what))
            .unwrap_or_default()
            .to_string()
    };
    assert!(row(&out, "by-bob").contains("acme"), "{out}");
    let (code, out) = ctl(&["--in-realm", "acme", "op", "list"], &ann_t);
    assert!(
        code == 0 && row(&out, &acme_op).contains("acme") && !out.contains(&other_op),
        "{out}"
    );
    let (code, out) = ctl(&["--in-realm", "default", "volumes", "--pool", "1"], &ann_t);
    assert!(code == 0 && !out.contains("by-bob"), "{out}");
    let (code, out) = ctl(&["--in-realm", "acme", "status"], &ann_t);
    assert_eq!(code, 4, "{out}");
    let (code, out) = ctl(&["--in-realm", "Acme", "status"], &ann_t);
    assert_eq!(code, 2, "{out}");
}

/// Beyond loopback, mgmt needs logins and HTTPS.
#[test]
fn dashboard_refuses_to_listen_beyond_loopback() {
    let c = Cluster::all_in_one("dashaddr", 1);
    let out = std::process::Command::new(c.bin.join("hyperstor-mgmt"))
        .args([
            "--listen",
            "0.0.0.0:1",
            "--api",
            &c.api,
            "--tls",
            c.pki().to_str().unwrap(),
            "--authd",
            "/nonexistent/auth.sock",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
}
