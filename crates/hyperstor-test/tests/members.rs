//! The map service's members (04-decisions.md D85, D87): adding and
//! removing members at run time, and recovering a group that lost its
//! majority by forcing one survivor alone.

use hyperstor_test::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// One group test at a time: each runs several members.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ctl(bin: &Path, api: &str, pki: &Path, args: &[&str]) -> Run {
    ctl_at(bin, api, pki, args)
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
            .args(env_args(&self.root, &s.run_of(&self.root), "meta.env"))
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
    let (mut g, list, file) = unstarted(name, n, listed);
    for i in 0..n {
        g.add_host(i, &list, if i == 0 { Some(&file) } else { None });
    }
    g
}

/// A group of `n` members, none installed yet: the group, its `--members`
/// list and its cluster file.
fn unstarted(name: &str, n: u16, listed: u16) -> (Group, String, PathBuf) {
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
    let g = Group {
        bin: PathBuf::new(),
        cluster: "21".into(),
        base,
        m: Vec::new(),
        others: Vec::new(),
        s,
    };
    (g, list.join(","), file)
}

impl Group {
    /// Installs and starts member `i + 1` (joining through the leader
    /// unless `file` makes it the first).
    fn add_host(&mut self, i: u16, members: &str, file: Option<&Path>) {
        let mut m = self.install_host(i, members, file);
        m.start(&self.s, &[]);
        self.m.push(m);
        if file.is_some() {
            let (bin, api, pki) = (self.bin.clone(), self.m[0].api.clone(), self.m[0].pki());
            wait_for("the first member's API", 20, || {
                (ctl(&bin, &api, &pki, &["token", "list"]).code == 0).then_some(())
            });
        }
    }

