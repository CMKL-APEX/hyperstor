# hyperstor-meta

The map service. See [The map service](../admin-guide/map-service.md).

```text
usage: hyperstor-meta --listen ADDR --cluster FILE [--data DIR] [--id N] [--member N=RAFT/MAP ...] [--api ADDR] [--api-tls DIR] [--api-socket PATH] [--lease-ms L] [--down-after-ms N]
```

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR` | required | `IP:PORT` to serve on; storage nodes and clients connect here |
| `--cluster FILE` | required | The [cluster file](cluster-file.md). With `--data`, it seeds the map the first time; later starts add the nodes, devices and pools it has that the map lacks, and change nothing else |
| `--data DIR` | off | Keep the map in a replicated store in `DIR`, so it survives restarts. Without `--member`, this member is a group of one |
| `--id N` | 1 | This member's number in the `--member` list |
| `--member N=RAFT/MAP` | none | One member of the map service: its number, the `IP:PORT` it talks to the other members on, and the `IP:PORT` it serves on (its `--listen`). Give one per member, this one included, the same on every member; use 3 members, or 5 for large clusters. The members elect a leader, which grants leases and changes the map; the others serve the map and send the rest to the leader. Needs `--data`; members talk over mutual TLS with the `--api-tls` certificates |
| `--api ADDR` | off | Also serve the [cluster API](cluster-api.md) on `IP:PORT` |
| `--api-tls DIR` | off | Serve the API over mutual TLS with `DIR/api.crt` (certificate chain), `DIR/api.key` and `DIR/root.crt`; clients need a certificate of this cluster. Without it the API is plain HTTP with no authentication: keep it on loopback |
| `--api-socket PATH` | off | Also serve the API on a local Unix socket (mode 0666): callers are identified by the kernel, and root, the service's own user and members of `hyperstor-admin` may do everything. Needs `--api` |
| `--lease-ms L` | 600 | Length of the map leases granted to storage nodes, in milliseconds |
| `--down-after-ms N` | 800 | Mark a node down after N ms without a heartbeat; at least 1.25 × `--lease-ms` |

On start it prints the number of nodes, devices, pools and PGs it
loaded, the map epoch and the lease length. It runs until stopped.

| Exit status | Meaning |
|---|---|
| 1 | The cluster file can't be read or has a mistake (the message names the line), or the `--api` or member address can't be bound, or the `--api-tls` files can't be read or don't fit together, or the store in `--data` can't be opened |
| 2 | A missing or malformed option |
