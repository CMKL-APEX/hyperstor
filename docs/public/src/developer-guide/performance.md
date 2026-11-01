# Performance

These are the current key figures, from `hyperstor-bench` (see
[Measuring performance](../admin-guide/benchmarking.md)) and the timing
tests. They are development measurements, not a product benchmark. Most
cluster figures run every node on **one host over loopback TCP**; the
figures on [three hosts](#a-cluster-on-three-hosts-host-c) run over
1 GbE with devices that are files on a filesystem. No RDMA, NVMe
passthrough or hard disk has been measured yet, nor a VM disk through
`hyperstor-vhost`.

## Hosts

| Label | Hardware | Used for |
|---|---|---|
| Host A | One AMD EPYC 9124 (16 cores), one consumer NVMe SSD without power-loss protection (a 4 KiB write and flush takes about 160 µs), ext4, Linux 7.0 | The engine figures |
| Host B | Two AMD EPYC 7742 (128 cores), 1 TiB RAM, data-center NVMe drives, ext4 on software RAID, Linux 6.8; shared with other work | The cluster and failure figures on one host |
| Host C | Three hosts: host B, a second two-socket server with data-center NVMe, and host A, linked by 1 GbE (MTU 1500; host A routed to the others). Each host runs a map service member and a storage node of two 64 GiB device files on ext4; peer timeout 300 ms | The figures across hosts |

Never compare absolute numbers across hosts.

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

### Paged checkpoints (host B)

2 GiB of 4 MiB chunks on a memory device, aged by a full pass of random
4 KiB overwrites, then overwrites to a share of the chunks
(`hyperstor-bench paged`). Checkpoint traffic is scaled to a 1 TiB shard
taking 50,000 writes a second:

| Chunks written | Whole checkpoints | Paged, 128 KiB pages |
|---|---|---|
| All | 207 MB/s | 211 MB/s |
| A tenth | 207 MB/s | 86 MB/s |
| A hundredth | 207 MB/s | 13 MB/s |

- Uniform random writes gain nothing and pay about 2 % for page headers.
- A volume's chunks spread over every PG, so a hot tenth of the chunks
  still touches 40 % of the pages. Smaller pages helped little until
  16 KiB, which cost 24 % more for uniform writes; 128 KiB stays the
  default.
- Opening a shard (replaying its log) takes as long as before.

## A replicated cluster on one host (host B)

Three storage nodes, one reactor each, three copies, in one process:

```sh
HS_BENCH_SECS=3 hyperstor-bench cluster mem 1            # memory devices: CPU and network cost
HS_BENCH_SECS=3 hyperstor-bench cluster <dir> 1          # 1 GiB O_DIRECT files
```

| Test | Memory devices | p99 | `O_DIRECT` files |
|---|---|---|---|
| 4 KiB write, QD1 | 7,300–7,600 /s | about 157 µs | 1,988 /s (p99 581 µs) |
| 4 KiB write, QD32 | 25,000–27,000 /s | 2.6–2.9 ms | about 6,050 /s |
| 4 KiB read, QD1 | 18,200–19,000 /s | about 66 µs | about 3,990 /s |
| 4 KiB read, QD32 | 53,400–54,500 /s | about 1.3 ms | about 12,300 /s |
| 1 MiB write, QD4 | 505–525 /s | 12.3–13.5 ms | about 330 /s |
| 1 MiB read, QD4 | 1,480–1,530 /s | 4.2–5.7 ms | about 1,220 /s |

These have held within about 2 % across the changes of the last
milestones, which is the noise between runs on a shared host. The
memory-device figures were last checked when every request started to
carry its lease identity: no measurable change.

## A cluster on three hosts (host C)

`hyperstor-bench remote` against the map service, into eight test
volumes, 10 s per test. The first run is from one of the two-socket
hosts, after commits moved off the reactor; the second from host A, a
day later, with probes running:

| Test | From a two-socket host: ops/s, p50, p99 | From host A: ops/s, p50, p99 |
|---|---|---|
| 4 KiB random writes, QD1 | 943, 1.09 ms, 1.38 ms | 880, 1.12 ms, 1.72 ms |
| 4 KiB random writes, QD32 | 15,008, 2.13 ms, 3.08 ms | 12,485, 2.49 ms, 4.77 ms |
| 4 KiB random reads, QD1 | 2,372, 0.40 ms, 0.63 ms | 1,927, 0.50 ms, 1.20 ms |
| 4 KiB random reads, QD32 | 22,629, 0.63 ms, 5.47 ms | 4,254, 7.83 ms, 9.32 ms |
| 1 MiB writes, QD4 | 63 (66 MB/s), 55 ms, 256 ms | 56 (59 MB/s), 70 ms, 129 ms |
| 1 MiB reads, QD4 | 106 (111 MB/s), 21 ms, 273 ms | 91 (96 MB/s), 42 ms, 85 ms |

- In the second run, 1 MiB reads held the reactor of host A's consumer
  NVMe device for 100–153 ms at a time, which is why reads now wait for
  the device off the reactor. Those figures predate that change.
- With commits off the reactor, no node logged a reactor hold, a slow
  commit or a device wait of 100 ms or more at 15,000 writes a second.
- Under 1 MiB load the network dropped a few packets in a thousand, with
  round trips under 10 ms. Each lost TCP ping cost a 200 ms
  retransmission timeout, the cause of the occasional 300–400 ms
  silences that peers reported; pings now go over UDP.

## A volume as a block device (first look, host B)

A `hyperstor-ublk` device on an all-in-one trial cluster (three nodes,
each a 1 GiB file on a shared development host's root filesystem,
flushed at every commit), a 512 MiB volume filled first, `--depth 64`,
fio 3.36 with `libaio` and direct I/O, 10 s per job:

```sh
fio --name=t --filename=/dev/ublkb0 --rw=randread --bs=4k --iodepth=1 \
    --ioengine=libaio --direct=1 --time_based --runtime=10 --size=512M
```

| Job | IOPS | p50 | p99 |
|---|---|---|---|
| 4 KiB random read, QD1 | 3,391 | 276 µs | 371 µs |
| 4 KiB random read, QD32 | 11,442 | 2.2 ms | 7.0 ms |
| 4 KiB random write, QD1 | 748 | 1.3 ms | 1.7 ms |
| 4 KiB random write, QD32 | 3,544 | 7.2 ms | 33.8 ms |
| 1 MiB write, QD4 | 285 (285 MiB/s) | 13.3 ms | 25.8 ms |
| 1 MiB read, QD4 | 789 (789 MiB/s) | 4.6 ms | 7.9 ms |

These are that cluster's numbers, not ublk's: writes are bound by the
files' flushes on a shared disk (the same client path to memory devices
does about 7,400 4 KiB writes a second at QD1), and the nodes and the
client share the host's cores. ublk runs one queue and copies data
through the client for now. What it costs, and what a VM disk through
`hyperstor-vhost` costs, needs a host with NVMe-backed nodes.

