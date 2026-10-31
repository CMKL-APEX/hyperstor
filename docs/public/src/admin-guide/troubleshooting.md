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
| PGs stay `degraded` for ten minutes after a node stops, with nothing copied | A down node keeps its place until `--down-out-secs` pass, so a reboot moves no data | Expected; take its devices out (`hyperstorctl out`) to copy its data sooner |
| `hyperstord: reactor failed: device D: the device holds device E of cluster C, not device D of cluster K` | The path given for device D is another device's (two paths swapped, a drive moved from another host or cluster) | Correct the `--shard` path; Hyperstor never serves or formats it. A drive meant to join afresh must be wiped first |
| A device is out and its node is up | The device returned an I/O error and was marked failed | Replace it, restart the node, then `hyperstorctl in --device D` |
| `hyperstorctl status` warns `PG(s) inconsistent` | Scrub found copies with no majority to repair from | Look at the storage node's log for the objects |
| A client's `attach` fails with `LeaseHeld` after the old holder crashed | The old holder's lease hasn't run out yet (up to 4 s) | Use `take_over`, which waits for it and blocklists the old holder |
| All I/O stops | The map service is down, or fewer than `min` copies of some PGs are up | Check `hyperstor-meta` and the storage nodes |
| A storage node never serves after start | Its addresses don't match the cluster file, or it can't reach the map service | Check `--node`, `--shard` addresses and `--meta` |
| `Disk quota exceeded` or `No space left on device` | A device file can't grow to its size | Free space or give a smaller size |
| `hyperstord` fails to open a block device | A size other than `0` was given for a block device | Use `/dev/...:0` |
| `hyperstord` exits with `the device has format version N, newer than this build reads` or `the device uses features this build doesn't have` | The device was formatted or upgraded by a newer `hyperstord`; this build refuses it rather than misread it | Run the newer build again (rolling back below a format change isn't possible) |
| `hyperstord` exits with `corrupt metadata: device superblock: no copy passes its checksum` or `shard N: no valid superblock` | The device's superblocks are damaged. `hyperstord` formats only blank devices, so it leaves this one as it is | Take the device out (`hyperstorctl out`) and let its PGs recover from the other copies; then wipe it and add it back |
