//! `hyperstorctl [--api ADDR] [--json] status|nodes|pools|pgs|io-test|out|in
//!               [--pool P] [--mib N] [--device D] [--node N]`
//!
//! Manages a cluster through the cluster API, `hyperstor.v1`
//! (spec/administration.md §23.5, §23.6):
//!
//! - `status`: health with its reasons, nodes, devices and PG states per
//!   pool;
//! - `nodes`, `pools`, `pgs [--pool P]`: tables;
//! - `out` and `in` (`--device D`, or every device of `--node N`): take a
//!   device out of placement, so its PGs move elsewhere and it can be
//!   serviced, and bring it back;
//! - `io-test`: writes N MiB (default 16) of random data to a new volume
//!   of pool P (default the first pool) through the client library,
//!   reads it back and compares.
//!
//! `--json` prints the API's answer as JSON, the form `curl` gets.
//!
//! The API is `--api`, else `$HYPERSTOR_API`, else the `api` line of
//! `/etc/hyperstor/hyperstorctl.conf` (written by `hyperstor-install`),
//! else `127.0.0.1:8850`.
//!
//! Exit status: 0 done (and, for `status`, health ok), 1 health warn or
//! error, a refused request or a failed test, 2 usage, 3 the API
//! unreachable.

use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use hyperstor_api::ClusterServiceClient;
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_client::Client;
use hyperstor_ctl::*;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: hyperstorctl [--api ADDR] [--json] status|nodes|pools|pgs|io-test|out|in [--pool P] [--mib N] [--device D] [--node N]";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn api_addr(flag: Option<String>) -> String {
    let from_conf = || {
        std::fs::read_to_string("/etc/hyperstor/hyperstorctl.conf")
            .ok()?
            .lines()
            .find_map(|l| l.trim().strip_prefix("api ").map(|v| v.trim().to_string()))
    };
    let s = flag
        .or_else(|| std::env::var("HYPERSTOR_API").ok())
        .or_else(from_conf)
        .unwrap_or_else(|| "127.0.0.1:8850".into());
    if s.parse::<std::net::SocketAddr>().is_err() {
        eprintln!("hyperstorctl: bad API address {s}");
        std::process::exit(2)
    }
    s
}

fn random_u64() -> u64 {
    let mut v = 0u64;
    // SAFETY: getrandom writes at most 8 bytes into `v`.
    unsafe { libc::getrandom(&mut v as *mut u64 as *mut libc::c_void, 8, 0) };
    v
}

struct Args {
    json: bool,
    pool: Option<u32>,
    mib: u64,
    device: Option<u32>,
    node: Option<u32>,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut api = None;
    let mut a = Args {
        json: false,
        pool: None,
        mib: 16,
        device: None,
        node: None,
    };
    let mut cmd = None;
    let mut it = argv.iter();
    let num =
        |v: Option<&String>| -> u32 { v.and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()) };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--api" => api = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--json" => a.json = true,
            "--pool" => a.pool = Some(num(it.next())),
            "--mib" => a.mib = u64::from(num(it.next())).max(1),
            "--device" => a.device = Some(num(it.next())),
            "--node" => a.node = Some(num(it.next())),
            "status" | "nodes" | "pools" | "pgs" | "io-test" | "out" | "in" if cmd.is_none() => {
                cmd = Some(arg.clone())
            }
            _ => usage(),
        }
    }
    let Some(cmd) = cmd else { usage() };
    if (cmd == "out" || cmd == "in") == (a.device.is_none() && a.node.is_none()) {
        // out and in need a target; nothing else takes one.
        usage();
    }
    let api = api_addr(api);

    // The API calls run on a small tokio runtime; io-test then runs the
    // client library on a reactor, after it.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let outcome = rt.block_on(manage(&api, &cmd, &a));
    let code = match outcome {
        Ok(Next::Exit(code)) => code,
        Ok(Next::IoTest { map, pool }) => {
            hyperstor_reactor::block_on(async move { io_test(map, pool, a.mib).await })
        }
        Err(e) => {
            let unreachable = e.code == connectrpc::ErrorCode::Unavailable
                || e.code == connectrpc::ErrorCode::DeadlineExceeded;
            eprintln!(
                "hyperstorctl: {api}: {}",
                e.message.as_deref().unwrap_or("request failed")
            );
            if unreachable { 3 } else { 1 }
        }
    };
    std::process::exit(code);
}

