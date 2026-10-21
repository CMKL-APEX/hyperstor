# Benchmarks

Status: **working record.** Each run lists the host, the method and the
raw numbers, so later runs can be compared. Reproduce with the commands
given; the tool is `crates/hyperstor-bench`.

## Hosts

| Name | CPU | Storage | Kernel |
|---|---|---|---|
| dev-1 | AMD EPYC 9124 (16 cores, 32 threads, one socket) | one consumer NVMe SSD (WD SN770 1 TB, DRAM-less, volatile write cache, FUA supported, **no power-loss protection**), ext4 on LVM, holding the root filesystem | Linux 7.0 |

On dev-1, the engine runs on a 2 GiB file on the root filesystem, opened
`O_DIRECT` and driven through io_uring (the block back end of D29). The
file is preallocated and then written once in full, so that no read is
served from an unwritten extent and no write converts one. NVMe
passthrough can't be measured there (implementation-notes.md I11).

Caveats for dev-1:
- Flushes are expensive (no power-loss protection): a 4 KiB write plus
  `fdatasync` takes about 160 µs. Server drives with power-loss
  protection complete a flush in microseconds, so absolute write latencies
  here are much worse than a deployment's, and batching matters more.
- The drive is shared with the root filesystem and other work on the
  host, and it slows down once its write cache fills. Write results vary
  between runs by up to 3× after several GB of writes; compare runs made
  close together, and treat single numbers as indicative.

## 2026-10-20: engine HS0 (dev-1)

Commands:

```sh
cargo build --release -p hyperstor-bench
HS_BENCH_SECS=3 target/release/hyperstor-bench crc
HS_BENCH_SECS=3 target/release/hyperstor-bench raw  ~/hyperstor-bench/dev.img 2
HS_BENCH_SECS=3 target/release/hyperstor-bench engine ~/hyperstor-bench/dev.img 2
HS_DEFER_MAX=0 HS_BENCH_SECS=3 target/release/hyperstor-bench engine ~/hyperstor-bench/dev.img 2
HS_BENCH_SECS=2 target/release/hyperstor-bench mem
```

Every engine test runs on one shard, from one thread, with the operations
of a batch committed as one call (group commit, I2). "Per call" latency is
one `commit` or `read` call, so a batch of 32 has 32 operations in one call.
Objects are 4 MiB (like volume chunks), 64 of them, filled first by the
1 MiB test.

**CRC32C:** 6.75 GB/s on one core.

**Raw device** (io_uring, `O_DIRECT`, no engine):

| Test | ops/s | MB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| 4 KiB random writes + fdatasync, batch 1 | 6020 | 24.7 | 162 | 210 |
| 4 KiB random writes + fdatasync, batch 8 | 46508 | 190.5 | 156 | 409 |
| 4 KiB random writes + fdatasync, batch 32 | 117015 | 479.3 | 250 | 919 |
| 4 KiB random writes + fdatasync, batch 128 | 160154 | 656.0 | 633 | 2165 |
| 4 KiB random reads, QD1 | 18151 | 74.3 | 53 | 88 |
| 128 KiB random reads, QD1 | 3770 | 494.1 | 224 | 850 |
| 1 MiB random reads, QD1 | 1077 | 1129.6 | 924 | 1104 |
| 4 KiB random reads, QD32 (one submit) | 128906 | 528.0 | 244 | 314 |

**Engine, deferred small writes** (`defer < 64 KiB`, the D22 default):

| Test | ops/s | MB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| 1 MiB writes, new extents, QD1 | 1500 | 1572.6 | 611 | 1622 |
| 4 KiB overwrites, batch 1 | 5581 | 22.9 | 168 | 218 |
| 4 KiB overwrites, batch 8 | 32380 | 132.6 | 186 | 253 |
| 4 KiB overwrites, batch 32 | 64093 | 262.5 | 267 | 407 |
| 4 KiB overwrites, batch 128 | 84725 | 347.0 | 607 | 2765 |
| 64 KiB overwrites, batch 1 | 4959 | 325.0 | 192 | 283 |
| 64 KiB overwrites, batch 32 | 19698 | 1290.9 | 1172 | 5938 |
| 4 KiB random reads, QD1 | 14714 | 60.3 | 64 | 92 |
| 128 KiB random reads, QD1 | 6566 | 860.6 | 145 | 274 |
| 1 MiB random reads, QD1 | 1510 | 1583.3 | 652 | 828 |

