# Managing a cluster

`hyperstorctl` talks to the [cluster API](../reference/cluster-api.md).
On a host set up with `hyperstor-install` it finds the API by itself;
elsewhere, give `--api IP:PORT` or set `HYPERSTOR_API`.

## Dashboard

The installer runs a read-only dashboard on the map service's host, at
<http://127.0.0.1:8852/>: health with its reasons, I/O rates, space,
pools with their PG states (and each PG's acting and up sets), and every
storage node with its state, last heartbeat, I/O, write and read p99
latency, space, log use and map epoch. It refreshes every two seconds.

People can't log in to it yet, so it listens on loopback only. From
another machine:

```sh
ssh -L 8852:127.0.0.1:8852 the-map-service-host
# then open http://127.0.0.1:8852/
```

Changes still go through `hyperstorctl`.

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
hyperstorctl out --node 2 --wait 600   # or --device D; waits for the PGs to move
...                                    # service the node
hyperstorctl in --node 2 --wait 600    # and for them to come back
```

A device that is **out** gets no placement: its PGs go to other devices,
which are refilled from the remaining copies. With as many storage nodes
as copies, there is nowhere to move them, so the PGs run with one copy
fewer (`degraded`) until the device is back **in**. Writes go on while
each PG keeps at least the pool's `min` copies.

> **Out doesn't survive a map service restart.** The map service reads
> the cluster file when it starts and doesn't keep changes made since,
> so after a restart every device is in again. Bring devices back in
> before restarting it, or expect their PGs to move back.

## Draining a node

`hyperstorctl drain --node N` takes every device of node N out and
follows its PGs as an operation, until none lives on the node and none
is still moving: then the node can be switched off or serviced.

```sh
hyperstorctl drain --node 3 --dry-run   # what would move; nothing changes
hyperstorctl drain --node 3 --wait 600  # drain and follow it to the end
hyperstorctl op list                    # running and recent operations
hyperstorctl op watch --id ID           # follow one
hyperstorctl op cancel --id ID          # stop; the devices come back in
hyperstorctl in --node 3                # after the service
```

`drain` prints each PG that moves, with its acting set before and where
it goes. Operations are kept in the map service's memory, the last 100;
`op watch` exits with status 1 if the operation failed or was
cancelled, for scripts.

## Revoking a certificate

A certificate that leaked, or a host that is gone, is revoked by serial:

```sh
hyperstorctl cert list                       # certificates issued through the API
hyperstorctl cert show --file /path/node.crt # a certificate file's serial
hyperstorctl cert revoke --serial SERIAL
```

Every map service member then refuses it, on the cluster API and
between members, until it would have expired anyway (30 days at most).
The local socket on a map service host needs no certificate, so
revoking the administrator's own never locks root out.

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
| `hyperstor_requests_total{kind}` | Requests served: `write` and `read` from clients (as primary), `repl` from primaries (as replica), `push` of whole objects from primaries during backfill, peering and scrub repair |
| `hyperstor_request_bytes_total{kind}` | Data bytes in those requests |
| `hyperstor_request_duration_seconds{kind}` | Histogram of client writes (until every replica has answered) and reads, from arrival to answer |
| `hyperstor_write_errors_total` | Client writes answered with an error |
| `hyperstor_scrub_objects_total{kind}` | Scrub on the PGs a device leads: objects `compared`, those whose copies differed (`inconsistent`), then `repaired` or `unresolved` (no majority: look at the log). `damaged`: objects of this device whose deep scrub found a bad checksum. `peering`: objects a new primary pushed before it served (see [Failures and recovery](failures.md#scrub)) |
| `hyperstor_device_objects` | Objects on the device |
| `hyperstor_device_free_bytes`, `hyperstor_device_data_bytes` | Free space and data area |
| `hyperstor_log_used_bytes`, `hyperstor_log_bytes` | Log in use since the last checkpoint, and its size |
| `hyperstor_commits_total`, `hyperstor_checkpoints_total` | Engine commit rounds and checkpoints |
| `hyperstor_map_epoch` | The cluster map epoch the node holds; nodes behind the others haven't heard of a change |

Gauges are updated once a second. `GET /health` answers `ok` while the
process runs. The port has no authentication: it only reads, but keep
it on the management network.

## Audit

The map service records every change made through the API, and every
refused call, in its journal (see [Cluster API](../reference/cluster-api.md)):

```sh
journalctl -u hyperstor-meta | grep '{"audit":1'
```

## Prometheus and Grafana

For history and alerts, scrape the storage nodes with Prometheus and
import the shipped Grafana dashboard:

- `deploy/prometheus/hyperstor.yml`: a scrape job reading its targets
  from `/etc/hyperstor/prometheus-targets.json`, which the installer
  writes on the map service's host.
- `deploy/grafana/hyperstor.json`: the dashboard (Grafana 10 or later):
  I/O rates and bytes by kind, write and read latency (p50, p99, and p99
  per device), space (orange at 85 %, red at 95 %), log use, commits and
  checkpoints, map epochs by node, and objects. Pick the Prometheus data
  source and the nodes at the top.

Space isn't part of health yet: watch it on either dashboard. Writes
fail when a device fills.

