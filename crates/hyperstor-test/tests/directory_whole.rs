//! A glidex directory trusted as a whole (19-administration.md §23.12,
//! D345–D347), against glidex-trust's fake directory: a directory offer
//! redeemed by the directory's owner, its tenants opting in with no offer
//! of their own, admitted at once (`open`) or once approved (`approval`),
//! each with a copy of the default caps; a pending tenant's feed rows not
//! applied, refused tenants asking again, the reports' statuses, a tenant
//! leaving while the directory stays trusted, and the whole trust ended
//! from either side.

use glidex_trust::fake::FakeDirectory;
use glidex_trust::{AdmissionStatus, AdmitSetting};
use hyperstor_test::*;

/// The `gxo_` token an `admission offer` printed.
fn token(r: &Run) -> String {
    r.out
        .split_whitespace()
        .find(|w| w.starts_with("gxo_"))
        .unwrap_or_else(|| panic!("{}", r.out))
        .to_string()
}

/// The id of tenant `name`, from `tenant list --json`, and its glidex id.
fn tenant(c: &Cluster, name: &str) -> Option<(u32, String)> {
    let r = c.ctl(&["tenant", "list", "--json"]).ok();
    let v: serde_json::Value = serde_json::from_str(&r.out).unwrap();
    v["tenants"]
        .as_array()?
        .iter()
        .find(|t| t["name"] == name)
        .map(|t| {
            (
                t["id"].as_u64().unwrap_or(0) as u32,
                t["glidex_id"].as_str().unwrap_or("").to_string(),
            )
        })
}

/// Whether tenant `tenant` here has project `name`.
fn has_project(c: &Cluster, tenant: &str, name: &str) -> bool {
    let r = c.ctl(&["tenant", "list", "--json"]).ok();
    let v: serde_json::Value = serde_json::from_str(&r.out).unwrap();
    v["tenants"].as_array().into_iter().flatten().any(|t| {
        t["name"] == tenant
            && t["projects"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|p| p["name"] == name)
    })
}

