# Installation

There are no packages yet: build the binaries from source and copy them
to each machine.

## Requirements

- **Linux** on x86-64 (other architectures are untested), with io_uring
  enabled: Linux 5.6 or later, and a current long-term kernel is
  recommended. Some distributions turn io_uring off with the
  `kernel.io_uring_disabled` sysctl; it must be `0`.
- **Rust** 1.88 or later, with cargo.
- For storage devices: a filesystem that supports `O_DIRECT` for device
  files, or whole block devices.
- TCP connectivity between all storage nodes, and from clients to the map
  service and to every storage node. See the security note in
  [Running a cluster](index.md).

## Building

```sh
git clone https://github.com/CMKL-APEX/hyperstor
cd hyperstor
cargo build --release
```

| Binary | Install on | Purpose |
|---|---|---|
| `target/release/hyperstor-meta` | the map service host | [map service](map-service.md) |
| `target/release/hyperstord` | every storage node | [storage node](storage-nodes.md) |
| `target/release/hyperstor-bench` | any host, optional | [load generator](benchmarking.md) |
| `target/release/hyperstorctl` | any host that reaches the cluster API | [managing a cluster](managing.md) |
| `target/release/hyperstor-install` | run from the build directory | [installer](../reference/hyperstor-install.md) |

Build once, on a machine with the same CPU architecture and a C library
no newer than the target hosts', and copy the binaries.

## Installing a trial cluster

`hyperstor-install` sets up a whole cluster on one Linux host: the map
service with its cluster API, three storage nodes on files, a pool with
three copies and a cache pool, systemd units, and `hyperstorctl`. It is for trying
Hyperstor and testing changes, not for data you want to keep.

```sh
cargo build --release
target/release/hyperstor-install check           # host checks, changes nothing
sudo target/release/hyperstor-install install --all-in-one
hyperstorctl status
hyperstorctl io-test
```

`check` reports each requirement as `ok`, `info`, `warn` or `FAIL`.
`install` runs the checks, copies the binaries to `/usr/local/bin`,
creates the `hyperstor` user, writes the configuration in
`/etc/hyperstor`, the cluster's certificates in `/etc/hyperstor/pki`,
and the units, and starts `hyperstor-meta` and
`hyperstord@0` to `hyperstord@2`. Everything listens on `127.0.0.1`:
the cluster API on 8850, the map service on 8849, the storage nodes on
9100 to 9102. Device files go to `/var/lib/hyperstor` (4 GiB each,
sparse).

The installer is convergent: run it again after rebuilding and it
updates what changed (binaries, configuration, units) and restarts the
services affected; with nothing changed it says so and does nothing.
The options of the last run are saved, so `sudo hyperstor-install
install` is enough later. `sudo hyperstor-install uninstall` stops and
removes the services and binaries and keeps the configuration and data.
All options are in the [reference](../reference/hyperstor-install.md).

`hyperstorctl` needs the administrator certificate, readable by root
and the `hyperstor-admin` group: run it with `sudo`, or join the group
(`sudo usermod -aG hyperstor-admin $USER`, then log in again).

Then see [Managing a cluster](managing.md).

## Joining hosts with tokens

Install the map service on one host first. It makes the cluster's
certificates and serves the cluster API on its own address, where the
other hosts reach it:

```sh
# on the map service's host (192.0.2.10); the cluster file may list no
# storage node at all: they join
sudo target/release/hyperstor-install install --cluster cluster.txt \
    --role meta --map 192.0.2.10:8849
```

Then, for each storage host, make a token on the map service's host and
use it on the new host:

```sh
# on the map service's host; umask 077 keeps the file private
(umask 077; sudo hyperstorctl token create --role node > token)
# copy `token` to the new host, readable by root only, then there:
sudo target/release/hyperstor-install install --join 192.0.2.10:8850 \
    --token-file token --role storage --address 192.0.2.21 --reactors 2
```

The new host pins the cluster by the root fingerprint in the token
before it sends anything, makes its own key (which never leaves it),
and gets a certificate for a new storage node number. The map service
adds the node to the map, down until its first heartbeat, and its PGs
start moving to it once it runs. The node gets one reactor per
`--reactors`, each on a file of `--size-gib` GiB in the data directory
and on its own port from `--node-port`, and its metrics on `--address`
at port 8853.

- A token is used once, expires after an hour (`--ttl SECS`, at most a
  week), and can be revoked with `hyperstorctl token revoke --id ID`;
  `hyperstorctl token list` shows them, never the secrets. Tokens are
  kept in the map service's replicated store, so they survive restarts;
  they are made and used through the leader.
- The token is never saved. Later runs of the installer, such as the
  daily timer's, need none; they renew the host's certificate through
  the API with the certificate itself.
- Installing a host again with the same addresses gets its node back.

**Several map service members.** Install the first member with the
whole member list, then join the others with `meta` tokens; they get
the cluster file, the root's certificate and the intermediate CA, so
every member signs with the same CA (the root's key stays on the first
host):

```sh
M=1=192.0.2.10:8851/192.0.2.10:8849,2=192.0.2.11:8851/192.0.2.11:8849,3=192.0.2.12:8851/192.0.2.12:8849
# first member
sudo hyperstor-install install --cluster cluster.txt --role meta --members $M --member-id 1
(umask 077; sudo hyperstorctl token create --role meta > token-2)
# second member (and likewise the third)
sudo hyperstor-install install --join 192.0.2.10:8850 --token-file token-2 \
    --role meta --members $M --member-id 2
```

A member's host can also run a storage node: list the nodes in the
cluster file, and give each member `--role both --node N` (the first
with `--cluster`, the others with `--join`); a joined member gets the
file, and so its node, from the first.

Tokens are made and used through the leader; the other members refuse
with the leader's address. Before the members have formed a group, the
first member makes the `meta` tokens itself.

**Back up the root CA's key** on the first host, which alone has it
(see [hyperstor-install](../reference/hyperstor-install.md)):

```sh
sudo hyperstor-install root-backup --out /media/usb/root.bak --passphrase-file pass
```

## Installing a cluster across hosts from one file

Without tokens, write the [cluster file](../reference/cluster-file.md)
once, copy it to every host, and install each host's part of it:

```sh
# on the map service's host (192.0.2.10)
sudo target/release/hyperstor-install install --cluster cluster.txt \
    --role meta --map 192.0.2.10:8849
# on each storage host, with its node number from the file
sudo target/release/hyperstor-install install --cluster cluster.txt \
    --role storage --node 0 --map 192.0.2.10:8849
```

A storage node gets one reactor for each address the cluster file lists
for it. Adding a host means editing every copy of the file. Storage
hosts installed this way get no certificate; joined ones do, though the
storage network doesn't use them yet: it has no authentication (see
[Running a cluster](index.md)).

## Running as services by hand

Without the installer, a minimal unit for a storage node:

```ini
[Unit]
Description=Hyperstor storage node
After=network-online.target
Wants=network-online.target

[Service]
User=hyperstor
ExecStart=/usr/local/bin/hyperstord --node 0 --meta 192.0.2.10:9000 \
    --shard 192.0.2.20:9100=/var/lib/hyperstor/dev0.img:100
Restart=on-failure
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
```

Run the services as an unprivileged user that owns the device files (or
has read and write access to the block devices). The map service needs no
privileges at all. `hyperstor-install install --all-in-one --root DIR
--no-start` writes its units and configuration under DIR without
installing anything, as a starting point.
