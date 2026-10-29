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
Each attach has a lease **identity** of its own, a random 64-bit number
recorded with the holder and carried by every request under the lease,
so one client's leases can be fenced one at a time (D150).

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
2. It adds the old lease's identity, and its delegates' (below), to the
   blocklist: one Raft write. The leader pushes the map and waits until every storage node
   has acknowledged it, or 1.25 map leases have passed (after which a
   node that didn't acknowledge has stopped serving).
3. It takes the lease.

From then on every storage node refuses requests under the old lease,
checked again just before a write is queued, so none of its writes lands
after the new holder's; the old holder's other volumes go on. Fencing a
whole client is the same with its instance, which every connection names
at `Hello`. `preempt` does the same for a live holder, for example when
glidex finds a disk still held by a node its VM no longer runs on: it
blocklists first, then waits out the lease. A forced delete preempts the
holder and deletes the volume while holding its lease. Blocklist entries
expire after an hour.

**Delegates** (D151) act under a holder's lease from another client:
the holder records their identities in the header, they renew nothing,
and they are blocklisted with the holder, before the holder detaches,
and when the holder removes one. A snapshot's quiesce waits for each.

**Why a blocklist, not a token checked on every write:** checking a
lease token against the header on every write adds work to every write
and state to every copy. Requests do carry the lease identity, but only
to look it up in the blocklist, which costs nothing on the write path
that could be measured, and one Raft write per takeover. Ceph uses the
same approach for RBD exclusive locks.

A forced takeover takes one volume lease, about 4.0 s; a preemption
about 3.8 s.

## Snapshots and clones

### Versions by epoch

A volume has a **head epoch**. Each chunk write creates or updates the
chunk's **version** at the head epoch: a separate engine object, named
by the chunk and the epoch. A **snapshot** is one compare-and-set on the
header that records the current epoch as a snapshot and moves the head
epoch on. Nothing is copied, and storage nodes take no part in the cut:
the next write to each chunk simply starts a new version, which holds
only the blocks written since (redirect-on-write).

A read at the head goes through **layers**: the chunk's versions from
newest to oldest, and the primary takes each block from the first version
that has it. A snapshot reads the versions up to its own epoch.

### Pausing the writer

If a client has the volume attached, its writes in flight must be on
the right side of the cut. The holder keeps one long-poll `Watch`
request open on its header's primary. To take a snapshot, the primary
tells it to quiesce: it pauses new writes, drains those in flight,
answers, and after the compare-and-set resumes at the new epoch. If it
doesn't drain within 2 s, the snapshot is abandoned (`Busy`) rather than
stall the volume. **Why a watch:** news carried by the 1 s lease
renewals could take most of a second to arrive; the watch costs
milliseconds plus the drain.

A **consistency group** holds every member's writer with one token,
snapshots each under its hold, then releases them all and marks the
group complete; on any failure it deletes the snapshots taken and
releases the holds.

### Clones, branches and rollback

A **clone** is a new volume on a new **branch** of the same lineage: its
chunk versions carry its branch number, and it reads through its own
branch first, then the snapshot's chain. Sharing the lineage keeps a
clone's chunks in the same PGs as its parent's, so a read that falls
through stays on the same devices. A lineage object records every branch
and what it reads, so a snapshot that something still reads can't be
deleted. **Flatten** copies into a clone what it still reads from its
parents, so it no longer depends on them.

A **rollback** to the newest snapshot of the head's branch is done in
place: the header moves the head epoch on and records the epochs since
the snapshot as skipped, so reads skip them at once, and their versions
are deleted afterwards. A rollback to an older snapshot makes a new
branch. **Why in place:** agent VMs are reset to a clean snapshot after
every task. A new branch per reset would add a level to the read chain
each time, and the chain is limited to 16 levels; in place, the chain
never grows.

### Deleting a snapshot

When nothing between them reads it, a snapshot's versions are merged
into the next newer ones with the engine's `Absorb`: the newer version
takes the blocks it lacks, by reference, and the older is deleted, in one
log record. No data is copied. Versions nothing reads are deleted
outright. **Why:** deleting snapshots is the hot background job when VMs
are reset after every task; this way it costs index work, not device
I/O.

## Planned

- Read-only attachments of snapshots, by several clients at once, with
  reads from any copy.
- A host-wide read cache in the client library, for versions that can't
  change.
