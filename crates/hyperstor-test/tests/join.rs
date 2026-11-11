//! Joining hosts with tokens (19-administration.md §23.4.2, 04-decisions.md
//! D57): a map service on one "host", storage hosts that join it and are
//! added to the map at run time, a group of map service members that join
//! the first, tokens that are single-use, expire and can be revoked, and
//! renewal of a joined host's certificate over the API.

use hyperstor_test::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// The tests here each run several map service members or hosts: one at
/// a time, so they don't slow each other's Raft.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// Processes killed when the test ends, however it ends.
#[derive(Default)]
struct Procs(Vec<Child>);

impl Drop for Procs {
    fn drop(&mut self) {
        for p in &mut self.0 {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
}

fn start(bin: &Path, mut args: Vec<String>, log: PathBuf) -> Child {
    if bin.ends_with("hyperstord") {
        // As the harness runs storage nodes.
        args.extend(NODE_ARGS.map(String::from));
    }
    Command::new(bin)
        .args(args)
        .stderr(std::fs::File::create(log).unwrap())
        .spawn()
        .unwrap()
}

/// `hyperstorctl` against `api` with the administrator certificate in
/// `pki`.
fn ctl(bin: &Path, api: &str, pki: &Path, args: &[&str]) -> Run {
    ctl_at(bin, api, pki, args)
}

/// A token for `role`, written to a file, as an operator would.
fn token(bin: &Path, api: &str, pki: &Path, s: &Scratch, role: &str, extra: &[&str]) -> PathBuf {
    let mut args = vec!["token", "create", "--role", role];
    args.extend(extra);
    // A map service that has just started has no map yet, and refuses
    // with `not_formed` until it has: retried, as an operator would.
    let o = wait_for("the map service to make a token", 20, || {
        let o = Command::new(bin.join("hyperstorctl"))
            .args(["--api", api, "--tls", pki.to_str().unwrap()])
            .args(&args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(o.status.success() || err.contains("(not_formed)"), "{err}");
        o.status.success().then_some(o)
    });
    let t = String::from_utf8(o.stdout).unwrap();
    assert!(
        t.starts_with("hsj1-") && t.lines().count() == 1,
        "only the token: {t}"
    );
    let hint = String::from_utf8_lossy(&o.stderr);
    assert!(hint.contains(&format!("--join {api}")), "{hint}");
    let f = s.path(&format!("token-{}", t.len() + hint.len() + rand_suffix()));
    std::fs::write(&f, t).unwrap();
    f
}

fn rand_suffix() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Waits until the API at `api` answers (the map service is starting).
fn wait_api(bin: &Path, api: &str, pki: &Path) {
    wait_for("the cluster API", 20, || {
        let r = ctl(bin, api, pki, &["token", "list"]);
        (r.code == 0).then_some(())
    });
}

/// Installs a storage host that joins through `api` with the token in
/// `token`: `reactors` reactors from `port`, metrics on `admin`.
fn join_storage(
    root: &Path,
    data: &Path,
    api: &str,
    token: &Path,
    port: u16,
    reactors: u32,
    admin: u16,
) -> Run {
    std::fs::create_dir_all(data).unwrap();
    link_binaries(root);
    install_cmd(
        root,
        "install",
        &[
            "--join",
            api,
            "--token-file",
            token.to_str().unwrap(),
            "--role",
            "storage",
            "--address",
            "127.0.0.1",
            "--reactors",
            &reactors.to_string(),
            "--node-port",
            &port.to_string(),
            "--admin-port",
            &admin.to_string(),
            "--size-gib",
            "1",
            "--data-dir",
            data.to_str().unwrap(),
        ],
    )
}

/// A map service, then three storage hosts that join it with tokens and
/// serve I/O; tokens are single-use, expire and can be revoked; a joined
/// host's certificate names its node and is renewed over the API, and
/// adds devices to that node only (D280).
#[test]
fn storage_hosts_join_with_tokens() {
    let _one = one_at_a_time();
    let s = Scratch::new("join");
    let base = free_ports(25);
    let (api_port, map_port) = (base, base + 1);
    let file = s.path("cluster.txt");
    // No storage node yet: they join.
    std::fs::write(
        &file,
        "cluster 11\nnetwork fast data subnet 127.0.0.0/8\n\
         pool 1 vol replicated size 3 min 2 pgs 32 domain host\n",
    )
    .unwrap();
    let meta_root = s.path("meta-host");
    link_binaries(&meta_root);
    let meta_data = s.path("meta-data");
    std::fs::create_dir_all(&meta_data).unwrap();
    let map = format!("127.0.0.1:{map_port}");
    let r = install(
        &meta_root,
        &[
            "--cluster",
            file.to_str().unwrap(),
            "--role",
            "meta",
            "--map",
            &map,
            "--api-port",
            &api_port.to_string(),
            "--data-dir",
            meta_data.to_str().unwrap(),
        ],
    );
    assert!(r.has("/etc/hyperstor/pki/root.crt"), "{}", r.out);
    let bin = meta_root.join("usr/local/bin");
    let pki = meta_root.join("etc/hyperstor/pki");
    let api = format!("127.0.0.1:{api_port}");
    let mut procs = Procs::default();
    let meta_log = s.path("meta.log");
    procs.0.push(start(
        &bin.join("hyperstor-meta"),
        env_args(&meta_root, s.run(), "meta.env"),
        meta_log.clone(),
    ));
    wait_api(&bin, &api, &pki);

    // Three hosts join; the first with two reactors.
    for n in 0..3u16 {
        let t = token(&bin, &api, &pki, &s, "node", &[]);
        let root = s.path(&format!("host-{n}"));
        let reactors = if n == 0 { 2 } else { 1 };
        let r = join_storage(
            &root,
            &s.path(&format!("data-{n}")),
            &api,
            &t,
            base + 2 + 3 * n,
            reactors,
            base + 12 + n,
        )
        .ok();
        assert!(
            r.has(&format!("This host is storage node {n}.")),
            "{}",
            r.out
        );
        let pki_n = root.join("etc/hyperstor/pki");
        let cert = std::fs::read_to_string(pki_n.join("node.crt")).unwrap();
        let who = hyperstor_pki::identity_in(&cert).unwrap();
        assert_eq!(
            (who.role, who.id.as_str(), who.cluster.as_str()),
            (hyperstor_pki::Role::Node, n.to_string().as_str(), "11")
        );
        assert!(!pki_n.join("root.key").exists() && !pki_n.join("intermediate.key").exists());
        assert!(!root.join("etc/hyperstor/cluster.txt").exists());
        let env =
            std::fs::read_to_string(root.join(format!("etc/hyperstor/node-{n}.env"))).unwrap();
        assert!(env.contains(&format!("--meta {map}")), "{env}");
        assert_eq!(env.matches("--shard ").count(), reactors as usize, "{env}");
        // The token is never saved.
        let conf = std::fs::read_to_string(root.join("etc/hyperstor/install.conf")).unwrap();
        assert!(!conf.contains("hsj1"), "{conf}");
        procs.0.push(start(
            &root.join("usr/local/bin/hyperstord"),
            env_args(&root, s.run(), &format!("node-{n}.env")),
            s.path(&format!("node-{n}.log")),
        ));
        // A used token joins nothing more.
        if n == 0 {
            let again = join_storage(
                &s.path("host-x"),
                &s.path("data-x"),
                &api,
                &t,
                base + 20,
                1,
                base + 21,
            );
            assert_eq!(again.code, 1, "{}", again.out);
            assert!(
                again.has("already used") && again.has("(token_used)"),
                "{}",
                again.out
            );
        }
    }
    let r = ctl(&bin, &api, &pki, &["status", "--wait", "30"]).ok();
    assert!(r.has("nodes   3 up of 3; devices 4 in of 4"), "{}", r.out);
    ctl(&bin, &api, &pki, &["io-test", "--mib", "8"]).ok();

    // Tokens are in the replicated store: one made before the map
    // service restarts still joins a host after it (D81).
    let kept = token(&bin, &api, &pki, &s, "node", &[]);
    let mut meta = procs.0.remove(0);
    let _ = meta.kill();
    let _ = meta.wait();
    let mut again = start(
        &bin.join("hyperstor-meta"),
        env_args(&meta_root, s.run(), "meta.env"),
        s.path("meta-2.log"),
    );
    // If it exits instead (a port taken in the gap), say so with its log.
    wait_for("the restarted map service's API", 20, || {
        if let Ok(Some(st)) = again.try_wait() {
            panic!(
                "the restarted map service exited ({st}):\n{}",
                std::fs::read_to_string(s.path("meta-2.log")).unwrap_or_default()
            );
        }
        (ctl(&bin, &api, &pki, &["token", "list"]).code == 0).then_some(())
    });
    procs.0.push(again);
    let r = wait_for("the restarted map service to lead", 20, || {
        let r = join_storage(
            &s.path("host-3"),
            &s.path("data-3"),
            &api,
            &kept,
            base + 16,
            1,
            base + 18,
        );
        // Not leading yet: refused, and the token not burnt.
        (r.code == 0 || !r.has("(not_leader)")).then_some(r)
    });
    assert!(r.has("This host is storage node 3."), "{}", r.out);

    // Revoked and expired tokens, and another cluster's: refused.
    let revoked = token(&bin, &api, &pki, &s, "node", &[]);
    // Its id: the first 12 hex digits of its secret's SHA-256.
    let secret = std::fs::read_to_string(&revoked).unwrap();
    let id = hyperstor_pki::sha256_hex(secret.trim().rsplit('-').next().unwrap().as_bytes())[..12]
        .to_string();
    let list = ctl(&bin, &api, &pki, &["token", "list", "--json"]).ok();
    assert!(list.has(&format!("\"id\":\"{id}\"")), "{}", list.out);
    ctl(&bin, &api, &pki, &["token", "revoke", "--id", &id]).ok();
    let r = join_storage(
        &s.path("host-r"),
        &s.path("data-r"),
        &api,
        &revoked,
        base + 20,
        1,
        base + 21,
    );
    assert!(r.code == 1 && r.has("(token_revoked)"), "{}", r.out);
    let expired = token(&bin, &api, &pki, &s, "node", &["--ttl", "1"]);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let r = join_storage(
        &s.path("host-e"),
        &s.path("data-e"),
        &api,
        &expired,
        base + 20,
        1,
        base + 21,
    );
    assert!(r.code == 1 && r.has("(token_expired)"), "{}", r.out);
    let other = std::fs::read_to_string(token(&bin, &api, &pki, &s, "node", &[])).unwrap();
    let wrong_pin = format!(
        "hsj1-{}-{}",
        "0".repeat(64),
        other.trim().rsplit('-').next().unwrap()
    );
    let f = s.path("wrong-pin");
    std::fs::write(&f, wrong_pin).unwrap();
    let r = join_storage(
        &s.path("host-w"),
        &s.path("data-w"),
        &api,
        &f,
        base + 20,
        1,
        base + 21,
    );
    assert!(
        r.code == 1 && r.has("fingerprint"),
        "refused before sending: {}",
        r.out
    );
    let list = ctl(&bin, &api, &pki, &["token", "list"]).ok();
    assert!(
        list.has("revoked") && list.has("used") && list.has("expired"),
        "{}",
        list.out
    );
    // A token needs a role that may make one; a node's certificate can't.
    let node_tls = s.path("node-tls");
    std::fs::create_dir_all(&node_tls).unwrap();
    let h0 = s.path("host-0").join("etc/hyperstor/pki");
    std::fs::copy(h0.join("node.crt"), node_tls.join("admin.crt")).unwrap();
    std::fs::copy(h0.join("node.key"), node_tls.join("admin.key")).unwrap();
    std::fs::copy(h0.join("root.crt"), node_tls.join("root.crt")).unwrap();
    let r = ctl(
        &bin,
        &api,
        &node_tls,
        &["token", "create", "--role", "node"],
    );
    assert!(r.code == 4 && r.has("(permission_denied)"), "{}", r.out);
    // D215: a node's certificate sets its own devices' endpoints, and no
    // other node's; the administrator any.
    let ep = format!("fast=127.0.0.2:{}", base + 23);
    let r = ctl(
        &bin,
        &api,
        &node_tls,
        &["device", "endpoints", "--device", "0", "--endpoints", &ep],
    );
    assert_eq!(r.code, 0, "{}", r.out);
    let r = ctl(
        &bin,
        &api,
        &node_tls,
        &[
            "device",
            "endpoints",
            "--device",
            "1000",
            "--endpoints",
            &ep,
        ],
    );
    assert!(r.code == 1 && r.has("only its own node"), "{}", r.out);
    // In the map once the change is published (fenced), a moment later.
    let t = std::time::Instant::now();
    loop {
        let nodes = ctl(&bin, &api, &pki, &["nodes", "--json"]).ok();
        if nodes.has(&ep[5..]) && nodes.has("fast") {
            break;
        }
        assert!(t.elapsed().as_secs() < 10, "{}", nodes.out);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let r = ctl(
        &bin,
        &api,
        &pki,
        &[
            "device",
            "endpoints",
            "--device",
            "0",
            "--endpoints",
            "none",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.out);
    let r = ctl(
        &bin,
        &api,
        &pki,
        &[
            "device",
            "endpoints",
            "--device",
            "0",
            "--endpoints",
            "slow=127.0.0.2:1",
        ],
    );
    assert!(r.code == 1 && r.has("no network slow"), "{}", r.out);
    // D280: a node's certificate adds devices to its own node only, at an
    // IP it already serves on and without a class; so the installer's
    // --device works on a storage-only host.
    let own = format!("127.0.0.1:{}", base + 24);
    let elsewhere = format!("127.0.0.2:{}", base + 24);
    for (args, why) in [
        (
            vec!["--node", "1", "--address", &own],
            "only to its own node",
        ),
        (
            vec!["--node", "0", "--address", &elsewhere],
            "already serves on",
        ),
        (
            vec!["--node", "0", "--address", &own, "--class", "fast"],
            "without a class",
        ),
    ] {
        let mut a = vec!["device", "add"];
        a.extend(args);
        let r = ctl(&bin, &api, &node_tls, &a);
        assert!(r.code != 0 && r.has(why), "{why}: {}", r.out);
    }
    let extra = s.path("extra-0.img");
    std::fs::File::create(&extra)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let e = extra.to_str().unwrap();
    let r = install(
        &s.path("host-0"),
        &[
            "--no-start",
            "--node",
            "0",
            "--device",
            e,
            "--device-address",
            &own,
        ],
    );
    assert!(
        r.has(&format!("added {e} to node 0 as device ")) && r.has(&format!("served at {own}")),
        "{}",
        r.out
    );

    // The joins' certificates are listed, by serial; revoking node 1's
    // refuses it from then on.
    let certs = ctl(&bin, &api, &pki, &["cert", "list"]).ok();
    for n in 0..4 {
        assert!(certs.has(&format!("/node/{n} ")), "node {n} not listed");
    }
    let h1 = s.path("host-1").join("etc/hyperstor/pki");
    let serial =
        hyperstor_pki::serial_in(&std::fs::read_to_string(h1.join("node.crt")).unwrap()).unwrap();
    ctl(&bin, &api, &pki, &["cert", "revoke", "--serial", &serial]).ok();
    let node1 = s.path("node1-tls");
    std::fs::create_dir_all(&node1).unwrap();
    std::fs::copy(h1.join("node.crt"), node1.join("admin.crt")).unwrap();
    std::fs::copy(h1.join("node.key"), node1.join("admin.key")).unwrap();
    std::fs::copy(h1.join("root.crt"), node1.join("root.crt")).unwrap();
    let r = ctl(&bin, &api, &node1, &["status"]);
    assert!(r.code == 4 && r.has("(revoked)"), "{}", r.out);

    // Run again: nothing changes, no token needed.
    let host0 = s.path("host-0");
    assert!(install(&host0, &[]).has("Nothing to change"));
    // A certificate near its end is renewed over the API with itself.
    let read = |f: &str| std::fs::read_to_string(pki.join(f)).unwrap();
    let ca = hyperstor_pki::Pem {
        cert: read("intermediate.crt"),
        key: read("intermediate.key"),
    };
    let who = hyperstor_pki::Identity {
        cluster: "11".into(),
        role: hyperstor_pki::Role::Node,
        id: "0".into(),
    };
    let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let short = hyperstor_pki::issue(&ca, &who, &[ip], &[], 2).unwrap();
    std::fs::write(h0.join("node.crt"), &short.cert).unwrap();
    std::fs::write(h0.join("node.key"), &short.key).unwrap();
    let r = install(&host0, &[]);
    assert!(r.has("Renewed /etc/hyperstor/pki/node.crt"), "{}", r.out);
    let renewed = std::fs::read_to_string(h0.join("node.crt")).unwrap();
    assert_ne!(renewed, short.cert);
    assert!(!hyperstor_pki::due_for_renewal(
        &renewed,
        hyperstor_pki::LEAF_DAYS
    ));
    assert_eq!(hyperstor_pki::identity_in(&renewed), Some(who));
    assert_eq!(hyperstor_pki::names_of(&renewed).unwrap().0, vec![ip]);
    assert_ne!(
        std::fs::read_to_string(h0.join("node.key")).unwrap(),
        short.key,
        "a new key"
    );
    // Every join, refused or not, is audited, without the secret.
    // Both lives of the map service: before and after its restart.
    let log = std::fs::read_to_string(&meta_log).unwrap()
        + &std::fs::read_to_string(s.path("meta-2.log")).unwrap();
    let audit: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("\"op\":\"Join\""))
        .collect();
    assert_eq!(
        audit
            .iter()
            .filter(|l| l.contains("\"result\":\"ok, token"))
            .count(),
        4,
        "{log}"
    );
    assert!(
        audit.iter().filter(|l| l.contains("refused")).count() >= 3,
        "{log}"
    );
    assert!(
        !log.contains(other.trim()),
        "the secret never reaches the log"
    );
    assert!(log.contains("\"op\":\"RenewCertificate\""), "{log}");
}

/// A map service of three members: the first installs the CAs, the other
/// two join it with `meta` tokens and get the same root and intermediate,
/// so Raft's mutual TLS works between them; a storage host joins, and the
/// cluster serves I/O.
#[test]
fn map_service_members_join_the_first() {
    let _one = one_at_a_time();
    let s = Scratch::new("join-group");
    let base = free_ports(16);
    let member = |i: u16| (base + 3 * i, base + 3 * i + 1, base + 3 * i + 2); // api, map, raft
    let members: String = (0..3)
        .map(|i| {
            let (_, m, r) = member(i);
            format!("{}=127.0.0.1:{r}/127.0.0.1:{m}", i + 1)
        })
        .collect::<Vec<_>>()
        .join(",");
    let file = s.path("cluster.txt");
    std::fs::write(
        &file,
        "cluster 12\npool 1 vol replicated size 1 min 1 pgs 16 domain host\n",
    )
    .unwrap();
    let mut procs = Procs::default();
    let mut apis: Vec<String> = Vec::new();
    let mut first_pki = PathBuf::new();
    let mut bin = PathBuf::new();
    for i in 0..3u16 {
        let root = s.path(&format!("member-{}", i + 1));
        link_binaries(&root);
        let data = s.path(&format!("member-data-{}", i + 1));
        std::fs::create_dir_all(&data).unwrap();
        let (api_port, _, _) = member(i);
        let id = (i + 1).to_string();
        let api_s = api_port.to_string();
        let mut args = vec![
            "--role",
            "meta",
            "--members",
            &members,
            "--member-id",
            &id,
            "--api-port",
            &api_s,
            "--data-dir",
            data.to_str().unwrap(),
        ];
        let tok;
        let first_api = apis.first().cloned().unwrap_or_default();
        if i == 0 {
            args.extend(["--cluster", file.to_str().unwrap()]);
        } else {
            tok = token(&bin, &first_api, &first_pki, &s, "meta", &[]);
            args.extend([
                "--join",
                first_api.as_str(),
                "--token-file",
                tok.to_str().unwrap(),
            ]);
        }
        let r = install(&root, &args);
        let pki = root.join("etc/hyperstor/pki");
        if i == 0 {
            first_pki = pki.clone();
            bin = root.join("usr/local/bin");
        } else {
            assert!(r.has("Joined cluster 12"), "{}", r.out);
            assert!(
                !pki.join("root.key").exists(),
                "the root key stays on the first host"
            );
            for f in [
                "root.crt",
                "intermediate.crt",
                "intermediate.key",
                "cluster.key",
            ] {
                assert_eq!(
                    std::fs::read(pki.join(f)).unwrap(),
                    std::fs::read(first_pki.join(f)).unwrap(),
                    "{f}"
                );
            }
            assert_eq!(
                std::fs::read(root.join("etc/hyperstor/cluster.txt")).unwrap(),
                std::fs::read(&file).unwrap()
            );
        }
        let api_cert = std::fs::read_to_string(pki.join("api.crt")).unwrap();
        let who = hyperstor_pki::identity_in(&api_cert).unwrap();
        assert_eq!((who.role, who.id), (hyperstor_pki::Role::Meta, id.clone()));
        procs.0.push(start(
            &root.join("usr/local/bin/hyperstor-meta"),
            env_args(&root, &s.run_of(&root), "meta.env"),
            s.path(&format!("member-{}.log", i + 1)),
        ));
        apis.push(format!("127.0.0.1:{api_port}"));
        if i == 0 {
            wait_api(&bin, &apis[0], &first_pki);
        }
    }
    // Storage tokens come from the leader, whichever member that is; the
    // others refuse and name it.
    let pkis: Vec<PathBuf> = (1..=3)
        .map(|i| s.path(&format!("member-{i}")).join("etc/hyperstor/pki"))
        .collect();
    // On a timeout, the members' logs say why.
    let last_refusals: std::cell::RefCell<Vec<String>> = Default::default();
    let dump = || {
        eprintln!("last refusals: {:#?}", last_refusals.borrow());
        for i in 1..=3 {
            let log =
                std::fs::read_to_string(s.path(&format!("member-{i}.log"))).unwrap_or_default();
            eprintln!("--- member {i}:\n{log}");
        }
    };
    let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for("a leader to make a storage token", 40, || {
            (0..3).find_map(|i| {
                let r = ctl(
                    &bin,
                    &apis[i],
                    &pkis[i],
                    &["token", "create", "--role", "node"],
                );
                if r.code != 0 {
                    assert!(r.has("(not_leader)") || r.code == 3, "{}", r.out);
                    let mut l = last_refusals.borrow_mut();
                    l.push(format!("member {}: {}", i + 1, r.out));
                    let n = l.len();
                    l.drain(..n.saturating_sub(3));
                    return None;
                }
                let f = s.path("store-token");
                std::fs::write(&f, r.out.lines().next().unwrap()).unwrap();
                Some((i, f))
            })
        })
    }));
    let (leader, t) = found.unwrap_or_else(|e| {
        dump();
        std::panic::resume_unwind(e)
    });
    // A follower refuses, and names the leader once it has heard from it.
    let follower = (leader + 1) % 3;
    wait_for("a follower to name the leader", 20, || {
        let r = ctl(
            &bin,
            &apis[follower],
            &pkis[follower],
            &["token", "create", "--role", "node"],
        );
        assert!(r.code == 3 && r.has("(not_leader)"), "{}", r.out);
        r.has("the leader is").then_some(())
    });
    // Through a follower the token is refused, and not burnt (only the
    // leader writes tokens, D82); through the leader it joins.
    let t_host = s.path("store");
    let r = join_storage(
        &s.path("store-via-follower"),
        &s.path("store-data-f"),
        &apis[follower],
        &t,
        base + 10,
        1,
        base + 11,
    );
    assert!(r.code == 1 && r.has("(not_leader)"), "{}", r.out);
    join_storage(
        &t_host,
        &s.path("store-data"),
        &apis[leader],
        &t,
        base + 10,
        1,
        base + 11,
    )
    .ok();
    let t = t_host;
    let env = std::fs::read_to_string(t.join("etc/hyperstor/node-0.env")).unwrap();
    let maps: Vec<String> = (0..3)
        .map(|i| format!("127.0.0.1:{}", member(i).1))
        .collect();
    assert!(env.contains(&format!("--meta {}", maps.join(","))), "{env}");
    procs.0.push(start(
        &t.join("usr/local/bin/hyperstord"),
        env_args(&t, s.run(), "node-0.env"),
        s.path("store.log"),
    ));
    let r = ctl(&bin, &apis[0], &first_pki, &["status", "--wait", "30"]);
    assert_eq!(r.code, 0, "{}", r.out);
    ctl(&bin, &apis[0], &first_pki, &["io-test", "--mib", "4"]).ok();
    // A follower answers with what only the leader knows (I197): each
    // device's space, nodes heard from, and no note to ask the leader.
    let n = wait_for("the follower to show device space", 30, || {
        let r = ctl(&bin, &apis[follower], &pkis[follower], &["nodes"]).ok();
        (r.out.lines().count() > 1 && r.out.lines().skip(1).all(|l| !l.contains(" - ")))
            .then_some(r)
    });
    assert!(n.has("G "), "{}", n.out);
    let st = ctl(&bin, &apis[follower], &pkis[follower], &["status"]).ok();
    assert!(!st.has("ask it for those"), "{}", st.out);
    // Every member lists the tokens: they are replicated.
    for i in 0..3 {
        let r = ctl(&bin, &apis[i], &pkis[i], &["token", "list"]).ok();
        assert!(r.has("used"), "member {}: {}", i + 1, r.out);
    }
    // A member without the cluster key (one that joined before it existed,
    // or lost it) gets it from the member it joined through (D162); only
    // members may ask, so an administrator's certificate is refused.
    let third = s.path("member-3");
    std::fs::remove_file(pkis[2].join("cluster.key")).unwrap();
    let r = install(&third, &[]);
    assert!(r.has("Fetched the cluster key from member"), "{}", r.out);
    assert_eq!(
        std::fs::read(pkis[2].join("cluster.key")).unwrap(),
        std::fs::read(first_pki.join("cluster.key")).unwrap()
    );
    let wrong = hyperstor_pki::client_config(
        &std::fs::read_to_string(first_pki.join("admin.crt")).unwrap(),
        &std::fs::read_to_string(first_pki.join("admin.key")).unwrap(),
        &std::fs::read_to_string(first_pki.join("root.crt")).unwrap(),
    )
    .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let refused = rt.block_on(async {
        let client = connectrpc::client::HttpClient::with_tls(std::sync::Arc::new(wrong));
        let config =
            connectrpc::client::ClientConfig::new(format!("https://{}", apis[0]).parse().unwrap());
        hyperstor_api::CaServiceClient::new(client, config)
            .get_cluster_key(hyperstor_api::GetClusterKeyRequest::default())
            .await
            .map(|_| ())
    });
    let e = refused.expect_err("an administrator may not read the cluster key");
    assert!(
        e.message
            .as_deref()
            .unwrap_or("")
            .contains("may not read the cluster key"),
        "{e:?}"
    );

    // The first member is lost: the other two elect a leader, which makes
    // tokens and adds hosts without it.
    let mut first = procs.0.remove(0);
    let _ = first.kill();
    let _ = first.wait();
    let (leader, t) = wait_for("a leader without the first member", 40, || {
        (1..3).find_map(|i| {
            let r = ctl(
                &bin,
                &apis[i],
                &pkis[i],
                &["token", "create", "--role", "node"],
            );
            if r.code != 0 {
                return None;
            }
            let f = s.path("store-2-token");
            std::fs::write(&f, r.out.lines().next().unwrap()).unwrap();
            Some((i, f))
        })
    });
    let host = s.path("store-2");
    let r = join_storage(
        &host,
        &s.path("store-2-data"),
        &apis[leader],
        &t,
        base + 12,
        1,
        base + 13,
    )
    .ok();
    assert!(r.has("This host is storage node 1."), "{}", r.out);
    procs.0.push(start(
        &host.join("usr/local/bin/hyperstord"),
        env_args(&host, s.run(), "node-1.env"),
        s.path("store-2.log"),
    ));
    // A new leader publishes changes once the old leases ran out (D73),
    // and the host is down until its first heartbeat: wait for both.
    wait_for("the new host up in the published map", 30, || {
        let r = ctl(&bin, &apis[leader], &pkis[leader], &["status"]);
        r.has("nodes   2 up of 2").then_some(())
    });
}

/// A host that is both a map service member and a storage node of the
/// cluster file (`--join --role both --node N`): it joins as the member,
/// with the first host's CAs and the cluster file, and installs its node
/// from that file.
#[test]
fn a_member_host_also_runs_storage() {
    let _one = one_at_a_time();
    let s = Scratch::new("join-both");
    let base = free_ports(12);
    let member = |i: u16| (base + 3 * i, base + 3 * i + 1, base + 3 * i + 2); // api, map, raft
    let members: String = (0..3)
        .map(|i| {
            let (_, m, r) = member(i);
            format!("{}=127.0.0.1:{r}/127.0.0.1:{m}", i + 1)
        })
        .collect::<Vec<_>>()
        .join(",");
    let (n0, n1) = (base + 9, base + 10);
    let file = s.path("cluster.txt");
    std::fs::write(
        &file,
        format!(
            "cluster 13\nnode 0 rack 0 127.0.0.1:{n0}\nnode 1 rack 1 127.0.0.1:{n1}\n\
             pool 1 vol replicated size 2 min 1 pgs 16 domain host\n"
        ),
    )
    .unwrap();
    let mut procs = Procs::default();
    let install_member = |i: u16, extra: &[&str]| {
        let root = s.path(&format!("member-{}", i + 1));
        link_binaries(&root);
        let data = s.path(&format!("member-data-{}", i + 1));
        std::fs::create_dir_all(&data).unwrap();
        let (api_port, _, _) = member(i);
        let (id, node, api_s) = ((i + 1).to_string(), i.to_string(), api_port.to_string());
        let mut args = vec![
            "--role",
            "both",
            "--node",
            &node,
            "--members",
            &members,
            "--member-id",
            &id,
            "--api-port",
            &api_s,
            "--size-gib",
            "1",
            "--data-dir",
            data.to_str().unwrap(),
        ];
        args.extend(extra);
        (root.clone(), install(&root, &args))
    };
    let (first, r) = install_member(0, &["--cluster", file.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.out);
    let first_pki = first.join("etc/hyperstor/pki");
    let bin = first.join("usr/local/bin");
    procs.0.push(start(
        &bin.join("hyperstor-meta"),
        env_args(&first, s.run(), "meta.env"),
        s.path("member-1.log"),
    ));
    let api = format!("127.0.0.1:{}", member(0).0);
    wait_api(&bin, &api, &first_pki);

    // Without --node, a storage role is refused before anything is sent.
    let tok = token(&bin, &api, &first_pki, &s, "meta", &[]);
    let lone = s.path("lone");
    link_binaries(&lone);
    let r = install_cmd(
        &lone,
        "install",
        &[
            "--role",
            "both",
            "--members",
            &members,
            "--member-id",
            "2",
            "--join",
            &api,
            "--token-file",
            tok.to_str().unwrap(),
        ],
    );
    assert!(r.code != 0 && r.has("--node N is needed"), "{}", r.out);

    // The token wasn't burnt: the second host joins with it.
    let (second, r) = install_member(1, &["--join", &api, "--token-file", tok.to_str().unwrap()]);
    assert!(r.has("Joined cluster 13"), "{}", r.out);
    let pki = second.join("etc/hyperstor/pki");
    assert!(
        !pki.join("root.key").exists(),
        "the root key stays on the first host"
    );
    for f in ["root.crt", "intermediate.crt", "intermediate.key"] {
        assert_eq!(
            std::fs::read(pki.join(f)).unwrap(),
            std::fs::read(first_pki.join(f)).unwrap(),
            "{f}"
        );
    }
    let who =
        hyperstor_pki::identity_in(&std::fs::read_to_string(pki.join("api.crt")).unwrap()).unwrap();
    assert_eq!(
        (who.role, who.id),
        (hyperstor_pki::Role::Meta, "2".to_string())
    );
    // Both services: the member, and node 1 of the file, reaching every
    // member.
    let etc = second.join("etc/hyperstor");
    assert!(etc.join("meta.env").exists());
    let node = std::fs::read_to_string(etc.join("node-1.env")).unwrap();
    assert!(node.contains(&format!("127.0.0.1:{n1}")), "{node}");
    for i in 0..3 {
        assert!(
            node.contains(&format!("127.0.0.1:{}", member(i).1)),
            "{node}"
        );
    }
    // Run again without a token: nothing to join, nothing changes.
    let (_, again) = install_member(1, &[]);
    assert_eq!(again.code, 0, "{}", again.out);
    assert!(!again.has("Joined cluster"), "{}", again.out);
}

/// D288: a storage host that joined with a token becomes a map service
/// member too (`--role both`), refused at an even count unless forced,
/// and adds itself once its map service runs; a member host that also
/// runs a node of the cluster file becomes storage only (`--role
/// storage`): its store backed up, its node given a certificate of its
/// own, out of the group, its member files, keys and store gone; its node
/// serves on. The member demoted is the one the promoted host joined
/// through, the leader as it happens: its node joins through itself, and
/// the promoted host's next run moves to another member's API. A demote
/// that would leave fewer than three voters is refused.
#[test]
fn a_storage_host_is_promoted_and_a_member_demoted() {
    let _one = one_at_a_time();
    let s = Scratch::new("roles");
    let base = free_ports(22);
    let member = |i: u16| (base + 3 * i, base + 3 * i + 1, base + 3 * i + 2); // api, map, raft
    let node_port = |i: u16| base + 9 + i; // nodes 0..2 of the file, 3 joined
    let admin_port = |i: u16| base + 13 + i;
    let (p_api, p_map, p_raft) = (base + 17, base + 18, base + 19);
    let members: String = (0..3)
        .map(|i| {
            let (_, m, r) = member(i);
            format!("{}=127.0.0.1:{r}/127.0.0.1:{m}", i + 1)
        })
        .collect::<Vec<_>>()
        .join(",");
    let file = s.path("cluster.txt");
    std::fs::write(
        &file,
        format!(
            "cluster 14\nnode 0 rack 0 127.0.0.1:{}\nnode 1 rack 1 127.0.0.1:{}\nnode 2 rack 2 127.0.0.1:{}\n\
             pool 1 vol replicated size 2 min 1 pgs 16 domain host\n",
            node_port(0),
            node_port(1),
            node_port(2)
        ),
    )
    .unwrap();
    let mut procs = Procs::default();
    let roots: Vec<PathBuf> = (1..=3).map(|i| s.path(&format!("member-{i}"))).collect();
    let pkis: Vec<PathBuf> = roots.iter().map(|r| r.join("etc/hyperstor/pki")).collect();
    let apis: Vec<String> = (0..3)
        .map(|i| format!("127.0.0.1:{}", member(i).0))
        .collect();
    let bin = roots[0].join("usr/local/bin");
    let install_member = |i: u16, extra: &[&str]| {
        let root = &roots[i as usize];
        link_binaries(root);
        let data = s.path(&format!("member-data-{}", i + 1));
        std::fs::create_dir_all(&data).unwrap();
        let (api_port, _, _) = member(i);
        let (id, node, api_s, adm) = (
            (i + 1).to_string(),
            i.to_string(),
            api_port.to_string(),
            admin_port(i).to_string(),
        );
        let mut args = vec![
            "--role",
            "both",
            "--node",
            &node,
            "--members",
            &members,
            "--member-id",
            &id,
            "--api-port",
            &api_s,
            "--admin-port",
            &adm,
            "--size-gib",
            "1",
            "--data-dir",
            data.to_str().unwrap(),
        ];
        args.extend(extra);
        install(root, &args).ok()
    };
    let start_meta = |i: usize| {
        start(
            &bin.join("hyperstor-meta"),
            env_args(&roots[i], &s.run_of(&roots[i]), "meta.env"),
            s.path(&format!("meta-{}.log", i + 1)),
        )
    };
    let start_node = |root: &Path, n: u32, log: &str| {
        start(
            &bin.join("hyperstord"),
            env_args(root, s.run(), &format!("node-{n}.env")),
            s.path(log),
        )
    };
    install_member(0, &["--cluster", file.to_str().unwrap()]);
    // Each member's map service and node on its own: any may be stopped.
    let mut metas: Vec<Procs> = vec![Procs(vec![start_meta(0)])];
    wait_api(&bin, &apis[0], &pkis[0]);
    for i in 1..3u16 {
        let tok = token(&bin, &apis[0], &pkis[0], &s, "meta", &[]);
        install_member(
            i,
            &["--join", &apis[0], "--token-file", tok.to_str().unwrap()],
        );
    }
    metas.push(Procs(vec![start_meta(1)]));
    metas.push(Procs(vec![start_meta(2)]));
    // A token from the leader, whichever member that is.
    let leader_token = |role: &str| {
        wait_for("the leader to make a token", 40, || {
            (0..3).find_map(|i| {
                let r = ctl(
                    &bin,
                    &apis[i],
                    &pkis[i],
                    &["token", "create", "--role", role],
                );
                (r.code == 0).then(|| {
                    let f = s.path(&format!("token-{role}-{}", rand_suffix()));
                    std::fs::write(&f, r.out.lines().next().unwrap()).unwrap();
                    (apis[i].clone(), f)
                })
            })
        })
    };
    let (lead, t) = leader_token("node");
    let host = s.path("store");
    join_storage(
        &host,
        &s.path("store-data"),
        &lead,
        &t,
        node_port(3),
        1,
        admin_port(3),
    )
    .ok();
    let mut nodes: Vec<Procs> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| Procs(vec![start_node(root, i as u32, &format!("node-{i}.log"))]))
        .collect();
    procs.0.push(start_node(&host, 3, "node-3.log"));
    // On a failure, the members' and nodes' logs say why.
    let logs = || {
        let mut out = format!("ports from {base}: members' APIs {apis:?}, promoted {p_api}\n");
        for e in std::fs::read_dir(s.path(""))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".log") {
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(8).collect();
                out += &format!(
                    "--- {name}:\n{}\n",
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                );
            }
        }
        out
    };
    let voters = |api: &str, pki: &Path| -> Vec<String> {
        let r = ctl(&bin, api, pki, &["member", "list"]);
        assert_eq!(
            r.code,
            0,
            "member list through {api}: {}\n{}",
            r.out,
            logs()
        );
        r.out
            .lines()
            .filter(|l| l.contains(" voter") || l.contains(" leader"))
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .collect()
    };
    wait_for("nodes up and health ok", 60, || {
        ctl(&bin, &apis[0], &pkis[0], &["status"])
            .has("health  ok")
            .then_some(())
    });
    ctl(&bin, &apis[0], &pkis[0], &["io-test", "--mib", "4"]).ok();

    // Promote: refused at four voters unless forced, then member 4.
    let (lead, t) = leader_token("meta");
    let promote = |extra: &[&str]| {
        let (a, m, r) = (p_api.to_string(), p_map.to_string(), p_raft.to_string());
        let mut args = vec![
            "--role",
            "both",
            "--token-file",
            t.to_str().unwrap(),
            "--api-port",
            &a,
            "--map-port",
            &m,
            "--raft-port",
            &r,
        ];
        args.extend(extra);
        install_cmd(&host, "install", &args)
    };
    let r = promote(&[]);
    assert!(
        r.code == 1 && r.has("4 voters survive no more failures"),
        "{}",
        r.out
    );
    let r = promote(&["--force"]).ok();
    assert!(r.has("joins as map service member 4"), "{}", r.out);
    assert!(
        !r.has("Added member 4") && r.has("member 4 isn't a voter yet"),
        "not added before its map service runs: {}",
        r.out
    );
    assert_eq!(voters(&apis[0], &pkis[0]).len(), 3);
    let hpki = host.join("etc/hyperstor/pki");
    let who = hyperstor_pki::identity_in(&std::fs::read_to_string(hpki.join("api.crt")).unwrap())
        .unwrap();
    assert_eq!(
        (who.role, who.id),
        (hyperstor_pki::Role::Meta, "4".to_string())
    );
    assert!(
        hpki.join("node.crt").exists(),
        "its node keeps its certificate"
    );
    let env = std::fs::read_to_string(host.join("etc/hyperstor/node-3.env")).unwrap();
    assert!(
        env.contains(&format!("127.0.0.1:{}", node_port(3))),
        "{env}"
    );
    let _ = lead;
    procs.0.push(start(
        &bin.join("hyperstor-meta"),
        env_args(&host, s.run(), "meta.env"),
        s.path("meta-4.log"),
    ));
    // The next run adds it, once its map service answers.
    let r = install(&host, &[]).ok();
    assert!(r.has("Member 4 is a voter."), "{}", r.out);
    let conf = std::fs::read_to_string(host.join("etc/hyperstor/install.conf")).unwrap();
    assert!(!conf.contains("member_pending"), "{conf}");
    assert_eq!(voters(&apis[0], &pkis[0]).len(), 4);

    // Demote the member whose API the promoted host joined through, the
    // leader as it happens (tokens and joins are the leader's): its node's
    // join goes through itself (D288).
    let conf = std::fs::read_to_string(host.join("etc/hyperstor/install.conf")).unwrap();
    let joined = conf
        .lines()
        .find_map(|l| l.strip_prefix("join = "))
        .unwrap()
        .to_string();
    let t = apis
        .iter()
        .position(|a| *a == joined)
        .expect("joined through a member");
    let (id, n) = (t + 1, t);
    // A member that stays, for the calls after.
    let a = (t + 1) % 3;
    // The root CA's key stays with no member that leaves: the first has
    // it, so it goes first, backed up (as 19-administration.md §23.4.3
    // asks).
    if roots[t].join("etc/hyperstor/pki/root.key").exists() {
        let refused = install_cmd(&roots[t], "install", &["--role", "storage"]);
        assert!(
            refused.code == 1 && refused.has("root CA's key"),
            "{}",
            refused.out
        );
        let pass = s.path("passphrase");
        std::fs::write(&pass, "a passphrase long enough for the test\n").unwrap();
        let out = s.path("root.backup");
        install_cmd(
            &roots[t],
            "root-backup",
            &[
                "--out",
                out.to_str().unwrap(),
                "--passphrase-file",
                pass.to_str().unwrap(),
                "--remove-key",
            ],
        )
        .ok();
    }
    // Only leaders record their API: name another member's.
    let r = install(&roots[t], &["--role", "storage", "--join", &apis[a]]).ok();
    assert!(
        r.has(&format!("Demoted: this host is storage node {n} only")),
        "{}",
        r.out
    );
    let etc = roots[t].join("etc/hyperstor");
    for gone in [
        "meta.env",
        "mgmt.env",
        "cluster.txt",
        "pki/api.crt",
        "pki/admin.key",
        "pki/intermediate.key",
        "pki/cluster.key",
    ] {
        assert!(!etc.join(gone).exists(), "{gone} is gone");
    }
    // The installer's paths are under its root; the processes' (the store
    // a running member writes) are the real ones.
    let data = s.path(&format!("member-data-{id}"));
    let data = roots[t].join(data.strip_prefix("/").unwrap());
    assert!(
        std::fs::read_dir(data.join("meta-backups"))
            .unwrap()
            .flatten()
            .any(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("demoted-member-{id}-"))),
        "the store backed up first"
    );
    let who =
        hyperstor_pki::identity_in(&std::fs::read_to_string(etc.join("pki/node.crt")).unwrap())
            .unwrap();
    assert_eq!(
        (who.role, who.id),
        (hyperstor_pki::Role::Node, n.to_string())
    );
    let conf = std::fs::read_to_string(etc.join("install.conf")).unwrap();
    assert!(
        conf.contains("role = storage") && conf.contains(&format!("node = {n}")),
        "{conf}"
    );
    let env = std::fs::read_to_string(etc.join(format!("node-{n}.env"))).unwrap();
    assert!(
        env.contains(&format!("127.0.0.1:{}", node_port(n as u16))),
        "the same device: {env}"
    );
    let mut left = vec!["1", "2", "3", "4"];
    left.retain(|m| *m != id.to_string());
    assert_eq!(voters(&apis[a], &pkis[a]), left, "member {id} left");
    for mut p in metas[t].0.drain(..) {
        let _ = p.kill();
        let _ = p.wait();
    }
    // Its node restarts with the members it reaches now, and serves.
    for mut p in nodes[t].0.drain(..) {
        let _ = p.kill();
        let _ = p.wait();
    }
    nodes[t]
        .0
        .push(start_node(&roots[t], n as u32, &format!("node-{n}b.log")));
    wait_for("health ok again", 60, || {
        ctl(&bin, &apis[a], &pkis[a], &["status"])
            .has("health  ok")
            .then_some(())
    });
    ctl(&bin, &apis[a], &pkis[a], &["io-test", "--mib", "4"]).ok();
    // The promoted host joined through the member that left: its next run
    // reaches the cluster through another.
    let r = install(&host, &[]).ok();
    assert!(r.has("Reaching the cluster through"), "{}", r.out);
    let conf = std::fs::read_to_string(host.join("etc/hyperstor/install.conf")).unwrap();
    assert!(!conf.contains(&format!("join = {joined}")), "{conf}");
    // Three voters left: none can go without force.
    let r = install_cmd(&roots[a], "install", &["--role", "storage"]);
    assert!(r.code == 1 && r.has("2 voters would be left"), "{}", r.out);
    assert!(
        roots[a].join("etc/hyperstor/meta.env").exists(),
        "nothing changed"
    );
}
