# Implementation notes

Status: **working log, for review with the project owner.** Decisions made
while implementing (`I…`), and considerations to discuss. Design decisions
stay in [design.md](design.md); a note here that changes the design says
so, and design.md gets the change once the owner agrees.

Benchmarks are in [benchmarks.md](benchmarks.md). Tests: `cargo test --workspace`; the crash test's seed count is `HS_CRASH_SEEDS` (300 by default; a 5,000-seed run, 30,000 crashes, passed on 2026-10-20).

## Engine (HS0)

| # | Decision | Why | To discuss |
|---|---|---|---|
| I1 | **A device is split into shards, one per reactor**, each with its own log ring, allocator and index; a PG lives in one shard. | No locks or cross-core traffic on the I/O path (design §6.1), and recovery runs per shard in parallel. | Load across shards depends on PG placement; moving a PG between shards of one device (a local backfill) is not built yet. |
| I2 | **Commits are batch-synchronous (group commit).** `Shard::commit` resolves a batch of lists, writes new data extents and the log record together, flushes once, and returns. The reactor calls it with everything that arrived since its last pass. | The simplest design that is correct and still amortizes the flush over a batch. | The reactor waits during the flush. On NVMe with power-loss protection a flush costs microseconds; on drives without it, milliseconds. A completion-driven engine (submit, return to polling, finish on completion) is the next step once the reactor carries real traffic (HS1). |
| I3 | **The index lives in memory; a checkpoint is the whole index serialized** into freshly allocated extents, named by the shard's superblock. The on-device copy-on-write B+tree of design §7.1 is deferred. | Small and easy to verify; recovery is load plus log replay. | Memory is about 4 bytes per 4 KiB block (its checksum) plus per-run overhead, and a checkpoint pauses the shard for the time to write the whole index. Fine for test and first deployments; must become incremental (the B+tree) before large shards. |
| I4 | **One flush per commit round**, not a flush of new data and then of the log record. Recovery checks the last commit group's new data against its checksums, and drops a record whose data didn't reach the media, with everything after it. Only blocks the replayed index still maps to the same place are checked. | Halves flushes per commit. A dropped record was never acknowledged: rounds are sequential, so only the last group can be incomplete. | Correct only while rounds don't overlap. If commits are pipelined later, recovery must check every group not known to be flushed. |
| I5 | **The log's checksums are seeded with a random salt that changes every time a shard is opened**, and opening always ends with a checkpoint under the new salt. | Stale blocks from an earlier pass of the ring or an earlier format, and tenant data that happens to look like a record (deferred writes carry tenant bytes into the log), never pass as records. | The salt is 32 bits and CRC32C is not a MAC: forging a record still needs the salt guessed, and a crash at the right moment. A keyed MAC per record would close it at some CPU cost. |
| I6 | **Revisions come from a shard-wide counter** and are never reused, even across delete and recreate. | Compare-and-set on revisions can't suffer ABA. | — |
| I7 | **A write is never deferred onto blocks allocated in the same round.** | Recovery's data check (I4) compares those blocks with the checksums their `Map` logged; an in-place write in the same round would contradict it and drop an acknowledged group. | — |
| I8 | **Data offsets and lengths are 4 KiB aligned.** | Matches the device and the checksum granularity. | S3 objects and file tails need padding by the caller (the S3 gateway, the filesystem client), with the logical size kept separately (`Truncate`). |
| I9 | **`Truncate` keeps the bytes of a partial last block** past the new size. | Avoids a read-modify-write. | A later extension shows the old bytes. Filesystem semantics need zeroing there (filesystem.md). |
| I10 | **Assertions are checked against the object before the list**, not at their position in it. | Simple to reason about: a list is validated whole, then applied, so a refused list never leaves a trace. | — |
| I11 | **NVMe passthrough (`uring_cmd`) is not implemented yet.** The block back end (io_uring, `O_DIRECT`) serves every device. | The development host has one NVMe device and it holds the root filesystem; passthrough needs a namespace of its own. | G5 is only half run (benchmarks.md); passthrough and its benchmark need a test host with a spare namespace. |
| I12 | **Write data is copied once** into an aligned buffer before it goes to the device. | Network buffers aren't aligned yet. | Zero copy comes with registered buffers shared by the reactor's network and disk paths (design §6.2). |
| I14 | **The deferral threshold is a per-shard setting** (`Shard::set_defer_max`), 64 KiB by default as D22 says. | On flash, deferral writes small overwrites twice; redirecting them reached 1.6× the 4 KiB write throughput on dev-1 (benchmarks.md). | **Proposed design change:** defer only on hard disks (D29), and redirect every write on flash, as BlueStore does on SSDs. The cost is fragmentation of chunk objects, which matters little for flash reads. Needs the owner's agreement. |
| I13 | **A checkpoint must fit in 200 extents** (named in one superblock). | Keeps the superblock one block. | On a badly fragmented shard a checkpoint can fail with `NoSpace`; a chained extent list removes the limit. |

### Bugs the crash test found

The randomized crash test (`crates/hyperstor-engine/tests/crash.rs`) found
two engine bugs before any other test did:

1. `[Delete, Touch]` in one list deleted the object; `Touch` must recreate
   it. Fixed by tracking whether the object is alive through the list.
2. Recovery's data check (I4) looked at blocks that a later part of the
   same group had freed. Those can legitimately be reused (by the next
   round's data, or by the checkpoint an earlier recovery wrote), so an
   acknowledged group was dropped. Fixed by checking only blocks the
   replayed index still references.
