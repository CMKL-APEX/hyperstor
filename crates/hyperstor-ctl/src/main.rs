//! `hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--mgmt URL [--token-file F] [--mgmt-ca FILE]]
//!               [--json | -o json|yaml] COMMAND
//!               [--wait SECS] [--pool P] [--pg N] [--mib N] [--device D] [--weight W|auto] [--node N] [--size S] [--name N] [--id ID]
//!               [--force] [--role R] [--ttl SECS] [--dry-run] [--serial S] [--file F] [--out FILE] [--raft ADDR] [--map ADDR[,ADDR...]]
//!               [--epoch E] [--ids ID,ID...] [--group G] [--new-id ID] [--grace SECS] [--on-full keep|purge]
//!               [--since AGO] [--principal P] [--op OP] [--limit N] [--retention-days N]`
//!
//! Manages a cluster through the cluster API, `hyperstor.v1`
//! (spec/19-administration.md §23.5, §23.6):
//!
//! - `status`: health with its reasons, nodes, devices and PG states per
//!   pool; with `--wait SECS`, asks again every half second until health
//!   is ok or SECS have passed (for scripts, after an install or a
//!   change);
//! - `nodes`, `pools`, `pgs [--pool P] [--pg N]`: tables; `pg-grid [--pool
//!   P] [--usage]`: a character per PG, 64 to a line (D154), and with
//!   `--usage` a digit per PG for the bytes on its primary (D155; the
//!   leader's to answer);
//! - `out` and `in` (`--device D`, or every device of `--node N`): take a
//!   device out of placement, so its PGs move elsewhere and it can be
//!   serviced, and bring it back; with `--wait SECS`, until no PG is
//!   backfilling or inactive;
//! - `weight --device D --weight W|auto`: keep a device's placement weight
//!   at W, or let it follow the device's capacity in TiB again (D158);
//!   its PGs move as the weight changes;
//! - `volumes [--pool P]`: the pool's volumes; `create-volume --size S
//!   [--pool P] [--name N] [--id ID]` (S in bytes, or with K, M, G or T)
//!   and `delete-volume --id ID [--force]` (04-decisions.md D77, D80);
//! - `snapshot list --id ID`, `snapshot create --id ID [--name N]`,
//!   `snapshot delete --id ID --epoch E`: a volume's snapshots
//!   (10-volumes.md §10.3); `snapshot group --ids ID,ID... [--name N]` takes
//!   them at one moment (a consistency group), `snapshot group-delete
//!   --group G` deletes a group's; `clone-volume --id ID --epoch E [--name
//!   N] [--new-id ID]`, `rollback-volume --id ID --epoch E` (detached) and
//!   `flatten-volume --id ID [--wait SECS]` (§10.4), which runs as an
//!   operation (D121);
//! - `delete-volume`, `snapshot delete` and `snapshot group-delete` move
//!   to the trash (D54): `trash list [--pool P]`; `trash restore --id ID
//!   [--epoch E] [--name N]` brings a volume (or its snapshot at E) back,
//!   as N if given; `trash purge --id ID [--epoch E] [--wait SECS]`
//!   deletes it for good, as an operation;
//!   `trash settings [--pool P] [--grace SECS] [--on-full keep|purge]`
//!   shows or sets how long a pool keeps it (default a day);
//! - `audit list [--since AGO] [--principal P] [--op OP] [--limit N]`:
//!   the audit log, newest first (AGO as `30m`, `2h`, `7d`; default the
//!   last 100 records); `audit settings [--retention-days N]` shows or sets
//!   how long it is kept (0: forever; D112, D113);
//! - `io-test`: writes N MiB (default 16) of random data to a new volume
//!   of pool P (default the first pool) through the client library,
//!   reads it back and compares;
//! - `token create --role node|meta|mgmt [--name N] [--ttl SECS]`, `token
//!   list`, `token revoke --id ID`: join tokens (§23.4.2); a `mgmt` token
//!   enrolls a management client named N, such as glidex (D122); `create`
//!   prints the token alone on standard output, and how to use it on
//!   standard error;
//! - `enroll --file TOKEN --out DIR`: a management client's identity from a
//!   mgmt token: a key made here, the API pinned to the token's root; DIR
//!   gets `admin.crt`, `admin.key` and `root.crt`, as `--tls` reads them;
//! - `expand-volume --id ID --size S`: grows a volume;
//! - `write-volume --id ID --file F [--offset O] [--sparse] [--map ADDR[,ADDR...]]`,
//!   `read-volume --id ID --size S --out F [--offset O] [--map ADDR[,ADDR...]]`: a
//!   file into a volume, holding its lease, or a range of it into a file,
//!   in whole 4 KiB blocks; `--sparse` skips zero blocks; with `--map`,
//!   straight to the map service (any of its members), without the API
//!   (D123);
//! - `drain --node N [--dry-run] [--wait SECS]`: takes every device of a
//!   node out and follows its PGs as an operation; `--dry-run` lists what
//!   would move and changes nothing;
//! - `op list`, `op get --id ID`, `op watch --id ID [--wait SECS]`, `op
//!   cancel --id ID`: long operations, drains, flattens and purges, kept a
//!   week after they finish (D120); `op watch` and `--wait` follow one's
//!   changes as they come; only a drain can be cancelled (it brings the
//!   devices back in);
//! - `member list`, `member add --id N --raft ADDR --map ADDR`, `member
//!   remove --id N [--force]`: the map service's members (D87). A new
//!   member must be running, joined with a meta token, before it is added;
//!   removal refuses to leave fewer than three voters unless forced;
//! - `map backup --out FILE`: a backup of the map service's store
//!   (D86), for `hyperstor-meta --restore`; written mode 0600, never over
//!   an existing file;
//! - `cert list`, `cert revoke --serial S`: certificates the API issued
//!   and the revocation list (D56); `cert show --file F` prints a
//!   certificate file's identity, serial and expiry, without the API.
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
//! With `--mgmt URL` (or `$HYPERSTOR_MGMT`) it goes through
//! `hyperstor-mgmt` instead, as a person: with their API token from
//! `--token-file F` or `$HYPERSTOR_TOKEN` (`hst_…`, made in the UI or at
//! `/auth/tokens`; never an argument), and their roles decide what it
//! may do (§23.9.2). `http://` is for loopback only (an SSH tunnel);
//! over `https://` it trusts the host's CAs and `--mgmt-ca FILE`'s, or
//! without it the cluster's root where `hyperstorctl.conf` names one
//! (mgmt's certificate is the cluster CA's by default, D108).
//!
//! Exit status: 0 done (and, for `status`, health ok), 1 health warn or
//! error, a failed request or test, 2 usage, 3 the API unreachable (or the
//! member asked isn't the leader), 4 not allowed: no certificate, or one
//! whose role may not do this.

