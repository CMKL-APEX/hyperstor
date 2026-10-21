# Implementation notes

Status: **working log, for review with the project owner.** Decisions made
while implementing (`I…`), and considerations to discuss. Design decisions
stay in the design documents ([README.md](README.md)); a note here that
changes the design says so, and the design gets the change once the owner
agrees. Decisions taken with the owner and open decision points are in
[decision-log.md](decision-log.md).

Benchmarks are in [benchmarks.md](benchmarks.md). Tests:
`cargo test --workspace --release`; the crash test's seed count is
`HS_CRASH_SEEDS` (300 by default; a 5,000-seed run, 30,000 crashes,
passed on 2026-10-20).

## Where things stand (2026-10-20)

Built, on branch `impl/hs0-hs1`:

| Milestone | Built | Not yet |
|---|---|---|
| HS0 | Engine (general objects, group commit, deferred writes, checkpoints, recovery; dense block maps and fuzzy checkpoints, D43), device back ends (memory, pread/pwrite, io_uring `O_DIRECT`, fault injection), crash tests, benchmarks | NVMe passthrough back end and the G5 comparison with SPDK (needs a spare namespace, I11) |
| HS1 | Placement, wire protocol, reactor, cluster map and map service, storage daemon with replication and group commit, client, `hyperstord` and `hyperstor-meta` binaries, end-to-end tests and benchmark | Raft for the map service (I18), `.meta` header objects and volume leases (metadata.md §4.2, §9) |
| HS2 | Liveness by heartbeats, degraded acting sets, backfill with a whole-object copy (including a node that returns with stale data: reset, refilled, stand-ins dropped), map leases with fencing and pushes (D42), peer reports (D40), failure end-to-end tests, a real multi-process failure check (`kill -9` of a node: writes go on with two copies) | PG log and delta recovery (replication.md §8.3), scrub, blocklist and forced takeovers (metadata.md §9), throttling |

### Review

The points raised here were reviewed with the owner on 2026-10-20; the
answers, and the two points still open (map leases, checkpoint spikes)
with their trade-offs, are in [decision-log.md](decision-log.md). Settled
points: D38 (I14), D39 (I19), D40 (I21, I25), D41 (I12, I23, I24), D42 (I20).

## Engine (HS0)

| # | Decision | Why | To discuss |
|---|---|---|---|
| I1 | **A device is split into shards, one per reactor**, each with its own log ring, allocator and index; a PG lives in one shard. | No locks or cross-core traffic on the I/O path (engine.md §6.1), and recovery runs per shard in parallel. | Load across shards depends on PG placement; moving a PG between shards of one device (a local backfill) is not built yet. |
| I2 | **Commits are batch-synchronous (group commit).** `Shard::commit` resolves a batch of lists, writes new data extents and the log record together, flushes once, and returns. The reactor calls it with everything that arrived since its last pass. | The simplest design that is correct and still amortizes the flush over a batch. | The reactor waits during the flush. On NVMe with power-loss protection a flush costs microseconds; on drives without it, milliseconds. A completion-driven engine (submit, return to polling, finish on completion) is the next step once the reactor carries real traffic (HS1). |
| I3 | **The index lives in memory; a checkpoint is the whole index serialized** into freshly allocated extents, named by the shard's superblock. The on-device copy-on-write B+tree of engine.md §7.1 is deferred. | Small and easy to verify; recovery is load plus log replay. | Open: decision-log.md, point 5 (fuzzy checkpoint now, paged index next). |
| I4 | **One flush per commit round**, not a flush of new data and then of the log record. Recovery checks the last commit group's new data against its checksums, and drops a record whose data didn't reach the media, with everything after it. Only blocks the replayed index still maps to the same place are checked. | Halves flushes per commit. A dropped record was never acknowledged: rounds are sequential, so only the last group can be incomplete. | Correct only while rounds don't overlap. If commits are pipelined later, recovery must check every group not known to be flushed. |
| I5 | **The log's checksums are seeded with a random salt that changes every time a shard is opened**, and opening always ends with a checkpoint under the new salt. | Stale blocks from an earlier pass of the ring or an earlier format, and tenant data that happens to look like a record (deferred writes carry tenant bytes into the log), never pass as records. | The salt is 32 bits and CRC32C is not a MAC: forging a record still needs the salt guessed, and a crash at the right moment. A keyed MAC per record would close it at some CPU cost. |
| I6 | **Revisions come from a shard-wide counter** and are never reused, even across delete and recreate. | Compare-and-set on revisions can't suffer ABA. | — |
| I7 | **A write is never deferred onto blocks allocated in the same round.** | Recovery's data check (I4) compares those blocks with the checksums their `Map` logged; an in-place write in the same round would contradict it and drop an acknowledged group. | — |
| I8 | **Data offsets and lengths are 4 KiB aligned.** | Matches the device and the checksum granularity. | S3 objects and file tails need padding by the caller (the S3 gateway, the filesystem client), with the logical size kept separately (`Truncate`). |
| I9 | **`Truncate` keeps the bytes of a partial last block** past the new size. | Avoids a read-modify-write. | A later extension shows the old bytes. Filesystem semantics need zeroing there (filesystem.md). |
| I10 | **Assertions are checked against the object before the list**, not at their position in it. | Simple to reason about: a list is validated whole, then applied, so a refused list never leaves a trace. | — |
| I11 | **NVMe passthrough (`uring_cmd`) is not implemented yet.** The block back end (io_uring, `O_DIRECT`) serves every device. | The development host has one NVMe device and it holds the root filesystem; passthrough needs a namespace of its own. | G5 is only half run (benchmarks.md); passthrough and its benchmark need a test host with a spare namespace. |
| I12 | **Write data is copied once** into an aligned buffer before it goes to the device. | Network buffers aren't aligned yet. | Zero copy comes with registered buffers shared by the reactor's network and disk paths (engine.md §6.2). |
| I14 | **The deferral threshold is a per-shard setting** (`Shard::set_defer_max`). Storage nodes turn deferral off unless told the device is a hard disk (`hyperstord --hdd`). | On flash, deferral writes small overwrites twice; redirecting them reached 1.6× the 4 KiB write throughput on dev-1 (benchmarks.md). | **Settled: D38.** |
| I13 | **A checkpoint must fit in 200 extents** (named in one superblock). | Keeps the superblock one block. | On a badly fragmented shard a checkpoint can fail with `NoSpace`; a chained extent list removes the limit. |

