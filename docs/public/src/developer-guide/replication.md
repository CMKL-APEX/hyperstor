# Placement and replication

## Placement

### Chunks and placement groups

A volume is cut into **chunks** (4 MiB by default). A chunk's name is its
volume's **lineage** (128 bits) and its index. A pool has a fixed,
power-of-two number of **placement groups** (PGs), and a chunk's PG is a
hash of its lineage and index, masked to the PG count. Hashes are
SplitMix64's finalizer for integers and FNV-1a for byte strings; their
values are pinned in tests, because changing them would move every
chunk.

**Why PGs:** a cluster with millions of chunks needs placement, recovery
and peering decisions for only thousands of PGs. **Why by lineage:** a
clone shares its parent's lineage, so a read that falls through to the
parent's chunks stays on the same devices (clones are planned).

### Weighted rendezvous hashing

Each PG is placed on devices by **weighted rendezvous hashing** (also
called highest random weight, HRW): every candidate gets a score from a
hash of the PG and the candidate, and the best scores win. Hyperstor
uses the exponential form, `score = −ln(u) / weight` with `u` from the
hash's top 53 bits, lowest score winning, so a candidate wins with
probability proportional to its weight.

Placement is hierarchical, over the pool's **failure domain**: it ranks
racks, then hosts within a rack, then devices within a host, each level
salted differently, with device weights summed up the tree. No two copies
share the failure domain (`device`, `host` or `rack`).

| Property | Why it matters |
|---|---|
| Only the PGs whose winner changed move when a device comes or goes | Rebalancing moves the minimum data |
| No tables to maintain: placement is a pure function of the map | Every node and client computes it the same way |
| Cost: 2.2 µs per PG at 1,024 devices and 3 copies | Recomputing a whole map is cheap |

For erasure-coded pools (planned), shard positions matter: survivors
keep their position and only vacated positions are refilled.

**Compared with CRUSH** (Ceph): CRUSH also hashes over a hierarchy, but
its `indep` mode for EC moved 2.1× the shards it needed to in
Hyperstor's comparison, and its buckets need tuning. Rendezvous hashing
is simpler, and minimal movement follows from its definition.

### Up, acting and primary

For each PG, placement gives the **up** set: where it wants the copies.
The **acting** set is the copies that hold every acknowledged write; the
first one is the **primary**. When up and acting differ (a device was
added, or one returns), the primary **backfills** the new copies while
writes go on, and once they are full a map change makes acting equal to
up with a new PG epoch.

## The replicated write

Hyperstor uses **primary-copy replication**: the client sends a write to
the PG's primary, which sends it to every other copy in the acting set
and answers the client only when **every** copy has made it durable.

- A PG with fewer than its pool's `min` copies refuses writes rather than
  accept them on fewer.
- Each PG's writes get a sequence number from the primary, and every copy
  applies them in that order.
- A copy being backfilled also receives live writes, so it doesn't fall
  behind while it fills.

**Why all copies, not a quorum:** with every acknowledged write on every
acting copy, any copy can become primary without a log comparison, and
reads need only the primary. The cost is that one slow copy slows the
PG's writes, which is why slow copies are dropped (below).

## The map service

`hyperstor-meta` holds the **cluster map** and is the only process that
changes it. Three or five members replicate it with **Raft** (through
openraft, with the map kept in an embedded redb store); the **leader**
grants leases, watches heartbeats and changes the map, and the
followers serve map reads and send everything else to the leader. The
map service is off the I/O path: clients and storage nodes compute
placement themselves.

| Timing | Value | Why |
|---|---|---|
| Raft heartbeat / election timeout | 50 ms / 200–400 ms | No lease is renewed without a leader, so an election must fit inside about half a lease |
| Storage node heartbeat | every 100 ms | |
| Map lease | 0.6 s | How long a cut-off node may go on serving |
| Marked down after | 0.8 s without a heartbeat | Must be at least 1.25 × the lease (below) |
| Devices marked out after | 600 s down | A reboot shouldn't move data |

### Map leases and fencing

A storage node serves a PG only while it holds a **map lease**, renewed by
each heartbeat answer. The node counts its lease from the moment it
*sent* the heartbeat, and the map service counts it from when it
answered, plus a quarter: so whatever the network delay, the node's lease
ends first, and the extra quarter covers clocks that run at slightly
different rates. No synchronized clocks are needed.

Before publishing a change that takes a role away from a device, the
leader **fences** it: the device stops accepting new work for those PGs
and answers once its writes in flight are done. A device that doesn't
answer gets no more leases, and the leader waits until its last lease has
run out (1.25 leases after the last grant). Only then does it publish the
new map and **push** it to every node.

