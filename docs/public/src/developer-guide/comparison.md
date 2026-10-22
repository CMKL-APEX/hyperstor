# Comparison with other systems

Hyperstor is early: today it has replicated pools, the replicated map
service, failure handling, recovery, scrub, volumes with leases,
snapshots, clones and consistency groups, used through a Rust client
library or, for testing, a ublk block device. Erasure coding, the front
ends for VMs and remote hosts, RDMA, GPU-direct, S3 and the filesystem
are designed but not built.
The systems below are mature products; this page compares **designs**,
and says where Hyperstor is behind.

Product facts are as of October 2026, from the sources in
[References](references.md#products). Check the vendors' own pages for
anything you rely on.

## At a glance

| System | License | Data path | Protection | Metadata | Recovery of a returning copy | Front ends | GPU-direct (NVIDIA GDS list) |
|---|---|---|---|---|---|---|---|
| **Hyperstor** | MIT or Apache-2.0 | Own io_uring reactor, thread per core, pure Rust; kernel NVMe driver | 2–3 copies; erasure coding planned; one-copy cache pools | Raft map service (3 or 5 members); volume headers in a replicated pool | Compare content digests, send what differs | Client library, ublk (testing); vhost-user-blk, NVMe-oF, S3, filesystem planned | Planned: dma-buf and RDMA behind cuFile and hipFile |
| **Ceph** (v20 Tentacle) | LGPL | BlueStore on raw devices; the Seastar-based Crimson OSD is a technology preview | Replication, erasure coding | Monitors (Paxos); CRUSH placement | PG log, then backfill | RBD, NVMe-oF gateway, S3 (RGW), CephFS | Not listed |
| **Longhorn** (1.9) | Apache-2.0 | One engine per volume; a V2 engine on SPDK (experimental) | Replicas per volume | Kubernetes resources | Replica rebuild | iSCSI, ublk; NVMe/TCP with V2 | Not listed |
| **OpenEBS Mayastor** | Apache-2.0 | SPDK, in Rust; huge pages and polling cores | Replicas per volume | etcd | Replica rebuild | NVMe/TCP | Not listed |
| **LINSTOR / DRBD 9** | GPL | Kernel module | Synchronous replication of whole devices | LINSTOR controller; DRBD quorum with tiebreakers | Resync of changed blocks | Kernel block device | Not listed |
| **Portworx** (3.5) | Proprietary | — | Synchronous replication | Internal key–value store (etcd); I/O goes on while it is down | — | Kubernetes volumes | Not listed |
| **Lightbits** | Proprietary | Userspace NVMe/TCP target with a global flash translation layer | Replication, erasure coding across SSDs | — | — | NVMe/TCP (Lightbits co-created the protocol) | Not listed |
| **simplyblock** | Proprietary | SPDK | Distributed erasure coding, optional replication | — | — | NVMe/TCP, NVMe/RoCE | Not listed |
| **DAOS** (2.x) | BSD-2-Clause-Patent | SPDK for NVMe, libfabric for the network | Replication, erasure coding | Raft-replicated pool and container services | Rebuild | libdaos, POSIX (dfuse), MPI-IO | Not listed |
| **WEKA** | Proprietary | DPDK kernel bypass, own protocol | Distributed erasure coding | Distributed, no central metadata server | Distributed rebuild | POSIX client, NFS, SMB, S3 | Listed |
| **VAST Data** | Proprietary | Stateless servers reach shared NVMe enclosures over NVMe-oF | Wide locally decodable codes | Global transactional store in storage-class memory | Rebuild reads a fraction of a stripe | NFS (with RDMA), SMB, S3, NVMe/TCP | Listed |
| **MinIO AIStor** | AGPL-3.0 community edition; commercial AIStor | Go, HTTP; RDMA in AIStor | Per-object erasure coding | No metadata server | Healing | S3, and S3 over RDMA | S3 over RDMA into GPU memory |

"—" means the vendor's public material we found doesn't say.

## System by system

### Ceph

The closest relative: Hyperstor shares its model of placement groups, a
primary per PG, map epochs, `min_size`, header objects for volumes and a
blocklist for exclusive locks.

- **Different pieces:** rendezvous hashing instead of CRUSH, Raft instead
  of Paxos monitors, and content digests instead of the PG log
  ([why](replication.md#peering-and-recovery-by-content-digests)).
- **Map leases:** a cut-off primary stops within one 0.6 s lease, and the
  same lease makes primary reads safe.
- **The data path Ceph is moving to:** Crimson rebuilds the OSD as
  thread-per-core on Seastar. Hyperstor started there, on io_uring
  without Seastar or SPDK.
- **Snapshots:** Hyperstor deletes a snapshot by handing its blocks to the
  newer version in one log record, without copying data, and resets a
  volume to its newest snapshot in place, which suits VMs reset after
  every task.
- **Behind:** Ceph has erasure coding, S3, a filesystem,
  multi-site replication and years of production at scale. Hyperstor has
  none of these yet.

### Longhorn and OpenEBS Mayastor

Kubernetes-native block storage. Both replicate whole volumes:
Longhorn with an engine per volume, Mayastor through a "nexus" on SPDK.

- **Sharding:** Hyperstor cuts every volume into chunks over all PGs, so
  one volume spreads its load and its recovery over the whole cluster.
- **Kernel bypass:** Mayastor and Longhorn's V2 engine take devices from
  the kernel with SPDK and poll whole cores; Hyperstor keeps the kernel
  driver and sleeps when idle, and has yet to show how close it gets
  to SPDK on NVMe.
- **Behind:** both ship NVMe/TCP, Kubernetes integration, backups and a
  UI today.

### LINSTOR and DRBD

DRBD replicates a block device in the kernel; LINSTOR places resources
across nodes. Very low latency on two or three nodes, and decades of
production.

- **Different scale model:** DRBD's unit is a whole device on a fixed set
  of nodes; Hyperstor's is a 4 MiB chunk placed by hash, so data and
  recovery spread over every node.
- **Fencing:** DRBD uses quorum and tiebreakers; Hyperstor uses map
  leases for primaries and volume leases with a blocklist for clients.

### Portworx

Enterprise Kubernetes storage. Its internal key–value store can be down
while I/O goes on, with volume operations blocked. Hyperstor's map
service is off the I/O path too, but storage nodes stop after one map
lease without it: that is the price of bounding a stale primary, which
is why the map service runs as three or five members.

### Lightbits and simplyblock

NVMe/TCP block storage: Lightbits with its own userspace target,
simplyblock on SPDK with erasure coding first. Both ship what Hyperstor
plans: an NVMe-oF target and erasure-coded pools. Hyperstor's
difference is the same object layer under VMs (vhost-user-blk, without
NVMe-oF in between), GPU-direct reads, S3 and a filesystem.

### DAOS

An object store for HPC and AI, with Raft-replicated services and
key–value semantics close to Hyperstor's object layer. It depends on
SPDK and libfabric, and targets POSIX and MPI-IO. Hyperstor is block
first for virtual machines, on io_uring and RDMA verbs.

### WEKA and VAST Data

Parallel file and object systems for AI, both on NVIDIA's GPUDirect
Storage list. WEKA bypasses the kernel with DPDK; VAST separates
stateless servers from shared flash enclosures and protects data with
very wide codes.

- **GPU-direct:** Hyperstor plans to keep the cuFile and hipFile APIs but
  move data by RDMA straight into GPU memory registered through dma-buf,
  which works for NVIDIA and AMD GPUs alike, in one hop to the storage
  node. Unmodified GDS programs keep a kernel NVMe-oF path. None of this
  is built.
- **Behind:** both are qualified and in production for GPU clusters.

### MinIO AIStor

S3 object storage. AIStor ships S3 over RDMA (the `x-amz-rdma-token`
extension) into GPU memory. Hyperstor plans the same extension on its S3
gateway, with the transfer made by the storage nodes that hold the data,
so the gateway stays off the data path.

## What sets Hyperstor apart

| Trait | How |
|---|---|
| **Kernel-friendly speed** | Pure Rust on io_uring, one thread per core, one port per reactor; no SPDK, DPDK or huge pages, devices stay visible to the host, idle cores sleep |
| **Recovery without a log** | Content digests from the index drive peering, delta recovery and scrub: no extra persisted state, 96× less sent on a rejoin after 1 % changed |
| **Bounded stale primaries** | Map leases fence a cut-off primary in under a second, with no synchronized clocks, and double as read leases |
| **One writer per volume, enforced** | Volume leases and a cluster-wide blocklist; preemption for a scheduler that moves VMs |
| **Placement that moves little** | Weighted rendezvous hashing over failure domains, by lineage so clones share placement |
| **Built for AI platforms** | Cache pools for recomputable data such as offloaded KV caches; GPU-direct for NVIDIA and AMD behind the standard APIs; separate indexes tuned for block maps and for metadata (planned) |
| **Tested against models** | Crash tests with torn writes, and an in-process fault rig with partitions and hung devices, replaying any failure exactly |
| **Open** | MIT or Apache-2.0 |
