//! Joining hosts with tokens (administration.md §23.4.2, decisions.md
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
        // As the harness runs storage nodes: no spinning on shared CPUs.
        args.extend(["--spin-us".into(), "0".into()]);
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
    let o = Command::new(bin.join("hyperstorctl"))
        .args(["--api", api, "--tls", pki.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    }
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
/// host's certificate names its node and is renewed over the API.
#[test]
fn storage_hosts_join_with_tokens() {
    let _one = one_at_a_time();
    let s = Scratch::new("join");
    let base = free_ports(24);
    let (api_port, map_port) = (base, base + 1);
    let file = s.path("cluster.txt");
    // No storage node yet: they join.
    std::fs::write(
        &file,
        "cluster 11\npool 1 vol replicated size 3 min 2 pgs 32 domain host\n",
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
            for f in ["root.crt", "intermediate.crt", "intermediate.key"] {
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
            env_args(&root, s.run(), "meta.env"),
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
    // Every member lists the tokens: they are replicated.
    for i in 0..3 {
        let r = ctl(&bin, &apis[i], &pkis[i], &["token", "list"]).ok();
        assert!(r.has("used"), "member {}: {}", i + 1, r.out);
    }

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
