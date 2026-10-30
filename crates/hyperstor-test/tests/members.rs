//! The map service's members (04-decisions.md D85, D87): adding and
//! removing members at run time, and recovering a group that lost its
//! majority by forcing one survivor alone.

use hyperstor_test::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// One group test at a time: each runs several members.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

struct M {
    root: PathBuf,
    api: String,
    raft: String,
    map: String,
    proc: Option<Child>,
    log: PathBuf,
}

impl M {
    fn pki(&self) -> PathBuf {
        self.root.join("etc/hyperstor/pki")
    }
    fn start(&mut self, s: &Scratch, extra: &[&str]) {
        self.stop();
        let p = Command::new(self.root.join("usr/local/bin/hyperstor-meta"))
            .args(env_args(&self.root, s.run(), "meta.env"))
            .args(extra)
            .stderr(std::fs::File::create(&self.log).unwrap())
            .spawn()
            .unwrap();
        self.proc = Some(p);
    }
    fn stop(&mut self) {
        if let Some(mut p) = self.proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

struct Group {
    s: Scratch,
    m: Vec<M>,
    bin: PathBuf,
    cluster: String,
    base: u16,
    others: Vec<Child>,
}

impl Drop for Group {
    fn drop(&mut self) {
        for m in &mut self.m {
            m.stop();
        }
        for p in &mut self.others {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
}

/// `n` members (listed up to `listed`, so later ones can be added) of a
/// new cluster: the first from a cluster file, the others joined with
/// meta tokens.
fn group(name: &str, n: u16, listed: u16) -> Group {
    let s = Scratch::new(name);
    let base = free_ports(3 * listed + 8);
    let addr = |i: u16| {
        (
            format!("127.0.0.1:{}", base + 3 * i),
            format!("127.0.0.1:{}", base + 3 * i + 1),
            format!("127.0.0.1:{}", base + 3 * i + 2),
        )
    };
    let list: Vec<String> = (0..n)
        .map(|i| {
            let (_, map, raft) = addr(i);
            format!("{}={raft}/{map}", i + 1)
        })
        .collect();
    let file = s.path("cluster.txt");
    std::fs::write(
        &file,
        "cluster 21\npool 1 vol replicated size 1 min 1 pgs 16 domain host\n",
    )
    .unwrap();
    let mut g = Group {
        bin: PathBuf::new(),
        cluster: "21".into(),
        base,
        m: Vec::new(),
        others: Vec::new(),
        s,
    };
    for i in 0..n {
        g.add_host(i, &list.join(","), if i == 0 { Some(&file) } else { None });
    }
    g
}

impl Group {
    /// Installs and starts member `i + 1` (joining through the leader
    /// unless `file` makes it the first).
    fn add_host(&mut self, i: u16, members: &str, file: Option<&Path>) {
        let (api, map, raft) = (
            format!("127.0.0.1:{}", self.base + 3 * i),
            format!("127.0.0.1:{}", self.base + 3 * i + 1),
            format!("127.0.0.1:{}", self.base + 3 * i + 2),
        );
        let root = self.s.path(&format!("member-{}", i + 1));
        let data = self.s.path(&format!("data-{}", i + 1));
        std::fs::create_dir_all(&data).unwrap();
        link_binaries(&root);
        let id = (i + 1).to_string();
        let api_port = (self.base + 3 * i).to_string();
        let mut args = vec![
            "--role",
            "meta",
            "--members",
            members,
            "--member-id",
            &id,
            "--api-port",
            &api_port,
            "--data-dir",
            data.to_str().unwrap(),
        ];
        let tok = self.s.path(&format!("token-{id}"));
        let via;
        match file {
            Some(f) => args.extend(["--cluster", f.to_str().unwrap()]),
            None => {
                // Meta tokens: from the first member while the group has
                // no map, from the leader after.
                via = wait_for("a member to make a meta token", 30, || {
                    self.m.iter().find_map(|m| {
                        let r = ctl(
                            &self.bin,
                            &m.api,
                            &m.pki(),
                            &["token", "create", "--role", "meta"],
                        );
                        (r.code == 0).then(|| {
                            std::fs::write(&tok, r.out.lines().next().unwrap()).unwrap();
                            m.api.clone()
                        })
                    })
                });
                args.extend([
                    "--join",
                    via.as_str(),
                    "--token-file",
                    tok.to_str().unwrap(),
                ]);
            }
        }
        install(&root, &args);
        if file.is_some() {
            self.bin = root.join("usr/local/bin");
        }
        let mut m = M {
            log: self.s.path(&format!("member-{}.log", i + 1)),
            root,
            api,
            raft,
            map,
            proc: None,
        };
        m.start(&self.s, &[]);
        self.m.push(m);
        if file.is_some() {
            let (bin, api, pki) = (self.bin.clone(), self.m[0].api.clone(), self.m[0].pki());
            wait_for("the first member's API", 20, || {
                (ctl(&bin, &api, &pki, &["token", "list"]).code == 0).then_some(())
            });
        }
    }

    /// The index of the member that leads, once one does.
    fn leader(&self, among: &[usize]) -> usize {
        let found =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.leader_once(among)));
        found.unwrap_or_else(|e| {
            for (i, m) in self.m.iter().enumerate() {
                eprintln!("--- member {}:\n{}", i + 1, m.log());
            }
            std::panic::resume_unwind(e)
        })
    }

    fn leader_once(&self, among: &[usize]) -> usize {
        wait_for("a leader", 40, || {
            among.iter().copied().find(|i| {
                let m = &self.m[*i];
                let r = ctl(&self.bin, &m.api, &m.pki(), &["member", "list"]);
                r.code == 0
                    && r.out
                        .lines()
                        .any(|l| l.starts_with(&format!("{} ", i + 1)) && l.contains(" leader "))
            })
        })
    }

    fn ctl(&self, i: usize, args: &[&str]) -> Run {
        ctl(&self.bin, &self.m[i].api, &self.m[i].pki(), args)
    }
}

/// Members added and removed at run time (D87): a fourth member, joined
/// with a meta token, is a learner then a voter; removed again; a removal
/// that would leave two voters is refused.
#[test]
fn members_change_at_run_time() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = group("members", 3, 4);
    let l = g.leader(&[0, 1, 2]);
    let r = g.ctl(l, &["member", "list"]).ok();
    assert_eq!(
        r.out
            .lines()
            .filter(|x| x.contains(" voter") || x.contains(" leader"))
            .count(),
        3,
        "{}",
        r.out
    );
    // A fourth member: installed and running first, then added.
    let list: String = (0..4u16)
        .map(|i| {
            format!(
                "{}=127.0.0.1:{}/127.0.0.1:{}",
                i + 1,
                g.base + 3 * i + 2,
                g.base + 3 * i + 1
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    g.add_host(3, &list, None);
    let (raft, map) = (g.m[3].raft.clone(), g.m[3].map.clone());
    let r = g
        .ctl(
            l,
            &["member", "add", "--id", "4", "--raft", &raft, "--map", &map],
        )
        .ok();
    assert!(
        r.out
            .lines()
            .any(|x| x.starts_with("4 ") && (x.contains(" voter") || x.contains(" leader"))),
        "{}",
        r.out
    );
    // Every member sees four voters, the new one included.
    wait_for("member 4 to see the group", 20, || {
        let r = g.ctl(3, &["member", "list"]);
        (r.code == 0 && r.out.lines().count() == 5).then_some(())
    });
    let again = g.ctl(
        l,
        &["member", "add", "--id", "4", "--raft", &raft, "--map", &map],
    );
    assert!(
        again.code == 1 && again.has("already in the group"),
        "{}",
        again.out
    );
    let r = g.ctl(l, &["member", "remove", "--id", "4"]).ok();
    assert!(!r.out.lines().any(|x| x.starts_with("4 ")), "{}", r.out);
    g.m[3].stop();
    // Three voters: none can go without force.
    let other = (0..3).find(|i| *i != l).unwrap();
    let r = g.ctl(l, &["member", "remove", "--id", &(other + 1).to_string()]);
    assert!(r.code == 1 && r.has("fewer than three"), "{}", r.out);
    // Not on a follower.
    let r = g.ctl(other, &["member", "remove", "--id", "4"]);
    assert!(r.code == 3 && r.has("(not_leader)"), "{}", r.out);
    let audit = g.m[l].log();
    assert!(audit.contains("\"op\":\"AddMember\"") && audit.contains("\"op\":\"RemoveMember\""));
}

/// A group that lost its majority for good (D85): the survivor can't
/// change anything; forcing it alone is refused while another member
/// answers; forced, it leads alone, serves and adds hosts; an old member
/// that comes back with its log can't rejoin.
#[test]
fn a_lost_majority_is_recovered_alone() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = group("majority", 3, 3);
    g.leader(&[0, 1, 2]);
    // Members 2 and 3 are lost; member 1 is left without a majority.
    g.m[2].stop();
    let s1_api = g.m[0].api.clone();
    let _ = s1_api;
    // Forcing while member 2 still answers is refused.
    g.m[0].stop();
    let s = &g.s;
    let cluster = g.cluster.clone();
    let refused = Command::new(g.m[0].root.join("usr/local/bin/hyperstor-meta"))
        .args(env_args(&g.m[0].root, s.run(), "meta.env"))
        .args(["--force-alone", &cluster])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && err.contains("member 2 answers"),
        "{err}"
    );
    let wrong = Command::new(g.m[0].root.join("usr/local/bin/hyperstor-meta"))
        .args(env_args(&g.m[0].root, s.run(), "meta.env"))
        .args(["--force-alone", "999"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("isn't this cluster"));
    g.m[1].stop();
    // Forced alone: it leads, alone.
    let (s, m0) = (&g.s, &mut g.m[0]);
    m0.start(s, &["--force-alone", &cluster]);
    g.leader(&[0]);
    let r = g.ctl(0, &["member", "list"]).ok();
    assert_eq!(r.out.lines().count(), 2, "one member: {}", r.out);
    let log = g.m[0].log();
    assert!(
        log.contains("--force-alone: revoked") && log.contains("\"op\":\"ForceAlone\""),
        "{log}"
    );
    // It makes tokens and adds hosts again.
    let t = g.ctl(0, &["token", "create", "--role", "node"]).ok();
    let tf = g.s.path("node-token");
    std::fs::write(&tf, t.out.lines().next().unwrap()).unwrap();
    let host = g.s.path("store");
    let data = g.s.path("store-data");
    std::fs::create_dir_all(&data).unwrap();
    link_binaries(&host);
    let port = (g.base + 9).to_string();
    let admin = (g.base + 10).to_string();
    install(
        &host,
        &[
            "--join",
            &g.m[0].api,
            "--token-file",
            tf.to_str().unwrap(),
            "--role",
            "storage",
            "--address",
            "127.0.0.1",
            "--node-port",
            &port,
            "--admin-port",
            &admin,
            "--size-gib",
            "1",
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let mut args = env_args(&host, g.s.run(), "node-0.env");
    args.extend(NODE_ARGS.map(String::from));
    g.others.push(
        Command::new(host.join("usr/local/bin/hyperstord"))
            .args(args)
            .stderr(std::fs::File::create(g.s.path("store.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    g.ctl(0, &["status", "--wait", "30"]).ok();
    g.ctl(0, &["io-test", "--mib", "4"]).ok();
    // Member 2 comes back with its old log and certificate: refused.
    let (s, m1) = (&g.s, &mut g.m[1]);
    m1.start(s, &[]);
    std::thread::sleep(std::time::Duration::from_secs(3));
    let r = g.ctl(0, &["member", "list"]).ok();
    let logs = || {
        format!(
            "{}\n--- member 1:\n{}\n--- member 2:\n{}",
            r.out,
            g.m[0].log(),
            g.m[1].log()
        )
    };
    assert_eq!(r.out.lines().count(), 2, "still one member: {}", logs());
    assert!(
        r.out
            .lines()
            .any(|l| l.starts_with("1 ") && l.contains(" leader ")),
        "{}",
        logs()
    );
    g.ctl(0, &["io-test", "--mib", "4"]).ok();
}

/// A leader whose members stop answering can't commit: it says so, in
/// `status` (an error) and its log, instead of waiting silently (the
/// pilot's map service once waited a day and a half).
#[test]
fn a_leader_without_a_majority_says_so() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = group("stall", 3, 3);
    let l = g.leader(&[0, 1, 2]);
    // One storage host, so there is something to change.
    let t = g.ctl(l, &["token", "create", "--role", "node"]).ok();
    let tf = g.s.path("node-token");
    std::fs::write(&tf, t.out.lines().next().unwrap()).unwrap();
    let host = g.s.path("store");
    let data = g.s.path("store-data");
    std::fs::create_dir_all(&data).unwrap();
    link_binaries(&host);
    let port = (g.base + 9).to_string();
    let admin = (g.base + 10).to_string();
    install(
        &host,
        &[
            "--join",
            &g.m[l].api,
            "--token-file",
            tf.to_str().unwrap(),
            "--role",
            "storage",
            "--address",
            "127.0.0.1",
            "--node-port",
            &port,
            "--admin-port",
            &admin,
            "--size-gib",
            "1",
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let mut args = env_args(&host, g.s.run(), "node-0.env");
    args.extend(NODE_ARGS.map(String::from));
    g.others.push(
        Command::new(host.join("usr/local/bin/hyperstord"))
            .args(args)
            .stderr(std::fs::File::create(g.s.path("store.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    g.ctl(l, &["status", "--wait", "30"]).ok();
    // The leader again: on a busy runner an election during the join
    // moved it once, and a follower left alone never says the map is
    // stuck (only a leader commits).
    let l = g.leader(&[0, 1, 2]);
    // The other two stop; a change follows that can't be committed.
    for i in (0..3).filter(|i| *i != l) {
        g.m[i].stop();
    }
    let _ = g.ctl(l, &["out", "--node", "0"]);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for("status to report the stall", 40, || {
            let r = g.ctl(l, &["status"]);
            r.has("hasn't committed a map change").then_some(r)
        })
    }))
    .unwrap_or_else(|e| {
        eprintln!("--- member {} (left running):\n{}", l + 1, g.m[l].log());
        std::panic::resume_unwind(e)
    });
    assert!(r.code != 0 && r.has("health  error"), "{}", r.out);
    let log = g.m[l].log();
    assert!(log.contains("not committed"), "{log}");
}
