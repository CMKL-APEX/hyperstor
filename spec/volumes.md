# Volume operations

Client I/O, create/expand/delete, snapshots, clones, space and QoS. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 10. Volume operations

### 10.1 I/O in the client library

- The client splits each request at chunk boundaries, looks up each
  chunk's PG and primary in its map, and sends the pieces to the owning
  reactors in parallel.
- **Retries.** On `StaleMap`, `NotPrimary` or a lost connection, it
  fetches the map delta and resends. On `Blocklisted` it stops and
  reports the volume as lost to its user (the VM sees I/O errors).
- **Readahead.** The client detects sequential reads per volume and per
  queue, and reads ahead up to the next two chunks, cancelled when the
  pattern breaks. Dataset streaming and model loading are long
  sequential reads; readahead keeps every reactor that holds the next
  chunks busy. Pools for such data can use a larger chunk size (e.g.
  16 MiB, placement.md §5.1), which the client's readahead follows.
- **Flush** is a no-op that returns at once: a write is acknowledged only
  when every in-sync copy has persisted it, so there is nothing to flush.
  The device is advertised as write-through.

### 10.2 Create, expand, delete

- **Create** writes a `Header` object and adds the volume to its pool's
  `Dir` shard. Nothing is allocated until data is written (thin
  provisioning).
- **Expand** changes the size in the header; the header's primary tells
  the lease holder, which reports the new capacity to its device (virtio
  config change, NVMe namespace attribute notice).
- **Shrink** is refused.
- **Delete** requires no lease and no clones of the volume's snapshots.
  The header is marked `deleting`, and its primary has each data PG drop
  the volume's branch, then removes the directory entry and the header.

### 10.3 Snapshots

- **Taking one** (D10) of a volume:
  - if the volume is attached, the header's primary asks the lease
    holder to **quiesce**: stop issuing writes, wait for the outstanding
    ones to be acknowledged, then report;
  - in one compare-and-set on the header, the snapshot is recorded at the
    current head epoch `e`, and the head epoch becomes `e + 1`;
  - the holder is told the new epoch and resumes, sending writes with it;
  - if quiescing takes longer than 2 s, the snapshot is abandoned and
    the holder resumes.

  A holder that crashes between quiescing and resuming reads the head
  epoch from the header when it re-attaches.
- **A consistency group** of volumes, whose headers sit in different
  PGs, is taken by a coordinator (the API member serving the call):
  1. it writes a `Group` object in state `preparing`, listing the members;
  2. it quiesces every member's holder;
  3. only then does it take each member's snapshot as above, tagged with
     the group id;
  4. it marks the group `complete`.

  Every write to every member stops before any epoch moves, so the
  snapshot is crash-consistent across a VM's disks. If the coordinator
  fails midway, the group stays `preparing`; a background job on the
  `Group` object's primary deletes its snapshots and the group after a
  minute. An incomplete group is never listed as a snapshot.
- **Deleting** a snapshot marks it deleted. Each PG then merges, in the
  background, the versions only that snapshot held into the next newer
  version (keeping the blocks the newer one doesn't override) or frees
  them.
- **Rollback** makes the volume's head a new branch whose parent is the
  snapshot, and drops the old head branch in the background. It is
  instant, and needs the volume detached.
- **Limits:** 255 snapshots per volume. Clone chains of more than 16
  levels are refused; flatten first.

### 10.4 Clones and flatten

- **Clone** creates a volume with a new branch in the parent's lineage,
  whose parent is `(branch, snapshot epoch)`. It is instant and allocates
  nothing (D21).
- **Flatten** copies into the clone's branch every block it still reads
  from its parents, then clears the parent link. The lineage doesn't
  change, so placement stays the same and the copy stays on each PG's own
  devices.
- A snapshot with clones can't be deleted until its clones are flattened
  or deleted.

### 10.5 Space and QoS

- Each volume reports allocated bytes for its head and the bytes held
  only by its snapshots. Pools report used and free bytes, and enforce
  the D27 thresholds.
- **QoS:** a volume can have IOPS and bandwidth limits with a burst.
  They are enforced in two places:
  - the client library uses token buckets, which serve well-behaved
    clients;
  - each reactor queues requests per volume with weighted fair queuing,
    so a client that ignores its limits can only take its fair share of
    the devices.

  Provisioned IOPS for metering (metering.md §16) build on these
  limits.
