//! The cluster API, `hyperstor.v1.ClusterService` (spec/19-administration.md
//! §23.5, 04-decisions.md D51), served with connectrpc on its own thread so
//! the control path never runs on the map service's reactor (D58).
//!
//! The map service's state is `Rc` on its reactor; every call is a
//! request on a cross-thread channel to [`Meta::admin`](crate::Meta::admin)
//! with a one-shot reply. Reads answer from the published map, writes
//! change the working map, which the publisher fences and publishes.
//!
//! With TLS (`hyperstor-meta --api-tls DIR`), every client presents a
//! certificate chaining to the cluster's root (D53), and its identity
//! decides what it may do (D56, §16): reads for any identity of this
//! cluster except bare-metal hosts, changes for `admin` and `mgmt` only.
//! Without TLS, every request is allowed: for loopback and tests only.
//!
//! A local Unix socket (`--api-socket PATH`) serves the same API for
//! break-glass use (§23.6, D55): the kernel names the caller
//! (`SO_PEERCRED`), and root, the service's own user and members of the
//! `hyperstor-admin` group may do everything, with no certificate.
//!
//! Every change and every refused request is audited (§23.9.4): one JSON
//! line on standard error, which systemd keeps in the journal, with the
//! time, the caller, the operation, its target and the result. Each
//! caller may make 200 calls per second, in bursts of up to 1000
//! (§23.3); more are refused with `resource_exhausted`.

use crate::health::{self, Health};
use crate::{Admin, ClusterMap, NO_DEVICE, PoolKind};
use connectrpc::{ConnectError, RequestContext, ServiceRequest, ServiceResult};
use hyperstor_api::proto::hyperstor::v1 as pb;
use hyperstor_api::{ClusterService, ClusterServiceExt};
use hyperstor_placement::Domain;
use hyperstor_reactor::chan::{Sender, oneshot};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

mod admin;
mod audit;
mod ops;
mod trash;
pub use admin::{Signing, parse_token};

struct Api {
    tx: Sender<Admin>,
    map_address: String,
    /// The cluster id certificates must name; `None` without TLS.
    cluster: Option<String>,
    /// Per caller: tokens left and when they were last topped up.
    buckets: Buckets,
    /// Joins, renewals, tokens and operations (`admin`).
    admin: admin::State,
}

/// What a call needs of its caller.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Any identity of this cluster but a bare-metal host.
    Read,
    /// `admin` or `mgmt`.
    Write,
    /// Any identity of this cluster.
    Identity,
    /// No certificate: the join, which a token authorizes.
    Anyone,
    /// The management service's own identity (`mgmt`) only: its rows.
    Mgmt,
}

pub use hyperstor_api::{REASON_DOMAIN, reason_of, with_reason};

/// A map service error as the API reports it: a member that doesn't lead
/// is `unavailable` (ask the leader), anything else `kind`.
fn meta_error(e: String, kind: fn(String) -> ConnectError, reason: &str) -> ConnectError {
    if e.starts_with("this map service member isn't the leader") {
        with_reason(ConnectError::unavailable(e), "not_leader")
    } else {
        with_reason(kind(e), reason)
    }
}

/// Calls per second per caller, and the burst allowed (§23.3).
const RATE: f64 = 200.0;
const BURST: f64 = 1000.0;

/// One audit record (§23.9.4), as a JSON line: now, as
/// [`crate::audit_log::line_at`] writes it.
pub fn audit_line(principal: &str, op: &str, target: &str, result: &str) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64);
    crate::audit_log::line_at(t, principal, op, target, result)
}

/// The caller on the local socket, from the kernel.
#[derive(Clone, Copy, Debug)]
pub struct LocalPeer {
    pub uid: u32,
    pub pid: Option<i32>,
}

/// Whether a local caller may administer: root, this service's own user,
/// or a member of `admin_gid` (its supplementary groups, from `/proc`).
pub fn local_allowed(uid: u32, own_uid: u32, groups: &[u32], admin_gid: Option<u32>) -> bool {
    uid == 0 || uid == own_uid || admin_gid.is_some_and(|g| groups.contains(&g))
}

