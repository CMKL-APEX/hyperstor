# Front ends

vhost-user-blk for VMs, the NVMe-oF target, the host read cache, and later front ends. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 11. Front ends

### 11.1 vhost-user-blk (D1, D26)

- `hyperstor-vhost` serves one socket per attached disk,
  `/run/hyperstor/vhost/<disk-id>.sock`, built on the rust-vmm crates.
- Guest memory is shared with the service. VMs with a vhost-user disk
  therefore need a shared memory backend: memfd, or hugepages per
  host-topology §8a, as vhost-user networking already does.
- **Virtio features:** multiqueue (one queue per vCPU, up to the reactors
  on the VM's NUMA node), packed virtqueues, `EVENT_IDX`, discard,
  write-zeroes, flush (volumes.md §10.1), and the config-change event for expand.
- **The disk's serial number** is the first 20 characters of the glidex
  disk id without dashes, unique within the VM. Tools in the guest, and
  the guest library (D33), find the volume behind a disk by it.
- **Fewer VM exits:**
  - while a queue is busy, the reactor polls it and uses `EVENT_IDX` to
    tell the guest not to notify, so submitting I/O costs the guest no
    exit;
  - after 50 µs (configurable) without new requests, the queue re-enables
    notifications and the reactor waits on its eventfd (adaptive
    polling);
  - completions are signalled through irqfd, which KVM delivers as a
    posted interrupt where the CPU supports it, again without an exit;
  - a VM marked latency-critical can use polled queues in its own
    virtio-blk driver (`poll_queues`), giving up guest CPU for the lowest
    latency.
- **Zero copy.** Guest memory is registered with the service's RDMA
  protection domain when the VM attaches:
  - on NICs with on-demand paging (mlx5 ODP), the whole guest memory is
    registered without pinning it;
  - otherwise the VM's memory must be hugepage-backed, and the
    registration pins it once, at attach (G8).

  With TCP, sends use `SEND_ZC` from guest memory.
- **Restart.** The service keeps its leases in a state file. After a
  restart it re-registers them, and the hypervisor reconnects to its
  sockets: QEMU with `reconnect`, Cloud Hypervisor to be checked (G6).
- **As built** (HS3, D116, D117; implementation-notes.md I152–I155):
  one reactor, the control socket, write-through 4 KiB-block disks with
  multiqueue, `EVENT_IDX`, flush and write-zeroes; VMs boot from volumes
  under Cloud Hypervisor and QEMU. Not yet: discard, packed rings,
  adaptive polling, zero copy, the state file and the config-change
  event.

### 11.2 NVMe-oF target (D19)

- `hyperstor-gw` runs the client library behind an NVMe-oF target
  written in Rust. NVMe/TCP comes first, NVMe/RDMA (RoCEv2 and IB) after.
- **Subsystems.** One subsystem per volume,
  `nqn.2026-10.org.glidex:hyperstor:<cluster-id>:<volume-id>`, with
  namespace 1. A read-only attachment of a snapshot exports it read-only.
- **Multipath.** Every gateway exports every attached volume; all of
  them are ANA-optimized, and the host spreads I/O over them (D17).
- **Access:**
  - a host allowlist of host NQNs, from the volume's attachments;
  - DH-HMAC-CHAP with keys from the credential store (credentials.md);
  - NVMe reservations, held for the host by the gateway: acquire takes
    the volume's `rw` lease, and preempt is a forced takeover (metadata.md §9).
- **Discovery** service on port 8009, listing only the subsystems the
  connecting host may see.
- **Stopgap (D19):** until the target is ready, each exported volume is
  a ublk device served by `hyperstor-ublk`'s crate (hyperstor's own
  ublk on its reactor, D94, D95, not the `libublk` crate), and
  is exported by the kernel's `nvmet` through configfs. The gateway
  manages that configuration and gives the same NQNs, so hosts don't
  change when the stopgap is replaced.

### 11.3 Host read cache (D32)

- **What is cached:** only versions that can't change, that is, versions
  with an epoch older than the volume's head epoch, read through a
  snapshot attachment, or read by a clone through to its parent. A
  read of a head version always goes to the primary.
- **Where:** memory first (default: 5% of the node's memory, at most
  64 GiB), and optionally a partition of a local NVMe device as a second
  tier. The second tier is a plain cache: it is dropped on restart if its
  header doesn't match, and nothing is ever written back from it.
- **Keys and verification.** Entries are keyed by `(lineage, chunk,
  branch, epoch, block)` and keep the blocks' CRCs from the storage node,
  checked again on every read from the NVMe tier.
- **Eviction:** least recently used, with a scan-resistant admission
  rule (a block is admitted to the NVMe tier on its second miss), so one
  large sequential read doesn't flush the cache.
- **Deleted snapshots** are dropped from the cache when the map delta
  (or the header notification) reports them.
- The GPU library (gpu.md §12) can use a host's cache through
  `hyperstor-vhost` on glidex nodes; on bare metal it reads directly.

### 11.4 Later

- **Kubernetes on glidex VMs.** A CSI driver in the style of cloud
  providers' (AWS EBS): the controller calls glidex's API to create a
  hyperstor disk and hot-plug it into the VM as vhost-user-blk. The
  guest never reaches the storage network. `ReadOnlyMany` volumes are
  read-only snapshot attachments, served by the host read cache. It
  needs vhost-user-blk hot-plug in glidex (glidex.md §20).
- **NVMe emulation over vfio-user**, for guests that need a real NVMe
  device (some Windows and appliance images). For Linux guests,
  virtio-blk performs as well.
- **The vhost client on a DPU** (BlueField SNAP and similar), presenting
  virtio-blk or NVMe functions to the guest with no host CPU. Worth it
  at scale, but tied to one vendor and hard to combine with live
  migration.
- **Live migration.** When glidex migrates VMs live, the destination
  attaches as a delegate (D33) during the copy and takes over the lease
  at switchover, with no blocklist round.
