# Session handoff

Written 2026-10-21, at the end of an autonomous session, so work can
resume from another host or session. Check `git log` first: anything
newer than this note wins over it.

## State

- `main` has PR #1 (HS0, HS1 without Raft, HS2's first part) and PR #2
  (CI, public docs; GitHub Pages is not yet enabled in the repository
  settings).
- **PR #3, branch `impl/dense-map`: open, not merged.** Merge only when
  the owner says so. It holds:
  - dense block maps and fuzzy checkpoints (D43, first step; I33–I37);
  - adaptive reactor polling (I38);
  - peer reports (D40, I39);
  - cache pools, first part (D45, I40): no expiry or eviction yet;
  - decisions D47–D49 (log ceiling 1 GiB, lease 0.6 s / timeout 0.8 s /
    peer timeout 0.3 s, 200 µs polling), with the code defaults changed;
  - the decision benchmarks (`hyperstor-bench decisions`, `redb`,
    `failover`, `replay`, `logsize`) and the `collect-decision-data`
    skill.
- Both #3 and #4 were rebased onto `main` (`ef2d765`) on 2026-10-21, so
  CI's checks run on them; they passed locally before the push.
- **PR #4, branch `bench/failover-targets`, stacked on PR #3** (base
  `impl/dense-map`): failover at the recommended 1 s setting, and the
  collect script runs `failover`, `replay` and `logsize`. Merge after #3.

## Waiting on the owner

Nothing from `spec/decision-log.md`'s "Data for pending decisions":
points 11, 13 and 16 were settled on 2026-10-21 as D47, D48 and D49.
Points 12, 14 and 15 are recommendations the owner hasn't objected to.
Still open for review: D20–D37, F1–F12, I1–I40 and the host checks,
now including G16 (short leases under load, on a real network).

## Next work

1. Cache-pool expiry and eviction (the rest of D45). Open question in
   I40: with more than one copy, any membership change empties every
   copy; keeping survivors needs a per-PG "last written epoch".
2. Paged checkpoints (D47).
3. The copy-on-write B+tree (D43, D44): the log stays its write-ahead
   log, and the tree commits only at checkpoints (point 12).
4. Priority classes (D46). G14 and G15 wait for snapshots, clones and
   the Bε-tree.

## How the owner wants sessions run

- In a timed autonomous session, don't stop to ask. When stuck, pick the
  most performant, secure and sensible option, and record the decision
  and what to discuss in `spec/implementation-notes.md`.
- Add tests as you go, and check each one fails with the code it covers
  removed.
- Measure with the `collect-decision-data` skill before opening a PR.
  The benchmarks caught two regressions last session (13× slower
  commits, 3.5× slower replicated writes).
- Push and update the PR; end with a summary that leads with the numbers
  bearing on open decisions.

## Practicalities

- Edit pull requests with
  `gh api -X PATCH repos/CMKL-APEX/hyperstor/pulls/<n>`; `gh pr edit`
  fails here (GitHub's classic Projects deprecation).
- The repository has no git identity configured; pass the author with
  `git -c user.name=… -c user.email=…`.
- Reads bypass the storage daemon's commit queue. Anything that must
  take effect before a read (such as the cache-pool purge) commits
  synchronously with `commit_queued()`.
- Don't kill processes with `pkill -f` or `pgrep -f` on a pattern from
  your own command line: it matches the shell running it. Kill by PID.
