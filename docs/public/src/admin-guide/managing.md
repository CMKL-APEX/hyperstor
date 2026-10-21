# Managing a cluster

`hyperstorctl` talks to the [cluster API](../reference/cluster-api.md).
On a host set up with `hyperstor-install` it finds the API by itself;
elsewhere, give `--api IP:PORT` or set `HYPERSTOR_API`.

## Health

```sh
hyperstorctl status
```

```text
cluster 7311402, map epoch 4
health  warn
  warn  1 node(s) down: 2
  warn  pool vol: 64 PG(s) degraded
nodes   2 up of 3; devices 3 in of 3
POOL NAME         KIND       SIZE MIN   PGS  CHUNK  CLEAN DEGRADED  BACKFILL INACTIVE
1    vol          replicated    3   2    64  4096K      0       64         0        0
```

| Health | Means | Exit status |
|---|---|---|
| `ok` | Every PG has all its copies | 0 |
| `warn` | Something is down, out, degraded or backfilling; data is still writable | 1 |
| `error` | Some PG has fewer copies than the pool's minimum, or no primary: writes to it stop | 1 |

The exit status makes `hyperstorctl status` usable in scripts and
monitoring checks; `--json` gives the whole answer. `hyperstorctl status
--wait 60` waits up to a minute for health to become `ok`, for example
right after an install or bringing a device back in.

`hyperstorctl nodes`, `pools` and `pgs --pool 1` show the details.

## Taking a device out for service

```sh
hyperstorctl out --node 2      # or --device D
hyperstorctl status            # PGs move; wait for them
...                            # service the node
hyperstorctl in --node 2
```

A device that is **out** gets no placement: its PGs go to other devices,
which are refilled from the remaining copies. With as many storage nodes
as copies, there is nowhere to move them, so the PGs run with one copy
fewer (`degraded`) until the device is back **in**. Writes go on while
each PG keeps at least the pool's `min` copies.

## Testing I/O

```sh
hyperstorctl io-test --mib 64
```

writes 64 MiB of random data to a new volume of the first pool (or
`--pool P`: `--pool 2` tests the installer's cache pool), reads it back
and compares, and prints the throughput of
each phase. A mismatch or an error exits with status 1. It is a quick
end-to-end check; for measurements use
[`hyperstor-bench`](benchmarking.md).

## Metrics

Each storage node started with `--admin ADDR` (the installer uses
`127.0.0.1:8853` for node 0, `8854` for node 1, and so on) serves
metrics for Prometheus:

```sh
curl -s http://127.0.0.1:8853/metrics | grep requests_total
```

```text
hyperstor_requests_total{node="0",device="0",kind="write"} 1024
hyperstor_requests_total{node="0",device="0",kind="read"} 1024
hyperstor_requests_total{node="0",device="0",kind="repl"} 2048
hyperstor_requests_total{node="0",device="0",kind="push"} 0
```

| Metric | Meaning |
|---|---|
| `hyperstor_requests_total{kind}` | Requests served: `write` and `read` from clients (as primary), `repl` from primaries (as replica), `push` during backfill |
| `hyperstor_request_bytes_total{kind}` | Data bytes in those requests |
| `hyperstor_request_duration_seconds{kind}` | Histogram of client writes (until every replica has answered) and reads, from arrival to answer |
| `hyperstor_write_errors_total` | Client writes answered with an error |
| `hyperstor_device_objects` | Objects on the device |
| `hyperstor_device_free_bytes`, `hyperstor_device_data_bytes` | Free space and data area |
| `hyperstor_log_used_bytes`, `hyperstor_log_bytes` | Log in use since the last checkpoint, and its size |
| `hyperstor_commits_total`, `hyperstor_checkpoints_total` | Engine commit rounds and checkpoints |
| `hyperstor_map_epoch` | The cluster map epoch the node holds; nodes behind the others haven't heard of a change |

Gauges are updated once a second. `GET /health` answers `ok` while the
process runs. The port has no authentication: it only reads, but keep
it on the management network.

