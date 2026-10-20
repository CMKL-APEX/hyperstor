# GPU-direct storage

The native GPU path behind cuFile and hipFile, VMs, scale-up fabrics, NCCL and RCCL. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 12. GPU-direct storage (D13–D18)

### 12.1 The native path

1. The application registers a file through cuFile or hipFile.
2. The interposer checks whether the file is on a hyperstor volume: its
   block device must be a hyperstor NVMe-oF namespace (by subsystem NQN),
   a hyperstor ublk device, or, in a glidex VM, a virtio disk whose
   serial the delegate credential lists (D33). If not, it passes the
   call through.
3. If it is, the interposer registers a userspace-FS handle (D14), opens
   the volume in `libhyperstor-gpu` as a delegate of the volume's lease
   holder (D33), and takes a read lease on the file.
4. **On a read**, the library:
   - maps the file range to volume blocks with `FIEMAP`;
   - for the extents that qualify (D16), registers the GPU buffer through
     dma-buf (registrations are cached per buffer);
   - sends one request per chunk to its primary (or, for a read-only
     snapshot, to any replica), carrying the buffer's address and rkey.

   The storage node writes the data into GPU memory by RDMA and answers.
   Ranges that don't qualify go through the kernel path (D17).
5. **Writes** go the same way, in the other direction: the storage node
   reads from GPU memory by RDMA.

### 12.2 Leases and consistency

- The native client and the kernel path share one `rw` lease. On bare
  metal, the gateway holds it for the host's NQN (its reservation) and
  the library is the gateway's delegate; in a VM, `hyperstor-vhost` holds
  it and the guest library is its delegate (D33). The library's host
  identity must match the one the lease was taken for.
- After a native write, the library drops the written range from the
  page cache (`fadvise(DONTNEED)`), so buffered readers on the host see
  the new data (G3).
- **Read-only multi-attach:** a snapshot attached `ro` by many hosts is
  mounted `ro,norecovery`. Its block map can't change, so every read of
  it qualifies for the native path.

### 12.3 In glidex VMs (D18, D33)

1. The VM has its volume as an ordinary vhost-user disk, its GPU, and an
   RDMA VF on the storage network, under the same PCIe switch as the GPU.
2. The guest mounts its filesystem on the virtio disk.
3. `libhyperstor-gpu` in the guest asks `hyperstor-vhost` over
   virtio-vsock for a delegate credential. It names the VM's volumes
   by disk serial, and expires with the VM's attachment.
4. Native reads and writes go over the VF straight to the storage nodes,
   as on bare metal (§12.1). Everything else, including the guest's own
   filesystem metadata, goes through the virtio disk.
5. The guest library's buffers in GPU memory are registered through the
   VF; whether the drivers allow that inside a VM is G2.

### 12.4 Scale-up fabrics (NVLink, UALink)

NVLink (with NVSwitch) and UALink connect the GPUs of one pod with
memory semantics. They have no storage endpoints and are not a transport
for hyperstor: storage stays on the scale-out fabric (protocol.md §13.2).

- **Loading the same data on many GPUs.** Inside an NVLink or UALink
  domain, a framework can read each part of a file on one GPU and share
  it over the scale-up fabric (sharded loading, then an all-gather).
  This divides the storage traffic by the domain's size. It is the
  framework's choice; hyperstor's part is to make the sharded reads
  efficient (any-replica reads of snapshots, readahead, the host cache).
- **Grace systems.** On GPUs coherently attached to a CPU (NVLink-C2C on
  Grace Hopper and Grace Blackwell), a copy from host memory into the GPU
  is far cheaper than over PCIe. The kernel path (D17) and its bounce
  buffer then lose less against the native path. G11 measures how much.
- **Placement inside a pod** (which GPUs share an NVLink partition, how
  passthrough VMs get whole partitions) is host-topology's concern.

### 12.5 Sharing a process with NCCL or RCCL

Training jobs load `libhyperstor-gpu` into processes that also run NCCL
or RCCL, which use the same NICs' queue pairs and memory registrations.
The library therefore:
- caps its own queue pairs (one per storage reactor it talks to, closed
  when idle, engine.md §6.1), its registration cache (by default 16 GiB of GPU
  memory) and its own locked host memory, all configurable;
- registers GPU buffers through dma-buf independently of NCCL's own
  registrations of the same memory, which the drivers allow;
- uses its own protection domain, so its rkeys and NCCL's never mix.

### 12.6 Libraries and upstream

- `libhyperstor-gpu` is a `cdylib` with a C ABI, also offering raw volume
  handles (D16) for callers that manage offsets themselves.
- `libhyperstor-cufile` and `libhyperstor-hipfile` are interposers,
  loaded with `LD_PRELOAD` or ahead of the vendor library. They look up
  the vendor symbols with `dlsym(RTLD_NEXT)` and ship no vendor headers
  (G4).
- **hipFile upstream:** add userspace-FS support to hipFile's AMD side as
  a backend (D15). Until it lands, `libhyperstor-hipfile` serves the I/O
  for hyperstor files itself.
