# hyperstorctl

Manages and tests a cluster through the [cluster API](cluster-api.md).
See [Managing a cluster](../admin-guide/managing.md).

```text
usage: hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--mgmt URL [--token-file F] [--mgmt-ca FILE]] [--json | -o json|yaml] status|nodes|pools|pgs|pg-grid|volumes|create-volume|delete-volume|io-test|out|in|weight|device add|device class|pool create|pool set|pool add-device|pool remove-device|drain|token create|token list|token revoke|op list|op get|op watch|op cancel|cert list|cert revoke|cert show|map backup|member list|member add|member remove|snapshot list|snapshot create|snapshot delete|snapshot group|snapshot group-delete|clone-volume|rollback-volume|flatten-volume|expand-volume|write-volume|read-volume|enroll|trash list|trash restore|trash purge|trash settings|audit list|audit settings [--wait SECS] [--pool P] [--pg N] [--mib N] [--device D] [--weight W|auto] [--node N] [--address A] [--class C] [--classes C,C...|any] [--kind K] [--copies N] [--min N] [--pgs N] [--chunk B] [--domain D] [--size S] [--name N] [--id ID] [--force] [--role R] [--ttl SECS] [--dry-run] [--serial S] [--file F] [--out FILE] [--raft ADDR] [--map ADDR[,ADDR...]] [--epoch E] [--ids ID,ID...] [--group G] [--new-id ID] [--grace SECS] [--on-full keep|purge] [--since AGO] [--principal P] [--op OP] [--limit N] [--retention-days N] [--offset O] [--sparse] [--usage]
```

