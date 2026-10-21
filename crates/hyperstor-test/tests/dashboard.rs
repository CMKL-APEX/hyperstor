//! The read-only dashboard (`hyperstor-mgmt`, administration.md §23.7,
//! interim before AD2): the page, its data, and its protections.

use hyperstor_test::*;

#[test]
fn dashboard_serves_the_cluster_read_only() {
    let mut c = Cluster::all_in_one("dash", 3);
    let host = c.dashboard_host();

    let (code, head, body) = c.dashboard("GET", "/", &host);
    assert_eq!(code, 200, "{head}");
    assert!(body.contains("<title>Hyperstor</title>"), "{body}");
    assert!(
        head.contains("script-src 'self'") && head.contains("frame-ancestors 'none'"),
        "{head}"
    );
    assert!(
        head.contains("X-Content-Type-Options: nosniff")
            || head.contains("x-content-type-options: nosniff"),
        "{head}"
    );
    let (code, _, js) = c.dashboard("GET", "/app.js", &host);
    assert_eq!(code, 200);
    assert!(js.contains("/api/status"));

    // Its data: the cluster API, read with the dashboard's own identity.
    let (code, _, status) = c.dashboard("GET", "/api/status", &host);
    assert_eq!(code, 200, "{status}");
    assert!(status.contains("\"health\":\"HEALTH_OK\""), "{status}");
    assert!(
        status.contains(&format!("\"cluster_id\":\"{}\"", c.cluster_id)),
        "{status}"
    );
    let (code, _, pgs) = c.dashboard("GET", "/api/pgs?pool=1", &host);
    assert_eq!(code, 200, "{pgs}");
    assert_eq!(pgs.matches("\"acting\"").count(), 64, "{pgs}");
    assert_eq!(c.dashboard("GET", "/api/pgs", &host).0, 400);
    // And every node's metrics.
    c.ctl(&["io-test", "--mib", "4"]).ok();
    let (code, _, m) = c.dashboard("GET", "/api/metrics", &host);
    assert_eq!(code, 200, "{m}");
    assert_eq!(m.matches("\"up\":true").count(), 3, "{m}");
    assert!(m.contains("[\"hyperstor_requests_total\",{"), "{m}");

    // Read-only, and only for this machine's names (DNS rebinding).
    assert_eq!(c.dashboard("POST", "/api/status", &host).0, 405);
    assert_eq!(c.dashboard("GET", "/", "evil.example").0, 421);
    assert_eq!(c.dashboard("GET", "/", "evil.example:80").0, 421);
    assert_eq!(c.dashboard("GET", "/nope", &host).0, 404);

    // A node gone: its metrics say so, and status reports it.
    c.kill_node(2);
    let m = wait_for("node 2 without metrics", 10, || {
        let (_, _, m) = c.dashboard("GET", "/api/metrics", &host);
        (m.matches("\"up\":false").count() == 1).then_some(m)
    });
    assert_eq!(m.matches("\"up\":true").count(), 2, "{m}");
    wait_for("node 2 down in the status", 20, || {
        let (_, _, s) = c.dashboard("GET", "/api/status", &host);
        s.contains("node(s) down: 2").then_some(())
    });
}

/// Without login (AD2), only loopback addresses are allowed.
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
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
}
