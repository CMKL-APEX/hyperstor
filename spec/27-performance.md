# Load and drives

Where a cluster's load is, what its drives say of themselves, and what to
do about either: what storage devices measure and report, what the map
service's leader keeps, the views in the web UI and `hyperstorctl`, and
the Advisor's rules. Part of the hyperstor design; sections keep their
numbers across documents, so `§N` in one document is the same section
everywhere (see [README.md](README.md)).

Status: **built** (2026-11-02, branch `feat/perf-heat`;
01-implementation-notes.md I247–I248). D218–D222 are settled with the
owner (points 181–182 of [05-decision-log.md](05-decision-log.md)). G54
and G55 are the host checks they rest on. It builds on the metrics of
15-operations.md §16 and 19-administration.md §23.3 (D58, D59), the PG
grid (D154, D155) and the device space reports (D156).

## 30. Load and drives (D218–D222)

### 30.1 Goal and scope

An operator should see where the load is (which PGs, which volumes,
which devices) and how each drive is doing (latency as the drive serves
it, temperature, wear, link), and be told what to do about what stands
out, with the numbers behind it.

| In | Out, for now |
|---|---|
| Requests per PG on its primary, with its top volumes | Requests per PG on replicas (replication follows the primary's writes) |
| The device's own time for reads and commits, and I/Os in flight | Per-request tracing |
| What sysfs says of a drive, and its NVMe SMART / health log | Vendor and OCP logs (write amplification), the error and event logs |
| 15 minutes in the leader's memory | History: Prometheus, from each node's metrics port (15-operations.md §16) |
| Findings with a command to run | Acting on them: moving primaries, moving volumes, limits |

Why: the first three answer "where is the hot spot" and "is this drive
the problem", which the per-device request counters of D58 can't tell
apart. History and automation are later steps (§30.8) once the pilot has
shown what findings are worth.

### 30.2 What a device reports (D218, D219, D220)

Every 5 s each device sends the leader one `PerfReport` (protocol kind
80), answered `Noted`. Best effort: a lost report is a lost window.

| Part | What | Why |
|---|---|---|
| PG heat | For each PG it served as primary in the window: reads, writes, and their bytes | Every client request of a PG goes to its primary (09-replication.md §8.2), so the primary sees all of it |
| Top volumes | Per PG, the 5 volumes (lineage, branch) that sent most requests, by a space-saving sketch of 10 slots | A PG holds chunks of many volumes; the sketch keeps the heavy ones in fixed space, a count at least the true one and over it by at most the smallest kept |
| Service time | For the device's reads and commits (a round of writes and its flush): count, bytes, a histogram in 14 buckets (powers of two from 16 µs to 65 ms, then slower), the max | The drive's own time, apart from the network and replication that request latency (D58) mixes in; buckets merge across windows and devices |
| In flight | The most I/Os the device had at once | With the p99, where the drive's knee is |
| Drive | Every 30 s, and when it changes: §30.3 | A new leader has it within 30 s |

**Cost.** Counting is plain additions on the reactor's own thread, with
no atomic, lock or system call: a map entry per PG served, a short scan
of 10 slots per request, two clock reads per device I/O. 02-benchmarks.md
(2026-11-02, "Load reports") has the A/B against the build without them.

### 30.3 The drive (D220, D221)

From sysfs, as `hyperstord`'s own user, on a thread of its own every 30 s
(never a reactor: sysfs and the helper may block):

| Field | From |
|---|---|
| Model, firmware | `/sys/class/block/NAME/device/{model,firmware_rev}` |
| Temperature | The controller's hwmon, `temp1_input` |
| PCIe link: width and speed, now and at most | The controller's PCI function, `{current,max}_link_{width,speed}` |
| NUMA node of the drive, and of the reactor's core | `numa_node`; the core's `node*` link |
| Volatile write cache | `queue/write_cache` |

The NVMe SMART / health log (Get Log Page 02h) needs `CAP_SYS_ADMIN`
(Linux 6.8, `nvme_cmd_allowed`), and `hyperstord` runs as `hyperstor`.
**`hyperstor-smart`** reads it for them:

- systemd's `hyperstor-smart.socket` (`/run/hyperstor-smart/smart.sock`,
  mode 0660, group `hyperstor`, `Accept=yes`) starts one short root
  process per request, with no network, a read-only system and no
  capability but `CAP_SYS_ADMIN`.
- A request is `health PATH`; it answers only for a path a storage node's
  `--shard` names on this host, and only for an NVMe namespace. It
  issues Get Log Page and Identify Controller (for the warning and
  critical temperatures), read commands, on the namespace's block
  device, which the kernel routes to its controller.

Why a helper rather than giving `hyperstord` the capability: the storage
daemon talks to the network and should hold no root power; the helper
does one read-only thing for named devices, and a process per request
holds nothing between them. Files and memory devices have no drive:
their reports carry service time only.

