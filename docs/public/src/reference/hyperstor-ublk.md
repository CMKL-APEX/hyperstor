# hyperstor-ublk

Serves a volume as a Linux block device, `/dev/ublkbN`, through the
kernel's ublk driver: for testing, `mkfs` and `mount`, and fio. Runs as
root, with the `ublk_drv` module loaded (`modprobe ublk_drv`; Linux 6.0
or later, tested on 6.8).

```text
usage: hyperstor-ublk attach --meta ADDR[,ADDR...] --id ID [--take-over] [--preempt] [--depth N] | delete --dev N
```

| Command | Does |
|---|---|
| `attach` | Takes volume `--id`'s read-write lease, serves it as `/dev/ublkbN` and prints that name; on SIGINT or SIGTERM, removes the device and gives the lease up |
| `delete` | Removes device `--dev` left behind by an `attach` that was killed |

| Option | Default | Meaning |
|---|---|---|
| `--meta ADDR[,ADDR...]` | | The map service's members (its `--listen` addresses) |
| `--id ID` | | The volume (32 hexadecimal digits, as `hyperstorctl volumes` shows) |
| `--take-over` | off | The volume's holder died or is cut off: wait until its lease runs out (up to 4 s), then blocklist it and take the lease |
| `--preempt` | off | Take the volume from a holder that is still running: blocklist it first, so its writes are refused |
| `--depth N` | 64 | Requests the device takes at once (1–4096) |
| `--dev N` | | `delete`: the device's number, as in `/dev/ublkbN` |

Without `--take-over` or `--preempt`, `attach` is refused while another
client has the volume attached. A volume can't be deleted while it is
attached, unless with `hyperstorctl delete-volume --force`.

```sh
id=$(hyperstorctl create-volume --size 10G --name scratch | awk '{print $2}')
sudo hyperstor-ublk attach --meta 192.0.2.10:8849 --id "$id" &
sudo mkfs.ext4 /dev/ublkb0 && sudo mount /dev/ublkb0 /mnt
```

The device has 4 KiB blocks and no volatile write cache: a write is
answered once every copy has it. Its size is the volume's when it was
attached; a volume expanded meanwhile shows its new size after the next
`attach`.
