# Milestones and host checks

Milestones HS0–HS14 and the host checks (`G…`) to run before sections are final. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 21. Milestones

| # | Content | Depends on |
|---|---|---|
| HS0 | Reactor prototype (io_uring passthrough, TCP, buffers) and gate G5 on a test host. Engine prototype with the general object format (D34: data, attributes, key–value tables, operation lists) and crash tests (log replay, atomicity, D22), on both device back ends. | — |
| HS1 | `glidex-raftstore` extracted (a glidex change); `hyperstor-meta` with the map, pools, PGs and placement; `hyperstord` with the engine; replicated pools over TCP; the `.meta` pool with headers, directories and leases; the client library; ublk devices for testing. fio through ublk. | HS0 |
| HS2 | Failures (built: liveness, degraded acting sets, backfill, map leases with fencing and pushes, D42; peering, delta recovery and scrub by digests, D90; the blocklist and forced takeovers; down and out, failed devices and slow replicas, D91; the in-process fault rig, D92; map leases as read leases, D89): peering, read leases, recovery, backfill, scrub, down and out, the blocklist and forced takeovers (replication.md §8.4–metadata.md §9). A fault-injection test rig (kill, partition, slow disk). | HS1 |
| HS3 | `hyperstor-vhost` with adaptive polling and guest-memory registration (frontends.md §11.1), client readahead, and the glidex driver (glidex.md §17): create, delete, expand, stage, images as golden snapshots and clones, metering. | HS2, storage S6 |
| HS4 | Snapshots, groups, clones, rollback, flatten, snapshot deletion; the host read cache (frontends.md §11.3). glidex disk snapshots spec. | HS2 |
| HS5 | NVMe-oF: the ublk and `nvmet` stopgap, then our NVMe/TCP target with ANA, reservations and CHAP. | HS2 |
| HS5b | CPU per operation (D41): remove copies in the engine, client and daemon (zero-copy payloads, registered buffers, vectored sends), adaptive polling in the reactor. Measured against the end-to-end benchmark. | HS2 |
| HS6 | RDMA: the D5 protocol over verbs (RoCEv2 and InfiniBand), shared receive queues, credits, transport choice per network, port roles and traffic classes, fabric readiness checks (protocol.md §13.2). No NVMe-oF needed. | HS2, HS5b |
| HS6b | NVMe/RDMA in our target. | HS5, HS6 |
| HS7 | EC pools with the write log and full-stripe writes (replication.md §8.6). | HS2 |
| HS8 | GPU-direct on bare metal: `libhyperstor-gpu`, the interposers, `FIEMAP` mapping, read-only multi-attach. G1, G3, G4. The hipFile upstream proposal. | HS4, HS6 |
| HS9 | GPU-direct in glidex VMs: lease delegates, the vsock credential, virtio disks in the interposer (gpu.md §12.3, G2); server-side QoS; PG splitting. | HS8 |
| HS10 | Hard-disk pools with flash log devices (D29). | HS2 |
| HS10b | Cache pools (D45): the pool kind, expiry, eviction, loss without recovery; priority classes (D46) in the reactors' queues and on the wire. | HS2 |
| HS11 | `hyperstor-kv`: ranges, splits and merges, the root range, transactions, the timestamp oracle, garbage collection. Tested against a model of snapshot isolation (Jepsen-style histories) under faults. | HS2 |
| HS12 | The S3 gateway over HTTP (s3.md §14.1, s3.md §14.2), with glidex access keys and Cedar. | HS7, HS11 |
| HS13 | S3 over RDMA, with DC queue pairs (s3.md §14.3), tested against cuObject and hipObject clients. | HS6, HS12 |
| HS14+ | The distributed filesystem, per filesystem.md. | HS11, HS13 |
| HS15 | Tenancy (tenants.md §24): tenant ids on lineages, buckets, filesystems and ranges; per-`(pool, tenant)` counters and quotas; two-level fair share in the reactors (D62); the glidex project mapping. | HS4, HS10b, AD3 |
| HS16 | Sites and replication (federation.md §25.2, §25.3): `site` as a failure domain with the preflight limit (G19); `hyperstor-sync` for snapshots and S3 objects, with policies, lag health and cross-cluster trust. | HS4, HS12, AD3 |
| HS17 | The global namespace (federation.md §25.4–§25.6): range homes, read-through replicas, forwarded writes, the partition queue with its allowlist (D66), cache-pool sources (D67), the federated view in `hyperstor-mgmt` and glidex (D68); G20. | HS11, HS16 |
| AD0 | Administration foundation (administration.md §23.11): `hyperstor.v1` on connectrpc with `buf` checks (D51), leaf identities and the two-level CA (D53, D56), `hyperstorctl` generated from the protos with the local socket (D55). | HS1 |
| AD1 | `hyperstor-install` with `check`, join tokens (D57) and `--all-in-one`. | AD0 |
| AD2 | `hyperstor-mgmt` read-only (D50): health, map, metrics, watch fan-out, UI pages without actions; G17. Needs `glidex-auth` extracted (D52, a glidex change). | AD0 |
| AD3 | UI and CLI writes, Cedar roles, step-up, trash (D54), audit, operations. | AD2 |
| AD4 | glidex's Storage section and `storage-admin` role (a glidex change). | AD3, HS3 |
| — | A libfabric transport, when a site has UET, EFA or Slingshot NICs with production drivers (D30). Queued, no milestone yet. | HS6 |

