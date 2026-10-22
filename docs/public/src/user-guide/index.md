# Using Hyperstor

Today a program stores data in Hyperstor through the Rust client
library, `hyperstor-client`. For testing, a volume can also be served as
a Linux block device with [`hyperstor-ublk`](../reference/hyperstor-ublk.md),
and used through any filesystem or tool. Block devices for virtual
machines and remote hosts (vhost-user-blk, NVMe over TCP), an S3 API and
a filesystem are planned (see [Status](../status.md)).

- [The client library](client-library.md): connecting, volumes,
  attaching, snapshots and clones.
- [Data guarantees and errors](guarantees.md): what a successful write
  means, and what each error means for your program.

You need the address of the cluster's map service from its
administrator, and a pool number to write to.
