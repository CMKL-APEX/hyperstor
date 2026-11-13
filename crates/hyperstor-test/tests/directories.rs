//! Trust in glidex directories (19-administration.md §23.12, D306),
//! against glidex-trust's fake directory: an offer redeemed once at
//! hyperstor-mgmt, the directory's realm and tenant made, the feed's
//! projects made, renamed, deleted (one holding a volume kept, unlinked)
//! and moved between its tenants, reports with storage counts, the
//! operator tenant, and the trust ended from either side.

use glidex_trust::fake::FakeDirectory;
use hyperstor_test::*;

/// The `gxo_` token an `admission offer` printed.
fn token(r: &Run) -> String {
    r.out
        .split_whitespace()
        .find(|w| w.starts_with("gxo_"))
        .unwrap_or_else(|| panic!("{}", r.out))
        .to_string()
}

/// The id of tenant `name`, from `tenant list --json`.
fn tenant_id(c: &Cluster, name: &str) -> Option<u32> {
    let r = c.ctl(&["tenant", "list", "--json"]).ok();
    let v: serde_json::Value = serde_json::from_str(&r.out).unwrap();
    v["tenants"]
        .as_array()?
        .iter()
        .find(|t| t["name"] == name)
        .map(|t| t["id"].as_u64().unwrap_or(0) as u32)
}

