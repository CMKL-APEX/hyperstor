# Metadata, leases and fencing

The three levels of metadata (Raft cluster map, `.meta` objects, engine metadata), volume leases and the blocklist. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 4. Metadata

Metadata is kept at three levels, each ordered by a different leader, so
that no single node sees every change (D28):

| Level | Holds | Ordered by | Changes with |
|---|---|---|---|
| Cluster map | nodes, devices, pools, the PG table and epochs, host exports, the blocklist | the `hyperstor-meta` Raft leader | hardware changes and failures |
| Volume metadata (the `.meta` pool) | volume headers, lineage trees, pool directories, snapshot groups | the primary of the PG that holds each object | volume operations, lease renewals |
| Block metadata (engines, 08-engine.md §7) | extent maps, versions, checksums, allocation | the primary of each data PG | I/O |

### 4.1 The cluster map (Raft)

- `hyperstor-meta` is a Raft group of 3 or 5 voters, on `openraft` with a
  ReDB state machine (D20). Writes follow clustering.md D2 and D3: the
  leader runs the change against its applied state, and the Raft entry
  is the resulting write set.
- **Tables:**

| Table | Key | Holds |
|---|---|---|
| `nodes` | node id | networks (13-protocol.md §13.2: for each, its address, its transports and their parameters), role, failure domain path (`rack/host`), reactor count, state |
| `devices` | device id | node, kind (`nvme`, `ssd`, `hdd`), identity (NVMe EUI-64/NGUID, or WWN and serial), log device (for `hdd`), capacity, class, weight, state (`in`, `out`, `failed`) |
| `pools` | pool id | kind (`replicated{size, min_size}`, `ec{k, m}` or `cache{copies, default_ttl}`, D45), chunk size, PG count, failure domain level, device class filter, thresholds, priority class (D46) |
| `pgs` | (pool, pg) | `pg_epoch`, `up` set (computed placement), `acting` set (who serves now), primary, in-sync set, state |
| `hosts` | host identity | bare-metal and non-glidex hosts: NQN, CHAP key reference, the volumes and snapshots exported to it |
| `blocklist` | client instance id | map epoch added, expiry |

- **As built** (HS1, 01-implementation-notes.md I62–I67): one table, `map`,
  with a `head` row (epoch, nodes, devices, pools) and one row per PG;
  `hosts` and `blocklist` come with their features. The leader publishes
  a change as one write set of the head and the PG rows that changed.
  Members talk Raft over TCP with mutual TLS; nodes and clients follow
  the leader by redirect (D72), and a new leader waits out the old
  leases before it changes roles (D73).
- **The map** is all of these together, with a `map_epoch` that every
  change bumps. Daemons and clients hold the map and follow it by a
  watch stream of deltas. Every request carries the `map_epoch` its
  sender used; a receiver with a newer map answers `StaleMap{current}`
  and the sender fetches the delta.
- **What is batched.** A node failure changes every PG of that node in
  one entry. A rebalance after a map change is one entry per batch of
  PGs that finish backfill together, not one per PG.
- **Liveness** follows clustering.md D7: storage daemons send a heartbeat
  to the leader every 100 ms (D48), kept in the leader's memory. Only transitions
  go through Raft.
- **Peer reports** (D40). Members of a PG also send each other
  heartbeats. A node that hears nothing from a peer for the peer timeout
  reports it to the leader. The leader marks the peer down on reports
  from at least two nodes in different failure domains, or when its own
  heartbeat timeout expires, so it can tell a down node from one that is
  cut off from the leader alone, and one confused node can't mark healthy
  peers down.
