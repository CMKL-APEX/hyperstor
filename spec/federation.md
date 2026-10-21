# Sites and federation

Several clusters as one system: sites as a failure domain, asynchronous
replication of immutable data, a global namespace with local
performance, and the federation view. Part of the hyperstor design;
sections keep their numbers across documents, so `§N` in one document is
the same section everywhere (see [README.md](README.md)).

Status: **draft.** D63–D65, D67 and D68 were settled with the owner on
2026-10-21 ([decision-log.md](decision-log.md)); D66's queueing was the
owner's choice and its merge rule is open to review. Host checks G19
and G20 are open.

## 25. Sites and federation

### 25.1 Principle

**The PG layer never spans a wide-area link.** Everything multi-site is
built above it: a *site* inside one cluster is a failure domain with
campus latency; anything farther is another cluster, and clusters are
joined by replication of immutable versions, by a key–value namespace
with a home per range, and by caches.

Why: the data path is tuned for one-second failover (D48) and
sub-millisecond writes; a lease fence over a 50 ms link would either
lengthen every lease or produce false detections, and a synchronous
copy across sites would make every write pay the WAN. The target
workloads (workloads.md) share *immutable* data across sites (weights,
datasets, checkpoints, images) and need *local* latency for the rest.
Active-active writes to one file or volume from two sites are not a
target and are not designed in.

```
   site A (cluster A)                       site B (cluster B)
 ┌───────────────────────┐   sync (D64)    ┌───────────────────────┐
 │ pools, PGs, leases    │ ◄─────────────► │ pools, PGs, leases    │
 │ hyperstor-kv ranges:  │   KV forward    │ hyperstor-kv ranges:  │
 │   home here · replica │ ◄─────────────► │   home here · replica │
 │ cache pool ←source── B│   read-through  │ cache pool ←source── A│
 │ hyperstor-sync        │                 │ hyperstor-sync        │
 └──────────▲────────────┘                 └──────────▲────────────┘
            │ cluster API                             │ cluster API
 ┌──────────┴─────────────────────────────────────────┴────────────┐
 │ federation: hyperstor-mgmt (standalone) or glidex (D68)          │
 │   clusters, trust, sync policies, range homes, the federated view │
 └──────────────────────────────────────────────────────────────────┘
```

### 25.2 Sites inside a cluster (D63)

- `site` is a failure-domain level above `rack` (placement.md §5.2). A
  pool whose level is `site` places each copy or EC shard in a
  different site.
- **Limits.** A cluster's sites must be within **1 ms round-trip** of
  each other and share one management network. The installer's
  preflight (administration.md §23.4.1) measures the round-trip to the
  existing meta members and refuses a join beyond the limit; G19 sets
  the limit from measurement.
- The meta members (D2) are spread over the sites, so a site loss keeps
  a majority where there are three or more sites; with two sites, the
  operator places the third member where the split should win.
- Clients prefer the copy in their own site for reads when the pool
  allows any-replica reads (D18, replication.md §8.2); writes go to the
  primary wherever it is.

Why 1 ms: at that distance a write to a remote copy adds about what a
flush costs on a drive without power-loss protection, and the D48
heartbeat (100 ms) is unaffected. Beyond it the lease math and write
latency both degrade, and the next sections apply instead.

### 25.3 Replication of immutable versions (D64)

`hyperstor-sync` is a daemon per cluster, on the control path (D3), that
copies **versions that can no longer change** from one cluster to
another:

| What | Unit | Incremental by |
|---|---|---|
| Volume snapshots | a lineage's snapshot versions | the chunk versions between two snapshot epochs (volumes.md §10.3's diff) |
| S3 objects | an object version | the bucket index's version keys |
| Images (golden snapshots) | a lineage | as snapshots |
| Filesystem snapshots (later) | a filesystem snapshot | as the filesystem's diff |

- **Pull-based.** The receiving cluster's sync daemon asks the source
  for versions newer than what it holds; a transfer is resumable at
  chunk granularity because every chunk carries its checksums
  (engine.md §7.2) and the copy is verified on arrival.
- **Policies** are per lineage or bucket: which cluster(s) receive it,
  how far behind they may be (a target lag, reported as health), and
  whether the copy is a full replica or a cache source (§25.5).
- **A replica is identical bytes** with the same checksums and the same
  snapshot epochs, so a volume cloned from a replicated snapshot at
  site B has the same lineage id and the same content as one cloned at
  A. Head versions are never replicated: they change.
- **Traffic** runs as `bulk` (D46) on the storage network's WAN side,
  over the D5 protocol's TCP transport with TLS (D25), between sync
  daemons only; storage reactors are not exposed across clusters.
- **Trust** between clusters is an exchange of roots (administration.md
  §23.8): each cluster's root is pinned in the other through the
  federation (§25.6), and sync daemons present their cluster's node
  certificate.

Why pull: the receiver knows what it has and what it may hold; a push
would need the source to track every receiver's state. Why immutable
only: no conflicts, no ordering, no fencing across the link, and the
copy serves as a cache source for free.

