# Running a cluster

A Hyperstor cluster today is:

- the **map service** (`hyperstor-meta`): one member, or three or five
  that keep the map together. It holds the cluster map, detects failed
  nodes, grants map leases and serves the cluster API;
- three or more **storage nodes** (`hyperstord`), each with one or more
  devices;
- the **management service** (`hyperstor-mgmt`) on a map service host:
  the web UI, people's logins, and the cluster API on their behalf;
- on hosts that run VMs, **`hyperstor-vhost`**, which serves volumes to
  them as disks;
- **clients**: programs using the client library, `hyperstor-ublk` and
  `hyperstorctl`.

The cluster is described by one text file, the
[cluster file](../reference/cluster-file.md), read by the map service at
start. Hosts can also join a running cluster with a token, and devices
and pools can be added and removed while it runs.

> **Security.** The cluster API needs a client certificate (mutual
> TLS), and the management service a login. The map service's and the
> storage nodes' data ports have no authentication and no encryption
> yet: anyone who can reach them can read and overwrite every volume.
> Run clusters only on an isolated network that untrusted hosts cannot
> reach, and keep test data only.

| Page | Covers |
|---|---|
| [Installation](installation.md) | Requirements, building, a trial cluster, joining hosts with tokens, hosts that run VMs |
| [Managing a cluster](managing.md) | The web UI and logins, health, volumes and VM disks, the trash, audit, operations, metrics |
| [Pools and devices](pools-and-devices.md) | Taking devices out and in, adding and removing them, weights, classes, full devices, splitting and deleting pools |
| [Planning a cluster](planning.md) | Nodes, devices, failure domains, pools, device classes, capacity |
| [The map service](map-service.md) | Running `hyperstor-meta`, leases, members, backups and recovery |
| [Storage nodes](storage-nodes.md) | Running `hyperstord`, devices, CPUs, restarts |
| [Failures and recovery](failures.md) | What happens when a node, device or client fails, or a device fills |
| [Measuring performance](benchmarking.md) | `hyperstor-bench` |
| [Troubleshooting](troubleshooting.md) | Common problems |
