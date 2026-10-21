# Hyperstor: notes for coding agents

Hyperstor is a sharded, distributed block, object and file store in
Rust, built for [glidex](https://github.com/CMKL-APEX/glidex) nodes and
usable without them. Read `spec/README.md` first, then the status line
of `spec/overview.md` and the decisions in `spec/decisions.md`; the
design is split by topic, with section numbers kept across files.

## Where things stand

- **Implemented:** HS0, HS1 and part of HS2 on `main`,
  including dense maps, peer reports, cache pools without expiry or
  eviction, peering, delta recovery and light and deep scrub by content
  digests, and the replicated map service (3 or 5 members on
  `glidex-raftstore`), atomic operation lists on objects, and volume
  headers in the `.meta` pool (create, list, expand, delete); the first part of AD0 and AD1: the cluster API on the map
  service over mutual TLS with roles and stable error reasons,
  `hyperstorctl` (checked against every RPC of the protos), join tokens
  for storage hosts and map service members with certificate renewal
  over the API, drains as operations, and `hyperstor-install` with host
  checks, an all-in-one trial cluster and `--join`. `spec/implementation-notes.md` has
  what was built, the decisions taken while building it (I1…), what to
  discuss with the owner, and bugs the tests found;
  `spec/benchmarks.md` has the measurements.
- **Settled with the project owner:** D1–D19, D38–D55, D61–D68 and D69–D77 in decisions.md, and the
  choices behind D28–D31 and D34–D37.
- **Open for the owner's review:** D20–D37, D56–D60 and D66's merge
  rule, F1–F12 in detail, I1–I73 and I80–I93, and every host check (G1–G20,
  FG1–FG2).
- **Next:** volume headers, leases and the blocklist (D75–D77,
  decision-log.md points 34–37), on the object operations of I68: the
  `.meta` pool and headers, then leases, then the blocklist and forced
  takeovers; changing the map service's members at run time (I65); the
  copy-on-write B+tree with snapshots and clones (D43, D44, HS4, which
  also gives D47's paged checkpoints); `hyperstor-vhost` and the glidex
  driver (HS3); cache-pool expiry and eviction and priority classes
  (HS10b, D45, D46).
  Administration (installer, API, CLI, UI, security) is designed in
  `spec/administration.md`, milestones AD0–AD4; next there: the
  certificate denylist pushed with the map (D56, with the blocklist),
  `buf` in CI (I93), installing on whole devices, then `glidex-auth`
  and people's logins (AD2).

## Design work

- **Resolve design questions before drafting.** When a choice is the
  owner's to make, ask, with options and a recommendation first, rather
  than picking silently. Record the answer as a numbered decision.
- **Decisions** (`D…` in decisions.md, `F…` in filesystem.md) are a table:
  the decision, then *why*. Never renumber them; a dropped decision is
  removed and its number not reused. Decisions you make while drafting
  are marked open for review in the status line.
- **Host checks** (`G…`, `FG…`) are claims to verify on real hardware or
  software before the dependent sections are final. When you aren't
  sure a kernel, driver or library behaves a certain way, write a check,
  not an assertion.
- **Style**, as in glidex's specs: short sentences, plain words, bullets
  and tables, a *why* for every rule. Cite external facts with versions.
- **Cross-references:** `§N` is a section of the same document; names
  of glidex documents (`storage.md S16`, `host-topology.md H7`) refer to
  glidex's `spec/`. When sections move, update every `§` reference and
  leave references into glidex untouched.
- A new spec document needs an entry in `spec/README.md` and
  `spec/SUMMARY.md`.

## Code conventions

- Rust 2024, rustfmt defaults (`cargo fmt --check`), crates under
  `crates/hyperstor-*` (operations.md §18.1).
- Before every pull request, run
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --check`, `cargo build --workspace --release`, then
  `cargo test --workspace --release` (the end-to-end suite runs the built
  binaries and refuses stale ones), and
  `scripts/spec-check.py`. CI (`.github/workflows/ci.yml`) runs them all.
- **Track what is built** in `spec/tracking.toml`: each milestone's items,
  their status (`done`, `partial`, `planned`) and the tests that show them
  working. CI fails when a tracked test is renamed, removed or failing,
  or when a `done` item has no test; untracked tests are only listed.
  After changing it, regenerate the public status page
  (`scripts/tracking.py status > docs/public/src/status.md`).
- **User-facing changes update `docs/public/`** (docs/README.md): a new
  or changed option of a binary changes its reference page, which CI
  checks against the binary's usage line.
- New behaviour comes with tests; anything that persists data or crosses
  a crash gets a randomized test against a model (see the engine's crash
  test). Anything an operator does (install, manage, fail over, security)
  gets an end-to-end test in `crates/hyperstor-test`, which installs
  clusters from the built binaries into scratch roots; `scripts/e2e.sh`
  runs it, and `--installed` checks a host where Hyperstor is installed. Record measurements in `spec/benchmarks.md` with the host and
  the command, and decisions in `spec/implementation-notes.md`. To gather
  data for the owner's decisions, use the `collect-decision-data` skill
  (`.claude/skills/`) and `scripts/collect-decision-data.sh`.
- No SPDK or DPDK dependency (D3). The data path is hyperstor's own
  io_uring reactor, with no third-party async runtime; tokio is for the
  control path only.
- FFI (io_uring passthrough, rdma-core verbs, dma-buf) stays in small,
  dedicated modules, and every `unsafe` block has a `// SAFETY:` comment.
- `glidex-raftstore` comes from the glidex repository as a pinned git
  dependency (D20); changes to it are glidex pull requests.
- License: MIT OR Apache-2.0. Vendored third-party code keeps its own
  license, noted where it lives.

## Working on the host

- Work on a branch in your own worktree or clone; other sessions may be
  using the main checkout. Commit and push only when asked; open pull
  requests against `main`.
- Use your own `CARGO_TARGET_DIR`. Never point it at another checkout's
  target directory (such as glidex's): cargo mixes the builds.
- On shared development hosts, build with `-j1` or `-j2` to avoid
  running out of memory.
- Never commit secrets, internal hostnames or addresses, or real
  credentials, in code, specs, tests or logs.
