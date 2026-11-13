# Tenancy

How tenants are represented, isolated, accounted and scheduled. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

Status: **draft.** D61 and D62 were settled with the owner on 2026-10-21,
realms (§24.8, D248–D253) on 2026-11-06, and HS15's counts, volume
tenants, host binding checks, realm roles and hosts' names (D257–D261),
and how roles on a realm work (D262–D265) on 2026-11-07
([05-decision-log.md](05-decision-log.md)); the details below follow from them
and are open to review.

## 24. Tenancy

### 24.1 Goals

- **Policy above, enforcement below.** Who a tenant is, and what its
  people may do, is decided in glidex (tenants and projects, glidex
  19-tenancy.md, §24.9) or in
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
| Volume lineage | the lineage object in `.meta` (06-metadata.md §4.2); every volume and clone of the lineage inherits it. To be replaced by a project per volume, named in its header (§24.9, D300), with the lineage's tenant kept for volumes made before | the first volume of the lineage is created; optional, 0 (none, realm `default`) by default (D258) |
| S3 bucket | the bucket record in `hyperstor-kv` (14-s3.md §14.1) | bucket creation |
| Filesystem | the filesystem record in `hyperstor-kv` (00-filesystem.md F12) | filesystem creation |
| KV range | the range's owning filesystem or bucket; ranges used directly by an application carry their own | range creation |
| Cache-pool objects | the writer's tenant, from its lease | each write |

Why containers rather than the engine's object key (D34): the engine
never needs the tenant to find or check an object, so putting it in
every key would grow the index for nothing; the containers are where
space is counted and access is decided anyway. A tenant id is a 32-bit
number issued by meta. Glidex's tenants map to hyperstor's tenants,
and its projects to hyperstor's projects, a level below (§24.9, D299;
this replaces D61's mapping of glidex projects to tenants).

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
  range, by the writes themselves (D257, 25-kv-ranges.md §28.9).
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
   operator, or by glidex from the tenant's plan).
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
  volumes (I299).

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
  The map service gives a gateway the keys of a realm only if the
  gateway is bound to it by its certificate's name, as hosts are
  (D251), and `default`'s alone while it is bound to none; the installer
  binds a map service host's gateway to its `--s3-realm` itself
  (I370). An NFS metadata server address serves one realm's exports.
  Why: a leaked key or a misdirected client works only at its own realm's
  endpoint, and each realm can have its own names.
- **Hosts (D251).** A host is unbound by default: it may attach volumes
  of any realm, each attach authorized by capability as now (§24.3). An
  operator may bind a host to one or more realms; a bound host may then
  attach volumes and mount exports only of those realms, even with a
  valid capability. The storage node holding the volume's header checks
  the binding when it grants the lease, on the identity the host named
  at `Hello` (D259), so the I/O path gains nothing; until the data
  path's mutual TLS (D25) authenticates `Hello`, it stops mistakes, not
  a host that lies. A host names itself with `--host`, its hostname by
  default (D261). Built (I302). Why: shared compute stays the default,
  and strict separation is one setting per host for those who need it.
- **Administration (D252).** Roles (19-administration.md §23.9.2) can be
  granted on a realm, as `role@realm` beside cluster-wide roles (D263);
  a realm's administrators manage only its tenants' volumes, buckets
  and keys, its placements and exports, and its people's grants (D264),
  and see nothing of other realms nor of the cluster as a whole (D265),
  but follow the operations on its tenants' volumes, whoever started
  them (D283). Lists across several realms name each row's realm, and a
  creation in realms names its tenant or realm (D286).
  Cluster administrators see everything, and may act in one realm to
  see what its administrators see: `hyperstorctl --in-realm R` and the
  UI's realm picker. People's grants live in mgmt, which names the
  realms a call reaches; the map service, which knows each container's
  realm, applies them (D260, D262); cluster API certificates stay
  cluster-wide. Pools, devices and hosts stay the cluster
  administrator's. Built (I303, I340). Why: the operator of one realm can't
  make keys or attach volumes in another.
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

### 24.9 Projects, and glidex's directories and tenants (D299–D304)

