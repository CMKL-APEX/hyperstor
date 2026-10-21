# hyperstor-meta

The map service. See [The map service](../admin-guide/map-service.md).

```text
usage: hyperstor-meta --listen ADDR --cluster FILE [--lease-ms L] [--down-after-ms N]
```

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR` | required | `IP:PORT` to serve on; storage nodes and clients connect here |
| `--cluster FILE` | required | The [cluster file](cluster-file.md) |
| `--lease-ms L` | 2000 | Length of the map leases granted to storage nodes, in milliseconds |
| `--down-after-ms N` | 3000 | Mark a node down after N ms without a heartbeat; at least 1.25 × `--lease-ms` |

On start it prints the number of nodes, devices, pools and PGs it
loaded, the map epoch and the lease length. It runs until stopped.

| Exit status | Meaning |
|---|---|
| 1 | The cluster file can't be read or has a mistake (the message names the line) |
| 2 | A missing or malformed option |
