# hyperstord

A storage node. See [Storage nodes](../admin-guide/storage-nodes.md).

```text
usage: hyperstord --node N --meta ADDR --shard ADDR=PATH:SIZE_GIB [--shard ...] [--cpus 2,3] [--buffered] [--hdd]
```

| Option | Default | Meaning |
|---|---|---|
| `--node N` | required | This node's number in the [cluster file](cluster-file.md) |
| `--meta ADDR` | required | `IP:PORT` of the map service |
| `--shard ADDR=PATH:SIZE_GIB` | at least one | One device: serve it on `ADDR` (as listed for this node in the cluster file, in order), store it in `PATH` (a file of `SIZE_GIB` GiB, created if needed, or a block device with size `0`). Repeat for each device |
| `--cpus LIST` | not pinned | Comma-separated CPUs; the i-th `--shard`'s thread runs on the i-th CPU |
| `--buffered` | off | Use the page cache instead of `O_DIRECT`, for testing |
| `--hdd` | off | The devices are hard disks: defer small overwrites through the log |

Devices without Hyperstor's format are formatted on first start. The node
runs until `SIGINT` or `SIGTERM`, and recovers on the next start.

| Exit status | Meaning |
|---|---|
| 1 | An address can't be bound, or a device's thread failed (the message says why) |
| 2 | A missing or malformed option |
