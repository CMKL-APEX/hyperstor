# Introduction

Hyperstor is a sharded, distributed storage system written in Rust. It
stores data on many storage nodes, keeps two or three copies of every
piece, and keeps serving reads and writes when a node fails. It is built
for [glidex](https://github.com/CMKL-APEX/glidex) clusters and also runs
on its own.

> **Early implementation.** Hyperstor is not ready for production data.
> Today it provides replicated block volumes over TCP, used through a
> Rust client library, as virtual machines' disks over vhost-user-blk,
> or, for testing, as Linux block devices through ublk, with failure
> handling, snapshots and clones, and tools to install and manage a
> cluster. There is no front end for remote hosts yet (NVMe over TCP),
> and the data path between clients and storage nodes has no
> authentication or encryption (the cluster API, the map service's
> members and the management service use TLS). See [Status](status.md)
> for what is built and what comes next.

## What it does today

- **Replicated pools.** Data is split into chunks, and each chunk is
  written to two or three storage nodes in different failure domains
  (devices, hosts or racks). A write returns once every copy has it.
  Pools are made, changed and deleted while the cluster runs, on the
  device classes they name (`nvme`, `ssd`, `hdd`, or your own), and
  their placement groups are split as devices are added.
- **Cache pools** keep one copy of data that can be recomputed: a failed
  device's data is lost instead of recovered. Expiry and eviction are
  not built yet.
- **Devices at run time.** Devices are added to and removed from running
  nodes. Each device gets data in proportion to its size. A device past
  its `full` threshold refuses new data, while overwrites and deletes go
  on, so space can always be freed.
- **Its own storage engine.** Each storage device is driven directly
  through Linux io_uring, with checksums on every 4 KiB block and crash
  recovery.
- **Failure handling.** The map service notices when a node stops
  answering, moves its work to other nodes, and refills a node when it
  comes back with only what it missed. A short reboot moves no data, and
  a node that keeps going down and up is held down. A failed device, or a
  replica too slow to answer, is replaced at once. A background scrub
  compares the copies and repairs one that is damaged or differs.
- **A replicated map service.** Three or five members keep the cluster
  map with Raft, so losing one doesn't stop the cluster.
- **Volumes** with one writer at a time: a client attaches a volume by
  holding its lease, and one that dies is fenced off when another takes
  the volume over. Each attached volume is fenced on its own, so taking
  one volume from a client leaves its others alone.
- **Snapshots and clones.** Crash-consistent snapshots of one volume or
  of a group at one instant, instant clones and rollbacks, and snapshot
  deletion that moves no data. Deleted volumes and snapshots go to a
  trash first.
- **Virtual machine disks.** `hyperstor-vhost` serves volumes to the VMs
  of a host over vhost-user-blk. VMs boot from them under Cloud
  Hypervisor and QEMU, and restarting the service keeps their disks.
- **Block devices for testing.** `hyperstor-ublk` serves a volume as
  `/dev/ublkbN`, for `mkfs`, `mount` and fio.
- **A client library** for Rust programs to read and write volumes, with
  a read cache that the clones of one image on a host share.
- **Administration.** `hyperstor-install` installs a trial cluster on one
  host, or a cluster across hosts that join with single-use tokens.
  `hyperstorctl` and a web UI manage it, with people's logins (a
  password or an identity provider), roles, an audit log, and long
  operations such as draining a node.

## What is planned

NVMe over TCP and RDMA, erasure coding, GPU-direct storage, hard-disk
pools with flash log devices, expiry and eviction in cache pools, an S3
gateway and a distributed filesystem, all on the same object layer. The
[Status](status.md) page lists the milestones.

## How this documentation is organized

| Part | For | Contents |
|---|---|---|
| [Quick start](quick-start.md) | Everyone | A three-node cluster on one machine, in a few minutes |
| [Concepts](concepts.md) | Everyone | Nodes, pools, placement groups, volumes and the map |
| [User guide](user-guide/index.md) | Developers who store data in Hyperstor | The ways in, the client library, what it guarantees, its errors |
| [Administrator guide](admin-guide/index.md) | People who install and run clusters | Installation, planning, managing a cluster, the services, failures |
| [Developer guide](developer-guide/index.md) | People who work on Hyperstor, or want to know how it works | Architecture, algorithms, design trade-offs, performance, comparison with other systems |
| [Reference](reference/index.md) | Both | Every command-line option, the cluster file and the cluster API |
