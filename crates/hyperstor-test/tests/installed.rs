//! Checks of a host where Hyperstor is really installed
//! (`sudo hyperstor-install install …`), for test hosts and future CI
//! runners. Ignored by default; run them as root or as a member of
//! `hyperstor-admin`:
//!
//! ```sh
//! cargo build --release -p hyperstor-test --tests
//! sudo target/release/deps/installed-* --ignored   # the test binary, not cargo
//! ```
//!
//! (Running cargo itself as root leaves root-owned files in `target`.)
//!
//! They use the installed `hyperstorctl` with its installed
//! configuration, write only to new volumes, and change nothing else.

use std::process::Command;

fn ctl(args: &[&str]) -> (i32, String) {
    let o = Command::new("/usr/local/bin/hyperstorctl")
        .args(args)
        .output()
        .expect("/usr/local/bin/hyperstorctl: is Hyperstor installed?");
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

#[test]
#[ignore = "needs a host with Hyperstor installed"]
fn installed_cluster_is_healthy_and_takes_io() {
    let (code, out) = ctl(&["status", "--wait", "60"]);
    assert_eq!(code, 0, "{out}");
    let (code, pools) = ctl(&["--json", "pools"]);
    assert_eq!(code, 0, "{pools}");
    // Every pool takes I/O.
    let pools = pools.split_once("\"pools\":[").map_or("", |(_, p)| p);
    for id in pools
        .split("\"id\":")
        .skip(1)
        .filter_map(|s| s.split(',').next())
    {
        let (code, out) = ctl(&["io-test", "--pool", id, "--mib", "16"]);
        assert_eq!(code, 0, "pool {id}: {out}");
    }
}

#[test]
#[ignore = "needs a host with Hyperstor installed"]
fn installed_services_and_metrics_answer() {
    for unit in ["hyperstor-meta", "hyperstor-renew.timer"] {
        let st = Command::new("systemctl")
            .args(["is-active", unit])
            .output()
            .unwrap();
        assert!(st.status.success(), "{unit} isn't active");
    }
    // Each storage node's metrics port, from its installed options.
    for e in std::fs::read_dir("/etc/hyperstor").unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("node-") && name.ends_with(".env")) {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap();
        let admin = text
            .split_whitespace()
            .skip_while(|w| *w != "--admin")
            .nth(1)
            .expect("--admin in node options");
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(admin).unwrap();
        s.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut t = String::new();
        s.read_to_string(&mut t).unwrap();
        assert!(
            t.starts_with("HTTP/1.1 200 OK") && t.ends_with("ok\n"),
            "{name}: {t}"
        );
    }
}