Proposed 2026-11-12 (05-decision-log.md points 261–266): the model and
its mapping to glidex are the owner's choices; the transfer of a
project (D303) and per-volume ownership (D300) are drafted for review.
The records, projects on volumes, clones, buckets and keys, and roles
on a tenant or a project are built (I378, #360 stage 1), and so is the
transfer (§24.9.1, I379, stage 2), and so is the feed's part below
(I380–I384, stage 4); quotas and logins through glidex are not. It amends D61's mapping (a glidex project was to be a
hyperstor tenant) so that the same word means the same thing on both
sides.

**Three levels (D299).**

| glidex (19-tenancy.md) | hyperstor | What it holds here |
|---|---|---|
| directory | **realm** (§24.8) | the pools it may use (D249), its S3 gateway, domain and certificate (D250), host bindings (D251) |
| tenant | **tenant** | a bucket namespace (D243), S3 keys (D231), a cap on space, a fair share of I/O (§24.5) |
| project | **project** (new) | volumes and buckets, and their space, counted against a quota per pool (§24.4) |
| members of the cluster's operator tenant | cluster-wide roles (19-administration.md §23.9.2) | everything, as now |

- A glidex tenant is in exactly one directory (glidex D2), as a hyperstor
  tenant is in exactly one realm (D248): the two rules agree.
- Without glidex nothing changes for whoever ignores the new level:
  realms, tenants and projects are made by hand, and each tenant has a
  **default project** that takes every container that names none.
- Why: a realm already carries what a directory's organizations share
  (pools, an endpoint and its certificate), a hyperstor tenant already
  carries what an organization owns (bucket names, keys), and a project
  is where glidex sets quotas. Mapping glidex's project to hyperstor's
  tenant, as D61 planned, would have made "tenant" name two levels in
  one unified UI.

**Records (D300).**

