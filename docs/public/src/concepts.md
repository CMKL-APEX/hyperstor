# Concepts

## Processes

| Process | Runs on | Does |
|---|---|---|
| `hyperstor-meta` | three or five machines (one for a trial), replicated with Raft | Keeps the **cluster map**, notices failed nodes, hands out map leases |
| `hyperstord` | every storage node | Stores data on local devices and serves reads and writes |
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

## Pools

A **pool** is a set of data with one protection rule:

- **size**: how many copies of each chunk (2 or 3);
- **min**: how many copies must be reachable to accept a write;
- **failure domain**: what the copies must not share: a `device`, a
  `host` or a `rack`;
- **chunk size**: how volumes in the pool are cut into chunks.

## Placement groups

A pool is divided into a fixed number of **placement groups** (PGs), a
power of two. Each chunk belongs to one PG, by a hash of its name, and
each PG is stored on `size` devices, its **acting set**. The first device
of the acting set is the PG's **primary**: clients send it reads and
writes, and it forwards writes to the other copies. When a node fails,
only the PGs it held move.

## Volumes

A **volume** is a sparse range of bytes in a pool, with a 128-bit id, a
size and a name. The cluster keeps a header for each volume, and its
chunks are created as they are written: blocks never written read as
zeros. Offsets and lengths are multiples of 4 KiB.

## Volume leases and the blocklist

One client at a time **attaches** a volume, by holding its lease, which
it renews every second. To take a volume from a client that died or is
cut off, another client waits for the lease to run out and
**blocklists** the old one: every storage node refuses it from then on.
So a volume never has two writers.

## Map leases

A storage node may act as a primary only while it holds a current
**map lease** from the map service, renewed by its heartbeats. A node cut
off from the map service stops serving before the map service gives its
PGs to other nodes, so two nodes never both accept writes for one PG.

## Down and out

A node that stops answering is **down**: its PGs go on with the other
copies, and nothing is copied, so a reboot moves no data. A device that
is **out**, because its node has been down for ten minutes, it failed,
or an administrator took it out, gets no placement, and its PGs are
copied to other devices.
