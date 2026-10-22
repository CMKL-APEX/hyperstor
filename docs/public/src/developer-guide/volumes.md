# Volumes, leases and the blocklist

## Volume metadata

Volume records are not in the map service. They are objects in a
replicated pool of their own, `.meta` (pool 0: three copies where there
are three nodes, 256 PGs):

| Object | Contents |
|---|---|
| Header `h/{id}` | The volume's pool, size, lineage, state and name, a version number, a creation token, and while attached, its lease holder |
| Directory `d/{pool}/{shard}` | The pool's volumes, in 64 shards by id, one key–value entry each |

Every change to a header is a **compare-and-set** on its version: an
operation list that asserts the version it read, then writes the new
header, decided by the header's PG primary under a lock on the key.
Creating a volume carries a token, so a retry after a lost answer is
recognised rather than doubled. Create writes the header and then the
directory entry; delete marks the header *deleting*, removes the data,
then the entry, then the header, so every step can be resumed.

**Why not in Raft:** volume operations then scale with the number of
PGs, not with one Raft group, and a Raft write is needed only for map
changes. This is the pattern Ceph's RBD uses with its header objects.

## One writer per volume: leases

A client **attaches** a volume by taking its **lease**: the header's PG
primary records the holder in the header and keeps the expiry in memory.

| Timing | Value |
|---|---|
| Renewal | every 1 s, in one message per holder and primary for all its volumes |
| Expiry | 4 s after the last renewal |

A renewal writes nothing, so leases cost no metadata writes. A new
primary for a header's PG gives every holder a full lease, so a primary
failure never takes a volume away from a live holder.

## The blocklist and forced takeovers

If a holder dies or is cut off, another client must not start writing
while the old one might still have writes in flight. Hyperstor uses a
**blocklist** in the cluster map:

1. `take_over` waits until the old holder's lease has run out (up to
   4 s). A holder that is still renewing is left alone.
2. It adds the old holder's client instance to the blocklist: one Raft
   write. The leader pushes the map and waits until every storage node
   has acknowledged it, or 1.25 map leases have passed (after which a
   node that didn't acknowledge has stopped serving).
3. It takes the lease.

From then on every storage node refuses requests from the old instance,
checked again just before a write is queued, so none of its writes lands
after the new holder's. `preempt` does the same for a live holder, for
example when glidex moves a VM off a node it has fenced: it blocklists
first, then waits out the lease. A forced delete preempts the holder and
deletes the volume while holding its lease. Blocklist entries expire
after an hour.

**Why a blocklist, not a token on every write:** checking a lease token
on every write adds work to every write and state to every copy. The
blocklist costs nothing on the write path that could be measured, and
one Raft write per takeover. Ceph uses the same approach for RBD
exclusive locks.

A forced takeover takes one volume lease, about 4.0 s; a preemption
about 3.8 s.

## Planned

- Read-only attachments, which come with snapshots.
- Snapshots and clones by redirect-on-write: a snapshot only bumps an
  epoch, and chunks are copied on their next write.
