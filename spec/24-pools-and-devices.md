# Pools and devices at run time

How devices join and leave a running cluster, how pools are made, changed
and deleted, and how a pool's capacity follows the devices it may use.
Part of the hyperstor design; sections keep their numbers across
documents, so `§N` in one document is the same section everywhere (see
[README.md](README.md)).

Status: **settled, not yet built** (2026-10-31): D163, D164 and
D167–D176 (points 116–127 of [05-decision-log.md](05-decision-log.md)).
Built so far, on `feature/elastic-pools`: the identity check of
§27.2.2 (I203, also on `main`), adding a device (§27.2.1, I204),
named device ids (§27.2.3, I205), the format rule (§27.2.2,
I206) and the installer's `--device` on map service hosts (§27.2.1,
I207). The work
lands on the branch `feature/elastic-pools`, which merges into `main`
once the required features are in.

## 27. Pools and devices at run time

### 27.1 Today, and the goal

What is fixed today, and where:

| What | Today | Where |
|---|---|---|
| A node's devices | Set when the node joins: the map service adds a host with all its addresses at once and refuses the same host with another set | `hyperstor-meta` `add_node` |
| A device's id | `node × 1000 + i`, `i` its position in the node's addresses and in `hyperstord`'s `--shard` order; since I203 the device's superblock must name that id and the map's cluster, or the node refuses it | `hyperstor-map` `add_node`, `hyperstord` |
| Formatting | `hyperstord` formats any device without hyperstor's superblock magic, without asking (D157 refuses newer or damaged hyperstor devices, not foreign ones) | `hyperstor-daemon` `ShardState::open` |
| The installer's devices | Files under `--data-dir`, rewritten on every run (the daily renewal too) | `hyperstor-install` |
| Device classes | Every device is `nvme`; nothing sets another class | `hyperstor-map` |
| Pools | Made from the cluster file when the map service first starts; no call makes, changes or deletes one | `ClusterMap::add_pool` |
| A pool's devices | One optional class; no class means every device | `hyperstor-placement` |
| PG count | Fixed when the pool is made | 07-placement.md §5.1 |

The goal, as settled:

- **A pool's capacity follows its devices (D163).** Adding a device of a
  class a pool uses grows the pool; removing one shrinks it, after its
  data has moved. Data rebalances, and the PG count grows with the
  devices. A pool's copies (`size`, `min`) or EC layout never change by
  themselves.
- **A pool uses a set of classes (D164).** It places on any device whose
  class is in its set, weighted by capacity (D158), under its failure
  domain rule. A pool's space is that of the devices it may use (D159).

What stays as it is: weighted rendezvous over failure domains (§5.2),
backfill with its budget (§5.3, I187), map changes through Raft, and
device weights from capacity (D158).

### 27.2 Devices

#### 27.2.1 Adding a device

- **`AddDevice {node, address, class}`** in the cluster API appends one
  device to a running node: the next free id on that node, weight 0
  (D158: nothing is placed by a guess), the class given or the one its
  kind implies (§27.2.4). `hyperstorctl device add --node N --address A
  [--class C]`; role `operate` in mgmt's Cedar policy; audited.
- A storage-only host has no administrator certificate, so a join of a
  node that is already in the map may also append: the same addresses
  first, in the same order, then the new ones. Anything else is still
  refused.
- **The installer** records each device in `install.conf`, so a later
  run (the daily renewal too) keeps it:
  - `--device PATH` (repeatable): a block device, by its
    `/dev/disk/by-id/` name, never `/dev/nvmeXnY`, whose number can
    change at boot;
  - files as now (`--reactors N`, `--data-dir`, `--size-gib`);
  - each device gets its own reactor and port, its own `--shard`, and a
    udev rule that gives the `hyperstor` user that device only (and its
    NVMe generic device, for passthrough), never the `disk` group.
- Adding a device to a running node: the installer formats it (§27.2.2),
  registers it (`AddDevice`, or the join above), rewrites `node-N.env`
  and restarts `hyperstord` (D170; a reactor started without a restart comes later). The node's other devices keep
  their data; a restart is a short down, held as any other (D149).
