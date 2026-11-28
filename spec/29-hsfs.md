# Datasets and hsfs

One noun, the **dataset**, over volumes, filesystems and buckets, with
inherited properties, snapshots, clones, send and receive, and a native
client that reads and writes many storage nodes at once. Part of the
hyperstor design; sections keep their numbers across documents, so `§N`
in one document is the same section everywhere (see
[README.md](README.md)).

Status: **design; H0 built, H1 begun** (01-implementation-notes.md I439,
I440, I442: the sealing keys, the capability's format, storage nodes
holding the keys, and reactors checking capabilities in pools that
require them; nothing issues a capability yet, and peers aren't told
from clients until D25. I459: dataset names over volumes and buckets,
every property kept and inherited, `pool` enforced, and `hsfs`; I461:
`readonly` for volumes; D387–D389 and D415–D416 settled 2026-11-27, points 351–353, 365 and 366). The order agreed with the owner (2026-11-25):
H0, then the data path's mutual TLS (D25), then H1 and H2. D360–D371 are settled with the owner
(05-decision-log.md points 323–334, 2026-11-24 and 2026-11-25), D361 (the name `hsfs`)
and D367 (no striping across clusters) among them. Host checks G62–G66
are still to run. It builds on the
filesystem ([00-filesystem.md](00-filesystem.md) F1–F12), volumes
(10-volumes.md §10), tenants and projects (20-tenants.md §24), NFS
([22-nfs.md](22-nfs.md) §26) and federation (21-federation.md §25).

## 32. Datasets and hsfs

### 32.1 Why

- Volumes, filesystems and buckets each have their own commands, quotas,
  snapshots and grants, or will. An operator learns three surfaces; a
  tenant's plan is set in three places.
- ZFS shows the model that works: datasets in a tree, properties
  inherited down it, snapshots and clones that cost nothing to take,
  `send` and `receive`, delegation. We take the model, not the code or
  the name.
- ZFS is one host. Its range locks, transaction groups and ARC live in
  one kernel. hsfs is the same model where every client talks to many
  storage nodes, so its concurrency rules (§32.5) and its trust rules
  (§32.6) are its own.

### 32.2 The dataset (D360)

- A dataset is a **volume**, a **filesystem** or a **bucket**. Its name
  is `tenant/project/name`, and a filesystem may have child datasets
  below it (`tenant/project/home/alice`).
- Tenants and projects (D299–D304) are the top two levels. They hold
  properties and no data.
- A child filesystem is a directory of its parent with its own
  properties, snapshots and usage. Crossing into it is like crossing a
  mount: a rename or hard link across the boundary is refused (`EXDEV`).
- Existing commands stay: `hyperstorctl volume …` is the dataset
  commands with `type=volume`.
- A name is unique in its project (D387): a new volume, clone or bucket
  may not take one another dataset there has; a copy named by none is
  `NAME-copy`. Two that shared a name before are addressed as
  `name@<volume id>` and `name@bucket`.

Why: one place for quotas, grants, snapshots and replication, and
glidex maps a project to one subtree.

### 32.3 Properties

Set on any level, inherited by everything below unless set there.
`hsfs get` says where each value comes from.

| Property | Applies to | Meaning |
|---|---|---|
| `quota`, `refquota` | all | Limit with, or without, snapshots and children |
| `reservation` | all | Space kept for the subtree |
| `pool`, `chunk`, `stripe` | all | Layout of new data (F4, D130); fixed per file or volume at creation |
| `compression`, `encryption` | all | As the pool allows; a key per branch (D162) |
| `readonly` | all | Refuses writes at the metadata server and in capabilities (§32.6) |
| `reads` | all | `primary` or `any` (D140) |
| `weight`, `class` | all | Fair share and priority class (D46, D62) |
| `snapdir` | filesystem | Whether `.snapshot/` is visible |
| `nfs` | filesystem | The export's options (D141 becomes this property) |
| `s3` | bucket | The bucket's gateway options |
| `sync` | all | Replication policy for `hyperstor-sync` (D64) |

Why inherited: a tenant's plan is one `set` at the tenant; a project
overrides what it needs. Layout stays fixed per file because moving
data on a property change would be a hidden rebalance.

### 32.4 The client, in three layers

One core in Rust (`hyperstor-client`, F8), three ways in:

1. **Objects.** A dataset handle gives objects by id: ranged reads and
   writes, attributes, and atomic operation lists (I68). Requests go
   straight to PG primaries, many at once. Public but small (D364): no
   transactions across objects; those are `hyperstor-kv`'s.
2. **POSIX.** The filesystem core of F1–F12, as a mount (FUSE over
   io_uring, virtio-fs) and as a library an application links without a
   mount.
3. **Administration.** The `hsfs` command (D361), the cluster API and
   the UI:

```text
hsfs create [-o prop=value] [-V size | -b] tenant/project/name
hsfs set prop=value … name      hsfs get [-r] prop|all name
hsfs list [-r] [-t volume|filesystem|bucket|snapshot] [name]
hsfs snapshot [-r] name@snap    hsfs rollback name@snap
hsfs clone name@snap newname    hsfs promote name
hsfs destroy [-r] name[@snap]   hsfs rename old new
hsfs send [-i @from] name@snap  hsfs receive name
hsfs allow|unallow who perm… name
hsfs mount name /path           hsfs unmount /path
```

