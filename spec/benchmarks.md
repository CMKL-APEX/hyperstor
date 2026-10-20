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
