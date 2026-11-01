# Replication, recovery and failures

Write and read paths, the PG log, primaries and epochs, recovery, backfill and scrub, erasure coding, and what each failure does. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 8. Replication and consistency

### 8.1 Write path (replicated pools)

1. The client sends `Write` to the PG primary with `map_epoch`,
   `pg_epoch`, the volume's lineage, branch and head epoch, the range,
   the data and the per-block CRCs. The connection carries the client's
   instance id.
2. The primary checks that:
   - it is primary for that `pg_epoch`;
   - the client instance isn't blocklisted (06-metadata.md §9);
   - the CRCs match the data.
3. It assigns the next PG sequence number and sends `Repl` to every
   other member of the in-sync set, in parallel, while writing locally.
4. Each member applies the operation in sequence order per object and
   answers once its log record is durable.
5. The primary acknowledges the client when every in-sync member has
   answered.

A member that doesn't answer within the operation timeout (default 5 s)
is reported to the metadata service, which drops it from the in-sync set
in a new `pg_epoch`. The write then completes with the remaining
members, as long as there are at least `min_size` of them. Below
`min_size` the PG blocks writes rather than acknowledge fewer copies.

### 8.2 Read path

- A read goes to the primary, which serves it while it holds a read
  lease (§8.4). The primary checks the CRCs. On a mismatch it reads the
  range from another in-sync member, answers with that, and repairs its
  own copy.
- **Snapshots and read-only attachments** (D18) may read from any member
  of the in-sync set. A snapshot's versions never change, so any copy is
  current. The client picks the closest member, then the least loaded.
- **As built** (HS2, 01-implementation-notes.md I104): the read lease is the
  map lease (D89). A primary whose copy fails its checksums fetches a
  member's copy, replaces its own with it, and serves the read from it.

### 8.3 The PG log

- Each PG keeps a log of its recent operations (sequence number, object,
  range, epoch), stored in the engine's log and trimmed to the newest
  10,000 operations or the last 30 minutes, whichever is more.
- A member that missed operations still covered by the log catches up by
  replaying them (delta recovery). Otherwise it is backfilled (§8.5).
- **As built** (HS2, 01-implementation-notes.md I57): no PG log yet. Delta
  recovery compares content digests instead: the returning member lists
  each object of the PG with a digest the engine computes from its index
  (size, each block's CRC, attributes, key–value table), and the primary
  sends only the objects whose digests differ, and deletes for objects
  it no longer has. It costs a scan of the PG's index entries on both
  sides, not of the data, and needs no new persisted state. The log is
  built only if G21 shows the scans too slow (D90).

### 8.4 Primaries, epochs and leases (D8, D42)

- Every change to a PG's `acting` set or primary is a new `pg_epoch`.
  Members accept operations only for the `pg_epoch` they have peered in.
- **Map lease** (D42, 06-metadata.md §4.1). A storage node serves a PG's
  reads and writes only while it holds a valid map lease and the PG isn't
  fenced. The map service publishes a change that takes a role away from
  a device only after fencing that device, by its acknowledgment or its
  lease running out. So a primary that serves a read is still the
  primary, and no write is acknowledged by a member that has been
  replaced.
- **As built** (HS2): leases and fencing as above, pushes as the fast
  path, polling every 200 ms as the fallback. Replica-granted read leases
  (below) are not built, by D89; they would let reads continue through an
  outage of the whole map service, which map leases don't (the amendment
  to D23).
- **Replica-granted read leases** (the original design, not built: D89). A primary may serve reads only while a lease from every in-sync
  member is valid. Members grant it with each `Repl` reply and each
  heartbeat, for 8 s, measured on their own monotonic clocks. A member
  that peers in a newer epoch first waits out the leases it has granted.