**The name (D361).** The filesystem, its mount type
(`mount -t fuse.hsfs`) and the command are `hsfs`. `hsfs` is a small
binary over the same cluster API as `hyperstorctl dataset`, which stays
for scripts that already use `hyperstorctl`. Never `zfs`: OpenZFS
installs that command on the same hosts, and its semantics differ where
it matters (one host against many).

**How a client reaches it (D369).**

| Client | Path |
|---|---|
| glidex VMs | virtio-fs from `hyperstor-vhost`; no daemon in the guest |
| Bare metal, Kubernetes | FUSE over io_uring: every program, no kernel module |
| Data-heavy jobs on those hosts | The same mount, plus `libhsfs-preload.so`: I/O on the mount's paths goes to the client core in process, with no kernel crossing |
| Applications that link the library | The POSIX or object layer in process (D364) |
| GPU-direct | `libhyperstor-gpu` (F9) |
| Hosts that can't run our daemon | pNFS (the `nfs` property) |

The preloaded library is a fast path, never the only one: what it
doesn't catch falls through to the mount, and both go through the same
client core's rules (§32.5), so a process with it and one without see
the same file. No kernel module (00-filesystem.md §1).

**Parallel and striped I/O.** A file's chunks are spread by stripe
groups over `W` PGs (D130), and PGs over nodes by placement
(07-placement.md §5). The client core splits a read or write by chunk
and sends the parts to their primaries together, on a connection per
reactor (D39). One large file is read from, and written to, as many
nodes as its stripe width reaches; many files reach every node. There
is no server between the client and the primaries (F5). G62 measures
how throughput grows with stripe width and client count.

### 32.5 Concurrent reads and writes (D365)

| Case | Rule | Why |
|---|---|---|
| Writes to different blocks of a file, any clients | No coordination; each is atomic at its primary (D22) | What parallel writers of checkpoints and shards need |
| File size | Committed as a maximum (as D132) ; only truncate shrinks it | Appenders at known offsets commit in any order |
| Overlapping writes | Last writer wins per 4 KiB block; a write is never torn inside a block | No distributed lock on the data path |
| `O_APPEND` | The metadata server assigns the offset, then the client writes there | The only way two appenders can't collide |
| Truncate, deallocate, clone of a range | Bumps the file's data version; primaries refuse writes carrying an older one, and the client re-reads the inode | The same race D132 closes with layout recalls |
| Visibility | Close-to-open; `O_DIRECT` immediate (F6) | Unchanged |
| Exclusion | Advisory locks (F7) | For applications that need it |

No byte-range leases. Strict coherence of cached data between clients
stays out of scope (00-filesystem.md §1).

### 32.6 Trust: sealed dataset capabilities (D363)

**The gap.** Tenancy isolates by capability at attach time: a volume
lease names the tenant (20-tenants.md §24.3). A mounted filesystem has
no lease per object. A tenant with root on a bare-metal host holds a
valid host certificate and could ask a primary for another dataset's
chunks.

**The rule.**

- At mount, the metadata server checks F12 and the dataset's grants and
  issues a **capability**: tenant, lineage, branch, access (`ro` or
  `rw`), the client's session and authenticated host, and an expiry,
  under a MAC with the cluster's sealing key (the key, key ids and
  rotation of D133).
- Storage nodes hold the sealing keys: each fetches them from the
  cluster API on a thread of its own, at start and every 30 s (D370).
- A pool requires capabilities only when flagged, off by default (D371).
- The client presents its capabilities when it opens a connection to a
  reactor. The reactor checks the MAC and the connection's host once and
  keeps `(tenant, lineage, branch, access)` with the connection. Each
  request is then a table lookup on the chunk's lineage. A request for a
  lineage the connection holds no capability for is refused.
- The tenant id feeds the reactor's fair-share queues (D62), as a
  lease's does.
- Capabilities are renewed with the session (F7). A session that
  expires is blocklisted (D24), which revokes them everywhere.
- `readonly`, a snapshot mount and a `ro` grant give a `ro` capability:
  the primary refuses writes, not only the client.
- virtio-fs needs none in the guest: `hyperstor-vhost` holds the
  capability and the guest never reaches the storage network.

What it does not do: enforce one user against another inside a mount.
POSIX permissions are checked by the client core and the metadata
server, as an NFS server trusts a host's uids (D135). The capability is
the tenant's wall.

Why: no lookup per I/O, as §24.3 asks; one sealing mechanism for pNFS
and native clients; and the bare-metal FUSE mount becomes as safe as
the pNFS export.

### 32.7 Snapshots, clones, send and receive (D362, D366)

- **A filesystem dataset has a lineage and branches**, as a volume has
  (D21). File chunks are keyed `(lineage, inode, chunk)` with versions
  by branch and epoch; a stripe group's base is
  `hash(lineage, inode, segment)`. This amends F4 and D130, which hashed
  the filesystem's id.
