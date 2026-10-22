//! Logins to `hyperstor-mgmt` with an identity provider (OIDC,
//! administration.md §23.9.1, D96), against a made-up provider: the
//! first administrator's claim, accounts named by verified email, the
//! login bound to its browser and used once, and step-up.

use hyperstor_test::*;

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

/// The value of the cookie `name` that `head` sets.
fn set_cookie(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(": ")?;
        if !k.eq_ignore_ascii_case("set-cookie") {
            return None;
        }
        let first = v.split(';').next()?;
        first
            .strip_prefix(&format!("{name}="))
            .filter(|v| !v.is_empty())
            .map(String::from)
    })
}

/// A login begun with `start` (JSON): the provider's redirect URL and the
/// browser cookie.
fn begin(c: &Cluster, start: &str) -> (String, String) {
    let (code, head, body) = c.web(
        "POST",
        "/auth/oidc/start",
        &c.dashboard_host(),
        &[("Content-Type", "application/json")],
        start,
    );
    assert_eq!(code, 200, "{body}");
    let browser = set_cookie(&head, "hs_oidc").expect("the browser cookie");
    assert!(
        head.contains("SameSite=Lax") && head.contains("HttpOnly"),
        "{head}"
    );
    (
        json(&body)["redirect"].as_str().unwrap().to_string(),
        browser,
    )
}

/// The provider sends the browser back with `state`.
fn come_back(c: &Cluster, state: &str, browser: Option<&str>) -> (u16, String, String) {
    let cookie = browser.map(|b| format!("hs_oidc={b}"));
    let mut headers = Vec::new();
    if let Some(ck) = &cookie {
        headers.push(("Cookie", ck.as_str()));
    }
    c.web(
        "GET",
        &format!("/auth/oidc/callback?code=c1&state={state}"),
        &c.dashboard_host(),
        &headers,
        "",
    )
}

