# hyperstor-install

Checks a host and installs Hyperstor on it. See
[Installation](../admin-guide/installation.md).

```text
usage: hyperstor-install check|install|uninstall|root-backup|root-restore [--all-in-one] [--cluster FILE] [--join API] [--token-file F] [--token T] [--address IP] [--reactors N] [--rack R] [--role R] [--node N] [--map ADDR] [--members LIST] [--member-id N] [--nodes N] [--size-gib G] [--data-dir DIR] [--api-port P] [--map-port P] [--node-port P] [--admin-port P] [--mgmt-port P] [--root DIR] [--no-start] [--out FILE] [--in FILE] [--passphrase-file F] [--remove-key] [--ui DIR] [--mgmt-listen all|loopback] [--web-tls DIR] [--tailscale-cert] [--no-tailscale-cert]
```

| Command | Does |
|---|---|
| `check` | Runs the host checks and changes nothing |
| `install` | Installs, or brings an installation up to date: only what differs is changed |
| `uninstall` | Stops and removes the services and binaries; keeps `/etc/hyperstor` and the data |
| `root-backup` | Writes the root CA's certificate and key to `--out`, encrypted with the passphrase, checks that it opens, and records the backup; with `--remove-key`, then removes the key from this host |
| `root-restore` | Puts the root CA from the backup `--in` back into `/etc/hyperstor/pki`, only onto its own cluster |

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
| `--out FILE` | | `root-backup`: the backup to write (created mode 0600; an existing file is never overwritten) |
| `--in FILE` | | `root-restore`: the backup to read |
| `--passphrase-file F` | | `root-backup`, `root-restore`: the passphrase, at least 12 characters, from a file; or `$HYPERSTOR_BACKUP_PASSPHRASE`. Never on the command line |
| `--ui DIR` | `hyperstor-ui` beside the installer | Where the map service runs: install the [web UI](hyperstor-mgmt.md)'s built files from `DIR` (`crates/hyperstor-mgmt/ui/dist` after `bun run build`) to `/usr/local/share/hyperstor/ui`, replacing what was there, and have `hyperstor-mgmt` serve them. Without it and without `hyperstor-ui`, an installed UI stays; with none, the built-in dashboard is served. Not saved |
| `--mgmt-listen all\|loopback` | `all` | Where the map service's host serves `hyperstor-mgmt`: every address over HTTPS (`all`), or `127.0.0.1` over plain HTTP, for an SSH tunnel (`loopback`). Saved |
| `--web-tls DIR` | the cluster CA's | mgmt's web certificate: `DIR/web.crt` and `DIR/web.key`, yours (an ACME client's, say), re-read when they change. `--web-tls ""` goes back to the cluster CA's. Saved |
| `--tailscale-cert` | off | Also serve the node's Tailscale certificate for its `ts.net` names: grants the `hyperstor` user `TS_PERMIT_CERT_UID` in a `tailscaled` drop-in and restarts `tailscaled` (the tailnet drops for a few seconds). Refused, changing nothing, when `tailscaled` already grants it to someone else (glidex-ui, say). Saved |
| `--no-tailscale-cert` | | Turn it off again: removes that drop-in |
| `--remove-key` | off | `root-backup`: once the backup opens, remove the root's key from this host, keeping the root offline |

Options are saved in `/etc/hyperstor/install.conf` and reused by later
runs, so `sudo hyperstor-install install` alone updates an existing
installation; options given on the command line replace the saved ones.

The binaries are taken from the directory `hyperstor-install` itself is
in, so run it from `target/release` after `cargo build --release`.

**What `install --all-in-one` writes:**

| Path | Contents |
|---|---|
| `/usr/local/bin/` | `hyperstor-meta`, `hyperstord`, `hyperstorctl`, `hyperstor-mgmt`, `hyperstor-authd`, and `hyperstor-install` itself |
| `/etc/systemd/system/hyperstor-authd.socket`, `.service`; `/etc/pam.d/hyperstor`; `/etc/hyperstor/authd.json` | The PAM helper for people's logins ([hyperstor-authd](hyperstor-authd.md)), socket-activated, on the map service's host |
| `/etc/hyperstor/cluster.txt` | The [cluster file](cluster-file.md): N nodes; pool 1 (`vol`, replicated) and pool 2 (`cache`, a cache pool with one copy) |
| `/etc/hyperstor/meta.env`, `mgmt.env`, `node-N.env` | Each service's options |
| `/etc/hyperstor/prometheus-targets.json` | Every storage node's metrics address, for Prometheus (`deploy/prometheus/hyperstor.yml`) |
| `/etc/hyperstor/hyperstorctl.conf` | The API address for `hyperstorctl` |
| `/etc/systemd/system/` | `hyperstor-meta.service`, `hyperstor-mgmt.service` and `hyperstord@.service` (one instance per node); `hyperstor-renew.timer`, which runs `hyperstor-install install` daily |
| `/etc/hyperstor/pki/` | The cluster's certificates: `root` and `intermediate` CAs (keys readable by root only), `api` for the map service (key readable by the `hyperstor` group), `admin` for `hyperstorctl` (key readable by the `hyperstor-admin` group), `mgmt` for the dashboard (key readable by the `hyperstor` group) |
| `/etc/hyperstor/pki/web.crt`, `web.key` | mgmt's web certificate from the cluster CA, for this host's names and addresses (unless `--web-tls`) |
| `/etc/systemd/system/tailscaled.service.d/50-hyperstor-cert.conf` | With `--tailscale-cert`: lets mgmt fetch the node's Tailscale certificate (removed by `--no-tailscale-cert` and `uninstall`) |
| `/usr/local/share/hyperstor/ui/` | The web UI's files, when installed with `--ui` (removed by `uninstall`) |
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
the `hyperstor-admin` group, whose members may use `hyperstorctl`, and
the `hyperstor-users` group, whose members may log in to the management
service with their password.
Certificates are made once and renewed by later runs when two thirds of
their life has passed (30 days for the API's and the administrator's,
a year for the intermediate CA). The daily timer is such a run: it
renews what is due, repairs anything that drifted, and otherwise changes
nothing.
Changed binaries, units, options or certificates restart only the services they affect; with `--root` or `--no-start`, the output says what a real run would restart.

**The root CA's key.** Only the first map service host has it, and it
signs a new intermediate CA every 9 months (the intermediate lasts a
year): losing it without a backup means a new root, and new certificates
everywhere, within a year. Until `root-backup` has run, every install and
`check` says so. The backup is AES-256-GCM, with a key derived from the
passphrase (PBKDF2-HMAC-SHA256, 600,000 iterations); keep it and the
passphrase apart. With `--remove-key` the host no longer holds the root,
and later installs still work; restore it when the intermediate is due.

```sh
sudo hyperstor-install root-backup --out /media/usb/root.bak \
    --passphrase-file pass --remove-key
sudo hyperstor-install root-restore --in /media/usb/root.bak --passphrase-file pass
```

**Checks** (`check`, and first thing in `install`): kernel version,
io_uring, locked-memory limit, CPUs, NVMe generic devices, clock
synchronization, free space, ports, the root CA key's backup, and systemd. Each line is `ok`,
`info`, `warn` or `FAIL`; a `FAIL` stops an install.

| Exit status | Meaning |
|---|---|
| 0 | Done (for `check`, nothing failed) |
| 1 | A check failed, or a step failed (the message says which) |
| 2 | A missing or malformed option |
