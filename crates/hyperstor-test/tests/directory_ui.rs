//! A tenant's storage in glidex's UI, and glidex's datastore driver
//! (19-administration.md §23.12, D310, D311; I395–I400): mgmt answers
//! pages at a trusted directory's origin, or at an origin named on a
//! management client's binding, with a person's directory access token,
//! never with a cookie; a management client bound to realms acts only in
//! them, and no management client acts for people or reads mgmt's rows;
//! and a browser holds several accounts, one per directory.

use glidex_trust::fake::FakeDirectory;
use glidex_trust::feed::Entity;
use hyperstor_pki::Role;
use hyperstor_test::*;

fn json(b: &str) -> serde_json::Value {
    serde_json::from_str(b).unwrap_or_else(|e| panic!("{e}: {b}"))
}

/// A header's values in a response head.
fn header<'a>(head: &'a str, name: &str) -> Vec<&'a str> {
    head.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(": ")?;
            k.eq_ignore_ascii_case(name).then_some(v)
        })
        .collect()
}

/// The `name=value` of cookie `name` among a head's `Set-Cookie`s (empty
/// values too: a cleared cookie).
fn cookie(head: &str, name: &str) -> Option<String> {
    header(head, "set-cookie")
        .into_iter()
        .map(|c| c.split(';').next().unwrap().to_string())
        .find(|c| c.starts_with(&format!("{name}=")))
}

/// `curl` with a client certificate's `dir` (`admin.crt`, `admin.key`, as
/// [`Cluster::identity`] writes them) to the cluster API: the body.
fn rpc(c: &Cluster, dir: &std::path::Path, call: &str, headers: &[&str], body: &str) -> String {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "--cacert"])
        .arg(c.pki().join("root.crt"))
        .arg("--cert")
        .arg(dir.join("admin.crt"))
        .arg("--key")
        .arg(dir.join("admin.key"))
        .args(["-H", "Content-Type: application/json"]);
    for h in headers {
        cmd.args(["-H", h]);
    }
    let o = cmd
        .args(["-d", body])
        .arg(format!("https://{}/hyperstor.v1.{call}", c.api))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// The id of tenant `name` and of its project `project`.
fn ids(c: &Cluster, name: &str, project: &str) -> (u64, u64) {
    let v = json(&c.ctl(&["tenant", "list", "--json"]).ok().out);
    let t = v["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no tenant {name}: {v}"));
    let p = t["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == project)
        .unwrap_or_else(|| panic!("no project {project}: {t}"));
    (t["id"].as_u64().unwrap(), p["id"].as_u64().unwrap())
}