**Engine, every write redirected** (`HS_DEFER_MAX=0`), run right after
the one above:

| Test | ops/s | MB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| 1 MiB writes, new extents, QD1 | 196 | 205.4 | 5907 | 12459 |
| 4 KiB overwrites, batch 1 | 4713 | 19.3 | 196 | 280 |
| 4 KiB overwrites, batch 8 | 35043 | 143.5 | 216 | 345 |
| 4 KiB overwrites, batch 32 | 86722 | 355.2 | 351 | 552 |
| 4 KiB overwrites, batch 128 | 132704 | 543.6 | 928 | 1131 |
| 64 KiB overwrites, batch 1 | 4694 | 307.6 | 200 | 318 |
| 64 KiB overwrites, batch 32 | 6515 | 427.0 | 1322 | 35212 |
| 4 KiB random reads, QD1 | 14210 | 58.2 | 79 | 95 |

The 1 MiB and 64 KiB rows of this run were hit by the drive's slowdown
(they write the same data as in the run before); its 4 KiB rows wrote
less data and are comparable.

**Engine on memory** (CPU cost only, no device):

| Test | ops/s | MB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| 4 KiB overwrites (deferred), batch 1 | 135010 | 553.0 | 3 | 8 |
| 4 KiB overwrites (deferred), batch 32 | 194632 | 797.2 | 78 | 93 |
| 4 KiB overwrites (deferred), batch 128 | 188294 | 771.3 | 321 | 12516 |
| 64 KiB overwrites (redirected), batch 32 | 53274 | 3491.4 | 552 | 575 |
| 4 KiB random reads, QD1 | 761876 | 3120.6 | 1 | 1 |

### Findings

1. **Engine overhead on reads is small:** 4 KiB QD1 reads take 64 µs
   against 53 µs raw; larger reads match the device.
2. **At batch 1, a write costs the same as a raw write plus flush**
   (168 µs against 162 µs): the flush dominates on this drive.
3. **Deferred small writes cost throughput on flash.** Each one is written
   twice (in the log, then in place), and at batch 128 the engine reaches
   85k ops/s against 160k raw. Redirecting every write reaches 133k.
   See I14 in implementation-notes.md: deferral may be worth keeping only
   for hard disks (D29), where it turns random writes into sequential
   ones.
4. **CPU per deferred 4 KiB write is about 5 µs** (190k ops/s on one core
   with no device). That is too much for a reactor that should do several
   hundred thousand operations per second; the copies and per-operation
   allocations in the commit path are the first things to remove (I12).
5. **Checkpoints show in p99** (batch 128 on memory: 12.5 ms), because a
   checkpoint writes the whole index (I3).
6. **G5 is half run:** the NVMe passthrough path needs a dedicated
   namespace. SPDK wasn't installed for the comparison either.

## 2026-10-20: placement (dev-1)

`HS_BENCH_SECS=2 target/release/hyperstor-bench placement`: 1,024 devices
in 128 hosts and 8 racks, 3 copies, one thread, topology built once per
map (`Topology`):

| Failure domain | PGs/s | µs per PG |
|---|---|---|
| host | 449,661 | 2.2 |
| rack | 756,105 | 1.3 |

The first version rebuilt the domain list for every PG (47 µs per PG by
host); building it once per map made placement 21× faster with identical
results. Placing every PG of a 65,536-PG pool after a map change takes
about 0.15 s on one core.

## 2026-10-20: end to end, one host (dev-1)

`HS_BENCH_SECS=3 target/release/hyperstor-bench cluster mem|<dir> [reactors]`:
a map service, three storage nodes and a client in one process on
dev-1, talking over loopback TCP. A replicated pool (size 3, 128 PGs,
4 MiB chunks), eight volumes of 64 MiB, filled first. Every write goes
client → primary → two replicas, each committing to its engine; reads go
to the primary. Each reactor is pinned to its own CPU; the client is one
reactor. "QD" is the number of concurrent requests from the client.

