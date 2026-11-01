# Status

<!-- Generated from spec/tracking.toml by `scripts/tracking.py status`;
     CI fails if it is out of date. Don't edit by hand. -->

Hyperstor is built in milestones. This page lists what each one
delivers and what is available today; every item marked done is
covered by tests that run on every change.

| Milestone | Status |
|---|---|
| HS0: Local storage engine and reactor | In progress |
| HS1: Replicated pools over TCP | In progress |
| HS2: Failure handling | Done |
| HS3: Virtual machine disks (vhost-user-blk) and the glidex driver | In progress |
| HS4: Snapshots, clones and the host read cache | In progress |
| HS5: NVMe over TCP | Planned |
| HS5b: Lower CPU cost per operation | Planned |
| HS6: RDMA over RoCEv2 and InfiniBand | Planned |
| HS6b: NVMe over RDMA | Planned |
| HS7: Erasure-coded pools | Planned |
| HS8: GPU-direct storage on bare metal | Planned |
| HS9: GPU-direct storage in virtual machines | Planned |
| HS10: Hard-disk pools with flash log devices | Planned |
| HS10b: Cache pools and priority classes | Planned |
| HS11: Transactional key-value store | Planned |
| HS12: S3 gateway | Planned |
| HS13: S3 over RDMA | Planned |
| HS14: Distributed filesystem | Planned |
| HS15: Tenancy: quotas and fair share between tenants | Planned |
| HS16: Sites and replication between clusters | Planned |
| HS17: Global namespace and federation | Planned |
| HS18: Pools and devices at run time | In progress |
| AD0: Administration API, identities and CLI | In progress |
| AD1: Installer and cluster join | In progress |
| AD2: Management service and read-only web UI | In progress |
| AD3: Administration from the UI and CLI, with roles, trash and audit | In progress |
| AD4: Storage administration in glidex | Planned |

## HS0: Local storage engine and reactor

| Feature | Status |
|---|---|
| Per-core I/O reactor on io_uring: tasks, TCP, timers, cross-thread wake-ups | Done |
| General objects: sparse data with checksums, attributes, key-value tables, atomic operation lists | Done |
| Group commit, small-write deferral, checkpoints and recovery | Done |
| Feature flags in the device superblock; only a blank device is formatted | Done |
| Each device's superblock names its device and cluster: a device of another device or cluster (a drive moved, swapped paths) is refused before it is opened, and one formatted without a cluster gets it recorded | Done |
| A slow device no longer holds its reactor: commits wait for the device in the background (first stage) | In progress |
| Crash safety: randomized crash tests against a model | Done |
| Device back ends: memory, files and block devices through io_uring with O_DIRECT | Done |
| NVMe passthrough back end | Planned |
| Performance gate against SPDK on a storage test host | In progress |

## HS1: Replicated pools over TCP

| Feature | Status |
|---|---|
| Placement of data across failure domains | Done |
| Wire protocol and encoding, with checksums on every block | Done |
| Cluster map and map service, configured from a cluster file | Done |
| Storage daemon: replicated writes and reads, group commit | Done |
| Client library: request splitting, map refresh and retries | Done |
| Replicated map service (Raft) | In progress |
| Map backups by the leader and on request, and restoring a lost store from one | Done |
| Changing the map service's members at run time, recovering a group that lost its majority, and a leader without one saying so | Done |
| Volume metadata: headers, directories and volume leases | Done |
| Test block devices through ublk | Done |

## HS2: Failure handling

| Feature | Status |
|---|---|
| Failure detection by heartbeats, and writes that continue with fewer copies | Done |
| Backfill: returning or replacement nodes are refilled | Done |
| A device deletes its copies of PGs it no longer serves, once they are clean elsewhere | Done |
| Objects too large for one frame are pushed in parts (backfill, peer sync, scrub repair) | Done |
| A primary fetches a member's copy of a large object in parts (repair of its own copy) | Done |
| Peer and map-service calls have deadlines; one connection per peer; TCP keepalive and connect timeouts | Done |
| Map leases with fencing and pushes: a cut-off primary stops serving | Done |
| Failure reports from peers | Done |
| Flap damping: a node that goes down is held down, longer each time; health warns | Done |
| Peering and delta recovery, by content digests instead of a PG log | Done |
| Reads: under map leases (no replica read leases), served from a member when the primary's copy fails its checksums | Done |
| Scrub | Done |
| Blocklist and forced takeovers | Done |
| Down and out, and failed devices | Done |
| Slow and unreachable replicas leave a PG | Done |
| Fault-injection test rig: kill, partition, slow disk | Done |

## HS3: Virtual machine disks (vhost-user-blk) and the glidex driver

| Feature | Status |
|---|---|
| hyperstor-vhost: volumes over vhost-user-blk on the reactor, staged through its control socket, as 512e disks | In progress |
| VMs boot from volumes under Cloud Hypervisor and QEMU | Done |
| Adaptive polling, queues on reactors per NUMA node, zero copy from guest memory | Planned |
| A restarted service keeps its leases; hypervisors reconnect, and requests in flight run again | Done |
| hyperstor-install --vhost / --role vhost: the service and its unit, for the users named | Done |
| Discard and write-zeroes without allocating; expand signalled to the guest | Planned |
| Fencing one volume of a shared client: a lease identity per attached volume, with D33's delegates | Done |
| Client readahead | Planned |
| hyperstor's side of the glidex driver: mgmt tokens and enrolment, ExpandVolume, write-volume and read-volume | Done |
| The glidex hyperstor driver: create, delete, expand, stage, images as golden snapshots and clones, metering | Planned |

