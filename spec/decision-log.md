# Decision log

Status: **working record.** Decision points raised during implementation,
the owner's answers, and the points still open with their trade-offs.
Settled points become numbered decisions in [decisions.md](decisions.md);
the measurements behind them are in [benchmarks.md](benchmarks.md).

## Review of 2026-10-20

Raised in [implementation-notes.md](implementation-notes.md) after HS0,
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

From [benchmarks.md](benchmarks.md), dev-1 (one consumer NVMe SSD without
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
replication.md §8.4, metadata.md §4.1).

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

**Today** (implementation-notes.md I3): the index lives in memory; a
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

**Option C: the on-device copy-on-write B+tree** (engine.md §7.1, the
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

Measured with `hyperstor-bench index` on dev-1 (benchmarks.md): 1 GiB of
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
[workloads.md](workloads.md). Points raised, and the answers:

| # | Point | Answer | Result |
|---|---|---|---|
| 8 | One index for everything, or two flavours (copy-on-write B+tree for block maps, Bε-tree for metadata) | Two flavours, as recommended | D44 |
| 9 | A pool semantic for recomputable data (KV-cache offload) | Make cache pools first-class | D45 |
| 10 | A priority class above fair share for latency-critical I/O | Agreed | D46 |

## Data for pending decisions (2026-10-21)

Collected with the `collect-decision-data` skill; full tables and method
in [benchmarks.md](benchmarks.md) (2026-10-21).

| # | Open question | Data | What it implies | Recommendation |
|---|---|---|---|---|
| 11 | **When to build paged checkpoints** (point 5, option B) | Dense maps: 2 GiB of checkpoint per TiB. A 4 KiB write costs about 128 bytes of log; fuzzy checkpoints start every quarter of the log. Measured (benchmarks.md, log size): per TiB of shard at 50k writes/s, 826 MB/s of checkpoint writes with a 64 MiB log, 207 with 256 MiB, 52 with 1 GiB; reopening after a crash took 0.38, 0.61 and 1.5 s | A 4 TiB shard at 50k writes/s rewrites its 8 GiB index every ~10 s (about 800 MB/s) with today's 256 MiB maximum log. A 4 GiB log brings that to ~50 MB/s; paged checkpoints to whatever changed | Before shards pass about 1 TiB. A larger log trades checkpoint bandwidth for recovery time: replay runs at about 270 MiB of log per second, so a 1 GiB log means up to ~3 s of recovery per shard and a 4 GiB log ~11 s (benchmarks.md, recovery time). I'd raise the ceiling to 1 GiB now (bandwidth ÷4 at ≤3 s recovery) and build paged checkpoints (B) next, which fix bandwidth without lengthening recovery. |
| 12 | **The B+tree's commit model** (D43, G13) | redb, committing per transaction, writes 7.8–25 KB of metadata per update; log plus checkpoint writes 128–258 bytes | Committing the tree per transaction would multiply metadata writes by 30–100 | The tree keeps the log as its write-ahead log and commits only at checkpoints (as D43 intends); redb as-is is not a candidate for the index, but stays a reference implementation |
| 13 | **Map lease and liveness defaults** (D40, D42) | Failover stall ≈ liveness timeout + 0.1 s: 3.1 s at 2 s / 3 s, 0.78 s at 0.5 s / 0.7 s. With peer reports (now built): 2.46 s at the defaults, 0.55 s with a 0.5 s lease, as predicted (about 1.25 leases). The recommended setting, lease 0.6 s and timeout 0.8 s, measured 0.91 s without peer reports and 0.66 s with them (two runs, within 2 ms of each other) | Peer reports alone save at most the gap between the timeout and 1.25 leases (0.6 s at defaults); the lease length decides failover time | Pick a target failover time first. For agent VMs and inference, 1 s seems reasonable: lease 0.6 s, timeout 0.8 s, heartbeats every 100 ms, then peer reports. Shorter leases mean more heartbeat traffic and more risk of false detection under load; measure on real networks (G-check) |
| 14 | **Dense threshold** (I34) | Every chunk was dense after 5% random overwrites | The threshold barely matters for VM disks; sequential and immutable data (images, weights) stays in runs and costs half as much | Keep the adaptive switch; no further tuning needed |
| 15 | **Deferral on flash** (D38, settled) | End to end, redirect-on-write: 37.8k writes/s and 2.1 ms p99 at QD32, against 31.7k and 5.4 ms with deferral | Confirms D38 | — |
| 16 | **Polling window for storage reactors** (D41) | 200 µs: QD1 writes 109 → 84 µs, reads 43 → 36 µs; 50 µs gets about half of that; throughput unchanged | Each reactor burns its core for up to the window after every request, so at low load a node's reactors stay busy | Default 200 µs for `hyperstord` (its reactors own their cores, D7); 0 in the client library, which runs inside applications. Revisit with power or shared-core deployments |

Settled with the owner on 2026-10-21: point 11 as recommended (D47),
point 13 with a target of about 1 s (D48), and point 16 as recommended
(D49).

Not measurable yet: G14 (sandbox clones and rollback need snapshots and
clones, volumes.md §10.3) and G15 (needs the Bε-tree).


## Data for pending decisions: administration (2026-10-21)

Collected with the `collect-decision-data` skill; tables and method in
[benchmarks.md](benchmarks.md) (2026-10-21, cluster-API load).

| # | Open question | Data | What it implies | Recommendation |
|---|---|---|---|---|
| 30 | **Do control-path threads need cores of their own?** (D58, G17) | 1000 `GetStatus` calls/s against a map service with three storage nodes in one process: with 256 CPUs, every p99 change is inside run-to-run noise (~10%); confined to 4 CPUs, QD1 read p99 goes 54–56 → 104–111 µs in both rounds, p50 unchanged, writes unaffected | Sharing cores, admin load reaches the read tail; with spare cores it doesn't show | Keep D58's rule (pin control threads off reactor cores; the installer should set `CPUAffinity=` once reactors are pinned). Repeat G17 on a small host with real devices before settling it; confidence medium. |

## Points from building administration (2026-10-21)

Raised while building AD0 and AD1 (implementation-notes.md I41–I52); not
settled.

| # | Point | Options | Recommendation |
|---|---|---|---|
| 31 | **One port or two for the map service** (I41) — *settled 2026-10-21: two ports are fine (D69)*: the map protocol (D5) on 8849 and the cluster API on 8850, against overview.md §3's single 8850 | (a) keep two; (b) detect the protocol on a connection's first bytes; (c) move the map watch into `hyperstor.v1` as a server stream | (a) until Raft (I18) reworks the map service, then (c): one protocol family, and the API is already HTTP/2 |
| 32 | **The root CA's key on a single host** (I48) — *settled 2026-10-21: keep it on a trusted host for now; offline is allowed, not required (D70)*: the all-in-one keeps it in `/etc/hyperstor/pki`, readable by root only; D53 wants it offline | (a) keep it for trial clusters; (b) export it encrypted at install and delete it | (a) for `--all-in-one`, (b) for multi-host installs, once `cluster init` exists |
| 33 | **What `out` means** (I45): today it marks a device out and returns; with as many failure domains as copies, its PGs run degraded | (a) keep it as "service"; (b) add a "drain" that refuses when PGs would end up degraded and runs as an `Operation` until backfill ends | (b), as §23.3 asks; keep (a) as the service mode |
| 34 | **Join tokens need a map that changes at run time** (I50): D57 needs the map service to add nodes and issue certificates, and the map isn't persisted (I18) | (a) persist the working map in a file first; (b) wait for Raft | (b) if Raft is next anyway; the shared-cluster-file install covers multi-host testing until then |
| 35 | **Control threads off reactor cores** (D58, point 30): on 4 shared CPUs, API load doubles the QD1 read tail | (a) pin control threads to non-reactor cores now (`CPUAffinity=` in the units, once `--cpus` is set); (b) wait for G17 on a small host | (a) for the installer's units when it pins reactors; confirm with G17 |

## Administration (2026-10-21)

The installer, API, CLI, UI and security of the control plane
(administration.md §23). Proposed in conversation with options and a
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

The rest of HS1 (`.meta` header objects and volume leases, metadata.md
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

After AD1's joins (implementation-notes.md I80–I93), the owner settled
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
