# Benchmarks

Status: **working record.** Each run lists the host, the method and the
raw numbers, so later runs can be compared. Reproduce with the commands
given; the tool is `crates/hyperstor-bench`.

## Hosts

| Name | CPU | Storage | Kernel |
|---|---|---|---|
| dev-1 | AMD EPYC 9124 (16 cores, 32 threads, one socket) | one consumer NVMe SSD (WD SN770 1 TB, DRAM-less, volatile write cache, FUA supported, **no power-loss protection**), ext4 on LVM, holding the root filesystem | Linux 7.0 |
| dev-2 | 2 × AMD EPYC 7742 (128 cores, 256 threads), 1 TiB of memory | Samsung PM983 and PM1733 data-center NVMe drives (write-through cache), ext4 on md RAID; shared with other sessions | Linux 6.8 |

On dev-1, the engine runs on a 2 GiB file on the root filesystem, opened
`O_DIRECT` and driven through io_uring (the block back end of D29). The
file is preallocated and then written once in full, so that no read is
served from an unwritten extent and no write converts one. NVMe
passthrough can't be measured there (01-implementation-notes.md I11).

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
   See I14 in 01-implementation-notes.md: deferral may be worth keeping only
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
   05-decision-log.md, point 5.

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

| Map lease | liveness timeout | peer reports (D40) | stall p50 ms | stall max ms |
|---|---|---|---|---|
| 2,000 ms (default) | 3,000 ms (default) | off | 3,115 | 3,128 |
| 500 ms | 700 ms | off | 783 | 785 |

After peer reports were built (a second run, `hyperstor-bench failover`):

| Map lease | liveness timeout | peer timeout | stall p50 ms | stall max ms |
|---|---|---|---|---|
| 2,000 ms | 3,000 ms | off | 2,964 | 3,132 |
| 2,000 ms | 3,000 ms | 1,000 ms | 2,457 | 2,460 |
| 500 ms | 700 ms | off | 784 | 784 |
| 500 ms | 700 ms | 300 ms | 552 | 553 |

With peer reports the stall sits at about 1.25 leases, as predicted: the
map service still waits out the dead node's lease before it republishes.

The recommended 1 s target (05-decision-log.md point 13), measured with
`scripts/collect-decision-data.sh failover` twice, back to back, at the
commit after peer reports and cache pools. Both runs, three stops each:

| Map lease | liveness timeout | peer timeout | stall p50 ms, run 1 | run 2 | stall max ms |
|---|---|---|---|---|---|
| 2,000 ms | 3,000 ms | off | 2,962 | 3,108 | 3,112 |
| 2,000 ms | 3,000 ms | 1,000 ms | 2,458 | 2,461 | 2,462 |
| 600 ms | 800 ms | off | 913 | 913 | 914 |
| 600 ms | 800 ms | 300 ms | 662 | 663 | 664 |
| 500 ms | 700 ms | off | 783 | 784 | 785 |
| 500 ms | 700 ms | 300 ms | 551 | 551 | 552 |

The stall follows two simple rules, to within a few milliseconds:

- without peer reports, the liveness timeout plus about 110 ms (the
  liveness check's period and the client's retry);
- with peer reports, 1.25 leases less the age of the dead node's last
  heartbeat (up to 100 ms): 750 − 88, 625 − 74, 2,500 − 42 ms. The
  quarter is the map service's margin for clock-rate drift (I29), so it
  is the floor while leases are the fence.

Peer reports cut the 1 s setting from 0.91 to 0.66 s. These are loopback
numbers with no load: they say nothing yet about false detections, which
decide how short the lease can safely be.

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

### Checkpoint traffic and recovery against log size

`hyperstor-bench logsize` (also part of `decisions`). 2 GiB of 4 MiB chunks
on a memory device, aged by one full pass of random 4 KiB overwrites (dense
maps, 64 KiB slices, a 4 MiB index), then batches of 32 random overwrites
until three checkpoints finish. Recovery: the shard is dropped as a
checkpoint starts (a quarter of the log to replay, the most fuzzy
checkpoints leave unless one falls behind) and opened again. The last column
scales checkpoint traffic to a 1 TiB shard, since the index grows with it.

| Log MiB | writes | checkpoints | checkpoint MiB each | log B per write | checkpoint B per write | checkpoint MB/s at 50k writes/s | replayed MiB | open ms | MB/s at 50k writes/s per TiB |
|---|---|---|---|---|---|---|---|---|---|
| 64 | 393312 | 3 | 4.0 | 128 | 32.3 | 1.6 | 16 | 382 | 826 |
| 256 | 1572960 | 3 | 4.0 | 128 | 8.1 | 0.4 | 64 | 608 | 207 |
| 1024 | 6291552 | 3 | 4.0 | 128 | 2.0 | 0.1 | 256 | 1498 | 52 |

Checkpoint traffic falls in exact proportion to the log size. Opening costs
about 0.3 s plus 4.5 ms per MiB replayed (about 225 MiB/s, in line with the
replay benchmark above).

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
   are needed before shards get that big (05-decision-log.md).


