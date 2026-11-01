# Volumes, leases and the blocklist

## Volume metadata

Volume records are not in the map service. They are objects in a
replicated pool of their own, `.meta` (pool 0: three copies where there
are three nodes, 256 PGs):

| Object | Contents |
|---|---|
| Header `h/{id}` | The volume's pool, size, lineage, state and name, a version number, a creation token; while attached, its lease (identity, client, mode and label) and delegates; in the trash, when it went there |
| Directory `d/{pool}/{shard}` | The pool's volumes, in 64 shards by id, one key–value entry each |

Every change to a header is a **compare-and-set** on its version: an
operation list that asserts the version it read, then writes the new
header, decided by the header's PG primary under a lock on the key.
Creating a volume carries a token, so a retry after a lost answer is
recognised rather than doubled. Create writes the header and then the
directory entry; expand raises the size (never lowers it). A delete
moves the volume to the **trash**: the header is marked with the time,
and a trashed volume is neither listed nor attachable, but its data
stays until it is restored or purged (after its pool's grace period, a
day by default, or at once on request). A purge marks the header
*deleting*, removes the data, then the entry, then the header, so every
step can be resumed. In a full pool, creating volumes, clones and
snapshots is refused.

**Why not in Raft:** volume operations then scale with the number of
PGs, not with one Raft group, and a Raft write is needed only for map
changes. This is the pattern Ceph's RBD uses with its header objects.

## One writer per volume: leases

A client **attaches** a volume by taking its **lease**: the header's PG
primary records the holder in the header and keeps the expiry in memory.
Each attach has a lease **identity** of its own, a random 64-bit number
recorded with the holder and carried by every request under the lease,
so one client's leases can be fenced one at a time. A lease also records
a short **label** from its holder (`vhost:<node>` for a VM disk,
`hyperstorctl` for a tool), which a refused attach names, so the caller
can tell a VM's disk from a tool's hold.

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
holder and moves the volume to the trash while holding its lease. Blocklist entries
expire after an hour.

**Delegates** act under a holder's lease from another client:
the holder records their identities in the header, they renew nothing,
and they are blocklisted with the holder, before the holder detaches,
and when the holder removes one. A snapshot's quiesce waits for each.

**Why a blocklist, not a token checked on every write:** checking a
lease token against the header on every write adds work to every write
and state to every copy. Requests do carry the lease identity, but only
to look it up in the blocklist, which costs nothing on the write path
that could be measured, and one Raft write per takeover. Ceph uses the
same approach for RBD exclusive locks.

**Why an identity per lease, not per client:** one `hyperstor-vhost`
serves every VM of its node through one client. Fencing the client took
every disk off the node to move one. With an identity per lease, the
connection stays shared and only the lease taken is fenced. Only client
connections are checked: a primary forwards its client's request to its
copies, and a copy must not refuse a write its primary accepted.

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

## The host read cache

Every client keeps an LRU cache of 4 KiB blocks in memory (256 MiB by
default; 0 turns it off). It holds only blocks of versions that can no
longer change: a block read from a version other than the volume's head
(a snapshot's, or a clone's parent's). **Why no invalidation:** versions
are never rewritten, epochs never reused, and a merge only adds blocks a
version lacked, so an entry never goes stale. Entries are keyed by
lineage, the lineage's random **incarnation** (so a volume deleted and
made again never matches), branch, epoch, chunk and block.

- Each read answer names the version each block came from. An attached
  volume remembers that source per block until it writes the block or
  its header changes; a read that overlapped a write records nothing.
- When the cache holds blocks of the lineage and a read's sources aren't
  known, the client first asks the primary for them alone, without the
  data. If every block comes from a cached version, the read is served
  from memory; otherwise it is read as usual.

**Why ask first:** clones of one golden image then share what any of
them has read, for one small round trip per miss, asked only once the
lineage is cached. Not built yet: serving the cached part of a read that
also needs head blocks (such a read fetches everything), a second tier
on local NVMe, and measurements on hardware.

## Volumes as disks

