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

/// The root CA's key (D84): an install says it has no backup; a backup
/// is encrypted, opens only with its passphrase, may take the key off the
/// host (later runs still converge), and restores only onto its own
/// cluster.
#[test]
fn root_key_backup_and_restore() {
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
    run("root-backup", &args(&["--remove-key"])).ok();
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = std::fs::read_to_string(&bak).unwrap();
    assert!(!text.contains("PRIVATE KEY") && text.starts_with("hyperstor root backup v1"));
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
    restore("a long enough passphrase", &root).ok();
    assert_eq!(std::fs::read_to_string(pki.join("root.key")).unwrap(), key);
    // --out belongs to the backup commands only.
    install_cmd(&root, "install", &["--out", "x"]).expect(2);
}

/// The web UI's files (D100): installed from `--ui` with mgmt told to
/// serve them, kept by later runs without it, made exactly the new
/// build's by a run with a new one, and removed by uninstall.
#[test]
fn the_web_ui_is_installed_as_files() {
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
    let mut child = Command::new(c.root.join("usr/local/bin/hyperstor-vhost"))
        .arg("serve")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
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
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert!(child.wait().unwrap().success());
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
