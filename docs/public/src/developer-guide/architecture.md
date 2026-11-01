# Architecture

## Processes

```text
     VMs (vhost-user-blk), ublk, glidex, tools: all through hyperstor-client
                 │  map, leases                     │  data: one TCP port per reactor
                 ▼                                  ▼
   ┌───────────────────────────┐   heartbeats,  ┌──────────────────────────┐
   │ hyperstor-meta × 3 or 5   │◄──────────────►│ hyperstord (every node)   │
   │ map service, Raft         │  map leases,   │ one reactor per device    │
   │ cluster API (mTLS)        │  fences, pushes│ shard, engine on io_uring │
   └───────────────────────────┘                └──────────────────────────┘
         ▲  cluster API                              ▲   replication, pings
         │                                           │   between storage nodes
   hyperstorctl, hyperstor-mgmt (web UI),            └── (same port numbers)
   hyperstor-install
```

| Process | Built | Role |
|---|---|---|
| `hyperstor-meta` | yes | The map service: holds the cluster map, detects failures, grants map leases, sets device weights, serves the cluster API and runs long operations (drains, purges, device removals). Three or five members replicate the map with Raft |
| `hyperstord` | yes | A storage node: one reactor thread per device shard, serving reads, writes, replication, recovery and scrub |
| `hyperstorctl` | yes | The administrator's CLI, over the cluster API, or through `hyperstor-mgmt` as a person with an API token (`--mgmt`) |
| `hyperstor-install` | yes | Host checks and a convergent installer; joins hosts with single-use tokens; formats and adds block devices |
| `hyperstor-mgmt` | yes | The management service: people log in with their host password or an identity provider (OIDC), get roles (viewer, operator, storage admin, security admin) checked by Cedar policies, and call the cluster API through it, named in its audit log. It serves the React web UI over HTTPS |
| `hyperstor-authd` | yes | A small root helper that checks people's passwords with PAM for the management service, so it never runs as root |
| `hyperstor-ublk` | yes | Serves a volume as a Linux block device (`/dev/ublkbN`) through the kernel's ublk driver |
| `hyperstor-vhost` | yes, partly | Serves volumes to VMs over vhost-user-blk, one service per compute node; glidex attaches and detaches volumes through its control socket. Discard, adaptive polling and zero copy are planned |
| NVMe-oF target, S3 gateway, GPU-direct library | planned | Further front ends on the same object layer |

### Ports

| Port | Use |
|---|---|
| 8849 | Map protocol: storage nodes and clients fetch the map, heartbeats, leases |
| 8850 | Cluster API (gRPC-compatible, over mutual TLS) |
| 8851 | Raft between map service members (mutual TLS) |
| 8852 | Management service |
| 8853 | Storage node metrics and health |
| one per reactor | Data path: each device shard listens on its own TCP port, recorded in the map, and answers its peers' pings over UDP on the same port number |