    /// Installs member `i + 1`, as [`Group::add_host`] does, without
    /// starting it.
    fn install_host(&mut self, i: u16, members: &str, file: Option<&Path>) -> M {
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
        M {
            log: self.s.path(&format!("member-{}.log", i + 1)),
            root,
            api,
            raft,
            map,
            proc: None,
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

    /// Waits until member `i + 1`'s own store holds the map: a leader is
    /// elected before its first map is applied, and until then the member
    /// makes no tokens and can't be forced alone (on a slow host, seconds
    /// later; CI). A map backup reads the member's store, and fails until
    /// then.
    fn formed(&self, i: usize) {
        let mut n = 0;
        wait_for("the map in the member's store", 40, || {
            n += 1;
            let out = self.s.path(&format!("formed-{i}-{n}"));
            let r = self.ctl(i, &["map", "backup", "--out", out.to_str().unwrap()]);
            let _ = std::fs::remove_file(&out);
            (r.code == 0).then_some(())
        });
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

/// Each member's Raft state in the leader's member list (D281): in step;
/// stopped, no reply, and behind by the writes since; started again, in
/// step. Another member's list has no state. The leader stopped, the next
/// one records the change in the audit (I336).
#[test]
fn the_member_list_shows_a_stopped_member() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = group("health", 3, 3);
    let l = g.leader(&[0, 1, 2]);
    // A member's row by its id, in the list `i` gives.
    let row = |g: &Group, i: usize, id: usize| -> String {
        let r = g.ctl(i, &["member", "list"]);
        let row = r.out.lines().find(|x| x.starts_with(&format!("{id} ")));
        row.unwrap_or_default().to_string()
    };
    let f = (0..3).find(|i| *i != l).unwrap();
    wait_for("every member in step", 20, || {
        (0..3)
            .all(|i| row(&g, l, i + 1).contains(" in step "))
            .then_some(())
    });
    let r = g.ctl(f, &["member", "list"]).ok();
    assert!(
        !r.has("in step"),
        "a follower's list has no state:\n{}",
        r.out
    );
    let json = g.ctl(l, &["--json", "member", "list"]).ok();
    let v: serde_json::Value = serde_json::from_str(&json.out).unwrap();
    // uint64 as a number or, in protobuf's JSON, a string.
    let num = |x: &serde_json::Value| x.as_u64().or_else(|| x.as_str()?.parse().ok());
    assert!(num(&v["term"]).unwrap_or(0) > 0, "{}", json.out);
    assert!(num(&v["quorum_ack_ms"]).is_some(), "{}", json.out);
    g.m[f].stop();
    for _ in 0..3 {
        g.ctl(l, &["token", "create", "--role", "meta"]).ok();
    }
    let gone = wait_for("the stopped member to show no reply", 20, || {
        let r = row(&g, l, f + 1);
        r.contains(" no reply ").then_some(r)
    });
    let behind: u64 = gone.split_whitespace().nth(4).unwrap().parse().unwrap();
    assert!(behind >= 3, "{gone}");
    g.m[f].start(&g.s, &[]);
    wait_for("the restarted member in step", 30, || {
        let r = row(&g, l, f + 1);
        r.contains(" in step ").then_some(())
    });
    // The leader stops: the next records the change in the audit (I336).
    g.m[l].stop();
    let others: Vec<usize> = (0..3).filter(|i| *i != l).collect();
    let n = g.leader(&others);
    let log = g.m[n].log();
    let elected = log
        .lines()
        .rfind(|x| x.contains("\"op\":\"LeaderElected\""))
        .unwrap_or_else(|| panic!("no LeaderElected record:\n{log}"));
    assert!(
        elected.contains(&format!("member {} before", l + 1)),
        "{elected}"
    );
}

/// A group that lost its majority for good (D85): the survivor can't
/// change anything; forcing it alone is refused while another member
/// answers; forced, it leads alone with every change it had applied
/// before it was killed (I341), serves and adds hosts; an old member that
/// comes back with its log can't rejoin.
#[test]
fn a_lost_majority_is_recovered_alone() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = group("majority", 3, 3);
    g.leader(&[0, 1, 2]);
    g.formed(0);
    // Members 2 and 3 are lost; member 1 is left without a majority.
    g.m[2].stop();
    let s1_api = g.m[0].api.clone();
    let _ = s1_api;
    // A token made just before member 1 is killed: applied there, likely
    // not yet at a checkpoint (glidex D31), and kept through the forced
    // start, which re-applies the log first (I341).
    let ids = |out: &str| -> Vec<String> {
        out.lines()
            .skip(1)
            .filter_map(|l| l.split_whitespace().next().map(String::from))
            .collect()
    };
    let l = g.leader(&[0, 1]);
    g.ctl(l, &["token", "create", "--role", "meta"]).ok();
    let want = ids(&g.ctl(l, &["token", "list"]).ok().out);
    wait_for("member 1 to apply the token", 10, || {
        (ids(&g.ctl(0, &["token", "list"]).out) == want).then_some(())
    });
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
    g.formed(0);
    let r = g.ctl(0, &["member", "list"]).ok();
    assert_eq!(r.out.lines().count(), 2, "one member: {}", r.out);
    let kept = ids(&g.ctl(0, &["token", "list"]).ok().out);
    assert!(
        want.iter().all(|t| kept.contains(t)),
        "{want:?} in {kept:?}"
    );
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

/// A scrub started through a follower goes on to the leader, at the API
/// the leader recorded when it began to lead; the member list names it.
#[test]
fn a_scrub_started_on_a_follower_goes_to_the_leader() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let g = group("scrubleader", 3, 3);
    let l = g.leader(&[0, 1, 2]);
    let f = (l + 1) % 3;
    wait_for("the leader's API in the member list", 30, || {
        let r = g.ctl(f, &["member", "list"]);
        (r.code == 0
            && r.out
                .lines()
                .any(|x| x.contains(" leader ") && x.ends_with(&g.m[l].api)))
        .then_some(())
    });
    let r = g.ctl(f, &["scrub", "start", "--pool", "1"]).ok();
    assert!(
        r.has("isn't the leader; asking the leader at") && r.has("scrubbing 16 PG(s)"),
        "{}",
        r.out
    );
    assert!(!r.has("tokens"), "{}", r.out);
    let ops = g.ctl(l, &["op", "list"]).ok();
    assert!(ops.has("pool 1 scrub"), "{}", ops.out);
    // So do S3's placements, which the leader writes (I287).
    let set = [
        "s3",
        "placement",
        "set",
        "--name",
        "default",
        "--data-pool",
        "vol",
        "--index-pool",
        "vol",
    ];
    let r = g.ctl(f, &set).ok();
    assert!(
        r.has("isn't the leader; asking the leader at") && r.has("S3 placement default"),
        "{}",
        r.out
    );
    let r = g.ctl(f, &["s3", "placement", "list"]).ok();
    assert!(r.has("vol (#1)"), "{}", r.out);
}

/// A follower host's mgmt, given only that member's API as the installer
/// writes it, sends its calls to the leader (D276): the first
/// administrator's claim, their account and their session, all rows the
/// leader writes, go through it.
#[test]
fn a_followers_mgmt_reaches_the_leader() {
    use std::io::{Read, Write};
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = group("mgmtleader", 3, 3);
    let l = g.leader(&[0, 1, 2]);
    let f = (l + 1) % 3;
    wait_for("the leader's API in the member list", 30, || {
        let r = g.ctl(f, &["member", "list"]);
        (r.code == 0
            && r.out
                .lines()
                .any(|x| x.contains(" leader ") && x.ends_with(&g.m[l].api)))
        .then_some(())
    });
    start_authd(&g.s.run().join("auth.sock"));
    let port = free_ports(1);
    let args: Vec<String> = env_args(&g.m[f].root, g.s.run(), "mgmt.env")
        .into_iter()
        .map(|a| a.replace("/run/hyperstor-authd", g.s.run().to_str().unwrap()))
        .map(|a| {
            if a.starts_with("0.0.0.0:") {
                format!("127.0.0.1:{port}")
            } else {
                a
            }
        })
        .collect();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--api" && w[1] == g.m[f].api),
        "{args:?}"
    );
    let log = g.s.path("mgmt.log");
    let p = Command::new(g.m[f].root.join("usr/local/bin/hyperstor-mgmt"))
        .args(&args)
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    g.others.push(p);
    let text = || std::fs::read_to_string(&log).unwrap_or_default();
    let claim = wait_for("the claim code", 30, || {
        let t = text();
        let rest = t.split("claim code ").nth(1)?;
        Some(rest.split_whitespace().next()?.to_string())
    });
    assert!(
        text().contains(&format!("go to {} first", g.m[l].api)),
        "{}",
        text()
    );
    let root = std::fs::read_to_string(g.m[f].pki().join("root.crt")).unwrap();
    let body =
        serde_json::json!({ "user": "ann", "password": PASSWORD, "claim": claim }).to_string();
    let req = format!(
        "POST /auth/login HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let config = hyperstor_pki::web_client_config(Some(&root)).unwrap();
    let name =
        rustls::pki_types::ServerName::IpAddress(std::net::IpAddr::from([127, 0, 0, 1]).into());
    let conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
    let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    tls.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    let answer = String::from_utf8_lossy(&raw).into_owned();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}\n{}", text());
    assert!(answer.contains("security-admin"), "{answer}");
}

/// Members restarted one at a time, in the installer's order (3, 2, 1),
/// keep committing: after each restart the leader takes a write within
/// seconds, and the restarted member catches up (issue #177, I279).
#[test]
fn a_rolling_restart_of_the_members_keeps_committing() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut g = group("rolling", 3, 3);
    g.leader(&[0, 1, 2]);
    // Each member's log goes with a failure: why a member didn't start,
    // take or catch up with a write is in them (CI).
    let dump = |g: &Group| {
        for (j, m) in g.m.iter().enumerate() {
            eprintln!("--- member {}:\n{}", j + 1, m.log());
        }
    };
    // Every member running before any restarts: a leader among the first
    // two doesn't mean the third has finished its first start, and a kill
    // while it creates its store leaves one redb can't open (I292,
    // glidex#317).
    for i in 0..3 {
        let formed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.formed(i)));
        if let Err(e) = formed {
            eprintln!("member {} has no map in its store", i + 1);
            dump(&g);
            std::panic::resume_unwind(e);
        }
    }
    for i in [2, 1, 0] {
        g.m[i].stop();
        g.m[i].start(&g.s, &[]);
        let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for("a write through the leader", 15, || {
                let l = g.leader_once(&[0, 1, 2]);
                let r = g.ctl(l, &["token", "create", "--role", "meta"]);
                (r.code == 0).then_some(())
            })
        }));
        if let Err(e) = made {
            dump(&g);
            std::panic::resume_unwind(e);
        }
        // The restarted member has the write too: its token list matches
        // the leader's.
        let l = g.leader(&[0, 1, 2]);
        let want = g.ctl(l, &["token", "list"]).ok().out;
        let mut got = String::new();
        let caught_up = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for("the restarted member to catch up", 15, || {
                got = g.ctl(i, &["token", "list"]).out;
                (got == want).then_some(())
            })
        }));
        if let Err(e) = caught_up {
            eprintln!(
                "member {} restarted, member {} leads; the restarted member's token list:\n{got}\nthe leader's:\n{want}",
                i + 1,
                l + 1
            );
            dump(&g);
            std::panic::resume_unwind(e);
        }
    }
}

