# Hyperstor filesystem

Status: **outline.** The approach is settled ([04-decisions.md](04-decisions.md)
D37); F1–F12 below follow from it and are open to review. The sections
after §3 are to be drafted before the filesystem milestones
(17-milestones.md §21, HS14+).

A native distributed filesystem on hyperstor. Several VMs on different
hosts, bare-metal hosts and Kubernetes nodes mount the same filesystem.
Its metadata lives in hyperstor's transactional key–value store
(`hyperstor-kv`, 04-decisions.md D35), its file data in hyperstor objects
(D34). It is meant first for shared working data in VMs and for training
datasets and checkpoints shared by many GPU nodes.

## 1. Scope

- **In:**
  - a POSIX namespace: files, directories, symbolic and hard links,
    extended attributes, permissions, ownership and timestamps;
  - several filesystems per cluster, each belonging to a tenant;
  - mounts in glidex VMs through virtio-fs, without a storage network in
    the guest;
  - mounts on bare-metal hosts and Kubernetes nodes through FUSE over
    io_uring;
  - GPU-direct reads and writes through `libhyperstor-gpu` as a cuFile
    and hipFile userspace filesystem;
  - POSIX advisory locks (`fcntl` byte ranges and `flock`);
  - replicated or erasure-coded file data, chosen per directory tree.
- **Later, designed in:**
  - filesystem snapshots;
  - quotas per directory tree;
  - POSIX ACLs;
  - NFSv4.1/4.2 with pNFS for unmodified clients (proposed in
    [22-nfs.md](22-nfs.md), D127–D137); SMB;
  - a Kubernetes CSI driver (`ReadWriteMany`);
  - one filesystem across clusters, through range homes and the
    partition allowlist (D65, D66, 21-federation.md §25.4).
- **Out:**
  - a kernel filesystem client;
  - strict POSIX coherence of file data between clients (F6).

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| F1 | **Stateless metadata servers** (`hyperstor-fsmeta`). Any server handles any operation on any filesystem, as a client of `hyperstor-kv`. They scale by adding servers and fail over by retrying on another. | No subtree ownership to balance, journal to replay or lease to hand over, as stateful metadata servers need. The transactional store gives atomicity and isolation (the DeepSeek 3FS model). |
| F2 | **Key schema.** Inodes are `i/<fs>/<inode>` (attributes, layout, data version, link count). Directory entries are `d/<fs>/<parent>/<h>/<name>` (child inode and type), where `<h>` is a short hash of the name. | `readdir` is a range scan under the parent's prefix. The hash spreads a busy directory's new entries over every range its span covers, so creating many files in one directory doesn't load one range. POSIX doesn't require `readdir` to be sorted. Inodes apart from entries make hard links simple. |
| F3 | **Inode numbers are 64-bit**, handed out in blocks of 1,024 per metadata server from a counter key. | No transaction per create to allocate a number. |
| F4 | **File data is in `fs` chunk objects** named `(inode, chunk_index)` and placed by hash. Each file's layout (pool, chunk size) is fixed at creation and recorded in its inode, inherited from the directory's policy. Amended by D130 (settled 2026-10-24): chunks are placed in stripe groups, each 64 GiB segment round-robin over `W` consecutive PGs from a hashed base, so pNFS layouts stay small (22-nfs.md §26.5). | Large files spread over many PGs and are read in parallel, as volumes and S3 objects are. Directory policies let a dataset tree live on EC and a scratch tree on replicas. |
| F5 | **Clients read and write file data directly with the storage nodes.** Metadata servers aren't on the data path. Size, modification time and the data version are committed at `close`, `fsync` or after a time limit (5 s), in one metadata transaction. | Data bandwidth scales with storage nodes, not metadata servers. |
| F6 | **Metadata operations are atomic and immediately visible everywhere; file data is close-to-open.** Opening a file reads its inode; a client keeps its cached data only if the data version hasn't changed. `close` and `fsync` make a client's writes visible to opens that follow. `O_DIRECT` reads and writes bypass caches and are visible at once. | What training data, checkpoints and shared working directories need, without data leases and invalidation traffic between clients. NFS works this way. |
| F7 | **Advisory locks are records in `hyperstor-kv`**, held by a client session. A session has a time-to-live renewed by the client; when it expires, the client is blocklisted (04-decisions.md D24) and its locks are released. | Locks survive metadata server restarts, and a dead or partitioned client can't keep writing under a lock it lost. |
| F8 | **One client core in Rust with two front ends.** `hyperstor-vhost` serves virtio-fs (vhost-user-fs) to VMs; a FUSE daemon over io_uring serves bare-metal and Kubernetes nodes. Both speak the FUSE protocol, so the request handling is shared. | VMs need no storage network, and their host's reactors do the I/O. No kernel module to maintain. |
| F9 | **GPU I/O registers hyperstor files as cuFile and hipFile userspace-filesystem handles directly.** `libhyperstor-gpu` reads the file's layout from its inode, with no `FIEMAP` mapping (04-decisions.md D16) and no kernel path behind it. | This is what the userspace-filesystem handle was designed for. Every read and write of a file can go native, not only stable extents. |
| F10 | **Caching:** the client keeps file data in the kernel's page cache (FUSE `keep_cache` when the data version is unchanged on open), and metadata with short timeouts (1 s by default) refreshed on open. The host read cache of 04-decisions.md D32 serves only filesystem snapshots, the only immutable file data. | Close-to-open (F6) needs no more than this. |
| F11 | **Open-but-unlinked files** are kept by an open-file record per session in `hyperstor-kv`. A file's data is removed when its link count is zero and no session has it open, or when the sessions holding it expire. | POSIX semantics for unlink-while-open, across clients. |
| F12 | **Mounts are authorized per VM or per host identity.** A filesystem belongs to a tenant. For virtio-fs, `hyperstor-vhost` enforces which VM may mount which filesystem and maps users (root squash by default). For FUSE, the host identity's certificate decides. | A guest can't reach a filesystem its VM isn't granted, because the host serves the mount. |

## 3. Components

1. `hyperstor-fsmeta`: stateless metadata servers (F1–F3, F7, F11).
2. The client core (F5, F6, F10), with its virtio-fs and FUSE front ends
   (F8).
3. `libhyperstor-gpu` support for filesystem files (F9).
4. Directory policies for layout (F4).
5. glidex integration: a filesystem resource, its Cedar actions, VM
   mounts (a vhost-user-fs device), gxctl and UI.
6. Management: create, delete, policies, sessions and locks, usage.

## 4. Questions to settle while drafting

- **Rename of directories:** a move must not create a cycle. The
  transaction reads the target's ancestor chain; check the cost for
  deep trees, and whether concurrent moves need a lock on the chain.
- **Hard links across directory policies:** a link keeps the file's
  layout (F4); confirm that is acceptable.
- **Snapshots:** `hyperstor-kv` keeps versions by timestamp, so reading
  metadata at a snapshot timestamp is natural; file data needs chunk
  versions by epoch, as volumes have (04-decisions.md D10). Design both
  together, with garbage collection that keeps snapshot versions.
- **Quotas per directory tree:** maintained counters along the ancestor
  chain, or periodic accounting.
- **FG1. virtio-fs in Cloud Hypervisor and QEMU:** confirm vhost-user-fs
  support in both, multiqueue, and whether a DAX window is available and
  worth using.
- **FG2. FUSE over io_uring:** check the kernels we ship (it arrived in
  Linux 6.14), and its gain over `/dev/fuse` for small-file and
  metadata-heavy workloads.
