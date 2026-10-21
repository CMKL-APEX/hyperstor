# hyperstor-install

Checks a host and installs Hyperstor on it. See
[Installation](../admin-guide/installation.md).

```text
usage: hyperstor-install check|install|uninstall [--all-in-one] [--cluster FILE] [--join API] [--token-file F] [--token T] [--address IP] [--reactors N] [--rack R] [--role R] [--node N] [--map ADDR] [--members LIST] [--member-id N] [--nodes N] [--size-gib G] [--data-dir DIR] [--api-port P] [--map-port P] [--node-port P] [--admin-port P] [--mgmt-port P] [--root DIR] [--no-start]
```

| Command | Does |
|---|---|
| `check` | Runs the host checks and changes nothing |
| `install` | Installs, or brings an installation up to date: only what differs is changed |
| `uninstall` | Stops and removes the services and binaries; keeps `/etc/hyperstor` and the data |

| Option | Default | Meaning |
|---|---|---|
| `--all-in-one` | off | A trial cluster on this host: the map service and several storage nodes on files |
| `--cluster FILE` | | Install this host's part of the cluster the [cluster file](cluster-file.md) describes; give every host the same file. Needed the first time; later runs use the installed copy |
| `--join API` | | Join this host to a running cluster through its cluster API (`IP:PORT`) with a token (see [Joining hosts](../admin-guide/installation.md#joining-hosts-with-tokens)): with `--role storage`, as a new storage node; with `--role meta`, as a map service member listed in `--members`. Later runs need no token |
| `--token-file F` | | The join token, from `hyperstorctl token create`, read from a file. Also `--token T` or `$HYPERSTOR_JOIN_TOKEN`; the token is never saved |
| `--token T` | | The join token itself (it shows in the process list: prefer `--token-file`) |
| `--address IP` | | A joining storage host: the IP its reactors listen on, on the storage network; the metrics port is on it too |
| `--reactors N` | 1 | A joining storage host: how many reactors, each on its own device file and port (`--node-port` + i) |
| `--rack R` | 0 | A joining storage host: its rack, the failure domain pools may spread copies across |
| `--role R` | | With `--cluster`: `meta` (the map service, its API and the CAs), `storage` (one storage node), or `both`. With `--join`: `storage` or `meta` |
| `--node N` | | With a `storage` or `both` role: which node of the cluster file this host runs. It gets one reactor per address the file lists for it, each on a file in the data directory |
| `--map ADDR` | | With `--cluster`: `IP:PORT` where the map service listens; the storage nodes connect there. Not needed with `--members` |
| `--members LIST` | | With `--cluster`, for a map service of several members: every member as `N=RAFT_ADDR/MAP_ADDR`, separated by commas, the same on every host (see [the map service](../admin-guide/map-service.md#several-members)). Storage nodes connect to all of them |
| `--member-id N` | | With `--members` and a `meta` or `both` role: which member this host runs |
| `--nodes N` | 3 | All-in-one: storage nodes (1 to 16). The pool keeps `min(N, 3)` copies |
| `--size-gib G` | 4 | Size of each storage node's device file, in GiB (sparse) |
| `--data-dir DIR` | `/var/lib/hyperstor` | Where the device files go |
| `--api-port P` | 8850 | The cluster API: all-in-one on `127.0.0.1`; otherwise on the map service's own address, where joining hosts reach it |
| `--map-port P` | 8849 | The map service, on `127.0.0.1` |
| `--node-port P` | 9100 | All-in-one: storage node `n` listens on `P + n`, on `127.0.0.1`. A joining storage host: its reactors listen on `P`, `P + 1`, … on `--address` |
| `--admin-port P` | 8853 | Metrics port: all-in-one, node `n` on `127.0.0.1:P + n`; with `--cluster`, the node on its first address's IP, port P |
| `--mgmt-port P` | 8852 | The dashboard (`hyperstor-mgmt`), on `127.0.0.1`, on the map service's host |
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
| `/usr/local/bin/` | `hyperstor-meta`, `hyperstord`, `hyperstorctl`, `hyperstor-mgmt`, and `hyperstor-install` itself |
| `/etc/hyperstor/cluster.txt` | The [cluster file](cluster-file.md): N nodes; pool 1 (`vol`, replicated) and pool 2 (`cache`, a cache pool with one copy) |
| `/etc/hyperstor/meta.env`, `mgmt.env`, `node-N.env` | Each service's options |
| `/etc/hyperstor/prometheus-targets.json` | Every storage node's metrics address, for Prometheus (`deploy/prometheus/hyperstor.yml`) |
| `/etc/hyperstor/hyperstorctl.conf` | The API address for `hyperstorctl` |
| `/etc/systemd/system/` | `hyperstor-meta.service`, `hyperstor-mgmt.service` and `hyperstord@.service` (one instance per node); `hyperstor-renew.timer`, which runs `hyperstor-install install` daily |
| `/etc/hyperstor/pki/` | The cluster's certificates: `root` and `intermediate` CAs (keys readable by root only), `api` for the map service (key readable by the `hyperstor` group), `admin` for `hyperstorctl` (key readable by the `hyperstor-admin` group), `mgmt` for the dashboard (key readable by the `hyperstor` group) |
| `/etc/hyperstor/install.conf` | The saved options |

The map service's unit hands it the intermediate CA's key as a systemd
credential (`LoadCredential=`), so the service can sign the certificates
of joining hosts while the key on disk stays readable by root only. Its
`api` certificate carries the root too, so joining hosts can pin it.

**What `install --join` writes**, besides the binaries and units of its
role: `/etc/hyperstor/pki/root.crt`, and the host's own key and
certificate, made from a key that never left the host: `node.crt` and
`node.key` for a storage host (identity `node/N`), `api.crt` and
`api.key` for a map service member (`meta/N`), which also gets the
intermediate CA and the cluster file. The root's key stays on the first
map service host. A storage host's certificate is renewed by later runs
through the cluster API, with the certificate itself.

It also creates the `hyperstor` system user, which the services run as,
and the `hyperstor-admin` group, whose members may use `hyperstorctl`.
Certificates are made once and renewed by later runs when two thirds of
their life has passed (30 days for the API's and the administrator's,
a year for the intermediate CA). The daily timer is such a run: it
renews what is due, repairs anything that drifted, and otherwise changes
nothing.
Changed binaries, units, options or certificates restart only the services they affect; with `--root` or `--no-start`, the output says what a real run would restart.

**Checks** (`check`, and first thing in `install`): kernel version,
io_uring, locked-memory limit, CPUs, NVMe generic devices, clock
synchronization, free space, ports, and systemd. Each line is `ok`,
`info`, `warn` or `FAIL`; a `FAIL` stops an install.

| Exit status | Meaning |
|---|---|
| 0 | Done (for `check`, nothing failed) |
| 1 | A check failed, or a step failed (the message says which) |
| 2 | A missing or malformed option |