**Why a port per reactor:** a connection lands on the thread that owns
the data, so no request is handed between threads. **Why pings over
UDP:** on TCP, one lost ping stalls the connection for the 200 ms
retransmission timeout, and the pings behind it with it; a lost UDP ping
costs only its own 100 ms slot ([failure detection](replication.md#failure-detection)).

## Crates

| Crate | Role | Depends on |
|---|---|---|
| `hyperstor-codec` | Fixed little-endian encoding and CRC32C, for disk and wire | — |
| `hyperstor-reactor` | The io_uring executor: tasks, TCP, timers, cross-thread channels | — |
| `hyperstor-engine` | The local storage engine: device superblock, shards, log, paged checkpoints, index, allocator, device back ends, the check for other systems' signatures | codec |
| `hyperstor-placement` | Pure functions: hashing and weighted rendezvous placement | — |
| `hyperstor-map` | The cluster map: nodes, devices and their classes and weights, pools, PGs and their splits, the blocklist, and recomputing placement | codec, placement |
| `hyperstor-proto` | Wire frames and messages | codec |
| `hyperstor-rpc` | Multiplexed request/response connections on the reactor | proto, reactor |
| `hyperstor-client` | The client library: request splitting, retries, volumes and leases, snapshots and clones, the host read cache | map, proto, rpc, reactor |
| `hyperstor-meta` | The map service: Raft member, failure detection, leases, weights and fullness, cluster API, join tokens, operations, trash, audit | map, client, rpc, api, pki |
| `hyperstor-daemon` | `hyperstord`: replication, recovery, scrub, stray copies, PG splits, metrics, the fault-injection rig | engine, client, meta, placement |
| `hyperstor-api` | Types, clients and server generated from the `hyperstor.v1` protobuf API | — |
| `hyperstor-pki` | Root and intermediate CAs, leaf certificates, TLS configurations | — |
| `hyperstor-ublk` | `hyperstor-ublk`: the ublk interface, queues as `uring_cmd`s on the reactor | client, reactor |
| `hyperstor-vhost` | `hyperstor-vhost`: vhost-user-blk on the reactor, the control socket, the state file and in-flight tracking for restarts | client, reactor, rust-vmm crates |
| `hyperstor-ctl`, `-install`, `-authd` | The administration binaries | api, pki, … |
| `hyperstor-mgmt` | The management service, with its Cedar policies, the React web UI (`ui/`, on `@glidex/ui-kit`) and its browser tests (`e2e/`) | api, pki, Cedar, … |
| `hyperstor-bench` | Benchmarks for every layer | most crates |
| `hyperstor-test` | End-to-end suite: installs clusters from the built binaries | pki |

The data path crates (codec, reactor, engine, placement, map, proto, rpc,
client, daemon, ublk, vhost) never use tokio. The map service runs Raft and the
cluster API on tokio threads beside its reactor, and passes map changes
across a channel.

## A write, end to end

1. **The client splits** the request at chunk boundaries (4 MiB chunks by
   default; offsets and lengths are multiples of 4 KiB). Each piece is
   sent in parallel.
2. **It routes each piece** without asking anyone: the chunk's placement
   group (PG) is a hash of the volume's lineage and the chunk index
   (while a pool splits its PGs, the map says whether a PG's new sibling
   serves it yet), and
   the PG's primary is the first device of its acting set in the client's
   copy of the map ([placement](replication.md#placement)).
3. **The primary checks** the request: it holds a current map lease, the
   PG is not fenced for a map change, the request's PG epoch matches its
   own, it is the primary, neither the client nor the volume lease the
   request names is blocklisted, and every block's CRC32C matches. A
   request at an old epoch is answered `StaleMap`; at a new epoch, the
   primary first makes the other copies match its own (peering).
4. **It replicates:** it gives the write the PG's next sequence number,
   sends it to every other copy and queues its own commit, with nothing
   in between, so every copy applies the PG's writes in the same order.
5. **Each copy commits** through its engine's **group commit**: the
   reactor gathers queued changes, writes their data to new extents and
   one log record, flushes once, and acknowledges them all. The reactor
   submits those writes and the flush on its io_uring and serves other
   requests while the device works ([data path](data-path.md#waiting-for-the-device)).
   A device past its `full` ratio refuses changes that take space: a new
   object, or a write to blocks the object doesn't hold yet. Overwrites
   and deletes go on.
6. **The primary answers** the client only when its own commit and every
   copy's have been acknowledged.
7. **On failure the client retries**: on a stale map, a moved primary,
   `Unavailable`, or a dropped connection, it refreshes its map and tries
   again, up to 20 times with a growing back-off (about two seconds in
   all). "No space" and "fenced" are not retried: the caller decides.

## A read, end to end

The client first looks in its **host read cache**: blocks of a version
that can no longer change (a snapshot, or a clone's parent) may already
be there ([volumes](volumes.md#the-host-read-cache)). Otherwise it sends
the read to the PG's primary, which serves it from its own engine while
it holds its map lease. Every block's CRC is checked on the device and
again by the client. If the primary's copy fails its check, the primary
fetches the object from another copy, replaces its own, and answers with
the good data. If its device takes longer than the read timeout (2 s),
the primary asks another acting copy to answer instead. Blocks never
written read as zeros.

**Why reads go to the primary only:** it is the one copy guaranteed to
have every acknowledged write. Reads from any copy are planned for
snapshots, which never change. A read of a volume with snapshots or
clones carries its **layers** (see
[snapshots](volumes.md#snapshots-and-clones)), and the primary reads each
block from the newest version that holds it.

## The map and its epochs

Every node and client holds a copy of the **cluster map**: nodes and
their devices, pools and their rules, and for every PG its **up** set
(where placement wants it), its **acting** set (the copies that have
every acknowledged write) and its primary. Each change bumps the
**map epoch**; each PG also has a **PG epoch**, the map epoch of the last
change to its acting set. A request carries both, so a node or client
with an older map is told so and fetches the new one, and no copy acts
on a write meant for an earlier arrangement. See
[the map service](replication.md#the-map-service).
