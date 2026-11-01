//! The map service (spec/06-metadata.md §4.1), and the cluster map, which
//! lives in `hyperstor-map` and is re-exported here.
//!
//! [`serve`] answers the map service's requests. A map service is one
//! member, or a group of 3 or 5 that replicate the published map with Raft
//! ([`replica`], [`store`]; 04-decisions.md D2, D20, D71–D74): one leader
//! keeps liveness, leases and fencing and publishes; every member serves
//! the map. Each device records the address its reactor listens on.

pub mod api;
pub mod audit_log;
pub mod backup;
pub mod certs;
pub mod health;
pub mod mgmt_store;
pub mod operations;
pub mod replica;
pub mod settings;
pub mod store;
pub mod tokens;

use hyperstor_reactor::time::sleep;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub use hyperstor_map::*;

/// A request from the cluster API's thread to the map service's reactor
/// (`Meta::admin`). Replies go back on one-shot channels that work across
/// threads and runtimes.
/// A pool to make (`CreatePool`, 24-pools-and-devices.md §27.3.1).
#[derive(Debug, Clone)]
pub struct NewPool {
    pub name: String,
    pub kind: PoolKind,
    pub size: u8,
    pub min_size: u8,
    /// None: [`default_pg_count`].
    pub pg_count: Option<u32>,
    pub chunk_size: u32,
    pub domain: hyperstor_placement::Domain,
    pub classes: Vec<String>,
    /// Allow classes of hard disks and flash both (D172).
    pub force: bool,
}

/// What `SetPool` changes (24-pools-and-devices.md §27.3.2); `None`
/// leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub struct PoolChange {
    /// The class set; empty: every device (D164).
    pub classes: Option<Vec<String>>,
    pub name: Option<String>,
    /// PGs move, as for the classes.
    pub domain: Option<hyperstor_placement::Domain>,
    /// Twice the PG count: the pool splits (24-pools-and-devices.md
    /// §27.5, D183).
    pub pg_count: Option<u32>,
}

pub enum Admin {
    /// The published map, what clients and storage nodes see, how long
    /// ago each node was last heard from (absent: not since start), the
    /// PGs scrub couldn't repair, and the nodes flapping (D149).
    Published(
        hyperstor_reactor::chan::OneSender<(
            ClusterMap,
            BTreeMap<u32, Duration>,
            Inconsistent,
            Flapping,
        )>,
    ),
    /// What only the leader knows (I197): this member's own if it leads,
    /// else the leader's, asked over the map protocol and kept a second;
    /// `None` when there is no leader or it doesn't answer.
    LeaderView(hyperstor_reactor::chan::OneSender<Option<LeaderView>>),
    SetDeviceIn {
        device: u32,
        up_in: bool,
        reply: hyperstor_reactor::chan::OneSender<Result<u64, String>>,
    },
    /// Sets a device's weight, or (`None`) lets it follow the device's
    /// capacity again (D158). Replies with the working map's epoch and the
    /// weight now.
    SetDeviceWeight {
        device: u32,
        weight: Option<f64>,
        reply: hyperstor_reactor::chan::OneSender<Result<(u64, f64), String>>,
    },
    /// Volume operations (04-decisions.md D77), run on this reactor as a
    /// client of `.meta` (06-metadata.md §4.2).
    /// A pool's volumes; with `trash`, those in the trash too (D54).
    ListVolumes {
        pool: u32,
        trash: bool,
        reply: VolumeReply<Vec<Volume>>,
    },
    CreateVolume {
        pool: u32,
        size: u64,
        name: String,
        id: Option<u128>,
        reply: VolumeReply<Volume>,
    },
    ExpandVolume {
        id: u128,
        size: u64,
        reply: VolumeReply<Volume>,
    },
    /// Moves a volume to the trash (D54): replies with it as trashed.
    DeleteVolume {
        id: u128,
        /// Even if attached: preempts the holder (D80).
        force: bool,
        reply: VolumeReply<Volume>,
    },
    /// Reads the audit log in `.meta` (D112): the lines, and where to go on.
    AuditRead {
        query: audit_log::Query,
        reply: VolumeReply<(Vec<String>, Option<String>)>,
    },
    /// The trash: what's in it, restore, purge (19-administration.md §23.9.3).
    Trash {
        op: TrashOp,
        reply: VolumeReply<TrashDone>,
    },
    /// Snapshots, clones, rollback, flatten and groups (10-volumes.md §10.3,
    /// §10.4), as a client of `.meta`.
    Snapshots {
        op: SnapOp,
        reply: VolumeReply<SnapDone>,
    },
    /// Adds a device to a pool, or removes it (D180). Replies with the
    /// working map's epoch and whether the pool's devices mix kinds.
    SetPoolDevice {
        pool: u32,
        device: u32,
        member: bool,
        force: bool,
        reply: hyperstor_reactor::chan::OneSender<Result<(u64, bool), String>>,
    },
    /// Takes a device out to remove it (24-pools-and-devices.md §27.2.5,
    /// D175), or with `dry_run` only works out what that would do.
    RemoveDevice {
        device: u32,
        force: bool,
        dry_run: bool,
        reply: hyperstor_reactor::chan::OneSender<Result<Removal, String>>,
    },
    /// Takes a device being removed out of the acting set of each PG that
    /// has nothing left to backfill but still lists it
    /// ([`ClusterMap::release`]); nothing for a device that is in. Replies
    /// with how many PGs changed.
    Release {
        device: u32,
        reply: hyperstor_reactor::chan::OneSender<usize>,
    },
    /// Removes a device that no PG lists from the map, its node with it if
    /// it was the last (D182). Replies with whether the node went, and the
    /// working map's epoch; `None` if the map hasn't the device.
    FinishRemoval {
        device: u32,
        reply: hyperstor_reactor::chan::OneSender<Result<Option<(bool, u64)>, String>>,
    },
    /// Deletes a pool from the map (D176, D181), once the caller has seen
    /// it hold nothing. Replies with the working map's epoch.
    DeletePool {
        pool: u32,
        reply: hyperstor_reactor::chan::OneSender<Result<u64, String>>,
    },
    /// Makes a pool (D163, D171). Replies with its id, PG count, the
    /// working map's epoch and whether its classes mix kinds (forced).
    CreatePool {
        new: NewPool,
        reply: hyperstor_reactor::chan::OneSender<Result<(u32, u32, u64, bool), String>>,
    },
    /// Changes a pool: its class set (D164), name or failure domain.
    /// Replies with the working map's epoch and whether its devices mix
    /// hard disks and flash (forced).
    SetPool {
        pool: u32,
        change: PoolChange,
        force: bool,
        reply: hyperstor_reactor::chan::OneSender<Result<(u64, bool), String>>,
    },
    /// Sets a device's class, or lets it follow its kind (D178). Replies
    /// with the working map's epoch and the class now.
    SetDeviceClass {
        device: u32,
        class: Option<String>,
        reply: hyperstor_reactor::chan::OneSender<Result<(u64, String), String>>,
    },
    /// Appends a device to a node (D167). Replies with the device's id
    /// and the working map's epoch.
    AddDevice {
        node: u32,
        addr: SocketAddr,
        class: Option<String>,
        reply: hyperstor_reactor::chan::OneSender<Result<(u32, u64), String>>,
    },
    /// Adds a storage host that joined with a token (19-administration.md
    /// §23.4.2): one device per address. Replies with its node number.
    AddNode {
        rack: u32,
        addrs: Vec<SocketAddr>,
        reply: hyperstor_reactor::chan::OneSender<Result<u32, String>>,
    },
    /// Whether this member leads: `Ok`, or why not (with the leader's
    /// map service address when known).
    Leads(hyperstor_reactor::chan::OneSender<Result<(), String>>),
    /// The bytes on each of the pool's PGs' primaries (D155), unless the
    /// reports are still at version `since` (`None`); only the leader has
    /// them.
    PgUsage {
        pool: u32,
        since: u64,
        reply: hyperstor_reactor::chan::OneSender<Result<PoolUsage, String>>,
    },
    /// How long the leader has failed to commit map changes, if it has.
    Uncommitted(hyperstor_reactor::chan::OneSender<Option<Duration>>),
    /// This member's role, and the client instance its volume work uses
    /// now (D125).
    Lead(hyperstor_reactor::chan::OneSender<Lead>),
    /// Blocklists client instances or lease identities (D150): the map
    /// epoch that has them, once every storage node holds it or its lease
    /// has run out ([`Meta::blocklist`]).
    Blocklist {
        ids: Vec<u64>,
        reply: hyperstor_reactor::chan::OneSender<Result<u64, String>>,
    },
    /// Takes every device of `node` out (or, with `dry_run`, only works
    /// out what that would do). Replies with each PG that moves: pool, PG,
    /// acting before, up after.
    Drain {
        node: u32,
        dry_run: bool,
        reply: hyperstor_reactor::chan::OneSender<Result<Vec<PgMove>, String>>,
    },
}

/// What removing a device does, or would (24-pools-and-devices.md
/// §27.2.5): the PGs that move, the pools left with fewer failure domains
/// than copies (pool, domains, copies), and the devices that take data,
/// with their bytes used after and their size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removal {
    pub moves: Vec<PgMove>,
    pub short: Vec<(u32, u32, u8)>,
    pub space: Vec<(u32, u64, u64)>,
}

/// Each marked PG's split reports: the PG epoch they name, and who sent
/// them (D183).
type SplitReports = HashMap<(u32, u32), (u64, BTreeSet<u32>)>;

/// PGs of one pool split at a time (D183): each pauses its I/O briefly.
pub const SPLITS_AT_ONCE: usize = 4;

/// A device may be filled to this share of its size by a removal: a pool's
/// `full` threshold (D27).
pub const FULL: f64 = 0.95;

/// A PG a drain moves: its acting set before, where it goes after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgMove {
    pub pool: u32,
    pub pg: u32,
    pub before: Vec<u32>,
    pub after: Vec<u32>,
}

pub use hyperstor_client::volume::{Snapshot, Volume, VolumeState};

/// A snapshot operation of the cluster API ([`Admin::Snapshots`]).
#[derive(Debug, Clone)]
pub enum SnapOp {
    List {
        id: u128,
    },
    /// The volume's header, whatever its state (a long operation's check).
    Get {
        id: u128,
    },
    Create {
        id: u128,
        name: String,
    },
    Delete {
        id: u128,
        epoch: u64,
    },
    Clone {
        id: u128,
        epoch: u64,
        name: String,
        new_id: Option<u128>,
    },
    Rollback {
        id: u128,
        epoch: u64,
    },
    Flatten {
        id: u128,
    },
    Group {
        ids: Vec<u128>,
        name: String,
    },
    DeleteGroup {
        group: u128,
    },
}

/// A trash operation of the cluster API ([`Admin::Trash`]); `epoch`
/// names a snapshot, none the volume itself.
#[derive(Debug, Clone)]
pub enum TrashOp {
    /// What's in the trash, in `pool` or every data pool.
    List {
        pool: Option<u32>,
    },
    Restore {
        id: u128,
        epoch: Option<u64>,
        name: Option<String>,
    },
    Purge {
        id: u128,
        epoch: Option<u64>,
    },
}

/// What a [`TrashOp`] gives back.
#[derive(Debug, Clone)]
pub enum TrashDone {
    Items(Vec<hyperstor_client::trash::Trashed>),
    Volume(Volume),
    Snapshot(Snapshot),
    Done,
}

async fn run_trash_op(
    c: std::rc::Rc<hyperstor_client::Client>,
    op: TrashOp,
) -> Result<TrashDone, hyperstor_client::ClientError> {
    Ok(match op {
        TrashOp::List { pool } => {
            let pools: Vec<u32> = match pool {
                Some(p) => vec![p],
                None => c
                    .map()
                    .pools
                    .keys()
                    .copied()
                    .filter(|p| *p != META_POOL)
                    .collect(),
            };
            let mut items = Vec::new();
            for p in pools {
                items.extend(hyperstor_client::trash::trash_of(&c.list_volumes(p).await?));
            }
            TrashDone::Items(items)
        }
        TrashOp::Restore {
            id,
            epoch: None,
            name,
        } => TrashDone::Volume(c.restore_volume(id, name.as_deref()).await?),
        TrashOp::Restore {
            id, epoch: Some(e), ..
        } => TrashDone::Snapshot(c.restore_snapshot(id, e).await?),
        TrashOp::Purge { id, epoch: None } => {
            c.purge_volume(id).await?;
            TrashDone::Done
        }
        TrashOp::Purge { id, epoch: Some(e) } => {
            c.purge_snapshot(id, e).await?;
            TrashDone::Done
        }
    })
}

/// What a [`SnapOp`] gives back.
#[derive(Debug, Clone)]
pub enum SnapDone {
    Snapshots(Vec<Snapshot>),
    /// Moved to the trash: each (pool, volume, snapshot).
    Trashed(Vec<(u32, u128, Snapshot)>),
    Snapshot(Snapshot),
    Volume(Volume),
    Group(u128, Vec<(u128, Snapshot)>),
    Done,
}

async fn run_snap_op(
    c: std::rc::Rc<hyperstor_client::Client>,
    op: SnapOp,
) -> Result<SnapDone, hyperstor_client::ClientError> {
    use hyperstor_client::ClientError;
    Ok(match op {
        // What's in the trash isn't listed (D54).
        SnapOp::List { id } => SnapDone::Snapshots(
            c.volume(id)
                .await?
                .filter(|v| v.state != VolumeState::Trashed)
                .ok_or(ClientError::NoVolume(id))?
                .snapshots
                .into_iter()
                .filter(|s| s.trashed_unix == 0)
                .collect(),
        ),
        SnapOp::Create { id, name } => SnapDone::Snapshot(c.snapshot(id, &name).await?),
        // To the trash (D54); purged later.
        SnapOp::Delete { id, epoch } => {
            let s = c.trash_snapshot(id, epoch).await?;
            let pool = c.volume(id).await?.map_or(0, |v| v.pool);
            SnapDone::Trashed(vec![(pool, id, s)])
        }
        SnapOp::Clone {
            id,
            epoch,
            name,
            new_id,
        } => SnapDone::Volume(c.clone_snapshot(id, epoch, &name, new_id).await?),
        SnapOp::Rollback { id, epoch } => SnapDone::Volume(c.rollback(id, epoch).await?),
        SnapOp::Get { id } => {
            SnapDone::Volume(c.volume(id).await?.ok_or(ClientError::NoVolume(id))?)
        }
        SnapOp::Flatten { id } => SnapDone::Volume(c.flatten(id).await?),
        SnapOp::Group { ids, name } => {
            let (g, s) = c.snapshot_group(&ids, &name).await?;
            SnapDone::Group(g, s)
        }
        SnapOp::DeleteGroup { group } => {
            let mut out = Vec::new();
            for (id, s) in c.trash_group(group).await? {
                let pool = c.volume(id).await?.map_or(0, |v| v.pool);
                out.push((pool, id, s));
            }
            SnapDone::Trashed(out)
        }
    })
}

/// Why a volume operation failed, for the API's error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeFault {
    NotFound,
    Exists,
    Invalid,
    /// Attached by a client: detach it first.
    Attached,
    /// A clone or rollback depends on the snapshot; or the operation is
    /// refused as things stand.
    InUse,
    Unavailable,
}

pub type VolumeReply<T> = hyperstor_reactor::chan::OneSender<Result<T, (VolumeFault, String)>>;

fn volume_fault(e: hyperstor_client::ClientError) -> (VolumeFault, String) {
    use hyperstor_client::ClientError as E;
    let kind = match &e {
        E::NoVolume(_) | E::NoPool(_) => VolumeFault::NotFound,
        E::VolumeExists(_) => VolumeFault::Exists,
        E::VolumeAttached(_) => VolumeFault::Attached,
        E::NoSnapshot(_) => VolumeFault::NotFound,
        E::SnapshotInUse(_) | E::Refused(_) => VolumeFault::InUse,
        E::Shrink { .. } | E::BadVolume(_) | E::Unaligned => VolumeFault::Invalid,
        _ => VolumeFault::Unavailable,
    };
    (kind, e.to_string())
}

