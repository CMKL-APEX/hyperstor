# hyperstorctl

Manages and tests a cluster through the [cluster API](cluster-api.md).
See [Managing a cluster](../admin-guide/managing.md).

```text
usage: hyperstorctl [--api ADDR] [--tls DIR] [--socket PATH] [--json] status|nodes|pools|pgs|volumes|create-volume|delete-volume|io-test|out|in [--wait SECS] [--pool P] [--mib N] [--device D] [--node N] [--size S] [--name N] [--id ID] [--force]
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
| `out` | Takes a device out of placement: its PGs move to other devices |
| `in` | Brings a device back into placement |
| `io-test` | Writes random data to a new volume, reads it back and compares |

| Option | Default | Meaning |
|---|---|---|
| `--api ADDR` | see below | `IP:PORT` of the cluster API |
| `--tls DIR` | see below | Connect over mutual TLS with `DIR/admin.crt`, `DIR/admin.key` and `DIR/root.crt` |
| `--socket PATH` | see below | Connect to the map service's local socket instead: no certificate needed (break-glass) |
| `--wait SECS` | 0 | `status`: ask again every half second until health is `ok` (also waiting for an API that isn't answering yet). `out` and `in`: until no PG is backfilling or inactive. Exit status 1 if SECS pass first |
| `--json` | off | Print the API's answer as JSON (`status`, `nodes`, `pools`, `pgs`, `volumes`, `create-volume`) |
| `--pool P` | the first data pool | The pool for `pgs`, `volumes`, `create-volume` and `io-test` (`.meta`, pool 0, holds volume headers) |
| `--mib N` | 16 | How much `io-test` writes, in MiB |
| `--device D` | | The device for `out` and `in` |
| `--node N` | | Every device of node N, for `out` and `in` |
| `--size S` | | The new volume's size for `create-volume`: bytes, or with `K`, `M`, `G` or `T` |
| `--name N` | empty | The new volume's name: shown, not unique |
| `--id ID` | | `delete-volume`: the volume (32 hex digits). `create-volume`: its id, instead of a random one |
| `--force` | off | `delete-volume`: delete it even if a client has it attached; that client is blocklisted first, so its writes are refused |

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

| Exit status | Meaning |
|---|---|
| 0 | Done; for `status`, health is `ok` |
| 1 | `status`: health is `warn` or `error`. Otherwise a refused request (no such device or pool, or the certificate's role may not do it) or a failed `io-test` |
| 2 | A missing or malformed option, or certificates that can't be read |
| 3 | The cluster API (or, for `io-test`, the map service) can't be reached |