## 2026-10-21: cluster-API load against I/O latency, G17 first part (dev-2)

Host **dev-2**: AMD EPYC 7742, 2 sockets, 256 CPUs, 1 TiB of memory, Linux
6.8 (not the dev-1 host of the sections above). Commit `a29162f` plus the
`adminload` suite. Question (G17, D58): does load on the cluster API
move client I/O latency?

Method: `hyperstor-bench adminload` runs a map service with its cluster
API and three storage nodes on **memory devices** in one process, and
the client workload (pinned to CPU 1) for 3 s per test: idle, then while
4 threads call `GetStatus` at 1000 calls/s in total over keep-alive
HTTP/1.1, twice each, back to back. Memory devices remove the drive, so
what's left is CPU and scheduling interference, which is G17's concern.
Each `GetStatus` crosses to the map service's reactor and clones the map
(64 PGs).

```sh
HS_TAIL=1 HS_BENCH_SECS=3 scripts/collect-decision-data.sh adminload
HS_TAIL=1 HS_BENCH_SECS=3 taskset -c 0-3 target/release/hyperstor-bench adminload
```

p99 in µs, idle → under API load, for rounds 1 and 2 (p50 didn't move
by more than 7% anywhere):

| Test | 256 CPUs | 4 CPUs |
|---|---|---|
| 4 KiB writes, QD1 | 147 → 170, 165 → 175 | 205 → 175, 188 → 195 |
| 4 KiB writes, QD32 | 2773 → 2917, 2951 → 2828 | 2259 → 2352, 2358 → 2204 |
| 4 KiB reads, QD1 | 64 → 67, 74 → 72 | **56 → 104, 54 → 111** |
| 4 KiB reads, QD32 | 1359 → 1438, 1403 → 1464 | 1154 → 1208, 1141 → 1228 |
| `GetStatus` itself, p50 / p99 | 76 / 437, 80 / 298 | 154 / 3344, 188 / 3429 |

Throughput changed by less than 3% everywhere except reads on 4 CPUs
(−3 to −5%).

### Findings

1. **With cores to spare, API load doesn't show:** every change on the
   256-CPU run is within the difference between the two idle rounds
   (about 10%), so a 5% threshold (G17) can't be confirmed on this host
   at this run length.
2. **Sharing four cores, the QD1 read tail doubles** (p99 54–56 → 104–111
   µs, the same in both rounds) while p50 stays at 41–42 µs: an API call
   occasionally takes the core a reactor would have polled on. Writes,
   slower per call, hide it. This is the all-in-one install on a small
   machine, and it supports D58's rule that control-path threads run on
   cores of their own.
3. **The API answers 1000 status calls per second at 80 µs p50** with
   spare cores; every call clones the map on the map service's reactor,
   which will grow with the map (§23.3's local-map reads would avoid the
   reactor entirely).
4. Not measured yet: load on storage nodes' admin ports (metrics aren't
   built), real devices, and the 50 UI watchers of G17.

## 2026-10-21: request counters on the I/O path, A/B against main (dev-2)

Question: do the storage nodes' new metric counters (I49: two relaxed
atomic adds per request, gauges copied once a second) cost anything?
`hyperstor-bench cluster mem 1` (three storage nodes of one reactor on
memory devices, 3 s per test), built from `main` (`cdff61f`) and from
this branch, run alternately twice:

```sh
HS_BENCH_SECS=3 hyperstor-bench cluster mem 1
```

| Test (ops/s; p99 µs) | main, run 1 | branch, run 1 | main, run 2 | branch, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 7485; 154 | 7563; 154 | 7548; 155 | 7640; 151 |
| 4 KiB writes, QD32 | 26839; 2557 | 26362; 2734 | 25563; 2897 | 26893; 2640 |
| 4 KiB reads, QD1 | 18789; 65 | 18813; 65 | 18884; 66 | 19053; 64 |
| 4 KiB reads, QD32 | 53973; 1337 | 53689; 1341 | 54372; 1325 | 54206; 1336 |
| 1 MiB writes, QD4 | 514; 12724 | 518; 12662 | 511; 13366 | 525; 12251 |
| 1 MiB reads, QD4 | 1466; 4774 | 1525; 4269 | 1501; 4457 | 1520; 4460 |

Then again with latency histograms added (a clock read per client
request and three more relaxed adds when it is answered), alternately:

| Test (ops/s; p99 µs) | main, run 1 | branch, run 1 | main, run 2 | branch, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 7578; 154 | 7579; 154 | 7593; 154 | 7555; 153 |
| 4 KiB writes, QD32 | 26551; 2707 | 26546; 2662 | 26432; 2724 | 26841; 2581 |
| 4 KiB reads, QD1 | 18740; 65 | 18719; 66 | 18731; 66 | 18731; 66 |
| 4 KiB reads, QD32 | 54291; 1333 | 53860; 1342 | 54324; 1327 | 53857; 1347 |
| 1 MiB writes, QD4 | 511; 13808 | 519; 12486 | 511; 13026 | 520; 12305 |
| 1 MiB reads, QD4 | 1490; 4340 | 1505; 4867 | 1478; 4800 | 1523; 4183 |

