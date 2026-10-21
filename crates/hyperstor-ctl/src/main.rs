//! `hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--json | -o json|yaml] COMMAND
//!               [--wait SECS] [--pool P] [--mib N] [--device D] [--node N] [--size S] [--name N] [--id ID]
//!               [--role R] [--ttl SECS] [--dry-run]`
//!
//! Manages a cluster through the cluster API, `hyperstor.v1`
//! (spec/administration.md §23.5, §23.6):
//!
//! - `status`: health with its reasons, nodes, devices and PG states per
//!   pool; with `--wait SECS`, asks again every half second until health
//!   is ok or SECS have passed (for scripts, after an install or a
//!   change);
//! - `nodes`, `pools`, `pgs [--pool P]`: tables;
//! - `out` and `in` (`--device D`, or every device of `--node N`): take a
//!   device out of placement, so its PGs move elsewhere and it can be
//!   serviced, and bring it back; with `--wait SECS`, until no PG is
//!   backfilling or inactive;
//! - `volumes [--pool P]`: the pool's volumes; `create-volume --size S
//!   [--pool P] [--name N] [--id ID]` (S in bytes, or with K, M, G or T)
//!   and `delete-volume --id ID` (decisions.md D77);
//! - `io-test`: writes N MiB (default 16) of random data to a new volume
//!   of pool P (default the first pool) through the client library,
//!   reads it back and compares;
//! - `token create --role node|meta [--ttl SECS]`, `token list`, `token
//!   revoke --id ID`: join tokens (§23.4.2); `create` prints the token
//!   alone on standard output, and how to join with it on standard error;
//! - `drain --node N [--dry-run] [--wait SECS]`: takes every device of a
//!   node out and follows its PGs as an operation; `--dry-run` lists what
//!   would move and changes nothing;
//! - `op list`, `op get --id ID`, `op watch --id ID [--wait SECS]`, `op
//!   cancel --id ID`: long operations (a cancelled drain brings the
//!   devices back in).
//!
//! `--json` (or `-o json`) prints the API's answer as JSON, the form `curl`
//! gets; `-o yaml` as YAML. A refused call prints the API's message and
//! its stable reason, as `(reason)`.
//!
//! The API is `--api`, else `$HYPERSTOR_API`, else the `api` line of
//! `/etc/hyperstor/hyperstorctl.conf` (written by `hyperstor-install`),
//! else `127.0.0.1:8850`. With `--tls DIR`, or a `tls DIR` line in that
//! file, it connects over mutual TLS with `DIR/admin.crt`,
//! `DIR/admin.key` and `DIR/root.crt` (§23.8); otherwise plain HTTP.
//! With `--socket PATH`, or when neither `--api` nor `--tls` is given and
//! the file's `socket PATH` exists, it uses the map service's local
//! socket instead: no certificate, the kernel names the caller
//! (break-glass, §23.6).
//!
//! Exit status: 0 done (and, for `status`, health ok), 1 health warn or
//! error, a failed request or test, 2 usage, 3 the API unreachable (or the
//! member asked isn't the leader), 4 not allowed: no certificate, or one
//! whose role may not do this.

use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{
    ClusterServiceClient, NodeServiceClient, OperationServiceClient, TokenServiceClient,
};
use hyperstor_client::Client;
use hyperstor_ctl::*;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--json | -o json|yaml] status|nodes|pools|pgs|volumes|create-volume|delete-volume|io-test|out|in|drain|token create|token list|token revoke|op list|op get|op watch|op cancel [--wait SECS] [--pool P] [--mib N] [--device D] [--node N] [--size S] [--name N] [--id ID] [--role R] [--ttl SECS] [--dry-run]";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

/// A setting from `/etc/hyperstor/hyperstorctl.conf`: `KEY VALUE` lines.
fn conf(key: &str) -> Option<String> {
    std::fs::read_to_string("/etc/hyperstor/hyperstorctl.conf")
        .ok()?
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix(key)?
                .strip_prefix(' ')
                .map(|v| v.trim().to_string())
        })
}

