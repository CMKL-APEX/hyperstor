# Target workloads

Status: **settled, 2026-10-20.** The workloads hyperstor is built for,
what each does to storage, and what that implies for the design. The
implications are decisions D43–D46.

## 1. The workloads

| Workload | I/O pattern | What it needs |
|---|---|---|
| **Computer-use agent VMs** | Hundreds of VMs per cluster cloned from a few golden images; short-lived; reset to a snapshot after each task; small random writes (OS, browser, scratch) | Instant clone and rollback; cheap delete; the parent image shared through a cache; an index that survives heavy fragmentation; isolation between tenants |
| **Inference serving** | Model weights read by every GPU node: large, sequential, immutable. KV-cache offload: large append-only writes with a lifetime, read back by prefix, recomputable if lost | Read-only multi-attach with any-replica reads (D18); the host read cache (D32); GPU-direct (D13); a pool semantic for recomputable data with a TTL |
| **Agent context and memory** | Many small objects and files, written often, read by key; metadata-heavy | A write-optimized metadata index; inline small objects; the key–value store (D35); the filesystem (D37) |
| **Online fine-tuning** | Checkpoint bursts: large, sequential, from every node at once. Dataset streaming | EC with full-stripe writes (D6); readahead (10-volumes.md §10.1); traffic separation from inference (D31) |
| **RAG** | Vector index pages read at random, small; document stores of many small objects; embeddings written in batches | Low-latency small reads from cache; S3 for documents (D36); the key–value store for indexes |

## 2. Implications

### Index (D43)

- **Dense per-block maps are required**, not an optimization: agent VM
  disks are the random-overwrite pattern that fragments chunks, and there
  are many per node (02-benchmarks.md: about 18× the index RAM otherwise).
- **Two index flavours on one node format** (D44):
  - *block maps* (VM disks, model weights, KV-cache blocks): lookups on
    the read path dominate; a copy-on-write B+tree with a node cache;
  - *metadata* (agent memory, RAG document stores, KV-store ranges):
    small random updates dominate; a Bε variant of the same tree
    (buffered updates in inner nodes, as in TokuDB and BetrFS) absorbs
    them with far less write amplification.
- **Node reference counts** (Rodeh-style shadowing): a clone or rollback
  shares the parent's subtree and costs nothing until the branch
  diverges; deleting a sandbox frees only what it wrote. Resetting
  sandboxes after every task makes garbage collection of abandoned
  branches the hot background job (10-volumes.md §10.3), so version merging
  must be cheap.
- **Lineage placement (D21) and the host read cache (D32)** keep a clone's
  reads of its golden image local and shared across every VM on the node.

### Pools and QoS (D45, D46)

- **Cache pools are first-class** (D45, 09-replication.md §8.7): single copy
  by default, objects with an expiry, no backfill on loss, eviction under
  pressure.
  Lost KV-cache is recomputed; replicating or erasure-coding it wastes
  most of that traffic.
- **Priority classes** (D46, 10-volumes.md §10.5): `latency` above fair share in the
  per-reactor queuing (10-volumes.md §10.5), so inference reads don't wait
  behind checkpoint bursts; the fabric side is D31's traffic classes.

### Front ends

- Agent VMs: vhost-user-blk (D1) with the host cache; snapshots and
  rollback through glidex (16-glidex.md §17).
- Inference and fine-tuning: GPU-direct (12-gpu.md §12), S3 over RDMA
  (14-s3.md §14.3), read-only multi-attach.
- Agent memory and RAG: the filesystem (00-filesystem.md) and S3; small
  objects inline (14-s3.md §14.1).

## 3. What to measure (host checks)

- **G14.** Index RAM and garbage-collection cost for a sandbox fleet:
  N clones of one image, each written randomly then rolled back every
  M minutes, over a day. Decides the dense map's conversion threshold and
  the version-merge rate needed.
- **G15.** Metadata update rate for an agent-memory workload (small
  objects, key–value puts) on a plain B+tree against a Bε-tree, same node
  format, to decide whether the second flavour is worth building.