**The result:** two primaries never accept writes for one PG, and the
lease is also what makes reads from the primary safe, with no separate
read leases. **The cost:** if the whole map service is down, I/O stops
after one lease, which is why it runs as three or five members.

A new Raft leader serves the map at once but waits 1.25 leases before
changing any role, because leases are kept in the old leader's memory
only. Replicating them would cost a Raft write per node per heartbeat.

## Failure detection

| Failure | Detected by | What happens |
|---|---|---|
| A node stops | No heartbeat for 0.8 s; or reports from two peers, which ping each other every 100 ms and report one silent for 0.3 s | Marked down; its PGs get new primaries and run degraded. After 600 s, its devices are marked out and their PGs placed elsewhere |
| A node is cut off from the map service | Its lease runs out | It stops serving before the map service gives its PGs away |
| A device returns an I/O error | Its reactor | The device answers `Unavailable` and is reported failed; the leader marks it out and places its PGs elsewhere at once |
| A copy is slow or unreachable from its primary | The primary: no answer within 2 s | The primary reports it; the leader drops it from that one PG if the reporter is the PG's current primary, `min` copies remain, and no peer has reported the primary itself; it is backfilled back later |
| Data is damaged | CRC32C on every read and on deep scrub | Repaired from another copy (below) |

**Why primaries report slow copies:** a hung disk on a live node, or a
network cut between two storage nodes only, is invisible to heartbeats,
and it would stall the PG's writes for as long as it lasted. Ignoring a
primary that peers report silent keeps a primary that is itself cut off
from dropping healthy copies.

## Peering and recovery by content digests

When a PG gets a new primary, the primary **peers** before it serves: it
asks every acting copy for its list of `(object, digest)` pairs
([digests](data-path.md#content-digests)), compares them with its own,
and pushes its own version, or a delete, of each object that differs.

**Why this is safe:** every acknowledged write is on every acting copy,
the new primary included. So any difference is a write that was never
acknowledged, and making every copy match the primary means no client
can read a value and later find it gone.

**Delta recovery and backfill** work the same way: a returning or new
copy lists its digests, and the primary sends only the objects that
differ, as whole objects (data, CRCs, attributes, key–value table), on the
same connection as live writes so a push never overtakes a write. A node
returning after 1 % of a volume changed received 2 MiB instead of
192 MiB.

**The alternative, a PG log** (Ceph's approach): keep the last
operations of each PG, and replay the missing ones to a returning copy.
It sends only what changed, but it is persisted state to write on every
operation, crash-test, and trim, and a copy away longer than the log
still needs a full comparison. Digests need no new state and cover every
case with one mechanism. Their cost is a scan of the PG's index entries
(not its data) on both sides. The log will be built only if measurements
on large PGs show the scan is too slow.

## Scrub

Each storage node compares, every 24 hours by default
(`hyperstord --scrub-hours`), the copies of each clean PG it leads,
spread evenly over the interval:

- **Light scrub** compares digests. A difference is looked at again after
  200 ms, so a write in flight isn't counted as damage.
- **Deep scrub**, on the first pass after a start and every seventh, has
  every copy first read all its blocks and check their CRCs, at most
  64 MiB/s per device. A damaged copy gets a digest that never matches,
  so it never counts toward a majority.
- **Repair by majority:** if the primary's copy is in a strict majority,
  it is pushed to the others; if another value is, the primary first
  pulls that copy, then pushes it. With no majority, nothing is changed:
  the object is reported, and cluster health warns.

When a client's read finds a damaged block on the primary, the primary
fetches the object from another copy, replaces its own and serves the
read, holding writes to that object meanwhile.

## How it is tested

| Test | What it shows |
|---|---|
| Engine crash test | Atomic operation lists across torn and reordered writes ([data path](data-path.md#crash-testing)) |
| `random_faults_lose_no_acknowledged_write` | With nodes stopping and restarting, pairs of nodes cut off from each other, and devices hanging, against a model in which a failed write may or may not have landed: after healing, every chunk and every copy matches. 20 seeds run in CI; a failing seed replays exactly |
| Scenario tests | Down and out, a failed device, a hung replica, a partition, a damaged primary, scrub outvoting the primary, a cut-off primary (fails without the lease check), map service leader failover |
| Mutation checks | Each fix's test fails when the fix is removed |

The fault-injection rig runs inside the test process (simulated
partitions and faulty devices), so it needs no root and replays exactly.
A multi-host rig with real network faults is not built.

## Known limits

- A slow *primary* is not handled: its clients wait for it.
- One I/O error fails the whole device; there is no error-rate threshold.
- Peer reports don't yet require reporters in different failure domains.
- Backfill and scrub are not throttled by priority class yet.
- Down times and scrub results are kept in the leader's memory; a new
  leader starts them again.
