//! The management service (`hyperstor-mgmt`, administration.md §23.7,
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
            .args(["--mgmt", &format!("http://{host}")])
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
