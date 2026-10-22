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
| vhost-user | QEMU, "Vhost-user Protocol". <https://www.qemu.org/docs/master/interop/vhost-user.html> |
| NVMe over Fabrics | NVM Express specifications. <https://nvmexpress.org/specifications/> |

## Products

| Product | Sources |
|---|---|
| Ceph | v20.2.0 Tentacle: <https://www.ceph.io/en/news/blog/2025/v20-2-0-tentacle-released/>; Crimson: <https://ceph.io/en/developers/crimson/> |
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
