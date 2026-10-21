# hyperstor-meta

The map service. See [The map service](../admin-guide/map-service.md).

```text
usage: hyperstor-meta --listen ADDR --cluster FILE [--api ADDR] [--lease-ms L] [--down-after-ms N]
```

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR` | required | `IP:PORT` to serve on; storage nodes and clients connect here |
| `--cluster FILE` | required | The [cluster file](cluster-file.md) |
| `--api ADDR` | off | Also serve the [cluster API](cluster-api.md) on `IP:PORT` (plain HTTP, no authentication: keep it on loopback or a closed network) |
| `--lease-ms L` | 600 | Length of the map leases granted to storage nodes, in milliseconds |
| `--down-after-ms N` | 800 | Mark a node down after N ms without a heartbeat; at least 1.25 × `--lease-ms` |

On start it prints the number of nodes, devices, pools and PGs it
loaded, the map epoch and the lease length. It runs until stopped.

| Exit status | Meaning |
|---|---|
| 1 | The cluster file can't be read or has a mistake (the message names the line), or the `--api` address can't be bound |
| 2 | A missing or malformed option |
