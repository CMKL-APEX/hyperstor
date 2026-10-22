# Overview

Status: **draft, implementation started.** The design is split by topic
(see [README.md](README.md)). Decisions are in
[decisions.md](decisions.md): D1–D19, D38–D55, D61–D68 and D69–D94 were settled
with the owner; D20–D37, D56–D60 and D66's merge rule were made while
drafting and are open to review. Host checks G1–G22 (milestones.md §22)
are still open. What is built, and the
decision points still open, are in
[implementation-notes.md](implementation-notes.md) and
[decision-log.md](decision-log.md).

Hyperstor is a sharded, distributed block store for glidex nodes. It
exports volumes over NVMe-oF (TCP, RoCEv2, InfiniBand), protects them by
replication or erasure coding, and supports snapshots, clones and the
regular datastore operations. It is written in pure Rust on Linux
io_uring, with no SPDK or DPDK dependency (D3).

To glidex it is one more datastore driver, `hyperstor`, with `shared`
topology ([storage.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/storage.md) S4, §11). glidex keeps its own records
(`Datastore`, `Disk`, `Image`); hyperstor owns everything below them.
Hyperstor also runs without glidex (D2).

Below the block layer, hyperstor is a general object layer with a
transactional key–value store on top (D34, D35). Block volumes are its
first client; an S3 gateway (D36) and a distributed filesystem (D37,
specified in [filesystem.md](filesystem.md)) come later on the same
layers.

