# hyperstord

A storage node. See [Storage nodes](../admin-guide/storage-nodes.md).

```text
usage: hyperstord --node N --meta ADDR --shard [ID@]ADDR=PATH:SIZE_GIB [--shard ...] [--cpus 2,3] [--buffered] [--hdd] [--admin ADDR] [--scrub-hours N] [--io-timeout-secs N] [--read-timeout-ms N] [--endpoint DEV=ADDR ...] [--networks NAME[,NAME...]]
```

| Option | Default | Meaning |
|---|---|---|
| `--node N` | required | This node's number in the [cluster file](cluster-file.md) |
| `--meta ADDR` | required | `IP:PORT` of the map service; with several members, all of them separated by commas (`IP:PORT,IP:PORT,…`), and the node finds the leader |
| `--shard [ID@]ADDR=PATH:SIZE_GIB` | at least one | One device: serve it on `ADDR` (as listed for this node in the cluster file, in order), store it in `PATH` (a file of `SIZE_GIB` GiB, created if needed, or a block device with size `0`, formatted first with `hyperstor-install format-device`: `hyperstord` formats only files). Repeat for each device. The device is `ID` if given (one of the node's, `N × 1000` to `N × 1000 + 999`), else `N × 1000 +` its position among the `--shard` options; the map must have it on this node at `ADDR`, or the node stops (it asks the map again for 5 s first, so a device just added with `hyperstorctl device add` is found) |
| `--cpus LIST` | not pinned | Comma-separated CPUs; the i-th `--shard`'s thread runs on the i-th CPU |
| `--buffered` | off | Use the page cache instead of `O_DIRECT`, for testing |
| `--hdd` | off | The devices are hard disks: defer small overwrites through the log |
| `--admin ADDR` | off | Serve metrics (`GET /metrics`, Prometheus format) and `GET /health` on `IP:PORT`; plain HTTP, read-only: keep it on the management network |
| `--scrub-hours N` | 24 | Compare every copy of each placement group this node leads, every N hours, and repair a copy the others outvote; the first pass and every seventh also read every block and check its checksum; `0` turns it off |
| `--io-timeout-secs N` | 30 | How long a device may take to write and flush a commit before the node fails it: its placement groups go elsewhere, as after an I/O error, and the node stays up. At least 1 |
| `--read-timeout-ms N` | 2000 | How long a read may wait for its device. Past it, the read is served from another copy of the placement group; three in a row fail the device, as after an I/O error. At least 1 |
| `--endpoint DEV=ADDR` | none | Also serve device `DEV` on `IP:PORT`: one of its endpoints on another network, as the [cluster file](cluster-file.md#endpoint) lists it. Repeat for each endpoint. `DEV` must be one of this node's `--shard` devices |
| `--networks NAME[,NAME...]` | none | The data networks this node reaches other storage nodes on, in order: a peer's endpoint on the first network it has one on, else its device's address. Without it, devices' addresses only |

**Diagnostic build.** Built with `HYPERSTOR_DIAG=1 cargo build --release
-p hyperstor-daemon`, a node also logs, to standard error (the journal),
what it otherwise does silently: each backfill attempt and why it fails,
attempts that run long, and waits for queued writes; each kind of line at
most once every 10 s. Use it to find out why a placement group stays
below its copies; run ordinary builds otherwise.

Devices without Hyperstor's format are formatted on first start. The node
runs until `SIGINT` or `SIGTERM`, and recovers on the next start.

| Exit status | Meaning |
|---|---|
| 1 | An address (including `--admin` and `--endpoint`) can't be bound, or a device's thread failed (the message says why) |
| 2 | A missing or malformed option |