- **Peering.** When a PG enters a new `pg_epoch`, the new primary:
  1. asks each member of the previous in-sync set for its last sequence
     number and log head;
  2. takes the longest log among them as authoritative. Every
     acknowledged write is on every member of the old in-sync set, so the
     longest log has them all. Writes beyond what was acknowledged are
     kept; whole-operation atomicity (D22) means they apply completely or
     not at all;
  3. pushes missing operations to the members that lack them;
  4. reports the in-sync set to the metadata service and starts serving.

  If no member of the last in-sync set can be reached, the PG stays
  `down` and serves nothing, because a reachable member outside that set
  may lack acknowledged writes.
- **As built** (HS2, 01-implementation-notes.md I61): peering by digests,
  without a log. The new primary's first request for a PG at a new epoch
  compares every acting member's digests with its own and pushes its
  copy where they differ, before it serves.
- A storage node whose map lease (06-metadata.md §4.1) has run out stops accepting
  writes for its PGs until it renews, because it may have missed a new
  `pg_epoch` or a fence.

### 8.5 Recovery, backfill and scrub

- **Recovery** replays PG log entries to members that missed them. It
  runs as part of peering when the delta is small (by default, up to
  1,000 operations), and in the background otherwise.
- **Backfill** brings a new or long-absent member up by copying objects
  in key order, while new writes for already-copied keys are sent to it
  too. It is rate-limited (07-placement.md §5.3). The member joins the in-sync set when
  the scan finishes.
- **Scrub:**
  - *light*, daily: compare each member's object list and versions for
    a PG;
  - *deep*, weekly: read every block, check its CRC, and compare
    per-object digests across members.

  Damaged copies are repaired from a copy whose CRCs match, and an
  inconsistency that can't be decided is reported, not guessed at.
- **As built** (HS2, 01-implementation-notes.md I58): light scrub by the
  same digests, every `hyperstord --scrub-hours` (default 24) for each
  clean PG a device is primary of, spread evenly over the interval. A difference that persists after a
  second look is repaired from the primary's copy when a majority of
  the copies agree with it, and otherwise reported. The first pass after
  a start and every seventh are **deep** (I60): each member first reads
  every block of the PG, checking its CRC (08-engine.md §7.2), at most
  64 MiB/s per device, and reports an object that fails as damaged,
  which never counts toward a majority; the majority's copy replaces it.
  When the majority outvotes the primary's own copy, the primary fetches
  a majority member's copy first (I104). PGs left with unresolved
  objects are reported to the map service and shown in cluster health
  (I105).
- **Large objects** go to a member in parts (D146, I177): the object
  with the first part of its key–value table, then the rest, each part
  well under the frame limit, in order on one connection. A primary
  fetching a member's copy (a damaged or outvoted copy of its own)
  takes it in parts the same way, holding the object's lock so the
  member's copy can't change between parts; a copy left half replaced
  stays marked damaged, so reads go to repair (I182).

### 8.6 Erasure-coded pools (D6)

- A chunk is striped over the `k` data shards in stripe units of
  `chunk_size / k`, with `m` parity shards computed with Reed–Solomon (the
  pure-Rust `reed-solomon-simd` crate).
- **Writes:**
  1. the client sends the write to shard 0, the primary;
  2. the primary appends it to the PG's **write log**, an object held on
     shards 0, 1 and 2 with the same rules as a replicated write (§8.1),
     and acknowledges.
- **Full-stripe writes** skip the log. A write that covers one or more
  whole stripes of a chunk is encoded by the primary at once and written
  to all `k + m` shards as a new version, acknowledged when every shard
  has committed it. The primary gives it a sequence number after any
  logged writes to the same stripes; when it commits, those log entries
  are dropped from the index of logged ranges, so they are never
  destaged over it. The parts of the write outside whole stripes go
  through the log as usual.
- **Destage**, in the background:
  1. the primary takes the logged writes of a chunk, reads the parts of
     the stripes they don't cover, and computes parity;
  2. it writes all `k + m` fragments as a new version;
  3. once every shard has committed the new version, it trims the log.

  Destage runs when a stripe is fully covered, when the log passes half
  its size, or after 30 s.
