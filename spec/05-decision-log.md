# Decision log

Status: **working record.** Decision points raised during implementation,
the owner's answers, and the points still open with their trade-offs.
Settled points become numbered decisions in [04-decisions.md](04-decisions.md);
the measurements behind them are in [02-benchmarks.md](02-benchmarks.md).

## Review of 2026-10-20

Raised in [01-implementation-notes.md](01-implementation-notes.md) after HS0,
HS1 and the first part of HS2.

| # | Point | Answer | Result |
|---|---|---|---|
| 1 | Defer small overwrites only on hard disks, redirect on flash (I14) | Redirect on flash | D38; storage nodes default to no deferral, `hyperstord --hdd` turns it on |
| 2 | One listening port per reactor (I19) | One port per reactor | D39 |
| 3 | The stale-primary window until map leases exist (I20) | Discussed ([below](#3-map-leases-before-or-after-the-vm-path)); then: leases now, pushes as the fast path | D42; built (HS2) |
| 4 | Failure detection by the map service only (I21, I25) | Nodes report each other | D40; to build in HS2 |
| 5 | Checkpoint spikes from whole-index checkpoints (I3) | Is there a paged, incremental or log-based approach with a sync marker? What data size can each support? Then: dense per-block maps for fragmented chunks, and eventually a cache-aware B+tree | D43. The interim checkpoint approach (fuzzy, then paged) is recommended [below](#5-checkpoints-without-the-spike) |
| 6 | Test hardware (NVMe namespace, RDMA NIC, power-loss-protected drives) | Later | G5 and HS6 measurements wait for it |
| 7 | CPU per operation before RDMA | Agreed | D41; milestone HS5b |

### Benchmark summary behind the review

From [02-benchmarks.md](02-benchmarks.md), dev-1 (one consumer NVMe SSD without
power-loss protection):

| Measurement | Result |
|---|---|
| Engine 4 KiB write, batch 1, against raw write + flush | 168 µs against 162 µs |
| Engine 4 KiB QD1 read against raw | 64 µs against 53 µs |
| Engine 4 KiB writes, batch 128: deferred against redirected (raw: 160k) | 85k against 133k ops/s (point 1) |
| Engine CPU per deferred 4 KiB write | about 5 µs (point 7) |
| Checkpoint stall at p99 (memory, batch 128) | 12.5 ms (point 5) |
| Cluster, memory devices: replicated 4 KiB write / read, QD1 | 92 µs / 40 µs (point 7) |
| Cluster, one client reactor, 4 KiB writes QD32 | 31k–47k ops/s |
| Placement, 1,024 devices | 2.2 µs per PG |
| Crash test soak | 5,000 seeds, 30,000 crashes, all consistent |
| Separate processes, `kill -9` of a node | every request succeeded afterwards, on two copies |

## Points settled after discussion

### 3. Map leases before or after the VM path

**Answer (2026-10-20): leases now, with pushes as the fast path: option A
below.** Built as D42: leases granted by the map service, two-phase map
changes (fence, then publish and push). The cluster test
`a_primary_cut_off_from_the_map_service_loses_no_acknowledged_write` cuts
a primary off from the map service while clients can still reach it.
With the lease check removed, the cut-off former primary serves stale
reads, and the test fails. With it, nothing acknowledged is lost and the
former primary refuses both reads and writes. Choosing map-service leases
over replica-granted read leases amends D23 (see D42).

The discussion as it stood:

**The window.** When the map changes (a node is marked down, a PG moves),
storage nodes learn it by polling (every 200 ms). Until they do, a primary
and replicas that are all still on the old map accept writes under the old
PG epoch. Replicas that already have the new map refuse, which bounds the
window, but doesn't close it.

**When it loses data.** Only when the old primary is still running and
reachable by clients, but has been taken out of the acting set: a network
partition from the map service, or a false detection. A node that has
really died accepts nothing. Writes acknowledged in the window go to the
old acting set; the new one never sees them, so they are lost when the
partition heals and backfill copies the new set's state.

**Option A: map leases and read leases now, before HS3** (design
09-replication.md §8.4, 06-metadata.md §4.1).

- How: every heartbeat reply renews a node's *map lease*. A node whose
  lease has run out stops accepting writes and reads for its PGs. The map
  service makes a change that removes a member effective only after that
  member's lease has expired or the member has acknowledged the new map.
  Primaries hold read leases from their replicas, as designed.
- Pros:
  - closes the hole: no acknowledged write can be lost to a partition or
    a false detection, which is the basic promise of a block store;
  - VMs (HS3) are the first real users of the data; building HS3 on a
    path that can lose writes would make every HS3 test suspect;
  - the write path is still small, so adding the lease check now is
    cheaper than retrofitting it under vhost, NVMe-oF and EC later;
  - D40 (peer reports) shortens detection, so the lease, not detection,
    becomes the main part of failover time, which is a number we choose.
- Cons:
  - failover takes at least one lease period after a failure (writes to
    the affected PGs stall for it), so the lease must be short (1–2 s)
    and heartbeats frequent;
  - more protocol: lease renewal, a bound on clock *rate* drift (monotonic
    clocks only, no synchronized time), and the map service's rule for
    when a change takes effect;
  - delays HS3 by roughly one milestone.

**Option B: push map changes, leases later.** The map service pushes each
new map to every node and waits for acknowledgements before the change
takes effect.

- Pros: shrinks the window to a round trip for every node that can be
  reached; less code than leases.
- Cons: a partitioned node can't be reached, which is exactly the case
  that loses data. B alone doesn't close the hole; it is the optimization
  that makes A's failover fast in the common case.

**Option C: after HS3.**

- Pros: VMs on hyperstor sooner; the lease design can use what HS3 teaches
  about failover time that VMs tolerate.
- Cons: the window stays open through HS3 testing; anything built on top
  assumes guarantees that aren't there yet.

**Recommendation: A, with B as its fast path, before HS3.** A 1–2 s lease,
pushes for the common case, and the lease for safety.

## Open points

### 5. Checkpoints without the spike

**Today** (01-implementation-notes.md I3): the index lives in memory; a
checkpoint serializes all of it into new extents, then writes the
superblock. The shard pauses for the whole write (12.5 ms p99 on a small
test index; it grows with the index).

**What makes the alternatives possible.** Every logged operation sets
state absolutely (map these blocks here, set this size, put this key,
delete this range). Replaying a sequence of them in order over a state
that already contains some of them ends in the same state as applying
them once. Recovery can therefore start from an index image taken
*while* commits continue, as long as replay starts from a log position
older than anything in the image.

**Option A: fuzzy checkpoint with a sync marker** (ARIES-style; the
log-based approach asked about).

1. Record the current log position `L` as the checkpoint's start (a
   marker in the log, or simply in memory until the end).
2. Serialize the index in slices, a bounded amount per commit round, while
   commits go on. Each object is written as it is when its slice runs.
3. When the last slice is durable, write the superblock: checkpoint
   extents plus *head = L*.
4. Recovery loads the image and replays from `L`; the absolute operations
   converge to the right state.

- Pros: removes the pause (each round pays a slice); small change to the
  current code; reuses the existing log and recovery path.
- Cons: still writes the whole index on every checkpoint, so checkpoint
  bandwidth grows with the index; the log must keep everything from `L`,
  so it needs room for one checkpoint's duration of writes.

**Option B: paged index with dirty-page tracking** (the paged,
incremental approach).

- Split the index into pages, naturally one or more per PG (key ranges of
  the index), each serialized separately. Track which pages changed since
  they were last written.
- A checkpoint writes only dirty pages, to new extents, then a page table
  (itself a small page) that names every page's extents; the superblock
  points at the page table. Combined with A, the dirty pages are written
  in slices too.
- Pros: checkpoint cost follows the rate of change, not the index size; a
  PG's pages can be shipped whole for backfill or moved between shards;
  it is the first half of the copy-on-write B+tree.
- Cons: more format to get right (page table, page versions, freeing
  superseded pages only after the superblock that drops them is durable);
  the whole index still lives in memory.

**Option C: the on-device copy-on-write B+tree** (08-engine.md §7.1, the
original design).

- Pros: the index no longer needs to fit in memory; checkpoints write only
  changed nodes; this is what large shards eventually need.
- Cons: the largest piece of work; read paths gain a cache and page-in
  latency.

**Option D: log-structured merge (sorted delta runs plus compaction).**

- Pros: writes are sequential and proportional to change.
- Cons: compaction brings its own spikes, and reads must merge runs; a
  poor fit for an index that is mostly point lookups by block.

**Recommendation: A now, B next, C when shards outgrow memory.** A removes
the spike with a small change. B makes checkpoint cost follow the rate of
change and gives per-PG pages that backfill and shard rebalancing can
use. C stays the long-term format. D isn't worth its compaction cost here.

#### Expected data size for each approach

Measured with `hyperstor-bench index` on dev-1 (02-benchmarks.md): 1 GiB of
4 MiB objects written sequentially, then every block overwritten in
random order with redirect-on-write (D38), the worst case for the index.

| State | Extent runs | Index RAM per TiB of data | Checkpoint per TiB | Checkpoint time per TiB |
|---|---|---|---|---|
| Written sequentially (one run per chunk) | 256 per GiB | about 1–2 GiB (the checksums, 4 bytes per 4 KiB block) | 1 GiB | about 2 s |
| Fully fragmented (one run per block) | 262,144 per GiB | about 36 GiB | 6 GiB | about 25 s |

The index RAM for the sequential case is the checkpoint size plus
structure: the measured 8 MiB per GiB is mostly write buffers the
allocator kept, not index. Fragmented chunks cost about 140 bytes of RAM
per block, in per-run map entries and their checksum vectors.

What each approach can then hold, per shard (one shard per reactor):

| Approach | What limits it | Data per shard |
|---|---|---|
| Today: whole-index checkpoint, shard paused | The pause: 2–25 ms per GiB, so a 10 ms p99 budget allows about 5 GiB (sequential) or 0.4 GiB (fragmented) | Tens of GiB at most; a test-only format |
| A: fuzzy checkpoint with a sync marker | RAM for the index; background bandwidth to rewrite the whole index each checkpoint; log room for one checkpoint's duration | With 64 GiB of RAM for the index: about 40 TiB sequential, about 2 TiB fragmented. Rewriting 6 GiB per TiB every checkpoint needs long checkpoint intervals (a log ring of several GiB) to keep background writes near 1–5% of the device |
| B: paged index, dirty pages only | RAM, as A; bandwidth now follows the pages changed, not the index size | As A for capacity; checkpoint writes drop with locality (a workload touching 10% of the index writes 10% as much). Uniform random writes over a whole shard dirty every page, the same as A |
| C: on-device copy-on-write B+tree | Device space (0.1–0.6% of data) and the cache for hot index pages | Not bounded by RAM: 100+ TiB per shard. Flush cost follows dirty leaves |
| D: log-structured merge | Device space, compaction | Like C, with compaction's write amplification |

#### A finding behind the numbers: fragmentation, not the approach

With D38, random-write VM disks end up fragmented, and a fragmented chunk
costs about 18 times the RAM of a sequential one (36 against about 2 GiB
per TiB). That decides how much data a node can hold far more than the
checkpoint approach does. Two ways to bound it, for the next engine
change:

- **A dense block map for fragmented chunks.** Once a chunk has more than
  a few dozen runs, store it as an array of (physical block, checksum),
  4 + 4 bytes per block: 8 KiB per 4 MiB chunk, **2 GiB per TiB however
  fragmented**. Physical block numbers fit in 32 bits for shards up to
  16 TiB. Recommended: no extra writes, and the index stays near the
  sequential case.
- **Defragmenting chunks in the background**, rewriting a chunk
  contiguously once its runs pass a threshold. Bounds memory too, but
  costs write amplification (a 4 MiB rewrite for every few hundred
  scattered 4 KiB writes), and helps sequential read performance.

**Recommendation, combined:** the dense block map now, together with the
fuzzy checkpoint (A). That gives about 2 GiB of RAM per TiB in every case,
so a node with 64 GiB for its index holds about 30 TiB, with no checkpoint
pause. Then paged checkpoints (B) to cut checkpoint bandwidth, and the
B+tree (C) when nodes need more data than their RAM allows.

#### Decision and prior art (2026-10-20)

**Answer:** switch fragmented chunks to a dense per-block map, and
eventually move the index to a cache-aware B+tree on the device (D43).
Proven approaches for each part:

**Switching representation by density** (the dense map):
- **XFS inode data forks** hold a file's mapping in one of three formats:
  inline in the inode, a list of extents while it fits, a B+tree of
  extents (the BMBT) when it doesn't. XFS converts between them as files
  grow and shrink. This is the closest match: hyperstor's runs and dense
  array are the same idea at chunk scale.
- **Roaring bitmaps** keep each 64 Ki range as a sorted array, a bitmap or
  runs, whichever is smallest, and convert as the range fills; widely
  used (Lucene, Spark, Druid). The conversion thresholds are the model for
  ours (convert at the size where the two encodings cost the same).
- **The adaptive radix tree** (Leis et al., ICDE 2013) picks node types by
  how many children are present. The same principle at node level.

**Dense maps with checksums** (what a dense chunk holds):
- **ZFS** indirect blocks are dense arrays of block pointers, each carrying
  the checksum of the block it points to (a Merkle tree), so integrity
  checking needs no separate checksum store.
- **SPDK's blobstore** gives each blob a dense array of cluster addresses.
- **Linux dm-thin** maps each virtual block of a thin volume to a data
  block, with a snapshot time, in the leaves of a B-tree.

**Copy-on-write B+trees on the device** (the long-term index):
- **Shadowing** (Rodeh, "B-trees, Shadowing, and Clones", ACM TOS 2008):
  a change copies the path from leaf to root and the new root commits it,
  so the tree on disk is always consistent; reference counts on nodes give
  cheap clones. The basis of Btrfs, and of what D10's snapshots and clones
  need.
- **dm-thin** (Linux device-mapper, `persistent-data`): copy-on-write
  B-trees for thin volumes and snapshots, a space map for block reference
  counts, and a transaction committed by writing the superblock. The
  nearest system to hyperstor's use: block volumes, thin provisioning,
  snapshots.
- **LMDB**: a copy-on-write B+tree with two meta pages and no write-ahead
  log. **redb**, the Rust B-tree database glidex already uses (via
  `glidex-raftstore`), follows the same design and exposes a
  `StorageBackend` trait (length, read, write, set length, sync), so it
  might run on a region of a hyperstor shard. Worth a prototype before
  writing our own (new host check: a commit rate and write amplification
  against our log-plus-checkpoint design).
- **WAFL** (NetApp) and **ZFS**: copy-on-write trees with periodic
  consistency points and an intent log between them, which is our log
  plus checkpoint, and the model for how often to commit the tree.

**Making it cache-aware:**
- **Node size = device page** (4 KiB, or 16–64 KiB for leaves holding
  dense maps: a 4 MiB chunk's dense map is 8 KiB) for the device side, and
  a node cache with a scan-resistant policy (CLOCK-Pro, 2Q or ARC) sized
  to the memory budget, so hot volumes stay in memory and cold ones cost a
  read.
- **Cache-conscious in-memory node layout** (CSB+-trees, Rao and Ross,
  SIGMOD 2000; FAST, Kim et al., SIGMOD 2010): keys packed into cache lines
  and searched without pointer chasing, for lookups on the I/O path.
- **Write-optimized variants**, if random metadata updates outrun the
  tree: **Bε-trees** (Brodal and Fagerberg 2003; TokuDB's fractal tree,
  BetrFS) buffer updates in internal nodes and are provably near-optimal
  for writes; the **Bw-tree** and **LLAMA** (Microsoft, ICDE 2013, VLDB
  2013) use delta records and a mapping table that also makes page
  checkpoints incremental. Neither needs latch-freedom here (one reactor
  owns a shard), but both reduce write amplification.

**Checkpointing until the tree exists:** ARIES-style fuzzy checkpoints
(Mohan et al., ACM TODS 1992): a begin marker, pages written while work
continues, and redo from the marker; it relies on log records that set
state absolutely, which hyperstor's do (option A above).

**Suggested order:**
1. The dense map, with the fuzzy checkpoint.
2. A redb prototype on a shard region, measured against (1).
3. Our own copy-on-write B+tree (or redb, if it measures well) with node
   reference counts for snapshots, a node cache, and per-PG subtrees so a
   PG can move between shards whole.

## Workload review (2026-10-20)

The target workloads and their implications are in
[18-workloads.md](18-workloads.md). Points raised, and the answers:

| # | Point | Answer | Result |
|---|---|---|---|
| 8 | One index for everything, or two flavours (copy-on-write B+tree for block maps, Bε-tree for metadata) | Two flavours, as recommended | D44 |
| 9 | A pool semantic for recomputable data (KV-cache offload) | Make cache pools first-class | D45 |
| 10 | A priority class above fair share for latency-critical I/O | Agreed | D46 |

## Data for pending decisions (2026-10-21)

Collected with the `collect-decision-data` skill; full tables and method
in [02-benchmarks.md](02-benchmarks.md) (2026-10-21).

| # | Open question | Data | What it implies | Recommendation |
|---|---|---|---|---|
| 11 | **When to build paged checkpoints** (point 5, option B) | Dense maps: 2 GiB of checkpoint per TiB. A 4 KiB write costs about 128 bytes of log; fuzzy checkpoints start every quarter of the log. Measured (02-benchmarks.md, log size): per TiB of shard at 50k writes/s, 826 MB/s of checkpoint writes with a 64 MiB log, 207 with 256 MiB, 52 with 1 GiB; reopening after a crash took 0.38, 0.61 and 1.5 s | A 4 TiB shard at 50k writes/s rewrites its 8 GiB index every ~10 s (about 800 MB/s) with today's 256 MiB maximum log. A 4 GiB log brings that to ~50 MB/s; paged checkpoints to whatever changed | Before shards pass about 1 TiB. A larger log trades checkpoint bandwidth for recovery time: replay runs at about 270 MiB of log per second, so a 1 GiB log means up to ~3 s of recovery per shard and a 4 GiB log ~11 s (02-benchmarks.md, recovery time). I'd raise the ceiling to 1 GiB now (bandwidth ÷4 at ≤3 s recovery) and build paged checkpoints (B) next, which fix bandwidth without lengthening recovery. |
| 12 | **The B+tree's commit model** (D43, G13) | redb, committing per transaction, writes 7.8–25 KB of metadata per update; log plus checkpoint writes 128–258 bytes | Committing the tree per transaction would multiply metadata writes by 30–100 | The tree keeps the log as its write-ahead log and commits only at checkpoints (as D43 intends); redb as-is is not a candidate for the index, but stays a reference implementation |
| 13 | **Map lease and liveness defaults** (D40, D42) | Failover stall ≈ liveness timeout + 0.1 s: 3.1 s at 2 s / 3 s, 0.78 s at 0.5 s / 0.7 s. With peer reports (now built): 2.46 s at the defaults, 0.55 s with a 0.5 s lease, as predicted (about 1.25 leases). The recommended setting, lease 0.6 s and timeout 0.8 s, measured 0.91 s without peer reports and 0.66 s with them (two runs, within 2 ms of each other) | Peer reports alone save at most the gap between the timeout and 1.25 leases (0.6 s at defaults); the lease length decides failover time | Pick a target failover time first. For agent VMs and inference, 1 s seems reasonable: lease 0.6 s, timeout 0.8 s, heartbeats every 100 ms, then peer reports. Shorter leases mean more heartbeat traffic and more risk of false detection under load; measure on real networks (G-check) |
| 14 | **Dense threshold** (I34) | Every chunk was dense after 5% random overwrites | The threshold barely matters for VM disks; sequential and immutable data (images, weights) stays in runs and costs half as much | Keep the adaptive switch; no further tuning needed |
| 15 | **Deferral on flash** (D38, settled) | End to end, redirect-on-write: 37.8k writes/s and 2.1 ms p99 at QD32, against 31.7k and 5.4 ms with deferral | Confirms D38 | — |
| 16 | **Polling window for storage reactors** (D41) | 200 µs: QD1 writes 109 → 84 µs, reads 43 → 36 µs; 50 µs gets about half of that; throughput unchanged | Each reactor burns its core for up to the window after every request, so at low load a node's reactors stay busy | Default 200 µs for `hyperstord` (its reactors own their cores, D7); 0 in the client library, which runs inside applications. Revisit with power or shared-core deployments |

Settled with the owner on 2026-10-21: point 11 as recommended (D47),
point 13 with a target of about 1 s (D48), and point 16 as recommended
(D49).

Not measurable yet: G14 (sandbox clones and rollback need snapshots and
clones, 10-volumes.md §10.3) and G15 (needs the Bε-tree).


## Data for pending decisions: administration (2026-10-21)

Collected with the `collect-decision-data` skill; tables and method in
[02-benchmarks.md](02-benchmarks.md) (2026-10-21, cluster-API load).

| # | Open question | Data | What it implies | Recommendation |
|---|---|---|---|---|
| 30 | **Do control-path threads need cores of their own?** (D58, G17) | 1000 `GetStatus` calls/s against a map service with three storage nodes in one process: with 256 CPUs, every p99 change is inside run-to-run noise (~10%); confined to 4 CPUs, QD1 read p99 goes 54–56 → 104–111 µs in both rounds, p50 unchanged, writes unaffected | Sharing cores, admin load reaches the read tail; with spare cores it doesn't show | Keep D58's rule (pin control threads off reactor cores; the installer should set `CPUAffinity=` once reactors are pinned). Repeat G17 on a small host with real devices before settling it; confidence medium. |

## Points from building administration (2026-10-21)

Raised while building AD0 and AD1 (01-implementation-notes.md I41–I52); not
settled.

| # | Point | Options | Recommendation |
|---|---|---|---|
| 31 | **One port or two for the map service** (I41) — *settled 2026-10-21: two ports are fine (D69)*: the map protocol (D5) on 8849 and the cluster API on 8850, against 03-overview.md §3's single 8850 | (a) keep two; (b) detect the protocol on a connection's first bytes; (c) move the map watch into `hyperstor.v1` as a server stream | (a) until Raft (I18) reworks the map service, then (c): one protocol family, and the API is already HTTP/2 |
| 32 | **The root CA's key on a single host** (I48) — *settled 2026-10-21: keep it on a trusted host for now; offline is allowed, not required (D70)*: the all-in-one keeps it in `/etc/hyperstor/pki`, readable by root only; D53 wants it offline | (a) keep it for trial clusters; (b) export it encrypted at install and delete it | (a) for `--all-in-one`, (b) for multi-host installs, once `cluster init` exists |
| 33 | **What `out` means** (I45): today it marks a device out and returns; with as many failure domains as copies, its PGs run degraded | (a) keep it as "service"; (b) add a "drain" that refuses when PGs would end up degraded and runs as an `Operation` until backfill ends | (b), as §23.3 asks; keep (a) as the service mode |
| 34 | **Join tokens need a map that changes at run time** (I50): D57 needs the map service to add nodes and issue certificates, and the map isn't persisted (I18) | (a) persist the working map in a file first; (b) wait for Raft | (b) if Raft is next anyway; the shared-cluster-file install covers multi-host testing until then |
| 35 | **Control threads off reactor cores** (D58, point 30): on 4 shared CPUs, API load doubles the QD1 read tail | (a) pin control threads to non-reactor cores now (`CPUAffinity=` in the units, once `--cpus` is set); (b) wait for G17 on a small host | (a) for the installer's units when it pins reactors; confirm with G17 |

## Administration (2026-10-21)

The installer, API, CLI, UI and security of the control plane
(19-administration.md §23). Proposed in conversation with options and a
recommendation each; the owner's answers:

| # | Point | Answer | Result |
|---|---|---|---|
| 17 | Where people are authenticated: a separate stateless `hyperstor-mgmt`, or inside the meta members | Separate `hyperstor-mgmt`; meta stays machine-only | D50 |
| 18 | API stack: `connectrpc` (glidex's) or `tonic` | `connectrpc` | D51 |
| 19 | glidex's session, token and Cedar code: extract a shared crate or copy it | Extract (`glidex-auth`) | D52 |
| 20 | CA: an offline root with an online intermediate, or one online CA | Root and intermediate, to simplify rotation and administration | D53 |
| 21 | Deletes: a trash with a grace period, or immediate | Trash | D54 |
| 22 | Offer a CLI as well as the UI | Yes, generated from the protos, with break-glass on a local socket | D55 |

Made while drafting and open for review: D56–D60 (certificate lifetimes
and revocation, join tokens, keeping the control path off reactors,
ports, a separate root under glidex). Host checks G17 and G18.


## Tenancy and multi-site (2026-10-21)

Asked by the owner: what makes hyperstor competitive against existing
high-performance enterprise storage, and where multi-tenancy and a
global namespace should be implemented. The answer, with options and a
recommendation each, and the owner's choices:

| # | Point | Answer | Result |
|---|---|---|---|
| 23 | Tenant as a pool per tenant, a field of every object key, or a property of the containers (lineages, buckets, filesystems, ranges) | Containers; pools shared; accounting per `(pool, tenant)` | D61 |
| 24 | Fair share per volume only, or tenants then volumes | Two levels | D62 |
| 25 | Stretch PGs across sites, or keep sites within campus latency and build multi-site above the PG layer | Above the PG layer; `site` as a failure domain within 1 ms | D63 |
| 26 | Cross-cluster replication: synchronous, or asynchronous and immutable-only | Asynchronous, immutable versions, pulled | D64 |
| 27 | Home of a KV range: creator's site and movable, per filesystem or bucket, or Raft across sites | Creator's site, movable | D65 |
| 28 | Writes while the home is unreachable: fail, or queue locally and replay | Queue locally; last-writer-wins with a per-front-end allowlist (the merge rule is open to review) | D66 |
| 29 | Where federation lives: `hyperstor-mgmt` and glidex, `hyperstor-meta`, or glidex only | `hyperstor-mgmt` standalone and glidex under glidex | D68 |

Cache-pool sources (D67) followed from 26 and D45. Host checks G19
(site round-trip limit) and G20 (the namespace across a WAN).

## Replicated map service (2026-10-21)

The next step after HS2's digests (CLAUDE.md, "Next"): Raft for the map
service, through `glidex-raftstore` (D20). Asked with options and a
recommendation each; the owner's answers:

| # | Point | Answer | Result |
|---|---|---|---|
| 30 | What `glidex-raftstore` holds: the Raft layer only (each project implements its state machine), or the Raft layer with glidex's ReDB write-set store (`Db`, `WriteSet`, tables defined per project) | Raft layer and the write-set store, as D20 says | D20 |
| 31 | Where Raft runs in `hyperstor-meta`: on the control thread's tokio runtime, or on hyperstor's reactor through an openraft runtime adapter | The tokio control thread | D71 |
| 32 | Order of work: a glidex pull request first (extract the crate, glidex uses it, behaviour unchanged), then hyperstor pins it; or a prototype in hyperstor first | glidex first, then hyperstor | — |
| 33 | How nodes and clients reach the leader; how a new leader treats leases it didn't grant; how many members | Redirect to the leader; wait 1.25 leases before changing roles, leases off the log; 3 members by default, 5 for large clusters | D72, D73, D74 |

The extraction merged in glidex as PR #141 (`fb59b71`). What it moved (glidex `crates/glidex-control-plane/src/cluster/raft/`, about 1,350 lines with tests): the log store and type config (no glidex dependency), the network (already behind a `Transport` trait), the state machine (depends on glidex's `store` module, which moves with it), and the node (depends on glidex's `NodeStore` and runtime helper, which stay in glidex behind a small interface).

## Volume headers, leases and the blocklist (2026-10-21)

The rest of HS1 (`.meta` header objects and volume leases, 06-metadata.md
§4.2, §9) and HS2's blocklist and forced takeovers. Asked with options
and a recommendation each; the owner's answers:

| # | Point | Answer | Result |
|---|---|---|---|
| 34 | Scope: headers, leases and the blocklist as one milestone, or the blocklist first | One milestone, in steps: object operations on the wire, headers, leases, the blocklist | — |
| 35 | Volume lease timing: 2 s / 10 s as specified, 1 s / 4 s, or 0.5 s / 2 s | Renew every second, expire after 4 s, batched per holder and primary | D75 |
| 36 | Enforcing a volume's single writer: blocklist only, or a lease token on every write | Blocklist only | D76 |
| 37 | Who creates and attaches volumes: the client library and the cluster API, or the library only | The library, and the cluster API and `hyperstorctl` for operators | D77 |
| 38 | I76: anyone who can reach the map service can blocklist an instance | Wait for the security work of AD | D78 |
| 39 | I77: should glidex `preempt` when it re-places a VM from a fenced node | Yes | D79 |
| 40 | I78: a forced delete of an attached volume in the API and `hyperstorctl` | Yes | D80 |

## Join tokens, the root key and recovering the map service (2026-10-22)

After AD1's joins (01-implementation-notes.md I80–I93), the owner settled
the open notes and asked how the cluster recovers when the first map
service member, or every member, is lost.

| # | Point | Answer | Result |
|---|---|---|---|
| 41 | Join tokens: in the issuing member's memory (I80), or replicated through Raft | Replicated; `meta` tokens checked by the first member until the group has a majority | D81 |
| 42 | Tokens only on the leader (I81), and TLS without a client certificate for the join (I82) | Both kept | D82, D83 |

### What each loss means today

- **The first member, after the group formed:** the others keep the map,
  the tokens (D81) and the intermediate CA, elect a leader and go on
  (tested: `join::map_service_members_join_the_first`). The root's key
  goes with the host unless it was backed up (D84); without it the
  intermediate can't be renewed, and the cluster needs a new root within
  a year. A replacement on the same addresses joins with a `meta` token
  from a surviving member and catches up from the leader; on other
  addresses it needs member changes (point 45).
- **Every member down, disks intact:** a majority restarting finds the
  map, the Raft log and the tokens on disk, elects a leader and goes on.
- **A majority lost for good:** the survivors can serve the map but not
  change it: nothing is marked down, no host joins. No way out yet
  (point 43).
- **Every member's disk lost:** the map is gone, though the data is on
  the storage nodes. No way back yet (point 44).

### Points settled the same day

The owner took each recommendation (D85–D88).

| # | Point | Options | Recommendation (taken) |
|---|---|---|---|
| 43 | **Recovering a group that lost its majority for good** | (a) `hyperstor-meta --force-alone` on a survivor, offline: it rewrites the membership to itself, starts as a group of one, and the others join again as new members; (b) only restore from a backup (point 44) into new members; (c) none: wait for the disks | (a), as etcd's `--force-new-cluster`, with rules: it refuses while another member answers; it needs an explicit flag naming the cluster id; the group gets a new generation number so the old members can't rejoin with their logs; it is audited. The survivor may lack the last commits before the loss: those map changes are lost, which leases make safe (a new leader waits them out, D73) but a host joined in that window must join again. |
| 44 | **Map backups** | (a) the leader writes a snapshot to a directory every few minutes and on every change, kept N deep; (b) `hyperstorctl map backup` fetches one through the API; (c) rebuild the map from what the storage nodes report | (a) and (b) now: the map has no secret (tokens are hashes), and a restore is `hyperstor-meta --restore FILE` into a group of one, then members join. (c) later, as a check of a restored map against the nodes. |
| 45 | **Changing the map service's members at run time** (I65) | (a) `hyperstorctl member add/remove` through openraft's joint consensus, a new member a learner until it has caught up; (b) keep members fixed at install | (a), one change at a time, refusing to go below three members unless forced; a replacement on new addresses becomes add, wait, remove. |
| 46 | **The root key's backup (D84)** | (a) warn on every install and `check` until backed up (built); (b) refuse to finish the first install without one; (c) split the root among several administrators (Shamir, k of n) | (a) now: an install that refuses breaks unattended and trial installs, and the warning repeats daily from the timer. (c) for sites with several administrators, later. |

| # | Point | Answer | Result |
|---|---|---|---|
| 43 | Recovering a group that lost its majority | A forced, offline start of one survivor, with the rules above | D85 |
| 44 | Map backups | Snapshots by the leader, and `hyperstorctl map backup`; rebuilding from the nodes later | D86 |
| 45 | Member changes at run time | Add and remove one at a time, a learner first, not below three unless forced | D87 |
| 46 | The root key's backup | Warn until backed up; Shamir splitting later | D88 |

## The rest of HS2 (2026-10-22)

Read leases, the PG log, slow replicas and the fault-injection rig.
Asked with options and a recommendation each; the owner took each
recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 47 | Read leases: build replica-granted read leases, or let map leases cover | Map leases cover; close the item | D89 |
| 48 | PG log: build it, or keep digests (I57, I61) | Keep digests; the log only if scans prove slow | D90, G21 |
| 49 | A slow or hung replica: the primary drops it per PG after an operation timeout, or leave it to liveness | The primary drops it per PG | D91 |
| 50 | Fault-injection rig: in process, or also multi-host with `tc`/netns | In process | D92 |

## People's logins and the web UI (AD2, 2026-10-22)

`glidex-auth` (D52) was to be extracted before AD2. glidex's login,
token, OIDC and Cedar code is about 8,000 lines tied to its directories,
invitations, tenancy and schema; the owner chose how much moves, and
whether the UI comes with it.

| # | Point | Answer | Result |
|---|---|---|---|
| 51 | What `glidex-auth` moves: the core only (sessions, tokens, Cedar), the core plus OIDC, everything behind a store trait, or nothing yet | The core plus OIDC | D96 |
| 52 | The React UI with the logins, or the logins behind the interim dashboard first | Both together | D97 |

Drafted for review with the plan: D98 (mgmt's state in the map
service's store, through the API, cached 10 s), D99 (`hyperstor-authd`
embeds `glidex-authd`), D100 (the UI embedded in the binary; settled
instead as files the installer copies, point 57).

### Order of work

1. **glidex PR**: `crates/glidex-auth` (sessions, tokens, step-up age,
   the generic Cedar engine, OIDC without directories), glidex using it
   with its behaviour unchanged and its tests passing; and the shared UI
   components as a package beside `glidex-ui`, glidex's UI using it.
   Merged by the owner.
2. **hyperstor PR**: the pinned `glidex-auth`; `hyperstor-authd` (D99);
   the `MgmtStore` service and table (D98); login with PAM and OIDC,
   sessions, `hst_` tokens, `hyperstorctl auth login`, the first-boot
   claim URL, Cedar roles (`viewer`, `operator`, `storage-admin`,
   `security-admin`) with step-up; HTTPS beyond loopback.
3. **hyperstor PR**: the React UI (§23.7's pages for what exists:
   overview, nodes and devices, pools, PGs, volumes, operations,
   security, audit), installed as files mgmt serves (D100), with end-to-end tests in the
   browser as glidex's.

## Snapshots and the engine (2026-10-22)

HS4 (snapshots, clones and the host read cache) came before HS3 at the
owner's request. Asked with options and a recommendation each; the owner
also proposed an engine design (a zoned log for data with large
immutable extents, an in-memory radix tree or hash for the mapping,
snapshots cut by sealing the index root and flushing its delta in the
background), discussed before deciding:

| # | Point | Answer | Result |
|---|---|---|---|
| 53 | Snapshots on today's index, or the copy-on-write B+tree first; the owner's zoned-log proposal | Split: snapshots now with the epoch in the key; seal-and-flush for checkpoints next; zones for write-once pools later | D101, D105, G23 |
| 54 | The host read cache: in the client library, or wait for `hyperstor-vhost` | The client library | D102 |
| 55 | Quiescing an attached volume: a long-poll watch, or news on renewals | Long-poll watch | D103 |
| 56 | Snapshot deletion: the engine moves blocks, or copy the data | The engine moves them | D104 |
| 58 | I123: a rollback is a new branch, so resetting an agent VM after every task reaches the chain limit in 16 resets | Reset to the newest snapshot in place | D106 |

## The open AD decisions (2026-10-22)

The owner reviewed the decisions drafted with revocation and the AD2
plan, with the options for each.

| # | Point | Answer | Result |
|---|---|---|---|
| 57 | Where the revocation list lives (store now, map push later; the map push now; short-lived certificates instead), and whether an unreadable list fails open or closed; where mgmt keeps people's state (the map service's store through the API, cached 10 s; a local database; wait for `hyperstor-kv`); how password logins reach PAM (`hyperstor-authd`, glidex's helper with hyperstor's defaults; glidex's own binary; single sign-on only); how the UI ships (in the binary; files the installer copies; a separate UI service) | The store now, pushed with the map once storage nodes use TLS, and fail open; the map service's store through the API; `hyperstor-authd`; files the installer copies, with a separate UI service later | D93, D98, D99, D100 |


## Web certificates (D107, 2026-10-22)

D107 left three ways for people to reach `hyperstor-mgmt` beyond
loopback. Asked what each means and which default to serve, with
options and a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 59 | "The cluster's CA from glidex": hyperstor's own CA, glidex's CA signing mgmt's certificate through its API, or serving glidex-ui's certificate | hyperstor's own CA | D108 |
| 60 | ACME: reload certificate files plus the node's Tailscale certificate, a built-in client with HTTP-01 (port 80) or DNS-01 (RFC 2136), or reload only | Reload and Tailscale | D109 |
| 61 | What mgmt serves beyond loopback before an operator configures a certificate: a cluster-CA certificate on every address, loopback only, or self-signed as glidex | A cluster-CA certificate on every address | D110 |

## Self-signed certificates and AD3 (2026-10-23)

The owner asked whether mgmt should serve self-signed certificates, for
when no other way is available or in an air-gapped site, and then for
AD3. Asked with options and a recommendation each:

| # | Point | Answer | Result |
|---|---|---|---|
| 62 | Self-signed certificates for mgmt: a fallback for a hand-started mgmt, an installer option with first-use pinning, or none (the cluster CA's certificate needs nothing from outside) | None; the cluster CA's, and a note for air-gapped sites | D111 |
| 63 | Where the audit log lives: append-only objects in `.meta`, rows in the map service's store, or journald only | Objects in `.meta` | D112 |
| 64 | How long audit records are kept | A year by default, configurable | D113 |
| 65 | What operators may do: as §23.9.2's table (snapshots, restore from trash), as built (no snapshots), or restore only | As the table | D114 |
| 66 | The full-pool trash rule before D27's thresholds exist: the setting now, acting with D27; all with D27; or a simple rule now | The setting now, acting with D27 | D115 |

## HS3: the vhost-user service (2026-10-23)

Before building `hyperstor-vhost`, three choices, each with options and a
recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 67 | How to implement vhost-user-blk (rust-vmm's crates as libraries on our reactor; rust-vmm's `vhost-user-backend` daemon with its own threads; all our own); how glidex stages a disk (a local control socket; through the cluster API); what to build first (the service, then glidex's driver; the service only; both at once) | rust-vmm's crates on our reactor; a local control socket; the service, then the driver | D116, D117, D118 |

## Preempting one disk (2026-10-23)

I160: preempting a volume blocklists the whole `hyperstor-vhost` it was
attached through. Asked for alternatives, with a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 68 | Preempt only from a fenced node, planned moves detaching first (glidex's driver, no hyperstor change); a client instance per volume (connections multiply with VMs); per-volume identities on shared connections (a protocol change, with D33's delegates); fencing tokens per volume (unsafe without fencing every chunk) | Preempt only from a fenced node, for now | D119 |

## Long operations (AD3, 2026-10-23)

To finish AD3's operations (§23.5–§23.7), asked which calls become
long operations and where operations live, with a recommendation each:

| # | Point | Answer | Result |
|---|---|---|---|
| 69 | Which calls become long operations: flatten, purge and snapshot delete; flatten only; or none new | Flatten, purge and snapshot delete | D120 |
| 70 | Where operations live: rows of the replicated store (seen by every member, resumed by a new leader); memory on the leader; or objects in `.meta` | The replicated store | D121 |

## glidex's hyperstor driver (2026-10-23)

Before writing glidex's `hyperstor` datastore driver, three choices the
spec's table (16-glidex.md §17) didn't settle, each with options and a
recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 71 | How glidex gets and keeps its certificate (a mgmt token it enrolls with, sealed in its store and renewed; files placed on every node; each node enrolling); how a worker writes disk bytes without root (a range read/write tool, editing partitions as files; ublk devices through stord; leaving extend-root to the guest); how `stage` knows to preempt (a mark the scheduler sets at re-placement; asking hyperstor who holds the disk) | A mgmt token, sealed and renewed; the range tool; the scheduler's mark | D122, D123, D124 |

## Fencing a former leader (2026-10-23)

Asked whether a former leader still running a flatten or purge could
harm consistency or durability, with fixes and a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 72 | Whether a former leader's run of a flatten or purge can harm data: copies stay consistent and flatten only fills blocks the clone lacks, but a late volume purge could delete a new volume created with the same id. Fixes: blocklist the last leader's client before resuming (recommended); refuse reusing the id; stop on losing the lead | Fence by blocklisting | D125 |

## Block size for VMs (HS3, 2026-10-23)

glidex's driver booted a cloud image from a volume: the firmware found
no bootable device, because `hyperstor-vhost` offered 4 KiB logical
blocks and the image's GPT is in 512-byte sectors (glidex storage.md
H9). Asked how disks should look to VMs, with a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 73 | 512e in `hyperstor-vhost` (read-modify-write for partial blocks, the engine unchanged); a block size per volume; glidex converting images to 4Kn | 512e in `hyperstor-vhost` | D126 |

## NFS and pNFS for unmodified clients (2026-10-24)

An investigation of how unmodified NFS clients could use the filesystem
in parallel (22-nfs.md §26), with options and a recommendation for each
choice. The owner took three; the rest of D128–D137 stay open.

A review of the proposals against RFC 8881, RFC 8434, RFC 8435 and the
Linux client (`fs/nfs`) found three that were wrong as written (D132,
D133, D134) and reopened the layout choice (D128); it also raised seven
points the spec had left implicit. Asked with options and a
recommendation each; the owner took every recommendation.

| # | Point | Answer | Result |
|---|---|---|---|
| 74 | Placing file data: F4's hash per chunk (one layout segment per 4 MiB), or stripe groups (each 64 GiB segment round-robin over `W` consecutive PGs from a random base, for every client path); the NFS server: hyperstor's own in Rust, NFS-Ganesha with a FSAL, or the kernel's `nfsd` over FUSE; where data servers run: inside `hyperstord` per reactor, a process per storage host, or gateways | Stripe groups; our own Rust server; data servers inside `hyperstord` | D127, D129, D130 |
| 75 | The review's fixes: `DATA_SYNC` instead of `FILE_SYNC` (Linux skips `LAYOUTCOMMIT` after `FILE_SYNC`); seal open, lock and delegation stateids, not layout stateids (RFC 8881 section 13.9.1); sessions and `csa_sequence` in kv with `CREATE_SESSION4_FLAG_PERSIST`; a lease check in every transaction; device IDs at the primaries' last change epoch; and both layouts, tightly coupled, over one model, chosen per export (`files` by default, `flexfiles`, or `auto` from the implementation id) | As recommended | D128, D131, D132, D133, D134 revised |
| 76 | Who moves an export's address on MDS failover: `hyperstor-nfs` itself under the kv lease (recommended); keepalived/VRRP; DNS records with client trunking | `hyperstor-nfs` itself | D138 |
| 77 | How a data server validates I/O: the sealed stateid alone, stateless, with bounded staleness after `CLOSE` or `chmod` (recommended); or opens and permission changes propagated to the `W` primaries (RFC-exact) | Stateless | D139 |
| 78 | Where pNFS reads go: the primary only, as native reads (recommended); or any in-sync replica for replicated pools | Primary only | D140 |
| 79 | Where stripe groups and kv state land in the plan: stripe groups with HS14's placement and N0 with state in kv from the start (recommended); or N0–N2 as first proposed | With HS14; kv from the start | D137 revised |
| 80 | Exports: cluster objects in kv through the API and `hyperstorctl` (recommended); or an `exports(5)` file per MDS | Cluster objects | D141 |
| 81 | Binding I/O credentials to the open: set `EXCHGID4_FLAG_BIND_PRINC_STATEID` and check that Linux honours it (recommended); or re-check the user against the file mode at the DS | Set the flag | D133 (G35) |
| 82 | Which pools may back NFS files: any pool, EC unaligned writes measured before the default is final (recommended); or replicated pools only | Any pool | D132 (G36) |
| 83 | A second review, for durability, performance and security (2026-10-29): size changes must recall RW layouts; `LAYOUTCOMMIT` takes the max; READ layouts out of kv; the address dropped before the lease can expire elsewhere; stateids unique per open (a nonce), bound to the authenticated host, with a key id for rotation; special stateids refused; a per-layout stateid for flexible files instead of a principal-bound open stateid; `all_squash` and uid ranges per export; G38 for one instance's metadata rate | All taken | D132, D133, D138 revised |

## hyperstor-vhost as a service (2026-10-24)

To install `hyperstor-vhost` for glidex and keep VMs' disks across its
restarts, asked three questions, each with a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 83 | Which hosts run it (a `--vhost` flag on any role plus `--role vhost`; always with storage; its own role only); what stopping it does to attached volumes (keep them, the restart takes them back; detach them); how glidex gets at its sockets (`--vhost-user NAME`; run it as glidex) | `--vhost` on any role; keep, the restart takes them back; `--vhost-user NAME` | D142, D143, D144 |

## The audit log after a retry storm (2026-10-28)

A client retrying refused calls filled a day's audit object to about
220 MiB (I177; hyperstor #60, #61). Asked two questions, each with a
recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 84 | How to keep refusals from flooding the log: summarise repeats (the first 10 of a principal's operation and error a minute recorded, the rest counted into one record a minute; journald keeps all; recommended); a hard cap a day; sampling. And how to bound a day's object: numbered parts per member, a new one at 16 MiB (recommended); an object an hour; parts shared by members | Summarise repeats; numbered parts per member | D147, D148 |

## A node that flaps (issue #64, 2026-10-28)

A reactor stalled by long synchronous work misses its peers' 300 ms
pings, so the node went down and up again about 150 times a minute.
Asked how to stop it, with a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 85 | Damping in the map service (a down node held down 2 s, doubling per flap within 10 minutes up to 60 s, and a health warning) plus long reactor work yielding (recommended); heartbeats and pings from a thread of their own; higher timeouts | Damping and yielding | D149, I183 |

## Fencing one volume of a shared client (2026-10-29)

Preempting one volume blocklisted the whole `hyperstor-vhost` that held
it, so every VM disk on its node was cut off (I160, D119). Asked how to
fence one volume, each with a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 86 | How a request names its lease: a field in `Target`, protocol version 2 (recommended); a connection per attached volume, with its own instance; a flag bit and 8 bytes after the header | A field in `Target` | D150, I186 |
| 87 | D33's delegates, which nothing uses yet: defer them, leaving room in the header (recommended); build them now | Build them now | D151 |
| 88 | Upgrading a running cluster: storage nodes speak versions 1 and 2, a version 1 client fenced by instance (recommended); version 2 only, every host at once | Version 2 only | D152 |
| 89 | glidex: hyperstor only, with a glidex issue to relax its rule (recommended); a glidex pull request in the same round | The glidex pull request too | D153 |
| 90 | When glidex preempts: a stale hold found when it starts a VM recorded as running nowhere, after asking the last node to detach (recommended); only on an operator's request; no change, the rationale only | A stale hold on start | D153 |
| 91 | Found while building 90: glidex tracks a disk operation only on the node running it, so preempting every hold could cut a tool off mid-operation. Label each lease with its holder and preempt only another node's `hyperstor-vhost`, each node letting go of holds for VMs placed elsewhere (recommended); that cleanup only, no preemption beyond S23; preempt after a hold lasts 2 minutes | Label holds | D153 |

## The PG grid in the web UI (2026-10-30)

The UI showed PG states only as counts, and a pool's PGs as a table that
reloaded every 5 s: about 80 bytes of JSON per PG, 330 KB for 4,096 PGs.
Asked how to show a grid of every PG without that cost, with a
recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 92 | A call that sends a byte per PG and nothing while the map's epoch hasn't moved, polled (recommended); a stream of changed PGs; the existing list, compressed | Polled by epoch | D154, I189 |

## PG usage in the grid (2026-10-30)

Asked to shade each PG's cell by how much it holds, darker for more,
with a recommendation for each of three choices:

| # | Point | Answer | Result |
|---|---|---|---|
| 93 | Where usage comes from: primaries report changes every 30 s to the leader, kept in memory (recommended), or the map service asks every host on demand; what it measures: bytes allocated (recommended) or objects; how it is drawn: the state's colour shaded on a log scale up to the pool's fullest PG (recommended), a toggle between states and a usage heatmap, or an absolute scale | The three recommendations | D155, I191 |
| 94 | Deployed, the log scale showed the pilot's PGs (19 to 82 MiB) at levels 235 to 255, one shade. Replace it with linear from the emptiest to the fullest PG (recommended), linear from zero to the fullest, or log from the emptiest | Linear from zero to the fullest | D155 revised |

## Pool capacity for glidex (2026-10-30)

glidex's datastores page showed 0 B free for hyperstor datastores:
storage devices knew their free space but only exported it to
Prometheus, so the map service and the API had none (16-glidex.md §17).
Asked how to report it, each with a recommendation:

| # | Point | Answer | Result |
|---|---|---|---|
| 95 | How devices send their space: a message of their own every 5 s, which an older map service refuses harmlessly (recommended); fields in `Heartbeat`, protocol version 3. How a pool's usable space is counted: its devices' bytes divided by redundancy (recommended); Ceph-style, limited by the device that fills first; raw bytes only | A message of their own; divided by redundancy | D156, I192 |

## The engine's layout and the AI workloads (2026-10-30)

Asked by the owner: whether the on-device layout (08-engine.md §7) still
fits, given storage research from 2025 and 2026, and whether the design
suits agent VMs, distributed inference, RAG and fine-tuning
(18-workloads.md). The layout's basis holds: devices opened raw, a shard
per reactor, every write atomic through the log, a CRC32C per block. The
points below are where the research or the workloads suggest a change.
None is settled. Three are for host checks (G39–G41, 17-milestones.md §22);
three of the sources are preprints, marked below.

| # | Point | Options | Recommendation |
|---|---|---|---|
| 96 | **Feature flags in the superblocks.** The device superblock carries `FORMAT_VERSION = 1` and refuses any other; the shard superblock has no version. The on-device index (D43) will be the first large format change | (a) compatible, read-only-compatible and incompatible flag words in both superblocks now (format version 2, reading version 1 as no flags), as ext4 and XFS do; (b) add them with D43's change; (c) version numbers only, with a conversion per change | (a). A few bytes, and later changes (the on-device index, stream placement, where checksums live) can then reach a running cluster one device at a time. Protocol version 2 could require every host at once (D152); a device's format can't be converted everywhere at once |
| 97 | **The allocator's write amplification on flash.** Lee, Ziegler and Leis (PVLDB 2026) made a B-tree engine write out of place, as hyperstor's does. On ordinary drives, the drive's own write amplification stayed about 1.94 at high fill, until new pages went into regions the size of the drive's erase unit (4–8 GB on the drives they measured; 32 GB when unknown), grouped by predicted death time, with cleaning on the host: then about 1.0, with 6–10× fewer flash writes and 1.65–2.24× the throughput. Hyperstor's allocator is best fit over freed 4 KiB holes (`alloc.rs`), which the drive sees as random overwrites. D105 kept it for VM pools so as not to copy live blocks on the host | (a) keep the allocator (D105 as settled); (b) regions sized to the erase unit, grouped by death time, cleaned by the host, for every pool on flash; (c) regions for write-once pools only (D105's later step) | Measure first (G39): the drive's physical writes at 50, 80 and 90 % full under a VM-like random overwrite load, today's allocator against regions. If regions save much at the fill levels pools run at, (b) as an allocation policy chosen per pool; otherwise (c) |
| 98 | **Placement handles for streams by lifetime** (NVMe Flexible Data Placement). Lan et al. (arXiv 2026, preprint), on drives with 8 handles: separating streams by lifetime took write amplification from about 3 to about 1, RocksDB's from 1.4–2.1 to 1.0–1.25; all-random streams gained little (3.95 to 3.60), gains faded above about 90 % full, and a handle shared by unlike streams rose from 1 to 1.83. Allison et al. (EuroSys 2025) reached about 1 in CacheLib. Each shard writes three streams with very different lifetimes: the log ring (seconds), checkpoint pages (one checkpoint) and data | (a) a handle per stream now, on the passthrough path, ignored by drives without FDP; (b) only with D105's write-once pools; (c) a handle per region, after point 97 | (a), then (c) if point 97 leads to regions. A hint per write, no format change. Measured with G23 and G39 |
| 99 | **Where per-block checksums live, and range keys, before D43's node format.** A dense map costs 8 bytes per block, half of it the CRC (D43). Zhao et al. (RASK, arXiv 2026, preprint) index a cloud block store by block ranges, with log-structured leaves for overlapping writes, and report up to 98.9 % less index memory than ten other indexes on four production traces. Range keys only pay if the checksums are kept elsewhere | (a) as D43 plans: per-block entries, CRCs in the leaves; (b) range keys, per-block CRCs in pages of their own on the device, read and cached by use (as 08-engine.md §7.2 already has for leaf CRC arrays); (c) range keys, checksums in NVMe protection information (per-block metadata; a 64-bit guard since NVMe 2.0), on drives formatted with metadata and only on the passthrough path | (b) as the design for D43's tree, (c) as an option where drives support it (G40). Decide with D43's node format, and prototype both against G14's sandbox fleet |
| 100 | **Media corruption in the crash test.** LeBlanc et al. (OSDI '25) verify crash consistency under a model of media corruption, not only of crashes. The engine's randomized crash test covers crashes | (a) add bit flips, lost writes and misdirected writes to `FaultDev` in that test, on the log, both superblocks, checkpoint pages and data, and check that each is caught: repaired from a replica or refused, never served; (b) leave corruption to the scrub tests | (a). It tests the claims the layout rests on (salted log blocks, alternating superblocks, per-block CRCs) and needs no new back end |
| 101 | **The order of milestones for AI workloads.** HS0–HS4 cover agent VMs. Inference, fine-tuning and RAG need HS6, HS7, HS8, HS10b, HS11–HS14, none started. The current *Next* list: §11.1's adaptive polling and reactors per NUMA node, zone or FDP placement, the on-device index, cache-pool expiry | (a) as it is; (b) HS5b and HS6 (RDMA) with HS7 (erasure coding), for the bandwidth and capacity of weights and checkpoints; then HS10b (KV-cache offload, inference ahead of checkpoint bursts); HS11, HS12 and HS13; HS8 (GPU-direct, with read-only multi-attach); HS14; (c) S3 first: HS7, HS11, HS12, then RDMA | (b). Weights read by every GPU node and checkpoints written by every node at once need RDMA and full-stripe EC first; three replicas over TCP triple checkpoint traffic. The engine items stay where point 97 and G14 put them |
| 102 | **The interface for KV-cache offload.** Cache pools (D45) give the semantics (one copy, an expiry, lost means recomputed), but no spec says what an inference engine calls. vLLM's KV connector and offload layers such as LMCache, Mooncake and NVIDIA Dynamo's KV block manager take pluggable storage back ends | (a) S3 over RDMA (HS13), for back ends that speak S3; (b) a native interface in the client library: put and get of cache-pool objects keyed by the block's hash, over the RDMA transport, with a connector for one or two engines; (c) files in the filesystem (HS14) | (b), with (a) as the fallback for any S3-capable back end. No gateway hop, and expiry and the single copy are native. G41 checks the connector interfaces before the spec is written |
| 103 | **A shared filesystem path for fine-tuning.** Training frameworks and data loaders mostly read datasets and write checkpoints as files in a shared path. NFS's N0 needs HS11 and the metadata part of HS14 (D137), so it doesn't come before the filesystem's metadata | (a) D137's order; (b) N0 right after HS11 and HS14's metadata, before HS14's own clients; (c) until then, datasets and weights as read-only snapshots attached by many hosts (D18, part of HS8) under a local filesystem, and checkpoints to a volume per node | (b), with (c) as the stopgap, pulling HS8's read-only multi-attach forward |

Sources: Lee, Ziegler and Leis, "How to Write to SSDs", PVLDB 19
(arXiv 2603.09927); Lan et al., "FDP: The Data Placement Promise of Modern
NVMe SSDs", arXiv 2610.02676 (preprint); Allison et al., "Towards Efficient
Flash Caches with Emerging NVMe Flexible Data Placement SSDs", EuroSys 2025
(arXiv 2503.11665); Zhao et al., "'Range as a Key' is the Key! Fast and
Compact Cloud Block Store Index with RASK", arXiv 2601.14129 (preprint);
LeBlanc et al., "Tool-Agnostic Verification of Crash Consistency", OSDI '25.
Also read, with nothing that changes the layout: Alibaba Cloud's "Here,
There and Everywhere" (FAST '26), Tidehunter (arXiv 2602.01873, preprint),
Zhou and Feng on erasure-coded updates (ACM TOS 2026), and Bf-Tree
(PVLDB 17) as a candidate for D44's metadata tree.

Settled on 2026-10-30: point 96 as recommended (D157). Building it found
that `hyperstord` formatted any device the engine reported as not
formatted, which included a newer format version and damaged
superblocks; D157 narrows that to blank devices (I196).

## Device weights and encryption at rest (2026-10-31)

Asked by the owner: the best way to give a host several devices, for
performance, durability and security. Hosts already serve several
devices, one reactor and port each (D39), and placement keeps copies on
different hosts. Two parts need the owner's choices: what a device's
weight is, and encryption at rest (03-overview.md §1 lists it as later;
15-operations.md §19).

**Weights.** Placement scores each device by `-ln(u) / weight`
(07-placement.md §5.2), and D156 counts a pool's space over devices with
a weight. But every device joins with weight 1 (I84), and neither the
cluster file nor the API can change it, so a 4 TB and a 30 TB device get
the same share of PGs and the smaller fills first.

| # | Point | Options | Recommendation |
|---|---|---|---|
| 104 | **What a device's weight is** | (a) its capacity, set by the map service from the device's data region when its first `DeviceSpace` report (D156) reaches the leader, in TiB, with an override the operator sets and the map keeps (`hyperstorctl device weight D W\|auto`); (b) set by the operator only, 1 until then; (c) capacity only, no override | (a). Mixed sizes fill evenly without a step to forget, and the override covers a slower drive or a slow drain. The leader commits a new weight through Raft only when the reported size differs from the weight by more than 1 %, and never over an override, so reports every 5 s don't churn the map |
| 105 | **How a weight change moves data.** Rendezvous hashing moves only the PGs whose best device changes, about the share of weight that changed. A host joining is the largest change; on 2026-10-29 an empty node's refill flapped the pilot until I187 put a budget on backfill per target | (a) at once, under I187's budget and D149's flap damping; (b) stepped: the map service raises a new device's weight in steps as each step's backfill finishes; (c) at once, with a cap on PGs backfilling in the cluster | (a), then (c) if a host joining a loaded cluster slows client I/O more than the operator accepts. Measure with a fourth pilot host before building more |
| 106 | **A pool's usable space with unequal hosts.** D156 divides the devices' sum by the copies. With domain `host` and as many hosts as copies (the pilot: 3 and 3), every host holds a copy of every PG, so the smallest host bounds the pool and the sum overstates it | (a) keep the sum, as `df` does for filesystems on one disk; (b) the most the pool can hold under its domain rule: no domain counts for more than `total / size` of it (water-filling over domains, then the same within each), computed by the leader with D156's reports; (c) Ceph's `MAX AVAIL`, limited by the device that fills first | (b), revising D156. glidex's no-overcommit rule needs a number that isn't too high; (b) is exact for the domain rule and, unlike (c), doesn't jump when one device runs ahead |
| 107 | **Uneven fill within a class.** With about 100 PGs per device (07-placement.md §5.1), each device's PG count varies by about 10 % (one standard deviation), and PGs differ in size (D155's grid shows the pilot's from 19 to 82 MiB) | (a) weights only; watch each device's space (I193) and the PG grid; (b) a balancer that sets a second, hidden weight per device from how full it is (Ceph's `crush-compat` mode), keeping placement a function of the map; (c) explicit PG overrides in the map (Ceph's `upmap`) | (a) until a device reaches D27's `nearfull` while its class has room, then (b). (c) makes the map grow with the overrides and placement no longer a pure function |

**Encryption at rest.** The network is encrypted per D25. 15-operations.md
§19 said the engine format reserves a key-id per lineage; it doesn't (no
such field is written), but D157's incompatible flags now let a format
change reach devices one at a time. Two facts constrain the choice:
peering, delta recovery and scrub compare content digests between copies
(D90), so every copy must hold the same bytes; and the GPU-direct path
(D18) reads blocks into GPU memory over RDMA, with no host step to decrypt
in. Measured on dev-2 (02-benchmarks.md, 2026-10-31), one core encrypts
4 KiB blocks at 5.6 GB/s with AES-256-XTS, 3.5 GB/s with AES-256-GCM and
1.8 GB/s with ChaCha20-Poly1305: a small share of a reactor's time.

| # | Point | Options | Recommendation |
|---|---|---|---|
| 108 | **What it protects against** | (a) drives lost, stolen or retired without wiping; (b) also a storage host's root reading data at rest, which needs storage nodes never to see plaintext; (c) (a), and erasing a volume or a tenant by deleting its key | (a) and (c). (b) means encrypting in the client (point 109's (a)), which costs the GPU-direct path and any later work on the storage side (EC partial updates, compression). Erasure by key also makes trash purges (D115) and tenant removal final without overwriting |
| 109 | **Where data is encrypted** | (a) in the client library, before a write leaves its host; (b) on the PG's primary, before it replicates: every copy holds the same ciphertext, backfill and scrub move it unchanged, and only nodes serving reads hold keys; (c) per device: self-encrypting drives (TCG Opal), dm-crypt under each device, or a key per device in the engine | (b) for volumes' data. (c) with dm-crypt loses the passthrough path (D29), and a key per device in the engine gives copies different bytes, against D90; Opal has no CPU cost but trusts drive firmware and erases only whole drives, so offer it as an install option for disposal where drives support it (G42) |
| 110 | **Cipher, and what is kept per block** | (a) AES-256-XTS, with the tweak from (lineage, branch, chunk, block): the same size, nothing stored per block, integrity left to the CRC; (b) AES-256-GCM, a nonce from the write's revision and a 16-byte tag per 4 KiB block: detects tampering, but adds 16 bytes or more per block beside D43's 8-byte entries; (c) (a) now, (b) as a later per-pool option once point 99 decides where per-block checksums live | (c). Disk encryption's usual choice; it fits point 108's threats. XTS shows when a block is rewritten with the same contents, which those threats don't cover |
| 111 | **Which key a block uses.** A volume and every clone made from its snapshots share a lineage (D21); each is a branch, a clone reads its parent's blocks, and a chunk version is keyed by lineage, chunk, branch and epoch (D4) | (a) a key per lineage, shared by a volume and its clones: erasing one needs all of them gone; (b) a key per branch, a block's key being that of the branch whose version holds it: no extra bytes, and a clone's own writes use its own key; (c) a key per tenant per pool | (b). A branch's key is deleted only when no clone reads through it, the rule its data already follows, so erasure stays per volume |
| 112 | **Where keys are kept.** Each branch's data key is stored wrapped by a key-encryption key (KEK), in the volume's header in `.meta` | (a) a cluster KEK held by the map service members as an encrypted systemd credential, TPM-sealed where the host has one (as the intermediate CA's key, I87 and G18), backed up with the root key (D84), and sent over mTLS to the primaries of a volume's PGs while it is attached; (b) an external KMS (KMIP, or Vault's transit engine) per cluster or tenant; (c) (a) by default, (b) per tenant as an option | (c), building (a) first. A cluster with no KMS still gets encryption, and a tenant whose policy names a KMS can use its own |
| 113 | **What is encrypted, and how it starts** | (a) chosen per pool when it is made: every volume in it; (b) per volume when it is made; (c) the whole cluster | (a), then (b) where a pool allows it. Existing pools stay as they are. `.meta` (headers, audit, people's state) is encrypted with the cluster key once (a) is built. A device that holds encrypted data sets an incompatible flag (D157), so a build without encryption refuses it rather than serving ciphertext as data |

Settled on 2026-10-31, every point as recommended: points 104, 105 and
107 as D158; 106 as D159, revising D156's count; 108, 109 and 113 as
D160; 110 as D161; 111 and 112 as D162.

## The engine's I/O and the reactor (2026-10-31)

Every engine call is synchronous on its reactor's thread: a commit plans
its lists, writes data and the log record, flushes, and returns
(I2), and a read waits for its blocks the same way. `UringDev` submits
to an io_uring of its own and waits on it, though 08-engine.md §6.2 puts
device I/O on the reactor's ring. While the device works, the reactor
answers no pings, accepts nothing and serves nothing. On NVMe with
power-loss protection a flush takes microseconds, so the pilot rarely
notices. Where a device is slower, peers report a healthy node silent:
on CI one commit held a reactor for up to 1.1 s, past the 600 ms peer
timeout, and nodes were marked down (I195, I200), the same pattern as
the pilot's flapping (D149, I187, I188). Hard disks (D29) write their
data extents inside the commit, at milliseconds a seek. A prototype of
point 114's first stage is built (I199), with tests; it is not settled.

| # | Point | Options | Recommendation |
|---|---|---|---|
| 114 | **Taking device waits off the reactor's thread** | (a) a completion-driven engine on the reactor's ring: a commit plans in memory, submits its writes and flush, returns to the reactor, and ends on their completions; reads submit and await the same way; (b) the engine on a thread of its own per device, the reactor handing it work over a channel; (c) pings and heartbeats answered by a thread of their own (point 85 chose against it); (d) leave it, relying on fast NVMe | (a), in stages, each its own pull request: (1) commits, one in flight at a time, with an I/O timeout (built as a prototype, I199); (2) reads, many in flight, with freed extents held until the reads that may use them end; (3) checkpoint slices and deferred in-place writes; (4) optionally, a commit planned while the previous one flushes. It keeps one reactor per core (D3, 08-engine.md §6.1) and is the submission path NVMe passthrough (D29) needs anyway. (b) costs a hop between threads per operation and threads outside the reactors' cores, and reads either wait behind flushes or need locks. (c) would make a stuck reactor look alive. (d) fails on any slower device |
| 115 | **What ends a device's wait.** Today a hung device holds its reactor, so the node stops answering and goes down: crude, but nothing hangs unseen. With (a), the reactor goes on, so something must end the wait | (a) a timeout per commit round, after which the device is failed and its PGs placed elsewhere (the path of a device returning I/O errors); (b) the same, and the node also reports the device's PGs unavailable at once while waiting longer than the peer timeout; (c) no timeout of hyperstor's: rely on the kernel's (NVMe aborts and retries a command after 30 s, then fails it) | (a), 30 s by default, as Linux gives an NVMe command, configurable per node. Requests to the device's PGs wait meanwhile, as they do today, and a primary already drops a replica that doesn't answer within 2 s (D91). (c) can leave a request waiting through several kernel retries |

Settled on 2026-10-31, both as recommended: point 114 as D165, point
115 as D166. Host check G43 (17-milestones.md §22) measures stage 1 on
the pilot.
