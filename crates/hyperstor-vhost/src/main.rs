//! `hyperstor-vhost serve --meta ADDR[,ADDR...] [--dir DIR] [--allow-uid UID]... [--group GID] [--queues N]`
//! `hyperstor-vhost ctl [--dir DIR] hello | list | attach ID [--serial S] [--take-over | --preempt] | detach ID`
//!
//! `serve` runs the service (frontends.md §11.1) until SIGINT or SIGTERM,
//! then detaches every volume. `ctl` sends one request to a running
//! service's control socket and prints the reply (decisions.md D117).

use hyperstor_vhost::{Options, control_path, serve};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

const USAGE: &str = "usage: hyperstor-vhost serve --meta ADDR[,ADDR...] [--dir DIR] [--allow-uid UID]... [--group GID] [--queues N] | ctl [--dir DIR] hello | list | attach ID [--serial S] [--take-over | --preempt] | detach ID";
const DIR: &str = "/run/hyperstor/vhost";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let dir = PathBuf::from(get("--dir").unwrap_or_else(|| DIR.into()));
    match args.first().map(String::as_str) {
        Some("serve") => {
            let meta = get("--meta").unwrap_or_else(|| usage());
            let meta = meta
                .split(',')
                .map(|a| a.parse().unwrap_or_else(|_| usage()))
                .collect();
            let allow_uids = args
                .windows(2)
                .filter(|w| w[0] == "--allow-uid")
                .map(|w| w[1].parse().unwrap_or_else(|_| usage()))
                .collect();
            let group = get("--group").map(|g| g.parse().unwrap_or_else(|_| usage()));
            let queues = match get("--queues") {
                None => 1,
                Some(q) => q
                    .parse()
                    .ok()
                    .filter(|q| (1..=64).contains(q))
                    .unwrap_or_else(|| usage()),
            };
            let stop = stop_on_signal();
            let r = serve(
                Options {
                    meta,
                    dir,
                    allow_uids,
                    group,
                    queues,
                },
                |p| {
                    println!("hyperstor-vhost: control socket {}", p.display());
                    let _ = std::io::stdout().flush();
                },
                stop,
            );
            if let Err(e) = r {
                eprintln!("hyperstor-vhost: {e}");
                std::process::exit(1);
            }
        }
        Some("ctl") => {
            let rest: Vec<&str> = args[1..]
                .iter()
                .enumerate()
                .filter(|(i, a)| {
                    let prev = i.checked_sub(1).map(|p| args[1 + p].as_str());
                    !a.starts_with("--") && !matches!(prev, Some("--dir" | "--serial"))
                })
                .map(|(_, a)| a.as_str())
                .collect();
            let req = match rest.as_slice() {
                ["hello"] => json!({"id": 1, "op": "hello"}),
                ["list"] => json!({"id": 1, "op": "list"}),
                ["detach", id] => json!({"id": 1, "op": "detach", "args": {"volume": id}}),
                ["attach", id] => {
                    let take = match (has("--take-over"), has("--preempt")) {
                        (false, false) => "attach",
                        (true, false) => "take_over",
                        (false, true) => "preempt",
                        (true, true) => usage(),
                    };
                    json!({"id": 1, "op": "attach", "args": {
                        "volume": id,
                        "serial": get("--serial").unwrap_or_default(),
                        "take": take,
                    }})
                }
                _ => usage(),
            };
            let path = control_path(&dir);
            let reply = std::os::unix::net::UnixStream::connect(&path).and_then(|mut s| {
                writeln!(s, "{req}")?;
                let mut line = String::new();
                BufReader::new(s).read_line(&mut line)?;
                Ok(line)
            });
            match reply {
                Ok(line) => {
                    print!("{line}");
                    let failed = serde_json::from_str::<serde_json::Value>(&line)
                        .map_or(true, |v| v.get("error").is_some());
                    if failed {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("hyperstor-vhost: {}: {e}", path.display());
                    std::process::exit(1);
                }
            }
        }
        _ => usage(),
    }
}

/// A channel that receives on SIGINT or SIGTERM. The signals are blocked
/// in every thread (they inherit the mask) and taken by one with sigwait.
fn stop_on_signal() -> std::sync::mpsc::Receiver<()> {
    // SAFETY: sigset operations on a local set, and a mask change of
    // this thread before any other is started.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sig = 0;
        // SAFETY: waits for one of the blocked signals in `set`.
        unsafe { libc::sigwait(&set, &mut sig) };
        let _ = tx.send(());
    });
    rx
}