| Test | ops/s | MB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| mem, 1 reactor per node: 4 KiB random writes, QD1 | 10083 | 41.3 | 92 | 116 |
| mem, 1 reactor per node: 4 KiB random writes, QD32 | 30899 | 126.6 | 591 | 8445 |
| mem, 1 reactor per node: 4 KiB random reads, QD1 | 25385 | 104.0 | 40 | 57 |
| mem, 1 reactor per node: 4 KiB random reads, QD32 | 80482 | 329.7 | 219 | 1003 |
| mem, 1 reactor per node: 1 MiB writes, QD4 | 755 | 791.8 | 5200 | 8780 |
| mem, 1 reactor per node: 1 MiB reads, QD4 | 2218 | 2325.7 | 1648 | 3793 |
| mem, 4 reactors per node: 4 KiB random writes, QD1 | 6638 | 27.2 | 137 | 234 |
| mem, 4 reactors per node: 4 KiB random writes, QD32 | 46959 | 192.3 | 338 | 12884 |
| mem, 4 reactors per node: 4 KiB random reads, QD1 | 17785 | 72.8 | 56 | 89 |
| mem, 4 reactors per node: 4 KiB random reads, QD32 | 74617 | 305.6 | 206 | 1966 |
| mem, 4 reactors per node: 1 MiB writes, QD4 | 598 | 627.5 | 6335 | 12156 |
| mem, 4 reactors per node: 1 MiB reads, QD4 | 1441 | 1510.6 | 2351 | 8553 |
| files, 1 reactor per node: 4 KiB random writes, QD1 | 1906 | 7.8 | 495 | 628 |
| files, 1 reactor per node: 4 KiB random writes, QD32 | 4921 | 20.2 | 4957 | 19816 |
| files, 1 reactor per node: 4 KiB random reads, QD1 | 10363 | 42.4 | 94 | 141 |
| files, 1 reactor per node: 4 KiB random reads, QD32 | 35838 | 146.8 | 511 | 2295 |
| files, 1 reactor per node: 1 MiB writes, QD4 | 368 | 386.0 | 7821 | 77929 |
| files, 1 reactor per node: 1 MiB reads, QD4 | 1237 | 1296.9 | 2791 | 7876 |

### Findings

1. **The software path costs about 40 µs for a read and 90 µs for a
   replicated write** at QD1 on memory devices: two (read) or four
   (write) loopback TCP hops plus engine work. This is the floor to
   improve before RDMA (HS6) can matter.
2. **One client reactor tops out near 31k–47k writes and 75k–80k reads
   per second.** Several reactors per client (and per volume) are needed
   for more; the per-request copies in the client and daemon (payload
   copied into each frame, once per replica) are the first CPU to remove.
3. **More reactors per node raised QD32 write throughput by half** (more
   PGs committing in parallel) but **raised QD1 latency** (137 µs against
   92 µs): a request now often wakes a sleeping reactor on another core.
   Adaptive polling (spin briefly before sleeping in the kernel) is the
   usual fix.
4. **p99 write latency of 8–13 ms on memory** comes from engine
   checkpoints, which write the whole index (I3).
5. **On files, the three nodes share one consumer SSD without power-loss
   protection**, so every write waits for three flushes on the same
   drive (QD1: 495 µs; QD32: 4.9k ops/s). This says more about the
   test host than about hyperstor; a real cluster has a drive per node.

## 2026-10-20: separate processes, and a node killed (dev-1)

The README's cluster: `hyperstor-meta` and three `hyperstord` processes on
dev-1, each shard on a 1 GiB file created by `hyperstord` (sparse, so the
first write to each block also updates the ext4 journal, I28), driven by
`hyperstor-bench remote 127.0.0.1:19000` (2 s per test).

