//! Datasets and their properties (spec/29-hsfs.md §32.2, §32.3;
//! 04-decisions.md D360, D361): volumes named `tenant/project/name`,
//! properties set on a tenant, a project or a dataset and inherited down,
//! `pool` choosing where a volume goes, names unique in a project (copies
//! and restores taking their own, claimed once however many ask, D424),
//! and `hsfs` as `hyperstorctl dataset`.

use hyperstor_test::*;

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

/// The id a `... made (id N)` line names first.
fn made_id(r: &Run) -> String {
    let at = r.out.find("(id ").unwrap_or_else(|| panic!("{}", r.out));
    r.out[at + 4..].split(')').next().unwrap().to_string()
}

/// The id a `volume ID created ...` line names.
fn volume_id(r: &Run) -> String {
    r.out.split_whitespace().nth(1).unwrap().to_string()
}

#[test]
fn datasets_are_named_in_projects_and_inherit_properties() {
    let c = Cluster::all_in_one("datasets", 3);
    wait_for("every device's kind", 20, || {
        let s = c.ctl(&["status", "--json"]);
        (s.out.matches("\"kind\":\"file\"").count() == 3).then_some(())
    });
    c.ctl(&["pool", "create", "--name", "fast", "--classes", "file"])
        .ok();
    wait_for("pool fast in the map", 10, || {
        c.ctl(&["pools"]).has("fast").then_some(())
    });
    let acme = made_id(&c.ctl(&["tenant", "create", "--name", "acme"]).ok());
    let ml = made_id(
        &c.ctl(&["project", "create", "--tenant", &acme, "--name", "ml"])
            .ok(),
    );

    // Properties: set on the tenant, inherited by the project, overridden
    // there, each with its source.
    c.hsfs(&["set", "quota=1024G", "class=bulk", "acme"]).ok();
    c.hsfs(&["set", "pool=fast", "class=latency", "acme/ml"])
        .ok();
    let got = json(&c.hsfs(&["get", "all", "acme/ml", "--json"]).ok().out);
    let prop = |g: &serde_json::Value, n: &str| {
        g["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("{n}: {g}"))
    };
    assert_eq!(got["kind"], "project");
    assert_eq!(prop(&got, "quota")["value"], "1T");
    assert_eq!(prop(&got, "quota")["source"], "acme");
    assert_eq!(prop(&got, "class")["value"], "latency");
    assert_eq!(prop(&got, "class")["source"], "local");
    assert_eq!(prop(&got, "pool")["value"], "fast");
    assert_eq!(prop(&got, "pool")["enforced"], true);
    assert_eq!(prop(&got, "reads")["source"], "default");
    let text = c.ctl(&["dataset", "get", "--dataset", "acme/ml"]).ok();
    assert!(text.has("not enforced yet"), "{}", text.out);
    // Refused: no such property, a value it doesn't take, a pool that
    // isn't, and nfs on a volume (below).
    c.hsfs(&["set", "nope=1", "acme"]).expect(1);
    c.hsfs(&["set", "quota=lots", "acme"]).expect(1);
    c.hsfs(&["set", "pool=nowhere", "acme"]).expect(1);
    c.hsfs(&["get", "all", "acme/nothing"]).expect(1);

    // `pool`: a volume made naming no pool goes to its project's.
    let v = c
        .ctl(&[
            "create-volume",
            "--size",
            "1M",
            "--name",
            "train",
            "--project",
            &ml,
        ])
        .ok();
    assert!(v.has("created in pool 3"), "{}", v.out);
    let train = volume_id(&v);
    // Without the property, the first pool, as before.
    c.hsfs(&["inherit", "pool", "acme/ml"]).ok();
    let w = c
        .ctl(&[
            "create-volume",
            "--size",
            "1M",
            "--name",
            "eval",
            "--project",
            &ml,
        ])
        .ok();
    assert!(!w.has("created in pool 3"), "{}", w.out);
    let eval = volume_id(&w);

    // Names are unique in a project; another project may have one.
    let r = c.ctl(&[
        "create-volume",
        "--size",
        "1M",
        "--name",
        "train",
        "--project",
        &ml,
    ]);
    assert!(r.code == 1 && r.has("unique in its project"), "{}", r.out);
    c.ctl(&[
        "create-volume",
        "--size",
        "1M",
        "--name",
        "train",
        "--tenant",
        &acme,
    ])
    .ok();
    // A copy named by none is NAME-copy.
    c.ctl(&["copy-volume", "--id", &train, "--pool", "1", "--wait", "60"])
        .ok();
    let list = json(&c.hsfs(&["list", "acme", "--json"]).ok().out);
    let names: Vec<&str> = list["datasets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    for n in [
        "acme/ml/train",
        "acme/ml/eval",
        "acme/ml/train-copy",
        "acme/default/train",
    ] {
        assert!(names.contains(&n), "{n}: {names:?}");
    }
    let only_ml = json(
        &c.hsfs(&["list", "-t", "volume", "acme/ml", "--json"])
            .ok()
            .out,
    );
    assert_eq!(
        only_ml["datasets"].as_array().unwrap().len(),
        3,
        "{only_ml}"
    );
    // A name clone may not take; one of its own it may.
    let snap = c
        .ctl(&["snapshot", "create", "--id", &train, "--name", "s"])
        .ok();
    let epoch = json(
        &c.ctl(&["snapshot", "list", "--id", &train, "--json"])
            .ok()
            .out,
    )["snapshots"][0]["epoch"]
        .clone();
    let epoch = epoch
        .as_u64()
        .or_else(|| epoch.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("{epoch} {}", snap.out))
        .to_string();
    let r = c.ctl(&[
        "clone-volume",
        "--id",
        &train,
        "--epoch",
        &epoch,
        "--name",
        "eval",
    ]);
    assert!(r.code == 1 && r.has("unique in its project"), "{}", r.out);
    c.ctl(&[
        "clone-volume",
        "--id",
        &train,
        "--epoch",
        &epoch,
        "--name",
        "train-2",
    ])
    .ok();

    // A dataset's own properties, and one that doesn't apply to it.
    c.hsfs(&["set", "readonly=on", "acme/ml/train"]).ok();
    let got = json(
        &c.hsfs(&["get", "readonly,class", "acme/ml/train", "--json"])
            .ok()
            .out,
    );
    assert_eq!(got["kind"], "volume");
    assert_eq!(prop(&got, "readonly")["source"], "local");
    assert_eq!(prop(&got, "class")["source"], "acme/ml");
    assert_eq!(got["properties"].as_array().unwrap().len(), 2);
    c.hsfs(&["set", "nfs=rw", "acme/ml/train"]).expect(1);
    c.hsfs(&["get", "snapdir", "acme/ml/train"]).expect(1);

    // A volume back from the trash whose name was taken meanwhile.
    c.ctl(&["delete-volume", "--id", &eval]).ok();
    c.ctl(&[
        "create-volume",
        "--size",
        "1M",
        "--name",
        "eval",
        "--project",
        &ml,
        "--pool",
        "3",
    ])
    .ok();
    let back = c.ctl(&["trash", "restore", "--id", &eval]).ok();
    assert!(back.has("eval-restored"), "{}", back.out);

    // A deleted project takes its properties; the tenant keeps its own.
    let tmp = made_id(
        &c.ctl(&["project", "create", "--tenant", &acme, "--name", "tmp"])
            .ok(),
    );
    c.hsfs(&["set", "quota=1G", "acme/tmp"]).ok();
    c.ctl(&["project", "delete", "--project", &tmp]).ok();
    let again = made_id(
        &c.ctl(&["project", "create", "--tenant", &acme, "--name", "tmp"])
            .ok(),
    );
    assert_ne!(again, tmp);
    let got = json(&c.hsfs(&["get", "quota", "acme/tmp", "--json"]).ok().out);
    assert_eq!(prop(&got, "quota")["value"], "1T");
    assert_eq!(prop(&got, "quota")["source"], "acme");

    // Audited, by who set them.
    let audit = c.ctl(&["audit", "list", "--op", "SetDatasetProperty"]).ok();
    assert!(
        audit.has("acme/ml") && audit.has("pool=fast"),
        "{}",
        audit.out
    );

    // hsfs prints its own usage line, and refuses what isn't a command.
    // Installed as a link to hyperstorctl.
    assert_eq!(
        std::fs::read_link(c.root.join("usr/local/bin/hsfs")).unwrap(),
        std::path::Path::new("hyperstorctl")
    );
    let u = Run::of(
        std::process::Command::new(c.root.join("usr/local/bin/hsfs"))
            .output()
            .unwrap(),
    );
    assert!(u.code == 2 && u.has("usage: hsfs"), "{}", u.out);
    c.hsfs(&["destroy", "acme"]).expect(2);
}

/// `readonly=on` (D420): the volume's primaries refuse clients' writes,
/// set on it or inherited from its project, while it still reads; the
/// API refuses expanding and rolling it back; inherited back off, it
/// takes writes again.
#[test]
fn a_read_only_volume_reads_and_refuses_writes() {
    let c = Cluster::all_in_one("readonly", 3);
    let acme = made_id(&c.ctl(&["tenant", "create", "--name", "acme"]).ok());
    let ml = made_id(
        &c.ctl(&["project", "create", "--tenant", &acme, "--name", "ml"])
            .ok(),
    );
    let v = volume_id(
        &c.ctl(&[
            "create-volume",
            "--size",
            "4M",
            "--name",
            "data",
            "--project",
            &ml,
        ])
        .ok(),
    );
    let image: Vec<u8> = (0..1 << 20).map(|i| (i / 4096 % 251) as u8 + 1).collect();
    let src = c.scratch.path("image.raw");
    std::fs::write(&src, &image).unwrap();
    let write = || c.ctl(&["write-volume", "--id", &v, "--file", src.to_str().unwrap()]);
    let read = || {
        let out = c.scratch.path("back.raw");
        c.ctl(&[
            "read-volume",
            "--id",
            &v,
            "--size",
            "1M",
            "--out",
            out.to_str().unwrap(),
        ])
        .ok();
        std::fs::read(out).unwrap()
    };
    write().ok();
    c.ctl(&["snapshot", "create", "--id", &v, "--name", "s"])
        .ok();

    // Inherited from its project.
    c.hsfs(&["set", "readonly=on", "acme/ml"]).ok();
    let r = write();
    assert!(r.code != 0 && r.has("read-only"), "{}", r.out);
    assert_eq!(read(), image, "it still reads");
    let r = c.ctl(&["expand-volume", "--id", &v, "--size", "8M"]);
    assert!(r.code == 1 && r.has("read-only"), "{}", r.out);
    let r = c.ctl(&["rollback-volume", "--id", &v, "--epoch", "1"]);
    assert!(r.code == 1 && r.has("read-only"), "{}", r.out);
    let got = json(
        &c.hsfs(&["get", "readonly", "acme/ml/data", "--json"])
            .ok()
            .out,
    );
    assert_eq!(got["properties"][0]["value"], "on");
    assert_eq!(got["properties"][0]["source"], "acme/ml");
    assert_eq!(got["properties"][0]["enforced"], true);

    // Set off on the volume itself, below its project's on: it writes.
    c.hsfs(&["set", "readonly=off", "acme/ml/data"]).ok();
    write().ok();
    // Back to its project's, then the project's cleared too.
    c.hsfs(&["inherit", "readonly", "acme/ml/data"]).ok();
    assert!(write().code != 0);
    c.hsfs(&["inherit", "readonly", "acme/ml"]).ok();
    write().ok();
    c.ctl(&["expand-volume", "--id", &v, "--size", "8M"]).ok();
}

/// D424: a name is claimed in the index before its dataset is made, so of
/// several creates of one name at once only one makes it; deleting it frees
/// the name; datasets made before the index are indexed by the leader.
#[test]
fn a_name_is_taken_once_however_many_ask() {
    let c = Cluster::all_in_one("names", 3);
    let acme = made_id(&c.ctl(&["tenant", "create", "--name", "acme"]).ok());
    let ml = made_id(
        &c.ctl(&["project", "create", "--tenant", &acme, "--name", "ml"])
            .ok(),
    );
    let create = |name: &str| {
        c.ctl(&[
            "create-volume",
            "--size",
            "1M",
            "--pool",
            "1",
            "--name",
            name,
            "--project",
            &ml,
        ])
    };
    // Made before the index is complete: checked against the datasets,
    // then indexed with the others.
    let early = volume_id(&create("early").ok());
    wait_for("the names indexed", 90, || {
        c.meta_log().contains("dataset names indexed").then_some(())
    });
    assert!(create("early").expect(1).has("unique in its project"));

    // Six at once: one is made, five are refused, whichever member or
    // order.
    let runs: Vec<Run> = std::thread::scope(|s| {
        let all: Vec<_> = (0..6).map(|_| s.spawn(|| create("dup"))).collect();
        all.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let made: Vec<&Run> = runs.iter().filter(|r| r.code == 0).collect();
    assert_eq!(made.len(), 1, "{runs:#?}");
    for r in runs.iter().filter(|r| r.code != 0) {
        assert!(r.has("unique in its project"), "{}", r.out);
    }
    let dup = volume_id(made[0]);
    let listed = c.hsfs(&["list"]).ok();
    assert_eq!(
        listed.out.matches("acme/ml/dup").count(),
        1,
        "{}",
        listed.out
    );

    // Deleting frees the name; the trash doesn't hold it (D387).
    for id in [&dup, &early] {
        c.ctl(&["delete-volume", "--id", id]).ok();
    }
    create("dup").ok();
    create("early").ok();
}
