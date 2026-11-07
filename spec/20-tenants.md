# Tenancy

How tenants are represented, isolated, accounted and scheduled. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

Status: **draft.** D61 and D62 were settled with the owner on 2026-10-21,
realms (§24.8, D248–D253) on 2026-11-06, and HS15's counts, volume
tenants, host binding checks and realm roles (D256–D259) on 2026-11-07
([05-decision-log.md](05-decision-log.md)); the details below follow from them
and are open to review.

## 24. Tenancy

### 24.1 Goals

- **Policy above, enforcement below.** Who a tenant is, and what its
  people may do, is decided in glidex (projects, tenancy.md) or in
  `hyperstor-mgmt` (accounts, 19-administration.md §23.9). The cluster API
  and the data path only ever see tenant *ids*.
- **No tenant lookup on the I/O path.** A reactor learns a request's
  tenant once, when the volume is attached, and never consults a
  directory or a policy per request.
- **Pools stay shared.** A cluster has a few large pools; tenants share
  them. Capacity, EC width and PG counts are tuned once per pool, not
  once per tenant. A realm (§24.8) may keep pools to itself, but still
  shares the drives.

### 24.2 Where the tenant is recorded (D61)

A tenant is a property of the **containers** of the object layer, not
of every object:

| Container | Where the tenant id is stored | Set when |
|---|---|---|
| Volume lineage | the lineage object in `.meta` (06-metadata.md §4.2); every volume and clone of the lineage inherits it | the first volume of the lineage is created; optional, 0 (none, realm `default`) by default (D257) |
| S3 bucket | the bucket record in `hyperstor-kv` (14-s3.md §14.1) | bucket creation |
| Filesystem | the filesystem record in `hyperstor-kv` (00-filesystem.md F12) | filesystem creation |
| KV range | the range's owning filesystem or bucket; ranges used directly by an application carry their own | range creation |
| Cache-pool objects | the writer's tenant, from its lease | each write |

Why containers rather than the engine's object key (D34): the engine
never needs the tenant to find or check an object, so putting it in
every key would grow the index for nothing; the containers are where
space is counted and access is decided anyway. A tenant id is a 32-bit
number issued by meta; glidex maps its project ids to it (16-glidex.md
§17).

### 24.3 Isolation

Isolation is by **capability at attach time**, not by checks per I/O:

- A client attaches a volume under a lease (06-metadata.md §9) that names
  the tenant. A `host` identity (19-administration.md §23.8) may only
  attach volumes exported to it; a delegate (D33) inherits the holder's
  tenant.
- The lease, not the request, carries the tenant to the reactor: on the
  first request of a lease, the reactor caches the lease's tenant id
  with its open-volume state and keys its queues by it (§24.5).
- Under glidex a tenant's VM never reaches the storage network in the
  vhost path; in the GPU path its RDMA VF is on a per-tenant network or
  partition (15-operations.md §19). Hyperstor maps the VM's host identity
  to its tenant and trusts nothing else from the guest.
- A filesystem or bucket belongs to one tenant; sharing across tenants
  is done above hyperstor (glidex sharing, S3 presigned URLs), never by
  giving a second tenant the container.

Why: a check per I/O would cost a lookup on every reactor pass for
something that cannot change during a lease; the lease already fences
the writer (D42), so it is the right unit of trust.

### 24.4 Accounting, quotas and trash

- **Space** is counted per `(pool, tenant)` in meta from the counters
  the header objects already keep (10-volumes.md §10.5): head bytes,
  snapshot-only bytes, and bytes in the trash (19-administration.md
  §23.9.3). Buckets and filesystems add their own counters in
  `hyperstor-kv`: a bucket's objects and bytes are counted exactly, per
  range, by the writes themselves (D256, 25-kv-ranges.md §28.9).
- **Quotas** are per `(pool, tenant)`: a soft limit that warns and a
  hard limit that refuses new allocations the way D27's `full` does,
  while overwrites, deletes and snapshot deletes keep working. A tenant
  over its hard limit in one pool is unaffected in others.
- **Trash** keeps its per-pool grace period; the trashed bytes count
  against the tenant until purged, so a tenant can't exceed its quota
  by deleting and recreating.
- **Metering** (glidex metering.md) reads the per-tenant counters from
  the cluster API; nothing is sampled on the reactors for it.

Why per pool: the pools differ in media and protection, so a byte in
the NVMe replicated pool and a byte in the HDD EC pool are not the same
cost; one quota across pools would let a tenant fill the expensive one.

### 24.5 Fair share between tenants (D62)

Each reactor's queueing (10-volumes.md §10.5) gains a level:

1. **Class** first: `latency`, then `normal`, then `bulk` with its floor
   (D46).
