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

/// Whether `python` has boto3, for the gateway's part; skipped without it
/// (as tests/s3.rs does), unless HS_S3_SDK_REQUIRED.
fn sdk_python() -> Option<String> {
    let python = std::env::var("HS_S3_SDK_PYTHON").unwrap_or_else(|_| "python3".into());
    let has = std::process::Command::new(&python)
        .args(["-c", "import boto3"])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has {
        assert!(std::env::var_os("HS_S3_SDK_REQUIRED").is_none());
        eprintln!("skipping the gateway's part: {python} has no boto3");
        return None;
    }
    Some(python)
}

/// One step of tests/s3_keys.py at the gateway on `port`, with key
/// `id`/`secret`: its last line.
fn sdk(
    python: &str,
    port: u16,
    ca: &std::path::Path,
    key: &(String, String),
    step: &[&str],
) -> String {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/s3_keys.py");
    let o = std::process::Command::new(python)
        .arg(script)
        .arg(format!("https://127.0.0.1:{port}"))
        .arg(ca)
        .args(step)
        .env("AWS_ACCESS_KEY_ID", &key.0)
        .env("AWS_SECRET_ACCESS_KEY", &key.1)
        .env_remove("AWS_SESSION_TOKEN")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    if !o.status.success() {
        // The gateway not up yet, say: the caller's assertion or wait says
        // what was expected.
        let err = String::from_utf8_lossy(&o.stderr);
        return format!("failed: {}", err.lines().last().unwrap_or_default());
    }
    out.lines().last().unwrap_or_default().to_string()
}

/// A project moves to another tenant (§24.9.1, D303): refused first for a
/// default project, its own tenant, a tenant that doesn't exist, and a
/// bucket name the new tenant holds; then, as an operation, its volume is
/// the new tenant's, its bucket takes the new tenant's name and is found
/// by the new tenant's key and no longer by the old one's, the old
/// tenant's key that put buckets in it puts them in its default project,
/// and the move is audited.
#[test]
fn a_project_moves_to_another_tenant() {
    let mut c = Cluster::all_in_one("projmove", 3);
    let lab = made_id(&c.ctl(&["tenant", "create", "--name", "lab"]).ok());
    let acme = made_id(&c.ctl(&["tenant", "create", "--name", "acme"]).ok());
    let (lab_s, acme_s) = (lab.to_string(), acme.to_string());
    let web = made_id(
        &c.ctl(&["project", "create", "--tenant", &lab_s, "--name", "web"])
            .ok(),
    );
    let web_s = web.to_string();
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
    let vol = v.out.split_whitespace().nth(1).unwrap().to_string();
    let default_of = |tenant: u32| {
        json(&c.ctl(&["tenant", "list", "--json"]).ok().out)["tenants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == tenant)
            .unwrap()["default_project"]
            .as_u64()
            .unwrap()
            .to_string()
    };
    // Refused before anything changes.
    let r = c
        .ctl(&[
            "project",
            "move",
            "--project",
            &default_of(lab),
            "--to",
            &acme_s,
        ])
        .expect(1);
    assert!(r.has("default project"), "{}", r.out);
    c.ctl(&["project", "move", "--project", &web_s, "--to", &lab_s])
        .expect(1);
    c.ctl(&["project", "move", "--project", &web_s, "--to", "999"])
        .expect(1);
    c.ctl(&["project", "move", "--project", &web_s]).expect(2);

    // The gateway's part: a bucket of the project, and one of acme's of
    // the same name in the way.
    let python = sdk_python();
    let key = |tenant: &str, project: Option<&str>| {
        let mut a = vec!["s3", "key", "create", "--tenant", tenant];
        if let Some(p) = project {
            a.extend(["--project", p]);
        }
        let made = c.ctl(&a).ok();
        let mut f = made.out.split_whitespace();
        (f.next().unwrap().to_string(), f.next().unwrap().to_string())
    };
    let lab_key = key(&lab_s, Some(&web_s));
    let acme_key = key(&acme_s, None);
    if python.is_some() {
        install(&c.root, &[]).ok(); // the default placement
        c.start_s3();
    }
    if let Some(python) = &python {
        let ca = c.pki().join("root.crt");
        let step = |k: &(String, String), s: &[&str]| sdk(python, c.s3_port, &ca, k, s);
        wait_for("lab's key at the gateway", 30, || {
            (step(&lab_key, &["create", "site"]) == "created").then_some(())
        });
        assert_eq!(step(&lab_key, &["roundtrip", "site"]), "roundtrip ok");
        wait_for("acme's key at the gateway", 30, || {
            (step(&acme_key, &["create", "site"]) == "created").then_some(())
        });
        let r = c
            .ctl(&["project", "move", "--project", &web_s, "--to", &acme_s])
            .expect(1);
        assert!(r.has("bucket named site"), "{}", r.out);
        c.ctl(&[
            "s3", "bucket", "delete", "--tenant", &acme_s, "--name", "site",
        ])
        .ok();
    }

    // The move, followed to its end.
    let r = c
        .ctl(&[
            "project",
            "move",
            "--project",
            &web_s,
            "--to",
            &acme_s,
            "--wait",
            "60",
        ])
        .ok();
    assert!(r.has("moving to tenant"), "{}", r.out);
    let vols = json(&c.ctl(&["volumes", "--json"]).ok().out);
    let row = vols["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == vol.as_str())
        .cloned()
        .unwrap();
    assert_eq!(row["tenant"], acme, "{row}");
    assert_eq!(row["project_name"], "acme/web", "{row}");
    let list = c.ctl(&["project", "list", "--tenant", &acme_s]).ok();
    assert!(list.has("web") && list.has("active"), "{}", list.out);
    // lab's key that put buckets in web puts them in lab's default now.
    let keys = json(&c.ctl(&["s3", "key", "list", "--json"]).ok().out);
    let k = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == lab_key.0.as_str())
        .cloned()
        .unwrap();
    assert!(k.get("project").is_none_or(|p| p == 0), "{k}");
    if let Some(python) = &python {
        let ca = c.pki().join("root.crt");
        let step = |k: &(String, String), s: &[&str]| sdk(python, c.s3_port, &ca, k, s);
        let buckets = c.ctl(&["s3", "bucket", "list"]).ok();
        assert!(
            buckets.has(&format!("{acme}/site")) && !buckets.has(&format!("{lab}/site")),
            "{}",
            buckets.out
        );
        assert_eq!(step(&acme_key, &["has", "site"]), "yes");
        assert_eq!(step(&acme_key, &["roundtrip", "site"]), "roundtrip ok");
        assert_ne!(step(&lab_key, &["has", "site"]), "yes");
    }
    wait_for("the move's audit records", 30, || {
        let r = c.ctl(&["audit", "list"]);
        (r.has("MoveProject") && r.has("operation")).then_some(())
    });
}
