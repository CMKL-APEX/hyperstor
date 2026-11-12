# Hyperstor

A sharded, distributed storage system in Rust for
[glidex](https://github.com/CMKL-APEX/glidex) nodes, usable without them:
block volumes for VMs and programs, an S3 gateway and key-value ranges
today; NVMe-oF (TCP, RoCEv2, InfiniBand), erasure coding, GPU-direct
storage behind the cuFile and hipFile APIs, and a distributed filesystem
later, all on one replicated object layer.

**Status: early implementation, not for production data.** The data
ports between clients and storage nodes have no authentication or
encryption yet: run clusters on an isolated network.

What v0.0.8 has:

- **Storage:** replicated pools over TCP on Hyperstor's own io_uring
  engine (checksums on every block, crash recovery, a device format
  hardened against devices that lie); a map service of one member, or
  three or five replicated with Raft; failure handling, recovery and scrub (HS0–HS2);
  cache pools without expiry or eviction.
- **Pools and devices at run time** (HS18): devices added, weighted,
  classed and removed on running nodes; pools made, split, given more or
  fewer copies, and deleted; fullness thresholds.
- **Volumes:** leases, fencing per attached volume, snapshots, clones,
  rollbacks and consistency groups, a host read cache in progress
  (HS4); VM disks
  through `hyperstor-vhost` over vhost-user-blk, with a reactor on each
  NUMA node and polled disks for latency-critical VMs, and glidex's
  driver (HS3); `hyperstor-ublk` for testing.
- **Objects:** an S3 gateway, `hyperstor-s3`, with multipart uploads,
  copies, conditional requests, virtual-hosted addressing and buckets
  per tenant (HS12), on key-value ranges (HS11).
- **Tenancy** (HS15): tenants on volumes and buckets, each bucket's
  objects and bytes counted, and realms that bind hosts, pools and
  gateways and grant roles on them; quotas and fair share between
  tenants come later.
- **Networks** (HS19): named networks per subnet, nodes serving each
  device on them, clients picking theirs.
- **Administration** (AD0–AD3): the cluster API over mutual TLS,
  `hyperstorctl`, `hyperstor-install` (a trial cluster, hosts joined with
  tokens, a host made a map service member or storage only), and
  `hyperstor-mgmt` with password and OIDC logins, roles, a React web UI,
  the trash, the audit log and long operations.

Devices formatted before v0.0.8 keep working; `hyperstorctl device
upgrade` converts them to the new format once every node runs v0.0.8
([upgrading](docs/public/src/admin-guide/upgrading.md)). The public
[status page](docs/public/src/status.md) lists every item. The design is
in [`spec/`](spec/README.md); what was built, how it deviates, and what
to review is in [`spec/01-implementation-notes.md`](spec/01-implementation-notes.md),
with measurements in [`spec/02-benchmarks.md`](spec/02-benchmarks.md).

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

Kill one `hyperstord`: after the map service marks it down (0.8 s), writes go
on with two copies; start it again and it receives only what it missed.
Programs use `hyperstor-client`; VMs use `hyperstor-vhost`; S3 clients
use `hyperstor-s3`; for testing, `hyperstor-ublk` serves a volume as
`/dev/ublkbN`. To install a trial cluster as system services instead:
`sudo target/release/hyperstor-install install --all-in-one`.

Tests: `cargo test --workspace --release` (the engine's crash test takes
`HS_CRASH_SEEDS`). CI also checks the spec and tracks each milestone's
progress against the tests (`spec/tracking.toml`).

User, administrator and developer documentation is in
[`docs/public/`](docs/public/src/introduction.md); the
[developer guide](docs/public/src/developer-guide/index.md) walks through
the architecture, the algorithms, the design trade-offs, current
benchmark figures and how Hyperstor compares with other systems; CI publishes it to
GitHub Pages from `main` ([`docs/README.md`](docs/README.md)).

## Planned features

- **Remote hosts:** NVMe over TCP (HS5), RDMA over RoCEv2 and InfiniBand
  (HS6), and NVMe over RDMA (HS6b)
- **Protection:** erasure coding (k+m) with a replicated write log for
  small writes (HS7)
- **GPU-direct storage:** one network hop into GPU memory, behind cuFile
  (GDS) and hipFile, on bare metal and in VMs (HS8, HS9)
- **Hard disks** with flash log devices (HS10); expiry, eviction and
  priority classes in cache pools (HS10b)
- **Later:** S3 over RDMA, a distributed filesystem mounted through
  virtio-fs and FUSE, NFS and pNFS for it (a proposal,
  [`spec/22-nfs.md`](spec/22-nfs.md)), sites and federation, and storage
  administration in glidex (AD4)

## Repository layout

| Path | Contents |
|---|---|
| `spec/` | Design specification (contributors) |
| `docs/` | Public documentation (users, administrators), the landing page, and the books' build configuration |
| `scripts/` | CI checks: spec consistency, implementation tracking, documentation |
| `.github/workflows/` | CI and documentation publishing |
| `crates/` | Rust crates (`spec/15-operations.md` §18.1): codec, engine, placement, map (cluster map), proto, reactor, rpc, fabric (networks and ports), kv (key-value ranges), meta (map service), daemon (`hyperstord`), client, ublk (`hyperstor-ublk`), vhost (`hyperstor-vhost`), s3 (`hyperstor-s3`), bench; for administration api (cluster API), pki, ctl (`hyperstorctl`), install (`hyperstor-install`), mgmt (`hyperstor-mgmt`, with the web UI in `crates/hyperstor-mgmt/ui`), authd (`hyperstor-authd`), smart (`hyperstor-smart`, NVMe health); and test (end-to-end suite) |
| `deploy/` | Prometheus and Grafana configuration for the storage nodes' metrics |
| `LICENSES/` | License texts and copyright notice |

## License

Dual-licensed under [MIT](LICENSES/LICENSE-MIT) or
[Apache-2.0](LICENSES/LICENSE-APACHE), at your option. See
[LICENSES/COPYRIGHT](LICENSES/COPYRIGHT).
