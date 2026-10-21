# Using Hyperstor

Today a program stores data in Hyperstor through the Rust client
library, `hyperstor-client`. Block devices for virtual machines and hosts
(vhost-user-blk, NVMe over TCP), an S3 API and a filesystem are planned
(see [Status](../status.md)); until then, the client library is the only
way in.

- [The client library](client-library.md): connecting, writing and
  reading.
- [Data guarantees and errors](guarantees.md): what a successful write
  means, and what each error means for your program.

You need the address of the cluster's map service from its
administrator, and a pool number to write to.
