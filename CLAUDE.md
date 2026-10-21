# Hyperstor: notes for coding agents

Hyperstor is a sharded, distributed block, object and file store in
Rust, built for [glidex](https://github.com/CMKL-APEX/glidex) nodes and
usable without them. Read `spec/README.md` first, then the status line
of `spec/overview.md` and the decisions in `spec/decisions.md`; the
design is split by topic, with section numbers kept across files.

## Where things stand

- **Implemented:** HS0, HS1 (no Raft yet) and part of HS2 on `main`,
  including dense maps, peer reports and cache pools without expiry or
  eviction; the first part of AD0 and AD1: the cluster API on the map
  service over mutual TLS with roles, `hyperstorctl`, and
  `hyperstor-install` with host checks and an all-in-one trial cluster. `spec/implementation-notes.md` has
  what was built, the decisions taken while building it (I1…), what to
  discuss with the owner, and bugs the tests found;
  `spec/benchmarks.md` has the measurements.
- **Settled with the project owner:** D1–D19, D38–D55, D61–D68 and D69–D70 in decisions.md, and the
  choices behind D28–D31 and D34–D37.
- **Open for the owner's review:** D20–D37, D56–D60 and D66's merge
  rule, F1–F12 in detail, I1–I52, and every host check (G1–G20,
  FG1–FG2).
- **Next:** cache-pool expiry and eviction (the rest of D45; open
  question in I40), paged checkpoints (D47), the copy-on-write B+tree
  (D43, D44), priority classes (D46); then the rest of HS2 (read leases,
  PG log, scrub, blocklist).
  Administration (installer, API, CLI, UI, security) is designed in
  `spec/administration.md`, milestones AD0–AD4.

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
  `cargo fmt --check`, `cargo test --workspace --release` and
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
  test). Record measurements in `spec/benchmarks.md` with the host and
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
