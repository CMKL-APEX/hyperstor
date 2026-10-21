# Introduction

Hyperstor is a sharded, distributed storage system written in Rust. It
stores data on many storage nodes, keeps two or three copies of every
piece, and keeps serving reads and writes when a node fails. It is built
for [glidex](https://github.com/CMKL-APEX/glidex) clusters and also runs
on its own.

> **Early implementation.** Hyperstor is not ready for production data.
> Today it provides replicated storage over TCP, used through a Rust
> client library, with failure detection and recovery. There is no block
> device front end, no authentication and no encryption yet. See
> [Status](status.md) for what is built and what comes next.

## What it does today

- **Replicated pools.** Data is split into chunks, and each chunk is
  written to two or three storage nodes in different failure domains
  (devices, hosts or racks). A write returns once every copy has it.
- **Its own storage engine.** Each storage device is driven directly
  through Linux io_uring, with checksums on every 4 KiB block and crash
  recovery.
- **Failure handling.** The map service notices when a node stops
  answering, moves its work to other nodes, and refills a node when it
  comes back.
- **A client library** for Rust programs to read and write volumes.

## What is planned

Block volumes for virtual machines (vhost-user-blk), NVMe over TCP and
RDMA, snapshots and clones, erasure coding, GPU-direct storage, an S3
gateway and a distributed filesystem, all on the same object layer. The
[Status](status.md) page lists the milestones.

## How this documentation is organized

| Part | For | Contents |
|---|---|---|
| [Quick start](quick-start.md) | Everyone | A three-node cluster on one machine, in a few minutes |
| [Concepts](concepts.md) | Everyone | Nodes, pools, placement groups, volumes and the map |
| [User guide](user-guide/index.md) | Developers who store data in Hyperstor | The client library, what it guarantees, its errors |
| [Administrator guide](admin-guide/index.md) | People who install and run clusters | Installation, planning, running the services, failures |
| [Reference](reference/index.md) | Both | Every command-line option and the cluster file format |
