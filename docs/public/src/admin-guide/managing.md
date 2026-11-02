# Managing a cluster

`hyperstorctl` talks to the [cluster API](../reference/cluster-api.md).
On a host set up with `hyperstor-install` it finds the API by itself;
elsewhere, give `--api IP:PORT` or set `HYPERSTOR_API`.

## Dashboard

The installer runs the management service (`hyperstor-mgmt`) on the map
service's hosts, on port 8852. Installed with `--ui` (see
[Installation](installation.md#building)), it serves the web UI: an
overview, nodes and devices (take out, bring in, drain, weights), pools
and a grid of their PGs, volumes with their snapshots and clones, the
trash, operations, the map service's members, security (people and
roles, API tokens, join tokens, certificates) and the audit log. Each
action acts as the person logged in, and shows its `hyperstorctl`
command. Without `--ui`, a built-in dashboard only shows: health with
its reasons, I/O rates, space, pools with their PG states (and each PG's
acting and up sets), and every storage node with its state, last
heartbeat, I/O, write and read p99 latency, space, log use and map
epoch. Both refresh every two seconds.

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

## Logins and roles

People log in with their user name and password on the map service's
host, checked by `hyperstor-authd` through PAM; they must be in the
group `hyperstor-users`:

```sh
sudo usermod -aG hyperstor-users ann
```