| Command | Does |
|---|---|
| `status` | Health (`ok`, `warn` or `error`) with its reasons, nodes and devices up, and PG states per pool |
| `nodes` | Each node and its devices: address, weight, free and total space (GiB; `-` until the device has reported to the leader), in or out |
| `pools` | Each pool: kind, copies, PGs clean, degraded, backfilling and inactive, and its usable free and total space (GiB, after copies or erasure coding) |
| `pgs` | One pool's PGs with their acting and up sets; `--pg N` for one PG |
| `pg-grid` | One pool's PG states, a character per PG, 64 to a line: `.` clean, `d` degraded, `b` backfilling, `X` inactive; with `--usage`, also a digit per PG for the data on its primary, `0` least to `9` most |
| `volumes` | One pool's volumes: id, name, size, state, and the client instance that has it attached |
| `create-volume` | Creates a volume of `--size` in `--pool`, with `--name` and, optionally, `--id`; prints its id |
| `delete-volume` | Moves volume `--id` (with its snapshots) to the trash, where it stays for its pool's grace period (a day by default); refused while it is attached, unless `--force`. Prints when it will be purged |
| `snapshot list` | Volume `--id`'s snapshots: epoch, name, group and state |
| `snapshot create` | Takes a snapshot of volume `--id`, named `--name`; an attached volume's client is paused for it (at most 2 s, else the snapshot is abandoned) |
| `snapshot delete` | Moves the snapshot at `--epoch` of volume `--id` to the trash; refused while a clone or rollback depends on it |
| `snapshot group` | Takes a snapshot of every volume in `--ids` at one moment (a consistency group); prints the group and each snapshot |
| `snapshot group-delete` | Moves group `--group`'s snapshots to the trash; the group goes with the last of them |
| `audit list` | The audit log, newest first: every change and every refused call, by the map service and by `hyperstor-mgmt` for people; `--since`, `--principal`, `--op` and `--limit` (default 100) narrow it. Administrators only |
| `audit settings` | How long audit records are kept; with `--retention-days`, sets it (0: forever) |
| `trash list` | What's in the trash, in `--pool` or every pool: volumes and snapshots, when each was deleted and when it will be purged |
| `trash restore` | Brings volume `--id` back (or its snapshot at `--epoch`), as `--name` if given, else under its own name, or `NAME-restored` if another volume took it meanwhile. Operators may |
| `trash purge` | Deletes volume `--id` (or its snapshot at `--epoch`) from the trash for good, at once, as an operation: prints its id; with `--wait`, follows it to the end |
| `trash settings` | Shows `--pool`'s grace period and what a full pool does with its trash; with `--grace` or `--on-full`, sets them |
| `clone-volume` | Creates a volume from the snapshot at `--epoch` of volume `--id`, sharing its data, named `--name`, with `--new-id` or a random id |
| `expand-volume` | Grows volume `--id` to `--size`; never shrinks it. A client that has it attached sees the new size after it attaches again |
| `write-volume` | Writes `--file` into volume `--id` at `--offset` (default 0), in whole 4 KiB blocks (the file's last block padded with zeros), holding the volume's lease meanwhile: refused while another client has it attached. `--sparse` skips blocks of zeros, for a volume that reads zeros there (a new one). With `--map`, goes straight to the map service, without the API or a certificate |
| `read-volume` | Reads `--size` bytes of volume `--id` at `--offset` into `--out`, in whole 4 KiB blocks; with `--map` as above |
| `enroll` | Makes this client a management client of the cluster from a `mgmt` token in `--file`: a key made here, the API (`--api`) checked against the root the token names; writes `admin.crt`, `admin.key` and `root.crt` into `--out`, as `--tls` reads them. The certificate lives 30 days |
| `rollback-volume` | Rolls volume `--id` (detached) back to its snapshot at `--epoch` |
| `flatten-volume` | Copies into clone `--id` what it reads from its parents, so they can be deleted, as an operation: prints its id; with `--wait`, follows it to the end |
| `out` | Takes a device out of placement: its PGs move to other devices |
| `in` | Brings a device back into placement |
| `weight` | Keeps `--device`'s placement weight at `--weight W`, or with `--weight auto` lets it follow the device's capacity in TiB again. Its PGs move as the weight changes. Needs every storage node and map service member on a build that has it |
| `device add` | Appends a device to `--node N`, served at `--address A` (D167), of class `--class C` (default: that of the node's other devices). It is device `N × 1000 + i`, at weight 0, so nothing moves until the node is restarted with one more `--shard A=PATH:SIZE` and the device reports its capacity. An address in use or a node that isn't there exits 1 |
| `device class` | Keeps `--device`'s class at `--class C`, or with `--class auto` lets it follow the device's kind (`nvme`, `ssd`, `hdd`, `file`) again (D178). A class holds devices of one kind, and a class named after a kind only that kind; a device whose kind isn't known yet (its node hasn't run) is refused, exit 1. Pools limited to classes move its PGs |
| `pool create` | Makes pool `--name N` on devices of `--classes C,C...` (required, D171): `--kind replicated` (default) or `cache`, `--copies N` (3; 1 for cache), `--min N` (half the copies, rounded up; 1 for cache), `--pgs N` (a power of two; default about 100 per device of its classes per copy), `--chunk BYTES` (4M), `--domain device\|host\|rack` (host). Refused (exit 1) for a name in use, classes whose devices span fewer domains than the copies, and hard disks with flash unless `--force` |
| `pool set` | Places pool `--pool P`'s PGs on devices of `--classes C,C...` only, or with `--classes any` on every device (D164); PGs move as for a weight change. Refused (exit 1) for `.meta`, when the classes' devices span fewer failure domains than the pool's copies, and when the classes hold hard disks and flash both, unless `--force`, which warns (D172). `pools` lists each pool's classes, `!` marking a forced mix |
| `pool add-device` | Lets pool `--pool P` use device `--device D` too, though it is not of the pool's classes, or takes back a device removed from it (D180); PGs move as for `pool set`, and it is refused as `pool set` is (exit 1), `--force` allowing hard disks with flash. A device serves every pool that uses it, sharing its space |
| `pool remove-device` | Stops pool `--pool P` using device `--device D` (D180), whether one of its classes or added; its PGs there move to the pool's other devices. Refused (exit 1) when the pool's devices would span fewer failure domains than its copies. `pools` lists a pool's classes, then `+D` for each device added and `-D` for each removed |
| `io-test` | Writes random data to a new volume, reads it back and compares |
| `drain` | Takes every device of `--node` out and follows its PGs as an operation, until none lives on the node; prints each PG that moves. With `--dry-run`, only prints what would move |
| `token create` | Makes a single-use join token for `--role node` (a storage host), `--role meta` (a map service member) or `--role mgmt --name N` (a management client such as glidex, named N: 1–32 of a–z, 0–9 and -), valid `--ttl` seconds. Prints the token alone on standard output, and how to use it on standard error. Storage tokens are made on the map service's leader |
| `token list` | The tokens this map service member made: id, role, expiry, and whether used, revoked or expired. Never the tokens themselves |
| `token revoke` | Makes token `--id` unusable |
| `op list` | Running and recent operations (drains, flattens, purges), newest first; finished ones are kept a week |
| `op get` | Operation `--id` |
| `op watch` | Prints operation `--id` as it changes, until it finishes (or `--wait` passes); exit status 1 if it failed or was cancelled |
| `member list` | The map service's members: voters and learners, their Raft and map addresses, and who leads |
| `member add` | Adds member `--id` at `--raft` and `--map`, already running and joined with a `meta` token: a learner until it has caught up (up to two minutes), then a voter. On the leader |
| `member remove` | Removes member `--id`; refused if fewer than three voters would be left, unless `--force`. On the leader |
| `map backup` | Writes a backup of the map service's store (the map, join tokens, certificates issued and revoked) to `--out`, for `hyperstor-meta --restore`. Administrators only |
| `cert list` | Certificates the API issued (joins and renewals): serial, identity, expiry, revoked or not; and every revoked serial |
| `cert revoke` | Revokes certificate `--serial`: every map service member refuses it, on the API and between members, until it would have expired |
| `cert show` | Prints the identity, serial and expiry of the certificate in `--file`, without the API: how to find the serial of a certificate the installer made |
| `op cancel` | Stops drain `--id`, bringing the node's devices back in; a flatten or a purge can't be cancelled |

| Option | Default | Meaning |
|---|---|---|
| `--api ADDR` | see below | `IP:PORT` of the cluster API |
| `--tls DIR` | see below | Connect over mutual TLS with `DIR/admin.crt`, `DIR/admin.key` and `DIR/root.crt` |
| `--socket PATH` | see below | Connect to the map service's local socket instead: no certificate needed (break-glass) |
| `--mgmt URL` | `$HYPERSTOR_MGMT` | Go through [hyperstor-mgmt](hyperstor-mgmt.md) as a person, with an API token: `http://127.0.0.1:8852` (loopback, or an SSH tunnel) or `https://NAME:8852`. Not with `--api`, `--tls` or `--socket` |
| `--token-file F` | `$HYPERSTOR_TOKEN` | With `--mgmt`: the file holding your API token (`hst_…`). Never on the command line, where others could read it |
| `--mgmt-ca FILE` | the host's CAs, and the cluster's root on its hosts | With an `https://` `--mgmt`: also trust the CA certificates in `FILE`. Without it, the cluster's root is trusted where `hyperstorctl.conf` names its `tls` directory |
| `--wait SECS` | 0 | `status`: ask again every half second until health is `ok` (also waiting for an API that isn't answering yet). `out` and `in`: until no PG is backfilling or inactive. `drain`, `flatten-volume`, `trash purge`: follow the operation until it finishes. `op watch`: give up after SECS. Exit status 1 if SECS pass first |
| `--json` | off | Print the API's answer as JSON, the form `curl` gets; the same as `-o json`. `-o yaml` prints it as YAML |
| `--offset O` | 0 | `write-volume`, `read-volume`: where in the volume, in bytes (a multiple of 4096; K, M, G suffixes) |
| `--sparse` | off | `write-volume`: skip blocks of zeros |
| `--usage` | off | `pg-grid`: each PG's data too (the map service's leader answers) |
| `--pool P` | the first data pool | The pool for `pgs`, `pg-grid`, `volumes`, `create-volume` and `io-test` (`.meta`, pool 0, holds volume headers) |
| `--pg N` | every PG | `pgs`: only this PG of the pool |
| `--mib N` | 16 | How much `io-test` writes, in MiB |
| `--device D` | | The device for `out`, `in`, `weight`, `pool add-device` and `pool remove-device` |
| `--weight W\|auto` | | `weight`: the weight to keep (0 or more; 0 places nothing on the device), or `auto` |
| `--node N` | | Every device of node N, for `out` and `in`; the node for `device add` |
| `--address A` | | `device add`: `IP:PORT` where the node's new reactor listens |
| `--class C` | | `device add`: the device's class; `device class`: the class to keep, or `auto` |
| `--classes C,C...\|any` | | `pool create`: the classes (required); `pool set`: the classes, or every device |
| `--kind K` | replicated | `pool create`: `replicated` or `cache` |
| `--copies N` | 3 (1 for cache) | `pool create`: copies |
| `--min N` | half the copies | `pool create`: copies that must be reachable to write |
| `--pgs N` | about 100 per device per copy | `pool create`: PG count, a power of two |
| `--chunk B` | 4M | `pool create`: chunk size, a multiple of 4096 bytes |
| `--domain D` | host | `pool create`: `device`, `host` or `rack` |
| `--size S` | | The new volume's size for `create-volume`: bytes, or with `K`, `M`, `G` or `T` |
| `--name N` | empty | The new volume's, clone's or snapshot's name: shown, not unique |
| `--id ID` | | `delete-volume` and the snapshot commands: the volume (32 hex digits). `create-volume`: its id, instead of a random one. `token revoke`: the token's id. `op get`, `op watch`, `op cancel`: the operation |
| `--epoch E` | | `snapshot delete`, `clone-volume`, `rollback-volume`: the snapshot, by the epoch `snapshot list` shows. `trash restore`, `trash purge`: a snapshot in the trash; without it, the volume |
| `--ids ID,ID...` | | `snapshot group`: the volumes, comma-separated |
| `--group G` | | `snapshot group-delete`: the group, as `snapshot group` printed it |
| `--new-id ID` | random | `clone-volume`: the clone's id (32 hex digits) |
| `--grace SECS` | | `trash settings`: how long a deleted volume or snapshot stays in the trash; 0 purges at once; at most a year |
| `--on-full keep\|purge` | `keep` | `trash settings`: what a full pool does with its trash (purge the oldest first, or keep it). Acts once pools have fullness thresholds |
| `--since AGO` | | `audit list`: only records from the last `AGO`: `30m`, `2h`, `7d`, or seconds |
| `--principal P` | | `audit list`: only records whose principal contains `P` (`user:ann`, `/admin/`) |
| `--op OP` | | `audit list`: only records of operation `OP` (`CreateVolume`) |
| `--limit N` | 100 | `audit list`: at most `N` records |
| `--retention-days N` | 365 | `audit settings`: keep records `N` days; 0 keeps them forever |
| `--force` | off | `delete-volume`: delete it even if a client has it attached; that client is blocklisted first, so its writes are refused. `member remove`: even below three voters. `pool create`, `pool set`, `pool add-device`: allow hard disks with flash |
| `--role R` | | `token create`: `node` or `meta` |
| `--ttl SECS` | 3600 | `token create`: how long the token is valid, at most a week |
| `--serial S` | | `cert revoke`: the certificate's serial, hex, as `cert list` and `cert show` print it |
| `--raft ADDR` | | `member add`: where the new member talks Raft, `IP:PORT` |
| `--map ADDR[,ADDR...]` | | `member add`: where the new member serves the map, one `IP:PORT`. `write-volume`, `read-volume`: the map service's members, any of which answers; the volume is reached without the API |
| `--out FILE` | | `map backup`: the file to write (mode 0600; never over an existing file) |
| `--file F` | | `cert show`: a certificate file (PEM) |
| `--dry-run` | off | `drain`: change nothing; print the PGs that would move |

**Through hyperstor-mgmt.** With `--mgmt`, `hyperstorctl` is a person,
not an administrator's certificate: make an API token on the dashboard
(or `POST /auth/tokens`), keep it in a file only you can read, and your
roles decide what you may do. What needs a fresh login (deleting,
rolling back, tokens, revocations, backups) is refused with
`(reauth_required)`: do it from the dashboard, or with the certificate.
The map service's audit names you (`… for user:NAME`).

```sh
export HYPERSTOR_MGMT=https://the-map-service-host:8852
export HYPERSTOR_TOKEN="$(cat ~/.hyperstor-token)"
hyperstorctl status
```

Without `--api`, the address is `$HYPERSTOR_API`, else the `api` line
of `/etc/hyperstor/hyperstorctl.conf` (written by
[hyperstor-install](hyperstor-install.md)), else `127.0.0.1:8850`. Likewise, without `--tls` the certificates are
those of the `tls` line in that file, if any; with neither, it uses
plain HTTP.

On the map service's host, when neither `--api` nor `--tls` is given and
the configuration's `socket` line names a socket that exists,
`hyperstorctl` uses that local socket: the kernel tells the map service
who is calling, and root and members of the `hyperstor-admin` group may
do everything. This keeps working when certificates have expired or
are lost. Elsewhere it uses the API address and the administrator
certificate, which root and `hyperstor-admin` can read.

`io-test` reaches the storage nodes as a client would: it asks the API
where the map service is, then writes and reads through the client
library. It prints the time and throughput of each phase.

A refused request prints the API's message and its reason, a stable
word for scripts such as `(not_found)`, `(token_used)` or
`(not_leader)`; the [cluster API](cluster-api.md#errors) lists them.

| Exit status | Meaning |
|---|---|
| 0 | Done; for `status`, health is `ok` |
| 1 | `status`: health is `warn` or `error`. Otherwise a failed request (no such device, pool or operation), a failed or cancelled operation, or a failed `io-test` |
| 2 | A missing or malformed option, or certificates that can't be read |
| 3 | The cluster API (or, for `io-test`, the map service) can't be reached, or the map service member asked isn't the leader |
| 4 | Not allowed: no certificate, or one whose role may not do this |