| Test | ops/s | MB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| 4 KiB random writes, QD1 | 942 | 3.9 | 994 | 1538 |
| 4 KiB random writes, QD32 | 2383 | 9.8 | 10388 | 34090 |
| 4 KiB random reads, QD1 | 10684 | 43.8 | 92 | 139 |
| 4 KiB random reads, QD32 | 36460 | 149.3 | 451 | 2304 |
| 1 MiB writes, QD4 | 391 | 410.0 | 10138 | 16853 |
| 1 MiB reads, QD4 | 1250 | 1310.4 | 2787 | 7928 |

Then `kill -9` of node 1's `hyperstord`, a 3 s wait (the map service marks
it down after 2 s), and the same workload for 1 s per test: every request
succeeded, now with two copies per write.

| Test | ops/s | MB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| 4 KiB random writes, QD1 | 1006 | 4.1 | 968 | 1643 |
| 4 KiB random writes, QD32 | 5301 | 21.7 | 5074 | 28416 |
| 4 KiB random reads, QD1 | 10327 | 42.3 | 94 | 128 |
| 4 KiB random reads, QD32 | 26158 | 107.1 | 1283 | 2358 |
| 1 MiB writes, QD4 | 616 | 646.1 | 6347 | 10381 |
| 1 MiB reads, QD4 | 1157 | 1213.5 | 3152 | 6264 |

Writes were faster degraded because each one waits for two flushes on the
shared drive instead of three.

## 2026-10-20: index size and checkpoint cost (dev-1)

`target/release/hyperstor-bench index <scratch file>`: one shard on a
buffered file on dev-1's disk, deferral off (D38). 1 GiB of 4 MiB objects
written sequentially, a checkpoint; then every block overwritten once in
random order (batches of 64), another checkpoint. Run twice, identical
results. "Index RAM" is the growth of the process's resident memory, which
for the sequential case is mostly write buffers the allocator keeps.

| 1 GiB of data | extent runs | index RAM MiB | checkpoint MiB | checkpoint ms | index RAM GiB per TiB | checkpoint GiB per TiB |
|---|---|---|---|---|---|---|
| written sequentially | 256 | 8.1 | 1.0 | 2 | 8.1 | 1.0 |
| every block overwritten (fully fragmented) | 262144 | 35.8 | 6.0 | 25 | 35.8 | 6.0 |

### Findings

1. **Checksums set the floor:** 4 bytes per 4 KiB block, 1 GiB per TiB.
2. **Fragmentation multiplies the index** by about 18 in RAM and 6 on
   disk, since every run carries a map entry and its own checksum vector.
   With redirect-on-write on flash (D38), random-write volumes get there.
3. **A whole-index checkpoint takes 2–25 ms per GiB**, so today's format
   pauses for seconds per TiB. The capacity each checkpoint approach
   allows, and the dense block map that bounds fragmentation, are in
   decision-log.md, point 5.

## 2026-10-21: dense maps, fuzzy checkpoints, decision data (dev-1)

Collected with `scripts/collect-decision-data.sh decisions cluster` (raw
output and host record under `target/decision-data/`, not committed),
plus `hyperstor-bench redb` and `hyperstor-bench failover`, with
`HS_BENCH_SECS=5`. The engine runs in buffered files on dev-1's disk for
the aging and pause tables, and in memory for the latency A/B.

### Index aging under random 4 KiB overwrites

1 GiB of 4 MiB chunks written sequentially, then overwritten at random,
redirect-on-write (D38); a checkpoint after each step. "Block maps" is
the estimated memory of the maps (calibrated, I35).

| Dense maps | overwrites | runs | dense chunks | block maps MiB | process growth MiB | checkpoint MiB | block maps GiB per TiB |
|---|---|---|---|---|---|---|---|
| off | 0 | 256 | 0 | 1.0 | 8.5 | 1.0 | 1.0 |
| off | 0.05× | 25,137 | 0 | 4.1 | 10.6 | 1.5 | 4.1 |
| off | 0.25× | 103,214 | 0 | 13.6 | 20.8 | 3.0 | 13.6 |
| off | 1× | 226,755 | 0 | 28.7 | 34.9 | 5.3 | 28.7 |
| off | 3× | 261,501 | 0 | 32.9 | 40.3 | 6.0 | 32.9 |
| on | 0 | 256 | 0 | 1.0 | — | 1.0 | 1.0 |
| on | 0.05× | 25,137 | 256 | 2.0 | — | 2.0 | 2.0 |
| on | 0.25× | 103,214 | 256 | 2.0 | — | 2.0 | 2.0 |
| on | 1× | 226,755 | 256 | 2.0 | — | 2.0 | 2.0 |
| on | 3× | 261,500 | 256 | 2.0 | — | 2.0 | 2.0 |

