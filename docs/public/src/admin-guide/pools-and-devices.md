# Pools and devices

How to change a running cluster's devices and pools: take devices out
for service and back in, add and remove them, set their weights and
classes, handle devices that fill, and make, split and delete pools.
Every change is made with `hyperstorctl` or in the web UI, while the
cluster serves; how to plan them is in [Planning a cluster](planning.md).

## Taking a device out for service

```sh
hyperstorctl out --node 2 --wait 600   # or --device D; waits for the PGs to move
...                                    # service the node
hyperstorctl in --node 2 --wait 600    # and for them to come back
```

A device that is **out** gets no placement: its PGs go to other devices,
which are refilled from the remaining copies. With as many storage nodes
as copies, there is nowhere to move them, so the PGs run with one copy
fewer (`degraded`) until the device is back **in**. Writes go on while
each PG keeps at least the pool's `min` copies.

A device deletes its copy of a PG it no longer serves a minute after
the PG is clean on its other devices, so space comes back after
any move: out, drain, a weight change, or a device added. Its log says
`deleted its copy of PG P.G`. A device brought back in is backfilled.

> **Without `--data`, out doesn't survive a map service restart.** A
> map service without a store reads the cluster file when it starts and
> doesn't keep changes made since, so after a restart every device is in
> again. With `--data` (every install does this) the map, out devices
> included, is kept.

## Device weights

Each device gets PGs in proportion to its **weight**. By default the
weight is the device's capacity in TiB, which the map service takes
from the device's own reports: a device twice the size gets twice the
data, and devices of one size get the same. A newly joined host gets
no PGs until its devices have reported, within a second of starting.
`hyperstorctl nodes` shows each weight.

To place less, or more, on a device than its size says, set its weight;
to go back, set it to `auto`:

```sh
hyperstorctl weight --device 2001 --weight 0.5    # kept until set back
hyperstorctl weight --device 2001 --weight auto   # follows its capacity
```

A weight you set is marked `*` in `hyperstorctl nodes`. The device's
PGs move as soon as the weight changes, as for `out` and `in`. Setting
a weight changes the map's format: do it only once every storage node
and map service member runs a build that has `hyperstorctl weight`.

A pool's usable space (`hyperstorctl pools`) counts what it can hold
with each copy on a different host (or rack): with as many hosts as
copies, the smallest host bounds it.

## Full devices

A thin-provisioned cluster can run out of space. Each device is held to
three thresholds, percentages of its data area, the same for every
device:

| Threshold | Default | Past it |
|---|---|---|
| `nearfull` | 85 % | Health warns |
| `backfillfull` | 90 % | Health warns, and no PG is backfilled onto the device; a PG that needs it waits, degraded or remapped, until there is room |
| `full` | 95 % | Health is an error. The device refuses what would grow its used space: new objects and writes to blocks not yet written |

A pool is `nearfull` or `full` while any device it uses is:
`hyperstorctl pools` shows it with the device, `hyperstorctl nodes`
each device's state, and the web UI's **Pools** page the pool's. While a
pool is `full`, new volumes, clones and snapshots in it are refused
(`pool_full`). Each device's change is in the audit log, as operation
`DeviceFullness` by `hyperstor-meta`.

