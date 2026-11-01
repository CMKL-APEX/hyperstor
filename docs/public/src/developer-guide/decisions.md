# Design decisions and trade-offs

Each section lists a decision, why it was made, and what was weighed
against it. **Status** says whether it is built; the
[Status](../status.md) page has the final word.

## Data path and runtime

### Pure Rust on io_uring, with no SPDK or DPDK

| | |
|---|---|
| **Decision** | Every storage core runs Hyperstor's own reactor on io_uring. NVMe stays on the kernel's driver (passthrough through `uring_cmd` is planned); the network goes through kernel sockets, and later RDMA verbs |
| **Why** | One language and one build, with no C framework in the data path. Devices aren't unbound from the kernel, so `lsblk`, SMART, partitions and the rest of the host keep working, and no DPDK process competes with the host's virtual switch. Idle cores sleep instead of polling. The client library can be embedded in any program. Owning the loop puts disk, sockets, RDMA and virtqueue polling in one place |
| **Alternatives** | **SPDK and DPDK**, used by Mayastor, Longhorn's V2 engine, simplyblock and DAOS: the fastest path today, but they take devices and NICs from the kernel, need huge pages, and poll whole cores. **tokio**: multi-threaded work stealing moves tasks between cores. **glommio, monoio, compio**: thread-per-core io_uring runtimes, but each would own the loop that RDMA and virtqueues must share |
| **Cost** | Some per-core speed against SPDK, to be measured on NVMe hardware; an NVMe-oF target of its own (planned) |
| **Status** | Built; the comparison with SPDK is still to run |

### tokio only on the control path

Raft, the cluster API, the installer and the CLI use tokio, on threads
of their own, and pass changes to reactors over channels. They need TLS,
HTTP/2 and gRPC, and none is latency-critical. Running openraft on the
reactor was rejected: a runtime adapter would be large and risky for no
gain.

### A device wait never holds a reactor

| | |
|---|---|
| **Decision** | The engine plans an operation in memory, submits its device I/O on the reactor's own io_uring, returns to the reactor, and finishes when the completions arrive. It comes in stages: commits first (one in flight per device), then reads (up to 128 in flight per device), then checkpoint slices and in-place writes |
| **Why** | A reactor waiting on its device answers no pings. On a three-host test cluster, a flush of 100–200 ms or a 1 MiB read held a reactor long enough for peers to report a healthy node silent. The same submission path is what NVMe passthrough needs |
| **Alternatives** | **An engine thread per device**: a hop per operation, and threads off the reactors' cores. **Pings on a thread of their own**: a stuck reactor would look alive |
| **Rules that come with it** | A commit's writes and flush get 30 s (`hyperstord --io-timeout-secs`), as Linux gives an NVMe command; past that the device is failed and its PGs placed elsewhere. Without it, a hung device would no longer stop its node's pings and would hang unseen. A read gets 2 s (`--read-timeout-ms`); past that the primary serves it from another copy, and three in a row fail the device. Space a commit frees while reads are in flight is held until those reads end, so a read never sees its blocks reused |
| **Status** | Commits and reads are on `main` and in testing; checkpoint slices and in-place writes are next |

### A custom framed protocol between nodes