### Bugs the tests found

Across the session, tests found five bugs before anything else did:

1. Engine: `[Delete, Touch]` deleted the object (crash test).
2. Engine: recovery checked freed and reused blocks, dropping an
   acknowledged group (crash test).
3. Placement: positional EC placement moved twice the necessary shards
   (movement test, I16).
4. Daemon: a write could be acknowledged without a target that had just
   finished backfilling (cluster test, 2 in 25 runs; I26).
5. Reactor: a task that yields in a loop starved all I/O, because each
   pass ran ready tasks until none were left (the returning-node cluster
   test hung). Each pass now runs only the tasks ready when it starts;
   `a_yielding_task_does_not_starve_io` guards it.

Found in review while designing leases:

6. Map: a PG whose acting members were all down was reset to an empty
   acting set, so when the members returned, backfill from the empty
   primary would have wiped them. Such a PG now stays down with its acting
   set until a member returns (`a_pg_whose_copies_are_all_down_stays_down`).

### Details of the engine bugs

The randomized crash test (`crates/hyperstor-engine/tests/crash.rs`) found
two engine bugs before any other test did:

1. `[Delete, Touch]` in one list deleted the object; `Touch` must recreate
   it. Fixed by tracking whether the object is alive through the list.
2. Recovery's data check (I4) looked at blocks that a later part of the
   same group had freed. Those can legitimately be reused (by the next
   round's data, or by the checkpoint an earlier recovery wrote), so an
   acknowledged group was dropped. Fixed by checking only blocks the
   replayed index still references.

## Placement (HS1)

| # | Decision | Why | To discuss |
|---|---|---|---|
| I15 | **The placement hashes are spelled out** (SplitMix64 mixing, FNV-1a for names), with pinned test values, instead of `std`'s hashers. | Every node and client must compute the same placement from the same map, across Rust releases and architectures. | Changing them is a format change: it would move every PG. |
| I16 | **EC shard positions are kept in the PG table, not recomputed.** `place` chooses the members; `assign_positions` keeps each surviving member at its shard index and fills vacated positions with new members. | A stateless positional choice (CRUSH-style `indep`, tried first) moved 2.1× the necessary shards when a device was removed, because changed positions collide with others. With kept positions, only shards whose device left the set move (2,068 of 30,000 in the test, against 3,176). | The metadata service must keep positions per EC PG in its acting set (placement.md §5.3 already keeps acting sets). |
| I17 | **Weights are hierarchical**: a device's weight counts for its host and rack, and choices go rack, host, device. | Load follows capacity at every level, which is what keeps devices evenly full. | Removing one device moves 1.44× the theoretical minimum of PGs in the test (its host's share drops, so some PGs that used the host's other devices move too). CRUSH behaves the same. A flat choice over devices would move less but couldn't keep copies in distinct hosts by weight. |

## Map, protocol, reactor, daemon, client (HS1)

| # | Decision | Why | To discuss |
|---|---|---|---|
| I18 | **The map service is a single in-memory map** (`hyperstor_meta::serve`), not yet the Raft group of D2/D20. | HS1 needed a map to route by; Raft through `glidex-raftstore` needs that crate extracted from glidex first (a glidex change). | Until then the map service is a single point of failure for map changes (not for I/O, D23). |
| I19 | **Each reactor listens on its own address, recorded per device in the map.** | No cross-thread hand-off of sockets, and tests can bind ephemeral ports before the map exists. | **Settled: D39.** |
| I20 | **(Superseded by D42.)** **Storage nodes poll the map every 200 ms**; replicas refuse `Repl` for a PG epoch they don't have. | Simple, and it bounds how long a stale primary can find willing replicas. | Open: decision-log.md, point 3. |
| I21 | **A write fails if any acting member fails** (`Unavailable`), and the client retries until the map service has marked the node down and the PG's acting set has shrunk (I25). | No in-sync shrinking on a primary's say-so: one authority (the map service's liveness) decides who is out. | **Settled: D40** (nodes report each other; to build in HS2). |
| I22 | **A read of a chunk never written returns zeros** from the primary. | Thin volumes. | — |
| I23 | **Payloads are copied into each outgoing frame**, once per replica. | Simplest correct path. | Zero copy with registered buffers and vectored sends (`SEND_ZC`) is the next CPU win (benchmarks.md, end to end). |
| I24 | **The reactor's executor is a simple FIFO of ready tasks**, woken through an eventfd from other threads, with an `AsyncCancel` sent for operations whose future is dropped. | Small (about 400 lines) and enough for the data path; buffers stay owned by the reactor until the kernel completes, so cancellation is safe. | No adaptive polling yet: an idle reactor sleeps in `io_uring_enter`, which costs QD1 latency when requests cross cores (benchmarks.md, end to end). |

## Failure handling (HS2, first part)

| # | Decision | Why | To discuss |
|---|---|---|---|
| I25 | **Liveness by heartbeats to the map service** (every 150 ms from each reactor; a node is down after a timeout, 600 ms in tests and 2 s in the benchmark). Each PG has an `up` set (placement) and an `acting` set (who has every acknowledged write). When a node goes down, `acting` keeps its live members in order, so the new primary is a surviving member that has every acknowledged write. | Writes are acknowledged only when every acting member has them (§8.1), so any surviving member is complete. Keeping order avoids an election. | Detection by the map service only: a node that can reach the map service but not its peers isn't detected (metadata.md §4.1 asks peers to report each other; not built). |
| I26 | **Backfill**: the primary resets the target's copy of the PG, then pushes every object whole, each read taken only when no commit is queued and sent on the same connection as live writes. Live writes also go to the target meanwhile (best effort). If any write or push to the target fails, the backfill starts over. When it finishes, the target becomes *required* for every write before the primary waits out in-flight writes to it and reports `BackfillDone`; the map service then adds it to `acting`, and once all of `up` is there, `acting = up`. | Ordering on one connection makes each pushed object exactly the primary's state at that point, with later writes following. Making the target required before reporting closes a race the cluster test found (2 failures in 25 runs before the fix, 0 in 30 after): a write could be acknowledged while its copy to a target that had just joined `acting` failed. | Objects are pushed whole, holes included (reads materialize zeros), so a backfill writes every byte of each chunk; an extent list from the engine would send only what's there. No throttling or priorities yet (replication.md §8.5). A node that returns with old data gets a reset and a full copy, not a delta from the PG log (no PG log yet). |
| I27 | **Storage nodes refuse `Repl` and `Push` for PGs they are in neither `acting` nor `up` of**, and for PG epochs they don't have. | A node that is no longer a member must not accept writes. | — |

## Binaries

| # | Decision | Why | To discuss |
|---|---|---|---|
| I28 | **`hyperstord` and `hyperstor-meta` take flags and a plain-text cluster file** (`hyperstor_meta::parse_cluster`), no configuration library. | Enough to run a cluster by hand; no extra dependencies. | glidex will drive these (glidex.md §17); the cluster file is a stopgap until the map service has its management API (§15). `hyperstord` creates sparse files for shard devices; on ext4 the first write to each block then costs a journal update. Use block devices, or files written once in full. |

## Leases (HS2, D42)

| # | Decision | Why | To discuss |
|---|---|---|---|
| I29 | **Leases come from the map service**, with every heartbeat answer, and run from when the node *sent* the heartbeat; the map service measures expiry from when it granted the lease plus a quarter. Storage reactors send heartbeats every 100 ms. | The node's lease always ends before the map service believes it has, whatever the network delay; the quarter covers clock *rate* differences between hosts (no synchronized clocks needed). | Defaults: lease 2 s, liveness timeout 3 s (must be at least 1.25 × the lease; `hyperstor-meta` refuses otherwise). With D40, detection can drop below the timeout; the lease remains the floor of failover time. |
| I30 | **A fence is acknowledged only after the device's in-flight writes for the fenced PGs are answered.** | A write accepted under the old role and answered after the new map took effect could otherwise be acknowledged without reaching the new acting set. | — |
| I31 | **The RPC connection moved to its own crate**, `hyperstor-rpc` (re-exported as `hyperstor_client::rpc`). | The map service now calls storage reactors (fences, pushes) and couldn't depend on the client crate, which depends on it. | — |
| I32 | **Test hook `NodeConfig::meta_blocked`** cuts a node off from the map service in both directions (heartbeats, map fetches, pushes, fences) while clients and peers can still reach it. | Lets the cluster test reproduce the partition leases exist for. Removing the lease check makes that test fail with a stale read, which shows the test checks what it should. | Test-only; a fault-injection layer for the network (drops, delays, partitions between any two nodes) is the general form, for the HS2 fault rig. |

## Dense maps and fuzzy checkpoints (D43)

| # | Decision | Why | To discuss |
|---|---|---|---|
| I33 | **Fuzzy checkpoints** (engine.md §7.3): one starts when a quarter of the log is used; each commit round writes a slice (64 KiB, eight times that once half the log is used); it finishes at once if the log reaches three quarters first. The blob is a stream of objects; a directory, named by the superblock, lists the blob's extents (no limit on how many). Recovery replays from the checkpoint's start over the fuzzy image. | Removes the checkpoint pause (benchmarks.md: worst commit 3.8 ms whole, 0.15 ms with 64 KiB slices). Replay over a fuzzy image works because every log record sets state absolutely; the one exception, deferred writes onto blocks a later trim unmapped, is skipped during replay. The crash tests run with 1 KiB slices, so about 400 of each run's crashes land mid-checkpoint; a planted bug (log head at the end instead of the start) fails them at once. | Checkpoints still rewrite the whole index (2 GiB per TiB with dense maps), only spread out. Paged checkpoints (decision-log.md, point 5, option B) would write only what changed. |
| I34 | **Dense maps switch by estimated memory**, with hysteresis: to dense when runs cost more than 8 bytes per block of span, back to runs below 60% of that (a contiguous chunk is about 51%). A dense map is only reconsidered after a change of at least an eighth of its span. 32-bit physical blocks: a shard over 16 TiB keeps runs for blocks beyond. | Measured: every chunk was dense after only 5% random overwrites, so dense is the steady state for VM disks; the threshold matters less than first thought. Reconsidering after every write scanned 1,024 slots and made commits 13× slower; the size condition removes that. | — |
| I35 | **Run overhead calibrated to 128 bytes** (was an assumed 96), from the process's growth per run in the aging benchmark. | Decides when a chunk goes dense. | — |
| I36 | **Benchmarks for decisions are a repeatable suite**: `hyperstor-bench decisions` and `redb`, run through `scripts/collect-decision-data.sh`, which records the host; the procedure is the agent skill `.claude/skills/collect-decision-data`. | The owner decides from data; the data must be repeatable, with the host and the exact commands. | — |
| I37 | **`Shard::stats()` is constant-time; index-wide totals are `index_stats()`.** | The decision-data run caught a 3.5× regression in replicated writes: with dense maps, `stats()` walked every block map, and the daemon called it after every commit. | Benchmarks belong in every session for exactly this reason (the skill's step 2). |
| I38 | **Adaptive polling** (`hyperstor_reactor::set_spin`): after activity, a reactor polls (entering the kernel without waiting, so network completions arrive) for the window before sleeping. `hyperstord` defaults to 200 µs (`--spin-us`); everything else to 0. | QD1 latency −15 to −23%, 4-reactor write p99 −80% (benchmarks.md, adaptive polling). | The window is a CPU cost at low load; see decision-log.md point 16. |
| I39 | **Peer reports** (D40): every storage reactor pings, every 100 ms, the nodes it shares a PG with, and reports one silent for the peer timeout (`hyperstord --peer-timeout-ms`, default 1 s) to the map service; a node reported by two distinct nodes within half the liveness timeout is marked down and gets no lease until the reports stop. | Measured: failover 3.0 → 2.5 s at the default lease, 0.78 → 0.55 s with a 0.5 s lease (benchmarks.md, failover). The cluster test fails with peer reports turned off. | "Two distinct nodes" doesn't yet require different failure domains (the cluster file's racks); with one rack per site, any two nodes count. Pings scale with the number of peer nodes, not PGs. |