- **Snapshot:** one `hyperstor-kv` timestamp for the metadata and one
  epoch for the chunks, taken in one transaction. Constant time. `-r`
  takes the same timestamp for every child.
- **Clone:** a new branch of the same lineage, reading metadata at the
  snapshot's timestamp. A clone's unwritten chunks are its parent's
  versions in the same PG on the same devices, so reading through never
  crosses the network. Inode numbers are handed out per lineage (F3), so
  branches never collide.
- **Rollback** to the newest snapshot in place; **promote** swaps a
  clone with its origin.
- **Send and receive** are the interface to `hyperstor-sync` (D64):
  `send -i` is the versions between two snapshot epochs. Within a
  federation a `sync` policy does it continuously; `hsfs send` to a
  stream is for moving a dataset by hand.

Why lineage in the key now: it is on-disk naming. Deciding it after
files exist would mean rewriting every chunk.

### 32.8 Across clusters (D367)

**One file is never striped across clusters.** A PG never spans a
wide-area link (D63, 21-federation.md §25.1), and a file's chunks live
in the pools of the cluster that is home to its inode's range (D65).

| Where the nodes are | Parallel, striped reads | Parallel, striped writes |
|---|---|---|
| One cluster, one site | Yes | Yes |
| One cluster, several sites within 1 ms (D63) | Yes; `reads=any` prefers the reader's site | Yes; each write waits for the copies its pool places at other sites |
| Another cluster, data replicated here (D64) or in a sourced cache pool (D67) | Yes, from the local copy, which is a snapshot | No: the copy is immutable |
| Another cluster, data only at its home | Yes, striped over the home's nodes across the link | Yes, to the home's nodes across the link; metadata forwards to the home (D65) |
| Home unreachable | Local copies only | Creates and data commits queue (D66) |

Why not stripe one file over two clusters: every write would wait for
the slower link, the file would be unreadable when either cluster is
away, and placement, epochs and fencing are per cluster. What users
want from it, speed and local reads, comes from wide stripes at the
home and snapshot replicas elsewhere. A namespace can still span
clusters: sibling datasets homed at different clusters under one tree
(D65), each striped inside its own.

### 32.9 Against pNFS

Both are doors to the same datasets; pNFS is the `nfs` property.

| Concern | hsfs client | pNFS |
|---|---|---|
| Tenant VMs under glidex | Better: virtio-fs, no storage network in the guest | Guest needs routes to MDS and data servers |
| Tenant-root bare metal | Equal once D363 is built | Sealed stateids per open (D133) |
| Metadata scaling | Stateless servers, any takes any operation (F1) | One MDS instance per export (D134, G38) |
| Snapshots, clones, quotas | First class | A directory, or invisible |
| Unmodified clients | Needs our daemon | Stock Linux |
| GPU-direct | Native handle (F9) | None |

### 32.10 Order of work (D368)

Part of HS14, in stages. N0 (22-nfs.md §26.10) needs H0–H2.

| Stage | What | Needs |
|---|---|---|
| H0 | Sealing key in the map service; capabilities at reactors; `ro` enforced | D133's key |
| H1 | Dataset names and inherited properties over volumes and buckets; `hsfs` for them | HS15's tenant ids |
| H2 | The POSIX core: `hyperstor-fsmeta`, FUSE and virtio-fs mounts, lineage-keyed chunks, §32.5 | HS11b |
| H3 | Snapshots, clones, rollback, promote; child datasets | H2 |
| H4 | `send`, `receive` and `sync` policies for filesystems | H3, HS16 |

### 32.11 Host checks

- **G62. Striped throughput** (§32.4). On the pilot, one client reading
  and writing one large file at stripe widths 1, 4 and 16, then 1 to 8
  clients on one file at different offsets: throughput against width
  and clients, and where it stops growing (client CPU, network or
  devices).
- **G63. Capability checks cost nothing per I/O** (D363). 4 KiB reads at
  queue depth 1 and 32 with and without the per-connection lineage
  table; the difference should be within noise.
- **G64. FUSE mount type and helper** (D361). That `mount -t fuse.hsfs`
  and an `/etc/fstab` line work through a `mount.fuse.hsfs` helper on
  the distributions we ship, and that no package claims `hsfs` (Solaris
  used the name for its ISO 9660 driver; check Linux distributions).
  The name half holds for Debian 13 and Fedora (I459, 2026-11-27);
  Ubuntu's archive and the mount half are still to check.
- **G66. What interception catches** (D369). With `libhsfs-preload.so`:
  Python (CPython's `open`, NumPy, PyTorch's `DataLoader` workers after
  `fork`), `dd`, `cp`, `tar`, a Go binary and a statically linked one.
  Which go in process and which fall through to FUSE, and that a file
  written one way reads the same the other. Then the gain: a data
  loader's throughput with and without it (with FG2's numbers).
- **G65. Stale data versions** (§32.5). A writer racing a truncate from
  another client: no write lands in a truncated range, under a
  randomized test against a model.