- **Data moves once it runs.** The new device reports its space as its
  reactor starts; the map service weights it by capacity at once (D158)
  and placement moves PGs onto it, under the backfill budget.

#### 27.2.2 Formatting and identity

- **`hyperstord` formats files only.** A block device is formatted by
  the installer, and only when it carries no other signature: a
  partition table (MBR, GPT), an LVM label, a Ceph BlueStore label, md
  RAID, or a filesystem's magic. `--force-format` overrides it, per
  device. Why: a renumbered `/dev/nvmeXnY`, or a typo, would otherwise
  format a disk in use; the pilot host has four Ceph disks beside the
  free one.
- **A device names itself** (built, I203). Formatting writes the
  device's id and the map's cluster id in the device superblock; before
  opening a device, `hyperstord` reads its superblock alone and refuses
  one whose ids aren't the ones its `--shard` expects
  (`EngineError::NotThisDevice`), writing nothing. A device from builds
  before it, with cluster 0, has the cluster recorded once. Why: two
  hyperstor devices whose names swap at boot would otherwise serve each
  other's data, under the other device's id, which scrub reports only
  after the fact.
- With explicit ids (§27.2.3), the expected id is the one `--shard`
  names instead of the position's; the check stays the same.

#### 27.2.3 Device ids

Built (I205), with one addition: a reactor checks that the map has its
device on its node at its address before opening it, and stops the node
otherwise.

- `--shard` names the device's id: `--shard ID@ADDR=PATH:SIZE`. Without
  `ID@`, the position gives it, as today. The map keeps each device's id
  and address; nothing derives one from the other. Why: removing a
  device that isn't the last would otherwise renumber the ones after it
  (D169).
- A device's id is never reused, as a pool's isn't (§27.3.4).

#### 27.2.4 Classes

Built (I209), with D178 (a device whose class no operator set takes its
kind, which replaced every device's `nvme`) and D179 (`file` mixes with
any kind). The map service writes kinds into the map only once every
running node has reported, so older nodes can still read it.

- A device's class is a label. The default comes from its kind, which
  the node reports: `nvme` (an NVMe namespace), `ssd` (other flash),
  `hdd` (rotational), `file` (a file on a filesystem).
- `hyperstorctl device class --device D --class C` relabels a device;
  `--class` on `device add` and on the installer's `--device` sets it
  first. Labels are short names (`nvme-qlc`, `archive`).
- The map keeps each class's **kind**: a label takes the kind of the
  first device given it, and a device of another kind can't take it.
  Why: §27.4 refuses pools that mix hard disks and flash, which needs to
  know what a label is.
- Relabelling a device moves the PGs of pools that no longer may use it,
  and of pools that now may, as a weight change does.

#### 27.2.5 Removing a device

Built (I224, I226, D186).

1. `hyperstorctl device remove --device D` begins a long operation
   (19-administration.md §23): the device is taken out (`out`), and its
   PGs move to the other devices of their pools.
2. It is refused at the start (D175) when, without the device, a pool
   that uses it would have fewer failure domains than copies (or EC
   shards), or its data would pass the pool's `full` threshold (D27).
   `--force` overrides the first: such a pool runs degraded, with fewer
   copies than its `size`, but never fewer than its `min`, below which
   it would stop taking writes. The space check isn't forceable: data
   that fits nowhere else can't leave the device. `--dry-run` shows the
   PGs that would move, the pools that would be degraded and the space
   left.
3. When no PG lists the device in `up` or `acting`, the map service
   removes it from the map, and the installer (or the operator) removes
   its `--shard` and restarts the node. A node whose last device goes is
   removed with it. Neither id is given again (D186).
4. The device's data is left as it is; wiping it is a separate step.

### 27.3 Pools

#### 27.3.1 Making a pool

Built (I211).

- **`CreatePool`**: a name; the kind (`replicated` with `size` and `min`,
  or `cache` with `copies`, D45; EC once it is served); the chunk size;
  the failure domain (`device`, `host`, `rack`); the class set; the
  thresholds (D27). The PG count is the autoscaler's (§27.5), unless
  given. `hyperstorctl pool create`; role `storage-admin`; audited.