### Findings

1. **Counters cost nothing measurable:** every difference in the first
   table is smaller than the difference between two runs of one build.
2. **Histograms cost at most about 1% at QD32 reads** (53860 and 53857
   against 54291 and 54324 ops/s, in both pairs), the most
   request-heavy test; elsewhere nothing shows. Acceptable for always-on
   metrics; a coarser clock would remove it if it matters.

## 2026-10-21: refilling a returning node, delta recovery (dev-2)

Question: how much does a node that returns after a short absence get
sent, now that backfill sends only objects whose digests differ (I57),
against a whole refill, and how long until its PGs are clean? Four
storage nodes of one reactor on 512 MiB memory devices, a pool of 3
copies and 64 PGs in 1 MiB chunks, one 256 MiB volume. Node 3 is
stopped; once the map service marks it down, a share of the chunks is
overwritten (spread evenly over the volume); node 3 is started again on
its old devices. "Whole refill" is what node 3 holds, which is what the
previous backfill sent. Run twice through the collect script (commit
`77b0d45`):

```sh
scripts/collect-decision-data.sh rejoin
```

| Chunks changed | node 3's copies (MiB) | sent to node 3, MiB | whole refill, MiB | until clean, run 1 s | run 2 s |
|---|---|---|---|---|---|
| 3 of 256 (1%) | 192 | 2.0 | 192 | 0.18 | 0.14 |
| 26 of 256 (10%) | 192 | 16.0 | 192 | 0.22 | 0.14 |
| 256 of 256 (100%) | 192 | 192.0 | 192 | 0.48 | 0.46 |

### Findings

1. **A returning node is sent what changed on its copies, to the
   block:** 2 MiB for 1% of the volume (96 times less than a whole
   refill), 16 MiB for 10% (12 times less). Of the 3 and 26 chunks
   changed, node 3 held 2 and 16.
2. **Time to clean drops by about two thirds** at 1–10% changed (0.14–
   0.22 s against 0.46–0.48 s). At these sizes most of it is the
   backfiller's 100 ms pass and the map's round trips, so on real
   volumes, where copying dominates, the gain follows the bytes.
3. **When everything changed, delta costs only a digest list more:**
   at 100% it sends the same 192 MiB as a whole refill, plus one list of
   about 40 bytes per object, in the same time.

