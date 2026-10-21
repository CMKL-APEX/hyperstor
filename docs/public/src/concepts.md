# Concepts

## Processes

| Process | Runs on | Does |
|---|---|---|
| `hyperstor-meta` | one machine (several, with Raft, later) | Keeps the **cluster map**, notices failed nodes, hands out map leases |
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

A **volume** is a sparse range of bytes in a pool, named by a 128-bit
number (its *lineage*). Volumes need no creation step yet: writing to a
lineage creates its chunks, and blocks never written read as zeros.
Offsets and lengths are multiples of 4 KiB.

## Map leases

A storage node may act as a primary only while it holds a current
**map lease** from the map service, renewed by its heartbeats. A node cut
off from the map service stops serving before the map service gives its
PGs to other nodes, so two nodes never both accept writes for one PG.
