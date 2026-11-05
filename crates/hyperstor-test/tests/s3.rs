//! S3's access keys, placements and gateways (14-s3.md §14.2,
//! 04-decisions.md D231-D233), on installed clusters:
//!
//! - `hyperstorctl s3` makes and revokes keys and sets placements; the
//!   secret is shown once and kept nowhere in the clear.
//! - The all-in-one install runs a gateway with an `s3` certificate and
//!   sets its default placement; only `s3` certificates may read the keys.
//! - A host joins as a gateway alone with an `s3` token.
//!
//! AWS's Python SDK drives the gateways where `HS_S3_SDK_PYTHON` names a
//! Python that has it (CI's does, and `HS_S3_SDK_REQUIRED` makes it a
//! failure without).

use hyperstor_test::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A gateway started by hand, killed when dropped.
struct Gateway(Child, PathBuf);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The SDK's Python, or None (and the reason printed) where it has no
/// boto3 and the SDK isn't required.
fn sdk_python() -> Option<String> {
    let python = std::env::var("HS_S3_SDK_PYTHON").unwrap_or_else(|_| "python3".into());
    let has = Command::new(&python)
        .args(["-c", "import boto3"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has {
        assert!(
            std::env::var_os("HS_S3_SDK_REQUIRED").is_none(),
            "{python} has no boto3"
        );
        eprintln!("skipping the SDK's part: {python} has no boto3 (set HS_S3_SDK_PYTHON)");
        return None;
    }
    Some(python)
}

/// One step of `tests/s3_keys.py` against the gateway on `port`, with key
/// `id` and `secret`; its last line.
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

/// A new key of tenant 1: its id and secret.
fn new_key(c: &Cluster) -> (String, String) {
    let made = c.ctl(&["s3", "key", "create", "--tenant", "1"]).ok();
    let line = made.out.lines().next().unwrap().to_string();
    let (id, secret) = line.split_once(' ').unwrap();
    (id.to_string(), secret.trim().to_string())
}

/// An identity signed by the cluster's intermediate CA, in a new
/// directory as the gateway's `--identity` reads it.
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
    let leaf = hyperstor_pki::issue(&ca, &who, &["127.0.0.1".parse().unwrap()], &[], 1).unwrap();
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

fn placement(c: &Cluster, name: &str, data: &str) -> Run {
    c.ctl(&[
        "s3",
        "placement",
        "set",
        "--name",
        name,
        "--data-pool",
        data,
        "--index-pool",
        "vol",
    ])
}

#[test]
fn s3_keys_and_placements_come_from_the_map_service() {
    let mut c = Cluster::all_in_one("s3keys", 3);
    // Installed with the cluster: a gateway whose certificate is an s3
    // identity for this host, serving HTTPS too.
    let env = std::fs::read_to_string(c.root.join("etc/hyperstor/s3.env")).unwrap();
    assert!(
        env.contains(&format!("--api {}", c.api)) && env.contains(&format!("--meta {}", c.map)),
        "{env}"
    );
    assert!(
        c.root
            .join("etc/systemd/system/hyperstor-s3.service")
            .exists()
    );
    let cert = std::fs::read_to_string(c.pki().join("s3.crt")).unwrap();
    let who = hyperstor_pki::identity_in(&cert).unwrap();
    assert_eq!(who.role, hyperstor_pki::Role::S3);
    let (ips, _) = hyperstor_pki::names_of(&cert).unwrap();
    assert!(ips.contains(&"127.0.0.1".parse().unwrap()), "{ips:?}");
    // The default placement is set by the run that finds the map service
    // answering: this one, after the first.
    assert!(
        c.ctl(&["s3", "placement", "list"])
            .ok()
            .has("no S3 placements")
    );
    let again = install(&c.root, &[]);
    assert!(
        again.has("S3's default placement: pool vol"),
        "{}",
        again.out
    );
    assert!(c.ctl(&["s3", "placement", "list"]).ok().has("default"));
    // Placements of a cache pool or a missing pool are refused.
    let r = placement(&c, "fast", "cache").expect(1);
    assert!(r.has("cache pool"), "{}", r.out);
    let r = placement(&c, "fast", "nope").expect(1);
    assert!(r.has("no pool"), "{}", r.out);
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

    // Only an s3 identity reads the keys: an administrator's is refused.
    let admin = identity(&c, hyperstor_pki::Role::Admin, "not-a-gateway");
    let refused_log = c.scratch.path("refused.log");
    let refused = Gateway(
        Command::new(bin_dir().join("hyperstor-s3"))
            .args(["--meta", &c.map, "--api", &c.api])
            .args(["--tls", admin.to_str().unwrap()])
            .args(["--identity", admin.to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{}", free_ports(1))])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&refused_log).unwrap())
            .spawn()
            .unwrap(),
        refused_log.clone(),
    );
    wait_for("the admin identity refused", 30, || {
        log(&refused.1)
            .contains("may not read S3's keys")
            .then_some(())
    });
    drop(refused);
    // The installed gateway, as systemd would run it.
    c.start_s3();
    let gw_log = c.scratch.path("s3.log");
    wait_for("the gateway's first keys", 30, || {
        log(&gw_log)
            .contains("1 access keys, 1 placements")
            .then_some(())
    });
    assert!(
        !log(&gw_log).contains(&secret),
        "the gateway logs no secret"
    );

    let Some(python) = sdk_python() else { return };
    let ca = c.pki().join("root.crt");
    let port = c.s3_port;
    let step = |s: &[&str]| sdk(&python, port, &ca, &id, &secret, s);
    // CreateBucket from the key, on the default placement; a placement
    // that isn't there is refused until it is set.
    assert_eq!(step(&["create", "first"]), "created");
    assert_eq!(step(&["roundtrip", "first"]), "roundtrip ok");
    assert_eq!(
        step(&["create", "second", "fast"]),
        "InvalidLocationConstraint"
    );
    placement(&c, "fast", "vol").ok();
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
    let (id2, _) = new_key(&c);
    let wrong2 = || sdk(&python, port, &ca, &id2, &"x".repeat(40), &["denied"]);
    wait_for("the new key at the gateway", 15, || {
        (wrong2() == "SignatureDoesNotMatch").then_some(())
    });
    c.stop_meta();
    wait_for("the gateway noticing", 30, || {
        log(&gw_log)
            .contains("serving with the last ones")
            .then_some(())
    });
    assert_eq!(wrong2(), "SignatureDoesNotMatch");
}

/// A host that runs only a gateway (D233): it joins with an `s3` token,
/// its certificate names it and its address, later runs need no token,
/// and it serves the cluster's buckets. A token of another role doesn't
/// make a gateway; `--no-s3` takes the all-in-one's away, and its
/// default placement isn't set again once an administrator removed it.
#[test]
fn a_gateway_joins_with_an_s3_token() {
    let c = Cluster::all_in_one("s3join", 3);
    install(&c.root, &[]);
    let token = |role: &str, name: &str| {
        let r = c
            .ctl(&["token", "create", "--role", role, "--name", name])
            .ok();
        let f = c.scratch.path(&format!("{name}.token"));
        std::fs::write(&f, r.out.lines().next().unwrap()).unwrap();
        f
    };
    let host = c.scratch.path("gw-root");
    link_binaries(&host);
    let port = free_ports(1);
    let join_args = |f: &Path| -> Vec<String> {
        [
            "--join",
            &c.api,
            "--token-file",
            f.to_str().unwrap(),
            "--role",
            "s3",
            "--address",
            "127.0.0.1",
            "--s3-port",
            &port.to_string(),
        ]
        .map(String::from)
        .to_vec()
    };
    let run = |root: &Path, a: &[String]| {
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        install_cmd(root, "install", &a)
    };
    // A mgmt token doesn't make a gateway.
    let other = c.scratch.path("other-root");
    link_binaries(&other);
    let r = run(&other, &join_args(&token("mgmt", "not-s3"))).expect(1);
    assert!(r.has("isn't an s3 token"), "{}", r.out);
    // An s3 token does.
    let r = run(&host, &join_args(&token("s3", "gw2"))).ok();
    assert!(r.has("as gateway hyperstor://"), "{}", r.out);
    let cert = std::fs::read_to_string(host.join("etc/hyperstor/pki/s3.crt")).unwrap();
    let who = hyperstor_pki::identity_in(&cert).unwrap();
    assert_eq!(
        (who.role, who.id.as_str(), who.cluster.as_str()),
        (hyperstor_pki::Role::S3, "gw2", c.cluster_id.as_str())
    );
    let env = std::fs::read_to_string(host.join("etc/hyperstor/s3.env")).unwrap();
    assert!(
        env.contains(&format!("--api {} ", c.api))
            && env.contains(&format!("--meta {} ", c.map))
            && env.contains(&format!("--listen 0.0.0.0:{port}")),
        "{env}"
    );
    // No storage node or map service there; later runs need no token.
    assert!(!host.join("etc/hyperstor/meta.env").exists());
    assert!(!host.join("etc/hyperstor/node-0.env").exists());
    let r = run(&host, &[]).ok();
    assert!(r.has("Nothing to change"), "{}", r.out);
    // It serves, from what was installed there.
    let run_dir = c.scratch.path("gw-run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let gw_log = c.scratch.path("gw2.log");
    let _gw = Gateway(
        Command::new(host.join("usr/local/bin/hyperstor-s3"))
            .args(env_args(&host, &run_dir, "s3.env"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&gw_log).unwrap())
            .spawn()
            .unwrap(),
        gw_log.clone(),
    );
    let (id, secret) = new_key(&c);
    wait_for("the joined gateway's keys", 30, || {
        log(&gw_log)
            .contains("1 access keys, 1 placements")
            .then_some(())
    });
    if let Some(python) = sdk_python() {
        let ca = c.pki().join("root.crt");
        let step = |s: &[&str]| sdk(&python, port, &ca, &id, &secret, s);
        assert_eq!(step(&["create", "joined"]), "created");
        assert_eq!(step(&["roundtrip", "joined"]), "roundtrip ok");
    }
    // The default placement, once removed, stays removed.
    c.ctl(&["s3", "placement", "delete", "--name", "default"])
        .ok();
    install(&c.root, &[]);
    assert!(
        c.ctl(&["s3", "placement", "list"])
            .ok()
            .has("no S3 placements")
    );
    // --no-s3 takes the all-in-one's gateway away, and keeps it away.
    let r = install(&c.root, &["--no-s3"]);
    assert!(
        r.has("removed /etc/systemd/system/hyperstor-s3.service"),
        "{}",
        r.out
    );
    assert!(!c.root.join("etc/hyperstor/s3.env").exists());
    install(&c.root, &[]);
    assert!(!c.root.join("etc/hyperstor/s3.env").exists());
}
