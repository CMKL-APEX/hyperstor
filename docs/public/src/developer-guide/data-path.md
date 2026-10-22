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
| Sockets | TCP `accept`, `connect`, `recv` and `send` as io_uring operations | One mechanism for disk and network |
| Waking from other threads | A shared queue and an `eventfd` that always has a read armed | The control plane (map pushes, the API) can reach a reactor without a lock on the data path |
| Polling | After each request the loop polls for 200 µs before sleeping in the kernel (`hyperstord --spin-us`) | Cuts QD1 latency by 15–23 % at little cost when idle; idle cores still sleep |
| Timers | Short timers are io_uring timeouts; per-request deadlines (the 2 s replica timeout) use one 50 ms ticker per reactor | One io_uring timeout per write cost about 3 % of write throughput ([Performance](performance.md#findings-that-changed-the-design)) |

**Why not tokio, glommio, monoio or SPDK:** see
[decisions](decisions.md#data-path-and-runtime). In short, owning the
loop lets disk I/O, sockets, and later RDMA completion queues and
virtqueues be polled from one place, with no FFI on the data path and no
devices taken away from the kernel.

**As built, with a known limit:** device I/O uses a second io_uring
owned by the engine and waits for it, so a commit's flush blocks its
reactor for the flush's duration. Making the engine completion-driven on
the reactor's own ring is planned. The reactor issues `uring_cmd`
passthrough commands (the ublk front end uses them); NVMe passthrough
through the same interface, zero-copy send and provided buffers are
planned.

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
data. Reads can go through **layers**: each block comes from the first of
a list of objects (a chunk's versions) that holds it, zeros if none
does. Every change gets a revision from a counter that never
repeats, so compare-and-set has no ABA problem.

### On-device format

```text
device:  [superblock A][superblock B][shard 0][shard 1] ... [shard n]
shard:   [super A][super B][log ring][data area]
```

- The two shard superblocks alternate by sequence number, so a torn
  superblock write leaves the other one valid.
- The **log ring** is a sixteenth of the shard, between 1 MiB and 1 GiB.
  It holds whole-block records, each with a CRC32C seeded by a salt that
  changes on every open, so an old record left in the ring is never
  mistaken for a new one.
- The **data area** holds extents. Every 4 KiB block in an extent has its
  own CRC32C, kept in the index and carried on the wire unchanged from
  the client to the device and back.

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
B+tree on the device, which pages the index, is planned after
checkpoints that seal the index and write only what changed (see
[decisions](decisions.md#the-index)). Snapshots don't wait for it: they
are versions of chunk objects ([volumes](volumes.md#snapshots-and-clones)).

### Content digests

The engine can compute a 64-bit **digest** of each object from its index
alone, reading no data: FNV-1a over its size, each non-zero block's
number and CRC32C, its attributes and its key–value table. Replication
uses these digests for peering, delta recovery and light scrub
([replication](replication.md#peering-and-recovery-by-content-digests)).
Listing them costs about 0.5 µs per small object and 20–50 µs per 4 MiB
chunk.

## Crash testing

The engine's crash test runs random operation lists against a reference
model (a sorted map in memory), and cuts the power at random device
operations: blocks not yet flushed are randomly kept or lost, so writes
are torn and reordered, and it cuts the power again during recovery.
After each crash the recovered shard must equal the model after every
acknowledged list plus some prefix of the ones in flight. 300 seeds run
on every change (`HS_CRASH_SEEDS` sets more); a 5,000-seed run (30,000
crashes) has passed.