/// D311, I396, I397: glidex's driver, a `mgmt` identity enrolled by a
/// token, acts in the cluster as a whole until bound to realms, then only
/// in their tenants' projects, reading the cluster's status still; no
/// management client reads mgmt's rows or acts for a person, and no token
/// takes hyperstor-mgmt's own name.
#[test]
fn a_bound_client_acts_only_in_its_realms() {
    let c = Cluster::all_in_one("dsclient", 1);
    let gx = c.identity(Role::Mgmt, "glidex", false);
    for r in ["acme", "blue"] {
        c.ctl(&["realm", "create", "--name", r]).ok();
    }
    for (t, r) in [("acme", "acme"), ("blue", "blue")] {
        c.ctl(&["tenant", "create", "--name", t]).ok();
        let (id, _) = ids(&c, t, "default");
        c.ctl(&["realm", "tenant", "--tenant", &id.to_string(), "--realm", r])
            .ok();
        c.ctl(&[
            "project",
            "create",
            "--tenant",
            &id.to_string(),
            "--name",
            "web",
        ])
        .ok();
    }
    let (_, acme_web) = ids(&c, "acme", "web");
    let (_, blue_web) = ids(&c, "blue", "web");
    let mk = |name: &str, project: u64| {
        c.ctl_via(
            Via::Tls(&gx),
            &[
                "create-volume",
                "--pool",
                "1",
                "--size",
                "1M",
                "--name",
                name,
                "--project",
                &project.to_string(),
            ],
        )
    };

    // Unbound: the whole cluster, as before.
    mk("a1", acme_web).ok();
    mk("b1", blue_web).ok();
    c.ctl_via(
        Via::Tls(&gx),
        &[
            "create-volume",
            "--pool",
            "1",
            "--size",
            "1M",
            "--name",
            "none",
        ],
    )
    .ok();

    // Hyperstor-mgmt's own name can't be a client's, nor a token's.
    let r = c.ctl(&[
        "realm",
        "client",
        "--client",
        "dashboard",
        "--realm",
        "acme",
    ]);
    assert_eq!(r.code, 1, "{}", r.out);
    let r = c.ctl(&["token", "create", "--role", "mgmt", "--name", "dashboard"]);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.has("hyperstor-mgmt's own"), "{}", r.out);
    // Bound to acme: only acme's projects.
    c.ctl(&["realm", "client", "--client", "glidex", "--realm", "acme"])
        .ok();
    let listed = c.ctl(&["realm", "list"]).ok();
    assert!(listed.has("Client glidex acts in: acme"), "{}", listed.out);
    mk("a2", acme_web).ok();
    let r = mk("b2", blue_web);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.has("is outside the realms"), "{}", r.out);
    // A volume naming no tenant or project would land in "default".
    let r = c.ctl_via(
        Via::Tls(&gx),
        &[
            "create-volume",
            "--pool",
            "1",
            "--size",
            "1M",
            "--name",
            "x",
        ],
    );
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.has("name the volume's tenant or project"), "{}", r.out);
    // Its list holds acme's volumes alone.
    let vols = c.ctl_via(Via::Tls(&gx), &["volumes", "--pool", "1"]).ok();
    let words: Vec<&str> = vols.out.split_whitespace().collect();
    assert!(
        words.contains(&"a1") && words.contains(&"a2"),
        "{}",
        vols.out
    );
    assert!(
        !words.contains(&"b1") && !words.contains(&"none"),
        "{}",
        vols.out
    );
    // Every pool's at once (pool 0, I401): still acme's alone.
    let all = rpc(&c, &gx, "ClusterService/ListVolumes", &[], r#"{"pool":0}"#);
    assert!(all.contains("\"a1\"") && all.contains("\"a2\""), "{all}");
    assert!(
        !all.contains("\"b1\"") && !all.contains("\"none\""),
        "{all}"
    );
    // The cluster's status, for its pools' space: still read.
    c.ctl_via(Via::Tls(&gx), &["status"]).ok();
    // The cluster's own changes: refused.
    let r = c.ctl_via(
        Via::Tls(&gx),
        &["pool", "create", "--name", "p2", "--classes", "file"],
    );
    assert_ne!(r.code, 0, "{}", r.out);
    assert!(r.has("acts only in realms acme"), "{}", r.out);
    // Another realm's tenant's: as if it didn't exist.
    let t = c.ctl_via(Via::Tls(&gx), &["tenant", "list"]).ok();
    assert!(t.has("acme") && !t.has("blue"), "{}", t.out);

    // No management client reads mgmt's rows or acts for a person, bound
    // or not: those are hyperstor-mgmt's alone.
    let other = c.identity(Role::Mgmt, "script", false);
    for dir in [&gx, &other] {
        let r = rpc(
            &c,
            dir,
            "MgmtStoreService/GetRow",
            &[],
            r#"{"key":"user/1"}"#,
        );
        assert!(r.contains("permission_denied"), "{r}");
        let r = rpc(
            &c,
            dir,
            "ClusterService/CreateVolume",
            &["X-Hyperstor-On-Behalf-Of: user:ann"],
            r#"{"pool":1,"size":"1048576","name":"for-ann"}"#,
        );
        assert!(r.contains("permission_denied"), "{r}");
        let r = rpc(&c, dir, "TrustService/GetLoginState", &[], "{}");
        assert!(r.contains("permission_denied"), "{r}");
    }
    // Unbound again: the whole cluster.
    c.ctl(&["realm", "client", "--client", "glidex", "--realm", "any"])
        .ok();
    mk("b3", blue_web).ok();
    // A realm a client is bound to isn't deleted.
    c.ctl(&["realm", "client", "--client", "glidex", "--realm", "blue"])
        .ok();
    let r = c.ctl(&["realm", "delete", "--name", "blue"]);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.has("management client"), "{}", r.out);
    assert!(
        c.audit()
            .iter()
            .any(|l| l.contains("\"op\":\"SetClientRealms\""))
    );
}