/// The map service's state (06-metadata.md §4.1, 04-decisions.md D42).
///
/// - `map` is the working map: liveness and backfill change it.
/// - `published` is what nodes and clients see. A change is published only
///   after every device that loses a role in it has been fenced: it has
///   acknowledged a `Fence` (and answered every write it had accepted for
///   those PGs), or its last granted lease has run out. Then the map is
///   pushed to every live device.
/// - Heartbeats renew a node's map lease; a node may serve PG I/O only
///   while its lease is valid. A node that doesn't answer a fence gets no
///   new lease until the fence resolves.
pub struct Meta {
    pub map: RefCell<ClusterMap>,
    pub published: RefCell<ClusterMap>,
    role: Cell<Role>,
    /// Bumped each time this member gains or loses the lead: its volume
    /// work (operations among it) then gets a new client, with a new
    /// instance, so the next leader can blocklist what a former leader
    /// still runs (D125).
    lead_gen: Cell<u64>,
    /// Since when the leader's commits have failed or timed out, until one
    /// succeeds: with no majority of members answering, nothing changes.
    uncommitted: Cell<Option<Instant>>,
    /// Where the leader commits what it publishes (none: a map service
    /// of one, without a store).
    commits: Option<tokio::sync::mpsc::UnboundedSender<replica::Commit>>,
    /// The cluster file's map: merged into the stored map when leading.
    seed: Option<ClusterMap>,
    /// Whether the store holds a map yet.
    persisted: Cell<bool>,
    beats: RefCell<HashMap<u32, Instant>>,
    space: RefCell<DeviceSpace>,
    /// Kinds devices reported (D178), kept here until every device of a
    /// node that is up has reported one: a map with kinds is format 5,
    /// which nodes from before can't read.
    kind_reports: RefCell<BTreeMap<u32, hyperstor_map::DeviceKind>>,
    /// Members' reports of marked PGs split (D183): the PG epoch they name
    /// and who reported.
    split_reports: RefCell<SplitReports>,
    /// A follower's last [`LeaderView`] from the leader, and when.
    leader_view: RefCell<Option<(Instant, LeaderView)>>,
    /// When each node was last granted a non-zero lease.
    granted: RefCell<HashMap<u32, Instant>>,
    /// Nodes that didn't answer a fence: no leases until it resolves.
    fencing: RefCell<std::collections::HashSet<u32>>,
    conns: RefCell<HashMap<SocketAddr, hyperstor_rpc::Conn>>,
    /// Peer reports (D40): peer -> reporter -> when last reported.
    reports: RefCell<HashMap<u32, HashMap<u32, Instant>>>,
    /// Nodes marked down on peer reports: no leases while reported.
    suspect: RefCell<std::collections::HashSet<u32>>,
    lease: Duration,
    /// When this member started serving, or became the leader: nodes not
    /// heard from since get this much grace.
    started: Cell<Instant>,
    /// The newest map epoch each device acknowledged a push of.
    acked: RefCell<HashMap<u32, u64>>,
    /// How long a node stays down before its devices are marked out
    /// (07-placement.md §5.3), and since when each down node has been.
    down_out: Cell<Duration>,
    down_since: RefCell<HashMap<u32, Instant>>,
    /// Flap damping (D149): each node's recent downs and hold.
    damping: RefCell<HashMap<u32, Damping>>,
    /// PGs whose last scrub left objects it couldn't repair, with how many
    /// (their primaries' `ScrubReport`s, kept by the leader).
    pub inconsistent: RefCell<Inconsistent>,
    /// The bytes on each PG's primary, with the device that reported
    /// them (D155): the leader's, in memory only.
    usage: RefCell<BTreeMap<(u32, u32), (u32, u64)>>,
    /// Bumped with every change to `usage`, from the wall clock's
    /// nanoseconds, so a new leader's versions don't repeat an old one's.
    usage_version: Cell<u64>,
}

/// PGs whose last scrub left objects unresolved, with how many.
pub type Inconsistent = BTreeMap<(u32, u32), u32>;

/// A pool's usage as [`Admin::PgUsage`] answers it: the reports' version
/// and, unless unchanged, the bytes of PG n at n (`None`: not reported).
pub type PoolUsage = (u64, Option<Vec<Option<u64>>>);

fn wall_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64)
}

/// Each device's data region and free bytes, as it last reported them
/// (D156): kept by the leader only, like heartbeats, and not in the map.
pub type DeviceSpace = BTreeMap<u32, (u64, u64)>;

/// Bytes in a TiB: a device's weight is its capacity in TiB (D158).
const TIB: f64 = (1u64 << 40) as f64;

/// What only the leader knows, for `GetStatus` (I197): how long ago each
/// node was last heard from, scrub's unrepaired PGs, flapping nodes and
/// each device's space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeaderView {
    pub heard: BTreeMap<u32, Duration>,
    pub inconsistent: Inconsistent,
    pub flapping: Flapping,
    pub space: DeviceSpace,
}

/// How long a follower keeps the leader's view: `GetStatus` is polled
/// every few seconds by each UI tab.
const LEADER_VIEW_FOR: Duration = Duration::from_secs(1);

/// How long the leader waits for the group to commit a map.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default down-out interval (07-placement.md §5.3).
pub const DOWN_OUT: Duration = Duration::from_secs(600);

/// What a member that doesn't lead says while it knows no leader: nothing
/// can change the map until one is elected.
pub const NO_LEADER: &str = "this map service member isn't the leader, and no leader is known";

/// A member's role as [`Admin::Lead`] reports it (D125).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lead {
    /// The only member, without Raft: no other leader to fence.
    Standalone,
    /// Leads since `since`, its volume work as client `instance`.
    Leader {
        since: Instant,
        instance: u64,
    },
    Follower,
}

/// What a map service member does (04-decisions.md D72, D73).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The only member, without Raft: everything, at once.
    Standalone,
    /// Leads the group: grants leases and serves the map at once, changes
    /// roles from 1.25 leases after `since` (D73).
    Leader { since: Instant },
    /// Serves the map; redirects the rest to the leader, if known.
    Follower { leader: Option<SocketAddr> },
}

/// Default map lease (04-decisions.md D48).
pub const LEASE: Duration = Duration::from_millis(600);

/// Flap damping (04-decisions.md D149): a node marked down stays down at
/// least this long, doubled for each further down within
/// [`FLAP_WINDOW`], up to [`FLAP_HOLD_MAX`].
pub const FLAP_HOLD_MIN: Duration = Duration::from_secs(2);
pub const FLAP_HOLD_MAX: Duration = Duration::from_secs(60);
/// How long a node's down transitions count toward its damping and its
/// flapping warning; after this long without one, it starts afresh.
pub const FLAP_WINDOW: Duration = Duration::from_secs(600);

/// Nodes that went down at least twice within [`FLAP_WINDOW`], with how
/// many times (health warns about them, D149).
pub type Flapping = BTreeMap<u32, u32>;

/// How long a node is held down after its `downs`th down within the
/// window: [`FLAP_HOLD_MIN`] doubled for each before, at most
/// [`FLAP_HOLD_MAX`].
pub fn flap_hold(downs: usize) -> Duration {
    let doublings = downs.saturating_sub(1).min(16) as u32;
    (FLAP_HOLD_MIN * 2u32.pow(doublings)).min(FLAP_HOLD_MAX)
}

/// A node's recent down transitions and how long it is held down.
#[derive(Default)]
struct Damping {
    downs: std::collections::VecDeque<Instant>,
    hold_until: Option<Instant>,
}

impl Meta {
    pub fn new(map: ClusterMap) -> Rc<Self> {
        Self::with_lease(map, LEASE)
    }

    pub fn with_lease(map: ClusterMap, lease: Duration) -> Rc<Self> {
        Self::build(map, lease, Role::Standalone, None)
    }

    /// A member of a replicated map service: it serves `seed` (the
    /// cluster file's map) until the store's map is applied, follows until
    /// Raft says it leads, and commits what it publishes through
    /// `commits`. Feed it [`replica::ToReactor`] with [`Meta::follow`].
    pub fn replicated(
        seed: ClusterMap,
        lease: Duration,
        commits: tokio::sync::mpsc::UnboundedSender<replica::Commit>,
    ) -> Rc<Self> {
        let m = Self::build(
            seed.clone(),
            lease,
            Role::Follower { leader: None },
            Some(commits),
        );
        // Not `Rc::get_mut`: the fields are set once, here.
        let mut m = Rc::try_unwrap(m).ok().expect("just built");
        m.seed = Some(seed);
        m.persisted.set(false);
        Rc::new(m)
    }

    fn build(
        map: ClusterMap,
        lease: Duration,
        role: Role,
        commits: Option<tokio::sync::mpsc::UnboundedSender<replica::Commit>>,
    ) -> Rc<Self> {
        Rc::new(Meta {
            published: RefCell::new(map.clone()),
            map: RefCell::new(map),
            role: Cell::new(role),
            lead_gen: Cell::new(0),
            uncommitted: Cell::new(None),
            commits,
            seed: None,
            persisted: Cell::new(true),
            beats: RefCell::new(HashMap::new()),
            space: RefCell::new(BTreeMap::new()),
            kind_reports: RefCell::new(BTreeMap::new()),
            split_reports: RefCell::new(HashMap::new()),
            leader_view: RefCell::new(None),
            granted: RefCell::new(HashMap::new()),
            fencing: RefCell::new(std::collections::HashSet::new()),
            conns: RefCell::new(HashMap::new()),
            reports: RefCell::new(HashMap::new()),
            suspect: RefCell::new(std::collections::HashSet::new()),
            lease,
            started: Cell::new(Instant::now()),
            acked: RefCell::new(HashMap::new()),
            down_out: Cell::new(DOWN_OUT),
            down_since: RefCell::new(HashMap::new()),
            damping: RefCell::new(HashMap::new()),
            inconsistent: RefCell::new(BTreeMap::new()),
            usage: RefCell::new(BTreeMap::new()),
            usage_version: Cell::new(wall_nanos()),
        })
    }

    fn bump_usage(&self) {
        self.usage_version
            .set(wall_nanos().max(self.usage_version.get() + 1));
    }

    /// Takes a primary's usage report (D155) and returns how many PGs it
    /// now has from `device`. A full report drops the device's PGs it
    /// leaves out: the device is no longer their primary.
    pub fn note_usage(&self, device: u32, full: bool, pgs: &[(u32, u32, u64)]) -> u32 {
        let mut u = self.usage.borrow_mut();
        let n = u.len();
        if full {
            let named: std::collections::HashSet<_> = pgs.iter().map(|&(p, g, _)| (p, g)).collect();
            u.retain(|k, (d, _)| *d != device || named.contains(k));
        }
        let mut changed = u.len() != n;
        for &(pool, pg, bytes) in pgs {
            changed |= u.insert((pool, pg), (device, bytes)) != Some((device, bytes));
        }
        let known = u.values().filter(|(d, _)| *d == device).count() as u32;
        drop(u);
        if changed {
            self.bump_usage();
        }
        known
    }

