# Placement and replication

## Placement

### Chunks and placement groups

A volume is cut into **chunks** (4 MiB by default). A chunk's name is its
volume's **lineage** (128 bits) and its index. A pool has a power-of-two
number of **placement groups** (PGs), and a chunk's PG is a hash of its
lineage and index, masked to the PG count. Other objects (volume headers,
the audit log) are placed by a hash of their name. Hashes are
SplitMix64's finalizer for integers and FNV-1a for byte strings; their
values are pinned in tests, because changing them would move every
chunk.

**Why PGs:** a cluster with millions of chunks needs placement, recovery
and peering decisions for only thousands of PGs. **Why by lineage:** a
clone shares its parent's lineage, so a read that falls through to the
parent's chunks stays on the same devices.

### Weighted rendezvous hashing

Each PG is placed on devices by **weighted rendezvous hashing** (also
called highest random weight, HRW): every candidate gets a score from a
hash of the PG and the candidate, and the best scores win. Hyperstor
uses the exponential form, `score = −ln(u) / weight` with `u` from the
hash's top 53 bits, lowest score winning, so a candidate wins with
probability proportional to its weight.

Placement is hierarchical, over the pool's **failure domain** (`device`,
`host` or `rack`): it picks distinct domains at that level, then a host
within each chosen rack, then a device within each host, each level
salted differently, with device weights summed up the tree. No two copies
share a failure domain.

**Which devices a pool may use.** A device is a candidate when it is
`in`, has a weight above 0, and the pool may use it: its **class** is in
the pool's class set (or the set is empty, meaning every device), or it
was added to the pool by hand, and it wasn't removed from it. A device's
class is its kind (`nvme`, `ssd`, `hdd`, `file`), which the node reports,
unless an operator labels it. A pool whose classes mix hard disks and
flash is refused unless forced (or a cache pool), and `.meta` never uses
hard disks. **Why:** a replicated write waits for its slowest copy.

**Weights are capacities.** A device's weight is its data region in TiB,
set by the map service's leader from the device's own space reports. A
new weight is committed only when the size differs by more than 1 %, and
changes to devices already weighted wait until every live device has
reported, then go in one map change: so an upgraded cluster, or devices
all of one size, move nothing. A device added to a running node, or a
joining host, starts at weight 0, so nothing is placed on a guess, and
gets its weight from its first report. An operator may fix a weight
(`hyperstorctl weight`), which the map keeps until set back to `auto`.

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

**Backfill is bounded.** A reactor's backfills to one target keep at
most 16 MiB of pushes on the wire together; a backfill that finds the
budget spent waits for its answers, then for room. A device past its
`backfillfull` ratio gets no new backfill. **Why the bound:** when an
empty host rejoined a three-host cluster, every primary started a
backfill for each of its PGs at once, and pings and replicated writes
queued behind hundreds of MiB. The budget bounds what a write waits
behind; pings now have a connection of their own as well.

**Stray copies.** A device that is in neither `up` nor `acting` of a PG
it still holds objects of deletes its copy once the PG is **clean**
(`acting` is `up` and every member is live) and has stayed so, at one PG
epoch, for 60 s. It deletes through its commit queue, 64 objects at a
time, checking before each batch that nothing changed. A PG the map lacks
is left alone, and a node cut off from the map service deletes nothing.
**Why:** every rebalance, drain or weight change used to leave the old
copies for ever, so space never came back. The grace keeps a copy
through a quick change back; waiting for clean means nothing a recovery
could need is deleted.

### PG counts grow by splitting

A pool's PG count doubles as devices are added, without moving data
twice. Because a chunk's PG is its hash masked to the count, doubling
the count sends the chunks of PG `p` either to `p` or to its new sibling
`p + n` (for `n` PGs before), and both start on the same devices.

1. **Split in place.** The map marks a few clean PGs of the pool at a time
   (4). Each member of a marked PG pauses its I/O (`Unavailable`, which
   clients retry), moves the objects that now belong to the sibling to
   their new keys with the engine's `Take` (metadata only, no data
   copied), and reports. Once every member has, the map **flips** the PG:
   the sibling appears with the same acting and up sets, and clients route
   by the key they send. The last flip doubles the PG count.