### 25.4 The global namespace (D65, D66)

The federated namespace is **`hyperstor-kv` with a home per range**
(engine.md §7.5), which gives the filesystem (filesystem.md) and S3's
bucket indexes (s3.md §14.1) one namespace across clusters without
stretching any PG:

- **Home.** A range is homed in the cluster where it was created
  (D65). The home holds the authoritative copy and runs the range's
  transactions with its own timestamp oracle. An operator (or the
  federation's policy) can **move** a range's home: an `Operation`
  (administration.md §23.3) that copies the range, drains writes at the
  old home, and switches, with a short unavailability of that range.
- **Replicas.** Other clusters hold **read-through replicas** of ranges
  they use, tagged with the home's version: a read is served locally
  and marked with its lag; a client that needs fresh data asks for the
  home's version and pays one round-trip.
- **Writes forward to the home.** A transaction that touches ranges
  with different homes is split by home; each part commits at its home,
  and the parts are ordered by the two-phase commit of D35 with the
  primary lock at the first range's home.
- **Partition** (D66, the owner's choice): when the home is
  unreachable, a site **queues writes locally** in a per-range log and
  replays them, in order, when the home returns. Rules:
  - a queued write is visible at once to readers at the queuing site
    (they see their own writes) and nowhere else until replayed;
  - at replay, a key written at both sites in the meantime takes the
    **later write by timestamp** (last writer wins); sites keep their
    clocks within the D48 bound, which G20 checks;
  - each front end names the operations it allows to queue: S3 PUT,
    DELETE and multipart completion (new object ids, so a replay never
    damages data); filesystem creates and data commits inside a
    directory; **not** rename or unlink across directories, which the
    filesystem refuses while its home is unreachable, because a replay
    could leave a child without a parent;
  - the queue has a size limit per range; beyond it, writes fail as if
    the home were down without queueing.

  The merge rule is open to review: last-writer-wins is simple and
  enough for S3 and for files with one writer per site, and the
  per-front-end allowlist is what keeps the invariants; a finer rule
  (per-key merge functions) can come later without changing the queue.
- **Locks** (filesystem.md F7) are home-site only: a lock on a range
  whose home is unreachable can't be taken.

Why a home per range rather than Raft across sites: metadata
transactions stay local to one oracle and one cluster, which is what
keeps `create`, `rename` and `complete multipart` at local latency;
the WAN is paid only by writes from away and by readers who ask for
freshness.

### 25.5 Caches with a source (D67)

A cache pool (D45, replication.md §8.7) can name a **source**: a
cluster that holds the data as a replica (§25.3). A miss on a sourced
cache pool fetches the object from the source through the local sync
daemon, stores it, and answers; a miss on an unsourced pool answers
"not found" as before.

- Only **immutable versions** are sourced: snapshot chunk versions,
  S3 object versions, filesystem snapshot data. A head version is never
  fetched.
- Fetches run as `latency` for the request that missed and prefetch
  the rest of the chunk's neighbours as `bulk`, so a model's weights
  stream in once and then serve locally; the host read cache (D32) sits
  in front of it on each compute node.
- Expiry and eviction are the local pool's; a fetched object is just
  another cache object.

Why: this is how a site serves another site's model weights, datasets
and images with local latency and without replicating everything in
advance; the sync daemon already has the transport and the trust.

### 25.6 Federation (D68)

The set of clusters, their trust, the sync policies, range homes and
the combined view live in the **management layer**, not in any cluster
API:

| Mode | Where | What it does |
|---|---|---|
| Standalone | `hyperstor-mgmt` (administration.md §23.7), one instance or several that share the same `hyperstor-kv` records | registers clusters and pins their roots; holds sync policies and range-home policies; shows the federated view (capacity, health, lag per policy); forwards each action to the right cluster's API |
| Under glidex | glidex's control plane, as regions or sites of one glidex deployment | the same, with glidex's tenancy and Cedar deciding who may do it |

- Each cluster's API (administration.md §23.5) stays single-cluster; it
  gains the resources the federation drives: `Peer` (a trusted cluster),
  `SyncPolicy`, `RangeHome`, and their operations.
- A cluster works without the federation: policies already applied
  keep running; only changes to them need it.
- `hyperstor-sync` daemons talk to each other with their own cluster's
  identity and the peer root pinned by the federation.

Why: WAN state in the Raft members (D2) would make every cluster depend
on every other's reachability for its own map; the management layer
is already stateless and restartable (D50), and under glidex the people
and tenants are there anyway.

### 25.7 Order of work

| Milestone | Delivers | Depends on |
|---|---|---|
| HS16 | `site` as a failure domain with the preflight limit; `hyperstor-sync` for snapshots and S3 objects with policies and lag health; cross-cluster trust; G19 | HS4, HS12, AD3 |
| HS17 | Range homes, read-through replicas, forwarded writes, the partition queue with the per-front-end allowlist; cache-pool sources; the federated view; G20 | HS11, HS16 |