2. Within a class, **tenants** share by weight (default 1; set by the
   operator, or by glidex from the project's plan).
3. Within a tenant, **volumes** share by weight, as before.

- QoS limits (IOPS and bandwidth with a burst) stay per volume and are
  checked before the queues; a tenant weight is a share, not a cap.
- A tenant with no request waiting takes no share; a tenant's share is
  never reserved idle.
- Backfill, recovery and scrub run as the cluster's own tenant, in
  `bulk`.

Why: with one level, a tenant with 500 sandboxes gets 500 shares
against a tenant with 5. Two levels keep the per-volume weights of D46
and make the tenant the unit that is fair, at the cost of one more
queue level, which the reactor keys by the cached tenant id (§24.3).

### 24.6 What is not tenancy

- **Encryption at rest** (15-operations.md §19) is keyed per lineage and
  can be used per tenant, but it is not the isolation mechanism; §24.3
  is.
- **Networks** are glidex's (host-topology.md §10); hyperstor only records
  which network a port serves (D31).
- **Sites** (21-federation.md §25) are orthogonal: a tenant may have
  containers at several sites, and a site serves several tenants.

### 24.7 Order of work

Milestone HS15 (17-milestones.md §21): the tenant id on containers, the
per-`(pool, tenant)` counters and quotas, the two-level queues, and the
glidex mapping. It needs snapshots and trash (HS4, AD3) for the
counters to be complete, and the priority classes (HS10b) for the
queue levels.

### 24.8 Realms (D248–D253)

Settled 2026-11-06 (05-decision-log.md points 211–216). Realms on S3
(D248–D250: the records, pool lists, placements per realm and a gateway
per realm) are built (2026-11-07, I288); the rest comes as below. A
**realm** groups tenants that should look like a separate
installation: their own pools, endpoints and administrators, on the
same cluster and, if the operator wants, the same drives. It answers
strict multi-tenancy without a cluster per customer.

**The model (D248).**

- Each tenant belongs to exactly one realm. A cluster starts with one
  realm, `default`, and every tenant is in it until an operator makes
  another; nobody needs to know realms exist until then.
- A realm id is a 32-bit number issued by meta, like a tenant id.
- A container's realm is its tenant's realm. Nothing new is stored on
  volumes, buckets, filesystems or KV ranges, and nothing in the
  engine's object key (D34) or on the I/O path changes. Tenant ids are
  unique in the cluster, so D243's bucket tables (tenant + name) already
  keep realms apart.
- Moving a tenant to another realm is an administrator's operation,
  refused while any of the tenant's containers is in a pool the new
  realm may not use (D249); likewise, a pool isn't taken from a realm
  while one of its tenants' containers is in it. Built for buckets and
  volumes (I298).

**What a realm limits**, the same way for every interface:

| | S3 | Volumes (ublk, vhost) | NFS and pNFS | KV ranges |
|---|---|---|---|---|
| Pools (D249) | placements | volume creation | exports | range creation |
| Who reaches it (D250, D251) | a gateway per realm, with its own domain and certificate | hosts by certificate, optionally bound to realms | a metadata server address per realm; client host certificates (D135) checked against it | the API, by role |
| Administration (D252) | realm-scoped administrators | same | same | same |

- **Pools (D249).** Each pool lists the realms that may use it; a pool
  with no list is open to every realm. A realm's tenants create
  containers only in its pools, and its S3 placements (D232) name only
  its pools. Pools of different realms may still share drives, since
  pools whose classes overlap share devices (D180). Why: a realm's data
  stays in pools its operator chose, while capacity stays pooled at the
  device level.
- **Endpoints (D250).** An S3 gateway serves one realm (`--realm`,
  default `default`), with its own domain and certificate, and a key of
  another realm's tenant doesn't exist to it: 403
  `InvalidAccessKeyId`, as for an unknown key (in the spirit of D244).
  An NFS metadata server address serves one realm's exports. Why: a
  leaked key or a misdirected client works only at its own realm's
  endpoint, and each realm can have its own names.
- **Hosts (D251).** A host is unbound by default: it may attach volumes
  of any realm, each attach authorized by capability as now (§24.3). An
  operator may bind a host to one or more realms; a bound host may then
  attach volumes and mount exports only of those realms, even with a
  valid capability. The storage node holding the volume's header checks
  the binding when it grants the lease, on the identity the host named
  at `Hello` (D258), so the I/O path gains nothing; until the data
  path's mutual TLS (D25) authenticates `Hello`, it stops mistakes, not
  a host that lies. Why: shared compute stays the default,
  and strict separation is one setting per host for those who need it.
- **Administration (D252).** Roles (19-administration.md §23.9.2) can be
  granted on a realm; a realm's administrators manage only its tenants,
  keys, placements, volumes and exports, and see nothing of other
  realms. Cluster administrators see everything; `hyperstorctl` and
  mgmt's UI take `--realm` and show the realm in lists. Enforced in
  mgmt, where people's roles live; cluster API certificates stay
  cluster-wide (D259). Pools, devices
  and hosts stay the cluster administrator's. Why: the operator of one
  realm can't make keys or attach volumes in another.
- **The name (D253).** "Realm", as in Kerberos: an administrative domain
  with its own keys. Rejected: *partition* (network partitions),
  *site* (21-federation.md §25), *zone* and *domain* (failure domains,
  D159), *namespace* (D243's bucket names), *account* and
  *organization* (read as a tenant or below). The documentation says
  that, unlike Ceph's multi-site realm, a hyperstor realm is within one
  cluster.

**Shared across realms:** drives, storage nodes, the map service and
pNFS data servers, which already check sealed stateids (D133).

**Not isolated:** performance. Realms on the same drives compete for
them; fair share (§24.5) has no realm level, so a realm that needs
performance isolation is given its own devices. Whether fair share
gains a realm level is open for the owner's review, with HS15.

**Order of work:** the realm records, pool lists and `--realm` on the S3
gateway came with D243 and D244, before S3 stage 4 (I288); host binding,
realm-scoped administrators and the NFS side come with HS15 and the NFS
work (22-nfs.md, still proposed).
