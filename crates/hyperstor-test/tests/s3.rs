//! S3's access keys and placements in the map service (14-s3.md §14.2,
//! 04-decisions.md D231-D233), on an installed cluster: `hyperstorctl s3`
//! makes and revokes keys and sets placements; the secret is shown once
//! and kept nowhere in the clear; `hyperstor-s3` fetches them over the
//! cluster API with an `s3` certificate, which no other role may do, and
//! follows changes within seconds. AWS's Python SDK drives the gateway
//! where `HS_S3_SDK_PYTHON` names a Python that has it (CI's does, and
//! `HS_S3_SDK_REQUIRED` makes it a failure without).

use hyperstor_test::{Cluster, bin_dir, free_ports, wait_for};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A running gateway, killed when dropped.
struct Gateway(Child, PathBuf);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Gateway {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.1).unwrap_or_default()
    }
}

/// `hyperstor-s3` on `port`, with the identity in `id` (also its HTTPS
/// certificate), keys from the cluster API.
fn gateway(c: &Cluster, id: &Path, port: u16, name: &str) -> Gateway {
    let log = c.scratch.path(&format!("{name}.log"));
    let p = Command::new(bin_dir().join("hyperstor-s3"))
        .args(["--meta", &c.map, "--api", &c.api])
        .args([
            "--tls",
            id.to_str().unwrap(),
            "--identity",
            id.to_str().unwrap(),
        ])
        .args(["--listen", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    Gateway(p, log)
}

/// An identity signed by the cluster's intermediate CA, as a join would
/// give one: `s3.crt` (the chain), `s3.key` and `root.crt` in a new
/// directory.
fn identity(c: &Cluster, role: hyperstor_pki::Role, name: &str) -> PathBuf {
    let pki = c.pki();
    let read = |f: &str| std::fs::read_to_string(pki.join(f)).unwrap();
    let ca = hyperstor_pki::Pem {
        cert: read("intermediate.crt"),
        key: read("intermediate.key"),
    };
    let who = hyperstor_pki::Identity {
        cluster: c.cluster_id.clone(),
        role,
        id: name.into(),
    };
    let leaf = hyperstor_pki::issue(
        &ca,
        &who,
        &["127.0.0.1".parse().unwrap()],
        &["localhost".into()],
        1,
    )
    .unwrap();
    let dir = c.scratch.path(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("s3.crt"), &leaf.cert).unwrap();
    std::fs::write(dir.join("s3.key"), &leaf.key).unwrap();
    std::fs::write(dir.join("root.crt"), read("root.crt")).unwrap();
    dir
}

/// Every file under `dir`, for looking for a secret in the clear.
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn holds(path: &Path, needle: &[u8]) -> bool {
    std::fs::read(path).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
}

/// One step of `tests/s3_keys.py` with key `id`, `secret`; its last line.
fn sdk(python: &str, port: u16, ca: &Path, id: &str, secret: &str, step: &[&str]) -> String {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/s3_keys.py");
    let o = Command::new(python)
        .arg(script)
        .arg(format!("https://127.0.0.1:{port}"))
        .arg(ca)
        .args(step)
        .env("AWS_ACCESS_KEY_ID", id)
        .env("AWS_SECRET_ACCESS_KEY", secret)
        .env_remove("AWS_SESSION_TOKEN")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        o.status.success(),
        "s3_keys.py {step:?}: {out}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    out.lines().last().unwrap_or_default().to_string()
}

#[test]
fn s3_keys_and_placements_come_from_the_map_service() {
    let c = Cluster::all_in_one("s3keys", 3);
    // Placements: none yet; a cache pool is refused, the replicated one
    // is the default.
    assert!(
        c.ctl(&["s3", "placement", "list"])
            .ok()
            .has("no S3 placements")
    );
    let r = c
        .ctl(&[
            "s3",
            "placement",
            "set",
            "--name",
            "default",
            "--data-pool",
            "cache",
            "--index-pool",
            "vol",
        ])
        .expect(1);
    assert!(r.has("cache pool"), "{}", r.out);
    let r = c
        .ctl(&[
            "s3",
            "placement",
            "set",
            "--name",
            "default",
            "--data-pool",
            "nope",
            "--index-pool",
            "vol",
        ])
        .expect(1);
    assert!(r.has("no pool"), "{}", r.out);
    c.ctl(&[
        "s3",
        "placement",
        "set",
        "--name",
        "default",
        "--data-pool",
        "vol",
        "--index-pool",
        "vol",
    ])
    .ok();
    assert!(c.ctl(&["s3", "placement", "list"]).ok().has("default"));
    // A key: the id and secret once, then only the id.
    c.ctl(&["s3", "key", "create", "--tenant", "0"]).expect(1);
    let made = c
        .ctl(&[
            "s3",
            "key",
            "create",
            "--tenant",
            "1",
            "--description",
            "e2e",
        ])
        .ok();
    let line = made.out.lines().next().unwrap().to_string();
    let (id, secret) = line.split_once(' ').unwrap();
    let (id, secret) = (id.to_string(), secret.trim().to_string());
    assert!(id.starts_with("HS") && id.len() == 20, "{id}");
    assert_eq!(secret.len(), 40);
    // The audit names the key; its records reach the log in batches.
    let audit = wait_for("the key's audit record", 30, || {
        let r = c.ctl(&["audit", "list"]);
        r.has("CreateS3Key").then_some(r)
    });
    for list in [
        c.ctl(&["s3", "key", "list"]).ok(),
        c.ctl(&["s3", "key", "list", "--json"]).ok(),
        audit,
    ] {
        assert!(list.has(&id), "{}", list.out);
        assert!(!list.has(&secret), "a secret shown again");
    }
    // Nowhere in the clear: not in the store, its backups or the logs.
    let bak = c.scratch.path("map.bak");
    c.ctl(&["map", "backup", "--out", bak.to_str().unwrap()])
        .ok();
    let mut all = vec![bak, c.meta_log.clone()];
    files(&c.data_dir().join("meta"), &mut all);
    for f in &all {
        assert!(
            !holds(f, secret.as_bytes()),
            "{} holds the secret",
            f.display()
        );
    }
    // s3 tokens name their gateway, as mgmt's do.
    c.ctl(&["token", "create", "--role", "s3"]).expect(1);
    c.ctl(&["token", "create", "--role", "s3", "--name", "gw1"])
        .ok();

    // Only an s3 identity reads the keys: an administrator's is refused.
    let port = free_ports(2);
    let admin = identity(&c, hyperstor_pki::Role::Admin, "not-a-gateway");
    let refused = gateway(&c, &admin, port + 1, "refused");
    wait_for("the admin identity refused", 30, || {
        refused
            .log()
            .contains("may not read S3's keys")
            .then_some(())
    });
    drop(refused);
    let gw = gateway(
        &c,
        &identity(&c, hyperstor_pki::Role::S3, "gw1"),
        port,
        "gw1",
    );
    wait_for("the gateway's first keys", 30, || {
        gw.log()
            .contains("1 access keys, 1 placements")
            .then_some(())
    });
    assert!(!gw.log().contains(&secret), "the gateway logs no secret");

    let python = std::env::var("HS_S3_SDK_PYTHON").unwrap_or_else(|_| "python3".into());
    let has_sdk = Command::new(&python)
        .args(["-c", "import boto3"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_sdk {
        assert!(
            std::env::var_os("HS_S3_SDK_REQUIRED").is_none(),
            "{python} has no boto3"
        );
        eprintln!("skipping the SDK's part: {python} has no boto3 (set HS_S3_SDK_PYTHON)");
        return;
    }
    let ca = c.pki().join("root.crt");
    let step = |s: &[&str]| sdk(&python, port, &ca, &id, &secret, s);
    // CreateBucket from the key, on the default placement; a placement
    // that isn't there is refused until it is set.
    assert_eq!(step(&["create", "first"]), "created");
    assert_eq!(step(&["roundtrip", "first"]), "roundtrip ok");
    assert_eq!(
        step(&["create", "second", "fast"]),
        "InvalidLocationConstraint"
    );
    c.ctl(&[
        "s3",
        "placement",
        "set",
        "--name",
        "fast",
        "--data-pool",
        "vol",
        "--index-pool",
        "vol",
    ])
    .ok();
    wait_for("the new placement at the gateway", 15, || {
        (step(&["create", "second", "fast"]) == "created").then_some(())
    });
    assert_eq!(step(&["roundtrip", "second"]), "roundtrip ok");
    // A wrong secret is refused; a revoked key within seconds.
    let wrong = sdk(&python, port, &ca, &id, &"x".repeat(40), &["denied"]);
    assert_eq!(wrong, "SignatureDoesNotMatch");
    assert_eq!(step(&["denied"]), "allowed");
    c.ctl(&["s3", "key", "revoke", "--id", &id]).ok();
    c.ctl(&["s3", "key", "revoke", "--id", &id]).expect(1);
    wait_for("the revoked key refused", 15, || {
        (step(&["denied"]) == "InvalidAccessKeyId").then_some(())
    });
    // The gateway keeps its keys while the map service is away: a wrong
    // secret for a key it knows is a bad signature, not an unknown key
    // (the data path isn't needed to tell).
    let again = c
        .ctl(&["s3", "key", "create", "--tenant", "1"])
        .ok()
        .out
        .lines()
        .next()
        .unwrap()
        .to_string();
    let (id2, _) = again.split_once(' ').unwrap();
    let wrong2 = || sdk(&python, port, &ca, id2, &"x".repeat(40), &["denied"]);
    wait_for("the new key at the gateway", 15, || {
        (wrong2() == "SignatureDoesNotMatch").then_some(())
    });
    let mut c = c;
    c.stop_meta();
    wait_for("the gateway noticing", 30, || {
        gw.log()
            .contains("serving with the last ones")
            .then_some(())
    });
    assert_eq!(wrong2(), "SignatureDoesNotMatch");
}
