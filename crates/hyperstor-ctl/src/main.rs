//! `hyperstorctl [--meta ADDR] [--json] status|nodes|pools|pgs|io-test|out|in
//!               [--pool P] [--mib N] [--device D] [--node N]`
//!
//! Reads the published cluster map from the map service and reports on
//! it (spec/administration.md §23.6), or tests I/O end to end:
//!
//! - `status`: health with its reasons, nodes, devices and PG states per
//!   pool (`--json` for one JSON object);
//! - `nodes`, `pools`, `pgs --pool P`: tables;
//! - `io-test`: writes N MiB (default 16) of random data to a new volume
//!   of pool P (default the first pool), reads it back and compares;
//! - `out` and `in` (`--device D`, or every device of `--node N`): take a
//!   device out of placement, so its PGs move elsewhere and it can be
//!   serviced, and bring it back.
//!
//! The map service is `--meta`, else `$HYPERSTOR_META`, else the `meta`
//! line of `/etc/hyperstor/hyperstorctl.conf` (written by
//! `hyperstor-install`), else `127.0.0.1:8850`.
//!
//! Exit status: 0 done (and, for `status`, health ok), 1 health warn or
//! error, or a failed test, 2 usage, 3 the map service unreachable.

use hyperstor_client::Client;
use hyperstor_ctl::*;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: hyperstorctl [--meta ADDR] [--json] status|nodes|pools|pgs|io-test|out|in [--pool P] [--mib N] [--device D] [--node N]";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn meta_addr(flag: Option<String>) -> SocketAddr {
    let from_conf = || {
        std::fs::read_to_string("/etc/hyperstor/hyperstorctl.conf")
            .ok()?
            .lines()
            .find_map(|l| l.trim().strip_prefix("meta ").map(|v| v.trim().to_string()))
    };
    let s = flag
        .or_else(|| std::env::var("HYPERSTOR_META").ok())
        .or_else(from_conf)
        .unwrap_or_else(|| "127.0.0.1:8850".into());
    s.parse().unwrap_or_else(|_| {
        eprintln!("hyperstorctl: bad map service address {s}");
        std::process::exit(2)
    })
}

fn random_u64() -> u64 {
    let mut v = 0u64;
    // SAFETY: getrandom writes at most 8 bytes into `v`.
    unsafe { libc::getrandom(&mut v as *mut u64 as *mut libc::c_void, 8, 0) };
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut meta = None;
    let mut json = false;
    let mut pool: Option<u32> = None;
    let mut mib: u64 = 16;
    let mut cmd = None;
    let mut device: Option<u32> = None;
    let mut node: Option<u32> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--meta" => meta = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--json" => json = true,
            "--pool" => {
                pool = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| usage()),
                )
            }
            "--mib" => {
                mib = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or_else(|| usage())
            }
            "--device" => {
                device = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| usage()),
                )
            }
            "--node" => {
                node = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| usage()),
                )
            }
            "status" | "nodes" | "pools" | "pgs" | "io-test" | "out" | "in" if cmd.is_none() => {
                cmd = Some(a.clone())
            }
            _ => usage(),
        }
    }
    let Some(cmd) = cmd else { usage() };
    if (cmd == "out" || cmd == "in") == (device.is_none() && node.is_none()) {
        // out and in need a target; nothing else takes one.
        usage();
    }
    let meta = meta_addr(meta);
    let code = hyperstor_reactor::block_on(async move {
        let connect = hyperstor_reactor::time::timeout(
            Duration::from_secs(5),
            Client::connect(meta, random_u64()),
        )
        .await;
        let cl = match connect {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                eprintln!("hyperstorctl: map service {meta}: {e}");
                return 3;
            }
            Err(_) => {
                eprintln!("hyperstorctl: map service {meta}: no answer in 5 s");
                return 3;
            }
        };
        let map = cl.map();
        let r = report(&map);
        match cmd.as_str() {
            "status" => {
                if json {
                    println!("{}", status_json(&map, &r));
                } else {
                    print!("{}", status_text(&map, &r));
                }
                i32::from(r.health != Health::Ok)
            }
            "nodes" => {
                print!("{}", nodes_text(&map));
                0
            }
            "pools" => {
                print!("{}", pools_text(&map, &r));
                0
            }
            "pgs" => {
                let Some(p) = pool.or_else(|| map.pools.keys().next().copied()) else {
                    eprintln!("hyperstorctl: the map has no pools");
                    return 1;
                };
                print!("{}", pgs_text(&map, p));
                0
            }
            "out" | "in" => {
                let devices: Vec<u32> = match (device, node) {
                    (Some(d), _) => vec![d],
                    (None, Some(n)) => map
                        .devices
                        .values()
                        .filter(|d| d.node == n)
                        .map(|d| d.id)
                        .collect(),
                    (None, None) => unreachable!(),
                };
                if devices.is_empty() {
                    eprintln!("hyperstorctl: node {} has no devices", node.unwrap_or(0));
                    return 1;
                }
                set_in(&cl, &devices, cmd == "in").await
            }
            _ => {
                let Some(p) = pool.or_else(|| map.pools.keys().next().copied()) else {
                    eprintln!("hyperstorctl: the map has no pools");
                    return 1;
                };
                io_test(cl, p, mib).await
            }
        }
    });
    std::process::exit(code);
}

