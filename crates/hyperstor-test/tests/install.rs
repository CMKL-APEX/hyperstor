//! Installing (administration.md §23.4): host checks, the all-in-one
//! install and its convergence, uninstall, and a cluster across hosts
//! from one cluster file.

use hyperstor_test::*;
use std::process::Command;

/// `check` reports every check on its own line with a level, and changes
/// nothing. Verdicts depend on the host, so only the shape is checked.
#[test]
fn check_reports_every_item() {
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
}

/// A first install writes everything; a second with no flags reuses the
/// saved ones and changes nothing; a changed flag rewrites only what
/// depends on it; uninstall removes services and binaries, keeps
/// configuration.
#[test]
fn install_is_convergent_and_uninstall_keeps_configuration() {
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

/// Bad input is refused with a usage message, and nothing is written.
#[test]
fn bad_flags_are_refused() {
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

/// A cluster across "hosts" from one cluster file (implementation-notes
/// I50): the map service on one, a storage node on each of three others,
/// each installed into a root of its own and started from what was
/// installed; then managed from the map service's host.
#[test]
fn cluster_file_install_across_hosts() {
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
    let mut a = common.to_vec();
    a.extend(["--role", "meta", "--api-port", &api_port]);
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
                .args(["--spin-us", "0"])
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    let api = format!("127.0.0.1:{api_port}");
    let pki = meta_root.join("etc/hyperstor/pki");
    let ctl = |args: &[&str]| {
        let o = Command::new(bin.join("hyperstorctl"))
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
