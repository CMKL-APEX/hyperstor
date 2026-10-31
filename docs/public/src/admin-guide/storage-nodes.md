# Storage nodes

`hyperstord` runs one storage node. Each `--shard` gives it one device
and the address it serves that device on:

```sh
hyperstord --node 3 --meta 192.0.2.10:9000 \
    --shard 192.0.2.23:9100=/var/lib/hyperstor/dev0.img:100 \
    --shard 192.0.2.23:9101=/dev/nvme1n1:0 \
    --cpus 2,3
```

All options are in the [reference](../reference/hyperstord.md).

## Devices

A device is a file or a block device:

- **A file** is created if needed and extended to the size given, in
  GiB, after the colon. It is sparse at first and fills up as data is
  written; keep that much space free on its filesystem.
- **A block device** is used whole. Give its size as `0`
  (`/dev/nvme1n1:0`).
- A **file** without Hyperstor's format is **formatted on first start**.
  A **block device** never is: format it first with `sudo hyperstor-install
  format-device --path /dev/disk/by-id/NAME --device D`, which refuses a
  device that holds anything else (a partition table, LVM, a Ceph OSD,
  a filesystem...) unless `--force-format`. A node given a blank block
  device stops, saying so. Name block devices by `/dev/disk/by-id/`:
  `/dev/nvmeXnY` numbers can change at boot.
- A device that holds another device's data, or another cluster's, is
  **refused**: the node stops and says whose it is (swapped paths, a
  drive from another host). Nothing is written to it.

Devices are opened with `O_DIRECT` and driven through io_uring.
`--buffered` goes through the page cache instead, for filesystems that
refuse `O_DIRECT`; use it for testing only.

## Hard disks

Flash devices write every change to a new place. `--hdd` tells the node
its devices are hard disks: small overwrites then go through the log
first and are applied in place later, which suits disks' slow seeks.
It applies to every device of the node.

## CPUs

Each device is served by one thread. `--cpus 2,3` pins the thread of the
first `--shard` to CPU 2 and the second to CPU 3. Pin to cores that
nothing else busy uses, and keep them on the same NUMA node as the
device and the network card where you can.

## Starting, stopping and restarting

- A node stops on `SIGINT` or `SIGTERM` by exiting.
- On the next start, each device recovers on its own: it replays its log
  and checks the last writes against their checksums. Writes that were
  acknowledged are never lost.
- While a node is stopped, the map service marks it down after
  `--down-after-ms`, and its placement groups go on without it. When it
  starts again it is refilled (see [Failures and recovery](failures.md)).
- Keep a node's number and device paths the same across restarts: they
  identify its data.