/// Marks `devices` in or out of placement, one request each.
async fn set_in(cl: &Client, devices: &[u32], up_in: bool) -> i32 {
    use hyperstor_proto::Msg;
    for &device in devices {
        match cl.call_meta(&Msg::SetDeviceIn { device, up_in }).await {
            Ok((Msg::SetDeviceInOk { map_epoch }, _)) => println!(
                "device {device} {} (map epoch {map_epoch}); hyperstorctl status shows the PGs moving",
                if up_in { "in" } else { "out" }
            ),
            Ok((Msg::Error { detail, .. }, _)) => {
                eprintln!("hyperstorctl: device {device}: {detail}");
                return 1;
            }
            Ok((m, _)) => {
                eprintln!("hyperstorctl: device {device}: unexpected answer {m:?}");
                return 1;
            }
            Err(e) => {
                eprintln!("hyperstorctl: device {device}: {e}");
                return 1;
            }
        }
    }
    0
}

/// Writes `mib` MiB to a fresh volume in 1 MiB requests, reads it back
/// and compares. A new random lineage each run, so runs never see each
/// other's data.
async fn io_test(cl: std::rc::Rc<Client>, pool: u32, mib: u64) -> i32 {
    let lineage = (u128::from(random_u64()) << 64) | u128::from(random_u64());
    let mut seed = random_u64() | 1;
    let mut data = vec![0u8; (mib << 20) as usize];
    for w in data.chunks_mut(8) {
        // xorshift: cheap, and different for every block.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        w.copy_from_slice(&seed.to_le_bytes()[..w.len()]);
    }
    const STEP: usize = 1 << 20;
    let t = Instant::now();
    for (i, piece) in data.chunks(STEP).enumerate() {
        if let Err(e) = cl
            .write(pool, lineage, (i * STEP) as u64, piece.to_vec())
            .await
        {
            eprintln!("hyperstorctl: write at {} MiB: {e}", i);
            return 1;
        }
    }
    let w = t.elapsed();
    let t = Instant::now();
    for (i, piece) in data.chunks(STEP).enumerate() {
        match cl
            .read(pool, lineage, (i * STEP) as u64, piece.len() as u64)
            .await
        {
            Ok(got) if got == piece => {}
            Ok(_) => {
                eprintln!("hyperstorctl: data read at {i} MiB differs from what was written");
                return 1;
            }
            Err(e) => {
                eprintln!("hyperstorctl: read at {i} MiB: {e}");
                return 1;
            }
        }
    }
    let r = t.elapsed();
    let rate = |d: Duration| mib as f64 / d.as_secs_f64().max(1e-9);
    println!(
        "pool {pool}: wrote {mib} MiB in {:.0} ms ({:.0} MiB/s), read and verified in {:.0} ms ({:.0} MiB/s)",
        w.as_secs_f64() * 1e3,
        rate(w),
        r.as_secs_f64() * 1e3,
        rate(r)
    );
    0
}