/// Project `name` of the tenant named `tenant`: its id and glidex id.
fn project(c: &Cluster, tenant: &str, name: &str) -> Option<(u32, String)> {
    let r = c.ctl(&["tenant", "list", "--json"]).ok();
    let v: serde_json::Value = serde_json::from_str(&r.out).unwrap();
    let t = v["tenants"]
        .as_array()?
        .iter()
        .find(|t| t["name"] == tenant)?;
    t["projects"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
        .map(|p| {
            (
                p["id"].as_u64().unwrap_or(0) as u32,
                p["glidex_id"].as_str().unwrap_or("").to_string(),
            )
        })
}

#[test]
fn a_directory_s_feed_makes_its_tenants_and_projects() {
    // Feeds pulled every 300 ms, not 30 s; reports every second.
    // SAFETY: the only test of this binary, before it starts a thread.
    unsafe {
        std::env::set_var("HS_TRUST_POLL_MS", "300");
        std::env::set_var("HS_TRUST_REPORT_MS", "1000");
    }
    let c = Cluster::all_in_one("directories", 3);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = rt.block_on(FakeDirectory::start("acme")).unwrap();
    let issuer = dir.issuer().to_string();
    let acme = dir.add_tenant("t1", "acme");
    let lab = dir.add_tenant("t2", "lab");
    dir.add_project("p1", "t1", "Web App");
    dir.add_project("p2", "t1", "api");
    let url = c.mgmt_url();

    // A local tenant keeps its name: an offer naming it is refused.
    c.ctl(&["tenant", "create", "--name", "local"]).ok();
    let r = c.ctl(&[
        "admission",
        "offer",
        "--tenant",
        "acme",
        "--directory",
        &issuer,
        "--url",
        &url,
        "--name",
        "local",
    ]);
    assert_eq!(r.code, 1, "{}", r.out);
    // Not an issuer, not https: usage or refusal.
    c.ctl(&[
        "admission",
        "offer",
        "--tenant",
        "acme",
        "--directory",
        "acme",
        "--url",
        &url,
    ])
    .expect(1);

    // The offer: its token once, the certificate read from mgmt.
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
        ])
        .ok();
    assert!(
        offer.has("serves a certificate with SHA-256"),
        "{}",
        offer.out
    );
    let t1 = token(&offer);
    let list = c.ctl(&["admission", "list"]).ok();
    assert!(list.has("open") && list.has("acme"), "{}", list.out);
    assert!(!list.out.contains(&t1[4..20]), "the secret is never listed");
    // Another tenant can't use it, and it redeems once.
    assert!(rt.block_on(dir.redeem(&t1, &lab)).is_err());
    let answer = rt.block_on(dir.redeem(&t1, &acme)).unwrap();
    assert_eq!(answer.cluster_id, c.cluster_id);
    assert!(
        rt.block_on(dir.redeem(&t1, &acme)).is_err(),
        "redeemed twice"
    );
    assert!(c.ctl(&["admission", "list"]).ok().has("redeemed"));

    // The directory's realm and tenant are made, linked.
    let dirs = c.ctl(&["admission", "directories"]).ok();
    assert!(
        dirs.has(&issuer) && dirs.has("realm acme") && dirs.has("acme (t1)"),
        "{}",
        dirs.out
    );
    let tenants = c.ctl(&["tenant", "list"]).ok();
    assert!(tenants.has(&format!("t1 at {issuer}")), "{}", tenants.out);
    let acme_id = tenant_id(&c, "acme").unwrap();

    // The feed: its projects made, named as names are here.
    let (web, _) = wait_for("the feed's projects", 30, || {
        let w = project(&c, "acme", "web-app")?;
        project(&c, "acme", "api")?;
        Some(w)
    });
    let (api, gid) = project(&c, "acme", "api").unwrap();
    assert_eq!(gid, "p2");
    // Renamed at the directory, renamed here; deleted, deleted.
    dir.add_project("p2", "t1", "API v2");
    wait_for("the rename", 30, || project(&c, "acme", "api-v2"));
    dir.remove_row("projects", "p1");
    wait_for("the delete", 30, || {
        project(&c, "acme", "web-app").is_none().then_some(())
    });
    let _ = web;

    // A volume in a project; the directory deletes the project: kept,
    // unlinked.
    let api_s = api.to_string();
    c.ctl(&[
        "create-volume",
        "--size",
        "1M",
        "--name",
        "disk",
        "--project",
        &api_s,
    ])
    .ok();
    // Reports carry its storage (glidex D85).
    let storage = wait_for("a report with the volume", 30, || {
        let s = dir.cluster(&c.cluster_id)?.storage.get("t1")?.clone();
        (s.volumes == 1).then_some(s)
    });
    assert_eq!(storage.volume_gib, 1);
    dir.remove_row("projects", "p2");
    wait_for("the project kept, unlinked", 30, || {
        let (_, g) = project(&c, "acme", "api-v2")?;
        g.is_empty().then_some(())
    });
    let dirs = c.ctl(&["admission", "directories"]).ok();
    assert!(!dirs.has("not done yet"), "{}", dirs.out);

    // A second tenant of the directory; a project moves between them.
    let offer2 = c
        .ctl(&[
            "admission",
            "offer",
            "--tenant",
            "lab",
            "--directory",
            &issuer,
            "--url",
            &url,
        ])
        .ok();
    rt.block_on(dir.redeem(&token(&offer2), &lab)).unwrap();
    dir.add_project("p3", "t1", "shared");
    wait_for("project shared", 30, || project(&c, "acme", "shared"));
    dir.add_project("p3", "t2", "shared");
    wait_for("the move", 60, || project(&c, "lab", "shared"));
    assert!(project(&c, "acme", "shared").is_none());

    // The operator tenant: a tenant of a trusted directory only.
    let local = tenant_id(&c, "local").unwrap().to_string();
    c.ctl(&["cluster", "set-operator", "--tenant", &local])
        .expect(1);
    c.ctl(&["cluster", "set-operator", "--tenant", &acme_id.to_string()])
        .ok();
    assert!(
        c.ctl(&["admission", "directories"])
            .ok()
            .has("the operator"),
        "the operator tenant listed"
    );

    // The directory ends lab's trust: lab stays, unlinked.
    let s = rt.block_on(dir.end_trust(&c.cluster_id, "t2")).unwrap();
    assert_eq!(s, 200);
    let tenants = c.ctl(&["tenant", "list"]).ok();
    assert!(!tenants.has("t2 at"), "{}", tenants.out);
    assert!(tenant_id(&c, "lab").is_some());
    assert_eq!(project(&c, "lab", "shared").unwrap().1, "");

    // An offer revoked can't be redeemed. lab is local now: offered
    // again, its tenant needs another name.
    let again = [
        "admission",
        "offer",
        "--tenant",
        "lab",
        "--directory",
        &issuer,
        "--url",
        &url,
    ];
    c.ctl(&again).expect(1);
    let offer3 = c.ctl(&[&again[..], &["--name", "lab-again"]].concat()).ok();
    let id = offer3.out.split_whitespace().nth(1).unwrap().to_string();
    c.ctl(&["admission", "revoke", "--id", &id]).ok();
    assert!(rt.block_on(dir.redeem(&token(&offer3), &lab)).is_err());

    // The cluster ends acme's trust: the directory is told, and nothing
    // is trusted any more; acme stays, unlinked.
    c.ctl(&[
        "admission",
        "end",
        "--directory",
        &issuer,
        "--tenant",
        "acme",
    ])
    .ok();
    wait_for("the directory told", 30, || {
        dir.cluster(&c.cluster_id).is_none().then_some(())
    });
    assert!(
        c.ctl(&["admission", "directories"])
            .ok()
            .has("No trusted directories")
    );
    assert!(tenant_id(&c, "acme").is_some());

    // Audited: the offers, the redeem and its refusals, the feed's
    // changes, the ends; never a secret.
    let audit = c.audit().join("\n");
    for op in [
        "CreateOffer",
        "RedeemOffer",
        "TrustFeed",
        "EndFromDirectory",
        "EndTrust",
        "EndToldDirectory",
        "RevokeOffer",
        "SetOperator",
    ] {
        assert!(audit.contains(op), "{op} not audited:\n{audit}");
    }
    assert!(!audit.contains(&t1[4..]), "a token in the audit");
}
