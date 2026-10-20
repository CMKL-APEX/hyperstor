# Hyperstor Design Specification

This directory is the authoritative design reference for Hyperstor, a
sharded, distributed block, object and file store in Rust, built for
[glidex](https://github.com/CMKL-APEX/glidex) nodes and usable without
them.

The top-level `README.md` is a *user* document. Documents here are for
contributors: they capture **invariants**, **contracts between
components**, and **why** the code is shaped the way it is, things a
fresh reader cannot infer just by reading the source.

## Contents

| Document | Scope |
|---|---|
| [design.md](design.md) | The whole system (draft): decisions D1–D37; metadata in three levels (Raft cluster map, `.meta` header objects, engine metadata); placement by lineage over PGs; the io_uring reactor storage daemon; the local engine with general objects, NVMe and block back ends; replication, peering, read leases, recovery, scrub; EC with a write log and full-stripe writes; volume leases, delegates and the blocklist; snapshots and clones; vhost-user-blk, the NVMe-oF target and the host read cache; GPU-direct storage behind cuFile and hipFile; the internal protocol over TCP, RoCEv2 and InfiniBand; the transactional key–value store; the S3 gateway with S3-over-RDMA; glidex integration; deployment; milestones HS0–HS14; host checks G1–G12 |
| [filesystem.md](filesystem.md) | The distributed filesystem (outline): stateless metadata servers on the key–value store, file data in objects, virtio-fs for VMs and FUSE elsewhere, close-to-open data with POSIX metadata, decisions F1–F12 |

## Conventions

- **Decisions** are numbered (`D1`, `F1`) and never renumbered; a
  dropped decision leaves a gap. Each has a *why*.
- **Host checks** (`G1`, `FG1`) are things to verify on real hardware
  before the sections that depend on them are final.
- **Milestones** (`HS0`…) list what each step delivers and depends on.
- Status lines at the top of each document say what is settled, what is
  draft, and what is open for review.
- References to glidex documents (`storage.md`, `host-topology.md`, …)
  are to glidex's `spec/` directory (see design.md's introduction).
