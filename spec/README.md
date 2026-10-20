# Hyperstor Design Specification

This directory is the authoritative design reference for Hyperstor, a
sharded, distributed block, object and file store in Rust, built for
[glidex](https://github.com/CMKL-APEX/glidex) nodes and usable without
them.

The top-level `README.md` is a *user* document. Documents here are for
contributors: they capture **invariants**, **contracts between
components**, and **why** the code is shaped the way it is, things a
fresh reader cannot infer just by reading the source.

## How the design is organized

The design is split by topic. **Section numbers are global**: `§8.4` is
the same section whichever document cites it, and a reference to a
section in another document names the file (`engine.md §7.3`).
**Decisions** (`D1`…) are all in [decisions.md](decisions.md), and other
documents cite them by number.

| Document | Sections | What needs consideration there |
|---|---|---|
| [overview.md](overview.md) | §1, §3 | Scope (in, later, out), processes and ports |
| [decisions.md](decisions.md) | §2 | Every design decision with its why; D20–D37 still open to review |
| [metadata.md](metadata.md) | §4, §9 | What goes through Raft and what doesn't; `.meta` header objects; volume leases, delegates and the blocklist |
| [placement.md](placement.md) | §5 | Chunk-to-PG hashing, weighted rendezvous over failure domains, rebalancing, EC shard positions |
| [engine.md](engine.md) | §6, §7 | Reactors, device back ends, the on-device format, write atomicity and deferral, checkpoints, the transactional key–value store |
| [replication.md](replication.md) | §8, §15 | Write and read paths, epochs and leases, peering, recovery, backfill, scrub, erasure coding, failure behaviour |
| [volumes.md](volumes.md) | §10 | Client I/O, snapshots, clones, flatten, QoS |
| [frontends.md](frontends.md) | §11 | vhost-user-blk, the NVMe-oF target, the host read cache |
| [gpu.md](gpu.md) | §12 | GPU-direct storage behind cuFile and hipFile, in VMs, alongside NCCL and RCCL |
| [protocol.md](protocol.md) | §13 | Message framing; TCP, RoCEv2 and InfiniBand transports; port roles and traffic classes |
| [s3.md](s3.md) | §14 | The S3 gateway and S3 over RDMA |
| [operations.md](operations.md) | §16, §18, §19 | Management API and CLI, crates and units, roles and resources, upgrades, security |
| [glidex.md](glidex.md) | §17, §20 | The glidex datastore driver and the glidex changes it needs |
| [milestones.md](milestones.md) | §21, §22 | Milestones HS0–HS14 and host checks G1–G12 |
| [filesystem.md](filesystem.md) | (own) | The distributed filesystem (outline), decisions F1–F12 |
| [decision-log.md](decision-log.md) | — | Decision points reviewed with the owner, answers, and points still open with their trade-offs |
| [implementation-notes.md](implementation-notes.md) | — | What is built, decisions made while building it (I1…), bugs the tests found |
| [benchmarks.md](benchmarks.md) | — | Hosts, methods, results and findings |

## Conventions

- **Decisions** (`D…`, `F…`, `I…`) are tables: the decision, then *why*.
  Never renumber them; a dropped decision is removed and its number not
  reused. A decision that changes an earlier one says so in both.
- **Host checks** (`G…`, `FG…`) are things to verify on real hardware
  before the sections that depend on them are final.
- **Milestones** (`HS0`…) list what each step delivers and depends on.
- References to glidex documents (`storage.md`, `host-topology.md`, …)
  are to glidex's `spec/` directory (see overview.md).