Every frame has a 32-byte header with its own CRC32C; block data travels
after the body with a CRC per block. Every request carries the map and PG
epochs, and the lease it acts under (see
[fencing one volume](#fencing-one-volume-not-the-whole-client)).
**Alternative:** NVMe-oF between nodes, which would need reservations
and vendor commands for fencing. NVMe-oF is planned as a front end for
clients instead.

### Redirect-on-write on flash, deferral only on hard disks

See [the data path](data-path.md#redirect-on-write-and-deferral-for-hard-disks).
Deferral everywhere was the first design; measurement showed it cost
about a third of batched 4 KiB write throughput on flash (85k against
133k writes per second at batch 128).

### Cut CPU per operation before adding RDMA

Software costs 40–90 µs per request today, more than a TCP round trip on
a fast network. Polling for 200 µs after each request took QD1 writes
from 109 to 84 µs. Shorter polling (50 µs) gets about half the gain; it
is the option for power-limited or shared-core hosts.

### Devices name themselves, and carry feature flags

| | |
|---|---|
| **Decision** | Each device's superblock records its device id and cluster id, and a node checks them before opening it. The superblock carries compatible, read-only-compatible and incompatible feature flags, as ext4 and XFS do. Only a blank device is ever formatted: a newer format or a damaged superblock is refused, not taken for blank. The installer formats block devices, and only those with no other system's signature (partition table, LVM, md RAID, Ceph, a filesystem); `hyperstord` formats files only. Device ids are never given again |
| **Why** | NVMe names can change at boot, and a drive moved between hosts or clusters must not be served as another. Flags let later format changes (an on-device index, encryption) reach a running cluster one device at a time: an older build refuses a device it can't read instead of misreading it. Before, a newer format or two damaged superblock copies read as blank and would have been formatted |
| **Status** | Built |

## Protection and placement

### Its own object store, not per-device volumes

Every device is an extent store of general objects with attributes and
key–value tables, like Ceph's RADOS with its object maps.
**Alternative:** a thin volume manager per device (LVM-thin, SPDK
logical volumes): every snapshot would then need coordination across
replicas, and erasure coding doesn't fit it.

### 4 MiB chunks, rendezvous hashing, placement by lineage

See [placement](replication.md#placement). **Alternative:** CRUSH, which
moves more data for erasure-coded pools and needs tuning.

### A device's weight is its capacity

| | |
|---|---|
| **Decision** | The map service sets each device's weight, in TiB, from the size the device reports. A new report changes the map only when it differs by more than 1 %. An operator can pin a weight (`hyperstorctl weight`) until it is set back to `auto`. A change moves its PGs at once, under a per-target backfill budget |
| **Why** | Every device used to join with weight 1, so a small device filled first. Capacity by default needs no step to forget; the override covers a slower drive or a slow drain. The 1 % threshold keeps reports every 5 s from churning the map |
| **Alternatives** | **A balancer** that sets a second weight from how full each device is (Ceph's `crush-compat`): added only if a device reaches `nearfull` while its class has room. **Per-PG overrides in the map** (Ceph's `upmap`): not used, so placement stays a pure function of the map's weights |
| **Status** | Built |

### Pools grow and shrink with their devices

| | |
|---|---|
| **Decision** | A pool names a set of device classes and places on any device of them, by capacity. Devices can be added to a running node (the installer registers and formats the device, then restarts the node), and added to or removed from one pool. A device is removed only after its PGs have moved, and its node with its last device. A pool's PG count grows by splitting PGs in place, then moving the new PGs a few at a time; the map service doubles a pool's count as devices are added, unless that is turned off. A pool's copies or layout change only by making a new pool |
| **Why** | Storage is bought and retired over a cluster's life; a pool fixed at creation would strand both. A split moves objects to their new PG's key without copying, and the new PGs start on their parent's devices, so no data moves twice (Ceph splits the same way). Changing copies in place would rewrite every PG under running volumes, for a need a new pool meets |
| **Limits** | Hard disks and flash don't share a pool unless forced, because a replicated write waits for its slowest copy. A pool that holds data is deleted only when forced, after a dry run lists what goes with it. PG merges come later, and only by the operator. Adding a device restarts its node for now; starting a reactor in a running node comes later |
| **Status** | Built, apart from parts of adding a device to a running node, which are in progress |

### A pool's usable space follows its failure-domain rule

Every device reports its data region and free bytes to the map service
every 5 s, beside its heartbeat and outside the replicated map: space
changes all the time and decides no placement. A pool's usable space
counts no failure domain for more than one copy of each PG, so with as
many hosts as copies, the smallest host bounds the pool.
**Alternatives:** a plain sum divided by the copies, which overstates a
pool on uneven hosts; Ceph's `MAX AVAIL`, limited by the device that
fills first, which jumps when one device runs ahead.

### Fullness is measured on each device

| | |
|---|---|
| **Decision** | Three cluster-wide ratios in the map: `nearfull` (85 %) warns, a device past `backfillfull` (90 %) takes no new backfill, and a device past `full` (95 %) refuses what would grow its used space: new objects and writes to unwritten blocks. Overwrites, deletes and discards go on. A pool is as full as the fullest device it uses; a full pool takes no new volumes, clones or snapshots. Each change shows in health and in the audit log |
| **Why** | A thin pool that runs out must still let the operator free space. The device is what runs out, and it knows at once, with no map change in between. Pools share devices, so a per-pool ratio would have no single meaning, and stopping a whole pool (as Ceph does) would stop every pool on the device. Refusing creations shows the problem to an operator before a guest's write fails. Recovery must not fill the survivors until their own writes stop |
| **For VMs** | virtio-blk has no "no space" status, and an I/O error can turn a guest's filesystem read-only. `hyperstor-vhost` holds such a write and retries it every second for up to 5 minutes (`--hold-full`), so space freed in time (trash purged, a device added) lets it finish. Pausing the VM, as QEMU's `werror=stop` does, waits until holding proves too short. `hyperstor-ublk` answers `ENOSPC` at once |
| **The trash** | A pool can be set to purge its oldest trash when full (`trash settings --on-full purge`); the default keeps it |
| **Status** | Built |

### A device deletes copies it no longer serves

Once a PG has been clean without a device (every member live, and the
device in neither its `up` nor its `acting` set) for a minute, at one
epoch, the device deletes its copy of that PG, a batch at a time.
**Why:** every rebalance, drain or weight change used to leave the old
copies for ever, and a device could reach `full` with data nothing
serves. The grace keeps a copy through a quick change back, and waiting
for a clean PG keeps every copy recovery could still need. A deleted
pool's data goes the same way.

### Erasure coding through a replicated log (planned)

Small writes to an erasure-coded pool go to a three-copy log and are
written out as full stripes later; full-stripe writes skip the log.
**Why:** replica-like write latency, with no read-modify-write in the
foreground.

### Cache pools for data that can be recomputed

A third pool kind beside replicated and erasure-coded: one copy, no
recovery, evicted by expiry then least recent use (eviction is planned).
**Why:** replicating an inference engine's offloaded KV cache wastes
bandwidth on data that can be recomputed. **Alternative:** a replicated
pool with one copy, which would still pay for recovery.

A cache pool keeps its data across PG splits and restarts: each cache
PG has a generation in the map, and a device drops its copy only when
the generation has moved. A cache PG whose copies are all on down nodes
waits for them for the pool's grace (60 s by default, `--cache-grace`):
reads miss meanwhile, and a write ends the wait for its PG. **Why:** a
node is marked down after 0.8 s, and a restart takes seconds. Reads miss
rather than wait, so nothing stalls on a cache; a write can't be
skipped without leaving the returning copy stale.

### Encryption at rest (designed, not built)

Chosen per pool and done on the PG's primary before it replicates, with
AES-256-XTS and a key per volume branch, wrapped by a cluster key.
**Why on the primary:** every copy then holds the same ciphertext, so
backfill, digest comparison and scrub work unchanged. **Alternatives:**
keys per device, or dm-crypt under the engine, which would break digest
comparison between copies; encrypting in the client, which costs the
GPU-direct path and later storage-side work. Deleting a key makes a
purge or a tenant's removal final.

## Metadata and consistency

### Its own Raft map service, holding the map only

**Why:** I/O must survive outages of the platform's control plane, and
the map is small. **Alternative:** keeping the map in glidex's Raft
store. The Raft layer and its store are shared with glidex as a library
instead.

### Volume metadata in header objects, not in Raft

See [volumes](volumes.md#volume-metadata).

### Map leases with fencing, and pushes as the fast path

See [map leases](replication.md#map-leases-and-fencing).

| Option | Weighed |
|---|---|
| **Leases now, with pushes** (chosen) | Bounds how long a cut-off primary serves; costs a stop of I/O after one lease if the whole map service is down |
| Pushes only | Can't reach a partitioned node, the very case that loses data |
| Leases later | Leaves a stale-primary window open while virtual machines are tested |

### Map leases are the read leases

**Alternative:** read leases granted by the replicas (the first design),
which would add state to every write and a wait to every peering. With a
replicated map service, a whole-map-service outage is rare enough.

### Fencing one volume, not the whole client

| | |
|---|---|
| **Decision** | Every attach takes a random 64-bit lease identity, recorded in the volume's header and carried on every request made under it. A forced takeover blocklists that identity, not the client. The lease's holder can add delegates, identities that act under its lease and are fenced with it |
| **Why** | One `hyperstor-vhost` serves every VM of a host through one client. Fencing the client to move one disk took every disk on the host with it |
| **Alternative** | A client per volume: volumes × storage nodes connections per host |
| **Cost** | 8 bytes a request, and a second lookup only while the blocklist isn't empty: nothing measurable |
| **Status** | Built. Each lease also carries a short label naming its holder, so a scheduler can tell a VM's stale hold (preempt it) from a tool's (wait for it) |

### Failover in about one second

Map lease 0.6 s, down after 0.8 s, heartbeats every 100 ms, peers report
after 0.3 s. Measured: 0.66 s of I/O gap when a primary dies, against
2.5–3.1 s with the first 2 s / 3 s timings. **Trade-off:** more heartbeat
traffic, and a higher risk of a false failure on a busy network. On a
three-host cluster over 1 GbE under load, the only silences seen came
from lost packets on TCP pings; the timeouts stay where they are until
they have been measured on faster networks.

### Peers report each other, pinged over UDP

A node is marked down early on reports from two peers. **Alternative:**
detection by the map service alone, which waits for the full timeout.

Peers ping each other over UDP, on each device's TCP port number, with a
nonce each pong echoes; a peer that never answers over UDP keeps TCP.
**Why:** on TCP, one lost ping or pong waits for the 200 ms
retransmission timeout, and the pings behind it with it: that caused the
300–400 ms silences seen under load. Over UDP a lost ping costs only its
own 100 ms slot. The TCP fallback needs no coordination during a rolling
upgrade, and a firewall that drops UDP leaves pings on TCP rather than
silent.

### A node that flaps is held down

The map service brings a node marked down back up only after a hold:
2 s, doubled for each further down within 10 minutes, at most 60 s.
Health warns while a node flaps. **Why:** a node whose reactor stalled
past the peer timeout went down and up about 150 times a minute on a
test cluster, and every change bumped the map and shrank or refilled
PGs. A down node is something the cluster handles; the warning says
someone should find out why.

## Recovery

### No PG log: peering and recovery by content digests

See [peering](replication.md#peering-and-recovery-by-content-digests).
The log comes only if digest scans prove too slow on large PGs. Large
key–value tables are digested in 1 MiB parts with a yield between, so
one object can't hold a reactor.

### A primary drops a slow copy from one PG

**Alternative:** leave it to liveness detection, which can't see a hung
disk on a live node.

### Backfill is budgeted per target

A reactor's backfills to one target keep at most 16 MiB of pushes on the
wire, objects too large for one frame go in parts, and pings travel
apart from data. **Why:** an empty node rejoining got a backfill of
every PG at once; pings and replicated writes waited behind hundreds of
MiB, and the cluster flapped. **Alternative:** a cap on PGs backfilling
at once, which also stopped it but kept PGs at one copy longer.

### The fault-injection rig runs in process

**Alternative:** a rig across hosts with traffic control, network
namespaces and delayed devices: it needs root on test hosts, and its
failures don't replay exactly. The device timeouts were also checked by
hand on a real kernel device slowed with `dm-delay`.

### The index

| Option | Weighed |
|---|---|
| **Dense block maps now** (built) | Caps index memory at 8 bytes per block, whatever the fragmentation |
| **Fuzzy checkpoints**, then **paged checkpoints** (both built) | A large shard rewrote its whole index on every checkpoint. Paged checkpoints write only the 128 KiB pages changed since the last, so their cost follows the rate of change: 16× less when a hundredth of the chunks change. Smaller pages helped little until 16 KiB, which cost a quarter more for disks written all over |
| **Copy-on-write B+tree on the device** (planned) | Cheap snapshots and clones, the index no longer bounded by memory; it keeps the log as its write-ahead log, because committing a tree per write costs 30–100× the metadata writes |
| LSM tree | Rejected: compaction spikes, and merged reads suit point lookups poorly |
| Background defragmentation | Rejected: write amplification |

Block maps and metadata (agent memory, retrieval stores, key–value
ranges) are planned to use two flavours of one node format: a B+tree for
block maps and a Bε-tree, which buffers writes in inner nodes, for
metadata.

### Snapshots before the copy-on-write B+tree

| | |
|---|---|
| **Decision** | Snapshots, clones, rollback and consistency groups are built on today's in-memory index, as chunk versions keyed by epoch ([volumes](volumes.md#snapshots-and-clones)) |
| **Why** | Snapshots don't need the B+tree. With the epoch in the chunk's key, every copy and every PG stays consistent without coordinating them, and versions as objects reuse the replication, recovery and scrub already built |
| **Alternative** | Sealing each shard's index at the cut: that snapshots shards, not volumes, at slightly different moments, and needs cluster-wide coordination for every snapshot |
| **Next** | Zone or FDP placement for write-once data (model weights, checkpoints, cache pools); not for VM disks, whose small overwrites under snapshots would need garbage collection that copies live blocks |
| **Status** | Built |

### Log size

The log is a sixteenth of the shard, up to 1 GiB: checkpoints then write
52 MB/s per TiB instead of 207, and replay takes up to about 3 s.
**Alternative:** a 4 GiB log, which means about 11 s of recovery.

## Front ends

### Its own ublk server, on the reactor

`hyperstor-ublk` serves volumes through the kernel's ublk driver. Each
queue command is a `uring_cmd` on the same reactor that runs the client,
so requests never cross runtimes. **Alternative:** the `libublk` crate,
which runs each queue on its own io_uring and runtime. **Status:** built.

### VM disks over vhost-user-blk, one service per host

| | |
|---|---|
| **Decision** | `hyperstor-vhost` serves volumes to Cloud Hypervisor and QEMU over vhost-user-blk, one service per host, sending each chunk's I/O straight to its PG's primary. It takes rust-vmm's `vhost`, `virtio-queue` and `vm-memory` crates as libraries and drives them from its own reactor; each queue is a task waiting on its kick eventfd |
| **Why** | One hop from the guest's queue to the storage node, with proven protocol and ring code and no second event loop on the data path |
| **Alternatives** | **A kernel NVMe-oF initiator**, which adds a hop through a gateway. **rust-vmm's `vhost-user-backend` daemon**, which runs an epoll thread per queue |
| **Staging** | A scheduler attaches and detaches disks through a local control socket (one JSON request per line, the caller's uid checked). Creating, expanding and deleting volumes go to the cluster API. Staging is local to the host, so it doesn't depend on reaching the cluster API |
| **Restarts** | Stopping the service keeps its volumes: a state file names them, and the next start takes each back, only from its own previous lease, so a volume moved meanwhile is never stolen. Requests in flight are tracked in the hypervisor's in-flight buffer and run again after a restart; without it, a guest waited for ever on requests the old process had taken. Both hypervisors reconnect to the restarted service (QEMU when told to) |
| **512e disks** | 512-byte logical sectors on 4 KiB physical blocks. Cloud images keep their partition table in 512-byte sectors and firmware looks for the GPT at sector 1, so a 4 KiB-sector disk doesn't boot them. A partial block is read, merged and written back under a per-volume block lock; guests align filesystem I/O to the physical block, so this is rare. **Rejected:** a block size per volume, and converting images |
| **Status** | In progress: VMs boot from volumes under both hypervisors, and restarts keep their disks. Adaptive polling, queues on reactors per NUMA node and zero copy from guest memory are planned |

### The host read cache lives in the client library

Each client keeps an in-memory cache (256 MiB by default) of blocks from
versions that can never change: snapshots, and the parents that clones
read through. The head of a volume is never cached. **Why:** such data
needs no invalidation, so the cache needs no protocol; and in the
client, `hyperstor-ublk` and `hyperstor-vhost` both get it. It serves
the common heavy readers: many VMs booting clones of one image, or
loading the same model weights. **Status:** in progress; a second tier
on local NVMe comes later.

### NFS and pNFS: a settled direction, the rest proposed

Settled: Hyperstor will serve unmodified NFSv4.1 and 4.2 clients with a
server of its own in Rust, on the reactor, with pNFS data servers inside
the storage nodes and file data placed in stripe groups of consecutive
PGs. **Why:** with no gateway on the data path, a pNFS write reaches the
primary in one hop, under the same leases and fencing.
**Alternatives:** NFS-Ganesha (C through FFI, its pNFS unmaintained) and
the kernel's `nfsd` (no usable layout for an object store). The layouts,
state handling and build steps are proposals; nothing is built, and it
comes after the filesystem.

### Planned

| Decision | Why | Alternative |
|---|---|---|
| Its own NVMe-oF target in Rust | Keeps the no-SPDK rule; the kernel's `nvmet` over a ublk device is the stopgap | SPDK's target |
| GPU-direct through dma-buf and RDMA, behind the cuFile and hipFile APIs | One network hop into GPU memory, for NVIDIA and AMD alike; unmodified programs keep their API | Forwarding through a kernel-initiator gateway |
| No NCCL or RCCL for storage traffic; ports carry roles and traffic classes | Storage stays off the GPU compute network | Sharing the collective libraries' transport |
| RDMA through rdma-core verbs | A small, stable interface | UCX, a large C runtime that would clash with copies programs bundle; libfabric only for newer fabrics |
| A native S3 gateway, with S3 over RDMA, and a filesystem on a transactional key–value layer | The same object layer carries them; filesystem metadata scales with key ranges | Stateful metadata servers with subtree partitioning |

## Administration

| Decision | Why |
|---|---|
| People log in only to a stateless management service, `hyperstor-mgmt`; the map service accepts machine identities only | Keeps human authentication out of the members that hold the map |
| mgmt keeps people's state (users, sessions, API tokens, roles) in the map service's replicated store, and caches a checked session for at most 10 s | Any number of mgmt instances share state without a second database; a logout or revocation takes effect within 10 s |
| Password logins go through a small root helper that checks PAM; OIDC logins use the authorization code flow with PKCE. Both come from a library shared with glidex | PAM needs root, and the service that faces browsers doesn't get it. One implementation of sessions, tokens and logins for both projects |
| Roles are Cedar policies over every cluster API call; destructive calls need a login of the last 10 minutes | One place decides who may do what, and deleting data takes a person who has just proved who they are; API tokens never count as a recent login |
| `hyperstorctl --mgmt` acts with a person's API token, and the map service's audit names that person | Scripts and people use one CLI, and every change is traced to someone |
| The web UI (React) is installed as files that mgmt serves | It can be updated without a new binary; serving files is what a separate UI service would do later |
| mgmt listens on every address over HTTPS by default, with a certificate from the cluster's own CA; operators can give it their own, or the node's Tailscale one. It never serves a self-signed certificate | People reach the UI without a tunnel, and nothing carrying passwords or cookies crosses the network in clear. The cluster's root is trusted once for every host, where a self-signed certificate would be checked by hand per host |
| An offline root CA and an online intermediate; an encrypted backup of the root key | A compromised host can't mint a new root; losing the root's host doesn't lose the cluster |
| Hosts join with single-use tokens, stored in Raft as hashes | A token works once across the whole group, and survives restarts |
| The leader backs up the map service's store on its own, and a group can be restored from one backup | A lost store comes back without rebuilding the map by hand |
| Members are added and removed at run time, one at a time; a new member is a learner until it catches up | The group never loses its majority to a member that is still copying the map |
| A group that lost its majority for good is restarted from one survivor, by an explicit, audited forced start that refuses while another member answers | The survivors' map is newer than any backup; the refusal keeps two groups from forming |
| Deletes go to a trash for a grace period (a day by default, set per pool); purging at once needs a recent login | A mistyped delete is the most common unrecoverable admin error; a day's grace costs only visible space |
| The audit log is append-only objects in the `.meta` pool: a day at a time, in parts per member, kept a year. Repeated refusals are summarised a minute at a time | It grows with the cluster's storage, not with the map service's snapshots, and outlives any one member. A client's retry storm once made one day's object too large to copy |
| Long work (drain, flatten, purge, removing a device, a forced pool delete) runs as operations: rows of the replicated store, kept a week, followed or cancelled from the CLI and the UI | No request holds a connection for minutes; an operation is cluster state, and a new leader resumes it |
| A new leader blocklists the last leader's client before it runs operations | A member that lost the lead may still be running a purge; a late purge must not delete a new volume that reused an id |

## Decisions still open

- Erasure-coded pools, the NVMe-oF and GPU-direct front ends, the
  on-device index and the key–value layer are designed but not built;
  their details may change as they are.
- Encryption at rest, and the NFS and pNFS design beyond the settled
  direction above, are not built.
- Several choices wait for measurements on real hardware: io_uring
  against SPDK on NVMe, failover timeouts on faster networks, digest
  scans on large PGs, how long `hyperstor-vhost` should hold a write
  when a device is full, and whether the Bε-tree is worth a second
  index flavour.
