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
three copies, systemd units, and `hyperstorctl`. It is for trying
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
`/etc/hyperstor` and the units, and starts `hyperstor-meta` and
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

Then see [Managing a cluster](managing.md).

## Running as services by hand

For a cluster across several hosts, write the units yourself until the
installer can join hosts. A minimal unit for a storage node:

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
