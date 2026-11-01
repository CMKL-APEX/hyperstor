# Hyperstor: notes for coding agents

Hyperstor is a sharded, distributed block, object and file store in
Rust, built for [glidex](https://github.com/CMKL-APEX/glidex) nodes and
usable without them. Read `spec/README.md` first, then the status line
of `spec/03-overview.md` and the decisions in `spec/04-decisions.md`; the
design is split by topic, with section numbers kept across files.

## Where things stand

- **Implemented:** HS0, HS1 and HS2 on `main`,
  including dense maps, peer reports, cache pools without expiry or
  eviction, peering, delta recovery and light and deep scrub by content
  digests, and the replicated map service (3 or 5 members on
  `glidex-raftstore`), atomic operation lists on objects, and volume
  headers in the `.meta` pool (create, list, expand, delete), volume
  leases (attach, detach, renewals), the blocklist and forced takeovers,
  down and out, failed devices and slow replicas, a primary repairing
  its own copy, and the in-process fault rig; volumes as ublk block
  devices (`hyperstor-ublk`, our own ublk on the reactor); snapshots,
  clones, rollback (in place to the newest snapshot), flatten and
  consistency groups, in the client library, the API and `hyperstorctl`,
  and the host read cache (HS4); paged checkpoints that write only the
  pages changed (D105); the first part of AD0 and AD1: the cluster API on the map
  service over mutual TLS with roles and stable error reasons,
  `hyperstorctl` (checked against every RPC of the protos), join tokens
  (replicated through Raft) for storage hosts and map service members
  with certificate renewal over the API, an encrypted backup of the root
  CA's key, drains as operations, and `hyperstor-install` with host
  checks, an all-in-one trial cluster and `--join`; in AD2, people's
  password and OIDC logins to `hyperstor-mgmt` (sessions, API tokens,
  Cedar roles with step-up, the first administrator's claim, the cluster
  API on their behalf, named in the map service's audit), and
  `hyperstorctl --mgmt` with a person's token, and the React web UI on
  `@glidex/ui-kit` with browser tests; in HS3, `hyperstor-vhost` (volumes
  as VM disks over vhost-user-blk, staged through its control socket; VMs
  boot from them under Cloud Hypervisor and QEMU), with each attached
  volume fenced on its own and D33's delegates (D150–D153). `spec/01-implementation-notes.md` has
  what was built, the decisions taken while building it (I1…), what to
  discuss with the owner, and bugs the tests found;
  `spec/02-benchmarks.md` has the measurements.
- **Settled with the project owner:** D1–D19, D38–D55, D61–D68, D69–D126, D127, D129, D130, D138–D141, D142–D144, D147–D149, D150–D153, D154–D188 and D27 in 04-decisions.md, and the
  choices behind D28–D31 and D34–D37.
- **Open for the owner's review:** D20–D26, D28–D37, D56–D60 and D66's merge
  rule, D145, D146, F1–F12 in detail, I1–I78, I80–I227, the NFS and pNFS proposals
  D128 and D131–D137 (`spec/22-nfs.md`), every host check (G1–G46,
  FG1–FG2), and points 97–103 (the engine's layout and AI workloads) and
  128–137 (S3 sooner) of `spec/05-decision-log.md`. Pools and devices at
  run time (`spec/24-pools-and-devices.md`, D163–D176, D184, D186–D188)
  are built through PG splits and the autoscaler (stage 5).
- **Next:** glidex's `hyperstor` driver is merged (glidex #177) and
  `hyperstor-vhost` installs and restarts without losing disks (D142–D145);
  then the rest of
  D165: reads off the reactor's thread first (G43 found reads on a slow
  device still hold it), then checkpoint slices and deferred writes; the rest of
  §11.1 (adaptive polling, reactors per NUMA node); zone or FDP placement for write-once pools (D105, G23);
  the on-device index (D43, D44) when index RAM calls for it; cache-pool expiry and eviction and priority classes
  (HS10b, D45, D46).
  Administration (installer, API, CLI, UI, security) is designed in
  `spec/19-administration.md`, milestones AD0–AD4; AD3 is done (trash,
  audit, long operations); next there: AD4, storage administration in
  glidex; operations streamed through mgmt (I166).
  Installing on whole devices waits for a storage test host.

## Design work

- **Resolve design questions before drafting.** When a choice is the
  owner's to make, ask, with options and a recommendation first, rather
  than picking silently. Record the answer as a numbered decision.
- **Decisions** (`D…` in 04-decisions.md, `F…` in 00-filesystem.md) are a table:
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
- Spec files are `NN-topic.md`, numbered in the order they were written.
  A new spec document takes the next free number (never renumber the
  others) and needs an entry in `spec/README.md` and `spec/SUMMARY.md`.
  Always cite the numbered name (`09-replication.md §8.5`);
  `scripts/spec-check.py` rejects a topic name alone.

## Code conventions

- Rust 2024, rustfmt defaults (`cargo fmt --check`), crates under
  `crates/hyperstor-*` (15-operations.md §18.1).
- Before every pull request, run
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --check`, `cargo build --workspace --release`, then
  `cargo test --workspace --release` (the end-to-end suite runs the built
  binaries and refuses stale ones), `scripts/spec-check.py` and
  `scripts/board.py check`. CI (`.github/workflows/ci.yml`) runs them all.
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
  runs it, and `--installed` checks a host where Hyperstor is installed. Record measurements in `spec/02-benchmarks.md` with the host and
  the command, and decisions in `spec/01-implementation-notes.md`. To gather
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
- **Keep the progress board current** (`spec/23-progress-board.md`):
  when you start a workstream, add `spec/board/<workstream>.toml` (fields
  in `scripts/board.py`); update its `status`, `now`, `next`, PRs and
  `updated` in the same branch as the work, at least with every pull
  request; mark it `done` when the work is merged and deployed, and
  remove entries done for more than two weeks. Edit only your own
  workstream's file, then run `scripts/board.py write`; CI runs
  `scripts/board.py check`.
- Use your own `CARGO_TARGET_DIR`. Never point it at another checkout's
  target directory (such as glidex's): cargo mixes the builds.
- On shared development hosts, build with `-j1` or `-j2` to avoid
  running out of memory.
- Never commit secrets, internal hostnames or addresses, or real
  credentials, in code, specs, tests or logs.
