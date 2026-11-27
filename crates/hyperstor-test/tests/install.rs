//! Installing (19-administration.md §23.4): host checks, the all-in-one
//! install and its convergence, uninstall, and a cluster across hosts
//! from one cluster file.

use hyperstor_test::*;
use std::process::{Child, Command};

/// One at a time: the installer's key work (certificates, the root key's
/// backup) starves a live cluster on a small machine, whose nodes then miss
/// their pings, and CI saw the vhost test's cluster time out.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// A process killed when the test ends, however it ends: one left running
/// by a failed test holds the suite's output open, and CI waits for it.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `check` reports every check on its own line with a level, and changes
/// nothing. Verdicts depend on the host, so only the shape is checked.
#[test]
fn check_reports_every_item() {
    let _one = one_at_a_time();
    let s = Scratch::new("check");
    let data = s.path("never-made");
    let out = Command::new(bin_dir().join("hyperstor-install"))
        .args(["check", "--data-dir", data.to_str().unwrap()])
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
    assert_eq!(
        out.status.code() == Some(1),
        text.contains("FAIL  "),
        "{text}"
    );
    assert!(!data.exists(), "check must not create anything");

    // --json (#416): the same checks by name, each with its level, the
    // worst level, and the same exit status.
    let out = Command::new(bin_dir().join("hyperstor-install"))
        .args(["check", "--json", "--data-dir", data.to_str().unwrap()])
        .output()
        .unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}:\n{json}"));
    let checks = report["checks"].as_array().expect("checks");
    let levels = ["ok", "info", "warn", "fail"];
    for c in checks {
        assert!(levels.contains(&c["level"].as_str().unwrap()), "{c}");
        assert!(c["detail"].is_string(), "{c}");
    }
    for item in [
        "kernel", "io_uring", "memlock", "cpus", "nvme", "clock", "disk", "ports", "systemd",
    ] {
        assert!(
            checks.iter().any(|c| c["name"] == item),
            "no {item}:\n{json}"
        );
    }
    let worst = checks
        .iter()
        .map(|c| levels.iter().position(|l| c["level"] == *l).unwrap())
        .max()
        .map_or("ok", |i| levels[i]);
    assert_eq!(report["worst"], worst, "{json}");
    assert_eq!(out.status.code() == Some(1), worst == "fail", "{json}");
    assert!(!data.exists(), "check must not create anything");

    // --json is check's only.
    let out = Command::new(bin_dir().join("hyperstor-install"))
        .args([
            "uninstall",
            "--json",
            "--root",
            s.path("root").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

/// A first install writes everything; a second with no flags reuses the
/// saved ones and changes nothing; a changed flag rewrites only what
/// depends on it; uninstall removes services and binaries, keeps
/// configuration.
#[test]
fn install_is_convergent_and_uninstall_keeps_configuration() {
    let _one = one_at_a_time();
    let s = Scratch::new("converge");
    let root = s.path("root");
    let first = install(&root, &["--all-in-one", "--nodes", "2", "--size-gib", "1"]);
    for f in [
        "/usr/local/bin/hyperstorctl",
        "/usr/local/bin/hyperstor-install",
        "/etc/hyperstor/cluster.txt",
        "/etc/hyperstor/node-1.env",
        "/etc/hyperstor/pki/admin.crt",
        "/etc/systemd/system/hyperstord@.service",
        "/etc/systemd/system/hyperstor-renew.timer",
        "/etc/hyperstor/install.conf",
    ] {
        assert!(first.has(f), "{f} not reported as written:\n{}", first.out);
    }
    let cluster = std::fs::read_to_string(root.join("etc/hyperstor/cluster.txt")).unwrap();
    assert!(install(&root, &[]).has("Nothing to change"));
    // A bigger device file: only the storage nodes' options change; the
    // cluster id and certificates stay.
    let again = install(&root, &["--size-gib", "2"]);
    assert!(
        again.has("/etc/hyperstor/node-0.env") && again.has("/etc/hyperstor/node-1.env"),
        "{}",
        again.out
    );
    assert!(
        !again.has("cluster.txt") && !again.has("pki"),
        "{}",
        again.out
    );
    assert_eq!(
        std::fs::read_to_string(root.join("etc/hyperstor/cluster.txt")).unwrap(),
        cluster
    );
    assert!(
        again.has("would restart hyperstord@0.service hyperstord@1.service\n"),
        "{}",
        again.out
    );
    // A dashboard change restarts only the dashboard.
    let dash = install(&root, &["--mgmt-port", "18852"]);
    assert!(
        dash.has("would restart hyperstor-mgmt.service\n"),
        "{}",
        dash.out
    );

    install_cmd(&root, "uninstall", &[]).ok();
    assert!(!root.join("usr/local/bin/hyperstorctl").exists());
    assert!(!root.join("usr/local/bin/hyperstor-install").exists());
    assert!(
        !root
            .join("etc/systemd/system/hyperstor-renew.timer")
            .exists()
    );
    assert!(
        root.join("etc/hyperstor/cluster.txt").exists(),
        "configuration is kept"
    );
}

/// Keys are readable by their owner (and the group that needs them)
/// only, the pki directory lists nothing to others, nothing is left
/// beside, and the daily renewal runs with a closed umask. A pki
/// directory an earlier install made 0755 is narrowed on the next run.
#[test]
fn key_files_and_the_pki_directory_have_their_final_modes() {
    use std::os::unix::fs::PermissionsExt;
    let _one = one_at_a_time();
    let s = Scratch::new("key-modes");
    let root = s.path("root");
    install(&root, &["--all-in-one", "--nodes", "1", "--size-gib", "1"]);
    let pki = root.join("etc/hyperstor/pki");
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let check = || {
        assert_eq!(mode(&pki) & 0o007, 0o001, "pki directory {:o}", mode(&pki));
        let mut keys = 0;
        for e in std::fs::read_dir(&pki).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".hs-new"), "{name} left beside");
            if name.ends_with(".key") {
                keys += 1;
                let m = mode(&e.path());
                assert!(m == 0o600 || m == 0o640, "{name} is {m:o}");
            }
        }
        assert!(keys >= 4, "{keys} keys");
    };
    check();
    let renew =
        std::fs::read_to_string(root.join("etc/systemd/system/hyperstor-renew.service")).unwrap();
    assert!(renew.contains("\nUMask=0077\n"), "{renew}");
    std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o755)).unwrap();
    install(&root, &[]);
    check();
}