    /// [`Admin::PgUsage`]'s answer.
    pub fn pool_usage(&self, pool: u32, since: u64) -> Result<PoolUsage, String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        let version = self.usage_version.get();
        if since == version {
            return Ok((version, None));
        }
        let Some(n) = self.published.borrow().pools.get(&pool).map(|p| p.pg_count) else {
            return Err(format!("no pool {pool}"));
        };
        let u = self.usage.borrow();
        let bytes = (0..n)
            .map(|pg| u.get(&(pool, pg)).map(|&(_, b)| b))
            .collect();
        Ok((version, Some(bytes)))
    }

    /// Sets how long a node stays down before its devices are marked out.
    pub fn set_down_out(&self, d: Duration) {
        self.down_out.set(d);
    }

    pub fn role(&self) -> Role {
        self.role.get()
    }

    /// Whether this member may change roles now: mark nodes down, move
    /// PGs, publish. A new leader first waits until every lease the old
    /// one granted has run out (D73).
    pub fn may_change_roles(&self) -> bool {
        match self.role.get() {
            Role::Standalone => true,
            Role::Leader { since } => since.elapsed() >= self.lease + self.lease / 4,
            Role::Follower { .. } => false,
        }
    }

    /// The leader's map-service address for a redirect: empty if unknown.
    fn leader_hint(&self) -> String {
        match self.role.get() {
            Role::Follower { leader: Some(a) } => a.to_string(),
            _ => String::new(),
        }
    }

    pub(crate) fn leads(&self) -> bool {
        !matches!(self.role.get(), Role::Follower { .. })
    }

    /// This member's role, with `instance`, its volume work's client
    /// instance, if it leads (D125).
    fn lead(&self, instance: u64) -> Lead {
        match self.role.get() {
            Role::Standalone => Lead::Standalone,
            Role::Leader { since } => Lead::Leader { since, instance },
            Role::Follower { .. } => Lead::Follower,
        }
    }

    /// This member's own [`LeaderView`]: complete only on the leader.
    fn own_view(&self, now: Instant) -> LeaderView {
        LeaderView {
            heard: self
                .beats
                .borrow()
                .iter()
                .map(|(n, t)| (*n, now.duration_since(*t)))
                .collect(),
            inconsistent: self.inconsistent.borrow().clone(),
            flapping: self.flapping(now),
            space: self.space.borrow().clone(),
        }
    }

    /// [`Admin::LeaderView`]'s answer.
    async fn leader_view(&self) -> Option<LeaderView> {
        let leader = match self.role.get() {
            Role::Follower { leader: Some(a) } => a,
            Role::Follower { leader: None } => return None,
            _ => return Some(self.own_view(Instant::now())),
        };
        if let Some((at, v)) = &*self.leader_view.borrow()
            && at.elapsed() < LEADER_VIEW_FOR
        {
            return Some(v.clone());
        }
        let asked = async {
            let c = self.conn(leader).await.ok()?;
            c.call(&hyperstor_proto::Msg::LeaderState, &[]).await.ok()
        };
        let Ok(Some((
            hyperstor_proto::Msg::LeaderStateOk {
                heard,
                inconsistent,
                flapping,
                space,
            },
            _,
        ))) = hyperstor_reactor::time::timeout(LEADER_VIEW_FOR, asked).await
        else {
            return None;
        };
        let v = LeaderView {
            heard: heard
                .into_iter()
                .map(|(n, ms)| (n, Duration::from_millis(ms)))
                .collect(),
            inconsistent: inconsistent
                .into_iter()
                .map(|(p, g, k)| ((p, g), k))
                .collect(),
            flapping: flapping.into_iter().collect(),
            space: space.into_iter().map(|(d, t, f)| (d, (t, f))).collect(),
        };
        *self.leader_view.borrow_mut() = Some((Instant::now(), v.clone()));
        Some(v)
    }

    /// Applies what the Raft side reports (`replica::spawn`): maps the
    /// group committed, and who leads.
    pub async fn follow(
        self: Rc<Self>,
        mut rx: hyperstor_reactor::chan::Receiver<replica::ToReactor>,
    ) {
        use replica::{Leading, ToReactor};
        while let Some(ev) = rx.recv().await {
            match ev {
                ToReactor::Applied(m) => {
                    let m = *m;
                    self.persisted.set(true);
                    if m.epoch >= self.published.borrow().epoch {
                        *self.published.borrow_mut() = m.clone();
                    }
                    // The leader's working map runs ahead of what it
                    // published; a follower's is what was committed.
                    if !self.leads() || m.epoch > self.map.borrow().epoch {
                        *self.map.borrow_mut() = m;
                    }
                }
                ToReactor::Leader(Leading::Me) => {
                    if self.leads() {
                        continue;
                    }
                    let now = Instant::now();
                    self.role.set(Role::Leader { since: now });
                    self.lead_gen.set(self.lead_gen.get() + 1);
                    // Liveness starts afresh (D73): every node gets a
                    // timeout's grace from now.
                    self.started.set(now);
                    self.beats.borrow_mut().clear();
                    self.space.borrow_mut().clear();
                    self.kind_reports.borrow_mut().clear();
                    self.granted.borrow_mut().clear();
                    self.reports.borrow_mut().clear();
                    self.suspect.borrow_mut().clear();
                    self.fencing.borrow_mut().clear();
                    self.down_since.borrow_mut().clear();
                    self.damping.borrow_mut().clear();
                    self.inconsistent.borrow_mut().clear();
                    self.usage.borrow_mut().clear();
                    self.bump_usage();
                    let published = self.published.borrow().clone();
                    *self.map.borrow_mut() = published.clone();
                    if !self.persisted.get() {
                        // A new group: the cluster file's map is the first.
                        let me = self.clone();
                        hyperstor_reactor::spawn(async move {
                            me.commit(&published).await;
                        });
                    } else if let Some(seed) = &self.seed {
                        // Hosts added to the cluster file since: published
                        // like any change, once the wait is over.
                        self.map.borrow_mut().merge_new(seed);
                    }
                }
                ToReactor::Leader(l) => {
                    if self.leads() {
                        // What it ran as leader may be blocklisted next.
                        self.lead_gen.set(self.lead_gen.get() + 1);
                    }
                    self.role.set(Role::Follower {
                        leader: match l {
                            Leading::Other(a) => Some(a),
                            _ => None,
                        },
                    })
                }
            }
        }
    }

    /// Commits `map` through the group, if there is one; whether it was
    /// committed (no: this member no longer leads, or the group can't
    /// reach a majority).
    async fn commit(&self, map: &ClusterMap) -> bool {
        let Some(tx) = &self.commits else {
            return true;
        };
        let (reply, rx) = hyperstor_reactor::chan::oneshot();
        if tx
            .send(replica::Commit {
                map: map.clone(),
                reply,
            })
            .is_err()
        {
            return false;
        }
        // Not forever: without a majority of members a commit never ends,
        // and the publisher must keep trying (and say so) instead.
        let why = match hyperstor_reactor::time::timeout(COMMIT_TIMEOUT, rx.recv()).await {
            Ok(Some(Ok(()))) => {
                self.uncommitted.set(None);
                return true;
            }
            Ok(Some(Err(e))) => e.to_string(),
            Ok(None) => "the Raft side stopped".into(),
            Err(_) => format!(
                "no answer in {} s (does a majority of members answer?)",
                COMMIT_TIMEOUT.as_secs()
            ),
        };
        eprintln!(
            "hyperstor-meta: map epoch {} not committed: {why}",
            map.epoch
        );
        if self.uncommitted.get().is_none() {
            self.uncommitted.set(Some(Instant::now()));
        }
        false
    }

    /// Marks a device in or out of placement (19-administration.md §23.6) and
    /// recomputes the working map if that changed it; the publisher
    /// fences and publishes. Returns the working map's epoch.
    pub fn set_device_in(&self, device: u32, up_in: bool) -> Result<u64, String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        let mut map = self.map.borrow_mut();
        if map
            .set_in(device, up_in)
            .ok_or_else(|| format!("no device {device}"))?
        {
            map.recompute();
        }
        Ok(map.epoch)
    }

    /// Sets a device's weight (04-decisions.md D158): `Some(w)` fixes it as
    /// the operator chose; `None` lets it follow the device's capacity
    /// again, from its last report if there is one, else from its next.
    /// Returns the working map's epoch and the weight now.
    pub fn set_device_weight(
        &self,
        device: u32,
        weight: Option<f64>,
    ) -> Result<(u64, f64), String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        if let Some(w) = weight.filter(|w| !w.is_finite() || *w < 0.0) {
            return Err(format!("a weight is a number of 0 or more, not {w}"));
        }
        let mut map = self.map.borrow_mut();
        let Some(was) = map.devices.get(&device).map(|d| d.weight) else {
            return Err(format!("no device {device}"));
        };
        let reported = self
            .space
            .borrow()
            .get(&device)
            .map(|(t, _)| *t as f64 / TIB);
        let now = weight.or(reported).unwrap_or(was);
        let set = if weight.is_some() {
            map.fixed_weights.insert(device)
        } else {
            map.fixed_weights.remove(&device)
        };
        if set || now != was {
            if let Some(d) = map.devices.get_mut(&device) {
                d.weight = now;
            }
            map.recompute();
        }
        Ok((map.epoch, now))
    }

    /// Gives devices the weight of their capacity (04-decisions.md D158),
    /// from their space reports, except those an operator set. A device
    /// still at weight 0 (on a host that just joined) gets it at once; the
    /// others change together, once every live device in placement has
    /// reported, so devices of one size move no data on the way. A weight
    /// within 1 % of the capacity is left as it is.
    fn auto_weights(&self) {
        let space = self.space.borrow();
        let mut map = self.map.borrow_mut();
        let mut waiting = false;
        let mut changes: Vec<(u32, f64, bool)> = Vec::new();
        for d in map.devices.values() {
            let live = d.up_in && map.nodes.get(&d.node).is_some_and(|n| n.up);
            match space.get(&d.id) {
                None => waiting |= live,
                Some(&(total, _)) if total > 0 && !map.fixed_weights.contains(&d.id) => {
                    let want = total as f64 / TIB;
                    if (d.weight - want).abs() > want / 100.0 {
                        changes.push((d.id, want, d.weight == 0.0));
                    }
                }
                Some(_) => {}
            }
        }
        if waiting {
            changes.retain(|c| c.2);
        }
        if changes.is_empty() {
            return;
        }
        for (id, w, _) in &changes {
            if let Some(d) = map.devices.get_mut(id) {
                d.weight = *w;
            }
        }
        map.recompute();
        let list: Vec<String> = changes
            .iter()
            .map(|(id, w, _)| format!("{id}: {w:.4}"))
            .collect();
        eprintln!(
            "hyperstor-meta: device weights from their capacity in TiB (D158), map epoch {}: {}",
            map.epoch,
            list.join(", ")
        );
    }

    fn not_leader(&self) -> String {
        match self.leader_hint().as_str() {
            "" => NO_LEADER.into(),
            l => format!("this map service member isn't the leader; the leader is {l}"),
        }
    }

    /// Adds a storage host with one device per address in `addrs`, down
    /// until its first heartbeat, so nothing is placed on it before it
    /// runs. A host whose addresses are already a node's (installed again)
    /// gets that node back. Returns the node number.
    pub fn add_node(&self, rack: u32, addrs: &[SocketAddr]) -> Result<u32, String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        if addrs.is_empty() || addrs.len() > 64 {
            return Err("a storage host needs 1 to 64 addresses".into());
        }
        let mut map = self.map.borrow_mut();
        let mut owners: Vec<u32> = addrs
            .iter()
            .filter_map(|a| map.devices.values().find(|d| d.addr == *a).map(|d| d.node))
            .collect();
        owners.dedup();
        match owners.as_slice() {
            [] => {}
            // Installed again: the node's addresses, in order, then any
            // new ones, which are appended as its next devices (D167).
            [n] if {
                let mut have: Vec<(u32, SocketAddr)> = map
                    .devices
                    .values()
                    .filter(|d| d.node == *n)
                    .map(|d| (d.shard, d.addr))
                    .collect();
                have.sort();
                have.len() <= addrs.len()
                    && have
                        .iter()
                        .map(|h| h.1)
                        .eq(addrs[..have.len()].iter().copied())
            } =>
            {
                let n = *n;
                let known = map.devices.values().filter(|d| d.node == n).count();
                if known < addrs.len() {
                    let class = map
                        .devices
                        .values()
                        .find(|d| d.node == n)
                        .map_or_else(|| "nvme".to_string(), |d| d.class.clone());
                    for a in &addrs[known..] {
                        map.add_device(n, *a, &class)
                            .ok_or("the node has 1000 devices")?;
                    }
                    map.recompute();
                }
                return Ok(n);
            }
            _ => {
                return Err(format!(
                    "addresses already in use by node(s) {owners:?} with other addresses"
                ));
            }
        }
        let id = map.next_node_id();
        // Weight 0 until each device reports its capacity (D158), so
        // nothing is placed by a guess.
        for d in map.add_node(id, addrs, rack) {
            if let Some(d) = map.devices.get_mut(&d) {
                d.weight = 0.0;
            }
        }
        if let Some(n) = map.nodes.get_mut(&id) {
            n.up = false;
        }
        map.grow_meta_pool();
        map.recompute();
        Ok(id)
    }

    /// Appends a device at `addr` to `node` (24-pools-and-devices.md
    /// §27.2.1, D167), at weight 0 until it reports its capacity (D158).
    /// `class` defaults to that of the node's other devices. Returns the
    /// device's id and the working map's epoch.
    pub fn add_device(
        &self,
        node: u32,
        addr: SocketAddr,
        class: Option<&str>,
    ) -> Result<(u32, u64), String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        if let Some(c) = class.filter(|c| !valid_class(c)) {
            return Err(format!(
                "a class is 1 to 32 lowercase letters, digits and dashes, not {c:?}"
            ));
        }
        let mut map = self.map.borrow_mut();
        if !map.nodes.contains_key(&node) {
            return Err(format!("no node {node}"));
        }
        if let Some(d) = map.devices.values().find(|d| d.addr == addr) {
            return Err(format!("{addr} is already device {}'s address", d.id));
        }
        let fixed = class.is_some();
        let class = class.map(str::to_string).unwrap_or_else(|| {
            map.devices
                .values()
                .find(|d| d.node == node)
                .map_or_else(|| "nvme".to_string(), |d| d.class.clone())
        });
        let id = map
            .add_device(node, addr, &class)
            .ok_or_else(|| format!("node {node} has 1000 devices"))?;
        // A class given is the operator's; otherwise the device takes its
        // kind once it reports it (D178).
        if fixed {
            map.fixed_classes.insert(id);
        }
        map.recompute();
        Ok((id, map.epoch))
    }

    /// Sets a device's class as an operator chose it, or (`None`) lets it
    /// follow the device's kind again (24-pools-and-devices.md §27.2.4,
    /// D178). Returns the working map's epoch and the class now.
    pub fn set_device_class(
        &self,
        device: u32,
        class: Option<&str>,
    ) -> Result<(u64, String), String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        if let Some(c) = class.filter(|c| !valid_class(c)) {
            return Err(format!(
                "a class is 1 to 32 lowercase letters, digits and dashes, not {c:?}"
            ));
        }
        self.apply_kinds();
        let mut map = self.map.borrow_mut();
        if map.set_class(device, class)? {
            map.recompute();
        }
        let now = map.devices[&device].class.clone();
        Ok((map.epoch, now))
    }

    /// Deletes pool `pool` (24-pools-and-devices.md §27.3.4; D176, D181):
    /// it leaves the map at once, its id kept so none is given again, and
    /// each device deletes what it holds of it. The caller checks first
    /// that it holds no volume, snapshot or trash. Refused for `.meta`.
    /// Returns the working map's epoch.
    pub fn delete_pool(&self, pool: u32) -> Result<u64, String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        if pool == META_POOL {
            return Err(".meta holds every volume's header; it can't be deleted".into());
        }
        let mut map = self.map.borrow_mut();
        if !map.delete_pool(pool) {
            return Err(format!("no pool {pool}"));
        }
        Ok(map.epoch)
    }

    /// Sets a pool's class set (D164): [`Meta::set_pool`] with only the
    /// classes.
    pub fn set_pool_classes(
        &self,
        pool: u32,
        classes: Vec<String>,
        force: bool,
    ) -> Result<(u64, bool), String> {
        let change = PoolChange {
            classes: Some(classes),
            ..Default::default()
        };
        self.set_pool(pool, change, force)
    }

    /// Changes a pool (24-pools-and-devices.md §27.3.2, §27.4; D164,
    /// D172): its class set, name or failure domain. Refused for `.meta`,
    /// whose devices follow the flash classes. The pool's devices under
    /// the new set and domain must span as many failure domains as its
    /// copies, and hold hard disks and flash both only when `force` or a
    /// cache pool; a name is checked as `create_pool` checks it. Returns
    /// the working map's epoch and whether the devices mix kinds.
    pub fn set_pool(
        &self,
        pool: u32,
        change: PoolChange,
        force: bool,
    ) -> Result<(u64, bool), String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        if pool == META_POOL {
            return Err(".meta places on every flash device; it can't be changed".into());
        }
        let PoolChange {
            classes,
            name,
            domain,
            pg_count,
        } = change;
        if classes.is_none() && name.is_none() && domain.is_none() && pg_count.is_none() {
            return Err("nothing to change: give classes, a name, a domain or a PG count".into());
        }
        let mut map = self.map.borrow_mut();
        let Some(info) = map.pools.get(&pool).cloned() else {
            return Err(format!("no pool {pool}"));
        };
        let mut wanted = info.clone();
        if let Some(mut classes) = classes {
            classes.sort();
            classes.dedup();
            wanted.classes = classes;
        }
        if let Some(domain) = domain {
            wanted.domain = domain;
        }
        if let Some(name) = name {
            check_pool_name(&name)?;
            if map.pools.values().any(|p| p.id != pool && p.name == name) {
                return Err(format!("a pool is already named {name}"));
            }
            wanted.name = name;
        }
        if let Some(n) = pg_count {
            if info.split.is_some() {
                return Err(format!(
                    "pool {pool} is splitting to {} PGs already",
                    info.pg_count * 2
                ));
            }
            if n != info.pg_count * 2 {
                return Err(format!(
                    "pool {pool} has {} PGs: a split doubles them, to {}",
                    info.pg_count,
                    info.pg_count * 2
                ));
            }
        }
        let mixed = check_class_set(&map, &wanted, force)?;
        if wanted != info {
            map.pools.insert(pool, wanted);
            map.recompute();
        }
        if pg_count.is_some() {
            map.start_split(pool)?;
            drop(map);
            self.advance_splits();
            return Ok((self.map.borrow().epoch, mixed));
        }
        Ok((map.epoch, mixed))
    }

    /// Makes a pool (24-pools-and-devices.md §27.3.1; D163, D171): its
    /// classes are required, checked as `SetPool` checks them; its id is
    /// the next one no pool has had. Returns the id, its PG count and the
    /// working map's epoch, and whether its classes mix hard disks and
    /// flash (forced).
    pub fn create_pool(&self, new: NewPool) -> Result<(u32, u32, u64, bool), String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        let NewPool {
            name,
            kind,
            size,
            min_size,
            pg_count,
            chunk_size,
            domain,
            mut classes,
            force,
        } = new;
        classes.sort();
        classes.dedup();
        if classes.is_empty() {
            return Err("a new pool names its device classes (D171)".into());
        }
        check_pool_name(&name)?;
        if size == 0 || min_size == 0 || min_size > size {
            return Err(format!(
                "copies {size}, min {min_size}: min is 1 to the copies"
            ));
        }
        if chunk_size == 0 || chunk_size % 4096 != 0 {
            return Err(format!(
                "a chunk is a multiple of 4096 bytes, not {chunk_size}"
            ));
        }
        if let Some(n) = pg_count.filter(|n| !n.is_power_of_two()) {
            return Err(format!("a PG count is a power of two, not {n}"));
        }
        let mut map = self.map.borrow_mut();
        if map.pools.values().any(|p| p.name == name) {
            return Err(format!("a pool is already named {name}"));
        }
        let id = map.next_pool_id();
        let mut info = PoolInfo {
            id,
            name,
            kind,
            size,
            min_size,
            pg_count: 0,
            domain,
            chunk_size,
            classes,
            added: BTreeSet::new(),
            removed: BTreeSet::new(),
            placement: 0,
            split: None,
        };
        let eligible = map
            .devices
            .values()
            .filter(|d| d.up_in && info.uses(d))
            .count();
        let mixed = check_class_set(&map, &info, force)?;
        info.pg_count = pg_count.unwrap_or_else(|| default_pg_count(eligible, size));
        info.placement = info.pg_count;
        let pgs = info.pg_count;
        map.add_pool(info);
        map.recompute();
        Ok((id, pgs, map.epoch, mixed))
    }

    /// Adds `device` to pool `pool`, or (`member` false) removes it
    /// (24-pools-and-devices.md §27.4; D180): a device of the pool's
    /// classes that was removed is taken back, one outside them added; a
    /// device added is dropped, one of its classes removed. Checked as
    /// `SetPool` checks a class set; `.meta` is refused. Returns the
    /// working map's epoch and whether the pool's devices mix hard disks
    /// and flash (forced).
    pub fn set_pool_device(
        &self,
        pool: u32,
        device: u32,
        member: bool,
        force: bool,
    ) -> Result<(u64, bool), String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        if pool == META_POOL {
            return Err(".meta places on every flash device; its devices can't be set".into());
        }
        let mut map = self.map.borrow_mut();
        let Some(info) = map.pools.get(&pool).cloned() else {
            return Err(format!("no pool {pool}"));
        };
        let Some(d) = map.devices.get(&device).cloned() else {
            return Err(format!("no device {device}"));
        };
        let mut wanted = info.clone();
        let of_classes = wanted.classes.is_empty() || wanted.classes.contains(&d.class);
        if member {
            wanted.removed.remove(&device);
            if !of_classes {
                wanted.added.insert(device);
            }
        } else {
            wanted.added.remove(&device);
            if of_classes {
                wanted.removed.insert(device);
            }
        }
        let mixed = check_class_set(&map, &wanted, force)?;
        if wanted != info {
            map.pools.insert(pool, wanted);
            map.recompute();
        }
        Ok((map.epoch, mixed))
    }

    /// Puts the kinds devices reported into the map once every device of
    /// a node that is up has reported one (D178): until then a node from
    /// before kinds may still be running, and couldn't read the map.
    fn apply_kinds(&self) {
        let reports = self.kind_reports.borrow();
        let mut map = self.map.borrow_mut();
        let waiting = map.devices.values().any(|d| {
            map.nodes.get(&d.node).is_some_and(|n| n.up)
                && !map.kinds.contains_key(&d.id)
                && !reports.contains_key(&d.id)
        });
        if waiting || reports.is_empty() {
            return;
        }
        let mut changed = false;
        for (&d, &k) in reports.iter() {
            changed |= map.set_kind(d, k);
        }
        if changed {
            map.recompute();
        }
    }

    /// Takes every device of `node` out of placement, as `set_device_in`
    /// does for one, and returns the PGs that move; with `dry_run` changes
    /// nothing and returns what would.
    pub fn drain(&self, node: u32, dry_run: bool) -> Result<Vec<PgMove>, String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        let mut map = self.map.borrow_mut();
        if !map.nodes.contains_key(&node) {
            return Err(format!("no node {node}"));
        }
        let mut after = map.clone();
        let mut changed = false;
        for d in after.devices.values_mut().filter(|d| d.node == node) {
            changed |= d.up_in;
            d.up_in = false;
        }
        if changed {
            after.recompute();
        }
        let moves = map
            .pgs
            .iter()
            .filter_map(|(k, before)| {
                let a = &after.pgs.get(k)?.up;
                (*a != before.acting).then(|| PgMove {
                    pool: k.0,
                    pg: k.1,
                    before: before.acting.clone(),
                    after: a.clone(),
                })
            })
            .collect();
        if !dry_run && changed {
            *map = after;
        }
        Ok(moves)
    }

    /// Takes `device` out so its PGs move off it, to remove it
    /// (24-pools-and-devices.md §27.2.5, D175); with `dry_run` changes
    /// nothing. Refused when a pool using it would keep fewer copies than
    /// its `min`, or (unless `force`) fewer than its `size`, for want of
    /// failure domains, and when a device that takes its data would pass
    /// [`FULL`], by the primaries' PG usage (D155) and the devices' space
    /// reports.
    pub fn remove_device(
        &self,
        device: u32,
        force: bool,
        dry_run: bool,
    ) -> Result<Removal, String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        let mut map = self.map.borrow_mut();
        let Some(dev) = map.devices.get(&device).cloned() else {
            return Err(format!("no device {device}"));
        };
        let mut after = map.clone();
        if after.set_in(device, false) == Some(true) {
            after.recompute();
        }
        let mut short = Vec::new();
        for p in map.pools.values().filter(|p| p.uses(&dev)) {
            let (before, left) = (domains(&map, p), domains(&after, p));
            if left >= before || left >= p.size as usize {
                continue;
            }
            if left < p.min_size as usize {
                return Err(format!(
                    "without device {device}, pool {} would keep {left} copies, fewer than its min of {}",
                    p.id, p.min_size
                ));
            }
            if !force {
                return Err(format!(
                    "without device {device}, pool {} would keep {left} of its {} copies; --force allows it",
                    p.id, p.size
                ));
            }
            short.push((p.id, left as u32, p.size));
        }
        let mut moves = Vec::new();
        let mut gains: BTreeMap<u32, u64> = BTreeMap::new();
        let usage = self.usage.borrow();
        for (k, was) in &map.pgs {
            let Some(now) = after.pgs.get(k) else {
                continue;
            };
            if now.up != was.acting {
                moves.push(PgMove {
                    pool: k.0,
                    pg: k.1,
                    before: was.acting.clone(),
                    after: now.up.clone(),
                });
            }
            // A copy's bytes: an EC shard holds 1/k of them.
            let bytes = usage
                .get(k)
                .map_or(0, |&(_, b)| match map.pools[&k.0].kind {
                    PoolKind::Ec { k, .. } => b / k.max(1) as u64,
                    _ => b,
                });
            for d in now.up.iter().filter(|d| !was.acting.contains(d)) {
                *gains.entry(*d).or_default() += bytes;
            }
        }
        drop(usage);
        let reports = self.space.borrow();
        let mut space = Vec::new();
        for (&d, &gain) in &gains {
            let Some(&(total, free)) = reports.get(&d) else {
                continue;
            };
            let used = total.saturating_sub(free) + gain;
            if used as f64 > total as f64 * FULL {
                return Err(format!(
                    "device {d} would be {:.0}% full with device {device}'s data, past {:.0}%: it has nowhere to go",
                    used as f64 * 100.0 / total.max(1) as f64,
                    FULL * 100.0
                ));
            }
            space.push((d, used, total));
        }
        if !dry_run {
            if map.set_in(device, false) == Some(true) {
                map.recompute();
            }
            map.release(device);
        }
        Ok(Removal {
            moves,
            short,
            space,
        })
    }

    /// Removes `device` from the map once it is out and no PG lists it in
    /// `up` or `acting`, and its node with it if it was the last; both ids
    /// are kept (D182). `None` if the map hasn't the device (removed
    /// already).
    pub fn finish_removal(&self, device: u32) -> Result<Option<(bool, u64)>, String> {
        if !self.leads() {
            return Err(self.not_leader());
        }
        let mut map = self.map.borrow_mut();
        let Some(d) = map.devices.get(&device) else {
            return Ok(None);
        };
        if d.up_in {
            return Err(format!("device {device} was brought back in"));
        }
        if let Some((k, _)) = map
            .pgs
            .iter()
            .find(|(_, p)| p.up.contains(&device) || p.acting.contains(&device))
        {
            return Err(format!("PG {}.{} still lists device {device}", k.0, k.1));
        }
        let gone = map.remove_device(device).unwrap_or(false);
        self.space.borrow_mut().remove(&device);
        self.kind_reports.borrow_mut().remove(&device);
        Ok(Some((gone, map.epoch)))
    }

    /// Answers the cluster API's requests (`api`), which arrive from its
    /// own thread: the map service's state lives on this reactor.
    pub async fn admin(
        self: Rc<Self>,
        mut rx: hyperstor_reactor::chan::Receiver<Admin>,
        map_address: SocketAddr,
    ) {
        // The client for volume operations, connected on first use to this
        // member, which serves the map (and redirects the rest).
        // A new one each time this member gains or loses the lead (D125):
        // what still runs keeps the old one, which the next leader may
        // blocklist.
        type Cached = Rc<RefCell<Option<(u64, Rc<hyperstor_client::Client>)>>>;
        let client: Cached = Rc::default();
        let me = self.clone();
        let connect = move |client: Cached| {
            let generation = me.lead_gen.get();
            async move {
                if let Some((g, c)) = client.borrow().clone()
                    && g == generation
                {
                    return Ok(c);
                }
                let c = hyperstor_client::Client::connect(map_address, 0)
                    .await
                    .map_err(volume_fault)?;
                *client.borrow_mut() = Some((generation, c.clone()));
                Ok::<_, (VolumeFault, String)>(c)
            }
        };
        while let Some(req) = rx.recv().await {
            match req {
                Admin::Published(reply) => {
                    let now = Instant::now();
                    let heard = self
                        .beats
                        .borrow()
                        .iter()
                        .map(|(n, t)| (*n, now.duration_since(*t)))
                        .collect();
                    let _ = reply.send((
                        self.published.borrow().clone(),
                        heard,
                        self.inconsistent.borrow().clone(),
                        self.flapping(now),
                    ));
                }
                // A task of its own: it may wait on the leader.
                Admin::LeaderView(reply) => {
                    let me = self.clone();
                    hyperstor_reactor::spawn(async move {
                        let _ = reply.send(me.leader_view().await);
                    });
                }
                Admin::SetDeviceIn {
                    device,
                    up_in,
                    reply,
                } => {
                    let _ = reply.send(self.set_device_in(device, up_in));
                }
                Admin::SetDeviceWeight {
                    device,
                    weight,
                    reply,
                } => {
                    let _ = reply.send(self.set_device_weight(device, weight));
                }
                // Each in a task of its own: a delete runs through every
                // chunk of the volume.
                Admin::ListVolumes { pool, trash, reply } => {
                    let c = connect(client.clone());
                    hyperstor_reactor::spawn(async move {
                        let r = async {
                            let c = c.await?;
                            c.refresh_map().await.map_err(volume_fault)?;
                            // What's in the trash is listed only if asked (D54).
                            Ok(c.list_volumes(pool)
                                .await
                                .map_err(volume_fault)?
                                .into_iter()
                                .filter(|v| trash || v.state != VolumeState::Trashed)
                                .collect())
                        };
                        let _ = reply.send(r.await);
                    });
                }
                Admin::CreateVolume {
                    pool,
                    size,
                    name,
                    id,
                    reply,
                } => {
                    let c = connect(client.clone());
                    hyperstor_reactor::spawn(async move {
                        let r = async {
                            let c = c.await?;
                            c.refresh_map().await.map_err(volume_fault)?;
                            c.create_volume(pool, size, id, &name)
                                .await
                                .map_err(volume_fault)
                        };
                        let _ = reply.send(r.await);
                    });
                }
                Admin::ExpandVolume { id, size, reply } => {
                    let c = connect(client.clone());
                    hyperstor_reactor::spawn(async move {
                        let r = async {
                            let c = c.await?;
                            c.refresh_map().await.map_err(volume_fault)?;
                            c.expand_volume(id, size).await.map_err(volume_fault)
                        };
                        let _ = reply.send(r.await);
                    });
                }
                Admin::Lead(reply) => {
                    let (me, c) = (self.clone(), connect(client.clone()));
                    hyperstor_reactor::spawn(async move {
                        let lead = match me.lead(0) {
                            Lead::Leader { .. } => match c.await {
                                // Read after connecting: the lead may have
                                // changed meanwhile.
                                Ok(c) => me.lead(c.instance),
                                Err(_) => Lead::Follower,
                            },
                            l => l,
                        };
                        let _ = reply.send(lead);
                    });
                }
                Admin::Blocklist { ids, reply } => {
                    let me = self.clone();
                    hyperstor_reactor::spawn(async move {
                        let r = match me.blocklist(ids).await {
                            hyperstor_proto::Msg::BlocklistOk { map_epoch } => Ok(map_epoch),
                            hyperstor_proto::Msg::Error { detail, .. } => Err(detail),
                            m => Err(format!("unexpected {m:?}")),
                        };
                        let _ = reply.send(r);
                    });
                }
                Admin::Uncommitted(reply) => {
                    let since = self.uncommitted.get().filter(|_| self.leads());
                    let _ = reply.send(since.map(|t| t.elapsed()));
                }
                Admin::PgUsage { pool, since, reply } => {
                    let _ = reply.send(self.pool_usage(pool, since));
                }
                Admin::Leads(reply) => {
                    let _ = reply.send(if self.leads() {
                        Ok(())
                    } else {
                        Err(self.not_leader())
                    });
                }
                Admin::AddNode { rack, addrs, reply } => {
                    let _ = reply.send(self.add_node(rack, &addrs));
                }
                Admin::SetPoolDevice {
                    pool,
                    device,
                    member,
                    force,
                    reply,
                } => {
                    let _ = reply.send(self.set_pool_device(pool, device, member, force));
                }
                Admin::DeletePool { pool, reply } => {
                    let _ = reply.send(self.delete_pool(pool));
                }
                Admin::RemoveDevice {
                    device,
                    force,
                    dry_run,
                    reply,
                } => {
                    let _ = reply.send(self.remove_device(device, force, dry_run));
                }
                Admin::Release { device, reply } => {
                    let mut map = self.map.borrow_mut();
                    let out = map.devices.get(&device).is_some_and(|d| !d.up_in);
                    let _ = reply.send(if out && self.leads() {
                        map.release(device)
                    } else {
                        0
                    });
                }
                Admin::FinishRemoval { device, reply } => {
                    let _ = reply.send(self.finish_removal(device));
                }
                Admin::CreatePool { new, reply } => {
                    let _ = reply.send(self.create_pool(new));
                }
                Admin::SetPool {
                    pool,
                    change,
                    force,
                    reply,
                } => {
                    let _ = reply.send(self.set_pool(pool, change, force));
                }
                Admin::SetDeviceClass {
                    device,
                    class,
                    reply,
                } => {
                    let _ = reply.send(self.set_device_class(device, class.as_deref()));
                }
                Admin::AddDevice {
                    node,
                    addr,
                    class,
                    reply,
                } => {
                    let _ = reply.send(self.add_device(node, addr, class.as_deref()));
                }
                Admin::Drain {
                    node,
                    dry_run,
                    reply,
                } => {
                    let _ = reply.send(self.drain(node, dry_run));
                }
                Admin::AuditRead { query, reply } => {
                    let c = connect(client.clone());
                    hyperstor_reactor::spawn(async move {
                        let r = async {
                            let c = c.await?;
                            c.refresh_map().await.map_err(volume_fault)?;
                            audit_log::read(&c, &query).await.map_err(volume_fault)
                        };
                        let _ = reply.send(r.await);
                    });
                }
                Admin::Trash { op, reply } => {
                    let c = connect(client.clone());
                    hyperstor_reactor::spawn(async move {
                        let r = async {
                            let c = c.await?;
                            c.refresh_map().await.map_err(volume_fault)?;
                            run_trash_op(c, op).await.map_err(volume_fault)
                        };
                        let _ = reply.send(r.await);
                    });
                }
                Admin::Snapshots { op, reply } => {
                    let c = connect(client.clone());
                    hyperstor_reactor::spawn(async move {
                        let r = async {
                            let c = c.await?;
                            c.refresh_map().await.map_err(volume_fault)?;
                            run_snap_op(c, op).await.map_err(volume_fault)
                        };
                        let _ = reply.send(r.await);
                    });
                }
                Admin::DeleteVolume { id, force, reply } => {
                    let c = connect(client.clone());
                    hyperstor_reactor::spawn(async move {
                        let r = async {
                            let c = c.await?;
                            c.refresh_map().await.map_err(volume_fault)?;
                            c.trash_volume(id, force).await.map_err(volume_fault)
                        };
                        let _ = reply.send(r.await);
                    });
                }
            }
        }
    }

    pub fn lease(&self) -> Duration {
        self.lease
    }

    /// Marks nodes down after `timeout` without a heartbeat, and up again
    /// when they send one, recomputing the working map on every change. A
    /// node gets `timeout` of grace from the service's start. The timeout
    /// must exceed the lease with margin, so a node marked down has
    /// stopped serving (its lease has run out) by the time it is.
    pub async fn liveness(self: Rc<Self>, timeout: Duration) {
        assert!(
            timeout >= self.lease + self.lease / 4,
            "liveness timeout must exceed the lease by a quarter"
        );
        loop {
            sleep((timeout / 4).min(Duration::from_millis(100))).await;
            if self.may_change_roles() && self.liveness_tick(timeout, Instant::now()) {
                self.map.borrow_mut().recompute();
            }
            if self.may_change_roles() {
                self.advance_splits();
            }
        }
    }

    /// Moves splits on (24-pools-and-devices.md §27.5, D183): flips each
    /// marked PG every member of whose acting set has reported it split at
    /// its epoch, and marks more PGs, the clean ones, up to
    /// [`SPLITS_AT_ONCE`] per pool. The last flip doubles the count. Then,
    /// once every PG of a split pool is clean, places [`SPLITS_AT_ONCE`]
    /// more of its new PGs on their own devices (D173), so they move a
    /// few at a time, under the backfill budget.
    pub fn advance_splits(&self) {
        if !self.leads() {
            return;
        }
        let mut map = self.map.borrow_mut();
        let moving: Vec<u32> = map
            .pools
            .values()
            .filter(|p| p.split.is_none() && p.placement < p.pg_count)
            .map(|p| p.id)
            .collect();
        let mut placed = false;
        for pool in moving {
            let clean = map.pgs.iter().filter(|(k, _)| k.0 == pool).all(|(_, i)| {
                !i.acting.is_empty()
                    && i.acting == i.up
                    && i.acting.iter().all(|d| *d != NO_DEVICE && map.live(*d))
            });
            if clean && let Some(p) = map.pools.get_mut(&pool) {
                p.placement = (p.placement + SPLITS_AT_ONCE as u32).min(p.pg_count);
                placed = true;
            }
        }
        if placed {
            map.recompute();
        }
        let splitting: Vec<u32> = map
            .pools
            .values()
            .filter(|p| p.split.is_some())
            .map(|p| p.id)
            .collect();
        for pool in splitting {
            let marked = map.pools[&pool]
                .split
                .as_ref()
                .map(|s| s.marked.clone())
                .unwrap_or_default();
            for pg in marked {
                let Some(info) = map.pgs.get(&(pool, pg)) else {
                    continue;
                };
                let members: Vec<u32> = info
                    .acting
                    .iter()
                    .copied()
                    .filter(|d| *d != NO_DEVICE)
                    .collect();
                let all = self
                    .split_reports
                    .borrow()
                    .get(&(pool, pg))
                    .is_some_and(|(e, by)| {
                        *e == info.epoch && members.iter().all(|d| by.contains(d))
                    });
                if !members.is_empty() && all && map.flip_split(pool, pg) {
                    self.split_reports.borrow_mut().remove(&(pool, pg));
                }
            }
            let Some(p) = map.pools.get(&pool) else {
                continue;
            };
            let Some(s) = &p.split else {
                continue;
            };
            let clean = |pg: u32| {
                map.pgs.get(&(pool, pg)).is_some_and(|i| {
                    !i.acting.is_empty()
                        && i.acting == i.up
                        && i.acting.iter().all(|d| map.live(*d))
                })
            };
            let next: Vec<u32> = (0..p.pg_count)
                .filter(|pg| !s.marked.contains(pg) && !s.done.contains(pg) && clean(*pg))
                .take(SPLITS_AT_ONCE.saturating_sub(s.marked.len()))
                .collect();
            for pg in next {
                map.mark_split(pool, pg);
            }
        }
    }

    /// One liveness check; true if any node changed state.
    fn liveness_tick(&self, timeout: Duration, now: Instant) -> bool {
        let mut changed = false;
        {
            let beats = self.beats.borrow();
            let mut map = self.map.borrow_mut();
            // Peer reports (D40): a node reported silent by at least two
            // distinct nodes, recently, is down even if its heartbeats
            // still arrive (cut off from its peers), and gets no lease.
            let fresh = timeout / 2;
            let mut reports = self.reports.borrow_mut();
            for r in reports.values_mut() {
                r.retain(|_, t| now.duration_since(*t) < fresh);
            }
            let mut suspect = self.suspect.borrow_mut();
            suspect.clear();
            for (peer, r) in reports.iter() {
                if r.len() >= 2 {
                    suspect.insert(*peer);
                }
            }
            let mut damping = self.damping.borrow_mut();
            for n in map.nodes.values_mut() {
                let last = beats.get(&n.id).copied().unwrap_or(self.started.get());
                let silent = now.duration_since(last) >= timeout;
                let reported = suspect.contains(&n.id);
                let alive = !silent && !reported;
                // Damping (D149): a node that went down comes back only
                // once its hold has passed. It never keeps a node up.
                let held = !n.up
                    && alive
                    && damping
                        .get(&n.id)
                        .and_then(|d| d.hold_until)
                        .is_some_and(|t| now < t);
                if n.up != alive && !held {
                    let mut hold_note = String::new();
                    if !alive {
                        let d = damping.entry(n.id).or_default();
                        d.downs.push_back(now);
                        while d
                            .downs
                            .front()
                            .is_some_and(|t| now.duration_since(*t) >= FLAP_WINDOW)
                        {
                            d.downs.pop_front();
                        }
                        let hold = flap_hold(d.downs.len());
                        d.hold_until = Some(now + hold);
                        hold_note = format!("; held down {} s", hold.as_secs());
                    }
                    n.up = alive;
                    changed = true;
                    // In the journal: why a node went down is the first
                    // question about any failover.
                    let why = match (silent, reported) {
                        (false, false) => "heartbeats again".to_string(),
                        (true, _) => format!(
                            "no heartbeat for {} ms",
                            now.duration_since(last).as_millis()
                        ),
                        (false, true) => {
                            let mut by: Vec<u32> = reports
                                .get(&n.id)
                                .map(|r| r.keys().copied().collect())
                                .unwrap_or_default();
                            by.sort();
                            format!("reported silent by nodes {by:?}")
                        }
                    };
                    eprintln!(
                        "hyperstor-meta: node {} {} ({why}{hold_note})",
                        n.id,
                        if alive { "up" } else { "down" }
                    );
                }
            }
            // Down and out (07-placement.md §5.3): a node down for the
            // interval has its devices marked out, so its PGs are placed
            // elsewhere; they come back in when it returns.
            let mut since = self.down_since.borrow_mut();
            let nodes: Vec<(u32, bool)> = map.nodes.values().map(|n| (n.id, n.up)).collect();
            for (id, up) in nodes {
                if up {
                    since.remove(&id);
                    if !map.auto_out.is_empty() && map.auto_in_node(id) {
                        changed = true;
                    }
                } else {
                    let t = *since.entry(id).or_insert(now);
                    if now.duration_since(t) >= self.down_out.get() && map.auto_out_node(id) {
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Nodes that went down at least twice within [`FLAP_WINDOW`] (D149).
    pub fn flapping(&self, now: Instant) -> Flapping {
        self.damping
            .borrow()
            .iter()
            .filter_map(|(n, d)| {
                let k = d
                    .downs
                    .iter()
                    .filter(|t| now.duration_since(**t) < FLAP_WINDOW)
                    .count() as u32;
                (k >= 2).then_some((*n, k))
            })
            .collect()
    }

    /// Publishes working-map changes: fence, publish, push.
    pub async fn publisher(self: Rc<Self>) {
        let mut pruned = Instant::now();
        loop {
            sleep(Duration::from_millis(10)).await;
            if self.may_change_roles() && pruned.elapsed() >= Duration::from_secs(1) {
                pruned = Instant::now();
                self.map.borrow_mut().blocklist_prune(unix_now());
            }
            if self.may_change_roles()
                && self.map.borrow().epoch > self.published.borrow().epoch
                && !self.publish().await
            {
                // Not committed: try again shortly, not every 10 ms.
                sleep(Duration::from_millis(500)).await;
            }
        }
    }

    /// Devices that lose a role between `old` and `new`, with the PGs.
    pub fn losers(old: &ClusterMap, new: &ClusterMap) -> HashMap<u32, Vec<(u32, u32)>> {
        let mut out: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for (k, o) in &old.pgs {
            let Some(n) = new.pgs.get(k) else { continue };
            for (pos, d) in o.acting.iter().enumerate() {
                if *d == NO_DEVICE {
                    continue;
                }
                let lost = match n.acting.iter().position(|x| x == d) {
                    None => true,
                    // Losing primacy, or (EC) changing shard position.
                    Some(p) => {
                        (pos == 0 && p != 0)
                            || (n.acting.len() == o.acting.len()
                                && p != pos
                                && new
                                    .pools
                                    .get(&k.0)
                                    .is_some_and(|pl| matches!(pl.kind, PoolKind::Ec { .. })))
                    }
                };
                if lost {
                    out.entry(*d).or_default().push(*k);
                }
            }
        }
        out
    }

    /// Fences, commits and publishes the working map; false if it wasn't
    /// committed.
    async fn publish(self: &Rc<Self>) -> bool {
        let new = self.map.borrow().clone();
        let old = self.published.borrow().clone();
        let mut waits = Vec::new();
        for (dev, pgs) in Self::losers(&old, &new) {
            let me = self.clone();
            let node = new
                .devices
                .get(&dev)
                .or(old.devices.get(&dev))
                .map(|d| d.node);
            let addr = new.device_addr(dev).or(old.device_addr(dev));
            let up = node.and_then(|n| new.nodes.get(&n)).is_some_and(|n| n.up);
            waits.push(hyperstor_reactor::spawn(async move {
                me.fence(node, addr, up, pgs, new.epoch).await
            }));
        }
        for w in waits {
            w.await;
        }
        // Committed by the group before anyone sees it: a new leader
        // starts from it.
        if !self.may_change_roles() || !self.commit(&new).await {
            return false;
        }
        if new.epoch > self.published.borrow().epoch {
            *self.published.borrow_mut() = new.clone();
        }
        // Fast path: push the new map to every live device.
        let bytes = Rc::new(new.encode());
        for d in new.devices.values() {
            if !new.nodes.get(&d.node).is_some_and(|n| n.up) {
                continue;
            }
            let (me, addr, bytes, epoch) = (self.clone(), d.addr, bytes.clone(), new.epoch);
            let dev = d.id;
            hyperstor_reactor::spawn(async move {
                if let Ok(c) = me.conn(addr).await
                    && let Ok(Ok((hyperstor_proto::Msg::MapPushOk, _))) =
                        hyperstor_reactor::time::timeout(
                            me.lease,
                            c.call(
                                &hyperstor_proto::Msg::MapPush {
                                    epoch,
                                    map: bytes.to_vec(),
                                },
                                &[],
                            ),
                        )
                        .await
                {
                    let mut acked = me.acked.borrow_mut();
                    let a = acked.entry(dev).or_default();
                    *a = (*a).max(epoch);
                }
            });
        }
        true
    }

    /// Blocklists client instances or lease identities (06-metadata.md §9, a
    /// forced takeover; D150) in one published map, answered once every
    /// storage node holds a map that has them, or has lost its map lease. A node renews its lease only
    /// with a map at least as new as the one the leader has published
    /// (`hyperstord`'s heartbeat), so a lease granted before the change
    /// runs out within 1.25 leases, and none granted after it can be used
    /// without the entry.
    pub async fn blocklist(self: Rc<Self>, ids: Vec<u64>) -> hyperstor_proto::Msg {
        use hyperstor_proto::{Code, Msg};
        let fail = |code: Code, detail: String| Msg::Error {
            code,
            map_epoch: self.published.borrow().epoch,
            detail,
        };
        let t = Instant::now();
        let deadline = 4 * self.lease + Duration::from_secs(2);
        // A new leader first waits out the old one's leases (D73).
        while !self.may_change_roles() {
            if !self.leads() {
                return fail(Code::NotLeader, self.leader_hint());
            }
            if t.elapsed() > deadline {
                return fail(
                    Code::Unavailable,
                    "the map service can't change the map".into(),
                );
            }
            sleep(Duration::from_millis(10)).await;
        }
        let expires = unix_now() + BLOCKLIST_FOR.as_secs();
        if ids.is_empty() || ids.contains(&0) {
            return fail(Code::Invalid, "nothing to blocklist".into());
        }
        let epoch = {
            let mut m = self.map.borrow_mut();
            ids.iter()
                .map(|i| m.blocklist_add(*i, expires))
                .max()
                .unwrap_or(0)
        };
        // The publisher commits and pushes it.
        while self.published.borrow().epoch < epoch {
            if !self.leads() {
                return fail(Code::NotLeader, self.leader_hint());
            }
            if t.elapsed() > deadline {
                return fail(Code::Unavailable, "the blocklist wasn't published".into());
            }
            sleep(Duration::from_millis(5)).await;
        }
        let until = Instant::now() + self.lease + self.lease / 4;
        loop {
            let held = {
                let p = self.published.borrow();
                let acked = self.acked.borrow();
                p.devices
                    .values()
                    .filter(|d| p.nodes.get(&d.node).is_some_and(|n| n.up))
                    .all(|d| acked.get(&d.id).is_some_and(|e| *e >= epoch))
            };
            if held || Instant::now() >= until {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        Msg::BlocklistOk { map_epoch: epoch }
    }

    async fn conn(&self, addr: SocketAddr) -> Result<hyperstor_rpc::Conn, hyperstor_rpc::RpcError> {
        if let Some(c) = self.conns.borrow().get(&addr)
            && !c.is_dead()
        {
            return Ok(c.clone());
        }
        let c = hyperstor_rpc::Conn::connect(addr).await?;
        self.conns.borrow_mut().insert(addr, c.clone());
        Ok(c)
    }

    /// Returns once `dev` can no longer serve `pgs` under an older map.
    async fn fence(
        self: Rc<Self>,
        node: Option<u32>,
        addr: Option<SocketAddr>,
        up: bool,
        pgs: Vec<(u32, u32)>,
        epoch: u64,
    ) {
        use hyperstor_proto::Msg;
        if up && let Some(addr) = addr {
            let asked = async {
                let c = self.conn(addr).await.ok()?;
                c.call(
                    &Msg::Fence {
                        epoch,
                        pgs: pgs.clone(),
                    },
                    &[],
                )
                .await
                .ok()
            };
            if let Ok(Some((Msg::FenceOk, _))) =
                hyperstor_reactor::time::timeout(self.lease, asked).await
            {
                return;
            }
        }
        // No answer: stop renewing the node's lease and wait out the last
        // one granted.
        let Some(node) = node else { return };
        self.fencing.borrow_mut().insert(node);
        loop {
            let last = self.granted.borrow().get(&node).copied();
            let Some(last) = last else { break };
            // Leases run from when the node sent its heartbeat, which is
            // before we granted it; a quarter more covers clock rate drift.
            let until = last + self.lease + self.lease / 4;
            let now = Instant::now();
            if now >= until {
                break;
            }
            sleep(until - now).await;
        }
        self.fencing.borrow_mut().remove(&node);
    }

    fn handle(
        &self,
        msg: Result<hyperstor_proto::Msg, hyperstor_proto::FrameError>,
    ) -> hyperstor_proto::Msg {
        use hyperstor_proto::*;
        // Followers serve the map; the rest is the leader's (D72).
        if !self.leads()
            && matches!(
                msg,
                Ok(Msg::Heartbeat { .. }
                    | Msg::PeerDown { .. }
                    | Msg::BackfillDone { .. }
                    | Msg::DeviceFailed { .. }
                    | Msg::DeviceSpace { .. }
                    | Msg::DeviceKind { .. }
                    | Msg::PgSplit { .. }
                    | Msg::LeaderState
                    | Msg::MemberSlow { .. }
                    | Msg::ScrubReport { .. }
                    | Msg::PgUsage { .. })
            )
        {
            return Msg::Error {
                code: Code::NotLeader,
                map_epoch: self.published.borrow().epoch,
                detail: self.leader_hint(),
            };
        }
        match msg {
            Ok(Msg::Hello { .. }) => Msg::HelloOk {
                version: VERSION,
                node: 0,
                map_epoch: self.published.borrow().epoch,
            },
            Ok(Msg::GetMap { have_epoch }) => {
                let m = self.published.borrow();
                Msg::Map {
                    epoch: m.epoch,
                    // Nothing new: an empty map body saves the encoding.
                    map: if have_epoch >= m.epoch {
                        Vec::new()
                    } else {
                        m.encode()
                    },
                }
            }
            // A node not heard from since this member started serving is
            // still starting: it heartbeats only once every device is
            // open (I188), and answers no pings until then. Its silence
            // isn't news; the heartbeat timeout covers one that never
            // comes up (I198).
            Ok(Msg::PeerDown { peer, .. }) if !self.beats.borrow().contains_key(&peer) => {
                Msg::PeerDownOk
            }
            Ok(Msg::PeerDown { reporter, peer }) => {
                self.reports
                    .borrow_mut()
                    .entry(peer)
                    .or_default()
                    .insert(reporter, Instant::now());
                Msg::PeerDownOk
            }
            Ok(Msg::Heartbeat { node }) => {
                let now = Instant::now();
                self.beats.borrow_mut().insert(node, now);
                let lease_ms = if self.fencing.borrow().contains(&node)
                    || self.suspect.borrow().contains(&node)
                {
                    0
                } else {
                    self.granted.borrow_mut().insert(node, now);
                    self.lease.as_millis() as u32
                };
                Msg::HeartbeatOk {
                    map_epoch: self.published.borrow().epoch,
                    lease_ms,
                }
            }
            Ok(Msg::BackfillDone {
                pool,
                pg,
                pg_epoch,
                device,
            }) => {
                // A marked PG's acting set only shrinks until it flips
                // (D183): a member that joined would hold the PG without
                // what moved to its sibling. Backfill tries again after.
                let marked = self
                    .map
                    .borrow()
                    .pools
                    .get(&pool)
                    .is_some_and(|p| p.split.as_ref().is_some_and(|s| s.marked.contains(&pg)));
                let ok = !marked
                    && self
                        .map
                        .borrow_mut()
                        .backfill_done(pool, pg, pg_epoch, device);
                let epoch = self.map.borrow().epoch;
                if ok {
                    Msg::BackfillOk { map_epoch: epoch }
                } else {
                    Msg::Error {
                        code: Code::StaleMap,
                        map_epoch: epoch,
                        detail: "PG changed since the backfill started".into(),
                    }
                }
            }
            Ok(Msg::Ping) => Msg::Pong,
            Ok(Msg::PgUsage { device, full, pgs }) => Msg::PgUsageOk {
                known: self.note_usage(device, full, &pgs),
            },
            // A device the engine can no longer write: out at once, without
            // waiting for the down-out interval (09-replication.md §15).
            Ok(Msg::DeviceFailed { device }) => {
                let mut map = self.map.borrow_mut();
                if map.fail_device(device) {
                    eprintln!("hyperstor-meta: device {device} failed; placing its PGs elsewhere");
                    map.recompute();
                }
                Msg::Noted {
                    map_epoch: map.epoch,
                }
            }
            Ok(Msg::LeaderState) => {
                let v = self.own_view(Instant::now());
                Msg::LeaderStateOk {
                    heard: v
                        .heard
                        .iter()
                        .map(|(n, d)| (*n, d.as_millis() as u64))
                        .collect(),
                    inconsistent: v
                        .inconsistent
                        .iter()
                        .map(|(&(p, g), &k)| (p, g, k))
                        .collect(),
                    flapping: v.flapping.into_iter().collect(),
                    space: v.space.iter().map(|(&d, &(t, f))| (d, t, f)).collect(),
                }
            }
            Ok(Msg::PgSplit {
                pool,
                pg,
                pg_epoch,
                device,
            }) => {
                {
                    // A report from before the PG's last change is stale;
                    // a newer one starts the count again.
                    let mut r = self.split_reports.borrow_mut();
                    let e = r.entry((pool, pg)).or_default();
                    if pg_epoch > e.0 {
                        *e = (pg_epoch, BTreeSet::new());
                    }
                    if pg_epoch == e.0 {
                        e.1.insert(device);
                    }
                }
                self.advance_splits();
                Msg::Noted {
                    map_epoch: self.published.borrow().epoch,
                }
            }
            Ok(Msg::DeviceKind { device, kind }) => {
                if let Some(k) = hyperstor_map::DeviceKind::from_code(kind)
                    && self.map.borrow().devices.contains_key(&device)
                {
                    let known = self.map.borrow().kinds.get(&device) == Some(&k);
                    if !known {
                        self.kind_reports.borrow_mut().insert(device, k);
                        self.apply_kinds();
                    }
                }
                Msg::Noted {
                    map_epoch: self.published.borrow().epoch,
                }
            }
            Ok(Msg::DeviceSpace {
                device,
                total_bytes,
                free_bytes,
            }) => {
                self.space
                    .borrow_mut()
                    .insert(device, (total_bytes, free_bytes));
                self.auto_weights();
                Msg::Noted {
                    map_epoch: self.published.borrow().epoch,
                }
            }
            // D91: only the PG's primary, at the PG's current epoch.
            Ok(Msg::MemberSlow {
                reporter,
                pool,
                pg,
                pg_epoch,
                device,
            }) => {
                let mut map = self.map.borrow_mut();
                let from_primary = map.pg(pool, pg).and_then(|i| i.primary()) == Some(reporter);
                // A primary its peers report silent may be the one cut
                // off: its reports wait until that is decided (D40).
                let node = map.devices.get(&reporter).map(|d| d.node);
                let doubted = node.is_some_and(|n| {
                    self.suspect.borrow().contains(&n)
                        || self.reports.borrow().get(&n).is_some_and(|r| !r.is_empty())
                });
                if from_primary && !doubted && map.drop_member(pool, pg, pg_epoch, device) {
                    eprintln!(
                        "hyperstor-meta: device {device} too slow for PG {pool}.{pg}; backfilling it"
                    );
                }
                Msg::Noted {
                    map_epoch: map.epoch,
                }
            }
            Ok(Msg::ScrubReport {
                pool,
                pg,
                unresolved,
            }) => {
                let mut inc = self.inconsistent.borrow_mut();
                if unresolved == 0 {
                    inc.remove(&(pool, pg));
                } else {
                    inc.insert((pool, pg), unresolved);
                }
                Msg::Noted {
                    map_epoch: self.map.borrow().epoch,
                }
            }
            Ok(Msg::Blocklist { .. }) => Msg::Error {
                code: Code::Invalid,
                map_epoch: self.map.borrow().epoch,
                detail: "blocklist requests are answered by `Meta::blocklist`".into(),
            },
            _ => Msg::Error {
                code: Code::Invalid,
                map_epoch: self.map.borrow().epoch,
                detail: "unsupported request".into(),
            },
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Serves the map service on `listener` until the reactor stops.
pub async fn serve(listener: hyperstor_reactor::net::TcpListener, meta: Rc<Meta>) {
    use hyperstor_proto::*;
    use hyperstor_reactor::spawn;
    loop {
        let Ok((s, _)) = listener.accept().await else {
            continue;
        };
        let meta = meta.clone();
        spawn(async move {
            let s = Rc::new(s);
            let (tx, rx) = hyperstor_reactor::chan::channel::<Vec<u8>>();
            spawn(hyperstor_rpc::writer(s.clone(), rx));
            loop {
                let (r, hb) = s.read_exact(vec![0; HEADER_LEN]).await;
                if r.is_err() {
                    return;
                }
                let Ok(h) = Header::decode(hb[..].try_into().unwrap()) else {
                    return;
                };
                let (r, body) = s
                    .read_exact(vec![0; (h.body_len + h.payload_len) as usize])
                    .await;
                if r.is_err() {
                    return;
                }
                let msg = Msg::decode(h.kind, &body[..h.body_len as usize]);
                // Answered later, from a task of its own: it waits for the
                // map to reach every node.
                if let Ok(Msg::Blocklist { ids }) = msg {
                    let (meta, tx) = (meta.clone(), tx.clone());
                    spawn(async move {
                        let reply = meta.blocklist(ids).await;
                        let _ = tx.send(encode_frame(h.req, &reply, &[]));
                    });
                    continue;
                }
                let reply = meta.handle(msg);
                if tx.send(encode_frame(h.req, &reply, &[])).is_err() {
                    return;
                }
            }
        });
    }
}

/// Checks a pool's class set (24-pools-and-devices.md §27.3, §27.4;
/// D172): its devices that are in must span at least as many failure
/// domains as `pool` keeps copies, and its classes may hold hard disks and
/// flash both only when forced or for a cache pool. Returns whether they
/// mix.
/// A pool's name is 1 to 64 characters, without spaces, and doesn't
/// start with a dot. Why: names with a dot are the system's (`.meta`).
pub(crate) fn check_pool_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || name.contains(char::is_whitespace)
    {
        return Err(format!(
            "{name:?}: a pool's name is 1 to 64 characters, without spaces, not starting with a dot"
        ));
    }
    Ok(())
}

fn check_class_set(map: &ClusterMap, pool: &PoolInfo, force: bool) -> Result<bool, String> {
    use hyperstor_placement::Domain;
    let what = || {
        let mut w = if pool.classes.is_empty() {
            "any".to_string()
        } else {
            pool.classes.join(",")
        };
        for d in &pool.added {
            w.push_str(&format!(" +{d}"));
        }
        for d in &pool.removed {
            w.push_str(&format!(" -{d}"));
        }
        w
    };
    let mixed = mixes_disks_and_flash(map, pool);
    if mixed && !force && pool.kind != PoolKind::Cache {
        return Err(format!(
            "devices {} hold hard disks and flash: a write would wait for the slowest copy; --force allows it",
            what()
        ));
    }
    let domains = domains(map, pool);
    if domains < pool.size as usize {
        return Err(format!(
            "the devices of classes {} span {} {}(s); pool {} keeps {} copies",
            what(),
            domains,
            match pool.domain {
                Domain::Device => "device",
                Domain::Host => "host",
                Domain::Rack => "rack",
            },
            pool.id,
            pool.size
        ));
    }
    Ok(mixed)
}

/// How many failure domains, at `pool`'s level, its devices in placement
/// span: at most this many copies of a PG can be placed.
fn domains(map: &ClusterMap, pool: &PoolInfo) -> usize {
    use hyperstor_placement::Domain;
    map.devices
        .values()
        .filter(|d| d.up_in && pool.uses(d))
        .map(|d| match pool.domain {
            Domain::Device => d.id,
            Domain::Host => d.node,
            Domain::Rack => map.nodes.get(&d.node).map_or(0, |n| n.rack),
        })
        .collect::<BTreeSet<u32>>()
        .len()
}

/// The PG count a new pool gets when none is given (24-pools-and-devices.md
/// §27.5): about 100 PGs per device it may use, counting each copy, as a
/// power of two, at least 8.
pub fn default_pg_count(devices: usize, copies: u8) -> u32 {
    let want = (devices * 100 / (copies.max(1) as usize)).max(8) as u32;
    want.next_power_of_two().min(1 << 16)
}

/// Whether `classes` hold hard disks and flash both (D172, D179): `file`
/// mixes with any kind, and a class whose devices haven't reported a kind
/// counts as neither.
pub fn mixes_disks_and_flash(map: &ClusterMap, pool: &PoolInfo) -> bool {
    use hyperstor_map::DeviceKind;
    // A pool without classes uses every device, as pools made before class
    // sets do (D171): the rule is about the sets operators choose.
    if pool.classes.is_empty() {
        return false;
    }
    // The kinds of the devices it uses, and of a class named after a kind
    // that has no device yet.
    let mut kinds: Vec<DeviceKind> = map
        .devices
        .values()
        .filter(|d| pool.uses(d))
        .filter_map(|d| map.kinds.get(&d.id).copied())
        .collect();
    kinds.extend(
        pool.classes
            .iter()
            .filter(|c| map.class_kind(c, None).is_none())
            .filter_map(|c| DeviceKind::from_name(c)),
    );
    kinds.contains(&DeviceKind::Hdd)
        && kinds
            .iter()
            .any(|k| matches!(k, DeviceKind::Nvme | DeviceKind::Ssd))
}

/// A device class: 1 to 32 lowercase letters, digits and dashes
/// (`nvme`, `nvme-qlc`), as labels are (24-pools-and-devices.md §27.2.4).
pub fn valid_class(c: &str) -> bool {
    (1..=32).contains(&c.len())
        && c.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperstor_placement::Domain;

    /// Hosts that join (19-administration.md §23.4.2) are added down, get the
    /// next node number, grow `.meta`, and get their node back when they
    /// join again with the same addresses.
    #[test]
    fn joined_hosts_are_added_to_an_empty_map() {
        let m = parse_cluster("cluster 3\npool 1 vol replicated size 3 min 2 pgs 16 domain host\n")
            .unwrap();
        assert!(m.nodes.is_empty());
        let meta = Meta::new(m);
        let addr = |p: u16| -> SocketAddr { format!("127.0.0.1:{p}").parse().unwrap() };
        assert_eq!(meta.add_node(0, &[addr(9000), addr(9001)]), Ok(0));
        assert_eq!(meta.add_node(1, &[addr(9100)]), Ok(1));
        assert_eq!(meta.add_node(0, &[addr(9200)]), Ok(2));
        let map = meta.map.borrow().clone();
        assert_eq!(map.devices.len(), 4);
        assert_eq!(
            (map.nodes[&0].up, map.nodes[&1].rack),
            (false, 1),
            "down until it beats"
        );
        assert_eq!(map.pools[&META_POOL].size, 3, ".meta grows to three copies");
        assert_eq!(map.pools[&META_POOL].min_size, 2);
        // Installed again: the same node; other addresses: refused.
        assert_eq!(meta.add_node(0, &[addr(9100)]), Ok(1));
        assert!(meta.add_node(0, &[addr(9300), addr(9100)]).is_err());
        assert!(meta.add_node(0, &[addr(9100), addr(9000)]).is_err());
        assert!(meta.add_node(0, &[]).is_err());
        let epoch = meta.map.borrow().epoch;
        assert_eq!(meta.add_node(0, &[addr(9100)]), Ok(1));
        assert_eq!(meta.map.borrow().epoch, epoch, "nothing changed");
        // Installed again with a device more (D167): appended, at weight 0.
        assert_eq!(meta.add_node(0, &[addr(9100), addr(9300)]), Ok(1));
        let map = meta.map.borrow().clone();
        let d = &map.devices[&1001];
        assert_eq!((d.addr, d.shard, d.weight), (addr(9300), 1, 0.0));
        assert_eq!(map.nodes[&1].reactors, 2);
    }

    /// D181: a deleted pool leaves the map; `.meta` and a pool that isn't
    /// there are refused; a new pool doesn't get its id.
    #[test]
    fn a_pool_is_deleted_and_its_id_not_given_again() {
        let meta = Meta::new(map());
        assert!(meta.delete_pool(0).unwrap_err().contains(".meta"));
        assert!(meta.delete_pool(9).unwrap_err().contains("no pool 9"));
        let top = *meta.map.borrow().pools.keys().next_back().unwrap();
        meta.delete_pool(top).unwrap();
        assert!(meta.delete_pool(top).unwrap_err().contains("no pool"));
        let m = meta.map.borrow().clone();
        assert!(!m.pools.contains_key(&top) && m.deleted_pools.contains(&top));
        assert_eq!(m.next_pool_id(), top + 1);
    }

    /// §27.3.2: a pool is renamed, unless another has the name or it
    /// starts with a dot; its failure domain changes when its devices
    /// span enough of the new one, and its PGs follow it.
    #[test]
    fn a_pool_is_renamed_and_its_domain_changed() {
        use hyperstor_placement::Domain;
        let meta = Meta::new(map());
        let change = |name: Option<&str>, domain: Option<Domain>| {
            meta.set_pool(
                1,
                PoolChange {
                    name: name.map(str::to_string),
                    domain,
                    ..Default::default()
                },
                false,
            )
        };
        assert!(
            change(None, None)
                .unwrap_err()
                .contains("nothing to change")
        );
        assert!(
            change(Some("ec"), None)
                .unwrap_err()
                .contains("already named ec")
        );
        assert!(
            change(Some(".x"), None)
                .unwrap_err()
                .contains("not starting with a dot")
        );
        let before = meta.map.borrow().epoch;
        let (epoch, _) = change(Some("blocks"), None).unwrap();
        assert!(epoch > before);
        assert_eq!(meta.map.borrow().pools[&1].name, "blocks");
        // Renaming to its own name changes nothing.
        assert_eq!(change(Some("blocks"), None).unwrap().0, epoch);
        // Every node is in rack 0: one rack for 3 copies.
        assert!(
            change(None, Some(Domain::Rack))
                .unwrap_err()
                .contains("span 1 rack(s)")
        );
        change(None, Some(Domain::Device)).unwrap();
        let host_of = |d: u32| d / 1000;
        let shared = |m: &ClusterMap| {
            m.pgs.iter().any(|(k, p)| {
                k.0 == 1 && {
                    let hosts: BTreeSet<u32> = p.up.iter().map(|&d| host_of(d)).collect();
                    hosts.len() < p.up.len()
                }
            })
        };
        assert_eq!(meta.map.borrow().pools[&1].domain, Domain::Device);
        // Back to a host per copy: no PG has two copies on one host.
        change(None, Some(Domain::Host)).unwrap();
        assert!(!shared(&meta.map.borrow()));
        assert!(
            meta.set_pool(
                0,
                PoolChange {
                    name: Some("m".into()),
                    ..Default::default()
                },
                false
            )
            .unwrap_err()
            .contains(".meta")
        );
    }

    /// D180: a device outside a pool's classes is added to it, one of
    /// them removed and taken back; each checked as a class set is (too
    /// few hosts, hard disks with flash unless forced); `.meta` refused.
    #[test]
    fn devices_are_added_to_and_removed_from_a_pool() {
        use hyperstor_map::DeviceKind;
        let meta = Meta::new(map());
        {
            let mut m = meta.map.borrow_mut();
            for n in 0..4u32 {
                m.set_kind(n * 1000, DeviceKind::Nvme);
                m.set_kind(n * 1000 + 1, DeviceKind::Hdd);
            }
            m.recompute();
        }
        meta.set_pool_classes(1, vec!["nvme".into()], false)
            .unwrap();
        let uses = |d: u32| {
            meta.map
                .borrow()
                .pgs
                .iter()
                .any(|(k, p)| k.0 == 1 && p.up.contains(&d))
        };
        assert!(
            meta.set_pool_device(0, 0, false, false)
                .unwrap_err()
                .contains(".meta")
        );
        assert!(
            meta.set_pool_device(1, 99, true, false)
                .unwrap_err()
                .contains("no device 99")
        );
        // An NVMe device out: 3 hosts left for 3 copies.
        meta.set_pool_device(1, 3000, false, false).unwrap();
        assert!(!uses(3000));
        assert_eq!(
            meta.map.borrow().pools[&1]
                .removed
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [3000]
        );
        assert!(
            meta.set_pool_device(1, 2000, false, false)
                .unwrap_err()
                .contains("span 2 host(s)")
        );
        // A hard disk in: refused unless forced.
        assert!(
            meta.set_pool_device(1, 1, true, false)
                .unwrap_err()
                .contains("--force")
        );
        assert_eq!(
            meta.set_pool_device(1, 1, true, true).map(|r| r.1),
            Ok(true)
        );
        assert!(uses(1) || meta.map.borrow().pools[&1].added.contains(&1));
        // Back as they were.
        meta.set_pool_device(1, 1, false, false).unwrap();
        meta.set_pool_device(1, 3000, true, false).unwrap();
        let m = meta.map.borrow().clone();
        assert!(m.pools[&1].added.is_empty() && m.pools[&1].removed.is_empty());
        drop(m);
        assert!(uses(3000));
    }

    /// D163, D171: a pool is made with its classes, checked as `SetPool`
    /// checks them, the next id and, unless given, about 100 PGs per
    /// device per copy; its PGs are placed on its classes only.
    #[test]
    fn pools_are_made_with_their_classes() {
        use hyperstor_map::DeviceKind;
        let meta = Meta::new(map());
        {
            let mut m = meta.map.borrow_mut();
            for n in 0..4u32 {
                m.set_kind(n * 1000, DeviceKind::Nvme);
                m.set_kind(n * 1000 + 1, DeviceKind::Hdd);
            }
            m.recompute();
        }
        let new = |name: &str, classes: &[&str]| NewPool {
            name: name.into(),
            kind: PoolKind::Replicated,
            size: 3,
            min_size: 2,
            pg_count: None,
            chunk_size: 4 << 20,
            domain: hyperstor_placement::Domain::Host,
            classes: classes.iter().map(|c| c.to_string()).collect(),
            force: false,
        };
        assert!(
            meta.create_pool(new("fast", &[]))
                .unwrap_err()
                .contains("names its device classes")
        );
        assert!(
            meta.create_pool(new("rbd", &["nvme"]))
                .unwrap_err()
                .contains("already named rbd")
        );
        assert!(meta.create_pool(new(".x", &["nvme"])).is_err());
        assert!(
            meta.create_pool(NewPool {
                min_size: 4,
                ..new("m", &["nvme"])
            })
            .is_err()
        );
        assert!(
            meta.create_pool(NewPool {
                pg_count: Some(100),
                ..new("p", &["nvme"])
            })
            .is_err()
        );
        assert!(
            meta.create_pool(new("bulk", &["nvme", "hdd"]))
                .unwrap_err()
                .contains("--force")
        );
        assert!(
            meta.create_pool(new("none", &["archive"]))
                .unwrap_err()
                .contains("span 0")
        );
        let next = meta.map.borrow().next_pool_id();
        let (id, pgs, _, mixed) = meta.create_pool(new("fast", &["nvme"])).unwrap();
        // 4 NVMe devices, 3 copies: 133 PGs wanted, so 256.
        assert_eq!((id, pgs, mixed), (next, 256, false));
        assert_eq!(default_pg_count(6, 3), 256, "the design's example");
        let m = meta.map.borrow().clone();
        assert_eq!(m.pools[&id].classes, ["nvme"]);
        let on = m.pgs.iter().filter(|(k, _)| k.0 == id);
        assert_eq!(on.clone().count(), 256);
        assert!(
            on.into_iter()
                .all(|(_, p)| p.up.len() == 3 && p.up.iter().all(|d| d % 1000 == 0))
        );
        let (id2, _, _, mixed) = meta
            .create_pool(NewPool {
                force: true,
                ..new("bulk", &["nvme", "hdd"])
            })
            .unwrap();
        assert!(mixed && id2 == id + 1);
    }

    /// D164, D172: a pool's class set is refused for `.meta`, when its
    /// devices span fewer failure domains than the pool's copies, and when
    /// it holds hard disks and flash both unless forced (or a cache pool);
    /// PGs then place only on its classes.
    #[test]
    fn pool_class_sets_are_checked_and_placed_on() {
        use hyperstor_map::DeviceKind;
        let meta = Meta::new(map());
        {
            let mut m = meta.map.borrow_mut();
            // Device 0 of each of the 4 nodes NVMe, device 1 a hard disk.
            for n in 0..4u32 {
                m.set_kind(n * 1000, DeviceKind::Nvme);
                m.set_kind(n * 1000 + 1, DeviceKind::Hdd);
            }
            m.recompute();
        }
        let set = |p: u32, c: &[&str], force: bool| {
            meta.set_pool_classes(p, c.iter().map(|c| c.to_string()).collect(), force)
        };
        assert!(set(0, &["nvme"], false).unwrap_err().contains(".meta"));
        assert!(set(9, &["nvme"], false).unwrap_err().contains("no pool 9"));
        // Only one class spans 4 hosts, enough for 3 copies.
        let (_, mixed) = set(1, &["nvme"], false).unwrap();
        assert!(!mixed);
        let m = meta.map.borrow().clone();
        assert!(
            m.pgs
                .iter()
                .filter(|(k, _)| k.0 == 1)
                .all(|(_, p)| p.up.iter().all(|d| d % 1000 == 0))
        );
        // A class of no device spans nothing.
        assert!(
            set(1, &["archive"], false)
                .unwrap_err()
                .contains("span 0 host(s)")
        );
        // Hard disks and flash: refused, forced, then marked mixed.
        assert!(
            set(1, &["nvme", "hdd"], false)
                .unwrap_err()
                .contains("--force")
        );
        assert_eq!(set(1, &["nvme", "hdd"], true).map(|r| r.1), Ok(true));
        assert_eq!(meta.map.borrow().pools[&1].classes, ["hdd", "nvme"]);
        // Every device again.
        assert_eq!(set(1, &[], false).map(|r| r.1), Ok(false));
        assert!(meta.map.borrow().pools[&1].classes.is_empty());
    }

    /// D178: kinds wait in memory until every device of a node that is up
    /// has reported (a node from before kinds couldn't read the map);
    /// then each device without an operator's class takes its kind, and
    /// an operator can set a class that fits, or go back to the kind.
    #[test]
    fn kinds_are_applied_once_every_live_device_has_reported() {
        use hyperstor_map::DeviceKind;
        use hyperstor_proto::Msg;
        let meta = Meta::new(map());
        let devices: Vec<u32> = meta.map.borrow().devices.keys().copied().collect();
        let report = |d: u32, k: DeviceKind| {
            meta.handle(Ok(Msg::DeviceKind {
                device: d,
                kind: k.code(),
            }))
        };
        let kind = |d: u32| {
            if d.is_multiple_of(1000) {
                DeviceKind::Nvme
            } else {
                DeviceKind::File
            }
        };
        for &d in &devices[1..] {
            report(d, kind(d));
        }
        assert!(meta.map.borrow().kinds.is_empty(), "one hasn't reported");
        // A device of a node that is down doesn't hold the others back.
        let first = devices[0];
        meta.map
            .borrow_mut()
            .nodes
            .get_mut(&(first / 1000))
            .unwrap()
            .up = false;
        report(devices[1], kind(devices[1]));
        assert_eq!(
            meta.map.borrow().kinds.len(),
            devices.len() - 1,
            "applied without the down node's device"
        );
        meta.map
            .borrow_mut()
            .nodes
            .get_mut(&(first / 1000))
            .unwrap()
            .up = true;
        report(first, kind(first));
        let m = meta.map.borrow().clone();
        assert_eq!(m.kinds.len(), devices.len());
        assert!(m.devices.values().all(|d| d.class == kind(d.id).name()));
        // An operator's class, which must fit the kind.
        assert!(meta.set_device_class(1, Some("nvme")).is_err());
        assert_eq!(
            meta.set_device_class(1, Some("scratch")).map(|r| r.1),
            Ok("scratch".into())
        );
        report(1, DeviceKind::File);
        assert_eq!(meta.map.borrow().devices[&1].class, "scratch", "kept");
        assert_eq!(
            meta.set_device_class(1, None).map(|r| r.1),
            Ok("file".into())
        );
        assert!(meta.set_device_class(99, None).is_err());
    }

    /// A device appended to a running node (D167) starts at weight 0, so
    /// nothing moves until it reports; addresses in use, unknown nodes and
    /// bad classes are refused.
    #[test]
    fn a_device_is_added_to_a_node_and_weighted_once_it_reports() {
        let meta = Meta::new(map());
        let addr: SocketAddr = "127.0.0.1:7901".parse().unwrap();
        let before = meta.map.borrow().clone();
        let (id, epoch) = meta.add_device(1, addr, None).unwrap();
        let map = meta.map.borrow().clone();
        assert_eq!(epoch, map.epoch);
        let d = &map.devices[&id];
        assert_eq!(
            (d.node, d.addr, d.weight, d.class.as_str()),
            (1, addr, 0.0, "nvme")
        );
        assert!(
            map.pgs.iter().all(|(k, p)| p.up == before.pgs[k].up),
            "nothing moves before it reports"
        );
        assert!(
            meta.add_device(1, addr, None)
                .unwrap_err()
                .contains("already device")
        );
        assert!(
            meta.add_device(99, "127.0.0.1:7902".parse().unwrap(), None)
                .unwrap_err()
                .contains("no node 99")
        );
        assert!(
            meta.add_device(1, "127.0.0.1:7902".parse().unwrap(), Some("NVMe!"))
                .is_err()
        );
        // Its first report weights it by capacity; PGs move onto it.
        meta.handle(Ok(hyperstor_proto::Msg::DeviceSpace {
            device: id,
            total_bytes: 1 << 40,
            free_bytes: 1 << 40,
        }));
        let map = meta.map.borrow().clone();
        assert_eq!(map.devices[&id].weight, 1.0);
        assert!(map.pgs.values().any(|p| p.up.contains(&id)));
    }

    /// A drain's dry run changes nothing and says what a drain moves; the
    /// drain takes every device of the node out.
    /// §27.5, D183: a split marks clean PGs, at most `SPLITS_AT_ONCE`;
    /// a marked PG flips once every member of its acting set has reported
    /// it split at its epoch (a report at an older epoch doesn't count),
    /// and refuses a member joining by backfill until then; the last flip
    /// doubles the count.
    #[test]
    fn splits_flip_pgs_once_every_member_reports() {
        use hyperstor_proto::Msg;
        let meta = Meta::new(map());
        let n = meta.map.borrow().pools[&1].pg_count;
        let change = |pg_count| PoolChange {
            pg_count: Some(pg_count),
            ..Default::default()
        };
        assert!(
            meta.set_pool(1, change(n + 1), false)
                .unwrap_err()
                .contains("doubles")
        );
        meta.set_pool(1, change(n * 2), false).unwrap();
        assert!(
            meta.set_pool(1, change(n * 2), false)
                .unwrap_err()
                .contains("already")
        );
        let marked = || {
            meta.map.borrow().pools[&1]
                .split
                .as_ref()
                .map(|s| s.marked.clone())
        };
        let first = marked().unwrap();
        assert_eq!(first.len(), SPLITS_AT_ONCE);
        let report = |pg: u32, device: u32, pg_epoch: u64| {
            meta.handle(Ok(Msg::PgSplit {
                pool: 1,
                pg,
                pg_epoch,
                device,
            }))
        };
        let pg = *first.iter().next().unwrap();
        let info = meta.map.borrow().pgs[&(1, pg)].clone();
        // Backfill to it is refused while it is marked.
        assert!(matches!(
            meta.handle(Ok(Msg::BackfillDone {
                pool: 1,
                pg,
                pg_epoch: info.epoch,
                device: 9999,
            })),
            Msg::Error { .. }
        ));
        for (i, d) in info.acting.iter().enumerate() {
            assert!(
                marked().unwrap().contains(&pg),
                "flipped before every member"
            );
            // A report at an older epoch first: not counted.
            report(pg, *d, info.epoch - 1);
            if i + 1 < info.acting.len() {
                report(pg, *d, info.epoch);
            }
        }
        assert!(marked().unwrap().contains(&pg));
        report(pg, *info.acting.last().unwrap(), info.epoch);
        let m = meta.map.borrow().clone();
        let s = m.pools[&1].split.as_ref().unwrap();
        assert!(s.done.contains(&pg) && !s.marked.contains(&pg));
        assert_eq!(s.marked.len(), SPLITS_AT_ONCE, "the next one marked");
        assert_eq!(m.pgs[&(1, pg + n)].acting, info.acting);
        // Every other PG, by every member.
        while let Some(marked) = marked() {
            for pg in marked {
                let i = meta.map.borrow().pgs[&(1, pg)].clone();
                for d in &i.acting {
                    report(pg, *d, i.epoch);
                }
            }
        }
        let p = meta.map.borrow().pools[&1].clone();
        assert_eq!((p.pg_count, p.placement, p.split), (n * 2, n, None));
        // The new PGs move a few at a time (D173), only while the pool's
        // PGs are all clean; no split until they have.
        assert!(
            meta.set_pool(1, change(n * 4), false)
                .unwrap_err()
                .contains("still moving")
        );
        let placement = || meta.map.borrow().pools[&1].placement;
        let settle = || {
            for (k, pg) in meta.map.borrow_mut().pgs.iter_mut() {
                if k.0 == 1 {
                    pg.acting = pg.up.clone();
                }
            }
        };
        settle();
        meta.advance_splits();
        assert_eq!(placement(), n + SPLITS_AT_ONCE as u32);
        // Unclean: wait.
        {
            let mut m = meta.map.borrow_mut();
            let pg = m.pgs.get_mut(&(1, 0)).unwrap();
            pg.acting.rotate_left(1);
        }
        meta.advance_splits();
        assert_eq!(placement(), n + SPLITS_AT_ONCE as u32);
        while placement() < n * 2 {
            settle();
            meta.advance_splits();
        }
        meta.set_pool(1, change(n * 4), false).unwrap();
    }

    /// §27.2.5, D175, D182: a device is taken out to be removed unless a
    /// device taking its data would pass [`FULL`], or a pool would keep
    /// fewer copies than its `min` (or, unforced, its `size`); it leaves
    /// the map once no PG lists it, its node with its last device, and
    /// neither id is given again.
    #[test]
    fn devices_are_removed_once_no_pg_lists_them() {
        let mut m = map();
        m.pools.get_mut(&2).unwrap().min_size = 2;
        let meta = Meta::new(m);
        assert!(
            meta.remove_device(9, false, true)
                .unwrap_err()
                .contains("no device 9")
        );
        // Every device 94% full, and every PG of pool 1 holding 20 bytes:
        // the devices that take device 3001's PGs would pass 95%.
        for d in meta.map.borrow().devices.keys() {
            meta.space.borrow_mut().insert(*d, (1000, 60));
        }
        let pgs: Vec<(u32, u32, u64)> = (0..64).map(|pg| (1, pg, 20)).collect();
        meta.note_usage(0, false, &pgs);
        let before = meta.map.borrow().clone();
        let e = meta.remove_device(3001, true, false).unwrap_err();
        assert!(e.contains("% full with device 3001's data"), "{e}");
        assert_eq!(*meta.map.borrow(), before, "refused, even forced");
        for d in meta.map.borrow().devices.keys() {
            meta.space.borrow_mut().insert(*d, (1000, 500));
        }
        let dry = meta.remove_device(3001, false, true).unwrap();
        assert_eq!(*meta.map.borrow(), before, "a dry run changes nothing");
        assert!(dry.short.is_empty() && !dry.moves.is_empty());
        assert!(
            dry.space
                .iter()
                .all(|&(_, used, total)| used >= 520 && total == 1000)
        );
        assert_eq!(meta.remove_device(3001, false, false).unwrap(), dry);
        let after = meta.map.borrow().clone();
        assert!(!after.devices[&3001].up_in);
        assert!(after.pgs.values().all(|pg| !pg.up.contains(&3001)));
        // Not while a PG still lists it, nor once it is back in.
        if after.pgs.values().any(|pg| pg.acting.contains(&3001)) {
            assert!(
                meta.finish_removal(3001)
                    .unwrap_err()
                    .contains("still lists")
            );
        }
        let settle = || {
            for pg in meta.map.borrow_mut().pgs.values_mut() {
                pg.acting = pg.up.clone();
            }
        };
        settle();
        meta.set_device_in(3001, true).unwrap();
        assert!(meta.finish_removal(3001).unwrap_err().contains("back in"));
        meta.set_device_in(3001, false).unwrap();
        settle();
        assert_eq!(meta.finish_removal(3001).unwrap().map(|r| r.0), Some(false));
        assert_eq!(meta.finish_removal(3001).unwrap(), None, "removed already");
        assert!(meta.map.borrow().removed_devices.contains(&3001));
        // With node 2 out, node 3's last device spans the third host of
        // pools that keep 3 copies.
        meta.drain(2, false).unwrap();
        settle();
        let e = meta.remove_device(3000, false, true).unwrap_err();
        assert!(e.contains("would keep 2 of its 3 copies; --force"), "{e}");
        let forced = meta.remove_device(3000, true, false).unwrap();
        assert_eq!(forced.short, vec![(1, 2, 3), (2, 2, 3)]);
        // Nothing is backfilled to a PG left short: device 3000 leaves its
        // acting set at once, or it never would.
        assert!(
            meta.map
                .borrow()
                .pgs
                .values()
                .all(|p| !p.acting.contains(&3000))
        );
        assert_eq!(meta.finish_removal(3000).unwrap().map(|r| r.0), Some(true));
        let m = meta.map.borrow().clone();
        assert!(!m.nodes.contains_key(&3) && m.removed_nodes.contains(&3));
        // Below a pool's min, not even forced: with device 1001 out, node
        // 1 goes with device 1000.
        meta.set_device_in(1001, false).unwrap();
        let e = meta.remove_device(1000, true, true).unwrap_err();
        assert!(e.contains("fewer than its min of 2"), "{e}");
        let a: SocketAddr = "127.0.0.1:7999".parse().unwrap();
        assert_eq!(
            meta.add_node(0, &[a]).unwrap(),
            4,
            "node 3's id isn't given again"
        );
    }

    #[test]
    fn drains_move_every_pg_off_the_node() {
        let meta = Meta::new(map());
        let before = meta.map.borrow().clone();
        let dry = meta.drain(3, true).unwrap();
        assert!(!dry.is_empty());
        assert_eq!(*meta.map.borrow(), before, "a dry run changes nothing");
        // Every PG with a device of node 3 in its acting set moves, and
        // none of them goes back to it.
        let on3 = |v: &[u32]| v.iter().any(|d| *d / 1000 == 3);
        for (k, pg) in &before.pgs {
            assert_eq!(
                on3(&pg.acting),
                dry.iter().any(|m| (m.pool, m.pg) == *k),
                "{k:?}"
            );
        }
        assert!(dry.iter().all(|m| !on3(&m.after)));
        let moves = meta.drain(3, false).unwrap();
        assert_eq!(moves, dry, "the drain does what the dry run said");
        let after = meta.map.borrow().clone();
        assert!(
            after
                .devices
                .values()
                .filter(|d| d.node == 3)
                .all(|d| !d.up_in)
        );
        assert!(after.pgs.values().all(|pg| !on3(&pg.up)));
        assert!(meta.drain(9, true).is_err(), "no node 9");
    }

    fn map() -> ClusterMap {
        let mut m = ClusterMap::new(7);
        for n in 0..4 {
            let addrs: Vec<SocketAddr> = (0..2)
                .map(|i| format!("127.0.0.1:{}", 7000 + n * 10 + i).parse().unwrap())
                .collect();
            m.add_node(n, &addrs, 0);
        }
        m.add_pool(replicated_pool(1, "rbd", 3, 64));
        m.add_pool(PoolInfo {
            id: 2,
            name: "ec".into(),
            kind: PoolKind::Ec { k: 2, m: 1 },
            size: 3,
            min_size: 3,
            pg_count: 32,
            domain: Domain::Host,
            chunk_size: 4 << 20,
            classes: Vec::new(),
            added: Default::default(),
            removed: Default::default(),
            placement: 32,
            split: None,
        });
        m.recompute();
        m
    }

    #[test]
    fn losers_are_devices_that_lose_a_role() {
        let mut m = map();
        let before = m.clone();
        m.nodes.get_mut(&1).unwrap().up = false;
        m.recompute();
        let losers = Meta::losers(&before, &m);
        for (dev, pgs) in &losers {
            for k in pgs {
                let (o, n) = (&before.pgs[k], &m.pgs[k]);
                let removed = !n.acting.contains(dev);
                let lost_primary = o.primary() == Some(*dev) && n.primary() != Some(*dev);
                let moved_shard = k.0 == 2
                    && o.acting.iter().position(|d| d == dev)
                        != n.acting.iter().position(|d| d == dev);
                assert!(removed || lost_primary || moved_shard, "{dev} {k:?}");
            }
        }
        // Node 1's devices lost their roles everywhere they had one.
        for (k, o) in &before.pgs {
            for d in o.acting.iter().filter(|d| m.devices[d].node == 1) {
                assert!(losers.get(d).is_some_and(|v| v.contains(k)), "{d} {k:?}");
            }
        }
        // Survivors keep their positions and primacy: nothing else is fenced.
        assert!(losers.keys().all(|d| m.devices[d].node == 1));
        assert!(Meta::losers(&m, &m).is_empty());
    }

    /// D155: primaries' reports, a full one dropping what its sender no
    /// longer leads, and a version that moves only with a change.
    #[test]
    fn usage_reports_follow_the_primaries() {
        use hyperstor_proto::Msg;
        let m = Meta::with_lease(map(), Duration::from_millis(400));
        let pool = *m.published.borrow().pools.keys().next().unwrap();
        let n = m.published.borrow().pools[&pool].pg_count as usize;
        let (v0, Some(none)) = m.pool_usage(pool, 0).unwrap() else {
            panic!("bytes")
        };
        assert!(none.len() == n && none.iter().all(Option::is_none));
        let report = |device, full, pgs: Vec<(u32, u32, u64)>| match m.handle(Ok(Msg::PgUsage {
            device,
            full,
            pgs,
        })) {
            Msg::PgUsageOk { known } => known,
            other => panic!("{other:?}"),
        };
        assert_eq!(report(7, true, vec![(pool, 0, 4096), (pool, 1, 0)]), 2);
        assert_eq!(report(8, false, vec![(pool, 2, 8192)]), 1);
        let (v1, Some(b)) = m.pool_usage(pool, v0).unwrap() else {
            panic!("bytes")
        };
        assert!(v1 > v0);
        assert_eq!(b[..3], [Some(4096), Some(0), Some(8192)]);
        // The same numbers again change nothing.
        assert_eq!(report(8, false, vec![(pool, 2, 8192)]), 1);
        assert_eq!(m.pool_usage(pool, v1).unwrap(), (v1, None));
        // Device 7 is no longer PG 0's primary.
        assert_eq!(report(7, true, vec![(pool, 1, 0)]), 1);
        let (_, Some(b)) = m.pool_usage(pool, v1).unwrap() else {
            panic!("bytes")
        };
        assert_eq!(b[..3], [None, Some(0), Some(8192)]);
        assert!(m.pool_usage(pool + 100, 0).is_err());
    }

    /// I198: peers' reports about a node not heard from yet (still
    /// opening its devices) don't mark it down; once it has heartbeated,
    /// they do.
    #[test]
    fn reports_about_a_starting_node_wait_for_its_first_heartbeat() {
        use hyperstor_proto::Msg;
        let m = Meta::with_lease(map(), Duration::from_millis(400));
        for n in [0, 1] {
            m.handle(Ok(Msg::Heartbeat { node: n }));
        }
        let report = |reporter| {
            m.handle(Ok(Msg::PeerDown { reporter, peer: 3 }));
        };
        let timeout = Duration::from_secs(3);
        report(0);
        report(1);
        assert!(!m.liveness_tick(timeout, Instant::now()));
        assert!(m.map.borrow().nodes[&3].up, "still starting");
        m.handle(Ok(Msg::Heartbeat { node: 3 }));
        report(0);
        report(1);
        assert!(m.liveness_tick(timeout, Instant::now()));
        assert!(!m.map.borrow().nodes[&3].up, "silent once it had started");
    }

    #[test]
    fn two_peer_reports_mark_a_node_down_and_stop_its_lease() {
        use hyperstor_proto::Msg;
        let m = Meta::with_lease(map(), Duration::from_millis(400));
        let beat = |n: u32| match m.handle(Ok(Msg::Heartbeat { node: n })) {
            Msg::HeartbeatOk { lease_ms, .. } => lease_ms,
            other => panic!("{other:?}"),
        };
        for n in 0..4 {
            assert_eq!(beat(n), 400);
        }
        let timeout = Duration::from_secs(3);
        assert!(!m.liveness_tick(timeout, Instant::now()));
        // One reporter is not enough.
        m.handle(Ok(Msg::PeerDown {
            reporter: 0,
            peer: 3,
        }));
        assert!(!m.liveness_tick(timeout, Instant::now()));
        assert!(m.map.borrow().nodes[&3].up);
        // Two are, even though node 3's heartbeats still arrive.
        m.handle(Ok(Msg::PeerDown {
            reporter: 1,
            peer: 3,
        }));
        assert!(m.liveness_tick(timeout, Instant::now()));
        assert!(!m.map.borrow().nodes[&3].up);
        assert_eq!(beat(3), 0, "no lease while reported");
        assert_eq!(beat(0), 400);
        // Reports go stale; heartbeats bring it back.
        let later = Instant::now() + timeout;
        beat(0);
        beat(1);
        beat(2);
        m.beats.borrow_mut().insert(3, later);
        for n in 0..3 {
            m.beats.borrow_mut().insert(n, later);
        }
        assert!(m.liveness_tick(timeout, later));
        assert!(m.map.borrow().nodes[&3].up);
        assert_eq!(beat(3), 400);
    }

    /// D149: a node that went down stays down for its hold, which doubles
    /// with each further down within the window, up to a minute; health
    /// warns while it flaps; ten quiet minutes start it afresh. A report
    /// still takes its lease at once.
    #[test]
    fn a_flapping_node_is_held_down_longer_each_time() {
        use hyperstor_proto::Msg;
        let m = Meta::with_lease(map(), Duration::from_millis(400));
        let timeout = Duration::from_secs(3);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let beats = |t: Instant| {
            for n in 0..4 {
                m.beats.borrow_mut().insert(n, t);
            }
        };
        let report = |t: Instant| {
            m.reports
                .borrow_mut()
                .insert(3, [(0, t), (1, t)].into_iter().collect());
        };
        let up = || m.map.borrow().nodes[&3].up;
        // Reported: down at once, and no lease.
        beats(at(0));
        report(at(0));
        assert!(m.liveness_tick(timeout, at(0)));
        assert!(!up());
        match m.handle(Ok(Msg::Heartbeat { node: 3 })) {
            Msg::HeartbeatOk { lease_ms, .. } => assert_eq!(lease_ms, 0, "no lease while reported"),
            other => panic!("{other:?}"),
        }
        // Reports gone and heartbeats fresh, but held for 2 s.
        m.reports.borrow_mut().clear();
        beats(at(1900));
        assert!(!m.liveness_tick(timeout, at(1900)), "held down");
        assert!(!up());
        beats(at(2100));
        assert!(m.liveness_tick(timeout, at(2100)));
        assert!(up());
        // Down again within the window: held twice as long.
        beats(at(3000));
        report(at(3000));
        assert!(m.liveness_tick(timeout, at(3000)));
        m.reports.borrow_mut().clear();
        beats(at(6900));
        assert!(!m.liveness_tick(timeout, at(6900)), "held 4 s now");
        beats(at(7100));
        assert!(m.liveness_tick(timeout, at(7100)));
        assert!(up());
        // Health warns about it.
        let f = m.flapping(at(7100));
        assert_eq!(f, [(3, 2)].into_iter().collect::<Flapping>());
        let s = api::status(
            &m.map.borrow(),
            None,
            &Default::default(),
            &f,
            "127.0.0.1:8849",
        );
        assert_eq!(s.health, hyperstor_api::Health::HEALTH_WARN);
        assert!(
            s.reasons
                .iter()
                .any(|r| r.text == "node 3 flapping (2 transitions in 10 min)"),
            "{:?}",
            s.reasons
        );
        // The hold doubles up to a minute.
        assert_eq!(flap_hold(1), Duration::from_secs(2));
        assert_eq!(flap_hold(3), Duration::from_secs(8));
        assert_eq!(flap_hold(6), Duration::from_secs(60));
        assert_eq!(flap_hold(100), Duration::from_secs(60));
        // Ten quiet minutes: no longer flapping, and the next hold is 2 s.
        let quiet = 7100 + 600_000;
        assert!(m.flapping(at(quiet)).is_empty());
        beats(at(quiet));
        report(at(quiet));
        assert!(m.liveness_tick(timeout, at(quiet)));
        m.reports.borrow_mut().clear();
        beats(at(quiet + 2100));
        assert!(m.liveness_tick(timeout, at(quiet + 2100)), "back after 2 s");
        assert!(up());
    }

    /// D91: a slow member leaves a PG on its primary's report only; a
    /// primary that peers report silent may be the one cut off, so its
    /// reports are ignored. A failed device leaves placement at once.
    #[test]
    fn slow_members_leave_on_their_primarys_word_and_failed_devices_at_once() {
        use hyperstor_proto::Msg;
        let m = Meta::new(map());
        let pg = m.map.borrow().pg(1, 5).unwrap().clone();
        let (primary, second, third) = (pg.acting[0], pg.acting[1], pg.acting[2]);
        let slow = |reporter, device| {
            m.handle(Ok(Msg::MemberSlow {
                reporter,
                pool: 1,
                pg: 5,
                pg_epoch: pg.epoch,
                device,
            }));
            !m.map.borrow().pg(1, 5).unwrap().acting.contains(&device)
        };
        assert!(!slow(second, third), "only the primary reports");
        // Reported silent by a peer: its own reports wait.
        let pnode = m.map.borrow().devices[&primary].node;
        m.reports
            .borrow_mut()
            .entry(pnode)
            .or_default()
            .insert(99, Instant::now());
        assert!(!slow(primary, third));
        m.reports.borrow_mut().clear();
        assert!(slow(primary, third));
        let e = m.map.borrow().epoch;
        m.handle(Ok(Msg::DeviceFailed { device: second }));
        let map = m.map.borrow();
        assert!(map.failed.contains(&second) && map.epoch > e);
        assert!(map.pgs.values().all(|p| !p.up.contains(&second)));
    }

    /// D156: the leader keeps each device's last space report, out of the
    /// map, so it changes no epoch.
    #[test]
    fn device_space_is_kept_out_of_the_map() {
        use hyperstor_proto::Msg;
        let m = Meta::new(map());
        let epoch = m.map.borrow().epoch;
        for free in [600, 500] {
            let r = m.handle(Ok(Msg::DeviceSpace {
                device: 7,
                total_bytes: 1000,
                free_bytes: free,
            }));
            assert!(matches!(r, Msg::Noted { map_epoch } if map_epoch == epoch));
        }
        assert_eq!(m.space.borrow().get(&7), Some(&(1000, 500)));
        assert_eq!(m.map.borrow().epoch, epoch);
    }

    /// D158: weights follow each device's capacity in TiB, all at once
    /// when every live device has reported; an operator's weight stays
    /// until set back to auto; a joining host's devices start at 0.
    #[test]
    fn device_weights_follow_capacity() {
        use hyperstor_proto::Msg;
        const TIB: u64 = 1 << 40;
        let m = Meta::new(map());
        let report = |device: u32, total: u64| {
            m.handle(Ok(Msg::DeviceSpace {
                device,
                total_bytes: total,
                free_bytes: total / 2,
            }))
        };
        let weight = |d: u32| m.map.borrow().devices[&d].weight;
        let devices: Vec<u32> = m.map.borrow().devices.keys().copied().collect();
        let epoch = m.map.borrow().epoch;
        // Half a TiB each, device 1 two TiB: nothing until the last report.
        for (i, d) in devices.iter().enumerate() {
            assert_eq!(m.map.borrow().epoch, epoch, "changed before every report");
            report(*d, if *d == 1 { 2 * TIB } else { TIB / 2 });
            assert_eq!(weight(*d), if i + 1 < devices.len() { 1.0 } else { 0.5 });
        }
        assert_eq!(m.map.borrow().epoch, epoch + 1, "one change for all");
        assert!(
            devices
                .iter()
                .all(|d| weight(*d) == if *d == 1 { 2.0 } else { 0.5 })
        );
        // Within 1 %: left as it is.
        report(0, TIB / 2 + TIB / 400);
        assert_eq!((weight(0), m.map.borrow().epoch), (0.5, epoch + 1));
        // An operator's weight stays, and shows in the map until auto.
        assert_eq!(m.set_device_weight(0, Some(0.25)), Ok((epoch + 2, 0.25)));
        report(0, TIB);
        assert_eq!(weight(0), 0.25);
        assert!(m.map.borrow().fixed_weights.contains(&0));
        assert_eq!(m.set_device_weight(0, None), Ok((epoch + 3, 1.0)));
        assert!(m.map.borrow().fixed_weights.is_empty());
        assert!(m.set_device_weight(0, Some(-1.0)).is_err());
        assert!(m.set_device_weight(0, Some(f64::NAN)).is_err());
        assert!(m.set_device_weight(99, Some(1.0)).is_err());
        // A host joining: weight 0, nothing placed on it, until it reports.
        let node = m.add_node(0, &["127.0.0.1:7900".parse().unwrap()]).unwrap();
        let new = node * 1000;
        assert_eq!(weight(new), 0.0);
        assert!(m.map.borrow().pgs.values().all(|p| !p.up.contains(&new)));
        report(new, TIB);
        assert_eq!(weight(new), 1.0);
    }

    /// A replicated member (D72, D73): a follower serves the map and
    /// redirects the leader's requests; a new leader grants leases at
    /// once but changes no role until 1.25 leases have passed; a member
    /// that loses the lead stops changing roles.
    #[test]
    fn members_redirect_and_a_new_leader_waits_out_old_leases() {
        use hyperstor_proto::*;
        use replica::{Leading, ToReactor};
        let lease = Duration::from_millis(200);
        let (commits, mut committed) = tokio::sync::mpsc::unbounded_channel::<replica::Commit>();
        let m = Meta::replicated(map(), lease, commits);
        let leader: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        hyperstor_reactor::block_on(async move {
            let (tx, rx) = hyperstor_reactor::chan::channel();
            hyperstor_reactor::spawn(m.clone().follow(rx));
            let settle = || hyperstor_reactor::time::sleep(Duration::from_millis(20));
            // Before an election: a follower with no leader to name.
            let beat = || m.handle(Ok(Msg::Heartbeat { node: 1 }));
            assert!(
                matches!(beat(), Msg::Error { code: Code::NotLeader, ref detail, .. } if detail.is_empty())
            );
            let space = Msg::DeviceSpace {
                device: 0,
                total_bytes: 1 << 30,
                free_bytes: 1 << 29,
            };
            assert!(matches!(
                m.handle(Ok(space)),
                Msg::Error {
                    code: Code::NotLeader,
                    ..
                }
            ));
            assert!(m.space.borrow().is_empty());
            assert!(matches!(
                m.handle(Ok(Msg::GetMap { have_epoch: 0 })),
                Msg::Map { .. }
            ));
            tx.send(ToReactor::Leader(Leading::Other(leader))).unwrap();
            settle().await;
            assert!(
                matches!(beat(), Msg::Error { code: Code::NotLeader, ref detail, .. } if *detail == leader.to_string())
            );
            assert!(m.set_device_in(0, false).is_err());
            // The store's map arrives; then this member is elected.
            tx.send(ToReactor::Applied(Box::new(m.published.borrow().clone())))
                .unwrap();
            tx.send(ToReactor::Leader(Leading::Me)).unwrap();
            settle().await;
            assert!(matches!(m.role(), Role::Leader { .. }));
            assert!(
                matches!(beat(), Msg::HeartbeatOk { lease_ms: 200, .. }),
                "leases at once"
            );
            assert!(
                !m.may_change_roles(),
                "no role changes before the old leases end"
            );
            hyperstor_reactor::time::sleep(lease + lease / 4).await;
            assert!(m.may_change_roles());
            // A change it publishes goes through the group first.
            m.set_device_in(0, false).unwrap();
            let pub_ = hyperstor_reactor::spawn(m.clone().publisher());
            let c = loop {
                if let Ok(c) = committed.try_recv() {
                    break c;
                }
                settle().await;
            };
            assert!(
                c.map.epoch > m.published.borrow().epoch,
                "not visible before it commits"
            );
            c.reply.send(Ok(())).unwrap();
            settle().await;
            assert_eq!(m.published.borrow().epoch, c.map.epoch);
            drop(pub_);
            // Lost the lead: back to redirecting.
            tx.send(ToReactor::Leader(Leading::Unknown)).unwrap();
            settle().await;
            assert!(!m.may_change_roles());
            assert!(matches!(
                beat(),
                Msg::Error {
                    code: Code::NotLeader,
                    ..
                }
            ));
        });
    }

    /// I197: a follower's view of what only the leader knows comes from
    /// the leader over the map protocol, kept a second; with no leader,
    /// none.
    #[test]
    fn a_follower_asks_the_leader_for_its_view() {
        use hyperstor_proto::*;
        use hyperstor_reactor::{block_on, net::*, spawn};
        use replica::{Leading, ToReactor};
        let leader = Meta::new(map());
        leader.handle(Ok(Msg::Heartbeat { node: 1 }));
        leader.handle(Ok(Msg::DeviceSpace {
            device: 7,
            total_bytes: 1 << 30,
            free_bytes: 1 << 29,
        }));
        leader.inconsistent.borrow_mut().insert((1, 2), 3);
        let (commits, _committed) = tokio::sync::mpsc::unbounded_channel::<replica::Commit>();
        let f = Meta::replicated(map(), Duration::from_millis(200), commits);
        block_on(async move {
            assert_eq!(f.leader_view().await, None, "no leader known");
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            spawn(serve(l, leader.clone()));
            let (tx, rx) = hyperstor_reactor::chan::channel();
            spawn(f.clone().follow(rx));
            tx.send(ToReactor::Leader(Leading::Other(addr))).unwrap();
            hyperstor_reactor::time::sleep(Duration::from_millis(20)).await;
            let v = f.leader_view().await.expect("the leader's view");
            assert_eq!(v.space.get(&7), Some(&(1 << 30, 1 << 29)));
            assert_eq!(v.inconsistent.get(&(1, 2)), Some(&3));
            assert!(v.heard[&1] < Duration::from_secs(1), "{v:?}");
            // Kept a second: a change on the leader shows after.
            leader.handle(Ok(Msg::DeviceSpace {
                device: 7,
                total_bytes: 1 << 30,
                free_bytes: 1 << 28,
            }));
            assert_eq!(f.leader_view().await.unwrap().space[&7].1, 1 << 29);
            hyperstor_reactor::time::sleep(LEADER_VIEW_FOR).await;
            assert_eq!(f.leader_view().await.unwrap().space[&7].1, 1 << 28);
            // The leader answers only as leader.
            assert!(matches!(
                f.handle(Ok(Msg::LeaderState)),
                Msg::Error {
                    code: Code::NotLeader,
                    ..
                }
            ));
        });
    }

    #[test]
    fn map_service_answers() {
        use hyperstor_proto::*;
        use hyperstor_reactor::{block_on, net::*, spawn};
        let m = Meta::new(map());
        let want = m.map.borrow().clone();
        block_on(async move {
            let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = l.local_addr().unwrap();
            spawn(serve(l, m));
            let s = TcpStream::connect(addr).await.unwrap();
            let (w, _) = s
                .write_all(encode_frame(9, &Msg::GetMap { have_epoch: 0 }, &[]))
                .await;
            w.unwrap();
            let (r, hb) = s.read_exact(vec![0; HEADER_LEN]).await;
            r.unwrap();
            let h = Header::decode(hb[..].try_into().unwrap()).unwrap();
            assert_eq!(h.req, 9);
            let (r, body) = s.read_exact(vec![0; h.body_len as usize]).await;
            r.unwrap();
            let Msg::Map { epoch, map } = Msg::decode(h.kind, &body).unwrap() else {
                panic!()
            };
            assert_eq!(epoch, want.epoch);
            assert_eq!(ClusterMap::decode(&map).unwrap(), want);
        });
    }
}