## Failures and recovery

| Event | Host | Figure |
|---|---|---|
| A storage node that is primary dies (0.6 s leases, 0.8 s detection, peer reports) | B | Longest I/O gap 0.66 s; 0.55 s at 0.5 s / 0.7 s; 3.1 s with the first 2 s / 3 s timings and no peer reports |
| The map service's Raft leader dies (0.4 s leases) | B | Longest write gap 0.37–0.46 s; about 0.7 s expected at the 0.6 s default |
| A node returns after 1 % of a 256 MiB volume changed | B | 2.0 MiB sent instead of 192 MiB (96× less), clean in 0.14–0.18 s |
| … after 10 % changed | B | 16 MiB (12× less) |
| A node stopped for 90 s under writes, then started | C | Marked down 0.44 s after it stopped; writes went on with two copies; its devices opened in about 1 s, and every PG was clean about 8 s after it was started |
| An emptied node refilled (384 MiB of 1 GiB devices) | B | Healthy in 10.2–10.9 s with no node reported; before the backfill budget, it flapped and wasn't healthy within 3 minutes |
| A 3.84 TB NVMe drive added to a running node | C | Took 95–98 % of its host's copies, as its weight says; 8.8 GiB backfilled at about 95 MiB/s, healthy 94 s after the install, no node reported |
| A device whose writes and flushes each take 2 s (`dm-delay`) | B | Commits waited about 4 s without holding the reactor; the primaries dropped the device from its PGs as too slow |
| A device that stops answering (60 s delay) | B | Failed after the 30 s I/O timeout; its PGs placed elsewhere and its node back up |
| Digest listing | B | 0.45–0.63 µs per one-block object; 19–50 µs per 4 MiB chunk |
| Digest of a 215 MB key–value table (660,234 entries) | B | 216 parts of 1 MiB, the longest 1.5 ms, instead of 290 ms at once |
| Deep scrub running continuously (a pass every 3.5 s, as a stress test) | B | 4–6 % lower throughput; QD32 read p99 +45 % |
| Forced takeover of a volume | B | 3.99–4.01 s (one volume lease); preemption 3.76–3.79 s |