/// A member killed during its first start, while it creates its store,
/// starts again and catches up (I292, glidex#317): glidex-raftstore builds
/// each store file beside it and renames it into place, so a kill leaves
/// no file or a whole one, and a half-made `.new` beside it is removed.
#[test]
fn a_member_killed_in_its_first_start_starts_again() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (mut g, list, file) = unstarted("firststart", 3, 3);
    g.add_host(0, &list, Some(&file));
    g.add_host(1, &list, None);
    g.leader(&[0, 1]);
    let mut third = g.install_host(2, &list, None);
    // What a kill leaves while redb creates a file: sized, with its header
    // written but not its magic number.
    let meta = g.s.path("data-3").join("meta");
    let half_made = [meta.join("map.redb.new"), meta.join("raft/log.redb.new")];
    std::fs::create_dir_all(meta.join("raft")).unwrap();
    for f in &half_made {
        let mut bytes = vec![0u8; 1_056_768];
        bytes[8..16].copy_from_slice(&[0, 4, 0, 0, 0, 0x10, 0, 0]);
        std::fs::write(f, bytes).unwrap();
    }
    // Killed at points through its first start, the early ones while it
    // creates its store.
    for ms in [0, 1, 2, 4, 8, 16, 32, 64] {
        third.start(&g.s, &[]);
        std::thread::sleep(std::time::Duration::from_millis(ms));
        third.stop();
    }
    third.start(&g.s, &[]);
    g.m.push(third);
    let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        g.formed(2);
        let l = g.leader(&[0, 1, 2]);
        g.ctl(l, &["token", "create", "--role", "meta"]).ok();
        let want = g.ctl(l, &["token", "list"]).ok().out;
        wait_for("the third member to catch up", 15, || {
            (g.ctl(2, &["token", "list"]).out == want).then_some(())
        });
    }));
    if let Err(e) = started {
        for (j, m) in g.m.iter().enumerate() {
            eprintln!("--- member {}:\n{}", j + 1, m.log());
        }
        std::panic::resume_unwind(e);
    }
    for f in &half_made {
        assert!(!f.exists(), "{} is still there", f.display());
    }
}