enum Next {
    Exit(i32),
    IoTest {
        map: std::net::SocketAddr,
        pool: u32,
    },
}

async fn manage(api: &str, cmd: &str, a: &Args) -> Result<Next, connectrpc::ConnectError> {
    let uri: http::Uri = format!("http://{api}")
        .parse()
        .map_err(|_| connectrpc::ConnectError::invalid_argument("bad address"))?;
    let client = ClusterServiceClient::new(
        HttpClient::plaintext(),
        ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5)),
    );
    let status = || async {
        client
            .get_status_with_options(pb::GetStatusRequest::default(), CallOptions::default())
            .await
            .map(|r| r.into_owned())
    };
    let first_pool = |s: &pb::GetStatusResponse| a.pool.or_else(|| s.pools.first().map(|p| p.id));
    Ok(match cmd {
        "status" => {
            let s = status().await?;
            if a.json {
                println!("{}", serde_json::to_string(&s).expect("json"));
            } else {
                print!("{}", status_text(&s));
            }
            Next::Exit(i32::from(!is_ok(&s)))
        }
        "nodes" | "pools" => {
            let s = status().await?;
            if a.json {
                println!("{}", serde_json::to_string(&s).expect("json"));
            } else if cmd == "nodes" {
                print!("{}", nodes_text(&s));
            } else {
                print!("{}", pools_text(&s));
            }
            Next::Exit(0)
        }
        "pgs" => {
            let s = status().await?;
            let Some(pool) = first_pool(&s) else {
                eprintln!("hyperstorctl: the cluster has no pools");
                return Ok(Next::Exit(1));
            };
            let r = client
                .list_pgs(pb::ListPgsRequest {
                    pool,
                    ..Default::default()
                })
                .await?
                .into_owned();
            if a.json {
                println!("{}", serde_json::to_string(&r).expect("json"));
            } else {
                print!("{}", pgs_text(&r));
            }
            Next::Exit(0)
        }
        "out" | "in" => {
            let up_in = cmd == "in";
            let devices: Vec<u32> = match (a.device, a.node) {
                (Some(d), _) => vec![d],
                (None, Some(n)) => {
                    let s = status().await?;
                    let devs: Vec<u32> = s
                        .nodes
                        .iter()
                        .filter(|x| x.id == n)
                        .flat_map(|x| x.devices.iter().map(|d| d.id))
                        .collect();
                    if devs.is_empty() {
                        eprintln!("hyperstorctl: no node {n}, or it has no devices");
                        return Ok(Next::Exit(1));
                    }
                    devs
                }
                (None, None) => unreachable!("checked with the arguments"),
            };
            for device in devices {
                let r = client
                    .set_device_in(pb::SetDeviceInRequest {
                        device,
                        up_in,
                        ..Default::default()
                    })
                    .await;
                match r {
                    Ok(r) => println!(
                        "device {device} {cmd} (map epoch {}); hyperstorctl status shows the PGs moving",
                        r.into_owned().map_epoch
                    ),
                    Err(e) if e.code == connectrpc::ErrorCode::NotFound => {
                        eprintln!(
                            "hyperstorctl: {}",
                            e.message.as_deref().unwrap_or("not found")
                        );
                        return Ok(Next::Exit(1));
                    }
                    Err(e) => return Err(e),
                }
            }
            Next::Exit(0)
        }
        _ => {
            let s = status().await?;
            let Some(pool) = first_pool(&s) else {
                eprintln!("hyperstorctl: the cluster has no pools");
                return Ok(Next::Exit(1));
            };
            let map = s.map_address.parse().map_err(|_| {
                connectrpc::ConnectError::internal("the API gave no map service address")
            })?;
            Next::IoTest { map, pool }
        }
    })
}

/// Writes `mib` MiB to a fresh volume in 1 MiB requests, reads it back
/// and compares. A new random lineage each run, so runs never see each
/// other's data.
async fn io_test(meta: std::net::SocketAddr, pool: u32, mib: u64) -> i32 {
    let cl = match hyperstor_reactor::time::timeout(
        Duration::from_secs(5),
        Client::connect(meta, random_u64()),
    )
    .await
    {
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
            eprintln!("hyperstorctl: write at {i} MiB: {e}");
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