#[test]
fn a_directory_trusted_as_a_whole_admits_its_tenants_opt_ins() {
    // Feeds pulled every 300 ms, not 30 s; reports every second.
    // SAFETY: the only test of this binary, before it starts a thread.
    unsafe {
        std::env::set_var("HS_TRUST_POLL_MS", "300");
        std::env::set_var("HS_TRUST_REPORT_MS", "1000");
    }
    let c = Cluster::all_in_one("dirwhole", 3);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = rt.block_on(FakeDirectory::start("uni")).unwrap();
    let issuer = dir.issuer().to_string();
    let physics = dir.add_tenant("t1", "physics");
    let chem = dir.add_tenant("t2", "chem");
    let bio = dir.add_tenant("t3", "bio");
    dir.add_project("p1", "t1", "Optics");
    dir.add_project("p2", "t2", "Labs");
    let url = c.mgmt_url();
    let id = c.cluster_id.clone();
    let pools = c.ctl(&["--json", "pools"]).ok();
    let pools: serde_json::Value = serde_json::from_str(&pools.out).unwrap();
    let pool = pools["pools"][0]["name"].as_str().unwrap().to_string();

    // A setting and caps are a directory offer's alone.
    c.ctl(&[
        "admission",
        "offer",
        "--tenant",
        "physics",
        "--directory",
        &issuer,
        "--url",
        &url,
        "--admit",
        "open",
    ])
    .expect(2);
    // A cap on a pool that doesn't exist is refused.
    c.ctl(&[
        "admission",
        "offer",
        "--directory",
        &issuer,
        "--url",
        &url,
        "--cap",
        "nopool=1G",
    ])
    .expect(1);

    // The directory offer: open, each admission capped at 1 GiB.
    let offer = c
        .ctl(&[
            "admission",
            "offer",
            "--directory",
            &issuer,
            "--url",
            &url,
            "--cap",
            &format!("{pool}=1G"),
        ])
        .ok();
    assert!(offer.has("as a whole"), "{}", offer.out);
    let t1 = token(&offer);
    // Not a tenant's to redeem; the owner redeems it once.
    assert!(rt.block_on(dir.redeem_as(&t1, Some(&physics))).is_err());
    let a = rt.block_on(dir.trust_cluster(&t1)).unwrap();
    assert_eq!(a.admit, Some(AdmitSetting::Open));
    assert!(
        rt.block_on(dir.trust_cluster(&t1)).is_err(),
        "redeemed twice"
    );
    let dirs = c.ctl(&["admission", "directories"]).ok();
    assert!(
        dirs.has("realm uni") && dirs.has("trusted as a whole") && dirs.has("when they opt in"),
        "{}",
        dirs.out
    );
    assert!(tenant(&c, "physics").is_none(), "no tenant admitted yet");

    // Open: physics opts in and is admitted, made at once, with the
    // defaults' copy; the feed then makes its project.
    assert_eq!(
        rt.block_on(dir.opt_in(&id, &physics)),
        Ok(AdmissionStatus::Admitted)
    );
    assert_eq!(tenant(&c, "physics").unwrap().1, "t1");
    wait_for("physics's project", 30, || {
        has_project(&c, "physics", "optics").then_some(())
    });
    let one_gib = format!("caps {pool}={}", 1u64 << 30);
    let dirs = c.ctl(&["admission", "directories"]).ok();
    assert!(dirs.has(&one_gib), "{}", dirs.out);

    // Approval from now on, new caps: chem and bio wait.
    c.ctl(&[
        "admission",
        "directory",
        "--directory",
        &issuer,
        "--admit",
        "approval",
        "--cap",
        &format!("{pool}=2G"),
    ])
    .ok();
    for t in [&chem, &bio, &bio] {
        assert_eq!(
            rt.block_on(dir.opt_in(&id, t)),
            Ok(AdmissionStatus::Pending)
        );
    }
    // An admitted tenant stays admitted.
    assert_eq!(
        rt.block_on(dir.opt_in(&id, &physics)),
        Ok(AdmissionStatus::Admitted)
    );
    // The reports say so; chem's rows, in the feed, aren't applied.
    wait_for("reports of the pending", 30, || {
        let fc = dir.cluster(&id)?;
        (fc.admissions.get("t2") == Some(&AdmissionStatus::Pending)
            && fc.admissions.get("t3") == Some(&AdmissionStatus::Pending))
        .then_some(())
    });
    assert!(tenant(&c, "chem").is_none());
    let pending = c.ctl(&["admission", "pending"]).ok();
    assert!(
        pending.has("chem") && pending.has("bio") && pending.has("pending"),
        "{}",
        pending.out
    );
    // Only a pending tenant is decided.
    c.ctl(&[
        "admission",
        "approve",
        "--directory",
        &issuer,
        "--tenant",
        "physics",
    ])
    .expect(1);

    // chem approved: made, its project from the feed, with the caps as
    // they are now; physics keeps its own.
    c.ctl(&[
        "admission",
        "approve",
        "--directory",
        &issuer,
        "--tenant",
        "chem",
    ])
    .ok();
    assert_eq!(tenant(&c, "chem").unwrap().1, "t2");
    wait_for("chem's project", 30, || {
        has_project(&c, "chem", "labs").then_some(())
    });
    let dirs = c.ctl(&["admission", "directories"]).ok();
    assert!(
        dirs.has(&one_gib) && dirs.has(&format!("caps {pool}={}", 2u64 << 30)),
        "{}",
        dirs.out
    );

    // bio refused: reported so; it asks again and waits again.
    c.ctl(&[
        "admission",
        "reject",
        "--directory",
        &issuer,
        "--tenant",
        "t3",
    ])
    .ok();
    wait_for("a report of the refusal", 30, || {
        (dir.cluster(&id)?.admissions.get("t3") == Some(&AdmissionStatus::Refused)).then_some(())
    });
    assert!(c.ctl(&["admission", "pending"]).ok().has("refused"));
    assert_eq!(
        rt.block_on(dir.opt_in(&id, &bio)),
        Ok(AdmissionStatus::Pending)
    );
    assert!(tenant(&c, "bio").is_none());

    // physics leaves: unlinked; the directory stays trusted.
    assert_eq!(rt.block_on(dir.end_trust(&id, "t1")), Ok(200));
    assert_eq!(tenant(&c, "physics").unwrap().1, "");
    assert!(c.ctl(&["admission", "directories"]).ok().has(&issuer));

    // The cluster ends the whole trust: chem unlinked, bio's opt-in gone,
    // the directory told.
    c.ctl(&["admission", "end", "--directory", &issuer]).ok();
    wait_for("the directory told", 30, || {
        dir.cluster(&id).is_none().then_some(())
    });
    assert!(
        c.ctl(&["admission", "directories"])
            .ok()
            .has("No trusted directories")
    );
    assert_eq!(tenant(&c, "chem").unwrap().1, "");

    // Trusted as a whole again, by default open; the directory ends it
    // from its side: bio, admitted meanwhile, is unlinked.
    let offer2 = c
        .ctl(&["admission", "offer", "--directory", &issuer, "--url", &url])
        .ok();
    let a = rt.block_on(dir.trust_cluster(&token(&offer2))).unwrap();
    assert_eq!(a.admit, Some(AdmitSetting::Open));
    assert_eq!(
        rt.block_on(dir.opt_in(&id, &bio)),
        Ok(AdmissionStatus::Admitted)
    );
    assert_eq!(tenant(&c, "bio").unwrap().1, "t3");
    assert_eq!(rt.block_on(dir.end_trust(&id, "")), Ok(200));
    assert!(
        c.ctl(&["admission", "directories"])
            .ok()
            .has("No trusted directories")
    );
    assert_eq!(tenant(&c, "bio").unwrap().1, "");

    // Audited: the offers, the redeems, each opt-in, decision and change
    // of the setting, the ends.
    let audit = wait_for("the told end audited", 30, || {
        let a = c.audit().join("\n");
        a.contains("EndToldDirectory").then_some(a)
    });
    for op in [
        "CreateOffer",
        "RedeemOffer",
        "AdmitFromDirectory",
        "SetDirectoryAdmission",
        "ApproveAdmission",
        "RefuseAdmission",
        "EndFromDirectory",
        "EndTrust",
        "EndToldDirectory",
    ] {
        assert!(audit.contains(op), "{op} not audited:\n{audit}");
    }
    assert!(!audit.contains(&t1[4..]), "a token in the audit");
}
