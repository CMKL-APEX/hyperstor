# hyperstor-smart

The root helper that reads NVMe drives' SMART / health logs for
[hyperstord](hyperstord.md), which runs as the unprivileged `hyperstor`
user while the kernel lets only `CAP_SYS_ADMIN` read the log.
`hyperstor-install` sets it up on every storage host.

```text
usage: hyperstor-smart [--config-dir DIR] --serve|health PATH
```

| Option | Default | Meaning |
|---|---|---|
| `--config-dir DIR` | `/etc/hyperstor` | Where the storage nodes' options are (`node-*.env`): it reads only the devices their `--shard` options name |
| `--serve` | | Answer one request on standard input and output, as systemd's `hyperstor-smart.socket` starts it |

`health PATH` reads one drive by hand, as root, and prints what a request
would get.

systemd's `hyperstor-smart.socket` listens on
`/run/hyperstor-smart/smart.sock` (mode 0660, group `hyperstor`) and
starts one short process per connection (`Accept=yes`), with no network,
a read-only system and no capability but `CAP_SYS_ADMIN`. Each storage
node asks it every 30 s for each NVMe device it serves.

A request is one line, `health PATH`, where `PATH` is a device exactly
as a node's `--shard` names it. The answer is one line: `ok HEX WCTEMP
CCTEMP`, the 512-byte log (NVMe Get Log Page 02h) in hex and the
controller's warning and critical temperatures in kelvin, or `error WHY`
for a device that isn't configured, isn't an NVMe namespace, or doesn't
answer. It issues read commands only.

The nodes show what it reads in `hyperstorctl perf --device D`, the web
UI's device panel, and the Advisor (`hyperstorctl advise`).

| Exit status | Meaning |
|---|---|
| 0 | Answered (an `error` answer too) |
| 2 | A malformed option |
