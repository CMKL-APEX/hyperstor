//! `ui-e2e-cluster --ui DIR`: a cluster for the web UI's browser tests
//! (crates/hyperstor-mgmt/e2e).
//!
//! Installs an all-in-one cluster of three nodes from the built binaries
//! into a scratch root, as the end-to-end suite does, and runs
//! hyperstor-mgmt serving the UI in `DIR` (`bun run build`'s `dist`), with
//! the made-up PAM helper ([`hyperstor_test::start_authd`]) and a made-up
//! identity provider that logs any browser in as `dee@example.com`. It
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
    let claim = c.claim_code();
    println!(
        "{}",
        serde_json::json!({
            "url": format!("http://{}", c.dashboard_host()),
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
