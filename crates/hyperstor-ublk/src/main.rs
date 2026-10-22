//! `hyperstor-ublk attach --meta ADDR[,ADDR...] --id ID [--take-over] [--preempt] [--depth N]`
//! `hyperstor-ublk delete --dev N`
//!
//! `attach` takes volume ID's `rw` lease (refused while another client
//! holds it; `--take-over` waits out a dead holder's lease and blocklists
//! it, `--preempt` blocklists a live one first) and serves the volume as
//! `/dev/ublkbN` until SIGINT or SIGTERM, then detaches (decisions.md D95).
//! `delete` removes device N left behind by a server that died. Both need
//! root and the `ublk_drv` module.

use hyperstor_ublk::{Options, Take, ctrl::Control, parse_id, serve};
use std::io::Write;

const USAGE: &str = "usage: hyperstor-ublk attach --meta ADDR[,ADDR...] --id ID [--take-over] [--preempt] [--depth N] | delete --dev N";

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
    match args.first().map(String::as_str) {
        Some("attach") => {
            let meta = get("--meta").unwrap_or_else(|| usage());
            let meta: Vec<std::net::SocketAddr> = meta
                .split(',')
                .map(|a| a.parse().unwrap_or_else(|_| usage()))
                .collect();
            let volume = get("--id")
                .and_then(|s| parse_id(&s))
                .unwrap_or_else(|| usage());
            let take = match (has("--take-over"), has("--preempt")) {
                (false, false) => Take::Attach,
                (true, false) => Take::TakeOver,
                (false, true) => Take::Preempt,
                (true, true) => usage(),
            };
            let depth = match get("--depth") {
                None => 64,
                Some(d) => d
                    .parse()
                    .ok()
                    .filter(|d| (1..=4096).contains(d))
                    .unwrap_or_else(|| usage()),
            };
            let stop = stop_on_signal();
            let r = serve(
                Options {
                    meta,
                    volume,
                    take,
                    depth,
                },
                |s| {
                    println!(
                        "volume {:032x} ({} bytes) served as /dev/ublkb{}",
                        s.volume.id, s.volume.size, s.dev
                    );
                    let _ = std::io::stdout().flush();
                },
                stop,
            );
            if let Err(e) = r {
                eprintln!("hyperstor-ublk: {e}");
                std::process::exit(1);
            }
        }
        Some("delete") => {
            let dev: u32 = get("--dev")
                .and_then(|d| d.parse().ok())
                .unwrap_or_else(|| usage());
            let r = Control::open().and_then(|mut c| {
                let _ = c.stop(dev);
                c.delete(dev)
            });
            if let Err(e) = r {
                eprintln!("hyperstor-ublk: device {dev}: {e}");
                std::process::exit(1);
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