fn api_addr(flag: Option<String>) -> String {
    let from_conf = || conf("api");
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
    yaml: bool,
    role: Option<String>,
    ttl: u32,
    dry_run: bool,
    pool: Option<u32>,
    mib: u64,
    wait: Option<u32>,
    device: Option<u32>,
    node: Option<u32>,
    size: Option<u64>,
    name: String,
    id: Option<String>,
}

/// `S` bytes, or with a K, M, G or T suffix (powers of 1024).
fn parse_size(s: &str) -> Option<u64> {
    let (n, mul) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 1u64 << 10),
        b'M' | b'm' => (&s[..s.len() - 1], 1 << 20),
        b'G' | b'g' => (&s[..s.len() - 1], 1 << 30),
        b'T' | b't' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    n.parse::<u64>().ok()?.checked_mul(mul)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut api = None;
    let mut tls = None;
    let mut socket = None;
    let mut a = Args {
        json: false,
        yaml: false,
        role: None,
        ttl: 0,
        dry_run: false,
        pool: None,
        mib: 16,
        wait: None,
        device: None,
        node: None,
        size: None,
        name: String::new(),
        id: None,
    };
    let mut cmd = None;
    let mut it = argv.iter();
    let num =
        |v: Option<&String>| -> u32 { v.and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()) };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--api" => api = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--tls" => tls = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--socket" => socket = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--json" => a.json = true,
            "-o" => match it.next().map(String::as_str) {
                Some("json") => a.json = true,
                Some("yaml") => (a.json, a.yaml) = (true, true),
                _ => usage(),
            },
            "--role" => a.role = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--ttl" => a.ttl = num(it.next()),
            "--dry-run" => a.dry_run = true,
            "--pool" => a.pool = Some(num(it.next())),
            "--mib" => a.mib = u64::from(num(it.next())).max(1),
            "--wait" => a.wait = Some(num(it.next())),
            "--device" => a.device = Some(num(it.next())),
            "--node" => a.node = Some(num(it.next())),
            "--size" => {
                a.size = Some(
                    it.next()
                        .and_then(|s| parse_size(s))
                        .unwrap_or_else(|| usage()),
                )
            }
            "--name" => a.name = it.next().cloned().unwrap_or_else(|| usage()),
            "--id" => a.id = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "status" | "nodes" | "pools" | "pgs" | "volumes" | "create-volume"
            | "delete-volume" | "io-test" | "out" | "in" | "drain"
                if cmd.is_none() =>
            {
                cmd = Some(arg.clone())
            }
            // Two words: the group, then what to do.
            "token" | "op" if cmd.is_none() => {
                let sub = it.next().map(String::as_str);
                let ok = match arg.as_str() {
                    "token" => matches!(sub, Some("create" | "list" | "revoke")),
                    _ => matches!(sub, Some("list" | "get" | "watch" | "cancel")),
                };
                if !ok {
                    usage();
                }
                cmd = Some(format!("{arg} {}", sub.unwrap_or_default()));
            }
            _ => usage(),
        }
    }
    let Some(cmd) = cmd else { usage() };
    if (cmd == "out" || cmd == "in" || cmd == "drain") == (a.device.is_none() && a.node.is_none())
        || (cmd == "drain" && a.node.is_none())
    {
        // out, in and drain need a target; nothing else takes one.
        usage();
    }
    let needs_id = matches!(
        cmd.as_str(),
        "token revoke" | "op get" | "op watch" | "op cancel"
    );
    if (cmd == "token create") != a.role.is_some()
        || (a.dry_run && cmd != "drain")
        || (a.ttl != 0 && cmd != "token create")
        || (needs_id && a.id.is_none())
    {
        usage();
    }
    // create-volume needs a size; delete-volume an id, which only these two take.
    if (cmd == "create-volume") != a.size.is_some()
        || (cmd == "delete-volume" && a.id.is_none())
        || (a.id.is_some() && cmd != "create-volume" && cmd != "delete-volume" && !needs_id)
    {
        usage();
    }
    // The local socket (break-glass, §23.6): given, or named in the
    // configuration and present, unless an address or certificates were
    // given on the command line.
    let socket = socket.or_else(|| {
        if api.is_some() || tls.is_some() {
            return None;
        }
        conf("socket").filter(|p| std::path::Path::new(p).exists())
    });
    let api = api_addr(api);
    let tls = match tls.or_else(|| conf("tls")).filter(|_| socket.is_none()) {
        None => None,
        Some(dir) => {
            let dir = std::path::PathBuf::from(dir);
            let read = |f: &str| {
                std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| {
                    eprintln!("hyperstorctl: {}: {e}", dir.join(f).display());
                    std::process::exit(2)
                })
            };
            let c = hyperstor_pki::client_config(
                &read("admin.crt"),
                &read("admin.key"),
                &read("root.crt"),
            )
            .unwrap_or_else(|e| {
                eprintln!("hyperstorctl: {}: {e}", dir.display());
                std::process::exit(2)
            });
            Some(std::sync::Arc::new(c))
        }
    };

    // The API calls run on a small tokio runtime; io-test then runs the
    // client library on a reactor, after it.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let outcome = rt.block_on(async {
        let config =
            |uri: http::Uri| ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5));
        match &socket {
            Some(path) => {
                let uri: http::Uri = "http://localhost".parse().expect("uri");
                let conn =
                    connectrpc::client::Http2Connection::connect_unix(path.clone(), uri.clone())
                        .await?
                        .shared(16);
                manage(conn, config(uri), &cmd, &a).await
            }
            None => {
                let scheme = if tls.is_some() { "https" } else { "http" };
                let uri: http::Uri = format!("{scheme}://{api}")
                    .parse()
                    .map_err(|_| connectrpc::ConnectError::invalid_argument("bad address"))?;
                let http = match tls {
                    Some(c) => HttpClient::with_tls(c),
                    None => HttpClient::plaintext(),
                };
                manage(http, config(uri), &cmd, &a).await
            }
        }
    });
    let api = socket.clone().unwrap_or(api);
    let code = match outcome {
        Ok(Next::Exit(code)) => code,
        Ok(Next::IoTest { map, pool }) => {
            hyperstor_reactor::block_on(async move { io_test(map, pool, a.mib).await })
        }
        Err(e) => {
            eprintln!("hyperstorctl: {api}: {}", error_text(&e));
            exit_code(&e)
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

/// The API's message and, when it gave one, its reason.
fn error_text(e: &connectrpc::ConnectError) -> String {
    let msg = e.message.as_deref().unwrap_or("request failed");
    match hyperstor_ctl::reason_of(e) {
        Some(r) => format!("{msg} ({r})"),
        None => msg.to_string(),
    }
}

/// The exit status for a failed call (see the top of this file).
fn exit_code(e: &connectrpc::ConnectError) -> i32 {
    use connectrpc::ErrorCode as C;
    match e.code {
        C::Unavailable | C::DeadlineExceeded => 3,
        C::PermissionDenied | C::Unauthenticated => 4,
        _ => 1,
    }
}

/// Prints `v` as JSON or YAML when asked, else `text`.
fn show<T: serde::Serialize>(a: &Args, v: &T, text: impl FnOnce() -> String) {
    if a.yaml {
        print!("{}", to_yaml(&serde_json::to_value(v).expect("json")));
    } else if a.json {
        println!("{}", serde_json::to_string(v).expect("json"));
    } else {
        print!("{}", text());
    }
}

async fn manage<T>(
    transport: T,
    config: ClientConfig,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    match cmd.split_once(' ').map_or(cmd, |(g, _)| g) {
        "token" => return tokens(TokenServiceClient::new(transport, config), cmd, a).await,
        "op" | "drain" => {
            return operations(
                NodeServiceClient::new(transport.clone(), config.clone()),
                OperationServiceClient::new(transport, config),
                cmd,
                a,
            )
            .await;
        }
        _ => {}
    }
    let client = ClusterServiceClient::new(transport, config);
    let status = || async {
        client
            .get_status_with_options(pb::GetStatusRequest::default(), CallOptions::default())
            .await
            .map(|r| r.into_owned())
    };
    // The first data pool: `.meta` (pool 0) holds volume headers.
    let first_pool = |s: &pb::GetStatusResponse| {
        a.pool
            .or_else(|| s.pools.iter().find(|p| p.id != 0).map(|p| p.id))
    };
    Ok(match cmd {
        "status" => {
            let deadline =
                std::time::Instant::now() + Duration::from_secs(a.wait.unwrap_or(0).into());
            let s = loop {
                let r = status().await;
                let done = std::time::Instant::now() >= deadline;
                match r {
                    Ok(s) if is_ok(&s) || done => break s,
                    // Still starting: the API may not answer yet.
                    Err(e) if done || e.code != connectrpc::ErrorCode::Unavailable => {
                        return Err(e);
                    }
                    _ => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            };
            show(a, &s, || status_text(&s));
            Next::Exit(i32::from(!is_ok(&s)))
        }
        "nodes" | "pools" => {
            let s = status().await?;
            show(a, &s, || {
                if cmd == "nodes" {
                    nodes_text(&s)
                } else {
                    pools_text(&s)
                }
            });
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
            show(a, &r, || pgs_text(&r));
            Next::Exit(0)
        }
        "volumes" => {
            let s = status().await?;
            let Some(pool) = first_pool(&s) else {
                eprintln!("hyperstorctl: the cluster has no data pools");
                return Ok(Next::Exit(1));
            };
            let r = client
                .list_volumes(pb::ListVolumesRequest {
                    pool,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || volumes_text(&r));
            Next::Exit(0)
        }
        "create-volume" => {
            let s = status().await?;
            let Some(pool) = first_pool(&s) else {
                eprintln!("hyperstorctl: the cluster has no data pools");
                return Ok(Next::Exit(1));
            };
            let r = client
                .create_volume(pb::CreateVolumeRequest {
                    pool,
                    size: a.size.expect("checked with the arguments"),
                    name: a.name.clone(),
                    id: a.id.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    if a.json {
                        println!("{}", serde_json::to_string(&r).expect("json"));
                    } else if let Some(v) = r.volume.as_option() {
                        println!("volume {} created in pool {}", v.id, v.pool);
                    }
                    Next::Exit(0)
                }
                Err(e) if e.code != connectrpc::ErrorCode::Unavailable => {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    Next::Exit(1)
                }
                Err(e) => return Err(e),
            }
        }
        "delete-volume" => {
            let id = a.id.clone().expect("checked with the arguments");
            match client
                .delete_volume(pb::DeleteVolumeRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => {
                    println!("volume {id} deleted");
                    Next::Exit(0)
                }
                Err(e) if e.code != connectrpc::ErrorCode::Unavailable => {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    Next::Exit(1)
                }
                Err(e) => return Err(e),
            }
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
            // The working map's epoch after the last change: --wait first
            // waits for the published map to reach it (it is fenced first).
            let mut changed_epoch = 0;
            for device in devices {
                let r = client
                    .set_device_in(pb::SetDeviceInRequest {
                        device,
                        up_in,
                        ..Default::default()
                    })
                    .await;
                match r {
                    Ok(r) => {
                        let epoch = r.into_owned().map_epoch;
                        changed_epoch = changed_epoch.max(epoch);
                        println!(
                            "device {device} {cmd} (map epoch {epoch}); hyperstorctl status shows the PGs moving"
                        )
                    }
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
            // --wait: until the PGs have settled, nothing backfilling or
            // inactive (degraded is as good as it gets with nowhere to go).
            if let Some(secs) = a.wait {
                let deadline = std::time::Instant::now() + Duration::from_secs(secs.into());
                loop {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let s = status().await?;
                    if s.epoch < changed_epoch {
                        // Not published yet: nothing has started moving.
                        if std::time::Instant::now() >= deadline {
                            eprintln!("hyperstorctl: map epoch {changed_epoch} not published yet");
                            return Ok(Next::Exit(1));
                        }
                        continue;
                    }
                    let busy: u32 = s
                        .pools
                        .iter()
                        .map(|p| p.pgs.backfilling + p.pgs.inactive)
                        .sum();
                    if busy == 0 {
                        println!("PGs settled");
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        eprintln!("hyperstorctl: {busy} PG(s) still backfilling or inactive");
                        return Ok(Next::Exit(1));
                    }
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

/// `token create|list|revoke`.
async fn tokens<T>(
    client: TokenServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    Ok(match cmd {
        "token create" => {
            let role = a.role.clone().expect("checked with the arguments");
            let r = client
                .create_token(pb::CreateTokenRequest {
                    role: role.clone(),
                    ttl_seconds: a.ttl,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || format!("{}\n", r.token));
            if !a.json {
                let info = r.info.as_option().cloned().unwrap_or_default();
                let how = if role == "meta" {
                    "--role meta --members LIST --member-id N"
                } else {
                    "--role storage --address IP"
                };
                eprintln!(
                    "Token {} for role {role}, single-use, expires {} (UTC). Put it in a file\n\
                     readable by root only on the new host, then run there:\n  \
                     sudo hyperstor-install install --join {} --token-file FILE {how}",
                    info.id,
                    utc(info.expires_unix),
                    r.api_address
                );
            }
            Next::Exit(0)
        }
        "token list" => {
            let r = client
                .list_tokens(pb::ListTokensRequest::default())
                .await?
                .into_owned();
            show(a, &r, || tokens_text(&r));
            Next::Exit(0)
        }
        _ => {
            let id = a.id.clone().expect("checked with the arguments");
            client
                .revoke_token(pb::RevokeTokenRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await?;
            println!("token {id} revoked");
            Next::Exit(0)
        }
    })
}

/// `drain` and `op list|get|watch|cancel`.
async fn operations<T>(
    nodes: NodeServiceClient<T>,
    ops: OperationServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    let get = |id: String| {
        let ops = &ops;
        async move {
            ops.get_operation(pb::GetOperationRequest {
                id,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned())
        }
    };
    let id = match cmd {
        "drain" => {
            let node = a.node.expect("checked with the arguments");
            let r = nodes
                .drain_node(pb::DrainNodeRequest {
                    node,
                    dry_run: a.dry_run,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || {
                let mut t = moves_text(&r.moves);
                match r.operation.as_option() {
                    Some(op) => t.push_str(&format!(
                        "node {node} draining: operation {} (hyperstorctl op watch --id {})\n",
                        op.id, op.id
                    )),
                    None => t.push_str(&format!(
                        "dry run: {} PG(s) would move off node {node}; nothing changed\n",
                        r.moves.len()
                    )),
                }
                t
            });
            match (r.operation.as_option(), a.wait) {
                (Some(op), Some(_)) => op.id.clone(),
                _ => return Ok(Next::Exit(0)),
            }
        }
        "op list" => {
            let r = ops
                .list_operations(pb::ListOperationsRequest::default())
                .await?
                .into_owned();
            show(a, &r, || ops_text(&r.operations));
            return Ok(Next::Exit(0));
        }
        "op get" => {
            let op = get(a.id.clone().expect("checked")).await?;
            show(a, &op, || ops_text(std::slice::from_ref(&op)));
            return Ok(Next::Exit(op_exit(&op)));
        }
        "op cancel" => {
            let op = ops
                .cancel_operation(pb::CancelOperationRequest {
                    id: a.id.clone().expect("checked"),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &op, || ops_text(std::slice::from_ref(&op)));
            return Ok(Next::Exit(0));
        }
        _ => a.id.clone().expect("checked"),
    };
    // op watch, or drain --wait: follow until it finishes.
    let deadline = a
        .wait
        .map(|s| Instant::now() + Duration::from_secs(s.into()));
    let mut last = String::new();
    loop {
        let op = get(id.clone()).await?;
        let line = op_line(&op);
        if line != last && !a.json {
            println!("{line}");
            last = line;
        }
        if op.state.as_known() != Some(pb::OperationState::OPERATION_STATE_RUNNING) {
            if a.json {
                show(a, &op, String::new);
            }
            return Ok(Next::Exit(op_exit(&op)));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("hyperstorctl: operation {id} still running");
            return Ok(Next::Exit(1));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 0 for an operation that finished well or still runs, else 1.
fn op_exit(op: &pb::Operation) -> i32 {
    use pb::OperationState as S;
    match op.state.as_known() {
        Some(S::OPERATION_STATE_DONE | S::OPERATION_STATE_RUNNING) => 0,
        _ => 1,
    }
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
