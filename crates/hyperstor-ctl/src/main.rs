//! `hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--mgmt URL [--token-file F] [--mgmt-ca FILE] [--in-realm R]]
//!               [--json | -o json|yaml] COMMAND
//!               [--wait SECS] [--pool P] [--pg N] [--mib N] [--device D] [--weight W|auto] [--nearfull PCT] [--backfillfull PCT] [--full PCT] [--node N] [--address A] [--class C] [--endpoints NET=ADDR[,NET=ADDR...]|none] [--classes C,C...|any] [--kind K] [--copies N] [--min N] [--pgs N] [--autoscale on|off] [--cache-grace SECS] [--chunk B] [--domain D] [--size S] [--name N] [--id ID]
//!               [--force] [--role R|default] [--ttl SECS] [--dry-run] [--serial S] [--file F] [--out FILE] [--raft ADDR] [--map ADDR[,ADDR...]]
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
//! - `full-ratios [--nearfull PCT] [--backfillfull PCT] [--full PCT]`:
//!   the fullness thresholds every device is held to, as percentages
//!   (D27, D182), or set one or more of them;
//! - `pool create --name N --classes C[,C...] [--kind replicated|cache]
//!   [--copies N] [--min N] [--pgs N] [--chunk BYTES] [--domain
//!   device|host|rack] [--autoscale on|off] [--cache-grace SECS]
//!   [--force]`: make a pool on devices of those classes (D171); PGs,
//!   unless given, about 100 per device per copy, doubled as devices are
//!   added unless `--autoscale off` (D184); a cache pool's PGs wait
//!   `--cache-grace` seconds (60) for a down member (D189);
//! - `pool set --pool P [--classes C[,C...]|any] [--name N] [--domain
//!   device|host|rack] [--pgs N [--wait SECS]] [--autoscale on|off]
//!   [--cache-grace SECS] [--force]`: place pool P's
//!   PGs on devices of those classes only, or on every device (D164),
//!   with a copy per device, host or rack; or rename it; or split its PGs
//!   to N, twice as many, PG by PG (D173, D187), with `--wait` until it
//!   is done; `--force` allows classes of hard disks and flash both
//!   (D172);
//! - `pool add-device --pool P --device D [--force]`, `pool remove-device
//!   --pool P --device D`: let pool P use one more device, beyond its
//!   classes, or stop using one of them (D180);
//! - `pool delete --pool P [--dry-run] [--force [--wait SECS]]`: delete
//!   pool P, which must hold no volume, snapshot or trash (D176); it
//!   leaves the map at once and devices delete its data (D181);
//!   `--dry-run` lists what it holds; `--force` deletes that first, as an
//!   operation;
//! - `device class --device D --class C|auto`: set a device's class, or
//!   let it follow the device's kind again (D178);
//! - `device endpoints --device D --endpoints NET=ADDR,...|none`: set a
//!   device's endpoints on the cluster's data networks, replacing them
//!   (26-networks.md §29.5, D215); the installer keeps its own node's;
//! - `network list`, `network set --name N --kind control|data|both
//!   --match "subnet IP/LEN"|"ib PREFIX" [--trusted]`, `network remove
//!   --name N`: the cluster's networks (D284), as the cluster file's
//!   `network` lines give them; one with endpoints on it stays, carries
//!   data and keeps its match;
//! - `device upgrade --device D | --all`: convert devices to v0.0.8's
//!   shard format (D274) at their node's next start; one-way, and only
//!   devices whose node runs v0.0.8 or later;
//! - `pool copies --pool P --copies N --min M [--dry-run]`: a replicated
//!   pool's copies and the copies a write needs (D287); more copies
//!   backfill, fewer are dropped; the dry run lists the PGs and each
//!   device's fill;
//! - `device add --node N --address A [--class C]`: append a device to a
//!   node (D167), at weight 0 until the node runs it (restarted with one
//!   more `--shard`) and it reports its capacity;
//! - `device remove --device D [--dry-run] [--force] [--wait SECS]`:
//!   take device D out and, once its PGs have moved, remove it from the
//!   map, with its node if it was the last (D175, D186); `--dry-run`
//!   shows what would move, the pools left short of copies and the space
//!   after; `--force` lets pools keep fewer copies than their size, never
//!   than their min;
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
//! - `copy-volume --id ID --pool P [--epoch E] [--name N] [--new-id ID]
//!   [--wait SECS]` copies a volume (its head, detached, or its snapshot at
//!   E) into a new volume in pool P, and `move-volume --id ID --pool P
//!   [--wait SECS]` moves a detached volume without snapshots there,
//!   keeping its id: operations (§10.6, D214);
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
//! - `token create --role node|meta|mgmt|s3|metrics [--name N] [--ttl
//!   SECS]`, `token list`, `token revoke --id ID`: join tokens (§23.4.2);
//!   a `mgmt` token enrolls a management client named N, such as glidex
//!   (D122), an `s3` token an S3 gateway named N (D233), and a `metrics`
//!   one a scraper that may read only `/metrics` (D234); `create`
//!   prints the token alone on standard output, and how to use it on
//!   standard error;
//! - `enroll --file TOKEN --out DIR`: a management client's or scraper's
//!   identity from a mgmt or metrics token: a key made here, the API pinned
//!   to the token's root; DIR
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
//! - `s3 key create --tenant N [--description TEXT]`, `s3 key list`, `s3
//!   key revoke --id ID`: S3 access keys (14-s3.md §14.2, D231); `create`
//!   prints the key's id and secret on standard output, the secret this
//!   once; gateways refuse a revoked key within seconds. `s3 placement set
//!   --name N --data-pool NAME --index-pool NAME`, `s3 placement list`, `s3
//!   placement delete --name N`: the pools new buckets go in (D232), `default`
//!   unless CreateBucket's LocationConstraint names another; with `--realm
//!   R`, that realm's placements (D249). `s3 bucket list [--tenant N]`, `s3
//!   bucket show --tenant N --name B`, `s3 bucket delete --tenant N --name
//!   B`: every tenant's buckets as `tenant/bucket`, read from the store by
//!   the map service (D246, D254, D255); `delete` only an empty one;
//! - `realm create --name N [--description TEXT]`, `realm list`, `realm
//!   delete --name N`, `realm tenant --tenant N --realm R`, `realm pool
//!   --pool P --realm R[,R...]|any`: realms, the tenants in them and the
//!   pools they may use (20-tenants.md §24.8, D248–D250);
//! - `cert list`, `cert revoke --serial S`: certificates the API issued
//!   and the revocation list (D56); `cert show --file F` prints a
//!   certificate file's identity, serial and expiry, without the API;
//!   `cert renew [--force]` renews the certificate of `--tls DIR` with
//!   itself once past two thirds of its life (now with `--force`), for a
//!   client the installer doesn't renew, from a daily timer.
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
//! `--in-realm R` acts in realm R alone, with the person's cluster-wide
//! roles and their roles in R (D265): lists show only R's volumes,
//! buckets, keys and placements, and what isn't done in a realm is
//! refused. Someone with roles only in realms needs it only to pick one.
//!
//! Each call gives up after `--timeout-secs N` (or `$HYPERSTOR_TIMEOUT`),
//! 5 s by default; the change it asked for may still be made.
//!
//! Exit status: 0 done (and, for `status`, health ok), 1 health warn or
//! error, a failed request or test, 2 usage, 3 the API unreachable (or the
//! member asked isn't the leader), 4 not allowed: no certificate, or one
//! whose role may not do this.

use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{
    AuditServiceClient, CaServiceClient, ClusterServiceClient, MapServiceClient,
    MemberServiceClient, NodeServiceClient, OperationServiceClient, RealmServiceClient,
    S3ServiceClient, TokenServiceClient, TrashServiceClient,
};
use hyperstor_client::Client;
use hyperstor_ctl::*;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--mgmt URL [--token-file F] [--mgmt-ca FILE] [--in-realm R]] [--timeout-secs N] [--json | -o json|yaml] status|nodes|pools|pgs|pg-grid|perf|top-volumes|advise|volumes|create-volume|delete-volume|io-test|out|in|weight|full-ratios|device add|device remove|device class|device endpoints|device upgrade|port list|port role|port lending|network list|network set|network remove|claim list|claim port|claim renew|claim release|pool create|pool set|pool copies|pool add-device|pool remove-device|pool delete|drain|token create|token list|token revoke|op list|op get|op watch|op cancel|cert list|cert revoke|cert show|cert renew|map backup|member list|member add|member remove|snapshot list|snapshot create|snapshot delete|snapshot group|snapshot group-delete|clone-volume|rollback-volume|flatten-volume|copy-volume|move-volume|expand-volume|write-volume|read-volume|enroll|trash list|trash restore|trash purge|trash settings|audit list|audit settings|scrub status|scrub pgs|scrub start|scrub set|s3 key create|s3 key list|s3 key revoke|s3 placement set|s3 placement list|s3 placement delete|s3 bucket list|s3 bucket show|s3 bucket delete|realm create|realm list|realm delete|realm tenant|realm pool|realm host [--wait SECS] [--pool P] [--pg N] [--mib N] [--device D] [--weight W|auto] [--nearfull PCT] [--backfillfull PCT] [--full PCT] [--node N] [--address A] [--class C] [--endpoints NET=ADDR[,NET=ADDR...]|none] [--classes C,C...|any] [--kind K] [--copies N] [--min N] [--pgs N] [--autoscale on|off] [--cache-grace SECS] [--chunk B] [--domain D] [--size S] [--name N] [--id ID] [--force] [--role R] [--ttl SECS] [--holder H] [--host H] [--exclusive] [--all] [--lending off|idle|always|default] [--match subnet CIDR|ib PREFIX] [--trusted] [--dry-run] [--serial S] [--file F] [--out FILE] [--raft ADDR] [--map ADDR[,ADDR...]] [--epoch E] [--ids ID,ID...] [--group G] [--new-id ID] [--grace SECS] [--on-full keep|purge] [--since AGO] [--principal P] [--op OP] [--limit N] [--retention-days N] [--offset O] [--sparse] [--usage] [--overdue] [--deep] [--heat read|write] [--hours N] [--deep-every N] [--hole-repair repair|report] [--deep-rate MIB] [--reason TEXT] [--tenant N] [--realm R[,R...]|any] [--description TEXT] [--data-pool NAME] [--index-pool NAME]";