- It is refused when the devices of its classes span fewer failure
  domains than its copies, and it warns when they hold less than one
  chunk per PG.
- A pool's id is the next unused one. Ids of deleted pools are never
  reused (§27.3.4).

#### 27.3.2 Changing a pool

`hyperstorctl pool set` changes:

| Field | Effect |
|---|---|
| Class set | PGs move to the devices of the new set; refused when the new set has fewer domains than copies |
| Thresholds | Only accounting |
| Failure domain | PGs move; refused as above |
| PG count | Only up, by doubling (§27.5, D174) |
| Name | Only the name |

Built (I213): the class set, failure domain and name, any of them in one
call; thresholds wait for them to exist, and the PG count for stage 5.

Not changed: the kind, copies, `min`, the chunk size (D9: fixed per pool;
chunks are named by index). A pool with other copies is a new pool and a
copy of the volumes.

#### 27.3.3 Space

- A pool's total and free space are those of the devices in its class
  set, under its failure domain rule (D159), so they change as devices
  come and go, without a step of their own.
- Pools that share classes share devices, and so share space: each pool
  reports the free space of all of them, and filling one fills the
  others'. `hyperstorctl pools` and the UI say which classes each pool
  uses, so overlap is visible.

#### 27.3.4 Deleting a pool

D181 amends this: the pool leaves the map at once, its id kept, and
devices delete its data as they delete stray copies (D177); there is no
"deleting" state and the call isn't an operation. Built (I214); `--force`
too (I215), as an operation that runs again from what is left.

- **`DeletePool`** is a long operation, with step-up and `--dry-run`
  (19-administration.md §23). It is refused while the pool holds a
  volume, a snapshot or anything in trash, or while a bucket uses it
  (D176, D198). `--force` deletes those first, each one audited,
  after `--dry-run` has listed them. glidex datastores
  name a pool, so glidex must stop using one first (16-glidex.md §17).
- Then: the pool is marked deleting in the map (clients get
  `not_found`), each device deletes its objects in the pool's PGs (the
  engine lists a PG and deletes objects already), and the pool leaves
  the map. The map keeps the ids of deleted pools, so none is reused.
  Why: clients, the host read cache (§11) and copies on devices that
  were down meanwhile name data by pool id.
- A device that returns after a pool's deletion deletes what it still
  holds of it, as it drops a cache pool's stale copy now.

### 27.4 Class sets

D180 amends this: a pool uses the devices of its classes, plus devices
added to it and less devices removed from it (`hyperstorctl pool
add-device`, `pool remove-device`), so one pool can have a device
another pool of the same class doesn't. A device serves every pool that
may use it, sharing its space. Built (I212): an added device is one not
of the pool's classes, a removed device one of them; each edit is
checked as a class set is (failure domains, kinds), and `hyperstorctl
pools` shows them after the classes (`nvme+3001-2000`). Why per pool:
a class is shared by every pool that names it, so moving a device
between classes moves it in all of them.

Built (I210), with `SetPool` for classes only until stage 3. Only
`SetPool` checks a set's domains and kinds; relabelling a device later
isn't checked against the pools that use its class.

- A pool's class set is a list of labels. A device is eligible when its
  class is in the list, and the pool's weight on it is its capacity, as
  for every pool (D158).
- **An empty set** (every device) stays as the meaning for pools made
  before class sets, so nothing moves on upgrade. A new pool names its
  classes (D171). Why: with devices of several kinds, "every
  device" mixes them silently, and a device added later of a new class
  would join pools nobody chose for it.
- **Hard disks and flash don't mix** in one pool (§5.2): a class set
  whose kinds include `hdd` and any flash kind is refused, unless the
  pool is a cache pool (D172). `--force` allows it: the call warns, the
  audit record says it was forced, and `hyperstorctl pools` and the UI
  mark the pool. Why: a replicated write waits for its slowest copy
  (§8.1); an operator may still accept that, knowingly.
- `.meta` uses flash only: its set is every flash class, and it follows
  them as they change (§4.2).
- A PG may hold copies on devices of different classes in the set; its
  writes are as fast as the slowest. Weights are per device, not per
  class; a preference between classes of a set would be a later change.

### 27.5 PG count

