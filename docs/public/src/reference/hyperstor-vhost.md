# hyperstor-vhost

Serves volumes to virtual machines on this node over vhost-user-blk: the
hypervisor (Cloud Hypervisor or QEMU) connects to a socket per volume,
and the guest sees a virtio disk. One service runs per compute node and
serves every VM on it. glidex attaches and detaches volumes through its
control socket; operators can do the same with `ctl`.

```text
usage: hyperstor-vhost serve --meta ADDR[,ADDR...] [--dir DIR] [--allow-uid UID]... [--group GID] [--queues N] [--hold-full SECS] [--hold-outage SECS] [--hold-standby SECS] [--networks NAME[,NAME...]] [--host NAME] [--identity DIR] [--cpus LIST] [--poll-us N] | ctl [--dir DIR] hello | list | attach ID [--serial S] [--take-over | --preempt | --delegate D] [--holder NAME] [--poll] [--for USER] | detach ID [--force] | delegate ID | undelegate ID D | promote ID
```

| Command | Does |
|---|---|
| `serve` | Runs the service until SIGINT or SIGTERM. Its volumes stay attached for its next start (see [Restarts](#restarts)) |
| `ctl hello` | Checks the service answers, and its protocol version |
| `ctl list` | The volumes attached here: socket, size, whether a hypervisor is connected, whether the volume was taken elsewhere (`lost`), the reactor serving it with that reactor's NUMA node (`reactor`, `node`), and whether it is polled (`poll`) |
| `ctl attach ID` | Takes the volume's read-write lease and opens its socket; prints the socket's path |
| `ctl detach ID` | Removes the socket and gives the lease up; refused (`busy`) while a hypervisor stays connected for 2 s, unless `--force` |
| `ctl delegate ID` | On a live migration's source: adds a delegate to the lease of the volume attached here, and prints its identity (see [Live migration](#live-migration)) |
| `ctl attach ID --delegate D` | On the target: attaches the volume as delegate `D`, standing by; `--holder NAME` labels the lease once promoted |
| `ctl promote ID` | On the target, at the switch: makes the delegate attached here the volume's holder |
| `ctl undelegate ID D` | On the source of a cancelled migration: removes delegate `D` and fences it |

| Option | Default | Meaning |
|---|---|---|
| `--meta ADDR[,ADDR...]` | | `serve`: the map service's members (their `--listen` addresses) |
| `--dir DIR` | `/run/hyperstor/vhost` | Where the control socket (`control.sock`) and the volumes' sockets (`<volume id>.sock`) are; at most 69 bytes, so a socket's path fits Unix's 107 |
| `--allow-uid UID` | | `serve`: a user allowed on the control socket besides root and the service's own; repeat for more. Each volume's socket is for the user who attached it: only they, root and the service connect (see [Who opens a disk](#who-opens-a-disk)) |
| `--group GID` | | `serve`: the group of volumes a state file from before protocol 4 names no user for; they go to the first `--allow-uid` user |
| `--queues N` | 1 | `serve`: queues each disk offers (1–64) |
| `--hold-full SECS` | 300 | `serve`: how long a write refused because a device is full is held, tried again every second, before the guest sees an I/O error (0–3600; 0: at once). See [Full devices](../admin-guide/pools-and-devices.md#full-devices) |
| `--hold-outage SECS` | 3600 | `serve`: how long a read or write is held while the cluster can't serve it (no primary for its data, the map service without a leader, storage nodes unreachable), tried again every second, before the guest sees an I/O error (0–86400; 0: after the client's own tries, about 30 s). See [Outages](#outages) |
| `--hold-standby SECS` | 30 | `serve`: how long a live migration's target holds a guest's writes before it is promoted, then fails them (0–3600; 0: at once). See [Live migration](#live-migration) |
| `--host NAME` | its hostname | `serve`: the name it gives storage nodes, by which a host is bound to realms (`hyperstorctl realm host`) |
| `--identity DIR` | none | `serve`: The host's identity on the storage network: `host.crt` and `host.key` in `DIR` (a host joined with `hyperstor-install --role vhost --join`), else `node.crt` or `api.crt` (a storage or map service host), with `root.crt`. With it, each connection to a storage node or the map service starts TLS where the cluster's mode allows. `hyperstor-install` sets it where the host has an identity |
| `--networks NAME[,NAME...]` | none | `serve`: the data networks this host reaches storage nodes on, in order: a device's endpoint on the first network it has one on (see the [cluster file](cluster-file.md#endpoint)), else its address |
| `--cpus LIST` | the first CPU of each NUMA node | `serve`: the CPUs its reactors run on, one reactor pinned to each (`0,64` or `0-1`). See [NUMA nodes](#numa-nodes) |
| `--poll-us N` | 50 | `serve`: how long a polled disk's queues stay polled after they were last busy, in microseconds (1–10000). See [Polling](#polling) |
| `--serial S` | empty | `attach`: the disk's serial number as the guest sees it, up to 20 characters |
| `--take-over` | off | `attach`: the volume's holder died or is cut off: wait until its lease runs out (up to 4 s), then take it |
| `--preempt` | off | `attach`: take the volume from a holder that is still running; that holder's lease on this volume is blocklisted, and its other volumes go on |
| `--delegate D` | | `attach`: attach as delegate `D` of the holder's lease, standing by, a live migration's target (see [Live migration](#live-migration)); not with `--take-over` or `--preempt` |
| `--holder NAME` | | `attach`: who asks, for the lease's label, `vhost:NAME`; glidex gives its node's name |
| `--poll` | off | `attach`: poll the disk's queues while it is busy, for latency-critical VMs. See [Polling](#polling) |
| `--for USER` | | `attach`, as root: the user the volume's socket is for, one of `--allow-uid`'s. Without it, the socket is for whoever attaches |
| `--force` | off | `detach`: detach even while a hypervisor is connected; the VM loses its disk |

Without `--take-over` or `--preempt`, `attach` is refused (`held`) while
another client has the volume attached. Attaching a volume that is
attached here already answers with its socket; one that was taken
elsewhere (`lost` in `list`, its I/O failing) is let go and attached
afresh.

`detach` is refused (`busy`) while a hypervisor is connected to the
volume's socket, so a running VM keeps its disk whoever asks: stop the
VM first, or pass `--force` (`"force": true`) to cut it off. A
hypervisor that is still exiting (a VM just stopped) gets 2 s to hang
up, so a detach right after stopping the VM works without `--force`;
one still connected then is answered `busy`. A volume taken elsewhere
detaches without `--force`. `hyperstor-install
--no-vhost` and `uninstall` force.

```sh
sudo hyperstor-vhost serve --meta 192.0.2.10:8849 --allow-uid "$(id -u)" &
id=$(hyperstorctl create-volume --size 20G --name vm-disk | awk '{print $2}')
sudo hyperstor-vhost ctl attach "$id" --serial vm-disk --for "$USER"
cloud-hypervisor --memory size=2G,shared=on \
  --disk vhost_user=true,socket=/run/hyperstor/vhost/$id.sock ...
```

### Who opens a disk

Each volume's socket is for the user who attached it: it has mode 0660
and that user's group, and only that user, root and the service may
connect; anyone else is cut off and the service logs it. Root attaching
for a hypervisor that runs as another user names that user with
`--for USER`. The directory has mode 0711: a socket is opened by its
path, and only the service lists them. The control socket takes any
user `--allow-uid` names.

The VM's memory must be shared with the service (`shared=on` in Cloud
Hypervisor; a `memory-backend-memfd` with `share=on` in QEMU). The disk
has 512-byte sectors on 4 KiB physical blocks and no volatile write
cache: a write is answered once every copy has it. Discard and
write-zeroes drop blocks rather than write them: one range of up to
16 MiB a request, discards on 4 KiB blocks (a partial block at either
end is left as it is), and write-zeroes may drop blocks
(`write_zeroes_may_unmap`). After a discard, a block a snapshot or the
clone's parent holds reads as theirs; after a write-zeroes, everything
reads as zeros.

The control socket takes one JSON request per line and answers each on
one line:

```text
{"id": 1, "op": "attach", "args": {"volume": "<32 hex digits>", "serial": "vm-disk", "take": "attach"}}
{"id": 1, "ok": {"socket": "/run/hyperstor/vhost/<id>.sock", "size": 21474836480}}
```

Errors are `{"id": N, "error": {"code": C, "message": M}}`, with codes
`invalid`, `denied`, `held`, `not_found`, `not_attached`, `busy`,
`unavailable` and `internal`. `hello` answers the protocol version
(4), its features (`live_migration`; `standby_hold`, a migration's
target holds the guest's writes until it is promoted) and `identity`: the name in the
certificate it shows storage nodes (`hyperstor://<cluster>/host/<name>`),
or `null` when it runs without `--identity`. An `attach` may name who asks, `"holder": "<name>"` (up
to 58 printable characters; glidex gives its node's name): the lease is
labelled `vhost:<name>` (`vhost` without one). A `held` error names the
label of whoever holds the volume, `"holder"`: `vhost:<name>` for
another node's service, `hyperstorctl` for a tool. `"poll": true`
polls the disk (see [Polling](#polling)); it is kept across restarts.

## NUMA nodes

The service runs a reactor, a thread doing all of a disk's I/O, on each
CPU of `--cpus`: by default the first CPU of each NUMA node, the core
glidex keeps for the host there. `hyperstor-install` passes the CPUs
glidex reserves (`--vhost-cpus`). One reactor serves about 35,000 4 KiB
reads a second. Each disk runs on one reactor: that of the node its
guest's memory is bound to, which the service reads from the guest's
memory when the hypervisor connects. Bind a VM's memory to a node for
this (Cloud Hypervisor: `--memory size=0 --memory-zone
id=mem0,size=2G,shared=on,host_numa_node=1`; QEMU:
`memory-backend-memfd,...,host-nodes=1,policy=bind`). A guest whose
memory isn't bound to one node starts on the reactor serving the fewest
disks; once the pages its I/O uses show which node its memory is on,
its disk moves, once, to that node's reactor (a pause of a few
milliseconds while requests in flight finish). `ctl list` shows where
each disk runs.

## Polling

A disk attached with `poll` has its queues watched while it has
requests running and for 50 µs after (`--poll-us`): the guest makes requests
available without notifying the service, and its reactor checks for
completions instead of sleeping. Measured with one VM, 4 KiB reads one
at a time were about a fifth faster (latency 133 µs to 109 µs) and
writes a tenth; at queue depth 32 nothing was gained. It costs about a
core while the disk is busy; idle, it costs nothing. Leave it off unless
a VM's latency matters that much.

## Memory per connection

Each request holds a buffer of its data while it runs (up to 8 MiB). A
hypervisor's connection may hold up to 64 MiB in them at once; a
request that would go past that waits for the connection's earlier ones
to finish, and none is refused for it. Linux guests send at most
1280 KiB per request, so they don't reach it.

## Live migration

The service logs the guest pages it writes while a hypervisor migrates
a VM live: it offers vhost-user's dirty-page log (`LOG_SHMFD`,
`VHOST_F_LOG_ALL`), and marks the pages each request writes and the
used ring's. When the hypervisor stops a queue, the service finishes the
requests in flight on it first (for up to 35 s), so none is lost and
their data reaches the new host.

glidex moves a running VM to another node with its disks attached
throughout. The volume's lease passes from the source's service to the
target's, which never writes before it holds it:

1. **On the source,** `delegate` adds a delegate to the disk's lease and
   answers its identity, `{"delegate": "<16 hex digits>"}`.
2. **On the target,** `attach` with `"delegate"` set to it (and
   `"holder"`) attaches the volume as that delegate, standing by: the
   receiving hypervisor connects to its socket, reads work, and writes
   wait for the promotion. It is checked with the cluster first: a cluster whose
   storage nodes can't hand a lease over (older builds) refuses it
   (`invalid`), before any memory is copied. `list` shows it as
   `"role": "delegate"`, which glidex's clean-up of stale holds leaves
   alone; a holder's disk is `"role": "holder"`.
3. **At the switch,** once the source's hypervisor has stopped its
   rings (the service finishes the requests in flight first), `promote`
   on the target makes it the volume's holder at once. The source's
   lease is fenced in the background, `{"fencing": [...]}`; the guest
   reads at once, and its first write waits until the fence is in place
   (one blocklist round). The source's renewals fail from then on, and
   detaching it there fences nothing of the target's. A `promote` sent
   again after a lost answer succeeds.

   A hypervisor may resume the guest before `promote` arrives (Cloud
   Hypervisor resumes it as soon as the source sends its last state).
   Its writes before then are held, never sent, and run once the disk is
   promoted. They fail as I/O errors if it isn't promoted within
   `--hold-standby` (30 s), or when the target is detached (a cancelled
   migration, step 4).
4. **A cancelled migration:** `detach` the target (with `force`, its
   hypervisor connected) and `undelegate` it on the source, which fences
   it. The source's VM goes on throughout.

A delegate standing by isn't in the state file: a restart of the
target's service before the switch drops it, and glidex starts the
migration again. Once promoted, it is kept like any other disk.

## Outages

A guest's filesystem treats a failed write as damage: ext4 aborts its
journal and remounts read-only, and stays so until the VM restarts. So
while the cluster can't serve a request (a PG without a primary, the map
service without a leader because a majority of its members is down,
storage nodes unreachable), the service holds the request and tries it
again every second, for up to `--hold-outage` (an hour by default). The
guest just waits: virtio-blk has no request timeout, though Linux logs
"task blocked" warnings after 2 minutes. Once the cluster answers, the
held requests finish in order and the guest goes on.

- Reads, writes and write-zeroes are held; a flush has nothing to wait
  for (writes are durable once acknowledged).
- The first request held and the last let go are logged
  (`holding requests up to 3600 s`, `no request held for an outage now`).
- Past the limit the guest sees an I/O error, as before.
- A lost lease, a fence or blocklist, a bad checksum or a bad request
  fail at once: waiting can't fix them.
- A live migration's switch waits at most 35 s for requests in flight;
  past that, a held request's answer is dropped, as for a write held for
  space. Don't migrate a VM through an outage: the source's held writes
  are fenced once the target is promoted.

## Restarts

Stopping or restarting the service doesn't detach its volumes. It keeps
them in `DIR/state.json`, and its next start takes them back and opens
their sockets again; the hypervisors reconnect and the guests carry on,
after a pause of about 4 s. Requests the guest had in flight are run
again from the hypervisor's in-flight buffer. Cloud Hypervisor
reconnects by itself; give QEMU's chardev `reconnect=1` (QEMU 9.2 and
later: `reconnect-ms=1000`). A volume another node took while the
service was down is left to it.

The state file is in the runtime directory: a reboot clears it, and with
it the VMs that used the volumes. To stop the service for good, detach
its volumes first (`ctl detach`); a stopped service's volumes stay held.
`hyperstor-install uninstall` and `--no-vhost` do this.

`hyperstor-install --vhost` installs it as `hyperstor-vhost.service`
(see [hyperstor-install](hyperstor-install.md)).
