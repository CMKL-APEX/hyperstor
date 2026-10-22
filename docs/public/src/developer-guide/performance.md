# Performance

These are the current key figures, from `hyperstor-bench` (see
[Measuring performance](../admin-guide/benchmarking.md)) and the timing
tests. They are development measurements, not a product benchmark: every
cluster figure runs all nodes on **one host over loopback TCP**, and no
RDMA, NVMe passthrough or hard disk has been measured yet.

## Hosts

| Label | Hardware | Used for |
|---|---|---|
| Host A | One AMD EPYC 9124 (16 cores), one consumer NVMe SSD without power-loss protection (a 4 KiB write and flush takes about 160 µs), ext4, Linux 7.0 | The engine figures |
| Host B | Two AMD EPYC 7742 (128 cores), 1 TiB RAM, data-center NVMe drives, ext4 on software RAID, Linux 6.8; shared with other work | The cluster and failure figures |

Never compare absolute numbers across the two hosts.

## The engine alone (host A)

One shard, one thread, a 2 GiB file with `O_DIRECT`:

```sh
HS_BENCH_SECS=3 hyperstor-bench raw    /path/dev.img 2
HS_BENCH_SECS=3 hyperstor-bench engine /path/dev.img 2   # HS_DEFER_MAX=0: redirect every write
```

| Test | Raw device | Engine, redirect-on-write | Engine, deferring writes < 64 KiB |
|---|---|---|---|
| 4 KiB write and flush, one at a time | 6,020 /s, p50 162 µs | 4,713 /s, p50 196 µs | 5,581 /s, p50 168 µs |
| 4 KiB writes, batch 32 | 117,015 /s | 86,722 /s | 64,093 /s |
| 4 KiB writes, batch 128 | 160,154 /s | 132,704 /s | 84,725 /s |
| 4 KiB read, QD1 | 18,151 /s, p50 53 µs | 14,210 /s | 14,714 /s, p50 64 µs |

| CPU-only figure | Value |
|---|---|
| A 4 KiB write through the engine on a memory device | about 5 µs (135,000 /s one at a time, 195,000 /s at batch 32) |
| A 4 KiB read through the engine on a memory device | 762,000 /s |
| CRC32C | 6.75 GB/s on one core |
| Placement of one PG, 1,024 devices, 3 copies | 2.2 µs by host, 1.3 µs by rack |
| Log replay | about 270 MiB/s; open takes about 0.3 s plus 4.5 ms per MiB replayed |
| Log space per update, batch 32 | 128 B (257 B with checkpoints) |
| Commit p99 during fuzzy checkpoints | 109 µs (whole checkpoints: up to 3.6 ms) |
| Index memory after random overwrites | 2 GiB per TiB with dense maps, against 33 GiB with run maps |

## A replicated cluster (host B)

Three storage nodes, one reactor each, three copies, in one process:

```sh
HS_BENCH_SECS=3 hyperstor-bench cluster mem 1            # memory devices: CPU and network cost
HS_BENCH_SECS=3 hyperstor-bench cluster <dir> 1          # 1 GiB O_DIRECT files
```

| Test | Memory devices | p99 | `O_DIRECT` files |
|---|---|---|---|
| 4 KiB write, QD1 | 7,400–7,600 /s | about 154 µs | 1,988 /s (p99 581 µs) |
| 4 KiB write, QD32 | 25,000–27,000 /s | 2.6–2.9 ms | about 6,050 /s |
| 4 KiB read, QD1 | 18,500–19,000 /s | about 65 µs | about 3,990 /s |
| 4 KiB read, QD32 | 53,500–54,500 /s | about 1.3 ms | about 12,300 /s |
| 1 MiB write, QD4 | 510–525 /s | 12.5–13.5 ms | about 330 /s |
| 1 MiB read, QD4 | 1,480–1,530 /s | 4.2–4.8 ms | about 1,220 /s |

These have held within about 2 % across the changes of the last
milestones; that is the noise between runs on a shared host.

## Failures and recovery (host B)

| Event | Figure |
|---|---|
| A storage node that is primary dies (0.6 s leases, 0.8 s detection, peer reports) | Longest I/O gap 0.66 s; 0.55 s at 0.5 s / 0.7 s; 3.1 s with the first 2 s / 3 s timings and no peer reports |
| The map service's Raft leader dies (0.4 s leases) | Longest write gap 0.37–0.46 s; about 0.7 s expected at the 0.6 s default |
| A node returns after 1 % of a 256 MiB volume changed | 2.0 MiB sent instead of 192 MiB (96× less), clean in 0.14–0.18 s |
| … after 10 % changed | 16 MiB (12× less) |
| Digest listing | 0.45–0.63 µs per one-block object; 19–50 µs per 4 MiB chunk |
| Deep scrub running continuously (a pass every 3.5 s, as a stress test) | 4–6 % lower throughput; QD32 read p99 +45 % |
| Forced takeover of a volume | 3.99–4.01 s (one volume lease); preemption 3.76–3.79 s |

## Findings that changed the design

| Finding | What changed |
|---|---|
| Deferring small writes cost about a third of batched write throughput on flash; end to end, redirecting gave 19 % more throughput and 2.6× lower p99 | Redirect-on-write on flash; deferral only for hard disks |
| Run maps grow to 33 GiB per TiB after random overwrites | Dense block maps |
| Writing a whole checkpoint at once stalled commits for 8–13 ms | Fuzzy checkpoints in 64 KiB slices |
| A copy-on-write B+tree committed per write costs 30–100× the metadata writes of the log | The planned B+tree keeps the log as its write-ahead log |
| Failover took the detection timeout plus about 110 ms | Shorter leases and peer reports: about 1 s |
| Polling 200 µs after each request cut QD1 latency 15–23 % and a four-reactor p99 by 5× | Reactors poll by default |
| Digest recovery sends 96× less than a full refill at 1 % changed, and digest lists are cheap | No PG log |
| One io_uring timeout per write cost about 3 % of write throughput | Per-request deadlines on one 50 ms ticker per reactor |
| With four shared CPUs, cluster API load doubled QD1 read p99 | Control-path threads get cores of their own |

## Regressions caught

Every change that touches the I/O path is measured against `main`,
alternating the two builds twice.

| Regression | Effect | Fix |
|---|---|---|
| Engine statistics walked every dense block map after each commit | QD1 write p50 353 µs instead of 101; QD32 at a quarter of its throughput | Statistics split from the per-commit path |
| Placement rebuilt the domain list for every PG | 47 µs per PG | 2.2 µs per PG |
| A timer per write | −3 % write throughput | The coarse ticker |

## Not measured yet

- io_uring against SPDK on the same NVMe drive.
- RDMA, NVMe passthrough, and hard disks with a flash log.
- Clusters across real hosts and networks, and false failure detection
  under load.
- Digest scans on PGs of hundreds of GiB.