Built (I218–I221): the engine's move, a device's split, the protocol
and `pool set --pgs`, the placement count rising, and the autoscaler
(D184). Clients switch to a split PG's sibling PG by PG (D187). Merges
(stage 6) come when needed.

- **Target:** about 100 PGs per device the pool may use (§5.1), counting
  each copy. A pool on 6 devices with 3 copies wants about 200 PGs, so
  256; the same pool on 60 devices, 2,048.
- **Splitting** (D173). `pg_of` reduces the hash by `mod 2^n`
  (`hyperstor-placement`), so when the count doubles the chunks of PG
  `p` go to `p` or `p + 2^n`, on the devices that already hold `p`:
  1. The pool gets a new count, and **a placement count** that stays at
     the old one: PG `p + 2^n` is placed where `p` is.
  2. Each device that holds `p` moves the objects that now belong to
     `p + 2^n` to their new keys (`ObjKey` holds the PG). Blocks move
     without a copy (`Absorb` does it today); attributes and key–value
     tables need a rename in the engine. A device does it in one commit
     per object and reports the PG split.
  3. When every member of a PG has split it, the split PG serves at a
     new PG epoch; requests that used the old count get `StaleMap` and
     retry with the new one (as for any map change).
  4. The placement count then rises to the new count a few PGs at a
     time, and the children move under the backfill budget, as any
     rebalance.
  Why: no data moves twice, and the slow part (moving data) is spread
  and bounded. Ceph splits the same way (`pg_num`, then `pgp_num`).
- **Merging** (halving) is the reverse and more delicate: two PGs' members
  must first agree, which needs both on the same devices. It comes
  later, and only by the operator (D174).
- **Who decides** (D174, D184): the map service doubles a pool's count
  when its devices call for at least twice its PGs (about 100 per
  device, counting copies), at most one pool at a time, and never while
  the cluster is degraded. `pool set --pgs N` does it by hand;
  `--autoscale off` stops it per pool.

### 27.6 Safety and operations

- Every change here is a map change through Raft, audited, and named in
  the map service's log.
- Device removal, pool deletion and splits are long operations
  (19-administration.md §23): progress, `--dry-run`, cancel where it
  makes sense (a removal stops and the device comes back in).
- A change that moves data goes under the backfill budget (I187) and
  the flap damping of D149; the autoscaler waits for a clean cluster.
- Volume leases are untouched: a split changes PG epochs, which the
  lease holder's writes already follow (06-metadata.md §9, D75).
- glidex's datastore (16-glidex.md §17) reads a pool's space and needs no
  change; `gxctl datastore` later gets `pool create` and `device add`
  through mgmt (§23).

### 27.7 What changes

| Part | Change |
|---|---|
| Map | Settable classes and a class's kind; pools' class sets; a placement count per pool; deleted pool ids; devices' ids kept, not derived. A new map format, written only once used, as formats 2–4 were |
| Map service | `AddDevice`, `RemoveDevice`, `SetDeviceClass`, `CreatePool`, `SetPool`, `DeletePool`; the join that appends; the autoscaler; splits |
| `hyperstord` | `--shard ID@…`; formats files only; checks the device's and cluster's ids; reports its kind; splits PGs on request; deletes deleted pools' objects |
| Engine | A rename of an object's attributes and key–value table, beside `Absorb` |
| Installer | `--device`, `--class`, `--force-format`; signature checks; udev rules; devices kept in `install.conf` |
| `hyperstorctl`, API, UI | The calls above; classes in `nodes` and `pools` |
| Tests | Randomized tests of add, remove, relabel and split against a model (D22's way), with faults; end-to-end tests of each operation through the built binaries; G44 on the pilot |

### 27.8 Stages

Each stage is a milestone item with its own pull requests and tests:

1. Devices added to a running node: `AddDevice`, the join that appends,
   the installer's `--device`, the identity check and the format rule
   (§27.2.1–§27.2.3).
2. Classes and class sets (§27.2.4, §27.4).
3. Pools made, changed and deleted (§27.3).
4. Devices removed (§27.2.5).
5. PG splits and the autoscaler (§27.5).
6. Merges, by the operator (D174), when needed.
