//! The audit log (19-administration.md §23.9.4, D112, D113): the map
//! service's records and hyperstor-mgmt's (people's logins and roles) land
//! in `.meta`; `hyperstorctl audit list` reads them newest first, filtered
//! and paged; retention is set; through mgmt only security administrators
//! read it. A storm of refusals is summarised (D147), and the records
//! of a day's parts page through mgmt (D148).

use hyperstor_pki::Role;
use hyperstor_test::*;

#[test]
fn every_change_lands_in_the_audit_log() {
    let c = Cluster::all_in_one("audit", 3);
    c.ctl(&["create-volume", "--size", "64M", "--name", "a1"])
        .ok();
    c.ctl(&["create-volume", "--size", "64M", "--name", "a2"])
        .ok();
    c.ctl(&["delete-volume", "--id", "00000000000000000000000000000abc"])
        .expect(1);
    // People through mgmt: the claim, a role change.
    let ann = c.login("ann", Some(&c.claim_code())).unwrap();
    let roles = r#"{"roles":["security-admin","viewer"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/ann/roles", roles).0,
        200
    );

    // In .meta within a few seconds: the map service's and mgmt's.
    let made = wait_for("CreateVolume in the audit log", 20, || {
        let r = c.ctl(&["audit", "list", "--op", "CreateVolume"]).ok();
        (r.out.lines().count() == 2).then_some(r)
    });
    assert!(
        made.has("/admin/local") && made.has("ok, volume"),
        "{}",
        made.out
    );
    let mgmt = wait_for("mgmt's records", 20, || {
        let r = c.ctl(&["audit", "list", "--principal", "user:ann"]).ok();
        (r.has("Claim") && r.has("People.SetRoles")).then_some(r)
    });
    assert!(mgmt.has("ok, pam"), "{}", mgmt.out);
    // With a time like any other: kept by --since, not dated 1970.
    let recent = c
        .ctl(&["audit", "list", "--since", "1h", "--principal", "user:ann"])
        .ok();
    assert!(
        recent.has("Claim") && recent.has("People.SetRoles"),
        "{}",
        recent.out
    );
    assert!(!recent.has("1970-"), "{}", recent.out);
    // Refused calls too.
    let refused = c.ctl(&["audit", "list", "--op", "DeleteVolume"]).ok();
    assert!(refused.has("failed"), "{}", refused.out);
    // Newest first, and paged: the second create before the first.
    let json = c
        .ctl(&["audit", "list", "--op", "CreateVolume", "--json"])
        .ok();
    let v: serde_json::Value = serde_json::from_str(&json.out).unwrap();
    let recs = v["records"].as_array().unwrap();
    assert_eq!(recs.len(), 2, "{}", json.out);
    assert!(recs[0]["time_ms"].as_u64() >= recs[1]["time_ms"].as_u64());
    assert!(recs[0]["result"].as_str().unwrap().contains("volume"));
    let two = c.ctl(&["audit", "list", "--limit", "2"]).ok();
    assert_eq!(two.out.lines().count(), 2, "{}", two.out);
    assert!(
        c.ctl(&["audit", "list", "--since", "1h", "--op", "CreateVolume"])
            .ok()
            .has("CreateVolume")
    );

    // Retention: a year by default, then set.
    assert!(c.ctl(&["audit", "settings"]).ok().has("kept 365 days"));
    assert!(
        c.ctl(&["audit", "settings", "--retention-days", "30"])
            .ok()
            .has("kept 30 days")
    );
    assert!(
        c.ctl(&["audit", "settings", "--retention-days", "0"])
            .ok()
            .has("kept forever")
    );

    // Through mgmt: a security administrator reads it; a viewer doesn't.
    let list = "/api/hyperstor.v1.AuditService/ListAudit";
    let (code, _, b) = c.as_session(&ann, "POST", list, r#"{"op":"CreateVolume"}"#);
    assert_eq!(code, 200, "{b}");
    assert!(b.contains("CreateVolume"), "{b}");
    let bob = c.login("bob", None).unwrap();
    let viewer = r#"{"roles":["viewer"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/bob/roles", viewer).0,
        200
    );
    assert_eq!(c.as_session(&bob, "POST", list, "{}").0, 403);
    // And nobody but mgmt appends.
    let append = "/api/hyperstor.v1.AuditService/AppendAudit";
    assert_eq!(c.as_session(&ann, "POST", append, r#"{"lines":[]}"#).0, 403);
}

#[test]
fn a_refusal_storm_is_summarised_and_pages() {
    let c = Cluster::all_in_one("audit-storm", 3);
    let node = c.identity(Role::Node, "node7", false);
    for _ in 0..15 {
        c.ctl_via(Via::Tls(&node), &["out", "--device", "0"])
            .expect(4);
    }
    let who = format!("hyperstor://{}/node/node7", c.cluster_id);
    let mine = |l: &String| l.contains(&format!("\"principal\":\"{who}\""));
    // journald has every one.
    let refused = |l: &String| mine(l) && l.contains("\"result\":\"refused: ");
    assert_eq!(c.audit().iter().filter(|l| refused(l)).count(), 15);
    // .meta has the first ten, then, once the minute is over, how many
    // more there were.
    let summary = wait_for("the refusals' summary", 90, || {
        let r = c.ctl(&["audit", "list", "--op", "Refused"]).ok();
        r.has("refused 5 more").then_some(r)
    });
    assert!(
        summary.has("node7") && summary.has("permission_denied"),
        "{}",
        summary.out
    );
    let json = c
        .ctl(&["audit", "list", "--principal", "node/node7", "--json"])
        .ok();
    let v: serde_json::Value = serde_json::from_str(&json.out).unwrap();
    let recs = v["records"].as_array().unwrap();
    let kept = recs.iter().filter(|r| r["op"] != "Refused").count();
    assert_eq!(kept, 10, "{}", json.out);
    assert_eq!(recs.len(), 11, "{}", json.out);

    // Paged through mgmt, three at a time, with its opaque cursor: every
    // record once, newest first, as one listing has them.
    let ann = c.login("ann", Some(&c.claim_code())).unwrap();
    let roles = r#"{"roles":["security-admin"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/ann/roles", roles).0,
        200
    );
    let list = "/api/hyperstor.v1.AuditService/ListAudit";
    let page = |after: &str| {
        let body = serde_json::json!({"principal": "node/node7", "limit": 3, "after": after});
        let (code, _, b) = c.as_session(&ann, "POST", list, &body.to_string());
        assert_eq!(code, 200, "{b}");
        serde_json::from_str::<serde_json::Value>(&b).unwrap()
    };
    let mut paged = Vec::new();
    let mut after = String::new();
    loop {
        let p = page(&after);
        let rs = p["records"].as_array().cloned().unwrap_or_default();
        assert!(rs.len() <= 3);
        paged.extend(rs);
        match p["next"].as_str() {
            Some(n) if !n.is_empty() => after = n.to_string(),
            _ => break,
        }
    }
    // (uint64 is a string in protobuf's JSON, maybe not in hyperstorctl's.)
    let key = |r: &serde_json::Value| {
        let t = r["time_ms"]
            .as_str()
            .map_or(r["time_ms"].to_string(), String::from);
        (t, r["op"].to_string(), r["result"].to_string())
    };
    let paged: Vec<_> = paged.iter().map(key).collect();
    let listed: Vec<_> = recs.iter().map(key).collect();
    assert_eq!(paged, listed);
}