use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{
    AuditServiceClient, CaServiceClient, ClusterServiceClient, MapServiceClient,
    MemberServiceClient, NodeServiceClient, OperationServiceClient, TokenServiceClient,
    TrashServiceClient,
};
use hyperstor_client::Client;
use hyperstor_ctl::*;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--mgmt URL [--token-file F] [--mgmt-ca FILE]] [--json | -o json|yaml] status|nodes|pools|pgs|pg-grid|volumes|create-volume|delete-volume|io-test|out|in|weight|drain|token create|token list|token revoke|op list|op get|op watch|op cancel|cert list|cert revoke|cert show|map backup|member list|member add|member remove|snapshot list|snapshot create|snapshot delete|snapshot group|snapshot group-delete|clone-volume|rollback-volume|flatten-volume|expand-volume|write-volume|read-volume|enroll|trash list|trash restore|trash purge|trash settings|audit list|audit settings [--wait SECS] [--pool P] [--pg N] [--mib N] [--device D] [--weight W|auto] [--node N] [--size S] [--name N] [--id ID] [--force] [--role R] [--ttl SECS] [--dry-run] [--serial S] [--file F] [--out FILE] [--raft ADDR] [--map ADDR[,ADDR...]] [--epoch E] [--ids ID,ID...] [--group G] [--new-id ID] [--grace SECS] [--on-full keep|purge] [--since AGO] [--principal P] [--op OP] [--limit N] [--retention-days N] [--offset O] [--sparse] [--usage]";

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
    serial: Option<String>,
    file: Option<String>,
    out: Option<String>,
    raft: Option<String>,
    map: Option<String>,
    pool: Option<u32>,
    pg: Option<u32>,
    mib: u64,
    wait: Option<u32>,
    device: Option<u32>,
    node: Option<u32>,
    size: Option<u64>,
    name: String,
    id: Option<String>,
    force: bool,
    epoch: Option<u64>,
    ids: Vec<String>,
    group: Option<String>,
    new_id: Option<String>,
    /// `trash settings`: the grace period, and what a full pool does.
    grace: Option<u64>,
    on_full: Option<String>,
    /// `audit list`: how far back (seconds), whose, which operation, how many.
    since: Option<u64>,
    principal: Option<String>,
    op: Option<String>,
    limit: Option<u32>,
    /// `audit settings`.
    retention_days: Option<u32>,
    /// `write-volume`, `read-volume`: where in the volume; skip zero blocks.
    offset: u64,
    sparse: bool,
    /// `pg-grid`: each PG's usage too.
    usage: bool,
    /// `weight`: the weight to set, or `None` for `auto` (D158).
    weight: Option<Option<f64>>,
    /// Through hyperstor-mgmt, which passes on whole answers only: an
    /// operation is followed by asking again, not by a watch.
    poll: bool,
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
    let mut mgmt = None;
    let mut token_file = None;
    let mut mgmt_ca = None;
    let mut a = Args {
        json: false,
        yaml: false,
        role: None,
        ttl: 0,
        dry_run: false,
        serial: None,
        file: None,
        out: None,
        raft: None,
        map: None,
        pool: None,
        pg: None,
        mib: 16,
        wait: None,
        device: None,
        node: None,
        size: None,
        name: String::new(),
        id: None,
        force: false,
        epoch: None,
        ids: Vec::new(),
        group: None,
        new_id: None,
        grace: None,
        on_full: None,
        since: None,
        principal: None,
        op: None,
        limit: None,
        retention_days: None,
        offset: 0,
        sparse: false,
        usage: false,
        weight: None,
        poll: false,
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
            "--mgmt" => {
                mgmt = Some(it.next().cloned().unwrap_or_else(|| usage()));
                a.poll = true;
            }
            "--token-file" => token_file = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--mgmt-ca" => mgmt_ca = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--json" => a.json = true,
            "-o" => match it.next().map(String::as_str) {
                Some("json") => a.json = true,
                Some("yaml") => (a.json, a.yaml) = (true, true),
                _ => usage(),
            },
            "--role" => a.role = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--ttl" => a.ttl = num(it.next()),
            "--dry-run" => a.dry_run = true,
            "--usage" => a.usage = true,
            "--serial" => a.serial = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--file" => a.file = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--out" => a.out = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--raft" => a.raft = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--map" => a.map = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--pool" => a.pool = Some(num(it.next())),
            "--pg" => a.pg = Some(num(it.next())),
            "--mib" => a.mib = u64::from(num(it.next())).max(1),
            "--wait" => a.wait = Some(num(it.next())),
            "--device" => a.device = Some(num(it.next())),
            "--weight" => {
                a.weight = match it.next().map(String::as_str) {
                    Some("auto") => Some(None),
                    Some(w) => Some(Some(
                        w.parse::<f64>()
                            .ok()
                            .filter(|w| w.is_finite() && *w >= 0.0)
                            .unwrap_or_else(|| usage()),
                    )),
                    None => usage(),
                }
            }
            "--node" => a.node = Some(num(it.next())),
            "--size" => {
                a.size = Some(
                    it.next()
                        .and_then(|s| parse_size(s))
                        .unwrap_or_else(|| usage()),
                )
            }
            "--name" => a.name = it.next().cloned().unwrap_or_else(|| usage()),
            "--offset" => {
                a.offset = it
                    .next()
                    .and_then(|s| parse_size(s))
                    .unwrap_or_else(|| usage())
            }
            "--sparse" => a.sparse = true,
            "--id" => a.id = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--force" => a.force = true,
            "--epoch" => {
                a.epoch = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| usage()),
                )
            }
            "--ids" => {
                a.ids = it
                    .next()
                    .unwrap_or_else(|| usage())
                    .split(',')
                    .filter(|x| !x.is_empty())
                    .map(String::from)
                    .collect()
            }
            "--group" => a.group = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--new-id" => a.new_id = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--grace" => {
                a.grace = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| usage()),
                )
            }
            "--since" => {
                a.since = Some(
                    it.next()
                        .and_then(|s| hyperstor_ctl::parse_ago(s))
                        .unwrap_or_else(|| usage()),
                )
            }
            "--principal" => a.principal = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--op" => a.op = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--limit" => a.limit = Some(num(it.next()).clamp(1, 10_000)),
            "--retention-days" => a.retention_days = Some(num(it.next())),
            "--on-full" => match it.next().map(String::as_str) {
                Some(v @ ("keep" | "purge")) => a.on_full = Some(v.to_string()),
                _ => usage(),
            },
            "status" | "nodes" | "pools" | "pgs" | "pg-grid" | "volumes" | "create-volume"
            | "delete-volume" | "io-test" | "out" | "in" | "weight" | "drain" | "clone-volume"
            | "rollback-volume" | "flatten-volume" | "expand-volume" | "write-volume"
            | "read-volume" | "enroll"
                if cmd.is_none() =>
            {
                cmd = Some(arg.clone())
            }
            // Two words: the group, then what to do.
            "token" | "op" | "cert" | "map" | "member" | "snapshot" | "trash" | "audit"
                if cmd.is_none() =>
            {
                let sub = it.next().map(String::as_str);
                let ok = match arg.as_str() {
                    "token" => matches!(sub, Some("create" | "list" | "revoke")),
                    "cert" => matches!(sub, Some("list" | "revoke" | "show")),
                    "map" => matches!(sub, Some("backup")),
                    "member" => matches!(sub, Some("list" | "add" | "remove")),
                    "snapshot" => matches!(
                        sub,
                        Some("list" | "create" | "delete" | "group" | "group-delete")
                    ),
                    "trash" => matches!(sub, Some("list" | "restore" | "purge" | "settings")),
                    "audit" => matches!(sub, Some("list" | "settings")),
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
    if (cmd == "out" || cmd == "in" || cmd == "drain" || cmd == "weight")
        == (a.device.is_none() && a.node.is_none())
        || (cmd == "drain" && a.node.is_none())
        || (cmd == "weight") != (a.weight.is_some() && a.device.is_some() && a.node.is_none())
    {
        // out, in, weight and drain need a target; nothing else takes one.
        // weight takes one device and its weight.
        usage();
    }
    let needs_id = matches!(
        cmd.as_str(),
        "token revoke"
            | "op get"
            | "op watch"
            | "op cancel"
            | "member add"
            | "member remove"
            | "snapshot list"
            | "snapshot create"
            | "snapshot delete"
            | "clone-volume"
            | "rollback-volume"
            | "flatten-volume"
            | "trash restore"
            | "trash purge"
            | "expand-volume"
            | "write-volume"
            | "read-volume"
    );
    // The data path's commands: --map instead of the API, if given.
    let data_path = matches!(cmd.as_str(), "write-volume" | "read-volume");
    if ((a.offset != 0) && !data_path) || (a.sparse && cmd != "write-volume") {
        usage();
    }
    // A trashed snapshot, or the volume when there's no epoch.
    let may_epoch = matches!(cmd.as_str(), "trash restore" | "trash purge");
    // The snapshot commands' own flags, and only theirs.
    let needs_epoch = matches!(
        cmd.as_str(),
        "snapshot delete" | "clone-volume" | "rollback-volume"
    );
    if (needs_epoch != a.epoch.is_some() && !may_epoch)
        || ((a.grace.is_some() || a.on_full.is_some()) && cmd != "trash settings")
        || ((a.since.is_some() || a.principal.is_some() || a.op.is_some() || a.limit.is_some())
            && cmd != "audit list")
        || (a.retention_days.is_some() && cmd != "audit settings")
        || (cmd == "snapshot group") == a.ids.is_empty()
        || (cmd == "snapshot group-delete") != a.group.is_some()
        || (a.new_id.is_some() && cmd != "clone-volume")
    {
        usage();
    }
    if (cmd == "cert revoke") != a.serial.is_some()
        || matches!(cmd.as_str(), "cert show" | "write-volume" | "enroll") != a.file.is_some()
        || matches!(cmd.as_str(), "map backup" | "read-volume" | "enroll") != a.out.is_some()
        || (cmd == "member add" && !(a.raft.is_some() && a.map.is_some()))
        || (cmd == "member add" && a.map.as_deref().is_some_and(|m| m.contains(',')))
        || (a.raft.is_some() && cmd != "member add")
        || (a.map.is_some() && cmd != "member add" && !data_path)
        || (matches!(cmd.as_str(), "member add" | "member remove") && a.id.is_none())
    {
        usage();
    }
    if cmd == "cert show" {
        // Local: no API needed, for finding a serial to revoke.
        let path = a.file.clone().expect("checked");
        let pem = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            eprintln!("hyperstorctl: {path}: {e}");
            std::process::exit(2)
        });
        match cert_text(&pem) {
            Some(t) => {
                print!("{t}");
                std::process::exit(0)
            }
            None => {
                eprintln!("hyperstorctl: {path}: no certificate");
                std::process::exit(1)
            }
        }
    }
    if cmd == "enroll" {
        // A management client's identity from a mgmt token (D122): local
        // key, the API pinned to the root the token names.
        std::process::exit(enroll(&api_addr(api.clone()), &a));
    }
    if data_path && let Some(map) = a.map.clone() {
        // Straight to the data path: no API, no certificate.
        // The map service's members, any of which answers.
        let map: Vec<std::net::SocketAddr> = map
            .split(',')
            .map(|m| m.trim().parse())
            .collect::<Result<_, _>>()
            .unwrap_or_else(|_| usage());
        std::process::exit(hyperstor_reactor::block_on(async move {
            volume_io(map, &cmd, &a).await
        }));
    }
    if (cmd == "token create") != a.role.is_some()
        || (a.dry_run && cmd != "drain")
        || (a.ttl != 0 && cmd != "token create")
        || (needs_id && a.id.is_none())
    {
        usage();
    }
    // create-volume, expand-volume and read-volume need a size;
    // delete-volume an id, which only these two take.
    if matches!(
        cmd.as_str(),
        "create-volume" | "expand-volume" | "read-volume"
    ) != a.size.is_some()
        || (cmd == "delete-volume" && a.id.is_none())
        || (a.id.is_some() && cmd != "create-volume" && cmd != "delete-volume" && !needs_id)
        || (a.force && cmd != "delete-volume" && cmd != "member remove")
    {
        usage();
    }
    // Through hyperstor-mgmt, as a person with an API token (§23.9.1):
    // neither the API's address nor certificates nor the socket apply.
    let mgmt = mgmt.or_else(|| std::env::var("HYPERSTOR_MGMT").ok());
    if mgmt.is_some() && (api.is_some() || tls.is_some() || socket.is_some())
        || mgmt.is_none() && (token_file.is_some() || mgmt_ca.is_some())
    {
        usage();
    }
    let mgmt = mgmt.map(|url| mgmt_target(&url, token_file.as_deref(), mgmt_ca.as_deref()));
    // The local socket (break-glass, §23.6): given, or named in the
    // configuration and present, unless an address or certificates were
    // given on the command line.
    let socket = socket.or_else(|| {
        if api.is_some() || tls.is_some() || mgmt.is_some() {
            return None;
        }
        conf("socket").filter(|p| std::path::Path::new(p).exists())
    });
    let api = api_addr(api);
    let tls = match tls
        .or_else(|| conf("tls"))
        .filter(|_| socket.is_none() && mgmt.is_none())
    {
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
        match (&mgmt, &socket) {
            (Some(m), _) => {
                let config = config(m.base.clone())
                    .with_default_header(http::header::AUTHORIZATION, m.bearer.clone());
                let http = match &m.tls {
                    Some(c) => HttpClient::with_tls(c.clone()),
                    None => HttpClient::plaintext(),
                };
                manage(http, config, &cmd, &a).await
            }
            (None, Some(path)) => {
                let uri: http::Uri = "http://localhost".parse().expect("uri");
                let conn =
                    connectrpc::client::Http2Connection::connect_unix(path.clone(), uri.clone())
                        .await?
                        .shared(16);
                manage(conn, config(uri), &cmd, &a).await
            }
            (None, None) => {
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
    let api = mgmt
        .map(|m| m.url)
        .or_else(|| socket.clone())
        .unwrap_or(api);
    let code = match outcome {
        Ok(Next::Exit(code)) => code,
        Ok(Next::Follow(_)) => unreachable!("followed in manage"),
        Ok(Next::IoTest { map, pool }) => {
            hyperstor_reactor::block_on(async move { io_test(map, pool, a.mib).await })
        }
        Ok(Next::VolumeIo { map }) => {
            let cmd = cmd.clone();
            hyperstor_reactor::block_on(async move { volume_io(map, &cmd, &a).await })
        }
        Err(e) => {
            eprintln!("hyperstorctl: {api}: {}", error_text(&e));
            exit_code(&e)
        }
    };
    std::process::exit(code);
}

/// hyperstor-mgmt as the way in: its cluster API under `/api`, and the
/// person's token.
struct Mgmt {
    url: String,
    base: http::Uri,
    bearer: http::HeaderValue,
    tls: Option<std::sync::Arc<connectrpc::rustls::ClientConfig>>,
}

/// `url` (`http://` on loopback, or `https://`), the token from `file`
/// or `$HYPERSTOR_TOKEN` (never an argument: others could read it in the
/// process list), and over HTTPS the host's CAs and `ca`'s.
fn mgmt_target(url: &str, file: Option<&str>, ca: Option<&str>) -> Mgmt {
    let fail = |m: String| -> ! {
        eprintln!("hyperstorctl: {m}");
        std::process::exit(2)
    };
    let url = url.trim_end_matches('/').to_string();
    let base: http::Uri = format!("{url}/api")
        .parse()
        .unwrap_or_else(|_| fail(format!("{url}: not a URL")));
    let loopback = matches!(base.host(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let https = match base.scheme_str() {
        Some("https") => true,
        // A token in clear text only where it doesn't leave the host.
        Some("http") if loopback => false,
        Some("http") => fail(format!("{url}: use https:// beyond loopback")),
        _ => fail(format!("{url}: not an http:// or https:// URL")),
    };
    let token = match file {
        Some(f) => std::fs::read_to_string(f).unwrap_or_else(|e| fail(format!("{f}: {e}"))),
        None => std::env::var("HYPERSTOR_TOKEN").unwrap_or_else(|_| {
            fail("--mgmt needs an API token: $HYPERSTOR_TOKEN or --token-file".into())
        }),
    };
    let token = token.trim();
    if !token.starts_with("hst_") {
        fail("the API token isn't one (hst_…)".into());
    }
    let mut bearer = http::HeaderValue::from_str(&format!("Bearer {token}"))
        .unwrap_or_else(|_| fail("the API token isn't one (hst_…)".into()));
    bearer.set_sensitive(true);
    let tls = https.then(|| {
        // On a host of the cluster, its root too: mgmt's certificate is
        // the cluster CA's by default (D108).
        let ca = ca
            .map(|f| std::fs::read_to_string(f).unwrap_or_else(|e| fail(format!("{f}: {e}"))))
            .or_else(|| {
                conf("tls").and_then(|d| {
                    std::fs::read_to_string(std::path::Path::new(&d).join("root.crt")).ok()
                })
            });
        std::sync::Arc::new(
            hyperstor_pki::web_client_config(ca.as_deref()).unwrap_or_else(|e| fail(e)),
        )
    });
    Mgmt {
        url,
        base,
        bearer,
        tls,
    }
}

enum Next {
    Exit(i32),
    /// Follow this operation (`--wait`).
    Follow(String),
    IoTest {
        map: std::net::SocketAddr,
        pool: u32,
    },
    /// `write-volume`, `read-volume`, through the map service the API named.
    VolumeIo {
        map: Vec<std::net::SocketAddr>,
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
    T::ResponseBody: Unpin,
{
    match cmd.split_once(' ').map_or(cmd, |(g, _)| g) {
        "token" => return tokens(TokenServiceClient::new(transport, config), cmd, a).await,
        "cert" => return certs(CaServiceClient::new(transport, config), cmd, a).await,
        "map" => return map_backup(MapServiceClient::new(transport, config), a).await,
        "member" => return members(MemberServiceClient::new(transport, config), cmd, a).await,
        "trash" | "snapshot" | "clone-volume" | "rollback-volume" | "flatten-volume" => {
            let ops = OperationServiceClient::new(transport.clone(), config.clone());
            let next = if cmd.starts_with("trash") {
                trash(TrashServiceClient::new(transport, config), cmd, a).await?
            } else {
                snapshots(ClusterServiceClient::new(transport, config), cmd, a).await?
            };
            return match next {
                Next::Follow(id) => follow(&ops, id, a).await,
                n => Ok(n),
            };
        }
        "audit" => return audit(AuditServiceClient::new(transport, config), cmd, a).await,
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
                    pg: a.pg,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || pgs_text(&r));
            Next::Exit(0)
        }
        "pg-grid" => {
            let s = status().await?;
            let Some(pool) = first_pool(&s) else {
                eprintln!("hyperstorctl: the cluster has no pools");
                return Ok(Next::Exit(1));
            };
            let r = client
                .get_pg_grid(pb::GetPgGridRequest {
                    pool,
                    since_usage: a.usage.then_some(0),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || pg_grid_text(pool, &r));
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
                    force: a.force,
                    ..Default::default()
                })
                .await
            {
                Ok(r) => {
                    // To the trash (D54), not gone.
                    println!(
                        "volume {id} moved to the trash: purged at {} unless restored (hyperstorctl trash restore --id {id})",
                        hyperstor_ctl::utc(r.into_owned().purge_unix)
                    );
                    Next::Exit(0)
                }
                // Its reason and exit status as every other refusal's.
                Err(e) => return Err(e),
            }
        }
        "weight" => {
            let (Some(device), Some(weight)) = (a.device, a.weight) else {
                unreachable!("checked with the arguments")
            };
            let r = client
                .set_device_weight(pb::SetDeviceWeightRequest {
                    device,
                    weight,
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    let how = if weight.is_some() {
                        "kept as set"
                    } else {
                        "following its capacity"
                    };
                    println!(
                        "device {device} weight {:.4}, {how} (map epoch {}); hyperstorctl status shows the PGs moving",
                        r.weight, r.map_epoch
                    );
                    Next::Exit(0)
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
        "expand-volume" => {
            let r = client
                .expand_volume(pb::ExpandVolumeRequest {
                    id: a.id.clone().expect("checked with the arguments"),
                    size: a.size.expect("checked with the arguments"),
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    show(a, &r, || {
                        r.volume
                            .as_option()
                            .map(|v| format!("volume {} is {} bytes\n", v.id, v.size))
                            .unwrap_or_default()
                    });
                    Next::Exit(0)
                }
                Err(e) if e.code != connectrpc::ErrorCode::Unavailable => {
                    eprintln!("hyperstorctl: {}", error_text(&e));
                    Next::Exit(1)
                }
                Err(e) => return Err(e),
            }
        }
        "write-volume" | "read-volume" => {
            let s = status().await?;
            let map = s.map_address.parse().map_err(|_| {
                connectrpc::ConnectError::internal("the API gave no map service address")
            })?;
            Next::VolumeIo { map: vec![map] }
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
                    name: a.name.clone(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || format!("{}\n", r.token));
            if !a.json && role == "mgmt" {
                let info = r.info.as_option().cloned().unwrap_or_default();
                eprintln!(
                    "Token {} for management client {:?}, single-use, expires {} (UTC). Give it\n\
                     to the client (glidex: gxctl datastore create … --hyperstor-token-file FILE),\n\
                     or enroll with it here:\n  \
                     hyperstorctl --api {} enroll --file FILE --out DIR",
                    info.id,
                    info.name,
                    utc(info.expires_unix),
                    r.api_address
                );
            } else if !a.json {
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

/// `snapshot list|create|delete|group|group-delete`, `clone-volume`,
/// `rollback-volume`, `flatten-volume` (10-volumes.md §10.3, §10.4).
/// The audit log (D112, D113): records newest first, and how long it's kept.
async fn audit<T>(
    client: AuditServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    Ok(match cmd {
        "audit list" => {
            let want = a.limit.unwrap_or(100) as usize;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut records = Vec::new();
            let mut after = String::new();
            // Page by page, up to --limit.
            loop {
                let r = client
                    .list_audit(pb::ListAuditRequest {
                        since_unix: a.since.map_or(0, |s| now.saturating_sub(s)),
                        principal: a.principal.clone().unwrap_or_default(),
                        op: a.op.clone().unwrap_or_default(),
                        limit: (want - records.len()).min(1000) as u32,
                        after,
                        ..Default::default()
                    })
                    .await?
                    .into_owned();
                records.extend(r.records);
                if r.next.is_empty() || records.len() >= want {
                    break;
                }
                after = r.next;
            }
            let r = pb::ListAuditResponse {
                records,
                ..Default::default()
            };
            show(a, &r, || hyperstor_ctl::audit_text(&r));
            Next::Exit(0)
        }
        "audit settings" => {
            let s = match a.retention_days {
                Some(d) => {
                    client
                        .set_audit_settings(pb::SetAuditSettingsRequest {
                            retention_days: d,
                            ..Default::default()
                        })
                        .await?
                        .into_owned()
                        .settings
                }
                None => {
                    client
                        .get_audit_settings(pb::GetAuditSettingsRequest::default())
                        .await?
                        .into_owned()
                        .settings
                }
            };
            let s = s.into_option().unwrap_or_default();
            show(a, &s, || match s.retention_days {
                0 => "audit records are kept forever\n".to_string(),
                d => format!("audit records are kept {d} days\n"),
            });
            Next::Exit(0)
        }
        _ => unreachable!("checked with the arguments"),
    })
}

/// The trash (D54): what's in it, restore, purge, a pool's settings.
async fn trash<T>(
    client: TrashServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    let id = || a.id.clone().unwrap_or_default();
    let what = || match a.epoch {
        Some(e) => format!("snapshot {e} of volume {}", id()),
        None => format!("volume {}", id()),
    };
    Ok(match cmd {
        "trash list" => {
            let r = client
                .list_trash(pb::ListTrashRequest {
                    pool: a.pool.unwrap_or(0),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || hyperstor_ctl::trash_text(&r));
            Next::Exit(0)
        }
        "trash restore" => {
            let r = client
                .restore_from_trash(pb::RestoreFromTrashRequest {
                    id: id(),
                    epoch: a.epoch,
                    name: a.name.clone(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || {
                let v = r.volume.as_option().cloned().unwrap_or_default();
                match a.epoch {
                    Some(_) => format!("{} restored\n", what()),
                    None => format!("volume {} restored as {:?}\n", v.id, v.name),
                }
            });
            Next::Exit(0)
        }
        "trash purge" => {
            let r = client
                .purge_from_trash(pb::PurgeFromTrashRequest {
                    id: id(),
                    epoch: a.epoch,
                    ..Default::default()
                })
                .await?
                .into_owned();
            let op = r.operation.as_option().cloned().unwrap_or_default();
            show(a, &r, || {
                format!(
                    "{} purging: operation {} (hyperstorctl op watch --id {})\n",
                    what(),
                    op.id,
                    op.id
                )
            });
            match a.wait {
                Some(_) => Next::Follow(op.id),
                None => Next::Exit(0),
            }
        }
        "trash settings" => {
            let pool = a.pool.unwrap_or(1);
            let s = if a.grace.is_some() || a.on_full.is_some() {
                let now = client
                    .get_trash_settings(pb::GetTrashSettingsRequest {
                        pool,
                        ..Default::default()
                    })
                    .await?
                    .into_owned()
                    .settings
                    .into_option()
                    .unwrap_or_default();
                client
                    .set_trash_settings(pb::SetTrashSettingsRequest {
                        pool,
                        grace_seconds: a.grace.unwrap_or(now.grace_seconds),
                        on_full: a.on_full.clone().unwrap_or_default(),
                        ..Default::default()
                    })
                    .await?
                    .into_owned()
                    .settings
            } else {
                client
                    .get_trash_settings(pb::GetTrashSettingsRequest {
                        pool,
                        ..Default::default()
                    })
                    .await?
                    .into_owned()
                    .settings
            };
            let s = s.into_option().unwrap_or_default();
            show(a, &s, || {
                format!(
                    "pool {}: kept {} s in the trash; a full pool would {} it (once pools have fullness thresholds, D27)\n",
                    s.pool, s.grace_seconds, s.on_full
                )
            });
            Next::Exit(0)
        }
        _ => unreachable!("checked with the arguments"),
    })
}

async fn snapshots<T>(
    client: ClusterServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    let id = || a.id.clone().unwrap_or_default();
    let epoch = a.epoch.unwrap_or_default();
    let volume = |v: Option<pb::Volume>, what: &str| {
        let v = v.unwrap_or_default();
        println!("volume {} {what}", v.id);
    };
    Ok(match cmd {
        "snapshot list" => {
            let r = client
                .list_snapshots(pb::ListSnapshotsRequest {
                    id: id(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || snapshots_text(&r));
            Next::Exit(0)
        }
        "snapshot create" => {
            let r = client
                .create_snapshot(pb::CreateSnapshotRequest {
                    id: id(),
                    name: a.name.clone(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || {
                let s = r.snapshot.as_option().cloned().unwrap_or_default();
                format!("snapshot {} of volume {} taken\n", s.epoch, id())
            });
            Next::Exit(0)
        }
        "snapshot delete" => {
            let r = client
                .delete_snapshot(pb::DeleteSnapshotRequest {
                    id: id(),
                    epoch,
                    ..Default::default()
                })
                .await?
                .into_owned();
            println!(
                "snapshot {epoch} of volume {} moved to the trash: purged at {} unless restored",
                id(),
                hyperstor_ctl::utc(r.purge_unix)
            );
            Next::Exit(0)
        }
        "snapshot group" => {
            let r = client
                .create_group_snapshot(pb::CreateGroupSnapshotRequest {
                    ids: a.ids.clone(),
                    name: a.name.clone(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || {
                let mut t = format!("group {}\n", r.group);
                for m in &r.members {
                    let s = m.snapshot.as_option().cloned().unwrap_or_default();
                    t.push_str(&format!("  volume {} snapshot {}\n", m.id, s.epoch));
                }
                t
            });
            Next::Exit(0)
        }
        "snapshot group-delete" => {
            let g = a.group.clone().unwrap_or_default();
            let r = client
                .delete_group_snapshot(pb::DeleteGroupSnapshotRequest {
                    group: g.clone(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            println!(
                "group {g}'s snapshots moved to the trash: purged at {} unless restored",
                hyperstor_ctl::utc(r.purge_unix)
            );
            Next::Exit(0)
        }
        "clone-volume" => {
            let r = client
                .clone_volume(pb::CloneVolumeRequest {
                    id: id(),
                    epoch,
                    name: a.name.clone(),
                    new_id: a.new_id.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            if a.json {
                show(a, &r, String::new);
            } else {
                volume(r.volume.as_option().cloned(), "created");
            }
            Next::Exit(0)
        }
        "rollback-volume" => {
            let r = client
                .rollback_volume(pb::RollbackVolumeRequest {
                    id: id(),
                    epoch,
                    ..Default::default()
                })
                .await?
                .into_owned();
            if a.json {
                show(a, &r, String::new);
            } else {
                volume(
                    r.volume.as_option().cloned(),
                    &format!("rolled back to snapshot {epoch}"),
                );
            }
            Next::Exit(0)
        }
        _ => {
            let r = client
                .flatten_volume(pb::FlattenVolumeRequest {
                    id: id(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            let op = r.operation.as_option().cloned().unwrap_or_default();
            show(a, &r, || {
                format!(
                    "volume {} flattening: operation {} (hyperstorctl op watch --id {})\n",
                    id(),
                    op.id,
                    op.id
                )
            });
            match a.wait {
                Some(_) => Next::Follow(op.id),
                None => Next::Exit(0),
            }
        }
    })
}

/// `member list|add|remove`.
async fn members<T>(
    client: MemberServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    let id = || -> Result<u64, connectrpc::ConnectError> {
        a.id.as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| connectrpc::ConnectError::invalid_argument("--id: a member number"))
    };
    let list = match cmd {
        "member list" => {
            let r = client
                .list_members(pb::ListMembersRequest::default())
                .await?
                .into_owned();
            show(a, &r, || members_text(&r.members));
            return Ok(Next::Exit(0));
        }
        "member add" => {
            // A learner catches up first: give it time.
            client
                .add_member_with_options(
                    pb::AddMemberRequest {
                        id: id()?,
                        raft_address: a.raft.clone().unwrap_or_default(),
                        map_address: a.map.clone().unwrap_or_default(),
                        ..Default::default()
                    },
                    CallOptions::default().with_timeout(Duration::from_secs(150)),
                )
                .await?
                .into_owned()
                .members
        }
        _ => {
            client
                .remove_member(pb::RemoveMemberRequest {
                    id: id()?,
                    force: a.force,
                    ..Default::default()
                })
                .await?
                .into_owned()
                .members
        }
    };
    if a.json {
        show(
            a,
            &pb::ListMembersResponse {
                members: list,
                ..Default::default()
            },
            String::new,
        );
    } else {
        print!("{}", members_text(&list));
    }
    Ok(Next::Exit(0))
}

/// `map backup --out FILE`.
async fn map_backup<T>(
    client: MapServiceClient<T>,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    let out = a.out.clone().expect("checked with the arguments");
    // A large map is tens of megabytes: above the default limit.
    let r = client
        .get_map_backup_with_options(
            pb::GetMapBackupRequest::default(),
            CallOptions::default()
                .with_max_message_size(512 << 20)
                .with_timeout(Duration::from_secs(60)),
        )
        .await?
        .into_owned();
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&out)
        .and_then(|mut f| f.write_all(&r.backup).and_then(|()| f.sync_all()));
    if let Err(e) = written {
        eprintln!("hyperstorctl: {out}: {e}");
        return Ok(Next::Exit(1));
    }
    println!(
        "wrote {out}: cluster {}, map epoch {}, {} bytes",
        r.cluster_id,
        r.epoch,
        r.backup.len()
    );
    Ok(Next::Exit(0))
}

/// `cert list|revoke`.
async fn certs<T>(
    client: CaServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    if cmd == "cert list" {
        let r = client
            .list_certificates(pb::ListCertificatesRequest::default())
            .await?
            .into_owned();
        show(a, &r, || certs_text(&r));
        return Ok(Next::Exit(0));
    }
    let serial = a.serial.clone().expect("checked with the arguments");
    let r = client
        .revoke_certificate(pb::RevokeCertificateRequest {
            serial,
            ..Default::default()
        })
        .await?
        .into_owned();
    show(a, &r, || {
        let x = r.revoked.as_option().cloned().unwrap_or_default();
        format!(
            "certificate {} revoked: refused by every map service member until {} (UTC)\n",
            x.serial,
            utc(x.until_unix)
        )
    });
    Ok(Next::Exit(0))
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
    T::ResponseBody: Unpin,
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
    follow(&ops, id, a).await
}

/// Follows operation `id` until it finishes (`WatchOperation`), printing
/// each change, for `--wait SECS` at most (else as long as it runs); a
/// watch cut off (a new leader, a restart) is started again.
async fn follow<T>(
    ops: &OperationServiceClient<T>,
    id: String,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
    T::ResponseBody: Unpin,
{
    let deadline = Instant::now()
        + a.wait.map_or(Duration::from_secs(86400), |s| {
            Duration::from_secs(s.into())
        });
    let mut last = String::new();
    let mut op: Option<pb::Operation> = None;
    let mut print = |o: &pb::Operation| {
        let line = op_line(o);
        if line != last && !a.json {
            println!("{line}");
            last = line;
        }
    };
    while a.poll && Instant::now() < deadline {
        let o = ops
            .get_operation(pb::GetOperationRequest {
                id: id.clone(),
                ..Default::default()
            })
            .await?
            .into_owned();
        print(&o);
        let done = o.state.as_known() != Some(pb::OperationState::OPERATION_STATE_RUNNING);
        op = Some(o);
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    'watch: while !a.poll && Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let opts = CallOptions::default().with_timeout(left);
        let mut stream = match ops
            .watch_operation_with_options(
                pb::WatchOperationRequest {
                    id: id.clone(),
                    ..Default::default()
                },
                opts,
            )
            .await
        {
            Ok(s) => s,
            Err(e) if e.code == connectrpc::ErrorCode::Unavailable => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Err(e) if e.code == connectrpc::ErrorCode::DeadlineExceeded => break,
            Err(e) => return Err(e),
        };
        loop {
            match stream.message().await {
                Ok(Some(m)) => {
                    let Some(o) = m.to_owned_message().operation.into_option() else {
                        continue;
                    };
                    print(&o);
                    let done =
                        o.state.as_known() != Some(pb::OperationState::OPERATION_STATE_RUNNING);
                    op = Some(o);
                    if done {
                        break 'watch;
                    }
                }
                // Ended while running: watch again.
                Ok(None) => continue 'watch,
                Err(e) if e.code == connectrpc::ErrorCode::DeadlineExceeded => break 'watch,
                Err(e) if e.code == connectrpc::ErrorCode::Unavailable => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue 'watch;
                }
                Err(e) => return Err(e),
            }
        }
    }
    match op {
        Some(o) if o.state.as_known() != Some(pb::OperationState::OPERATION_STATE_RUNNING) => {
            if a.json {
                show(a, &o, String::new);
            }
            Ok(Next::Exit(op_exit(&o)))
        }
        _ => {
            eprintln!("hyperstorctl: operation {id} still running");
            Ok(Next::Exit(1))
        }
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
/// Data written and read a piece at a time.
const IO_PIECE: usize = 1 << 20;

/// `write-volume`: `--file` into the volume at `--offset`, holding its
/// lease meanwhile (refused while another client has it attached);
/// `--sparse` skips blocks of zeros, for a volume that reads zeros there.
/// `read-volume`: `--size` bytes at `--offset` into `--out`. Both in whole
/// 4 KiB blocks; a file's last block is padded with zeros (D123).
async fn volume_io(meta: Vec<std::net::SocketAddr>, cmd: &str, a: &Args) -> i32 {
    let fail = |m: String| {
        eprintln!("hyperstorctl: {m}");
        1
    };
    let Some(id) = a.id.as_deref().and_then(parse_volume_id) else {
        return fail("--id: a volume id, up to 32 hexadecimal digits".into());
    };
    if !a.offset.is_multiple_of(4096) {
        return fail("--offset: a multiple of 4096".into());
    }
    let named: Vec<String> = meta.iter().map(|m| m.to_string()).collect();
    let named = named.join(",");
    let cl = match hyperstor_reactor::time::timeout(
        Duration::from_secs(5),
        Client::connect_members(meta, random_u64()),
    )
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return fail(format!("map service {named}: {e}")),
        Err(_) => return fail(format!("map service {named}: no answer in 5 s")),
    };
    // Whoever finds the volume held knows a tool holds it (D153).
    cl.set_lease_label("hyperstorctl");
    let vol = match cl.volume(id).await {
        Ok(Some(v)) => v,
        Ok(None) => return fail(format!("volume {id:032x}: no such volume")),
        Err(e) => return fail(format!("volume {id:032x}: {e}")),
    };
    if cmd == "read-volume" {
        let size = a.size.unwrap_or(0);
        if !size.is_multiple_of(4096) || a.offset.saturating_add(size) > vol.size {
            return fail(format!(
                "{size} bytes at {}: whole blocks inside the volume ({} bytes)",
                a.offset, vol.size
            ));
        }
        let path = a.out.clone().unwrap_or_default();
        let mut f = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => return fail(format!("{path}: {e}")),
        };
        let layers = vol.layers();
        let mut at = 0u64;
        while at < size {
            let n = (IO_PIECE as u64).min(size - at);
            let data = match cl
                .read_at(vol.pool, vol.lineage, &layers, a.offset + at, n)
                .await
            {
                Ok(d) => d,
                Err(e) => return fail(format!("read at {}: {e}", a.offset + at)),
            };
            if let Err(e) = std::io::Write::write_all(&mut f, &data) {
                return fail(format!("{path}: {e}"));
            }
            at += n;
        }
        println!("read {size} bytes at {} into {path}", a.offset);
        return 0;
    }
    let path = a.file.clone().unwrap_or_default();
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => return fail(format!("{path}: {e}")),
    };
    let len = match f.metadata() {
        Ok(m) => m.len().next_multiple_of(4096),
        Err(e) => return fail(format!("{path}: {e}")),
    };
    if a.offset.saturating_add(len) > vol.size {
        return fail(format!(
            "{path}: {len} bytes at {} don't fit the volume ({} bytes)",
            a.offset, vol.size
        ));
    }
    let att = match cl.attach(id).await {
        Ok(v) => v,
        Err(e) => return fail(format!("volume {id:032x}: {e}")),
    };
    let (mut at, mut skipped) = (0u64, 0u64);
    let mut buf = vec![0u8; IO_PIECE];
    let r = loop {
        // Fill a piece; the buffer is zeroed between pieces, so a short
        // last one is padded with zeros to whole blocks.
        let mut n = 0;
        let mut failed = None;
        while n < buf.len() {
            match std::io::Read::read(&mut f, &mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => {
                    failed = Some(format!("{path}: {e}"));
                    break;
                }
            }
        }
        if let Some(e) = failed {
            break Err(e);
        }
        if n == 0 {
            break Ok(());
        }
        let n = n.next_multiple_of(4096);
        let piece = &buf[..n];
        let mut start = 0;
        let mut result = Ok(());
        while start < n {
            // Runs of non-zero blocks are written; zero blocks are skipped
            // with --sparse.
            let zero = |b: &[u8]| b.iter().all(|x| *x == 0);
            let block_zero = zero(&piece[start..start + 4096]);
            let mut end = start + 4096;
            while end < n && zero(&piece[end..end + 4096]) == block_zero {
                end += 4096;
            }
            if block_zero && a.sparse {
                skipped += (end - start) as u64;
            } else if let Err(e) = att
                .write(a.offset + at + start as u64, piece[start..end].to_vec())
                .await
            {
                result = Err(format!("write at {}: {e}", a.offset + at + start as u64));
                break;
            }
            start = end;
        }
        if let Err(e) = result {
            break Err(e);
        }
        at += n as u64;
        buf.iter_mut().for_each(|b| *b = 0);
    };
    let detached = att.detach().await;
    if let Err(e) = r {
        return fail(e);
    }
    if let Err(e) = detached {
        return fail(format!("volume {id:032x}: detaching: {e}"));
    }
    println!(
        "wrote {at} bytes at {} from {path}{}",
        a.offset,
        if a.sparse {
            format!(" ({skipped} bytes of zeros skipped)")
        } else {
            String::new()
        }
    );
    0
}

fn parse_volume_id(s: &str) -> Option<u128> {
    (!s.is_empty() && s.len() <= 32)
        .then(|| u128::from_str_radix(s, 16).ok())
        .flatten()
}

/// `enroll`: a management client's identity from a mgmt token (D122).
/// The key is made here and never leaves; the API is reached over TLS
/// pinned to the root the token names. Writes `admin.crt` (the chain),
/// `admin.key` and `root.crt` into `--out`, the layout `--tls` reads.
fn enroll(api: &str, a: &Args) -> i32 {
    let fail = |m: String| {
        eprintln!("hyperstorctl: {m}");
        1
    };
    let path = a.file.clone().unwrap_or_default();
    let token = match std::fs::read_to_string(&path) {
        Ok(t) => t.trim().to_string(),
        Err(e) => return fail(format!("{path}: {e}")),
    };
    let Some((pin, _)) = hyperstor_pki::parse_join_token(&token) else {
        return fail(format!("{path}: not a hyperstor join token (hsj1-…)"));
    };
    let (key, csr) = match hyperstor_pki::new_request() {
        Ok(k) => k,
        Err(e) => return fail(format!("key: {e}")),
    };
    let tls = match hyperstor_pki::pinned_client_config(pin) {
        Ok(c) => std::sync::Arc::new(c),
        Err(e) => return fail(e),
    };
    let uri: http::Uri = match format!("https://{api}").parse() {
        Ok(u) => u,
        Err(_) => return fail(format!("{api}: not an address")),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(async {
        let config = ClientConfig::new(uri).with_default_timeout(Duration::from_secs(10));
        hyperstor_api::TokenServiceClient::new(HttpClient::with_tls(tls), config)
            .join(pb::JoinRequest {
                token: token.clone(),
                csr,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned())
    });
    let r = match r {
        Ok(r) => r,
        Err(e) => return fail(format!("{api}: {}", error_text(&e))),
    };
    let dir = std::path::PathBuf::from(a.out.clone().unwrap_or_default());
    let write = |name: &str, content: &str, mode: u32| -> Result<(), String> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let p = dir.join(name);
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&p)
            .and_then(|mut f| f.write_all(content.as_bytes()))
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let made = std::fs::create_dir_all(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))
        .and_then(|()| write("admin.key", &key, 0o600))
        .and_then(|()| write("admin.crt", &r.certificate, 0o644))
        .and_then(|()| write("root.crt", &r.root, 0o644));
    if let Err(e) = made {
        return fail(e);
    }
    // The identity as the cluster states it, not read back from the
    // certificate.
    println!(
        "enrolled as {} in cluster {}; certificate in {} (renew within 30 days); map service {}",
        r.identity,
        r.cluster_id,
        dir.display(),
        r.map_addresses
    );
    0
}

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
