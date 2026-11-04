//! `hyperstor-s3 --meta ADDR[,ADDR...] --tls DIR --keys FILE --data-pool NAME --index-pool NAME [--listen ADDR[,ADDR...]] [--reactors N] [--threads N] [--networks NAME[,NAME...]]`
//!
//! The S3 gateway (14-s3.md §14.2), until SIGINT or SIGTERM. `--tls DIR`
//! holds `s3.crt` (the chain) and `s3.key`; `--keys FILE` the access
//! keys ([`hyperstor_s3::keys`]). New buckets go in the two pools named.

use hyperstor_s3::Config;
use hyperstor_s3::gateway::{Options, serve};
use hyperstor_s3::keys::Keys;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "usage: hyperstor-s3 --meta ADDR[,ADDR...] --tls DIR --keys FILE --data-pool NAME --index-pool NAME [--listen ADDR[,ADDR...]] [--reactors N] [--threads N] [--networks NAME[,NAME...]]";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("hyperstor-s3: {e}");
    std::process::exit(1)
}

fn addrs(s: &str) -> Vec<SocketAddr> {
    s.split(',')
        .map(|a| a.parse().unwrap_or_else(|_| usage()))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let need = |flag: &str| get(flag).unwrap_or_else(|| usage());
    let count = |flag: &str, default: usize| match get(flag) {
        None => default,
        Some(n) => n
            .parse()
            .ok()
            .filter(|n| (1..=64).contains(n))
            .unwrap_or_else(|| usage()),
    };
    let dir = PathBuf::from(need("--tls"));
    let read = |name: &str| {
        let p = dir.join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| fail(format!("{}: {e}", p.display())))
    };
    let tls = hyperstor_pki::web_server_config(&read("s3.crt"), &read("s3.key"))
        .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())));
    let keys = Keys::load(&PathBuf::from(need("--keys"))).unwrap_or_else(|e| fail(e));
    if keys.is_empty() {
        fail("no access keys");
    }
    let opts = Options {
        meta: addrs(&need("--meta")),
        networks: get("--networks")
            .map(|s| {
                s.split(',')
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        listen: addrs(&get("--listen").unwrap_or_else(|| "0.0.0.0:443".into())),
        tls: std::sync::Arc::new(tls),
        keys,
        data_pool: need("--data-pool"),
        index_pool: need("--index-pool"),
        reactors: count("--reactors", 2),
        threads: count("--threads", 2),
        config: Config::default(),
        clean_every: Duration::from_secs(3600),
    };
    let stop = stop_on_signal();
    let r = serve(
        opts,
        |at| {
            let at: Vec<String> = at.iter().map(|a| format!("https://{a}/")).collect();
            println!("hyperstor-s3: on {}", at.join(" "));
            let _ = std::io::stdout().flush();
        },
        stop,
    );
    if let Err(e) = r {
        fail(e);
    }
}

/// A channel that receives on SIGINT or SIGTERM. The signals are blocked
/// in every thread (this one's mask is inherited) and waited for by one.
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
