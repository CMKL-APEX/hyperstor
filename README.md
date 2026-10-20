# Hyperstor

A sharded, distributed storage system in Rust for
[glidex](https://github.com/CMKL-APEX/glidex) nodes: block volumes over
vhost-user-blk and NVMe-oF (TCP, RoCEv2, InfiniBand), GPU-direct storage
behind the cuFile and hipFile APIs, and later an S3 gateway and a
distributed filesystem, all on one replicated or erasure-coded object
layer.

**Status: early implementation.** HS0 (the local engine) and HS1 (placement,
wire protocol, reactor, map service, storage daemon, client) are built, and
HS2 has started (failure detection, degraded writes, backfill). The design
is in [`spec/`](spec/README.md); what was built, how it deviates, and what
to review is in [`spec/implementation-notes.md`](spec/implementation-notes.md),
with measurements in [`spec/benchmarks.md`](spec/benchmarks.md).

## Trying it (development)

```sh
cargo build --release
cat > cluster.txt <<'END'
cluster 1
node 0 rack 0 127.0.0.1:19100
node 1 rack 0 127.0.0.1:19200
node 2 rack 0 127.0.0.1:19300
pool 1 vol replicated size 3 min 2 pgs 128 chunk 4194304 domain host
END
target/release/hyperstor-meta --listen 127.0.0.1:19000 --cluster cluster.txt &
for n in 0 1 2; do
  target/release/hyperstord --node $n --meta 127.0.0.1:19000 \
    --shard 127.0.0.1:19$((n+1))00=/var/tmp/hs-n$n.img:1 &
done
HS_BENCH_SECS=2 target/release/hyperstor-bench remote 127.0.0.1:19000
```

Kill one `hyperstord`: after the map service marks it down (2 s), writes go
on with two copies. There is no block-device front end yet (vhost-user-blk
is HS3, NVMe-oF HS5); clients use `hyperstor-client`.

Tests: `cargo test --workspace --release` (the engine's crash test takes
`HS_CRASH_SEEDS`).

## Planned features

- **Block volumes**: thin provisioned, expandable, with crash-consistent
  snapshots, consistency groups and instant clones
- **Protection**: 2 or 3 replicas, or erasure coding (k+m) with a
  replicated write log for small writes
- **Data path**: pure Rust on io_uring, one reactor per core; NVMe
  passthrough, SAS and SATA SSDs, and hard disks with a flash log;
  RDMA over RoCEv2 and InfiniBand without NVMe-oF
- **Virtual machines**: vhost-user-blk for glidex VMs, a host-wide read
  cache for images and snapshots
- **GPU-direct storage**: one network hop into GPU memory, behind cuFile
  (GDS) and hipFile, on bare metal and in VMs
- **Later**: an S3 gateway with S3-over-RDMA, and a distributed
  filesystem mounted through virtio-fs and FUSE

## Repository layout

| Path | Contents |
|---|---|
| `spec/` | Design specification (contributors) |
| `docs/` | Documentation sets and their build configuration |
| `crates/` | Rust crates (`spec/design.md` §18.1): codec, engine, placement, proto, reactor, meta (map service), daemon (`hyperstord`), client, bench |
| `LICENSES/` | License texts and copyright notice |

## License

Dual-licensed under [MIT](LICENSES/LICENSE-MIT) or
[Apache-2.0](LICENSES/LICENSE-APACHE), at your option. See
[LICENSES/COPYRIGHT](LICENSES/COPYRIGHT).
