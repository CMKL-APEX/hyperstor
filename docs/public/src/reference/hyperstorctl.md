# hyperstorctl

Manages and tests a cluster through the [cluster API](cluster-api.md).
See [Managing a cluster](../admin-guide/managing.md).

```text
usage: hyperstorctl [--api ADDR] [--tls DIR] [--json] status|nodes|pools|pgs|io-test|out|in [--wait SECS] [--pool P] [--mib N] [--device D] [--node N]
```

| Command | Does |
|---|---|
| `status` | Health (`ok`, `warn` or `error`) with its reasons, nodes and devices up, and PG states per pool |
| `nodes` | Each node and its devices: address, weight, in or out |
| `pools` | Each pool: kind, copies, PGs clean, degraded, backfilling and inactive |
| `pgs` | One pool's PGs with their acting and up sets |
| `out` | Takes a device out of placement: its PGs move to other devices |
| `in` | Brings a device back into placement |
| `io-test` | Writes random data to a new volume, reads it back and compares |

| Option | Default | Meaning |
|---|---|---|
| `--api ADDR` | see below | `IP:PORT` of the cluster API |
| `--tls DIR` | see below | Connect over mutual TLS with `DIR/admin.crt`, `DIR/admin.key` and `DIR/root.crt` |
| `--wait SECS` | 0 | `status`: ask again every half second until health is `ok` (also waiting for an API that isn't answering yet). `out` and `in`: until no PG is backfilling or inactive. Exit status 1 if SECS pass first |
| `--json` | off | Print the API's answer as JSON (`status`, `nodes`, `pools`, `pgs`) |
| `--pool P` | the first pool | The pool for `pgs` and `io-test` |
| `--mib N` | 16 | How much `io-test` writes, in MiB |
| `--device D` | | The device for `out` and `in` |
| `--node N` | | Every device of node N, for `out` and `in` |

Without `--api`, the address is `$HYPERSTOR_API`, else the `api` line
of `/etc/hyperstor/hyperstorctl.conf` (written by
[hyperstor-install](hyperstor-install.md)), else `127.0.0.1:8850`. Likewise, without `--tls` the certificates are
those of the `tls` line in that file, if any; with neither, it uses
plain HTTP. On a host installed with `hyperstor-install`, run it as
root or as a member of the `hyperstor-admin` group, which can read the
administrator certificate.

`io-test` reaches the storage nodes as a client would: it asks the API
where the map service is, then writes and reads through the client
library. It prints the time and throughput of each phase.

| Exit status | Meaning |
|---|---|
| 0 | Done; for `status`, health is `ok` |
| 1 | `status`: health is `warn` or `error`. Otherwise a refused request (no such device or pool, or the certificate's role may not do it) or a failed `io-test` |
| 2 | A missing or malformed option, or certificates that can't be read |
| 3 | The cluster API (or, for `io-test`, the map service) can't be reached |
