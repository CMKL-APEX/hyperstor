# Managing a cluster

`hyperstorctl` talks to the [cluster API](../reference/cluster-api.md).
On a host set up with `hyperstor-install` it finds the API by itself;
elsewhere, give `--api IP:PORT` or set `HYPERSTOR_API`.

## Dashboard

The installer runs the management service (`hyperstor-mgmt`) on the map
service's host, at <http://127.0.0.1:8852/>. Installed with `--ui` (see
[hyperstor-install](../reference/hyperstor-install.md)), it serves the web
UI: an overview, nodes and devices (take out, bring in, drain), pools and
PGs, volumes with their snapshots and clones, operations, the map
service's members, and security (people and roles, API tokens, join
tokens, certificates); each action shows its `hyperstorctl` command.
Without it, a built-in dashboard shows health with its reasons, I/O rates, space,
pools with their PG states (and each PG's acting and up sets), and every
storage node with its state, last heartbeat, I/O, write and read p99
latency, space, log use and map epoch. It refreshes every two seconds.

People log in with their user name and password on that host; they
must be in the group `hyperstor-users`:

```sh
sudo usermod -aG hyperstor-users ann
```

The first to log in with the **claim code** becomes the first security
administrator, who then gives people roles (`viewer`, `operator`,
`storage-admin`, `security-admin`; see
[hyperstor-mgmt](../reference/hyperstor-mgmt.md#logins)). The code is
in the service's log, valid for an hour:

```sh
journalctl -u hyperstor-mgmt | grep 'claim code'
# a new one: sudo systemctl restart hyperstor-mgmt
```

Someone who logs in without a role sees nothing until given one.

**Reaching it.** mgmt listens on every address of the map service's host,
over HTTPS: `https://HOST:8852/`. Its certificate comes from the
cluster's own CA, so a browser warns until you import the cluster's root
once (`/etc/hyperstor/pki/root.crt`, as a trusted authority for
websites). To avoid that, give it a certificate browsers already trust:

- your own, kept current by an ACME client:
  `sudo hyperstor-install install --web-tls /etc/hyperstor/web` (see
  [hyperstor-mgmt](../reference/hyperstor-mgmt.md#certificates));
- on Tailscale, the node's own:
  `sudo hyperstor-install install --tailscale-cert`, then
  `https://NODE.TAILNET.ts.net:8852/`.

**Air-gapped sites.** Nothing here needs the internet: the cluster's CA
and mgmt's certificate are made on the map service's host, and renewed
there. To have browsers trust it without a warning, either distribute the
cluster's root (`/etc/hyperstor/pki/root.crt`) to the site's machines once,
with whatever configuration management the site uses, after which every
hyperstor host verifies; or issue mgmt a certificate from the site's own
internal CA and give it with `--web-tls DIR` (mgmt re-reads the files when
they are renewed). mgmt serves no self-signed certificate.

To keep it on loopback instead, `--mgmt-listen loopback`, and reach it
through a tunnel:

```sh
ssh -L 8852:127.0.0.1:8852 the-map-service-host
# then open http://127.0.0.1:8852/
```

The dashboard only shows; changes go through `hyperstorctl`, or the
cluster API through `hyperstor-mgmt` (`POST /api/hyperstor.v1.…`) as the
person logged in, with an API token for scripts.

## The trash

Deleting a volume or a snapshot moves it to the trash: it disappears from
the lists and can't be attached, but its data stays, and its space still
counts against the pool, for the pool's grace period (a day by default).
Then the map service purges it.

```sh
hyperstorctl delete-volume --id ID        # → the trash, purged in a day
hyperstorctl trash list
hyperstorctl trash restore --id ID        # back, under its name
hyperstorctl trash purge --id ID          # gone now (a fresh login, through mgmt)
hyperstorctl trash settings --pool 1 --grace 604800   # keep a week
```

A restored volume whose name another volume took meanwhile comes back
as `NAME-restored`. `--on-full purge` lets a full pool purge its oldest
trash first rather than refuse writes; it acts once pools have fullness
thresholds. The UI's Trash page does the same.

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
| `warn` | Something is down, out, degraded or backfilling, or scrub found PGs whose copies it couldn't repair (`inconsistent`); data is still writable | 1 |
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

> **Without `--data`, out doesn't survive a map service restart.** A
> map service without a store reads the cluster file when it starts and
> doesn't keep changes made since, so after a restart every device is in
> again. With `--data` (every install does this) the map, out devices
> included, is kept.

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

## Volumes and snapshots

```sh
hyperstorctl create-volume --size 10G --name db      # prints its id
hyperstorctl volumes --pool 1
hyperstorctl snapshot create --id ID --name nightly
hyperstorctl snapshot list --id ID
hyperstorctl snapshot group --ids ID1,ID2 --name db  # one instant for both
hyperstorctl clone-volume --id ID --epoch E --name test-copy
hyperstorctl rollback-volume --id ID --epoch E       # the volume must be detached
hyperstorctl flatten-volume --id CLONE               # so its parents can go
hyperstorctl snapshot delete --id ID --epoch E
```

A snapshot of an attached volume pauses its client's writes while it is
cut, at most 2 s. A snapshot that a clone or a rollback still reads from
can't be deleted (reason `in_use`): flatten or delete the clone first.
To test a volume as a block device on a host, serve it with
[`hyperstor-ublk`](../reference/hyperstor-ublk.md). All options are in
the [`hyperstorctl` reference](../reference/hyperstorctl.md).

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