| Record | Fields | Kept |
|---|---|---|
| Realm | as now, plus an optional link to a glidex directory (its issuer, which a directory keeps through a standby's promote) | the map |
| Tenant | `id`, `name`, `realm`, an optional link (directory, glidex tenant id); today a tenant is only a number | the map |
| Project | `id`, `tenant`, `name`, an optional glidex project id, `state` (`active`, `moving`) | the map |

- **Ids** are 32-bit, issued by the map service and unique in the
  cluster, projects as tenants (point 264). A container names its
  project alone, and the project names its tenant: one lookup, and a
  transfer (below) changes one record.
- **A tenant's default project** is a project like the others, made with
  the tenant.
- **Glidex links are by directory, not by glidex cluster.** Two glidex
  clusters may share one hyperstor cluster (glidex storage.md S24); the
  same glidex tenant must be the same hyperstor tenant on both.

**Ownership is per volume, not per lineage (D300).** A volume's header
names its project. D258 put the tenant on the lineage, which clones
inherit, but a glidex disk is a clone of its image's `golden` snapshot
(16-glidex.md §17): every disk made from one image is in the image's
lineage, whoever owns it. So:

- a volume, its snapshots and its trashed copies are its project's;
- a clone may be in another project, or another tenant's, than its
  parent; its own blocks count to its project, and the blocks it still
  reads from its parent count to the parent's;
- cloning from another project's snapshot needs the right to read that
  snapshot (glidex's identity has it for its images);
- a header that names no project (volumes made before) belongs to the
  default project of its lineage's tenant (D258), 0's being the
  cluster's own.

Buckets name their project in the bucket record (the table record's
appended bytes, 14-s3.md §14.9); their names stay per tenant (D243).
An S3 key stays a tenant's, and may name the project its new buckets go
to (the tenant's default project otherwise).

**Quotas and fair share.** Quotas are per `(pool, project)`, from
glidex's per-cluster project quotas, with a cap per `(pool, tenant)`,
from the operator's admission caps (glidex 19-tenancy.md §11); both are
checked where space is allocated (§24.4), never per I/O. Fair share
stays tenant, then volume (§24.5, D62): fairness matters most between
organizations, and a level for projects is one more queue on every
reactor. Projects get one later if one starves its siblings.

**Administration (D301).**

- A **realm's resources** (its pools, gateway and certificate, host
  bindings) stay the operator's, through cluster-wide roles, unless the
  operator grants `role@realm` (D263) to a directory's staff. A
  directory's owners have no rights on a cluster beyond what its
  operator gives them (glidex D7), and hyperstor is hardware.
- A **tenant's** administrators manage its projects' volumes and
  buckets, its keys and its people's grants, through a new grant
  `role@tenant` beside `role@realm` (D263). Under glidex, a
  `tenant-admin` is `storage-admin@tenant` and `security-admin@tenant`;
  a project's roles are `role@project` likewise, for volumes and
  buckets of that project only.
- mgmt names the tenants and projects a call reaches as it names realms
  (D262); the map service applies them the same way, answering a
  container outside them as one that doesn't exist.

**Gateways (D302).** One S3 gateway per realm, so per directory: a
university's labs that share a directory share its S3 domain. Tenants
are kept apart within it as now (their own bucket names and keys), and
directories by their gateways (D250, I370). A gateway serving several
realms waits until an operator hosts many directories (glidex
19-tenancy.md §1), which one gateway each would make costly.

**The operator's own storage (D304).** The operator tenant is a tenant
of its directory's realm like the others; its members' cluster-wide
roles are separate from it.

**From glidex.** mgmt trusts each glidex directory through glidex's
handshake (glidex 19-tenancy.md §12; 19-administration.md §23.12), and its feed
is the authority for the records above:

- a directory's first admitted tenant makes its realm;
- a tenant admitted makes a tenant; a project made, renamed or deleted
  in the feed does the same here (a project with containers isn't
  deleted: it is marked and reported, as glidex refuses deleting a
  tenant that still holds resources);
- a project whose tenant changes in the feed starts a transfer (below);
- glidex's datastore driver creates each disk in its project
  (`--project`), and the map service refuses a project outside the
  realms that glidex cluster's identity is bound to (D311), which a bare
  `--tenant` from glidex's machine identity can't ensure.

#### 24.9.1 Moving a project to another tenant (D303)

Built (I379), with these choices open for review: across realms while
hosts are bound, the project's volumes must be detached first; the caps
check is a hook until caps exist; the old tenant's keys are pointed at
its default project when the move starts; and a move can't be cancelled.

A project may move to another tenant, of the same realm or another
(an organization splitting, a lab moving to the university's own
tenant). It is a long operation (19-administration.md §23.3,
`Operation`), resumable after a failover, audited once and per bucket.

**Checks first; any failure refuses it, naming what to fix:**

| Check | Why |
|---|---|
| The new tenant's realm may use every pool the project's volumes and buckets are in (D249) | a realm's data stays in its operator's pools |
| Every host holding a lease on the project's volumes may attach in the new realm (D251) | a bound host would otherwise lose its disks at the next lease |
| No bucket of the project has a name the new tenant already holds | names are per tenant (D243); the operator renames first |
| The new tenant's caps per pool hold the project's space, unless a cluster administrator forces it | a transfer is no way around a cap; forced, the tenant is over its cap as a full pool is (§24.4) |

**Then, in order:**

1. The project becomes `moving`: new volumes and buckets in it are
   refused (`project_moving`, retry), its existing containers serve as
   before.
2. Each bucket moves in one atomic operation on store 1's root
   (25-kv-ranges.md §28.3): its `t` entry is written under the new
   tenant's name and the old one deleted, and its `i` entry renamed.
   The table id, its ranges and its rows don't change, so no data is
   copied. Gateways must stop resolving the old name before the old
   tenant's keys stop reaching it: how the root's cached entries are
   dropped on a gateway is to settle when built (a bump of the bucket
   record's epoch, checked as range epochs are, is the first candidate).
3. The project's record names the new tenant, in one Raft write.
   Volumes need nothing: their headers name the project. Storage nodes
   learn the project's tenant from the map, as they learn tenants'
   realms (D259); a reactor re-keys an open volume's fair-share queue
   at that map, and leases go on.
4. The project is `active` again. Keys of the old tenant that named it
   as their default project name the old tenant's default project
   instead.

A failover between steps resumes the operation where it stopped; until
it ends, the project's buckets may be under either tenant, each
consistent on its own.

**What doesn't move:** the project's clones in other projects stay
where they are, and may now read through another tenant's snapshot;
the transfer lists them and refuses nothing, since glidex's images are
shared across tenants that way by design. Space counters per tenant are
sums over projects, so they follow at once.

**Who:** a cluster administrator, always. Within one realm, someone
with `storage-admin` on both tenants too. Under glidex, mgmt starts it
when the feed moves the project. Across realms, only a cluster
administrator, since it moves data across a directory's boundary.
