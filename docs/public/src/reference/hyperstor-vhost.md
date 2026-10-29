# hyperstor-vhost

Serves volumes to virtual machines on this node over vhost-user-blk: the
hypervisor (Cloud Hypervisor or QEMU) connects to a socket per volume,
and the guest sees a virtio disk. One service runs per compute node and
serves every VM on it. glidex attaches and detaches volumes through its
control socket; operators can do the same with `ctl`.

```text
usage: hyperstor-vhost serve --meta ADDR[,ADDR...] [--dir DIR] [--allow-uid UID]... [--group GID] [--queues N] | ctl [--dir DIR] hello | list | attach ID [--serial S] [--take-over | --preempt] | detach ID
```

| Command | Does |
|---|---|
| `serve` | Runs the service until SIGINT or SIGTERM. Its volumes stay attached for its next start (see [Restarts](#restarts)) |
| `ctl hello` | Checks the service answers, and its protocol version |
| `ctl list` | The volumes attached here: socket, size, whether a hypervisor is connected, and whether the volume was taken elsewhere (`lost`) |
| `ctl attach ID` | Takes the volume's read-write lease and opens its socket; prints the socket's path |
| `ctl detach ID` | Disconnects the hypervisor, removes the socket and gives the lease up |

| Option | Default | Meaning |
|---|---|---|
| `--meta ADDR[,ADDR...]` | | `serve`: the map service's members (their `--listen` addresses) |
| `--dir DIR` | `/run/hyperstor/vhost` | Where the control socket (`control.sock`) and the volumes' sockets (`<volume id>.sock`) are; at most 69 bytes, so a socket's path fits Unix's 107 |
| `--allow-uid UID` | | `serve`: a user allowed on the control socket besides root and the service's own; repeat for more |
| `--group GID` | | `serve`: the group given the sockets (mode 0660), so a hypervisor running as that group can connect |
| `--queues N` | 1 | `serve`: queues each disk offers (1–64) |
| `--serial S` | empty | `attach`: the disk's serial number as the guest sees it, up to 20 characters |
| `--take-over` | off | `attach`: the volume's holder died or is cut off: wait until its lease runs out (up to 4 s), then take it |
| `--preempt` | off | `attach`: take the volume from a holder that is still running; that holder's lease on this volume is blocklisted, and its other volumes go on |

Without `--take-over` or `--preempt`, `attach` is refused (`held`) while
another client has the volume attached. Attaching a volume that is
attached here already answers with its socket; one that was taken
elsewhere (`lost` in `list`, its I/O failing) is let go and attached
afresh.

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
`invalid`, `denied`, `held`, `not_found`, `not_attached`, `unavailable`
and `internal`. An `attach` may name who asks, `"holder": "<name>"` (up
to 58 printable characters; glidex gives its node's name): the lease is
labelled `vhost:<name>` (`vhost` without one). A `held` error names the
label of whoever holds the volume, `"holder"`: `vhost:<name>` for
another node's service, `hyperstorctl` for a tool.

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
