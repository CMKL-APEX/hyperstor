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
  format-device --path /dev/disk/by-id/NAME --id D`, which refuses a
  device that holds anything else (a partition table, LVM, a Ceph OSD,
  a filesystem...) unless `--force-format`. A node given a blank block
  device stops, saying so. Name block devices by `/dev/disk/by-id/`:
  `/dev/nvmeXnY` numbers can change at boot.
- A device that holds another device's data, or another cluster's, is
  **refused**: the node stops and says whose it is (swapped paths, a
  drive from another host). Nothing is written to it. Each device's
  superblock records its device number and its cluster for this check.
- A device the map doesn't have, or has on another node or at another
  address, is refused too. `--shard ID@ADDR=PATH:SIZE` names the
  device's number when it isn't the node's next (after a device was
  removed, say).
- A device formatted by a newer build, with features this build
  doesn't know, is refused rather than misread; so is one whose
  superblocks are damaged. Neither is formatted again.

Devices can be added to a running node and removed from it, and each
gets PGs in proportion to its weight (its capacity, unless set): see
[Managing a cluster](pools-and-devices.md#adding-a-device-to-a-node).

Devices are opened with `O_DIRECT` and driven through io_uring.
`--buffered` goes through the page cache instead, for filesystems that
refuse `O_DIRECT`; use it for testing only.

## Space on a device

- Each device keeps room for its next checkpoint, so a device that fills
  up keeps working: it refuses new data, while overwrites and deletes go
  on and free space.
- Past the cluster's `full` threshold (95 % by default) a device refuses
  new objects and writes to blocks not yet written. See
  [Full devices](pools-and-devices.md#full-devices).
- A device deletes its copy of a PG it no longer serves a minute after
  the PG is clean on its other devices, so the space comes back after
  data moves.
- Checkpoints write only the parts of the index that changed since the
  last one, so their cost follows the rate of change, not the size of
  the device.

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

The thread doesn't wait for its device on commits and client reads: it
answers its peers' pings and takes new requests meanwhile, and only the
requests that need the commit in flight wait for it. A commit
that takes longer than `--io-timeout-secs` (30 s), or three reads in a
row that take longer than `--read-timeout-ms` (2 s), fail the device
(see [Failures and recovery](failures.md#a-device-fails)). Checkpoints,
and reads for backfill and scrub, still run on the thread. The node logs
each time the thread is held 100 ms or more (see
[Health](managing.md#health)).

## Starting, stopping and restarting

- A node stops on `SIGINT` or `SIGTERM` by exiting.
- On the next start, each device recovers on its own: it replays its log
  and checks the last writes against their checksums. Writes that were
  acknowledged are never lost. Each device logs how long it took to open
  (`device D open in N ms`).
- The node is marked up only once every device of it is open, so a slow
  device delays the node rather than making it go down and up.
- While a node is stopped, the map service marks it down after
  `--down-after-ms`, and its placement groups go on without it. When it
  starts again it stays down for at least 2 s after it was marked down,
  then is refilled (see
  [Failures and recovery](failures.md#a-storage-node-returns)).
- Keep a node's number and device paths the same across restarts: they
  identify its data, and a device given the wrong path is refused.