### 30.4 The leader's view and the API (D218)

The leader keeps, in memory only: each PG's newest window (for 20 s, then
it reads as idle), and each device's last 180 windows (15 minutes) with
its newest drive reading. A new leader starts empty and fills within a
window; nothing is replicated. Another member answers the calls below
`unavailable` (`not_leader`); `hyperstor-mgmt` moves on to the leader.

| Call | Answers |
|---|---|
| `GetPgHeat` | One pool's reads, writes and bytes per second per PG, with each PG's reporting device; with `pg`, its top volumes |
| `ListDevicePerf` | Each device's samples (requests, bytes, service time with p50, p99 and buckets, in flight) and its drive; or one device's |
| `ListTopVolumes` | Volumes by requests per second, summed over their PGs' top lists (a lower bound) |
| `ListFindings` | The Advisor's findings (§30.5) |

All four are reads (`viewer`). Volumes are named by looking up (lineage,
branch): the lineage's own volume for branch 0, else the branch's
recorded owner.

### 30.5 The Advisor (D222)

Rules over the last minute (12 windows) of every device and its newest
drive reading. Each finding has a rule, a severity, its subject, the
numbers behind it, what to do, and the command if there is one. The
Advisor changes nothing.

| Rule | Severity | When | Suggests |
|---|---|---|---|
| `slow-device` | warn | Reads' or commits' p99 is 4× the median of at least 3 devices of its kind, and over 2 ms, with 20 I/Os or more | Check the drive and its node; take it out if it stays slow |
| `hot-primary` | info | Its PGs serve 2× a fair share of the cluster's requests (at least 200/s in all, over two devices) | Move the hot volumes; moving primaries is planned (§30.8) |
| `hot-volume` | info | A volume sends a quarter of the cluster's requests (at least 200/s), and a pool on NVMe only, not its own, exists | `move-volume` to that pool (D214) |
| `drive-critical` | error | The drive's critical warning bits are set | Plan to take the device out |
| `media-errors` | warn | Media and data integrity errors above zero | Watch; scrub finds what they damaged |
| `wear` | warn | 80 % of rated endurance used, or spare within 10 points of its threshold | Replace; weight 0 meanwhile |
| `thermal` | warn | Any time at a throttle level or above the warning temperature | Airflow, heatsink |
| `pcie-link` | warn | The link is narrower or slower than the drive can do | Reseat or move the drive |
| `numa` | info | The drive hangs off another NUMA node than its reactor's core | Pin the reactor (`hyperstord --cpus`) |

The thresholds are first values, from what drive makers publish and the
pilot's first numbers, to be tuned with more (I248). A finding can be
snoozed for a day in the browser that shows it.

### 30.6 Views

| Where | What |
|---|---|
| Pools → PG grid | State (with data, D155), Reads or Writes, shaded quiet to busiest; a picked PG's rates, device and top volumes |
| Nodes | A matrix of every device's last minute (requests, MiB/s, read and commit p99, in flight, temperature, wear), sortable, each cell shaded within its column; a device's panel with its drive, charts of requests, bytes and p99, a heatmap of service time by bucket over 15 minutes, and in flight against p99 |
| Volumes | The busiest volumes now; **Move to pool**, or copy, with its command |
| Overview | Requests per second over 15 minutes, the slowest device's p99, the top 3 findings |
| Advisor | Every finding, with its numbers, what to do and its command |
| `hyperstorctl` | `pg-grid --heat read\|write [--pg N]`, `perf [--device D]`, `top-volumes [--limit N]`, `advise` |

Charts are plain SVG in the page's colours; each view polls only while
shown (every 5 s; findings every 10 s).

### 30.7 Host checks

- **G54. Reading the SMART log doesn't stall I/O.** On a drive serving a
  4 KiB random-read load at queue depth 32, read the health log every
  second for ten minutes and compare the read p99 with and without.
  Some drives serve a log page by pausing I/O; if one does, poll it less
  often or not at all for that model. Every 30 s is the default.
- **G55. Service time sees what the drive does.** On an NVMe drive
  filled to 90 % and written sustainedly, the commit p99 and the
  heatmap show garbage collection as the drive's own tools (`nvme
  smart-log`, vendor latency logs) do, and the `slow-device` rule names
  a drive slowed on purpose (a cgroup I/O limit on its node) within a
  minute.

### 30.8 Later

- **Moving a PG's primary role** to another in-sync copy: a map change
  with fencing, the cheapest fix for a hot device, once the
  `hot-primary` finding has shown how often it is needed.
- **Write amplification** from the OCP datacenter log (C0h) or the FDP
  statistics log, for G23 and G39.
- **Snoozes kept by the cluster**, and findings in health and the audit
  log, once their rules are trusted.
- **Per-volume QoS limits** (10-volumes.md §10.5), the fix when the hot
  spot is one noisy volume.