## 22. Questions

Decisions are settled. These need checking on real hosts before the
sections that depend on them are final:

- **G1. cuFile's userspace-FS path without nvidia-fs.** Does `libcufile`
  accept a userspace-FS handle on a P2PDMA-only install (open kernel
  modules, no `nvidia-fs.ko`)? The interposer ignores `desc_str` and
  registers the buffer itself through dma-buf, so the format of `desc_str`
  doesn't matter. If `libcufile` refuses the handle, the cuFile
  interposer serves reads and writes itself, as the hipFile one does
  (D15).
- **G2. P2P inside a guest** (D18). Do `ibv_reg_dmabuf_mr` and the GPU
  drivers' dma-buf export (which checks `pci_p2pdma_distance`) accept a
  GPU and an RDMA VF passed through to one VM? Check QEMU with a virtual
  PCIe switch, and Cloud Hypervisor.
- **G3. Extent stability** (D16). On xfs and ext4, check that a read
  lease and `FIEMAP` flags (`UNWRITTEN`, `DELALLOC`, `SHARED`) are enough
  to keep a mapped extent valid while a native I/O is in flight. Check
  that dropping cached pages (`fadvise(DONTNEED)`) after a native write
  keeps buffered readers on the same host coherent.
- **G4. Interposing.** Look up the cuFile and hipFile symbols with
  `dlsym(RTLD_NEXT)` and don't ship vendor headers. Confirm the cuFile
  EULA allows this.
- **G5. io_uring versus SPDK** (D3). On a storage test host, measure
  IOPS and latency per core for 4 KiB random reads and writes, and
  bandwidth for 1 MiB I/O. Compare `uring_cmd` passthrough with polled
  queues against SPDK's `perf`, and our NVMe/TCP target against SPDK's.
  This is a gate in the first milestone. It doesn't bring SPDK back; it
  records the gap and points to what to optimise.
- **G6. vhost-user reconnect in Cloud Hypervisor** (frontends.md §11.1). Does a
  `vhost_user=true` disk survive a restart of its backend, and how long
  does the guest wait? If not, `hyperstor-vhost` restarts must be
  avoided for running VMs (upgrades by live migration later) or the
  feature must go upstream.
- **G7. Passthrough without root** (operations.md §18.2). Since Linux 6.2 the NVMe
  driver allows read, write and dataset-management commands without
  `CAP_SYS_ADMIN` when the device is open for writing (`nvme_cmd_allowed`).
  Confirm on the kernels we ship, and list the admin commands the engine
  needs (identify, format) so that stord can run those at hand-over.
