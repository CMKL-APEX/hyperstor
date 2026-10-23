# Storage node and local engine

The storage daemon's threads and devices, and the local engine: objects, writes, allocation, the key–value store. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 6. Storage daemon

### 6.1 Threads and cores

- `hyperstord` runs one reactor thread per reserved core (D7). Cores
  come from host-topology's reserved list, on the NUMA node of the
  NVMe devices and NIC they serve.
- A small tokio runtime (two threads) runs the control work: map watch,
  heartbeats, peering, recovery scheduling, the admin socket.
- **PG ownership.** Each PG belongs to one reactor, chosen by
  `hash(pool, pg) mod reactors`. That reactor handles all of the PG's
  requests, so nothing on the I/O path takes a lock.
- **Connections land on the owning reactor** (D39). Each reactor listens
  on its own port, and the map records each device's address, so a
  client connects straight to the reactor that owns the PGs it needs.
  There is no acceptor hand-off between threads.
- A client opens a connection to a reactor only when it first has a
  request for one of that reactor's PGs, and closes connections idle for
  a minute. `hyperstor-vhost` on a busy node ends up connected to most
  reactors in the cluster; an application with `libhyperstor-gpu` only
  to those holding its files.

### 6.2 Devices and queues

- **NVMe back end.** The engine opens each NVMe namespace through its
  generic character device (`/dev/ngXnY`) and submits reads, writes and
  deallocates with `uring_cmd` passthrough, on one io_uring per reactor.
  Polled queues are used where the driver has them (`nvme.poll_queues`);
  otherwise completions come by interrupt. The installer suggests the
  module option and never forces it (host-topology H12).
- **Block back end** (D29), for every other device:
  - io_uring `O_DIRECT` reads and writes on the block device (`/dev/sdX`,
    by its stable `/dev/disk/by-id` name), on the same per-reactor
    io_uring;
  - durability: a log write is `RWF_DSYNC`, which the kernel sends as FUA
    where the device supports it (`queue/fua`) and as a write plus flush
    otherwise. A device with a volatile write cache and no FUA is
    accepted with a warning, because every log write then costs a flush;
  - discard and write-zeroes through `IORING_OP_FALLOCATE` (punch hole
    and zero range on the block device), which the kernel turns into the
    device's own commands;
  - queue depth per reactor is capped from the device's
    `queue/nr_requests`, so one reactor can't starve the others on a SATA
    device's single queue.
- **Hard disks** (D29):
  - the log and the index go on a partition of a flash **log device**;
    one log device serves up to eight hard disks, each with its own
    partition;
  - allocation is in 64 KiB units, not 4 KiB, to keep extents long;
  - deferred small writes (§7.3) are applied in place in batches sorted
    by offset, so the disk sees mostly sequential writes;
  - losing a log device fails every hard disk that uses it. Placement
    treats them as one failure unit, which the host failure domain
    already covers.
- **Buffers.** Each reactor owns a buffer pool registered once with its
  io_uring (fixed buffers) and with its RDMA protection domain, so data
  moves between disk and network without a copy and without registration
  on the I/O path.

## 7. Local engine

### 7.1 Layout on a device

| Region | Contents |
|---|---|
| Superblock (two copies) | device id, node id, cluster id, format version, region offsets; for a hard disk, the log device's id and partition |
| Log | circular. Operation records (§7.3), deferred small writes, checkpoint markers |
| Index | an on-device copy-on-write B+tree of objects and their extents |
| Data | 4 KiB blocks, allocated in extents (64 KiB units on hard disks) |

On a hard disk, the log and index regions are on its partition of the
log device, which has its own superblock naming the data device (§6.2).

### 7.2 Objects

- An **object** (D34) is keyed by `(pool, pg, namespace, name,
  version)` and has:
  - **data**: sparse; its extent map lists the blocks it holds, and each
    extent carries one CRC32C per 4 KiB block;
  - **attributes**: a few small named values;
  - a **key–value table**: ordered keys and values of its own, stored in
    the index B+tree under the object's key prefix, so a range scan of
    them is a range scan of the tree.
- **Namespaces** and their names:

| Namespace | Name | Used by |
|---|---|---|
| `vol` | `(lineage, chunk_index, branch)` | volume chunk versions (below) |
| `ec` | the above plus the shard index | EC fragments (replication.md §8.6) |
| `meta` | object kind and id | `.meta` headers, lineages, directories, groups (metadata.md §4.2) |
| `kv` | range id | `hyperstor-kv` ranges (§7.5) |
| `s3` | `(bucket id, object id, part, chunk_index)` | S3 object data (s3.md §14) |
| `fs` | `(inode, chunk_index)` | file data (filesystem.md) |

  The tenant is not part of the key: it is a property of the lineage,
  bucket, filesystem or range the object belongs to (D61, tenants.md
  §24.2).

- **Operations** on one object come as a list applied atomically
  (D22): compare-and-set on the version, read a range, write a range,
  get, put and delete keys, delete a key range, set attributes. The
  primary applies the list in one log record, and replicas apply the
  same record.
- **Block maps switch representation** (D43). A chunk version holds its
  blocks as extent runs while it has few, and as a dense array of
  (physical block, checksum), 8 bytes per block, once a run list would
  cost more; a rewrite that makes it contiguous switches it back. Holes in
  a dense map hold a reserved block number. *Built* (implementation-notes.md
  I34).
