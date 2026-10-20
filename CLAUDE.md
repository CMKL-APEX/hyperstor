# Hyperstor: notes for coding agents

Hyperstor is a sharded, distributed block, object and file store in
Rust, built for [glidex](https://github.com/CMKL-APEX/glidex) nodes and
usable without them. Read `spec/README.md` first, then the status line
and decisions table of `spec/design.md`.

## Where things stand

- **Design only; no code yet.** `spec/design.md` is a full draft;
  `spec/filesystem.md` is an outline.
- **Settled with the project owner:** D1–D19 in design.md, and the
  choices behind D28–D31 and D34–D37 (metadata out of Raft, non-NVMe
  devices, verbs as the RDMA transport, NCCL coexistence, a native S3
  gateway with S3-over-RDMA, a native filesystem).
- **Open for the owner's review:** the wording and details of D20–D37 and
  F1–F12, and every host check (G1–G12, FG1–FG2).
- **Next:** finish that review, then plan milestone HS0 (design.md §21):
  the reactor prototype, the engine format and the io_uring-versus-SPDK
  benchmark gate G5.

## Design work

- **Resolve design questions before drafting.** When a choice is the
  owner's to make, ask, with options and a recommendation first, rather
  than picking silently. Record the answer as a numbered decision.
- **Decisions** (`D…` in design.md, `F…` in filesystem.md) are a table:
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

## Code conventions (from HS0 on)

- Rust 2024, rustfmt defaults (`cargo fmt --check`), crates under
  `crates/hyperstor-*` (design.md §18.1).
- Before every pull request, run
  `cargo clippy --workspace --all-targets -- -D warnings` and the tests.
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
