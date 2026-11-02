# Planning a cluster

## Nodes and devices

- Use at least as many storage nodes as the largest pool's `size`, in
  distinct failure domains, plus one so that a failed node's data can be
  copied to a stand-in (see [Failures and recovery](failures.md)). Three
  nodes run a three-copy pool; it keeps working with two copies while
  one node is down, but can't restore the third until the node returns.
- Give each node one device per CPU core you want to dedicate to
  storage. Each device has its own thread and its own port.
- Device files grow up to the size given to `hyperstord` as data is
  written. Keep that much room on the filesystem.
- Devices of different sizes are fine: each gets data in proportion to
  its capacity (its **weight**; see
  [Device weights](pools-and-devices.md#device-weights)).
- Nodes, devices and pools can be added and removed later, while the
  cluster runs (see [Managing a cluster](managing.md)): start with what
  you have.

## Node numbers and addresses

Every node has a number, unique in the cluster, and one address per
device. The addresses in the [cluster file](../reference/cluster-file.md)
and the `--shard` options of `hyperstord` must match, in the same order:
the first address listed for node 3 is node 3's first `--shard`.
Devices are numbered `node × 1000 + i`; an address can name its
device's number instead (`1002@192.0.2.21:9102`). Numbers are never
given again once a device is removed. Hosts that join with a token get
their numbers from the map service.

## Failure domains

The `rack` number of each node in the cluster file, and each pool's
`domain`, decide what two copies of a chunk may share:

| `domain` | Copies are on different | Use when |
|---|---|---|
| `device` | devices (possibly on one host) | testing on one machine |
| `host` | storage nodes | the default |
| `rack` | racks | nodes in a rack share power or a switch |

When there are fewer domains than copies, a PG gets fewer copies than
`size`: plan at least `size` domains. Pools made or changed at run time
are refused when their devices span fewer domains than their copies.

## Pools

| Setting | Guidance |
|---|---|
| `size` | 3 for data you care about; 2 halves the space but survives only one failure |
| `min` | `size - 1`: writes go on with one copy missing, and stop rather than run on one copy |
| `pgs` | a power of two; about 100 PGs per device across all pools, counting copies, spreads data evenly. It can be doubled later, and the map service doubles it by itself as devices are added (see [Splitting a pool's PGs](pools-and-devices.md#splitting-a-pools-pgs)); it can't shrink |
| `chunk` | 4 MiB (`4194304`) suits most volumes; it must be a multiple of 4096 |
| `class` | the device classes the pool places on (`nvme,ssd`); without it, every device |

Pools are declared in the cluster file, or made at run time with
`hyperstorctl pool create`.

| Kind | For |
|---|---|
| `replicated` | Volumes and anything you can't recompute |
| `cache` | Data that can be recomputed. One copy by default; the data on a failed device is lost, and its PGs start empty elsewhere. A restart or a PG split keeps it, and a node that comes back within the pool's grace (60 s by default) keeps its PGs and their data. Expiry and eviction aren't built yet: a cache pool fills up like any other |
| `ecK+M` | Erasure coding: accepted by the cluster file but not served yet; use replicated pools |

## Device classes

Each device has a class: its kind (`nvme`, `ssd`, `hdd`, or `file` for a
device file) unless you set another. A pool that names classes places
its PGs only on those devices, so one cluster can hold a fast pool on
NVMe and a large one on hard disks. A pool mixing hard disks and flash
is refused unless forced: a replicated write waits for its slowest
copy. See [Device classes](pools-and-devices.md#device-classes).

## Capacity

- **Usable space.** A pool's usable space is its devices' space divided
  by its copies, counted so that no failure domain holds more than one
  copy of a PG: with as many hosts as copies, the smallest host bounds
  the pool. Pools that share devices share their space.
- **Volumes are thin.** A volume takes space only as it is written, so
  the volumes in a pool can add up to more than the pool holds. Watch
  the space (`hyperstorctl pools`, the web UI) rather than the volumes'
  sizes.
- **Thresholds.** Health warns when a device passes 85 % (`nearfull`),
  devices past 90 % take no backfill (`backfillfull`), and past 95 %
  (`full`) a device refuses new data. See
  [Full devices](pools-and-devices.md#full-devices).
- **Room to recover.** When a host is lost for good, its data is copied
  onto the others. Keep enough free space below `backfillfull` on the
  remaining hosts to hold one host's data, or the copies wait for room.
- **The trash.** Deleted volumes and snapshots keep their space for
  their pool's grace period (a day by default). See
  [The trash](managing.md#the-trash).

## The map service

Run three members on three hosts (five for large clusters), so that the
map survives a lost host; one member is a single point of failure. The
installer runs the management service on the map service's hosts. See
[The map service](map-service.md#several-members).

## Network

Each storage node device needs one port reachable from every client and
every other storage node, for TCP and for UDP: storage nodes ping each
other over UDP at the same port number, and fall back to TCP for a peer
that doesn't answer (a firewall that drops UDP makes failures slower to
notice under load). The map service needs one TCP port reachable from
all of them, and its members one more between themselves. Pick ports
and firewall rules accordingly, and keep the storage network isolated
(see [Running a cluster](index.md)).

### Several networks

A host with several network ports (a management port, and fast storage
or InfiniBand ports) can carry reads, writes and replication on the
fast ones and keep the rest on the management network:

- Declare the networks in the [cluster file](../reference/cluster-file.md#network),
  each `control`, `data` or `both`.
- Give each device one or more `endpoint` lines on a data network. The
  device's own address (on its `node` line) stays where the map service
  and pings reach it.
- Install as usual. Each storage node then also listens on its devices'
  endpoints, and reaches the other nodes at their endpoints on the data
  networks it has endpoints on.
- Clients choose too: [`hyperstor-vhost --networks`](../reference/hyperstor-vhost.md)
  for VM disks. A client on no network uses devices' addresses.

Put each endpoint on a port close to its device: on the same PCIe switch
or, failing that, the same CPU socket. With several ports in one subnet
(InfiniBand ports often are), Linux sends from every address through
one of them unless each address has a routing rule of its own; add
those, or traffic leaves by one port.

Choosing endpoints for you, from the host's topology, and moving traffic
between ports as load changes are planned.
