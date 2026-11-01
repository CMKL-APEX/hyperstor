# Planning a cluster

## Nodes and devices

- Use at least as many storage nodes as the largest pool's `size`, in
  distinct failure domains, plus one so that a failed node's data can be
  copied to a stand-in (see [Failures and recovery](failures.md)). Three
  nodes run a three-copy pool; it keeps working with two copies while
  one node is down, but can't restore the third until the node returns.
- Give each node one device per CPU core you want to dedicate to
  storage. Each device has its own thread and its own TCP port.
- Device files grow up to the size given to `hyperstord` as data is
  written. Keep that much room on the filesystem.

## Node numbers and addresses

Every node has a number, unique in the cluster, and one address per
device. The addresses in the [cluster file](../reference/cluster-file.md)
and the `--shard` options of `hyperstord` must match, in the same order:
the first address listed for node 3 is node 3's first `--shard`.

## Failure domains

The `rack` number of each node in the cluster file, and each pool's
`domain`, decide what two copies of a chunk may share:

| `domain` | Copies are on different | Use when |
|---|---|---|
| `device` | devices (possibly on one host) | testing on one machine |
| `host` | storage nodes | the default |
| `rack` | racks | nodes in a rack share power or a switch |

When there are fewer domains than copies, a PG gets fewer copies than
`size`: plan at least `size` domains.

## Pools

| Setting | Guidance |
|---|---|
| `size` | 3 for data you care about; 2 halves the space but survives only one failure |
| `min` | `size - 1`: writes go on with one copy missing, and stop rather than run on one copy |
| `pgs` | a power of two; about 100 PGs per device across all pools spreads data evenly. It can't be changed later yet |
| `chunk` | 4 MiB (`4194304`) suits most volumes; it must be a multiple of 4096 |

Erasure-coded pools (`ec4+2`) are accepted by the cluster file but not
served yet; use replicated pools.

## Network

Each storage node device needs one port reachable from every client and
every other storage node, for TCP and for UDP: storage nodes ping each
other over UDP at the same port number, and fall back to TCP for a peer
that doesn't answer (a firewall that drops UDP makes failures slower to
notice under load). The map service needs one TCP port reachable from
all of them. Pick ports and firewall rules accordingly, and keep the
storage network isolated (see [Running a cluster](index.md)).
