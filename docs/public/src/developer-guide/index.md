# Developer guide

This part is for people who work on Hyperstor itself, or want to know
how it works inside: its architecture, the algorithms it uses, the
trade-offs behind its design, how fast it is today, and how it differs
from other storage systems.

> **Built and planned.** Hyperstor is an early implementation. Each page
> says what is **built** (on `main`, covered by tests) and what is only
> **planned**. The [Status](../status.md) page is generated from the
> project's tracking file and is the final word.

| Page | What it covers |
|---|---|
| [Architecture](architecture.md) | Processes, ports, crates, and the path of a write and a read |
| [The data path: reactor and engine](data-path.md) | The io_uring reactor, the on-device format, the log, checkpoints, checksums and crash testing |
| [Placement and replication](replication.md) | Placement groups, rendezvous hashing, the replicated write, the map service, leases and fencing, failure detection, peering, recovery and scrub |
| [Volumes, leases and the blocklist](volumes.md) | Volume headers, one writer per volume, forced takeovers |
| [Design decisions and trade-offs](decisions.md) | The main choices, the alternatives weighed, and why |
| [Performance](performance.md) | Current benchmark figures, and the findings that changed the design |
| [Comparison with other systems](comparison.md) | How Hyperstor differs from Ceph, Longhorn, Mayastor, DRBD, Lightbits, DAOS, WEKA, VAST and others |
| [References](references.md) | Papers, kernel interfaces and products cited here |

## Goals in one paragraph

Hyperstor is a sharded, distributed store written in pure Rust on Linux
io_uring. It keeps two or three copies of every chunk (erasure coding is
planned) on many storage nodes, places them by failure domain, and keeps
serving when nodes, devices or links fail. Block volumes come first: for
virtual machines through vhost-user-blk, and for other hosts through
NVMe over Fabrics (both planned). The same object layer is meant to
carry GPU-direct reads into GPU memory, a host-wide read cache, an S3
gateway and a distributed filesystem later. It is built for the
[glidex](https://github.com/CMKL-APEX/glidex) platform, which runs agent
VMs, inference, fine-tuning and retrieval workloads, and it also runs on
its own.

## Working on the code

### Layout

| Path | Contents |
|---|---|
| `crates/hyperstor-*` | The Rust crates (see [Architecture](architecture.md#crates)) |
| `docs/public/` | This book |
| `docs/site/` | The landing page |
| `scripts/` | CI checks: documentation against the binaries, implementation tracking, end-to-end runs |
| `.github/workflows/` | CI and documentation publishing |

### Building and testing

```sh
cargo build --workspace --release
cargo test --workspace --release
```

- Rust 2024 edition, formatted with `rustfmt` defaults.
- On a shared or small machine, build with `-j1` or `-j2`: the workspace
  links several large binaries.
- The end-to-end suite (`crates/hyperstor-test`) installs clusters from
  the built release binaries into scratch directories and drives them as
  an operator would. It refuses stale binaries, so build first.
  `scripts/e2e.sh` runs it on its own.

Before a pull request, run what CI runs:

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo build --workspace --release
cargo test --workspace --release
```

CI also checks that each binary's reference page matches its usage line,
that every tracked test exists and passes, and that this book builds.

### Rules that shape the code

| Rule | Why |
|---|---|
| No SPDK or DPDK; the data path runs on Hyperstor's own io_uring reactor, with no third-party async runtime | One language and build, NVMe stays on the kernel driver, no cores burned polling while idle ([decisions](decisions.md#data-path-and-runtime)) |
| tokio only on the control path (Raft, the cluster API, the installer, the CLI) | Those need a mature ecosystem (TLS, HTTP/2, gRPC), and they are not latency-critical |
| FFI (io_uring passthrough, RDMA verbs, dma-buf) stays in small modules, and every `unsafe` block has a `// SAFETY:` comment | Unsafe code is reviewed where it lives |
| New behaviour comes with tests; anything that persists data or crosses a crash gets a randomized test against a model | Storage bugs hide in rare interleavings; models find them ([crash testing](data-path.md#crash-testing), [fault rig](replication.md#how-it-is-tested)) |
| Anything an operator does gets an end-to-end test | Install, join, drain and failover must work from the shipped binaries |
| A performance-sensitive change is measured against `main`, alternating runs | Two large regressions were caught this way ([Performance](performance.md#regressions-caught)) |
| User-facing changes update this book | CI checks each binary's options against its reference page |
