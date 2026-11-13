//! Tenants and projects (spec/20-tenants.md §24.9, 04-decisions.md
//! D299–D301): records with ids given once, a default project per
//! tenant, volumes and clones in projects (a clone in another tenant's),
//! keys naming the project of their buckets, deletes refused while
//! something is in them, and roles on a tenant or a project through
//! hyperstor-mgmt reaching only it.

use hyperstor_test::*;

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

/// The id a `... made (id N)` line names first.
fn made_id(r: &Run) -> u32 {
    let at = r.out.find("(id ").unwrap_or_else(|| panic!("{}", r.out));
    r.out[at + 4..]
        .split(')')
        .next()
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("{}", r.out))
}

#[test]
fn tenants_and_projects_own_volumes_and_keys() {
    let c = Cluster::all_in_one("tenants", 3);
    // A bare tenant in use before records: its number is adopted, and
    // a new tenant gets one past it.
    let old = c
        .ctl(&[
            "create-volume",
            "--size",
            "1M",
            "--name",
            "old",
            "--tenant",
            "40",
        ])
        .ok();
    let old_vol = old.out.split_whitespace().nth(1).unwrap().to_string();
    let lab = c.ctl(&["tenant", "create", "--name", "lab"]).ok();
    let lab_id = made_id(&lab);
    assert!(lab_id > 40, "{}", lab.out);
    let acme = c
        .ctl(&["tenant", "create", "--name", "acme", "--tenant", "40"])
        .ok();
    assert_eq!(made_id(&acme), 40);
    c.ctl(&["tenant", "create", "--name", "lab"]).expect(1);
    c.ctl(&["tenant", "create", "--name", "default"]).expect(1);
    c.ctl(&["tenant", "create", "--name", "x", "--tenant", "40"])
        .expect(1);
    let lab_s = lab_id.to_string();
    let web = made_id(
        &c.ctl(&["project", "create", "--tenant", &lab_s, "--name", "web"])
            .ok(),
    );
    let web_s = web.to_string();
    c.ctl(&["project", "create", "--tenant", &lab_s, "--name", "web"])
        .expect(1);
    let list = c.ctl(&["project", "list", "--tenant", &lab_s]).ok();
    assert!(
        list.has("default (default)") && list.has("web"),
        "{}",
        list.out
    );
    let tenants = c.ctl(&["tenant", "list"]).ok();
    assert!(tenants.has("lab") && tenants.has("acme"), "{}", tenants.out);

    // A volume in a project is its tenant's; naming both, they agree.
    let v = c
        .ctl(&[
            "create-volume",
            "--size",
            "1M",
            "--name",
            "site",
            "--project",
            &web_s,
        ])
        .ok();
    let site = v.out.split_whitespace().nth(1).unwrap().to_string();
    c.ctl(&[
        "create-volume",
        "--size",
        "1M",
        "--name",
        "x",
        "--project",
        &web_s,
        "--tenant",
        "40",
    ])
    .expect(1);
    c.ctl(&[
        "create-volume",
        "--size",
        "1M",
        "--name",
        "x",
        "--project",
        "999",
    ])
    .expect(1);
    let vols = c.ctl(&["volumes", "--json"]).ok();
    let vols = json(&vols.out);
    let row = |id: &str| {
        vols["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id}: {vols}"))
    };
    assert_eq!(row(&site)["tenant"], lab_id);
    assert_eq!(row(&site)["project"], web);
    assert_eq!(row(&site)["project_name"], "lab/web");
    // The adopted tenant's old volume counts to its default project.
    assert_eq!(row(&old_vol)["project_name"], "acme/default");
    assert!(c.ctl(&["volumes"]).ok().has("lab/web"));

    // A clone in another tenant's project (a disk from a shared image).
    let snap = c
        .ctl(&["snapshot", "create", "--id", &site, "--name", "golden"])
        .ok();
    let epoch = json(
        &c.ctl(&["snapshot", "list", "--id", &site, "--json"])
            .ok()
            .out,
    )["snapshots"][0]["epoch"]
        .clone();
    // A 64-bit number: JSON gives it as a string.
    let epoch = epoch
        .as_u64()
        .or_else(|| epoch.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("{epoch} {}", snap.out));
    let acme_default = json(&c.ctl(&["tenant", "list", "--json"]).ok().out)["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == 40)
        .unwrap()["default_project"]
        .as_u64()
        .unwrap()
        .to_string();
    let k = c
        .ctl(&[
            "clone-volume",
            "--id",
            &site,
            "--epoch",
            &epoch.to_string(),
            "--name",
            "vm1",
            "--project",
            &acme_default,
            "--json",
        ])
        .ok();
    let clone = json(&k.out)["volume"]["id"].as_str().unwrap().to_string();
    let vols = json(&c.ctl(&["volumes", "--json"]).ok().out);
    let clone_row = vols["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == clone.as_str())
        .cloned()
        .unwrap();
    assert_eq!(clone_row["tenant"], 40);
    assert_eq!(clone_row["project_name"], "acme/default");

    // Keys name a project of their tenant.
    c.ctl(&["s3", "key", "create", "--tenant", "40", "--project", &web_s])
        .expect(1);
    let key = c
        .ctl(&[
            "s3",
            "key",
            "create",
            "--tenant",
            &lab_s,
            "--project",
            &web_s,
            "--json",
        ])
        .ok();
    assert_eq!(json(&key.out)["key"]["project"], web);

    // Deletes wait until nothing is in them.
    let r = c.ctl(&["project", "delete", "--project", &web_s]).expect(1);
    assert!(r.has("still has"), "{}", r.out);
    let r = c.ctl(&["tenant", "delete", "--tenant", &lab_s]).expect(1);
    assert!(r.has("projects web"), "{}", r.out);
    c.ctl(&["project", "rename", "--project", &web_s, "--name", "www"])
        .ok();
    assert!(c.ctl(&["volumes"]).ok().has("lab/www"));
    let lab_default = json(&c.ctl(&["tenant", "list", "--json"]).ok().out)["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == lab_id)
        .unwrap()["default_project"]
        .as_u64()
        .unwrap()
        .to_string();
    c.ctl(&["project", "delete", "--project", &lab_default])
        .expect(1);
    c.ctl(&["tenant", "rename", "--tenant", &lab_s, "--name", "labs"])
        .ok();
    assert!(c.ctl(&["tenant", "list"]).ok().has("labs"));

    // Audited, refusals too; records reach the log in batches.
    wait_for("the tenants' audit records", 30, || {
        let r = c.ctl(&["audit", "list"]);
        [
            "CreateTenant",
            "CreateProject",
            "RenameProject",
            "DeleteProject",
        ]
        .iter()
        .all(|op| r.has(op))
        .then_some(())
    });
}

/// Roles on a tenant or a project (D301), through mgmt: a tenant's
/// storage administrator sees and makes its volumes and projects only; a
/// project's viewer sees its volumes only; grants name ones that exist.
#[test]
fn roles_on_a_tenant_or_project_reach_only_it() {
    let c = Cluster::all_in_one("tenantroles", 3);
    let ann = c.login("ann", Some(&c.claim_code())).unwrap();
    let both = r#"{"roles":["security-admin","storage-admin"]}"#;
    assert_eq!(
        c.as_session(&ann, "PUT", "/auth/users/ann/roles", both).0,
        200
    );
    let lab = made_id(&c.ctl(&["tenant", "create", "--name", "lab"]).ok());
    let other = made_id(&c.ctl(&["tenant", "create", "--name", "other"]).ok());
    let web = made_id(
        &c.ctl(&[
            "project",
            "create",
            "--tenant",
            &lab.to_string(),
            "--name",
            "web",
        ])
        .ok(),
    );
    let vol = |args: &[&str], name: &str| {
        let mut a = vec!["create-volume", "--size", "1M", "--name", name];
        a.extend_from_slice(args);
        c.ctl(&a).ok();
    };
    vol(&["--project", &web.to_string()], "web-vol");
    vol(&["--tenant", &lab.to_string()], "lab-vol");
    vol(&["--tenant", &other.to_string()], "other-vol");

    // Grants name a tenant and a project that exist.
    let bob = c.login("bob", None).unwrap();
    let cy = c.login("cy", None).unwrap();
    for bad in ["viewer@tenant:nope", "viewer@project:lab/nope"] {
        let r = format!(r#"{{"roles":["{bad}"]}}"#);
        assert_eq!(
            c.as_session(&ann, "PUT", "/auth/users/bob/roles", &r).0,
            404,
            "{bad}"
        );
    }
    let r = r#"{"roles":["storage-admin@tenant:lab"]}"#;
    assert_eq!(c.as_session(&ann, "PUT", "/auth/users/bob/roles", r).0, 200);
    let r = r#"{"roles":["viewer@project:lab/web"]}"#;
    assert_eq!(c.as_session(&ann, "PUT", "/auth/users/cy/roles", r).0, 200);

    let call = |s: &Session, rpc: &str, body: &str| {
        c.as_session(s, "POST", &format!("/api/hyperstor.v1.{rpc}"), body)
    };
    // bob: lab's volumes, every project of lab's, and nothing else.
    let (code, _, b) = call(&bob, "ClusterService/ListVolumes", r#"{"pool":1}"#);
    assert_eq!(code, 200, "{b}");
    assert!(
        b.contains("web-vol") && b.contains("lab-vol") && !b.contains("other-vol"),
        "{b}"
    );
    let (code, _, b) = call(
        &bob,
        "TenantService/CreateProject",
        &format!(r#"{{"tenant":{lab},"name":"api"}}"#),
    );
    assert_eq!(code, 200, "{b}");
    let (code, _, b) = call(
        &bob,
        "TenantService/CreateProject",
        &format!(r#"{{"tenant":{other},"name":"api"}}"#),
    );
    assert_eq!(code, 404, "{b}");
    let (code, _, b) = call(&bob, "TenantService/CreateTenant", r#"{"name":"mine"}"#);
    assert_eq!(code, 403, "{b}");
    let (code, _, b) = call(&bob, "TenantService/ListTenants", "{}");
    assert!(
        code == 200 && b.contains("lab") && !b.contains("other"),
        "{b}"
    );
    let create = |s: &Session, args: &str| {
        call(
            s,
            "ClusterService/CreateVolume",
            &format!(r#"{{"pool":1,"size":"1048576","name":"n",{args}}}"#),
        )
    };
    let (code, _, b) = create(&bob, &format!(r#""project":{web}"#));
    assert_eq!(code, 200, "{b}");
    let (code, _, b) = create(&bob, &format!(r#""tenant":{other}"#));
    assert_eq!(code, 403, "{b}");
    let (code, _, b) = create(&bob, r#""tenant":0"#);
    assert_eq!(code, 400, "{b}");

    // cy: project web's volumes alone, to read.
    let (code, _, b) = call(&cy, "ClusterService/ListVolumes", r#"{"pool":1}"#);
    assert_eq!(code, 200, "{b}");
    assert!(
        b.contains("web-vol") && !b.contains("lab-vol") && !b.contains("other-vol"),
        "{b}"
    );
    let (code, _, b) = call(&cy, "TenantService/ListTenants", "{}");
    assert!(
        code == 200 && b.contains("\"web\"") && !b.contains("\"api\""),
        "{b}"
    );
    let (code, _, b) = create(&cy, &format!(r#""project":{web}"#));
    assert_eq!(code, 403, "{b}");
}
