//! People from glidex directories (19-administration.md §23.12,
//! D307–D309), against glidex-trust's fake directory: a browser sign-in
//! through the directory, kept in the map service and checked against
//! its replica; step-up and logout there; roles from its links and, for
//! the operator tenant's members, from here; its access tokens and API
//! tokens as bearer tokens, through hyperstor-mgmt and hyperstorctl;
//! `hyperstorctl auth login`'s device grant and renewal; an ended login
//! chain; and a directory cut off past its max_staleness, while local
//! logins keep working.

use glidex_trust::fake::{FakeDirectory, Reply, Request};
use glidex_trust::feed::Entity;
use hyperstor_test::*;
use std::sync::Arc;

/// The `gxo_` token an `admission offer` printed.
fn token(r: &Run) -> String {
    r.out
        .split_whitespace()
        .find(|w| w.starts_with("gxo_"))
        .unwrap_or_else(|| panic!("{}", r.out))
        .to_string()
}

/// The path and query of URL `u`.
fn path_of(u: &str) -> String {
    let rest = u.split_once("://").map_or(u, |(_, r)| r);
    rest.find('/').map_or("/".into(), |i| rest[i..].to_string())
}

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

/// The `name=value` of the cookie `name` among a head's `Set-Cookie`s.
fn cookie(head: &str, name: &str) -> Option<String> {
    header(head, "set-cookie")
        .into_iter()
        .map(|c| c.split(';').next().unwrap().to_string())
        .find(|c| c.starts_with(&format!("{name}=")) && c.len() > name.len() + 1)
}

/// `GET /auth/whoami` with this bearer token: status and body.
fn whoami_bearer(c: &Cluster, t: &str) -> (u16, String) {
    let (code, _, b) = c.web(
        "GET",
        "/auth/whoami",
        &c.dashboard_host(),
        &[("Authorization", &format!("Bearer {t}"))],
        "",
    );
    (code, b)
}

/// A browser's sign-in through `dir` as whoever it logs in: the session
/// cookie, or the callback's status and body. `reauth` asks for a fresh
/// login.
fn sign_in(
    c: &Cluster,
    dir: &FakeDirectory,
    reauth: bool,
) -> Result<(String, String), (u16, String)> {
    let body = serde_json::json!({
        "directory": dir.issuer(), "return_to": "/volumes", "reauth": reauth,
    })
    .to_string();
    let (code, head, b) = c.web(
        "POST",
        "/auth/directory/start",
        &c.dashboard_host(),
        &[("Content-Type", "application/json")],
        &body,
    );
    assert_eq!(code, 200, "{b}");
    let go = json(&b)["redirect"].as_str().unwrap().to_string();
    assert!(go.starts_with(dir.issuer()), "{go}");
    if reauth {
        assert!(
            go.contains("prompt=login") && go.contains("max_age=0"),
            "{go}"
        );
    }
    let browser = cookie(&head, "hs_dir").expect("the browser's cookie");
    let back = dir.browse(&go).map_err(|e| (0, e))?;
    let path = path_of(&back);
    assert!(path.starts_with("/auth/directory/callback?"), "{path}");
    let (code, head, b) = c.web(
        "GET",
        &path,
        &c.dashboard_host(),
        &[("Cookie", &browser)],
        "",
    );
    if code != 303 {
        return Err((code, b));
    }
    assert_eq!(header(&head, "location"), vec!["/volumes"]);
    let session = cookie(&head, c.session_cookie_name()).expect("a session");
    // The same callback again: the login is spent.
    let (again, _, _) = c.web(
        "GET",
        &path,
        &c.dashboard_host(),
        &[("Cookie", &browser)],
        "",
    );
    assert_eq!(again, 403);
    let (code, _, b) = c.web(
        "GET",
        "/auth/whoami",
        &c.dashboard_host(),
        &[("Cookie", &session)],
        "",
    );
    assert_eq!(code, 200, "{b}");
    let csrf = json(&b)["csrf"].as_str().unwrap().to_string();
    Ok((session, csrf))
}

fn whoami(c: &Cluster, session: &str) -> (u16, serde_json::Value) {
    let (code, _, b) = c.web(
        "GET",
        "/auth/whoami",
        &c.dashboard_host(),
        &[("Cookie", session)],
        "",
    );
    (code, serde_json::from_str(&b).unwrap_or_default())
}

