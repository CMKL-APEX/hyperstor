# Architecture

## Processes

```text
            clients (hyperstor-client, later vhost / NVMe-oF / S3)
                 │  map, leases                     │  data: one TCP port per reactor
                 ▼                                  ▼
   ┌───────────────────────────┐   heartbeats,  ┌──────────────────────────┐
   │ hyperstor-meta × 3 or 5   │◄──────────────►│ hyperstord (every node)   │
   │ map service, Raft         │  map leases,   │ one reactor per device    │
   │ cluster API (mTLS)        │  fences, pushes│ shard, engine on io_uring │
   └───────────────────────────┘                └──────────────────────────┘
         ▲  cluster API                              ▲   replication between
         │                                           │   storage nodes
   hyperstorctl, hyperstor-mgmt,                     └── (same data ports)
   hyperstor-install
```

| Process | Built | Role |
|---|---|---|
| `hyperstor-meta` | yes | The map service: holds the cluster map, detects failures, grants map leases, serves the cluster API. Three or five members replicate the map with Raft |
| `hyperstord` | yes | A storage node: one reactor thread per device shard, serving reads, writes, replication, recovery and scrub |
| `hyperstorctl` | yes | The administrator's CLI, over the cluster API |
| `hyperstor-install` | yes | Host checks and a convergent installer; joins hosts with single-use tokens |
| `hyperstor-mgmt` | partly | The management service: people log in with their host password, get roles (viewer, operator, storage admin, security admin) checked by Cedar policies, and call the cluster API through it; the web UI is planned |
| `hyperstor-authd` | yes | A small root helper that checks people's passwords with PAM for the management service, so it never runs as root |
| `hyperstor-ublk` | yes | Serves a volume as a Linux block device (`/dev/ublkbN`) through the kernel's ublk driver, for testing |
| `hyperstor-vhost` | planned | vhost-user-blk back end for VMs, one per host |
| NVMe-oF target, S3 gateway, GPU-direct library | planned | Further front ends on the same object layer |

### Ports

| Port | Use |
|---|---|
| 8849 | Map protocol: storage nodes and clients fetch the map, heartbeats, leases |
| 8850 | Cluster API (gRPC-compatible, over mutual TLS) |
| 8851 | Raft between map service members (mutual TLS) |
| 8852 | Management service |
| 8853 | Storage node metrics and health |
| one per reactor | Data path; each device shard listens on its own port, recorded in the map |

**Why a port per reactor:** a connection lands on the thread that owns
the data, so no request is handed between threads.

## Crates

| Crate | Role | Depends on |
|---|---|---|
| `hyperstor-codec` | Fixed little-endian encoding and CRC32C, for disk and wire | — |
| `hyperstor-reactor` | The io_uring executor: tasks, TCP, timers, cross-thread channels | — |
| `hyperstor-engine` | The local storage engine: shards, log, index, allocator, device back ends | codec |
| `hyperstor-placement` | Pure functions: hashing and weighted rendezvous placement | — |
| `hyperstor-map` | The cluster map: nodes, devices, pools, PGs, and recomputing placement | codec, placement |
| `hyperstor-proto` | Wire frames and messages | codec |
| `hyperstor-rpc` | Multiplexed request/response connections on the reactor | proto, reactor |
| `hyperstor-client` | The client library: request splitting, retries, volumes and leases | map, proto, rpc, reactor |
| `hyperstor-meta` | The map service: Raft member, failure detection, leases, cluster API, join tokens | map, client, rpc, api, pki |
| `hyperstor-daemon` | `hyperstord`: replication, recovery, scrub, metrics, the fault-injection rig | engine, client, meta, rpc |
| `hyperstor-api` | Types, clients and server generated from the `hyperstor.v1` protobuf API | — |
| `hyperstor-pki` | Root and intermediate CAs, leaf certificates, TLS configurations | — |
| `hyperstor-ublk` | `hyperstor-ublk`: the ublk interface, queues as `uring_cmd`s on the reactor | client, reactor |
| `hyperstor-ctl`, `-install`, `-mgmt`, `-authd` | The administration binaries | api, pki, … |
| `hyperstor-bench` | Benchmarks for every layer | most crates |
| `hyperstor-test` | End-to-end suite: installs clusters from the built binaries | pki |

The data path crates (codec, reactor, engine, placement, map, proto, rpc,
client, daemon) never use tokio. The map service runs Raft and the
cluster API on tokio threads beside its reactor, and passes map changes
across a channel.

## A write, end to end

1. **The client splits** the request at chunk boundaries (4 MiB chunks by
   default; offsets and lengths are multiples of 4 KiB). Each piece is
   sent in parallel.
2. **It routes each piece** without asking anyone: the chunk's placement
   group (PG) is a hash of the volume's lineage and the chunk index, and
   the PG's primary is the first device of its acting set in the client's
   copy of the map ([placement](replication.md#placement)).
3. **The primary checks** the request: it holds a current map lease, the
   PG is not fenced for a map change, the request's PG epoch matches its
   own, it is the primary, the client is not blocklisted, and every
   block's CRC32C matches. A request at an old epoch is answered
   `StaleMap`; at a new epoch, the primary first makes the other copies
   match its own (peering).
4. **It replicates:** it gives the write the PG's next sequence number,
   sends it to every other copy and queues its own commit, with nothing
   in between, so every copy applies the PG's writes in the same order.
5. **Each copy commits** through its engine's **group commit**: the
   reactor gathers queued changes, writes their data to new extents and
   one log record, flushes once, and acknowledges them all.
6. **The primary answers** the client only when its own commit and every
   copy's have been acknowledged.
7. **On failure the client retries**: on a stale map, a moved primary,
   `Unavailable`, or a dropped connection, it refreshes its map and tries
   again, up to 20 times with a growing back-off (about two seconds in
   all).

## A read, end to end

The client sends the read to the PG's primary, which serves it from its
own engine while it holds its map lease. Every block's CRC is checked
on the device and again by the client. If the primary's copy fails its
check, the primary fetches the object from another copy, replaces its
own, and answers with the good data. Blocks never written read as zeros.

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