**References to glidex.** Hyperstor is developed for
[glidex](https://github.com/CMKL-APEX/glidex) and integrates with it
(glidex.md §17). Names of documents that aren't hyperstor's
(`storage.md`, `clustering.md`, `host-topology.md`, `networking.md`,
`metering.md`, `security.md`, `tenancy.md`, `credentials.md`,
`hypervisors.md`, `architecture.md`, `images.md`) are glidex's, in its
[`spec/`](https://github.com/CMKL-APEX/glidex/tree/main/spec) directory,
and their section and decision numbers (`storage.md S16`, `clustering.md
D7`) refer to those documents.


## 1. Scope

- **In:**
  - volumes: create, delete, expand (shrink refused), thin provisioning;
  - replicated pools (2 or 3 copies) and erasure-coded pools (k+m);
  - crash-consistent snapshots, consistency groups, clones from a
    snapshot, rollback, flatten, snapshot deletion and garbage
    collection;
  - NVMe-oF export over TCP, RoCEv2 and InfiniBand;
  - a vhost-user-blk path for glidex VMs (D1);
  - GPU-direct storage: one network hop and zero copies into GPU memory,
    behind the cuFile (GDS) and hipFile APIs (D13–D18);
  - read-only multi-attach of snapshots (D18);
  - a host-wide read cache for data that never changes (D32);
  - GPU-direct storage inside glidex VMs, through a delegate of the
    host's lease (D33);
  - NVMe, SAS and SATA SSDs, and hard disks with their log on flash
    (D29);
  - a native data path over RoCEv2 and InfiniBand, without NVMe-oF (D30);
  - placement by failure domain, rebalancing, recovery, backfill, scrub;
  - per-volume QoS (IOPS and bandwidth limits);
  - the glidex driver, deployment by node role, and accounting of the
    cores, NICs and NVMe devices hyperstor takes;
  - the general object layer and the transactional key–value store
    (D34, D35). Their engine format is part of the first milestone, even
    though their first users come later.
- **Later, designed in:**
  - an S3 gateway, with S3-over-RDMA to GPU memory (D36, s3.md §14);
  - a distributed filesystem with POSIX metadata, mounted by VMs through
    virtio-fs and elsewhere through FUSE (D37, filesystem.md);
  - incremental snapshot diff export (backup);
  - PG splitting (doubling a pool's PG count, placement.md §5.1);
  - a libfabric transport, for Ultra Ethernet (UET) and other fabrics
    without RDMA verbs (D30);
  - Mellanox dynamically connected (DC) queue pairs (protocol.md §13.2), required by
    the S3-over-RDMA milestone (D36);
  - a CSI driver for Kubernetes clusters running on glidex VMs, with
    disks hot-plugged as vhost-user-blk (frontends.md §11.4);
  - NVMe emulation for guests over vfio-user (frontends.md §11.4);
  - the vhost client on a DPU (frontends.md §11.4);
  - lease hand-over during live migration (frontends.md §11.4);
  - NVMe/TCP TLS;
  - encryption at rest;
  - tenancy: tenant ids on containers, quotas, two-level fair share
    (D61, D62, tenants.md §24);
  - sites as a failure domain, asynchronous replication between
    clusters, a global namespace with range homes, cache sources and
    the federation view (D63–D68, federation.md §25).
- **Out:**
  - zoned devices (SMR hard disks, ZNS SSDs);
  - PGs stretched over a wide-area link (D63): sites in one cluster
    stay within 1 ms; farther apart means another cluster;
  - active-active writes to one volume or file from two clusters.

## 3. Architecture

```
   compute node                      bare-metal / non-glidex host
 ┌──────────────────────┐          ┌──────────────────────────────┐
 │ VM ─ virtio-blk      │          │ app ─ cuFile / hipFile        │
 │   │ vhost-user       │          │   │ libhyperstor-gpu (D13)    │
 │ hyperstor-vhost (D26)│          │   │       or                  │
 │   client library     │          │ kernel nvme-rdma / nvme-tcp   │
 └─────┬────────────────┘          └───┬───────────────┬──────────┘
       │ D5 protocol                   │ D5 (native)   │ NVMe-oF
       │ (TCP / RDMA)                  │               ▼
       │                               │        hyperstor-gw (D19)
       │                               │          client library
       ▼                               ▼               │
 ┌───────────────────────────────────────────────────────────────┐
 │ storage nodes: hyperstord                                     │
 │   reactors (one per core) · PG primaries and replicas         │
 │   local engine per NVMe namespace                             │
 └───────────────────────────────────────────────────────────────┘
 │   `.meta` pool: volume headers, leases, directories (D28)    │
 └───────────────────────────────────────────────────────────────┘
                 ▲ map, PG epochs, blocklist
 ┌───────────────┴───────────────────────────────────────────────┐
 │ hyperstor-meta ×3 or ×5 (Raft, D2, D20)                       │
 │   management API (every member; volume ops as a client)       │
 └───────────────────────────────────────────────────────────────┘
                 ▲ management API (gRPC, mTLS)
          glidex control plane (datastore driver, glidex.md §17) · hyperstorctl
          · hyperstor-mgmt (standalone UI and people, administration.md §23)
```

| Process | Runs on | Role |
|---|---|---|
| `hyperstor-meta` | 3 or 5 nodes; glidex server hosts by default | Cluster map, PG table, blocklist (metadata.md §4.1); the cluster API (operations.md §16, administration.md §23.5) |
| `hyperstor-mgmt` | standalone only; any nodes | Web UI, people's authentication and authorization, audit (administration.md §23.7, D50) |
| `hyperstord` | storage nodes | PG primary and replica roles; local engines (engine.md §6–replication.md §8) |
| `hyperstor-vhost` | compute nodes | vhost-user-blk for local VMs (frontends.md §11.1) |
| `hyperstor-gw` | storage nodes, or nodes chosen as gateways | NVMe-oF target (frontends.md §11.2) |
| `libhyperstor-gpu` | inside applications | Native GPU-direct client (gpu.md §12) |
| `hyperstor-s3` (later) | any nodes, behind a load balancer | S3 gateway (s3.md §14) |
| filesystem metadata servers and clients (later) | see filesystem.md | distributed filesystem (D37) |

The client library (`hyperstor-client`) is one crate used by
`hyperstor-vhost`, `hyperstor-gw` and `libhyperstor-gpu`. It holds a
cached cluster map, a volume lease, connections to storage reactors, and
the request splitting and retry logic (volumes.md §10.1).

**Ports:**

| Port | Protocol | Between |
|---|---|---|
| 8849/tcp | D5 protocol | map service: storage nodes and clients → `hyperstor-meta` (D69) |
| 8850/tcp | gRPC over mTLS (connectrpc, D51), management network | cluster API: `hyperstorctl`, glidex, `hyperstor-mgmt` → any `hyperstor-meta` member (D69) |
| 8852/tcp | HTTPS | browsers and `hyperstorctl` → `hyperstor-mgmt` (D59) |
| 8853/tcp | HTTPS, mTLS, management network | metrics and health, on each daemon (D58, D59) |
| 8851/tcp | Raft over mTLS | `hyperstor-meta` members |
| one port per reactor, recorded per device in the map (D39) | D5 protocol (TCP, and RDMA connection setup) | clients ↔ storage nodes, storage ↔ storage |
| 4420/tcp | NVMe/TCP and NVMe/RDMA (rdma-cm port) | hosts → `hyperstor-gw` |
| 8009/tcp | NVMe-oF discovery | hosts → `hyperstor-gw` |
