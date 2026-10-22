# Design decisions and trade-offs

Each section lists a decision, why it was made, and what was weighed
against it. **Status** says whether it is built.

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

### A custom framed protocol between nodes

Every frame has a 32-byte header with its own CRC32C; block data travels
after the body with a CRC per block. Every request carries the map and PG
epochs. **Alternative:** NVMe-oF between nodes, which would need
reservations and vendor commands for fencing. NVMe-oF is planned as a
front end for clients instead.

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

### Failover in about one second

Map lease 0.6 s, down after 0.8 s, heartbeats every 100 ms, peers report
after 0.3 s. Measured: 0.66 s of I/O gap when a primary dies, against
2.5–3.1 s with the first 2 s / 3 s timings. **Trade-off:** more heartbeat
traffic, and a higher risk of a false failure on a busy network, still
to be measured under load.

### Peers report each other

A node is marked down early on reports from two peers. **Alternative:**
detection by the map service alone, which waits for the full timeout.

## Recovery

### No PG log: peering and recovery by content digests

See [peering](replication.md#peering-and-recovery-by-content-digests).
The log comes only if digest scans prove too slow on large PGs.

### A primary drops a slow copy from one PG

**Alternative:** leave it to liveness detection, which can't see a hung
disk on a live node.

### The fault-injection rig runs in process

**Alternative:** a rig across hosts with traffic control, network
namespaces and delayed devices: it needs root on test hosts, and its
failures don't replay exactly.

### The index

| Option | Weighed |
|---|---|
| **Dense block maps now** (built) | Caps index memory at 8 bytes per block, whatever the fragmentation |
| **Fuzzy checkpoints** (built), then **paged checkpoints** | A large shard rewrites its whole index on every checkpoint; paging writes only what changed |
| **Copy-on-write B+tree on the device** (planned) | Cheap snapshots and clones, the index no longer bounded by memory; it keeps the log as its write-ahead log, because committing a tree per write costs 30–100× the metadata writes |
| LSM tree | Rejected: compaction spikes, and merged reads suit point lookups poorly |
| Background defragmentation | Rejected: write amplification |

Block maps and metadata (agent memory, retrieval stores, key–value
ranges) are planned to use two flavours of one node format: a B+tree for
block maps and a Bε-tree, which buffers writes in inner nodes, for
metadata.

### Log size

The log is a sixteenth of the shard, up to 1 GiB: checkpoints then write
52 MB/s per TiB instead of 207, and replay takes up to about 3 s.
**Alternative:** a 4 GiB log, which means about 11 s of recovery.

## Front ends (planned)

| Decision | Why | Alternative |
|---|---|---|
| VMs use vhost-user-blk, served by one `hyperstor-vhost` per host | One hop from the guest's queue to the storage node | A kernel NVMe-oF initiator, which adds a hop through a gateway |
| Its own NVMe-oF target in Rust | Keeps the no-SPDK rule; the kernel's `nvmet` over a ublk device is the stopgap | SPDK's target |
| GPU-direct through dma-buf and RDMA, behind the cuFile and hipFile APIs | One network hop into GPU memory, for NVIDIA and AMD alike; unmodified programs keep their API | Forwarding through a kernel-initiator gateway |
| No NCCL or RCCL for storage traffic; ports carry roles and traffic classes | Storage stays off the GPU compute network | Sharing the collective libraries' transport |
| RDMA through rdma-core verbs | A small, stable interface | UCX, a large C runtime that would clash with copies programs bundle; libfabric only for newer fabrics |
| A native S3 gateway, with S3 over RDMA, and a filesystem on a transactional key–value layer | The same object layer carries them; filesystem metadata scales with key ranges | Stateful metadata servers with subtree partitioning |

## Administration

| Decision | Why |
|---|---|
| People log in only to a stateless management service; the map service accepts machine identities only | Keeps human authentication out of the members that hold the map |
| An offline root CA and an online intermediate; an encrypted backup of the root key | A compromised host can't mint a new root; losing the root's host doesn't lose the cluster |
| Hosts join with single-use tokens, stored in Raft as hashes | A token works once across the whole group, and survives restarts |
| The leader backs up the map service's store on its own, and a group can be restored from one backup | A lost store, or a group that lost its majority, comes back without rebuilding the map by hand |
| Long work (drains) runs as operations that can be followed or cancelled | Scripts and the UI can follow progress without polling the map |

## Decisions still open

- Erasure-coded pools, snapshots, the front ends and the key–value layer
  are designed but not built; their details may change as they are.
- Restarting a map service group that lost its majority for good from
  one survivor, and changing members at run time, are decided but not
  built. Map backups and restoring a group from one are built.
- Several choices wait for measurements on real hardware: io_uring
  against SPDK on NVMe, the one-second failover under load, digest scans
  on large PGs, and whether the Bε-tree is worth a second index flavour.
