# hyperstor-install

Checks a host and installs Hyperstor on it. See
[Installation](../admin-guide/installation.md).

```text
usage: hyperstor-install check|install|uninstall [--all-in-one] [--nodes N] [--size-gib G] [--data-dir DIR] [--api-port P] [--map-port P] [--node-port P] [--admin-port P] [--root DIR] [--no-start]
```

| Command | Does |
|---|---|
| `check` | Runs the host checks and changes nothing |
| `install` | Installs, or brings an installation up to date: only what differs is changed |
| `uninstall` | Stops and removes the services and binaries; keeps `/etc/hyperstor` and the data |

| Option | Default | Meaning |
|---|---|---|
| `--all-in-one` | off | A trial cluster on this host: the map service and several storage nodes on files. The only kind of install for now |
| `--nodes N` | 3 | Storage nodes (1 to 16). The pool keeps `min(N, 3)` copies |
| `--size-gib G` | 4 | Size of each storage node's device file, in GiB (sparse) |
| `--data-dir DIR` | `/var/lib/hyperstor` | Where the device files go |
| `--api-port P` | 8850 | The cluster API, on `127.0.0.1` |
| `--map-port P` | 8849 | The map service, on `127.0.0.1` |
| `--node-port P` | 9100 | Storage node `n` listens on `P + n`, on `127.0.0.1` |
| `--admin-port P` | 8853 | Storage node `n` serves its metrics on `P + n`, on `127.0.0.1` |
| `--root DIR` | `/` | Write every file under DIR instead (for packaging and tests); no users or services are touched |
| `--no-start` | off | Install the files but don't enable or start the services |

Options are saved in `/etc/hyperstor/install.conf` and reused by later
runs, so `sudo hyperstor-install install` alone updates an existing
installation; options given on the command line replace the saved ones.

The binaries are taken from the directory `hyperstor-install` itself is
in, so run it from `target/release` after `cargo build --release`.

**What `install --all-in-one` writes:**

| Path | Contents |
|---|---|
| `/usr/local/bin/` | `hyperstor-meta`, `hyperstord`, `hyperstorctl`, and `hyperstor-install` itself |
| `/etc/hyperstor/cluster.txt` | The [cluster file](cluster-file.md): N nodes; pool 1 (`vol`, replicated) and pool 2 (`cache`, a cache pool with one copy) |
| `/etc/hyperstor/meta.env`, `node-N.env` | Each service's options |
| `/etc/hyperstor/hyperstorctl.conf` | The API address for `hyperstorctl` |
| `/etc/systemd/system/` | `hyperstor-meta.service` and `hyperstord@.service` (one instance per node); `hyperstor-renew.timer`, which runs `hyperstor-install install` daily |
| `/etc/hyperstor/pki/` | The cluster's certificates: `root` and `intermediate` CAs (keys readable by root only), `api` for the map service (key readable by the `hyperstor` group), `admin` for `hyperstorctl` (key readable by the `hyperstor-admin` group) |
| `/etc/hyperstor/install.conf` | The saved options |

It also creates the `hyperstor` system user, which the services run as,
and the `hyperstor-admin` group, whose members may use `hyperstorctl`.
Certificates are made once and renewed by later runs when two thirds of
their life has passed (30 days for the API's and the administrator's,
a year for the intermediate CA). The daily timer is such a run: it
renews what is due, repairs anything that drifted, and otherwise changes
nothing.
Changed binaries, units or options restart the services they affect.

**Checks** (`check`, and first thing in `install`): kernel version,
io_uring, locked-memory limit, CPUs, NVMe generic devices, clock
synchronization, free space, ports, and systemd. Each line is `ok`,
`info`, `warn` or `FAIL`; a `FAIL` stops an install.

| Exit status | Meaning |
|---|---|
| 0 | Done (for `check`, nothing failed) |
| 1 | A check failed, or a step failed (the message says which) |
| 2 | A missing or malformed option |
