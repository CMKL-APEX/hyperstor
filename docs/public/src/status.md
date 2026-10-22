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
| HS2: Failure handling | In progress |
| HS3: Virtual machine disks (vhost-user-blk) and the glidex driver | Planned |
| HS4: Snapshots, clones and the host read cache | Planned |
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
| AD3: Administration from the UI and CLI, with roles, trash and audit | In progress |
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
| Recovering the map service: map backups, member changes at run time, and a group that lost its majority | Planned |
| Volume metadata: headers, directories and volume leases | In progress |
| Test block devices through ublk | Planned |

## HS2: Failure handling

| Feature | Status |
|---|---|
| Failure detection by heartbeats, and writes that continue with fewer copies | Done |
| Backfill: returning or replacement nodes are refilled | Done |
| Map leases with fencing and pushes: a cut-off primary stops serving | Done |
| Failure reports from peers | Done |
| Peering, placement-group log and delta recovery | In progress |
| Read leases | Planned |
| Scrub | In progress |
| Down and out, blocklist and forced takeovers | Planned |
| Fault-injection test rig: kill, partition, slow disk | Planned |

## AD0: Administration API, identities and CLI

| Feature | Status |
|---|---|
| Cluster health with its reasons, from what clients see | Done |
| Cluster API (status, placement groups, taking devices out and in), callable with curl | In progress |
| Command-line tool: status, nodes, pools, placement groups, devices out and in, I/O test | In progress |
| Metrics on each storage node's admin port | In progress |
| Certificates, identities and roles for the API | In progress |

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
| People log in to the management service (sessions, tokens, roles) | Planned |

## AD3: Administration from the UI and CLI, with roles, trash and audit

| Feature | Status |
|---|---|
| Audit of every change and every refused call to the cluster API | In progress |
| Long operations: draining a node, with a dry run, followed to the end or cancelled | In progress |
