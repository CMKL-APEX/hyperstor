# Concepts

## Processes

| Process | Runs on | Does |
|---|---|---|
| `hyperstor-meta` | three or five machines (one for a trial), replicated with Raft | Keeps the **cluster map**, notices failed nodes, hands out map leases, and serves the cluster API |
| `hyperstord` | every storage node | Stores data on local devices and serves reads and writes |
| `hyperstor-vhost` | every host that runs VMs on volumes | Serves volumes to the VMs as virtio disks, over vhost-user-blk |
| `hyperstor-mgmt` | the map service's hosts | The web UI, people's logins, and the cluster API on their behalf |
| your program, with `hyperstor-client` | anywhere on the network | Reads and writes volumes |

## The cluster map

The map describes the whole cluster: its nodes, their devices, the pools
and where each placement group lives. Every node and client holds a copy
and computes data locations from it, so no lookup is needed per I/O.
Each change to the map has a new **epoch** number; a node or client with
an older map is told so and fetches the new one.

## Nodes, devices and shards

A **node** is one `hyperstord` process. It drives one or more
**devices**: files or block devices, each served by its own thread (a
*reactor*, pinned to a CPU if you choose) that listens on its own TCP
port. In the cluster file, each address listed for a node is one device.
Node `N`'s devices are numbered `N × 1000`, `N × 1000 + 1` and so on;
a number is never given again once its device is removed. Devices can
be added to and removed from a running node.

Each device has:

- a **class**: its kind (`nvme`, `ssd`, `hdd`, or `file` for a device
  file) unless an administrator sets another name;
- a **weight**: by default its capacity in TiB, from the device's own
  reports, so a device twice the size gets twice the data.

## Pools

A **pool** is a set of data with one protection rule:

- **kind**: `replicated`, or `cache` for data that can be recomputed;
- **size**: how many copies of each chunk (2 or 3; 1 by default in a
  cache pool);
- **min**: how many copies must be reachable to accept a write;
- **failure domain**: what the copies must not share: a `device`, a
  `host` or a `rack`;
- **classes**: the device classes it places data on, or every device;
- **chunk size**: how volumes in the pool are cut into chunks.

Pools can be made, changed and deleted while the cluster runs. A pool's
usable space is its devices' space divided by its copies, counting only
what fits with each copy in a different failure domain.

A **cache pool** gives up durability for speed and space. A device or
node that fails loses its data: nothing is recovered, and a volume
reads zeros where its data was. A node that is only down for a moment
(a restart) keeps its share: the pool waits for it up to its grace
period, 60 seconds by default. Expiry and eviction are not built yet,
so a full cache pool refuses new data as other pools do.

## Placement groups

A pool is divided into a number of **placement groups** (PGs), a power
of two. Each chunk belongs to one PG, by a hash of its name, and each PG
is stored on `size` devices, its **acting set**. The first device of the
acting set is the PG's **primary**: clients send it reads and writes,
and it forwards writes to the other copies. When a node fails, only the
PGs it held move. As devices are added, the map service doubles a
pool's PG count, unless turned off for the pool: each PG is split in
place, then the new PGs move to devices of their own.

## Full devices

Each device is held to three thresholds, percentages of its space, the
same across the cluster:

| Threshold | Default | Past it |
|---|---|---|
| `nearfull` | 85 % | Health warns |
| `backfillfull` | 90 % | No PG is copied onto the device |
| `full` | 95 % | The device refuses new objects and writes to blocks not yet written |

Overwrites of written blocks and deletes still work past `full`, so
space can always be freed. A pool is as full as the fullest device it
uses, and a full pool takes no new volumes, clones or snapshots.

## Volumes

A **volume** is a sparse range of bytes in a pool, with a 128-bit id, a
size and a name. The cluster keeps a header for each volume, and its
chunks are created as they are written: blocks never written read as
zeros. Offsets and lengths are multiples of 4 KiB. A volume is used
through the [client library](user-guide/client-library.md), as a
VM's disk through `hyperstor-vhost`, or as a Linux block device
through `hyperstor-ublk` (see [Using Hyperstor](user-guide/index.md)).

## Snapshots and clones

A **snapshot** keeps a volume as it was at one instant. Taking one only
moves the volume's **epoch** on: later writes go to new versions of the
chunks, tagged with the new epoch, and the snapshot reads the versions
up to its own. So a snapshot copies nothing, and a **clone** is a new
volume that reads through its snapshot's versions until it writes its
own. A **consistency group** snapshots several volumes at one instant.

## The trash

Deleting a volume or a snapshot through the cluster API, `hyperstorctl`
or the web UI moves it to the **trash**: it can't be attached, but its
data stays, and counts against the pool, for the pool's grace period (a
day by default). Until then it can be restored; after it, the map
service purges it. See [The trash](admin-guide/managing.md#the-trash).

## The read cache

A version of a chunk that is no longer a volume's newest (a snapshot's,
or a clone's parent's) never changes. The client library caches blocks
of such versions in memory, shared by every volume it serves, so the
VMs of a host that boot clones of one image read it from the cluster
once.

## Volume leases and the blocklist

One client at a time **attaches** a volume, by holding its lease, which
it renews every second. To take a volume from a client that died or is
cut off, another client waits for the lease to run out and
**blocklists** the old lease: every storage node refuses requests made
under it from then on. So a volume never has two writers. Each attached
volume has a lease of its own, so taking one volume from a client leaves
its other volumes alone. The holder can let **delegates**, such as a
library inside a VM, act under its lease; they are cut off with it.

## Map leases

A storage node may act as a primary only while it holds a current
**map lease** from the map service, renewed by its heartbeats. A node cut
off from the map service stops serving before the map service gives its
PGs to other nodes, so two nodes never both accept writes for one PG.

## Down and out

A node that stops answering is **down**: its PGs go on with the other
copies, and nothing is copied, so a reboot moves no data. A node that
keeps going down and coming back is held down a little longer each
time, and health warns that it flaps. A device that is **out**, because
its node has been down for ten minutes, it failed, or an administrator
took it out, gets no placement, and its PGs are copied to other
devices.