Past `full`, overwrites of written blocks, deletes, discards and
snapshot deletes still work, so you can always free space. A refused
write reaches a host's block device (`hyperstor-ublk`) as `ENOSPC`. A
VM's disk has no such error: `hyperstor-vhost` holds the write instead,
trying it again every second for up to 5 minutes (`--hold-full`), so
space freed in time lets it finish. Meanwhile the guest waits: Linux
logs a task blocked for more than 120 s, nothing worse. Past the hold
the guest sees an I/O error, which may abort its filesystem's journal
(ext4 then needs a remount). The service logs when it
starts and stops holding writes for a volume. Free space by deleting
volumes or snapshots and purging the trash, or add devices. A pool
whose trash is set to `--on-full purge` ([The trash](managing.md#the-trash))
purges its oldest trash by itself.

```sh
hyperstorctl full-ratios                       # the thresholds now
hyperstorctl full-ratios --full 97             # one or more of them
```

Each is between 50 and 99, with `nearfull` below `backfillfull` below
`full`. Setting them to anything but the defaults changes the map's
format: do it only once every storage node and map service member runs
a build that has `hyperstorctl full-ratios`.

## Adding a device to a node

A node can take more devices while the cluster runs. On the
node's host, if it runs the map service too:

```sh
sudo hyperstor-install install --node 1 --device /dev/disk/by-id/nvme-NAME
```

The installer refuses a device that holds anything it recognises (a
partition table, LVM, a Ceph OSD, a filesystem...; `--force-format`
overwrites it), registers it with the map service, formats it, lets
the `hyperstor` user open it (a udev rule for that disk only), records
it in `install.conf` and restarts the node with it. Later runs, the
daily renewal included, keep it. Name devices by `/dev/disk/by-id/`.

The device is the node's next one, `node × 1000 + i`, and starts at
weight 0: nothing moves until the node runs it. Once it has reported
its capacity, within a second of starting, it gets its weight and PGs
move onto it, as after a weight change.

By hand, or on a host without the map service: `hyperstorctl device
add --node 1 --address 192.0.2.21:9102` (from a host that has the
administrator's certificate), `hyperstor-install format-device --path
PATH --id D` on the node's host, then one more `--shard
192.0.2.21:9102=PATH:0` in the node's options (which a later installer
run rewrites).

## Removing a device

A device leaves once its data has moved:

```sh
hyperstorctl device remove --device 1001 --dry-run   # what would move; nothing changes
hyperstorctl device remove --device 1001 --wait 3600
```

The device is taken out at once, and its PGs move to the other devices
of their pools, under the backfill budget. An operation follows them;
once no PG lists the device, the map service removes it from the map,
and the node with it if it was the node's last device. Neither id is
given again. `hyperstorctl op cancel --id ID` before then brings the
device back in.

It is refused when a pool using the device would be left with fewer
failure domains (hosts, by default) than its copies. `--force` allows
it: the pool keeps fewer copies until a device in another domain is
added, but never fewer than its `min`. It is refused, forced or not,
when a device taking the data would pass 95% full. `--dry-run` shows
the PGs that move, the pools left short and each receiving device's
space after. Through `hyperstor-mgmt` it needs a fresh login (step-up).

Then drop the device from its node, on the node's host:

```sh
sudo hyperstor-install install --node 1 --remove-device 1001
```

The installer checks that the map no longer has the device (on a host
with the administrator's certificate), records it in `install.conf`
so later runs leave its `--shard` out too, and restarts the node
without it. A node started by hand with a removed device's `--shard`
stops, saying so. The device's data is left as it was; wipe it
separately. A node's last device leaves the map with the node:
uninstall the storage role there instead.

## Device classes

Each node reports what its devices are: `nvme`, `ssd`, `hdd`, or `file`
for a device file. A device takes its kind as its class, which
`hyperstorctl nodes` shows in the CLASS column, unless you set one:

```sh
hyperstorctl device class --device 2001 --class archive   # kept, marked +
hyperstorctl device class --device 2001 --class auto      # its kind again
```

A class holds devices of one kind, and a class named after a kind
(`nvme`, `ssd`, `hdd`, `file`) only that kind. Pools that name classes
place their PGs on those devices; the others use every device, so
classes change nothing for them. To limit a pool to some classes, or
let it use every device again:

```sh
hyperstorctl pool set --pool 1 --classes nvme,ssd
hyperstorctl pool set --pool 1 --classes any
```

The same command renames a pool or changes its failure domain, a copy
per device, host or rack; what isn't given stays:

```sh
hyperstorctl pool set --pool 1 --name blocks --domain rack
```

The classes' devices must span at least as many failure domains (hosts,
by default) as the pool keeps copies. A set with hard disks and flash
both is refused unless `--force`: a write waits for its slowest copy.
`.meta` always uses every device but hard disks.

A pool can also use one device beyond its classes, or leave out one of
them, without changing any device's class: other pools keep using it
as before.

```sh
hyperstorctl pool add-device --pool 1 --device 3001
hyperstorctl pool remove-device --pool 1 --device 2000
```

Each is checked as `pool set` is, and its PGs move the same way;
`hyperstorctl pools` shows the classes, then `+3001-2000`. A device's
space is shared by every pool that uses it, so a pool has no size of
its own: its free space is that of its devices.

A pool is made on the classes it names:

```sh
hyperstorctl pool create --name fast --classes nvme --copies 3
```

It gets about 100 PGs per device of its classes per copy, as a power
of two, unless `--pgs` says otherwise; the PG count can't grow yet, so
give more if many devices are to come. Volumes go to it with
`create-volume --pool P`. Kinds appear once every running node
has a build that reports them; a node from before that starts after
that can't read the map, so upgrade it first.

## Splitting a pool's PGs

A pool's PG count doubles in place: each PG splits into itself and
a new one, with no data copied.

```sh
hyperstorctl pool set --pool 1 --pgs 128 --wait 600
```

The map service splits a few PGs at a time, the clean ones. Each PG's
devices pause its I/O for a moment, move the objects that now belong to
the new PG to its keys, and report; once all have, clients use the new
PG. Volumes stay attached and writes go on, retried over each
pause. `hyperstorctl pools` shows how far it has got. The new PGs start
on their parents' devices; then, whenever every PG of the pool is clean,
the map service places a few more on their own devices, and they move
under the backfill budget, until all are placed. A cache pool keeps its data
through all of it: its new PGs are copied to their devices, not started
empty, and a restart doesn't empty its devices either. A split is refused for
any count but twice the pool's, while one runs, and until the last
one's PGs have moved. A device that was down during the
split catches up when it returns.

A cache pool also waits for a node that goes down: a PG whose
copies are all on down nodes stays there for the pool's grace, 60
seconds unless set with `hyperstorctl pool set --pool P --cache-grace
SECS` (0 to 3600; 0 doesn't wait), so a host that restarts, or comes back
from a short crash, keeps its share of the cache. Meanwhile reads of
those PGs miss (a volume reads zeros there), health warns that they are
waiting, and a write to one moves it at once to another device, empty.
A PG moved that way, or because the grace passed, moves back to the node
when it returns and starts empty there.

The map service also splits on its own: when a pool's devices
call for at least twice its PGs (about 100 per device, counting copies),
typically after devices were added, it doubles the count, one pool at a
time and only while every PG is clean. Its log names each split. To keep
a pool's count as set:

```sh
hyperstorctl pool set --pool 1 --autoscale off
```

`hyperstorctl pools` lists the pools with it off; `pool create` takes
`--autoscale off` too.

## Deleting a pool

A pool is deleted once it holds nothing: no volume, no snapshot, and
nothing in the trash. List what it holds, delete and purge
that, then delete it:

```sh
hyperstorctl pool delete --pool 3 --dry-run
hyperstorctl pool delete --pool 3
```

It leaves the map at once, and clients using it get `not_found`. Its
id is never given to another pool. Each device deletes the pool's data
about a minute later; a device that was down does when it returns.
Through `hyperstor-mgmt` it needs a fresh login (step-up).

To delete a pool with what it holds, list it first, then force:

```sh
hyperstorctl pool delete --pool 3 --dry-run
hyperstorctl pool delete --pool 3 --force --wait 600
```

An operation moves each volume to the trash, preempting a client that
has it attached, and purges it with its snapshots, each audited; then
it deletes the pool. Nothing of it can be restored.
