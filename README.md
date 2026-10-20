# Hyperstor

A sharded, distributed storage system in Rust for
[glidex](https://github.com/CMKL-APEX/glidex) nodes: block volumes over
vhost-user-blk and NVMe-oF (TCP, RoCEv2, InfiniBand), GPU-direct storage
behind the cuFile and hipFile APIs, and later an S3 gateway and a
distributed filesystem, all on one replicated or erasure-coded object
layer.

**Status: design.** There is no code yet. The design is in
[`spec/`](spec/README.md); implementation starts with milestone HS0
(`spec/design.md` §21).

## Planned features

- **Block volumes**: thin provisioned, expandable, with crash-consistent
  snapshots, consistency groups and instant clones
- **Protection**: 2 or 3 replicas, or erasure coding (k+m) with a
  replicated write log for small writes
- **Data path**: pure Rust on io_uring, one reactor per core; NVMe
  passthrough, SAS and SATA SSDs, and hard disks with a flash log;
  RDMA over RoCEv2 and InfiniBand without NVMe-oF
- **Virtual machines**: vhost-user-blk for glidex VMs, a host-wide read
  cache for images and snapshots
- **GPU-direct storage**: one network hop into GPU memory, behind cuFile
  (GDS) and hipFile, on bare metal and in VMs
- **Later**: an S3 gateway with S3-over-RDMA, and a distributed
  filesystem mounted through virtio-fs and FUSE

## Repository layout

| Path | Contents |
|---|---|
| `spec/` | Design specification (contributors) |
| `docs/` | Documentation sets and their build configuration |
| `crates/` | Rust crates, from HS0 (`spec/design.md` §18.1) |
| `LICENSES/` | License texts and copyright notice |

## License

Dual-licensed under [MIT](LICENSES/LICENSE-MIT) or
[Apache-2.0](LICENSES/LICENSE-APACHE), at your option. See
[LICENSES/COPYRIGHT](LICENSES/COPYRIGHT).
