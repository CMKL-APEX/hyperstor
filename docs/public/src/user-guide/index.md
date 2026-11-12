# Using Hyperstor

Data lives in **volumes**: sparse block devices in a pool, attached by
one client at a time (see [Concepts](../concepts.md#volumes)), and in
**objects** in S3 buckets. There are four ways in today:

| Way in | For | Page |
|---|---|---|
| The Rust client library, `hyperstor-client` | Programs that read and write volumes themselves | [The client library](client-library.md) |
| [`hyperstor-vhost`](../reference/hyperstor-vhost.md) | Virtual machines: a volume is a VM's virtio disk, over vhost-user-blk, under Cloud Hypervisor or QEMU. glidex's `hyperstor` datastores use it | [Hosts that run VMs](../admin-guide/installation.md#hosts-that-run-vms) |
| [`hyperstor-ublk`](../reference/hyperstor-ublk.md) | Testing: a volume as a Linux block device, `/dev/ublkbN`, for any filesystem or tool | Its reference page |
| [`hyperstor-s3`](../reference/hyperstor-s3.md) | Objects: any S3 client or SDK, with an access key made by an administrator | [S3 gateways](../admin-guide/installation.md#s3-gateways), [Object storage](../admin-guide/managing.md#object-storage) |

Volumes are made, snapshotted, cloned and deleted through the client
library, or by administrators with
[`hyperstorctl`](../reference/hyperstorctl.md) and the web UI (see
[Volumes and snapshots](../admin-guide/managing.md#volumes-and-snapshots)).
Block devices for remote hosts (NVMe over TCP) and a filesystem are
planned (see [Status](../status.md)).

- [The client library](client-library.md): connecting, volumes,
  attaching, snapshots and clones, the trash, the read cache.
- [Data guarantees and errors](guarantees.md): what a successful write
  means, what happens when devices fill up, and what each error means
  for your program.

To use the client library you need the addresses of the cluster's map
service members from its administrator, and a pool number to write to.
