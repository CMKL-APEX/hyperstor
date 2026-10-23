//! The audit log (administration.md §23.9.4, D112, D113): the map
//! service's records and hyperstor-mgmt's (people's logins and roles) land
//! in `.meta`; `hyperstorctl audit list` reads them newest first, filtered
//! and paged; retention is set; through mgmt only security administrators
//! read it.

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
