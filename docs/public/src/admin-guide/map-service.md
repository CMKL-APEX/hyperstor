# The map service

`hyperstor-meta` holds the cluster map, answers map requests from
storage nodes and clients, detects failed nodes from their heartbeats,
and grants **map leases**: a storage node serves I/O only while its lease
is current.

## Starting it

```sh
hyperstor-meta --listen 192.0.2.10:9000 --cluster /etc/hyperstor/cluster.txt
```

It prints the size of the map it loaded, then runs until stopped. All
options are in the [reference](../reference/hyperstor-meta.md).

With `--api 127.0.0.1:8850` it also serves the
[cluster API](../reference/cluster-api.md), which `hyperstorctl` uses to
report health and take devices out and in. The API has no
authentication yet: keep it on loopback or a closed network.

## Leases and failure detection

| Option | Default | Effect |
|---|---|---|
| `--lease-ms` | 600 | How long a storage node may serve after its last heartbeat was answered |
| `--down-after-ms` | 800 | How long without a heartbeat before a node is marked down; at least 1.25 × the lease |

A node marked down loses its roles: its placement groups go to other
nodes, and a node cut off from the map service has already stopped
serving, because its lease ran out first. Shorter times detect failures
sooner, at the cost of false alarms on a slow or busy network.

## One map service

The map service is not replicated yet (Raft is planned). Until it is:

- **It is a single point of failure.** While it is down, clients can't
  connect, and storage nodes stop serving I/O once their leases run out.
  Run it on a reliable host and restart it automatically.
- **It keeps its state in memory only.** On restart it reads the cluster
  file again and starts with every node up; storage nodes reconnect on
  their own. It forgets which nodes were down and which were being
  refilled, so **restart it only while all storage nodes are up and no
  recovery is running** (see [Failures and recovery](failures.md)).
- **Changes to the cluster file take effect on restart only.** Adding or
  removing nodes and pools online is not supported yet.