The data path itself didn't change; checked with `hyperstor-bench
cluster mem 1` (three storage nodes of one reactor on memory devices,
3 s per test), built from `main` (`e5657f8`) and from this branch
(`77b0d45`, scrub off as in every bench), run alternately twice:

| Test (ops/s; p99 µs) | main, run 1 | branch, run 1 | main, run 2 | branch, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 7480; 154 | 7507; 155 | 7443; 155 | 7514; 154 |
| 4 KiB writes, QD32 | 25088; 2917 | 26119; 2732 | 25482; 2831 | 26349; 2672 |
| 4 KiB reads, QD1 | 18779; 65 | 18843; 65 | 18493; 66 | 18819; 65 |
| 4 KiB reads, QD32 | 53764; 1343 | 54168; 1330 | 53259; 1360 | 54405; 1326 |
| 1 MiB writes, QD4 | 516; 12685 | 516; 12670 | 529; 12113 | 523; 12576 |
| 1 MiB reads, QD4 | 1492; 4604 | 1521; 4391 | 1527; 4241 | 1502; 5423 |

4. **No cost on the I/O path:** every difference is within the spread
   between two runs of one build.

## 2026-10-21: deep scrub against I/O, and the cost of a digest list (dev-2)

**Deep scrub against I/O** (I60). Question: what does a deep scrub cost
the I/O path? `hyperstor-bench cluster <dir> 1`: three storage nodes of
one reactor on 1 GiB `O_DIRECT` files (ext4 on md RAID of data-center
NVMe, shared with other sessions), 3 s per test, scrub off against
`HS_SCRUB_MS=500` (a pass every 0.5 s, so a deep pass every 3.5 s, far
more often than the default daily light and weekly deep), alternately
twice; commit `bbdc0cd` plus scrub pacing:

```sh
hyperstor-bench cluster target/bench-files 1
HS_SCRUB_MS=500 hyperstor-bench cluster target/bench-files 1
```

Whole objects read at once (as built):

| Test (ops/s; p99 µs) | off, run 1 | scrub, run 1 | off, run 2 | scrub, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 1988; 581 | 1901; 1529 | 1988; 583 | 1904; 988 |
| 4 KiB writes, QD32 | 6055; 11792 | 5745; 14321 | 6028; 11689 | 5798; 13487 |
| 4 KiB reads, QD1 | 3956; 318 | 3801; 341 | 3997; 316 | 3790; 353 |
| 4 KiB reads, QD32 | 12355; 6680 | 11551; 9723 | 12290; 6736 | 11483; 9840 |
| 1 MiB writes, QD4 | 301; 25119 | 323; 24839 | 327; 24226 | 302; 26457 |
| 1 MiB reads, QD4 | 1212; 6383 | 1190; 7189 | 1222; 6093 | 1182; 7380 |

In 256 KiB pieces with a pause after each (tried, then reverted):

| Test (ops/s; p99 µs) | off, run 1 | scrub, run 1 | off, run 2 | scrub, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 1989; 582 | 1749; 1180 | 1987; 586 | 1756; 1204 |
| 4 KiB writes, QD32 | 6017; 11620 | 5635; 13194 | 6047; 11710 | 5671; 12992 |
| 4 KiB reads, QD1 | 3992; 312 | 3649; 875 | 4003; 314 | 3668; 855 |
| 4 KiB reads, QD32 | 12327; 6695 | 10889; 9152 | 12299; 6738 | 10968; 8423 |
| 1 MiB writes, QD4 | 352; 25304 | 269; 27776 | 330; 24421 | 280; 25503 |
| 1 MiB reads, QD4 | 1230; 6175 | 1147; 7451 | 1228; 6329 | 1166; 6946 |

**The cost of a digest list** (I57). `hyperstor-bench digest`: one PG
on a memory device, the time to list it and digest every object, from
the index; run twice through the collect script:

| Objects in the PG | blocks each | run 1: ms; µs per object | run 2: ms; µs per object |
|---|---|---|---|
| 10,000 | 1 | 4.5; 0.45 | 4.6; 0.46 |
| 100,000 | 1 | 62.5; 0.63 | 62.7; 0.63 |
| 1,000 | 256 (1 MiB) | 5.3; 5.3 | 5.3; 5.3 |
| 500 | 1024 (4 MiB) | 24.9; 50 | 9.5; 19 |

### Findings

1. **With a deep pass every 3.5 s (weekly by default), deep scrub costs
   4–6% of throughput**; p99 rises most for QD32 reads (+45%) and QD1 writes.
   At the default (weekly), a device of 1 TiB is read for about 4.5 hours
   a week at 64 MiB/s, with this cost meanwhile.
2. **Smaller reads made it worse** (−12% throughput, QD1 read p99
   ×2.7), so the cost is the extra device and CPU work, not one long
   synchronous read; pieces were reverted. Deep scrub belongs in the
   `bulk` class (D46), at a rate measured on dedicated drives.
3. **A digest list costs about 0.5 µs per small object and 20–50 µs per
   4 MiB chunk**, so a PG of 10,000 chunks (40 GiB) lists in 0.2–0.5 s on
   each side. Against that, a whole refill of the same PG moves 40 GiB;
   a PG log would only pay off for PGs of millions of objects with few
   writes missed.

## 2026-10-21: failover with peering (dev-2)

Question: does peering (I61), one digest round trip per PG on the new
primary's first request, lengthen failover? The failover suite of
2026-10-21 (PR #4), run twice through the collect script with peering
built in:

```sh
scripts/collect-decision-data.sh failover
```

| Map lease | down after | peer timeout | before (PR #4), p50 ms | with peering, run 1 | run 2 |
|---|---|---|---|---|---|
| 2000 ms | 3000 ms | off | 2962–3108 | 3103 | 3103 |
| 2000 ms | 3000 ms | 1000 ms | 2458–2461 | 2452 | 2453 |
| 600 ms | 800 ms | off | 913 | 920 | 1050 |
| 600 ms | 800 ms | 300 ms | 662–663 | 660 | 660 |
| 500 ms | 700 ms | off | 783–784 | 780 | 780 |
| 500 ms | 700 ms | 300 ms | 551 | 550 | 549 |

### Findings

1. **Peering doesn't move failover**: every setting is within the
   spread measured before, except 600/800 ms without peer reports in
   run 2 (1050 ms in all three of its stops, against 920 ms in run 1).
   Peering would shift every row alike, so that is the liveness tick's
   alignment in that run, not peering; D48's 1 s target is met with peer
   reports (660 ms) either way.
2. **Peering costs the I/O path nothing measurable** (a map lookup per
   request to a primary): `hyperstor-bench cluster mem 1`, `main`
   (`e5657f8`) against this branch (`da7b3eb`), alternately twice:

| Test (ops/s; p99 µs) | main, run 1 | branch, run 1 | main, run 2 | branch, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 7443; 155 | 7473; 154 | 7426; 156 | 7407; 155 |
| 4 KiB writes, QD32 | 25726; 2791 | 25936; 2761 | 25462; 2865 | 25676; 2831 |
| 4 KiB reads, QD1 | 18779; 66 | 18646; 65 | 18833; 64 | 18547; 65 |
| 4 KiB reads, QD32 | 54465; 1326 | 53984; 1339 | 53765; 1344 | 53914; 1339 |
| 1 MiB writes, QD4 | 519; 13372 | 523; 13034 | 523; 12683 | 524; 12783 |
| 1 MiB reads, QD4 | 1514; 4583 | 1502; 4637 | 1517; 4214 | 1536; 4153 |

## 2026-10-21: the map service's leader fails (dev-2)

Question: how long does I/O stop when the map service's leader dies
(D71–D73)? Three members on loopback, four storage nodes knowing all
three, 400 ms leases and a 700 ms liveness timeout; Raft heartbeats
every 50 ms and elections after 200–400 ms (I64). After the leader is
stopped, one client writes 1 MiB chunks without pause for 3.4 s; the
longest gap between two successful writes is the stall. Three runs:

```sh
cargo test --release -p hyperstor-daemon --test meta_group -- --nocapture
```

| Run | Longest gap between writes |
|---|---|
| 1 | 461 ms |
| 2 | 460 ms |
| 3 | 368 ms |

### Findings

1. **A map-service failover costs about one lease of I/O** (0.37–0.46 s
   with 400 ms leases): storage nodes serve on until their leases run
   out, the election takes 200–400 ms, and the new leader grants leases
   at once (D73). At the default 600 ms lease, expect about 0.7 s.
2. **Role changes wait 1.25 leases longer** (D73), so a storage node that
   fails at the same moment is detected about 0.5 s later than usual.

## 2026-10-22: forced takeover, and leases on the I/O path (dev-2)

Question: how long does a forced takeover take with D75's 1 s / 4 s
leases, and do the blocklist checks cost the I/O path anything (D76)?

1. **Takeover.** The cluster test `a_forced_takeover_blocklists_the_old_holder`
   (four storage nodes in one process, 400 ms map leases), timed around
   `take_over` (the old holder stopped renewing just before) and
   `preempt` (the holder still renewing), five runs:

```sh
cargo test --release -p hyperstor-daemon --test cluster -- a_forced --nocapture
```

| Run | `take_over` | `preempt` |
|---|---|---|
| 1 | 3.996 s | 3.764 s |
| 2 | 3.993 s | 3.770 s |
| 3 | 3.993 s | 3.766 s |
| 4 | 3.997 s | 3.768 s |
| 5 | 4.010 s | 3.793 s |

2. **I/O path.** `HS_BENCH_SECS=3 hyperstor-bench cluster mem 1`, `main`
   (`586d476`) against this branch, alternately twice:

| Test (ops/s; p99 µs) | main, run 1 | branch, run 1 | main, run 2 | branch, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 7536; 153 | 7419; 155 | 7428; 156 | 7460; 154 |
| 4 KiB writes, QD32 | 26891; 2594 | 26291; 2686 | 26396; 2687 | 26325; 2688 |
| 4 KiB reads, QD1 | 18710; 65 | 18360; 66 | 18542; 66 | 18427; 69 |
| 4 KiB reads, QD32 | 54232; 1336 | 53582; 1350 | 53848; 1337 | 53567; 1352 |
| 1 MiB writes, QD4 | 517; 13129 | 515; 12896 | 525; 12449 | 517; 12726 |
| 1 MiB reads, QD4 | 1510; 4863 | 1501; 4364 | 1521; 4320 | 1493; 4504 |

### Findings

1. **A forced takeover takes one volume lease, 4 s** (D75): it waits
   for the old holder's lease to run out from its last renewal; the
   blocklist round adds a few milliseconds, since every node
   acknowledges the pushed map at once (I75). It would add up to 1.25
   map leases (0.75 s at the default) for a node that doesn't answer.
   A preemption takes the same, less the time since the holder's last
   renewal.
2. **The blocklist costs the I/O path nothing measurable**: one map
   lookup per request, skipped while the list is empty. The two builds
   differ by no more than run to run (about 2%).

## 2026-10-22: the rest of HS2 on the I/O path (dev-2)

Question: do the slow-replica timeout (D91), the repair hold and the
failed-device check cost the write path anything? `HS_BENCH_SECS=3
hyperstor-bench cluster mem 1`, `main` (`08407b4`) against this branch,
alternately twice, at three stages of the branch:

| Branch build | 4 KiB writes QD1, ops/s (main; branch) | 4 KiB writes QD32, ops/s (main; branch) |
|---|---|---|
| a reactor timer per replica per write | 7468, 7520; 7238, 7227 | 25687, 26443; 25328, 24916 |
| one reactor timer per write | 7416, 7471; 7229, 7220 | 26306, 26475; 25622, 25735 |
| coarse deadlines, one ticker per reactor | 7423, 7403; 7401, 7412 | 26774, 26336; 25965, 26251 |

Reads (4 KiB QD1 and QD32, 1 MiB QD4) differed by no more than run to
run in every build.

### Findings

1. **A reactor timer is not free on the I/O path.** Each is an io_uring
   timeout: a submission, and an entry that stays in the kernel until
   it fires, so a 2 s timeout at 7,000 writes a second keeps about
   14,000 of them pending. One per write cost about 3% of write
   throughput. Coarse deadlines (a sorted map per reactor, woken by one
   50 ms ticker) cost nothing measurable; use them for any per-request
   timeout.
2. **The rest of HS2 costs the I/O path nothing measurable** with them:
   QD1 writes are within 0.3% of `main`, QD32 within 2% (run to run).

## 2026-10-22: a volume through ublk, first look (dev-2)

Question: does a `hyperstor-ublk` device carry ordinary block I/O, and
roughly how fast, before a test host with NVMe-backed nodes (G22)? An
all-in-one cluster of three nodes from `hyperstor-test` (each a 1 GiB
file on the shared root filesystem, flushed at every commit), a 512 MiB
volume filled first, served with `--depth 64`; fio 3.36, `libaio`,
direct, 10 s per job:

```sh
fio --name=t --filename=/dev/ublkb0 --rw=randread --bs=4k --iodepth=1 \
    --ioengine=libaio --direct=1 --time_based --runtime=10 --size=512M