- **G8. RDMA registration of guest memory** (frontends.md §11.1). Measure, for large
  VMs, the attach-time cost of registering the whole guest memory with
  ODP and with pinned hugepages, and the cost of ODP page faults on the
  first I/O to each region. Decide whether NICs without ODP need a
  registration cache of bounce buffers instead.
- **G13. redb on a shard region** (D43). Run redb through its
  `StorageBackend` on a region of a shard device, and measure commit rate
  and write amplification for the index's update pattern against the
  log-plus-checkpoint design, before writing our own copy-on-write
  B+tree.
- **G9. Hard disks behind one log device** (D29). Measure write
  latency and throughput for eight hard disks sharing one SATA SSD and
  one NVMe log device, to set the limit per log device by class.
- **G10. GPU-direct alongside NCCL** (D31). On a GPU host, run a
  checkpoint write burst through `libhyperstor-gpu` during an all-reduce
  benchmark (`nccl-tests`, `rccl-tests`), on a separate storage NIC and on
  a converged one with its own traffic class. Measure the slowdown of
  each, to check the defaults.
- **G11. Native path against the kernel path on Grace systems** (gpu.md §12.4).
  On a Grace Hopper or Grace Blackwell host, compare `libhyperstor-gpu`
  with the kernel path and its bounce buffer for model loading and
  checkpoint writes, to see whether the native path is still worth its
  cost there.
- **G12. The `x-amz-rdma-token` format** (s3.md §14.3). Derive it from
  hipObject's MIT-licensed source, which interoperates with cuObject
  servers, and confirm with a cuObject client that a Rust
  implementation is accepted. Check whether the format is published,
  and whether its use needs any agreement with NVIDIA.
- **G16. Short leases under load** (D48). On a real network, with the
  storage nodes busy (backfill, checkpoint bursts, a saturated NIC),
  count false down-markings and missed lease renewals at a 0.6 s lease
  and 0.8 s timeout, with and without peer reports, to confirm the
  defaults or lengthen them.
- **G17. Admin load doesn't move I/O latency** (D58, administration.md
  §23.3). With reactors at QD1 and QD32, run 1000 API reads/s, a 1 s
  metrics scrape and 50 UI watchers against the node; p99 latency
  should stay within 5% of an idle control path. If not, the control
  threads need cores of their own or a lower scheduling priority.
- **G18. Sealing the intermediate key** (D53, administration.md §23.8).
  Do `systemd-creds encrypt --with-key=tpm2` and its host-key fallback
  work on the target distributions (systemd version, with and without a
  TPM), and does a reinstalled host fail closed rather than start
  without the key?
- **G19. Sites within one cluster** (D63, federation.md §25.2). On two
  buildings joined by a real campus link, measure write p99 with one
  copy remote and the rate of false down-markings at the D48 defaults,
  as round-trip time is raised with `tc netem` from 0.2 to 5 ms. Set
  the preflight limit where either degrades; 1 ms is the estimate.
- **G20. The namespace across a WAN** (D65–D67, federation.md §25.4,
  §25.5). Over a 30–80 ms link: the latency and throughput of
  cache-source fetches for model weights against a local replica; the
  clock offset between the sites' oracles against the D48 bound that
  last-writer-wins relies on; and, for an agent-memory workload split
  over two sites through a 10-minute partition, the number of keys
  that conflict at replay and whether any front-end invariant breaks.
- **G21. Digest scans at scale** (D90, replication.md §8.3). On a data
  device with a large PG (100,000 objects of 4 MiB, 400 GiB), time a
  `Digests` list and a peering
  round, and their cost to I/O on the same device. A PG log is needed
  if peering takes more than about a second, or the scan costs I/O more
  than scrub does.
- **G22. ublk under load** (D94, D95). On a storage test host with
  NVMe-backed nodes, run fio through a `hyperstor-ublk` device (4 KiB
  random reads and writes at QD1 and QD32, 1 MiB sequential) against
  `hyperstor-bench cluster` on the same cluster, to see what ublk and
  its one queue cost; and check the kernels glidex hosts run (ublk needs
  Linux 6.0; tested on 6.8).