`hyperstor-mgmt` can also log people in with an OIDC identity provider
(`--oidc FILE`, see
[hyperstor-mgmt](../reference/hyperstor-mgmt.md#identity-providers));
the installer doesn't set that up yet.

The first to log in with the **claim code** becomes the first security
administrator, who then gives people roles. The code is in the
service's log, valid for an hour:

```sh
journalctl -u hyperstor-mgmt | grep 'claim code'
# a new one: sudo systemctl restart hyperstor-mgmt
```

Someone who logs in without a role sees nothing until given one. Each
role adds to the one before, except the last:

| Role | May |
|---|---|
| `viewer` | Read: status, PGs, volumes, snapshots, the trash, operations, members |
| `operator` | Also take devices out and in, add and remove devices, set weights, classes and fullness thresholds, drain nodes, cancel operations, take snapshots, restore from the trash |
| `storage-admin` | Also create, change and delete pools, volumes, snapshots and clones, roll volumes back, purge the trash and set its grace, add and remove members |
| `security-admin` | People and their roles, join tokens, certificates, map backups and the audit log; nothing else |

The exact list is in [hyperstor-mgmt](../reference/hyperstor-mgmt.md#logins).
Deleting, rolling back, purging, removing a device, making tokens,
revoking certificates, fetching a backup and changing roles need a
login of the last 10 minutes (step-up), since they are hard to undo:
the UI asks you to log in again. Sessions last 12 hours, or 30 minutes
idle.

**API tokens.** For scripts, each person makes API tokens for
themselves on the UI's Security page (90 days by default, a year at
most). A token acts with its owner's roles, but never counts as a fresh
login. `hyperstorctl` uses one through mgmt with `--mgmt`, so the map
service's audit names the person rather than a certificate:

```sh
# the token in a file only you can read
hyperstorctl --mgmt https://192.0.2.10:8852 --token-file ~/.hyperstor-token status
```

`HYPERSTOR_MGMT` can stand for `--mgmt`. What needs a fresh login is
refused that way with `(reauth_required)`: do it on the UI, or with the
administrator's certificate. See
[hyperstorctl](../reference/hyperstorctl.md).

The web UI does what this page and [Pools and devices](pools-and-devices.md)
do with `hyperstorctl`, each
action on the page of what it changes, showing the command it stands
for: on **Nodes**, adding a device, setting its weight or class, taking
it out or in, removing it (its plan first) and draining a node; on
**Pools**, creating a pool, changing it, choosing the devices it uses and
deleting it (what it holds listed first); on **Volumes**, creating,
expanding, snapshotting, cloning, flattening and deleting volumes. It
shows only what your roles allow, and asks you to log in again for what
needs a fresh login.

## The trash

Deleting a volume or a snapshot moves it to the trash: it disappears from
the lists and can't be attached, but its data stays, and its space still
counts against the pool, for the pool's grace period (a day by default).
Then the map service purges it.

```sh
hyperstorctl delete-volume --id ID        # → the trash, purged in a day
hyperstorctl trash list
hyperstorctl trash restore --id ID        # back, under its name
hyperstorctl trash purge --id ID --wait 600   # gone now (a fresh login, through mgmt)
hyperstorctl trash settings --pool 1 --grace 604800   # keep a week
```

A restored volume whose name another volume took meanwhile comes back
as `NAME-restored`. `--on-full purge` lets a full pool purge its oldest
trash rather than refuse new data: while a pool is past `full`
([Full devices](pools-and-devices.md#full-devices)), the leader purges its trash oldest
first, one item at a time, and waits for the devices' next reports
before the next, so it stops once the pool is no longer full. Each
purge is an operation of `map-service/purger`.
The default, `keep`, never loses data an operator may still want; the
pool stays full until space is freed. The UI's Trash page does the
same.

## The audit log

Every change, every refused call and every break-glass call is recorded:
when, who (a certificate's identity, `local:uid=N` on the socket, or a
person `user:NAME` through `hyperstor-mgmt`), what, on what, and how it
went. Each record goes to the service's journal at once, and to the
cluster's audit log in `.meta` within a second, kept a year by default:

```sh
hyperstorctl audit list --since 1d --op DeleteVolume
hyperstorctl audit list --principal user:ann --json
hyperstorctl audit settings --retention-days 730
journalctl -u hyperstor-meta -u hyperstor-mgmt | grep '"audit":1'
```

Security administrators read it on the UI's Audit page, and can export
it there as JSON lines.

A client that keeps retrying a refused call doesn't flood the log: of
one principal's calls refused for one reason, the first ten a minute are
recorded, and when the minute ends one `Refused` record says how many
more there were. The journal has every one:

```sh
hyperstorctl audit list --op Refused
```

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
space   171.6G free of 189.0G raw (9% used)
POOL NAME         KIND       SIZE MIN   PGS  CHUNK  CLEAN DEGRADED  BACKFILL INACTIVE
1    vol          replicated    3   2    64  4096K      0       64         0        0
```

| Health | Means | Exit status |
|---|---|---|
| `ok` | Every PG has all its copies | 0 |
| `warn` | Something is down, out, degraded or backfilling, a node flaps, a device is past `nearfull` or `backfillfull`, cache PGs wait for a down node, or scrub found PGs whose copies it couldn't repair (`inconsistent`); data is still writable | 1 |
| `error` | Some PG has fewer copies than the pool's minimum, or no primary: writes to it stop. Or a device is past `full`: it refuses new data | 1 |

The exit status makes `hyperstorctl status` usable in scripts and
monitoring checks; `--json` gives the whole answer. `hyperstorctl status
--wait 60` waits up to a minute for health to become `ok`, for example
right after an install or bringing a device back in.

Space: `hyperstorctl pools` shows each pool's usable free and total
space, after its copies or erasure coding, and `hyperstorctl nodes` each
device's raw free and total space. The web UI shows the same as bars on
the Pools and Nodes pages and under each pool on the Overview, amber
from 85 % used and red from 95 %. Devices report their space to the map
service's leader every 5 seconds; until each has, its figures show `-`.
Any member answers with them: one that doesn't lead asks the leader,
so `hyperstorctl` gives the same figures wherever it runs.

`hyperstorctl nodes`, `pools` and `pgs --pool 1` show the details;
`pgs --pool 1 --pg 17` shows one PG. `hyperstorctl pg-grid --pool 1`
prints a character per PG, 64 to a line (`.` clean, `d` degraded, `b`
backfilling, `X` inactive), as the web UI's grid shows them in colour:

```
pool 1, map epoch 412: . clean  d degraded  b backfilling  X inactive
     0  ................................................................
```

The grid costs little to watch: the UI asks every two seconds, and gets
the states again (a byte per PG) only when the cluster map has changed.

The UI also shades each cell by how much data the PG holds: the more,
the more solid its colour. Each PG's primary reports the bytes its copy
holds every 30 seconds, and the shading is its share of the pool's
fullest PG, so pale cells next to solid ones show an uneven pool. Hover over a cell for its state and approximate size. `hyperstorctl
pg-grid --usage` adds a digit per PG, `0` for the least data to `9` for
the most (`?` before the PG's primary has reported):

```
usage as a share of the fullest PG, 86204416 bytes: 0 least to 9 most  ? not reported
     0  5763648597466574836569957465486757646385674659548677366485765947
```

Only the map service's leader has the reports. `--usage` asked of
another member exits with status 3 and names the leader; the web UI
finds the leader by itself.
Click a cell to see that PG's acting and up sets.

A node that goes down is kept down for a while before it can come back:
2 s, doubled each time it goes down again within 10 minutes, up to a
minute. A node that keeps going down and up shows as `warn  node 2
flapping (3 transitions in 10 min)`: look at that node's logs and load
(`journalctl -u hyperstord@2`), since something stalls it for longer than
its peers wait. Each storage node logs:

- `device D open in N ms` when it starts. A node is marked up only once
  every device of it is open, so a slow open delays the node but doesn't
  make it flap.
- `device D: reactor held N ms: ...` when the thread serving a device is
  held 100 ms or more, and `device D: a commit of N list(s) took N ms` when a
  commit is the cause. These count in `hyperstor_reactor_stalls_total`.
  The rest of the line says where the time went: the thread on a CPU,
  waiting for one (the host was running something else), or off one
  (asleep, or blocked in the kernel); then the longest single task poll,
  with the source line that started the task, and the longest call into
  io_uring. A long poll points at that task; a long wait for a CPU, or
  both of a node's devices held at once, points at the host.
- `device D: a commit of N list(s) waited N ms for the device` when the
  device took 100 ms or more to write and flush a commit. The thread goes
  on serving meanwhile, so this alone doesn't make a node flap; requests
  for that device's data wait.
- `device D: node P silent for N ms; reporting it` once each time it
  reports a peer to the map service.

## Pools and devices

Taking devices out and in, adding and removing them, weights, classes,
full devices, and making, splitting and deleting pools are on their own
page: [Pools and devices](pools-and-devices.md).

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
it goes. Removing a device, deleting a pool with `--force`, flattening
a clone, copying or moving a volume and purging from the trash are operations too, as is each purge
the map service makes when a grace period ends or a full pool purges
its trash. The web UI's Operations page shows them. Operations are kept
in the map service's replicated store, a week after they finish, and a
new leader carries on with those still running. A drain, a device
removal, a copy and a move can be cancelled. `op watch` exits with status 1 if the
operation failed or was cancelled, for scripts.

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
hyperstorctl flatten-volume --id CLONE --wait 3600  # so its parents can go
hyperstorctl snapshot delete --id ID --epoch E
```

A snapshot of an attached volume pauses its client's writes while it is
cut, at most 2 s. A snapshot that a clone or a rollback still reads from
can't be deleted (reason `in_use`): flatten or delete the clone first.
To test a volume as a block device on a host, serve it with
[`hyperstor-ublk`](../reference/hyperstor-ublk.md). All options are in
the [`hyperstorctl` reference](../reference/hyperstorctl.md).

`hyperstorctl expand-volume --id ID --size 20G` grows a volume (never
shrinks it); a client that has it attached sees the new size once it
attaches again. `write-volume` and `read-volume` copy a file into or
out of a detached volume, such as a disk image.

Deleting a volume or a snapshot moves it to [the trash](#the-trash).

### Copying and moving between pools

```sh
hyperstorctl copy-volume --id ID --epoch E --pool 3 --name db-copy --wait 3600
hyperstorctl move-volume --id ID --pool 3 --wait 3600
```

`copy-volume` makes a new volume in another pool from a volume's
snapshot (`--epoch`), while the volume stays in use, or from its current
data, which needs it detached. The copy is flat: no snapshots, no
parents. `move-volume` moves a detached volume, keeping its id, so
whatever names it (a glidex disk) needs no change; delete and purge its
snapshots first, or copy it instead. Both copy every block that holds
data through the map service's leader, and take the new pool's space
for it; a move then deletes the data in the old pool.

## VM disks

[`hyperstor-vhost`](../reference/hyperstor-vhost.md) serves volumes to
the VMs on a host over vhost-user-blk: one service per host, a socket
per volume, which Cloud Hypervisor or QEMU opens as the VM's disk.
Install it with `hyperstor-install --vhost` (see
[Installation](installation.md#hosts-that-run-vms)). glidex attaches
volumes through its control socket; by hand:

```sh
sudo hyperstor-vhost ctl attach ID --serial vm-disk   # prints the socket's path
sudo hyperstor-vhost ctl list                          # what is attached here
sudo hyperstor-vhost ctl detach ID
```

- **Disks.** The guest sees a virtio disk with 512-byte sectors on
  4 KiB physical blocks and no volatile write cache: a write is answered
  once every copy has it. Discard isn't offered yet.
- **One holder.** A volume is attached on one host at a time. Attaching
  it elsewhere is refused (`held`, naming the holder, such as
  `vhost:NAME` for another host's service) unless `--take-over` (its
  holder died: wait for its lease to run out) or `--preempt` (take it
  from a holder still running). Each attached volume has a lease of its
  own, so preempting one fences that disk alone; the old host's other
  VMs keep theirs. The old host's `ctl list` shows the volume `lost`.
- **Restarts.** Restarting the service (an upgrade, say) keeps its
  volumes attached: the hypervisors reconnect, and the guests carry on
  after a pause of a few seconds. A reboot of the host clears them.
- **Full pools.** A write refused for space is held, and tried again
  every second, for up to 5 minutes (`--hold-full`) before the guest sees an
  I/O error. See [Full devices](pools-and-devices.md#full-devices).

**For glidex.** glidex's `hyperstor` datastore driver keeps a glidex
datastore's disks as volumes in one pool: blank disks, disks filled from
an image, and linked clones of an image's `golden` snapshot, staged on
each node through `hyperstor-vhost`. It uses the cluster API as a
management client. Make it a `mgmt` token, named after the
client, and give it to the client, which enrols with it (it makes its
own key and pins the cluster's root):

```sh
(umask 077; sudo hyperstorctl token create --role mgmt --name glidex > token)
# on the client's side: hyperstorctl enroll --file token --api 192.0.2.10:8850 --out DIR
```

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
| `hyperstor_reactor_stalls_total` | Times a device's reactor was held 100 ms or more (each is logged with its length, and a slow commit with what it committed). Pings wait meanwhile, so stalls near the peer timeout (300 ms) can get a node reported silent |
| `hyperstor_read_timeouts_total` | Reads that waited for their device past the read timeout (2 s, `hyperstord --read-timeout-ms`), by `kind`: `abandoned`, and `from_member` for those another copy answered. Three in a row fail the device |
| `hyperstor_scrub_objects_total{kind}` | Scrub on the PGs a device leads: objects `compared`, those whose copies differed (`inconsistent`), then `repaired` or `unresolved` (no majority: look at the log). `damaged`: objects of this device whose deep scrub found a bad checksum. `peering`: objects a new primary pushed before it served (see [Failures and recovery](failures.md#scrub)) |
| `hyperstor_device_objects` | Objects on the device |
| `hyperstor_device_free_bytes`, `hyperstor_device_data_bytes` | Free space and data area |
| `hyperstor_log_used_bytes`, `hyperstor_log_bytes` | Log in use since the last checkpoint, and its size |
| `hyperstor_commits_total`, `hyperstor_checkpoints_total` | Engine commit rounds and checkpoints |
| `hyperstor_map_epoch` | The cluster map epoch the node holds; nodes behind the others haven't heard of a change |

Gauges are updated once a second. `GET /health` answers `ok` while the
process runs. The port has no authentication: it only reads, but keep
it on the management network.

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

Health warns when a device passes `nearfull` (see
[Full devices](pools-and-devices.md#full-devices)); for history and alerts on space, use the
Grafana dashboard.