/// Bad input is refused with a usage message, and nothing is written.
#[test]
fn bad_flags_are_refused() {
    let _one = one_at_a_time();
    let s = Scratch::new("badflags");
    let root = s.path("root");
    install_cmd(&root, "install", &["--nodes", "0"]).expect(2);
    install_cmd(&root, "install", &["--role", "king"]).expect(2);
    let r = install_cmd(&root, "install", &[]).expect(1);
    assert!(r.has("--all-in-one"), "{}", r.out);
    let r = install_cmd(
        &root,
        "install",
        &["--role", "storage", "--map", "127.0.0.1:1"],
    )
    .expect(1);
    assert!(r.has("--cluster FILE"), "{}", r.out);
    assert!(!root.join("etc").exists());
}

/// An installer beside an incomplete set of binaries refuses before
/// writing any: a host is never left with some binaries new and its
/// services still running the old ones (I293).
#[test]
fn a_missing_binary_installs_nothing() {
    let _one = one_at_a_time();
    let s = Scratch::new("missingbin");
    let (dir, root) = (s.path("bin"), s.path("root"));
    std::fs::create_dir_all(&dir).unwrap();
    // Everything the installer installs but hyperstor-smart.
    for b in [
        "hyperstor-install",
        "hyperstor-meta",
        "hyperstord",
        "hyperstorctl",
        "hsfs",
        "hyperstor-mgmt",
        "hyperstor-authd",
        "hyperstor-vhost",
        "hyperstor-s3",
    ] {
        if std::fs::hard_link(bin_dir().join(b), dir.join(b)).is_err() {
            std::fs::copy(bin_dir().join(b), dir.join(b)).unwrap();
        }
    }
    let r = Run::of(
        Command::new(dir.join("hyperstor-install"))
            .args(["install", "--root", root.to_str().unwrap()])
            .args(["--all-in-one", "--nodes", "1", "--size-gib", "1"])
            .output()
            .unwrap(),
    )
    .expect(1);
    assert!(
        r.has("hyperstor-smart") && r.has("nothing was installed"),
        "{}",
        r.out
    );
    assert!(!root.join("usr").exists() && !root.join("etc").exists());
}