## Findings that changed the design

| Finding | What changed |
|---|---|
| Deferring small writes cost about a third of batched write throughput on flash; end to end, redirecting gave 19 % more throughput and 2.6× lower p99 | Redirect-on-write on flash; deferral only for hard disks |
| Run maps grow to 33 GiB per TiB after random overwrites | Dense block maps |
| Writing a whole checkpoint at once stalled commits for 8–13 ms | Fuzzy checkpoints in 64 KiB slices |
| Every checkpoint rewrote the whole index, however little changed | Paged checkpoints: 16× less traffic when a hundredth of the chunks change |
| A copy-on-write B+tree committed per write costs 30–100× the metadata writes of the log | The planned B+tree keeps the log as its write-ahead log |
| Failover took the detection timeout plus about 110 ms | Shorter leases and peer reports: about 1 s |
| Polling 200 µs after each request cut QD1 latency 15–23 % and a four-reactor p99 by 5× | Reactors poll by default |
| Digest recovery sends 96× less than a full refill at 1 % changed, and digest lists are cheap | No PG log |
| One io_uring timeout per write cost about 3 % of write throughput | Per-request deadlines on one 50 ms ticker per reactor |
| With four shared CPUs, cluster API load doubled QD1 read p99 | Control-path threads get cores of their own |
| A node whose reactor stalled past the peer timeout went down and up about 150 times a minute | Flap damping: a node is held down 2 s, doubling up to 60 s |
| Pings queued behind backfill pushes on the one connection to a peer, and an empty node's refill flapped the cluster | A 16 MiB backfill budget per target, and pings apart from data |
| A large key–value table held a reactor for 290 ms while digested, and opening its device took 1.4–4.4 s | Digests in 1 MiB parts; the index copied at open only when there is a log to replay (1.4 s to 0.67 s) |
| Flushes of 105–212 ms on a shared filesystem held a reactor; under 1 MiB reads, so did reads | The engine waits for the device off the reactor, with I/O and read timeouts |
| A lost TCP ping waited 200 ms for a retransmission and delayed the pings behind it | Pings over UDP |
| Carrying a lease identity on every request costs nothing measurable | Per-volume fencing on shared connections, instead of a client per volume |

## Regressions caught

Every change that touches the I/O path is measured against `main`,
alternating the two builds twice.

| Regression | Effect | Fix |
|---|---|---|
| Engine statistics walked every dense block map after each commit | QD1 write p50 353 µs instead of 101; QD32 at a quarter of its throughput | Statistics split from the per-commit path |
| Placement rebuilt the domain list for every PG | 47 µs per PG | 2.2 µs per PG |
| A timer per write | −3 % write throughput | The coarse ticker |
| Paged checkpoints first made every logged operation 80 bytes instead of 56 | Log reads and replay 12–17 % slower | Back to 56 bytes |

## Not measured yet

- io_uring against SPDK on the same NVMe drive.
- RDMA, NVMe passthrough, and hard disks with a flash log.
- VM disks through `hyperstor-vhost`, and the host read cache, on real
  hardware.
- Clusters on networks faster than 1 GbE, and false failure detection
  there.
- Client latency while a node is refilled.
- Digest scans on PGs of hundreds of GiB.