/// D310, D314, I399: glidex's server relays its UI's reads to mgmt with a
/// person's directory access token, server to server (no `Origin`, no
/// cookie), and reads what their links reach; a browser's request from
/// another origin gets no CORS headers, and its writes are refused. The
/// driver names its disks' projects by glidex's ids. A browser holds a
/// local account and a directory's, one per directory, and switches
/// between them.
#[test]
fn glidex_s_server_reads_a_tenant_s_storage_for_a_person() {
    // SAFETY: before the cluster starts a thread; feeds pulled often.
    unsafe {
        std::env::set_var("HS_TRUST_POLL_MS", "300");
        std::env::set_var("HS_TRUST_REPORT_MS", "1000");
    }
    let c = Cluster::all_in_one("dirui", 1);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = rt.block_on(FakeDirectory::start("acme")).unwrap();
    let issuer = dir.issuer().to_string();
    let acme = dir.add_tenant("t1", "acme");
    dir.add_project("p1", "t1", "web");
    dir.add_user("u1", "Ann", false);
    dir.add_member("t1", "u1");
    dir.link(
        Entity::User("u1".into()),
        Entity::Tenant("t1".into()),
        "role.tenant-admin",
    );
    let url = c.mgmt_url();
    let admin = c.login("ann", Some(&c.claim_code())).unwrap();
    let offer = c
        .ctl(&[
            "admission",
            "offer",
            "--tenant",
            "acme",
            "--directory",
            &issuer,
            "--url",
            &url,
            "--ui",
            &url,
        ])
        .ok();
    let token = offer
        .out
        .split_whitespace()
        .find(|w| w.starts_with("gxo_"))
        .unwrap()
        .to_string();
    rt.block_on(dir.redeem(&token, &acme)).unwrap();
    wait_for("the feed's project", 30, || {
        c.ctl(&["project", "list"]).ok().has("web").then_some(())
    });
    let (_, web) = ids(&c, "acme", "web");
    // Volumes in acme's project, and one of no tenant's.
    c.ctl(&[
        "create-volume",
        "--pool",
        "1",
        "--size",
        "1M",
        "--name",
        "acme-disk",
        "--project",
        &web.to_string(),
    ])
    .ok();
    c.ctl(&[
        "create-volume",
        "--pool",
        "1",
        "--size",
        "1M",
        "--name",
        "theirs",
    ])
    .ok();
    // A second pool, with one of acme's and one of no tenant's.
    c.ctl(&[
        "pool",
        "create",
        "--name",
        "second",
        "--classes",
        "file",
        "--copies",
        "1",
        "--min",
        "1",
    ])
    .ok();
    let second = wait_for("the second pool in the map", 30, || {
        let v = json(&c.ctl(&["status", "--json"]).ok().out);
        v["pools"]
            .as_array()?
            .iter()
            .find(|p| p["name"] == "second")
            .and_then(|p| p["id"].as_u64())
    })
    .to_string();
    for (name, project) in [("acme-disk2", Some(web)), ("theirs2", None)] {
        let mut args = vec![
            "create-volume",
            "--pool",
            &second,
            "--size",
            "1M",
            "--name",
            name,
        ];
        let p = project.map(|p| p.to_string());
        if let Some(p) = &p {
            args.extend(["--project", p.as_str()]);
        }
        c.ctl(&args).ok();
    }

    let host = c.dashboard_host();
    let list = "/api/hyperstor.v1.ClusterService/ListVolumes";
    // As glidex's server relays them: a bearer token, no Origin, no cookie.
    let ann = dir.access_token("u1", &c.cluster_id, "s-ann").unwrap();
    let bearer = format!("Bearer {ann}");
    let relay = |auth: &str, path: &str, body: &str| {
        c.web(
            "POST",
            path,
            &host,
            &[
                ("Authorization", auth),
                ("Content-Type", "application/json"),
            ],
            body,
        )
    };
    let (code, head, b) = relay(&bearer, list, r#"{"pool":1}"#);
    assert_eq!(code, 200, "{b}");
    assert!(header(&head, "access-control-allow-origin").is_empty());
    assert!(b.contains("acme-disk") && !b.contains("theirs"), "{b}");
    // Pool 0, or none: every pool's she reaches, each naming its pool
    // (I401), so glidex's server needn't know the pools.
    for body in [r#"{"pool":0}"#, "{}"] {
        let (code, _, b) = relay(&bearer, list, body);
        assert_eq!(code, 200, "{b}");
        let v = json(&b);
        let mut got: Vec<(String, u64)> = v["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x["name"].as_str().unwrap().to_string(),
                    x["pool"].as_u64().unwrap(),
                )
            })
            .collect();
        got.sort();
        let second: u64 = second.parse().unwrap();
        assert_eq!(
            got,
            vec![
                ("acme-disk".to_string(), 1),
                ("acme-disk2".to_string(), second)
            ],
            "{b}"
        );
    }
    // Her tenant's buckets, keys and projects too (usage per project).
    for p in [
        "/api/hyperstor.v1.S3Service/ListS3Buckets",
        "/api/hyperstor.v1.S3Service/ListS3Keys",
        "/api/hyperstor.v1.TenantService/ListTenants",
    ] {
        let (code, _, b) = relay(&bearer, p, "{}");
        assert_eq!(code, 200, "{p}: {b}");
    }
    let (code, _, b) = c.web(
        "GET",
        "/auth/whoami",
        &host,
        &[("Authorization", &bearer)],
        "",
    );
    assert_eq!(code, 200, "{b}");
    assert_eq!(json(&b)["user"], "u1@acme");
    // Under her grants: nothing cluster-wide.
    let (code, _, b) = relay(&bearer, "/api/hyperstor.v1.ClusterService/GetStatus", "{}");
    assert_eq!(code, 403, "{b}");
    // A write by the token needs no CSRF value (no cookie, no session).
    let (code, _, b) = relay(
        &bearer,
        "/api/hyperstor.v1.ClusterService/CreateVolume",
        &serde_json::json!({"pool": 1, "size": "1048576", "name": "relayed", "project": web})
            .to_string(),
    );
    assert_eq!(code, 200, "{b}");
    // No token: refused.
    let (code, _, _) = c.web(
        "POST",
        list,
        &host,
        &[("Content-Type", "application/json")],
        r#"{"pool":1}"#,
    );
    assert_eq!(code, 401);

    // A browser at another origin (glidex's UI calling mgmt itself): no
    // CORS headers to read with, and its writes refused.
    let gx = format!("https://{}", issuer.split('/').nth(2).unwrap());
    let (code, head, _) = c.web(
        "OPTIONS",
        list,
        &host,
        &[
            ("Origin", &gx),
            ("Access-Control-Request-Method", "POST"),
            (
                "Access-Control-Request-Headers",
                "authorization, content-type",
            ),
        ],
        "",
    );
    assert_ne!(code, 204);
    assert!(header(&head, "access-control-allow-origin").is_empty());
    let (code, head, b) = c.web(
        "POST",
        list,
        &host,
        &[
            ("Origin", &gx),
            ("Authorization", &bearer),
            ("Content-Type", "application/json"),
        ],
        r#"{"pool":1}"#,
    );
    assert_eq!(code, 403, "{b}");
    assert!(b.contains("bad_origin"), "{b}");
    assert!(header(&head, "access-control-allow-origin").is_empty());
    let (_, head, _) = c.web(
        "GET",
        "/auth/whoami",
        &host,
        &[("Origin", &gx), ("Authorization", &bearer)],
        "",
    );
    assert!(header(&head, "access-control-allow-origin").is_empty());

    let realm = json(&c.ctl(&["tenant", "list", "--json"]).ok().out)["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "acme")
        .unwrap()["realm"]
        .as_str()
        .unwrap()
        .to_string();

    // Its driver, bound to the directory's realm and "default": an image
    // volume naming no project lands in "default" on purpose, and each
    // disk, a clone of its snapshot, in its project by glidex's id.
    c.ctl(&[
        "realm",
        "client",
        "--client",
        "glidex",
        "--realm",
        &format!("{realm},default"),
    ])
    .ok();
    let gx_id = c.identity(Role::Mgmt, "glidex", false);
    let drv = |args: &[&str]| c.ctl_via(Via::Tls(&gx_id), args);
    let image = json(
        &drv(&[
            "create-volume",
            "--pool",
            "1",
            "--size",
            "1M",
            "--name",
            "image",
            "--json",
        ])
        .ok()
        .out,
    )["volume"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    drv(&["snapshot", "create", "--id", &image, "--name", "golden"]).ok();
    let epoch = json(
        &drv(&["snapshot", "list", "--id", &image, "--json"])
            .ok()
            .out,
    )["snapshots"][0]["epoch"]
        .clone();
    let epoch = epoch
        .as_u64()
        .or_else(|| epoch.as_str().and_then(|s| s.parse().ok()))
        .unwrap()
        .to_string();
    let disk = drv(&[
        "clone-volume",
        "--id",
        &image,
        "--epoch",
        &epoch,
        "--name",
        "disk1",
        "--directory-project",
        "p1",
        "--json",
    ])
    .ok();
    assert_eq!(json(&disk.out)["volume"]["project"], web, "{}", disk.out);
    // A project no directory of its realms links: not found.
    let r = drv(&[
        "clone-volume",
        "--id",
        &image,
        "--epoch",
        &epoch,
        "--name",
        "disk2",
        "--directory-project",
        "p9",
    ]);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.has("no project linked"), "{}", r.out);
    let r = drv(&[
        "create-volume",
        "--pool",
        "1",
        "--size",
        "1M",
        "--name",
        "disk3",
        "--directory-project",
        "p1",
    ])
    .ok();
    assert!(r.has("disk3") || r.has("volume"), "{}", r.out);
    // Ann's reads hold the disks of her project, not the image.
    let (_, _, b) = relay(&bearer, list, r#"{"pool":1}"#);
    assert!(
        b.contains("disk1") && b.contains("disk3") && !b.contains("\"image\""),
        "{b}"
    );

    // Accounts in one browser: Ann's local account, put aside...
    let held = |cookies: &str| {
        let (code, head, b) = c.web("GET", "/auth/accounts", &host, &[("Cookie", cookies)], "");
        assert_eq!(code, 200, "{b}");
        (json(&b)["accounts"].as_array().unwrap().clone(), head)
    };
    let (code, head, b) = c.web(
        "POST",
        "/auth/accounts/add",
        &host,
        &[
            ("Cookie", &admin.cookie),
            ("X-Hyperstor-CSRF", &admin.csrf),
            ("Origin", &url),
            ("Content-Type", "application/json"),
        ],
        "{}",
    );
    assert_eq!(code, 200, "{b}");
    let list_cookie = cookie(&head, "hs_accounts").unwrap();
    assert!(list_cookie.len() > "hs_accounts=".len());
    let cleared = cookie(&head, c.session_cookie_name()).unwrap();
    assert_eq!(cleared, format!("{}=", c.session_cookie_name()));
    // ...then Bob signs in, locally too: one account per directory, so
    // Ann's is ended when the list is next read.
    let bob = c.login("bob", None).unwrap();
    let (accounts, head) = held(&format!("{}; {list_cookie}", bob.cookie));
    let users: Vec<_> = accounts.iter().map(|a| a["user"].clone()).collect();
    assert_eq!(accounts.len(), 1, "users: {users:?}");
    assert_eq!(accounts[0]["user"], "bob");
    assert_eq!(cookie(&head, "hs_accounts").unwrap(), "hs_accounts=");
    let (code, _, _) = c.web(
        "GET",
        "/auth/whoami",
        &host,
        &[("Cookie", &admin.cookie)],
        "",
    );
    assert_eq!(code, 401);
    // Bob's local account and Ann's at the directory: both held.
    let (code, head, _) = c.web(
        "POST",
        "/auth/accounts/add",
        &host,
        &[
            ("Cookie", &bob.cookie),
            ("X-Hyperstor-CSRF", &bob.csrf),
            ("Origin", &url),
        ],
        "{}",
    );
    assert_eq!(code, 200);
    let list_cookie = cookie(&head, "hs_accounts").unwrap();
    dir.login_as(Some("u1"), true);
    let start = c.web(
        "POST",
        "/auth/directory/start",
        &host,
        &[("Content-Type", "application/json")],
        &serde_json::json!({ "directory": issuer, "return_to": "/" }).to_string(),
    );
    let browser = cookie(&start.1, "hs_dir").unwrap();
    let back = dir
        .browse(json(&start.2)["redirect"].as_str().unwrap())
        .unwrap();
    let path = back.split_once("://").unwrap().1;
    let path = &path[path.find('/').unwrap()..];
    let (code, head, _) = c.web("GET", path, &host, &[("Cookie", &browser)], "");
    assert_eq!(code, 303);
    let ann_dir = cookie(&head, c.session_cookie_name()).unwrap();
    let both = format!("{ann_dir}; {list_cookie}");
    let (accounts, _) = held(&both);
    let names: Vec<(&str, bool)> = accounts
        .iter()
        .map(|a| (a["user"].as_str().unwrap(), a["active"].as_bool().unwrap()))
        .collect();
    assert_eq!(names, vec![("u1@acme", true), ("bob", false)]);
    assert_eq!(accounts[0]["directory"], issuer.as_str());
    // Switching to Bob: his session is the active one, Ann's held.
    let (_, _, b) = c.web("GET", "/auth/whoami", &host, &[("Cookie", &ann_dir)], "");
    let ann_csrf = json(&b)["csrf"].as_str().unwrap().to_string();
    let (code, head, b) = c.web(
        "POST",
        "/auth/accounts/switch",
        &host,
        &[
            ("Cookie", &both),
            ("X-Hyperstor-CSRF", &ann_csrf),
            ("Origin", &url),
            ("Content-Type", "application/json"),
        ],
        r#"{"user":"bob"}"#,
    );
    assert_eq!(code, 200, "{b}");
    let now = cookie(&head, c.session_cookie_name()).unwrap();
    assert_eq!(now, bob.cookie);
    let list_cookie = cookie(&head, "hs_accounts").unwrap();
    let (accounts, _) = held(&format!("{now}; {list_cookie}"));
    assert_eq!(accounts[0]["user"], "bob");
    assert_eq!(accounts[1]["user"], "u1@acme");
    // Without the page's Origin, or with the wrong CSRF value: refused.
    let (code, _, _) = c.web(
        "POST",
        "/auth/accounts/switch",
        &host,
        &[
            ("Cookie", &format!("{now}; {list_cookie}")),
            ("X-Hyperstor-CSRF", &bob.csrf),
        ],
        r#"{"user":"u1@acme"}"#,
    );
    assert_eq!(code, 403);
    // Removing Ann's ends her session.
    let (code, _, b) = c.web(
        "POST",
        "/auth/accounts/remove",
        &host,
        &[
            ("Cookie", &format!("{now}; {list_cookie}")),
            ("X-Hyperstor-CSRF", &bob.csrf),
            ("Origin", &url),
        ],
        r#"{"user":"u1@acme"}"#,
    );
    assert_eq!(code, 200, "{b}");
    let (code, _, _) = c.web("GET", "/auth/whoami", &host, &[("Cookie", &ann_dir)], "");
    assert_eq!(code, 401);
    // mgmt's audit reaches the cluster's log in a moment (§23.9.4).
    wait_for("the switch audited", 30, || {
        c.audit()
            .iter()
            .any(|l| l.contains("\"op\":\"SwitchAccount\""))
            .then_some(())
    });
}
