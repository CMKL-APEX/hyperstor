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
section in another document names the file (`08-engine.md §7.3`).
**Decisions** (`D1`…) are all in [04-decisions.md](04-decisions.md), and other
documents cite them by number.

| Document | Sections | What needs consideration there |
|---|---|---|
| [03-overview.md](03-overview.md) | §1, §3 | Scope (in, later, out), processes and ports |
| [18-workloads.md](18-workloads.md) | (own) | The target workloads (agent VMs, inference, agent memory, fine-tuning, RAG) and what each implies; proposed pool semantics and index flavours |
| [04-decisions.md](04-decisions.md) | §2 | Every design decision with its why; D20–D37, D56–D60 and D66's merge rule still open to review |
| [06-metadata.md](06-metadata.md) | §4, §9 | What goes through Raft and what doesn't; `.meta` header objects; volume leases, delegates and the blocklist |
| [07-placement.md](07-placement.md) | §5 | Chunk-to-PG hashing, weighted rendezvous over failure domains, rebalancing, EC shard positions |
| [08-engine.md](08-engine.md) | §6, §7 | Reactors, device back ends, the on-device format, write atomicity and deferral, checkpoints, the transactional key–value store |
| [09-replication.md](09-replication.md) | §8, §15 | Write and read paths, epochs and leases, peering, recovery, backfill, scrub, erasure coding, failure behaviour |
| [10-volumes.md](10-volumes.md) | §10 | Client I/O, snapshots, clones, flatten, QoS |
| [11-frontends.md](11-frontends.md) | §11 | vhost-user-blk, the NVMe-oF target, the host read cache |
| [12-gpu.md](12-gpu.md) | §12 | GPU-direct storage behind cuFile and hipFile, in VMs, alongside NCCL and RCCL |
| [13-protocol.md](13-protocol.md) | §13 | Message framing; TCP, RoCEv2 and InfiniBand transports; port roles and traffic classes |
| [14-s3.md](14-s3.md) | §14 | The S3 gateway and S3 over RDMA |
| [22-nfs.md](22-nfs.md) | §26 | NFSv4.1/4.2 and pNFS for unmodified clients: the metadata server, files layouts, data servers in the storage nodes, stripe groups, fencing, failover (investigation; D127, D129, D130 settled) |
| [15-operations.md](15-operations.md) | §16, §18, §19 | Management API and CLI, crates and units, roles and resources, upgrades, security |
| [19-administration.md](19-administration.md) | §23 | Installer, cluster API, `hyperstorctl`, the management service and web UI, certificates, people's authentication and authorization, trash and audit |
| [20-tenants.md](20-tenants.md) | §24 | Tenant ids on containers, isolation by lease, per-tenant accounting and quotas, two-level fair share |
| [21-federation.md](21-federation.md) | §25 | Sites as a failure domain, asynchronous replication of immutable versions, the global namespace with range homes, cache sources, the federation view |
| [16-glidex.md](16-glidex.md) | §17, §20 | The glidex datastore driver and the glidex changes it needs |
| [17-milestones.md](17-milestones.md) | §21, §22 | Milestones HS0–HS17 and host checks G1–G24 and G39–G41 (G25–G38 in 22-nfs.md) |
| [00-filesystem.md](00-filesystem.md) | (own) | The distributed filesystem (outline), decisions F1–F12 |
| [05-decision-log.md](05-decision-log.md) | — | Decision points reviewed with the owner, answers, and points still open with their trade-offs |
| [01-implementation-notes.md](01-implementation-notes.md) | — | What is built, decisions made while building it (I1…), bugs the tests found |
| [02-benchmarks.md](02-benchmarks.md) | — | Hosts, methods, results and findings |
| [23-progress-board.md](23-progress-board.md) | — | Who is working on what, and how far each milestone is built (generated) |

What each milestone delivers, how far it is built and the tests that
show it are tracked in [tracking.toml](tracking.toml), which CI checks
against every test run (`scripts/tracking.py`).

The [progress board](23-progress-board.md) shows the work under way:
each workstream (what one agent or person is carrying) keeps one file
in [board/](board/), and `scripts/board.py write` makes the page from
those files and tracking.toml. CI checks the files and that the page is
current (`scripts/board.py check`).

## Conventions

- **File names** are `NN-topic.md`, numbered in the order the documents
  were written (documents split from one at the same time follow their
  section numbers). A new document takes the next free number and never
  renumbers the others; references always name the numbered file
  (`09-replication.md §8.5`). `README.md` and `SUMMARY.md` keep their
  names, which mdBook needs. `scripts/spec-check.py` checks all three.

- **Decisions** (`D…`, `F…`, `I…`) are tables: the decision, then *why*.
  Never renumber them; a dropped decision is removed and its number not
  reused. A decision that changes an earlier one says so in both.
- **Host checks** (`G…`, `FG…`) are things to verify on real hardware
  before the sections that depend on them are final.
- **Milestones** (`HS0`…) list what each step delivers and depends on;
  a milestone added to 17-milestones.md §21 needs an entry in tracking.toml.
- References to glidex documents (`storage.md`, `host-topology.md`, …)
  are to glidex's `spec/` directory (see 03-overview.md).