/// A cluster across "hosts" from one cluster file (implementation-notes
/// I50): the map service on one, a storage node on each of three others,
/// each installed into a root of its own and started from what was
/// installed; then managed from the map service's host.
#[test]
fn cluster_file_install_across_hosts() {
    let _one = one_at_a_time();
    let s = Scratch::new("hosts");
    let base = free_ports(8);
    let file = s.path("cluster.txt");
    std::fs::write(
        &file,
        format!(
            "cluster 7\nnode 0 rack 0 127.0.0.1:{}\nnode 1 rack 0 127.0.0.1:{}\nnode 2 rack 0 127.0.0.1:{}\n\
             pool 1 vol replicated size 3 min 2 pgs 32 domain host\n",
            base + 2,
            base + 3,
            base + 4
        ),
    )
    .unwrap();
    let map = format!("127.0.0.1:{}", base + 1);
    let common = [
        "--cluster",
        file.to_str().unwrap(),
        "--map",
        map.as_str(),
        "--size-gib",
        "1",
    ];
    let meta_root = s.path("meta-host");
    let api_port = base.to_string();
    let meta_data = s.path("meta-data");
    std::fs::create_dir_all(&meta_data).unwrap();
    let mut a = common.to_vec();
    a.extend([
        "--role",
        "meta",
        "--api-port",
        &api_port,
        "--data-dir",
        meta_data.to_str().unwrap(),
    ]);
    install(&meta_root, &a);
    assert!(!meta_root.join("etc/hyperstor/node-0.env").exists());
    let bin = meta_root.join("usr/local/bin");
    let mut procs = vec![
        Command::new(bin.join("hyperstor-meta"))
            .args(env_args(&meta_root, s.run(), "meta.env"))
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ];
    for n in 0..3u16 {
        let root = s.path(&format!("host-{n}"));
        let data = s.path(&format!("data-{n}"));
        std::fs::create_dir_all(&data).unwrap();
        let (node, admin) = (n.to_string(), (base + 5 + n).to_string());
        let mut a = common.to_vec();
        a.extend([
            "--role",
            "storage",
            "--node",
            &node,
            "--data-dir",
            data.to_str().unwrap(),
            "--admin-port",
            &admin,
        ]);
        install(&root, &a);
        assert!(!root.join("etc/hyperstor/meta.env").exists());
        assert!(
            !root.join("etc/hyperstor/pki").exists(),
            "no CA on storage hosts"
        );
        procs.push(
            Command::new(root.join("usr/local/bin/hyperstord"))
                .args(env_args(&root, s.run(), &format!("node-{n}.env")))
                .args(NODE_ARGS)
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    let api = format!("127.0.0.1:{api_port}");
    let pki = meta_root.join("etc/hyperstor/pki");
    let ctl = |args: &[&str]| {
        let o = Command::new(bin.join("hyperstorctl"))
            .env("HYPERSTOR_TIMEOUT", CTL_TIMEOUT)
            .args(["--api", &api, "--tls", pki.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
        )
    };
    let (code, out) = ctl(&["status", "--wait", "30"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("cluster 7,"), "{out}");
    let (code, out) = ctl(&["io-test", "--mib", "8"]);
    assert_eq!(code, Some(0), "{out}");
    for p in &mut procs {
        let _ = p.kill();
        let _ = p.wait();
    }
}

/// Endpoints from the cluster file (26-networks.md §29.5, D207): each
/// storage host's installer gives `hyperstord` its devices' endpoints on
/// network `fast` (a second loopback address here) and the network, and
/// the nodes replicate to each other there. `hyperstorctl io-test
/// --networks fast` reaches them there too (D340).
#[test]
fn storage_nodes_replicate_over_endpoints_from_the_cluster_file() {
    let _one = one_at_a_time();
    let s = Scratch::new("endpoints");
    let base = free_ports(11);
    let file = s.path("cluster.txt");
    let ep = |n: u16| format!("127.0.0.2:{}", base + 8 + n);
    std::fs::write(
        &file,
        format!(
            "cluster 8\nnetwork fast data trusted subnet 127.0.0.2/32\n\
             node 0 rack 0 127.0.0.1:{}\nnode 1 rack 0 127.0.0.1:{}\nnode 2 rack 0 127.0.0.1:{}\n\
             endpoint 0 fast {}\nendpoint 1000 fast {}\nendpoint 2000 fast {}\n\
             pool 1 vol replicated size 3 min 2 pgs 32 domain host\n",
            base + 2,
            base + 3,
            base + 4,
            ep(0),
            ep(1),
            ep(2)
        ),
    )
    .unwrap();
    let map = format!("127.0.0.1:{}", base + 1);
    let common = [
        "--cluster",
        file.to_str().unwrap(),
        "--map",
        map.as_str(),
        "--size-gib",
        "1",
    ];
    let meta_root = s.path("meta-host");
    let api_port = base.to_string();
    let meta_data = s.path("meta-data");
    std::fs::create_dir_all(&meta_data).unwrap();
    let mut a = common.to_vec();
    a.extend([
        "--role",
        "meta",
        "--api-port",
        &api_port,
        "--data-dir",
        meta_data.to_str().unwrap(),
    ]);
    install(&meta_root, &a);
    let bin = meta_root.join("usr/local/bin");
    let mut procs = vec![
        Command::new(bin.join("hyperstor-meta"))
            .args(env_args(&meta_root, s.run(), "meta.env"))
            // Kept for a failure's diagnosis (#330's io-test mismatch).
            .stderr(std::fs::File::create(s.path("meta.log")).unwrap())
            .spawn()
            .unwrap(),
    ];
    for n in 0..3u16 {
        let root = s.path(&format!("host-{n}"));
        let data = s.path(&format!("data-{n}"));
        std::fs::create_dir_all(&data).unwrap();
        let (node, admin) = (n.to_string(), (base + 5 + n).to_string());
        let mut a = common.to_vec();
        a.extend([
            "--role",
            "storage",
            "--node",
            &node,
            "--data-dir",
            data.to_str().unwrap(),
            "--admin-port",
            &admin,
        ]);
        install(&root, &a);
        let env =
            std::fs::read_to_string(root.join(format!("etc/hyperstor/node-{n}.env"))).unwrap();
        assert!(
            env.contains(&format!("--endpoint {}={}", n as u32 * 1000, ep(n)))
                && env.contains("--networks fast"),
            "{env}"
        );
        procs.push(
            Command::new(root.join("usr/local/bin/hyperstord"))
                .args(env_args(&root, s.run(), &format!("node-{n}.env")))
                .args(NODE_ARGS)
                .stderr(std::fs::File::create(s.path(&format!("node-{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        );
    }
    let api = format!("127.0.0.1:{api_port}");
    let pki = meta_root.join("etc/hyperstor/pki");
    let ctl = |args: &[&str]| {
        let o = Command::new(bin.join("hyperstorctl"))
            .env("HYPERSTOR_TIMEOUT", CTL_TIMEOUT)
            .args(["--api", &api, "--tls", pki.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
        )
    };
    // A hyperstorctl run, and the endpoints on network `fast` it held
    // connections to while it ran.
    let ctl_sockets = |args: &[&str]| {
        let mut child = Command::new(bin.join("hyperstorctl"))
            .env("HYPERSTOR_TIMEOUT", CTL_TIMEOUT)
            .args(["--api", &api, "--tls", pki.to_str().unwrap()])
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut used = std::collections::BTreeSet::new();
        while child.try_wait().unwrap().is_none() {
            used.extend(remote_ports_of(child.id(), [127, 0, 0, 2]));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let o = child.wait_with_output().unwrap();
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
            used,
        )
    };
    let (code, out) = ctl(&["status", "--wait", "30"]);
    assert_eq!(code, Some(0), "{out}");
    // At the devices' addresses (and whatever the host's own
    // hyperstorctl.conf says, kept out by `none`).
    let (code, out, used) = ctl_sockets(&["io-test", "--mib", "64", "--networks", "none"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("at the devices' addresses"), "{out}");
    assert!(used.is_empty(), "{used:?}");
    // Sessions to every node's endpoint, from the primaries replicating:
    // 16 chunks over 32 PGs, so every node is a replica of some.
    let want: std::collections::BTreeSet<u16> = (0..3).map(|n| base + 8 + n).collect();
    let seen = established_on([127, 0, 0, 2]);
    // D340: hyperstorctl on network `fast` reaches every node at its
    // endpoint there, the primaries of the PGs it writes.
    let (code, out, used) = ctl_sockets(&["io-test", "--mib", "64", "--networks", "fast"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("on network fast"), "{out}");
    // A network the cluster doesn't have: warned, the addresses used.
    let (code, out, _) = ctl_sockets(&["io-test", "--mib", "1", "--networks", "ib"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("the cluster has no network ib"), "{out}");
    for p in &mut procs {
        let _ = p.kill();
        let _ = p.wait();
    }
    assert!(want.is_subset(&seen), "{want:?} not all in {seen:?}");
    assert!(
        want.is_subset(&used),
        "hyperstorctl's endpoints {used:?}, not all of {want:?}"
    );
}

/// The remote ports on `ip` (IPv4) of process `pid`'s TCP connections:
/// its sockets' inodes, found in /proc/net/tcp.
fn remote_ports_of(pid: u32, ip: [u8; 4]) -> std::collections::BTreeSet<u16> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Default::default();
    };
    let inodes: std::collections::BTreeSet<String> = fds
        .filter_map(|f| std::fs::read_link(f.ok()?.path()).ok())
        .filter_map(|l| {
            let l = l.to_string_lossy().into_owned();
            Some(l.strip_prefix("socket:[")?.strip_suffix(']')?.to_string())
        })
        .collect();
    let want = format!("{:08X}", u32::from_le_bytes(ip));
    std::fs::read_to_string("/proc/net/tcp")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (addr, port) = f.get(2)?.split_once(':')?;
            (addr == want && inodes.contains(*f.get(9)?))
                .then(|| u16::from_str_radix(port, 16).ok())?
        })
        .collect()
}

/// The local ports of established TCP connections on `ip` (IPv4), from
/// /proc/net/tcp.
fn established_on(ip: [u8; 4]) -> std::collections::BTreeSet<u16> {
    let want = format!("{:08X}", u32::from_le_bytes(ip));
    std::fs::read_to_string("/proc/net/tcp")
        .unwrap()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (addr, port) = f.get(1)?.split_once(':')?;
            // 01: ESTABLISHED.
            (addr == want && f.get(3) == Some(&"01")).then(|| u16::from_str_radix(port, 16).ok())?
        })
        .collect()
}

/// The root CA's key (D84): an install says it has no backup; a backup
/// is encrypted, opens only with its passphrase, may take the key off the
/// host (later runs still converge), and restores only onto its own
/// cluster. The cluster key (D162) is made with the root, handed to the
/// map service as a credential, and goes in the backup with it.
#[test]
fn root_key_backup_and_restore() {
    let _one = one_at_a_time();
    let s = Scratch::new("rootbackup");
    let root = s.path("root");
    let r = install(&root, &["--all-in-one", "--nodes", "1", "--size-gib", "1"]);
    assert!(r.has("Back up the root CA's key"), "{}", r.out);
    let check = install_cmd(&root, "check", &[]);
    assert!(
        check
            .out
            .lines()
            .any(|l| l.starts_with("warn") && l.contains("rootkey")),
        "{}",
        check.out
    );
    let pki = root.join("etc/hyperstor/pki");
    let key = std::fs::read_to_string(pki.join("root.key")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let cluster_key = std::fs::read_to_string(pki.join("cluster.key")).unwrap();
    assert!(cluster_key.starts_with("hyperstor cluster key v1\n"));
    let mode = std::fs::metadata(pki.join("cluster.key"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "readable by root only");
    let unit =
        std::fs::read_to_string(root.join("etc/systemd/system/hyperstor-meta.service")).unwrap();
    assert!(
        unit.contains("LoadCredential=cluster.key:/etc/hyperstor/pki/cluster.key"),
        "{unit}"
    );
    let pass = s.path("pass");
    std::fs::write(&pass, "short\n").unwrap();
    let bak = s.path("root.bak");
    let args = |extra: &[&str]| -> Vec<String> {
        let mut a = vec!["--out".into(), bak.to_str().unwrap().into()];
        a.extend(["--passphrase-file".into(), pass.to_str().unwrap().into()]);
        a.extend(extra.iter().map(|x| x.to_string()));
        a
    };
    let run = |cmd: &str, a: &[String]| {
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        install_cmd(&root, cmd, &a)
    };
    let r = run("root-backup", &args(&[])).expect(1);
    assert!(r.has("at least 12 characters"), "{}", r.out);
    std::fs::write(&pass, "a long enough passphrase\n").unwrap();
    let r = run("root-backup", &args(&["--remove-key"])).ok();
    assert!(
        r.has("the root CA and the cluster key, encrypted"),
        "{}",
        r.out
    );
    let mode = std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = std::fs::read_to_string(&bak).unwrap();
    assert!(!text.contains("PRIVATE KEY") && text.starts_with("hyperstor root backup v1"));
    let hex_key = cluster_key.lines().nth(1).unwrap();
    assert!(!text.contains(hex_key), "the cluster key is encrypted");
    assert!(!pki.join("root.key").exists(), "taken off the host");
    // Without the key, installs still converge, and stop asking.
    let again = install(&root, &[]);
    assert!(
        again.has("Nothing to change") && !again.has("Back up"),
        "{}",
        again.out
    );
    let check = install_cmd(&root, "check", &[]);
    assert!(
        check
            .out
            .lines()
            .any(|l| l.starts_with("ok") && l.contains("rootkey")),
        "{}",
        check.out
    );
    // No overwrite; a wrong passphrase opens nothing.
    assert!(run("root-backup", &args(&[])).code == 1);
    let restore = |pass_text: &str, onto: &std::path::Path| {
        std::fs::write(&pass, pass_text).unwrap();
        install_cmd(
            onto,
            "root-restore",
            &[
                "--in",
                bak.to_str().unwrap(),
                "--passphrase-file",
                pass.to_str().unwrap(),
            ],
        )
    };
    let r = restore("not the passphrase!!", &root).expect(1);
    assert!(r.has("wrong passphrase"), "{}", r.out);
    // Not onto another cluster's host.
    let other = s.path("other");
    install(&other, &["--all-in-one", "--nodes", "1", "--size-gib", "1"]);
    let r = restore("a long enough passphrase", &other).expect(1);
    assert!(r.has("another cluster's"), "{}", r.out);
    // Not onto a host with another cluster key: the secrets it sealed
    // would no longer open.
    std::fs::write(pki.join("cluster.key"), "hyperstor cluster key v1\n").unwrap();
    let r = restore("a long enough passphrase", &root).expect(1);
    assert!(r.has("another cluster key"), "{}", r.out);
    // Lost, it comes back with the root.
    std::fs::remove_file(pki.join("cluster.key")).unwrap();
    restore("a long enough passphrase", &root).ok();
    assert_eq!(std::fs::read_to_string(pki.join("root.key")).unwrap(), key);
    assert_eq!(
        std::fs::read_to_string(pki.join("cluster.key")).unwrap(),
        cluster_key
    );
    // --out belongs to the backup commands only.
    install_cmd(&root, "install", &["--out", "x"]).expect(2);
}

/// The web UI's files (D100): installed from `--ui` with mgmt told to
/// serve them, kept by later runs without it, made exactly the new
/// build's by a run with a new one, and removed by uninstall.
#[test]
fn the_web_ui_is_installed_as_files() {
    let _one = one_at_a_time();
    let s = Scratch::new("ui-files");
    let root = s.path("root");
    let build = |files: &[(&str, &str)]| {
        let d = s.path(&format!("ui-{}", files.len()));
        for (f, body) in files {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        d
    };
    let ui = root.join("usr/local/share/hyperstor/ui");
    let first = build(&[("index.html", "<html>1"), ("assets/a-1.js", "1")]);
    let r = install(
        &root,
        &[
            "--all-in-one",
            "--nodes",
            "1",
            "--size-gib",
            "1",
            "--ui",
            first.to_str().unwrap(),
        ],
    );
    assert!(r.has("/usr/local/share/hyperstor/ui"), "{}", r.out);
    assert!(ui.join("assets/a-1.js").is_file());
    let env = std::fs::read_to_string(root.join("etc/hyperstor/mgmt.env")).unwrap();
    assert!(env.contains("--ui /usr/local/share/hyperstor/ui"), "{env}");
    assert!(install(&root, &[]).has("Nothing to change"));
    let second = build(&[
        ("index.html", "<html>2"),
        ("assets/a-2.js", "2"),
        ("favicon.svg", "<svg/>"),
    ]);
    let r = install(&root, &["--ui", second.to_str().unwrap()]);
    assert!(r.has("/usr/local/share/hyperstor/ui"), "{}", r.out);
    assert!(
        !ui.join("assets/a-1.js").exists(),
        "the old build's files go"
    );
    assert_eq!(
        std::fs::read_to_string(ui.join("index.html")).unwrap(),
        "<html>2"
    );
    // A build without index.html isn't one.
    let broken = build(&[("x.js", "")]);
    let out = Command::new(bin_dir().join("hyperstor-install"))
        .args([
            "install",
            "--root",
            root.to_str().unwrap(),
            "--ui",
            broken.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = install_cmd(&root, "uninstall", &[]);
    assert!(r.has("removed /usr/local/share/hyperstor/ui"), "{}", r.out);
    assert!(!ui.exists());
}

/// Where people reach mgmt (D108–D110): every address over HTTPS with a
/// web certificate from the cluster CA by default; the operator's own
/// certificate directory instead; loopback on request. The Tailscale
/// certificate is granted by a marked tailscaled drop-in, refused when
/// someone else holds the grant, and removed when turned off.
#[test]
fn mgmt_listens_on_every_address_with_a_cluster_certificate() {
    let _one = one_at_a_time();
    let s = Scratch::new("web-certs");
    let root = s.path("root");
    let env = || std::fs::read_to_string(root.join("etc/hyperstor/mgmt.env")).unwrap();
    install(&root, &["--all-in-one", "--nodes", "1", "--size-gib", "1"]);
    let e = env();
    assert!(
        e.contains("--listen 0.0.0.0:8852,[::]:8852 --web-tls /etc/hyperstor/pki"),
        "{e}"
    );
    let web = std::fs::read_to_string(root.join("etc/hyperstor/pki/web.crt")).unwrap();
    let (ips, names) = hyperstor_pki::names_of(&web).unwrap();
    // No certificate data in the failure message.
    assert!(names.contains(&"localhost".to_string()), "no localhost");
    assert!(ips.contains(&"127.0.0.1".parse().unwrap()), "no 127.0.0.1");
    assert!(install(&root, &[]).has("Nothing to change"));

    // The operator's own certificate, kept current by their ACME client.
    install(&root, &["--web-tls", "/etc/letsencrypt-hyperstor"]);
    assert!(
        env().contains("--web-tls /etc/letsencrypt-hyperstor"),
        "{}",
        env()
    );
    install(&root, &["--mgmt-listen", "loopback"]);
    let e = env();
    assert!(
        e.contains("--listen 127.0.0.1:8852 --api") && !e.contains("--web-tls"),
        "{e}"
    );
    install(&root, &["--mgmt-listen", "all", "--web-tls", ""]);

    // Tailscale: nobody holds the grant, so ours is written.
    let dropin = root.join("etc/systemd/system/tailscaled.service.d/50-hyperstor-cert.conf");
    let r = install(&root, &["--tailscale-cert"]);
    assert!(r.has("would restart tailscaled.service"), "{}", r.out);
    assert!(
        std::fs::read_to_string(&dropin)
            .unwrap()
            .contains("TS_PERMIT_CERT_UID=hyperstor")
    );
    assert!(
        env().contains("--tailscale /run/tailscale/tailscaled.sock"),
        "{}",
        env()
    );
    assert!(
        install(&root, &[]).has("Nothing to change"),
        "the choice is saved"
    );
    // Off: our drop-in goes.
    let r = install(&root, &["--no-tailscale-cert"]);
    assert!(r.has("would restart tailscaled.service"), "{}", r.out);
    assert!(!dropin.exists());
    assert!(!env().contains("--tailscale"), "{}", env());
    // Someone else holds it (glidex-ui): left alone, and not served.
    let glidex = root.join("etc/systemd/system/tailscaled.service.d/50-glidex-cert.conf");
    std::fs::write(
        &glidex,
        "[Service]\nEnvironment=TS_PERMIT_CERT_UID=glidex-ui\n",
    )
    .unwrap();
    let out = std::process::Command::new(bin_dir().join("hyperstor-install"))
        .args([
            "install",
            "--root",
            root.to_str().unwrap(),
            "--tailscale-cert",
        ])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("lets only glidex-ui"), "{err}");
    assert!(!dropin.exists() && !env().contains("--tailscale"));
    assert!(
        std::fs::read_to_string(&glidex)
            .unwrap()
            .contains("glidex-ui")
    );
}

/// `--vhost` (D142) installs `hyperstor-vhost` for the users named (D144);
/// started from what was installed, it serves a volume. `--no-vhost`
/// removes it again; `--role vhost` needs the map service's addresses, and
/// an unknown user is refused.
#[test]
fn hyperstor_vhost_is_installed_for_its_users() {
    let _one = one_at_a_time();
    let c = Cluster::all_in_one("inst-vhost", 3);
    let out = Command::new("id").arg("-un").output().unwrap();
    let me = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let r = install(&c.root, &["--vhost", "--vhost-user", &me]);
    for f in [
        "/etc/hyperstor/vhost.env",
        "/etc/systemd/system/hyperstor-vhost.service",
        "would restart hyperstor-vhost.service",
    ] {
        assert!(r.has(f), "{f}:\n{}", r.out);
    }
    // Installed on every host with the others.
    assert!(c.root.join("usr/local/bin/hyperstor-vhost").exists());
    // Started as its unit would start it, its sockets somewhere short.
    let dir = std::env::temp_dir().join(format!("hsi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let args: Vec<String> = env_args(&c.root, c.scratch.run(), "vhost.env")
        .into_iter()
        .map(|a| a.replace("/run/hyperstor/vhost", dir.to_str().unwrap()))
        .collect();
    let mut child = Killed(
        Command::new(c.root.join("usr/local/bin/hyperstor-vhost"))
            .arg("serve")
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.0.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    assert!(line.contains("control socket"), "{line}");
    let made = c
        .ctl(&["create-volume", "--size", "8M", "--name", "v"])
        .ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let ctl = |a: &[&str]| {
        Command::new(c.root.join("usr/local/bin/hyperstor-vhost"))
            .args(["ctl", "--dir", dir.to_str().unwrap()])
            .args(a)
            .output()
            .unwrap()
    };
    let a = ctl(&["attach", &id]);
    assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stdout));
    assert!(ctl(&["detach", &id]).status.success());
    // SAFETY: kill on our own child's pid.
    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    assert!(child.0.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&dir);
    // Saved: a run with no flags changes nothing; --no-vhost removes it.
    assert!(install(&c.root, &[]).has("Nothing to change"));
    let r = install(&c.root, &["--no-vhost"]);
    assert!(
        r.has("removed /etc/systemd/system/hyperstor-vhost.service"),
        "{}",
        r.out
    );
    assert!(!c.root.join("etc/hyperstor/vhost.env").exists());
    // Refused: a VM host without the map service's addresses, a user
    // that doesn't exist.
    let s = Scratch::new("vhost-refused");
    let r = install_cmd(&s.path("a"), "install", &["--role", "vhost"]).expect(1);
    assert!(r.has("--map"), "{}", r.out);
    let r = install_cmd(
        &s.path("b"),
        "install",
        &[
            "--role",
            "vhost",
            "--map",
            "127.0.0.1:1",
            "--vhost-user",
            "no-such-user-hs",
        ],
    )
    .expect(1);
    assert!(r.has("no such user"), "{}", r.out);
}

/// D383: a host that joins with a token a management client bound to
/// realms made is bound to that client's realms, so its certificate opens
/// no other realm's volumes. The client gets no token for a host bound
/// past its realms, nor for one that joined before and serves every realm.
#[test]
fn a_host_a_bound_client_joins_is_bound_to_its_realms() {
    let c = Cluster::all_in_one("hostbind", 1);
    c.ctl(&["status", "--wait", "60"]).ok();
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_string();
    let gx = c.identity(hyperstor_pki::Role::Mgmt, "glidex", false);
    for realm in ["acme", "other"] {
        c.ctl(&["realm", "create", "--name", realm]).ok();
    }
    c.ctl(&["realm", "client", "--client", "glidex", "--realm", "acme"])
        .ok();
    c.ctl(&[
        "realm",
        "client-tokens",
        "--client",
        "glidex",
        "--role",
        "host",
    ])
    .ok();
    let make = |host: &str| {
        c.ctl_via(
            Via::Tls(&gx),
            &["token", "create", "--role", "host", "--name", host],
        )
    };
    let join = |root: &str, token: &str| {
        let root = c.scratch.path(root);
        link_binaries(&root);
        let f = c.scratch.path(&format!("{}.token", root.display()));
        std::fs::write(&f, token.lines().next().unwrap()).unwrap();
        install_cmd(
            &root,
            "install",
            &[
                "--role",
                "vhost",
                "--join",
                c.api.as_str(),
                "--token-file",
                f.to_str().unwrap(),
            ],
        )
    };
    let json = |b: &str| -> serde_json::Value {
        serde_json::from_str(b).unwrap_or_else(|e| panic!("{e}:\n{b}"))
    };
    let hosts_of = |realm: &str| {
        let l = json(&c.ctl(&["realm", "list", "--json"]).ok().out);
        l["realms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == realm)
            .map(|r| r["hosts"].to_string())
            .unwrap_or_default()
    };

    // The token says what its join binds.
    let t = make(&name).ok();
    let tokens = json(&c.ctl(&["token", "list", "--json"]).ok().out);
    let mine = tokens["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name.as_str())
        .unwrap_or_else(|| panic!("{tokens}"));
    assert_eq!(mine["bind_realms"], serde_json::json!(["acme"]), "{mine}");
    let listed = c.ctl(&["token", "list"]).ok();
    assert!(
        listed.has(&format!("(binds host {name} to acme)")),
        "{}",
        listed.out
    );
    // Not bound before the join.
    assert!(!hosts_of("acme").contains(&name), "{}", hosts_of("acme"));
    join("vm-a", &t.out).ok();
    assert!(hosts_of("acme").contains(&name), "{}", hosts_of("acme"));
    assert!(!hosts_of("other").contains(&name));
    assert!(
        c.audit().iter().any(|l| l.contains("\"op\":\"Join\"")
            && l.contains(&format!("host {name}, bound to realms acme"))),
        "{:?}",
        c.audit()
    );
    // The same host joins again (a reinstall), and stays as it is.
    join("vm-b", &make(&name).ok().out).ok();
    assert!(hosts_of("acme").contains(&name));

    // Bound past the client's realms: no token.
    c.ctl(&["realm", "host", "--host", &name, "--realm", "acme,other"])
        .ok();
    let r = make(&name);
    assert_ne!(r.code, 0, "{}", r.out);
    assert!(r.has("is bound to realm other"), "{}", r.out);
    // Unbound by an administrator, it serves every realm: no token.
    c.ctl(&["realm", "host", "--host", &name, "--realm", "any"])
        .ok();
    let r = make(&name);
    assert_ne!(r.code, 0, "{}", r.out);
    assert!(r.has("joined before and is bound to no realm"), "{}", r.out);
    assert!(!hosts_of("acme").contains(&name));
    // A name no binding could hold.
    let r = make(&"h".repeat(65));
    assert_ne!(r.code, 0, "{}", r.out);
    assert!(r.has("1-64"), "{}", r.out);
    // An administrator's token binds nothing.
    let t = c
        .ctl(&["token", "create", "--role", "host", "--name", &name])
        .ok();
    join("vm-c", &t.out).ok();
    assert!(!hosts_of("acme").contains(&name) && !hosts_of("other").contains(&name));
}

/// `hyperstorctl --identity DIR` (D385, I454): the commands that move data
/// show storage nodes the host's own certificate, and the nodes hold them
/// to the host's realms though the tool names no host: a volume of its
/// realm is written, another realm's isn't. With the administrator's
/// certificate both are. A directory without a certificate is an error.
#[test]
fn a_hosts_certificate_moves_data_within_its_realms() {
    let c = Cluster::all_in_one("hostdata", 3);
    c.ctl(&["status", "--wait", "60"]).ok();
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_string();
    for realm in ["acme", "other"] {
        c.ctl(&["realm", "create", "--name", realm]).ok();
    }
    // A VM host of its own, bound to one realm.
    let token = c
        .ctl(&["token", "create", "--role", "host", "--name", &name])
        .ok();
    let root = c.scratch.path("vm");
    link_binaries(&root);
    let f = c.scratch.path("vm.token");
    std::fs::write(&f, token.out.lines().next().unwrap()).unwrap();
    install_cmd(
        &root,
        "install",
        &[
            "--role",
            "vhost",
            "--join",
            c.api.as_str(),
            "--token-file",
            f.to_str().unwrap(),
        ],
    )
    .ok();
    c.ctl(&["realm", "host", "--host", &name, "--realm", "acme"])
        .ok();
    c.ctl(&["realm", "tenant", "--tenant", "7", "--realm", "acme"])
        .ok();
    c.ctl(&["realm", "tenant", "--tenant", "8", "--realm", "other"])
        .ok();
    let vol = |tenant: &str| {
        let made = c
            .ctl(&[
                "create-volume",
                "--size",
                "1M",
                "--name",
                &format!("vol-{tenant}"),
                "--tenant",
                tenant,
            ])
            .ok();
        made.out.split_whitespace().nth(1).unwrap().to_string()
    };
    let (mine, theirs) = (vol("7"), vol("8"));
    let data = c.scratch.path("data.raw");
    std::fs::write(&data, vec![7u8; 1 << 20]).unwrap();
    let host_pki = c.scratch.path("vm").join("etc/hyperstor/pki");
    let write = |id: &str, as_host: bool| {
        let mut args = vec!["write-volume", "--id", id, "--file", data.to_str().unwrap()];
        if as_host {
            args.extend(["--identity", host_pki.to_str().unwrap()]);
        }
        c.ctl(&args)
    };
    write(&mine, true).ok();
    let r = write(&theirs, true);
    assert!(r.code != 0 && r.has("realm"), "{}", r.out);
    write(&theirs, false).ok();
    // A directory with no certificate of a host is an error, not a
    // fall back to the administrator's.
    let empty = c.scratch.path("no-pki");
    std::fs::create_dir_all(&empty).unwrap();
    c.ctl(&[
        "write-volume",
        "--id",
        &mine,
        "--file",
        data.to_str().unwrap(),
        "--identity",
        empty.to_str().unwrap(),
    ])
    .expect(2);
}

/// A VM host joins with a `host` token (D375; 30-data-path-tls.md §33.6):
/// its certificate names it as it names itself to storage nodes, the map
/// service's addresses come with the join, and later runs need no token.
/// A token for another name or role is refused, with the command that
/// makes the right one. On the cluster's own host, each storage node has
/// a certificate of its own (D378).
#[test]
fn a_vm_host_joins_with_a_host_token() {
    let c = Cluster::all_in_one("hostjoin", 2);
    c.ctl(&["status", "--wait", "60"]).ok();
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_string();
    let token = |role: &str, name: &str| {
        let r = c
            .ctl(&["token", "create", "--role", role, "--name", name])
            .ok();
        let f = c.scratch.path(&format!("{role}-{name}.token"));
        std::fs::write(&f, r.out.lines().next().unwrap()).unwrap();
        f
    };
    let join = |root: &std::path::Path, f: Option<&std::path::Path>| {
        link_binaries(root);
        let mut a = vec!["--role", "vhost", "--join", c.api.as_str()];
        if let Some(f) = f {
            a.extend(["--token-file", f.to_str().unwrap()]);
        }
        install_cmd(root, "install", &a)
    };
    // A host token names a host; a name that is none is refused.
    c.ctl(&["token", "create", "--role", "host"]).expect(1);
    c.ctl(&["token", "create", "--role", "host", "--name", "a/b"])
        .expect(1);

    // No token, a token for another host, a token of another role.
    let r = join(&c.scratch.path("none"), None).expect(1);
    assert!(
        r.has(&format!("token create --role host --name {name}")),
        "{}",
        r.out
    );
    let r = join(
        &c.scratch.path("other"),
        Some(&token("host", "some-other-host")),
    )
    .expect(1);
    assert!(
        r.has("is for host \"some-other-host\"") && r.has(&format!("--name {name}")),
        "{}",
        r.out
    );
    let r = join(&c.scratch.path("role"), Some(&token("mgmt", "not-a-host"))).expect(1);
    assert!(r.has("isn't a host token"), "{}", r.out);

    // Its own.
    let host = c.scratch.path("vm-host");
    let r = join(&host, Some(&token("host", &name))).ok();
    assert!(r.has("as host hyperstor://"), "{}", r.out);
    let pki = host.join("etc/hyperstor/pki");
    let cert = std::fs::read_to_string(pki.join("host.crt")).unwrap();
    let who = hyperstor_pki::identity_in(&cert).unwrap();
    assert_eq!(
        (who.role, who.id.as_str(), who.cluster.as_str()),
        (
            hyperstor_pki::Role::Host,
            name.as_str(),
            c.cluster_id.as_str()
        )
    );
    assert!(pki.join("host.key").exists() && pki.join("root.crt").exists());
    // The map service's addresses came with the join; nothing of the
    // cluster's own runs there.
    let env = std::fs::read_to_string(host.join("etc/hyperstor/vhost.env")).unwrap();
    assert!(env.contains(&format!("--meta {}", c.map)), "{env}");
    assert!(!host.join("etc/hyperstor/meta.env").exists());
    assert!(!host.join("etc/hyperstor/node-0.env").exists());
    // Later runs need no token, and keep the certificate.
    let r = join(&host, None).ok();
    assert!(r.has("Nothing to change"), "{}", r.out);
    assert_eq!(std::fs::read_to_string(pki.join("host.crt")).unwrap(), cert);
    // The audit names the join.
    assert!(
        c.audit()
            .iter()
            .any(|l| l.contains("\"op\":\"Join\"") && l.contains(&format!("host {name}"))),
        "{:#?}",
        c.audit()
    );

    // Each storage node of the cluster's host has its own certificate.
    for n in 0..2 {
        let cert = std::fs::read_to_string(c.root.join(format!("etc/hyperstor/pki/node-{n}.crt")))
            .unwrap();
        let who = hyperstor_pki::identity_in(&cert).unwrap();
        assert_eq!(
            (who.role, who.id),
            (hyperstor_pki::Role::Node, n.to_string())
        );
    }
}