- **Flap damping** (D149). A node marked down comes back up only after a
  hold: 2 s, doubled for each further down within 10 minutes, at most
  60 s. Health warns about a node that went down twice or more in that
  window. As built (I183), long work on a storage node's reactor
  (digests and backfill's comparison) also yields in batches, so it
  misses fewer pings in the first place. As built (I188), a large
  key–value table's digest is taken in parts with yields between; a
  storage node heartbeats only once every device of it is open, since a
  device still opening answers no pings; and a node whose own reactor
  was held counts its peers' silence afresh instead of reporting them,
  since the answers it missed were waiting for it.
- **Map lease** (D42). Each heartbeat answer renews the node's *map
  lease* (0.6 s by default, D48), which runs from when the node sent the
  heartbeat. A node without a valid lease serves no PG I/O (reads
  included) until it renews. The liveness timeout is at least 1.25 times
  the lease, so a node marked down has stopped serving by then. This also
  bounds how long a blocklist round (D24) can wait for a silent node.
- **Publishing a change** (D42). The leader keeps a working map and a
  published one. Before publishing a change, it fences every device that
  loses a role in it: the device acknowledges a `Fence` once its in-flight
  writes for those PGs are answered, or its last granted lease runs out
  (a device that doesn't answer gets no new lease meanwhile). Then the map
  is published and pushed to every live device; polling remains the
  fallback.
- **Blocklist.** An entry names a client *instance*: a random id each
  client process makes at start, bound to its certificate identity in
  `Hello`. A storage node refuses every request from a blocklisted
  instance once it holds that map epoch. Entries expire after an hour
  by default. A restarted client has a new instance id, so it is never
  held back by an entry for its previous life.

### 4.2 Volume metadata (the `.meta` pool)

- `.meta` is created at cluster init: replicated, `size 3`,
  `min_size 2`, failure domain `host`, 256 PGs by default, on the
  fastest device class. It is a pool like any other: the same
  placement, peering, recovery and scrub (07-placement.md §5, 09-replication.md §8).
- **Objects** in it are small (a few KiB) and versioned. A change is a
  compare-and-set on the object's version, ordered by its PG primary and
  replicated like any write (09-replication.md §8.1), so it is durable on every in-sync
  copy before it is acknowledged. Reads come from the primary under its
  read lease (09-replication.md §8.4), so they are linearizable.

| Object | Key | Holds |
|---|---|---|
| `Header` | volume id | pool, size, lineage, branch, parent `(branch, epoch)`, head epoch, snapshots (epoch, name, group, state), state, QoS, lease (holder instance, mode, expiry), space (head and snapshot-only bytes), background job progress |
| `Lineage` | lineage id | the branch tree: each branch's parent and its volume id |
| `Dir` | (pool, shard) | the volumes of a pool, in 64 shards by volume id hash |
| `Group` | group id | a consistency-group snapshot: members, state (`preparing`, `complete`) |

- **The header's primary serves the volume's control traffic.** The
  lease holder keeps a connection to it, renews the lease over it, and
  is told of changes (expand, snapshot) over it. The primary keeps the
  list of attached clients in memory; a notification that is lost is
  repeated on the next renewal, which returns the header's version.
- **Background jobs** of a volume (delete, snapshot merge, flatten,
  rollback cleanup) are run by the header's primary, which sends work to
  the data PGs and records progress in the header. A new primary after
  failover reads the header and resumes.
- **The management API** (15-operations.md §16) is served by every `hyperstor-meta`
  member, not only the leader. Volume operations are performed as a
  client of `.meta`; only map changes are forwarded to the leader.

### 4.3 Scale envelope

Sized for a first deployment of up to 256 storage nodes, 4,096 NVMe
namespaces, pools of up to 65,536 PGs and a million volumes.

| Kind of change | Where | Rate |
|---|---|---|
| map changes | Raft | follows hardware changes and failures; one entry per event |
| blocklist entries | Raft | forced takeovers only |
| heartbeats | leader memory | ten per node per second |
| volume operations, lease renewals | `.meta` PG primaries | one renewal message per second per holder and primary (D75), spread over 256 PGs on every storage node |
| block metadata | engines | follows I/O, always local |

## 9. Volume leases and fencing (D11, D23, D24)

- **As built** (01-implementation-notes.md I74–I78, I186): the holder is
  the header's `lease` attribute (the lease's identity, the holder's
  instance and the mode); the expiry lives in the primary's memory, so a
  renewal writes nothing, and a new primary gives the holder a full
  lease. The client library's `attach`, `take_over`, `preempt` and
  `detach` do the rest; storage nodes refuse a blocklisted instance by
  the instance its connection named at `Hello`, and a blocklisted lease
  by the identity its request names (D150).
- **Attaching** a volume takes a lease, recorded in the volume's header
  object by its PG primary (§4.2):
  - `rw` is exclusive, one holder;
  - `ro` is shared, and is allowed only on snapshots (D18).
- A lease names its identity (D150), its holder (client instance), mode
  and expiry, and its delegates (D33, D151). The holder
  renews it every second, for 4 s (D75), over its connection to the header's
  primary. Raft is not involved.
- **Every request carries the client's instance id**, bound to its
  identity at `Hello`, **and the identity of the lease it acts under**
  (`Target.lease`, D150). Storage nodes refuse requests from blocklisted
  instances (§4.1) and under blocklisted identities.
- **Release and hand-off.** A holder that detaches releases the lease.
  A planned move (a VM re-placed while stopped, a gateway hand-over)
  is a release by the old holder and an acquire by the new one: no
  blocklist round.
- **Forced takeover** (the holder died, was partitioned, or is being
  preempted by the operator) is the blocklist round of D24:
  1. the new holder asks the header primary for the lease; it is refused
     while the old lease hasn't expired;
  2. once it has, the new holder (or the API member acting for it) asks
     the metadata service to blocklist the old lease's identity and its
     delegates' (D150): one Raft write, map epoch `N`;
  3. the metadata service pushes the map and reports when every storage
     node has acknowledged epoch `N` or has lost its map lease (and so
     refuses all writes, 09-replication.md §8.4);
  4. the header primary, holding a map of epoch ≥ `N`, breaks the old
     lease and grants the new one.
- **The metadata service being down doesn't stop I/O**, and doesn't stop
  releases, hand-offs or renewals either, because none of them touch
  Raft. A forced takeover waits for it.
- **NVMe reservations** at the gateway (11-frontends.md §11.2) map onto this: acquire is
  an acquire, preempt is a forced takeover.
- **Effect on glidex.** storage.md S10 needs `node forget --fenced`
  before a `Lost` VM is re-placed, because the partitioned node may still
  be running it (clustering.md D8). For disks on hyperstor the blocklist
  round already ensures the old VM's writes fail: glidex's driver
  preempts the disks of a VM it re-places from a fenced node (D79), and
  a disk still held elsewhere when it starts a VM that runs nowhere
  (D153). Whether glidex relaxes
  `--fenced` for VMs whose disks are all on hyperstor is for
  clustering.md to decide. The VM's network identity is affected too, not
  only its disks.
- **Fencing one volume of a shared client** (D150, D151; built
  2026-10-29). A `hyperstor-vhost` serves every VM of its node through
  one client; fencing that client would cut off every disk it serves
  (01-implementation-notes.md I160). So:
  - **A lease identity per attached volume**, chosen by the client at
    attach and recorded in the header with the lease, alongside the
    instance it belongs to. It travels on every request under the lease,
    so connections stay shared: one per storage node, as before.
  - **Blocklist entries name an identity or an instance.** A storage node
    refuses a client's request whose identity, or whose connection's
    instance, is listed (`Fenced`, `Blocklisted`); preempting one volume
    lists its identity and its delegates', and fencing a whole client
    still lists the instance.
  - **D33's delegates are the same thing seen from the guest**: an
    identity that acts under a holder, renews nothing, and is fenced with
    it (D151). A snapshot's quiesce waits for each.
  - **Costs:** 8 bytes on every request, not measurable (02-benchmarks.md,
    2026-10-29), and a second lookup per request while the blocklist
    isn't empty (not measured); the blocklist grows by one entry per
    preempted lease and delegate, for an hour.
  - **Renewals per identity still batch per primary** (D75): each entry
    of a `LeaseRenew` names its identity.
