# hyperstor-bench

The load generator. See [Measuring performance](../admin-guide/benchmarking.md).

```text
usage: hyperstor-bench engine|raw <file> [size_gib] | mem | crc
```

The usage line lists the most common modes; all of them:

| Mode | Measures |
|---|---|
| `raw FILE [SIZE_GIB]` | The device alone: the engine's I/O patterns on an `O_DIRECT` file of `SIZE_GIB` GiB (default 2), preallocated |
| `engine FILE [SIZE_GIB]` | The storage engine on the same file |
| `commit FILE [SIZE_GIB]` | Commit rounds of 1, 8 and 32 blocks made durable by `fdatasync` and by `RWF_DSYNC` (FUA), with the disk's flushes per round |
| `mem` | The engine on memory: its CPU cost only |
| `crc` | CRC32C checksum throughput, and both libraries (`crc32c` and `crc-fast`) on 4 KiB blocks in and out of cache and on 1 MiB calls, with the one chosen for this CPU |
| `placement` | How fast data placement is computed, for a cluster of 1,024 devices |
| `cluster mem\|DIR [REACTORS]` | A whole cluster in one process, on memory or on files in `DIR`, with `REACTORS` devices per node (default 1) |
| `remote ADDR` | A running cluster, as a client of the map service at `ADDR`; writes test data to pool 1 |
| `index FILE` | Memory and checkpoint cost of the engine's index; needs 2 GiB free |
| `tls-certs DIR` | Writes a throwaway CA and a `node` and a `host` certificate to `DIR`, for `tls-serve` and `tls`; nothing of a cluster's |
| `tls-serve ADDR DIR` | Answers `tls`'s requests on `ADDR`, as a storage node's reactor would, until killed; `DIR` from `tls-certs` |
| `tls ADDR DIR` | The cost of encrypting the data path: 4 KiB and 1 MiB writes and reads at queue depths 1 and 16 on one connection to a `tls-serve`, in plaintext after the handshake and in kernel TLS, with each end's CPU per GB |
| `space-walk` | The walk each primary makes of its PGs every 30 s: D155's sum of blocks alone, and with each chunk's versions laid over each other for the space counts (D335), on one PG's index in memory; the whole walk and its longest slice of 4,096 objects |

| Environment variable | Default | Meaning |
|---|---|---|
| `HS_BENCH_SECS` | 3 | Seconds per test |
| `HS_BENCH_ONLY` | every test | `remote`: only the tests named, as in its output after the address (`1 MiB reads, QD4`), separated by `;` |
| `HS_DEFER_MAX` | | Override the size up to which the engine defers overwrites through its log, in bytes (`engine` and `mem`) |
| `HS_BENCH_CPUS` | see below | `cluster`, `remote` and `adminload`: the CPUs to pin to, as a list (`8-11`), the client's first and then each node's reactors |

`cluster`, `remote` and `adminload` pin each reactor to a CPU of its own:
the client's, then (`cluster`) each node's, one per device. Without
`HS_BENCH_CPUS` they take the CPUs the bench may run on (so
`taskset -c 8-15 hyperstor-bench cluster mem` keeps it there), less
CPU 0 and the CPUs a running Pylon `pylond` takes on a glidex host:
its polling engine's (`--engine-cpu`, 1 by default), which spins, and
the cores it gives VMs (`--cores`). The CPUs chosen are printed first; with
too few free, the bench stops and says so.

Output is Markdown table rows. Files given to `raw`, `engine`,
`commit` and `index` are overwritten.
