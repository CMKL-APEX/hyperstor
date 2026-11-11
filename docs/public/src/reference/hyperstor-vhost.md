# hyperstor-vhost

Serves volumes to virtual machines on this node over vhost-user-blk: the
hypervisor (Cloud Hypervisor or QEMU) connects to a socket per volume,
and the guest sees a virtio disk. One service runs per compute node and
serves every VM on it. glidex attaches and detaches volumes through its
control socket; operators can do the same with `ctl`.

```text
usage: hyperstor-vhost serve --meta ADDR[,ADDR...] [--dir DIR] [--allow-uid UID]... [--group GID] [--queues N] [--hold-full SECS] [--networks NAME[,NAME...]] [--host NAME] [--cpus LIST] | ctl [--dir DIR] hello | list | attach ID [--serial S] [--take-over | --preempt] | detach ID [--force]
```

| Command | Does |
|---|---|
| `serve` | Runs the service until SIGINT or SIGTERM. Its volumes stay attached for its next start (see [Restarts](#restarts)) |
| `ctl hello` | Checks the service answers, and its protocol version |
| `ctl list` | The volumes attached here: socket, size, whether a hypervisor is connected, whether the volume was taken elsewhere (`lost`), and the reactor serving it with that reactor's NUMA node (`reactor`, `node`) |
| `ctl attach ID` | Takes the volume's read-write lease and opens its socket; prints the socket's path |
| `ctl detach ID` | Removes the socket and gives the lease up; refused (`busy`) while a hypervisor stays connected for 2 s, unless `--force` |

| Option | Default | Meaning |
|---|---|---|
| `--meta ADDR[,ADDR...]` | | `serve`: the map service's members (their `--listen` addresses) |
| `--dir DIR` | `/run/hyperstor/vhost` | Where the control socket (`control.sock`) and the volumes' sockets (`<volume id>.sock`) are; at most 69 bytes, so a socket's path fits Unix's 107 |
| `--allow-uid UID` | | `serve`: a user allowed on the control socket besides root and the service's own; repeat for more |
| `--group GID` | | `serve`: the group given the sockets (mode 0660), so a hypervisor running as that group can connect |
| `--queues N` | 1 | `serve`: queues each disk offers (1–64) |
| `--hold-full SECS` | 300 | `serve`: how long a write refused because a device is full is held, tried again every second, before the guest sees an I/O error (0–3600; 0: at once). See [Full devices](../admin-guide/pools-and-devices.md#full-devices) |
| `--host NAME` | its hostname | `serve`: the name it gives storage nodes, by which a host is bound to realms (`hyperstorctl realm host`) |
| `--networks NAME[,NAME...]` | none | `serve`: the data networks this host reaches storage nodes on, in order: a device's endpoint on the first network it has one on (see the [cluster file](cluster-file.md#endpoint)), else its address |
| `--cpus LIST` | the first CPU of each NUMA node | `serve`: the CPUs its reactors run on, one reactor pinned to each (`0,64` or `0-1`). See [NUMA nodes](#numa-nodes) |
| `--serial S` | empty | `attach`: the disk's serial number as the guest sees it, up to 20 characters |
| `--take-over` | off | `attach`: the volume's holder died or is cut off: wait until its lease runs out (up to 4 s), then take it |
| `--preempt` | off | `attach`: take the volume from a holder that is still running; that holder's lease on this volume is blocklisted, and its other volumes go on |
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
sudo hyperstor-vhost serve --meta 192.0.2.10:8849 --group kvm &
id=$(hyperstorctl create-volume --size 20G --name vm-disk | awk '{print $2}')
sudo hyperstor-vhost ctl attach "$id" --serial vm-disk
cloud-hypervisor --memory size=2G,shared=on \
  --disk vhost_user=true,socket=/run/hyperstor/vhost/$id.sock ...
```

The VM's memory must be shared with the service (`shared=on` in Cloud
Hypervisor; a `memory-backend-memfd` with `share=on` in QEMU). The disk
has 512-byte sectors on 4 KiB physical blocks and no volatile write
cache: a write is answered once every copy has it. Discard isn't offered
yet.

The control socket takes one JSON request per line and answers each on
one line:

```text
{"id": 1, "op": "attach", "args": {"volume": "<32 hex digits>", "serial": "vm-disk", "take": "attach"}}
{"id": 1, "ok": {"socket": "/run/hyperstor/vhost/<id>.sock", "size": 21474836480}}
```

Errors are `{"id": N, "error": {"code": C, "message": M}}`, with codes
`invalid`, `denied`, `held`, `not_found`, `not_attached`, `busy`,
`unavailable` and `internal`. An `attach` may name who asks, `"holder": "<name>"` (up
to 58 printable characters; glidex gives its node's name): the lease is
labelled `vhost:<name>` (`vhost` without one). A `held` error names the
label of whoever holds the volume, `"holder"`: `vhost:<name>` for
another node's service, `hyperstorctl` for a tool.

## NUMA nodes

The service runs a reactor, a thread doing all of a disk's I/O, on each
CPU of `--cpus`: by default the first CPU of each NUMA node, the core
glidex keeps for the host there. One reactor serves about 35,000 4 KiB
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
