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
| `mem` | The engine on memory: its CPU cost only |
| `crc` | CRC32C checksum throughput |
| `placement` | How fast data placement is computed, for a cluster of 1,024 devices |
| `cluster mem\|DIR [REACTORS]` | A whole cluster in one process, on memory or on files in `DIR`, with `REACTORS` devices per node (default 1) |
| `remote ADDR` | A running cluster, as a client of the map service at `ADDR`; writes test data to pool 1 |
| `index FILE` | Memory and checkpoint cost of the engine's index; needs 2 GiB free |

| Environment variable | Default | Meaning |
|---|---|---|
| `HS_BENCH_SECS` | 3 | Seconds per test |
| `HS_DEFER_MAX` | | Override the size up to which the engine defers overwrites through its log, in bytes (`engine` and `mem`) |

Output is Markdown table rows. Files given to `raw`, `engine` and
`index` are overwritten.
