//! The all-in-one install, end to end (spec/administration.md §23.4):
//! install into a scratch root, check that a second run changes nothing,
//! start the installed binaries with the installed configuration, and
//! manage the cluster with the installed `hyperstorctl`: health, an I/O
//! test, a node failure and its report, then uninstall.
//!
//! Needs the workspace's binaries beside the installer, as
//! `cargo test --workspace` builds them.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

struct Procs(Vec<Child>);

impl Drop for Procs {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn free_base(n: u16) -> u16 {
    // A run of free ports, from a pid-dependent start to avoid parallel runs.
    let mut p = 20000 + (std::process::id() % 2000) as u16 * 10;
    loop {
        if (0..n).all(|i| std::net::TcpListener::bind(("127.0.0.1", p + i)).is_ok()) {
            return p;
        }
        p += n + 1;
    }
}

fn install(root: &Path, extra: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_hyperstor-install"))
        .arg("install")
        .args(["--root", root.to_str().unwrap()])
        .args(extra)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "install failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// The arguments in an installed `.env` file, with configuration paths
/// moved under `root`.
fn args(root: &Path, env: &str) -> Vec<String> {
    let text = std::fs::read_to_string(root.join("etc/hyperstor").join(env)).unwrap();
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("HS_ARGS="))
        .unwrap();
    line.split_whitespace()
        .map(|a| {
            a.replace(
                "/etc/hyperstor",
                root.join("etc/hyperstor").to_str().unwrap(),
            )
        })
        .collect()
}

fn ctl(bin: &Path, api: &str, a: &[&str]) -> (i32, String) {
    // The installed certificates, under the scratch root: bin is
    // <root>/usr/local/bin.
    let pki = bin.join("../../../etc/hyperstor/pki");
    let out = Command::new(bin.join("hyperstorctl"))
        .args(["--api", api, "--tls", pki.to_str().unwrap()])
        .args(a)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> Option<String>) -> String {
    let t = Instant::now();
    let mut last = String::new();
    while t.elapsed() < Duration::from_secs(secs) {
        match f() {
            Some(s) => return s,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
        last = what.to_string();
    }
    panic!("timed out waiting for {last}");
}

#[test]
fn install_manage_and_fail_a_node() {
    let here = PathBuf::from(env!("CARGO_BIN_EXE_hyperstor-install"));
    for b in ["hyperstor-meta", "hyperstord", "hyperstorctl"] {
        assert!(
            here.with_file_name(b).exists(),
            "{b} isn't built beside the installer: run cargo test --workspace"
        );
    }
    let scratch =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("aio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let root = scratch.join("root");
    let data = scratch.join("data");
    // With --root, the data directory is made under the root like every
    // other path; the processes here use the final path, so make it.
    std::fs::create_dir_all(&data).unwrap();
    let base = free_base(8);
    let (api_port, map_port, node_port, admin_port) = (
        base.to_string(),
        (base + 1).to_string(),
        (base + 2).to_string(),
        (base + 5).to_string(),
    );

    let first = install(
        &root,
        &[
            "--all-in-one",
            "--nodes",
            "3",
            "--size-gib",
            "1",
            "--data-dir",
            data.to_str().unwrap(),
            "--api-port",
            &api_port,
            "--map-port",
            &map_port,
            "--node-port",
            &node_port,
            "--admin-port",
            &admin_port,
        ],
    );
    let text = String::from_utf8_lossy(&first.stdout);
    for f in [
        "/usr/local/bin/hyperstorctl",
        "/etc/hyperstor/cluster.txt",
        "/etc/hyperstor/node-2.env",
        "/etc/systemd/system/hyperstord@.service",
        "/etc/systemd/system/hyperstor-renew.timer",
        "/usr/local/bin/hyperstor-install",
        "/etc/hyperstor/pki/admin.crt",
        "/etc/hyperstor/install.conf",
    ] {
        assert!(text.contains(f), "{f} not reported as written:\n{text}");
    }
    // Convergent: a second run with no flags reuses the saved ones and
    // finds nothing to change.
    let again = install(&root, &[]);
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("Nothing to change"),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    let cid = std::fs::read_to_string(root.join("etc/hyperstor/cluster.txt")).unwrap();

    // Start what was installed, as the units would.
    let bin = root.join("usr/local/bin");
    let mut procs = Procs(Vec::new());
    procs.0.push(
        Command::new(bin.join("hyperstor-meta"))
            .args(args(&root, "meta.env"))
            .spawn()
            .unwrap(),
    );
    for n in 0..3 {
        procs.0.push(
            Command::new(bin.join("hyperstord"))
                .args(args(&root, &format!("node-{n}.env")))
                .args(["--spin-us", "0"])
                .spawn()
                .unwrap(),
        );
    }
    let api = format!("127.0.0.1:{api_port}");

    // --wait polls until health is ok: every node heard from (I43).
    let (code, status) = ctl(&bin, &api, &["status", "--json", "--wait", "20"]);
    assert_eq!(code, 0, "{status}");
    assert!(status.contains("\"health\":\"HEALTH_OK\""), "{status}");
    assert_eq!(status.matches("\"last_heard_ms\"").count(), 3, "{status}");
    assert!(
        status.contains(&format!("\"cluster_id\":\"{}\"", cid_of(&cid))),
        "{status}"
    );
    assert_eq!(status.matches("\"up\":true").count(), 3, "{status}");

    // Identities (D56): the API takes only certificates of this cluster's
    // root, a storage node's may read but not change, and plain HTTP
    // gets nowhere.
    let pki = root.join("etc/hyperstor/pki");
    let read = |f: &str| std::fs::read_to_string(pki.join(f)).unwrap();
    let ca = hyperstor_pki::Pem {
        cert: read("intermediate.crt"),
        key: read("intermediate.key"),
    };
    let as_role =
        |name: &str, role: hyperstor_pki::Role, root_pem: &str, ca: &hyperstor_pki::Pem| {
            let dir = scratch.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let who = hyperstor_pki::Identity {
                cluster: cid_of(&cid).to_string(),
                role,
                id: name.into(),
            };
            let p = hyperstor_pki::issue(ca, &who, &[], &[], 1).unwrap();
            std::fs::write(dir.join("admin.crt"), p.cert).unwrap();
            std::fs::write(dir.join("admin.key"), p.key).unwrap();
            std::fs::write(dir.join("root.crt"), root_pem).unwrap();
            dir
        };
    let run = |tls: Option<&Path>, a: &[&str]| {
        let mut c = Command::new(bin.join("hyperstorctl"));
        c.args(["--api", &api]);
        if let Some(t) = tls {
            c.args(["--tls", t.to_str().unwrap()]);
        }
        let out = c.args(a).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr),
        )
    };
    let node = as_role("node7", hyperstor_pki::Role::Node, &read("root.crt"), &ca);
    let (code, out) = run(Some(&node), &["status"]);
    assert_eq!(code, 0, "a node may read: {out}");
    let (code, out) = run(Some(&node), &["out", "--device", "0"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("may not change the cluster"), "{out}");
    let host = as_role("host1", hyperstor_pki::Role::Host, &read("root.crt"), &ca);
    let (code, out) = run(Some(&host), &["status"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("may not read"), "{out}");
    // A certificate from another cluster's CA fails the handshake.
    let other_root = hyperstor_pki::new_root("1").unwrap();
    let other_ca = hyperstor_pki::new_intermediate(&other_root, "1").unwrap();
    let stranger = as_role(
        "stranger",
        hyperstor_pki::Role::Admin,
        &read("root.crt"),
        &other_ca,
    );
    let (code, out) = run(Some(&stranger), &["status"]);
    assert_ne!(code, 0, "{out}");
    let (code, out) = run(None, &["status"]);
    assert_ne!(code, 0, "plain HTTP to a TLS API: {out}");

    let (code, out) = ctl(&bin, &api, &["io-test", "--mib", "8"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("read and verified"), "{out}");
    // Each storage node's admin port counts what it served (D58, D59).
    let metrics = |n: u16| {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", base + 5 + n)).unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut t = String::new();
        s.read_to_string(&mut t).unwrap();
        t
    };
    let counted: u64 = (0..3)
        .map(|n| {
            let t = metrics(n);
            assert!(t.starts_with("HTTP/1.1 200 OK"), "{t}");
            t.lines()
                .filter(|l| {
                    l.starts_with("hyperstor_request_bytes_total{") && l.contains("kind=\"write\"")
                })
                .map(|l| l.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
                .sum::<u64>()
        })
        .sum();
    // 8 MiB written as client writes to primaries (plus whatever the
    // waits above didn't write: nothing).
    assert_eq!(counted, 8 << 20, "client write bytes counted by primaries");

    let (code, out) = ctl(&bin, &api, &["pools"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("replicated") && out.contains("cache"), "{out}");
    let (code, out) = ctl(&bin, &api, &["io-test", "--pool", "2", "--mib", "4"]);
    assert_eq!(code, 0, "the cache pool takes I/O: {out}");

    // Take node 2's device out: its copies move to... nowhere, since
    // three copies need three hosts, so PGs run degraded (warn); in again
    // brings them back to clean after backfill.
    let (code, out) = ctl(&bin, &api, &["out", "--node", "2"]);
    assert_eq!(code, 0, "{out}");
    let degraded = wait_for("PGs degraded after out", 20, || {
        let (code, out) = ctl(&bin, &api, &["status"]);
        (code == 1 && out.contains("device(s) down or out")).then_some(out)
    });
    assert!(degraded.contains("PG(s)"), "{degraded}");
    let (code, out) = ctl(&bin, &api, &["io-test", "--mib", "4"]);
    assert_eq!(code, 0, "writes go on with two copies: {out}");
    let (code, out) = ctl(&bin, &api, &["in", "--node", "2"]);
    assert_eq!(code, 0, "{out}");
    wait_for("health ok after in", 30, || {
        let (code, out) = ctl(&bin, &api, &["status"]);
        (code == 0).then_some(out)
    });
    let (code, out) = ctl(&bin, &api, &["out", "--device", "999999"]);
    assert_eq!(code, 1);
    assert!(out.contains("no device 999999"), "{out}");

    // Fail storage node 2: the map service marks it down within about a
    // second (D48), status says so and exits 1, and I/O goes on with two
    // copies (min_size 2).
    let mut n2 = procs.0.remove(3);
    n2.kill().unwrap();
    n2.wait().unwrap();
    let warn = wait_for("node 2 reported down", 20, || {
        let (code, out) = ctl(&bin, &api, &["status"]);
        (code == 1 && out.contains("node(s) down: 2")).then_some(out)
    });
    assert!(warn.contains("health  warn"), "{warn}");
    let (code, out) = ctl(&bin, &api, &["io-test", "--mib", "4"]);
    assert_eq!(code, 0, "{out}");

    // An unreachable map service is exit status 3, not a hang.
    let dead = format!("127.0.0.1:{}", free_base(1));
    let (code, _) = ctl(&bin, &dead, &["status"]);
    assert_eq!(code, 3);

    drop(procs);
    let out = Command::new(env!("CARGO_BIN_EXE_hyperstor-install"))
        .args(["uninstall", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!bin.join("hyperstorctl").exists());
    assert!(!bin.join("hyperstor-install").exists());
    assert!(
        !root
            .join("etc/systemd/system/hyperstor-renew.timer")
            .exists()
    );
    assert!(
        root.join("etc/hyperstor/cluster.txt").exists(),
        "configuration is kept"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

fn cid_of(cluster: &str) -> u64 {
    cluster
        .lines()
        .find_map(|l| l.strip_prefix("cluster ")?.trim().parse().ok())
        .unwrap()
}

/// `check` reports every check on its own line, with a level, and
/// changes nothing. Its verdicts depend on the host, so only the shape
/// is checked here.
#[test]
fn check_reports_every_item() {
    let scratch =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("check-{}", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_hyperstor-install"))
        .args(["check", "--data-dir", scratch.to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for item in [
        "kernel", "io_uring", "memlock", "cpus", "nvme", "clock", "disk", "ports", "systemd",
    ] {
        let line = text
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some(item))
            .unwrap_or_else(|| panic!("no {item} line:\n{text}"));
        let level = line.split_whitespace().next().unwrap();
        assert!(["ok", "info", "warn", "FAIL"].contains(&level), "{line}");
    }
    // Exit status 1 exactly when something failed.
    assert_eq!(
        out.status.code() == Some(1),
        text.contains("FAIL  "),
        "{text}"
    );
    assert!(!scratch.exists(), "check must not create anything");
}
