# Hyperstor design

Status: **draft, implementation started.** Decisions D1–D19 were settled
with the user; D20–D37 were made while drafting and are open to review.
Host checks G1–G12 (§22) are still open. Built so far: HS0, HS1 without
Raft, and the failure-detection and backfill part of HS2; see
[implementation-notes.md](implementation-notes.md) for what was built,
the deviations it proposes for this document (I14, I19), and what is
left.

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
(§17). Document names other than this one and filesystem.md
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
  - an S3 gateway, with S3-over-RDMA to GPU memory (D36, §14);
  - a distributed filesystem with POSIX metadata, mounted by VMs through
    virtio-fs and elsewhere through FUSE (D37, filesystem.md);
  - incremental snapshot diff export (backup);
  - PG splitting (doubling a pool's PG count, §5.1);
  - a libfabric transport, for Ultra Ethernet (UET) and other fabrics
    without RDMA verbs (D30);
  - Mellanox dynamically connected (DC) queue pairs (§13.2), required by
    the S3-over-RDMA milestone (D36);
  - a CSI driver for Kubernetes clusters running on glidex VMs, with
    disks hot-plugged as vhost-user-blk (§11.4);
  - NVMe emulation for guests over vfio-user (§11.4);
  - the vhost client on a DPU (§11.4);
  - lease hand-over during live migration (§11.4);
  - NVMe/TCP TLS;
  - encryption at rest.
- **Out:**
  - zoned devices (SMR hard disks, ZNS SSDs);
  - stretched clusters across sites.

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | **glidex VMs reach volumes through vhost-user-blk.** A hyperstor client on each hypervisor node serves the VM from shared guest memory and sends each chunk's I/O straight to its PG primary. NVMe-oF is exported for bare-metal and non-glidex hosts. | One network hop and no gateway. Works with Cloud Hypervisor and QEMU alike. A kernel NVMe-oF initiator would need the target to forward to the shard primary (two hops) and adds kernel overhead. |
| D2 | **Hyperstor runs its own metadata service** (`hyperstor-meta`): a Raft group of 3–5 members holding the **cluster map only**: nodes, devices, pools, the PG table and epochs, host exports and the blocklist. Volumes, snapshots and leases are not in Raft (D28). | Storage I/O and recovery keep working while the glidex control plane is down or busy, and map changes stay out of glidex's Raft. Hyperstor can be deployed without glidex. |
| D3 | **Pure Rust on io_uring; no SPDK or DPDK.**<br>- **Data path:** one reactor per pinned core, our own loop on the `io-uring` crate. Each pass reaps io_uring completions (NVMe passthrough through `uring_cmd` with polled queues, sockets with zero-copy send, multishot receive and provided buffers), RDMA completion queues (rdma-core verbs), vhost-user virtqueues, and rings from the other cores. A small in-house executor lets request state machines be `async fn`. No third-party async runtime (monoio, compio, glommio).<br>- **Control path:** tokio, as in the rest of the workspace: the metadata Raft, peering, recovery scheduling, the management API and the glidex driver.<br>- **Client library** (`libhyperstor-gpu`, D13): runs its own reactor threads and never assumes a runtime in the host process.<br>- **vhost-user-blk:** the rust-vmm crates (`vhost-user-backend`, `virtio-queue`, `vm-memory`), driven from the reactor. | One language, one build, no FFI boundary in the data path. NVMe devices stay on the kernel driver, so nothing is bound to vfio, there's no second DPDK process next to OVS-DPDK, and idle cores aren't burned by polling. The library can be embedded in applications, which SPDK can't (D13). Owning the loop lets RDMA and virtqueue polling sit next to io_uring, which a general runtime makes awkward. The cost is some per-core speed and an NVMe-oF target of our own (D19). Benchmark gate G5 checks it. |
| D4 | **The local engine is hyperstor's own Rust extent store** on devices it opens exclusively (D29 for the device kinds): chunk versions keyed by lineage, chunk, branch and snapshot epoch (§7), a CRC32C per 4 KiB block, and a stripe log for EC. | Snapshots and EC fit the sharded model directly. A per-device engine with its own snapshots (LVM-thin, SPDK lvol) would need every cluster snapshot coordinated across every replica, and EC doesn't fit it. |
| D5 | **Traffic between nodes uses a custom framed protocol** over io_uring TCP sockets and RDMA verbs (RoCEv2, IB). Each message carries the epoch, PG, chunk version and checksum (§13). | Fencing, versioning and recovery are expressed directly. NVMe-oF throughout would have to carry them in reservations and vendor-specific commands. |
| D6 | **EC pools take writes through a replicated log** (3 copies) and destage full stripes to k+m in the background. A write that covers whole stripes skips the log and goes straight to k+m (§8.6). | Random-write VM disks can live on EC with replica-like write latency, and there is no read-modify-write in the foreground. Checkpoints and dataset ingest are large sequential writes; through the log they would cross the network and the media twice. |
| D7 | **A node's role is storage, compute or both.** Hyperstor's reactor cores and NVMe namespaces are carved out through host-topology accounting ([host-topology.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/host-topology.md) H7). `glidex-stord` hands the namespaces and the RDMA devices to the `hyperstor` user. NICs stay on their kernel drivers, and hugepages are needed only for the guest memory that vhost-user shares (H19). | Hyperconverged and dedicated-storage sites both work, and VMs never compete with storage reactors. |
| D8 | **Each PG has one primary per PG epoch.** Replicas reject writes from an older epoch. Reads go to the primary while it holds a read lease from its replicas (D23). A write is acknowledged once every in-sync copy has persisted it. | Linearizable reads and writes without quorum reads. The epoch fences a primary that was partitioned. |
| D9 | **Volumes are split into 4 MiB chunks** (the size is fixed per pool at creation). Chunks map to PGs by hash; PGs map to replica sets by rendezvous hashing over failure domains (host, then rack). | Adding or removing a node moves only the PGs whose placement changes. |
| D10 | **Snapshots are redirect-on-write, by snapshot epoch.** A write after a snapshot creates a new chunk version holding only the blocks written; older versions stay as they are. Consistency groups cover a VM's disks. | Taking a snapshot only bumps an epoch in metadata; nothing is copied, then or later. |
| D11 | **Volume leases replace glidex's volume lock** for hyperstor datastores (storage.md S16, §10 item 4). | A node that was partitioned and kept running is blocklisted before anyone else gets the lease, and every storage node refuses it (§9). |
| D12 | **Hyperstor has its own repository** (`CMKL-APEX/hyperstor`), with the spec in `spec/` and the code in `crates/hyperstor-*`, laid out like glidex. The glidex datastore driver (§17) lives in glidex. | Hyperstor runs without glidex (D2) and has its own release cycle, while following glidex's conventions and review flow. |
| D13 | **GPU-direct storage takes a native path.** The hyperstor client (D1) also ships as a library, `libhyperstor-gpu` (Rust, C ABI), loaded into the application. It registers GPU memory with the RDMA NIC through dma-buf (`cuMemGetHandleForAddressRange` / `hipMemGetHandleForAddressRange`, then `ibv_reg_dmabuf_mr`) and sends each chunk's request straight to the node that serves it. That node moves the data by RDMA directly to or from GPU memory: it writes into GPU memory for reads and reads from it for writes. | One network hop and no copy through host memory. A kernel NVMe-oF initiator would make the target a gateway that forwards every chunk (two hops). dma-buf registration is the same for NVIDIA and AMD. Only storage nodes ever get remote keys (rkeys): a client never receives a key to a storage node's memory. |
| D14 | **Applications keep the cuFile and hipFile APIs.** Both define a userspace-filesystem handle (`CU_FILE_HANDLE_TYPE_USERSPACE_FS` / `hipFileHandleTypeUserspaceFS`) whose `fs_ops` read and write callbacks the library calls. Hyperstor provides those callbacks. A thin interposer (`libhyperstor-cufile`, `libhyperstor-hipfile`) turns an ordinary `OPAQUE_FD` registration of a file on a hyperstor volume into a userspace-FS registration, and passes every other call through to the vendor library unchanged. | PyTorch, KvikIO, DALI and similar callers register plain file descriptors and won't change. The interposer catches only hyperstor files. Buffer registration, streams and batch calls remain the vendor's. |
| D15 | **hipFile needs an upstream change; until it lands, the interposer serves the I/O itself.** On AMD, hipFile rejects userspace-FS handles (`hipFileIONotSupported`) and has only its fastpath (kernel P2PDMA) and fallback backends. Its internal `Backend` interface (`score`/`io`) fits one more backend. hipFile is MIT-licensed (`ROCm/rocm-systems`, `projects/hipfile`), so we contribute userspace-FS support there. | We don't fork a vendor library. The interposer keeps AMD working while the change goes through review. |
| D16 | **Files are mapped to volume blocks on the client.** The volume carries ext4 or xfs, mounted through the kernel initiator. For an `O_DIRECT` file, the client maps file offsets to volume blocks with `FIEMAP`. Read extents that are mapped, written and stable go native; so do overwrites of written extents that aren't shared. Everything else (holes, unwritten or delalloc extents, reflinked extents, appends) goes through the kernel path (D17). The client holds a read lease on the file so a change from another process breaks it. Raw volume handles are also offered, for callers that manage offsets themselves (checkpoint engines, KV caches). | GDS and hipFile workloads are files. The filesystem stays the only thing that allocates blocks. Model weights, datasets and checkpoint restores are reads of stable extents, which is the common case. |
| D17 | **The kernel path stays:** NVMe-oF over RDMA to the kernel `nvme-rdma` initiator, multipath across several gateways (all ANA optimized). Unmodified GDS (P2PDMA mode or nvidia-fs) and hipFile fastpath work on it without the interposer, with one extra hop. NVMe/TCP runs in their fallback (bounce-buffer) mode. | It is the fallback for whatever D16 doesn't take, and the path for hosts where the interposer isn't installed. Neither library does zero-copy over TCP. |
| D18 | **GPU-direct storage comes first on bare-metal hosts, then on glidex VMs.** A GPU VM then gets the GPU and an RDMA VF passed through, placed under the same PCIe switch (host-topology device pools). Its filesystem stays on its ordinary vhost-user disk, and `libhyperstor-gpu` in the guest reads and writes the same volume over the VF as a delegate of the host's lease (D33), so the guest needs no NVMe-oF. Snapshots can be **attached read-only by many hosts** (`ro,norecovery` mounts), and their reads may be served by any replica because a snapshot never changes. | Bare metal carries no risk from P2P inside a guest (G2). Read-only multi-attach covers the main GPU use case: one dataset or one set of model weights on every GPU node. |
| D19 | **Hyperstor has its own NVMe-oF target in Rust**, inside the gateway process: NVMe/TCP first, then RDMA (rdma-cm and verbs). It provides discovery, a subsystem per volume, ANA groups, reservations, host NQN allowlists and DH-HMAC-CHAP. Until it is ready, a stopgap exports volumes through ublk and the kernel's `nvmet` (`nvmet-tcp`, `nvmet-rdma`). | The gateway routes chunks to primaries in the same process that terminates NVMe-oF, with no extra trip through the kernel. Because of the ublk stopgap, bare-metal hosts and GDS and hipFile on the kernel path (D17) don't wait for the target. |
| D20 | **`hyperstor-meta` reuses glidex's Raft stack:** `openraft` (the same pinned version), a ReDB state machine, and the leader-computes, write-set apply of clustering.md D2/D3. The shared code moves out of glidex's control plane into a crate in the glidex repository (`glidex-raftstore`), which hyperstor uses as a git dependency pinned to a revision. | One consensus implementation to test and upgrade. Its rules (D18 there: a server that lost its log never rejoins as the same voter) carry over. |
| D21 | **Chunks are placed by lineage, not by volume.** A volume and every clone made from its snapshots share a lineage id, and a chunk's PG is `hash(lineage, chunk index)`. | A clone's unwritten blocks are read from its parent's versions of the same chunk. With the same lineage, those versions sit in the same PG on the same devices, so reading through to the parent never crosses the network. |
| D22 | **Every write is atomic in the engine.** Large or unallocated writes go to new extents and are committed by a log record. Small overwrites of unshared extents are written into the log record, acknowledged, and applied in place later. Shared extents (held by a snapshot) are never overwritten. | No torn writes, even across a crash, so replicas never disagree about a half-written range, and recovery only replays whole operations. Small random writes don't fragment the layout. |
| D23 | **The metadata service is not on the I/O path, and not on the volume-operation path either.** A primary's read lease comes from its PG's replicas, not from `hyperstor-meta`. Volume leases are renewed with the volume's header object (D28), not with Raft. | A metadata outage stops map changes (PG failover, rebalancing, forced lease takeovers) but not I/O, and not ordinary volume operations. |
| D24 | **Taking a lease away from a holder that hasn't released it is a blocklist round.** The metadata service adds the holder's client instance to the blocklist in one Raft write and pushes the map. The new holder starts only when every storage node has acknowledged that map epoch or has lost its own map lease. A graceful release or hand-off needs no round at all. | After the round, no storage node accepts a request from the old holder, which is what makes D11 hold for a node that was partitioned and kept its VM running. Raft sees only forced takeovers, which are rare. |
| D25 | **The data path is authenticated by mTLS; encryption is per network.** Every TCP connection starts with a TLS 1.3 handshake against the cluster CA (glidex-tls). On networks marked `trusted`, the session then drops to plaintext framed by CRC32C. Elsewhere the session stays TLS, with kernel TLS offload where available. RDMA queue pairs are set up only over such an authenticated TCP session. | Storage fabrics are usually dedicated, and encrypting every byte there costs reactor cores. A network that isn't dedicated gets encryption with no other change. |
| D26 | **One vhost-user service per compute node** (`hyperstor-vhost`), serving every VM on that node. A VM's queues are pinned to reactors on the VM's NUMA node. | One set of reactor cores and RDMA connections per node rather than per VM. The hypervisor reconnects after a restart of the service (G6). |
| D27 | **Pools have fullness thresholds:** `nearfull` (85%) warns, `full` (95%) refuses new allocations, while overwrites of allocated blocks, deletes and snapshot deletes keep working. | A thin pool that runs out must still let the operator free space. |
| D28 | **Volume metadata lives in header objects in a replicated metadata pool** (`.meta`), not in Raft. Each volume has one header object (size, lineage, branch, head epoch, snapshots, lease, QoS, space), placed by hash like any chunk and changed by compare-and-set through its PG primary. Lineage trees, pool directories and snapshot groups are objects there too. The header's primary also runs the volume's background jobs. | Volume operations and lease renewals then scale with the number of PGs, across every storage node, instead of going through one Raft leader. Raft keeps only what changes with hardware and failures. This is the pattern Ceph uses (RBD headers, exclusive locks and a monitor blocklist), proven at scale. |
| D29 | **The engine has two device back ends behind one interface.** NVMe namespaces use `uring_cmd` passthrough on the generic character device. Every other block device (SAS and SATA SSDs, hard disks, NVMe without passthrough) uses io_uring `O_DIRECT` reads and writes on the block device. A hard disk is only accepted with a **flash log device**: the log and the index go on an SSD or NVMe partition shared by up to eight hard disks, and only data extents go on the disk. | Sites have SAS SSDs and dense hard-disk shelves as well as NVMe, and dev rigs often have no NVMe at all. The engine's design (D22) needs nothing NVMe-specific; only the submission path differs. On a hard disk, the log's small synchronous writes and the index's random reads would cost a seek each, while D22 already sends small writes through the log, so putting the log on flash removes nearly all of the disk's random writes. |
| D30 | **The RDMA transport is rdma-core verbs, behind hyperstor's own transport layer; neither UCX nor libfabric is used now.** The layer offers TCP and verbs (RoCEv2, InfiniBand). A libfabric provider is added later, only for a fabric without verbs that a site actually has: Ultra Ethernet (UET, whose software interface *is* libfabric), AWS EFA, HPE Slingshot. To keep that door open, the D5 protocol never relies on delivery order between messages (§13.2). | Verbs gives exact control over what hyperstor depends on: server-initiated RDMA, dma-buf registration of GPU memory (D13), shared receive queues and DC queue pairs. UCX's strengths (tag matching, active messages, its own GPU memory handling, automatic transport choice) overlap with what hyperstor does itself, and it is a large C runtime configured by environment variables. Loaded into applications through `libhyperstor-gpu`, it would clash with the UCX that PyTorch, Dask and MPI applications often bundle. libfabric reaches fabrics verbs can't, but on RoCE and InfiniBand it only adds a layer. `libibverbs` is the system library everything else sits on, UCX included. |
| D31 | **NCCL and RCCL are not used, and hyperstor stays off the GPU compute rails by default.** Each RDMA port in the map has a role: `storage` (the default for hyperstor), `compute` (a GPU rail used by NCCL or RCCL) or `converged`. Hyperstor uses `storage` ports; on `converged` ports, and on `compute` ports a host opts into for GPU-direct, it uses its own traffic class (RoCE DSCP and PFC priority, InfiniBand service level). | NCCL and RCCL serve the GPUs of one job with a fixed set of ranks and abort on a failure; storage is client–server with clients coming and going, and needs failover. Both already sit on the same verbs and GPU memory registration hyperstor uses, so nothing is gained by going through them. What matters is that checkpoint bursts and collectives don't slow each other down: separate NICs where the cluster has them, a separate traffic class where it doesn't. The same goes for NVIDIA SHARP (in-network reduction on InfiniBand switches, used by NCCL and MPI): hyperstor doesn't use it, and keeps its service level apart from SHARP's (§13.2). |
| D32 | **Each compute node caches immutable data for all its VMs.** `hyperstor-vhost` keeps a read cache keyed by `(lineage, chunk, branch, epoch)` of versions that can't change: snapshot versions, and the parent versions clones read through to. It is in memory, with an optional partition of a local NVMe device as a second tier. Head versions, which change, are never cached. | Such data needs no invalidation, ever, so the cache is safe without any protocol. It serves the common heavy readers on a virtualized host: many VMs booting linked clones of one image, and many VMs or pods loading the same model weights or dataset snapshot. |
| D33 | **A lease can have delegates.** The holder of a volume's `rw` lease may add client instances that act under it: the guest library of D18 under the host's `hyperstor-vhost`, or `libhyperstor-gpu` under the gateway that serves the host's kernel initiator (D17). Delegates are recorded in the header with the lease, renew nothing themselves, and are blocklisted together with their holder. A guest gets its delegate credential at run time over virtio-vsock from `hyperstor-vhost`, which knows the VM by its vsock CID. | One volume keeps one writer identity while two paths use it: the VM's virtio disk and its RDMA VF, or a host's kernel initiator and its GPU library. The CID is set by the host and can't be forged by the guest, so no long-lived secret is stored in the VM. |
| D34 | **The engine stores general objects; volumes are one kind.** An object is named within a namespace of its pool and has versioned, sparse data, small attributes, and an **ordered key–value table** of its own. Operations on one object are atomic as a list (compare-and-set on the version, read, write a range, put, delete and delete a range of keys). Chunk versions (§7.2), `.meta` headers (D28), KV ranges (D35), S3 data and file data are all such objects. | S3 needs named objects and sorted bucket indexes, and a filesystem needs directory tables. All of them need the same placement, replication, EC, peering and scrub that volumes already have. Designing the format for general objects now avoids an engine format change when they arrive. Ceph's object layer (RADOS with omap) shows the model works. |
| D35 | **A transactional key–value store (`hyperstor-kv`) runs on the object layer.** Its ordered keyspace is split into ranges; each range is one object whose key–value table holds the range's keys, and ranges split and merge as they grow and shrink. Transactions are multi-key and multi-range with snapshot isolation, in the Percolator style: two-phase commit with a primary lock, and timestamps from an oracle on the `hyperstor-meta` leader. A transaction that touches one range commits in one step. | The filesystem's metadata operations (create, rename across directories, unlink) change several keys in different places atomically, and so do S3's multipart completion and versioned buckets. Building it on objects reuses PG replication and failover instead of adding a second replication system, and keeps it in Rust. The oracle hands out timestamps from memory and writes only a reservation to Raft every few seconds, so Raft still sees no per-operation traffic (D28). |
| D36 | **S3 is served natively by a stateless gateway** (`hyperstor-s3`), with bucket indexes in `hyperstor-kv` and object data in chunk objects placed by hash. It includes **S3-over-RDMA**: the `x-amz-rdma-token` header protocol used by NVIDIA cuObject and AMD hipObject. For such requests the gateway passes the token to the storage nodes holding the object's chunks, which move the data straight to or from the client's GPU or host memory by RDMA. The milestone needs DC queue pairs, because cuObject clients only accept DC. | The gateway scales out with no state of its own. Large immutable S3 objects suit EC pools with full-stripe writes (D6). Training stacks read datasets and write checkpoints through S3, and both GPU vendors now ship clients for S3-over-RDMA; serving it from the storage nodes gives one network hop with the gateway out of the data path. NVIDIA's server library is under the CUDA EULA, so the protocol is implemented in Rust (G12). |
| D37 | **Hyperstor gets a native distributed filesystem**, specified in filesystem.md. Stateless metadata servers keep directories and inodes in `hyperstor-kv` (D35), and file data lives in chunk objects. VMs mount it through virtio-fs served by `hyperstor-vhost`, so the guest needs no storage network. Bare-metal and Kubernetes nodes mount it through FUSE over io_uring. GPU I/O goes through `libhyperstor-gpu` as a cuFile and hipFile userspace filesystem. Metadata operations are atomic and immediately visible everywhere; file data has close-to-open consistency, and `O_DIRECT` makes writes visible at once. | Shared mounts among VMs on different hosts, and datasets and checkpoints shared by many training nodes, need more than one writer per volume. Stateless metadata servers on a transactional store (the DeepSeek 3FS model) avoid the subtree partitioning and journal replay of stateful metadata servers. No kernel module to maintain. Close-to-open is what training and checkpoint workloads need, without the cost of data leases. |

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
          glidex control plane (datastore driver, §17) · hyperstorctl
```

| Process | Runs on | Role |
|---|---|---|
| `hyperstor-meta` | 3 or 5 nodes; glidex server hosts by default | Cluster map, PG table, blocklist (§4.1); the management API (§16) |
| `hyperstord` | storage nodes | PG primary and replica roles; local engines (§6–§8) |
| `hyperstor-vhost` | compute nodes | vhost-user-blk for local VMs (§11.1) |
| `hyperstor-gw` | storage nodes, or nodes chosen as gateways | NVMe-oF target (§11.2) |
| `libhyperstor-gpu` | inside applications | Native GPU-direct client (§12) |
| `hyperstor-s3` (later) | any nodes, behind a load balancer | S3 gateway (§14) |
| filesystem metadata servers and clients (later) | see filesystem.md | distributed filesystem (D37) |

The client library (`hyperstor-client`) is one crate used by
`hyperstor-vhost`, `hyperstor-gw` and `libhyperstor-gpu`. It holds a
cached cluster map, a volume lease, connections to storage reactors, and
the request splitting and retry logic (§10.1).

**Ports:**

| Port | Protocol | Between |
|---|---|---|
| 8850/tcp | gRPC over mTLS | management API and map watch: clients, daemons, glidex → any `hyperstor-meta` member |
| 8851/tcp | Raft over mTLS | `hyperstor-meta` members |
| 8852/tcp | D5 protocol (TCP, and RDMA connection setup) | clients ↔ storage nodes, storage ↔ storage |
| 4420/tcp | NVMe/TCP and NVMe/RDMA (rdma-cm port) | hosts → `hyperstor-gw` |
| 8009/tcp | NVMe-oF discovery | hosts → `hyperstor-gw` |

## 4. Metadata

Metadata is kept at three levels, each ordered by a different leader, so
that no single node sees every change (D28):

| Level | Holds | Ordered by | Changes with |
|---|---|---|---|
| Cluster map | nodes, devices, pools, the PG table and epochs, host exports, the blocklist | the `hyperstor-meta` Raft leader | hardware changes and failures |
| Volume metadata (the `.meta` pool) | volume headers, lineage trees, pool directories, snapshot groups | the primary of the PG that holds each object | volume operations, lease renewals |
| Block metadata (engines, §7) | extent maps, versions, checksums, allocation | the primary of each data PG | I/O |

### 4.1 The cluster map (Raft)

- `hyperstor-meta` is a Raft group of 3 or 5 voters, on `openraft` with a
  ReDB state machine (D20). Writes follow clustering.md D2 and D3: the
  leader runs the change against its applied state, and the Raft entry
  is the resulting write set.
- **Tables:**

| Table | Key | Holds |
|---|---|---|
| `nodes` | node id | networks (§13.2: for each, its address, its transports and their parameters), role, failure domain path (`rack/host`), reactor count, state |
| `devices` | device id | node, kind (`nvme`, `ssd`, `hdd`), identity (NVMe EUI-64/NGUID, or WWN and serial), log device (for `hdd`), capacity, class, weight, state (`in`, `out`, `failed`) |
| `pools` | pool id | kind (`replicated{size, min_size}` or `ec{k, m}`), chunk size, PG count, failure domain level, device class filter, thresholds |
| `pgs` | (pool, pg) | `pg_epoch`, `up` set (computed placement), `acting` set (who serves now), primary, in-sync set, state |
| `hosts` | host identity | bare-metal and non-glidex hosts: NQN, CHAP key reference, the volumes and snapshots exported to it |
| `blocklist` | client instance id | map epoch added, expiry |

- **The map** is all of these together, with a `map_epoch` that every
  change bumps. Daemons and clients hold the map and follow it by a
  watch stream of deltas. Every request carries the `map_epoch` its
  sender used; a receiver with a newer map answers `StaleMap{current}`
  and the sender fetches the delta.
- **What is batched.** A node failure changes every PG of that node in
  one entry. A rebalance after a map change is one entry per batch of
  PGs that finish backfill together, not one per PG.
- **Liveness** follows clustering.md D7: storage daemons send a heartbeat
  to the leader every 2 s, kept in the leader's memory. Only transitions
  go through Raft. PG members also send heartbeats to each other and
  report a silent peer to the leader, so the leader can tell a down node
  from one that is cut off from it alone.
- **Map lease.** Each daemon's heartbeat reply renews a *map lease* of
  10 s. A daemon whose map lease has run out stops accepting writes for
  its PGs (§8.4) until it renews. This bounds how long a blocklist round
  (D24) can wait for a silent node.
- **Blocklist.** An entry names a client *instance*: a random id each
  client process makes at start, bound to its certificate identity in
  `Hello`. A storage node refuses every request from a blocklisted
  instance once it holds that map epoch. Entries expire after an hour
  by default. A restarted client has a new instance id, so it is never
  held back by an entry for its previous life.

### 4.2 Volume metadata (the `.meta` pool)

- `.meta` is created at cluster init: replicated, `size 3`,
  `min_size 2`, failure domain `host`, 256 PGs by default, on the
  fastest device class. It is a pool like any other: the same
  placement, peering, recovery and scrub (§5, §8).
- **Objects** in it are small (a few KiB) and versioned. A change is a
  compare-and-set on the object's version, ordered by its PG primary and
  replicated like any write (§8.1), so it is durable on every in-sync
  copy before it is acknowledged. Reads come from the primary under its
  read lease (§8.4), so they are linearizable.

| Object | Key | Holds |
|---|---|---|
| `Header` | volume id | pool, size, lineage, branch, parent `(branch, epoch)`, head epoch, snapshots (epoch, name, group, state), state, QoS, lease (holder instance, mode, expiry), space (head and snapshot-only bytes), background job progress |
| `Lineage` | lineage id | the branch tree: each branch's parent and its volume id |
| `Dir` | (pool, shard) | the volumes of a pool, in 64 shards by volume id hash |
| `Group` | group id | a consistency-group snapshot: members, state (`preparing`, `complete`) |

- **The header's primary serves the volume's control traffic.** The
  lease holder keeps a connection to it, renews the lease over it, and
  is told of changes (expand, snapshot) over it. The primary keeps the
  list of attached clients in memory; a notification that is lost is
  repeated on the next renewal, which returns the header's version.
- **Background jobs** of a volume (delete, snapshot merge, flatten,
  rollback cleanup) are run by the header's primary, which sends work to
  the data PGs and records progress in the header. A new primary after
  failover reads the header and resumes.
- **The management API** (§16) is served by every `hyperstor-meta`
  member, not only the leader. Volume operations are performed as a
  client of `.meta`; only map changes are forwarded to the leader.

### 4.3 Scale envelope

Sized for a first deployment of up to 256 storage nodes, 4,096 NVMe
namespaces, pools of up to 65,536 PGs and a million volumes.

| Kind of change | Where | Rate |
|---|---|---|
| map changes | Raft | follows hardware changes and failures; one entry per event |
| blocklist entries | Raft | forced takeovers only |
| heartbeats | leader memory | one per node per 2 s |
| volume operations, lease renewals | `.meta` PG primaries | one renewal per attached volume per 2 s, spread over 256 PGs on every storage node |
| block metadata | engines | follows I/O, always local |

## 5. Placement

### 5.1 Chunk to PG

- A volume offset maps to chunk index `offset / chunk_size`.
- The chunk's PG is `pg = hash64(lineage, chunk_index) mod pg_count` (D21).
  `pg_count` is a power of two, chosen when the pool is created (default:
  enough for about 100 PGs per device in the pool's device class).
- **Later:** doubling `pg_count` splits each PG in two. Because the hash
  is reduced by `mod 2^n`, the chunks of PG `p` go to `p` or
  `p + 2^n`, and the split happens on the devices that already hold them.

### 5.2 PG to devices

- **Rendezvous hashing with weights.** For each candidate, the score is
  `-ln(u) / weight`, where `u` is a uniform hash of `(pool, pg, candidate)`.
  The lowest score wins. The weight is the device's capacity.
- **Failure domains.** A pool names a level (`host` or `rack`). Placement
  first picks distinct domains at that level by the same scoring over the
  domain's total weight, then one device within each chosen domain.
- **Order.** The ranked list is the `up` set. For a replicated pool the
  first entry is the preferred primary. For an EC pool the position is
  the shard index (shards `0..k-1` hold data, `k..k+m-1` parity).
- **Device classes.** A pool can be limited to a class, so fast and
  dense media make different pools. Each device gets a class from its
  kind (`nvme`, `ssd`, `hdd`), which the operator can override with a
  label (`nvme-qlc`, `archive`). A pool never mixes hard disks with
  flash, because the slowest copy sets a write's latency (§8.1).
- `.meta` (§4.2) is always placed on flash.

### 5.3 Rebalancing

- A map change recomputes `up` for the PGs it affects. While `up`
  differs from `acting`, the PG keeps serving from `acting` and the new
  members are backfilled (§8.5). When they have caught up, `acting`
  becomes `up` in one Raft write and a new `pg_epoch`.
- **Down and out.** A node that misses heartbeats for 10 s is marked
  `down`: its PGs get new primaries from the rest of `acting`, but data
  doesn't move. After `down_out_interval` (10 min by default) it is
  marked `out` and its PGs are re-placed. A short reboot moves no data.
- Backfill and recovery are rate-limited per device and per node, and
  PGs below `min_size` go first.

## 6. Storage daemon

### 6.1 Threads and cores

- `hyperstord` runs one reactor thread per reserved core (D7). Cores
  come from host-topology's reserved list, on the NUMA node of the
  NVMe devices and NIC they serve.
- A small tokio runtime (two threads) runs the control work: map watch,
  heartbeats, peering, recovery scheduling, the admin socket.
- **PG ownership.** Each PG belongs to one reactor, chosen by
  `hash(pool, pg) mod reactors`. That reactor handles all of the PG's
  requests, so nothing on the I/O path takes a lock.
- **Connections land on the owning reactor.** A client's connection
  hello names the reactor it wants (the map lists each node's reactor
  count). The acceptor hands the socket or queue pair to that reactor
  once, and from then on requests arrive on the core that owns their PGs.
- A client opens a connection to a reactor only when it first has a
  request for one of that reactor's PGs, and closes connections idle for
  a minute. `hyperstor-vhost` on a busy node ends up connected to most
  reactors in the cluster; an application with `libhyperstor-gpu` only
  to those holding its files.

### 6.2 Devices and queues

- **NVMe back end.** The engine opens each NVMe namespace through its
  generic character device (`/dev/ngXnY`) and submits reads, writes and
  deallocates with `uring_cmd` passthrough, on one io_uring per reactor.
  Polled queues are used where the driver has them (`nvme.poll_queues`);
  otherwise completions come by interrupt. The installer suggests the
  module option and never forces it (host-topology H12).
- **Block back end** (D29), for every other device:
  - io_uring `O_DIRECT` reads and writes on the block device (`/dev/sdX`,
    by its stable `/dev/disk/by-id` name), on the same per-reactor
    io_uring;
  - durability: a log write is `RWF_DSYNC`, which the kernel sends as FUA
    where the device supports it (`queue/fua`) and as a write plus flush
    otherwise. A device with a volatile write cache and no FUA is
    accepted with a warning, because every log write then costs a flush;
  - discard and write-zeroes through `IORING_OP_FALLOCATE` (punch hole
    and zero range on the block device), which the kernel turns into the
    device's own commands;
  - queue depth per reactor is capped from the device's
    `queue/nr_requests`, so one reactor can't starve the others on a SATA
    device's single queue.
- **Hard disks** (D29):
  - the log and the index go on a partition of a flash **log device**;
    one log device serves up to eight hard disks, each with its own
    partition;
  - allocation is in 64 KiB units, not 4 KiB, to keep extents long;
  - deferred small writes (§7.3) are applied in place in batches sorted
    by offset, so the disk sees mostly sequential writes;
  - losing a log device fails every hard disk that uses it. Placement
    treats them as one failure unit, which the host failure domain
    already covers.
- **Buffers.** Each reactor owns a buffer pool registered once with its
  io_uring (fixed buffers) and with its RDMA protection domain, so data
  moves between disk and network without a copy and without registration
  on the I/O path.

## 7. Local engine

### 7.1 Layout on a device

| Region | Contents |
|---|---|
| Superblock (two copies) | device id, node id, cluster id, format version, region offsets; for a hard disk, the log device's id and partition |
| Log | circular. Operation records (§7.3), deferred small writes, checkpoint markers |
| Index | an on-device copy-on-write B+tree of objects and their extents |
| Data | 4 KiB blocks, allocated in extents (64 KiB units on hard disks) |

On a hard disk, the log and index regions are on its partition of the
log device, which has its own superblock naming the data device (§6.2).

### 7.2 Objects

- An **object** (D34) is keyed by `(pool, pg, namespace, name,
  version)` and has:
  - **data**: sparse; its extent map lists the blocks it holds, and each
    extent carries one CRC32C per 4 KiB block;
  - **attributes**: a few small named values;
  - a **key–value table**: ordered keys and values of its own, stored in
    the index B+tree under the object's key prefix, so a range scan of
    them is a range scan of the tree.
- **Namespaces** and their names:

| Namespace | Name | Used by |
|---|---|---|
| `vol` | `(lineage, chunk_index, branch)` | volume chunk versions (below) |
| `ec` | the above plus the shard index | EC fragments (§8.6) |
| `meta` | object kind and id | `.meta` headers, lineages, directories, groups (§4.2) |
| `kv` | range id | `hyperstor-kv` ranges (§7.5) |
| `s3` | `(bucket id, object id, part, chunk_index)` | S3 object data (§14) |
| `fs` | `(inode, chunk_index)` | file data (filesystem.md) |

- **Operations** on one object come as a list applied atomically
  (D22): compare-and-set on the version, read a range, write a range,
  get, put and delete keys, delete a key range, set attributes. The
  primary applies the list in one log record, and replicas apply the
  same record.
- **Volume chunk versions.** For `vol`, the version is the volume's
  snapshot epoch:
  - `branch` is a volume within a lineage (D21). A new volume has its own
    lineage and branch 0; a clone gets a new branch in its parent's
    lineage;
  - `epoch` is the head epoch the version was created in (D10).
- **Reading block `b` of a chunk** for volume `(lineage, branch)` at epoch
  `e`: take the newest version of that branch with epoch ≤ `e` that holds
  `b`; if none does, continue in the parent branch at the epoch the clone
  was made from, and so on up the chain. A block nothing holds reads as
  zeros.
- EC fragments are objects too, with the shard index in the key (§8.6).
- The index is cached in memory per PG. The CRC arrays stay on disk in
  the index's leaf pages and are cached by use.

### 7.3 Writes (D22)

- **Into a version that only the head uses** (no snapshot holds it):
  - a write of 64 KiB or more, or into blocks the object doesn't hold
    yet, goes to a newly allocated extent. A log record then points the
    object's map at the new extent, with the new CRCs, and frees the old
    blocks;
  - a smaller overwrite of blocks the object holds is written *into* the
    log record together with its CRCs. It is acknowledged when the record
    is durable and applied in place later, in the background. Reads check
    the not-yet-applied records first.
- **After a snapshot** (the head epoch is newer than the chunk's newest
  version): the write creates a new version object holding only the
  written blocks. Nothing is copied (D10).
- A write is durable when its log record is: one write with FUA, or a
  flush on devices without power-loss protection. The engine batches the
  records of concurrent writes into one log write.
- **Checkpoint.** The index is updated in memory, and dirty pages are
  written copy-on-write with a new root when the log is half full or
  every 30 s. The root is recorded in the log, and the log is trimmed up
  to it. Recovery after a crash replays the log from the last root.

### 7.4 Allocation and space

- Free space is a bitmap of 4 KiB blocks on disk, with an in-memory tree
  of free extents built from it at start.
- Discard and write-zeroes from a client drop blocks from the head
  version. Blocks no version holds any more are freed, and the device is
  sent a deallocate.
- The engine reports used, free and allocated-per-lineage bytes to the
  daemon, which reports them to the metadata service with its heartbeat.

### 7.5 The transactional key–value store (D35)

- **Ranges.** The keyspace of each KV store (one per user: the S3
  gateway, each filesystem) is split into ranges. A range is a `kv`
  object in a replicated pool on flash; its key–value table holds the
  range's keys. A range is split in two when it passes 64 MiB or a
  sustained write rate, and adjacent small ranges are merged.
- **Finding a range.** A root range, with a fixed id, maps key
  boundaries to range ids, like Bigtable's root table. Clients cache it
  and refresh an entry when a range answers `RangeMoved`.
- **Records.** Each key holds versions by commit timestamp, plus a lock
  column during a transaction, as in Percolator.
- **Transactions:**
  1. a transaction reads at a start timestamp from the oracle;
  2. at commit, it writes a lock and the new value to every key it
     changes (prewrite), with one key chosen as the primary lock;
  3. it gets a commit timestamp and commits the primary key; the
     transaction is committed at that moment;
  4. it then commits the other keys, in the background;
  5. a reader that meets a lock checks the primary: committed, it rolls
     the key forward; abandoned past its time-to-live, it rolls it back.

  A transaction whose keys are all in one range commits in one atomic
  operation on that range's object, with no locks.
- **Timestamp oracle.** The `hyperstor-meta` leader hands out timestamps
  from memory in batches. It writes a high-water mark through Raft,
  reserving the next three seconds of timestamps, so a new leader starts
  above anything handed out. That is one Raft write per three seconds,
  whatever the transaction rate.
- **Old versions** are removed by a garbage collector once they are
  older than the oldest running transaction, and at most after ten
  minutes.

## 8. Replication and consistency

### 8.1 Write path (replicated pools)

1. The client sends `Write` to the PG primary with `map_epoch`,
   `pg_epoch`, the volume's lineage, branch and head epoch, the range,
   the data and the per-block CRCs. The connection carries the client's
   instance id.
2. The primary checks that:
   - it is primary for that `pg_epoch`;
   - the client instance isn't blocklisted (§9);
   - the CRCs match the data.
3. It assigns the next PG sequence number and sends `Repl` to every
   other member of the in-sync set, in parallel, while writing locally.
4. Each member applies the operation in sequence order per object and
   answers once its log record is durable.
5. The primary acknowledges the client when every in-sync member has
   answered.

A member that doesn't answer within the operation timeout (default 5 s)
is reported to the metadata service, which drops it from the in-sync set
in a new `pg_epoch`. The write then completes with the remaining
members, as long as there are at least `min_size` of them. Below
`min_size` the PG blocks writes rather than acknowledge fewer copies.

### 8.2 Read path

- A read goes to the primary, which serves it while it holds a read
  lease (§8.4). The primary checks the CRCs. On a mismatch it reads the
  range from another in-sync member, answers with that, and repairs its
  own copy.
- **Snapshots and read-only attachments** (D18) may read from any member
  of the in-sync set. A snapshot's versions never change, so any copy is
  current. The client picks the closest member, then the least loaded.

### 8.3 The PG log

- Each PG keeps a log of its recent operations (sequence number, object,
  range, epoch), stored in the engine's log and trimmed to the newest
  10,000 operations or the last 30 minutes, whichever is more.
- A member that missed operations still covered by the log catches up by
  replaying them (delta recovery). Otherwise it is backfilled (§8.5).

### 8.4 Primaries, epochs and read leases (D8, D23)

- Every change to a PG's `acting` set or primary is a new `pg_epoch`.
  Members accept operations only for the `pg_epoch` they have peered in.
- **Read lease.** A primary may serve reads only while a lease from
  every in-sync member is valid. Members grant it with each `Repl` reply
  and each heartbeat, for 8 s, measured on their own monotonic clocks.
  A member that peers in a newer epoch first waits out the leases it has
  granted. So a partitioned old primary stops serving reads before a new
  one starts.
- **Peering.** When a PG enters a new `pg_epoch`, the new primary:
  1. asks each member of the previous in-sync set for its last sequence
     number and log head;
  2. takes the longest log among them as authoritative. Every
     acknowledged write is on every member of the old in-sync set, so the
     longest log has them all. Writes beyond what was acknowledged are
     kept; whole-operation atomicity (D22) means they apply completely or
     not at all;
  3. pushes missing operations to the members that lack them;
  4. reports the in-sync set to the metadata service and starts serving.

  If no member of the last in-sync set can be reached, the PG stays
  `down` and serves nothing, because a reachable member outside that set
  may lack acknowledged writes.
- A storage node whose map lease (§4.1) has run out stops accepting
  writes for its PGs until it renews, because it may have missed a new
  `pg_epoch` or a fence.

### 8.5 Recovery, backfill and scrub

- **Recovery** replays PG log entries to members that missed them. It
  runs as part of peering when the delta is small (by default, up to
  1,000 operations), and in the background otherwise.
- **Backfill** brings a new or long-absent member up by copying objects
  in key order, while new writes for already-copied keys are sent to it
  too. It is rate-limited (§5.3). The member joins the in-sync set when
  the scan finishes.
- **Scrub:**
  - *light*, daily: compare each member's object list and versions for
    a PG;
  - *deep*, weekly: read every block, check its CRC, and compare
    per-object digests across members.

  Damaged copies are repaired from a copy whose CRCs match, and an
  inconsistency that can't be decided is reported, not guessed at.

### 8.6 Erasure-coded pools (D6)

- A chunk is striped over the `k` data shards in stripe units of
  `chunk_size / k`, with `m` parity shards computed with Reed–Solomon (the
  pure-Rust `reed-solomon-simd` crate).
- **Writes:**
  1. the client sends the write to shard 0, the primary;
  2. the primary appends it to the PG's **write log**, an object held on
     shards 0, 1 and 2 with the same rules as a replicated write (§8.1),
     and acknowledges.
- **Full-stripe writes** skip the log. A write that covers one or more
  whole stripes of a chunk is encoded by the primary at once and written
  to all `k + m` shards as a new version, acknowledged when every shard
  has committed it. The primary gives it a sequence number after any
  logged writes to the same stripes; when it commits, those log entries
  are dropped from the index of logged ranges, so they are never
  destaged over it. The parts of the write outside whole stripes go
  through the log as usual.
- **Destage**, in the background:
  1. the primary takes the logged writes of a chunk, reads the parts of
     the stripes they don't cover, and computes parity;
  2. it writes all `k + m` fragments as a new version;
  3. once every shard has committed the new version, it trims the log.

  Destage runs when a stripe is fully covered, when the log passes half
  its size, or after 30 s.
- **Reads** check the primary's in-memory index of logged ranges first,
  then read the `k` data fragments. With shards missing, any `k` of the
  `k + m` reconstruct the data (a degraded read).
- **Back-pressure.** When the write log of a PG is full, new writes wait
  for destage.
- `min_size` for an EC pool is `k + 1` by default.

## 9. Volume leases and fencing (D11, D23, D24)

- **Attaching** a volume takes a lease, recorded in the volume's header
  object by its PG primary (§4.2):
  - `rw` is exclusive, one holder;
  - `ro` is shared, and is allowed only on snapshots (D18).
- A lease names its holder (client instance), mode and expiry. The holder
  renews it every 2 s, for 10 s, over its connection to the header's
  primary. Raft is not involved.
- **Every request carries the client's instance id**, bound to its
  identity at `Hello`. Storage nodes refuse requests from blocklisted
  instances (§4.1).
- **Release and hand-off.** A holder that detaches releases the lease.
  A planned move (a VM re-placed while stopped, a gateway hand-over)
  is a release by the old holder and an acquire by the new one: no
  blocklist round.
- **Forced takeover** (the holder died, was partitioned, or is being
  preempted by the operator) is the blocklist round of D24:
  1. the new holder asks the header primary for the lease; it is refused
     while the old lease hasn't expired;
  2. once it has, the new holder (or the API member acting for it) asks
     the metadata service to blocklist the old instance: one Raft write,
     map epoch `N`;
  3. the metadata service pushes the map and reports when every storage
     node has acknowledged epoch `N` or has lost its map lease (and so
     refuses all writes, §8.4);
  4. the header primary, holding a map of epoch ≥ `N`, breaks the old
     lease and grants the new one.
- **The metadata service being down doesn't stop I/O**, and doesn't stop
  releases, hand-offs or renewals either, because none of them touch
  Raft. A forced takeover waits for it.
- **NVMe reservations** at the gateway (§11.2) map onto this: acquire is
  an acquire, preempt is a forced takeover.
- **Effect on glidex.** storage.md S10 needs `node forget --fenced`
  before a `Lost` VM is re-placed, because the partitioned node may still
  be running it (clustering.md D8). For disks on hyperstor the blocklist
  round already ensures the old VM's writes fail. Whether glidex relaxes
  `--fenced` for VMs whose disks are all on hyperstor is for
  clustering.md to decide. The VM's network identity is affected too, not
  only its disks.

## 10. Volume operations

### 10.1 I/O in the client library

- The client splits each request at chunk boundaries, looks up each
  chunk's PG and primary in its map, and sends the pieces to the owning
  reactors in parallel.
- **Retries.** On `StaleMap`, `NotPrimary` or a lost connection, it
  fetches the map delta and resends. On `Blocklisted` it stops and
  reports the volume as lost to its user (the VM sees I/O errors).
- **Readahead.** The client detects sequential reads per volume and per
  queue, and reads ahead up to the next two chunks, cancelled when the
  pattern breaks. Dataset streaming and model loading are long
  sequential reads; readahead keeps every reactor that holds the next
  chunks busy. Pools for such data can use a larger chunk size (e.g.
  16 MiB, §5.1), which the client's readahead follows.
- **Flush** is a no-op that returns at once: a write is acknowledged only
  when every in-sync copy has persisted it, so there is nothing to flush.
  The device is advertised as write-through.

### 10.2 Create, expand, delete

- **Create** writes a `Header` object and adds the volume to its pool's
  `Dir` shard. Nothing is allocated until data is written (thin
  provisioning).
- **Expand** changes the size in the header; the header's primary tells
  the lease holder, which reports the new capacity to its device (virtio
  config change, NVMe namespace attribute notice).
- **Shrink** is refused.
- **Delete** requires no lease and no clones of the volume's snapshots.
  The header is marked `deleting`, and its primary has each data PG drop
  the volume's branch, then removes the directory entry and the header.

### 10.3 Snapshots

- **Taking one** (D10) of a volume:
  - if the volume is attached, the header's primary asks the lease
    holder to **quiesce**: stop issuing writes, wait for the outstanding
    ones to be acknowledged, then report;
  - in one compare-and-set on the header, the snapshot is recorded at the
    current head epoch `e`, and the head epoch becomes `e + 1`;
  - the holder is told the new epoch and resumes, sending writes with it;
  - if quiescing takes longer than 2 s, the snapshot is abandoned and
    the holder resumes.

  A holder that crashes between quiescing and resuming reads the head
  epoch from the header when it re-attaches.
- **A consistency group** of volumes, whose headers sit in different
  PGs, is taken by a coordinator (the API member serving the call):
  1. it writes a `Group` object in state `preparing`, listing the members;
  2. it quiesces every member's holder;
  3. only then does it take each member's snapshot as above, tagged with
     the group id;
  4. it marks the group `complete`.

  Every write to every member stops before any epoch moves, so the
  snapshot is crash-consistent across a VM's disks. If the coordinator
  fails midway, the group stays `preparing`; a background job on the
  `Group` object's primary deletes its snapshots and the group after a
  minute. An incomplete group is never listed as a snapshot.
- **Deleting** a snapshot marks it deleted. Each PG then merges, in the
  background, the versions only that snapshot held into the next newer
  version (keeping the blocks the newer one doesn't override) or frees
  them.
- **Rollback** makes the volume's head a new branch whose parent is the
  snapshot, and drops the old head branch in the background. It is
  instant, and needs the volume detached.
- **Limits:** 255 snapshots per volume. Clone chains of more than 16
  levels are refused; flatten first.

### 10.4 Clones and flatten

- **Clone** creates a volume with a new branch in the parent's lineage,
  whose parent is `(branch, snapshot epoch)`. It is instant and allocates
  nothing (D21).
- **Flatten** copies into the clone's branch every block it still reads
  from its parents, then clears the parent link. The lineage doesn't
  change, so placement stays the same and the copy stays on each PG's own
  devices.
- A snapshot with clones can't be deleted until its clones are flattened
  or deleted.

### 10.5 Space and QoS

- Each volume reports allocated bytes for its head and the bytes held
  only by its snapshots. Pools report used and free bytes, and enforce
  the D27 thresholds.
- **QoS:** a volume can have IOPS and bandwidth limits with a burst.
  They are enforced in two places:
  - the client library uses token buckets, which serve well-behaved
    clients;
  - each reactor queues requests per volume with weighted fair queuing,
    so a client that ignores its limits can only take its fair share of
    the devices.

  Provisioned IOPS for metering (metering.md §16) build on these
  limits.

## 11. Front ends

### 11.1 vhost-user-blk (D1, D26)

- `hyperstor-vhost` serves one socket per attached disk,
  `/run/hyperstor/vhost/<disk-id>.sock`, built on the rust-vmm crates.
- Guest memory is shared with the service. VMs with a vhost-user disk
  therefore need a shared memory backend: memfd, or hugepages per
  host-topology §8a, as vhost-user networking already does.
- **Virtio features:** multiqueue (one queue per vCPU, up to the reactors
  on the VM's NUMA node), packed virtqueues, `EVENT_IDX`, discard,
  write-zeroes, flush (§10.1), and the config-change event for expand.
- **The disk's serial number** is the first 20 characters of the glidex
  disk id without dashes, unique within the VM. Tools in the guest, and
  the guest library (D33), find the volume behind a disk by it.
- **Fewer VM exits:**
  - while a queue is busy, the reactor polls it and uses `EVENT_IDX` to
    tell the guest not to notify, so submitting I/O costs the guest no
    exit;
  - after 50 µs (configurable) without new requests, the queue re-enables
    notifications and the reactor waits on its eventfd (adaptive
    polling);
  - completions are signalled through irqfd, which KVM delivers as a
    posted interrupt where the CPU supports it, again without an exit;
  - a VM marked latency-critical can use polled queues in its own
    virtio-blk driver (`poll_queues`), giving up guest CPU for the lowest
    latency.
- **Zero copy.** Guest memory is registered with the service's RDMA
  protection domain when the VM attaches:
  - on NICs with on-demand paging (mlx5 ODP), the whole guest memory is
    registered without pinning it;
  - otherwise the VM's memory must be hugepage-backed, and the
    registration pins it once, at attach (G8).

  With TCP, sends use `SEND_ZC` from guest memory.
- **Restart.** The service keeps its leases in a state file. After a
  restart it re-registers them, and the hypervisor reconnects to its
  sockets: QEMU with `reconnect`, Cloud Hypervisor to be checked (G6).

### 11.2 NVMe-oF target (D19)

- `hyperstor-gw` runs the client library behind an NVMe-oF target
  written in Rust. NVMe/TCP comes first, NVMe/RDMA (RoCEv2 and IB) after.
- **Subsystems.** One subsystem per volume,
  `nqn.2026-10.org.glidex:hyperstor:<cluster-id>:<volume-id>`, with
  namespace 1. A read-only attachment of a snapshot exports it read-only.
- **Multipath.** Every gateway exports every attached volume; all of
  them are ANA-optimized, and the host spreads I/O over them (D17).
- **Access:**
  - a host allowlist of host NQNs, from the volume's attachments;
  - DH-HMAC-CHAP with keys from the credential store (credentials.md);
  - NVMe reservations, held for the host by the gateway: acquire takes
    the volume's `rw` lease, and preempt is a forced takeover (§9).
- **Discovery** service on port 8009, listing only the subsystems the
  connecting host may see.
- **Stopgap (D19):** until the target is ready, each exported volume is
  a ublk device served by the client library (the `libublk` crate), and
  is exported by the kernel's `nvmet` through configfs. The gateway
  manages that configuration and gives the same NQNs, so hosts don't
  change when the stopgap is replaced.

### 11.3 Host read cache (D32)

- **What is cached:** only versions that can't change, that is, versions
  with an epoch older than the volume's head epoch, read through a
  snapshot attachment, or read by a clone through to its parent. A
  read of a head version always goes to the primary.
- **Where:** memory first (default: 5% of the node's memory, at most
  64 GiB), and optionally a partition of a local NVMe device as a second
  tier. The second tier is a plain cache: it is dropped on restart if its
  header doesn't match, and nothing is ever written back from it.
- **Keys and verification.** Entries are keyed by `(lineage, chunk,
  branch, epoch, block)` and keep the blocks' CRCs from the storage node,
  checked again on every read from the NVMe tier.
- **Eviction:** least recently used, with a scan-resistant admission
  rule (a block is admitted to the NVMe tier on its second miss), so one
  large sequential read doesn't flush the cache.
- **Deleted snapshots** are dropped from the cache when the map delta
  (or the header notification) reports them.
- The GPU library (§12) can use a host's cache through
  `hyperstor-vhost` on glidex nodes; on bare metal it reads directly.

### 11.4 Later

- **Kubernetes on glidex VMs.** A CSI driver in the style of cloud
  providers' (AWS EBS): the controller calls glidex's API to create a
  hyperstor disk and hot-plug it into the VM as vhost-user-blk. The
  guest never reaches the storage network. `ReadOnlyMany` volumes are
  read-only snapshot attachments, served by the host read cache. It
  needs vhost-user-blk hot-plug in glidex (§20).
- **NVMe emulation over vfio-user**, for guests that need a real NVMe
  device (some Windows and appliance images). For Linux guests,
  virtio-blk performs as well.
- **The vhost client on a DPU** (BlueField SNAP and similar), presenting
  virtio-blk or NVMe functions to the guest with no host CPU. Worth it
  at scale, but tied to one vendor and hard to combine with live
  migration.
- **Live migration.** When glidex migrates VMs live, the destination
  attaches as a delegate (D33) during the copy and takes over the lease
  at switchover, with no blocklist round.

## 12. GPU-direct storage (D13–D18)

### 12.1 The native path

1. The application registers a file through cuFile or hipFile.
2. The interposer checks whether the file is on a hyperstor volume: its
   block device must be a hyperstor NVMe-oF namespace (by subsystem NQN),
   a hyperstor ublk device, or, in a glidex VM, a virtio disk whose
   serial the delegate credential lists (D33). If not, it passes the
   call through.
3. If it is, the interposer registers a userspace-FS handle (D14), opens
   the volume in `libhyperstor-gpu` as a delegate of the volume's lease
   holder (D33), and takes a read lease on the file.
4. **On a read**, the library:
   - maps the file range to volume blocks with `FIEMAP`;
   - for the extents that qualify (D16), registers the GPU buffer through
     dma-buf (registrations are cached per buffer);
   - sends one request per chunk to its primary (or, for a read-only
     snapshot, to any replica), carrying the buffer's address and rkey.

   The storage node writes the data into GPU memory by RDMA and answers.
   Ranges that don't qualify go through the kernel path (D17).
5. **Writes** go the same way, in the other direction: the storage node
   reads from GPU memory by RDMA.

### 12.2 Leases and consistency

- The native client and the kernel path share one `rw` lease. On bare
  metal, the gateway holds it for the host's NQN (its reservation) and
  the library is the gateway's delegate; in a VM, `hyperstor-vhost` holds
  it and the guest library is its delegate (D33). The library's host
  identity must match the one the lease was taken for.
- After a native write, the library drops the written range from the
  page cache (`fadvise(DONTNEED)`), so buffered readers on the host see
  the new data (G3).
- **Read-only multi-attach:** a snapshot attached `ro` by many hosts is
  mounted `ro,norecovery`. Its block map can't change, so every read of
  it qualifies for the native path.

### 12.3 In glidex VMs (D18, D33)

1. The VM has its volume as an ordinary vhost-user disk, its GPU, and an
   RDMA VF on the storage network, under the same PCIe switch as the GPU.
2. The guest mounts its filesystem on the virtio disk.
3. `libhyperstor-gpu` in the guest asks `hyperstor-vhost` over
   virtio-vsock for a delegate credential. It names the VM's volumes
   by disk serial, and expires with the VM's attachment.
4. Native reads and writes go over the VF straight to the storage nodes,
   as on bare metal (§12.1). Everything else, including the guest's own
   filesystem metadata, goes through the virtio disk.
5. The guest library's buffers in GPU memory are registered through the
   VF; whether the drivers allow that inside a VM is G2.

### 12.4 Scale-up fabrics (NVLink, UALink)

NVLink (with NVSwitch) and UALink connect the GPUs of one pod with
memory semantics. They have no storage endpoints and are not a transport
for hyperstor: storage stays on the scale-out fabric (§13.2).

- **Loading the same data on many GPUs.** Inside an NVLink or UALink
  domain, a framework can read each part of a file on one GPU and share
  it over the scale-up fabric (sharded loading, then an all-gather).
  This divides the storage traffic by the domain's size. It is the
  framework's choice; hyperstor's part is to make the sharded reads
  efficient (any-replica reads of snapshots, readahead, the host cache).
- **Grace systems.** On GPUs coherently attached to a CPU (NVLink-C2C on
  Grace Hopper and Grace Blackwell), a copy from host memory into the GPU
  is far cheaper than over PCIe. The kernel path (D17) and its bounce
  buffer then lose less against the native path. G11 measures how much.
- **Placement inside a pod** (which GPUs share an NVLink partition, how
  passthrough VMs get whole partitions) is host-topology's concern.

### 12.5 Sharing a process with NCCL or RCCL

Training jobs load `libhyperstor-gpu` into processes that also run NCCL
or RCCL, which use the same NICs' queue pairs and memory registrations.
The library therefore:
- caps its own queue pairs (one per storage reactor it talks to, closed
  when idle, §6.1), its registration cache (by default 16 GiB of GPU
  memory) and its own locked host memory, all configurable;
- registers GPU buffers through dma-buf independently of NCCL's own
  registrations of the same memory, which the drivers allow;
- uses its own protection domain, so its rkeys and NCCL's never mix.

### 12.6 Libraries and upstream

- `libhyperstor-gpu` is a `cdylib` with a C ABI, also offering raw volume
  handles (D16) for callers that manage offsets themselves.
- `libhyperstor-cufile` and `libhyperstor-hipfile` are interposers,
  loaded with `LD_PRELOAD` or ahead of the vendor library. They look up
  the vendor symbols with `dlsym(RTLD_NEXT)` and ship no vendor headers
  (G4).
- **hipFile upstream:** add userspace-FS support to hipFile's AMD side as
  a backend (D15). Until it lands, `libhyperstor-hipfile` serves the I/O
  for hyperstor files itself.

## 13. Internal protocol (D5)

### 13.1 Messages

- **Framing.** Every message is a fixed header:
  - magic, version, message type and flags;
  - request id, header length, payload length;
  - CRC32C of the header.

  Then a typed header body, encoded little-endian with a fixed layout
  and no general-purpose serializer, then the payload.
- **Messages:**

| Group | Messages |
|---|---|
| Session | `Hello` (version range, identity, reactor wanted), `Ping` |
| I/O | `Read`, `Write`, `WriteZeroes`, `Discard` |
| Replication | `Repl`, `ReplAck` (with a read-lease grant) |
| Volume metadata | `ObjGet`, `ObjCas`, `LeaseAcquire`, `LeaseRenew`, `LeaseRelease`, `Quiesce`, `Resume`, `Notify` |
| Peering and recovery | `PeerQuery`, `PeerInfo`, `PushOps`, `BackfillScan`, `BackfillObjects` |
| EC | `LogAppend`, `FragmentWrite`, `FragmentRead` |
| Errors | `StaleMap{current}`, `NotPrimary{epoch}`, `Blocklisted`, `CasConflict{version}`, `NoSpace`, `Checksum`, `Busy{retry_after}` |

- **Versions.** `Hello` agrees on the highest version both sides speak.
  Each release speaks its own version and the one before, so rolling
  upgrades work node by node (§18.4).
- **Security** per D25.

### 13.2 Transports (D30)

- **One transport layer.** The reactor talks to a `Transport` trait with
  two implementations, TCP (io_uring sockets) and verbs (rdma-core).
  The messages of §13.1 are the same on both. A libfabric
  implementation can be added behind the same trait (D30).
- **Networks and transports in the map.** Each network a node is on
  lists its address and the transports it offers there: `tcp`, `roce`
  (RoCEv2, with GID index and traffic class) or `ib` (with GID, LID and
  partition key). A sender uses RDMA to a peer when both have it on a
  common network and TCP otherwise, so a cluster can mix InfiniBand
  storage nodes with Ethernet-only compute nodes.
- **Connection setup.** Every connection starts with the authenticated
  TCP session of D25 (over IPoIB on an InfiniBand-only fabric). The two
  sides exchange queue pair numbers, GIDs and receive-buffer credits over
  it, then move to the queue pair. The TCP session stays open, idle, to
  carry a clean close and to detect a dead peer.
- **Queue pairs** are reliable connected (RC). A reactor posts receive
  buffers to one **shared receive queue** for all its queue pairs, so
  memory doesn't grow with the number of peers. **Later:** on mlx5
  devices, dynamically connected (DC) queue pairs let one queue pair
  reach every peer, which removes per-peer queue pairs altogether.
- **No reliance on order.** Every message carries what its receiver
  needs to place it: request ids for replies, PG sequence numbers for
  `Repl` (§8.1). A reply that covers an RDMA write is sent only after
  that write has completed at the target (on libfabric, with
  delivery-complete semantics). The protocol then also runs on
  transports that deliver out of order, such as UET's unordered mode
  (D30).
- **Payloads:**
  - messages up to the inline limit (typically 256 bytes) go as inline
    `SEND`s;
  - other headers go as `SEND`s from the reactor's registered buffers;
  - data moves by RDMA operations that only the server starts: for a
    read, the server writes into the client's buffer; for a write, the
    server reads from it. The client names its buffer by address,
    length and rkey. No server rkey ever leaves the server (D13).
- **Flow control** is by credits. Each side grants the other a number of
  receive buffers at setup and returns credits in every reply. A sender
  without credits queues, so receivers never see receiver-not-ready
  retries.
- **Several ports.** A node with more than one RDMA port (or several
  NICs, one per NUMA node) lists each as its own address. A reactor
  uses the port on its own NUMA node.
- **Port roles and traffic classes** (D31):
  - each port has a role, `storage`, `compute` or `converged`. The
    installer proposes `compute` for ports that sit under the same PCIe
    switch as a GPU, and `storage` for the others; the operator
    confirms;
  - each network has a traffic class for hyperstor: a DSCP value and PFC
    priority on RoCE, a service level on InfiniBand. Hyperstor sets it on
    every queue pair;
  - hyperstor uses `storage` ports. It uses `converged` ports with its
    traffic class. It uses a `compute` port only for GPU-direct on a host
    whose policy is `gpu_nic: rail`, and then with its traffic class.
    The default policy is `gpu_nic: storage`: GPU-direct goes through the
    storage NIC, at the cost of a hop through the CPU's root complex
    instead of the GPU's PCIe switch;
  - the readiness check warns when a `converged` or `compute` port has
    no separate traffic class for hyperstor, or when the class's PFC
    priority isn't lossless on the NIC.
- **NVIDIA SHARP.** Hyperstor doesn't use in-network reduction:
  - SHARP's operations (sums, minimum and maximum, bitwise operations)
    can't compute Reed–Solomon parity, which needs Galois-field
    arithmetic; only single-parity XOR would fit, and EC pools need
    `m ≥ 2`;
  - replication needs reliable point-to-point transfers, which SHARP
    doesn't provide;
  - SHARP trees are set up per job for a fixed group of members, and
    switches hold few of them at a time.

  On an InfiniBand fabric where the aggregation manager (`sharp_am`)
  runs, hyperstor's service level must differ from the ones SHARP and
  NCCL use. The subnet manager should then map it to its own virtual
  lane, so collectives and storage don't queue behind each other. The
  host can't read the switches' SL-to-VL tables, so this is an
  installation step, with a readiness warning when hyperstor's service
  level equals the one in the host's NCCL or SHARP configuration
  (`NCCL_IB_SL`, `SHARP_COLL_*`).
- **Fabric requirements** (RoCEv2): a lossless traffic class (PFC) or
  ECN with DCQCN congestion control, consistently on NICs and switches.
  The node's readiness check (as in host-topology §5) reports, per RDMA
  device: the link layer, MTU, the RoCE mode and GID table, the PFC and
  trust settings of the NIC, and whether ECN is on. A mismatch is a
  warning, not a refusal, since switches can't be checked from the host.
  On InfiniBand it checks the port state, the subnet manager and the
  partition key.

## 14. S3 gateway (D36)

### 14.1 Model

- `hyperstor-s3` is stateless. Any number run behind a load balancer;
  each is a client of the object layer and of `hyperstor-kv`.
- **Buckets** belong to a tenant (a glidex project, or an account when
  standalone) and to a pool, chosen at creation (default: the site's
  EC pool for objects).
- **Bucket index** in `hyperstor-kv`: one key per object (and per
  version, with versioning), holding size, checksum, ETag, content type,
  user metadata and the object's data location. Listing is a range scan
  of these keys, so `ListObjectsV2` with a prefix and delimiter is
  ordered and paginated without a separate index.
- **Object data** goes to `s3` chunk objects named
  `(bucket id, object id, part, chunk_index)` and placed by hash, so a
  large object is spread over many PGs and read in parallel. Objects
  up to 64 KiB are stored inline in their index entry.
- **Writes** go to new object ids, never over existing data. A PUT writes
  the data, then inserts the index entry in one KV transaction, which
  is the moment the object becomes visible. Overwritten and deleted data
  is removed by a background collector once no index entry points at it.
  The data never changes after it is written, so full-stripe EC writes
  (§8.6) apply to nearly every byte.
- **Multipart upload:** each part is written as its own chunk objects;
  completion is one KV transaction that writes the index entry naming
  the parts in order. Nothing is copied.

### 14.2 API

- **First milestone:** PUT, GET (with ranges), HEAD, DELETE and
  multi-object delete, `ListObjectsV2`, `ListBuckets`, create and
  delete bucket, multipart upload (create, upload part, complete, abort,
  list), copy object, conditional requests (`If-Match`,
  `If-None-Match`, including conditional writes), and the checksum
  headers (CRC32C is stored natively, others computed on write).
- **Authentication:** SigV4 (headers and presigned URLs). Access keys
  belong to a tenant and are stored in the credential store, as secrets
  never shown again after creation (credentials.md). In glidex, keys are
  issued per project and bucket access is checked with Cedar.
- **Later:** versioning, lifecycle rules, object lock, bucket policies,
  replication between clusters, event notifications.
- TLS on the S3 endpoint always, with the site's certificate.

### 14.3 S3 over RDMA

- A client using cuObject or hipObject sends an ordinary S3 request with
  an `x-amz-rdma-token` header describing its buffer (GPU or host
  memory) for RDMA.
- **GET:** the gateway resolves the object, then asks the storage nodes
  holding its chunks to write each chunk's range into the client's
  buffer by RDMA, at the right offset. When all have completed, it
  answers the HTTP request with `x-amz-rdma-reply` and no body. The data
  doesn't pass through the gateway.
- **PUT** and **upload part:** the same in the other direction; storage
  nodes read the data from the client's buffer as they write it.
- **Transport:** the token names a DC target on the client's NIC, which
  any storage node can reach without a connection of its own. The
  milestone therefore includes DC queue pairs on mlx5 devices
  (§13.2). Requests without a token, or from clients not reachable by
  RDMA, are served over HTTP as usual.
- **Authorization** is the same as for the HTTP request: the token only
  says where the data goes; the signed request says what may be read or
  written.

### 14.4 Ports

| Port | Protocol | Between |
|---|---|---|
| 443/tcp (configurable) | HTTPS, S3 | clients → `hyperstor-s3` |

## 15. Failures

| Failure | What happens |
|---|---|
| A storage daemon crashes | Its PGs go to new primaries in a new `pg_epoch` after 10 s. When it restarts it replays its engine log, rejoins, and catches up by recovery. |
| A node is down for good | After `down_out_interval` its PGs are re-placed and backfilled (§5.3). |
| A device fails | It is marked `failed`; its PGs are re-placed at once, without waiting for `down_out_interval`. |
| A replica is slow | It is dropped from the in-sync set after the operation timeout, and caught up by recovery. |
| The primary is cut off from its replicas | Its read leases run out and its writes are refused for the old epoch; a new primary takes over after the leases (§8.4). |
| The metadata service loses quorum | I/O, lease renewals, releases and ordinary volume operations continue on the current map. PG failover, rebalancing, blocklisting and so forced lease takeovers wait (D23). A PG that loses a member meanwhile can't shrink its in-sync set, so its writes stall until quorum is back. |
| The client's host dies | Its volume leases expire; the next attachment is a forced takeover with a blocklist round (§9). |
| A `.meta` PG is unavailable | The volumes whose headers it holds can't be attached, snapshotted or renewed; I/O on already-attached ones continues, and their holders keep writing past lease expiry until the PG is back, since nobody else can be granted the lease meanwhile. |
| `hyperstor-vhost` crashes | VM I/O pauses until the service restarts and the hypervisor reconnects (§11.1). |
| A checksum mismatch | The read is served from another copy and the bad copy is repaired (§8.2). |

## 16. Management API and CLI

- **gRPC** `hyperstor.v1` on port 8850, served by every
  `hyperstor-meta` member (§4.2):
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
  through glidex's own API and Cedar policies (§17).
- **CLI:** `hyperstorctl` for standalone use and for operators (`cluster
  init`, `node add`, `device add`, `pool create`, `volume …`,
  `status`). In glidex, the usual path is `gxctl datastore` (§17).
- **Health** is `ok`, `warn` (degraded PGs recovering, nearfull, a
  device slow) or `error` (PGs below `min_size`, full, scrub found
  damage that wasn't repaired), with the reasons listed.
- **Metrics** in Prometheus format on each daemon's admin port:
  - IOPS, bandwidth and latency histograms per reactor, per pool and
    per volume;
  - recovery and backfill progress;
  - device wear and capacity.

## 17. glidex integration

This builds on storage.md S6 (`glidex-stord` and `VolumeBinding::Block`,
PR #114) and adds a `hyperstor` driver behind the `StorageDriver` trait
(storage.md §7).

| glidex | hyperstor |
|---|---|
| `Datastore{driver: hyperstor}` | parameters: management endpoints, pool, the admin identity's credential (credentials.md). Topology `shared`. |
| `capabilities` | block, linked clones, expand, shared, snapshots (new). No shrink. |
| `probe` | the node runs `hyperstor-vhost` and reaches the metadata service: the node reaches the datastore. |
| `create_volume` | blank: create. From an image copy: clone of the image's golden snapshot. From a volume: snapshot, clone, then flatten in the background. |
| `delete_volume`, `expand_volume`, `capacity` | delete, expand, pool free space |
| `stage` / `unstage` | attach (`rw` lease) and a vhost-user socket: `VolumeBinding::VhostUserBlk{socket}` (new). Detach on unstage. |
| image copies (S9) | one per datastore: a volume holding the image, plus its snapshot `golden` |
| linked clones (S8) | clones of `golden` (D21), instant |
| extend-root | on the worker: attach through a ublk device mapped by stord, run `images::partition` on it as for `Block`, detach |
| disk move (S5) | through two bindings on the worker, as S6c does for `lvm` |
| volume lock (S16) | replaced by the lease (D11); the shim takes it at start through `hyperstor-vhost` |
| metering `disk.stored` | volume allocated bytes, head plus snapshots |

- **Hypervisor arguments.** `VhostUserBlk` gives Cloud Hypervisor
  `--disk vhost_user=true,socket=…` and QEMU a `vhost-user-blk-pci`
  device, with the VM's memory backend shared (§11.1). Command-line pin
  tests cover both.
- **Disk snapshots in glidex** (API, Cedar, UI) are a separate glidex
  spec change. storage.md lists disk snapshots as out of scope today, and
  hyperstor is the first datastore that can do them.
- **Bare-metal GPU hosts** aren't glidex nodes. An operator exports a
  volume or snapshot to a host identity (§16) through `gxctl datastore`
  sub-commands that call the management API.

## 18. Deployment

### 18.1 Crates and binaries

| Crate | Builds |
|---|---|
| `hyperstor-proto` | wire types for D5 and the gRPC API |
| `hyperstor-reactor` | the io_uring reactor, executor, RDMA and buffer pools |
| `hyperstor-engine` | the local engine (§7) |
| `hyperstor-meta` | the metadata service binary |
| `hyperstor-daemon` | `hyperstord` |
| `hyperstor-client` | the client library |
| `hyperstor-vhost` | the vhost-user-blk service |
| `hyperstor-gw` | the NVMe-oF gateway |
| `hyperstor-gpu` | `libhyperstor-gpu` and the two interposers (`cdylib`) |
| `hyperstor-ctl` | `hyperstorctl` |
| `hyperstor-kv` | the transactional key–value store (§7.5) |
| `hyperstor-s3` | the S3 gateway (§14) |
| `glidex-raftstore` (in the glidex repository) | the Raft store shared with glidex's control plane (D20), a pinned git dependency |

### 18.2 Units and privileges

- Units: `hyperstor-meta.service`, `hyperstord.service`,
  `hyperstor-vhost.service`, `hyperstor-gw.service`. All run as the
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
  is marked `trusted` or not (D25) and lists its transports (§13.2). On
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

- mTLS everywhere on TCP, against a cluster CA issued by glidex-tls (or
  by `hyperstorctl cluster init` when standalone). Encryption on
  untrusted networks per D25.
- RDMA queue pairs are reliable-connected and are set up only over an
  authenticated TCP session. Only storage nodes start RDMA reads and
  writes, into buffers their clients named (D13).
- NVMe-oF hosts: allowlists and DH-HMAC-CHAP (§11.2). NVMe/TCP TLS is
  later.
- A tenant's VM never reaches the storage network in the vhost path. In
  the GPU VM path (D18) it does, through its RDMA VF. That VF is put on a
  storage network or IB partition per tenant with host-topology §10's VF
  settings (VLAN, MAC, spoof check), and the host identity is per VM.
- Encryption at rest is later. The engine format reserves a key-id field
  per lineage so it can be added without reformatting.

## 20. Edits to other documents (when implemented)

- **storage.md §11:** the `hyperstor` driver, `VolumeBinding::VhostUserBlk`,
  and the snapshot capability.
- **host-topology.md:** hyperstor's reserved cores, namespaces and NICs
  (§18.3).
- **clustering.md §18:** the `--fenced` question for VMs whose disks are
  all on hyperstor (§9).
- **security.md:** hyperstor's identities and networks.
- **architecture.md:** hyperstor's processes.
- **tenancy.md, credentials.md:** S3 access keys per project, and
  Cedar actions for buckets and objects (§14.2).
- **hypervisors.md:** a vhost-user-fs device for VMs mounting the
  hyperstor filesystem (filesystem.md); a virtio-vsock device for VMs with GPU-direct
  storage (D33); vhost-user-blk hot-plug, for the later CSI driver
  (§11.4).

## 21. Milestones

| # | Content | Depends on |
|---|---|---|
| HS0 | Reactor prototype (io_uring passthrough, TCP, buffers) and gate G5 on a test host. Engine prototype with the general object format (D34: data, attributes, key–value tables, operation lists) and crash tests (log replay, atomicity, D22), on both device back ends. | — |
| HS1 | `glidex-raftstore` extracted (a glidex change); `hyperstor-meta` with the map, pools, PGs and placement; `hyperstord` with the engine; replicated pools over TCP; the `.meta` pool with headers, directories and leases; the client library; ublk devices for testing. fio through ublk. | HS0 |
| HS2 | Failures: peering, read leases, recovery, backfill, scrub, down and out, the blocklist and forced takeovers (§8.4–§9). A fault-injection test rig (kill, partition, slow disk). | HS1 |
| HS3 | `hyperstor-vhost` with adaptive polling and guest-memory registration (§11.1), client readahead, and the glidex driver (§17): create, delete, expand, stage, images as golden snapshots and clones, metering. | HS2, storage S6 |
| HS4 | Snapshots, groups, clones, rollback, flatten, snapshot deletion; the host read cache (§11.3). glidex disk snapshots spec. | HS2 |
| HS5 | NVMe-oF: the ublk and `nvmet` stopgap, then our NVMe/TCP target with ANA, reservations and CHAP. | HS2 |
| HS6 | RDMA: the D5 protocol over verbs (RoCEv2 and InfiniBand), shared receive queues, credits, transport choice per network, port roles and traffic classes, fabric readiness checks (§13.2). No NVMe-oF needed. | HS2 |
| HS6b | NVMe/RDMA in our target. | HS5, HS6 |
| HS7 | EC pools with the write log and full-stripe writes (§8.6). | HS2 |
| HS8 | GPU-direct on bare metal: `libhyperstor-gpu`, the interposers, `FIEMAP` mapping, read-only multi-attach. G1, G3, G4. The hipFile upstream proposal. | HS4, HS6 |
| HS9 | GPU-direct in glidex VMs: lease delegates, the vsock credential, virtio disks in the interposer (§12.3, G2); server-side QoS; PG splitting. | HS8 |
| HS10 | Hard-disk pools with flash log devices (D29). | HS2 |
| HS11 | `hyperstor-kv`: ranges, splits and merges, the root range, transactions, the timestamp oracle, garbage collection. Tested against a model of snapshot isolation (Jepsen-style histories) under faults. | HS2 |
| HS12 | The S3 gateway over HTTP (§14.1, §14.2), with glidex access keys and Cedar. | HS7, HS11 |
| HS13 | S3 over RDMA, with DC queue pairs (§14.3), tested against cuObject and hipObject clients. | HS6, HS12 |
| HS14+ | The distributed filesystem, per filesystem.md. | HS11, HS13 |
| — | A libfabric transport, when a site has UET, EFA or Slingshot NICs with production drivers (D30). Queued, no milestone yet. | HS6 |

## 22. Questions

Decisions are settled. These need checking on real hosts before the
sections that depend on them are final:

- **G1. cuFile's userspace-FS path without nvidia-fs.** Does `libcufile`
  accept a userspace-FS handle on a P2PDMA-only install (open kernel
  modules, no `nvidia-fs.ko`)? The interposer ignores `desc_str` and
  registers the buffer itself through dma-buf, so the format of `desc_str`
  doesn't matter. If `libcufile` refuses the handle, the cuFile
  interposer serves reads and writes itself, as the hipFile one does
  (D15).
- **G2. P2P inside a guest** (D18). Do `ibv_reg_dmabuf_mr` and the GPU
  drivers' dma-buf export (which checks `pci_p2pdma_distance`) accept a
  GPU and an RDMA VF passed through to one VM? Check QEMU with a virtual
  PCIe switch, and Cloud Hypervisor.
- **G3. Extent stability** (D16). On xfs and ext4, check that a read
  lease and `FIEMAP` flags (`UNWRITTEN`, `DELALLOC`, `SHARED`) are enough
  to keep a mapped extent valid while a native I/O is in flight. Check
  that dropping cached pages (`fadvise(DONTNEED)`) after a native write
  keeps buffered readers on the same host coherent.
- **G4. Interposing.** Look up the cuFile and hipFile symbols with
  `dlsym(RTLD_NEXT)` and don't ship vendor headers. Confirm the cuFile
  EULA allows this.
- **G5. io_uring versus SPDK** (D3). On a storage test host, measure
  IOPS and latency per core for 4 KiB random reads and writes, and
  bandwidth for 1 MiB I/O. Compare `uring_cmd` passthrough with polled
  queues against SPDK's `perf`, and our NVMe/TCP target against SPDK's.
  This is a gate in the first milestone. It doesn't bring SPDK back; it
  records the gap and points to what to optimise.
- **G6. vhost-user reconnect in Cloud Hypervisor** (§11.1). Does a
  `vhost_user=true` disk survive a restart of its backend, and how long
  does the guest wait? If not, `hyperstor-vhost` restarts must be
  avoided for running VMs (upgrades by live migration later) or the
  feature must go upstream.
- **G7. Passthrough without root** (§18.2). Since Linux 6.2 the NVMe
  driver allows read, write and dataset-management commands without
  `CAP_SYS_ADMIN` when the device is open for writing (`nvme_cmd_allowed`).
  Confirm on the kernels we ship, and list the admin commands the engine
  needs (identify, format) so that stord can run those at hand-over.
- **G8. RDMA registration of guest memory** (§11.1). Measure, for large
  VMs, the attach-time cost of registering the whole guest memory with
  ODP and with pinned hugepages, and the cost of ODP page faults on the
  first I/O to each region. Decide whether NICs without ODP need a
  registration cache of bounce buffers instead.
- **G9. Hard disks behind one log device** (D29). Measure write
  latency and throughput for eight hard disks sharing one SATA SSD and
  one NVMe log device, to set the limit per log device by class.
- **G10. GPU-direct alongside NCCL** (D31). On a GPU host, run a
  checkpoint write burst through `libhyperstor-gpu` during an all-reduce
  benchmark (`nccl-tests`, `rccl-tests`), on a separate storage NIC and on
  a converged one with its own traffic class. Measure the slowdown of
  each, to check the defaults.
- **G11. Native path against the kernel path on Grace systems** (§12.4).
  On a Grace Hopper or Grace Blackwell host, compare `libhyperstor-gpu`
  with the kernel path and its bounce buffer for model loading and
  checkpoint writes, to see whether the native path is still worth its
  cost there.
- **G12. The `x-amz-rdma-token` format** (§14.3). Derive it from
  hipObject's MIT-licensed source, which interoperates with cuObject
  servers, and confirm with a cuObject client that a Rust
  implementation is accepted. Check whether the format is published,
  and whether its use needs any agreement with NVIDIA.