fn roles(v: &serde_json::Value) -> Vec<String> {
    v["roles"]
        .as_array()
        .map(|a| a.iter().map(|r| r.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

/// hyperstorctl through mgmt with `args` after `--mgmt` and its CA.
fn ctl_mgmt(c: &Cluster, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let o = std::process::Command::new(c.bin.join("hyperstorctl"))
        .args(["--mgmt", &c.mgmt_url()])
        .arg("--mgmt-ca")
        .arg(c.pki().join("root.crt"))
        .args(args)
        .env_remove("HYPERSTOR_TOKEN")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.code().unwrap_or(-1), text)
}

#[test]
fn people_sign_in_through_a_directory() {
    // Feeds pulled every 300 ms; a directory unheard for 15 s is cut off
    // (more than mgmt's 10 s cache, so a live one never looks stale).
    // SAFETY: the only test of this binary that runs by default, before
    // it starts a thread.
    unsafe {
        std::env::set_var("HS_TRUST_POLL_MS", "300");
        std::env::set_var("HS_TRUST_REPORT_MS", "1000");
        std::env::set_var("HS_TRUST_STALE_SECS", "15");
    }
    let c = Cluster::all_in_one("dirlogins", 3);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = Arc::new(rt.block_on(FakeDirectory::start("acme")).unwrap());
    let issuer = dir.issuer().to_string();
    let acme = dir.add_tenant("t1", "acme");
    dir.add_project("p1", "t1", "web");
    dir.add_user("u1", "Ann", false);
    dir.add_user("u2", "Bob", false);
    dir.add_user("u3", "Cy", true);
    dir.add_member("t1", "u1");
    dir.link(
        Entity::User("u1".into()),
        Entity::Tenant("t1".into()),
        "role.tenant-admin",
    );
    dir.link(
        Entity::User("u2".into()),
        Entity::Project("p1".into()),
        "role.viewer",
    );
    let url = c.mgmt_url();

    // A local security administrator, to give roles here.
    let admin = c.login("ann", Some(&c.claim_code())).unwrap();

    // The offer names where people's browsers reach mgmt: the redeem
    // registers its callback.
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
    let answer = rt.block_on(dir.redeem(&token(&offer), &acme)).unwrap();
    assert_eq!(
        answer.ui_callback.as_deref(),
        Some(format!("{url}/auth/directory/callback").as_str())
    );
    let dirs = c.ctl(&["admission", "directories"]).ok();
    assert!(dirs.has("logins sent back to"), "{}", dirs.out);
    wait_for("the feed's project", 30, || {
        let r = c.ctl(&["project", "list"]).ok();
        r.has("web").then_some(())
    });
    let acme_id = {
        let r = c.ctl(&["tenant", "list", "--json"]).ok();
        let v = json(&r.out);
        v["tenants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "acme")
            .unwrap()["id"]
            .as_u64()
            .unwrap()
    };

    // The login page lists the directory, and the cluster's id.
    let (code, _, b) = c.web("GET", "/auth/methods", &c.dashboard_host(), &[], "");
    assert_eq!(code, 200);
    let m = json(&b);
    assert_eq!(m["cluster_id"], c.cluster_id.as_str());
    assert_eq!(m["directories"][0]["issuer"], issuer.as_str());

    // Ann signs in through the directory, with a second factor: her
    // account here is her id there at the directory's realm, and her
    // roles come from her links.
    dir.login_as(Some("u1"), true);
    let (ann_s, ann_csrf) = sign_in(&c, &dir, false).unwrap();
    let (_, me) = whoami(&c, &ann_s);
    assert_eq!(me["user"], "u1@acme");
    assert_eq!(me["method"], "directory-mfa");
    assert_eq!(me["directory"], issuer.as_str());
    assert_eq!(
        roles(&me),
        vec!["security-admin@tenant:acme", "storage-admin@tenant:acme"]
    );
    // Nothing cluster-wide: the dashboard's status is refused.
    let (code, _, _) = c.web(
        "GET",
        "/api/status",
        &c.dashboard_host(),
        &[("Cookie", &ann_s)],
        "",
    );
    assert_eq!(code, 403);
    // Step-up asks the directory for a fresh login.
    sign_in(&c, &dir, true).unwrap();
    // Without the browser's cookie, a callback is refused.
    let (_, _, b) = c.web(
        "POST",
        "/auth/directory/start",
        &c.dashboard_host(),
        &[("Content-Type", "application/json")],
        &serde_json::json!({ "directory": issuer }).to_string(),
    );
    let back = dir.browse(json(&b)["redirect"].as_str().unwrap()).unwrap();
    let (code, _, _) = c.web("GET", &path_of(&back), &c.dashboard_host(), &[], "");
    assert_eq!(code, 403);
    // Cy is disabled at the directory: refused.
    dir.login_as(Some("u3"), false);
    assert_eq!(sign_in(&c, &dir, false).unwrap_err().0, 403);
    // The directory says no: refused.
    dir.login_as(None, false);
    assert_eq!(sign_in(&c, &dir, false).unwrap_err().0, 403);

    // Bob, by an access token addressed to this cluster: his project's
    // viewer. His account is made as he first comes.
    let bob_t = dir.access_token("u2", &c.cluster_id, "s-bob").unwrap();
    let (code, b) = whoami_bearer(&c, &bob_t);
    assert_eq!(code, 200, "{b}");
    assert_eq!(json(&b)["user"], "u2@acme");
    assert_eq!(roles(&json(&b)), vec!["viewer@project:acme/web"]);
    // Addressed to another cluster, or Cy's: refused.
    let other = dir.access_token("u2", "another-cluster", "s").unwrap();
    assert_eq!(whoami_bearer(&c, &other).0, 401);
    let cy = dir.access_token("u3", &c.cluster_id, "s").unwrap();
    assert_eq!(whoami_bearer(&c, &cy).0, 401);

    // Roles given here count only for the operator tenant's members, and
    // never in tenants or projects.
    c.ctl(&["cluster", "set-operator", "--tenant", &acme_id.to_string()])
        .ok();
    let set = |who: &str, r: &str| {
        c.as_session(
            &admin,
            "PUT",
            &format!("/auth/users/{who}/roles"),
            &format!(r#"{{"roles":[{r}]}}"#),
        )
    };
    let (code, _, b) = set("u2@acme", r#""viewer""#);
    assert_eq!(code, 403, "{b}");
    assert!(b.contains("not_operator"), "{b}");
    let (code, _, b) = set("u1@acme", r#""viewer@tenant:acme""#);
    assert_eq!(code, 403, "{b}");
    assert!(b.contains("from_directory"), "{b}");
    // Within mgmt's 10 s cache of who is the operator tenant's member.
    wait_for("Ann given roles as the operator's member", 30, || {
        let (code, _, b) = set("u1@acme", r#""security-admin","viewer""#);
        assert!(code == 200 || b.contains("not_operator"), "{b}");
        (code == 200).then_some(())
    });
    assert!(roles(&whoami(&c, &ann_s).1).contains(&"viewer".to_string()));

    // Through hyperstorctl: a directory's access token in a token file,
    // and a glidex API token in $HYPERSTOR_TOKEN.
    let file = c.scratch.path("token");
    let ann_t = dir.access_token("u1", &c.cluster_id, "s-ann").unwrap();
    std::fs::write(&file, &ann_t).unwrap();
    let f = file.to_str().unwrap();
    let (code, out) = ctl_mgmt(&c, &["--token-file", f, "status"], &[]);
    assert_eq!(code, 0, "{out}");
    let gxt = dir.add_api_token("u1", &c.cluster_id, glidex_trust::now() + 3600);
    let late = dir.add_api_token("u1", &c.cluster_id, glidex_trust::now() - 1);
    let elsewhere = dir.add_api_token("u1", "another-cluster", glidex_trust::now() + 3600);
    let (code, out) = wait_for("the API token in the feed", 30, || {
        let r = ctl_mgmt(&c, &["status"], &[("HYPERSTOR_TOKEN", &gxt)]);
        (r.0 == 0).then_some(r)
    });
    assert_eq!(code, 0, "{out}");
    assert_eq!(whoami_bearer(&c, &late).0, 401);
    assert_eq!(whoami_bearer(&c, &elsewhere).0, 401);
    assert_eq!(json(&whoami_bearer(&c, &gxt).1)["user"], "u1@acme");

    // `auth login`: the directory's device grant, as gxctl's, at a stand-in
    // for the directory's endpoints (the fake has none), minting the fake
    // directory's tokens; then renewed with its refresh token.
    let minting = dir.clone();
    let cluster = c.cluster_id.clone();
    let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let seen = polls.clone();
    type Answer = std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send>>;
    let handler: glidex_trust::fake::Handler = Arc::new(move |r: Request| -> Answer {
        let (minting, cluster, seen) = (minting.clone(), cluster.clone(), seen.clone());
        Box::pin(async move {
            let f = r.fields();
            let get = |k: &str| f.get(k).cloned().unwrap_or_default();
            match r.path.as_str() {
                "/d/acme/device_authorization" if get("client_id") == "gxctl" => Reply::json(
                    200,
                    serde_json::json!({
                        "device_code": "dc1", "user_code": "WXYZ-1234",
                        "verification_uri": "https://id.example/device",
                        "verification_uri_complete": "https://id.example/device?code=WXYZ-1234",
                        "expires_in": 60, "interval": 1,
                    }),
                ),
                "/d/acme/token" => {
                    if get("resource") != format!("urn:glidex:cluster:{cluster}") {
                        return Reply::json(400, serde_json::json!({ "error": "invalid_target" }));
                    }
                    let fresh = |refresh: &str| {
                        Reply::json(
                            200,
                            serde_json::json!({
                                "access_token": minting.access_token("u1", &cluster, "s-cli").unwrap(),
                                "refresh_token": refresh, "expires_in": 600, "token_type": "Bearer",
                            }),
                        )
                    };
                    match get("grant_type").as_str() {
                        "urn:ietf:params:oauth:grant-type:device_code"
                            if get("device_code") == "dc1" =>
                        {
                            // Approved at the second poll.
                            if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                                Reply::json(
                                    400,
                                    serde_json::json!({ "error": "authorization_pending" }),
                                )
                            } else {
                                fresh("r1")
                            }
                        }
                        "refresh_token" if get("refresh_token") == "r1" => fresh("r2"),
                        _ => Reply::json(400, serde_json::json!({ "error": "invalid_grant" })),
                    }
                }
                _ => Reply::status(404),
            }
        })
    });
    let stand_in = rt
        .block_on(glidex_trust::fake::serve_https(handler))
        .unwrap();
    let kept = c.scratch.path("login");
    let k = kept.to_str().unwrap();
    let at = format!("{}/d/acme", stand_in.base);
    let (code, out) = ctl_mgmt(
        &c,
        &[
            "--token-file",
            k,
            "auth",
            "login",
            "--directory",
            &at,
            "--directory-pin",
            &stand_in.pin,
        ],
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("WXYZ-1234"), "{out}");
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&kept).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let (code, out) = ctl_mgmt(&c, &["--token-file", k, "status"], &[]);
    assert_eq!(code, 0, "{out}");
    // Pinned to another certificate: refused.
    let (code, _) = ctl_mgmt(
        &c,
        &[
            "--token-file",
            k,
            "auth",
            "login",
            "--directory",
            &at,
            "--directory-pin",
            &"ab".repeat(32),
        ],
        &[],
    );
    assert_eq!(code, 1);
    // About to expire: renewed, and kept.
    let mut v = json(&std::fs::read_to_string(&kept).unwrap());
    v["expires_at"] = 0.into();
    std::fs::write(&kept, v.to_string()).unwrap();
    let (code, out) = ctl_mgmt(&c, &["--token-file", k, "status"], &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        json(&std::fs::read_to_string(&kept).unwrap())["refresh_token"],
        "r2"
    );

    // Out of the operator tenant at the directory: the roles given here
    // stop counting; her links' stay.
    dir.remove_row("tenant_members", "t1/user/u1");
    wait_for("the roles given here dropped", 40, || {
        let r = roles(&whoami(&c, &ann_s).1);
        (!r.contains(&"viewer".to_string()) && r.contains(&"storage-admin@tenant:acme".into()))
            .then_some(())
    });

    // A login chain the directory ended: its token is refused.
    let chain = dir.access_token("u2", &c.cluster_id, "s-ended").unwrap();
    assert_eq!(whoami_bearer(&c, &chain).0, 200);
    dir.end_chain("s-ended");
    wait_for("the ended chain refused", 40, || {
        (whoami_bearer(&c, &chain).0 == 401).then_some(())
    });
    // Disabled at the directory: Bob's sessions and tokens go.
    dir.add_user("u2", "Bob", true);
    wait_for("Bob refused", 40, || {
        (whoami_bearer(&c, &bob_t).0 == 401).then_some(())
    });

    // Logout ends the session here and tells the directory.
    let (code, _, _) = c.web(
        "POST",
        "/auth/logout",
        &c.dashboard_host(),
        &[("Cookie", &ann_s), ("X-Hyperstor-CSRF", &ann_csrf)],
        "",
    );
    assert_eq!(code, 200);
    assert_eq!(whoami(&c, &ann_s).0, 401);
    wait_for("the directory told of the logout", 30, || {
        dir.logouts()
            .iter()
            .any(|(cl, sid)| cl == &c.cluster_id && !sid.is_empty())
            .then_some(())
    });

    // Cut off: past max_staleness, the directory's people are refused;
    // local logins work. Back, they work again.
    dir.login_as(Some("u1"), false);
    let (ann_s, _) = sign_in(&c, &dir, false).unwrap();
    dir.set_down(true);
    wait_for("the directory's people refused", 60, || {
        let (code, b) = whoami_bearer(&c, &ann_t);
        (code == 503 && b.contains("directory_stale")).then_some(())
    });
    assert_eq!(whoami(&c, &ann_s).0, 503);
    let (code, _, _) = c.web(
        "GET",
        "/auth/whoami",
        &c.dashboard_host(),
        &[("Cookie", &admin.cookie)],
        "",
    );
    assert_eq!(code, 200, "a local login while the directory is down");
    assert!(c.login("ann", None).is_ok());
    dir.set_down(false);
    wait_for("the directory's people back", 60, || {
        (whoami_bearer(&c, &ann_t).0 == 200).then_some(())
    });
    assert_eq!(whoami(&c, &ann_s).0, 200);

    // Audited: the logins and their refusals, never a token.
    let log = c.mgmt_log();
    assert!(log.contains("ok, directory-mfa"), "{log}");
    assert!(log.contains("refused"), "{log}");
    assert!(!log.contains(&gxt[4..]), "a token in the log");
    c.ctl(&[
        "admission",
        "staleness",
        "--directory",
        &issuer,
        "--minutes",
        "30",
    ])
    .ok();
    assert!(
        c.ctl(&["admission", "directories"])
            .ok()
            .has("older than 30 min")
    );
    let audit = c.audit().join("\n");
    assert!(audit.contains("SetDirectoryStaleness"), "{audit}");
}

/// A real glidex directory's access token for an installed cluster, from
/// the environment only: `HS_REAL_MGMT_URL` (mgmt's https:// address),
/// `HS_REAL_DIRECTORY_TOKEN_FILE` (from `gxctl auth token --cluster ID`,
/// or a token file `hyperstorctl auth login` kept) and, if mgmt's
/// certificate isn't the host's CAs', `HS_REAL_MGMT_CA`. Run by hand on
/// a pilot host: `cargo test --release -p hyperstor-test --test
/// directory_logins -- --ignored`.
#[test]
#[ignore]
fn a_real_directory_s_people_reach_mgmt() {
    let (Ok(url), Ok(file)) = (
        std::env::var("HS_REAL_MGMT_URL"),
        std::env::var("HS_REAL_DIRECTORY_TOKEN_FILE"),
    ) else {
        panic!("set HS_REAL_MGMT_URL and HS_REAL_DIRECTORY_TOKEN_FILE");
    };
    let mut c = std::process::Command::new(bin_dir().join("hyperstorctl"));
    c.args(["--mgmt", &url, "--token-file", &file]);
    if let Ok(ca) = std::env::var("HS_REAL_MGMT_CA") {
        c.args(["--mgmt-ca", &ca]);
    }
    // Any answer but a refused login: the person may lack the role.
    let o = c.arg("tenant").arg("list").output().unwrap();
    let out = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(
        !out.contains("unauthenticated") && !out.contains("directory_stale"),
        "{out}"
    );
}