2. **Move gradually.** A **placement count** stays at the old count, so a
   sibling is placed as its parent and nothing moves yet. The leader then
   raises it a few PGs at a time (4), each time every PG of the pool is
   clean; each step moves exactly the PGs it passes, under the backfill
   budget.

The **autoscaler** on the map service's leader doubles a pool's count
when its devices call for at least twice its PGs (a target of about 100
PGs per device, counting each copy), one pool at a time, never `.meta`,
and only while every PG is clean. An operator can split by hand
(`hyperstorctl pool set --pgs`) or turn it off per pool. Merges are not
built. **Why this way:** each pause is short and bounded, a slow device
holds up only its own PGs, and the slow part, moving data, is spread out.
Ceph splits the same way (`pg_num`, then `pgp_num`).

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
- Each request names the volume lease it acts under; storage nodes refuse
  one whose lease or client is blocklisted ([volumes](volumes.md#the-blocklist-and-forced-takeovers)).
  Only client connections are checked: a primary decides for its copies,
  so a write it accepted is never refused by a copy.

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

Storage nodes send heartbeats, space reports and scrub results to the
leader only. A follower asked for the cluster's status gets what only
the leader knows (when each node was last heard from, each device's
space, flapping nodes, unrepaired PGs) from the leader, kept for 1 s,
so every member answers in full without clients needing to find the
leader.

| Timing | Value | Why |
|---|---|---|
| Raft heartbeat / election timeout | 50 ms / 200–400 ms | No lease is renewed without a leader, so an election must fit inside about half a lease |
| Storage node heartbeat | every 100 ms, once every device is open | A node is up only when it can answer for all its devices |
| Device space report | every 5 s | Weights, pool space and fullness; apart from the heartbeat so a slow answer never delays one |
| Map lease | 0.6 s | How long a cut-off node may go on serving |
| Marked down after | 0.8 s without a heartbeat | Must be at least 1.25 × the lease (below) |
| Held down after going down | 2 s, doubled for each further down within 10 min, at most 60 s | Flap damping (below) |
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
| A node keeps going down and coming back | The map service's record of its downs | Held down longer each time (below); health warns that it is flapping |
| A node is cut off from the map service | Its lease runs out | It stops serving before the map service gives its PGs away |
| A device returns an I/O error, or a commit takes over 30 s | Its reactor | The device answers `Unavailable` and is reported failed; the leader marks it out and places its PGs elsewhere at once. The node stays up |
| A device is slow to read | Its reactor: a read past 2 s | The read is served from another copy; three in a row fail the device ([data path](data-path.md#waiting-for-the-device)) |
| A copy is slow or unreachable from its primary | The primary: no answer within 2 s | The primary reports it; the leader drops it from that one PG if the reporter is the PG's current primary, `min` copies remain, and no peer has reported the primary itself; it is backfilled back later |
| Data is damaged | CRC32C on every read and on deep scrub | Repaired from another copy (below) |

**Why primaries report slow copies:** a hung disk on a live node, or a
network cut between two storage nodes only, is invisible to heartbeats,
and it would stall the PG's writes for as long as it lasted. Ignoring a
primary that peers report silent keeps a primary that is itself cut off
from dropping healthy copies.

### Peer pings

Each device pings the nodes it shares a PG with every 100 ms and reports
one that stays silent for 300 ms; the
map service marks a node down on reports from two different nodes.

- **Over UDP.** A device answers 12-byte pings on UDP at its TCP port's
  number. Each ping carries a random 64-bit nonce that its answer echoes;
  an answer matching no ping in flight is ignored. A peer is pinged over
  TCP as well (on a connection of its own, apart from data) until it first
  answers over UDP; one that never does stays on TCP, logged once.
  **Why:** on a busy network a few packets in a thousand are lost. On TCP
  a lost ping waits at least 200 ms for its retransmission, and the pings
  behind it wait with it, which made single 300–400 ms silences under
  load. A lost UDP ping costs only its own slot.
- **A held reactor reports no one.** A watcher whose 100 ms tick comes
  100 ms late or more was held itself; it counts every peer's silence
  afresh, since its peers' answers were waiting for it to read them.
- **A starting node isn't reported.** A node heartbeats only once all its
  devices are open, and the map service ignores reports about a node it
  hasn't had a heartbeat from since it took the lead. A device still
  opening answers no pings, so those reports said only that it was
  starting; the heartbeat timeout still catches one that never comes up.

### Flap damping

A node marked down comes back up only after a **hold**: 2 s, doubled for
each further down within 10 minutes, at most 60 s; ten quiet minutes
start it afresh. A node still goes down, and loses its lease, at once.
Health warns about a node that went down twice or more in the window.
**Why:** a node whose reactor stalled past the peer timeout went down and
up about 150 times a minute on a test cluster; each change bumped the
map and shrank or refilled PGs until some fell below `min`. Holding it
down turns a flapping node into a down one, which the cluster handles.

## When devices fill

Three cluster-wide ratios, kept in the map (`hyperstorctl full-ratios`),
give each device a **fullness** from its space reports:

| Ratio | Default | What happens past it |
|---|---|---|
| `nearfull` | 85 % | Health warns |
| `backfillfull` | 90 % | The device gets no new backfill; the PG waits, degraded, until space is freed or added. Cleared a percent below, so a device at the line doesn't change the map with every report |
| `full` | 95 % | The device refuses changes that take space (a new object, a write to blocks an object doesn't hold yet); overwrites, deletes, and an existing object's keys and attributes go on. Health is an error |

A pool's fullness is that of the fullest device it may use. In a full
pool, new volumes, clones and snapshots are refused, and a pool set to
purge its trash when full (`hyperstorctl trash settings --on-full purge`)
purges its oldest item, one at a time, waiting
for the space freed to be counted before choosing the next. Clients see
"no space": `hyperstor-ublk` answers `ENOSPC` at once, while
`hyperstor-vhost` holds the write and tries again every second for up to
5 minutes (`--hold-full`), since virtio-blk has no "no space" status and an
I/O error can turn a guest's filesystem read-only.

**Why overwrites go on:** leases and volume headers are updated in place,
and refusing them would stop every volume on the device, not only the
ones that grow. **Why `backfillfull`:** recovering a lost device must not
fill the survivors before their own writes stop.

**A pool's usable space** is computed by the leader from the devices'
reports under its failure-domain rule: no domain counts for more than
one copy of each PG. With as many hosts as copies, the smallest host
bounds the pool, which a plain sum would overstate.

## Cache pools

A cache pool holds data that can be recomputed: one copy by default, no
backfill after a failure, and a read of a missing object answers "not
found". Each cache PG has a **generation** in the map, which moves when a
member joins without the data; a device serving a cache PG at another
generation than it last did first empties its copy, so a returning node
never serves data older than a write made while it was away. A PG all of
whose copies are on down nodes waits for them for the pool's grace (60 s
by default, `hyperstorctl pool set --cache-grace`), so a node back in
time keeps its data: reads meanwhile answer as an empty device would,
and a write ends the wait, moving the PG at once. Expiry and
eviction are not built yet.

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
same connection as live writes so a push never overtakes a write. An
object too large for one frame (a key–value table of hundreds of MiB)
is pushed, or fetched, in parts. A node returning after 1 % of a volume
changed received 2 MiB instead of 192 MiB.

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
| Scenario tests | Down and out, a failed device, a hung replica, a partition, a damaged primary, scrub outvoting the primary, a cut-off primary (fails without the lease check), map service leader failover, slow commits and reads that hold no reactor, a hung device failed after the I/O timeout, lost TCP pings, stray copies deleted |
| End-to-end tests | Installed clusters from the built binaries: an empty node refilled without flapping, a returning node going down once, swapped device files refused, a device added and removed, a pool's PGs split in place, a full pool purging its trash |
| Mutation checks | Each fix's test fails when the fix is removed |

The fault-injection rig runs inside the test process (simulated
partitions and faulty devices), so it needs no root and replays exactly.
A multi-host rig with real network faults is not built.

## Known limits

- A slow *primary* is handled only for reads: a read past the read
  timeout is served by another copy, but writes wait for it.
- One I/O error fails the whole device; there is no error-rate threshold.
- Peer reports don't yet require reporters in different failure domains.
- Backfill is bounded by bytes in flight, not by a rate or a priority
  class, and the bound is per reactor and target, not per link.
- Down times, flap holds, space reports and scrub results are kept in
  the leader's memory; a new leader starts them again.
