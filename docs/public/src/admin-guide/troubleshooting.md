# Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `hyperstord` exits with `bind ADDR: Address already in use` | Another process, or an old `hyperstord`, holds the port | Stop it, or change the address in both the cluster file and `--shard` |
| `hyperstord` or `hyperstor-meta` prints its usage line and exits | A missing or malformed option | Compare with the [reference](../reference/index.md) |
| `hyperstord: reactor failed: ...` with `Invalid argument` when opening a device | The filesystem refuses `O_DIRECT` | Put device files on a filesystem that supports it, or use `--buffered` for testing |
| Reactors fail to start with `Operation not permitted` | io_uring is disabled | Set the `kernel.io_uring_disabled` sysctl to `0` |
| `hyperstor-meta` prints `line N: ...` and exits | A mistake in the cluster file; the message names the line | See the [cluster file reference](../reference/cluster-file.md) |
| `--down-after-ms must be at least 1.25 x --lease-ms` | Detection faster than leases expire, which would let two nodes serve one PG | Raise `--down-after-ms` or lower `--lease-ms` |
| Clients fail with `Exhausted` right after a node stops | The node isn't marked down yet | Wait `--down-after-ms`; clients should retry |
| All I/O stops | The map service is down, or fewer than `min` copies of some PGs are up | Check `hyperstor-meta` and the storage nodes |
| A storage node never serves after start | Its addresses don't match the cluster file, or it can't reach the map service | Check `--node`, `--shard` addresses and `--meta` |
| `Disk quota exceeded` or `No space left on device` | A device file can't grow to its size | Free space or give a smaller size |
| `hyperstord` fails to open a block device | A size other than `0` was given for a block device | Use `/dev/...:0` |
