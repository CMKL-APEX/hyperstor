# Running a cluster

A Hyperstor cluster today is:

- one **map service** (`hyperstor-meta`), which holds the cluster map,
  detects failed nodes and grants map leases;
- three or more **storage nodes** (`hyperstord`), each with one or more
  devices;
- **clients**: programs using the client library.

The cluster is described by one text file, the
[cluster file](../reference/cluster-file.md), read by the map service at
start.

> **Security.** Only the cluster API has authentication (mutual TLS
> with `--api-tls`). The map service's and storage nodes' ports have
> none and no encryption yet: anyone who can reach them can read and
> overwrite every volume. Run clusters only on an isolated network that
> untrusted hosts cannot reach, and keep test data only. Run clusters
> only on an isolated network that untrusted hosts cannot reach, and
> keep test data only.

| Page | Covers |
|---|---|
| [Installation](installation.md) | Requirements, building, and installing with `hyperstor-install` |
| [Managing a cluster](managing.md) | Health, taking devices out and in, testing I/O with `hyperstorctl` |
| [Planning a cluster](planning.md) | Nodes, devices, failure domains, pools and PG counts |
| [The map service](map-service.md) | Running `hyperstor-meta`, leases and failure detection |
| [Storage nodes](storage-nodes.md) | Running `hyperstord`, devices, CPUs, restarts |
| [Failures and recovery](failures.md) | What happens when a node fails or returns |
| [Measuring performance](benchmarking.md) | `hyperstor-bench` |
| [Troubleshooting](troubleshooting.md) | Common problems |
