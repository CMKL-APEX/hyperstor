//! `ui-e2e-cluster --ui DIR`: a cluster for the web UI's browser tests
//! (crates/hyperstor-mgmt/e2e).
//!
//! Installs an all-in-one cluster of three nodes from the built binaries
//! into a scratch root, as the end-to-end suite does, and runs
//! hyperstor-mgmt serving the UI in `DIR` (`bun run build`'s `dist`), with
//! the made-up PAM helper ([`hyperstor_test::start_authd`]) and a made-up
//! identity provider that logs any browser in as `dee@example.com`. It
//! realm `acme` holding tenant 7, and tenant `lab` with a volume in its
//! default project and one in its project `web`. It
//! prints one JSON line: `url`, the claim code, the people's password,
//! and the provider's person; then runs until its standard input closes,
//! and removes everything.

use hyperstor_test::*;
use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ui = match args.as_slice() {
        [flag, dir] if flag == "--ui" => std::path::PathBuf::from(dir),
        _ => {
            eprintln!("usage: ui-e2e-cluster --ui DIR");
            std::process::exit(2)
        }
    };
    let ui = ui.canonicalize().unwrap_or_else(|e| {
        eprintln!("ui-e2e-cluster: {}: {e}", ui.display());
        std::process::exit(1)
    });
    if !ui.join("index.html").is_file() {
        eprintln!(
            "ui-e2e-cluster: {}: no index.html (bun run build)",
            ui.display()
        );
        std::process::exit(1)
    }
    let mut c = Cluster::all_in_one("ui-e2e", 3);
    let idp = FakeIdp::start();
    idp.auto_login(serde_json::json!({
        "sub": "s-dee", "email": "dee@example.com", "email_verified": true,
    }));
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
    c.restart_mgmt_with(&[
        "--ui",
        ui.to_str().unwrap(),
        "--oidc",
        conf.to_str().unwrap(),
    ]);
    // Realm acme, with tenant 7, for roles granted on a realm (D262).
    c.ctl(&["realm", "create", "--name", "acme"]).ok();
    c.ctl(&["realm", "tenant", "--tenant", "7", "--realm", "acme"])
        .ok();
    // Tenant lab with its project web, and a volume in each of its
    // projects, for the tenant and project pickers (D310).
    let lab = c.ctl(&["tenant", "create", "--name", "lab", "--json"]).ok();
    let lab: serde_json::Value = serde_json::from_str(&lab.out).unwrap();
    let lab = lab["tenant"]["id"].as_u64().unwrap().to_string();
    let web = c
        .ctl(&[
            "project", "create", "--tenant", &lab, "--name", "web", "--json",
        ])
        .ok();
    let web: serde_json::Value = serde_json::from_str(&web.out).unwrap();
    let web = web["project"]["id"].as_u64().unwrap().to_string();
    for (name, flag, id) in [
        ("lab-default-vol", "--tenant", &lab),
        ("lab-web-vol", "--project", &web),
    ] {
        c.ctl(&[
            "create-volume",
            "--pool",
            "1",
            "--size",
            "4M",
            "--name",
            name,
            flag,
            id,
        ])
        .ok();
    }
    let claim = c.claim_code();
    println!(
        "{}",
        serde_json::json!({
            "url": c.mgmt_url(),
            "claim": claim,
            "password": PASSWORD,
            "people": ["ann", "bob", "cy"],
            "sso": "dee@example.com",
        })
    );
    // Until the tests are done.
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
    drop(c);
}