Two front ends serve volumes through the client library, each on one
reactor thread with no other runtime.

| | `hyperstor-ublk` | `hyperstor-vhost` |
|---|---|---|
| Serves | One volume as `/dev/ublkbN`, through the kernel's ublk driver | Every VM disk of a compute node, over vhost-user-blk: a socket per volume, which Cloud Hypervisor or QEMU connects to |
| Queues | ublk's `uring_cmd`s, one task per tag | Each virtqueue a task woken by its kick eventfd; `EVENT_IDX`, indirect descriptors, multiqueue, flush, write-zeroes |
| Sectors | 4 KiB | 512-byte sectors on 4 KiB blocks (512e) |
| Device full | `ENOSPC` at once | The write is held and tried every second for up to 5 minutes, then `IOERR` |

**512e.** Guests and their boot loaders expect 512-byte sectors, while
a volume stores 4 KiB blocks with a CRC each. A read of part of a block
reads the whole block and returns the part. A write takes a lock on the
range of whole blocks it touches, and, when it starts or ends inside a
block, reads that block, merges and writes the span. Reads take no lock: a read racing a write may
see either, as on any disk. Without the lock, two writes into one
block's sectors lose one of them; a test checks it.

**Flush is answered at once:** a write is acknowledged only once every
copy has it durable, so the disk has no volatile cache to flush.

**Held writes.** virtio-blk has no "no space" status, and an I/O error
can turn a guest's filesystem read-only. So `hyperstor-vhost` tries a
write refused for space again every second, up to `--hold-full`
(60 s by default), while trash is purged or a device added. Each try
reads the data from guest memory again, which stays the guest's until
the request completes, so the usual path copies nothing more.

**Restarts.** Stopping `hyperstor-vhost` doesn't detach its volumes. A
state file in the runtime directory names its client and each volume's
lease identity and serial; the next start takes each lease back from
that identity (waiting out the lease, about 4 s) and opens the sockets
again, and the hypervisors reconnect. Requests the guest had in flight
are found in the hypervisor's **in-flight buffer** (a shared memory
region where each request is marked when taken and cleared when
answered) and run again. **Why the runtime directory:** after a reboot,
no VM uses the volumes, and their leases should lapse, not come back. A
volume another node took meanwhile is left to it. One taken while the
service runs is listed as `lost`, its I/O failing, until it is attached
afresh.

**For glidex.** Hyperstor's side of glidex's driver is built:
`hyperstor-vhost`'s control socket (one JSON request per line: attach,
with the node's name as the lease's label; detach; list); a management
identity enrolled with a single-use join token (`hyperstorctl enroll`);
`expand-volume`; and `hyperstorctl write-volume` and `read-volume` for
images (`--sparse` skips zero blocks). The label is what lets a driver
starting a VM preempt a disk still held by another node's
`hyperstor-vhost` after a crash or a failed detach, fencing only that
lease, while it waits for a tool's hold.

glidex's driver is built on these: a disk on a `hyperstor` datastore is
a volume, blank, filled from an image, or a linked clone of the image
volume's `golden` snapshot, grown to the disk's size; the VM's node
stages it through its `hyperstor-vhost`, and Cloud Hypervisor or QEMU
connects over vhost-user-blk. A deleted disk's clone waits in the trash,
keeping the image volume it reads from; glidex deletes leftover image
volumes once their image is gone and the clone is purged. Metering of
hyperstor disks in glidex is still to come.

## Planned

- Read-only attachments of snapshots, by several clients at once, with
  reads from any copy.
- Discard and write-zeroes that allocate nothing: discard can't simply
  unmap the head's blocks, or a read would fall through to a snapshot's,
  so it needs a "zeroed" marker in the head version.
- Telling a running guest its disk grew (virtio's config-change event);
  today it sees the new size at its next attach.
- In `hyperstor-vhost`: queues on several reactors, one per NUMA node,
  adaptive polling, and zero copy from guest memory.
- A delegate's identity handed to a guest over virtio-vsock.