(Process growth isn't meaningful for the second run in the same process:
the allocator reuses the first run's memory.)

4 KiB reads from the page cache, aged index: 364k/s (p50 2.6 µs) with run
maps, 441k/s (p50 2.2 µs) with dense maps.

### Commit latency while checkpoints run

Memory device, 2 GiB aged once (dense maps), 16 MiB log so checkpoints
recur, batches of 32 random 4 KiB overwrites for 5 s.

| Checkpoint | commits | checkpoints | slices | p50 µs | p99 µs | p99.9 µs | max µs | metadata bytes per write |
|---|---|---|---|---|---|---|---|---|
| whole (one round) | 66,545 | 65 | 65 | 69 | 79 | 132 | 3,629 | 257 |
| fuzzy, 1 MiB slices | 66,585 | 65 | 325 | 69 | 88 | 644 | 744 | 257 |
| fuzzy, 256 KiB slices | 67,279 | 66 | 1,056 | 68 | 212 | 230 | 262 | 258 |
| fuzzy, 64 KiB slices | 66,204 | 65 | 4,160 | 70 | 109 | 119 | 776* | 258 |

\* A single outlier in this run; the previous run's 64 KiB maximum was
145 µs. 64 KiB is now the default (I33).

### Whole-checkpoint pause against data size (buffered file)

| Data | state | dense maps | checkpoint MiB | pause ms |
|---|---|---|---|---|
| 1 GiB | sequential | off | 1.0 | 2 |
| 1 GiB | aged 1× | off | 5.3 | 16 |
| 1 GiB | aged 1× | on | 2.0 | 4 |
| 4 GiB | sequential | on | 4.1 | 23 |
| 4 GiB | aged 1× | off | 21.4 | 269 |
| 4 GiB | aged 1× | on | 8.1 | 14 |

Pauses on the shared drive vary by up to 10× between runs (flushes); the
sizes are exact.

### G13 baseline: a copy-on-write B+tree for the index's updates

`hyperstor-bench redb`: redb 3, a block-map table with 524,288 entries
(2 GiB of data's worth), random updates in transactions of 1 or 32, each
made durable; the engine on the same disk with the same pattern, where
every update is also a 4 KiB data write.

| Store | updates per commit | commits/s | updates/s | p50 µs | p99 µs | metadata bytes written per update |
|---|---|---|---|---|---|---|
| redb, fsync per commit | 1 | 4,126 | 4,126 | 218 | 342 | 24,896 |
| redb, fsync per commit | 32 | 1,294 | 41,395 | 740 | 1,117 | 7,833 |
| engine (log + fuzzy checkpoint), with the data | 1 | 2,263 | 2,263 | 427 | 663 | 4,096 |
| engine (log + fuzzy checkpoint), with the data | 32 | 530 | 16,963 | 1,843 | 2,816 | 128 |

The engine's batch-1 figure is one whole log block per commit (records
are padded to 4 KiB); at batch 32 the log costs 128 bytes per update,
257 with checkpoints included (table above).

### Failover stall

`hyperstor-bench failover`: four nodes, one reactor each, memory devices;
a client writes one volume continuously; the node holding its chunk 0's
primary is stopped; the stall is from the stop to the first successful
write. Three runs per setting.

| Map lease | liveness timeout | stall p50 ms | stall max ms |
|---|---|---|---|
| 2,000 ms (default) | 3,000 ms (default) | 3,115 | 3,128 |
| 500 ms | 700 ms | 783 | 785 |

### Recovery time against log length

`hyperstor-bench replay`: a 1 GiB log on a memory device filled with
redirect-on-write records (64 writes per record, checkpoints held off),
the shard dropped without a clean stop, then opened again. Opening
includes loading the checkpoint, replaying, and the new checkpoint every
open writes.

| Log replayed | records | writes replayed | recovery ms | writes replayed per second |
|---|---|---|---|---|
| 64 MiB | 8,192 | 524,288 | 294 | 1.78 M |
| 256 MiB | 32,768 | 2,097,152 | 998 | 2.10 M |
| 700 MiB | 89,600 | 5,734,400 | 2,591 | 2.21 M |

About 270 MiB of log per second on one core, so recovery time grows with
the log: up to about 3 s for a 1 GiB log, about 11 s for 4 GiB (three
quarters replayed at most; shards recover in parallel, one per reactor).
On a device, reading the log adds little at NVMe speeds.

### End to end, before and after a regression fix

`hyperstor-bench cluster mem 1`, replicated writes at size 3. With dense
maps, `stats()` walked every block map, and the daemon called it after
every commit (fixed: `index_stats()` is separate, I37).

| Build | Deferral | 4 KiB writes QD1 p50 µs | 4 KiB writes QD32 ops/s | QD32 p99 µs |
|---|---|---|---|---|
| before the fix | off (D38) | 353 | 8,733 | 7,823 |
| after the fix | off (D38) | 101 | 37,765 | 2,065 |
| after the fix | on (64 KiB) | 100 | 31,714 | 5,409 |

### Adaptive polling (D41)

`HS_SPIN_US=N hyperstor-bench cluster mem R`: storage and client reactors
poll for N µs after activity before sleeping in the kernel. 2 s per test.

| Reactors per node | Polling window | 4 KiB writes QD1 p50 / p99 µs | 4 KiB writes QD32 ops/s | 4 KiB reads QD1 p50 / p99 µs |
|---|---|---|---|---|
| 1 | off | 109 / 183 | 36,677 | 43 / 72 |
| 1 | 50 µs | 96 / 131 | 38,331 | 37 / 59 |
| 1 | 200 µs | 84 / 130 | 38,426 | 36 / 54 |
| 4 | off | 140 / 1,092 | 67,729 | 56 / 101 |
| 4 | 50 µs | 139 / 224 | 66,852 | 48 / 96 |

### Findings

1. **Dense maps cap the index at 2 GiB per TiB**, against 33 GiB
   estimated (40 GiB of process growth) without them; reads are slightly
   faster. **Every chunk went dense after 5% random overwrites**, so dense
   is the steady state for VM disks.
2. **Fuzzy checkpoints remove the pause:** with 64 KiB slices the worst
   commit is 0.15–0.8 ms against 3.6 ms, at no cost in throughput.
3. **A copy-on-write B+tree committed per transaction writes 30–100× more
   metadata** (7.8–25 KB per update) than the log plus checkpoint
   (128–258 bytes). The B+tree of D43 must keep the log as its write-ahead
   log and commit only at checkpoints.
4. **Failover takes about the liveness timeout plus 0.1 s.** With peer
   reports (D40) detection gets faster, but the map service still waits
   out the dead node's lease, so the floor is about 1.25 leases: the lease
   length is the main lever (0.78 s with a 0.5 s lease).
5. **D38 holds end to end:** with redirect-on-write, replicated writes
   reach 19% more throughput than with deferral, with 2.6× lower p99.
6. **Adaptive polling cuts QD1 latency by 15–23%** (200 µs window) and
   the 4-reactor write p99 by 5×, with no throughput change; the rest of
   the per-request cost is system calls and copies (D41's remaining
   work). `hyperstord` now polls for 200 µs by default.
7. **Whole-index checkpoints get expensive for large shards even when
   fuzzy:** at 2 GiB of checkpoint per TiB, a 4 TiB shard taking 50k
   writes per second would rewrite its 8 GiB index about every 10 s
   (checkpoints start every 64 MiB of log at 128 bytes per write), about
   800 MB/s of background writes. Paged checkpoints, or a much larger log,
   are needed before shards get that big (decision-log.md).

