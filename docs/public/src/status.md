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
| AD0: Administration API, identities and CLI | In progress |
| AD1: Installer and cluster join | In progress |
| AD2: Management service and read-only web UI | In progress |
| AD3: Administration from the UI and CLI, with roles, trash and audit | Done |
| AD4: Storage administration in glidex | Planned |

## HS0: Local storage engine and reactor

| Feature | Status |
|---|---|
| Per-core I/O reactor on io_uring: tasks, TCP, timers, cross-thread wake-ups | Done |
| General objects: sparse data with checksums, attributes, key-value tables, atomic operation lists | Done |
| Group commit, small-write deferral, checkpoints and recovery | Done |
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
| Changing the map service's members at run time, and recovering a group that lost its majority | Done |
| Volume metadata: headers, directories and volume leases | Done |
| Test block devices through ublk | Done |

## HS2: Failure handling

| Feature | Status |
|---|---|
| Failure detection by heartbeats, and writes that continue with fewer copies | Done |
| Backfill: returning or replacement nodes are refilled | Done |
| Map leases with fencing and pushes: a cut-off primary stops serving | Done |
| Failure reports from peers | Done |
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
| hyperstor-vhost: volumes over vhost-user-blk on the reactor, staged through its control socket | In progress |
| VMs boot from volumes under Cloud Hypervisor and QEMU | Done |
| Adaptive polling, queues on reactors per NUMA node, zero copy from guest memory | Planned |
| A restarted service keeps its leases; hypervisors reconnect | Planned |
| Discard and write-zeroes without allocating; expand signalled to the guest | Planned |
| Fencing one volume of a shared client: a lease identity per attached volume, with D33's delegates | Planned |
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
| Joining storage hosts and map service members with single-use join tokens, and renewing their certificates | Done |
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
| Long operations: draining a node (with a dry run, cancellable), flattening a clone and purging from the trash; kept in the replicated store a week, resumed by a new leader once it has fenced the last one, watched as a stream; in the API, hyperstorctl (--wait, op watch) and the UI | Done |