- **Volume chunk versions.** For `vol`, the version is the volume's
  snapshot epoch (as built, I115: the branch is appended to the chunk's
  name, and branch 0 keeps the bare name):
  - `branch` is a volume within a lineage (D21). A new volume has its own
    lineage and branch 0; a clone gets a new branch in its parent's
    lineage;
  - `epoch` is the head epoch the version was created in (D10).
- **Reading block `b` of a chunk** for volume `(lineage, branch)` at epoch
  `e`: take the newest version of that branch with epoch ≤ `e` that holds
  `b`; if none does, continue in the parent branch at the epoch the clone
  was made from, and so on up the chain. A block nothing holds reads as
  zeros.
- EC fragments are objects too, with the shard index in the key (replication.md §8.6).
- The index is cached in memory per PG. The CRC arrays stay on disk in
  the index's leaf pages and are cached by use.

### 7.3 Writes (D22)

- **Into a version that only the head uses** (no snapshot holds it):
  - a write of 64 KiB or more, or into blocks the object doesn't hold
    yet, goes to a newly allocated extent. A log record then points the
    object's map at the new extent, with the new CRCs, and frees the old
    blocks;
  - on a hard disk only (D38), a smaller overwrite of blocks the object
    holds is written *into* the log record together with its CRCs. It is
    acknowledged when the record is durable and applied in place later,
    in the background. Reads check the not-yet-applied records first. On
    flash, every write goes to a new extent.
- **After a snapshot** (the head epoch is newer than the chunk's newest
  version): the write creates a new version object holding only the
  written blocks. Nothing is copied (D10).
- A write is durable when its log record is: one write with FUA, or a
  flush on devices without power-loss protection. The engine batches the
  records of concurrent writes into one log write.
- **Checkpoint** (as built, I33). A *fuzzy* checkpoint: it starts when a
  quarter of the log is used, records the log position `L`, and writes
  the index as a stream of objects, a slice per commit round, while
  commits continue. A directory listing the stream's extents is written
  last, then the superblock names the directory with the log's head at
  `L`. Recovery loads the stream and replays the log from `L`: every
  record sets state absolutely, so objects written to the stream at
  different moments converge. If the log reaches three quarters before
  the checkpoint is done, it is finished at once.
- **Paged** (as built, D105, I148). The stream is cut into pages by key
  range, about 128 KiB each. A change marks its page; a checkpoint
  writes only the pages marked since the last one began, cutting full
  pages and dropping empty ones, then a directory of every page. A page
  nothing changed still holds what replay from `L` needs, so checkpoint
  traffic follows the share of the index that changes.
- **Deferred writes in replay** (I149). A deferred write's record names
  its physical blocks. Replayed over a page written after it, a write
  whose object no longer maps the block (a later trim, or an absorb that
  moved it) is kept only if, once replay ends, some object maps the
  block with the write's checksum.

### 7.4 Allocation and space

- Free space is a bitmap of 4 KiB blocks on disk, with an in-memory tree
  of free extents built from it at start.
- Discard and write-zeroes from a client drop blocks from the head
  version. Blocks no version holds any more are freed, and the device is
  sent a deallocate.
- The engine reports used, free and allocated-per-lineage bytes to the
  daemon, which reports them to the metadata service with its heartbeat.
- **A full shard stays up** (as built, I162). Room for a whole checkpoint
  is kept free: a change that would take it is refused (no space), while
  one that only frees is let through. A checkpoint that still finds no
  room is abandoned, not fatal: the shard refuses what takes space until
  the room is back, then checkpoints at once.

### 7.5 The transactional key–value store (D35)

- **Ranges.** The keyspace of each KV store (one per user: the S3
  gateway, each filesystem) is split into ranges. A range is a `kv`
  object in a replicated pool on flash; its key–value table holds the
  range's keys. A range is split in two when it passes 64 MiB or a
  sustained write rate, and adjacent small ranges are merged.
- **Finding a range.** A root range, with a fixed id, maps key
  boundaries to range ids, like Bigtable's root table. Clients cache it
  and refresh an entry when a range answers `RangeMoved`.
- **Records.** Each key holds versions by commit timestamp, plus a lock
  column during a transaction, as in Percolator.
- **Transactions:**
  1. a transaction reads at a start timestamp from the oracle;
  2. at commit, it writes a lock and the new value to every key it
     changes (prewrite), with one key chosen as the primary lock;
  3. it gets a commit timestamp and commits the primary key; the
     transaction is committed at that moment;
  4. it then commits the other keys, in the background;
  5. a reader that meets a lock checks the primary: committed, it rolls
     the key forward; abandoned past its time-to-live, it rolls it back.

  A transaction whose keys are all in one range commits in one atomic
  operation on that range's object, with no locks.
- **Timestamp oracle.** The `hyperstor-meta` leader hands out timestamps
  from memory in batches. It writes a high-water mark through Raft,
  reserving the next three seconds of timestamps, so a new leader starts
  above anything handed out. That is one Raft write per three seconds,
  whatever the transaction rate.
- **Old versions** are removed by a garbage collector once they are
  older than the oldest running transaction, and at most after ten
  minutes.
