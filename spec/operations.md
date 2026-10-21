# Management, deployment and security

The management API and CLI, crates and units, roles and resources, upgrades, and security. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 16. Management API and CLI

administration.md §23 details the API (§23.5), the CLI (§23.6), the
management service and UI (§23.7), and people's access (§23.9).

- **gRPC** `hyperstor.v1` on port 8850, served by every
  `hyperstor-meta` member (metadata.md §4.2):
  - `ClusterService`: status, nodes, devices, health;
  - `PoolService`: create, update thresholds, delete when empty;
  - `VolumeService`: create, get, list, expand, delete, attach and detach
    (leases), snapshot, clone, rollback, flatten, QoS;
  - `SnapshotService`: list, delete, groups;
  - `HostService`: bare-metal host identities, exports, CHAP keys
    (the `hosts` table).
- **Authorization** by the client certificate:
  - the glidex control plane holds an admin identity;
  - each daemon and gateway holds a node identity;
  - a bare-metal GPU host holds a host identity, which may only attach
    the volumes exported to it.

  Tenants never reach the management API. In glidex, everything goes
  through glidex's own API and Cedar policies (glidex.md §17).
- **CLI:** `hyperstorctl` for standalone use and for operators (`cluster
  init`, `node add`, `device add`, `pool create`, `volume …`,
  `status`). In glidex, the usual path is `gxctl datastore` (glidex.md §17).
- **Health** is `ok`, `warn` (degraded PGs recovering, nearfull, a
  device slow) or `error` (PGs below `min_size`, full, scrub found
  damage that wasn't repaired), with the reasons listed.
- **Metrics** in Prometheus format on each daemon's admin port:
  - IOPS, bandwidth and latency histograms per reactor, per pool and
    per volume;
  - recovery and backfill progress;
  - device wear and capacity.

## 18. Deployment

### 18.1 Crates and binaries

| Crate | Builds |
|---|---|
| `hyperstor-proto` | wire types for D5 and the gRPC API |
| `hyperstor-reactor` | the io_uring reactor, executor, RDMA and buffer pools |
| `hyperstor-engine` | the local engine (engine.md §7) |
| `hyperstor-map` | the cluster map: pools, devices, placement |
| `hyperstor-meta` | the metadata service binary |
| `hyperstor-daemon` | `hyperstord` |
| `hyperstor-client` | the client library |
| `hyperstor-vhost` | the vhost-user-blk service |
| `hyperstor-gw` | the NVMe-oF gateway |
| `hyperstor-gpu` | `libhyperstor-gpu` and the two interposers (`cdylib`) |
| `hyperstor-ctl` | `hyperstorctl` |
| `hyperstor-kv` | the transactional key–value store (engine.md §7.5) |
| `hyperstor-s3` | the S3 gateway (s3.md §14) |
| `glidex-raftstore` (in the glidex repository) | the Raft store shared with glidex's control plane (D20), a pinned git dependency |

### 18.2 Units and privileges

- Units: `hyperstor-meta.service`, `hyperstord.service`,
  `hyperstor-vhost.service`, `hyperstor-gw.service`, and
  `hyperstor-mgmt.service` when standalone (administration.md §23.4). All run as the
  `hyperstor` user.
- `glidex-stord` (root) hands over devices, as on the netd pattern:
  - sets ownership of the NVMe generic character devices, the block
    devices of the other back end, and the RDMA devices;
  - writes `DeviceAllow=` drop-ins for exactly those devices.

  Without glidex, the installer writes udev rules instead.
- `LimitMEMLOCK=infinity` for RDMA registration. No capability beyond
  that is expected for passthrough I/O (G7).

### 18.3 Roles and resources (D7)

- `glidex-install --hyperstor storage|compute|both`, with
  `--hyperstor-devices` listing the devices (NVMe namespaces, SAS and
  SATA disks, hard disks with `:log=<flash partition>`) and
  `--hyperstor-meta` on server hosts.
- **Host-topology accounting:**
  - reactor cores join the node's reserved CPU list (H7) under the label
    `hyperstor`;
  - devices given to hyperstor are no longer assignable to VMs;
  - the storage NIC and its RDMA device are recorded as hyperstor's.
- **Networks.** Each node has a client network address and, optionally, a
  separate cluster network for replication and recovery. Each network
  is marked `trusted` or not (D25) and lists its transports (protocol.md §13.2). On
  InfiniBand, a partition key per network.

### 18.4 Upgrades

- Rolling, one node at a time: metadata service members first, then
  storage nodes, then clients.
- The D5 protocol speaks the current and previous versions. A node waits
  for its PGs to be clean (all in-sync, no recovery) before the next one
  is upgraded.
- Engine format changes are versioned in the superblock and applied at
  start.

## 19. Security

- mTLS everywhere on TCP, against hyperstor's own two-level CA, created
  by `hyperstorctl cluster init` in both modes (D53, D60;
  administration.md §23.8). Encryption on
  untrusted networks per D25.
- RDMA queue pairs are reliable-connected and are set up only over an
  authenticated TCP session. Only storage nodes start RDMA reads and
  writes, into buffers their clients named (D13).
- NVMe-oF hosts: allowlists and DH-HMAC-CHAP (frontends.md §11.2). NVMe/TCP TLS is
  later.
- A tenant's VM never reaches the storage network in the vhost path. In
  the GPU VM path (D18) it does, through its RDMA VF. That VF is put on a
  storage network or IB partition per tenant with host-topology §10's VF
  settings (VLAN, MAC, spoof check), and the host identity is per VM.
- Encryption at rest is later. The engine format reserves a key-id field
  per lineage so it can be added without reformatting.