fn groups_of(pid: Option<i32>) -> Vec<u32> {
    let Some(pid) = pid else { return Vec::new() };
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("Groups:")).map(|g| {
                g.split_whitespace()
                    .filter_map(|x| x.parse().ok())
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn admin_gid() -> Option<u32> {
    let name = std::ffi::CString::new("hyperstor-admin").ok()?;
    // SAFETY: getgrnam reads a static NUL-terminated name and returns a
    // pointer into static storage, or null; it's read at once.
    let g = unsafe { libc::getgrnam(name.as_ptr()) };
    // SAFETY: a non-null result points to a valid group entry.
    (!g.is_null()).then(|| unsafe { (*g).gr_gid })
}

type Buckets = std::sync::Mutex<std::collections::HashMap<String, (f64, std::time::Instant)>>;

/// Takes one call from `principal`'s token bucket at `now`; false when it
/// is empty. A runaway script can't starve the other callers.
fn take(buckets: &Buckets, principal: &str, now: std::time::Instant) -> bool {
    let mut b = buckets.lock().unwrap_or_else(|e| e.into_inner());
    let (tokens, last) = b.entry(principal.to_string()).or_insert((BURST, now));
    *tokens = (*tokens + now.saturating_duration_since(*last).as_secs_f64() * RATE).min(BURST);
    *last = now;
    if *tokens < 1.0 {
        return false;
    }
    *tokens -= 1.0;
    true
}

/// Records a call: on standard error, and in `.meta`'s audit log (D112).
fn audit(principal: &str, op: &str, target: &str, result: &str) {
    crate::audit_log::record(principal, op, target, result);
}

/// Records a refused call: on standard error, and in `.meta` summarised
/// when it repeats (D147).
fn refused(principal: &str, op: &str, target: &str, e: &ConnectError) {
    crate::audit_log::refused(
        principal,
        op,
        target,
        e.code.as_str(),
        e.message.as_deref().unwrap_or(""),
    );
}

impl Api {
    /// Checks the caller: with TLS, a certificate of this cluster whose
    /// role allows `op` (any but `host` to read; `admin` or `mgmt` to
    /// change); then the caller's rate. Returns who the caller is, for
    /// the audit; a refusal is audited here.
    fn authorize(
        &self,
        ctx: &RequestContext,
        op: &str,
        target: &str,
        write: bool,
    ) -> Result<String, ConnectError> {
        self.authorize_as(
            ctx,
            op,
            target,
            if write { Need::Write } else { Need::Read },
        )
    }

    fn authorize_as(
        &self,
        ctx: &RequestContext,
        op: &str,
        target: &str,
        need: Need,
    ) -> Result<String, ConnectError> {
        let refuse = |who: &str, e: ConnectError| {
            refused(who, op, target, &e);
            e
        };
        let local = ctx.extensions().get::<LocalPeer>().copied();
        let principal = match (&self.cluster, local) {
            (_, Some(p)) => {
                let who = format!("local:uid={}", p.uid);
                // SAFETY: getuid has no preconditions.
                let own = unsafe { libc::getuid() };
                if !local_allowed(p.uid, own, &groups_of(p.pid), admin_gid()) {
                    return Err(refuse(
                        &who,
                        with_reason(
                            ConnectError::permission_denied(format!(
                                "uid {} is not root, the service's user, or in hyperstor-admin",
                                p.uid
                            )),
                            "permission_denied",
                        ),
                    ));
                }
                who
            }
            (None, None) => format!(
                "anonymous@{}",
                ctx.peer_addr().map_or("?".into(), |a| a.to_string())
            ),
            (Some(cluster), None) => {
                let peer = ctx.peer_addr().map_or("?".into(), |a| a.to_string());
                let leaf = ctx
                    .peer_certs()
                    .and_then(|c| c.first())
                    .and_then(|leaf| hyperstor_pki::identity_of(leaf));
                let who = match leaf {
                    // A join is authorized by its token, not a certificate,
                    // and rated by address alone: a new connection is a
                    // new port.
                    _ if need == Need::Anyone => {
                        let ip = ctx.peer_addr().map_or("?".into(), |a| a.ip().to_string());
                        return self.take_rate(op, target, format!("join@{ip}"));
                    }
                    Some(who) => who,
                    None => {
                        return Err(refuse(
                            &format!("unknown@{peer}"),
                            with_reason(
                                ConnectError::unauthenticated(
                                    "a hyperstor client certificate is required",
                                ),
                                "unauthenticated",
                            ),
                        ));
                    }
                };
                if &who.cluster != cluster {
                    return Err(refuse(
                        &who.uri(),
                        with_reason(
                            ConnectError::permission_denied(format!(
                                "{} is not of cluster {cluster}",
                                who.uri()
                            )),
                            "wrong_cluster",
                        ),
                    ));
                }
                // A revoked certificate (D56) gets nothing, not even renewal.
                let leaf = ctx.peer_certs().and_then(|c| c.first());
                if let Some(leaf) = leaf
                    && self.admin.is_revoked(leaf)
                {
                    let serial = hyperstor_pki::serial_of(leaf).unwrap_or_default();
                    return Err(refuse(
                        &who.uri(),
                        with_reason(
                            ConnectError::permission_denied(format!(
                                "certificate {serial} of {} is revoked",
                                who.uri()
                            )),
                            "revoked",
                        ),
                    ));
                }
                let allowed = match need {
                    Need::Write => who.role.may_administer(),
                    Need::Read => who.role != hyperstor_pki::Role::Host,
                    Need::Mgmt => who.role == hyperstor_pki::Role::Mgmt,
                    Need::Identity | Need::Anyone => true,
                };
                if !allowed {
                    return Err(refuse(
                        &who.uri(),
                        with_reason(
                            ConnectError::permission_denied(format!(
                                "{} may not {}",
                                who.uri(),
                                if need == Need::Write {
                                    "change the cluster"
                                } else {
                                    "read the cluster's status"
                                }
                            )),
                            "permission_denied",
                        ),
                    ));
                }
                // mgmt names the person a call is for; no one else may.
                match ctx.header(hyperstor_api::ON_BEHALF_HEADER) {
                    None => who.uri(),
                    Some(v) => match v.to_str() {
                        Ok(p)
                            if who.role == hyperstor_pki::Role::Mgmt
                                && hyperstor_api::valid_person(p) =>
                        {
                            format!("{} for {p}", who.uri())
                        }
                        _ => {
                            return Err(refuse(
                                &who.uri(),
                                with_reason(
                                    ConnectError::permission_denied(format!(
                                        "{} may not act on someone's behalf",
                                        who.uri()
                                    )),
                                    "permission_denied",
                                ),
                            ));
                        }
                    },
                }
            }
        };
        self.take_rate(op, target, principal)
    }

    /// Takes a call from `principal`'s rate (§23.3); a refusal is audited.
    fn take_rate(&self, op: &str, target: &str, principal: String) -> Result<String, ConnectError> {
        if !take(&self.buckets, &principal, std::time::Instant::now()) {
            let e = with_reason(
                ConnectError::resource_exhausted(format!(
                    "more than {RATE} calls per second from {principal}"
                )),
                "rate_limited",
            );
            refused(&principal, op, target, &e);
            return Err(e);
        }
        Ok(principal)
    }

    async fn published(
        &self,
    ) -> Result<
        (
            ClusterMap,
            std::collections::BTreeMap<u32, std::time::Duration>,
            crate::Inconsistent,
            crate::Flapping,
        ),
        ConnectError,
    > {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Published(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        rx.recv()
            .await
            .ok_or_else(|| ConnectError::unavailable("map service stopped"))
    }
}

fn health_pb(h: Health) -> pb::Health {
    match h {
        Health::Ok => pb::Health::HEALTH_OK,
        Health::Warn => pb::Health::HEALTH_WARN,
        Health::Error => pb::Health::HEALTH_ERROR,
    }
}

fn kind(k: PoolKind) -> String {
    match k {
        PoolKind::Replicated => "replicated".into(),
        PoolKind::Ec { k, m } => format!("ec{k}+{m}"),
        PoolKind::Cache => "cache".into(),
    }
}

fn domain(d: Domain) -> String {
    match d {
        Domain::Device => "device",
        Domain::Host => "host",
        Domain::Rack => "rack",
    }
    .into()
}

/// Adds that the leader hasn't committed a map change for `d`: the map
/// shown may be out of date, and failures aren't acted on.
pub fn stalled(s: &mut pb::GetStatusResponse, d: std::time::Duration) {
    s.health = pb::Health::HEALTH_ERROR.into();
    s.reasons.insert(
        0,
        pb::Reason {
            level: pb::Health::HEALTH_ERROR.into(),
            text: format!(
                "the map service hasn't committed a map change for {} s: no majority of members answers? Failures can't be acted on until it does",
                d.as_secs()
            ),
            ..Default::default()
        },
    );
}

/// The status as this member can tell it: `leads` is `Err(why)` on a
/// follower, which hears no heartbeats and gets no scrub results (they go
/// to the leader). A follower asks the leader for those first (I197);
/// only when it can't does it answer from the map alone, which the leader
/// published, without warning about what only the leader knows, and say
/// so in a note; one that knows no leader warns, since nothing can change
/// the map (and `hyperstorctl status --wait` waits for an election).
pub fn status_for(
    map: &ClusterMap,
    heard: &std::collections::BTreeMap<u32, std::time::Duration>,
    inconsistent: &crate::Inconsistent,
    flapping: &crate::Flapping,
    map_address: &str,
    leads: Result<(), String>,
) -> pb::GetStatusResponse {
    match leads {
        Ok(()) => status(map, Some(heard), inconsistent, flapping, map_address),
        Err(why) => {
            let mut s = status(
                map,
                None,
                &Default::default(),
                &Default::default(),
                map_address,
            );
            if why == crate::NO_LEADER {
                s.health = pb::Health::HEALTH_WARN.into();
                s.reasons.push(pb::Reason {
                    level: pb::Health::HEALTH_WARN.into(),
                    text: format!("{why}: the map can't change until one is elected"),
                    ..Default::default()
                });
                return s;
            }
            s.reasons.push(pb::Reason {
                level: pb::Health::HEALTH_OK.into(),
                text: format!(
                    "{why}: nodes' heartbeats and scrub results are only the leader's; ask it for those"
                ),
                ..Default::default()
            });
            s
        }
    }
}

/// Each pool's usable total and free bytes (D156), from what its devices
/// reported: the devices placement may use for it, grouped by its failure
/// domain, as much as it can hold with one shard of each PG per domain
/// (D159). A pool with a device not heard from is left without. Each
/// device gets its own report, raw.
pub fn pool_space(s: &mut pb::GetStatusResponse, map: &ClusterMap, space: &crate::DeviceSpace) {
    for d in s.nodes.iter_mut().flat_map(|n| n.devices.iter_mut()) {
        if let Some(&(total, free)) = space.get(&d.id) {
            (d.total_bytes, d.free_bytes) = (Some(total), Some(free));
        }
    }
    for p in &mut s.pools {
        let Some(info) = map.pools.get(&p.id) else {
            continue;
        };
        // As placement chooses them (hyperstor-placement's `Topology`),
        // grouped by the pool's failure domain.
        let mut domains: BTreeMap<u32, (u128, u128)> = BTreeMap::new();
        let mut reported = true;
        for d in map.devices.values().filter(|d| {
            d.up_in
                && d.weight > 0.0
                && info.uses(d)
                && !(info.id == hyperstor_map::META_POOL
                    && map.kinds.get(&d.id) == Some(&hyperstor_map::DeviceKind::Hdd))
        }) {
            let Some(&(total, free)) = space.get(&d.id) else {
                reported = false;
                break;
            };
            let domain = match info.domain {
                Domain::Device => d.id,
                Domain::Host => d.node,
                Domain::Rack => map.nodes.get(&d.node).map_or(0, |n| n.rack),
            };
            let e = domains.entry(domain).or_default();
            (e.0, e.1) = (e.0 + total as u128, e.1 + free as u128);
        }
        if !reported || domains.is_empty() {
            continue;
        }
        // Each PG puts one of its shards (a copy, or an EC shard of 1/k of
        // the data) in each of `n` domains.
        let (n, k) = match info.kind {
            PoolKind::Ec { k, m } => (k as usize + m as usize, k as u128),
            PoolKind::Replicated | PoolKind::Cache => ((info.size as usize).max(1), 1),
        };
        let (total, free): (Vec<u128>, Vec<u128>) = domains.into_values().unzip();
        p.total_bytes = Some((per_shard(total, n) * k).min(u64::MAX as u128) as u64);
        p.free_bytes = Some((per_shard(free, n) * k).min(u64::MAX as u128) as u64);
    }
}

/// How much of a pool's data fits in domains holding `bytes` each when every
/// PG puts one shard in each of `n` distinct domains (04-decisions.md D159):
/// the largest `y` with `sum(min(b, y)) >= n * y`. A domain larger than
/// that can't fill, as it holds at most one shard of each PG; the largest
/// are capped in turn. Fewer domains than `n` hold one shard each, as
/// placement places them.
fn per_shard(mut bytes: Vec<u128>, n: usize) -> u128 {
    bytes.sort_unstable_by(|a, b| b.cmp(a));
    let n = n.min(bytes.len());
    if n == 0 {
        return 0;
    }
    let mut rest: u128 = bytes.iter().sum();
    let mut capped = 0;
    loop {
        let y = rest / (n - capped) as u128;
        if capped + 1 < n && bytes[capped] > y {
            rest -= bytes[capped];
            capped += 1;
        } else {
            return y;
        }
    }
}

/// The status of `map`, as the API answers it. `heard` is how long ago
/// each node was last heard from, when the map service knows (I43): a
/// node it hasn't heard from since it started is reported, since the map
/// counts it up for the liveness timeout's grace.
pub fn status(
    map: &ClusterMap,
    heard: Option<&std::collections::BTreeMap<u32, std::time::Duration>>,
    inconsistent: &crate::Inconsistent,
    flapping: &crate::Flapping,
    map_address: &str,
) -> pb::GetStatusResponse {
    let mut r = health::report(map);
    // Nodes going down and up again (D149): each down holds the node down
    // longer, and someone should find out why it keeps going.
    for (n, k) in flapping {
        r.reasons.push((
            Health::Warn,
            format!(
                "node {n} flapping ({k} transitions in {} min)",
                crate::FLAP_WINDOW.as_secs() / 60
            ),
        ));
        r.health = r.health.max(Health::Warn);
    }
    // Scrub found copies that differ with no majority to decide
    // (09-replication.md §8.5): someone must look.
    if !inconsistent.is_empty() {
        let objects: u32 = inconsistent.values().sum();
        let pgs: Vec<String> = inconsistent
            .keys()
            .map(|(p, g)| format!("{p}.{g}"))
            .collect();
        r.reasons.push((
            Health::Warn,
            format!(
                "{} PG(s) inconsistent, {objects} object(s) scrub couldn't repair: {}",
                pgs.len(),
                pgs.join(", ")
            ),
        ));
        r.health = r.health.max(Health::Warn);
    }
    if let Some(h) = heard {
        let silent: Vec<String> = map
            .nodes
            .values()
            .filter(|n| n.up && !h.contains_key(&n.id))
            .map(|n| n.id.to_string())
            .collect();
        if !silent.is_empty() {
            r.reasons.push((
                Health::Warn,
                format!(
                    "{} node(s) not heard from since the map service started: {}",
                    silent.len(),
                    silent.join(", ")
                ),
            ));
            r.health = r.health.max(Health::Warn);
        }
    }
    pb::GetStatusResponse {
        cluster_id: map.cluster_id.to_string(),
        epoch: map.epoch,
        health: health_pb(r.health).into(),
        reasons: r
            .reasons
            .iter()
            .map(|(h, text)| pb::Reason {
                level: health_pb(*h).into(),
                text: text.clone(),
                ..Default::default()
            })
            .collect(),
        nodes: map
            .nodes
            .values()
            .map(|n| pb::Node {
                id: n.id,
                rack: n.rack,
                up: n.up,
                last_heard_ms: heard
                    .and_then(|h| h.get(&n.id))
                    .map(|d| d.as_millis() as u64),
                devices: map
                    .devices
                    .values()
                    .filter(|d| d.node == n.id)
                    .map(|d| pb::Device {
                        id: d.id,
                        addr: d.addr.to_string(),
                        up_in: d.up_in,
                        weight: d.weight,
                        class: d.class.clone(),
                        weight_fixed: map.fixed_weights.contains(&d.id),
                        kind: map.kinds.get(&d.id).map_or("", |k| k.name()).into(),
                        class_fixed: map.fixed_classes.contains(&d.id),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect(),
        pools: map
            .pools
            .values()
            .zip(&r.pools)
            .map(|(p, s)| pb::Pool {
                id: p.id,
                name: p.name.clone(),
                kind: kind(p.kind),
                size: p.size.into(),
                min_size: p.min_size.into(),
                pg_count: p.pg_count,
                chunk_size: p.chunk_size,
                domain: domain(p.domain),
                classes: p.classes.clone(),
                mixed_kinds: crate::mixes_disks_and_flash(map, p),
                added_devices: p.added.iter().copied().collect(),
                removed_devices: p.removed.iter().copied().collect(),
                pgs: pb::PgCounts {
                    clean: s.clean,
                    degraded: s.degraded,
                    backfilling: s.backfilling,
                    inactive: s.inactive,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .collect(),
        map_address: map_address.to_string(),
        ..Default::default()
    }
}

/// `pool`'s PG states, a byte each (D154); none while `map` is still at
/// `since`, since they change only with the map.
fn pg_grid(map: &ClusterMap, pool: u32, since: u64) -> Result<pb::GetPgGridResponse, ConnectError> {
    let Some(p) = map.pools.get(&pool) else {
        return Err(ConnectError::not_found(format!("no pool {pool}")));
    };
    let unchanged = since == map.epoch;
    Ok(pb::GetPgGridResponse {
        epoch: map.epoch,
        unchanged,
        states: if unchanged {
            Vec::new()
        } else {
            crate::health::pg_states(map, p).map(|s| s as u8).collect()
        },
        ..Default::default()
    })
}

/// Each PG's usage level (D155) and the fullest PG's bytes: 0 for a PG
/// not reported yet, else 1 + 254 x bytes / max, rounded: a share of the
/// fullest PG.
fn usage_levels(bytes: &[Option<u64>]) -> (Vec<u8>, u64) {
    let max = bytes.iter().flatten().copied().max().unwrap_or(0);
    let levels = bytes
        .iter()
        .map(|b| match b {
            None => 0,
            Some(_) if max == 0 => 1,
            Some(b) => 1 + (254.0 * *b as f64 / max as f64).round() as u8,
        })
        .collect();
    (levels, max)
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl ClusterService for Api {
    async fn get_status(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, pb::GetStatusRequest>,
    ) -> ServiceResult<pb::GetStatusResponse> {
        self.authorize(&ctx, "GetStatus", "cluster", false)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Leads(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let leads = rx
            .recv()
            .await
            .ok_or_else(|| ConnectError::unavailable("map service stopped"))?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Uncommitted(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let uncommitted = rx.recv().await.flatten();
        let (map, ..) = self.published().await?;
        // What only the leader knows, asked of it by a follower (I197).
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::LeaderView(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let mut s = match rx.recv().await.flatten() {
            Some(v) => {
                let mut s = status(
                    &map,
                    Some(&v.heard),
                    &v.inconsistent,
                    &v.flapping,
                    &self.map_address,
                );
                pool_space(&mut s, &map, &v.space);
                s
            }
            // No leader, or it didn't answer: the map alone.
            None => status_for(
                &map,
                &Default::default(),
                &Default::default(),
                &Default::default(),
                &self.map_address,
                leads,
            ),
        };
        if let Some(d) = uncommitted {
            stalled(&mut s, d);
        }
        Ok(s.into())
    }

    async fn list_pgs(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListPgsRequest>,
    ) -> ServiceResult<pb::ListPgsResponse> {
        self.authorize(&ctx, "ListPgs", &format!("pool {}", req.pool), false)?;
        let pool = req.pool;
        let (map, ..) = self.published().await?;
        if !map.pools.contains_key(&pool) {
            return Err(ConnectError::not_found(format!("no pool {pool}")));
        }
        let range = match req.pg {
            Some(pg) => (pool, pg)..=(pool, pg),
            None => (pool, 0)..=(pool, u32::MAX),
        };
        let pgs: Vec<_> = map
            .pgs
            .range(range)
            .map(|((pool, pg), info)| pb::Pg {
                pool: *pool,
                pg: *pg,
                epoch: info.epoch,
                acting: info.acting.clone(),
                up: info.up.clone(),
                ..Default::default()
            })
            .collect();
        if let (Some(pg), true) = (req.pg, pgs.is_empty()) {
            return Err(ConnectError::not_found(format!("no PG {pool}.{pg}")));
        }
        debug_assert_ne!(NO_DEVICE, 0);
        Ok(pb::ListPgsResponse {
            pgs,
            ..Default::default()
        }
        .into())
    }

    async fn get_pg_grid(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::GetPgGridRequest>,
    ) -> ServiceResult<pb::GetPgGridResponse> {
        self.authorize(&ctx, "GetPgGrid", &format!("pool {}", req.pool), false)?;
        let (map, ..) = self.published().await?;
        let mut r = pg_grid(&map, req.pool, req.since_epoch)?;
        if let Some(since) = req.since_usage {
            let (reply, rx) = oneshot();
            self.tx
                .send(Admin::PgUsage {
                    pool: req.pool,
                    since,
                    reply,
                })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            let (version, bytes) = rx
                .recv()
                .await
                .ok_or_else(|| ConnectError::unavailable("map service stopped"))?
                .map_err(|e| meta_error(e, ConnectError::not_found, "not_found"))?;
            r.usage_version = version;
            match bytes {
                None => r.usage_unchanged = true,
                Some(b) => (r.usage, r.max_bytes) = usage_levels(&b),
            }
        }
        Ok(r.into())
    }

    async fn set_device_in(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetDeviceInRequest>,
    ) -> ServiceResult<pb::SetDeviceInResponse> {
        let target = format!("device {} up_in={}", req.device, req.up_in);
        let who = self.authorize(&ctx, "SetDeviceIn", &target, true)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SetDeviceIn {
                device: req.device,
                up_in: req.up_in,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "SetDeviceIn",
            &target,
            &match &r {
                Some(Ok(epoch)) => format!("ok, map epoch {epoch}"),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok(map_epoch)) => Ok(pb::SetDeviceInResponse {
                map_epoch,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) => Err(meta_error(e, ConnectError::not_found, "not_found")),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn set_device_weight(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetDeviceWeightRequest>,
    ) -> ServiceResult<pb::SetDeviceWeightResponse> {
        let target = match req.weight {
            Some(w) => format!("device {} weight={w}", req.device),
            None => format!("device {} weight=auto", req.device),
        };
        let who = self.authorize(&ctx, "SetDeviceWeight", &target, true)?;
        if let Some(w) = req.weight.filter(|w| !w.is_finite() || *w < 0.0) {
            return Err(ConnectError::invalid_argument(format!(
                "a weight is a number of 0 or more, not {w}"
            )));
        }
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SetDeviceWeight {
                device: req.device,
                weight: req.weight,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "SetDeviceWeight",
            &target,
            &match &r {
                Some(Ok((epoch, w))) => format!("ok, weight {w}, map epoch {epoch}"),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok((map_epoch, weight))) => Ok(pb::SetDeviceWeightResponse {
                map_epoch,
                weight,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) => Err(meta_error(e, ConnectError::not_found, "not_found")),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn set_device_class(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetDeviceClassRequest>,
    ) -> ServiceResult<pb::SetDeviceClassResponse> {
        let class = req.class.map(str::to_string);
        let target = format!(
            "device {} class={}",
            req.device,
            class.as_deref().unwrap_or("auto")
        );
        let who = self.authorize(&ctx, "SetDeviceClass", &target, true)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SetDeviceClass {
                device: req.device,
                class,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "SetDeviceClass",
            &target,
            &match &r {
                Some(Ok((epoch, c))) => format!("ok, class {c}, map epoch {epoch}"),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok((map_epoch, class))) => Ok(pb::SetDeviceClassResponse {
                map_epoch,
                class,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) if e.starts_with("no device") => {
                Err(meta_error(e, ConnectError::not_found, "not_found"))
            }
            Some(Err(e)) if e.starts_with("a class is") => Err(meta_error(
                e,
                ConnectError::invalid_argument,
                "invalid_argument",
            )),
            Some(Err(e)) => Err(meta_error(
                e,
                ConnectError::failed_precondition,
                "class_conflict",
            )),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn create_pool(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreatePoolRequest>,
    ) -> ServiceResult<pb::CreatePoolResponse> {
        use hyperstor_map::PoolKind;
        use hyperstor_placement::Domain;
        let classes: Vec<String> = req.classes.iter().map(|c| c.to_string()).collect();
        let target = format!(
            "pool {} {} copies={} classes={}{}",
            req.name,
            req.kind,
            req.size,
            classes.join(","),
            if req.force { " force" } else { "" }
        );
        let who = self.authorize(&ctx, "CreatePool", &target, true)?;
        let bad = |m: String| with_reason(ConnectError::invalid_argument(m), "invalid_argument");
        let kind = match req.kind {
            "replicated" => PoolKind::Replicated,
            "cache" => PoolKind::Cache,
            k => return Err(bad(format!("a pool is replicated or cache, not {k:?}"))),
        };
        let domain = match req.domain.unwrap_or("host") {
            "device" => Domain::Device,
            "host" => Domain::Host,
            "rack" => Domain::Rack,
            d => return Err(bad(format!("a domain is device, host or rack, not {d:?}"))),
        };
        if let Some(c) = classes.iter().find(|c| !crate::valid_class(c)) {
            return Err(bad(format!(
                "a class is 1 to 32 lowercase letters, digits and dashes, not {c:?}"
            )));
        }
        let size = u8::try_from(req.size).map_err(|_| bad(format!("{} copies", req.size)))?;
        let min_size = match req.min_size {
            Some(m) => u8::try_from(m).map_err(|_| bad(format!("min {m}")))?,
            None if kind == PoolKind::Cache => 1,
            None => size.div_ceil(2).max(1),
        };
        let new = crate::NewPool {
            name: req.name.to_string(),
            kind,
            size,
            min_size,
            pg_count: req.pg_count,
            chunk_size: req.chunk_size.unwrap_or(4 << 20),
            domain,
            classes,
            force: req.force,
        };
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::CreatePool { new, reply })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "CreatePool",
            &target,
            &match &r {
                Some(Ok((id, pgs, epoch, mixed))) => format!(
                    "ok, pool {id}, {pgs} PGs, map epoch {epoch}{}",
                    if *mixed {
                        ", forced: mixes hard disks and flash"
                    } else {
                        ""
                    }
                ),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok((pool, pg_count, map_epoch, mixed_kinds))) => Ok(pb::CreatePoolResponse {
                pool,
                pg_count,
                map_epoch,
                mixed_kinds,
                ..Default::default()
            }
            .into()),
            Some(Err(e))
                if e.starts_with("the devices")
                    || e.starts_with("classes")
                    || e.starts_with("a pool is already") =>
            {
                Err(meta_error(
                    e,
                    ConnectError::failed_precondition,
                    "pool_conflict",
                ))
            }
            Some(Err(e)) => Err(meta_error(
                e,
                ConnectError::invalid_argument,
                "invalid_argument",
            )),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn set_pool(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetPoolRequest>,
    ) -> ServiceResult<pb::SetPoolResponse> {
        let classes: Vec<String> = req.classes.iter().map(|c| c.to_string()).collect();
        let force = req.force;
        let mut target = format!("pool {}", req.pool);
        if req.every_device {
            target.push_str(" classes=any");
        } else if !classes.is_empty() {
            target.push_str(&format!(" classes={}", classes.join(",")));
        }
        if let Some(n) = req.name {
            target.push_str(&format!(" name={n}"));
        }
        if let Some(d) = req.domain {
            target.push_str(&format!(" domain={d}"));
        }
        if force {
            target.push_str(" force");
        }
        let who = self.authorize(&ctx, "SetPool", &target, true)?;
        let bad = |m: String| with_reason(ConnectError::invalid_argument(m), "invalid_argument");
        if req.every_device && !classes.is_empty() {
            return Err(bad("every_device takes no classes".into()));
        }
        if let Some(c) = classes.iter().find(|c| !crate::valid_class(c)) {
            return Err(bad(format!(
                "a class is 1 to 32 lowercase letters, digits and dashes, not {c:?}"
            )));
        }
        if let Some(n) = req.name {
            crate::check_pool_name(n).map_err(bad)?;
        }
        let domain = match req.domain {
            None => None,
            Some("device") => Some(Domain::Device),
            Some("host") => Some(Domain::Host),
            Some("rack") => Some(Domain::Rack),
            Some(d) => return Err(bad(format!("a domain is device, host or rack, not {d:?}"))),
        };
        let change = crate::PoolChange {
            classes: (req.every_device || !classes.is_empty()).then_some(classes),
            name: req.name.map(str::to_string),
            domain,
        };
        if change.classes.is_none() && change.name.is_none() && change.domain.is_none() {
            return Err(bad(
                "nothing to change: give classes, a name or a domain".into()
            ));
        }
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SetPool {
                pool: req.pool,
                change,
                force,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "SetPool",
            &target,
            &match &r {
                Some(Ok((epoch, mixed))) => format!(
                    "ok, map epoch {epoch}{}",
                    if *mixed {
                        ", forced: mixes hard disks and flash"
                    } else {
                        ""
                    }
                ),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok((map_epoch, mixed_kinds))) => Ok(pb::SetPoolResponse {
                map_epoch,
                mixed_kinds,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) if e.starts_with("no pool") => {
                Err(meta_error(e, ConnectError::not_found, "not_found"))
            }
            Some(Err(e)) => Err(meta_error(
                e,
                ConnectError::failed_precondition,
                "pool_conflict",
            )),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn delete_pool(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeletePoolRequest>,
    ) -> ServiceResult<pb::DeletePoolResponse> {
        let pool = req.pool;
        let dry_run = req.dry_run;
        let target = format!("pool {pool}{}", if dry_run { " dry-run" } else { "" });
        let who = self.authorize(&ctx, "DeletePool", &target, !dry_run)?;
        let (map, ..) = self.published().await?;
        if !map.pools.contains_key(&pool) {
            return Err(with_reason(
                ConnectError::not_found(format!("no pool {pool}")),
                "not_found",
            ));
        }
        if pool == hyperstor_map::META_POOL {
            return Err(with_reason(
                ConnectError::failed_precondition(
                    ".meta holds every volume's header; it can't be deleted",
                ),
                "pool_conflict",
            ));
        }
        // What the pool holds, the trash with it (D176): volumes, and so
        // their snapshots.
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::ListVolumes {
                pool,
                trash: true,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let held = volume_result(rx.recv().await)?;
        let volumes: Vec<pb::Volume> = held.iter().map(volume_pb).collect();
        if dry_run {
            return Ok(pb::DeletePoolResponse {
                volumes,
                ..Default::default()
            }
            .into());
        }
        let r = if held.is_empty() {
            let (reply, rx) = oneshot();
            self.tx
                .send(Admin::DeletePool { pool, reply })
                .map_err(|_| ConnectError::unavailable("map service stopped"))?;
            rx.recv().await
        } else {
            Some(Err(format!(
                "pool {pool} holds {} volume(s), with their snapshots and the trash; delete and purge them first (--dry-run lists them)",
                held.len()
            )))
        };
        audit(
            &who,
            "DeletePool",
            &target,
            &match &r {
                Some(Ok(epoch)) => format!("ok, map epoch {epoch}"),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok(map_epoch)) => Ok(pb::DeletePoolResponse {
                map_epoch,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) if e.starts_with("no pool") => {
                Err(meta_error(e, ConnectError::not_found, "not_found"))
            }
            Some(Err(e)) => Err(meta_error(
                e,
                ConnectError::failed_precondition,
                "pool_conflict",
            )),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn set_pool_device(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::SetPoolDeviceRequest>,
    ) -> ServiceResult<pb::SetPoolDeviceResponse> {
        let target = format!(
            "pool {} {} device {}{}",
            req.pool,
            if req.member { "add" } else { "remove" },
            req.device,
            if req.force { " force" } else { "" }
        );
        let who = self.authorize(&ctx, "SetPoolDevice", &target, true)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::SetPoolDevice {
                pool: req.pool,
                device: req.device,
                member: req.member,
                force: req.force,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "SetPoolDevice",
            &target,
            &match &r {
                Some(Ok((epoch, mixed))) => format!(
                    "ok, map epoch {epoch}{}",
                    if *mixed {
                        ", forced: mixes hard disks and flash"
                    } else {
                        ""
                    }
                ),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok((map_epoch, mixed_kinds))) => Ok(pb::SetPoolDeviceResponse {
                map_epoch,
                mixed_kinds,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) if e.starts_with("no pool") || e.starts_with("no device") => {
                Err(meta_error(e, ConnectError::not_found, "not_found"))
            }
            Some(Err(e)) => Err(meta_error(
                e,
                ConnectError::failed_precondition,
                "pool_conflict",
            )),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn add_device(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::AddDeviceRequest>,
    ) -> ServiceResult<pb::AddDeviceResponse> {
        let class = req.class.map(str::to_string);
        let target = format!(
            "node {} address {}{}",
            req.node,
            req.address,
            class
                .as_ref()
                .map_or(String::new(), |c| format!(" class {c}"))
        );
        let who = self.authorize(&ctx, "AddDevice", &target, true)?;
        let addr = req
            .address
            .parse::<SocketAddr>()
            .ok()
            .filter(|a| !a.ip().is_unspecified() && a.port() != 0)
            .ok_or_else(|| {
                with_reason(
                    ConnectError::invalid_argument(format!(
                        "{}: not an address a node can listen on",
                        req.address
                    )),
                    "invalid_argument",
                )
            })?;
        if let Some(c) = class.as_deref().filter(|c| !crate::valid_class(c)) {
            return Err(with_reason(
                ConnectError::invalid_argument(format!(
                    "a class is 1 to 32 lowercase letters, digits and dashes, not {c:?}"
                )),
                "invalid_argument",
            ));
        }
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::AddDevice {
                node: req.node,
                addr,
                class,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "AddDevice",
            &target,
            &match &r {
                Some(Ok((device, epoch))) => format!("ok, device {device}, map epoch {epoch}"),
                Some(Err(e)) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        match r {
            Some(Ok((device, map_epoch))) => Ok(pb::AddDeviceResponse {
                device,
                map_epoch,
                ..Default::default()
            }
            .into()),
            Some(Err(e)) if e.starts_with("no node") => {
                Err(meta_error(e, ConnectError::not_found, "not_found"))
            }
            Some(Err(e)) => Err(meta_error(
                e,
                ConnectError::failed_precondition,
                "device_conflict",
            )),
            None => Err(ConnectError::unavailable("map service stopped")),
        }
    }

    async fn list_volumes(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListVolumesRequest>,
    ) -> ServiceResult<pb::ListVolumesResponse> {
        self.authorize(&ctx, "ListVolumes", &format!("pool {}", req.pool), false)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::ListVolumes {
                pool: req.pool,
                trash: false,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let vols = volume_result(rx.recv().await)?;
        Ok(pb::ListVolumesResponse {
            volumes: vols.iter().map(volume_pb).collect(),
            ..Default::default()
        }
        .into())
    }

    async fn create_volume(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateVolumeRequest>,
    ) -> ServiceResult<pb::CreateVolumeResponse> {
        let target = format!("pool {} size {} name {:?}", req.pool, req.size, req.name);
        let who = self.authorize(&ctx, "CreateVolume", &target, true)?;
        let id = if req.id.is_empty() {
            None
        } else {
            Some(parse_volume_id(req.id)?)
        };
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::CreateVolume {
                pool: req.pool,
                size: req.size,
                name: req.name.to_string(),
                id,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "CreateVolume",
            &target,
            &match &r {
                Some(Ok(v)) => format!("ok, volume {:032x}", v.id),
                Some(Err((_, e))) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        let v = volume_result(r)?;
        Ok(pb::CreateVolumeResponse {
            volume: buffa::MessageField::some(volume_pb(&v)),
            ..Default::default()
        }
        .into())
    }

    async fn expand_volume(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ExpandVolumeRequest>,
    ) -> ServiceResult<pb::ExpandVolumeResponse> {
        let target = format!("volume {} size {}", req.id, req.size);
        let who = self.authorize(&ctx, "ExpandVolume", &target, true)?;
        let id = parse_volume_id(req.id)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::ExpandVolume {
                id,
                size: req.size,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "ExpandVolume",
            &target,
            &match &r {
                Some(Ok(v)) => format!("ok, {} bytes", v.size),
                Some(Err((_, e))) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        let v = volume_result(r)?;
        Ok(pb::ExpandVolumeResponse {
            volume: buffa::MessageField::some(volume_pb(&v)),
            ..Default::default()
        }
        .into())
    }

    async fn delete_volume(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteVolumeRequest>,
    ) -> ServiceResult<pb::DeleteVolumeResponse> {
        let target = if req.force {
            format!("volume {} (forced)", req.id)
        } else {
            format!("volume {}", req.id)
        };
        let who = self.authorize(&ctx, "DeleteVolume", &target, true)?;
        let id = parse_volume_id(req.id)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::DeleteVolume {
                id,
                force: req.force,
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        audit(
            &who,
            "DeleteVolume",
            &target,
            &match &r {
                Some(Ok(_)) => "ok, to the trash".into(),
                Some(Err((_, e))) => format!("failed: {e}"),
                None => "failed: map service stopped".into(),
            },
        );
        let v = volume_result(r)?;
        Ok(pb::DeleteVolumeResponse {
            purge_unix: self.purge_at(v.pool, v.trashed_unix),
            ..Default::default()
        }
        .into())
    }

    async fn list_snapshots(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::ListSnapshotsRequest>,
    ) -> ServiceResult<pb::ListSnapshotsResponse> {
        let id = parse_volume_id(req.id)?;
        let op = crate::SnapOp::List { id };
        match self
            .snap(
                &ctx,
                "ListSnapshots",
                format!("volume {id:032x}"),
                false,
                op,
            )
            .await?
        {
            crate::SnapDone::Snapshots(v) => Ok(pb::ListSnapshotsResponse {
                snapshots: v.iter().map(snapshot_pb).collect(),
                ..Default::default()
            }
            .into()),
            d => Err(unexpected_done(d)),
        }
    }

    async fn create_snapshot(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateSnapshotRequest>,
    ) -> ServiceResult<pb::CreateSnapshotResponse> {
        let id = parse_volume_id(req.id)?;
        let op = crate::SnapOp::Create {
            id,
            name: req.name.to_string(),
        };
        match self
            .snap(
                &ctx,
                "CreateSnapshot",
                format!("volume {id:032x}"),
                true,
                op,
            )
            .await?
        {
            crate::SnapDone::Snapshot(s) => Ok(pb::CreateSnapshotResponse {
                snapshot: Some(snapshot_pb(&s)).into(),
                ..Default::default()
            }
            .into()),
            d => Err(unexpected_done(d)),
        }
    }

    async fn delete_snapshot(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteSnapshotRequest>,
    ) -> ServiceResult<pb::DeleteSnapshotResponse> {
        let id = parse_volume_id(req.id)?;
        let epoch = req.epoch;
        let target = format!("volume {id:032x} snapshot {epoch}");
        let op = crate::SnapOp::Delete { id, epoch };
        let done = self.snap(&ctx, "DeleteSnapshot", target, true, op).await?;
        Ok(pb::DeleteSnapshotResponse {
            purge_unix: self.purge_of(&done),
            ..Default::default()
        }
        .into())
    }

    async fn clone_volume(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CloneVolumeRequest>,
    ) -> ServiceResult<pb::CloneVolumeResponse> {
        let id = parse_volume_id(req.id)?;
        let new_id = if req.new_id.is_empty() {
            None
        } else {
            Some(parse_volume_id(req.new_id)?)
        };
        let target = format!("volume {id:032x} snapshot {}", req.epoch);
        let op = crate::SnapOp::Clone {
            id,
            epoch: req.epoch,
            name: req.name.to_string(),
            new_id,
        };
        match self.snap(&ctx, "CloneVolume", target, true, op).await? {
            crate::SnapDone::Volume(v) => Ok(pb::CloneVolumeResponse {
                volume: Some(volume_pb(&v)).into(),
                ..Default::default()
            }
            .into()),
            d => Err(unexpected_done(d)),
        }
    }

    async fn rollback_volume(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::RollbackVolumeRequest>,
    ) -> ServiceResult<pb::RollbackVolumeResponse> {
        let id = parse_volume_id(req.id)?;
        let target = format!("volume {id:032x} to snapshot {}", req.epoch);
        let op = crate::SnapOp::Rollback {
            id,
            epoch: req.epoch,
        };
        match self.snap(&ctx, "RollbackVolume", target, true, op).await? {
            crate::SnapDone::Volume(v) => Ok(pb::RollbackVolumeResponse {
                volume: Some(volume_pb(&v)).into(),
                ..Default::default()
            }
            .into()),
            d => Err(unexpected_done(d)),
        }
    }

    async fn flatten_volume(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::FlattenVolumeRequest>,
    ) -> ServiceResult<pb::FlattenVolumeResponse> {
        let id = parse_volume_id(req.id)?;
        let target = format!("volume {id:032x}");
        let who = self.authorize(&ctx, "FlattenVolume", &target, true)?;
        // Checked now, run as an operation (D121): it copies every chunk.
        let v = self.volume_any(id).await?;
        if v.state == crate::VolumeState::Trashed {
            return Err(with_reason(
                ConnectError::not_found(format!("no volume {id:032x}")),
                "not_found",
            ));
        }
        let op = self
            .start_op(
                &who,
                "FlattenVolume",
                crate::operations::Work::Flatten { volume: id },
                target,
                format!("copying what {} reads through its parents", v.name),
            )
            .await?;
        Ok(pb::FlattenVolumeResponse {
            volume: Some(volume_pb(&v)).into(),
            operation: buffa::MessageField::some(op),
            ..Default::default()
        }
        .into())
    }

    async fn create_group_snapshot(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::CreateGroupSnapshotRequest>,
    ) -> ServiceResult<pb::CreateGroupSnapshotResponse> {
        let ids = req
            .ids
            .iter()
            .map(|i| parse_volume_id(i))
            .collect::<Result<Vec<u128>, _>>()?;
        if ids.is_empty() {
            return Err(with_reason(
                ConnectError::invalid_argument("a group needs at least one volume"),
                "invalid_argument",
            ));
        }
        let target = format!("{} volume(s)", ids.len());
        let op = crate::SnapOp::Group {
            ids,
            name: req.name.to_string(),
        };
        match self
            .snap(&ctx, "CreateGroupSnapshot", target, true, op)
            .await?
        {
            crate::SnapDone::Group(g, members) => Ok(pb::CreateGroupSnapshotResponse {
                group: format!("{g:032x}"),
                members: members
                    .iter()
                    .map(|(id, s)| pb::GroupMember {
                        id: format!("{id:032x}"),
                        snapshot: Some(snapshot_pb(s)).into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
            .into()),
            d => Err(unexpected_done(d)),
        }
    }

    async fn delete_group_snapshot(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, pb::DeleteGroupSnapshotRequest>,
    ) -> ServiceResult<pb::DeleteGroupSnapshotResponse> {
        let group = parse_volume_id(req.group)?;
        let op = crate::SnapOp::DeleteGroup { group };
        let done = self
            .snap(
                &ctx,
                "DeleteGroupSnapshot",
                format!("group {group:032x}"),
                true,
                op,
            )
            .await?;
        Ok(pb::DeleteGroupSnapshotResponse {
            purge_unix: self.purge_of(&done),
            ..Default::default()
        }
        .into())
    }
}

fn snapshot_pb(s: &crate::Snapshot) -> pb::Snapshot {
    pb::Snapshot {
        epoch: s.epoch,
        name: s.name.clone(),
        group: if s.group == 0 {
            String::new()
        } else {
            format!("{:032x}", s.group)
        },
        state: if s.deleting { "deleting" } else { "active" }.into(),
        ..Default::default()
    }
}

impl Api {
    /// A snapshot operation (10-volumes.md §10.3, §10.4): authorized, run on
    /// the map service's reactor as a client of `.meta`, and audited if
    /// it changes anything.
    /// Volume `id`'s header, whatever its state.
    async fn volume_any(&self, id: u128) -> Result<crate::Volume, ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Snapshots {
                op: crate::SnapOp::Get { id },
                reply,
            })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        match volume_result(rx.recv().await)? {
            crate::SnapDone::Volume(v) => Ok(v),
            d => Err(unexpected_done(d)),
        }
    }

    /// A long operation started for `who` (D120): on the leader only,
    /// audited as `rpc`'s start; its end is audited too.
    async fn start_op(
        &self,
        who: &str,
        rpc: &str,
        work: crate::operations::Work,
        target: String,
        message: String,
    ) -> Result<pb::Operation, ConnectError> {
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Leads(reply))
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        if let Some(Err(e)) = rx.recv().await {
            return Err(meta_error(e, ConnectError::unavailable, "not_leader"));
        }
        let r = self
            .admin
            .runner
            .create(work, target.clone(), 1, message, who.to_string())
            .await;
        audit(
            who,
            rpc,
            &target,
            &match &r {
                Ok(o) => format!("ok, operation {} started", o.id),
                Err(e) => format!("failed: {e:?}"),
            },
        );
        match r {
            Ok(o) => Ok(ops::op_pb(&o)),
            Err(e) => Err(self.store_error(e).await),
        }
    }

    async fn snap(
        &self,
        ctx: &RequestContext,
        rpc: &str,
        target: String,
        write: bool,
        op: crate::SnapOp,
    ) -> Result<crate::SnapDone, ConnectError> {
        let who = self.authorize(ctx, rpc, &target, write)?;
        let (reply, rx) = oneshot();
        self.tx
            .send(Admin::Snapshots { op, reply })
            .map_err(|_| ConnectError::unavailable("map service stopped"))?;
        let r = rx.recv().await;
        if write {
            audit(
                &who,
                rpc,
                &target,
                &match &r {
                    Some(Ok(_)) => "ok".into(),
                    Some(Err((_, e))) => format!("failed: {e}"),
                    None => "failed: map service stopped".into(),
                },
            );
        }
        volume_result(r)
    }
}

impl Api {
    /// When the last of what a delete trashed is purged.
    fn purge_of(&self, d: &crate::SnapDone) -> u64 {
        match d {
            crate::SnapDone::Trashed(v) => v
                .iter()
                .map(|(pool, _, s)| self.purge_at(*pool, s.trashed_unix))
                .max()
                .unwrap_or(0),
            _ => 0,
        }
    }
}

fn unexpected_done(d: crate::SnapDone) -> ConnectError {
    ConnectError::internal(format!("unexpected result {d:?}"))
}

fn parse_volume_id(s: &str) -> Result<u128, ConnectError> {
    if s.len() > 32 {
        return Err(ConnectError::invalid_argument(format!(
            "{s}: not a volume id"
        )));
    }
    u128::from_str_radix(s, 16)
        .map_err(|_| ConnectError::invalid_argument(format!("{s}: not a volume id")))
}

fn volume_pb(v: &crate::Volume) -> pb::Volume {
    pb::Volume {
        id: format!("{:032x}", v.id),
        pool: v.pool,
        size: v.size,
        name: v.name.clone(),
        state: match v.state {
            crate::VolumeState::Active => "active",
            crate::VolumeState::Deleting => "deleting",
            crate::VolumeState::Trashed => "trashed",
        }
        .into(),
        version: v.version,
        attached_by: v.holder.map(|h| format!("{h:016x}")).unwrap_or_default(),
        ..Default::default()
    }
}

fn volume_result<T>(r: Option<Result<T, (crate::VolumeFault, String)>>) -> Result<T, ConnectError> {
    use crate::VolumeFault as F;
    match r {
        Some(Ok(v)) => Ok(v),
        Some(Err((F::NotFound, e))) => Err(with_reason(ConnectError::not_found(e), "not_found")),
        Some(Err((F::Exists, e))) => Err(with_reason(
            ConnectError::already_exists(e),
            "already_exists",
        )),
        Some(Err((F::Invalid, e))) => Err(with_reason(
            ConnectError::invalid_argument(e),
            "invalid_argument",
        )),
        Some(Err((F::Attached, e))) => Err(with_reason(
            ConnectError::failed_precondition(e),
            "attached",
        )),
        Some(Err((F::InUse, e))) => {
            Err(with_reason(ConnectError::failed_precondition(e), "in_use"))
        }
        Some(Err((F::Unavailable, e))) => {
            Err(with_reason(ConnectError::unavailable(e), "unavailable"))
        }
        None => Err(ConnectError::unavailable("map service stopped")),
    }
}

/// TLS for the API: the server's chain and key, the root clients must
/// chain to, and the cluster id their certificates must name.
pub struct Tls {
    pub config: connectrpc::rustls::ServerConfig,
    pub cluster: String,
    /// The root's certificate, PEM: what joining hosts are given.
    pub root: String,
}

/// Reads `DIR/api.crt` (chain), `DIR/api.key` and `DIR/root.crt`. Clients
/// may connect without a certificate, for the join; every other call
/// needs one.
pub fn tls_from_dir(dir: &std::path::Path, cluster: u128) -> Result<Tls, String> {
    let read = |f: &str| {
        std::fs::read_to_string(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()))
    };
    let root = read("root.crt")?;
    Ok(Tls {
        config: hyperstor_pki::api_server_config(&read("api.crt")?, &read("api.key")?, &root)?,
        cluster: cluster.to_string(),
        root,
    })
}

/// Serves the cluster API on `listener` from a new thread with its own
/// single-threaded tokio runtime, over TLS when `tls` is given, and on
/// the local socket `unix` if given. `map_address` is reported to clients
/// as where the map service listens. With `signing`, this member signs
/// joins and renewals (§23.4.2). Long operations are kept in `store`
/// (D120), else in memory; the leader runs the trash's purger (D54) every
/// `trash_scan`.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    listener: std::net::TcpListener,
    unix: Option<std::os::unix::net::UnixListener>,
    tx: Sender<Admin>,
    map_address: SocketAddr,
    tls: Option<Tls>,
    signing: Option<Signing>,
    store: Option<Arc<crate::store::Db>>,
    trash_scan: std::time::Duration,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hyperstor-api".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                listener.set_nonblocking(true).expect("nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                let api_address = listener
                    .local_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                let (config, cluster, root) = match tls {
                    Some(t) => (Some(Arc::new(t.config)), Some(t.cluster), Some(t.root)),
                    None => (None, None, None),
                };
                let settings = store
                    .clone()
                    .map(|db| Arc::new(crate::settings::Settings::new(db)));
                let runner = ops::Runner::new(
                    tx.clone(),
                    Arc::new(match store {
                        Some(db) => crate::operations::Ops::replicated(db),
                        None => crate::operations::Ops::in_memory(),
                    }),
                    settings,
                );
                tokio::spawn(runner.clone().upkeep(trash_scan));
                let api = Arc::new(Api {
                    tx,
                    map_address: map_address.to_string(),
                    cluster,
                    buckets: Default::default(),
                    admin: admin::State::new(signing, root, api_address, runner),
                });
                let router = ClusterServiceExt::register(api.clone(), connectrpc::Router::new());
                let router = hyperstor_api::TrashServiceExt::register(api.clone(), router);
                let router = hyperstor_api::AuditServiceExt::register(api.clone(), router);
                let router = admin::register(api, router);
                let service = connectrpc::ConnectRpcService::new(router);
                if let Some(u) = unix {
                    u.set_nonblocking(true).expect("nonblocking");
                    let u = tokio::net::UnixListener::from_std(u).expect("unix listener");
                    tokio::spawn(serve_local(u, service.clone()));
                }
                let mut server = connectrpc::Server::from_listener(listener);
                if let Some(c) = config {
                    server = server.with_tls(c);
                }
                if let Err(e) = server.serve_with_service(service).await {
                    eprintln!("hyperstor-meta: cluster API: {e}");
                    std::process::exit(1);
                }
            })
        })
        .expect("spawn the API thread")
}

/// The local socket: each connection served over HTTP/1 or h2c, every
/// request tagged with the caller's credentials from the kernel.
async fn serve_local(listener: tokio::net::UnixListener, service: connectrpc::ConnectRpcService) {
    use tower::Service;
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let Ok(cred) = stream.peer_cred() else {
            continue;
        };
        let peer = LocalPeer {
            uid: cred.uid(),
            pid: cred.pid(),
        };
        let service = service.clone();
        tokio::spawn(async move {
            let svc =
                hyper::service::service_fn(move |mut req: http::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(peer);
                    let mut s = service.clone();
                    async move { s.call(req).await }
                });
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
        });
    }
}

#[cfg(test)]
mod tests {
    /// A byte per PG in PG order, sent again only when the map's epoch
    /// moves (D154), and in agreement with the counts.
    #[test]
    fn usage_levels_are_a_share_of_the_fullest() {
        // The pilot's spread (19 to 82 MiB) uses most of the range.
        let (l, max) = super::usage_levels(&[
            None,
            Some(0),
            Some(82 << 20),
            Some(41 << 20),
            Some(19 << 20),
        ]);
        assert_eq!(max, 82 << 20);
        assert_eq!(l, [0, 1, 255, 128, 60]);
        // Nothing written anywhere: every reported PG at the lowest level.
        assert_eq!(super::usage_levels(&[Some(0), None]).0, [1, 0]);
    }

    #[test]
    fn pg_grid_follows_the_epoch() {
        let mut m = crate::parse_cluster(
            "cluster 5\nnode 0 rack 0 127.0.0.1:9100\nnode 1 rack 0 127.0.0.1:9101\n\
             pool 1 vol replicated size 2 min 2 pgs 4 domain host\n",
        )
        .unwrap();
        let g = super::pg_grid(&m, 1, 0).unwrap();
        assert!(!g.unchanged);
        assert_eq!((g.epoch, g.states), (m.epoch, vec![1; 4]));
        let g = super::pg_grid(&m, 1, m.epoch).unwrap();
        assert!(g.unchanged && g.states.is_empty());
        let pg = m.pgs.get_mut(&(1, 2)).unwrap();
        pg.acting.truncate(1);
        m.epoch += 1;
        let g = super::pg_grid(&m, 1, m.epoch - 1).unwrap();
        assert_eq!(g.states, [1, 1, 4, 1]);
        let s = super::status(
            &m,
            None,
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
        );
        let pool = s.pools.iter().find(|p| p.id == 1).unwrap();
        assert_eq!((pool.pgs.clean, pool.pgs.inactive), (3, 1));
        assert!(super::pg_grid(&m, 7, 0).is_err());
    }

    /// Pools' usable space from their devices' reports (D156): divided by
    /// redundancy, only the devices placement may use, and none until
    /// each of those has reported.
    #[test]
    fn pool_space_from_device_reports() {
        let mut m = crate::parse_cluster(
            "cluster 5\nnode 0 rack 0 127.0.0.1:9100\nnode 1 rack 0 127.0.0.1:9101\n\
             node 2 rack 0 127.0.0.1:9102\n\
             pool 1 vol replicated size 2 min 1 pgs 4 domain host\n\
             pool 2 wide ec2+1 pgs 4 domain host\n\
             pool 3 slow replicated size 2 min 1 pgs 4 domain host class hdd\n",
        )
        .unwrap();
        let devices: Vec<u32> = m.devices.keys().copied().collect();
        assert_eq!(devices.len(), 3);
        let status = |m: &crate::ClusterMap, space: &crate::DeviceSpace| {
            let mut s = super::status(
                m,
                None,
                &Default::default(),
                &Default::default(),
                "127.0.0.1:8849",
            );
            super::pool_space(&mut s, m, space);
            s
        };
        let pool = |s: &super::pb::GetStatusResponse, id: u32| {
            let p = s.pools.iter().find(|p| p.id == id).unwrap();
            (p.total_bytes, p.free_bytes)
        };
        let mut space: crate::DeviceSpace = devices.iter().map(|d| (*d, (1200, 600))).collect();
        let s = status(&m, &space);
        assert_eq!(pool(&s, 1), (Some(1800), Some(900)));
        assert_eq!(pool(&s, 2), (Some(2400), Some(1200)));
        // No hdd devices: nothing to report.
        assert_eq!(pool(&s, 3), (None, None));
        // Each device with its own report, raw.
        let device = |s: &super::pb::GetStatusResponse, id: u32| {
            let d = s
                .nodes
                .iter()
                .flat_map(|n| &n.devices)
                .find(|d| d.id == id)
                .unwrap();
            (d.total_bytes, d.free_bytes)
        };
        assert_eq!(device(&s, devices[2]), (Some(1200), Some(600)));
        // A device not heard from leaves its pools without.
        space.remove(&devices[2]);
        let s = status(&m, &space);
        assert_eq!(pool(&s, 1), (None, None));
        assert_eq!(device(&s, devices[2]), (None, None));
        assert_eq!(device(&s, devices[0]), (Some(1200), Some(600)));
        // Out of placement, it no longer counts.
        m.devices.get_mut(&devices[2]).unwrap().up_in = false;
        assert_eq!(pool(&status(&m, &space), 1), (Some(1200), Some(600)));
    }

    /// D159: no failure domain counts for more than one shard of each PG,
    /// so the smallest of as many hosts as copies bounds the pool.
    #[test]
    fn pool_space_under_the_domain_rule() {
        use super::per_shard;
        assert_eq!(per_shard(vec![10, 10, 10], 3), 10);
        assert_eq!(per_shard(vec![10, 10, 1], 3), 1);
        assert_eq!(per_shard(vec![10, 10, 1], 2), 10);
        assert_eq!(per_shard(vec![30, 10, 10, 10], 3), 15);
        assert_eq!(per_shard(vec![12, 10, 10, 10], 3), 14);
        // Fewer domains than shards: one shard in each.
        assert_eq!(per_shard(vec![10, 4], 3), 4);
        assert_eq!(per_shard(vec![], 3), 0);
        // Two devices on host 0, one each on hosts 1 and 2; 3 copies by
        // host: host 0's two devices hold one copy, so hosts 1 and 2
        // bound the pool. By device, all four count.
        let m = crate::parse_cluster(
            "cluster 5\nnode 0 rack 0 127.0.0.1:9100 127.0.0.1:9103\n\
             node 1 rack 1 127.0.0.1:9101\nnode 2 rack 1 127.0.0.1:9102\n\
             pool 1 vol replicated size 3 min 2 pgs 8 domain host\n\
             pool 2 any replicated size 3 min 2 pgs 8 domain device\n\
             pool 3 wide ec2+1 pgs 8 domain host\n\
             pool 4 racks replicated size 2 min 1 pgs 8 domain rack\n",
        )
        .unwrap();
        let space: crate::DeviceSpace = m.devices.keys().map(|d| (*d, (1000, 400))).collect();
        let mut s = super::status(
            &m,
            None,
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
        );
        super::pool_space(&mut s, &m, &space);
        let pool = |id: u32| {
            let p = s.pools.iter().find(|p| p.id == id).unwrap();
            (p.total_bytes, p.free_bytes)
        };
        assert_eq!(pool(1), (Some(1000), Some(400)));
        assert_eq!(pool(2), (Some(1333), Some(533)));
        assert_eq!(pool(3), (Some(2000), Some(800)));
        // Rack 0 holds 2000, rack 1 2000: two copies, 2000.
        assert_eq!(pool(4), (Some(2000), Some(800)));
    }

    #[test]
    fn status_carries_health_and_pools() {
        let m = crate::parse_cluster(
            "cluster 5\nnode 0 rack 0 127.0.0.1:9100\nnode 1 rack 0 127.0.0.1:9101\n\
             pool 1 vol replicated size 2 min 1 pgs 4 domain host\n",
        )
        .unwrap();
        let s = super::status(
            &m,
            None,
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
        );
        assert_eq!(s.cluster_id, "5");
        assert_eq!(s.health, super::pb::Health::HEALTH_OK);
        assert_eq!(s.nodes.len(), 2);
        let pool = s.pools.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(pool.pgs.clean, 4);
        assert_eq!(pool.kind, "replicated");
        assert_eq!(s.map_address, "127.0.0.1:8849");
        // Node 1 not heard from since start: warn, though the map says up.
        let heard = [(0, std::time::Duration::from_millis(40))].into();
        let s = super::status(
            &m,
            Some(&heard),
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
        );
        assert_eq!(s.health, super::pb::Health::HEALTH_WARN);
        assert!(
            s.reasons[0]
                .text
                .contains("not heard from since the map service started: 1")
        );
        assert_eq!(s.nodes[0].last_heard_ms, Some(40));
        assert_eq!(s.nodes[1].last_heard_ms, None);
        // A follower hears no heartbeats: no warning for that, a note
        // naming the leader, and no time heard.
        let empty = Default::default();
        let s = super::status_for(
            &m,
            &empty,
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
            Err("this map service member isn't the leader; the leader is 127.0.0.1:9849".into()),
        );
        assert_eq!(s.health, super::pb::Health::HEALTH_OK);
        assert_eq!(s.reasons.len(), 1);
        assert!(s.reasons[0].text.contains("the leader is 127.0.0.1:9849"));
        assert_eq!(s.nodes[0].last_heard_ms, None);
        // One that knows no leader warns: nothing can change the map.
        let s = super::status_for(
            &m,
            &empty,
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
            Err(crate::NO_LEADER.into()),
        );
        assert_eq!(s.health, super::pb::Health::HEALTH_WARN);
        assert!(s.reasons[0].text.contains("until one is elected"));
        // A leader that can't commit says so first, as an error.
        let mut st = super::status_for(
            &m,
            &heard,
            &Default::default(),
            &Default::default(),
            "x",
            Ok(()),
        );
        super::stalled(&mut st, std::time::Duration::from_secs(90));
        assert_eq!(st.health, super::pb::Health::HEALTH_ERROR);
        assert!(st.reasons[0].text.contains("for 90 s"));
        // The leader warns as before.
        let s = super::status_for(
            &m,
            &heard,
            &Default::default(),
            &Default::default(),
            "127.0.0.1:8849",
            Ok(()),
        );
        assert_eq!(s.health, super::pb::Health::HEALTH_WARN);
        // Scrub left objects unresolved: warn, naming the PGs.
        let inc = [((1, 2), 3)].into();
        let s = super::status(&m, None, &inc, &Default::default(), "127.0.0.1:8849");
        assert_eq!(s.health, super::pb::Health::HEALTH_WARN);
        assert!(
            s.reasons[0]
                .text
                .contains("1 PG(s) inconsistent, 3 object(s)")
        );
        assert!(s.reasons[0].text.contains("1.2"));
    }

    #[test]
    fn each_caller_gets_its_own_burst_then_the_rate() {
        let b = super::Buckets::default();
        let t = std::time::Instant::now();
        assert!((0..1000).all(|_| super::take(&b, "a", t)));
        assert!(!super::take(&b, "a", t), "burst used up");
        assert!(super::take(&b, "b", t), "another caller isn't affected");
        // 200 calls per second come back: 10 ms gives two.
        let later = t + std::time::Duration::from_millis(10);
        assert!(super::take(&b, "a", later));
        assert!(super::take(&b, "a", later));
        assert!(!super::take(&b, "a", later));
    }

    #[test]
    fn local_callers_allowed() {
        use super::local_allowed;
        assert!(local_allowed(0, 999, &[], None), "root");
        assert!(local_allowed(999, 999, &[], None), "the service's user");
        assert!(
            local_allowed(1000, 999, &[27, 1001], Some(1001)),
            "hyperstor-admin"
        );
        assert!(!local_allowed(1000, 999, &[27], Some(1001)));
        assert!(!local_allowed(1000, 999, &[27], None));
    }

    #[test]
    fn audit_lines_are_json_with_escapes() {
        let l = super::audit_line("hyperstor://7/admin/a\"b", "SetDeviceIn", "device 2", "ok");
        assert!(l.starts_with("{\"audit\":1,\"time\":"), "{l}");
        assert!(
            l.contains("\"principal\":\"hyperstor://7/admin/a\\\"b\""),
            "{l}"
        );
        assert!(
            l.ends_with("\"op\":\"SetDeviceIn\",\"target\":\"device 2\",\"result\":\"ok\"}"),
            "{l}"
        );
    }
}
