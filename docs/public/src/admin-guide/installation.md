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

Build once, on a machine with the same CPU architecture and a C library
no newer than the target hosts', and copy the binaries.

## Running as services

No systemd units are shipped yet. A minimal unit for a storage node:

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

[Install]
WantedBy=multi-user.target
```

Run the services as an unprivileged user that owns the device files (or
has read and write access to the block devices). The map service needs no
privileges at all.