/// Prints with `ok` on success; a refusal the caller can act on (not
/// found, invalid, claimed) as a message and exit 1. Not being allowed
/// exits 4, as for every command.
fn refused_or<T>(
    r: Result<T, connectrpc::ConnectError>,
    ok: impl FnOnce(T),
) -> Result<Next, connectrpc::ConnectError> {
    match r {
        Ok(v) => {
            ok(v);
            Ok(Next::Exit(0))
        }
        Err(e)
            if matches!(
                e.code,
                connectrpc::ErrorCode::NotFound
                    | connectrpc::ErrorCode::InvalidArgument
                    | connectrpc::ErrorCode::FailedPrecondition
            ) =>
        {
            eprintln!(
                "hyperstorctl: {}",
                e.message.as_deref().unwrap_or("refused")
            );
            Ok(Next::Exit(1))
        }
        Err(e) => Err(e),
    }
}

/// Each call's deadline: `--timeout-secs`, else `$HYPERSTOR_TIMEOUT`.
static TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// How long a call may take before the client gives up (5 s unless
/// set).
fn call_timeout() -> Duration {
    TIMEOUT.get().copied().unwrap_or(Duration::from_secs(5))
}

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
    /// `device endpoints`: `NET=ADDR,...` or `none`.
    endpoints: Option<String>,
    /// `claim ...`: the claim's holder (D205).
    holder: Option<String>,
    /// `claim port`: storage leaves the port.
    exclusive: bool,
    /// `device upgrade`: every device.
    all: bool,
    /// `port lending`: off, idle, always or default (D209).
    lending: Option<String>,
    /// `network set`: `subnet IP/LEN` or `ib PREFIX` (D284).
    matches: Option<String>,
    /// `network set`: plaintext after the handshake (D25).
    trusted: bool,
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
    /// `scrub start`: every `--pg` given.
    pg_list: Vec<u32>,
    /// `scrub start`: deep.
    deep: bool,
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
    /// `pool create`, `pool set`: the autoscaler on or off (D184).
    autoscale: Option<bool>,
    /// `pool create`, `pool set`: how long a cache PG waits for a down
    /// member, in seconds (D189).
    cache_grace: Option<u32>,
    /// `audit list`: how far back (seconds), whose, which operation, how many.
    since: Option<u64>,
    principal: Option<String>,
    op: Option<String>,
    limit: Option<u32>,
    /// `audit settings`.
    retention_days: Option<u32>,
    /// `scrub set` (D224): hours, deep every, hole repair, deep rate; and
    /// why, to weaken scrub (D226).
    scrub_hours: Option<u32>,
    deep_every: Option<u32>,
    hole_repair: Option<String>,
    deep_rate: Option<u32>,
    reason: Option<String>,
    /// `write-volume`, `read-volume`: where in the volume; skip zero blocks.
    offset: u64,
    sparse: bool,
    /// `pg-grid`: each PG's usage too.
    usage: bool,
    /// `scrub pgs`: only PGs overdue.
    overdue: bool,
    /// `pg-grid --heat`: writes (true) or reads.
    heat: Option<bool>,
    /// `weight`: the weight to set, or `None` for `auto` (D158).
    weight: Option<Option<f64>>,
    /// `full-ratios`: nearfull, backfillfull and full, as percentages
    /// (D27, D182); those not given stay as they are.
    ratios: [Option<f64>; 3],
    /// `device add`: where the node's new reactor listens, and the class.
    address: Option<String>,
    class: Option<String>,
    /// `pool set`: the classes, `any` for every device.
    classes: Option<String>,
    /// `pool create`: kind, copies, min, PG count, chunk bytes, domain.
    kind: Option<String>,
    copies: Option<u32>,
    min: Option<u32>,
    pgs: Option<u32>,
    chunk: Option<u64>,
    domain: Option<String>,
    /// Through hyperstor-mgmt, which passes on whole answers only: an
    /// operation is followed by asking again, not by a watch.
    poll: bool,
    /// `s3 key create`.
    tenant: Option<u32>,
    /// `realm tenant|pool|host`, `s3 placement set|delete`: a realm, or
    /// for `realm pool` and `realm host` realms, comma-separated, or `any`
    /// (D248, D249, D251).
    realm: Option<String>,
    /// `realm host`: the host, as it names itself (D261).
    host: Option<String>,
    description: Option<String>,
    /// `s3 placement set`: pool names.
    data_pool: Option<String>,
    index_pool: Option<String>,
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
    let mut in_realm = None;
    let mut mgmt_ca = None;
    let mut timeout = None;
    let mut a = Args {
        endpoints: None,
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
        holder: None,
        host: None,
        exclusive: false,
        all: false,
        lending: None,
        matches: None,
        trusted: false,
        epoch: None,
        ids: Vec::new(),
        group: None,
        new_id: None,
        grace: None,
        on_full: None,
        autoscale: None,
        cache_grace: None,
        since: None,
        principal: None,
        op: None,
        limit: None,
        retention_days: None,
        scrub_hours: None,
        deep_every: None,
        hole_repair: None,
        deep_rate: None,
        reason: None,
        offset: 0,
        sparse: false,
        usage: false,
        overdue: false,
        pg_list: Vec::new(),
        deep: false,
        heat: None,
        weight: None,
        ratios: [None; 3],
        address: None,
        class: None,
        classes: None,
        kind: None,
        copies: None,
        min: None,
        pgs: None,
        chunk: None,
        domain: None,
        poll: false,
        tenant: None,
        realm: None,
        description: None,
        data_pool: None,
        index_pool: None,
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
            "--in-realm" => in_realm = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--mgmt-ca" => mgmt_ca = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--timeout-secs" => timeout = Some(num(it.next())),
            "--json" => a.json = true,
            "-o" => match it.next().map(String::as_str) {
                Some("json") => a.json = true,
                Some("yaml") => (a.json, a.yaml) = (true, true),
                _ => usage(),
            },
            "--role" => a.role = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--ttl" => a.ttl = num(it.next()),
            "--tenant" => a.tenant = Some(num(it.next())),
            "--realm" => a.realm = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--description" => a.description = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--data-pool" => a.data_pool = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--index-pool" => a.index_pool = Some(it.next().cloned().unwrap_or_else(|| usage())),
            // Three words: s3, what, and what to do (D231, D232).
            "s3" if cmd.is_none() => {
                let what = it.next().map(String::as_str);
                let sub = it.next().map(String::as_str);
                let ok = match what {
                    Some("key") => matches!(sub, Some("create" | "list" | "revoke")),
                    Some("placement") => matches!(sub, Some("set" | "list" | "delete")),
                    Some("bucket") => matches!(sub, Some("list" | "show" | "delete")),
                    _ => false,
                };
                if !ok {
                    usage();
                }
                cmd = Some(format!(
                    "s3 {} {}",
                    what.unwrap_or_default(),
                    sub.unwrap_or_default()
                ));
            }
            "--dry-run" => a.dry_run = true,
            "--usage" => a.usage = true,
            "--overdue" => a.overdue = true,
            "--heat" => match it.next().map(String::as_str) {
                Some("read") => a.heat = Some(false),
                Some("write") => a.heat = Some(true),
                _ => usage(),
            },
            "--serial" => a.serial = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--file" => a.file = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--out" => a.out = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--raft" => a.raft = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--map" => a.map = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--pool" => a.pool = Some(num(it.next())),
            "--pg" => {
                let n = num(it.next());
                a.pg = Some(n);
                a.pg_list.push(n);
            }
            "--deep" => a.deep = true,
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
            "--nearfull" | "--backfillfull" | "--full" => {
                let i = ["--nearfull", "--backfillfull", "--full"]
                    .iter()
                    .position(|f| f == arg)
                    .unwrap_or_default();
                a.ratios[i] = Some(
                    it.next()
                        .and_then(|v| v.parse::<f64>().ok())
                        .filter(|v| v.is_finite())
                        .unwrap_or_else(|| usage()),
                );
            }
            "--node" => a.node = Some(num(it.next())),
            "--address" => a.address = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--class" => a.class = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--endpoints" => a.endpoints = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--classes" => a.classes = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--kind" => a.kind = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--copies" => a.copies = Some(num(it.next())),
            "--min" => a.min = Some(num(it.next())),
            "--pgs" => a.pgs = Some(num(it.next())),
            "--chunk" => {
                a.chunk = Some(
                    it.next()
                        .and_then(|s| parse_size(s))
                        .filter(|c| *c <= u64::from(u32::MAX))
                        .unwrap_or_else(|| usage()),
                )
            }
            "--domain" => a.domain = Some(it.next().cloned().unwrap_or_else(|| usage())),
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
            "--holder" => a.holder = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--host" => a.host = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--exclusive" => a.exclusive = true,
            "--all" => a.all = true,
            "--lending" => a.lending = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--match" => a.matches = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--trusted" => a.trusted = true,
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
            "--hours" => a.scrub_hours = Some(num(it.next())),
            "--deep-every" => a.deep_every = Some(num(it.next())),
            "--deep-rate" => a.deep_rate = Some(num(it.next())),
            "--hole-repair" => match it.next().map(String::as_str) {
                Some(v @ ("repair" | "report")) => a.hole_repair = Some(v.to_string()),
                _ => usage(),
            },
            "--reason" => a.reason = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "--on-full" => match it.next().map(String::as_str) {
                Some(v @ ("keep" | "purge")) => a.on_full = Some(v.to_string()),
                _ => usage(),
            },
            "--cache-grace" => a.cache_grace = Some(num(it.next())),
            "--autoscale" => match it.next().map(String::as_str) {
                Some("on") => a.autoscale = Some(true),
                Some("off") => a.autoscale = Some(false),
                _ => usage(),
            },
            "status" | "nodes" | "pools" | "pgs" | "pg-grid" | "perf" | "top-volumes"
            | "advise" | "volumes" | "create-volume" | "delete-volume" | "io-test" | "out"
            | "in" | "weight" | "full-ratios" | "drain" | "clone-volume" | "rollback-volume"
            | "flatten-volume" | "expand-volume" | "write-volume" | "read-volume" | "enroll"
            | "copy-volume" | "move-volume"
                if cmd.is_none() =>
            {
                cmd = Some(arg.clone())
            }
            // Two words: the group, then what to do.
            "token" | "op" | "cert" | "map" | "member" | "snapshot" | "trash" | "audit"
            | "device" | "pool" | "scrub" | "port" | "claim" | "realm" | "network"
                if cmd.is_none() =>
            {
                let sub = it.next().map(String::as_str);
                let ok = match arg.as_str() {
                    "token" => matches!(sub, Some("create" | "list" | "revoke")),
                    "cert" => matches!(sub, Some("list" | "revoke" | "show" | "renew")),
                    "map" => matches!(sub, Some("backup")),
                    "member" => matches!(sub, Some("list" | "add" | "remove")),
                    "snapshot" => matches!(
                        sub,
                        Some("list" | "create" | "delete" | "group" | "group-delete")
                    ),
                    "trash" => matches!(sub, Some("list" | "restore" | "purge" | "settings")),
                    "audit" => matches!(sub, Some("list" | "settings")),
                    "scrub" => matches!(sub, Some("set" | "status" | "pgs" | "start")),
                    "device" => {
                        matches!(
                            sub,
                            Some("add" | "remove" | "class" | "endpoints" | "upgrade")
                        )
                    }
                    "port" => matches!(sub, Some("list" | "role" | "lending")),
                    "network" => matches!(sub, Some("list" | "set" | "remove")),
                    "claim" => matches!(sub, Some("list" | "port" | "renew" | "release")),
                    "realm" => {
                        matches!(
                            sub,
                            Some("create" | "list" | "delete" | "tenant" | "pool" | "host")
                        )
                    }
                    "pool" => {
                        matches!(
                            sub,
                            Some(
                                "set"
                                    | "copies"
                                    | "create"
                                    | "add-device"
                                    | "remove-device"
                                    | "delete"
                            )
                        )
                    }
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
    let timeout = timeout.or_else(|| {
        let v = std::env::var("HYPERSTOR_TIMEOUT").ok()?;
        Some(v.parse().unwrap_or_else(|_| {
            eprintln!("hyperstorctl: bad $HYPERSTOR_TIMEOUT {v}");
            std::process::exit(2)
        }))
    });
    match timeout {
        Some(0) => usage(),
        Some(secs) => TIMEOUT
            .set(Duration::from_secs(secs.into()))
            .expect("set once"),
        None => {}
    }
    // device add takes a node, an address and maybe a class; nothing else
    // takes the last two.
    let device_add = cmd == "device add";
    let device_class = cmd == "device class";
    let device_endpoints = cmd == "device endpoints";
    // pool set takes a pool and what to change (classes, a name, a
    // domain), pool create a name and classes; nothing else takes classes
    // or a domain, and only pool create its other options.
    let create = cmd == "pool create";
    let set = cmd == "pool set";
    if (set
        && (a.pool.is_none()
            || (a.classes.is_none()
                && a.name.is_empty()
                && a.domain.is_none()
                && a.pgs.is_none()
                && a.autoscale.is_none()
                && a.cache_grace.is_none())))
        || (create && (a.name.is_empty() || a.classes.is_none() || a.pool.is_some()))
        || (!set
            && !create
            && (a.classes.is_some()
                || a.domain.is_some()
                || a.autoscale.is_some()
                || a.cache_grace.is_some()))
        || (cmd == "pool copies" && (a.pool.is_none() || a.copies.is_none() || a.min.is_none()))
        || (!create
            && ((a.kind.is_some() && cmd != "network set")
                || ((a.copies.is_some() || a.min.is_some()) && cmd != "pool copies")
                || (a.pgs.is_some() && !set)
                || a.chunk.is_some()))
    {
        usage();
    }
    // pool add-device and remove-device take a pool and one device (D180).
    let pool_device = cmd == "pool add-device" || cmd == "pool remove-device";
    if pool_device
        && (a.pool.is_none()
            || a.device.is_none()
            || a.node.is_some()
            || (a.force && cmd == "pool remove-device"))
    {
        usage();
    }
    if device_add != (a.node.is_some() && a.address.is_some() && a.device.is_none())
        || device_class != (a.device.is_some() && a.class.is_some() && a.node.is_none())
        || (!device_add && a.address.is_some())
        || (!device_add && !device_class && a.class.is_some())
        || device_endpoints != (a.device.is_some() && a.endpoints.is_some() && a.node.is_none())
        || (!device_endpoints && a.endpoints.is_some())
    {
        usage();
    }
    // port role takes a node, a port's name and a role; port list maybe a
    // node; port lending a lending, and a node and name or neither. The
    // claim commands but list take a node, a name and a holder (release:
    // or --force); claim port and renew a ttl.
    let claim_one = matches!(cmd.as_str(), "claim port" | "claim renew" | "claim release");
    let port = cmd.starts_with("port ") || cmd.starts_with("claim ");
    if (cmd == "port role" && (a.node.is_none() || a.name.is_empty()))
        || (port && a.device.is_some())
        || (cmd == "port list" && !a.name.is_empty())
        || (cmd == "port lending") != a.lending.is_some()
        || (cmd == "port lending" && a.node.is_some() == a.name.is_empty())
        || (claim_one
            && (a.node.is_none()
                || a.name.is_empty()
                || (a.holder.is_none() && !(cmd == "claim release" && a.force))))
        || (cmd == "claim list" && (a.node.is_some() || !a.name.is_empty()))
        || (!cmd.starts_with("claim ") && a.holder.is_some())
        || (cmd != "claim port" && a.exclusive)
        || (cmd == "device upgrade" && (a.all == a.device.is_some() || a.node.is_some()))
        || (cmd != "device upgrade" && a.all)
        || (matches!(cmd.as_str(), "claim port" | "claim renew") && a.ttl == 0)
    {
        usage();
    }
    if !device_add
        && !device_class
        && !device_endpoints
        && cmd != "device upgrade"
        && !pool_device
        && !port
        && (cmd == "out"
            || cmd == "in"
            || cmd == "drain"
            || cmd == "weight"
            || cmd == "device remove")
            == (a.device.is_none() && a.node.is_none())
        && !(cmd == "perf" && a.node.is_none())
        || (cmd == "device remove" && a.node.is_some())
        || (cmd == "drain" && a.node.is_none())
        || (cmd == "weight") != (a.weight.is_some() && a.device.is_some() && a.node.is_none())
    {
        // out, in, weight and drain need a target; nothing else takes one.
        // weight takes one device and its weight.
        usage();
    }
    // scrub set takes a pool and something to change; nothing else takes
    // scrub's settings or a reason.
    let scrub_given = a.scrub_hours.is_some()
        || a.deep_every.is_some()
        || a.hole_repair.is_some()
        || a.deep_rate.is_some();
    if (cmd == "scrub set") != scrub_given
        || (cmd == "scrub set" && a.pool.is_none())
        || (a.reason.is_some() && cmd != "scrub set")
    {
        usage();
    }
    // network set takes a name, a kind and a match, maybe --trusted;
    // network remove a name; nothing else takes a match or trust (D284).
    let network_set = cmd == "network set";
    if (network_set && (a.name.is_empty() || a.kind.is_none() || a.matches.is_none()))
        || (cmd == "network remove" && a.name.is_empty())
        || (cmd == "network list" && !a.name.is_empty())
        || (!network_set && (a.matches.is_some() || a.trusted))
    {
        usage();
    }
    // Only full-ratios takes ratios.
    if cmd != "full-ratios" && a.ratios.iter().any(Option::is_some) {
        usage();
    }
    // S3's own flags, on their commands only; key create needs a tenant,
    // placement set a name and both pools, placement delete a name. Realms
    // (D248, D249): create and delete need a name, tenant a tenant and a
    // realm, pool a pool and its realms; a placement may name its realm.
    let placement_set = cmd == "s3 placement set";
    let realm_given = matches!(
        cmd.as_str(),
        "realm tenant" | "realm pool" | "realm host" | "s3 placement set" | "s3 placement delete"
    );
    // Buckets (D246): show and delete name one by --tenant and --name; list
    // may keep to one tenant.
    let bucket_one = matches!(cmd.as_str(), "s3 bucket show" | "s3 bucket delete");
    let tenant_needed = bucket_one || matches!(cmd.as_str(), "s3 key create" | "realm tenant");
    let tenant_ok = match a.tenant {
        // A volume's tenant is optional (D258).
        Some(_) => tenant_needed || cmd == "s3 bucket list" || cmd == "create-volume",
        None => !tenant_needed,
    };
    if !tenant_ok
        || (bucket_one && a.name.is_empty())
        || (a.description.is_some() && !matches!(cmd.as_str(), "s3 key create" | "realm create"))
        || (matches!(cmd.as_str(), "realm tenant" | "realm pool" | "realm host")
            && a.realm.is_none())
        || ((cmd == "realm host") != a.host.is_some())
        || (!realm_given && a.realm.is_some())
        || (matches!(cmd.as_str(), "realm create" | "realm delete") && a.name.is_empty())
        || (cmd == "realm pool" && a.pool.is_none())
        || placement_set != (a.data_pool.is_some() && a.index_pool.is_some())
        || (!placement_set && (a.data_pool.is_some() || a.index_pool.is_some()))
        || (matches!(cmd.as_str(), "s3 placement set" | "s3 placement delete") && a.name.is_empty())
    {
        usage();
    }
    let needs_id = matches!(
        cmd.as_str(),
        "token revoke"
            | "s3 key revoke"
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
            | "copy-volume"
            | "move-volume"
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
    let may_epoch = matches!(
        cmd.as_str(),
        "trash restore" | "trash purge" | "copy-volume"
    );
    // The snapshot commands' own flags, and only theirs.
    let needs_epoch = matches!(
        cmd.as_str(),
        "snapshot delete" | "clone-volume" | "rollback-volume"
    );
    if (needs_epoch != a.epoch.is_some() && !may_epoch)
        || ((a.grace.is_some() || a.on_full.is_some()) && cmd != "trash settings")
        || ((a.since.is_some() || a.principal.is_some() || a.op.is_some()) && cmd != "audit list")
        || (a.limit.is_some() && !matches!(cmd.as_str(), "audit list" | "top-volumes"))
        || (a.heat.is_some() && cmd != "pg-grid")
        || (a.heat.is_some() && a.usage)
        || (a.overdue && cmd != "scrub pgs")
        || (cmd == "scrub pgs" && a.pool.is_none())
        || (a.deep && cmd != "scrub start")
        || (cmd == "scrub start" && a.pool.is_none())
        || (a.pg_list.len() > 1 && cmd != "scrub start")
        || (a.retention_days.is_some() && cmd != "audit settings")
        || (cmd == "snapshot group") == a.ids.is_empty()
        || (cmd == "snapshot group-delete") != a.group.is_some()
        || (a.new_id.is_some() && !matches!(cmd.as_str(), "clone-volume" | "copy-volume"))
        || (matches!(cmd.as_str(), "copy-volume" | "move-volume") && a.pool.is_none())
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
    if (cmd == "token create" || cmd == "port role") != a.role.is_some()
        || (a.dry_run
            && !matches!(
                cmd.as_str(),
                "drain" | "pool delete" | "device remove" | "pool copies"
            ))
        || (cmd == "pool delete" && a.pool.is_none())
        || (a.ttl != 0 && !matches!(cmd.as_str(), "token create" | "claim port" | "claim renew"))
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
        || (a.force
            && cmd != "delete-volume"
            && cmd != "member remove"
            && cmd != "pool set"
            && cmd != "pool create"
            && cmd != "pool add-device"
            && cmd != "pool delete"
            && cmd != "device remove"
            && cmd != "cert renew"
            && cmd != "claim release")
    {
        usage();
    }
    // Through hyperstor-mgmt, as a person with an API token (§23.9.1):
    // neither the API's address nor certificates nor the socket apply.
    let mgmt = mgmt.or_else(|| std::env::var("HYPERSTOR_MGMT").ok());
    if mgmt.is_some() && (api.is_some() || tls.is_some() || socket.is_some())
        || mgmt.is_none() && (token_file.is_some() || mgmt_ca.is_some() || in_realm.is_some())
    {
        usage();
    }
    let mgmt = mgmt.map(|url| {
        mgmt_target(
            &url,
            token_file.as_deref(),
            mgmt_ca.as_deref(),
            in_realm.as_deref(),
        )
    });
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
    let tls_dir = tls.or_else(|| conf("tls")).filter(|_| mgmt.is_none());
    if cmd == "cert renew" {
        let Some(dir) = tls_dir else {
            eprintln!(
                "hyperstorctl: cert renew renews the certificate of --tls DIR \
                 (or the tls line of hyperstorctl.conf), not through --mgmt"
            );
            std::process::exit(2)
        };
        std::process::exit(renew(&api, &dir, a.force));
    }
    let tls = tls_dir
        .as_deref()
        .filter(|_| socket.is_none())
        .map(client_tls);

    // The API calls run on a small tokio runtime; io-test then runs the
    // client library on a reactor, after it.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let target = match (&mgmt, &socket) {
        (Some(m), _) => Target::Mgmt(m),
        (None, Some(path)) => Target::Socket(path.clone()),
        (None, None) => Target::Api(api.clone(), tls.clone()),
    };
    let mut outcome = rt.block_on(run(&target, Call::Cmd(&cmd, &a)));
    let mut api = mgmt
        .as_ref()
        .map(|m| m.url.clone())
        .or_else(|| socket.clone())
        .unwrap_or(api);
    // A call only the leader serves, refused by a member that doesn't
    // lead, goes on to the leader's API (mgmt finds the leader itself).
    if let Err(e) = &outcome
        && LEADER_ONLY.contains(&cmd.as_str())
        && mgmt.is_none()
        && hyperstor_ctl::reason_of(e).as_deref() == Some("not_leader")
    {
        let certs = tls_dir.as_deref().map(client_tls);
        let plain = matches!(target, Target::Api(_, None));
        let route = match rt.block_on(run(&target, Call::Members)) {
            Ok(Next::Members(m)) => hyperstor_ctl::leader_api(&m, certs.is_some(), plain),
            _ => Err("the member list isn't available".into()),
        };
        match route {
            Ok(leader) => {
                eprintln!("hyperstorctl: {api} isn't the leader; asking the leader at {leader}");
                outcome = rt.block_on(run(
                    &Target::Api(leader.clone(), certs),
                    Call::Cmd(&cmd, &a),
                ));
                api = format!("{leader} (the leader)");
            }
            Err(why) => {
                if let Err(e) = &mut outcome {
                    let msg = e.message.take().unwrap_or_default();
                    e.message = Some(format!("{msg}; {why}"));
                }
            }
        }
    }
    let code = match outcome {
        Ok(Next::Exit(code)) => code,
        Ok(Next::Follow(_)) => unreachable!("followed in manage"),
        Ok(Next::Members(_)) => unreachable!("only asked for the leader"),
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
    /// `--in-realm` (D265).
    realm: Option<http::HeaderValue>,
}

/// `url` (`http://` on loopback, or `https://`), the token from `file`
/// or `$HYPERSTOR_TOKEN` (never an argument: others could read it in the
/// process list), and over HTTPS the host's CAs and `ca`'s.
fn mgmt_target(url: &str, file: Option<&str>, ca: Option<&str>, realm: Option<&str>) -> Mgmt {
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
    let realm = realm.map(|r| match hyperstor_api::parse_realms(r) {
        Some(v) if v.len() == 1 => http::HeaderValue::from_str(r).expect("checked"),
        _ => fail(format!(
            "--in-realm {r:?}: a realm is 1-63 of a-z, 0-9 and -"
        )),
    });
    Mgmt {
        url,
        base,
        bearer,
        tls,
        realm,
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
    /// The map service's members, to find the leader.
    Members(Vec<pb::Member>),
}

/// Commands that, refused by a member that doesn't lead, go on to the
/// leader's API: they start work only the leader does, and a refused
/// one did nothing.
const LEADER_ONLY: &[&str] = &[
    "scrub start",
    // S3's keys and placements are rows the leader writes (D231, D232); a
    // refused call wrote nothing, so asking again makes one key, not two.
    "s3 key create",
    "s3 key revoke",
    "s3 placement set",
    "s3 placement delete",
    // So are realms (D248).
    "realm create",
    "realm delete",
    "realm tenant",
    "realm pool",
    "realm host",
];

/// Where the API calls go.
enum Target<'a> {
    Mgmt(&'a Mgmt),
    /// The map service's local socket.
    Socket(String),
    /// An API address, over TLS with these certificates or plain HTTP.
    Api(
        String,
        Option<std::sync::Arc<connectrpc::rustls::ClientConfig>>,
    ),
}

/// What to ask: a command, or the member list.
#[derive(Clone, Copy)]
enum Call<'a> {
    Cmd(&'a str, &'a Args),
    Members,
}

async fn run(target: &Target<'_>, call: Call<'_>) -> Result<Next, connectrpc::ConnectError> {
    let config = |uri: http::Uri| ClientConfig::new(uri).with_default_timeout(call_timeout());
    match target {
        Target::Mgmt(m) => {
            let mut config = config(m.base.clone())
                .with_default_header(http::header::AUTHORIZATION, m.bearer.clone());
            if let Some(r) = &m.realm {
                config = config.with_default_header(
                    http::HeaderName::from_static(hyperstor_api::ACT_IN_REALM_HEADER),
                    r.clone(),
                );
            }
            let http = match &m.tls {
                Some(c) => HttpClient::with_tls(c.clone()),
                None => HttpClient::plaintext(),
            };
            ask(http, config, call).await
        }
        Target::Socket(path) => {
            let uri: http::Uri = "http://localhost".parse().expect("uri");
            let conn = connectrpc::client::Http2Connection::connect_unix(path.clone(), uri.clone())
                .await?
                .shared(16);
            ask(conn, config(uri), call).await
        }
        Target::Api(api, tls) => {
            let scheme = if tls.is_some() { "https" } else { "http" };
            let uri: http::Uri = format!("{scheme}://{api}")
                .parse()
                .map_err(|_| connectrpc::ConnectError::invalid_argument("bad address"))?;
            let http = match tls {
                Some(c) => HttpClient::with_tls(c.clone()),
                None => HttpClient::plaintext(),
            };
            ask(http, config(uri), call).await
        }
    }
}

async fn ask<T>(
    transport: T,
    config: ClientConfig,
    call: Call<'_>,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
    T::ResponseBody: Unpin,
{
    match call {
        Call::Cmd(cmd, a) => manage(transport, config, cmd, a).await,
        Call::Members => Ok(Next::Members(
            MemberServiceClient::new(transport, config)
                .list_members(pb::ListMembersRequest::default())
                .await?
                .into_owned()
                .members,
        )),
    }
}

/// The client certificates in `dir` (§23.8); exits if they can't be read.
fn client_tls(dir: &str) -> std::sync::Arc<connectrpc::rustls::ClientConfig> {
    let dir = std::path::PathBuf::from(dir);
    let read = |f: &str| {
        std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| {
            eprintln!("hyperstorctl: {}: {e}", dir.join(f).display());
            std::process::exit(2)
        })
    };
    let c = hyperstor_pki::client_config(&read("admin.crt"), &read("admin.key"), &read("root.crt"))
        .unwrap_or_else(|e| {
            eprintln!("hyperstorctl: {}: {e}", dir.display());
            std::process::exit(2)
        });
    std::sync::Arc::new(c)
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

/// `scrub set` (28-scrub.md §31.8; D224, D226): with `--reason`, it may
/// weaken scrub; without, a change that would is refused, saying what.
async fn scrub<T>(
    client: ClusterServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
    T::ResponseBody: Unpin,
{
    // `scrub status` and `scrub pgs` (D227): what was last scrubbed when.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match cmd {
        "scrub status" => {
            let r = client
                .get_scrub_status(pb::GetScrubStatusRequest {
                    pool: a.pool,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || hyperstor_ctl::scrub_status_text(&r));
            return Ok(Next::Exit(0));
        }
        "scrub start" => {
            let pool = a.pool.unwrap_or_default();
            let r = client
                .start_scrub(pb::StartScrubRequest {
                    pool,
                    pgs: a.pg_list.clone(),
                    deep: a.deep,
                    ..Default::default()
                })
                .await?
                .into_owned();
            let op = r.operation.as_option().cloned().unwrap_or_default();
            show(a, &r, || {
                format!(
                    "pool {pool}: scrubbing {} PG(s){}: operation {} (hyperstorctl op watch --id {})\n",
                    op.total,
                    if a.deep { " deep" } else { "" },
                    op.id,
                    op.id
                )
            });
            return Ok(match a.wait {
                Some(_) => Next::Follow(op.id),
                None => Next::Exit(0),
            });
        }
        "scrub pgs" => {
            let pool = a.pool.unwrap_or_default();
            let r = client
                .list_pg_scrubs(pb::ListPgScrubsRequest {
                    pool,
                    overdue: a.overdue,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || hyperstor_ctl::pg_scrubs_text(pool, &r, now));
            return Ok(Next::Exit(0));
        }
        _ => {}
    }
    let r = if let Some(reason) = &a.reason {
        let req = pb::WeakenScrubSettingsRequest {
            pool: a.pool.unwrap_or_default(),
            hours: a.scrub_hours,
            deep_every: a.deep_every,
            hole_repair: a.hole_repair.clone(),
            deep_rate_mib: a.deep_rate,
            reason: reason.clone(),
            ..Default::default()
        };
        client
            .weaken_scrub_settings(req)
            .await
            .map(|r| r.into_owned().settings)
    } else {
        let req = pb::SetScrubSettingsRequest {
            pool: a.pool.unwrap_or_default(),
            hours: a.scrub_hours,
            deep_every: a.deep_every,
            hole_repair: a.hole_repair.clone(),
            deep_rate_mib: a.deep_rate,
            ..Default::default()
        };
        client
            .set_scrub_settings(req)
            .await
            .map(|r| r.into_owned().settings)
    };
    let settings = match r {
        Ok(s) => s,
        Err(e) if hyperstor_ctl::reason_of(&e).as_deref() == Some("weakens_scrub") => {
            let msg = e.message.as_deref().unwrap_or("this weakens scrub");
            let msg = msg.split(": use").next().unwrap_or(msg);
            eprintln!("hyperstorctl: {msg}: to do it, give --reason TEXT (D226)");
            return Ok(Next::Exit(1));
        }
        Err(e) if e.code == connectrpc::ErrorCode::InvalidArgument => {
            eprintln!("hyperstorctl: {}", error_text(&e));
            return Ok(Next::Exit(1));
        }
        Err(e) => return Err(e),
    };
    let s = settings.into_option().unwrap_or_default();
    show(a, &s, || {
        let mut t = format!(
            "pool {}: scrub every {} hours, deep every {} passes, holes {}, deep rate {} MiB/s\n",
            a.pool.unwrap_or_default(),
            s.hours,
            s.deep_every,
            if s.hole_repair == "repair" {
                "repaired"
            } else {
                "only reported"
            },
            s.deep_rate_mib
        );
        if !s.weakened.is_empty() {
            t.push_str(&format!(
                "weaker than the defaults: {} (health warns)\n",
                s.weakened.join(", ")
            ));
        }
        t
    });
    Ok(Next::Exit(0))
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
        "s3" => return s3(S3ServiceClient::new(transport, config), cmd, a).await,
        "realm" => return realms(RealmServiceClient::new(transport, config), cmd, a).await,
        "cert" => return certs(CaServiceClient::new(transport, config), cmd, a).await,
        "map" => return map_backup(MapServiceClient::new(transport, config), a).await,
        "member" => return members(MemberServiceClient::new(transport, config), cmd, a).await,
        "trash" | "snapshot" | "clone-volume" | "rollback-volume" | "flatten-volume"
        | "copy-volume" | "move-volume" => {
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
        "scrub" => {
            let ops = OperationServiceClient::new(transport.clone(), config.clone());
            return match scrub(ClusterServiceClient::new(transport, config), cmd, a).await? {
                Next::Follow(id) => follow(&ops, id, a).await,
                n => Ok(n),
            };
        }
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
    // pool delete --force --wait follows its operation.
    let ops = OperationServiceClient::new(transport.clone(), config.clone());
    let realm_client = RealmServiceClient::new(transport.clone(), config.clone());
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
    // For volumes: `--pool`, or the first data pool; for someone with
    // roles only in realms, who doesn't read the cluster's status, the
    // first their realms may use (D262).
    let volume_pool = || async {
        if a.pool.is_some() {
            return Ok(a.pool);
        }
        match status().await {
            Ok(s) => Ok(first_pool(&s)),
            Err(e) if e.code == connectrpc::ErrorCode::PermissionDenied => {
                let r = realm_client
                    .list_realms(pb::ListRealmsRequest::default())
                    .await?
                    .into_owned();
                Ok(r.realms
                    .iter()
                    .flat_map(|r| r.pool_ids.iter())
                    .chain(r.open_pool_ids.iter())
                    .copied()
                    .next())
            }
            Err(e) => Err(e),
        }
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
        "pg-grid" if a.heat.is_some() => {
            let s = status().await?;
            let Some(pool) = first_pool(&s) else {
                eprintln!("hyperstorctl: the cluster has no pools");
                return Ok(Next::Exit(1));
            };
            let r = client
                .get_pg_heat(pb::GetPgHeatRequest {
                    pool,
                    pg: a.pg,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || pg_heat_text(pool, a.heat == Some(true), &r));
            Next::Exit(0)
        }
        "perf" => {
            let r = client
                .list_device_perf(pb::ListDevicePerfRequest {
                    device: a.device,
                    ..Default::default()
                })
                .await?
                .into_owned();
            match (a.device, r.devices.first()) {
                (Some(d), None) => {
                    eprintln!("hyperstorctl: device {d} hasn't reported to the leader");
                    Next::Exit(1)
                }
                (Some(_), Some(d)) => {
                    show(a, &r, || device_perf_text(d));
                    Next::Exit(0)
                }
                (None, _) => {
                    show(a, &r, || perf_text(&r));
                    Next::Exit(0)
                }
            }
        }
        "top-volumes" => {
            let r = client
                .list_top_volumes(pb::ListTopVolumesRequest {
                    limit: a.limit.unwrap_or(0),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || volume_heat_text(&r.volumes));
            Next::Exit(0)
        }
        "advise" => {
            let r = client
                .list_findings(pb::ListFindingsRequest::default())
                .await?
                .into_owned();
            show(a, &r, || findings_text(&r));
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
            let Some(pool) = volume_pool().await? else {
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
            let Some(pool) = volume_pool().await? else {
                eprintln!("hyperstorctl: the cluster has no data pools");
                return Ok(Next::Exit(1));
            };
            let r = client
                .create_volume(pb::CreateVolumeRequest {
                    pool,
                    size: a.size.expect("checked with the arguments"),
                    name: a.name.clone(),
                    id: a.id.clone().unwrap_or_default(),
                    tenant: a.tenant.unwrap_or_default(),
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
        "full-ratios" => {
            let r = if a.ratios.iter().any(Option::is_some) {
                let [nearfull, backfillfull, full] = a.ratios;
                let r = client
                    .set_full_ratios(pb::SetFullRatiosRequest {
                        nearfull,
                        backfillfull,
                        full,
                        ..Default::default()
                    })
                    .await;
                match r {
                    Ok(r) => r.into_owned().ratios,
                    Err(e) if e.code == connectrpc::ErrorCode::InvalidArgument => {
                        eprintln!(
                            "hyperstorctl: {}",
                            e.message.as_deref().unwrap_or("invalid ratios")
                        );
                        return Ok(Next::Exit(1));
                    }
                    Err(e) => return Err(e),
                }
            } else {
                client
                    .get_status(pb::GetStatusRequest::default())
                    .await?
                    .into_owned()
                    .full_ratios
            };
            let r = r.into_option().unwrap_or_default();
            show(a, &r, || {
                format!(
                    "nearfull {}%  backfillfull {}%  full {}%\n",
                    r.nearfull, r.backfillfull, r.full
                )
            });
            Next::Exit(0)
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
        "pool create" => {
            let classes: Vec<String> = a
                .classes
                .clone()
                .unwrap_or_default()
                .split(',')
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect();
            let kind = a.kind.clone().unwrap_or_else(|| "replicated".into());
            let copies = a.copies.unwrap_or(if kind == "cache" { 1 } else { 3 });
            let r = client
                .create_pool(pb::CreatePoolRequest {
                    name: a.name.clone(),
                    kind,
                    size: copies,
                    min_size: a.min,
                    pg_count: a.pgs,
                    chunk_size: a.chunk.map(|c| c as u32),
                    domain: a.domain.clone(),
                    classes,
                    force: a.force,
                    autoscale: a.autoscale,
                    cache_grace: a.cache_grace,
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    println!(
                        "pool {} {} made, {} PGs (map epoch {})",
                        r.pool, a.name, r.pg_count, r.map_epoch
                    );
                    if r.mixed_kinds {
                        eprintln!(
                            "hyperstorctl: warning: its classes hold hard disks and flash; a write waits for its slowest copy"
                        );
                    }
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::FailedPrecondition
                            | connectrpc::ErrorCode::InvalidArgument
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "pool set" => {
            let Some(pool) = a.pool else {
                unreachable!("checked with the arguments")
            };
            // `any`: every device; absent: the classes stay.
            let every_device = a.classes.as_deref() == Some("any");
            let classes: Vec<String> = match a.classes.as_deref() {
                Some(c) if c != "any" => c
                    .split(',')
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            let r = client
                .set_pool(pb::SetPoolRequest {
                    pool,
                    classes: classes.clone(),
                    every_device,
                    name: (!a.name.is_empty()).then(|| a.name.clone()),
                    domain: a.domain.clone(),
                    force: a.force,
                    pg_count: a.pgs,
                    autoscale: a.autoscale,
                    cache_grace: a.cache_grace,
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    let mut what = Vec::new();
                    if let Some(n) = a.pgs {
                        what.push(format!("splits to {n} PGs"));
                    }
                    if let Some(on) = a.autoscale {
                        what.push(format!("autoscale {}", if on { "on" } else { "off" }));
                    }
                    if let Some(g) = a.cache_grace {
                        what.push(format!("cache PGs wait {g} s for a down member"));
                    }
                    if every_device {
                        what.push("places on every device".to_string());
                    } else if !classes.is_empty() {
                        what.push(format!("places on classes {}", classes.join(",")));
                    }
                    if let Some(d) = &a.domain {
                        what.push(format!("keeps a copy per {d}"));
                    }
                    if !a.name.is_empty() {
                        what.push(format!("is named {}", a.name));
                    }
                    println!(
                        "pool {pool} {} (map epoch {}){}",
                        what.join(", "),
                        r.map_epoch,
                        if every_device || !classes.is_empty() || a.domain.is_some() {
                            "; hyperstorctl status shows its PGs moving"
                        } else {
                            ""
                        }
                    );
                    if r.mixed_kinds {
                        eprintln!(
                            "hyperstorctl: warning: these classes hold hard disks and flash; a write waits for its slowest copy"
                        );
                    }
                    // --wait: until the split is done (D187).
                    if let (Some(n), Some(secs)) = (a.pgs, a.wait) {
                        let deadline = Instant::now() + Duration::from_secs(secs.into());
                        loop {
                            let s = status().await?;
                            let p = s.pools.iter().find(|p| p.id == pool);
                            if let Some(p) = p {
                                if p.splitting {
                                    println!("{} of {} PGs split", p.split_done, p.pg_count);
                                } else if p.pg_count >= n {
                                    println!("pool {pool} has {} PGs", p.pg_count);
                                    break;
                                }
                            }
                            if Instant::now() >= deadline {
                                eprintln!("hyperstorctl: pool {pool} still splitting");
                                return Ok(Next::Exit(1));
                            }
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound | connectrpc::ErrorCode::FailedPrecondition
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "pool delete" => {
            let Some(pool) = a.pool else {
                unreachable!("checked with the arguments")
            };
            let r = client
                .delete_pool(pb::DeletePoolRequest {
                    pool,
                    dry_run: a.dry_run,
                    force: a.force,
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    if a.dry_run {
                        show(a, &r, || {
                            if r.volumes.is_empty() {
                                format!("pool {pool} holds nothing; it can be deleted\n")
                            } else {
                                format!(
                                    "pool {pool} holds these volumes (with their snapshots); delete and purge them first:\n{}",
                                    volumes_text(&pb::ListVolumesResponse {
                                        volumes: r.volumes.clone(),
                                        ..Default::default()
                                    })
                                )
                            }
                        });
                        return Ok(Next::Exit(if r.volumes.is_empty() { 0 } else { 1 }));
                    }
                    // Forced (D176): an operation deletes what it holds first.
                    if let Some(op) = r.operation.as_option().cloned() {
                        show(a, &r, || {
                            format!(
                                "pool {pool}: deleting these volumes (with their snapshots), then the pool: operation {} (hyperstorctl op watch --id {})\n{}",
                                op.id,
                                op.id,
                                volumes_text(&pb::ListVolumesResponse {
                                    volumes: r.volumes.clone(),
                                    ..Default::default()
                                })
                            )
                        });
                        return match a.wait {
                            Some(_) => follow(&ops, op.id, a).await,
                            None => Ok(Next::Exit(0)),
                        };
                    }
                    println!(
                        "pool {pool} deleted (map epoch {}); its devices delete its data",
                        r.map_epoch
                    );
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound | connectrpc::ErrorCode::FailedPrecondition
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "pool add-device" | "pool remove-device" => {
            let (Some(pool), Some(device)) = (a.pool, a.device) else {
                unreachable!("checked with the arguments")
            };
            let member = cmd == "pool add-device";
            let r = client
                .set_pool_device(pb::SetPoolDeviceRequest {
                    pool,
                    device,
                    member,
                    force: a.force,
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    println!(
                        "pool {pool} {} device {device} (map epoch {}); hyperstorctl status shows its PGs moving",
                        if member { "uses" } else { "no longer uses" },
                        r.map_epoch
                    );
                    if r.mixed_kinds {
                        eprintln!(
                            "hyperstorctl: warning: the pool's devices hold hard disks and flash; a write waits for its slowest copy"
                        );
                    }
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound | connectrpc::ErrorCode::FailedPrecondition
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "port list" => {
            let r = client
                .list_ports(pb::ListPortsRequest {
                    node: a.node,
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || ports_text(&r));
            Next::Exit(0)
        }
        "port lending" => {
            let lending = a.lending.clone().expect("checked with the arguments");
            let r = client
                .set_lending(pb::SetLendingRequest {
                    node: a.node,
                    name: a.name.clone(),
                    lending: (lending != "default").then(|| lending.clone()),
                    ..Default::default()
                })
                .await;
            let what = a
                .node
                .map_or("cluster".into(), |n| format!("port {n}/{}", a.name));
            refused_or(r, |r| {
                println!(
                    "{what}: lending {lending} (map epoch {})",
                    r.into_owned().map_epoch
                )
            })?
        }
        "claim list" => {
            let r = client
                .list_claims(pb::ListClaimsRequest {
                    holder: a.holder.clone(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || claims_text(&r));
            Next::Exit(0)
        }
        "claim port" | "claim renew" => {
            let (node, name) = (a.node.expect("checked"), a.name.clone());
            let holder = a.holder.clone().expect("checked with the arguments");
            let claim = if cmd == "claim port" {
                client
                    .claim_port(pb::ClaimPortRequest {
                        node,
                        name,
                        holder,
                        exclusive: a.exclusive,
                        ttl_secs: a.ttl,
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned().claim.into_option())
            } else {
                client
                    .renew_claim(pb::RenewClaimRequest {
                        node,
                        name,
                        holder,
                        ttl_secs: a.ttl,
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned().claim.into_option())
            };
            refused_or(claim, |c| {
                if let Some(c) = c {
                    println!(
                        "port {}/{}: claimed by {}, {}, for {} s",
                        c.node,
                        c.name,
                        c.holder,
                        if c.exclusive { "exclusive" } else { "shared" },
                        a.ttl
                    )
                }
            })?
        }
        "claim release" => {
            let node = a.node.expect("checked");
            let r = client
                .release_claim(pb::ReleaseClaimRequest {
                    node,
                    name: a.name.clone(),
                    holder: a.holder.clone().unwrap_or_default(),
                    any: a.force,
                    ..Default::default()
                })
                .await;
            refused_or(r, |r| {
                println!(
                    "port {node}/{}: claim released (map epoch {})",
                    a.name,
                    r.into_owned().map_epoch
                )
            })?
        }
        "port role" => {
            let (Some(node), Some(role)) = (a.node, a.role.clone()) else {
                unreachable!("checked with the arguments")
            };
            let r = client
                .set_port_role(pb::SetPortRoleRequest {
                    node,
                    name: a.name.clone(),
                    role: (role != "default").then(|| role.clone()),
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    println!(
                        "port {node}/{}: role {role} (map epoch {})",
                        a.name,
                        r.into_owned().map_epoch
                    );
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound | connectrpc::ErrorCode::InvalidArgument
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "device endpoints" => {
            let (Some(device), Some(given)) = (a.device, a.endpoints.clone()) else {
                unreachable!("checked with the arguments")
            };
            let mut endpoints = Vec::new();
            if given != "none" {
                for e in given.split(',').filter(|e| !e.is_empty()) {
                    let Some((network, address)) = e.split_once('=') else {
                        eprintln!("hyperstorctl: --endpoints: {e:?} isn't NET=ADDR");
                        return Ok(Next::Exit(2));
                    };
                    endpoints.push(pb::Endpoint {
                        network: network.to_string(),
                        address: address.to_string(),
                        ..Default::default()
                    });
                }
            }
            let n = endpoints.len();
            let r = client
                .set_endpoints(pb::SetEndpointsRequest {
                    device,
                    endpoints,
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    println!(
                        "device {device}: {n} endpoint(s) (map epoch {}); its node listens on them once restarted with --endpoint",
                        r.map_epoch
                    );
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound
                            | connectrpc::ErrorCode::InvalidArgument
                            | connectrpc::ErrorCode::PermissionDenied
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "network list" => {
            let r = client
                .get_status(pb::GetStatusRequest::default())
                .await?
                .into_owned();
            show(a, &r.networks, || hyperstor_ctl::networks_text(&r.networks));
            Next::Exit(0)
        }
        "network set" | "network remove" => {
            let remove = cmd == "network remove";
            let name = a.name.clone();
            let r = client
                .set_network(pb::SetNetworkRequest {
                    network: pb::Network {
                        name: name.clone(),
                        kind: a.kind.clone().unwrap_or_default(),
                        trusted: a.trusted,
                        matches: a.matches.clone().unwrap_or_default(),
                        ..Default::default()
                    }
                    .into(),
                    remove,
                    ..Default::default()
                })
                .await;
            if let Err(e) = &r
                && e.code == connectrpc::ErrorCode::PermissionDenied
            {
                eprintln!(
                    "hyperstorctl: {}",
                    e.message.as_deref().unwrap_or("refused")
                );
                return Ok(Next::Exit(1));
            }
            return refused_or(r, |r| {
                println!(
                    "network {name} {} (map epoch {}); hosts take endpoints on it at their installer's next run",
                    if remove { "removed" } else { "set" },
                    r.into_owned().map_epoch
                )
            });
        }
        "device class" => {
            let (Some(device), Some(class)) = (a.device, a.class.clone()) else {
                unreachable!("checked with the arguments")
            };
            let class = (class != "auto").then_some(class);
            let r = client
                .set_device_class(pb::SetDeviceClassRequest {
                    device,
                    class: class.clone(),
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    let how = if class.is_some() {
                        "kept as set"
                    } else {
                        "following its kind"
                    };
                    println!(
                        "device {device} class {}, {how} (map epoch {}); pools limited to classes move its PGs",
                        r.class, r.map_epoch
                    );
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound | connectrpc::ErrorCode::FailedPrecondition
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "device upgrade" => {
            let r = client
                .upgrade_devices(pb::UpgradeDevicesRequest {
                    devices: a.device.into_iter().collect(),
                    all: a.all,
                    ..Default::default()
                })
                .await;
            refused_or(r, |r| {
                let r = r.into_owned();
                let list = |v: &[u32]| {
                    v.iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                if !r.asked.is_empty() {
                    println!(
                        "devices {} convert to v0.0.8's format at their node's next start (map epoch {}); \
                         builds before v0.0.8 can't open them after",
                        list(&r.asked),
                        r.map_epoch
                    );
                }
                if !r.done.is_empty() {
                    println!("devices {} are already in v0.0.8's format", list(&r.done));
                }
            })?
        }
        "device add" => {
            let (Some(node), Some(address)) = (a.node, a.address.clone()) else {
                unreachable!("checked with the arguments")
            };
            let r = client
                .add_device(pb::AddDeviceRequest {
                    node,
                    address: address.clone(),
                    class: a.class.clone(),
                    ..Default::default()
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_owned();
                    println!(
                        "device {} added to node {node} at {address} (map epoch {}), at weight 0 until it reports its capacity; restart node {node} with one more --shard {address}=PATH to run it",
                        r.device, r.map_epoch
                    );
                    Next::Exit(0)
                }
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound | connectrpc::ErrorCode::FailedPrecondition
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            }
        }
        "pool copies" => {
            let pool = a.pool.expect("checked with the arguments");
            let (size, min) = (a.copies.unwrap_or(0), a.min.unwrap_or(0));
            let r = client
                .set_pool_copies(pb::SetPoolCopiesRequest {
                    pool,
                    size,
                    min,
                    dry_run: a.dry_run,
                    ..Default::default()
                })
                .await;
            let r = match r {
                Ok(r) => r.into_owned(),
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound
                            | connectrpc::ErrorCode::FailedPrecondition
                            | connectrpc::ErrorCode::InvalidArgument
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            };
            show(a, &r, || {
                let mut t = moves_text(&r.moves);
                for f in &r.fill {
                    t.push_str(&format!(
                        "device {} {} after\n",
                        f.device,
                        fill_text(f.used_bytes, f.total_bytes)
                    ));
                }
                t.push_str(&if a.dry_run {
                    format!(
                        "dry run: pool {pool} would keep {size} copies, min {min}; {} PG(s) would change; nothing changed\n",
                        r.moves.len()
                    )
                } else {
                    format!(
                        "pool {pool} keeps {size} copies, min {min} (map epoch {}); {} PG(s) change: new copies backfill, dropped ones are deleted\n",
                        r.map_epoch,
                        r.moves.len()
                    )
                });
                t
            });
            Next::Exit(0)
        }
        "device remove" => {
            let device = a.device.expect("checked with the arguments");
            let r = client
                .remove_device(pb::RemoveDeviceRequest {
                    device,
                    dry_run: a.dry_run,
                    force: a.force,
                    ..Default::default()
                })
                .await;
            let r = match r {
                Ok(r) => r.into_owned(),
                Err(e)
                    if matches!(
                        e.code,
                        connectrpc::ErrorCode::NotFound | connectrpc::ErrorCode::FailedPrecondition
                    ) =>
                {
                    eprintln!(
                        "hyperstorctl: {}",
                        e.message.as_deref().unwrap_or("refused")
                    );
                    return Ok(Next::Exit(1));
                }
                Err(e) => return Err(e),
            };
            show(a, &r, || {
                let mut t = moves_text(&r.moves);
                for p in &r.short_pools {
                    t.push_str(&format!(
                        "pool {} keeps {} of its {} copies, for want of failure domains\n",
                        p.pool, p.copies, p.size
                    ));
                }
                for f in &r.fill {
                    t.push_str(&format!(
                        "device {} {} after\n",
                        f.device,
                        fill_text(f.used_bytes, f.total_bytes)
                    ));
                }
                match r.operation.as_option() {
                    Some(op) => t.push_str(&format!(
                        "device {device} out; it leaves the map once its PGs have moved: operation {} (hyperstorctl op watch --id {})\n",
                        op.id, op.id
                    )),
                    None => t.push_str(&format!(
                        "dry run: {} PG(s) would move off device {device}; nothing changed\n",
                        r.moves.len()
                    )),
                }
                t
            });
            match (r.operation.as_option(), a.wait) {
                (Some(op), Some(_)) => follow(&ops, op.id.clone(), a).await?,
                _ => Next::Exit(0),
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
            if !a.json && role == "metrics" {
                let info = r.info.as_option().cloned().unwrap_or_default();
                eprintln!(
                    "Token {} for scraper {:?}, single-use, expires {} (UTC). On the scraper's\n\
                     host, enroll with it, then point Prometheus at the files (see the docs):\n  \
                     hyperstorctl --api {} enroll --file FILE --out DIR",
                    info.id,
                    info.name,
                    utc(info.expires_unix),
                    r.api_address
                );
            } else if !a.json && role == "mgmt" {
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
                } else if role == "s3" {
                    "--role s3 --address IP"
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

/// `s3 key create|list|revoke`, `s3 placement set|list|delete` (14-s3.md
/// §14.2, D231, D232).
async fn s3<T>(
    client: S3ServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    match cmd {
        "s3 key create" => {
            let r = client
                .create_s3_key(pb::CreateS3KeyRequest {
                    tenant: a.tenant.expect("checked with the arguments"),
                    description: a.description.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            // The id and secret alone on standard output, for scripts.
            let id = r.key.as_option().map(|k| k.id.clone()).unwrap_or_default();
            show(a, &r, || format!("{id} {}\n", r.secret));
            if !a.json {
                eprintln!(
                    "Access key {id} for tenant {}, and its secret: shown this once. Give both to\n\
                     the S3 client (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY).",
                    a.tenant.unwrap_or_default()
                );
            }
        }
        "s3 key list" => {
            let r = client
                .list_s3_keys(pb::ListS3KeysRequest::default())
                .await?
                .into_owned();
            show(a, &r, || s3_keys_text(&r));
        }
        "s3 key revoke" => {
            let id = a.id.clone().expect("checked with the arguments");
            client
                .revoke_s3_key(pb::RevokeS3KeyRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await?;
            println!("S3 key {id} revoked: gateways refuse it within seconds");
        }
        "s3 placement set" => {
            client
                .set_s3_placement(pb::SetS3PlacementRequest {
                    placement: buffa::MessageField::some(pb::S3Placement {
                        name: a.name.clone(),
                        data_pool: a.data_pool.clone().unwrap_or_default(),
                        index_pool: a.index_pool.clone().unwrap_or_default(),
                        realm: a.realm.clone().unwrap_or_default(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .await?;
            println!(
                "S3 placement {}{}: data in {}, index in {}",
                a.name,
                a.realm
                    .as_deref()
                    .map_or(String::new(), |r| format!(" of realm {r}")),
                a.data_pool.as_deref().unwrap_or_default(),
                a.index_pool.as_deref().unwrap_or_default()
            );
        }
        "s3 placement list" => {
            let r = client
                .list_s3_placements(pb::ListS3PlacementsRequest::default())
                .await?
                .into_owned();
            show(a, &r, || s3_placements_text(&r));
        }
        "s3 bucket list" => {
            let r = client
                .list_s3_buckets(pb::ListS3BucketsRequest {
                    tenant: a.tenant.unwrap_or_default(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || s3_buckets_text(&r.buckets));
        }
        "s3 bucket show" => {
            let r = client
                .get_s3_bucket(pb::GetS3BucketRequest {
                    tenant: a.tenant.expect("checked with the arguments"),
                    name: a.name.clone(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            show(a, &r, || {
                s3_buckets_text(
                    r.bucket
                        .as_option()
                        .map(std::slice::from_ref)
                        .unwrap_or(&[]),
                )
            });
        }
        "s3 bucket delete" => {
            let tenant = a.tenant.expect("checked with the arguments");
            client
                .delete_s3_bucket(pb::DeleteS3BucketRequest {
                    tenant,
                    name: a.name.clone(),
                    ..Default::default()
                })
                .await?;
            println!("S3 bucket {tenant}/{} deleted", a.name);
        }
        _ => {
            client
                .delete_s3_placement(pb::DeleteS3PlacementRequest {
                    name: a.name.clone(),
                    realm: a.realm.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .await?;
            println!(
                "S3 placement {} deleted; its buckets keep their pools",
                a.name
            );
        }
    }
    Ok(Next::Exit(0))
}

/// `realm create|list|delete|tenant|pool` (20-tenants.md §24.8, D248,
/// D249).
async fn realms<T>(
    client: RealmServiceClient<T>,
    cmd: &str,
    a: &Args,
) -> Result<Next, connectrpc::ConnectError>
where
    T: connectrpc::client::ClientTransport,
    <T::ResponseBody as http_body::Body>::Error: std::fmt::Display,
{
    let realm = a.realm.clone().unwrap_or_default();
    match cmd {
        "realm create" => {
            let r = client
                .create_realm(pb::CreateRealmRequest {
                    name: a.name.clone(),
                    description: a.description.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            let id = r.realm.as_option().map_or(0, |r| r.id);
            show(a, &r, || format!("realm {} made (id {id})\n", a.name));
        }
        "realm list" => {
            let r = client
                .list_realms(pb::ListRealmsRequest::default())
                .await?
                .into_owned();
            show(a, &r, || realms_text(&r));
        }
        "realm delete" => {
            client
                .delete_realm(pb::DeleteRealmRequest {
                    name: a.name.clone(),
                    ..Default::default()
                })
                .await?;
            println!("realm {} deleted", a.name);
        }
        "realm tenant" => {
            let tenant = a.tenant.expect("checked with the arguments");
            client
                .set_tenant_realm(pb::SetTenantRealmRequest {
                    tenant,
                    realm: realm.clone(),
                    ..Default::default()
                })
                .await?;
            println!("tenant {tenant} is in realm {realm}");
        }
        "realm host" => {
            let host = a.host.clone().expect("checked with the arguments");
            let realms: Vec<String> = if realm == "any" {
                Vec::new()
            } else {
                realm
                    .split(',')
                    .filter(|r| !r.is_empty())
                    .map(String::from)
                    .collect()
            };
            client
                .set_host_realms(pb::SetHostRealmsRequest {
                    host: host.clone(),
                    realms: realms.clone(),
                    ..Default::default()
                })
                .await?;
            if realms.is_empty() {
                println!("host {host}: unbound, it attaches volumes of any realm");
            } else {
                println!(
                    "host {host}: attaches only volumes of realm(s) {}",
                    realms.join(", ")
                );
            }
        }
        _ => {
            let pool = a.pool.expect("checked with the arguments");
            let realms: Vec<String> = if realm == "any" {
                Vec::new()
            } else {
                realm
                    .split(',')
                    .filter(|r| !r.is_empty())
                    .map(String::from)
                    .collect()
            };
            client
                .set_pool_realms(pb::SetPoolRealmsRequest {
                    pool,
                    realms: realms.clone(),
                    ..Default::default()
                })
                .await?;
            if realms.is_empty() {
                println!("pool {pool}: every realm may use it");
            } else {
                println!(
                    "pool {pool}: only realm(s) {} may use it",
                    realms.join(", ")
                );
            }
        }
    }
    Ok(Next::Exit(0))
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
                let full = match s.on_full.as_str() {
                    "purge" => "purges the oldest first",
                    _ => "keeps it",
                };
                format!(
                    "pool {}: kept {} s in the trash; when full, the pool {full}\n",
                    s.pool, s.grace_seconds
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
        "copy-volume" => {
            let r = client
                .copy_volume(pb::CopyVolumeRequest {
                    id: id(),
                    epoch,
                    pool: a.pool.unwrap_or_default(),
                    name: a.name.clone(),
                    new_id: a.new_id.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            let op = r.operation.as_option().cloned().unwrap_or_default();
            show(a, &r, || {
                format!(
                    "volume {} copying to volume {} in pool {}: operation {} \
                     (hyperstorctl op watch --id {})\n",
                    id(),
                    r.new_id,
                    a.pool.unwrap_or_default(),
                    op.id,
                    op.id
                )
            });
            match a.wait {
                Some(_) => Next::Follow(op.id),
                None => Next::Exit(0),
            }
        }
        "move-volume" => {
            let r = client
                .move_volume(pb::MoveVolumeRequest {
                    id: id(),
                    pool: a.pool.unwrap_or_default(),
                    ..Default::default()
                })
                .await?
                .into_owned();
            let op = r.operation.as_option().cloned().unwrap_or_default();
            show(a, &r, || {
                format!(
                    "volume {} moving to pool {}: operation {} (hyperstorctl op watch --id {})\n",
                    id(),
                    a.pool.unwrap_or_default(),
                    op.id,
                    op.id
                )
            });
            match a.wait {
                Some(_) => Next::Follow(op.id),
                None => Next::Exit(0),
            }
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
        let config = ClientConfig::new(uri)
            .with_default_timeout(call_timeout().max(Duration::from_secs(10)));
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
        "enrolled as {} in cluster {}; certificate in {} for 30 days (renew it from a daily timer: \
         hyperstorctl --api ADDR --tls DIR cert renew); map service {}",
        r.identity,
        r.cluster_id,
        dir.display(),
        r.map_addresses
    );
    0
}

/// `cert renew [--force]` (D56, D234): renews the certificate in `dir`
/// (`admin.crt` and `admin.key`) through the API, with itself, once it is
/// past two thirds of its life, or now with `force`. For a client the
/// installer doesn't look after, such as a scraper: run it from a daily
/// timer.
fn renew(api: &str, dir: &str, force: bool) -> i32 {
    let dir = std::path::PathBuf::from(dir);
    let crt = dir.join("admin.crt");
    let cert = match std::fs::read_to_string(&crt) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hyperstorctl: {}: {e}", crt.display());
            return 2;
        }
    };
    if !force && !hyperstor_pki::due_for_renewal(&cert, hyperstor_pki::LEAF_DAYS) {
        println!(
            "{}: not due yet (renewed in the last third of its life)",
            crt.display()
        );
        return 0;
    }
    let tls = client_tls(&dir.to_string_lossy());
    let (key, csr) = match hyperstor_pki::new_request() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("hyperstorctl: key: {e}");
            return 1;
        }
    };
    let uri: http::Uri = match format!("https://{api}").parse() {
        Ok(u) => u,
        Err(_) => {
            eprintln!("hyperstorctl: {api}: not an address");
            return 2;
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let r = rt.block_on(async {
        let config = ClientConfig::new(uri)
            .with_default_timeout(call_timeout().max(Duration::from_secs(10)));
        CaServiceClient::new(HttpClient::with_tls(tls), config)
            .renew_certificate(pb::RenewCertificateRequest {
                csr,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned())
    });
    let r = match r {
        Ok(r) => r,
        Err(e) => {
            eprintln!("hyperstorctl: {api}: {}", error_text(&e));
            return exit_code(&e);
        }
    };
    // Each file whole or not at all: written aside, then renamed over.
    let put = |name: &str, content: &str, mode: u32| -> Result<(), String> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let p = dir.join(name);
        let new = dir.join(format!("{name}.new"));
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&new)
            .and_then(|mut f| f.write_all(content.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&new, &p))
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    if let Err(e) =
        put("admin.key", &key, 0o600).and_then(|()| put("admin.crt", &r.certificate, 0o644))
    {
        eprintln!("hyperstorctl: {e}");
        return 1;
    }
    println!(
        "renewed {} (hyperstorctl cert show --file {} shows its expiry)",
        crt.display(),
        crt.display()
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