- **Reads** check the primary's in-memory index of logged ranges first,
  then read the `k` data fragments. With shards missing, any `k` of the
  `k + m` reconstruct the data (a degraded read).
- **Back-pressure.** When the write log of a PG is full, new writes wait
  for destage.
- `min_size` for an EC pool is `k + 1` by default.

### 8.7 Cache pools (D45)

As built (01-implementation-notes.md I40, I222): the pool kind, copies,
loss without backfill, and "not found" on a miss. Each cache PG has a
generation in the map, which moves when a member joins it without the
data; a node that serves a cache PG at another generation than it last
did first empties what it holds of it, so a node that returns never
serves a copy older than a write made while it was away (D188). A split,
the moves of the PGs it makes (backfilled, D188) and a restart keep the
generation, and so the data. Expiry and eviction are not built yet.

- **Copies.** One by default, placed like a replicated pool of that size;
  writes are acknowledged when every copy has them, as in §8.1.
- **Expiry.** Every object has an expiry time (the pool's default, or set
  per object by the writer). Each primary drops expired objects in the
  background, and reads of an expired object answer "not found".
- **Loss.** A device or node that goes down takes its copies with it:
  no backfill, no recovery. The PG is placed again with an empty
  `acting` set, and its first write starts it afresh. Clients treat "not
  found" as a miss and recompute. The one move that backfills is a PG
  a split made going to its own devices (D188): nothing failed, and the
  data is there to copy.
- **Space.** Under pressure a cache pool evicts, by expiry first, then
  least recently read, instead of refusing writes; the D27 thresholds
  don't apply.
- **Source.** A cache pool can name a source cluster; a miss then
  fetches the immutable version from it instead of answering "not
  found" (D67, 21-federation.md §25.5).
- **Everything else is shared:** placement, map leases (D42), the engine,
  and every front end (S3 and S3 over RDMA, the native GPU library).

## 15. Failures

| Failure | What happens |
|---|---|
| A storage daemon crashes | Its PGs go to new primaries in a new `pg_epoch` after 10 s. When it restarts it replays its engine log, rejoins, and catches up by recovery. |
| A node is down for good | After `down_out_interval` its PGs are re-placed and backfilled (07-placement.md §5.3). |
| A device fails | It is marked `failed`; its PGs are re-placed at once, without waiting for `down_out_interval` (I102). |
| A replica is slow, or its primary can't reach it | Its primary reports it after the operation timeout (2 s) and it is dropped from that PG's acting set, then backfilled back (D91, I103). |
| A peer stops answering, or its host vanishes | A call to it gives up after 10 s (a ping after 200 ms) and its connection is dropped; TCP keepalive (10 s idle, then every 5 s, 3 probes) and a 30 s send timeout close a connection to a host that is gone, and a connect gives up after 3 s. Each daemon and client keeps one connection per peer, so concurrent callers share it (I184); a daemon pings its peers on a second connection, so a ping doesn't wait behind data (I187). Replication keeps its own timeouts (D91). |
| The primary is cut off from its replicas | Its peers report it and it is marked down; its map lease runs out, so it stops serving, and a new primary takes over (§8.4, D40). |
| The metadata service loses quorum | I/O, lease renewals, releases and ordinary volume operations continue on the current map. PG failover, rebalancing, blocklisting and so forced lease takeovers wait (D23). A PG that loses a member meanwhile can't shrink its in-sync set, so its writes stall until quorum is back. |
| The client's host dies | Its volume leases expire; the next attachment is a forced takeover with a blocklist round (06-metadata.md §9). |
| A `.meta` PG is unavailable | The volumes whose headers it holds can't be attached, snapshotted or renewed; I/O on already-attached ones continues, and their holders keep writing past lease expiry until the PG is back, since nobody else can be granted the lease meanwhile. |
| `hyperstor-vhost` crashes | VM I/O pauses until the service restarts and the hypervisor reconnects (11-frontends.md §11.1). |
| A checksum mismatch | The read is served from another copy and the bad copy is repaired (§8.2). |