/// A token asked for before the group has committed its first map waits
/// for it (I275), rather than failing with `not_formed` at once: the first
/// of three members, alone, can't elect, so it answers `not_formed` only
/// after the wait; asked again while a second member starts, it waits
/// and makes the token.
#[test]
fn a_token_waits_for_the_first_map() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (mut g, list, file) = unstarted("firstmap", 3, 3);
    g.add_host(0, &list, Some(&file));
    let t = std::time::Instant::now();
    let r = g.ctl(0, &["token", "create", "--role", "node"]);
    assert!(r.code != 0 && r.has("(not_formed)"), "{}", r.out);
    assert!(
        t.elapsed() >= std::time::Duration::from_secs(3),
        "refused after {:?}, without waiting",
        t.elapsed()
    );
    let mut second = g.install_host(1, &list, None);
    let (bin, api, pki) = (g.bin.clone(), g.m[0].api.clone(), g.m[0].pki());
    // Electing and committing the first map can outlast the server's wait
    // on a slow runner, so a call that waited it out and was refused is
    // asked again (each one must have waited, not refused at once).
    let asked = std::thread::spawn(move || {
        let t = std::time::Instant::now();
        loop {
            let one = std::time::Instant::now();
            let r = ctl(&bin, &api, &pki, &["token", "create", "--role", "node"]);
            if r.code == 0
                || !r.has("(not_formed)")
                || t.elapsed() > std::time::Duration::from_secs(60)
            {
                return (r, t.elapsed());
            }
            assert!(
                one.elapsed() >= std::time::Duration::from_secs(3),
                "refused after {:?}, without waiting",
                one.elapsed()
            );
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    second.start(&g.s, &[]);
    g.m.push(second);
    let (r, took) = asked.join().unwrap();
    assert_eq!(r.code, 0, "{}", r.out);
    assert!(r.out.starts_with("hsj1-"), "{}", r.out);
    assert!(
        took >= std::time::Duration::from_millis(500),
        "made in {took:?}, before the second member started"
    );
}
