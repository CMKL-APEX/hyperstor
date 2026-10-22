# hyperstorctl

Manages and tests a cluster through the [cluster API](cluster-api.md).
See [Managing a cluster](../admin-guide/managing.md).

```text
usage: hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--mgmt URL [--token-file F] [--mgmt-ca FILE]] [--json | -o json|yaml] status|nodes|pools|pgs|volumes|create-volume|delete-volume|io-test|out|in|drain|token create|token list|token revoke|op list|op get|op watch|op cancel|cert list|cert revoke|cert show|map backup|member list|member add|member remove|snapshot list|snapshot create|snapshot delete|snapshot group|snapshot group-delete|clone-volume|rollback-volume|flatten-volume [--wait SECS] [--pool P] [--mib N] [--device D] [--node N] [--size S] [--name N] [--id ID] [--force] [--role R] [--ttl SECS] [--dry-run] [--serial S] [--file F] [--out FILE] [--raft ADDR] [--map ADDR] [--epoch E] [--ids ID,ID...] [--group G] [--new-id ID]
```

| Command | Does |
|---|---|
| `status` | Health (`ok`, `warn` or `error`) with its reasons, nodes and devices up, and PG states per pool |
| `nodes` | Each node and its devices: address, weight, in or out |
| `pools` | Each pool: kind, copies, PGs clean, degraded, backfilling and inactive |
| `pgs` | One pool's PGs with their acting and up sets |
| `volumes` | One pool's volumes: id, name, size, state, and the client instance that has it attached |
| `create-volume` | Creates a volume of `--size` in `--pool`, with `--name` and, optionally, `--id`; prints its id |
| `delete-volume` | Deletes volume `--id` and its data; refused while it is attached, unless `--force` |
| `snapshot list` | Volume `--id`'s snapshots: epoch, name, group and state |
| `snapshot create` | Takes a snapshot of volume `--id`, named `--name`; an attached volume's client is paused for it (at most 2 s, else the snapshot is abandoned) |
| `snapshot delete` | Deletes the snapshot at `--epoch` of volume `--id`; refused while a clone or rollback depends on it |
| `snapshot group` | Takes a snapshot of every volume in `--ids` at one moment (a consistency group); prints the group and each snapshot |
| `snapshot group-delete` | Deletes group `--group`'s snapshots, and the group |
| `clone-volume` | Creates a volume from the snapshot at `--epoch` of volume `--id`, sharing its data, named `--name`, with `--new-id` or a random id |
| `rollback-volume` | Rolls volume `--id` (detached) back to its snapshot at `--epoch` |
| `flatten-volume` | Copies into clone `--id` what it reads from its parents, so they can be deleted |
| `out` | Takes a device out of placement: its PGs move to other devices |
| `in` | Brings a device back into placement |
| `io-test` | Writes random data to a new volume, reads it back and compares |
| `drain` | Takes every device of `--node` out and follows its PGs as an operation, until none lives on the node; prints each PG that moves. With `--dry-run`, only prints what would move |
| `token create` | Makes a single-use join token for `--role node` (a storage host) or `--role meta` (a map service member), valid `--ttl` seconds. Prints the token alone on standard output, and the command that joins a host with it on standard error. Storage tokens are made on the map service's leader |
| `token list` | The tokens this map service member made: id, role, expiry, and whether used, revoked or expired. Never the tokens themselves |
| `token revoke` | Makes token `--id` unusable |
| `op list` | Running and recent operations, newest first |
| `op get` | Operation `--id` |
| `op watch` | Prints operation `--id` as it progresses, until it finishes (or `--wait` passes); exit status 1 if it failed or was cancelled |
| `member list` | The map service's members: voters and learners, their Raft and map addresses, and who leads |
| `member add` | Adds member `--id` at `--raft` and `--map`, already running and joined with a `meta` token: a learner until it has caught up (up to two minutes), then a voter. On the leader |
| `member remove` | Removes member `--id`; refused if fewer than three voters would be left, unless `--force`. On the leader |
| `map backup` | Writes a backup of the map service's store (the map, join tokens, certificates issued and revoked) to `--out`, for `hyperstor-meta --restore`. Administrators only |
| `cert list` | Certificates the API issued (joins and renewals): serial, identity, expiry, revoked or not; and every revoked serial |
| `cert revoke` | Revokes certificate `--serial`: every map service member refuses it, on the API and between members, until it would have expired |
| `cert show` | Prints the identity, serial and expiry of the certificate in `--file`, without the API: how to find the serial of a certificate the installer made |
| `op cancel` | Stops operation `--id`; a cancelled drain brings the node's devices back in |

