# References

## Algorithms and papers

| Topic | Reference | Where it is used |
|---|---|---|
| Rendezvous (HRW) hashing | D. G. Thaler and C. V. Ravishankar, "Using Name-Based Mappings to Increase Hit Rates", *IEEE/ACM Transactions on Networking*, 1998. <https://www.cs.ucr.edu/~ravi/Papers/Jrnl/HRW98.pdf> | [Placement](replication.md#weighted-rendezvous-hashing) |
| CRUSH | S. A. Weil, S. A. Brandt, E. L. Miller, C. Maltzahn, "CRUSH: Controlled, Scalable, Decentralized Placement of Replicated Data", SC '06. <https://www.ssrc.ucsc.edu/Papers/weil-sc06.pdf> | The alternative to rendezvous hashing |
| RADOS and Ceph | S. A. Weil et al., "Ceph: A Scalable, High-Performance Distributed File System", OSDI '06; "RADOS: A Scalable, Reliable Storage Service for Petabyte-scale Storage Clusters", PDSW '07. Ceph peering: <https://docs.ceph.com/en/reef/dev/peering/> | PGs, epochs, the PG log Hyperstor does without |
| Raft | D. Ongaro and J. Ousterhout, "In Search of an Understandable Consensus Algorithm", USENIX ATC '14. <https://www.usenix.org/conference/atc14/technical-sessions/presentation/ongaro> | [The map service](replication.md#the-map-service) |
| openraft | <https://github.com/databendlabs/openraft> | The Raft implementation |
| Leases | C. G. Gray and D. R. Cheriton, "Leases: An Efficient Fault-Tolerant Mechanism for Distributed File Cache Consistency", SOSP '89. <https://www.andrew.cmu.edu/course/15-440/assets/READINGS/gray1989.pdf> | [Map leases](replication.md#map-leases-and-fencing), [volume leases](volumes.md#one-writer-per-volume-leases) |
| Fencing | M. Kleppmann, "How to do distributed locking", 2016. <https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html> | Epochs and the blocklist |
| Primary-backup replication | N. Budhiraja, K. Marzullo, F. B. Schneider, S. Toueg, "The Primary-Backup Approach", in *Distributed Systems* (2nd ed.), 1993 | [The replicated write](replication.md#the-replicated-write) |
| Chain replication | R. van Renesse and F. B. Schneider, "Chain Replication for Supporting High Throughput and Availability", OSDI '04 | An alternative to primary-copy |
| Anti-entropy | A. Demers et al., "Epidemic Algorithms for Replicated Database Maintenance", PODC '87; G. DeCandia et al., "Dynamo: Amazon's Highly Available Key-value Store", SOSP '07 | Digest comparison for recovery and scrub |
| io_uring | J. Axboe, "Efficient IO with io_uring", 2019 (Linux 5.1 and later). <https://kernel.dk/io_uring.pdf> | [The reactor](data-path.md#the-reactor) |
| Shared-nothing, thread per core | Seastar, "Shared-nothing Design". <https://seastar.io/shared-nothing/> | The reactor model |
| CRC32C | G. Castagnoli, S. Brauer, M. Herrmann, "Optimization of Cyclic Redundancy-Check Codes with 24 and 32 Parity Bits", *IEEE Transactions on Communications* 41(6), 1993 | Per-block and frame checksums |
| FNV-1a | G. Fowler, L. C. Noll, K.-P. Vo, the FNV hash. <http://www.isthe.com/chongo/tech/comp/fnv/> | Object digests, name hashing |
| SplitMix64 | G. L. Steele, D. Lea, C. H. Flood, "Fast Splittable Pseudorandom Number Generators", OOPSLA '14 | The integer hash finalizer |
| Bε-trees | M. A. Bender, M. Farach-Colton et al., "An Introduction to Bε-trees and Write-Optimization", USENIX ;login:, October 2015. <https://www.usenix.org/publications/login/oct15/Bender>; G. S. Brodal and R. Fagerberg, "Lower Bounds for External Memory Dictionaries", SODA '03 | The planned metadata index |
| Copy-on-write B-trees | O. Rodeh, "B-trees, Shadowing, and Clones", *ACM Transactions on Storage*, 2008 | The planned block map index, snapshots and clones |
| Percolator | D. Peng and F. Dabek, "Large-scale Incremental Processing Using Distributed Transactions and Notifications", OSDI '10 | The planned transactional key–value layer |

## Interfaces

| Interface | Reference |
|---|---|
| NVIDIA GPUDirect Storage (cuFile) | Release notes and support list: <https://docs.nvidia.com/gpudirect-storage/release-notes/index.html>; API: <https://docs.nvidia.com/gpudirect-storage/api-reference-guide/index.html> |
| vhost-user | QEMU, "Vhost-user Protocol" (including the in-flight buffer, `INFLIGHT_SHMFD`). <https://www.qemu.org/docs/master/interop/vhost-user.html>; used by [`hyperstor-vhost`](decisions.md#vm-disks-over-vhost-user-blk-one-service-per-host) |
| virtio-blk | OASIS, *Virtual I/O Device (VIRTIO) Version 1.2*, §5.2 Block Device (topology fields for 512e). <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html> |
| rust-vmm | The `vhost`, `virtio-queue` and `vm-memory` crates. <https://github.com/rust-vmm> |
| Cloud Hypervisor and QEMU | The hypervisors VM disks are tested with. <https://www.cloudhypervisor.org/>, <https://www.qemu.org/> |
| ublk | Linux kernel documentation, "Userspace block device driver (ublk driver)", Linux 6.0 and later. <https://docs.kernel.org/block/ublk.html>; used by [`hyperstor-ublk`](decisions.md#its-own-ublk-server-on-the-reactor) |
| ext4 feature flags | Linux kernel documentation, "The Super Block" (compatible, read-only-compatible and incompatible features). <https://docs.kernel.org/filesystems/ext4/super.html>; the model for [device feature flags](decisions.md#devices-name-themselves-and-carry-feature-flags) |
| dm-delay | Linux kernel documentation, "dm-delay". <https://docs.kernel.org/admin-guide/device-mapper/delay.html>; used to check device timeouts |
| NVMe over Fabrics | NVM Express specifications. <https://nvmexpress.org/specifications/> |
| NFSv4.1 and pNFS | RFC 8881, "Network File System (NFS) Version 4 Minor Version 1 Protocol" (the files layout in chapter 13); RFC 8435, "Parallel NFS (pNFS) Flexible File Layout"; RFC 9289, "Towards Remote Procedure Call Encryption by Default" (RPC-with-TLS). <https://www.rfc-editor.org/rfc/rfc8881>, <https://www.rfc-editor.org/rfc/rfc8435>, <https://www.rfc-editor.org/rfc/rfc9289>; the [proposed NFS front end](decisions.md#nfs-and-pnfs-a-settled-direction-the-rest-proposed) |

## Security and identity

| Topic | Reference | Where it is used |
|---|---|---|
| Cedar | The Cedar policy language. <https://www.cedarpolicy.com/>; J. W. Cutler et al., "Cedar: A New Language for Expressive, Fast, Safe, and Analyzable Authorization", OOPSLA 2024 | [Roles](decisions.md#administration) over every cluster API call |
| OpenID Connect | OpenID Foundation, "OpenID Connect Core 1.0". <https://openid.net/specs/openid-connect-core-1_0.html> | Logins with an identity provider |
| OAuth 2.0 and PKCE | RFC 6749, "The OAuth 2.0 Authorization Framework"; RFC 7636, "Proof Key for Code Exchange by OAuth Public Clients". <https://www.rfc-editor.org/rfc/rfc6749>, <https://www.rfc-editor.org/rfc/rfc7636> | The authorization code flow behind OIDC logins |
| PAM | Linux-PAM. <https://github.com/linux-pam/linux-pam> | Password logins, through a root helper |
| AES-XTS | IEEE Std 1619, "Cryptographic Protection of Data on Block-Oriented Storage Devices"; NIST SP 800-38E. <https://csrc.nist.gov/pubs/sp/800/38/e/final> | The [designed encryption at rest](decisions.md#encryption-at-rest-designed-not-built) |

## Products

| Product | Sources |
|---|---|
| Ceph | v20.2.0 Tentacle: <https://www.ceph.io/en/news/blog/2025/v20-2-0-tentacle-released/>; Crimson: <https://ceph.io/en/developers/crimson/>; full ratios: <https://docs.ceph.com/en/latest/rados/configuration/mon-config-ref/#storage-capacity>, <https://docs.ceph.com/en/latest/rados/operations/health-checks/> |
| Longhorn | v1.9: <https://longhorn.io/blog/longhorn-v1.9.0/>; V2 data engine: <https://documentation.suse.com/cloudnative/virtualization/v1.9/en/storage/longhorn-v2-data-engine.html> |
| OpenEBS Mayastor | <https://github.com/openebs/mayastor> |
| DRBD and LINSTOR | <https://github.com/LINBIT/drbd>, <https://github.com/LINBIT/linstor-server> |
| Portworx | Internal KVDB (3.5): <https://docs.portworx.com/portworx-enterprise/3.5/concepts/kvdb-for-portworx/internal-kvdb>; run-flat: <https://docs.portworx.com/concepts/run-flat> |
| Lightbits | <https://www.lightbitslabs.com/> |
| simplyblock | SNIA SDC session: <https://www.snia.org/sniadeveloper/session/18450> |
| DAOS | Architecture: <https://github.com/daos-stack/daos/blob/release/2.4/docs/overview/architecture.md> |
| WEKA | Networking: <https://docs.weka.io/5.0/weka-system-overview/networking-in-wekaio> |
| VAST Data | <https://vastdata.com/blog/providing-resilience-efficiently-part-ii> |
| MinIO AIStor | RDMA: <https://docs.min.io/aistor/operations/rdma/> |
