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
| [Design decisions and trade-offs](decisions.md) | The main choices, from the reactor to VM disks, pools, fullness and administration; the alternatives weighed, and why |
| [Performance](performance.md) | Current benchmark figures on one host and on three, and the findings that changed the design |
| [Comparison with other systems](comparison.md) | How Hyperstor differs from Ceph, Longhorn, Mayastor, DRBD, Lightbits, DAOS, WEKA, VAST and others |
| [References](references.md) | Papers, kernel interfaces and products cited here |

## Goals in one paragraph

Hyperstor is a sharded, distributed store written in pure Rust on Linux
io_uring. It keeps two or three copies of every chunk (erasure coding is
planned) on many storage nodes, places them by failure domain, and keeps
serving when nodes, devices or links fail. Block volumes come first: for
virtual machines through vhost-user-blk (built), and for other hosts
through NVMe over Fabrics (planned). A read cache for data that can't
change is being built into the client library. The same object layer is
meant to carry GPU-direct reads into GPU memory, an S3 gateway and a
distributed filesystem later, with NFS proposed on top of it. It is built for the
[glidex](https://github.com/CMKL-APEX/glidex) platform, which runs agent
VMs, inference, fine-tuning and retrieval workloads, and it also runs on
its own.

## Working on the code

### Layout

| Path | Contents |
|---|---|
| `crates/hyperstor-*` | The Rust crates (see [Architecture](architecture.md#crates)) |
| `crates/hyperstor-mgmt/ui` | The web UI |
| `docs/public/` | This book |
| `docs/site/` | The landing page |
| `spec/` | The internal design documents, the implementation tracking file (`spec/tracking.toml`) and the progress board (`spec/board/`) |
| `scripts/` | CI checks: documentation against the binaries, implementation tracking, the specs and the progress board, end-to-end runs |
| `.github/workflows/` | CI and documentation publishing |

### Building and testing

```sh
cargo build --workspace --release
cargo test --workspace --release
```

- Rust 2024 edition, formatted with `rustfmt` defaults.
- On a shared or small machine, build with `-j1` or `-j2`: the workspace
  links several large binaries.
- Use a `CARGO_TARGET_DIR` of your own; never share one with another
  checkout, or cargo mixes the builds.
- The end-to-end suite (`crates/hyperstor-test`) installs clusters from
  the built release binaries into scratch directories and drives them as
  an operator would. It refuses stale binaries, so build first.
  `scripts/e2e.sh` runs it on its own.

**The web UI** (`crates/hyperstor-mgmt/ui`: React, Vite, TypeScript,
Tailwind and Connect-ES) is built with [bun](https://bun.sh):

```sh
cd crates/hyperstor-mgmt/ui && bun install && bun run build   # → dist/
sudo target/release/hyperstor-install install --ui crates/hyperstor-mgmt/ui/dist
```

- `bun run dev` serves it with hot reload, passing `/api` and `/auth` to
  a running `hyperstor-mgmt` (`HYPERSTOR_MGMT_URL`, default
  `http://127.0.0.1:8852`).
- Its TypeScript for the protos is generated and committed: after changing
  a `.proto`, run `buf generate` in `crates/hyperstor-api`.
- The login pieces come from glidex's `@glidex/ui-kit`, copied unchanged
  into `crates/hyperstor-mgmt/ui-kit` at a pinned glidex commit; change
  them in glidex, then `scripts/ui-kit-sync.sh <commit>`.
- Browser tests (`crates/hyperstor-mgmt/e2e`, Playwright) drive the built
  UI against a cluster installed from the release binaries:
  `bun install && bunx playwright install chromium && bunx playwright test`.

Before a pull request, run what CI runs:

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo build --workspace --release
cargo test --workspace --release
python3 scripts/spec-check.py
python3 scripts/board.py check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p hyperstor-client -p hyperstor-reactor
```

CI also checks that each binary's reference page matches its usage line,
that every tracked test exists and passes (the browser tests too), that
the UI's generated TypeScript and the ui-kit copy are current, and that
this book builds (`mdbook build docs/public`).

### Tracking and the progress board

Two files keep the state of the work, and CI fails when they drift:

| File | Keep it current | Why |
|---|---|---|
| `spec/tracking.toml` | Each milestone's items, their status (`done`, `partial`, `planned`) and the tests that show them working. After changing it, regenerate the [Status](../status.md) page: `scripts/tracking.py status > docs/public/src/status.md` | A `done` item needs a test, and a tracked test that is renamed, removed or failing fails CI, so the status page can't claim more than is built |
| `spec/board/<workstream>.toml` | One file per workstream you carry: its `status`, `now`, `next`, pull requests and `updated`, changed in the same branch as the work, at least with every pull request; `done` once merged and deployed. Edit only your own, then run `scripts/board.py write` | Several people and agents work at once; one file each means no conflicts, and everyone sees who carries what |

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
| No secrets, internal hostnames, addresses or real credentials in code, specs, tests or logs | What is committed is hard to take back, and this book is published |