```

| Job | IOPS | MiB/s | p50 µs | p99 µs |
|---|---|---|---|---|
| 4 KiB random read, QD1 | 3,391 | 13.2 | 276 | 371 |
| 4 KiB random read, QD32 | 11,442 | 44.7 | 2,245 | 6,980 |
| 4 KiB random write, QD1 | 748 | 2.9 | 1,286 | 1,679 |
| 4 KiB random write, QD32 | 3,544 | 13.8 | 7,242 | 33,817 |
| 1 MiB write, QD4 | 285 | 284.7 | 13,304 | 25,821 |
| 1 MiB read, QD4 | 789 | 789.0 | 4,620 | 7,897 |

### Findings

1. **It works end to end**: raw I/O across chunk boundaries, ext4, a
   detach and attach with the data intact, and fio at depth (`ublk::`
   test, run with `HS_UBLK=1`).
2. **A full cluster was an I/O error.** A first run used a 2 GiB volume
   on a cluster that holds under 1 GiB; writes past that failed with
   `EIO`, because a shard out of space was answered `Io`. It is now
   answered `NoSpace` and the device fails the write with `ENOSPC`; the
   test fills a 4 GiB volume and checks for it.
3. **These numbers are this cluster's, not ublk's**: writes are bound by
   the scratch files' flushes on a shared disk (the same path to memory
   devices does 7,400 4 KiB writes a second at QD1), and three nodes
   and the client share the host's cores with other work. What ublk and
   its one queue cost needs the comparison of G22.

## 2026-10-22: paged checkpoints (dev-2)

### Checkpoint traffic against the share of chunks written

`hyperstor-bench paged`. 2 GiB of 4 MiB chunks on a memory device (a
4 MiB index of dense maps; chunk keys spread over 128 PGs, as a volume's
are), aged by one full pass of random 4 KiB overwrites, a 256 MiB log.
Then batches of 32 random overwrites to the first share of the chunks,
until three checkpoints finish after two to settle. "Whole" is one page:
the checkpoint before D105. The second-last column scales the traffic to a
1 TiB shard with the same share written.

| Chunks written | checkpoint | pages | pages written per checkpoint | checkpoint B per write | checkpoint MB/s at 50k writes/s per TiB | open ms |
|---|---|---|---|---|---|---|
| 100% | whole | 1 | 1.0 | 8.07 | 207 | 392 |
| 100% | paged | 32 | 32.0 | 8.26 | 211 | 372 |
| 10% | whole | 1 | 1.0 | 8.07 | 207 | 307 |
| 10% | paged | 32 | 13.0 | 3.36 | 86 | 331 |
| 1% | whole | 1 | 1.0 | 8.07 | 207 | 301 |
| 1% | paged | 32 | 2.0 | 0.52 | 13 | 296 |

### Recovery against the log replayed, before and after

`hyperstor-bench replay` (the method of "Recovery time against log
length" above), main and this branch built from the same sources, run back
to back. Before boxing the deferred write in the logged operation, the
added physical blocks made every operation 80 bytes instead of 56, and
reading and replaying the log took 12–17% longer; boxed, it is back to
56 bytes.

| Log replayed | main: open ms (runs) | paged, boxed: open ms (runs) | log scan ms (main / paged) | replay ms (main / paged) |
|---|---|---|---|---|
| 64 MiB | 410–415 (5) | 423 (1) | — | — |
| 256 MiB | 1,424–1,455 (5) | 1,449–1,478 (3) | 761 / 747–750 | 383 / 364 |
| 700 MiB | 3,704–4,562 (5) | 3,752–4,396 (3) | 1,997 / 1,927–2,185 | 1,046 / 977–1,012 |

The 700 MiB row varies by 20% between runs of the same binary (a shared
host, with other sessions' builds), so it shows no difference either way.

### Findings

1. **Checkpoint traffic follows the share of the index that changes**:
   2.4× less when a tenth of the chunks are written, 16× less for a
   hundredth. Pages are cut by key range, and a volume's chunks spread
   over every PG, so a hot tenth of the chunks still touches 40% of the
   pages.
2. **Uniform random writes gain nothing** (every page changes each
   quarter of the log) and pay about 2% for the directory and page
   headers. The log-size table above still holds for them.
3. **Recovery time is unchanged**: reading every page in one batch costs
   no more than the one blob did, and the logged operation stays the same
   size.

## 2026-10-23: checkpoint page size (dev-2)

`hyperstor-bench paged`, as above, with page sizes from 16 to 128 KiB.
The default chunk is 4 MiB, so a chunk's dense block map is 8 KiB; a
128 KiB page holds about 16 chunks. "Of which directory" is the
directory's share of the checkpoint bytes (it is written by every
checkpoint, whatever changed). Load average about 3 on a shared host;
open times are within run-to-run variation.

| Chunks written | page | pages | pages written per checkpoint | directory KiB | checkpoint B per write | of which directory | checkpoint MB/s at 50k writes/s per TiB | open ms |
|---|---|---|---|---|---|---|---|---|
| 100% | whole | 1 | 1.0 | 0.1 | 8.07 | 0% | 207 | 385 |
| 100% | 128 KiB | 32 | 32.0 | 1.9 | 8.26 | 0% | 211 | 376 |
| 100% | 64 KiB | 64 | 64.0 | 3.9 | 8.51 | 0% | 218 | 371 |
| 100% | 32 KiB | 128 | 128.0 | 7.8 | 9.02 | 0% | 231 | 367 |
| 100% | 16 KiB | 256 | 256.0 | 15.5 | 10.03 | 0% | 257 | 370 |
| 10% | whole | 1 | 1.0 | 0.1 | 8.07 | 0% | 207 | 308 |
| 10% | 128 KiB | 32 | 13.0 | 1.9 | 3.36 | 0% | 86 | 305 |
| 10% | 64 KiB | 64 | 26.0 | 3.9 | 3.46 | 0% | 89 | 316 |
| 10% | 32 KiB | 128 | 51.0 | 7.8 | 3.60 | 0% | 92 | 303 |
| 10% | 16 KiB | 256 | 51.0 | 15.5 | 2.02 | 2% | 52 | 305 |
| 1% | whole | 1 | 1.0 | 0.1 | 8.07 | 0% | 207 | 303 |
| 1% | 128 KiB | 32 | 2.0 | 1.9 | 0.52 | 1% | 13 | 299 |
| 1% | 64 KiB | 64 | 3.0 | 3.9 | 0.41 | 2% | 10 | 300 |
| 1% | 32 KiB | 128 | 5.0 | 7.8 | 0.37 | 4% | 9 | 296 |
| 1% | 16 KiB | 256 | 5.0 | 15.5 | 0.23 | 14% | 6 | 299 |

### Findings

1. **Smaller pages help a busy volume only once a page nears one
   chunk's map.** A volume's chunks spread over every PG, so its hot
   chunks land in different pages: halving the page doubles the pages
   written and leaves the bytes about the same (10% of the chunks: 3.36
   B per write at 128 KiB, 3.60 at 32 KiB). At 16 KiB, about two chunks
   a page, each hot chunk costs its own page: 2.02 B per write, 40% less.
2. **Every halving costs uniform writes**, which change every page: +2%
   at 128 KiB, +5% at 64, +12% at 32 and +24% at 16 against one page.
   Most of it is padding (each page is rounded up to whole blocks) and
   page headers, not the directory.
3. **The directory stays small** (0.4% of the index at 16 KiB, about
   8 MiB per TiB of shard) but is written every time: at 16 KiB it is 14%
   of what a checkpoint writes when 1% of the chunks change.
4. **128 KiB stays the default** (01-implementation-notes.md I150). The
   gain from smaller pages is small until 16 KiB, which costs a quarter
   more for VM disks written all over, the common case with dense maps
   (benchmarks above). What the hot-volume case needs is fewer pages per
   volume, which the on-device index of D43 gives by writing changed
   nodes, not smaller pages.

## 2026-10-29: lease identities on the I/O path, A/B against main (dev-2)

Question: does carrying a lease identity on every request, and checking
it against the blocklist (D150), cost the I/O path anything?
`hyperstor-bench cluster mem 1` (three storage nodes of one reactor on
memory devices, 3 s per test), built from `main` (`3475d8b`) and from
this branch, run alternately twice:

```sh
HS_BENCH_SECS=3 hyperstor-bench cluster mem 1
```

| Test (ops/s; p99 µs) | main, run 1 | branch, run 1 | main, run 2 | branch, run 2 |
|---|---|---|---|---|
| 4 KiB writes, QD1 | 7322; 157 | 7275; 159 | 7271; 159 | 7267; 159 |
| 4 KiB writes, QD32 | 26273; 2668 | 25752; 2771 | 25646; 2798 | 26046; 2666 |
| 4 KiB reads, QD1 | 18350; 66 | 18510; 66 | 18255; 67 | 18270; 67 |
| 4 KiB reads, QD32 | 53423; 1354 | 53740; 1343 | 53813; 1340 | 53687; 1340 |
| 1 MiB writes, QD4 | 521; 12776 | 513; 13240 | 521; 12285 | 507; 12751 |
| 1 MiB reads, QD4 | 1488; 5514 | 1512; 4457 | 1484; 5747 | 1494; 4494 |

### Findings

1. **Nothing measurable**: every difference between the builds is within
   the difference between two runs of one build (about 2%). The
   identity adds 8 bytes to each request's target; the blocklist is
   empty here, so its lookups are skipped, as in a cluster with no
   recent take-over.
2. Not measured: a non-empty blocklist, where each client request costs
   two hash lookups instead of one (I186).

## 2026-10-29: refilling an empty node (dev-2)

A three-host pilot cluster flapped when a rebuilt host rejoined empty:
every node reported the others silent while backfill ran, and for a
while no node was up (I187). The same happens on one host with the
end-to-end harness: an all-in-one cluster of 3 nodes (1 reactor each,
1 GiB devices, peer timeout 600 ms, loopback), a 512 MiB volume with
384 MiB written, node 2 killed, its device file deleted and node 2
started again:

```sh
cargo test --release -p hyperstor-test --test manage an_empty_node_is_refilled_without_flapping
```

| Build | Runs | Nodes reported silent during the refill | Refill to health ok |
|---|---|---|---|
| `main` (`3a1e7d5`) | 3 | node 2, 1–2 times each run | not within 180 s (flapping warns for 10 min) |
| at most 2 PG backfills per reactor, 8 MiB window each | 4 | none | 18.4–18.8 s |
| pings on their own connection only | 2 | none | 10.5–10.6 s |
| 16 MiB backfill budget per target only | 2 | none | 10.2–10.9 s |
| both (this branch) | 2 | none | 10.2–10.9 s |

### Findings

1. **Pings queued behind backfill.** A primary started a backfill for
   every PG it led at once, each with up to 16 pushes of up to 4 MiB in
   flight, all on its one connection to the target; a ping sent after
   them waited until they were on the wire. No request held the
   target's connection for more than 100 ms (measured with a temporary
   log in its server loop), so the target was not slow to answer.
2. Either change alone stops the flapping here. Capping the number of
   PG backfills slows a refill (18 s against 10 s) and kept PGs at one
   copy longer: the randomized failure test lost a write at seed 3
   (`too few copies`) when a second node died before the first was
   refilled. A byte budget per target bounds the same queue without
   that cost.
3. Not measured: a 1 GbE link, where 16 MiB of pushes is about 140 ms
   ahead of each replicated write; nor a client's write latency while
   a node is refilled.

## 2026-10-30: a returning node and a large table (pilot, dev-2)

The three-host pilot (nodes 0 and 1 on two dual-socket hosts, node 0 on
dev-2; node 2 on dev-1; 2 devices of 64 GiB per node, files on the root
filesystem, peer timeout 300 ms, 1 GbE between dev-1 and the others)
ran I187. Node 2 came back after about 7 hours down, its devices out,
and the cluster still flapped (I188):

| Time | Event (map service) |
|---|---|
| +0.4 s, +2.8 s, +7.2 s | node 2 up, reported silent by nodes 0 and 1 0.4 s later, held down 2, 4 and 8 s |
| +15.6 s | node 2 up for good; node 1, then node 0, reported silent and held down 2 s, for 0.5 s at once |

Only 67 pushes (280 MB) reached node 2: the refill was small. A build
logging device open times and reactor holds, installed on every node
and each restarted once:

| Node | Device open times | Reactor held after open |
|---|---|---|
| 0 | 80 ms, 2,036 ms | 270 ms |
| 1 | 77 ms, 1,986 ms | 270 ms |
| 2 | 56 ms, 4,366 ms | 457 ms |

Each slow device holds the PG of a `.meta` audit day written before
I185's parts: 660,234 key–value entries, 215 MB. On a copy of node 0's
slow device, on dev-2, with a test program around `hyperstor_engine::open`
and `Shard::digest`:

| Measure | Before | After |
|---|---|---|
| Open, page cache warm (3 runs) | 1.36–1.56 s (4.0 s cold) | 0.66–0.69 s |
| The table's digest, the reactor held | 289–290 ms at once | 216 parts of 1 MiB, longest 1.4–1.5 ms, 293 ms in all |

### Findings

1. **The time is CPU in the index, not I/O**: 9 io_uring submissions
   and 10 ms in system calls per open. A third of it was two copies of
   the checkpoint's index made for replay when there was no log to
   replay; the rest is decoding and inserting 660,234 entries.
2. A node came up on its fast device's heartbeat while its slow device
   was still opening; peers pinged the slow one.
3. A reactor held by the digest missed its peers' answers, and reported
   them: two held at once marked a healthy node down.
4. In the end-to-end harness, a stale node coming back, out and in
   again, with 64 GiB devices and the 300 ms peer timeout did not flap:
   its tables are small. The in-process tests reproduce each cause with
   a slow device instead (I188).
5. **Deployed** (main at `2eb859a`, I188), after the audit day was
   pruned, with a rolling install on all three hosts: each node went down
   once for its restart and came up once. Node 2's slower device opened
   in 3.3 s and the node came up 0.1 s after it; no other node was
   reported, `hyperstor_reactor_stalls_total` stayed 0 on every device,
   and a 32 MiB I/O test passed from every host (25–28 MiB/s writes,
   86–102 MiB/s reads).