/// The session a callback's answer set, with its CSRF value.
fn session_of(c: &Cluster, head: &str) -> Session {
    let id = set_cookie(head, "hs_session").expect("a session cookie");
    let cookie = format!("hs_session={id}");
    let (code, _, w) = c.web(
        "GET",
        "/auth/whoami",
        &c.dashboard_host(),
        &[("Cookie", &cookie)],
        "",
    );
    assert_eq!(code, 200, "{w}");
    let w = json(&w);
    Session {
        cookie,
        set_cookie: String::new(),
        csrf: w["csrf"].as_str().unwrap().to_string(),
        roles: w["roles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect(),
    }
}

#[test]
fn people_log_in_with_an_identity_provider() {
    let mut c = Cluster::all_in_one("oidc", 1);
    let idp = FakeIdp::start();
    let secret = c.scratch.path("oidc-secret");
    std::fs::write(&secret, "test-only client secret\n").unwrap();
    let conf = c.scratch.path("oidc.json");
    std::fs::write(
        &conf,
        serde_json::json!({
            "issuer": idp.issuer, "client_id": "hyperstor", "client_secret_file": secret,
        })
        .to_string(),
    )
    .unwrap();
    c.restart_mgmt_with(&["--oidc", conf.to_str().unwrap()]);
    let host = c.dashboard_host();
    let (_, _, m) = c.dashboard("GET", "/auth/methods", &host);
    let m = json(&m);
    assert_eq!((&m["pam"], &m["oidc"]), (&true.into(), &true.into()), "{m}");

    // The first security administrator claims the cluster through the
    // provider: the code goes in the POST, never in a URL.
    let claim = c.claim_code();
    let (redirect, browser) = begin(
        &c,
        &format!(r#"{{"claim":"{claim}","return_to":"/volumes"}}"#),
    );
    assert!(
        redirect.starts_with(&format!("{}/authorize?", idp.issuer)),
        "{redirect}"
    );
    assert!(!redirect.contains(&claim), "no claim code in the URL");
    let state = query_param(&redirect, "state").unwrap();
    let nonce = query_param(&redirect, "nonce").unwrap();
    assert_eq!(
        query_param(&redirect, "redirect_uri").unwrap(),
        format!("{}/auth/oidc/callback", c.mgmt_url())
    );
    assert_eq!(
        query_param(&redirect, "code_challenge_method").unwrap(),
        "S256"
    );
    idp.next_id_token(
        "hyperstor",
        serde_json::json!({
            "sub": "s-ann", "email": "Ann@Example.com", "email_verified": true, "nonce": nonce,
        }),
    );
    let (code, head, body) = come_back(&c, &state, Some(&browser));
    assert_eq!(code, 303, "{head}\n{body}");
    assert!(
        head.contains("location: /volumes") || head.contains("Location: /volumes"),
        "{head}"
    );
    let ann = session_of(&c, &head);
    assert_eq!(ann.roles, ["security-admin"]);
    // The client authenticated itself and sent its PKCE verifier.
    let req = idp.token_requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        req.starts_with("authorization: Basic") || req.starts_with("Authorization: Basic"),
        "{req}"
    );
    assert!(req.contains("code_verifier="), "{req}");
    let (_, _, w) = c.as_session(&ann, "GET", "/auth/whoami", "");
    let w = json(&w);
    assert_eq!(
        (&w["user"], &w["method"]),
        (&"ann@example.com".into(), &"oidc".into())
    );

    // A login state is used once, and only by the browser that began it.
    idp.next_id_token(
        "hyperstor",
        serde_json::json!({ "sub": "s-ann", "email": "ann@example.com", "email_verified": true, "nonce": nonce }),
    );
    assert_eq!(come_back(&c, &state, Some(&browser)).0, 403, "replayed");
    let (redirect, _) = begin(&c, "{}");
    let state = query_param(&redirect, "state").unwrap();
    assert_eq!(come_back(&c, &state, Some("someone-else")).0, 403);
    // A wrong nonce, or an email the provider doesn't vouch for, gets no
    // session.
    for claims in [
        serde_json::json!({ "sub": "s-bob", "email": "bob@example.com", "email_verified": true, "nonce": "wrong" }),
        serde_json::json!({ "sub": "s-eve", "email": "eve@example.com", "email_verified": false }),
    ] {
        let (redirect, browser) = begin(&c, "{}");
        let state = query_param(&redirect, "state").unwrap();
        let mut claims = claims;
        if claims.get("nonce").is_none() {
            claims["nonce"] = query_param(&redirect, "nonce").unwrap().into();
        }
        idp.next_id_token("hyperstor", claims);
        let (code, head, body) = come_back(&c, &state, Some(&browser));
        assert_eq!(code, 403, "{body}");
        assert!(set_cookie(&head, "hs_session").is_none());
    }

    // Someone else: no roles until a security administrator gives some.
    let (redirect, browser) = begin(&c, "{}");
    idp.next_id_token(
        "hyperstor",
        serde_json::json!({
            "sub": "s-bob", "email": "bob@example.com", "email_verified": true,
            "nonce": query_param(&redirect, "nonce").unwrap(),
        }),
    );
    let (code, head, _) = come_back(
        &c,
        &query_param(&redirect, "state").unwrap(),
        Some(&browser),
    );
    assert_eq!(code, 303);
    let bob = session_of(&c, &head);
    assert!(bob.roles.is_empty());
    assert_eq!(c.as_session(&bob, "GET", "/api/status", "").0, 403);
    let roles = r#"{"roles":["viewer"]}"#;
    let path = "/auth/users/bob@example.com/roles";
    assert_eq!(c.as_session(&ann, "PUT", path, roles).0, 200);
    assert_eq!(c.as_session(&bob, "GET", "/api/status", "").0, 200);

    // A password login still works beside it.
    assert!(c.login("cy", None).is_ok());

    // What happened is audited, with how each person logged in.
    let log = c.mgmt_log();
    for want in [
        "\"Claim\"",
        "user:ann@example.com",
        "ok, oidc",
        "user:bob@example.com",
    ] {
        assert!(log.contains(want), "{want} not in:\n{log}");
    }
    assert_eq!(
        log.matches(&claim).count(),
        1,
        "the claim code only at start"
    );
}