| Option | Default | Meaning |
|---|---|---|
| `--api ADDR` | see below | `IP:PORT` of the cluster API |
| `--tls DIR` | see below | Connect over mutual TLS with `DIR/admin.crt`, `DIR/admin.key` and `DIR/root.crt` |
| `--socket PATH` | see below | Connect to the map service's local socket instead: no certificate needed (break-glass) |
| `--mgmt URL` | `$HYPERSTOR_MGMT` | Go through [hyperstor-mgmt](hyperstor-mgmt.md) as a person, with an API token: `http://127.0.0.1:8852` (loopback, or an SSH tunnel) or `https://NAME:8852`. Not with `--api`, `--tls` or `--socket` |
| `--token-file F` | `$HYPERSTOR_TOKEN` | With `--mgmt`: the file holding your API token (`hst_…`). Never on the command line, where others could read it |
| `--mgmt-ca FILE` | the host's CAs | With an `https://` `--mgmt`: also trust the CA certificates in `FILE` |
| `--wait SECS` | 0 | `status`: ask again every half second until health is `ok` (also waiting for an API that isn't answering yet). `out` and `in`: until no PG is backfilling or inactive. `drain`: follow the operation until it finishes. `op watch`: give up after SECS. Exit status 1 if SECS pass first |
| `--json` | off | Print the API's answer as JSON, the form `curl` gets; the same as `-o json`. `-o yaml` prints it as YAML |
| `--pool P` | the first data pool | The pool for `pgs`, `volumes`, `create-volume` and `io-test` (`.meta`, pool 0, holds volume headers) |
| `--mib N` | 16 | How much `io-test` writes, in MiB |
| `--device D` | | The device for `out` and `in` |
| `--node N` | | Every device of node N, for `out` and `in` |
| `--size S` | | The new volume's size for `create-volume`: bytes, or with `K`, `M`, `G` or `T` |
| `--name N` | empty | The new volume's, clone's or snapshot's name: shown, not unique |
| `--id ID` | | `delete-volume` and the snapshot commands: the volume (32 hex digits). `create-volume`: its id, instead of a random one. `token revoke`: the token's id. `op get`, `op watch`, `op cancel`: the operation |
| `--epoch E` | | `snapshot delete`, `clone-volume`, `rollback-volume`: the snapshot, by the epoch `snapshot list` shows |
| `--ids ID,ID...` | | `snapshot group`: the volumes, comma-separated |
| `--group G` | | `snapshot group-delete`: the group, as `snapshot group` printed it |
| `--new-id ID` | random | `clone-volume`: the clone's id (32 hex digits) |
| `--force` | off | `delete-volume`: delete it even if a client has it attached; that client is blocklisted first, so its writes are refused. `member remove`: even below three voters |
| `--role R` | | `token create`: `node` or `meta` |
| `--ttl SECS` | 3600 | `token create`: how long the token is valid, at most a week |
| `--serial S` | | `cert revoke`: the certificate's serial, hex, as `cert list` and `cert show` print it |
| `--raft ADDR` | | `member add`: where the new member talks Raft, `IP:PORT` |
| `--map ADDR` | | `member add`: where the new member serves the map, `IP:PORT` |
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
export HYPERSTOR_MGMT=http://127.0.0.1:8852
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