## HS4: Snapshots, clones and the host read cache

| Feature | Status |
|---|---|
| Engine: chunk versions read through layers, and merged without copying | Done |
| Snapshots, clones, rollback, flatten and snapshot deletion | Done |
| Consistency groups | Done |
| Paged checkpoints: only the pages changed since the last (D105) | Done |
| The host read cache, in the client library | In progress |

## HS18: Pools and devices at run time

| Feature | Status |
|---|---|
| A device added to a running node: registered, weighted once it runs, then given PGs | In progress |
| Device ids named in --shard and the cluster file; a node serves only devices the map has, on it, at their address | Done |
| Block devices are formatted by the installer after a check for other systems' signatures; the node formats only files | Done |
| The installer adds a block device to an installed node and keeps it across runs | In progress |
| Each device's kind reported; classes follow it unless an operator sets one | Done |
| Pools place on a set of device classes; hard disks and flash mix only when forced | Done |
| Pools made at run time on the device classes they name | Done |
| A pool deleted once it holds nothing: out of the map at once, its id kept, its data deleted by each device | Done |
| A pool deleted with its volumes, snapshots and trash when forced, as an operation | Done |
| A pool renamed, or its failure domain changed, with its PGs following | Done |
| A pool's devices added beyond its classes, and removed from them | Done |

## AD0: Administration API, identities and CLI

| Feature | Status |
|---|---|
| Cluster health with its reasons, from what clients see | Done |
| Cluster API (status, placement groups, taking devices out and in), callable with curl | In progress |
| Command-line tool: status, nodes, pools, placement groups, devices out and in, I/O test | In progress |
| Metrics on each storage node's admin port | In progress |
| Certificates, identities and roles for the API | In progress |
| Revoking certificates by serial, refused by the API and between map service members | Done |

## AD1: Installer and cluster join

| Feature | Status |
|---|---|
| Host checks before installing | Done |
| One-command trial cluster on one host, with system services, updated in place on later runs | Done |
| Installing a cluster across hosts from a shared cluster file | Done |
| Joining storage hosts and map service members (also running a storage node of the cluster file) with single-use join tokens, and renewing their certificates | Done |
| Backing up the root CA's key, encrypted, and restoring it | Done |
| Installing storage hosts on whole devices | Planned |

## AD2: Management service and read-only web UI

| Feature | Status |
|---|---|
| Read-only dashboard on loopback: health, I/O, space, pools, placement groups, nodes | In progress |
| Grafana dashboard and Prometheus scrape configuration for the storage nodes' metrics | Done |
| The PAM helper, and the management service's rows in the map service's store | Done |
| People log in to the management service with their password (sessions, API tokens, roles in Cedar, step-up, the first administrator's claim, the cluster API on their behalf) | Done |
| Logins with an identity provider (OIDC), hyperstorctl through the management service, and the person named in the map service's audit | Done |
| People reach mgmt on the host's addresses over HTTPS: a certificate from the cluster CA, the operator's own re-read when it changes, the node's Tailscale certificate | Done |
| The web UI (React, @glidex/ui-kit): overview, nodes, pools and PGs, volumes and snapshots, operations, members, security; installed as files and served by hyperstor-mgmt --ui | Done |

## AD3: Administration from the UI and CLI, with roles, trash and audit

| Feature | Status |
|---|---|
| Deletes go to a trash: restore, purge at once (step-up), a purger after each pool's grace period, its settings; in the API, hyperstorctl and the UI | Done |
| Audit of every change and every refused call, in .meta (a day per object, a year kept), with mgmt's records; in the API, hyperstorctl and the UI | Done |
| The audit log bounded after a retry storm: repeated refusals summarised a minute at a time, and a day written as numbered parts per member of at most 16 MiB, merged on read and pruned whole | Done |
| Long operations: draining a node (with a dry run, cancellable), flattening a clone and purging from the trash; kept in the replicated store a week, resumed by a new leader once it has fenced the last one, watched as a stream; in the API, hyperstorctl (--wait, op watch) and the UI | Done |
| A grid of each pool's PGs in the web UI and hyperstorctl pg-grid, sent a byte per PG and only when the map's epoch has moved; one PG's sets on request | Done |
| Each PG's usage in the grid: primaries report the bytes their copies hold, the leader answers with a level per PG as a share of the fullest, the UI shades each cell and hyperstorctl pg-grid --usage prints a digit | Done |
| Each pool's usable space and each device's raw space in GetStatus: every device reports its data region and free bytes to the leader, divided by the pool's redundancy for pools; hyperstorctl pools and nodes show FREE and TOTAL, the web UI bars on the Pools, Nodes and Overview pages | Done |
| Device weights from capacity: each device's weight is its data region in TiB, set by the leader from its space reports (all at once, so devices of one size move nothing), 0 for a joining host until it reports; hyperstorctl weight sets one, kept and marked until set back to auto | Done |
| Fullness thresholds: cluster-wide ratios in the map, set with hyperstorctl full-ratios; past full a device refuses new objects and writes to unwritten blocks while overwrites, keys of existing objects and deletes go on; devices past backfillfull get no new backfill; each device's and pool's fullness in health, the tables, the UI and the audit log, and no new volume, clone or snapshot in a full pool | Done |
| A write refused for space held by hyperstor-vhost, retried for up to 60 s, before the guest gets an I/O error | Planned |
| A pool's usable space under its failure-domain rule: no domain counts for more than one copy, or one EC shard, of each PG, so the smallest of as many hosts as copies bounds the pool | Done |
