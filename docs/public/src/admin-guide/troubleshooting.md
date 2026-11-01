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
| `hyperstord: reactor failed: device D: a blank block device; hyperstord formats files only` | The node was given an unformatted block device | `sudo hyperstor-install format-device --path /dev/disk/by-id/NAME --id D`, then start the node |
| `hyperstor-install: PATH holds an LVM physical volume (or another signature): not formatted` | `format-device` found another system's data on the device | Check the path: it may be a disk in use. If the data is meant to go, `--force-format` |
| `hyperstord: reactor failed: device D: not in the map: add it first (hyperstorctl device add ...), or check --shard` | The node was given a `--shard` for a device the map doesn't have: a typo, or a device not added yet | `hyperstorctl device add --node N --address ADDR`, or correct the `--shard`; nothing was written to the device |
| `hyperstord: reactor failed: device D: the map has it on node M at ADDR, not node N at ADDR2: check --shard` | The `--shard` names a device id the map has elsewhere | Correct the id (`ID@`) or the address in `--shard` |
| `hyperstord: reactor failed: device D: the device holds device E of cluster C, not device D of cluster K` | The path given for device D is another device's (two paths swapped, a drive moved from another host or cluster) | Correct the `--shard` path; Hyperstor never serves or formats it. A drive meant to join afresh must be wiped first |
| A device is out and its node is up | The device returned an I/O error and was marked failed | Replace it, restart the node, then `hyperstorctl in --device D` |
| `hyperstorctl status` warns `PG(s) inconsistent` | Scrub found copies with no majority to repair from | Look at the storage node's log for the objects |
| A client's `attach` fails with `LeaseHeld` after the old holder crashed | The old holder's lease hasn't run out yet (up to 4 s) | Use `take_over`, which waits for it and blocklists the old holder |
| All I/O stops | The map service is down, or fewer than `min` copies of some PGs are up | Check `hyperstor-meta` and the storage nodes |
| A storage node never serves after start | Its addresses don't match the cluster file, or it can't reach the map service | Check `--node`, `--shard` addresses and `--meta` |
| `hyperstord` logs `Disk quota exceeded` or `No space left on device` | A device file can't grow to its size on its filesystem | Free space there, or give a smaller size |
| Health warns `device D nearfull` or `backfillfull`, or errs `device D full` | The device is past a [fullness threshold](pools-and-devices.md#full-devices) | Delete volumes or snapshots and purge the trash (`hyperstorctl trash purge`), or add devices. Raising `hyperstorctl full-ratios --full` buys time only |
| `create-volume`, `clone-volume` or `snapshot create` fails with `(pool_full)` | A device the pool uses is past `full` | As above; `hyperstorctl pools` names the device |
| Writes on a `hyperstor-ublk` device fail with `ENOSPC`, or a VM's filesystem turns read-only after I/O errors | A device of the pool is past `full`; `hyperstor-vhost` held the VM's writes for `--hold-full` (5 minutes by default) first. Its log says `holding writes` for the volume | Free space as above; a VM's held writes finish if space comes back in time. Then check the guest's filesystem |
| A PG stays degraded or backfilling and a device is past `backfillfull` | No PG is backfilled onto a device past `backfillfull` | Free space, or add devices |
| Health warns `node N flapping` | The node goes down and up: something holds it longer than its peers wait | Look at `journalctl -u hyperstord@N` for `reactor held`, slow commits and slow device opens; see [Health](managing.md#health) |
| Health warns `node(s) not heard from since the map service started` | A node in the map hasn't started, or can't reach the map service | Start it, or check its `--meta` and the network |
| A node logs `answers no UDP pings at ADDR; pinging it over TCP` | A firewall drops UDP on the storage ports | Open UDP on the storage ports (see [Planning](planning.md#network)); failures are slower to notice meanwhile |
| Health warns `cache PG(s) waiting up to 60 s for their node (reads miss)` | A node with cache PGs is down; they wait for it for the pool's grace | Expected after a restart; it clears when the node is back. See [Cache pools](failures.md#cache-pools) |
| `hyperstorctl pg-grid --usage` exits with status 3, naming the leader | Only the map service's leader has the PGs' usage | Ask the leader (`--api`), or use the web UI |
| `hyperstord` fails to open a block device | A size other than `0` was given for a block device | Use `/dev/...:0` |
| `hyperstord` exits with `the device has format version N, newer than this build reads` or `the device uses features this build doesn't have` | The device was formatted or upgraded by a newer `hyperstord`; this build refuses it rather than misread it | Run the newer build again (rolling back below a format change isn't possible) |
| `hyperstord` exits with `corrupt metadata: device superblock: no copy passes its checksum` or `shard N: no valid superblock` | The device's superblocks are damaged. `hyperstord` formats only blank devices, so it leaves this one as it is | Take the device out (`hyperstorctl out`) and let its PGs recover from the other copies; then wipe it and add it back |
| A browser warns about mgmt's certificate | It comes from the cluster's own CA | Import `/etc/hyperstor/pki/root.crt` as a trusted authority, or give mgmt a certificate browsers trust (see [Dashboard](managing.md#dashboard)) |
| A login to the web UI fails with `wrong user name or password` | A wrong password, or a user not in the group `hyperstor-users` | `sudo usermod -aG hyperstor-users NAME` |
| A login fails with `too many failed attempts` | Five failures for one user in 15 minutes (or 30 a minute in all) | Wait, then try again |
| A login fails as `unavailable` | `hyperstor-authd`, which checks passwords, isn't answering | `systemctl status hyperstor-authd.socket hyperstor-authd` |
| Logged in, but the web UI shows nothing | The person has no role yet | A security administrator gives one; for the first, log in with the claim code from `journalctl -u hyperstor-mgmt` |
| `hyperstorctl --mgmt` fails with `(reauth_required)` | The call needs a login of the last 10 minutes, which an API token never is | Do it from the web UI after logging in again, or with the administrator's certificate |
| `hyperstorctl --mgmt` fails with `(unauthenticated)` | The API token expired or was revoked | Make a new one on the web UI |
