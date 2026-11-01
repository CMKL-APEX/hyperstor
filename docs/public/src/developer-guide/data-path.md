# The data path: reactor and engine

Each storage device is split into **shards**, and each shard is owned by
one **reactor**: a thread pinned to a core, running Hyperstor's own
io_uring event loop. The shard's engine, its network port, its
replication and its recovery all run on that thread. Nothing on the data
path takes a lock shared with another core.

## The reactor

| Aspect | As built | Why |
|---|---|---|
| Threading | One thread, one io_uring (1,024 entries), one FIFO task queue; futures are not `Send` and never leave their core | Shared-nothing, like Seastar: no locks, no cache-line bouncing between cores |
| Buffers | Each I/O owns its buffer until its completion arrives; dropping a future sends an `AsyncCancel` and frees the buffer only on completion | io_uring may still write into a buffer after the future is gone |
| Sockets | TCP `accept`, `connect`, `recv` and `send` as io_uring operations; UDP for peers' pings; eventfds and Unix sockets for `hyperstor-vhost` | One mechanism for disk and network |
| Waking from other threads | A shared queue and an `eventfd` that always has a read armed | The control plane (map pushes, the API) can reach a reactor without a lock on the data path |
| Polling | After each request the loop polls for 200 µs before sleeping in the kernel (`hyperstord --spin-us`) | Cuts QD1 latency by 15–23 % at little cost when idle; idle cores still sleep |
| Timers | Short timers are io_uring timeouts; per-request deadlines (the 2 s replica timeout) use one 50 ms ticker per reactor | One io_uring timeout per write cost about 3 % of write throughput ([Performance](performance.md#findings-that-changed-the-design)) |

**Why not tokio, glommio, monoio or SPDK:** see
[decisions](decisions.md#data-path-and-runtime). In short, owning the
loop lets disk I/O, sockets, and later RDMA completion queues and
virtqueues be polled from one place, with no FFI on the data path and no
devices taken away from the kernel.

The reactor issues `uring_cmd` passthrough commands (the ublk front end
uses them); NVMe passthrough through the same interface, zero-copy send
and provided buffers are planned.

**Stalls are counted.** Each reactor runs a 10 ms timer; when it fires
100 ms or more late, something held the thread, and the reactor logs it
and counts it in `hyperstor_reactor_stalls_total`. Each device also logs how long it took
to open, and any commit or in-place write that took that long. **Why:**
a held reactor answers no pings, so its peers report its node silent; the
log says which work held it.

### Waiting for the device

A reactor that waits for its device serves nothing else meanwhile, pings
included: one slow flush got healthy nodes reported and marked down. So
the engine is being made **completion-driven**: it plans an operation in
memory, the reactor submits the device I/O on its own ring, and the
operation finishes when the completions arrive. It comes in stages.

| Stage | Status | What waits off the reactor's thread |
|---|---|---|
| Commits | in progress: built for file and block devices | A commit round's data and log writes and its flush. One round is in flight per shard; other requests to the shard wait for it, as before, since the index already shows the round |
| Client reads | in progress: built for file and block devices | Reads and layered reads: up to 128 per device in flight, first come first served, and many per connection, so a write no longer waits behind a read on its connection |
| Checkpoint slices, deferred in-place writes, recovery and scrub reads | planned | These still wait in the device's own calls |
| A commit planned while the last one flushes | maybe later | |

Memory devices and the test rig's devices have no file descriptor and
still complete in their own calls. On a cluster of three hosts at 15,000
writes a second, no reactor was held 100 ms or more once commits waited
off the thread; before, at that cluster's ordinary load, one
node's flushes held its reactor for up to 205 ms.

Two timeouts follow, since a hung device no longer stops its node's pings
and would otherwise hang unseen:

- **I/O timeout** (30 s, `hyperstord --io-timeout-secs`, as Linux gives an
  NVMe command): a commit whose writes and flush take longer fails, and the
  device is failed and its PGs placed elsewhere, as after an I/O error. The
  node stays up.
- **Read timeout** (2 s, `hyperstord --read-timeout-ms`): the primary
  abandons the read and asks the PG's other acting copies, in turn, to
  answer from their own copies, once the writes queued before the read are
  applied there. If none answers in time, the client gets `Unavailable` and
  retries. Three timeouts in a row fail the device; a read in time resets
  the count.

**Reads in flight and freed space.** A read plans which physical blocks to
read, then waits. If a commit meanwhile frees those blocks (an overwrite
moved the data elsewhere) and a later write reuses them, the read would
return the wrong data. So extents freed while reads are in flight are
held, by generation, until every read begun before the free has ended;
and while a read is in flight, a small overwrite is redirected rather
than deferred in place (on hard disks). **The cost:** one counter per
read, and space freed at most a read timeout late. A randomized test
runs reads, writes and deletes begun and ended in any order against a
model.

## The engine

The engine stores **objects**. An object has sparse data in 4 KiB
blocks, a table of attributes, and an ordered key–value table. Volumes
are made of chunk objects; volume headers, and later the key–value store,
S3 and the filesystem, use the same objects in their own namespaces.

### Operation lists

A change is an **operation list**, applied atomically: all of it or none
of it survives a crash. Operations include `Write`, `Zero`, `Truncate`,
`SetAttr`, `KvPut`, `KvDelRange`, `Delete`, and asserts
(`AssertRevision`, `AssertExists`, `AssertAbsent`) checked before the
list applies. Asserts make compare-and-set possible, which the volume
headers use. `Absorb` serves snapshot deletion: it hands the blocks an
older version of a chunk holds, and a newer one lacks, to the newer one,
then deletes the older, all in one log record and without copying any
data. `Take` moves a whole object (blocks, attributes and key–value
table) to a new key in one constant-size log record, which a PG split
uses ([placement](replication.md#pg-counts-grow-by-splitting)). Reads can
go through **layers**: each block comes from the first of
a list of objects (a chunk's versions) that holds it, zeros if none
does. Every change gets a revision from a counter that never
repeats, so compare-and-set has no ABA problem.

### On-device format

```text
device:  [superblock A][superblock B][shard 0][shard 1] ... [shard n]
shard:   [super A][super B][log ring][data area]
```

- The **device superblock**, in two copies, names the device (its id in
  the map), the cluster, the format version and three words of **feature
  flags**: *compatible* ones, which a build that doesn't know them
  ignores; *read-only-compatible* ones, which it could only read past
  (and refuses, since no build opens a device read-only); and
  *incompatible* ones, which it refuses. Every format change from now on
  takes a flag, as in ext4 and XFS.
- The two shard superblocks alternate by sequence number, so a torn
  superblock write leaves the other one valid.
- The **log ring** is a sixteenth of the shard, between 1 MiB and 1 GiB.
  It holds whole-block records, each with a CRC32C seeded by a salt that
  changes on every open, so an old record left in the ring is never
  mistaken for a new one.
- The **data area** holds extents. Every 4 KiB block in an extent has its
  own CRC32C, kept in the index and carried on the wire unchanged from
  the client to the device and back.

### Whose device it is

Before a device is opened, `hyperstord` connects to the map service and
reads the device superblock alone, before any log is replayed:

- The device id must be the one this reactor serves, and the cluster the
  map's. A drive moved from another host or cluster, or two device paths
  swapped, is refused with what it holds, and never written. A superblock
  written before the cluster was recorded gets it recorded once, the second
  copy first, so a crash leaves the old or the new.
- Only a **blank** device is formatted: one where neither copy holds a
  Hyperstor superblock. A newer format version, a damaged superblock or a
  shard without one is refused, never formatted over.
- `hyperstord` formats only files. Block devices are formatted by the
  installer, after it looks for other systems' signatures (partition
  tables, filesystems, swap, LVM, md RAID, LUKS, Ceph BlueStore) in the
  device's first and last 128 KiB, unless the operator forces it.

**Why:** the superblock recorded its device, but nothing read it; a
swapped drive would have been replayed and served as this device's data.
Checking from the superblock alone keeps a refused device untouched.

### Redirect-on-write, and deferral for hard disks

On flash, every write goes to **new** extents, and the log record that
commits it names them: data is written once. On hard disks
(`hyperstord --hdd`), a small overwrite (under 64 KiB) of blocks an
object already holds goes into the log record and is applied in place
later, so the disk doesn't seek to a new place for every small write.

**Why:** on flash, deferring writes data twice. Measured on the same
drive, redirecting gave 1.6× the 4 KiB write throughput, and end to end
19 % more throughput with a 2.6× lower p99 ([Performance](performance.md)).

### Group commit

A per-reactor committer drains the queue of operation lists: it writes
their new extents and one log record, issues **one flush** for the whole
round, then acknowledges every list in it. At batch 32 the log costs
about 128 bytes per update (257 B with checkpoints).

### Checkpoints

The index lives in memory and is written out by **fuzzy checkpoints**:
starting when a quarter of the log is used, each commit round also writes
64 KiB of the checkpoint (8× that past half), and the checkpoint must be
done by three quarters. The superblock then names the new checkpoint and
the log position after it.

**Why fuzzy:** writing the whole checkpoint at once stalled commits for
8–13 ms at p99. In slices, commit p99 is 109 µs at the same throughput.

**Paged.** The index is cut into pages by key range, about 128 KiB each.
Every change marks the page holding its key, and a checkpoint writes only
the pages marked since the last one began, each to fresh blocks, then a
directory of every page. A page that grows past its size is cut in two;
one left empty is dropped, its range joining the page before.
**Why it is safe:** a page that nothing changed since the last
checkpoint began is still right as written then, and every change it
lacks is in a page written now or in the log replayed after it. **What it gains:**
checkpoint traffic follows the share of the index that changes, not its
size: 2.4× less when a tenth of the chunks are written, 16× less for a
hundredth. Uniform random writes touch every page and gain nothing.
Recovery time is unchanged.

### When a shard fills

- **Room for a checkpoint is kept.** A change that would take the space a
  whole checkpoint needs is refused (no space), while one that only frees
  (delete, truncate, zero, absorb) goes through. A checkpoint that still
  finds no room is abandoned, not fatal, and runs again once room is
  back. **Why:** a checkpoint that failed used to poison the shard, so
  every volume on it got I/O errors instead of "no space".
- **The `full` ratio** (95 % by default, set cluster-wide in the map)
  refuses, from each commit round, a list that takes room past it: a new
  object, or a write to blocks an object doesn't hold yet. Overwrites, and
  an existing object's keys and attributes, go on, so volume leases and
  headers keep working on a full device. See
  [fullness](replication.md#when-devices-fill).

### Recovery

On open, the engine loads the checkpoint the newer superblock names,
replays the log from there, and checks the data CRCs of the last commit
group: a record whose data didn't reach the media is dropped, with
everything after it. Replay runs at about 270 MiB of log per second;
opening takes about 0.3 s plus 4.5 ms per MiB replayed.

### The index

Each object's block map starts as a list of **runs** (extents). When a
chunk gets fragmented by random overwrites, it switches to a **dense
map**: 8 bytes per block (a 32-bit physical block and its CRC), and back
to runs when it compacts again.

**Why:** after random overwrites, run maps cost about 33 GiB of memory
per TiB of data; dense maps cap it at 2 GiB per TiB. A copy-on-write
B+tree on the device, which pages the index out of memory, is planned
for when index memory calls for it; paged checkpoints already write only
what changed (see [decisions](decisions.md#the-index)). Snapshots don't wait for it: they
are versions of chunk objects ([volumes](volumes.md#snapshots-and-clones)).

### Content digests

The engine can compute a 64-bit **digest** of each object from its index
alone, reading no data: FNV-1a over its size, each non-zero block's
number and CRC32C, its attributes and its key–value table. Replication
uses these digests for peering, delta recovery and light scrub
([replication](replication.md#peering-and-recovery-by-content-digests)).
Listing them costs about 0.5 µs per small object and 20–50 µs per 4 MiB
chunk. An object's digest is cached until it changes. A large one (an
object with a key–value table of hundreds of MiB) is taken in parts of
1 MiB with a yield between them, and digest lists and backfill compare in
batches of 64 objects, so no PG holds the reactor for long; the digest
is the same as taken at once, so nodes of both versions agree.

## Crash testing

The engine's crash test runs random operation lists against a reference
model (a sorted map in memory), and cuts the power at random device
operations: blocks not yet flushed are randomly kept or lost, so writes
are torn and reordered, and it cuts the power again during recovery.
After each crash the recovered shard must equal the model after every
acknowledged list plus some prefix of the ones in flight. 300 seeds run
on every change (`HS_CRASH_SEEDS` sets more); a 5,000-seed run (30,000
crashes) has passed.
